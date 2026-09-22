//! Sealed, offline verifier supervision. VM creation and its outer deadline are
//! owned by the caller; only the root supervisor may write the report channel.

use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};

use super::{canonical_digest, identity, valid_digest, Verifier, MAX_TIMEOUT_MS};

pub(crate) const SUPERVISOR_PATH: &str = "/opt/pillbox-execution/verifier-supervisor.py";
pub(crate) const SOURCE_PATH: &str = "/opt/pillbox-execution/verifier-source.py";
pub(crate) const CONFIG_PATH: &str = "/opt/pillbox-execution/verifier-config.json";
pub(crate) const INPUT_PATH: &str = "/opt/pillbox-execution/input-tree";
pub(crate) const REPORT_PORT: u32 = 1067;
const VERSION: &str = "pillbox.verifier/1";
const MAX_OUTPUT_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VerifierConfiguration {
    pub(crate) version: String,
    pub(crate) verifier_id: String,
    pub(crate) run_id: String,
    pub(crate) definition_digest: String,
    pub(crate) output_id: String,
    pub(crate) result_digest: String,
    pub(crate) result_snapshot_digest: String,
    pub(crate) timeout_ms: u64,
    pub(crate) max_output_bytes: u64,
}

impl VerifierConfiguration {
    fn validate(&self) -> Result<()> {
        ensure!(self.version == VERSION, "unsupported verifier version");
        for value in [&self.verifier_id, &self.run_id, &self.output_id] {
            identity(value)?;
        }
        for value in [
            &self.definition_digest,
            &self.result_digest,
            &self.result_snapshot_digest,
        ] {
            ensure!(valid_digest(value), "invalid verifier digest");
        }
        ensure!(
            (1..=MAX_TIMEOUT_MS).contains(&self.timeout_ms),
            "invalid verifier timeout"
        );
        ensure!(
            (1..=MAX_OUTPUT_BYTES).contains(&self.max_output_bytes),
            "invalid verifier output bound"
        );
        Ok(())
    }

    /// Includes both base64 paddings, the closed identity envelope and newline.
    /// A receiver must stop reading at this bound, before passing bytes to us.
    pub(crate) fn max_report_bytes(&self) -> usize {
        (self.max_output_bytes.min(MAX_OUTPUT_BYTES).div_ceil(3) * 4) as usize + 4096
    }
}

pub(crate) fn configuration(
    verifier: &Verifier,
    output_id: &str,
    result_digest: &str,
    result_snapshot_digest: &str,
) -> Result<VerifierConfiguration> {
    let definition = &verifier.definition;
    ensure!(
        definition.runtime == "python3",
        "unsupported verifier runtime"
    );
    ensure!(
        !definition.source.is_empty()
            && definition.source.len() <= 64 * 1024
            && !definition.source.contains('\0'),
        "invalid sealed verifier source"
    );
    ensure!(
        canonical_digest(definition)? == verifier.definition_digest,
        "verifier definition digest mismatch"
    );
    let config = VerifierConfiguration {
        version: VERSION.to_owned(),
        verifier_id: verifier.verifier_id.clone(),
        run_id: verifier.run_id.clone(),
        definition_digest: verifier.definition_digest.clone(),
        output_id: output_id.to_owned(),
        result_digest: result_digest.to_owned(),
        result_snapshot_digest: result_snapshot_digest.to_owned(),
        timeout_ms: definition.timeout_ms,
        max_output_bytes: definition.max_output_bytes,
    };
    config.validate()?;
    Ok(config)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VerifierOutcome {
    Passed,
    Failed,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct VerifierObservation {
    pub(crate) exit_code: Option<i32>,
    pub(crate) signal: Option<i32>,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) timed_out: bool,
    pub(crate) output_limited: bool,
    pub(crate) outcome: VerifierOutcome,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Report {
    version: String,
    verifier_id: String,
    run_id: String,
    definition_digest: String,
    output_id: String,
    result_digest: String,
    result_snapshot_digest: String,
    // Required nullable fields: Option alone would also accept missing fields.
    exit_code: serde_json::Value,
    signal: serde_json::Value,
    stdout_base64: String,
    stderr_base64: String,
    timed_out: bool,
    output_limited: bool,
}

/// Parse exactly one newline-terminated supervisor frame, never child stdout.
/// Receiving another frame, noise, EOF without a frame or a malformed report is
/// an infrastructure failure, not a failed/passed verifier observation.
pub(crate) fn parse_report(
    bytes: &[u8],
    expected: &VerifierConfiguration,
) -> Result<VerifierObservation> {
    expected.validate()?;
    ensure!(
        bytes.len() <= expected.max_report_bytes(),
        "verifier report too large"
    );
    let json = bytes
        .strip_suffix(b"\n")
        .context("unterminated verifier report")?;
    ensure!(
        !json.is_empty() && !json.contains(&b'\n') && !json.contains(&b'\r'),
        "expected one verifier report frame"
    );
    let report: Report = serde_json::from_slice(json).context("invalid verifier report")?;
    ensure!(
        report.version == expected.version
            && report.verifier_id == expected.verifier_id
            && report.run_id == expected.run_id
            && report.definition_digest == expected.definition_digest
            && report.output_id == expected.output_id
            && report.result_digest == expected.result_digest
            && report.result_snapshot_digest == expected.result_snapshot_digest,
        "verifier report identity mismatch"
    );
    let status = |value: &serde_json::Value, min, max| -> Result<Option<i32>> {
        if value.is_null() {
            return Ok(None);
        }
        let code = value.as_i64().context("noninteger verifier status")?;
        ensure!((min..=max).contains(&code), "invalid verifier status");
        Ok(Some(code as i32))
    };
    let exit_code = status(&report.exit_code, 0, 255)?;
    let signal = status(&report.signal, 1, 64)?;
    ensure!(
        exit_code.is_some() != signal.is_some(),
        "nonterminal verifier status"
    );
    let stdout = STANDARD
        .decode(report.stdout_base64)
        .context("invalid stdout base64")?;
    let stderr = STANDARD
        .decode(report.stderr_base64)
        .context("invalid stderr base64")?;
    ensure!(
        stdout.len() + stderr.len() <= expected.max_output_bytes as usize,
        "verifier output exceeds combined bound"
    );
    ensure!(
        !report.output_limited || stdout.len() + stderr.len() == expected.max_output_bytes as usize,
        "inconsistent verifier output limit flag"
    );
    let outcome = if exit_code == Some(0) && !report.timed_out && !report.output_limited {
        VerifierOutcome::Passed
    } else {
        VerifierOutcome::Failed
    };
    Ok(VerifierObservation {
        exit_code,
        signal,
        stdout,
        stderr,
        timed_out: report.timed_out,
        output_limited: report.output_limited,
        outcome,
    })
}

/// Root-owned script for a fresh offline VM. The host accepts its connection
/// once and closes the listener before test code starts. There are no inbound
/// control messages: either EOF or any received byte is fatal. The caller owns
/// VM termination even after a successful report, including escaped descendants.
pub(crate) fn guest_script() -> &'static str {
    GUEST_SCRIPT
}

const GUEST_SCRIPT: &str = r#"import base64
import ctypes
import hashlib
import json
import os
import selectors
import signal
import socket
import stat
import subprocess
import time

ROOT = '/opt/pillbox-execution'
REPORT_PORT = 1067
IDENTITIES = ('version', 'verifier_id', 'run_id', 'definition_digest',
              'output_id', 'result_digest', 'result_snapshot_digest')

def require(condition, message):
    if not condition:
        raise RuntimeError(message)

def sha(data):
    return 'sha256:' + hashlib.sha256(data).hexdigest()

def canonical(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True,
                      separators=(',', ':')).encode('utf-8')

def closed_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, 'duplicate config field')
        result[key] = value
    return result

def sealed_file(path, mode, limit):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    with os.fdopen(fd, 'rb') as source:
        meta = os.fstat(source.fileno())
        require(stat.S_ISREG(meta.st_mode) and meta.st_uid == 0 and
                stat.S_IMODE(meta.st_mode) == mode and 0 < meta.st_size <= limit,
                'unsealed verifier file')
        content = source.read(limit + 1)
        require(len(content) == meta.st_size, 'verifier file changed')
        return content

def load_configuration():
    config = json.loads(sealed_file(ROOT + '/verifier-config.json', 0o400, 8192),
                        object_pairs_hook=closed_object)
    require(type(config) is dict and
            set(config) == set(IDENTITIES) | {'timeout_ms', 'max_output_bytes'},
            'invalid config shape')
    require(config['version'] == 'pillbox.verifier/1', 'invalid config version')
    for key in ('verifier_id', 'run_id', 'output_id'):
        value = config[key]
        require(type(value) is str and 1 <= len(value) <= 128 and
                all(c in 'abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_' for c in value),
                'invalid config identity')
    for key in ('definition_digest', 'result_digest', 'result_snapshot_digest'):
        value = config[key]
        require(type(value) is str and len(value) == 71 and value.startswith('sha256:') and
                all(c in '0123456789abcdef' for c in value[7:]), 'invalid config digest')
    for key, maximum in (('timeout_ms', 3600000), ('max_output_bytes', 1048576)):
        require(type(config[key]) is int and 1 <= config[key] <= maximum, 'invalid config limit')
    source = sealed_file(ROOT + '/verifier-source.py', 0o444, 65536).decode('utf-8')
    require('\x00' not in source, 'invalid verifier source')
    definition = dict(runtime='python3', source=source, timeout_ms=config['timeout_ms'],
                      max_output_bytes=config['max_output_bytes'])
    require(sha(canonical(definition)) == config['definition_digest'], 'verifier definition mismatch')
    return config

def libc_call(name, *args):
    library = ctypes.CDLL(None, use_errno=True)
    function = getattr(library, name)
    if function(*args) != 0:
        raise OSError(ctypes.get_errno(), name + ' failed')

def prepare_workspace(config):
    for path, options in ((b'/workspace', b'size=134217728,mode=0700'),
                          (b'/tmp', b'size=67108864,mode=1777')):
        meta = os.lstat(path)
        require(stat.S_ISDIR(meta.st_mode) and meta.st_uid == 0, 'invalid scratch mountpoint')
        # MS_NOSUID | MS_NODEV. Scratch must not touch the host-backed rootfs.
        libc_call('mount', b'tmpfs', path, b'tmpfs', ctypes.c_ulong(2 | 4), options)
    manifest = []
    total = 0
    input_root = ROOT + '/input-tree'
    require(stat.S_ISDIR(os.lstat(input_root).st_mode), 'invalid input tree')
    for directory, dirs, files in os.walk(input_root, followlinks=False):
        for name in dirs:
            meta = os.lstat(os.path.join(directory, name))
            require(stat.S_ISDIR(meta.st_mode) and meta.st_uid == 0, 'invalid input directory')
        for name in files:
            path = os.path.join(directory, name)
            relative = os.path.relpath(path, input_root)
            require(relative.isascii() and len(relative) <= 1024 and len(manifest) < 4096,
                    'input path/count limit exceeded')
            fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
            with os.fdopen(fd, 'rb') as source:
                meta = os.fstat(source.fileno())
                require(stat.S_ISREG(meta.st_mode) and meta.st_uid == 0 and
                        meta.st_size <= 8388608 and total + meta.st_size <= 67108864,
                        'invalid input file or size')
                target = '/workspace/' + relative
                os.makedirs(os.path.dirname(target), mode=0o700, exist_ok=True)
                executable = bool(meta.st_mode & 0o111)
                output = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                                 0o700 if executable else 0o600)
                digest = hashlib.sha256()
                size = 0
                with os.fdopen(output, 'wb') as destination:
                    while True:
                        chunk = source.read(65536)
                        if not chunk:
                            break
                        size += len(chunk)
                        require(size <= meta.st_size, 'input file grew')
                        digest.update(chunk)
                        destination.write(chunk)
                require(size == meta.st_size, 'input file shrank')
                total += size
                manifest.append(dict(executable=executable, path=relative,
                                     sha256='sha256:' + digest.hexdigest()))
    manifest.sort(key=lambda entry: entry['path'])
    require(sha(canonical(manifest)) == config['result_snapshot_digest'], 'input snapshot mismatch')
    for directory, dirs, files in os.walk('/workspace'):
        for name in files + dirs:
            os.chown(os.path.join(directory, name), 65534, 65534, follow_symlinks=False)
    os.chown('/workspace', 65534, 65534)
    # MS_REMOUNT | MS_RDONLY. Failure is fatal, before any untrusted child.
    libc_call('mount', None, b'/', None, ctypes.c_ulong(32 | 1), None)
    require(os.statvfs('/').f_flag & os.ST_RDONLY, 'root is still writable')

def drop_privileges():
    os.setgroups([])
    os.setgid(65534)
    os.setuid(65534)
    libc_call('prctl', 38, 1, 0, 0, 0)  # PR_SET_NO_NEW_PRIVS
    require(os.getuid() == 65534 and os.getgid() == 65534 and not os.getgroups(),
            'privilege drop failed')

def kill_group(child):
    try:
        os.killpg(child.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    child.wait(timeout=5)

def check_parent(control):
    try:
        data = control.recv(1)
    except BlockingIOError:
        return
    raise RuntimeError('parent closed' if not data else 'invalid parent control')

def observe(child, control, config):
    deadline = time.monotonic() + config['timeout_ms'] / 1000
    outputs = {'stdout': bytearray(), 'stderr': bytearray()}
    used = 0
    timed_out = False
    output_limited = False
    try:
        with selectors.DefaultSelector() as poll:
            poll.register(control, selectors.EVENT_READ, 'parent')
            for name, stream in (('stdout', child.stdout), ('stderr', child.stderr)):
                os.set_blocking(stream.fileno(), False)
                poll.register(stream, selectors.EVENT_READ, name)
            while True:
                check_parent(control)
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    timed_out = True
                    break
                if child.poll() is not None and len(poll.get_map()) == 1:
                    break
                for key, _ in poll.select(min(remaining, 0.05)):
                    if key.data == 'parent':
                        check_parent(control)
                        continue
                    chunk = os.read(key.fd, min(65536, config['max_output_bytes'] - used + 1))
                    if not chunk:
                        poll.unregister(key.fileobj)
                        continue
                    available = config['max_output_bytes'] - used
                    outputs[key.data].extend(chunk[:available])
                    used += min(len(chunk), available)
                    if len(chunk) > available:
                        output_limited = True
                        break
                if output_limited:
                    break
    finally:
        # Also kill same-group descendants after an ordinary parent exit. The
        # caller destroys the VM to cover descendants that created a new group.
        kill_group(child)
        child.stdout.close()
        child.stderr.close()
    check_parent(control)
    code = child.returncode
    require(code is not None, 'missing child wait status')
    report = {key: config[key] for key in IDENTITIES}
    report.update(exit_code=code if code >= 0 else None, signal=-code if code < 0 else None,
                  stdout_base64=base64.b64encode(outputs['stdout']).decode('ascii'),
                  stderr_base64=base64.b64encode(outputs['stderr']).decode('ascii'),
                  timed_out=timed_out, output_limited=output_limited)
    return report

def send_report(control, report):
    pending = memoryview(canonical(report) + b'\n')
    deadline = time.monotonic() + 5
    with selectors.DefaultSelector() as poll:
        poll.register(control, selectors.EVENT_READ | selectors.EVENT_WRITE)
        while pending:
            require(time.monotonic() < deadline, 'report write deadline exceeded')
            for _, ready in poll.select(max(0, deadline - time.monotonic())):
                if ready & selectors.EVENT_READ:
                    check_parent(control)
                if ready & selectors.EVENT_WRITE:
                    try:
                        sent = control.send(pending)
                    except BlockingIOError:
                        continue
                    require(sent > 0, 'report connection lost')
                    pending = pending[sent:]

def main():
    require(os.geteuid() == 0, 'supervisor must be root')
    os.umask(0o077)
    libc_call('prctl', 4, 0, 0, 0, 0)  # PR_SET_DUMPABLE=0 protects report FD via procfs.
    config = load_configuration()
    with socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM) as control:
        control.set_inheritable(False)
        control.settimeout(5)
        control.connect((2, REPORT_PORT))
        control.setblocking(False)
        check_parent(control)
        prepare_workspace(config)
        check_parent(control)
        child = subprocess.Popen(['/usr/bin/python3', '-I', '-S', ROOT + '/verifier-source.py'],
                                 cwd='/workspace', stdin=subprocess.DEVNULL,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                 env={'PATH': '/usr/bin:/bin', 'HOME': '/workspace', 'TMPDIR': '/tmp'},
                                 close_fds=True, start_new_session=True, preexec_fn=drop_privileges)
        send_report(control, observe(child, control, config))

if __name__ == '__main__':
    main()
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::{digest, VerifierDefinition};
    use serde_json::{json, Value};
    use std::io::Write;
    use std::process::{Command, Stdio};

    fn fixture() -> (Verifier, VerifierConfiguration) {
        let definition = VerifierDefinition {
            runtime: "python3".to_owned(),
            source: "assert True\n".to_owned(),
            timeout_ms: 1000,
            max_output_bytes: 128,
        };
        let verifier = Verifier {
            verifier_id: "verifier-1".to_owned(),
            run_id: "run-1".to_owned(),
            definition_digest: canonical_digest(&definition).unwrap(),
            definition,
        };
        let config =
            configuration(&verifier, "output-1", &digest(b"result"), &digest(b"[]")).unwrap();
        (verifier, config)
    }

    fn passing(config: &VerifierConfiguration) -> Value {
        json!({
            "version": config.version,
            "verifier_id": config.verifier_id,
            "run_id": config.run_id,
            "definition_digest": config.definition_digest,
            "output_id": config.output_id,
            "result_digest": config.result_digest,
            "result_snapshot_digest": config.result_snapshot_digest,
            "exit_code": 0,
            "signal": null,
            "stdout_base64": STANDARD.encode(b"ok\0\xff"),
            "stderr_base64": "",
            "timed_out": false,
            "output_limited": false,
        })
    }

    fn frame(value: &Value) -> Vec<u8> {
        let mut bytes = serde_json::to_vec(value).unwrap();
        bytes.push(b'\n');
        bytes
    }

    #[test]
    fn configuration_seals_source_runtime_identities_and_limits() {
        let (verifier, config) = fixture();
        assert_eq!(config.definition_digest, verifier.definition_digest);
        let mut stale = verifier.clone();
        stale.definition.source.push_str("raise Exception()");
        assert!(configuration(&stale, "out", &digest(b"r"), &digest(b"s")).is_err());
        for mutate in [
            |v: &mut Verifier| v.definition.runtime = "shell".to_owned(),
            |v: &mut Verifier| v.definition.timeout_ms = 0,
            |v: &mut Verifier| v.definition.timeout_ms = MAX_TIMEOUT_MS + 1,
            |v: &mut Verifier| v.definition.max_output_bytes = 0,
            |v: &mut Verifier| v.definition.max_output_bytes = MAX_OUTPUT_BYTES + 1,
            |v: &mut Verifier| v.definition.source = "\0".to_owned(),
            |v: &mut Verifier| v.definition.source = "a".repeat(65537),
            |v: &mut Verifier| v.run_id = "../../run".to_owned(),
        ] {
            let mut changed = verifier.clone();
            mutate(&mut changed);
            changed.definition_digest = canonical_digest(&changed.definition).unwrap();
            assert!(configuration(&changed, "out", &digest(b"r"), &digest(b"s")).is_err());
        }
        assert!(configuration(&verifier, "out/invalid", &digest(b"r"), &digest(b"s")).is_err());
        assert!(configuration(&verifier, "out", "sha256:bad", &digest(b"s")).is_err());
        assert!(configuration(&verifier, "out", &digest(b"r"), "sha256:bad").is_err());
    }

    #[test]
    fn report_binds_every_identity_and_rejects_forged_outcome() {
        let (_, config) = fixture();
        let report = passing(&config);
        let observation = parse_report(&frame(&report), &config).unwrap();
        assert_eq!(observation.outcome, VerifierOutcome::Passed);
        assert_eq!(observation.stdout, b"ok\0\xff");
        for key in [
            "version",
            "verifier_id",
            "run_id",
            "definition_digest",
            "output_id",
            "result_digest",
            "result_snapshot_digest",
        ] {
            let mut changed = report.clone();
            changed[key] = json!("forged");
            assert!(parse_report(&frame(&changed), &config).is_err(), "{key}");
        }
        let mut changed = report;
        changed["outcome"] = json!("passed");
        assert!(parse_report(&frame(&changed), &config).is_err());
    }

    #[test]
    fn report_requires_closed_single_frame_and_all_fields() {
        let (_, config) = fixture();
        let report = passing(&config);
        for key in report.as_object().unwrap().keys() {
            let mut changed = report.clone();
            changed.as_object_mut().unwrap().remove(key);
            assert!(
                parse_report(&frame(&changed), &config).is_err(),
                "missing {key}"
            );
        }
        let good = frame(&report);
        for bad in [
            vec![],
            b"\n".to_vec(),
            good[..good.len() - 1].to_vec(),
            [good.clone(), good.clone()].concat(),
            [b"noise".to_vec(), good.clone()].concat(),
            [good.clone(), b"noise".to_vec()].concat(),
            [b"{\"exit_code\":0,".to_vec(), good[1..].to_vec()].concat(),
            vec![b' '; config.max_report_bytes() + 1],
        ] {
            assert!(parse_report(&bad, &config).is_err());
        }
    }

    #[test]
    fn status_is_terminal_and_failure_flags_can_never_pass() {
        let (_, config) = fixture();
        for (exit, signal) in [
            (Value::Null, Value::Null),
            (json!(0), json!(9)),
            (json!(-1), Value::Null),
            (json!(256), Value::Null),
            (json!(0.0), Value::Null),
            (json!("0"), Value::Null),
            (Value::Null, json!(0)),
            (Value::Null, json!(65)),
        ] {
            let mut report = passing(&config);
            report["exit_code"] = exit;
            report["signal"] = signal;
            assert!(parse_report(&frame(&report), &config).is_err());
        }
        for (exit, signal, timeout, limited) in [
            (Some(17), None, false, false),
            (None, Some(9), false, false),
            (Some(0), None, true, false),
            (Some(0), None, false, true),
            (None, Some(9), true, true),
        ] {
            let mut report = passing(&config);
            report["exit_code"] = json!(exit);
            report["signal"] = json!(signal);
            report["timed_out"] = json!(timeout);
            report["output_limited"] = json!(limited);
            if limited {
                report["stdout_base64"] = json!(STANDARD.encode(vec![0; 128]));
            }
            assert_eq!(
                parse_report(&frame(&report), &config).unwrap().outcome,
                VerifierOutcome::Failed
            );
        }
    }

    #[test]
    fn output_is_canonical_base64_and_combined_bounded() {
        let (_, config) = fixture();
        for malformed in ["YQ", "YQ==\n", "YR==", "_w==", "====", "🎈"] {
            let mut report = passing(&config);
            report["stdout_base64"] = json!(malformed);
            assert!(
                parse_report(&frame(&report), &config).is_err(),
                "{malformed}"
            );
        }
        let mut report = passing(&config);
        report["stdout_base64"] = json!(STANDARD.encode(vec![0; 64]));
        report["stderr_base64"] = json!(STANDARD.encode(vec![0; 64]));
        assert!(parse_report(&frame(&report), &config).is_ok());
        report["stderr_base64"] = json!(STANDARD.encode(vec![0; 65]));
        assert!(parse_report(&frame(&report), &config).is_err());
        report["stderr_base64"] = json!("");
        report["output_limited"] = json!(true);
        assert!(parse_report(&frame(&report), &config).is_err());
    }

    // Executes only the portable monitor with ordinary local fixture children.
    // Linux mounts, vsock and privilege changes require the separate real VM gate.
    fn python_fixture(body: &str, config: &VerifierConfiguration) -> Vec<u8> {
        let harness = format!(
            "import sys, json, socket, subprocess, os, signal, ast\n\
             payload = json.loads(sys.stdin.read())\n\
             ast.parse(payload['script'])\n\
             scope = {{'__name__': 'fixture'}}\n\
             exec(compile(payload['script'], '<supervisor>', 'exec'), scope)\n\
             config = payload['config']\n\
             control, host = socket.socketpair()\n\
             control.setblocking(False)\n\
             control.set_inheritable(False)\n\
             def child(source): return subprocess.Popen([sys.executable, '-I', '-S', '-c', source], stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, close_fds=True, start_new_session=True, env={{'PATH': '/usr/bin:/bin'}})\n\
             {body}\n"
        );
        let mut process = Command::new("/usr/bin/python3")
            .args(["-I", "-S", "-c", &harness])
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        process
            .stdin
            .take()
            .unwrap()
            .write_all(
                &serde_json::to_vec(&json!({"script": guest_script(), "config": config})).unwrap(),
            )
            .unwrap();
        let output = process.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    #[test]
    fn supervisor_captures_real_exit_binary_output_and_not_child_reports() {
        let (_, mut config) = fixture();
        config.max_output_bytes = 512;
        let bytes = python_fixture(
            r#"source = 'import os, sys; os.write(1, b\'{"outcome":"passed"}\\n\\xff\\x00\'); os.write(2, b"err"); sys.exit(17)'
report = scope['observe'](child(source), control, config)
scope['send_report'](control, report)
control.close()
sys.stdout.buffer.write(host.recv(8192))"#,
            &config,
        );
        let result = parse_report(&bytes, &config).unwrap();
        assert_eq!(result.exit_code, Some(17));
        assert_eq!(result.outcome, VerifierOutcome::Failed);
        assert_eq!(result.stdout, b"{\"outcome\":\"passed\"}\n\xff\0");
        assert_eq!(result.stderr, b"err");
    }

    #[test]
    fn supervisor_kills_and_reaps_deadline_and_output_overflow() {
        let (_, config) = fixture();
        for (source, timeout, overflow) in [
            ("import time; time.sleep(60)", true, false),
            (
                "import os; os.write(1, b'x' * 10000); import time; time.sleep(60)",
                false,
                true,
            ),
            (
                "import os, signal; os.kill(os.getpid(), signal.SIGTERM)",
                false,
                false,
            ),
        ] {
            let body = format!(
                "process = child({source:?})\nreport = scope['observe'](process, control, config)\nassert process.poll() is not None\nprint(json.dumps(report))"
            );
            let result = parse_report(&python_fixture(&body, &config), &config).unwrap();
            assert_eq!(result.timed_out, timeout);
            assert_eq!(result.output_limited, overflow);
            assert_eq!(result.outcome, VerifierOutcome::Failed);
            assert_eq!(
                result.signal,
                Some(if timeout || overflow { 9 } else { 15 })
            );
        }
    }

    #[test]
    fn supervisor_treats_parent_eof_or_data_as_fatal_and_closes_report_fd_in_child() {
        let (_, config) = fixture();
        for action in ["host.close()", "host.send(b'x')"] {
            let body = format!(
                "process = child('import time; time.sleep(60)')\n{action}\ntry:\n    scope['observe'](process, control, config)\n    raise AssertionError('accepted parent loss/control')\nexcept RuntimeError as error:\n    assert str(error) in ('parent closed', 'invalid parent control')\nassert process.poll() is not None\n"
            );
            assert!(python_fixture(&body, &config).is_empty());
        }
        let body = r#"source = 'import os;\ntry: os.fstat(' + str(control.fileno()) + ')\nexcept OSError: raise SystemExit(0)\nraise SystemExit(99)'
print(json.dumps(scope['observe'](child(source), control, config)))"#;
        let result = parse_report(&python_fixture(body, &config), &config).unwrap();
        assert_eq!(result.exit_code, Some(0));
    }

    #[test]
    fn guest_validates_sealed_definition_and_exposes_matching_paths() {
        let (verifier, config) = fixture();
        let body = format!(
            r#"source = {}
def sealed(path, mode, limit):
    if path.endswith('/verifier-source.py'):
        assert mode == 0o444 and limit == 65536
        return source.encode('utf-8')
    assert path.endswith('/verifier-config.json') and mode == 0o400
    return json.dumps(config).encode('utf-8')
scope['sealed_file'] = sealed
assert scope['load_configuration']() == config
source += '# changed'
try:
    scope['load_configuration']()
    raise AssertionError('accepted changed source')
except RuntimeError as error:
    assert str(error) == 'verifier definition mismatch'
print(json.dumps([scope['ROOT'] + '/verifier-supervisor.py', scope['ROOT'] + '/verifier-source.py', scope['ROOT'] + '/verifier-config.json', scope['ROOT'] + '/input-tree', scope['REPORT_PORT']]))"#,
            serde_json::to_string(&verifier.definition.source).unwrap(),
        );
        let paths: Value = serde_json::from_slice(&python_fixture(&body, &config)).unwrap();
        assert_eq!(
            paths,
            json!([
                SUPERVISOR_PATH,
                SOURCE_PATH,
                CONFIG_PATH,
                INPUT_PATH,
                REPORT_PORT
            ])
        );
    }

    #[test]
    fn guest_privilege_transition_fails_closed_when_an_operation_fails() {
        let (_, config) = fixture();
        let body = r#"calls = []
class FakeOS:
    def setgroups(self, value): calls.append(('groups', value))
    def setgid(self, value): calls.append(('gid', value))
    def setuid(self, value): calls.append(('uid', value))
    def getuid(self): return 65534
    def getgid(self): return 65534
    def getgroups(self): return []
scope['os'] = FakeOS()
scope['libc_call'] = lambda *args: calls.append(args)
scope['drop_privileges']()
assert calls == [('groups', []), ('gid', 65534), ('uid', 65534), ('prctl', 38, 1, 0, 0, 0)]
def reject(*args): raise OSError('fixture privilege failure')
scope['libc_call'] = reject
try:
    scope['drop_privileges']()
    raise AssertionError('ignored failed no_new_privs')
except OSError as error:
    assert str(error) == 'fixture privilege failure'
scope['os'].setuid = reject
try:
    scope['drop_privileges']()
    raise AssertionError('ignored failed setuid')
except OSError as error:
    assert str(error) == 'fixture privilege failure'"#;
        assert!(python_fixture(body, &config).is_empty());
    }
}
