//! pillbox — pillbox-as-bundle CLI (v0.6).
//!
//! See README.md and AGENTS.md for the design rationale. High-level:
//!
//! 1. A **pillbox** is a self-contained bundle of (workspace + code +
//!    vault + config). There's one **global** pillbox at
//!    `~/.pillbox/global/`, plus a **project** pillbox per directory
//!    that has a `pillbox.toml`. State lives at
//!    `~/.pillbox/projects/<dash-encoded-cwd>/`.
//!
//! 2. Top-level commands operate on pillbox **lifecycle**:
//!    `init / new / list / rm / info`.
//!
//! 3. Per-pillbox commands operate on the **current** pillbox, resolved
//!    from cwd or `--pillbox NAME`:
//!    `run / secret / env / auth / vault / doctor / sidecar / version`.

use std::{path::PathBuf, process::ExitCode};

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};

mod agents;
mod commands;
mod config;
mod docker;
mod doctor;
mod envs;
mod errors;
mod events;
mod paths;
mod pillbox;
mod registry;
mod remote;
mod sandbox;
mod secrets;
mod session;
#[cfg(test)]
mod test_util;
mod url_safety;
mod vault;
mod workspace;

use agents::RunOpts;
use errors::PillboxError;
use pillbox::Pillbox;
use secrets::WriteScope;

#[derive(Parser, Debug)]
#[command(name = "pillbox", version, about, long_about = None)]
struct Cli {
    /// Select a specific named pillbox (matches `meta.json.name` or the
    /// path-encoded state-dir key). Overrides cwd-based discovery.
    #[arg(long, global = true, value_name = "NAME")]
    pillbox: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Create the global pillbox at `~/.pillbox/global/`. Idempotent.
    Init,
    /// Create a project pillbox in the current directory. Writes
    /// `pillbox.toml` to cwd, creates a state dir at
    /// `~/.pillbox/projects/<dash-encoded-cwd>/`, and initializes a
    /// rustic repository (local by default; `--workspace-backend s3`
    /// to use an S3-shaped bucket).
    New {
        /// Display name for the pillbox. Defaults to the cwd's basename.
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// Default agent for `pillbox run` (`claude` | `codex`).
        #[arg(long, value_name = "AGENT")]
        agent: Option<String>,
        /// Workspace backend variant. `local` (default) stores the
        /// rustic repo under `~/.pillbox/projects/<key>/repo/`; `s3`
        /// stores it in a user-owned S3-compatible bucket.
        #[arg(long = "workspace-backend", value_name = "VARIANT")]
        workspace_backend: Option<String>,
        /// S3-only: bucket name.
        #[arg(long, value_name = "BUCKET")]
        bucket: Option<String>,
        /// S3-only: endpoint URL (R2, MinIO, native S3, …).
        #[arg(long, value_name = "URL")]
        endpoint: Option<String>,
        /// S3-only: region. Defaults to `auto`.
        #[arg(long, value_name = "REGION")]
        region: Option<String>,
        /// S3-only: object key prefix within the bucket.
        #[arg(long, value_name = "PREFIX")]
        prefix: Option<String>,
        /// S3-only: env var name that holds the access key.
        #[arg(long = "access-key-env", value_name = "VAR")]
        access_key_env: Option<String>,
        /// S3-only: env var name that holds the secret key.
        #[arg(long = "secret-key-env", value_name = "VAR")]
        secret_key_env: Option<String>,
        /// Clone a git repository into cwd at pillbox-creation time.
        /// Refuses if cwd isn't empty.
        #[arg(long = "from-git", value_name = "URL")]
        from_git: Option<String>,
        /// Optional ref (branch or SHA) when paired with `--from-git`.
        #[arg(long = "git-ref", value_name = "REF", requires = "from_git")]
        git_ref: Option<String>,
    },
    /// List every pillbox on disk (global + projects).
    List {
        #[arg(long)]
        json: bool,
    },
    /// Delete a pillbox by name. Refuses to remove the global pillbox.
    Rm {
        /// Pillbox name (`meta.json.name`) or path-encoded key.
        name: String,
    },
    /// Show the current pillbox: source, state dir, default agent.
    Info {
        #[arg(long)]
        json: bool,
    },
    /// Launch an agent against the current pillbox.
    Run {
        /// Agent to launch (`claude` | `codex`). Defaults to the current
        /// pillbox's `agent =` field, or `claude` if unset.
        #[arg(long, value_name = "AGENT")]
        agent: Option<String>,
        /// Host path to mount as the workspace. Defaults to cwd.
        #[arg(long, value_name = "PATH")]
        workspace: Option<PathBuf>,
        /// Override the workspace mount-point name (`/workspace/<name>`).
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// Extra bind mount. Repeatable. Forwarded to `docker run -v`.
        #[arg(long = "mount", value_name = "HOST:GUEST")]
        mounts: Vec<String>,
        /// Inject a stored secret as an env var. `NAME` binds to `NAME`;
        /// `NAME=ENV_VAR` rebinds. Repeatable. Highest precedence.
        #[arg(long = "with", value_name = "NAME[=ENV_VAR]")]
        withs: Vec<String>,
        /// Inject every variable from a stored env bundle. Repeatable.
        #[arg(long = "env", value_name = "BUNDLE")]
        env_bundles: Vec<String>,
        /// Inject every variable from a `.env` file on disk. Repeatable.
        #[arg(long = "env-file", value_name = "PATH")]
        env_files: Vec<PathBuf>,
        /// Route the agent's API traffic through pillbox's vault proxy.
        #[arg(long)]
        vault: bool,
        /// Attach a shared MCP server to the sandbox. NAME is the
        /// identifier the agent sees in its tool list; URL must be
        /// http:// or https://. `localhost` / `127.0.0.1` are
        /// rewritten to `host.docker.internal` so the sandbox can
        /// reach host-bound servers. Repeatable.
        /// v0: Claude only. Not supported with `--remote`.
        #[arg(long = "mcp", value_name = "NAME=URL", conflicts_with = "remote")]
        mcps: Vec<agents::McpAttachment>,
        /// Run on a registered remote VPS (`pillbox remote add NAME …`).
        /// The agent launches inside a pillbox sandbox on the remote;
        /// the local terminal proxies the remote PTY.
        #[arg(long, value_name = "NAME", conflicts_with = "vault_stdin")]
        remote: Option<String>,
        /// Hidden: invoked by the remote side of `pillbox run --remote`.
        /// Reads a [`crate::sandbox::remote_ssh::VaultStdinBlob`] from
        /// stdin and runs the agent locally with the pre-resolved
        /// state. Not for direct user consumption — the protocol is
        /// internal and may change between releases.
        #[arg(long = "vault-stdin", hide = true)]
        vault_stdin: bool,
        /// Start the agent and immediately return — keeps the remote
        /// session alive in the background. Reattach later with
        /// `pillbox session attach <id>`. v0.6 PR 6: e2b:// remotes
        /// only (ssh:// detach lands in a follow-up).
        #[arg(long, requires = "remote")]
        detach: bool,
        /// Human label for the detached session (surfaced in `session
        /// list`). Only meaningful with `--detach` — clap rejects the
        /// flag without it instead of silently dropping the value.
        #[arg(long, value_name = "TEXT", requires = "detach")]
        label: Option<String>,
        /// Emit the started session as a JSON object on stdout instead
        /// of the human "session started" banner. Useful for
        /// orchestrators: `pillbox run --detach --json | jq -r
        /// .session.id`. Only meaningful with `--detach`.
        #[arg(long, requires = "detach")]
        json: bool,
        /// POST every lifecycle event to URL as JSON. Forwarded to the
        /// in-sandbox wrapper so its `pillbox session done` call can
        /// reach the same URL — orchestrators that subscribe to the
        /// webhook see started + completed/failed end-to-end without
        /// pillbox running a daemon. Equivalent to setting
        /// `$PILLBOX_EVENTS_WEBHOOK` in the environment; when both are
        /// set, the flag wins (it overwrites the env for the rest of
        /// this pillbox invocation).
        #[arg(long = "events-webhook", value_name = "URL")]
        events_webhook: Option<String>,
        /// Retention TTL — duration after which `pillbox session prune`
        /// will tear this session down (sandbox kill + record delete).
        /// Format: `30m`, `24h`, `7d` (`s`/`m`/`h`/`d` units only).
        /// Captures per-session retention intent at spawn time so
        /// different sessions can have different lifetimes — failed
        /// experiments 1h, prod runs 7d. Only meaningful with
        /// `--detach` (interactive runs don't persist a record).
        /// Pillbox does NOT auto-prune; the user / orchestrator runs
        /// `pillbox session prune` from cron or by hand.
        /// Caveats: the TTL anchor is the moment the sandbox-spawn
        /// helper returns (potentially seconds after you press
        /// enter), not CLI dispatch time — irrelevant for hour/day
        /// TTLs, occasionally surprising for `--ttl 30s` tests.
        /// And `expires_at` is computed against the local system
        /// clock, so badly skewed clocks (no NTP) skew expirations.
        #[arg(long, value_name = "DURATION", requires = "detach")]
        ttl: Option<String>,
        /// Reference to the session this run was forked from. Carried
        /// through to the lifecycle event payload as
        /// `parent_session_id` and to OTel as `parent_span_id`, so a
        /// consumer can stitch a forked trace tree even when the
        /// parent lives in a different pillbox. Shape-validated via
        /// `validate_session_id` (alphanumeric + hyphen, max 64
        /// chars), but pillbox does NOT require the parent to exist
        /// in this pillbox's registry — the field is observability
        /// metadata; consumers reconcile.
        #[arg(long, value_name = "ID")]
        parent: Option<String>,
        /// Args forwarded to the agent CLI inside the sandbox.
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Manage remotes (SSH VPSes + E2B sandboxes) for `pillbox run --remote NAME`.
    Remote {
        #[command(subcommand)]
        action: RemoteAction,
    },
    /// Manage detached sessions started with `pillbox run --remote NAME --detach`.
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
    /// Manage stored secrets for the current pillbox.
    Secret {
        #[command(subcommand)]
        action: SecretAction,
    },
    /// Manage stored env bundles for the current pillbox.
    Env {
        #[command(subcommand)]
        action: EnvAction,
    },
    /// Inspect or remove persisted agent state.
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
    /// Inspect the credential vault for the current pillbox.
    Vault {
        #[command(subcommand)]
        action: VaultAction,
    },
    /// Run the credential vault as a standalone sidecar process.
    Sidecar {
        #[arg(long, value_name = "ADDR")]
        bind: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Diagnose pillbox's environment.
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// Print pillbox version + the runner image tag it targets.
    Version,
    /// Snapshot the current workspace (cwd) into the pillbox's
    /// rustic repository.
    Push {
        /// Short tag for the snapshot (e.g. `v1`, `before-refactor`).
        /// Surfaced in `snapshot list` next to the handle.
        #[arg(long, value_name = "NAME")]
        tag: Option<String>,
        /// Free-form snapshot message (analogous to a commit message).
        #[arg(long, short = 'm', value_name = "TEXT")]
        message: Option<String>,
        /// Emit the snapshot record as JSON on stdout. Stable schema —
        /// pin against `version: 1`.
        #[arg(long)]
        json: bool,
    },
    /// Restore the workspace from a snapshot. Defaults to the latest.
    Pull {
        /// Snapshot to restore. Accepts a unique prefix (≥ 4 hex chars)
        /// or the full handle. Omit to restore the latest snapshot.
        #[arg(long, value_name = "HANDLE")]
        snapshot: Option<String>,
    },
    /// Inspect / manage the pillbox's snapshots.
    Snapshot {
        #[command(subcommand)]
        action: SnapshotAction,
    },
    /// Workspace-level operations (rekey, …).
    Workspace {
        #[command(subcommand)]
        action: WorkspaceAction,
    },
    /// Emit a shell completion script on stdout. Pipe into your shell's
    /// completion dir (`bash`, `zsh`, `fish`, `powershell`, `elvish`).
    Completions {
        /// Shell to generate completions for.
        #[arg(value_name = "SHELL")]
        shell: clap_complete::Shell,
    },
}

#[derive(Subcommand, Debug)]
enum SnapshotAction {
    /// List every snapshot in the pillbox's repository.
    List {
        /// Emit a JSON array of snapshot records on stdout. Stable
        /// schema — pin against `version: 1`.
        #[arg(long)]
        json: bool,
    },
    /// Show one snapshot's metadata. Accepts a unique prefix.
    Show {
        /// Snapshot handle (full hex ID or a unique prefix ≥ 4 chars).
        handle: String,
        /// Emit the snapshot record as JSON on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Remove one snapshot. Data packs survive until a future `prune`.
    Rm {
        /// Snapshot handle (full hex ID or a unique prefix ≥ 4 chars).
        handle: String,
    },
}

#[derive(Subcommand, Debug)]
enum WorkspaceAction {
    /// Rotate the repository encryption password.
    Rekey,
}

#[derive(Subcommand, Debug)]
enum SecretAction {
    /// Store a secret value (reads from stdin by default).
    Add {
        name: String,
        #[arg(long, value_name = "VAR")]
        from_env: Option<String>,
        #[arg(long)]
        if_not_exists: bool,
        /// Write to the global pillbox instead of the resolved one.
        #[arg(long)]
        global: bool,
        /// Mark this secret as vaulted (stub-swap at injection time).
        #[arg(long)]
        vault: bool,
        #[arg(long, value_name = "KNOWN_NAME", requires = "vault",
              conflicts_with_all = ["host", "header_scheme", "prefix"])]
        maps_to: Option<String>,
        #[arg(long, value_name = "HOST", requires = "vault")]
        host: Option<String>,
        #[arg(long = "header-scheme", value_name = "SCHEME", requires = "vault")]
        header_scheme: Option<String>,
        #[arg(long, value_name = "PREFIX", requires = "vault")]
        prefix: Option<String>,
    },
    /// List stored secret names (project + global, deduplicated).
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show a secret's value (masked by default).
    Show {
        name: String,
        #[arg(long)]
        reveal: bool,
        #[arg(long, requires = "reveal")]
        to_stdout: bool,
        #[arg(long)]
        json: bool,
    },
    /// Delete a stored secret from the resolved scope (or `--global`).
    Rm {
        name: String,
        #[arg(long)]
        global: bool,
    },
}

#[derive(Subcommand, Debug)]
enum EnvAction {
    Load {
        name: String,
        path: PathBuf,
        #[arg(long)]
        if_not_exists: bool,
        #[arg(long)]
        global: bool,
    },
    List {
        #[arg(long)]
        json: bool,
    },
    Show {
        name: String,
        #[arg(long)]
        reveal: bool,
        #[arg(long, requires = "reveal")]
        to_stdout: bool,
        #[arg(long)]
        json: bool,
    },
    Rm {
        name: String,
        #[arg(long)]
        global: bool,
    },
}

#[derive(Subcommand, Debug)]
enum AuthAction {
    /// Run the OAuth flow inside a one-shot sandbox.
    Login {
        /// Agent to authenticate (`claude` | `codex`).
        #[arg(long, value_name = "AGENT")]
        agent: String,
        /// Reserved — v0.6 PR 2 always writes to global. Pass for
        /// forward compatibility; identical to default behavior today.
        #[arg(long)]
        global: bool,
    },
    /// Show which agents have authenticated state.
    List {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        global: bool,
    },
    /// Remove an agent's persistent state.
    Rm {
        provider: String,
        #[arg(long)]
        global: bool,
    },
}

#[derive(Subcommand, Debug)]
enum RemoteAction {
    /// Register a remote VPS for use with `pillbox run --remote NAME`.
    ///
    /// Two positional args: `NAME URL`, matching `git remote add`. The
    /// long `--url` spelling is accepted as a hidden alias so scripts
    /// written against earlier drafts of this PR keep working.
    Add {
        /// Display name. Used as `pillbox run --remote NAME`.
        name: String,
        /// SSH destination URL: `ssh://user@host[:port]`. Either
        /// positional or via `--url`; exactly one form is required.
        url: Option<String>,
        /// Hidden alias for the positional URL — see the command docs.
        #[arg(long = "url", value_name = "URL", hide = true, conflicts_with = "url")]
        url_flag: Option<String>,
        /// Default agent for runs against this remote (overrides the
        /// pillbox's own `agent` field). Optional.
        #[arg(long, value_name = "AGENT")]
        agent: Option<String>,
        /// Fail if the remote already exists in the chosen scope.
        #[arg(long)]
        if_not_exists: bool,
        /// Write to the global pillbox instead of the resolved one.
        #[arg(long)]
        global: bool,
    },
    /// List remotes visible from the current pillbox (project + global).
    List {
        #[arg(long)]
        json: bool,
    },
    /// Remove a registered remote from the resolved scope (or `--global`).
    Rm {
        name: String,
        #[arg(long)]
        global: bool,
    },
    /// Show details for one remote (with inheritance).
    Info {
        name: String,
        #[arg(long)]
        json: bool,
    },
}

/// Terminal status passed to `pillbox session done <id>`. Maps to the
/// `session.completed` / `session.failed` event types in `events.rs`.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum DoneStatus {
    Ok,
    Failed,
}

#[derive(Subcommand, Debug)]
enum SessionAction {
    /// List sessions started from this pillbox (oldest first).
    List {
        /// Emit a JSON array of session records. Pin to `version: 1`.
        #[arg(long)]
        json: bool,
    },
    /// Show one session (accepts a unique id prefix ≥ 4 chars).
    Info {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Reattach to a detached session. Streams the remote PTY back
    /// into the current terminal. Detach again with Ctrl-A + D or by
    /// running `pillbox session detach <id>` from another shell.
    Attach { id: String },
    /// Signal a currently-attached pillbox process to detach. The
    /// session record is left in place; the backend keeps running.
    /// Idempotent — no error if the session is already detached.
    Detach { id: String },
    /// Tear down the backend resources (kill the sandbox) and remove
    /// the session record. Idempotent.
    Rm { id: String },
    /// Rehydrate a session's result workspace into a directory. Reads
    /// `result_snapshot` from the session record (set by `session done
    /// --result-snapshot`) and asks the workspace backend to pull it.
    /// Used by orchestrators for post-mortem inspection of a failed
    /// fork: analyzer agents read the failed session's workspace
    /// without having to re-run anything.
    Pull {
        id: String,
        /// Destination directory. Created if missing. Defaults to
        /// `./session-<id>` so two sequential pulls don't clobber each
        /// other.
        #[arg(long, value_name = "DIR")]
        to: Option<PathBuf>,
    },
    /// Tear down every session whose `expires_at` is in the past.
    /// Drives `session rm` for each — sandbox killed, record deleted.
    /// Sessions with no `expires_at` (no `--ttl` at spawn) are left
    /// alone forever; the user / orchestrator owns the policy.
    /// Intended for cron / orchestrator schedules; pillbox doesn't
    /// auto-prune on every invocation.
    Prune {
        /// List what would be pruned without actually pruning. Useful
        /// for sanity-checking a cron entry before turning it on.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Emit a sandbox-side `session.started` event. Mirror of
    /// [`Done`] for the front of the lifecycle: the in-sandbox
    /// wrapper script calls this right before launching the agent
    /// so consumers can compute cold-start latency from
    /// `host.started_at` → `sandbox.started_at` (distinguished by
    /// the `emitter` attribute on each event).
    ///
    /// No-op cost when no sink is configured. The event payload is
    /// minimal — `session_id` + `started_at` + `emitter=sandbox` —
    /// because the host's prior `session.started` already carries
    /// the full record (agent_id, remote, backend, label, …);
    /// consumers correlate by `session_id`.
    Started { id: String },
    /// Mark a session done, emitting `session.completed` or
    /// `session.failed` to every configured sink (JSONL + webhook +
    /// OTel). Invoked manually for orchestrator-driven completion, or
    /// automatically by the in-sandbox wrapper script after the agent
    /// exits. Does NOT tear down the sandbox — use `session rm` for
    /// that.
    ///
    /// Sandbox-side use: when called from inside an E2B sandbox where
    /// the session record doesn't exist locally, builds a stub
    /// payload from the id and relies on webhook / OTel sinks to ferry
    /// the event to the host or orchestrator.
    Done {
        id: String,
        /// `ok` → emits `session.completed`. `failed` → emits
        /// `session.failed`.
        #[arg(long, value_enum)]
        status: DoneStatus,
        /// Free-text reason. Only meaningful with `--status failed`;
        /// surfaced in the event payload + via OTel `status.message`.
        #[arg(long, value_name = "TEXT")]
        reason: Option<String>,
        /// Exit code of the agent process, if known. Surfaced in the
        /// event payload + as an OTel attribute on the span.
        #[arg(long = "exit-code", value_name = "N")]
        exit_code: Option<i32>,
        /// Path (rustic snapshot URL or local file) to the agent's
        /// captured tool-call trace. Carried verbatim through to event
        /// consumers; pillbox doesn't dereference it today.
        #[arg(long = "trace-path", value_name = "PATH")]
        trace_path: Option<String>,
        /// Rustic snapshot handle of the agent's result workspace,
        /// captured by the in-sandbox wrapper (`pillbox push --tag
        /// session-<id>`) after the agent exits. Recorded on the
        /// session record so `pillbox session pull <id>` can rehydrate
        /// it later; also included in the terminal-event payload.
        #[arg(long = "result-snapshot", value_name = "HANDLE")]
        result_snapshot: Option<String>,
    },
    /// Stream session lifecycle events as JSONL on stdout.
    ///
    /// v0.7 spike emits `session.started` and `session.dropped` only;
    /// PR 2 adds `completed`/`failed` + webhook + OTel sinks. The
    /// `--json` flag is reserved for compat — every line is already
    /// JSON today; the flag exists so PR 2 can introduce a human
    /// default without breaking orchestrator callers.
    Events {
        /// Keep streaming as new events arrive (`tail -f` shape).
        #[arg(long)]
        follow: bool,
        /// Reserved for compat. v0.7 spike output is always JSONL.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
enum VaultAction {
    Ca {
        #[arg(long)]
        json: bool,
    },
    Status {
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = run(cli);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => errors::report(&e),
    }
}

fn run(cli: Cli) -> Result<()> {
    let pillbox_arg = cli.pillbox.as_deref();
    match cli.command {
        Command::Init => pillbox::init(),
        Command::New {
            name,
            agent,
            workspace_backend,
            bucket,
            endpoint,
            region,
            prefix,
            access_key_env,
            secret_key_env,
            from_git,
            git_ref,
        } => pillbox::new(
            name,
            agent,
            pillbox::NewWorkspaceArgs {
                backend: workspace_backend,
                endpoint,
                region,
                bucket,
                prefix,
                access_key_env,
                secret_key_env,
                from_git,
                git_ref,
            },
        ),
        Command::List { json } => pillbox::list(json),
        Command::Rm { name } => pillbox::rm(&name),
        Command::Info { json } => pillbox::info(pillbox_arg, json),
        Command::Run {
            agent,
            workspace,
            name,
            mounts,
            withs,
            env_bundles,
            env_files,
            vault,
            mcps,
            remote,
            vault_stdin,
            detach,
            label,
            json,
            events_webhook,
            ttl,
            parent,
            args,
        } => {
            let resolved = Pillbox::resolve(pillbox_arg)?;
            // Hidden remote-side handler. The blob carries everything we
            // need (agent id, args, env, secrets); the rest of `--run` is
            // ignored. clap's `conflicts_with` already rejects `--remote`
            // + `--vault-stdin` together, so no further check needed.
            if vault_stdin {
                return crate::sandbox::remote_ssh::dispatch_vault_stdin(&resolved);
            }
            // `--events-webhook URL` sets the env var for the duration of
            // this process so every downstream `emit_session_event` call
            // (here and threaded through to the helper subprocess) picks
            // it up uniformly. Equivalent to `PILLBOX_EVENTS_WEBHOOK=URL
            // pillbox run …`; either form works. When both the flag and
            // the env are set, the flag wins (the `set_var` below
            // overwrites the inherited env for this process tree).
            //
            // SAFETY: `std::env::set_var` is `unsafe` in edition 2024
            // because concurrent `env::var` reads in other threads can
            // observe a torn pointer. At this call site we're still on
            // the main thread — `Cli::parse` doesn't spawn, and the
            // first downstream `std::thread::spawn` (the stderr pump in
            // `spawn_and_pump`) happens strictly after this `set_var`
            // returns. We never mutate `PILLBOX_EVENTS_WEBHOOK` again,
            // so subsequent `env::var` reads (from the pump thread,
            // from `emit_session_event`, from the helper subprocess)
            // race only against this single completed write.
            if let Some(url) = &events_webhook {
                let validated = validate_events_webhook_url(url)?;
                unsafe {
                    std::env::set_var("PILLBOX_EVENTS_WEBHOOK", &validated);
                }
            }
            // `--parent <id>` plumbing mirrors the webhook flow: shape-
            // validate, stash in `PILLBOX_PARENT_SESSION_ID`, and let
            // both the host's `session.started` emit and the sandbox-
            // side `session started` CLI (via the helper's bash export)
            // pick it up off the env. Shape-only validation: the parent
            // may live in another pillbox's registry, so we don't
            // reject "unknown" ids — the field is observability
            // metadata; consumers reconcile.
            //
            // SAFETY: same single-threaded justification as the
            // PILLBOX_EVENTS_WEBHOOK `set_var` above.
            if let Some(id) = &parent {
                commands::session::validate_session_id(id)?;
                unsafe {
                    std::env::set_var(events::PARENT_SESSION_ID_ENV, id);
                }
            }
            // Parse `--ttl 30m` / `24h` / `7d` at the CLI boundary.
            // Stored as seconds on `RunOpts` so the backend converts
            // to an absolute RFC3339 `expires_at` close to the actual
            // session-write time (no clock drift between parse and
            // persist).
            let ttl_seconds = match ttl {
                Some(s) => Some(session::parse_ttl_seconds(&s)?),
                None => None,
            };
            dispatch_run(
                &resolved,
                agent,
                RunOpts {
                    workspace,
                    name,
                    mounts,
                    withs,
                    env_bundles,
                    env_files,
                    vault,
                    mcps,
                    args,
                    remote_name: remote,
                    detach,
                    label,
                    json,
                    ttl_seconds,
                },
            )
        }
        Command::Remote { action } => {
            let resolved = Pillbox::resolve(pillbox_arg)?;
            commands::remote::dispatch(&resolved, action)
        }
        Command::Session { action } => {
            let resolved = Pillbox::resolve(pillbox_arg)?;
            commands::session::dispatch(&resolved, action)
        }
        Command::Secret { action } => {
            let resolved = Pillbox::resolve(pillbox_arg)?;
            commands::secret::dispatch(&resolved, action)
        }
        Command::Env { action } => {
            let resolved = Pillbox::resolve(pillbox_arg)?;
            commands::env::dispatch(&resolved, action)
        }
        Command::Auth { action } => {
            // Auth always resolves to global in PR 2, but we still
            // resolve the current pillbox so `--pillbox NAME` works for
            // the v0.7 path forward without breaking the CLI shape now.
            let resolved = Pillbox::resolve(pillbox_arg)?;
            commands::auth::dispatch(&resolved, action)
        }
        Command::Vault { action } => {
            let resolved = Pillbox::resolve(pillbox_arg)?;
            commands::vault::dispatch(&resolved, action)
        }
        Command::Sidecar { bind, json } => {
            let resolved = Pillbox::resolve(pillbox_arg)?;
            commands::sidecar::run(&resolved, bind, json)
        }
        Command::Doctor { json } => doctor::run(json),
        Command::Version => {
            println!(
                "pillbox {} (runner image: {})",
                env!("CARGO_PKG_VERSION"),
                docker::RUNNER_IMAGE
            );
            Ok(())
        }
        Command::Push { tag, message, json } => {
            let resolved = Pillbox::resolve(pillbox_arg)?;
            commands::workspace::push(&resolved, tag, message, json)
        }
        Command::Pull { snapshot } => {
            let resolved = Pillbox::resolve(pillbox_arg)?;
            commands::workspace::pull(&resolved, snapshot)
        }
        Command::Snapshot { action } => {
            let resolved = Pillbox::resolve(pillbox_arg)?;
            commands::workspace::snapshot_dispatch(&resolved, action)
        }
        Command::Completions { shell } => {
            // `Cli::command()` materializes the clap definition without
            // re-parsing argv; generate_to_stdout writes the shell
            // script for the user to source. No pillbox resolution
            // needed — this is a static codegen step.
            let mut cmd = Cli::command();
            let bin_name = cmd.get_name().to_string();
            clap_complete::generate(shell, &mut cmd, bin_name, &mut std::io::stdout());
            Ok(())
        }
        Command::Workspace { action } => {
            let resolved = Pillbox::resolve(pillbox_arg)?;
            commands::workspace::dispatch(&resolved, action)
        }
    }
}

fn resolve_agent_spec(
    resolved: &Pillbox,
    override_id: Option<&str>,
) -> Result<&'static agents::AgentSpec> {
    let id = if let Some(id) = override_id {
        id.to_string()
    } else if let Some(meta) = &resolved.meta {
        meta.agent_default
            .clone()
            .unwrap_or_else(|| "claude".into())
    } else {
        "claude".into()
    };
    agents::lookup("run", &id)
}

fn dispatch_run(resolved: &Pillbox, agent: Option<String>, mut opts: RunOpts) -> Result<()> {
    // Resolve the agent + apply pillbox.toml defaults; the backend
    // selection happens below.
    let spec = resolve_agent_spec(resolved, agent.as_deref())?;
    if let Some(meta) = &resolved.meta {
        if opts.name.is_none() {
            opts.name = Some(meta.name.clone());
        }
    }

    let remote_record = match opts.remote_name.as_deref() {
        Some(name) => Some(remote::read(resolved, name)?.ok_or_else(|| {
            PillboxError::runtime("run", format!("remote `{name}` not found"))
                .with_next(format!("pillbox remote add {name} ssh://user@host"))
        })?),
        None => None,
    };

    let backend = crate::sandbox::select_backend(remote_record);
    backend.run(spec, opts, resolved)
}

/// Validate an `--events-webhook URL`:
///   - non-empty after trim
///   - no embedded whitespace or control chars (so it can't smuggle
///     newlines into `Host:` headers or escape the shell wrapper)
///   - scheme is `http://` or `https://` (no `file://`, `gopher://`, …)
///   - warns to stderr if scheme is `http://` and the host isn't a
///     loopback / `.local` address — events contain session ids + exit
///     codes; in-cluster collectors are fine over plain HTTP, but a
///     remote cleartext endpoint is almost always a misconfig
///
/// Returns the trimmed URL ready to stash in the env var.
fn validate_events_webhook_url(url: &str) -> Result<String> {
    let trimmed = url.trim();
    if trimmed.is_empty() || trimmed.bytes().any(|b| b.is_ascii_whitespace() || b < 0x20) {
        return Err(PillboxError::usage(
            "run",
            "--events-webhook URL must be a single URL with no whitespace \
             or control characters",
        )
        .into());
    }
    if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
        return Err(PillboxError::usage(
            "run",
            format!(
                "--events-webhook URL must start with http:// or https:// \
                 (got `{trimmed}`)"
            ),
        )
        .into());
    }
    if let Some(host) = url_safety::plaintext_non_loopback_host(trimmed) {
        eprintln!(
            "pillbox: warning: --events-webhook is http:// to a non-loopback \
             host (`{host}`); session events will traverse the network in \
             cleartext. Use https:// in production."
        );
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_events_webhook_accepts_https() {
        let out = validate_events_webhook_url("https://collector.example.com/events").unwrap();
        assert_eq!(out, "https://collector.example.com/events");
    }

    #[test]
    fn validate_events_webhook_trims_whitespace() {
        let out = validate_events_webhook_url("  https://x/y  ").unwrap();
        assert_eq!(out, "https://x/y");
    }

    #[test]
    fn validate_events_webhook_rejects_embedded_whitespace() {
        let err = validate_events_webhook_url("https://x/y\nHost: evil").unwrap_err();
        assert!(format!("{err}").contains("whitespace"));
    }

    #[test]
    fn validate_events_webhook_rejects_non_http_scheme() {
        let err = validate_events_webhook_url("file:///etc/passwd").unwrap_err();
        assert!(format!("{err}").contains("http://"));
        let err = validate_events_webhook_url("//collector.example.com").unwrap_err();
        assert!(format!("{err}").contains("http://"));
    }

    #[test]
    fn validate_events_webhook_accepts_http_loopback_silently() {
        // 127.0.0.1, ::1, localhost, *.localhost — all valid http
        // collector targets for development; no warning emitted.
        for url in [
            "http://127.0.0.1:8080/events",
            "http://localhost:9000",
            "http://collector.localhost/x",
            "http://[::1]:9000/events",
        ] {
            let out = validate_events_webhook_url(url).unwrap();
            assert_eq!(out, url);
        }
    }

    #[test]
    fn validate_events_webhook_accepts_http_non_loopback() {
        // Plain http to a remote is allowed but warns on stderr (not
        // captured here). The function still returns Ok — pillbox
        // doesn't refuse the URL, just nudges the user.
        let url = "http://collector.example.com/events";
        let out = validate_events_webhook_url(url).unwrap();
        assert_eq!(out, url);
    }
}
