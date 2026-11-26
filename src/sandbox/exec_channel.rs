//! The sandbox exec channel: one framed request/response exchange per command over
//! a byte stream (the libkrun sandbox's vsock, bridged to a host unix socket).
//!
//! Compiled in every build: the guest side ([`serve_exec`], behind the hidden
//! `pillbox exec-host`) runs in the in-guest `--no-default-features` binary, the
//! host side ([`run_exec`]) in the CLI.
//!
//! Wire: `[tag:u8][len:u32 BE][payload]`, the framing of `attach::frame`. The host
//! sends one `Request` (JSON `{argv, cwd}`); the guest answers with any number of
//! `Stdout`/`Stderr` chunks, then exactly one `Exit(i32 BE)`, or a single
//! `Error(utf8)` when the command could not be started. One connection carries one
//! command; concurrent commands use concurrent connections.

use std::io::{self, Read, Write};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Cap on one frame's payload. The length prefix is peer-controlled, so `decode`
/// refuses anything larger instead of allocating it. Output chunks are
/// [`CHUNK_BYTES`]; only a request's argv could approach this.
const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

/// Read size for the child's stdout/stderr pipes — one output frame at most.
const CHUNK_BYTES: usize = 64 * 1024;

// Frame tags. Stable; add new tags at the end.
const T_REQUEST: u8 = 1;
const T_STDOUT: u8 = 2;
const T_STDERR: u8 = 3;
const T_EXIT: u8 = 4;
const T_ERROR: u8 = 5;

/// What the host asks the guest to run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ExecRequest {
    /// `argv[0]` is resolved on the guest's `PATH`. Never empty.
    pub(crate) argv: Vec<String>,
    /// Working directory in the guest; `None` keeps `exec-host`'s own.
    #[serde(default)]
    pub(crate) cwd: Option<String>,
}

/// One frame on the exec channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExecFrame {
    /// Host → guest, first and only host frame.
    Request(ExecRequest),
    /// Guest → host: a chunk of the command's stdout.
    Stdout(Vec<u8>),
    /// Guest → host: a chunk of the command's stderr.
    Stderr(Vec<u8>),
    /// Guest → host, last frame: the exit code (128+signo for a signal death).
    Exit(i32),
    /// Guest → host, instead of any output: the command could not be started.
    Error(String),
}

impl ExecFrame {
    /// Serialize and write the frame, then flush.
    pub(crate) fn encode(&self, w: &mut impl Write) -> io::Result<()> {
        let (tag, payload) = self.to_wire()?;
        if payload.len() > MAX_FRAME_LEN {
            return Err(invalid(&format!(
                "frame length {} exceeds cap {MAX_FRAME_LEN}",
                payload.len()
            )));
        }
        let mut hdr = [0u8; 5];
        hdr[0] = tag;
        hdr[1..].copy_from_slice(&(payload.len() as u32).to_be_bytes());
        w.write_all(&hdr)?;
        w.write_all(&payload)?;
        w.flush()
    }

    /// Read one frame. `Ok(None)` is a clean EOF between frames; a truncated,
    /// oversized, unknown-tag or malformed frame is an `InvalidData` error.
    pub(crate) fn decode(r: &mut impl Read) -> io::Result<Option<ExecFrame>> {
        let mut hdr = [0u8; 5];
        match r.read_exact(&mut hdr) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
        if len > MAX_FRAME_LEN {
            return Err(invalid(&format!(
                "frame length {len} exceeds cap {MAX_FRAME_LEN}"
            )));
        }
        let mut payload = vec![0u8; len];
        r.read_exact(&mut payload)?;
        Ok(Some(ExecFrame::from_wire(hdr[0], payload)?))
    }

    fn to_wire(&self) -> io::Result<(u8, Vec<u8>)> {
        Ok(match self {
            ExecFrame::Request(req) => (
                T_REQUEST,
                serde_json::to_vec(req).map_err(|e| invalid(&e.to_string()))?,
            ),
            ExecFrame::Stdout(b) => (T_STDOUT, b.clone()),
            ExecFrame::Stderr(b) => (T_STDERR, b.clone()),
            ExecFrame::Exit(code) => (T_EXIT, code.to_be_bytes().to_vec()),
            ExecFrame::Error(msg) => (T_ERROR, msg.clone().into_bytes()),
        })
    }

    fn from_wire(tag: u8, payload: Vec<u8>) -> io::Result<ExecFrame> {
        Ok(match tag {
            T_REQUEST => ExecFrame::Request(
                serde_json::from_slice(&payload)
                    .map_err(|e| invalid(&format!("Request payload: {e}")))?,
            ),
            T_STDOUT => ExecFrame::Stdout(payload),
            T_STDERR => ExecFrame::Stderr(payload),
            T_EXIT => ExecFrame::Exit(i32::from_be_bytes(
                payload
                    .as_slice()
                    .try_into()
                    .map_err(|_| invalid("Exit payload must be 4 bytes"))?,
            )),
            T_ERROR => ExecFrame::Error(
                String::from_utf8(payload).map_err(|_| invalid("Error payload is not UTF-8"))?,
            ),
            other => return Err(invalid(&format!("unknown exec frame tag {other}"))),
        })
    }
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

/// Host side: run `argv` (in `cwd`) through the guest at the other end of
/// `stream`, handing each output chunk to `on_chunk(is_stderr, bytes)` in arrival
/// order, and return the command's exit code. An `Err` from `on_chunk` aborts the
/// exchange (closing the stream; the guest then kills the command). A guest that
/// could not start the command, or a stream that closes before `Exit`, is an error.
#[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
pub(crate) fn run_exec<S: Read + Write>(
    mut stream: S,
    argv: &[String],
    cwd: Option<&str>,
    mut on_chunk: impl FnMut(bool, &[u8]) -> Result<()>,
) -> Result<i32> {
    if argv.is_empty() {
        bail!("exec channel: empty argv");
    }
    ExecFrame::Request(ExecRequest {
        argv: argv.to_vec(),
        cwd: cwd.map(str::to_string),
    })
    .encode(&mut stream)
    .context("send exec request")?;
    loop {
        match ExecFrame::decode(&mut stream).context("read exec channel")? {
            Some(ExecFrame::Stdout(b)) => on_chunk(false, &b)?,
            Some(ExecFrame::Stderr(b)) => on_chunk(true, &b)?,
            Some(ExecFrame::Exit(code)) => return Ok(code),
            Some(ExecFrame::Error(msg)) => bail!("sandbox could not start `{}`: {msg}", argv[0]),
            Some(ExecFrame::Request(_)) => bail!("exec channel: unexpected Request from the guest"),
            None => bail!("exec channel closed before the command's exit code arrived"),
        }
    }
}

/// Guest side: serve one command on `stream`. Reads the `Request`, spawns its argv
/// with stdin empty and stdout/stderr piped, relays both pipes concurrently as
/// frames, then sends `Exit`. A spawn failure is answered with `Error`. If the host
/// goes away mid-command, the command is killed rather than left running.
// Its caller, `exec-host`, is Linux-only (the libkrun guest).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn serve_exec<S: Read + Write + Send + 'static>(mut stream: S) -> Result<()> {
    let req = match ExecFrame::decode(&mut stream).context("read exec request")? {
        Some(ExecFrame::Request(req)) => req,
        Some(other) => bail!("exec channel: expected Request, got {other:?}"),
        None => return Ok(()), // host connected and left (e.g. a readiness probe)
    };
    let Some((program, args)) = req.argv.split_first() else {
        ExecFrame::Error("empty argv".into()).encode(&mut stream)?;
        return Ok(());
    };
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = &req.cwd {
        cmd.current_dir(cwd);
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            ExecFrame::Error(e.to_string()).encode(&mut stream)?;
            return Ok(());
        }
    };

    let writer = Arc::new(Mutex::new(stream));
    let (done_tx, done_rx) = mpsc::channel::<io::Result<()>>();
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    for (pipe, is_stderr) in [
        (Box::new(stdout) as Box<dyn Read + Send>, false),
        (Box::new(stderr) as Box<dyn Read + Send>, true),
    ] {
        let writer = Arc::clone(&writer);
        let done_tx = done_tx.clone();
        std::thread::spawn(move || {
            let _ = done_tx.send(relay(pipe, is_stderr, &writer));
        });
    }
    drop(done_tx);
    let mut relay_err = None;
    for result in done_rx {
        if let Err(e) = result {
            // The host is gone (or the pipe broke): don't leave the command running.
            let _ = child.kill();
            relay_err.get_or_insert(e);
        }
    }
    let status = child.wait().context("wait for exec'd command")?;
    if let Some(e) = relay_err {
        return Err(e).context("relay exec output");
    }
    let mut stream = writer.lock().unwrap_or_else(|p| p.into_inner());
    ExecFrame::Exit(exit_code(status)).encode(&mut *stream)?;
    Ok(())
}

/// Copy `pipe` to the shared stream as `Stdout`/`Stderr` frames until EOF.
// Its caller, `exec-host`, is Linux-only (the libkrun guest).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn relay<S: Write>(mut pipe: impl Read, is_stderr: bool, writer: &Mutex<S>) -> io::Result<()> {
    let mut buf = vec![0u8; CHUNK_BYTES];
    loop {
        let n = match pipe.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        let chunk = buf[..n].to_vec();
        let frame = if is_stderr {
            ExecFrame::Stderr(chunk)
        } else {
            ExecFrame::Stdout(chunk)
        };
        let mut w = writer.lock().unwrap_or_else(|p| p.into_inner());
        frame.encode(&mut *w)?;
    }
}

/// A shell-style exit code: the code, or 128+signo when a signal killed it.
// Its caller, `exec-host`, is Linux-only (the libkrun guest).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt as _;
    status
        .code()
        .or_else(|| status.signal().map(|sig| 128 + sig))
        .unwrap_or(1)
}

/// `pillbox exec-host --vsock-port N`: listen on guest vsock `port` and serve each
/// connection's command on its own thread. Blocks until the VM is torn down.
#[cfg(target_os = "linux")]
pub(crate) fn run_exec_host(port: u32) -> Result<()> {
    use std::os::unix::io::FromRawFd as _;
    use std::os::unix::net::UnixStream;
    let lfd = crate::attach::host::vsock_listen(port)?;
    loop {
        let cfd = unsafe { libc::accept(lfd, std::ptr::null_mut(), std::ptr::null_mut()) };
        if cfd < 0 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            continue;
        }
        // SAFETY: `cfd` is a freshly accepted socket this stream now solely owns.
        let conn = unsafe { UnixStream::from_raw_fd(cfd) };
        std::thread::spawn(move || {
            if let Err(e) = serve_exec(conn) {
                eprintln!("pillbox exec-host: {e:#}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    fn round_trip(frame: &ExecFrame) -> ExecFrame {
        let mut buf = Vec::new();
        frame.encode(&mut buf).unwrap();
        ExecFrame::decode(&mut buf.as_slice()).unwrap().unwrap()
    }

    #[test]
    fn exec_channel_codec_round_trips_every_frame() {
        for frame in [
            ExecFrame::Request(ExecRequest {
                argv: vec!["sh".into(), "-c".into(), "echo 'hi'\n".into()],
                cwd: Some("/workspace/app".into()),
            }),
            ExecFrame::Request(ExecRequest {
                argv: vec!["true".into()],
                cwd: None,
            }),
            ExecFrame::Stdout(b"out\x00\xff".to_vec()),
            ExecFrame::Stderr(Vec::new()),
            ExecFrame::Exit(-3),
            ExecFrame::Exit(137),
            ExecFrame::Error("No such file or directory (os error 2)".into()),
        ] {
            assert_eq!(round_trip(&frame), frame);
        }
    }

    #[test]
    fn exec_channel_clean_eof_and_truncation() {
        assert!(ExecFrame::decode(&mut [].as_slice()).unwrap().is_none());
        let mut buf = Vec::new();
        ExecFrame::Stdout(b"abcdef".to_vec())
            .encode(&mut buf)
            .unwrap();
        buf.truncate(buf.len() - 2);
        assert!(ExecFrame::decode(&mut buf.as_slice()).is_err());
    }

    #[test]
    fn exec_channel_refuses_oversize_and_unknown_frames() {
        let mut hdr = vec![T_STDOUT];
        hdr.extend_from_slice(&((MAX_FRAME_LEN as u32) + 1).to_be_bytes());
        let err = ExecFrame::decode(&mut hdr.as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        let big = ExecFrame::Stdout(vec![0u8; MAX_FRAME_LEN + 1]);
        assert!(big.encode(&mut Vec::new()).is_err());

        let unknown = [99u8, 0, 0, 0, 0];
        assert!(ExecFrame::decode(&mut unknown.as_slice()).is_err());
        let short_exit = [T_EXIT, 0, 0, 0, 1, 7];
        assert!(ExecFrame::decode(&mut short_exit.as_slice()).is_err());
    }

    fn exec_over_pair(argv: &[&str], cwd: Option<&str>) -> (Result<i32>, Vec<u8>, Vec<u8>) {
        let (host, guest) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || serve_exec(guest));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let code = run_exec(host, &argv, cwd, |is_stderr, bytes| {
            if is_stderr { &mut err } else { &mut out }.extend_from_slice(bytes);
            Ok(())
        });
        server.join().unwrap().unwrap();
        (code, out, err)
    }

    #[test]
    fn exec_channel_serve_and_run_deliver_both_streams_and_exit_code() {
        let (code, out, err) =
            exec_over_pair(&["sh", "-c", "echo out; echo err >&2; exit 3"], None);
        assert_eq!(code.unwrap(), 3);
        assert_eq!(out, b"out\n");
        assert_eq!(err, b"err\n");
    }

    #[test]
    fn exec_channel_runs_in_the_requested_cwd() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker"), "x").unwrap();
        let (code, out, _) = exec_over_pair(&["ls"], Some(dir.path().to_str().unwrap()));
        assert_eq!(code.unwrap(), 0);
        assert_eq!(out, b"marker\n");
    }

    #[test]
    fn exec_channel_signal_death_is_128_plus_signo() {
        let (code, _, _) = exec_over_pair(&["sh", "-c", "kill -9 $$"], None);
        assert_eq!(code.unwrap(), 128 + 9);
    }

    #[test]
    fn exec_channel_spawn_failure_is_an_error() {
        let (code, out, err) = exec_over_pair(&["/nonexistent/pillbox-no-such-binary"], None);
        let msg = code.unwrap_err().to_string();
        assert!(msg.contains("could not start"), "got: {msg}");
        assert!(out.is_empty() && err.is_empty());
    }

    #[test]
    fn exec_channel_rejects_empty_argv() {
        let (host, _guest) = UnixStream::pair().unwrap();
        assert!(run_exec(host, &[], None, |_, _| Ok(())).is_err());
    }
}
