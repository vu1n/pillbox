//! The `EventLog` write-side seam — one trait, two placements.
//!
//! A §0 producer (the agent-output stream, `session send`, the lifecycle
//! wrapper) appends [`Event`]s and lets the **seq authority** stamp each
//! `seq`. WHERE that authority lives is the only thing that changes between a
//! local run and a managed one:
//!
//!   - [`SessionLog`] (this crate's `events/log.rs`) is the **co-located
//!     single-writer** placement — it holds the authority under a file lock and
//!     appends to `log.jsonl`. It is the "JsonlSessionLog" of the design doc.
//!   - [`ManagedDoSink`] is the **resident-sequencer** placement — it POSTs each
//!     event to a per-session Cloudflare Durable Object (the §0 gateway in
//!     `cloudflare-spike/`), which holds the authority and stamps `seq`
//!     server-side, returning it.
//!
//! Both honor the same invariant: the producer submits `seq == 0` and the
//! authority assigns it (the log is the authority, never the producer). A
//! producer that writes through `&mut dyn EventLog` swaps local ⇄ managed with
//! one constructor call ([`open_event_log`]) — same `Event`, same builder.
//! Readers (`subscribe`/`watch`/`wait-idle`) gain a DO-WS source alongside the
//! local file source separately; this trait is the write side only.
//!
//! Full design: docs/managed-tier.md (§Consume path) + docs/session-event-log.md
//! (§Sequencing — local single-writer vs resident sequencer).

use anyhow::{Context, Result};
use serde::Deserialize;

use super::log::SessionLog;
use crate::contract::Event;
use crate::pillbox::Pillbox;

/// The write side of the §0 event log: append durable events, letting the seq
/// **authority** assign each `seq` (producers submit `seq == 0`). Returns the
/// last `seq` assigned, or the prior high-water mark for an empty batch. The
/// impls differ only in where the authority lives.
pub(crate) trait EventLog {
    fn append(&mut self, events: &[Event]) -> Result<u64>;
}

/// Local placement: the file-backed single-writer log is itself the seq
/// authority (it stamps under an exclusive `flock`).
impl EventLog for SessionLog {
    fn append(&mut self, events: &[Event]) -> Result<u64> {
        // Disambiguate from this trait method: call the inherent `SessionLog`
        // append, which holds the lock and stamps `seq`.
        SessionLog::append(self, events)
    }
}

/// Resident-sequencer placement: POST each event to the per-session §0 gateway
/// Durable Object, which stamps `seq` and returns it. The DO derives the actor
/// from `token` (the body's `actor`/`seq` are ignored), so a producer here
/// pushes only agent-output payloads — control payloads (`input`/`annotation`/
/// `driver_changed`/`scored`) have their own authenticated routes and are
/// rejected on `/event` with 403.
pub(crate) struct ManagedDoSink {
    /// The per-session base URL, e.g.
    /// `https://…workers.dev/agents/session-gateway/<sessionId>` (no trailing
    /// slash). `/event` is appended per append.
    endpoint: String,
    /// Actor bearer token the DO verifies + stamps. Empty → the DO fails closed
    /// (401), surfaced as an error on the first append.
    token: String,
    client: reqwest::blocking::Client,
    /// Last `seq` the DO assigned, so an empty batch can return it (parity with
    /// `SessionLog::last_seq`).
    last_seq: u64,
}

impl ManagedDoSink {
    pub(crate) fn new(endpoint: impl Into<String>, token: impl Into<String>) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(super::EVENTS_SINK_TIMEOUT)
            .build()
            .context("build managed §0 gateway http client")?;
        Ok(Self {
            endpoint: endpoint.into().trim_end_matches('/').to_string(),
            token: token.into(),
            client,
            last_seq: 0,
        })
    }
}

/// The DO's `/event` ack — `{seq, head}`. We only need the assigned `seq`;
/// `head` (the current high-water mark) is accepted and ignored.
#[derive(Deserialize)]
struct SeqAck {
    seq: u64,
}

impl EventLog for ManagedDoSink {
    fn append(&mut self, events: &[Event]) -> Result<u64> {
        let url = format!("{}/event", self.endpoint);
        for ev in events {
            // The producer never self-assigns seq: send `seq == 0` and let the
            // DO stamp the authority's value (matching `SessionLog::append`,
            // which overwrites the producer's seq). Serializing the whole
            // `Event` matches the gateway's `contract.ts` shape (camelCase).
            let body = serde_json::to_string(&Event {
                seq: 0,
                ..ev.clone()
            })
            .context("serialize managed §0 event")?;
            let resp = self
                .client
                .post(&url)
                .header("content-type", "application/json")
                .bearer_auth(&self.token)
                .body(body)
                .send()
                .with_context(|| format!("POST {url}"))?;
            if !resp.status().is_success() {
                return Err(anyhow::anyhow!(
                    "managed §0 gateway {url} returned HTTP {}",
                    resp.status()
                ));
            }
            // `reqwest` is built without the `json` feature; parse the body text.
            let text = resp
                .text()
                .with_context(|| format!("read ack from {url}"))?;
            let ack: SeqAck = serde_json::from_str(&text)
                .with_context(|| format!("parse {{seq}} ack from {url}: {text}"))?;
            self.last_seq = ack.seq;
        }
        Ok(self.last_seq)
    }
}

/// Open the §0 sink for a producer — the one swap point the managed tier turns
/// on. With `PILLBOX_MANAGED_DO_URL` set (and `PILLBOX_ACTOR_TOKEN` for the
/// actor credential), events go to the per-session Durable Object; otherwise to
/// the local file-backed [`SessionLog`]. The caller writes through the returned
/// `dyn EventLog` and never sees which placement it got. `+ Send` so a producer
/// can move the sink into its background tailer thread; both placements are `Send`.
pub(crate) fn open_event_log(pb: &Pillbox, session_id: &str) -> Result<Box<dyn EventLog + Send>> {
    if let Ok(base) = std::env::var("PILLBOX_MANAGED_DO_URL") {
        let token = std::env::var("PILLBOX_ACTOR_TOKEN").unwrap_or_default();
        let endpoint = format!(
            "{}/agents/session-gateway/{session_id}",
            base.trim_end_matches('/')
        );
        return Ok(Box::new(ManagedDoSink::new(endpoint, token)?));
    }
    Ok(Box::new(SessionLog::open(pb, session_id)?))
}

/// Best-effort [`open_event_log`] for a run producer: a sink-open failure warns
/// and yields `None` (the run continues OTLP-only) instead of aborting the run.
/// The single home for that fallback policy, shared by the backend producers.
pub(crate) fn open_or_warn(pb: &Pillbox, session_id: &str) -> Option<Box<dyn EventLog + Send>> {
    match open_event_log(pb, session_id) {
        Ok(log) => Some(log),
        Err(e) => {
            eprintln!("pillbox: warning: couldn't open session log: {e:#}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{Payload, ToolCall, ToolStatus};
    use crate::test_util::with_isolated_home;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

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

    #[test]
    fn session_log_is_an_event_log() {
        // The local placement satisfies the trait: appending through the trait
        // object stamps the same monotonic seq the inherent method would.
        with_isolated_home("sink-local", || {
            let pb = crate::pillbox::global();
            let mut log: Box<dyn EventLog> = Box::new(SessionLog::open(&pb, "sess-sink").unwrap());
            let last = log
                .append(&[
                    Event::session("sess-sink", tool_call("a")),
                    Event::session("sess-sink", tool_call("b")),
                ])
                .unwrap();
            assert_eq!(last, 2, "trait append stamps via the file authority");
        });
    }

    /// Spawn a one-shot loopback HTTP server that returns `body` (a JSON object)
    /// for the first request, capturing the raw request for assertions.
    fn one_shot_server(
        status_line: &'static str,
        body: &'static str,
    ) -> (String, Arc<Mutex<Option<String>>>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        let received: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let recv = Arc::clone(&received);
        let handle = thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            sock.set_read_timeout(Some(Duration::from_secs(2))).ok();
            let mut buf = [0u8; 8192];
            let n = sock.read(&mut buf).unwrap_or(0);
            *recv.lock().unwrap() = Some(String::from_utf8_lossy(&buf[..n]).to_string());
            let resp = format!(
                "{status_line}\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes());
        });
        (url, received, handle)
    }

    #[test]
    fn managed_sink_posts_event_with_token_and_returns_assigned_seq() {
        // The DO assigns seq 7; the sink must return it (not the producer's 0),
        // and the request must carry the bearer token + a camelCase Event with
        // seq:0 to /event.
        let (base, received, server) = one_shot_server("HTTP/1.1 200 OK", r#"{"seq":7,"head":7}"#);
        let endpoint = format!("{base}/agents/session-gateway/sess-1");
        let mut sink = ManagedDoSink::new(endpoint, "tok-abc").unwrap();
        let seq = sink
            .append(&[Event::session("sess-1", tool_call("x"))])
            .unwrap();
        server.join().expect("server thread");
        assert_eq!(
            seq, 7,
            "sink returns the DO-assigned seq, not the producer's"
        );

        let req = received.lock().unwrap().take().expect("got request");
        assert!(
            req.starts_with("POST /agents/session-gateway/sess-1/event"),
            "got: {req}"
        );
        assert!(
            req.contains("authorization: Bearer tok-abc")
                || req.contains("Authorization: Bearer tok-abc"),
            "missing bearer token in: {req}"
        );
        assert!(
            req.contains("\"seq\":0"),
            "producer must submit seq=0: {req}"
        );
        assert!(req.contains("toolCall"), "camelCase payload missing: {req}");
    }

    #[test]
    fn managed_sink_surfaces_non_2xx() {
        // A control payload type is rejected by the gateway with 403; the sink
        // surfaces it as an error rather than swallowing it.
        let (base, _received, server) =
            one_shot_server("HTTP/1.1 403 Forbidden", r#"{"error":"not allowed"}"#);
        let endpoint = format!("{base}/agents/session-gateway/sess-2");
        let mut sink = ManagedDoSink::new(endpoint, "tok").unwrap();
        let err = sink
            .append(&[Event::session("sess-2", tool_call("y"))])
            .unwrap_err();
        server.join().expect("server thread");
        assert!(format!("{err:#}").contains("403"), "expected 403 in error");
    }

    #[test]
    fn open_event_log_defaults_to_the_local_file_placement() {
        // With no managed env, the swap point returns the local file-backed
        // placement: appended events persist to the on-disk session log (a fresh
        // handle sees them) and never leave the host. This is the path every
        // `pillbox run` takes by default — guard against the env-gate inverting.
        with_isolated_home("sink-open-local", || {
            std::env::remove_var("PILLBOX_MANAGED_DO_URL");
            let pb = crate::pillbox::global();
            let mut sink = open_event_log(&pb, "sess-default").unwrap();
            let last = sink
                .append(&[
                    Event::session("sess-default", tool_call("a")),
                    Event::session("sess-default", tool_call("b")),
                ])
                .unwrap();
            assert_eq!(last, 2, "local placement stamps via the file authority");
            assert_eq!(
                SessionLog::open(&pb, "sess-default").unwrap().last_seq(),
                2,
                "events persisted to the on-disk log, not a remote sink"
            );
        });
    }

    /// Removes the named env vars on drop. The managed-routing test sets a var
    /// (`PILLBOX_MANAGED_DO_URL`) that changes `open_event_log`'s placement for
    /// ALL subsequent code; an RAII guard removes it even if the body panics
    /// (`with_isolated_home` holds the test HOME lock but does not `catch_unwind`,
    /// so a bare `remove_var` at the end would be skipped on panic and leak the
    /// routing into the next test).
    struct EnvGuard(&'static [&'static str]);
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for key in self.0 {
                std::env::remove_var(key);
            }
        }
    }

    #[test]
    fn open_event_log_routes_to_managed_do_when_env_set() {
        // PILLBOX_MANAGED_DO_URL flips the swap point to the managed Durable
        // Object: open_event_log builds the per-session URL and POSTs there with
        // the actor token. Held under the test HOME lock (via with_isolated_home)
        // so the env mutation serializes against other env-touching tests; the
        // EnvGuard clears the vars on scope exit (incl. panic) so routing can't leak.
        with_isolated_home("sink-open-managed", || {
            let _env = EnvGuard(&["PILLBOX_MANAGED_DO_URL", "PILLBOX_ACTOR_TOKEN"]);
            let (base, received, server) =
                one_shot_server("HTTP/1.1 200 OK", r#"{"seq":3,"head":3}"#);
            std::env::set_var("PILLBOX_MANAGED_DO_URL", &base);
            std::env::set_var("PILLBOX_ACTOR_TOKEN", "tok-open");

            let pb = crate::pillbox::global();
            let seq = open_event_log(&pb, "sess-open")
                .and_then(|mut sink| sink.append(&[Event::session("sess-open", tool_call("z"))]))
                .expect("append via managed sink");

            server.join().expect("server thread");
            assert_eq!(seq, 3);
            let req = received.lock().unwrap().take().expect("got request");
            assert!(
                req.starts_with("POST /agents/session-gateway/sess-open/event"),
                "open_event_log must build the per-session DO URL: {req}"
            );
            assert!(
                req.contains("Bearer tok-open"),
                "actor token must ride the request: {req}"
            );
        });
    }
}
