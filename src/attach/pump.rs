//! The shared terminal front-end pump. Drives the attach transport from a
//! real terminal: writes `Snapshot`/`Data` to the tty, sends `Hello`/
//! `Input`/`Resize` from stdin + SIGWINCH, detaches on `Ctrl-A D`, and
//! returns how the session ended.
//!
//! The read/write halves are separate types, so the same pump serves a
//! cloned `UnixStream` pair (local) and a child process's `stdout`/`stdin`
//! (docker exec / ssh transports). The embedder front-end (phase 5) is the
//! other consumer of the same frames; this is the human one.

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
    /// The user detached (Ctrl-A D); the session keeps running.
    Detached,
    /// The pipe closed without an Exit frame (host gone / transport dropped).
    Disconnected,
}

/// Connect a real terminal to a pty-host listening on a local unix socket.
pub(crate) fn attach_unix(sock: &str) -> Result<Outcome> {
    let stream = std::os::unix::net::UnixStream::connect(sock)
        .with_context(|| format!("connecting to pty-host at {sock}"))?;
    let read_half = stream.try_clone().context("cloning socket read half")?;
    attach_terminal(read_half, stream)
}

/// Attach a real terminal and pump until the agent exits or the user
/// detaches. Returns the [`Outcome`] so callers can propagate the agent's
/// exit status (non-detached run) or leave the session running (detach).
pub(crate) fn attach_terminal<R, W>(read_half: R, write_half: W) -> Result<Outcome>
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
    // stdin runs in its own thread: when the agent exits, the reader resolves
    // the outcome immediately rather than waiting for the next keypress.
    {
        let (writer, done, otx) = (writer.clone(), done.clone(), otx);
        thread::spawn(move || {
            if pump_stdin(&writer, &done) {
                let _ = otx.send(Outcome::Detached);
            }
        });
    }

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

/// stdin -> Input frames, with the Ctrl-A detach prefix interpreted
/// (Ctrl-A D detaches; Ctrl-A Ctrl-A sends a literal Ctrl-A through).
/// Returns true if the user detached.
fn pump_stdin<W: Write + Send + 'static>(writer: &Arc<Mutex<W>>, done: &Arc<AtomicBool>) -> bool {
    let mut inp = stdin();
    let mut byte = [0u8; 1];
    let mut pending_ctrl_a = false;
    while !done.load(Ordering::SeqCst) {
        match inp.read(&mut byte) {
            Ok(0) | Err(_) => return false,
            Ok(_) => {
                let b = byte[0];
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
