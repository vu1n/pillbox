//! Session lifecycle for the libkrun backend — **feature-gated** (`libkrun`).
//!
//! The launch/attach/detach/reattach/teardown choreography on top of the VMM
//! substrate in [`super`]: building the launch packet ([`prepare_launch`] /
//! [`launch_base`]), the [`SandboxBackend::run`] entry (foreground PTY pump,
//! `--detach`, and the opencode server path), reattach/kill, and the §0/opencode
//! accessors `commands::session` calls. The VMM child entry, the spec types, and
//! the CoW/stub/rootfs helpers stay in [`super`] (shared with `vmm_child_main`).

use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::agents::{
    resolve_run_env, resolve_with_entries, workspace_mount_name, AgentSpec, Integration, RunOpts,
    GUEST_HOME, GUEST_WORKSPACE,
};
use crate::attach::pump;
use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::sandbox::SandboxBackend;
use crate::workspace::WorkspaceBackend;

// VMM substrate kept in the parent module (used here AND by `vmm_child_main`):
// spec types, the CoW/stub/rootfs helpers, the cache dir, and shared consts.
use super::{
    cow_clone_and_scrub, cow_clone_home, egress, http, krun_cache_dir, materialize_rootfs,
    shell_quote, stub_oauth_creds, unsupported, EgressSpec, LibkrunBackend, Share, SwapPair,
    VmSpec, VsockAttach, GUEST_CA_PATH,
};

impl SandboxBackend for LibkrunBackend {
    fn run(&self, spec: &AgentSpec, opts: RunOpts, resolved: &Pillbox) -> Result<()> {
        // Server-integration agents (opencode) run headless + are driven/read over
        // their HTTP API through a vsock port-forward — a distinct path with no PTY
        // (mirrors local_docker's split). Keep claude/codex on the PTY path below.
        if spec.integration == Integration::Server {
            return run_server(spec, opts, resolved);
        }
        if opts.vault {
            return Err(unsupported(
                spec,
                "--vault (egress + vault v2 is a later slice)",
            ));
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

/// The guest entrypoint preamble shared by every libkrun launch (PTY agents and
/// the opencode server): bring the NIC up, install the vault CA, mount the creds
/// and workspace virtio-fs shares, and `cd` into the workspace. The caller
/// appends its own exec. `home_q`/`gw_q` are pre-[`shell_quote`]d (a workspace
/// name may contain a space). The CA travels as single-line base64 decoded
/// in-guest — a raw multi-line PEM in the exec argv trips libkrun's cmdline
/// encoder (`InvalidAscii` on the newlines).
fn guest_launch_preamble(ca_cert_pem: &str, home_q: &str, gw_q: &str) -> String {
    let net = egress::guest_net_commands();
    let ca_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, ca_cert_pem);
    format!(
        "set -e; {net}; \
         printf '%s' {ca_b64q} | base64 -d > {GUEST_CA_PATH}; \
         update-ca-certificates >/dev/null 2>&1 || true; \
         mkdir -p {home_q}; mount -t virtiofs creds {home_q}; \
         mkdir -p {gw_q}; mount -t virtiofs workspace {gw_q}; cd {gw_q}",
        ca_b64q = shell_quote(&ca_b64),
    )
}

/// The launch prologue shared by both libkrun paths — `prepare_launch` (PTY
/// agents) and `run_server` (opencode): materialize the rootfs, check the creds
/// exist, CoW + secret-scrub the workspace, compose the guest env (base vars +
/// bundles/`--with`, no vault layer), and ensure the vault CA (cert trusted in
/// the guest via `NODE_EXTRA_CA_CERTS` + the preamble; key stays host-side). Kept
/// in one place because it's security-sensitive setup the two paths must not let
/// drift; they diverge *after* this on creds (stub vs raw), entrypoint, and VmSpec.
struct LaunchBase {
    rootfs: PathBuf,
    home: PathBuf,
    workspace_clone: PathBuf,
    guest_workspace: String,
    guest_env: Vec<(String, String)>,
    ca_cert_pem: String,
    vault_ca_dir: PathBuf,
}

fn launch_base(spec: &AgentSpec, opts: &RunOpts, resolved: &Pillbox) -> Result<LaunchBase> {
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
    let workspace_clone = cow_clone_and_scrub(&workspace_host)?;

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

    let vault_ca_dir = resolved.subdir("vault")?;
    let ca = crate::vault::Ca::ensure(&vault_ca_dir)
        .map_err(|e| anyhow::anyhow!("ensure vault CA: {e}"))?;
    let ca_cert_pem = std::fs::read_to_string(ca.cert_path()).context("read vault CA cert")?;
    guest_env.push(("NODE_EXTRA_CA_CERTS".into(), GUEST_CA_PATH.into()));

    Ok(LaunchBase {
        rootfs,
        home,
        workspace_clone,
        guest_workspace,
        guest_env,
        ca_cert_pem,
        vault_ca_dir,
    })
}

/// Build everything needed to boot a PTY-agent microVM: the shared [`launch_base`]
/// prologue, then stub the agent's creds (the env-fork), assemble the in-guest
/// `pty-host` entrypoint, and write the VmSpec. The env-fork guard fails fast here
/// if a real credential reached a guest-readable channel (the env or the script).
fn prepare_launch(spec: &AgentSpec, opts: &RunOpts, resolved: &Pillbox) -> Result<Launch> {
    let LaunchBase {
        rootfs,
        home,
        workspace_clone: clone,
        guest_workspace,
        guest_env,
        ca_cert_pem,
        vault_ca_dir,
    } = launch_base(spec, opts, resolved)?;

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
    let agent = agent_argv
        .iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    let preamble = guest_launch_preamble(&ca_cert_pem, &home_q, &gw_q);
    // Detach: the guest pty-host *listens* (so the attach socket persists for
    // reattach after the parent returns); foreground: it dials the parent.
    let vsock_flag = if opts.detach { " --vsock-listen" } else { "" };
    let script = format!(
        "{preamble}; \
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

    let attach_sock =
        krun_cache_dir()?.join(format!("attach-{}.sock", uuid::Uuid::now_v7().simple()));
    let _ = std::fs::remove_file(&attach_sock);

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
    let (_, spec_path) = launch
        .spec_file
        .keep()
        .context("persist VMM spec for detach")?;

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
    println!(
        "pillbox: ✓ session `{}` started in background (libkrun)",
        session.id
    );
    println!("  Next: pillbox session attach {}", session.id);
    Ok(())
}

/// Run a `Server`-integration agent (opencode) in a microVM: boot the VM running
/// `opencode serve` + a vsock port-forward relay, then drive/read it over its
/// HTTP API through that forward. The VM is detached (the server outlives the
/// CLI, reaped by `session rm`); `run` returns once the opencode session exists
/// — it does NOT auto-send (see [`opencode::print_started`]).
///
/// Differs from the PTY path ([`prepare_launch`]) at the inline-commented sites:
/// creds cloned *unstubbed* (opencode is non-vault), entrypoint is `opencode
/// serve` + the forward relay (not `pty-host`), vsock carries HTTP. Shares the
/// launch preamble via [`guest_launch_preamble`]; the rest stays per-backend (a
/// branch-laden shared builder would be worse) — factor further only once both
/// paths are stable.
fn run_server(spec: &AgentSpec, opts: RunOpts, resolved: &Pillbox) -> Result<()> {
    use crate::sandbox::opencode;

    // opencode is non-vault: refuse --vault / vaulted --with rather than hand it
    // a stub it would ship to the provider.
    // opencode is non-vault: refuse --vault / vaulted --with before anything else
    // (the shared base would otherwise compose them into the guest env).
    let withs = resolve_with_entries(resolved, &opts.withs)?;
    if opts.vault || withs.iter().any(|w| w.meta.is_some()) {
        return Err(unsupported(
            spec,
            "the vault (opencode is not vault-capable)",
        ));
    }

    let LaunchBase {
        rootfs,
        home,
        workspace_clone: clone,
        guest_workspace,
        guest_env,
        ca_cert_pem,
        vault_ca_dir,
    } = launch_base(spec, &opts, resolved)?;

    // Creds: CoW clone the *real* auth home (no stub — opencode authenticates to
    // its provider directly; the MITM forwards it untouched, empty swap).
    let creds_share = cow_clone_home(&home)?;

    // Guest entrypoint: NIC + CA + mounts, then `opencode serve` (background) and
    // the vsock forward relay (foreground — the script's main process).
    let home_q = shell_quote(GUEST_HOME);
    let gw_q = shell_quote(&guest_workspace);
    let serve = opencode::serve_args()
        .iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    let preamble = guest_launch_preamble(&ca_cert_pem, &home_q, &gw_q);
    // §0 producer: a co-located, persistent `/event` capture (raw SSE → a file
    // in the shared/CoW home, so it persists + is host-readable). The host drains
    // this file on watch/subscribe (replay + follow) — complete capture, no host
    // daemon. curl holds one long-lived SSE connection open, so we re-open the
    // file per line (`printf >> file` opens+writes+closes) to force a virtio-fs
    // flush to the host backing on every line — otherwise the appends sit in the
    // guest's page cache (file stays open) and the host sees 0 bytes. The outer
    // loop reconnects if the stream drops.
    let events_q = shell_quote(&format!("{GUEST_HOME}/{}", opencode::EVENTS_FILE));
    let script = format!(
        "{preamble}; \
         {serve} & \
         ( while :; do curl -sN http://127.0.0.1:{port}/event 2>/dev/null \
             | while IFS= read -r l; do printf '%s\\n' \"$l\" >> {events_q}; done; \
           sleep 1; done ) & \
         exec pillbox vsock-forward --vsock-port {FORWARD_PORT} --to-port {port}",
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
        Ok(session)
    })();
    let session = match built {
        Ok(s) => s,
        Err(e) => {
            // Bring-up failed: kill AND reap the VMM child (we still own it — it
            // hasn't reparented to init yet) before scrubbing the CoW clones, so
            // we don't leave a zombie and don't race the dying child's virtio-fs
            // mounts on the clones we're removing. (The success path leaves the
            // child running, reparented; `kill_session` reaps that one by pid.)
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(&host_sock);
            let _ = std::fs::remove_file(&spec_path);
            let _ = std::fs::remove_dir_all(&creds_share);
            let _ = std::fs::remove_dir_all(&clone);
            return Err(e);
        }
    };

    // No auto-send: opencode comes up ready (wait_ready), so the first prompt
    // goes through `session send` — captured by a subscribed watch, not streamed
    // to no one at start.
    opencode::print_started(
        &session,
        opts.json,
        (!prompt.is_empty()).then_some(prompt.as_str()),
    );
    Ok(())
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

impl LibkrunHandle {
    /// Decode the handle a detached/server libkrun session stored in its record.
    fn decode(session: &crate::session::Session) -> Result<Self> {
        serde_json::from_str(&session.sandbox_id).context("decode libkrun session handle")
    }
}

/// `SandboxHttp` to a libkrun server session's in-guest opencode server, decoded
/// from its [`LibkrunHandle`]. Used by `session send`/`watch`/`subscribe`.
pub(crate) fn opencode_http(
    session: &crate::session::Session,
) -> Result<Box<dyn crate::sandbox::http::SandboxHttp>> {
    let handle = LibkrunHandle::decode(session)?;
    Ok(Box::new(http::LibkrunHttp::new(PathBuf::from(handle.sock))))
}

/// Host-side path of a libkrun server session's `/event` capture file (inside
/// the CoW creds clone the guest mounts at its home). The §0 read drains this
/// for `watch`/`subscribe`. See [`crate::sandbox::opencode::EVENTS_FILE`].
pub(crate) fn opencode_events_file(session: &crate::session::Session) -> Result<PathBuf> {
    let handle = LibkrunHandle::decode(session)?;
    Ok(PathBuf::from(handle.creds).join(crate::sandbox::opencode::EVENTS_FILE))
}

/// Host path of a libkrun session's result-workspace — the CoW clone the guest
/// mounted and the agent edited. Exposed via `session info --json` so consumers
/// (graders, the eval harness) get it from a stable surface instead of parsing
/// the session record.
pub(crate) fn workspace_path(session: &crate::session::Session) -> Result<PathBuf> {
    Ok(PathBuf::from(LibkrunHandle::decode(session)?.workspace))
}

/// Run a grader command in a one-shot microVM (the same runner rootfs/toolchain
/// the agent had) against `workspace` (the agent's edited tree, mounted), and
/// return `(exit_code, combined_output)`. Backs `session score --in-sandbox`:
/// real repos' tests need the image's toolchain, which host-side grading lacks.
///
/// By default the grader-VM is deliberately bare — NO vsock, NO egress (offline),
/// NO creds: the grade must be reproducible and secret-free, and a fresh env (vs
/// the agent's still-running VM with its ad-hoc `pip install`s) keeps the verdict
/// honest. The grader is responsible for its own setup; deps it can't fetch
/// (no network) must be vendored or in the image.
///
/// `egress_allow` (from `--grader-egress`) opts a *single grade* into network
/// for the listed hosts only — so a real repo's tests can `pip install` /
/// `npm install` deps. It reuses the run path's egress exactly: DNS-fence
/// (only these hosts resolve) → MITM terminate-and-forward with an **empty
/// swap** (no credential substitution — the grader holds no creds), and the
/// guest trusts the MITM leaf via the CA preamble. This trades the offline
/// reproducibility guarantee for reachability; the caller notes it in feedback.
///
/// `krun_start_enter` exits the child with the guest's code, so the child's exit
/// IS the grader's exit, and the guest console (merged via `2>&1`) is the
/// child's stdout.
pub(crate) fn score_in_sandbox(
    resolved: &Pillbox,
    workspace: &Path,
    cmd: &str,
    egress_allow: &[String],
) -> Result<(i32, String)> {
    use std::io::Read as _;
    let rootfs = materialize_rootfs(resolved)?;

    // Egress is opt-in: an empty allowlist keeps the bare/offline grader. When
    // hosts are declared, prepend the NIC-up + CA-trust preamble (so the guest's
    // TLS clients accept the MITM leaf) and route through the same fence as a run.
    let (egress, env_extra, script) = if egress_allow.is_empty() {
        // Mount the workspace, cd in, run the grader with stderr merged to the
        // console. `&&` so a failed mount/cd surfaces; the grader's exit is last.
        let script = format!(
            "mkdir -p /grade && mount -t virtiofs grade /grade && cd /grade && {{ {cmd} ; }} 2>&1"
        );
        (None, Vec::new(), script)
    } else {
        let vault_ca_dir = resolved.subdir("vault")?;
        let ca = crate::vault::Ca::ensure(&vault_ca_dir)
            .map_err(|e| anyhow::anyhow!("ensure vault CA: {e}"))?;
        let ca_pem = std::fs::read_to_string(ca.cert_path()).context("read vault CA cert")?;
        let ca_b64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            ca_pem.as_bytes(),
        );
        let net = egress::guest_net_commands();
        // `exec 2>&1` so setup output (a dep-fetch failure) lands in feedback too;
        // `set -e` aborts loudly on a setup failure; `set +e` before the grader so
        // its own exit code (not set -e) is the verdict, captured as the script's.
        // Write the CA where the cert envs below point — no `update-ca-certificates`
        // merge needed (it's not even on the grader's PATH), since the fence is
        // all-MITM: every allowlisted host terminates at our CA-signed leaf, so
        // trusting that one cert is sufficient (the same single-cert trust agents
        // use via NODE_EXTRA_CA_CERTS).
        let script = format!(
            "exec 2>&1; set -e; {net}; \
             printf '%s' {ca_b64q} | base64 -d > {GUEST_CA_PATH}; \
             mkdir -p /grade; mount -t virtiofs grade /grade; cd /grade; \
             set +e; {cmd}",
            ca_b64q = shell_quote(&ca_b64),
        );
        let egress = Some(EgressSpec {
            // Only the invoker-declared hosts resolve — the tightest fence (no
            // provider/standard-egress union; the grader reaches deps, nothing else).
            allowlist: egress_allow.to_vec(),
            log_path: std::env::var("PILLBOX_KRUN_EGRESS_LOG").ok(),
            ca_dir: Some(vault_ca_dir.to_string_lossy().into_owned()),
        });
        // Point every TLS client at the single MITM CA cert. pip/curl/openssl/
        // requests ignore the system store by default; Node reads NODE_EXTRA_CA_CERTS.
        // All point at GUEST_CA_PATH (not the system bundle) so trust doesn't depend
        // on an `update-ca-certificates` merge.
        let env_extra = vec![
            ("NODE_EXTRA_CA_CERTS", GUEST_CA_PATH),
            ("PIP_CERT", GUEST_CA_PATH),
            ("REQUESTS_CA_BUNDLE", GUEST_CA_PATH),
            ("SSL_CERT_FILE", GUEST_CA_PATH),
            ("CURL_CA_BUNDLE", GUEST_CA_PATH),
        ];
        (egress, env_extra, script)
    };

    let vmspec = VmSpec {
        rootfs: rootfs.to_string_lossy().into_owned(),
        vcpus: 2,
        ram_mib: 2048,
        shares: vec![Share {
            tag: "grade".into(),
            host_path: workspace.to_string_lossy().into_owned(),
        }],
        exec: vec!["/bin/sh".into(), "-c".into(), script],
        vsock: None,
        egress,
    };
    let spec_file = tempfile::Builder::new()
        .prefix("pillbox-grade-spec-")
        .suffix(".json")
        .tempfile()
        .context("create grader VMM spec tempfile")?;
    serde_json::to_writer(&spec_file, &vmspec).context("write grader VMM spec")?;

    let exe = std::env::current_exe().context("locate the pillbox binary to re-exec as VMM")?;
    let mut child = Command::new(&exe)
        .arg("__krun-vmm")
        .arg(spec_file.path())
        // No secrets reach the grader — only a minimal guest env for the toolchain
        // (plus CA-bundle pointers when egress is on; all non-secret).
        .env_clear()
        .env("HOME", "/root")
        .env("TERM", "xterm-256color")
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .envs(env_extra.iter().copied())
        // Empty stdin: the VMM child reads swap pairs from stdin when egress is on
        // — null → EOF → empty swap (the MITM forwards untouched). No creds.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null()) // VMM diagnostics; the grade's output is the guest console (stdout)
        .spawn()
        .context("spawn the grader VMM subprocess")?;
    let mut out = String::new();
    if let Some(mut so) = child.stdout.take() {
        so.read_to_string(&mut out).ok();
    }
    let code = child
        .wait()
        .context("await grader VMM")?
        .code()
        .unwrap_or(-1);
    Ok((code, out))
}

/// Reattach to a detached libkrun session: dial the persistent attach socket
/// libkrun bound for the guest's listening pty-host, and pump the terminal (with
/// the detach hotkey enabled). The agent + its screen persisted in the VM.
pub(crate) fn reattach(resolved: &Pillbox, session: &crate::session::Session) -> Result<()> {
    let handle = LibkrunHandle::decode(session)?;
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
            eprintln!(
                "pillbox: detached. reattach with `pillbox session attach {}`",
                session.id
            );
            Ok(())
        }
        pump::Outcome::Exited(code) => {
            eprintln!(
                "pillbox: agent exited ({code}). `pillbox session rm {}` to clean up.",
                session.id
            );
            Ok(())
        }
    }
}

/// Tear down a detached libkrun session: kill the VMM child (the VM + egress +
/// MITM go with it), scrub the persisted socket/spec/CoW clones, drop the record.
pub(crate) fn kill_session(resolved: &Pillbox, session: &crate::session::Session) -> Result<()> {
    let handle = LibkrunHandle::decode(session)?;
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
            bail!(
                "libkrun VMM exited before attach was ready (status {:?})",
                status.code()
            );
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for the guest pty-host to connect");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
