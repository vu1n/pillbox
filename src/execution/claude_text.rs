//! Claude's sealed text lane. Native stdout is untrusted protocol input: removing
//! tools at launch is enforced again on every frame, including MCP and denials.

use std::io::Read;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};

use super::usage::TurnUsage;

#[cfg(feature = "libkrun")]
mod local;
#[cfg(feature = "libkrun")]
pub(crate) use local::execute;

pub(crate) const ADAPTER_REVISION: &str = "pillbox/claude-text-v1";
pub(crate) const PROVIDER_HOST: &str = "api.anthropic.com";
pub(crate) const CREDENTIAL_REF: &str = "pillbox:claude:default";
pub(crate) const MAX_FINAL_TEXT_BYTES: u64 = 32 * 1024;

/// Only the generated, identity-free OAuth file enters the guest. The release
/// token stays host-side and is bound to PROVIDER_HOST by the launcher.
pub(crate) struct Credentials {
    pub(crate) guest_auth: Vec<u8>,
    pub(crate) stub: String,
    pub(crate) real: String,
}

pub(crate) fn credentials(
    path: &std::path::Path,
    invocation: &str,
    deadline: Instant,
) -> Result<Credentials> {
    let real = crate::vault::pre_refresh_until(path, "claude", deadline)?
        .context("Claude OAuth credentials unavailable")?;
    fresh_credentials(&real, invocation)
}

fn fresh_credentials(real: &Value, invocation: &str) -> Result<Credentials> {
    let token = |field: &str| -> Result<&str> {
        let value = real["claudeAiOauth"][field]
            .as_str()
            .context("missing Claude OAuth token")?;
        ensure!(
            !value.is_empty()
                && value.len() <= 65536
                && value.bytes().all(|b| b.is_ascii_graphic()),
            "invalid Claude OAuth token"
        );
        Ok(value)
    };
    let access = token("accessToken")?;
    let refresh = token("refreshToken")?;
    // Do not clone arbitrary fields from the host credential store into the VM.
    let mut source = json!({"claudeAiOauth":{
        "accessToken":access, "refreshToken":refresh,
        "scopes":["user:inference"],
    }});
    for key in ["subscriptionType", "rateLimitTier"] {
        if let Some(value) = real["claudeAiOauth"][key].as_str() {
            ensure!(value.len() <= 128, "invalid Claude subscription metadata");
            source["claudeAiOauth"][key] = json!(value);
        }
    }
    let provider = crate::vault::providers::provider_for("claude")
        .context("Claude vault provider unavailable")?;
    let stub = provider
        .oauth_codec()
        .context("Claude OAuth codec unavailable")?
        .host_proxy_stub(invocation, &source)
        .map_err(anyhow::Error::msg)?;
    let access_stub = stub.credentials["claudeAiOauth"]["accessToken"]
        .as_str()
        .context("missing OAuth stub")?
        .to_owned();
    Ok(Credentials {
        guest_auth: serde_json::to_vec(&stub.credentials)?,
        stub: access_stub,
        real: access.into(),
    })
}
// Conservative adapter catalog, qualified against the runner's Claude 2.1.289
// binary. Aliases are excluded: they can route to an undisclosed default.
const MODELS: &[&str] = &[
    "claude-opus-4-8",
    "claude-opus-4-7",
    "claude-opus-4-6",
    "claude-sonnet-4-6",
];

#[derive(Clone)]
pub(crate) struct Resolved {
    pub(crate) runner_image_id: String,
    pub(crate) model: String,
    pub(crate) effort: String,
    pub(crate) catalog_digest: String,
}

impl Resolved {
    pub(crate) fn selection(model: &str, effort: &str) -> Result<()> {
        ensure!(MODELS.contains(&model), "unservable Claude text model");
        ensure!(
            matches!(effort, "low" | "medium" | "high"),
            "unsupported Claude effort"
        );
        Ok(())
    }

    pub(crate) fn new(model: &str, effort: &str, runner_image_id: String) -> Result<Self> {
        Self::selection(model, effort)?;
        ensure!(
            super::valid_digest(&runner_image_id),
            "invalid resolved image ID"
        );
        Ok(Self {
            runner_image_id,
            model: model.into(),
            effort: effort.into(),
            catalog_digest: super::digest(&serde_json::to_vec(MODELS)?),
        })
    }

    pub(crate) fn argv(&self) -> Vec<String> {
        [
            "/usr/local/bin/claude",
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--tools",
            "",
            "--strict-mcp-config",
            "--mcp-config",
            "{\"mcpServers\":{}}",
            "--disable-slash-commands",
            "--safe-mode",
            "--setting-sources",
            "",
            "--settings",
            "{\"disableAllHooks\":true,\"permissions\":{\"deny\":[\"*\"]}}",
            "--permission-mode",
            "default",
            "--permission-prompts",
            "none",
            "--no-session-persistence",
            "--max-turns",
            "1",
            "--model",
            &self.model,
            "--effort",
            &self.effort,
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }
}

pub(crate) struct Capture {
    pub(crate) frames: Vec<Value>,
    pub(crate) usage: Option<TurnUsage>,
    version: Option<String>,
    served_model: Option<String>,
    result: Option<String>,
    initialized: bool,
    exited: bool,
}

impl Capture {
    fn new() -> Self {
        Self {
            frames: Vec::new(),
            usage: None,
            version: None,
            served_model: None,
            result: None,
            initialized: false,
            exited: false,
        }
    }

    fn frame(&mut self, line: Value, max_final: usize) -> Result<()> {
        ensure!(!self.exited, "frame after harness exit");
        if line["type"] == "result" {
            // Preserve reported spend even if the terminal frame violates policy.
            self.usage = TurnUsage::from_claude_result(&line);
        }
        ensure!(
            self.frames.len() < 16_384,
            "native evidence frame limit exceeded"
        );
        self.frames
            .push(json!({"direction":"inbound", "message":line}));
        let line = &self.frames.last().unwrap()["message"];
        match (line["type"].as_str(), line["subtype"].as_str()) {
            (Some("system"), Some("init")) => {
                ensure!(
                    !self.initialized && self.result.is_none(),
                    "duplicate or late init"
                );
                ensure!(
                    line["tools"].as_array().is_some_and(Vec::is_empty),
                    "tools are enabled"
                );
                ensure!(
                    line["mcp_servers"].as_array().is_some_and(Vec::is_empty),
                    "MCP is enabled"
                );
                let version = line["claude_code_version"]
                    .as_str()
                    .context("missing harness version")?;
                ensure!(
                    !version.is_empty()
                        && version.len() <= 128
                        && version.bytes().all(|b| b.is_ascii_graphic()),
                    "invalid harness version"
                );
                self.version = Some(version.into());
                self.initialized = true;
            }
            (Some("assistant"), _) => {
                ensure!(
                    self.initialized && self.result.is_none(),
                    "assistant outside turn"
                );
                let message = &line["message"];
                ensure!(line["parent_tool_use_id"].is_null(), "subagent message");
                if let Some(reason) = message["stop_reason"].as_str() {
                    ensure!(reason == "end_turn", "truncated or tool-stopped answer");
                }
                let blocks = message["content"]
                    .as_array()
                    .context("missing assistant content")?;
                for block in blocks {
                    ensure!(
                        matches!(
                            block["type"].as_str(),
                            Some("text" | "thinking" | "redacted_thinking")
                        ),
                        "tool or unsupported assistant block"
                    );
                }
                if let Some(model) = message["model"].as_str().filter(|m| !m.is_empty()) {
                    ensure!(model.len() <= 256, "invalid served model");
                    ensure!(
                        self.served_model
                            .as_deref()
                            .is_none_or(|seen| seen == model),
                        "multiple served models"
                    );
                    self.served_model = Some(model.into());
                }
            }
            (Some("result"), Some("success")) => {
                ensure!(
                    self.initialized && self.result.is_none(),
                    "duplicate or early result"
                );
                ensure!(line["is_error"] == false, "error result");
                ensure!(
                    line["permission_denials"]
                        .as_array()
                        .is_some_and(Vec::is_empty),
                    "tool permission denial or absent denial evidence"
                );
                ensure!(line["num_turns"] == 1, "multiple turns");
                ensure!(
                    line["stop_reason"] == "end_turn",
                    "truncated result or missing stop evidence"
                );
                ensure!(
                    line["terminal_reason"] == "completed",
                    "incomplete result or missing completion evidence"
                );
                let text = line["result"].as_str().context("missing final text")?;
                ensure!(!text.trim().is_empty(), "empty final text");
                ensure!(
                    text.len() <= max_final && text.len() as u64 <= MAX_FINAL_TEXT_BYTES,
                    "final text byte limit exceeded"
                );
                self.result = Some(text.into());
            }
            (Some("system"), Some("status" | "informational" | "api_retry"))
            | (Some("rate_limit_event"), _) => {
                ensure!(self.result.is_none(), "event after result");
            }
            _ => anyhow::bail!("tool event or unsupported Claude frame"),
        }
        Ok(())
    }

    fn exited(&mut self, code: &Value) -> Result<()> {
        ensure!(
            !self.exited && self.result.is_some() && code == 0,
            "harness exited without success"
        );
        ensure!(
            self.frames.len() < 16_384,
            "native evidence frame limit exceeded"
        );
        self.frames.push(json!({"direction":"inbound","message":{"type":"pillbox_supervisor_exit","exit_code":code}}));
        self.exited = true;
        Ok(())
    }

    pub(crate) fn evidence_bytes(&self) -> Result<u64> {
        self.frames.iter().try_fold(0u64, |total, frame| {
            Ok(total + serde_json::to_vec(frame)?.len() as u64 + 1)
        })
    }

    pub(crate) fn completed(&self, resolved: &Resolved) -> Result<Value> {
        ensure!(self.exited, "missing harness exit");
        let mut detail = json!({
            "resolved": {
                "harness": "claude_code", "harness_version": self.version.as_ref().context("missing observed version")?,
                "adapter_revision": ADAPTER_REVISION, "runner_image_id": resolved.runner_image_id,
                "requested_model": resolved.model, "served_model": self.served_model,
            },
            "output_text": self.result.as_ref().context("missing result")?,
        });
        if let Some(usage) = &self.usage {
            detail["usage"] = json!(usage);
        }
        Ok(detail)
    }
}

pub(crate) struct Limits {
    pub(crate) deadline: Instant,
    pub(crate) max_frame_bytes: usize,
    pub(crate) max_evidence_bytes: u64,
    pub(crate) max_final_text_bytes: usize,
}

/// An error retains the bounded observed frames and usage for failure evidence.
pub(crate) fn run(
    mut stream: UnixStream,
    limits: Limits,
    mut live: impl FnMut() -> Result<()>,
) -> (Capture, Result<()>) {
    let mut capture = Capture::new();
    let result = (|| -> Result<()> {
        stream.set_read_timeout(Some(Duration::from_millis(50)))?;
        let mut partial = Vec::new();
        let mut used = 0u64;
        let mut buffer = [0u8; 8192];
        loop {
            live()?;
            ensure!(
                Instant::now() < limits.deadline,
                "text invocation deadline exceeded"
            );
            match stream.read(&mut buffer) {
                Ok(0) => {
                    ensure!(partial.is_empty(), "unterminated native frame");
                    break;
                }
                Ok(count) => {
                    for byte in &buffer[..count] {
                        used += 1;
                        // Account for the evidence envelope rather than only stdout.
                        ensure!(
                            used <= limits.max_evidence_bytes,
                            "native evidence byte limit exceeded"
                        );
                        if *byte == b'\n' {
                            // The bridge prefixes every stdout line with N. Only
                            // it can write E after waiting for the child to exit.
                            let line = match partial.split_first() {
                                Some((b'N', bytes)) => serde_json::from_slice(bytes)
                                    .context("invalid native JSON frame")?,
                                Some((b'E', bytes)) => {
                                    capture.exited(&serde_json::from_slice::<Value>(bytes)?)?;
                                    partial.clear();
                                    continue;
                                }
                                _ => anyhow::bail!("unsupported transport frame"),
                            };
                            let envelope_bytes =
                                serde_json::to_vec(&json!({"direction":"inbound","message":line}))?
                                    .len() as u64
                                    + 1;
                            used += envelope_bytes.saturating_sub(partial.len() as u64 + 1);
                            ensure!(
                                used <= limits.max_evidence_bytes,
                                "native evidence byte limit exceeded"
                            );
                            capture.frame(line, limits.max_final_text_bytes)?;
                            partial.clear();
                        } else {
                            ensure!(
                                partial.len() < limits.max_frame_bytes.saturating_add(1),
                                "native frame byte limit exceeded"
                            );
                            partial.push(*byte);
                        }
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => return Err(e.into()),
            }
        }
        ensure!(capture.exited, "missing harness exit");
        ensure!(
            capture.evidence_bytes()? <= limits.max_evidence_bytes,
            "native evidence byte limit exceeded"
        );
        Ok(())
    })();
    (capture, result)
}

#[cfg(test)]
pub(super) fn smoke_request() -> Value {
    let output = std::process::Command::new("bash")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/scripts/smoke/claude-text.sh"
        ))
        .arg("--request-only")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn smoke_request_uses_the_shared_output_format_wire_schema() {
        let mut request = smoke_request();
        let format: super::super::TextOutputFormat =
            serde_json::from_value(request["output_format"].clone()).unwrap();
        assert_eq!(format.kind, "text");
        assert_eq!(format.retry_count, 0);
        request["output_format"] = json!({"kind":"text","retry_count":0});
        assert!(serde_json::from_value::<super::super::TextOutputFormat>(
            request["output_format"].clone()
        )
        .is_err());
    }

    #[test]
    fn missing_credential_fails_without_configuring_access() {
        let temp = tempfile::tempdir().unwrap();
        assert!(credentials(
            &temp.path().join("missing.json"),
            "invocation-1",
            Instant::now() + Duration::from_secs(2)
        )
        .is_err());
        assert!(!temp.path().join("missing.json").exists());
    }

    #[test]
    fn guest_oauth_file_contains_no_real_tokens_or_extra_host_fields() {
        let real = json!({"claudeAiOauth":{"accessToken":"REAL_ACCESS_TOKEN","refreshToken":"REAL_REFRESH_TOKEN","subscriptionType":"pro","private":"PRIVATE_VALUE"},"secret":"PRIVATE_VALUE"});
        let fresh = fresh_credentials(&real, "invocation-1").unwrap();
        let guest = String::from_utf8(fresh.guest_auth).unwrap();
        assert!(!guest.contains("REAL_"));
        assert!(!guest.contains("PRIVATE_"));
        assert!(guest.contains(&fresh.stub));
        assert_eq!(fresh.real, "REAL_ACCESS_TOKEN");
        assert!(guest.contains("4102444800000"));
    }

    fn resolved() -> Resolved {
        Resolved::new(
            "claude-opus-4-8",
            "low",
            format!("sha256:{}", "b".repeat(64)),
        )
        .unwrap()
    }

    fn frames(text: &str) -> Vec<Value> {
        vec![
            json!({"type":"system","subtype":"init","tools":[],"mcp_servers":[],"claude_code_version":"2.1.289","model":"selection-only"}),
            json!({"type":"assistant","message":{"model":"claude-opus-4-8","stop_reason":"end_turn","content":[{"type":"text","text":text}]}}),
            json!({"type":"result","subtype":"success","is_error":false,"result":text,"num_turns":1,"permission_denials":[],"stop_reason":"end_turn","terminal_reason":"completed","total_cost_usd":0.25,"usage":{"input_tokens":3,"output_tokens":4,"cache_read_input_tokens":5,"cache_creation_input_tokens":6}}),
            json!({"type":"pillbox_harness_exit","exit_code":0}),
        ]
    }

    fn fake(lines: &[Value], max_final: usize) -> (Capture, Result<()>) {
        let (host, mut child) = UnixStream::pair().unwrap();
        let bytes = lines
            .iter()
            .map(|l| {
                if l["type"] == "pillbox_harness_exit" {
                    format!("E{}\n", l["exit_code"])
                } else {
                    format!("N{l}\n")
                }
            })
            .collect::<String>();
        let worker = std::thread::spawn(move || {
            let _ = child.write_all(bytes.as_bytes());
        });
        let observed = run(
            host,
            Limits {
                deadline: Instant::now() + Duration::from_secs(2),
                max_frame_bytes: 1_048_576,
                max_evidence_bytes: 8_388_608,
                max_final_text_bytes: max_final,
            },
            || Ok(()),
        );
        worker.join().unwrap();
        observed
    }

    #[test]
    fn selection_lowers_to_resolved_image_and_removes_all_tool_sources() {
        let resolved = resolved();
        assert_eq!(
            resolved.runner_image_id,
            format!("sha256:{}", "b".repeat(64))
        );
        assert_eq!(
            resolved.catalog_digest,
            super::super::digest(&serde_json::to_vec(MODELS).unwrap())
        );
        let argv = resolved.argv();
        for pair in [
            ["--tools", ""],
            ["--mcp-config", "{\"mcpServers\":{}}"],
            ["--setting-sources", ""],
            ["--permission-prompts", "none"],
        ] {
            assert!(argv.windows(2).any(|w| w == pair));
        }
        assert!(argv.contains(&"--strict-mcp-config".into()));
        assert!(argv.contains(&"--safe-mode".into()));
        assert!(!argv
            .iter()
            .any(|a| a.contains("bypass") || a.contains("dangerously")));
        assert!(Resolved::selection("gpt-6-luna", "low").is_err());
        assert!(Resolved::selection("sonnet", "low").is_err());
        assert!(Resolved::selection("claude-sonnet-4-5-20250929", "low").is_err());
        assert!(Resolved::selection("claude-haiku-4-5-20251001", "low").is_err());
    }

    #[test]
    fn fake_harness_success_reports_resolved_and_non_overlapping_usage() {
        let (capture, result) = fake(&frames("hello"), 32768);
        result.unwrap();
        let detail = capture.completed(&resolved()).unwrap();
        assert_eq!(detail["resolved"]["harness_version"], "2.1.289");
        assert_eq!(detail["resolved"]["served_model"], "claude-opus-4-8");
        assert_eq!(
            detail["usage"],
            json!({"cost_usd":0.25,"input_tokens":3,"output_tokens":4,"cache_read_tokens":5,"cache_write_tokens":6})
        );
    }

    #[test]
    fn fake_harness_refuses_every_tool_attempt_and_denial() {
        for tool in ["Bash", "Edit", "Write", "WebFetch", "mcp__server__read"] {
            let mut lines = frames("hello");
            lines[1]["message"]["content"] =
                json!([{"type":"tool_use","name":tool,"id":"tool-1","input":{}}]);
            assert!(fake(&lines, 32768).1.is_err(), "accepted {tool}");
        }
        for event in [
            json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"x"}]}}),
            json!({"type":"system","subtype":"permission_denied"}),
        ] {
            let mut lines = frames("hello");
            lines[1] = event;
            assert!(fake(&lines, 32768).1.is_err());
        }
        let mut lines = frames("hello");
        lines[2]["permission_denials"] = json!([{"tool_name":"Bash"}]);
        let (capture, result) = fake(&lines, 32768);
        assert!(result.is_err());
        assert!(capture.usage.is_some());
        lines[2]["permission_denials"] = Value::Null;
        assert!(fake(&lines, 32768).1.is_err());
        for field in ["tools", "mcp_servers"] {
            let mut lines = frames("hello");
            lines[0][field] = json!(["unexpected"]);
            assert!(fake(&lines, 32768).1.is_err());
        }
    }

    #[test]
    fn fake_harness_rejects_empty_truncated_oversized_and_duplicate_answers() {
        for text in ["".into(), " ".into(), "x".repeat(32769)] {
            assert!(fake(&frames(&text), 32768).1.is_err());
        }
        assert!(fake(&frames("hello"), 4).1.is_err());
        let mut lines = frames("hello");
        lines[1]["message"]["stop_reason"] = json!("max_tokens");
        assert!(fake(&lines, 32768).1.is_err());
        for field in ["stop_reason", "terminal_reason"] {
            for value in [Value::Null, json!(42), json!("max_tokens")] {
                let mut lines = frames("hello");
                lines[2][field] = value;
                assert!(fake(&lines, 32768).1.is_err());
            }
            let mut lines = frames("hello");
            lines[2].as_object_mut().unwrap().remove(field);
            assert!(fake(&lines, 32768).1.is_err());
        }
        let mut lines = frames("hello");
        lines.insert(3, lines[2].clone());
        assert!(fake(&lines, 32768).1.is_err());
        assert!(fake(&frames("hello")[..3], 32768).1.is_err());
    }

    #[test]
    fn missing_usage_and_authoritative_model_remain_absent_or_null() {
        let mut lines = frames("hello");
        lines.remove(1);
        lines[1].as_object_mut().unwrap().remove("usage");
        lines[1].as_object_mut().unwrap().remove("total_cost_usd");
        let (capture, result) = fake(&lines, 32768);
        result.unwrap();
        let detail = capture.completed(&resolved()).unwrap();
        assert!(detail.get("usage").is_none());
        assert!(detail["resolved"]["served_model"].is_null());
    }

    #[test]
    fn native_stdout_cannot_forge_a_successful_supervisor_exit() {
        let (host, mut child) = UnixStream::pair().unwrap();
        // Apparent exit originates in native stdout, followed by abnormal EOF.
        let bytes = frames("hello")
            .into_iter()
            .map(|line| format!("N{line}\n"))
            .collect::<String>();
        let worker = std::thread::spawn(move || {
            let _ = child.write_all(bytes.as_bytes());
        });
        let (_, result) = run(
            host,
            Limits {
                deadline: Instant::now() + Duration::from_secs(2),
                max_frame_bytes: 1048576,
                max_evidence_bytes: 8388608,
                max_final_text_bytes: 32768,
            },
            || Ok(()),
        );
        worker.join().unwrap();
        assert!(result.is_err());
    }

    #[test]
    fn native_limits_deadline_and_cancellation_fail_closed() {
        let wire = frames("hello")
            .iter()
            .map(|line| {
                if line["type"] == "pillbox_harness_exit" {
                    format!("E{}\n", line["exit_code"])
                } else {
                    format!("N{line}\n")
                }
            })
            .collect::<String>();
        for (frame, evidence, expired, cancelled) in [
            (4, 8388608, false, false),
            (1048576, 32, false, false),
            (1048576, 8388608, true, false),
            (1048576, 8388608, false, true),
        ] {
            let (host, mut child) = UnixStream::pair().unwrap();
            let bytes = wire.clone();
            let worker = std::thread::spawn(move || {
                let _ = child.write_all(bytes.as_bytes());
            });
            let (_, result) = run(
                host,
                Limits {
                    deadline: Instant::now()
                        + if expired {
                            Duration::ZERO
                        } else {
                            Duration::from_secs(2)
                        },
                    max_frame_bytes: frame,
                    max_evidence_bytes: evidence,
                    max_final_text_bytes: 32768,
                },
                || {
                    ensure!(!cancelled, "text invocation cancelled");
                    Ok(())
                },
            );
            worker.join().unwrap();
            assert!(result.is_err());
        }
    }
}
