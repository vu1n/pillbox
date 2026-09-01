//! pty-relay: a verbatim bidirectional byte pump between a unix socket and
//! this process's stdio.
//!
//! Run *inside* a sandbox by the per-attach transport (`docker exec -i`,
//! `ssh`, an e2b command) so one client's frame stream reaches the
//! pty-host's socket. It deliberately does not parse frames — both ends
//! speak the same protocol; the relay only copies bytes. This is what makes
//! one pty-host serve N clients: each transport connection runs its own
//! relay against the same socket.

use std::io::{copy, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

pub(crate) fn run(sock: &str) -> Result<()> {
    let to_sock =
        connect_retry(sock).with_context(|| format!("connecting to pty-host at {sock}"))?;
    let from_sock = to_sock.try_clone().context("cloning socket")?;

    // stdin -> socket, in a detached thread. It may block forever on a
    // still-open stdin (the transport keeps it open); that's fine — the
    // process exits when the socket->stdout direction ends below, which is
    // the authoritative "session over" signal. On stdin EOF, half-close the
    // socket so the host prunes this client without tearing the host down.
    {
        let mut to_sock = to_sock;
        thread::spawn(move || {
            let _ = copy(&mut std::io::stdin(), &mut to_sock);
            let _ = to_sock.shutdown(Shutdown::Write);
        });
    }

    // socket -> stdout. Flush every chunk: Rust's stdout is line-buffered
    // even to a pipe, but the frames are binary and the final Exit frame has
    // no trailing newline — without an explicit flush it would sit in the
    // buffer and the client would never see the exit code.
    let mut from = from_sock;
    let mut out = std::io::stdout();
    let mut buf = [0u8; 8192];
    loop {
        match from.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if out.write_all(&buf[..n]).and_then(|_| out.flush()).is_err() {
                    break;
                }
            }
        }
    }
    Ok(())
}

/// The relay starts the moment the transport opens, which can race the
/// pty-host's socket bind inside the sandbox. Retry briefly before failing.
fn connect_retry(sock: &str) -> std::io::Result<UnixStream> {
    let mut last = None;
    for _ in 0..150 {
        match UnixStream::connect(sock) {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
        thread::sleep(Duration::from_millis(20));
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("pty-host socket never appeared")))
}
