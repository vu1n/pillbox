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

impl TurnUsage {
    /// pi's and Prime Agent's `--mode json` turn: every assistant `message_end`'s
    /// `usage`, summed, because a retried or tool-looped turn makes several provider
    /// calls and each one costs. `input` already excludes `cacheRead` and `cacheWrite`
    /// (pi subtracts cached prompt tokens), and `cost.total` is pi's own figure from
    /// its model catalog. A field missing from any response is not reported.
    pub(crate) fn from_pi_frames(frames: &[Value]) -> Option<Self> {
        let usages: Vec<&Value> = frames
            .iter()
            .filter(|frame| frame["type"] == "message_end")
            .map(|frame| &frame["message"])
            .filter(|message| message["role"] == "assistant" && message["usage"].is_object())
            .map(|message| &message["usage"])
            .collect();
        if usages.is_empty() {
            return None;
        }
        let sum = |key: &str| -> Option<u64> {
            usages
                .iter()
                .try_fold(0u64, |total, usage| total.checked_add(usage[key].as_u64()?))
        };
        let cost = usages.iter().try_fold(0f64, |total, usage| {
            Some(total + usage["cost"]["total"].as_f64()?)
        });
        Self::new(
            cost,
            sum("input"),
            sum("output"),
            sum("cacheRead"),
            sum("cacheWrite"),
        )
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
    fn pi_turn_sums_every_assistant_response() {
        let end = |input: u64, cost: Value| {
            json!({"type": "message_end", "message": {"role": "assistant", "usage": {
                "input": input, "output": 7, "cacheRead": 20, "cacheWrite": 0,
                "totalTokens": input + 27, "cost": {"total": cost}}}})
        };
        let user = json!({"type": "message_end", "message": {"role": "user", "content": "hi"}});
        let usage =
            TurnUsage::from_pi_frames(&[user.clone(), end(100, json!(0.25)), end(50, json!(0.5))])
                .unwrap();
        assert_eq!(
            serde_json::to_value(&usage).unwrap(),
            json!({"cost_usd": 0.75, "input_tokens": 150, "output_tokens": 14,
                "cache_read_tokens": 40, "cache_write_tokens": 0})
        );
        // One response without a cost makes the turn's cost unknown, not partial.
        let usage =
            TurnUsage::from_pi_frames(&[end(100, json!(0.25)), end(50, json!(null))]).unwrap();
        assert_eq!(usage.cost_usd, None);
        assert_eq!(TurnUsage::from_pi_frames(&[user]), None);
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
}
