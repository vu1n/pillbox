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
mod attach;
mod bookmarks;
mod cli;
mod commands;
mod config;
mod contract;
mod docker;
mod doctor;
mod envs;
mod errors;
mod events;
mod gateway;
mod memory;
mod paths;
mod pillbox;
mod registry;
mod remote;
mod sandbox;
mod sandboxes;
mod secrets;
mod session;
#[cfg(test)]
mod test_util;
mod url_safety;
mod vault;
mod workspace;

use agents::RunOpts;
use cli::{
    AuthAction, BookmarkAction, EnvAction, RemoteAction, SandboxAction, SecretAction,
    SessionAction, SnapshotAction, VaultAction, WorkspaceAction,
};
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
        /// Default agent for `pillbox run` (`claude` | `codex` | `opencode`).
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
        /// Agent to launch (`claude` | `codex` | `opencode`). Defaults to the current
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
        /// Wire in swarm memory (the external `kypp` engine, attached not owned): brief the agent
        /// from project memory at start and capture this session's §0 log after. Host-side, best-
        /// effort — a missing/erroring `kypp` warns, never fails the run. See github.com/vu1n/kypp.
        #[arg(long)]
        memory: bool,
        /// Attach a shared MCP server to the sandbox. NAME is the
        /// identifier the agent sees in its tool list; URL must be
        /// http:// or https://. `localhost` / `127.0.0.1` are
        /// rewritten to `host.docker.internal` so the sandbox can
        /// reach host-bound servers. Repeatable.
        /// Not supported with `--remote`.
        #[arg(long = "mcp", value_name = "NAME=URL", conflicts_with = "remote")]
        mcps: Vec<agents::McpAttachment>,
        /// Attach a bearer token to a `--mcp NAME=URL` from the
        /// pillbox secret store. NAME must match a `--mcp` entry;
        /// SECRET_NAME is the name passed to `pillbox secret add`.
        /// Claude folds it into a 0600 tempfile as
        /// `headers.Authorization: Bearer <value>`; Codex stashes
        /// it in an env var and references it via
        /// `bearer_token_env_var`. Token values never land in argv
        /// or shell history. Repeatable.
        #[arg(
            long = "mcp-token",
            value_name = "NAME=SECRET_NAME",
            conflicts_with = "remote"
        )]
        mcp_tokens: Vec<agents::McpTokenSpec>,
        /// Run on a remote: either a registered name (`pillbox remote add
        /// NAME …`) or an inline URL — `docker://[user@]host[:port]`,
        /// `ssh://user@host[:port]`, or `e2b://TEMPLATE_ID`. A value
        /// containing `://` is treated as an inline URL (no `remote add`
        /// needed). The agent launches inside a pillbox sandbox on the
        /// remote; the local terminal proxies the remote PTY.
        #[arg(long, value_name = "NAME|URL", conflicts_with_all = ["vault_stdin", "vault_stdin_direct"])]
        remote: Option<String>,
        /// Hidden: invoked by the remote side of `pillbox run --remote`.
        /// Reads a [`crate::sandbox::remote_ssh::VaultStdinBlob`] from
        /// stdin and runs the agent locally with the pre-resolved
        /// state. The SSH / VPS path: assumes Docker is available on the
        /// remote and the agent is already `pillbox auth login`'d there;
        /// runs the agent inside a nested runner-image container. Not for
        /// direct user consumption — the protocol is internal.
        #[arg(
            long = "vault-stdin",
            hide = true,
            conflicts_with = "vault_stdin_direct"
        )]
        vault_stdin: bool,
        /// Hidden: sandbox-side sibling of `--vault-stdin` for environments
        /// that already ARE an isolation boundary (e2b sandboxes). Reads
        /// the same blob, materializes the forwarded agent auth into
        /// `$HOME`, hydrates the workspace, and `exec`s the agent
        /// DIRECTLY — no nested Docker, no pre-existing login required.
        /// Selected by the e2b helper's wrapper.
        #[arg(long = "vault-stdin-direct", hide = true)]
        vault_stdin_direct: bool,
        /// Hidden: companion to `--vault-stdin` / `--vault-stdin-direct`.
        /// When set, the blob is read from this file instead of stdin. The
        /// ssh pty-host transport uses this so the child's stdin stays the
        /// PTY (the inner `docker run -it` needs a TTY on stdin); the
        /// launch path stages the blob to a remote temp file and points
        /// here. Meaningful only with one of the two vault-stdin flags.
        #[arg(long = "blob-file", value_name = "PATH", hide = true)]
        blob_file: Option<PathBuf>,
        /// Start the agent and immediately return — keeps the session
        /// alive in the background. Reattach later with `pillbox session
        /// attach <id>`. Works for local Docker, e2b:// remotes, and
        /// ssh:// remotes (the remote pty-host outlives the launch ssh
        /// session). (Local --detach doesn't support --vault: the proxy
        /// can't outlive the CLI.)
        #[arg(long)]
        detach: bool,
        /// Human label for the detached session (surfaced in `session
        /// list`). Only meaningful with `--detach` — clap rejects the
        /// flag without it instead of silently dropping the value.
        #[arg(long, value_name = "TEXT", requires = "detach")]
        label: Option<String>,
        /// Emit the started session as a JSON object on stdout instead
        /// of the human "session started" banner. Useful for
        /// orchestrators: `pillbox run --json | jq -r .session.id`.
        /// Needs a persisted session record: a `--detach` run (any
        /// agent) or a server-mode agent (opencode, always reparented).
        /// A foreground PTY run has nothing to emit — rejected at
        /// dispatch (not a clap `requires`, since server-mode validity
        /// depends on the resolved agent, not a flag).
        #[arg(long)]
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
        /// Start the run from a named snapshot bookmark. For remote
        /// runs this selects the base snapshot hydrated into the remote
        /// workspace. Without this flag, remote runs snapshot the current
        /// workspace at launch time and fork from that.
        #[arg(long = "from-bookmark", value_name = "NAME")]
        from_bookmark: Option<String>,
        /// Model for a server-integration agent (opencode): `PROVIDER/MODEL`,
        /// e.g. `zai-coding-plan/glm-4.5-air`. Ignored by PTY agents.
        #[arg(long, value_name = "PROVIDER/MODEL")]
        model: Option<String>,
        /// Extra host to allow through the libkrun egress fence (repeatable),
        /// for a custom/self-hosted model endpoint beyond the built-in set.
        #[arg(long = "egress-allow", value_name = "HOST")]
        egress_allow: Vec<String>,
        /// Args forwarded to the agent CLI inside the sandbox.
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Manage remotes (docker:// daemons, SSH VPSes, E2B sandboxes) for `pillbox run --remote NAME`.
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
        /// Also point a bookmark at the new snapshot — atomic snapshot+name,
        /// binding the bookmark to *this* push (avoids the handle-copy and the
        /// `latest` race of a separate `bookmark set`). Requires a project pillbox.
        #[arg(long, value_name = "NAME")]
        bookmark: Option<String>,
        /// Emit the snapshot record as JSON on stdout. Stable schema —
        /// pin against `version: 1`.
        #[arg(long)]
        json: bool,
    },
    /// Restore the workspace from a snapshot. Defaults to the latest.
    Pull {
        /// Snapshot to restore. Accepts a unique prefix (≥ 4 hex chars)
        /// or the full handle. Omit to restore the latest snapshot.
        #[arg(long, value_name = "HANDLE", conflicts_with = "bookmark")]
        snapshot: Option<String>,
        /// Bookmark to restore. Mutually exclusive with `--snapshot`.
        #[arg(long, value_name = "NAME", conflicts_with = "snapshot")]
        bookmark: Option<String>,
    },
    /// Inspect / manage the pillbox's snapshots.
    Snapshot {
        #[command(subcommand)]
        action: SnapshotAction,
    },
    /// Manage named bookmarks that point at snapshots.
    Bookmark {
        #[command(subcommand)]
        action: BookmarkAction,
    },
    /// Spawn / exec / destroy long-lived sandboxes (PTY-free container I/O).
    Sandbox {
        #[command(subcommand)]
        action: SandboxAction,
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
    /// Internal: run the interactive attach pty-host. Owns the agent's
    /// PTY + a screen model and serves the attach-transport frame
    /// protocol on a unix socket. Invoked by pillbox inside a sandbox
    /// (or locally); not a user-facing command. See docs/attach-transport.md.
    #[command(hide = true)]
    PtyHost {
        /// Unix socket to listen on for attach clients (docker / ssh backends).
        #[arg(long, value_name = "PATH")]
        sock: Option<String>,
        /// vsock port instead (libkrun guest backend; Linux-only). Default
        /// direction is guest-dials-host (foreground); `--vsock-listen` flips it
        /// to guest-listens for `--detach` (so the socket persists for reattach).
        #[arg(long, value_name = "PORT")]
        vsock_port: Option<u32>,
        /// With `--vsock-port`: the guest *listens* (detach), instead of dialing.
        #[arg(long)]
        vsock_listen: bool,
        /// Command to run under the PTY: everything after `--`.
        #[arg(last = true, value_name = "CMD")]
        argv: Vec<String>,
    },
    /// Internal: verbatim byte pump between a pty-host socket and stdio.
    /// Run inside a sandbox by the per-attach transport (docker exec / ssh)
    /// so one client's frames reach the pty-host. See docs/attach-transport.md.
    #[command(hide = true)]
    PtyRelay {
        /// Unix socket the in-sandbox `pty-host` is listening on.
        #[arg(long, value_name = "PATH")]
        sock: String,
    },
    /// Internal: guest-side opencode port-forward relay (libkrun; Linux-only).
    /// Listens on a vsock port and bridges each connection to an in-guest TCP
    /// port (the headless `opencode serve`), so the host speaks HTTP to a
    /// `Server`-mode agent over vsock. Not user-facing.
    #[command(hide = true)]
    VsockForward {
        /// vsock port the guest listens on (libkrun binds the host side).
        #[arg(long, value_name = "PORT")]
        vsock_port: u32,
        /// In-guest TCP port to forward each connection to (e.g. opencode 4096).
        #[arg(long, value_name = "PORT")]
        to_port: u16,
    },
    /// Internal: guest-side `codex app-server` bridge (libkrun codex-serve).
    /// Spawns `codex app-server`, does the JSON-RPC `initialize` + `thread/start`
    /// handshake, captures every notification line to `--events-file` (the §0
    /// source the host drains), auto-accepts approval requests (the sandbox is
    /// the boundary), and serves a small one-shot HTTP API on `--port`
    /// (`GET /health`, `POST /session`, `POST /turn`) the host drives over the
    /// vsock forward. Not user-facing. See `sandbox::appserver`.
    #[command(hide = true)]
    AppserverHost {
        /// Loopback TCP port to serve the host-facing HTTP API on.
        #[arg(long, value_name = "PORT")]
        port: u16,
        /// File to append each codex notification line to (NDJSON, the §0 source).
        #[arg(long, value_name = "PATH")]
        events_file: String,
        /// The `codex app-server` command to spawn: everything after `--`.
        /// Defaults to `codex app-server` when omitted.
        #[arg(last = true, value_name = "CMD")]
        argv: Vec<String>,
    },
}

fn main() -> ExitCode {
    // Internal re-exec entrypoint: the libkrun VMM child *becomes* the microVM.
    // `krun_start_enter` doesn't return — it `exit()`s with the guest's code when
    // the VM shuts down — so the backend spawns this hidden subprocess and
    // supervises it from the parent (which keeps a live process for attach +
    // cleanup). Not a user-facing command; argv is set by the backend.
    #[cfg(feature = "libkrun")]
    if std::env::args().nth(1).as_deref() == Some("__krun-vmm") {
        crate::sandbox::libkrun::vmm_child_main(); // never returns
    }
    // Internal re-exec entrypoint: the detached §0 PRODUCER for a reparented server
    // session. Tails the guest capture → durable log forever (until SIGTERM on
    // teardown) so the log stays live for every consumer. argv set by the backend:
    // [exe, __session-tailer, <session_dir>, <capture>, <format>, <sid>].
    if std::env::args().nth(1).as_deref() == Some("__session-tailer") {
        let a: Vec<String> = std::env::args().collect();
        let code = match (a.get(2), a.get(3), a.get(4), a.get(5)) {
            (Some(dir), Some(cap), Some(fmt), Some(sid)) => {
                match crate::events::EventsFormat::from_token(fmt) {
                    Some(fmt) => match crate::commands::session::run_detached_tailer(
                        dir.into(),
                        cap.into(),
                        fmt,
                        sid.clone(),
                    ) {
                        Ok(()) => 0,
                        Err(e) => {
                            eprintln!("pillbox __session-tailer: {e:#}");
                            1
                        }
                    },
                    None => {
                        eprintln!("pillbox __session-tailer: unknown format {fmt:?}");
                        2
                    }
                }
            }
            _ => {
                eprintln!("pillbox __session-tailer: usage: <session_dir> <capture> <format> <sid>");
                2
            }
        };
        return ExitCode::from(code as u8);
    }
    init_vault_trace();
    let cli = Cli::parse();
    let result = run(cli);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => errors::report(&e),
    }
}

/// Install a file-backed `tracing` subscriber when `PILLBOX_VAULT_TRACE`
/// names a path, so hudsucker's internal proxy errors (TLS / WebSocket)
/// become visible for debugging. Writes to a file rather than stderr so
/// an attached agent's raw-mode terminal isn't corrupted. Filter comes
/// from `RUST_LOG`, defaulting to everything hudsucker + pillbox emit at
/// debug. No-op (and zero overhead) when the var is unset.
fn init_vault_trace() {
    let Ok(path) = std::env::var("PILLBOX_VAULT_TRACE") else {
        return;
    };
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        eprintln!("pillbox: warning: could not open PILLBOX_VAULT_TRACE file `{path}`");
        return;
    };
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("hudsucker=debug,pillbox=debug"));
    // Per-event writer: clone the handle, falling back to a sink rather
    // than panicking — a debug logging facility must never crash the CLI.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(move || -> Box<dyn std::io::Write> {
            match file.try_clone() {
                Ok(f) => Box::new(f),
                Err(_) => Box::new(std::io::sink()),
            }
        })
        .with_ansi(false)
        .try_init();
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
            memory,
            mcps,
            mcp_tokens,
            remote,
            vault_stdin,
            vault_stdin_direct,
            blob_file,
            detach,
            label,
            json,
            events_webhook,
            ttl,
            parent,
            from_bookmark,
            model,
            egress_allow,
            args,
        } => {
            let resolved = Pillbox::resolve(pillbox_arg)?;
            // Hidden remote-side handlers. The blob carries everything we
            // need (agent id, args, env, secrets, auth); the rest of `run`
            // is ignored. clap rejects `--remote` + `--vault-stdin*` and
            // the two vault-stdin variants from being combined.
            if vault_stdin {
                return crate::sandbox::remote_ssh::dispatch_vault_stdin(
                    &resolved,
                    blob_file.as_deref(),
                );
            }
            if vault_stdin_direct {
                return crate::sandbox::remote_ssh::dispatch_vault_stdin_direct(
                    &resolved,
                    blob_file.as_deref(),
                );
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
                    memory,
                    memory_briefed: Vec::new(), // dispatch_run fills this from the kypp briefing
                    mcps,
                    mcp_tokens,
                    args,
                    remote_name: remote,
                    detach,
                    label,
                    json,
                    ttl_seconds,
                    from_bookmark,
                    model,
                    egress_allow,
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
        Command::Doctor { json } => {
            let resolved = Pillbox::resolve(pillbox_arg)?;
            doctor::run(json, &resolved)
        }
        Command::Version => {
            println!(
                "pillbox {} (runner image: {})",
                env!("CARGO_PKG_VERSION"),
                docker::default_runner_image()
            );
            Ok(())
        }
        Command::Push {
            tag,
            message,
            bookmark,
            json,
        } => {
            let resolved = Pillbox::resolve(pillbox_arg)?;
            commands::workspace::push(&resolved, tag, message, bookmark, json)
        }
        Command::Pull { snapshot, bookmark } => {
            let resolved = Pillbox::resolve(pillbox_arg)?;
            commands::workspace::pull(&resolved, snapshot, bookmark)
        }
        Command::Snapshot { action } => {
            let resolved = Pillbox::resolve(pillbox_arg)?;
            commands::workspace::snapshot_dispatch(&resolved, action)
        }
        Command::Bookmark { action } => {
            let resolved = Pillbox::resolve(pillbox_arg)?;
            commands::bookmark::dispatch(&resolved, action)
        }
        Command::Sandbox { action } => {
            let resolved = Pillbox::resolve(pillbox_arg)?;
            commands::sandbox::dispatch(&resolved, action)
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
        // Internal attach-transport commands. No pillbox resolution: they
        // operate on a raw PTY + socket and are invoked by pillbox itself.
        Command::PtyHost {
            sock,
            vsock_port,
            vsock_listen,
            argv,
        } => match (sock, vsock_port) {
            (Some(s), None) => attach::host::run(&s, &argv),
            (None, Some(port)) => pty_host_vsock(port, vsock_listen, &argv),
            _ => Err(PillboxError::usage(
                "pty-host",
                "exactly one of --sock or --vsock-port is required",
            )
            .into()),
        },
        Command::PtyRelay { sock } => attach::relay::run(&sock),
        Command::VsockForward {
            vsock_port,
            to_port,
        } => vsock_forward(vsock_port, to_port),
        Command::AppserverHost {
            port,
            events_file,
            argv,
        } => sandbox::appserver::run_host(port, &events_file, &argv),
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

    // `--json` emits the started-session record; only `--detach` runs and
    // server-mode agents (opencode, always reparented) persist one. A foreground
    // PTY run has nothing to emit, so reject loudly here rather than print
    // nothing. (Can't be a clap `requires` — server-mode validity depends on the
    // resolved agent, not a flag.)
    if opts.json && !opts.detach && spec.integration != crate::agents::Integration::Server {
        return Err(PillboxError::usage(
            "run",
            "--json needs a persisted session: add --detach, or run a server-mode agent (opencode)",
        )
        .with_next(format!("pillbox run --agent {} --detach --json", spec.id))
        .into());
    }

    // Resolve `--remote` (an inline URL or a registered name) to a record;
    // the name-vs-URL policy lives in the canonical `remote` module.
    let remote_record = opts
        .remote_name
        .as_deref()
        .map(|target| remote::resolve_run_target(resolved, target))
        .transpose()?;

    // Server-integration agents (opencode) run headless + are driven/read over
    // their HTTP API by the LocalDocker server path; the remote backends only
    // know the PTY launch, so `--remote` would silently pty-host `opencode serve`
    // and misbehave. Reject loudly here (the canonical dispatch point) rather
    // than let a wrong-path run through. Local-only until a server-mode remote
    // backend exists (the libkrun/managed work).
    if spec.integration == crate::agents::Integration::Server && remote_record.is_some() {
        return Err(PillboxError::usage(
            "run",
            format!(
                "`{}` runs as a headless server and is local-only today",
                spec.id
            ),
        )
        .with_next(format!("drop --remote: pillbox run --agent {}", spec.id))
        .into());
    }

    // Local runs only: nudge if Raindrop Workshop is installed but no
    // OTLP endpoint is set, so a silent "no events" doesn't surprise the
    // user. Remote sandboxes can't reach Workshop's localhost endpoint
    // anyway, so the hint would mislead there.
    if remote_record.is_none() {
        crate::events::hint_workshop_if_unconfigured();
    }

    let backend = crate::sandbox::select_backend(remote_record);
    if !opts.memory {
        return backend.run(spec, opts, resolved);
    }
    // --memory: brief the agent from kypp at start, capture THIS run's §0 log after. Host-side +
    // best-effort. Detached runs skip the post-capture — the agent outlives this call, so the §0 log
    // isn't complete yet; a scheduled `kypp sweep` (cron) catches them. Project scope = pillbox name.
    let project = opts.name.clone().unwrap_or_else(|| "default".to_string());
    let is_server = spec.integration == crate::agents::Integration::Server;
    let capture_after = !opts.detach;
    let briefed = crate::memory::brief_into_args(&mut opts.args, &project);
    opts.memory_briefed = briefed.clone(); // carried into the backend so a server bring-up can stash it
    let started = std::time::SystemTime::now(); // run-window start: capture only logs written after this
    let result = backend.run(spec, opts, resolved);
    // A FOREGROUND (PTY) agent finalizes its §0 log before `run` returns → capture now. A SERVER agent
    // (opencode) is reparented: the log drains LATER, so capturing here would race an empty log —
    // `session ingest` does the capture instead, from the brief the bring-up stashed (crate::memory).
    if capture_after && !is_server {
        let sessions = crate::session::sessions_root_path(resolved);
        crate::memory::capture_run(&sessions, &project, started);
        // record which briefed claims this run saw, for later credit assignment
        crate::memory::record_brief_usage(&sessions, &project, started, &briefed);
    }
    result
}

/// Dispatch `pty-host --vsock-port` — Linux-only. The libkrun guest serves the
/// attach frame protocol over vsock; the macOS host never runs this (it connects
/// to the bridged socket and pumps).
#[cfg(target_os = "linux")]
fn pty_host_vsock(port: u32, listen: bool, argv: &[String]) -> Result<()> {
    attach::host::run_vsock(port, listen, argv)
}
#[cfg(not(target_os = "linux"))]
fn pty_host_vsock(_port: u32, _listen: bool, _argv: &[String]) -> Result<()> {
    Err(PillboxError::usage("pty-host", "--vsock-port is Linux-only (the libkrun guest)").into())
}

/// Dispatch `vsock-forward` — Linux-only (the libkrun guest). Forwards a vsock
/// port to an in-guest TCP port so the host can reach a `Server`-mode agent's
/// HTTP API over vsock.
#[cfg(target_os = "linux")]
fn vsock_forward(vsock_port: u32, to_port: u16) -> Result<()> {
    attach::host::run_vsock_forward(vsock_port, to_port)
}
#[cfg(not(target_os = "linux"))]
fn vsock_forward(_vsock_port: u32, _to_port: u16) -> Result<()> {
    Err(PillboxError::usage("vsock-forward", "Linux-only (the libkrun guest)").into())
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
