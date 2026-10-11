//! `grok_build` text driver for `pillbox.text/2`.
//!
//! Grok Build is xAI's `grok` CLI (`xai-org/grok-build`, headless
//! `--output-format streaming-json`). This module is the harness-agnostic
//! protocol: catalog, credential release, tool-free argv, and the NDJSON
//! interpreter. The microVM launch lives in `sandbox::libkrun::repository`
//! and is called from [`run`], which compiles only with the libkrun feature.
//! Linux CI runs the protocol tests here without booting a VM.

use anyhow::{bail, ensure, Result};
use serde_json::{json, Value};

use super::usage::TurnUsage;

/// Reported on every grok_build `resolved` block. Matches `text_v2::ADAPTER_REVISION`.
pub(crate) const ADAPTER_REVISION: &str = "pillbox/local-text-v2";
pub(crate) const HARNESS: &str = "grok_build";
pub(crate) const CREDENTIAL_REF: &str = "pillbox:grok_build:default";
pub(crate) const HOST: &str = "api.x.ai";
pub(crate) const SECRET_NAME: &str = "XAI_API_KEY";
pub(crate) const STUB_PREFIX: &str = "xai-";
/// Guest path of the prompt file. The prompt is never an argv element.
pub(crate) const PROMPT_PATH: &str = "/opt/pillbox-execution/prompt.txt";
const GROK_BIN: &str = "/usr/local/bin/grok";
const MAX_VERSION_BYTES: usize = 128;

/// Closed catalog from grok-build `default_models.json` at source revision
/// `559751fd` (CLI crate 1.0.45, the public tree closest to the 1.0.46 pin).
/// A model the pinned CLI might add later is rejected here until this list
/// is updated. Resolve must not boot a VM to ask `grok models`.
const CATALOG: &[(&str, &[&str])] = &[
    ("grok-4.6", &["xhigh", "high", "medium", "low"]),
    ("grok-4.5", &["high", "medium", "low"]),
];

/// Short tool ids from the 1.0.45 toolset, plus bare `Agent` (blocks subagent
/// spawn). `--tools` is not a denylist: an unknown allowlist name keeps the
/// full toolset, and an empty `--tools` means no restriction. The denylist
/// runs first and wins. The driver also fails the turn if a `tool_call`
/// event arrives.
const DISALLOWED_TOOLS: &str = "Agent,ask_user_question,bash,deploy_app,edit,enter_plan_mode,\
exit_plan_mode,get_task_output,get_terminal_command_output,glob,grep,image_edit,image_gen,\
image_to_video,init_or_update_app,kill_task,kill_terminal_command,list_dir,lsp,memory_get,\
memory_search,monitor,read,read_file,reference_to_video,run_terminal_cmd,scheduler_create,\
scheduler_delete,scheduler_list,search_replace,search_tool,send_feedback,send_subagent_message,\
task,todo_write,todowrite,update_goal,use_tool,wait_tasks,web_fetch,web_search,workflow,write";

/// What a selection resolves to before a VM starts. The CLI version is not
/// here: the guest observes it with `grok --version`.
pub(crate) struct Lowered {
    pub(crate) runner_image_id: String,
    pub(crate) requested_model: String,
    pub(crate) reasoning_effort: String,
    pub(crate) credential_ref: &'static str,
    pub(crate) network_hosts: Vec<String>,
    pub(crate) argv: Vec<String>,
}

/// Stub placed in the guest, and the real key released only on the VMM
/// child's stdin. No Debug: both fields are credential material.
pub(crate) struct ReleasedKey {
    pub(crate) stub: String,
    pub(crate) real: String,
}

impl std::fmt::Debug for ReleasedKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReleasedKey")
            .field("stub", &self.stub)
            .field("real", &"<redacted>")
            .finish()
    }
}

/// A completed fake or live turn.
#[derive(Debug)]
pub(crate) struct TurnObservation {
    pub(crate) harness_version: String,
    pub(crate) output_text: String,
    pub(crate) served_model: Option<String>,
    pub(crate) usage: Option<TurnUsage>,
}

/// A turn the interpreter refused. `message` is session evidence only.
#[derive(Debug)]
pub(crate) struct TurnFault {
    pub(crate) message: String,
    pub(crate) usage: Option<TurnUsage>,
}

/// Reject a model or effort the embedded catalog cannot serve.
pub(crate) fn resolve_model(model: &str, effort: &str) -> Result<()> {
    let Some((_, efforts)) = CATALOG.iter().find(|(id, _)| *id == model) else {
        bail!("unsupported grok_build model");
    };
    ensure!(
        efforts.contains(&effort),
        "unsupported grok_build reasoning effort"
    );
    Ok(())
}

/// Lower a servable selection onto an immutable runner image id.
pub(crate) fn lower(model: &str, effort: &str, runner_image_id: &str) -> Result<Lowered> {
    resolve_model(model, effort)?;
    ensure!(
        super::valid_digest(runner_image_id),
        "runner image id is not an immutable digest"
    );
    Ok(Lowered {
        runner_image_id: runner_image_id.to_owned(),
        requested_model: model.to_owned(),
        reasoning_effort: effort.to_owned(),
        credential_ref: CREDENTIAL_REF,
        network_hosts: vec![HOST.to_owned()],
        argv: text_argv(model, effort),
    })
}

/// Headless argv. One final answer, no tools, prompt from a file.
pub(crate) fn text_argv(model: &str, effort: &str) -> Vec<String> {
    vec![
        GROK_BIN.to_owned(),
        "--no-auto-update".to_owned(),
        "--prompt-file".to_owned(),
        PROMPT_PATH.to_owned(),
        "-m".to_owned(),
        model.to_owned(),
        "--effort".to_owned(),
        effort.to_owned(),
        "--output-format".to_owned(),
        "streaming-json".to_owned(),
        "--disallowed-tools".to_owned(),
        DISALLOWED_TOOLS.to_owned(),
        "--disable-web-search".to_owned(),
        "--no-memory".to_owned(),
        "--no-subagents".to_owned(),
        "--no-plan".to_owned(),
        "--max-turns".to_owned(),
        "1".to_owned(),
        "--verbatim".to_owned(),
        "--cwd".to_owned(),
        "/workspace".to_owned(),
    ]
}

/// Load `XAI_API_KEY` and mint a stub. The real key is never copied into the
/// error string. The secret must be vaulted for `api.x.ai` with bearer auth
/// so the text VM can bind the swap to that host.
pub(crate) fn load_credential(
    pb: &crate::pillbox::Pillbox,
    invocation_id: &str,
) -> Result<ReleasedKey> {
    let Some(raw) = crate::secrets::read(pb, SECRET_NAME)? else {
        bail!("{SECRET_NAME} is not in the secret store");
    };
    let real = raw.trim().to_owned();
    if real.is_empty() || real.len() > 64 * 1024 || real.bytes().any(|b| b < 0x20) {
        bail!("{SECRET_NAME} is empty or not a single-line credential");
    }
    let Some(meta) = crate::secrets::read_meta(pb, SECRET_NAME)? else {
        bail!("{SECRET_NAME} is not vaulted for {HOST}");
    };
    if meta.vault.host != HOST
        || meta.vault.header_scheme != crate::vault::HeaderScheme::AuthorizationBearer
    {
        bail!("{SECRET_NAME} is not bound to {HOST} with authorization-bearer");
    }
    let mut stub = String::new();
    for _ in 0..4 {
        let candidate = crate::vault::providers::mint_stub(STUB_PREFIX, invocation_id);
        if candidate != real && !candidate.contains(&real) && !real.contains(&candidate) {
            stub = candidate;
            break;
        }
    }
    if stub.is_empty() {
        bail!("could not mint a credential stub");
    }
    Ok(ReleasedKey { stub, real })
}

/// Interpret one grok streaming-json capture plus the bridge's version line.
/// Tool calls, empty text, a non-`end_turn` stop, and text over the byte cap
/// all fail. Nothing is sliced or padded.
pub(crate) fn interpret(
    stdout: &str,
    max_final_text_bytes: u64,
) -> Result<TurnObservation, TurnFault> {
    let mut version: Option<String> = None;
    let mut version_count = 0u32;
    let mut text = String::new();
    let mut saw_tool = false;
    let mut error_message: Option<String> = None;
    let mut error_value: Option<Value> = None;
    let mut end: Option<Value> = None;
    let mut end_count = 0u32;

    for (index, raw) in stdout.split('\n').enumerate() {
        let raw = raw.trim_end_matches('\r');
        if raw.is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(raw) {
            Ok(value) => value,
            Err(_) => {
                return Err(TurnFault {
                    message: format!("turn line {} is not json", index + 1),
                    usage: None,
                });
            }
        };
        match value.get("type").and_then(Value::as_str).unwrap_or("") {
            "pillbox.harness_version" => {
                version_count += 1;
                version = value
                    .get("version")
                    .and_then(Value::as_str)
                    .map(|version| version.trim().to_owned());
            }
            "text" => {
                if let Some(data) = value.get("data").and_then(Value::as_str) {
                    text.push_str(data);
                }
            }
            "tool_call" | "tool_call_update" => saw_tool = true,
            "error" => {
                error_message = Some(
                    value
                        .get("message")
                        .and_then(Value::as_str)
                        .filter(|message| !message.is_empty())
                        .unwrap_or("harness error")
                        .to_owned(),
                );
                error_value = Some(value.clone());
            }
            "end" => {
                end_count += 1;
                end = Some(value.clone());
            }
            _ => {}
        }
    }

    let usage = end
        .as_ref()
        .and_then(TurnUsage::from_grok_end)
        .or_else(|| error_value.as_ref().and_then(TurnUsage::from_grok_end));
    let fail = |message: String| TurnFault {
        message,
        usage: usage.clone(),
    };

    if saw_tool {
        return Err(fail("harness attempted a tool call".to_owned()));
    }
    if let Some(message) = error_message {
        return Err(fail(message));
    }
    let version = match (version_count, version) {
        (1, Some(version))
            if !version.is_empty()
                && version.len() <= MAX_VERSION_BYTES
                && version.bytes().all(|byte| (0x20..0x7f).contains(&byte)) =>
        {
            version
        }
        _ => return Err(fail("harness version was not observed".to_owned())),
    };
    if end_count != 1 {
        return Err(fail("harness ended without a single end event".to_owned()));
    }
    let Some(end) = end else {
        return Err(fail("harness ended without a single end event".to_owned()));
    };
    if end.get("stopReason").and_then(Value::as_str) != Some("end_turn") {
        return Err(fail("harness stop reason was not end_turn".to_owned()));
    }
    if text.is_empty() {
        return Err(fail("harness returned empty text".to_owned()));
    }
    if text.len() as u64 > max_final_text_bytes {
        return Err(fail(
            "harness text exceeded max_final_text_bytes".to_owned(),
        ));
    }
    Ok(TurnObservation {
        harness_version: version,
        output_text: text,
        served_model: served_model(&end),
        usage: usage.clone(),
    })
}

fn served_model(end: &Value) -> Option<String> {
    let keys: Vec<&str> = end
        .get("modelUsage")?
        .as_object()?
        .keys()
        .map(String::as_str)
        .filter(|key| !key.is_empty())
        .collect();
    match keys.as_slice() {
        [key] if *key != "unknown" => Some((*key).to_owned()),
        _ => None,
    }
}

pub(crate) fn completion_record(
    invocation_id: &str,
    request_hash: &str,
    runner_image_id: &str,
    requested_model: &str,
    observed: &TurnObservation,
    session_ref: &Value,
) -> Value {
    let mut detail = json!({
        "invocation_id": invocation_id,
        "request_hash": request_hash,
        "resolved": {
            "harness": HARNESS,
            "harness_version": observed.harness_version,
            "adapter_revision": ADAPTER_REVISION,
            "runner_image_id": runner_image_id,
            "requested_model": requested_model,
            "served_model": observed.served_model,
        },
        "session_ref": session_ref,
        "output_text": observed.output_text,
    });
    if let Some(usage) = &observed.usage {
        detail["usage"] = json!(usage);
    }
    detail
}

pub(crate) fn failure_record(
    invocation_id: &str,
    request_hash: &str,
    code: &str,
    stage: &str,
    session_ref: &Value,
    usage: Option<&TurnUsage>,
) -> Value {
    let mut detail = json!({
        "invocation_id": invocation_id,
        "request_hash": request_hash,
        "code": code,
        "stage": stage,
        "session_ref": session_ref,
    });
    if let Some(usage) = usage {
        detail["usage"] = json!(usage);
    }
    detail
}

#[cfg(feature = "libkrun")]
pub(crate) fn run(
    pb: &crate::pillbox::Pillbox,
    request: &super::text_v2::TextRequestV2,
    owner: &mut super::store::OwnedInvocation,
    lowered: Lowered,
) -> Result<super::text_v2::Outcome> {
    use std::time::{Duration, Instant};

    use serde_json::json;

    use crate::contract::{Custom, Payload};
    use crate::execution::evidence::ExecutionEvidence;
    use crate::sandbox::libkrun::repository::{self, GrokInput, TeardownUnconfirmed, VmLimits};

    use super::text_v2::Outcome;

    const STAGES: [&str; 6] = [
        "credentials",
        "image_prepare",
        "guest_prepare",
        "vmm_spawn",
        "guest_rpc_ready",
        "turn",
    ];

    struct Stages {
        timer: crate::startup::StartupTimer,
        completed: usize,
        started_emitted: bool,
    }

    impl Stages {
        fn start() -> Self {
            Self {
                timer: crate::startup::StartupTimer::start(),
                completed: 0,
                started_emitted: false,
            }
        }

        fn in_progress(&self) -> &'static str {
            STAGES.get(self.completed).copied().unwrap_or("finalize")
        }

        fn complete(&mut self, name: &'static str, evidence: &mut ExecutionEvidence) -> Result<()> {
            self.timer.mark(name);
            self.completed += 1;
            let duration_ms = self
                .timer
                .snapshot()
                .stages
                .last()
                .map(|stage| stage.duration_ms);
            evidence.append(Payload::Custom(Custom {
                name: "text.stage.completed".into(),
                payload: Some(json!({"stage": name, "duration_ms": duration_ms})),
            }))?;
            Ok(())
        }

        fn emit_started(&mut self, pb: &crate::pillbox::Pillbox, session_id: &str) {
            if self.started_emitted {
                return;
            }
            self.started_emitted = true;
            crate::events::emit_session_event(
                pb,
                crate::events::EventType::SessionStarted {
                    parent_session_id: crate::events::parent_session_id_from_env(),
                    startup: Some(self.timer.snapshot()),
                },
                session_id,
                None,
            );
        }
    }

    let deadline = Instant::now() + Duration::from_millis(request.limits.timeout_ms);
    let mut evidence = ExecutionEvidence::start_text(
        pb,
        &request.session_ref.session_id,
        &request.invocation_id,
        json!({
            "request_hash": owner.record().request_hash,
            "adapter_revision": ADAPTER_REVISION,
            "runner_image_id": lowered.runner_image_id,
            "requested_model": lowered.requested_model,
            "reasoning_effort": lowered.reasoning_effort,
            "credential_ref": lowered.credential_ref,
            "network_hosts": lowered.network_hosts,
        }),
    )?;
    owner.running(json!({
        "runner_image_id": lowered.runner_image_id,
        "requested_model": lowered.requested_model,
        "session_ref": evidence.reference(),
    }))?;
    let mut stages = Stages::start();
    let session_id = request.session_ref.session_id.clone();

    let released = match load_credential(pb, &request.invocation_id) {
        Ok(released) => released,
        Err(error) => {
            return seal(
                pb,
                request,
                owner,
                &mut evidence,
                stages.in_progress(),
                error,
                None,
            );
        }
    };
    stages.complete("credentials", &mut evidence)?;
    let remaining = match deadline.checked_duration_since(Instant::now()) {
        Some(remaining) if !remaining.is_zero() => remaining,
        _ => {
            return seal(
                pb,
                request,
                owner,
                &mut evidence,
                stages.in_progress(),
                anyhow::anyhow!("bounded execution deadline exceeded"),
                None,
            );
        }
    };
    let input = GrokInput {
        image_id: lowered.runner_image_id.clone(),
        argv: lowered.argv,
        prompt: request.rendered_input.clone(),
        stub: released.stub,
        real: released.real,
    };
    let cancelled = || !matches!(owner.cancelled(), Ok(false));
    let mut stage_error = None;
    let launched = repository::launch_grok_text(
        input,
        VmLimits {
            max_duration: remaining,
            max_output_bytes: request.limits.max_evidence_bytes,
            max_frame_bytes: request.limits.max_frame_bytes as usize,
        },
        &cancelled,
        &mut |name| {
            if let Err(error) = stages.complete(name, &mut evidence) {
                stage_error.get_or_insert(error);
            }
        },
    );
    let mut vm = match launched {
        Ok(vm) => vm,
        Err(error) if error.downcast_ref::<TeardownUnconfirmed>().is_some() => return Err(error),
        Err(error) => {
            return seal(
                pb,
                request,
                owner,
                &mut evidence,
                stages.in_progress(),
                error,
                None,
            );
        }
    };
    stages.emit_started(pb, &session_id);
    let read = (|| -> Result<String> {
        if let Some(error) = stage_error {
            return Err(error.context("persist text stage evidence"));
        }
        let mut stream = vm.connect_rpc(&cancelled)?;
        stages.complete("guest_rpc_ready", &mut evidence)?;
        read_guest_stdout(
            &mut stream,
            deadline,
            request.limits.max_evidence_bytes,
            request.limits.max_frame_bytes as usize,
            &cancelled,
        )
    })();
    let diagnostics = vm.diagnostics();
    if let Err(error) = vm.stop_and_reap() {
        return Err(match read {
            Ok(_) => error,
            Err(read_error) => error.context(format!("turn ended with: {read_error:#}")),
        });
    }
    let stdout = match read {
        Ok(stdout) => stdout,
        Err(error) => {
            return seal(
                pb,
                request,
                owner,
                &mut evidence,
                stages.in_progress(),
                error,
                None,
            );
        }
    };
    if let Ok(bytes) = diagnostics {
        let artifact = evidence.artifact(&bytes, "text/plain")?;
        evidence.append(Payload::Custom(Custom {
            name: "text.builder.stopped".into(),
            payload: Some(json!({"diagnostics": artifact})),
        }))?;
    }
    let observed = match interpret(&stdout, request.limits.max_final_text_bytes) {
        Ok(observed) => observed,
        Err(fault) => {
            return seal(
                pb,
                request,
                owner,
                &mut evidence,
                stages.in_progress(),
                anyhow::anyhow!(fault.message),
                fault.usage,
            );
        }
    };
    stages.complete("turn", &mut evidence)?;
    if let Some(usage) = &observed.usage {
        evidence.append(Payload::Custom(Custom {
            name: "usage".into(),
            payload: Some(json!({"turn_usage": usage})),
        }))?;
    }
    let capture = evidence.artifact(stdout.as_bytes(), "application/x-ndjson")?;
    evidence.append(Payload::Custom(Custom {
        name: "text.result.captured".into(),
        payload: Some(json!({"capture": capture, "output_text_bytes": observed.output_text.len()})),
    }))?;
    let session_ref = serde_json::to_value(evidence.reference())?;
    owner.observe(json!({
        "session_ref": session_ref,
        "runner_image_id": lowered.runner_image_id,
        "requested_model": lowered.requested_model,
    }))?;
    crate::events::emit_session_event(
        pb,
        crate::events::EventType::SessionCompleted {
            exit_code: Some(0),
            trace_path: None,
            result_snapshot: None,
        },
        &session_id,
        None,
    );
    Ok(Outcome::Completed(completion_record(
        &request.invocation_id,
        &owner.record().request_hash,
        &lowered.runner_image_id,
        &lowered.requested_model,
        &observed,
        &session_ref,
    )))
}

#[cfg(feature = "libkrun")]
fn seal(
    pb: &crate::pillbox::Pillbox,
    request: &super::text_v2::TextRequestV2,
    owner: &mut super::store::OwnedInvocation,
    evidence: &mut crate::execution::evidence::ExecutionEvidence,
    stage: &'static str,
    error: anyhow::Error,
    usage: Option<TurnUsage>,
) -> Result<super::text_v2::Outcome> {
    use crate::contract::{Custom, Payload};
    use crate::sandbox::libkrun::repository::TeardownUnconfirmed;

    let recorded = evidence.append(Payload::Custom(Custom {
        name: "text.execution.failed".into(),
        payload: Some(json!({"error": format!("{error:#}"), "stage": stage})),
    }));
    crate::events::emit_session_event(
        pb,
        crate::events::EventType::SessionFailed {
            reason: format!("{stage}: {error:#}"),
            exit_code: Some(1),
            trace_path: None,
            result_snapshot: None,
        },
        &request.session_ref.session_id,
        None,
    );
    if error.downcast_ref::<TeardownUnconfirmed>().is_some() {
        return Err(error);
    }
    if let Err(persist) = recorded {
        return Err(error.context(format!("failure evidence was not persisted: {persist:#}")));
    }
    let session_ref = serde_json::to_value(evidence.reference())?;
    let _ = owner.observe(json!({
        "session_ref": session_ref,
        "stage": stage,
    }));
    Ok(super::text_v2::Outcome::Failed(failure_record(
        &request.invocation_id,
        &owner.record().request_hash,
        super::text_v2::failure_code(stage, &error),
        stage,
        &session_ref,
        usage.as_ref(),
    )))
}

#[cfg(feature = "libkrun")]
fn read_guest_stdout(
    stream: &mut std::os::unix::net::UnixStream,
    deadline: std::time::Instant,
    max_bytes: u64,
    max_frame: usize,
    cancelled: &dyn Fn() -> bool,
) -> Result<String> {
    use anyhow::Context;
    use std::io::{ErrorKind, Read};

    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut line_start = 0usize;
    loop {
        if cancelled() {
            bail!("text invocation cancelled");
        }
        if std::time::Instant::now() >= deadline {
            bail!("bounded execution deadline exceeded");
        }
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() as u64 + n as u64 > max_bytes {
                    bail!("turn output limit");
                }
                buf.extend_from_slice(&chunk[..n]);
                while let Some(rel) = buf[line_start..].iter().position(|byte| *byte == b'\n') {
                    let end = line_start + rel;
                    if end - line_start > max_frame {
                        bail!("turn frame limit");
                    }
                    line_start = end + 1;
                }
                if buf.len() - line_start > max_frame {
                    bail!("turn frame limit");
                }
            }
            Err(error)
                if error.kind() == ErrorKind::WouldBlock
                    || error.kind() == ErrorKind::TimedOut
                    || error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(error).context("read guest turn"),
        }
    }
    String::from_utf8(buf).context("guest turn was not utf-8")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pillbox::{Pillbox, Scope};
    use crate::vault::{HeaderScheme, VaultMeta};

    fn image() -> String {
        format!("sha256:{}", "b".repeat(64))
    }

    fn session_ref() -> Value {
        json!({"session_id": "chat_1", "seq_range": [0, 1]})
    }

    fn version_line(version: &str) -> String {
        json!({"type": "pillbox.harness_version", "version": version}).to_string()
    }

    fn end_line(stop: &str, extra: Value) -> String {
        let mut end = json!({"type": "end", "stopReason": stop});
        if let (Some(obj), Some(extra)) = (end.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                obj.insert(key.clone(), value.clone());
            }
        }
        end.to_string()
    }

    fn temp_pillbox() -> (tempfile::TempDir, Pillbox) {
        let dir = tempfile::tempdir().unwrap();
        let pb = Pillbox {
            scope: Scope::Global,
            state_dir: dir.path().join("global"),
            meta: None,
        };
        (dir, pb)
    }

    fn write_secret(pb: &Pillbox, value: impl AsRef<str>, meta: Option<VaultMeta>) {
        let dir = pb.state_dir.join("secrets");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(SECRET_NAME), value.as_ref()).unwrap();
        if let Some(meta) = meta {
            std::fs::write(
                dir.join(format!("{SECRET_NAME}.meta.json")),
                serde_json::to_string(&meta).unwrap(),
            )
            .unwrap();
        }
    }

    #[test]
    fn grok_build_selection_lowers_onto_the_resolved_image() {
        let lowered = lower("grok-4.6", "low", &image()).unwrap();
        assert_eq!(lowered.runner_image_id, image());
        assert_eq!(lowered.requested_model, "grok-4.6");
        assert_eq!(lowered.reasoning_effort, "low");
        assert_eq!(lowered.credential_ref, CREDENTIAL_REF);
        assert_eq!(lowered.network_hosts, [HOST]);
        assert!(lowered
            .argv
            .windows(2)
            .any(|pair| pair == ["-m", "grok-4.6"]));
        assert!(lowered
            .argv
            .windows(2)
            .any(|pair| pair == ["--effort", "low"]));
        assert_eq!(
            lower("grok-4.5", "high", &image()).unwrap().requested_model,
            "grok-4.5"
        );
    }

    #[test]
    fn unservable_model_is_rejected_at_resolve() {
        for (model, effort) in [
            ("grok-4.7", "low"),
            ("gpt-6-luna", "low"),
            ("grok-build", "low"),
            ("grok-4.5", "xhigh"),
            ("grok-4.6", "max"),
            ("", "low"),
        ] {
            let error = resolve_model(model, effort).unwrap_err();
            assert!(
                error.to_string().starts_with("unsupported grok_build"),
                "{model}/{effort}: {error}"
            );
            assert!(lower(model, effort, &image()).is_err());
        }
    }

    #[test]
    fn text_argv_denies_tools_and_keeps_the_prompt_out_of_argv() {
        let argv = text_argv("grok-4.6", "low");
        let denied = argv
            .windows(2)
            .find(|pair| pair[0] == "--disallowed-tools")
            .unwrap();
        for name in [
            "Agent",
            "read_file",
            "run_terminal_cmd",
            "search_tool",
            "use_tool",
            "web_search",
            "write",
            "task",
        ] {
            assert!(
                denied[1].split(',').any(|entry| entry == name),
                "missing {name}"
            );
        }
        assert!(!argv
            .iter()
            .any(|arg| arg == "--tools" || arg == "--yolo" || arg == "--always-approve"));
        assert!(argv.iter().any(|arg| arg == "--disable-web-search"));
        assert!(argv.iter().any(|arg| arg == "--no-memory"));
        assert!(argv.iter().any(|arg| arg == "--no-subagents"));
        assert!(argv.iter().any(|arg| arg == "--no-plan"));
        assert!(argv.windows(2).any(|pair| pair == ["--max-turns", "1"]));
        assert!(argv
            .windows(2)
            .any(|pair| pair == ["--prompt-file", PROMPT_PATH]));
        assert!(!argv.iter().any(|arg| arg.contains("ignore previous")));
    }

    #[test]
    fn completed_turn_reports_resolved_and_usage() {
        let stdout = format!(
            "{}\n{}\n{}\n{}\n{}\n",
            version_line("grok 1.0.46"),
            json!({"type": "thought", "data": "hmm"}),
            json!({"type": "text", "data": "Hello "}),
            json!({"type": "text", "data": "world"}),
            end_line(
                "end_turn",
                json!({
                    "modelUsage": {"grok-4.6": {"costUSD": 9.0, "inputTokens": 1}},
                    "usage": {
                        "input_tokens": 10,
                        "output_tokens": 4,
                        "cache_read_input_tokens": 2,
                        "cache_creation_input_tokens": 1,
                        "reasoning_tokens": 9
                    },
                    "total_cost_usd": 0.25
                })
            ),
        );
        let observed = interpret(&stdout, 32_768).unwrap();
        assert_eq!(observed.harness_version, "grok 1.0.46");
        assert_eq!(observed.output_text, "Hello world");
        assert_eq!(observed.served_model.as_deref(), Some("grok-4.6"));
        let detail = completion_record(
            "chat_1",
            "sha256:ab",
            &image(),
            "grok-4.6",
            &observed,
            &session_ref(),
        );
        assert_eq!(detail["resolved"]["harness"], "grok_build");
        assert_eq!(detail["resolved"]["harness_version"], "grok 1.0.46");
        assert_eq!(detail["resolved"]["adapter_revision"], ADAPTER_REVISION);
        assert_eq!(detail["resolved"]["runner_image_id"], image());
        assert_eq!(detail["resolved"]["requested_model"], "grok-4.6");
        assert_eq!(detail["resolved"]["served_model"], "grok-4.6");
        assert_eq!(detail["output_text"], "Hello world");
        assert_eq!(
            detail["usage"],
            json!({
                "cost_usd": 0.25,
                "input_tokens": 10,
                "output_tokens": 4,
                "cache_read_tokens": 2,
                "cache_write_tokens": 1
            })
        );
        assert!(detail.get("error").is_none());
        let serialized = detail.to_string();
        assert!(!serialized.contains("reasoning_tokens"));
        assert!(!serialized.contains("\"9\""));
    }

    #[test]
    fn served_model_is_null_when_the_harness_does_not_name_one() {
        for extra in [
            json!({}),
            json!({"modelUsage": {}}),
            json!({"modelUsage": {"unknown": {}}}),
            json!({"modelUsage": {"grok-4.6": {}, "grok-4.5": {}}}),
        ] {
            let stdout = format!(
                "{}\n{}\n{}\n",
                version_line("1.0.46"),
                json!({"type": "text", "data": "ok"}),
                end_line("end_turn", extra),
            );
            let observed = interpret(&stdout, 32).unwrap();
            assert_eq!(observed.served_model, None);
            assert_eq!(observed.usage, None);
        }
    }

    #[test]
    fn a_tool_attempt_fails_the_turn() {
        let stdout = format!(
            "{}\n{}\n{}\n{}\n",
            version_line("1.0.46"),
            json!({"type": "text", "data": "I will read it"}),
            json!({"type": "tool_call", "toolName": "read_file", "toolCallId": "c1"}),
            end_line(
                "end_turn",
                json!({
                    "usage": {"input_tokens": 3, "output_tokens": 1},
                    "total_cost_usd": 0.01
                })
            ),
        );
        let fault = interpret(&stdout, 32_768).unwrap_err();
        assert!(fault.message.contains("tool"));
        let detail = failure_record(
            "chat_1",
            "sha256:ab",
            "runtime_protocol_error",
            "turn",
            &session_ref(),
            fault.usage.as_ref(),
        );
        assert_eq!(detail["code"], "runtime_protocol_error");
        assert_eq!(detail["stage"], "turn");
        assert!(detail.get("output_text").is_none());
        assert!(detail.get("error").is_none());
        assert!(!detail.to_string().contains("read_file"));
        assert_eq!(detail["usage"]["input_tokens"], 3);
        assert_eq!(detail["usage"]["cost_usd"], 0.01);
    }

    #[test]
    fn text_over_the_limit_fails_without_slicing() {
        let stdout = format!(
            "{}\n{}\n{}\n",
            version_line("1.0.46"),
            json!({"type": "text", "data": "abcdefghij"}),
            end_line("end_turn", json!({})),
        );
        let fault = interpret(&stdout, 4).unwrap_err();
        assert!(fault.message.contains("max_final_text_bytes"));
        let detail = failure_record(
            "chat_1",
            "sha256:ab",
            "runtime_protocol_error",
            "turn",
            &session_ref(),
            None,
        );
        assert_eq!(detail["code"], "runtime_protocol_error");
        assert_eq!(detail["stage"], "turn");
        assert!(detail.get("output_text").is_none());
        assert!(!detail.to_string().contains("abcdefghij"));
    }

    #[test]
    fn empty_or_truncated_answers_fail() {
        let empty = format!(
            "{}\n{}\n",
            version_line("1.0.46"),
            end_line("end_turn", json!({})),
        );
        assert!(interpret(&empty, 32).unwrap_err().message.contains("empty"));
        let truncated = format!(
            "{}\n{}\n{}\n",
            version_line("1.0.46"),
            json!({"type": "text", "data": "partial"}),
            end_line("max_tokens", json!({})),
        );
        assert!(interpret(&truncated, 32)
            .unwrap_err()
            .message
            .contains("end_turn"));
        assert!(interpret("not json\n", 32).is_err());
    }

    #[test]
    fn missing_credential_fails_at_credentials() {
        let (_dir, pb) = temp_pillbox();
        let error = load_credential(&pb, "chat_1").unwrap_err();
        assert!(format!("{error:#}").contains("not in the secret store"));
        let detail = failure_record(
            "chat_1",
            "sha256:ab",
            "runtime_unavailable",
            "credentials",
            &session_ref(),
            None,
        );
        assert_eq!(detail["code"], "runtime_unavailable");
        assert_eq!(detail["stage"], "credentials");
        assert!(detail.get("error").is_none());
        assert!(detail.get("usage").is_none());

        let secret = "xai-super-secret-value";
        write_secret(&pb, secret, None);
        let error = load_credential(&pb, "chat_1").unwrap_err();
        let rendered = format!("{error:#}");
        assert!(rendered.contains("not vaulted"));
        assert!(!rendered.contains(secret));

        write_secret(
            &pb,
            secret,
            Some(VaultMeta::new(
                "evil.example".into(),
                HeaderScheme::AuthorizationBearer,
                STUB_PREFIX.into(),
            )),
        );
        let error = load_credential(&pb, "chat_1").unwrap_err();
        let rendered = format!("{error:#}");
        assert!(rendered.contains("not bound"));
        assert!(!rendered.contains(secret));
    }

    #[test]
    fn a_vaulted_key_mints_a_stub_that_does_not_carry_the_secret() {
        let (_dir, pb) = temp_pillbox();
        let secret = "xai-real-key-0123456789abcdef";
        write_secret(
            &pb,
            format!("{secret}\n"),
            Some(VaultMeta::new(
                HOST.into(),
                HeaderScheme::AuthorizationBearer,
                STUB_PREFIX.into(),
            )),
        );
        let released = load_credential(&pb, "chat_1").unwrap_or_else(|error| panic!("{error:#}"));
        assert!(released.stub.starts_with(STUB_PREFIX));
        assert_ne!(released.stub, released.real);
        assert!(!released.stub.contains(secret));
        assert_eq!(released.real, secret);
    }
}
