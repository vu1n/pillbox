//! Stable exit codes + error-message formatting.
//!
//! Public contract documented in AGENTS.md. Agents script against these
//! codes; do NOT renumber without a major version bump.
//! Context: doc://pillbox/stable-exit-codes@0001#stable-exit-codes

use std::process::ExitCode;

use anyhow::Error;

/// Exit code emitted by `main()`. Agents and shell scripts depend on the
/// numeric values — see AGENTS.md "Exit codes" for the contract.
#[repr(u8)]
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)] // Success is here for the documented contract; main.rs uses ExitCode::SUCCESS directly.
pub(crate) enum ExitCategory {
    /// Operation completed successfully.
    Success = 0,
    /// Recoverable runtime error: secret/agent not yet set up, login
    /// expired, agent exited non-zero. Caller can usually fix by
    /// running the suggested `Next:` command.
    Runtime = 1,
    /// User error: bad flag, unknown subcommand, mutually-exclusive
    /// options. Caller passed something pillbox couldn't accept.
    Usage = 2,
    /// Configuration / persistent-state error: corrupt secret store,
    /// `.env` parse failure, file mode that can't be repaired.
    Config = 3,
    /// External resource not ready: Docker daemon down, runner image
    /// missing locally. Often needs host-level action outside pillbox.
    Resource = 4,
}

impl From<ExitCategory> for ExitCode {
    fn from(c: ExitCategory) -> Self {
        ExitCode::from(c as u8)
    }
}

/// Error type that pairs a user-facing failure with the exact recovery
/// command (if any) and the right exit category.
#[derive(Debug)]
pub(crate) struct PillboxError {
    pub(crate) action: &'static str,
    pub(crate) reason: String,
    pub(crate) next: Option<String>,
    pub(crate) category: ExitCategory,
}

// Some constructors are used only by modules that land later in v0.3
// (secrets, env bundles, doctor). Suppress dead-code warnings until
// those modules are wired up.
#[allow(dead_code)]
impl PillboxError {
    pub(crate) fn runtime(action: &'static str, reason: impl Into<String>) -> Self {
        Self {
            action,
            reason: reason.into(),
            next: None,
            category: ExitCategory::Runtime,
        }
    }

    pub(crate) fn usage(action: &'static str, reason: impl Into<String>) -> Self {
        Self {
            action,
            reason: reason.into(),
            next: None,
            category: ExitCategory::Usage,
        }
    }

    pub(crate) fn config(action: &'static str, reason: impl Into<String>) -> Self {
        Self {
            action,
            reason: reason.into(),
            next: None,
            category: ExitCategory::Config,
        }
    }

    pub(crate) fn resource(action: &'static str, reason: impl Into<String>) -> Self {
        Self {
            action,
            reason: reason.into(),
            next: None,
            category: ExitCategory::Resource,
        }
    }

    pub(crate) fn with_next(mut self, cmd: impl Into<String>) -> Self {
        self.next = Some(cmd.into());
        self
    }
}

impl std::fmt::Display for PillboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} failed. {}", self.action, self.reason)?;
        if let Some(next) = &self.next {
            write!(f, "\n  Next: {next}")?;
        }
        Ok(())
    }
}

impl std::error::Error for PillboxError {}

/// Print a top-level error in the documented format and return the
/// matching exit code. Anyhow errors without a `PillboxError` downcast
/// surface as runtime errors with no `Next:` line — least-info default.
pub(crate) fn report(err: &Error) -> ExitCode {
    if let Some(pb) = err.downcast_ref::<PillboxError>() {
        eprintln!("pillbox: {pb}");
        return pb.category.into();
    }
    eprintln!("pillbox: {err:#}");
    ExitCategory::Runtime.into()
}
