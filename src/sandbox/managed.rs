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
//! ## Workspace placement — container-native rustic-on-R2
//!
//! [`ManagedBackend::run`] places the workspace by reusing the pillbox's rustic
//! repo: the host snapshots cwd into R2, POSTs the DO `/provision` (repo config +
//! password + snapshot) to restore it into the container `/workspace`, drives the
//! turn, then POSTs `/finalize` to snapshot `/workspace` back and records the
//! result handle. The R2 creds + the repo password travel ONLY in those HTTPS
//! bodies — never in argv, a log, a §0 event, or the persisted `Session` record
//! (which holds endpoint + session id + result handle; creds are re-resolved from
//! env each run). The DO/worker restore+snapshot half is built separately to the
//! same frozen contract (docs/managed-tier.md).
//!
//! ## Open follow-ups (flagged, not faked)
//!
//!   - **R2 key scoping.** When `PILLBOX_R2_CF_API_TOKEN` is set, `run` mints a
//!     short-lived, prefix-scoped R2 temp credential ([`r2_scope`], fresh per
//!     transfer) and hands the DO *that*, so a credential reaching CF can touch
//!     only this run's prefix — and the bucket-wide parent *secret* never crosses
//!     to CF (the Bearer API token authorizes the mint). With no token configured
//!     the parent key still travels, but the exposure is announced loudly rather
//!     than silently. The DO side must honor the credential's `session_token`
//!     (`X-Amz-Security-Token`) for a scoped credential to authenticate — part of
//!     the frozen contract.
//!   - **Detached finalize.** Only the foreground path is implemented (drive a
//!     turn, wait for idle, finalize). For a `--detach` managed run the host
//!     returns before the turn ends, so the in-container wrapper would own the
//!     `/finalize` + result-handle emission instead.
//!   - **Token provisioning / trust.** Where a real user's token/secret comes
//!     from (vs the spike's `/tmp` file) is unresolved; the env config above is
//!     the interim surface.
// Context: doc://pillbox/managed-store-of-record@0001#managed-store-of-record
// Context: doc://pillbox/managed-tier-do-gateway@0001#managed-tier-do-gateway

use std::path::PathBuf;

use anyhow::Result;

use super::{Caps, LiveSession, SandboxBackend};
use crate::agents::{AgentSpec, RunOpts};
use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::session::{self, Session, BACKEND_MANAGED};
use crate::workspace::WorkspaceBackend;

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

    /// Container-native, rustic-on-R2 workspace placement.
    ///
    /// The host snapshots cwd into the pillbox's R2 repo, hands the DO a
    /// provisioning payload (repo config + password + snapshot), and the DO's
    /// container restores it into `/workspace`, drives the agent, and snapshots
    /// the result back to R2; the host records the result handle on the session.
    /// This builds ONLY the host side — the DO/worker restore+snapshot is a
    /// separate build to the same frozen contract (see docs/managed-tier.md).
    fn run(&self, spec: &AgentSpec, opts: RunOpts, resolved: &Pillbox) -> Result<()> {
        // 1. Require an R2/S3 workspace backend. The DO restores from a rustic
        //    repo it can reach (R2), not the host's local-filesystem repo —
        //    refuse a local-backend pillbox loudly instead of silently running
        //    the agent against an empty container tree.
        let workspace = resolved.workspace()?;
        let s3 = require_s3_repo(&workspace)?;
        // Read the repo password back from its local 0600 file. It travels ONLY
        // in the HTTPS body to the DO (never argv/log/§0/the Session record).
        let password = workspace.resolved_password()?;

        // 2. Snapshot the run's workspace into the R2 repo, reusing the same push
        //    path `pillbox push` calls (no reimplemented rustic backup). The DO
        //    restores THIS handle into the container `/workspace`.
        let workspace_host = match &opts.workspace {
            Some(p) => p.clone(),
            None => std::env::current_dir()
                .map_err(|e| PillboxError::runtime("run", format!("resolve cwd: {e}")))?,
        };
        let snapshot = workspace
            .push(&workspace_host, crate::workspace::PushOptions::default())?
            .handle;

        // 3. Resolve the DO endpoint (the same `PILLBOX_MANAGED_DO_URL` + per-session
        //    path the §0 sink/source build), refusing a non-`https://` origin: the
        //    POST body carries the resolved R2 creds + the repo password, so it must
        //    never cross the wire in cleartext.
        let session_id = crate::session::Session::new_id();
        let endpoint = resolve_https_endpoint(&session_id)?;

        // 4. Mint a driver token + POST /provision. `/provision` is driver-gated,
        //    so it carries the same driver actor token as `/input` (`send`).
        let token = driver_token().ok_or_else(|| {
            PillboxError::config(
                "run",
                "no managed actor token: set PILLBOX_MANAGED_TOKEN_SECRET (to mint a driver \
                 token) or PILLBOX_ACTOR_TOKEN (a pre-minted one)",
            )
        })?;
        // Prefix-scope the credential before it crosses to the managed plane: the
        // DO only needs this repo's prefix, not the whole bucket. Minted fresh per
        // transfer so each credential outlives only its own round-trip, never the
        // whole turn.
        let provision_creds = r2_scope::scope_for_transfer(s3)?;
        workspace_xfer::provision(
            &endpoint,
            &token,
            &provision_creds,
            &password,
            snapshot.as_str(),
        )?;

        // 5. Build + persist the Session record. The record holds the DO endpoint +
        //    session id + (later) the result handle — NEVER the creds or password,
        //    which are re-resolved from env via `workspace()` on every run.
        let handle = ManagedHandle {
            endpoint: endpoint.clone(),
            do_session_id: session_id.clone(),
        };
        let model = opts
            .model
            .clone()
            .unwrap_or_else(|| crate::sandbox::opencode::DEFAULT_MODEL.to_string());
        let session = Session {
            id: session_id.clone(),
            label: opts.label.clone(),
            backend: BACKEND_MANAGED.to_string(),
            sandbox_id: serde_json::to_string(&handle)
                .map_err(|e| PillboxError::config("run", format!("encode managed handle: {e}")))?,
            pty_pid: 0,
            agent_id: spec.id.to_string(),
            started_at: crate::session::now_rfc3339(),
            attached_pid: None,
            // The base the agent forked from — the snapshot the DO restored.
            base_snapshot: Some(snapshot.as_str().to_string()),
            result_snapshot: None,
            expires_at: opts.ttl_seconds.map(crate::session::expires_at_from_ttl),
            // The container mounts the restored tree at `/workspace`.
            guest_cwd: crate::agents::GUEST_WORKSPACE.to_string(),
            placement: session::Placement::Managed,
            server: Some(crate::session::ServerSession {
                // The DO is the agent-session authority; correlate by our id.
                agent_session_id: session_id.clone(),
                model,
                temperature: opts.temperature,
            }),
        };
        session::write(resolved, &session)?;
        crate::events::emit_session_event(
            resolved,
            crate::events::EventType::SessionStarted {
                parent_session_id: crate::events::parent_session_id_from_env(),
                startup: None,
            },
            &session.id,
            Some(&session),
        );

        let live = ManagedLiveSession::new(session.clone());

        // 6. Drive the first turn through the plane (`send` → `/input`), then read
        //    the §0 stream until the turn goes idle — the foreground path. The
        //    initial prompt is the agent's positional args; with none, leave the
        //    session ready for `session send` and return (detached-style).
        let prompt = opts.args.join(" ").trim().to_string();
        if prompt.is_empty() {
            crate::sandbox::opencode::print_started(&session, opts.json, None);
            // FOLLOW-UP: a no-prompt managed run leaves the workspace provisioned
            // but never finalized (no turn → no result). When the drive surface
            // gains a host-free "finalize on idle", the in-container wrapper owns
            // that; for now a no-prompt run is bring-up only.
            return Ok(());
        }
        live.send(format!("{prompt}\n").as_bytes())?;
        wait_until_idle(resolved, &session.id)?;

        // 7. Snapshot `/workspace` back to R2 via /finalize; record the handle so
        //    `session pull <id>` can rehydrate the result.
        let finalize_creds = r2_scope::scope_for_transfer(s3)?;
        let result_snapshot =
            workspace_xfer::finalize(&endpoint, &token, &finalize_creds, &password)?;
        let mut finished = session;
        finished.result_snapshot = Some(result_snapshot);
        session::write(resolved, &finished)?;

        if opts.json {
            crate::session::print_started_json(&finished);
        } else {
            println!(
                "pillbox: ✓ managed session `{}` finished; result snapshot recorded.",
                finished.id
            );
            println!(
                "         pillbox session pull {}   # rehydrate the result",
                finished.id
            );
        }
        // FOLLOW-UP (detached managed run): for `--detach` the host returns before
        // the turn ends, so this host-side `/finalize` can't run. The in-container
        // wrapper would finalize + emit the result handle to the §0 sink instead.
        // This pass implements only the foreground path; `--detach` managed is
        // flagged, not faked.
        Ok(())
    }
}

/// Require an R2/S3 workspace backend, returning its resolved [`S3Config`]. The DO
/// restores from a rustic repo it can reach (R2) — a local-filesystem repo is
/// host-only, so a local-backend pillbox is refused loudly rather than running the
/// agent against an empty container tree. Split out of `run` so the contract guard
/// is unit-testable without a live DO.
fn require_s3_repo(
    workspace: &crate::workspace::rustic::RusticBackend,
) -> Result<&crate::workspace::rustic::S3Config> {
    workspace.s3_config().ok_or_else(|| {
        PillboxError::config(
            "run",
            "managed run needs an R2/S3 workspace backend — this pillbox uses the \
             local-filesystem rustic repo, which the managed container can't reach.",
        )
        .with_next("pillbox new --endpoint <r2-url> --bucket <bucket> …  # an R2-backed pillbox")
        .into()
    })
}

/// Resolve the per-session managed DO endpoint and enforce HTTPS. The provision /
/// finalize bodies carry the resolved R2 creds + the repo password, so a non-HTTPS
/// origin (which would put them on the wire in cleartext) is refused — this is the
/// mandatory transport guard. Split out of `run` so it's unit-testable.
fn resolve_https_endpoint(session_id: &str) -> Result<String> {
    let (endpoint, _read_token) = crate::events::managed_endpoint(session_id).ok_or_else(|| {
        PillboxError::config(
            "run",
            "the managed backend needs PILLBOX_MANAGED_DO_URL set to the §0-gateway worker origin",
        )
        .with_next("export PILLBOX_MANAGED_DO_URL=https://<worker>.workers.dev")
    })?;
    if !endpoint.starts_with("https://") {
        return Err(PillboxError::config(
            "run",
            format!(
                "refusing to provision a managed workspace over a non-HTTPS endpoint \
                 (`{endpoint}`): the request body carries the R2 credentials + the repo \
                 password, which must not travel in cleartext"
            ),
        )
        .with_next("set PILLBOX_MANAGED_DO_URL to an https:// origin")
        .into());
    }
    Ok(endpoint)
}

/// Block until the managed session's current turn goes idle — the §0
/// `AttentionRequired` signal (the agent yielded for input) or a terminal
/// `RunFinished`/`RunFailed`. Reads the DO log through the same `open_event_source`
/// the §0 read verbs use (it routes to [`ManagedDoSource`] under the managed env),
/// so the wait isn't a hand-rolled read. Subscribes from seq 0 — a freshly-minted
/// DO session has no prior turn, so the first idle is this turn's.
fn wait_until_idle(resolved: &Pillbox, session_id: &str) -> Result<()> {
    use crate::contract::Payload;
    use std::sync::atomic::AtomicBool;

    let source = crate::events::source::open_event_source(resolved, session_id)?;
    let stop = AtomicBool::new(false);
    let mut idle = false;
    source.subscribe(0, &stop, &mut |ev| {
        let done = matches!(
            ev.payload,
            Payload::AttentionRequired(_) | Payload::RunFinished(_) | Payload::RunFailed(_)
        );
        if done {
            idle = true;
        }
        !done // keep reading until a turn-done event
    })?;
    if idle {
        Ok(())
    } else {
        // The stream closed without an idle/terminal event (DO went away
        // mid-turn) — surface it rather than silently finalizing a partial run.
        Err(PillboxError::runtime(
            "run",
            format!("managed session `{session_id}` stream ended before the turn went idle"),
        )
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
        // `/input` blocks for the WHOLE agent turn: the DO's handleInput awaits the
        // opencode turn to idle before responding. So this is a turn-length wait, not
        // a quick steer — the ~2s event-sink budget would time out every real turn.
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(600))
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

/// Container-native workspace transfer — the host half of the frozen R2/rustic
/// placement contract (see docs/managed-tier.md). `provision` hands the DO the
/// rustic-on-R2 coordinates so its container restores the snapshot into
/// `/workspace`; `finalize` asks the DO to snapshot `/workspace` back and returns
/// the result handle. Both POST over HTTPS — the **only** channel the resolved R2
/// creds + the repo password travel on. Kept in one submodule so the wire shapes
/// (`{workspace:{repo,password,snapshot}}` / `{workspace:{repo,password}}`) and
/// the error mapping live together, mirroring [`input`].
mod workspace_xfer {
    use anyhow::{Context, Result};
    use serde::Serialize;

    use crate::errors::PillboxError;
    use crate::workspace::rustic::S3Config;

    /// Restore + snapshot can move a whole workspace tree through R2, so the
    /// per-event-sink budget ([`crate::events::EVENTS_SINK_TIMEOUT`], ~2s) is far
    /// too tight — a real restore would time out spuriously. Bound it generously
    /// instead so a genuinely hung DO still fails loud rather than parking forever.
    const XFER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

    /// The repo + password the DO needs to open the rustic repo on R2. The creds
    /// inside `repo` (an [`S3Config`]) and `password` are resolved secret material;
    /// they leave the host ONLY inside this body, over HTTPS — never logged,
    /// never persisted on the `Session` record.
    // The `repo` handed in is already prefix-scoped when scoping is configured
    // (see [`super::r2_scope`]); a scoped credential carries a `session_token`
    // the DO must forward as `X-Amz-Security-Token`.
    #[derive(Serialize)]
    struct WorkspaceRepo<'a> {
        repo: &'a S3Config,
        password: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        snapshot: Option<&'a str>,
    }

    #[derive(Serialize)]
    struct ProvisionBody<'a> {
        workspace: WorkspaceRepo<'a>,
    }

    /// `POST <endpoint>/provision` — the DO restores `snapshot` from the R2 repo
    /// into the container `/workspace`. Driver-gated, so it carries the driver
    /// actor token. Non-2xx maps to a clear pillbox error with the DO's body text.
    pub(super) fn provision(
        endpoint: &str,
        token: &str,
        repo: &S3Config,
        password: &str,
        snapshot: &str,
    ) -> Result<()> {
        let body = serde_json::to_string(&ProvisionBody {
            workspace: WorkspaceRepo {
                repo,
                password,
                snapshot: Some(snapshot),
            },
        })
        .context("serialize managed /provision body")?;
        let resp = post(endpoint, "provision", token, body)?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        // The error text may echo our request; the DO is trusted not to reflect
        // the creds, but cap it so a hostile/buggy body can't flood the terminal.
        let detail = error_detail(resp);
        Err(PillboxError::runtime(
            "run",
            format!("managed workspace provision failed (HTTP {status}): {detail}"),
        )
        .into())
    }

    /// `POST <endpoint>/finalize` — the DO snapshots `/workspace` back to the R2
    /// repo and returns `{ "resultSnapshot": "<handle>" }`. Returns the handle.
    pub(super) fn finalize(
        endpoint: &str,
        token: &str,
        repo: &S3Config,
        password: &str,
    ) -> Result<String> {
        let body = serde_json::to_string(&ProvisionBody {
            workspace: WorkspaceRepo {
                repo,
                password,
                snapshot: None,
            },
        })
        .context("serialize managed /finalize body")?;
        let resp = post(endpoint, "finalize", token, body)?;
        let status = resp.status();
        if !status.is_success() {
            let detail = error_detail(resp);
            return Err(PillboxError::runtime(
                "run",
                format!("managed workspace finalize failed (HTTP {status}): {detail}"),
            )
            .into());
        }
        let text = resp.text().context("read managed /finalize response")?;
        let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
            PillboxError::runtime(
                "run",
                format!("managed /finalize: unexpected response: {e}"),
            )
        })?;
        parsed
            .get("resultSnapshot")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                PillboxError::runtime(
                    "run",
                    format!("managed /finalize returned no `resultSnapshot`: {text}"),
                )
                .into()
            })
    }

    /// One `POST <endpoint>/<path>` with the driver token + JSON body. Mirrors
    /// [`super::input::drive_agent`]'s client/error-mapping shape; the only
    /// difference is the longer [`XFER_TIMEOUT`] (a transfer, not a steer).
    fn post(
        endpoint: &str,
        path: &str,
        token: &str,
        body: String,
    ) -> Result<reqwest::blocking::Response> {
        let url = format!("{}/{path}", endpoint.trim_end_matches('/'));
        let client = reqwest::blocking::Client::builder()
            .timeout(XFER_TIMEOUT)
            .build()
            .context("build managed workspace-transfer http client")?;
        client
            .post(&url)
            .header("content-type", "application/json")
            .bearer_auth(token)
            .body(body)
            .send()
            .with_context(|| format!("POST {url}"))
    }

    /// The DO's error-body text, capped so a large/hostile body can't flood the
    /// terminal. A read failure degrades to a placeholder rather than masking the
    /// HTTP status the caller already reports.
    fn error_detail(resp: reqwest::blocking::Response) -> String {
        const CAP: usize = 2048;
        let mut s = resp.text().unwrap_or_else(|_| "<unreadable body>".into());
        if s.len() > CAP {
            s.truncate(CAP);
            s.push('…');
        }
        s
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn cfg() -> S3Config {
            S3Config {
                endpoint: "https://r2.example.com".into(),
                region: "auto".into(),
                bucket: "ws".into(),
                prefix: "p/".into(),
                access_key: "AK".into(),
                secret_key: "SK".into(),
                session_token: None,
            }
        }

        /// The frozen `/provision` shape: `{workspace:{repo:<S3Config>,password,snapshot}}`
        /// — the S3Config nested under `repo`, the password + snapshot handle as
        /// siblings. The DO side is built to this exact JSON.
        #[test]
        fn provision_body_serializes_to_the_frozen_shape() {
            let c = cfg();
            let body = serde_json::to_value(ProvisionBody {
                workspace: WorkspaceRepo {
                    repo: &c,
                    password: "repo-pw",
                    snapshot: Some("snap-handle"),
                },
            })
            .unwrap();

            let ws = &body["workspace"];
            assert_eq!(ws["password"], "repo-pw");
            assert_eq!(ws["snapshot"], "snap-handle");
            // The S3Config is nested verbatim under `repo` (its serde fields).
            let repo = &ws["repo"];
            assert_eq!(repo["endpoint"], "https://r2.example.com");
            assert_eq!(repo["bucket"], "ws");
            assert_eq!(repo["access_key"], "AK");
            assert_eq!(repo["secret_key"], "SK");
            // A long-lived key carries no session token: absent on the wire, so
            // the frozen contract is byte-identical to pre-scoping.
            assert!(repo.get("session_token").is_none());
        }

        /// `/finalize` is the same shape minus `snapshot` (the DO snapshots
        /// `/workspace`, it doesn't restore one) — `snapshot` is omitted, not null.
        #[test]
        fn finalize_body_omits_the_snapshot_field() {
            let c = cfg();
            let body = serde_json::to_value(ProvisionBody {
                workspace: WorkspaceRepo {
                    repo: &c,
                    password: "repo-pw",
                    snapshot: None,
                },
            })
            .unwrap();
            assert!(
                body["workspace"].get("snapshot").is_none(),
                "finalize must omit `snapshot`, got: {body}"
            );
            assert_eq!(body["workspace"]["repo"]["bucket"], "ws");
        }
    }
}

/// Prefix-scope the R2 credential before it crosses to the managed plane.
///
/// `run` hands the DO an [`S3Config`] so its container can restore + snapshot
/// the rustic repo. Handing it the pillbox's *parent* R2 key gives a credential
/// reaching Cloudflare bucket-wide reach — far more than this run's repo needs.
/// When a Cloudflare API token is configured (`PILLBOX_R2_CF_API_TOKEN`), this
/// mints a short-lived, **prefix-scoped** temp credential via R2's
/// `temp-access-credentials` API and hands the DO *that* instead, so a credential
/// reaching CF can touch only `bucket/<prefix>` for a bounded TTL. With no token
/// configured the parent key still travels (unchanged behavior) but the exposure
/// is announced once, loudly, instead of silently — the gap is visible, not faked.
mod r2_scope {
    use std::borrow::Cow;
    use std::sync::Once;

    use anyhow::{Context, Result};
    use serde::{Deserialize, Serialize};

    use crate::errors::PillboxError;
    use crate::workspace::rustic::S3Config;

    /// The CF API token that authorizes minting temp credentials (a Bearer token
    /// with R2 read+write on the bucket). Absent ⇒ scoping is off and the parent
    /// key travels; present ⇒ scoping is required (a mint failure is fatal, never
    /// a silent fall-back to the bucket-wide key).
    const API_TOKEN_ENV: &str = "PILLBOX_R2_CF_API_TOKEN";
    /// Lifetime of a minted transfer credential. A credential is minted fresh
    /// *per transfer* (provision, then finalize), so it only has to outlive one
    /// synchronous round-trip (bounded by `XFER_TIMEOUT` = 300s) — not the whole
    /// turn. 30 min gives generous headroom over that while keeping a leaked
    /// credential short-lived. This is pillbox's policy, not a claim about R2's
    /// own min/max — CF rejects an out-of-range value loudly at mint time.
    const TRANSFER_TTL_SECS: u64 = 1_800;
    const CF_API_BASE: &str = "https://api.cloudflare.com/client/v4";
    /// Read + write: the DO both restores (GET) and snapshots back (PUT).
    const PERMISSION: &str = "object-read-write";

    static WARNED_BUCKET_WIDE: Once = Once::new();

    /// Mint a fresh prefix-scoped temp credential for one workspace transfer
    /// (provision or finalize) when scoping is configured, else borrow the parent
    /// key (with a loud one-time warning). Called once per transfer so each
    /// credential only spans a single round-trip — a long turn between provision
    /// and finalize can't expire it.
    ///
    /// Fail-closed: with the API token set, scoping is *required* — a missing
    /// account id or an empty repo prefix (nothing narrower than the bucket to
    /// scope to) or a mint failure aborts the run rather than handing CF a
    /// bucket-wide key dressed up as scoped.
    pub(super) fn scope_for_transfer(parent: &S3Config) -> Result<Cow<'_, S3Config>> {
        let Some(api_token) = configured_token() else {
            // Only nudge toward scoping where it can actually apply — a non-R2
            // S3 host (MinIO/Backblaze/native S3) has no CF temp-credential API,
            // so the remediation would be inapplicable noise there.
            if account_id_from_endpoint(&parent.endpoint).is_some() {
                warn_bucket_wide();
            }
            return Ok(Cow::Borrowed(parent));
        };
        let account_id = account_id_from_endpoint(&parent.endpoint).ok_or_else(|| {
            PillboxError::config(
                "run",
                format!(
                    "{API_TOKEN_ENV} is set (R2 credential scoping requested) but the R2 endpoint \
                     `{}` isn't an `<account-id>.r2.cloudflarestorage.com` host, so the account id \
                     can't be derived to mint a scoped credential",
                    parent.endpoint
                ),
            )
        })?;
        let cf_prefix = cf_key_prefix(&parent.prefix).ok_or_else(|| {
            PillboxError::config(
                "run",
                format!(
                    "{API_TOKEN_ENV} is set (R2 credential scoping requested) but the workspace \
                     repo prefix is empty, so a scoped credential would still be bucket-wide. Set \
                     a non-empty workspace prefix, or unset {API_TOKEN_ENV} to accept bucket-wide \
                     reach explicitly"
                ),
            )
        })?;
        let scoped = mint(parent, &api_token, &account_id, &cf_prefix)?;
        Ok(Cow::Owned(scoped))
    }

    fn configured_token() -> Option<String> {
        std::env::var(API_TOKEN_ENV)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    fn warn_bucket_wide() {
        WARNED_BUCKET_WIDE.call_once(|| {
            eprintln!(
                "pillbox: note: handing the managed plane a bucket-wide R2 credential. \
                 Set {API_TOKEN_ENV} (a Cloudflare API token with R2 read+write) to mint a \
                 short-lived, prefix-scoped credential instead."
            );
        });
    }

    /// Parse the R2 account id out of an `<account-id>.r2.cloudflarestorage.com`
    /// endpoint (with or without scheme / trailing path). `None` for any other
    /// S3-compatible host (MinIO, Backblaze, native S3) — those have no CF
    /// temp-credential API, so scoping doesn't apply.
    fn account_id_from_endpoint(endpoint: &str) -> Option<String> {
        const SUFFIX: &str = ".r2.cloudflarestorage.com";
        let after_scheme = endpoint
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or(endpoint);
        let host = after_scheme
            .split('/')
            .next()? // strip any path
            .rsplit('@')
            .next()? // strip any userinfo
            .split(':')
            .next()?; // strip any port
        let account = host.strip_suffix(SUFFIX)?;
        if account.is_empty() || account.contains('.') {
            return None;
        }
        Some(account.to_string())
    }

    /// The CF object-key prefix to scope to: the repo prefix as an S3 key prefix
    /// (no leading slash, trailing slash so it matches the subtree). `None` for an
    /// empty repo prefix — there's nothing narrower than the bucket to scope to.
    fn cf_key_prefix(prefix: &str) -> Option<String> {
        let trimmed = prefix.trim_matches('/');
        if trimmed.is_empty() {
            None
        } else {
            Some(format!("{trimmed}/"))
        }
    }

    /// The `temp-access-credentials` HTTP API authorizes the mint with the Bearer
    /// CF API token and names the parent key by id only — it does NOT take the
    /// parent *secret* (that belongs to the client-side local-signing variant).
    /// So the bucket-wide parent secret never crosses to CF, which is the whole
    /// point of scoping. `prefixes` is always populated (the caller refuses an
    /// empty prefix), bounding the credential to `bucket/<prefix>`.
    #[derive(Serialize)]
    struct TempCredRequest<'a> {
        bucket: &'a str,
        #[serde(rename = "parentAccessKeyId")]
        parent_access_key_id: &'a str,
        permission: &'a str,
        #[serde(rename = "ttlSeconds")]
        ttl_seconds: u64,
        prefixes: Vec<String>,
    }

    fn build_request<'a>(parent: &'a S3Config, cf_prefix: &str, ttl: u64) -> TempCredRequest<'a> {
        TempCredRequest {
            bucket: &parent.bucket,
            parent_access_key_id: &parent.access_key,
            permission: PERMISSION,
            ttl_seconds: ttl,
            prefixes: vec![cf_prefix.to_string()],
        }
    }

    #[derive(Deserialize)]
    struct TempCredEnvelope {
        success: bool,
        #[serde(default)]
        result: Option<TempCred>,
        #[serde(default)]
        errors: Vec<serde_json::Value>,
    }

    #[derive(Deserialize)]
    struct TempCred {
        #[serde(rename = "accessKeyId")]
        access_key_id: String,
        #[serde(rename = "secretAccessKey")]
        secret_access_key: String,
        #[serde(rename = "sessionToken")]
        session_token: String,
    }

    /// Parse the CF envelope into a scoped [`S3Config`]: the same coordinates as
    /// `parent` (endpoint/region/bucket/prefix) with the temp key + its session
    /// token swapped in. Fail-closed — a non-`success` envelope or any missing /
    /// empty credential field is an error, never a partial credential.
    fn parse_scoped(body: &str, parent: &S3Config) -> Result<S3Config> {
        let env: TempCredEnvelope = serde_json::from_str(body)
            .with_context(|| format!("parse R2 temp-credential response: {body}"))?;
        if !env.success {
            return Err(PillboxError::runtime(
                "run",
                format!("R2 temp-credential mint failed: {:?}", env.errors),
            )
            .into());
        }
        let cred = env.result.ok_or_else(|| {
            PillboxError::runtime("run", "R2 temp-credential response had no `result`")
        })?;
        if cred.access_key_id.is_empty()
            || cred.secret_access_key.is_empty()
            || cred.session_token.is_empty()
        {
            return Err(PillboxError::runtime(
                "run",
                "R2 temp-credential response was missing a credential field",
            )
            .into());
        }
        Ok(S3Config {
            endpoint: parent.endpoint.clone(),
            region: parent.region.clone(),
            bucket: parent.bucket.clone(),
            prefix: parent.prefix.clone(),
            access_key: cred.access_key_id,
            secret_key: cred.secret_access_key,
            session_token: Some(cred.session_token),
        })
    }

    /// `POST <api>/accounts/<id>/r2/temp-access-credentials` — mint a scoped
    /// credential. The HTTP seam (the only un-unit-tested part); body + response
    /// parsing are pure and covered.
    fn mint(
        parent: &S3Config,
        api_token: &str,
        account_id: &str,
        cf_prefix: &str,
    ) -> Result<S3Config> {
        let url = format!("{CF_API_BASE}/accounts/{account_id}/r2/temp-access-credentials");
        let body = serde_json::to_string(&build_request(parent, cf_prefix, TRANSFER_TTL_SECS))
            .context("serialize R2 mint body")?;
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("build R2 temp-credential http client")?;
        let resp = client
            .post(&url)
            .header("content-type", "application/json")
            .bearer_auth(api_token)
            .body(body)
            .send()
            .with_context(|| format!("POST {url}"))?;
        let status = resp.status();
        let text = resp.text().context("read R2 temp-credential response")?;
        if !status.is_success() {
            return Err(PillboxError::runtime(
                "run",
                format!("R2 temp-credential mint returned HTTP {status}: {text}"),
            )
            .into());
        }
        parse_scoped(&text, parent)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn parent() -> S3Config {
            S3Config {
                endpoint: "https://abc123.r2.cloudflarestorage.com".into(),
                region: "auto".into(),
                bucket: "ws".into(),
                prefix: "proj/".into(),
                access_key: "PARENT_AK".into(),
                secret_key: "PARENT_SK".into(),
                session_token: None,
            }
        }

        #[test]
        fn account_id_parses_from_r2_endpoint_forms() {
            assert_eq!(
                account_id_from_endpoint("https://abc123.r2.cloudflarestorage.com"),
                Some("abc123".to_string())
            );
            assert_eq!(
                account_id_from_endpoint("abc123.r2.cloudflarestorage.com/ws"),
                Some("abc123".to_string())
            );
            // Not an R2 host → no scoping (MinIO/Backblaze/native S3).
            assert_eq!(account_id_from_endpoint("https://s3.amazonaws.com"), None);
            assert_eq!(account_id_from_endpoint("https://minio.local:9000"), None);
            // A sub-subdomain isn't a bare account id.
            assert_eq!(
                account_id_from_endpoint("https://x.abc123.r2.cloudflarestorage.com"),
                None
            );
        }

        #[test]
        fn request_scopes_to_the_prefix_subtree_with_rw() {
            let body = serde_json::to_value(build_request(&parent(), "proj/", 1800)).unwrap();
            assert_eq!(body["bucket"], "ws");
            assert_eq!(body["parentAccessKeyId"], "PARENT_AK");
            assert_eq!(body["permission"], "object-read-write");
            assert_eq!(body["ttlSeconds"], 1800);
            // Scoped to the repo prefix as a key subtree (no leading slash).
            assert_eq!(body["prefixes"][0], "proj/");
            // The parent SECRET must NOT cross to CF — the Bearer API token
            // authorizes the mint; only the parent key *id* is named.
            assert!(body.get("parentSecretAccessKey").is_none());
        }

        #[test]
        fn cf_key_prefix_requires_a_nonempty_prefix() {
            // The fail-closed foundation: an empty repo prefix yields None, which
            // scope_for_transfer turns into a hard error rather than a bucket-wide
            // mint. A non-empty prefix becomes a trailing-slash key subtree.
            assert_eq!(cf_key_prefix(""), None);
            assert_eq!(cf_key_prefix("/"), None);
            assert_eq!(cf_key_prefix("proj/"), Some("proj/".to_string()));
            assert_eq!(cf_key_prefix("/a/b"), Some("a/b/".to_string()));
        }

        #[test]
        fn parse_scoped_swaps_in_temp_key_and_session_token() {
            let resp = r#"{"success":true,"errors":[],"messages":[],
                "result":{"accessKeyId":"TMP_AK","secretAccessKey":"TMP_SK","sessionToken":"TMP_ST"}}"#;
            let scoped = parse_scoped(resp, &parent()).unwrap();
            assert_eq!(scoped.access_key, "TMP_AK");
            assert_eq!(scoped.secret_key, "TMP_SK");
            assert_eq!(scoped.session_token.as_deref(), Some("TMP_ST"));
            // Coordinates are untouched — same repo, just a narrower key.
            assert_eq!(scoped.endpoint, parent().endpoint);
            assert_eq!(scoped.bucket, "ws");
            assert_eq!(scoped.prefix, "proj/");
        }

        #[test]
        fn parse_scoped_fails_closed_on_unsuccess_or_missing_fields() {
            let failed = r#"{"success":false,"errors":[{"message":"bad token"}],"result":null}"#;
            assert!(parse_scoped(failed, &parent()).is_err());
            // success but a blank credential field is not a usable credential.
            let blank = r#"{"success":true,"errors":[],
                "result":{"accessKeyId":"TMP_AK","secretAccessKey":"","sessionToken":"TMP_ST"}}"#;
            assert!(parse_scoped(blank, &parent()).is_err());
        }
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

    /// Removes the managed env vars on drop so a panic between set and the
    /// assertions can't leak managed-routing state into another test.
    struct ManagedEnvGuard;
    impl Drop for ManagedEnvGuard {
        fn drop(&mut self) {
            std::env::remove_var("PILLBOX_MANAGED_DO_URL");
        }
    }

    /// The S3-backend-required guard: a local-filesystem rustic backend is refused
    /// (the DO can't reach a host-local repo); an S3 backend resolves to its config.
    #[test]
    fn require_s3_repo_rejects_local_accepts_s3() {
        use crate::workspace::rustic::{RusticBackend, RusticVariant, S3Config};
        use std::path::PathBuf;

        let local = RusticBackend {
            variant: RusticVariant::Local {
                repo_path: PathBuf::from("/tmp/repo"),
            },
            password_file: PathBuf::from("/tmp/pw"),
        };
        let err = require_s3_repo(&local).expect_err("local backend must be refused");
        assert!(
            err.to_string().contains("R2/S3 workspace backend"),
            "guard must name the missing backend, got: {err}"
        );

        let s3 = RusticBackend {
            variant: RusticVariant::S3(S3Config {
                endpoint: "https://r2.example.com".into(),
                region: "auto".into(),
                bucket: "ws".into(),
                prefix: String::new(),
                access_key: "AK".into(),
                secret_key: "SK".into(),
                session_token: None,
            }),
            password_file: PathBuf::from("/tmp/pw"),
        };
        assert_eq!(require_s3_repo(&s3).unwrap().bucket, "ws");
    }

    /// The mandatory HTTPS transport guard: a plaintext `http://` origin is refused
    /// (the body carries the R2 creds + the repo password), and an unset env names
    /// the missing var; an `https://` origin resolves to the per-session endpoint.
    #[test]
    fn resolve_https_endpoint_enforces_https() {
        // Serialize the env mutation under the shared test lock (held by
        // `with_isolated_home`) so a parallel test can't trample the var.
        crate::test_util::with_isolated_home("managed-https-guard", || {
            let _env = ManagedEnvGuard;

            // Unset → config error naming the var.
            std::env::remove_var("PILLBOX_MANAGED_DO_URL");
            let err = resolve_https_endpoint("sess-1").expect_err("unset env must error");
            assert!(err.to_string().contains("PILLBOX_MANAGED_DO_URL"));

            // Plaintext http:// → refused before any network touch.
            std::env::set_var("PILLBOX_MANAGED_DO_URL", "http://insecure.example.com");
            let err = resolve_https_endpoint("sess-1").expect_err("http:// must be refused");
            assert!(
                err.to_string().contains("non-HTTPS"),
                "guard must explain the HTTPS refusal, got: {err}"
            );

            // https:// → the per-session endpoint (matches `managed_endpoint`).
            std::env::set_var("PILLBOX_MANAGED_DO_URL", "https://w.workers.dev");
            let endpoint = resolve_https_endpoint("sess-1").expect("https resolves");
            assert_eq!(
                endpoint,
                "https://w.workers.dev/agents/session-gateway/sess-1"
            );
        });
    }

    /// The persisted record must NEVER carry the R2 creds or the repo password —
    /// it holds only the endpoint + session id (in the handle) + the result handle;
    /// creds are re-resolved from env each run. Build the record exactly as `run`
    /// does and assert no secret material survives the serialization.
    #[test]
    fn persisted_record_excludes_creds_and_password() {
        let handle = ManagedHandle {
            endpoint: "https://w.workers.dev/agents/session-gateway/sess-do".into(),
            do_session_id: "sess-do".into(),
        };
        let session = Session {
            id: "sess-do".into(),
            label: None,
            backend: BACKEND_MANAGED.to_string(),
            sandbox_id: serde_json::to_string(&handle).unwrap(),
            pty_pid: 0,
            agent_id: "opencode".into(),
            started_at: crate::session::now_rfc3339(),
            attached_pid: None,
            base_snapshot: Some("snap-base".into()),
            result_snapshot: Some("snap-result".into()),
            expires_at: None,
            guest_cwd: crate::agents::GUEST_WORKSPACE.to_string(),
            placement: session::Placement::Managed,
            server: Some(crate::session::ServerSession {
                agent_session_id: "sess-do".into(),
                model: "zai-coding-plan/glm-4.5-air".into(),
                temperature: None,
            }),
        };
        // Serialize both the on-disk (TOML) and JSON forms; neither may leak the
        // R2 access/secret keys or the repo password used during provisioning.
        let toml = toml::to_string(&session).unwrap();
        let json = serde_json::to_string(&session.to_json_value()).unwrap();
        for blob in [&toml, &json] {
            for secret in [
                "AKIA-secret-access",
                "super-secret-key",
                "repo-password-value",
            ] {
                assert!(
                    !blob.contains(secret),
                    "record must not carry secret material `{secret}`: {blob}"
                );
            }
        }
        // Positive: it DOES carry the non-secret correlation handles.
        assert!(toml.contains("sess-do"));
        assert!(toml.contains("snap-result"));
    }
}
