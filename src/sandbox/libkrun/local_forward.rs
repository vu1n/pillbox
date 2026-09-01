//! Plain TCP host-loopback forward for the libkrun egress stack — the local-model
//! escape hatch.
//!
//! Where [`super::mitm`] terminates an allowlisted host's TLS and forwards to an
//! external upstream, this is a **plaintext passthrough** from a guest TCP socket
//! to a service on the *host's loopback* (e.g. an ollama server on
//! `127.0.0.1:11434`). The VMM child runs on the host, so `127.0.0.1` here IS the
//! host. No TLS, no DNS pin, no credential swap — bytes relay as-is.
//!
//! **Opt-in only.** It is configured with a single port and deliberately punches a
//! hole in the default-deny fence so a guest agent can reach a LOCAL model the host
//! runs (no API throttle → real eval parallelism; the local-worker thesis). When no
//! port is configured this module is inert. Guests route `gateway:PORT` here via the
//! default route; a smoltcp listener on `PORT` accepts it and dials `127.0.0.1:PORT`.

use std::io::{Read, Write};
use std::net::TcpStream;

use smoltcp::iface::{SocketHandle, SocketSet};
use smoltcp::socket::tcp;

use super::egress::Diag;

const POOL_MIN_FREE: usize = 4;
const POOL_MAX: usize = 16;
const RELAY_BUF: usize = 65535;

/// One pooled listener on the forward port, plus the host-side connection it
/// relays to once a guest connects.
pub(super) struct Forwarder {
    handle: SocketHandle,
    host: Option<TcpStream>,
    to_host: Vec<u8>, // guest→host bytes awaiting a (possibly WouldBlock) host write
    to_guest: Vec<u8>, // host→guest bytes awaiting smoltcp send-buffer space
    host_eof: bool,
    closing: bool,
}

/// Keep `POOL_MIN_FREE` listeners ready (up to `POOL_MAX`) on `port`.
pub(super) fn replenish(forwarders: &mut Vec<Forwarder>, sockets: &mut SocketSet, port: u16) {
    let free = forwarders
        .iter()
        .filter(|f| sockets.get::<tcp::Socket>(f.handle).state() == tcp::State::Listen)
        .count();
    for _ in free..POOL_MIN_FREE {
        if forwarders.len() >= POOL_MAX {
            break;
        }
        let s = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0u8; RELAY_BUF]),
            tcp::SocketBuffer::new(vec![0u8; RELAY_BUF]),
        );
        let handle = sockets.add(s);
        let sock = sockets.get_mut::<tcp::Socket>(handle);
        // Reclaim idle/incomplete connections so they can't pin a slot until VM death.
        sock.set_timeout(Some(smoltcp::time::Duration::from_secs(120)));
        sock.listen(port).expect("listen local-forward port");
        forwarders.push(Forwarder {
            handle,
            host: None,
            to_host: Vec::new(),
            to_guest: Vec::new(),
            host_eof: false,
            closing: false,
        });
    }
}

/// Drive every forwarder one tick: dial the host on a fresh connection, then relay
/// bytes both ways with backpressure buffering (no rustls buffer to lean on here).
/// Reset to listening when the connection closes.
pub(super) fn drive(forwarders: &mut [Forwarder], sockets: &mut SocketSet, port: u16, diag: &Diag) {
    for f in forwarders.iter_mut() {
        let sock = sockets.get_mut::<tcp::Socket>(f.handle);

        // Fresh accepted connection → dial the host loopback service once.
        if f.host.is_none() && !f.closing && sock.can_recv() {
            match TcpStream::connect(("127.0.0.1", port)) {
                Ok(s) => {
                    let _ = s.set_nonblocking(true);
                    diag.log(&format!(
                        "krun-egress: [local-fwd] guest connected → 127.0.0.1:{port}"
                    ));
                    f.host = Some(s);
                }
                Err(e) => {
                    diag.log(&format!(
                        "krun-egress: [local-fwd] dial 127.0.0.1:{port} failed → RST ({e})"
                    ));
                    sock.abort();
                    continue;
                }
            }
        }

        if let Some(host) = f.host.as_mut() {
            let mut tmp = [0u8; 8192];

            // guest → buffer → host
            while sock.can_recv() {
                match sock.recv_slice(&mut tmp) {
                    Ok(0) => break,
                    Ok(n) => f.to_host.extend_from_slice(&tmp[..n]),
                    Err(_) => break,
                }
            }
            while !f.to_host.is_empty() {
                match host.write(&f.to_host) {
                    Ok(0) => break,
                    Ok(n) => {
                        f.to_host.drain(..n);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        f.closing = true;
                        break;
                    }
                }
            }

            // host → buffer → guest
            if !f.host_eof {
                loop {
                    match host.read(&mut tmp) {
                        Ok(0) => {
                            f.host_eof = true;
                            break;
                        }
                        Ok(n) => f.to_guest.extend_from_slice(&tmp[..n]),
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(_) => {
                            f.closing = true;
                            break;
                        }
                    }
                }
            }
            if !f.to_guest.is_empty() && sock.can_send() {
                if let Ok(n) = sock.send_slice(&f.to_guest) {
                    f.to_guest.drain(..n);
                }
            }
        }

        // Half-close cleanly: the host hung up and its bytes are flushed to the guest.
        let sock = sockets.get_mut::<tcp::Socket>(f.handle);
        if (f.host_eof && f.to_guest.is_empty()) || f.closing {
            sock.close();
        }

        // Connection done → drop the host stream and return the socket to the pool.
        if sock.state() == tcp::State::Closed {
            f.host = None;
            f.to_host.clear();
            f.to_guest.clear();
            f.host_eof = false;
            f.closing = false;
            let _ = sock.listen(port);
        }
    }
}
