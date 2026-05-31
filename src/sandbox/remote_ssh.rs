//! Remote-SSH sandbox backend — `pillbox run --remote NAME`.
//!
//! Pillbox doesn't deploy itself; the user has already installed pillbox
//! on the VPS (`brew install pillbox` / `cargo install pillbox`) at
//! [`REMOTE_PILLBOX`]. This backend teaches the local pillbox how to
//! (a) ship one run-worth of state over the wire and (b) drive an
//! interactive, reattachable agent session on the remote.
//!
//! ## Transport (phase 4 — the attach pty-host model)
//!
//! Mirrors `local_docker` over ssh instead of docker. The remote runs a
//! persistent `pillbox pty-host` that owns the agent's PTY + screen
//! model; the local terminal attaches over an ssh-exec'd `pillbox
//! pty-relay` and the shared [`crate::attach::pump`]. Because the frame
//! protocol + pump are shared across backends, ssh `--detach` /
//! `session attach` / `session rm` fall out of the same primitives the
//! docker + e2b backends use.
//!
//! 1. Parse `--remote NAME`, look up the [`Remote`] from the resolved
//!    pillbox's `remotes/` registry (with global fallback).
//! 2. Resolve `--with` entries + the workspace base into a
//!    [`VaultStdinBlob`] (schema doc-commented below).
//! 3. Refuse if the pillbox's workspace backend is `local` — remote
//!    runs require an S3/R2-shaped rustic repo (the remote rustic_core
//!    needs the same bucket/endpoint).
//! 4. Stage the secret-bearing blob to a remote 0600 temp file over a
//!    non-PTY ssh exec ([`stage_blob`]) — it can't ride the relay's
//!    keystroke channel.
//! 5. Launch the remote pty-host detached so it outlives the launch ssh
//!    session ([`launch_pty_host`]): `setsid pillbox pty-host --sock S --
//!    bash -lc <wrapper>`, where `<wrapper>` brackets `pillbox run
//!    --vault-stdin --blob-file B` with `session started` / `session
//!    done`.
//! 6. Interactive: attach the pump over `pillbox pty-relay` and tear the
//!    host down on exit. `--detach`: persist a [`Session`] and return.
//!
//! ## Flow (remote side)
//!
//! The pty-host's child wraps `pillbox run --vault-stdin` — the **same**
//! remote entrypoint the e2b backend uses: it reads the blob, provisions
//! a vault session, restores the base snapshot into an isolated temp
//! workspace, runs the agent under LocalDocker, and pushes the result
//! workspace back. We reuse this verbatim — vault + workspace parity
//! comes for free, no ssh-specific re-implementation. Two ssh-specific
//! wrinkles:
//!   - `--blob-file` (vs e2b's `< blob`): the inner `docker run -it`
//!     needs a TTY on stdin, so the blob is read from a file and the
//!     child's stdin stays the pty-host's PTY.
//!   - The child is a `bash -lc` wrapper ([`build_wrapper`]) that mirrors
//!     e2b's sandbox-side wrapper — `session started` before the run,
//!     `session done` (with the captured exit code + result snapshot)
//!     after — so detached ssh runs emit the same terminal events +
//!     result-snapshot handle e2b does. Without it, `session pull` and
//!     the `session.completed`/`failed` webhook had no data to work from.
//!
//! ## Vault-stdin blob schema (internal — `pillbox run --vault-stdin`)
//!
//! ```jsonc
//! {
//!   "version": 2,
//!   "agent_id": "claude",
//!   "agent_args": ["--continue"],          // forwarded to the agent CLI
//!   "workspace_mount_name": "my-app",      // /workspace/<name> on the remote
//!   "vault": true,                          // route through pillbox vault proxy?
//!   "workspace": {
//!     "s3": {                                 // RusticVariant::S3 config, verbatim
//!       "endpoint": "https://acct.r2.cloudflarestorage.com",
//!       "region": "auto",
//!       "bucket": "my-bucket",
//!       "prefix": "pillbox/",
//!       "access_key": "<resolved value>",
//!       "secret_key": "<resolved value>"
//!     },
//!     "repo_password": "<rustic repo password>",
//!     "base_snapshot": "<64-char handle>"
//!   },
//!   "secrets": [
//!     { "name": "ANTHROPIC_API_KEY",
//!       "env_var": "ANTHROPIC_API_KEY",
//!       "value": "<real plaintext>",
//!       "vault_meta": null }                // null = plain; object = stub-swap
//!   ],
//!   "env": { "KEY": "value", ... }          // pre-merged --env / --env-file
//! }
//! ```
//!
//! Forward-compat: unknown JSON keys are ignored by serde so a newer
//! local pillbox can add fields without breaking older remotes. The
//! `version` integer, by contrast, IS enforced — a mismatch fails the
//! parse loudly so a semantic-breaking schema change can't silently
//! drop required state. Bump `BLOB_VERSION` only for breaking changes.
//! The blob is secret-bearing. SSH streams it over stdin; E2B stages it
//! through 0600 temp files because the PTY channel carries user input.

use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result};

use super::SandboxBackend;
use crate::agents::{base_docker_args, AgentSpec, RunOpts, GUEST_HOME, GUEST_WORKSPACE};
use crate::attach::pump::{self, Outcome};
use crate::config::BackendKind;
use crate::docker;
use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::remote::{parse_ssh_url, Remote, SshUrl};
use crate::sandbox::vault_stdin::{
    build_vault_stdin_blob, AuthFile, InlineWorkspace, VaultStdinBlob, WorkspaceProvision,
    RESULT_SNAPSHOT_FILE_ENV,
};
use crate::session::{self, Session, BACKEND_SSH};
use crate::vault::{OAuthAgent, VaultSession};
use crate::workspace::rustic::{RusticBackend, RusticVariant, PASSWORD_FILE};
use crate::workspace::{PushOptions, SnapshotHandle, WorkspaceBackend};

/// Absolute path to the remote `pillbox` binary. The user installs it
/// separately (package / `cargo install`); we don't deploy it. Hard-coded
/// rather than relying on the remote shell's `$PATH` because the launch
/// runs through `ssh <dest> <command>` (a non-login shell on many hosts,
/// where `/usr/local/bin` may be absent from `$PATH`).
const REMOTE_PILLBOX: &str = "/usr/local/bin/pillbox";

/// `pillbox run --remote NAME` backend. Owns one [`Remote`] for the
/// duration of the run; the openssh `Session` lives inside `run` (it
/// holds a tempdir for the control socket, so we let `Drop` clean it
/// up at the end of the call).
pub(crate) struct RemoteSshSandbox {
    remote: Remote,
}

impl RemoteSshSandbox {
    pub(crate) fn new(remote: Remote) -> Self {
        Self { remote }
    }
}

impl SandboxBackend for RemoteSshSandbox {
    fn run(&self, spec: &AgentSpec, opts: RunOpts, resolved: &Pillbox) -> Result<()> {
        // Workspace handoff rule: S3-shaped backends only. The remote
        // runs its own pillbox against the SAME `[workspace]` config —
        // bucket, endpoint, prefix all match, so no data has to cross
        // SSH. Local-rustic-via-tarball is the planned follow-up.
        let meta = resolved.meta.as_ref().ok_or_else(|| {
            PillboxError::usage(
                "run --remote",
                "the global pillbox can't run remotely; cd into a project pillbox first",
            )
        })?;
        if meta.workspace.backend_kind() != BackendKind::S3 {
            return Err(PillboxError::usage(
                "run --remote",
                "remote runs require an S3-shaped workspace backend \
                 (the remote rustic_core needs the same bucket/endpoint). \
                 Local-rustic via tarball transport is the planned follow-up.",
            )
            .with_next(
                "pillbox new --workspace-backend s3 …  # or use a project that already has one",
            )
            .into());
        }

        // Pre-mint the session id so the per-session remote sock / blob /
        // log paths are unique, the `--detach` record references a
        // stable handle, AND the vault correlates its gen_ai spans
        // with the session span via the blob (see VaultStdinBlob.
        // session_id). Minted before the blob is built so it can be
        // baked in.
        let session_id = Session::new_id();
        let remote = RemoteSession::new(&session_id);

        let mut blob = build_vault_stdin_blob(
            spec,
            &opts,
            resolved,
            "run --remote",
            WorkspaceProvision::S3,
        )?;
        blob.context = crate::vault::RunContext {
            session_id: Some(session_id.clone()),
            mode: Some(crate::vault::RunContext::mode_for(opts.detach).to_string()),
            workspace_id: Some(resolved.workspace_id().to_string()),
        };

        // Sanity-check the URL once more before connecting — the registry
        // validates on add, but a hand-edited file could slip through.
        let url = parse_ssh_url(&self.remote.url).map_err(|e| {
            PillboxError::config(
                "run --remote",
                format!("remote `{}`: {e}", self.remote.name),
            )
        })?;

        // User-facing reassurance: SSH dial-up can stall for tens of
        // seconds on a cold connection, and the local terminal otherwise
        // sits silent until the remote sandbox prints. eprintln so we
        // don't taint stdout for `--json`-style downstream consumers.
        eprintln!(
            "pillbox: connecting to `{}` ({}) …",
            self.remote.name, self.remote.url
        );

        // Stage the blob to a remote 0600 temp file, then launch the
        // pty-host as a persistent background process running
        // `pillbox run --vault-stdin --blob-file <blob>` under the PTY.
        // The blob can't ride the relay pipe (that carries user
        // keystrokes) so it goes over a separate, non-PTY ssh exec —
        // mirroring the e2b backend's "file, not stdin" reasoning.
        stage_blob(&url, &remote, &blob)?;
        launch_pty_host(&url, &remote, spec, &session_id)?;

        if opts.detach {
            let s = Session {
                id: session_id.clone(),
                label: opts.label.clone(),
                remote: self.remote.name.clone(),
                backend: BACKEND_SSH.to_string(),
                sandbox_id: remote.sock.clone(),
                pty_pid: 0,
                agent_id: spec.id.to_string(),
                started_at: session::now_rfc3339(),
                attached_pid: None,
                base_snapshot: blob
                    .workspace
                    .as_ref()
                    .and_then(|w| w.base_snapshot.clone()),
                result_snapshot: None,
                expires_at: opts.ttl_seconds.map(session::expires_at_from_ttl),
                // Empty: a remote session's transcript is sandbox-side, so the
                // host gateway can't tail it (live tailing is local-docker only).
                guest_cwd: String::new(),
            };
            session::write(resolved, &s)?;
            crate::events::emit_session_event(
                resolved,
                crate::events::EventType::SessionStarted {
                    parent_session_id: crate::events::parent_session_id_from_env(),
                },
                &s.id,
                Some(&s),
            );
            if opts.json {
                println!(
                    "{}",
                    crate::paths::json_v1(vec![("session", s.to_json_value())])
                );
            } else {
                println!(
                    "pillbox: ✓ session `{}` started in background on `{}`.",
                    s.id, self.remote.name
                );
                println!("         pillbox session attach {}  # reattach", s.id);
            }
            return Ok(());
        }

        // Interactive: attach the terminal pump over an ssh relay exec,
        // then tear the remote host down regardless of how it ended
        // (mirrors `local_docker::run`'s foreground path). Detach is OFF for
        // a foreground run — there's no persisted session to reattach to, so
        // we must NOT advertise or honor Ctrl-A D here (it would silently
        // destroy the run); Ctrl-A passes through to the agent instead.
        let outcome = attach_via_ssh(&url, &remote, false);
        let _ = kill_pty_host(&url, &remote);

        match outcome? {
            Outcome::Exited(0) | Outcome::Detached | Outcome::Disconnected => Ok(()),
            Outcome::Exited(code) => Err(PillboxError::runtime(
                "run --remote",
                format!("{} exited with status {code}", spec.id),
            )
            .into()),
        }
    }
}

/// Per-session remote paths. The sock is the addressable handle stored in
/// the `Session` record (`sandbox_id`); blob + log + result are derived
/// from the same id so `kill_session` can scrub them all without
/// persisting four fields. Kept private and constructed only from a
/// validated session id (12 ascii-hex chars), so the paths can never
/// carry shell metacharacters. `result` is the file the wrapper points
/// `PILLBOX_RESULT_SNAPSHOT_FILE` at (the result-snapshot round-trip).
struct RemoteSession {
    sock: String,
    blob: String,
    log: String,
    result: String,
}

impl RemoteSession {
    fn new(session_id: &str) -> Self {
        Self {
            sock: format!("/tmp/pillbox-attach-{session_id}.sock"),
            blob: format!("/tmp/pillbox-blob-{session_id}.json"),
            log: format!("/tmp/pillbox-host-{session_id}.log"),
            result: format!("/tmp/pillbox-result-{session_id}.txt"),
        }
    }

    /// Reconstruct from a stored `Session.sandbox_id` (the sock path).
    /// The blob + result are already consumed + unlinked by the wrapper,
    /// so only the sock + log matter for reattach / kill; derive them
    /// from the id embedded in the sock path. Falls back to sock-derived
    /// names if the shape is unexpected (hand-edited record).
    fn from_sock(sock: &str) -> Self {
        let id = sock
            .strip_prefix("/tmp/pillbox-attach-")
            .and_then(|s| s.strip_suffix(".sock"));
        let (blob, log, result) = match id {
            Some(id) => (
                format!("/tmp/pillbox-blob-{id}.json"),
                format!("/tmp/pillbox-host-{id}.log"),
                format!("/tmp/pillbox-result-{id}.txt"),
            ),
            None => (
                format!("{sock}.blob"),
                format!("{sock}.log"),
                format!("{sock}.result"),
            ),
        };
        Self {
            sock: sock.to_string(),
            blob,
            log,
            result,
        }
    }
}

/// Common base for every short-lived ssh exec: keep the connection from
/// being culled mid-run by aggressive NATs / firewalls, and disable
/// pseudo-terminal allocation (`-T`) so the relay/stage pipes stay
/// binary-clean. The destination + remote command are appended by the
/// caller.
fn ssh_base(url: &SshUrl) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.arg("-T").arg("-o").arg("ServerAliveInterval=30");
    match url.port {
        // A `user@host:port` positional destination isn't portable across
        // openssh versions, so pass the port via `-p` and the bare
        // `user@host` (which `destination()` returns when port is None)
        // as the destination.
        Some(port) => {
            cmd.arg("-p")
                .arg(port.to_string())
                .arg(format!("{}@{}", url.user, url.host));
        }
        None => {
            cmd.arg(url.destination());
        }
    }
    cmd
}

/// Stage the secret-bearing blob into a 0600 file on the remote, piped
/// over a non-PTY ssh exec (so it never crosses the relay's keystroke
/// channel). `umask 077` makes the redirect-created file private before
/// any bytes land.
fn stage_blob(url: &SshUrl, remote: &RemoteSession, blob: &VaultStdinBlob) -> Result<()> {
    let bytes = blob.to_bytes()?;
    let mut cmd = ssh_base(url);
    // `cat > file` with a leading `umask 077` keeps the staged blob
    // private. The path is built from a validated session id so it
    // carries no shell metacharacters; still single-quote it defensively.
    cmd.arg(format!("umask 077; cat > '{}'", remote.blob));
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::inherit());

    let mut child = cmd.spawn().map_err(|e| {
        PillboxError::resource("run --remote", format!("could not spawn ssh: {e}"))
            .with_next("ensure the openssh client is on PATH (`which ssh`)")
    })?;
    {
        let stdin = child.stdin.as_mut().ok_or_else(|| {
            PillboxError::runtime("run --remote", "ssh child stdin unexpectedly closed")
        })?;
        stdin
            .write_all(&bytes)
            .map_err(|e| PillboxError::runtime("run --remote", format!("stage blob: {e}")))?;
    }
    drop(child.stdin.take());
    let status = child
        .wait()
        .map_err(|e| PillboxError::runtime("run --remote", format!("wait on ssh: {e}")))?;
    if !status.success() {
        return Err(PillboxError::runtime(
            "run --remote",
            format!("staging the blob over ssh failed (status {status})"),
        )
        .into());
    }
    Ok(())
}

/// POSIX single-quote escape for one value spliced into a `sh -c` /
/// `bash -lc` command string. Mirrors `shellEscape` in `e2b-helper.mjs`
/// (and lum's `e2b-provider.mjs`): close the single quote, emit a
/// literal `'` as `"'"`, reopen. The result is a single token that any
/// POSIX shell unquotes back to `value` verbatim — no metacharacter can
/// escape it.
///
/// Used at BOTH shell boundaries of the ssh launch (see
/// [`launch_pty_host`]): once around values inside the inner wrapper that
/// `bash -lc` evaluates, and once more around the whole wrapper for the
/// outer login shell that `ssh dest '<line>'` runs it under. Applying it
/// consistently at each level is what keeps the nested quoting correct.
fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// Build the inner wrapper — the script `bash -lc` evaluates on the
/// remote (a SINGLE shell level). Every interpolated value is
/// [`sh_quote`]d once for THIS level. Mirrors the e2b helper's
/// `buildWrapper`: both are `exec`'d as the pty-host's child argv (the
/// pty-host owns the PTY + raw mode + frame protocol, so the wrapper
/// itself never touches `stty` or markers). The one wire-protocol
/// difference from e2b is `--blob-file` (vs e2b's `< blob`): it keeps the
/// child's stdin the PTY so the inner `docker run -it` gets a TTY.
///
/// `webhook` / `parent` are the host-env values (already filtered to
/// non-empty); `None` drops the corresponding `export`.
/// The OTLP env vars the events/otel layer reads, in spec-canonical
/// form. Forwarded verbatim into the sandbox so the sandbox-side tailer
/// streams to the same collector the host would. Only set + non-empty
/// vars are carried (an unset var stays unset in the sandbox, so
/// `otlp_traces_configured()` there matches the host's intent).
const FORWARDED_OTEL_VARS: &[&str] = &[
    "OTEL_EXPORTER_OTLP_ENDPOINT",
    "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
    "OTEL_EXPORTER_OTLP_HEADERS",
    "OTEL_EXPORTER_OTLP_TIMEOUT",
    "OTEL_SERVICE_NAME",
];

/// Read [`FORWARDED_OTEL_VARS`] off the host env. Kept out of
/// [`build_wrapper`]/[`build_launch_line`] so those stay pure +
/// unit-testable (mirrors how the webhook/parent are read at the call
/// site).
fn forwarded_otel_env() -> Vec<(String, String)> {
    FORWARDED_OTEL_VARS
        .iter()
        .filter_map(|k| {
            std::env::var(k)
                .ok()
                .filter(|v| !v.is_empty())
                .map(|v| (k.to_string(), v))
        })
        .collect()
}

fn build_wrapper(
    remote: &RemoteSession,
    session_id: &str,
    webhook: Option<&str>,
    parent: Option<&str>,
    otel: &[(String, String)],
) -> String {
    let id_q = sh_quote(session_id);
    let blob_q = sh_quote(&remote.blob);
    let result_q = sh_quote(&remote.result);

    let webhook_export = match webhook {
        Some(u) => format!("export PILLBOX_EVENTS_WEBHOOK={}; ", sh_quote(u)),
        None => String::new(),
    };
    let parent_export = match parent {
        Some(id) => format!("export PILLBOX_PARENT_SESSION_ID={}; ", sh_quote(id)),
        None => String::new(),
    };
    // Forward the operator's OTLP config so the sandbox-side tailer emits
    // spans straight to their collector. Reachability of that collector
    // from the sandbox is the operator's responsibility (it's a separate
    // product) — we just carry the env across.
    let otel_exports: String = otel
        .iter()
        .map(|(k, v)| format!("export {k}={}; ", sh_quote(v)))
        .collect();

    format!(
        "export PILLBOX_SANDBOX_SIDE=1; \
         export PILLBOX_SESSION_STARTED_AT=\"$(date -u -Iseconds 2>/dev/null)\"; \
         {webhook_export}{parent_export}{otel_exports}\
         export PILLBOX_RESULT_SNAPSHOT_FILE={result_q}; \
         rm -f {result_q}; \
         {pillbox} session started {id_q}; \
         {pillbox} run --vault-stdin --blob-file {blob_q}; \
         PB_EXIT=$?; \
         RESULT_SNAPSHOT=$(cat {result_q} 2>/dev/null || true); \
         {pillbox} session done {id_q} \
         --status \"$([ $PB_EXIT = 0 ] && echo ok || echo failed)\" \
         --exit-code \"$PB_EXIT\" \
         --reason \"$([ $PB_EXIT = 0 ] && echo agent-completed || echo \"agent exited $PB_EXIT\")\" \
         $([ -n \"$RESULT_SNAPSHOT\" ] && echo --result-snapshot \"$RESULT_SNAPSHOT\"); \
         rm -f {blob_q} {result_q}",
        pillbox = REMOTE_PILLBOX,
    )
}

/// Build the full `setsid … pty-host … -- bash -lc <wrapper> … &` launch
/// line for [`launch_pty_host`]. Pure (no I/O, no env reads) so the
/// nested quoting is unit-testable without an ssh round-trip — the
/// reviewer caveat is that this round-trip can't run in the sandbox, so
/// the quoting correctness is locked in by tests here instead.
///
/// The outer launch line is evaluated by the remote login shell that
/// `ssh dest '<line>'` runs (ONE shell level). The child argv is `bash
/// -lc <wrapper>`; the whole [`build_wrapper`] result is `sh_quote`d once
/// MORE for this level — consistent escaping at each boundary, no
/// hand-nested quotes. Sock + log are `sh_quote`d the same way. `setsid`
/// + `&` background the host so it outlives the launch ssh session.
fn build_launch_line(
    remote: &RemoteSession,
    session_id: &str,
    webhook: Option<&str>,
    parent: Option<&str>,
    otel: &[(String, String)],
) -> String {
    let wrapper = build_wrapper(remote, session_id, webhook, parent, otel);
    format!(
        "setsid {pillbox} pty-host --sock {sock} -- \
         bash -lc {wrapper} \
         </dev/null >{log} 2>&1 &",
        pillbox = REMOTE_PILLBOX,
        sock = sh_quote(&remote.sock),
        wrapper = sh_quote(&wrapper),
        log = sh_quote(&remote.log),
    )
}

/// Launch the in-remote pty-host as a persistent background process that
/// outlives the launch ssh session. `setsid` detaches it from the ssh
/// controlling terminal so closing the launch connection (or detaching)
/// doesn't SIGHUP the host; output goes to a per-session log for
/// post-mortem.
///
/// The pty-host's child is a `bash -lc` wrapper around `pillbox run
/// --vault-stdin --blob-file <blob>` (NOT the bare run): it mirrors the
/// e2b sandbox-side wrapper so detached ssh sessions get the SAME
/// result-snapshot capture + terminal-event wiring e2b has. The wrapper
///   1. exports `PILLBOX_SESSION_STARTED_AT` (one `date` read shared by
///      both `session started` and `session done`, no skew),
///   2. forwards `PILLBOX_EVENTS_WEBHOOK` + `PILLBOX_PARENT_SESSION_ID`
///      off the host env (so terminal events reach the orchestrator),
///   3. sets `PILLBOX_RESULT_SNAPSHOT_FILE` so `dispatch_vault_stdin`
///      writes the result handle there,
///   4. `pillbox session started <id>`,
///   5. runs `pillbox run --vault-stdin --blob-file <blob>` (`--blob-file`
///      keeps the child's stdin the PTY so the inner `docker run -it`
///      gets a TTY — the one wire-protocol difference from e2b's `< blob`),
///   6. captures `$?` + the result handle, then
///   7. `pillbox session done <id> --status … --exit-code … --reason …
///      [--result-snapshot …]` so the `session.completed`/`failed`
///      terminal event fires for every detached run.
///
/// `pillbox run --vault-stdin` still owns the full remote flow (workspace
/// hydrate, vault session, `docker run -it`, result push); the wrapper
/// only adds the started/done bookends.
///
/// ## Quoting (two shell levels — the trap)
///
/// The whole `setsid … &` line is evaluated by ONE outer login shell
/// (`ssh dest '<line>'`). Inside it, `bash -lc <wrapper>` evaluates the
/// wrapper as a SECOND shell level. So every interpolated value is
/// [`sh_quote`]d once for the inner wrapper, and the entire wrapper is
/// `sh_quote`d once more for the outer shell — consistent escaping at
/// each boundary, no hand-nested quotes. The session id is validated hex
/// and the sock/blob/log paths derive from it; only the webhook URL +
/// parent id are user-influenced, and they're escaped like everything
/// else.
fn launch_pty_host(
    url: &SshUrl,
    remote: &RemoteSession,
    spec: &AgentSpec,
    session_id: &str,
) -> Result<()> {
    // Forward the events webhook + parent session id off the HOST env —
    // the same source the e2b backend reads (the `--events-webhook` flag
    // / `--parent` flag both `set_var` into this process env in main.rs
    // before the backend runs). Empty / unset → the export is omitted and
    // the corresponding sink is simply absent. Reading them here keeps
    // [`build_launch_line`] a pure, unit-testable function.
    let webhook = std::env::var("PILLBOX_EVENTS_WEBHOOK")
        .ok()
        .filter(|u| !u.is_empty());
    let parent = crate::events::parent_session_id_from_env();
    let otel = forwarded_otel_env();
    let launch = build_launch_line(
        remote,
        session_id,
        webhook.as_deref(),
        parent.as_deref(),
        &otel,
    );

    let mut cmd = ssh_base(url);
    cmd.arg(launch);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::inherit());
    let status = cmd
        .status()
        .map_err(|e| PillboxError::resource("run --remote", format!("could not spawn ssh: {e}")))?;
    if !status.success() {
        return Err(PillboxError::runtime(
            "run --remote",
            format!(
                "launching the remote pty-host failed (status {status}); \
                 is `{REMOTE_PILLBOX}` installed on the remote?"
            ),
        )
        .with_next("install pillbox on the remote host (package / `cargo install`)")
        .into());
    }
    let _ = spec; // spec drives the blob's agent_id; argv is fixed here.
    Ok(())
}

/// Attach the terminal pump to a running remote pty-host by execing the
/// per-attach relay over ssh and pumping its stdio. Direct analogue of
/// `local_docker::attach_via_exec` — the only difference is the transport
/// (ssh exec vs docker exec). NO `-t`: the relay speaks binary frames, so
/// the pipe must stay byte-clean.
fn attach_via_ssh(url: &SshUrl, remote: &RemoteSession, detach_enabled: bool) -> Result<Outcome> {
    let mut cmd = ssh_base(url);
    cmd.arg(format!(
        "{REMOTE_PILLBOX} pty-relay --sock '{}'",
        remote.sock
    ));
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::inherit());
    let mut child = cmd.spawn().map_err(|e| {
        PillboxError::resource("run --remote", format!("could not spawn ssh: {e}"))
            .with_next("ensure the openssh client is on PATH (`which ssh`)")
    })?;
    let stdout = child
        .stdout
        .take()
        .context("ssh relay stdout unexpectedly closed")?;
    let stdin = child
        .stdin
        .take()
        .context("ssh relay stdin unexpectedly closed")?;
    let outcome = pump::attach_terminal(stdout, stdin, detach_enabled)?;
    // Don't leave the relay ssh exec lingering once the pump returns.
    let _ = child.kill();
    let _ = child.wait();
    Ok(outcome)
}

/// Kill the remote pty-host process and scrub its per-session files.
/// Best-effort: `pkill` matches the unique sock path in the host's argv;
/// the `rm -f` cleans the sock + log + blob + result (the blob + result
/// are normally unlinked by the wrapper, but remove them too in case the
/// run was killed before the wrapper's own cleanup ran).
fn kill_pty_host(url: &SshUrl, remote: &RemoteSession) -> Result<()> {
    let cmd_line = format!(
        "pkill -f 'pty-host --sock {sock}' 2>/dev/null; rm -f {sock_q} {blob_q} {log_q} {result_q}",
        sock = remote.sock,
        sock_q = sh_quote(&remote.sock),
        blob_q = sh_quote(&remote.blob),
        log_q = sh_quote(&remote.log),
        result_q = sh_quote(&remote.result),
    );
    let mut cmd = ssh_base(url);
    cmd.arg(cmd_line);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::inherit());
    let status = cmd
        .status()
        .map_err(|e| PillboxError::resource("session rm", format!("could not spawn ssh: {e}")))?;
    if !status.success() {
        // The trailing `rm -f` succeeds even when `pkill` matched nothing, so
        // a non-zero status here is the ssh exec itself failing (host
        // unreachable / auth) — the teardown did NOT run. Surface it so the
        // caller warns instead of silently assuming the host was reaped.
        return Err(PillboxError::runtime(
            "session rm",
            format!("remote teardown over ssh exited with status {status} (host unreachable?)"),
        )
        .into());
    }
    Ok(())
}

/// `pillbox session attach <id>` for an ssh session: re-open the relay to
/// the still-running remote pty-host and pump. Mirrors
/// `local_docker::reattach` and `remote_e2b::reattach`.
pub(crate) fn reattach(resolved: &Pillbox, remote: &Remote, session: &Session) -> Result<()> {
    if session::Backend::parse(&session.backend) != Some(session::Backend::Ssh) {
        return Err(PillboxError::usage(
            "session attach",
            format!(
                "session `{}` is backed by `{}`, not ssh",
                session.id, session.backend
            ),
        )
        .into());
    }
    let url = parse_ssh_url(&remote.url).map_err(|e| {
        PillboxError::config("session attach", format!("remote `{}`: {e}", remote.name))
    })?;
    let rs = RemoteSession::from_sock(&session.sandbox_id);

    eprintln!(
        "pillbox: reattaching to session `{}` on `{}` …",
        session.id, remote.name
    );
    eprintln!("pillbox: detach with Ctrl-A D (the session keeps running).");

    session::mark_attached(resolved, &session.id, std::process::id() as i64)?;
    let outcome = attach_via_ssh(&url, &rs, true);
    let _ = session::mark_detached(resolved, &session.id);

    match outcome? {
        Outcome::Detached => {
            eprintln!(
                "pillbox: detached. reattach with `pillbox session attach {}`",
                session.id
            );
            Ok(())
        }
        Outcome::Exited(code) => {
            eprintln!(
                "pillbox: agent exited ({code}). `pillbox session rm {}` to clean up.",
                session.id
            );
            Ok(())
        }
        Outcome::Disconnected => {
            eprintln!("pillbox: session connection closed.");
            Ok(())
        }
    }
}

/// `pillbox session rm <id>` for an ssh session: kill the remote pty-host
/// and scrub its files, then drop the local record unconditionally (a
/// failed kill shouldn't strand the record; the host may already be gone).
/// Mirrors `local_docker::kill_session`. `remote` is `None` when it has been
/// deregistered — we can't reach the host to kill the pty-host, but we still
/// drop the local record (never strand it).
pub(crate) fn kill_session(
    resolved: &Pillbox,
    remote: Option<&Remote>,
    session: &Session,
) -> Result<()> {
    if session::Backend::parse(&session.backend) != Some(session::Backend::Ssh) {
        return Err(PillboxError::usage(
            "session rm",
            format!(
                "session `{}` is backed by `{}`, not ssh",
                session.id, session.backend
            ),
        )
        .into());
    }
    match remote {
        Some(remote) => {
            let rs = RemoteSession::from_sock(&session.sandbox_id);
            match parse_ssh_url(&remote.url) {
                Ok(url) => {
                    if let Err(e) = kill_pty_host(&url, &rs) {
                        eprintln!("pillbox: warning: remote pty-host teardown failed: {e}");
                    }
                }
                Err(e) => {
                    eprintln!(
                        "pillbox: warning: remote `{}` url unparseable ({e}); skipping remote teardown.",
                        remote.name
                    );
                }
            }
        }
        None => {
            eprintln!(
                "pillbox: warning: remote `{}` is no longer registered — dropping the record \
                 without remote teardown; kill the remote pty-host by hand if it's still running.",
                session.remote
            );
        }
    }
    crate::events::emit_session_event(
        resolved,
        crate::events::EventType::SessionDropped,
        &session.id,
        Some(session),
    );
    session::delete(resolved, &session.id)?;
    println!("pillbox: ✓ session `{}` removed.", session.id);
    Ok(())
}

fn read_blob(blob_file: Option<&Path>, action: &'static str) -> Result<VaultStdinBlob> {
    let buf = match blob_file {
        Some(path) => {
            let buf = fs::read(path).map_err(|e| {
                PillboxError::runtime(action, format!("read blob file {}: {e}", path.display()))
            })?;
            // SECURITY: the blob carries real OAuth + `--with` secret values.
            // We've now read it fully into memory, so scrub it from the sandbox
            // immediately — otherwise it lingers on disk (e.g. docker://'s
            // `/tmp/pillbox-blob-<id>.json`) where an untrusted agent, running
            // as root in its own container, could read the real credentials the
            // vault exists to withhold. The e2b/ssh wrappers also `rm` it, but
            // doing it here is backend-agnostic (covers docker://, which has no
            // wrapper) and survives a wrapper whose trailing `rm` never runs.
            // Truncate before unlink so the content is gone even if the unlink
            // fails; both are best-effort (cleanup must not abort a valid run).
            let _ = fs::write(path, b"");
            let _ = fs::remove_file(path);
            buf
        }
        None => {
            let mut buf = Vec::new();
            std::io::stdin()
                .read_to_end(&mut buf)
                .map_err(|e| PillboxError::runtime(action, format!("read stdin: {e}")))?;
            buf
        }
    };
    VaultStdinBlob::from_bytes(&buf)
}

fn resolve_blob_spec(blob: &VaultStdinBlob, action: &'static str) -> Result<&'static AgentSpec> {
    crate::agents::ALL
        .iter()
        .copied()
        .find(|s| s.id() == blob.agent_id)
        .ok_or_else(|| {
            PillboxError::usage(action, format!("unknown agent `{}` in blob", blob.agent_id)).into()
        })
}

/// Layer `blob.env` with `--with` secret values, leasing vaulted entries
/// through the active `VaultSession` (so the stub the agent sees never
/// matches the real value the upstream API will see). Returns the env
/// map the caller hands to the agent process.
fn build_blob_env(
    blob: &VaultStdinBlob,
    mut vault_session: Option<&mut VaultSession>,
    action: &'static str,
) -> Result<std::collections::BTreeMap<String, String>> {
    let mut env = blob.env.clone();
    for s in &blob.secrets {
        let injected = match (&s.vault_meta, vault_session.as_deref_mut()) {
            (Some(meta), Some(session)) => session.lease_api_key(&s.name, &s.value, meta)?,
            (Some(_), None) => {
                return Err(PillboxError::runtime(
                    action,
                    format!(
                        "secret `{}` is marked vaulted but no vault session is active",
                        s.name
                    ),
                )
                .into());
            }
            (None, _) => s.value.clone(),
        };
        env.insert(s.env_var.clone(), injected);
    }
    Ok(env)
}

/// Push the (possibly-mutated) workspace back as the result snapshot,
/// surface the handle to `RESULT_SNAPSHOT_FILE_ENV` if set, and translate
/// a non-zero agent exit into a runtime error. Consumes the
/// `RemoteWorkspace` so its tempdir is dropped at function return.
fn finalize_blob_run(
    remote_workspace: RemoteWorkspace,
    status: std::process::ExitStatus,
    spec_id: &str,
    action: &'static str,
) -> Result<()> {
    let result_snapshot = remote_workspace.backend.push(
        &remote_workspace.workspace_dir,
        PushOptions {
            tag: Some("remote-result".into()),
            message: Some("remote run result".into()),
        },
    )?;
    if let Some(path) = std::env::var_os(RESULT_SNAPSHOT_FILE_ENV) {
        fs::write(&path, result_snapshot.handle.as_str()).map_err(|e| {
            PillboxError::runtime(
                action,
                format!("write {}: {e}", PathBuf::from(path).display()),
            )
        })?;
    }
    eprintln!(
        "pillbox: result snapshot {}",
        result_snapshot.handle.short()
    );
    propagate_blob_exit(status, spec_id, action)
}

/// Translate a non-zero agent exit into a runtime error. The pre-staged
/// (docker://) path uses this directly — its result snapshot is taken
/// host-side after the container is reaped, so there's no in-sandbox push to
/// do, just the exit-code contract.
fn propagate_blob_exit(
    status: std::process::ExitStatus,
    spec_id: &str,
    action: &'static str,
) -> Result<()> {
    if !status.success() {
        return Err(PillboxError::runtime(
            action,
            format!("{spec_id} exited with status {status}"),
        )
        .into());
    }
    Ok(())
}

pub(crate) fn dispatch_vault_stdin(resolved: &Pillbox, blob_file: Option<&Path>) -> Result<()> {
    let action: &'static str = "run --vault-stdin";
    let blob = read_blob(blob_file, action)?;
    let spec = resolve_blob_spec(&blob, action)?;

    let runner_image = docker::check_ready_for(resolved)?;
    let home = spec.home_dir(resolved)?;
    if !home.join(spec.cred_sentinel).exists() {
        return Err(PillboxError::runtime(
            action,
            format!("no stored credentials for `{}` on this remote", spec.id),
        )
        .with_next(format!(
            "pillbox auth login --agent {}  # run this on the REMOTE host",
            spec.id
        ))
        .into());
    }

    // The nested-docker ssh path is S3-only — it has no pre-staged tar-cp
    // mode (that's docker://). A blob without an S3 workspace can't run here.
    let workspace = blob.workspace.as_ref().ok_or_else(|| {
        PillboxError::config(
            action,
            "the ssh path requires an S3 workspace in the blob (no pre-staged mode)",
        )
    })?;
    let remote_workspace = hydrate_remote_workspace(workspace)?;
    let workspace_host = remote_workspace.workspace_dir.clone();
    let guest_workspace = format!("{GUEST_WORKSPACE}/{}", blob.workspace_mount_name);

    let any_vaulted = blob.secrets.iter().any(|s| s.vault_meta.is_some());
    let mut vault_session = if blob.vault || any_vaulted {
        let oauth = blob.vault.then_some(OAuthAgent {
            agent_id: spec.id,
            agent_home: &home,
        });
        Some(VaultSession::start(oauth, resolved, blob.context.clone())?)
    } else {
        None
    };

    let env = build_blob_env(&blob, vault_session.as_mut(), action)?;

    let mut args = base_docker_args();
    args.extend([
        "-v".into(),
        format!("{}:{GUEST_HOME}", home.display()),
        "-v".into(),
        format!("{}:{guest_workspace}", workspace_host.display()),
        "-w".into(),
        guest_workspace.clone(),
    ]);
    for (k, v) in &env {
        args.push("-e".into());
        args.push(format!("{k}={v}"));
    }
    if let Some(session) = &vault_session {
        args.extend(session.docker_extras(GUEST_HOME));
        eprintln!(
            "pillbox: vault proxy listening on {} (ca: {})",
            session.listen_addr(),
            session.ca_cert_path().display()
        );
    }
    args.push(runner_image);
    args.extend(spec.run_argv.iter().map(|s| s.to_string()));
    // Sandbox defaults (claude `--permission-mode auto`) before the user's
    // agent_args, which override. Mirrors the local-docker path.
    args.extend(spec.sandbox_args.iter().map(|s| s.to_string()));
    args.extend(blob.agent_args.clone());

    // Pre-accept the agent's workspace trust dialog (claude) on the
    // bind-mounted home before the nested container starts — claude runs with
    // cwd `guest_workspace`, so that's the project key.
    spec.prepare_workspace_or_warn(&home, &guest_workspace);

    // Live observability, sandbox-side: the agent's transcript lands in
    // this remote's bind-mounted $HOME (local to this process), so the
    // tailer reads it and streams OTLP straight to the operator's
    // collector — the OTEL env was forwarded by the wrapper. The agent
    // runs in the nested container, but its transcript dir is keyed off
    // the in-container cwd (`guest_workspace`). Held until the agent
    // exits; dropped after `finalize_blob_run` for a final drain.
    let _obs = blob.context.session_id.as_deref().and_then(|sid| {
        crate::events::transcripts::spawn_session_observability(
            // No host-side durable log for remote runs: the transcript is
            // written sandbox-side (ephemeral with the sandbox) and remote
            // sequencing is deferred — OTLP-only here, as before.
            None,
            sid,
            spec.id,
            &home,
            &guest_workspace,
            blob.vault || any_vaulted,
            std::time::SystemTime::now(),
        )
    });

    let status = docker::run_interactive(&args)?;
    drop(vault_session);
    finalize_blob_run(remote_workspace, status, spec.id, action)
}

/// Sandbox-side sibling of [`dispatch_vault_stdin`] for environments that
/// ARE the isolation boundary (e2b sandboxes). The diff vs. that function:
///
///   - **No Docker**, no nested runner-image. The agent is `exec`d
///     directly in this process — the sandbox is already isolated.
///   - **Auth comes from the blob**, not a pre-existing `pillbox auth
///     login` on the host. We materialize `blob.agent_auth` into `$HOME`
///     so the agent finds its credentials at the canonical paths.
///   - **No vault proxy** (initial drop). The stub-swap proxy is host-side
///     and can't be reached from inside an e2b sandbox; refuse `--vault`
///     and any vaulted `--with` until that path is built.
///
/// Everything else (workspace hydrate from S3, env layering, result push +
/// `RESULT_SNAPSHOT_FILE_ENV` write, non-zero-exit propagation) mirrors
/// `dispatch_vault_stdin` exactly.
pub(crate) fn dispatch_vault_stdin_direct(
    // `resolved` is the sandbox-side pillbox resolution (typically global —
    // the sandbox has no project pillbox.toml). Used only by the vault
    // session to anchor its CA directory.
    resolved: &Pillbox,
    blob_file: Option<&Path>,
) -> Result<()> {
    let action: &'static str = "run --vault-stdin-direct";
    let blob = read_blob(blob_file, action)?;
    let spec = resolve_blob_spec(&blob, action)?;

    // The Docker path piggybacks on a remote-host login; the direct path
    // can't. If the host pillbox is too old to populate `agent_auth`, the
    // sandbox has no way to authenticate the agent.
    if blob.agent_auth.is_empty() {
        return Err(PillboxError::config(
            action,
            format!(
                "blob carries no forwarded agent auth (agent `{}`) — the direct path needs it",
                spec.id
            ),
        )
        .with_next("upgrade the host pillbox; the direct path requires agent_auth in the blob")
        .into());
    }

    // Resolve $HOME inside the sandbox; auth files write under it and the
    // agent inherits it on exec. Required (e2b sandboxes set HOME for the
    // run user); refuse if absent rather than guess.
    let home_env = std::env::var("HOME")
        .map_err(|_| PillboxError::runtime(action, "HOME is not set in the sandbox environment"))?;
    let home_dir = PathBuf::from(&home_env);
    materialize_agent_auth(&home_dir, &blob.agent_auth, action)?;

    // Workspace: pre-staged (docker://, tar-cp'd into the container, no S3)
    // vs hydrate-from-S3 (e2b). `remote_workspace` (the S3 tempdir) is `None`
    // for pre-staged — the container path persists with the container, and
    // the host pulls results back via `docker cp` after exit.
    let (workspace_host, remote_workspace) = match (&blob.workspace_dir, &blob.workspace) {
        (Some(dir), _) => (PathBuf::from(dir), None),
        (None, Some(ws)) => {
            let rw = hydrate_remote_workspace(ws)?;
            (rw.workspace_dir.clone(), Some(rw))
        }
        (None, None) => {
            return Err(PillboxError::config(
                action,
                "blob carries neither an S3 workspace nor a pre-staged workspace_dir",
            )
            .into());
        }
    };

    // Vault session: in-process here, so the proxy listens on the
    // sandbox's 127.0.0.1 and the agent reaches it without any host
    // round-trip. We start it BEFORE building env so lease_api_key can
    // hand us stub values to inject. Kept alive across `cmd.status()`;
    // its `Drop` order tears down leases → server → runtime → stub files
    // (see `vault/session.rs`).
    let any_vaulted = blob.secrets.iter().any(|s| s.vault_meta.is_some());
    let mut vault_session = if blob.vault || any_vaulted {
        let oauth = blob.vault.then_some(OAuthAgent {
            agent_id: spec.id,
            agent_home: &home_dir,
        });
        Some(VaultSession::start(oauth, resolved, blob.context.clone())?)
    } else {
        None
    };

    let mut env = build_blob_env(&blob, vault_session.as_mut(), action)?;

    // If a vault session is up, lay down its OAuth stub (overwriting the
    // real OAuth we materialized earlier — the proxy now owns the real
    // value in memory) and inject the proxy env. The agent reads stub
    // creds from $HOME, sends outbound to 127.0.0.1:<port>, and our
    // CA-backed MITM swaps the stub for the real value in flight.
    if let Some(session) = &vault_session {
        let extras = session.direct_extras();
        for stub in &extras.oauth_stub_writes {
            let target = home_dir.join(&stub.creds_rel);
            fs::copy(&stub.stub_source, &target).map_err(|e| {
                PillboxError::runtime(
                    action,
                    format!("install OAuth stub at {}: {e}", target.display()),
                )
            })?;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o600))
                .with_context(|| format!("chmod 0o600 {}", target.display()))?;
        }
        for (k, v) in extras.env {
            env.insert(k, v);
        }
        eprintln!(
            "pillbox: vault proxy listening on {} (ca: {})",
            session.listen_addr(),
            session.ca_cert_path().display()
        );
    }

    // Pre-accept the agent's workspace trust dialog (claude): the agent runs
    // with cwd `workspace_host`, so that's the project key in its $HOME's
    // ~/.claude.json.
    spec.prepare_workspace_or_warn(&home_dir, &workspace_host.to_string_lossy());

    let mut cmd = std::process::Command::new(spec.run_argv[0]);
    if spec.run_argv.len() > 1 {
        cmd.args(&spec.run_argv[1..]);
    }
    // Sandbox defaults (claude `--permission-mode auto`) before the user's
    // agent_args, which override.
    cmd.args(spec.sandbox_args);
    cmd.args(&blob.agent_args);
    cmd.current_dir(&workspace_host);
    for (k, v) in &env {
        cmd.env(k, v);
    }
    // Live observability, sandbox-side: the agent runs directly in this
    // e2b sandbox (no nested docker), writing its transcript under our
    // own $HOME. The tailer reads it and streams OTLP to the operator's
    // collector (OTEL env forwarded by the e2b helper wrapper). The
    // agent's cwd is `workspace_host`, which keys its transcript dir.
    // Held until the agent exits; dropped after finalize for a final drain.
    let _obs = blob.context.session_id.as_deref().and_then(|sid| {
        crate::events::transcripts::spawn_session_observability(
            // No host-side durable log for remote runs (see the ssh path).
            None,
            sid,
            spec.id,
            &home_dir,
            &workspace_host.to_string_lossy(),
            blob.vault || any_vaulted,
            std::time::SystemTime::now(),
        )
    });

    let status = cmd
        .status()
        .map_err(|e| PillboxError::runtime(action, format!("spawn `{}`: {e}", spec.id)))?;
    drop(vault_session);
    match remote_workspace {
        // e2b: push the mutated workspace back through the shared S3 repo.
        Some(rw) => finalize_blob_run(rw, status, spec.id, action),
        // Pre-staged (docker://): the host pulls results via `docker cp` once
        // the container exits — no in-sandbox push, just the exit-code contract.
        None => propagate_blob_exit(status, spec.id, action),
    }
}

/// Write every [`AuthFile`] from the blob under `home`, creating parent
/// dirs at 0o700 and files at the stated mode. Rejects path components
/// that would escape `home` (`..`, absolute paths) so a hostile blob can't
/// scribble outside the agent's HOME.
fn materialize_agent_auth(home: &Path, files: &[AuthFile], action: &'static str) -> Result<()> {
    for f in files {
        // Reject `..` / absolute / empty / drive-prefixed paths. `Path`
        // does the heavy lifting; we just refuse anything that resolves to
        // a non-`Normal` component.
        let rel = Path::new(&f.rel_path);
        if rel.as_os_str().is_empty() {
            return Err(PillboxError::config(action, "empty rel_path in agent_auth").into());
        }
        for c in rel.components() {
            use std::path::Component;
            if !matches!(c, Component::Normal(_)) {
                return Err(PillboxError::config(
                    action,
                    format!("unsafe rel_path `{}` in agent_auth", f.rel_path),
                )
                .into());
            }
        }
        let target = home.join(rel);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
                .with_context(|| format!("chmod 0o700 {}", parent.display()))?;
        }
        fs::write(&target, &f.contents).with_context(|| format!("write {}", target.display()))?;
        fs::set_permissions(&target, fs::Permissions::from_mode(f.mode))
            .with_context(|| format!("chmod {:o} {}", f.mode, target.display()))?;
    }
    Ok(())
}

struct RemoteWorkspace {
    _tempdir: tempfile::TempDir,
    workspace_dir: PathBuf,
    backend: RusticBackend,
}

fn hydrate_remote_workspace(workspace: &InlineWorkspace) -> Result<RemoteWorkspace> {
    let tempdir = tempfile::Builder::new()
        .prefix("pillbox-remote-workspace-")
        .tempdir()
        .map_err(|e| PillboxError::runtime("run --vault-stdin", format!("create tempdir: {e}")))?;
    let password_file = tempdir.path().join(PASSWORD_FILE);
    let mut password = workspace.repo_password.clone();
    password.push('\n');
    crate::paths::write_private_file(&password_file, password.as_bytes())?;
    let workspace_dir = tempdir.path().join("workspace");
    fs::create_dir_all(&workspace_dir)
        .with_context(|| format!("create {}", workspace_dir.display()))?;
    let backend = RusticBackend {
        variant: RusticVariant::S3(workspace.s3.clone()),
        password_file,
    };
    if let Some(base) = workspace.base_snapshot.as_deref() {
        let handle = SnapshotHandle::new(base.to_string());
        backend.pull(&workspace_dir, Some(&handle))?;
    }
    Ok(RemoteWorkspace {
        _tempdir: tempdir,
        workspace_dir,
        backend,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SECURITY regression: `read_blob` must scrub the on-disk blob (real
    /// OAuth + secrets) the moment it's read, so it can't linger in the
    /// sandbox for an untrusted agent to recover. Guards the docker:// vault
    /// bypass (the blob is `docker cp`'d to a predictable container path with
    /// no shell wrapper to `rm` it).
    #[test]
    fn read_blob_unlinks_the_file() {
        use std::io::Write as _;
        let blob = VaultStdinBlob {
            version: crate::sandbox::vault_stdin::BLOB_VERSION,
            agent_id: "claude".into(),
            agent_args: vec![],
            workspace_mount_name: "w".into(),
            vault: false,
            secrets: vec![],
            env: Default::default(),
            context: crate::vault::RunContext::default(),
            workspace: None,
            workspace_dir: Some("/workspace".into()),
            agent_auth: vec![],
        };
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(&blob.to_bytes().unwrap()).unwrap();
        let path = tmp.path().to_path_buf();
        assert!(path.exists());

        let back = read_blob(Some(&path), "test").expect("read");
        assert_eq!(back.agent_id, "claude");
        assert!(
            !path.exists(),
            "blob must be unlinked after read so creds don't linger in the sandbox"
        );
        drop(tmp); // its Drop tolerates the already-removed file
    }

    #[test]
    fn remote_session_paths_are_session_scoped() {
        let rs = RemoteSession::new("abcdef012345");
        assert_eq!(rs.sock, "/tmp/pillbox-attach-abcdef012345.sock");
        assert_eq!(rs.blob, "/tmp/pillbox-blob-abcdef012345.json");
        assert_eq!(rs.log, "/tmp/pillbox-host-abcdef012345.log");
        assert_eq!(rs.result, "/tmp/pillbox-result-abcdef012345.txt");
        // No shell metacharacters in any path — they're interpolated into
        // ssh command lines, so this guards against a future id format
        // that could break out of the single quotes.
        for p in [&rs.sock, &rs.blob, &rs.log, &rs.result] {
            assert!(
                p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.')),
                "unexpected char in remote path `{p}`"
            );
        }
    }

    #[test]
    fn remote_session_from_sock_recovers_sibling_paths() {
        // `session attach` / `session rm` reconstruct blob + log from the
        // stored sock (the only handle persisted on the record), so a
        // freshly-minted session and a round-tripped one must agree.
        let minted = RemoteSession::new("0011aabbccdd");
        let recovered = RemoteSession::from_sock(&minted.sock);
        assert_eq!(recovered.sock, minted.sock);
        assert_eq!(recovered.blob, minted.blob);
        assert_eq!(recovered.log, minted.log);
        assert_eq!(recovered.result, minted.result);
    }

    #[test]
    fn remote_session_from_sock_tolerates_unexpected_shape() {
        // A hand-edited / older record might not match the minting
        // pattern; `from_sock` must still produce *some* sibling paths
        // for the kill path rather than panic.
        let rs = RemoteSession::from_sock("/custom/path.sock");
        assert_eq!(rs.sock, "/custom/path.sock");
        assert_eq!(rs.blob, "/custom/path.sock.blob");
        assert_eq!(rs.log, "/custom/path.sock.log");
        assert_eq!(rs.result, "/custom/path.sock.result");
    }

    #[test]
    fn sh_quote_wraps_plain_value() {
        // A value with no single quotes is just single-quoted whole.
        assert_eq!(sh_quote("plain-value_1.2"), "'plain-value_1.2'");
        // Spaces / metacharacters live safely inside the single quotes.
        assert_eq!(sh_quote("a b; rm -rf /"), "'a b; rm -rf /'");
    }

    #[test]
    fn sh_quote_escapes_embedded_single_quote() {
        // The classic close-quote / literal-quote / reopen idiom: a lone
        // `'` becomes `'"'"'`. This is what makes a hostile value (e.g. a
        // hand-edited webhook URL) fail to parse cleanly instead of
        // breaking out of the quotes and injecting a command.
        assert_eq!(sh_quote("it's"), r#"'it'"'"'s'"#);
        assert_eq!(
            sh_quote("'; touch pwned; '"),
            r#"''"'"'; touch pwned; '"'"''"#
        );
    }

    #[test]
    fn build_wrapper_has_started_done_and_result_wiring() {
        // The inner wrapper is what `bash -lc` evaluates — assert on its
        // (single-shell-level) text directly.
        let rs = RemoteSession::new("abcdef012345");
        let w = build_wrapper(&rs, "abcdef012345", None, None, &[]);
        // Pre-minted id is baked into BOTH bookends.
        assert!(w.contains("session started 'abcdef012345'"));
        assert!(w.contains("session done 'abcdef012345'"));
        // Result-snapshot round-trip: file exported, read back, forwarded.
        assert!(w.contains(
            "export PILLBOX_RESULT_SNAPSHOT_FILE='/tmp/pillbox-result-abcdef012345.txt'"
        ));
        assert!(w.contains("--result-snapshot"));
        // The inner command is the real run, via --blob-file (NOT < blob)
        // so the child's stdin stays the PTY for the inner `docker run -it`.
        assert!(w.contains("run --vault-stdin --blob-file '/tmp/pillbox-blob-abcdef012345.json'"));
        // Terminal status is derived from the captured exit code.
        assert!(w.contains("--exit-code \"$PB_EXIT\""));
        assert!(w.contains("echo ok || echo failed"));
        // Sandbox-side emitter tag so events render with emitter=sandbox.
        assert!(w.contains("export PILLBOX_SANDBOX_SIDE=1"));
        // No webhook / parent exports when neither is set.
        assert!(!w.contains("PILLBOX_EVENTS_WEBHOOK"));
        assert!(!w.contains("PILLBOX_PARENT_SESSION_ID"));
    }

    #[test]
    fn build_launch_line_wraps_the_child_in_bash() {
        // The outer line: `bash -lc <quoted-wrapper>`, backgrounded under
        // setsid. The wrapper is double-quoted (outer level) so its inner
        // single quotes don't appear verbatim here — only the structure.
        let rs = RemoteSession::new("abcdef012345");
        let line = build_launch_line(&rs, "abcdef012345", None, None, &[]);
        assert!(line.contains("setsid"));
        assert!(line.contains("pty-host --sock '/tmp/pillbox-attach-abcdef012345.sock'"));
        assert!(line.contains("-- bash -lc "));
        assert!(line.contains(">'/tmp/pillbox-host-abcdef012345.log'"));
        assert!(line.trim_end().ends_with('&'));
    }

    #[test]
    fn build_wrapper_threads_webhook_and_parent() {
        let rs = RemoteSession::new("0011aabbccdd");
        let w = build_wrapper(
            &rs,
            "0011aabbccdd",
            Some("https://hook.example/e"),
            Some("00112233"),
            &[],
        );
        assert!(w.contains("export PILLBOX_EVENTS_WEBHOOK='https://hook.example/e'"));
        assert!(w.contains("export PILLBOX_PARENT_SESSION_ID='00112233'"));
    }

    #[test]
    fn build_wrapper_forwards_otel_env() {
        let rs = RemoteSession::new("0011aabbccdd");
        let otel = vec![
            (
                "OTEL_EXPORTER_OTLP_ENDPOINT".to_string(),
                "https://collector.example:4318".to_string(),
            ),
            (
                "OTEL_EXPORTER_OTLP_HEADERS".to_string(),
                "authorization=Bearer xyz".to_string(),
            ),
        ];
        let w = build_wrapper(&rs, "0011aabbccdd", None, None, &otel);
        assert!(w.contains("export OTEL_EXPORTER_OTLP_ENDPOINT='https://collector.example:4318'"));
        // Header value (with `=` and a space) survives as one quoted token.
        assert!(w.contains("export OTEL_EXPORTER_OTLP_HEADERS='authorization=Bearer xyz'"));
        // OTEL exports precede the run, so the tailer the run spawns sees them.
        let otel_pos = w.find("OTEL_EXPORTER_OTLP_ENDPOINT").unwrap();
        let run_pos = w.find("run --vault-stdin").unwrap();
        assert!(otel_pos < run_pos, "otel export must come before the run");
    }

    #[test]
    fn build_wrapper_quotes_a_hostile_webhook() {
        // Belt-and-suspenders: the host validates the URL shape, but if a
        // `'` ever slipped through it must stay quoted inside the wrapper,
        // not break out into a second command.
        let rs = RemoteSession::new("0011aabbccdd");
        let w = build_wrapper(&rs, "0011aabbccdd", Some("h'; rm -rf /; '"), None, &[]);
        // The hostile quote is neutralized via the `'"'"'` idiom — the
        // literal `rm -rf /` is data, never a standalone command token.
        assert!(w.contains(r#"export PILLBOX_EVENTS_WEBHOOK='h'"'"'; rm -rf /; '"'"''"#));
    }

    /// Prove the nested two-level quoting actually unquotes correctly by
    /// running the real outer + inner shells. We can't do the full ssh
    /// round-trip in the sandbox, but the quoting is the part that bit a
    /// prior reviewer; this exercises it end-to-end with `bash`.
    ///
    /// We replace the un-runnable bits (`setsid`, the absolute remote
    /// pillbox path, `pty-host`) — the test only validates that a hostile
    /// webhook value survives BOTH shell levels as a single literal token
    /// rather than splitting into an injected command.
    #[test]
    fn nested_quoting_round_trips_through_two_real_shells() {
        let rs = RemoteSession::new("abcdef012345");
        let hostile = "h'; touch /tmp/pillbox-pwned-$$; '";
        // Build just the webhook export at the inner level, then wrap the
        // whole thing in the same outer `bash -lc <sh_quote(wrapper)>`
        // shape `build_launch_line` produces.
        let inner = format!(
            "export PILLBOX_EVENTS_WEBHOOK={}; printf '%s' \"$PILLBOX_EVENTS_WEBHOOK\"",
            sh_quote(hostile)
        );
        let _ = &rs; // RemoteSession constructed to mirror the real call shape.
        let outer = format!("bash -lc {}", sh_quote(&inner));
        // Run the OUTER line through a real shell (stand-in for the ssh
        // remote login shell). If quoting is wrong, the `touch` would run
        // and/or the echoed value would be truncated.
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(&outer)
            .output()
            .expect("run nested shell");
        assert!(out.status.success(), "stderr: {:?}", out.stderr);
        let echoed = String::from_utf8_lossy(&out.stdout);
        // The value survives both shell levels verbatim — proof the
        // hostile `'` was data, not a quote-breakout.
        assert_eq!(echoed, hostile);
    }

    #[test]
    fn ssh_base_uses_dash_p_for_ported_hosts() {
        // Port must go through `-p`, not a `user@host:port` positional
        // (not portable across openssh versions). Inspect the rendered
        // argv via the `Debug` form of the built `Command`.
        let url = SshUrl {
            user: "root".into(),
            host: "152.53.188.221".into(),
            port: Some(2222),
        };
        let cmd = ssh_base(&url);
        let rendered = format!("{cmd:?}");
        assert!(rendered.contains("-p"), "missing -p: {rendered}");
        assert!(rendered.contains("2222"), "missing port: {rendered}");
        assert!(
            rendered.contains("root@152.53.188.221"),
            "missing bare user@host: {rendered}"
        );
        assert!(
            !rendered.contains("152.53.188.221:2222"),
            "port leaked into destination: {rendered}"
        );
    }

    #[test]
    fn ssh_base_uses_bare_destination_when_no_port() {
        let url = SshUrl {
            user: "root".into(),
            host: "152.53.188.221".into(),
            port: None,
        };
        let cmd = ssh_base(&url);
        let rendered = format!("{cmd:?}");
        assert!(
            rendered.contains("root@152.53.188.221"),
            "missing destination: {rendered}"
        );
        assert!(!rendered.contains("-p"), "unexpected -p without a port");
    }

    #[test]
    fn materialize_agent_auth_rejects_unsafe_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        // `..` would escape HOME — must reject.
        let dot_dot = vec![AuthFile {
            rel_path: "../escape.json".into(),
            mode: 0o600,
            contents: b"x".to_vec(),
        }];
        assert!(materialize_agent_auth(home, &dot_dot, "test").is_err());
        // Absolute path — must reject.
        let abs = vec![AuthFile {
            rel_path: "/etc/passwd".into(),
            mode: 0o600,
            contents: b"x".to_vec(),
        }];
        assert!(materialize_agent_auth(home, &abs, "test").is_err());
        // Empty — must reject.
        let empty = vec![AuthFile {
            rel_path: "".into(),
            mode: 0o600,
            contents: b"x".to_vec(),
        }];
        assert!(materialize_agent_auth(home, &empty, "test").is_err());
        // Nested ".." midway — must reject (a Normal+ParentDir sequence
        // is rejected as a whole because every component is checked).
        let mid = vec![AuthFile {
            rel_path: ".claude/../../escape".into(),
            mode: 0o600,
            contents: b"x".to_vec(),
        }];
        assert!(materialize_agent_auth(home, &mid, "test").is_err());
        // Confirm the rejected paths didn't actually land anywhere.
        assert!(!home.join("escape.json").exists());
        assert!(!home.parent().unwrap().join("escape.json").exists());

        // Sanity: a well-formed nested path under HOME is accepted.
        let ok = vec![AuthFile {
            rel_path: ".claude/.credentials.json".into(),
            mode: 0o600,
            contents: br#"{"k":"v"}"#.to_vec(),
        }];
        materialize_agent_auth(home, &ok, "test").unwrap();
        let written = home.join(".claude").join(".credentials.json");
        assert!(written.exists());
        assert_eq!(std::fs::read(&written).unwrap(), br#"{"k":"v"}"#);
        // 0o600 on the written file (mode includes file-type bits; mask).
        let mode = std::fs::metadata(&written).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
