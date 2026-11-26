//! `pillbox sandbox …` handlers — spawn a long-lived microVM, then drive it
//! over the PTY-free contract via two channels:
//!
//! - **exec** (`exec`): run a one-off command. `--json` emits
//!   `ExecStarted`/`ExecOutput`/`ExecExit` as JSONL; plain passes raw bytes
//!   through and mirrors the exit code.
//! - **agent** (`agent`): run a headless agent turn, emitting contract events
//!   (`--json`) or a human trace. Requires the sandbox to be spawned
//!   `--agent`. `AgentDriver` execs the harness's `run_argv` (claude `-p`,
//!   cursor, pi) and streams its stdout JSON-lines through the normalizer.
//!   Server harnesses (opencode, codex-serve) have no stdout adapter and are
//!   refused here; they run via `pillbox run`.
//!
//! Backend-blind: spawn goes through `select_backend()`, everything else through
//! the record's `sandbox_target`.

use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::Result;
use base64::Engine as _;

use crate::agents::harness::{self, HarnessAdapter};
use crate::cli::SandboxAction;
use crate::contract::{
    Correlation, Event, EventEmitter, EventSink, ExecExit, ExecOutput, ExecStarted, JsonlSink,
    Payload, StdStream, ToolStatus,
};
use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::sandbox::{sandbox_target, select_backend, Destroyed, SandboxSpawn, SandboxTarget};
use crate::sandboxes::{self, Sandbox};

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

fn spawn(
    resolved: &Pillbox,
    image: Option<String>,
    agent: Option<String>,
    workspace: Option<PathBuf>,
    label: Option<String>,
) -> Result<()> {
    // The microVM boots the pillbox's own runner image; there is no per-sandbox image.
    if let Some(image) = image {
        return Err(PillboxError::usage(
            "sandbox spawn",
            format!(
                "`--image {image}` isn't supported: the sandbox boots this pillbox's runner image"
            ),
        )
        .with_next("set `[runner] image` in pillbox.toml (or PILLBOX_RUNNER_IMAGE)")
        .into());
    }
    let workspace = match workspace {
        // Absolute, so the record names the directory the clone came from.
        Some(p) => std::fs::canonicalize(&p).map_err(|e| {
            PillboxError::usage("sandbox spawn", format!("--workspace {}: {e}", p.display()))
        })?,
        None => std::env::current_dir()
            .map_err(|e| PillboxError::runtime("sandbox spawn", format!("resolve cwd: {e}")))?,
    };
    let backend = select_backend();
    if !backend.capabilities().long_lived_exec {
        return Err(backend.capabilities().unsupported("sandbox spawn"));
    }
    let record = backend.spawn_sandbox(
        resolved,
        &SandboxSpawn {
            agent,
            workspace,
            label,
        },
    )?;
    let kind = record
        .agent
        .as_deref()
        .map(|a| format!(" ({a})"))
        .unwrap_or_default();
    eprintln!("pillbox: ✓ sandbox {} ready{kind}", record.id);
    println!("{}", record.id); // stdout: the id, for `ID=$(pillbox sandbox spawn)`
    Ok(())
}

/// The agent channel: run a headless agent turn in `id` and stream its
/// normalized contract events through the harness-agnostic `AgentDriver`.
/// An agent with no stdout adapter (e.g. opencode) is a usage error.
fn agent(resolved: &Pillbox, id: &str, json: bool, prompt: Vec<String>) -> Result<()> {
    let prompt = prompt.join(" ");
    if prompt.trim().is_empty() {
        return Err(
            PillboxError::usage("sandbox agent", "empty prompt (use `-- your prompt`)").into(),
        );
    }
    let record = sandboxes::resolve(resolved, id)?;
    let agent_id = record.agent.clone().ok_or_else(|| {
        PillboxError::usage(
            "sandbox agent",
            format!("sandbox `{}` was not spawned with an agent", record.id),
        )
        .with_next("pillbox sandbox spawn --agent claude")
    })?;
    let Some(adapter) = harness::lookup(&agent_id) else {
        return Err(PillboxError::usage(
            "sandbox agent",
            format!("no harness adapter for `{agent_id}`"),
        )
        .into());
    };
    let target = sandbox_target(&record)?;

    let sink: Box<dyn EventSink> = if json {
        Box::new(JsonlSink::new(io::stdout()))
    } else {
        Box::new(HumanSink)
    };
    let argv = adapter.run_argv(&prompt);
    let driver = AgentDriver::new(adapter, sink, record.id.clone());
    let (harness_code, finished) = run_agent_turn(target.as_ref(), driver, &argv)?;
    match agent_exit_code(json, harness_code, finished) {
        0 => Ok(()),
        code => std::process::exit(code),
    }
}

/// Run the harness `argv` in `target` through `driver`; returns the harness's
/// exit code and the `run_finished` exit code the turn reported, if any.
fn run_agent_turn(
    target: &dyn SandboxTarget,
    mut driver: AgentDriver,
    argv: &[String],
) -> Result<(i32, Option<i32>)> {
    let harness_code = target.exec(argv, &mut |is_stderr, bytes| {
        // the harness emits its protocol on stdout; stderr is diagnostics
        if is_stderr {
            Ok(())
        } else {
            driver.feed(bytes)
        }
    })?;
    driver.finish()?;
    Ok((harness_code, driver.finished))
}

/// The code `sandbox agent` exits with. The turn's outcome is the harness's
/// non-zero exit, else `run_finished`'s exit code, else 1 (no terminal event).
// --json exits 0 only when run_finished already reports the outcome, as exec's ExecExit does.
fn agent_exit_code(json: bool, harness_code: i32, finished: Option<i32>) -> i32 {
    let outcome = match (harness_code, finished) {
        (0, Some(code)) => code,
        (0, None) => 1,
        (code, _) => code,
    };
    let reported = finished.is_some_and(|code| (code != 0) == (outcome != 0));
    if json && reported {
        0
    } else {
        outcome
    }
}

/// The shared agent-channel driver: buffers a harness's stdout into JSON
/// lines, normalizes each via the adapter, and emits the resulting contract
/// events to a sink — independent of which harness or sink is in play.
struct AgentDriver {
    adapter: Box<dyn HarnessAdapter>,
    emitter: EventEmitter,
    buf: Vec<u8>,
    /// The exit code of the turn's `run_finished`, once one is emitted.
    finished: Option<i32>,
}

impl AgentDriver {
    fn new(adapter: Box<dyn HarnessAdapter>, sink: Box<dyn EventSink>, sandbox_id: String) -> Self {
        let run_id = crate::registry::new_id();
        Self {
            adapter,
            emitter: EventEmitter::new(sink, sandbox_id, Correlation::Run(run_id)),
            buf: Vec::new(),
            finished: None,
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
            if let Payload::RunFinished(finished) = &payload {
                self.finished = Some(finished.exit_code);
            }
            self.emitter.emit(payload)?;
        }
        Ok(())
    }
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
    let target = sandbox_target(&record)?;
    if json {
        exec_json(&record, target.as_ref(), &argv)
    } else {
        exec_passthrough(target.as_ref(), &argv)
    }
}

/// Structured mode: emit the contract's exec events as JSONL on stdout.
/// pillbox exits 0 here — the command's exit code rides in `ExecExit`.
fn exec_json(record: &Sandbox, target: &dyn SandboxTarget, argv: &[String]) -> Result<()> {
    // A distinct per-exec id (≠ sandbox id) so a consumer can correlate
    // concurrent execs on the same sandbox.
    let exec_id = crate::registry::new_id();
    let emitter = EventEmitter::new(
        Box::new(JsonlSink::new(io::stdout())),
        record.id.clone(),
        Correlation::Exec(exec_id),
    );
    emit_exec_events(emitter, target, argv)
}

/// `ExecStarted`, one `ExecOutput` per chunk, then `ExecExit` with the code.
fn emit_exec_events(
    mut emitter: EventEmitter,
    target: &dyn SandboxTarget,
    argv: &[String],
) -> Result<()> {
    // A refusal, a dead socket included, fails before exec_started: a started exec always ends in exec_exit or a channel error.
    let channel = target.open_exec()?;
    emitter.emit(Payload::ExecStarted(ExecStarted {
        argv: argv.to_vec(),
    }))?;
    let code = channel.run(argv, &mut |is_stderr, bytes| {
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
fn exec_passthrough(target: &dyn SandboxTarget, argv: &[String]) -> Result<()> {
    let code = target.exec(argv, &mut |is_stderr, bytes| {
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
    match sandbox_target(&record)?.destroy(resolved)? {
        Destroyed::TornDown => eprintln!("pillbox: ✓ sandbox {} destroyed", record.id),
        // The target already said what it left behind.
        Destroyed::RecordDropped => eprintln!(
            "pillbox: sandbox {} record dropped; nothing was torn down",
            record.id
        ),
    }
    Ok(())
}

/// One `sandbox list --json` entry (schema `version: 1`; optional keys omitted).
fn sandbox_json(s: &Sandbox) -> serde_json::Value {
    let mut o = serde_json::Map::new();
    o.insert("id".into(), s.id.clone().into());
    o.insert("backend".into(), s.backend.clone().into());
    o.insert("image".into(), s.image.clone().into());
    o.insert("workspace".into(), s.workspace.clone().into());
    if let Some(c) = &s.workspace_clone {
        o.insert("workspace_clone".into(), c.clone().into());
    }
    o.insert("status".into(), s.status.clone().into());
    o.insert("created_at".into(), s.created_at.clone().into());
    if let Some(a) = &s.agent {
        o.insert("agent".into(), a.clone().into());
    }
    if let Some(l) = &s.label {
        o.insert("label".into(), l.clone().into());
    }
    serde_json::Value::Object(o)
}

fn list(resolved: &Pillbox, json: bool) -> Result<()> {
    let all = sandboxes::list(resolved)?;
    if json {
        let arr = all.iter().map(sandbox_json).collect();
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

    /// A sandbox that answers every exec with fixed chunks and an exit code.
    #[derive(Clone)]
    struct FakeTarget(Vec<(bool, &'static [u8])>, i32);
    impl SandboxTarget for FakeTarget {
        fn open_exec(&self) -> Result<Box<dyn crate::sandbox::ExecChannel>> {
            Ok(Box::new(self.clone()))
        }
        fn destroy(&self, _resolved: &Pillbox) -> Result<Destroyed> {
            Ok(Destroyed::TornDown)
        }
    }
    impl crate::sandbox::ExecChannel for FakeTarget {
        fn run(
            self: Box<Self>,
            _argv: &[String],
            on_chunk: &mut dyn FnMut(bool, &[u8]) -> Result<()>,
        ) -> Result<i32> {
            for (is_stderr, b) in &self.0 {
                on_chunk(*is_stderr, b)?;
            }
            Ok(self.1)
        }
    }

    /// A sandbox whose exec channel can't be opened (a dead VM, a record-only target).
    struct RefusingTarget;
    impl SandboxTarget for RefusingTarget {
        fn open_exec(&self) -> Result<Box<dyn crate::sandbox::ExecChannel>> {
            anyhow::bail!("can't reach sandbox")
        }
        fn destroy(&self, _resolved: &Pillbox) -> Result<Destroyed> {
            Ok(Destroyed::RecordDropped)
        }
    }

    #[test]
    fn exec_json_emits_nothing_when_the_target_refuses_before_the_command_starts() {
        let collected = Rc::new(RefCell::new(Vec::new()));
        let emitter = EventEmitter::new(
            Box::new(CollectSink(collected.clone())),
            "sb-test".into(),
            Correlation::Exec("ex1".into()),
        );
        assert!(emit_exec_events(emitter, &RefusingTarget, &["sh".into()]).is_err());
        assert!(
            collected.borrow().is_empty(),
            "exec_started before a refusal"
        );
    }

    #[test]
    fn exec_json_emits_started_output_per_stream_and_exit_code() {
        let collected = Rc::new(RefCell::new(Vec::new()));
        let emitter = EventEmitter::new(
            Box::new(CollectSink(collected.clone())),
            "sb-test".into(),
            Correlation::Exec("ex1".into()),
        );
        let target = FakeTarget(vec![(false, b"out\n"), (true, b"err\n")], 3);
        emit_exec_events(emitter, &target, &["sh".into()]).unwrap();
        let events = collected.borrow();
        let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
        match &events[..] {
            [Event {
                payload: Payload::ExecStarted(_),
                ..
            }, Event {
                payload: Payload::ExecOutput(o),
                ..
            }, Event {
                payload: Payload::ExecOutput(e),
                ..
            }, Event {
                payload: Payload::ExecExit(x),
                ..
            }] => {
                assert_eq!(
                    (o.stream, o.data.clone()),
                    (StdStream::Stdout, b64(b"out\n"))
                );
                assert_eq!(
                    (e.stream, e.data.clone()),
                    (StdStream::Stderr, b64(b"err\n"))
                );
                assert_eq!(x.code, 3);
            }
            other => panic!("unexpected events: {}", other.len()),
        }
    }

    fn claude_turn(lines: &[&'static [u8]], harness_code: i32) -> (i32, Option<i32>) {
        let collected = Rc::new(RefCell::new(Vec::new()));
        let driver = AgentDriver::new(
            Box::new(ClaudeAdapter::default()),
            Box::new(CollectSink(collected)),
            "sb-test".into(),
        );
        let target = FakeTarget(lines.iter().map(|l| (false, *l)).collect(), harness_code);
        run_agent_turn(&target, driver, &["claude".into()]).unwrap()
    }

    #[test]
    fn agent_turn_reports_run_finished_and_the_harness_exit_code() {
        let ok: &[u8] = b"{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"pong\"}\n";
        let failed: &[u8] = b"{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":true,\"result\":\"api error\"}\n";
        assert_eq!(claude_turn(&[ok], 0), (0, Some(0)));
        assert_eq!(claude_turn(&[failed], 0), (0, Some(1)));
        assert_eq!(claude_turn(&[], 2), (2, None));
    }

    #[test]
    fn agent_exit_code_is_zero_in_json_only_when_run_finished_carries_the_outcome() {
        // A clean turn exits 0 in both modes.
        assert_eq!(agent_exit_code(false, 0, Some(0)), 0);
        assert_eq!(agent_exit_code(true, 0, Some(0)), 0);
        // A failure run_finished reports: plain mirrors it, --json leaves it in the events.
        assert_eq!(agent_exit_code(false, 0, Some(1)), 1);
        assert_eq!(agent_exit_code(true, 0, Some(1)), 0);
        assert_eq!(agent_exit_code(true, 3, Some(3)), 0);
        // A failure the events don't carry exits non-zero in both modes.
        assert_eq!(agent_exit_code(true, 3, Some(0)), 3);
        assert_eq!(agent_exit_code(true, 3, None), 3);
        assert_eq!(agent_exit_code(false, 3, None), 3);
        // No run_finished at all: the turn never finished.
        assert_eq!(agent_exit_code(true, 0, None), 1);
        assert_eq!(agent_exit_code(false, 0, None), 1);
    }

    #[test]
    fn spawn_rejects_image_before_touching_a_backend() {
        let err = spawn(
            &crate::pillbox::global(),
            Some("my:image".into()),
            None,
            None,
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("--image my:image"), "got: {err}");
    }

    #[test]
    fn list_json_reports_the_workspace_clone() {
        let mut s = Sandbox {
            id: "abcdef012345".into(),
            backend: crate::sandboxes::BACKEND_LIBKRUN.into(),
            backend_ref: "{}".into(),
            image: "pillbox:latest".into(),
            agent: None,
            workspace: "/work/app".into(),
            workspace_clone: Some("/k/ws/1".into()),
            guest_workspace: Some("/workspace/app".into()),
            label: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            status: "ready".into(),
        };
        assert_eq!(sandbox_json(&s)["workspace_clone"], "/k/ws/1");
        s.workspace_clone = None;
        assert!(sandbox_json(&s).get("workspace_clone").is_none());
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
