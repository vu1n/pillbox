//! CLI subcommand handlers. main.rs owns the clap surface (Cli +
//! all the *Action enums) and the top-level `run(cli)` dispatcher;
//! the actual per-subcommand handler bodies live here, one file per
//! domain. Keeps main.rs at a size where the CLI shape is scannable.
//!
//! Each submodule exposes a `dispatch(resolved, action)` entry that
//! main.rs's `Command::*` match arms call into.

pub(crate) mod auth;
pub(crate) mod bookmark;
// Contract-ahead-of-impl (GHOST-002): the dispatch types + JSON envelope are
// declared for GHOST-003 (the fork/score/select loop) and GHOST-004 (e2e) to
// program against; the handler is a stub. Same stance as the `ffi` module.
// Remove the allow when GHOST-003 wires the consumers.
#[allow(dead_code)]
pub(crate) mod dispatch;
pub(crate) mod env;
pub(crate) mod sandbox;
pub(crate) mod secret;
pub(crate) mod session;
pub(crate) mod sidecar;
pub(crate) mod vault;
pub(crate) mod workspace;
