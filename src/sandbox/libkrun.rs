//! Local microVM backend (libkrun) — **experimental, feature-gated** (`libkrun`).
//!
//! The graduation of the `libkrun-boot` proof crate (steps 1–6: boot, vsock
//! control channel, frame attach, §0, egress + vault v2, CoW workspace) into a
//! real [`SandboxBackend`]. This first slice establishes the seam — the libkrun
//! FFI bindings + a runtime-selectable backend — with the default build and the
//! Docker path untouched. Boot → agent run → attach (the `Frame` protocol over
//! vsock) land in the following slices; [`LibkrunBackend::run`] errors clearly
//! until then.
//!
//! Build: `cargo build --features libkrun` (needs libkrun + libkrunfw), then
//! codesign the binary with the hypervisor entitlement (macOS/HVF). Select at
//! runtime with `PILLBOX_BACKEND=libkrun`.

use anyhow::Result;

use super::SandboxBackend;
use crate::agents::{AgentSpec, RunOpts};
use crate::errors::PillboxError;
use crate::pillbox::Pillbox;

/// libkrun C API bindings — the single home for the `unsafe extern "C"`
/// signatures (header: `/opt/homebrew/include/libkrun.h`; linked + rpath'd by
/// `build.rs` under the `libkrun` feature). Surface lands ahead of the boot/
/// attach wiring that consumes it.
#[allow(dead_code)]
pub(crate) mod ffi {
    use std::os::raw::{c_char, c_int};

    #[link(name = "krun")]
    extern "C" {
        pub fn krun_create_ctx() -> c_int;
        pub fn krun_set_vm_config(ctx_id: u32, num_vcpus: u8, ram_mib: u32) -> c_int;
        pub fn krun_set_root(ctx_id: u32, root_path: *const c_char) -> c_int;
        pub fn krun_set_workdir(ctx_id: u32, workdir: *const c_char) -> c_int;
        pub fn krun_add_vsock_port(ctx_id: u32, port: u32, c_filepath: *const c_char) -> c_int;
        pub fn krun_add_net_unixstream(
            ctx_id: u32,
            c_path: *const c_char,
            fd: c_int,
            c_mac: *const u8,
            features: u32,
            flags: u32,
        ) -> c_int;
        pub fn krun_add_virtiofs(ctx_id: u32, c_tag: *const c_char, c_path: *const c_char)
            -> c_int;
        pub fn krun_set_exec(
            ctx_id: u32,
            exec_path: *const c_char,
            argv: *const *const c_char,
            envp: *const *const c_char,
        ) -> c_int;
        pub fn krun_start_enter(ctx_id: u32) -> c_int;
    }

    /// Map a negative libkrun return (`-errno`) to an error.
    pub fn check(rc: c_int, what: &str) -> anyhow::Result<()> {
        if rc < 0 {
            anyhow::bail!("{what} failed: rc={rc} (-errno)");
        }
        Ok(())
    }
}

/// The local microVM backend. Selected for a local run when the `libkrun`
/// feature is built in and `PILLBOX_BACKEND=libkrun` is set.
pub(crate) struct LibkrunBackend;

impl SandboxBackend for LibkrunBackend {
    fn run(&self, spec: &AgentSpec, _opts: RunOpts, _resolved: &Pillbox) -> Result<()> {
        Err(PillboxError::runtime(
            "run",
            format!(
                "libkrun backend selected for `{}`, but boot/attach is not yet wired \
                 (step 7 landing in progress)",
                spec.id
            ),
        )
        .with_next("unset PILLBOX_BACKEND to use the default backend")
        .into())
    }
}
