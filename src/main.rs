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
mod config;
mod docker;
mod doctor;
mod envs;
mod errors;
mod paths;
mod pillbox;
mod registry;
mod remote;
mod sandbox;
mod secrets;
mod session;
#[cfg(test)]
mod test_util;
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
            remote,
            vault_stdin,
            detach,
            label,
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
                    args,
                    remote_name: remote,
                    detach,
                    label,
                },
            )
        }
        Command::Remote { action } => {
            let resolved = Pillbox::resolve(pillbox_arg)?;
            dispatch_remote(&resolved, action)
        }
        Command::Session { action } => {
            let resolved = Pillbox::resolve(pillbox_arg)?;
            dispatch_session(&resolved, action)
        }
        Command::Secret { action } => {
            let resolved = Pillbox::resolve(pillbox_arg)?;
            dispatch_secret(&resolved, action)
        }
        Command::Env { action } => {
            let resolved = Pillbox::resolve(pillbox_arg)?;
            dispatch_env(&resolved, action)
        }
        Command::Auth { action } => {
            // Auth always resolves to global in PR 2, but we still
            // resolve the current pillbox so `--pillbox NAME` works for
            // the v0.7 path forward without breaking the CLI shape now.
            let resolved = Pillbox::resolve(pillbox_arg)?;
            dispatch_auth(&resolved, action)
        }
        Command::Vault { action } => {
            let resolved = Pillbox::resolve(pillbox_arg)?;
            match action {
                VaultAction::Ca { json } => vault_ca(&resolved, json),
                VaultAction::Status { json } => vault_status(&resolved, json),
            }
        }
        Command::Sidecar { bind, json } => {
            let resolved = Pillbox::resolve(pillbox_arg)?;
            sidecar_run(&resolved, bind, json)
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
            dispatch_push(&resolved, tag, message, json)
        }
        Command::Pull { snapshot } => {
            let resolved = Pillbox::resolve(pillbox_arg)?;
            dispatch_pull(&resolved, snapshot)
        }
        Command::Snapshot { action } => {
            let resolved = Pillbox::resolve(pillbox_arg)?;
            dispatch_snapshot(&resolved, action)
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
            dispatch_workspace(&resolved, action)
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

fn dispatch_remote(resolved: &Pillbox, action: RemoteAction) -> Result<()> {
    match action {
        RemoteAction::Add {
            name,
            url,
            url_flag,
            agent,
            if_not_exists,
            global,
        } => {
            // clap's `conflicts_with` already rejects passing both, so
            // here we just pick whichever was given. Missing-both → a
            // pointed usage error rather than the generic "ARGS missing".
            let url = url.or(url_flag).ok_or_else(|| {
                PillboxError::usage(
                    "remote add",
                    "missing SSH URL — pass it positionally: \
                     `pillbox remote add NAME ssh://user@host`",
                )
            })?;
            remote::add(
                resolved,
                WriteScope::from_global_flag(global),
                &name,
                &url,
                agent,
                if_not_exists,
            )
        }
        RemoteAction::List { json } => remote::list(resolved, json),
        RemoteAction::Rm { name, global } => {
            remote::rm(resolved, WriteScope::from_global_flag(global), &name)
        }
        RemoteAction::Info { name, json } => remote::info(resolved, &name, json),
    }
}

fn dispatch_session(resolved: &Pillbox, action: SessionAction) -> Result<()> {
    match action {
        SessionAction::List { json } => session_list(resolved, json),
        SessionAction::Info { id, json } => session_info(resolved, &id, json),
        SessionAction::Attach { id } => session_attach(resolved, &id),
        SessionAction::Detach { id } => session_detach(resolved, &id),
        SessionAction::Rm { id } => session_rm(resolved, &id),
    }
}

fn session_list(resolved: &Pillbox, json: bool) -> Result<()> {
    let entries = session::list(resolved)?;
    if json {
        // Single source of truth for the on-wire shape lives on
        // `Session::to_json_value` so list + info stay in lockstep.
        let arr: Vec<serde_json::Value> = entries
            .iter()
            .map(session::Session::to_json_value)
            .collect();
        println!(
            "{}",
            crate::paths::json_v1(vec![
                ("pillbox", resolved.display_name().into()),
                ("sessions", serde_json::Value::Array(arr))
            ])
        );
        return Ok(());
    }
    if entries.is_empty() {
        println!("(no sessions in `{}`)", resolved.display_name());
        println!();
        println!("Start one with: pillbox run --remote NAME --detach");
        return Ok(());
    }
    println!(
        "Sessions in `{}` (id        attached?  agent    remote          started_at):",
        resolved.display_name()
    );
    for s in entries {
        let attached = match s.attached_pid {
            Some(_) => "active  ",
            None => "detached",
        };
        let label = s
            .label
            .as_deref()
            .map(|l| format!(" [{l}]"))
            .unwrap_or_default();
        println!(
            "  {}  {attached}  {:<7}  {:<14}  {}{label}",
            s.id, s.agent_id, s.remote, s.started_at
        );
    }
    Ok(())
}

fn session_info(resolved: &Pillbox, id: &str, json: bool) -> Result<()> {
    let s = session::resolve(resolved, id)?;
    if json {
        println!(
            "{}",
            crate::paths::json_v1(vec![("session", s.to_json_value())])
        );
        return Ok(());
    }
    println!("Session: {}", s.id);
    if let Some(label) = &s.label {
        println!("  label:        {label}");
    }
    println!("  remote:       {}", s.remote);
    println!("  backend:      {}", s.backend);
    println!("  sandbox_id:   {}", s.sandbox_id);
    println!("  pty_pid:      {}", s.pty_pid);
    println!("  agent:        {}", s.agent_id);
    println!("  started_at:   {}", s.started_at);
    println!(
        "  attached_pid: {}",
        match s.attached_pid {
            Some(p) => p.to_string(),
            None => "(detached)".to_string(),
        }
    );
    Ok(())
}

fn session_attach(resolved: &Pillbox, id: &str) -> Result<()> {
    let s = session::resolve(resolved, id)?;
    let remote = remote::read(resolved, &s.remote)?.ok_or_else(|| {
        PillboxError::runtime(
            "session attach",
            format!(
                "remote `{}` is no longer registered — session record is orphaned",
                s.remote
            ),
        )
        .with_next(format!("pillbox session rm {}", s.id))
    })?;
    match session::Backend::parse(&s.backend) {
        Some(session::Backend::E2b) => sandbox::remote_e2b::reattach(resolved, &remote, &s),
        Some(session::Backend::Ssh) => Err(PillboxError::usage(
            "session attach",
            "ssh session attach is not yet implemented (tmux integration lands next)",
        )
        .into()),
        None => Err(PillboxError::config(
            "session attach",
            format!("unknown session backend `{}`", s.backend),
        )
        .into()),
    }
}

fn session_detach(resolved: &Pillbox, id: &str) -> Result<()> {
    let s = session::resolve(resolved, id)?;
    let pid = match s.attached_pid {
        Some(p) => p,
        None => {
            println!("(session `{}` is already detached)", s.id);
            return Ok(());
        }
    };
    // SIGTERM the attached pillbox process. Its helper handles SIGTERM
    // → emits `detach-pressed` → exits 100 → that pillbox sees the
    // exit code, marks the session detached, and prints the reattach
    // hint. We clear the attached_pid field below as a belt-and-
    // suspenders — if the attached pillbox has crashed already, its
    // cleanup never ran.
    //
    // The session record is user-writable TOML — `attached_pid` could
    // be hand-edited (or stale after a pillbox crash + pid reuse).
    // Defenses, in order:
    //   1. Reject pid <= 1 and our own pid up front — kill(0, _)
    //      signals the whole process group; kill(-1, _) broadcasts;
    //      kill(1, _) targets init/launchd. None of those can ever be
    //      a pillbox we spawned.
    //   2. Range-check into libc::pid_t (i32 on Linux/macOS).
    //   3. Probe with kill(pid, 0) first: if the pid no longer exists
    //      we treat the session as already-detached without sending a
    //      signal to a recycled process.
    // We cannot fully defeat pid reuse races (the kernel could recycle
    // between probe and SIGTERM); these checks shrink the window and
    // reject the obviously-wrong cases.
    #[cfg(unix)]
    {
        let self_pid = i64::from(std::process::id());
        if pid <= 1 || pid == self_pid {
            return Err(PillboxError::runtime(
                "session detach",
                format!(
                    "refusing to signal pid {pid} (reserved / self) — session record may be \
                     corrupted; inspect `pillbox session info {}` and clear with `pillbox session rm`",
                    s.id
                ),
            )
            .into());
        }
        let target = libc::pid_t::try_from(pid).map_err(|_| {
            PillboxError::runtime("session detach", format!("pid {pid} out of range"))
        })?;
        // Liveness probe: signal 0 returns 0 iff the pid exists AND we
        // have permission to signal it. If ESRCH the attached pillbox
        // already exited; clear the stamp and return without firing
        // SIGTERM at whatever recycled pid now lives there.
        // SAFETY: signal 0 performs no signal delivery, only the pid
        // and permission checks. Always safe to call.
        let probe = unsafe { libc::kill(target, 0) };
        if probe != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ESRCH) {
                eprintln!("pillbox: warning: attached pid {pid} no longer exists; clearing stamp.");
                session::mark_detached(resolved, &s.id)?;
                return Ok(());
            }
            // EPERM or other: the pid exists but isn't ours to signal.
            // Refuse rather than try.
            return Err(PillboxError::runtime(
                "session detach",
                format!("kill probe pid {pid}: {err}"),
            )
            .into());
        }
        // SAFETY: SIGTERM to a validated pid we just confirmed is
        // signalable by this uid; no signal handler installed on this
        // side; we own the target process (it's another pillbox we
        // spawned).
        let rc = unsafe { libc::kill(target, libc::SIGTERM) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::ESRCH) {
                eprintln!("pillbox: warning: kill {pid}: {err}");
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        return Err(PillboxError::resource(
            "session detach",
            "session detach requires SIGTERM (Unix only) in v0.6",
        )
        .into());
    }
    session::mark_detached(resolved, &s.id)?;
    println!("pillbox: ✓ session `{}` detach signalled.", s.id);
    Ok(())
}

fn session_rm(resolved: &Pillbox, id: &str) -> Result<()> {
    let s = session::resolve(resolved, id)?;
    match session::Backend::parse(&s.backend) {
        Some(session::Backend::E2b) => sandbox::remote_e2b::kill_session(resolved, &s),
        Some(session::Backend::Ssh) => Err(PillboxError::usage(
            "session rm",
            "ssh session rm is not yet implemented (tmux integration lands next)",
        )
        .into()),
        None => Err(PillboxError::config(
            "session rm",
            format!("unknown session backend `{}`", s.backend),
        )
        .into()),
    }
}

fn dispatch_secret(resolved: &Pillbox, action: SecretAction) -> Result<()> {
    match action {
        SecretAction::Add {
            name,
            from_env,
            if_not_exists,
            global,
            vault,
            maps_to,
            host,
            header_scheme,
            prefix,
        } => {
            let source = match from_env {
                Some(var) => secrets::AddSource::EnvVar(var),
                None => secrets::AddSource::Stdin,
            };
            let vault_meta = resolve_vault_meta(
                &name,
                vault,
                maps_to.as_deref(),
                host.as_deref(),
                header_scheme.as_deref(),
                prefix.as_deref(),
            )?;
            secrets::add(
                resolved,
                WriteScope::from_global_flag(global),
                &name,
                source,
                if_not_exists,
                vault_meta,
            )
        }
        SecretAction::List { json } => secrets::list(resolved, json),
        SecretAction::Show {
            name,
            reveal,
            to_stdout,
            json,
        } => secrets::show(resolved, &name, reveal, to_stdout, json),
        SecretAction::Rm { name, global } => {
            secrets::rm(resolved, WriteScope::from_global_flag(global), &name)
        }
    }
}

fn dispatch_env(resolved: &Pillbox, action: EnvAction) -> Result<()> {
    match action {
        EnvAction::Load {
            name,
            path,
            if_not_exists,
            global,
        } => envs::load(
            resolved,
            WriteScope::from_global_flag(global),
            &name,
            &path,
            if_not_exists,
        ),
        EnvAction::List { json } => envs::list(resolved, json),
        EnvAction::Show {
            name,
            reveal,
            to_stdout,
            json,
        } => envs::show(resolved, &name, reveal, to_stdout, json),
        EnvAction::Rm { name, global } => {
            envs::rm(resolved, WriteScope::from_global_flag(global), &name)
        }
    }
}

fn dispatch_auth(resolved: &Pillbox, action: AuthAction) -> Result<()> {
    match action {
        AuthAction::Login { agent, global } => {
            note_auth_global_is_implicit(global);
            // Auth is always global today — passing the resolved pillbox
            // keeps the API uniform for the v0.7 per-project override.
            agents::lookup("auth login", &agent)?.login(resolved)
        }
        AuthAction::List { json, global } => {
            note_auth_global_is_implicit(global);
            auth_list(resolved, json)
        }
        AuthAction::Rm { provider, global } => {
            note_auth_global_is_implicit(global);
            auth_rm(resolved, &provider)
        }
    }
}

/// Auth always lives on the global pillbox in v0.6. Surface the implicit
/// behavior on stderr when the user explicitly passes `--global` so they
/// don't silently assume an alternate scope worked. Removed when v0.7
/// adds the per-project override.
fn note_auth_global_is_implicit(passed: bool) {
    if passed {
        eprintln!(
            "pillbox: note: auth always writes to the global pillbox in v0.6; `--global` is implicit."
        );
    }
}

fn auth_list(resolved: &Pillbox, json: bool) -> Result<()> {
    if json {
        println!("{}", build_auth_list_json(resolved));
        return Ok(());
    }
    // Auth currently always lives in global; show that explicitly so the
    // user understands `--global` is implicit.
    let auth_pb = agents::ALL[0].auth_pillbox(resolved);
    println!(
        "Persistent state under `{}` (auth/<provider>/):",
        auth_pb.display_name()
    );
    let mut any = false;
    for spec in agents::ALL {
        let home = spec.home_dir(resolved)?;
        if spec.is_authenticated(resolved) {
            println!("  {:<10} ✓ ({})", spec.id(), home.display());
            any = true;
        }
    }
    if !any {
        println!("  (none)");
        println!();
        println!("Run `pillbox auth login --agent claude` to authenticate.");
    }
    Ok(())
}

fn build_auth_list_json(resolved: &Pillbox) -> String {
    let arr: Vec<serde_json::Value> = agents::ALL
        .iter()
        .map(|spec| {
            let home = spec
                .home_dir(resolved)
                .ok()
                .map(|h| serde_json::Value::String(h.display().to_string()))
                .unwrap_or(serde_json::Value::Null);
            let mut o = serde_json::Map::new();
            o.insert("id".into(), serde_json::Value::String(spec.id().into()));
            o.insert("home".into(), home);
            o.insert(
                "authenticated".into(),
                serde_json::Value::Bool(spec.is_authenticated(resolved)),
            );
            serde_json::Value::Object(o)
        })
        .collect();
    paths::json_v1(vec![("agents", serde_json::Value::Array(arr))])
}

fn auth_rm(resolved: &Pillbox, provider: &str) -> Result<()> {
    let spec = agents::ALL
        .iter()
        .copied()
        .find(|s| s.id() == provider)
        .ok_or_else(|| {
            PillboxError::usage("auth rm", format!("unknown provider `{provider}`"))
                .with_next("pillbox auth list  # see what's available")
        })?;
    if spec.forget(resolved)? {
        println!("Removed {provider} state.");
    } else {
        println!("No state stored for {provider}.");
    }
    Ok(())
}

fn vault_ca(resolved: &Pillbox, json: bool) -> Result<()> {
    let ca_dir = resolved.subdir("vault")?;
    let ca = vault::Ca::ensure(&ca_dir)
        .map_err(|e| PillboxError::runtime("vault ca", format!("ensure ca: {e}")))?;
    if json {
        println!(
            "{}",
            paths::json_v1(vec![(
                "ca_cert_path",
                serde_json::Value::String(ca.cert_path().display().to_string()),
            )]),
        );
    } else {
        println!("{}", ca.cert_path().display());
    }
    Ok(())
}

fn vault_status(resolved: &Pillbox, json: bool) -> Result<()> {
    let ca_dir = resolved.subdir("vault")?;
    let ca_cert = vault::ca_cert_path_in(&ca_dir);
    let exists = ca_cert.exists();
    if json {
        let cert_path_val = if exists {
            serde_json::Value::String(ca_cert.display().to_string())
        } else {
            serde_json::Value::Null
        };
        println!(
            "{}",
            paths::json_v1(vec![
                ("ca_exists", serde_json::Value::Bool(exists)),
                (
                    "ca_dir",
                    serde_json::Value::String(ca_dir.display().to_string())
                ),
                ("ca_cert_path", cert_path_val),
                (
                    "pillbox",
                    serde_json::Value::String(resolved.display_name().into())
                ),
            ]),
        );
        return Ok(());
    }
    if exists {
        println!(
            "CA for `{}` exists at {}",
            resolved.display_name(),
            ca_cert.display()
        );
        println!();
        println!("Run `pillbox run --vault` to route agent traffic through the proxy.");
    } else {
        println!("No vault CA on disk yet for `{}`.", resolved.display_name());
        println!();
        println!("The CA is created lazily on first `pillbox run --vault`,");
        println!("or eagerly with `pillbox vault ca`.");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn resolve_vault_meta(
    name: &str,
    vault: bool,
    maps_to: Option<&str>,
    host: Option<&str>,
    header_scheme: Option<&str>,
    prefix: Option<&str>,
) -> Result<Option<vault::VaultMeta>> {
    if !vault {
        if maps_to.is_some() || host.is_some() || header_scheme.is_some() || prefix.is_some() {
            return Err(PillboxError::usage(
                "secret add",
                "--maps-to / --host / --header-scheme / --prefix require --vault",
            )
            .into());
        }
        return Ok(None);
    }

    if let Some(alias) = maps_to {
        let known = vault::known_secrets::lookup(alias).ok_or_else(|| {
            PillboxError::usage(
                "secret add",
                format!(
                    "--maps-to `{alias}` is not a known secret name. \
                     Known: ANTHROPIC_API_KEY, OPENAI_API_KEY, GITHUB_TOKEN (alias GH_TOKEN)"
                ),
            )
        })?;
        return Ok(Some(known.to_meta()));
    }

    let manual_count = [host.is_some(), header_scheme.is_some(), prefix.is_some()]
        .iter()
        .filter(|b| **b)
        .count();

    if manual_count == 0 {
        let known = vault::known_secrets::lookup(name).ok_or_else(|| {
            PillboxError::usage(
                "secret add",
                format!(
                    "`{name}` is not a known secret. Pass `--maps-to KNOWN` to alias \
                     it, or `--host H --header-scheme {{x-api-key|authorization-bearer}} --prefix P` \
                     to spell out the vault config."
                ),
            )
            .with_next(format!(
                "pillbox secret add {name} --vault --maps-to ANTHROPIC_API_KEY"
            ))
        })?;
        return Ok(Some(known.to_meta()));
    }

    if manual_count != 3 {
        return Err(PillboxError::usage(
            "secret add",
            "--host, --header-scheme, and --prefix must all be passed together",
        )
        .into());
    }

    let scheme = vault::HeaderScheme::parse(header_scheme.unwrap())
        .map_err(|e| PillboxError::usage("secret add", e))?;
    Ok(Some(vault::VaultMeta::new(
        host.unwrap().to_string(),
        scheme,
        prefix.unwrap().to_string(),
    )))
}

fn sidecar_run(resolved: &Pillbox, bind: Option<String>, json: bool) -> Result<()> {
    use std::net::SocketAddr;

    let bind_addr =
        match bind {
            Some(s) => Some(s.parse::<SocketAddr>().map_err(|e| {
                PillboxError::usage("sidecar", format!("invalid --bind `{s}`: {e}"))
            })?),
            None => None,
        };

    let ca_dir = resolved.subdir("vault")?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| PillboxError::runtime("sidecar", format!("tokio runtime: {e}")))?;

    let server = runtime
        .block_on(vault::Server::start(vault::ServerConfig {
            bind: bind_addr,
            ca_dir,
        }))
        .map_err(|e| PillboxError::runtime("sidecar", format!("start vault server: {e}")))?;

    let listen_addr = server.listen_addr();
    let ca_cert_path = server.ca_cert_path().to_path_buf();
    let pid = std::process::id();

    if json {
        println!(
            "{}",
            paths::json_v1(vec![
                (
                    "listen_addr",
                    serde_json::Value::String(listen_addr.to_string())
                ),
                (
                    "ca_cert_path",
                    serde_json::Value::String(ca_cert_path.display().to_string())
                ),
                ("pid", serde_json::Value::Number(pid.into())),
                (
                    "pillbox",
                    serde_json::Value::String(resolved.display_name().into())
                ),
            ]),
        );
    } else {
        println!(
            "pillbox sidecar listening on {listen_addr} (pillbox: {})",
            resolved.display_name()
        );
        println!("  ca_cert: {}", ca_cert_path.display());
        println!("  pid:     {pid}");
        println!();
        println!("Send SIGTERM (or Ctrl+C) to stop.");
    }
    use std::io::Write;
    let _ = std::io::stdout().flush();

    runtime.block_on(async {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .map_err(|e| PillboxError::runtime("sidecar", format!("install SIGTERM: {e}")))?;
        let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .map_err(|e| PillboxError::runtime("sidecar", format!("install SIGINT: {e}")))?;
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
        Ok::<(), PillboxError>(())
    })?;

    drop(server);
    Ok(())
}

// ── workspace dispatch ────────────────────────────────────────────────────

fn dispatch_push(
    resolved: &Pillbox,
    tag: Option<String>,
    message: Option<String>,
    json: bool,
) -> Result<()> {
    use crate::workspace::{PushOptions, WorkspaceBackend};
    let backend = resolved.workspace()?;
    let cwd = std::env::current_dir()
        .map_err(|e| PillboxError::runtime("push", format!("could not resolve cwd: {e}")))?;
    let snap = backend.push(&cwd, PushOptions { tag, message })?;
    if json {
        println!("{}", snapshot_json(&snap));
    } else {
        println!(
            "pillbox: ✓ snapshot {} ({})",
            snap.handle.short(),
            human_bytes(snap.bytes)
        );
        // `files_changed` from rustic counts files where content hash
        // moved, including newly added ones, so `files_new` is a subset
        // of `files_changed`. Surface both — "5 new, 12 changed (200
        // total)" reads more clearly than a single "changed" number.
        println!(
            "  files:      {} new, {} changed ({} total)",
            snap.files_new, snap.files_changed, snap.files_total
        );
        if let Some(t) = &snap.tag {
            println!("  tag:        {t}");
        }
        if let Some(m) = &snap.message {
            println!("  message:    {m}");
        }
        if let Some(a) = &snap.git_anchor {
            let dirty = if snap.git_dirty { " (dirty)" } else { "" };
            println!("  git anchor: {a}{dirty}");
        }
        println!("  created:    {}", snap.created_at);
    }
    Ok(())
}

/// Render `bytes` as a short human-readable string (`104 B`, `4.2 KB`,
/// `1.3 MB`, …). Used by push / snapshot list / snapshot show output.
/// Binary prefixes intentionally — restic/rustic dedup math is binary
/// too, so the units line up if anyone cross-checks against the repo.
fn human_bytes(b: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if b < KB {
        format!("{b} B")
    } else if b < MB {
        format!("{:.1} KB", b as f64 / KB as f64)
    } else if b < GB {
        format!("{:.1} MB", b as f64 / MB as f64)
    } else {
        format!("{:.2} GB", b as f64 / GB as f64)
    }
}

fn dispatch_pull(resolved: &Pillbox, snapshot: Option<String>) -> Result<()> {
    use crate::workspace::{SnapshotHandle, WorkspaceBackend};
    let backend = resolved.workspace()?;
    let cwd = std::env::current_dir()
        .map_err(|e| PillboxError::runtime("pull", format!("could not resolve cwd: {e}")))?;
    let handle = snapshot.as_ref().map(|s| SnapshotHandle::new(s.clone()));
    backend.pull(&cwd, handle.as_ref())?;
    let label = handle
        .as_ref()
        .map(|h| h.short().to_string())
        .unwrap_or_else(|| "latest".into());
    println!(
        "pillbox: ✓ restored snapshot {label} into {}",
        cwd.display()
    );
    Ok(())
}

fn dispatch_snapshot(resolved: &Pillbox, action: SnapshotAction) -> Result<()> {
    use crate::workspace::{SnapshotHandle, WorkspaceBackend};
    let backend = resolved.workspace()?;
    match action {
        SnapshotAction::List { json } => {
            let snaps = backend.snapshots()?;
            if json {
                let arr: Vec<serde_json::Value> = snaps.iter().map(snapshot_value).collect();
                println!(
                    "{}",
                    paths::json_v1(vec![
                        (
                            "pillbox",
                            serde_json::Value::String(resolved.display_name().into())
                        ),
                        ("snapshots", serde_json::Value::Array(arr)),
                    ])
                );
                return Ok(());
            }
            if snaps.is_empty() {
                println!("(no snapshots yet)");
                println!();
                println!("Run `pillbox push` to take the first snapshot.");
                return Ok(());
            }
            println!("Snapshots for `{}`:", resolved.display_name());
            for s in snaps {
                let tag = s
                    .tag
                    .as_deref()
                    .map(|t| format!(" [{t}]"))
                    .unwrap_or_default();
                println!("  {} {}{}", s.handle.short(), s.created_at, tag);
                if let Some(m) = &s.message {
                    println!("    {m}");
                }
                // git anchor — short SHA + dirty marker, mirroring
                // `git log --oneline`. Helps the user correlate a
                // snapshot back to a commit at a glance.
                if let Some(a) = &s.git_anchor {
                    let short = &a[..a.len().min(7)];
                    let dirty = if s.git_dirty { " (dirty)" } else { "" };
                    println!("    git {short}{dirty}");
                }
            }
            println!();
            println!("Use `pillbox snapshot show <HANDLE>` for details, `pillbox pull --snapshot <HANDLE>` to restore.");
        }
        SnapshotAction::Show { handle, json } => {
            let snap = backend.snapshot_show(&SnapshotHandle::new(handle))?;
            if json {
                println!("{}", snapshot_json(&snap));
            } else {
                println!("Snapshot {}", snap.handle);
                println!("  created:    {}", snap.created_at);
                if let Some(t) = &snap.tag {
                    println!("  tag:        {t}");
                }
                if let Some(m) = &snap.message {
                    println!("  message:    {m}");
                }
                if let Some(a) = &snap.git_anchor {
                    let dirty = if snap.git_dirty { " (dirty)" } else { "" };
                    println!("  git anchor: {a}{dirty}");
                }
                println!("  size:       {}", human_bytes(snap.bytes));
            }
        }
        SnapshotAction::Rm { handle } => {
            // `handle` may be a prefix the user typed; echo it back via
            // the canonical short form. Resolution already happened
            // inside `snapshot_rm`.
            let h = SnapshotHandle::new(handle.clone());
            backend.snapshot_rm(&h)?;
            println!("pillbox: ✓ removed snapshot {}", h.short());
        }
    }
    Ok(())
}

fn dispatch_workspace(resolved: &Pillbox, action: WorkspaceAction) -> Result<()> {
    use crate::workspace::WorkspaceBackend;
    let backend = resolved.workspace()?;
    match action {
        WorkspaceAction::Rekey => {
            backend.rekey()?;
            println!("pillbox: ✓ workspace password rotated");
            // rustic_core 0.11 exposes `add_key` but not a public
            // single-call "remove old key by password" — see the NOTE
            // in `RusticBackend::rekey`. Surface that explicitly so the
            // user isn't surprised when the previous password still
            // opens the repo. Drop this hint once rustic adds the API.
            println!();
            println!("note: rustic_core 0.11 cannot revoke the previous password from the repo;");
            println!("      treat the old password as compromised — back up + recreate the");
            println!("      pillbox if you need a hard cutover.");
        }
    }
    Ok(())
}

fn snapshot_value(snap: &crate::workspace::Snapshot) -> serde_json::Value {
    let mut o = serde_json::Map::new();
    o.insert(
        "handle".into(),
        serde_json::Value::String(snap.handle.as_str().into()),
    );
    o.insert(
        "short".into(),
        serde_json::Value::String(snap.handle.short().into()),
    );
    o.insert(
        "created_at".into(),
        serde_json::Value::String(snap.created_at.clone()),
    );
    o.insert(
        "tag".into(),
        snap.tag
            .as_deref()
            .map(|s| serde_json::Value::String(s.to_string()))
            .unwrap_or(serde_json::Value::Null),
    );
    o.insert(
        "message".into(),
        snap.message
            .as_deref()
            .map(|s| serde_json::Value::String(s.to_string()))
            .unwrap_or(serde_json::Value::Null),
    );
    o.insert(
        "git_anchor".into(),
        snap.git_anchor
            .as_deref()
            .map(|s| serde_json::Value::String(s.to_string()))
            .unwrap_or(serde_json::Value::Null),
    );
    o.insert("git_dirty".into(), serde_json::Value::Bool(snap.git_dirty));
    o.insert("bytes".into(), serde_json::Value::Number(snap.bytes.into()));
    o.insert(
        "files_new".into(),
        serde_json::Value::Number(snap.files_new.into()),
    );
    o.insert(
        "files_changed".into(),
        serde_json::Value::Number(snap.files_changed.into()),
    );
    o.insert(
        "files_total".into(),
        serde_json::Value::Number(snap.files_total.into()),
    );
    serde_json::Value::Object(o)
}

fn snapshot_json(snap: &crate::workspace::Snapshot) -> String {
    paths::json_v1(vec![("snapshot", snapshot_value(snap))])
}
