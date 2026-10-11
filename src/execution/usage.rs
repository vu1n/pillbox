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
    /// OpenCode 2 emits per-step spend and separate running totals. Count each
    /// driven step once; running totals and replayed terminal events add nothing.
    /// Its output and reasoning counts are disjoint (core/session/usage.ts).
    pub(crate) fn from_opencode_steps(frames: &[Value], session: &str) -> Option<Self> {
        let mut seen = std::collections::HashSet::new();
        let steps: Vec<_> = frames
            .iter()
            .filter_map(|event| {
                let data = &event["data"];
                if !matches!(
                    event["type"].as_str(),
                    Some("session.step.ended" | "session.step.failed")
                ) || data["sessionID"] != session
                {
                    return None;
                }
                let id = data["assistantMessageID"]
                    .as_str()
                    .filter(|id| !id.is_empty())?;
                seen.insert(id).then_some(data)
            })
            .collect();
        let sum = |values: Vec<Option<u64>>| {
            let valid: Vec<_> = values
                .into_iter()
                .flatten()
                .filter(|n| *n <= MAX_TOKENS)
                .collect();
            if valid.is_empty() {
                return None;
            }
            valid
                .into_iter()
                .try_fold(0u64, |total, n| total.checked_add(n))
                .filter(|total| *total <= MAX_TOKENS)
        };
        let costs: Vec<_> = steps
            .iter()
            .filter_map(|d| d["cost"].as_f64())
            .filter(|v| v.is_finite() && (0.0..=MAX_COST_USD).contains(v))
            .collect();
        let cost = (!costs.is_empty()).then(|| costs.iter().sum::<f64>());
        Self::new(
            cost,
            sum(steps
                .iter()
                .map(|d| d["tokens"]["input"].as_u64())
                .collect()),
            sum(steps
                .iter()
                .flat_map(|d| {
                    [
                        d["tokens"]["output"].as_u64(),
                        d["tokens"]["reasoning"].as_u64(),
                    ]
                })
                .collect()),
            sum(steps
                .iter()
                .map(|d| d["tokens"]["cache"]["read"].as_u64())
                .collect()),
            sum(steps
                .iter()
                .map(|d| d["tokens"]["cache"]["write"].as_u64())
                .collect()),
        )
    }

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

    #[test]
    fn the_shape_is_closed() {
        let extra = json!({"cost_usd": null, "input_tokens": 1, "total_tokens": 2});
        assert!(serde_json::from_value::<TurnUsage>(extra).is_err());
    }

    #[test]
    fn opencode_sums_steps_once_without_running_totals_or_token_overlap() {
        let first = json!({"type": "session.step.ended", "data": {"sessionID": "s", "assistantMessageID": "m1",
            "cost": 0.1, "tokens": {"input": 12, "output": 5, "reasoning": 7, "cache": {"read": 30, "write": 2}}}});
        let second = json!({"type": "session.step.failed", "data": {"sessionID": "s", "assistantMessageID": "m2",
            "cost": 0.2, "tokens": {"input": 9, "output": 3, "reasoning": 2, "cache": {"read": 20, "write": 4}}}});
        let mut child = second.clone();
        child["data"]["sessionID"] = json!("child");
        let frames = vec![
            first.clone(),
            first,
            second,
            child,
            json!({"type": "session.usage.updated", "data": {"sessionID": "s", "cost": 1000,
                "tokens": {"input": 1000, "output": 1000}}}),
        ];
        let usage = TurnUsage::from_opencode_steps(&frames, "s").unwrap();
        assert!((usage.cost_usd.unwrap() - 0.3).abs() < 1e-12);
        assert_eq!(usage.input_tokens, Some(21));
        assert_eq!(usage.output_tokens, Some(17));
        assert_eq!(usage.cache_read_tokens, Some(50));
        assert_eq!(usage.cache_write_tokens, Some(6));
        assert!(TurnUsage::from_opencode_steps(&[], "s").is_none());
    }

    #[test]
    fn opencode_missing_or_invalid_usage_is_omitted_and_oversized_sum_dropped() {
        let missing = json!({"type": "session.step.ended", "data": {"sessionID": "s", "assistantMessageID": "m"}});
        assert!(TurnUsage::from_opencode_steps(std::slice::from_ref(&missing), "s").is_none());
        let mut invalid = missing.clone();
        invalid["data"]["cost"] = json!(-1);
        invalid["data"]["tokens"] =
            json!({"input": -1, "output": "3", "reasoning": 20_000_000_000_u64});
        assert!(TurnUsage::from_opencode_steps(&[invalid], "s").is_none());
        let mut large = missing.clone();
        large["data"]["tokens"]["output"] = json!(MAX_TOKENS);
        let mut next = missing;
        next["data"]["assistantMessageID"] = json!("m2");
        next["data"]["tokens"]["output"] = json!(1);
        next["data"]["tokens"]["input"] = json!(2);
        let usage = TurnUsage::from_opencode_steps(&[large, next], "s").unwrap();
        assert_eq!(usage.output_tokens, None);
        assert_eq!(usage.input_tokens, Some(2));
        assert_eq!(usage.cost_usd, None);
    }
}
