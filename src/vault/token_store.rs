//! Single-writer OAuth refresh coordination — the host-side lock that lets one
//! subscription account back many concurrent pillbox sessions (`dispatch -k`,
//! concurrent `--detach`) without tripping the provider's refresh-token reuse
//! detection. See `docs/vault-oauth-refresh-coordination.md` (M1a).
//!
//! This is the provider-agnostic **core**: the [`RefreshAdapter`] contract plus
//! the [`TokenStore::ensure_fresh`] protocol. Wiring the live Claude/Codex
//! refresh paths through it (so the in-proxy `/oauth/token` handlers and the
//! start-of-run pre-refresh rotate *here* instead of forwarding a per-run
//! registry copy) is the next step; until then nothing in the run path calls it.
//!
//! ## The invariant that makes it correct
//!
//! **A refresh token is POSTed at most once, ever.** Anthropic (and OpenAI) treat
//! a refresh token used twice as a stolen credential and revoke the whole token
//! family. So the rotation is serialized by a cross-process `flock`, the current
//! token is re-read from disk *inside* the lock (never a stale start-of-run
//! copy), and a `pending` marker is fsync'd *before* the upstream POST — so even
//! across a crash, a timeout, or a wedged holder, no broker and no retry ever
//! re-sends a token that may already be consumed. An ambiguous failure (the POST
//! was sent but the outcome is unknown) resolves to **re-auth, not retry**.

// Until the live refresh paths are wired through this (next PR), the type is only
// exercised by its own tests.
#![allow(dead_code)]

use std::fs;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Outcome of [`TokenStore::ensure_fresh`].
#[derive(Debug)]
pub(crate) enum EnsureOutcome {
    /// The creds are fresh — already, or after a rotation this call performed.
    /// Carries the live creds JSON the caller should swap onto the wire.
    Fresh(Value),
    /// A fresh token could not be established *safely* — the refresh token is
    /// gone, was rejected, or its rotation outcome is unknown (so it must not be
    /// re-sent). The caller surfaces this as "run `pillbox auth login`". Distinct
    /// from a transient I/O error (which is `Err`).
    ReauthRequired,
    /// The lock couldn't be acquired within the deadline. The caller should
    /// proceed on its current (still-valid) access token rather than block a turn
    /// behind another broker's in-flight refresh (convoy control).
    LockBusy,
}

/// The result of a single attempted rotation, distinguishing a *definite* failure
/// (the token was cleanly rejected) from an *ambiguous* one (the request was sent
/// but the outcome is unknown). The distinction is load-bearing: an ambiguous
/// failure means the provider *may* have consumed the token, so it must never be
/// re-sent.
#[derive(Debug)]
pub(crate) enum RotateError {
    /// A clean rejection (e.g. HTTP 4xx `invalid_grant`): the token is dead, the
    /// family is gone, re-auth is required — but the token was *not* silently
    /// consumed in a way another caller could re-trigger.
    Definite(String),
    /// The outcome is unknown — a timeout, a connection drop, or a 5xx *after* the
    /// request left the host. The provider may or may not have rotated the token.
    /// Never retry it.
    Ambiguous(String),
}

/// What a provider plugs in so the store can rotate its creds generically. The
/// store owns the locking, the `pending` discipline, and the atomic write; the
/// adapter owns only the provider-specific shape (which field is the refresh
/// token, when a refresh is due, how to perform it).
pub(crate) trait RefreshAdapter {
    /// The refresh token currently in `creds` — the exact value a rotation would
    /// POST. `None` if absent/malformed (→ re-auth).
    fn refresh_token(&self, creds: &Value) -> Option<String>;

    /// Whether the access token in `creds` is expired or within the pre-expiry
    /// buffer. Re-evaluated on the creds re-read under the lock, so it doubles as
    /// the coalesce check: if a concurrent holder already refreshed, this returns
    /// false and no second POST happens.
    fn needs_refresh(&self, creds: &Value) -> bool;

    /// POST the current refresh token and splice the new tokens into `creds` in
    /// place. Called at most once per token, with the lock held and the `pending`
    /// marker already durable.
    fn rotate(&self, creds: &mut Value) -> std::result::Result<(), RotateError>;
}

/// pillbox-owned rotation bookkeeping, stored in a sidecar next to the vendor
/// creds file (the vendor file keeps its own schema untouched). Two files can't
/// be written atomically, so the `pending` marker is reconciled against the
/// on-disk creds rather than trusted blindly (see [`TokenStore::ensure_fresh`]).
#[derive(Debug, Default, Serialize, Deserialize)]
struct RotationState {
    /// Bumped on every successful rotation. Diagnostics + a coarse "did someone
    /// else rotate since I looked" signal; correctness rests on the creds re-read,
    /// not this counter.
    generation: u64,
    /// Set to `fingerprint(refresh_token)` immediately before a POST and cleared
    /// only on a *definite* outcome. A set marker that still matches the on-disk
    /// refresh token means a POST is in flight or its result was lost — fail
    /// closed. A set marker that no longer matches means the rotation completed
    /// and the marker is stale — clear it.
    pending: Option<String>,
    last_refresh_at_ms: Option<u64>,
}

pub(crate) struct TokenStore {
    creds_path: PathBuf,
    state_path: PathBuf,
    lock_path: PathBuf,
    max_lock_wait: Duration,
}

impl TokenStore {
    /// `creds_path` is the vendor creds file (the agent's `cred_sentinel` under
    /// its auth home). The lock + state sidecars live beside it.
    pub(crate) fn new(creds_path: PathBuf, max_lock_wait: Duration) -> Self {
        let sibling = |suffix: &str| {
            let mut name = creds_path
                .file_name()
                .map(|n| n.to_os_string())
                .unwrap_or_default();
            name.push(suffix);
            creds_path.with_file_name(name)
        };
        Self {
            state_path: sibling(".pillbox-rotation.json"),
            lock_path: sibling(".pillbox-rotation.lock"),
            creds_path,
            max_lock_wait,
        }
    }

    /// Ensure the creds hold a usable, fresh access token — rotating at most once,
    /// coalescing concurrent callers, never re-sending a maybe-consumed refresh
    /// token. The single entry point a refresh path should call.
    pub(crate) fn ensure_fresh(&self, adapter: &dyn RefreshAdapter) -> Result<EnsureOutcome> {
        let Some(_guard) = self.lock_with_deadline()? else {
            return Ok(EnsureOutcome::LockBusy);
        };

        let mut creds = self.read_creds()?;
        let mut state = self.read_state();

        // Reconcile a stale `pending`: it's only "real" if it still matches the
        // refresh token on disk. If the on-disk token has moved past it, a prior
        // rotation completed (we just crashed before clearing the marker) — drop
        // it and continue.
        if let Some(pending_fp) = state.pending.clone() {
            let current_fp = adapter.refresh_token(&creds).map(|t| fingerprint(&t));
            if current_fp.as_deref() != Some(pending_fp.as_str()) {
                state.pending = None;
                self.write_state(&state)?;
            }
        }

        // A real `pending` → a token may be consumed but unconfirmed → fail closed.
        if state.pending.is_some() {
            return Ok(EnsureOutcome::ReauthRequired);
        }

        // Coalesce: re-reading the creds under the lock is the check — if another
        // holder already refreshed, the access token is fresh and we POST nothing.
        if !adapter.needs_refresh(&creds) {
            return Ok(EnsureOutcome::Fresh(creds));
        }

        let Some(refresh) = adapter.refresh_token(&creds) else {
            return Ok(EnsureOutcome::ReauthRequired);
        };

        // Mark the token consumed *before* the POST, fsync'd, so "at most once"
        // survives a crash/timeout between here and the result.
        state.pending = Some(fingerprint(&refresh));
        self.write_state_durable(&state)?;

        match adapter.rotate(&mut creds) {
            Ok(()) => {
                // New tokens first (atomic), then clear the marker. If we crash
                // between, the next caller sees pending != on-disk-token → stale →
                // proceeds.
                self.write_creds_atomic(&creds)?;
                state.generation = state.generation.saturating_add(1);
                state.pending = None;
                state.last_refresh_at_ms = Some(now_ms());
                self.write_state(&state)?;
                Ok(EnsureOutcome::Fresh(creds))
            }
            Err(RotateError::Definite(_)) => {
                // Cleanly rejected: the token didn't silently rotate, so clearing
                // the marker is safe. Family is dead → re-auth.
                state.pending = None;
                self.write_state(&state)?;
                Ok(EnsureOutcome::ReauthRequired)
            }
            Err(RotateError::Ambiguous(_)) => {
                // Outcome unknown: leave `pending` SET so neither this caller nor
                // any other ever re-sends the token. Re-auth, never retry.
                Ok(EnsureOutcome::ReauthRequired)
            }
        }
    }

    /// Acquire the exclusive cross-process lock, retrying until `max_lock_wait`
    /// elapses. `Ok(None)` = busy (caller proceeds on its current token). The
    /// `flock` is per-open-file-description and releases when the returned file's
    /// fd closes, so a crash can't leak it (matches `events::log::LogLock`).
    fn lock_with_deadline(&self) -> Result<Option<LockGuard>> {
        let file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .mode(0o600)
            .open(&self.lock_path)
            .with_context(|| format!("open lock {}", self.lock_path.display()))?;
        let deadline = Instant::now() + self.max_lock_wait;
        loop {
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc == 0 {
                return Ok(Some(LockGuard { _file: file }));
            }
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EWOULDBLOCK) {
                return Err(err).with_context(|| format!("lock {}", self.lock_path.display()));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn read_creds(&self) -> Result<Value> {
        let bytes = fs::read(&self.creds_path)
            .with_context(|| format!("read creds {}", self.creds_path.display()))?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("parse creds {}", self.creds_path.display()))
    }

    /// Missing/unreadable/corrupt state is treated as a fresh default — the state
    /// is recoverable bookkeeping, not the source of truth (the creds are).
    fn read_state(&self) -> RotationState {
        fs::read(&self.state_path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn write_state(&self, state: &RotationState) -> Result<()> {
        let body = serde_json::to_vec(state).context("serialize rotation state")?;
        write_atomic(&self.state_path, &body, false)
    }

    /// Like [`write_state`] but fsync'd through to disk before returning — used
    /// for the `pending` marker, which must be durable before the POST.
    fn write_state_durable(&self, state: &RotationState) -> Result<()> {
        let body = serde_json::to_vec(state).context("serialize rotation state")?;
        write_atomic(&self.state_path, &body, true)
    }

    fn write_creds_atomic(&self, creds: &Value) -> Result<()> {
        let body = serde_json::to_vec(creds).context("serialize creds")?;
        write_atomic(&self.creds_path, &body, true)
    }

    /// Persist `creds` to the creds file **under the lock**, atomically, but ONLY
    /// if `allow_overwrite(disk)` returns true for the current on-disk creds — the
    /// teardown's guarded compare-and-swap, so an older session can't clobber a
    /// token a peer rotated under it. Returns whether it wrote.
    ///
    /// Outcomes: `LockBusy` → `Ok(false)` (a refresh is mid-flight; its writer is
    /// the authority — the teardown defers rather than block). A present, parseable
    /// disk → `allow_overwrite(&disk)` decides. An **absent or unreadable** disk →
    /// `Ok(false)` (skip): we can't confirm the compare-and-swap, so we don't write
    /// — that avoids resurrecting a creds file a concurrent `auth rm` removed, and
    /// avoids a blind overwrite on a transient read error. This is the teardown
    /// sibling of [`ensure_fresh`]; the refresh path itself is separate.
    pub(crate) fn persist_if(
        &self,
        creds: &Value,
        allow_overwrite: &dyn Fn(&Value) -> bool,
    ) -> Result<bool> {
        let Some(_guard) = self.lock_with_deadline()? else {
            return Ok(false);
        };
        let Ok(disk) = self.read_creds() else {
            return Ok(false);
        };
        if allow_overwrite(&disk) {
            self.write_creds_atomic(creds)?;
            return Ok(true);
        }
        Ok(false)
    }
}

/// RAII flock holder — releases on drop (fd close).
struct LockGuard {
    _file: fs::File,
}

/// Write `bytes` to `path` atomically: a 0600 temp file in the same directory,
/// optionally fsync'd, then `rename`d over the target (atomic on a POSIX fs). A
/// crash leaves either the old or the new file whole, never a torn one.
///
/// When `sync`, the parent directory is fsync'd *after* the rename — the rename is
/// a directory-metadata change, so without this a crash can lose it even though the
/// temp file's data was synced. Load-bearing for any durable credential write
/// (the next run must see the rotated token, not the consumed one).
fn write_atomic(path: &Path, bytes: &[u8], sync: bool) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::Builder::new()
        .prefix(".pillbox-tmp-")
        .tempfile_in(dir)
        .with_context(|| format!("temp file in {}", dir.display()))?;
    {
        use std::io::Write;
        tmp.as_file_mut()
            .set_permissions(perms_0600())
            .context("chmod temp 0600")?;
        tmp.write_all(bytes).context("write temp")?;
        if sync {
            tmp.as_file().sync_all().context("fsync temp")?;
        }
    }
    tmp.persist(path)
        .map_err(|e| e.error)
        .with_context(|| format!("rename into {}", path.display()))?;
    if sync {
        let dir_file =
            fs::File::open(dir).with_context(|| format!("open dir for fsync {}", dir.display()))?;
        dir_file
            .sync_all()
            .with_context(|| format!("fsync dir {}", dir.display()))?;
    }
    Ok(())
}

fn perms_0600() -> fs::Permissions {
    use std::os::unix::fs::PermissionsExt;
    fs::Permissions::from_mode(0o600)
}

/// A short, stable, non-reversible fingerprint of a token — enough to tell two
/// tokens apart in the `pending` marker without ever storing the token itself.
fn fingerprint(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(token.as_bytes());
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// A mock provider. "Staleness" lives in the creds JSON (`stale: bool`) so the
    /// store's creds re-read drives the coalesce; every `rotate` records the token
    /// it was handed (to prove no reuse) and can be told to fail.
    struct MockAdapter {
        rotate_calls: AtomicUsize,
        tokens_posted: Mutex<Vec<String>>,
        outcome: MockOutcome,
        next_token: AtomicUsize,
    }
    #[derive(Clone, Copy)]
    enum MockOutcome {
        Ok,
        Definite,
        Ambiguous,
    }
    impl MockAdapter {
        fn new(outcome: MockOutcome) -> Self {
            Self {
                rotate_calls: AtomicUsize::new(0),
                tokens_posted: Mutex::new(Vec::new()),
                outcome,
                next_token: AtomicUsize::new(1),
            }
        }
    }
    impl RefreshAdapter for MockAdapter {
        fn refresh_token(&self, creds: &Value) -> Option<String> {
            creds
                .get("refresh")
                .and_then(|v| v.as_str())
                .map(String::from)
        }
        fn needs_refresh(&self, creds: &Value) -> bool {
            creds
                .get("stale")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        }
        fn rotate(&self, creds: &mut Value) -> std::result::Result<(), RotateError> {
            self.rotate_calls.fetch_add(1, Ordering::SeqCst);
            let posted = creds
                .get("refresh")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            self.tokens_posted.lock().unwrap().push(posted);
            match self.outcome {
                MockOutcome::Ok => {
                    let n = self.next_token.fetch_add(1, Ordering::SeqCst);
                    creds["refresh"] = Value::String(format!("RT{n}"));
                    creds["access"] = Value::String(format!("AT{n}"));
                    creds["stale"] = Value::Bool(false);
                    Ok(())
                }
                MockOutcome::Definite => Err(RotateError::Definite("invalid_grant".into())),
                MockOutcome::Ambiguous => Err(RotateError::Ambiguous("timeout".into())),
            }
        }
    }

    fn store_with_wait(creds: Value, wait: Duration) -> (tempfile::TempDir, TokenStore) {
        let dir = tempfile::tempdir().unwrap();
        let creds_path = dir.path().join(".credentials.json");
        fs::write(&creds_path, serde_json::to_vec(&creds).unwrap()).unwrap();
        let store = TokenStore::new(creds_path, wait);
        (dir, store)
    }

    /// A generous lock deadline by default: contention tests serialize many
    /// threads through, and the 20ms poll granularity must not race a tight
    /// deadline on a slow/loaded CI runner. The `LockBusy` deadline path is tested
    /// deterministically in `lock_busy_when_already_held`, not by timing.
    fn store_with(creds: Value) -> (tempfile::TempDir, TokenStore) {
        store_with_wait(creds, Duration::from_secs(30))
    }

    fn stale() -> Value {
        serde_json::json!({ "refresh": "RT0", "access": "AT0", "stale": true })
    }

    #[test]
    fn rotates_once_when_stale() {
        let (_d, store) = store_with(stale());
        let adapter = MockAdapter::new(MockOutcome::Ok);
        let out = store.ensure_fresh(&adapter).unwrap();
        assert!(matches!(out, EnsureOutcome::Fresh(_)));
        assert_eq!(adapter.rotate_calls.load(Ordering::SeqCst), 1);
        // Generation bumped, no pending left behind.
        assert_eq!(store.read_state().generation, 1);
        assert!(store.read_state().pending.is_none());
        // The persisted creds carry the rotated token.
        assert_eq!(store.read_creds().unwrap()["refresh"], "RT1");
    }

    #[test]
    fn fresh_token_is_adopted_without_a_post() {
        let (_d, store) = store_with(serde_json::json!({ "refresh": "RT0", "stale": false }));
        let adapter = MockAdapter::new(MockOutcome::Ok);
        let out = store.ensure_fresh(&adapter).unwrap();
        assert!(matches!(out, EnsureOutcome::Fresh(_)));
        assert_eq!(adapter.rotate_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn concurrent_callers_coalesce_to_exactly_one_rotation() {
        let (_d, store) = store_with(stale());
        let store = Arc::new(store);
        let adapter = Arc::new(MockAdapter::new(MockOutcome::Ok));
        let mut handles = Vec::new();
        for _ in 0..16 {
            let s = Arc::clone(&store);
            let a = Arc::clone(&adapter);
            handles.push(std::thread::spawn(move || {
                matches!(s.ensure_fresh(&*a).unwrap(), EnsureOutcome::Fresh(_))
            }));
        }
        let all_fresh = handles.into_iter().all(|h| h.join().unwrap());
        assert!(all_fresh, "every concurrent caller should end Fresh");
        // The flock serialized them; the first rotated, the rest re-read and adopted.
        assert_eq!(
            adapter.rotate_calls.load(Ordering::SeqCst),
            1,
            "exactly one rotation across the concurrent burst"
        );
        // The one token sent was the original; never reused.
        let posted = adapter.tokens_posted.lock().unwrap().clone();
        assert_eq!(posted, vec!["RT0".to_string()]);
    }

    #[test]
    fn sequential_rotations_never_reuse_a_token() {
        let (_d, store) = store_with(stale());
        let adapter = MockAdapter::new(MockOutcome::Ok);
        store.ensure_fresh(&adapter).unwrap(); // RT0 -> RT1
                                               // Mark stale again to force a second rotation.
        let mut creds = store.read_creds().unwrap();
        creds["stale"] = Value::Bool(true);
        fs::write(&store.creds_path, serde_json::to_vec(&creds).unwrap()).unwrap();
        store.ensure_fresh(&adapter).unwrap(); // RT1 -> RT2
        let posted = adapter.tokens_posted.lock().unwrap().clone();
        assert_eq!(posted, vec!["RT0".to_string(), "RT1".to_string()]);
        // No duplicates.
        let mut sorted = posted.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), posted.len());
    }

    #[test]
    fn ambiguous_failure_sets_pending_and_next_call_reauths_without_retry() {
        let (_d, store) = store_with(stale());
        let adapter = MockAdapter::new(MockOutcome::Ambiguous);
        let out = store.ensure_fresh(&adapter).unwrap();
        assert!(matches!(out, EnsureOutcome::ReauthRequired));
        assert!(store.read_state().pending.is_some(), "pending stays set");
        // The token on disk is unchanged (rotate didn't splice), so a second call
        // sees pending matching the current token → fail closed, NO second POST.
        let out2 = store.ensure_fresh(&adapter).unwrap();
        assert!(matches!(out2, EnsureOutcome::ReauthRequired));
        assert_eq!(
            adapter.rotate_calls.load(Ordering::SeqCst),
            1,
            "a maybe-consumed token is never re-POSTed"
        );
    }

    #[test]
    fn definite_failure_clears_pending_and_reauths() {
        let (_d, store) = store_with(stale());
        let adapter = MockAdapter::new(MockOutcome::Definite);
        let out = store.ensure_fresh(&adapter).unwrap();
        assert!(matches!(out, EnsureOutcome::ReauthRequired));
        assert!(
            store.read_state().pending.is_none(),
            "a clean rejection clears the marker"
        );
    }

    #[test]
    fn stale_pending_is_dropped_when_disk_token_advanced() {
        // Simulate a crash after a successful rotation but before clearing
        // pending: creds already hold RT1, but pending still fingerprints RT0.
        let (_d, store) = store_with(serde_json::json!({ "refresh": "RT1", "stale": false }));
        let mut state = RotationState {
            generation: 1,
            pending: Some(fingerprint("RT0")),
            last_refresh_at_ms: None,
        };
        store.write_state(&state).unwrap();
        let adapter = MockAdapter::new(MockOutcome::Ok);
        // The pending no longer matches the on-disk token → reconciled away →
        // and the token is fresh → adopt.
        let out = store.ensure_fresh(&adapter).unwrap();
        assert!(matches!(out, EnsureOutcome::Fresh(_)));
        state = store.read_state();
        assert!(state.pending.is_none(), "stale pending reconciled away");
        assert_eq!(adapter.rotate_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn lock_busy_when_already_held() {
        // Short deadline: the lock is held for the whole call, so we want it to
        // give up quickly rather than wait the generous default.
        let (_d, store) = store_with_wait(stale(), Duration::from_millis(100));
        // Hold the flock from an independent fd (separate open file description).
        let held = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&store.lock_path)
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let adapter = MockAdapter::new(MockOutcome::Ok);
        let out = store.ensure_fresh(&adapter).unwrap();
        assert!(matches!(out, EnsureOutcome::LockBusy), "got {out:?}");
        assert_eq!(adapter.rotate_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn fingerprint_never_contains_the_token() {
        let fp = fingerprint("super-secret-refresh-token-RT0");
        assert!(!fp.contains("secret"));
        assert!(!fp.contains("RT0"));
        assert_eq!(fp.len(), 16); // 8 bytes hex
        assert_eq!(fingerprint("a"), fingerprint("a")); // stable
        assert_ne!(fingerprint("a"), fingerprint("b"));
    }

    // ── persist_if (the teardown's guarded compare-and-swap) ────────────────

    fn disk_access_is(expected: &str) -> impl Fn(&Value) -> bool + '_ {
        move |disk| disk.get("access").and_then(|v| v.as_str()) == Some(expected)
    }

    #[test]
    fn persist_if_writes_when_predicate_allows() {
        let (_d, store) = store_with(serde_json::json!({ "access": "AT0" }));
        // CAS: disk access is still "AT0" (what we leased) → overwrite with ours.
        let wrote = store
            .persist_if(
                &serde_json::json!({ "access": "AT1" }),
                &disk_access_is("AT0"),
            )
            .unwrap();
        assert!(wrote);
        assert_eq!(store.read_creds().unwrap()["access"], "AT1");
    }

    #[test]
    fn persist_if_skips_when_predicate_denies_no_clobber() {
        // A peer rotated disk to AT_PEER; our CAS expected the leased "AT0" → deny.
        let (_d, store) = store_with(serde_json::json!({ "access": "AT_PEER" }));
        let wrote = store
            .persist_if(
                &serde_json::json!({ "access": "AT_MINE" }),
                &disk_access_is("AT0"),
            )
            .unwrap();
        assert!(!wrote);
        // Disk is untouched — the peer's token is not clobbered.
        assert_eq!(store.read_creds().unwrap()["access"], "AT_PEER");
    }

    #[test]
    fn persist_if_skips_when_disk_absent() {
        let dir = tempfile::tempdir().unwrap();
        let creds_path = dir.path().join(".credentials.json");
        let store = TokenStore::new(creds_path.clone(), Duration::from_secs(5));
        // No disk file → can't confirm the CAS → skip (don't recreate a file a
        // concurrent `auth rm` may have removed; predicate never consulted).
        let wrote = store
            .persist_if(&serde_json::json!({ "access": "AT1" }), &|_| true)
            .unwrap();
        assert!(!wrote);
        assert!(
            !creds_path.exists(),
            "must not recreate an absent creds file"
        );
    }

    #[test]
    fn persist_if_skips_on_lock_busy() {
        let (_d, store) = store_with_wait(
            serde_json::json!({ "access": "AT0" }),
            Duration::from_millis(100),
        );
        let held = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&store.lock_path)
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        // Lock held by a peer (a refresh in flight) → defer, don't write.
        let wrote = store
            .persist_if(&serde_json::json!({ "access": "AT1" }), &|_| true)
            .unwrap();
        assert!(!wrote);
        assert_eq!(store.read_creds().unwrap()["access"], "AT0");
    }
}
