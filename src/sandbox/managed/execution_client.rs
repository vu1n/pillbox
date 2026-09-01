//! One bounded managed HTTP execution and its crash-idempotent local commit.

use std::io::Read as _;

use anyhow::{Context, Result};

use crate::contract::{Custom, Event, Payload};
use crate::errors::PillboxError;
use crate::events::log::SessionLog;
use crate::pillbox::Pillbox;

use super::execution_contract::*;
use super::pending_journal::{CommitIntent, PendingJournal, PendingState};

#[derive(Debug)]
enum PostJsonError {
    LostResponse(anyhow::Error),
    NotFound,
    Rejected(anyhow::Error),
}

impl PostJsonError {
    fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::LostResponse(error) | Self::Rejected(error) => error,
            Self::NotFound => {
                PillboxError::runtime("session send", "managed execution status returned HTTP 404")
                    .into()
            }
        }
    }
}

pub(super) fn execute_turn(
    resolved: &Pillbox,
    session_id: &str,
    endpoint: &str,
    capability_secret: &str,
    text: &str,
    model: Option<&str>,
) -> Result<()> {
    let model = model.unwrap_or(crate::sandbox::opencode::DEFAULT_MODEL);
    let (provider, model_id) = model.split_once('/').ok_or_else(|| {
        PillboxError::config(
            "session send",
            format!("managed model must be provider/model, got `{model}`"),
        )
    })?;
    let journal = PendingJournal::open(resolved, session_id)?;
    let existing = journal.load()?;
    let invocation_id = existing
        .as_ref()
        .map(|state| state.request().invocation_id.clone())
        .unwrap_or_else(crate::session::Session::new_id);
    let rendered_hash = request_sha256(text);
    let request = serde_json::json!({
        "contract_version": CONTRACT_VERSION,
        "session_ref": { "session_id": session_id },
        "invocation_id": invocation_id,
        "idempotency_key": invocation_id,
        "rendered_input": text,
        "rendered_input_hash": rendered_hash,
        "tool_policy": "deny_all",
        "execution": {
            "transport": {
                "harness": "opencode",
                "transport": "http",
                "harness_version": "managed-v2",
                "adapter_revision": "pillbox-cli-v2"
            },
            "requested": {
                "provider": provider,
                "model": model_id,
                "profile": null,
                "reasoning_effort": "medium"
            },
            "placement": "managed_container",
            "context_renderer_revision": "pillbox-cli-v2"
        },
        "execution_policy_revision": EXECUTION_POLICY_REVISION,
        "output_format": { "type": "text", "retry_count": 0 }
    });
    let body = serde_json::to_string(&request).context("serialize managed execution request")?;
    let prepared = PreparedInvocation {
        pending: PendingRequest {
            invocation_id: invocation_id.clone(),
            body_sha256: request_sha256(&body),
            request_hash: canonical_sha256(&request)?,
            execution_digest: canonical_sha256(&serde_json::json!({
                "execution": request["execution"].clone(),
                "execution_policy_revision": EXECUTION_POLICY_REVISION,
            }))?,
            execution_policy_revision: EXECUTION_POLICY_REVISION.into(),
            request_body: body,
        },
        requested_model: model.to_string(),
    };
    let resumed = existing.is_some();
    if let Some(existing) = existing {
        validate_pending(existing.request())?;
        if existing.request() != &prepared.pending {
            return Err(PillboxError::runtime(
                "session send",
                format!(
                    "managed session has unresolved invocation `{}` with a different canonical request identity",
                    existing.request().invocation_id
                ),
            )
            .with_next("retry the exact pending turn before sending different input")
            .into());
        }
        if let Some(commit) = existing.commit() {
            return recover_commit(resolved, session_id, &journal, commit);
        }
    } else {
        journal.persist(&PendingState::prepared(prepared.pending.clone()))?;
    }

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .context("build managed execution http client")?;
    let execute_url = format!("{}/v2/executions", endpoint.trim_end_matches('/'));
    // A surviving marker means the prior POST may have sampled the model.
    // Query its stable identity before reusing the exact request bytes.
    let (mut result, mut response_bytes) = if resumed {
        match status(&client, endpoint, capability_secret, &prepared, 0)? {
            Some(response) => response,
            None => execute_with_reconciliation(
                &client,
                &execute_url,
                endpoint,
                capability_secret,
                session_id,
                &prepared,
            )?,
        }
    } else {
        execute_with_reconciliation(
            &client,
            &execute_url,
            endpoint,
            capability_secret,
            session_id,
            &prepared,
        )?
    };

    while result.status == ExecutionStatus::Running {
        validate_running(&result, session_id, &prepared)?;
        let retry_after = result.retry_after_ms.expect("validated retry_after_ms");
        std::thread::sleep(std::time::Duration::from_millis(retry_after.min(5_000)));
        let (next, bytes) = status(&client, endpoint, capability_secret, &prepared, 0)?
            .ok_or_else(|| {
                PillboxError::runtime(
                    "session send",
                    "managed invocation disappeared while reconciling its pending response",
                )
            })?;
        response_bytes = add_response_bytes(response_bytes, bytes)?;
        result = next;
    }

    let terminal = validate_terminal(&result, session_id, &prepared, 0)?;
    let terminal_cost = result.cost.clone().expect("validated terminal cost");
    let terminal_status = result.status;
    let terminal_error = result.error.clone();
    let terminal_output = result.output.clone();
    let terminal_attribution = result.attribution.clone();
    let terminal_range = result.session_ref.seq_range;
    let terminal_artifact = terminal.clone();
    let mut payloads = std::mem::take(&mut result.evidence.events);
    let mut cursor = result.evidence.next;
    let mut pages = 1usize;
    while let Some(after) = cursor {
        if pages >= MAX_PAGES || payloads.len() >= MAX_EVIDENCE_EVENTS {
            return Err(PillboxError::runtime(
                "session send",
                "managed execution evidence exceeded the bounded page budget",
            )
            .into());
        }
        let (mut page, bytes) = status(&client, endpoint, capability_secret, &prepared, after)?
            .ok_or_else(|| {
                PillboxError::runtime(
                    "session send",
                    "managed invocation disappeared during evidence pagination",
                )
            })?;
        response_bytes = add_response_bytes(response_bytes, bytes)?;
        let artifact = validate_terminal(&page, session_id, &prepared, after)?;
        if page.disposition != Disposition::Reused
            || page.status != terminal_status
            || page.error != terminal_error
            || page.output != terminal_output
            || page.attribution != terminal_attribution
            || page.session_ref.seq_range != terminal_range
            || page.cost.as_ref() != Some(&terminal_cost)
            || artifact != terminal_artifact
        {
            return Err(PillboxError::runtime(
                "session send",
                "managed execution changed terminal identity across evidence pages",
            )
            .into());
        }
        payloads.append(&mut page.evidence.events);
        if payloads.len() > MAX_EVIDENCE_EVENTS {
            return Err(PillboxError::runtime(
                "session send",
                "managed execution evidence exceeded 2000 events",
            )
            .into());
        }
        cursor = page.evidence.next;
        pages += 1;
    }
    let expected_events = terminal_range.map_or(0, |range| range[1] + 1);
    if payloads.len() as u64 != expected_events {
        return Err(PillboxError::runtime(
            "session send",
            "managed execution positional session range does not match collected evidence",
        )
        .into());
    }

    payloads.push(Payload::Custom(Custom {
        name: "run_cost".into(),
        payload: Some(serde_json::to_value(terminal_cost).expect("cost envelope serializes")),
    }));
    let terminal_error = terminal_outcome(terminal_status, terminal_error);
    let events: Vec<_> = payloads
        .iter()
        .cloned()
        .map(|payload| Event::session(session_id, payload))
        .collect();
    let mut log = SessionLog::open(resolved, session_id)?;
    log.append_exact_batch(&events, None, |pre_append_seq| {
        let commit = CommitIntent::new(pre_append_seq, payloads.clone(), terminal_error.clone())?;
        journal.persist(&PendingState::committing(prepared.pending.clone(), commit))
    })?;
    // The fsynced local log is authoritative. The journal only brackets its
    // exact batch and is deleted after the append (or recovered append) proves
    // durable.
    journal.clear()?;
    finish_outcome(terminal_error)
}

fn recover_commit(
    resolved: &Pillbox,
    session_id: &str,
    journal: &PendingJournal,
    commit: &CommitIntent,
) -> Result<()> {
    let events: Vec<_> = commit
        .payloads
        .iter()
        .cloned()
        .map(|payload| Event::session(session_id, payload))
        .collect();
    SessionLog::open(resolved, session_id)?.append_exact_batch(
        &events,
        Some(commit.pre_append_seq),
        |_| anyhow::bail!("recovering managed commit was unexpectedly prepared twice"),
    )?;
    journal.clear()?;
    finish_outcome(commit.terminal_error.clone())
}

fn terminal_outcome(status: ExecutionStatus, error: Option<ExecutionError>) -> Option<String> {
    if status == ExecutionStatus::Completed {
        return None;
    }
    Some(error.map_or_else(
        || format!("managed execution ended with status {}", status.as_str()),
        |error| format!("{}: {}", error.code.as_str(), error.message),
    ))
}

fn finish_outcome(error: Option<String>) -> Result<()> {
    match error {
        None => Ok(()),
        Some(detail) => Err(PillboxError::runtime("session send", detail).into()),
    }
}

fn execute(
    client: &reqwest::blocking::Client,
    url: &str,
    capability_secret: &str,
    session_id: &str,
    prepared: &PreparedInvocation,
) -> std::result::Result<(ExecutionResult, usize), PostJsonError> {
    let token = super::mint_managed_capability(
        "execute",
        &prepared.pending.body_sha256,
        Some(session_id),
        Some(&prepared.pending.invocation_id),
        capability_secret,
    );
    post_json(client, url, &token, &prepared.pending.request_body)
}

fn execute_with_reconciliation(
    client: &reqwest::blocking::Client,
    execute_url: &str,
    endpoint: &str,
    capability_secret: &str,
    session_id: &str,
    prepared: &PreparedInvocation,
) -> Result<(ExecutionResult, usize)> {
    match execute(
        client,
        execute_url,
        capability_secret,
        session_id,
        prepared,
    ) {
        Ok(response) => Ok(response),
        Err(PostJsonError::LostResponse(execute_error)) => status(
            client,
            endpoint,
            capability_secret,
            prepared,
            0,
        )?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "managed execute response was lost and invocation `{}` is not yet queryable: {execute_error:#}",
                prepared.pending.invocation_id
            )
        }),
        Err(error) => Err(error.into_anyhow()),
    }
}

fn status(
    client: &reqwest::blocking::Client,
    endpoint: &str,
    capability_secret: &str,
    prepared: &PreparedInvocation,
    after: u64,
) -> Result<Option<(ExecutionResult, usize)>> {
    let body = serde_json::to_string(&serde_json::json!({
        "contract_version": CONTRACT_VERSION,
        "invocation_id": prepared.pending.invocation_id,
        "evidence_after": after,
        "evidence_limit": 100
    }))
    .context("serialize managed status request")?;
    let token = super::mint_managed_capability(
        "status",
        &request_sha256(&body),
        None,
        Some(&prepared.pending.invocation_id),
        capability_secret,
    );
    match post_json(
        client,
        &format!("{}/v2/executions/status", endpoint.trim_end_matches('/')),
        &token,
        &body,
    ) {
        Ok(result) => Ok(Some(result)),
        Err(PostJsonError::NotFound) => Ok(None),
        Err(error) => Err(error.into_anyhow()),
    }
}

fn post_json(
    client: &reqwest::blocking::Client,
    url: &str,
    token: &str,
    body: &str,
) -> std::result::Result<(ExecutionResult, usize), PostJsonError> {
    let resp = client
        .post(url)
        .header("content-type", "application/json")
        .bearer_auth(token)
        .body(body.to_owned())
        .send()
        .map_err(|error| PostJsonError::LostResponse(anyhow::anyhow!("POST {url}: {error}")))?;
    let status = resp.status();
    let response = read_execution_response(resp)?;
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(PostJsonError::NotFound);
    }
    if !status.is_success() {
        return Err(PostJsonError::Rejected(
            PillboxError::runtime(
                "session send",
                format!(
                    "managed execution returned HTTP {status}: {}",
                    capped(&response)
                ),
            )
            .into(),
        ));
    }
    let bytes = response.len();
    let result = serde_json::from_str(&response).map_err(|error| {
        PostJsonError::Rejected(
            PillboxError::runtime(
                "session send",
                format!("invalid managed execution response: {error}"),
            )
            .into(),
        )
    })?;
    Ok((result, bytes))
}

fn read_execution_response(
    mut response: reqwest::blocking::Response,
) -> std::result::Result<String, PostJsonError> {
    let mut bytes = Vec::new();
    response
        .by_ref()
        .take(MAX_RESPONSE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            PostJsonError::LostResponse(anyhow::anyhow!("read managed execution response: {error}"))
        })?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(PostJsonError::Rejected(
            PillboxError::runtime(
                "session send",
                "managed execution response exceeded 8388608 bytes",
            )
            .into(),
        ));
    }
    String::from_utf8(bytes).map_err(|_| {
        PostJsonError::Rejected(
            PillboxError::runtime("session send", "managed execution response was not UTF-8")
                .into(),
        )
    })
}

fn add_response_bytes(total: usize, page: usize) -> Result<usize> {
    let total = total.checked_add(page).ok_or_else(|| {
        PillboxError::runtime("session send", "managed response byte counter overflowed")
    })?;
    if total > MAX_RESPONSE_BYTES {
        return Err(PillboxError::runtime(
            "session send",
            "managed execution responses exceeded 8 MiB in total",
        )
        .into());
    }
    Ok(total)
}

fn capped(value: &str) -> &str {
    let mut end = value.len().min(2048);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::os::unix::fs::PermissionsExt as _;

    use sha2::{Digest, Sha256};

    use super::*;

    #[test]
    fn capped_stops_before_a_split_utf8_character() {
        let value = format!("{}étail", "a".repeat(2047));
        assert_eq!(super::capped(&value), "a".repeat(2047));
    }

    #[test]
    fn canonical_json_hash_matches_execution_v2_reference_vector() {
        let value = serde_json::json!({
            "z": [3, { "é": true, "a": null }],
            "a": "한글",
        });
        assert_eq!(
            canonical_sha256(&value).unwrap(),
            "sha256:1932a99cba0c005a524adf4671beb60a440f495ab7a7f0fcdfc23a937c3afb20"
        );
    }

    #[test]
    fn terminal_validation_rejects_corrupted_response_identity_and_cursor() {
        let prepared = fixture_prepared("invocation-1", "session-1", "hello");
        let valid = fixture_terminal(&prepared, "session-1");
        assert!(validate_terminal(&valid, "session-1", &prepared, 0).is_ok());

        let mut corrupted = valid.clone();
        corrupted.request_hash = format!("sha256:{}", "b".repeat(64));
        assert!(validate_terminal(&corrupted, "session-1", &prepared, 0).is_err());

        let mut corrupted = valid.clone();
        corrupted.execution_digest = format!("sha256:{}", "c".repeat(64));
        assert!(validate_terminal(&corrupted, "session-1", &prepared, 0).is_err());

        let mut corrupted = valid.clone();
        corrupted.attribution.requested_model = "wrong/model".into();
        assert!(validate_terminal(&corrupted, "session-1", &prepared, 0).is_err());

        let mut corrupted = valid.clone();
        corrupted.evidence.next = Some(2);
        corrupted.evidence.truncated = true;
        assert!(validate_terminal(&corrupted, "session-1", &prepared, 0).is_err());

        let mut corrupted = valid.clone();
        corrupted.evidence.artifact_ref.as_mut().unwrap().key = "other".into();
        assert!(validate_terminal(&corrupted, "session-1", &prepared, 0).is_err());

        let mut value = serde_json::to_value(fixture_terminal_json(&prepared, "session-1"))
            .expect("response serializes");
        value
            .as_object_mut()
            .unwrap()
            .insert("unexpected".into(), serde_json::Value::Bool(true));
        assert!(serde_json::from_value::<ExecutionResult>(value).is_err());
    }

    #[test]
    fn committing_retry_recovers_crash_before_append_without_network() {
        crate::test_util::with_isolated_home("managed-commit-before-append", || {
            let resolved = crate::pillbox::global();
            let session_id = "abc123def456";
            let prepared = fixture_prepared("invocation-1", session_id, "hello");
            let payloads = fixture_commit_payloads(&prepared, session_id);
            let journal = persist_commit(&resolved, session_id, &prepared, 0, payloads.clone());

            execute_turn(
                &resolved,
                session_id,
                "https://network-must-not-run.invalid",
                "capability-secret",
                "hello",
                Some("provider/model"),
            )
            .expect("committing retry appends the journaled batch locally");

            assert!(!journal.path().exists());
            let events = SessionLog::open(&resolved, session_id)
                .unwrap()
                .read_from(0)
                .unwrap();
            assert_eq!(
                events
                    .iter()
                    .map(|event| &event.payload)
                    .collect::<Vec<_>>(),
                payloads.iter().collect::<Vec<_>>()
            );
        });
    }

    #[test]
    fn committing_retry_repairs_torn_partial_prefix_without_duplication() {
        crate::test_util::with_isolated_home("managed-commit-partial", || {
            let resolved = crate::pillbox::global();
            let session_id = "abc123def456";
            let prepared = fixture_prepared("invocation-1", session_id, "hello");
            let payloads = fixture_commit_payloads(&prepared, session_id);
            let journal = persist_commit(&resolved, session_id, &prepared, 0, payloads.clone());
            let mut log = SessionLog::open(&resolved, session_id).unwrap();
            log.append(&[Event::session(session_id, payloads[0].clone())])
                .unwrap();
            let path = crate::session::session_dir_path(&resolved, session_id).join("log.jsonl");
            let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
            file.write_all(b"{\"seq\":2,\"payload\":").unwrap();
            drop(file);

            execute_turn(
                &resolved,
                session_id,
                "https://network-must-not-run.invalid",
                "capability-secret",
                "hello",
                Some("provider/model"),
            )
            .expect("committing retry appends only the missing suffix");

            assert!(!journal.path().exists());
            let events = log.read_from(0).unwrap();
            assert_eq!(events.len(), payloads.len());
            assert_eq!(events[0].payload, payloads[0]);
            assert_eq!(events[1].payload, payloads[1]);
        });
    }

    #[test]
    fn committing_retry_after_full_append_clears_without_duplication() {
        crate::test_util::with_isolated_home("managed-commit-after-append", || {
            let resolved = crate::pillbox::global();
            let session_id = "abc123def456";
            let prepared = fixture_prepared("invocation-1", session_id, "hello");
            let payloads = fixture_commit_payloads(&prepared, session_id);
            let journal = persist_commit(&resolved, session_id, &prepared, 0, payloads.clone());
            let mut log = SessionLog::open(&resolved, session_id).unwrap();
            let events: Vec<_> = payloads
                .iter()
                .cloned()
                .map(|payload| Event::session(session_id, payload))
                .collect();
            log.append(&events).unwrap();

            execute_turn(
                &resolved,
                session_id,
                "https://network-must-not-run.invalid",
                "capability-secret",
                "hello",
                Some("provider/model"),
            )
            .expect("full prefix is recognized as already committed");

            assert!(!journal.path().exists());
            assert_eq!(log.read_from(0).unwrap().len(), payloads.len());
        });
    }

    #[test]
    fn committing_retry_fails_loud_on_interleaving_and_keeps_journal() {
        crate::test_util::with_isolated_home("managed-commit-interleaved", || {
            let resolved = crate::pillbox::global();
            let session_id = "abc123def456";
            let prepared = fixture_prepared("invocation-1", session_id, "hello");
            let payloads = fixture_commit_payloads(&prepared, session_id);
            let journal = persist_commit(&resolved, session_id, &prepared, 0, payloads);
            let mut log = SessionLog::open(&resolved, session_id).unwrap();
            log.append(&[Event::session(
                session_id,
                Payload::Custom(Custom {
                    name: "interleaved".into(),
                    payload: None,
                }),
            )])
            .unwrap();

            let error = execute_turn(
                &resolved,
                session_id,
                "https://network-must-not-run.invalid",
                "capability-secret",
                "hello",
                Some("provider/model"),
            )
            .unwrap_err();

            assert!(error.to_string().contains("payload mismatch"));
            assert!(journal.path().exists());
            assert_eq!(log.read_from(0).unwrap().len(), 1);
        });
    }

    #[test]
    fn lost_execute_response_reconciles_same_persisted_invocation_through_status() {
        crate::test_util::with_isolated_home("managed-lost-response", || {
            let resolved = crate::pillbox::global();
            let session_id = "abc123def456";
            let pending = PendingJournal::open(&resolved, session_id)
                .unwrap()
                .path()
                .to_path_buf();
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let pending_for_server = pending.clone();
            let server = std::thread::spawn(move || {
                let (mut execute_socket, _) = listener.accept().unwrap();
                let execute_request = read_http_request(&mut execute_socket);
                assert!(execute_request.starts_with("POST /v2/executions HTTP/1.1"));
                assert!(
                    pending_for_server.exists(),
                    "pending marker must precede POST"
                );
                assert_eq!(
                    std::fs::metadata(&pending_for_server)
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o600
                );
                let pending_bytes = std::fs::read(&pending_for_server).unwrap();
                let pending_text = String::from_utf8(pending_bytes).unwrap();
                assert!(!pending_text.contains("Bearer"));
                assert!(!pending_text.contains("capability-secret"));
                let execute_body = http_body(&execute_request);
                let request: serde_json::Value = serde_json::from_str(execute_body).unwrap();
                let invocation_id = request["invocation_id"].as_str().unwrap().to_string();
                drop(execute_socket); // The model turn exists, but its HTTP response is lost.

                let (mut status_socket, _) = listener.accept().unwrap();
                let status_request = read_http_request(&mut status_socket);
                assert!(status_request.starts_with("POST /v2/executions/status HTTP/1.1"));
                let status_body: serde_json::Value =
                    serde_json::from_str(http_body(&status_request)).unwrap();
                assert_eq!(status_body["invocation_id"], invocation_id);
                let prepared = fixture_prepared_from_request(request);
                let response = fixture_terminal_json(&prepared, session_id);
                write_json_response(&mut status_socket, &response.to_string());
            });

            execute_turn(
                &resolved,
                session_id,
                &endpoint,
                "capability-secret",
                "hello",
                Some("provider/model"),
            )
            .expect("lost execute response reconciles through status");
            server.join().unwrap();
            assert!(!pending.exists(), "pending clears only after local append");
            let events = SessionLog::open(&resolved, session_id)
                .unwrap()
                .read_from(0)
                .unwrap();
            assert!(events.iter().any(|event| matches!(
                &event.payload,
                Payload::MessageDelta(delta) if delta.text == "done"
            )));
        });
    }

    fn fixture_prepared(invocation_id: &str, session_id: &str, text: &str) -> PreparedInvocation {
        let request = serde_json::json!({
            "contract_version": CONTRACT_VERSION,
            "session_ref": { "session_id": session_id },
            "invocation_id": invocation_id,
            "idempotency_key": invocation_id,
            "rendered_input": text,
            "rendered_input_hash": request_sha256(text),
            "tool_policy": "deny_all",
            "execution": {
                "transport": {
                    "harness": "opencode",
                    "transport": "http",
                    "harness_version": "managed-v2",
                    "adapter_revision": "pillbox-cli-v2"
                },
                "requested": {
                    "provider": "provider",
                    "model": "model",
                    "profile": null,
                    "reasoning_effort": "medium"
                },
                "placement": "managed_container",
                "context_renderer_revision": "pillbox-cli-v2"
            },
            "execution_policy_revision": EXECUTION_POLICY_REVISION,
            "output_format": { "type": "text", "retry_count": 0 }
        });
        fixture_prepared_from_request(request)
    }

    fn fixture_prepared_from_request(request: serde_json::Value) -> PreparedInvocation {
        let body = serde_json::to_string(&request).unwrap();
        let execution_digest = canonical_sha256(&serde_json::json!({
            "execution": request["execution"].clone(),
            "execution_policy_revision": EXECUTION_POLICY_REVISION,
        }))
        .unwrap();
        PreparedInvocation {
            pending: PendingRequest {
                invocation_id: request["invocation_id"].as_str().unwrap().into(),
                body_sha256: request_sha256(&body),
                request_hash: canonical_sha256(&request).unwrap(),
                execution_digest,
                execution_policy_revision: EXECUTION_POLICY_REVISION.into(),
                request_body: body,
            },
            requested_model: "provider/model".into(),
        }
    }

    fn fixture_terminal(prepared: &PreparedInvocation, session_id: &str) -> ExecutionResult {
        serde_json::from_value(fixture_terminal_json(prepared, session_id)).unwrap()
    }

    fn fixture_commit_payloads(prepared: &PreparedInvocation, session_id: &str) -> Vec<Payload> {
        let mut result = fixture_terminal(prepared, session_id);
        let cost = result.cost.take().unwrap();
        let mut payloads = std::mem::take(&mut result.evidence.events);
        payloads.push(Payload::Custom(Custom {
            name: "run_cost".into(),
            payload: Some(serde_json::to_value(cost).unwrap()),
        }));
        payloads
    }

    fn persist_commit(
        resolved: &Pillbox,
        session_id: &str,
        prepared: &PreparedInvocation,
        pre_append_seq: u64,
        payloads: Vec<Payload>,
    ) -> PendingJournal {
        let journal = PendingJournal::open(resolved, session_id).unwrap();
        let commit = CommitIntent::new(pre_append_seq, payloads, None).unwrap();
        journal
            .persist(&PendingState::committing(prepared.pending.clone(), commit))
            .unwrap();
        journal
    }

    fn fixture_terminal_json(prepared: &PreparedInvocation, session_id: &str) -> serde_json::Value {
        let invocation_digest = format!(
            "{:x}",
            Sha256::digest(prepared.pending.invocation_id.as_bytes())
        );
        serde_json::json!({
            "disposition": "reused",
            "invocation_id": prepared.pending.invocation_id,
            "request_hash": prepared.pending.request_hash,
            "execution_digest": prepared.pending.execution_digest,
            "execution_policy_revision": prepared.pending.execution_policy_revision,
            "attribution": {
                "harness": "opencode",
                "transport": "http",
                "requested_model": prepared.requested_model,
                "served_model": "provider/model"
            },
            "session_ref": { "session_id": session_id, "seq_range": [0, 0] },
            "status": "completed",
            "output": { "text": "done" },
            "evidence": {
                "from": 0,
                "next": null,
                "truncated": false,
                "events": [{
                    "type": "message_delta",
                    "messageId": "message-1",
                    "text": "done"
                }],
                "artifact_ref": {
                    "key": format!(
                        "executions/{invocation_digest}/{}.json",
                        prepared.pending.request_hash.trim_start_matches("sha256:")
                    ),
                    "media_type": "application/json",
                    "bytes": 512,
                    "sha256": format!("sha256:{}", "a".repeat(64))
                }
            },
            "cost": {
                "version": 1,
                "status": "completed",
                "model": {
                    "input_tokens": 1,
                    "output_tokens": 1,
                    "cache_read_input_tokens": 0,
                    "cache_creation_input_tokens": 0,
                    "provider_reported_cost_usd": null
                },
                "infrastructure": {
                    "d1_rows_read": 1,
                    "d1_rows_written": 2,
                    "r2_reads": 0,
                    "r2_writes": 1,
                    "r2_bytes_read": 0,
                    "r2_bytes_written": 512,
                    "analytics_points_planned": 1,
                    "sandbox_duration_ms": 5,
                    "sandbox_profile": "standard-2"
                },
                "known_cost_usd": null,
                "estimated_total_cost_usd": null,
                "rate_card_version": null
            }
        })
    }

    fn read_http_request(stream: &mut TcpStream) -> String {
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let count = stream.read(&mut chunk).unwrap();
            bytes.extend_from_slice(&chunk[..count]);
            let text = String::from_utf8_lossy(&bytes);
            if let Some(header_end) = text.find("\r\n\r\n") {
                let content_length = text[..header_end]
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .and_then(|value| value.parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if bytes.len() >= header_end + 4 + content_length {
                    return String::from_utf8(bytes).unwrap();
                }
            }
            assert!(count > 0, "connection closed before complete request");
        }
    }

    fn http_body(request: &str) -> &str {
        request.split_once("\r\n\r\n").unwrap().1
    }

    fn write_json_response(stream: &mut TcpStream, body: &str) {
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
    }
}
