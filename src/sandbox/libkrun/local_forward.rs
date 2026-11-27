//! Plain TCP host-loopback forwards for the libkrun egress stack — the local-model
//! and host-MCP escape hatch.
//!
//! Where [`super::mitm`] terminates an allowlisted host's TLS and forwards to an
//! external upstream, this is a **plaintext passthrough** from a guest TCP socket
//! to a service on the *host's loopback* (e.g. an ollama server on
//! `127.0.0.1:11434`). The VMM child runs on the host, so `127.0.0.1` here IS the
//! host. No TLS, no DNS pin, no credential swap — bytes relay as-is.
//!
//! **Opt-in only.** It is configured with a set of ports, each one deliberately
//! punching a hole in the default-deny fence: a LOCAL model the host runs (no API
//! throttle → real eval parallelism; the local-worker thesis) or a host-loopback
//! `--mcp` server. When no port is configured this module is inert. Guests route
//! `gateway:PORT` here via the default route; a smoltcp listener on `PORT` accepts
//! it and dials `127.0.0.1:PORT`. Each port keeps its own listener pool, so a busy
//! port can't take another port's slots.

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
    port: u16,
    handle: SocketHandle,
    host: Option<TcpStream>,
    to_host: Vec<u8>, // guest→host bytes awaiting a (possibly WouldBlock) host write
    to_guest: Vec<u8>, // host→guest bytes awaiting smoltcp send-buffer space
    host_eof: bool,
    closing: bool,
}

/// Keep `POOL_MIN_FREE` listeners ready on every port in `ports`.
pub(super) fn replenish(forwarders: &mut Vec<Forwarder>, sockets: &mut SocketSet, ports: &[u16]) {
    for &port in ports {
        replenish_port(forwarders, sockets, port);
    }
}

// The pool bound is per port, so one saturated forward never starves another.
fn replenish_port(forwarders: &mut Vec<Forwarder>, sockets: &mut SocketSet, port: u16) {
    let on_port = || forwarders.iter().filter(|f| f.port == port);
    let free = on_port()
        .filter(|f| sockets.get::<tcp::Socket>(f.handle).state() == tcp::State::Listen)
        .count();
    let total = on_port().count();
    for added in 0..POOL_MIN_FREE.saturating_sub(free) {
        if total + added >= POOL_MAX {
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
            port,
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
pub(super) fn drive(forwarders: &mut [Forwarder], sockets: &mut SocketSet, diag: &Diag) {
    for f in forwarders.iter_mut() {
        let port = f.port;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn listening(forwarders: &[Forwarder], sockets: &SocketSet, port: u16) -> usize {
        forwarders
            .iter()
            .filter(|f| f.port == port)
            .filter(|f| sockets.get::<tcp::Socket>(f.handle).state() == tcp::State::Listen)
            .count()
    }

    #[test]
    fn every_port_gets_its_own_pool() {
        let mut sockets = SocketSet::new(vec![]);
        let mut forwarders = Vec::new();
        replenish(&mut forwarders, &mut sockets, &[11434, 7777]);
        assert_eq!(listening(&forwarders, &sockets, 11434), POOL_MIN_FREE);
        assert_eq!(listening(&forwarders, &sockets, 7777), POOL_MIN_FREE);
        // Replenishing again adds nothing: the pools are already full.
        replenish(&mut forwarders, &mut sockets, &[11434, 7777]);
        assert_eq!(forwarders.len(), 2 * POOL_MIN_FREE);
    }

    #[test]
    fn a_saturated_port_does_not_starve_another() {
        let mut sockets = SocketSet::new(vec![]);
        let mut forwarders = Vec::new();
        // Fill port A to its cap with busy (non-listening) sockets.
        for _ in 0..POOL_MAX {
            replenish(&mut forwarders, &mut sockets, &[1111]);
            for f in forwarders.iter().filter(|f| f.port == 1111) {
                sockets.get_mut::<tcp::Socket>(f.handle).abort();
            }
        }
        assert_eq!(
            forwarders.iter().filter(|f| f.port == 1111).count(),
            POOL_MAX
        );
        replenish(&mut forwarders, &mut sockets, &[1111, 2222]);
        assert_eq!(listening(&forwarders, &sockets, 2222), POOL_MIN_FREE);
        assert_eq!(
            forwarders.iter().filter(|f| f.port == 1111).count(),
            POOL_MAX
        );
    }
}
