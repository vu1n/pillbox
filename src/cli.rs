//! Subcommand argument definitions for the `pillbox` CLI.
//!
//! The top-level `Cli` struct and `Command` enum live in `main.rs`; the
//! per-area `*Action` subcommand enums live here so `main.rs` stays a
//! readable entrypoint instead of a 1k-line clap wall. Each enum is the
//! declarative shape of one command group; the matching behavior lives
//! in `commands/<area>.rs` and dispatches off these.

use std::path::PathBuf;

use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub(crate) enum SnapshotAction {
    /// List every snapshot in the pillbox's repository.
    List {
        /// Emit a JSON array of snapshot records on stdout. Stable
        /// schema — pin against `version: 1`.
        #[arg(long)]
        json: bool,
    },
    /// Show one snapshot's metadata. Accepts a unique prefix.
    Show {
        /// Snapshot handle (full hex ID or a unique prefix ≥ 4 chars).
        handle: String,
        /// Emit the snapshot record as JSON on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Remove one snapshot. Data packs survive until a future `prune`.
    Rm {
        /// Snapshot handle (full hex ID or a unique prefix ≥ 4 chars).
        handle: String,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum BookmarkAction {
    /// List every bookmark in the current project pillbox.
    List {
        /// Emit JSON. Stable schema — pin against `version: 1`.
        #[arg(long)]
        json: bool,
    },
    /// Show one bookmark.
    Show {
        /// Bookmark name.
        name: String,
        /// Emit JSON. Stable schema — pin against `version: 1`.
        #[arg(long)]
        json: bool,
    },
    /// Point a bookmark at a snapshot. Defaults to `latest`.
    Set {
        /// Bookmark name. Allows slash refs like `session/abc123`.
        name: String,
        /// Snapshot handle/prefix, or `latest`. Defaults to `latest`.
        snapshot: Option<String>,
    },
    /// Remove one bookmark. The underlying snapshot is untouched.
    Rm {
        /// Bookmark name.
        name: String,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum SandboxAction {
    /// Spawn a long-lived container with the workspace mounted, kept idle so
    /// commands can be `exec`'d into it. Prints the sandbox id on stdout.
    Spawn {
        /// Runner image (default: the pillbox runner image).
        #[arg(long, value_name = "IMAGE")]
        image: Option<String>,
        /// Provision for an agent harness (`claude` | `codex` | `opencode`):
        /// mounts its auth and runs the container non-root so the agent
        /// channel can drive it headlessly. Omit for a bare exec-only sandbox.
        #[arg(long, value_name = "AGENT")]
        agent: Option<String>,
        /// Host directory to mount as the workspace (default: cwd).
        #[arg(long, value_name = "PATH")]
        workspace: Option<PathBuf>,
        /// Human label, surfaced in `sandbox list`.
        #[arg(long, value_name = "TEXT")]
        label: Option<String>,
    },
    /// Run an agent turn in a sandbox (the agent channel). Streams the agent's
    /// activity as contract events; `--json` emits them as JSONL, otherwise a
    /// human-readable trace. The sandbox must have been spawned `--agent`.
    Agent {
        /// Sandbox id (or unique prefix ≥ 4 chars).
        id: String,
        /// Emit contract events as JSONL instead of a human trace.
        #[arg(long)]
        json: bool,
        /// The prompt, after `--`.
        #[arg(trailing_var_arg = true, value_name = "PROMPT")]
        prompt: Vec<String>,
    },
    /// Run a command in a sandbox (PTY-free). Default streams raw output and
    /// mirrors the exit code; `--json` emits structured exec events as JSONL.
    Exec {
        /// Sandbox id (or unique prefix ≥ 4 chars).
        id: String,
        /// Emit `ExecStarted`/`ExecOutput`/`ExecExit` as JSONL instead of
        /// passing raw bytes through to the terminal.
        #[arg(long)]
        json: bool,
        /// Command and arguments, after `--`.
        #[arg(trailing_var_arg = true, value_name = "ARGV")]
        argv: Vec<String>,
    },
    /// Tear down a sandbox — kill the container and remove the record.
    Destroy {
        /// Sandbox id (or unique prefix ≥ 4 chars).
        id: String,
    },
    /// List sandboxes in the current pillbox.
    List {
        /// Emit JSON. Stable schema — pin against `version: 1`.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum WorkspaceAction {
    /// Rotate the repository encryption password.
    Rekey,
}

#[derive(Subcommand, Debug)]
pub(crate) enum SecretAction {
    /// Store a secret value (reads from stdin by default).
    Add {
        name: String,
        #[arg(long, value_name = "VAR")]
        from_env: Option<String>,
        #[arg(long)]
        if_not_exists: bool,
        /// Write to the global pillbox instead of the resolved one.
        #[arg(long)]
        global: bool,
        /// Mark this secret as vaulted (stub-swap at injection time).
        #[arg(long)]
        vault: bool,
        #[arg(long, value_name = "KNOWN_NAME", requires = "vault",
              conflicts_with_all = ["host", "header_scheme", "prefix"])]
        maps_to: Option<String>,
        #[arg(long, value_name = "HOST", requires = "vault")]
        host: Option<String>,
        #[arg(long = "header-scheme", value_name = "SCHEME", requires = "vault")]
        header_scheme: Option<String>,
        #[arg(long, value_name = "PREFIX", requires = "vault")]
        prefix: Option<String>,
    },
    /// List stored secret names (project + global, deduplicated).
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show a secret's value (masked by default).
    Show {
        name: String,
        #[arg(long)]
        reveal: bool,
        #[arg(long, requires = "reveal")]
        to_stdout: bool,
        #[arg(long)]
        json: bool,
    },
    /// Delete a stored secret from the resolved scope (or `--global`).
    Rm {
        name: String,
        #[arg(long)]
        global: bool,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum EnvAction {
    Load {
        name: String,
        path: PathBuf,
        #[arg(long)]
        if_not_exists: bool,
        #[arg(long)]
        global: bool,
    },
    List {
        #[arg(long)]
        json: bool,
    },
    Show {
        name: String,
        #[arg(long)]
        reveal: bool,
        #[arg(long, requires = "reveal")]
        to_stdout: bool,
        #[arg(long)]
        json: bool,
    },
    Rm {
        name: String,
        #[arg(long)]
        global: bool,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum AuthAction {
    /// Run the OAuth flow inside a one-shot sandbox.
    Login {
        /// Agent to authenticate (`claude` | `codex` | `opencode`).
        #[arg(long, value_name = "AGENT")]
        agent: String,
        /// Reserved — v0.6 PR 2 always writes to global. Pass for
        /// forward compatibility; identical to default behavior today.
        #[arg(long)]
        global: bool,
    },
    /// Show which agents have authenticated state.
    List {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        global: bool,
    },
    /// Remove an agent's persistent state.
    Rm {
        provider: String,
        #[arg(long)]
        global: bool,
    },
}

/// Terminal status passed to `pillbox session done <id>`. Maps to the
/// `session.completed` / `session.failed` event types in `events.rs`.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub(crate) enum DoneStatus {
    Ok,
    Failed,
}

#[derive(Subcommand, Debug)]
pub(crate) enum SessionAction {
    /// List sessions started from this pillbox (oldest first).
    List {
        /// Emit a JSON array of session records. Pin to `version: 1`.
        #[arg(long)]
        json: bool,
    },
    /// Show one session (accepts a unique id prefix ≥ 4 chars).
    Info {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Diagnose one session: derived status, failure detail, and an activity
    /// summary from the durable log — the "what happened / why is it stuck"
    /// companion to `info`. Accepts a unique id prefix ≥ 4 chars.
    Diagnose {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Reattach to a detached session. Streams the session's PTY back
    /// into the current terminal. Detach again with Ctrl-A + D or by
    /// running `pillbox session detach <id>` from another shell.
    Attach { id: String },
    /// Signal a currently-attached pillbox process to detach. The
    /// session record is left in place; the backend keeps running.
    /// Idempotent — no error if the session is already detached.
    Detach { id: String },
    /// Tear down the backend resources (kill the sandbox) and remove
    /// the session record. Idempotent.
    Rm { id: String },
    /// Drive a running (detached) session: push text to its agent's PTY as if
    /// typed — the programmatic SendInput half (pair with `session subscribe`
    /// to read the response). Bytes are sent as-is; add a trailing newline to
    /// submit a prompt to a TUI agent. Local Docker sessions today.
    Send {
        id: String,
        /// Text to send (as-is). Include a trailing newline/`\r` to submit.
        text: String,
    },
    /// Watch a session's event stream rendered to this terminal — the
    /// human-facing reader over the durable log (the `docker logs` model;
    /// `subscribe` is the machine/WS sibling). For a live session it tails the
    /// transcript→log as the agent works. Ctrl-C to stop. Accepts an id prefix.
    Watch {
        id: String,
        /// Start from this seq (0 = full history from the top, the default).
        #[arg(long, default_value_t = 0)]
        from: u64,
    },
    /// Stream a session's durable event log to WebSocket subscribers as
    /// JSON (one Event per text frame, in seq order). The §0 read surface a
    /// chat bridge / orchestrator / browser connects to without a shell.
    /// Serves localhost until Ctrl-C. Accepts a unique id prefix.
    Subscribe {
        id: String,
        /// Start from this seq (0 = full replay from the top, the default).
        #[arg(long, default_value_t = 0)]
        from: u64,
        /// Address to bind (default `127.0.0.1:0` — an ephemeral port,
        /// printed on start).
        #[arg(long)]
        bind: Option<String>,
    },
    /// Rehydrate a session's result workspace into a directory. Reads
    /// `result_snapshot` from the session record (set by `session done
    /// --result-snapshot`) and asks the workspace backend to pull it.
    /// Used by orchestrators for post-mortem inspection of a failed
    /// fork: analyzer agents read the failed session's workspace
    /// without having to re-run anything.
    Pull {
        id: String,
        /// Destination directory. Created if missing. Defaults to
        /// `./session-<id>` so two sequential pulls don't clobber each
        /// other.
        #[arg(long, value_name = "DIR")]
        to: Option<PathBuf>,
    },
    /// Externally grade a session's result — the **verifiable, non-self-
    /// reported reward channel** the optimization loops gate on. Runs the grader
    /// with cwd = the rehydrated result-snapshot (or `--workspace`/`--snapshot`),
    /// captures its **exit code + output**, and appends a `scored` §0 event.
    /// `--cmd` is one verifier (exit 0 → passed/score 1.0, else 0.0; output →
    /// feedback); `--rubric FILE` is N named criteria → per-criterion verdicts
    /// (`criteria[]`) + a fractional score. Distinct from `session done
    /// --status`, the agent's self-report (Goodhart-banned as a reward).
    #[command(group(clap::ArgGroup::new("grader").required(true).args(["cmd", "rubric"])))]
    Score {
        id: String,
        /// Verifier command, run via `sh -c` with cwd = the graded workspace.
        /// Its exit status is the verifiable pass/fail; its output is the
        /// feedback gradient (e.g. `pytest -q`, `cargo test`, a scoring script).
        /// Mutually exclusive with `--rubric`.
        #[arg(long, value_name = "CMD")]
        cmd: Option<String>,
        /// Grade against a rubric file instead of one `--cmd`: each non-blank,
        /// non-`#` line is `NAME :: COMMAND`, a named criterion run via `sh -c`
        /// in the graded workspace. The `scored` event gains per-criterion
        /// verdicts (`criteria[]`) and `score` becomes the passed fraction — the
        /// rich, decomposed feedback an optimizer reflects on. Mutually exclusive
        /// with `--cmd`.
        #[arg(long, value_name = "FILE")]
        rubric: Option<PathBuf>,
        /// Grade this snapshot (rehydrated) instead of the session's
        /// `result_snapshot`.
        #[arg(long, value_name = "HANDLE")]
        snapshot: Option<String>,
        /// Grade this directory directly (skip snapshot rehydration) — for a
        /// session whose result is already on disk.
        #[arg(long, value_name = "DIR")]
        workspace: Option<PathBuf>,
        /// Run the grader inside a one-shot microVM (the runner image's
        /// toolchain) against the workspace, instead of on the host — for real
        /// repos whose tests need the image's deps. Requires the `libkrun`
        /// feature. The grader-VM is offline (no network) + secret-free by
        /// default; opt into network with `--grader-egress`.
        #[arg(long = "in-sandbox")]
        in_sandbox: bool,
        /// Let the `--in-sandbox` grader reach these hosts (repeatable) so a real
        /// repo's tests can fetch deps — e.g. `--grader-egress pypi.org
        /// --grader-egress files.pythonhosted.org` (pip) or `registry.npmjs.org`
        /// (npm). Routed through the same DNS-fence + MITM (empty swap, no creds)
        /// as a vault run; every other host stays fenced. Trades the offline
        /// reproducibility guarantee for reachability. Only valid with `--in-sandbox`.
        #[arg(long = "grader-egress", value_name = "HOST")]
        grader_egress: Vec<String>,
        /// Emit the verdict as JSON on stdout — `{version, session, grader,
        /// passed, score, feedback, seq}` — so a caller reads the structured
        /// result directly instead of scraping stdout text or the §0 log. The
        /// `scored` event is still appended either way; `seq` is its log seq.
        #[arg(long)]
        json: bool,
    },
    /// Drain a session's durable raw §0 capture (its persisted `/event` stream)
    /// into the canonical `log.jsonl`, post-hoc and idempotent. For headless /
    /// batch runs (the optimization loop) where no live `subscribe`/`watch` ran
    /// to fill the log: the reparented guest outlives `run`, so a host-side live
    /// tailer can't persist for it, but the guest's capture file does — so the
    /// full agent trajectory lands in the §0 log without racing the session.
    /// libkrun opencode today; docker/PTY sessions drain live via
    /// `session subscribe`/`watch`. Re-running is a no-op (already ingested).
    Ingest {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Read a session's durable §0 log (`log.jsonl`) — the per-session event
    /// stream `score`/`ingest`/`wait-idle` write. The structured §0 read an
    /// orchestrator uses instead of opening the on-disk log by hand. Emits one
    /// event JSON per line (seq order). Distinct from `session events`, which is
    /// the pillbox-wide *lifecycle* stream (`session.started`/`dropped`).
    Log {
        id: String,
        /// Keep only events whose payload `type` is one of these (repeatable;
        /// snake_case tags, e.g. `tool_call`, `scored`, `message_end`). Unknown
        /// tags simply match nothing — a typo yields empty output, not an error.
        #[arg(long = "type", value_name = "TYPE")]
        r#type: Vec<String>,
        /// Emit only the last matching event (after `--type` filtering) — the
        /// "what's the latest scored/idle verdict" read.
        #[arg(long)]
        last: bool,
        /// Start from this seq, inclusive (default 0 = the whole log).
        #[arg(long, value_name = "SEQ")]
        from: Option<u64>,
    },
    /// Block until the session's current turn goes idle — the agent finished and
    /// is waiting for input (the §0 `AttentionRequired{NeedsInput}` signal, or a
    /// terminal `RunFinished`). The drive-surface "turn done" primitive: an
    /// orchestrator `session send`s a prompt, then `wait-idle` instead of polling.
    /// Drains the §0 capture into the durable log WHILE waiting (so the trajectory
    /// lands too — a later `session ingest` is then redundant). Exits 0 when idle,
    /// 1 on `--timeout`. Reads new events from the current log tail (or `--from SEQ`).
    WaitIdle {
        id: String,
        /// Give up after this many seconds (exit 1). Omit to wait indefinitely.
        #[arg(long, value_name = "SECS")]
        timeout: Option<u64>,
        /// Wait for an idle event with `seq >= FROM` (default: only events after
        /// the current log tail — i.e. the turn you just triggered).
        #[arg(long, value_name = "SEQ")]
        from: Option<u64>,
    },
    /// Tear down every session whose `expires_at` is in the past.
    /// Drives `session rm` for each — sandbox killed, record deleted.
    /// Sessions with no `expires_at` (no `--ttl` at spawn) are left
    /// alone forever; the user / orchestrator owns the policy.
    /// Intended for cron / orchestrator schedules; pillbox doesn't
    /// auto-prune on every invocation.
    Prune {
        /// List what would be pruned without actually pruning. Useful
        /// for sanity-checking a cron entry before turning it on.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Emit a sandbox-side `session.started` event. Mirror of
    /// [`Done`] for the front of the lifecycle: the in-sandbox
    /// wrapper script calls this right before launching the agent
    /// so consumers can compute cold-start latency from
    /// `host.started_at` → `sandbox.started_at` (distinguished by
    /// the `emitter` attribute on each event).
    ///
    /// No-op cost when no sink is configured. The event payload is
    /// minimal — `session_id` + `started_at` + `emitter=sandbox` —
    /// because the host's prior `session.started` already carries
    /// the full record (agent_id, remote, backend, label, …);
    /// consumers correlate by `session_id`.
    Started { id: String },
    /// Mark a session done, emitting `session.completed` or
    /// `session.failed` to every configured sink (JSONL + webhook +
    /// OTel). Invoked manually for orchestrator-driven completion, or
    /// automatically by the in-sandbox wrapper script after the agent
    /// exits. Does NOT tear down the sandbox — use `session rm` for
    /// that.
    ///
    /// Sandbox-side use: when called from inside a sandbox where the
    /// session record doesn't exist locally, builds a stub payload from
    /// the id and relies on webhook / OTel sinks to ferry the event to
    /// the host or orchestrator.
    Done {
        id: String,
        /// `ok` → emits `session.completed`. `failed` → emits
        /// `session.failed`.
        #[arg(long, value_enum)]
        status: DoneStatus,
        /// Free-text reason. Only meaningful with `--status failed`;
        /// surfaced in the event payload + via OTel `status.message`.
        #[arg(long, value_name = "TEXT")]
        reason: Option<String>,
        /// Exit code of the agent process, if known. Surfaced in the
        /// event payload + as an OTel attribute on the span.
        #[arg(long = "exit-code", value_name = "N")]
        exit_code: Option<i32>,
        /// Path (rustic snapshot URL or local file) to the agent's
        /// captured tool-call trace. Carried verbatim through to event
        /// consumers; pillbox doesn't dereference it today.
        #[arg(long = "trace-path", value_name = "PATH")]
        trace_path: Option<String>,
        /// Rustic snapshot handle of the agent's result workspace,
        /// captured by the in-sandbox wrapper (`pillbox push --tag
        /// session-<id>`) after the agent exits. Recorded on the
        /// session record so `pillbox session pull <id>` can rehydrate
        /// it later; also included in the terminal-event payload.
        #[arg(long = "result-snapshot", value_name = "HANDLE")]
        result_snapshot: Option<String>,
    },
    /// Stream session lifecycle events as JSONL on stdout.
    ///
    /// v0.7 spike emits `session.started` and `session.dropped` only;
    /// PR 2 adds `completed`/`failed` + webhook + OTel sinks. The
    /// `--json` flag is reserved for compat — every line is already
    /// JSON today; the flag exists so PR 2 can introduce a human
    /// default without breaking orchestrator callers.
    Events {
        /// Keep streaming as new events arrive (`tail -f` shape).
        #[arg(long)]
        follow: bool,
        /// Reserved for compat. v0.7 spike output is always JSONL.
        #[arg(long)]
        json: bool,
    },
    /// Drain (or `--follow`) an agent-native transcript file
    /// (Claude Code's `~/.claude/projects/<encoded>/<uuid>.jsonl`
    /// or Codex's `~/.codex/sessions/<y>/<m>/<d>/rollout-*.jsonl`)
    /// and emit one OTLP child span per rendered event, parented
    /// under the session span derived from `--session-id`. Requires
    /// `OTEL_EXPORTER_OTLP_ENDPOINT` to be set to actually ship
    /// the spans — the parser still runs without it (useful for
    /// counting parses dry-run).
    ///
    /// Auto-emit-on-sandbox-launch (bind-mount the transcript dir,
    /// auto-spawn the tailer) is a follow-up; today the tail is
    /// driven by hand.
    Transcript {
        /// Path to the .jsonl transcript file.
        file: PathBuf,
        /// Pillbox-run session id whose session span the emitted
        /// transcript spans should parent under.
        #[arg(long = "session-id", value_name = "ID")]
        session_id: String,
        /// Harness that wrote the file (`claude` or `codex`).
        /// Auto-detected from path when omitted: `~/.claude/...`
        /// → claude, `~/.codex/...` → codex.
        #[arg(long, value_enum)]
        agent: Option<TranscriptAgent>,
        /// After draining existing content, block waiting on FS
        /// notifications and emit spans for each appended line.
        /// Exit with Ctrl-C. Designed for "watch your agent think"
        /// — point it at a live transcript and watch Workshop
        /// stream as the agent runs.
        #[arg(long)]
        follow: bool,
    },
}

/// Harness selector for `pillbox session transcript --agent`.
#[derive(clap::ValueEnum, Debug, Clone, Copy)]
pub(crate) enum TranscriptAgent {
    Claude,
    Codex,
}

#[derive(Subcommand, Debug)]
pub(crate) enum VaultAction {
    Ca {
        #[arg(long)]
        json: bool,
    },
    Status {
        #[arg(long)]
        json: bool,
    },
}
