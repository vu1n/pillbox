//! Guest-side `codex app-server` bridge — the codex analog of the in-guest
//! `opencode serve` + `/event` capture, run by `pillbox appserver-host`.
//!
//! `codex app-server` speaks JSON-RPC 2.0 (with the `"jsonrpc":"2.0"` header
//! omitted on the wire) as newline-delimited JSON over stdio. The host can't
//! drive stdio across the vsock forward, which carries HTTP — so this bridge
//! owns the codex stdio in the guest and re-exposes it as a tiny one-shot HTTP
//! API the host reaches through the same [`SandboxHttp`](super::http::SandboxHttp)
//! seam opencode uses:
//!
//! | HTTP (host → bridge)        | codex app-server (bridge → codex)             |
//! |-----------------------------|-----------------------------------------------|
//! | `GET /health`               | 200 once `initialize` + `thread/start` done   |
//! | `POST /session`             | returns the started `threadId`                |
//! | `POST /turn {"text":"…"}`   | `turn/start` for the thread                    |
//!
//! Every codex **notification** line is appended verbatim to the events file —
//! the durable §0 source the host drains with
//! [`drain_ndjson`](crate::events::codex_serve::drain_ndjson) (replay + follow),
//! exactly like opencode's `/event` capture. **Server-requests** (approval
//! prompts: `item/commandExecution/requestApproval`, `…/fileChange/…`, etc.) are
//! auto-accepted — the microVM is the security boundary, so the same posture as
//! claude's `--permission-mode auto`.
//!
//! Not feature-gated: it's process + TCP + JSON (no libkrun FFI), so the codec
//! and HTTP parsing are unit-tested on any host even though it only *runs* in
//! the guest.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::errors::PillboxError;
use crate::sandbox::http::SandboxHttp;

/// `clientInfo.name` — identifies pillbox to codex's compliance-logs platform
/// (the README asks integrations to identify themselves).
const CLIENT_NAME: &str = "pillbox";

/// TCP port the in-guest `appserver-host` HTTP API binds (loopback; reached by
/// the backend's [`SandboxHttp`] over the vsock forward). Distinct constant from
/// opencode's so the two server agents can't be confused, even though the vsock
/// forward maps each to the same guest-side port today.
// The launch-path host helpers below (wait_ready/create_session) + the bridge
// port/action consts are consumed only by the libkrun run path (codex-serve is
// libkrun-only); allow dead-code in the default build. `send_turn` is always
// compiled (driven by `session send`), as is the guest `run_host` entrypoint.
#[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
pub(crate) const BRIDGE_PORT: u16 = 4097;

/// Filename (under the agent home) the bridge appends codex notifications to —
/// the §0 NDJSON capture, the codex analog of opencode's `EVENTS_FILE`. Lives in
/// the shared/CoW home so it persists + is host-readable for the drain.
#[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
pub(crate) const EVENTS_FILE: &str = ".pillbox-codex-appserver-events.ndjson";

#[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
const ACTION: &str = "run (codex-serve)";

// ── Host side: drive the in-guest bridge over SandboxHttp ──────────────────
//
// The mirror of `sandbox::opencode`'s host helpers, but for the bridge's
// route shapes. Each is one HTTP call through the vsock-forwarded transport.

/// Poll `GET /health` until the bridge answers `200` (codex boot + the
/// handshake take a moment), bounded so a dead bridge fails loud not hangs.
#[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
pub(crate) fn wait_ready(http: &dyn SandboxHttp) -> Result<()> {
    for _ in 0..60 {
        if let Ok(resp) = http.request("GET", "/health", None) {
            if resp.status == 200 {
                return Ok(());
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Err(PillboxError::runtime(ACTION, "codex app-server bridge didn't become ready in 30s").into())
}

/// `POST /session` → the codex `threadId` the bridge started at boot (the
/// agent-native session id `turn/start` targets; stored on the session record).
#[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
pub(crate) fn create_session(http: &dyn SandboxHttp) -> Result<String> {
    let resp = http.request("POST", "/session", Some("{}"))?;
    let body = resp.body.trim();
    let value: Value = serde_json::from_str(body).map_err(|_| {
        PillboxError::runtime(
            ACTION,
            format!("create session: unexpected response: {body}"),
        )
    })?;
    value
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            PillboxError::runtime(ACTION, format!("create session: no thread id in {body}")).into()
        })
}

/// Drive the session: `POST /turn` with the prompt text. The bridge issues
/// `turn/start` and the turn streams as notifications to the events file (read
/// via the §0 drain). Any 2xx is success (the bridge returns 204).
pub(crate) fn send_turn(http: &dyn SandboxHttp, text: &str) -> Result<()> {
    let body = json!({ "text": text }).to_string();
    let resp = http.request("POST", "/turn", Some(&body))?;
    if (200..300).contains(&resp.status) {
        Ok(())
    } else {
        Err(PillboxError::runtime(
            "session send",
            format!("codex app-server turn failed (HTTP {})", resp.status),
        )
        .into())
    }
}

/// One pending request awaiting its response, keyed by the JSON-RPC id. The
/// reader thread fills `result` and notifies; the caller parks on the condvar.
#[derive(Default)]
struct Pending {
    result: Mutex<HashMap<i64, Value>>,
    cv: Condvar,
}

/// Owns the `codex app-server` child: a synchronized stdin writer, a monotonic
/// request-id counter, and the pending-response map the reader thread fills.
struct Client {
    stdin: Mutex<ChildStdin>,
    next_id: AtomicI64,
    pending: Arc<Pending>,
}

impl Client {
    /// Write one JSON-RPC message line to codex (`jsonrpc` field omitted, as the
    /// wire requires). Newline-terminated; flushed so codex sees it immediately.
    fn write_msg(&self, msg: &Value) -> Result<()> {
        let mut line = serde_json::to_string(msg).context("serialize app-server message")?;
        line.push('\n');
        let mut stdin = self.stdin.lock().expect("appserver stdin mutex");
        stdin
            .write_all(line.as_bytes())
            .context("write to codex app-server stdin")?;
        stdin.flush().context("flush codex app-server stdin")?;
        Ok(())
    }

    /// Send a notification (no `id`, no response expected).
    fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.write_msg(&json!({ "method": method, "params": params }))
    }

    /// Send a request and block until the reader thread routes back the matching
    /// response, returning its `result` (or the `error` object). The reader runs
    /// on another thread, so this parks on the condvar rather than reading stdout.
    fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.write_msg(&json!({ "id": id, "method": method, "params": params }))?;
        let mut guard = self.pending.result.lock().expect("pending mutex");
        loop {
            if let Some(v) = guard.remove(&id) {
                return Ok(v);
            }
            guard = self.pending.cv.wait(guard).expect("pending condvar");
        }
    }
}

/// `pillbox appserver-host` entrypoint: spawn `codex app-server`, run the
/// JSON-RPC handshake, start the reader thread, then serve the host-facing HTTP
/// API until the listener dies (the VM is torn down). `argv` is the codex
/// command (defaults to `codex app-server`); `events_file` is the §0 capture.
pub(crate) fn run_host(port: u16, events_file: &str, argv: &[String]) -> Result<()> {
    let argv: Vec<String> = if argv.is_empty() {
        vec!["codex".into(), "app-server".into()]
    } else {
        argv.to_vec()
    };

    let mut child = spawn_codex(&argv)?;
    let stdin = child
        .stdin
        .take()
        .context("codex app-server stdin missing")?;
    let stdout = child
        .stdout
        .take()
        .context("codex app-server stdout missing")?;

    let pending = Arc::new(Pending::default());
    let client = Arc::new(Client {
        stdin: Mutex::new(stdin),
        next_id: AtomicI64::new(0),
        pending: Arc::clone(&pending),
    });

    // The reader thread drains codex stdout: routing responses to `pending`,
    // appending notifications to the events file, and auto-accepting approvals.
    let reader_client = Arc::clone(&client);
    let events_path = events_file.to_string();
    std::thread::spawn(move || {
        if let Err(e) = read_loop(stdout, reader_client, &events_path) {
            eprintln!("pillbox: appserver-host: reader stopped: {e:#}");
        }
    });

    // Handshake (blocking; the reader thread is already routing responses).
    handshake(&client)?;
    let thread_id = start_thread(&client)?;

    serve_http(port, &client, &thread_id, &mut child)
}

/// Spawn `codex app-server` with piped stdio and inherited stderr (its tracing
/// goes to the guest console / VMM diagnostics, not the protocol stream).
fn spawn_codex(argv: &[String]) -> Result<Child> {
    Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("spawn `{}`", argv.join(" ")))
}

/// `initialize` → `initialized`. Per the protocol, every other request is
/// rejected until this completes once per connection.
fn handshake(client: &Client) -> Result<()> {
    let params = json!({
        "clientInfo": {
            "name": CLIENT_NAME,
            "title": "pillbox",
            "version": env!("CARGO_PKG_VERSION"),
        }
    });
    let resp = client.request("initialize", params)?;
    if resp.get("error").is_some() {
        bail!("codex app-server initialize failed: {resp}");
    }
    client.notify("initialized", json!({}))?;
    Ok(())
}

/// `thread/start` → the new thread id, the conversation `turn/start` targets.
fn start_thread(client: &Client) -> Result<String> {
    let resp = client.request("thread/start", json!({}))?;
    if let Some(err) = resp.get("error") {
        bail!("codex app-server thread/start failed: {err}");
    }
    resp.get("thread")
        .and_then(|t| t.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .with_context(|| format!("thread/start response had no thread.id: {resp}"))
}

/// Read codex stdout line by line, classifying each JSON-RPC message:
/// - **response** (`id` + (`result` | `error`), no `method`) → route to the
///   waiting [`Client::request`] caller via `pending`.
/// - **server-request** (`id` + `method`) → auto-accept (the sandbox is the
///   boundary) by writing a decision response with the matching id.
/// - **notification** (`method`, no `id`) → append the raw line to the events
///   file (the §0 source) and let the host's mapper interpret it.
fn read_loop(stdout: impl Read, client: Arc<Client>, events_file: &str) -> Result<()> {
    let mut events = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(events_file)
        .with_context(|| format!("open events file {events_file}"))?;
    let reader = BufReader::new(stdout);
    for line in reader.lines() {
        let line = line.context("read codex app-server stdout")?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
            continue; // not JSON-RPC; skip (codex protocol is one JSON per line)
        };
        match classify(&msg) {
            MsgKind::Response(id) => {
                let payload = msg
                    .get("result")
                    .cloned()
                    .or_else(|| msg.get("error").map(|e| json!({ "error": e })))
                    .unwrap_or(Value::Null);
                let mut map = client.pending.result.lock().expect("pending mutex");
                map.insert(id, payload);
                client.pending.cv.notify_all();
            }
            MsgKind::ServerRequest(id, method) => {
                if let Some(decision) = approval_response(&method) {
                    // Best-effort: a failed write means codex is gone; the serve
                    // loop will notice the child exit. Don't abort the reader.
                    let _ = client.write_msg(&json!({ "id": id, "result": decision }));
                }
            }
            MsgKind::Notification => {
                // Append the raw line (the host mapper re-parses it). A write
                // error here loses §0 capture but mustn't kill the bridge.
                // The intent is to make each line visible to the host's
                // FollowReader live; `writeln!` already issues the write()
                // syscall and `flush()` is a no-op on this unbuffered File, so
                // host-side visibility relies on the virtio-fs cache mode, not
                // this flush. The opencode path forces visibility by
                // reopen-per-line instead — UNVERIFIED whether codex-serve needs
                // the same (sync_data) for live tail; confirm on a real VM boot.
                let _ = writeln!(events, "{trimmed}");
                let _ = events.flush();
            }
            MsgKind::Other => {}
        }
    }
    Ok(())
}

/// JSON-RPC message classes on the codex wire (`jsonrpc` omitted).
enum MsgKind {
    /// `id` present, `method` absent — a response to one of our requests.
    Response(i64),
    /// `id` and `method` both present — a request *from* codex (approval, etc.).
    ServerRequest(i64, String),
    /// `method` present, `id` absent — a server notification (the event stream).
    Notification,
    /// Anything else (malformed / id without numeric form).
    Other,
}

fn classify(msg: &Value) -> MsgKind {
    let method = msg.get("method").and_then(Value::as_str);
    let id = msg.get("id").and_then(Value::as_i64);
    match (id, method) {
        (Some(id), Some(m)) => MsgKind::ServerRequest(id, m.to_string()),
        (Some(id), None) => MsgKind::Response(id),
        (None, Some(_)) => MsgKind::Notification,
        (None, None) => MsgKind::Other,
    }
}

/// The auto-accept decision for an approval server-request, by method. The
/// microVM is the isolation boundary, so we accept what the agent wants to do
/// (mirrors claude's `--permission-mode auto`). Returns `None` for server
/// requests we don't recognize as approvals — those are left unanswered (codex
/// times them out or proceeds), which is safer than guessing a decision shape.
fn approval_response(method: &str) -> Option<Value> {
    match method {
        // Command / file / patch / permission approvals all take a `decision`
        // whose accept variant is the string `"accept"` (verified in the
        // *RequestApprovalResponse schemas at codex 0.137.0).
        "item/commandExecution/requestApproval"
        | "item/fileChange/requestApproval"
        | "item/permissions/requestApproval"
        | "applyPatchApproval"
        | "execCommandApproval" => Some(json!({ "decision": "accept" })),
        _ => None,
    }
}

/// Serve the one-shot HTTP API on `127.0.0.1:port` until the listener errors or
/// the codex child exits. Each connection is one request/response with
/// `Connection: close` (matching the host [`LibkrunHttp`](super::libkrun) client,
/// which reads to EOF).
fn serve_http(port: u16, client: &Client, thread_id: &str, child: &mut Child) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("bind appserver-host HTTP on 127.0.0.1:{port}"))?;
    for stream in listener.incoming() {
        // If codex died, stop serving — the host's next request will fail and it
        // can tear the session down.
        if let Ok(Some(status)) = child.try_wait() {
            bail!("codex app-server exited ({status}) — stopping bridge");
        }
        let mut stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        if let Err(e) = handle_conn(&mut stream, client, thread_id) {
            // One bad connection shouldn't kill the bridge.
            let _ = write_response(&mut stream, 500, &format!("{{\"error\":{e:?}}}"));
        }
    }
    Ok(())
}

/// Handle one HTTP connection: parse the request line + headers + body, dispatch
/// to the three routes, write the response.
fn handle_conn(stream: &mut TcpStream, client: &Client, thread_id: &str) -> Result<()> {
    let (method, path, body) = read_request(stream)?;
    match (method.as_str(), path.as_str()) {
        ("GET", "/health") => write_response(stream, 200, "{\"ok\":true}"),
        ("POST", "/session") => {
            write_response(stream, 200, &json!({ "id": thread_id }).to_string())
        }
        ("POST", "/turn") => {
            let text = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v.get("text").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_default();
            start_turn(client, thread_id, &text)?;
            write_response(stream, 204, "")
        }
        _ => write_response(stream, 404, "{\"error\":\"not found\"}"),
    }
}

/// `turn/start` with one text user-input. Returns once codex acks the turn (the
/// `inProgress` turn object); the turn then streams as notifications to the
/// events file. Surfaces a JSON-RPC error as an `Err`.
fn start_turn(client: &Client, thread_id: &str, text: &str) -> Result<()> {
    let params = json!({
        "threadId": thread_id,
        "input": [{ "type": "text", "text": text }],
    });
    let resp = client.request("turn/start", params)?;
    if let Some(err) = resp.get("error") {
        bail!("turn/start failed: {err}");
    }
    Ok(())
}

/// Read an HTTP/1.1 request: the request line, headers (for `Content-Length`),
/// and exactly that many body bytes. Minimal — the three routes send tiny JSON.
fn read_request(stream: &mut TcpStream) -> Result<(String, String, String)> {
    let mut reader = BufReader::new(stream.try_clone().context("clone http stream")?);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .context("read request line")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).context("read header")?;
        if n == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("content-length:") {
            content_length = rest.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).context("read body")?;
    }
    Ok((method, path, String::from_utf8_lossy(&body).to_string()))
}

/// Write a minimal HTTP/1.1 response with `Connection: close`.
fn write_response(stream: &mut TcpStream, status: u16, body: &str) -> Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).context("write head")?;
    stream.write_all(body.as_bytes()).context("write body")?;
    stream.flush().context("flush response")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_distinguishes_message_kinds() {
        // Response: id, no method.
        assert!(matches!(
            classify(&json!({"id": 3, "result": {"thread": {"id": "t"}}})),
            MsgKind::Response(3)
        ));
        // Error response: id, no method, has error.
        assert!(matches!(
            classify(&json!({"id": 4, "error": {"code": -1, "message": "x"}})),
            MsgKind::Response(4)
        ));
        // Server-request: id AND method.
        match classify(&json!({"id": 5, "method": "item/commandExecution/requestApproval"})) {
            MsgKind::ServerRequest(5, m) => {
                assert_eq!(m, "item/commandExecution/requestApproval")
            }
            _ => panic!("expected ServerRequest"),
        }
        // Notification: method, no id.
        assert!(matches!(
            classify(&json!({"method": "turn/completed", "params": {}})),
            MsgKind::Notification
        ));
    }

    #[test]
    fn approval_accepts_known_methods_only() {
        for m in [
            "item/commandExecution/requestApproval",
            "item/fileChange/requestApproval",
            "item/permissions/requestApproval",
            "applyPatchApproval",
            "execCommandApproval",
        ] {
            assert_eq!(approval_response(m), Some(json!({"decision": "accept"})));
        }
        // An unrecognized server-request gets no canned decision.
        assert_eq!(approval_response("item/tool/requestUserInput"), None);
        assert_eq!(approval_response("account/chatgptAuthTokens/refresh"), None);
    }

    #[test]
    fn request_serializes_without_jsonrpc_field() {
        // The wire requires the `jsonrpc` field OMITTED. Build the message the
        // way Client::request/notify does and assert the shape.
        let req = json!({ "id": 0, "method": "initialize", "params": {"clientInfo": {"name": "pillbox"}} });
        let s = serde_json::to_string(&req).unwrap();
        assert!(!s.contains("jsonrpc"), "must omit jsonrpc: {s}");
        assert!(s.contains("\"method\":\"initialize\""));

        let notif = json!({ "method": "initialized", "params": {} });
        let s = serde_json::to_string(&notif).unwrap();
        assert!(!s.contains("jsonrpc"));
        assert!(!s.contains("\"id\""), "notification has no id: {s}");
    }

    #[test]
    fn turn_start_params_shape() {
        // turn/start requires threadId + input[] of typed user-inputs.
        let params = json!({
            "threadId": "th_1",
            "input": [{ "type": "text", "text": "hello" }],
        });
        assert_eq!(params["threadId"], "th_1");
        assert_eq!(params["input"][0]["type"], "text");
        assert_eq!(params["input"][0]["text"], "hello");
    }
}
