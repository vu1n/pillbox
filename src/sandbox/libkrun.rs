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
use std::os::raw::c_char;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use super::SandboxBackend;
use crate::attach::pump;
use crate::agents::{
    resolve_run_env, resolve_with_entries, workspace_mount_name, AgentSpec, Integration, RunOpts,
    GUEST_HOME, GUEST_WORKSPACE,
};
use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::workspace::WorkspaceBackend;

/// libkrun C API bindings — the single home for the `unsafe extern "C"`
/// signatures (header: `/opt/homebrew/include/libkrun.h`; linked + rpath'd by
/// `build.rs` under the `libkrun` feature). `krun_add_vsock_port` (attach, L4)
/// and `krun_add_net_unixstream` (egress, L5) are declared ahead of the slices
/// that consume them — the same contract-first stance as `contract.rs`.
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
}

// macOS APFS copy-on-write clone (recursive for directories), from libSystem.
extern "C" {
    fn clonefile(src: *const c_char, dst: *const c_char, flags: u32) -> std::os::raw::c_int;
}

/// The microVM spec the parent hands the VMM child (via a temp file — paths,
/// not secrets). The guest env (incl. any `--with` secrets) travels separately
/// as the child process's *environment*, which the child forwards to the guest
/// — never on argv or in this file.
#[derive(Serialize, Deserialize)]
struct VmSpec {
    rootfs: String,
    vcpus: u8,
    ram_mib: u32,
    /// virtio-fs shares: host dir → guest tag (mounted by the exec script).
    shares: Vec<Share>,
    /// Guest entrypoint argv (`exec[0]` is the path).
    exec: Vec<String>,
    /// Attach channel: the guest pty-host listens on this vsock port, the parent
    /// connects via the host unix socket (`krun_add_vsock_port2`, listen=true).
    vsock: Option<VsockAttach>,
}

#[derive(Serialize, Deserialize)]
struct Share {
    tag: String,
    host_path: String,
}

#[derive(Serialize, Deserialize)]
struct VsockAttach {
    port: u32,
    host_sock: String,
}

/// The local microVM backend. Selected for a local run when the `libkrun`
/// feature is built in and `PILLBOX_BACKEND=libkrun` is set.
///
/// Slice 3 (this): mirror `local_docker::run`'s creds + workspace + env pipeline
/// — share the agent's auth home, CoW-clone + secret-scrub the workspace, compose
/// the run env — and launch the agent in the microVM (libkrun's native console
/// for I/O). Attach over vsock (the `Frame` protocol) + §0 + vault-v2 are next.
pub(crate) struct LibkrunBackend;

impl SandboxBackend for LibkrunBackend {
    fn run(&self, spec: &AgentSpec, opts: RunOpts, resolved: &Pillbox) -> Result<()> {
        // Deferred to later slices — reject loudly rather than silently misbehave.
        if spec.integration == Integration::Server {
            return Err(unsupported(spec, "server-mode agents (opencode)"));
        }
        if opts.vault {
            return Err(unsupported(spec, "--vault (egress + vault v2 is a later slice)"));
        }
        if opts.detach {
            return Err(unsupported(spec, "--detach (session lifecycle is a later slice)"));
        }

        let rootfs = materialize_rootfs(resolved)?;

        // ── creds: the agent's auth home, shared live at GUEST_HOME ──
        let home = spec.home_dir(resolved)?;
        if !home.join(spec.cred_sentinel).exists() {
            return Err(PillboxError::runtime(
                "run",
                format!("no stored credentials for `{}`", spec.id),
            )
            .with_next(format!("pillbox auth login --agent {}", spec.id))
            .into());
        }

        // ── workspace: CoW clone + secret-scrub, shared at GUEST_WORKSPACE/<name> ──
        let workspace_host = match &opts.workspace {
            Some(p) => p.clone(),
            None => std::env::current_dir().context("resolve current working directory")?,
        };
        if let Some(name) = opts.from_bookmark.as_deref() {
            let handle = crate::bookmarks::resolve_existing(resolved, name)?;
            resolved.workspace()?.pull(&workspace_host, Some(&handle))?;
        }
        let workspace_name = workspace_mount_name(&workspace_host, opts.name.as_deref())?;
        let guest_workspace = format!("{GUEST_WORKSPACE}/{workspace_name}");
        let clone = cow_clone_and_scrub(&workspace_host)?;

        // ── env: the canonical composer (bundles → env-file → --with), no vault ──
        let withs = resolve_with_entries(resolved, &opts.withs)?;
        let composed = resolve_run_env(resolved, &opts, &withs, None)?;
        let mut guest_env: Vec<(String, String)> = vec![
            ("HOME".into(), GUEST_HOME.into()),
            ("TERM".into(), "xterm-256color".into()),
            (
                "PATH".into(),
                format!("/usr/local/bin:/usr/bin:/bin:{GUEST_HOME}/.local/bin"),
            ),
        ];
        guest_env.extend(composed);

        // Pre-accept the agent's workspace-trust dialog on the live auth home
        // before boot (claude); operates on host paths, like the docker path.
        spec.prepare_workspace_or_warn(&home, &guest_workspace);

        // The guest entrypoint: mount the shares, cd into the workspace, exec the
        // agent (run_argv + sandbox defaults + user `-- args`).
        let agent_argv: Vec<String> = spec
            .run_argv
            .iter()
            .map(|s| s.to_string())
            .chain(spec.sandbox_args.iter().map(|s| s.to_string()))
            .chain(opts.args.iter().cloned())
            .collect();
        // Quote every interpolated path — the workspace name can legitimately
        // contain a space (and must never be shell-evaluated). The agent runs
        // under the in-guest pty-host serving the Frame protocol over vsock (the
        // same attach transport the docker/ssh backends use, just a different
        // pipe), which the parent attaches to and pumps below.
        let home_q = shell_quote(GUEST_HOME);
        let gw_q = shell_quote(&guest_workspace);
        let agent = agent_argv
            .iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" ");
        let script = format!(
            "set -e; mkdir -p {home_q}; mount -t virtiofs creds {home_q}; \
             mkdir -p {gw_q}; mount -t virtiofs workspace {gw_q}; cd {gw_q}; \
             exec pillbox pty-host --vsock-port {ATTACH_PORT} -- {agent}",
        );

        let attach_sock = krun_cache_dir()?
            .join(format!("attach-{}.sock", uuid::Uuid::now_v7().simple()));
        let _ = std::fs::remove_file(&attach_sock);

        let vmspec = VmSpec {
            rootfs: rootfs.to_string_lossy().into_owned(),
            vcpus: 2,
            ram_mib: 2048,
            shares: vec![
                Share { tag: "creds".into(), host_path: home.to_string_lossy().into_owned() },
                Share { tag: "workspace".into(), host_path: clone.to_string_lossy().into_owned() },
            ],
            exec: vec!["/bin/sh".into(), "-c".into(), script],
            vsock: Some(VsockAttach {
                port: ATTACH_PORT,
                host_sock: attach_sock.to_string_lossy().into_owned(),
            }),
        };
        let spec_file = tempfile::Builder::new()
            .prefix("pillbox-krun-spec-")
            .suffix(".json")
            .tempfile()
            .context("create VMM spec tempfile")?;
        serde_json::to_writer(&spec_file, &vmspec).context("write VMM spec")?;

        eprintln!(
            "pillbox: libkrun backend (experimental) — launching `{}` in a microVM",
            spec.id
        );
        let exe = std::env::current_exe().context("locate the pillbox binary to re-exec as VMM")?;
        // Spawn (don't block): the child boots the VM + the guest pty-host listens
        // on the vsock attach port; the parent attaches over the bridged socket and
        // pumps the terminal, then reaps the VM. Secrets travel as the child's env
        // (env_clear so only the composed guest env reaches the VM).
        // Bind the attach listener BEFORE booting: libkrun (in the child) dials it
        // when the guest pty-host connects out, so it must already exist.
        let listener = UnixListener::bind(&attach_sock)
            .with_context(|| format!("bind attach socket {}", attach_sock.display()))?;
        listener.set_nonblocking(true).ok();

        let mut child = Command::new(&exe)
            .arg("__krun-vmm")
            .arg(spec_file.path())
            .env_clear()
            .envs(guest_env)
            .spawn()
            .context("spawn the libkrun VMM subprocess")?;

        // Poll the listener until the guest pty-host dials in (across VM boot),
        // then run the shared terminal pump. Reaps the VM on return.
        let stream = accept_attach(&listener, &mut child)?;
        let write_half = stream.try_clone().context("clone attach stream")?;
        let outcome = pump::attach_terminal(stream, write_half, false)?;
        let _ = child.wait();
        let _ = std::fs::remove_file(&attach_sock); // don't litter ~/.pillbox/krun
        match outcome {
            pump::Outcome::Exited(code) if code != 0 => std::process::exit(code),
            _ => Ok(()),
        }
    }
}

/// vsock port the guest pty-host dials for the attach channel.
const ATTACH_PORT: u32 = 1024;

/// Accept the guest pty-host's attach connection on the pre-bound listener,
/// waiting across VM boot. Fails fast if the VMM child dies first.
fn accept_attach(listener: &UnixListener, child: &mut std::process::Child) -> Result<UnixStream> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match listener.accept() {
            Ok((s, _)) => {
                s.set_nonblocking(false).ok();
                return Ok(s);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e).context("accept attach connection"),
        }
        if let Some(status) = child.try_wait().context("poll VMM child")? {
            bail!("libkrun VMM exited before attach was ready (status {:?})", status.code());
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for the guest pty-host to connect");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The VMM child (`pillbox __krun-vmm <spec.json>`). Reads the [`VmSpec`],
/// configures a libkrun context (root + virtio-fs shares + exec), and enters it.
///
/// **This process's environment IS the guest environment**: the parent spawns it
/// with `env_clear().envs(guest_env)`, so `std::env::vars()` here is exactly the
/// composed guest env (config + any secrets) and is forwarded to `krun_set_exec`.
/// `krun_start_enter` does not return — it `exit()`s with the guest's code; only
/// returns on a pre-boot config error.
pub(crate) fn vmm_child_main() -> ! {
    let spec_path = match std::env::args().nth(2) {
        Some(p) => p,
        None => {
            eprintln!("krun-vmm: usage: pillbox __krun-vmm <spec.json>");
            std::process::exit(2);
        }
    };
    let raw = match std::fs::read_to_string(&spec_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("krun-vmm: read spec {spec_path}: {e}");
            std::process::exit(2);
        }
    };
    let spec: VmSpec = match serde_json::from_str(&raw) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("krun-vmm: parse spec {spec_path}: {e}");
            std::process::exit(2);
        }
    };
    if spec.exec.is_empty() {
        eprintln!("krun-vmm: spec.exec is empty");
        std::process::exit(2);
    }

    // Keep every CString alive until after start_enter.
    let rootfs = cstr(&spec.rootfs);
    let workdir = cstr("/");
    let exec = cstr(&spec.exec[0]);
    let arg_cstrs: Vec<CString> = spec.exec[1..].iter().map(|s| cstr(s)).collect();
    let mut argv_ptrs: Vec<*const c_char> = arg_cstrs.iter().map(|c| c.as_ptr()).collect();
    argv_ptrs.push(std::ptr::null());
    // Forward this process's env (= the guest env; see the doc above) to the VM.
    let env_cstrs: Vec<CString> = std::env::vars()
        .map(|(k, v)| cstr(&format!("{k}={v}")))
        .collect();
    let mut envp: Vec<*const c_char> = env_cstrs.iter().map(|c| c.as_ptr()).collect();
    envp.push(std::ptr::null());
    let shares: Vec<(CString, CString)> = spec
        .shares
        .iter()
        .map(|s| (cstr(&s.tag), cstr(&s.host_path)))
        .collect();
    let vsock = spec.vsock.as_ref().map(|v| (v.port, cstr(&v.host_sock)));

    unsafe {
        let ctx = ffi::krun_create_ctx();
        if ctx < 0 {
            eprintln!("krun-vmm: krun_create_ctx rc={ctx}");
            std::process::exit(1);
        }
        let ctx = ctx as u32;
        let mut rc = ffi::krun_set_vm_config(ctx, spec.vcpus, spec.ram_mib);
        rc = rc.min(ffi::krun_set_root(ctx, rootfs.as_ptr()));
        rc = rc.min(ffi::krun_set_workdir(ctx, workdir.as_ptr()));
        for (tag, host) in &shares {
            rc = rc.min(ffi::krun_add_virtiofs(ctx, tag.as_ptr(), host.as_ptr()));
        }
        if let Some((port, host_sock)) = &vsock {
            // Default direction: the guest pty-host dials `port`, libkrun bridges
            // to the parent's listener at `host_sock`. (Guest-connects-out is the
            // proven direction; the parent's accept() waits for us — no race.)
            rc = rc.min(ffi::krun_add_vsock_port(ctx, *port, host_sock.as_ptr()));
        }
        rc = rc.min(ffi::krun_set_exec(ctx, exec.as_ptr(), argv_ptrs.as_ptr(), envp.as_ptr()));
        if rc < 0 {
            eprintln!("krun-vmm: configuration failed (rc={rc})");
            std::process::exit(1);
        }
        let rc = ffi::krun_start_enter(ctx);
        eprintln!("krun-vmm: start_enter returned {rc} (pre-boot config error)");
        std::process::exit(1);
    }
}

/// CoW-clone the workspace (so the base is the immutable fork point) and scrub
/// secret files from the clone using the canonical `workspace::ingest` denylist.
/// Returns the clone dir to share.
fn cow_clone_and_scrub(src: &Path) -> Result<PathBuf> {
    let clone = krun_cache_dir()?
        .join("ws")
        .join(uuid::Uuid::now_v7().simple().to_string());
    if let Some(parent) = clone.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let src_c = cstr(&src.to_string_lossy());
    let clone_c = cstr(&clone.to_string_lossy());
    let rc = unsafe { clonefile(src_c.as_ptr(), clone_c.as_ptr(), 0) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        let _ = std::fs::remove_dir_all(&clone); // don't leave a half clone behind
        bail!("clonefile {} → {} failed: {err} (APFS only)", src.display(), clone.display());
    }
    // Reuse the canonical walker + denylist; delete what it flags as secret.
    let plan = crate::workspace::ingest::plan_ingest(&clone)?;
    for rel in &plan.excluded_secrets {
        let p = clone.join(rel);
        let _ = if p.is_dir() {
            std::fs::remove_dir_all(&p)
        } else {
            std::fs::remove_file(&p)
        };
    }
    Ok(clone)
}

/// Single-quote a shell argument (`'` → `'\''`).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn unsupported(spec: &AgentSpec, what: &str) -> anyhow::Error {
    PillboxError::usage(
        "run",
        format!("libkrun backend: {what} not supported yet (running `{}`)", spec.id),
    )
    .with_next("unset PILLBOX_BACKEND to use the default backend")
    .into()
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
        // Clear the half-populated cache so the next run retries from scratch
        // (the marker is written only on success, but leave nothing partial).
        Ok(s) => {
            let _ = std::fs::remove_dir_all(&cache);
            bail!("rootfs export failed (status {:?})", s.code());
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&cache);
            bail!("rootfs export failed: {e}");
        }
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
