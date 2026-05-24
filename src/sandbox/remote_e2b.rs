//! Remote-E2B sandbox backend — `pillbox run --remote NAME` where the
//! remote is `e2b://TEMPLATE_ID`.
//!
//! ## Flow (local side)
//!
//! 1. Resolve `--with` / `--env` exactly like the SSH backend does, into
//!    a [`VaultStdinBlob`] (the wire shape is shared — same struct, same
//!    `BLOB_VERSION`).
//! 2. Stage the blob to a local 0600 temp file. We can't pipe it through
//!    the helper subprocess's stdin because that channel forwards user
//!    keystrokes to the sandbox PTY.
//! 3. Extract the embedded Node helper script (`e2b-helper.mjs`) to
//!    `~/.pillbox/cache/e2b-helper-vX.mjs` if not already there, and
//!    exec `node helper.mjs attach --template T --blob-file F`. The
//!    helper inherits stdin/stdout/stderr.
//! 4. Helper uploads the blob into the sandbox via the E2B Files API,
//!    opens a PTY, and runs `pillbox run --vault-stdin < /tmp/blob` —
//!    the remote half is the same path the SSH backend uses.
//!
//! ## Why Node (not native Rust)
//!
//! No official E2B Rust SDK; the only third-party crate
//! (`e2b-sdk` 0.1.1) is code-interpreter only — no PTY, no
//! `commands.run`. Porting the SDK protocol natively is ~1.5K LOC of
//! HTTP + WebSocket plumbing we'd have to keep in sync with E2B's
//! release cadence. A small embedded helper is pragmatic; the cost is
//! a `node` + `npm i -g e2b` prereq on the user's machine.
//!
//! ## Why a file (not stdin) for the blob
//!
//! The PTY echoes its input by default, and turning echo off only takes
//! effect after the shell runs `stty`. If we sent the blob through the
//! PTY before then, the user's terminal would briefly display tens of
//! kilobytes of JSON (including secret material). Staging to a sandbox
//! `/tmp` file via the Files API keeps secrets off the user-visible
//! display path. The file is unlinked by the launch line as soon as
//! `pillbox run --vault-stdin` has read it.

use std::{
    io::{self, BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
};

use anyhow::{Context, Result};

use super::remote_ssh::{build_vault_stdin_blob, VaultStdinBlob};
use super::SandboxBackend;
use crate::agents::{AgentSpec, RunOpts};
use crate::config::BackendKind;
use crate::errors::PillboxError;
use crate::paths::{ensure_mode_0700, pillbox_root};
use crate::pillbox::Pillbox;
use crate::remote::{E2bRef, Remote, RemoteUrl};
use crate::session::{self, Backend, Session, BACKEND_E2B};

/// Embedded helper script — bundled into the binary so users don't have
/// to manage two files in lockstep. The cache path embeds
/// `CARGO_PKG_VERSION`, so an upgraded pillbox writes a new
/// `e2b-helper-v<new>.mjs` next to the old one and uses the new file
/// going forward; older versioned files sit on disk until the user
/// cleans `~/.pillbox/cache/`. We do NOT mutate an existing file in
/// place — that would race with a concurrent older `pillbox run`
/// reading the same path.
const HELPER_SCRIPT: &str = include_str!("e2b-helper.mjs");

/// Wire-protocol version the Rust side expects for the helper's
/// `{type:"sandbox-up"}` handshake on stderr. Bumped only when the
/// shape changes in a breaking way; the helper carries the same constant
/// (`PROTO_VERSION` in `e2b-helper.mjs`) and a mismatch fails the run
/// loudly so a stale cached helper from an older pillbox can't silently
/// drop into the new dispatcher.
const HELPER_PROTO_VERSION: u32 = 1;

/// Exit code the helper uses when the user intentionally detaches
/// (Ctrl-A D) — distinct from 0 (clean PTY exit), 130 (SIGINT), 143
/// (SIGTERM). Kept in lockstep with `DETACH_EXIT_CODE` in
/// `e2b-helper.mjs`.
pub(crate) const DETACH_EXIT_CODE: i32 = 100;

/// `pillbox run --remote NAME` backend for an E2B remote.
pub(crate) struct RemoteE2bSandbox {
    remote: Remote,
}

impl RemoteE2bSandbox {
    pub(crate) fn new(remote: Remote) -> Self {
        Self { remote }
    }
}

impl SandboxBackend for RemoteE2bSandbox {
    fn run(&self, spec: &AgentSpec, opts: RunOpts, resolved: &Pillbox) -> Result<()> {
        // Same workspace gating as the SSH backend: a local-rustic
        // workspace has no story for "the E2B sandbox pulls from where".
        // S3-shaped backends share config between local and sandbox, so
        // no workspace bytes have to cross the helper subprocess.
        let meta = resolved.meta.as_ref().ok_or_else(|| {
            PillboxError::usage(
                "run --remote (e2b)",
                "the global pillbox can't run remotely; cd into a project pillbox first",
            )
        })?;
        if meta.workspace.backend_kind() != BackendKind::S3 {
            return Err(PillboxError::usage(
                "run --remote (e2b)",
                "remote runs require an S3-shaped workspace backend \
                 (the E2B sandbox needs the same bucket/endpoint to restore from). \
                 Local-rustic via tarball transport is the planned follow-up.",
            )
            .with_next("pillbox new --workspace-backend s3 …")
            .into());
        }

        // Same belt-and-suspenders re-validation as the SSH backend:
        // someone could hand-edit the TOML between `add` and `run`. Route
        // through `Remote::parsed_url` so the `"e2b://"` literal stays in
        // one place (`remote::parse_remote_url`).
        let e2b = match self.remote.parsed_url().map_err(|e| {
            PillboxError::config(
                "run --remote (e2b)",
                format!("remote `{}`: {e}", self.remote.name),
            )
        })? {
            RemoteUrl::E2b(e) => e,
            RemoteUrl::Ssh(_) => {
                return Err(PillboxError::config(
                    "run --remote (e2b)",
                    format!("remote `{}` is not an e2b:// URL", self.remote.name),
                )
                .into());
            }
        };

        let blob = build_vault_stdin_blob(spec, &opts, resolved, "run --remote (e2b)")?;
        let label = opts.label.clone();
        let detach = opts.detach;
        let json = opts.json;
        // Pre-compute the absolute expiration timestamp from the
        // `--ttl` duration; storing the resolved instant (not the raw
        // duration) keeps the meaning unambiguous regardless of when
        // `session prune` runs.
        let expires_at = opts.ttl_seconds.map(session::expires_at_from_ttl);
        run_attach(
            resolved,
            &self.remote.name,
            spec.id,
            label,
            &e2b,
            &blob,
            detach,
            json,
            expires_at,
        )
    }
}

/// Initial attach — create a new sandbox + PTY, launch the agent, and
/// either stream the PTY back to the user (interactive) or exit
/// immediately after the launch line is delivered (`--detach`). Either
/// way the session record is written to the local registry so the user
/// can `pillbox session list` / `attach` / `rm` it later.
///
/// Exit-code semantics from the helper:
///   - 0  + last event `detached`       → `--detach` success: keep record, sandbox running
///   - 0  (no detach event)             → agent finished cleanly: kill sandbox, remove record
///   - 100 + last event `detach-pressed` → user typed Ctrl-A D: keep record, sandbox running
///   - non-zero (other)                 → failure: kill sandbox best-effort, remove record
#[allow(clippy::too_many_arguments)]
fn run_attach(
    resolved: &Pillbox,
    remote_name: &str,
    agent_id: &'static str,
    label: Option<String>,
    e2b: &E2bRef,
    blob: &VaultStdinBlob,
    detach: bool,
    json: bool,
    expires_at: Option<String>,
) -> Result<()> {
    let blob_bytes = blob.to_bytes()?;
    // `tempfile()` creates the file atomically via `O_CREAT | O_EXCL`
    // with mode 0o600 on Unix. We write through its open handle so the
    // staged blob never exists on disk with a wider mode and we never
    // re-open the path (which would risk a symlink/race against the
    // predictable suffix).
    let mut tmp = tempfile::Builder::new()
        .prefix("pillbox-e2b-blob-")
        .suffix(".json")
        .tempfile()
        .map_err(|e| PillboxError::runtime("run --remote (e2b)", format!("stage blob: {e}")))?;
    tmp.as_file_mut().write_all(&blob_bytes).map_err(|e| {
        PillboxError::runtime("run --remote (e2b)", format!("write staged blob: {e}"))
    })?;
    tmp.as_file_mut().sync_all().map_err(|e| {
        PillboxError::runtime("run --remote (e2b)", format!("flush staged blob: {e}"))
    })?;

    let helper = ensure_helper_extracted()?;
    eprintln!(
        "pillbox: connecting to `{remote_name}` (e2b://{}) …",
        e2b.template
    );
    if !detach {
        // Interactive attach — surface the detach hotkey so the user
        // can leave the session running without reading the docs.
        eprintln!("pillbox: detach with Ctrl-A D to keep the sandbox running.");
    }

    // Pre-mint the session id so the sandbox-side wrapper can bake it
    // into the `pillbox session done` call. Without pre-minting, the id
    // wouldn't exist until after the helper handshake — too late for
    // the wrapper to know what to reference. The same id gets written
    // into the registry once the handshake completes; if the helper
    // fails before then, the id is orphaned (no record persists), which
    // is fine — id collisions are astronomically unlikely.
    let session_id = Session::new_id();

    let mut cmd = Command::new("node");
    cmd.arg(&helper)
        .arg("attach")
        .arg("--template")
        .arg(&e2b.template)
        .arg("--name")
        .arg(remote_name)
        .arg("--blob-file")
        .arg(tmp.path())
        .arg("--session-id")
        .arg(&session_id);
    if detach {
        cmd.arg("--detach");
    }
    // Forward the events webhook to the sandbox-side wrapper so its
    // `pillbox session done` call can POST the terminal event back.
    // Read from process env (set either by the user's shell or by the
    // `--events-webhook URL` flag below; we don't gate on detach
    // because attached runs also want completion events for their
    // observability story).
    if let Ok(url) = std::env::var("PILLBOX_EVENTS_WEBHOOK") {
        if !url.is_empty() {
            cmd.arg("--events-webhook").arg(&url);
        }
    }
    let (status, pumped) = run_helper(cmd, "run --remote (e2b)")?;
    drop(tmp);

    // Decide what to do based on the final pump state + the helper's
    // exit code. The session record is written before any branch that
    // leaves the sandbox alive, so the user always has an id to attach
    // to. `detach == true` is the only branch where the helper exits
    // with status 0 AND we keep the sandbox.
    match (status.success(), pumped.last_event.as_deref(), detach) {
        // Interactive launch, agent ran to completion cleanly — tear
        // it all down (the sandbox is empty / agent done).
        (true, None, false) => {
            // No record was ever written — nothing to clean up.
            Ok(())
        }
        // `--detach` path: helper emitted `detached` after launch and
        // exited. Persist the session so the user can reattach.
        (true, Some("detached"), true) => {
            let session = persist_and_emit_started(
                PersistArgs {
                    resolved,
                    remote_name,
                    agent_id,
                    label,
                    pre_minted_id: &session_id,
                    expires_at: expires_at.clone(),
                },
                &pumped,
            )?;
            if json {
                // Machine-readable: matches `pillbox session info --json`
                // so orchestrators can use the same parsing path.
                println!(
                    "{}",
                    crate::paths::json_v1(vec![("session", session.to_json_value())])
                );
            } else {
                println!(
                    "pillbox: ✓ session `{}` started in background (sandbox `{}`).",
                    session.id, session.sandbox_id
                );
                println!("         pillbox session attach {}  # reattach", session.id);
            }
            Ok(())
        }
        // Interactive Ctrl-A D — helper exited 100 with `detach-pressed`.
        // Persist the session and tell the user how to come back.
        (false, Some("detach-pressed"), false) if status.code() == Some(DETACH_EXIT_CODE) => {
            let session = persist_and_emit_started(
                PersistArgs {
                    resolved,
                    remote_name,
                    agent_id,
                    label,
                    pre_minted_id: &session_id,
                    expires_at: expires_at.clone(),
                },
                &pumped,
            )?;
            eprintln!(
                "pillbox: detached. reattach with `pillbox session attach {}`",
                session.id
            );
            Ok(())
        }
        // Anything else is a failure — surface the helper diagnostic
        // and a generic prereq hint if we never saw the handshake.
        _ => {
            let mut err = PillboxError::runtime(
                "run --remote (e2b)",
                format!("helper exited with status {status}"),
            );
            if pumped.sandbox_id.is_none() {
                err = err.with_next(
                    "check the helper diagnostic above; common causes: \
                     `npm i -g e2b`, set $E2B_API_KEY, valid template id",
                );
            }
            Err(err.into())
        }
    }
}

/// `pillbox session attach <id>` for an E2B session. Spawns the helper
/// in `reattach` mode, marks the session as attached for the duration,
/// and clears the mark on exit (clean detach, Ctrl-A D, or peer death).
pub(crate) fn reattach(resolved: &Pillbox, remote: &Remote, session: &Session) -> Result<()> {
    if Backend::parse(&session.backend) != Some(Backend::E2b) {
        return Err(PillboxError::usage(
            "session attach",
            format!(
                "session `{}` is backed by `{}`, not e2b — attach not yet supported",
                session.id, session.backend
            ),
        )
        .into());
    }
    let helper = ensure_helper_extracted()?;
    eprintln!(
        "pillbox: reattaching to `{}` (sandbox `{}`) …",
        remote.name, session.sandbox_id
    );
    // Surface the detach hotkey on every reattach — without this the
    // user has no way to discover it short of reading the docs.
    eprintln!("pillbox: detach with Ctrl-A D (the sandbox keeps running).");

    session::mark_attached(resolved, &session.id, std::process::id() as i64)?;

    let mut cmd = Command::new("node");
    cmd.arg(&helper)
        .arg("reattach")
        .arg("--sandbox-id")
        .arg(&session.sandbox_id)
        .arg("--pid")
        .arg(session.pty_pid.to_string());
    let pump_result = run_helper(cmd, "session attach");

    // Always clear attached_pid before returning, even on error. The
    // session record is still valid (sandbox is up); only the "who's
    // attached" stamp changes.
    let _ = session::mark_detached(resolved, &session.id);

    let (status, pumped) = pump_result?;
    if status.code() == Some(DETACH_EXIT_CODE)
        && pumped.last_event.as_deref() == Some("detach-pressed")
    {
        eprintln!(
            "pillbox: detached. reattach with `pillbox session attach {}`",
            session.id
        );
        return Ok(());
    }
    if !status.success() {
        return Err(PillboxError::runtime(
            "session attach",
            format!("helper exited with status {status}"),
        )
        .into());
    }
    // Process inside the PTY exited (e.g. user typed `exit`). The
    // sandbox is empty but still alive — leave the record. The user
    // can `pillbox session rm <id>` to tear it down or reattach to
    // start something else inside.
    Ok(())
}

/// `pillbox session rm <id>` for an E2B session. Spawns the helper in
/// `kill` mode, then unconditionally deletes the local record (a
/// failed kill is logged but doesn't leave a dangling session entry —
/// the sandbox may have already timed out / been killed elsewhere).
///
/// **Trade-off (intentional):** if `sandbox.kill` fails for an
/// unrelated reason (e.g. transient network blip) we still drop the
/// record. The user loses the handle to retry from pillbox, but the
/// sandbox will time out on E2B's side (the `SANDBOX_TIMEOUT_MS`
/// helper default) and any further cleanup can be done from the
/// `e2b` CLI directly. The alternative (gating delete on kill) leaves
/// stale records for sandboxes that E2B has already reaped, which we
/// saw bite users more often. Reconsider if/when E2B exposes a clean
/// "kill or already-dead" status code.
pub(crate) fn kill_session(resolved: &Pillbox, session: &Session) -> Result<()> {
    if Backend::parse(&session.backend) != Some(Backend::E2b) {
        return Err(PillboxError::usage(
            "session rm",
            format!(
                "session `{}` is backed by `{}`, not e2b — rm not yet supported",
                session.id, session.backend
            ),
        )
        .into());
    }
    let helper = ensure_helper_extracted()?;
    let mut cmd = Command::new("node");
    cmd.arg(&helper)
        .arg("kill")
        .arg("--sandbox-id")
        .arg(&session.sandbox_id);
    // `kill` mode has no PTY and no user interaction — capture stdout
    // so a chatty SDK doesn't leak random bytes onto the user's terminal.
    // Stderr keeps the helper handshake + diagnostics.
    let outcome = run_helper_quiet(cmd, "session rm");
    if let Err(e) = outcome.as_ref() {
        eprintln!("pillbox: warning: sandbox kill failed: {e}");
    }
    // Emit the lifecycle event BEFORE deleting the record so the event
    // payload can reference a still-valid `Session`. Best-effort — a
    // failed emit only logs to stderr.
    crate::events::emit_session_event(
        resolved,
        crate::events::EventType::SessionDropped,
        &session.id,
        Some(session),
    );
    session::delete(resolved, &session.id)?;
    println!(
        "pillbox: ✓ session `{}` removed (sandbox `{}` killed).",
        session.id, session.sandbox_id
    );
    Ok(())
}

/// Everything `persist_session_from_pump` and `persist_and_emit_started`
/// need beyond what the dynamic `PumpOutcome` already carries. Kept as
/// a struct so the two `run_attach` call sites stay readable (named
/// fields, not positional bag-of-args) and so adding another field
/// only touches one signature.
pub(super) struct PersistArgs<'a> {
    pub(super) resolved: &'a Pillbox,
    pub(super) remote_name: &'a str,
    pub(super) agent_id: &'static str,
    pub(super) label: Option<String>,
    pub(super) pre_minted_id: &'a str,
    pub(super) expires_at: Option<String>,
}

/// Persist a freshly-started session and emit the `session.started`
/// lifecycle event in one call. The two `run_attach` happy-path arms
/// (`--detach` and Ctrl-A D) both need the exact same pair, in the same
/// order; bundling them here keeps event emission from drifting out of
/// step with persistence the next time we add a third detach-shaped
/// outcome. `attached_pid` is always `None` on initial detach — both
/// arms are post-helper-exit, so by definition nothing is attached.
fn persist_and_emit_started(args: PersistArgs<'_>, pump: &PumpOutcome) -> Result<Session> {
    let resolved = args.resolved;
    let session = persist_session_from_pump(args, pump, None)?;
    crate::events::emit_session_event(
        resolved,
        crate::events::EventType::SessionStarted {
            parent_session_id: None,
        },
        &session.id,
        Some(&session),
    );
    Ok(session)
}

/// Build a [`Session`] from the data we learned during the helper run,
/// persist it, and return it. Pulled out so `run_attach` doesn't grow
/// duplicate "did we have a sandbox_id and pid? then write a record"
/// blocks at each happy path.
fn persist_session_from_pump(
    args: PersistArgs<'_>,
    pump: &PumpOutcome,
    attached_pid: Option<i64>,
) -> Result<Session> {
    let sandbox_id = pump.sandbox_id.clone().ok_or_else(|| {
        PillboxError::runtime(
            "run --remote (e2b)",
            "helper exited successfully but never sent the sandbox-up handshake",
        )
    })?;
    // The id was minted at the top of `run_attach` so the sandbox-side
    // wrapper script could bake it into the `pillbox session done` call.
    // Pull it through here so the registry entry matches what the
    // wrapper will reference.
    let session = Session {
        id: args.pre_minted_id.to_string(),
        label: args.label,
        remote: args.remote_name.to_string(),
        backend: BACKEND_E2B.to_string(),
        sandbox_id,
        pty_pid: pump.pid.unwrap_or(0),
        agent_id: args.agent_id.to_string(),
        started_at: session::now_rfc3339(),
        attached_pid,
        base_snapshot: crate::workspace::latest_snapshot_handle(args.resolved),
        result_snapshot: None,
        expires_at: args.expires_at,
    };
    session::write(args.resolved, &session)?;
    Ok(session)
}

/// Run a pre-configured helper command with full stdio inheritance
/// (terminal in, PTY out). Pumps stderr line-by-line on a background
/// thread; returns the helper's exit status + everything we learned
/// from its stderr stream.
fn run_helper(
    mut cmd: Command,
    action: &'static str,
) -> Result<(std::process::ExitStatus, PumpOutcome)> {
    cmd.stdin(Stdio::inherit());
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::piped());
    spawn_and_pump(cmd, action)
}

/// Like [`run_helper`] but captures stdout too. Used by `kill` mode
/// where the helper has no PTY and any bytes on stdout would be SDK
/// noise spilling onto the user's terminal.
fn run_helper_quiet(mut cmd: Command, action: &'static str) -> Result<()> {
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let (status, _pumped) = spawn_and_pump(cmd, action)?;
    if !status.success() {
        return Err(
            PillboxError::runtime(action, format!("helper exited with status {status}")).into(),
        );
    }
    Ok(())
}

fn spawn_and_pump(
    mut cmd: Command,
    action: &'static str,
) -> Result<(std::process::ExitStatus, PumpOutcome)> {
    let mut child: Child = cmd.spawn().map_err(|e| {
        PillboxError::resource(action, format!("could not spawn node: {e}"))
            .with_next("install Node.js + run `npm i -g e2b` (https://e2b.dev/docs)")
    })?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| PillboxError::runtime(action, "helper stderr unexpectedly closed"))?;
    let stderr_thread = std::thread::spawn(move || pump_helper_stderr(stderr));
    let status = child
        .wait()
        .map_err(|e| PillboxError::runtime(action, format!("wait on helper: {e}")))?;
    let pumped = stderr_thread.join().unwrap_or_default();
    Ok((status, pumped))
}

/// What `pump_helper_stderr` learned from the helper's stderr stream.
/// The Rust side uses these to (a) write the session record after a
/// successful handshake and (b) distinguish "user detached" from
/// "agent finished" / "helper crashed" via `last_event`.
#[derive(Debug, Default)]
struct PumpOutcome {
    /// Sandbox id from the `sandbox-up` handshake (if seen).
    sandbox_id: Option<String>,
    /// PTY pid from the `sandbox-up` handshake (if present — only
    /// attach + reattach send it; `kill` does not).
    pid: Option<i64>,
    /// `type` of the last JSON event the helper wrote — typically
    /// `sandbox-up`, `detached`, or `detach-pressed`. Used by the
    /// caller to disambiguate the meaning of the helper's exit code.
    last_event: Option<String>,
}

/// Extract `e2b-helper.mjs` to `~/.pillbox/cache/e2b-helper-v<pkg>.mjs`.
/// The pkg-version suffix means an upgraded pillbox replaces the cache
/// automatically; a stale copy from an older binary can't accidentally
/// be invoked by a newer one. We check `exists()` first because the
/// happy path (every run after the first) reads only — no write needed.
pub(crate) fn ensure_helper_extracted() -> Result<PathBuf> {
    let dir = pillbox_root()?.join("cache");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    // `create_dir_all` honors the process umask (typically 022 → 0755).
    // The parent `~/.pillbox/` is already 0700 so this is defense-in-
    // depth, but every other pillbox-owned dir is 0700 — keep this
    // invariant uniform so a future `chmod` audit doesn't surface a
    // stray 0755.
    ensure_mode_0700(&dir)?;
    let path = dir.join(format!("e2b-helper-v{}.mjs", env!("CARGO_PKG_VERSION")));
    if !path.exists() {
        std::fs::write(&path, HELPER_SCRIPT.as_bytes())
            .with_context(|| format!("write {}", path.display()))?;
    }
    Ok(path)
}

/// Hard cap on a single helper stderr line we'll buffer in memory.
/// Real helper events are ~100 bytes; sanity-cap at 64 KiB so a buggy
/// SDK or someone feeding non-newline-terminated junk through the pipe
/// can't make us grow a `String` unboundedly. Lines longer than this
/// are truncated with a `…` marker and we resync on the next newline.
const MAX_HELPER_LINE_BYTES: usize = 64 * 1024;

/// Read helper stderr line-by-line.
///
/// The protocol is a sequence of JSON event lines (see `e2b-helper.mjs`
/// header for the schema). The first such line is the `sandbox-up`
/// handshake; subsequent lines may be `detached` / `detach-pressed`.
/// Everything that isn't a recognized JSON event is forwarded raw so
/// real diagnostics (network errors, `sandbox.kill` failures, stack
/// traces from the SDK) stay visible.
///
/// Forwarded text is sanitized through [`sanitize_terminal_line`] so a
/// chatty SDK / network error can't smuggle ANSI escape sequences (cursor
/// moves, title rewrites, OSC 8 hyperlinks) through to the user's
/// terminal. The local pillbox-formatted messages above this layer are
/// trusted and pass through unchanged.
///
/// Per-line memory is bounded by [`MAX_HELPER_LINE_BYTES`] so a runaway
/// helper writing no newline can't make the pump thread balloon. The
/// reader resynchronises at the next `\n` after the truncation marker.
///
/// Returns the cumulative state at end-of-stream — caller uses
/// `last_event` to disambiguate "user detached" from "agent finished".
fn pump_helper_stderr(stderr: std::process::ChildStderr) -> PumpOutcome {
    let mut outcome = PumpOutcome::default();
    let mut reader = BufReader::new(stderr);
    let mut sink = io::stderr();
    loop {
        let line = match read_bounded_line(&mut reader, MAX_HELPER_LINE_BYTES) {
            Ok(Some(l)) => l,
            Ok(None) => break,
            Err(_) => break,
        };
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            // Free-text diagnostic — forward sanitized.
            let _ = writeln!(sink, "{}", sanitize_terminal_line(&line));
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(trimmed) {
            Ok(v) => match v.get("type").and_then(|x| x.as_str()) {
                Some("sandbox-up") => {
                    let proto = v.get("protoVersion").and_then(|x| x.as_u64()).unwrap_or(0);
                    let sandbox_id = v
                        .get("sandboxId")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    let pid = v.get("pid").and_then(|x| x.as_i64());
                    if proto != u64::from(HELPER_PROTO_VERSION) {
                        let _ = writeln!(
                            sink,
                            "pillbox: helper protoVersion mismatch (got {proto}, expected {HELPER_PROTO_VERSION}). \
                             Stale extracted helper — `rm ~/.pillbox/cache/e2b-helper-*` and retry."
                        );
                        continue;
                    }
                    let _ = writeln!(
                        sink,
                        "pillbox: ✓ sandbox `{}` up",
                        sanitize_terminal_line(&sandbox_id)
                    );
                    outcome.sandbox_id = Some(sandbox_id);
                    outcome.pid = pid;
                    outcome.last_event = Some("sandbox-up".into());
                }
                Some(ty @ ("detached" | "detach-pressed")) => {
                    outcome.last_event = Some(ty.to_string());
                }
                Some(ty) => {
                    let _ = writeln!(
                        sink,
                        "pillbox: unexpected helper event `{}` — forwarding raw",
                        sanitize_terminal_line(ty)
                    );
                    let _ = writeln!(sink, "{}", sanitize_terminal_line(&line));
                }
                None => {
                    let _ = writeln!(sink, "{}", sanitize_terminal_line(&line));
                }
            },
            Err(_) => {
                // JSON-looking but malformed — likely a diagnostic that
                // happens to start with `{`. Forward sanitized.
                let _ = writeln!(sink, "{}", sanitize_terminal_line(&line));
            }
        }
    }
    outcome
}

/// Read up to one newline-terminated line from `reader`, capping the
/// in-memory line buffer at `max`. Returns `Ok(None)` on clean EOF,
/// `Ok(Some(line))` for a normal (or truncated) line — the trailing
/// `\n` is stripped if present.
///
/// Truncation strategy: once the line buffer hits `max`, we append a
/// `…` marker and then drain bytes from the underlying reader until we
/// hit `\n` (or EOF), without copying them anywhere. This keeps memory
/// bounded while still resynchronising on the next line boundary so a
/// runaway helper can't poison the pump for the rest of the stream.
fn read_bounded_line<R: BufRead>(reader: &mut R, max: usize) -> io::Result<Option<String>> {
    let mut buf: Vec<u8> = Vec::new();
    let mut truncated = false;
    loop {
        let chunk = match reader.fill_buf() {
            Ok(c) => c,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if chunk.is_empty() {
            // EOF. If we have buffered bytes return them as the final
            // line (no trailing newline); otherwise signal end-of-stream.
            if buf.is_empty() && !truncated {
                return Ok(None);
            }
            return Ok(Some(finalize_bounded(buf, truncated)));
        }
        match chunk.iter().position(|&b| b == b'\n') {
            Some(idx) => {
                if !truncated {
                    let take = (max - buf.len()).min(idx);
                    buf.extend_from_slice(&chunk[..take]);
                    if take < idx {
                        truncated = true;
                    }
                }
                // Consume up to AND including the newline.
                reader.consume(idx + 1);
                return Ok(Some(finalize_bounded(buf, truncated)));
            }
            None => {
                if !truncated {
                    let room = max.saturating_sub(buf.len());
                    let take = room.min(chunk.len());
                    buf.extend_from_slice(&chunk[..take]);
                    if buf.len() >= max {
                        truncated = true;
                    }
                }
                let len = chunk.len();
                reader.consume(len);
            }
        }
    }
}

fn finalize_bounded(buf: Vec<u8>, truncated: bool) -> String {
    let mut s = String::from_utf8_lossy(&buf).into_owned();
    if truncated {
        s.push('…');
    }
    s
}

/// Replace control bytes that can drive a terminal (ESC = 0x1B, CSI
/// 0x9B, BEL = 0x07, plus the rest of the C0 set except `\t`) with a
/// printable `^X` form so a stderr diagnostic forwarded from the helper
/// can't move the cursor, rewrite the title, or smuggle an OSC 8
/// hyperlink onto the user's terminal. Cheap — only allocates when the
/// input actually contains something dangerous.
fn sanitize_terminal_line(s: &str) -> std::borrow::Cow<'_, str> {
    let needs_sanitize = s
        .bytes()
        .any(|b| b == 0x1B || b == 0x07 || (b < 0x20 && b != b'\t'));
    if !needs_sanitize {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        let c = ch as u32;
        if c == 0x1B {
            out.push_str("^[");
        } else if c == 0x07 {
            out.push_str("^G");
        } else if c < 0x20 && ch != '\t' {
            // Caret notation for other C0 controls.
            out.push('^');
            out.push(char::from_u32(c + 0x40).unwrap_or('?'));
        } else {
            out.push(ch);
        }
    }
    std::borrow::Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::with_isolated_home;

    #[test]
    fn helper_script_is_embedded_and_nonempty() {
        assert!(HELPER_SCRIPT.contains("Sandbox.create"));
        assert!(HELPER_SCRIPT.contains("PILLBOX_BOOT"));
    }

    #[test]
    fn helper_extracts_into_pillbox_cache() {
        with_isolated_home("e2b-helper-extract", || {
            let p = ensure_helper_extracted().unwrap();
            assert!(p.exists());
            let body = std::fs::read_to_string(&p).unwrap();
            assert!(body.starts_with("#!/usr/bin/env node"));
            // Idempotent: second call should not error / re-write.
            let p2 = ensure_helper_extracted().unwrap();
            assert_eq!(p, p2);
        });
    }

    #[test]
    fn helper_path_is_versioned() {
        with_isolated_home("e2b-helper-version", || {
            let p = ensure_helper_extracted().unwrap();
            let stem = p.file_stem().unwrap().to_str().unwrap();
            assert!(stem.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))));
        });
    }

    #[test]
    fn sanitize_terminal_line_passes_plain_text() {
        // Borrowed (zero-alloc) fast path for the common case.
        let s = "sandbox sb_123 ready";
        let out = sanitize_terminal_line(s);
        assert!(matches!(out, std::borrow::Cow::Borrowed(_)));
        assert_eq!(out, s);
    }

    #[test]
    fn sanitize_terminal_line_neutralizes_esc_and_csi() {
        // Cursor-up: ESC [ A — must not survive.
        let s = "boom\x1b[Aoverwrite";
        let out = sanitize_terminal_line(s);
        assert_eq!(out, "boom^[[Aoverwrite");
        assert!(!out.contains('\x1b'));
    }

    #[test]
    fn sanitize_terminal_line_neutralizes_osc_hyperlink() {
        // OSC 8 hyperlink frames a clickable link in modern terminals.
        // Sanitizer strips the ESC/BEL framing so the literal URL shows.
        let s = "\x1b]8;;https://evil.example/\x07click\x1b]8;;\x07";
        let out = sanitize_terminal_line(s);
        assert!(!out.contains('\x1b'));
        assert!(!out.contains('\x07'));
        assert!(out.contains("https://evil.example/"));
    }

    #[test]
    fn sanitize_terminal_line_keeps_tab() {
        // Tab is benign and breaking it would mangle real diagnostics.
        let s = "col1\tcol2";
        let out = sanitize_terminal_line(s);
        assert_eq!(out, s);
    }

    #[test]
    fn read_bounded_line_reads_normal_lines() {
        let data = b"hello\nworld\n";
        let mut reader = BufReader::new(&data[..]);
        let a = read_bounded_line(&mut reader, 64).unwrap();
        assert_eq!(a.as_deref(), Some("hello"));
        let b = read_bounded_line(&mut reader, 64).unwrap();
        assert_eq!(b.as_deref(), Some("world"));
        let c = read_bounded_line(&mut reader, 64).unwrap();
        assert!(c.is_none(), "EOF after last newline");
    }

    #[test]
    fn read_bounded_line_returns_final_unterminated_line() {
        // Stream ends without a trailing newline — the last line still
        // surfaces so we don't drop helper diagnostics on a crashy exit.
        let data = b"final-line";
        let mut reader = BufReader::new(&data[..]);
        let a = read_bounded_line(&mut reader, 64).unwrap();
        assert_eq!(a.as_deref(), Some("final-line"));
        let b = read_bounded_line(&mut reader, 64).unwrap();
        assert!(b.is_none());
    }

    #[test]
    fn read_bounded_line_truncates_runaway_lines() {
        // Megabyte of `A` followed by a newline — must not allocate more
        // than `max` and must resync on the next line.
        let mut data = vec![b'A'; 1_000_000];
        data.push(b'\n');
        data.extend_from_slice(b"next\n");
        let mut reader = BufReader::new(&data[..]);
        let a = read_bounded_line(&mut reader, 128).unwrap().unwrap();
        // Truncation marker appended.
        assert!(a.ends_with('…'), "expected truncation marker, got {a:?}");
        assert!(
            a.len() <= 128 + '…'.len_utf8(),
            "bounded line grew past cap: {}",
            a.len()
        );
        // Next line is intact — pump resynced past the runaway.
        let b = read_bounded_line(&mut reader, 128).unwrap();
        assert_eq!(b.as_deref(), Some("next"));
    }
}
