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
mod http;
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
    /// `false`: guest dials the host (foreground — parent binds + accepts).
    /// `true`: guest listens, libkrun binds `host_sock` (detach — it persists for
    /// reattach after the parent returns). Selects `krun_add_vsock_port{,2}`.
    #[serde(default)]
    listen: bool,
}

/// What a detached libkrun session stores in `Session::sandbox_id` (as JSON): the
/// persistent attach socket (libkrun-bound for the guest's listen) to reattach to,
/// the VMM child PID to signal on `rm`, and the CoW clones + spec file to scrub on
/// `rm`. The detached child + these artifacts outlive the launching CLI.
#[derive(Serialize, Deserialize)]
struct LibkrunHandle {
    sock: String,
    pid: i32,
    creds: String,
    workspace: String,
    spec: String,
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
        // Server-integration agents (opencode) run headless + are driven/read over
        // their HTTP API through a vsock port-forward — a distinct path with no PTY
        // (mirrors local_docker's split). Keep claude/codex on the PTY path below.
        if spec.integration == Integration::Server {
            return run_server(spec, opts, resolved);
        }
        if opts.vault {
            return Err(unsupported(spec, "--vault (egress + vault v2 is a later slice)"));
        }

        let run_started = std::time::SystemTime::now();
        let session_id = crate::session::Session::new_id();

        // Build the launch packet (rootfs, CoW workspace + creds, env, CA, script,
        // VmSpec). The env-fork guard fails fast inside if a real credential leaked.
        let launch = prepare_launch(spec, &opts, resolved)?;

        // Detach: spawn the VM to outlive the CLI + record the session, then return.
        // libkrun keeps the vault on detach (the MITM lives in the child, not the
        // parent), unlike local Docker. Reattach/teardown go through the session
        // record. Foreground (below) supervises + pumps the terminal inline.
        if opts.detach {
            return run_detached(spec, resolved, &session_id, &opts, launch);
        }

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
    // Detach: the guest pty-host *listens* (so the attach socket persists for
    // reattach after the parent returns); foreground: it dials the parent.
    let vsock_flag = if opts.detach { " --vsock-listen" } else { "" };
    let script = format!(
        "set -e; {net}; {ca_setup}; mkdir -p {home_q}; mount -t virtiofs creds {home_q}; \
         mkdir -p {gw_q}; mount -t virtiofs workspace {gw_q}; cd {gw_q}; \
         exec pillbox pty-host --vsock-port {ATTACH_PORT}{vsock_flag} -- {agent}",
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
            listen: opts.detach,
        }),
        egress: Some(EgressSpec {
            // The vault providers' full intercept set (API + OAuth/platform hosts)
            // — so the agent can reach its provider *and* refresh a token — plus
            // any invoker-declared `--egress-allow` hosts (forwarded, no swap).
            allowlist: crate::vault::providers::intercepted_hosts()
                .into_iter()
                .map(str::to_string)
                .chain(opts.egress_allow.iter().cloned())
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

/// Start the microVM detached: spawn the VMM child so it outlives the CLI (it's
/// reparented to init), hand it the reals on stdin, record the session, return.
/// No pump, no §0 tailer (that spawns on `reattach`), no teardown (the clones +
/// spec persist for the running VM; `kill_session` scrubs them). The guest
/// pty-host listens (set by `prepare_launch` when `opts.detach`), so libkrun's
/// bound socket persists for reattach.
fn run_detached(
    spec: &AgentSpec,
    resolved: &Pillbox,
    session_id: &str,
    opts: &RunOpts,
    launch: Launch,
) -> Result<()> {
    // The child reads the spec at startup, *after* we return — so persist it (a
    // `NamedTempFile` would delete on drop); `kill_session` removes it.
    let (_, spec_path) = launch.spec_file.keep().context("persist VMM spec for detach")?;

    let exe = std::env::current_exe().context("locate the pillbox binary to re-exec as VMM")?;
    let mut child = Command::new(&exe)
        .arg("__krun-vmm")
        .arg(&spec_path)
        .env_clear()
        .envs(launch.guest_env)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("spawn the detached libkrun VMM subprocess")?;

    // Env fork: hand the reals to the child's MITM on stdin (out-of-band), as the
    // foreground path does, then drop the pipe (EOF).
    if let Some(mut sin) = child.stdin.take() {
        let blob = serde_json::to_string(&launch.swap_pairs).unwrap_or_else(|_| "[]".into());
        use std::io::Write as _;
        let _ = sin.write_all(blob.as_bytes());
    }

    let handle = LibkrunHandle {
        sock: launch.attach_sock.to_string_lossy().into_owned(),
        pid: child.id() as i32,
        creds: launch.creds_share.to_string_lossy().into_owned(),
        workspace: launch.workspace_clone.to_string_lossy().into_owned(),
        spec: spec_path.to_string_lossy().into_owned(),
    };
    let session = crate::session::Session {
        id: session_id.to_string(),
        label: opts.label.clone(),
        remote: crate::session::LOCAL_REMOTE.to_string(),
        backend: crate::session::BACKEND_LIBKRUN.to_string(),
        sandbox_id: serde_json::to_string(&handle).context("encode libkrun handle")?,
        pty_pid: 0,
        agent_id: spec.id.to_string(),
        started_at: crate::session::now_rfc3339(),
        attached_pid: None,
        base_snapshot: None,
        result_snapshot: None,
        expires_at: opts.ttl_seconds.map(crate::session::expires_at_from_ttl),
        guest_cwd: launch.guest_workspace,
        server: None,
    };
    crate::session::write(resolved, &session)?;
    // Don't wait: the child (VM + egress + MITM, with the vault) is reparented to
    // init and keeps running.
    println!("pillbox: ✓ session `{}` started in background (libkrun)", session.id);
    println!("  Next: pillbox session attach {}", session.id);
    Ok(())
}

/// Run a `Server`-integration agent (opencode) in a microVM: boot the VM
/// running `opencode serve` + a vsock port-forward relay, then drive/read it
/// over its HTTP API through that forward. The VM is detached (the server
/// outlives the CLI, reaped by `session rm`) — `run` returns after creating the
/// opencode session + sending the prompt, like the docker server path.
///
/// Differences from the PTY path ([`prepare_launch`]): the creds clone is *not*
/// stubbed (opencode is non-vault — its real key must reach the provider; the
/// MITM still terminates+forwards the allowlisted hosts, with an empty swap
/// set), the guest entrypoint is `opencode serve` + `pillbox vsock-forward`
/// (not `pty-host`), and the vsock port carries HTTP, not the attach protocol.
///
/// NOTE (deliberate duplication): this shares ~30 lines of launch scaffolding
/// with `prepare_launch` (rootfs / workspace clone / env / CA / NIC / VmSpec).
/// Server mode is a genuinely different launch (creds, script, bring-up), so a
/// branch-laden shared builder would be worse; extract a shared prep helper once
/// both paths are stable (same call the L3 dedup note made).
fn run_server(spec: &AgentSpec, opts: RunOpts, resolved: &Pillbox) -> Result<()> {
    use crate::sandbox::opencode;

    // opencode is non-vault: refuse --vault / vaulted --with rather than hand it
    // a stub it would ship to the provider.
    let withs = resolve_with_entries(resolved, &opts.withs)?;
    if opts.vault || withs.iter().any(|w| w.meta.is_some()) {
        return Err(unsupported(spec, "the vault (opencode is not vault-capable)"));
    }

    let rootfs = materialize_rootfs(resolved)?;
    let home = spec.home_dir(resolved)?;
    if !home.join(spec.cred_sentinel).exists() {
        return Err(PillboxError::runtime(
            "run",
            format!("no stored credentials for `{}`", spec.id),
        )
        .with_next(format!("pillbox auth login --agent {}", spec.id))
        .into());
    }

    // Workspace: CoW clone + secret-scrub (same as the PTY path).
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

    // Creds: CoW clone the *real* auth home (no stub — opencode authenticates to
    // its provider directly; the MITM forwards it untouched).
    let creds_share = cow_clone_home(&home)?;

    // Env (no vault layer).
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

    // CA trust so the MITM can terminate+forward opencode's provider TLS (empty
    // swap = transparent forward). The CA key stays host-side.
    let vault_ca_dir = resolved.subdir("vault")?;
    let ca = crate::vault::Ca::ensure(&vault_ca_dir)
        .map_err(|e| anyhow::anyhow!("ensure vault CA: {e}"))?;
    let ca_cert_pem = std::fs::read_to_string(ca.cert_path()).context("read vault CA cert")?;
    guest_env.push(("NODE_EXTRA_CA_CERTS".into(), GUEST_CA_PATH.into()));

    // Guest entrypoint: NIC + CA + mounts, then `opencode serve` (background) and
    // the vsock forward relay (foreground — the script's main process).
    let home_q = shell_quote(GUEST_HOME);
    let gw_q = shell_quote(&guest_workspace);
    let serve = opencode::serve_args()
        .iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    let net = egress::guest_net_commands();
    let ca_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &ca_cert_pem);
    let ca_setup = format!(
        "printf '%s' {b64} | base64 -d > {GUEST_CA_PATH}; \
         update-ca-certificates >/dev/null 2>&1 || true",
        b64 = shell_quote(&ca_b64),
    );
    let script = format!(
        "set -e; {net}; {ca_setup}; mkdir -p {home_q}; mount -t virtiofs creds {home_q}; \
         mkdir -p {gw_q}; mount -t virtiofs workspace {gw_q}; cd {gw_q}; \
         {serve} & exec pillbox vsock-forward --vsock-port {FORWARD_PORT} --to-port {port}",
        port = opencode::SERVE_PORT,
    );

    let host_sock =
        krun_cache_dir()?.join(format!("opencode-{}.sock", uuid::Uuid::now_v7().simple()));
    let _ = std::fs::remove_file(&host_sock);

    let vmspec = VmSpec {
        rootfs: rootfs.to_string_lossy().into_owned(),
        vcpus: 2,
        ram_mib: 2048,
        shares: vec![
            Share {
                tag: "creds".into(),
                host_path: creds_share.to_string_lossy().into_owned(),
            },
            Share {
                tag: "workspace".into(),
                host_path: clone.to_string_lossy().into_owned(),
            },
        ],
        exec: vec!["/bin/sh".into(), "-c".into(), script],
        // Guest listens; the host dials `host_sock` once per HTTP request.
        vsock: Some(VsockAttach {
            port: FORWARD_PORT,
            host_sock: host_sock.to_string_lossy().into_owned(),
            listen: true,
        }),
        egress: Some(EgressSpec {
            // opencode is non-vault: allow the vault-intercepted hosts (so an
            // anthropic/openai-backed model works + gets the swap) UNION the
            // "standard" model-provider set (openrouter/deepseek/kimi/grok/
            // gemini/glm/… — terminated + forwarded with an empty swap, since
            // opencode holds its own key). Anything else is fenced (NXDOMAIN).
            allowlist: crate::vault::providers::intercepted_hosts()
                .into_iter()
                .chain(egress::standard_egress_hosts().iter().copied())
                .map(str::to_string)
                .chain(opts.egress_allow.iter().cloned())
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
    // The child reads the spec after we return (detached) — persist it.
    let (_, spec_path) = spec_file.keep().context("persist VMM spec")?;

    // Spawn the VM detached (it runs the server + relay, reparented to init).
    let exe = std::env::current_exe().context("locate the pillbox binary to re-exec as VMM")?;
    let mut child = Command::new(&exe)
        .arg("__krun-vmm")
        .arg(&spec_path)
        .env_clear()
        .envs(guest_env)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("spawn the libkrun VMM subprocess")?;
    // No swap pairs (non-vault): hand the child's MITM an empty set + EOF.
    if let Some(mut sin) = child.stdin.take() {
        use std::io::Write as _;
        let _ = sin.write_all(b"[]");
    }
    let pid = child.id() as i32;

    let session_id = crate::session::Session::new_id();
    let http = http::LibkrunHttp::new(host_sock.clone());
    let model = opts
        .model
        .clone()
        .unwrap_or_else(|| opencode::DEFAULT_MODEL.to_string());
    let prompt = opts.args.join(" ").trim().to_string();

    // Bring-up over the forward; capture the result so a failure tears the VM
    // down rather than leaking it + the clones.
    let built = (|| -> Result<crate::session::Session> {
        opencode::wait_ready(&http)?;
        let ocid = opencode::create_session(&http)?;
        let handle = LibkrunHandle {
            sock: host_sock.to_string_lossy().into_owned(),
            pid,
            creds: creds_share.to_string_lossy().into_owned(),
            workspace: clone.to_string_lossy().into_owned(),
            spec: spec_path.to_string_lossy().into_owned(),
        };
        let session = crate::session::Session {
            id: session_id.clone(),
            label: opts.label.clone(),
            remote: crate::session::LOCAL_REMOTE.to_string(),
            backend: crate::session::BACKEND_LIBKRUN.to_string(),
            sandbox_id: serde_json::to_string(&handle).context("encode libkrun handle")?,
            pty_pid: 0,
            agent_id: spec.id.to_string(),
            started_at: crate::session::now_rfc3339(),
            attached_pid: None,
            base_snapshot: None,
            result_snapshot: None,
            expires_at: opts.ttl_seconds.map(crate::session::expires_at_from_ttl),
            guest_cwd: guest_workspace.clone(),
            server: Some(crate::session::ServerSession {
                agent_session_id: ocid.clone(),
                model: model.clone(),
            }),
        };
        crate::session::write(resolved, &session)?;
        crate::events::emit_session_event(
            resolved,
            crate::events::EventType::SessionStarted {
                parent_session_id: crate::events::parent_session_id_from_env(),
            },
            &session.id,
            Some(&session),
        );
        if !prompt.is_empty() {
            opencode::send_prompt(&http, &ocid, &prompt, &model)?;
        }
        Ok(session)
    })();
    let session = match built {
        Ok(s) => s,
        Err(e) => {
            if pid > 0 {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            let _ = std::fs::remove_file(&host_sock);
            let _ = std::fs::remove_file(&spec_path);
            let _ = std::fs::remove_dir_all(&creds_share);
            let _ = std::fs::remove_dir_all(&clone);
            return Err(e);
        }
    };

    opencode::print_started(&session, opts.json, !prompt.is_empty());
    Ok(())
}

/// `SandboxHttp` to a libkrun server session's in-guest opencode server, decoded
/// from its [`LibkrunHandle`]. Used by `session send`/`watch`/`subscribe`.
pub(crate) fn opencode_http(
    session: &crate::session::Session,
) -> Result<Box<dyn crate::sandbox::http::SandboxHttp>> {
    let handle: LibkrunHandle =
        serde_json::from_str(&session.sandbox_id).context("decode libkrun session handle")?;
    Ok(Box::new(http::LibkrunHttp::new(PathBuf::from(handle.sock))))
}

/// Reattach to a detached libkrun session: dial the persistent attach socket
/// libkrun bound for the guest's listening pty-host, and pump the terminal (with
/// the detach hotkey enabled). The agent + its screen persisted in the VM.
pub(crate) fn reattach(resolved: &Pillbox, session: &crate::session::Session) -> Result<()> {
    let handle: LibkrunHandle =
        serde_json::from_str(&session.sandbox_id).context("decode libkrun session handle")?;
    eprintln!("pillbox: reattaching to session `{}` …", session.id);
    eprintln!("pillbox: detach with Ctrl-A D");
    crate::session::mark_attached(resolved, &session.id, std::process::id() as i64)?;
    let outcome = (|| -> Result<pump::Outcome> {
        let stream = UnixStream::connect(&handle.sock)
            .with_context(|| format!("connect attach socket {}", handle.sock))?;
        let write_half = stream.try_clone().context("clone attach stream")?;
        pump::attach_terminal(stream, write_half, true)
    })();
    let _ = crate::session::mark_detached(resolved, &session.id);
    match outcome? {
        pump::Outcome::Detached | pump::Outcome::Disconnected => {
            eprintln!("pillbox: detached. reattach with `pillbox session attach {}`", session.id);
            Ok(())
        }
        pump::Outcome::Exited(code) => {
            eprintln!("pillbox: agent exited ({code}). `pillbox session rm {}` to clean up.", session.id);
            Ok(())
        }
    }
}

/// Tear down a detached libkrun session: kill the VMM child (the VM + egress +
/// MITM go with it), scrub the persisted socket/spec/CoW clones, drop the record.
pub(crate) fn kill_session(resolved: &Pillbox, session: &crate::session::Session) -> Result<()> {
    let handle: LibkrunHandle =
        serde_json::from_str(&session.sandbox_id).context("decode libkrun session handle")?;
    if handle.pid > 0 {
        unsafe { libc::kill(handle.pid, libc::SIGKILL) };
    }
    let _ = std::fs::remove_file(&handle.sock);
    let _ = std::fs::remove_file(&handle.spec);
    let _ = std::fs::remove_dir_all(&handle.creds);
    let _ = std::fs::remove_dir_all(&handle.workspace);
    crate::events::emit_session_event(
        resolved,
        crate::events::EventType::SessionDropped,
        &session.id,
        Some(session),
    );
    crate::session::delete(resolved, &session.id)?;
    println!("pillbox: ✓ session `{}` removed.", session.id);
    Ok(())
}

/// vsock port the guest pty-host dials for the attach channel.
const ATTACH_PORT: u32 = 1024;

/// vsock port the guest's opencode port-forward relay listens on (server mode);
/// the host dials it per HTTP request. Distinct from [`ATTACH_PORT`].
const FORWARD_PORT: u32 = 1025;

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
    let vsock = spec.vsock.as_ref().map(|v| (v.port, cstr(&v.host_sock), v.listen));
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
        bail!("clonefile creds {} → {} failed: {err} (APFS only)", home.display(), clone.display());
    }
    Ok(clone)
}

/// CoW-clone the auth home and replace its OAuth tokens with stubs (the
/// env-fork): the guest mounts the stubbed clone, the reals reach the MITM
/// out-of-band. Returns the clone + the stub→real swap pairs. Server-mode
/// agents (opencode, non-vault) skip this and mount [`cow_clone_home`] as-is —
/// their real key must reach the provider, so there's nothing to swap.
fn stub_oauth_creds(home: &Path, sentinel: &str) -> Result<(PathBuf, Vec<SwapPair>)> {
    let clone = cow_clone_home(home)?;
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
