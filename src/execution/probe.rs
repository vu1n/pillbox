//! Operator-selected, credential-free verification of one committed project tree.
//! This probe is not a repository execution receipt or a Pillbox session.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::time::Duration;

use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};

use super::files::FileLimits;
use super::{canonical_digest, digest, snapshot, valid_digest, verifier, Verifier, MAX_TIMEOUT_MS};
use crate::sandbox::libkrun::repository::{self, VerifierInput, VmLimits};

const VERSION: &str = "pillbox.project-verifier-probe/1";
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_DIAGNOSTICS_BYTES: u64 = 64 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectVerifierProbe {
    contract_version: String,
    commit: String,
    snapshot_digest: String,
    image_id: String,
    output_id: String,
    max_duration_ms: u64,
    verifier: Verifier,
}

impl ProjectVerifierProbe {
    fn configuration(&self) -> Result<(String, String, verifier::VerifierConfiguration)> {
        ensure!(
            self.contract_version == VERSION,
            "unsupported probe contract version"
        );
        ensure!(
            valid_digest(&self.snapshot_digest),
            "invalid expected snapshot digest"
        );
        ensure!(
            valid_digest(&self.image_id),
            "probe requires a full immutable image ID"
        );
        ensure!(
            (1..=MAX_TIMEOUT_MS).contains(&self.max_duration_ms)
                && self.max_duration_ms > self.verifier.definition.timeout_ms,
            "probe outer deadline must exceed verifier timeout and stay within 1 hour"
        );
        let probe_digest = canonical_digest(self)?;
        let result_identity = ProbeResultIdentity {
            version: VERSION,
            probe_digest: &probe_digest,
            commit: &self.commit,
            output_id: &self.output_id,
            result_snapshot_digest: &self.snapshot_digest,
        };
        let result_digest = canonical_digest(&result_identity)?;
        let configuration = verifier::configuration(
            &self.verifier,
            &self.output_id,
            &result_digest,
            &self.snapshot_digest,
        )?;
        Ok((probe_digest, result_digest, configuration))
    }
}

#[derive(Serialize)]
struct ProbeResultIdentity<'a> {
    version: &'static str,
    probe_digest: &'a str,
    commit: &'a str,
    output_id: &'a str,
    result_snapshot_digest: &'a str,
}

#[derive(Serialize)]
struct Observation {
    outcome: &'static str,
    exit_code: Option<i32>,
    signal: Option<i32>,
    timed_out: bool,
    output_limited: bool,
    stdout_base64: String,
    stderr_base64: String,
}

#[derive(Serialize)]
struct ProbeOutput<'a> {
    record_kind: &'static str,
    contract_version: &'static str,
    probe_digest: &'a str,
    commit: &'a str,
    image_id: &'a str,
    max_duration_ms: u64,
    result_digest: &'a str,
    result_snapshot_digest: &'a str,
    configuration: &'a verifier::VerifierConfiguration,
    report_digest: String,
    report_bytes: usize,
    report_base64: String,
    diagnostics_digest: String,
    diagnostics_base64: String,
    diagnostics_truncated: bool,
    teardown_confirmed: bool,
    observation: Option<Observation>,
    infrastructure_error: Option<String>,
}

#[derive(Debug)]
pub(crate) struct ProbeExit;

impl std::fmt::Display for ProbeExit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("project verifier probe did not pass; see the JSON record")
    }
}

impl std::error::Error for ProbeExit {}

pub(crate) fn run(manifest_path: &Path, repository: &Path) -> Result<()> {
    ensure!(
        repository.is_absolute(),
        "probe repository must be an absolute path"
    );
    let mut bytes = Vec::new();
    File::open(manifest_path)
        .with_context(|| format!("open probe manifest {}", manifest_path.display()))?
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_MANIFEST_BYTES,
        "probe manifest exceeds 1 MiB"
    );
    let probe: ProjectVerifierProbe =
        serde_json::from_slice(&bytes).context("decode project verifier probe")?;
    let (probe_digest, result_digest, configuration) = probe.configuration()?;
    let limits = FileLimits {
        max_file_bytes: 8 * 1024 * 1024,
        max_snapshot_bytes: 64 * 1024 * 1024,
        max_tool_calls: 1,
        max_output_bytes: 64 * 1024 * 1024,
    };
    let tree = snapshot::read_git_tree(repository, &probe.commit, &limits)?;
    ensure!(
        tree.digest() == probe.snapshot_digest,
        "probe snapshot digest does not match complete committed tree"
    );
    let mut vm = repository::launch_verifier(
        VerifierInput {
            image_id: probe.image_id.clone(),
            tree,
            verifier: probe.verifier,
            configuration: configuration.clone(),
        },
        VmLimits {
            // This single deadline covers image preparation, boot, and evaluation.
            max_duration: Duration::from_millis(probe.max_duration_ms),
            max_output_bytes: MAX_DIAGNOSTICS_BYTES,
            max_frame_bytes: configuration.max_report_bytes(),
        },
        &|| false,
    )?;
    let mut report = Vec::new();
    let received = receive_report(&mut vm, &configuration, &mut report);
    let stopped = vm.stop_and_reap();
    let teardown_confirmed = stopped.is_ok();
    let diagnostics = vm.final_diagnostics();
    let mut failures = Vec::new();
    if let Err(error) = stopped {
        failures.push(format!("VM teardown unconfirmed: {error:#}"));
    }
    if diagnostics.truncated {
        failures.push("VM diagnostic output limit exceeded".into());
    }
    if let Some(error) = &diagnostics.error {
        failures.push(format!("collect VM diagnostics: {error:#}"));
    }
    if let Err(error) = received {
        failures.push(format!("receive verifier report: {error:#}"));
    }
    let error = if failures.is_empty() {
        None
    } else {
        Some(failures.join("; "))
    };
    let parsed = if error.is_none() {
        Some(verifier::parse_report(&report, &configuration))
    } else {
        None
    };
    let parse_error = parsed
        .as_ref()
        .and_then(|result| result.as_ref().err())
        .map(|error| format!("parse verifier report: {error:#}"));
    let observation = parsed.and_then(Result::ok).map(|value| Observation {
        outcome: if value.outcome == verifier::VerifierOutcome::Passed {
            "pass"
        } else {
            "fail"
        },
        exit_code: value.exit_code,
        signal: value.signal,
        timed_out: value.timed_out,
        output_limited: value.output_limited,
        stdout_base64: STANDARD.encode(value.stdout),
        stderr_base64: STANDARD.encode(value.stderr),
    });
    let passed = observation
        .as_ref()
        .is_some_and(|value| value.outcome == "pass");
    let output = ProbeOutput {
        record_kind: "manual_operator_vm_probe_not_execution_receipt",
        contract_version: VERSION,
        probe_digest: &probe_digest,
        commit: &probe.commit,
        image_id: &probe.image_id,
        max_duration_ms: probe.max_duration_ms,
        result_digest: &result_digest,
        result_snapshot_digest: &probe.snapshot_digest,
        configuration: &configuration,
        report_digest: digest(&report),
        report_bytes: report.len(),
        report_base64: STANDARD.encode(&report),
        diagnostics_digest: digest(&diagnostics.bytes),
        diagnostics_base64: STANDARD.encode(&diagnostics.bytes),
        diagnostics_truncated: diagnostics.truncated,
        teardown_confirmed,
        observation,
        infrastructure_error: error.or(parse_error),
    };
    println!(
        "{}",
        crate::paths::json_v1(vec![(
            "project_verifier_probe",
            serde_json::to_value(output)?
        )])
    );
    if passed {
        Ok(())
    } else {
        Err(ProbeExit.into())
    }
}

fn receive_report(
    vm: &mut repository::OwnedVm,
    configuration: &verifier::VerifierConfiguration,
    report: &mut Vec<u8>,
) -> Result<()> {
    let cancelled = || false;
    let mut stream = vm.connect_rpc(&cancelled)?;
    loop {
        let mut chunk = [0; 8192];
        match stream.read(&mut chunk) {
            Ok(0) => {
                ensure!(!report.is_empty(), "verifier disconnected without a report");
                return Ok(());
            }
            Ok(n) => {
                let remaining = configuration
                    .max_report_bytes()
                    .saturating_sub(report.len());
                ensure!(n <= remaining, "verifier report limit exceeded");
                report.extend_from_slice(&chunk[..n]);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                vm.check_running(&cancelled)?;
            }
            Err(error) => return Err(error).context("read verifier report"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::VerifierDefinition;
    use super::*;
    use serde_json::json;

    fn manifest() -> serde_json::Value {
        let definition = VerifierDefinition {
            runtime: "python3".into(),
            source: "assert True\n".into(),
            timeout_ms: 1_000,
            max_output_bytes: 1_024,
        };
        json!({
            "contract_version": VERSION,
            "commit": "a".repeat(40),
            "snapshot_digest": format!("sha256:{}", "b".repeat(64)),
            "image_id": format!("sha256:{}", "c".repeat(64)),
            "output_id": "probe-output",
            "max_duration_ms": 60_000,
            "verifier": {
                "verifier_id": "probe-verifier",
                "run_id": "probe-run",
                "definition_digest": canonical_digest(&definition).unwrap(),
                "definition": definition,
            }
        })
    }

    #[test]
    fn closed_probe_contract_checks_image_deadline_and_sealed_source() {
        let valid: ProjectVerifierProbe = serde_json::from_value(manifest()).unwrap();
        let (probe_digest, result_digest, configuration) = valid.configuration().unwrap();
        assert_eq!(configuration.result_digest, result_digest);
        assert_eq!(configuration.result_snapshot_digest, valid.snapshot_digest);
        assert_eq!(
            configuration.definition_digest,
            valid.verifier.definition_digest
        );
        let mut changed_output = manifest();
        changed_output["output_id"] = json!("another-output");
        let changed_output: ProjectVerifierProbe = serde_json::from_value(changed_output).unwrap();
        assert_ne!(changed_output.configuration().unwrap().1, result_digest);
        let mut changed_deadline = manifest();
        changed_deadline["max_duration_ms"] = json!(61_000);
        let changed_deadline: ProjectVerifierProbe =
            serde_json::from_value(changed_deadline).unwrap();
        let (new_probe_digest, new_result_digest, _) = changed_deadline.configuration().unwrap();
        assert_ne!(new_probe_digest, probe_digest);
        assert_ne!(new_result_digest, result_digest);
        let mut changed_image = manifest();
        changed_image["image_id"] = json!(format!("sha256:{}", "d".repeat(64)));
        let changed_image: ProjectVerifierProbe = serde_json::from_value(changed_image).unwrap();
        assert_ne!(changed_image.configuration().unwrap().0, probe_digest);
        let mut changed_source = manifest();
        changed_source["verifier"]["definition"]["source"] = json!("assert False\n");
        let definition: VerifierDefinition =
            serde_json::from_value(changed_source["verifier"]["definition"].clone()).unwrap();
        changed_source["verifier"]["definition_digest"] =
            json!(canonical_digest(&definition).unwrap());
        let changed_source: ProjectVerifierProbe = serde_json::from_value(changed_source).unwrap();
        assert_ne!(changed_source.configuration().unwrap().0, probe_digest);
        let record = ProbeOutput {
            record_kind: "manual_operator_vm_probe_not_execution_receipt",
            contract_version: VERSION,
            probe_digest: &probe_digest,
            commit: &valid.commit,
            image_id: &valid.image_id,
            max_duration_ms: valid.max_duration_ms,
            result_digest: &result_digest,
            result_snapshot_digest: &valid.snapshot_digest,
            configuration: &configuration,
            report_digest: digest(b""),
            report_bytes: 0,
            report_base64: String::new(),
            diagnostics_digest: digest(b""),
            diagnostics_base64: String::new(),
            diagnostics_truncated: false,
            teardown_confirmed: true,
            observation: None,
            infrastructure_error: None,
        };
        let encoded = serde_json::to_string(&record).unwrap();
        assert!(encoded.contains("\"max_duration_ms\":60000"));
        assert!(!encoded.contains("assert True"));
        assert!(!encoded.contains("credential_ref"));
        let mut unknown = manifest();
        unknown["credential_ref"] = json!("anything");
        assert!(serde_json::from_value::<ProjectVerifierProbe>(unknown).is_err());
        for (path, replacement) in [
            ("image_id", json!("latest")),
            ("snapshot_digest", json!("sha256:BAD")),
            ("max_duration_ms", json!(1_000)),
            ("contract_version", json!("pillbox.execution/3")),
        ] {
            let mut changed = manifest();
            changed[path] = replacement;
            let probe: ProjectVerifierProbe = serde_json::from_value(changed).unwrap();
            assert!(probe.configuration().is_err(), "{path}");
        }
        let mut changed = manifest();
        changed["verifier"]["definition"]["source"] = json!("assert False\n");
        let probe: ProjectVerifierProbe = serde_json::from_value(changed).unwrap();
        assert!(probe.configuration().is_err());
    }

    #[test]
    fn bounded_probe_file_rejects_oversize_before_snapshot_or_vm() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("probe.json");
        std::fs::write(&path, vec![b' '; MAX_MANIFEST_BYTES as usize + 1]).unwrap();
        let error = run(&path, directory.path()).unwrap_err();
        assert!(error.to_string().contains("exceeds 1 MiB"));
    }
}
