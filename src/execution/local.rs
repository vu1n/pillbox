//! Orchestrates one admitted local invocation; the VM owns no repository files.

use std::io::{self, Read};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{ensure, Context, Result};
use serde_json::json;

use super::evidence::ExecutionEvidence;
use super::files::{FileBroker, FileTree};
use super::native::{self, NativeLimits};
use super::protocol::{self, CodexProfile};
use super::store::OwnedInvocation;
use super::{snapshot, verifier, *};
use crate::contract::{Custom, Payload, RunFinished, Scored};
use crate::pillbox::Pillbox;
use crate::sandbox::libkrun::repository::{self, BuilderInput, VerifierInput, VmLimits};

pub(crate) fn execute(
    pb: &Pillbox,
    repository: &Path,
    request: &ExecuteRequest,
    owner: &mut OwnedInvocation,
) -> Result<Completion> {
    let deadline = Instant::now() + Duration::from_millis(request.manifest.limits.timeout_ms);
    let policy = request.validate()?;
    let base = snapshot::read_git_tree(
        repository,
        &request.manifest.base.commit,
        &request.manifest.limits.file_limits(),
    )?;
    ensure!(
        base.digest() == request.manifest.base.snapshot_digest,
        "input snapshot digest mismatch"
    );
    check_live(owner, deadline)?;
    let manifest_digest = request.manifest_digest()?;
    let request_hash = owner.record().request_hash.clone();
    let mut evidence = ExecutionEvidence::start(
        pb,
        &request.session_ref.session_id,
        &request.invocation_id,
        json!({
            "request_hash": request_hash,
            "manifest_digest": manifest_digest,
            "input_snapshot_digest": base.digest(),
            "runner_image_id": request.manifest.runner_image_id,
            "adapter_revision": ADAPTER_REVISION,
            "policy_revision": POLICY_REVISION,
            "effective_model_catalog_digest": protocol::EFFECTIVE_CATALOG_SHA256,
            "source_model_catalog_digest": protocol::SOURCE_CATALOG_SHA256,
        }),
    )?;
    let admission = AdmissionReceipt {
        invocation_id: request.invocation_id.clone(),
        request_hash,
        manifest_digest,
        input_snapshot_digest: base.digest().into(),
        runner_image_id: request.manifest.runner_image_id.clone(),
        adapter_revision: ADAPTER_REVISION.into(),
        policy_revision: POLICY_REVISION.into(),
        effective_model_catalog_digest: protocol::EFFECTIVE_CATALOG_SHA256.into(),
        evidence: evidence.reference(),
    };
    owner.running(serde_json::to_value(&admission)?)?;
    let result = run_builder(
        pb,
        request,
        owner,
        deadline,
        base,
        policy,
        &mut evidence,
        admission,
    );
    if let Err(error) = &result {
        // Native success is never inferred from a model response. This is a runtime
        // failure observation; the producer's real frames remain a separate artifact.
        let recorded = evidence.append(custom(
            "repository.execution.failed",
            json!({"error": format!("{error:#}")}),
        ));
        if let Err(persistence) = recorded {
            // Preserve an unconfirmed teardown as the root error: callers must not
            // turn a logging failure into permission to commit terminal state.
            return Err(result.unwrap_err().context(format!(
                "failure evidence was not persisted: {persistence:#}"
            )));
        }
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn run_builder(
    pb: &Pillbox,
    request: &ExecuteRequest,
    owner: &OwnedInvocation,
    deadline: Instant,
    base: FileTree,
    policy: files::FilePolicy,
    evidence: &mut ExecutionEvidence,
    admission: AdmissionReceipt,
) -> Result<Completion> {
    check_live(owner, deadline)?;
    let spec = crate::agents::lookup("execution", "codex")?;
    let credentials_path = spec.home_dir(pb)?.join(spec.cred_sentinel);
    let real = crate::vault::pre_refresh(&credentials_path, "codex")?
        .context("Codex token store did not return credentials")?;
    let fresh = crate::vault::providers::codex_execution::fresh_codex_credentials(
        &real,
        &request.invocation_id,
    )
    .map_err(anyhow::Error::msg)?;
    let profile = CodexProfile::new(
        &request.execution.requested.model,
        &request.execution.requested.reasoning_effort,
    )?;
    evidence.append(custom(
        "repository.profile.configured",
        json!({
            "model": profile.model(), "reasoning_effort": profile.effort(),
            "provider": protocol::PROVIDER_ID, "codex_version": protocol::CODEX_VERSION,
        }),
    ))?;
    let input = BuilderInput {
        image_id: request.manifest.runner_image_id.clone(),
        codex_config: profile
            .config_toml("/home/pillbox/.codex/models.json")?
            .into_bytes(),
        model_catalog: protocol::effective_catalog_json()?.into_bytes(),
        guest_auth: serde_json::to_vec(&fresh.guest_credentials)?,
        access_release: fresh.access_release,
        refresh_credentials: credentials_path,
    };
    drop(real);
    check_live(owner, deadline)?;
    // An unreadable cancellation record fails closed. check_live retains its actual
    // error at the orchestration boundary instead of treating it as no cancellation.
    let cancelled = || !matches!(owner.cancelled(), Ok(false));
    let mut vm = repository::launch_builder(input, vm_limits(request, deadline)?, &cancelled)?;
    let mut broker = FileBroker::new(base.clone(), policy)?;
    let operations: Vec<_> = request
        .manifest
        .scope
        .tool_operations
        .iter()
        .map(|tool| tool.operation)
        .collect();
    let limits = &request.manifest.limits;
    let native = (|| -> Result<_> {
        let stream = vm.connect_rpc(&cancelled)?;
        let result = native::run(
            stream,
            &profile,
            &request.rendered_input,
            &operations,
            &mut broker,
            NativeLimits {
                deadline,
                max_frame_bytes: limits.max_frame_bytes as usize,
                max_input_bytes: limits.max_evidence_bytes,
                max_output_bytes: limits.max_evidence_bytes,
                max_evidence_bytes: limits.max_evidence_bytes,
                max_tool_calls: limits.max_tool_calls,
                max_write_bytes: limits.max_file_bytes,
            },
            || {
                check_live(owner, deadline)?;
                vm.check_running(&cancelled)
            },
        );
        Ok(result)
    })();
    let diagnostics = vm.diagnostics();
    vm.stop_and_reap()?;
    // Capture starts only after confirmed producer termination, including errors.
    let diagnostics = evidence.artifact(&diagnostics?, "text/plain")?;
    evidence.append(custom(
        "repository.builder.stopped",
        json!({"diagnostics": diagnostics}),
    ))?;
    let native = native?;
    let frames = match &native {
        Ok(done) => &done.evidence,
        Err(failed) => &failed.evidence,
    };
    let native_evidence = evidence.native_frames(frames)?;
    let native = native.map_err(|failure| failure.error)?;
    check_live(owner, deadline)?;
    let usage = broker.usage();
    evidence.append(custom(
        "repository.tools.observed",
        json!({
            "tool_calls": usage.tool_calls, "output_bytes": usage.output_bytes,
        }),
    ))?;
    let result = broker.finish()?;
    ensure!(
        result.base_digest == base.digest(),
        "broker base identity changed"
    );
    ensure!(
        result.changed_paths.len() <= limits.max_changed_paths,
        "changed-path limit exceeded"
    );
    let patch = snapshot::capture_patch(&base, &result.tree, limits.max_patch_bytes)?;
    let patch = evidence.artifact(&patch, "text/x-diff")?;
    let snapshot_manifest = evidence.snapshot(&result.tree)?;
    let text = evidence.artifact(native.text.as_bytes(), "text/plain; charset=utf-8")?;
    evidence.append(custom(
        "repository.result.captured",
        json!({
            "output_id": request.manifest.output_id, "patch": patch,
            "result_snapshot_digest": result.result_digest, "snapshot_manifest": snapshot_manifest,
            "changed_paths": result.changed_paths, "text": text,
            "native_thread_id": native.thread_id, "native_turn_id": native.turn_id,
            "native_evidence": native_evidence,
        }),
    ))?;
    evidence.append(Payload::RunFinished(RunFinished {
        result_snapshot: result.result_digest.clone(),
        exit_code: 0,
        served_model: None,
        effective_limits: None,
    }))?;
    let captured = RepositoryResult {
        invocation_id: request.invocation_id.clone(),
        request_hash: admission.request_hash.clone(),
        manifest_digest: admission.manifest_digest.clone(),
        output_id: request.manifest.output_id.clone(),
        base: request.manifest.base.clone(),
        execution: request.execution.clone(),
        patch,
        result_snapshot_digest: result.result_digest,
        snapshot_manifest,
        changed_paths: result.changed_paths,
        evidence: evidence.reference(),
    };
    let verification = run_verifier(pb, request, owner, deadline, &captured, result.tree)?;
    Ok(Completion {
        admission,
        result: captured,
        verification,
        native_evidence,
        text,
    })
}

fn run_verifier(
    pb: &Pillbox,
    request: &ExecuteRequest,
    owner: &OwnedInvocation,
    deadline: Instant,
    captured: &RepositoryResult,
    tree: FileTree,
) -> Result<Verification> {
    check_live(owner, deadline)?;
    let sealed = &request.manifest.verifier;
    let result_digest = canonical_digest(captured)?;
    let configuration =
        verifier::configuration(sealed, &captured.output_id, &result_digest, tree.digest())?;
    let session_id = crate::session::Session::new_id();
    ensure!(
        session_id != request.session_ref.session_id,
        "verifier session collision"
    );
    let mut evidence = ExecutionEvidence::start(
        pb,
        &session_id,
        &sealed.run_id,
        serde_json::to_value(&configuration)?,
    )?;
    let cancelled = || !matches!(owner.cancelled(), Ok(false));
    let mut vm = repository::launch_verifier(
        VerifierInput {
            image_id: request.manifest.runner_image_id.clone(),
            tree,
            verifier: sealed.clone(),
            configuration: configuration.clone(),
        },
        vm_limits(request, deadline)?,
        &cancelled,
    )?;
    let report = (|| -> Result<Vec<u8>> {
        let mut stream = vm.connect_rpc(&cancelled)?;
        let mut bytes = Vec::new();
        loop {
            check_live(owner, deadline)?;
            let mut chunk = [0; 8192];
            match stream.read(&mut chunk) {
                Ok(0) => {
                    ensure!(!bytes.is_empty(), "verifier disconnected without a report");
                    return Ok(bytes);
                }
                Ok(n) => {
                    ensure!(
                        bytes.len() + n <= configuration.max_report_bytes(),
                        "verifier report limit exceeded"
                    );
                    bytes.extend_from_slice(&chunk[..n]);
                    if bytes.contains(&b'\n') {
                        return Ok(bytes);
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) =>
                {
                    // A verifier may exit immediately after sending its report.
                    // Drain queued bytes before interpreting process exit.
                    vm.check_running(&cancelled)?;
                }
                Err(error) => return Err(error).context("receive independent verifier report"),
            }
        }
    })();
    let diagnostics = vm.diagnostics();
    vm.stop_and_reap()?;
    let diagnostics = evidence.artifact(&diagnostics?, "text/plain")?;
    evidence.append(custom(
        "repository.verifier.stopped",
        json!({"diagnostics": diagnostics}),
    ))?;
    let report = report?;
    let artifact = evidence.artifact(&report, "application/json")?;
    let observation = verifier::parse_report(&report, &configuration)?;
    let passed = observation.outcome == verifier::VerifierOutcome::Passed;
    evidence.append(custom(
        "repository.verifier.observed",
        json!({
            "report": artifact, "configuration": configuration,
            "exit_code": observation.exit_code, "signal": observation.signal,
            "timed_out": observation.timed_out, "output_limited": observation.output_limited,
        }),
    ))?;
    evidence.append(Payload::Scored(Scored {
        grader: sealed.verifier_id.clone(),
        passed,
        score: if passed { 1.0 } else { 0.0 },
        feedback: format!("Sealed verifier report: {}", artifact.digest),
        criteria: vec![],
    }))?;
    check_live(owner, deadline)?;
    Ok(Verification {
        verifier_id: sealed.verifier_id.clone(),
        run_id: sealed.run_id.clone(),
        definition_digest: sealed.definition_digest.clone(),
        output_id: captured.output_id.clone(),
        result_digest,
        result_snapshot_digest: captured.result_snapshot_digest.clone(),
        outcome: if passed {
            VerificationOutcome::Pass
        } else {
            VerificationOutcome::Fail
        },
        report: artifact,
        evidence: evidence.reference(),
    })
}

fn custom(name: &str, payload: serde_json::Value) -> Payload {
    Payload::Custom(Custom {
        name: name.into(),
        payload: Some(payload),
    })
}

fn check_live(owner: &OwnedInvocation, deadline: Instant) -> Result<()> {
    ensure!(!owner.cancelled()?, "repository invocation cancelled");
    ensure!(
        Instant::now() < deadline,
        "repository invocation deadline exceeded"
    );
    Ok(())
}

fn vm_limits(request: &ExecuteRequest, deadline: Instant) -> Result<VmLimits> {
    Ok(VmLimits {
        max_duration: deadline
            .checked_duration_since(Instant::now())
            .context("execution deadline exceeded")?,
        max_output_bytes: request.manifest.limits.max_evidence_bytes,
        max_frame_bytes: request.manifest.limits.max_frame_bytes as usize,
    })
}
