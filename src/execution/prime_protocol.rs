//! Prime Agent's qualified, tool-free JSONL turn (native 0.9.8).
//! The image owns the catalog; only `responseModel` is served-model evidence.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::usage::TurnUsage;

#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FailureCode {
    RuntimeUnavailable,
    RuntimeRejected,
    RuntimeTimeout,
    RuntimeProtocolError,
    InternalError,
}

pub(crate) fn failure_code(stage: &str, error: &anyhow::Error, rejected: bool) -> FailureCode {
    if format!("{error:#}").contains("deadline exceeded") {
        return FailureCode::RuntimeTimeout;
    }
    match stage {
        "resolve" if rejected => FailureCode::RuntimeRejected,
        "resolve" | "credentials" | "image_prepare" | "guest_prepare" | "vmm_spawn"
        | "guest_rpc_ready" => FailureCode::RuntimeUnavailable,
        "turn" => FailureCode::RuntimeProtocolError,
        _ => FailureCode::InternalError,
    }
}

pub(crate) fn managed_key(path: &std::path::Path) -> Result<String> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .context("Prime managed credential is unavailable")?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.nlink() == 1
            && metadata.permissions().mode() & 0o077 == 0
            && metadata.len() <= 64 * 1024,
        "Prime managed credential is not a bounded private file"
    );
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(64 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 64 * 1024,
        "Prime managed credential exceeds bound"
    );
    let auth: Value = serde_json::from_slice(&bytes).context("parse Prime managed credential")?;
    Ok(crate::vault::providers::prime::credential_key(&auth)
        .map_err(anyhow::Error::msg)?
        .to_owned())
}

pub(crate) const ADAPTER_REVISION: &str = "pillbox/prime-agent-text-v1";
pub(crate) const HOST: &str = "api.pinference.ai";
pub(crate) const PROFILE_PATH: &str = "/opt/pillbox-prime-text/profile.json";
pub(crate) const MAX_FINAL_TEXT: usize = 32 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ImageProfile {
    pub(crate) schema_version: u8,
    pub(crate) harness_version: String,
    pub(crate) models: Vec<Value>,
}

#[derive(Clone, Debug)]
pub(crate) struct Selection {
    pub(crate) runner_image_id: String,
    pub(crate) harness_version: String,
    pub(crate) requested_model: String,
    pub(crate) model: String,
    pub(crate) effort: String,
}

impl ImageProfile {
    pub(crate) fn select(&self, image: &str, requested: &str, effort: &str) -> Result<Selection> {
        ensure!(
            super::valid_digest(image),
            "invalid immutable runner image ID"
        );
        // A new native cohort needs qualification of its execution-registry and
        // final-message semantics; a matching flag name alone is insufficient.
        ensure!(
            self.schema_version == 1 && self.harness_version == "0.9.8",
            "unqualified Prime Agent image profile"
        );
        let model = requested
            .strip_prefix("prime-inference/")
            .unwrap_or(requested);
        ensure!(
            !model.is_empty() && model.len() <= 256,
            "invalid Prime model ID"
        );
        let matches: Vec<_> = self
            .models
            .iter()
            .filter(|entry| entry["id"] == model && entry["provider"] == "prime-inference")
            .collect();
        ensure!(
            matches.len() == 1,
            "Prime model is absent or ambiguous in the image catalog"
        );
        let entry = matches[0];
        ensure!(
            entry["api"] == "openai-completions"
                && entry["baseUrl"] == "https://api.pinference.ai/api/v1"
                && entry["input"]
                    .as_array()
                    .is_some_and(|input| input.iter().any(|kind| kind == "text")),
            "Prime model cannot be served by the qualified text transport"
        );
        ensure!(
            matches!(effort, "off" | "low" | "medium" | "high"),
            "unsupported Prime reasoning effort"
        );
        if effort != "off" {
            ensure!(
                entry["reasoning"] == true,
                "Prime model does not support reasoning"
            );
        }
        if let Some(mapped) = entry["thinkingLevelMap"].get(effort) {
            ensure!(
                mapped.as_str() == Some(effort),
                "Prime model cannot serve the requested effort"
            );
        }
        Ok(Selection {
            runner_image_id: image.into(),
            harness_version: self.harness_version.clone(),
            requested_model: requested.into(),
            model: model.into(),
            effort: effort.into(),
        })
    }
}

impl Selection {
    pub(crate) fn argv(&self) -> Vec<String> {
        [
            "/usr/local/bin/prime-agent",
            "-p",
            "--mode",
            "json",
            "--offline",
            "--no-session",
            "--no-tools",
            "--no-extensions",
            "--no-skills",
            "--no-context-files",
            "--no-prompt-templates",
            "--provider",
            "prime-inference",
            "--model",
            &self.model,
            "--thinking",
            &self.effort,
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }

    pub(crate) fn resolved(&self, observed_version: &str, served: Option<&str>) -> Value {
        json!({"harness": "prime-agent", "harness_version": observed_version,
            "adapter_revision": ADAPTER_REVISION, "runner_image_id": self.runner_image_id,
            "requested_model": self.requested_model, "served_model": served})
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Limits {
    pub(crate) deadline: Instant,
    pub(crate) frame: usize,
    pub(crate) evidence: usize,
    pub(crate) final_text: usize,
}

#[derive(Default)]
pub(crate) struct Turn {
    pub(crate) frames: Vec<Value>,
    pub(crate) usage: Option<TurnUsage>,
    version: Option<String>,
    message: Option<Value>,
    ended: bool,
    exited: bool,
}

pub(crate) struct Completion {
    pub(crate) text: String,
    pub(crate) version: String,
    pub(crate) served_model: Option<String>,
}

impl Turn {
    fn accept(&mut self, frame: Value, selection: &Selection) -> Result<()> {
        ensure!(!self.exited, "frame after Prime process exit");
        self.frames.push(frame.clone());
        match frame["type"].as_str().context("Prime event has no type")? {
            "pillbox_prime_init" => {
                ensure!(
                    self.version.is_none() && self.message.is_none(),
                    "duplicate or late Prime initialization"
                );
                let version = frame["harness_version"]
                    .as_str()
                    .context("Prime version absent")?;
                ensure!(
                    version == selection.harness_version,
                    "Prime binary differs from image profile"
                );
                self.version = Some(version.into());
            }
            "pillbox_prime_exit" => {
                ensure!(
                    frame["exit_code"] == 0 && self.ended,
                    "Prime exited without a successful terminal event"
                );
                self.exited = true;
            }
            "message_end" if frame["message"]["role"] == "assistant" => {
                ensure!(
                    self.version.is_some() && self.message.is_none() && !self.ended,
                    "multiple or unordered Prime answers"
                );
                let message = &frame["message"];
                self.usage = TurnUsage::from_prime_message(message);
                ensure!(
                    message["stopReason"] == "stop",
                    "Prime answer did not stop normally"
                );
                ensure!(
                    message["provider"] == "prime-inference" && message["model"] == selection.model,
                    "Prime selected a different provider or model"
                );
                let content = message["content"]
                    .as_array()
                    .context("Prime answer has no content")?;
                ensure!(
                    content
                        .iter()
                        .all(|block| matches!(block["type"].as_str(), Some("text" | "thinking"))),
                    "Prime attempted a denied tool"
                );
                self.message = Some(message.clone());
            }
            "agent_end" => {
                ensure!(
                    !self.ended && self.message.is_some() && frame["willRetry"] != true,
                    "Prime turn is incomplete or retrying"
                );
                let messages = frame["messages"]
                    .as_array()
                    .context("Prime terminal messages absent")?;
                let assistant: Vec<_> = messages
                    .iter()
                    .filter(|message| message["role"] == "assistant")
                    .collect();
                ensure!(
                    assistant.len() == 1 && Some(assistant[0]) == self.message.as_ref(),
                    "Prime terminal answer differs from message_end"
                );
                self.ended = true;
            }
            "tool_execution_start" | "tool_execution_end" => {
                anyhow::bail!("Prime attempted a denied tool")
            }
            "message_update" => {
                let update = &frame["assistantMessageEvent"]["type"];
                ensure!(
                    !update
                        .as_str()
                        .is_some_and(|kind| kind.starts_with("toolcall")),
                    "Prime attempted a denied tool"
                );
            }
            "session" | "agent_start" | "turn_start" | "message_start" | "message_end"
            | "turn_end" => {
                ensure!(
                    self.version.is_some() && !self.ended,
                    "unordered Prime event"
                );
            }
            _ => anyhow::bail!("unsupported Prime event"),
        }
        Ok(())
    }

    fn finish(&self, max_text: usize) -> Result<Completion> {
        ensure!(self.exited, "truncated Prime stream");
        let message = self.message.as_ref().context("Prime answer absent")?;
        let mut text = String::new();
        for block in message["content"]
            .as_array()
            .context("Prime content absent")?
        {
            if block["type"] == "text" {
                let part = block["text"]
                    .as_str()
                    .context("Prime text is not a string")?;
                ensure!(
                    text.len()
                        .checked_add(part.len())
                        .is_some_and(|len| len <= max_text.min(MAX_FINAL_TEXT)),
                    "Prime final text exceeds byte limit"
                );
                text.push_str(part);
            }
        }
        ensure!(!text.trim().is_empty(), "Prime final text is empty");
        let served_model = message["responseModel"]
            .as_str()
            .filter(|model| !model.is_empty() && model.len() <= 256)
            .map(str::to_owned);
        Ok(Completion {
            text,
            version: self.version.clone().context("Prime version absent")?,
            served_model,
        })
    }
}

/// A fake or real guest uses this same bounded transport. Success requires EOF
/// after the guest supervisor's exit report, so a partial tail is never accepted.
pub(crate) fn run(
    stream: &mut UnixStream,
    selection: &Selection,
    input: &str,
    limits: Limits,
    turn: &mut Turn,
    mut check_live: impl FnMut() -> Result<()>,
) -> Result<Completion> {
    stream.set_read_timeout(Some(Duration::from_millis(50)))?;
    stream.set_write_timeout(Some(Duration::from_millis(50)))?;
    let mut request = serde_json::to_vec(&json!({"prompt": input}))?;
    ensure!(
        request.len() <= limits.frame,
        "Prime input frame exceeds byte limit"
    );
    request.push(b'\n');
    let mut sent = 0;
    while sent < request.len() {
        check_live()?;
        ensure!(Instant::now() < limits.deadline, "Prime deadline exceeded");
        match stream.write(&request[sent..]) {
            Ok(0) => anyhow::bail!("Prime input closed"),
            Ok(count) => sent += count,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error.into()),
        }
    }
    let mut partial = Vec::new();
    let mut used = request.len();
    let mut buffer = [0_u8; 8192];
    loop {
        check_live()?;
        ensure!(Instant::now() < limits.deadline, "Prime deadline exceeded");
        match stream.read(&mut buffer) {
            Ok(0) => {
                ensure!(partial.is_empty(), "unterminated Prime frame");
                return turn.finish(limits.final_text);
            }
            Ok(count) => {
                used = used
                    .checked_add(count)
                    .context("Prime evidence size overflow")?;
                ensure!(used <= limits.evidence, "Prime evidence exceeds byte limit");
                for byte in &buffer[..count] {
                    if *byte == b'\n' {
                        ensure!(
                            !partial.is_empty() && turn.frames.len() < 4096,
                            "invalid or excessive Prime frames"
                        );
                        let frame =
                            serde_json::from_slice(&partial).context("invalid Prime JSON frame")?;
                        turn.accept(frame, selection)?;
                        partial.clear();
                    } else {
                        ensure!(
                            partial.len() < limits.frame,
                            "Prime frame exceeds byte limit"
                        );
                        partial.push(*byte);
                    }
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead as _;

    #[test]
    fn missing_managed_credential_has_a_closed_credentials_failure() {
        let dir = tempfile::tempdir().unwrap();
        let error = managed_key(&dir.path().join("absent.json")).unwrap_err();
        let detail =
            json!({"code": failure_code("credentials", &error, false), "stage": "credentials"});
        assert_eq!(
            detail,
            json!({"code": "runtime_unavailable", "stage": "credentials"})
        );
        assert!(!detail.to_string().contains("absent.json"));
        assert_eq!(
            failure_code("turn", &anyhow::anyhow!("denied tool"), false),
            FailureCode::RuntimeProtocolError
        );
        assert_eq!(
            failure_code("turn", &anyhow::anyhow!("Prime deadline exceeded"), false),
            FailureCode::RuntimeTimeout
        );
    }

    fn profile() -> ImageProfile {
        ImageProfile {
            schema_version: 1,
            harness_version: "0.9.8".into(),
            models: vec![json!({
                "id": "openai/test-model", "provider": "prime-inference", "api": "openai-completions",
                "baseUrl": "https://api.pinference.ai/api/v1", "input": ["text"], "reasoning": true
            })],
        }
    }

    fn selection() -> Selection {
        profile()
            .select(
                &format!("sha256:{}", "a".repeat(64)),
                "openai/test-model",
                "low",
            )
            .unwrap()
    }

    fn message(text: &str) -> Value {
        json!({"role": "assistant", "provider": "prime-inference", "model": "openai/test-model",
            "responseModel": "authoritative-served-id", "stopReason": "stop",
            "content": [{"type": "text", "text": text}],
            "usage": {"input": 11, "output": 22, "cacheRead": 33, "cacheWrite": 44, "cost": {"total": 0.012}}})
    }

    fn frames(message: Value) -> Vec<Value> {
        vec![
            json!({"type": "pillbox_prime_init", "harness_version": "0.9.8"}),
            json!({"type": "agent_start"}),
            json!({"type": "turn_start"}),
            json!({"type": "message_end", "message": message}),
            json!({"type": "agent_end", "messages": [message]}),
            json!({"type": "pillbox_prime_exit", "exit_code": 0}),
        ]
    }

    fn fake(
        bytes: Vec<u8>,
        final_text: usize,
        frame: usize,
        evidence: usize,
    ) -> (Result<Completion>, Turn) {
        let (mut host, mut guest) = UnixStream::pair().unwrap();
        let sender = std::thread::spawn(move || {
            let mut input = String::new();
            std::io::BufReader::new(guest.try_clone().unwrap())
                .read_line(&mut input)
                .unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&input).unwrap(),
                json!({"prompt": "hello"})
            );
            // Failure may intentionally close the reader before the full reply.
            let _ = guest.write_all(&bytes);
        });
        let mut turn = Turn::default();
        let result = run(
            &mut host,
            &selection(),
            "hello",
            Limits {
                deadline: Instant::now() + Duration::from_secs(2),
                frame,
                evidence,
                final_text,
            },
            &mut turn,
            || Ok(()),
        );
        drop(host);
        sender.join().unwrap();
        (result, turn)
    }

    fn encoded(frames: &[Value]) -> Vec<u8> {
        frames
            .iter()
            .flat_map(|frame| {
                let mut bytes = frame.to_string().into_bytes();
                bytes.push(b'\n');
                bytes
            })
            .collect()
    }

    #[test]
    fn selection_lowers_onto_image_catalog_and_rejects_unservable_models() {
        let selected = selection();
        let argv = selected.argv();
        assert!(argv
            .windows(2)
            .any(|pair| pair == ["--model", "openai/test-model"]));
        for flag in [
            "--no-tools",
            "--no-extensions",
            "--no-skills",
            "--no-context-files",
            "--no-prompt-templates",
            "--offline",
        ] {
            assert!(argv.iter().any(|arg| arg == flag));
        }
        assert!(profile()
            .select(&selected.runner_image_id, "missing-model", "low")
            .is_err());
        let mut invalid = profile();
        invalid.models[0]["baseUrl"] = json!("https://attacker.example");
        assert!(invalid
            .select(&selected.runner_image_id, "openai/test-model", "low")
            .is_err());
        invalid = profile();
        invalid.models[0]["reasoning"] = json!(false);
        assert!(invalid
            .select(&selected.runner_image_id, "openai/test-model", "high")
            .is_err());
        let mut mapped = profile();
        mapped.models[0]["thinkingLevelMap"] =
            json!({"xhigh": "xhigh", "minimal": null, "max": "max"});
        for effort in ["off", "low", "medium", "high"] {
            assert!(mapped
                .select(&selected.runner_image_id, "openai/test-model", effort)
                .is_ok());
        }
        mapped.models[0]["thinkingLevelMap"]["off"] = Value::Null;
        assert!(mapped
            .select(&selected.runner_image_id, "openai/test-model", "off")
            .is_err());
        mapped.models[0]["thinkingLevelMap"]["low"] = json!("high");
        assert!(mapped
            .select(&selected.runner_image_id, "openai/test-model", "low")
            .is_err());
    }

    #[test]
    fn fake_harness_success_carries_observed_resolution_and_disjoint_usage() {
        let (done, turn) = fake(
            encoded(&frames(message("hello back"))),
            32768,
            1048576,
            8388608,
        );
        let done = done.unwrap();
        assert_eq!(done.text, "hello back");
        assert_eq!(
            selection().resolved(&done.version, done.served_model.as_deref()),
            json!({
            "harness": "prime-agent", "harness_version": "0.9.8", "adapter_revision": ADAPTER_REVISION,
            "runner_image_id": format!("sha256:{}", "a".repeat(64)), "requested_model": "openai/test-model",
            "served_model": "authoritative-served-id"})
        );
        assert_eq!(
            serde_json::to_value(turn.usage.unwrap()).unwrap(),
            json!({"cost_usd": 0.012,
            "input_tokens": 11, "output_tokens": 22, "cache_read_tokens": 33, "cache_write_tokens": 44})
        );
    }

    #[test]
    fn absent_served_evidence_and_usage_are_not_filled_from_the_request() {
        let mut reply = message("hello");
        reply.as_object_mut().unwrap().remove("responseModel");
        reply.as_object_mut().unwrap().remove("usage");
        let (done, turn) = fake(encoded(&frames(reply)), 32768, 1048576, 8388608);
        assert!(done.unwrap().served_model.is_none());
        assert!(turn.usage.is_none());
    }

    #[test]
    fn denied_tools_including_mcp_files_and_network_fail_the_turn() {
        for name in ["bash", "edit", "write", "read", "mcp_tool", "curl"] {
            let mut reply = message("ignored");
            reply["stopReason"] = json!("toolUse");
            reply["content"] =
                json!([{"type": "toolCall", "id": "denied", "name": name, "arguments": {}}]);
            assert!(fake(encoded(&frames(reply)), 32768, 1048576, 8388608)
                .0
                .is_err());
            let mut events = frames(message("ignored"));
            events.insert(1, json!({"type": "tool_execution_start", "toolName": name}));
            assert!(fake(encoded(&events), 32768, 1048576, 8388608).0.is_err());
        }
    }

    #[test]
    fn empty_truncated_oversized_and_partial_answers_fail_without_rewriting() {
        for text in [
            "".to_owned(),
            " ".to_owned(),
            "a".repeat(32769),
            "é".repeat(16385),
        ] {
            assert!(
                fake(encoded(&frames(message(&text))), 32768, 1048576, 8388608)
                    .0
                    .is_err()
            );
        }
        let mut reply = message("partial");
        reply["stopReason"] = json!("length");
        assert!(fake(encoded(&frames(reply)), 32768, 1048576, 8388608)
            .0
            .is_err());
        let events = frames(message("truncated"));
        assert!(fake(
            encoded(&events[..events.len() - 1]),
            32768,
            1048576,
            8388608
        )
        .0
        .is_err());
        let mut bytes = encoded(&events);
        bytes.pop();
        assert!(fake(bytes, 32768, 1048576, 8388608).0.is_err());
    }

    #[test]
    fn frames_evidence_versions_and_duplicate_answers_are_bounded() {
        let events = frames(message("hello"));
        assert!(fake(encoded(&events), 32768, 100, 8388608).0.is_err());
        assert!(fake(encoded(&events), 32768, 1048576, 100).0.is_err());
        let mut invalid = events.clone();
        invalid[0]["harness_version"] = json!("0.10.0");
        assert!(fake(encoded(&invalid), 32768, 1048576, 8388608).0.is_err());
        invalid = events;
        invalid.insert(
            4,
            json!({"type": "message_end", "message": message("second")}),
        );
        assert!(fake(encoded(&invalid), 32768, 1048576, 8388608).0.is_err());
    }
}
