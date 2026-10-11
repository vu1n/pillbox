//! OpenCode 2's sealed text profile and strict, bounded native turn collector.
//! The ordinary server driver has different tool and credential contracts.
// Context: doc://pillbox/agent-io-pty-free-contract@0002#agent-io-pty-free-contract — normalize native structured events without a PTY.

#![cfg_attr(not(feature = "libkrun"), allow(dead_code))]

use std::collections::{BTreeMap, HashSet};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Instant;

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::execution::usage::TurnUsage;

pub(crate) const ADAPTER_REVISION: &str = "pillbox/opencode-text-v2/1";
pub(crate) const IMAGE_METADATA: &str = "/opt/pillbox-opencode-text/catalog.json";
pub(crate) const BRIDGE: &str = include_str!("../../execution/opencode_bridge.py");

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Catalog {
    pub(crate) version: String,
    pub(crate) models: Vec<Value>,
}

pub(crate) struct Profile {
    pub(crate) image_id: String,
    pub(crate) version: String,
    pub(crate) model: String,
    pub(crate) model_ref: Value,
    pub(crate) credential_ref: &'static str,
    pub(crate) host: &'static str,
}

impl Profile {
    pub(crate) fn lower(
        image_id: String,
        catalog: &Catalog,
        model: &str,
        effort: &str,
    ) -> Result<Self> {
        ensure!(
            crate::execution::valid_digest(&image_id),
            "invalid image ID"
        );
        ensure!(
            catalog.version.starts_with("2."),
            "unsupported OpenCode protocol"
        );
        let (provider, id) = model.split_once('/').context("model must be provider/id")?;
        let (credential_ref, host) = match provider {
            "anthropic" => ("ANTHROPIC_API_KEY", "api.anthropic.com"),
            "openai" => ("OPENAI_API_KEY", "api.openai.com"),
            _ => bail!("provider has no sealed vault text profile"),
        };
        let matches: Vec<_> = catalog
            .models
            .iter()
            .filter(|m| m["providerID"] == provider && m["id"] == id && m["enabled"] == true)
            .collect();
        ensure!(
            matches.len() == 1,
            "model is not uniquely servable in the image catalog"
        );
        let selected = matches[0];
        ensure!(
            selected["capabilities"]["input"]
                .as_array()
                .is_some_and(|v| v.contains(&json!("text")))
                && selected["capabilities"]["output"]
                    .as_array()
                    .is_some_and(|v| v.contains(&json!("text"))),
            "model does not support text"
        );
        let variants = selected["variants"]
            .as_array()
            .context("model variants absent")?;
        ensure!(
            variants.iter().any(|v| v["id"] == effort),
            "unsupported reasoning effort"
        );
        Ok(Self {
            image_id,
            version: catalog.version.clone(),
            model: model.into(),
            model_ref: json!({"providerID": provider, "id": id, "variant": effort}),
            credential_ref,
            host,
        })
    }

    pub(crate) fn resolved(&self, observed_version: &str) -> Value {
        json!({"harness": "opencode", "harness_version": observed_version,
            "adapter_revision": ADAPTER_REVISION, "runner_image_id": self.image_id,
            "requested_model": self.model,
            // step.started reports a selected catalog reference, not the upstream served model.
            "served_model": null})
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Limits {
    pub(crate) deadline: Instant,
    pub(crate) frame: usize,
    pub(crate) evidence: usize,
    pub(crate) final_text: usize,
}

pub(crate) struct Turn {
    pub(crate) frames: Vec<Value>,
    pub(crate) usage: Option<TurnUsage>,
    pub(crate) version: Option<String>,
    pub(crate) text: String,
    session: Option<String>,
    step: Option<String>,
    blocks: BTreeMap<u64, String>,
    text_bytes: usize,
    open: HashSet<u64>,
    ended: bool,
}

impl Turn {
    pub(crate) fn new() -> Self {
        Self {
            frames: Vec::new(),
            usage: None,
            version: None,
            text: String::new(),
            session: None,
            step: None,
            blocks: BTreeMap::new(),
            text_bytes: 0,
            open: HashSet::new(),
            ended: false,
        }
    }

    /// The bridge carries one JSON envelope per line. Read incrementally: no
    /// BufRead::read_line allocation can run ahead of the caller's frame limit.
    pub(crate) fn run(
        &mut self,
        stream: &mut UnixStream,
        profile: &Profile,
        prompt: &str,
        limits: Limits,
        mut live: impl FnMut() -> Result<()>,
    ) -> Result<()> {
        ensure!(
            (1..=32_768).contains(&limits.final_text),
            "invalid text limit"
        );
        let request = json!({"model": profile.model_ref, "prompt": prompt,
            "max_frame_bytes": limits.frame, "max_evidence_bytes": limits.evidence});
        let mut bytes = serde_json::to_vec(&request)?;
        bytes.push(b'\n');
        let mut sent = 0;
        while sent < bytes.len() {
            live()?;
            ensure!(Instant::now() < limits.deadline, "text deadline exceeded");
            match stream.write(&bytes[sent..]) {
                Ok(0) => bail!("OpenCode request channel closed"),
                Ok(n) => sent += n,
                Err(e) if retryable(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }
        let mut line = Vec::new();
        let mut total = 0usize;
        let mut chunk = [0; 8192];
        loop {
            live()?;
            ensure!(Instant::now() < limits.deadline, "text deadline exceeded");
            match stream.read(&mut chunk) {
                Ok(0) => bail!("OpenCode stream ended before complete final text"),
                Ok(n) => {
                    for byte in &chunk[..n] {
                        total = total.checked_add(1).context("evidence size overflow")?;
                        ensure!(total <= limits.evidence, "native evidence limit exceeded");
                        if *byte != b'\n' {
                            ensure!(line.len() < limits.frame, "native frame limit exceeded");
                            line.push(*byte);
                            continue;
                        }
                        let ev: Value =
                            serde_json::from_slice(&line).context("invalid native frame")?;
                        line.clear();
                        ensure!(
                            self.frames.len() < 16_384,
                            "native frame count limit exceeded"
                        );
                        self.frames.push(ev.clone());
                        if self.on_event(&ev, profile, limits.final_text)? {
                            return Ok(());
                        }
                    }
                }
                Err(e) if retryable(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }
    }

    fn on_event(&mut self, ev: &Value, profile: &Profile, max_text: usize) -> Result<bool> {
        let ty = ev["type"].as_str().context("native event type absent")?;
        let d = &ev["data"];
        match ty {
            "pillbox.info" => {
                ensure!(self.version.is_none(), "duplicate harness version");
                let version = ev["version"].as_str().context("harness version absent")?;
                ensure!(version == profile.version, "image/harness version mismatch");
                self.version = Some(version.into());
                return Ok(false);
            }
            "pillbox.session" => {
                ensure!(
                    self.version.is_some() && self.session.is_none(),
                    "invalid session order"
                );
                let id = ev["id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .context("session ID absent")?;
                self.session = Some(id.into());
                return Ok(false);
            }
            "pillbox.error" => bail!("guest bridge failed"),
            "permission.asked" | "form.created" => bail!("deny_all permission or form attempted"),
            _ => {}
        }
        if !ty.starts_with("session.") {
            return Ok(false);
        }
        let session = self
            .session
            .as_ref()
            .context("session event before session binding")?;
        ensure!(d["sessionID"] == *session, "event from another session");
        ensure!(
            !ty.starts_with("session.tool.") && !ty.starts_with("session.shell."),
            "deny_all tool attempted"
        );
        match ty {
            "session.step.started" => {
                ensure!(self.step.is_none(), "multiple model steps in text turn");
                ensure!(
                    d["model"] == profile.model_ref,
                    "harness selected another model"
                );
                let id = d["assistantMessageID"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .context("step ID absent")?;
                self.step = Some(id.into());
            }
            "session.text.started" | "session.text.delta" | "session.text.ended" => {
                ensure!(
                    self.step
                        .as_ref()
                        .is_some_and(|id| d["assistantMessageID"] == *id),
                    "text from another step"
                );
                let ordinal = d["ordinal"].as_u64().context("text ordinal absent")?;
                match ty {
                    "session.text.started" => {
                        ensure!(!self.blocks.contains_key(&ordinal), "duplicate text block");
                        self.blocks.insert(ordinal, String::new());
                        self.open.insert(ordinal);
                    }
                    "session.text.delta" => {
                        ensure!(self.open.contains(&ordinal), "delta outside text block");
                        let text = d["delta"].as_str().context("text delta absent")?;
                        let total = self.text_bytes.saturating_add(text.len());
                        ensure!(total <= max_text, "final text limit exceeded");
                        self.blocks.get_mut(&ordinal).unwrap().push_str(text);
                        self.text_bytes = total;
                    }
                    _ => {
                        ensure!(self.open.remove(&ordinal), "ended outside text block");
                        let text = d["text"].as_str().context("final block text absent")?;
                        let previous = self.blocks.get(&ordinal).unwrap();
                        ensure!(
                            previous.is_empty() || previous == text,
                            "truncated text stream"
                        );
                        let total = (self.text_bytes - previous.len()).saturating_add(text.len());
                        ensure!(total <= max_text, "final text limit exceeded");
                        self.blocks.insert(ordinal, text.into());
                        self.text_bytes = total;
                    }
                }
            }
            "session.step.ended" | "session.step.failed" => {
                ensure!(
                    self.step
                        .as_ref()
                        .is_some_and(|id| d["assistantMessageID"] == *id)
                        && !self.ended,
                    "invalid step completion"
                );
                self.usage = TurnUsage::from_opencode_steps(&self.frames, session);
                ensure!(
                    ty == "session.step.ended" && d["finish"] == "stop",
                    "model did not finish text normally"
                );
                self.ended = true;
            }
            "session.execution.succeeded" => {
                ensure!(self.ended && self.open.is_empty(), "incomplete text turn");
                self.text = self.blocks.values().cloned().collect();
                ensure!(
                    !self.text.trim().is_empty() && self.text.len() <= max_text,
                    "empty or oversized final text"
                );
                return Ok(true);
            }
            "session.execution.failed" | "session.execution.interrupted" => {
                bail!("native execution failed")
            }
            _ => {}
        }
        Ok(false)
    }
}

fn retryable(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::Interrupted
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::time::Duration;

    pub(crate) fn catalog() -> Catalog {
        Catalog {
            version: "2.0.24".into(),
            models: vec![json!({"providerID": "openai",
            "id": "gpt-6-luna", "modelID": "gpt-6-luna", "enabled": true,
            "capabilities": {"input": ["text"], "output": ["text"]},
            "variants": [{"id": "low"}]})],
        }
    }

    pub(crate) fn profile() -> Profile {
        Profile::lower(
            format!("sha256:{}", "b".repeat(64)),
            &catalog(),
            "openai/gpt-6-luna",
            "low",
        )
        .unwrap()
    }

    fn event(ty: &str, mut data: Value) -> Value {
        data["sessionID"] = json!("ses_text");
        data["assistantMessageID"] = json!("msg_text");
        json!({"type": ty, "data": data})
    }

    fn frames(text: &str) -> Vec<Value> {
        vec![
            json!({"type": "pillbox.info", "version": "2.0.24"}),
            json!({"type": "pillbox.session", "id": "ses_text"}),
            event("session.created", json!({})),
            event(
                "session.step.started",
                json!({"model": profile().model_ref}),
            ),
            event("session.text.started", json!({"ordinal": 0})),
            event("session.text.delta", json!({"ordinal": 0, "delta": text})),
            event("session.text.ended", json!({"ordinal": 0, "text": text})),
            event(
                "session.step.ended",
                json!({"finish": "stop", "cost": 0.25,
                "tokens": {"input": 10, "output": 5, "reasoning": 3, "cache": {"read": 20, "write": 4}}}),
            ),
            event("session.execution.succeeded", json!({})),
        ]
    }

    fn fake_harness(frames: Vec<Value>, frame_limit: usize, evidence: usize) -> (Turn, Result<()>) {
        let (mut host, mut guest) = UnixStream::pair().unwrap();
        host.set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        host.set_write_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let harness = std::thread::spawn(move || {
            let mut line = String::new();
            BufReader::new(guest.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["model"], profile().model_ref);
            assert_eq!(request["prompt"], "hello");
            assert!(request.get("credentials").is_none());
            for frame in frames {
                let mut bytes = serde_json::to_vec(&frame).unwrap();
                bytes.push(b'\n');
                if guest.write_all(&bytes).is_err() {
                    break;
                }
            }
        });
        let mut turn = Turn::new();
        let result = turn.run(
            &mut host,
            &profile(),
            "hello",
            Limits {
                deadline: Instant::now() + Duration::from_secs(2),
                frame: frame_limit,
                evidence,
                final_text: 32_768,
            },
            || Ok(()),
        );
        drop(host);
        harness.join().unwrap();
        (turn, result)
    }

    #[test]
    fn selection_lowers_only_catalog_models_onto_resolved_image() {
        let profile = profile();
        assert_eq!(profile.image_id, format!("sha256:{}", "b".repeat(64)));
        assert_eq!(profile.credential_ref, "OPENAI_API_KEY");
        assert_eq!(profile.host, "api.openai.com");
        for (model, effort) in [
            ("openai/imaginary", "low"),
            ("openai/gpt-6-luna", "ultra"),
            ("evil/gpt-6-luna", "low"),
            ("gpt-6-luna", "low"),
        ] {
            assert!(Profile::lower(profile.image_id.clone(), &catalog(), model, effort).is_err());
        }
        let mut disabled = catalog();
        disabled.models[0]["enabled"] = json!(false);
        assert!(Profile::lower(profile.image_id, &disabled, "openai/gpt-6-luna", "low").is_err());
    }

    #[test]
    fn fake_harness_success_has_observed_resolved_and_nonoverlapping_usage() {
        let (turn, result) = fake_harness(frames("hello 🌍"), 1_048_576, 8_388_608);
        result.unwrap();
        assert_eq!(turn.text, "hello 🌍");
        assert_eq!(
            profile().resolved(turn.version.as_deref().unwrap()),
            json!({
            "harness": "opencode", "harness_version": "2.0.24", "adapter_revision": ADAPTER_REVISION,
            "runner_image_id": profile().image_id, "requested_model": "openai/gpt-6-luna", "served_model": null})
        );
        assert_eq!(
            serde_json::to_value(turn.usage).unwrap(),
            json!({"cost_usd": 0.25,
            "input_tokens": 10, "output_tokens": 8, "cache_read_tokens": 20, "cache_write_tokens": 4})
        );
    }

    #[test]
    fn fake_harness_tool_attempts_are_refused() {
        for name in [
            "shell",
            "write",
            "edit",
            "mcp_server_tool",
            "webfetch",
            "execute",
        ] {
            let mut frames = frames("unused");
            frames.insert(
                4,
                event(
                    "session.tool.input.started",
                    json!({"id": "tool_1", "name": name}),
                ),
            );
            let (turn, result) = fake_harness(frames, 1_048_576, 8_388_608);
            assert!(format!("{:#}", result.unwrap_err()).contains("deny_all"));
            assert!(turn.text.is_empty());
        }
    }

    #[test]
    fn empty_oversized_truncated_or_abnormally_finished_answers_fail() {
        for text in ["".to_owned(), " ".into(), "é".repeat(16_385)] {
            let (turn, result) = fake_harness(frames(&text), 1_048_576, 8_388_608);
            assert!(result.is_err());
            assert!(turn.text.len() <= 32_768);
        }
        let mut truncated = frames("partial");
        truncated[6]["data"]["text"] = json!("partial with missing delta");
        assert!(fake_harness(truncated, 1_048_576, 8_388_608).1.is_err());
        let mut length = frames("partial");
        length[7]["data"]["finish"] = json!("length");
        let (turn, result) = fake_harness(length, 1_048_576, 8_388_608);
        assert!(result.is_err());
        assert!(turn.usage.is_some());
        let mut early = frames("partial");
        early.remove(6);
        assert!(fake_harness(early, 1_048_576, 8_388_608).1.is_err());
    }

    #[test]
    fn many_sequential_blocks_obey_the_aggregate_text_limit() {
        let mut fragmented = frames("unused");
        fragmented.splice(
            4..7,
            (0..1024).flat_map(|ordinal| {
                [
                    event("session.text.started", json!({"ordinal": ordinal})),
                    event(
                        "session.text.ended",
                        json!({"ordinal": ordinal, "text": "x".repeat(32)}),
                    ),
                ]
            }),
        );
        let (turn, result) = fake_harness(fragmented.clone(), 1_048_576, 8_388_608);
        result.unwrap();
        assert_eq!(turn.text, "x".repeat(32_768));
        assert_eq!(turn.text_bytes, 32_768);
        fragmented.splice(
            2052..2052,
            [
                event("session.text.started", json!({"ordinal": 1024})),
                event("session.text.ended", json!({"ordinal": 1024, "text": "x"})),
            ],
        );
        let (turn, result) = fake_harness(fragmented, 1_048_576, 8_388_608);
        assert!(result.is_err());
        assert!(turn.text.is_empty());
        assert_eq!(turn.text_bytes, 32_768);
    }

    #[test]
    fn frame_evidence_eof_and_identity_are_strictly_bounded() {
        assert!(fake_harness(frames("hello"), 40, 8_388_608).1.is_err());
        assert!(fake_harness(frames("hello"), 1_048_576, 120).1.is_err());
        let mut eof = frames("hello");
        eof.pop();
        assert!(fake_harness(eof, 1_048_576, 8_388_608).1.is_err());
        let mut other = frames("hello");
        other[8]["data"]["sessionID"] = json!("child");
        assert!(fake_harness(other, 1_048_576, 8_388_608).1.is_err());
        let mut version = frames("hello");
        version[0]["version"] = json!("1.0");
        assert!(fake_harness(version, 1_048_576, 8_388_608).1.is_err());
    }

    #[test]
    fn timeout_and_cancel_are_executable_transport_gates() {
        let (mut host, _guest) = UnixStream::pair().unwrap();
        let mut turn = Turn::new();
        let mut limits = Limits {
            deadline: Instant::now(),
            frame: 1000,
            evidence: 1000,
            final_text: 10,
        };
        assert!(format!(
            "{:#}",
            turn.run(&mut host, &profile(), "hello", limits, || Ok(()))
                .unwrap_err()
        )
        .contains("deadline exceeded"));
        limits.deadline = Instant::now() + Duration::from_secs(1);
        assert!(turn
            .run(&mut host, &profile(), "hello", limits, || bail!(
                "cancelled"
            ))
            .is_err());
    }
}
