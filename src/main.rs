//! pillbox — sandboxed coding agents with one-command auth.
//!
//! See README.md for the design rationale. The high-level model:
//!
//! 1. `pillbox <agent> login` — boots a one-shot Docker sandbox, runs the
//!    agent's OAuth flow inside it, captures the resulting credentials,
//!    persists them to the OS keychain, destroys the sandbox.
//!
//! 2. `pillbox <agent> run [args]` — boots a fresh Docker sandbox with
//!    the saved credentials mounted in + the current working directory
//!    mounted at /workspace, attaches a PTY, runs the agent.
//!
//! 3. `pillbox auth list / rm` — manage stored credentials.
//!
//! v0.1 supports Claude Code only. Codex / opencode adapters are v0.2.

use anyhow::Result;
use clap::{Parser, Subcommand};

mod agents;
mod docker;
mod keychain;

#[derive(Parser, Debug)]
#[command(name = "pillbox", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Claude Code agent commands (`pillbox claude login` / `pillbox claude run`).
    Claude {
        #[command(subcommand)]
        action: ClaudeAction,
    },
    /// Manage stored credentials.
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
}

#[derive(Subcommand, Debug)]
enum ClaudeAction {
    /// Run the Claude Code OAuth flow inside a one-shot sandbox and store
    /// the resulting credentials in the OS keychain.
    Login,
    /// Run Claude Code in a fresh sandbox with stored credentials + the
    /// current working directory mounted in.
    Run {
        /// Arguments to forward to `claude` inside the sandbox.
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
        /// Provider to remove (e.g. `claude`).
        provider: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Claude { action } => match action {
            ClaudeAction::Login => agents::claude::login(),
            ClaudeAction::Run { args } => agents::claude::run(args),
        },
        Command::Auth { action } => match action {
            AuthAction::List => keychain::list(),
            AuthAction::Rm { provider } => keychain::remove(&provider),
        },
    }
}
