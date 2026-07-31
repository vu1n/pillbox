//! `pillbox sandbox …` handlers — spawn a long-lived container, then drive it
//! over the PTY-free contract via two channels:
//!
//! - **exec** (`exec`): run a one-off command. `--json` emits
//!   `ExecStarted`/`ExecOutput`/`ExecExit` as JSONL; plain passes raw bytes
//!   through and mirrors the exit code.
//! - **agent** (`agent`): run a headless agent turn, emitting contract events
//!   (`--json`) or a human trace. Requires the sandbox to be spawned
//!   `--agent`. Two driver flavours, picked by which adapter the agent has:
//!     - `AgentDriver` (stdout) — execs the harness's `run_argv` (claude `-p`,
//!       pi) and streams its stdout JSON-lines through the normalizer.
//!     - `ServeDriver` (HTTP/SSE) — for harnesses with no headless stdout mode
//!       (opencode): brings up `opencode serve` in the sandbox, POSTs the
//!       prompt, and streams its SSE events through the normalizer.

use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::Result;
use base64::Engine as _;

use crate::agents::harness::{self, HarnessAdapter, ServeAdapter};
use crate::agents::{workspace_mount_name, GUEST_HOME, GUEST_WORKSPACE};
use crate::cli::SandboxAction;
use crate::contract::{
    Correlation, Event, EventEmitter, EventSink, ExecExit, ExecOutput, ExecStarted, JsonlSink,
    Payload, RunFinished, RunStarted, StdStream, ToolStatus,
};
use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::sandbox::{docker::DockerBackend, SandboxBackend};
use crate::sandboxes::{self, Sandbox, BACKEND_DOCKER};
use crate::{docker, session};

pub(crate) fn dispatch(resolved: &Pillbox, action: SandboxAction) -> Result<()> {
    match action {
        SandboxAction::Spawn {
            image,
            agent,
            workspace,
            label,
        } => spawn(resolved, image, agent, workspace, label),
        SandboxAction::Agent { id, json, prompt } => agent(resolved, &id, json, prompt),
        SandboxAction::Exec { id, json, argv } => exec(resolved, &id, json, argv),
        SandboxAction::Destroy { id } => destroy(resolved, &id),
        SandboxAction::List { json } => list(resolved, json),
    }
}

/// The sandbox group (`spawn`/`exec`/`agent`) needs a long-lived exec target —
/// the `long_lived_exec` capability, which only the Docker backend provides
/// (the libkrun microVM is one-shot). Independent of the run-path default
/// (`select_backend`), this group always runs on Docker. The guard sources that
/// dependence from the capability rather than a bare backend string, so the
/// default flip to libkrun can't silently strand `sandbox spawn` on a backend
/// that lacks the exec channel. Docker daemon/image health is a separate probe
/// (`docker::check_ready_for`, run right after).
fn require_long_lived_exec_backend() -> Result<()> {
    if DockerBackend.capabilities().long_lived_exec {
        return Ok(());
    }
    Err(PillboxError::resource(
        "sandbox",
        "the sandbox group needs the docker backend (long-lived exec)",
    )
    .with_next("use `pillbox run` (the microVM run path)")
    .into())
}

fn spawn(
    resolved: &Pillbox,
    image: Option<String>,
    agent: Option<String>,
    workspace: Option<PathBuf>,
    label: Option<String>,
) -> Result<()> {
    require_long_lived_exec_backend()?;
    let image = match image {
        Some(i) => {
            docker::check_ready(&i)?;
            i
        }
        None => docker::check_ready_for(resolved)?,
    };
    let workspace_host = match workspace {
        Some(p) => p,
        None => std::env::current_dir()
            .map_err(|e| PillboxError::runtime("sandbox spawn", format!("resolve cwd: {e}")))?,
    };
    let workspace_name = workspace_mount_name(&workspace_host, None)?;
    let guest_workspace = format!("{GUEST_WORKSPACE}/{workspace_name}");
    let id = Sandbox::new_id();

    // Detached, idle (`sleep infinity`) container with the workspace mounted.
    // No `--rm` (destroy removes it explicitly), no `-it` (no PTY).
    let mut args: Vec<String> = vec![
        "-d".into(),
        "--add-host".into(),
        "host.docker.internal:host-gateway".into(),
        "--label".into(),
        format!("pillbox.sandbox={id}"),
        "-e".into(),
        format!("HOME={GUEST_HOME}"),
        "-e".into(),
        format!("PATH=/usr/local/bin:/usr/bin:/bin:{GUEST_HOME}/.local/bin"),
    ];

    // Agent-ready sandbox: mount the agent's auth and run non-root. Headless
    // skip-permissions refuses under root (validated), so the agent channel
    // needs uid 1000. Bare exec-only sandboxes stay root for now.
    let agent_id = match &agent {
        Some(a) => {
            let spec = crate::agents::lookup("sandbox spawn", a)?;
            if !spec.is_authenticated(resolved) {
                return Err(PillboxError::runtime(
                    "sandbox spawn",
                    format!("no stored credentials for `{a}`"),
                )
                .with_next(format!("pillbox auth login --agent {a}"))
                .into());
            }
            let home = spec.home_dir(resolved)?;
            args.push("--user".into());
            args.push("1000:1000".into());
            args.push("-v".into());
            args.push(format!("{}:{GUEST_HOME}", home.display()));
            Some(spec.id().to_string())
        }
        None => None,
    };

    args.push("-v".into());
    args.push(format!("{}:{guest_workspace}", workspace_host.display()));
    args.push("-w".into());
    args.push(guest_workspace);
    args.push(image.clone());
    args.push("sleep".into());
    args.push("infinity".into());

    let backend_ref = docker::run_detached(&args, &std::collections::BTreeMap::new())?;

    let record = Sandbox {
        id: id.clone(),
        backend: BACKEND_DOCKER.into(),
        backend_ref,
        image,
        agent: agent_id,
        workspace: workspace_host.display().to_string(),
        label,
        created_at: session::now_rfc3339(),
        status: "ready".into(),
    };
    sandboxes::write(resolved, &record)?;
    let kind = record
        .agent
        .as_deref()
        .map(|a| format!(" ({a})"))
        .unwrap_or_default();
    eprintln!("pillbox: ✓ sandbox {id} ready{kind}");
    println!("{id}"); // stdout: the id, for `ID=$(pillbox sandbox spawn)`
    Ok(())
}

/// The agent channel: run a headless agent turn in `id` and stream its
/// normalized contract events. Dispatches to one of two harness-agnostic
/// drivers by the agent's adapter kind: `AgentDriver` for stdout-streaming
/// harnesses (claude/pi), `ServeDriver` for serve harnesses (opencode).
fn agent(resolved: &Pillbox, id: &str, json: bool, prompt: Vec<String>) -> Result<()> {
    let prompt = prompt.join(" ");
    if prompt.trim().is_empty() {
        return Err(
            PillboxError::usage("sandbox agent", "empty prompt (use `-- your prompt`)").into(),
        );
    }
    let record = sandboxes::resolve(resolved, id)?;
    if record.backend != BACKEND_DOCKER {
        return Err(PillboxError::usage(
            "sandbox agent",
            format!("backend `{}` is not supported yet", record.backend),
        )
        .into());
    }
    let agent_id = record.agent.clone().ok_or_else(|| {
        PillboxError::usage(
            "sandbox agent",
            format!("sandbox `{}` was not spawned with an agent", record.id),
        )
        .with_next("pillbox sandbox spawn --agent claude")
    })?;

    let sink: Box<dyn EventSink> = if json {
        Box::new(JsonlSink::new(io::stdout()))
    } else {
        Box::new(HumanSink)
    };

    // Two driver flavours behind one verb. Stdout-streaming harnesses
    // (claude `-p`, pi) use the shared `AgentDriver`: exec the harness, feed
    // its stdout JSON-lines through the normalizer. Serve harnesses
    // (opencode) use `ServeDriver`: bring up `opencode serve` in the sandbox,
    // POST the prompt, and feed its SSE stream through the normalizer. Same
    // contract events out either way.
    if let Some(adapter) = harness::lookup(&agent_id) {
        let argv = adapter.run_argv(&prompt);
        let mut driver = AgentDriver::new(adapter, sink, record.id.clone());
        docker::exec_streamed(&record.backend_ref, &argv, |is_stderr, bytes| {
            // the harness emits its protocol on stdout; stderr is diagnostics
            if is_stderr {
                Ok(())
            } else {
                driver.feed(bytes)
            }
        })?;
        return driver.finish();
    }
    if let Some(adapter) = harness::lookup_serve(&agent_id) {
        let mut driver = ServeDriver::new(adapter, sink, record.id.clone());
        return driver.run(&record.backend_ref, &prompt);
    }
    Err(PillboxError::usage(
        "sandbox agent",
        format!("no harness adapter for `{agent_id}`"),
    )
    .into())
}

/// The shared agent-channel driver: buffers a harness's stdout into JSON
/// lines, normalizes each via the adapter, and emits the resulting contract
/// events to a sink — independent of which harness or sink is in play.
struct AgentDriver {
    adapter: Box<dyn HarnessAdapter>,
    emitter: EventEmitter,
    buf: Vec<u8>,
}

impl AgentDriver {
    fn new(adapter: Box<dyn HarnessAdapter>, sink: Box<dyn EventSink>, sandbox_id: String) -> Self {
        let run_id = crate::registry::new_id();
        Self {
            adapter,
            emitter: EventEmitter::new(sink, sandbox_id, Correlation::Run(run_id)),
            buf: Vec::new(),
        }
    }

    /// Feed a raw stdout chunk; emits events for every complete line it sees.
    fn feed(&mut self, bytes: &[u8]) -> Result<()> {
        self.buf.extend_from_slice(bytes);
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            self.emit_line(&line[..line.len() - 1])?;
        }
        Ok(())
    }

    /// Flush a trailing line that arrived without a newline.
    fn finish(&mut self) -> Result<()> {
        let rest = std::mem::take(&mut self.buf);
        self.emit_line(&rest)
    }

    fn emit_line(&mut self, line: &[u8]) -> Result<()> {
        if line.is_empty() {
            return Ok(());
        }
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
            return Ok(()); // tolerate non-JSON noise on the stream
        };
        for payload in self.adapter.parse_line(&value) {
            self.emitter.emit(payload)?;
        }
        Ok(())
    }
}

/// The serve-channel driver — the opencode counterpart to [`AgentDriver`].
///
/// opencode has no headless stdout-JSON mode; it runs an HTTP server. So this
/// driver:
///   1. starts `opencode serve` in the sandbox (detached `docker exec -d`),
///   2. waits for `GET /global/health` to answer,
///   3. creates a session (`POST /session`) and emits a synthetic `RunStarted`
///      (opencode's `session.created` predates our subscription),
///   4. fires the prompt (`POST /session/{id}/prompt_async`) on a background
///      thread after a short delay, so the subscription attaches first, and
///   5. consumes the SSE stream (`GET /event`) on the main thread, feeding
///      each event through the [`ServeAdapter`] normalizer.
///
/// Every REST call and the SSE subscription run as `curl` *inside* the
/// container (`docker exec`), so the transport reuses the existing docker
/// plumbing — no host↔container port publishing on a long-lived sandbox, and
/// the server stays bound to the container's loopback. The driver stops when
/// the adapter reports a terminal event (`session.idle`).
struct ServeDriver {
    adapter: Box<dyn ServeAdapter>,
    emitter: EventEmitter,
    /// Set once a terminal event (`session.idle` → `RunFinished`) is emitted,
    /// so `run` can synthesize a `RunFinished` fallback if the stream ends
    /// (server died, connection dropped) without one.
    terminal_seen: bool,
}

/// Loopback port for the in-sandbox `opencode serve`. Fixed (not published to
/// the host) — one agent turn per sandbox at a time, server bound to the
/// container's own loopback.
const SERVE_PORT: u16 = 47821;

impl ServeDriver {
    fn new(adapter: Box<dyn ServeAdapter>, sink: Box<dyn EventSink>, sandbox_id: String) -> Self {
        let run_id = crate::registry::new_id();
        Self {
            adapter,
            emitter: EventEmitter::new(sink, sandbox_id, Correlation::Run(run_id)),
            terminal_seen: false,
        }
    }

    /// Emit one payload through the shared emitter (durable seq + run id).
    fn emit(&mut self, payload: Payload) -> Result<()> {
        self.emitter.emit(payload)
    }

    fn run(&mut self, container: &str, prompt: &str) -> Result<()> {
        let base = format!("http://127.0.0.1:{SERVE_PORT}");

        // 1. Bring up the server in the sandbox (detached).
        docker::exec_detached(container, &self.adapter.serve_argv(SERVE_PORT))?;

        // 2. Wait for it to answer. opencode boots in well under a second
        // normally; poll up to ~15s before giving up.
        Self::wait_ready(container, &base)?;

        // 3. Create a session.
        let session_id = Self::create_session(container, &base)?;

        // Synthesize RunStarted: opencode's `session.created` fires before our
        // SSE subscription attaches (step 5), so we'd miss it. The driver owns
        // the run boundary anyway, so emit it here once the session exists.
        self.emit(Payload::RunStarted(RunStarted {
            agent: "opencode".into(),
            parent_run_id: String::new(),
            base_snapshot: String::new(),
            requested: None,
        }))?;

        // 4. Fire the prompt on a background thread *after* a short delay, so
        // the foreground SSE subscription (step 5) is attached first —
        // opencode replays no history to a late subscriber, so events emitted
        // before `/event` is connected would be missed. Only `Send` data
        // (strings) crosses the boundary; the non-`Send` sink + normalizer
        // stay on the main thread with the SSE loop.
        let prompt_body = serde_json::json!({
            "parts": [{ "type": "text", "text": prompt }]
        })
        .to_string();
        let prompt_url = format!("{base}/session/{session_id}/prompt_async");
        let container_owned = container.to_string();
        let prompter = std::thread::spawn(move || -> Result<()> {
            std::thread::sleep(std::time::Duration::from_millis(400));
            let (code, out) = docker::exec_capture(
                &container_owned,
                &curl_argv(&[
                    "-s",
                    "-X",
                    "POST",
                    "-H",
                    "content-type: application/json",
                    "-d",
                    &prompt_body,
                    &prompt_url,
                ]),
            )?;
            if code != 0 {
                return Err(PillboxError::runtime(
                    "sandbox agent",
                    format!("opencode prompt POST failed (curl exit {code}): {out}"),
                )
                .into());
            }
            Ok(())
        });

        // 5. Consume the SSE stream on the main thread until `session.idle`.
        let stream_argv = curl_argv(&["-sN", &format!("{base}/event")]);
        let stream_result =
            docker::exec_stream_lines(container, &stream_argv, |line| self.on_sse_line(line));

        // Surface a prompt-POST failure ahead of a stream error — a failed
        // prompt is the more actionable diagnostic (e.g. unauthenticated).
        match prompter.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(PillboxError::runtime(
                    "sandbox agent",
                    "opencode prompt thread panicked".to_string(),
                )
                .into())
            }
        }
        stream_result?;

        // The stream ended without a terminal `session.idle` (server died /
        // connection dropped). Synthesize a non-zero RunFinished so consumers
        // always see a run boundary and can tell the turn didn't complete
        // cleanly.
        if !self.terminal_seen {
            self.emit(Payload::RunFinished(RunFinished {
                result_snapshot: String::new(),
                exit_code: 1,
                served_model: None,
                effective_limits: None,
            }))?;
        }
        Ok(())
    }

    /// Feed one raw SSE line. Returns `Ok(true)` to stop the stream (terminal
    /// event seen). Non-`data:` lines (SSE comments, blank separators) are
    /// skipped.
    fn on_sse_line(&mut self, line: &str) -> Result<bool> {
        let Some(json) = line
            .strip_prefix("data: ")
            .or_else(|| line.strip_prefix("data:"))
        else {
            return Ok(false);
        };
        let Ok(event) = serde_json::from_str::<serde_json::Value>(json.trim()) else {
            return Ok(false); // tolerate keep-alive / non-JSON noise
        };
        for payload in self.adapter.parse_event(&event) {
            match payload {
                // The driver already synthesized RunStarted (the adapter's
                // `session.created` may also fire if we catch it); don't
                // double-emit.
                Payload::RunStarted(_) => continue,
                Payload::RunFinished(_) => self.terminal_seen = true,
                _ => {}
            }
            self.emit(payload)?;
        }
        Ok(self.adapter.is_terminal(&event))
    }

    fn wait_ready(container: &str, base: &str) -> Result<()> {
        let url = format!("{base}/global/health");
        for _ in 0..75 {
            if let Ok((0, _)) =
                docker::exec_capture(container, &curl_argv(&["-sf", "-o", "/dev/null", &url]))
            {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        Err(PillboxError::runtime(
            "sandbox agent",
            "opencode serve did not become ready within ~15s".to_string(),
        )
        .with_next("pillbox sandbox exec <id> -- opencode serve  # check it starts manually")
        .into())
    }

    fn create_session(container: &str, base: &str) -> Result<String> {
        let (code, out) = docker::exec_capture(
            container,
            &curl_argv(&["-s", "-X", "POST", "-d", "{}", &format!("{base}/session")]),
        )?;
        if code != 0 {
            return Err(PillboxError::runtime(
                "sandbox agent",
                format!("opencode session create failed (curl exit {code}): {out}"),
            )
            .into());
        }
        serde_json::from_str::<serde_json::Value>(&out)
            .ok()
            .and_then(|v| v.get("id").and_then(|i| i.as_str()).map(str::to_string))
            .ok_or_else(|| {
                PillboxError::runtime(
                    "sandbox agent",
                    format!("opencode session create returned no id: {out}"),
                )
                .into()
            })
    }
}

/// Build a `curl` argv. Centralized so every in-sandbox REST/SSE call shares
/// the same base flags.
fn curl_argv(extra: &[&str]) -> Vec<String> {
    let mut argv = vec!["curl".to_string()];
    argv.extend(extra.iter().map(|s| s.to_string()));
    argv
}

/// Human-readable rendering of the agent event stream (the non-`--json` path):
/// assistant text to stdout, tool activity + lifecycle to stderr.
struct HumanSink;
impl EventSink for HumanSink {
    fn emit(&mut self, event: &Event) -> Result<()> {
        match &event.payload {
            Payload::RunStarted(r) => eprintln!("pillbox: {} run started", r.agent),
            Payload::ToolCall(t) if t.status == ToolStatus::Running => {
                eprintln!("  → {}", t.name)
            }
            Payload::MessageDelta(d) => {
                print!("{}", d.text);
                let _ = io::stdout().flush();
            }
            Payload::RunFinished(_) => println!(),
            Payload::Custom(c) if c.name == "usage" => {
                if let Some(cost) = c.payload.as_ref().and_then(|p| p.get("total_cost_usd")) {
                    eprintln!("pillbox: cost ${cost}");
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn exec(resolved: &Pillbox, id: &str, json: bool, argv: Vec<String>) -> Result<()> {
    if argv.is_empty() {
        return Err(
            PillboxError::usage("sandbox exec", "no command given (use `-- argv…`)").into(),
        );
    }
    let record = sandboxes::resolve(resolved, id)?;
    if record.backend != BACKEND_DOCKER {
        return Err(PillboxError::usage(
            "sandbox exec",
            format!("backend `{}` is not supported yet", record.backend),
        )
        .into());
    }
    if json {
        exec_json(&record, &argv)
    } else {
        exec_passthrough(&record, &argv)
    }
}

/// Structured mode: emit the contract's exec events as JSONL on stdout.
/// pillbox exits 0 here — the command's exit code rides in `ExecExit`.
fn exec_json(record: &Sandbox, argv: &[String]) -> Result<()> {
    // A distinct per-exec id (≠ sandbox id) so a consumer can correlate
    // concurrent execs on the same sandbox.
    let exec_id = crate::registry::new_id();
    let mut emitter = EventEmitter::new(
        Box::new(JsonlSink::new(io::stdout())),
        record.id.clone(),
        Correlation::Exec(exec_id),
    );

    emitter.emit(Payload::ExecStarted(ExecStarted {
        argv: argv.to_vec(),
    }))?;
    let code = docker::exec_streamed(&record.backend_ref, argv, |is_stderr, bytes| {
        let stream = if is_stderr {
            StdStream::Stderr
        } else {
            StdStream::Stdout
        };
        emitter.emit(Payload::ExecOutput(ExecOutput {
            stream,
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        }))
    })?;
    emitter.emit(Payload::ExecExit(ExecExit { code }))?;
    Ok(())
}

/// Plain mode: stream raw bytes to the terminal and mirror the exit code,
/// so `pillbox sandbox exec <id> -- python foo.py` behaves like a local run.
fn exec_passthrough(record: &Sandbox, argv: &[String]) -> Result<()> {
    let code = docker::exec_streamed(&record.backend_ref, argv, |is_stderr, bytes| {
        if is_stderr {
            let mut e = io::stderr();
            e.write_all(bytes).and_then(|()| e.flush())?;
        } else {
            let mut o = io::stdout();
            o.write_all(bytes).and_then(|()| o.flush())?;
        }
        Ok(())
    })?;
    std::process::exit(code);
}

fn destroy(resolved: &Pillbox, id: &str) -> Result<()> {
    let record = sandboxes::resolve(resolved, id)?;
    if record.backend == BACKEND_DOCKER {
        docker::rm(&record.backend_ref)?;
    }
    sandboxes::delete(resolved, &record.id)?;
    eprintln!("pillbox: ✓ sandbox {} destroyed", record.id);
    Ok(())
}

fn list(resolved: &Pillbox, json: bool) -> Result<()> {
    let all = sandboxes::list(resolved)?;
    if json {
        let arr = all
            .iter()
            .map(|s| {
                let mut o = serde_json::Map::new();
                o.insert("id".into(), s.id.clone().into());
                o.insert("backend".into(), s.backend.clone().into());
                o.insert("image".into(), s.image.clone().into());
                o.insert("workspace".into(), s.workspace.clone().into());
                o.insert("status".into(), s.status.clone().into());
                o.insert("created_at".into(), s.created_at.clone().into());
                if let Some(a) = &s.agent {
                    o.insert("agent".into(), a.clone().into());
                }
                if let Some(l) = &s.label {
                    o.insert("label".into(), l.clone().into());
                }
                serde_json::Value::Object(o)
            })
            .collect();
        println!(
            "{}",
            crate::paths::json_v1(vec![
                (
                    "pillbox",
                    serde_json::Value::String(resolved.display_name().into())
                ),
                ("sandboxes", serde_json::Value::Array(arr)),
            ])
        );
        return Ok(());
    }
    if all.is_empty() {
        println!("(no sandboxes for `{}`)", resolved.display_name());
        println!();
        println!("Spawn one with: pillbox sandbox spawn");
        return Ok(());
    }
    println!("Sandboxes for `{}`:", resolved.display_name());
    for s in all {
        let label = s.label.map(|l| format!("  ({l})")).unwrap_or_default();
        println!("  {}  {}  {}{label}", s.id, s.status, s.created_at);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::harness::ClaudeAdapter;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// Sink that keeps a clone of every emitted event for assertions.
    struct CollectSink(Rc<RefCell<Vec<Event>>>);
    impl EventSink for CollectSink {
        fn emit(&mut self, event: &Event) -> Result<()> {
            self.0.borrow_mut().push(event.clone());
            Ok(())
        }
    }

    fn type_of(p: &Payload) -> &'static str {
        match p {
            Payload::RunStarted(_) => "run_started",
            Payload::ToolCall(_) => "tool_call",
            Payload::MessageStart(_) => "message_start",
            Payload::MessageDelta(_) => "message_delta",
            Payload::MessageEnd(_) => "message_end",
            Payload::RunFinished(_) => "run_finished",
            Payload::Custom(_) => "custom",
            _ => "other",
        }
    }

    #[test]
    fn driver_buffers_split_chunks_into_the_full_event_sequence() {
        // Real `claude -p` stream-json lines for one turn that ran a Bash tool.
        let raw = concat!(
            r#"{"type":"system","subtype":"init","apiKeySource":"none"}"#,
            "\n",
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"echo HELLO"}}]}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":false,"content":"HELLO"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"id":"m2","content":[{"type":"text","text":"done"}]}}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.02,"num_turns":2}"#,
            "\n",
        )
        .as_bytes();

        let collected = Rc::new(RefCell::new(Vec::new()));
        let mut driver = AgentDriver::new(
            Box::new(ClaudeAdapter::default()),
            Box::new(CollectSink(collected.clone())),
            "sb-test".into(),
        );
        // 7-byte chunks split lines mid-token — exercises the line buffering.
        for chunk in raw.chunks(7) {
            driver.feed(chunk).unwrap();
        }
        driver.finish().unwrap();

        let events = collected.borrow();
        let types: Vec<&str> = events.iter().map(|e| type_of(&e.payload)).collect();
        assert_eq!(
            types,
            [
                "run_started",
                "tool_call",
                "tool_call",
                "message_start",
                "message_delta",
                "message_end",
                "run_finished",
                "custom",
            ]
        );
        // Monotonic seq from 1, consistent non-empty run_id, sandbox_id set.
        assert_eq!(
            events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            (1..=8).collect::<Vec<_>>()
        );
        assert!(events
            .iter()
            .all(|e| e.run_id == events[0].run_id && !e.run_id.is_empty()));
        assert!(events.iter().all(|e| e.sandbox_id == "sb-test"));
        // Tool pairing across lines: running → completed, name recovered.
        match (&events[1].payload, &events[2].payload) {
            (Payload::ToolCall(a), Payload::ToolCall(b)) => {
                assert_eq!(a.status, ToolStatus::Running);
                assert_eq!(b.status, ToolStatus::Completed);
                assert_eq!(b.name, "Bash");
            }
            _ => panic!("expected two tool calls"),
        }
    }
}
