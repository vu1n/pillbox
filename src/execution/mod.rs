//! Bounded repository execution is a separate protocol from managed execution/2.
#![cfg_attr(not(feature = "libkrun"), allow(dead_code))]

#[cfg(any(feature = "libkrun", test))]
pub(crate) mod evidence;
pub(crate) mod files;
#[cfg(feature = "libkrun")]
pub(crate) mod local;
#[cfg(any(feature = "libkrun", test))]
pub(crate) mod native;
pub(crate) mod protocol;
pub(crate) mod snapshot;
pub(crate) mod store;
#[cfg(any(feature = "libkrun", test))]
pub(crate) mod verifier;

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use files::{FileLimits, FileOperation, FilePolicy};

pub(crate) const CONTRACT_VERSION: &str = "pillbox.execution/3";
pub(crate) const MANIFEST_VERSION: &str = "pillbox.repository/1";
pub(crate) const ADAPTER_REVISION: &str = "pillbox/local-repository-v1";
pub(crate) const POLICY_REVISION: &str = "pillbox-local-files-v1";
pub(crate) const CREDENTIAL_REFERENCE: &str = "pillbox:codex:default";
pub(crate) const MAX_TIMEOUT_MS: u64 = 3_600_000;
pub(crate) const MAX_PATCH_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const MAX_FRAME_BYTES: u64 = 12 * 1024 * 1024;
pub(crate) const MAX_EVIDENCE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecuteRequest {
    pub(crate) contract_version: String,
    pub(crate) session_ref: SessionIdentity,
    pub(crate) invocation_id: String,
    pub(crate) idempotency_key: String,
    pub(crate) rendered_input: String,
    pub(crate) rendered_input_hash: String,
    pub(crate) tool_policy: String,
    pub(crate) execution: InvocationExecution,
    pub(crate) execution_policy_revision: String,
    pub(crate) output_format: TextOutputFormat,
    pub(crate) manifest: RepositoryManifest,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionIdentity {
    pub(crate) session_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InvocationExecution {
    pub(crate) transport: HarnessTransport,
    pub(crate) requested: ModelProfile,
    pub(crate) placement: String,
    pub(crate) context_renderer_revision: String,
    pub(crate) verifier_ref: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HarnessTransport {
    pub(crate) harness: String,
    pub(crate) transport: String,
    pub(crate) harness_version: String,
    pub(crate) adapter_revision: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelProfile {
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) profile: String,
    pub(crate) reasoning_effort: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TextOutputFormat {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    pub(crate) retry_count: u8,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepositoryManifest {
    pub(crate) contract_version: String,
    pub(crate) base: RepositoryBase,
    pub(crate) output_id: String,
    pub(crate) runner_image_id: String,
    pub(crate) scope: RepositoryScope,
    pub(crate) network_hosts: Vec<String>,
    pub(crate) limits: ExecutionLimits,
    pub(crate) verifier: Verifier,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepositoryBase {
    pub(crate) repository_id: String,
    pub(crate) object_format: String,
    pub(crate) commit: String,
    pub(crate) snapshot_digest: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepositoryScope {
    pub(crate) read_paths: Vec<String>,
    pub(crate) write_paths: Vec<String>,
    pub(crate) tool_operations: Vec<ToolOperation>,
    pub(crate) secret_refs: Vec<SecretReference>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolOperation {
    pub(crate) tool: String,
    pub(crate) operation: FileOperation,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SecretReference {
    pub(crate) secret_ref: String,
    pub(crate) purpose: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecutionLimits {
    pub(crate) timeout_ms: u64,
    pub(crate) max_tool_calls: u64,
    pub(crate) max_patch_bytes: u64,
    pub(crate) max_changed_paths: usize,
    pub(crate) max_file_bytes: u64,
    pub(crate) max_snapshot_bytes: u64,
    pub(crate) max_read_bytes: u64,
    pub(crate) max_frame_bytes: u64,
    pub(crate) max_evidence_bytes: u64,
}

impl ExecutionLimits {
    pub(crate) fn file_limits(&self) -> FileLimits {
        FileLimits {
            max_file_bytes: self.max_file_bytes,
            max_snapshot_bytes: self.max_snapshot_bytes,
            max_tool_calls: self.max_tool_calls,
            max_output_bytes: self.max_read_bytes,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Verifier {
    pub(crate) verifier_id: String,
    pub(crate) run_id: String,
    pub(crate) definition_digest: String,
    pub(crate) definition: VerifierDefinition,
}

/// Trusted source is sealed before the builder starts and is stored outside
/// its result tree. A model cannot replace the program that evaluates its work.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VerifierDefinition {
    pub(crate) runtime: String,
    pub(crate) source: String,
    pub(crate) timeout_ms: u64,
    pub(crate) max_output_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EvidenceRef {
    pub(crate) session_id: String,
    /// Inclusive positions assigned by the existing local SessionLog.
    pub(crate) seq_range: [u64; 2],
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArtifactRef {
    pub(crate) session_id: String,
    pub(crate) digest: String,
    pub(crate) bytes: u64,
    pub(crate) media_type: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AdmissionReceipt {
    pub(crate) invocation_id: String,
    pub(crate) request_hash: String,
    pub(crate) manifest_digest: String,
    pub(crate) input_snapshot_digest: String,
    pub(crate) runner_image_id: String,
    pub(crate) adapter_revision: String,
    pub(crate) policy_revision: String,
    pub(crate) effective_model_catalog_digest: String,
    pub(crate) evidence: EvidenceRef,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepositoryResult {
    pub(crate) invocation_id: String,
    pub(crate) request_hash: String,
    pub(crate) manifest_digest: String,
    pub(crate) output_id: String,
    pub(crate) base: RepositoryBase,
    pub(crate) execution: InvocationExecution,
    pub(crate) patch: ArtifactRef,
    pub(crate) result_snapshot_digest: String,
    pub(crate) snapshot_manifest: ArtifactRef,
    pub(crate) changed_paths: Vec<String>,
    pub(crate) evidence: EvidenceRef,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Verification {
    pub(crate) verifier_id: String,
    pub(crate) run_id: String,
    pub(crate) definition_digest: String,
    pub(crate) output_id: String,
    pub(crate) result_digest: String,
    pub(crate) result_snapshot_digest: String,
    pub(crate) outcome: VerificationOutcome,
    pub(crate) report: ArtifactRef,
    pub(crate) evidence: EvidenceRef,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VerificationOutcome {
    Pass,
    Fail,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Completion {
    pub(crate) admission: AdmissionReceipt,
    pub(crate) result: RepositoryResult,
    pub(crate) verification: Verification,
    pub(crate) native_evidence: ArtifactRef,
    pub(crate) text: ArtifactRef,
}

/// Coarse durable phase observations, including evidence from failed invocations.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecutionProgress {
    pub(crate) admission: AdmissionReceipt,
    pub(crate) builder_evidence: EvidenceRef,
    pub(crate) native_evidence: Option<ArtifactRef>,
    pub(crate) result: Option<RepositoryResult>,
    pub(crate) verifier_session_id: Option<String>,
    pub(crate) verifier_evidence: Option<EvidenceRef>,
}

impl ExecuteRequest {
    pub(crate) fn validate(&self) -> Result<FilePolicy> {
        ensure!(
            self.contract_version == CONTRACT_VERSION,
            "unsupported execution contract"
        );
        identity(&self.invocation_id)?;
        identity(&self.session_ref.session_id)?;
        ensure!(
            self.idempotency_key == self.invocation_id,
            "invocation/idempotency mismatch"
        );
        ensure!(
            !self.rendered_input.is_empty() && self.rendered_input.len() <= 512 * 1024,
            "rendered input must contain 1..524288 UTF-8 bytes"
        );
        ensure!(
            self.rendered_input_hash == digest(self.rendered_input.as_bytes()),
            "rendered input digest mismatch"
        );
        ensure!(
            self.tool_policy == "repository_files",
            "unsupported tool policy"
        );
        ensure!(
            self.execution_policy_revision == POLICY_REVISION,
            "unsupported execution policy revision"
        );
        let execution = &self.execution;
        ensure!(
            execution.transport.harness == "codex"
                && execution.transport.transport == "app_server"
                && execution.transport.harness_version == "0.151.0"
                && execution.transport.adapter_revision == ADAPTER_REVISION
                && execution.placement == "local_microvm",
            "unsupported exact harness or placement"
        );
        ensure!(
            execution.requested.provider == "openai",
            "unsupported provider"
        );
        ensure!(
            matches!(
                execution.requested.profile.as_str(),
                "sol" | "terra" | "luna"
            ) && execution.requested.model == format!("gpt-5.6-{}", execution.requested.profile)
                && matches!(
                    execution.requested.reasoning_effort.as_str(),
                    "low" | "medium" | "high"
                ),
            "unsupported exact model profile or reasoning effort"
        );
        ensure!(
            !execution.context_renderer_revision.is_empty()
                && execution.context_renderer_revision.len() <= 256,
            "invalid context renderer revision"
        );
        ensure!(
            self.output_format.kind == "text" && self.output_format.retry_count == 0,
            "unsupported output format or implicit retry"
        );
        let manifest = &self.manifest;
        ensure!(
            manifest.contract_version == MANIFEST_VERSION,
            "unsupported repository manifest"
        );
        identity(&manifest.base.repository_id)?;
        identity(&manifest.output_id)?;
        let oid_len = match manifest.base.object_format.as_str() {
            "sha1" => 40,
            "sha256" => 64,
            _ => bail!("unsupported Git object format"),
        };
        ensure!(
            lower_hex(&manifest.base.commit, oid_len)
                && manifest.base.commit.bytes().any(|b| b != b'0'),
            "repository base requires a full nonzero Git object ID"
        );
        ensure!(
            valid_digest(&manifest.base.snapshot_digest),
            "invalid input snapshot digest"
        );
        ensure!(
            valid_digest(&manifest.runner_image_id),
            "runner requires an exact OCI image ID"
        );
        ensure!(
            manifest.network_hosts == ["chatgpt.com"],
            "unsupported network policy"
        );
        let scope = &manifest.scope;
        ensure!(
            scope.secret_refs.len() == 1
                && scope.secret_refs[0].secret_ref == CREDENTIAL_REFERENCE
                && scope.secret_refs[0].purpose == "model",
            "unsupported credential reference or purpose"
        );
        ensure!(
            scope
                .tool_operations
                .iter()
                .all(|op| op.tool == "pillbox_repository"),
            "unsupported tool operation"
        );
        let limits = &manifest.limits;
        bounded(limits.timeout_ms, MAX_TIMEOUT_MS, "timeout_ms")?;
        bounded(limits.max_patch_bytes, MAX_PATCH_BYTES, "max_patch_bytes")?;
        bounded(
            limits.max_changed_paths as u64,
            files::MAX_FILES as u64,
            "max_changed_paths",
        )?;
        bounded(limits.max_frame_bytes, MAX_FRAME_BYTES, "max_frame_bytes")?;
        bounded(
            limits.max_evidence_bytes,
            MAX_EVIDENCE_BYTES,
            "max_evidence_bytes",
        )?;
        ensure!(
            limits.max_frame_bytes <= limits.max_evidence_bytes,
            "frame limit exceeds evidence limit"
        );
        let verifier = &manifest.verifier;
        identity(&verifier.verifier_id)?;
        identity(&verifier.run_id)?;
        ensure!(
            execution.verifier_ref == verifier.verifier_id,
            "verifier reference mismatch"
        );
        let definition = &verifier.definition;
        ensure!(
            definition.runtime == "python3",
            "unsupported verifier runtime"
        );
        ensure!(
            !definition.source.is_empty()
                && definition.source.len() <= 64 * 1024
                && !definition.source.contains('\0'),
            "invalid sealed verifier source"
        );
        bounded(
            definition.timeout_ms,
            limits.timeout_ms,
            "verifier timeout_ms",
        )?;
        bounded(
            definition.max_output_bytes,
            1024 * 1024,
            "verifier max_output_bytes",
        )?;
        ensure!(
            verifier.definition_digest == canonical_digest(definition)?,
            "verifier definition digest mismatch"
        );
        FilePolicy::new(
            scope.read_paths.clone(),
            scope.write_paths.clone(),
            scope
                .tool_operations
                .iter()
                .map(|op| op.operation)
                .collect(),
            limits.file_limits(),
        )
    }

    pub(crate) fn canonical_request(&self) -> Result<String> {
        canonical_json(&serde_json::to_value(self)?)
    }

    pub(crate) fn manifest_digest(&self) -> Result<String> {
        canonical_digest(&self.manifest)
    }
}

pub(crate) fn canonical_digest(value: &impl Serialize) -> Result<String> {
    Ok(digest(
        canonical_json(&serde_json::to_value(value)?)?.as_bytes(),
    ))
}

pub(crate) fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

pub(crate) fn valid_digest(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|hex| lower_hex(hex, 64))
}

fn lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn identity(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 128 && value.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "unsupported runtime identity; expected 1..128 ASCII letters, digits, hyphens or underscores"
    );
    Ok(())
}

fn bounded(value: u64, maximum: u64, name: &str) -> Result<()> {
    ensure!(
        value > 0 && value <= maximum,
        "{name} must be in 1..={maximum}"
    );
    Ok(())
}

pub(crate) fn canonical_json(value: &Value) -> Result<String> {
    fn write(value: &Value, output: &mut String) -> Result<()> {
        match value {
            Value::Null => output.push_str("null"),
            Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
            Value::Number(number) => {
                ensure!(
                    number.as_u64().is_some_and(|n| n <= 9_007_199_254_740_991),
                    "canonical runtime JSON supports nonnegative safe integers only"
                );
                output.push_str(&number.to_string());
            }
            Value::String(value) => output.push_str(&serde_json::to_string(value)?),
            Value::Array(values) => {
                output.push('[');
                for (index, value) in values.iter().enumerate() {
                    if index != 0 {
                        output.push(',');
                    }
                    write(value, output)?;
                }
                output.push(']');
            }
            Value::Object(values) => {
                output.push('{');
                let mut keys: Vec<_> = values.keys().collect();
                keys.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
                for (index, key) in keys.into_iter().enumerate() {
                    if index != 0 {
                        output.push(',');
                    }
                    output.push_str(&serde_json::to_string(key)?);
                    output.push(':');
                    write(&values[key], output)?;
                }
                output.push('}');
            }
        }
        Ok(())
    }
    let mut output = String::new();
    write(value, &mut output).context("canonicalize repository execution request")?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> ExecuteRequest {
        let mut request: ExecuteRequest = serde_json::from_value(serde_json::json!({
            "contract_version": CONTRACT_VERSION,
            "session_ref": {"session_id": "builder-1"},
            "invocation_id": "inv-1", "idempotency_key": "inv-1",
            "rendered_input": "Fix src/main.rs", "rendered_input_hash": digest(b"Fix src/main.rs"),
            "tool_policy": "repository_files", "execution_policy_revision": POLICY_REVISION,
            "output_format": {"type": "text", "retry_count": 0},
            "execution": {
                "transport": {"harness": "codex", "transport": "app_server", "harness_version": "0.151.0", "adapter_revision": ADAPTER_REVISION},
                "requested": {"provider": "openai", "model": "gpt-5.6-sol", "profile": "sol", "reasoning_effort": "high"},
                "placement": "local_microvm", "context_renderer_revision": "renderer-1", "verifier_ref": "test-1"
            },
            "manifest": {
                "contract_version": MANIFEST_VERSION,
                "base": {"repository_id": "repo-1", "object_format": "sha1", "commit": "1234567890123456789012345678901234567890", "snapshot_digest": digest(b"tree")},
                "output_id": "output-1", "runner_image_id": digest(b"image"),
                "scope": {
                    "read_paths": ["src/main.rs"], "write_paths": ["src/main.rs"],
                    "tool_operations": [{"tool": "pillbox_repository", "operation": "read"}, {"tool": "pillbox_repository", "operation": "write"}],
                    "secret_refs": [{"secret_ref": CREDENTIAL_REFERENCE, "purpose": "model"}]
                },
                "network_hosts": ["chatgpt.com"],
                "limits": {"timeout_ms": 300000, "max_tool_calls": 10, "max_patch_bytes": 1024, "max_changed_paths": 4, "max_file_bytes": 128, "max_snapshot_bytes": 512, "max_read_bytes": 512, "max_frame_bytes": 2048, "max_evidence_bytes": 4096},
                "verifier": {"verifier_id": "test-1", "run_id": "verify-1", "definition_digest": "pending", "definition": {"runtime": "python3", "source": "assert True\n", "timeout_ms": 30000, "max_output_bytes": 1024}}
            }
        })).unwrap();
        request.manifest.verifier.definition_digest =
            canonical_digest(&request.manifest.verifier.definition).unwrap();
        request
    }

    #[test]
    fn valid_request_binds_every_manifest_field() {
        let request = request();
        request.validate().unwrap();
        let hash = canonical_digest(&request).unwrap();
        let manifest_hash = request.manifest_digest().unwrap();
        let mut changed = request.clone();
        changed.manifest.output_id = "another-output".into();
        assert_ne!(hash, canonical_digest(&changed).unwrap());
        assert_ne!(manifest_hash, changed.manifest_digest().unwrap());
        assert_eq!(
            hash,
            digest(request.canonical_request().unwrap().as_bytes())
        );
    }

    #[test]
    fn wire_is_closed_at_every_boundary() {
        let value = serde_json::to_value(request()).unwrap();
        for path in [
            "",
            "/session_ref",
            "/execution",
            "/execution/transport",
            "/execution/requested",
            "/output_format",
            "/manifest",
            "/manifest/base",
            "/manifest/scope",
            "/manifest/scope/tool_operations/0",
            "/manifest/scope/secret_refs/0",
            "/manifest/limits",
            "/manifest/verifier",
            "/manifest/verifier/definition",
        ] {
            let mut invalid = value.clone();
            invalid
                .pointer_mut(path)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert("unknown".into(), true.into());
            assert!(
                serde_json::from_value::<ExecuteRequest>(invalid).is_err(),
                "accepted unknown field at {path}"
            );
        }
    }

    #[test]
    fn unsupported_policy_never_normalizes_to_an_admitted_request() {
        let value = serde_json::to_value(request()).unwrap();
        for (path, replacement) in [
            ("/contract_version", Value::from("pillbox.execution/2")),
            ("/idempotency_key", Value::from("another")),
            ("/rendered_input_hash", Value::from(digest(b"changed"))),
            ("/tool_policy", Value::from("runtime_default")),
            (
                "/execution/transport/harness_version",
                Value::from("0.144.5"),
            ),
            (
                "/execution/transport/adapter_revision",
                Value::from("other"),
            ),
            ("/execution/requested/provider", Value::from("other")),
            ("/execution/requested/model", Value::from("gpt-5.6-terra")),
            (
                "/execution/requested/reasoning_effort",
                Value::from("default"),
            ),
            ("/execution/placement", Value::from("managed_container")),
            ("/execution/verifier_ref", Value::from("other")),
            ("/output_format/retry_count", Value::from(2)),
            ("/manifest/base/commit", Value::from("HEAD")),
            (
                "/manifest/runner_image_id",
                Value::from("pillbox-runner:dev"),
            ),
            (
                "/manifest/network_hosts",
                serde_json::json!(["chatgpt.com", "example.com"]),
            ),
            (
                "/manifest/scope/secret_refs/0/purpose",
                Value::from("shell"),
            ),
            (
                "/manifest/scope/tool_operations/0/tool",
                Value::from("shell"),
            ),
            (
                "/manifest/scope/write_paths",
                serde_json::json!(["../escape"]),
            ),
            ("/manifest/limits/max_tool_calls", Value::from(0)),
            (
                "/manifest/limits/timeout_ms",
                Value::from(MAX_TIMEOUT_MS + 1),
            ),
            (
                "/manifest/limits/max_patch_bytes",
                Value::from(MAX_PATCH_BYTES + 1),
            ),
            (
                "/manifest/verifier/definition/source",
                Value::from("changed"),
            ),
        ] {
            let mut invalid = value.clone();
            *invalid.pointer_mut(path).unwrap() = replacement;
            let invalid: ExecuteRequest = serde_json::from_value(invalid).unwrap();
            assert!(invalid.validate().is_err(), "accepted unsupported {path}");
        }
    }

    #[test]
    fn canonical_json_matches_utf16_key_order_and_rejects_ambiguous_numbers() {
        let value = serde_json::json!({"\u{e000}": 1, "\u{10000}": 2, "a": "é\n"});
        assert_eq!(
            canonical_json(&value).unwrap(),
            "{\"a\":\"é\\n\",\"\u{10000}\":2,\"\u{e000}\":1}"
        );
        for invalid in [
            serde_json::json!(-1),
            serde_json::json!(1.5),
            serde_json::json!(9_007_199_254_740_992_u64),
        ] {
            assert!(canonical_json(&invalid).is_err());
        }
    }
}
