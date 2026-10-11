//! OpenCode 2 as a `pillbox.text/2` harness: what a selection resolves to, and how
//! one captured turn folds into its final text and usage. The microVM launch lives in
//! `sandbox::libkrun::repository::opencode`; this half is pure so Linux CI runs it.
//!
//! The guest drives `opencode serve` over its loopback HTTP API (create a session,
//! prompt it) and relays each `/api/event` object to the host as one JSON line, after
//! two control frames of its own (`{"pillbox":"info"|"session"|"error",...}`). The host
//! folds those lines with [`TurnFold`]. Shapes were captured from OpenCode 2.0.24
//! (`fixtures/opencode-2.0.24-*.jsonl`), not read from its OpenAPI document.

use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};

use super::usage::TurnUsage;

/// The models.dev catalog OpenCode loads its provider and model list from at startup.
const CATALOG_HOST: &str = "models.dev";

/// OpenCode provider id → the API host its catalog entry targets (models.dev
/// `api`, or the SDK default where the entry has none). A provider not listed has
/// no egress in the text VM, so a selection naming it is rejected at `resolve`.
const PROVIDERS: &[(&str, &str)] = &[
    ("anthropic", "api.anthropic.com"),
    ("deepseek", "api.deepseek.com"),
    ("google", "generativelanguage.googleapis.com"),
    ("groq", "api.groq.com"),
    ("mistral", "api.mistral.ai"),
    ("moonshotai", "api.moonshot.ai"),
    ("moonshotai-cn", "api.moonshot.cn"),
    ("openai", "api.openai.com"),
    ("opencode", "opencode.ai"),
    ("opencode-go", "opencode.ai"),
    ("openrouter", "openrouter.ai"),
    ("xai", "api.x.ai"),
    ("zai", "api.z.ai"),
    ("zai-coding-plan", "api.z.ai"),
    ("zhipuai", "open.bigmodel.cn"),
    ("zhipuai-coding-plan", "open.bigmodel.cn"),
];

/// Set on the session so OpenCode skips its title-generation request: without a
/// title every turn makes a second, hidden model call.
const SESSION_TITLE: &str = "pillbox text turn";

/// The turn ends on the driven session's first execution outcome.
const TERMINAL: [&str; 3] = [
    "session.execution.succeeded",
    "session.execution.failed",
    "session.execution.interrupted",
];

/// One `deny` rule for every action on every resource. OpenCode drops denied tools
/// from the request entirely, so the model is offered none.
fn deny_all() -> Value {
    json!([{"action": "*", "resource": "*", "effect": "deny"}])
}

/// What an `opencode` selection runs: the session's model reference and the only
/// hosts the VM may reach.
#[derive(Debug, PartialEq)]
pub(crate) struct Plan {
    pub(crate) provider: String,
    pub(crate) model: String,
    /// OpenCode's per-model variant, named by the reasoning effort. A model without
    /// that variant fails the turn (`provider.no-route`); it is never dropped.
    pub(crate) variant: String,
    pub(crate) hosts: Vec<String>,
}

/// Resolve `provider/model` and the effort without the catalog, which only the
/// guest has. Unknown providers and Zen's free models are rejected here: the free
/// tier refuses any session with its tools removed (`FreeTierError`, 403).
pub(crate) fn plan(model: &str, reasoning_effort: &str) -> Result<Plan> {
    let (provider, model_id) = model
        .split_once('/')
        .context("OpenCode model must be provider/model")?;
    let host = PROVIDERS
        .iter()
        .find(|(id, _)| *id == provider)
        .map(|(_, host)| *host)
        .with_context(|| format!("OpenCode provider {provider} has no text egress"))?;
    ensure!(
        !model_id.is_empty()
            && model_id.len() <= 128
            && model_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/' | b':')),
        "invalid OpenCode model id"
    );
    if provider.starts_with("opencode") && model_id.ends_with("-free") {
        bail!("OpenCode Zen free models refuse tool-free sessions");
    }
    ensure!(
        !reasoning_effort.is_empty()
            && reasoning_effort.len() <= 32
            && reasoning_effort
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
        "invalid OpenCode reasoning effort"
    );
    Ok(Plan {
        provider: provider.into(),
        model: model_id.into(),
        variant: reasoning_effort.into(),
        hosts: vec![host.into(), CATALOG_HOST.into()],
    })
}

/// The guest's turn document: the server config, the session to create and the
/// prompt. Tools are denied twice, in config and on the session.
pub(crate) fn turn_document(plan: &Plan, rendered_input: &str) -> Value {
    json!({
        "config": {"permissions": deny_all()},
        "session": {
            "title": SESSION_TITLE,
            "model": {"providerID": plan.provider, "id": plan.model, "variant": plan.variant},
            "permissions": deny_all(),
        },
        "text": rendered_input,
    })
}

/// What a completed turn reported besides its text.
#[derive(Debug)]
pub(crate) struct Completed {
    pub(crate) text: String,
    pub(crate) harness_version: String,
}

/// Folds the guest's frames for one turn. Every frame is kept for evidence; only
/// the driven session's events count.
#[derive(Default)]
pub(crate) struct TurnFold {
    version: Option<String>,
    session: Option<String>,
    events: Vec<Value>,
    /// Text parts of the latest assistant message, in order.
    message: Option<String>,
    parts: Vec<String>,
    outcome: Option<String>,
}

impl TurnFold {
    /// Take one frame. `Ok(true)` once the turn reached an outcome; an error means
    /// the turn must be stopped and failed (a tool attempt, a question for a human,
    /// a guest-side failure, or a frame out of order).
    pub(crate) fn observe(&mut self, frame: &Value) -> Result<bool> {
        ensure!(
            self.outcome.is_none(),
            "OpenCode frame after the turn ended"
        );
        if let Some(control) = frame.get("pillbox").and_then(Value::as_str) {
            return self.control(control, frame).map(|()| false);
        }
        let kind = frame["type"]
            .as_str()
            .context("OpenCode event has no type")?;
        let data = &frame["data"];
        // permission/form requests carry the session too; a server-wide event does not.
        let ours = self.session.is_some() && data["sessionID"].as_str() == self.session.as_deref();
        if kind.starts_with("permission.") || kind.starts_with("form.") {
            bail!("OpenCode asked for a decision ({kind}) during a tool-free turn");
        }
        if !ours {
            return Ok(false);
        }
        self.events.push(frame.clone());
        if kind.starts_with("session.tool.") {
            bail!("OpenCode attempted a tool call ({kind}) during a tool-free turn");
        }
        match kind {
            "session.text.ended" => {
                let message = data["assistantMessageID"].as_str().map(str::to_owned);
                if message != self.message {
                    self.message = message;
                    self.parts.clear();
                }
                let text = data["text"]
                    .as_str()
                    .context("OpenCode text part has no text")?;
                self.parts.push(text.to_owned());
            }
            kind if TERMINAL.contains(&kind) => {
                self.outcome = Some(kind.to_owned());
                return Ok(true);
            }
            _ => {}
        }
        Ok(false)
    }

    fn control(&mut self, control: &str, frame: &Value) -> Result<()> {
        match control {
            "info" => {
                ensure!(self.version.is_none(), "duplicate OpenCode server info");
                let version = frame["info"]["version"]
                    .as_str()
                    .filter(|version| !version.is_empty() && version.len() <= 64)
                    .context("OpenCode server did not report its version")?;
                self.version = Some(version.to_owned());
            }
            "session" => {
                ensure!(
                    self.version.is_some() && self.session.is_none(),
                    "OpenCode session frame out of order"
                );
                let id = frame["id"]
                    .as_str()
                    .filter(|id| id.starts_with("ses") && id.len() <= 128)
                    .context("OpenCode session id is invalid")?;
                self.session = Some(id.to_owned());
            }
            "error" => bail!(
                "OpenCode guest driver failed during {}",
                frame["stage"].as_str().unwrap_or("an unknown step")
            ),
            other => bail!("unknown OpenCode guest frame {other}"),
        }
        Ok(())
    }

    /// The turn's result. Usage is reported for a failed turn too, so a caller can
    /// count what a provider error still spent.
    pub(crate) fn finish(
        self,
        max_final_text_bytes: usize,
    ) -> (Result<Completed>, Option<TurnUsage>) {
        let usage = TurnUsage::from_opencode_events(&self.events);
        let result = (|| {
            match self.outcome.as_deref() {
                Some("session.execution.succeeded") => {}
                Some(outcome) => bail!("OpenCode turn ended with {outcome}"),
                None => bail!("OpenCode turn ended without an outcome"),
            }
            let text = self.parts.join("\n\n");
            ensure!(!text.trim().is_empty(), "OpenCode returned no final text");
            ensure!(
                text.len() <= max_final_text_bytes,
                "OpenCode final text exceeds {max_final_text_bytes} bytes"
            );
            Ok(Completed {
                text,
                harness_version: self.version.context("OpenCode version is absent")?,
            })
        })();
        (result, usage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT_TURN: &str = include_str!("fixtures/opencode-2.0.24-text-turn.jsonl");
    const TOOL_ATTEMPT: &str = include_str!("fixtures/opencode-2.0.24-tool-attempt.jsonl");
    const FREE_TIER: &str = include_str!("fixtures/opencode-2.0.24-free-tier-refused.jsonl");
    const DRIVER_TURN: &str = include_str!("fixtures/opencode-2.0.24-driver-text-turn.jsonl");
    const ZAI_TURN: &str = include_str!("fixtures/opencode-2.0.24-zai-coding-plan-turn.jsonl");
    const OPENROUTER_TURN: &str = include_str!("fixtures/opencode-2.0.24-openrouter-turn.jsonl");

    fn session_of(capture: &str) -> String {
        capture
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|event| event["type"] == "session.created")
            .unwrap()["data"]["sessionID"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// Replays a capture behind the guest's control frames, as the host receives it.
    fn replay(capture: &str) -> (TurnFold, Result<bool>) {
        let mut fold = TurnFold::default();
        let mut frames = vec![
            json!({"pillbox": "info", "info": {"version": "2.0.24"}}),
            json!({"pillbox": "session", "id": session_of(capture)}),
        ];
        frames.extend(
            capture
                .lines()
                .map(|line| serde_json::from_str(line).unwrap()),
        );
        for frame in &frames {
            match fold.observe(frame) {
                Ok(false) => {}
                done => return (fold, done),
            }
        }
        (fold, Ok(false))
    }

    #[test]
    fn a_selection_resolves_to_its_provider_host_and_variant() {
        let plan = plan("zai-coding-plan/glm-4.5-air", "high").unwrap();
        assert_eq!(
            plan,
            Plan {
                provider: "zai-coding-plan".into(),
                model: "glm-4.5-air".into(),
                variant: "high".into(),
                hosts: vec!["api.z.ai".into(), "models.dev".into()],
            }
        );
        let routed = super::plan("openrouter/anthropic/claude-sonnet-4", "low").unwrap();
        assert_eq!(routed.model, "anthropic/claude-sonnet-4");
        let document = turn_document(&plan, "hello");
        assert_eq!(document["session"]["permissions"], deny_all());
        assert_eq!(document["config"]["permissions"], deny_all());
        assert_eq!(document["session"]["title"], SESSION_TITLE);
        assert_eq!(document["session"]["model"]["variant"], "high");
    }

    #[test]
    fn unservable_selections_are_rejected() {
        for (model, effort) in [
            ("glm-4.5-air", "low"),
            ("ollama/llama3", "low"),
            ("opencode/nemotron-3.5-lightning-free", "low"),
            ("opencode/", "low"),
            ("openai/gpt 5", "low"),
            ("openai/gpt-5", ""),
            ("openai/gpt-5", "High"),
        ] {
            assert!(plan(model, effort).is_err(), "{model} {effort}");
        }
    }

    #[test]
    fn captured_turn_completes_with_text_version_and_usage() {
        let (fold, done) = replay(TEXT_TURN);
        assert!(done.unwrap());
        let (completed, usage) = fold.finish(32_768);
        let completed = completed.unwrap();
        assert_eq!(completed.text, "pong");
        assert_eq!(completed.harness_version, "2.0.24");
        // The capture provider reported 120 prompt tokens (100 cached) and 7
        // completion tokens (2 reasoning) at $1/$2/$0.50 per million.
        assert_eq!(
            serde_json::to_value(usage.unwrap()).unwrap(),
            json!({"cost_usd": 0.000084, "input_tokens": 20, "output_tokens": 7,
                "cache_read_tokens": 100, "cache_write_tokens": 0})
        );
    }

    /// What the host received from the guest driver run against OpenCode 2.0.24
    /// outside a VM, control frames included.
    #[test]
    fn the_guest_driver_output_folds_to_the_answer() {
        let mut fold = TurnFold::default();
        let mut done = false;
        for line in DRIVER_TURN.lines() {
            assert!(!done, "frames after the outcome");
            done = fold.observe(&serde_json::from_str(line).unwrap()).unwrap();
        }
        assert!(done);
        let (completed, usage) = fold.finish(32_768);
        assert_eq!(completed.unwrap().text, "pong");
        assert_eq!(usage.unwrap().output_tokens, Some(7));
    }

    /// The guest driver's output from real GLM turns, z.ai's coding plan
    /// (which models.dev prices at zero) and OpenRouter, run outside a VM.
    #[test]
    fn live_glm_turns_fold_to_the_answer_and_usage() {
        for (capture, model, cost, input, output) in [
            (ZAI_TURN, "zai-coding-plan/glm-5.3-flash", 0.0, 354, 42),
            (
                OPENROUTER_TURN,
                "openrouter/z-ai/glm-4.5-air",
                0.00010147,
                349,
                66,
            ),
        ] {
            assert!(plan(model, "low").is_ok(), "{model}");
            let mut fold = TurnFold::default();
            let mut done = false;
            for line in capture.lines() {
                assert!(!done, "frames after the outcome");
                done = fold.observe(&serde_json::from_str(line).unwrap()).unwrap();
            }
            assert!(done, "{model}");
            let (completed, usage) = fold.finish(32_768);
            let completed = completed.unwrap();
            assert_eq!(completed.text, "The capital of France is Paris.");
            assert_eq!(completed.harness_version, "2.0.24");
            let usage = usage.unwrap();
            assert_eq!(usage.cost_usd, Some(cost), "{model}");
            assert_eq!(usage.input_tokens, Some(input), "{model}");
            assert_eq!(usage.output_tokens, Some(output), "{model}");
        }
    }

    #[test]
    fn a_tool_attempt_stops_the_turn() {
        let (_, done) = replay(TOOL_ATTEMPT);
        let error = done.unwrap_err().to_string();
        assert!(error.contains("session.tool.input.started"), "{error}");
    }

    #[test]
    fn a_decision_request_stops_the_turn() {
        let mut fold = TurnFold::default();
        let asked = json!({"type": "permission.asked", "data": {"sessionID": "ses_x"}});
        assert!(fold.observe(&asked).is_err());
    }

    #[test]
    fn a_provider_failure_fails_the_turn() {
        let (fold, done) = replay(FREE_TIER);
        assert!(done.unwrap());
        let (completed, usage) = fold.finish(32_768);
        let error = completed.unwrap_err().to_string();
        assert!(error.contains("session.execution.failed"), "{error}");
        assert_eq!(usage, None);
    }

    #[test]
    fn an_answer_over_the_limit_or_empty_fails() {
        let (fold, _) = replay(TEXT_TURN);
        let (completed, usage) = fold.finish(3);
        assert!(completed
            .unwrap_err()
            .to_string()
            .contains("exceeds 3 bytes"));
        assert!(usage.is_some(), "a failed turn still reports what it spent");

        let mut empty = TEXT_TURN.replace(r#""text":"pong""#, r#""text":"  ""#);
        empty = empty.replace(r#""delta":"pong""#, r#""delta":"  ""#);
        let (fold, _) = replay(&empty);
        let error = fold.finish(32_768).0.unwrap_err().to_string();
        assert!(error.contains("no final text"), "{error}");
    }

    #[test]
    fn other_sessions_and_guest_errors_are_handled() {
        let mut fold = TurnFold::default();
        fold.observe(&json!({"pillbox": "info", "info": {"version": "2.0.24"}}))
            .unwrap();
        fold.observe(&json!({"pillbox": "session", "id": "ses_mine"}))
            .unwrap();
        let foreign = json!({"type": "session.tool.called", "data": {"sessionID": "ses_other"}});
        assert!(!fold.observe(&foreign).unwrap());
        let failed = json!({"pillbox": "error", "stage": "prompt", "status": 400});
        assert!(fold
            .observe(&failed)
            .unwrap_err()
            .to_string()
            .contains("during prompt"));
        let mut early = TurnFold::default();
        assert!(early
            .observe(&json!({"pillbox": "session", "id": "ses_mine"}))
            .is_err());
    }
}
