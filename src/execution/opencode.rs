//! One invocation-owned OpenCode text turn, separate from its ordinary server sessions.

use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};

use crate::agents::harness::opencode::Profile;
use crate::pillbox::Pillbox;

pub(crate) fn credential(pb: &Pillbox, profile: &Profile) -> Result<String> {
    let value = crate::secrets::read(pb, profile.credential_ref)?
        .context("provider credential unavailable")?;
    ensure!(
        !value.is_empty() && value.len() <= 64 * 1024,
        "invalid provider credential"
    );
    Ok(value)
}

/// Failure records carry only closed vocabulary and evidence references.
pub(crate) fn failure(
    invocation: &str,
    request_hash: &str,
    stage: &str,
    code: &str,
    evidence: Value,
    usage: Option<&super::usage::TurnUsage>,
) -> Value {
    let mut value = json!({"invocation_id": invocation, "request_hash": request_hash,
        "stage": stage, "code": code, "session_ref": evidence});
    if let Some(usage) = usage {
        value["usage"] = json!(usage);
    }
    value
}

fn failure_code(stage: &str, error: &anyhow::Error) -> &'static str {
    if format!("{error:#}").contains("deadline exceeded") {
        "runtime_timeout"
    } else if error.is::<RejectedModel>() {
        "runtime_rejected"
    } else {
        match stage {
            "turn" => "runtime_protocol_error",
            "finalize" => "internal_error",
            _ => "runtime_unavailable",
        }
    }
}

#[cfg(feature = "libkrun")]
pub(crate) fn execute(
    pb: &Pillbox,
    request: &super::text_v2::TextRequestV2,
    owner: &mut super::store::OwnedInvocation,
) -> Result<super::text_v2::Outcome> {
    use super::evidence::ExecutionEvidence;
    use super::text_v2::Outcome;
    use crate::agents::harness::opencode::{Limits, Turn, ADAPTER_REVISION};
    use crate::contract::{Custom, Payload};
    use crate::events::EventType;
    use crate::sandbox::libkrun::repository::{opencode as vm, TeardownUnconfirmed, VmLimits};
    use std::time::{Duration, Instant};

    request.validate()?;
    let deadline = Instant::now() + Duration::from_millis(request.limits.timeout_ms);
    let mut timer = crate::startup::StartupTimer::start();
    let mut stage = "resolve";
    let mut evidence = ExecutionEvidence::start_text(
        pb,
        &request.session_ref.session_id,
        &request.invocation_id,
        json!({"request_hash": owner.record().request_hash, "adapter_revision": ADAPTER_REVISION,
            "agent": request.agent}),
    )?;
    owner.running(json!({"session_ref": evidence.reference()}))?;
    let mut turn = Turn::new();
    let mut started = false;
    let result = (|| -> Result<Value> {
        check_live(owner, deadline)?;
        let image_id = vm::image_id(pb, deadline, &|| !matches!(owner.cancelled(), Ok(false)))?;
        let cancelled = || !matches!(owner.cancelled(), Ok(false));
        let image = vm::resolve_image(&image_id, deadline, &cancelled)?;
        let profile = Profile::lower(
            image_id,
            &image.catalog,
            &request.agent.model,
            &request.agent.reasoning_effort,
        )
        .context(RejectedModel)?;
        complete_stage("resolve", &mut timer, &mut evidence)?;
        stage = "credentials";
        let real = credential(pb, &profile)?;
        complete_stage("credentials", &mut timer, &mut evidence)?;
        stage = "image_prepare";
        complete_stage("image_prepare", &mut timer, &mut evidence)?;
        stage = "guest_prepare";
        let mut stage_error = None;
        let mut owned_vm = vm::launch(
            image,
            &profile,
            real,
            VmLimits {
                max_duration: deadline
                    .checked_duration_since(Instant::now())
                    .context("text deadline exceeded")?,
                max_output_bytes: request.limits.max_evidence_bytes,
                max_frame_bytes: request.limits.max_frame_bytes as usize,
            },
            &cancelled,
            &mut |name| {
                if let Err(error) = complete_stage(name, &mut timer, &mut evidence) {
                    stage_error.get_or_insert(error);
                }
                stage = match name {
                    "guest_prepare" => "vmm_spawn",
                    _ => "guest_rpc_ready",
                };
            },
        )?;
        crate::events::emit_session_event(
            pb,
            EventType::SessionStarted {
                parent_session_id: crate::events::parent_session_id_from_env(),
                startup: Some(timer.snapshot()),
            },
            &request.session_ref.session_id,
            None,
        );
        started = true;
        let native = (|| -> Result<()> {
            if let Some(error) = stage_error {
                return Err(error);
            }
            let mut stream = owned_vm.connect_rpc(&cancelled)?;
            complete_stage("guest_rpc_ready", &mut timer, &mut evidence)?;
            stage = "turn";
            turn.run(
                &mut stream,
                &profile,
                &request.rendered_input,
                Limits {
                    deadline,
                    frame: request.limits.max_frame_bytes as usize,
                    evidence: request.limits.max_evidence_bytes as usize,
                    final_text: request.limits.max_final_text_bytes as usize,
                },
                || {
                    check_live(owner, deadline)?;
                    owned_vm.check_running(&cancelled)
                },
            )
        })();
        owned_vm.stop_and_reap()?;
        // Preserve spend and raw frames even on a refused or malformed turn.
        let mut raw = Vec::new();
        let mut mapper = crate::events::opencode::EventMapper::new();
        for frame in &turn.frames {
            serde_json::to_writer(&mut raw, frame)?;
            raw.push(b'\n');
            for payload in mapper.on_event(frame) {
                evidence.append(payload)?;
            }
        }
        let native_artifact = evidence.artifact(&raw, "application/x-ndjson")?;
        evidence.append(Payload::Custom(Custom {
            name: "text.native.captured".into(),
            payload: Some(json!({"native_evidence": native_artifact})),
        }))?;
        if let Some(usage) = &turn.usage {
            evidence.append(Payload::Custom(Custom {
                name: "usage".into(),
                payload: Some(json!({"turn_usage": usage})),
            }))?;
        }
        owner.observe(json!({"session_ref": evidence.reference()}))?;
        native?;
        check_live(owner, deadline)?;
        complete_stage("turn", &mut timer, &mut evidence)?;
        stage = "finalize";
        let text = evidence.artifact(turn.text.as_bytes(), "text/plain;charset=utf-8")?;
        evidence.append(Payload::Custom(Custom {
            name: "text.result.captured".into(),
            payload: Some(json!({"text": text, "native_evidence": native_artifact})),
        }))?;
        let mut value = json!({"invocation_id": request.invocation_id,
            "request_hash": owner.record().request_hash,
            "session_ref": evidence.reference(), "output_text": turn.text,
            "resolved": profile.resolved(turn.version.as_deref().context("harness version absent")?)});
        if let Some(usage) = &turn.usage {
            value["usage"] = json!(usage);
        }
        Ok(value)
    })();
    if !started {
        crate::events::emit_session_event(
            pb,
            EventType::SessionStarted {
                parent_session_id: crate::events::parent_session_id_from_env(),
                startup: Some(timer.snapshot()),
            },
            &request.session_ref.session_id,
            None,
        );
    }
    match result {
        Ok(detail) => {
            crate::events::emit_session_event(
                pb,
                EventType::SessionCompleted {
                    exit_code: Some(0),
                    trace_path: None,
                    result_snapshot: None,
                },
                &request.session_ref.session_id,
                None,
            );
            Ok(Outcome::Completed(detail))
        }
        Err(error) if error.is::<TeardownUnconfirmed>() => Err(error),
        Err(error) => {
            let code = failure_code(stage, &error);
            evidence.append(Payload::Custom(Custom {
                name: "text.execution.failed".into(),
                payload: Some(json!({"stage": stage, "error": format!("{error:#}")})),
            }))?;
            owner.observe(json!({"session_ref": evidence.reference()}))?;
            crate::events::emit_session_event(
                pb,
                EventType::SessionFailed {
                    reason: format!("{stage}: {error:#}"),
                    exit_code: Some(1),
                    trace_path: None,
                    result_snapshot: None,
                },
                &request.session_ref.session_id,
                None,
            );
            Ok(Outcome::Failed(failure(
                &request.invocation_id,
                &owner.record().request_hash,
                stage,
                code,
                json!(evidence.reference()),
                turn.usage.as_ref(),
            )))
        }
    }
}

#[cfg(feature = "libkrun")]
fn complete_stage(
    name: &'static str,
    timer: &mut crate::startup::StartupTimer,
    evidence: &mut super::evidence::ExecutionEvidence,
) -> Result<()> {
    timer.mark(name);
    evidence
        .append(crate::contract::Payload::Custom(crate::contract::Custom {
            name: "text.stage.completed".into(),
            payload: Some(json!({"stage": name,
            "duration_ms": timer.snapshot().stages.last().map(|stage| stage.duration_ms)})),
        }))
        .map(|_| ())
}

#[derive(Debug)]
struct RejectedModel;
impl std::fmt::Display for RejectedModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("unservable model selection")
    }
}
impl std::error::Error for RejectedModel {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::harness::opencode::Catalog;
    use crate::pillbox::Scope;

    fn profile() -> Profile {
        Profile::lower(
            format!("sha256:{}", "b".repeat(64)),
            &Catalog {
                version: "2.0.24".into(),
                models: vec![json!({"providerID": "openai", "id": "gpt-6-luna",
                "enabled": true, "capabilities": {"input": ["text"], "output": ["text"]},
                "variants": [{"id": "low"}]})],
            },
            "openai/gpt-6-luna",
            "low",
        )
        .unwrap()
    }

    #[test]
    fn missing_credential_is_unavailable_at_credentials_without_free_text() {
        let temp = tempfile::tempdir().unwrap();
        let pb = Pillbox {
            scope: Scope::Global,
            state_dir: temp.path().into(),
            meta: None,
        };
        let error = credential(&pb, &profile()).unwrap_err();
        let detail = failure(
            "inv_1",
            "hash",
            "credentials",
            failure_code("credentials", &error),
            json!({"session_id": "session_1"}),
            None,
        );
        assert_eq!(
            detail,
            json!({"invocation_id": "inv_1", "request_hash": "hash",
            "stage": "credentials", "code": "runtime_unavailable", "session_ref": {"session_id": "session_1"}})
        );
        assert!(detail.get("usage").is_none());
    }

    #[test]
    fn model_rejections_and_bad_turns_have_closed_codes() {
        let error =
            anyhow::anyhow!("unservable model details only in evidence").context(RejectedModel);
        assert_eq!(failure_code("resolve", &error), "runtime_rejected");
        let error = anyhow::anyhow!("deny_all shell attempt");
        let detail = failure(
            "inv_1",
            "hash",
            "turn",
            failure_code("turn", &error),
            Value::Null,
            None,
        );
        assert_eq!(detail["code"], "runtime_protocol_error");
        assert_eq!(detail["stage"], "turn");
        assert!(detail.get("error").is_none());
        assert!(!detail.to_string().contains("shell"));
        assert_eq!(
            failure_code("turn", &anyhow::anyhow!("text deadline exceeded")),
            "runtime_timeout"
        );
    }
}

#[cfg(feature = "libkrun")]
fn check_live(owner: &super::store::OwnedInvocation, deadline: std::time::Instant) -> Result<()> {
    ensure!(!owner.cancelled()?, "text invocation cancelled");
    ensure!(
        std::time::Instant::now() < deadline,
        "text deadline exceeded"
    );
    Ok(())
}
