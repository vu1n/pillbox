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
use clap::{Parser, Subcommand};

mod agents;
mod config;
mod docker;
mod doctor;
mod envs;
mod errors;
mod paths;
mod pillbox;
mod sandbox;
mod secrets;
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
        /// Args forwarded to the agent CLI inside the sandbox.
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
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
        #[arg(long, value_name = "NAME")]
        tag: Option<String>,
        #[arg(long, short = 'm', value_name = "TEXT")]
        message: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Restore the workspace from a snapshot. Defaults to the latest.
    Pull {
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
}

#[derive(Subcommand, Debug)]
enum SnapshotAction {
    /// List every snapshot in the pillbox's repository.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show one snapshot's metadata. Accepts a unique prefix.
    Show {
        handle: String,
        #[arg(long)]
        json: bool,
    },
    /// Remove one snapshot. Data packs survive until a future `prune`.
    Rm { handle: String },
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
            args,
        } => {
            let resolved = Pillbox::resolve(pillbox_arg)?;
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
                },
            )
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
    agents::ALL
        .iter()
        .copied()
        .find(|s| s.id() == id)
        .ok_or_else(|| {
            let known: Vec<&str> = agents::ALL.iter().map(|s| s.id()).collect();
            PillboxError::usage(
                "run",
                format!("unknown agent `{id}` (known: {})", known.join(", ")),
            )
            .into()
        })
}

fn dispatch_run(resolved: &Pillbox, agent: Option<String>, mut opts: RunOpts) -> Result<()> {
    let spec = resolve_agent_spec(resolved, agent.as_deref())?;
    // Apply the `name` default from pillbox.toml when the CLI didn't
    // pass `--name`. The descriptor lives next to cwd; `Config::load_from`
    // would re-parse it, so we just consult the loaded meta.json instead.
    if let Some(meta) = &resolved.meta {
        if opts.name.is_none() {
            opts.name = Some(meta.name.clone());
        }
    }
    use crate::sandbox::SandboxBackend;
    crate::sandbox::local_docker::LocalDocker.run(spec, opts, resolved)
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
                if global {
                    WriteScope::Global
                } else {
                    WriteScope::Resolved
                },
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
        SecretAction::Rm { name, global } => secrets::rm(
            resolved,
            if global {
                WriteScope::Global
            } else {
                WriteScope::Resolved
            },
            &name,
        ),
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
            if global {
                WriteScope::Global
            } else {
                WriteScope::Resolved
            },
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
        EnvAction::Rm { name, global } => envs::rm(
            resolved,
            if global {
                WriteScope::Global
            } else {
                WriteScope::Resolved
            },
            &name,
        ),
    }
}

fn dispatch_auth(resolved: &Pillbox, action: AuthAction) -> Result<()> {
    match action {
        AuthAction::Login { agent, global } => {
            note_auth_global_is_implicit(global);
            let spec = agents::ALL
                .iter()
                .copied()
                .find(|s| s.id() == agent)
                .ok_or_else(|| {
                    PillboxError::usage("auth login", format!("unknown agent `{agent}`"))
                })?;
            // Auth is always global today — passing the resolved pillbox
            // keeps the API uniform for the v0.7 per-project override.
            spec.login(resolved)
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
            "pillbox: ✓ snapshot {} ({} bytes)",
            snap.handle.short(),
            snap.bytes
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
            }
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
                println!("  bytes:      {}", snap.bytes);
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
    serde_json::Value::Object(o)
}

fn snapshot_json(snap: &crate::workspace::Snapshot) -> String {
    paths::json_v1(vec![("snapshot", snapshot_value(snap))])
}
