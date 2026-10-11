//! A turn's spend as the harness itself reported it, in one closed shape that a
//! text completion carries and a caller (Huddles) can enforce a spend cap on.
//!
//! Pillbox never prices tokens: `cost_usd` is the harness's own figure or null.
//! Every field is validated on the way in, and a value that is not a sane number
//! is dropped rather than replaced, so "not reported" never turns into a zero.
//! When nothing valid is left, there is no usage at all.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Upper bounds shared with the run cost envelope (`crate::cost`).
const MAX_TOKENS: u64 = 10_000_000_000;
const MAX_COST_USD: f64 = 1_000_000.0;

/// Token counts do not overlap: `input_tokens` excludes cache reads and writes,
/// which have their own fields, and reasoning tokens are inside `output_tokens`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TurnUsage {
    /// The harness's reported cost; null when it reports tokens but no cost.
    pub(crate) cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cache_write_tokens: Option<u64>,
}

impl TurnUsage {
    fn new(
        cost_usd: Option<f64>,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cache_read_tokens: Option<u64>,
        cache_write_tokens: Option<u64>,
    ) -> Option<Self> {
        let usage = Self {
            cost_usd: cost_usd
                .filter(|cost| cost.is_finite() && (0.0..=MAX_COST_USD).contains(cost)),
            input_tokens: input_tokens.filter(|n| *n <= MAX_TOKENS),
            output_tokens: output_tokens.filter(|n| *n <= MAX_TOKENS),
            cache_read_tokens: cache_read_tokens.filter(|n| *n <= MAX_TOKENS),
            cache_write_tokens: cache_write_tokens.filter(|n| *n <= MAX_TOKENS),
        };
        let reported = usage.cost_usd.is_some()
            || usage.input_tokens.is_some()
            || usage.output_tokens.is_some()
            || usage.cache_read_tokens.is_some()
            || usage.cache_write_tokens.is_some();
        reported.then_some(usage)
    }

    /// Claude Code's stream-json `result` line: `total_cost_usd` and `usage`
    /// (`input_tokens`, `output_tokens`, `cache_read_input_tokens`,
    /// `cache_creation_input_tokens`), already non-overlapping. On a subscription
    /// `total_cost_usd` is the API-equivalent figure Claude computes, not a bill.
    pub(crate) fn from_claude_result(line: &Value) -> Option<Self> {
        let usage = &line["usage"];
        Self::new(
            line["total_cost_usd"].as_f64(),
            usage["input_tokens"].as_u64(),
            usage["output_tokens"].as_u64(),
            usage["cache_read_input_tokens"].as_u64(),
            usage["cache_creation_input_tokens"].as_u64(),
        )
    }

    /// One OpenCode 2 turn on a fresh session, from that session's events. The
    /// latest `session.usage.updated` is the session total, which also covers model
    /// calls outside a step (title generation, compaction); without one, the
    /// `session.step.ended`/`.failed` figures are summed. OpenCode's `cost` is its
    /// catalog price for the tokens. Its `input` already excludes cache reads and
    /// writes, and `reasoning` is reported beside `output`, so it is added back.
    pub(crate) fn from_opencode_events(events: &[Value]) -> Option<Self> {
        let total = events
            .iter()
            .rev()
            .find(|event| event["type"] == "session.usage.updated");
        let reports: Vec<&Value> = match total {
            Some(event) => vec![&event["data"]],
            None => events
                .iter()
                .filter(|event| {
                    matches!(
                        event["type"].as_str(),
                        Some("session.step.ended" | "session.step.failed")
                    )
                })
                .map(|event| &event["data"])
                .collect(),
        };
        if reports.is_empty() {
            return None;
        }
        let count = |report: &Value, path: &str| report.pointer(path).and_then(Value::as_u64);
        let sum = |field: &dyn Fn(&Value) -> Option<u64>| {
            reports
                .iter()
                .try_fold(0u64, |total, report| total.checked_add(field(report)?))
        };
        let output = |report: &Value| {
            count(report, "/tokens/output")?
                .checked_add(count(report, "/tokens/reasoning").unwrap_or(0))
        };
        let cost = reports
            .iter()
            .try_fold(0f64, |total, report| Some(total + report["cost"].as_f64()?));
        Self::new(
            cost,
            sum(&|report| count(report, "/tokens/input")),
            sum(&output),
            sum(&|report| count(report, "/tokens/cache/read")),
            sum(&|report| count(report, "/tokens/cache/write")),
        )
    }

    /// The Codex app-server turn in `frames` (native evidence envelopes): the
    /// newest `thread/tokenUsage/updated` correlated with the thread and turn that
    /// `thread/start` and `turn/start` returned. Its `total` is the turn's usage
    /// because a text invocation runs exactly one turn on a fresh thread. Codex
    /// reports no cost. Its `inputTokens` contains `cachedInputTokens` and
    /// `cacheWriteInputTokens` (see `events::codex_serve`), so `input_tokens` is
    /// the remainder; a breakdown whose cached counts exceed its input drops
    /// `input_tokens`.
    pub(crate) fn from_codex_frames(frames: &[Value]) -> Option<Self> {
        let inbound = || {
            frames
                .iter()
                .filter(|frame| frame["direction"] == "inbound")
                .map(|frame| &frame["message"])
        };
        let response = |id: &str| inbound().find(|message| message["id"] == id);
        let thread_id = response("pillbox-thread")?["result"]["thread"]["id"].as_str()?;
        let turn_id = response("pillbox-turn")?["result"]["turn"]["id"].as_str()?;
        let total = inbound()
            .rev()
            .find(|message| {
                message["method"] == "thread/tokenUsage/updated"
                    && message["params"]["threadId"] == thread_id
                    && message["params"]["turnId"] == turn_id
            })
            .map(|message| &message["params"]["tokenUsage"]["total"])?;
        let count = |key: &str| total[key].as_u64();
        let cached = count("cachedInputTokens");
        let cache_write = count("cacheWriteInputTokens");
        let input = count("inputTokens").and_then(|input| {
            input
                .checked_sub(cached.unwrap_or(0))?
                .checked_sub(cache_write.unwrap_or(0))
        });
        Self::new(None, input, count("outputTokens"), cached, cache_write)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn inbound(message: Value) -> Value {
        json!({"direction": "inbound", "message": message})
    }

    fn codex_turn(updates: &[Value]) -> Vec<Value> {
        let mut frames = vec![
            json!({"direction": "outbound", "message": {"id": "pillbox-thread", "method": "thread/start"}}),
            inbound(json!({"id": "pillbox-thread", "result": {"thread": {"id": "thread-1"}}})),
            inbound(json!({"id": "pillbox-turn", "result": {"turn": {"id": "turn-1"}}})),
        ];
        frames.extend(updates.iter().cloned().map(inbound));
        frames
    }

    fn token_update(thread: &str, turn: &str, total: Value) -> Value {
        json!({"method": "thread/tokenUsage/updated", "params": {
            "threadId": thread, "turnId": turn,
            "tokenUsage": {"total": total, "last": {"inputTokens": 1}}}})
    }

    #[test]
    fn claude_result_reports_cost_and_tokens() {
        let line = json!({"type": "result", "subtype": "success", "total_cost_usd": 0.0123,
            "usage": {"input_tokens": 12, "output_tokens": 340, "cache_read_input_tokens": 5000,
                "cache_creation_input_tokens": 800, "server_tool_use": {"web_search_requests": 0}}});
        let usage = TurnUsage::from_claude_result(&line).unwrap();
        assert_eq!(
            serde_json::to_value(&usage).unwrap(),
            json!({"cost_usd": 0.0123, "input_tokens": 12, "output_tokens": 340,
                "cache_read_tokens": 5000, "cache_write_tokens": 800})
        );
    }

    #[test]
    fn nothing_reported_is_no_usage() {
        assert_eq!(
            TurnUsage::from_claude_result(&json!({"type": "result"})),
            None
        );
        assert_eq!(TurnUsage::from_codex_frames(&codex_turn(&[])), None);
        assert_eq!(TurnUsage::from_codex_frames(&[]), None);
    }

    #[test]
    fn invalid_values_are_dropped_not_zeroed() {
        for cost in [f64::NAN, f64::INFINITY, -0.01, 1_000_000.5] {
            assert_eq!(TurnUsage::new(Some(cost), None, None, None, None), None);
        }
        let line = json!({"total_cost_usd": -1.0, "usage": {"input_tokens": -5,
            "output_tokens": 1.5, "cache_read_input_tokens": "7",
            "cache_creation_input_tokens": 20_000_000_000_u64}});
        assert_eq!(TurnUsage::from_claude_result(&line), None);
        let line =
            json!({"total_cost_usd": "0.5", "usage": {"input_tokens": 3, "output_tokens": -1}});
        let usage = TurnUsage::from_claude_result(&line).unwrap();
        assert_eq!(
            serde_json::to_value(&usage).unwrap(),
            json!({"cost_usd": null, "input_tokens": 3})
        );
        let free = TurnUsage::from_claude_result(&json!({"total_cost_usd": 0})).unwrap();
        assert_eq!(free.cost_usd, Some(0.0));
    }

    #[test]
    fn codex_turn_reports_the_correlated_total_without_cost() {
        let frames = codex_turn(&[
            token_update(
                "thread-1",
                "turn-1",
                json!({"inputTokens": 100, "outputTokens": 5}),
            ),
            token_update(
                "thread-1",
                "turn-1",
                json!({"inputTokens": 900, "cachedInputTokens": 600,
                "cacheWriteInputTokens": 50, "outputTokens": 40, "reasoningOutputTokens": 30,
                "totalTokens": 940}),
            ),
            token_update(
                "thread-1",
                "turn-other",
                json!({"inputTokens": 1, "outputTokens": 1}),
            ),
            token_update(
                "thread-other",
                "turn-1",
                json!({"inputTokens": 1, "outputTokens": 1}),
            ),
        ]);
        let usage = TurnUsage::from_codex_frames(&frames).unwrap();
        assert_eq!(
            serde_json::to_value(&usage).unwrap(),
            json!({"cost_usd": null, "input_tokens": 250, "output_tokens": 40,
                "cache_read_tokens": 600, "cache_write_tokens": 50})
        );
    }

    #[test]
    fn codex_cached_count_above_input_drops_only_input() {
        let frames = codex_turn(&[token_update(
            "thread-1",
            "turn-1",
            json!({"inputTokens": 10, "cachedInputTokens": 20, "outputTokens": 3}),
        )]);
        let usage = TurnUsage::from_codex_frames(&frames).unwrap();
        assert_eq!(usage.input_tokens, None);
        assert_eq!(usage.cache_read_tokens, Some(20));
        assert_eq!(usage.output_tokens, Some(3));
    }

    fn opencode(kind: &str, cost: f64, input: u64, output: u64, reasoning: u64) -> Value {
        json!({"type": kind, "data": {"sessionID": "ses_1", "cost": cost, "tokens": {
            "input": input, "output": output, "reasoning": reasoning,
            "cache": {"read": 10, "write": 1}}}})
    }

    #[test]
    fn opencode_prefers_the_session_total_and_counts_reasoning_as_output() {
        let steps = [
            opencode("session.step.ended", 0.25, 100, 5, 2),
            opencode("session.step.failed", 0.5, 50, 1, 0),
        ];
        let summed = TurnUsage::from_opencode_events(&steps).unwrap();
        assert_eq!(
            serde_json::to_value(&summed).unwrap(),
            json!({"cost_usd": 0.75, "input_tokens": 150, "output_tokens": 8,
                "cache_read_tokens": 20, "cache_write_tokens": 2})
        );
        let mut with_total = steps.to_vec();
        with_total.push(opencode("session.usage.updated", 1.0, 200, 9, 3));
        let total = TurnUsage::from_opencode_events(&with_total).unwrap();
        assert_eq!(total.cost_usd, Some(1.0));
        assert_eq!(total.input_tokens, Some(200));
        assert_eq!(total.output_tokens, Some(12));
        assert_eq!(TurnUsage::from_opencode_events(&[]), None);
        let unpriced = json!({"type": "session.step.ended", "data": {"tokens": {"input": 3}}});
        let usage = TurnUsage::from_opencode_events(&[unpriced]).unwrap();
        assert_eq!(
            serde_json::to_value(&usage).unwrap(),
            json!({"cost_usd": null, "input_tokens": 3})
        );
    }

    #[test]
    fn the_shape_is_closed() {
        let extra = json!({"cost_usd": null, "input_tokens": 1, "total_tokens": 2});
        assert!(serde_json::from_value::<TurnUsage>(extra).is_err());
    }
}
