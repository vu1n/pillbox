//! `pillbox sandbox spawn|exec|agent|destroy` on the libkrun microVM.
//!
//! A sandbox is a detached VM whose guest runs `pillbox exec-host` on a listening
//! vsock port; each `exec` dials the libkrun-bound host socket and speaks
//! [`exec_channel`](crate::sandbox::exec_channel). Its record stores the
//! [`LibkrunHandle`] JSON in `backend_ref`, and destroy is [`teardown_handle`] plus
//! deleting the record.
//!
//! The VM takes the run path's [`launch_base`] prologue: a private rootfs clone, a
//! CoW secret-scrubbed workspace clone (never the host directory itself), the
//! egress fence and the vault CA. With `--agent`, it takes the structured run's
//! creds ([`agent_launch_creds`]): a vault-capable agent's env-fork puts stubbed
//! creds in the guest and the reals only on the VMM child's stdin.
//!
//! Wired in: `LibkrunBackend::spawn_sandbox` calls [`spawn`], and
//! `sandbox::sandbox_target` calls [`target`] for a libkrun record.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::agents::{AgentSpec, RunOpts, GUEST_HOME};
use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::sandbox::exec_channel;
use crate::sandbox::{SandboxSpawn, SandboxTarget};
use crate::sandboxes::{self, Sandbox};

use super::session::{
    abandon_launch, agent_launch_creds, authenticated_home, ensure_env_fork_sealed,
    guest_launch_preamble, launch_base, spawn_vmm, standard_allowlist, teardown_handle,
    write_vm_spec, CaLifetime, LaunchBase, LaunchCreds, LibkrunHandle, VaultCa, VmmTether,
};
use super::{boot, krun_cache_dir, shell_quote, EgressSpec, Share, SwapPair, VmSpec, VsockAttach};

/// Guest vsock port `exec-host` listens on. Distinct from the attach (1024) and
/// forward (1025) ports, and ≥ 1024 so a non-root `exec-host` may bind it.
pub(super) const EXEC_PORT: u32 = 1026;

/// The uid:gid an agent sandbox's commands run as (the docker `--user 1000:1000`).
const AGENT_UID: u32 = 1000;

/// How long spawn waits for the guest's exec channel to answer: VM boot plus the
/// agent sandbox's ownership pass over the workspace clone.
const EXEC_READY_TIMEOUT: Duration = Duration::from_secs(120);

/// One readiness probe's socket I/O bound, so a half-open bridge can't hang spawn.
const PROBE_IO_TIMEOUT: Duration = Duration::from_secs(10);

/// Boot a sandbox VM for `req`, wait until its exec channel answers, and write
/// its record. Any failure after the VMM spawn tears the VM and its files down.
pub(super) fn spawn(resolved: &Pillbox, req: &SandboxSpawn) -> Result<Sandbox> {
    let agent = req
        .agent
        .as_deref()
        .map(|a| crate::agents::lookup("sandbox spawn", a))
        .transpose()?;
    let id = Sandbox::new_id();
    let prepared = prepare(resolved, agent, &sandbox_run_opts(&req.workspace))?;
    launch(resolved, &id, req, agent, prepared)
}

/// The live target for a libkrun sandbox record.
pub(crate) fn target(record: &Sandbox) -> Result<Box<dyn SandboxTarget>> {
    Ok(Box::new(LibkrunSandbox {
        id: record.id.clone(),
        handle: LibkrunHandle::parse(&record.backend_ref)
            .with_context(|| format!("sandbox `{}`", record.id))?,
        guest_workspace: record.guest_workspace.clone(),
    }))
}

/// The run options a sandbox launch feeds the shared prologue: only the workspace.
/// No `--with`, bundles, bookmark or extra egress — a sandbox takes none of them.
fn sandbox_run_opts(workspace: &Path) -> RunOpts {
    RunOpts {
        workspace: Some(workspace.to_path_buf()),
        name: None,
        mounts: Vec::new(),
        withs: Vec::new(),
        env_bundles: Vec::new(),
        env_files: Vec::new(),
        vault: false,
        memory: false,
        memory_briefed: Vec::new(),
        mcps: Vec::new(),
        mcp_tokens: Vec::new(),
        args: Vec::new(),
        detach: true,
        label: None,
        json: false,
        ttl_seconds: None,
        from_bookmark: None,
        model: None,
        profile: None,
        reasoning_effort: None,
        temperature: None,
        egress_allow: Vec::new(),
        egress_deny: false,
    }
}

/// Host directories created for a launch that is not yet owned by a running VM;
/// removed on drop unless [`release`](Self::release)d to the VM's handle.
#[derive(Default)]
struct Scratch(Vec<PathBuf>);

impl Scratch {
    fn keep(&mut self, dir: &Path) {
        self.0.push(dir.to_path_buf());
    }
    fn release(mut self) {
        self.0.clear();
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        for dir in &self.0 {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Everything [`launch`] needs, built before any VMM exists.
struct Prepared {
    rootfs: super::rootfs::PrivateRootfs,
    spec_path: PathBuf,
    sock: PathBuf,
    creds_share: PathBuf,
    workspace_clone: PathBuf,
    guest_workspace: String,
    swap_pairs: Vec<SwapPair>,
    scratch: Scratch,
    _ca: VaultCa,
}

/// Build the launch: the shared prologue, the creds share, the boot script
/// (sealed against a leaked real credential), and the persisted VmSpec.
fn prepare(resolved: &Pillbox, agent: Option<&AgentSpec>, opts: &RunOpts) -> Result<Prepared> {
    let mut scratch = Scratch::default();
    let home = agent
        .map(|spec| authenticated_home(spec, opts, resolved, "sandbox spawn"))
        .transpose()?;
    // The sandbox VM reparents past this CLI, so its CA must persist.
    let LaunchBase {
        rootfs,
        workspace_clone,
        guest_workspace,
        guest_env,
        ca,
        with_vault,
    } = launch_base(agent, opts, resolved, CaLifetime::Persistent)?;
    scratch.keep(&workspace_clone);
    let LaunchCreds {
        creds_share,
        swap_pairs,
        allowlist,
        refresh,
    } = match agent.zip(home.as_deref()) {
        Some((spec, home)) => agent_launch_creds(spec, home, opts, with_vault)?,
        None => LaunchCreds {
            creds_share: empty_home()?,
            swap_pairs: Vec::new(),
            allowlist: standard_allowlist(opts),
            refresh: None,
        },
    };
    scratch.keep(&creds_share);

    let preamble = guest_launch_preamble(&ca.cert_pem, &guest_workspace);
    let exports = boot::env_exports(&guest_env)?;
    let script = boot_script(&exports, &preamble, &guest_workspace, agent.is_some());
    ensure_env_fork_sealed(&swap_pairs, &guest_env, &script)?;
    let (boot_share, boot_exec) = boot::boot_channel(
        &creds_share,
        "creds",
        GUEST_HOME,
        boot::MountedShareOwnership::GuestRootClone,
        &script,
    )?;

    let sock = krun_cache_dir()?.join(format!("exec-{}.sock", uuid::Uuid::now_v7().simple()));
    let _ = std::fs::remove_file(&sock);
    let vmspec = vm_spec(
        &rootfs,
        boot_share,
        boot_exec,
        &workspace_clone,
        &sock,
        EgressSpec {
            allowlist,
            log_path: super::egress_log_path(),
            ca_dir: Some(ca.dir.to_string_lossy().into_owned()),
            local_forward_port: None,
            refresh,
            // Only `auth login` opens the token endpoints; a sandbox never does.
            login: false,
        },
    );
    let spec_file = write_vm_spec(&vmspec)?;
    // The reparented child reads the spec after this CLI returns: persist it.
    let (_, spec_path) = spec_file.keep().context("persist VMM spec")?;

    Ok(Prepared {
        rootfs,
        spec_path,
        sock,
        creds_share,
        workspace_clone,
        guest_workspace,
        swap_pairs,
        scratch,
        _ca: ca,
    })
}

/// A fresh, empty home share for a sandbox with no agent.
fn empty_home() -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = krun_cache_dir()?
        .join("creds")
        .join(uuid::Uuid::now_v7().simple().to_string());
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod {} 0700", dir.display()))?;
    Ok(dir)
}

/// The guest boot script: env exports, the shared preamble (NIC, CA, workspace
/// mount), then `exec-host` listening on [`EXEC_PORT`]. An agent sandbox first
/// hands the home and workspace to uid 1000 and runs `exec-host` (so every exec'd
/// command) as that user.
fn boot_script(exports: &str, preamble: &str, guest_workspace: &str, agent: bool) -> String {
    let exec_host = format!("pillbox exec-host --vsock-port {EXEC_PORT}");
    if !agent {
        return format!("{exports}{preamble}; exec {exec_host}");
    }
    // claude refuses skip-permissions as root, so an agent sandbox's commands run as uid 1000.
    let uid = AGENT_UID;
    let home_q = shell_quote(GUEST_HOME);
    let gw_q = shell_quote(guest_workspace);
    let runuser = crate::commands::session::RUNUSER;
    format!(
        "{exports}{preamble}; \
         getent group {uid} >/dev/null || echo 'pillbox:x:{uid}:' >> /etc/group; \
         AGENT_USER=$(getent passwd {uid} | cut -d: -f1); \
         if [ -z \"$AGENT_USER\" ]; then \
             echo 'pillbox:x:{uid}:{uid}:pillbox:{GUEST_HOME}:/bin/sh' >> /etc/passwd; AGENT_USER=pillbox; \
         fi; \
         chown -R {uid}:{uid} {home_q} {gw_q} || \
             echo 'pillbox: warning: some sandbox files could not be handed to uid {uid}' >&2; \
         exec {runuser} -u \"$AGENT_USER\" --preserve-environment -- {exec_host}"
    )
}

/// The sandbox VM: the boot share, the workspace clone, and one guest-listening
/// vsock channel bridged to `sock`.
fn vm_spec(
    rootfs: &Path,
    boot_share: Share,
    boot_exec: Vec<String>,
    workspace_clone: &Path,
    sock: &Path,
    egress: EgressSpec,
) -> VmSpec {
    VmSpec {
        offline: false,
        ownership: None,
        rootfs: rootfs.to_string_lossy().into_owned(),
        vcpus: 2,
        ram_mib: 2048,
        shares: vec![
            boot_share,
            Share {
                tag: "workspace".into(),
                host_path: workspace_clone.to_string_lossy().into_owned(),
            },
        ],
        exec: boot_exec,
        // Guest listens; the host dials `sock` once per command.
        vsock: vec![VsockAttach {
            port: EXEC_PORT,
            host_sock: sock.to_string_lossy().into_owned(),
            listen: true,
        }],
        egress: Some(egress),
    }
}

/// Spawn the reparented VMM, wait for its exec channel, and commit the record.
fn launch(
    resolved: &Pillbox,
    id: &str,
    req: &SandboxSpawn,
    agent: Option<&AgentSpec>,
    mut prepared: Prepared,
) -> Result<Sandbox> {
    let record_path = Sandbox::record_path(resolved, id);
    let mut child = spawn_vmm(
        &prepared.spec_path,
        VmmTether::Reparented {
            record: &record_path,
        },
        &prepared.swap_pairs,
    )?;
    // From here the handle's teardown owns the rootfs and clones.
    prepared.rootfs.preserve();
    prepared.scratch.release();
    let handle = LibkrunHandle {
        sock: prepared.sock.to_string_lossy().into_owned(),
        pid: child.id() as i32,
        creds: prepared.creds_share.to_string_lossy().into_owned(),
        workspace: prepared.workspace_clone.to_string_lossy().into_owned(),
        spec: prepared.spec_path.to_string_lossy().into_owned(),
        rootfs: Some(prepared.rootfs.to_string_lossy().into_owned()),
    };
    let committed = (|| -> Result<Sandbox> {
        wait_exec_ready(
            &prepared.sock,
            &prepared.guest_workspace,
            EXEC_READY_TIMEOUT,
            || child.try_wait().context("poll the sandbox VMM"),
        )?;
        let record = Sandbox {
            id: id.to_string(),
            backend: sandboxes::BACKEND_LIBKRUN.into(),
            backend_ref: serde_json::to_string(&handle).context("encode libkrun handle")?,
            image: crate::docker::resolve_runner_image(resolved).0,
            agent: agent.map(|spec| spec.id.to_string()),
            workspace: req.workspace.display().to_string(),
            workspace_clone: Some(handle.workspace.clone()),
            guest_workspace: Some(prepared.guest_workspace.clone()),
            label: req.label.clone(),
            created_at: crate::session::now_rfc3339(),
            status: "ready".into(),
        };
        sandboxes::write(resolved, &record)?;
        Ok(record)
    })();
    committed.map_err(|e| abandon_launch("sandbox spawn", &mut child, &handle, e))
}

/// Poll the sandbox's exec channel until a `true` runs to exit 0 in the guest
/// workspace. Fails fast when `vmm_exited` reports the VMM gone, and after
/// `timeout` with the last probe's error.
fn wait_exec_ready(
    sock: &Path,
    guest_workspace: &str,
    timeout: Duration,
    mut vmm_exited: impl FnMut() -> Result<Option<std::process::ExitStatus>>,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let last = match probe_exec(sock, guest_workspace) {
            Ok(0) => return Ok(()),
            Ok(code) => anyhow::anyhow!("readiness probe exited {code}"),
            Err(e) => e,
        };
        if let Some(status) = vmm_exited()? {
            bail!("the sandbox VM exited before its exec channel was ready ({status})");
        }
        if Instant::now() >= deadline {
            bail!(
                "timed out after {}s waiting for the sandbox exec channel: {last:#}",
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn probe_exec(sock: &Path, guest_workspace: &str) -> Result<i32> {
    let stream = UnixStream::connect(sock).context("dial the sandbox exec socket")?;
    stream.set_read_timeout(Some(PROBE_IO_TIMEOUT)).ok();
    stream.set_write_timeout(Some(PROBE_IO_TIMEOUT)).ok();
    exec_channel::run_exec(
        stream,
        &["true".to_string()],
        Some(guest_workspace),
        |_, _| Ok(()),
    )
}

/// A running libkrun sandbox, addressed by its record's handle.
struct LibkrunSandbox {
    id: String,
    handle: LibkrunHandle,
    /// The exec cwd. Every libkrun record is written with it; a record without
    /// one can still be destroyed, never exec'd.
    guest_workspace: Option<String>,
}

impl SandboxTarget for LibkrunSandbox {
    fn exec(
        &self,
        argv: &[String],
        on_chunk: &mut dyn FnMut(bool, &[u8]) -> Result<()>,
    ) -> Result<i32> {
        let guest_workspace = self.guest_workspace.as_deref().ok_or_else(|| {
            PillboxError::config(
                "sandbox exec",
                format!("sandbox `{}` has no guest workspace in its record", self.id),
            )
            .with_next(format!("pillbox sandbox destroy {}", self.id))
        })?;
        let stream = UnixStream::connect(&self.handle.sock).map_err(|e| {
            PillboxError::runtime(
                "sandbox exec",
                format!("can't reach sandbox `{}` ({e})", self.id),
            )
            .with_next(format!("pillbox sandbox destroy {}", self.id))
        })?;
        exec_channel::run_exec(stream, argv, Some(guest_workspace), |is_stderr, b| {
            on_chunk(is_stderr, b)
        })
    }

    fn destroy(&self, resolved: &Pillbox) -> Result<()> {
        // The record goes only once the VM is confirmed down and its files removed.
        teardown_handle(&self.handle)?;
        sandboxes::delete(resolved, &self.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::SandboxBackend as _;
    use std::os::unix::net::UnixListener;

    #[test]
    fn libkrun_declares_long_lived_exec() {
        assert!(super::super::LibkrunBackend.capabilities().long_lived_exec);
    }

    #[test]
    fn bare_boot_script_runs_exec_host_as_root_on_the_exec_port() {
        let s = boot_script(
            "export A='1'\n",
            "set -e; cd /workspace/app",
            "/workspace/app",
            false,
        );
        assert!(
            s.ends_with("exec pillbox exec-host --vsock-port 1026"),
            "{s}"
        );
        assert!(!s.contains("runuser") && !s.contains("chown"), "{s}");
    }

    #[test]
    fn agent_boot_script_hands_files_to_uid_1000_and_drops_before_exec_host() {
        let s = boot_script(
            "",
            "set -e; cd '/workspace/my app'",
            "/workspace/my app",
            true,
        );
        let chown = s.find("chown -R 1000:1000 '/home/pillbox' '/workspace/my app'");
        let drop = s.find(
            "exec /usr/sbin/runuser -u \"$AGENT_USER\" --preserve-environment -- \
             pillbox exec-host --vsock-port 1026",
        );
        assert!(chown.is_some() && drop.is_some(), "{s}");
        assert!(chown < drop);
        assert!(s.contains("getent passwd 1000"), "{s}");
        let parsed = std::process::Command::new("sh")
            .args(["-n", "-c", &s])
            .status()
            .unwrap();
        assert!(parsed.success(), "boot script does not parse: {s}");
    }

    #[test]
    fn sandbox_boot_script_is_sealed_against_the_real_credential() {
        let pair = SwapPair {
            stub: "sk-ant-oat01-STUB".into(),
            real: "sk-ant-oat01-REAL".into(),
            hosts: vec!["api.anthropic.com".into()],
        };
        let env = vec![("TOKEN".to_string(), pair.stub.clone())];
        let exports = boot::env_exports(&env).unwrap();
        let ok = boot_script(&exports, "set -e", "/workspace/app", true);
        assert!(ensure_env_fork_sealed(std::slice::from_ref(&pair), &env, &ok).is_ok());
        let leaked = boot_script(&exports, "echo sk-ant-oat01-REAL", "/workspace/app", true);
        assert!(ensure_env_fork_sealed(&[pair], &env, &leaked).is_err());
    }

    #[test]
    fn sandbox_vm_spec_listens_on_the_exec_port_and_never_opens_login_egress() {
        let spec = vm_spec(
            Path::new("/r"),
            Share {
                tag: "creds".into(),
                host_path: "/c".into(),
            },
            vec!["/bin/sh".into()],
            Path::new("/ws-clone"),
            Path::new("/k/exec.sock"),
            EgressSpec {
                allowlist: vec!["api.anthropic.com".into()],
                log_path: None,
                ca_dir: None,
                local_forward_port: None,
                refresh: None,
                login: false,
            },
        );
        assert_eq!(spec.vsock.len(), 1);
        assert_eq!(spec.vsock[0].port, EXEC_PORT);
        assert!(spec.vsock[0].listen);
        assert_eq!(spec.vsock[0].host_sock, "/k/exec.sock");
        assert_eq!(spec.shares[1].tag, "workspace");
        assert_eq!(spec.shares[1].host_path, "/ws-clone");
        assert!(!spec.egress.as_ref().unwrap().login);
        assert!(!spec.offline);
    }

    #[test]
    fn sandbox_run_opts_carry_only_the_workspace() {
        let opts = sandbox_run_opts(Path::new("/work/app"));
        assert_eq!(opts.workspace.as_deref(), Some(Path::new("/work/app")));
        assert!(opts.withs.is_empty() && opts.egress_allow.is_empty() && !opts.vault);
    }

    /// A host socket served by the real guest-side exec channel, as `exec-host` would.
    fn serve_on(sock: &Path) {
        let listener = UnixListener::bind(sock).unwrap();
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let _ = exec_channel::serve_exec(conn);
                });
            }
        });
    }

    #[test]
    fn exec_readiness_waits_for_the_channel_and_fails_on_vmm_exit_or_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap().to_string();
        let sock = dir.path().join("exec.sock");

        let err = wait_exec_ready(&sock, &cwd, Duration::from_millis(300), || Ok(None))
            .unwrap_err()
            .to_string();
        assert!(err.contains("timed out"), "{err}");

        let exited = std::process::Command::new("true").status().unwrap();
        let err = wait_exec_ready(&sock, &cwd, Duration::from_secs(30), || Ok(Some(exited)))
            .unwrap_err()
            .to_string();
        assert!(err.contains("exited before"), "{err}");

        // The guest comes up while spawn is already polling.
        let late = sock.clone();
        let server = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            serve_on(&late);
        });
        wait_exec_ready(&sock, &cwd, Duration::from_secs(30), || Ok(None)).unwrap();
        server.join().unwrap();
    }

    fn handle_in(dir: &Path) -> LibkrunHandle {
        let creds = dir.join("creds");
        let ws = dir.join("ws");
        std::fs::create_dir_all(&creds).unwrap();
        std::fs::create_dir_all(&ws).unwrap();
        let spec = dir.join("pillbox-krun-spec-test.json");
        std::fs::write(&spec, "{}").unwrap();
        LibkrunHandle {
            sock: dir.join("exec.sock").to_string_lossy().into_owned(),
            pid: 0,
            creds: creds.to_string_lossy().into_owned(),
            workspace: ws.to_string_lossy().into_owned(),
            spec: spec.to_string_lossy().into_owned(),
            rootfs: None,
        }
    }

    #[test]
    fn target_exec_streams_both_streams_and_the_exit_code_in_the_guest_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let handle = handle_in(dir.path());
        serve_on(Path::new(&handle.sock));
        std::fs::write(dir.path().join("ws/marker"), "x").unwrap();
        let target = LibkrunSandbox {
            id: "abcdef012345".into(),
            guest_workspace: Some(handle.workspace.clone()),
            handle,
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let argv: Vec<String> = ["sh", "-c", "ls; echo err >&2; exit 3"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let code = target
            .exec(&argv, &mut |is_stderr, b| {
                if is_stderr { &mut err } else { &mut out }.extend_from_slice(b);
                Ok(())
            })
            .unwrap();
        assert_eq!(code, 3);
        assert_eq!(out, b"marker\n");
        assert_eq!(err, b"err\n");
    }

    #[test]
    fn target_exec_on_a_dead_sandbox_names_the_destroy_fix() {
        let dir = tempfile::tempdir().unwrap();
        let target = LibkrunSandbox {
            id: "abcdef012345".into(),
            guest_workspace: Some("/workspace/app".into()),
            handle: handle_in(dir.path()),
        };
        let err = target
            .exec(&["true".into()], &mut |_, _| Ok(()))
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("can't reach sandbox"), "{msg}");
    }

    #[test]
    fn a_record_without_a_guest_workspace_refuses_exec_and_names_destroy() {
        let dir = tempfile::tempdir().unwrap();
        let handle = handle_in(dir.path());
        serve_on(Path::new(&handle.sock));
        let target = LibkrunSandbox {
            id: "abcdef012345".into(),
            guest_workspace: None,
            handle,
        };
        let err = target
            .exec(&["true".into()], &mut |_, _| Ok(()))
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("no guest workspace"), "{msg}");
        let next = err.downcast_ref::<PillboxError>().unwrap().next.clone();
        assert!(next
            .unwrap()
            .contains("pillbox sandbox destroy abcdef012345"));
    }

    #[test]
    fn libkrun_record_round_trips_to_a_target_and_destroy_removes_files_then_record() {
        crate::test_util::with_isolated_home("sandbox-vm-destroy", || {
            let g = crate::pillbox::global();
            let dir = tempfile::tempdir().unwrap();
            let handle = handle_in(dir.path());
            let record = Sandbox {
                id: Sandbox::new_id(),
                backend: sandboxes::BACKEND_LIBKRUN.into(),
                backend_ref: serde_json::to_string(&handle).unwrap(),
                image: "pillbox:latest".into(),
                agent: Some("claude".into()),
                workspace: "/work/app".into(),
                workspace_clone: Some(handle.workspace.clone()),
                guest_workspace: Some("/workspace/app".into()),
                label: None,
                created_at: crate::session::now_rfc3339(),
                status: "ready".into(),
            };
            sandboxes::write(&g, &record).unwrap();
            let back = sandboxes::resolve(&g, &record.id).unwrap();
            assert_eq!(back, record);

            crate::sandbox::sandbox_target(&back)
                .unwrap()
                .destroy(&g)
                .unwrap();
            assert!(sandboxes::resolve(&g, &record.id).is_err());
            for p in [&handle.creds, &handle.workspace, &handle.spec] {
                assert!(!Path::new(p).exists(), "{p} survived destroy");
            }
        });
    }

    #[test]
    fn scratch_removes_unreleased_dirs_only() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let mut dropped = Scratch::default();
        dropped.keep(&a);
        drop(dropped);
        let mut released = Scratch::default();
        released.keep(&b);
        released.release();
        assert!(!a.exists());
        assert!(b.exists());
    }
}
