//! pty-relay: a verbatim bidirectional byte pump between a unix socket and
//! this process's stdio.
//!
//! Run *inside* a sandbox by the per-attach transport (`docker exec -i`,
//! `ssh`, an e2b command) so one client's frame stream reaches the
//! pty-host's socket. It deliberately does not parse frames — both ends
//! speak the same protocol; the relay only copies bytes. This is what makes
//! one pty-host serve N clients: each transport connection runs its own
//! relay against the same socket.

use std::io::copy;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::thread;

use anyhow::{Context, Result};

pub(crate) fn run(sock: &str) -> Result<()> {
    let mut to_sock =
        UnixStream::connect(sock).with_context(|| format!("connecting to pty-host at {sock}"))?;
    let mut from_sock = to_sock.try_clone().context("cloning socket")?;

    // stdin -> socket. On stdin EOF, half-close the socket so the host sees
    // this client disconnect (and prunes it) without tearing the host down.
    let pump_in = thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let _ = copy(&mut stdin, &mut to_sock);
        let _ = to_sock.shutdown(Shutdown::Write);
    });

    // socket -> stdout. Ends when the host closes the connection.
    let mut stdout = std::io::stdout();
    let _ = copy(&mut from_sock, &mut stdout);
    let _ = pump_in.join();
    Ok(())
}
