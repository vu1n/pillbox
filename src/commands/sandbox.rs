//! `pillbox sandbox …` handlers — spawn a long-lived container, `exec`
//! commands into it over the PTY-free contract, then tear it down.
//!
//! This is the first producer of the agent I/O contract ([`crate::contract`]):
//! the exec channel. `exec --json` emits `ExecStarted`/`ExecOutput`/`ExecExit`
//! as JSONL on stdout (the structured primitive a consumer reads); plain
//! `exec` passes raw bytes through to the terminal and mirrors the exit code.

use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::Result;
use base64::Engine as _;

use crate::agents::{workspace_mount_name, GUEST_HOME, GUEST_WORKSPACE};
use crate::cli::SandboxAction;
use crate::contract::{
    Event, EventSink, ExecExit, ExecOutput, ExecStarted, JsonlSink, Payload, StdStream,
};
use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::sandboxes::{self, Sandbox, BACKEND_DOCKER};
use crate::{docker, session};

pub(crate) fn dispatch(resolved: &Pillbox, action: SandboxAction) -> Result<()> {
    match action {
        SandboxAction::Spawn {
            image,
            workspace,
            label,
        } => spawn(resolved, image, workspace, label),
        SandboxAction::Exec { id, json, argv } => exec(resolved, &id, json, argv),
        SandboxAction::Destroy { id } => destroy(resolved, &id),
        SandboxAction::List { json } => list(resolved, json),
    }
}

fn spawn(
    resolved: &Pillbox,
    image: Option<String>,
    workspace: Option<PathBuf>,
    label: Option<String>,
) -> Result<()> {
    docker::check_ready()?;
    let image = image.unwrap_or_else(|| docker::RUNNER_IMAGE.to_string());
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
    let args = vec![
        "-d".into(),
        "--add-host".into(),
        "host.docker.internal:host-gateway".into(),
        "--label".into(),
        format!("pillbox.sandbox={id}"),
        "-e".into(),
        format!("HOME={GUEST_HOME}"),
        "-e".into(),
        format!("PATH=/usr/local/bin:/usr/bin:/bin:{GUEST_HOME}/.local/bin"),
        "-v".into(),
        format!("{}:{guest_workspace}", workspace_host.display()),
        "-w".into(),
        guest_workspace,
        image.clone(),
        "sleep".into(),
        "infinity".into(),
    ];
    let backend_ref = docker::run_detached(&args)?;

    let record = Sandbox {
        id: id.clone(),
        backend: BACKEND_DOCKER.into(),
        backend_ref,
        image,
        workspace: workspace_host.display().to_string(),
        label,
        created_at: session::now_rfc3339(),
        status: "ready".into(),
    };
    sandboxes::write(resolved, &record)?;
    eprintln!("pillbox: ✓ sandbox {id} ready");
    println!("{id}"); // stdout: the id, for `ID=$(pillbox sandbox spawn)`
    Ok(())
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
    let stdout = io::stdout();
    let mut sink = JsonlSink::new(stdout.lock());
    // Distinct from the sandbox id so a consumer can correlate concurrent
    // execs on the same sandbox.
    let exec_id = crate::registry::new_id();
    let mut seq = 0u64;
    let mut emit = |sink: &mut JsonlSink<_>, payload| {
        seq += 1;
        sink.emit(&Event::durable(seq, &record.id, payload).with_exec(&exec_id))
    };

    emit(
        &mut sink,
        Payload::ExecStarted(ExecStarted {
            argv: argv.to_vec(),
        }),
    )?;
    let code = docker::exec_streamed(&record.backend_ref, argv, |is_stderr, bytes| {
        let stream = if is_stderr {
            StdStream::Stderr
        } else {
            StdStream::Stdout
        };
        emit(
            &mut sink,
            Payload::ExecOutput(ExecOutput {
                stream,
                data: base64::engine::general_purpose::STANDARD.encode(bytes),
            }),
        )
    })?;
    emit(&mut sink, Payload::ExecExit(ExecExit { code }))?;
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
