//! Shared helpers across integration test binaries.
//!
//! Each `tests/*.rs` file compiles to its own binary; cargo's convention
//! is `tests/common/mod.rs` (not `tests/common.rs`, which would itself
//! be a separate test binary). Each consumer adds `mod common;`.
//!
//! All three test files (`lifecycle.rs`, `sidecar.rs`, `workspace.rs`)
//! invoke the real `pillbox` binary against a temp `$HOME`, so the
//! invocation helpers live here.

use std::path::PathBuf;
use std::process::{Command, Output};

/// Resolved path to the `pillbox` binary that cargo built for this test.
pub fn pillbox_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_pillbox"))
}

/// Run `pillbox <args>` with `$HOME` overridden to `home` and cwd set
/// to `cwd`. Returns the captured `Output` for the caller to assert on.
#[allow(dead_code)] // each test binary uses a subset of this module
pub fn run(home: &std::path::Path, cwd: &std::path::Path, args: &[&str]) -> Output {
    run_with_env(home, cwd, &[], args)
}

/// Like [`run`] but with additional environment variables overlaid on
/// top of the parent's env. Used to exercise env-driven code paths
/// (e.g. `PILLBOX_SANDBOX_SIDE=1` for sandbox-side emitter detection)
/// without polluting the test process's own env or racing siblings.
#[allow(dead_code)]
pub fn run_with_env(
    home: &std::path::Path,
    cwd: &std::path::Path,
    envs: &[(&str, &str)],
    args: &[&str],
) -> Output {
    let mut cmd = Command::new(pillbox_bin());
    cmd.env("HOME", home).current_dir(cwd);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.args(args).output().expect("spawn pillbox")
}

/// Panic with a structured dump if `out` is a non-zero exit. `label`
/// tags the message so failures from chained runs are identifiable.
#[allow(dead_code)]
pub fn assert_ok(out: &Output, label: &str) {
    if !out.status.success() {
        panic!(
            "{label} failed: status={:?}\nstdout:\n{}\nstderr:\n{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
}
