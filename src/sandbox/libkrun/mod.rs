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

mod boot;
mod egress;
mod host;
mod http;
mod local_forward;
mod mitm;
mod session;
mod vault;

// The control verbs (attach/teardown/§0 accessors) are driven through the
// `LiveSession` plane (`LibkrunLiveSession`); only the one-shot grader, which has
// no session record to dispatch on, is still reached directly.
pub(crate) use session::{score_in_sandbox, LibkrunLiveSession};

// Host-capability probes shared by `doctor` and the launch preflight.
pub(crate) use host::{
    disk_headroom, runtime_deps_present, virtualization_available, MIN_HEADROOM_BYTES,
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
/// **This process's environment is forwarded to `krun_set_exec`** — and libkrun
/// serializes exec env + argv into the kernel cmdline, which accepts printable
/// ASCII only. So the parent spawns this child with `env_clear()` + the static
/// base env only; the composed guest env (and the agent argv's prompt) travel in
/// the boot script instead (see `boot::boot_channel`). `krun_start_enter`
/// does not return — it `exit()`s with the guest's code; only returns on a
/// pre-boot config error.
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
    spec: &AgentSpec,
    hosts: &[String],
) -> Result<(PathBuf, Vec<SwapPair>)> {
    let clone = cow_clone_home(home)?;
    let mut pairs = Vec::new();
    let creds_file = clone.join(spec.cred_sentinel);
    if let Ok(text) = std::fs::read_to_string(&creds_file) {
        if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&text) {
            // Each provider lays its OAuth tokens out differently, so dispatch on
            // the agent's owning provider (the same key `oauth_swap_hosts` uses) to
            // stub the right fields. An unhandled shape stubs nothing → the launch
            // guard ([`env_fork_left_real_unstubbed`]) refuses to leak it.
            let stubbed = match spec.auth_id {
                "claude" => stub_claude_oauth(&mut json, hosts, &mut pairs),
                "codex" => stub_codex_oauth(&mut json, hosts, &mut pairs),
                _ => false,
            };
            if stubbed {
                let body = serde_json::to_string(&json).context("reserialize stubbed creds")?;
                // The clone's file is already 0600 (clonefile preserves perms) and
                // `write` truncates in place without changing them.
                std::fs::write(&creds_file, body).context("write stubbed creds")?;
            }
        }
    }
    Ok((clone, pairs))
}

/// Stub claude's `claudeAiOauth.{accessToken,refreshToken}` in place, pushing one
/// host-bound swap pair per token. Returns whether anything was stubbed.
fn stub_claude_oauth(
    json: &mut serde_json::Value,
    hosts: &[String],
    pairs: &mut Vec<SwapPair>,
) -> bool {
    let Some(oauth) = json
        .get_mut("claudeAiOauth")
        .and_then(|v| v.as_object_mut())
    else {
        return false;
    };
    let mut stubbed = false;
    for field in ["accessToken", "refreshToken"] {
        let real = oauth
            .get(field)
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .filter(|s| !s.is_empty());
        if let Some(real) = real {
            let stub = mint_oauth_stub(&real);
            oauth.insert(field.to_string(), serde_json::Value::String(stub.clone()));
            pairs.push(SwapPair {
                stub,
                real,
                hosts: hosts.to_vec(),
            });
            stubbed = true;
        }
    }
    // Broker move: post-date the stub's expiry so the guest's Claude Code trusts its
    // local expiry and never refreshes itself — the MITM swaps the live access token
    // on the wire, and the host-side `pre_refresh` (in `prepare_launch`) keeps the
    // real token fresh. Only stamp when we actually stubbed; an unhandled/empty file
    // is left untouched for the launch guard to catch.
    if stubbed {
        oauth.insert(
            "expiresAt".to_string(),
            serde_json::Value::Number(serde_json::Number::from(
                crate::vault::STUB_FAR_FUTURE_EXPIRES_AT_MS,
            )),
        );
    }
    stubbed
}

/// Stub codex's ChatGPT-mode `tokens.{access_token,refresh_token}` in place. The
/// MITM swaps each stub→real as raw bytes on the request leg (content-agnostic),
/// so a curated-prefix stub — reusing the host-side codex vault provider's
/// prefixes for consistency — is all that's needed, NEVER the claude
/// [`mint_oauth_stub`] derivation, whose `-`-splitting would leak base64url JWT
/// body chunks from the access token. ApiKey-mode auth.json (no `tokens` block)
/// stubs nothing.
fn stub_codex_oauth(
    json: &mut serde_json::Value,
    hosts: &[String],
    pairs: &mut Vec<SwapPair>,
) -> bool {
    use crate::vault::providers::codex::{STUB_ACCESS_PREFIX, STUB_REFRESH_PREFIX};
    let Some(tokens) = json.get_mut("tokens").and_then(|v| v.as_object_mut()) else {
        return false;
    };
    let mut stubbed = false;
    for (field, prefix) in [
        ("access_token", STUB_ACCESS_PREFIX),
        ("refresh_token", STUB_REFRESH_PREFIX),
    ] {
        let real = tokens
            .get(field)
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .filter(|s| !s.is_empty());
        if let Some(real) = real {
            let stub = mint_curated_stub(prefix);
            tokens.insert(field.to_string(), serde_json::Value::String(stub.clone()));
            pairs.push(SwapPair {
                stub,
                real,
                hosts: hosts.to_vec(),
            });
            stubbed = true;
        }
    }
    stubbed
}

/// A curated-prefix stub `<prefix>pllbxstub<uuid>` for a token whose real bytes
/// must never appear in the stub (an opaque or JWT token). The public prefix is a
/// fixed pillbox marker, not derived from the real, so a leaked stub reveals
/// nothing of the credential. Contrast [`mint_oauth_stub`] (claude's `sk-ant`
/// shape, where the first 3 hyphen segments are a safe public type marker).
fn mint_curated_stub(prefix: &str) -> String {
    format!("{prefix}pllbxstub{}", uuid::Uuid::now_v7().simple())
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

/// The hosts the agent's OAuth credential swap is bound to: ONLY its owning
/// provider's hosts (its API + OAuth/refresh endpoints), never the full
/// cross-provider [`intercepted_hosts`] union. Binding to the union would let a
/// guest replay this agent's OAuth stub onto a *different* provider's host (e.g. a
/// claude stub to `api.openai.com`) and extract the real token there. Reachability
/// (the DNS fence) is a separate axis. Empty when no provider claims the agent — a
/// non-vault agent; the launch guard ([`env_fork_left_real_unstubbed`]) catches a
/// vault-capable agent that nonetheless stubs nothing.
fn oauth_swap_hosts(spec: &AgentSpec) -> Vec<String> {
    crate::vault::providers::provider_for(spec.auth_id)
        .map(|p| p.hosts().iter().map(|h| (*h).to_string()).collect())
        .unwrap_or_default()
}

/// True when the env-fork left a real credential exposed: a vault-capable agent
/// that has an on-disk credentials file but produced no stub pairs would mount the
/// real token into the guest unstubbed (exfiltratable by a prompt-injected agent).
/// `prepare_launch` turns this into a hard launch failure rather than a silent
/// leak. Non-vault agents (opencode, pi) legitimately mount their key as-is, so
/// they're exempt; an agent that isn't logged in (no creds file) has nothing to
/// leak.
fn env_fork_left_real_unstubbed(spec: &AgentSpec, creds_root: &Path, pairs: &[SwapPair]) -> bool {
    spec.vault_capable && pairs.is_empty() && creds_root.join(spec.cred_sentinel).exists()
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
    .with_next("retry on the docker backend: PILLBOX_BACKEND=docker")
    .into()
}

/// Materialize the runner OCI image into a cached on-disk directory usable as a
/// virtio-fs root (libkrun's `krun_set_root` takes a *directory*, not an image).
/// One-time per concrete image via `docker export`; cached under
/// `~/.pillbox/krun/rootfs/`. The key includes Docker's image id, not just the
/// tag string, so mutable tags (`:rolling`) rematerialize after `docker pull`
/// instead of reusing an older exported rootfs.
fn materialize_rootfs(resolved: &Pillbox) -> Result<PathBuf> {
    let (image, _) = crate::docker::resolve_runner_image(resolved);
    let rootfs_root = krun_cache_dir()?.join("rootfs");
    // The id-keyed cache needs Docker to resolve the tag → id. When Docker is
    // unreachable (daemon down, image pruned) we can't compute the live key, but
    // a prior materialization may already be on disk — boot from the newest
    // cached generation for this image rather than hard-failing on the daemon.
    let image_id = match docker_image_id(&image) {
        Ok(id) => id,
        Err(_) => {
            if let Some(cache) = find_cached_rootfs(&rootfs_root, &image) {
                eprintln!(
                    "pillbox: note: docker unavailable — reusing cached rootfs for {image} (may be stale)"
                );
                return Ok(cache);
            }
            return Err(PillboxError::resource(
                "rootfs materialize",
                format!("docker unavailable and no cached rootfs for `{image}`"),
            )
            .with_next(format!("start Docker, then `docker pull {image}`"))
            .into());
        }
    };
    let cache = rootfs_root.join(rootfs_cache_key(&image, &image_id));
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

    // Stream the container filesystem straight into the cache dir. Capture both
    // commands' stdio: `docker rm` echoes the container id, and `run --json`'s
    // machine-readable stdout must stay pure JSON (a leaked line corrupts the
    // surface and the caller loses the session id).
    let export = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "docker export {cid} | tar -C {} -xf -",
            cache.display()
        ))
        .output()
        .map(|o| o.status);
    let _ = Command::new("docker").args(["rm", "-f", &cid]).output();
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
    std::fs::write(&marker, format!("{image}\n{image_id}\n"))
        .context("write rootfs cache marker")?;
    Ok(cache)
}

/// Newest materialized rootfs generation for `image`, or `None`. The fallback
/// when Docker can't resolve the live image id: scan the rootfs cache root for
/// generation dirs whose `.materialized` marker's first line is exactly `image`
/// (the marker is `format!("{image}\n{image_id}\n")`), and pick the one with the
/// most-recent marker mtime — the freshest export we have for this tag.
fn find_cached_rootfs(root: &Path, image: &str) -> Option<PathBuf> {
    let mut best: Option<(PathBuf, std::time::SystemTime)> = None;
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let marker = dir.join(".materialized");
        let Ok(text) = std::fs::read_to_string(&marker) else {
            continue;
        };
        if text.lines().next() != Some(image) {
            continue;
        }
        let Ok(mtime) = marker.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if best.as_ref().is_none_or(|(_, t)| mtime > *t) {
            best = Some((dir, mtime));
        }
    }
    best.map(|(dir, _)| dir)
}

fn docker_image_id(image: &str) -> Result<String> {
    let out = Command::new("docker")
        .arg("image")
        .arg("inspect")
        .arg(image)
        .arg("--format")
        .arg("{{.Id}}")
        .output()
        .with_context(|| format!("inspect runner image {image}"))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    bail!(
        "docker image inspect {image} failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    )
}

fn rootfs_cache_key(image: &str, image_id: &str) -> String {
    format!("{}_{}", sanitize(image), sanitize(image_id))
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
    use super::{
        env_fork_left_real_unstubbed, find_cached_rootfs, mint_curated_stub, mint_oauth_stub,
        oauth_swap_hosts, stub_claude_oauth, stub_codex_oauth, SwapPair,
    };
    use crate::agents::{CLAUDE, CODEX, PI};

    #[test]
    fn stub_claude_oauth_postdates_expiry_and_swaps_tokens() {
        // Realistic reals: mint_oauth_stub derives the stub from the real's `sk-ant`
        // type prefix, so the fixture must carry that shape.
        let real_access = "sk-ant-oat01-REALACCESSBODYxyz";
        let real_refresh = "sk-ant-ort01-REALREFRESHBODYxyz";
        let mut json = serde_json::json!({
            "claudeAiOauth": {
                "accessToken": real_access,
                "refreshToken": real_refresh,
                "expiresAt": 1_700_000_000_000_u64,
                "subscriptionType": "pro",
            }
        });
        let hosts = vec!["api.anthropic.com".to_string()];
        let mut pairs = Vec::new();
        assert!(stub_claude_oauth(&mut json, &hosts, &mut pairs));

        let oauth = json.get("claudeAiOauth").unwrap();
        // Broker move: the guest-mounted stub is post-dated to year 2100 so the agent
        // never refreshes itself (the host-side pre-refresh + MITM own rotation).
        assert_eq!(
            oauth.get("expiresAt").and_then(|v| v.as_u64()),
            Some(crate::vault::STUB_FAR_FUTURE_EXPIRES_AT_MS)
        );
        // Real tokens replaced with stubs in the file the guest sees (the stub keeps
        // the public `sk-ant` type prefix but never the secret body)…
        let access = oauth.get("accessToken").and_then(|v| v.as_str()).unwrap();
        let refresh = oauth.get("refreshToken").and_then(|v| v.as_str()).unwrap();
        assert!(access.starts_with("sk-ant-oat01-") && access != real_access);
        assert!(refresh.starts_with("sk-ant-ort01-") && refresh != real_refresh);
        assert!(!access.contains("REALACCESSBODY") && !refresh.contains("REALREFRESHBODY"));
        // …and the real values live ONLY in the out-of-band swap pairs.
        assert!(pairs.iter().any(|p| p.real == real_access));
        assert!(pairs.iter().any(|p| p.real == real_refresh));
        // Other fields preserved.
        assert_eq!(
            oauth.get("subscriptionType").and_then(|v| v.as_str()),
            Some("pro")
        );
    }

    // Write a generation dir with a marker shaped like the real one, then set
    // its mtime so "newest wins" is testable without sleeping (`age_secs` ago).
    fn gen_dir(root: &std::path::Path, name: &str, first_line: Option<&str>, age_secs: u64) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(image) = first_line {
            let path = dir.join(".materialized");
            let f = std::fs::File::create(&path).unwrap();
            use std::io::Write;
            (&f).write_all(format!("{image}\nsha256:deadbeef\n").as_bytes())
                .unwrap();
            let when = std::time::SystemTime::now() - std::time::Duration::from_secs(age_secs);
            f.set_modified(when).unwrap();
        }
    }

    #[test]
    fn find_cached_rootfs_picks_newest_matching_image() {
        let root = tempfile::tempdir().unwrap();
        let image = "ghcr.io/vu1n/pillbox:rolling";
        // Two generations of the same image (older + newer) plus a dir for a
        // different image and a dir with no marker — only the newest match wins.
        gen_dir(root.path(), "old", Some(image), 100);
        gen_dir(root.path(), "new", Some(image), 1);
        gen_dir(root.path(), "other", Some("ghcr.io/vu1n/pillbox:pinned"), 0);
        gen_dir(root.path(), "nomarker", None, 0);

        let hit = find_cached_rootfs(root.path(), image).expect("a matching generation");
        assert_eq!(hit.file_name().unwrap(), "new", "expected the newest match");
    }

    #[test]
    fn find_cached_rootfs_ignores_non_matching_and_markerless() {
        let root = tempfile::tempdir().unwrap();
        gen_dir(root.path(), "other", Some("some/other:image"), 0);
        gen_dir(root.path(), "nomarker", None, 0);
        assert!(find_cached_rootfs(root.path(), "ghcr.io/vu1n/pillbox:rolling").is_none());
    }

    #[test]
    fn find_cached_rootfs_empty_root_is_none() {
        let root = tempfile::tempdir().unwrap();
        assert!(find_cached_rootfs(root.path(), "anything").is_none());
    }

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
    fn oauth_swap_hosts_binds_to_only_the_owning_provider() {
        // claude's OAuth swap binds to Anthropic's hosts — and MUST NOT include
        // another provider's host (the cross-host replay the binding closes).
        let hosts = oauth_swap_hosts(&CLAUDE);
        assert!(
            hosts.iter().any(|h| h == "api.anthropic.com"),
            "got: {hosts:?}"
        );
        assert!(
            !hosts.iter().any(|h| h == "api.openai.com"),
            "leaked openai host: {hosts:?}"
        );
        assert!(
            !hosts.iter().any(|h| h == "api.github.com"),
            "leaked github host: {hosts:?}"
        );
        // codex binds to its own (OpenAI ChatGPT) hosts, not anthropic's.
        let codex = oauth_swap_hosts(&CODEX);
        assert!(codex.iter().any(|h| h == "chatgpt.com"), "got: {codex:?}");
        assert!(
            !codex.iter().any(|h| h == "api.anthropic.com"),
            "leaked anthropic host: {codex:?}"
        );
        // pi has no vault provider → no swap hosts at all.
        assert!(oauth_swap_hosts(&PI).is_empty());
    }

    #[test]
    fn env_fork_guard_fires_for_vault_agent_with_unstubbed_creds() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path();
        // A vault-capable agent (codex) with a creds file but zero stub pairs means
        // the env-fork didn't understand its shape → must be flagged (the real
        // token would otherwise mount unstubbed).
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(home.join(CODEX.cred_sentinel), "{}").unwrap();
        assert!(env_fork_left_real_unstubbed(&CODEX, home, &[]));
        // Once pairs are minted, it's fine.
        let pair = SwapPair {
            stub: "s".into(),
            real: "r".into(),
            hosts: vec![],
        };
        assert!(!env_fork_left_real_unstubbed(
            &CODEX,
            home,
            std::slice::from_ref(&pair)
        ));
        // A non-vault agent (pi) mounts its key as-is by design — never flagged.
        std::fs::create_dir_all(home.join(".pi/agent")).unwrap();
        std::fs::write(home.join(PI.cred_sentinel), "{}").unwrap();
        assert!(!env_fork_left_real_unstubbed(&PI, home, &[]));
        // Not logged in (no creds file) → nothing to leak.
        let empty = tempfile::tempdir().unwrap();
        assert!(!env_fork_left_real_unstubbed(&CODEX, empty.path(), &[]));
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

    #[test]
    fn stub_codex_oauth_stubs_chatgpt_tokens_without_leaking_them() {
        // codex ChatGPT-mode auth.json: the access token is a JWT whose base64url
        // body contains '-' (the exact shape mint_oauth_stub would leak), the
        // refresh token is opaque. Both must be swapped for curated-prefix stubs
        // that share NO bytes with the real.
        let real_access = "eyJhbGciOiJSUzI1NiJ9.PA-YL0AD-with-dashes.SIGSEG";
        let real_refresh = "rt-OPAQUE-SECRETBODY";
        let mut json = serde_json::json!({
            "auth_mode": "ChatGPT",
            "tokens": {
                "id_token": "ID_TOKEN_UNTOUCHED",
                "access_token": real_access,
                "refresh_token": real_refresh,
                "account_id": "acc-123",
            },
        });
        let mut pairs = Vec::new();
        let hosts = vec!["chatgpt.com".to_string()];
        assert!(stub_codex_oauth(&mut json, &hosts, &mut pairs));

        let tokens = &json["tokens"];
        let stub_access = tokens["access_token"].as_str().unwrap();
        let stub_refresh = tokens["refresh_token"].as_str().unwrap();
        assert!(stub_access.starts_with("pb-codex-oat-"), "{stub_access}");
        assert!(stub_refresh.starts_with("pb-codex-ort-"), "{stub_refresh}");
        // No real-token bytes leak into the stubs (the JWT-body '-' footgun).
        for leak in ["PA-YL0AD", "with-dashes", "SIGSEG", "OPAQUE", "SECRETBODY"] {
            assert!(!stub_access.contains(leak), "access stub leaked {leak}");
            assert!(!stub_refresh.contains(leak), "refresh stub leaked {leak}");
        }
        // Non-credential fields are left intact.
        assert_eq!(tokens["id_token"], "ID_TOKEN_UNTOUCHED");
        assert_eq!(tokens["account_id"], "acc-123");

        // Two pairs, each carrying the real and bound to the agent's hosts.
        assert_eq!(pairs.len(), 2);
        assert!(pairs.iter().any(|p| p.real == real_access));
        assert!(pairs.iter().any(|p| p.real == real_refresh));
        assert!(pairs.iter().all(|p| p.hosts == hosts));
    }

    #[test]
    fn stub_codex_oauth_skips_apikey_mode() {
        // ApiKey-mode auth.json has no `tokens` block — nothing to stub, so the
        // launch guard fires (codex --vault is rejected for ApiKey mode anyway).
        let mut json = serde_json::json!({ "OPENAI_API_KEY": "sk-real" });
        let mut pairs = Vec::new();
        assert!(!stub_codex_oauth(
            &mut json,
            &["chatgpt.com".into()],
            &mut pairs
        ));
        assert!(pairs.is_empty());
    }

    #[test]
    fn mint_curated_stub_uses_the_prefix_and_is_unique() {
        let a = mint_curated_stub("pb-codex-oat-");
        let b = mint_curated_stub("pb-codex-oat-");
        assert!(a.starts_with("pb-codex-oat-pllbxstub"));
        assert_ne!(a, b, "uuid suffix must make each stub unique");
    }
}
