//! Broker JIT OAuth refresh for the libkrun in-VMM MITM.
//!
//! In the broker model the guest's stub creds carry a far-future expiry, so the agent
//! never refreshes itself; the host keeps the real token fresh. `prepare_launch` does
//! the start-of-run rotation, but a session that outlives the access token's lifetime
//! (~8h) would then inject a stale token and 401 with no recovery (the guest can't
//! self-heal — its expiry says year 2100). This driver runs in the VMM child (a host
//! process) beside the egress poll loop: near the real token's expiry it rotates it
//! through the SAME coordinated, at-most-once `TokenStore`
//! ([`crate::vault::broker_jit_refresh`]) and splices the fresh access token into the
//! live MITM swap — so the wire token stays fresh for arbitrarily long sessions while
//! the guest still never refreshes.
//!
//! The rotation (flock + network POST) runs on a spawned thread so it never stalls the
//! 2ms poll loop — the same spawn-thread / poll-receiver idiom the upstream connect
//! uses ([`super::vault::Vault::spawn_connect`]). The `TokenStore`'s `pending`
//! discipline makes a retry safe (a consumed refresh token is never re-sent), so a
//! failed rotation just backs off and retries rather than risking reuse.
//!
//! A swap pair the MITM cloned into an already-established connection isn't updated by
//! a rotation (the clone happened at SNI-pin time); only connections pinned AFTER the
//! rotation inject the new token. In practice agents reconnect frequently, and a stale
//! connection self-heals: its request 401s, the client reconnects, the fresh pin picks
//! up the rotated `real`.

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{SystemTime, UNIX_EPOCH};

use super::egress::Diag;
use super::vault::CredSwap;

/// Rotate this long before the real token's expiry. Strictly LESS than the host-side
/// `PRE_EXPIRY_BUFFER` (5min) so the first in-window attempt actually rotates rather
/// than coalescing onto a not-yet-due token, with headroom for the rotation round-trip.
const REFRESH_LEAD_MS: u64 = 4 * 60 * 1000;

/// Minimum spacing between rotation attempts: caps the spawn cadence and doubles as the
/// backoff after a failed/coalesced rotation. A retry is always safe — the shared
/// `TokenStore` never re-sends a consumed refresh token (its `pending` marker).
const REFRESH_RETRY_BACKOFF_MS: u64 = 60 * 1000;

/// The non-secret context the parent hands the child (in the VmSpec, not on the secret
/// stdin channel) to drive broker JIT refresh: a creds-file path, an agent id, and the
/// public access-token stub. The real token is read host-side by the child from
/// `creds_path` and never leaves the child's memory + the MITM swap.
pub(super) struct RefreshCtx {
    /// The LIVE host creds file (not the stubbed guest clone) to rotate + read back.
    pub(super) creds_path: PathBuf,
    pub(super) auth_id: String,
    /// The public access-token stub that marks which swap pair to keep fresh.
    pub(super) access_stub: Vec<u8>,
}

/// `Ok((new real access-token bytes, new expiry ms))` or a sanitized error string. The
/// access token is a secret (kept in-process, spliced into the swap, never logged); the
/// error string is secret-free by construction (the refresh error reasons are sanitized
/// at the source — see `vault::refresh`).
type RefreshResult = Result<(Vec<u8>, u64), String>;

/// Drives broker JIT refresh from the egress poll loop. At most one rotation is in
/// flight at a time; the loop calls [`drive`](Self::drive) each tick and it never
/// blocks.
pub(super) struct RefreshDriver {
    creds_path: PathBuf,
    auth_id: String,
    /// Index of the access-token swap pair in the loop's `swap_pairs` (located once by
    /// stub; the Vec's length never changes, so the index stays valid).
    access_idx: usize,
    expires_at_ms: u64,
    /// Floor for the next spawn (cadence cap + post-failure backoff).
    next_attempt_ms: u64,
    in_flight: Option<mpsc::Receiver<RefreshResult>>,
}

impl RefreshDriver {
    /// Arm the driver: locate the access pair by its stub and seed the expiry from the
    /// live creds. `None` (JIT stays disabled) when the stub matches no pair, or the
    /// agent has no broker decider ([`crate::vault::broker_expiry`] → `None`, e.g.
    /// codex), or the creds carry no usable expiry.
    pub(super) fn arm(ctx: RefreshCtx, swap_pairs: &[CredSwap], diag: &Diag) -> Option<Self> {
        let access_idx = swap_pairs.iter().position(|p| p.stub == ctx.access_stub)?;
        let expires_at_ms = crate::vault::broker_expiry(&ctx.creds_path, &ctx.auth_id)?;
        diag.log(&format!(
            "krun-egress: [refresh] broker JIT armed for `{}` (token expires in ~{}s)",
            ctx.auth_id,
            expires_at_ms.saturating_sub(now_unix_ms()) / 1000,
        ));
        Some(Self {
            creds_path: ctx.creds_path,
            auth_id: ctx.auth_id,
            access_idx,
            expires_at_ms,
            next_attempt_ms: 0,
            in_flight: None,
        })
    }

    /// One poll tick: service an in-flight rotation, else spawn one when within the
    /// pre-expiry window. Never blocks the caller's poll loop.
    pub(super) fn drive(&mut self, swap_pairs: &mut [CredSwap], diag: &Diag) {
        if self.in_flight.is_some() {
            self.poll_in_flight(swap_pairs, diag);
            return; // one rotation at a time — revisit the spawn decision next tick
        }
        let now = now_unix_ms();
        if self.should_spawn(now) {
            self.spawn(now);
        }
    }

    /// Resolve the in-flight slot if its rotation thread has finished (splice a fresh
    /// token on success), else leave it pending.
    fn poll_in_flight(&mut self, swap_pairs: &mut [CredSwap], diag: &Diag) {
        let Some(rx) = self.in_flight.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok((real, expires_at_ms))) => {
                self.apply(swap_pairs, real, expires_at_ms, diag);
                self.in_flight = None;
            }
            Ok(Err(reason)) => {
                // Fail-closed: keep injecting the current (now-stale) token — it's the
                // only one we hold, and the guest can't self-heal (far-future stub
                // expiry). Retrying after the backoff (floored at spawn) is safe: the
                // TokenStore's `pending` marker guarantees a consumed refresh token is
                // never re-sent.
                diag.log(&format!(
                    "krun-egress: [refresh] rotation failed ({reason}); retrying after backoff"
                ));
                self.in_flight = None;
            }
            Err(mpsc::TryRecvError::Empty) => {} // still running — poll next tick
            Err(mpsc::TryRecvError::Disconnected) => {
                diag.log("krun-egress: [refresh] rotation thread died; retrying after backoff");
                self.in_flight = None;
            }
        }
    }

    /// Within the pre-expiry window AND past the cadence/backoff floor?
    fn should_spawn(&self, now: u64) -> bool {
        now >= self.next_attempt_ms && now >= self.expires_at_ms.saturating_sub(REFRESH_LEAD_MS)
    }

    /// Spawn the coordinated rotation on a background thread (the flock + POST are
    /// blocking; off-loop keeps the 2ms poll loop responsive) and floor the next attempt.
    fn spawn(&mut self, now: u64) {
        let creds_path = self.creds_path.clone();
        let auth_id = self.auth_id.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = crate::vault::broker_jit_refresh(&creds_path, &auth_id)
                .map(|(access, expires_at_ms)| (access.into_bytes(), expires_at_ms))
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });
        self.in_flight = Some(rx);
        // Floor the next attempt regardless of outcome: caps the spawn cadence and
        // prevents a hot loop if a rotation keeps returning a still-near expiry.
        self.next_attempt_ms = now.saturating_add(REFRESH_RETRY_BACKOFF_MS);
    }

    /// Splice the fresh real access token into its swap pair and reschedule against the
    /// new expiry. A no-op on the swap (beyond rescheduling) when the token didn't
    /// actually change — a coalesced peer rotation that yielded the token we already had.
    fn apply(
        &mut self,
        swap_pairs: &mut [CredSwap],
        new_real: Vec<u8>,
        expires_at_ms: u64,
        diag: &Diag,
    ) {
        if let Some(pair) = swap_pairs.get_mut(self.access_idx) {
            if pair.real != new_real {
                pair.real = new_real;
                diag.log(
                    "krun-egress: [refresh] access token rotated; MITM now injects the fresh token",
                );
            }
        }
        self.expires_at_ms = expires_at_ms;
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(stub: &str, real: &str) -> CredSwap {
        CredSwap {
            stub: stub.as_bytes().to_vec(),
            real: real.as_bytes().to_vec(),
            hosts: vec!["api.anthropic.com".to_string()],
        }
    }

    fn driver(access_idx: usize, expires_at_ms: u64) -> RefreshDriver {
        RefreshDriver {
            creds_path: PathBuf::from("/nonexistent/creds.json"),
            auth_id: "claude".to_string(),
            access_idx,
            expires_at_ms,
            next_attempt_ms: 0,
            in_flight: None,
        }
    }

    #[test]
    fn arm_returns_none_when_stub_matches_no_pair() {
        let pairs = vec![
            pair("accessstub", "realaccess"),
            pair("refreshstub", "realrefresh"),
        ];
        let ctx = RefreshCtx {
            creds_path: PathBuf::from("/nonexistent/creds.json"),
            auth_id: "claude".to_string(),
            access_stub: b"not-a-real-stub".to_vec(),
        };
        assert!(RefreshDriver::arm(ctx, &pairs, &Diag::open(None)).is_none());
    }

    #[test]
    fn should_spawn_only_inside_the_window_and_past_the_floor() {
        let now: u64 = 1_000_000_000_000;
        // Expiry far in the future → not due.
        let far = driver(0, now + 24 * 60 * 60 * 1000);
        assert!(!far.should_spawn(now));
        // Expiry within REFRESH_LEAD_MS → due.
        let near = driver(0, now + REFRESH_LEAD_MS - 1000);
        assert!(near.should_spawn(now));
        // Due by expiry, but a backoff floor in the future blocks the spawn.
        let mut backed_off = driver(0, now);
        backed_off.next_attempt_ms = now + 30_000;
        assert!(!backed_off.should_spawn(now));
        assert!(backed_off.should_spawn(now + 30_000));
    }

    #[test]
    fn apply_updates_the_access_pair_and_reschedules() {
        let mut pairs = vec![
            pair("accessstub", "oldaccess"),
            pair("refreshstub", "realrefresh"),
        ];
        let mut d = driver(0, 1);
        d.apply(
            &mut pairs,
            b"newaccess".to_vec(),
            2_000_000_000_000,
            &Diag::open(None),
        );
        // Only the access pair's real rotated; the refresh pair is untouched.
        assert_eq!(pairs[0].real, b"newaccess");
        assert_eq!(pairs[1].real, b"realrefresh");
        assert_eq!(d.expires_at_ms, 2_000_000_000_000);
    }

    #[test]
    fn apply_is_a_noop_swap_when_token_unchanged_but_still_reschedules() {
        // Coalesce case: a peer rotation yields the token we already inject — only the
        // expiry advances, so the next JIT is scheduled correctly without churning.
        let mut pairs = vec![pair("accessstub", "sameaccess")];
        let mut d = driver(0, 1);
        d.apply(
            &mut pairs,
            b"sameaccess".to_vec(),
            9_000_000_000_000,
            &Diag::open(None),
        );
        assert_eq!(pairs[0].real, b"sameaccess");
        assert_eq!(d.expires_at_ms, 9_000_000_000_000);
    }
}
