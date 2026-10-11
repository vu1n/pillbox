//! An invocation-owned microVM for one tool-free OpenCode 2 text turn.
//!
//! Same ownership, deadline and teardown as the Codex builder, with no shares. The
//! differences: OpenCode is not vault-capable, so its own credential store (from the
//! user's OpenCode home) is copied into the private rootfs and egress is forwarded
//! with no swap, only to the selected provider's host and the model catalog; and the
//! guest entrypoint is [`DRIVER`], which runs `opencode serve`, drives one prompt
//! over loopback HTTP and relays the event stream to the host as JSON lines.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{ensure, Context, Result};

use super::{
    check_live, fresh_directory, launch_prepared, prepare_generated_metadata, prepare_rootfs,
    remaining_ms, validate_image_id, OwnedVm, OwnershipSpec, VmLimits, GUEST_HOME, GUEST_RUNTIME,
    MAX_GENERATED_FILE, RPC_PORT,
};
use crate::paths::write_private_file;
use crate::sandbox::libkrun::{EgressSpec, VmSpec, VsockAttach};

/// OpenCode 2 keeps provider credentials (and its session history) in one SQLite
/// store; the WAL holds writes not yet checkpointed into it.
const CREDENTIAL_FILES: [&str; 3] = ["opencode.db", "opencode.db-wal", "opencode.db-shm"];
const GUEST_DATA: &str = ".local/share/opencode";
const MAX_CREDENTIAL_STORE: u64 = 512 * 1024 * 1024;
const MAX_HOSTS: usize = 8;

/// The caller has admitted the invocation and resolved the selection.
pub(crate) struct OpencodeInput {
    pub(crate) image_id: String,
    /// `execution::opencode::turn_document`, serialized.
    pub(crate) turn: Vec<u8>,
    /// The host directory holding `opencode.db` (the agent home's data dir).
    pub(crate) credential_dir: PathBuf,
    /// The provider's API host and the catalog host; nothing else resolves.
    pub(crate) hosts: Vec<String>,
}

fn validate(input: &OpencodeInput) -> Result<()> {
    validate_image_id(&input.image_id)?;
    ensure!(
        !input.turn.is_empty() && input.turn.len() <= MAX_GENERATED_FILE,
        "generated OpenCode turn is empty or too large"
    );
    serde_json::from_slice::<serde_json::Value>(&input.turn).context("parse OpenCode turn")?;
    ensure!(
        input.credential_dir.is_absolute(),
        "OpenCode credentials require an absolute host path"
    );
    ensure!(
        !input.hosts.is_empty()
            && input.hosts.len() <= MAX_HOSTS
            && input.hosts.iter().all(|host| {
                !host.is_empty()
                    && host.len() <= 253
                    && host
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-.".contains(&b))
            }),
        "OpenCode egress hosts are invalid"
    );
    Ok(())
}

/// Launch the VM, reporting `image_prepare`, `guest_prepare` and `vmm_spawn` as
/// they complete. The caller connects the RPC stream and reads the turn.
pub(crate) fn launch_staged(
    input: OpencodeInput,
    limits: VmLimits,
    cancelled: &dyn Fn() -> bool,
    stage: &mut dyn FnMut(&'static str),
) -> Result<OwnedVm> {
    limits.validate()?;
    validate(&input)?;
    let deadline = Instant::now()
        .checked_add(limits.max_duration)
        .context("VM deadline overflow")?;
    let (runtime, rootfs) = prepare_rootfs(&input.image_id, deadline, cancelled)?;
    stage("image_prepare");
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
    check_live(deadline, cancelled)?;
    stage("guest_prepare");
    // No credential channel: OpenCode authenticates to its provider itself, so the
    // MITM forwards with an empty swap set.
    let vm = launch_prepared(
        runtime,
        limits,
        deadline,
        cancelled,
        false,
        |owner, rpc, remaining| spec(&rootfs, &ca_dir, owner, rpc, &input.hosts, remaining),
    )?;
    stage("vmm_spawn");
    Ok(vm)
}

fn spec(
    rootfs: &Path,
    ca_dir: &Path,
    owner: &Path,
    rpc: &Path,
    hosts: &[String],
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
            format!("{GUEST_RUNTIME}/opencode_turn.py"),
        ],
        vsock: Some(VsockAttach {
            port: RPC_PORT,
            host_sock: rpc.to_string_lossy().into_owned(),
            listen: false,
        }),
        egress: Some(EgressSpec {
            allowlist: hosts.to_vec(),
            log_path: None,
            ca_dir: Some(ca_dir.to_string_lossy().into_owned()),
            local_forward_port: None,
            refresh: None,
        }),
        ownership: Some(OwnershipSpec {
            socket: owner.to_string_lossy().into_owned(),
            remaining_ms: Some(remaining_ms),
        }),
    }
}

fn prepare_guest(
    rootfs: &Path,
    input: &OpencodeInput,
    certificate: &[u8],
    limits: VmLimits,
    remaining_ms: u64,
) -> Result<()> {
    fresh_directory(rootfs, GUEST_HOME)?;
    fresh_directory(rootfs, GUEST_RUNTIME)?;
    fresh_directory(rootfs, "/workspace")?;
    let data = rootfs
        .join(GUEST_HOME.trim_start_matches('/'))
        .join(GUEST_DATA);
    fs::create_dir_all(&data)?;
    copy_credentials(&input.credential_dir, &data)?;
    let runtime = rootfs.join(GUEST_RUNTIME.trim_start_matches('/'));
    write_private_file(&runtime.join("opencode_turn.py"), DRIVER.as_bytes())?;
    write_private_file(&runtime.join("turn.json"), &input.turn)?;
    write_private_file(&runtime.join("ca.crt"), certificate)?;
    write_private_file(
        &runtime.join("limits.json"),
        &serde_json::to_vec(&serde_json::json!({
            "duration_ms": remaining_ms,
            "max_frame_bytes": limits.max_frame_bytes,
        }))?,
    )?;
    prepare_generated_metadata(rootfs, &[GUEST_RUNTIME, GUEST_HOME, "/workspace"])
}

/// Copy the credential store (never link it: the guest writes its session into the
/// copy, which is discarded with the VM). The store itself must exist.
fn copy_credentials(source: &Path, destination: &Path) -> Result<()> {
    let mut total = 0u64;
    for (index, name) in CREDENTIAL_FILES.iter().enumerate() {
        let path = source.join(name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && index > 0 => continue,
            Err(error) => return Err(error).context("read the OpenCode credential store"),
        };
        ensure!(
            metadata.is_file(),
            "OpenCode credential store entry {name} is not a regular file"
        );
        total += metadata.len();
        ensure!(
            total <= MAX_CREDENTIAL_STORE,
            "OpenCode credential store is larger than {MAX_CREDENTIAL_STORE} bytes"
        );
        write_private_file(&destination.join(name), &fs::read(&path)?)?;
    }
    Ok(())
}

// Guest driver: fixed trusted code. It never interprets model output; it forwards
// OpenCode's events verbatim and the host decides what they mean.
const DRIVER: &str = r#"import base64
import http.client
import json
import os
import signal
import socket
import subprocess
import time

RUNTIME = '/opt/pillbox-execution'
PORT = 4096
PASSWORD = 'pillbox-guest-loopback'
AUTH = 'Basic ' + base64.b64encode(('opencode:' + PASSWORD).encode()).decode()
TERMINAL = ('session.execution.succeeded', 'session.execution.failed',
            'session.execution.interrupted')


class Host:
    def __init__(self, sock, limit):
        self.sock = sock
        self.limit = limit

    def send(self, line):
        if len(line) > self.limit or b'\n' in line:
            raise RuntimeError('frame limit')
        self.sock.sendall(line + b'\n')

    def control(self, frame):
        self.send(json.dumps(frame, separators=(',', ':')).encode())


def remaining(deadline):
    left = deadline - time.monotonic()
    if left <= 0:
        raise RuntimeError('turn deadline')
    return left


def call(method, path, body, deadline):
    conn = http.client.HTTPConnection('127.0.0.1', PORT, timeout=min(remaining(deadline), 30))
    headers = {'Authorization': AUTH}
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        headers['Content-Type'] = 'application/json'
    try:
        conn.request(method, path, body=data, headers=headers)
        response = conn.getresponse()
        return response.status, response.read(1 << 20)
    finally:
        conn.close()


def fail(host, stage, status, body):
    host.control({'pillbox': 'error', 'stage': stage, 'status': status,
                  'body': body[:4096].decode('utf-8', 'replace')})
    raise RuntimeError(stage + ' failed')


def stop(proc):
    try:
        os.killpg(proc.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    proc.wait(timeout=2)


def main():
    with open(RUNTIME + '/limits.json', 'rb') as handle:
        limits = json.load(handle)
    with open(RUNTIME + '/turn.json', 'rb') as handle:
        turn = json.load(handle)
    deadline = time.monotonic() + limits['duration_ms'] / 1000
    env = {
        'HOME': '/home/pillbox',
        'PATH': '/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin',
        'LANG': 'C.UTF-8',
        'NODE_EXTRA_CA_CERTS': RUNTIME + '/ca.crt',
        'OPENCODE_SERVER_PASSWORD': PASSWORD,
        'OPENCODE_CONFIG_CONTENT': json.dumps(turn['config']),
        'OPENCODE_DISABLE_AUTOUPDATE': '1',
        'OPENCODE_DISABLE_PROJECT_CONFIG': '1',
    }
    for argv in (
        ['/usr/sbin/ip', 'link', 'set', 'eth0', 'up'],
        ['/usr/sbin/ip', 'addr', 'add', '10.0.2.15/24', 'dev', 'eth0'],
        ['/usr/sbin/ip', 'route', 'add', 'default', 'via', '10.0.2.2'],
    ):
        subprocess.run(argv, env=env, check=True, stdin=subprocess.DEVNULL,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                       timeout=min(remaining(deadline), 5))
    with open('/etc/resolv.conf', 'w') as handle:
        handle.write('nameserver 10.0.2.2\n')
    sock = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
    sock.settimeout(min(remaining(deadline), 10))
    sock.connect((2, 1067))
    sock.settimeout(None)
    host = Host(sock, limits['max_frame_bytes'])
    proc = subprocess.Popen(['opencode', 'serve', '--port', str(PORT), '--hostname', '127.0.0.1'],
                            cwd='/workspace', env=env, stdin=subprocess.DEVNULL,
                            stdout=subprocess.DEVNULL, close_fds=True,
                            start_new_session=True)
    try:
        while True:
            if proc.poll() is not None:
                raise RuntimeError('opencode serve exited')
            try:
                status, body = call('GET', '/api/info', None, deadline)
                if status == 200:
                    break
            except OSError:
                pass
            time.sleep(0.2)
        host.control({'pillbox': 'info', 'info': json.loads(body)})
        events = http.client.HTTPConnection('127.0.0.1', PORT, timeout=remaining(deadline))
        events.request('GET', '/api/event', headers={'Authorization': AUTH,
                                                     'Accept': 'text/event-stream'})
        stream = events.getresponse()
        if stream.status != 200:
            fail(host, 'events', stream.status, stream.read(4096))
        while True:
            line = stream.fp.readline(limits['max_frame_bytes'] + 8)
            if not line:
                raise RuntimeError('event stream closed')
            if line.startswith(b'data: ') and b'"server.connected"' in line:
                break
        status, body = call('POST', '/api/session', turn['session'], deadline)
        if status != 200:
            fail(host, 'session', status, body)
        session = json.loads(body)['data']['id']
        host.control({'pillbox': 'session', 'id': session})
        status, body = call('POST', '/api/session/' + session + '/prompt',
                            {'text': turn['text']}, deadline)
        if status != 200:
            fail(host, 'prompt', status, body)
        while True:
            remaining(deadline)
            line = stream.fp.readline(limits['max_frame_bytes'] + 8)
            if not line:
                raise RuntimeError('event stream closed')
            if not line.startswith(b'data: '):
                continue
            payload = line[6:].rstrip(b'\r\n')
            host.send(payload)
            event = json.loads(payload)
            if event.get('type') in TERMINAL and event.get('data', {}).get('sessionID') == session:
                return
    finally:
        stop(proc)
        sock.close()

if __name__ == '__main__':
    main()
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn input(dir: &Path) -> OpencodeInput {
        OpencodeInput {
            image_id: format!("sha256:{}", "a".repeat(64)),
            turn: br#"{"config":{},"session":{},"text":"hi"}"#.to_vec(),
            credential_dir: dir.to_path_buf(),
            hosts: vec!["api.z.ai".into(), "models.dev".into()],
        }
    }

    #[test]
    fn the_vm_has_no_shares_no_swap_and_only_the_resolved_hosts() {
        let value = input(Path::new("/host/opencode"));
        validate(&value).unwrap();
        let spec = spec(
            Path::new("/private/rootfs"),
            Path::new("/private/ca"),
            Path::new("/private/owner"),
            Path::new("/private/rpc"),
            &value.hosts,
            100,
        );
        assert!(spec.shares.is_empty());
        let egress = spec.egress.unwrap();
        assert_eq!(egress.allowlist, ["api.z.ai", "models.dev"]);
        assert!(egress.refresh.is_none() && egress.local_forward_port.is_none());
        assert!(spec.ownership.unwrap().is_bounded());
        let mut wide = input(Path::new("/host/opencode"));
        wide.hosts = vec!["*.example.com".into()];
        assert!(validate(&wide).is_err());
        let mut relative = input(Path::new("opencode"));
        relative.hosts = vec!["api.z.ai".into()];
        assert!(validate(&relative).is_err());
    }

    #[test]
    fn the_credential_store_is_copied_and_required() {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        assert!(copy_credentials(source.path(), destination.path()).is_err());
        fs::write(source.path().join("opencode.db"), b"db").unwrap();
        fs::write(source.path().join("opencode.db-wal"), b"wal").unwrap();
        copy_credentials(source.path(), destination.path()).unwrap();
        assert_eq!(
            fs::read(destination.path().join("opencode.db-wal")).unwrap(),
            b"wal"
        );
        assert!(!destination.path().join("opencode.db-shm").exists());
    }
}
