//! Shared test helpers. Cfg-gated so production builds drop it.
//!
//! Several test modules (`secrets`, `agents`, `config`, …) need to run
//! code that reads `$HOME` (e.g. `secrets_dir()`, `expand_tilde`).
//! Mutating `$HOME` in-process is inherently racy under cargo's
//! multi-threaded test runner, so we share a single mutex
//! ([`paths::TEST_HOME_LOCK`]) across every caller and wrap the
//! mutation in [`with_isolated_home`].

#![cfg(test)]

use std::path::PathBuf;

/// Run `body` with `$HOME` pointed at a fresh tempdir, then restore the
/// previous value and clean up. Serialised across the whole test binary
/// via [`crate::paths::TEST_HOME_LOCK`] so concurrent tests can't trample
/// each other.
///
/// `label` is folded into the tempdir name to make debugging leaked
/// dirs easier — pass a short, unique string per call site.
pub(crate) fn with_isolated_home<F: FnOnce()>(label: &str, body: F) {
    let _guard = crate::paths::TEST_HOME_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let tmp: PathBuf =
        std::env::temp_dir().join(format!("pillbox-{label}-{}", uuid::Uuid::now_v7().simple()));
    std::fs::create_dir_all(&tmp).unwrap();
    let prev_home = std::env::var_os("HOME");
    std::env::set_var("HOME", &tmp);

    body();

    match prev_home {
        Some(v) => std::env::set_var("HOME", v),
        None => std::env::remove_var("HOME"),
    }
    let _ = std::fs::remove_dir_all(&tmp);
}
