//! `pillbox.text/2`: a harness-agnostic text invocation.
//!
//! The caller names the agent (harness, model, effort), the input and the limits.
//! Pillbox resolves the runner image, harness version, model catalog, credential and
//! egress itself, and reports what it ran in `resolved`. A failure carries one closed
//! code and the stage that was running; free text stays in the session evidence.

use std::process::Command;

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::evidence::ExecutionEvidence;
use super::protocol;
use super::store::OwnedInvocation;
use super::text::{self, FailedStage, TextExecution, TextLimits, TextRequest, TextRuntime};
use super::usage::TurnUsage;
use super::{digest, HarnessTransport, ModelProfile, SessionIdentity, TextOutputFormat};
use crate::contract::{Custom, Payload};
use crate::pillbox::Pillbox;
use crate::sandbox::libkrun::repository::TeardownUnconfirmed;

pub(crate) const CONTRACT_VERSION: &str = "pillbox.text/2";
pub(crate) const ADAPTER_REVISION: &str = "pillbox/local-text-v2";
const CODEX_HOST: &str = "chatgpt.com";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentSelection {
    pub(crate) harness: String,
    pub(crate) model: String,
    pub(crate) reasoning_effort: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TextRequestV2 {
    pub(crate) contract_version: String,
    pub(crate) session_ref: SessionIdentity,
    pub(crate) invocation_id: String,
    pub(crate) idempotency_key: String,
    pub(crate) rendered_input: String,
    pub(crate) rendered_input_hash: String,
    pub(crate) tool_policy: String,
    pub(crate) agent: AgentSelection,
    pub(crate) placement: String,
    pub(crate) output_format: TextOutputFormat,
    pub(crate) limits: TextLimits,
}

/// A terminal state to record on the invocation. `Err` from [`execute`] is reserved
/// for an unconfirmed VM teardown, which must not seal the invocation.
pub(crate) enum Outcome {
    Completed(Value),
    Failed(Value),
}

impl TextRequestV2 {
    /// Checks that do not depend on what is installed. A request that fails here was
    /// never admitted.
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.contract_version == CONTRACT_VERSION,
            "unsupported text contract"
        );
        ensure!(
            self.idempotency_key == self.invocation_id,
            "invocation/idempotency mismatch"
        );
        ensure!(
            self.rendered_input_hash == digest(self.rendered_input.as_bytes()),
            "rendered input digest mismatch"
        );
        ensure!(
            self.tool_policy == "deny_all",
            "unsupported text tool policy"
        );
        ensure!(
            self.placement == "local_microvm",
            "unsupported text placement"
        );
        ensure!(
            matches!(
                self.agent.harness.as_str(),
                "codex" | "claude_code" | "pi" | "opencode"
            ),
            "unknown text harness"
        );
        for id in [&self.invocation_id, &self.session_ref.session_id] {
            ensure!(
                !id.is_empty()
                    && id.len() <= 128
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')),
                "invalid text identity"
            );
        }
        ensure!(
            !self.rendered_input.is_empty() && self.rendered_input.len() <= 512 * 1024,
            "invalid rendered input size"
        );
        ensure!(
            self.output_format.kind == "text" && self.output_format.retry_count == 0,
            "unsupported text output format"
        );
        let limits = &self.limits;
        ensure!(
            (1..=super::MAX_TIMEOUT_MS).contains(&limits.timeout_ms)
                && (1..=32_768).contains(&limits.max_final_text_bytes)
                && (1..=super::MAX_FRAME_BYTES).contains(&limits.max_frame_bytes)
                && (1..=super::MAX_EVIDENCE_BYTES).contains(&limits.max_evidence_bytes)
                && limits.max_frame_bytes <= limits.max_evidence_bytes,
            "invalid text limits"
        );
        Ok(())
    }

    /// The Codex request this selection resolves to on `runner_image_id`. The embedded
    /// catalog cohort is an implementation detail; the CLI version is observed, not required.
    fn codex_request(&self, runner_image_id: String) -> Result<TextRequest> {
        let profile = self
            .agent
            .model
            .strip_prefix("gpt-6-")
            .context("unsupported Codex model")?;
        let codex = protocol::CodexProfile::new_for_version(
            protocol::GPT6_CODEX_VERSION,
            &self.agent.model,
            &self.agent.reasoning_effort,
        )?;
        let request = TextRequest {
            contract_version: text::CONTRACT_VERSION.into(),
            session_ref: self.session_ref.clone(),
            invocation_id: self.invocation_id.clone(),
            idempotency_key: self.idempotency_key.clone(),
            rendered_input: self.rendered_input.clone(),
            rendered_input_hash: self.rendered_input_hash.clone(),
            tool_policy: self.tool_policy.clone(),
            execution: TextExecution {
                transport: HarnessTransport {
                    harness: "codex".into(),
                    transport: "app_server".into(),
                    harness_version: codex.version().into(),
                    adapter_revision: text::ADAPTER_REVISION.into(),
                },
                requested: ModelProfile {
                    provider: "openai".into(),
                    model: self.agent.model.clone(),
                    profile: profile.into(),
                    reasoning_effort: self.agent.reasoning_effort.clone(),
                },
                placement: self.placement.clone(),
                context_renderer_revision: CONTRACT_VERSION.into(),
            },
            execution_policy_revision: text::POLICY_REVISION.into(),
            output_format: self.output_format.clone(),
            runtime: TextRuntime {
                runner_image_id,
                effective_model_catalog_digest: codex.effective_catalog_digest().into(),
                credential_ref: "pillbox:codex:default".into(),
                network_hosts: vec![CODEX_HOST.into()],
                limits: self.limits.clone(),
            },
        };
        request.validate()?;
        Ok(request)
    }
}

pub(crate) fn execute(
    pb: &Pillbox,
    request: &TextRequestV2,
    owner: &mut OwnedInvocation,
) -> Result<Outcome> {
    if request.agent.harness == "opencode" {
        return super::opencode::execute(pb, request, owner);
    }
    let resolved = resolve(pb, request);
    let (lowered, runner_image_id) = match resolved {
        Ok(resolved) => resolved,
        Err(rejection) => return unresolved(pb, request, owner, rejection),
    };
    match text::execute_with(pb, &lowered, owner, false) {
        Ok((completion, observed)) => {
            let mut detail = json!({
                "invocation_id": completion.invocation_id,
                "request_hash": completion.request_hash,
                "resolved": {
                    "harness": request.agent.harness,
                    "harness_version": observed
                        .cli_version
                        .context("Codex did not report its version")?,
                    "adapter_revision": ADAPTER_REVISION,
                    "runner_image_id": runner_image_id,
                    "requested_model": completion.requested_model,
                    "served_model": completion.served_model,
                },
                "session_ref": completion.session_ref,
                "output_text": completion.output_text,
            });
            with_usage(&mut detail, observed.usage.as_ref());
            Ok(Outcome::Completed(detail))
        }
        Err(error) if error.downcast_ref::<TeardownUnconfirmed>().is_some() => Err(error),
        Err(error) => {
            let failed = error.downcast_ref::<FailedStage>();
            let stage = failed.map_or("credentials", |failed| failed.stage);
            let mut detail = json!({
                "invocation_id": request.invocation_id,
                "request_hash": owner.record().request_hash,
                "code": failure_code(stage, &error),
                "stage": stage,
                "session_ref": owner.record().detail.get("session_ref"),
            });
            with_usage(&mut detail, failed.and_then(|failed| failed.usage.as_ref()));
            Ok(Outcome::Failed(detail))
        }
    }
}

/// Add the turn's reported spend to a completed or failed record. A harness that
/// reported nothing gets no `usage` key, never zeros.
fn with_usage(detail: &mut Value, usage: Option<&TurnUsage>) {
    if let Some(usage) = usage {
        detail["usage"] = json!(usage);
    }
}

/// Why a selection could not be turned into a run, with the closed code to report.
struct Rejection {
    code: &'static str,
    error: anyhow::Error,
}

fn resolve(
    pb: &Pillbox,
    request: &TextRequestV2,
) -> std::result::Result<(TextRequest, String), Rejection> {
    // Only Codex has a tool-free text driver today. The other harnesses are valid
    // selections that this build cannot run, which is a rejection, not a bad request.
    if request.agent.harness != "codex" {
        return Err(Rejection {
            code: "runtime_rejected",
            error: anyhow::anyhow!("no text driver for harness {}", request.agent.harness),
        });
    }
    let runner_image_id = runner_image_id(pb).map_err(|error| Rejection {
        code: "runtime_unavailable",
        error,
    })?;
    let lowered = request
        .codex_request(runner_image_id.clone())
        .map_err(|error| Rejection {
            code: "runtime_rejected",
            error,
        })?;
    Ok((lowered, runner_image_id))
}

/// The immutable id of the runner image this pillbox is configured to use.
fn runner_image_id(pb: &Pillbox) -> Result<String> {
    let (image, _) = crate::docker::resolve_runner_image(pb);
    let output = Command::new("docker")
        .args(["image", "inspect", &image, "--format", "{{.Id}}"])
        .output()
        .context("run docker image inspect")?;
    ensure!(
        output.status.success(),
        "runner image {image} is not available: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let id = String::from_utf8(output.stdout)?.trim().to_owned();
    if !super::valid_digest(&id) {
        bail!("Docker returned an invalid runner image id for {image}");
    }
    Ok(id)
}

/// Record a failure that happened before any VM work, so it has session evidence
/// like every later failure.
fn unresolved(
    pb: &Pillbox,
    request: &TextRequestV2,
    owner: &mut OwnedInvocation,
    rejection: Rejection,
) -> Result<Outcome> {
    let mut evidence = ExecutionEvidence::start_text(
        pb,
        &request.session_ref.session_id,
        &request.invocation_id,
        json!({
            "request_hash": owner.record().request_hash,
            "adapter_revision": ADAPTER_REVISION,
            "agent": request.agent,
        }),
    )?;
    evidence.append(Payload::Custom(Custom {
        name: "text.execution.failed".into(),
        payload: Some(json!({"error": format!("{:#}", rejection.error), "stage": "resolve"})),
    }))?;
    crate::events::emit_session_event(
        pb,
        crate::events::EventType::SessionFailed {
            reason: format!("resolve: {:#}", rejection.error),
            exit_code: Some(1),
            trace_path: None,
            result_snapshot: None,
        },
        &request.session_ref.session_id,
        None,
    );
    Ok(Outcome::Failed(json!({
        "invocation_id": request.invocation_id,
        "request_hash": owner.record().request_hash,
        "code": rejection.code,
        "stage": "resolve",
        "session_ref": evidence.reference(),
    })))
}

/// Map a failed stage to the closed code set shared with the caller. Deadlines are
/// timeouts wherever they hit; otherwise the stage decides.
fn failure_code(stage: &str, error: &anyhow::Error) -> &'static str {
    let message = format!("{error:#}");
    if message.contains("deadline exceeded") {
        return "runtime_timeout";
    }
    match stage {
        "credentials" | "image_prepare" | "guest_prepare" | "vmm_spawn" | "guest_rpc_ready" => {
            "runtime_unavailable"
        }
        "turn" => "runtime_protocol_error",
        _ => "internal_error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(harness: &str, model: &str) -> TextRequestV2 {
        let rendered_input = "hello".to_owned();
        TextRequestV2 {
            contract_version: CONTRACT_VERSION.into(),
            session_ref: SessionIdentity {
                session_id: "chat_1".into(),
            },
            invocation_id: "chat_1".into(),
            idempotency_key: "chat_1".into(),
            rendered_input_hash: digest(rendered_input.as_bytes()),
            rendered_input,
            tool_policy: "deny_all".into(),
            agent: AgentSelection {
                harness: harness.into(),
                model: model.into(),
                reasoning_effort: "low".into(),
            },
            placement: "local_microvm".into(),
            output_format: TextOutputFormat {
                kind: "text".into(),
                retry_count: 0,
            },
            limits: TextLimits {
                timeout_ms: 120_000,
                max_final_text_bytes: 32_768,
                max_frame_bytes: 1_048_576,
                max_evidence_bytes: 8_388_608,
            },
        }
    }

    #[test]
    fn every_pillbox_harness_is_a_valid_selection() {
        for harness in ["codex", "claude_code", "pi", "opencode"] {
            request(harness, "any-model").validate().unwrap();
        }
        assert!(request("custom", "any-model").validate().is_err());
    }

    #[test]
    fn a_request_cannot_pin_a_harness_version_or_image() {
        let mut value = serde_json::to_value(request("codex", "gpt-6-luna")).unwrap();
        value["runner_image_id"] = json!(format!("sha256:{}", "a".repeat(64)));
        assert!(serde_json::from_value::<TextRequestV2>(value.clone()).is_err());
        value.as_object_mut().unwrap().remove("runner_image_id");
        value["agent"]["harness_version"] = json!("0.156.1");
        assert!(serde_json::from_value::<TextRequestV2>(value).is_err());
    }

    #[test]
    fn codex_selection_lowers_onto_the_resolved_image() {
        let image = format!("sha256:{}", "b".repeat(64));
        let lowered = request("codex", "gpt-6-luna")
            .codex_request(image.clone())
            .unwrap();
        assert_eq!(lowered.runtime.runner_image_id, image);
        assert_eq!(lowered.execution.requested.profile, "luna");
        assert!(request("codex", "gpt-4").codex_request(image).is_err());
    }

    #[test]
    fn usage_is_added_only_when_reported() {
        let mut detail = json!({"invocation_id": "chat_1"});
        with_usage(&mut detail, None);
        assert_eq!(detail, json!({"invocation_id": "chat_1"}));
        let usage = TurnUsage::from_claude_result(&json!({"total_cost_usd": 0.5,
            "usage": {"input_tokens": 3, "output_tokens": 4}}));
        with_usage(&mut detail, usage.as_ref());
        assert_eq!(
            detail["usage"],
            json!({"cost_usd": 0.5, "input_tokens": 3, "output_tokens": 4})
        );
    }

    #[test]
    fn failures_map_to_the_closed_code_set() {
        let timeout = anyhow::anyhow!("bounded execution deadline exceeded");
        assert_eq!(failure_code("image_prepare", &timeout), "runtime_timeout");
        let other = anyhow::anyhow!("unsupported Codex version");
        assert_eq!(failure_code("turn", &other), "runtime_protocol_error");
        assert_eq!(failure_code("vmm_spawn", &other), "runtime_unavailable");
        assert_eq!(failure_code("finalize", &other), "internal_error");
    }
}
