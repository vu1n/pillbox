//! `pillbox.text/2` driver for the Cursor Agent CLI (`cursor_agent`).
//!
//! The harness is the `agent` binary already registered as `--agent cursor` and
//! bundled in the runner image. Headless turns speak `--print` +
//! `--output-format stream-json`. This module resolves a selection onto that
//! image and decodes the stream. It does not launch the microVM: the CLI
//! exchanges an API key for access and refresh tokens, and those tokens would
//! enter the guest. See [`admit`].

use anyhow::{ensure, Result};
use serde_json::{json, Value};

use super::usage::TurnUsage;
use super::valid_digest;

pub(crate) const HARNESS: &str = "cursor_agent";
/// Same adapter identity as the rest of `pillbox.text/2`.
pub(crate) const ADAPTER_REVISION: &str = "pillbox/local-text-v2";
pub(crate) const CREDENTIAL_REF: &str = "pillbox:cursor:default";
/// Model-provider hosts only. Login hosts (`cursor.com`, `authenticator.cursor.sh`)
/// stay off this list: the guest must not perform login.
pub(crate) const NETWORK_HOSTS: &[&str] = &["api2.cursor.sh", "agentn.global.api5.cursor.sh"];

/// What resolve produces for one cursor_agent selection. No harness version:
/// that is observed from `agent --version`, never taken from the image pin.
#[derive(Debug)]
pub(crate) struct Lowered {
    pub(crate) runner_image_id: String,
    pub(crate) credential_ref: &'static str,
    pub(crate) network_hosts: &'static [&'static str],
    pub(crate) requested_model: String,
    pub(crate) reasoning_effort: String,
    pub(crate) argv: Vec<String>,
    pub(crate) cli_config: Value,
}

/// Why a resolved selection still cannot sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Admission {
    /// No login sentinel and no `CURSOR_API_KEY` secret.
    MissingCredential,
    /// A credential exists, and launching would place the real token in the guest.
    VaultBlocked,
}

impl Admission {
    pub(crate) fn stage(self) -> &'static str {
        match self {
            Self::MissingCredential => "credentials",
            Self::VaultBlocked => "guest_prepare",
        }
    }

    pub(crate) fn code(self) -> &'static str {
        "runtime_unavailable"
    }

    /// Session-evidence text. Stays out of the failure record.
    pub(crate) fn evidence_message(self) -> &'static str {
        match self {
            Self::MissingCredential => {
                "cursor_agent credential is absent (auth.json sentinel and CURSOR_API_KEY)"
            }
            Self::VaultBlocked => {
                "cursor_agent launch refused: the CLI exchanges the API key for access and refresh tokens that would enter the guest, and its model transport defaults to HTTP/2"
            }
        }
    }
}

/// A finished tool-free turn, decoded from the harness stream.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Turn {
    pub(crate) text: String,
    pub(crate) usage: Option<TurnUsage>,
    pub(crate) harness_version: String,
}

/// The stream was not a single tool-free final answer. `usage` is set when the
/// result line had already reported spend.
#[derive(Clone, Debug)]
pub(crate) struct TurnError {
    pub(crate) message: String,
    /// Spend already reported on the result line, when the failure happened
    /// after that line. Production sampling does not reach this yet.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) usage: Option<TurnUsage>,
}

impl std::fmt::Display for TurnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Bare model ids the CLI can be asked for (`composer-2.5`, `grok-4.5`).
/// Provider-prefixed ids, whitespace, and bracket parameter smuggling are
/// unservable. The account's live catalog is not known at resolve.
fn validate_model(model: &str) -> Result<()> {
    let bare = !model.is_empty()
        && model.len() <= 128
        && model
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    ensure!(bare, "cursor_agent cannot serve model `{model}`");
    Ok(())
}

fn validate_effort(effort: &str) -> Result<()> {
    ensure!(
        matches!(effort, "low" | "medium" | "high"),
        "unsupported cursor_agent reasoning effort"
    );
    Ok(())
}

/// argv for a headless turn. No `--force`, `--yolo`, `--api-key`, or MCP approval:
/// tools are denied in [`deny_all_cli_config`], and the credential stays out of argv.
pub(crate) fn text_argv(model: &str) -> Vec<String> {
    vec![
        "agent".into(),
        "-p".into(),
        "--trust".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--sandbox".into(),
        "enabled".into(),
        "--model".into(),
        model.into(),
    ]
}

pub(crate) fn launch_argv(lowered: &Lowered, prompt: &str) -> Vec<String> {
    let mut argv = lowered.argv.clone();
    argv.push("--".into());
    argv.push(prompt.into());
    argv
}

/// Guest `~/.cursor/cli-config.json`. Deny beats allow, including under `--force`,
/// so every tool class the CLI documents is denied. `useHttp1ForAgent` asks the
/// CLI to speak HTTP/1.1; the server can still force HTTP/2.
pub(crate) fn deny_all_cli_config() -> Value {
    json!({
        "permissions": {
            "allow": [],
            "deny": ["Shell(*)", "Read(**)", "Write(**)", "WebFetch(*)", "Mcp(*:*)"]
        },
        "approvalMode": "allowlist",
        "network": { "useHttp1ForAgent": true }
    })
}

/// Image lookup failures are `runtime_unavailable`. A model or effort the CLI
/// cannot be asked for is `runtime_rejected`. Both happen at resolve, before
/// credentials.
pub(crate) fn resolve_selection(
    model: &str,
    reasoning_effort: &str,
    runner_image_id: Result<String>,
) -> std::result::Result<Lowered, (&'static str, anyhow::Error)> {
    let runner_image_id = runner_image_id.map_err(|error| ("runtime_unavailable", error))?;
    lower(model, reasoning_effort, runner_image_id).map_err(|error| ("runtime_rejected", error))
}

pub(crate) fn lower(
    model: &str,
    reasoning_effort: &str,
    runner_image_id: String,
) -> Result<Lowered> {
    ensure!(valid_digest(&runner_image_id), "invalid runner image id");
    validate_model(model)?;
    validate_effort(reasoning_effort)?;
    Ok(Lowered {
        runner_image_id,
        credential_ref: CREDENTIAL_REF,
        network_hosts: NETWORK_HOSTS,
        requested_model: model.to_string(),
        reasoning_effort: reasoning_effort.to_string(),
        argv: text_argv(model),
        cli_config: deny_all_cli_config(),
    })
}

/// True when the login sentinel exists or a non-empty `CURSOR_API_KEY` secret is stored.
pub(crate) fn credentials_ready(sentinel_exists: bool, api_key: Option<&str>) -> bool {
    sentinel_exists || api_key.is_some_and(|key| !key.trim().is_empty())
}

// Context: doc://pillbox/libkrun-env-fork-substrate@0002#libkrun-env-fork-substrate — the guest mounts stubs; cursor_agent does not launch while key exchange would place access and refresh tokens in the guest.
pub(crate) fn admit(credentials_ready: bool) -> Admission {
    if credentials_ready {
        Admission::VaultBlocked
    } else {
        Admission::MissingCredential
    }
}

/// `agent --version` stdout. Empty, multi-line, or help-text output is not a version.
pub(crate) fn observed_cli_version(stdout: &str) -> Option<String> {
    let version = stdout.trim();
    if version.is_empty()
        || version.len() > 128
        || version
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        None
    } else {
        Some(version.to_string())
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn parse_stream(stdout: &str) -> Result<Vec<Value>, TurnError> {
    let mut lines = Vec::new();
    for (index, line) in stdout.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line).map_err(|_| TurnError {
            message: format!("cursor stream line {} is not JSON", index + 1),
            usage: None,
        })?;
        if !value.is_object() {
            return Err(TurnError {
                message: format!("cursor stream line {} is not an object", index + 1),
                usage: None,
            });
        }
        lines.push(value);
    }
    if lines.is_empty() {
        return Err(TurnError {
            message: "cursor stream ended without events".into(),
            usage: None,
        });
    }
    Ok(lines)
}

/// Decode one stream-json turn. A tool or interaction event fails the turn.
/// Final text is the `result` field only; an empty or over-limit answer fails
/// and is not padded or cut.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn interpret(
    lines: &[Value],
    max_final_text_bytes: usize,
    observed_version: &str,
) -> Result<Turn, TurnError> {
    let harness_version = observed_cli_version(observed_version).ok_or_else(|| TurnError {
        message: "cursor CLI did not report a version".into(),
        usage: None,
    })?;
    let mut saw_tool = false;
    let mut extra_result = false;
    let mut result = None;
    for line in lines {
        match line.get("type").and_then(Value::as_str) {
            Some("tool_call" | "interaction_query") => saw_tool = true,
            Some("result") if result.is_some() => extra_result = true,
            Some("result") => result = Some(line),
            _ => {}
        }
    }
    // A tool event usually precedes the result that reports spend. Keep that
    // usage on the protocol error instead of dropping it.
    let usage = result.and_then(TurnUsage::from_cursor_result);
    let fail = |message: String| TurnError {
        message,
        usage: usage.clone(),
    };
    if saw_tool {
        return Err(fail("cursor turn attempted a tool or interaction".into()));
    }
    if extra_result {
        return Err(fail("cursor stream contained more than one result".into()));
    }
    let result = result.ok_or_else(|| TurnError {
        message: "cursor stream ended without a result".into(),
        usage: None,
    })?;
    if result.get("subtype").and_then(Value::as_str) != Some("success")
        || result.get("is_error").and_then(Value::as_bool) != Some(false)
    {
        return Err(fail(
            "cursor result was not a successful final answer".into(),
        ));
    }
    let Some(text) = result.get("result").and_then(Value::as_str) else {
        return Err(fail("cursor result omitted final text".into()));
    };
    if text.trim().is_empty() {
        return Err(fail("cursor result text was empty".into()));
    }
    if text.len() > max_final_text_bytes {
        return Err(fail(format!(
            "cursor result is {} bytes, over the {max_final_text_bytes} byte limit",
            text.len()
        )));
    }
    Ok(Turn {
        text: text.to_string(),
        usage,
        harness_version,
    })
}

/// The `pillbox.text/2` completion object for a decoded turn. `served_model` is
/// null: the CLI's init `model` field is a display name, not the model id.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn completed_record(
    invocation_id: &str,
    request_hash: &str,
    lowered: &Lowered,
    turn: &Turn,
) -> Value {
    let mut detail = json!({
        "invocation_id": invocation_id,
        "request_hash": request_hash,
        "resolved": {
            "harness": HARNESS,
            "harness_version": turn.harness_version,
            "adapter_revision": ADAPTER_REVISION,
            "runner_image_id": lowered.runner_image_id,
            "requested_model": lowered.requested_model,
            "served_model": Value::Null,
        },
        "output_text": turn.text,
    });
    if let Some(usage) = &turn.usage {
        detail["usage"] = json!(usage);
    }
    detail
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const TURN_STAGE: &str = "turn";
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const TURN_CODE: &str = "runtime_protocol_error";

/// Closed failure record. The message stays out of this object.
pub(crate) fn failure_detail(
    invocation_id: &str,
    request_hash: &str,
    code: &str,
    stage: &str,
    session_ref: Option<&Value>,
    usage: Option<&TurnUsage>,
) -> Value {
    let mut detail = json!({
        "invocation_id": invocation_id,
        "request_hash": request_hash,
        "code": code,
        "stage": stage,
    });
    if let Some(session_ref) = session_ref {
        detail["session_ref"] = session_ref.clone();
    }
    if let Some(usage) = usage {
        detail["usage"] = json!(usage);
    }
    detail
}

pub(crate) fn launch_plan(lowered: &Lowered, prompt: &str) -> Value {
    json!({
        "argv": launch_argv(lowered, prompt),
        "reasoning_effort": lowered.reasoning_effort,
        "cli_config": lowered.cli_config,
        "credential_ref": lowered.credential_ref,
        "network_hosts": lowered.network_hosts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn image() -> String {
        format!("sha256:{}", "c".repeat(64))
    }

    fn lowered(model: &str) -> Lowered {
        lower(model, "low", image()).unwrap()
    }

    fn success_lines(text: &str, usage: Value) -> Vec<Value> {
        vec![
            json!({"type":"system","subtype":"init","model":"Composer 2.5","session_id":"s"}),
            json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":text}]},"session_id":"s"}),
            json!({"type":"result","subtype":"success","is_error":false,"result":text,"usage":usage,"session_id":"s"}),
        ]
    }

    #[test]
    fn selection_lowers_onto_the_resolved_image() {
        let lowered = lowered("composer-2.5");
        assert_eq!(lowered.runner_image_id, image());
        assert_eq!(lowered.credential_ref, "pillbox:cursor:default");
        assert_eq!(
            lowered.network_hosts,
            ["api2.cursor.sh", "agentn.global.api5.cursor.sh"]
        );
        assert_eq!(lowered.requested_model, "composer-2.5");
        assert_eq!(lowered.reasoning_effort, "low");
        assert!(lowered
            .argv
            .windows(2)
            .any(|pair| pair[0] == "--model" && pair[1] == "composer-2.5"));
        let plan = launch_plan(&lowered, "hello");
        assert!(plan.get("harness_version").is_none());
        assert!(plan["argv"].as_array().unwrap().iter().all(|arg| {
            !matches!(
                arg.as_str(),
                Some("--force" | "--yolo" | "--api-key" | "--approve-mcps")
            )
        }));
    }

    #[test]
    fn unservable_model_is_rejected_at_resolve() {
        for model in [
            "openai/gpt-5",
            "",
            "composer 2",
            "composer-2.5[effort=high]",
            "a/b",
        ] {
            let error = lower(model, "low", image()).unwrap_err();
            assert!(
                error.to_string().contains("cannot serve"),
                "{model}: {error}"
            );
        }
        assert!(lower("composer-2.5", "ultra", image()).is_err());
        assert!(lower("composer-2.5", "low", "latest".into()).is_err());
    }

    #[test]
    fn prepared_turn_denies_every_documented_tool_class() {
        let config = deny_all_cli_config();
        let deny = config["permissions"]["deny"].as_array().unwrap();
        for token in [
            "Shell(*)",
            "Read(**)",
            "Write(**)",
            "WebFetch(*)",
            "Mcp(*:*)",
        ] {
            assert!(deny.iter().any(|entry| entry == token), "{token}");
        }
        assert!(config["permissions"]["allow"]
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(config["network"]["useHttp1ForAgent"], true);
        let argv = launch_argv(&lowered("grok-4.5"), "say hello");
        assert_eq!(argv[argv.len() - 2], "--");
        assert_eq!(argv.last().unwrap(), "say hello");
        assert!(!argv.iter().any(|arg| arg == "--force" || arg == "--yolo"));
    }

    #[test]
    fn fake_harness_completed_turn_reports_resolved_and_usage() {
        let lowered = lowered("composer-2.5");
        let lines = success_lines(
            "Hello",
            json!({"inputTokens":10,"outputTokens":4,"cacheReadTokens":7,"cacheWriteTokens":1}),
        );
        let turn = interpret(&lines, 32_768, "2026.10.01-e373342\n").unwrap();
        let record = completed_record("chat_1", "hash", &lowered, &turn);
        assert_eq!(
            record["resolved"],
            json!({
                "harness": "cursor_agent",
                "harness_version": "2026.10.01-e373342",
                "adapter_revision": "pillbox/local-text-v2",
                "runner_image_id": image(),
                "requested_model": "composer-2.5",
                "served_model": null,
            })
        );
        assert_eq!(record["output_text"], "Hello");
        assert_eq!(
            record["usage"],
            json!({"cost_usd": null, "input_tokens": 10, "output_tokens": 4,
                "cache_read_tokens": 7, "cache_write_tokens": 1})
        );
    }

    #[test]
    fn fake_harness_without_usage_omits_the_usage_key() {
        let lowered = lowered("composer-2.5");
        let lines =
            vec![json!({"type":"result","subtype":"success","is_error":false,"result":"Hi"})];
        let turn = interpret(&lines, 32_768, "2026.10.01-e373342").unwrap();
        let record = completed_record("chat_1", "hash", &lowered, &turn);
        assert!(record.get("usage").is_none());
        assert_eq!(record["resolved"]["served_model"], Value::Null);
    }

    #[test]
    fn fake_harness_tool_attempt_fails_the_turn() {
        let lines = vec![
            json!({"type":"tool_call","subtype":"started","call_id":"c1","tool_call":{"bashToolCall":{"args":{"command":"ls"}}}}),
            json!({"type":"result","subtype":"success","is_error":false,"result":"done","usage":{"inputTokens":3,"outputTokens":1}}),
        ];
        let error = interpret(&lines, 32_768, "2026.10.01-e373342").unwrap_err();
        assert!(error.message.contains("tool"));
        assert_eq!(error.usage.as_ref().unwrap().input_tokens, Some(3));
        let detail = failure_detail(
            "chat_1",
            "hash",
            TURN_CODE,
            TURN_STAGE,
            None,
            error.usage.as_ref(),
        );
        assert_eq!(detail["code"], "runtime_protocol_error");
        assert_eq!(detail["stage"], "turn");
        assert_eq!(detail["usage"]["input_tokens"], 3);
        assert!(detail.get("error").is_none());
        assert!(detail.get("message").is_none());
        let interaction = vec![json!({"type":"interaction_query","subtype":"request"})];
        assert!(interpret(&interaction, 32_768, "2026.10.01-e373342").is_err());
    }

    #[test]
    fn fake_harness_over_limit_or_empty_or_truncated_answer_fails_without_cutting() {
        let over = success_lines("hello", json!({"inputTokens":2,"outputTokens":1}));
        let error = interpret(&over, 4, "2026.10.01-e373342").unwrap_err();
        assert!(error.message.contains("over"));
        assert!(!error.message.contains("hell"));
        assert_eq!(error.usage.unwrap().input_tokens, Some(2));

        let empty = vec![json!({"type":"result","subtype":"success","is_error":false,"result":""})];
        assert!(interpret(&empty, 32_768, "2026.10.01-e373342").is_err());
        let blank =
            vec![json!({"type":"result","subtype":"success","is_error":false,"result":" \n"})];
        assert!(interpret(&blank, 32_768, "2026.10.01-e373342").is_err());

        let truncated =
            vec![json!({"type":"assistant","message":{"content":[{"text":"partial"}]}})];
        let error = interpret(&truncated, 32_768, "2026.10.01-e373342").unwrap_err();
        assert!(error.message.contains("without a result"));

        assert!(interpret(&over, 32_768, "   ").is_err());
        assert!(observed_cli_version("agent --help\nmore").is_none());
    }

    #[test]
    fn missing_credential_fails_at_credentials_and_a_present_one_still_does_not_launch() {
        assert!(!credentials_ready(false, None));
        assert!(!credentials_ready(false, Some("  ")));
        assert!(credentials_ready(true, None));
        assert!(credentials_ready(false, Some("key")));
        let missing = admit(false);
        assert_eq!(missing.stage(), "credentials");
        assert_eq!(missing.code(), "runtime_unavailable");
        let blocked = admit(true);
        assert_eq!(blocked.stage(), "guest_prepare");
        assert_eq!(blocked.code(), "runtime_unavailable");
        assert_ne!(missing.stage(), blocked.stage());
    }

    #[test]
    fn a_non_json_stream_is_a_protocol_failure() {
        let error = parse_stream("not-json\n").unwrap_err();
        assert!(error.message.contains("not JSON"));
        let lines = parse_stream(
            "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"ok\"}\n",
        )
        .unwrap();
        assert!(interpret(&lines, 32, "v1").is_ok());
    }

    #[test]
    fn resolver_maps_an_unservable_model_to_runtime_rejected() {
        let (code, _) = resolve_selection("openai/gpt-5", "low", Ok(image())).unwrap_err();
        assert_eq!(code, "runtime_rejected");
        let (code, _) =
            resolve_selection("composer-2.5", "low", Err(anyhow::anyhow!("no image"))).unwrap_err();
        assert_eq!(code, "runtime_unavailable");
        assert!(resolve_selection("composer-2.5", "low", Ok(image())).is_ok());
    }
}
