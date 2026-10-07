//! `pillbox sandbox …` handlers — spawn a long-lived container, then drive it
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
//!   refused here; they run on the libkrun backend via `pillbox run`.

use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::Result;
use base64::Engine as _;

use crate::agents::harness::{self, HarnessAdapter};
use crate::agents::{workspace_mount_name, GUEST_HOME, GUEST_WORKSPACE};
use crate::cli::SandboxAction;
use crate::contract::{
    Correlation, Event, EventEmitter, EventSink, ExecExit, ExecOutput, ExecStarted, JsonlSink,
    Payload, StdStream, ToolStatus,
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
