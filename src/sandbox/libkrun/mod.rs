//! Local microVM backend (libkrun) — **feature-gated** (`libkrun`); Linux/KVM + macOS/HVF.
//!
//! The graduation of the `libkrun-boot` proof crate into a real [`SandboxBackend`]
//! (feature-gated; the default build + Docker path stay untouched).
//! [`LibkrunBackend::run`] boots a microVM and runs the agent end-to-end: a
//! CoW-cloned + secret-scrubbed workspace and the agent's auth home; the agent
//! under an in-guest `pillbox pty-host` serving the `Frame` protocol over vsock
//! (the parent attaches + pumps the terminal); a userspace egress stack in the
//! VMM child ([`egress`]) — virtio-net + smoltcp with a default-deny DNS fence;
//! an owned TLS MITM ([`vault`]) that terminates the allowlisted providers,
//! **swaps a stubbed credential for the real one** (the env-fork — the real never
//! enters the VM; see the guard in `run`), and forwards to the real upstream; and
//! §0 transcript events tailed into the `SessionLog`. Design + the deferred
//! consolidation items: `docs/libkrun-sandbox.md` (L1–L5).
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
//! Build + run (macOS/HVF; on Linux/KVM drop the codesign step — KVM needs no entitlement):
//! ```text
//! cargo build --features libkrun
//! codesign --entitlements krun/entitlements.plist -f -s - target/debug/pillbox
//! PILLBOX_BACKEND=libkrun pillbox run --agent claude
//! ```
//! Re-codesign after every build (cargo invalidates the signature). Select at
//! runtime with `PILLBOX_BACKEND=libkrun`.

use std::ffi::CString;
use std::os::raw::{c_char, c_int};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

mod egress;
mod http;
mod local_forward;
mod mitm;
mod session;
mod vault;

// The launch/attach/detach/reattach/teardown choreography lives in `session`;
// `commands::session` reaches these accessors through the backend module.
pub(crate) use session::{
    kill_session, opencode_http, reattach, score_in_sandbox, server_events_file, workspace_path,
};

use crate::agents::AgentSpec;
use crate::errors::PillboxError;
use crate::pillbox::Pillbox;

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
        /// `listen=false`: guest dials `port`, libkrun bridges to the host unix
        /// socket (host listens) — the foreground attach. `listen=true`: the guest
        /// listens on `port`, libkrun binds the host unix socket (a host client
        /// dials it) — the **detach** direction, so the socket persists for reattach
        /// after the parent returns.
        pub fn krun_add_vsock_port2(
            ctx_id: u32,
            port: u32,
            c_filepath: *const c_char,
            listen: bool,
        ) -> c_int;
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
    /// Egress: when set, the child attaches a virtio-net device and runs the
    /// userspace stack (DNS fence over `allowlist`). Policy only — non-secret.
    egress: Option<EgressSpec>,
}

#[derive(Serialize, Deserialize)]
struct EgressSpec {
    /// DNS-fence allowlist: only these hosts resolve; everything else NXDOMAINs.
    allowlist: Vec<String>,
    /// Optional host-side diagnostics file (the guest console eats the child's
    /// stderr). Set from `PILLBOX_KRUN_EGRESS_LOG`; `None` falls back to stderr.
    log_path: Option<String>,
    /// Host path of the vault CA dir. When set, the child loads the CA (key stays
    /// host-side) to mint per-SNI MITM leaves; `None` = DNS-fence only.
    ca_dir: Option<String>,
    /// Opt-in local-model forward: relay `gateway:PORT` → host `127.0.0.1:PORT`
    /// (e.g. ollama on 11434), so a guest agent reaches a model the host runs.
    /// Deliberately punches the default-deny fence; set only when a local worker
    /// is requested. `None` = no forward. See [`super::local_forward`].
    #[serde(default)]
    local_forward_port: Option<u16>,
}

#[derive(Serialize, Deserialize)]
struct Share {
    tag: String,
    host_path: String,
}

/// A stub→real credential substitution the MITM applies to the guest→upstream
/// stream. Passed parent→child on **stdin** (out-of-band): the real value never
/// touches the guest env, argv, or the VmSpec file.
#[derive(Serialize, Deserialize)]
struct SwapPair {
    stub: String,
    real: String,
    /// Hosts this credential is bound to — the MITM applies the swap ONLY on a
    /// connection whose pinned SNI is in this set, so a stub leaked into the guest
    /// can't be replayed to a *different* allowlisted host to extract the real
    /// (destination-bound release). OAuth → the provider's intercept set; a
    /// vaulted `--with` → its declared `vault.host`.
    #[serde(default)]
    hosts: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct VsockAttach {
    port: u32,
    host_sock: String,
    /// `false`: guest dials the host (foreground — parent binds + accepts).
    /// `true`: guest listens, libkrun binds `host_sock` (detach — it persists for
    /// reattach after the parent returns). Selects `krun_add_vsock_port{,2}`.
    #[serde(default)]
    listen: bool,
}

/// The local microVM backend. Selected for a local run when the `libkrun`
/// feature is built in and `PILLBOX_BACKEND=libkrun` is set.
///
/// Mirrors `docker::run`'s creds + workspace + env pipeline (share the
/// agent's auth home, CoW-clone + secret-scrub the workspace, compose the run
/// env), launches the agent under an in-guest pty-host serving the `Frame`
/// protocol over vsock (L4), and attaches a userspace egress stack with a DNS
/// fence (L5a). The vault-v2 MITM (terminate + cred swap + forward) and §0 are
/// the remaining slices — L5b consumes the DNS-pin this egress stack populates.
pub(crate) struct LibkrunBackend;

/// Where the guest writes the vault CA cert (system trust dir → `update-ca-certificates`;
/// also `NODE_EXTRA_CA_CERTS` for Node agents). The cert is public — the key never leaves the host.
const GUEST_CA_PATH: &str = "/usr/local/share/ca-certificates/pillbox-vault.crt";

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
    let vsock = spec
        .vsock
        .as_ref()
        .map(|v| (v.port, cstr(&v.host_sock), v.listen));
    // Egress: a passt socketpair — one end to libkrun's virtio-net, the other to
    // our userspace stack (which the child runs in a thread beside the VM).
    struct NetAttach {
        libkrun_fd: c_int,
        host_fd: c_int,
        allowlist: Vec<String>,
        ca_dir: Option<String>,
        log_path: Option<String>,
        local_forward_port: Option<u16>,
    }
    let net: Option<NetAttach> = spec.egress.as_ref().map(|e| {
        let mut fds = [0 as c_int; 2];
        if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) } != 0 {
            eprintln!(
                "krun-vmm: egress socketpair: {}",
                std::io::Error::last_os_error()
            );
            std::process::exit(1);
        }
        NetAttach {
            libkrun_fd: fds[0],
            host_fd: fds[1],
            allowlist: e.allowlist.clone(),
            ca_dir: e.ca_dir.clone(),
            log_path: e.log_path.clone(),
            local_forward_port: e.local_forward_port,
        }
    });
    // Read the stub→real credential pairs the parent pipes on stdin (the env-fork
    // channel — reals arrive out-of-band, never in the guest env/argv/VmSpec).
    let swap_pairs = if net.is_some() {
        read_swap_pairs()
    } else {
        Vec::new()
    };

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
        if let Some((port, host_sock, listen)) = &vsock {
            // Foreground (listen=false): the guest pty-host dials `port`, libkrun
            // bridges to the parent's pre-bound listener at `host_sock` — no race
            // (the parent's accept waits for us). Detach (listen=true): the guest
            // listens, libkrun binds `host_sock` so it persists for reattach after
            // the parent returns.
            rc = rc.min(if *listen {
                ffi::krun_add_vsock_port2(ctx, *port, host_sock.as_ptr(), true)
            } else {
                ffi::krun_add_vsock_port(ctx, *port, host_sock.as_ptr())
            });
        }
        if let Some(n) = net.as_ref() {
            rc = rc.min(ffi::krun_add_net_unixstream(
                ctx,
                std::ptr::null(),
                n.libkrun_fd,
                egress::GUEST_MAC.as_ptr(),
                0,
                0,
            ));
        }
        rc = rc.min(ffi::krun_set_exec(
            ctx,
            exec.as_ptr(),
            argv_ptrs.as_ptr(),
            envp.as_ptr(),
        ));
        if rc < 0 {
            eprintln!("krun-vmm: configuration failed (rc={rc})");
            std::process::exit(1);
        }
        // Run the userspace egress stack on our end of the socketpair before the
        // VM boots, so it's servicing frames the moment the guest's NIC comes up.
        // The thread dies when start_enter exit()s this process on VM shutdown.
        if let Some(n) = net {
            std::thread::spawn(move || {
                egress::run(
                    n.host_fd,
                    n.allowlist,
                    n.ca_dir,
                    swap_pairs,
                    n.log_path,
                    n.local_forward_port,
                )
            });
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
        bail!(
            "clonefile {} → {} failed: {err} (APFS only)",
            src.display(),
            clone.display()
        );
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

/// Read the stub→real pairs the parent piped on stdin (JSON `[{stub,real}]`). The
/// env-fork channel: reals arrive here, never via the guest env/argv/VmSpec.
fn read_swap_pairs() -> Vec<vault::CredSwap> {
    use std::io::Read;
    let mut buf = String::new();
    if std::io::stdin().read_to_string(&mut buf).is_err() || buf.trim().is_empty() {
        return Vec::new();
    }
    serde_json::from_str::<Vec<SwapPair>>(&buf)
        .unwrap_or_default()
        .into_iter()
        .map(|p| vault::CredSwap {
            stub: p.stub.into_bytes(),
            real: p.real.into_bytes(),
            hosts: p.hosts,
        })
        .collect()
}

/// CoW-clone the agent's auth home and replace its OAuth tokens with stubs, so the
/// guest mounts *stubbed* creds — the real tokens never enter the VM; the MITM
/// swaps stub→real on the wire. Returns the stubbed-creds dir to mount + the
/// (stub, real) pairs. Anthropic-shaped (`claudeAiOauth` in the credentials file);
/// other agents get the home cloned as-is + no pairs (transparent relay).
fn cow_clone_home(home: &Path) -> Result<PathBuf> {
    let clone = krun_cache_dir()?
        .join("creds")
        .join(uuid::Uuid::now_v7().simple().to_string());
    if let Some(parent) = clone.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let src_c = cstr(&home.to_string_lossy());
    let clone_c = cstr(&clone.to_string_lossy());
    if unsafe { clonefile(src_c.as_ptr(), clone_c.as_ptr(), 0) } != 0 {
        let err = std::io::Error::last_os_error();
        let _ = std::fs::remove_dir_all(&clone);
        bail!(
            "clonefile creds {} → {} failed: {err} (APFS only)",
            home.display(),
            clone.display()
        );
    }
    Ok(clone)
}

/// CoW-clone the auth home and replace its OAuth tokens with stubs (the
/// env-fork): the guest mounts the stubbed clone, the reals reach the MITM
/// out-of-band. Returns the clone + the stub→real swap pairs. Server-mode
/// agents (opencode, non-vault) skip this and mount [`cow_clone_home`] as-is —
/// their real key must reach the provider, so there's nothing to swap.
fn stub_oauth_creds(
    home: &Path,
    sentinel: &str,
    hosts: &[String],
) -> Result<(PathBuf, Vec<SwapPair>)> {
    let clone = cow_clone_home(home)?;
    let mut pairs = Vec::new();
    let creds_file = clone.join(sentinel);
    if let Ok(text) = std::fs::read_to_string(&creds_file) {
        if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(oauth) = json
                .get_mut("claudeAiOauth")
                .and_then(|v| v.as_object_mut())
            {
                for field in ["accessToken", "refreshToken"] {
                    let real = oauth
                        .get(field)
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    if let Some(real) = real.filter(|s| !s.is_empty()) {
                        let stub = mint_oauth_stub(&real);
                        oauth.insert(field.to_string(), serde_json::Value::String(stub.clone()));
                        pairs.push(SwapPair {
                            stub,
                            real,
                            hosts: hosts.to_vec(),
                        });
                    }
                }
                let body = serde_json::to_string(&json).context("reserialize stubbed creds")?;
                // The clone's file is already 0600 (clonefile preserves perms) and
                // `write` truncates in place without changing them.
                std::fs::write(&creds_file, body).context("write stubbed creds")?;
            }
        }
    }
    Ok((clone, pairs))
}

/// Mint a stub for an **OAuth token** (the `claudeAiOauth` `access`/`refreshToken`,
/// shaped `sk-ant-{oat01,ort01}-<body>` — a fixed 3-hyphen-segment type prefix).
/// Derives the stub prefix from `real` (the two token types differ, so a single
/// curated prefix can't serve both), keeping ONLY the first 3 segments — for the
/// OAuth shape that's exactly the public type marker, never the body.
///
/// **Only for OAuth-shaped tokens.** For an arbitrary `--with` secret use the
/// curated `crate::vault::providers::mint_stub(prefix, …)` instead: a key whose
/// public prefix is <3 segments (OpenAI `sk-proj-<body>`) would leak a body chunk
/// through this derivation. The ≥4-segment guard only protects short/odd tokens
/// (synthetic fallback), NOT 2-segment-prefix keys — hence the OAuth-only contract.
fn mint_oauth_stub(real: &str) -> String {
    let prefix = if real.split('-').count() >= 4 {
        real.splitn(4, '-').take(3).collect::<Vec<_>>().join("-")
    } else {
        "pllbx".to_string()
    };
    format!("{prefix}-pllbxstub{}", uuid::Uuid::now_v7().simple())
}

/// Single-quote a shell argument (`'` → `'\''`).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn unsupported(spec: &AgentSpec, what: &str) -> anyhow::Error {
    PillboxError::usage(
        "run",
        format!(
            "libkrun backend: {what} not supported yet (running `{}`)",
            spec.id
        ),
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

#[cfg(test)]
mod tests {
    use super::mint_oauth_stub;

    #[test]
    fn mint_stub_keeps_type_prefix_not_the_secret_body() {
        // Anthropic OAuth tokens are sk-ant-oat01-<base64url> and the body can
        // contain hyphens — the stub must keep only the type prefix, never the body.
        let real = "sk-ant-oat01-3WY-itf8QpVP38ipXjip-SECRETBODYxyz";
        let stub = mint_oauth_stub(real);
        assert!(
            stub.starts_with("sk-ant-oat01-pllbxstub"),
            "stub leaked shape: {stub}"
        );
        assert!(!stub.contains("3WY"), "stub leaked body: {stub}");
        assert!(!stub.contains("SECRETBODYxyz"), "stub leaked body: {stub}");
        assert_ne!(stub, real);
        // Distinct each call (uuid suffix).
        assert_ne!(mint_oauth_stub(real), stub);
    }

    #[test]
    fn mint_stub_does_not_leak_short_or_odd_tokens() {
        // A token with <4 hyphen segments has no clear type/body split, so the
        // prefix must be synthetic — never any of the real bytes.
        for real in ["sk-secret", "justonesecretword", "sk-ant-secretbody"] {
            let stub = mint_oauth_stub(real);
            assert!(
                stub.starts_with("pllbx-pllbxstub"),
                "short token leaked shape: {stub}"
            );
            assert!(!stub.contains("secret"), "short token leaked body: {stub}");
        }
    }
}
