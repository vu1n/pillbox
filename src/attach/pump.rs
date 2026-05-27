//! The shared terminal front-end pump. Drives the attach transport from a
//! real terminal: writes `Snapshot`/`Data` to the tty, sends `Hello`/
//! `Input`/`Resize` from stdin + SIGWINCH, and returns how the session ended.
//!
//! `detach_enabled` distinguishes the two callers:
//!   - **reattach** (`session attach`): detach IS meaningful — there's a
//!     persisted session to leave running — so `Ctrl-A D` and SIGTERM
//!     resolve as `Detached` (clean detach), and a SIGTERM handler restores
//!     the terminal.
//!   - **foreground run**: there's no session to reattach to, so detach is
//!     OFF — `Ctrl-A` passes through to the agent (readline beginning-of-line
//!     etc.), and SIGTERM keeps its default disposition (terminate), so the
//!     run stays killable and a stray Ctrl-A D can't silently destroy it.
//!
//! The read/write halves are separate types, so the same pump serves a
//! cloned `UnixStream` pair (local) and a child process's `stdout`/`stdin`
//! (docker exec / ssh transports).

use std::io::{stdin, stdout, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

use super::frame::Frame;

const CTRL_A: u8 = 0x01;

/// How an attached session ended.
pub(crate) enum Outcome {
    /// The agent/PTY exited with this code.
    Exited(i32),
    /// The user detached (Ctrl-A D / SIGTERM); the session keeps running.
    Detached,
    /// The pipe closed without an Exit frame (host gone / transport dropped).
    Disconnected,
}

/// Connect a real terminal to a pty-host listening on a local unix socket.
/// This is an explicit attach, so detach is enabled.
pub(crate) fn attach_unix(sock: &str) -> Result<Outcome> {
    let stream = std::os::unix::net::UnixStream::connect(sock)
        .with_context(|| format!("connecting to pty-host at {sock}"))?;
    let read_half = stream.try_clone().context("cloning socket read half")?;
    attach_terminal(read_half, stream, true)
}

/// Attach a real terminal and pump until the agent exits or the user
/// detaches. See the module docs for `detach_enabled`.
pub(crate) fn attach_terminal<R, W>(
    read_half: R,
    write_half: W,
    detach_enabled: bool,
) -> Result<Outcome>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let writer = Arc::new(Mutex::new(write_half));
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    send(&writer, Frame::Hello { cols, rows });

    let done = Arc::new(AtomicBool::new(false));
    let (otx, orx) = mpsc::channel::<Outcome>();

    // pipe -> stdout (Snapshot + Data); resolve the outcome on Exit/EOF.
    {
        let (done, otx) = (done.clone(), otx.clone());
        let mut rh = read_half;
        thread::spawn(move || {
            let mut out = stdout();
            let outcome = loop {
                match Frame::decode(&mut rh) {
                    Ok(Some(Frame::Snapshot(b))) | Ok(Some(Frame::Data(b))) => {
                        if out.write_all(&b).and_then(|_| out.flush()).is_err() {
                            break Outcome::Disconnected;
                        }
                    }
                    Ok(Some(Frame::Exit(code))) => break Outcome::Exited(code),
                    Ok(None) | Err(_) => break Outcome::Disconnected,
                    Ok(Some(_)) => {}
                }
            };
            done.store(true, Ordering::SeqCst);
            let _ = otx.send(outcome);
        });
    }

    // SIGWINCH-ish: poll terminal size, send Resize on change.
    {
        let (writer, done) = (writer.clone(), done.clone());
        thread::spawn(move || {
            let mut last = (cols, rows);
            while !done.load(Ordering::SeqCst) {
                if let Ok(sz) = crossterm::terminal::size() {
                    if sz != last {
                        last = sz;
                        send(
                            &writer,
                            Frame::Resize {
                                cols: sz.0,
                                rows: sz.1,
                            },
                        );
                    }
                }
                thread::sleep(Duration::from_millis(150));
            }
        });
    }

    crossterm::terminal::enable_raw_mode().ok();
    // SIGTERM -> clean Detached, only when detach is meaningful. The guard
    // restores the default disposition + closes its pipe on return, so the
    // handler doesn't outlive the pump (no swallowed SIGTERM, no fd leak).
    #[cfg(unix)]
    let _sigterm_guard = detach_enabled
        .then(|| install_sigterm_detach(otx.clone()))
        .flatten();
    // stdin runs in its own thread: when the agent exits, the reader resolves
    // the outcome immediately rather than waiting for the next keypress.
    {
        let (writer, done, otx) = (writer.clone(), done.clone(), otx.clone());
        thread::spawn(move || {
            if pump_stdin(&writer, &done, detach_enabled) {
                let _ = otx.send(Outcome::Detached);
            }
        });
    }
    drop(otx); // only the receiver remains in this thread

    let outcome = orx.recv().unwrap_or(Outcome::Disconnected);
    done.store(true, Ordering::SeqCst);
    crossterm::terminal::disable_raw_mode().ok();
    // Leave the alt screen + restore the cursor locally so the user's shell
    // prompt comes back cleanly regardless of where the agent left it.
    let mut out = stdout();
    let _ = out.write_all(b"\x1b[?1049l\x1b[?25h");
    let _ = out.flush();
    Ok(outcome)
}

/// stdin -> Input frames. When `detach_enabled`, `Ctrl-A D` detaches and
/// `Ctrl-A Ctrl-A` sends a literal Ctrl-A; returns true on detach. When
/// disabled, every byte (including Ctrl-A) passes straight through.
fn pump_stdin<W: Write + Send + 'static>(
    writer: &Arc<Mutex<W>>,
    done: &Arc<AtomicBool>,
    detach_enabled: bool,
) -> bool {
    let mut inp = stdin();
    let mut byte = [0u8; 1];
    let mut pending_ctrl_a = false;
    while !done.load(Ordering::SeqCst) {
        match inp.read(&mut byte) {
            Ok(0) | Err(_) => return false,
            Ok(_) => {
                let b = byte[0];
                if !detach_enabled {
                    send(writer, Frame::Input(vec![b]));
                    continue;
                }
                if pending_ctrl_a {
                    pending_ctrl_a = false;
                    match b {
                        b'd' | b'D' => {
                            send(writer, Frame::Signal("detach".into()));
                            return true;
                        }
                        CTRL_A => send(writer, Frame::Input(vec![CTRL_A])),
                        other => send(writer, Frame::Input(vec![CTRL_A, other])),
                    }
                } else if b == CTRL_A {
                    pending_ctrl_a = true;
                } else {
                    send(writer, Frame::Input(vec![b]));
                }
            }
        }
    }
    false
}

fn send<W: Write>(writer: &Arc<Mutex<W>>, frame: Frame) {
    let _ = frame.encode(&mut *writer.lock().unwrap());
}

// ─── SIGTERM clean-detach (unix) ────────────────────────────────────────

/// Write end of the SIGTERM self-pipe; read by the guard's thread. -1 when
/// no pump has detach enabled. A signal handler may only touch
/// async-signal-safe state, so we stash a raw fd here and `write()` to it.
#[cfg(unix)]
static SIGTERM_PIPE_W: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

#[cfg(unix)]
extern "C" fn handle_sigterm(_sig: libc::c_int) {
    let fd = SIGTERM_PIPE_W.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = [1u8];
        // SAFETY: write() is async-signal-safe; one byte to our own pipe.
        unsafe { libc::write(fd, byte.as_ptr() as *const libc::c_void, 1) };
    }
}

/// RAII handle for the SIGTERM detach wiring. On drop it restores the default
/// SIGTERM disposition, clears the static, and tears down the self-pipe +
/// reader thread — so the handler never outlives the pump.
#[cfg(unix)]
struct SigtermGuard {
    read_fd: i32,
    write_fd: i32,
    reader: Option<thread::JoinHandle<()>>,
}

#[cfg(unix)]
impl Drop for SigtermGuard {
    fn drop(&mut self) {
        // SAFETY: restore the default disposition so a later SIGTERM kills
        // the process normally instead of writing to a dead pipe.
        unsafe { libc::signal(libc::SIGTERM, libc::SIG_DFL) };
        SIGTERM_PIPE_W.store(-1, Ordering::Relaxed);
        // Closing the write end gives the reader thread EOF; join it, then
        // close the read end (now nobody is blocked on it).
        unsafe { libc::close(self.write_fd) };
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
        unsafe { libc::close(self.read_fd) };
    }
}

#[cfg(unix)]
fn install_sigterm_detach(otx: mpsc::Sender<Outcome>) -> Option<SigtermGuard> {
    let mut fds = [0i32; 2];
    // SAFETY: pipe() fills the 2-element array; we check the return code.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return None;
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);
    SIGTERM_PIPE_W.store(write_fd, Ordering::Relaxed);
    // SAFETY: install our async-signal-safe handler for SIGTERM. Cast via a
    // typed fn pointer first (a direct item→integer cast is a clippy lint).
    let handler = handle_sigterm as extern "C" fn(libc::c_int);
    unsafe { libc::signal(libc::SIGTERM, handler as libc::sighandler_t) };
    let reader = thread::spawn(move || {
        let mut buf = [0u8; 1];
        // SAFETY: blocking read on our own pipe's read end. Returns 1 on a
        // delivered SIGTERM, 0 on EOF (guard closed the write end at teardown).
        let n = unsafe { libc::read(read_fd, buf.as_mut_ptr() as *mut libc::c_void, 1) };
        if n > 0 {
            let _ = otx.send(Outcome::Detached);
        }
    });
    Some(SigtermGuard {
        read_fd,
        write_fd,
        reader: Some(reader),
    })
}
