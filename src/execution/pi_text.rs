//! Pi's sealed text transport. The ordinary Pi driver deliberately has tools;
//! this transport uses the image's SDK with no tools or discovered resources.

use std::io::{BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::usage::TurnUsage;

#[cfg(feature = "libkrun")]
pub(crate) mod runtime;

pub(crate) const ADAPTER_REVISION: &str = "pillbox/pi-text-v1";
pub(crate) const PROVIDER: &str = "openai-codex";
pub(crate) const PROVIDER_HOST: &str = "chatgpt.com";
pub(crate) const DRIVER: &str = include_str!("pi_text/driver.mjs");
pub(crate) const BRIDGE: &str = include_str!("pi_text/bridge.py");

pub(crate) fn failure_code(stage: &str, timed_out: bool, rejected: bool) -> &'static str {
    if timed_out {
        return "runtime_timeout";
    }
    if rejected {
        return "runtime_rejected";
    }
    match stage {
        "turn" => "runtime_protocol_error",
        "finalize" => "internal_error",
        _ => "runtime_unavailable",
    }
}

#[derive(Debug)]
pub(crate) struct FailureAtDetection {
    pub(crate) timed_out: bool,
}

impl std::fmt::Display for FailureAtDetection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Pi failure classification captured before cleanup")
    }
}

impl std::error::Error for FailureAtDetection {}

pub(crate) fn classify<T>(result: Result<T>, deadline: Instant) -> Result<T> {
    result.map_err(|error| {
        error.context(FailureAtDetection {
            timed_out: Instant::now() >= deadline,
        })
    })
}

/// Host releases are deliberately neither serializable nor printable. Only the
/// synthetic OAuth credential can enter the private guest.
pub(crate) struct Credentials {
    pub(crate) guest: Value,
    pub(crate) access: crate::vault::providers::codex_execution::CodexAccessRelease,
    pub(crate) account: crate::vault::providers::codex_execution::CodexAccessRelease,
}

impl Credentials {
    /// The bounded path leases an already-fresh managed token. Rotation belongs
    /// to the existing host broker; its blocking startup refresh has no turn
    /// deadline, so this path fails closed when renewal is needed.
    pub(crate) fn read_managed(
        path: &std::path::Path,
        invocation: &str,
        live: &mut dyn FnMut() -> Result<()>,
    ) -> Result<Self> {
        live()?;
        let file = std::fs::File::open(path).context("managed credential absent")?;
        let mut bytes = Vec::new();
        file.take(1_048_577).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 1_048_576, "managed credential size limit");
        live()?;
        let real: Value = serde_json::from_slice(&bytes).context("invalid managed credential")?;
        let provider = crate::vault::providers::provider_for("codex")
            .context("managed credential provider absent")?;
        let codec = provider
            .oauth_codec()
            .context("managed credential codec absent")?;
        ensure!(
            codec.access_usable(&real) && !codec.needs_refresh(&real),
            "managed credential requires renewal through the host broker"
        );
        let credentials = Self::from_codex(&real, invocation)?;
        live()?;
        Ok(credentials)
    }

    pub(crate) fn from_codex(real: &Value, invocation: &str) -> Result<Self> {
        let checked =
            crate::vault::providers::codex_execution::fresh_codex_credentials(real, invocation)
                .map_err(anyhow::Error::msg)?;
        let access = real["tokens"]["access_token"]
            .as_str()
            .context("managed access credential absent")?;
        let claims = access
            .split('.')
            .nth(1)
            .and_then(|part| URL_SAFE_NO_PAD.decode(part).ok())
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        let claim = claims
            .as_ref()
            .and_then(|value| value.pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id"));
        let mut accounts = [real.pointer("/tokens/account_id"), claim]
            .into_iter()
            .flatten()
            .filter(|value| !value.is_null());
        let account = accounts
            .next()
            .and_then(Value::as_str)
            .context("managed account routing identity absent")?;
        ensure!(
            !account.is_empty()
                && account.len() <= 256
                && account.bytes().all(|b| b.is_ascii_graphic()),
            "invalid managed account identity"
        );
        ensure!(
            accounts.all(|value| value.as_str() == Some(account)),
            "conflicting managed account identity"
        );
        let account_stub = format!(
            "pb-pi-account-{}",
            URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
        );
        let access_stub = format!(
            "{}.{}.{}",
            URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#),
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({"exp": 4_102_444_800_u64,
                "https://api.openai.com/auth": {"chatgpt_account_id": account_stub}}))?),
            URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
        );
        Ok(Self {
            guest: json!({"type": "oauth", "access": access_stub,
                "refresh": format!("pb-pi-refresh-{}", URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())),
                "expires": 4_102_444_800_000_u64}),
            access: crate::vault::providers::codex_execution::CodexAccessRelease {
                stub: access_stub,
                real: checked.access_release.real,
            },
            account: crate::vault::providers::codex_execution::CodexAccessRelease {
                stub: account_stub,
                real: account.into(),
            },
        })
    }
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Limits {
    pub(crate) timeout_ms: u64,
    pub(crate) max_final_text_bytes: u64,
    pub(crate) max_frame_bytes: u64,
    pub(crate) max_evidence_bytes: u64,
}

impl Limits {
    pub(crate) fn validate(self) -> Result<()> {
        ensure!(
            (1..=super::MAX_TIMEOUT_MS).contains(&self.timeout_ms),
            "invalid Pi timeout"
        );
        ensure!(
            (1..=32_768).contains(&self.max_final_text_bytes),
            "invalid Pi final-text limit"
        );
        ensure!(
            (1..=super::MAX_FRAME_BYTES).contains(&self.max_frame_bytes),
            "invalid Pi frame limit"
        );
        ensure!(
            (1..=super::MAX_EVIDENCE_BYTES).contains(&self.max_evidence_bytes)
                && self.max_frame_bytes <= self.max_evidence_bytes,
            "invalid Pi evidence limit"
        );
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Catalog {
    pub(crate) harness_version: String,
    pub(crate) models: Vec<CatalogModel>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CatalogModel {
    provider: String,
    id: String,
    base_url: String,
    efforts: Vec<String>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Plan {
    pub(crate) harness_version: String,
    pub(crate) runner_image_id: String,
    pub(crate) model: String,
    pub(crate) reasoning_effort: String,
}

impl Plan {
    pub(crate) fn resolve(
        image: String,
        catalog: Catalog,
        model: &str,
        effort: &str,
    ) -> Result<Self> {
        ensure!(super::valid_digest(&image), "invalid Pi runner image ID");
        // Versions are selected internally from the image, never from the request.
        ensure!(
            catalog.harness_version == "1.0.2",
            "unqualified Pi SDK version"
        );
        let id = model.strip_prefix("openai-codex/").unwrap_or(model);
        let mut matching = catalog
            .models
            .iter()
            .filter(|entry| entry.provider == PROVIDER && entry.id == id);
        let selected = matching
            .next()
            .context("Pi model is absent from the image catalog")?;
        ensure!(matching.next().is_none(), "ambiguous Pi model catalog");
        ensure!(
            selected.base_url == "https://chatgpt.com/backend-api",
            "unsupported Pi provider endpoint"
        );
        ensure!(
            matches!(effort, "low" | "medium" | "high")
                && selected.efforts.iter().any(|level| level == effort),
            "Pi model cannot serve requested effort"
        );
        Ok(Self {
            harness_version: catalog.harness_version,
            runner_image_id: image,
            model: id.into(),
            reasoning_effort: effort.into(),
        })
    }

    pub(crate) fn resolved(&self, requested_model: &str, served_model: Option<&str>) -> Value {
        json!({"harness": "pi", "harness_version": self.harness_version,
            "adapter_revision": ADAPTER_REVISION, "runner_image_id": self.runner_image_id,
            "requested_model": requested_model, "served_model": served_model})
    }
}

/// Capture complete JSONL frames, with an independent deadline and cancellation
/// check. No frame or text is trimmed to meet a bound.
pub(crate) struct Wire {
    stream: BufReader<UnixStream>,
    limits: Limits,
    deadline: Instant,
    partial: Vec<u8>,
    frame_count: usize,
    pub(crate) bytes: Vec<u8>,
}

impl Wire {
    pub(crate) fn new(stream: UnixStream, limits: Limits, deadline: Instant) -> Result<Self> {
        stream.set_read_timeout(Some(Duration::from_millis(100)))?;
        stream.set_write_timeout(Some(Duration::from_millis(100)))?;
        Ok(Self {
            stream: BufReader::new(stream),
            limits,
            deadline,
            partial: Vec::new(),
            frame_count: 0,
            bytes: Vec::new(),
        })
    }

    pub(crate) fn send(
        &mut self,
        request: &Value,
        live: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        let mut bytes = serde_json::to_vec(request)?;
        ensure!(
            bytes.len() as u64 <= self.limits.max_frame_bytes,
            "Pi request frame limit"
        );
        bytes.push(b'\n');
        let mut remaining = bytes.as_slice();
        while !remaining.is_empty() {
            self.check(live)?;
            match self.stream.get_mut().write(remaining) {
                Ok(0) => anyhow::bail!("Pi request channel closed"),
                Ok(n) => remaining = &remaining[n..],
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
        Ok(())
    }

    fn check(&self, live: &mut dyn FnMut() -> Result<()>) -> Result<()> {
        ensure!(Instant::now() < self.deadline, "Pi text deadline exceeded");
        live()
    }

    pub(crate) fn next(&mut self, live: &mut dyn FnMut() -> Result<()>) -> Result<Option<Value>> {
        loop {
            self.check(live)?;
            let mut byte = [0_u8; 1];
            match self.stream.read(&mut byte) {
                Ok(0) => {
                    ensure!(self.partial.is_empty(), "unterminated Pi frame");
                    return Ok(None);
                }
                Ok(_) => {
                    ensure!(
                        (self.bytes.len() as u64) < self.limits.max_evidence_bytes,
                        "Pi evidence limit"
                    );
                    self.bytes.push(byte[0]);
                    if byte[0] == b'\n' {
                        ensure!(self.frame_count < 16_384, "Pi event count limit");
                        let value: Value = serde_json::from_slice(&self.partial)
                            .context("invalid Pi JSONL frame")?;
                        ensure!(value.is_object(), "Pi frame must be an object");
                        self.partial.clear();
                        self.frame_count += 1;
                        return Ok(Some(value));
                    }
                    ensure!(
                        (self.partial.len() as u64) < self.limits.max_frame_bytes,
                        "Pi frame limit"
                    );
                    self.partial.push(byte[0]);
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
}

#[derive(Default)]
pub(crate) struct Turn {
    message: Option<Value>,
    done: Option<Value>,
    exited: bool,
    pub(crate) usage: Option<TurnUsage>,
}

impl Turn {
    pub(crate) fn observe(&mut self, line: &Value, limit: usize) -> Result<()> {
        ensure!(!self.exited, "Pi event after exit");
        let kind = line["type"].as_str().context("Pi event type missing")?;
        ensure!(
            !kind.starts_with("tool_")
                && !kind.starts_with("auto_retry")
                && !kind.starts_with("auto_compaction"),
            "Pi attempted denied tool or extra turn"
        );
        if let Some(event) = line.get("assistantMessageEvent") {
            ensure!(
                !event["type"]
                    .as_str()
                    .unwrap_or_default()
                    .starts_with("toolcall"),
                "Pi attempted denied tool"
            );
        }
        for message in line
            .get("message")
            .into_iter()
            .chain(line["messages"].as_array().into_iter().flatten())
        {
            if message["role"] == "toolResult" {
                anyhow::bail!("Pi emitted a denied tool result");
            }
            if let Some(content) = message["content"].as_array() {
                ensure!(
                    content
                        .iter()
                        .all(|block| matches!(block["type"].as_str(), Some("text" | "thinking"))),
                    "Pi attempted denied content block"
                );
            }
        }
        match kind {
            "pillbox_pi.usage" => {
                self.usage = TurnUsage::from_pi_provider_usage(&line["usage"]);
            }
            "message_end" if line["message"]["role"] == "assistant" => {
                ensure!(
                    self.message.is_none() && self.done.is_none(),
                    "Pi emitted multiple final answers"
                );
                let message = &line["message"];
                // Pi reports its usage on the terminal assistant; preserve it on
                // failures too, but only when the native provider reported usage.
                self.message = Some(message.clone());
                if let Some(native) = line.get("pillbox_pi_native_usage") {
                    self.usage = TurnUsage::from_pi_text_message(message, native);
                }
                let text = final_text(message)?;
                ensure!(text.len() <= limit, "Pi final text byte limit");
                ensure!(
                    message["stopReason"] == "stop",
                    "Pi answer was truncated or failed"
                );
                ensure!(!text.is_empty(), "Pi answer is empty");
            }
            "pillbox_pi.done" => {
                ensure!(self.done.is_none(), "duplicate Pi terminal marker");
                ensure!(self.message.is_some(), "Pi final message missing");
                self.done = Some(line.clone());
            }
            "pillbox_pi.exit" => {
                ensure!(
                    line["code"] == 0 && self.done.is_some(),
                    "Pi process did not finish successfully"
                );
                self.exited = true;
            }
            _ => ensure!(self.done.is_none(), "Pi event after terminal marker"),
        }
        Ok(())
    }

    pub(crate) fn finish(&self) -> Result<(String, Option<String>)> {
        ensure!(self.exited, "Pi process exit unconfirmed");
        let done = self.done.as_ref().context("Pi terminal marker absent")?;
        let served = done["served_model"]
            .as_str()
            .filter(|model| !model.is_empty())
            .map(str::to_owned);
        Ok((
            final_text(self.message.as_ref().context("Pi final message absent")?)?,
            served,
        ))
    }
}

fn final_text(message: &Value) -> Result<String> {
    let content = message["content"]
        .as_array()
        .context("Pi final content absent")?;
    let mut text = String::new();
    for block in content {
        match block["type"].as_str() {
            Some("text") => {
                text.push_str(block["text"].as_str().context("Pi text block malformed")?)
            }
            Some("thinking") => {}
            _ => anyhow::bail!("Pi final content denied"),
        }
    }
    Ok(text)
}

#[cfg(test)]
mod tests;
