//! Webhook sink — POSTs each event's JSON payload to whatever URL
//! the user set via `--events-webhook` / `$PILLBOX_EVENTS_WEBHOOK`.
//! Primary use case is letting the sandbox-side pillbox ferry
//! terminal events back to the orchestrator without pillbox running
//! a daemon. Sync emit via `reqwest::blocking` matches the call-site
//! shape; a 2s timeout per request keeps a stuck endpoint from
//! dominating session runtime.

use std::sync::OnceLock;

use anyhow::{Context, Result};

use super::EVENTS_SINK_TIMEOUT;

/// Shared blocking HTTP client. Built once on first use and reused
/// for every subsequent emit so a session's 2-4 terminal events
/// don't pay the TLS-context setup cost on each call.
/// `reqwest::blocking::Client` is `Send + Sync` and internally pools
/// connections, which is the whole point of caching it.
static WEBHOOK_CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();

/// POST one event line to the configured webhook URL. Body is the
/// JSON payload (without trailing newline) produced by the JSONL
/// sink's renderer. A short request timeout keeps a slow webhook
/// from blocking the run.
pub(super) fn sink_emit(url: &str, payload: &str) -> Result<()> {
    // First-call build is the only path that can fail (e.g. native TLS
    // backend missing). Subsequent calls reuse the cached client, so the
    // `?` here only short-circuits the first attempt per process.
    let client = client()?;
    let resp = client
        .post(url)
        .header("content-type", "application/json")
        .body(payload.to_string())
        .send()
        .with_context(|| format!("POST {url}"))?;
    if !resp.status().is_success() {
        return Err(anyhow::anyhow!(
            "webhook {url} returned HTTP {}",
            resp.status()
        ));
    }
    Ok(())
}

/// Lazy-init the shared client. `get_or_try_init` would let us bubble the
/// `build` error through `OnceLock`, but it's still nightly-only on
/// stable Rust; fall back to building, caching on success, and surfacing
/// the error directly otherwise. Two threads racing here both build a
/// client; whichever one calls `set` first wins — the loser's client is
/// dropped harmlessly. Worth the simpler code given how rare a build
/// failure is.
fn client() -> Result<&'static reqwest::blocking::Client> {
    if let Some(c) = WEBHOOK_CLIENT.get() {
        return Ok(c);
    }
    let built = reqwest::blocking::Client::builder()
        .timeout(EVENTS_SINK_TIMEOUT)
        .build()
        .context("build webhook http client")?;
    let _ = WEBHOOK_CLIENT.set(built);
    Ok(WEBHOOK_CLIENT.get().expect("set or already-set"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{Arc, Mutex},
        thread,
        time::Duration,
    };

    #[test]
    fn webhook_sink_posts_json_body() {
        // Bind a real loopback TCP listener and verify the sink POSTs
        // a well-formed HTTP request with the JSON payload as the
        // body. Avoids env-var coupling (which would force serial
        // execution) by calling the sink function directly. The HTTP
        // server is the dumbest possible single-request handler —
        // enough to verify shape, no need for hyper/reqwest mocks.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/events");

        let received: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let recv_clone = Arc::clone(&received);
        let server = thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            sock.set_read_timeout(Some(Duration::from_secs(2))).ok();
            let mut buf = [0u8; 4096];
            // Read once — the test payload fits in one packet and we
            // only need to verify the request shape, not handle pipelining.
            let n = sock.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            *recv_clone.lock().unwrap() = Some(request);
        });

        let payload = r#"{"event":"session.completed","session_id":"abc"}"#;
        sink_emit(&url, payload).expect("emit");

        server.join().expect("server thread");
        let req = received.lock().unwrap().take().expect("got request");
        assert!(req.starts_with("POST /events"), "got: {req}");
        assert!(
            req.to_lowercase()
                .contains("content-type: application/json"),
            "got: {req}"
        );
        assert!(req.contains(payload), "body missing in: {req}");
    }

    #[test]
    fn webhook_sink_surfaces_non_2xx() {
        // Server returns 500; sink should return Err with the status.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/events");
        let server = thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            sock.set_read_timeout(Some(Duration::from_secs(2))).ok();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf);
            let _ =
                sock.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n");
        });
        let err = sink_emit(&url, "{}").unwrap_err();
        server.join().expect("server thread");
        let msg = format!("{err:#}");
        assert!(msg.contains("500"), "expected 500 in: {msg}");
    }
}
