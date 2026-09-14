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
    /// Holds every HTTP/1.1 request line until it can be classified. Token
    /// endpoints are rejected before their bytes reach an upstream.
    request_gate: RequestGate,
    /// Gated request bytes not yet accepted by the upstream TLS buffer.
    outbound: Vec<u8>,
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
                            request_gate: RequestGate::new(),
                            outbound: Vec::new(),
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

const MAX_REQUEST_LINE_BYTES: usize = 8 * 1024;
const MAX_REQUEST_HEADER_BYTES: usize = 64 * 1024;
/// Bound unsent guest bytes during both connect and upstream backpressure.
/// The fixed listener pool bounds the aggregate; overflow fails closed.
const MAX_PENDING_UPSTREAM_BYTES: usize = 256 * 1024;
const MAX_PLAINTEXT_CHUNK_BYTES: usize = 4 * 1024;

struct RequestGate {
    state: RequestState,
    upstream_allowed: bool,
}

enum RequestState {
    RequestLine(Vec<u8>),
    Headers(Vec<u8>),
    FixedBody(usize),
    ChunkedBody(ChunkedBody),
    Rejected,
}

impl RequestGate {
    fn new() -> Self {
        Self {
            state: RequestState::RequestLine(Vec::new()),
            upstream_allowed: false,
        }
    }

    /// Hold and classify every request line on a keep-alive connection. Request
    /// framing is tracked only to find the next line; allowed bytes otherwise
    /// stream through unchanged. Malformed, ambiguous, or oversized framing
    /// fails closed so it cannot create a request-smuggling bypass.
    fn push(&mut self, host: &str, chunk: &[u8]) -> Result<Option<Vec<u8>>, ()> {
        if matches!(self.state, RequestState::Rejected) {
            return Err(());
        }

        let mut input = chunk.to_vec();
        let mut released = Vec::with_capacity(chunk.len());
        while !input.is_empty() {
            match &mut self.state {
                RequestState::RequestLine(buffer) => {
                    buffer.extend_from_slice(&input);
                    let Some(line_end) = find_bytes(buffer, b"\r\n") else {
                        if buffer.len() > MAX_REQUEST_LINE_BYTES {
                            return self.reject();
                        }
                        break;
                    };
                    let tail = buffer.split_off(line_end + 2);
                    let line = &buffer[..line_end];
                    let mut fields = line.split(|byte| byte.is_ascii_whitespace());
                    let method = fields.next().filter(|field| !field.is_empty());
                    let path = fields.next().filter(|field| !field.is_empty());
                    let version = fields.next().filter(|field| !field.is_empty());
                    if method.is_none()
                        || path.is_none()
                        || !matches!(version, Some(b"HTTP/1.0" | b"HTTP/1.1"))
                        || fields.any(|field| !field.is_empty())
                    {
                        return self.reject();
                    }
                    let Some(path) = path.and_then(|path| std::str::from_utf8(path).ok()) else {
                        return self.reject();
                    };
                    let path = path.split('?').next().unwrap_or(path);
                    if crate::vault::providers::is_oauth_token_endpoint(host, path) {
                        return self.reject();
                    }
                    released.extend_from_slice(buffer);
                    self.upstream_allowed = true;
                    self.state = RequestState::Headers(Vec::new());
                    input = tail;
                }
                RequestState::Headers(buffer) => {
                    buffer.extend_from_slice(&input);
                    let head_len = if buffer.starts_with(b"\r\n") {
                        Some(2)
                    } else {
                        find_bytes(buffer, b"\r\n\r\n").map(|offset| offset + 4)
                    };
                    let Some(head_len) = head_len else {
                        if buffer.len() > MAX_REQUEST_HEADER_BYTES {
                            return self.reject();
                        }
                        break;
                    };
                    let tail = buffer.split_off(head_len);
                    let Some(framing) = request_framing(buffer) else {
                        return self.reject();
                    };
                    released.extend_from_slice(buffer);
                    self.state = match framing {
                        RequestFraming::Empty => RequestState::RequestLine(Vec::new()),
                        RequestFraming::Fixed(length) => RequestState::FixedBody(length),
                        RequestFraming::Chunked => RequestState::ChunkedBody(ChunkedBody::new()),
                    };
                    input = tail;
                }
                RequestState::FixedBody(remaining) => {
                    let take = (*remaining).min(input.len());
                    released.extend_from_slice(&input[..take]);
                    *remaining -= take;
                    input.drain(..take);
                    if *remaining == 0 {
                        self.state = RequestState::RequestLine(Vec::new());
                    }
                }
                RequestState::ChunkedBody(body) => {
                    let Some(consumed) = body.consume(&input) else {
                        return self.reject();
                    };
                    released.extend_from_slice(&input[..consumed.bytes]);
                    input.drain(..consumed.bytes);
                    if consumed.done {
                        self.state = RequestState::RequestLine(Vec::new());
                    } else {
                        break;
                    }
                }
                RequestState::Rejected => return Err(()),
            }
        }
        Ok((!released.is_empty()).then_some(released))
    }

    fn is_allowed(&self) -> bool {
        self.upstream_allowed && !matches!(self.state, RequestState::Rejected)
    }

    fn at_request_boundary(&self) -> bool {
        matches!(self.state, RequestState::RequestLine(_))
    }

    fn reject<T>(&mut self) -> Result<T, ()> {
        self.state = RequestState::Rejected;
        Err(())
    }
}

enum RequestFraming {
    Empty,
    Fixed(usize),
    Chunked,
}

fn request_framing(headers: &[u8]) -> Option<RequestFraming> {
    let headers = std::str::from_utf8(headers).ok()?;
    let mut content_length = None;
    let mut chunked = false;
    for line in headers.split("\r\n").filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("content-length") {
            let parsed = value.trim().parse::<usize>().ok()?;
            if content_length
                .replace(parsed)
                .is_some_and(|old| old != parsed)
            {
                return None;
            }
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            if !value
                .split(',')
                .any(|coding| coding.trim().eq_ignore_ascii_case("chunked"))
            {
                return None;
            }
            chunked = true;
        }
    }
    match (content_length, chunked) {
        (Some(_), true) => None,
        (_, true) => Some(RequestFraming::Chunked),
        (Some(0) | None, false) => Some(RequestFraming::Empty),
        (Some(length), false) => Some(RequestFraming::Fixed(length)),
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn queue_pending_upstream(outbound: &mut Vec<u8>, bytes: &[u8]) -> bool {
    let Some(remaining) = MAX_PENDING_UPSTREAM_BYTES.checked_sub(outbound.len()) else {
        return false;
    };
    if bytes.len() > remaining {
        return false;
    }
    outbound.extend_from_slice(bytes);
    true
}

fn queue_swapped_request(
    outbound: &mut Vec<u8>,
    swap: &mut StubSwap,
    gate: &RequestGate,
    allowed: &[u8],
) -> bool {
    if !queue_pending_upstream(outbound, &swap.push(allowed)) {
        return false;
    }
    // A poll/read boundary is not an HTTP boundary: a stub may continue in the
    // next TLS record. Only completed framing makes the retained tail final.
    !gate.at_request_boundary() || queue_pending_upstream(outbound, &swap.flush())
}

fn forward_pending(
    outbound: &mut Vec<u8>,
    mut send: impl FnMut(&[u8]) -> std::io::Result<usize>,
) -> std::io::Result<()> {
    while !outbound.is_empty() {
        match send(outbound) {
            Ok(0) => break,
            Ok(written) => {
                outbound.drain(..written);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

struct ChunkedBody {
    state: ChunkState,
}

enum ChunkState {
    Size(Vec<u8>),
    Data(usize),
    DataCrlf(usize),
    Trailer(Vec<u8>),
}

struct ChunkConsume {
    bytes: usize,
    done: bool,
}

impl ChunkedBody {
    fn new() -> Self {
        Self {
            state: ChunkState::Size(Vec::new()),
        }
    }

    fn consume(&mut self, input: &[u8]) -> Option<ChunkConsume> {
        let mut cursor = 0;
        while cursor < input.len() {
            match &mut self.state {
                ChunkState::Size(line) => {
                    line.push(input[cursor]);
                    cursor += 1;
                    if line.ends_with(b"\r\n") {
                        let size = std::str::from_utf8(&line[..line.len() - 2])
                            .ok()?
                            .split(';')
                            .next()?
                            .trim();
                        let size = usize::from_str_radix(size, 16).ok()?;
                        self.state = if size == 0 {
                            ChunkState::Trailer(Vec::new())
                        } else {
                            ChunkState::Data(size)
                        };
                    } else if line.len() > MAX_REQUEST_LINE_BYTES {
                        return None;
                    }
                }
                ChunkState::Data(remaining) => {
                    let take = (*remaining).min(input.len() - cursor);
                    *remaining -= take;
                    cursor += take;
                    if *remaining == 0 {
                        self.state = ChunkState::DataCrlf(0);
                    }
                }
                ChunkState::DataCrlf(matched) => {
                    if input[cursor] != b"\r\n"[*matched] {
                        return None;
                    }
                    *matched += 1;
                    cursor += 1;
                    if *matched == 2 {
                        self.state = ChunkState::Size(Vec::new());
                    }
                }
                ChunkState::Trailer(line) => {
                    line.push(input[cursor]);
                    cursor += 1;
                    if line.ends_with(b"\r\n") {
                        if line.len() == 2 {
                            return Some(ChunkConsume {
                                bytes: cursor,
                                done: true,
                            });
                        }
                        line.clear();
                    } else if line.len() > MAX_REQUEST_HEADER_BYTES {
                        return None;
                    }
                }
            }
        }
        Some(ChunkConsume {
            bytes: cursor,
            done: false,
        })
    }
}

fn oauth_forbidden_response() -> Vec<u8> {
    const BODY: &str = "pillbox vault: OAuth rotation is broker-owned\n";
    format!(
        "HTTP/1.1 403 Forbidden\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{BODY}",
        BODY.len()
    )
    .into_bytes()
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
        request_gate,
        outbound,
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

    // Drain guest plaintext before opening the upstream. Every request line is held
    // until the provider-owned token-endpoint matcher allows it, which makes a guest
    // refresh a local response rather than even a partial upstream request. Process
    // bounded chunks so a guest cannot create a second unbounded staging buffer in
    // front of the pending-upstream queue.
    if !*closing {
        let mut plain = [0u8; MAX_PLAINTEXT_CHUNK_BYTES];
        'plaintext: loop {
            let n = match tls.reader().read(&mut plain) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            };
            match request_gate.push(host, &plain[..n]) {
                Ok(Some(allowed)) => {
                    if !*req_logged {
                        let head = String::from_utf8_lossy(&allowed);
                        diag.log(&format!(
                            "krun-egress: [mitm] {:?} → {host} (forwarding{})",
                            head.lines().next().unwrap_or(""),
                            if swap.is_noop() { "" } else { ", cred swapped" }
                        ));
                        *req_logged = true;
                    }
                    if !queue_swapped_request(outbound, swap, request_gate, &allowed) {
                        diag.log(&format!(
                            "krun-egress: [mitm] pending request exceeds {MAX_PENDING_UPSTREAM_BYTES} bytes → RST"
                        ));
                        sock.abort();
                        return;
                    }
                }
                Ok(None) => {}
                Err(()) => {
                    diag.log(&format!(
                        "krun-egress: [mitm] DENY guest OAuth rotation → {host} (local 403)"
                    ));
                    let _ = tls.writer().write_all(&oauth_forbidden_response());
                    *closing = true;
                    *connecting = None;
                    *upstream = None;
                    outbound.clear();
                    break 'plaintext;
                }
            }
        }
    }

    // Open the forward connection once the request gate passes — on a background thread so
    // the blocking resolve/connect doesn't stall the poll loop. Spawn once, then
    // poll the receiver each tick until the upstream (validated against the Mozilla
    // roots) is ready.
    if !host.is_empty() && request_gate.is_allowed() && upstream.is_none() && !*closing {
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

    // Relay gated request bytes upstream and provider response bytes back to the
    // guest. OAuth rotation responses cannot exist here because their requests
    // never leave the local gate.
    if let Some(up) = upstream.as_mut() {
        if let Err(error) = forward_pending(outbound, |bytes| up.send(bytes)) {
            diag.log(&format!(
                "krun-egress: [mitm] upstream write failed → RST ({error})"
            ));
            sock.abort();
            return;
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

    #[test]
    fn request_gate_rejects_anthropic_and_codex_token_endpoints_across_chunks() {
        for (host, chunks) in [
            (
                "platform.claude.com",
                [b"POST /v1/oauth/".as_slice(), b"token HTTP/1.1\r\n"],
            ),
            (
                "auth.openai.com",
                [b"POST /oauth/to".as_slice(), b"ken?x=1 HTTP/1.1\r\n"],
            ),
        ] {
            let mut gate = RequestGate::new();
            assert!(matches!(gate.push(host, chunks[0]), Ok(None)));
            assert!(gate.push(host, chunks[1]).is_err());
            assert!(!gate.is_allowed());
        }
    }

    #[test]
    fn request_gate_releases_non_rotation_request_only_after_complete_line() {
        let mut gate = RequestGate::new();
        assert!(matches!(
            gate.push("platform.claude.com", b"POST /v1/messages HT"),
            Ok(None)
        ));
        let released = gate
            .push(
                "platform.claude.com",
                b"TP/1.1\r\nauthorization: Bearer stub\r\n\r\n",
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            released,
            b"POST /v1/messages HTTP/1.1\r\nauthorization: Bearer stub\r\n\r\n"
        );
        assert!(gate.is_allowed());
    }

    #[test]
    fn request_gate_rejects_rotation_after_allowed_keep_alive_request() {
        let mut gate = RequestGate::new();
        let first = b"POST /v1/messages HTTP/1.1\r\ncontent-length: 2\r\n\r\n{}";
        assert_eq!(
            gate.push("platform.claude.com", first).unwrap().unwrap(),
            first
        );

        assert!(gate
            .push(
                "platform.claude.com",
                b"POST /v1/oauth/token HTTP/1.1\r\ncontent-length: 2\r\n\r\n{}",
            )
            .is_err());
        assert!(!gate.is_allowed());
    }

    #[test]
    fn request_gate_tracks_chunked_body_before_classifying_next_request() {
        let mut gate = RequestGate::new();
        let first =
            b"POST /v1/messages HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n";
        assert_eq!(
            gate.push("platform.claude.com", first).unwrap().unwrap(),
            first
        );
        assert!(gate
            .push(
                "platform.claude.com",
                b"POST /v1/oauth/token HTTP/1.1\r\n\r\n",
            )
            .is_err());
    }

    fn push_pending(gate: &mut RequestGate, outbound: &mut Vec<u8>, chunk: &[u8]) -> bool {
        match gate.push("platform.claude.com", chunk) {
            Ok(Some(allowed)) => queue_pending_upstream(outbound, &allowed),
            Ok(None) => true,
            Err(()) => false,
        }
    }

    #[test]
    fn pending_upstream_queue_rejects_only_after_exact_boundary() {
        let mut outbound = Vec::new();
        assert!(queue_pending_upstream(
            &mut outbound,
            &vec![b'x'; MAX_PENDING_UPSTREAM_BYTES]
        ));
        assert_eq!(outbound.len(), MAX_PENDING_UPSTREAM_BYTES);
        assert!(!queue_pending_upstream(&mut outbound, b"x"));
        assert_eq!(outbound.len(), MAX_PENDING_UPSTREAM_BYTES);
    }

    #[test]
    fn fixed_length_pending_request_fails_closed_at_queue_bound() {
        let mut gate = RequestGate::new();
        let mut outbound = Vec::new();
        let head = b"POST /v1/messages HTTP/1.1\r\ncontent-length: 400000\r\n\r\n";
        assert!(push_pending(&mut gate, &mut outbound, head));

        let body = vec![b'x'; MAX_PENDING_UPSTREAM_BYTES + 1];
        let overflowed = body
            .chunks(MAX_PLAINTEXT_CHUNK_BYTES)
            .any(|chunk| !push_pending(&mut gate, &mut outbound, chunk));
        assert!(overflowed);
        assert!(outbound.len() <= MAX_PENDING_UPSTREAM_BYTES);
    }

    #[test]
    fn chunked_pending_request_fails_closed_at_queue_bound() {
        let mut gate = RequestGate::new();
        let mut outbound = Vec::new();
        let head = b"POST /v1/messages HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\n";
        assert!(push_pending(&mut gate, &mut outbound, head));

        let mut wire_chunk = Vec::with_capacity(4096 + 10);
        wire_chunk.extend_from_slice(b"1000\r\n");
        wire_chunk.extend(std::iter::repeat_n(b'x', 4096));
        wire_chunk.extend_from_slice(b"\r\n");
        let overflowed = (0..128).any(|_| !push_pending(&mut gate, &mut outbound, &wire_chunk));
        assert!(overflowed);
        assert!(outbound.len() <= MAX_PENDING_UPSTREAM_BYTES);
    }

    #[test]
    fn pending_queue_bound_is_per_forward_batch() {
        let mut outbound = Vec::new();
        assert!(queue_pending_upstream(
            &mut outbound,
            &vec![b'x'; MAX_PENDING_UPSTREAM_BYTES]
        ));
        forward_pending(&mut outbound, |bytes| Ok(bytes.len())).unwrap();
        assert!(outbound.is_empty());
        assert!(queue_pending_upstream(&mut outbound, b"next request"));
    }

    #[test]
    fn partial_upstream_writes_retain_the_exact_suffix_until_accepted() {
        let original = b"exact request bytes";
        let mut outbound = original.to_vec();
        let mut received = Vec::new();
        forward_pending(&mut outbound, |bytes| {
            if received.len() >= 5 {
                return Err(std::io::ErrorKind::WouldBlock.into());
            }
            received.extend_from_slice(&bytes[..5]);
            Ok(5)
        })
        .unwrap();
        assert_eq!(outbound, original[5..]);
        forward_pending(&mut outbound, |_| Ok(0)).unwrap();
        assert_eq!(outbound, original[5..]);
        let error = forward_pending(
            &mut outbound,
            |_| Err(std::io::ErrorKind::BrokenPipe.into()),
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        assert_eq!(outbound, original[5..]);
        forward_pending(&mut outbound, |bytes| {
            received.extend_from_slice(bytes);
            Ok(bytes.len())
        })
        .unwrap();
        assert!(outbound.is_empty());
        assert_eq!(received, original);
    }

    #[test]
    fn bounded_plaintext_chunks_preserve_split_credential_substitution() {
        let mut swap = StubSwap::new(vec![CredSwap {
            stub: b"stub-token".to_vec(),
            real: b"real-token".to_vec(),
            hosts: vec!["api.anthropic.com".to_string()],
        }]);
        let mut input = vec![b'x'; MAX_PLAINTEXT_CHUNK_BYTES - 4];
        input.extend_from_slice(b"stub-token\r\n");
        let mut outbound = Vec::new();
        for chunk in input.chunks(MAX_PLAINTEXT_CHUNK_BYTES) {
            assert!(queue_pending_upstream(&mut outbound, &swap.push(chunk)));
        }
        assert!(queue_pending_upstream(&mut outbound, &swap.flush()));
        let mut expected = vec![b'x'; MAX_PLAINTEXT_CHUNK_BYTES - 4];
        expected.extend_from_slice(b"real-token\r\n");
        assert_eq!(outbound, expected);
    }

    #[test]
    fn separate_poll_cycles_preserve_stub_carry_until_request_end() {
        for (first, second, expected) in [
            (
                "POST /messages HTTP/1.1\r\nContent-Length: 10\r\n\r\nstub-",
                "token",
                "POST /messages HTTP/1.1\r\nContent-Length: 10\r\n\r\nreal-token",
            ),
            (
                "POST /messages HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\na\r\nstub-",
                "token\r\n0\r\n\r\n",
                "POST /messages HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\na\r\nreal-token\r\n0\r\n\r\n",
            ),
        ] {
            let mut gate = RequestGate::new();
            let mut swap = StubSwap::new(vec![CredSwap {
                stub: b"stub-token".to_vec(),
                real: b"real-token".to_vec(),
                hosts: vec!["api.anthropic.com".to_string()],
            }]);
            let mut outbound = Vec::new();
            let mut received = Vec::new();
            for fragment in [first, second] {
                let allowed = gate.push("api.anthropic.com", fragment.as_bytes()).unwrap().unwrap();
                assert!(queue_swapped_request(&mut outbound, &mut swap, &gate, &allowed));
                // Simulate an upstream drain between polls, not just two chunks
                // accumulated before a single final flush.
                forward_pending(&mut outbound, |bytes| {
                    received.extend_from_slice(bytes);
                    Ok(bytes.len())
                }).unwrap();
            }
            assert!(gate.at_request_boundary());
            assert_eq!(received, expected.as_bytes());
            assert!(swap.flush().is_empty());
        }
    }
}
