//! `pillbox auth login` on the libkrun microVM — a foreground VM whose guest runs
//! the agent's login under the in-guest `pty-host`, with the agent's real auth home
//! as its read-write creds share (no env-fork: the login writes the credential).
//!
//! Its egress is the one built by [`EgressSpec::for_login`], which opens the
//! providers' OAuth token endpoints through the MITM; every run, session and
//! sandbox launch builds [`EgressSpec::fence`], which keeps them rejected. A second vsock entry in
//! [`VmSpec`](super::VmSpec)`.vsock` can relay an agent's loopback OAuth callback
//! port. Shared launch helpers live in [`super::session`] (`pub(super)`).

use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::agents::{AgentSpec, GUEST_HOME, GUEST_WORKSPACE};
use crate::attach::pump;
use crate::pillbox::Pillbox;

use super::session::{
    disk_preflight, guest_launch_preamble, provision_vault_ca, run_foreground_attached,
    rust_tls_ca_env, CaLifetime, KillUnlessExited, OwnedPath, PendingVm, RemoveOnDrop, ATTACH_PORT,
    FORWARD_PORT,
};
use super::{
    boot, krun_cache_dir, materialize_rootfs, shell_quote, EgressSpec, Share, VmSpec, VsockAttach,
};

/// Where the guest mounts the login VM's boot-script share. A dedicated share, so
/// nothing pillbox-internal is ever written into the user's real auth home.
const GUEST_BOOT_DIR: &str = "/run/pillbox-boot";
/// The guest workspace the shared preamble mounts and `cd`s into (an empty scratch dir).
const GUEST_LOGIN_WORKSPACE: &str = "login";

/// Boot a foreground microVM that runs `spec.login_argv`, pump the terminal until
/// it exits, tear the VM down, and return the guest's exit code. The caller checks
/// the credential sentinel and finalizes. `home` is `spec`'s real auth home.
pub(crate) fn login(spec: &AgentSpec, resolved: &Pillbox, home: &Path) -> Result<i32> {
    disk_preflight()?;
    let rootfs = materialize_rootfs(resolved)?;
    let ca = provision_vault_ca(resolved, CaLifetime::Ephemeral)?;

    // Private, empty, and removed with the VM: the boot-script share and the
    // workspace the shared preamble mounts. The real auth home is never removed.
    let scratch = |prefix: &str, what: &str| -> Result<OwnedPath> {
        let dir = tempfile::Builder::new()
            .prefix(prefix)
            .tempdir()
            .with_context(|| format!("create {what}"))?;
        Ok(OwnedPath::new(dir.keep()))
    };
    let boot_dir = scratch("pillbox-login-boot-", "login boot dir")?;
    let workspace = scratch("pillbox-login-ws-", "login scratch workspace")?;

    let cache = krun_cache_dir()?;
    std::fs::create_dir_all(&cache).with_context(|| format!("create {}", cache.display()))?;
    let id = uuid::Uuid::now_v7().simple().to_string();
    let attach_sock = cache.join(format!("attach-{id}.sock"));
    let forward_sock = cache.join(format!("login-fwd-{id}.sock"));
    let _ = std::fs::remove_file(&forward_sock);
    // libkrun binds the callback socket for the guest's listener; it goes on every path.
    let _forward_sock = RemoveOnDrop(forward_sock.clone());

    let oauth_port = spec.resolved_oauth_port();
    let egress_log = super::egress_log_path();
    let (vmspec, _script) = login_vm_spec(
        spec,
        oauth_port,
        &ca.cert_pem,
        &LoginPaths {
            rootfs: &rootfs,
            home,
            boot_dir: &boot_dir,
            workspace: &workspace,
            attach_sock: &attach_sock,
            forward_sock: &forward_sock,
            ca_dir: &ca.dir,
            egress_log: egress_log.clone(),
        },
    )?;
    let mut vm = PendingVm::new(rootfs, workspace, boot_dir, &vmspec, attach_sock)?;

    // The host side of the OAuth callback: bound before boot so a busy port fails
    // the login before a VM exists. Stopped when dropped, on every path.
    let _relay = match oauth_port {
        Some(port) => Some(CallbackRelay::start(port, forward_sock.clone())?),
        None => None,
    };

    eprintln!(
        "pillbox: libkrun backend — launching `{}` login in a microVM",
        spec.id
    );
    // The login VM must stay Foreground: a reparented VM's CommitGuard scrubs every
    // share on an abandoned launch, and this VM's creds share is the user's real auth home.
    // No swap pairs: the login holds no credential to swap, it mints one.
    // A login that ends any way but the guest's exit kills its VM: nothing outlives it.
    let outcome =
        run_foreground_attached(&mut vm, &[], egress_log.as_deref(), &mut KillUnlessExited)?;
    match outcome {
        pump::Outcome::Exited(code) => Ok(code),
        pump::Outcome::Detached | pump::Outcome::Disconnected => {
            bail!("the login VM disconnected before `{}` exited", spec.id)
        }
    }
}

/// Host paths a login VmSpec points at.
struct LoginPaths<'a> {
    rootfs: &'a Path,
    home: &'a Path,
    boot_dir: &'a Path,
    workspace: &'a Path,
    attach_sock: &'a Path,
    forward_sock: &'a Path,
    ca_dir: &'a Path,
    egress_log: Option<String>,
}

/// Build the login VM: write its boot script into `paths.boot_dir` and return the
/// VmSpec with the script. Pure apart from that write, so it is unit-tested.
fn login_vm_spec(
    spec: &AgentSpec,
    oauth_port: Option<u16>,
    ca_cert_pem: &str,
    paths: &LoginPaths<'_>,
) -> Result<(VmSpec, String)> {
    let guest_workspace = format!("{GUEST_WORKSPACE}/{GUEST_LOGIN_WORKSPACE}");
    let mut guest_env = boot::static_child_env();
    guest_env.push(("NODE_EXTRA_CA_CERTS".into(), super::GUEST_CA_PATH.into()));
    guest_env.extend(rust_tls_ca_env(spec));
    let exports = boot::env_exports(&guest_env)?;
    let preamble = guest_launch_preamble(ca_cert_pem, &guest_workspace);
    let home_q = shell_quote(GUEST_HOME);
    let forward = match oauth_port {
        Some(port) => {
            format!("{} & ", boot::vsock_forward_cmd(FORWARD_PORT, port))
        }
        None => String::new(),
    };
    let argv = spec
        .login_argv
        .iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    let script = format!(
        "{exports}{preamble}; \
         mkdir -p {home_q}; mount -t virtiofs creds {home_q}; \
         {forward}exec {pty_host} {argv}",
        pty_host = boot::pty_host_cmd(ATTACH_PORT, false),
    );
    // The real auth home keeps its host ownership: only disposable clones are normalized.
    let (boot_share, exec) = boot::boot_channel(
        paths.boot_dir,
        "boot",
        GUEST_BOOT_DIR,
        boot::MountedShareOwnership::PreserveHost,
        &script,
    )?;

    let mut vsock = vec![VsockAttach {
        port: ATTACH_PORT,
        host_sock: paths.attach_sock.to_string_lossy().into_owned(),
        listen: false,
    }];
    if oauth_port.is_some() {
        vsock.push(VsockAttach {
            port: FORWARD_PORT,
            host_sock: paths.forward_sock.to_string_lossy().into_owned(),
            listen: true,
        });
    }

    let vmspec = VmSpec {
        offline: false,
        ownership: None,
        rootfs: paths.rootfs.to_string_lossy().into_owned(),
        vcpus: 2,
        ram_mib: 2048,
        shares: vec![
            boot_share,
            // The real auth home, read-write: the login writes the credential here.
            Share {
                tag: "creds".into(),
                host_path: paths.home.to_string_lossy().into_owned(),
            },
            Share {
                tag: "workspace".into(),
                host_path: paths.workspace.to_string_lossy().into_owned(),
            },
        ],
        exec,
        vsock,
        egress: Some(EgressSpec::for_login(
            login_allowlist(spec),
            paths.egress_log.clone(),
            Some(paths.ca_dir.to_string_lossy().into_owned()),
        )),
    };
    Ok((vmspec, script))
}

/// The login VM's fence: every vault provider's hosts (API + OAuth/token
/// endpoints) plus the hosts only this agent's login flow reaches.
fn login_allowlist(spec: &AgentSpec) -> Vec<String> {
    let mut hosts: Vec<String> = crate::vault::providers::intercepted_hosts()
        .into_iter()
        .map(str::to_string)
        .collect();
    for host in login_only_hosts(spec) {
        if !hosts.iter().any(|h| h == host) {
            hosts.push((*host).to_string());
        }
    }
    hosts
}

/// Hosts an agent's login flow reaches beyond the vault providers' hosts.
fn login_only_hosts(spec: &AgentSpec) -> &'static [&'static str] {
    match spec.auth_id {
        "claude" | "pi" | "prime" => &["claude.ai"],
        "cursor" => &["cursor.com", "www.cursor.com", "api2.cursor.sh"],
        "opencode" => &["models.dev"],
        _ => &[],
    }
}

/// Host loopback listener(s) on the agent's OAuth callback port, relaying each
/// connection to the libkrun-bound socket of the guest's `vsock-forward`.
struct CallbackRelay {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl CallbackRelay {
    fn start(port: u16, forward_sock: PathBuf) -> Result<Self> {
        let v4 = TcpListener::bind(("127.0.0.1", port)).map_err(|e| {
            crate::errors::PillboxError::runtime(
                "login",
                format!("cannot listen on 127.0.0.1:{port} for the OAuth callback: {e}"),
            )
            .with_next("free that port (or set PILLBOX_<AGENT>_OAUTH_PORT), then retry")
        })?;
        let mut listeners = vec![v4];
        // Browsers may try `localhost` as ::1 first; best effort, IPv4 is the contract.
        if let Ok(v6) = TcpListener::bind(("::1", port)) {
            listeners.push(v6);
        }
        let stop = Arc::new(AtomicBool::new(false));
        let mut threads = Vec::new();
        for listener in listeners {
            listener
                .set_nonblocking(true)
                .context("set OAuth relay listener nonblocking")?;
            let stop = stop.clone();
            let sock = forward_sock.clone();
            threads.push(std::thread::spawn(move || {
                accept_loop(&listener, &sock, &stop)
            }));
        }
        Ok(Self { stop, threads })
    }
}

impl Drop for CallbackRelay {
    /// Stop accepting and release the port; in-flight relays finish on their own.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for t in std::mem::take(&mut self.threads) {
            let _ = t.join();
        }
    }
}

fn accept_loop(listener: &TcpListener, forward_sock: &Path, stop: &AtomicBool) {
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((tcp, _)) => {
                let sock = forward_sock.to_path_buf();
                std::thread::spawn(move || relay_conn(tcp, &sock));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

/// Copy one callback connection both ways until either side closes.
fn relay_conn(tcp: TcpStream, forward_sock: &Path) {
    // An accepted socket inherits the listener's O_NONBLOCK on macOS.
    let _ = tcp.set_nonblocking(false);
    let guest = match UnixStream::connect(forward_sock) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "pillbox: OAuth callback relay: connect {}: {e}",
                forward_sock.display()
            );
            return;
        }
    };
    let (Ok(mut tcp_rd), Ok(mut guest_wr)) = (tcp.try_clone(), guest.try_clone()) else {
        return;
    };
    let up = std::thread::spawn(move || {
        let _ = std::io::copy(&mut tcp_rd, &mut guest_wr);
        let _ = guest_wr.shutdown(Shutdown::Write);
    });
    let (mut guest_rd, mut tcp_wr) = (guest, tcp);
    let _ = std::io::copy(&mut guest_rd, &mut tcp_wr);
    let _ = tcp_wr.shutdown(Shutdown::Write);
    let _ = up.join();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{CLAUDE, CODEX};

    fn build(spec: &AgentSpec, oauth_port: Option<u16>) -> (VmSpec, String, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let boot_dir = dir.path().join("boot");
        std::fs::create_dir(&boot_dir).unwrap();
        let home = dir.path().join("auth/claude");
        std::fs::create_dir_all(&home).unwrap();
        let (vmspec, script) = login_vm_spec(
            spec,
            oauth_port,
            "PEM",
            &LoginPaths {
                rootfs: &dir.path().join("rootfs"),
                home: &home,
                boot_dir: &boot_dir,
                workspace: &dir.path().join("ws"),
                attach_sock: &dir.path().join("attach.sock"),
                forward_sock: &dir.path().join("fwd.sock"),
                ca_dir: &dir.path().join("ca"),
                egress_log: None,
            },
        )
        .unwrap();
        (vmspec, script, dir)
    }

    #[test]
    fn claude_login_vm_opens_token_endpoints_mounts_the_real_home_and_forwards_the_callback() {
        let (vm, script, dir) = build(&CLAUDE, Some(54545));
        let egress = vm.egress.as_ref().unwrap();
        assert!(egress.opens_token_endpoints());
        assert!(egress.refresh.is_none());
        assert!(egress.allowlist.iter().any(|h| h == "platform.claude.com"));
        let creds = vm.shares.iter().find(|s| s.tag == "creds").unwrap();
        assert_eq!(
            creds.host_path,
            dir.path().join("auth/claude").to_string_lossy()
        );
        let fwd = vm.vsock.iter().find(|v| v.port == FORWARD_PORT).unwrap();
        assert!(fwd.listen);
        assert!(vm.vsock.iter().any(|v| v.port == ATTACH_PORT && !v.listen));
        assert!(script.contains(&format!(
            "pillbox vsock-forward --vsock-port {FORWARD_PORT} --to-port 54545 &"
        )));
        assert!(script.contains(&format!("mount -t virtiofs creds '{GUEST_HOME}'")));
        assert!(script.ends_with(&format!(
            "exec pillbox pty-host --vsock-port {ATTACH_PORT} -- 'claude' 'auth' 'login' '--claudeai'"
        )));
        // The boot script lands in the boot share, never in the real auth home.
        assert_eq!(
            std::fs::read_dir(dir.path().join("auth/claude"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn dropping_the_callback_relay_releases_the_oauth_port() {
        let port = TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let relay = super::CallbackRelay::start(port, PathBuf::from("/nonexistent.sock")).unwrap();
        assert!(
            TcpListener::bind(("127.0.0.1", port)).is_err(),
            "relay holds the port"
        );
        drop(relay);
        TcpListener::bind(("127.0.0.1", port)).expect("an early return frees the port");
    }

    #[test]
    fn login_without_an_oauth_port_has_no_callback_forward() {
        assert_eq!(CODEX.oauth_port, None);
        let (vm, script, _dir) = build(&CODEX, None);
        assert!(vm.egress.as_ref().unwrap().opens_token_endpoints());
        assert_eq!(vm.vsock.len(), 1);
        assert_eq!(vm.vsock[0].port, ATTACH_PORT);
        assert!(!script.contains("vsock-forward"));
        // codex's Rust TLS trusts the login MITM only through SSL_CERT_FILE.
        assert!(script.contains(&format!("SSL_CERT_FILE='{}'", super::super::GUEST_CA_PATH)));
        let (_, claude_script, _dir) = build(&CLAUDE, None);
        assert!(!claude_script.contains("SSL_CERT_FILE"));
        assert!(vm
            .egress
            .as_ref()
            .unwrap()
            .allowlist
            .iter()
            .any(|h| h == "auth.openai.com"));
    }
}
