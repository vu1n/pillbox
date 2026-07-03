//! Session lifecycle for the libkrun backend — **feature-gated** (`libkrun`).
//!
//! The launch/attach/detach/reattach/teardown choreography on top of the VMM
//! substrate in [`super`]: building the launch packet ([`prepare_launch`] /
//! [`launch_base`]), the [`SandboxBackend::run`] entry (foreground PTY pump,
//! `--detach`, and the opencode server path), reattach/kill, and the §0/opencode
//! accessors `commands::session` calls. The VMM child entry, the spec types, and
//! the CoW/stub/rootfs helpers stay in [`super`] (shared with `vmm_child_main`).
// Context: doc://pillbox/libkrun-env-fork-substrate@0001#libkrun-env-fork-substrate

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
use crate::sandbox::{Caps, SandboxBackend};
use crate::startup::StartupTimer;
use crate::workspace::{SnapshotHandle, WorkspaceBackend};

// VMM substrate kept in the parent module (used here AND by `vmm_child_main`):
// spec types, the CoW/stub/rootfs helpers, the cache dir, and shared consts.
use super::{
    boot, cow_clone_and_scrub, cow_clone_home, disk_headroom, egress, env_fork_left_real_unstubbed,
    http, krun_cache_dir, materialize_rootfs, oauth_swap_hosts, runtime_deps_present, shell_quote,
    stub_oauth_creds, unsupported, EgressSpec, LibkrunBackend, RefreshSpec, Share, SwapPair,
    VmSpec, VsockAttach, GUEST_CA_PATH, MIN_HEADROOM_BYTES,
};

impl SandboxBackend for LibkrunBackend {
    /// The microVM family: KVM-isolation features are uniquely libkrun's
    /// (real egress fence, in-sandbox grading, detached vault, post-hoc ingest).
    /// `pty_drive` drives the guest pty-host over the persistent attach socket;
    /// `live_pty_tail` tails the agent's transcript out of the host-readable
    /// creds clone. No long-lived exec target. See docs/substrate-plane.md.
    fn capabilities(&self) -> Caps {
        Caps {
            pty_drive: true,
            live_pty_tail: true,
            server_mode: true,
            long_lived_exec: false,
            in_sandbox_grading: true,
            real_egress_fence: true,
            detached_vault: true,
            post_hoc_ingest: true,
        }
    }

    fn id(&self) -> &'static str {
        crate::session::BACKEND_LIBKRUN
    }

    fn run(&self, spec: &AgentSpec, opts: RunOpts, resolved: &Pillbox) -> Result<()> {
        // Server-integration agents run headless + are driven/read over their HTTP
        // API through a vsock port-forward — a distinct path with no PTY (mirrors
        // docker's split). The two share `launch_server_vm`; this picks the
        // per-agent builder (codex-serve drives `codex app-server` via the in-guest
        // bridge; opencode runs `opencode serve`). PTY agents fall through below.
        if spec.integration == Integration::Server {
            return if spec.id == crate::agents::CODEX_SERVE.id {
                run_codex_serve(spec, opts, resolved)
            } else {
                run_server(spec, opts, resolved)
            };
        }
        // `--vault` is a no-op on libkrun (accepted for CLI parity with docker):
        // OAuth creds are *always* env-forked + MITM-swapped here, and egress is
        // always sole-fenced — there's no non-vault mode to switch on. A vaulted
        // `--with` (handled in `prepare_launch`) auto-enables the API-key swap on
        // its own, exactly like docker, no `--vault` required.

        let run_started = std::time::SystemTime::now();
        let session_id = crate::session::Session::new_id();

        // Build the launch packet (rootfs, CoW workspace + creds, env, CA, script,
        // VmSpec). The env-fork guard fails fast inside if a real credential leaked.
        let mut startup = StartupTimer::start();
        let launch = prepare_launch(spec, &opts, resolved)?;
        startup.mark("launch_prepare");

        // Detach: spawn the VM to outlive the CLI + record the session, then return.
        // libkrun keeps the vault on detach (the MITM lives in the child, not the
        // parent), unlike local Docker. Reattach/teardown go through the session
        // record. Foreground (below) supervises + pumps the terminal inline.
        if opts.detach {
            return run_detached(spec, resolved, &session_id, &opts, launch, startup);
        }

        // §0: tail the agent's transcript from the host-side creds clone into the
        // durable §0 sink (the same producer docker uses; no guest emitter).
        // `open_or_warn` picks the placement — local file by default, managed
        // Durable Object when the managed tier is on — and falls back to OTLP-only
        // if the open fails. Spawned before the child so it's ready when the agent
        // first writes.
        let log = crate::events::sink::open_or_warn(resolved, &session_id);
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
        // terminal, then reap + tear down. `env_clear` + the static base so the
        // kernel cmdline (which carries the child's env) stays printable-ASCII;
        // the composed guest env rides the boot script, the real creds stdin.
        eprintln!(
            "pillbox: libkrun backend — launching `{}` in a microVM",
            spec.id
        );
        let exe = std::env::current_exe().context("locate the pillbox binary to re-exec as VMM")?;
        // Bind the attach listener BEFORE booting: libkrun (in the child) dials it
        // when the guest pty-host connects out, so it must already exist.
        let listener = UnixListener::bind(&launch.attach_sock)
            .with_context(|| format!("bind attach socket {}", launch.attach_sock.display()))?;
        listener.set_nonblocking(true).ok();

        let mut cmd = Command::new(&exe);
        cmd.arg("__krun-vmm")
            .arg(launch.spec_file.path())
            .env_clear()
            .envs(boot::static_child_env())
            .stdin(std::process::Stdio::piped());
        // No `setsid` here: a foreground run reaps its VMM synchronously via
        // `child.wait` + the teardown below, and putting it in its own session
        // would orphan it on a Ctrl-C in the boot window (before the pump's raw
        // mode forwards signals in-band). setsid is for the detached spawns only.
        let mut child = cmd.spawn().context("spawn the libkrun VMM subprocess")?;

        // Hand the real credentials to the child's MITM out-of-band on stdin — the
        // reals never touch the guest env, argv, or the VmSpec file. Closing the
        // pipe (drop) signals EOF so the child's read completes.
        if let Some(mut sin) = child.stdin.take() {
            let blob = serde_json::to_string(&launch.swap_pairs).unwrap_or_else(|_| "[]".into());
            use std::io::Write as _;
            let _ = sin.write_all(blob.as_bytes());
        }
        startup.mark("vmm_spawn");

        // Poll the listener until the guest pty-host dials in (across VM boot), then
        // run the shared terminal pump. Capture the result rather than `?` so the
        // teardown runs on every path (a failed accept/pump must not leak clones).
        let outcome = match accept_attach(&listener, &mut child) {
            Ok(stream) => {
                startup.mark("attach_ready");
                let started_session = crate::session::Session {
                    id: session_id.clone(),
                    label: None,
                    backend: crate::session::BACKEND_LIBKRUN.to_string(),
                    sandbox_id: String::new(),
                    pty_pid: 0,
                    agent_id: spec.id.to_string(),
                    started_at: crate::session::now_rfc3339(),
                    attached_pid: Some(std::process::id() as i64),
                    base_snapshot: None,
                    result_snapshot: None,
                    expires_at: None,
                    guest_cwd: launch.guest_workspace.clone(),
                    placement: crate::session::Placement::Local,
                    server: None,
                };
                let startup_metrics = startup.finish("host_started_event");
                crate::events::emit_session_event(
                    resolved,
                    crate::events::EventType::SessionStarted {
                        parent_session_id: crate::events::parent_session_id_from_env(),
                        startup: Some(startup_metrics),
                    },
                    &started_session.id,
                    Some(&started_session),
                );
                let write_half = stream.try_clone().context("clone attach stream")?;
                pump::attach_terminal(stream, write_half, false)
            }
            Err(e) => Err(e),
        };
        let status = child.wait().ok();
        // Final drain of the agent's last transcript lines into the SessionLog.
        if let Some(tailer) = tailer {
            tailer.shutdown();
        }
        // Teardown on every path (the §0 events are persisted, so the clones go).
        let _ = std::fs::remove_file(&launch.attach_sock);
        let _ = std::fs::remove_dir_all(&launch.creds_share);
        let _ = std::fs::remove_dir_all(&launch.workspace_clone);
        // A SIGABRT'd VMM is the missing-runtime-dep footgun (`brew cleanup` swept
        // libkrun's undeclared dylibs). The attach/pump already failed with an
        // opaque "VMM exited" — replace it with the actionable cause when we can.
        if outcome.is_err() {
            if let Some(deps_err) = sigabrt_boot_error(status) {
                return Err(deps_err);
            }
        }
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
    swap_pairs: Vec<SwapPair>,
    creds_share: PathBuf,
    workspace_clone: PathBuf,
    guest_workspace: String,
    /// The run's vault CA — held so its per-run ephemeral backing dir (if any)
    /// outlives the supervised VMM child and is dropped after the run.
    _ca: VaultCa,
}

/// The boot-script preamble shared by every libkrun launch (PTY agents and
/// the opencode server): bring the NIC up, install the vault CA, mount the
/// workspace virtio-fs share, and `cd` into the workspace. The caller appends
/// its own exec. The creds share is already mounted by the boot channel's
/// static bootstrap (see [`boot::boot_channel`]) — the boot script itself
/// lives there. `gw_q` is pre-[`shell_quote`]d (a workspace name may contain
/// a space).
fn guest_launch_preamble(ca_cert_pem: &str, gw_q: &str) -> String {
    let net = egress::guest_net_commands();
    // The PEM is shell-quoted straight into the script — it lands in the boot
    // script file (see [`boot::boot_channel`]), which carries arbitrary bytes, so
    // the multi-line cert no longer needs the base64 detour the kernel cmdline
    // once forced.
    format!(
        "set -e; {net}; \
         printf '%s' {ca_q} > {GUEST_CA_PATH}; \
         update-ca-certificates >/dev/null 2>&1 || true; \
         mkdir -p {gw_q}; mount -t virtiofs workspace {gw_q}; cd {gw_q}",
        ca_q = shell_quote(ca_cert_pem),
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
    ca: VaultCa,
    /// Stub→real swaps for vaulted `--with` secrets (empty unless the run has
    /// any). The caller feeds `swap` into `swap_pairs` (the in-child MITM blob)
    /// and `host` into the egress allowlist — the host gate is what binds each
    /// release to its destination. Always empty on the server path (it refuses
    /// vaulted `--with` before reaching [`launch_base`]).
    with_vault: Vec<WithSwap>,
}

/// One vaulted `--with` secret's MITM material: the byte-substitution pair and
/// the host whose egress must be allowlisted for the swap to fire.
struct WithSwap {
    swap: SwapPair,
    host: String,
}

/// The CA a libkrun run's MITM uses, with its on-disk lifetime bundled in. `dir`
/// is what the VMM child reads (forging leaf certs throughout the run); `_tempdir`
/// is its backing store for the per-run ephemeral case (`Some`) — held in the same
/// value so the dir can't outlive the guard. The supervising caller keeps the
/// whole `VaultCa` alive (in [`Launch`] / a local) until the VM exits.
struct VaultCa {
    dir: PathBuf,
    cert_pem: String,
    _tempdir: Option<tempfile::TempDir>,
}

/// Whether a run's caller can host a per-run ephemeral CA. The discriminator is
/// *who supervises the VM*, not the user's `--detach` flag: a `Persistent` caller
/// reparents the VMM past this process (detached PTY, or server-mode — which
/// reparents regardless of `--detach`), so a host tempdir would vanish under the
/// still-running child; an `Ephemeral` caller supervises the VM to completion, so
/// the tempdir lives exactly as long as it's needed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CaLifetime {
    Ephemeral,
    Persistent,
}

/// Provision the vault CA for a libkrun run. If a stable CA has been pinned
/// (`pillbox vault ca` wrote one under `<pillbox>/vault/`), reuse it. Otherwise an
/// `Ephemeral` caller mints a per-run CA in a tempdir discarded after the run —
/// the guest reinstalls the cert every boot, so a stable CA buys it nothing, and
/// ephemeral shrinks a leaked CA key's blast radius to a single run. A
/// `Persistent` caller falls back to the per-pillbox dir (same trade as docker
/// `--detach`, which doesn't support `--vault`).
fn provision_vault_ca(resolved: &Pillbox, lifetime: CaLifetime) -> Result<VaultCa> {
    let persistent = resolved.subdir_path("vault");
    let pinned = crate::vault::ca_cert_path_in(&persistent).exists();
    let (dir, tempdir) = if pinned || lifetime == CaLifetime::Persistent {
        (resolved.subdir("vault")?, None) // subdir (not the probe path) creates + chmods 0700
    } else {
        let td = tempfile::Builder::new()
            .prefix("pillbox-vault-ca-")
            .tempdir()
            .context("create per-run vault CA dir")?;
        (td.path().to_path_buf(), Some(td))
    };
    let ca = crate::vault::Ca::ensure(&dir).map_err(|e| anyhow::anyhow!("ensure vault CA: {e}"))?;
    let cert_pem = std::fs::read_to_string(ca.cert_path()).context("read vault CA cert")?;
    Ok(VaultCa {
        dir,
        cert_pem,
        _tempdir: tempdir,
    })
}

fn launch_base(
    spec: &AgentSpec,
    opts: &RunOpts,
    resolved: &Pillbox,
    ca_lifetime: CaLifetime,
) -> Result<LaunchBase> {
    // Disk-pressure guard: the rootfs materialize + CoW workspace/creds clones
    // below all allocate on the krun cache fs. Below the floor a boot stalls
    // half-allocated rather than failing — so refuse loudly here, before any clone.
    disk_preflight()?;
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
    // Pick the directory the guest's CoW clone forks from:
    //  • --from-bookmark WITHOUT an explicit --workspace (the dispatch/default
    //    case): materialize the immutable snapshot ONCE into the shared base cache
    //    and clone from there. `k` workers off one bookmark pay a single rustic
    //    restore, not `k`, and the caller's cwd is left untouched (`pillbox pull`
    //    is the verb that restores a snapshot into cwd).
    //  • --from-bookmark WITH --workspace: the user asked for the content in that
    //    dir — restore into it, then clone it (behavior unchanged).
    //  • no bookmark: clone the workspace dir (cwd or --workspace) as before.
    let clone_src = match opts.from_bookmark.as_deref() {
        Some(name) => {
            let handle = crate::bookmarks::resolve_existing(resolved, name)?;
            if opts.workspace.is_some() {
                // WITH --workspace: restore into that dir, then clone it.
                resolved.workspace()?.pull(&workspace_host, Some(&handle))?;
                workspace_host.clone()
            } else {
                // Default/dispatch: warm-base cache; cwd untouched.
                warm_base(resolved, &handle)?
            }
        }
        None => workspace_host.clone(),
    };
    let workspace_name = workspace_mount_name(&workspace_host, opts.name.as_deref())?;
    let guest_workspace = format!("{GUEST_WORKSPACE}/{workspace_name}");
    let workspace_clone = cow_clone_and_scrub(&clone_src)?;

    // Split vaulted `--with` (stub-swapped through the MITM) from plain entries
    // (real value straight into the env). Plain entries + bundles/env-file compose
    // via the shared resolver with no vault session; the vaulted ones are handled
    // libkrun-style below (the docker path routes them through a host-side
    // `VaultSession` proxy, which libkrun has no equivalent of — its MITM lives in
    // the VMM child and is fed swaps over stdin).
    let (vaulted, plain): (Vec<_>, Vec<_>) = resolve_with_entries(resolved, &opts.withs)?
        .into_iter()
        .partition(|w| w.meta.is_some());
    if !vaulted.is_empty() && !spec.vault_capable {
        let names: Vec<&str> = vaulted.iter().map(|w| w.secret_name.as_str()).collect();
        return Err(PillboxError::usage(
            "run",
            format!(
                "agent `{}` does not support the vault, so it can't use vaulted secret(s): {}",
                spec.id,
                names.join(", ")
            ),
        )
        .into());
    }
    let composed = resolve_run_env(resolved, opts, &plain, None)?;
    let mut guest_env: Vec<(String, String)> = boot::static_child_env();
    guest_env.extend(composed);

    // Vaulted `--with`: inject a STUB into the env (never the real key), and carry
    // the {stub→real} swap + host out for the MITM. Same env-fork as OAuth — the
    // real reaches only the in-child MITM via stdin; the host allowlist binds the
    // swap to its destination (the guard in `prepare_launch` fails loud otherwise).
    let mut with_vault = Vec::with_capacity(vaulted.len());
    for w in &vaulted {
        let real = crate::secrets::read(resolved, &w.secret_name)?
            .ok_or_else(|| {
                PillboxError::runtime("run", format!("secret `{}` not found", w.secret_name))
                    .with_next(format!("pillbox secret add {}", w.secret_name))
            })?
            .trim_end()
            .to_string();
        let meta = w.meta.as_ref().expect("partitioned into vaulted");
        // Mint the stub from the secret's *declared* prefix, never from the real
        // value: deriving a prefix off `real` would copy body bytes into the stub
        // for keys whose public prefix isn't exactly 3 hyphen-segments (OpenAI
        // `sk-proj-…`, `sk-svcacct-…`), and that stub lands in the guest env. The
        // curated prefix also keeps the stub format-valid (e.g. GitHub `ghp_…`) so
        // in-guest SDK validators accept it. (Matches the docker host-side path.)
        let stub = crate::vault::providers::mint_stub(&meta.vault.prefix, spec.id);
        guest_env.push((w.env_var.clone(), stub.clone()));
        with_vault.push(WithSwap {
            swap: SwapPair {
                stub,
                real,
                // Bound to this secret's declared host — the swap fires only there.
                hosts: vec![meta.vault.host.clone()],
            },
            host: meta.vault.host.clone(),
        });
    }

    let ca = provision_vault_ca(resolved, ca_lifetime)?;
    // Node agents (claude, opencode) trust the per-run MITM CA via NODE_EXTRA_CA_CERTS
    // (additive to Node's built-ins). Rust/OpenSSL agents that need the CA in their own
    // trust store (codex's reqwest) get it via a per-agent `SSL_CERT_FILE` (see
    // ServerLaunch::extra_env) — NOT here: a process-wide `SSL_CERT_FILE` *replaces* the
    // trust set, and narrowing it to the vault CA broke opencode's bring-up (its startup
    // also reaches non-MITM'd hosts that need the base roots).
    guest_env.push(("NODE_EXTRA_CA_CERTS".into(), GUEST_CA_PATH.into()));

    Ok(LaunchBase {
        rootfs,
        home,
        workspace_clone,
        guest_workspace,
        guest_env,
        ca,
        with_vault,
    })
}

/// Build everything needed to boot a PTY-agent microVM: the shared [`launch_base`]
/// prologue, then stub the agent's creds (the env-fork), assemble the in-guest
/// `pty-host` entrypoint, and write the VmSpec. The env-fork guard fails fast here
/// if a real credential reached a guest-readable channel (the env or the script).
fn prepare_launch(spec: &AgentSpec, opts: &RunOpts, resolved: &Pillbox) -> Result<Launch> {
    // Foreground supervises the VM (ephemeral CA ok); `--detach` reparents it.
    let ca_lifetime = if opts.detach {
        CaLifetime::Persistent
    } else {
        CaLifetime::Ephemeral
    };
    let LaunchBase {
        rootfs,
        home,
        workspace_clone: clone,
        guest_workspace,
        guest_env,
        ca,
        with_vault,
    } = launch_base(spec, opts, resolved, ca_lifetime)?;

    // Pre-accept the agent's workspace-trust dialog on the live auth home before
    // boot (claude); operates on host paths, like the docker path.
    spec.prepare_workspace_or_warn(&home, &guest_workspace);

    // Env fork: CoW the auth home and stub its OAuth tokens (after the seed so the
    // clone inherits it). The guest mounts the *stubbed* creds — the real tokens
    // never enter the VM; the MITM swaps stub→real on the wire. The reals reach the
    // child out-of-band on stdin (not env/argv/VmSpec).
    // OAuth tokens are bound to ONLY the agent's own provider hosts (its API +
    // OAuth/refresh endpoints), never the full cross-provider intercepted_hosts()
    // union — so a leaked OAuth stub can't be replayed to a *different* provider's
    // host to extract the real token. Reachability (the DNS fence below) is a
    // separate axis.
    let oauth_hosts = oauth_swap_hosts(spec);
    // A vault-capable agent must resolve to a provider so its swap can be bound to
    // that provider's hosts. An empty host set is treated as "unbound" by the MITM
    // (applies on every host) — which would re-open the cross-host replay — so a
    // vault-capable agent with no provider is fail-closed here, not silently unbound.
    if spec.vault_capable && oauth_hosts.is_empty() {
        bail!(
            "libkrun env-fork: agent `{}` is vault-capable but no vault provider claims it, \
             so its credential swap can't be host-bound (an unbound swap applies on every \
             host). Refusing to launch.",
            spec.id
        );
    }

    // Broker pre-refresh: rotate the real OAuth token NOW — coordinated across
    // concurrent launches by the single-writer `TokenStore` — on the live creds file,
    // BEFORE the CoW clone below, so the stubbed clone the guest mounts inherits a
    // fresh token. Paired with the post-dated stub expiry (`stub_claude_oauth`), the
    // guest never refreshes itself, which dissolves the host-creds clobber and the
    // refresh-token-reuse race on libkrun the same way it does on the host-side proxy.
    // Fail closed: a stale token the guest can't self-heal (its expiry says year 2100)
    // would just 401 with no recovery. Placed AFTER the provider-resolution bail so an
    // aborted launch never burns a rotation. `pre_refresh` itself no-ops for any
    // `auth_id` without a broker decider (only claude today), so the `vault_capable`
    // gate just avoids the call for non-vaulted agents.
    if spec.vault_capable {
        crate::vault::pre_refresh(&home.join(spec.cred_sentinel), spec.auth_id)?;
    }

    let (creds_share, mut swap_pairs, access_stub) = stub_oauth_creds(&home, spec, &oauth_hosts)?;
    // Fail loud: a vault-capable agent whose credentials file produced no stubs
    // would mount the real token into the guest unstubbed (exfiltratable by a
    // prompt-injected agent). Refuse to launch rather than leak. Generalizing the
    // stubber to that agent's credential shape is the fix; until then this guard
    // keeps an unhandled shape safe. (Checked before the `--with` swaps are folded
    // in below, so their pairs can't mask an empty OAuth set.)
    if env_fork_left_real_unstubbed(spec, &home, &swap_pairs) {
        bail!(
            "libkrun env-fork: agent `{}` is vault-capable and has credentials at `{}`, but \
             the env-fork produced no credential stubs — its credential shape isn't handled \
             yet, so the real token would mount into the guest unstubbed. Refusing to launch.",
            spec.id,
            spec.cred_sentinel
        );
    }
    // Fold the vaulted `--with` swaps in alongside the OAuth ones (one MITM blob),
    // and collect their hosts for the egress allowlist below.
    let with_hosts: Vec<String> = with_vault
        .into_iter()
        .map(|w| {
            swap_pairs.push(w.swap);
            w.host
        })
        .collect();

    // Broker JIT refresh context for the in-VMM MITM: rotate the access token near its
    // expiry mid-session (the guest never refreshes — far-future stub expiry) so a
    // session outliving the token lifetime (~8h) doesn't 401 with no recovery. Built only
    // when the env-fork produced an access-token stub; the child arms JIT only for an
    // agent with a broker decider (`broker_expiry`, claude today) and no-ops it otherwise.
    // Non-secret: the child reads the real token from the live creds file host-side.
    let refresh = access_stub.map(|stub| RefreshSpec {
        creds_path: home.join(spec.cred_sentinel).to_string_lossy().into_owned(),
        auth_id: spec.auth_id.to_string(),
        access_stub: stub,
    });

    // The guest boot script: env exports, NIC + CA + workspace mount, then exec
    // the agent under the in-guest pty-host (Frame over vsock). Written into the
    // creds share and exec'd by the static cmdline bootstrap — the prompt in the
    // agent argv (and any env value) may carry newlines/unicode the cmdline can't
    // (see [`boot::boot_channel`]). Quote every interpolated path and argv element.
    let agent_argv: Vec<String> = spec
        .run_argv
        .iter()
        .map(|s| s.to_string())
        .chain(spec.sandbox_args.iter().map(|s| s.to_string()))
        .chain(opts.args.iter().cloned())
        .collect();
    let gw_q = shell_quote(&guest_workspace);
    let agent = agent_argv
        .iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    let preamble = guest_launch_preamble(&ca.cert_pem, &gw_q);
    // Detach: the guest pty-host *listens* (so the attach socket persists for
    // reattach after the parent returns); foreground: it dials the parent.
    let vsock_flag = if opts.detach { " --vsock-listen" } else { "" };
    let exports = boot::env_exports(&guest_env)?;
    let boot_script = format!(
        "{exports}{preamble}; \
         exec pillbox pty-host --vsock-port {ATTACH_PORT}{vsock_flag} -- {agent}",
    );

    // ── env-fork invariant (the security thesis, guarded) ──
    // Three channels into the VM, and the real credential belongs to exactly one:
    // non-secret config → the boot script (env exports + argv; a guest- and
    // host-readable file in the creds share); the real credential → ONLY the
    // MITM swap, out-of-band on the child's stdin + held in the VMM child's memory.
    // A real in a guest-readable channel is exfiltratable by a prompt-injected agent
    // — fail fast rather than silently leak if a future change crosses channels.
    // Raw env values are scanned pre-quoting (shell-quoting can mangle the needle,
    // e.g. `'` → `'\''`); the rendered script catches argv/path interpolations.
    for pair in &swap_pairs {
        if guest_env.iter().any(|(_, v)| v.contains(&pair.real)) || boot_script.contains(&pair.real)
        {
            bail!(
                "libkrun env-fork violated: a real credential reached a guest-readable \
                 channel (the boot script) — it must travel only via the MITM stdin swap"
            );
        }
    }
    let (boot_share, boot_exec) =
        boot::boot_channel(&creds_share, "creds", GUEST_HOME, &boot_script)?;

    let attach_sock =
        krun_cache_dir()?.join(format!("attach-{}.sock", uuid::Uuid::now_v7().simple()));
    let _ = std::fs::remove_file(&attach_sock);

    let vmspec = VmSpec {
        rootfs: rootfs.to_string_lossy().into_owned(),
        vcpus: 2,
        ram_mib: 2048,
        shares: vec![
            boot_share,
            Share {
                tag: "workspace".into(),
                host_path: clone.to_string_lossy().into_owned(),
            },
        ],
        exec: boot_exec,
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
                .chain(with_hosts.iter().cloned()) // vaulted --with hosts (the swap's destination)
                .collect(),
            log_path: std::env::var("PILLBOX_KRUN_EGRESS_LOG").ok(),
            ca_dir: Some(ca.dir.to_string_lossy().into_owned()),
            local_forward_port: None, // vaulted agents: no local-model forward
            refresh,
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
        swap_pairs,
        creds_share,
        workspace_clone: clone,
        guest_workspace,
        _ca: ca,
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
    mut startup: StartupTimer,
) -> Result<()> {
    // The child reads the spec at startup, *after* we return — so persist it (a
    // `NamedTempFile` would delete on drop); `kill_session` removes it.
    let (_, spec_path) = launch
        .spec_file
        .keep()
        .context("persist VMM spec for detach")?;

    let exe = std::env::current_exe().context("locate the pillbox binary to re-exec as VMM")?;
    let mut cmd = Command::new(&exe);
    cmd.arg("__krun-vmm")
        .arg(&spec_path)
        .env_clear()
        .envs(boot::static_child_env())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Self-destruct if this CLI dies before the record commits below (an abandoned
    // detached launch must not orphan its reparented VM).
    arm_commit_guard(&mut cmd, resolved, session_id);
    vmm_own_process_group(&mut cmd);
    let mut child = cmd
        .spawn()
        .context("spawn the detached libkrun VMM subprocess")?;

    // Env fork: hand the reals to the child's MITM on stdin (out-of-band), as the
    // foreground path does, then drop the pipe (EOF).
    if let Some(mut sin) = child.stdin.take() {
        let blob = serde_json::to_string(&launch.swap_pairs).unwrap_or_else(|_| "[]".into());
        use std::io::Write as _;
        let _ = sin.write_all(blob.as_bytes());
    }
    startup.mark("vmm_spawn");

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
        placement: crate::session::Placement::Local,
        server: None,
    };
    crate::session::write(resolved, &session)?;
    let startup_metrics = startup.finish("session_record");
    crate::events::emit_session_event(
        resolved,
        crate::events::EventType::SessionStarted {
            parent_session_id: crate::events::parent_session_id_from_env(),
            startup: Some(startup_metrics),
        },
        &session.id,
        Some(&session),
    );
    // Don't wait: the child (VM + egress + MITM, with the vault) is reparented to
    // init and keeps running.
    if opts.json {
        crate::session::print_started_json(&session);
    } else {
        println!(
            "pillbox: ✓ session `{}` started in background (libkrun)",
            session.id
        );
        println!("  Next: pillbox session attach {}", session.id);
    }
    Ok(())
}

/// Builds the guest entrypoint script from `(preamble, quoted_events_path)`.
type ScriptBuilder = Box<dyn FnOnce(&str, &str) -> String>;
/// Brings the server up over the forward → its agent-native session id.
type BringUp = Box<dyn FnOnce(&http::LibkrunHttp) -> Result<String>>;

/// Per-agent inputs to [`launch_server_vm`] — everything that differs between the
/// two server agents' microVM bring-up. The shared boot/teardown choreography
/// lives in `launch_server_vm`; the thin per-agent builders below fill this in.
struct ServerLaunch {
    /// Message for the `--vault`/vaulted-`--with` rejection (both server agents
    /// are non-vault in v1, but for different reasons).
    vault_refusal: &'static str,
    /// Extra egress hosts beyond the vault-intercepted set (+ invoker
    /// `--egress-allow`), terminated + forwarded with an empty swap.
    egress_extra: Vec<String>,
    /// Extra guest env vars for this agent only (appended to the shared
    /// `launch_base` set). codex-serve uses it to point its Rust TLS stack at the
    /// vault CA via `SSL_CERT_FILE`; opencode (Node, served by NODE_EXTRA_CA_CERTS)
    /// needs none — and must not get a narrowed `SSL_CERT_FILE`, which broke its
    /// bring-up by dropping the base roots its startup also reaches.
    extra_env: Vec<(String, String)>,
    /// Opt-in local-model forward port (opencode's `PILLBOX_LOCAL_MODEL_PORT`).
    local_forward_port: Option<u16>,
    /// The model recorded on the session (from `--model` or a per-agent default).
    model: String,
    /// Build the guest entrypoint script from the launch preamble + the quoted
    /// events-capture path (both produced inside `launch_server_vm`).
    build_script: ScriptBuilder,
    /// Bring the server up over the forward and return its agent-native session
    /// id (opencode session / codex thread). Runs after the VM boots.
    bringup: BringUp,
}

/// Boot a `Server`-integration agent in a detached microVM and record its
/// session — the shared bring-up + teardown for opencode and codex-serve. The
/// per-agent differences (guest script, events file, egress, bring-up calls)
/// arrive as a [`ServerLaunch`]; everything else (CoW creds, the `VmSpec`, the
/// detached `__krun-vmm` spawn, the empty-swap stdin, the session record + event,
/// the error-path teardown, `print_started`) is identical and lives here so a
/// launch invariant can't drift between the two agents. The VM outlives the CLI
/// (reaped by `session rm`); the call returns once the agent session exists and
/// does NOT auto-send (the first prompt goes through `session send`). Differs
/// from the PTY path ([`prepare_launch`]): creds cloned *unstubbed* (non-vault),
/// entrypoint is the server + forward relay (not `pty-host`), vsock carries HTTP.
fn launch_server_vm(
    spec: &AgentSpec,
    opts: RunOpts,
    resolved: &Pillbox,
    launch: ServerLaunch,
) -> Result<()> {
    let mut startup = StartupTimer::start();
    // Non-vault: refuse --vault / vaulted --with before anything else (the shared
    // base would otherwise compose them into the guest env).
    let withs = resolve_with_entries(resolved, &opts.withs)?;
    if opts.vault || withs.iter().any(|w| w.meta.is_some()) {
        return Err(unsupported(spec, launch.vault_refusal));
    }
    let profile = spec
        .server
        .expect("launch_server_vm requires a Server-integration agent");

    // Server-mode VMs always reparent (regardless of `--detach`), so the CA must
    // outlive this process — `Persistent`, never a host tempdir.
    let LaunchBase {
        rootfs,
        home,
        workspace_clone: clone,
        guest_workspace,
        mut guest_env,
        ca,
        with_vault: _, // server agents refuse vaulted --with above; always empty here
    } = launch_base(spec, &opts, resolved, CaLifetime::Persistent)?;
    guest_env.extend(launch.extra_env); // per-agent additions (codex's SSL_CERT_FILE)

    // Creds: CoW clone the *real* auth home (no stub — the agent authenticates to
    // its provider directly; the MITM forwards it untouched, empty swap).
    let creds_share = cow_clone_home(&home)?;

    // Guest boot script: env exports, then NIC + CA + workspace mount (the shared
    // preamble) and the agent's own server + vsock forward relay (the per-agent
    // script). Written into the creds share, exec'd by the static cmdline
    // bootstrap (see [`boot::boot_channel`]) — model names/env values may carry bytes
    // the kernel cmdline can't.
    let gw_q = shell_quote(&guest_workspace);
    let preamble = guest_launch_preamble(&ca.cert_pem, &gw_q);
    let events_q = shell_quote(&format!("{GUEST_HOME}/{}", profile.events_file));
    let script = (launch.build_script)(&preamble, &events_q);
    let exports = boot::env_exports(&guest_env)?;
    let (boot_share, boot_exec) = boot::boot_channel(
        &creds_share,
        "creds",
        GUEST_HOME,
        &format!("{exports}{script}"),
    )?;

    // host_sock prefix = the agent id (distinct per agent; one socket per VM).
    let host_sock = krun_cache_dir()?.join(format!(
        "{}-{}.sock",
        spec.id,
        uuid::Uuid::now_v7().simple()
    ));
    let _ = std::fs::remove_file(&host_sock);

    let vmspec = VmSpec {
        rootfs: rootfs.to_string_lossy().into_owned(),
        vcpus: 2,
        ram_mib: 2048,
        shares: vec![
            boot_share,
            Share {
                tag: "workspace".into(),
                host_path: clone.to_string_lossy().into_owned(),
            },
        ],
        exec: boot_exec,
        // Guest listens; the host dials `host_sock` once per HTTP request.
        vsock: Some(VsockAttach {
            port: FORWARD_PORT,
            host_sock: host_sock.to_string_lossy().into_owned(),
            listen: true,
        }),
        egress: Some(EgressSpec {
            // Vault-intercepted hosts (so a provider-backed model works + gets the
            // swap) UNION the agent's extra hosts (terminated + forwarded with an
            // empty swap), plus invoker --egress-allow. Everything else is fenced.
            allowlist: crate::vault::providers::intercepted_hosts()
                .into_iter()
                .map(str::to_string)
                .chain(launch.egress_extra)
                .chain(opts.egress_allow.iter().cloned())
                .collect(),
            log_path: std::env::var("PILLBOX_KRUN_EGRESS_LOG").ok(),
            ca_dir: Some(ca.dir.to_string_lossy().into_owned()),
            local_forward_port: launch.local_forward_port,
            refresh: None, // opencode server mode: non-vault, holds its own real key
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
    startup.mark("launch_prepare");

    // The session id is minted BEFORE the spawn so the VMM's self-destruct guard can
    // watch the record path it'll commit to (an abandoned bring-up must not orphan
    // this reparented server VM).
    let session_id = crate::session::Session::new_id();

    // Spawn the VM detached (it runs the server + relay, reparented to init).
    let exe = std::env::current_exe().context("locate the pillbox binary to re-exec as VMM")?;
    let mut cmd = Command::new(&exe);
    cmd.arg("__krun-vmm")
        .arg(&spec_path)
        .env_clear()
        .envs(boot::static_child_env())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    arm_commit_guard(&mut cmd, resolved, &session_id);
    vmm_own_process_group(&mut cmd);
    let mut child = cmd.spawn().context("spawn the libkrun VMM subprocess")?;
    // No swap pairs (non-vault): hand the child's MITM an empty set + EOF.
    if let Some(mut sin) = child.stdin.take() {
        use std::io::Write as _;
        let _ = sin.write_all(b"[]");
    }
    let pid = child.id() as i32;
    startup.mark("vmm_spawn");

    let http = http::LibkrunHttp::new(host_sock.clone());
    let prompt = opts.args.join(" ").trim().to_string();

    // Bring-up over the forward; capture the result so a failure tears the VM
    // down rather than leaking it + the clones.
    let built = (|| -> Result<crate::session::Session> {
        let agent_session_id = (launch.bringup)(&http)?;
        startup.mark("server_ready");
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
            placement: crate::session::Placement::Local,
            server: Some(crate::session::ServerSession {
                agent_session_id,
                model: launch.model.clone(),
                temperature: opts.temperature,
            }),
        };
        crate::session::write(resolved, &session)?;
        let startup_metrics = startup.finish("session_record");
        // --memory + server: this session is reparented, so its §0 log drains on `session ingest`,
        // AFTER bring-up returns — dispatch-time capture would race an empty log. Stash the brief in
        // the session dir for ingest to capture + record briefed-claim usage from. Best-effort.
        if opts.memory {
            if let Ok(dir) = crate::session::session_dir(resolved, &session.id) {
                crate::memory::stash_brief(
                    &dir,
                    &opts
                        .memory_project()
                        .unwrap_or_else(|| "default".to_string()),
                    &opts.memory_briefed,
                );
            }
        }
        // Reparented: nothing host-side supervises this agent, so spawn the detached §0 PRODUCER —
        // it tails the guest capture → durable log forever, keeping the log live for every consumer
        // (list/diagnose/subscribe + telemetry exporters) with no explicit drain. Killed by
        // kill_session. The libkrun analog of docker's always-on transcript tailer.
        spawn_session_tailer(resolved, &session, spec);
        crate::events::emit_session_event(
            resolved,
            crate::events::EventType::SessionStarted {
                parent_session_id: crate::events::parent_session_id_from_env(),
                startup: Some(startup_metrics),
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
            // child running, reparented; `kill_session` reaps that one by spec path.)
            // Read the child's own status FIRST: a SIGABRT'd-at-boot VMM (the swept
            // runtime-dep footgun) has already exited, and our `kill` would otherwise
            // clobber that status with SIGKILL and mask the actionable cause.
            let self_status = child.try_wait().ok().flatten();
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(&host_sock);
            let _ = std::fs::remove_file(&spec_path);
            let _ = std::fs::remove_dir_all(&creds_share);
            let _ = std::fs::remove_dir_all(&clone);
            if let Some(deps_err) = sigabrt_boot_error(self_status) {
                return Err(deps_err);
            }
            // `try_wait` can race the dying child, so a SIGABRT-at-boot may not be
            // caught above. A swept runtime dep is the likeliest cause of a boot
            // failure regardless — surface it instead of the opaque generic error.
            if let Err(reason) = runtime_deps_present() {
                return Err(PillboxError::runtime("run", reason).into());
            }
            return Err(e);
        }
    };

    // No auto-send: the server came up ready (wait_ready in bringup), so the first
    // prompt goes through `session send` — captured by a subscribed watch.
    crate::sandbox::opencode::print_started(
        &session,
        opts.json,
        (!prompt.is_empty()).then_some(prompt.as_str()),
    );
    Ok(())
}

/// opencode via `opencode serve` — fills a [`ServerLaunch`] for [`launch_server_vm`].
fn run_server(spec: &AgentSpec, opts: RunOpts, resolved: &Pillbox) -> Result<()> {
    use crate::sandbox::opencode;

    let model = opts
        .model
        .clone()
        .unwrap_or_else(|| opencode::DEFAULT_MODEL.to_string());
    let serve = opencode::serve_args()
        .iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    let launch = ServerLaunch {
        vault_refusal: "the vault (opencode is not vault-capable)",
        // The "standard" model-provider set (openrouter/deepseek/kimi/grok/gemini/
        // glm/…), terminated + forwarded empty-swap since opencode holds its key.
        egress_extra: egress::standard_egress_hosts()
            .iter()
            .map(|s| s.to_string())
            .collect(),
        // Node — CA trust is NODE_EXTRA_CA_CERTS (launch_base); needs no extra env.
        extra_env: vec![],
        // Opt-in: PILLBOX_LOCAL_MODEL_PORT lets the guest reach a host-run ollama.
        local_forward_port: std::env::var("PILLBOX_LOCAL_MODEL_PORT")
            .ok()
            .and_then(|s| s.parse::<u16>().ok()),
        model,
        // §0 producer: a co-located, persistent /event capture (raw SSE → the
        // shared/CoW home file, host-readable). curl holds one long-lived SSE
        // connection open, so we reopen-per-line (`printf >> file` open+write+
        // close) to force a virtio-fs flush each line — else the appends sit in
        // the guest page cache and the host sees 0 bytes. Outer loop reconnects.
        build_script: Box::new(move |preamble, events_q| {
            format!(
                "{preamble}; \
                 {serve} & \
                 ( while :; do curl -sN http://127.0.0.1:{port}/event 2>/dev/null \
                     | while IFS= read -r l; do printf '%s\\n' \"$l\" >> {events_q}; done; \
                   sleep 1; done ) & \
                 exec pillbox vsock-forward --vsock-port {FORWARD_PORT} --to-port {port}",
                port = opencode::SERVE_PORT,
            )
        }),
        bringup: Box::new(|http| {
            opencode::wait_ready(http)?;
            opencode::create_session(http)
        }),
    };
    launch_server_vm(spec, opts, resolved, launch)
}

/// codex via the in-guest `appserver-host` bridge (`codex app-server`) — fills a
/// [`ServerLaunch`] for [`launch_server_vm`]. **Non-vault v1**: the app-server's
/// model egress is the ChatGPT backend over WebSocket
/// (`wss://chatgpt.com/backend-api/codex/responses` for a ChatGPT-subscription
/// login), which the [`codex` vault provider](crate::vault::providers) doesn't yet
/// intercept — so `--vault` is rejected until that interception lands; the egress
/// fence still confines the VM to the OpenAI/ChatGPT hosts.
fn run_codex_serve(spec: &AgentSpec, opts: RunOpts, resolved: &Pillbox) -> Result<()> {
    use crate::sandbox::appserver_client as appserver;

    let model = opts.model.clone().unwrap_or_else(|| "codex-default".into());
    let launch = ServerLaunch {
        vault_refusal: "the vault (codex-serve v1 is non-vault: the app-server's model egress is \
                        the ChatGPT backend over WebSocket, which the codex provider doesn't yet intercept)",
        // codex's model egress hosts. A ChatGPT-subscription login (auth_mode=chatgpt
        // — the common case) dials chatgpt.com (`/backend-api/…` + the
        // `wss://…/backend-api/codex/responses` model socket) and refreshes tokens via
        // auth.openai.com; an API-key login uses api.openai.com instead. The codex +
        // openai providers already put these apexes in `intercepted_hosts`; list them
        // explicitly (plus `.chatgpt.com` for subdomains) so codex-serve's reachability
        // doesn't silently depend on an unrelated provider staying registered.
        // Terminated + forwarded empty-swap (non-vault: no credential substitution);
        // the egress fence still confines the VM to these hosts.
        egress_extra: vec![
            ".chatgpt.com".to_string(),
            "api.openai.com".to_string(),
            "auth.openai.com".to_string(),
        ],
        // codex is Rust (reqwest) — reads neither the system bundle nor
        // NODE_EXTRA_CA_CERTS, so it needs the vault CA via SSL_CERT_FILE or its model
        // WebSocket hits `UnknownCA`. MITM-only is correct: all codex egress is
        // MITM-terminated. (Why this is per-agent and not in launch_base: field doc.)
        extra_env: vec![("SSL_CERT_FILE".into(), GUEST_CA_PATH.into())],
        local_forward_port: None,
        model,
        // The appserver-host bridge owns `codex app-server`, captures notifications
        // to the events file itself, and serves HTTP on BRIDGE_PORT (no curl loop).
        build_script: Box::new(|preamble, events_q| {
            format!(
                "{preamble}; \
                 pillbox appserver-host --port {bridge} --events-file {events_q} & \
                 exec pillbox vsock-forward --vsock-port {FORWARD_PORT} --to-port {bridge}",
                bridge = appserver::BRIDGE_PORT,
            )
        }),
        bringup: Box::new(|http| {
            appserver::wait_ready(http)?;
            appserver::create_session(http)
        }),
    };
    launch_server_vm(spec, opts, resolved, launch)
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

/// Host-side path of a libkrun server session's event-capture file (inside the
/// CoW creds clone the guest mounts at its home). The §0 read drains this for
/// `watch`/`subscribe`/`ingest`. The filename comes from the agent's
/// [`ServerProfile`](crate::agents::ServerProfile) (opencode's SSE capture or the
/// codex bridge's NDJSON capture) — one source of truth, not a per-site `if`.
pub(crate) fn server_events_file(session: &crate::session::Session) -> Result<PathBuf> {
    let spec = crate::agents::lookup("session", &session.agent_id)?;
    let profile = spec
        .server
        .with_context(|| format!("`{}` is not a server-mode agent", session.agent_id))?;
    let handle = LibkrunHandle::decode(session)?;
    Ok(PathBuf::from(handle.creds).join(profile.events_file))
}

/// Spawn the detached §0 producer for a reparented server session: a re-exec'd
/// `pillbox __session-tailer` that tails the guest capture → durable log forever,
/// so the log stays live for every consumer with no explicit drain. Best-effort —
/// a failed spawn just falls back to drain-on-demand (`ingest`/`subscribe`).
fn spawn_session_tailer(resolved: &Pillbox, session: &crate::session::Session, spec: &AgentSpec) {
    let Some(profile) = spec.server.as_ref() else {
        return; // not a server agent — no capture to tail
    };
    let (Ok(dir), Ok(capture)) = (
        crate::session::session_dir(resolved, &session.id),
        server_events_file(session),
    ) else {
        return;
    };
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let _ = Command::new(exe)
        .arg("__session-tailer")
        .arg(&dir)
        .arg(&capture)
        .arg(profile.events_format.as_str())
        .arg(&session.id)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
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
    // Same disk-pressure guard as the run path: the grader VM materializes the
    // rootfs and mounts the workspace clone — refuse below the floor, not mid-boot.
    disk_preflight()?;
    let rootfs = materialize_rootfs(resolved)?;

    // Egress is opt-in: an empty allowlist keeps the bare/offline grader. When
    // hosts are declared, prepend the NIC-up + CA-trust preamble (so the guest's
    // TLS clients accept the MITM leaf) and route through the same fence as a run.
    // `_grade_ca` (bound here, not inside the egress branch) must outlive
    // `child.wait()` below — the VMM reads the per-run CA while the grader runs.
    let (egress, env_extra, script, _grade_ca) = if egress_allow.is_empty() {
        // Mount the workspace, cd in, run the grader with stderr merged to the
        // console. `&&` so a failed mount/cd surfaces; the grader's exit is last.
        let script = format!(
            "mkdir -p /grade && mount -t virtiofs grade /grade && cd /grade && {{ {cmd} ; }} 2>&1"
        );
        (None, Vec::new(), script, None)
    } else {
        // Supervised one-shot (`child.wait()` below), so a per-run ephemeral CA
        // is safe; the whole VaultCa is returned from this branch to outlive the grader.
        let ca = provision_vault_ca(resolved, CaLifetime::Ephemeral)?;
        let net = egress::guest_net_commands();
        // `exec 2>&1` so setup output (a dep-fetch failure) lands in feedback too;
        // `set -e` aborts loudly on a setup failure; `set +e` before the grader so
        // its own exit code (not set -e) is the verdict, captured as the script's.
        // Write the CA where the cert envs below point — no `update-ca-certificates`
        // merge needed (it's not even on the grader's PATH), since the fence is
        // all-MITM: every allowlisted host terminates at our CA-signed leaf, so
        // trusting that one cert is sufficient (the same single-cert trust agents
        // use via NODE_EXTRA_CA_CERTS). The PEM is shell-quoted straight in — it
        // rides the boot script (see [`boot::boot_channel`]), so no base64 detour.
        let script = format!(
            "exec 2>&1; set -e; {net}; \
             printf '%s' {ca_q} > {GUEST_CA_PATH}; \
             mkdir -p /grade; mount -t virtiofs grade /grade; cd /grade; \
             set +e; {cmd}",
            ca_q = shell_quote(&ca.cert_pem),
        );
        let egress = Some(EgressSpec {
            // Only the invoker-declared hosts resolve — the tightest fence (no
            // provider/standard-egress union; the grader reaches deps, nothing else).
            allowlist: egress_allow.to_vec(),
            log_path: std::env::var("PILLBOX_KRUN_EGRESS_LOG").ok(),
            ca_dir: Some(ca.dir.to_string_lossy().into_owned()),
            local_forward_port: None, // grader: tightest fence, no local forward
            refresh: None,            // grader holds no credentials — nothing to refresh
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
        (egress, env_extra, script, Some(ca))
    };

    // The grader command is user content (`--cmd`, rubric lines) — route it
    // through a boot-script share, off the printable-ASCII-only kernel cmdline
    // (see [`boot::boot_channel`]). A dedicated share, NOT the grade workspace:
    // a script in /grade would pollute the tree the verifier inspects. Held past
    // `child.wait()` below — the guest reads it at boot.
    let boot_dir = tempfile::Builder::new()
        .prefix("pillbox-grade-boot-")
        .tempdir()
        .context("create grader boot dir")?;
    let (boot_share, boot_exec) =
        boot::boot_channel(boot_dir.path(), "boot", "/run/pillbox-boot", &script)?;

    let vmspec = VmSpec {
        rootfs: rootfs.to_string_lossy().into_owned(),
        vcpus: 2,
        ram_mib: 2048,
        shares: vec![
            Share {
                tag: "grade".into(),
                host_path: workspace.to_string_lossy().into_owned(),
            },
            boot_share,
        ],
        exec: boot_exec,
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
    let mut cmd = Command::new(&exe);
    cmd.arg("__krun-vmm")
        .arg(spec_file.path())
        // No secrets reach the grader — only a minimal guest env for the toolchain
        // (plus CA-bundle pointers when egress is on; all non-secret). The base env
        // tracks the agent VM's PATH so a verifier resolves the same toolchain
        // (see [`boot::grader_child_env`]).
        .env_clear()
        .envs(boot::grader_child_env())
        .envs(env_extra.iter().copied())
        // Empty stdin: the VMM child reads swap pairs from stdin when egress is on
        // — null → EOF → empty swap (the MITM forwards untouched). No creds.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null()); // VMM diagnostics; the grade's output is the guest console (stdout)
                                              // No `setsid`: the grader is a supervised one-shot reaped via `child.wait`
                                              // below, not via `killpg` — same rationale as the foreground run.
    let mut child = cmd.spawn().context("spawn the grader VMM subprocess")?;
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

/// How long to let a buffered `Input` frame traverse socket → guest pty-host →
/// PTY before closing — mirrors the docker driver's settle. All in-VM once the
/// frame is written (libkrun bridges the unix socket straight to the guest's
/// listening pty-host, no exec cold-start), so it's short.
const SEND_SETTLE: Duration = Duration::from_millis(300);

/// Drive a detached PTY session: dial its persistent attach socket, write one
/// `Frame::Input` (bytes as if typed), drain the pty-host's on-connect `Snapshot`
/// then settle briefly so the guest forwards the input to the PTY before the
/// socket closes, then close. The `SendInput` half of the drive surface; the guest
/// pty-host (`run_vsock` listen-loop) accepts this transient connection and writes
/// the frame to the PTY. A dead/refused socket surfaces as a `connect` error.
///
/// The guest accept loop is serial (`serve_blocking`), so a `send` to an
/// *unattended* detached session (the IDE drive + §0-read keystone) lands at once.
/// A `send` *while a terminal is separately attached* contends with that pump and
/// may be DROPPED, not merely queued: the bytes sit in the socket buffer but the
/// serial loop won't accept this connection until the attached client leaves, and
/// the socket is torn down after the bounded settle below. Concurrent
/// drive-while-attached needs a threaded accept loop.
fn pty_send(sock: &str, bytes: &[u8]) -> Result<()> {
    use std::io::{Read as _, Write as _};
    let stream =
        UnixStream::connect(sock).with_context(|| format!("connect attach socket {sock}"))?;
    // Bound BOTH directions so a wedged guest — or the serial accept loop busy with
    // an attached terminal — can't hang `session send`: `write_all` into a full
    // socket buffer and the snapshot read each get a deadline. (Connecting to the
    // already-listening socket is local and doesn't block.)
    stream.set_write_timeout(Some(SEND_SETTLE)).ok();
    stream.set_read_timeout(Some(SEND_SETTLE)).ok();
    let mut stream = stream;
    crate::attach::driver::send_input(&mut stream, bytes).context("frame the session input")?;
    stream.flush().ok();
    // The pty-host sends an on-connect `Snapshot` to EVERY client (see
    // [`crate::attach::host`]). Receiving any bytes here is our proof the guest
    // accepted THIS connection and is now in its read loop — so it will pull the
    // `Input` we just queued. No bytes within the deadline means it never accepted
    // (the serial accept loop is busy with an attached terminal), so the input was
    // NOT delivered: fail rather than record a false "sent". A single bounded read
    // (not a re-armable decode loop) also caps the total time a drip-feeding guest
    // can hold us.
    let mut buf = [0u8; 1024];
    match stream.read(&mut buf) {
        Ok(n) if n > 0 => Ok(()),
        _ => Err(PillboxError::runtime(
            "session send",
            "the session didn't confirm receipt — the input was not delivered \
             (a terminal may be attached, or the guest is busy)",
        )
        .into()),
    }
}

/// Tear down a detached libkrun session: kill the VMM child (the VM + egress +
/// MITM go with it), scrub the persisted socket/spec/CoW clones, drop the record.
pub(crate) fn kill_session(resolved: &Pillbox, session: &crate::session::Session) -> Result<()> {
    // Stop the detached §0 producer (if any) BEFORE scrubbing the dir/log it writes to, so it
    // doesn't error mid-append or race the removal.
    let session_dir = crate::session::session_dir_path(resolved, &session.id);
    if let Some(pid) = crate::commands::session::tailer_pid(&session_dir) {
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    let handle = LibkrunHandle::decode(session)?;
    reap_vmm_by_spec(&handle.spec);
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
    // The reap above is scoped to THIS session's unique spec path — never a blind
    // host-wide `__krun-vmm` sweep (`~/.pillbox/krun/` is shared across pillboxes,
    // so a scan can't tell a dead local orphan from another pillbox's *live* VM;
    // the spec path is the attribution that makes the reap safe).
    println!("pillbox: ✓ session `{}` removed.", session.id);
    Ok(())
}

/// SIGKILL the microVM(s) backing this session, attributed by **spec path** rather
/// than by the pid recorded at spawn.
///
/// Why spec, not `handle.pid`: a server-mode / detached VMM reparents to launchd
/// and ends up as its OWN process-group leader (`pgid == its own pid`), distinct
/// from the `handle.pid` group recorded at spawn — so `killpg(handle.pid)` misses
/// it, and if `handle.pid` has exited or been recycled a pid-gated guard declines
/// and leaves a live VM. The per-session spec-file path is the durable, unique
/// attribution key (it's the `__krun-vmm <spec>` argv argument), so scanning for
/// the live VMM that carries it finds the process regardless of pid/pgid drift.
///
/// killpg, not kill: the VMM may have forked a subprocess into its group, and
/// signalling the whole group reaps the leader plus that fork; a pid-only SIGKILL
/// strands it. Each distinct group is killed once.
///
/// Attribution-safe: the spec path is unique to THIS session (a `.keep()`'d
/// tempfile), so it can't match another pillbox's VM — never a blind host-wide
/// sweep of `~/.pillbox/krun/` (shared across pillboxes). Nothing carrying the
/// spec ⇒ the VM is already down (a clean no-op); if `ps` itself can't run,
/// [`vmm_groups_for_spec`] warns rather than letting the leak go silent.
fn reap_vmm_by_spec(spec: &str) {
    let mut killed: Vec<i32> = Vec::new();
    for (_pid, pgid) in vmm_groups_for_spec(spec) {
        // pgid must be a real, positive group id: `killpg(0)` would signal OUR OWN
        // process group (the pillbox CLI + whatever launched it) and a negative
        // arg is invalid — never let a malformed `ps` line turn a reap into
        // self-slaughter. A real VMM's pgid is its own pid (setsid), always > 1.
        if pgid > 1 && !killed.contains(&pgid) {
            killed.push(pgid);
            // Result ignored: a group that exited between the scan and here (ESRCH)
            // is already in the goal state; reaping is best-effort, not a gate.
            unsafe { libc::killpg(pgid, libc::SIGKILL) };
        }
    }
}

/// `(pid, pgid)` of every live `__krun-vmm` whose argv carries `spec`. One
/// host-wide `ps` read (`-ax` so a reparented, controlling-terminal-less VMM is
/// still listed; `-ww` so the trailing `<spec>` arg isn't width-truncated — it
/// sits at the END of `__krun-vmm <spec>`, exactly where a clipped line would drop
/// it). Filtering on both the subcommand AND the unique spec path is the
/// attribution; empty (VM down, or `ps` unavailable) ⇒ no group to reap.
fn vmm_groups_for_spec(spec: &str) -> Vec<(i32, i32)> {
    match Command::new("ps")
        .args(["-axww", "-o", "pid=,pgid=,command="])
        .output()
    {
        Ok(out) if out.status.success() => {
            parse_vmm_groups(&String::from_utf8_lossy(&out.stdout), spec)
        }
        // `ps` couldn't run (missing/un-spawnable) or exited non-zero — we could
        // NOT verify whether a VMM lingers, and `kill_session` is about to delete
        // the record. Warn loudly rather than silently leak a credential-holding
        // microVM; this is the user's only signal to reap it by hand. (Empty spec
        // has nothing to attribute, so stays quiet.)
        result => {
            if !spec.is_empty() {
                let why = result
                    .err()
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "ps exited non-zero".to_string());
                eprintln!(
                    "pillbox: warning: couldn't scan for this session's microVM ({why}); \
                     if a `__krun-vmm` lingers, reap it with `pkill -9 -f {spec}`."
                );
            }
            Vec::new()
        }
    }
}

/// Pure parser for [`vmm_groups_for_spec`]: keep `ps` lines whose command carries
/// both `__krun-vmm` and `spec`, yielding their `(pid, pgid)`. An empty `spec`
/// matches nothing — the guard against a blank attribution key degenerating into a
/// match-all (blind host-wide sweep).
fn parse_vmm_groups(ps_stdout: &str, spec: &str) -> Vec<(i32, i32)> {
    if spec.is_empty() {
        return Vec::new();
    }
    ps_stdout
        .lines()
        .filter(|l| l.contains("__krun-vmm") && l.contains(spec))
        .filter_map(|l| {
            let mut fields = l.split_whitespace();
            let pid = fields.next()?.parse::<i32>().ok()?;
            let pgid = fields.next()?.parse::<i32>().ok()?;
            Some((pid, pgid))
        })
        .collect()
}

/// Spawn the VMM child as its own session+group leader, so the whole group (this
/// `__krun-vmm` process plus any VMM subprocess `krun_start_enter` forks) can be
/// reaped together by `killpg` on teardown — a pid-only kill strands the fork.
/// `setsid` also detaches the child from the launching CLI's controlling terminal,
/// which is correct on every path: the guest console rides the vsock attach
/// channel (PTY) or the agent's HTTP API (server), never this terminal.
/// Put a DETACHED VMM child in its own session/process group (`setsid`) so the
/// VMM is a group leader (`pgid == its own pid`) and `kill_session`'s spec-path
/// reap (`reap_vmm_by_spec` → `killpg(pgid)`) takes down the whole group on
/// `session rm`, even after the VMM reparents to launchd.
/// ONLY the detached spawns (`run_detached`, `launch_server_vm`) use this — the
/// synchronous foreground + grader paths reap via `child.wait` and must NOT
/// `setsid`: their VMM would otherwise survive in its own session if the CLI is
/// interrupted before teardown (e.g. Ctrl-C in the boot window), orphaning the VM.
fn vmm_own_process_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    // SAFETY: `pre_exec` runs in the forked child before `exec`; the closure must be
    // async-signal-safe and touch no shared state. `setsid()` is a single bare
    // syscall meeting both; `io::Error::last_os_error` reads `errno` (no alloc).
    // A failed `setsid` would leave the child in the CLI's own group — then a
    // `killpg(handle.pid)` would both miss the VMM and risk signalling the CLI's
    // group — so fail the spawn loudly instead of proceeding into that hazard.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Refuse a launch when the krun cache filesystem is below the headroom floor —
/// the disk-pressure guard. A boot that runs out of space mid-allocation stalls a
/// half-built VM rather than failing cleanly; fail fast and loud instead.
/// `disk_headroom` returns 0 (== "unknown") on stat failure, which we treat as not
/// below the floor: an unstattable cache dir is a different problem the materialize
/// step surfaces, not a reason to block an otherwise-fine host here.
fn disk_preflight() -> Result<()> {
    let cache = krun_cache_dir()?;
    headroom_check(disk_headroom(&cache), MIN_HEADROOM_BYTES, &cache)
}

/// The pure decision behind [`disk_preflight`] (split out so it's unit-testable
/// without a real low-disk fs): `Err` when `free` is below `floor`, naming the
/// shortfall and the fix. `free == 0` is "unknown" (a stat failure), not below the
/// floor — see [`disk_preflight`].
fn headroom_check(free: u64, floor: u64, cache: &Path) -> Result<()> {
    if free > 0 && free < floor {
        return Err(PillboxError::runtime(
            "run",
            format!(
                "low disk headroom on the krun cache ({}): {} free, need {} — a microVM boot \
                 would stall half-allocated",
                cache.display(),
                human_bytes(free),
                human_bytes(floor),
            ),
        )
        .with_next("free space on that filesystem, then retry")
        .into());
    }
    Ok(())
}

/// Map a VMM child that died by `SIGABRT` at boot to an actionable error, or `None`
/// for any other exit. `SIGABRT` is the loader aborting the VMM — almost always
/// libkrun's *undeclared* runtime dylibs (`libepoxy`, `MoltenVK`) swept by a `brew
/// cleanup`. Surface the deps fix when the probe confirms it; otherwise a generic
/// "VMM aborted at boot" naming the deps as the likely cause (the probe can't see
/// a dep that's present-but-broken). Does not swallow — the run already failed;
/// this only makes the *why* legible instead of an opaque signal death.
fn sigabrt_boot_error(status: Option<std::process::ExitStatus>) -> Option<anyhow::Error> {
    use std::os::unix::process::ExitStatusExt as _;
    if status?.signal() != Some(libc::SIGABRT) {
        return None;
    }
    let err = match runtime_deps_present() {
        Err(reason) => {
            PillboxError::runtime("run", format!("the libkrun VMM aborted at boot ({reason})"))
                .with_next("brew install libepoxy molten-vk")
        }
        Ok(()) => PillboxError::runtime(
            "run",
            "the libkrun VMM aborted at boot (SIGABRT) — the likely cause is missing runtime \
             deps swept by `brew cleanup`",
        )
        .with_next("brew install libepoxy molten-vk"),
    };
    Some(err.into())
}

/// Human-readable bytes for an operator-facing error (GiB/MiB, one decimal).
fn human_bytes(n: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    if n >= GIB {
        format!("{:.1} GiB", n as f64 / GIB as f64)
    } else {
        format!("{:.1} MiB", n as f64 / MIB as f64)
    }
}

/// Backstop deadline for the VMM self-destruct [`CommitGuard`]: if a detached
/// launch neither commits its session record nor sees its launcher die within
/// this window, the VMM self-destructs anyway (covers a recycled/hung owner pid).
/// Generous — a real launch commits within seconds of bring-up; the owner-death
/// path handles the common watchdog-`kill -9` case promptly, well before here.
const COMMIT_DEADLINE_SECS: u64 = 600;

/// Arm a detached VMM's self-destruct guard by setting the `PILLBOX_COMMIT_*` env on
/// its Command (read + cleared by [`CommitGuard::from_env`](super::CommitGuard) in the
/// child, so it never reaches the guest). ONLY the reparenting spawns (`run_detached`,
/// `launch_server_vm`) call this — foreground/grader VMMs are reaped via `child.wait`
/// and need no guard. The owner pid is THIS launching CLI; the record path is where the
/// commit lands.
fn arm_commit_guard(cmd: &mut Command, resolved: &Pillbox, session_id: &str) {
    cmd.env("PILLBOX_COMMIT_OWNER_PID", std::process::id().to_string())
        .env(
            "PILLBOX_COMMIT_RECORD",
            crate::session::record_path(resolved, session_id),
        )
        .env("PILLBOX_COMMIT_DEADLINE", COMMIT_DEADLINE_SECS.to_string());
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

/// The libkrun [`LiveSession`](crate::sandbox::LiveSession) — a detached/server
/// microVM the command layer drives and reads without branching on the backend.
/// A thin adapter over the existing free fns (the proven transport), so the plane
/// gains no new libkrun behavior, only one polymorphic surface. Holds a cloned
/// [`Session`](crate::session::Session); the [`LibkrunHandle`] is decoded on
/// demand from its `sandbox_id`.
pub(crate) struct LibkrunLiveSession {
    session: crate::session::Session,
}

impl crate::sandbox::LiveSession for LibkrunLiveSession {
    fn caps(&self) -> Caps {
        LibkrunBackend.capabilities()
    }

    fn send(&self, bytes: &[u8]) -> Result<()> {
        // A server agent's prompt goes over its HTTP API; a PTY agent's is raw
        // keystrokes. Both flow through this one `send` so the command layer never
        // branches on integration.
        if self.session.integration() == Integration::Server {
            return crate::sandbox::drive_server_prompt(&self.session, &*self.http()?, bytes);
        }
        pty_send(&LibkrunHandle::decode(&self.session)?.sock, bytes)
    }

    fn attach(&self, resolved: &Pillbox) -> Result<()> {
        reattach(resolved, &self.session)
    }

    fn spawn_log_tailer(
        &self,
        resolved: &Pillbox,
    ) -> Result<Option<crate::events::transcripts::TailerHandle>> {
        // A detached PTY session has no reparented §0 producer, so each reader spawns
        // its own transcript tailer — two concurrent readers (`watch`+`subscribe`)
        // would double-write the log, and a never-watched session captures nothing.
        // The single-reader IDE drive+read keystone is unaffected.
        //
        // A producer already draining the capture into the log keeps it current; a
        // second drainer would double-write every event (fresh seqs).
        if crate::commands::session::detached_tailer_alive(resolved, &self.session) {
            return Ok(None);
        }
        let log = crate::events::log::SessionLog::open(resolved, &self.session.id)?;
        let tailer = if self.session.integration() == Integration::Server {
            // A server agent persists its event capture to a host file in the CoW
            // creds clone; tail that (SSE/NDJSON → the durable log).
            Some(self.server_file_tailer(log)?)
        } else {
            // A PTY agent's transcript lands in the host-readable creds clone
            // (`handle.creds`, the agent home the guest mounts); tail it with the
            // same producer the foreground PTY run uses, just pointed at the
            // persisted clone instead of the live auth home.
            let handle = LibkrunHandle::decode(&self.session)?;
            let tailer = crate::events::transcripts::spawn_attach_tailer(
                log,
                Path::new(&handle.creds),
                &self.session.agent_id,
                &self.session.guest_cwd,
                &self.session.id,
            );
            // `None` means this agent has no transcript parser, so there's no live
            // tail even though the backend advertises one — surface it rather than
            // silently serving only the existing log.
            if tailer.is_none() {
                eprintln!(
                    "pillbox: note: `{}` has no transcript parser; serving the existing log without a live tail",
                    self.session.agent_id
                );
            }
            tailer
        };
        Ok(tailer)
    }

    fn http(&self) -> Result<Box<dyn crate::sandbox::http::SandboxHttp>> {
        // Only a server-mode agent runs an in-guest HTTP server to talk to; a PTY
        // agent has none, so the verb is unsupported rather than handing back a
        // handle to a port nothing listens on.
        if self.session.integration() != Integration::Server {
            return Err(self.caps().unsupported("http"));
        }
        opencode_http(&self.session)
    }

    fn workspace_path(&self) -> Result<PathBuf> {
        workspace_path(&self.session)
    }

    fn ingest(&self, resolved: &Pillbox) -> Result<usize> {
        // Post-hoc trajectory drain: the reparented guest's persisted capture is read
        // to EOF into the durable log. Only a server agent produces that capture.
        if self.session.integration() != Integration::Server {
            return Err(PillboxError::usage(
                "session ingest",
                format!(
                    "ingest applies to server-mode sessions; `{}` is not one",
                    self.session.agent_id
                ),
            )
            .into());
        }
        // If the detached §0 producer is still draining the capture live, the log is
        // already current — draining again would duplicate every event.
        if crate::commands::session::detached_tailer_alive(resolved, &self.session) {
            return Ok(0);
        }
        // A plain `File` (not the follow reader) — the drain returns at EOF rather
        // than tailing, and the never-set stop flag is meaningless on this path.
        let path = server_events_file(&self.session)?;
        let format = self.events_format()?;
        let file = std::fs::File::open(&path).map_err(|e| {
            PillboxError::runtime(
                "session ingest",
                format!("open events file {}: {e}", path.display()),
            )
            .with_next("the session may not have produced any §0 events yet")
        })?;
        let mut log = crate::events::log::SessionLog::open(resolved, &self.session.id)?;
        let stop = std::sync::atomic::AtomicBool::new(false);
        crate::events::drain_server_capture(format, file, &self.session.id, &mut log, &stop)
    }

    fn kill(&self, resolved: &Pillbox) -> Result<()> {
        kill_session(resolved, &self.session)
    }
}

impl LibkrunLiveSession {
    pub(crate) fn new(session: crate::session::Session) -> Self {
        Self { session }
    }

    /// The server agent's capture wire format, resolved from its
    /// [`ServerProfile`](crate::agents::ServerProfile) — the drain-dispatch axis
    /// (SSE vs NDJSON), shared by [`event_source`](Self::event_source) and
    /// [`ingest`](Self::ingest) so the format→drain mapping lives in one place.
    fn events_format(&self) -> Result<crate::events::EventsFormat> {
        let spec = crate::agents::lookup("session", &self.session.agent_id)?;
        spec.server.map(|p| p.events_format).ok_or_else(|| {
            PillboxError::usage(
                "session",
                format!("`{}` is not a server-mode agent", self.session.agent_id),
            )
            .into()
        })
    }

    /// Tail a server session's persistent event-capture file (replay + follow via
    /// [`FollowReader`](crate::events::opencode::FollowReader)) into the log on a
    /// background thread. The gateway-free, complete-capture source: a late watcher
    /// still gets the whole history because the file persisted. The returned
    /// [`TailerHandle`](crate::events::transcripts::TailerHandle) flips the shared
    /// stop flag to end the follow.
    fn server_file_tailer(
        &self,
        log: crate::events::log::SessionLog,
    ) -> Result<crate::events::transcripts::TailerHandle> {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let path = server_events_file(&self.session)?;
        let format = self.events_format()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let sid = self.session.id.clone();
        let join = std::thread::spawn(move || {
            let mut log = log;
            let reader = crate::events::opencode::FollowReader::new(path, Arc::clone(&stop_thread));
            if let Err(e) =
                crate::events::drain_server_capture(format, reader, &sid, &mut log, &stop_thread)
            {
                eprintln!("pillbox: warning: server events drain stopped: {e:#}");
            }
        });
        Ok(crate::events::transcripts::TailerHandle::from_flag(
            stop, join,
        ))
    }
}

/// Content-addressed base cache root: `<krun-cache>/base`. Holds one
/// materialized snapshot per handle (the once-only materialization + locking
/// lives in [`crate::workspace::base_cache`]). Cache entries are immutable (a
/// snapshot handle IS its content), so they never go stale.
///
/// NOTE: entries are not yet evicted — the cache grows by one workspace tree per
/// unique snapshot ever forked from. Eviction (LRU/TTL, or tie-in to `session
/// prune`) is a follow-up, mirroring rustic's deferred `prune`.
fn base_cache_root() -> Result<PathBuf> {
    Ok(krun_cache_dir()?.join("base"))
}

/// Materialize `handle`'s immutable snapshot into the shared base cache exactly
/// once and return the cached dir to CoW-clone the guest workspace from. So `k`
/// workers forked off one bookmark (dispatch) pay a **single** rustic restore
/// instead of `k`, and the caller's cwd is never touched.
fn warm_base(resolved: &Pillbox, handle: &SnapshotHandle) -> Result<PathBuf> {
    let root = base_cache_root()?;
    crate::workspace::base_cache::materialize_once(&root, handle.as_str(), |dst| {
        resolved.workspace()?.pull(dst, Some(handle))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::LiveSession;
    use crate::session::{Session, BACKEND_LIBKRUN};

    /// A libkrun-backed [`Session`] over a PTY agent (claude), carrying a decodable
    /// [`LibkrunHandle`] whose paths point nowhere real — enough for `send`/
    /// `event_source` to reach past the capability check into the transport without
    /// a live VM. The creds path is absent on disk; the transcript tailer's
    /// discovery tolerates a not-yet-created dir.
    fn libkrun_pty_session() -> Session {
        let mut s = Session::test_fixture();
        s.backend = BACKEND_LIBKRUN.to_string();
        s.sandbox_id = serde_json::to_string(&LibkrunHandle {
            sock: "/nonexistent/pillbox-test-attach.sock".to_string(),
            pid: 0,
            creds: "/nonexistent/pillbox-test-creds".to_string(),
            workspace: "/nonexistent/pillbox-test-workspace".to_string(),
            spec: "/nonexistent/pillbox-test-spec.json".to_string(),
        })
        .unwrap();
        s
    }

    #[test]
    fn pty_drive_and_live_tail_caps_are_on() {
        let caps = LibkrunBackend.capabilities();
        assert!(
            caps.pty_drive,
            "libkrun drives the guest pty-host over the attach socket — pty_drive must be true"
        );
        assert!(
            caps.live_pty_tail,
            "libkrun tails the PTY transcript from the creds clone — live_pty_tail must be true"
        );
        assert!(
            caps.in_sandbox_grading,
            "libkrun is the microVM family — in_sandbox_grading must be true"
        );
    }

    #[test]
    fn pty_send_is_no_longer_capability_unsupported() {
        // The PTY `send` now reaches the transport: with an undecodable handle it
        // fails at the handle decode (or the socket connect), NOT at the old
        // `caps().unsupported("send")` short-circuit. (Byte delivery to a live PTY
        // is the live smoke's job — this only proves the verb is wired.)
        let live = LibkrunLiveSession::new(libkrun_pty_session());
        let err = live.send(b"hi").unwrap_err().to_string();
        assert!(
            !err.contains("isn't supported on this backend"),
            "PTY send must no longer be capability-unsupported, got: {err}"
        );
    }

    #[test]
    fn pty_spawn_log_tailer_is_no_longer_capability_unsupported() {
        // PTY `spawn_log_tailer` now spawns the creds-clone transcript tailer instead
        // of rejecting the verb. Isolated $HOME so the log it opens (and the discovery
        // tailer it spawns) land in a tempdir, not the real global pillbox. The
        // returned tailer handle drops at scope end, stopping the thread.
        crate::test_util::with_isolated_home("libkrun-pty-spawn-log-tailer", || {
            let live = LibkrunLiveSession::new(libkrun_pty_session());
            assert!(
                live.spawn_log_tailer(&crate::pillbox::global()).is_ok(),
                "a PTY session's spawn_log_tailer must now be supported"
            );
        });
    }

    #[test]
    fn headroom_check_below_floor_is_a_loud_error() {
        let cache = Path::new("/fake/krun/cache");
        // 1 GiB free under a 2 GiB floor → refuse, naming the shortfall + the fix.
        let err = headroom_check(1024 * 1024 * 1024, MIN_HEADROOM_BYTES, cache)
            .expect_err("below the floor must error");
        let msg = err.to_string();
        assert!(msg.contains("low disk headroom"), "names the cause: {msg}");
        assert!(msg.contains("free space"), "carries the Next fix: {msg}");
    }

    #[test]
    fn headroom_check_at_or_above_floor_passes() {
        let cache = Path::new("/fake/krun/cache");
        assert!(headroom_check(MIN_HEADROOM_BYTES, MIN_HEADROOM_BYTES, cache).is_ok());
        assert!(headroom_check(MIN_HEADROOM_BYTES + 1, MIN_HEADROOM_BYTES, cache).is_ok());
    }

    #[test]
    fn headroom_check_unknown_zero_does_not_block() {
        // 0 == "stat failed / unknown" — not a reason to refuse an otherwise-fine host.
        let cache = Path::new("/fake/krun/cache");
        assert!(headroom_check(0, MIN_HEADROOM_BYTES, cache).is_ok());
    }

    #[test]
    fn sigabrt_status_maps_to_deps_error() {
        use std::os::unix::process::ExitStatusExt as _;
        // A wait-status whose low byte is the signal number → `signal() == SIGABRT`.
        let status = std::process::ExitStatus::from_raw(libc::SIGABRT);
        let err = sigabrt_boot_error(Some(status)).expect("SIGABRT must map to an error");
        let msg = err.to_string();
        assert!(
            msg.contains("aborted at boot"),
            "names the abort cause: {msg}"
        );
    }

    #[test]
    fn non_sigabrt_status_is_not_mapped() {
        use std::os::unix::process::ExitStatusExt as _;
        // A clean exit and a different signal both pass through unmapped — only the
        // boot-abort footgun gets the deps hint.
        assert!(sigabrt_boot_error(Some(std::process::ExitStatus::from_raw(0))).is_none());
        assert!(
            sigabrt_boot_error(Some(std::process::ExitStatus::from_raw(libc::SIGKILL))).is_none()
        );
        assert!(sigabrt_boot_error(None).is_none());
    }

    #[test]
    fn human_bytes_formats_gib_and_mib() {
        assert_eq!(human_bytes(2 * 1024 * 1024 * 1024), "2.0 GiB");
        assert_eq!(human_bytes(512 * 1024 * 1024), "512.0 MiB");
    }

    const SPEC: &str = "/var/folders/aa/pillbox-krun-spec-ours.json";

    /// A `ps -axww -o pid=,pgid=,command=` line (leading-padded pid, pgid, argv).
    fn ps_line(pid: i32, pgid: i32, argv: &str) -> String {
        format!("{pid:>6} {pgid:>6} {argv}")
    }

    #[test]
    fn parse_finds_reparented_vmm_by_spec_not_pid() {
        // The leak case: a reparented VMM is its OWN group leader (pgid == its own
        // pid, 9001), NOT the pid recorded at spawn — yet it still carries our spec
        // in argv, so the spec scan finds it (where `killpg(handle.pid)` would miss).
        let out = [
            ps_line(800, 800, "/Users/x/pillbox session rm s-123"),
            ps_line(
                9001,
                9001,
                &format!("/Users/x/target/debug/pillbox __krun-vmm {SPEC}"),
            ),
            ps_line(42, 1, "/sbin/launchd"),
        ]
        .join("\n");
        assert_eq!(parse_vmm_groups(&out, SPEC), vec![(9001, 9001)]);
    }

    #[test]
    fn parse_excludes_other_pillboxes_vmm() {
        // Another pillbox's live VMM carries a DIFFERENT spec path — the attribution
        // wall: we must never match (and later killpg) it.
        let theirs = "/var/folders/zz/pillbox-krun-spec-theirs.json";
        let out = [
            ps_line(9001, 9001, &format!("/Users/x/pillbox __krun-vmm {SPEC}")),
            ps_line(7777, 7777, &format!("/Users/y/pillbox __krun-vmm {theirs}")),
        ]
        .join("\n");
        assert_eq!(parse_vmm_groups(&out, SPEC), vec![(9001, 9001)]);
    }

    #[test]
    fn parse_empty_spec_matches_nothing() {
        // A blank attribution key must NOT degenerate into a match-all sweep over
        // every `__krun-vmm` on the host.
        let out = ps_line(9001, 9001, "/Users/x/pillbox __krun-vmm /a/b.json");
        assert!(parse_vmm_groups(&out, "").is_empty());
    }

    #[test]
    fn parse_requires_the_krun_vmm_subcommand() {
        // A process that merely mentions our spec path in argv (e.g. an editor or a
        // grep) but isn't a `__krun-vmm` is not a VM to reap.
        let out = ps_line(123, 123, &format!("/usr/bin/vim {SPEC}"));
        assert!(parse_vmm_groups(&out, SPEC).is_empty());
    }

    #[test]
    fn parse_returns_each_matching_pid_for_group_dedup() {
        // A forked subprocess sharing the leader's group: both pids surface here;
        // `reap_vmm_by_spec` dedups to one `killpg` per group.
        let out = [
            ps_line(9001, 9001, &format!("pillbox __krun-vmm {SPEC}")),
            ps_line(9002, 9001, &format!("pillbox __krun-vmm {SPEC}")),
        ]
        .join("\n");
        assert_eq!(
            parse_vmm_groups(&out, SPEC),
            vec![(9001, 9001), (9002, 9001)]
        );
    }
}
