//! Managed execution/2 wire contract and validation.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::contract::Payload;
use crate::errors::PillboxError;

pub(super) const CONTRACT_VERSION: &str = "pillbox.execution/2";
pub(super) const EXECUTION_POLICY_REVISION: &str = "pillbox-managed-v2";
/// Keep the client-side request bound in lockstep with the managed Worker.
/// This is a byte bound on the serialized UTF-8 JSON body, not a character
/// bound on `rendered_input`.
pub(super) const MAX_MANAGED_REQUEST_BYTES: usize = 1024 * 1024;
pub(super) const MAX_EVIDENCE_EVENTS: usize = 2_000;
pub(super) const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
pub(super) const MAX_PAGES: usize = 20;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct PendingRequest {
    pub(super) invocation_id: String,
    pub(super) request_body: String,
    pub(super) body_sha256: String,
    pub(super) request_hash: String,
    pub(super) execution_digest: String,
    pub(super) execution_policy_revision: String,
}

#[derive(Debug)]
pub(super) struct PreparedInvocation {
    pub(super) pending: PendingRequest,
    pub(super) requested_model: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct EvidencePage {
    pub(super) from: u64,
    pub(super) events: Vec<Payload>,
    pub(super) next: Option<u64>,
    pub(super) truncated: bool,
    #[serde(default)]
    pub(super) artifact_ref: Option<ArtifactRef>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct ExecutionError {
    pub(super) code: ExecutionErrorCode,
    pub(super) message: String,
    #[serde(default)]
    pub(super) existing_request_hash: Option<String>,
    #[serde(default)]
    pub(super) requested_request_hash: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum ExecutionErrorCode {
    IdempotencyConflict,
    ManagedDisabled,
    UnsupportedExecution,
    UnsupportedPolicy,
    AuthUnavailable,
    RuntimeUnavailable,
    RuntimeBusy,
    RuntimeInterrupted,
    RuntimeFailed,
    Cancelled,
    StructuredOutputMissing,
}

impl ExecutionErrorCode {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::IdempotencyConflict => "idempotency_conflict",
            Self::ManagedDisabled => "managed_disabled",
            Self::UnsupportedExecution => "unsupported_execution",
            Self::UnsupportedPolicy => "unsupported_policy",
            Self::AuthUnavailable => "auth_unavailable",
            Self::RuntimeUnavailable => "runtime_unavailable",
            Self::RuntimeBusy => "runtime_busy",
            Self::RuntimeInterrupted => "runtime_interrupted",
            Self::RuntimeFailed => "runtime_failed",
            Self::Cancelled => "cancelled",
            Self::StructuredOutputMissing => "structured_output_missing",
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct ArtifactRef {
    pub(super) key: String,
    pub(super) media_type: String,
    pub(super) bytes: u64,
    pub(super) sha256: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct ExecutionAttribution {
    pub(super) harness: String,
    pub(super) transport: String,
    pub(super) requested_model: String,
    pub(super) served_model: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct ExecutionSessionRef {
    pub(super) session_id: String,
    #[serde(default)]
    pub(super) seq_range: Option<[u64; 2]>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct ExecutionOutput {
    #[serde(default)]
    pub(super) text: Option<String>,
    #[serde(default)]
    pub(super) json: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(super) enum ExecutionStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
    Conflict,
}

impl ExecutionStatus {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
            Self::Conflict => "conflict",
        }
    }

    pub(super) fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(super) enum Disposition {
    Created,
    Reused,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ExecutionResult {
    pub(super) disposition: Disposition,
    pub(super) invocation_id: String,
    pub(super) request_hash: String,
    pub(super) execution_digest: String,
    pub(super) execution_policy_revision: String,
    pub(super) attribution: ExecutionAttribution,
    pub(super) session_ref: ExecutionSessionRef,
    pub(super) status: ExecutionStatus,
    pub(super) evidence: EvidencePage,
    #[serde(default)]
    pub(super) cost: Option<crate::cost::RunCostEnvelope>,
    #[serde(default)]
    pub(super) error: Option<ExecutionError>,
    #[serde(default)]
    pub(super) output: Option<ExecutionOutput>,
    #[serde(default)]
    pub(super) retry_after_ms: Option<u64>,
}

pub(super) fn validate_running(
    result: &ExecutionResult,
    session_id: &str,
    prepared: &PreparedInvocation,
) -> Result<()> {
    validate_identity(result, session_id, prepared)?;
    validate_evidence_page(&result.evidence, 0, None)?;
    if result.disposition != Disposition::Reused
        || result.cost.is_some()
        || result.error.is_some()
        || result.output.is_some()
        || result.session_ref.seq_range.is_some()
        || result.evidence.artifact_ref.is_some()
        || !(1..=600_000).contains(&result.retry_after_ms.unwrap_or(0))
    {
        return Err(PillboxError::runtime(
            "session send",
            "managed running response violated the execution/2 schema",
        )
        .into());
    }
    Ok(())
}

pub(super) fn validate_terminal(
    result: &ExecutionResult,
    session_id: &str,
    prepared: &PreparedInvocation,
    expected_from: u64,
) -> Result<ArtifactRef> {
    validate_identity(result, session_id, prepared)?;
    if !result.status.is_terminal() || result.status == ExecutionStatus::Conflict {
        return Err(PillboxError::runtime(
            "session send",
            "managed invocation returned a non-terminal or conflicting result",
        )
        .into());
    }
    let cost = result.cost.as_ref().ok_or_else(|| {
        PillboxError::runtime(
            "session send",
            "managed terminal response omitted its required cost envelope",
        )
    })?;
    if !cost.validate_untrusted(result.status.as_str()) {
        return Err(PillboxError::runtime(
            "session send",
            "managed execution returned an invalid cost envelope",
        )
        .into());
    }
    match result.status {
        ExecutionStatus::Completed
            if result.output.is_none()
                || result.error.is_some()
                || result.retry_after_ms.is_some() =>
        {
            return Err(PillboxError::runtime(
                "session send",
                "managed completed response violated the execution/2 terminal schema",
            )
            .into());
        }
        ExecutionStatus::Failed | ExecutionStatus::Cancelled | ExecutionStatus::Interrupted
            if result.error.is_none()
                || result.output.is_some()
                || result.retry_after_ms.is_some() =>
        {
            return Err(PillboxError::runtime(
                "session send",
                "managed error response violated the execution/2 terminal schema",
            )
            .into());
        }
        _ => {}
    }
    let error_status_matches = match (result.status, result.error.as_ref().map(|e| e.code)) {
        (ExecutionStatus::Completed, None) => true,
        (ExecutionStatus::Cancelled, Some(ExecutionErrorCode::Cancelled)) => true,
        (ExecutionStatus::Interrupted, Some(ExecutionErrorCode::RuntimeInterrupted)) => true,
        (ExecutionStatus::Failed, Some(code)) => !matches!(
            code,
            ExecutionErrorCode::Cancelled
                | ExecutionErrorCode::RuntimeInterrupted
                | ExecutionErrorCode::IdempotencyConflict
        ),
        _ => false,
    };
    if !error_status_matches {
        return Err(PillboxError::runtime(
            "session send",
            "managed terminal status and error disposition mismatch",
        )
        .into());
    }
    if result.error.as_ref().is_some_and(|error| {
        error.existing_request_hash.is_some() || error.requested_request_hash.is_some()
    }) {
        return Err(PillboxError::runtime(
            "session send",
            "managed non-conflict response carried conflict-only identity fields",
        )
        .into());
    }
    let seq_range = result.session_ref.seq_range;
    let total = match seq_range {
        Some([0, end]) if end < MAX_EVIDENCE_EVENTS as u64 => Some(end + 1),
        None => Some(0),
        _ => None,
    }
    .ok_or_else(|| {
        PillboxError::runtime(
            "session send",
            "managed terminal response returned an invalid positional session range",
        )
    })?;
    if result.status == ExecutionStatus::Completed && total == 0 {
        return Err(PillboxError::runtime(
            "session send",
            "managed completed response has no immutable positional evidence",
        )
        .into());
    }
    validate_evidence_page(&result.evidence, expected_from, Some(total))?;
    validate_payloads(&result.evidence.events)?;
    let artifact = result.evidence.artifact_ref.clone().ok_or_else(|| {
        PillboxError::runtime(
            "session send",
            "managed terminal response omitted its stable artifact reference",
        )
    })?;
    validate_artifact_ref(&artifact, prepared)?;
    Ok(artifact)
}

pub(super) fn validate_identity(
    result: &ExecutionResult,
    session_id: &str,
    prepared: &PreparedInvocation,
) -> Result<()> {
    if result.invocation_id != prepared.pending.invocation_id
        || result.request_hash != prepared.pending.request_hash
        || result.execution_digest != prepared.pending.execution_digest
        || result.execution_policy_revision != prepared.pending.execution_policy_revision
        || result.session_ref.session_id != session_id
        || result.attribution.harness != "opencode"
        || result.attribution.transport != "http"
        || result.attribution.requested_model != prepared.requested_model
        || result
            .attribution
            .served_model
            .as_ref()
            .is_some_and(|model| model.is_empty() || model.len() > 256)
    {
        return Err(PillboxError::runtime(
            "session send",
            "managed execution response identity, policy, or attribution mismatch",
        )
        .into());
    }
    Ok(())
}

pub(super) fn validate_evidence_page(
    page: &EvidencePage,
    expected_from: u64,
    total: Option<u64>,
) -> Result<()> {
    let end = page
        .from
        .checked_add(page.events.len() as u64)
        .ok_or_else(|| {
            PillboxError::runtime("session send", "managed evidence cursor overflowed")
        })?;
    let valid_cursor = page.from == expected_from
        && match page.next {
            Some(next) => page.truncated && next == end && total.is_none_or(|total| end < total),
            None => !page.truncated && total.is_none_or(|total| end == total),
        };
    if !valid_cursor {
        return Err(PillboxError::runtime(
            "session send",
            "managed evidence cursor did not equal from + events.length",
        )
        .into());
    }
    Ok(())
}

pub(super) fn validate_artifact_ref(
    artifact: &ArtifactRef,
    prepared: &PreparedInvocation,
) -> Result<()> {
    let invocation_digest = format!(
        "{:x}",
        Sha256::digest(prepared.pending.invocation_id.as_bytes())
    );
    let expected_key = format!(
        "executions/{invocation_digest}/{}.json",
        prepared
            .pending
            .request_hash
            .strip_prefix("sha256:")
            .expect("validated request digest")
    );
    if artifact.key != expected_key
        || artifact.media_type != "application/json"
        || artifact.bytes == 0
        || artifact.bytes > MAX_RESPONSE_BYTES as u64
        || !valid_sha256(&artifact.sha256)
    {
        return Err(PillboxError::runtime(
            "session send",
            "managed execution returned an invalid artifact reference",
        )
        .into());
    }
    Ok(())
}

pub(super) fn validate_pending(pending: &PendingRequest) -> Result<()> {
    validate_request_body(&pending.request_body)?;
    let request: serde_json::Value =
        serde_json::from_str(&pending.request_body).map_err(|error| {
            PillboxError::config(
                "session send",
                format!("pending managed request is invalid JSON: {error}"),
            )
        })?;
    let execution = request.get("execution").cloned().ok_or_else(|| {
        PillboxError::config("session send", "pending managed request omitted execution")
    })?;
    let valid = valid_sha256(&pending.body_sha256)
        && valid_sha256(&pending.request_hash)
        && valid_sha256(&pending.execution_digest)
        && pending.body_sha256 == request_sha256(&pending.request_body)
        && pending.request_hash == canonical_sha256(&request)?
        && pending.execution_digest
            == canonical_sha256(&serde_json::json!({
                "execution": execution,
                "execution_policy_revision": pending.execution_policy_revision,
            }))?
        && pending.execution_policy_revision == EXECUTION_POLICY_REVISION
        && request["invocation_id"] == pending.invocation_id
        && request["idempotency_key"] == pending.invocation_id
        && request
            .get("rendered_input")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|rendered_input| !rendered_input.is_empty());
    if !valid {
        return Err(PillboxError::config(
            "session send",
            "pending managed invocation identity is corrupted or mismatched",
        )
        .into());
    }
    Ok(())
}

pub(super) fn validate_rendered_input(rendered_input: &str) -> Result<()> {
    if rendered_input.is_empty() {
        return Err(PillboxError::config(
            "session send",
            "managed rendered_input must be a non-empty string",
        )
        .into());
    }
    Ok(())
}

pub(super) fn validate_request_body(body: &str) -> Result<()> {
    if body.len() > MAX_MANAGED_REQUEST_BYTES {
        return Err(PillboxError::config(
            "session send",
            format!("managed request body exceeds {MAX_MANAGED_REQUEST_BYTES} bytes"),
        )
        .into());
    }
    Ok(())
}

pub(super) fn request_sha256(body: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(body.as_bytes()))
}

pub(super) fn canonical_sha256(value: &serde_json::Value) -> Result<String> {
    fn write(value: &serde_json::Value, output: &mut String) -> Result<()> {
        match value {
            serde_json::Value::Null => output.push_str("null"),
            serde_json::Value::Bool(value) => {
                output.push_str(if *value { "true" } else { "false" })
            }
            serde_json::Value::Number(value) => output.push_str(&value.to_string()),
            serde_json::Value::String(value) => {
                output.push_str(&serde_json::to_string(value).context("canonicalize JSON string")?)
            }
            serde_json::Value::Array(values) => {
                output.push('[');
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        output.push(',');
                    }
                    write(value, output)?;
                }
                output.push(']');
            }
            serde_json::Value::Object(values) => {
                output.push('{');
                let mut keys: Vec<_> = values.keys().collect();
                keys.sort_unstable();
                for (index, key) in keys.into_iter().enumerate() {
                    if index > 0 {
                        output.push(',');
                    }
                    output.push_str(&serde_json::to_string(key).context("canonicalize JSON key")?);
                    output.push(':');
                    write(&values[key], output)?;
                }
                output.push('}');
            }
        }
        Ok(())
    }
    let mut canonical = String::new();
    write(value, &mut canonical)?;
    Ok(request_sha256(&canonical))
}

pub(super) fn valid_sha256(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

pub(super) fn validate_payloads(payloads: &[Payload]) -> Result<()> {
    if payloads.iter().all(|payload| match payload {
        Payload::MessageStart(_)
        | Payload::MessageDelta(_)
        | Payload::MessageEnd(_)
        | Payload::ToolCall(_)
        | Payload::Thinking(_) => true,
        Payload::Usage(usage) => {
            [
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read_input_tokens,
                usage.cache_creation_input_tokens,
            ]
            .into_iter()
            .flatten()
            .all(|tokens| tokens <= 10_000_000_000)
                && usage
                    .cost_usd
                    .is_none_or(|cost| cost.is_finite() && (0.0..=1_000_000.0).contains(&cost))
        }
        _ => false,
    }) {
        return Ok(());
    }
    Err(PillboxError::runtime(
        "session send",
        "managed execution returned a disallowed evidence event",
    )
    .into())
}
