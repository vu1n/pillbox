//! One sealed local text invocation. Huddles owns conversation and packet authority.

use std::time::{Duration, Instant};

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::evidence::ExecutionEvidence;
use super::native::{self, NativeLimits};
use super::protocol::{self, CodexProfile};
use super::store::OwnedInvocation;
use super::{
    digest, valid_digest, ArtifactRef, EvidenceRef, HarnessTransport, ModelProfile,
    SessionIdentity, TextOutputFormat,
};
use crate::contract::{Custom, Payload};
use crate::pillbox::Pillbox;
use crate::sandbox::libkrun::repository::{self, BuilderInput, VmLimits};
use crate::startup::StartupTimer;

pub(crate) const CONTRACT_VERSION: &str = "pillbox.text/1";
pub(crate) const ADAPTER_REVISION: &str = "pillbox/local-text-v1";
pub(crate) const POLICY_REVISION: &str = "pillbox-local-text-v1";
const MAX_RENDERED_INPUT_BYTES: usize = 512 * 1024;
const MAX_FINAL_TEXT_BYTES: u64 = 1024 * 1024;
/// Host-visible stages of one text invocation, in the order they complete.
const STAGES: [&str; 6] = [
    "credentials",
    "image_prepare",
    "guest_prepare",
    "vmm_spawn",
    "guest_rpc_ready",
    "turn",
];

/// Stage timings for one text invocation. A sealed turn gets the same lifecycle
/// telemetry as a `pillbox run` session (`session.*` events with `startup_stages`,
/// OTel when configured) plus a `text.stage.completed` evidence event per stage, so
/// a slow or failed turn says where the time went.
struct Stages {
    timer: StartupTimer,
    completed: usize,
    started_emitted: bool,
}

impl Stages {
    fn start() -> Self {
        Self {
            timer: StartupTimer::start(),
            completed: 0,
            started_emitted: false,
        }
    }

    /// The stage that was running when the invocation ended.
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

    fn emit_started(&mut self, pb: &Pillbox, session_id: &str) {
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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TextExecution {
    pub(crate) transport: HarnessTransport,
    pub(crate) requested: ModelProfile,
    pub(crate) placement: String,
    pub(crate) context_renderer_revision: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TextLimits {
    pub(crate) timeout_ms: u64,
    pub(crate) max_final_text_bytes: u64,
    pub(crate) max_frame_bytes: u64,
    pub(crate) max_evidence_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TextRuntime {
    pub(crate) runner_image_id: String,
    pub(crate) effective_model_catalog_digest: String,
    pub(crate) credential_ref: String,
    pub(crate) network_hosts: Vec<String>,
    pub(crate) limits: TextLimits,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TextRequest {
    pub(crate) contract_version: String,
    pub(crate) session_ref: SessionIdentity,
    pub(crate) invocation_id: String,
    pub(crate) idempotency_key: String,
    pub(crate) rendered_input: String,
    pub(crate) rendered_input_hash: String,
    pub(crate) tool_policy: String,
    pub(crate) execution: TextExecution,
    pub(crate) execution_policy_revision: String,
    pub(crate) output_format: TextOutputFormat,
    pub(crate) runtime: TextRuntime,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TextAdmission {
    pub(crate) invocation_id: String,
    pub(crate) request_hash: String,
    pub(crate) runner_image_id: String,
    pub(crate) effective_model_catalog_digest: String,
    pub(crate) execution_policy_revision: String,
    pub(crate) evidence: EvidenceRef,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TextProgress {
    pub(crate) admission: TextAdmission,
    pub(crate) session_ref: EvidenceRef,
    pub(crate) native_evidence: Option<ArtifactRef>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TextCompletion {
    pub(crate) invocation_id: String,
    pub(crate) request_hash: String,
    pub(crate) execution_policy_revision: String,
    pub(crate) execution: TextExecution,
    pub(crate) runner_image_id: String,
    pub(crate) effective_model_catalog_digest: String,
    pub(crate) session_ref: EvidenceRef,
    pub(crate) native_evidence: ArtifactRef,
    pub(crate) text: ArtifactRef,
    pub(crate) output_text: String,
    pub(crate) native_thread_id: String,
    pub(crate) native_turn_id: String,
    pub(crate) requested_model: String,
    pub(crate) served_model: Option<String>,
}

impl TextRequest {
    pub(crate) fn validate(&self) -> Result<CodexProfile> {
        ensure!(
            self.contract_version == CONTRACT_VERSION,
            "unsupported text contract"
        );
        validate_id(&self.invocation_id)?;
        validate_id(&self.session_ref.session_id)?;
        ensure!(
            self.idempotency_key == self.invocation_id,
            "invocation/idempotency mismatch"
        );
        ensure!(
            !self.rendered_input.is_empty()
                && self.rendered_input.len() <= MAX_RENDERED_INPUT_BYTES,
            "rendered input must contain 1..524288 UTF-8 bytes"
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
            self.execution_policy_revision == POLICY_REVISION,
            "unsupported text execution policy"
        );
        ensure!(
            self.output_format.kind == "text" && self.output_format.retry_count == 0,
            "unsupported text output format"
        );
        let execution = &self.execution;
        ensure!(
            execution.transport.harness == "codex"
                && execution.transport.transport == "app_server"
                && execution.transport.harness_version == protocol::GPT6_CODEX_VERSION
                && execution.transport.adapter_revision == ADAPTER_REVISION
                && execution.placement == "local_microvm",
            "unsupported text harness or placement"
        );
        ensure!(
            execution.requested.provider == "openai"
                && execution.requested.model == "gpt-6-luna"
                && execution.requested.profile == "luna"
                && matches!(
                    execution.requested.reasoning_effort.as_str(),
                    "low" | "medium" | "high"
                ),
            "unsupported text model profile"
        );
        ensure!(
            !execution.context_renderer_revision.is_empty()
                && execution.context_renderer_revision.len() <= 256,
            "invalid context renderer revision"
        );
        let profile = CodexProfile::new_for_version(
            &execution.transport.harness_version,
            &execution.requested.model,
            &execution.requested.reasoning_effort,
        )?;
        let runtime = &self.runtime;
        ensure!(
            valid_digest(&runtime.runner_image_id),
            "invalid immutable runner image ID"
        );
        ensure!(
            runtime.effective_model_catalog_digest == profile.effective_catalog_digest(),
            "text model catalog digest mismatch"
        );
        ensure!(
            runtime.credential_ref == super::CREDENTIAL_REFERENCE,
            "unsupported text credential reference"
        );
        ensure!(
            runtime.network_hosts == ["chatgpt.com"],
            "unsupported text network policy"
        );
        let limits = &runtime.limits;
        ensure!(
            (1..=super::MAX_TIMEOUT_MS).contains(&limits.timeout_ms),
            "invalid text timeout"
        );
        ensure!(
            (1..=MAX_FINAL_TEXT_BYTES).contains(&limits.max_final_text_bytes),
            "invalid final text byte limit"
        );
        ensure!(
            (1..=super::MAX_FRAME_BYTES).contains(&limits.max_frame_bytes),
            "invalid native frame byte limit"
        );
        ensure!(
            (1..=super::MAX_EVIDENCE_BYTES).contains(&limits.max_evidence_bytes)
                && limits.max_frame_bytes <= limits.max_evidence_bytes,
            "invalid native evidence byte limit"
        );
        Ok(profile)
    }
}

fn validate_id(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')),
        "text identity must contain 1..128 ASCII letters, digits, hyphens or underscores"
    );
    Ok(())
}

/// The stage that was running when a text invocation failed, carried in the error
/// chain so a caller can report it without parsing the message.
#[derive(Debug)]
pub(crate) struct FailedStage(pub(crate) &'static str);

impl std::fmt::Display for FailedStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "text invocation failed during {}", self.0)
    }
}

pub(crate) fn execute(
    pb: &Pillbox,
    request: &TextRequest,
    owner: &mut OwnedInvocation,
) -> Result<TextCompletion> {
    execute_with(pb, request, owner, true).map(|(completion, _)| completion)
}

/// [`execute`], also returning the Codex CLI version the guest reported. With
/// `exact_cli_version` false the runner image decides the version and it is only recorded.
pub(crate) fn execute_with(
    pb: &Pillbox,
    request: &TextRequest,
    owner: &mut OwnedInvocation,
    exact_cli_version: bool,
) -> Result<(TextCompletion, Option<String>)> {
    let profile = request.validate()?;
    let profile = if exact_cli_version {
        profile
    } else {
        profile.observing_cli_version()
    };
    let deadline = Instant::now() + Duration::from_millis(request.runtime.limits.timeout_ms);
    check_live(owner, deadline)?;
    let mut evidence = ExecutionEvidence::start_text(
        pb,
        &request.session_ref.session_id,
        &request.invocation_id,
        json!({
            "request_hash": owner.record().request_hash,
            "runner_image_id": request.runtime.runner_image_id,
            "effective_model_catalog_digest": profile.effective_catalog_digest(),
            "source_model_catalog_digest": profile.source_catalog_digest(),
            "adapter_revision": ADAPTER_REVISION,
            "policy_revision": POLICY_REVISION,
        }),
    )?;
    let admission = TextAdmission {
        invocation_id: request.invocation_id.clone(),
        request_hash: owner.record().request_hash.clone(),
        runner_image_id: request.runtime.runner_image_id.clone(),
        effective_model_catalog_digest: profile.effective_catalog_digest().into(),
        execution_policy_revision: POLICY_REVISION.into(),
        evidence: evidence.reference(),
    };
    let mut progress = TextProgress {
        admission,
        session_ref: evidence.reference(),
        native_evidence: None,
    };
    owner.running(serde_json::to_value(&progress)?)?;
    let mut stages = Stages::start();
    let result = run_local(
        pb,
        request,
        owner,
        deadline,
        &profile,
        &mut evidence,
        &mut progress,
        &mut stages,
    );
    let session_id = &request.session_ref.session_id;
    stages.emit_started(pb, session_id);
    match result {
        Ok(completion) => {
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
            Ok(completion)
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
            Err(
                record_failure(error, stage, &mut evidence, owner, &mut progress)
                    .context(FailedStage(stage)),
            )
        }
    }
}

fn record_failure(
    error: anyhow::Error,
    stage: &'static str,
    evidence: &mut ExecutionEvidence,
    owner: &mut OwnedInvocation,
    progress: &mut TextProgress,
) -> anyhow::Error {
    let recorded = evidence.append(Payload::Custom(Custom {
        name: "text.execution.failed".into(),
        payload: Some(json!({"error": format!("{error:#}"), "stage": stage})),
    }));
    progress.session_ref = evidence.reference();
    let recorded = recorded.and_then(|_| owner.observe(serde_json::to_value(&progress)?));
    match recorded {
        Ok(()) => error,
        // Keep TeardownUnconfirmed in the error chain. A later evidence failure
        // cannot grant permission to seal a terminal invocation while its VM lives.
        Err(persistence) => error.context(format!(
            "failure evidence was not persisted: {persistence:#}"
        )),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_local(
    pb: &Pillbox,
    request: &TextRequest,
    owner: &mut OwnedInvocation,
    deadline: Instant,
    profile: &CodexProfile,
    evidence: &mut ExecutionEvidence,
    progress: &mut TextProgress,
    stages: &mut Stages,
) -> Result<(TextCompletion, Option<String>)> {
    let spec = crate::agents::lookup("execution", "codex")?;
    let credentials_path = spec.home_dir(pb)?.join(spec.cred_sentinel);
    let real = crate::vault::pre_refresh(&credentials_path, "codex")?
        .context("Codex token store did not return credentials")?;
    let fresh = crate::vault::providers::codex_execution::fresh_codex_credentials(
        &real,
        &request.invocation_id,
    )
    .map_err(anyhow::Error::msg)?;
    let input = BuilderInput {
        image_id: request.runtime.runner_image_id.clone(),
        codex_config: profile
            .config_toml("/home/pillbox/.codex/models.json")?
            .into_bytes(),
        model_catalog: profile.effective_catalog_json()?.into_bytes(),
        guest_auth: serde_json::to_vec(&fresh.guest_credentials)?,
        access_release: fresh.access_release,
        refresh_credentials: credentials_path,
    };
    drop(real);
    stages.complete("credentials", evidence)?;
    check_live(owner, deadline)?;
    let cancelled = || !matches!(owner.cancelled(), Ok(false));
    let mut stage_error = None;
    let launched = repository::launch_builder_staged(
        input,
        VmLimits {
            max_duration: deadline
                .checked_duration_since(Instant::now())
                .context("text deadline exceeded")?,
            max_output_bytes: request.runtime.limits.max_evidence_bytes,
            max_frame_bytes: request.runtime.limits.max_frame_bytes as usize,
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
    let native = (|| -> Result<_> {
        if let Some(error) = stage_error {
            return Err(error.context("persist text stage evidence"));
        }
        let stream = vm.connect_rpc(&cancelled)?;
        stages.complete("guest_rpc_ready", evidence)?;
        let limits = &request.runtime.limits;
        Ok(native::run_text(
            stream,
            profile,
            &request.rendered_input,
            NativeLimits {
                deadline,
                max_frame_bytes: limits.max_frame_bytes as usize,
                max_input_bytes: limits.max_evidence_bytes,
                max_output_bytes: limits.max_evidence_bytes,
                max_evidence_bytes: limits.max_evidence_bytes,
                max_tool_calls: 1,
                max_write_bytes: 1,
            },
            limits.max_final_text_bytes as usize,
            || {
                check_live(owner, deadline)?;
                vm.check_running(&cancelled)
            },
        ))
    })();
    let diagnostics = vm.diagnostics();
    vm.stop_and_reap()?;
    if matches!(&native, Ok(Ok(_))) {
        stages.complete("turn", evidence)?;
    }
    if let Ok(observed) = &native {
        let frames = match observed {
            Ok(done) => &done.evidence,
            Err(failed) => &failed.evidence,
        };
        progress.native_evidence = Some(evidence.native_frames(frames)?);
        progress.session_ref = evidence.reference();
        owner.observe(serde_json::to_value(&progress)?)?;
    }
    let diagnostics = evidence.artifact(&diagnostics?, "text/plain")?;
    evidence.append(Payload::Custom(Custom {
        name: "text.builder.stopped".into(),
        payload: Some(json!({"diagnostics": diagnostics})),
    }))?;
    let native = native?.map_err(|failure| failure.error)?;
    let cli_version = observed_cli_version(&native.evidence);
    check_live(owner, deadline)?;
    let text = evidence.artifact(native.text.as_bytes(), "text/plain;charset=utf-8")?;
    let native_evidence = progress
        .native_evidence
        .clone()
        .context("native capture is absent")?;
    evidence.append(Payload::Custom(Custom {
        name: "text.result.captured".into(),
        payload: Some(json!({
            "text": text,
            "native_evidence": native_evidence,
            "native_thread_id": native.thread_id,
            "native_turn_id": native.turn_id,
        })),
    }))?;
    progress.session_ref = evidence.reference();
    owner.observe(serde_json::to_value(&progress)?)?;
    let completion = TextCompletion {
        invocation_id: request.invocation_id.clone(),
        request_hash: owner.record().request_hash.clone(),
        execution_policy_revision: POLICY_REVISION.into(),
        execution: request.execution.clone(),
        runner_image_id: request.runtime.runner_image_id.clone(),
        effective_model_catalog_digest: profile.effective_catalog_digest().into(),
        session_ref: evidence.reference(),
        native_evidence,
        text,
        output_text: native.text,
        native_thread_id: native.thread_id,
        native_turn_id: native.turn_id,
        requested_model: profile.model().into(),
        served_model: None,
    };
    Ok((completion, cli_version))
}

/// The CLI version Codex reported in its `thread/start` response.
fn observed_cli_version(frames: &[serde_json::Value]) -> Option<String> {
    frames.iter().find_map(|frame| {
        frame["message"]["result"]["thread"]["cliVersion"]
            .as_str()
            .map(str::to_owned)
    })
}

fn check_live(owner: &OwnedInvocation, deadline: Instant) -> Result<()> {
    ensure!(!owner.cancelled()?, "text invocation cancelled");
    ensure!(
        Instant::now() < deadline,
        "text invocation deadline exceeded"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::store::{Claim, InvocationStore, Status};
    use super::*;
    use crate::pillbox::Scope;
    use crate::sandbox::libkrun::repository::TeardownUnconfirmed;

    fn request() -> TextRequest {
        let input = "sealed HCP prompt";
        TextRequest {
            contract_version: CONTRACT_VERSION.into(),
            session_ref: SessionIdentity {
                session_id: "session-1".into(),
            },
            invocation_id: "invocation-1".into(),
            idempotency_key: "invocation-1".into(),
            rendered_input: input.into(),
            rendered_input_hash: digest(input.as_bytes()),
            tool_policy: "deny_all".into(),
            execution: TextExecution {
                transport: HarnessTransport {
                    harness: "codex".into(),
                    transport: "app_server".into(),
                    harness_version: protocol::GPT6_CODEX_VERSION.into(),
                    adapter_revision: ADAPTER_REVISION.into(),
                },
                requested: ModelProfile {
                    provider: "openai".into(),
                    model: "gpt-6-luna".into(),
                    profile: "luna".into(),
                    reasoning_effort: "low".into(),
                },
                placement: "local_microvm".into(),
                context_renderer_revision: "renderer-v1".into(),
            },
            execution_policy_revision: POLICY_REVISION.into(),
            output_format: TextOutputFormat {
                kind: "text".into(),
                retry_count: 0,
            },
            runtime: TextRuntime {
                runner_image_id: format!("sha256:{}", "a".repeat(64)),
                effective_model_catalog_digest: protocol::GPT6_EFFECTIVE_CATALOG_SHA256.into(),
                credential_ref: super::super::CREDENTIAL_REFERENCE.into(),
                network_hosts: vec!["chatgpt.com".into()],
                limits: TextLimits {
                    timeout_ms: 120_000,
                    max_final_text_bytes: 65_536,
                    max_frame_bytes: 1_048_576,
                    max_evidence_bytes: 8_388_608,
                },
            },
        }
    }

    #[test]
    fn exact_luna_text_request_rejects_altered_authority_and_limits() {
        let valid = request();
        assert!(valid.validate().is_ok());
        let mut value = serde_json::to_value(&valid).unwrap();
        value["extra"] = json!(true);
        assert!(serde_json::from_value::<TextRequest>(value).is_err());

        let mut invalid = request();
        invalid.rendered_input_hash = digest(b"different");
        assert!(invalid.validate().is_err());
        let mut invalid = request();
        invalid.execution.requested.model = "gpt-6-sol".into();
        assert!(invalid.validate().is_err());
        let mut invalid = request();
        invalid.execution.requested.reasoning_effort = "ultra".into();
        assert!(invalid.validate().is_err());
        let mut invalid = request();
        invalid.runtime.effective_model_catalog_digest = digest(b"wrong catalog");
        assert!(invalid.validate().is_err());
        let mut invalid = request();
        invalid.tool_policy = "runtime_default".into();
        assert!(invalid.validate().is_err());
        let mut invalid = request();
        invalid.runtime.limits.max_evidence_bytes = 0;
        assert!(invalid.validate().is_err());
        let mut invalid = request();
        invalid.output_format.retry_count = 1;
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn failed_progress_write_cannot_mask_unconfirmed_vm_teardown_or_seal_failed() {
        let temp = tempfile::tempdir().unwrap();
        let pb = Pillbox {
            scope: Scope::Global,
            state_dir: temp.path().into(),
            meta: None,
        };
        let store = InvocationStore::new(&temp.path().join("text-executions")).unwrap();
        let Claim::Owned(mut owner) = store.claim("invocation-1", "sealed-request").unwrap() else {
            panic!()
        };
        let mut evidence = ExecutionEvidence::start_text(
            &pb,
            "session-1",
            "invocation-1",
            json!({"request_hash":owner.record().request_hash}),
        )
        .unwrap();
        let mut progress = TextProgress {
            admission: TextAdmission {
                invocation_id: "invocation-1".into(),
                request_hash: owner.record().request_hash.clone(),
                runner_image_id: request().runtime.runner_image_id,
                effective_model_catalog_digest: protocol::GPT6_EFFECTIVE_CATALOG_SHA256.into(),
                execution_policy_revision: POLICY_REVISION.into(),
                evidence: evidence.reference(),
            },
            session_ref: evidence.reference(),
            native_evidence: None,
        };
        // Still admitted: owner.observe fails after the failure event append.
        let error = record_failure(
            anyhow::Error::new(TeardownUnconfirmed),
            "image_prepare",
            &mut evidence,
            &mut owner,
            &mut progress,
        );
        assert!(error
            .to_string()
            .contains("failure evidence was not persisted"));
        assert!(error.downcast_ref::<TeardownUnconfirmed>().is_some());
        let returned = crate::commands::text::settle(&mut owner, Err(error)).unwrap_err();
        assert!(returned.downcast_ref::<TeardownUnconfirmed>().is_some());
        assert_eq!(owner.record().status, Status::Admitted);
        drop(owner);
        assert_eq!(
            store.status("invocation-1").unwrap().status,
            Status::Interrupted
        );
    }
    #[test]
    fn in_progress_stage_follows_completed_stages() {
        let mut stages = Stages::start();
        assert_eq!(stages.in_progress(), "credentials");
        stages.completed = 1;
        assert_eq!(stages.in_progress(), "image_prepare");
        stages.completed = STAGES.len();
        assert_eq!(stages.in_progress(), "finalize");
    }
}
