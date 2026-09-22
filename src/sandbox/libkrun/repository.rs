//! Invocation-owned microVM transport for the bounded repository adapter.
//! The host file broker is the only repository access: builder VMs have no
//! repository shares. Live confinement and native-fork tests remain a release gate.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;

use super::{EgressSpec, RefreshSpec, SwapPair, VmSpec, VsockAttach};
use crate::paths::write_private_file;
use crate::vault::providers::codex_execution::CodexAccessRelease;

const PROVIDER_HOST: &str = "chatgpt.com";
const RPC_PORT: u32 = 1067;
const MAX_DURATION: Duration = Duration::from_secs(86_400);
const MAX_OUTPUT: u64 = 64 * 1024 * 1024;
const MAX_FRAME: usize = 4 * 1024 * 1024;
const MAX_GENERATED_FILE: usize = 4 * 1024 * 1024;
const COMMAND_OUTPUT_LIMIT: u64 = 64 * 1024;
const MAX_IMAGE_ARCHIVE: u64 = 16 * 1024 * 1024 * 1024;
const POLL: Duration = Duration::from_millis(20);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
const GUEST_RUNTIME: &str = "/opt/pillbox-execution";
const GUEST_HOME: &str = "/home/pillbox";
const GUEST_CODEX_HOME: &str = "/home/pillbox/.codex";

/// The caller has already admitted this invocation and pre-refreshed TokenStore.
/// No Debug or Serialize implementation: access_release contains a real token.
pub(crate) struct BuilderInput {
    pub(crate) image_id: String,
    pub(crate) codex_config: Vec<u8>,
    pub(crate) model_catalog: Vec<u8>,
    pub(crate) guest_auth: Vec<u8>,
    pub(crate) access_release: CodexAccessRelease,
    pub(crate) refresh_credentials: PathBuf,
}

#[derive(Clone, Copy)]
pub(crate) struct VmLimits {
    pub(crate) max_duration: Duration,
    pub(crate) max_output_bytes: u64,
    pub(crate) max_frame_bytes: usize,
}

impl VmLimits {
    fn validate(self) -> Result<()> {
        ensure!(
            !self.max_duration.is_zero() && self.max_duration <= MAX_DURATION,
            "VM duration is outside supported bounds"
        );
        ensure!(
            self.max_output_bytes > 0 && self.max_output_bytes <= MAX_OUTPUT,
            "VM output limit is outside supported bounds"
        );
        ensure!(
            self.max_frame_bytes > 0 && self.max_frame_bytes <= MAX_FRAME,
            "VM frame limit is outside supported bounds"
        );
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
pub(super) struct OwnershipSpec {
    socket: String,
    remaining_ms: u64,
}

/// Holds the private rootfs and host-side CA until termination is confirmed.
pub(crate) struct OwnedVm {
    process: OwnedProcess,
    owner: Option<UnixStream>,
    rpc_listener: Option<UnixListener>,
    deadline: Instant,
    runtime: Option<TempDir>,
    sockets: Option<TempDir>,
}

impl OwnedVm {
    pub(crate) fn connect_rpc(&mut self, cancelled: &dyn Fn() -> bool) -> Result<UnixStream> {
        loop {
            self.check_running(cancelled)?;
            let listener = self
                .rpc_listener
                .as_ref()
                .context("builder RPC connection was already consumed")?;
            match listener.accept() {
                Ok((stream, _)) => {
                    self.rpc_listener.take();
                    stream.set_read_timeout(Some(Duration::from_millis(100)))?;
                    stream.set_write_timeout(Some(Duration::from_millis(100)))?;
                    return Ok(stream);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(POLL)
                }
                Err(error) => return Err(error).context("accept builder RPC"),
            }
        }
    }

    pub(crate) fn check_running(&mut self, cancelled: &dyn Fn() -> bool) -> Result<()> {
        check_live(self.deadline, cancelled)?;
        self.process.drain()?;
        ensure!(
            self.process.exited()?.is_none(),
            "builder VMM exited before completion"
        );
        Ok(())
    }

    pub(crate) fn diagnostics(&mut self) -> Result<Vec<u8>> {
        self.process.drain()?;
        let mut output = self.process.stdout_bytes.clone();
        output.extend_from_slice(&self.process.stderr_bytes);
        Ok(output)
    }

    /// Must succeed before capturing results. Closing ownership also covers a VMM
    /// that reached its watchdog before the host sends the explicit group kill.
    pub(crate) fn stop_and_reap(&mut self) -> Result<ExitStatus> {
        self.owner.take();
        self.rpc_listener.take();
        let status = self.process.stop_and_reap()?;
        if let Some(runtime) = self.runtime.take() {
            runtime
                .close()
                .context("remove stopped builder rootfs and CA")?;
        }
        if let Some(sockets) = self.sockets.take() {
            sockets.close().context("remove stopped builder sockets")?;
        }
        Ok(status)
    }
}

impl Drop for OwnedVm {
    fn drop(&mut self) {
        if let Err(error) = self.stop_and_reap() {
            eprintln!("pillbox: bounded VM teardown failed: {error:#}");
            // Never remove a filesystem that a surviving VM might still serve.
            if let Some(runtime) = self.runtime.take() {
                let path = runtime.keep();
                eprintln!("pillbox: preserved VM files at {}", path.display());
            }
        }
    }
}

pub(crate) fn launch_builder(
    input: BuilderInput,
    limits: VmLimits,
    cancelled: &dyn Fn() -> bool,
) -> Result<OwnedVm> {
    limits.validate()?;
    validate_input(&input)?;
    let deadline = Instant::now()
        .checked_add(limits.max_duration)
        .context("VM deadline overflow")?;
    check_live(deadline, cancelled)?;
    super::host::virtualization_available().map_err(anyhow::Error::msg)?;
    super::host::runtime_deps_present().map_err(anyhow::Error::msg)?;
    let cache_root = super::krun_cache_dir()?.join("repository-images-v1");
    fs::create_dir_all(&cache_root)?;
    crate::paths::ensure_mode_0700(&cache_root)?;
    let free = super::host::disk_headroom(&cache_root);
    ensure!(
        free >= super::host::MIN_HEADROOM_BYTES,
        "insufficient or unknown disk headroom for bounded VM"
    );
    let base = provision_image(&input.image_id, &cache_root, deadline, cancelled)?;
    check_live(deadline, cancelled)?;
    let runtime = tempfile::Builder::new()
        .prefix("invocation-")
        .tempdir_in(&cache_root)?;
    let rootfs = runtime.path().join("rootfs");
    let method = crate::workspace::cow::cow_clone_dir(&base, &rootfs)?;
    if method == crate::workspace::cow::CloneMethod::Copied {
        eprintln!("pillbox: bounded rootfs fork fell back to a full copy");
    }
    check_live(deadline, cancelled)?;
    let ca_dir = runtime.path().join("ca");
    fs::create_dir(&ca_dir)?;
    crate::paths::ensure_mode_0700(&ca_dir)?;
    let ca = crate::vault::Ca::ensure(&ca_dir).map_err(anyhow::Error::msg)?;
    let certificate = fs::read(ca.cert_path())?;
    prepare_guest(
        &rootfs,
        &input,
        &certificate,
        limits,
        remaining_ms(deadline)?,
    )?;

    let sockets = socket_directory()?;
    let owner_path = sockets.path().join("owner.sock");
    let rpc_path = sockets.path().join("rpc.sock");
    let owner_listener = bind_listener(&owner_path)?;
    let rpc_listener = bind_listener(&rpc_path)?;
    let spec = builder_spec(
        &rootfs,
        &ca_dir,
        &owner_path,
        &rpc_path,
        &input,
        remaining_ms(deadline)?,
    );
    let spec_path = runtime.path().join("vm.json");
    write_private_file(&spec_path, &serde_json::to_vec(&spec)?)?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("__krun-vmm")
        .arg(&spec_path)
        .env_clear()
        .envs(super::boot::static_child_env());
    let process = OwnedProcess::spawn(&mut command, limits.max_output_bytes, true)?;
    let mut vm = OwnedVm {
        process,
        owner: None,
        rpc_listener: Some(rpc_listener),
        deadline,
        runtime: Some(runtime),
        sockets: Some(sockets),
    };
    vm.owner = Some(accept_owner(
        &owner_listener,
        &mut vm.process,
        deadline,
        cancelled,
    )?);
    let swaps = vec![SwapPair {
        stub: input.access_release.stub,
        real: input.access_release.real,
        hosts: vec![PROVIDER_HOST.into()],
    }];
    let bytes = serde_json::to_vec(&swaps)?;
    let mut stdin = vm
        .process
        .child
        .stdin
        .take()
        .context("VMM credential channel missing")?;
    nonblocking(stdin.as_raw_fd())?;
    write_until(&mut stdin, &bytes, deadline, cancelled)?;
    drop(stdin);
    Ok(vm)
}

fn validate_input(input: &BuilderInput) -> Result<()> {
    validate_image_id(&input.image_id)?;
    ensure!(
        input.refresh_credentials.is_absolute(),
        "refresh credentials require an absolute host path"
    );
    let release = &input.access_release;
    ensure!(
        !release.real.is_empty() && release.real.len() <= 64 * 1024,
        "access-token release is empty or too large"
    );
    ensure!(
        !release.stub.is_empty() && release.stub.len() <= 64 * 1024 && release.stub != release.real,
        "access-token stub is invalid"
    );
    for bytes in [&input.codex_config, &input.model_catalog, &input.guest_auth] {
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_GENERATED_FILE,
            "generated guest file is empty or too large"
        );
        ensure!(
            !bytes
                .windows(release.real.len())
                .any(|part| part == release.real.as_bytes()),
            "real access token appeared in guest input"
        );
    }
    let auth: serde_json::Value =
        serde_json::from_slice(&input.guest_auth).context("parse generated guest auth")?;
    ensure!(
        auth.pointer("/tokens/access_token")
            .and_then(|value| value.as_str())
            == Some(release.stub.as_str()),
        "generated guest auth does not contain the release stub"
    );
    serde_json::from_slice::<serde_json::Value>(&input.model_catalog)
        .context("parse generated model catalog")?;
    Ok(())
}

fn builder_spec(
    rootfs: &Path,
    ca_dir: &Path,
    owner: &Path,
    rpc: &Path,
    input: &BuilderInput,
    remaining_ms: u64,
) -> VmSpec {
    VmSpec {
        rootfs: rootfs.to_string_lossy().into_owned(),
        vcpus: 2,
        ram_mib: 2048,
        shares: vec![],
        exec: vec![
            "/usr/bin/python3".into(),
            "-I".into(),
            "-S".into(),
            format!("{GUEST_RUNTIME}/bridge.py"),
        ],
        vsock: Some(VsockAttach {
            port: RPC_PORT,
            host_sock: rpc.to_string_lossy().into_owned(),
            listen: false,
        }),
        egress: Some(EgressSpec {
            allowlist: vec![PROVIDER_HOST.into()],
            log_path: None,
            ca_dir: Some(ca_dir.to_string_lossy().into_owned()),
            local_forward_port: None,
            refresh: Some(RefreshSpec {
                creds_path: input.refresh_credentials.to_string_lossy().into_owned(),
                auth_id: "codex".into(),
                access_stub: input.access_release.stub.clone(),
            }),
        }),
        ownership: Some(OwnershipSpec {
            socket: owner.to_string_lossy().into_owned(),
            remaining_ms,
        }),
    }
}

fn prepare_guest(
    rootfs: &Path,
    input: &BuilderInput,
    certificate: &[u8],
    limits: VmLimits,
    remaining_ms: u64,
) -> Result<()> {
    fresh_directory(rootfs, GUEST_HOME)?;
    fresh_directory(rootfs, GUEST_RUNTIME)?;
    fresh_directory(rootfs, "/workspace")?;
    let codex_home = rootfs.join(GUEST_CODEX_HOME.trim_start_matches('/'));
    fs::create_dir(&codex_home)?;
    crate::paths::ensure_mode_0700(&codex_home)?;
    write_private_file(&codex_home.join("config.toml"), &input.codex_config)?;
    write_private_file(&codex_home.join("auth.json"), &input.guest_auth)?;
    write_private_file(&codex_home.join("models.json"), &input.model_catalog)?;
    let runtime = rootfs.join(GUEST_RUNTIME.trim_start_matches('/'));
    write_private_file(&runtime.join("bridge.py"), BRIDGE.as_bytes())?;
    write_private_file(&runtime.join("ca.crt"), certificate)?;
    write_private_file(
        &runtime.join("limits.json"),
        &serde_json::to_vec(&serde_json::json!({
            "duration_ms": remaining_ms,
            "max_output_bytes": limits.max_output_bytes,
            "max_frame_bytes": limits.max_frame_bytes,
        }))?,
    )?;
    Ok(())
}

fn fresh_directory(rootfs: &Path, guest_path: &str) -> Result<()> {
    let components: Vec<_> = guest_path.trim_start_matches('/').split('/').collect();
    let mut path = rootfs.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                ensure!(
                    metadata.is_dir() && !metadata.file_type().is_symlink(),
                    "generated guest path has a non-directory ancestor"
                );
                if index + 1 == components.len() {
                    fs::remove_dir_all(&path)
                        .context("clear generated guest directory in private clone")?;
                    fs::create_dir(&path)?;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&path)?,
            Err(error) => return Err(error).context("inspect generated guest directory"),
        }
    }
    crate::paths::ensure_mode_0700(&path)
}

pub(super) fn arm_vmm_ownership(spec: &OwnershipSpec) -> Result<()> {
    let mut owner = connect_owner(spec)?;
    let group = unsafe { libc::getpgrp() };
    ensure!(
        group == std::process::id() as i32 && group > 1,
        "owned VMM is not its process-group leader"
    );
    let deadline = Instant::now() + Duration::from_millis(spec.remaining_ms);
    std::thread::Builder::new()
        .name("invocation-owner".into())
        .spawn(move || {
            loop {
                if Instant::now() >= deadline || owner_gone(&mut owner).unwrap_or(true) {
                    // Group ownership was established before any guest or egress work.
                    if let Err(error) = signal_group(group, libc::SIGKILL) {
                        eprintln!(
                            "pillbox: invocation watchdog could not stop its group: {error:#}"
                        );
                    }
                    std::process::exit(70);
                }
                std::thread::sleep(POLL);
            }
        })
        .context("start invocation ownership watcher")?;
    Ok(())
}

fn connect_owner(spec: &OwnershipSpec) -> Result<UnixStream> {
    ensure!(
        spec.remaining_ms > 0 && spec.remaining_ms <= MAX_DURATION.as_millis() as u64,
        "invalid owner deadline"
    );
    let mut stream = UnixStream::connect(&spec.socket).context("connect invocation owner")?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    stream.write_all(b"owned\n")?;
    stream.set_nonblocking(true)?;
    Ok(stream)
}

fn owner_gone(stream: &mut UnixStream) -> Result<bool> {
    let mut byte = [0_u8];
    match stream.read(&mut byte) {
        Ok(0) => Ok(true),
        Ok(_) => bail!("unexpected data on ownership channel"),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => Ok(false),
        Err(error) => Err(error).context("read invocation ownership channel"),
    }
}

fn socket_directory() -> Result<TempDir> {
    tempfile::Builder::new()
        .prefix("pb-owned-")
        .tempdir_in("/tmp")
        .context("create private invocation socket directory")
}

fn bind_listener(path: &Path) -> Result<UnixListener> {
    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

fn accept_owner(
    listener: &UnixListener,
    process: &mut OwnedProcess,
    deadline: Instant,
    cancelled: &dyn Fn() -> bool,
) -> Result<UnixStream> {
    loop {
        check_live(deadline, cancelled)?;
        process.drain()?;
        ensure!(
            process.exited()?.is_none(),
            "owned child exited before ownership handshake"
        );
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_nonblocking(true)?;
                let mut handshake = [0_u8; 6];
                let mut read = 0;
                while read < handshake.len() {
                    check_live(deadline, cancelled)?;
                    match stream.read(&mut handshake[read..]) {
                        Ok(0) => bail!("ownership handshake ended early"),
                        Ok(count) => read += count,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(POLL)
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(error) => return Err(error).context("read ownership handshake"),
                    }
                }
                ensure!(&handshake == b"owned\n", "invalid ownership handshake");
                return Ok(stream);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(POLL)
            }
            Err(error) => return Err(error).context("accept invocation owner"),
        }
    }
}

fn check_live(deadline: Instant, cancelled: &dyn Fn() -> bool) -> Result<()> {
    ensure!(!cancelled(), "bounded execution cancelled");
    ensure!(
        Instant::now() < deadline,
        "bounded execution deadline exceeded"
    );
    Ok(())
}

fn remaining_ms(deadline: Instant) -> Result<u64> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .context("bounded execution deadline exceeded")?;
    let millis = remaining.as_millis() as u64;
    ensure!(millis > 0, "bounded execution deadline exceeded");
    Ok(millis)
}

fn nonblocking(fd: RawFd) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    ensure!(
        flags >= 0,
        "read pipe flags: {}",
        std::io::Error::last_os_error()
    );
    ensure!(
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == 0,
        "set nonblocking pipe: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

fn write_until(
    writer: &mut impl Write,
    mut bytes: &[u8],
    deadline: Instant,
    cancelled: &dyn Fn() -> bool,
) -> Result<()> {
    while !bytes.is_empty() {
        check_live(deadline, cancelled)?;
        match writer.write(bytes) {
            Ok(0) => bail!("owned child input closed before delivery"),
            Ok(count) => bytes = &bytes[count..],
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(POLL)
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error).context("write owned child input"),
        }
    }
    Ok(())
}

struct OwnedProcess {
    child: Child,
    group: i32,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    stdout_bytes: Vec<u8>,
    stderr_bytes: Vec<u8>,
    output_limit: u64,
    status: Option<ExitStatus>,
    stopped: bool,
}

impl OwnedProcess {
    fn spawn(command: &mut Command, output_limit: u64, stdin: bool) -> Result<Self> {
        command
            .stdin(if stdin { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Every host child owns a group; stopping the leader alone leaves helpers.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let limit = libc::rlimit {
                    rlim_cur: MAX_IMAGE_ARCHIVE,
                    rlim_max: MAX_IMAGE_ARCHIVE,
                };
                if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().context("spawn invocation-owned process")?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let process = Self {
            group: child.id() as i32,
            child,
            stdout,
            stderr,
            stdout_bytes: vec![],
            stderr_bytes: vec![],
            output_limit,
            status: None,
            stopped: false,
        };
        nonblocking(
            process
                .stdout
                .as_ref()
                .context("child stdout missing")?
                .as_raw_fd(),
        )?;
        nonblocking(
            process
                .stderr
                .as_ref()
                .context("child stderr missing")?
                .as_raw_fd(),
        )?;
        Ok(process)
    }

    fn exited(&mut self) -> Result<Option<ExitStatus>> {
        if self.status.is_none() {
            self.status = self.child.try_wait().context("poll owned child")?;
        }
        Ok(self.status)
    }

    fn drain(&mut self) -> Result<()> {
        let used = self.stdout_bytes.len() + self.stderr_bytes.len();
        drain_pipe(
            &mut self.stdout,
            &mut self.stdout_bytes,
            self.output_limit.saturating_sub(used as u64),
        )?;
        let used = self.stdout_bytes.len() + self.stderr_bytes.len();
        drain_pipe(
            &mut self.stderr,
            &mut self.stderr_bytes,
            self.output_limit.saturating_sub(used as u64),
        )
    }

    fn wait(&mut self, deadline: Instant, cancelled: &dyn Fn() -> bool) -> Result<ExitStatus> {
        loop {
            check_live(deadline, cancelled)?;
            self.drain()?;
            if let Some(status) = self.exited()? {
                if self.stdout.is_none() && self.stderr.is_none() {
                    return Ok(status);
                }
            }
            std::thread::sleep(POLL);
        }
    }

    fn stop_and_reap(&mut self) -> Result<ExitStatus> {
        if self.stopped {
            return self.status.context("stopped process has no exit status");
        }
        signal_group(self.group, libc::SIGKILL)?;
        let deadline = Instant::now() + STOP_TIMEOUT;
        loop {
            if let Some(status) = self.exited()? {
                self.stopped = true;
                return Ok(status);
            }
            ensure!(
                Instant::now() < deadline,
                "owned process did not reap after SIGKILL"
            );
            std::thread::sleep(POLL);
        }
    }
}

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        if let Err(error) = self.stop_and_reap() {
            eprintln!("pillbox: owned process cleanup failed: {error:#}");
        }
    }
}

fn signal_group(group: i32, signal: i32) -> Result<()> {
    ensure!(group > 1, "refusing invalid owned process group");
    if unsafe { libc::killpg(group, signal) } < 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error).context("signal owned process group");
        }
    }
    Ok(())
}

fn drain_pipe<T: Read>(pipe: &mut Option<T>, output: &mut Vec<u8>, remaining: u64) -> Result<()> {
    let Some(reader) = pipe.as_mut() else {
        return Ok(());
    };
    // One read per pipe per poll prevents a noisy child from starving cancellation.
    let mut buffer = [0_u8; 8192];
    let size = buffer.len().min(remaining.saturating_add(1) as usize);
    match reader.read(&mut buffer[..size]) {
        Ok(0) => {
            pipe.take();
        }
        Ok(count) => {
            ensure!(
                count as u64 <= remaining,
                "owned process output limit exceeded"
            );
            output.extend_from_slice(&buffer[..count]);
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            ) => {}
        Err(error) => return Err(error).context("read owned process output"),
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct ImageSpec {
    image_id: String,
    cache_root: PathBuf,
    ownership: OwnershipSpec,
}

fn validate_image_id(image_id: &str) -> Result<()> {
    ensure!(
        image_id.len() == 71
            && image_id.starts_with("sha256:")
            && image_id.as_bytes()[7..]
                .iter()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte)),
        "bounded VM requires a full lowercase immutable sha256 image ID"
    );
    Ok(())
}

fn provision_image(
    image_id: &str,
    cache_root: &Path,
    deadline: Instant,
    cancelled: &dyn Fn() -> bool,
) -> Result<PathBuf> {
    let sockets = socket_directory()?;
    let owner_path = sockets.path().join("image-owner.sock");
    let listener = bind_listener(&owner_path)?;
    let spec = ImageSpec {
        image_id: image_id.into(),
        cache_root: cache_root.into(),
        ownership: OwnershipSpec {
            socket: owner_path.to_string_lossy().into_owned(),
            remaining_ms: remaining_ms(deadline)?,
        },
    };
    let spec_path = sockets.path().join("image.json");
    write_private_file(&spec_path, &serde_json::to_vec(&spec)?)?;
    let mut command = Command::new(std::env::current_exe()?);
    command.arg("__repository-image").arg(&spec_path);
    let mut process = OwnedProcess::spawn(&mut command, COMMAND_OUTPUT_LIMIT, false)?;
    let owner = accept_owner(&listener, &mut process, deadline, cancelled)?;
    let result = process.wait(deadline, cancelled);
    drop(owner);
    // Give the guardian its own bounded cleanup window before forcing termination.
    if result.is_err() {
        if let Err(error) = process.wait(Instant::now() + STOP_TIMEOUT, &|| false) {
            eprintln!("pillbox: image guardian cleanup did not finish: {error:#}");
        }
    }
    process.stop_and_reap()?;
    let status = result?;
    ensure!(
        status.success(),
        "exact image preparation failed ({status}): {}",
        String::from_utf8_lossy(&process.stderr_bytes)
    );
    sockets.close().context("remove image guardian sockets")?;
    let rootfs = cache_root.join(&image_id[7..]).join("rootfs");
    ensure!(
        cache_complete(&rootfs, image_id)?,
        "image guardian returned without an exact completed rootfs"
    );
    Ok(rootfs)
}

/// Internal dispatch only: the caller retains the owner socket for this entire
/// export transaction. EOF stops command groups and removes the stopped container.
pub(crate) fn image_child_main() -> ! {
    let result = (|| -> Result<()> {
        let path = std::env::args_os()
            .nth(2)
            .context("missing image guardian spec")?;
        let mut bytes = vec![];
        File::open(path)?
            .take(64 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 64 * 1024, "image guardian spec too large");
        let spec: ImageSpec = serde_json::from_slice(&bytes)?;
        validate_image_id(&spec.image_id)?;
        ensure!(
            spec.cache_root.is_absolute(),
            "image cache path must be absolute"
        );
        let owner = std::cell::RefCell::new(connect_owner(&spec.ownership)?);
        let cancelled = || owner_gone(&mut owner.borrow_mut()).unwrap_or(true);
        let deadline = Instant::now() + Duration::from_millis(spec.ownership.remaining_ms);
        materialize_exact(&spec.image_id, &spec.cache_root, deadline, &cancelled)
    })();
    match result {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            eprintln!("repository-image: {error:#}");
            std::process::exit(70);
        }
    }
}

fn cache_complete(rootfs: &Path, image_id: &str) -> Result<bool> {
    let generation = rootfs.parent().context("rootfs generation missing")?;
    match fs::symlink_metadata(generation) {
        Ok(metadata) => ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "invalid exact image generation"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("inspect exact image generation"),
    }
    let metadata = fs::symlink_metadata(rootfs).context("inspect exact rootfs")?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "invalid exact rootfs"
    );
    let marker = generation.join("complete");
    let metadata = fs::symlink_metadata(&marker).context("inspect exact image marker")?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.len() == image_id.len() as u64,
        "invalid exact image marker"
    );
    ensure!(
        fs::read(&marker)? == image_id.as_bytes(),
        "exact image marker mismatch"
    );
    Ok(true)
}

fn materialize_exact(
    image_id: &str,
    cache_root: &Path,
    deadline: Instant,
    cancelled: &dyn Fn() -> bool,
) -> Result<()> {
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(cache_root.join(format!("{}.lock", &image_id[7..])))?;
    loop {
        check_live(deadline, cancelled)?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            break;
        }
        let error = std::io::Error::last_os_error();
        if !matches!(
            error.raw_os_error(),
            Some(libc::EWOULDBLOCK) | Some(libc::EINTR)
        ) {
            return Err(error).context("lock exact image cache");
        }
        std::thread::sleep(POLL);
    }
    let generation = cache_root.join(&image_id[7..]);
    if cache_complete(&generation.join("rootfs"), image_id)? {
        return Ok(());
    }
    let mut inspect = Command::new("docker");
    inspect.args(["image", "inspect", image_id, "--format", "{{.Id}}"]);
    let inspected = run_command(
        &mut inspect,
        deadline,
        cancelled,
        "inspect immutable runner image",
    )?;
    ensure!(
        std::str::from_utf8(&inspected)?.trim() == image_id,
        "Docker returned a different runner image ID"
    );
    let stage = tempfile::Builder::new()
        .prefix("export-")
        .tempdir_in(cache_root)?;
    let rootfs = stage.path().join("rootfs");
    fs::create_dir(&rootfs)?;
    let archive = stage.path().join("rootfs.tar");
    let container = format!("pillbox-repository-{}", uuid::Uuid::now_v7());
    let export = (|| -> Result<()> {
        let mut create = Command::new("docker");
        create.args(["create", "--name", &container, image_id]);
        run_command(
            &mut create,
            deadline,
            cancelled,
            "create exact runner export",
        )?;
        let mut export = Command::new("docker");
        export
            .args(["export", "--output"])
            .arg(&archive)
            .arg(&container);
        run_command(
            &mut export,
            deadline,
            cancelled,
            "export exact runner rootfs",
        )?;
        let length = fs::metadata(&archive)?.len();
        ensure!(
            length > 0 && length <= MAX_IMAGE_ARCHIVE,
            "runner export outside archive limit"
        );
        let mut extract = Command::new("tar");
        extract.arg("-C").arg(&rootfs).arg("-xpf").arg(&archive);
        run_command(
            &mut extract,
            deadline,
            cancelled,
            "extract exact runner rootfs",
        )?;
        fs::remove_file(&archive).context("remove exported runner archive")?;
        Ok(())
    })();
    let mut remove = Command::new("docker");
    remove.args(["rm", "--force", &container]);
    let cleanup = run_command(
        &mut remove,
        Instant::now() + STOP_TIMEOUT,
        &|| false,
        "remove runner export container",
    );
    match (export, cleanup) {
        (Err(error), Err(cleanup)) => bail!("{error:#}; export cleanup also failed: {cleanup:#}"),
        (Err(error), Ok(_)) => return Err(error),
        (Ok(()), Err(error)) => return Err(error),
        (Ok(()), Ok(_)) => {}
    }
    check_live(deadline, cancelled)?;
    commit_generation(stage, &generation, image_id)
}

fn commit_generation(stage: TempDir, generation: &Path, image_id: &str) -> Result<()> {
    write_private_file(&stage.path().join("complete"), image_id.as_bytes())?;
    fs::rename(stage.path(), generation).context("commit pristine exact image generation")?;
    // The renamed directory is cache authority now, no longer temporary state.
    drop(stage.keep());
    Ok(())
}

fn run_command(
    command: &mut Command,
    deadline: Instant,
    cancelled: &dyn Fn() -> bool,
    purpose: &str,
) -> Result<Vec<u8>> {
    check_live(deadline, cancelled)?;
    let mut process = OwnedProcess::spawn(command, COMMAND_OUTPUT_LIMIT, false)
        .with_context(|| purpose.to_owned())?;
    let result = process.wait(deadline, cancelled);
    process
        .stop_and_reap()
        .with_context(|| format!("stop {purpose}"))?;
    let status = result.with_context(|| purpose.to_owned())?;
    ensure!(
        status.success(),
        "{purpose} failed ({status}): {}",
        String::from_utf8_lossy(&process.stderr_bytes)
    );
    Ok(std::mem::take(&mut process.stdout_bytes))
}

// Guest transport is fixed trusted code. It owns no repository data and never
// interprets model output as a process status or host operation.
const BRIDGE: &str = r#"import json
import os
import select
import signal
import socket
import subprocess
import time

RUNTIME = '/opt/pillbox-execution'

class Frames:
    def __init__(self, limit):
        self.limit = limit
        self.partial = bytearray()
        self.ready = bytearray()

    def capacity(self):
        return self.limit + 1 - len(self.partial) - len(self.ready)

    def append(self, data):
        if len(data) > self.capacity():
            raise RuntimeError('RPC queue limit')
        self.partial.extend(data)
        while True:
            end = self.partial.find(b'\n')
            if end < 0:
                if len(self.partial) > self.limit:
                    raise RuntimeError('RPC frame limit')
                return
            if end > self.limit:
                raise RuntimeError('RPC frame limit')
            self.ready.extend(self.partial[:end + 1])
            del self.partial[:end + 1]

    def eof(self):
        if self.partial:
            raise RuntimeError('unterminated RPC frame')


def stop(proc):
    # Descendants can retain pipes after the leader exits.
    try:
        os.killpg(proc.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    proc.wait(timeout=2)


def relay(sock, proc, limits, deadline):
    to_child = Frames(limits['max_frame_bytes'])
    to_host = Frames(limits['max_frame_bytes'])
    console = bytearray()
    console_cap = 65536
    used = 0
    out_open = True
    err_open = True
    input_fd = proc.stdin.fileno()
    output_fd = proc.stdout.fileno()
    error_fd = proc.stderr.fileno()
    for fd in (input_fd, output_fd, error_fd, 2):
        os.set_blocking(fd, False)
    sock.setblocking(False)
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise RuntimeError('builder deadline')
        if not out_open and not err_open and not to_host.ready and not console:
            code = proc.wait(timeout=min(remaining, 2))
            if code != 0:
                raise RuntimeError('native app-server failed')
            return
        reads = []
        if to_child.capacity():
            reads.append(sock)
        if out_open and to_host.capacity():
            reads.append(output_fd)
        if err_open and len(console) < console_cap:
            reads.append(error_fd)
        writes = []
        if to_child.ready:
            writes.append(input_fd)
        if to_host.ready:
            writes.append(sock)
        if console:
            writes.append(2)
        readable, writable, _ = select.select(reads, writes, [], min(remaining, 0.1))
        for source in readable:
            budget = limits['max_output_bytes'] - used
            try:
                if source is sock:
                    data = sock.recv(min(65536, to_child.capacity()))
                    if not data:
                        to_child.eof()
                        raise RuntimeError('host disconnected')
                    to_child.append(data)
                elif source == output_fd:
                    data = os.read(output_fd, min(65536, to_host.capacity(), budget + 1))
                    used += len(data)
                    if used > limits['max_output_bytes']:
                        raise RuntimeError('builder output limit')
                    if data:
                        to_host.append(data)
                    else:
                        to_host.eof()
                        out_open = False
                else:
                    data = os.read(error_fd, min(65536, console_cap - len(console), budget + 1))
                    used += len(data)
                    if used > limits['max_output_bytes']:
                        raise RuntimeError('builder output limit')
                    if data:
                        console.extend(data)
                    else:
                        err_open = False
            except BlockingIOError:
                pass
        for destination in writable:
            if destination is sock:
                queue = to_host.ready
            elif destination == input_fd:
                queue = to_child.ready
            else:
                queue = console
            try:
                sent = sock.send(queue) if destination is sock else os.write(destination, queue)
                if sent <= 0:
                    raise RuntimeError('bridge write made no progress')
                del queue[:sent]
            except BlockingIOError:
                pass


def main():
    with open(RUNTIME + '/limits.json', 'rb') as handle:
        limits = json.load(handle)
    deadline = time.monotonic() + limits['duration_ms'] / 1000
    env = {
        'HOME': '/home/pillbox',
        'CODEX_HOME': '/home/pillbox/.codex',
        'PATH': '/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin',
        'LANG': 'C.UTF-8',
        'SSL_CERT_FILE': RUNTIME + '/ca.crt',
        'NODE_EXTRA_CA_CERTS': RUNTIME + '/ca.crt',
    }
    for argv in (
        ['/usr/sbin/ip', 'link', 'set', 'eth0', 'up'],
        ['/usr/sbin/ip', 'addr', 'add', '10.0.2.15/24', 'dev', 'eth0'],
        ['/usr/sbin/ip', 'route', 'add', 'default', 'via', '10.0.2.2'],
    ):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise RuntimeError('builder deadline')
        subprocess.run(argv, env=env, check=True, stdin=subprocess.DEVNULL,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                       timeout=min(remaining, 5))
    with open('/etc/resolv.conf', 'w') as handle:
        handle.write('nameserver 10.0.2.2\n')
    sock = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
    proc = None
    try:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise RuntimeError('builder deadline')
        sock.settimeout(min(remaining, 10))
        sock.connect((2, 1067))
        proc = subprocess.Popen(['/usr/local/bin/codex', 'app-server'],
                                cwd='/workspace', env=env,
                                stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, bufsize=0,
                                close_fds=True, start_new_session=True)
        relay(sock, proc, limits, deadline)
    finally:
        sock.close()
        if proc is not None:
            stop(proc)

if __name__ == '__main__':
    main()
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> VmLimits {
        VmLimits {
            max_duration: Duration::from_secs(2),
            max_output_bytes: 4096,
            max_frame_bytes: 512,
        }
    }

    fn input() -> BuilderInput {
        BuilderInput {
            image_id: format!("sha256:{}", "a".repeat(64)),
            codex_config: b"model = 'test'\n".to_vec(),
            model_catalog: br#"{"models":[]}"#.to_vec(),
            guest_auth: br#"{"tokens":{"access_token":"stub-only"}}"#.to_vec(),
            access_release: CodexAccessRelease {
                stub: "stub-only".into(),
                real: "host-only-real-token".into(),
            },
            refresh_credentials: PathBuf::from("/host/private/codex.json"),
        }
    }

    #[test]
    fn exact_image_id_never_accepts_tags_or_short_ids() {
        assert!(validate_image_id(&input().image_id).is_ok());
        for value in [
            "runner:dev",
            "sha256:abc",
            &format!("sha256:{}", "A".repeat(64)),
            &format!("sha256:{}z", "a".repeat(63)),
        ] {
            assert!(validate_image_id(value).is_err());
        }
    }

    #[test]
    fn explicit_limits_have_absolute_ceilings() {
        assert!(limits().validate().is_ok());
        let mut value = limits();
        value.max_duration = Duration::ZERO;
        assert!(value.validate().is_err());
        value = limits();
        value.max_output_bytes = MAX_OUTPUT + 1;
        assert!(value.validate().is_err());
        value = limits();
        value.max_frame_bytes = MAX_FRAME + 1;
        assert!(value.validate().is_err());
    }

    #[test]
    fn guest_inputs_cannot_contain_released_token() {
        assert!(validate_input(&input()).is_ok());
        let mut value = input();
        value.model_catalog =
            format!("{{\"leak\":\"{}\"}}", value.access_release.real).into_bytes();
        assert!(validate_input(&value)
            .unwrap_err()
            .to_string()
            .contains("real access token"));
        let mut value = input();
        value.guest_auth = br#"{"tokens":{"access_token":"other"}}"#.to_vec();
        assert!(validate_input(&value).is_err());
        let mut value = input();
        value.refresh_credentials = "relative.json".into();
        assert!(validate_input(&value).is_err());
    }

    #[test]
    fn builder_spec_has_no_shares_and_only_provider_egress() {
        let value = input();
        let spec = builder_spec(
            Path::new("/private/rootfs"),
            Path::new("/private/ca"),
            Path::new("/private/owner"),
            Path::new("/private/rpc"),
            &value,
            100,
        );
        assert!(spec.shares.is_empty());
        assert!(spec.ownership.is_some());
        assert_eq!(
            spec.exec,
            [
                "/usr/bin/python3",
                "-I",
                "-S",
                "/opt/pillbox-execution/bridge.py"
            ]
        );
        let egress = spec.egress.as_ref().unwrap();
        assert_eq!(egress.allowlist, ["chatgpt.com"]);
        assert!(egress.local_forward_port.is_none());
        let serialized = serde_json::to_string(&spec).unwrap();
        assert!(!serialized.contains(&value.access_release.real));
        assert!(serialized.contains(&value.access_release.stub));
    }

    #[test]
    fn legacy_vm_spec_defaults_to_no_invocation_owner() {
        let spec: VmSpec = serde_json::from_value(serde_json::json!({
            "rootfs":"/rootfs", "vcpus":1, "ram_mib":512,
            "shares":[], "exec":["/bin/true"], "vsock":null, "egress":null
        }))
        .unwrap();
        assert!(spec.ownership.is_none());
    }

    #[test]
    fn generated_home_and_workspace_are_fresh_private_directories() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("home/pillbox/.codex");
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("ambient-secret"), "must disappear").unwrap();
        fs::create_dir(dir.path().join("workspace")).unwrap();
        fs::write(dir.path().join("workspace/ambient-repo"), "must disappear").unwrap();
        prepare_guest(dir.path(), &input(), b"test-ca", limits(), 100).unwrap();
        assert!(!old.join("ambient-secret").exists());
        assert_eq!(fs::read(old.join("auth.json")).unwrap(), input().guest_auth);
        assert_eq!(
            fs::metadata(old.join("auth.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::read_dir(dir.path().join("workspace")).unwrap().count(),
            0
        );
        assert!(!dir.path().join("opt/pillbox-execution/ca.key").exists());
    }

    #[test]
    fn generated_guest_paths_reject_symlink_ancestors() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("home")).unwrap();
        assert!(prepare_guest(dir.path(), &input(), b"test-ca", limits(), 100).is_err());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn cache_requires_exact_marker_and_plain_rootfs() {
        let dir = tempfile::tempdir().unwrap();
        let generation = dir.path().join("generation");
        assert!(!cache_complete(&generation.join("rootfs"), &input().image_id).unwrap());
        fs::create_dir_all(generation.join("rootfs")).unwrap();
        fs::write(generation.join("complete"), &input().image_id).unwrap();
        assert!(cache_complete(&generation.join("rootfs"), &input().image_id).unwrap());
        fs::write(
            generation.join("complete"),
            format!("sha256:{}", "b".repeat(64)),
        )
        .unwrap();
        assert!(cache_complete(&generation.join("rootfs"), &input().image_id).is_err());
        fs::remove_dir(generation.join("rootfs")).unwrap();
        std::os::unix::fs::symlink("/", generation.join("rootfs")).unwrap();
        assert!(cache_complete(&generation.join("rootfs"), &input().image_id).is_err());
    }

    #[test]
    fn owner_channel_requires_live_silent_peer() {
        let (mut watcher, mut owner) = UnixStream::pair().unwrap();
        watcher.set_nonblocking(true).unwrap();
        assert!(!owner_gone(&mut watcher).unwrap());
        owner.write_all(b"x").unwrap();
        assert!(owner_gone(&mut watcher).is_err());
        drop(owner);
        assert!(owner_gone(&mut watcher).unwrap());
    }

    #[test]
    fn command_failure_keeps_exit_status_and_context() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf 'expected failure' >&2; exit 7"]);
        let error = run_command(
            &mut command,
            Instant::now() + Duration::from_secs(2),
            &|| false,
            "fixture export",
        )
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("fixture export"));
        assert!(text.contains("expected failure"));
        assert!(text.contains('7'));
    }

    #[test]
    fn output_exhaustion_is_bounded_and_child_reaped() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "while :; do printf '0123456789'; done"]);
        let mut process = OwnedProcess::spawn(&mut command, 128, false).unwrap();
        let error = process
            .wait(Instant::now() + Duration::from_secs(2), &|| false)
            .unwrap_err();
        assert!(error.to_string().contains("output limit"));
        assert!(process.stdout_bytes.len() + process.stderr_bytes.len() <= 128);
        process.stop_and_reap().unwrap();
        assert!(process.exited().unwrap().is_some());
        process.stop_and_reap().unwrap();
    }

    #[test]
    fn deadline_and_cancel_stop_process_groups_with_descendants() {
        for cancel in [false, true] {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", "sleep 10 & wait"]);
            let mut process = OwnedProcess::spawn(&mut command, 128, false).unwrap();
            let deadline = Instant::now() + Duration::from_millis(60);
            let error = process.wait(deadline, &|| cancel).unwrap_err();
            assert!(error
                .to_string()
                .contains(if cancel { "cancelled" } else { "deadline" }));
            let status = process.stop_and_reap().unwrap();
            assert!(!status.success());
            assert!(process.exited().unwrap().is_some());
        }
    }

    #[test]
    fn bridge_framing_handles_split_lines_capacity_and_partial_eof() {
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("bridge.py");
        fs::write(&script, BRIDGE).unwrap();
        let fixture = r#"import runpy, sys
s = runpy.run_path(sys.argv[1], run_name='test_bridge')
Frames = s['Frames']
f = Frames(8)
f.append(b'ab')
assert f.ready == b''
f.append(b'c\nd\n')
assert f.ready == b'abc\nd\n'
assert f.capacity() == 3
f.eof()
f = Frames(3)
f.append(b'abc\n')
assert f.ready == b'abc\n'
try:
    Frames(3).append(b'abcd')
    raise AssertionError('oversized frame accepted')
except RuntimeError:
    pass
f = Frames(8)
f.append(b'partial')
try:
    f.eof()
    raise AssertionError('partial EOF accepted')
except RuntimeError:
    pass
"#;
        let mut command = Command::new("python3");
        command.args(["-I", "-S", "-c", fixture]).arg(&script);
        run_command(
            &mut command,
            Instant::now() + Duration::from_secs(5),
            &|| false,
            "bridge framing fixture",
        )
        .unwrap();
    }

    #[test]
    fn complete_generation_is_committed_without_partial_cache_state() {
        let directory = tempfile::tempdir().unwrap();
        let stage = tempfile::tempdir_in(directory.path()).unwrap();
        fs::create_dir(stage.path().join("rootfs")).unwrap();
        let old_path = stage.path().to_path_buf();
        let generation = directory.path().join("generation");
        commit_generation(stage, &generation, &input().image_id).unwrap();
        assert!(!old_path.exists());
        assert!(cache_complete(&generation.join("rootfs"), &input().image_id).unwrap());
    }

    #[test]
    fn ownership_watchdog_fixture() {
        let Some(serialized) = std::env::var_os("PILLBOX_TEST_OWNERSHIP_SPEC") else {
            return;
        };
        let spec: OwnershipSpec = serde_json::from_str(serialized.to_str().unwrap()).unwrap();
        arm_vmm_ownership(&spec).unwrap();
        std::thread::sleep(Duration::from_secs(10));
        panic!("ownership watchdog did not terminate its process");
    }

    #[test]
    fn owner_loss_and_deadline_terminate_the_owned_process_group() {
        for owner_loss in [true, false] {
            let sockets = socket_directory().unwrap();
            let socket = sockets.path().join("owner.sock");
            let listener = bind_listener(&socket).unwrap();
            let spec = OwnershipSpec {
                socket: socket.to_str().unwrap().into(),
                remaining_ms: if owner_loss { 2000 } else { 120 },
            };
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "sandbox::libkrun::repository::tests::ownership_watchdog_fixture",
                    "--nocapture",
                ])
                .env(
                    "PILLBOX_TEST_OWNERSHIP_SPEC",
                    serde_json::to_string(&spec).unwrap(),
                );
            let mut process = OwnedProcess::spawn(&mut command, 4096, false).unwrap();
            let mut owner = Some(
                accept_owner(
                    &listener,
                    &mut process,
                    Instant::now() + Duration::from_secs(2),
                    &|| false,
                )
                .unwrap(),
            );
            if owner_loss {
                owner.take();
            }
            let status = process
                .wait(Instant::now() + Duration::from_secs(2), &|| false)
                .unwrap();
            assert!(!status.success());
            process.stop_and_reap().unwrap();
        }
    }
}
