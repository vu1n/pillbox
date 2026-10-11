use super::*;
use std::thread;

fn limits() -> Limits {
    Limits {
        timeout_ms: 10_000,
        max_final_text_bytes: 32_768,
        max_frame_bytes: 1_048_576,
        max_evidence_bytes: 8_388_608,
    }
}

fn catalog() -> Catalog {
    serde_json::from_value(json!({"harness_version": "1.0.2", "models": [{
        "provider": "openai-codex", "id": "gpt-6-luna", "base_url": "https://chatgpt.com/backend-api",
        "efforts": ["low", "medium", "high"] }]})).unwrap()
}

fn plan() -> Plan {
    Plan::resolve(
        format!("sha256:{}", "b".repeat(64)),
        catalog(),
        "gpt-6-luna",
        "low",
    )
    .unwrap()
}

fn message(text: &str) -> Value {
    json!({"type": "message_end", "pillbox_pi_native_usage": {"input_tokens": 19, "output_tokens": 3, "input_tokens_details": {"cached_tokens": 7}}, "message": {
        "role": "assistant", "model": "gpt-6-luna", "stopReason": "stop",
        "content": [{"type": "text", "text": text}],
        "usage": {"input": 12, "output": 3, "cacheRead": 7, "cacheWrite": 2, "cost": {"total": 0.0123}} }})
}

fn transcript(text: &str) -> Vec<Value> {
    vec![
        message(text),
        json!({"type": "agent_end", "messages": [], "willRetry": false}),
        json!({"type": "pillbox_pi.done", "served_model": "gpt-6-luna-2026-09-01", "usage_reported": true}),
        json!({"type": "pillbox_pi.exit", "code": 0}),
    ]
}

fn fake_harness(
    lines: Vec<Value>,
    limits: Limits,
) -> (Result<(String, Option<String>)>, Turn, Wire) {
    let (host, mut harness) = UnixStream::pair().unwrap();
    let worker = thread::spawn(move || {
        let mut request = Vec::new();
        let mut byte = [0_u8];
        while harness.read(&mut byte).unwrap() > 0 {
            if byte[0] == b'\n' {
                break;
            }
            request.push(byte[0]);
        }
        let request: Value = serde_json::from_slice(&request).unwrap();
        assert_eq!(request["model"], "gpt-6-luna");
        for line in lines {
            let mut bytes = serde_json::to_vec(&line).unwrap();
            bytes.push(b'\n');
            if harness.write_all(&bytes).is_err() {
                break;
            }
        }
        harness.shutdown(std::net::Shutdown::Write).unwrap();
    });
    let mut wire = Wire::new(host, limits, Instant::now() + Duration::from_secs(3)).unwrap();
    wire.send(
        &json!({"model": plan().model, "input": "hello"}),
        &mut || Ok(()),
    )
    .unwrap();
    let mut turn = Turn::default();
    let result = (|| -> Result<_> {
        while let Some(line) = wire.next(&mut || Ok(()))? {
            turn.observe(&line, limits.max_final_text_bytes as usize)?;
        }
        turn.finish()
    })();
    worker.join().unwrap();
    (result, turn, wire)
}

#[test]
fn selection_lowers_onto_resolved_image_and_rejects_unservable_models() {
    let plan = plan();
    assert_eq!(plan.runner_image_id, format!("sha256:{}", "b".repeat(64)));
    assert_eq!(plan.harness_version, "1.0.2");
    assert_eq!(plan.model, "gpt-6-luna");
    assert!(Plan::resolve(plan.runner_image_id.clone(), catalog(), "invented", "low").is_err());
    assert!(Plan::resolve(
        plan.runner_image_id.clone(),
        catalog(),
        "anthropic/gpt-6-luna",
        "low"
    )
    .is_err());
    assert!(Plan::resolve(plan.runner_image_id, catalog(), "gpt-6-luna", "ultra").is_err());
    assert_eq!(failure_code("resolve", false, true), "runtime_rejected");
}

#[test]
fn fake_harness_success_has_observed_resolved_metadata_and_exclusive_usage() {
    let (result, turn, wire) = fake_harness(transcript("hello"), limits());
    let (text, served) = result.unwrap();
    assert_eq!(text, "hello");
    assert_eq!(
        plan().resolved("openai-codex/gpt-6-luna", served.as_deref()),
        json!({
        "harness": "pi", "harness_version": "1.0.2", "adapter_revision": ADAPTER_REVISION,
        "runner_image_id": format!("sha256:{}", "b".repeat(64)),
        "requested_model": "openai-codex/gpt-6-luna", "served_model": "gpt-6-luna-2026-09-01" })
    );
    assert_eq!(
        serde_json::to_value(turn.usage.unwrap()).unwrap(),
        json!({"cost_usd": 0.0123,
        "input_tokens": 12, "output_tokens": 3, "cache_read_tokens": 7})
    );
    assert_eq!(wire.frame_count, 4);
}

#[test]
fn missing_native_usage_and_model_are_not_filled_from_request_or_pi_defaults() {
    let mut lines = transcript("answer");
    lines[0]
        .as_object_mut()
        .unwrap()
        .remove("pillbox_pi_native_usage");
    lines[2]["usage_reported"] = json!(false);
    lines[2]["served_model"] = Value::Null;
    let (result, turn, _) = fake_harness(lines, limits());
    assert_eq!(result.unwrap().1, None);
    assert_eq!(turn.usage, None);
    assert_eq!(
        plan().resolved("gpt-6-luna", None)["served_model"],
        Value::Null
    );
}

#[test]
fn executable_transport_refuses_tools_mcp_file_edits_and_network_tools() {
    for name in [
        "bash",
        "edit",
        "write",
        "mcp__server__tool",
        "web_search",
        "fetch",
    ] {
        let attempted = json!({"type": "tool_execution_start", "toolName": name, "toolCallId": "denied", "args": {}});
        let (result, _, _) = fake_harness(vec![attempted], limits());
        assert!(result.is_err(), "{name}");
        assert_eq!(failure_code("turn", false, false), "runtime_protocol_error");
    }
    for attempted in [
        json!({"type": "message_update", "assistantMessageEvent": {"type": "toolcall_start", "toolName": "edit"}}),
        json!({"type": "message_end", "message": {"role": "assistant", "content": [{"type": "toolCall", "name": "bash"}]}}),
    ] {
        assert!(fake_harness(vec![attempted], limits()).0.is_err());
    }
}

#[test]
fn oversized_empty_truncated_multiple_and_unconfirmed_answers_fail_at_turn() {
    for text in [String::new(), "é".repeat(16_385)] {
        let (result, turn, _) = fake_harness(transcript(&text), limits());
        assert!(result.is_err());
        assert!(
            turn.usage.is_some(),
            "reported spend survives protocol rejection"
        );
    }
    let mut truncated = transcript("prefix");
    truncated[0]["message"]["stopReason"] = json!("length");
    assert!(fake_harness(truncated, limits()).0.is_err());
    let mut duplicate = transcript("a");
    duplicate.insert(1, message("b"));
    assert!(fake_harness(duplicate, limits()).0.is_err());
    let mut unconfirmed = transcript("a");
    unconfirmed.pop();
    assert!(fake_harness(unconfirmed, limits()).0.is_err());
}

#[test]
fn framing_evidence_timeout_and_cancellation_are_bounded() {
    for bytes in [b"{\"type\":".as_slice(), b"garbage\n".as_slice()] {
        let (host, mut guest) = UnixStream::pair().unwrap();
        guest.write_all(bytes).unwrap();
        guest.shutdown(std::net::Shutdown::Write).unwrap();
        let mut wire = Wire::new(host, limits(), Instant::now() + Duration::from_secs(1)).unwrap();
        assert!(wire.next(&mut || Ok(())).is_err());
    }
    for (frame, evidence) in [(3, 1024), (1024, 3)] {
        let (host, mut guest) = UnixStream::pair().unwrap();
        guest.write_all(b"{\"type\":\"event\"}\n").unwrap();
        let mut small = limits();
        small.max_frame_bytes = frame;
        small.max_evidence_bytes = evidence;
        let mut wire = Wire::new(host, small, Instant::now() + Duration::from_secs(1)).unwrap();
        assert!(wire.next(&mut || Ok(())).is_err());
    }
    let (host, _guest) = UnixStream::pair().unwrap();
    let mut wire = Wire::new(host, limits(), Instant::now()).unwrap();
    assert!(wire.next(&mut || Ok(())).is_err());
    assert_eq!(failure_code("turn", true, false), "runtime_timeout");
    let (host, _guest) = UnixStream::pair().unwrap();
    let mut wire = Wire::new(host, limits(), Instant::now() + Duration::from_secs(1)).unwrap();
    assert!(wire.next(&mut || anyhow::bail!("cancelled")).is_err());
}

#[test]
fn missing_credential_fails_at_credentials_without_free_text_in_failure_code() {
    assert!(Credentials::from_codex(&json!({}), "invocation").is_err());
    assert_eq!(
        failure_code("credentials", false, false),
        "runtime_unavailable"
    );
}

#[test]
fn synthetic_pi_credentials_keep_real_tokens_account_and_refresh_out_of_guest() {
    let access = format!("h.{}.s", URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({
        "https://api.openai.com/auth": {"chatgpt_account_id": "real-account"}, "exp": 9999999999_u64
    })).unwrap()));
    let real = json!({"tokens": {"access_token": access, "refresh_token": "real-refresh",
        "id_token": format!("h.{}.s", URL_SAFE_NO_PAD.encode(b"{}")), "account_id": "real-account"}});
    let first = Credentials::from_codex(&real, "invocation").unwrap();
    let second = Credentials::from_codex(&real, "invocation").unwrap();
    let guest = first.guest.to_string();
    assert!(!guest.contains(&access));
    assert!(!guest.contains("real-account"));
    assert!(!guest.contains("real-refresh"));
    assert_ne!(first.access.stub, second.access.stub);
    assert_ne!(first.account.stub, second.account.stub);
    assert_eq!(first.access.real, access);
    assert_eq!(first.account.real, "real-account");
    let claim: Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(first.access.stub.split('.').nth(1).unwrap())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        claim["https://api.openai.com/auth"]["chatgpt_account_id"],
        first.account.stub
    );
    let mut conflict = real.clone();
    conflict["tokens"]["account_id"] = json!("different-account");
    assert!(Credentials::from_codex(&conflict, "invocation").is_err());
}

#[test]
fn native_failed_and_partial_usage_never_becomes_initialized_zero_usage() {
    let mut turn = Turn::default();
    turn.observe(
        &json!({"type": "pillbox_pi.usage", "usage": {
        "input_tokens": 25, "output_tokens": 3, "input_tokens_details": {"cached_tokens": 5}}}),
        32_768,
    )
    .unwrap();
    let mut failed = message("partial");
    failed["message"]["stopReason"] = json!("error");
    failed
        .as_object_mut()
        .unwrap()
        .remove("pillbox_pi_native_usage");
    assert!(turn.observe(&failed, 32_768).is_err());
    assert_eq!(
        serde_json::to_value(turn.usage.unwrap()).unwrap(),
        json!({
        "cost_usd": null, "input_tokens": 20, "output_tokens": 3, "cache_read_tokens": 5})
    );
    assert_eq!(
        serde_json::to_value(
            TurnUsage::from_pi_provider_usage(&json!({
                "input_tokens": 100, "output_tokens": 5,
                "input_tokens_details": {"cached_tokens": 20, "cache_write_tokens": 10}
            }))
            .unwrap()
        )
        .unwrap(),
        json!({"cost_usd": null, "input_tokens": 70,
        "output_tokens": 5, "cache_read_tokens": 20, "cache_write_tokens": 10})
    );
    let initialized = &message("ok")["message"];
    assert_eq!(
        TurnUsage::from_pi_text_message(initialized, &json!({})),
        None
    );
    assert_eq!(
        TurnUsage::from_pi_text_message(initialized, &json!({"input_tokens": 100})),
        None
    );
    assert_eq!(
        serde_json::to_value(
            TurnUsage::from_pi_text_message(initialized, &json!({"output_tokens": 5})).unwrap()
        )
        .unwrap(),
        json!({"cost_usd": null, "output_tokens": 5})
    );
}

#[test]
fn failure_classification_survives_cleanup_past_deadline() {
    let deadline = Instant::now() + Duration::from_millis(20);
    let error =
        classify::<()>(Err(anyhow::anyhow!("Pi final text byte limit")), deadline).unwrap_err();
    thread::sleep(Duration::from_millis(30));
    let timed_out = error
        .downcast_ref::<FailureAtDetection>()
        .unwrap()
        .timed_out;
    assert_eq!(
        failure_code("turn", timed_out, false),
        "runtime_protocol_error"
    );
    assert!(
        classify::<()>(Err(anyhow::anyhow!("deadline")), deadline)
            .unwrap_err()
            .downcast_ref::<FailureAtDetection>()
            .unwrap()
            .timed_out
    );
}

#[test]
fn managed_credential_loading_is_local_bounded_and_checks_cancellation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.json");
    assert!(Credentials::read_managed(&path, "test", &mut || Ok(())).is_err());
    let mut checks = 0;
    assert!(Credentials::read_managed(&path, "test", &mut || {
        checks += 1;
        anyhow::bail!("cancelled");
    })
    .is_err());
    assert_eq!(checks, 1);
    std::fs::write(&path, b"{}").unwrap();
    assert!(Credentials::read_managed(&path, "test", &mut || Ok(())).is_err());
    let payload = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&json!({"exp": 4_102_444_800_u64,
        "https://api.openai.com/auth": {"chatgpt_account_id": "synthetic-account"}}))
        .unwrap(),
    );
    let real = json!({"auth_mode": "chatgpt", "tokens": {"access_token": format!("h.{payload}.s"),
        "id_token": "e30.e30.s", "refresh_token": "synthetic-refresh"}});
    std::fs::write(&path, serde_json::to_vec(&real).unwrap()).unwrap();
    assert!(Credentials::read_managed(&path, "test", &mut || Ok(())).is_ok());
    let mut stale = real;
    stale["tokens"]["access_token"] = json!("h.eyJleHAiOjF9.s");
    std::fs::write(&path, serde_json::to_vec(&stale).unwrap()).unwrap();
    assert!(Credentials::read_managed(&path, "test", &mut || Ok(())).is_err());
    std::fs::write(&path, vec![b' '; 1_048_577]).unwrap();
    assert!(Credentials::read_managed(&path, "test", &mut || Ok(())).is_err());
}
