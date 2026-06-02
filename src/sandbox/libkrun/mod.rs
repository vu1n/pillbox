//! Local microVM backend (libkrun) — **experimental, feature-gated** (`libkrun`).
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
//! Build + run (macOS/HVF):
//! ```text
//! cargo build --features libkrun
//! codesign --entitlements krun/entitlements.plist -f -s - target/debug/pillbox
//! PILLBOX_BACKEND=libkrun pillbox run --agent claude
//! ```
//! Re-codesign after every build (cargo invalidates the signature). Select at
//! runtime with `PILLBOX_BACKEND=libkrun`.

use std::ffi::CString;
use std::os::raw::{c_char, c_int};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

mod egress;
mod mitm;
mod vault;

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
}

#[derive(Serialize, Deserialize)]
struct VsockAttach {
    port: u32,
    host_sock: String,
}

/// The local microVM backend. Selected for a local run when the `libkrun`
/// feature is built in and `PILLBOX_BACKEND=libkrun` is set.
///
/// Mirrors `local_docker::run`'s creds + workspace + env pipeline (share the
/// agent's auth home, CoW-clone + secret-scrub the workspace, compose the run
/// env), launches the agent under an in-guest pty-host serving the `Frame`
/// protocol over vsock (L4), and attaches a userspace egress stack with a DNS
/// fence (L5a). The vault-v2 MITM (terminate + cred swap + forward) and §0 are
/// the remaining slices — L5b consumes the DNS-pin this egress stack populates.
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

        let run_started = std::time::SystemTime::now();
        let session_id = crate::session::Session::new_id();

        // Build the launch packet (rootfs, CoW workspace + creds, env, CA, script,
        // VmSpec). The env-fork guard fails fast inside if a real credential leaked.
        let launch = prepare_launch(spec, &opts, resolved)?;

        // §0: tail the agent's transcript from the host-side creds clone into the
        // durable SessionLog (the same producer docker/ssh use; no guest emitter).
        // Spawned before the child so it's ready when the agent first writes.
        let log = match crate::events::log::SessionLog::open(resolved, &session_id) {
            Ok(l) => Some(l),
            Err(e) => {
                eprintln!("pillbox: warning: couldn't open session log: {e:#}");
                None
            }
        };
        let tailer = crate::events::transcripts::spawn_session_observability(
            log,
            &session_id,
            spec.id,
            &launch.creds_share,
            &launch.guest_workspace,
            false,
            run_started,
        );

        // Spawn the VMM child (it becomes the VM), attach over vsock + pump the
        // terminal, then reap + tear down. `env_clear` so only the composed guest
        // env reaches the VM; the real creds go out-of-band on stdin.
        eprintln!(
            "pillbox: libkrun backend (experimental) — launching `{}` in a microVM",
            spec.id
        );
        let exe = std::env::current_exe().context("locate the pillbox binary to re-exec as VMM")?;
        // Bind the attach listener BEFORE booting: libkrun (in the child) dials it
        // when the guest pty-host connects out, so it must already exist.
        let listener = UnixListener::bind(&launch.attach_sock)
            .with_context(|| format!("bind attach socket {}", launch.attach_sock.display()))?;
        listener.set_nonblocking(true).ok();

        let mut child = Command::new(&exe)
            .arg("__krun-vmm")
            .arg(launch.spec_file.path())
            .env_clear()
            .envs(launch.guest_env)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .context("spawn the libkrun VMM subprocess")?;

        // Hand the real credentials to the child's MITM out-of-band on stdin — the
        // reals never touch the guest env, argv, or the VmSpec file. Closing the
        // pipe (drop) signals EOF so the child's read completes.
        if let Some(mut sin) = child.stdin.take() {
            let blob = serde_json::to_string(&launch.swap_pairs).unwrap_or_else(|_| "[]".into());
            use std::io::Write as _;
            let _ = sin.write_all(blob.as_bytes());
        }

        // Poll the listener until the guest pty-host dials in (across VM boot), then
        // run the shared terminal pump. Capture the result rather than `?` so the
        // teardown runs on every path (a failed accept/pump must not leak clones).
        let outcome = accept_attach(&listener, &mut child).and_then(|stream| {
            let write_half = stream.try_clone().context("clone attach stream")?;
            pump::attach_terminal(stream, write_half, false)
        });
        let _ = child.wait();
        // Final drain of the agent's last transcript lines into the SessionLog.
        if let Some(tailer) = tailer {
            tailer.shutdown();
        }
        // Teardown on every path (the §0 events are persisted, so the clones go).
        let _ = std::fs::remove_file(&launch.attach_sock);
        let _ = std::fs::remove_dir_all(&launch.creds_share);
        let _ = std::fs::remove_dir_all(&launch.workspace_clone);
        match outcome? {
            pump::Outcome::Exited(code) if code != 0 => std::process::exit(code),
            _ => Ok(()),
        }
    }
}

/// The artifacts [`prepare_launch`] builds for [`LibkrunBackend::run`] to spawn +
/// supervise. `spec_file` (the VmSpec tempfile) must outlive the child's read, so
/// it stays owned here; the CoW clones are torn down after the run.
struct Launch {
    spec_file: tempfile::NamedTempFile,
    attach_sock: PathBuf,
    guest_env: Vec<(String, String)>,
    swap_pairs: Vec<SwapPair>,
    creds_share: PathBuf,
    workspace_clone: PathBuf,
    guest_workspace: String,
}

/// Build everything needed to boot the microVM: materialize the rootfs, CoW +
/// secret-scrub the workspace, compose the guest env, ensure + trust the vault CA,
/// stub the agent's creds (the env-fork), assemble the guest entrypoint, and write
/// the VmSpec. The env-fork guard fails fast here if a real credential reached a
/// guest-readable channel (the env or the script).
fn prepare_launch(spec: &AgentSpec, opts: &RunOpts, resolved: &Pillbox) -> Result<Launch> {
    let rootfs = materialize_rootfs(resolved)?;

    // ── creds: the agent's auth home (stubbed + shared at GUEST_HOME below) ──
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
    let composed = resolve_run_env(resolved, opts, &withs, None)?;
    let mut guest_env: Vec<(String, String)> = vec![
        ("HOME".into(), GUEST_HOME.into()),
        ("TERM".into(), "xterm-256color".into()),
        (
            "PATH".into(),
            format!("/usr/local/bin:/usr/bin:/bin:{GUEST_HOME}/.local/bin"),
        ),
    ];
    guest_env.extend(composed);

    // ── vault MITM trust: ensure the per-pillbox CA, trust its (public) cert in
    // the guest. The CA *key* stays host-side (the child reads it); only the cert
    // reaches the guest. No HTTPS_PROXY — we're transparent via the DNS fence.
    let vault_ca_dir = resolved.subdir("vault")?;
    let ca = crate::vault::Ca::ensure(&vault_ca_dir)
        .map_err(|e| anyhow::anyhow!("ensure vault CA: {e}"))?;
    let ca_cert_pem = std::fs::read_to_string(ca.cert_path()).context("read vault CA cert")?;
    guest_env.push(("NODE_EXTRA_CA_CERTS".into(), GUEST_CA_PATH.into()));

    // Pre-accept the agent's workspace-trust dialog on the live auth home before
    // boot (claude); operates on host paths, like the docker path.
    spec.prepare_workspace_or_warn(&home, &guest_workspace);

    // Env fork: CoW the auth home and stub its OAuth tokens (after the seed so the
    // clone inherits it). The guest mounts the *stubbed* creds — the real tokens
    // never enter the VM; the MITM swaps stub→real on the wire. The reals reach the
    // child out-of-band on stdin (not env/argv/VmSpec).
    let (creds_share, swap_pairs) = stub_oauth_creds(&home, spec.cred_sentinel)?;

    // The guest entrypoint: bring up the NIC + trust the CA, mount the shares, cd
    // into the workspace, exec the agent under the in-guest pty-host (Frame over
    // vsock). Quote every interpolated path (a workspace name may contain a space).
    let agent_argv: Vec<String> = spec
        .run_argv
        .iter()
        .map(|s| s.to_string())
        .chain(spec.sandbox_args.iter().map(|s| s.to_string()))
        .chain(opts.args.iter().cloned())
        .collect();
    let home_q = shell_quote(GUEST_HOME);
    let gw_q = shell_quote(&guest_workspace);
    let agent = agent_argv.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ");
    let net = egress::guest_net_commands();
    // base64 the PEM: a raw multi-line cert in the exec argv trips libkrun's cmdline
    // encoder (`InvalidAscii` on the newlines). Single-line b64 → decode in-guest.
    let ca_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &ca_cert_pem);
    let ca_setup = format!(
        "printf '%s' {b64} | base64 -d > {GUEST_CA_PATH}; \
         update-ca-certificates >/dev/null 2>&1 || true",
        b64 = shell_quote(&ca_b64),
    );
    let script = format!(
        "set -e; {net}; {ca_setup}; mkdir -p {home_q}; mount -t virtiofs creds {home_q}; \
         mkdir -p {gw_q}; mount -t virtiofs workspace {gw_q}; cd {gw_q}; \
         exec pillbox pty-host --vsock-port {ATTACH_PORT} -- {agent}",
    );

    // ── env-fork invariant (the security thesis, guarded) ──
    // Three channels into the VM, and the real credential belongs to exactly one:
    // non-secret config → the guest env (`guest_env` → envp) and the exec
    // `script`/VmSpec (a host-readable tempfile); the real credential → ONLY the
    // MITM swap, out-of-band on the child's stdin + held in the VMM child's memory.
    // A real in a guest-readable channel is exfiltratable by a prompt-injected agent
    // — fail fast rather than silently leak if a future change crosses channels.
    for pair in &swap_pairs {
        if guest_env.iter().any(|(_, v)| v.contains(&pair.real)) || script.contains(&pair.real) {
            bail!(
                "libkrun env-fork violated: a real credential reached a guest-readable \
                 channel (env/script) — it must travel only via the MITM stdin swap"
            );
        }
    }

    let attach_sock = krun_cache_dir()?
        .join(format!("attach-{}.sock", uuid::Uuid::now_v7().simple()));
    let _ = std::fs::remove_file(&attach_sock);

    let vmspec = VmSpec {
        rootfs: rootfs.to_string_lossy().into_owned(),
        vcpus: 2,
        ram_mib: 2048,
        shares: vec![
            Share { tag: "creds".into(), host_path: creds_share.to_string_lossy().into_owned() },
            Share { tag: "workspace".into(), host_path: clone.to_string_lossy().into_owned() },
        ],
        exec: vec!["/bin/sh".into(), "-c".into(), script],
        vsock: Some(VsockAttach {
            port: ATTACH_PORT,
            host_sock: attach_sock.to_string_lossy().into_owned(),
        }),
        egress: Some(EgressSpec {
            // The vault providers' full intercept set (API + OAuth/platform hosts)
            // — so the agent can reach its provider *and* refresh a token.
            allowlist: crate::vault::providers::intercepted_hosts()
                .into_iter()
                .map(str::to_string)
                .collect(),
            log_path: std::env::var("PILLBOX_KRUN_EGRESS_LOG").ok(),
            ca_dir: Some(vault_ca_dir.to_string_lossy().into_owned()),
        }),
    };
    let spec_file = tempfile::Builder::new()
        .prefix("pillbox-krun-spec-")
        .suffix(".json")
        .tempfile()
        .context("create VMM spec tempfile")?;
    serde_json::to_writer(&spec_file, &vmspec).context("write VMM spec")?;

    Ok(Launch {
        spec_file,
        attach_sock,
        guest_env,
        swap_pairs,
        creds_share,
        workspace_clone: clone,
        guest_workspace,
    })
}

/// vsock port the guest pty-host dials for the attach channel.
const ATTACH_PORT: u32 = 1024;

/// Where the guest writes the vault CA cert (system trust dir → `update-ca-certificates`;
/// also `NODE_EXTRA_CA_CERTS` for Node agents). The cert is public — the key never leaves the host.
const GUEST_CA_PATH: &str = "/usr/local/share/ca-certificates/pillbox-vault.crt";

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
    // Egress: a passt socketpair — one end to libkrun's virtio-net, the other to
    // our userspace stack (which the child runs in a thread beside the VM).
    struct NetAttach {
        libkrun_fd: c_int,
        host_fd: c_int,
        allowlist: Vec<String>,
        ca_dir: Option<String>,
        log_path: Option<String>,
    }
    let net: Option<NetAttach> = spec.egress.as_ref().map(|e| {
        let mut fds = [0 as c_int; 2];
        if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) } != 0 {
            eprintln!("krun-vmm: egress socketpair: {}", std::io::Error::last_os_error());
            std::process::exit(1);
        }
        NetAttach {
            libkrun_fd: fds[0],
            host_fd: fds[1],
            allowlist: e.allowlist.clone(),
            ca_dir: e.ca_dir.clone(),
            log_path: e.log_path.clone(),
        }
    });
    // Read the stub→real credential pairs the parent pipes on stdin (the env-fork
    // channel — reals arrive out-of-band, never in the guest env/argv/VmSpec).
    let swap_pairs = if net.is_some() { read_swap_pairs() } else { Vec::new() };

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
        rc = rc.min(ffi::krun_set_exec(ctx, exec.as_ptr(), argv_ptrs.as_ptr(), envp.as_ptr()));
        if rc < 0 {
            eprintln!("krun-vmm: configuration failed (rc={rc})");
            std::process::exit(1);
        }
        // Run the userspace egress stack on our end of the socketpair before the
        // VM boots, so it's servicing frames the moment the guest's NIC comes up.
        // The thread dies when start_enter exit()s this process on VM shutdown.
        if let Some(n) = net {
            std::thread::spawn(move || {
                egress::run(n.host_fd, n.allowlist, n.ca_dir, swap_pairs, n.log_path)
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
        .map(|p| vault::CredSwap { stub: p.stub.into_bytes(), real: p.real.into_bytes() })
        .collect()
}

/// CoW-clone the agent's auth home and replace its OAuth tokens with stubs, so the
/// guest mounts *stubbed* creds — the real tokens never enter the VM; the MITM
/// swaps stub→real on the wire. Returns the stubbed-creds dir to mount + the
/// (stub, real) pairs. Anthropic-shaped (`claudeAiOauth` in the credentials file);
/// other agents get the home cloned as-is + no pairs (transparent relay).
fn stub_oauth_creds(home: &Path, sentinel: &str) -> Result<(PathBuf, Vec<SwapPair>)> {
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
        bail!("clonefile creds {} → {} failed: {err} (APFS only)", home.display(), clone.display());
    }

    let mut pairs = Vec::new();
    let creds_file = clone.join(sentinel);
    if let Ok(text) = std::fs::read_to_string(&creds_file) {
        if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(oauth) = json.get_mut("claudeAiOauth").and_then(|v| v.as_object_mut()) {
                for field in ["accessToken", "refreshToken"] {
                    let real = oauth.get(field).and_then(|v| v.as_str()).map(str::to_string);
                    if let Some(real) = real.filter(|s| !s.is_empty()) {
                        let stub = mint_stub(&real);
                        oauth.insert(field.to_string(), serde_json::Value::String(stub.clone()));
                        pairs.push(SwapPair { stub, real });
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

/// A unique stub shaped like `real` so a format check passes. **Keeps only the
/// fixed type prefix** — the first 3 hyphen segments (e.g. `sk-ant-oat01`) — and
/// only when the token clearly has a body *beyond* that prefix (≥4 segments, the
/// Anthropic `sk-ant-oat01-<body>` shape). The body is the secret and must NEVER
/// leak into the stub (which lands in the guest's creds), so a short/odd token
/// (<4 segments) gets a fully synthetic prefix instead. High-entropy uuid suffix
/// so the byte-level swap never false-matches on other content.
fn mint_stub(real: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::mint_stub;

    #[test]
    fn mint_stub_keeps_type_prefix_not_the_secret_body() {
        // Anthropic OAuth tokens are sk-ant-oat01-<base64url> and the body can
        // contain hyphens — the stub must keep only the type prefix, never the body.
        let real = "sk-ant-oat01-3WY-itf8QpVP38ipXjip-SECRETBODYxyz";
        let stub = mint_stub(real);
        assert!(stub.starts_with("sk-ant-oat01-pllbxstub"), "stub leaked shape: {stub}");
        assert!(!stub.contains("3WY"), "stub leaked body: {stub}");
        assert!(!stub.contains("SECRETBODYxyz"), "stub leaked body: {stub}");
        assert_ne!(stub, real);
        // Distinct each call (uuid suffix).
        assert_ne!(mint_stub(real), stub);
    }

    #[test]
    fn mint_stub_does_not_leak_short_or_odd_tokens() {
        // A token with <4 hyphen segments has no clear type/body split, so the
        // prefix must be synthetic — never any of the real bytes.
        for real in ["sk-secret", "justonesecretword", "sk-ant-secretbody"] {
            let stub = mint_stub(real);
            assert!(stub.starts_with("pllbx-pllbxstub"), "short token leaked shape: {stub}");
            assert!(!stub.contains("secret"), "short token leaked body: {stub}");
        }
    }
}
