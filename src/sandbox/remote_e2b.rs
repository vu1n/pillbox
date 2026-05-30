//! Remote-E2B sandbox backend — `pillbox run --remote NAME` where the
//! remote is `e2b://TEMPLATE_ID`.
//!
//! ## Flow (local side)
//!
//! 1. Resolve `--with` / `--env` exactly like the SSH backend does, into
//!    a [`VaultStdinBlob`] (the wire shape is shared — same struct, same
//!    `BLOB_VERSION`).
//! 2. Stage the blob to a local 0600 temp file. stdin is the frame
//!    channel (see below), so it can't also carry the blob.
//! 3. Extract the embedded Node helper script (`e2b-helper.mjs`) to
//!    `~/.pillbox/cache/e2b-helper-vX.mjs` if not already there, and
//!    spawn `node helper.mjs attach …` with stdin/stdout PIPED.
//! 4. The helper uploads the blob, launches `pillbox pty-host` in the
//!    sandbox (which runs the agent under a real PTY and serves the
//!    attach-transport frame protocol on a unix socket), and shuttles
//!    raw frames between its own stdio and an in-sandbox `pillbox
//!    pty-relay`. We run [`pump::attach_terminal`] over the helper's
//!    stdout/stdin — the SAME pump the docker and ssh backends use. The
//!    helper is a dumb byte pipe; all protocol/raw-mode/Ctrl-A logic is
//!    host-side. Its stderr carries the JSON handshake (`sandbox-up` /
//!    `detached`).
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
//! stdin carries the attach-transport frames from the host pump, so it
//! can't also carry the vault blob. Staging to a sandbox `/tmp` file via
//! the Files API also keeps the secret material off any display path.
//! The file is unlinked by the pty-host wrapper as soon as `pillbox run
//! --vault-stdin` has read it.

use std::{
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};

use anyhow::{Context, Result};

use crate::attach::pump::{self, Outcome};

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
            RemoteUrl::Ssh(_) | RemoteUrl::Docker(_) => {
                return Err(PillboxError::config(
                    "run --remote (e2b)",
                    format!("remote `{}` is not an e2b:// URL", self.remote.name),
                )
                .into());
            }
        };

        // Pre-mint the session id BEFORE building the blob so it
        // can be baked into the blob — the sandbox-resident vault
        // reads it back to correlate gen_ai spans with the session
        // span. Also reused by `run_attach`'s wrapper invocation
        // (single source of truth for the run's id).
        let session_id = Session::new_id();
        let mut blob = build_vault_stdin_blob(spec, &opts, resolved, "run --remote (e2b)")?;
        blob.context = crate::vault::RunContext {
            session_id: Some(session_id.clone()),
            mode: Some(crate::vault::RunContext::mode_for(opts.detach).to_string()),
            workspace_id: Some(resolved.workspace_id().to_string()),
        };

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
            &session_id,
            detach,
            json,
            expires_at,
        )
    }
}

/// Initial attach — create a new sandbox, launch the agent under an
/// in-sandbox `pty-host`, and either drive the terminal pump over a relay
/// (interactive) or exit after launch (`--detach`).
///
///   - **foreground**: run [`pump::attach_terminal`] over the helper's
///     stdio (`detach_enabled = false`). On `Outcome::Exited(0)` we're
///     done; a non-zero exit propagates as an error; `Disconnected`
///     surfaces the helper diagnostic. The helper's SIGTERM teardown
///     kills the (ephemeral) sandbox — no record is written.
///   - **`--detach`**: the helper launches the pty-host headless, emits
///     `detached`, and exits without killing. We persist the session so
///     the user can `pillbox session attach <id>` later.
#[allow(clippy::too_many_arguments)]
fn run_attach(
    resolved: &Pillbox,
    remote_name: &str,
    agent_id: &'static str,
    label: Option<String>,
    e2b: &E2bRef,
    blob: &VaultStdinBlob,
    session_id: &str,
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

    let helper = prepare_helper()?;
    eprintln!(
        "pillbox: connecting to `{remote_name}` (e2b://{}) …",
        e2b.template
    );
    if !detach {
        // Interactive attach — surface the detach hotkey so the user
        // can leave the session running without reading the docs.
        eprintln!("pillbox: detach with Ctrl-A D to keep the sandbox running.");
    }

    // `session_id` is minted by the caller (see the launch path's
    // pre-mint) so it can be baked into the blob before staging.
    // Borrow as an owned String for the legacy uses below that
    // expected a local binding.
    let session_id = session_id.to_string();

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
    // Forward the parent session id (`pillbox run --parent <id>`) so
    // the wrapper can `export PILLBOX_PARENT_SESSION_ID=…` for the
    // sandbox-side `pillbox session started` invocation to consume.
    // Host-side `persist_and_emit_started` reads the same env via the
    // shared `events::parent_session_id_from_env` helper, so we don't
    // re-thread the value through `PersistArgs`.
    if let Some(id) = crate::events::parent_session_id_from_env() {
        cmd.arg("--parent").arg(&id);
    }

    if detach {
        // `--detach`: the helper launches the pty-host headless and exits
        // after emitting `detached`. No interactive pump — record the
        // session so the user can `pillbox session attach <id>` later.
        let (status, pumped) = run_helper_launch(cmd, "run --remote (e2b)")?;
        drop(tmp);
        if !status.success() || pumped.last_event.as_deref() != Some("detached") {
            return Err(helper_launch_error(status, &pumped));
        }
        let session = persist_and_emit_started(
            PersistArgs {
                resolved,
                remote_name,
                agent_id,
                label,
                pre_minted_id: &session_id,
                expires_at: expires_at.clone(),
                base_snapshot: blob.workspace.base_snapshot.clone(),
            },
            &pumped,
        )?;
        if json {
            // Machine-readable: matches `pillbox session info --json` so
            // orchestrators can use the same parsing path.
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
        return Ok(());
    }

    // Foreground: run the shared terminal pump over the helper's stdio,
    // exactly like the docker / ssh backends. `detach_enabled = false` —
    // a foreground run has no session record to leave behind, so Ctrl-A
    // passes through to the agent and we tear the sandbox down on exit.
    let (outcome, status, pumped) = attach_via_helper(cmd, "run --remote (e2b)", false)?;
    drop(tmp);
    match outcome {
        Outcome::Exited(0) | Outcome::Detached => Ok(()),
        Outcome::Exited(code) => Err(PillboxError::runtime(
            "run --remote (e2b)",
            format!("{agent_id} exited with status {code}"),
        )
        .into()),
        // The pipe closed without an Exit frame. If we never saw the
        // `sandbox-up` handshake the sandbox never came up — surface the
        // prereqs as a hard error. If it DID come up, the transport dropped
        // mid-run; treat that as `Ok` to match the docker / ssh foreground
        // arms (same `Outcome::Disconnected` → same exit code across
        // backends). The helper's SIGTERM teardown killed the sandbox.
        Outcome::Disconnected if pumped.sandbox_id.is_none() => {
            Err(helper_launch_error(status, &pumped))
        }
        Outcome::Disconnected => Ok(()),
    }
}

/// Build the failure error for a helper that exited without the expected
/// happy path. Adds the prereq hint only when we never saw the
/// `sandbox-up` handshake (i.e. the sandbox never even came up).
fn helper_launch_error(status: std::process::ExitStatus, pumped: &PumpOutcome) -> anyhow::Error {
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
    err.into()
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
    let helper = prepare_helper()?;
    eprintln!(
        "pillbox: reattaching to `{}` (sandbox `{}`) …",
        remote.name, session.sandbox_id
    );
    // Surface the detach hotkey on every reattach — without this the
    // user has no way to discover it short of reading the docs.
    eprintln!("pillbox: detach with Ctrl-A D (the sandbox keeps running).");

    session::mark_attached(resolved, &session.id, std::process::id() as i64)?;

    // The helper re-derives the pty-host socket path from the session id
    // (`sockForSession`), so no PTY pid bookkeeping crosses the wire — it
    // connects a fresh relay to the still-running pty-host.
    let mut cmd = Command::new("node");
    cmd.arg(&helper)
        .arg("reattach")
        .arg("--sandbox-id")
        .arg(&session.sandbox_id)
        .arg("--session-id")
        .arg(&session.id);
    // `detach_enabled = true`: there's a persisted session to leave
    // running, so Ctrl-A D / SIGTERM resolve as a clean `Detached`.
    let pump_result = attach_via_helper(cmd, "session attach", true);

    // Always clear attached_pid before returning, even on error. The
    // session record is still valid (sandbox is up); only the "who's
    // attached" stamp changes.
    let _ = session::mark_detached(resolved, &session.id);

    let (outcome, _status, _pumped) = pump_result?;
    match outcome {
        // Ctrl-A D / SIGTERM, or the transport dropped while the sandbox
        // is still up — leave the record so the user can come back.
        Outcome::Detached | Outcome::Disconnected => {
            eprintln!(
                "pillbox: detached. reattach with `pillbox session attach {}`",
                session.id
            );
            Ok(())
        }
        // The process inside the PTY exited. The sandbox is still alive —
        // leave the record; `pillbox session rm <id>` tears it down.
        Outcome::Exited(code) => {
            eprintln!(
                "pillbox: agent exited ({code}). `pillbox session rm {}` to clean up.",
                session.id
            );
            Ok(())
        }
    }
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
    let helper = prepare_helper()?;
    let mut cmd = Command::new("node");
    cmd.arg(&helper)
        .arg("kill")
        .arg("--sandbox-id")
        .arg(&session.sandbox_id);
    // `kill` mode has no PTY and no user interaction. A failed kill is a
    // warning, not an error — we drop the local record regardless (see the
    // doc comment above), since the sandbox will time out on E2B's side.
    match run_helper_launch(cmd, "session rm") {
        Ok((status, _)) if !status.success() => {
            eprintln!("pillbox: warning: sandbox kill exited with status {status}");
        }
        Ok(_) => {}
        Err(e) => eprintln!("pillbox: warning: sandbox kill failed: {e}"),
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
    pub(super) base_snapshot: Option<String>,
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
            parent_session_id: crate::events::parent_session_id_from_env(),
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
        // Reattach derives the pty-host socket from the session id, so
        // there's no live PTY pid to record (kept 0 for the shared shape).
        pty_pid: 0,
        agent_id: args.agent_id.to_string(),
        started_at: session::now_rfc3339(),
        attached_pid,
        base_snapshot: args.base_snapshot,
        result_snapshot: None,
        expires_at: args.expires_at,
    };
    session::write(args.resolved, &session)?;
    Ok(session)
}

/// Spawn the helper with stdin/stdout PIPED and drive the shared
/// terminal pump ([`pump::attach_terminal`]) over them — the helper is a
/// dumb byte pipe to the in-sandbox `pty-relay`, so the host owns the
/// frame protocol, raw mode, resize, and Ctrl-A detach (identical to the
/// docker / ssh backends). stderr is pumped line-by-line on a background
/// thread for the JSON handshake. Returns the pump [`Outcome`], the
/// helper's exit status, and what we learned from stderr.
///
/// Teardown: once the pump resolves we SIGTERM the helper, which triggers
/// its mode-specific cleanup — a foreground `attach` kills the sandbox; a
/// `reattach` kills only its own relay PTY (the run owns the sandbox).
/// The SIGTERM also unblocks the pump's reader thread (still parked on the
/// helper's stdout) by making the helper close its pipes on exit.
fn attach_via_helper(
    mut cmd: Command,
    action: &'static str,
    detach_enabled: bool,
) -> Result<(Outcome, std::process::ExitStatus, PumpOutcome)> {
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let (mut child, stderr_thread) = spawn_with_stderr_pump(cmd, action)?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| PillboxError::runtime(action, "helper stdout unexpectedly closed"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| PillboxError::runtime(action, "helper stdin unexpectedly closed"))?;

    let outcome = pump::attach_terminal(stdout, stdin, detach_enabled)?;
    terminate_child(&child);
    let status = child
        .wait()
        .map_err(|e| PillboxError::runtime(action, format!("wait on helper: {e}")))?;
    let pumped = stderr_thread.join().unwrap_or_default();
    Ok((outcome, status, pumped))
}

/// Signal the helper to tear down. SIGTERM hits its `SIGTERM` handler
/// (mode-specific cleanup) rather than `child.kill()`'s SIGKILL, which
/// would leak the sandbox / relay. ESRCH (already exited) is fine.
#[cfg(unix)]
fn terminate_child(child: &Child) {
    // SAFETY: kill() with a valid pid + SIGTERM is async-safe and total;
    // a stale pid returns ESRCH, which we ignore.
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
}

#[cfg(not(unix))]
fn terminate_child(child: &Child) {
    // No graceful path off-unix; this backend is unix-only in practice
    // (the agent images are Linux), so a hard kill is acceptable here.
    let _ = child;
}

/// Run the helper for a non-interactive launch (`attach --detach`,
/// `kill`): no terminal pump, just collect the stderr handshake + exit
/// status. stdin is closed (the helper exits after its one-shot work) and
/// stdout is discarded — the helper writes nothing meaningful there, so
/// `null` both drops any E2B SDK noise AND avoids a full-pipe deadlock
/// (nothing on the host drains stdout during the bare `child.wait()`).
fn run_helper_launch(
    mut cmd: Command,
    action: &'static str,
) -> Result<(std::process::ExitStatus, PumpOutcome)> {
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::piped());
    let (mut child, stderr_thread) = spawn_with_stderr_pump(cmd, action)?;
    let status = child
        .wait()
        .map_err(|e| PillboxError::runtime(action, format!("wait on helper: {e}")))?;
    let pumped = stderr_thread.join().unwrap_or_default();
    Ok((status, pumped))
}

/// Spawn the helper and start draining its stderr (the JSON handshake) on
/// a background thread. Shared prologue for both the interactive pump path
/// ([`attach_via_helper`]) and the one-shot launch path
/// ([`run_helper_launch`]); the caller drives stdout/stdin (or not) and
/// joins the returned thread after `child.wait()`.
fn spawn_with_stderr_pump(
    mut cmd: Command,
    action: &'static str,
) -> Result<(Child, std::thread::JoinHandle<PumpOutcome>)> {
    let mut child: Child = cmd.spawn().map_err(|e| {
        PillboxError::resource(action, format!("could not spawn node: {e}"))
            .with_next("install Node.js + run `npm i -g e2b` (https://e2b.dev/docs)")
    })?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| PillboxError::runtime(action, "helper stderr unexpectedly closed"))?;
    let stderr_thread = std::thread::spawn(move || pump_helper_stderr(stderr));
    Ok((child, stderr_thread))
}

/// What `pump_helper_stderr` learned from the helper's stderr stream.
/// The Rust side uses these to (a) write the session record after a
/// successful handshake and (b) confirm the `--detach` launch reached
/// `detached` via `last_event`.
#[derive(Debug, Default)]
struct PumpOutcome {
    /// Sandbox id from the `sandbox-up` handshake (if seen).
    sandbox_id: Option<String>,
    /// `type` of the last JSON event the helper wrote — `sandbox-up` or
    /// `detached`. The reattach socket is derived from the session id, so
    /// no PTY pid crosses the handshake anymore.
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
    // Content-address the cache file: the name carries a hash of the
    // embedded script, so ANY edit to e2b-helper.mjs lands a new file
    // and is picked up immediately. (Keying on CARGO_PKG_VERSION alone
    // masked helper edits during a release cycle — the version rarely
    // bumps mid-dev, and we only write when the file is absent.) Old
    // hashes linger until a future cache cleanup; that's by design.
    let path = dir.join(format!(
        "e2b-helper-v{}-{:016x}.mjs",
        env!("CARGO_PKG_VERSION"),
        fnv1a(HELPER_SCRIPT.as_bytes())
    ));
    if !path.exists() {
        std::fs::write(&path, HELPER_SCRIPT.as_bytes())
            .with_context(|| format!("write {}", path.display()))?;
    }
    Ok(path)
}

/// FNV-1a over `bytes` — a small, stable (build-independent) hash for
/// content-addressing the cached helper file. Not cryptographic; just
/// needs to change whenever the embedded script changes.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Extract the helper *and* make its `e2b` import resolvable — the full
/// prep the run paths need before spawning node. Kept separate from
/// [`ensure_helper_extracted`] (which only writes the script) so unit
/// tests can exercise extraction without requiring the `e2b` SDK to be
/// installed on the test machine.
fn prepare_helper() -> Result<PathBuf> {
    let path = ensure_helper_extracted()?;
    let cache_dir = path.parent().expect("helper path always has a parent dir");
    ensure_e2b_module_linked(cache_dir)?;
    Ok(path)
}

/// Make the helper's bare `import { Sandbox } from "e2b"` resolve.
///
/// The helper runs from `~/.pillbox/cache/`, where node's ESM resolver
/// finds no `node_modules` — and the SDK is installed *globally* (`npm i
/// -g e2b`), which ESM resolution ignores (NODE_PATH only affects CJS).
/// So symlink the global package into the cache's `node_modules`; node
/// then resolves the bare specifier the normal way. Without this the
/// helper dies with `ERR_MODULE_NOT_FOUND` before it can do anything.
fn ensure_e2b_module_linked(cache_dir: &Path) -> Result<()> {
    let out = Command::new("npm")
        .args(["root", "-g"])
        .output()
        .context("running `npm root -g` — is Node/npm installed?")?;
    if !out.status.success() {
        return Err(PillboxError::resource(
            "run --remote (e2b)",
            "couldn't locate the global npm modules directory",
        )
        .with_next("install Node.js, then `npm i -g e2b`")
        .into());
    }
    let global_e2b = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()).join("e2b");
    if !global_e2b.exists() {
        return Err(PillboxError::resource(
            "run --remote (e2b)",
            "the `e2b` SDK isn't installed globally",
        )
        .with_next("npm i -g e2b")
        .into());
    }
    let node_modules = cache_dir.join("node_modules");
    std::fs::create_dir_all(&node_modules)
        .with_context(|| format!("create {}", node_modules.display()))?;
    let link = node_modules.join("e2b");
    // Idempotent: leave it if it already points at the current global SDK.
    if link.read_link().is_ok_and(|t| t == global_e2b) {
        return Ok(());
    }
    let _ = std::fs::remove_file(&link); // clear a stale link/file, if any
    std::os::unix::fs::symlink(&global_e2b, &link)
        .with_context(|| format!("symlink {} -> {}", link.display(), global_e2b.display()))?;
    Ok(())
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
/// handshake; an `attach --detach` launch then emits `detached`.
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
                    outcome.last_event = Some("sandbox-up".into());
                }
                Some("detached") => {
                    outcome.last_event = Some("detached".into());
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
        // The reshaped helper carries the frame transport: it launches the
        // in-sandbox pty-host and shuttles bytes to/from a pty-relay.
        assert!(HELPER_SCRIPT.contains("pillbox pty-host"));
        assert!(HELPER_SCRIPT.contains("pillbox pty-relay"));
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

    // Needs the `e2b` SDK installed globally (`npm i -g e2b`), so it's not
    // part of the default suite. Run: `cargo test --bin pillbox -- --ignored
    // links_e2b_module`. Proves the symlink that makes the helper's
    // `import "e2b"` resolve actually lands and points at the global SDK.
    #[test]
    #[ignore = "requires `npm i -g e2b`"]
    fn links_e2b_module_into_cache() {
        with_isolated_home("e2b-module-link", || {
            let path = prepare_helper().unwrap();
            let link = path.parent().unwrap().join("node_modules").join("e2b");
            let target = link.read_link().expect("e2b should be symlinked");
            assert!(
                target.ends_with("e2b"),
                "symlink should point at the e2b package"
            );
            assert!(
                target.join("package.json").exists(),
                "target must be the real SDK"
            );
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
