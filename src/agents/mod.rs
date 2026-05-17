//! Per-agent adapters. Each module owns: the OAuth login flow, the file
//! path where the agent stores its credentials inside its own home dir,
//! the command to launch the agent, and any environment variables.
//!
//! v0.1 has Claude Code only. Codex / opencode are v0.2.

pub mod claude;
