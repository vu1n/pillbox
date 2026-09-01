//! Programmatic (non-terminal) client of the pty-host [`Frame`] protocol — the
//! **drive-surface** engine.
//!
//! Where [`super::pump::attach_terminal`] drives a *human's* TTY (raw mode,
//! SIGWINCH, Ctrl-A detach), this drives a detached session from **code**: push
//! input (`SendInput`) and consume the output stream (`Subscribe`). It's the
//! piece that lets a non-human driver — an orchestrator, a Slack/Discord
//! bridge, an IDE/ADE — drive an interactive (subscription-billed) agent that
//! has no terminal attached. The pty-host already fans out to N concurrent
//! clients (each gets a `Snapshot` then the live `Data`/`Exit` stream), so a
//! driver, a human pump, and a read-only subscriber can coexist on one session.
//!
//! This is the foundational vNext §0 surface (see
//! [[pillbox-driven-interactive-keystone]]): interactive-and-drivable, not
//! `claude -p` one-shots and not full-autonomous. Completeness is the driver's
//! job, not pillbox's — so the primitives here are deliberately dumb: bytes in,
//! bytes out; the driver decides when a turn is done and what to send next.
//!
//! Built contract-first: the runtime consumers are the `session send` /
//! `session subscribe` verbs (+ the local subscribe socket) — the next slice.
#![allow(dead_code)]

use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::time::Duration;

use super::frame::Frame;

/// How long to wait for the relay to connect to the pty-host (signalled by the
/// host's on-connect `Snapshot` frame) before giving up. Generous because the
/// relay rides the attach transport — for `docker://` that's `docker exec` over
/// an SSH tunnel (`docker system dial-stdio`), whose per-call cold-start was
/// measured at ~13s against a real VPS. 30s leaves headroom without hanging a
/// dead session forever. (An SSH `ControlMaster` on the endpoint would collapse
/// this to a local exec's latency — a separate transport optimization.)
const DRIVE_CONNECT_DEADLINE: Duration = Duration::from_secs(30);
/// After the relay is connected, how long to let the buffered `Input` traverse
/// relay → socket → pty-host → PTY before tearing the exec down. All in-sandbox
/// once connected, so this is short.
const DRIVE_SETTLE: Duration = Duration::from_millis(300);

/// Push bytes to the agent's stdin — the `SendInput` half. Encodes one
/// `Frame::Input`; the pty-host writes it straight to the PTY, exactly as if a
/// human had typed it. The caller frames "messages" however it likes (e.g. a
/// trailing `\n` to submit a prompt).
pub(crate) fn send_input(w: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    Frame::Input(bytes.to_vec()).encode(w)
}

/// One-shot `SendInput` over an already-spawned pty-relay child (a
/// `docker exec … pillbox pty-relay`, local or endpoint-aware): buffer one
/// `Input` frame, wait until the relay has actually connected to the pty-host,
/// let the frame land, then tear the exec down. The transport (which daemon,
/// which endpoint) is the caller's job; this owns the *protocol*.
///
/// The relay forwards our buffered frame only once its `connect_retry` to the
/// pty-host socket succeeds, so we can't tear down on a blind timer — over an
/// SSH-tunnelled `docker exec` (docker://) the relay can take well over a second
/// just to start and connect, and killing it first drops the input on the
/// floor. Instead we gate on the pty-host's **on-connect `Snapshot` frame**:
/// once we've read it, the relay is connected and its stdin→socket pump is live,
/// so the buffered `Input` is (or is about to be) forwarded — a short settle
/// then covers the in-sandbox relay→socket→PTY hop. There's no `DataAck` yet, so
/// this is delivery-confirmed-to-connect, not confirmed-to-PTY; a future ack
/// frame would close that gap. `pillbox session send`.
pub(crate) fn drive_once(mut child: std::process::Child, bytes: &[u8]) -> io::Result<()> {
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "relay stdin unavailable"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "relay stdout unavailable"))?;

    // Buffer the frame; the relay forwards it once connected. Keep stdin open so
    // a slow-to-connect relay isn't EOF'd (and killed) before it forwards.
    send_input(&mut stdin, bytes)?;
    stdin.flush().ok();

    // Wait for the host's on-connect Snapshot via a reader thread — std's
    // blocking read has no timeout, so a dead session would otherwise hang. The
    // thread keeps draining afterward so the relay's socket→stdout side never
    // blocks on a full pipe (claude redraws a screenful on input) during the
    // settle; it ends when we kill the child below (stdout closes → read EOF).
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut r = stdout;
        let _ = tx.send(matches!(Frame::decode(&mut r), Ok(Some(_))));
        let mut buf = [0u8; 8192];
        while r.read(&mut buf).map(|n| n > 0).unwrap_or(false) {}
    });
    let _connected = rx.recv_timeout(DRIVE_CONNECT_DEADLINE).unwrap_or(false);

    std::thread::sleep(DRIVE_SETTLE);
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();
    Ok(())
}

/// Stream the agent's output — the `Subscribe` half. Decodes frames and hands
/// each `Snapshot`/`Data` chunk to `sink`; `sink` returns `false` to stop
/// reading (e.g. the driver saw enough / the turn went quiescent). Returns
/// `Some(exit_code)` if the agent exited, or `None` on a sink-requested stop or
/// a clean EOF. Resize/Signal/Hello/DataAck/Unknown frames are ignored — a
/// driver consuming output doesn't act on terminal-control chatter.
///
/// Deliberately *streaming* and *dumb*: it doesn't decide when a turn is done
/// (the driver does, via `sink`). For a blocking, deadline-bounded read the
/// caller sets a read timeout on `r` and treats the resulting error as "no more
/// output right now."
pub(crate) fn read_output<R: Read>(
    r: &mut R,
    mut sink: impl FnMut(&[u8]) -> bool,
) -> io::Result<Option<i32>> {
    loop {
        match Frame::decode(r)? {
            Some(Frame::Snapshot(b)) | Some(Frame::Data(b)) => {
                if !sink(&b) {
                    return Ok(None);
                }
            }
            Some(Frame::Exit(code)) => return Ok(Some(code)),
            Some(_) => {} // Resize/Signal/Hello/DataAck/Unknown: not a driver's concern
            None => return Ok(None), // peer closed between frames
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_input_encodes_one_input_frame() {
        let mut buf = Vec::new();
        send_input(&mut buf, b"echo hi\n").unwrap();
        let decoded = Frame::decode(&mut &buf[..]).unwrap();
        assert_eq!(decoded, Some(Frame::Input(b"echo hi\n".to_vec())));
    }

    #[test]
    fn read_output_accumulates_snapshot_and_data_until_exit() {
        // A pty-host gives a late-joining client a Snapshot, then live Data,
        // then Exit — assert the driver collects the visible bytes and the code.
        let mut stream = Vec::new();
        Frame::Snapshot(b"[screen]".to_vec())
            .encode(&mut stream)
            .unwrap();
        Frame::Data(b"hello".to_vec()).encode(&mut stream).unwrap();
        Frame::Resize { cols: 80, rows: 24 }
            .encode(&mut stream)
            .unwrap(); // ignored
        Frame::Data(b" world".to_vec()).encode(&mut stream).unwrap();
        Frame::Exit(7).encode(&mut stream).unwrap();

        let mut out = Vec::new();
        let code = read_output(&mut &stream[..], |b| {
            out.extend_from_slice(b);
            true
        })
        .unwrap();
        assert_eq!(out, b"[screen]hello world");
        assert_eq!(code, Some(7), "exit code surfaces to the driver");
    }

    #[test]
    fn read_output_stops_when_sink_requests() {
        // A driver that's seen its marker stops mid-stream (the turn is "done"
        // by the driver's judgment, not pillbox's).
        let mut stream = Vec::new();
        Frame::Data(b"MARKER".to_vec()).encode(&mut stream).unwrap();
        Frame::Data(b"more-that-should-not-be-read".to_vec())
            .encode(&mut stream)
            .unwrap();

        let mut out = Vec::new();
        let code = read_output(&mut &stream[..], |b| {
            out.extend_from_slice(b);
            !out.windows(6).any(|w| w == b"MARKER") // stop once MARKER seen
        })
        .unwrap();
        assert_eq!(out, b"MARKER");
        assert_eq!(code, None, "sink-requested stop is not an exit");
    }

    #[test]
    fn read_output_clean_eof_is_none() {
        let empty: &[u8] = &[];
        let code = read_output(&mut &empty[..], |_| true).unwrap();
        assert_eq!(code, None);
    }
}
