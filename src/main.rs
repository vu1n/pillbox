//! pillbox — sandboxed coding agents with one-command auth.
//!
//! See README.md for the design rationale. High-level model:
//!
//! 1. `pillbox <agent> login` — boots a one-shot Docker sandbox, runs
//!    the agent's OAuth flow inside it, captures the resulting
//!    credentials, persists them to the OS keychain, destroys the
//!    sandbox.
//!
//! 2. `pillbox <agent> run [args]` — boots a fresh Docker sandbox with
//!    the saved credentials mounted in + the current working directory
//!    mounted at /workspace, attaches a PTY, runs the agent.
//!
//! 3. `pillbox auth list / rm` — manage stored credentials.
//!
//! Adding a new agent = adding one `AgentSpec` constant in `agents`
//! and one variant here.

use anyhow::Result;
use clap::{Parser, Subcommand};

mod agents;
mod docker;
mod keychain;

use agents::AgentSpec;

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
    /// Manage stored credentials.
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
}

#[derive(Subcommand, Debug)]
enum AgentAction {
    /// Run the OAuth flow inside a one-shot sandbox and store the
    /// resulting credentials in the OS keychain.
    Login,
    /// Launch the agent in a fresh sandbox with stored credentials + the
    /// current working directory mounted in.
    Run {
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
enum AuthAction {
    /// Show which providers have stored credentials.
    List,
    /// Remove a provider's stored credentials.
    Rm {
        /// Provider id (e.g. `claude`, `codex`).
        provider: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Claude { action } => dispatch_agent(agents::CLAUDE, action),
        Command::Codex { action } => dispatch_agent(agents::CODEX, action),
        Command::Auth { action } => match action {
            AuthAction::List => keychain::list(),
            AuthAction::Rm { provider } => keychain::remove(&provider),
        },
    }
}

fn dispatch_agent(spec: AgentSpec, action: AgentAction) -> Result<()> {
    match action {
        AgentAction::Login => spec.login(),
        AgentAction::Run { args } => spec.run(args),
    }
}
