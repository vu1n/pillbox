//! One Prime Agent text turn. No host workspace, configuration or real key
//! enters the VM. The image catalog and the observed native version must agree.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};

use super::evidence::ExecutionEvidence;
use super::prime_protocol::{self, Turn};
use super::store::OwnedInvocation;
use super::text_v2::{Outcome, TextRequestV2};
use crate::contract::{Custom, Payload};
use crate::pillbox::Pillbox;
use crate::sandbox::libkrun::repository::{
    prime_text::{runner_image_id, CredentialRelease, PreparedImage},
    TeardownUnconfirmed, VmLimits,
};
use crate::startup::StartupTimer;

pub(crate) fn execute(
    pb: &Pillbox,
    request: &TextRequestV2,
    owner: &mut OwnedInvocation,
) -> Result<Outcome> {
    let deadline = Instant::now() + Duration::from_millis(request.limits.timeout_ms);
    let mut evidence = ExecutionEvidence::start_text(
        pb,
        &request.session_ref.session_id,
        &request.invocation_id,
        json!({"request_hash": owner.record().request_hash,
            "agent": request.agent, "adapter_revision": prime_protocol::ADAPTER_REVISION}),
    )?;
    owner.running(json!({"session_ref": evidence.reference()}))?;
    let mut stage = "resolve";
    let mut resolve_rejected = false;
    let mut timer = StartupTimer::start();
    let mut turn = Turn::default();
    let mut diagnostics = Vec::new();
    let result = (|| -> Result<Value> {
        check_live(owner, deadline)?;
        let cancelled = || !matches!(owner.cancelled(), Ok(false));
        let image = runner_image_id(pb, deadline, &cancelled)?;
        let prepared = PreparedImage::prepare(&image, deadline, &cancelled)?;
        resolve_rejected = true;
        let selection = prepared.profile.select(
            &image,
            &request.agent.model,
            &request.agent.reasoning_effort,
        )?;
        stage_done("resolve", &mut timer, &mut evidence)?;
        stage = "credentials";
        let spec = crate::agents::lookup("text execute", "prime-agent")?;
        let release = credentials(&spec.home_dir(pb)?.join(spec.cred_sentinel))?;
        stage_done("credentials", &mut timer, &mut evidence)?;
        stage = "image_prepare";
        let mut stage_error = None;
        let mut vm = prepared.launch(
            &selection,
            release,
            VmLimits {
                max_duration: deadline
                    .checked_duration_since(Instant::now())
                    .context("Prime deadline exceeded")?,
                max_output_bytes: request.limits.max_evidence_bytes,
                max_frame_bytes: request.limits.max_frame_bytes as usize,
            },
            deadline,
            &cancelled,
            &mut |finished| {
                if let Err(error) = stage_done(finished, &mut timer, &mut evidence) {
                    stage_error.get_or_insert(error);
                }
                stage = match finished {
                    "image_prepare" => "guest_prepare",
                    "guest_prepare" => "vmm_spawn",
                    _ => "guest_rpc_ready",
                };
            },
        )?;
        crate::events::emit_session_event(
            pb,
            crate::events::EventType::SessionStarted {
                parent_session_id: crate::events::parent_session_id_from_env(),
                startup: Some(timer.snapshot()),
            },
            &request.session_ref.session_id,
            None,
        );
        let native = (|| -> Result<_> {
            if let Some(error) = stage_error {
                return Err(error);
            }
            let mut stream = vm.connect_rpc(&cancelled)?;
            stage_done("guest_rpc_ready", &mut timer, &mut evidence)?;
            stage = "turn";
            prime_protocol::run(
                &mut stream,
                &selection,
                &request.rendered_input,
                prime_protocol::Limits {
                    deadline,
                    frame: request.limits.max_frame_bytes as usize,
                    evidence: request.limits.max_evidence_bytes as usize,
                    final_text: request.limits.max_final_text_bytes as usize,
                },
                &mut turn,
                || {
                    check_live(owner, deadline)?;
                    vm.check_running(&cancelled)
                },
            )
        })();
        // A teardown error remains in the chain and prevents terminal sealing.
        vm.stop_and_reap()?;
        let captured = vm.final_diagnostics();
        diagnostics = captured.bytes;
        ensure!(
            !captured.truncated,
            "Prime diagnostics exceed evidence byte limit"
        );
        if let Some(error) = captured.error {
            return Err(error.context("collect Prime diagnostics"));
        }
        let native = native?;
        let raw = native_capture(&turn);
        ensure!(
            raw.len() + diagnostics.len() + native.text.len()
                <= request.limits.max_evidence_bytes as usize,
            "Prime evidence exceeds byte limit"
        );
        let capture = evidence.artifact(&raw, "application/x-ndjson")?;
        let diagnostic_artifact = evidence.artifact(&diagnostics, "text/plain")?;
        let text = evidence.artifact(native.text.as_bytes(), "text/plain;charset=utf-8")?;
        evidence.append(Payload::Custom(Custom {
            name: "text.result.captured".into(),
            payload: Some(
                json!({"native_evidence": capture, "diagnostics": diagnostic_artifact, "text": text}),
            ),
        }))?;
        stage_done("turn", &mut timer, &mut evidence)?;
        check_live(owner, deadline)?;
        stage = "finalize";
        let mut adapter = crate::agents::harness::PrimeAdapter::default();
        use crate::agents::harness::HarnessAdapter as _;
        for frame in &turn.frames {
            check_live(owner, deadline)?;
            for payload in adapter.parse_line(frame) {
                check_live(owner, deadline)?;
                evidence.append(payload)?;
            }
        }
        if let Some(usage) = &turn.usage {
            check_live(owner, deadline)?;
            evidence.append(Payload::Custom(Custom {
                name: "usage".into(),
                payload: Some(json!({"turn_usage": usage})),
            }))?;
        }
        let mut detail = json!({"invocation_id": request.invocation_id, "request_hash": owner.record().request_hash,
            "resolved": selection.resolved(&native.version, native.served_model.as_deref()),
            "session_ref": evidence.reference(), "output_text": native.text});
        with_usage(&mut detail, &turn);
        owner.observe(json!({"session_ref": evidence.reference()}))?;
        check_live(owner, deadline)?;
        Ok(detail)
    })();
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
            Ok(Outcome::Completed(detail))
        }
        Err(error) if error.downcast_ref::<TeardownUnconfirmed>().is_some() => Err(error),
        Err(error) => {
            let raw = native_capture(&turn);
            if !raw.is_empty() && raw.len() <= request.limits.max_evidence_bytes as usize {
                let capture = evidence.artifact(&raw, "application/x-ndjson")?;
                evidence.append(Payload::Custom(Custom {
                    name: "text.native.failed".into(),
                    payload: Some(json!({"native_evidence": capture})),
                }))?;
            }
            if !diagnostics.is_empty()
                && raw.len() + diagnostics.len() <= request.limits.max_evidence_bytes as usize
            {
                let artifact = evidence.artifact(&diagnostics, "text/plain")?;
                evidence.append(Payload::Custom(Custom {
                    name: "text.diagnostics.failed".into(),
                    payload: Some(json!({"diagnostics": artifact})),
                }))?;
            }
            evidence.append(Payload::Custom(Custom {
                name: "text.execution.failed".into(),
                payload: Some(json!({"error": format!("{error:#}"), "stage": stage})),
            }))?;
            if let Some(usage) = &turn.usage {
                evidence.append(Payload::Custom(Custom {
                    name: "usage".into(),
                    payload: Some(json!({"turn_usage": usage})),
                }))?;
            }
            owner.observe(json!({"session_ref": evidence.reference()}))?;
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
            let code = prime_protocol::failure_code(stage, &error, resolve_rejected);
            let mut detail = json!({"invocation_id": request.invocation_id, "request_hash": owner.record().request_hash,
                "code": code, "stage": stage, "session_ref": evidence.reference()});
            with_usage(&mut detail, &turn);
            Ok(Outcome::Failed(detail))
        }
    }
}

fn stage_done(
    stage: &'static str,
    timer: &mut StartupTimer,
    evidence: &mut ExecutionEvidence,
) -> Result<()> {
    timer.mark(stage);
    evidence.append(Payload::Custom(Custom {name: "text.stage.completed".into(),
        payload: Some(json!({"stage": stage, "duration_ms": timer.snapshot().stages.last().map(|stage| stage.duration_ms)}))}))?;
    Ok(())
}

fn check_live(owner: &OwnedInvocation, deadline: Instant) -> Result<()> {
    ensure!(Instant::now() < deadline, "Prime deadline exceeded");
    ensure!(!owner.cancelled()?, "Prime invocation cancelled");
    Ok(())
}

fn with_usage(detail: &mut Value, turn: &Turn) {
    if let Some(usage) = &turn.usage {
        detail["usage"] = json!(usage);
    }
}

fn native_capture(turn: &Turn) -> Vec<u8> {
    turn.frames
        .iter()
        .flat_map(|frame| {
            let mut bytes = frame.to_string().into_bytes();
            bytes.push(b'\n');
            bytes
        })
        .collect()
}

fn credentials(path: &Path) -> Result<CredentialRelease> {
    let real = prime_protocol::managed_key(path)?;
    let stub = format!("pillbox_prime_{}", uuid::Uuid::now_v7());
    let guest_auth =
        serde_json::to_vec(&json!({"prime-inference": {"type": "api_key", "key": stub}}))?;
    Ok(CredentialRelease {
        real,
        stub,
        guest_auth,
    })
}
