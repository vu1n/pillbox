//! The shared terminal front-end pump. Drives a [`FramePipe`] from a real
//! terminal: writes `Snapshot`/`Data` to the tty, sends `Hello`/`Input`/
//! `Resize` from stdin + SIGWINCH, and detaches on `Ctrl-A D`.
//!
//! It is generic over the pipe, so the same pump serves a local unix-socket
//! attach and (later) a docker-exec / ssh / e2b transport — only the pipe
//! differs. The embedder front-end (phase 5) is the other consumer of the
//! same frames; this is the human one.

use std::io::{stdin, stdout, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

use super::frame::Frame;

const CTRL_A: u8 = 0x01;

/// Connect a real terminal to a pty-host listening on a local unix socket.
/// Used by `pillbox pty-attach` and (until the docker transport lands) the
/// local interactive path.
pub(crate) fn attach_unix(sock: &str) -> Result<()> {
    let stream = std::os::unix::net::UnixStream::connect(sock)
        .with_context(|| format!("connecting to pty-host at {sock}"))?;
    let read_half = stream.try_clone().context("cloning socket read half")?;
    attach_terminal(read_half, stream)
}

/// Attach a real terminal to `pipe` and pump until the agent exits or the
/// user detaches (Ctrl-A D). `pipe` must be cloneable into independent
/// read/write halves (UnixStream and the transport stdios all are).
pub(crate) fn attach_terminal<P>(read_half: P, write_half: P) -> Result<()>
where
    P: Read + Write + Send + 'static,
{
    let writer = Arc::new(Mutex::new(write_half));
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    send(&writer, Frame::Hello { cols, rows });

    let done = Arc::new(AtomicBool::new(false));

    // pipe -> stdout (Snapshot + Data). Owns the read half.
    {
        let done = done.clone();
        let mut rh = read_half;
        thread::spawn(move || {
            let mut out = stdout();
            loop {
                match Frame::decode(&mut rh) {
                    Ok(Some(Frame::Snapshot(b))) | Ok(Some(Frame::Data(b))) => {
                        if out.write_all(&b).and_then(|_| out.flush()).is_err() {
                            break;
                        }
                    }
                    Ok(Some(Frame::Exit(_))) | Ok(None) | Err(_) => break,
                    Ok(Some(_)) => {}
                }
            }
            done.store(true, Ordering::SeqCst);
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
    let result = pump_stdin(&writer, &done);
    crossterm::terminal::disable_raw_mode().ok();
    // Leave the alt screen + restore the cursor locally so the user's
    // shell prompt comes back cleanly regardless of where the agent left it.
    let mut out = stdout();
    let _ = out.write_all(b"\x1b[?1049l\x1b[?25h");
    let _ = out.flush();
    result
}

/// stdin -> Input frames, with the Ctrl-A detach prefix interpreted
/// (Ctrl-A D detaches; Ctrl-A Ctrl-A sends a literal Ctrl-A through).
fn pump_stdin<P: Read + Write + Send + 'static>(
    writer: &Arc<Mutex<P>>,
    done: &Arc<AtomicBool>,
) -> Result<()> {
    let mut inp = stdin();
    let mut byte = [0u8; 1];
    let mut pending_ctrl_a = false;
    while !done.load(Ordering::SeqCst) {
        match inp.read(&mut byte) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let b = byte[0];
                if pending_ctrl_a {
                    pending_ctrl_a = false;
                    match b {
                        b'd' | b'D' => {
                            send(writer, Frame::Signal("detach".into()));
                            break;
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
    Ok(())
}

fn send<P: Write>(writer: &Arc<Mutex<P>>, frame: Frame) {
    let _ = frame.encode(&mut *writer.lock().unwrap());
}
