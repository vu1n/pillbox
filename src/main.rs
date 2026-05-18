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
mod docker;
mod doctor;
mod envs;
mod errors;
mod paths;
mod secrets;

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
        #[arg(long)]
        json: bool,
    },
    /// Show a bundle's variables (values masked by default).
    Show {
        name: String,
        #[arg(long)]
        reveal: bool,
        #[arg(long, requires = "reveal")]
        to_stdout: bool,
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
            } => {
                let source = match from_env {
                    Some(var) => secrets::AddSource::EnvVar(var),
                    None => secrets::AddSource::Stdin,
                };
                secrets::add(&name, source, if_not_exists)
            }
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
            args,
        } => spec.run(RunOpts {
            workspace,
            name,
            mounts,
            withs,
            env_bundles,
            env_files,
            args,
        }),
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
    let mut root = serde_json::Map::new();
    root.insert("version".into(), serde_json::Value::Number(1.into()));
    root.insert("agents".into(), serde_json::Value::Array(arr));
    serde_json::Value::Object(root).to_string()
}

fn auth_rm(provider: &str) -> Result<()> {
    let spec = agents::ALL
        .iter()
        .find(|s| s.id() == provider)
        .ok_or_else(|| {
            PillboxError::usage(
                "auth rm",
                format!("unknown provider `{provider}`"),
            )
            .with_next("pillbox auth list  # see what's available")
        })?;
    if spec.forget()? {
        println!("Removed {provider} state.");
    } else {
        println!("No state stored for {provider}.");
    }
    Ok(())
}
