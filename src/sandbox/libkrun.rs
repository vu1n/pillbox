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
//! **Subprocess VMM (the load-bearing shape).** `krun_start_enter` does *not*
//! return — the VMM takes over the calling process and `exit()`s with the
//! guest's code when the VM shuts down. A `SandboxBackend::run` that called it
//! in-process would terminate the whole pillbox CLI (no cleanup, no attach
//! supervision, no `Ok`). So the backend **re-execs itself** as a hidden
//! `__krun-vmm` child that *becomes* the VM, while the parent supervises it
//! (and, in later slices, connects to the control sockets the child sets up for
//! attach + §0). The child's process exit code IS the guest's exit code.
//!
//! Build + run (macOS/HVF):
//! ```text
//! cargo build --features libkrun
//! codesign --entitlements krun/entitlements.plist -f -s - target/debug/pillbox
//! PILLBOX_BACKEND=libkrun pillbox run --agent claude
//! ```
//! Re-codesign after every build (cargo invalidates the signature). Select at
//! runtime with `PILLBOX_BACKEND=libkrun`.

use std::ffi::CString;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};

use super::SandboxBackend;
use crate::agents::{AgentSpec, RunOpts};
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
///
/// Slice 2 (this): boot the runner rootfs as a microVM and smoke-run the agent
/// binary, proving boot from the real (codesigned) pillbox binary via the
/// subprocess VMM. Creds/workspace/env + attach over vsock + §0 layer on next.
pub(crate) struct LibkrunBackend;

impl SandboxBackend for LibkrunBackend {
    fn run(&self, spec: &AgentSpec, _opts: RunOpts, resolved: &Pillbox) -> Result<()> {
        let rootfs = materialize_rootfs(resolved)?;
        let exe = std::env::current_exe().context("locate the pillbox binary to re-exec as VMM")?;

        eprintln!(
            "pillbox: libkrun backend (experimental) — booting microVM, rootfs {}",
            rootfs.display()
        );
        // Smoke: prove the agent binary runs inside the booted runner rootfs.
        // The full run (creds + workspace + env + attach) is the next slice.
        let smoke = format!(
            "{} --version 2>&1 || true; echo PILLBOX-KRUN-SMOKE-OK; uname -sm",
            spec.id
        );
        let status = Command::new(&exe)
            .arg("__krun-vmm")
            .arg(&rootfs)
            .args(["/bin/sh", "-c", &smoke])
            .status()
            .context("spawn the libkrun VMM subprocess")?;

        // The VMM child `exit()`s with the guest's code, so the child's status IS
        // the guest's exit code.
        eprintln!("pillbox: libkrun microVM exited (guest status {:?})", status.code());
        Ok(())
    }
}

/// The VMM child (`pillbox __krun-vmm <rootfs> <exec> [args…]`). Configures a
/// libkrun context and enters it — `krun_start_enter` does not return; it
/// `exit()`s with the guest's exit code. Only returns on a pre-boot error.
pub(crate) fn vmm_child_main() -> ! {
    let argv: Vec<String> = std::env::args().collect();
    if argv.len() < 4 {
        eprintln!("krun-vmm: usage: pillbox __krun-vmm <rootfs> <exec> [args…]");
        std::process::exit(2);
    }
    let rootfs = cstr(&argv[2]);
    let workdir = cstr("/");
    let exec = cstr(&argv[3]);
    // krun_set_exec's argv is the args *after* argv[0]; keep the CStrings alive
    // for the duration of the call, then build a null-terminated pointer array.
    let arg_cstrs: Vec<CString> = argv[4..].iter().map(|s| cstr(s)).collect();
    let mut argv_ptrs: Vec<*const std::os::raw::c_char> =
        arg_cstrs.iter().map(|c| c.as_ptr()).collect();
    argv_ptrs.push(std::ptr::null());
    let envp: [*const std::os::raw::c_char; 1] = [std::ptr::null()];

    unsafe {
        let ctx = ffi::krun_create_ctx();
        if ctx < 0 {
            eprintln!("krun-vmm: krun_create_ctx rc={ctx}");
            std::process::exit(1);
        }
        let ctx = ctx as u32;
        let cfg = ffi::krun_set_vm_config(ctx, 2, 1024)
            .min(ffi::krun_set_root(ctx, rootfs.as_ptr()))
            .min(ffi::krun_set_workdir(ctx, workdir.as_ptr()))
            .min(ffi::krun_set_exec(ctx, exec.as_ptr(), argv_ptrs.as_ptr(), envp.as_ptr()));
        if cfg < 0 {
            eprintln!("krun-vmm: configuration failed (rc={cfg})");
            std::process::exit(1);
        }
        // Enters the microVM; exits the process with the guest's status.
        let rc = ffi::krun_start_enter(ctx);
        eprintln!("krun-vmm: start_enter returned {rc} (pre-boot config error)");
        std::process::exit(1);
    }
}

/// Materialize the runner OCI image into a cached on-disk directory usable as a
/// virtio-fs root (libkrun's `krun_set_root` takes a *directory*, not an image).
/// One-time per image via `docker export`; cached under `~/.pillbox/krun/rootfs/`.
fn materialize_rootfs(resolved: &Pillbox) -> Result<PathBuf> {
    let (image, _) = crate::docker::resolve_runner_image(resolved);
    let cache = krun_cache_dir()?.join("rootfs").join(sanitize(&image));
    let marker = cache.join(".materialized");
    if marker.exists() {
        return Ok(cache);
    }
    let _ = std::fs::remove_dir_all(&cache);
    std::fs::create_dir_all(&cache).with_context(|| format!("create {}", cache.display()))?;
    eprintln!("pillbox: materializing runner rootfs from {image} (one-time)…");

    let create = Command::new("docker")
        .args(["create", &image])
        .output()
        .context("docker create (is the runner image present + docker running?)")?;
    if !create.status.success() {
        bail!(
            "docker create {image} failed: {}",
            String::from_utf8_lossy(&create.stderr).trim()
        );
    }
    let cid = String::from_utf8_lossy(&create.stdout).trim().to_string();

    // Stream the container filesystem straight into the cache dir.
    let export = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "docker export {cid} | tar -C {} -xf -",
            cache.display()
        ))
        .status();
    let _ = Command::new("docker").args(["rm", "-f", &cid]).status();
    match export {
        Ok(s) if s.success() => {}
        Ok(s) => bail!("rootfs export failed (status {:?})", s.code()),
        Err(e) => bail!("rootfs export failed: {e}"),
    }
    std::fs::write(&marker, image.as_bytes()).context("write rootfs cache marker")?;
    Ok(cache)
}

fn krun_cache_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME unset")?;
    Ok(PathBuf::from(home).join(".pillbox").join("krun"))
}

/// Filesystem-safe cache key for an image ref (`a/b:c` → `a_b_c`).
fn sanitize(image: &str) -> String {
    image
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn cstr(s: &str) -> CString {
    CString::new(s).expect("rootfs/exec path contains an interior NUL")
}
