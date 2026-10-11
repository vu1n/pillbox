//! One-shot text harness microVM for `pillbox.text/2` drivers whose harness prints a
//! JSON-lines stream on stdout (pi, Prime Agent).
//!
//! The guest has no shares and an empty `/workspace`. Its only egress is the model
//! provider's host, where the VMM swaps the guest's stub key for the real one. A small
//! bridge reports the harness's `--version`, feeds the prompt on stdin and relays
//! stdout over vsock; the host reads it back as bounded frames.

use std::fs;
use std::io::{ErrorKind, Read};
use std::path::Path;
use std::time::Instant;

use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};

use super::repository::{self, OwnedVm, VmLimits, GUEST_HOME, GUEST_RUNTIME, RPC_PORT};
use super::{EgressSpec, SwapPair, VmSpec, VsockAttach};
use crate::execution::pi_text::{Guest, GuestOutput, GuestTurn, Limits};
use crate::paths::write_private_file;

const MAX_GUEST_FILE: usize = 64 * 1024;

/// The libkrun [`Guest`].
pub(crate) struct LibkrunTextGuest;

impl Guest for LibkrunTextGuest {
    fn run(
        &mut self,
        turn: GuestTurn,
        limits: Limits,
        deadline: Instant,
        live: &dyn Fn() -> Result<()>,
        stage: &mut dyn FnMut(&'static str),
    ) -> Result<GuestOutput> {
        let cancelled = || live().is_err();
        let mut vm = launch(&turn, limits, deadline, &cancelled, stage)?;
        let mut frames = Vec::new();
        let relayed = vm.connect_rpc(&cancelled).and_then(|stream| {
            stage("guest_rpc_ready");
            read_frames(stream, limits, &mut frames, &mut || {
                live()?;
                // Drain the console so a chatty guest cannot block on a full pipe.
                vm.diagnostics().map(drop)
            })
        });
        let diagnostics = vm.diagnostics();
        vm.stop_and_reap()?;
        Ok(GuestOutput {
            frames,
            diagnostics: diagnostics?,
            error: relayed.err(),
        })
    }
}

fn launch(
    turn: &GuestTurn,
    limits: Limits,
    deadline: Instant,
    cancelled: &dyn Fn() -> bool,
    stage: &mut dyn FnMut(&'static str),
) -> Result<OwnedVm> {
    validate(turn)?;
    let vm_limits = VmLimits {
        max_duration: deadline
            .checked_duration_since(Instant::now())
            .context("text deadline exceeded")?,
        max_output_bytes: limits.max_evidence_bytes,
        max_frame_bytes: limits.max_frame_bytes as usize,
    };
    let (runtime, rootfs) = repository::prepare_rootfs(&turn.runner_image_id, deadline, cancelled)?;
    stage("image_prepare");
    let ca_dir = runtime.path().join("ca");
    fs::create_dir(&ca_dir)?;
    crate::paths::ensure_mode_0700(&ca_dir)?;
    let ca = crate::vault::Ca::ensure(&ca_dir).map_err(anyhow::Error::msg)?;
    let certificate = fs::read(ca.cert_path())?;
    prepare_guest(
        &rootfs,
        turn,
        &certificate,
        limits,
        repository::remaining_ms(deadline)?,
    )?;
    stage("guest_prepare");
    let mut vm = repository::launch_prepared(
        runtime,
        vm_limits,
        deadline,
        cancelled,
        true,
        |owner, rpc, remaining| spec(&rootfs, &ca_dir, owner, rpc, &turn.host, remaining),
    )?;
    // Context: doc://pillbox/libkrun-env-fork-substrate@0002#libkrun-env-fork-substrate — the guest holds the stub; the real key reaches only the VMM child, bound to the provider host.
    let swaps = [SwapPair {
        stub: turn.stub_key.clone(),
        real: turn.real_key.clone(),
        hosts: vec![turn.host.clone()],
    }];
    repository::deliver_swaps(&mut vm, &swaps, deadline, cancelled)?;
    stage("vmm_spawn");
    Ok(vm)
}

fn validate(turn: &GuestTurn) -> Result<()> {
    repository::validate_image_id(&turn.runner_image_id)?;
    ensure!(
        !turn.argv.is_empty() && !turn.version_argv.is_empty(),
        "text harness argv is empty"
    );
    ensure!(
        !turn.host.is_empty()
            && turn.host.len() <= 253
            && turn
                .host
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-')),
        "invalid provider host"
    );
    ensure!(
        !turn.real_key.is_empty() && turn.stub_key.len() >= 16 && turn.stub_key != turn.real_key,
        "invalid credential stub"
    );
    let real = turn.real_key.as_bytes();
    let carries_real = |bytes: &[u8]| bytes.windows(real.len()).any(|part| part == real);
    for (path, bytes) in &turn.home_files {
        ensure!(
            !path.is_empty()
                && !path.starts_with('/')
                && path
                    .split('/')
                    .all(|part| !part.is_empty() && part != "." && part != ".."),
            "invalid guest home path"
        );
        ensure!(bytes.len() <= MAX_GUEST_FILE, "guest file is too large");
        ensure!(
            !carries_real(bytes),
            "real credential appeared in guest input"
        );
    }
    let plain = serde_json::to_vec(&json!([turn.argv, turn.version_argv, turn.env]))?;
    ensure!(
        !carries_real(&plain) && !carries_real(&turn.stdin),
        "real credential appeared in guest input"
    );
    Ok(())
}

fn spec(
    rootfs: &Path,
    ca_dir: &Path,
    owner: &Path,
    rpc: &Path,
    host: &str,
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
            format!("{GUEST_RUNTIME}/text-bridge.py"),
        ],
        vsock: Some(VsockAttach {
            port: RPC_PORT,
            host_sock: rpc.to_string_lossy().into_owned(),
            listen: false,
        }),
        // Context: doc://pillbox/vault-egress-default-deny@0002#vault-egress-default-deny — only the provider host resolves.
        egress: Some(EgressSpec {
            allowlist: vec![host.into()],
            log_path: None,
            ca_dir: Some(ca_dir.to_string_lossy().into_owned()),
            local_forward_port: None,
            refresh: None,
        }),
        ownership: Some(repository::OwnershipSpec::bounded(owner, remaining_ms)),
    }
}

fn prepare_guest(
    rootfs: &Path,
    turn: &GuestTurn,
    certificate: &[u8],
    limits: Limits,
    remaining_ms: u64,
) -> Result<()> {
    repository::fresh_directory(rootfs, GUEST_HOME)?;
    repository::fresh_directory(rootfs, GUEST_RUNTIME)?;
    repository::fresh_directory(rootfs, "/workspace")?;
    let home = rootfs.join(GUEST_HOME.trim_start_matches('/'));
    for (path, bytes) in &turn.home_files {
        let target = home.join(path);
        let mut dir = home.clone();
        for part in Path::new(path)
            .parent()
            .into_iter()
            .flat_map(Path::components)
        {
            dir.push(part);
            if !dir.exists() {
                fs::create_dir(&dir)?;
                crate::paths::ensure_mode_0700(&dir)?;
            }
        }
        write_private_file(&target, bytes)?;
    }
    let runtime = rootfs.join(GUEST_RUNTIME.trim_start_matches('/'));
    write_private_file(&runtime.join("text-bridge.py"), BRIDGE.as_bytes())?;
    write_private_file(&runtime.join("ca.crt"), certificate)?;
    write_private_file(&runtime.join("input.txt"), &turn.stdin)?;
    write_private_file(
        &runtime.join("turn.json"),
        &serde_json::to_vec(&json!({
            "argv": turn.argv,
            "version_argv": turn.version_argv,
            "env": turn.env.iter().cloned().collect::<std::collections::BTreeMap<_, _>>(),
            "duration_ms": remaining_ms,
            "max_output_bytes": limits.max_evidence_bytes,
        }))?,
    )?;
    repository::prepare_generated_metadata(rootfs, &[GUEST_RUNTIME, GUEST_HOME, "/workspace"])
}

/// Read the relayed stdout as JSON lines until the guest closes the stream. Each line
/// is bounded by `max_frame_bytes` and the whole stream by `max_evidence_bytes`; a line
/// that is not a JSON object fails the read. `frames` keeps everything read so far.
pub(crate) fn read_frames(
    mut stream: impl Read,
    limits: Limits,
    frames: &mut Vec<Value>,
    poll: &mut dyn FnMut() -> Result<()>,
) -> Result<()> {
    let max_frame = limits.max_frame_bytes as usize;
    let mut total = 0u64;
    let mut pending = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let count = match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                poll()?;
                continue;
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error).context("read text harness stream"),
        };
        total += count as u64;
        ensure!(
            total <= limits.max_evidence_bytes,
            "text harness output exceeds max_evidence_bytes"
        );
        pending.extend_from_slice(&chunk[..count]);
        while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = pending.drain(..=end).collect();
            push_frame(&line[..end], max_frame, frames)?;
        }
        ensure!(
            pending.len() <= max_frame,
            "text harness frame exceeds max_frame_bytes"
        );
    }
    if !pending.is_empty() {
        push_frame(&pending, max_frame, frames)?;
    }
    Ok(())
}

fn push_frame(line: &[u8], max_frame: usize, frames: &mut Vec<Value>) -> Result<()> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.iter().all(u8::is_ascii_whitespace) {
        return Ok(());
    }
    ensure!(
        line.len() <= max_frame,
        "text harness frame exceeds max_frame_bytes"
    );
    let value: Value =
        serde_json::from_slice(line).context("text harness wrote a non-JSON line")?;
    if !value.is_object() {
        bail!("text harness wrote a non-object frame");
    }
    frames.push(value);
    Ok(())
}

// Kept dependency-free: the guest runs it as PID 1 with `python3 -I -S`.
const BRIDGE: &str = r#"import json
import os
import select
import signal
import socket
import subprocess
import time

RUNTIME = '/opt/pillbox-execution'


def send(sock, data, deadline):
    view = memoryview(data)
    while view:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise RuntimeError('text deadline')
        _, writable, _ = select.select([], [sock], [], min(remaining, 0.1))
        if writable:
            sent = sock.send(view)
            if sent <= 0:
                raise RuntimeError('relay write made no progress')
            view = view[sent:]


def frame(sock, value, deadline):
    send(sock, json.dumps(value, separators=(',', ':')).encode() + b'\n', deadline)


def stop(proc):
    if proc.poll() is None:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        proc.wait()


def main():
    with open(RUNTIME + '/turn.json', 'rb') as handle:
        turn = json.load(handle)
    deadline = time.monotonic() + turn['duration_ms'] / 1000
    env = dict(turn['env'])
    env.update({
        'HOME': '/home/pillbox',
        'PATH': '/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin',
        'LANG': 'C.UTF-8',
        'SSL_CERT_FILE': RUNTIME + '/ca.crt',
        'NODE_EXTRA_CA_CERTS': RUNTIME + '/ca.crt',
    })
    for argv in (
        ['/usr/sbin/ip', 'link', 'set', 'eth0', 'up'],
        ['/usr/sbin/ip', 'addr', 'add', '10.0.2.15/24', 'dev', 'eth0'],
        ['/usr/sbin/ip', 'route', 'add', 'default', 'via', '10.0.2.2'],
    ):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise RuntimeError('text deadline')
        subprocess.run(argv, env=env, check=True, stdin=subprocess.DEVNULL,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                       timeout=min(remaining, 5))
    with open('/etc/resolv.conf', 'w') as handle:
        handle.write('nameserver 10.0.2.2\n')
    sock = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
    proc = None
    try:
        sock.settimeout(max(0.1, min(deadline - time.monotonic(), 10)))
        sock.connect((2, 1067))
        sock.setblocking(False)
        version = subprocess.run(turn['version_argv'], cwd='/workspace', env=env,
                                 stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                 stderr=subprocess.DEVNULL,
                                 timeout=max(0.1, min(deadline - time.monotonic(), 30)))
        frame(sock, {'type': 'pillbox.harness_version',
                     'version': version.stdout[:64].decode('utf-8', 'replace').strip()},
              deadline)
        with open(RUNTIME + '/input.txt', 'rb') as stdin:
            proc = subprocess.Popen(turn['argv'], cwd='/workspace', env=env, stdin=stdin,
                                    stdout=subprocess.PIPE, bufsize=0, close_fds=True,
                                    start_new_session=True)
        output = proc.stdout.fileno()
        used = 0
        last = b'\n'
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise RuntimeError('text deadline')
            readable, _, _ = select.select([output], [], [], min(remaining, 0.1))
            if not readable:
                continue
            data = os.read(output, 65536)
            if not data:
                break
            used += len(data)
            if used > turn['max_output_bytes']:
                raise RuntimeError('text harness output limit')
            send(sock, data, deadline)
            last = data[-1:]
        if last != b'\n':
            send(sock, b'\n', deadline)
        code = proc.wait(timeout=max(0.1, min(deadline - time.monotonic(), 5)))
        frame(sock, {'type': 'pillbox.harness_exit', 'code': code}, deadline)
    finally:
        if proc is not None:
            stop(proc)
        sock.close()


if __name__ == '__main__':
    main()
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(frame: u64, total: u64) -> Limits {
        Limits {
            timeout_ms: 1_000,
            max_final_text_bytes: 32_768,
            max_frame_bytes: frame,
            max_evidence_bytes: total,
        }
    }

    fn read(bytes: &[u8], frame: u64, total: u64) -> (Result<()>, Vec<Value>) {
        let mut frames = Vec::new();
        let result = read_frames(bytes, limits(frame, total), &mut frames, &mut || Ok(()));
        (result, frames)
    }

    #[test]
    fn relayed_lines_become_bounded_frames() {
        let (result, frames) = read(b"{\"a\":1}\r\n\n{\"b\":2}", 64, 1024);
        result.unwrap();
        assert_eq!(frames, [json!({"a": 1}), json!({"b": 2})]);
        let (result, frames) = read(b"{\"a\":1}\n{\"b\":\"0123456789\"}\n", 12, 1024);
        assert!(result.is_err());
        assert_eq!(
            frames,
            [json!({"a": 1})],
            "frames before the failure are kept"
        );
        assert!(read(b"{\"a\":1}\n{\"b\":2}\n", 64, 10).0.is_err());
        assert!(read(b"not json\n", 64, 1024).0.is_err());
        assert!(read(b"[1]\n", 64, 1024).0.is_err());
    }

    fn turn(real: &str) -> GuestTurn {
        GuestTurn {
            runner_image_id: format!("sha256:{}", "d".repeat(64)),
            argv: vec!["pi".into(), "-p".into()],
            version_argv: vec!["pi".into(), "--version".into()],
            env: vec![("PI_OFFLINE".into(), "1".into())],
            home_files: vec![(
                ".pi/agent/auth.json".into(),
                br#"{"zai":{"type":"api_key","key":"pillbox-stub-0123456789abcdef"}}"#.to_vec(),
            )],
            stdin: b"hello".to_vec(),
            host: "api.z.ai".into(),
            stub_key: "pillbox-stub-0123456789abcdef".into(),
            real_key: real.into(),
        }
    }

    #[test]
    fn the_real_key_never_enters_guest_input_and_egress_is_one_host() {
        validate(&turn("zk-real")).unwrap();
        let mut leaked = turn("zk-real");
        leaked.home_files[0].1 = br#"{"zai":{"key":"zk-real"}}"#.to_vec();
        assert!(validate(&leaked).is_err());
        let mut leaked = turn("zk-real");
        leaked.stdin = b"my key is zk-real".to_vec();
        assert!(validate(&leaked).is_err());
        let mut leaked = turn("zk-real");
        leaked.env.push(("ZAI_API_KEY".into(), "zk-real".into()));
        assert!(validate(&leaked).is_err());
        let mut escape = turn("zk-real");
        escape.home_files[0].0 = "../etc/passwd".into();
        assert!(validate(&escape).is_err());

        let dir = tempfile::tempdir().unwrap();
        let spec = spec(
            dir.path(),
            dir.path(),
            dir.path(),
            dir.path(),
            "api.z.ai",
            1_000,
        );
        assert!(spec.shares.is_empty());
        let egress = spec.egress.unwrap();
        assert_eq!(egress.allowlist, ["api.z.ai"]);
        assert!(egress.refresh.is_none());
    }
}
