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
//! egress fence and the vault CA. `--agent` takes only a vault-capable agent with a
//! stdout harness adapter ([`sandbox_agent`]), whose env-fork
//! ([`agent_launch_creds`]) puts stubbed creds in the guest and the reals only on
//! the VMM child's stdin.
//!
//! Wired in: `LibkrunBackend::spawn_sandbox` calls [`spawn`], and
//! `sandbox::sandbox_target` calls [`target`] for a libkrun record.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::agents::harness;
use crate::agents::{AgentSpec, GUEST_HOME};
use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::sandbox::exec_channel;
use crate::sandbox::{Destroyed, ExecChannel, SandboxSpawn, SandboxTarget};
use crate::sandboxes::{self, Sandbox};

use super::session::{
    agent_launch_creds, authenticated_home, ensure_env_fork_sealed, guest_launch_preamble,
    launch_base, spawn_reparented, standard_allowlist, teardown_handle, CaLifetime, LaunchBase,
    LaunchCreds, LaunchInputs, LibkrunHandle, OwnedPath, PendingVm, VaultCa,
};
use super::{boot, krun_cache_dir, shell_quote, EgressSpec, Share, SwapPair, VmSpec, VsockAttach};

/// Guest vsock port `exec-host` listens on. Distinct from the attach (1024) and
/// forward (1025) ports.
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
    let agent = req.agent.as_deref().map(sandbox_agent).transpose()?;
    let id = Sandbox::new_id();
    let prepared = prepare(resolved, agent, &req.workspace)?;
    launch(resolved, &id, req, agent, prepared)
}

/// The agent `sandbox spawn --agent` may provision: env-forked (vault-capable,
/// so the guest only ever holds stubs) and drivable by `sandbox agent` (it has a
/// stdout harness adapter). Refused before any rootfs or clone exists.
fn sandbox_agent(id: &str) -> Result<&'static AgentSpec> {
    let spec = crate::agents::lookup("sandbox spawn", id)?;
    // Any other agent would mount its real credential in the guest, or could never be driven.
    if spec.vault_capable && harness::lookup(spec.id).is_some() {
        return Ok(spec);
    }
    Err(PillboxError::usage(
        "sandbox spawn",
        format!(
            "`--agent {id}` can't run in a sandbox: only an agent whose credential is \
             env-forked and whose output `sandbox agent` can read is supported ({})",
            sandbox_agents().join(", ")
        ),
    )
    .into())
}

/// Every agent [`sandbox_agent`] accepts.
fn sandbox_agents() -> Vec<&'static str> {
    crate::agents::ALL
        .iter()
        .filter(|spec| spec.vault_capable && harness::lookup(spec.id).is_some())
        .map(|spec| spec.id)
        .collect()
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

/// Everything [`launch`] needs, built before any VMM exists.
struct Prepared {
    vm: PendingVm,
    guest_workspace: String,
    swap_pairs: Vec<SwapPair>,
    _ca: VaultCa,
}

/// Build the launch: the shared prologue over `workspace` (a sandbox takes no
/// `--with`, bundles, bookmark or extra egress), the creds share, the boot script
/// (sealed against a leaked real credential), and the persisted VmSpec.
fn prepare(resolved: &Pillbox, agent: Option<&AgentSpec>, workspace: &Path) -> Result<Prepared> {
    let inputs = LaunchInputs {
        workspace: Some(workspace),
        ..LaunchInputs::default()
    };
    let agent = agent
        .map(|spec| authenticated_home(spec, &inputs, resolved, "sandbox spawn"))
        .transpose()?;
    // The sandbox VM reparents past this CLI, so its CA must persist.
    let LaunchBase {
        rootfs,
        workspace_clone,
        guest_workspace,
        mut guest_env,
        ca,
        vaulted,
    } = launch_base(&inputs, resolved, CaLifetime::Persistent)?;
    let LaunchCreds {
        creds_share,
        swap_pairs,
        allowlist,
        refresh,
        stub_env,
    } = match &agent {
        Some(agent) => agent_launch_creds(agent, resolved, &[], &vaulted)?,
        None => LaunchCreds {
            creds_share: OwnedPath::new(empty_home()?),
            swap_pairs: Vec::new(),
            allowlist: standard_allowlist(&[]),
            refresh: None,
            stub_env: Vec::new(),
        },
    };
    guest_env.extend(stub_env);

    let script = sealed_boot_script(
        &guest_env,
        &ca.cert_pem,
        &guest_workspace,
        agent.is_some(),
        &swap_pairs,
    )?;
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
        EgressSpec::fence(
            allowlist,
            super::egress_log_path(),
            Some(ca.dir.to_string_lossy().into_owned()),
            None,
            refresh,
        ),
    );
    let vm = PendingVm::new(rootfs, workspace_clone, creds_share, &vmspec, sock)?;
    Ok(Prepared {
        vm,
        guest_workspace,
        swap_pairs,
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
/// mount), then `exec-host` listening on [`EXEC_PORT`], sealed against a real
/// swap credential: the only way to build a sandbox boot script, so no launch
/// can skip the seal. An agent sandbox first hands the home and workspace to
/// uid 1000, whose identity every exec'd command then runs as.
fn sealed_boot_script(
    guest_env: &[(String, String)],
    ca_cert_pem: &str,
    guest_workspace: &str,
    agent: bool,
    swap_pairs: &[SwapPair],
) -> Result<String> {
    let exports = boot::env_exports(guest_env)?;
    let preamble = guest_launch_preamble(ca_cert_pem, guest_workspace);
    // A bare sandbox holds no credential, so its commands run as guest root; an agent
    // sandbox's run as uid 1000 under a root exec-host they cannot kill or impersonate.
    let script = if !agent {
        format!(
            "{exports}{preamble}; exec {}",
            boot::exec_host_cmd(EXEC_PORT, None)
        )
    } else {
        // claude refuses skip-permissions as root, so an agent sandbox's commands run as uid 1000.
        let uid = AGENT_UID;
        let home_q = shell_quote(GUEST_HOME);
        let gw_q = shell_quote(guest_workspace);
        format!(
            "{exports}{preamble}; \
             getent group {uid} >/dev/null || echo 'pillbox:x:{uid}:' >> /etc/group; \
             getent passwd {uid} >/dev/null || \
                 echo 'pillbox:x:{uid}:{uid}:pillbox:{GUEST_HOME}:/bin/sh' >> /etc/passwd; \
             chown -R {uid}:{uid} {home_q} {gw_q} || \
                 echo 'pillbox: warning: some sandbox files could not be handed to uid {uid}' >&2; \
             exec {exec_host}",
            exec_host = boot::exec_host_cmd(EXEC_PORT, Some(uid)),
        )
    };
    ensure_env_fork_sealed(swap_pairs, guest_env, &script)?;
    Ok(script)
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
    prepared: Prepared,
) -> Result<Sandbox> {
    let record_path = Sandbox::record_path(resolved, id);
    let guest_workspace = prepared.guest_workspace;
    let sock = prepared.vm.sock.clone();
    let spawned = spawn_reparented(prepared.vm, &record_path, &prepared.swap_pairs)?;
    spawned
        .commit("sandbox spawn", |child, handle| {
            wait_exec_ready(&sock, &guest_workspace, EXEC_READY_TIMEOUT, || {
                child.try_wait().context("poll the sandbox VMM")
            })?;
            let record = Sandbox {
                id: id.to_string(),
                backend: sandboxes::BACKEND_LIBKRUN.into(),
                backend_ref: serde_json::to_string(handle).context("encode libkrun handle")?,
                image: crate::docker::resolve_runner_image(resolved).0,
                agent: agent.map(|spec| spec.id.to_string()),
                workspace: req.workspace.display().to_string(),
                workspace_clone: Some(handle.workspace.clone()),
                guest_workspace: Some(guest_workspace.clone()),
                label: req.label.clone(),
                created_at: crate::session::now_rfc3339(),
                status: "ready".into(),
            };
            sandboxes::write(resolved, &record)?;
            Ok(record)
        })
        .map(|(record, _child)| record)
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
        let last = match probe_exec(sock, guest_workspace, deadline) {
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

fn probe_exec(sock: &Path, guest_workspace: &str, deadline: Instant) -> Result<i32> {
    let stream = UnixStream::connect(sock).context("dial the sandbox exec socket")?;
    exec_channel::run_exec(
        DeadlineStream { stream, deadline },
        &["true".to_string()],
        Some(guest_workspace),
        |_, _| Ok(()),
    )
}

/// A probe's stream: every read and write fails once `deadline` passes, so a guest
/// trickling frames can't hold readiness past its timeout.
struct DeadlineStream {
    stream: UnixStream,
    deadline: Instant,
}

impl DeadlineStream {
    /// Bound the next socket call by the time left (and by [`PROBE_IO_TIMEOUT`]).
    fn arm(&self) -> std::io::Result<()> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "readiness probe deadline passed",
            ));
        }
        let bound = Some(left.min(PROBE_IO_TIMEOUT));
        self.stream.set_read_timeout(bound)?;
        self.stream.set_write_timeout(bound)
    }
}

impl std::io::Read for DeadlineStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.arm()?;
        self.stream.read(buf)
    }
}

impl std::io::Write for DeadlineStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.arm()?;
        self.stream.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
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
    fn open_exec(&self) -> Result<Box<dyn ExecChannel>> {
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
        Ok(Box::new(LibkrunExec {
            stream,
            guest_workspace: guest_workspace.to_string(),
        }))
    }

    fn destroy(&self, resolved: &Pillbox) -> Result<Destroyed> {
        // The record goes only once the VM is confirmed down and its files removed.
        teardown_handle(&self.handle)?;
        sandboxes::delete(resolved, &self.id)?;
        Ok(Destroyed::TornDown)
    }
}

/// A dialed libkrun exec channel and the guest cwd its command runs in.
struct LibkrunExec {
    stream: UnixStream,
    guest_workspace: String,
}

impl ExecChannel for LibkrunExec {
    fn run(
        self: Box<Self>,
        argv: &[String],
        on_chunk: &mut dyn FnMut(bool, &[u8]) -> Result<()>,
    ) -> Result<i32> {
        exec_channel::run_exec(
            self.stream,
            argv,
            Some(&self.guest_workspace),
            |is_stderr, b| on_chunk(is_stderr, b),
        )
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

    fn script(env: &[(String, String)], agent: bool, pairs: &[SwapPair]) -> Result<String> {
        sealed_boot_script(env, "PEM", "/workspace/my app", agent, pairs)
    }

    #[test]
    fn bare_boot_script_runs_exec_host_and_its_commands_as_root_on_the_exec_port() {
        let env = vec![("A".to_string(), "1".to_string())];
        let s = script(&env, false, &[]).unwrap();
        assert!(s.starts_with("export A='1'\n"), "{s}");
        assert!(
            s.ends_with("exec pillbox exec-host --vsock-port 1026"),
            "{s}"
        );
        assert!(
            !s.contains("runuser") && !s.contains("chown -R 1000"),
            "{s}"
        );
    }

    #[test]
    fn agent_boot_script_hands_files_to_uid_1000_and_exec_host_stays_root_dropping_each_command() {
        let s = script(&[], true, &[]).unwrap();
        let chown = s.find("chown -R 1000:1000 '/home/pillbox' '/workspace/my app'");
        // exec-host is exec'd as root (no runuser); it drops each command to uid 1000.
        let exec_host = s.find("exec pillbox exec-host --vsock-port 1026 --run-as-uid 1000");
        assert!(chown.is_some() && exec_host.is_some(), "{s}");
        assert!(chown < exec_host);
        assert!(s.ends_with("exec pillbox exec-host --vsock-port 1026 --run-as-uid 1000"));
        assert!(!s.contains("runuser"), "{s}");
        assert!(s.contains("getent passwd 1000"), "{s}");
        let parsed = std::process::Command::new("sh")
            .args(["-n", "-c", &s])
            .status()
            .unwrap();
        assert!(parsed.success(), "boot script does not parse: {s}");
    }

    #[test]
    fn building_a_sandbox_boot_script_that_leaks_a_swap_secret_fails() {
        let pair = SwapPair {
            stub: "sk-ant-oat01-STUB".into(),
            real: "sk-ant-oat01-REAL".into(),
            hosts: vec!["api.anthropic.com".into()],
        };
        let pairs = std::slice::from_ref(&pair);
        let stubbed = vec![("TOKEN".to_string(), pair.stub.clone())];
        for agent in [false, true] {
            assert!(script(&stubbed, agent, pairs).is_ok());
            // A real in an exported env value…
            let leaked = vec![("TOKEN".to_string(), pair.real.clone())];
            let err = script(&leaked, agent, pairs).unwrap_err().to_string();
            assert!(err.contains("env-fork violated"), "{err}");
            // …or anywhere else in the rendered script (here, the preamble's CA text).
            let err = sealed_boot_script(&stubbed, &pair.real, "/workspace/app", agent, pairs)
                .unwrap_err()
                .to_string();
            assert!(err.contains("env-fork violated"), "{err}");
        }
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
            EgressSpec::fence(vec!["api.anthropic.com".into()], None, None, None, None),
        );
        assert_eq!(spec.vsock.len(), 1);
        assert_eq!(spec.vsock[0].port, EXEC_PORT);
        assert!(spec.vsock[0].listen);
        assert_eq!(spec.vsock[0].host_sock, "/k/exec.sock");
        assert_eq!(spec.shares[1].tag, "workspace");
        assert_eq!(spec.shares[1].host_path, "/ws-clone");
        assert!(!spec.egress.as_ref().unwrap().opens_token_endpoints());
        assert!(!spec.offline);
    }

    #[test]
    fn spawn_refuses_agents_that_are_not_env_forked_or_drivable_before_any_launch() {
        assert_eq!(sandbox_agents(), ["claude"]);
        assert_eq!(sandbox_agent("claude").unwrap().id, "claude");
        for id in [
            "pi",
            "cursor",
            "opencode",
            "prime-agent",
            "codex",
            "claude-stream",
        ] {
            let Err(err) = sandbox_agent(id) else {
                panic!("{id} accepted")
            };
            let msg = err.to_string();
            assert!(
                msg.contains("can't run in a sandbox") && msg.contains("(claude)"),
                "{id}: {msg}"
            );
            assert!(
                err.downcast_ref::<PillboxError>().is_some(),
                "{id}: not a usage error"
            );
        }
        // The refusal comes from `spawn` before any rootfs or clone exists.
        let req = SandboxSpawn {
            agent: Some("pi".into()),
            workspace: PathBuf::from("/nonexistent/pillbox-sandbox-ws"),
            label: None,
        };
        let msg = spawn(&crate::pillbox::global(), &req)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("can't run in a sandbox"), "{msg}");
    }

    /// A host socket served by the real guest-side exec channel, as `exec-host` would.
    fn serve_on(sock: &Path) {
        let listener = UnixListener::bind(sock).unwrap();
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let _ = exec_channel::serve_exec(conn, None);
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

    #[test]
    fn exec_readiness_deadline_bounds_a_probe_that_trickles_output() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("exec.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        // A guest that answers every probe with an endless trickle of stdout frames.
        std::thread::spawn(move || {
            for mut conn in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let _ = exec_channel::ExecFrame::decode(&mut conn);
                    loop {
                        if exec_channel::ExecFrame::Stdout(b"x".to_vec())
                            .encode(&mut conn)
                            .is_err()
                        {
                            return;
                        }
                        let _ = conn.flush();
                        std::thread::sleep(Duration::from_millis(20));
                    }
                });
            }
        });
        let started = Instant::now();
        let err = wait_exec_ready(&sock, "/", Duration::from_millis(500), || Ok(None))
            .unwrap_err()
            .to_string();
        assert!(err.contains("timed out"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
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
}
