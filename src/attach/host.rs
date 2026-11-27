//! The pty-host: owns the agent's real PTY + a [`ScreenModel`], and serves
//! the [`Frame`] protocol to N clients over a unix socket. It keeps running
//! across client disconnects, so detach / `--detach` / reattach all work.
//!
//! This is the process that runs *inside* the sandbox (it is `pillbox` in
//! `pty-host` mode). The libkrun backend bridges its unix socket to the guest's
//! vsock listener, so the local pump connects to this socket directly.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};

use super::frame::Frame;
use super::screen::ScreenModel;

const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

/// Count of client writer threads still draining; the reader waits on this
/// (bounded) after broadcasting the Exit frame so the exit code actually
/// reaches clients before the host process tears down. `(count, condvar)`.
type WriterGate = Arc<(Mutex<usize>, Condvar)>;

/// Upper bound on how long the reader waits for writers to flush the Exit
/// frame before exiting anyway — covers a wedged/slow client that would
/// otherwise hang teardown forever.
const EXIT_DRAIN_TIMEOUT: Duration = Duration::from_secs(3);

/// What the broadcast carries to each client's writer thread.
enum Out {
    Data(Vec<u8>),
    Exit(i32),
}

struct Hub {
    screen: ScreenModel,
    /// Per-client senders; pruned when a send fails.
    clients: Vec<Sender<Out>>,
}

type SharedWriter = Arc<Mutex<Box<dyn std::io::Write + Send>>>;
type SharedMaster = Arc<Mutex<Box<dyn MasterPty + Send>>>;

/// Run the pty-host on a unix socket: spawn `argv` under a PTY and serve frames
/// on `sock` until the child exits. `argv[0]` is the program; the rest are args.
pub(crate) fn run(sock: &str, argv: &[String]) -> Result<()> {
    let _ = std::fs::remove_file(sock); // clear a stale socket before binding
    let session = spawn_pty_session(argv)?;
    let listener = UnixListener::bind(sock).with_context(|| format!("binding {sock}"))?;
    for stream in listener.incoming().flatten() {
        session.serve(stream);
    }
    Ok(())
}

/// Run the pty-host over **vsock** (guest-side, for the libkrun backend), reusing
/// [`handle_client`] over the vsock fd wrapped as a `UnixStream`. Linux-only — the
/// host (macOS) never runs this; it pumps the bridged socket. Two directions:
///
/// - `listen=false` (**foreground**): the guest **dials the host**
///   (`VMADDR_CID_HOST`) on `port`; libkrun bridges to the parent's pre-bound
///   listener, which `accept()`s once we're up — no connect-before-ready race.
///   One client; the agent's exit ends the process.
/// - `listen=true` (**detach**): the guest **listens** on `port` and accepts
///   reattach clients one at a time (the agent + screen persist across them via
///   the [`PtySession`]); libkrun binds the host socket so it survives the parent
///   returning. The agent's exit (the reader thread) ends the process.
#[cfg(target_os = "linux")]
pub(crate) fn run_vsock(port: u32, listen: bool, argv: &[String]) -> Result<()> {
    use std::os::unix::io::FromRawFd;
    use std::time::Duration;

    const VMADDR_CID_HOST: u32 = 2;
    let session = spawn_pty_session(argv)?;

    if listen {
        // Accept reattach clients one at a time; the agent (and its screen) live in
        // `session` across them. The agent's own exit (reader thread →
        // process::exit) is the real terminator.
        return vsock_serve(port, |conn| session.serve_blocking(conn));
    }

    // Dial the host, retrying only until the bridge + parent listener are ready
    // (the guest boots well after the parent binds, so this connects promptly).
    let stream = loop {
        // CLOEXEC: the agent this pty-host spawns must not inherit its host channel.
        let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            anyhow::bail!("AF_VSOCK socket: {}", std::io::Error::last_os_error());
        }
        let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
        addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
        addr.svm_port = port;
        addr.svm_cid = VMADDR_CID_HOST;
        let alen = std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;
        if unsafe { libc::connect(fd, &addr as *const _ as *const libc::sockaddr, alen) } == 0 {
            break unsafe { UnixStream::from_raw_fd(fd) };
        }
        unsafe { libc::close(fd) };
        std::thread::sleep(Duration::from_millis(100));
    };
    // Serve the foreground client (blocks until it disconnects); on agent exit the
    // reader thread exits the process, so there's no client to reattach yet.
    session.serve_blocking(stream);
    Ok(())
}

/// Listen on guest vsock `port` and hand every accepted connection to `handler`,
/// forever. An accept error backs off and retries rather than returning: the
/// caller's process (a live agent, a relay) must not end because one accept
/// failed. Shared by the detach reattach loop ([`run_vsock`]), the port-forward
/// relay ([`run_vsock_forward`]), and the sandbox `exec-host`
/// ([`crate::sandbox::exec_channel::run_exec_host`]).
#[cfg(target_os = "linux")]
pub(crate) fn vsock_serve(port: u32, mut handler: impl FnMut(UnixStream)) -> Result<()> {
    let lfd = vsock_listen(port)?;
    loop {
        match accept_cloexec(lfd) {
            Ok(conn) => handler(conn),
            Err(_) => thread::sleep(Duration::from_millis(100)),
        }
    }
}

/// Accept one connection on the listening socket `lfd`.
#[cfg(target_os = "linux")]
fn accept_cloexec(lfd: i32) -> std::io::Result<UnixStream> {
    use std::os::unix::io::FromRawFd;
    // CLOEXEC: a command spawned for one connection must not inherit another's socket.
    let cfd = unsafe {
        libc::accept4(
            lfd,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            libc::SOCK_CLOEXEC,
        )
    };
    if cfd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `cfd` is a freshly accepted socket this stream now solely owns.
    Ok(unsafe { UnixStream::from_raw_fd(cfd) })
}

/// Bind + listen a guest-side vsock listener on `port` (CID_ANY). libkrun binds
/// the host side (`krun_add_vsock_port2` listen=true); the host dials it.
/// Returns the listening fd.
#[cfg(target_os = "linux")]
fn vsock_listen(port: u32) -> Result<i32> {
    const VMADDR_CID_ANY: u32 = 0xffff_ffff;
    // CLOEXEC: no spawned command may inherit the listener (and hold or reuse the port).
    let lfd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if lfd < 0 {
        anyhow::bail!("AF_VSOCK socket: {}", std::io::Error::last_os_error());
    }
    let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
    addr.svm_port = port;
    addr.svm_cid = VMADDR_CID_ANY;
    let alen = std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;
    if unsafe { libc::bind(lfd, &addr as *const _ as *const libc::sockaddr, alen) } != 0 {
        anyhow::bail!("bind vsock :{port}: {}", std::io::Error::last_os_error());
    }
    if unsafe { libc::listen(lfd, 4) } != 0 {
        anyhow::bail!("listen vsock :{port}: {}", std::io::Error::last_os_error());
    }
    Ok(lfd)
}

/// Guest-side opencode port-forward relay: listen on vsock `port` and bridge
/// each accepted connection to `127.0.0.1:<to_port>` (the in-guest `opencode
/// serve`). The host opens one connection per HTTP request and speaks HTTP over
/// the relayed byte stream ([`LibkrunHttp`](crate::sandbox::libkrun)). Unlike a
/// generic exec channel this exposes ONLY the opencode port — no command-exec
/// surface in the guest. Blocks forever (the VM teardown kills it).
#[cfg(target_os = "linux")]
pub(crate) fn run_vsock_forward(port: u32, to_port: u16) -> Result<()> {
    vsock_serve(port, |client| {
        thread::spawn(move || forward_conn(client, to_port));
    })
}

/// Bridge one client connection to `127.0.0.1:<to_port>`, copying both ways
/// until either side closes. Errors are logged, not propagated — one failed
/// connection mustn't kill the relay.
#[cfg(target_os = "linux")]
pub(crate) fn forward_conn(client: UnixStream, to_port: u16) {
    use std::net::Shutdown;
    let upstream = match std::net::TcpStream::connect(("127.0.0.1", to_port)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("pillbox: vsock-forward: connect 127.0.0.1:{to_port}: {e}");
            return;
        }
    };
    // Each direction needs its own handle to both sockets.
    let (Ok(mut client_rd), Ok(mut upstream_wr)) = (client.try_clone(), upstream.try_clone())
    else {
        return;
    };
    // client → upstream on a thread; upstream → client on this one.
    let up = thread::spawn(move || {
        let _ = std::io::copy(&mut client_rd, &mut upstream_wr);
        let _ = upstream_wr.shutdown(Shutdown::Write);
    });
    let (mut up_rd, mut client_wr) = (upstream, client);
    let _ = std::io::copy(&mut up_rd, &mut client_wr);
    let _ = client_wr.shutdown(Shutdown::Write);
    let _ = up.join();
}

/// The running PTY session: the agent's PTY behind the hub/screen, ready to
/// serve attach clients. Built once; each accepted connection (unix or vsock)
/// is handed to [`Self::serve`].
struct PtySession {
    hub: Arc<Mutex<Hub>>,
    writer: SharedWriter,
    master: SharedMaster,
    gate: WriterGate,
}

impl PtySession {
    /// Serve one attach client connection on its own thread.
    fn serve(&self, stream: UnixStream) {
        let (hub, writer, master, gate) = (
            self.hub.clone(),
            self.writer.clone(),
            self.master.clone(),
            self.gate.clone(),
        );
        thread::spawn(move || handle_client(stream, hub, writer, master, gate));
    }

    /// Serve one client on the calling thread (blocks until it disconnects).
    /// Used by the connect-out vsock loop, which dials a fresh connection per
    /// client rather than accepting on a listener.
    #[cfg(target_os = "linux")]
    fn serve_blocking(&self, stream: UnixStream) {
        handle_client(
            stream,
            self.hub.clone(),
            self.writer.clone(),
            self.master.clone(),
            self.gate.clone(),
        );
    }
}

/// Spawn `argv` under a PTY and start the reader→clients fan-out, returning the
/// [`PtySession`] the transport-specific accept loop serves clients from.
fn spawn_pty_session(argv: &[String]) -> Result<PtySession> {
    let (program, args) = argv
        .split_first()
        .context("pty-host requires a command after `--`")?;

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: DEFAULT_ROWS,
            cols: DEFAULT_COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("opening PTY")?;

    let mut cmd = CommandBuilder::new(program);
    cmd.args(args);
    // CommandBuilder defaults cwd to $HOME when unset — but the agent must
    // start in the workspace (the backend sets this process's cwd to
    // `/workspace/<name>`). Inherit it explicitly.
    if let Ok(cwd) = std::env::current_dir() {
        cmd.cwd(cwd);
    }
    // portable-pty switches to explicit-env mode the moment we set any var,
    // so inherit the full environment first (the agent needs HOME, PATH,
    // and the secrets/env the backend injected), then
    // ensure a sane TERM.
    for (k, v) in std::env::vars() {
        cmd.env(k, v);
    }
    cmd.env("TERM", "xterm-256color");
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .context("spawning agent under PTY")?;

    let mut reader = pair
        .master
        .try_clone_reader()
        .context("cloning PTY reader")?;
    let writer: SharedWriter =
        Arc::new(Mutex::new(pair.master.take_writer().context("PTY writer")?));
    drop(pair.slave); // so the master sees EOF when the child exits
    let master: SharedMaster = Arc::new(Mutex::new(pair.master));
    let hub = Arc::new(Mutex::new(Hub {
        screen: ScreenModel::new(DEFAULT_COLS, DEFAULT_ROWS),
        clients: Vec::new(),
    }));
    let gate: WriterGate = Arc::new((Mutex::new(0usize), Condvar::new()));

    // PTY reader: feed the screen model + fan out raw bytes to clients.
    // Owns the child so it can read the exit code on EOF.
    {
        let (hub, gate) = (hub.clone(), gate.clone());
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let chunk = buf[..n].to_vec();
                        let mut h = hub.lock().unwrap();
                        h.screen.feed(&chunk);
                        h.clients
                            .retain(|c| c.send(Out::Data(chunk.clone())).is_ok());
                    }
                }
            }
            // Child exited. Broadcast its code as an Exit frame so attached
            // clients can propagate it (e.g. a non-detached `pillbox run`
            // returns the agent's status), then exit so the container stops.
            // A wait() error means we couldn't determine the status — treat
            // that as failure (1), never silently as success (0).
            let code = child.wait().map(|s| s.exit_code() as i32).unwrap_or(1);
            {
                let mut h = hub.lock().unwrap();
                h.clients.retain(|c| c.send(Out::Exit(code)).is_ok());
            }
            // Wait for the per-client writers to actually encode + flush the
            // Exit frame (each then shuts its socket), instead of guessing
            // with a fixed sleep that loses the code under output backlog.
            // Bounded so a wedged client can't hang teardown forever.
            let (count, cvar) = &*gate;
            let mut n = count.lock().unwrap();
            let deadline = Instant::now() + EXIT_DRAIN_TIMEOUT;
            while *n > 0 {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                n = cvar.wait_timeout(n, remaining).unwrap().0;
            }
            drop(n);
            std::process::exit(0);
        });
    }

    Ok(PtySession {
        hub,
        writer,
        master,
        gate,
    })
}

fn handle_client(
    stream: UnixStream,
    hub: Arc<Mutex<Hub>>,
    writer: SharedWriter,
    master: SharedMaster,
    gate: WriterGate,
) {
    let mut wstream = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };

    // Register + snapshot atomically so no live byte is missed or doubled:
    // the snapshot reflects exactly the state up to this client's first
    // queued chunk.
    let (tx, rx) = channel::<Out>();
    let snapshot = {
        let mut h = hub.lock().unwrap();
        let snap = h.screen.snapshot();
        h.clients.push(tx);
        snap
    };
    if Frame::Snapshot(snapshot).encode(&mut wstream).is_err() {
        return;
    }

    // Writer thread: broadcast -> Data/Exit frames. On Exit, flush then
    // shut the socket down so the client sees the exit code followed by a
    // clean EOF immediately. The reader's teardown waits on `gate` until
    // this thread finishes, so the Exit frame can't be lost to a race.
    {
        let mut count = gate.0.lock().unwrap();
        *count += 1;
    }
    let writer_gate = gate.clone();
    thread::spawn(move || {
        while let Ok(out) = rx.recv() {
            let (frame, is_exit) = match out {
                Out::Data(chunk) => (Frame::Data(chunk), false),
                Out::Exit(code) => (Frame::Exit(code), true),
            };
            if frame.encode(&mut wstream).is_err() {
                break;
            }
            if is_exit {
                let _ = wstream.shutdown(std::net::Shutdown::Both);
                break;
            }
        }
        let (count, cvar) = &*writer_gate;
        *count.lock().unwrap() -= 1;
        cvar.notify_all();
    });

    // This thread: client -> Input / Resize / Hello.
    let mut rstream = stream;
    loop {
        match Frame::decode(&mut rstream) {
            Ok(Some(Frame::Input(bytes))) => {
                let mut w = writer.lock().unwrap();
                if w.write_all(&bytes).and_then(|_| w.flush()).is_err() {
                    break;
                }
            }
            Ok(Some(Frame::Resize { cols, rows })) | Ok(Some(Frame::Hello { cols, rows })) => {
                let _ = master.lock().unwrap().resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                });
                hub.lock().unwrap().screen.resize(cols, rows);
            }
            // Signal/DataAck/Unknown: no-ops in this increment (flow
            // control + signal forwarding land in phase 5 / later).
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn cloexec(fd: i32) -> bool {
        // SAFETY: F_GETFD on an fd the test owns.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(flags >= 0, "{}", std::io::Error::last_os_error());
        flags & libc::FD_CLOEXEC != 0
    }

    #[test]
    fn guest_channel_sockets_are_close_on_exec() {
        use std::os::fd::AsRawFd as _;
        // Accepted connections (accept4 is family-agnostic; vsock loopback may be absent).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("l.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let _client = UnixStream::connect(&path).unwrap();
        let conn = accept_cloexec(listener.as_raw_fd()).unwrap();
        assert!(cloexec(conn.as_raw_fd()));
        // The vsock listener, where this kernel has AF_VSOCK.
        match vsock_listen(40_000 + std::process::id() % 20_000) {
            Ok(lfd) => {
                assert!(cloexec(lfd));
                // SAFETY: closing the listener this test opened.
                unsafe { libc::close(lfd) };
            }
            Err(e) => eprintln!("skipping the vsock listener check: {e:#}"),
        }
    }
}
