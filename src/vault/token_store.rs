//! Single-writer OAuth refresh coordination — the host-side lock that lets one
//! subscription account back many concurrent pillbox sessions (`dispatch -k`,
//! concurrent `--detach`) without tripping the provider's refresh-token reuse
//! detection. See `docs/vault-oauth-refresh-coordination.md` (M1a).
//!
//! This is the provider-agnostic **core**: the [`RefreshDecider`] contract plus the
//! [`TokenStore::begin`] / [`RotateGuard`] commit-or-abort protocol. The core is
//! agnostic to *who* forwards the grant — it hands the winner the refresh token and
//! a lock-holding guard, and the caller forwards exactly once then commits/aborts.
//! Two callers drive it: the host-side Claude **broker** (`super::refresh`), where
//! pillbox itself POSTs the grant at run start so the guest never refreshes; and the
//! in-proxy handler (codex, and the claude 401-retry fallback), which relays the
//! guest's own `/oauth/token` request. Either way the store guarantees exactly one
//! forward across all concurrent sessions sharing an account, and the rest coalesce
//! on its result.
//!
//! ## The invariant that makes it correct
//!
//! **A refresh token is POSTed at most once, ever.** Anthropic (and OpenAI) treat a
//! refresh token used twice as a stolen credential and revoke the whole token
//! family. So a forward is serialized by a cross-process `flock` held for the whole
//! forward (via [`RotateGuard`]); the current creds are re-read from disk *inside*
//! the lock (never a stale start-of-run copy); and a `pending` marker — the
//! fingerprint of the **access token** being replaced — is fsync'd *before* the
//! caller forwards. The access token is the sound signal because it lives in the
//! atomic vendor creds file and always rotates; an expiry timestamp is not, since an
//! in-proxy rotation need not update it. Across a crash, a timeout, or a wedged
//! holder, no handler ever re-sends a token that may already be consumed: an
//! ambiguous outcome (request sent, result unknown) resolves to **re-auth, not
//! retry**.

use std::fs;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What a provider plugs in so the store can coordinate its refresh generically.
/// The store owns the locking, the `pending` discipline, and the atomic write; the
/// decider owns only the provider-specific *reads* — which field is the refresh
/// token, which is the access token, and whether a forward is actually due. It
/// never performs the rotation: the in-proxy handler forwards the guest's own
/// `/oauth/token` request.
pub(crate) trait RefreshDecider {
    /// Whether a forward is due for these on-disk creds — i.e. no peer has already
    /// advanced the access token past the one this caller is replacing. Re-evaluated
    /// on the under-lock disk re-read, so it doubles as the coalesce gate: `false`
    /// ⇒ adopt the on-disk creds and forward nothing.
    fn needs_refresh(&self, creds: &Value) -> bool;

    /// The refresh token in `creds` — the value the handler forwards upstream.
    /// `None` if absent/malformed (⇒ re-auth).
    fn refresh_token(&self, creds: &Value) -> Option<String>;

    /// The access token in `creds` — the value the `pending` marker fingerprints.
    /// It always rotates and lives in the atomic creds file, so it is the sound
    /// crash-safety signal (an expiry timestamp is not, since an in-proxy rotation
    /// may not update it). `None` if absent/malformed (⇒ re-auth).
    fn access_token(&self, creds: &Value) -> Option<String>;
}

/// Outcome of [`TokenStore::begin`]. Deliberately not `Debug`: the `Coalesced` and
/// `Rotate` variants carry live credential material, which must never reach a log
/// line or a panic message.
pub(crate) enum Begin {
    /// The lock couldn't be acquired within the deadline — a peer's forward is in
    /// flight. The caller must NOT forward (a second POST risks reuse); it surfaces
    /// a retryable error so the guest retries, by when the peer has committed and the
    /// retry coalesces.
    LockBusy,
    /// A forward can't be done safely — the refresh/access token is missing, or a
    /// prior forward's outcome is unknown (the `pending` marker is still real). The
    /// caller surfaces "run `pillbox auth login`". Distinct from a transient I/O
    /// error (which is `Err`). Carries a short, secret-free reason.
    ReauthRequired(String),
    /// A peer already refreshed (or no forward was due): adopt these on-disk creds
    /// and forward nothing.
    Coalesced(Value),
    /// This caller won the race. Forward [`RotateGuard::refresh_token`] upstream
    /// exactly once, then resolve the guard with [`RotateGuard::commit`] (success)
    /// or [`RotateGuard::abort`] (failure).
    Rotate(RotateGuard),
}

/// pillbox-owned rotation bookkeeping, stored in a sidecar next to the vendor creds
/// file (the vendor file keeps its own schema untouched). Two files can't be written
/// atomically, so the `pending` marker is reconciled against the on-disk creds
/// rather than trusted blindly (see [`TokenStore::begin`]).
#[derive(Debug, Default, Serialize, Deserialize)]
struct RotationState {
    /// Bumped on every successful commit. Diagnostics + a coarse "did someone else
    /// rotate since I looked" signal; correctness rests on the creds re-read, not
    /// this counter.
    generation: u64,
    /// Set to `fingerprint(access_token)` for the token being replaced, immediately
    /// before the caller forwards, and cleared only on a successful `commit`. A set
    /// marker that still matches the on-disk **access** token means a forward is in
    /// flight or its outcome was lost — fail closed. A set marker that no longer
    /// matches means a forward committed and the marker is stale — clear it.
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
    /// `creds_path` is the vendor creds file (the agent's `cred_sentinel` under its
    /// auth home). The lock + state sidecars live beside it.
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

    /// Decide whether *this* caller should forward the refresh upstream, coalesce on
    /// a peer's result, or re-auth — and, when it should forward, hand back a
    /// [`RotateGuard`] that holds the lock across the forward and persists the
    /// outcome. The single entry point the in-proxy refresh handler calls.
    pub(crate) fn begin(&self, decider: &dyn RefreshDecider) -> Result<Begin> {
        let Some(lock) = self.lock_with_deadline()? else {
            return Ok(Begin::LockBusy);
        };

        let disk = match self.read_creds() {
            Ok(d) => d,
            // Can't refresh what we can't read; re-auth is the safe floor. Under the
            // lock on a local file a read error is genuinely broken, and treating it
            // as re-auth never risks reuse.
            Err(_) => return Ok(Begin::ReauthRequired("credentials unreadable".into())),
        };
        let mut state = self.read_state();

        // Reconcile a stale `pending`: it fingerprints the access token a forward was
        // replacing. If the on-disk access token has moved past it, that forward
        // committed (we crashed before clearing the marker) — drop it. If it still
        // matches, a forward is in flight or its outcome was lost — fail closed below.
        if let Some(pending_fp) = state.pending.clone() {
            let disk_fp = decider.access_token(&disk).map(|t| fingerprint(&t));
            if disk_fp.as_deref() != Some(pending_fp.as_str()) {
                state.pending = None;
                self.write_state(&state)?;
            }
        }
        if state.pending.is_some() {
            return Ok(Begin::ReauthRequired(
                "a prior refresh is in flight or its outcome is unknown".into(),
            ));
        }

        // Coalesce: the under-lock disk re-read is the check. If a peer already
        // advanced the token (or no forward was due), adopt the on-disk creds.
        if !decider.needs_refresh(&disk) {
            return Ok(Begin::Coalesced(disk));
        }

        let (Some(access), Some(refresh)) =
            (decider.access_token(&disk), decider.refresh_token(&disk))
        else {
            return Ok(Begin::ReauthRequired(
                "refresh or access token missing".into(),
            ));
        };

        // Mark the access token we're about to replace as pending, fsync'd, BEFORE
        // the caller forwards — so "at most once" survives a crash between here and
        // the forward's result.
        state.pending = Some(fingerprint(&access));
        self.write_state_durable(&state)?;

        Ok(Begin::Rotate(RotateGuard {
            _lock: lock,
            creds_path: self.creds_path.clone(),
            state_path: self.state_path.clone(),
            state,
            refresh,
            base_creds: disk,
        }))
    }

    /// Acquire the exclusive cross-process lock, retrying until `max_lock_wait`
    /// elapses. `Ok(None)` = busy. The `flock` is per-open-file-description and
    /// releases when the returned file's fd closes, so a crash can't leak it
    /// (matches `events::log::LogLock`).
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

    /// Missing/unreadable/corrupt state is treated as a fresh default — the state is
    /// recoverable bookkeeping, not the source of truth (the creds are).
    fn read_state(&self) -> RotationState {
        fs::read(&self.state_path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn write_state(&self, state: &RotationState) -> Result<()> {
        write_atomic(&self.state_path, &state_bytes(state)?, false)
    }

    /// Like [`write_state`] but fsync'd through to disk before returning — used for
    /// the `pending` marker, which must be durable before the caller forwards.
    fn write_state_durable(&self, state: &RotationState) -> Result<()> {
        write_atomic(&self.state_path, &state_bytes(state)?, true)
    }

    fn write_creds_atomic(&self, creds: &Value) -> Result<()> {
        write_atomic(&self.creds_path, &creds_bytes(creds)?, true)
    }

    /// Persist `creds` to the creds file **under the lock**, atomically, but ONLY if
    /// `allow_overwrite(disk)` returns true for the current on-disk creds — the
    /// teardown's guarded compare-and-swap, so an older session can't clobber a token
    /// a peer rotated under it. Returns whether it wrote.
    ///
    /// Outcomes: `LockBusy` → `Ok(false)` (a refresh is mid-flight; its writer is the
    /// authority — the teardown defers rather than block). A present, parseable disk →
    /// `allow_overwrite(&disk)` decides. An **absent or unreadable** disk → `Ok(false)`
    /// (skip): we can't confirm the compare-and-swap, so we don't write — that avoids
    /// resurrecting a creds file a concurrent `auth rm` removed, and avoids a blind
    /// overwrite on a transient read error. This is the teardown sibling of
    /// [`begin`](Self::begin); the refresh path itself goes through `begin`.
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

/// The winner's handle for one rotation. Holds the cross-process flock for its whole
/// lifetime — so while the caller forwards the refresh upstream, no peer can
/// `begin()` a second forward — and carries the durable `pending` marker `begin()`
/// already wrote.
///
/// Resolve it exactly one of three ways — and only the first ever clears `pending`:
/// - [`commit`](Self::commit) — the forward returned new creds; persist them and
///   clear `pending`. Refuses (leaving `pending` set) if the access token did not
///   actually rotate, so a caller parse bug can't turn a "success" into reuse.
/// - [`abort`](Self::abort) — the forward failed (4xx, 5xx, timeout, drop, …);
///   `pending` stays set so no peer re-forwards a maybe-consumed token, and the next
///   `begin()` fails closed to re-auth.
/// - **drop without either** — the caller panicked or returned mid-forward; identical
///   to `abort`. That fail-closed default is *why there is no explicit `Drop` impl* —
///   dropping the fields (leaving `pending` set, releasing the flock) is already the
///   correct behavior.
pub(crate) struct RotateGuard {
    _lock: LockGuard,
    creds_path: PathBuf,
    state_path: PathBuf,
    state: RotationState,
    refresh: String,
    base_creds: Value,
}

impl RotateGuard {
    /// The refresh token to forward upstream. The caller swaps this onto the wire in
    /// place of the guest's stub, exactly once.
    pub(crate) fn refresh_token(&self) -> &str {
        &self.refresh
    }

    /// The full credentials blob read under the lock. A caller that assembles its own
    /// rotated creds (the host-side pre-refresh, which POSTs and gets back only the
    /// changed token fields) splices the response onto this — so fields the OAuth
    /// response omits (`scopes`, `subscriptionType`, …) are preserved, and the
    /// committed creds reflect exactly what was on disk when the lock was taken, not a
    /// possibly-staler start-of-run copy. Carries live credential material, like
    /// `refresh_token`; never log it.
    pub(crate) fn base_creds(&self) -> &Value {
        &self.base_creds
    }

    /// The forward returned new creds: persist them, then clear `pending`.
    ///
    /// Two safety properties:
    /// - **The access token must have rotated.** `commit` refuses (returns `Err`,
    ///   leaving `pending` set → the guard drops → fail closed) if `new_creds` carries
    ///   no access token, or the same one we marked pending. Defense in depth at the
    ///   *authority*: a caller bug (a misparsed or stale response presented as success)
    ///   cannot clear the marker and let a peer re-forward a consumed token.
    /// - **Write order is load-bearing.** New tokens land durably *first*, then the
    ///   marker clears. A crash between self-heals on the next `begin()` (the advanced
    ///   on-disk access token reconciles the stale marker away). The reverse order
    ///   could let a peer re-POST a just-consumed refresh token.
    pub(crate) fn commit(mut self, decider: &dyn RefreshDecider, new_creds: Value) -> Result<()> {
        let new_access = decider
            .access_token(&new_creds)
            .context("commit: refreshed creds carry no access token")?;
        if Some(fingerprint(&new_access)) == self.state.pending {
            anyhow::bail!(
                "commit: access token did not rotate — refusing to clear pending \
                 (would risk refresh-token reuse)"
            );
        }
        write_atomic(&self.creds_path, &creds_bytes(&new_creds)?, true)?;
        self.state.generation = self.state.generation.saturating_add(1);
        self.state.pending = None;
        self.state.last_refresh_at_ms = Some(now_ms());
        write_atomic(&self.state_path, &state_bytes(&self.state)?, false)?;
        Ok(())
    }

    /// The forward did not yield usable creds — a 4xx rejection, a 5xx, a timeout, a
    /// dropped connection, an unparseable body: any non-success. Leaves `pending` SET
    /// so neither this caller nor any peer re-forwards a token that may already have
    /// been exchanged, and the next `begin()` fails closed to re-auth. Identical in
    /// effect to dropping the guard; the method exists so a failed forward reads as a
    /// deliberate decision at the call site.
    ///
    /// `abort` does NOT distinguish "cleanly rejected" from "outcome unknown": both
    /// leave `pending` set and resolve the next `begin()` to re-auth. That's the only
    /// safe move when the caller can't prove what reached the server — the in-proxy
    /// handler forwards the guest's opaque request and is in exactly that position.
    /// A caller that owns its own POST and can *prove* the token never went on the
    /// wire (or was rejected without being issued) uses [`abort_intact`](Self::abort_intact)
    /// instead, so a transient blip on the every-run host-side pre-refresh doesn't
    /// brick the credential into forced re-auth.
    pub(crate) fn abort(self) {
        // No-op beyond the drop: `pending` was made durable in `begin()` and is left
        // set; the flock releases as `self` (holding `_lock`) goes out of scope.
    }

    /// The forward provably did NOT consume the refresh token — a pre-send connect
    /// failure (the bytes never left), or an RFC 6749 grant rejection the
    /// authorization server returns *without* issuing or rotating a token
    /// (`invalid_grant` &c.). The token is intact and reusable, so `pending` is
    /// **cleared**: the next `begin()` may retry the refresh rather than fail closed.
    /// This is what keeps a transient network blip on the start-of-run pre-refresh
    /// from poisoning every subsequent run.
    ///
    /// **The caller MUST have proven non-consumption** before calling this — clearing
    /// `pending` on a maybe-consumed token would let a peer re-POST it (reuse). Any
    /// ambiguous outcome (timeout, 5xx, 429, unparseable/missing body, an unexpected
    /// status) goes to [`abort`](Self::abort), not here. The current rotation still
    /// fails (no fresh creds were produced); clearing `pending` only governs whether
    /// the *next* attempt may proceed.
    ///
    /// Not fsync'd: a lost clear leaves `pending` set, which fails safe (re-auth).
    pub(crate) fn abort_intact(mut self) -> Result<()> {
        self.state.pending = None;
        write_atomic(&self.state_path, &state_bytes(&self.state)?, false)?;
        Ok(())
    }
}

/// RAII flock holder — releases on drop (fd close).
struct LockGuard {
    _file: fs::File,
}

fn state_bytes(state: &RotationState) -> Result<Vec<u8>> {
    serde_json::to_vec(state).context("serialize rotation state")
}

fn creds_bytes(creds: &Value) -> Result<Vec<u8>> {
    serde_json::to_vec(creds).context("serialize creds")
}

/// Write `bytes` to `path` atomically: a 0600 temp file in the same directory,
/// optionally fsync'd, then `rename`d over the target (atomic on a POSIX fs). A crash
/// leaves either the old or the new file whole, never a torn one.
///
/// When `sync`, the parent directory is fsync'd *after* the rename — the rename is a
/// directory-metadata change, so without this a crash can lose it even though the
/// temp file's data was synced. Load-bearing for any durable credential write (the
/// next run must see the rotated token, not the consumed one).
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

    /// A mock provider. "Dueness" lives in the creds JSON (`stale: bool`) so the
    /// store's creds re-read drives the coalesce; the refresh/access tokens are plain
    /// fields. The "forward" is the test calling `commit`/`abort` — the decider never
    /// rotates anything itself.
    struct MockDecider;
    impl RefreshDecider for MockDecider {
        fn needs_refresh(&self, creds: &Value) -> bool {
            creds
                .get("stale")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        }
        fn refresh_token(&self, creds: &Value) -> Option<String> {
            creds
                .get("refresh")
                .and_then(|v| v.as_str())
                .map(String::from)
        }
        fn access_token(&self, creds: &Value) -> Option<String> {
            creds
                .get("access")
                .and_then(|v| v.as_str())
                .map(String::from)
        }
    }

    fn store_with_wait(creds: Value, wait: Duration) -> (tempfile::TempDir, TokenStore) {
        let dir = tempfile::tempdir().unwrap();
        let creds_path = dir.path().join(".credentials.json");
        fs::write(&creds_path, serde_json::to_vec(&creds).unwrap()).unwrap();
        let store = TokenStore::new(creds_path, wait);
        (dir, store)
    }

    /// A generous lock deadline by default: contention tests serialize many threads
    /// through, and the 20ms poll granularity must not race a tight deadline on a
    /// slow/loaded CI runner. The `LockBusy` deadline path is tested deterministically
    /// in `lock_busy_when_already_held`, not by timing.
    fn store_with(creds: Value) -> (tempfile::TempDir, TokenStore) {
        store_with_wait(creds, Duration::from_secs(30))
    }

    fn stale() -> Value {
        serde_json::json!({ "refresh": "RT0", "access": "AT0", "stale": true })
    }
    fn fresh() -> Value {
        serde_json::json!({ "refresh": "RT0", "access": "AT0", "stale": false })
    }

    #[test]
    fn coalesces_when_not_due() {
        let (_d, store) = store_with(fresh());
        assert!(matches!(
            store.begin(&MockDecider).unwrap(),
            Begin::Coalesced(_)
        ));
    }

    #[test]
    fn rotate_then_commit_persists_and_clears_pending() {
        let (_d, store) = store_with(stale());
        let Begin::Rotate(guard) = store.begin(&MockDecider).unwrap() else {
            panic!("expected Rotate");
        };
        assert_eq!(guard.refresh_token(), "RT0");
        // pending is durable mid-flight, fingerprinting the access token being replaced.
        assert_eq!(
            store.read_state().pending.as_deref(),
            Some(fingerprint("AT0").as_str())
        );
        guard
            .commit(
                &MockDecider,
                serde_json::json!({ "refresh": "RT1", "access": "AT1", "stale": false }),
            )
            .unwrap();
        let st = store.read_state();
        assert!(st.pending.is_none());
        assert_eq!(st.generation, 1);
        let disk = store.read_creds().unwrap();
        assert_eq!(disk["refresh"], "RT1");
        assert_eq!(disk["access"], "AT1");
    }

    #[test]
    fn abort_leaves_pending_and_next_begin_reauths() {
        // abort never clears pending — a failed forward (clean reject OR unknown
        // outcome) fails closed so the same RT is never handed out twice.
        let (_d, store) = store_with(stale());
        let Begin::Rotate(guard) = store.begin(&MockDecider).unwrap() else {
            panic!("expected Rotate");
        };
        guard.abort();
        assert!(
            store.read_state().pending.is_some(),
            "abort leaves pending set"
        );
        // Disk access unchanged (no commit) → next begin sees pending matching disk →
        // fail closed, and critically it does NOT hand out a second Rotate.
        assert!(matches!(
            store.begin(&MockDecider).unwrap(),
            Begin::ReauthRequired(_)
        ));
    }

    #[test]
    fn abort_intact_clears_pending_and_next_begin_retries() {
        // A provably non-consuming failure (pre-send connect error / clean
        // grant rejection): the token never went on the wire, so the next run
        // must be free to retry — NOT fail closed the way plain abort() does.
        let (_d, store) = store_with(stale());
        let Begin::Rotate(guard) = store.begin(&MockDecider).unwrap() else {
            panic!("expected Rotate");
        };
        guard.abort_intact().unwrap();
        assert!(
            store.read_state().pending.is_none(),
            "abort_intact clears pending"
        );
        // Disk creds untouched (no rotation happened) → still stale → the next
        // begin hands out a fresh Rotate to retry, not ReauthRequired.
        let Begin::Rotate(g2) = store.begin(&MockDecider).unwrap() else {
            panic!("expected a retryable Rotate after abort_intact");
        };
        assert_eq!(
            g2.refresh_token(),
            "RT0",
            "the retry re-forwards the intact token"
        );
    }

    #[test]
    fn commit_refuses_when_access_did_not_rotate() {
        // A caller bug presents a "success" whose creds still carry the old access
        // token (a misparsed/stale response). commit must refuse and leave pending
        // set, or a peer could re-forward the consumed refresh token.
        let (_d, store) = store_with(stale());
        let Begin::Rotate(guard) = store.begin(&MockDecider).unwrap() else {
            panic!("expected Rotate");
        };
        let err = guard
            .commit(
                &MockDecider,
                serde_json::json!({ "refresh": "RT9", "access": "AT0", "stale": false }),
            )
            .unwrap_err();
        assert!(format!("{err}").contains("did not rotate"), "got: {err}");
        // pending stays set → next begin fails closed; the stale creds were NOT written.
        assert!(store.read_state().pending.is_some());
        assert_eq!(store.read_creds().unwrap()["access"], "AT0");
        assert!(matches!(
            store.begin(&MockDecider).unwrap(),
            Begin::ReauthRequired(_)
        ));
    }

    #[test]
    fn commit_refuses_when_new_creds_have_no_access_token() {
        let (_d, store) = store_with(stale());
        let Begin::Rotate(guard) = store.begin(&MockDecider).unwrap() else {
            panic!("expected Rotate");
        };
        let err = guard
            .commit(&MockDecider, serde_json::json!({ "refresh": "RT1" }))
            .unwrap_err();
        assert!(format!("{err}").contains("no access token"), "got: {err}");
        assert!(store.read_state().pending.is_some(), "pending left set");
    }

    #[test]
    fn drop_uncommitted_leaves_pending_and_next_begin_reauths() {
        let (_d, store) = store_with(stale());
        {
            let Begin::Rotate(_guard) = store.begin(&MockDecider).unwrap() else {
                panic!("expected Rotate");
            };
            // _guard dropped here with neither commit nor abort — the caller
            // "panicked mid-forward". The outcome is unknown → ambiguous.
        }
        assert!(
            store.read_state().pending.is_some(),
            "an uncommitted drop leaves pending set"
        );
        assert!(matches!(
            store.begin(&MockDecider).unwrap(),
            Begin::ReauthRequired(_)
        ));
    }

    #[test]
    fn pending_matching_disk_access_fails_closed() {
        let (_d, store) = store_with(stale()); // access AT0
        store
            .write_state(&RotationState {
                generation: 0,
                pending: Some(fingerprint("AT0")),
                last_refresh_at_ms: None,
            })
            .unwrap();
        // pending fingerprints the on-disk access token → a forward is in flight or
        // its outcome was lost → re-auth, never Rotate.
        assert!(matches!(
            store.begin(&MockDecider).unwrap(),
            Begin::ReauthRequired(_)
        ));
    }

    #[test]
    fn stale_pending_reconciled_when_access_advanced() {
        // Simulate a crash after a successful commit wrote creds (access AT1) but
        // before clearing pending (which still fingerprints the old AT0).
        let (_d, store) =
            store_with(serde_json::json!({ "refresh": "RT1", "access": "AT1", "stale": false }));
        store
            .write_state(&RotationState {
                generation: 1,
                pending: Some(fingerprint("AT0")),
                last_refresh_at_ms: None,
            })
            .unwrap();
        // pending(AT0) != on-disk access(AT1) → reconciled away; token fresh → coalesce.
        assert!(matches!(
            store.begin(&MockDecider).unwrap(),
            Begin::Coalesced(_)
        ));
        assert!(
            store.read_state().pending.is_none(),
            "stale pending reconciled away"
        );
    }

    #[test]
    fn sequential_forwards_never_reuse_a_token() {
        let (_d, store) = store_with(stale());
        let Begin::Rotate(g) = store.begin(&MockDecider).unwrap() else {
            panic!("expected Rotate");
        };
        assert_eq!(g.refresh_token(), "RT0");
        // Commit, but keep it stale to force a second forward.
        g.commit(
            &MockDecider,
            serde_json::json!({ "refresh": "RT1", "access": "AT1", "stale": true }),
        )
        .unwrap();
        let Begin::Rotate(g2) = store.begin(&MockDecider).unwrap() else {
            panic!("expected Rotate");
        };
        assert_eq!(
            g2.refresh_token(),
            "RT1",
            "the second forward uses the rotated token, never RT0 again"
        );
        g2.commit(
            &MockDecider,
            serde_json::json!({ "refresh": "RT2", "access": "AT2", "stale": false }),
        )
        .unwrap();
    }

    #[test]
    fn concurrent_callers_coalesce_to_one_forward() {
        let (_d, store) = store_with(stale());
        let store = Arc::new(store);
        let forwards = Arc::new(AtomicUsize::new(0));
        let forwarded = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        for _ in 0..16 {
            let s = Arc::clone(&store);
            let n = Arc::clone(&forwards);
            let f = Arc::clone(&forwarded);
            handles.push(std::thread::spawn(move || {
                match s.begin(&MockDecider).unwrap() {
                    Begin::Rotate(g) => {
                        n.fetch_add(1, Ordering::SeqCst);
                        f.lock().unwrap().push(g.refresh_token().to_string());
                        // Simulate a successful upstream forward that freshens the token.
                        g.commit(
                        &MockDecider,
                        serde_json::json!({ "refresh": "RT1", "access": "AT1", "stale": false }),
                    )
                    .unwrap();
                        true
                    }
                    Begin::Coalesced(_) => true,
                    Begin::ReauthRequired(_) | Begin::LockBusy => false,
                }
            }));
        }
        let all_ok = handles.into_iter().all(|h| h.join().unwrap());
        assert!(all_ok, "every concurrent caller should forward or coalesce");
        // The flock serialized them; the first forwarded, the rest re-read and adopted.
        assert_eq!(
            forwards.load(Ordering::SeqCst),
            1,
            "exactly one forward across the concurrent burst"
        );
        // The one token forwarded was the original; never reused.
        assert_eq!(*forwarded.lock().unwrap(), vec!["RT0".to_string()]);
    }

    #[test]
    fn lock_busy_when_already_held() {
        // Short deadline: the lock is held for the whole call, so we want it to give
        // up quickly rather than wait the generous default.
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
        assert!(matches!(
            store.begin(&MockDecider).unwrap(),
            Begin::LockBusy
        ));
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
