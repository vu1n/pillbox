//! The `EventSource` read-side seam — the read counterpart to [`super::sink`].
//!
//! Where the write side ([`super::sink::EventLog`]) lets a producer append §0
//! events to either the local file-backed log or the managed Durable Object, the
//! read side lets a consumer *subscribe* to the same two placements through one
//! trait. A reader (`session subscribe`'s WS gateway, `session watch`) calls
//! [`open_event_source`] and streams events without knowing where they come from:
//!
//!   - [`SessionLog`] is the **local file** placement — [`SessionLog::subscribe`]
//!     replays `seq >= from` then notify-tails `log.jsonl`.
//!   - [`ManagedDoSource`] is the **resident-sequencer** placement — it opens a
//!     WebSocket to the per-session Durable Object's subscribe endpoint
//!     (`wss://…/agents/session-gateway/<id>?from=<seq>`), which replays then
//!     live-fans-out one JSON [`Event`] per text frame in seq order (1:1 with
//!     `SessionLog::subscribe`; see `cloudflare-spike/src/session_gateway.ts`).
//!
//! Both honor the same contract as [`SessionLog::subscribe`]: stream events with
//! `seq >= from` to `sink`, then keep tailing, returning when `stop` is set or
//! `sink` returns `false`. The trait is object-safe (the sink is `&mut dyn
//! FnMut`, not `impl FnMut`) so a consumer can hold `Box<dyn EventSource + Send>`
//! and move it into its per-connection thread.
//!
//! Full design: docs/managed-tier.md (§Consume path). The write side is
//! [`super::sink`]; this is the read side only.

use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{client_tls_with_config, Connector, Message};

use super::log::SessionLog;
use crate::contract::Event;
use crate::pillbox::Pillbox;

/// Read timeout on the DO WebSocket stream so a parked `read()` returns
/// periodically and `stop` is honored between frames (the managed mirror of
/// `SessionLog::subscribe`'s `SUBSCRIBE_POLL` — `notify` does the real relaying
/// locally, the DO's fan-out does it remotely; this just bounds stop latency).
const WS_READ_POLL: Duration = Duration::from_millis(500);

/// Bound the connect + TLS/WS handshake. Without it, a DO that accepts the TCP
/// connection but stalls the upgrade would park the subscriber thread (or
/// `session watch`) forever — *before* the stop-aware read loop begins, so
/// `stop` couldn't unblock it. Reset to [`WS_READ_POLL`] once connected.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The read side of the §0 event log: stream events with `seq >= from` to
/// `sink`, then keep tailing, returning when `stop` is set or `sink` returns
/// `false`. The impls differ only in where the events come from. Object-safe
/// (the sink is `&mut dyn FnMut`) so the placement can be a trait object.
pub(crate) trait EventSource {
    fn subscribe(
        &self,
        from: u64,
        stop: &AtomicBool,
        sink: &mut dyn FnMut(&Event) -> bool,
    ) -> Result<()>;
}

/// Local placement: forward to the inherent file-backed
/// [`SessionLog::subscribe`] (`&mut dyn FnMut` satisfies its `impl FnMut`).
impl EventSource for SessionLog {
    fn subscribe(
        &self,
        from: u64,
        stop: &AtomicBool,
        sink: &mut dyn FnMut(&Event) -> bool,
    ) -> Result<()> {
        SessionLog::subscribe(self, from, stop, sink)
    }
}

/// Resident-sequencer placement: subscribe over a WebSocket to the per-session
/// §0 gateway Durable Object, which replays `seq >= from` then live-fans-out
/// each new append — one JSON [`Event`] per text frame in seq order.
pub(crate) struct ManagedDoSource {
    /// The per-session subscribe URL with a `ws://`/`wss://` scheme, no query —
    /// `?from=` (and `&token=`) are appended per subscribe.
    ws_url: String,
    /// Optional actor token, sent as `?token=` (the DO derives the connection's
    /// actor from it; reads are allowed anonymously, so `None` is fine). The WS
    /// handshake can't carry an `Authorization` header cross-origin, so the
    /// credential rides the query — matching the DO's `onConnect`.
    token: Option<String>,
}

impl ManagedDoSource {
    /// `endpoint` is the `http(s)://…/agents/session-gateway/<id>` base (as built
    /// by [`open_event_source`], 1:1 with `sink::open_event_log`); the scheme is
    /// rewritten to `ws`/`wss` for the upgrade.
    pub(crate) fn new(endpoint: &str, token: Option<String>) -> Self {
        let ws_url = if let Some(rest) = endpoint.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = endpoint.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            endpoint.to_string()
        };
        Self { ws_url, token }
    }

    fn full_url(&self, from: u64) -> String {
        let mut url = format!("{}?from={from}", self.ws_url);
        if let Some(token) = &self.token {
            // Tokens are URL-safe by construction (the actor credential is
            // base64url/hex); passed verbatim, matching the DO's `searchParams`.
            url.push_str("&token=");
            url.push_str(token);
        }
        url
    }

    /// Open the WS connection for `from`, using our explicit aws_lc_rs rustls
    /// connector for `wss` (the tree installs no process-default crypto
    /// provider) and a plain connector for `ws`.
    fn connect(&self, from: u64) -> Result<tungstenite::WebSocket<MaybeTlsStream<TcpStream>>> {
        // Parse host/port from the token-free base, not full_url: a malformed
        // endpoint must not leak the actor token through parse_ws_authority's
        // error strings (which echo the url). The query never affects authority.
        let (is_tls, host, port) = parse_ws_authority(&self.ws_url)?;
        let url = self.full_url(from);
        let addr = (host.as_str(), port)
            .to_socket_addrs()
            .with_context(|| format!("resolve {host}:{port}"))?
            .next()
            .ok_or_else(|| anyhow::anyhow!("no address for {host}:{port}"))?;
        let tcp = TcpStream::connect_timeout(&addr, HANDSHAKE_TIMEOUT)
            .with_context(|| format!("connect managed §0 gateway {addr}"))?;
        // Deadline the handshake reads/writes too (reset to WS_READ_POLL in
        // `subscribe` once connected) — see HANDSHAKE_TIMEOUT.
        tcp.set_read_timeout(Some(HANDSHAKE_TIMEOUT)).ok();
        tcp.set_write_timeout(Some(HANDSHAKE_TIMEOUT)).ok();
        let connector = if is_tls {
            Connector::Rustls(Arc::new(tls_client_config()?))
        } else {
            Connector::Plain
        };
        let (ws, _resp) = client_tls_with_config(url, tcp, None, Some(connector))
            .map_err(|e| anyhow::anyhow!("managed §0 gateway ws handshake: {e}"))?;
        Ok(ws)
    }
}

impl EventSource for ManagedDoSource {
    fn subscribe(
        &self,
        from: u64,
        stop: &AtomicBool,
        sink: &mut dyn FnMut(&Event) -> bool,
    ) -> Result<()> {
        let mut ws = self.connect(from)?;
        // A read timeout makes `read()` return `WouldBlock` periodically so we
        // re-check `stop` while the stream is quiet (the DO holds the socket open
        // to live-tail). Without it a parked read would never observe `stop`, so
        // a missing TcpStream is a hard error, not a silent best-effort skip.
        let tcp = tcp_of(ws.get_ref())
            .context("managed §0 WS stream exposes no TcpStream to deadline")?;
        tcp.set_read_timeout(Some(WS_READ_POLL))
            .context("set managed §0 read timeout")?;
        loop {
            if stop.load(Ordering::Relaxed) {
                let _ = ws.close(None);
                return Ok(());
            }
            match ws.read() {
                Ok(Message::Text(text)) => {
                    let ev: Event = serde_json::from_str(text.as_str())
                        .context("parse §0 event from managed gateway")?;
                    if !sink(&ev) {
                        let _ = ws.close(None);
                        return Ok(());
                    }
                }
                // The server closed the stream — replay/tail is done. No
                // auto-reconnect: a DO hibernation/eviction ends the
                // subscription here (the consumer would re-`subscribe` from its
                // last seq); reconnect-from-cursor is a follow-up. The local
                // SessionLog source has no such close (it tails the file forever).
                Ok(Message::Close(_)) => return Ok(()),
                // Control/other frames carry no §0 event; tungstenite answers
                // Ping with Pong internally, so we just keep reading.
                Ok(_) => {}
                // Read timeout: loop and re-check `stop` (the quiet-tail case).
                Err(tungstenite::Error::Io(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                    return Ok(())
                }
                Err(e) => return Err(e).context("read from managed §0 gateway"),
            }
        }
    }
}

/// The underlying `TcpStream` of a WS stream, for setting the read timeout.
/// `MaybeTlsStream` is `#[non_exhaustive]`; we only build `Plain`/`Rustls`, so
/// the catch-all is unreachable today. If a future variant appeared, returning
/// `None` makes the caller fail loud — proceeding without a timeout would let a
/// quiet live-tail park forever, never observing `stop`.
fn tcp_of(stream: &MaybeTlsStream<TcpStream>) -> Option<&TcpStream> {
    match stream {
        MaybeTlsStream::Plain(tcp) => Some(tcp),
        MaybeTlsStream::Rustls(tls) => Some(tls.get_ref()),
        _ => None,
    }
}

/// Split a `ws://`/`wss://` URL into (is_tls, host, port). Minimal parse (no
/// `url` crate): authority is everything between `//` and the first `/`, with an
/// optional `:port`. Hostnames only (the DO is a workers.dev name) — IPv6
/// literals aren't expected here.
fn parse_ws_authority(url: &str) -> Result<(bool, String, u16)> {
    let (is_tls, rest) = if let Some(r) = url.strip_prefix("wss://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("ws://") {
        (false, r)
    } else {
        anyhow::bail!("managed §0 url must be ws:// or wss://: {url}");
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>()
                .with_context(|| format!("parse port in {url}"))?,
        ),
        None => (authority.to_string(), if is_tls { 443 } else { 80 }),
    };
    Ok((is_tls, host, port))
}

/// A rustls client config built on our explicit `aws_lc_rs` provider +
/// webpki roots — the tree installs no process-default crypto provider (the
/// vault passes one explicitly too; see `vault::forward`), so tungstenite's
/// auto-`connect` would panic. We hand the provider in via `Connector::Rustls`.
fn tls_client_config() -> Result<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .context("aws-lc-rs default protocol versions")?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(config)
}

/// Open the §0 read source for a consumer — the read-side mirror of
/// [`super::sink::open_event_log`], gated on the same env. With
/// `PILLBOX_MANAGED_DO_URL` set, reads stream from the per-session Durable
/// Object; otherwise from the local file-backed [`SessionLog`]. `+ Send` so the
/// consumer can move the source into its per-connection thread (the gateway
/// spawns one per subscriber); both placements are `Send`.
pub(crate) fn open_event_source(
    pb: &Pillbox,
    session_id: &str,
) -> Result<Box<dyn EventSource + Send>> {
    if let Some((endpoint, token)) = super::managed_endpoint(session_id) {
        return Ok(Box::new(ManagedDoSource::new(&endpoint, token)));
    }
    Ok(Box::new(SessionLog::open(pb, session_id)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{Payload, ToolCall, ToolStatus};
    use crate::test_util::with_isolated_home;
    use std::net::TcpListener;
    use std::sync::Mutex;
    use std::thread::{self, JoinHandle};

    fn tool_call(name: &str) -> Payload {
        Payload::ToolCall(ToolCall {
            tool_call_id: format!("tc-{name}"),
            name: name.into(),
            status: ToolStatus::Running,
            input: None,
            output: String::new(),
            title: String::new(),
        })
    }

    /// Removes the named env vars on drop, so a panic between set and the
    /// assertions can't leak managed-routing state into other tests (the test
    /// HOME lock serializes us, but `with_isolated_home` does not `catch_unwind`).
    struct EnvGuard(&'static [&'static str]);
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for key in self.0 {
                std::env::remove_var(key);
            }
        }
    }

    /// A one-shot loopback WS server that captures the upgrade request URI, sends
    /// `events` as JSON text frames, then (if `hold_open`) keeps the socket open
    /// to mimic the DO's live tail until the client disconnects. Returns the
    /// `ws://` base URL, the captured-URI handle, and the server thread.
    // The accept_hdr callback's Err type (tungstenite's ErrorResponse) is fixed
    // by the API, so the large-Err lint doesn't apply here.
    #[allow(clippy::result_large_err)]
    fn ws_server(
        events: Vec<Event>,
        hold_open: bool,
    ) -> (String, Arc<Mutex<Option<String>>>, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let url = format!("ws://{addr}/agents/session-gateway/sess-t");
        let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let cap = Arc::clone(&captured);
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let sink = Arc::clone(&cap);
            let mut ws = tungstenite::accept_hdr(
                stream,
                |req: &tungstenite::handshake::server::Request, resp| {
                    *sink.lock().unwrap() = Some(req.uri().to_string());
                    Ok(resp)
                },
            )
            .expect("ws handshake");
            for ev in &events {
                let json = serde_json::to_string(ev).unwrap();
                if ws.send(Message::Text(json.into())).is_err() {
                    return;
                }
            }
            if hold_open {
                // Live-tail mimic: read until the client closes (drives the
                // close handshake), so the test's `stop`/sink-false path drains.
                while ws.read().is_ok() {}
            } else {
                let _ = ws.close(None);
                while ws.read().is_ok() {}
            }
        });
        (url, captured, handle)
    }

    #[test]
    fn managed_source_streams_frames_and_sends_from() {
        // The client passes `?from=` and parses each text frame back into an
        // Event, in order — the replay half of subscribe.
        let evs = vec![
            Event::session("sess-t", tool_call("a")),
            Event::session("sess-t", tool_call("b")),
        ];
        let (url, captured, server) = ws_server(evs, false);
        let source = ManagedDoSource::new(&url, None);
        let mut got: Vec<Event> = Vec::new();
        let stop = AtomicBool::new(false);
        source
            .subscribe(5, &stop, &mut |ev| {
                got.push(ev.clone());
                true
            })
            .unwrap();
        server.join().unwrap();
        assert_eq!(got.len(), 2, "both replayed frames parsed");
        assert!(
            matches!(&got[0].payload, Payload::ToolCall(t) if t.name == "a"),
            "frames preserved in order"
        );
        let uri = captured
            .lock()
            .unwrap()
            .take()
            .expect("captured upgrade uri");
        assert!(uri.contains("from=5"), "client must send ?from=: {uri}");
    }

    #[test]
    fn subscribe_stops_when_sink_returns_false() {
        // `sink` returning false ends the subscription after the current event
        // (the consumer says "seen enough").
        let evs = vec![
            Event::session("sess-t", tool_call("a")),
            Event::session("sess-t", tool_call("b")),
        ];
        let (url, _cap, server) = ws_server(evs, true);
        let source = ManagedDoSource::new(&url, None);
        let mut count = 0;
        let stop = AtomicBool::new(false);
        source
            .subscribe(0, &stop, &mut |_ev| {
                count += 1;
                false // stop after the first
            })
            .unwrap();
        server.join().unwrap();
        assert_eq!(count, 1, "sink-false halts after one event");
    }

    #[test]
    fn subscribe_honors_stop_on_a_quiet_stream() {
        // A connected-but-silent server (no frames): the read timeout lets the
        // loop observe `stop` and return, rather than parking forever.
        let (url, _cap, server) = ws_server(Vec::new(), true);
        let source = ManagedDoSource::new(&url, None);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_setter = Arc::clone(&stop);
        let setter = thread::spawn(move || {
            thread::sleep(Duration::from_millis(150));
            stop_setter.store(true, Ordering::Relaxed);
        });
        source.subscribe(0, &stop, &mut |_ev| true).unwrap();
        setter.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn open_event_source_defaults_to_local_sessionlog() {
        // No managed env: the source reads the local file-backed log. Seed it
        // via the inherent SessionLog::append, then read it back through the
        // default source.
        with_isolated_home("source-open-local", || {
            std::env::remove_var("PILLBOX_MANAGED_DO_URL");
            let pb = crate::pillbox::global();
            let mut log = SessionLog::open(&pb, "sess-local").unwrap();
            log.append(&[
                Event::session("sess-local", tool_call("a")),
                Event::session("sess-local", tool_call("b")),
            ])
            .unwrap();

            let source = open_event_source(&pb, "sess-local").unwrap();
            let mut got = 0;
            let stop = AtomicBool::new(false);
            source
                .subscribe(0, &stop, &mut |_ev| {
                    got += 1;
                    got < 2 // stop once both replayed events are seen
                })
                .unwrap();
            assert_eq!(got, 2, "default source replays the local log");
        });
    }

    #[test]
    fn open_event_source_routes_to_managed_when_env_set() {
        // PILLBOX_MANAGED_DO_URL flips the source to the DO: open_event_source
        // builds the per-session URL and streams frames from it.
        let evs = vec![Event::session("sess-do", tool_call("z"))];
        let (base, captured, server) = ws_server(evs, false);
        with_isolated_home("source-open-managed", || {
            let _env = EnvGuard(&["PILLBOX_MANAGED_DO_URL", "PILLBOX_ACTOR_TOKEN"]);
            // Strip the gateway path the server URL carries; the factory re-adds
            // `/agents/session-gateway/<id>` itself.
            let host_base = base.strip_suffix("/agents/session-gateway/sess-t").unwrap();
            std::env::set_var("PILLBOX_MANAGED_DO_URL", host_base);

            let pb = crate::pillbox::global();
            let source = open_event_source(&pb, "sess-do").unwrap();
            let mut got = 0;
            let stop = AtomicBool::new(false);
            source
                .subscribe(0, &stop, &mut |_ev| {
                    got += 1;
                    true
                })
                .unwrap();
            assert_eq!(got, 1, "managed source streamed the DO frame");
        });
        server.join().unwrap();
        let uri = captured.lock().unwrap().take().expect("captured uri");
        assert!(
            uri.contains("/agents/session-gateway/sess-do?from=0"),
            "factory builds the per-session subscribe URL: {uri}"
        );
    }
}
