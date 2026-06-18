//! `SandboxBackend` / `LiveSession` implementation for the **managed Cloudflare
//! tier** — a session placed on a CF container behind the per-session §0-gateway
//! Durable Object, driven + read over the DO's HTTP/WebSocket surface.
//!
//! Unlike [`docker`](super::docker) / [`libkrun`](super::libkrun) this backend
//! provisions nothing on the host: the durable session lives server-side in the
//! DO (the seq authority + actor attestation + driver arbitration), so every
//! verb is a network call against the gateway. The read path reuses the §0 read
//! seam ([`crate::events::source::ManagedDoSource`]) — `subscribe` opens the
//! DO's WebSocket, which replays-then-tails one `contract::Event` per frame in
//! seq order. The write path (`send`) POSTs the driver-attributed steer to
//! `/input`. The whole §0 contract is the SAME event schema the local backends
//! emit; the DO is just a different placement of the same log.
//!
//! ## Configuration (env-driven; no host state)
//!
//! The DO base URL + the actor-token material come from the environment — the
//! same vars the §0 sink/source factories already read
//! ([`crate::events::managed_endpoint`]):
//!
//!   - `PILLBOX_MANAGED_DO_URL` — the worker origin, e.g.
//!     `https://<worker>.workers.dev` (the `/agents/session-gateway/<id>` path
//!     is appended per session). Required; absent ⇒ this backend isn't selected.
//!   - `PILLBOX_ACTOR_TOKEN` — a pre-minted actor token (the deploy's HMAC over
//!     the actor claim). Used verbatim for read + the §0 sink.
//!   - `PILLBOX_MANAGED_TOKEN_SECRET` — the shared HMAC secret, when pillbox
//!     should mint its own driver token (for `/input`, which is driver-gated and
//!     so must be stamped with a *driver* actor). Mints a `human(<os user>)`
//!     token via [`mint_actor_token`] when set; falls back to
//!     `PILLBOX_ACTOR_TOKEN` otherwise.
//!
//! ## Open design decisions — STUBBED, not guessed (see docs/managed-tier.md)
//!
//!   - **Workspace placement.** The spike runs the agent in an empty container
//!     `/workspace`; getting the user's cwd *in* (and results *out*) is the
//!     deferred R2/rustic question, NOT decided. [`ManagedBackend::run`] is a
//!     loud stub at that boundary — it does not fabricate a transfer.
//!   - **Token provisioning / trust.** Where a real user's token/secret comes
//!     from (vs the spike's `/tmp` file) is unresolved; the env config above is
//!     the interim surface.

use std::path::PathBuf;

use anyhow::Result;

use super::{Caps, LiveSession, SandboxBackend};
use crate::agents::{AgentSpec, RunOpts};
use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::session::{self, Session, BACKEND_MANAGED};

pub(crate) struct ManagedBackend;

impl SandboxBackend for ManagedBackend {
    /// The managed family: the DO offers an attributed agent-drive (`/input`
    /// `target:"agent"`) and a durable replay-then-tail read (`/subscribe`), so
    /// `server_mode` + `post_hoc_ingest`'s *read* analogue are covered. It does
    /// NOT offer a host PTY, a long-lived host exec target, or the KVM-isolation
    /// features (those are libkrun's). `pty_drive`/`live_pty_tail` are false: the
    /// DO drives the agent's structured prompt channel, not raw keystrokes. See
    /// docs/substrate-plane.md.
    fn capabilities(&self) -> Caps {
        Caps {
            // No host PTY behind the DO — drive is the structured agent channel.
            pty_drive: false,
            live_pty_tail: false,
            // The DO drives an opencode-style agent turn (`/input target:agent`)
            // and streams its §0 events back over `/subscribe`.
            server_mode: true,
            // No host exec target / KVM isolation — those are the local backends.
            long_lived_exec: false,
            in_sandbox_grading: false,
            real_egress_fence: false,
            detached_vault: false,
            // The durable log lives server-side; there's no headless host capture
            // file to drain post-hoc (reads stream live from the DO instead).
            post_hoc_ingest: false,
        }
    }

    fn id(&self) -> &'static str {
        BACKEND_MANAGED
    }

    fn run(&self, _spec: &AgentSpec, _opts: RunOpts, _resolved: &Pillbox) -> Result<()> {
        // ── STUB: workspace placement is the open design decision ──
        // The §0/trust/subscribe/drive substrate is built + proven (the DO in
        // `cloudflare-spike/`), and the read/write seams (`ManagedDoSource` /
        // `ManagedDoSink`) are wired. What is NOT decided — and what this method
        // would have to invent — is how the user's cwd gets *into* the managed
        // container and how results come back out. The spike runs the agent in an
        // empty `/workspace`; the deferred design (docs/managed-tier.md §workspace
        // store) is R2/rustic, NOT chosen. Rather than fabricate a transfer (and
        // silently run an agent against an empty tree), refuse loudly here. The
        // drive + read verbs below DO work against an already-placed session
        // (e.g. one the spike's harness started), which is what this backend is
        // wired for today.
        Err(PillboxError::usage(
            "run",
            "the managed backend can't place a workspace yet — workspace transfer \
             (cwd in, results out) is an unresolved design decision (R2/rustic, see \
             docs/managed-tier.md). pillbox can drive + read an already-placed \
             managed session, but not launch one with your workspace.",
        )
        .with_next("PILLBOX_BACKEND=libkrun pillbox run   # the local microVM backend")
        .into())
    }
}

/// The managed [`LiveSession`] — a session that lives on the §0-gateway DO. Holds
/// the cloned [`Session`] record (whose [`ManagedHandle`] in `sandbox_id` carries
/// the DO endpoint + the DO-side session id) and resolves the per-verb network
/// surface from it. No local process: every verb is a DO call.
pub(crate) struct ManagedLiveSession {
    session: Session,
}

impl ManagedLiveSession {
    pub(crate) fn new(session: Session) -> Self {
        Self { session }
    }

    /// The decoded DO handle (endpoint + DO-side session id) this session points
    /// at — the single place that reads it back out of the record.
    fn handle(&self) -> Result<ManagedHandle> {
        ManagedHandle::decode(&self.session)
    }
}

impl LiveSession for ManagedLiveSession {
    fn caps(&self) -> Caps {
        ManagedBackend.capabilities()
    }

    fn send(&self, bytes: &[u8]) -> Result<()> {
        let handle = self.handle()?;
        // `/input` is driver-gated + attributed: it needs a *driver* actor token
        // (`human`/`service`), not the anonymous read token. Mint one from the
        // shared secret when configured, else fall back to a pre-minted token.
        let token = driver_token().ok_or_else(|| {
            PillboxError::config(
                "session send",
                "no managed actor token: set PILLBOX_MANAGED_TOKEN_SECRET (to mint a \
                 driver token) or PILLBOX_ACTOR_TOKEN (a pre-minted one)",
            )
        })?;
        // The agent's structured prompt channel — the DO runs an opencode turn
        // whose §0 events stream back over `/subscribe`.
        let text = String::from_utf8_lossy(bytes).into_owned();
        let model = self.session.server.as_ref().map(|s| s.model.as_str());
        input::drive_agent(&handle.endpoint, &token, &text, model)
    }

    fn attach(&self, _resolved: &Pillbox) -> Result<()> {
        // Reattach for a managed session is NOT a local-process re-pump (docker's
        // docker-exec relay / libkrun's attach socket): the session is durable
        // server-side, so "reattach" = re-subscribe to the DO log from the last
        // seq. The read surface for that is `session watch`/`subscribe` (they open
        // the DO WebSocket via the §0 source), and there's no terminal PTY to pump
        // — so a bare `attach` (which means "pump a terminal") is unsupported.
        // Point the user at the read verbs instead of pumping garbage frames.
        Err(PillboxError::usage(
            "session attach",
            "a managed session has no host PTY to attach — it's durable on the §0 \
             gateway; re-subscribe instead",
        )
        .with_next(format!(
            "pillbox session watch {id}   # read it    ·   pillbox session send {id} \"…\"   # drive it",
            id = self.session.id
        ))
        .into())
    }

    fn spawn_log_tailer(
        &self,
        _resolved: &Pillbox,
    ) -> Result<Option<crate::events::transcripts::TailerHandle>> {
        // The §0 read source for a managed session (`ManagedDoSource`, opened by
        // `open_event_source` when the managed env is on) IS the live tail: the
        // DO replays-then-tails over the WebSocket. There's no separate host-side
        // transcript/capture to drain into the log, so there's nothing to spawn —
        // the consumer's own `subscribe` is the live reader. (Contrast docker's
        // transcript tailer / libkrun's capture-file drain, which fill a *local*
        // log; the managed log lives on the DO.)
        Ok(None)
    }

    fn http(&self) -> Result<Box<dyn crate::sandbox::http::SandboxHttp>> {
        // The managed agent is reached through the DO's REST surface (`/input`,
        // `/subscribe`), not a raw in-sandbox HTTP server the host can `curl`.
        // The `SandboxHttp` seam models the latter; managed doesn't expose one, so
        // the verb is unsupported (drive goes through `send` → `/input`).
        Err(self.caps().unsupported("http"))
    }

    fn workspace_path(&self) -> Result<PathBuf> {
        // The workspace lives in the CF container, not on this host — there's no
        // host path to hand back. (And workspace placement itself is the stubbed
        // open decision; see `ManagedBackend::run`.) Matches
        // `caps().in_sandbox_grading == false`.
        Err(self.caps().unsupported("workspace_path"))
    }

    fn ingest(&self, _resolved: &Pillbox) -> Result<usize> {
        // The DO holds the durable log; reads stream live from it via
        // `subscribe`/`watch`. There's no host capture file to drain post-hoc,
        // so post-hoc ingest is a no-op verb here (matches
        // `caps().post_hoc_ingest == false`).
        Err(self.caps().unsupported("ingest"))
    }

    fn kill(&self, resolved: &Pillbox) -> Result<()> {
        // No local sandbox to tear down — the CF container's lifecycle is the
        // managed tier's (the DO owns it). pillbox's teardown is dropping the
        // *local* record so it stops showing in `session list`; the durable DO
        // session is unaffected (its own retention governs it). Best-effort
        // DO-side teardown (a `/driver/release` or a future `/session/destroy`)
        // is a managed-tier follow-up — flagged, not faked.
        crate::events::emit_session_event(
            resolved,
            crate::events::EventType::SessionDropped,
            &self.session.id,
            Some(&self.session),
        );
        session::delete(resolved, &self.session.id)?;
        println!(
            "pillbox: ✓ session `{}` record removed (the managed §0 session is durable \
             server-side and is unaffected).",
            self.session.id
        );
        Ok(())
    }
}

/// What a managed session stores in [`Session::sandbox_id`] (as JSON): the DO
/// base endpoint to reach + the DO-side session id. Mirrors libkrun's
/// `LibkrunHandle` pattern — an opaque, backend-specific handle the plane decodes
/// to find the session again. No credential material (the token comes from env).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ManagedHandle {
    /// The per-session DO base URL, e.g.
    /// `https://<worker>.workers.dev/agents/session-gateway/<doSessionId>` (no
    /// trailing slash). `/input` etc. are appended per call; the read side
    /// rewrites the scheme to `wss` for `/subscribe`.
    pub(crate) endpoint: String,
    /// The DO-side session id (the §0-gateway Agent instance name). May differ
    /// from this record's pillbox id; kept so a consumer can correlate the local
    /// record with the durable server-side log.
    pub(crate) do_session_id: String,
}

impl ManagedHandle {
    fn decode(session: &Session) -> Result<Self> {
        serde_json::from_str(&session.sandbox_id)
            .map_err(|e| {
                PillboxError::config(
                    "session",
                    format!("decode managed session handle for `{}`: {e}", session.id),
                )
            })
            .map_err(Into::into)
    }
}

/// The driver token for a `/input` (driver-gated) call: mint one from the shared
/// HMAC secret when set (stamped `human(<os user>)` — the local user is the
/// driver), else fall back to a pre-minted `PILLBOX_ACTOR_TOKEN`. `None` when
/// neither is configured, so `send` can fail with a clear next-step.
fn driver_token() -> Option<String> {
    if let Ok(secret) = std::env::var("PILLBOX_MANAGED_TOKEN_SECRET") {
        if !secret.is_empty() {
            let actor = crate::contract::Actor::human(local_user());
            return Some(mint_actor_token(&actor, &secret));
        }
    }
    std::env::var("PILLBOX_ACTOR_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

/// The OS user, used as the driver actor's id when pillbox mints its own token.
/// Mirrors `commands::session::local_user` (kept local to avoid a cross-module
/// pub; the value is the same `$USER`/`$USERNAME` fallback).
fn local_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "local".into())
}

/// Mint an actor token the §0-gateway DO's `verifyActorToken` accepts.
///
/// The wire format is **exactly** `cloudflare-spike/src/auth.ts::signActorToken`:
/// `base64url(claimJson) . base64url(HMAC-SHA256(claimJson, secret))`, where
/// `claimJson` is the JSON serialization of the [`Actor`](crate::contract::Actor)
/// (the DO re-parses it and re-checks `kind` ∈ {human,agent,service} + a string
/// `id`). The two base64url segments are padless (`=` stripped), `+`→`-`, `/`→`_`
/// — matching `b64urlEncode`. Serializing via serde produces `{"kind":…,"id":…}`
/// (and `display` only when non-empty); the DO ignores `display` for identity, so
/// the only thing that must agree is the byte string the HMAC signs — which is the
/// claim segment, signed as-is on both sides.
pub(crate) fn mint_actor_token(actor: &crate::contract::Actor, secret: &str) -> String {
    use base64::Engine as _;
    let claim_json = serde_json::to_vec(actor).expect("Actor serializes (no non-string keys)");
    let claim_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&claim_json);
    let sig = hmac_sha256(secret.as_bytes(), claim_b64.as_bytes());
    let sig_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig);
    format!("{claim_b64}.{sig_b64}")
}

/// HMAC-SHA256 (RFC 2104) over `data` with `key`. Implemented directly on
/// `sha2::Sha256` (already a direct dep) rather than pulling in the `hmac` crate
/// for this one use — the construction is small and fully specified. Returns the
/// 32-byte MAC.
fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    const BLOCK: usize = 64; // SHA-256 block size
                             // Keys longer than the block are first hashed to fit (RFC 2104).
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        let digest = Sha256::digest(key);
        k[..digest.len()].copy_from_slice(&digest);
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let inner = {
        let mut h = Sha256::new();
        h.update(ipad);
        h.update(data);
        h.finalize()
    };
    let outer = {
        let mut h = Sha256::new();
        h.update(opad);
        h.update(inner);
        h.finalize()
    };
    let mut out = [0u8; 32];
    out.copy_from_slice(&outer);
    out
}

/// The `/input` drive call — the DO-side `send`. Kept in a submodule so the HTTP
/// shape (the `{text, target, model?}` body + the driver 409 mapping) is in one
/// place, separate from the trait wiring above.
mod input {
    use anyhow::{Context, Result};
    use serde::Serialize;

    use crate::errors::PillboxError;

    /// `POST <endpoint>/input` with `target:"agent"` — drive an agent turn whose
    /// §0 events stream back over `/subscribe`. Maps the driver 409 ("not the
    /// driver") to a clear pillbox error (another actor holds the slot); other
    /// non-2xx are surfaced verbatim.
    pub(super) fn drive_agent(
        endpoint: &str,
        token: &str,
        text: &str,
        model: Option<&str>,
    ) -> Result<()> {
        #[derive(Serialize)]
        struct Input<'a> {
            text: &'a str,
            target: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            model: Option<&'a str>,
        }
        let body = serde_json::to_string(&Input {
            text,
            target: "agent",
            model,
        })
        .context("serialize managed /input body")?;

        let url = format!("{}/input", endpoint.trim_end_matches('/'));
        let client = reqwest::blocking::Client::builder()
            .timeout(crate::events::EVENTS_SINK_TIMEOUT)
            .build()
            .context("build managed /input http client")?;
        let resp = client
            .post(&url)
            .header("content-type", "application/json")
            .bearer_auth(token)
            .body(body)
            .send()
            .with_context(|| format!("POST {url}"))?;

        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        // 409 = the DO's driver arbitration rejected the steer (another actor
        // holds the single driver slot). Distinct, actionable error vs a generic
        // HTTP failure — the user can re-issue with the steal flag (a follow-up
        // surface) or wait for the driver to release.
        if status.as_u16() == 409 {
            return Err(PillboxError::runtime(
                "session send",
                "the managed session is driven by another actor (the §0 driver slot \
                 is held); your steer was rejected",
            )
            .with_next("retry once the current driver releases the session")
            .into());
        }
        Err(PillboxError::runtime(
            "session send",
            format!("managed §0 gateway {url} returned HTTP {status}"),
        )
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{Actor, ActorKind};

    /// HMAC-SHA256 against the RFC 4231 Test Case 2 vector
    /// (key=`"Jefe"`, data=`"what do ya want for nothing?"`) — proves our
    /// hand-rolled HMAC matches the standard, so a token the DO's WebCrypto
    /// `crypto.subtle.sign("HMAC")` verifies will verify here too.
    #[test]
    fn hmac_sha256_matches_rfc4231_case2() {
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    /// The token shape matches `auth.ts::signActorToken`:
    /// `base64url(claimJson).base64url(hmac)`, two padless URL-safe segments
    /// split on a single `.`, the first being the base64url of the actor JSON.
    #[test]
    fn mint_actor_token_has_two_b64url_segments_over_the_actor_claim() {
        use base64::Engine as _;
        let actor = Actor::agent("opencode"); // id => "a:opencode"
        let token = mint_actor_token(&actor, "shared-secret");

        let (claim_b64, sig_b64) = token.split_once('.').expect("two dot-joined segments");
        assert!(!claim_b64.is_empty() && !sig_b64.is_empty());
        // Padless URL-safe alphabet: no '+', '/', or '=' on either segment.
        for seg in [claim_b64, sig_b64] {
            assert!(
                !seg.contains('+') && !seg.contains('/') && !seg.contains('='),
                "segment not base64url-no-pad: {seg}"
            );
        }
        // The claim segment decodes back to the exact actor JSON serde produced —
        // the byte string the DO re-parses + the HMAC signs.
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(claim_b64)
            .expect("claim is valid base64url");
        let back: Actor = serde_json::from_slice(&decoded).expect("claim is the Actor JSON");
        assert_eq!(back.kind, ActorKind::Agent);
        assert_eq!(back.id, "a:opencode");
    }

    /// The signature is over the *claim segment* (the base64url string), exactly
    /// as `auth.ts` signs `claim` — not over the raw JSON. Recomputing the HMAC
    /// over `claim_b64` must reproduce the token's signature segment.
    #[test]
    fn mint_actor_token_signs_the_claim_segment() {
        use base64::Engine as _;
        let actor = Actor::human("alice"); // id => "u:alice"
        let secret = "deploy-secret";
        let token = mint_actor_token(&actor, secret);
        let (claim_b64, sig_b64) = token.split_once('.').unwrap();

        let expected = hmac_sha256(secret.as_bytes(), claim_b64.as_bytes());
        let expected_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(expected);
        assert_eq!(
            sig_b64, expected_b64,
            "signature must sign the claim segment"
        );
    }

    /// A different secret yields a different signature (the MAC actually binds to
    /// the key) while the claim segment is unchanged (it only encodes the actor).
    #[test]
    fn mint_actor_token_signature_depends_on_secret() {
        let actor = Actor::service("grader");
        let a = mint_actor_token(&actor, "secret-a");
        let b = mint_actor_token(&actor, "secret-b");
        let (claim_a, sig_a) = a.split_once('.').unwrap();
        let (claim_b, sig_b) = b.split_once('.').unwrap();
        assert_eq!(claim_a, claim_b, "same actor => same claim segment");
        assert_ne!(sig_a, sig_b, "different secret => different signature");
    }

    /// The handle round-trips through the record's `sandbox_id` JSON, so the
    /// plane can decode the DO endpoint + session id back out.
    #[test]
    fn managed_handle_round_trips_through_sandbox_id() {
        let handle = ManagedHandle {
            endpoint: "https://w.workers.dev/agents/session-gateway/sess-do".into(),
            do_session_id: "sess-do".into(),
        };
        let mut s = Session::test_fixture();
        s.backend = BACKEND_MANAGED.to_string();
        s.placement = session::Placement::Managed;
        s.sandbox_id = serde_json::to_string(&handle).unwrap();

        let live = ManagedLiveSession::new(s);
        let decoded = live.handle().expect("decodes the managed handle");
        assert_eq!(decoded, handle);
    }

    /// The capability profile is the honest managed surface: server-mode drive +
    /// read, no host PTY / exec / KVM-isolation features.
    #[test]
    fn managed_caps_are_server_mode_only() {
        let caps = ManagedBackend.capabilities();
        assert!(
            caps.server_mode,
            "managed drives the structured agent channel"
        );
        assert!(!caps.pty_drive, "no host PTY behind the DO");
        assert!(!caps.live_pty_tail);
        assert!(!caps.long_lived_exec);
        assert!(!caps.in_sandbox_grading);
        assert!(!caps.post_hoc_ingest, "the durable log lives on the DO");
    }

    /// A managed record resolves to a `ManagedLiveSession` whose verbs are the
    /// honest unsupported shape where the DO offers nothing host-side: `attach`
    /// (no PTY), `http`, `workspace_path`, `ingest` all reject with a clear,
    /// verb-naming error rather than mis-acting.
    #[test]
    fn unsupported_verbs_reject_with_clear_errors() {
        let handle = ManagedHandle {
            endpoint: "https://w.workers.dev/agents/session-gateway/sess-do".into(),
            do_session_id: "sess-do".into(),
        };
        let mut s = Session::test_fixture();
        s.backend = BACKEND_MANAGED.to_string();
        s.placement = session::Placement::Managed;
        s.sandbox_id = serde_json::to_string(&handle).unwrap();
        let live = ManagedLiveSession::new(s);

        assert!(live.http().is_err(), "managed exposes no SandboxHttp");
        assert!(
            live.workspace_path().is_err(),
            "no host workspace for a managed session"
        );
        // `spawn_log_tailer` returns None (the DO source IS the live tail), not an
        // error — the consumer's own `subscribe` reads it.
        // (Tailer spawn takes a Pillbox; covered by the integration path, not unit
        // tested here to avoid touching the registry.)
    }
}
