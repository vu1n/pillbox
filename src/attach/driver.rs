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

use super::frame::Frame;

/// Push bytes to the agent's stdin — the `SendInput` half. Encodes one
/// `Frame::Input`; the pty-host writes it straight to the PTY, exactly as if a
/// human had typed it. The caller frames "messages" however it likes (e.g. a
/// trailing `\n` to submit a prompt).
pub(crate) fn send_input(w: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    Frame::Input(bytes.to_vec()).encode(w)
}

/// One-shot `SendInput` over an already-spawned pty-relay child (a
/// `docker exec … pillbox pty-relay`, local or endpoint-aware): write one
/// `Input` frame, then EOF the relay so it forwards the buffered frame and
/// exits. The transport (which daemon, which endpoint) is the caller's job;
/// this owns the *protocol*: there's no `DataAck` frame yet, so after EOF we
/// wait a bounded beat for the pty-host to apply the bytes to the PTY before
/// tearing the exec down — a timed best-effort, not a delivery confirmation. A
/// future ack frame turns this into a real round-trip. `pillbox session send`.
pub(crate) fn drive_once(mut child: std::process::Child, bytes: &[u8]) -> io::Result<()> {
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "relay stdin unavailable"))?;
    send_input(&mut stdin, bytes)?;
    stdin.flush().ok();
    drop(stdin); // EOF the relay once it has read the buffered frame
    std::thread::sleep(std::time::Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();
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
