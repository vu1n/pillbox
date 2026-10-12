use std::time::{Duration, Instant};

use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};

use super::{Credentials, Resolved};
use crate::contract::{Custom, Payload};
use crate::execution::evidence::ExecutionEvidence;
use crate::execution::store::OwnedInvocation;
use crate::execution::text::{self, FailedStage, Stages};
use crate::execution::text_v2::TextRequestV2;
use crate::pillbox::Pillbox;
use crate::sandbox::libkrun::repository::{claude_text as launcher, VmLimits};

pub(crate) fn execute(
    pb: &Pillbox,
    request: &TextRequestV2,
    resolved: Resolved,
    owner: &mut OwnedInvocation,
) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_millis(request.limits.timeout_ms);
    let mut evidence = ExecutionEvidence::start_text(
        pb,
        &request.session_ref.session_id,
        &request.invocation_id,
        json!({
            "request_hash": owner.record().request_hash,
            "runner_image_id": resolved.runner_image_id,
            "effective_model_catalog_digest": resolved.catalog_digest,
            "adapter_revision": super::ADAPTER_REVISION,
            "credential_ref": super::CREDENTIAL_REF,
            "network_hosts": [super::PROVIDER_HOST],
        }),
    )?;
    let mut progress = json!({"session_ref":evidence.reference()});
    owner.running(progress.clone())?;
    let mut stages = Stages::start();
    let mut usage = None;
    let result = (|| -> Result<Value> {
        text::check_live(owner, deadline)?;
        let spec = crate::agents::lookup("execution", "claude")?;
        let path = spec.home_dir(pb)?.join(spec.cred_sentinel);
        let credentials: Credentials = super::credentials(&path, &request.invocation_id, deadline)?;
        stages.complete("credentials", &mut evidence)?;
        text::check_live(owner, deadline)?;
        let limits = &request.limits;
        let cancelled = || !matches!(owner.cancelled(), Ok(false));
        let mut stage_error = None;
        let mut vm = launcher::launch(
            launcher::Input {
                resolved: resolved.clone(),
                prompt: request.rendered_input.clone(),
                credentials,
                refresh_credentials: path,
            },
            VmLimits {
                max_duration: deadline
                    .checked_duration_since(Instant::now())
                    .context("text invocation deadline exceeded")?,
                max_output_bytes: limits.max_evidence_bytes,
                max_frame_bytes: limits.max_frame_bytes as usize,
            },
            &cancelled,
            &mut |name| {
                if let Err(error) = stages.complete(name, &mut evidence) {
                    stage_error.get_or_insert(error);
                }
            },
        )?;
        stages.emit_started(pb, &request.session_ref.session_id);
        let native = (|| -> Result<_> {
            if let Some(error) = stage_error {
                return Err(error.context("persist text stage evidence"));
            }
            let stream = vm.connect_rpc(&cancelled)?;
            stages.complete("guest_rpc_ready", &mut evidence)?;
            Ok(super::run(
                stream,
                super::Limits {
                    deadline,
                    max_frame_bytes: limits.max_frame_bytes as usize,
                    max_evidence_bytes: limits.max_evidence_bytes,
                    max_final_text_bytes: limits.max_final_text_bytes as usize,
                },
                || {
                    text::check_live(owner, deadline)?;
                    vm.drain_output()
                },
            ))
        })();
        let diagnostics = vm.diagnostics();
        // Unconfirmed teardown stays in the error chain and prevents sealing.
        let (capture, result) =
            super::finish_native(native, &mut usage, || vm.stop_and_reap().map(|_| ()))?;
        let diagnostics = diagnostics?;
        ensure!(
            capture.evidence_bytes()? + diagnostics.len() as u64 <= limits.max_evidence_bytes,
            "native evidence byte limit exceeded including diagnostics"
        );
        let native_evidence = evidence.native_frames(&capture.frames)?;
        progress["native_evidence"] = json!(native_evidence);
        progress["session_ref"] = json!(evidence.reference());
        owner.observe(progress.clone())?;
        let diagnostics = evidence.artifact(&diagnostics, "text/plain")?;
        evidence.append(Payload::Custom(Custom {
            name: "text.builder.stopped".into(),
            payload: Some(json!({"diagnostics":diagnostics})),
        }))?;
        if let Some(usage) = &usage {
            evidence.append(Payload::Custom(Custom {
                name: "usage".into(),
                payload: Some(json!({"turn_usage":usage})),
            }))?;
        }
        result?;
        let mut detail = capture.completed(&resolved)?;
        text::check_live(owner, deadline)?;
        stages.complete("turn", &mut evidence)?;
        let text = evidence.artifact(
            detail["output_text"]
                .as_str()
                .context("missing final text")?
                .as_bytes(),
            "text/plain;charset=utf-8",
        )?;
        evidence.append(Payload::Custom(Custom {
            name: "text.result.captured".into(),
            payload: Some(json!({"text":text,"native_evidence":native_evidence})),
        }))?;
        detail["invocation_id"] = json!(request.invocation_id);
        detail["request_hash"] = json!(owner.record().request_hash);
        detail["session_ref"] = json!(evidence.reference());
        owner.observe(
            json!({"session_ref":evidence.reference(),"native_evidence":native_evidence}),
        )?;
        Ok(detail)
    })();
    stages.emit_started(pb, &request.session_ref.session_id);
    match result {
        Ok(detail) => {
            crate::events::emit_session_event(
                pb,
                crate::events::EventType::SessionCompleted {
                    exit_code: Some(0),
                    trace_path: None,
                    result_snapshot: None,
                },
                &request.session_ref.session_id,
                None,
            );
            Ok(detail)
        }
        Err(error) => {
            let stage = stages.in_progress();
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
            let persisted = evidence
                .append(Payload::Custom(Custom {
                    name: "text.execution.failed".into(),
                    payload: Some(json!({"error":format!("{error:#}"),"stage":stage})),
                }))
                .and_then(|_| {
                    progress["session_ref"] = json!(evidence.reference());
                    owner.observe(progress)
                });
            let error = match persisted {
                Ok(()) => error,
                Err(persistence) => error.context(format!(
                    "failure evidence was not persisted: {persistence:#}"
                )),
            };
            Err(error.context(FailedStage { stage, usage }))
        }
    }
}
