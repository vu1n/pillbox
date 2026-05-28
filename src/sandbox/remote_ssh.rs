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
    fmt, fs,
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::SandboxBackend;
use crate::agents::{
    base_docker_args, workspace_mount_name, AgentSpec, RunOpts, GUEST_HOME, GUEST_WORKSPACE,
};
use crate::attach::pump::{self, Outcome};
use crate::config::BackendKind;
use crate::docker;
use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::remote::{parse_ssh_url, Remote, SshUrl};
use crate::session::{self, Session, BACKEND_SSH};
use crate::vault::{OAuthAgent, VaultMeta, VaultSession};
use crate::workspace::rustic::{RusticBackend, RusticVariant, S3Config, PASSWORD_FILE};
use crate::workspace::{PushOptions, SnapshotHandle, WorkspaceBackend};

/// Absolute path to the remote `pillbox` binary. The user installs it
/// separately (package / `cargo install`); we don't deploy it. Hard-coded
/// rather than relying on the remote shell's `$PATH` because the launch
/// runs through `ssh <dest> <command>` (a non-login shell on many hosts,
/// where `/usr/local/bin` may be absent from `$PATH`).
const REMOTE_PILLBOX: &str = "/usr/local/bin/pillbox";

/// Wire format for `pillbox run --vault-stdin`. See module docs.
///
/// `Debug` is implemented by hand to keep secret-bearing `InlineSecret`
/// values out of any accidental `{:?}` / `dbg!` / tracing output.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct VaultStdinBlob {
    pub(crate) version: u32,
    pub(crate) agent_id: String,
    #[serde(default)]
    pub(crate) agent_args: Vec<String>,
    pub(crate) workspace_mount_name: String,
    #[serde(default)]
    pub(crate) vault: bool,
    #[serde(default)]
    pub(crate) secrets: Vec<InlineSecret>,
    #[serde(default)]
    pub(crate) env: std::collections::BTreeMap<String, String>,
    pub(crate) workspace: InlineWorkspace,
    /// Files copied from the host's `<auth_pillbox>/auth/<agent_id>/`,
    /// to be re-materialized under the agent's `$HOME` on the receiver.
    /// Populated for the direct-exec path ([`dispatch_vault_stdin_direct`])
    /// where the sandbox has no pre-existing agent login; Docker-shelled
    /// receivers (the SSH backend's [`dispatch_vault_stdin`]) ignore it
    /// because they mount `home_dir` directly. `#[serde(default)]` keeps
    /// the wire backward-compatible.
    #[serde(default)]
    pub(crate) agent_auth: Vec<AuthFile>,
}

impl fmt::Debug for VaultStdinBlob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let auth_bytes: usize = self.agent_auth.iter().map(|f| f.contents.len()).sum();
        f.debug_struct("VaultStdinBlob")
            .field("version", &self.version)
            .field("agent_id", &self.agent_id)
            .field("agent_args", &self.agent_args)
            .field("workspace_mount_name", &self.workspace_mount_name)
            .field("vault", &self.vault)
            .field("secrets", &self.secrets)
            .field("env_keys", &self.env.keys().collect::<Vec<_>>())
            .field("workspace", &self.workspace)
            .field(
                "agent_auth",
                &format_args!("<{} files, {}B redacted>", self.agent_auth.len(), auth_bytes),
            )
            .finish()
    }
}

/// Workspace material needed by a remote host to hydrate and publish through
/// the same S3/R2 rustic repository as the local pillbox.
///
/// `s3` is the repo config verbatim — it slots straight back into a
/// [`RusticBackend`] on the remote, no field-by-field copy. This is
/// secret-bearing (`s3` credentials + `repo_password` are real values,
/// not env var names); `Debug` redacts them.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct InlineWorkspace {
    pub(crate) s3: S3Config,
    pub(crate) repo_password: String,
    #[serde(default)]
    pub(crate) base_snapshot: Option<String>,
}

impl fmt::Debug for InlineWorkspace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `s3` redacts its own credentials via its `Debug` impl.
        f.debug_struct("InlineWorkspace")
            .field("s3", &self.s3)
            .field("repo_password", &"<redacted>")
            .field("base_snapshot", &self.base_snapshot)
            .finish()
    }
}

/// One `--with` entry's resolved form on the wire. `vault_meta` is
/// `None` for plain secrets; `Some` for vaulted ones (the remote
/// re-leases through its own vault session at swap time).
///
/// `Debug` is implemented by hand so `value` (real secret material)
/// never lands in logs / panic backtraces / tracing spans.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct InlineSecret {
    pub(crate) name: String,
    pub(crate) env_var: String,
    pub(crate) value: String,
    #[serde(default)]
    pub(crate) vault_meta: Option<VaultMeta>,
}

impl fmt::Debug for InlineSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InlineSecret")
            .field("name", &self.name)
            .field("env_var", &self.env_var)
            .field("value", &"<redacted>")
            .field("vault_meta", &self.vault_meta)
            .finish()
    }
}

/// One file lifted from the LOCAL host's `<auth_pillbox>/auth/<agent_id>/`
/// and serialized into the blob so the receiving sandbox can rehydrate the
/// agent's `$HOME` before exec. `rel_path` is normalized to forward slashes
/// and validated against `..` / absolute components — the receiver joins it
/// onto `$HOME` directly.
///
/// `Debug` redacts `contents` so accidental tracing of the blob doesn't
/// surface OAuth tokens / session keys.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct AuthFile {
    pub(crate) rel_path: String,
    /// 0o600 (single-file) / 0o700 (directory) on extract. Mode 0o644 is
    /// upcast to 0o600; we never widen perms relative to the source dir.
    pub(crate) mode: u32,
    #[serde(with = "base64_bytes")]
    pub(crate) contents: Vec<u8>,
}

impl fmt::Debug for AuthFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthFile")
            .field("rel_path", &self.rel_path)
            .field("mode", &format_args!("0o{:o}", self.mode))
            .field("contents", &format_args!("<{}B redacted>", self.contents.len()))
            .finish()
    }
}

/// Bytes ↔ base64 (standard alphabet, padded) for the JSON wire. The
/// receiver doesn't need to interpret these — `from_bytes` just decodes
/// and writes back — so we use the standard engine, not the URL variant.
mod base64_bytes {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(b: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(b))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let raw = String::deserialize(d)?;
        STANDARD
            .decode(raw.as_bytes())
            .map_err(serde::de::Error::custom)
    }
}

/// Current blob version. Bumped only on breaking changes; the remote
/// rejects unknown future versions explicitly so a newer-CLI / older-
/// remote combo fails loudly instead of silently dropping required
/// fields. Unknown JSON keys WITHIN a known version are still tolerated
/// (serde default) — version bumps are reserved for semantic changes.
pub(crate) const BLOB_VERSION: u32 = 2;

pub(crate) const RESULT_SNAPSHOT_FILE_ENV: &str = "PILLBOX_RESULT_SNAPSHOT_FILE";

impl VaultStdinBlob {
    pub(crate) fn to_bytes(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self).with_context(|| "serialize vault-stdin blob".to_string())
    }

    pub(crate) fn from_bytes(buf: &[u8]) -> Result<Self> {
        // We deliberately don't surface serde_json's error detail: it
        // includes line/column/snippet of the offending bytes, which
        // could echo back partial secret material if the blob is
        // corrupted or if a user accidentally pipes a credential file
        // into `pillbox run --vault-stdin`.
        let blob: VaultStdinBlob = serde_json::from_slice(buf).map_err(|_| {
            PillboxError::config("vault-stdin", "invalid JSON blob on stdin".to_string())
        })?;
        if blob.version != BLOB_VERSION {
            return Err(PillboxError::config(
                "vault-stdin",
                format!(
                    "blob version {} not supported by this pillbox (expected {})",
                    blob.version, BLOB_VERSION
                ),
            )
            .with_next("upgrade pillbox on the remote host to match the client version")
            .into());
        }
        Ok(blob)
    }
}

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

        let blob = build_vault_stdin_blob(spec, &opts, resolved, "run --remote")?;

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

        // Pre-mint the session id so the per-session remote sock / blob /
        // log paths are unique and so a `--detach` record (and the kill
        // path) reference a stable handle. Mirrors the e2b backend.
        let session_id = Session::new_id();
        let remote = RemoteSession::new(&session_id);

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
                base_snapshot: blob.workspace.base_snapshot.clone(),
                result_snapshot: None,
                expires_at: opts.ttl_seconds.map(session::expires_at_from_ttl),
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
fn build_wrapper(
    remote: &RemoteSession,
    session_id: &str,
    webhook: Option<&str>,
    parent: Option<&str>,
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

    format!(
        "export PILLBOX_SANDBOX_SIDE=1; \
         export PILLBOX_SESSION_STARTED_AT=\"$(date -u -Iseconds 2>/dev/null)\"; \
         {webhook_export}{parent_export}\
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
) -> String {
    let wrapper = build_wrapper(remote, session_id, webhook, parent);
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
    let launch = build_launch_line(remote, session_id, webhook.as_deref(), parent.as_deref());

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

/// Build a [`VaultStdinBlob`] from `RunOpts` against the resolved pillbox.
/// Shared between the SSH backend (sends inline over stdin) and the E2B
/// backend (stages the same struct to a sandbox tmp file via the Files
/// API). Both backends consume identical wire shape on the remote side
/// (`pillbox run --vault-stdin`), so the resolution logic lives here in
/// one place — secret material crosses the host boundary once, in a
/// known JSON wrapping with `BLOB_VERSION`.
///
/// `action` is the diagnostic label threaded into error messages (e.g.
/// `"run --remote"` vs `"run --remote (e2b)"`) so the user sees which
/// backend they were on when a `--with` / `--env` resolution failed.
/// Defense-in-depth cap on total forwarded auth bytes. The narrowed walk
/// below (non-recursive over `cred_sentinel`'s parent dir) keeps real
/// auth state well under this — typical case is a few hundred KB — but a
/// pathological config-file size should error loudly rather than smuggle
/// the agent's whole dir into the sandbox blob.
const AGENT_AUTH_MAX_BYTES: u64 = 10 * 1024 * 1024;
/// Per-file ceiling (matches `AGENT_AUTH_MAX_BYTES` for now): a single
/// runaway config file would dominate the blob just as badly as many.
const AGENT_AUTH_MAX_FILE_BYTES: u64 = AGENT_AUTH_MAX_BYTES;

/// Collect the agent's canonical auth state from `home` for forwarding to
/// a sandbox without a pre-existing login. The walk is intentionally
/// narrow: only **regular files in `cred_sentinel`'s parent directory**,
/// non-recursive. That covers
///
///   - claude:   `.claude/.credentials.json` + sibling config(s)
///   - codex:    `.codex/auth.json` + sibling config(s)
///   - opencode: `.local/share/opencode/auth.json` + siblings
///   - pi:       `.pi/agent/auth.json` + siblings
///
/// and excludes the noisy cache/history subdirs adjacent to it (e.g.
/// `.claude/projects/` per-project session histories, easily many MiB)
/// which a fresh remote `-p`-style run does not need. Symlinks are
/// skipped so a hostile link can't pull in arbitrary host bytes.
fn collect_agent_auth(
    home: &Path,
    cred_sentinel: &str,
    action: &'static str,
) -> Result<Vec<AuthFile>> {
    let sentinel_rel = Path::new(cred_sentinel);
    let parent_rel = sentinel_rel.parent().ok_or_else(|| {
        PillboxError::runtime(
            action,
            format!("cred_sentinel `{cred_sentinel}` has no parent directory"),
        )
    })?;
    let parent_abs = home.join(parent_rel);
    if !parent_abs.exists() {
        // No login on this host yet — return empty; the receiving direct
        // dispatcher decides whether that's an error for its mode.
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    let mut total: u64 = 0;
    let entries = fs::read_dir(&parent_abs)
        .with_context(|| format!("read agent auth dir {}", parent_abs.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("walk {}", parent_abs.display()))?;
        // `DirEntry::metadata` is `lstat`-equivalent on Unix — does NOT
        // follow symlinks, so a symlink check skips them safely.
        let meta = entry
            .metadata()
            .with_context(|| format!("stat {}", entry.path().display()))?;
        let ty = meta.file_type();
        if ty.is_symlink() || !ty.is_file() {
            continue;
        }
        if meta.len() > AGENT_AUTH_MAX_FILE_BYTES {
            return Err(PillboxError::runtime(
                action,
                format!(
                    "agent auth file {} exceeds {} MiB",
                    entry.path().display(),
                    AGENT_AUTH_MAX_FILE_BYTES / (1024 * 1024)
                ),
            )
            .into());
        }
        total = total.saturating_add(meta.len());
        if total > AGENT_AUTH_MAX_BYTES {
            return Err(PillboxError::runtime(
                action,
                format!(
                    "agent auth dir {} exceeds {} MiB across forwarded files",
                    parent_abs.display(),
                    AGENT_AUTH_MAX_BYTES / (1024 * 1024)
                ),
            )
            .into());
        }
        // rel_path is relative to `home` (the agent's mounted HOME on the
        // receiver) so the sandbox writes back at the canonical location.
        let rel = parent_rel.join(entry.file_name());
        let rel_str = rel
            .to_str()
            .ok_or_else(|| {
                PillboxError::runtime(
                    action,
                    format!("non-UTF8 path in agent auth dir: {}", rel.display()),
                )
            })?
            .replace(std::path::MAIN_SEPARATOR, "/");
        let contents = fs::read(entry.path())
            .with_context(|| format!("read {}", entry.path().display()))?;
        // Don't widen perms on extract: 0o600 for every forwarded file —
        // a fresh sandbox HOME shouldn't carry group/world-readable agent
        // state even if the local file was loose.
        out.push(AuthFile {
            rel_path: rel_str,
            mode: 0o600,
            contents,
        });
    }
    Ok(out)
}

pub(super) fn build_vault_stdin_blob(
    spec: &AgentSpec,
    opts: &RunOpts,
    resolved: &Pillbox,
    action: &'static str,
) -> Result<VaultStdinBlob> {
    // Resolve --with entries locally. Real secret values come from THIS
    // host's vault; only the resolved values cross the wire (once, into
    // the remote pillbox's vault session memory or the sandbox's tmp blob).
    let withs = crate::agents::resolve_with_entries(resolved, &opts.withs)?;
    if opts.vault && !spec.vault_capable {
        return Err(PillboxError::usage(
            action,
            format!("--vault is not supported for `{}`", spec.id),
        )
        .into());
    }
    // A vaulted `--with` secret drives the stub-swap proxy just like
    // `--vault`; reject it for agents that can't reach the proxy so the
    // stub never ships to the provider.
    let vaulted: Vec<&str> = withs
        .iter()
        .filter(|w| w.meta.is_some())
        .map(|w| w.secret_name.as_str())
        .collect();
    if !vaulted.is_empty() && !spec.vault_capable {
        return Err(PillboxError::usage(
            action,
            format!(
                "agent `{}` does not support the vault proxy, so it can't use vaulted secret(s): {}",
                spec.id,
                vaulted.join(", ")
            ),
        )
        .into());
    }

    let mut secrets = Vec::with_capacity(withs.len());
    for w in &withs {
        let real = crate::secrets::read(resolved, &w.secret_name)?.ok_or_else(|| {
            PillboxError::runtime(action, format!("secret `{}` not found", w.secret_name))
                .with_next(format!("pillbox secret add {}", w.secret_name))
        })?;
        secrets.push(InlineSecret {
            name: w.secret_name.clone(),
            env_var: w.env_var.clone(),
            value: real.trim_end().to_string(),
            vault_meta: w.meta.clone(),
        });
    }

    // Pre-resolve --env / --env-file the same way LocalDocker would, so
    // the remote doesn't have to know anything about the host's env
    // bundles or local files.
    let mut env = std::collections::BTreeMap::new();
    for bundle in &opts.env_bundles {
        let vars = crate::envs::read(resolved, bundle)?.ok_or_else(|| {
            PillboxError::runtime(action, format!("env bundle `{bundle}` not found"))
        })?;
        for (k, v) in vars {
            env.insert(k, v);
        }
    }
    for path in &opts.env_files {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            PillboxError::runtime(
                action,
                format!("could not read --env-file {}: {e}", path.display()),
            )
        })?;
        let vars = crate::envs::parse_dotenv(&raw, &path.display().to_string())?;
        for (k, v) in vars {
            env.insert(k, v);
        }
    }

    let workspace_host = match &opts.workspace {
        Some(p) => p.clone(),
        None => std::env::current_dir().context("resolve current working directory")?,
    };
    let workspace_name = workspace_mount_name(&workspace_host, opts.name.as_deref())?;
    let workspace = build_inline_workspace(resolved, opts, &workspace_host, action)?;
    // Forward the agent's local auth state (OAuth tokens, config) so a
    // sandbox without a pre-existing login (e.g. e2b) can rehydrate it
    // before exec. The Docker-shelled path ignores `agent_auth` and mounts
    // `home_dir` directly, so this is wasted bytes for SSH today — small
    // enough not to fight about, and unifying on the forward keeps a
    // future "ssh without pre-login" path open.
    let agent_auth = collect_agent_auth(&spec.home_dir(resolved)?, spec.cred_sentinel, action)?;

    Ok(VaultStdinBlob {
        version: BLOB_VERSION,
        agent_id: spec.id.to_string(),
        agent_args: opts.args.clone(),
        workspace_mount_name: workspace_name,
        vault: opts.vault,
        secrets,
        env,
        workspace,
        agent_auth,
    })
}

fn build_inline_workspace(
    resolved: &Pillbox,
    opts: &RunOpts,
    workspace_host: &Path,
    action: &'static str,
) -> Result<InlineWorkspace> {
    let backend = resolved.workspace()?;
    let s3 = match &backend.variant {
        RusticVariant::S3(cfg) => cfg.clone(),
        RusticVariant::Local { .. } => {
            return Err(PillboxError::usage(
                action,
                "remote runs require an S3-shaped workspace backend",
            )
            .into());
        }
    };
    let repo_password = fs::read_to_string(&backend.password_file)
        .with_context(|| format!("read {}", backend.password_file.display()))?
        .trim_end()
        .to_string();
    let base_snapshot = resolve_base_snapshot(resolved, opts, &backend, workspace_host)?;
    Ok(InlineWorkspace {
        s3,
        repo_password,
        base_snapshot,
    })
}

/// The snapshot a remote run forks from: an explicit `--from-bookmark`,
/// or — by default — a fresh snapshot of the current workspace pushed to
/// the shared repo. The default branch performs a `push` (a write), so
/// this is kept separate from the credential-gathering above.
fn resolve_base_snapshot(
    resolved: &Pillbox,
    opts: &RunOpts,
    backend: &RusticBackend,
    workspace_host: &Path,
) -> Result<Option<String>> {
    let handle = match opts.from_bookmark.as_deref() {
        Some(name) => crate::bookmarks::resolve_existing(resolved, name)?,
        None => {
            backend
                .push(
                    workspace_host,
                    PushOptions {
                        tag: Some("remote-base".into()),
                        message: Some("remote run base".into()),
                    },
                )?
                .handle
        }
    };
    Ok(Some(handle.as_str().to_string()))
}

/// `pillbox run --vault-stdin` entry point — invoked by the local pillbox
/// over SSH (and by the e2b sandbox-side wrapper). Reads the blob (from
/// `blob_file` when set, else stdin), provisions a vault session for
/// vaulted secrets, then runs the agent under the existing LocalDocker
/// sandbox path. This is the **remote** half of the protocol — unchanged
/// by phase 4, so the pty-host transport reuses the full workspace /
/// vault / docker flow verbatim.
///
/// `blob_file` is set by the ssh pty-host transport ([`launch_pty_host`]):
/// the inner `docker run -it` needs a TTY on stdin, so the blob can't ride
/// stdin there — the launch path stages it to a remote temp file and
/// points `--blob-file` at it. The e2b wrapper still pipes the blob
/// through stdin (`< blob`), so that path passes `None`.
pub(crate) fn dispatch_vault_stdin(resolved: &Pillbox, blob_file: Option<&Path>) -> Result<()> {
    let buf = match blob_file {
        Some(path) => fs::read(path).map_err(|e| {
            PillboxError::runtime(
                "run --vault-stdin",
                format!("read blob file {}: {e}", path.display()),
            )
        })?,
        None => {
            let mut buf = Vec::new();
            std::io::stdin().read_to_end(&mut buf).map_err(|e| {
                PillboxError::runtime("run --vault-stdin", format!("read stdin: {e}"))
            })?;
            buf
        }
    };
    let blob = VaultStdinBlob::from_bytes(&buf)?;

    let spec = crate::agents::ALL
        .iter()
        .copied()
        .find(|s| s.id() == blob.agent_id)
        .ok_or_else(|| {
            PillboxError::usage(
                "run --vault-stdin",
                format!("unknown agent `{}` in blob", blob.agent_id),
            )
        })?;

    let runner_image = docker::check_ready_for(resolved)?;

    let home = spec.home_dir(resolved)?;
    if !home.join(spec.cred_sentinel).exists() {
        return Err(PillboxError::runtime(
            "run --vault-stdin",
            format!("no stored credentials for `{}` on this remote", spec.id),
        )
        .with_next(format!(
            "pillbox auth login --agent {}  # run this on the REMOTE host",
            spec.id
        ))
        .into());
    }

    let remote_workspace = hydrate_remote_workspace(&blob.workspace)?;
    let workspace_host = remote_workspace.workspace_dir.clone();
    let guest_workspace = format!("{GUEST_WORKSPACE}/{}", blob.workspace_mount_name);

    let any_vaulted = blob.secrets.iter().any(|s| s.vault_meta.is_some());
    let mut vault_session = if blob.vault || any_vaulted {
        let oauth = if blob.vault {
            Some(OAuthAgent {
                agent_id: spec.id,
                agent_home: &home,
            })
        } else {
            None
        };
        Some(VaultSession::start(oauth, resolved)?)
    } else {
        None
    };

    // Resolve --with entries using the inline real values instead of the
    // local secrets store. Vaulted entries are leased through the local
    // vault session (the stub never leaves this host).
    let mut env = blob.env.clone();
    for s in &blob.secrets {
        let injected = match (&s.vault_meta, vault_session.as_mut()) {
            (Some(meta), Some(session)) => session.lease_api_key(&s.name, &s.value, meta)?,
            (Some(_), None) => {
                return Err(PillboxError::runtime(
                    "run --vault-stdin",
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

    let mut args = base_docker_args();
    args.extend([
        "-v".into(),
        format!("{}:{GUEST_HOME}", home.display()),
        "-v".into(),
        format!("{}:{guest_workspace}", workspace_host.display()),
        "-w".into(),
        guest_workspace,
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
    args.extend(blob.agent_args.clone());

    let status = docker::run_interactive(&args)?;
    let result_snapshot = remote_workspace.backend.push(
        &workspace_host,
        PushOptions {
            tag: Some("remote-result".into()),
            message: Some("remote run result".into()),
        },
    )?;
    drop(vault_session);
    if let Some(path) = std::env::var_os(RESULT_SNAPSHOT_FILE_ENV) {
        fs::write(&path, result_snapshot.handle.as_str()).map_err(|e| {
            PillboxError::runtime(
                "run --vault-stdin",
                format!("write {}: {e}", PathBuf::from(path).display()),
            )
        })?;
    }
    eprintln!(
        "pillbox: result snapshot {}",
        result_snapshot.handle.short()
    );
    if !status.success() {
        return Err(PillboxError::runtime(
            "run --vault-stdin",
            format!("{} exited with status {status}", spec.id),
        )
        .into());
    }
    Ok(())
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
    // Unused: the direct path's state lives entirely in the blob (auth +
    // workspace creds + env). Kept symmetric with `dispatch_vault_stdin`'s
    // signature so the CLI dispatch site stays uniform.
    _resolved: &Pillbox,
    blob_file: Option<&Path>,
) -> Result<()> {
    let action: &'static str = "run --vault-stdin-direct";
    let buf = match blob_file {
        Some(path) => fs::read(path).map_err(|e| {
            PillboxError::runtime(action, format!("read blob file {}: {e}", path.display()))
        })?,
        None => {
            let mut buf = Vec::new();
            std::io::stdin()
                .read_to_end(&mut buf)
                .map_err(|e| PillboxError::runtime(action, format!("read stdin: {e}")))?;
            buf
        }
    };
    let blob = VaultStdinBlob::from_bytes(&buf)?;

    let spec = crate::agents::ALL
        .iter()
        .copied()
        .find(|s| s.id() == blob.agent_id)
        .ok_or_else(|| {
            PillboxError::usage(action, format!("unknown agent `{}` in blob", blob.agent_id))
        })?;

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

    // Vault stub-swap is host-side; deferring its sandbox port to a follow-up.
    let any_vaulted = blob.secrets.iter().any(|s| s.vault_meta.is_some());
    if blob.vault || any_vaulted {
        return Err(PillboxError::usage(
            action,
            "--vault / vaulted --with not yet supported on the direct sandbox path",
        )
        .with_next("rerun without --vault for now; the e2b vault path lands in a follow-up")
        .into());
    }

    // Resolve $HOME inside the sandbox; auth files write under it and the
    // agent inherits it on exec. Required (e2b sandboxes set HOME for the
    // run user); refuse if absent rather than guess.
    let home_env = std::env::var("HOME").map_err(|_| {
        PillboxError::runtime(action, "HOME is not set in the sandbox environment")
    })?;
    let home_dir = PathBuf::from(&home_env);
    materialize_agent_auth(&home_dir, &blob.agent_auth, action)?;

    let remote_workspace = hydrate_remote_workspace(&blob.workspace)?;
    let workspace_host = remote_workspace.workspace_dir.clone();

    // Layer blob env on top of the sandbox's existing env (HOME, PATH, USER,
    // LANG, TERM, …). Inheriting matches `docker run`'s practical behavior
    // (docker provides those defaults plus `-e` overrides) and gives the
    // agent the normal shell-shaped env it expects. The sandbox is already
    // the isolation boundary; there's no host env to keep out.
    let mut env = blob.env.clone();
    for s in &blob.secrets {
        env.insert(s.env_var.clone(), s.value.clone());
    }

    let mut cmd = std::process::Command::new(spec.run_argv[0]);
    if spec.run_argv.len() > 1 {
        cmd.args(&spec.run_argv[1..]);
    }
    cmd.args(&blob.agent_args);
    cmd.current_dir(&workspace_host);
    for (k, v) in &env {
        cmd.env(k, v);
    }
    let status = cmd
        .status()
        .map_err(|e| PillboxError::runtime(action, format!("spawn `{}`: {e}", spec.id)))?;

    // Push the (possibly mutated) workspace back regardless of agent exit
    // status — even a failed run is worth capturing so the host can pull
    // partial results. Mirrors `dispatch_vault_stdin`.
    let result_snapshot = remote_workspace.backend.push(
        &workspace_host,
        PushOptions {
            tag: Some("remote-result".into()),
            message: Some("remote run result".into()),
        },
    )?;
    if let Some(path) = std::env::var_os(RESULT_SNAPSHOT_FILE_ENV) {
        fs::write(&path, result_snapshot.handle.as_str()).map_err(|e| {
            PillboxError::runtime(action, format!("write {}: {e}", PathBuf::from(path).display()))
        })?;
    }
    eprintln!(
        "pillbox: result snapshot {}",
        result_snapshot.handle.short()
    );
    if !status.success() {
        return Err(
            PillboxError::runtime(action, format!("{} exited with status {status}", spec.id))
                .into(),
        );
    }
    Ok(())
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
            fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
                .with_context(|| format!("chmod 0o700 {}", parent.display()))?;
        }
        fs::write(&target, &f.contents)
            .with_context(|| format!("write {}", target.display()))?;
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
    use crate::vault::HeaderScheme;

    fn test_workspace() -> InlineWorkspace {
        InlineWorkspace {
            s3: S3Config {
                endpoint: "https://r2.example".into(),
                region: "auto".into(),
                bucket: "bucket".into(),
                prefix: "pillbox/".into(),
                access_key: "access".into(),
                secret_key: "secret".into(),
            },
            repo_password: "repo-password".into(),
            base_snapshot: Some("a".repeat(64)),
        }
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
        let w = build_wrapper(&rs, "abcdef012345", None, None);
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
        let line = build_launch_line(&rs, "abcdef012345", None, None);
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
        );
        assert!(w.contains("export PILLBOX_EVENTS_WEBHOOK='https://hook.example/e'"));
        assert!(w.contains("export PILLBOX_PARENT_SESSION_ID='00112233'"));
    }

    #[test]
    fn build_wrapper_quotes_a_hostile_webhook() {
        // Belt-and-suspenders: the host validates the URL shape, but if a
        // `'` ever slipped through it must stay quoted inside the wrapper,
        // not break out into a second command.
        let rs = RemoteSession::new("0011aabbccdd");
        let w = build_wrapper(&rs, "0011aabbccdd", Some("h'; rm -rf /; '"), None);
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
    fn blob_round_trips_through_json() {
        let blob = VaultStdinBlob {
            version: BLOB_VERSION,
            agent_id: "claude".into(),
            agent_args: vec!["--continue".into()],
            workspace_mount_name: "my-app".into(),
            vault: true,
            secrets: vec![
                InlineSecret {
                    name: "PLAIN".into(),
                    env_var: "PLAIN".into(),
                    value: "raw-value".into(),
                    vault_meta: None,
                },
                InlineSecret {
                    name: "ANTHROPIC_API_KEY".into(),
                    env_var: "ANTHROPIC_API_KEY".into(),
                    value: "sk-ant-api03-REAL".into(),
                    vault_meta: Some(VaultMeta::new(
                        "api.anthropic.com".into(),
                        HeaderScheme::XApiKey,
                        "sk-ant-api03-".into(),
                    )),
                },
            ],
            env: [("LOG_LEVEL".to_string(), "debug".to_string())]
                .into_iter()
                .collect(),
            workspace: test_workspace(),
            agent_auth: vec![AuthFile {
                rel_path: ".claude/.credentials.json".into(),
                mode: 0o600,
                contents: br#"{"oauth_token":"redacted"}"#.to_vec(),
            }],
        };
        let bytes = blob.to_bytes().unwrap();
        let back = VaultStdinBlob::from_bytes(&bytes).unwrap();
        assert_eq!(back.version, BLOB_VERSION);
        assert_eq!(back.agent_id, "claude");
        assert_eq!(back.agent_auth.len(), 1);
        assert_eq!(back.agent_auth[0].rel_path, ".claude/.credentials.json");
        assert_eq!(back.agent_auth[0].contents, br#"{"oauth_token":"redacted"}"#);
        assert_eq!(back.agent_args, vec!["--continue"]);
        assert_eq!(back.workspace_mount_name, "my-app");
        assert!(back.vault);
        assert_eq!(back.secrets.len(), 2);
        assert!(back.secrets[0].vault_meta.is_none());
        assert!(back.secrets[1].vault_meta.is_some());
        assert_eq!(back.env.get("LOG_LEVEL").map(String::as_str), Some("debug"));
        assert_eq!(back.workspace.s3.bucket, "bucket");
        assert_eq!(
            back.workspace.base_snapshot.as_deref().map(str::len),
            Some(64)
        );
    }

    #[test]
    fn blob_rejects_invalid_json() {
        let err = VaultStdinBlob::from_bytes(b"not json").unwrap_err();
        assert!(format!("{err}").contains("vault-stdin"));
    }

    #[test]
    fn blob_ignores_unknown_fields() {
        // Forward compatibility: a newer pillbox writing extra fields
        // shouldn't break older deserialize paths.
        let raw = serde_json::json!({
            "version": BLOB_VERSION,
            "agent_id": "claude",
            "agent_args": [],
            "workspace_mount_name": "x",
            "vault": false,
            "secrets": [],
            "env": {},
            "workspace": test_workspace(),
            "future_field": {"nested": true},
        });
        let bytes = serde_json::to_vec(&raw).unwrap();
        let back = VaultStdinBlob::from_bytes(&bytes).unwrap();
        assert_eq!(back.agent_id, "claude");
    }

    #[test]
    fn blob_defaults_for_missing_optional_fields() {
        // A minimal blob (no secrets, no env, no agent_args) should
        // deserialize cleanly so trivial agent runs don't need to write
        // empty arrays/objects.
        let raw = serde_json::json!({
            "version": BLOB_VERSION,
            "agent_id": "codex",
            "workspace_mount_name": "p",
            "workspace": test_workspace(),
        });
        let bytes = serde_json::to_vec(&raw).unwrap();
        let back = VaultStdinBlob::from_bytes(&bytes).unwrap();
        assert_eq!(back.agent_id, "codex");
        assert!(back.secrets.is_empty());
        assert!(back.env.is_empty());
        assert!(back.agent_args.is_empty());
        assert!(!back.vault);
    }
}
