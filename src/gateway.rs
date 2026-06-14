//! Ephemeral per-session WebSocket gateway — streams a session's durable event
//! log ([`crate::events::log::SessionLog`]) to WS subscribers as JSON frames.
//!
//! This is the first network adapter over the §0 spine's read side
//! ([`SessionLog::subscribe`]): a chat bridge (Slack/Discord), a DSPy/RLM loop,
//! a browser, or an orchestrator connects and watches the agent's semantic
//! stream live — no shell, no PTY. The wire is one JSON-encoded
//! [`crate::contract::Event`] per text message, in seq order from a caller-
//! chosen point (`from`). The SendInput (drive) half — a WS message → the
//! pty-host's `Frame::Input` — is the next slice; this slice is read-only.
//!
//! Sync + thread-per-connection (`tungstenite`), matching pillbox's model of
//! reaching for tokio only in the vault proxy. No async runtime here.
//!
//! Lifetime: serves until Ctrl-C. An ephemeral exit-when-the-session-ends
//! (watching the log for a terminal event) is a follow-up, as is auth — today
//! it binds localhost only.

use std::net::{TcpListener, TcpStream};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::{Context, Result};
use tungstenite::Message;

use crate::events::source::{open_event_source, EventSource};
use crate::pillbox::Pillbox;

/// Default bind: an ephemeral localhost port (printed on start). Zero-config —
/// the consumer reads the address off stderr; nothing to configure or collide.
const DEFAULT_BIND: &str = "127.0.0.1:0";

/// Bind a WS server and stream `session_id`'s event log (from seq `from`) to
/// every subscriber. Blocks accepting connections until the process is killed.
pub(crate) fn serve_session_ws(
    pb: &Pillbox,
    session_id: &str,
    from: u64,
    bind: Option<&str>,
) -> Result<()> {
    // `session_id` is already resolved (the caller ran `resolve_logged`, which
    // confirmed the log exists), so we open a fresh read view per connection
    // and don't probe up front.
    let addr = bind.unwrap_or(DEFAULT_BIND);
    let listener = TcpListener::bind(addr).with_context(|| format!("bind {addr}"))?;
    let local = listener.local_addr().context("resolve bound address")?;
    eprintln!(
        "pillbox: streaming session {session_id} on ws://{local} (from seq {from}); Ctrl-C to stop"
    );

    // Server-wide shutdown handle. Never set in this slice (Ctrl-C kills the
    // process); it's the seam for the future ephemeral exit-when-the-session-
    // ends, and it lets each subscriber thread stop deterministically.
    let shutdown = Arc::new(AtomicBool::new(false));

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(e) => {
                eprintln!("pillbox: warning: accept failed: {e}");
                continue;
            }
        };
        // A fresh read source per connection — each subscriber tails from its own
        // seq. The placement (local file vs managed DO WebSocket) is chosen by
        // `open_event_source`, so a managed-tier subscriber relays from the DO.
        let source = match open_event_source(pb, session_id) {
            Ok(source) => source,
            Err(e) => {
                eprintln!("pillbox: warning: open session event source failed: {e:#}");
                continue;
            }
        };
        let stop = Arc::clone(&shutdown);
        std::thread::spawn(move || serve_one(stream, source, from, &stop));
    }
    Ok(())
}

/// Handshake one connection, then relay the event stream to it until `stop` is
/// set or the client disconnects (a failed send ends the subscription). One
/// subscriber's slow or dead socket can't affect another — each runs on its own
/// thread + log view.
///
/// Note: a client that disconnects while *caught up* (no new events) isn't
/// reaped until the next event, since disconnect is detected on send. Fine
/// while a live session is producing events; a ping/read-side keepalive is the
/// follow-up for idle-subscriber reaping.
fn serve_one(stream: TcpStream, source: Box<dyn EventSource + Send>, from: u64, stop: &AtomicBool) {
    let mut ws = match tungstenite::accept(stream) {
        Ok(ws) => ws,
        Err(_) => return, // not a WS client / handshake failed — drop it
    };
    let relay = source.subscribe(
        from,
        stop,
        &mut |event| match serde_json::to_string(event) {
            Ok(json) => ws.send(Message::Text(json.into())).is_ok(),
            // An event we somehow can't serialize shouldn't kill the stream; skip it.
            Err(_) => true,
        },
    );
    // A source error (e.g. the managed DO was unreachable) shouldn't be silent —
    // the client already sees the closed socket; log so the operator can tell
    // "upstream failed" from "no events yet".
    if let Err(e) = relay {
        eprintln!("pillbox: warning: subscriber stream ended: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{Event, Payload, ToolCall, ToolStatus};
    use crate::events::log::SessionLog;
    use crate::test_util::with_isolated_home;

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

    fn event_seq(msg: &Message) -> u64 {
        let json = msg.to_text().expect("text frame");
        let ev: Event = serde_json::from_str(json).expect("event json");
        ev.seq
    }

    /// End-to-end over real loopback: a WS client receives the existing log
    /// (replay) and then a live-appended event, each as a JSON Event in seq
    /// order. Proves the gateway is a faithful adapter over `subscribe`.
    #[test]
    fn ws_subscriber_gets_replay_then_live_events() {
        with_isolated_home("gateway-ws", || {
            let pb = crate::pillbox::global();
            let mut log = SessionLog::open(&pb, "sess-ws").unwrap();
            log.append(&[
                Event::session("sess-ws", tool_call("a")),
                Event::session("sess-ws", tool_call("b")),
            ])
            .unwrap();

            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            // Move a read view into the server thread; keep `log` here to append.
            let server_log = SessionLog::open(&pb, "sess-ws").unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let server_stop = Arc::clone(&stop);
            let server = std::thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                serve_one(stream, Box::new(server_log), 0, &server_stop);
            });

            let (mut client, _) = tungstenite::connect(format!("ws://{addr}")).unwrap();
            // Replay of the two pre-existing events, in order.
            assert_eq!(event_seq(&client.read().unwrap()), 1);
            assert_eq!(event_seq(&client.read().unwrap()), 2);

            // A live append reaches the subscriber on the next poll.
            log.append(&[Event::session("sess-ws", tool_call("c"))])
                .unwrap();
            assert_eq!(event_seq(&client.read().unwrap()), 3);

            // Tell the server to stop; subscribe returns within one poll, so the
            // join is deterministic (no reliance on disconnect-via-send-failure).
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            server.join().unwrap();
            client.close(None).ok();
        });
    }
}
