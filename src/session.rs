//! Session registry — durable handles for `pillbox run --detach`
//! runs that the user wants to reattach to later.
//!
//! ## Storage
//!
//! One TOML file per session at `<pillbox>/sessions/<id>.toml`. IDs are
//! 12 hex chars (48 bits of entropy via `rand`), short enough to type
//! and unique enough at per-pillbox scale.
//!
//! Sessions are NOT inherited (project ↔ global). Unlike remotes /
//! secrets / env bundles, a session is concrete runtime state tied to
//! the pillbox that started it — surfacing global sessions from inside
//! a project pillbox would mislead the user about which workspace is
//! mounted in the running sandbox.
//!
//! ## Backend coverage
//!
//! The local `docker` and `libkrun` backends mint sessions today. They
//! reattach through the same shared attach transport (frame protocol +
//! pump): docker over `docker exec`, libkrun over the persistent attach
//! socket. The `backend` string drives dispatch in `commands::session`.
//!
//! ## Threat model
//!
//! Session records are stored under `~/.pillbox/.../sessions/<id>.toml`,
//! which is parented by the per-pillbox state dir (0700, uid-owned).
//! Co-tenants on the same machine cannot read or write them.
//!
//! The fields, by sensitivity:
//!   - `sandbox_id`: an opaque local backend handle (a Docker container
//!     id, or a libkrun attach-socket path + VMM pid). Not a secret;
//!     low-confidentiality config, not credential material.
//!   - `attached_pid`: a local process id; not a secret. See the
//!     hardening in `main.rs::session_detach` for why we still
//!     validate it (pid 0/1/-1/self are rejected, ESRCH is treated
//!     as already-detached).
//!   - `backend`, `agent_id`, `started_at`, `label`:
//!     user-supplied or derived metadata.
//!
//! No credentials live in this file. The vault path stays where
//! credentials belong (encrypted by [`crate::vault`]).
//!
//! ## Concurrency
//!
//! [`mark_attached`] / [`mark_detached`] are read-modify-write on a
//! single file with no lock. Two pillboxes racing to attach the same
//! session is a UX bug, not a security one (both are user-owned).
//! The losing writer's `attached_pid` is dropped; `session detach`
//! will only signal whichever pid landed last. Detect by reading the
//! file after attach if you need certainty.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::contract::RequestedRunProfile;
use crate::errors::PillboxError;
use crate::paths::ensure_mode_0700;
use crate::pillbox::Pillbox;
use crate::registry::{self as reg, IdRegistry, Registry};

/// Subdirectory under a pillbox's state dir holding session records.
/// Mirrors `remotes/` and `secrets/` — one file per record, easy to
/// grep, easy to `rm -f` if something goes sideways. Module-private;
/// callers go through `list` / `read` / `write` / `delete`.
const SESSIONS_DIR: &str = "sessions";

/// The per-session state directory `<pillbox>/sessions/<id>/`, created 0700.
/// Owns the session-id level of the storage layout — the `<id>.toml` record
/// (written by the registry) is its sibling, and the durable event log
/// ([`crate::events::log`]) lives under here. Centralizing it here keeps the
/// session-storage layout in the module that owns the session concept, rather
/// than having callers reach into `sessions/` directly.
pub(crate) fn session_dir(pb: &Pillbox, id: &str) -> Result<PathBuf> {
    let dir = pb.subdir(SESSIONS_DIR)?.join(id);
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    ensure_mode_0700(&dir)?;
    Ok(dir)
}

/// Read-only path to a session's directory — no `mkdir`, no `chmod` (the
/// counterpart to [`session_dir`]). Read commands that fold a session's log
/// for status (`session list` / `diagnose`) use this so a `list` never writes.
pub(crate) fn session_dir_path(pb: &Pillbox, id: &str) -> PathBuf {
    pb.subdir_path(SESSIONS_DIR).join(id)
}

/// Read-only path to the sessions root `<pillbox>/sessions/` (no mkdir). The post-run `--memory`
/// capture scans it for the run's freshly-written log(s) — see [`crate::memory::capture_run`].
pub(crate) fn sessions_root_path(pb: &Pillbox) -> PathBuf {
    pb.subdir_path(SESSIONS_DIR)
}

/// Absolute path of a session's record file — exactly where [`write`] persists it
/// (`<pillbox>/sessions/<id>.toml`, the path the `SessionRegistry` composes). The
/// libkrun commit-guard watches this path: its existence is the "launch committed"
/// signal that keeps a detached VMM from self-destructing. Pinned to `write`'s
/// real path by a test (`record_path_matches_write`).
///
/// libkrun-only: its sole caller is the commit-guard wiring (`arm_commit_guard`),
/// feature-gated — so without the feature this would be dead code (a `-D warnings`
/// build error).
#[cfg(feature = "libkrun")]
pub(crate) fn record_path(pb: &Pillbox, id: &str) -> PathBuf {
    sessions_root_path(pb).join(format!("{id}.toml"))
}

/// Registry plumbing for sessions. No-inheritance — a session is
/// concrete runtime state tied to the pillbox that started it, so we
/// implement [`Registry`] but not `InheritedRegistry`.
struct SessionRegistry;
impl Registry for SessionRegistry {
    type Record = Session;
    const SUBDIR: &'static str = SESSIONS_DIR;
    fn read_action() -> &'static str {
        "session read"
    }
    fn filename(name: &str) -> String {
        format!("{name}.toml")
    }
    fn parse(raw: &str, source: &Path) -> Result<Self::Record> {
        toml::from_str(raw).map_err(|e| {
            PillboxError::config("session read", format!("{}: {e}", source.display())).into()
        })
    }
}
impl IdRegistry for SessionRegistry {
    const ENTITY: &'static str = "session";
    fn record_id(record: &Session) -> &str {
        &record.id
    }
}

/// Backend label written into the session record. Kept as a string
/// (not an enum) on disk so a future binary that adds a backend can
/// still read older sessions. Dispatch goes through [`Backend::parse`]
/// so callers can match on a typed enum without resurrecting the
/// stringly path each time.
pub(crate) const BACKEND_DOCKER: &str = "docker";
pub(crate) const BACKEND_LIBKRUN: &str = "libkrun";
pub(crate) const BACKEND_MANAGED: &str = "managed";

/// Typed view of the on-disk `backend` string. Returned by
/// [`Backend::parse`]; `None` means a backend label this binary doesn't
/// know about (older or hand-edited record). Callers report the raw
/// label in the error so the user can grep for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Backend {
    /// Local Docker — a detached `pillbox run --detach` against the host
    /// daemon. `sandbox_id` is the container id; attach/rm act on the local
    /// daemon directly.
    Docker,
    /// Local libkrun microVM — a detached `pillbox run --detach` (feature-gated
    /// `libkrun`). `sandbox_id` carries the persistent attach socket path + the
    /// VMM child PID; attach dials that socket, rm kills the child + scrubs the
    /// CoW clones. Detach keeps the vault (the MITM lives in the child, not the
    /// parent — unlike local Docker, which can't keep a host-side proxy alive).
    Libkrun,
    /// Managed Cloudflare tier — the session runs on a CF container placed by the
    /// §0-gateway Durable Object, not on this host. `sandbox_id` carries the DO
    /// base URL + the DO-side session id (a JSON [`ManagedHandle`]); there's no
    /// local process. Drive (`send`) and read (`subscribe`/`watch`) go over the
    /// DO's HTTP/WebSocket surface, so attach/teardown route to the DO, never a
    /// local container/VM. See docs/managed-tier.md + [`crate::sandbox::managed`].
    Managed,
}

impl Backend {
    pub(crate) fn parse(label: &str) -> Option<Self> {
        match label {
            BACKEND_DOCKER => Some(Backend::Docker),
            BACKEND_LIBKRUN => Some(Backend::Libkrun),
            BACKEND_MANAGED => Some(Backend::Managed),
            _ => None,
        }
    }
}

/// Where a session physically runs — the dispatch axis attach/reattach/kill key
/// off (docs/managed-tier.md §`Session` gains a `placement`). A *real* axis, not
/// display sugar: a `Managed` session has no local sandbox to reach, so the plane
/// routes its verbs to the §0 gateway DO instead of a local container/VM. Stored
/// as a string (not the [`Backend`] label) so a future placement that reuses a
/// transport — or an old record that predates the field — round-trips cleanly.
/// `Local` is the default for every record written before this field existed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Placement {
    /// On this host — a Docker container or a libkrun microVM. The historical
    /// (and default) case; attach/kill act on the local daemon/VMM directly.
    #[default]
    Local,
    /// On the managed Cloudflare tier — a CF container behind the §0-gateway DO.
    /// Attach = re-subscribe to the durable DO log (the session is durable
    /// server-side); there is no local process to signal or tear down.
    Managed,
}

/// On-disk shape. Forward-compatible: serde will ignore unknown fields
/// so a future binary writing extra metadata (e.g. a "last seen"
/// timestamp) doesn't break older readers.
// No `Eq`: `ServerSession.temperature` is an `f64` (PartialEq only).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct Session {
    /// Short hex id — 12 chars. Used as the registry filename and as
    /// the `pillbox session attach <id>` argument.
    pub(crate) id: String,
    /// Optional human label (`pillbox run --detach --label foo`).
    /// Surfaced in `session list` next to the id.
    #[serde(default)]
    pub(crate) label: Option<String>,
    /// Backend kind — one of [`BACKEND_DOCKER`], [`BACKEND_LIBKRUN`].
    pub(crate) backend: String,
    /// Opaque handle the backend uses to find this session again.
    /// For Docker: the container id. For libkrun: the persistent attach
    /// socket path + VMM child PID.
    pub(crate) sandbox_id: String,
    /// PTY process id inside the backend. For Docker: 0 — the relay finds
    /// the pty-host by socket path, not pid.
    #[serde(default)]
    pub(crate) pty_pid: i64,
    /// Agent that's running inside (`claude` | `codex` | …).
    pub(crate) agent_id: String,
    /// RFC3339 timestamp from `time::OffsetDateTime::now_utc()`.
    pub(crate) started_at: String,
    /// PID of a currently-attached `pillbox` process, or `None` if the
    /// session is detached. Set by [`mark_attached`] when a pillbox
    /// starts streaming, cleared by [`mark_detached`] on exit.
    /// `pillbox session detach <id>` sends `SIGTERM` to this pid.
    #[serde(default)]
    pub(crate) attached_pid: Option<i64>,
    /// Workspace snapshot the session forked from. Captured at session
    /// create time (the latest snapshot in the pillbox's rustic repo).
    /// `None` if the workspace had no snapshots yet — first run against
    /// an empty repo. Used by `session diff` (PR 1b) to compute what
    /// the agent changed relative to its starting point.
    #[serde(default)]
    pub(crate) base_snapshot: Option<String>,
    /// Workspace snapshot of the agent's result, captured by the
    /// in-sandbox wrapper after the agent exits and passed to
    /// `pillbox session done --result-snapshot HANDLE`. `None` until
    /// the session finishes (or never set for runs that crashed
    /// before the wrapper could push). `pillbox session pull <id>`
    /// rehydrates this snapshot for post-mortem inspection.
    #[serde(default)]
    pub(crate) result_snapshot: Option<String>,
    /// Absolute RFC3339 timestamp after which `pillbox session prune`
    /// will tear this session down. Set by `pillbox run --ttl
    /// DURATION` at spawn time so per-session retention intent is
    /// captured at creation (different sessions can have different
    /// TTLs — failed experiments 1h, prod runs 7d, etc.). `None`
    /// means no TTL: the session lives until explicit `session rm`.
    /// Pillbox does NOT auto-prune on every invocation; `session
    /// prune` is the explicit one-shot the user/orchestrator
    /// schedules.
    #[serde(default)]
    pub(crate) expires_at: Option<String>,
    /// The agent's guest working directory (`/workspace/<name>`) — the project
    /// key its transcript is written under. Lets `session subscribe` locate and
    /// tail a live session's transcript into the durable log while it serves
    /// (so a driven detached session is also readable). Empty for records that
    /// predate the field.
    #[serde(default)]
    pub(crate) guest_cwd: String,
    /// Where this session physically runs (host-local vs the managed CF tier) —
    /// the dispatch axis attach/reattach/kill route on. `Local` (the default)
    /// for every record written before the managed tier existed, so old records
    /// deserialize unchanged. A scalar string in TOML, so it sits above `server`
    /// (a table) with the other scalars.
    #[serde(default)]
    pub(crate) placement: Placement,
    /// Server-integration (opencode) state — `Some` iff the agent is a `Server`
    /// integration, `None` for PTY agents (claude/codex). Grouped so a PTY record
    /// can't carry a half-populated `(agent_session_id, model)` tail.
    ///
    /// No scalar field may follow this table: TOML would parse it into
    /// `[server]`. New scalar fields go above; new table/struct fields go below.
    #[serde(default)]
    pub(crate) server: Option<ServerSession>,
    /// Harness-neutral request persisted before execution. Kept separate from
    /// `ServerSession`, which is agent-native transport state, so Pi and future
    /// structured non-server adapters can share the same evidence contract.
    #[serde(default)]
    pub(crate) requested_execution: Option<RequestedRunProfile>,
}

/// The agent-native state a `Server`-integration agent (opencode) needs to be
/// driven/read over its HTTP API — both fields are always set together.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ServerSession {
    /// The agent-native session id its HTTP API uses (`ses_…`), distinct from
    /// this record's pillbox id. `session send` (→ POST `/prompt_async`) and the
    /// event bridge target it.
    pub(crate) agent_session_id: String,
    /// The `providerID/modelID` to drive with (resolved from `--model` or a
    /// default at run time, reused by every `session send`).
    pub(crate) model: String,
    /// Sampling temperature (`--temperature`) sent on every `session send`.
    /// `None` → the model/provider default. `Some(0.0)` = greedy decoding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) temperature: Option<f64>,
}

impl Session {
    /// Mint a new 12-hex-char id. 48 bits is plenty at per-pillbox
    /// scale; collisions across a user's lifetime of sessions are
    /// astronomically unlikely.
    pub(crate) fn new_id() -> String {
        reg::new_id()
    }

    // `stub_from_id` removed — `emit_session_event` now takes
    // `Option<&Session>` directly. Sandbox-side callers pass `None`;
    // host-side callers pass the record. Empty-strings-as-stub-flag
    // was a typed-state smell.
    /// Fixed-shape test fixture — same `Session` every call so tests
    /// across modules (`session`, `events`, future consumers) agree
    /// on the field values they're asserting against. Override fields
    /// after construction for test-specific shapes:
    ///
    /// ```ignore
    /// let mut s = Session::test_fixture();
    /// s.backend = BACKEND_LIBKRUN.into();
    /// ```
    ///
    /// Kept `#[cfg(test)]` to keep the production binary slim.
    #[cfg(test)]
    pub(crate) fn test_fixture() -> Self {
        Self {
            id: "abc123def456".to_string(),
            label: Some("test".to_string()),
            backend: BACKEND_DOCKER.to_string(),
            sandbox_id: "sb_test".to_string(),
            pty_pid: 0,
            agent_id: "claude".to_string(),
            started_at: "2026-05-23T13:37:00Z".to_string(),
            attached_pid: None,
            base_snapshot: None,
            result_snapshot: None,
            expires_at: None,
            guest_cwd: String::new(),
            placement: Placement::Local,
            server: None,
            requested_execution: None,
        }
    }

    /// Stable JSON shape used by both `session list` (as an array
    /// element) and `session info`. Keep this method (not raw serde
    /// derive) as the single point of truth for the on-wire field set
    /// so `--json` output stays stable as the on-disk struct evolves.
    /// `label` is omitted when `None` to match the existing
    /// `remote info` convention; `attached_pid` is always present
    /// (null when detached) so consumers can branch on it.
    pub(crate) fn to_json_value(&self) -> serde_json::Value {
        let mut o = serde_json::Map::new();
        o.insert("id".into(), self.id.clone().into());
        if let Some(label) = &self.label {
            o.insert("label".into(), label.clone().into());
        }
        o.insert("backend".into(), self.backend.clone().into());
        // `placement` tells a consumer whether the session runs locally or on the
        // managed tier — the axis `session attach`/`rm` route on. Always present
        // (defaults to `local`) so an orchestrator can branch on it.
        o.insert(
            "placement".into(),
            serde_json::to_value(self.placement)
                .unwrap_or_else(|_| serde_json::Value::String("local".into())),
        );
        o.insert("sandbox_id".into(), self.sandbox_id.clone().into());
        o.insert("pty_pid".into(), self.pty_pid.into());
        o.insert("agent_id".into(), self.agent_id.clone().into());
        o.insert("started_at".into(), self.started_at.clone().into());
        o.insert(
            "attached_pid".into(),
            match self.attached_pid {
                Some(p) => serde_json::Value::from(p),
                None => serde_json::Value::Null,
            },
        );
        o.insert(
            "base_snapshot".into(),
            self.base_snapshot
                .clone()
                .map(serde_json::Value::String)
                .unwrap_or(serde_json::Value::Null),
        );
        o.insert(
            "result_snapshot".into(),
            self.result_snapshot
                .clone()
                .map(serde_json::Value::String)
                .unwrap_or(serde_json::Value::Null),
        );
        o.insert(
            "expires_at".into(),
            self.expires_at
                .clone()
                .map(serde_json::Value::String)
                .unwrap_or(serde_json::Value::Null),
        );
        if self.server.is_some() || self.requested_execution.is_some() {
            let requested = self
                .requested_execution
                .as_ref()
                .map(|profile| serde_json::to_value(profile).expect("requested profile serializes"))
                .unwrap_or_else(|| {
                    serde_json::json!({
                        "status": "unavailable",
                        "reason": "legacy_record",
                    })
                });
            o.insert(
                "execution".into(),
                serde_json::json!({
                    "requested": requested,
                    "served_model": {
                        "status": "unavailable",
                        "reason": "not_reported",
                        "source": { "session_id": self.id },
                    },
                    "effective_limits": {
                        "status": "unavailable",
                        "reason": "not_reported",
                        "source": { "session_id": self.id },
                    },
                }),
            );
        }
        serde_json::Value::Object(o)
    }
}

/// Emit the `pillbox run --json` start envelope — the one pinned
/// `{version:1, session:{…}}` shape every backend's `--json` start path must
/// agree on (docker detach, libkrun detach, and the server-mode bring-up).
/// The human banner stays per-backend (it differs by lifecycle); only this
/// machine surface is shared, so the schema can't drift between backends.
pub(crate) fn print_started_json(session: &Session) {
    println!(
        "{}",
        crate::paths::json_v1(vec![("session", session.to_json_value())])
    );
}

/// Persist a session record. Used by both detached-start (writes the
/// initial record) and attach (updates `attached_pid`).
pub(crate) fn write(pb: &Pillbox, session: &Session) -> Result<()> {
    let body = toml::to_string(session)
        .map_err(|e| PillboxError::config("session write", e.to_string()))?;
    reg::write_record::<SessionRegistry>(pb, &session.id, body.as_bytes())
}

/// Read by exact id. Returns `Ok(None)` for missing records (callers
/// usually want `resolve` instead, which accepts prefixes).
pub(crate) fn read(pb: &Pillbox, id: &str) -> Result<Option<Session>> {
    SessionRegistry::read_one(pb, id)
}

impl Session {
    /// Minimal stub for sandbox-side event emission. The full record
    /// lives host-side in the registry; the sandbox only owns `id` +
    /// its own wall-clock `started_at` (the cold-start latency
    /// signal). All other fields are empty/None — consumers correlate
    /// to the host's `session.started` via `id`.
    ///
    /// Single chokepoint so adding a new Session field doesn't require
    /// hunting down every "I need a stub here" call site.
    pub(crate) fn sandbox_stub(id: &str) -> Self {
        Self {
            id: id.to_string(),
            label: None,
            backend: String::new(),
            sandbox_id: String::new(),
            pty_pid: 0,
            agent_id: String::new(),
            started_at: now_rfc3339(),
            attached_pid: None,
            base_snapshot: None,
            result_snapshot: None,
            expires_at: None,
            guest_cwd: String::new(),
            placement: Placement::Local,
            server: None,
            requested_execution: None,
        }
    }
}

/// Resolve a user-typed id that may be a unique prefix (>=4 chars).
/// Mirrors `pillbox snapshot show HANDLE` ergonomics. Returns the full
/// session record; the caller can use `session.id` for any further
/// writes back to the registry.
pub(crate) fn resolve(pb: &Pillbox, id_or_prefix: &str) -> Result<Session> {
    reg::resolve_id::<SessionRegistry>(pb, id_or_prefix)
}

/// Resolve an id-or-prefix to a session that has a durable event log, scanning
/// the per-session log dirs (`sessions/<id>/log.jsonl`) rather than the `.toml`
/// registry: a foreground run writes a log but no record, so [`resolve`] can't
/// find it. Errors on no match or an ambiguous prefix. Min prefix length 4
/// (matches the registry) so a typo can't silently latch onto a session.
pub(crate) fn resolve_logged(pb: &Pillbox, id_or_prefix: &str) -> Result<String> {
    const MIN_PREFIX: usize = 4;
    if id_or_prefix.len() < MIN_PREFIX {
        return Err(PillboxError::usage(
            "session subscribe",
            format!("`{id_or_prefix}` is too short — use at least {MIN_PREFIX} characters"),
        )
        .into());
    }
    let dir = pb.subdir_path(SESSIONS_DIR);
    let mut matches: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if name.starts_with(id_or_prefix) && entry.path().join("log.jsonl").is_file() {
                matches.push(name);
            }
        }
    }
    match matches.len() {
        1 => Ok(matches.pop().unwrap()),
        0 => Err(PillboxError::runtime(
            "session subscribe",
            format!("no session with a durable log matches `{id_or_prefix}`"),
        )
        .with_next(format!("ls {}", dir.display()))
        .into()),
        n => Err(PillboxError::runtime(
            "session subscribe",
            format!("`{id_or_prefix}` is ambiguous — {n} sessions match"),
        )
        .into()),
    }
}

/// All sessions in the current pillbox, sorted by `started_at` (oldest
/// first so `session list` is stable across re-runs).
pub(crate) fn list(pb: &Pillbox) -> Result<Vec<Session>> {
    let mut out = reg::list_all::<SessionRegistry>(pb)?;
    out.sort_by(|a, b| a.started_at.cmp(&b.started_at));
    Ok(out)
}

/// Idempotent — `Ok(())` whether the record was present or not. The
/// caller is expected to have already torn down the backend resources
/// (sandbox kill, etc.) before scrubbing the record.
pub(crate) fn delete(pb: &Pillbox, id: &str) -> Result<()> {
    SessionRegistry::delete(pb, id).map(|_| ())
}

/// Stamp the currently-running pillbox's PID into the session record.
/// `pillbox session detach <id>` reads this to know what to SIGTERM.
pub(crate) fn mark_attached(pb: &Pillbox, id: &str, pid: i64) -> Result<()> {
    let mut s = read(pb, id)?.ok_or_else(|| {
        PillboxError::runtime("session attach", format!("session `{id}` not found"))
    })?;
    s.attached_pid = Some(pid);
    write(pb, &s)
}

/// Clear the attached-pid stamp. Called on clean exit (Ctrl-A+D, peer
/// closure, sandbox death). Failing to clear is non-fatal — the next
/// `attach` will overwrite it.
///
/// No-op (no write) when the record is already detached — `reattach`'s
/// cleanup fires this on every exit path, so the common case after a
/// clean detach is "already None on disk; skip the write". Avoids
/// touching the file on every helper exit just to set the same value.
pub(crate) fn mark_detached(pb: &Pillbox, id: &str) -> Result<()> {
    let mut s = match read(pb, id)? {
        Some(s) => s,
        None => return Ok(()),
    };
    if s.attached_pid.is_none() {
        return Ok(());
    }
    s.attached_pid = None;
    write(pb, &s)
}

/// Parse a `--ttl` duration like `30m`, `24h`, `7d`. Returns the
/// number of seconds. Rejects empty / negative / >365d / malformed
/// inputs. Hand-rolled because pillbox doesn't depend on
/// `humantime` and the surface is tiny.
pub(crate) fn parse_ttl_seconds(s: &str) -> Result<u64> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(PillboxError::usage("--ttl", "empty duration").into());
    }
    let (num_part, unit) =
        trimmed.split_at(trimmed.find(|c: char| c.is_alphabetic()).ok_or_else(|| {
            PillboxError::usage(
                "--ttl",
                format!("missing unit in `{s}` (use `30m`, `24h`, `7d`)"),
            )
        })?);
    // Strict-digit guard: `u64::from_str` accepts a leading `+`
    // (so `+30m` would silently parse as 30) and would obviously
    // accept other non-decimal noise if we ever switched parsers.
    // Reject anything that isn't a non-empty run of ASCII digits
    // up front so the contract is "digits + unit", not "whatever
    // FromStr happens to accept this week".
    if num_part.is_empty() || !num_part.bytes().all(|b| b.is_ascii_digit()) {
        return Err(
            PillboxError::usage("--ttl", format!("invalid number `{num_part}` in `{s}`")).into(),
        );
    }
    let n: u64 = num_part.parse().map_err(|_| {
        PillboxError::usage("--ttl", format!("invalid number `{num_part}` in `{s}`"))
    })?;
    if n == 0 {
        return Err(PillboxError::usage("--ttl", "duration must be > 0").into());
    }
    let secs_per_unit = match unit {
        "s" => 1u64,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 60 * 60 * 24,
        other => {
            return Err(PillboxError::usage(
                "--ttl",
                format!("unsupported unit `{other}` (use `s`, `m`, `h`, or `d`)"),
            )
            .into())
        }
    };
    let total = n.checked_mul(secs_per_unit).ok_or_else(|| {
        PillboxError::usage("--ttl", format!("`{s}` overflows duration arithmetic"))
    })?;
    // 365 days cap — sessions are runtime artifacts, not archival.
    // A higher cap would still work but anything past a year is
    // almost certainly a typo.
    let one_year = 60 * 60 * 24 * 365;
    if total > one_year {
        return Err(PillboxError::usage(
            "--ttl",
            format!("`{s}` exceeds 365d cap; use `session rm` for permanent records"),
        )
        .into());
    }
    Ok(total)
}

/// Compute an RFC3339 `expires_at` timestamp from a TTL in seconds,
/// based on the current wall clock. Separated from
/// [`parse_ttl_seconds`] so tests can hold time constant.
pub(crate) fn expires_at_from_ttl(ttl_seconds: u64) -> String {
    let expires = time::OffsetDateTime::now_utc() + time::Duration::seconds(ttl_seconds as i64);
    format_rfc3339(expires)
}

/// Classification of a session's `expires_at` field, computed against
/// the current wall clock. Exhaustive so callers can dispatch by match
/// rather than juggling two boolean predicates (`is_expired` +
/// `is_valid_expires_at`) that each re-parse the same string and
/// each forget the malformed case differently.
///
/// `'a` borrows the malformed string from the source record so the
/// warning emitter can surface the exact value without an alloc.
#[derive(Debug)]
pub(crate) enum ExpiryStatus<'a> {
    /// `expires_at` is absent. Session has no TTL; never expires.
    NotSet,
    /// `expires_at` parsed and is in the future (or equal-to-now,
    /// which we treat as "not yet" to avoid racing the wall clock).
    Active,
    /// `expires_at` parsed and is in the past. `session prune` will
    /// drop this record.
    Expired,
    /// `expires_at` is present but not parseable as RFC3339. Surfaced
    /// as a warning by `session prune`; the record is left in place
    /// so a corrupt timestamp doesn't silently drop user data.
    Malformed(&'a str),
}

impl Session {
    /// Single-pass classification of `expires_at` against the current
    /// wall clock. Used by `session prune` to dispatch all three
    /// terminal cases (warn / drop / leave alone) without re-parsing
    /// or scanning twice.
    pub(crate) fn expiry_status(&self) -> ExpiryStatus<'_> {
        use time::format_description::well_known::Rfc3339;
        let Some(exp) = &self.expires_at else {
            return ExpiryStatus::NotSet;
        };
        match time::OffsetDateTime::parse(exp, &Rfc3339) {
            Err(_) => ExpiryStatus::Malformed(exp),
            Ok(when) if when < time::OffsetDateTime::now_utc() => ExpiryStatus::Expired,
            Ok(_) => ExpiryStatus::Active,
        }
    }

    /// How pillbox drives/reads this session's agent — the dispatch axis for
    /// `send`/`subscribe`/`watch`. Derived from the agent registry (not a
    /// stored field: the integration is a property of the agent id, so storing
    /// it would duplicate derivable state and risk drift). An unknown agent id
    /// (registry change after the record was written) falls back to `Pty`, the
    /// conservative default — server-mode dispatch then fails loud at the
    /// transport rather than mis-driving a PTY as an HTTP server.
    pub(crate) fn integration(&self) -> crate::agents::Integration {
        crate::agents::lookup("session", &self.agent_id)
            .map(|spec| spec.integration)
            .unwrap_or(crate::agents::Integration::Pty)
    }
}

/// RFC3339 timestamp for the `started_at` field. Pulled into a function
/// so tests can stub it (today they just take whatever wall clock
/// gives them).
pub(crate) fn now_rfc3339() -> String {
    format_rfc3339(time::OffsetDateTime::now_utc())
}

/// Format an [`OffsetDateTime`] as RFC3339. Centralised so
/// `now_rfc3339`, `expires_at_from_ttl`, and any future timestamp
/// writers share one fallback policy on the (effectively
/// unreachable) format error.
fn format_rfc3339(dt: time::OffsetDateTime) -> String {
    use time::format_description::well_known::Rfc3339;
    dt.format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pillbox;
    use crate::test_util::with_isolated_home;
    use std::fs;

    fn make(backend: &str) -> Session {
        // Per-call overrides (id, backend, started_at) on top of the
        // shared `Session::test_fixture()`. Keeps the ambiguous-prefix /
        // list-ordering tests deterministic where the fixture's stable
        // fields wouldn't fit.
        let mut s = Session::test_fixture();
        s.id = Session::new_id();
        s.label = None;
        s.backend = backend.to_string();
        s.pty_pid = 42;
        s.started_at = now_rfc3339();
        s
    }

    #[test]
    fn new_id_is_12_hex_chars() {
        let id = Session::new_id();
        assert_eq!(id.len(), 12);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn placement_defaults_to_local_for_records_without_the_field() {
        // Backward-compat: a record written before `placement` existed (no
        // `placement` line) deserializes with `Placement::Local`, not an error.
        let pre_field = r#"
            id = "abc123def456"
            backend = "docker"
            sandbox_id = "container-1"
            agent_id = "claude"
            started_at = "2026-05-23T13:37:00Z"
        "#;
        let s: Session = toml::from_str(pre_field).expect("old record parses");
        assert_eq!(s.placement, Placement::Local);
        assert_eq!(s.requested_execution, None);
        assert!(s.to_json_value().get("execution").is_none());
    }

    #[test]
    fn model_profile_contract_legacy_server_record_is_explicitly_unavailable() {
        let legacy = r#"
            id = "abc123def456"
            backend = "libkrun"
            sandbox_id = "handle"
            agent_id = "opencode"
            started_at = "2026-05-23T13:37:00Z"

            [server]
            agent_session_id = "ses_native"
            model = "openai/gpt-5.6-luna"
        "#;
        let session: Session = toml::from_str(legacy).expect("legacy server record parses");
        assert_eq!(session.requested_execution, None);
        assert_eq!(
            session.to_json_value()["execution"]["requested"],
            serde_json::json!({
                "status": "unavailable",
                "reason": "legacy_record"
            })
        );
    }

    #[test]
    fn placement_round_trips_managed_through_toml() {
        let mut s = Session::test_fixture();
        s.backend = BACKEND_MANAGED.to_string();
        s.placement = Placement::Managed;
        let toml = toml::to_string(&s).unwrap();
        assert!(toml.contains(r#"placement = "managed""#), "{toml}");
        let back: Session = toml::from_str(&toml).unwrap();
        assert_eq!(back.placement, Placement::Managed);
        // `server` (a table) must still parse — `placement` is a scalar above it,
        // so the table-ordering invariant the struct doc warns about holds.
        assert_eq!(back.server, s.server);
    }

    #[test]
    fn server_session_json_exposes_requested_execution_profile() {
        let mut s = Session::test_fixture();
        s.agent_id = "opencode".into();
        s.server = Some(ServerSession {
            agent_session_id: "ses_native".into(),
            model: "openai/gpt-5.6-luna".into(),
            temperature: Some(0.2),
        });
        s.requested_execution = Some(
            RequestedRunProfile::parse(
                "openai/gpt-5.6-luna",
                Some("luna".into()),
                Some(crate::contract::ReasoningEffort::High),
            )
            .unwrap(),
        );

        let execution = &s.to_json_value()["execution"];
        assert_eq!(
            execution["requested"],
            serde_json::json!({
                "provider": "openai",
                "model": "gpt-5.6-luna",
                "profile": "luna",
                "reasoningEffort": "high"
            })
        );
        assert_eq!(
            execution["served_model"],
            serde_json::json!({
                "status": "unavailable",
                "reason": "not_reported",
                "source": { "session_id": "abc123def456" }
            })
        );
        assert!(execution.get("agent_session_id").is_none());
    }

    #[test]
    fn backend_parse_round_trips_managed() {
        assert_eq!(Backend::parse(BACKEND_MANAGED), Some(Backend::Managed));
    }

    #[test]
    fn write_and_read_round_trip() {
        with_isolated_home("session-rt", || {
            let g = pillbox::global();
            let s = make(BACKEND_DOCKER);
            write(&g, &s).unwrap();
            let back = read(&g, &s.id).unwrap().expect("present");
            assert_eq!(back.id, s.id);
            assert_eq!(back.backend, BACKEND_DOCKER);
            assert_eq!(back.pty_pid, 42);
            assert!(back.attached_pid.is_none());
        });
    }

    #[cfg(feature = "libkrun")]
    #[test]
    fn record_path_matches_write() {
        // The libkrun commit-guard self-destructs a detached VMM unless this exact
        // path exists — so it MUST be where `write` actually persists the record. A
        // drift here would make every launch look "abandoned" and kill itself.
        with_isolated_home("session-record-path", || {
            let g = pillbox::global();
            let s = make(BACKEND_LIBKRUN);
            let expected = record_path(&g, &s.id);
            assert!(!expected.exists(), "record must not exist before write");
            write(&g, &s).unwrap();
            assert!(
                expected.exists(),
                "record_path() must point at the file write() creates: {}",
                expected.display()
            );
        });
    }

    #[test]
    fn list_is_stable_by_started_at() {
        with_isolated_home("session-list", || {
            let g = pillbox::global();
            let mut a = make(BACKEND_DOCKER);
            a.started_at = "2026-01-01T00:00:00Z".into();
            let mut b = make(BACKEND_DOCKER);
            b.started_at = "2026-02-01T00:00:00Z".into();
            write(&g, &b).unwrap(); // write newer first
            write(&g, &a).unwrap();
            let all = list(&g).unwrap();
            assert_eq!(all.len(), 2);
            assert_eq!(all[0].started_at, "2026-01-01T00:00:00Z");
            assert_eq!(all[1].started_at, "2026-02-01T00:00:00Z");
        });
    }

    #[test]
    fn resolve_accepts_prefix() {
        with_isolated_home("session-resolve", || {
            let g = pillbox::global();
            let s = make(BACKEND_DOCKER);
            write(&g, &s).unwrap();
            let prefix = &s.id[..6];
            let found = resolve(&g, prefix).unwrap();
            assert_eq!(found.id, s.id);
        });
    }

    #[test]
    fn resolve_rejects_short_prefix() {
        with_isolated_home("session-short", || {
            let g = pillbox::global();
            let s = make(BACKEND_DOCKER);
            write(&g, &s).unwrap();
            let err = resolve(&g, "abc").unwrap_err().to_string();
            assert!(err.contains("too short"), "got: {err}");
        });
    }

    #[test]
    fn resolve_rejects_ambiguous_prefix() {
        with_isolated_home("session-ambig", || {
            let g = pillbox::global();
            // Mint two ids that collide on a known prefix to make the test
            // deterministic.
            let mut a = make(BACKEND_DOCKER);
            a.id = "abcdef000001".into();
            let mut b = make(BACKEND_DOCKER);
            b.id = "abcdef000002".into();
            write(&g, &a).unwrap();
            write(&g, &b).unwrap();
            let err = resolve(&g, "abcdef").unwrap_err().to_string();
            assert!(err.contains("matches 2"), "got: {err}");
            // Both candidate ids should be listed so the user can pick
            // one without re-running `session list`.
            assert!(err.contains("abcdef000001"), "got: {err}");
            assert!(err.contains("abcdef000002"), "got: {err}");
        });
    }

    #[test]
    fn mark_attached_then_detached_roundtrips() {
        with_isolated_home("session-attach", || {
            let g = pillbox::global();
            let s = make(BACKEND_DOCKER);
            write(&g, &s).unwrap();
            mark_attached(&g, &s.id, 12345).unwrap();
            assert_eq!(read(&g, &s.id).unwrap().unwrap().attached_pid, Some(12345));
            mark_detached(&g, &s.id).unwrap();
            assert_eq!(read(&g, &s.id).unwrap().unwrap().attached_pid, None);
        });
    }

    #[test]
    fn delete_is_idempotent() {
        with_isolated_home("session-del", || {
            let g = pillbox::global();
            let s = make(BACKEND_DOCKER);
            write(&g, &s).unwrap();
            delete(&g, &s.id).unwrap();
            assert!(read(&g, &s.id).unwrap().is_none());
            delete(&g, &s.id).unwrap();
        });
    }

    #[test]
    fn mark_detached_is_no_op_when_already_detached() {
        // `reattach`'s cleanup fires `mark_detached` on every exit path,
        // including the common case where the local pump has already
        // cleared the stamp. Re-writing the TOML to set the same
        // `attached_pid = None` is wasted work; pin the optimization
        // in place by checking mtime stability.
        with_isolated_home("session-detach-noop", || {
            let g = pillbox::global();
            let s = make(BACKEND_DOCKER);
            write(&g, &s).unwrap();
            let path = SessionRegistry::path_read(&g, &s.id);
            let mtime_a = fs::metadata(&path).unwrap().modified().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
            mark_detached(&g, &s.id).unwrap();
            let mtime_b = fs::metadata(&path).unwrap().modified().unwrap();
            assert_eq!(mtime_a, mtime_b, "mark_detached re-wrote a no-op");
        });
    }

    #[test]
    fn known_backend_labels_are_constants() {
        let _: &str = BACKEND_DOCKER;
        let _: &str = BACKEND_LIBKRUN;
        let _: &str = SESSIONS_DIR;
    }

    #[test]
    fn backend_parse_round_trips_known_labels() {
        assert_eq!(Backend::parse(BACKEND_DOCKER), Some(Backend::Docker));
        assert_eq!(Backend::parse(BACKEND_LIBKRUN), Some(Backend::Libkrun));
        assert_eq!(Backend::parse("nope"), None);
    }

    #[test]
    fn parse_ttl_accepts_supported_units() {
        assert_eq!(parse_ttl_seconds("30s").unwrap(), 30);
        assert_eq!(parse_ttl_seconds("5m").unwrap(), 300);
        assert_eq!(parse_ttl_seconds("2h").unwrap(), 7200);
        assert_eq!(parse_ttl_seconds("7d").unwrap(), 604_800);
    }

    #[test]
    fn parse_ttl_trims_whitespace() {
        assert_eq!(parse_ttl_seconds("  24h  ").unwrap(), 86_400);
    }

    #[test]
    fn parse_ttl_rejects_zero() {
        let err = parse_ttl_seconds("0h").unwrap_err().to_string();
        assert!(err.contains("> 0"), "got: {err}");
    }

    #[test]
    fn parse_ttl_rejects_missing_unit() {
        let err = parse_ttl_seconds("42").unwrap_err().to_string();
        assert!(err.contains("missing unit"), "got: {err}");
    }

    #[test]
    fn parse_ttl_rejects_unknown_unit() {
        let err = parse_ttl_seconds("5y").unwrap_err().to_string();
        assert!(err.contains("unsupported unit"), "got: {err}");
    }

    #[test]
    fn parse_ttl_rejects_over_one_year() {
        let err = parse_ttl_seconds("400d").unwrap_err().to_string();
        assert!(err.contains("365d cap"), "got: {err}");
    }

    #[test]
    fn parse_ttl_rejects_empty() {
        assert!(parse_ttl_seconds("").is_err());
        assert!(parse_ttl_seconds("   ").is_err());
    }

    #[test]
    fn parse_ttl_rejects_signed_numbers() {
        // Defense against `u64::from_str` accepting a leading `+`
        // (and to make a future negative-shaped input fail loudly).
        let err = parse_ttl_seconds("+30m").unwrap_err().to_string();
        assert!(err.contains("invalid number"), "got: {err}");
        let err = parse_ttl_seconds("-5m").unwrap_err().to_string();
        assert!(err.contains("invalid number"), "got: {err}");
    }

    #[test]
    fn expiry_status_covers_all_four_cases() {
        let mut s = Session::test_fixture();
        s.expires_at = None;
        assert!(matches!(s.expiry_status(), ExpiryStatus::NotSet));
        s.expires_at = Some("2000-01-01T00:00:00Z".into());
        assert!(matches!(s.expiry_status(), ExpiryStatus::Expired));
        s.expires_at = Some("2099-01-01T00:00:00Z".into());
        assert!(matches!(s.expiry_status(), ExpiryStatus::Active));
        s.expires_at = Some("not a timestamp".into());
        match s.expiry_status() {
            ExpiryStatus::Malformed(v) => assert_eq!(v, "not a timestamp"),
            other => panic!("expected Malformed, got {other:?}"),
        }
        // RFC3339 requires the full date-time form.
        s.expires_at = Some("2099-01-01".into());
        assert!(matches!(s.expiry_status(), ExpiryStatus::Malformed(_)));
    }

    #[test]
    fn expires_at_from_ttl_is_rfc3339_in_the_future() {
        use time::format_description::well_known::Rfc3339;
        let before = time::OffsetDateTime::now_utc();
        let exp = expires_at_from_ttl(3600);
        let parsed = time::OffsetDateTime::parse(&exp, &Rfc3339).unwrap();
        assert!(parsed > before);
        assert!(parsed - before <= time::Duration::seconds(3601));
    }
}
