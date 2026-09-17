//! The **critic** seam for `pillbox dispatch` — a cheap, calibrated System One
//! judgment on a worker's edits *before* the verifier runs.
//!
//! Split out of `dispatch.rs` the way `grader.rs` was split out of `session`:
//! this module owns the pure policy (what a critic is, how a worker's edits are
//! rendered for it, what it returns) and the live HTTP client; the dispatch loop
//! owns *when* to ask and *what to do with the answer* (`--critic-policy`).
//!
//! Why a critic at all: the verifier (`session score`) is the reward — exact,
//! exit-derived, unforgeable — but it is also the expensive, slow, last step.
//! With `-k` workers, most verifier runs are spent on losers. A critic that
//! returns a **probability** (not a verdict) lets the loop grade in the order
//! most likely to pass and stop early (`order`), or pick without verifying at
//! all (`select`, measurement only). And because the verifier still produces
//! ground truth on every graded worker, **every dispatch is a labeled
//! `(p, passed)` pair** — the harness grades its own critic for free
//! (`dispatch.critic_verdict` artifacts, `signal` class, poolable).
//!
//! The only critic today is TypeSafe's `jev-latest` (`noul` = P(yes) natively,
//! no verbalized confidence). The trait is the seam a second one plugs into.
//!
//! **State format is a contract.** [`render_state`] is byte-identical to the
//! `buildState` in the companion offline harness (typesafe.vu1n.dev,
//! `deploy/deciders.js`), so calibration measured offline on the held-out set
//! is the calibration the live loop gets.

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::errors::PillboxError;

/// Env var the live critic reads its key from (no secret ever lands in argv).
pub(crate) const TYPESAFE_API_KEY_ENV: &str = "TYPESAFE_API_KEY";
pub(crate) const TYPESAFE_ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";
pub(crate) const DEFAULT_CRITIC_MODEL: &str = "jev-latest";
/// $ per input token, quoted from typesafe.ai on 2026-09-17 (output is free).
const TYPESAFE_USD_PER_INPUT_TOKEN: f64 = 0.042 / 1_000_000.0;
/// Same cap as the offline harness — the prompt (the spec) survives, the edit
/// tail is cut. Recorded on the verdict so a truncated judgment is never mistaken
/// for a full one.
pub(crate) const MAX_STATE_CHARS: usize = 14_000;

/// What `--critic-policy` does with the probabilities. Parsed from the CLI
/// string in `dispatch()` (a loud usage error, like `Grader::from_opts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CriticPolicy {
    /// Score every worker before its grade; grading is unchanged. The
    /// measurement mode: attaches `p` next to `passed` on every worker.
    Record,
    /// Grade workers in descending `p`, stop at the first passer; the rest are
    /// left `unverified`. Saves verifier runs when the critic ranks well.
    Order,
    /// Grade ONLY the argmax-`p` worker. If it fails, there is no winner — the
    /// honest cost of trusting the critic. The "critic instead of verifier" claim,
    /// ground-truthed on the one worker it picked.
    Select,
}

impl CriticPolicy {
    pub(crate) fn parse(s: &str) -> Result<Self> {
        match s {
            "record" => Ok(Self::Record),
            "order" => Ok(Self::Order),
            "select" => Ok(Self::Select),
            other => Err(PillboxError::usage(
                "dispatch",
                format!("unknown --critic-policy `{other}`"),
            )
            .with_next("use one of: record | order | select")
            .into()),
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Record => "record",
            Self::Order => "order",
            Self::Select => "select",
        }
    }
}

/// One critic's answer for one worker. Serialized additively into the verdict
/// JSON (`workers[].critic`) and as the body of a `dispatch.critic_verdict`
/// artifact — signal class, so it can pool: no code, no prompt, just numbers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CriticVerdict {
    /// Which critic (`typesafe`).
    pub(crate) critic: String,
    pub(crate) model: String,
    /// P(the verifier passes this worker) in `[0,1]`.
    pub(crate) p: f64,
    pub(crate) latency_ms: u64,
    /// What the call cost, from the vendor's own usage report; `None` if the
    /// vendor didn't report usage (never estimated).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cost_usd: Option<f64>,
    pub(crate) input_tokens: u64,
    /// The rendered state hit [`MAX_STATE_CHARS`] and was cut.
    pub(crate) truncated: bool,
    /// How many edits the critic saw (0 = the worker made none; still scorable —
    /// a no-op attempt is a confident `false`).
    pub(crate) n_edits: usize,
}

/// The seam. `state` is the rendered task + edits (see [`render_state`]).
pub(crate) trait Critic {
    fn name(&self) -> &str;
    fn model(&self) -> &str;
    fn score(&self, state: &str) -> Result<CriticVerdict>;
}

/// One edit a worker made, lifted from its §0 log's completed `tool_call`
/// events (`edit`/`write` and their Claude/Codex spellings). The same shape the
/// offline harness extracts, so live and offline critics see the same thing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct EditRecord {
    pub(crate) tool: String,
    pub(crate) file: String,
    #[serde(default)]
    pub(crate) old: String,
    #[serde(default)]
    pub(crate) new: String,
}

const EDIT_TOOLS: &[&str] = &[
    "edit",
    "write",
    "Edit",
    "Write",
    "MultiEdit",
    "apply_patch",
    "write_file",
    "create_file",
];

/// Parse a `session log` dump (one event JSON per line) into the worker's edit
/// list, in order. Only *completed* edit tool calls with an input count; a
/// `running` frame carries no input yet, and reads/greps/shell are not edits.
/// Shell writes (`cat > f`, `sed -i`) are NOT recovered — same blind spot as the
/// offline extractor, stated rather than papered over.
pub(crate) fn edits_from_log(log_jsonl: &str) -> Vec<EditRecord> {
    let mut out = Vec::new();
    for line in log_jsonl.lines() {
        let Ok(ev) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let p = &ev["payload"];
        if p["type"] != "tool_call" || p["status"] != "completed" {
            continue;
        }
        let name = p["name"].as_str().unwrap_or_default();
        if !EDIT_TOOLS.contains(&name) {
            continue;
        }
        let input = &p["input"];
        if !input.is_object() {
            continue;
        }
        let file = first_str(input, &["filePath", "path", "file_path"]);
        let old = first_str(input, &["oldString", "old_string"]);
        let new = first_str(input, &["newString", "new_string", "content"]);
        if file.is_empty() && old.is_empty() && new.is_empty() {
            continue;
        }
        out.push(EditRecord {
            tool: name.to_string(),
            file,
            old,
            new,
        });
    }
    out
}

fn first_str(v: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|k| v[k].as_str())
        .unwrap_or_default()
        .to_string()
}

/// Render the task prompt + edits as the critic's `state`. **Byte-identical to
/// `buildState` in `deploy/deciders.js`** — change both or neither. Returns the
/// state and whether it was truncated.
pub(crate) fn render_state(prompt: &str, edits: &[EditRecord]) -> (String, bool) {
    let mut parts = vec![
        format!("# Task\n{}", prompt.trim()),
        format!("# Agent's final edits ({})", edits.len()),
    ];
    for (i, e) in edits.iter().enumerate() {
        let old = if e.old.is_empty() {
            String::new()
        } else {
            format!("--- before\n{}\n", e.old)
        };
        parts.push(format!(
            "## edit {}: {} {}\n{old}+++ after\n{}",
            i + 1,
            e.tool,
            e.file,
            e.new
        ));
    }
    let mut state = parts.join("\n\n");
    let mut truncated = false;
    if state.chars().count() > MAX_STATE_CHARS {
        let cut: String = state.chars().take(MAX_STATE_CHARS).collect();
        state = format!("{cut}\n\n[… truncated at {MAX_STATE_CHARS} chars]");
        truncated = true;
    }
    (state, truncated)
}

// ── the live critic ──────────────────────────────────────────────────────────

/// TypeSafe `jev-latest` over one `noul` question. The question is phrased in
/// TypeSafe's own idiom (a boolean assessment + plain-language true/false
/// criteria) — the same wording the offline harness uses, so nothing is tuned
/// for the live path.
pub(crate) struct TypesafeCritic {
    api_key: String,
    model: String,
    client: reqwest::blocking::Client,
}

impl TypesafeCritic {
    /// Reads the key from the environment; a missing key is a usage error at
    /// `dispatch` parse time (before any VM boots), not a mid-run 401.
    pub(crate) fn from_env(model: &str) -> Result<Self> {
        let api_key = std::env::var(TYPESAFE_API_KEY_ENV)
            .ok()
            .filter(|k| !k.trim().is_empty());
        let Some(api_key) = api_key else {
            return Err(PillboxError::usage(
                "dispatch",
                format!("--critic typesafe needs {TYPESAFE_API_KEY_ENV}"),
            )
            .with_next("export TYPESAFE_API_KEY=… (console.typesafe.ai/settings/keys)")
            .into());
        };
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .context("build critic http client")?;
        Ok(Self {
            api_key,
            model: model.to_string(),
            client,
        })
    }

    fn request_body(&self, state: &str) -> Value {
        json!({
            "state": state,
            "model": self.model,
            "questions": {
                "will_pass": {
                    "type": "noul",
                    "instructions": "The edits are a coding agent's final attempt at the task. Will the task's hidden test suite pass on the resulting code?",
                    "criteria": {
                        "true": "The edits correctly and completely implement what the task asks; the tests would pass.",
                        "false": "The edits are incomplete, wrong, break existing behavior, or miss part of the task; at least one test would fail."
                    }
                }
            }
        })
    }
}

impl Critic for TypesafeCritic {
    fn name(&self) -> &str {
        "typesafe"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn score(&self, state: &str) -> Result<CriticVerdict> {
        let body = self.request_body(state);
        let n_edits = count_edits(state);
        let truncated = state.contains("[… truncated at");
        let started = Instant::now();
        // 429/529 are the documented back-off codes; anything else fails loudly.
        let mut attempt = 0u32;
        let resp: Value = loop {
            // reqwest is built without its `json` feature here; the body is
            // serialized by hand so no dependency changes ride along.
            let bytes = serde_json::to_vec(&body).context("serialize typesafe request")?;
            let res = self
                .client
                .post(TYPESAFE_ENDPOINT)
                .bearer_auth(&self.api_key)
                .header("content-type", "application/json")
                .body(bytes)
                .send()
                .context("typesafe request")?;
            let status = res.status().as_u16();
            if (status == 429 || status == 529) && attempt < 3 {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(600 * 2u64.pow(attempt)));
                continue;
            }
            let text = res.text().context("typesafe response body")?;
            if !(200..300).contains(&status) {
                bail!(
                    "typesafe HTTP {status}: {}",
                    text.chars().take(300).collect::<String>()
                );
            }
            break serde_json::from_str(&text).context("parse typesafe response")?;
        };
        let latency_ms = started.elapsed().as_millis() as u64;
        let Some(p) = resp["answers"]["will_pass"]["noul"].as_f64() else {
            bail!(
                "typesafe: no `noul` in response: {}",
                resp.to_string().chars().take(200).collect::<String>()
            );
        };
        let input_tokens = resp["usage"]["input_tokens"].as_u64().unwrap_or(0);
        let cost_usd = resp["usage"]["input_tokens"]
            .as_u64()
            .map(|t| t as f64 * TYPESAFE_USD_PER_INPUT_TOKEN);
        Ok(CriticVerdict {
            critic: self.name().to_string(),
            model: resp["model"].as_str().unwrap_or(&self.model).to_string(),
            p: p.clamp(0.0, 1.0),
            latency_ms,
            cost_usd,
            input_tokens,
            truncated,
            n_edits,
        })
    }
}

/// The edit count the state header declares — read back so the verdict carries
/// it without threading the edit list through the trait.
fn count_edits(state: &str) -> usize {
    state
        .lines()
        .find_map(|l| l.strip_prefix("# Agent's final edits ("))
        .and_then(|rest| rest.trim_end_matches(')').parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(file: &str, old: &str, new: &str) -> EditRecord {
        EditRecord {
            tool: "edit".into(),
            file: file.into(),
            old: old.into(),
            new: new.into(),
        }
    }

    #[test]
    fn policy_parses_the_three_tokens_and_rejects_others() {
        assert_eq!(CriticPolicy::parse("record").unwrap(), CriticPolicy::Record);
        assert_eq!(CriticPolicy::parse("order").unwrap(), CriticPolicy::Order);
        assert_eq!(CriticPolicy::parse("select").unwrap(), CriticPolicy::Select);
        let err = CriticPolicy::parse("yolo").unwrap_err().to_string();
        assert!(err.contains("unknown --critic-policy"), "{err}");
    }

    /// Golden: this string is the contract with `deploy/deciders.js buildState`.
    #[test]
    fn render_state_matches_the_offline_harness_format() {
        let (s, truncated) = render_state(
            "# Instructions\nDo the thing.\n",
            &[edit("/ws/a.py", "x = 1", "x = 2")],
        );
        assert_eq!(
            s,
            "# Task\n# Instructions\nDo the thing.\n\n# Agent's final edits (1)\n\n## edit 1: edit /ws/a.py\n--- before\nx = 1\n+++ after\nx = 2"
        );
        assert!(!truncated);
    }

    #[test]
    fn render_state_omits_before_block_for_a_fresh_write_and_truncates_from_the_tail() {
        let (s, _) = render_state("t", &[edit("/ws/b.py", "", "print(1)")]);
        assert!(s.ends_with("## edit 1: edit /ws/b.py\n+++ after\nprint(1)"));
        assert!(!s.contains("--- before"));
        let big = edit("/ws/c.py", "", &"y".repeat(20_000));
        let (s, truncated) = render_state("spec", &[big]);
        assert!(truncated);
        assert!(s.starts_with("# Task\nspec"));
        assert!(s.ends_with(&format!("[… truncated at {MAX_STATE_CHARS} chars]")));
        assert_eq!(count_edits(&s), 1);
    }

    #[test]
    fn edits_from_log_keeps_completed_edit_calls_only_in_order() {
        let log = [
            r#"{"seq":1,"payload":{"type":"tool_call","name":"read","status":"completed","input":{"filePath":"/ws/a.py"}}}"#,
            r#"{"seq":2,"payload":{"type":"tool_call","name":"edit","status":"running","input":{}}}"#,
            r#"{"seq":3,"payload":{"type":"tool_call","name":"edit","status":"completed","input":{"filePath":"/ws/a.py","oldString":"1","newString":"2"}}}"#,
            r#"{"seq":4,"payload":{"type":"tool_call","name":"Write","status":"completed","input":{"file_path":"/ws/b.py","content":"hi"}}}"#,
            r#"{"seq":5,"payload":{"type":"scored","passed":true}}"#,
            "not json",
        ]
        .join("\n");
        let edits = edits_from_log(&log);
        assert_eq!(
            edits,
            vec![
                EditRecord {
                    tool: "edit".into(),
                    file: "/ws/a.py".into(),
                    old: "1".into(),
                    new: "2".into()
                },
                EditRecord {
                    tool: "Write".into(),
                    file: "/ws/b.py".into(),
                    old: String::new(),
                    new: "hi".into()
                },
            ]
        );
    }

    #[test]
    fn typesafe_request_is_one_noul_question_in_their_idiom() {
        let c = TypesafeCritic {
            api_key: "k".into(),
            model: DEFAULT_CRITIC_MODEL.into(),
            client: reqwest::blocking::Client::new(),
        };
        let body = c.request_body("STATE");
        assert_eq!(body["model"], "jev-latest");
        assert_eq!(body["state"], "STATE");
        assert_eq!(body["questions"]["will_pass"]["type"], "noul");
        assert!(body["questions"]["will_pass"]["criteria"]["true"].is_string());
        assert!(body["questions"]["will_pass"]["criteria"]["false"].is_string());
    }

    #[test]
    fn from_env_without_a_key_is_a_usage_error_naming_the_var() {
        std::env::remove_var(TYPESAFE_API_KEY_ENV);
        let err = match TypesafeCritic::from_env(DEFAULT_CRITIC_MODEL) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a missing key must be a usage error"),
        };
        assert!(err.contains(TYPESAFE_API_KEY_ENV), "{err}");
    }
}
