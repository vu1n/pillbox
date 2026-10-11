//! The `pillbox.text/2` driver for the pi family: `pi` and `prime-agent`.
//!
//! Prime Agent is a hard fork of pi and keeps pi's one-shot `--mode json` stream, its
//! `--no-tools` switch and its `auth.json` shape, so one driver serves both. The turn is
//! one stdin prompt, every tool, extension, skill, prompt template and context file
//! disabled, and one API-key provider reached through the vault: the guest holds a stub
//! key, and the VMM swaps it for the real one only on that provider's host.
//!
//! The guest side (VM launch and the stdout relay) is behind [`Guest`] so the whole
//! invocation runs against a fake harness in tests; the libkrun implementation is
//! `sandbox::libkrun::text_harness`.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use super::digest;
use super::evidence::ExecutionEvidence;
use super::store::OwnedInvocation;
use super::usage::TurnUsage;
use crate::contract::{Custom, Payload};
use crate::pillbox::Pillbox;
use crate::startup::StartupTimer;

pub(crate) const ADAPTER_REVISION: &str = "pillbox/local-text-v2-pi";
const POLICY_REVISION: &str = "pillbox-local-text-v2-pi";
/// The first frame the guest bridge sends: the harness's own `--version` output.
pub(crate) const VERSION_FRAME: &str = "pillbox.harness_version";
/// The last frame the guest bridge sends: the harness process's exit code.
pub(crate) const EXIT_FRAME: &str = "pillbox.harness_exit";
const MAX_VERSION_BYTES: usize = 64;
const MAX_KEY_BYTES: usize = 16 * 1024;
const STAGES: [&str; 6] = [
    "credentials",
    "image_prepare",
    "guest_prepare",
    "vmm_spawn",
    "guest_rpc_ready",
    "turn",
];

const PI_CATALOG: &str = include_str!("text-models-pi-1.0.2.json");
const PRIME_AGENT_CATALOG: &str = include_str!("text-models-prime-agent-0.9.8.json");

/// A harness that speaks pi's JSON mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PiHarness {
    Pi,
    PrimeAgent,
}

impl PiHarness {
    /// The `agent.harness` selection this driver serves.
    pub(crate) fn from_selection(harness: &str) -> Option<Self> {
        match harness {
            "pi" => Some(Self::Pi),
            "prime-agent" => Some(Self::PrimeAgent),
            _ => None,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Pi => "pi",
            Self::PrimeAgent => "prime-agent",
        }
    }

    /// The Pillbox agent whose auth home holds this harness's `auth.json`.
    pub(crate) fn agent(self) -> &'static crate::agents::AgentSpec {
        match self {
            Self::Pi => &crate::agents::PI,
            Self::PrimeAgent => &crate::agents::PRIME_AGENT,
        }
    }

    /// Where the harness reads `auth.json`, relative to the guest home.
    pub(crate) fn config_dir(self) -> &'static str {
        match self {
            Self::Pi => ".pi/agent",
            Self::PrimeAgent => ".prime/agent",
        }
    }

    fn catalog_source(self) -> &'static str {
        match self {
            Self::Pi => PI_CATALOG,
            Self::PrimeAgent => PRIME_AGENT_CATALOG,
        }
    }

    fn catalog(self) -> &'static Catalog {
        static PI: OnceLock<Catalog> = OnceLock::new();
        static PRIME_AGENT: OnceLock<Catalog> = OnceLock::new();
        let cell = match self {
            Self::Pi => &PI,
            Self::PrimeAgent => &PRIME_AGENT,
        };
        cell.get_or_init(|| {
            serde_json::from_str(self.catalog_source()).expect("embedded text model catalog")
        })
    }

    /// Digest of the embedded catalog `resolve` admitted the model against.
    pub(crate) fn catalog_digest(self) -> String {
        digest(self.catalog_source().as_bytes())
    }

    /// The harness's process environment besides `HOME`, `PATH` and the CA trust. Every
    /// startup network operation (update checks, catalog refreshes, telemetry) is off, so
    /// the only egress is the model request.
    pub(crate) fn env(self) -> Vec<(String, String)> {
        let mut env = vec![
            ("PI_OFFLINE".to_owned(), "1".to_owned()),
            ("PI_SKIP_VERSION_CHECK".to_owned(), "1".to_owned()),
            ("PI_TELEMETRY".to_owned(), "0".to_owned()),
        ];
        if self == Self::PrimeAgent {
            env.push(("PRIME_AGENT_TELEMETRY".to_owned(), "0".to_owned()));
        }
        env
    }

    pub(crate) fn version_argv(self) -> Vec<String> {
        vec![self.name().to_owned(), "--version".to_owned()]
    }

    /// One tool-free JSON-mode turn. The prompt arrives on stdin, so it is never parsed
    /// as a flag and is not bound by the kernel's per-argument length limit.
    pub(crate) fn argv(self, selection: &Selection) -> Vec<String> {
        // Context: doc://pillbox/agent-io-pty-free-contract@0002#agent-io-pty-free-contract — structured stdout events, no PTY.
        let mut argv: Vec<String> = [
            self.name(),
            "-p",
            "--mode",
            "json",
            "--no-session",
            "--no-tools",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-context-files",
            "--no-themes",
            "--offline",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        if self == Self::Pi {
            // Ignore project-local `.pi/` files; the workspace is empty, but the turn
            // must not depend on that.
            argv.push("--no-approve".to_owned());
        }
        argv.extend([
            "--model".to_owned(),
            format!("{}/{}", selection.provider, selection.model),
            "--thinking".to_owned(),
            selection.reasoning_effort.clone(),
        ]);
        argv
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    providers: std::collections::BTreeMap<String, CatalogProvider>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogProvider {
    host: String,
    models: Vec<String>,
}

/// A model selection the harness can serve, and the one host it needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Selection {
    pub(crate) harness: PiHarness,
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) reasoning_effort: String,
    pub(crate) host: String,
}

impl Selection {
    /// `model` is the harness's own `PROVIDER/MODEL` form. Only API-key providers with
    /// one known host are admitted, so the egress allowlist and the credential swap
    /// are both exactly that host.
    pub(crate) fn resolve(harness: PiHarness, model: &str, reasoning_effort: &str) -> Result<Self> {
        let (provider, id) = model
            .split_once('/')
            .context("pi-family models are PROVIDER/MODEL")?;
        let entry = harness
            .catalog()
            .providers
            .get(provider)
            .with_context(|| format!("{} has no text provider {provider}", harness.name()))?;
        ensure!(
            entry.models.iter().any(|known| known == id),
            "{} cannot serve {model}",
            harness.name()
        );
        ensure!(
            matches!(reasoning_effort, "low" | "medium" | "high"),
            "unsupported reasoning effort"
        );
        Ok(Self {
            harness,
            provider: provider.to_owned(),
            model: id.to_owned(),
            reasoning_effort: reasoning_effort.to_owned(),
            host: entry.host.clone(),
        })
    }

    pub(crate) fn requested_model(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }
}

/// The stored API key for `provider` in a pi-format `auth.json`. A missing entry, an
/// OAuth login, or a key pi would resolve by running a host command (`!cmd`) is not a
/// credential Pillbox can vault.
pub(crate) fn api_key(auth_json: &[u8], provider: &str) -> Result<String> {
    let auth: Value = serde_json::from_slice(auth_json).context("parse auth.json")?;
    let entry = auth
        .get(provider)
        .with_context(|| format!("no stored credential for {provider}"))?;
    ensure!(
        entry["type"] == "api_key",
        "the stored {provider} credential is not an API key"
    );
    let key = entry["key"]
        .as_str()
        .context("the stored API key is not a string")?;
    ensure!(
        !key.is_empty()
            && key.len() <= MAX_KEY_BYTES
            && !key.starts_with('!')
            && key.bytes().all(|byte| byte.is_ascii_graphic()),
        "the stored {provider} API key is not a literal key"
    );
    Ok(key.to_owned())
}

/// A random stand-in for the real key. It is the only key the guest ever holds.
pub(crate) fn new_stub() -> String {
    let bytes: [u8; 24] = rand::random();
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("pillbox-stub-{hex}")
}

/// The guest's `auth.json`: the selected provider, holding only the stub.
pub(crate) fn guest_auth(provider: &str, stub: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({provider: {"type": "api_key", "key": stub}}))
        .expect("serialize guest auth")
}

/// What the guest bridge reported besides the turn itself.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Observed {
    pub(crate) harness_version: Option<String>,
    pub(crate) exit_code: Option<i64>,
    pub(crate) usage: Option<TurnUsage>,
}

/// Version, exit code and spend, read from whatever frames arrived, so a failed turn
/// still reports what it spent.
pub(crate) fn observe(frames: &[Value]) -> Observed {
    let version = frames
        .iter()
        .find(|frame| frame["type"] == VERSION_FRAME)
        .and_then(|frame| frame["version"].as_str())
        .map(str::trim)
        .filter(|version| {
            !version.is_empty()
                && version.len() <= MAX_VERSION_BYTES
                && version
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
        })
        .map(str::to_owned);
    let exit_code = frames
        .iter()
        .rev()
        .find(|frame| frame["type"] == EXIT_FRAME)
        .and_then(|frame| frame["code"].as_i64());
    Observed {
        harness_version: version,
        exit_code,
        usage: TurnUsage::from_pi_frames(frames),
    }
}

pub(crate) fn assistant_ends(frames: &[Value]) -> impl Iterator<Item = &Value> {
    frames
        .iter()
        .filter(|frame| frame["type"] == "message_end")
        .map(|frame| &frame["message"])
        .filter(|message| message["role"] == "assistant")
}

/// The completed turn's answer.
#[derive(Debug, PartialEq)]
pub(crate) struct Answer {
    pub(crate) text: String,
    pub(crate) served_model: Option<String>,
}

/// The single final text of a tool-free turn, or why there is none. Any tool call the
/// model makes fails the turn, even though `--no-tools` means pi refuses to run it
/// ("Tool bash not found"): a deny_all answer must not have been shaped by a tool
/// round-trip.
pub(crate) fn answer(frames: &[Value], max_final_text_bytes: usize) -> Result<Answer> {
    for frame in frames {
        let kind = frame["type"].as_str().unwrap_or_default();
        ensure!(
            !kind.starts_with("tool_execution"),
            "the model attempted a tool under deny_all"
        );
    }
    let ends: Vec<&Value> = assistant_ends(frames).collect();
    ensure!(
        !ends.iter().any(|message| message["content"]
            .as_array()
            .is_some_and(|content| content.iter().any(|part| part["type"] == "toolCall"))),
        "the model attempted a tool under deny_all"
    );
    let finished = frames
        .iter()
        .rev()
        .find(|frame| frame["type"] == "agent_end")
        .context("the harness did not finish the turn")?;
    ensure!(
        finished["willRetry"] != true,
        "the harness ended while a retry was pending"
    );
    let observed = observe(frames);
    ensure!(
        observed.exit_code == Some(0),
        "the harness exited with {:?}",
        observed.exit_code
    );
    let last = ends.last().context("the turn has no assistant response")?;
    let stop = last["stopReason"].as_str().unwrap_or_default();
    ensure!(stop == "stop", "the turn stopped with {stop:?}");
    let text: String = last["content"]
        .as_array()
        .context("assistant content is not a list")?
        .iter()
        .filter(|part| part["type"] == "text")
        .filter_map(|part| part["text"].as_str())
        .collect();
    ensure!(!text.trim().is_empty(), "the turn produced no text");
    ensure!(
        text.len() <= max_final_text_bytes,
        "final text exceeds max_final_text_bytes"
    );
    // pi reports the configured `model`; `responseModel` is what the provider said
    // served the request, the only authoritative fact.
    let served_model = last["responseModel"]
        .as_str()
        .filter(|model| !model.is_empty() && model.len() <= 256)
        .map(str::to_owned);
    Ok(Answer { text, served_model })
}

/// The bounds a text/2 request sets on this turn.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub(crate) timeout_ms: u64,
    pub(crate) max_final_text_bytes: u64,
    pub(crate) max_frame_bytes: u64,
    pub(crate) max_evidence_bytes: u64,
}

/// Everything the guest needs to run one turn. No `Debug`: `real_key` is the credential.
pub(crate) struct GuestTurn {
    pub(crate) runner_image_id: String,
    pub(crate) argv: Vec<String>,
    pub(crate) version_argv: Vec<String>,
    pub(crate) env: Vec<(String, String)>,
    /// Files written under the guest home, by relative path.
    pub(crate) home_files: Vec<(String, Vec<u8>)>,
    pub(crate) stdin: Vec<u8>,
    pub(crate) host: String,
    pub(crate) stub_key: String,
    pub(crate) real_key: String,
}

/// What came back from the guest. `error` is a transport failure after frames
/// started arriving (deadline, limit, cancellation); the frames so far are kept.
pub(crate) struct GuestOutput {
    pub(crate) frames: Vec<Value>,
    pub(crate) diagnostics: Vec<u8>,
    pub(crate) error: Option<anyhow::Error>,
}

/// Boots the guest and relays the harness's stdout frames. `stage` is called with
/// `image_prepare`, `guest_prepare`, `vmm_spawn` and `guest_rpc_ready` as each
/// completes; `live` fails once the invocation is cancelled or past its deadline.
/// An `Err` means no output exists, or teardown was not confirmed.
pub(crate) trait Guest {
    fn run(
        &mut self,
        turn: GuestTurn,
        limits: Limits,
        deadline: Instant,
        live: &dyn Fn() -> Result<()>,
        stage: &mut dyn FnMut(&'static str),
    ) -> Result<GuestOutput>;
}

/// The stage that was running when the turn failed and what it had spent, carried in
/// the error chain so the caller reports both without parsing the message.
#[derive(Debug)]
pub(crate) struct Failed {
    pub(crate) stage: &'static str,
    pub(crate) usage: Option<TurnUsage>,
}

impl std::fmt::Display for Failed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "text invocation failed during {}", self.stage)
    }
}

/// A completed turn and what ran it.
#[derive(Debug)]
pub(crate) struct Completion {
    pub(crate) session_ref: super::EvidenceRef,
    pub(crate) output_text: String,
    pub(crate) harness_version: String,
    pub(crate) requested_model: String,
    pub(crate) served_model: Option<String>,
    pub(crate) usage: Option<TurnUsage>,
}

/// The invocation's identity, input and limits, already validated by text/2.
pub(crate) struct Invocation<'a> {
    pub(crate) session_id: &'a str,
    pub(crate) invocation_id: &'a str,
    pub(crate) rendered_input: &'a str,
    pub(crate) limits: Limits,
}

struct Stages {
    timer: StartupTimer,
    completed: usize,
    started_emitted: bool,
}

impl Stages {
    fn in_progress(&self) -> &'static str {
        STAGES.get(self.completed).copied().unwrap_or("finalize")
    }

    fn complete(&mut self, name: &'static str, evidence: &mut ExecutionEvidence) -> Result<()> {
        ensure!(
            STAGES.get(self.completed) == Some(&name),
            "text stage {name} completed out of order"
        );
        self.timer.mark(name);
        self.completed += 1;
        let duration_ms = self
            .timer
            .snapshot()
            .stages
            .last()
            .map(|stage| stage.duration_ms);
        evidence.append(Payload::Custom(Custom {
            name: "text.stage.completed".into(),
            payload: Some(json!({"stage": name, "duration_ms": duration_ms})),
        }))?;
        Ok(())
    }

    fn emit_started(&mut self, pb: &Pillbox, session_id: &str) {
        if std::mem::replace(&mut self.started_emitted, true) {
            return;
        }
        crate::events::emit_session_event(
            pb,
            crate::events::EventType::SessionStarted {
                parent_session_id: crate::events::parent_session_id_from_env(),
                startup: Some(self.timer.snapshot()),
            },
            session_id,
            None,
        );
    }
}

/// Run one tool-free turn of `selection` on `runner_image_id`. On failure the error
/// chain carries [`Failed`]; a guest error such as unconfirmed teardown stays in it.
pub(crate) fn execute(
    pb: &Pillbox,
    invocation: &Invocation<'_>,
    selection: &Selection,
    runner_image_id: &str,
    owner: &mut OwnedInvocation,
    guest: &mut dyn Guest,
) -> Result<Completion> {
    let deadline = Instant::now() + Duration::from_millis(invocation.limits.timeout_ms);
    check_live(owner, deadline)?;
    let harness = selection.harness;
    let admission = json!({
        "request_hash": owner.record().request_hash,
        "harness": harness.name(),
        "runner_image_id": runner_image_id,
        "effective_model_catalog_digest": harness.catalog_digest(),
        "adapter_revision": ADAPTER_REVISION,
        "policy_revision": POLICY_REVISION,
    });
    let mut evidence = ExecutionEvidence::start_text(
        pb,
        invocation.session_id,
        invocation.invocation_id,
        admission.clone(),
    )?;
    let mut progress = json!({
        "admission": admission,
        "session_ref": evidence.reference(),
        "native_evidence": null,
    });
    owner.running(progress.clone())?;
    let mut stages = Stages {
        timer: StartupTimer::start(),
        completed: 0,
        started_emitted: false,
    };
    let mut usage = None;
    let result = run(
        pb,
        invocation,
        selection,
        runner_image_id,
        owner,
        guest,
        deadline,
        &mut evidence,
        &mut progress,
        &mut stages,
        &mut usage,
    );
    stages.emit_started(pb, invocation.session_id);
    match result {
        Ok(completion) => {
            crate::events::emit_session_event(
                pb,
                crate::events::EventType::SessionCompleted {
                    exit_code: Some(0),
                    trace_path: None,
                    result_snapshot: None,
                },
                invocation.session_id,
                None,
            );
            Ok(completion)
        }
        Err(error) => {
            let stage = stages.in_progress();
            crate::events::emit_session_event(
                pb,
                crate::events::EventType::SessionFailed {
                    reason: format!("{stage}: {error:#}"),
                    exit_code: Some(1),
                    trace_path: None,
                    result_snapshot: None,
                },
                invocation.session_id,
                None,
            );
            let recorded = evidence
                .append(Payload::Custom(Custom {
                    name: "text.execution.failed".into(),
                    payload: Some(json!({"error": format!("{error:#}"), "stage": stage})),
                }))
                .and_then(|_| {
                    progress["session_ref"] = json!(evidence.reference());
                    owner.observe(progress)
                });
            let error = match recorded {
                Ok(()) => error,
                Err(persistence) => error.context(format!(
                    "failure evidence was not persisted: {persistence:#}"
                )),
            };
            Err(error.context(Failed { stage, usage }))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run(
    pb: &Pillbox,
    invocation: &Invocation<'_>,
    selection: &Selection,
    runner_image_id: &str,
    owner: &mut OwnedInvocation,
    guest: &mut dyn Guest,
    deadline: Instant,
    evidence: &mut ExecutionEvidence,
    progress: &mut Value,
    stages: &mut Stages,
    usage: &mut Option<TurnUsage>,
) -> Result<Completion> {
    let harness = selection.harness;
    let agent = harness.agent();
    let credentials = agent.home_dir(pb)?.join(agent.cred_sentinel);
    let auth = std::fs::read(&credentials)
        .with_context(|| format!("{} has no stored credentials", harness.name()))?;
    let real_key = api_key(&auth, &selection.provider)?;
    drop(auth);
    let stub_key = new_stub();
    let turn = GuestTurn {
        runner_image_id: runner_image_id.to_owned(),
        argv: harness.argv(selection),
        version_argv: harness.version_argv(),
        env: harness.env(),
        home_files: vec![(
            format!("{}/auth.json", harness.config_dir()),
            guest_auth(&selection.provider, &stub_key),
        )],
        stdin: invocation.rendered_input.as_bytes().to_vec(),
        host: selection.host.clone(),
        stub_key,
        real_key,
    };
    stages.complete("credentials", evidence)?;
    check_live(owner, deadline)?;
    let mut stage_error = None;
    let output = {
        let live = || check_live(owner, deadline);
        guest.run(turn, invocation.limits, deadline, &live, &mut |name| {
            if let Err(error) = stages.complete(name, evidence) {
                stage_error.get_or_insert(error);
            }
        })?
    };
    stages.emit_started(pb, invocation.session_id);
    if let Some(error) = stage_error {
        return Err(error.context("persist text stage evidence"));
    }
    let observed = observe(&output.frames);
    *usage = observed.usage.clone();
    let envelopes: Vec<Value> = output
        .frames
        .iter()
        .map(|frame| json!({"direction": "inbound", "message": frame}))
        .collect();
    let native_evidence = evidence.native_frames(&envelopes)?;
    progress["native_evidence"] = json!(native_evidence);
    let diagnostics = evidence.artifact(&output.diagnostics, "text/plain")?;
    evidence.append(Payload::Custom(Custom {
        name: "text.builder.stopped".into(),
        payload: Some(json!({"diagnostics": diagnostics})),
    }))?;
    if let Some(usage) = usage.as_ref() {
        evidence.append(Payload::Custom(Custom {
            name: "usage".into(),
            payload: Some(json!({"turn_usage": usage})),
        }))?;
    }
    progress["session_ref"] = json!(evidence.reference());
    owner.observe(progress.clone())?;
    if let Some(error) = output.error {
        return Err(error);
    }
    let answer = answer(
        &output.frames,
        invocation.limits.max_final_text_bytes as usize,
    )?;
    let harness_version = observed
        .harness_version
        .with_context(|| format!("{} did not report its version", harness.name()))?;
    stages.complete("turn", evidence)?;
    check_live(owner, deadline)?;
    let text = evidence.artifact(answer.text.as_bytes(), "text/plain;charset=utf-8")?;
    evidence.append(Payload::Custom(Custom {
        name: "text.result.captured".into(),
        payload: Some(json!({"text": text, "native_evidence": native_evidence})),
    }))?;
    progress["session_ref"] = json!(evidence.reference());
    owner.observe(progress.clone())?;
    Ok(Completion {
        session_ref: evidence.reference(),
        output_text: answer.text,
        harness_version,
        requested_model: selection.requested_model(),
        served_model: answer.served_model,
        usage: usage.clone(),
    })
}

fn check_live(owner: &OwnedInvocation, deadline: Instant) -> Result<()> {
    ensure!(!owner.cancelled()?, "text invocation cancelled");
    if Instant::now() >= deadline {
        bail!("text invocation deadline exceeded");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::store::{Claim, InvocationStore};
    use crate::pillbox::Scope;

    const PI_TEXT: &str = include_str!("fixtures/pi-1.0.2-text.jsonl");
    const PI_TOOL: &str = include_str!("fixtures/pi-1.0.2-tool-refused.jsonl");
    const PRIME_TEXT: &str = include_str!("fixtures/prime-agent-0.9.8-text.jsonl");
    const PRIME_TOOL: &str = include_str!("fixtures/prime-agent-0.9.8-tool-refused.jsonl");

    /// A capture as the bridge relays it: version first, harness stdout, exit code last.
    fn relayed(capture: &str, version: &str, exit: i64) -> Vec<Value> {
        let mut frames = vec![json!({"type": VERSION_FRAME, "version": version})];
        frames.extend(
            capture
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap()),
        );
        frames.push(json!({"type": EXIT_FRAME, "code": exit}));
        frames
    }

    #[test]
    fn selections_lower_onto_the_catalog_and_unservable_models_are_rejected() {
        let pi = Selection::resolve(PiHarness::Pi, "anthropic/claude-sonnet-5", "low").unwrap();
        assert_eq!(pi.host, "api.anthropic.com");
        assert_eq!(
            PiHarness::Pi.argv(&pi)[PiHarness::Pi.argv(&pi).len() - 4..],
            ["--model", "anthropic/claude-sonnet-5", "--thinking", "low"]
        );
        let prime = Selection::resolve(PiHarness::PrimeAgent, "zai/glm-5.3-flash", "high").unwrap();
        assert_eq!(prime.host, "api.z.ai");
        for (model, effort) in [
            ("claude-sonnet-5", "low"),
            ("anthropic/claude-sonnet-404", "low"),
            ("amazon-bedrock/amazon.nova-2-lite-v1:0", "low"),
            ("anthropic/claude-sonnet-5", "max"),
        ] {
            assert!(Selection::resolve(PiHarness::Pi, model, effort).is_err());
        }
        // Each harness admits only its own bundled catalog.
        assert!(Selection::resolve(PiHarness::Pi, "anthropic/claude-haiku-5-5", "low").is_ok());
        assert!(
            Selection::resolve(PiHarness::PrimeAgent, "anthropic/claude-haiku-5-5", "low").is_err()
        );
    }

    #[test]
    fn the_argv_disables_every_tool_and_ambient_resource() {
        let selection = Selection::resolve(PiHarness::Pi, "openai/gpt-5.2", "medium").unwrap();
        for harness in [PiHarness::Pi, PiHarness::PrimeAgent] {
            let argv = harness.argv(&selection);
            for flag in [
                "--no-tools",
                "--no-extensions",
                "--no-skills",
                "--no-prompt-templates",
                "--no-context-files",
                "--no-session",
                "--offline",
            ] {
                assert!(
                    argv.iter().any(|arg| arg == flag),
                    "{harness:?} lacks {flag}"
                );
            }
            assert!(!argv.iter().any(|arg| arg == "-t" || arg == "--tools"));
        }
    }

    #[test]
    fn only_a_literal_api_key_is_a_vaultable_credential() {
        let auth = br#"{"zai":{"type":"api_key","key":"zk-real"},
            "anthropic":{"type":"oauth","access":"a","refresh":"r","expires":1},
            "openai":{"type":"api_key","key":"!security find-generic-password -ws openai"}}"#;
        assert_eq!(api_key(auth, "zai").unwrap(), "zk-real");
        assert!(api_key(auth, "anthropic").is_err());
        assert!(api_key(auth, "openai").is_err());
        assert!(api_key(auth, "openrouter").is_err());
        let stub = new_stub();
        let guest: Value = serde_json::from_slice(&guest_auth("zai", &stub)).unwrap();
        assert_eq!(guest, json!({"zai": {"type": "api_key", "key": stub}}));
        assert!(!String::from_utf8(guest_auth("zai", &stub))
            .unwrap()
            .contains("zk-real"));
    }

    #[test]
    fn real_text_turns_yield_the_answer_served_model_and_summed_usage() {
        for (capture, version) in [(PI_TEXT, "1.0.2"), (PRIME_TEXT, "0.9.8")] {
            let frames = relayed(capture, version, 0);
            let answer = answer(&frames, 32_768).unwrap();
            assert_eq!(answer.text, "Hello from mock.");
            assert_eq!(answer.served_model.as_deref(), Some("mock-served-1"));
            let observed = observe(&frames);
            assert_eq!(observed.harness_version.as_deref(), Some(version));
            let usage = serde_json::to_value(observed.usage.unwrap()).unwrap();
            assert_eq!(
                usage,
                json!({"cost_usd": 0.000411, "input_tokens": 100, "output_tokens": 7,
                    "cache_read_tokens": 20, "cache_write_tokens": 0})
            );
        }
    }

    #[test]
    fn a_refused_tool_attempt_fails_the_turn_and_still_reports_its_spend() {
        for capture in [PI_TOOL, PRIME_TOOL] {
            let frames = relayed(capture, "1.0.2", 0);
            // pi refused the call itself: the tool never ran.
            assert!(frames
                .iter()
                .any(|frame| frame["type"] == "tool_execution_end"
                    && frame["isError"] == true
                    && frame["result"]["content"][0]["text"] == "Tool bash not found"));
            let error = answer(&frames, 32_768).unwrap_err();
            assert!(error.to_string().contains("attempted a tool"));
            // Two provider calls, both counted.
            let usage = observe(&frames).usage.unwrap();
            assert_eq!(usage.input_tokens, Some(200));
            assert_eq!(usage.output_tokens, Some(14));
        }
    }

    #[test]
    fn empty_oversized_errored_and_unfinished_turns_fail() {
        let frames = relayed(PI_TEXT, "1.0.2", 0);
        assert!(answer(&frames, "Hello from mock.".len() - 1).is_err());
        assert!(answer(&frames, "Hello from mock.".len()).is_ok());
        assert!(answer(&relayed(PI_TEXT, "1.0.2", 1), 32_768).is_err());
        let unfinished: Vec<Value> = frames
            .iter()
            .filter(|frame| frame["type"] != "agent_end")
            .cloned()
            .collect();
        assert!(answer(&unfinished, 32_768).is_err());
        let rewrite = |stop: &str, text: &str| -> Vec<Value> {
            frames
                .iter()
                .cloned()
                .map(|mut frame| {
                    if frame["type"] == "message_end" && frame["message"]["role"] == "assistant" {
                        frame["message"]["stopReason"] = json!(stop);
                        frame["message"]["content"] = json!([{"type": "text", "text": text}]);
                    }
                    frame
                })
                .collect()
        };
        assert!(answer(&rewrite("stop", "  \n"), 32_768).is_err());
        assert!(answer(&rewrite("length", "cut off"), 32_768).is_err());
        assert!(answer(&rewrite("error", ""), 32_768).is_err());
        assert!(answer(&rewrite("stop", "fine"), 32_768).is_ok());
        assert_eq!(observe(&[]), Observed::default());
    }

    /// A harness that never boots a VM: it checks what the driver handed the guest and
    /// replays a capture.
    struct FakeGuest {
        frames: Vec<Value>,
        seen: Option<GuestTurn>,
    }

    impl Guest for FakeGuest {
        fn run(
            &mut self,
            turn: GuestTurn,
            _limits: Limits,
            _deadline: Instant,
            live: &dyn Fn() -> Result<()>,
            stage: &mut dyn FnMut(&'static str),
        ) -> Result<GuestOutput> {
            for name in [
                "image_prepare",
                "guest_prepare",
                "vmm_spawn",
                "guest_rpc_ready",
            ] {
                live()?;
                stage(name);
            }
            self.seen = Some(turn);
            Ok(GuestOutput {
                frames: self.frames.clone(),
                diagnostics: b"guest console".to_vec(),
                error: None,
            })
        }
    }

    struct Fixture {
        _temp: tempfile::TempDir,
        pb: Pillbox,
        store: InvocationStore,
    }

    fn fixture(auth: Option<&str>) -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let pb = Pillbox {
            scope: Scope::Global,
            state_dir: temp.path().into(),
            meta: None,
        };
        if let Some(auth) = auth {
            let path = crate::agents::PI
                .home_dir(&pb)
                .unwrap()
                .join(crate::agents::PI.cred_sentinel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, auth).unwrap();
        }
        let store = InvocationStore::new(&temp.path().join("text-executions")).unwrap();
        Fixture {
            _temp: temp,
            pb,
            store,
        }
    }

    fn invoke(fixture: &Fixture, guest: &mut FakeGuest, max_text: u64) -> Result<Completion> {
        let Claim::Owned(mut owner) = fixture.store.claim("chat_1", "sealed").unwrap() else {
            panic!("fresh invocation")
        };
        let selection = Selection::resolve(PiHarness::Pi, "zai/glm-5.3-flash", "low").unwrap();
        execute(
            &fixture.pb,
            &Invocation {
                session_id: "chat_1",
                invocation_id: "chat_1",
                rendered_input: "-- say hello",
                limits: Limits {
                    timeout_ms: 60_000,
                    max_final_text_bytes: max_text,
                    max_frame_bytes: 1_048_576,
                    max_evidence_bytes: 8_388_608,
                },
            },
            &selection,
            &format!("sha256:{}", "c".repeat(64)),
            &mut owner,
            guest,
        )
    }

    fn failed(error: &anyhow::Error) -> &Failed {
        error
            .downcast_ref::<Failed>()
            .expect("failure carries its stage")
    }

    const AUTH: &str = r#"{"zai":{"type":"api_key","key":"zk-real-key"}}"#;

    #[test]
    fn fake_harness_completed_turn_reports_what_ran_and_its_usage() {
        crate::test_util::with_isolated_home("pi-text", || {
            let fixture = fixture(Some(AUTH));
            let mut guest = FakeGuest {
                frames: relayed(PI_TEXT, "1.0.2", 0),
                seen: None,
            };
            let completion = invoke(&fixture, &mut guest, 32_768).unwrap();
            assert_eq!(completion.output_text, "Hello from mock.");
            assert_eq!(completion.harness_version, "1.0.2");
            assert_eq!(completion.requested_model, "zai/glm-5.3-flash");
            assert_eq!(completion.served_model.as_deref(), Some("mock-served-1"));
            assert_eq!(completion.usage.unwrap().cost_usd, Some(0.000411));
            let GuestTurn {
                argv,
                host,
                real_key: real,
                stdin,
                home_files: files,
                ..
            } = guest.seen.unwrap();
            assert!(argv.iter().any(|arg| arg == "--no-tools"));
            // The prompt goes over stdin, never argv, and the real key only to the VMM.
            assert!(!argv.iter().any(|arg| arg.contains("say hello")));
            assert_eq!(stdin, b"-- say hello");
            assert_eq!((host.as_str(), real.as_str()), ("api.z.ai", "zk-real-key"));
            assert_eq!(files.len(), 1);
            assert!(!String::from_utf8_lossy(&files[0].1).contains("zk-real-key"));
        });
    }

    #[test]
    fn fake_harness_tool_attempt_is_refused_at_turn_with_usage() {
        crate::test_util::with_isolated_home("pi-text", || {
            let fixture = fixture(Some(AUTH));
            let mut guest = FakeGuest {
                frames: relayed(PI_TOOL, "1.0.2", 0),
                seen: None,
            };
            let error = invoke(&fixture, &mut guest, 32_768).unwrap_err();
            let failed = failed(&error);
            assert_eq!(failed.stage, "turn");
            assert_eq!(failed.usage.as_ref().unwrap().input_tokens, Some(200));
            assert!(!format!("{failed}").contains("bash"));
        });
    }

    #[test]
    fn fake_harness_response_over_the_limit_fails_at_turn() {
        crate::test_util::with_isolated_home("pi-text", || {
            let fixture = fixture(Some(AUTH));
            let mut guest = FakeGuest {
                frames: relayed(PI_TEXT, "1.0.2", 0),
                seen: None,
            };
            let error = invoke(&fixture, &mut guest, 4).unwrap_err();
            assert_eq!(failed(&error).stage, "turn");
        });
    }

    #[test]
    fn fake_harness_missing_credential_fails_at_credentials_before_the_guest() {
        crate::test_util::with_isolated_home("pi-text", || {
            for auth in [None, Some(r#"{"anthropic":{"type":"api_key","key":"k"}}"#)] {
                let fixture = fixture(auth);
                let mut guest = FakeGuest {
                    frames: relayed(PI_TEXT, "1.0.2", 0),
                    seen: None,
                };
                let error = invoke(&fixture, &mut guest, 32_768).unwrap_err();
                let failed = failed(&error);
                assert_eq!(failed.stage, "credentials");
                assert!(failed.usage.is_none());
                assert!(guest.seen.is_none(), "the guest never ran");
            }
        });
    }

    /// Real turns on `zai/glm-5.3-flash`, relayed by [`LocalGuest`] (version and exit
    /// frames included). z.ai returns no response model, and Prime Agent 0.9.8 prices
    /// GLM at zero.
    const PI_LIVE: &str = include_str!("fixtures/pi-1.0.2-zai-live.jsonl");
    const PRIME_LIVE: &str = include_str!("fixtures/prime-agent-0.9.8-zai-live.jsonl");
    /// pi with no route to the provider: three retries, then `stopReason: "error"`, exit 0.
    const PI_NO_EGRESS: &str = include_str!("fixtures/pi-1.0.2-zai-no-egress.jsonl");

    fn frames(capture: &str) -> Vec<Value> {
        capture
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn live_glm_captures_yield_the_answer_and_usage() {
        for (capture, version, input, cost) in [
            (PI_LIVE, "1.0.2", 44, Some(0.00001962)),
            (PRIME_LIVE, "0.9.8", 895, Some(0.0)),
        ] {
            let frames = frames(capture);
            let answer = answer(&frames, 32_768).unwrap();
            assert_eq!(answer.text, "pong");
            assert_eq!(answer.served_model, None);
            let observed = observe(&frames);
            assert_eq!(observed.harness_version.as_deref(), Some(version));
            assert_eq!(observed.exit_code, Some(0));
            let usage = observed.usage.unwrap();
            assert_eq!(usage.input_tokens, Some(input));
            assert_eq!(usage.cost_usd, cost);
        }
        let error = answer(&frames(PI_NO_EGRESS), 32_768).unwrap_err();
        assert!(format!("{error:#}").contains("error"));
    }

    /// Runs the harness as a local process: the cloud stand-in for the guest when no
    /// HVF/KVM host can boot one. The VMM's credential swap is done here instead: the
    /// stub in `home_files` is replaced by the real key before the harness reads it.
    /// It relays frames the way the guest bridge does (version, stdout, exit).
    const PASSTHROUGH: [&str; 6] = [
        "HTTPS_PROXY",
        "https_proxy",
        "NO_PROXY",
        "no_proxy",
        "SSL_CERT_FILE",
        "NODE_EXTRA_CA_CERTS",
    ];

    struct LocalGuest {
        home: tempfile::TempDir,
        workspace: tempfile::TempDir,
        /// Prime Agent keys its daemon socket by `TMPDIR`, which the VM makes private.
        tmp: tempfile::TempDir,
        frames: Vec<Value>,
    }

    impl Guest for LocalGuest {
        fn run(
            &mut self,
            turn: GuestTurn,
            _limits: Limits,
            _deadline: Instant,
            live: &dyn Fn() -> Result<()>,
            stage: &mut dyn FnMut(&'static str),
        ) -> Result<GuestOutput> {
            use std::io::Write;
            use std::process::{Command, Stdio};
            live()?;
            stage("image_prepare");
            for (path, bytes) in &turn.home_files {
                let target = self.home.path().join(path);
                std::fs::create_dir_all(target.parent().unwrap())?;
                let swapped =
                    String::from_utf8(bytes.clone())?.replace(&turn.stub_key, &turn.real_key);
                std::fs::write(target, swapped)?;
            }
            stage("guest_prepare");
            let command = |argv: &[String]| {
                let mut command = Command::new(&argv[0]);
                command
                    .args(&argv[1..])
                    .current_dir(self.workspace.path())
                    .env_clear()
                    .envs(turn.env.iter().cloned())
                    .env("HOME", self.home.path())
                    .env("PATH", "/usr/local/bin:/usr/bin:/bin")
                    .env("LANG", "C.UTF-8")
                    .env("TMPDIR", self.tmp.path());
                // The host's egress path stands in for the VMM's: its proxy and CA trust.
                for name in PASSTHROUGH {
                    if let Some(value) = std::env::var_os(name) {
                        command.env(name, value);
                    }
                }
                command
            };
            let version = command(&turn.version_argv)
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()?;
            stage("vmm_spawn");
            let mut child = command(&turn.argv)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            stage("guest_rpc_ready");
            child.stdin.take().unwrap().write_all(&turn.stdin)?;
            let output = child.wait_with_output()?;
            // VM teardown would take Prime Agent's detached daemon with it.
            let _ = Command::new("pkill")
                .arg("-f")
                .arg(self.tmp.path())
                .status();
            let mut frames = vec![json!({
                "type": VERSION_FRAME,
                "version": String::from_utf8_lossy(&version.stdout).trim(),
            })];
            for line in String::from_utf8(output.stdout)?.lines() {
                if !line.trim().is_empty() {
                    frames.push(serde_json::from_str(line)?);
                }
            }
            frames.push(json!({"type": EXIT_FRAME, "code": output.status.code()}));
            self.frames.clone_from(&frames);
            Ok(GuestOutput {
                frames,
                diagnostics: output.stderr,
                error: None,
            })
        }
    }

    /// One real turn against the real provider, outside a VM. Run with
    /// `PILLBOX_LIVE_TEXT=1 cargo test pi_text::tests::live_ -- --ignored` with the
    /// provider's `<PROVIDER>_API_KEY` set and the harness on PATH;
    /// `PILLBOX_LIVE_CAPTURE_DIR` keeps the relayed frames.
    fn live_turn(harness: PiHarness) {
        if std::env::var_os("PILLBOX_LIVE_TEXT").is_none() {
            return;
        }
        let model =
            std::env::var("PILLBOX_LIVE_MODEL").unwrap_or_else(|_| "zai/glm-5.3-flash".to_owned());
        let selection = Selection::resolve(harness, &model, "low").unwrap();
        let key_var = format!("{}_API_KEY", selection.provider.to_uppercase());
        let key = std::env::var(&key_var).unwrap_or_else(|_| panic!("{key_var} is not set"));
        let temp = tempfile::tempdir().unwrap();
        let pb = Pillbox {
            scope: Scope::Global,
            state_dir: temp.path().into(),
            meta: None,
        };
        let agent = harness.agent();
        let path = agent.home_dir(&pb).unwrap().join(agent.cred_sentinel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let auth = json!({selection.provider.as_str(): {"type": "api_key", "key": key}});
        std::fs::write(path, auth.to_string()).unwrap();
        let store = InvocationStore::new(&temp.path().join("text-executions")).unwrap();
        let Claim::Owned(mut owner) = store.claim("chat_live", "sealed").unwrap() else {
            panic!("fresh invocation")
        };
        let mut guest = LocalGuest {
            home: tempfile::tempdir().unwrap(),
            workspace: tempfile::tempdir().unwrap(),
            tmp: tempfile::tempdir().unwrap(),
            frames: Vec::new(),
        };
        let completion = execute(
            &pb,
            &Invocation {
                session_id: "chat_live",
                invocation_id: "chat_live",
                rendered_input: "Reply with exactly one word: pong",
                limits: Limits {
                    timeout_ms: 180_000,
                    max_final_text_bytes: 32_768,
                    max_frame_bytes: 1_048_576,
                    max_evidence_bytes: 8_388_608,
                },
            },
            &selection,
            &format!("sha256:{}", "c".repeat(64)),
            &mut owner,
            &mut guest,
        );
        let capture: String = guest
            .frames
            .iter()
            .map(|frame| format!("{frame}\n"))
            .collect();
        assert!(!capture.contains(&key), "the real key reached the stream");
        if let Some(dir) = std::env::var_os("PILLBOX_LIVE_CAPTURE_DIR") {
            let file = format!("{}-live.jsonl", harness.name());
            std::fs::write(std::path::Path::new(&dir).join(file), capture).unwrap();
        }
        let completion = completion.unwrap();
        eprintln!(
            "{}",
            json!({
                "harness": harness.name(),
                "output_text": completion.output_text,
                "harness_version": completion.harness_version,
                "requested_model": completion.requested_model,
                "served_model": completion.served_model,
                "usage": completion.usage,
            })
        );
        assert!(!completion.output_text.trim().is_empty());
        assert!(completion.usage.is_some());
    }

    #[test]
    #[ignore = "live: needs PILLBOX_LIVE_TEXT, a provider key and pi on PATH"]
    fn live_pi_turn() {
        crate::test_util::with_isolated_home("pi-text-live", || live_turn(PiHarness::Pi));
    }

    #[test]
    #[ignore = "live: needs PILLBOX_LIVE_TEXT, a provider key and prime-agent on PATH"]
    fn live_prime_agent_turn() {
        crate::test_util::with_isolated_home("pi-text-live", || live_turn(PiHarness::PrimeAgent));
    }
}
