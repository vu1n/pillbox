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
mod errors;

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
        /// instead of `/workspace/<basename-of-workspace>`. Useful when
        /// pillbox is driven by automation (e.g. lum spawning per-thread
        /// workspaces with synthetic ids).
        #[arg(long, value_name = "NAME")]
        name: Option<String>,

        /// Extra bind mount, passed through to `docker run -v`.
        /// Repeatable: `--mount ~/.aws:/home/lum/.aws:ro --mount /tmp:/scratch`.
        #[arg(long = "mount", value_name = "HOST:GUEST")]
        mounts: Vec<String>,

        /// Args forwarded to the agent CLI inside the sandbox.
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
enum AuthAction {
    /// Show which providers have authenticated state on disk.
    List,
    /// Remove a provider's persistent state (`~/.pillbox/data/<provider>/`).
    Rm {
        /// Provider id (e.g. `claude`, `codex`).
        provider: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Claude { action } => dispatch_agent(agents::CLAUDE, action),
        Command::Codex { action } => dispatch_agent(agents::CODEX, action),
        Command::Auth { action } => match action {
            AuthAction::List => auth_list(),
            AuthAction::Rm { provider } => auth_rm(&provider),
        },
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
            args,
        } => spec.run(RunOpts {
            workspace,
            name,
            mounts,
            args,
        }),
    }
}

fn auth_list() -> Result<()> {
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
