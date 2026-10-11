//! Admission/evidence orchestration for the Pi-only owned libkrun transport.

use std::time::{Duration, Instant};

use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};

use super::{Catalog, Credentials, Limits, Plan, Turn, Wire, ADAPTER_REVISION};
use crate::contract::{Custom, Payload};
use crate::execution::evidence::ExecutionEvidence;
use crate::execution::store::OwnedInvocation;
use crate::execution::text_v2::{Outcome, TextRequestV2};
use crate::execution::usage::TurnUsage;
use crate::pillbox::Pillbox;
use crate::sandbox::libkrun::repository::{self, OwnedVm, TeardownUnconfirmed, VmLimits};
use crate::startup::StartupTimer;

pub(crate) fn execute(
    pb: &Pillbox,
    request: &TextRequestV2,
    owner: &mut OwnedInvocation,
) -> Result<Outcome> {
    let limits = limits(request);
    limits.validate()?;
    let deadline = Instant::now() + Duration::from_millis(limits.timeout_ms);
    let mut evidence = ExecutionEvidence::start_text(
        pb,
        &request.session_ref.session_id,
        &request.invocation_id,
        json!({"request_hash": owner.record().request_hash, "adapter_revision": ADAPTER_REVISION, "agent": request.agent}),
    )?;
    owner.running(json!({"session_ref": evidence.reference()}))?;
    let mut stage = "resolve";
    let mut rejection = false;
    let mut usage = None;
    let mut timer = StartupTimer::start();
    let mut budget = limits.max_evidence_bytes;
    let result = run(
        pb,
        request,
        owner,
        &mut evidence,
        limits,
        deadline,
        &mut stage,
        &mut rejection,
        &mut usage,
        &mut timer,
        &mut budget,
    );
    match result {
        Ok(mut detail) => {
            if let Some(usage) = usage {
                detail["usage"] = json!(usage);
            }
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
        Err(error) => {
            let timed_out = error
                .downcast_ref::<super::FailureAtDetection>()
                .map_or_else(|| Instant::now() >= deadline, |failure| failure.timed_out);
            let code = super::failure_code(stage, timed_out, rejection);
            evidence.append(Payload::Custom(Custom {
                name: "text.execution.failed".into(),
                payload: Some(json!({"stage": stage, "error": format!("{error:#}")})),
            }))?;
            owner.observe(json!({"session_ref": evidence.reference()}))?;
            crate::events::emit_session_event(
                pb,
                crate::events::EventType::SessionFailed {
                    reason: format!("{stage}: {code}"),
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
            let mut detail = json!({"invocation_id": request.invocation_id, "request_hash": owner.record().request_hash,
                "stage": stage, "code": code, "session_ref": evidence.reference()});
            if let Some(usage) = usage {
                detail["usage"] = json!(usage);
            }
            Ok(Outcome::Failed(detail))
        }
    }
}

fn limits(request: &TextRequestV2) -> Limits {
    Limits {
        timeout_ms: request.limits.timeout_ms,
        max_final_text_bytes: request.limits.max_final_text_bytes,
        max_frame_bytes: request.limits.max_frame_bytes,
        max_evidence_bytes: request.limits.max_evidence_bytes,
    }
}

#[allow(clippy::too_many_arguments)]
fn run(
    pb: &Pillbox,
    request: &TextRequestV2,
    owner: &mut OwnedInvocation,
    evidence: &mut ExecutionEvidence,
    limits: Limits,
    deadline: Instant,
    stage: &mut &'static str,
    rejection: &mut bool,
    usage: &mut Option<TurnUsage>,
    timer: &mut StartupTimer,
    budget: &mut u64,
) -> Result<Value> {
    let cancelled = || !matches!(owner.cancelled(), Ok(false));
    let image = repository::pi_text::image_id(pb, deadline, &cancelled)?;
    let mut vm = repository::pi_text::launch(
        &image,
        None,
        None,
        vm_limits(limits, deadline, *budget)?,
        &cancelled,
        &mut |_| {},
    )?;
    let observed = (|| -> Result<Catalog> {
        let stream = vm.connect_rpc(&cancelled)?;
        let mut wire = Wire::new(stream, remaining_limits(limits, *budget), deadline)?;
        let result = super::classify(
            read_catalog(&mut wire, &mut || live(owner, deadline)),
            deadline,
        );
        capture(evidence, &wire.bytes, "pi.resolve", budget)?;
        result
    })();
    let catalog = after_stop(observed, stop(&mut vm, evidence, budget))?;
    let plan = Plan::resolve(
        image,
        catalog,
        &request.agent.model,
        &request.agent.reasoning_effort,
    )
    .inspect_err(|_| *rejection = true)?;
    mark("resolve", evidence, timer)?;
    *stage = "credentials";
    live(owner, deadline)?;
    let spec = crate::agents::lookup("text", "codex")?;
    let credential_path = spec.home_dir(pb)?.join(spec.cred_sentinel);
    let credentials =
        Credentials::read_managed(&credential_path, &request.invocation_id, &mut || {
            live(owner, deadline)
        })?;
    mark("credentials", evidence, timer)?;
    *stage = "image_prepare";
    let cancelled = || !matches!(owner.cancelled(), Ok(false));
    let mut callback_error = None;
    let launched = repository::pi_text::launch(
        &plan.runner_image_id,
        Some(&credentials),
        Some(&credential_path),
        vm_limits(limits, deadline, *budget)?,
        &cancelled,
        &mut |completed| {
            if let Err(error) = mark(completed, evidence, timer) {
                callback_error.get_or_insert(error);
            }
            *stage = match completed {
                "image_prepare" => "guest_prepare",
                "guest_prepare" => "vmm_spawn",
                _ => "guest_rpc_ready",
            };
        },
    );
    let mut vm = launched?;
    crate::events::emit_session_event(
        pb,
        crate::events::EventType::SessionStarted {
            parent_session_id: crate::events::parent_session_id_from_env(),
            startup: Some(timer.snapshot()),
        },
        &request.session_ref.session_id,
        None,
    );
    let mut turn = Turn::default();
    let result = (|| -> Result<(String, Option<String>)> {
        if let Some(error) = callback_error {
            return Err(error);
        }
        let stream = vm.connect_rpc(&cancelled)?;
        mark("guest_rpc_ready", evidence, timer)?;
        *stage = "turn";
        let mut wire = Wire::new(stream, remaining_limits(limits, *budget), deadline)?;
        let result = (|| -> Result<_> {
            wire.send(&json!({"model": plan.model, "reasoning_effort": plan.reasoning_effort,
                "timeout_ms": deadline.checked_duration_since(Instant::now()).context("Pi deadline exceeded")?.as_millis() as u64,
                "input": request.rendered_input, "credential": credentials.guest,
                "max_frame_bytes": limits.max_frame_bytes, "max_evidence_bytes": *budget}), &mut || live(owner, deadline))?;
            while let Some(line) = wire.next(&mut || live(owner, deadline))? {
                turn.observe(&line, limits.max_final_text_bytes as usize)?;
            }
            turn.finish()
        })();
        let result = super::classify(result, deadline);
        *usage = turn.usage.clone();
        capture(evidence, &wire.bytes, "pi.turn", budget)?;
        if let Some(usage) = usage {
            evidence.append(Payload::Custom(Custom {
                name: "usage".into(),
                payload: Some(json!({"turn_usage": usage})),
            }))?;
        }
        result
    })();
    drop(credentials);
    let (text, served_model) = after_stop(result, stop(&mut vm, evidence, budget))?;
    live(owner, deadline)?;
    capture(evidence, text.as_bytes(), "pi.final_text", budget)?;
    mark("turn", evidence, timer)?;
    *stage = "finalize";
    owner.observe(json!({"session_ref": evidence.reference()}))?;
    Ok(
        json!({"invocation_id": request.invocation_id, "request_hash": owner.record().request_hash,
        "resolved": plan.resolved(&request.agent.model, served_model.as_deref()),
        "session_ref": evidence.reference(), "output_text": text}),
    )
}

fn read_catalog(wire: &mut Wire, live: &mut dyn FnMut() -> Result<()>) -> Result<Catalog> {
    let first = wire.next(live)?.context("Pi catalog missing")?;
    ensure!(
        first["type"] == "pillbox_pi.catalog",
        "Pi catalog response malformed"
    );
    let catalog = serde_json::from_value(first["catalog"].clone())?;
    let terminal = wire.next(live)?.context("Pi catalog exit absent")?;
    ensure!(
        terminal["type"] == "pillbox_pi.exit" && terminal["code"] == 0,
        "Pi catalog failed"
    );
    ensure!(wire.next(live)?.is_none(), "Pi catalog trailing event");
    Ok(catalog)
}

fn live(owner: &OwnedInvocation, deadline: Instant) -> Result<()> {
    ensure!(!owner.cancelled()?, "Pi invocation cancelled");
    ensure!(Instant::now() < deadline, "Pi deadline exceeded");
    Ok(())
}

fn remaining_limits(mut limits: Limits, budget: u64) -> Limits {
    limits.max_evidence_bytes = budget;
    limits
}

fn vm_limits(limits: Limits, deadline: Instant, budget: u64) -> Result<VmLimits> {
    ensure!(budget > 0, "Pi evidence limit");
    Ok(VmLimits {
        max_duration: deadline
            .checked_duration_since(Instant::now())
            .context("Pi deadline exceeded")?,
        max_output_bytes: budget,
        max_frame_bytes: limits.max_frame_bytes as usize,
    })
}

fn capture(
    evidence: &mut ExecutionEvidence,
    bytes: &[u8],
    name: &str,
    budget: &mut u64,
) -> Result<()> {
    *budget = budget
        .checked_sub(bytes.len() as u64)
        .context("Pi evidence limit")?;
    let artifact = evidence.artifact(bytes, "application/octet-stream")?;
    evidence.append(Payload::Custom(Custom {
        name: format!("text.{name}.captured"),
        payload: Some(json!({"artifact": artifact})),
    }))?;
    Ok(())
}

fn stop(vm: &mut OwnedVm, evidence: &mut ExecutionEvidence, budget: &mut u64) -> Result<()> {
    vm.stop_and_reap()?;
    let diagnostics = vm.final_diagnostics();
    capture(evidence, &diagnostics.bytes, "pi.diagnostics", budget)?;
    ensure!(!diagnostics.truncated, "Pi diagnostics truncated");
    if let Some(error) = diagnostics.error {
        return Err(error);
    }
    Ok(())
}

fn after_stop<T>(result: Result<T>, stopped: Result<()>) -> Result<T> {
    match (result, stopped) {
        (_, Err(error)) if error.downcast_ref::<TeardownUnconfirmed>().is_some() => Err(error),
        (Err(error), Err(cleanup)) => Err(error.context(format!("Pi cleanup failed: {cleanup:#}"))),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Ok(value), Ok(())) => Ok(value),
    }
}

fn mark(
    stage: &'static str,
    evidence: &mut ExecutionEvidence,
    timer: &mut StartupTimer,
) -> Result<()> {
    timer.mark(stage);
    evidence.append(Payload::Custom(Custom { name: "text.stage.completed".into(),
        payload: Some(json!({"stage": stage, "duration_ms": timer.snapshot().stages.last().map(|stage| stage.duration_ms)})) }))?;
    Ok(())
}
