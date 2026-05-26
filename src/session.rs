//! Session registry — durable handles for `pillbox run --remote --detach`
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
//! Only `e2b` backed sessions exist today — E2B's `sandbox.pty.connect`
//! gives us a clean reattach primitive. SSH-backed sessions error
//! loudly on `--detach` / `session attach`; the tmux-based path lands
//! in a follow-up.
//!
//! ## Threat model
//!
//! Session records are stored under `~/.pillbox/.../sessions/<id>.toml`,
//! which is parented by the per-pillbox state dir (0700, uid-owned).
//! Co-tenants on the same machine cannot read or write them.
//!
//! The fields, by sensitivity:
//!   - `sandbox_id` (e.g. `sb_xxx`): an opaque E2B resource handle.
//!     Not a secret in isolation — leaking it does **not** grant
//!     access without a valid `E2B_API_KEY`. Treated as
//!     low-confidentiality config, not credential material.
//!   - `attached_pid`: a local process id; not a secret. See the
//!     hardening in `main.rs::session_detach` for why we still
//!     validate it (pid 0/1/-1/self are rejected, ESRCH is treated
//!     as already-detached).
//!   - `remote`, `backend`, `agent_id`, `started_at`, `label`:
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

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::registry::{self as reg, IdRegistry, Registry};

/// Subdirectory under a pillbox's state dir holding session records.
/// Mirrors `remotes/` and `secrets/` — one file per record, easy to
/// grep, easy to `rm -f` if something goes sideways. Module-private;
/// callers go through `list` / `read` / `write` / `delete`.
const SESSIONS_DIR: &str = "sessions";

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
pub(crate) const BACKEND_E2B: &str = "e2b";
pub(crate) const BACKEND_SSH: &str = "ssh";

/// Typed view of the on-disk `backend` string. Returned by
/// [`Backend::parse`]; `None` means a backend label this binary doesn't
/// know about (older or hand-edited record). Callers report the raw
/// label in the error so the user can grep for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Backend {
    E2b,
    Ssh,
}

impl Backend {
    pub(crate) fn parse(label: &str) -> Option<Self> {
        match label {
            BACKEND_E2B => Some(Backend::E2b),
            BACKEND_SSH => Some(Backend::Ssh),
            _ => None,
        }
    }
}

/// On-disk shape. Forward-compatible: serde will ignore unknown fields
/// so a future binary writing extra metadata (e.g. a "last seen"
/// timestamp) doesn't break older readers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Session {
    /// Short hex id — 12 chars. Used as the registry filename and as
    /// the `pillbox session attach <id>` argument.
    pub(crate) id: String,
    /// Optional human label (`pillbox run --remote NAME --detach --label foo`).
    /// Surfaced in `session list` next to the id.
    #[serde(default)]
    pub(crate) label: Option<String>,
    /// Name of the remote this session lives on (resolved via the
    /// remote registry on reattach so per-host details aren't stale).
    pub(crate) remote: String,
    /// Backend kind — one of [`BACKEND_E2B`], [`BACKEND_SSH`].
    pub(crate) backend: String,
    /// Opaque handle the backend uses to find this session again.
    /// For E2B: the sandbox id (`sb_xxx`). For SSH (future): the tmux
    /// session name.
    pub(crate) sandbox_id: String,
    /// PTY process id inside the backend. For E2B: the pid returned by
    /// `sandbox.pty.create`. For SSH (future): 0 (tmux finds by name).
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
    /// s.remote = "other-remote".into();
    /// ```
    ///
    /// Kept `#[cfg(test)]` to keep the production binary slim.
    #[cfg(test)]
    pub(crate) fn test_fixture() -> Self {
        Self {
            id: "abc123def456".to_string(),
            label: Some("test".to_string()),
            remote: "test-remote".to_string(),
            backend: BACKEND_E2B.to_string(),
            sandbox_id: "sb_test".to_string(),
            pty_pid: 0,
            agent_id: "claude".to_string(),
            started_at: "2026-05-23T13:37:00Z".to_string(),
            attached_pid: None,
            base_snapshot: None,
            result_snapshot: None,
            expires_at: None,
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
        o.insert("remote".into(), self.remote.clone().into());
        o.insert("backend".into(), self.backend.clone().into());
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
        serde_json::Value::Object(o)
    }
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
            remote: String::new(),
            backend: String::new(),
            sandbox_id: String::new(),
            pty_pid: 0,
            agent_id: String::new(),
            started_at: now_rfc3339(),
            attached_pid: None,
            base_snapshot: None,
            result_snapshot: None,
            expires_at: None,
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

    fn make(remote: &str, backend: &str) -> Session {
        // Per-call overrides (id, remote, backend, started_at) on top
        // of the shared `Session::test_fixture()`. Keeps the
        // ambiguous-prefix / list-ordering tests deterministic where
        // the fixture's stable fields wouldn't fit.
        let mut s = Session::test_fixture();
        s.id = Session::new_id();
        s.label = None;
        s.remote = remote.to_string();
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
    fn write_and_read_round_trip() {
        with_isolated_home("session-rt", || {
            let g = pillbox::global();
            let s = make("vps1", BACKEND_E2B);
            write(&g, &s).unwrap();
            let back = read(&g, &s.id).unwrap().expect("present");
            assert_eq!(back.id, s.id);
            assert_eq!(back.remote, "vps1");
            assert_eq!(back.backend, BACKEND_E2B);
            assert_eq!(back.pty_pid, 42);
            assert!(back.attached_pid.is_none());
        });
    }

    #[test]
    fn list_is_stable_by_started_at() {
        with_isolated_home("session-list", || {
            let g = pillbox::global();
            let mut a = make("r", BACKEND_E2B);
            a.started_at = "2026-01-01T00:00:00Z".into();
            let mut b = make("r", BACKEND_E2B);
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
            let s = make("r", BACKEND_E2B);
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
            let s = make("r", BACKEND_E2B);
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
            let mut a = make("r", BACKEND_E2B);
            a.id = "abcdef000001".into();
            let mut b = make("r", BACKEND_E2B);
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
            let s = make("r", BACKEND_E2B);
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
            let s = make("r", BACKEND_E2B);
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
            let s = make("r", BACKEND_E2B);
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
        let _: &str = BACKEND_E2B;
        let _: &str = BACKEND_SSH;
        let _: &str = SESSIONS_DIR;
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
