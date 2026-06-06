//! Thin wrapper over the `smolvm` CLI — SPIKE for adopting smol-machines/smolvm
//! (Apache-2.0) as a local microVM backend instead of maintaining our own
//! libkrun L1–L7. Shells out (mirrors [`crate::docker`]) rather than linking the
//! `smolvm` crate, so the spike compiles without the libkrun/libkrunfw toolchain.
//! See docs/managed-tier.md (leverage list) + docs/libkrun-sandbox.md.

use std::process::{Command, ExitStatus, Stdio};

use anyhow::{Context, Result};

/// `smolvm <args...>` with stdio inherited — the interactive PTY path. smolvm's
/// `machine run -it` gives the terminal directly, so the spike skips pillbox's
/// pty-host/attach layer (that's the §0 surface, out of spike scope).
pub fn run_interactive(args: &[String]) -> Result<ExitStatus> {
    Command::new("smolvm")
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context(
            "invoking `smolvm` — install it with \
             `curl -sSL https://smolmachines.com/install.sh | bash` (spike backend)",
        )
}
