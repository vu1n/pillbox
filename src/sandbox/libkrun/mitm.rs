//! The L7 TLS MITM pump for the libkrun egress stack.
//!
//! Where [`super::egress`] is the L3/L5 stack (virtio-net + smoltcp + the DNS
//! fence), this is the L7 termination + forward that sits on its TCP sockets: a
//! self-replenishing pool of `:443` listeners, and per-connection driving that
//! terminates the guest's TLS (the [`Vault`]'s leaf), gates on the DNS-pin, swaps
//! the stubbed credential for the real one ([`StubSwap`], the env fork), and
//! relays to the real upstream ([`Upstream`]). The egress poll loop calls
//! [`replenish_listeners`] + [`drive_listeners`] each tick on the shared
//! `SocketSet`; everything here runs on that one thread in the VMM child.

use std::io::{Read, Write};

use std::sync::mpsc;

use smoltcp::iface::{SocketHandle, SocketSet};
use smoltcp::socket::tcp;

use super::egress::{Diag, PinTable};
use super::vault::{CredSwap, StubSwap, Upstream, Vault};

/// The MITM listens here; allowlisted names resolve to the gateway, so their TLS
/// lands on these sockets. A self-replenishing pool keeps free listeners ready.
const PROXY_PORT: u16 = 443;
const POOL_MIN_FREE: usize = 8;
const POOL_MAX: usize = 32;

/// One pooled TCP socket listening on `:443`, plus the rustls session driving any
/// connection accepted on it.
pub(super) struct Listener {
    handle: SocketHandle,
    conn: Option<Conn>,
}

/// Per-connection MITM state. `host` is the pinned SNI, set once the gate passes
/// — empty means the gate hasn't run yet (the deny path aborts the socket, so a
/// gated-but-empty state never persists). `upstream` is opened after the gate;
/// the pump then relays plaintext between the guest TLS and the upstream TLS.
struct Conn {
    tls: rustls::ServerConnection,
    host: String,
    upstream: Option<Upstream>,
    /// The in-flight upstream connect (spawned off the poll loop, polled each tick
    /// until it yields the `Upstream`). `None` once connected (or before the gate).
    connecting: Option<mpsc::Receiver<Result<Upstream, String>>>,
    /// Stub→real credential substitution applied to the guest→upstream stream.
    swap: StubSwap,
    req_logged: bool,
    closing: bool,
}

/// Keep `POOL_MIN_FREE` listening sockets ready (up to `POOL_MAX`), adding new
/// ones as accepted connections consume the free pool.
pub(super) fn replenish_listeners(listeners: &mut Vec<Listener>, sockets: &mut SocketSet) {
    let free = listeners
        .iter()
        .filter(|l| sockets.get::<tcp::Socket>(l.handle).state() == tcp::State::Listen)
        .count();
    for _ in free..POOL_MIN_FREE {
        if listeners.len() >= POOL_MAX {
            break;
        }
        let s = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0u8; 65535]),
            tcp::SocketBuffer::new(vec![0u8; 65535]),
        );
        let handle = sockets.add(s);
        let sock = sockets.get_mut::<tcp::Socket>(handle);
        // Reclaim incomplete/idle connections (no ClientHello, slowloris) so they
        // can't pin a pool slot until the VM dies.
        sock.set_timeout(Some(smoltcp::time::Duration::from_secs(30)));
        sock.listen(PROXY_PORT).expect("listen :443");
        listeners.push(Listener { handle, conn: None });
    }
}

/// Drive every pooled listener: start a rustls session on a fresh connection,
/// pump it, and reset the socket back to listening when it closes.
pub(super) fn drive_listeners(
    listeners: &mut [Listener],
    sockets: &mut SocketSet,
    vault: &Vault,
    pins: &PinTable,
    swap_pairs: &[CredSwap],
    diag: &Diag,
) {
    for l in listeners.iter_mut() {
        let sock = sockets.get_mut::<tcp::Socket>(l.handle);
        if l.conn.is_some() || sock.can_recv() {
            if l.conn.is_none() {
                match vault.new_conn() {
                    Ok(tls) => {
                        l.conn = Some(Conn {
                            tls,
                            host: String::new(),
                            upstream: None,
                            connecting: None,
                            // Empty until the SNI pins — then rebuilt with only the
                            // pairs bound to this host (destination-bound release).
                            // No plaintext is relayed before the gate, so this is safe.
                            swap: StubSwap::new(Vec::new()),
                            req_logged: false,
                            closing: false,
                        })
                    }
                    Err(_) => sock.abort(),
                }
            }
            if let Some(c) = l.conn.as_mut() {
                drive_conn(sock, c, vault, pins, swap_pairs, diag);
            }
        }
        let sock = sockets.get_mut::<tcp::Socket>(l.handle);
        if sock.state() == tcp::State::Closed {
            l.conn = None;
            let _ = sock.listen(PROXY_PORT);
        }
    }
}

/// Build the per-connection swap from only the pairs bound to `host` (the pinned
/// SNI) — destination-bound release. A pair applies only on its own host(s)
/// (case-insensitively); a pair with NO hosts matches nothing and is never
/// released (fail-closed — an unbound credential is a bug, not a wildcard). This
/// is the last line of defense that stops a guest-held stub from being replayed to
/// a different allowlisted host to extract the real credential: the binding is
/// enforced here, in the MITM, not only by the launch-time guards that build the
/// pairs.
fn host_bound_swap(swap_pairs: &[CredSwap], host: &str) -> StubSwap {
    StubSwap::new(
        swap_pairs
            .iter()
            .filter(|p| p.hosts.iter().any(|h| h.eq_ignore_ascii_case(host)))
            .cloned()
            .collect(),
    )
}

/// Pump one MITM session: smoltcp rx → guest rustls, gate on the DNS-pin (the
/// allowlist is already enforced by the cert resolver — a non-allowlisted SNI
/// never got a cert), then relay decrypted bytes to/from the upstream TLS, guest
/// rustls → smoltcp tx. Split-borrows the connection's fields so the guest and
/// upstream sessions can be driven in the same call.
fn drive_conn(
    sock: &mut tcp::Socket,
    c: &mut Conn,
    vault: &Vault,
    pins: &PinTable,
    swap_pairs: &[CredSwap],
    diag: &Diag,
) {
    let Conn {
        tls,
        host,
        upstream,
        connecting,
        swap,
        req_logged,
        closing,
    } = c;

    // smoltcp rx → guest rustls
    while sock.can_recv() {
        let mut got = 0usize;
        let _ = sock.recv(|data| {
            got = tls.read_tls(&mut std::io::Cursor::new(data)).unwrap_or(0);
            (got, ())
        });
        if got == 0 {
            break;
        }
        // A handshake failure lands here — including a non-allowlisted SNI, whose
        // cert the resolver refused to mint. Log the RST (the only place we see it).
        if let Err(e) = tls.process_new_packets() {
            diag.log(&format!("krun-egress: [mitm] TLS error → RST ({e})"));
            sock.abort();
            return;
        }
    }

    // Pin gate (SNI available once the ClientHello is processed): the guest must
    // have resolved this exact host through our resolver — a hardcoded-IP +
    // forged-SNI connection that skipped DNS isn't pinned, so it's denied. `host`
    // empty = gate not yet run; setting it (only on ALLOW) marks the gate passed.
    if host.is_empty() {
        if let Some(sni) = tls.server_name() {
            let sni = sni.to_string();
            if !pins.contains(&sni) {
                diag.log(&format!(
                    "krun-egress: [mitm] DENY sni={sni:?} → RST (SNI not resolved via our resolver)"
                ));
                sock.abort();
                return;
            }
            diag.log(&format!(
                "krun-egress: [mitm] ALLOW sni={sni:?} → DNS-pinned, terminating"
            ));
            *host = sni;
            // Destination-bound release: apply only the credential swaps bound to
            // THIS host, so a stub the guest holds can't be replayed to a different
            // allowlisted host to extract the real.
            *swap = host_bound_swap(swap_pairs, host);
        }
    }

    // Open the forward connection once the gate passes — on a background thread so
    // the blocking resolve/connect doesn't stall the poll loop. Spawn once, then
    // poll the receiver each tick until the upstream (validated against the Mozilla
    // roots) is ready.
    if !host.is_empty() && upstream.is_none() && !*closing {
        if connecting.is_none() {
            *connecting = Some(vault.spawn_connect(host.clone()));
        }
        match connecting.as_ref().unwrap().try_recv() {
            Ok(Ok(up)) => {
                *upstream = Some(up);
                *connecting = None;
            }
            Ok(Err(e)) => {
                diag.log(&format!(
                    "krun-egress: [mitm] upstream {host} failed → RST ({e})"
                ));
                sock.abort();
                return;
            }
            Err(mpsc::TryRecvError::Empty) => {} // still connecting — poll next tick
            Err(mpsc::TryRecvError::Disconnected) => {
                diag.log(&format!(
                    "krun-egress: [mitm] upstream {host} connect thread died → RST"
                ));
                sock.abort();
                return;
            }
        }
    }

    // Relay decrypted bytes both ways: guest request → upstream, upstream response
    // → guest. The env-fork swap (stub→real credential) is applied to the guest's
    // request stream before it reaches the upstream.
    if let Some(up) = upstream.as_mut() {
        let mut plain = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match tls.reader().read(&mut buf) {
                Ok(0) => break,
                Ok(n) => plain.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        if !plain.is_empty() {
            if !*req_logged {
                let head = String::from_utf8_lossy(&plain);
                diag.log(&format!(
                    "krun-egress: [mitm] {:?} → {host} (forwarding{})",
                    head.lines().next().unwrap_or(""),
                    if swap.is_noop() { "" } else { ", cred swapped" }
                ));
                *req_logged = true;
            }
            // Substitute stub→real; flush per drain so no partial-stub tail is held
            // back across ticks (the credential lands in the request head, which
            // arrives whole in one drain).
            let mut out = swap.push(&plain);
            out.extend(swap.flush());
            up.send(&out);
        }
        let alive = up.pump();
        let mut resp = Vec::new();
        up.recv_into(&mut resp);
        if !resp.is_empty() {
            let _ = tls.writer().write_all(&resp);
        }
        if !alive {
            *closing = true;
            *upstream = None;
        }
    }

    // guest rustls → smoltcp tx
    while tls.wants_write() && sock.can_send() {
        let mut wrote = 0usize;
        let _ = sock.send(|mut b| {
            wrote = tls.write_tls(&mut b).unwrap_or(0);
            (wrote, ())
        });
        if wrote == 0 {
            break;
        }
    }
    // Close the guest side once the upstream is gone and its response is flushed.
    if *closing && !tls.wants_write() {
        sock.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(stub: &str, hosts: &[&str]) -> CredSwap {
        CredSwap {
            stub: stub.as_bytes().to_vec(),
            real: b"REAL".to_vec(),
            hosts: hosts.iter().map(|h| h.to_string()).collect(),
        }
    }

    // Full swap output for one input (push then flush — push alone holds a carry
    // tail for stubs that might straddle the next chunk).
    fn full(mut s: StubSwap, input: &[u8]) -> Vec<u8> {
        let mut out = s.push(input);
        out.extend(s.flush());
        out
    }

    // Destination-bound release: a pair fires only on a connection whose pinned
    // SNI is in its host set — a stub bound to host A must NOT be applied on host B.
    #[test]
    fn host_bound_swap_applies_only_matching_pairs() {
        let pairs = vec![
            pair("oauthstub", &["api.anthropic.com", "console.anthropic.com"]),
            pair("ghstub", &["api.github.com"]),
        ];

        // On the github connection, only the github pair is live — the OAuth stub
        // cannot be swapped here (the exfil hole this closes).
        assert_eq!(
            full(host_bound_swap(&pairs, "api.github.com"), b"oauthstub here"),
            b"oauthstub here" // NOT swapped — oauth pair isn't loaded on this host
        );
        assert_eq!(
            full(host_bound_swap(&pairs, "api.github.com"), b"ghstub here"),
            b"REAL here" // swapped (its bound host)
        );

        // On an Anthropic host, the OAuth pair is live, the github pair is not.
        assert_eq!(
            full(
                host_bound_swap(&pairs, "api.anthropic.com"),
                b"oauthstub here"
            ),
            b"REAL here"
        );
        assert_eq!(
            full(host_bound_swap(&pairs, "api.anthropic.com"), b"ghstub here"),
            b"ghstub here" // github pair not loaded on an anthropic host
        );
    }

    #[test]
    fn host_match_is_case_insensitive_and_unbound_is_fail_closed() {
        let bound = vec![pair("s", &["API.Anthropic.COM"])];
        assert!(!host_bound_swap(&bound, "api.anthropic.com").is_noop());
        assert!(host_bound_swap(&bound, "evil.example").is_noop());
        // A pair with no hosts matches NOTHING (fail-closed): an unbound credential
        // is never released, so a stray empty-host pair can't become a wildcard swap.
        let unbound = vec![pair("s", &[])];
        assert!(host_bound_swap(&unbound, "api.anthropic.com").is_noop());
        assert!(host_bound_swap(&unbound, "evil.example").is_noop());
    }
}
