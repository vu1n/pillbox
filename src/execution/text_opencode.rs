//! The OpenCode 2 driver behind `pillbox.text/2`: one tool-free turn in an
//! invocation-owned microVM, with the same stages, stage evidence and session events
//! as the Codex path (`text.rs`). What the frames mean is [`super::opencode`].

use std::io::Read;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};

use super::evidence::ExecutionEvidence;
use super::opencode::{self, Plan, TurnFold};
use super::store::OwnedInvocation;
use super::text::{check_live, FailedStage, Stages};
use super::text_v2::{TextRequestV2, Turn, ADAPTER_REVISION};
use super::usage::TurnUsage;
use crate::contract::{Custom, Payload};
use crate::pillbox::Pillbox;
use crate::sandbox::libkrun::repository::opencode::{launch_staged, OpencodeInput};
use crate::sandbox::libkrun::repository::VmLimits;

/// Below the evidence store's own frame ceiling, so a long turn fails here with a
/// clear reason instead of at evidence time.
const MAX_FRAMES: usize = 16_000;

pub(crate) fn execute(
    pb: &Pillbox,
    request: &TextRequestV2,
    plan: &Plan,
    runner_image_id: &str,
    owner: &mut OwnedInvocation,
) -> Result<Turn> {
    let deadline = Instant::now() + Duration::from_millis(request.limits.timeout_ms);
    check_live(owner, deadline)?;
    let session_id = &request.session_ref.session_id;
    let mut evidence = ExecutionEvidence::start_text(
        pb,
        session_id,
        &request.invocation_id,
        json!({
            "request_hash": owner.record().request_hash,
            "runner_image_id": runner_image_id,
            "adapter_revision": ADAPTER_REVISION,
            "agent": request.agent,
            "egress_hosts": plan.hosts,
        }),
    )?;
    let mut progress = json!({
        "invocation_id": request.invocation_id,
        "request_hash": owner.record().request_hash,
        "runner_image_id": runner_image_id,
        "session_ref": evidence.reference(),
    });
    owner.running(progress.clone())?;
    let mut stages = Stages::start();
    let mut usage = None;
    let result = run(
        pb,
        request,
        plan,
        runner_image_id,
        owner,
        deadline,
        &mut evidence,
        &mut progress,
        &mut stages,
        &mut usage,
    );
    stages.emit_started(pb, session_id);
    match result {
        Ok((output_text, harness_version)) => {
            crate::events::emit_session_event(
                pb,
                crate::events::EventType::SessionCompleted {
                    exit_code: Some(0),
                    trace_path: None,
                    result_snapshot: None,
                },
                session_id,
                None,
            );
            Ok(Turn {
                output_text,
                harness_version,
                // OpenCode does not surface the provider's model id.
                served_model: None,
                session_ref: evidence.reference(),
                usage,
            })
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
                session_id,
                None,
            );
            let recorded = evidence
                .append(Payload::Custom(Custom {
                    name: "text.execution.failed".into(),
                    payload: Some(json!({"error": format!("{error:#}"), "stage": stage})),
                }))
                .and_then(|_| {
                    progress["session_ref"] = json!(evidence.reference());
                    owner.observe(progress)
                });
            // Keep TeardownUnconfirmed in the chain whatever happens to the evidence.
            let error = match recorded {
                Ok(()) => error,
                Err(persistence) => error.context(format!(
                    "failure evidence was not persisted: {persistence:#}"
                )),
            };
            Err(error.context(FailedStage { stage, usage }))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run(
    pb: &Pillbox,
    request: &TextRequestV2,
    plan: &Plan,
    runner_image_id: &str,
    owner: &mut OwnedInvocation,
    deadline: Instant,
    evidence: &mut ExecutionEvidence,
    progress: &mut Value,
    stages: &mut Stages,
    usage: &mut Option<TurnUsage>,
) -> Result<(String, String)> {
    let input = OpencodeInput {
        image_id: runner_image_id.into(),
        turn: serde_json::to_vec(&opencode::turn_document(plan, &request.rendered_input))?,
        key_env: plan.key_env.clone(),
        key: provider_key(pb, &plan.key_env)?,
        hosts: plan.hosts.clone(),
    };
    stages.complete("credentials", evidence)?;
    check_live(owner, deadline)?;
    let limits = &request.limits;
    let cancelled = || !matches!(owner.cancelled(), Ok(false));
    let mut stage_error = None;
    let launched = launch_staged(
        input,
        VmLimits {
            max_duration: deadline
                .checked_duration_since(Instant::now())
                .context("text deadline exceeded")?,
            max_output_bytes: limits.max_evidence_bytes,
            max_frame_bytes: limits.max_frame_bytes as usize,
        },
        &cancelled,
        &mut |name| {
            if let Err(error) = stages.complete(name, evidence) {
                stage_error.get_or_insert(error);
            }
        },
    );
    let mut vm = launched?;
    stages.emit_started(pb, &request.session_ref.session_id);
    let mut capture = Capture::default();
    let read = (|| -> Result<()> {
        if let Some(error) = stage_error {
            return Err(error.context("persist text stage evidence"));
        }
        let stream = vm.connect_rpc(&cancelled)?;
        stages.complete("guest_rpc_ready", evidence)?;
        capture.read(
            stream,
            limits.max_frame_bytes,
            limits.max_evidence_bytes,
            |idle| {
                check_live(owner, deadline)?;
                if idle {
                    vm.check_running(&cancelled)?;
                }
                Ok(())
            },
        )
    })();
    let diagnostics = vm.diagnostics();
    vm.stop_and_reap()?;
    let (finished, spent) = capture.fold.finish(limits.max_final_text_bytes as usize);
    *usage = spent;
    let outcome = read.and(finished);
    if outcome.is_ok() {
        stages.complete("turn", evidence)?;
    }
    if !capture.frames.is_empty() {
        progress["native_evidence"] = json!(evidence.native_frames(&capture.frames)?);
    }
    let diagnostics = evidence.artifact(&diagnostics?, "text/plain")?;
    evidence.append(Payload::Custom(Custom {
        name: "text.builder.stopped".into(),
        payload: Some(json!({"diagnostics": diagnostics})),
    }))?;
    if let Some(usage) = usage {
        evidence.append(Payload::Custom(Custom {
            name: "usage".into(),
            payload: Some(json!({"turn_usage": usage})),
        }))?;
    }
    progress["session_ref"] = json!(evidence.reference());
    owner.observe(progress.clone())?;
    let completed = outcome?;
    check_live(owner, deadline)?;
    let text = evidence.artifact(completed.text.as_bytes(), "text/plain;charset=utf-8")?;
    evidence.append(Payload::Custom(Custom {
        name: "text.result.captured".into(),
        payload: Some(json!({"text": text, "native_evidence": progress["native_evidence"]})),
    }))?;
    progress["session_ref"] = json!(evidence.reference());
    owner.observe(progress.clone())?;
    Ok((completed.text, completed.harness_version))
}

/// The selected provider's key, from the pillbox secret named after the variable
/// OpenCode reads it from. Only that key enters the VM: the user's own OpenCode
/// store holds every provider's credential and their session history.
fn provider_key(pb: &Pillbox, key_env: &str) -> Result<String> {
    crate::secrets::read(pb, key_env)?
        .map(|key| key.trim().to_owned())
        .filter(|key| !key.is_empty())
        .with_context(|| format!("no {key_env} secret; run `pillbox secret add {key_env}`"))
}

/// The guest's JSON lines as evidence envelopes, folded as they arrive.
#[derive(Default)]
struct Capture {
    fold: TurnFold,
    frames: Vec<Value>,
    bytes: u64,
}

impl Capture {
    /// Read until the turn reaches an outcome. Frames read before a failure stay in
    /// `frames`, so a refused tool call is still in the evidence. `poll(idle)` runs
    /// before every read and again when a read times out; only an idle poll may fail
    /// on the VM having exited, because the guest exits right after it sends the
    /// outcome, which can still be buffered in the stream.
    fn read(
        &mut self,
        mut stream: UnixStream,
        max_frame_bytes: u64,
        max_bytes: u64,
        mut poll: impl FnMut(bool) -> Result<()>,
    ) -> Result<()> {
        let mut line = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            poll(false)?;
            let count = match stream.read(&mut chunk) {
                Ok(0) => {
                    // The guest closes without a frame when its own clock runs out, so an
                    // expired deadline is reported as one, not as a protocol error.
                    poll(false)?;
                    bail!("OpenCode guest closed the stream before the turn ended")
                }
                Ok(count) => count,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    poll(true)?;
                    continue;
                }
                Err(error) => return Err(error).context("read the OpenCode guest stream"),
            };
            for &byte in &chunk[..count] {
                if byte != b'\n' {
                    ensure!(
                        (line.len() as u64) < max_frame_bytes,
                        "OpenCode frame byte limit exceeded"
                    );
                    line.push(byte);
                    continue;
                }
                ensure!(
                    self.frames.len() < MAX_FRAMES,
                    "OpenCode frame limit exceeded"
                );
                let frame: Value =
                    serde_json::from_slice(&line).context("invalid OpenCode guest frame")?;
                line.clear();
                let envelope = json!({"direction": "inbound", "message": frame});
                // Charge what the evidence store writes (envelope and newline), not the
                // raw guest line, so the limit holds for the persisted artifact.
                self.bytes += serde_json::to_vec(&envelope)?.len() as u64 + 1;
                ensure!(
                    self.bytes <= max_bytes,
                    "OpenCode evidence byte limit exceeded"
                );
                self.frames.push(envelope);
                if self.fold.observe(&frame)? {
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn turn(lines: &[Value]) -> UnixStream {
        let (host, mut guest) = UnixStream::pair().unwrap();
        for line in lines {
            guest.write_all(line.to_string().as_bytes()).unwrap();
            guest.write_all(b"\n").unwrap();
        }
        drop(guest);
        host
    }

    fn opening() -> Vec<Value> {
        vec![
            json!({"pillbox": "info", "info": {"version": "2.0.24"}}),
            json!({"pillbox": "session", "id": "ses_1"}),
        ]
    }

    #[test]
    fn a_missing_provider_key_fails_before_any_vm_work() {
        crate::test_util::with_isolated_home("text-opencode-key", || {
            let pb = crate::pillbox::global();
            let error = provider_key(&pb, "ZHIPU_API_KEY").unwrap_err().to_string();
            assert!(
                error.contains("pillbox secret add ZHIPU_API_KEY"),
                "{error}"
            );
            std::env::set_var("__PILLBOX_TEST_OPENCODE_KEY", " k-1 ");
            crate::secrets::add(
                &pb,
                crate::pillbox::WriteScope::Resolved,
                "ZHIPU_API_KEY",
                crate::secrets::AddSource::EnvVar("__PILLBOX_TEST_OPENCODE_KEY".into()),
                false,
                None,
            )
            .unwrap();
            std::env::remove_var("__PILLBOX_TEST_OPENCODE_KEY");
            assert_eq!(provider_key(&pb, "ZHIPU_API_KEY").unwrap(), "k-1");
        });
    }

    #[test]
    fn reads_until_the_outcome_and_keeps_every_frame() {
        let mut lines = opening();
        lines.push(json!({"type": "session.text.ended",
            "data": {"sessionID": "ses_1", "assistantMessageID": "msg_1", "text": "hi"}}));
        lines.push(json!({"type": "session.execution.succeeded", "data": {"sessionID": "ses_1"}}));
        let mut capture = Capture::default();
        capture
            .read(turn(&lines), 1024, 1 << 20, |_| Ok(()))
            .unwrap();
        assert_eq!(capture.frames.len(), 4);
        assert_eq!(capture.fold.finish(16).0.unwrap().text, "hi");
    }

    #[test]
    fn an_outcome_buffered_after_the_vm_exits_still_completes() {
        let mut lines = opening();
        lines.push(json!({"type": "session.text.ended",
            "data": {"sessionID": "ses_1", "assistantMessageID": "msg_1", "text": "hi"}}));
        lines.push(json!({"type": "session.execution.succeeded", "data": {"sessionID": "ses_1"}}));
        let exited = |idle: bool| -> Result<()> {
            ensure!(!idle, "owned VMM exited before completion");
            Ok(())
        };
        let mut capture = Capture::default();
        capture.read(turn(&lines), 1024, 1 << 20, exited).unwrap();
        assert_eq!(capture.fold.finish(16).0.unwrap().text, "hi");
    }

    #[test]
    fn limits_and_a_closed_stream_fail_the_read() {
        let mut lines = opening();
        lines.push(
            json!({"type": "session.text.ended", "data": {"sessionID": "ses_1",
            "assistantMessageID": "msg_1", "text": "x".repeat(200)}}),
        );
        let read = |max_frame, max_bytes| {
            Capture::default()
                .read(turn(&lines), max_frame, max_bytes, |_| Ok(()))
                .unwrap_err()
                .to_string()
        };
        assert!(read(100, 1 << 20).contains("frame byte limit"));
        assert!(read(1024, 100).contains("evidence byte limit"));
        assert!(read(1024, 1 << 20).contains("closed the stream"));
    }

    #[test]
    fn the_byte_limit_charges_the_persisted_envelopes() {
        let mut lines = opening();
        lines.push(json!({"type": "session.execution.succeeded", "data": {"sessionID": "ses_1"}}));
        let raw: u64 = lines
            .iter()
            .map(|line| line.to_string().len() as u64 + 1)
            .sum();
        let mut capture = Capture::default();
        capture
            .read(turn(&lines), 1024, 1 << 20, |_| Ok(()))
            .unwrap();
        let persisted: u64 = capture
            .frames
            .iter()
            .map(|envelope| serde_json::to_vec(envelope).unwrap().len() as u64 + 1)
            .sum();
        assert!(raw < persisted - 1);
        let read = |max_bytes| Capture::default().read(turn(&lines), 1024, max_bytes, |_| Ok(()));
        read(persisted).unwrap();
        let error = read(persisted - 1).unwrap_err().to_string();
        assert!(error.contains("evidence byte limit"), "{error}");
    }

    #[test]
    fn a_stream_closed_after_the_deadline_is_a_timeout() {
        // The guest hangs up when its clock runs out; by then the host's has too.
        let mut polls = 0;
        let expired = |_: bool| -> Result<()> {
            polls += 1;
            ensure!(polls < 3, "text invocation deadline exceeded");
            Ok(())
        };
        let error = Capture::default()
            .read(turn(&opening()), 1024, 1 << 20, expired)
            .unwrap_err()
            .to_string();
        assert!(error.contains("deadline exceeded"), "{error}");
    }

    #[test]
    fn a_tool_attempt_is_kept_as_evidence() {
        let mut lines = opening();
        lines.push(json!({"type": "session.tool.called", "data": {"sessionID": "ses_1"}}));
        let mut capture = Capture::default();
        let error = capture.read(turn(&lines), 1024, 1 << 20, |_| Ok(()));
        assert!(error.unwrap_err().to_string().contains("tool"));
        assert_eq!(capture.frames.len(), 3);
    }
}
