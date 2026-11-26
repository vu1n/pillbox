//! `pillbox sandbox spawn|exec|agent|destroy` on the libkrun microVM.
//!
//! A sandbox is a detached VM whose guest runs `pillbox exec-host` on a listening
//! vsock port; each `exec` dials the libkrun-bound host socket and speaks
//! [`exec_channel`](crate::sandbox::exec_channel). Its record stores the
//! [`LibkrunHandle`] JSON in `backend_ref`, and destroy is [`teardown_handle`] plus
//! deleting the record.
//!
//! The VM gets the run path's launch prologue: a private rootfs clone, a CoW
//! secret-scrubbed workspace clone (never the host directory itself), the egress
//! fence and the vault CA. With `--agent`, a vault-capable agent also gets the PTY
//! path's env-fork: stubbed creds in the guest, the reals only on the VMM child's
//! stdin.
//!
//! Wired in: `LibkrunBackend::spawn_sandbox` calls [`spawn`], and
//! `sandbox::sandbox_target` calls [`target`] for a libkrun record.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::agents::{workspace_mount_name, AgentSpec, RunOpts, GUEST_HOME, GUEST_WORKSPACE};
use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::sandbox::exec_channel;
use crate::sandbox::{SandboxSpawn, SandboxTarget};
use crate::sandboxes::{self, Sandbox};

use super::session::{
    arm_commit_guard_at, disk_preflight, ensure_env_fork_sealed, env_fork_creds,
    guest_launch_preamble, launch_base, provision_vault_ca, sigabrt_boot_error, teardown_handle,
    vault_egress_allowlist, vmm_own_process_group, CaLifetime, EnvFork, LaunchBase, LibkrunHandle,
    VaultCa,
};
use super::{
    boot, cow_clone_and_scrub, cow_clone_home, egress, krun_cache_dir, materialize_rootfs,
    shell_quote, EgressSpec, Share, SwapPair, VmSpec, VsockAttach, GUEST_CA_PATH,
};

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
    let agent = match req.agent.as_deref() {
        Some(a) => {
            let spec = crate::agents::lookup("sandbox spawn", a)?;
            if !spec.is_authenticated(resolved) {
                return Err(PillboxError::runtime(
                    "sandbox spawn",
                    format!("no stored credentials for `{a}`"),
                )
                .with_next(format!("pillbox auth login --agent {a}"))
                .into());
            }
            Some(spec)
        }
        None => None,
    };
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
        guest_workspace: guest_workspace_for(Path::new(&record.workspace))?,
    }))
}

/// Where the guest mounts the workspace clone of host workspace `host`.
fn guest_workspace_for(host: &Path) -> Result<String> {
    Ok(format!(
        "{GUEST_WORKSPACE}/{}",
        workspace_mount_name(host, None)?
    ))
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

/// The rootfs, workspace clone, guest env and CA, from the run path's
/// [`launch_base`] for an agent, or the same steps minus the credential check
/// for a bare sandbox (which has no agent to check).
struct Base {
    rootfs: super::rootfs::PrivateRootfs,
    workspace_clone: PathBuf,
    guest_workspace: String,
    guest_env: Vec<(String, String)>,
    ca: VaultCa,
}

fn bare_base(resolved: &Pillbox, opts: &RunOpts, scratch: &mut Scratch) -> Result<Base> {
    let workspace = opts
        .workspace
        .as_deref()
        .context("sandbox launch without a workspace")?;
    disk_preflight()?;
    let rootfs = materialize_rootfs(resolved)?;
    let guest_workspace = guest_workspace_for(workspace)?;
    // The guest gets a scrubbed clone, never the host workspace itself.
    let workspace_clone = cow_clone_and_scrub(workspace)?;
    scratch.keep(&workspace_clone);
    let mut guest_env = boot::static_child_env();
    guest_env.extend(crate::agents::resolve_run_env(resolved, opts, &[], None)?);
    let ca = provision_vault_ca(resolved, CaLifetime::Persistent)?;
    guest_env.push(("NODE_EXTRA_CA_CERTS".into(), GUEST_CA_PATH.into()));
    Ok(Base {
        rootfs,
        workspace_clone,
        guest_workspace,
        guest_env,
        ca,
    })
}

/// Build the launch: base, creds share, boot script (sealed against a leaked
/// real credential), and the persisted VmSpec.
fn prepare(resolved: &Pillbox, agent: Option<&AgentSpec>, opts: &RunOpts) -> Result<Prepared> {
    let mut scratch = Scratch::default();
    let (base, creds_share, swap_pairs, allowlist, refresh) = match agent {
        // A vault-capable agent takes the PTY path's env-fork: stubbed creds in
        // the guest, the reals only on the VMM child's stdin.
        Some(spec) => {
            // The sandbox VM reparents past this CLI, so its CA must persist.
            let LaunchBase {
                rootfs,
                home,
                workspace_clone,
                guest_workspace,
                guest_env,
                ca,
                with_vault,
            } = launch_base(spec, opts, resolved, CaLifetime::Persistent)?;
            scratch.keep(&workspace_clone);
            let base = Base {
                rootfs,
                workspace_clone,
                guest_workspace,
                guest_env,
                ca,
            };
            if spec.vault_capable {
                let EnvFork {
                    creds_share,
                    swap_pairs,
                    with_hosts,
                    refresh,
                } = env_fork_creds(spec, &home, with_vault, |_| Ok(()))?;
                scratch.keep(&creds_share);
                let allowlist = vault_egress_allowlist(opts, &with_hosts);
                (base, creds_share, swap_pairs, allowlist, refresh)
            } else {
                // Non-vault agents (pi, cursor) mount their own key, as on the
                // structured run path; they take no vaulted secrets.
                let creds_share = cow_clone_home(&home)?;
                scratch.keep(&creds_share);
                (
                    base,
                    creds_share,
                    Vec::new(),
                    standard_allowlist(opts),
                    None,
                )
            }
        }
        None => {
            let base = bare_base(resolved, opts, &mut scratch)?;
            let creds_share = empty_home()?;
            scratch.keep(&creds_share);
            (
                base,
                creds_share,
                Vec::new(),
                standard_allowlist(opts),
                None,
            )
        }
    };

    let preamble = guest_launch_preamble(&base.ca.cert_pem, &base.guest_workspace);
    let exports = boot::env_exports(&base.guest_env)?;
    let script = boot_script(&exports, &preamble, &base.guest_workspace, agent.is_some());
    ensure_env_fork_sealed(&swap_pairs, &base.guest_env, &script)?;
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
        &base.rootfs,
        boot_share,
        boot_exec,
        &base.workspace_clone,
        &sock,
        EgressSpec {
            allowlist,
            log_path: super::egress_log_path(),
            ca_dir: Some(base.ca.dir.to_string_lossy().into_owned()),
            local_forward_port: None,
            refresh,
            // Only `auth login` opens the token endpoints; a sandbox never does.
            login: false,
        },
    );
    let spec_file = tempfile::Builder::new()
        .prefix("pillbox-krun-spec-")
        .suffix(".json")
        .tempfile()
        .context("create VMM spec tempfile")?;
    serde_json::to_writer(&spec_file, &vmspec).context("write VMM spec")?;
    // The reparented child reads the spec after this CLI returns: persist it.
    let (_, spec_path) = spec_file.keep().context("persist VMM spec")?;

    Ok(Prepared {
        rootfs: base.rootfs,
        spec_path,
        sock,
        creds_share,
        workspace_clone: base.workspace_clone,
        guest_workspace: base.guest_workspace,
        swap_pairs,
        scratch,
        _ca: base.ca,
    })
}

/// Egress for a sandbox holding no vaulted credential: the provider set plus the
/// standard egress hosts (the structured run path's non-vault posture).
fn standard_allowlist(opts: &RunOpts) -> Vec<String> {
    let mut allowlist = vault_egress_allowlist(opts, &[]);
    allowlist.extend(
        egress::standard_egress_hosts()
            .iter()
            .map(|host| (*host).to_string()),
    );
    allowlist
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
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            let _ = std::fs::remove_file(&prepared.spec_path);
            return Err(e).context("locate the pillbox binary to re-exec as VMM");
        }
    };
    let mut cmd = Command::new(&exe);
    cmd.arg("__krun-vmm")
        .arg(&prepared.spec_path)
        .env_clear()
        .envs(boot::static_child_env())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // The VM self-destructs unless this sandbox's record lands before this CLI dies.
    arm_commit_guard_at(&mut cmd, &Sandbox::record_path(resolved, id));
    vmm_own_process_group(&mut cmd);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            let _ = std::fs::remove_file(&prepared.spec_path);
            return Err(e).context("spawn the sandbox VMM subprocess");
        }
    };
    // From here the handle's teardown owns the rootfs and clones.
    prepared.rootfs.preserve();
    let Prepared {
        rootfs,
        spec_path,
        sock,
        creds_share,
        workspace_clone,
        guest_workspace,
        swap_pairs,
        scratch,
        _ca,
    } = prepared;
    scratch.release();

    // The real credentials reach only the VMM child's MITM, on its stdin.
    if let Some(mut sin) = child.stdin.take() {
        let blob = serde_json::to_string(&swap_pairs).unwrap_or_else(|_| "[]".into());
        use std::io::Write as _;
        let _ = sin.write_all(blob.as_bytes());
    }

    let handle = LibkrunHandle {
        sock: sock.to_string_lossy().into_owned(),
        pid: child.id() as i32,
        creds: creds_share.to_string_lossy().into_owned(),
        workspace: workspace_clone.to_string_lossy().into_owned(),
        spec: spec_path.to_string_lossy().into_owned(),
        rootfs: Some(rootfs.to_string_lossy().into_owned()),
    };
    let committed = (|| -> Result<Sandbox> {
        wait_exec_ready(&sock, &guest_workspace, EXEC_READY_TIMEOUT, || {
            child.try_wait().context("poll the sandbox VMM")
        })?;
        let record = Sandbox {
            id: id.to_string(),
            backend: sandboxes::BACKEND_LIBKRUN.into(),
            backend_ref: serde_json::to_string(&handle).context("encode libkrun handle")?,
            image: crate::docker::resolve_runner_image(resolved).0,
            agent: agent.map(|spec| spec.id.to_string()),
            workspace: req.workspace.display().to_string(),
            workspace_clone: Some(handle.workspace.clone()),
            label: req.label.clone(),
            created_at: crate::session::now_rfc3339(),
            status: "ready".into(),
        };
        sandboxes::write(resolved, &record)?;
        Ok(record)
    })();
    match committed {
        Ok(record) => Ok(record),
        Err(e) => {
            // We still own the child: read its own status first (a SIGABRT at boot
            // is the missing-runtime-deps footgun), then kill, reap and tear down.
            let self_status = child.try_wait().ok().flatten();
            let _ = child.kill();
            let _ = child.wait();
            teardown_handle(&handle).with_context(|| format!("sandbox spawn failed: {e:#}"))?;
            Err(sigabrt_boot_error(self_status).unwrap_or(e))
        }
    }
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
    guest_workspace: String,
}

impl SandboxTarget for LibkrunSandbox {
    fn exec(
        &self,
        argv: &[String],
        on_chunk: &mut dyn FnMut(bool, &[u8]) -> Result<()>,
    ) -> Result<i32> {
        let stream = UnixStream::connect(&self.handle.sock).map_err(|e| {
            PillboxError::runtime(
                "sandbox exec",
                format!("can't reach sandbox `{}` ({e})", self.id),
            )
            .with_next(format!("pillbox sandbox destroy {}", self.id))
        })?;
        exec_channel::run_exec(stream, argv, Some(&self.guest_workspace), |is_stderr, b| {
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
        assert_eq!(
            guest_workspace_for(Path::new("/work/app")).unwrap(),
            "/workspace/app"
        );
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
            guest_workspace: handle.workspace.clone(),
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
            guest_workspace: "/workspace/app".into(),
            handle: handle_in(dir.path()),
        };
        let err = target
            .exec(&["true".into()], &mut |_, _| Ok(()))
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("can't reach sandbox"), "{msg}");
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
