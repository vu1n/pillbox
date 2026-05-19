//! pillbox — sandboxed coding agents with one-command auth.
//!
//! See README.md for the design rationale. High-level model:
//!
//! 1. `pillbox <agent> login` — boots a one-shot Docker sandbox, runs
//!    the agent's OAuth flow inside it. Whatever the agent writes to
//!    HOME during login persists at `~/.pillbox/data/<provider>/`.
//!
//! 2. `pillbox <agent> run [args]` — boots a fresh Docker sandbox with
//!    that persistent HOME mounted in + the current working directory
//!    mounted at `/workspace/<name>`, attaches a PTY, runs the agent.
//!
//! 3. `pillbox auth list / rm` — show / forget persistent state.

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
mod sandbox;
mod secrets;
#[cfg(test)]
mod test_util;
mod vault;

use agents::{AgentSpec, RunOpts};
use errors::PillboxError;

#[derive(Parser, Debug)]
#[command(name = "pillbox", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Claude Code agent.
    Claude {
        #[command(subcommand)]
        action: AgentAction,
    },
    /// OpenAI Codex agent.
    Codex {
        #[command(subcommand)]
        action: AgentAction,
    },
    /// Inspect or remove persisted agent state.
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
    /// Manage named secrets (single value → env var).
    Secret {
        #[command(subcommand)]
        action: SecretAction,
    },
    /// Manage env bundles (whole .env files, loaded as a unit).
    Env {
        #[command(subcommand)]
        action: EnvAction,
    },
    /// Show the resolved pillbox.toml (if any) for the current directory.
    Config {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Inspect the credential vault (CA cert path, status).
    Vault {
        #[command(subcommand)]
        action: VaultAction,
    },
    /// Run the credential vault as a standalone sidecar process.
    ///
    /// v0.6 scaffolding for the local-or-remote sandbox model: PR 2 will
    /// `ssh remote pillbox sidecar` to host the vault next to a remote
    /// agent run. For PR 1 the subcommand only verifies the standalone
    /// vault-server mode boots and is killable; the credential-accept
    /// path lands in PR 2.
    Sidecar {
        /// Listen address for the proxy. Defaults to `127.0.0.1:0`
        /// (pick a free port).
        #[arg(long, value_name = "ADDR")]
        bind: Option<String>,
        /// Emit the listen address + CA cert path + pid as JSON to
        /// stdout instead of human text.
        #[arg(long)]
        json: bool,
    },
    /// Diagnose pillbox's environment (Docker, image, perms).
    Doctor {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print pillbox version + the runner image tag it targets.
    Version,
}

#[derive(Subcommand, Debug)]
enum VaultAction {
    /// Print the path to the vault CA cert (created on first vault use).
    Ca {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print vault state: whether a CA exists, where its files live.
    Status {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
enum AgentAction {
    /// Run the OAuth flow inside a one-shot sandbox and persist the
    /// resulting state under `~/.pillbox/data/<provider>/`.
    Login,
    /// Launch the agent in a fresh sandbox with stored state and a
    /// project directory mounted in.
    Run {
        /// Host path to mount as the workspace. Defaults to the current
        /// working directory.
        #[arg(long, value_name = "PATH")]
        workspace: Option<PathBuf>,

        /// Override the workspace mount-point name inside the guest.
        /// The agent's working directory becomes `/workspace/<name>`
        /// instead of `/workspace/<basename-of-workspace>`.
        #[arg(long, value_name = "NAME")]
        name: Option<String>,

        /// Extra bind mount, passed through to `docker run -v`.
        /// Repeatable: `--mount ~/.aws:/home/lum/.aws:ro`.
        #[arg(long = "mount", value_name = "HOST:GUEST")]
        mounts: Vec<String>,

        /// Inject a stored secret as an env var in the sandbox.
        /// `NAME` alone binds to `NAME`; `NAME=ENV_VAR` rebinds.
        /// Repeatable. Highest precedence in the env composition order.
        #[arg(long = "with", value_name = "NAME[=ENV_VAR]")]
        withs: Vec<String>,

        /// Inject every variable from a stored env bundle.
        /// Repeatable. Lowest precedence — overridden by --env-file and --with.
        #[arg(long = "env", value_name = "BUNDLE")]
        env_bundles: Vec<String>,

        /// Inject every variable from a `.env`-formatted file on disk.
        /// No persistence. Resolved relative to the current working
        /// directory at invocation time. Repeatable.
        #[arg(long = "env-file", value_name = "PATH")]
        env_files: Vec<PathBuf>,

        /// Load defaults from a specific pillbox.toml. Disables discovery.
        #[arg(long, value_name = "PATH", conflicts_with = "no_config")]
        config: Option<PathBuf>,

        /// Skip pillbox.toml discovery entirely.
        #[arg(long)]
        no_config: bool,

        /// Route the agent's API traffic through pillbox's vault proxy.
        /// Real OAuth tokens stay on the host; the guest sees stubs.
        /// Anthropic-only in v0.4; only `claude` is supported.
        #[arg(long)]
        vault: bool,

        /// Args forwarded to the agent CLI inside the sandbox.
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
enum AuthAction {
    /// Show which providers have authenticated state on disk.
    List {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Remove a provider's persistent state (`~/.pillbox/data/<provider>/`).
    Rm {
        /// Provider id (e.g. `claude`, `codex`).
        provider: String,
    },
}

#[derive(Subcommand, Debug)]
enum SecretAction {
    /// Store a secret value (reads from stdin by default).
    Add {
        /// Secret name. ASCII alphanumeric + `_`, `-`, `.` only.
        name: String,
        /// Read the value from this host env var instead of stdin.
        #[arg(long, value_name = "VAR")]
        from_env: Option<String>,
        /// Fail (exit 1) if the secret already exists. Default is silent overwrite.
        #[arg(long)]
        if_not_exists: bool,
        /// Mark this secret as vaulted: at `run` time, the real value
        /// stays on the host and the guest sees a per-sandbox stub
        /// that gets swapped at the pillbox proxy. Known names
        /// (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GITHUB_TOKEN`,
        /// `GH_TOKEN`) auto-fill the host/scheme/prefix; for anything
        /// else, pass `--maps-to` or `--host`+`--header-scheme`+`--prefix`.
        #[arg(long)]
        vault: bool,
        /// Alias this secret to a known name's vault config (e.g.
        /// `--maps-to ANTHROPIC_API_KEY` reuses Anthropic's host /
        /// scheme / prefix). Mutually exclusive with manual --host/--header-scheme/--prefix.
        #[arg(
            long,
            value_name = "KNOWN_NAME",
            requires = "vault",
            conflicts_with_all = ["host", "header_scheme", "prefix"],
        )]
        maps_to: Option<String>,
        /// Vault host the secret talks to (e.g. `api.anthropic.com`).
        #[arg(long, value_name = "HOST", requires = "vault")]
        host: Option<String>,
        /// Header scheme used by the upstream: `x-api-key` or `authorization-bearer`.
        #[arg(long = "header-scheme", value_name = "SCHEME", requires = "vault")]
        header_scheme: Option<String>,
        /// Stub prefix the minted token should mimic (e.g. `sk-ant-api03-`).
        #[arg(long, value_name = "PREFIX", requires = "vault")]
        prefix: Option<String>,
    },
    /// List stored secret names.
    List {
        /// Emit machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },
    /// Show a secret's value (masked by default).
    Show {
        name: String,
        /// Print the plain value. Refuses if stdout is not a TTY unless --to-stdout is set.
        #[arg(long)]
        reveal: bool,
        /// Acknowledge writing the revealed value to a non-TTY (pipe / file).
        #[arg(long, requires = "reveal")]
        to_stdout: bool,
        /// Emit machine-readable JSON instead of human text.
        #[arg(long)]
        json: bool,
    },
    /// Delete a stored secret.
    Rm { name: String },
}

#[derive(Subcommand, Debug)]
enum EnvAction {
    /// Parse a `.env`-formatted file and persist it as a named bundle.
    Load {
        /// Bundle name.
        name: String,
        /// Path to the `.env` file to load (resolved against cwd at invocation time).
        path: PathBuf,
        /// Fail if a bundle by this name already exists.
        #[arg(long)]
        if_not_exists: bool,
    },
    /// List stored bundles.
    List {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show a bundle's variables (values masked by default).
    Show {
        name: String,
        /// Print plain values. Refuses if stdout is not a TTY unless --to-stdout is set.
        #[arg(long)]
        reveal: bool,
        /// Acknowledge writing the revealed values to a non-TTY (pipe / file).
        #[arg(long, requires = "reveal")]
        to_stdout: bool,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Delete a stored bundle.
    Rm { name: String },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Claude { action } => dispatch_agent(agents::CLAUDE, action),
        Command::Codex { action } => dispatch_agent(agents::CODEX, action),
        Command::Auth { action } => match action {
            AuthAction::List { json } => auth_list(json),
            AuthAction::Rm { provider } => auth_rm(&provider),
        },
        Command::Secret { action } => match action {
            SecretAction::Add {
                name,
                from_env,
                if_not_exists,
                vault,
                maps_to,
                host,
                header_scheme,
                prefix,
            } => secret_add(
                name,
                from_env,
                if_not_exists,
                vault,
                maps_to,
                host,
                header_scheme,
                prefix,
            ),
            SecretAction::List { json } => secrets::list(json),
            SecretAction::Show {
                name,
                reveal,
                to_stdout,
                json,
            } => secrets::show(&name, reveal, to_stdout, json),
            SecretAction::Rm { name } => secrets::rm(&name),
        },
        Command::Env { action } => match action {
            EnvAction::Load {
                name,
                path,
                if_not_exists,
            } => envs::load(&name, &path, if_not_exists),
            EnvAction::List { json } => envs::list(json),
            EnvAction::Show {
                name,
                reveal,
                to_stdout,
                json,
            } => envs::show(&name, reveal, to_stdout, json),
            EnvAction::Rm { name } => envs::rm(&name),
        },
        Command::Config { json } => show_config(json),
        Command::Vault { action } => match action {
            VaultAction::Ca { json } => vault_ca(json),
            VaultAction::Status { json } => vault_status(json),
        },
        Command::Sidecar { bind, json } => sidecar_run(bind, json),
        Command::Doctor { json } => doctor::run(json),
        Command::Version => {
            println!(
                "pillbox {} (runner image: {})",
                env!("CARGO_PKG_VERSION"),
                docker::RUNNER_IMAGE
            );
            Ok(())
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => errors::report(&e),
    }
}

fn dispatch_agent(spec: AgentSpec, action: AgentAction) -> Result<()> {
    match action {
        AgentAction::Login => spec.login(),
        AgentAction::Run {
            workspace,
            name,
            mounts,
            withs,
            env_bundles,
            env_files,
            config,
            no_config,
            vault,
            args,
        } => {
            let mut opts = RunOpts {
                workspace,
                name,
                mounts,
                withs,
                env_bundles,
                env_files,
                vault,
                args,
            };
            opts.apply_defaults(config::Config::resolve(config, no_config)?);
            // Backend selection lives here, at the CLI/config seam, so a
            // future `--target` flag or `pillbox.toml` field can pick
            // between local Docker and a remote backend without
            // threading state through `AgentSpec`. PR 1 only ships
            // LocalDocker; PR 2 adds the remote arm.
            use crate::sandbox::SandboxBackend;
            crate::sandbox::local_docker::LocalDocker.run(&spec, opts)
        }
    }
}

fn auth_list(json: bool) -> Result<()> {
    if json {
        println!("{}", build_auth_list_json());
        return Ok(());
    }
    println!("Persistent state under ~/.pillbox/data/:");
    let mut any = false;
    for spec in agents::ALL {
        let home = spec.home_dir()?;
        if spec.is_authenticated() {
            println!("  {:<10} ✓ ({})", spec.id(), home.display());
            any = true;
        }
    }
    if !any {
        println!("  (none)");
        println!();
        println!("Run `pillbox claude login` to authenticate.");
    }
    Ok(())
}

fn show_config(json: bool) -> Result<()> {
    let cfg = config::Config::discover()?;
    if json {
        println!(
            "{}",
            paths::json_v1(vec![("config", config_json_payload(&cfg))])
        );
        return Ok(());
    }
    match cfg {
        Some(c) => {
            println!("Loaded from: {}", c.source.as_ref().unwrap().display());
            if let Some(n) = &c.name {
                println!("  name      = {n}");
            }
            if let Some(e) = &c.env {
                println!("  env       = {e}");
            }
            if !c.with.is_empty() {
                println!("  with      = {:?}", c.with);
            }
            if !c.mount.is_empty() {
                println!("  mount     = {:?}", c.mount);
            }
            if !c.env_file.is_empty() {
                println!("  env_file  = {:?}", c.env_file);
            }
        }
        None => {
            println!("No pillbox.toml found between cwd and filesystem root.");
            println!();
            println!("Create one to set per-project defaults. Example:");
            println!("  name = \"myapp\"");
            println!("  env = \"dev\"");
            println!("  with = [\"ANTHROPIC_API_KEY\"]");
        }
    }
    Ok(())
}

fn config_json_payload(cfg: &Option<config::Config>) -> serde_json::Value {
    let mut o = serde_json::Map::new();
    let source = cfg
        .as_ref()
        .and_then(|c| c.source.as_ref())
        .map(|p| serde_json::Value::String(p.display().to_string()))
        .unwrap_or(serde_json::Value::Null);
    o.insert("source".into(), source);
    if let Some(c) = cfg {
        if let Some(n) = &c.name {
            o.insert("name".into(), serde_json::Value::String(n.clone()));
        }
        if let Some(e) = &c.env {
            o.insert("env".into(), serde_json::Value::String(e.clone()));
        }
        o.insert("with".into(), json_string_array(&c.with));
        o.insert("mount".into(), json_string_array(&c.mount));
        o.insert("env_file".into(), json_string_array(&c.env_file));
    }
    serde_json::Value::Object(o)
}

fn json_string_array(items: &[String]) -> serde_json::Value {
    serde_json::Value::Array(
        items
            .iter()
            .map(|s| serde_json::Value::String(s.clone()))
            .collect(),
    )
}

fn vault_ca(json: bool) -> Result<()> {
    let ca_dir = paths::data_subdir("vault")?;
    let ca = vault::Ca::ensure(&ca_dir)
        .map_err(|e| errors::PillboxError::runtime("vault ca", format!("ensure ca: {e}")))?;
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

fn vault_status(json: bool) -> Result<()> {
    let ca_dir = paths::data_subdir("vault")?;
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
            ]),
        );
        return Ok(());
    }
    if exists {
        println!("CA exists at {}", ca_cert.display());
        println!();
        println!("Run `pillbox claude run --vault` to route claude traffic through the proxy.");
    } else {
        println!("No vault CA on disk yet.");
        println!();
        println!("The CA is created lazily on first `pillbox claude run --vault`,");
        println!("or eagerly with `pillbox vault ca`.");
    }
    Ok(())
}

fn build_auth_list_json() -> String {
    let arr: Vec<serde_json::Value> = agents::ALL
        .iter()
        .map(|spec| {
            let home = spec
                .home_dir()
                .ok()
                .map(|h| serde_json::Value::String(h.display().to_string()))
                .unwrap_or(serde_json::Value::Null);
            let mut o = serde_json::Map::new();
            o.insert("id".into(), serde_json::Value::String(spec.id().into()));
            o.insert("home".into(), home);
            o.insert(
                "authenticated".into(),
                serde_json::Value::Bool(spec.is_authenticated()),
            );
            serde_json::Value::Object(o)
        })
        .collect();
    paths::json_v1(vec![("agents", serde_json::Value::Array(arr))])
}

#[allow(clippy::too_many_arguments)] // mirrors the SecretAction::Add fields 1-to-1
fn secret_add(
    name: String,
    from_env: Option<String>,
    if_not_exists: bool,
    vault: bool,
    maps_to: Option<String>,
    host: Option<String>,
    header_scheme: Option<String>,
    prefix: Option<String>,
) -> Result<()> {
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
    secrets::add(&name, source, if_not_exists, vault_meta)
}

/// Build a [`vault::VaultMeta`] from the user-provided flags on
/// `pillbox secret add`. Mutual-exclusion and "manual flags must come
/// together" are enforced here so the call-site stays linear.
fn resolve_vault_meta(
    name: &str,
    vault: bool,
    maps_to: Option<&str>,
    host: Option<&str>,
    header_scheme: Option<&str>,
    prefix: Option<&str>,
) -> Result<Option<vault::VaultMeta>> {
    if !vault {
        // Sanity: clap's `requires = "vault"` should already block this,
        // but defend against future refactors that strip the requires.
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
        // No manual flags: try the known-secrets registry by name.
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

/// Run the credential vault as a standalone process. PR 1 scope:
/// start the server, print its listen address + CA cert path so a
/// caller can attach, and block until SIGTERM / SIGINT. PR 2 will
/// add the credentials-accept endpoint so a remote `pillbox run`
/// orchestrator can ship real creds in over a control channel.
fn sidecar_run(bind: Option<String>, json: bool) -> Result<()> {
    use std::net::SocketAddr;

    let bind_addr =
        match bind {
            Some(s) => Some(s.parse::<SocketAddr>().map_err(|e| {
                PillboxError::usage("sidecar", format!("invalid --bind `{s}`: {e}"))
            })?),
            None => None,
        };

    let ca_dir = paths::data_subdir("vault")?;

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
                    serde_json::Value::String(listen_addr.to_string()),
                ),
                (
                    "ca_cert_path",
                    serde_json::Value::String(ca_cert_path.display().to_string()),
                ),
                ("pid", serde_json::Value::Number(pid.into())),
            ]),
        );
    } else {
        println!("pillbox sidecar listening on {listen_addr}");
        println!("  ca_cert: {}", ca_cert_path.display());
        println!("  pid:     {pid}");
        println!();
        println!("Send SIGTERM (or Ctrl+C) to stop. PR 2 wires the credential-accept side.");
    }
    // Flush so a parent reading stdout line-buffered sees the address
    // before it blocks on SIGTERM.
    use std::io::Write;
    let _ = std::io::stdout().flush();

    // Block until SIGTERM or SIGINT. Server is dropped after the block,
    // which signals graceful shutdown to the proxy task.
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

fn auth_rm(provider: &str) -> Result<()> {
    let spec = agents::ALL
        .iter()
        .find(|s| s.id() == provider)
        .ok_or_else(|| {
            PillboxError::usage("auth rm", format!("unknown provider `{provider}`"))
                .with_next("pillbox auth list  # see what's available")
        })?;
    if spec.forget()? {
        println!("Removed {provider} state.");
    } else {
        println!("No state stored for {provider}.");
    }
    Ok(())
}
