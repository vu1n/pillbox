//! The sandbox exec channel: one framed request/response exchange per command over
//! a byte stream (the libkrun sandbox's vsock, bridged to a host unix socket).
//!
//! Compiled in every build: the guest side ([`serve_exec`], behind the hidden
//! `pillbox exec-host`) runs in the in-guest `--no-default-features` binary, the
//! host side ([`run_exec`]) in the CLI.
//!
//! Wire: `[tag:u8][len:u32 BE][payload]`, read and written by `attach::frame`'s
//! shared codec (with this channel's own cap and a strict tag set). The host
//! sends one `Request` (JSON `{argv, cwd}`); the guest answers with any number of
//! `Stdout`/`Stderr` chunks, then exactly one `Exit(i32 BE)` — also for a command
//! that could not be started (127/126, the reason on `Stderr`). A single
//! `Error(utf8)` instead answers only a malformed request. One connection carries
//! one command; concurrent commands use concurrent connections.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::attach::frame::{read_frame, write_frame};

/// Cap on one frame's payload. The length prefix is peer-controlled, so `decode`
/// refuses anything larger instead of allocating it (attach's cap is 64 MiB). Output chunks are
/// [`CHUNK_BYTES`]; only a request's argv could approach this.
const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

/// Read size for the child's stdout/stderr pipes — one output frame at most.
/// The host decodes every guest frame with this cap ([`run_exec`]).
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
    /// Guest → host, instead of any output: the request itself was unusable
    /// (empty argv). A command that can't be started is an `Exit`, not this.
    Error(String),
}

impl ExecFrame {
    /// Serialize and write the frame, then flush.
    pub(crate) fn encode(&self, w: &mut impl Write) -> io::Result<()> {
        let (tag, payload) = self.to_wire()?;
        write_frame(w, tag, &payload, MAX_FRAME_LEN)
    }

    /// Read one frame. `Ok(None)` is a clean EOF between frames; a truncated
    /// (including a partial header), oversized, unknown-tag or malformed frame is
    /// an `InvalidData` error.
    pub(crate) fn decode(r: &mut impl Read) -> io::Result<Option<ExecFrame>> {
        Self::decode_capped(r, MAX_FRAME_LEN)
    }

    /// [`decode`](Self::decode) with a smaller payload cap.
    fn decode_capped(r: &mut impl Read, cap: usize) -> io::Result<Option<ExecFrame>> {
        match read_frame(r, cap)? {
            Some((tag, payload)) => Ok(Some(ExecFrame::from_wire(tag, payload)?)),
            None => Ok(None),
        }
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
            // Strict, unlike attach: both ends ship in one binary, so an unknown tag is corruption.
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
/// exchange (closing the stream; the guest then kills the command's process
/// group). A guest that could not start the command, or a stream that closes
/// before `Exit`, is an error.
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
        // Every guest frame (output chunk, exit code, refusal) fits one CHUNK_BYTES read.
        match ExecFrame::decode_capped(&mut stream, CHUNK_BYTES).context("read exec channel")? {
            Some(ExecFrame::Stdout(b)) => on_chunk(false, &b)?,
            Some(ExecFrame::Stderr(b)) => on_chunk(true, &b)?,
            Some(ExecFrame::Exit(code)) => return Ok(code),
            Some(ExecFrame::Error(msg)) => bail!("sandbox refused the exec request: {msg}"),
            Some(ExecFrame::Request(_)) => bail!("exec channel: unexpected Request from the guest"),
            None => bail!("exec channel closed before the command's exit code arrived"),
        }
    }
}

/// The uid:gid a sandbox command is dropped to, with no supplementary groups.
/// `exec-host` itself stays root, so a command can neither kill it nor take its
/// vsock port and forge another command's output or exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RunAs {
    pub(crate) uid: u32,
    pub(crate) gid: u32,
}

/// Guest side: serve one command on `stream`, as `run_as` when given (else as
/// `exec-host`'s own identity).
///
/// Reads the `Request` and spawns its argv in a new process group (stdin empty,
/// stdout/stderr piped). Until the command's own process exits, it relays both
/// pipes as frames, at most one chunk per pipe per wake-up, and watches the host
/// end: if the host closes the stream (or a frame write fails), it SIGKILLs the
/// whole process group and returns an error. Once the command's own process
/// exits, it relays only the bytes buffered in the pipes at that moment and sends
/// `Exit` without waiting for pipe EOF, so a backgrounded descendant still holding
/// them (`sh -c 'server &'`, even a fast writer like `yes &`) does not hold the
/// exchange open. Such descendants keep running after `Exit`; anything they write
/// later is read and discarded by a detached thread until they close the pipes. A
/// command that can't be started gets a shell-style `Exit` (see [`spawn_failure`]).
// Its caller, `exec-host`, is Linux-only (the libkrun guest).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn serve_exec(mut stream: UnixStream, run_as: Option<RunAs>) -> Result<()> {
    use std::os::unix::process::CommandExt as _;
    use std::process::{Command, Stdio};

    let req = match ExecFrame::decode(&mut stream).context("read exec request")? {
        Some(ExecFrame::Request(req)) => req,
        Some(other) => bail!("exec channel: expected Request, got {other:?}"),
        None => return Ok(()), // host connected and left (e.g. a readiness probe)
    };
    let Some((program, args)) = req.argv.split_first() else {
        ExecFrame::Error("empty argv".into()).encode(&mut stream)?;
        return Ok(());
    };
    // Set up before the spawn, so a failure here never strands a running command.
    let (exited, mut exited_tx) = UnixStream::pair().context("exec exit notifier")?;
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    if let Some(RunAs { uid, gid }) = run_as {
        // std drops from root as setgroups(0) (no supplementary groups), setgid, setuid,
        // then chdir: the cwd is entered as the command's user, never as root.
        cmd.uid(uid).gid(gid);
    }
    if let Some(cwd) = &req.cwd {
        cmd.current_dir(cwd);
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            let (code, reason) = spawn_failure(program, req.cwd.as_deref(), run_as, &e);
            ExecFrame::Stderr(format!("pillbox exec: {reason}\n").into_bytes())
                .encode(&mut stream)?;
            ExecFrame::Exit(code).encode(&mut stream)?;
            return Ok(());
        }
    };
    let group = child.id() as libc::pid_t;
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let pipes =
        OutPipe::new(stdout, false).and_then(|out| Ok(vec![out, OutPipe::new(stderr, true)?]));
    let mut pipes = match pipes {
        Ok(pipes) => pipes,
        Err(e) => {
            unsafe { libc::killpg(group, libc::SIGKILL) };
            let _ = child.wait();
            return Err(e);
        }
    };
    // The waiter reaps the command and wakes the poll loop through `exited`.
    let waiter = std::thread::spawn(move || {
        let status = child.wait();
        let _ = exited_tx.write_all(&[1]);
        status
    });
    let kill_group = |waiter: std::thread::JoinHandle<io::Result<std::process::ExitStatus>>| {
        unsafe { libc::killpg(group, libc::SIGKILL) };
        let _ = waiter.join();
    };

    loop {
        match wait_readable(&stream, &exited, &pipes) {
            Ok(Ready {
                host,
                exited: done,
                pipes: ready,
            }) => {
                if host && host_gone(&mut stream) {
                    kill_group(waiter);
                    bail!("the host closed the exec channel; killed the command's process group");
                }
                for (pipe, readable) in pipes.iter_mut().zip(ready) {
                    if readable {
                        if let Err(e) = pipe.relay_chunk(&mut stream, CHUNK_BYTES) {
                            kill_group(waiter);
                            return Err(e).context("relay exec output");
                        }
                    }
                }
                pipes.retain(|p| p.open);
                if done {
                    break;
                }
            }
            Err(e) => {
                kill_group(waiter);
                return Err(e).context("poll the exec channel");
            }
        }
    }
    // The command's own process is gone: relay what the pipes held at this moment,
    // not what a background writer keeps adding.
    for pipe in &mut pipes {
        if let Err(e) = pipe.relay_buffered(&mut stream) {
            kill_group(waiter);
            return Err(e).context("relay exec output");
        }
    }
    let status = waiter
        .join()
        .map_err(|_| anyhow::anyhow!("exec waiter panicked"))?
        .context("wait for exec'd command")?;
    pipes.retain(|p| p.open);
    if !pipes.is_empty() {
        // Background descendants still hold the pipes: drain them so they never hit SIGPIPE.
        std::thread::spawn(move || {
            for pipe in pipes {
                pipe.discard_until_eof();
            }
        });
    }
    ExecFrame::Exit(exit_code(status)).encode(&mut stream)?;
    Ok(())
}

/// One of the command's output pipes, non-blocking, relayed as `Stdout`/`Stderr`.
struct OutPipe {
    file: std::fs::File,
    is_stderr: bool,
    /// False once the pipe hit EOF (every writer closed it).
    open: bool,
}

impl OutPipe {
    fn new(pipe: impl Into<std::os::fd::OwnedFd>, is_stderr: bool) -> Result<Self> {
        use std::os::fd::AsRawFd as _;
        let file = std::fs::File::from(pipe.into());
        let fd = file.as_raw_fd();
        // SAFETY: plain fcntl on an fd this struct owns.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error()).context("make an exec pipe non-blocking");
        }
        Ok(Self {
            file,
            is_stderr,
            open: true,
        })
    }

    /// One read of at most `max` bytes, relayed as one frame. Returns the bytes
    /// relayed: 0 at EOF (the pipe is then closed) or when nothing is readable.
    fn relay_chunk(&mut self, stream: &mut impl Write, max: usize) -> io::Result<usize> {
        let mut buf = vec![0u8; max.min(CHUNK_BYTES)];
        loop {
            return match self.file.read(&mut buf) {
                Ok(0) => {
                    self.open = false;
                    Ok(0)
                }
                Ok(n) => {
                    let chunk = buf[..n].to_vec();
                    let frame = if self.is_stderr {
                        ExecFrame::Stderr(chunk)
                    } else {
                        ExecFrame::Stdout(chunk)
                    };
                    frame.encode(stream)?;
                    Ok(n)
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(0),
                Err(e) => Err(e),
            };
        }
    }

    /// Relay the bytes buffered in the pipe now (`FIONREAD`), and no more.
    fn relay_buffered(&mut self, stream: &mut impl Write) -> io::Result<()> {
        use std::os::fd::AsRawFd as _;
        let mut buffered: libc::c_int = 0;
        // SAFETY: FIONREAD writes one c_int through a valid pointer.
        if unsafe { libc::ioctl(self.file.as_raw_fd(), libc::FIONREAD, &mut buffered) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut left = usize::try_from(buffered).unwrap_or(0);
        while left > 0 && self.open {
            match self.relay_chunk(stream, left)? {
                0 => break,
                n => left -= n,
            }
        }
        Ok(())
    }

    /// Read and drop everything until every writer has closed the pipe.
    fn discard_until_eof(mut self) {
        let mut buf = vec![0u8; CHUNK_BYTES];
        while self.open {
            match wait_one(&self.file) {
                Ok(()) => match self.file.read(&mut buf) {
                    Ok(0) => self.open = false,
                    Ok(_) => {}
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(_) => self.open = false,
                },
                Err(_) => self.open = false,
            }
        }
    }
}

/// Which of the poll loop's descriptors are readable.
struct Ready {
    host: bool,
    exited: bool,
    pipes: Vec<bool>,
}

/// Block until the host stream, the exit notifier, or an open pipe is readable
/// (or hung up). No timeout: every wake-up has a cause.
fn wait_readable(host: &UnixStream, exited: &UnixStream, pipes: &[OutPipe]) -> io::Result<Ready> {
    use std::os::fd::AsRawFd as _;
    let mut fds: Vec<libc::pollfd> = [host.as_raw_fd(), exited.as_raw_fd()]
        .into_iter()
        .chain(pipes.iter().map(|p| p.file.as_raw_fd()))
        .map(|fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    loop {
        // SAFETY: `fds` is a valid, exclusively borrowed pollfd array.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if rc >= 0 {
            break;
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
    let woke = |p: &libc::pollfd| p.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0;
    Ok(Ready {
        host: woke(&fds[0]),
        exited: woke(&fds[1]),
        pipes: fds[2..].iter().map(woke).collect(),
    })
}

/// Block until `file` is readable or hung up.
fn wait_one(file: &std::fs::File) -> io::Result<()> {
    use std::os::fd::AsRawFd as _;
    let mut fd = libc::pollfd {
        fd: file.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: one valid pollfd.
        if unsafe { libc::poll(&mut fd, 1, -1) } >= 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// The host stream polled readable: true when the host has gone (EOF or a read
/// error). The host sends nothing after its `Request`, so stray bytes are dropped.
fn host_gone(stream: &mut UnixStream) -> bool {
    let mut buf = [0u8; 256];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => return true,
            Ok(_) => return false,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return true,
        }
    }
}

/// The shell-style exit code and reason for a command that could not be started:
/// 127 when the program isn't found, 126 when it can't be run (permission denied,
/// not an executable, or an unusable `cwd`).
fn spawn_failure(
    program: &str,
    cwd: Option<&str>,
    run_as: Option<RunAs>,
    e: &io::Error,
) -> (i32, String) {
    // The child chdirs before exec, so a bad cwd fails the spawn with the same errno as
    // a bad program; tell them apart by checking the cwd as the command's user.
    if let Some(problem) = cwd.and_then(|cwd| cwd_problem(cwd, run_as)) {
        return (126, problem);
    }
    match e.kind() {
        io::ErrorKind::NotFound => (127, format!("{program}: command not found")),
        _ => (126, format!("{program}: cannot execute: {e}")),
    }
}

/// Why the command's user can't enter `cwd`, if it can't: missing, not a
/// directory, or (EACCES) a directory on its path it may not search.
fn cwd_problem(cwd: &str, run_as: Option<RunAs>) -> Option<String> {
    let path = std::path::Path::new(cwd);
    match std::fs::metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Some(format!("cwd `{cwd}` does not exist"))
        }
        Err(e) => return Some(format!("cwd `{cwd}` is not usable: {e}")),
        Ok(meta) if !meta.is_dir() => return Some(format!("cwd `{cwd}` is not a directory")),
        Ok(_) => {}
    }
    let who = match run_as {
        Some(RunAs { uid, gid }) => {
            if searchable_by(path, uid, gid) {
                return None;
            }
            format!("uid {uid}")
        }
        None => {
            let c = std::ffi::CString::new(cwd).ok()?;
            // SAFETY: access() reads a valid NUL-terminated path.
            if unsafe { libc::access(c.as_ptr(), libc::X_OK) } == 0 {
                return None;
            }
            // SAFETY: getuid() has no preconditions.
            format!("uid {}", unsafe { libc::getuid() })
        }
    };
    Some(format!(
        "cwd `{cwd}` is not accessible to {who}: permission denied"
    ))
}

/// Whether `uid`:`gid` (no supplementary groups) may search `dir` and every
/// directory above it, by their mode bits. uid 0 always may.
fn searchable_by(dir: &std::path::Path, uid: u32, gid: u32) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    if uid == 0 {
        return true;
    }
    dir.ancestors().all(|d| {
        let Ok(meta) = std::fs::metadata(d) else {
            return false;
        };
        let bit = if meta.uid() == uid {
            0o100
        } else if meta.gid() == gid {
            0o010
        } else {
            0o001
        };
        meta.mode() & bit != 0
    })
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

/// `pillbox exec-host --vsock-port N [--run-as-uid U]`: listen on guest vsock
/// `port` and serve each connection's command on its own thread, as uid:gid U:U
/// when given. Blocks until the VM is torn down.
#[cfg(target_os = "linux")]
pub(crate) fn run_exec_host(port: u32, run_as_uid: Option<u32>) -> Result<()> {
    let run_as = run_as_uid.map(|uid| RunAs { uid, gid: uid });
    crate::attach::host::vsock_serve(port, move |conn| {
        std::thread::spawn(move || {
            if let Err(e) = serve_exec(conn, run_as) {
                eprintln!("pillbox exec-host: {e:#}");
            }
        });
    })
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
        // A partial header then EOF is a torn frame, not a clean close.
        for cut in 1..5 {
            let partial = [T_STDOUT, 0, 0, 0, 1];
            let err = ExecFrame::decode(&mut &partial[..cut]).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "cut at {cut}");
        }
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
        exec_over_pair_as(argv, cwd, None)
    }

    fn exec_over_pair_as(
        argv: &[&str],
        cwd: Option<&str>,
        run_as: Option<RunAs>,
    ) -> (Result<i32>, Vec<u8>, Vec<u8>) {
        let (host, guest) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || serve_exec(guest, run_as));
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
    fn a_command_that_cannot_start_exits_127_or_126_with_the_reason_on_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let not_exec = dir.path().join("data.txt");
        std::fs::write(&not_exec, "not a program").unwrap();
        let no_format = dir.path().join("garbage");
        std::fs::write(&no_format, [0u8, 1, 2, 3]).unwrap();
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&no_format, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let cases: [(&[&str], Option<&str>, i32, &str); 5] = [
            (
                &["/nonexistent/pillbox-no-such-binary"],
                None,
                127,
                "command not found",
            ),
            (
                &["pillbox-no-such-command-on-path"],
                None,
                127,
                "command not found",
            ),
            (&[not_exec.to_str().unwrap()], None, 126, "cannot execute"),
            (&[no_format.to_str().unwrap()], None, 126, "cannot execute"),
            (
                &["true"],
                Some("/nonexistent/pillbox-cwd"),
                126,
                "does not exist",
            ),
        ];
        for (argv, cwd, want, reason) in cases {
            let (code, out, err) = exec_over_pair(argv, cwd);
            assert_eq!(code.unwrap(), want, "{argv:?} in {cwd:?}");
            assert!(out.is_empty());
            let err = String::from_utf8(err).unwrap();
            assert!(err.contains(reason), "{argv:?}: {err}");
        }
    }

    /// Poll until `path` holds a pid (the command wrote it), bounded.
    fn read_pid(path: &std::path::Path) -> libc::pid_t {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some(pid) = std::fs::read_to_string(path)
                .ok()
                .and_then(|s| s.trim().parse().ok())
            {
                return pid;
            }
            assert!(std::time::Instant::now() < deadline, "no pid in {path:?}");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn exec_channel_kills_a_quiet_command_when_the_host_disconnects() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let (mut host, guest) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || serve_exec(guest, None));
        let script = format!("echo $$ > '{}'; exec sleep 60", pidfile.display());
        ExecFrame::Request(ExecRequest {
            argv: vec!["sh".into(), "-c".into(), script],
            cwd: None,
        })
        .encode(&mut host)
        .unwrap();
        let pid = read_pid(&pidfile);
        let started = std::time::Instant::now();
        drop(host);
        let err = server.join().unwrap().unwrap_err().to_string();
        assert!(err.contains("host closed"), "{err}");
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        // serve_exec reaped it before returning: the pid no longer exists.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "sleep {pid} survived");
    }

    #[test]
    fn exec_channel_exits_when_the_command_does_even_if_a_background_child_holds_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("bg");
        let script = format!("echo hi; sleep 60 & echo $! > '{}'", pidfile.display());
        let started = std::time::Instant::now();
        let (code, out, _) = exec_over_pair(&["sh", "-c", &script], None);
        let elapsed = started.elapsed();
        let bg = read_pid(&pidfile);
        unsafe { libc::kill(bg, libc::SIGKILL) };
        assert_eq!(code.unwrap(), 0);
        assert_eq!(out, b"hi\n");
        assert!(elapsed < std::time::Duration::from_secs(10), "{elapsed:?}");
    }

    #[test]
    fn exit_arrives_promptly_even_while_a_background_writer_floods_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("yes");
        let script = format!("yes & echo $! > '{}'", pidfile.display());
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (code, _, _) = exec_over_pair(&["sh", "-c", &script], None);
            let _ = done_tx.send(code.map_err(|e| e.to_string()));
        });
        let outcome = done_rx.recv_timeout(std::time::Duration::from_secs(10));
        let writer = read_pid(&pidfile);
        unsafe { libc::kill(writer, libc::SIGKILL) };
        assert_eq!(outcome.expect("Exit never arrived").unwrap(), 0);
    }

    #[test]
    fn exec_channel_rejects_empty_argv() {
        let (host, _guest) = UnixStream::pair().unwrap();
        assert!(run_exec(host, &[], None, |_, _| Ok(())).is_err());
    }

    #[test]
    fn run_exec_refuses_a_guest_output_frame_over_one_chunk() {
        for (len, ok) in [(CHUNK_BYTES, true), (CHUNK_BYTES + 1, false)] {
            let (host, mut guest) = UnixStream::pair().unwrap();
            let guest = std::thread::spawn(move || {
                let _ = ExecFrame::decode(&mut guest);
                // Encoded with the guest's own (larger) cap, as a hostile guest could.
                let _ = ExecFrame::Stdout(vec![b'x'; len]).encode(&mut guest);
                let _ = ExecFrame::Exit(0).encode(&mut guest);
            });
            let mut seen = 0;
            let result = run_exec(host, &["true".into()], None, |_, b| {
                seen += b.len();
                Ok(())
            });
            guest.join().unwrap();
            if ok {
                assert_eq!(result.unwrap(), 0);
                assert_eq!(seen, len);
            } else {
                let err = format!("{:#}", result.unwrap_err());
                assert!(err.contains("exceeds cap"), "{err}");
                assert_eq!(seen, 0);
            }
        }
    }

    /// A frame sink that, like a background writer racing the post-exit relay,
    /// pushes more bytes into the pipe each time a frame is written.
    struct RacingSink {
        frames: Vec<u8>,
        writer: std::fs::File,
        added: usize,
    }
    impl Write for RacingSink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.added < 40 * 1024 {
                self.writer.write_all(&[b'y'; 4096])?;
                self.added += 4096;
            }
            self.frames.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn the_post_exit_relay_sends_only_what_was_buffered_when_it_started() {
        use std::os::fd::FromRawFd as _;
        let mut fds = [0; 2];
        // SAFETY: pipe() fills two fds this test then owns.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let (read, mut write) = unsafe {
            (
                std::os::fd::OwnedFd::from_raw_fd(fds[0]),
                std::fs::File::from_raw_fd(fds[1]),
            )
        };
        const BUFFERED: usize = 1000;
        write.write_all(&[b'x'; BUFFERED]).unwrap();
        let mut pipe = OutPipe::new(read, false).unwrap();
        let mut sink = RacingSink {
            frames: Vec::new(),
            writer: write,
            added: 0,
        };
        pipe.relay_buffered(&mut sink).unwrap();
        assert!(sink.added > 0, "the racing writer never ran");
        let mut relayed = Vec::new();
        let mut frames = sink.frames.as_slice();
        while let Some(frame) = ExecFrame::decode(&mut frames).unwrap() {
            match frame {
                ExecFrame::Stdout(b) => relayed.extend(b),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(relayed, vec![b'x'; BUFFERED]);
    }

    #[test]
    fn a_cwd_problem_is_named_as_one() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, "x").unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        let inner = locked.join("inner");
        std::fs::create_dir(&inner).unwrap();
        std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o777)).unwrap();
        let stranger = Some(RunAs {
            uid: 54_321,
            gid: 54_321,
        });
        let problem = |p: &std::path::Path, who| cwd_problem(p.to_str().unwrap(), who);
        assert!(problem(&dir.path().join("missing"), None)
            .unwrap()
            .contains("does not exist"));
        assert!(problem(&file, None).unwrap().contains("is not a directory"));
        // A searchable path is fine; a 0700 directory, or one under it, is not for another uid.
        assert_eq!(problem(dir.path(), None), None);
        for p in [&locked, &inner] {
            let msg = problem(p, stranger).unwrap();
            assert!(
                msg.contains("is not accessible to uid 54321"),
                "{p:?}: {msg}"
            );
        }
        // Root may search anything.
        assert_eq!(problem(&inner, Some(RunAs { uid: 0, gid: 0 })), None);
        // The spawn failure for that cwd names it, not the program.
        let denied = io::Error::from_raw_os_error(libc::EACCES);
        let (code, reason) = spawn_failure("true", inner.to_str(), stranger, &denied);
        assert_eq!(code, 126);
        assert!(
            reason.contains("cwd") && !reason.contains("cannot execute"),
            "{reason}"
        );
    }

    #[test]
    fn a_run_as_command_runs_as_that_uid_with_no_supplementary_groups() {
        // SAFETY: geteuid() has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            eprintln!(
                "skipping: dropping to another uid needs root (exec-host is root in the guest)"
            );
            return;
        }
        let run_as = Some(RunAs {
            uid: 54_321,
            gid: 54_322,
        });
        let (code, out, err) =
            exec_over_pair_as(&["sh", "-c", "id -u; id -g; id -G"], Some("/"), run_as);
        assert_eq!(code.unwrap(), 0, "{}", String::from_utf8_lossy(&err));
        assert_eq!(String::from_utf8(out).unwrap(), "54321\n54322\n54322\n");
        // A cwd the dropped user can't enter is reported as a cwd problem.
        let dir = tempfile::tempdir().unwrap();
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let (code, _, err) = exec_over_pair_as(&["true"], dir.path().to_str(), run_as);
        assert_eq!(code.unwrap(), 126);
        let err = String::from_utf8(err).unwrap();
        assert!(err.contains("is not accessible to uid 54321"), "{err}");
    }
}
