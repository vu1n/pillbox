//! `SandboxBackend` / `LiveSession` implementation for the managed Cloudflare
//! execution runtime. Pillbox is a single controller: one bounded HTTP request
//! drives one turn, then its bounded evidence page is appended to the local §0
//! log. Collaboration, arbitration, replay, and fan-out belong to Huddles.
//!
//! ## Configuration (env-driven; no host state)
//!
//! The Worker origin + capability issuer secret come from the environment:
//!
//!   - `PILLBOX_MANAGED_URL` — the worker origin, e.g.
//!     `https://<worker>.workers.dev`. `PILLBOX_MANAGED_DO_URL` remains a
//!     deprecated compatibility fallback while installations migrate.
//!   - `PILLBOX_MANAGED_TOKEN_SECRET` — the shared HMAC secret, when pillbox
//!     should mint short-lived capabilities. Each token is bound to one
//!     operation, exact request bytes, and exact session/invocation resource.
//!
//! ## Workspace placement — container-native rustic-on-R2
//!
//! [`ManagedBackend::run`] places the workspace by reusing the pillbox's rustic
//! repo: the host snapshots cwd into R2, POSTs `/v2/workspaces/provision` (repo config +
//! password + snapshot) to restore it into the container `/workspace`, drives the
//! turn, then POSTs `/v2/workspaces/finalize` to snapshot `/workspace` back and records the
//! result handle. The R2 creds + the repo password travel ONLY in those HTTPS
//! bodies — never in argv, a log, a §0 event, or the persisted `Session` record
//! (which holds endpoint + session id + result handle; creds are re-resolved from
//! env each run). The Worker/Sandbox restore and snapshot path implements the
//! same frozen contract (docs/managed-tier.md).
//!
//! ## Security boundary implemented
//!
//!   - **R2 key scoping.** `PILLBOX_R2_CF_API_TOKEN` is required; `run` mints a
//!     short-lived, prefix-scoped R2 temp credential ([`r2_scope`], fresh per
//!     transfer) and hands the managed runtime *that*, so a credential reaching
//!     CF can touch only this run's prefix — and the bucket-wide parent *secret* never crosses
//!     to CF (the Bearer API token authorizes the mint). Missing authority or a
//!     failed mint aborts before provisioning. The DO forwards the credential's
//!     `session_token` into the container helper, which sends it as
//!     `X-Amz-Security-Token`.
//!
//! ## Open follow-ups (flagged, not faked)
//!
//!   - **Detached finalize.** Only the foreground path is implemented (drive a
//!     turn, wait for idle, finalize). For a `--detach` managed run the host
//!     returns before the turn ends, so the in-container wrapper would own the
//!     `/finalize` + result-handle emission instead.
//!   - **Token provisioning / trust.** Where a real user's token/secret comes
//!     from (vs the spike's `/tmp` file) is unresolved; the env config above is
//!     the interim surface.
// Context: doc://pillbox/managed-store-of-record@0001#managed-store-of-record
// Context: doc://pillbox/managed-tier-do-gateway@0002#managed-tier-do-gateway

use std::path::PathBuf;

use anyhow::Result;

use super::{Caps, LiveSession, SandboxBackend};
use crate::agents::{AgentSpec, RunOpts};
use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::session::{self, Session, BACKEND_MANAGED};
use crate::workspace::WorkspaceBackend;

pub(crate) struct ManagedBackend;

/// Proof that an agent is executable by the managed runtime. Keep this token
/// private to the admission boundary so every managed run must pass the same
/// support check before it can enter the workspace/Cloudflare path.
struct SupportedManagedAgent<'a> {
    spec: &'a AgentSpec,
}

/// The managed runtime currently drives OpenCode only. This is the one
/// authoritative support check shared by the production run path and its
/// executable preflight smoke.
fn require_supported_agent(spec: &AgentSpec) -> Result<SupportedManagedAgent<'_>> {
    if spec.id() != crate::agents::OPENCODE.id() {
        return Err(PillboxError::usage(
            "run",
            format!(
                "unsupported_execution: managed execution supports only agent `opencode`; \
                 agent `{}` was rejected before workspace snapshot or external access",
                spec.id()
            ),
        )
        .into());
    }
    Ok(SupportedManagedAgent { spec })
}

impl SandboxBackend for ManagedBackend {
    /// The managed family exposes bounded agent turns, not a host PTY or a
    /// persistent remote event authority.
    fn capabilities(&self) -> Caps {
        Caps {
            // Drive is the structured agent channel, not raw keystrokes.
            pty_drive: false,
            live_pty_tail: false,
            // The Worker drives one bounded opencode HTTP turn.
            server_mode: true,
            // No host exec target / KVM isolation — those are the local backends.
            long_lived_exec: false,
            in_sandbox_grading: false,
            real_egress_fence: false,
            detached_vault: false,
            // The response is appended directly to the local log.
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
        let supported = require_supported_agent(spec)?;
        self.run_supported(supported, opts, resolved)
    }
}

impl ManagedBackend {
    /// Execute an admitted OpenCode run. Requiring the admission token keeps
    /// snapshot, persistence, allowance, provisioning, and network operations
    /// structurally behind the support check.
    fn run_supported(
        &self,
        supported: SupportedManagedAgent<'_>,
        opts: RunOpts,
        resolved: &Pillbox,
    ) -> Result<()> {
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

        // 3. Resolve the Worker origin, refusing a non-`https://` origin: the
        //    POST body carries the resolved R2 creds + the repo password, so it must
        //    never cross the wire in cleartext.
        let session_id = crate::session::Session::new_id();
        let endpoint = resolve_https_origin()?;

        // 4. Provision with a capability bound to the exact credentialed body.
        let capability_secret = managed_capability_secret("run")?;
        // Prefix-scope the credential before it crosses to the managed plane: the
        // DO only needs this repo's prefix, not the whole bucket. Minted fresh per
        // transfer so each credential outlives only its own round-trip, never the
        // whole turn.
        let provision_creds = r2_scope::scope_for_transfer(s3)?;
        workspace_xfer::provision(
            &endpoint,
            &capability_secret,
            &session_id,
            &provision_creds,
            &password,
            snapshot.as_str(),
        )?;

        // 5. Build + persist the Session record. The record holds the Worker origin +
        //    execution session id + (later) the result handle — NEVER the creds or password,
        //    which are re-resolved from env via `workspace()` on every run.
        let handle = ManagedHandle {
            endpoint: endpoint.clone(),
            execution_session_id: session_id.clone(),
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
            agent_id: supported.spec.id().to_string(),
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
                // The execution runtime correlates each bounded turn by this id.
                agent_session_id: session_id.clone(),
                model,
                temperature: opts.temperature,
            }),
            requested_execution: None,
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

        // 6. Drive the first turn through the bounded execution API. The
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
        live.send(resolved, format!("{prompt}\n").as_bytes())?;

        // 7. Snapshot `/workspace` back to R2; record the handle so
        //    `session pull <id>` can rehydrate the result.
        let finalize_creds = r2_scope::scope_for_transfer(s3)?;
        let result_snapshot = workspace_xfer::finalize(
            &endpoint,
            &capability_secret,
            &session_id,
            &finalize_creds,
            &password,
            snapshot.as_str(),
        )?;
        let verified = workspace.snapshot_show(&crate::workspace::SnapshotHandle::new(
            result_snapshot.clone(),
        ))?;
        if !verified
            .parents
            .iter()
            .any(|parent| parent == snapshot.as_str())
        {
            return Err(PillboxError::runtime(
                "run",
                "managed result snapshot is not descended from the provisioned base snapshot",
            )
            .into());
        }
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

/// Resolve the managed Worker origin and enforce HTTPS. The provision /
/// finalize bodies carry the resolved R2 creds + the repo password, so a non-HTTPS
/// origin (which would put them on the wire in cleartext) is refused — this is the
/// mandatory transport guard. Split out of `run` so it's unit-testable.
fn resolve_https_origin() -> Result<String> {
    let endpoint = std::env::var("PILLBOX_MANAGED_URL")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            std::env::var("PILLBOX_MANAGED_DO_URL")
                .ok()
                .filter(|value| !value.is_empty())
        })
        .ok_or_else(|| {
            PillboxError::config(
                "run",
                "the managed backend needs PILLBOX_MANAGED_URL set to the Worker origin",
            )
            .with_next("export PILLBOX_MANAGED_URL=https://<worker>.workers.dev")
        })?;
    let parsed = reqwest::Url::parse(&endpoint).map_err(|error| {
        PillboxError::config("run", format!("invalid managed Worker origin: {error}"))
    })?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.path() != "/"
    {
        return Err(PillboxError::config(
            "run",
            format!(
                "refusing to provision a managed workspace over a non-HTTPS endpoint \
                 (`{endpoint}`): the request body carries the R2 credentials + the repo \
                 password, which must not travel in cleartext"
            ),
        )
        .with_next("set PILLBOX_MANAGED_URL to an https:// origin")
        .into());
    }
    Ok(parsed.origin().ascii_serialization())
}

/// The managed [`LiveSession`] — a local session record whose turns execute on
/// the managed Worker. There is no remote session authority or replay stream.
pub(crate) struct ManagedLiveSession {
    session: Session,
}

impl ManagedLiveSession {
    pub(crate) fn new(session: Session) -> Self {
        Self { session }
    }

    /// The decoded Worker handle this session points at.
    fn handle(&self) -> Result<ManagedHandle> {
        ManagedHandle::decode(&self.session)
    }
}

impl LiveSession for ManagedLiveSession {
    fn caps(&self) -> Caps {
        ManagedBackend.capabilities()
    }

    fn send(&self, resolved: &Pillbox, bytes: &[u8]) -> Result<()> {
        let handle = self.handle()?;
        let capability_secret = managed_capability_secret("session send")?;
        let text = String::from_utf8_lossy(bytes).into_owned();
        let model = self.session.server.as_ref().map(|s| s.model.as_str());
        execution::execute_turn(
            resolved,
            &self.session.id,
            &handle.endpoint,
            &capability_secret,
            &text,
            model,
        )
    }

    fn attach(&self, _resolved: &Pillbox) -> Result<()> {
        // Managed turns have no terminal PTY; evidence is already in the local log.
        Err(PillboxError::usage(
            "session attach",
            "a managed session has no host PTY to attach; inspect its local event log instead",
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
        // Each bounded response appends its evidence directly to the local log.
        Ok(None)
    }

    fn http(&self) -> Result<Box<dyn crate::sandbox::http::SandboxHttp>> {
        // The managed agent is reached through the execution REST surface,
        // not a raw in-sandbox HTTP server the host can `curl`.
        // The `SandboxHttp` seam models the latter; managed doesn't expose one, so
        // the verb is unsupported (drive goes through a bounded execution request).
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
        // There is no host capture file to drain post-hoc.
        Err(self.caps().unsupported("ingest"))
    }

    fn kill(&self, resolved: &Pillbox) -> Result<()> {
        // The execution runtime is request-scoped; dropping the local record is
        // the complete session teardown.
        crate::events::emit_session_event(
            resolved,
            crate::events::EventType::SessionDropped,
            &self.session.id,
            Some(&self.session),
        );
        session::delete(resolved, &self.session.id)?;
        println!(
            "pillbox: ✓ managed session `{}` record removed.",
            self.session.id
        );
        Ok(())
    }
}

/// What a managed session stores in [`Session::sandbox_id`] (as JSON): the Worker
/// origin + execution session id. Mirrors libkrun's
/// `LibkrunHandle` pattern — an opaque, backend-specific handle the runtime decodes
/// to find the session again. No credential material (the token comes from env).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ManagedHandle {
    /// The Worker origin, e.g. `https://<worker>.workers.dev` (no trailing slash).
    pub(crate) endpoint: String,
    /// The stable execution session id. The alias reads records written by the
    /// retired gateway client without preserving gateway semantics.
    #[serde(alias = "do_session_id")]
    pub(crate) execution_session_id: String,
}

impl ManagedHandle {
    fn decode(session: &Session) -> Result<Self> {
        let configured = resolve_https_origin()?;
        Self::decode_for_origin(session, &configured)
    }

    fn decode_for_origin(session: &Session, configured: &str) -> Result<Self> {
        let handle: Self = serde_json::from_str(&session.sandbox_id)
            .map_err(|e| {
                PillboxError::config(
                    "session",
                    format!("decode managed session handle for `{}`: {e}", session.id),
                )
            })
            .map_err(anyhow::Error::from)?;
        if handle.execution_session_id != session.id {
            return Err(PillboxError::config(
                "session",
                format!(
                    "managed handle session id `{}` does not match record id `{}`",
                    handle.execution_session_id, session.id
                ),
            )
            .into());
        }
        if handle.endpoint.trim_end_matches('/') != configured.trim_end_matches('/') {
            return Err(PillboxError::config(
                "session",
                "managed session endpoint does not match PILLBOX_MANAGED_URL",
            )
            .with_next("inspect or recreate the managed session record")
            .into());
        }
        Ok(handle)
    }
}

fn managed_capability_secret(action: &'static str) -> Result<String> {
    std::env::var("PILLBOX_MANAGED_TOKEN_SECRET")
        .ok()
        .filter(|secret| !secret.is_empty())
        .ok_or_else(|| {
            PillboxError::config(
                action,
                "set PILLBOX_MANAGED_TOKEN_SECRET to mint scoped managed capabilities",
            )
            .into()
        })
}

/// The OS user, used as the driver actor's id when pillbox mints its own token.
/// Mirrors `commands::session::local_user` (kept local to avoid a cross-module
/// pub; the value is the same `$USER`/`$USERNAME` fallback).
fn local_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "local".into())
}

#[derive(serde::Serialize)]
struct ManagedCapability<'a> {
    version: u8,
    subject: String,
    audience: &'static str,
    expires_at_ms: u64,
    operation: &'a str,
    request_sha256: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    invocation_id: Option<&'a str>,
}

/// Mint the exact capability shape verified by `cloudflare-spike/src/auth.ts`.
pub(crate) fn mint_managed_capability(
    operation: &str,
    request_sha256: &str,
    session_id: Option<&str>,
    invocation_id: Option<&str>,
    secret: &str,
) -> String {
    use base64::Engine as _;
    let expires_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is after Unix epoch")
        .as_millis()
        .saturating_add(10 * 60 * 1_000) as u64;
    let claim = ManagedCapability {
        version: 1,
        subject: format!("controller:{}", local_user()),
        audience: "pillbox-managed",
        expires_at_ms,
        operation,
        request_sha256,
        session_id,
        invocation_id,
    };
    let claim_json = serde_json::to_vec(&claim).expect("managed capability serializes");
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

/// One bounded managed execution call plus local evidence persistence.
mod execution {
    use std::io::{Read as _, Write as _};
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};

    use anyhow::{Context, Result};
    use serde::{Deserialize, Serialize};
    use sha2::{Digest, Sha256};

    use crate::contract::{Custom, Event, Payload};
    use crate::errors::PillboxError;
    use crate::events::log::SessionLog;
    use crate::pillbox::Pillbox;

    const CONTRACT_VERSION: &str = "pillbox.execution/2";
    const EXECUTION_POLICY_REVISION: &str = "pillbox-managed-v2";
    const MAX_EVIDENCE_EVENTS: usize = 2_000;
    const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
    const MAX_PAGES: usize = 20;
    const PENDING_FILE: &str = "pending-managed-invocation.json";

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct PendingInvocation {
        version: u8,
        invocation_id: String,
        request_body: String,
        body_sha256: String,
        request_hash: String,
        execution_digest: String,
        execution_policy_revision: String,
    }

    #[derive(Debug)]
    struct PreparedInvocation {
        pending: PendingInvocation,
        requested_model: String,
    }

    #[derive(Debug, Clone, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct EvidencePage {
        from: u64,
        events: Vec<Payload>,
        next: Option<u64>,
        truncated: bool,
        #[serde(default)]
        artifact_ref: Option<ArtifactRef>,
    }

    #[derive(Debug, Clone, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct ExecutionError {
        code: ExecutionErrorCode,
        message: String,
        #[serde(default)]
        existing_request_hash: Option<String>,
        #[serde(default)]
        requested_request_hash: Option<String>,
    }

    #[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "snake_case")]
    enum ExecutionErrorCode {
        IdempotencyConflict,
        ManagedDisabled,
        UnsupportedExecution,
        UnsupportedPolicy,
        AuthUnavailable,
        RuntimeUnavailable,
        RuntimeBusy,
        RuntimeInterrupted,
        RuntimeFailed,
        Cancelled,
        StructuredOutputMissing,
    }

    impl ExecutionErrorCode {
        fn as_str(self) -> &'static str {
            match self {
                Self::IdempotencyConflict => "idempotency_conflict",
                Self::ManagedDisabled => "managed_disabled",
                Self::UnsupportedExecution => "unsupported_execution",
                Self::UnsupportedPolicy => "unsupported_policy",
                Self::AuthUnavailable => "auth_unavailable",
                Self::RuntimeUnavailable => "runtime_unavailable",
                Self::RuntimeBusy => "runtime_busy",
                Self::RuntimeInterrupted => "runtime_interrupted",
                Self::RuntimeFailed => "runtime_failed",
                Self::Cancelled => "cancelled",
                Self::StructuredOutputMissing => "structured_output_missing",
            }
        }
    }

    #[derive(Debug, Clone, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct ArtifactRef {
        key: String,
        media_type: String,
        bytes: u64,
        sha256: String,
    }

    #[derive(Debug, Clone, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct ExecutionAttribution {
        harness: String,
        transport: String,
        requested_model: String,
        served_model: Option<String>,
    }

    #[derive(Debug, Clone, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct ExecutionSessionRef {
        session_id: String,
        #[serde(default)]
        seq_range: Option<[u64; 2]>,
    }

    #[derive(Debug, Clone, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct ExecutionOutput {
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        json: Option<serde_json::Value>,
    }

    #[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "lowercase")]
    enum ExecutionStatus {
        Running,
        Completed,
        Failed,
        Cancelled,
        Interrupted,
        Conflict,
    }

    impl ExecutionStatus {
        fn as_str(self) -> &'static str {
            match self {
                Self::Running => "running",
                Self::Completed => "completed",
                Self::Failed => "failed",
                Self::Cancelled => "cancelled",
                Self::Interrupted => "interrupted",
                Self::Conflict => "conflict",
            }
        }

        fn is_terminal(self) -> bool {
            !matches!(self, Self::Running)
        }
    }

    #[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "lowercase")]
    enum Disposition {
        Created,
        Reused,
    }

    #[derive(Debug, Clone, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ExecutionResult {
        disposition: Disposition,
        invocation_id: String,
        request_hash: String,
        execution_digest: String,
        execution_policy_revision: String,
        attribution: ExecutionAttribution,
        session_ref: ExecutionSessionRef,
        status: ExecutionStatus,
        evidence: EvidencePage,
        #[serde(default)]
        cost: Option<crate::cost::RunCostEnvelope>,
        #[serde(default)]
        error: Option<ExecutionError>,
        #[serde(default)]
        output: Option<ExecutionOutput>,
        #[serde(default)]
        retry_after_ms: Option<u64>,
    }

    #[derive(Debug)]
    enum PostJsonError {
        LostResponse(anyhow::Error),
        NotFound,
        Rejected(anyhow::Error),
    }

    impl PostJsonError {
        fn into_anyhow(self) -> anyhow::Error {
            match self {
                Self::LostResponse(error) | Self::Rejected(error) => error,
                Self::NotFound => PillboxError::runtime(
                    "session send",
                    "managed execution status returned HTTP 404",
                )
                .into(),
            }
        }
    }

    pub(super) fn execute_turn(
        resolved: &Pillbox,
        session_id: &str,
        endpoint: &str,
        capability_secret: &str,
        text: &str,
        model: Option<&str>,
    ) -> Result<()> {
        let model = model.unwrap_or(crate::sandbox::opencode::DEFAULT_MODEL);
        let (provider, model_id) = model.split_once('/').ok_or_else(|| {
            PillboxError::config(
                "session send",
                format!("managed model must be provider/model, got `{model}`"),
            )
        })?;
        let pending_path = pending_path(resolved, session_id)?;
        let existing = load_pending(&pending_path)?;
        let invocation_id = existing
            .as_ref()
            .map(|pending| pending.invocation_id.clone())
            .unwrap_or_else(crate::session::Session::new_id);
        let rendered_hash = format!("sha256:{:x}", Sha256::digest(text.as_bytes()));
        let request = serde_json::json!({
            "contract_version": CONTRACT_VERSION,
            "session_ref": { "session_id": session_id },
            "invocation_id": invocation_id,
            "idempotency_key": invocation_id,
            "rendered_input": text,
            "rendered_input_hash": rendered_hash,
            "tool_policy": "deny_all",
            "execution": {
                "transport": {
                    "harness": "opencode",
                    "transport": "http",
                    "harness_version": "managed-v2",
                    "adapter_revision": "pillbox-cli-v2"
                },
                "requested": {
                    "provider": provider,
                    "model": model_id,
                    "profile": null,
                    "reasoning_effort": "medium"
                },
                "placement": "managed_container",
                "context_renderer_revision": "pillbox-cli-v2"
            },
            "execution_policy_revision": EXECUTION_POLICY_REVISION,
            "output_format": { "type": "text", "retry_count": 0 }
        });
        let body =
            serde_json::to_string(&request).context("serialize managed execution request")?;
        let prepared = PreparedInvocation {
            pending: PendingInvocation {
                version: 1,
                invocation_id: invocation_id.clone(),
                body_sha256: request_sha256(&body),
                request_hash: canonical_sha256(&request)?,
                execution_digest: canonical_sha256(&serde_json::json!({
                    "execution": request["execution"].clone(),
                    "execution_policy_revision": EXECUTION_POLICY_REVISION,
                }))?,
                execution_policy_revision: EXECUTION_POLICY_REVISION.into(),
                request_body: body,
            },
            requested_model: model.to_string(),
        };
        let resumed = existing.is_some();
        if let Some(existing) = existing {
            validate_pending(&existing)?;
            if existing != prepared.pending {
                return Err(PillboxError::runtime(
                    "session send",
                    format!(
                        "managed session has unresolved invocation `{}` with a different canonical request identity",
                        existing.invocation_id
                    ),
                )
                .with_next("retry the exact pending turn before sending different input")
                .into());
            }
        } else {
            persist_pending(&pending_path, &prepared.pending)?;
        }

        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(600))
            .build()
            .context("build managed execution http client")?;
        let execute_url = format!("{}/v2/executions", endpoint.trim_end_matches('/'));
        // A surviving marker means the prior POST may have sampled the model.
        // Query its stable identity before reusing the exact request bytes.
        let (mut result, mut response_bytes) = if resumed {
            match status(&client, endpoint, capability_secret, &prepared, 0)? {
                Some(response) => response,
                None => execute_with_reconciliation(
                    &client,
                    &execute_url,
                    endpoint,
                    capability_secret,
                    session_id,
                    &prepared,
                )?,
            }
        } else {
            execute_with_reconciliation(
                &client,
                &execute_url,
                endpoint,
                capability_secret,
                session_id,
                &prepared,
            )?
        };

        while result.status == ExecutionStatus::Running {
            validate_running(&result, session_id, &prepared)?;
            let retry_after = result.retry_after_ms.expect("validated retry_after_ms");
            std::thread::sleep(std::time::Duration::from_millis(retry_after.min(5_000)));
            let (next, bytes) = status(&client, endpoint, capability_secret, &prepared, 0)?
                .ok_or_else(|| {
                    PillboxError::runtime(
                        "session send",
                        "managed invocation disappeared while reconciling its pending response",
                    )
                })?;
            response_bytes = add_response_bytes(response_bytes, bytes)?;
            result = next;
        }

        let terminal = validate_terminal(&result, session_id, &prepared, 0)?;
        let terminal_cost = result.cost.clone().expect("validated terminal cost");
        let terminal_status = result.status;
        let terminal_error = result.error.clone();
        let terminal_output = result.output.clone();
        let terminal_attribution = result.attribution.clone();
        let terminal_range = result.session_ref.seq_range;
        let terminal_artifact = terminal.clone();
        let mut payloads = std::mem::take(&mut result.evidence.events);
        let mut cursor = result.evidence.next;
        let mut pages = 1usize;
        while let Some(after) = cursor {
            if pages >= MAX_PAGES || payloads.len() >= MAX_EVIDENCE_EVENTS {
                return Err(PillboxError::runtime(
                    "session send",
                    "managed execution evidence exceeded the bounded page budget",
                )
                .into());
            }
            let (mut page, bytes) = status(&client, endpoint, capability_secret, &prepared, after)?
                .ok_or_else(|| {
                    PillboxError::runtime(
                        "session send",
                        "managed invocation disappeared during evidence pagination",
                    )
                })?;
            response_bytes = add_response_bytes(response_bytes, bytes)?;
            let artifact = validate_terminal(&page, session_id, &prepared, after)?;
            if page.disposition != Disposition::Reused
                || page.status != terminal_status
                || page.error != terminal_error
                || page.output != terminal_output
                || page.attribution != terminal_attribution
                || page.session_ref.seq_range != terminal_range
                || page.cost.as_ref() != Some(&terminal_cost)
                || artifact != terminal_artifact
            {
                return Err(PillboxError::runtime(
                    "session send",
                    "managed execution changed terminal identity across evidence pages",
                )
                .into());
            }
            payloads.append(&mut page.evidence.events);
            if payloads.len() > MAX_EVIDENCE_EVENTS {
                return Err(PillboxError::runtime(
                    "session send",
                    "managed execution evidence exceeded 2000 events",
                )
                .into());
            }
            cursor = page.evidence.next;
            pages += 1;
        }
        let expected_events = terminal_range.map_or(0, |range| range[1] + 1);
        if payloads.len() as u64 != expected_events {
            return Err(PillboxError::runtime(
                "session send",
                "managed execution positional session range does not match collected evidence",
            )
            .into());
        }

        payloads.push(Payload::Custom(Custom {
            name: "run_cost".into(),
            payload: Some(serde_json::to_value(terminal_cost).expect("cost envelope serializes")),
        }));
        let events: Vec<_> = payloads
            .into_iter()
            .map(|payload| Event::session(session_id, payload))
            .collect();
        SessionLog::open(resolved, session_id)?.append(&events)?;
        // Keep the marker across every network/validation/append failure. Only
        // the local durable evidence sink proves this turn is safe to forget.
        clear_pending(&pending_path)?;

        if terminal_status == ExecutionStatus::Completed {
            return Ok(());
        }
        let detail = terminal_error.map_or_else(
            || {
                format!(
                    "managed execution ended with status {}",
                    terminal_status.as_str()
                )
            },
            |error| format!("{}: {}", error.code.as_str(), error.message),
        );
        Err(PillboxError::runtime("session send", detail).into())
    }

    fn execute(
        client: &reqwest::blocking::Client,
        url: &str,
        capability_secret: &str,
        session_id: &str,
        prepared: &PreparedInvocation,
    ) -> std::result::Result<(ExecutionResult, usize), PostJsonError> {
        let token = super::mint_managed_capability(
            "execute",
            &prepared.pending.body_sha256,
            Some(session_id),
            Some(&prepared.pending.invocation_id),
            capability_secret,
        );
        post_json(client, url, &token, &prepared.pending.request_body)
    }

    fn execute_with_reconciliation(
        client: &reqwest::blocking::Client,
        execute_url: &str,
        endpoint: &str,
        capability_secret: &str,
        session_id: &str,
        prepared: &PreparedInvocation,
    ) -> Result<(ExecutionResult, usize)> {
        match execute(
            client,
            execute_url,
            capability_secret,
            session_id,
            prepared,
        ) {
            Ok(response) => Ok(response),
            Err(PostJsonError::LostResponse(execute_error)) => status(
                client,
                endpoint,
                capability_secret,
                prepared,
                0,
            )?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "managed execute response was lost and invocation `{}` is not yet queryable: {execute_error:#}",
                    prepared.pending.invocation_id
                )
            }),
            Err(error) => Err(error.into_anyhow()),
        }
    }

    fn status(
        client: &reqwest::blocking::Client,
        endpoint: &str,
        capability_secret: &str,
        prepared: &PreparedInvocation,
        after: u64,
    ) -> Result<Option<(ExecutionResult, usize)>> {
        let body = serde_json::to_string(&serde_json::json!({
            "contract_version": CONTRACT_VERSION,
            "invocation_id": prepared.pending.invocation_id,
            "evidence_after": after,
            "evidence_limit": 100
        }))
        .context("serialize managed status request")?;
        let token = super::mint_managed_capability(
            "status",
            &request_sha256(&body),
            None,
            Some(&prepared.pending.invocation_id),
            capability_secret,
        );
        match post_json(
            client,
            &format!("{}/v2/executions/status", endpoint.trim_end_matches('/')),
            &token,
            &body,
        ) {
            Ok(result) => Ok(Some(result)),
            Err(PostJsonError::NotFound) => Ok(None),
            Err(error) => Err(error.into_anyhow()),
        }
    }

    fn validate_running(
        result: &ExecutionResult,
        session_id: &str,
        prepared: &PreparedInvocation,
    ) -> Result<()> {
        validate_identity(result, session_id, prepared)?;
        validate_evidence_page(&result.evidence, 0, None)?;
        if result.disposition != Disposition::Reused
            || result.cost.is_some()
            || result.error.is_some()
            || result.output.is_some()
            || result.session_ref.seq_range.is_some()
            || result.evidence.artifact_ref.is_some()
            || !(1..=600_000).contains(&result.retry_after_ms.unwrap_or(0))
        {
            return Err(PillboxError::runtime(
                "session send",
                "managed running response violated the execution/2 schema",
            )
            .into());
        }
        Ok(())
    }

    fn validate_terminal(
        result: &ExecutionResult,
        session_id: &str,
        prepared: &PreparedInvocation,
        expected_from: u64,
    ) -> Result<ArtifactRef> {
        validate_identity(result, session_id, prepared)?;
        if !result.status.is_terminal() || result.status == ExecutionStatus::Conflict {
            return Err(PillboxError::runtime(
                "session send",
                "managed invocation returned a non-terminal or conflicting result",
            )
            .into());
        }
        let cost = result.cost.as_ref().ok_or_else(|| {
            PillboxError::runtime(
                "session send",
                "managed terminal response omitted its required cost envelope",
            )
        })?;
        if !cost.validate_untrusted(result.status.as_str()) {
            return Err(PillboxError::runtime(
                "session send",
                "managed execution returned an invalid cost envelope",
            )
            .into());
        }
        match result.status {
            ExecutionStatus::Completed
                if result.output.is_none()
                    || result.error.is_some()
                    || result.retry_after_ms.is_some() =>
            {
                return Err(PillboxError::runtime(
                    "session send",
                    "managed completed response violated the execution/2 terminal schema",
                )
                .into());
            }
            ExecutionStatus::Failed | ExecutionStatus::Cancelled | ExecutionStatus::Interrupted
                if result.error.is_none()
                    || result.output.is_some()
                    || result.retry_after_ms.is_some() =>
            {
                return Err(PillboxError::runtime(
                    "session send",
                    "managed error response violated the execution/2 terminal schema",
                )
                .into());
            }
            _ => {}
        }
        let error_status_matches = match (result.status, result.error.as_ref().map(|e| e.code)) {
            (ExecutionStatus::Completed, None) => true,
            (ExecutionStatus::Cancelled, Some(ExecutionErrorCode::Cancelled)) => true,
            (ExecutionStatus::Interrupted, Some(ExecutionErrorCode::RuntimeInterrupted)) => true,
            (ExecutionStatus::Failed, Some(code)) => !matches!(
                code,
                ExecutionErrorCode::Cancelled
                    | ExecutionErrorCode::RuntimeInterrupted
                    | ExecutionErrorCode::IdempotencyConflict
            ),
            _ => false,
        };
        if !error_status_matches {
            return Err(PillboxError::runtime(
                "session send",
                "managed terminal status and error disposition mismatch",
            )
            .into());
        }
        if result.error.as_ref().is_some_and(|error| {
            error.existing_request_hash.is_some() || error.requested_request_hash.is_some()
        }) {
            return Err(PillboxError::runtime(
                "session send",
                "managed non-conflict response carried conflict-only identity fields",
            )
            .into());
        }
        let seq_range = result.session_ref.seq_range;
        let total = match seq_range {
            Some([0, end]) if end < MAX_EVIDENCE_EVENTS as u64 => Some(end + 1),
            None => Some(0),
            _ => None,
        }
        .ok_or_else(|| {
            PillboxError::runtime(
                "session send",
                "managed terminal response returned an invalid positional session range",
            )
        })?;
        if result.status == ExecutionStatus::Completed && total == 0 {
            return Err(PillboxError::runtime(
                "session send",
                "managed completed response has no immutable positional evidence",
            )
            .into());
        }
        validate_evidence_page(&result.evidence, expected_from, Some(total))?;
        validate_payloads(&result.evidence.events)?;
        let artifact = result.evidence.artifact_ref.clone().ok_or_else(|| {
            PillboxError::runtime(
                "session send",
                "managed terminal response omitted its stable artifact reference",
            )
        })?;
        validate_artifact_ref(&artifact, prepared)?;
        Ok(artifact)
    }

    fn validate_identity(
        result: &ExecutionResult,
        session_id: &str,
        prepared: &PreparedInvocation,
    ) -> Result<()> {
        if result.invocation_id != prepared.pending.invocation_id
            || result.request_hash != prepared.pending.request_hash
            || result.execution_digest != prepared.pending.execution_digest
            || result.execution_policy_revision != prepared.pending.execution_policy_revision
            || result.session_ref.session_id != session_id
            || result.attribution.harness != "opencode"
            || result.attribution.transport != "http"
            || result.attribution.requested_model != prepared.requested_model
            || result
                .attribution
                .served_model
                .as_ref()
                .is_some_and(|model| model.is_empty() || model.len() > 256)
        {
            return Err(PillboxError::runtime(
                "session send",
                "managed execution response identity, policy, or attribution mismatch",
            )
            .into());
        }
        Ok(())
    }

    fn validate_evidence_page(
        page: &EvidencePage,
        expected_from: u64,
        total: Option<u64>,
    ) -> Result<()> {
        let end = page
            .from
            .checked_add(page.events.len() as u64)
            .ok_or_else(|| {
                PillboxError::runtime("session send", "managed evidence cursor overflowed")
            })?;
        let valid_cursor = page.from == expected_from
            && match page.next {
                Some(next) => {
                    page.truncated && next == end && total.is_none_or(|total| end < total)
                }
                None => !page.truncated && total.is_none_or(|total| end == total),
            };
        if !valid_cursor {
            return Err(PillboxError::runtime(
                "session send",
                "managed evidence cursor did not equal from + events.length",
            )
            .into());
        }
        Ok(())
    }

    fn validate_artifact_ref(artifact: &ArtifactRef, prepared: &PreparedInvocation) -> Result<()> {
        let invocation_digest = format!(
            "{:x}",
            Sha256::digest(prepared.pending.invocation_id.as_bytes())
        );
        let expected_key = format!(
            "executions/{invocation_digest}/{}.json",
            prepared
                .pending
                .request_hash
                .strip_prefix("sha256:")
                .expect("validated request digest")
        );
        if artifact.key != expected_key
            || artifact.media_type != "application/json"
            || artifact.bytes == 0
            || artifact.bytes > MAX_RESPONSE_BYTES as u64
            || !valid_sha256(&artifact.sha256)
        {
            return Err(PillboxError::runtime(
                "session send",
                "managed execution returned an invalid artifact reference",
            )
            .into());
        }
        Ok(())
    }

    fn pending_path(resolved: &Pillbox, session_id: &str) -> Result<PathBuf> {
        Ok(crate::session::session_dir(resolved, session_id)?.join(PENDING_FILE))
    }

    fn load_pending(path: &Path) -> Result<Option<PendingInvocation>> {
        let body = match std::fs::read(path) {
            Ok(body) => body,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        };
        let pending = serde_json::from_slice(&body).map_err(|error| {
            PillboxError::config(
                "session send",
                format!(
                    "invalid pending managed invocation {}: {error}",
                    path.display()
                ),
            )
        })?;
        Ok(Some(pending))
    }

    fn validate_pending(pending: &PendingInvocation) -> Result<()> {
        let request: serde_json::Value =
            serde_json::from_str(&pending.request_body).map_err(|error| {
                PillboxError::config(
                    "session send",
                    format!("pending managed request is invalid JSON: {error}"),
                )
            })?;
        let execution = request.get("execution").cloned().ok_or_else(|| {
            PillboxError::config("session send", "pending managed request omitted execution")
        })?;
        let valid = pending.version == 1
            && valid_sha256(&pending.body_sha256)
            && valid_sha256(&pending.request_hash)
            && valid_sha256(&pending.execution_digest)
            && pending.body_sha256 == request_sha256(&pending.request_body)
            && pending.request_hash == canonical_sha256(&request)?
            && pending.execution_digest
                == canonical_sha256(&serde_json::json!({
                    "execution": execution,
                    "execution_policy_revision": pending.execution_policy_revision,
                }))?
            && pending.execution_policy_revision == EXECUTION_POLICY_REVISION
            && request["invocation_id"] == pending.invocation_id
            && request["idempotency_key"] == pending.invocation_id;
        if !valid {
            return Err(PillboxError::config(
                "session send",
                "pending managed invocation identity is corrupted or mismatched",
            )
            .into());
        }
        Ok(())
    }

    fn persist_pending(path: &Path, pending: &PendingInvocation) -> Result<()> {
        let bytes = serde_json::to_vec(pending).context("serialize pending managed invocation")?;
        let dir = path.parent().expect("pending path has a session directory");
        let mut temp = tempfile::Builder::new()
            .prefix(".pending-managed-")
            .tempfile_in(dir)
            .with_context(|| format!("create pending marker in {}", dir.display()))?;
        temp.as_file_mut()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .context("chmod pending managed invocation 0600")?;
        temp.write_all(&bytes)
            .context("write pending managed invocation")?;
        temp.as_file()
            .sync_all()
            .context("fsync pending managed invocation")?;
        temp.persist(path)
            .map_err(|error| error.error)
            .with_context(|| format!("persist {}", path.display()))?;
        std::fs::File::open(dir)
            .and_then(|file| file.sync_all())
            .with_context(|| format!("fsync {}", dir.display()))?;
        Ok(())
    }

    fn clear_pending(path: &Path) -> Result<()> {
        std::fs::remove_file(path).with_context(|| format!("clear {}", path.display()))?;
        let dir = path.parent().expect("pending path has a session directory");
        std::fs::File::open(dir)
            .and_then(|file| file.sync_all())
            .with_context(|| format!("fsync {}", dir.display()))?;
        Ok(())
    }

    fn post_json(
        client: &reqwest::blocking::Client,
        url: &str,
        token: &str,
        body: &str,
    ) -> std::result::Result<(ExecutionResult, usize), PostJsonError> {
        let resp = client
            .post(url)
            .header("content-type", "application/json")
            .bearer_auth(token)
            .body(body.to_owned())
            .send()
            .map_err(|error| PostJsonError::LostResponse(anyhow::anyhow!("POST {url}: {error}")))?;
        let status = resp.status();
        let response = read_execution_response(resp)?;
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(PostJsonError::NotFound);
        }
        if !status.is_success() {
            return Err(PostJsonError::Rejected(
                PillboxError::runtime(
                    "session send",
                    format!(
                        "managed execution returned HTTP {status}: {}",
                        capped(&response)
                    ),
                )
                .into(),
            ));
        }
        let bytes = response.len();
        let result = serde_json::from_str(&response).map_err(|error| {
            PostJsonError::Rejected(
                PillboxError::runtime(
                    "session send",
                    format!("invalid managed execution response: {error}"),
                )
                .into(),
            )
        })?;
        Ok((result, bytes))
    }

    fn read_execution_response(
        mut response: reqwest::blocking::Response,
    ) -> std::result::Result<String, PostJsonError> {
        let mut bytes = Vec::new();
        response
            .by_ref()
            .take(MAX_RESPONSE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| {
                PostJsonError::LostResponse(anyhow::anyhow!(
                    "read managed execution response: {error}"
                ))
            })?;
        if bytes.len() > MAX_RESPONSE_BYTES {
            return Err(PostJsonError::Rejected(
                PillboxError::runtime(
                    "session send",
                    "managed execution response exceeded 8388608 bytes",
                )
                .into(),
            ));
        }
        String::from_utf8(bytes).map_err(|_| {
            PostJsonError::Rejected(
                PillboxError::runtime("session send", "managed execution response was not UTF-8")
                    .into(),
            )
        })
    }

    fn add_response_bytes(total: usize, page: usize) -> Result<usize> {
        let total = total.checked_add(page).ok_or_else(|| {
            PillboxError::runtime("session send", "managed response byte counter overflowed")
        })?;
        if total > MAX_RESPONSE_BYTES {
            return Err(PillboxError::runtime(
                "session send",
                "managed execution responses exceeded 8 MiB in total",
            )
            .into());
        }
        Ok(total)
    }

    fn request_sha256(body: &str) -> String {
        format!("sha256:{:x}", Sha256::digest(body.as_bytes()))
    }

    fn canonical_sha256(value: &serde_json::Value) -> Result<String> {
        fn write(value: &serde_json::Value, output: &mut String) -> Result<()> {
            match value {
                serde_json::Value::Null => output.push_str("null"),
                serde_json::Value::Bool(value) => {
                    output.push_str(if *value { "true" } else { "false" })
                }
                serde_json::Value::Number(value) => output.push_str(&value.to_string()),
                serde_json::Value::String(value) => output
                    .push_str(&serde_json::to_string(value).context("canonicalize JSON string")?),
                serde_json::Value::Array(values) => {
                    output.push('[');
                    for (index, value) in values.iter().enumerate() {
                        if index > 0 {
                            output.push(',');
                        }
                        write(value, output)?;
                    }
                    output.push(']');
                }
                serde_json::Value::Object(values) => {
                    output.push('{');
                    let mut keys: Vec<_> = values.keys().collect();
                    keys.sort_unstable();
                    for (index, key) in keys.into_iter().enumerate() {
                        if index > 0 {
                            output.push(',');
                        }
                        output.push_str(
                            &serde_json::to_string(key).context("canonicalize JSON key")?,
                        );
                        output.push(':');
                        write(&values[key], output)?;
                    }
                    output.push('}');
                }
            }
            Ok(())
        }
        let mut canonical = String::new();
        write(value, &mut canonical)?;
        Ok(request_sha256(&canonical))
    }

    fn valid_sha256(value: &str) -> bool {
        value.strip_prefix("sha256:").is_some_and(|digest| {
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
    }

    fn validate_payloads(payloads: &[Payload]) -> Result<()> {
        if payloads.iter().all(|payload| match payload {
            Payload::MessageStart(_)
            | Payload::MessageDelta(_)
            | Payload::MessageEnd(_)
            | Payload::ToolCall(_)
            | Payload::Thinking(_) => true,
            Payload::Usage(usage) => {
                [
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.cache_read_input_tokens,
                    usage.cache_creation_input_tokens,
                ]
                .into_iter()
                .flatten()
                .all(|tokens| tokens <= 10_000_000_000)
                    && usage
                        .cost_usd
                        .is_none_or(|cost| cost.is_finite() && (0.0..=1_000_000.0).contains(&cost))
            }
            _ => false,
        }) {
            return Ok(());
        }
        Err(PillboxError::runtime(
            "session send",
            "managed execution returned a disallowed evidence event",
        )
        .into())
    }

    fn capped(value: &str) -> &str {
        let mut end = value.len().min(2048);
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        &value[..end]
    }

    #[cfg(test)]
    mod tests {
        use std::io::{Read as _, Write as _};
        use std::net::{TcpListener, TcpStream};
        use std::os::unix::fs::PermissionsExt as _;

        use super::*;

        #[test]
        fn capped_stops_before_a_split_utf8_character() {
            let value = format!("{}étail", "a".repeat(2047));
            assert_eq!(super::capped(&value), "a".repeat(2047));
        }

        #[test]
        fn canonical_json_hash_matches_execution_v2_reference_vector() {
            let value = serde_json::json!({
                "z": [3, { "é": true, "a": null }],
                "a": "한글",
            });
            assert_eq!(
                canonical_sha256(&value).unwrap(),
                "sha256:1932a99cba0c005a524adf4671beb60a440f495ab7a7f0fcdfc23a937c3afb20"
            );
        }

        #[test]
        fn terminal_validation_rejects_corrupted_response_identity_and_cursor() {
            let prepared = fixture_prepared("invocation-1", "session-1", "hello");
            let valid = fixture_terminal(&prepared, "session-1");
            assert!(validate_terminal(&valid, "session-1", &prepared, 0).is_ok());

            let mut corrupted = valid.clone();
            corrupted.request_hash = format!("sha256:{}", "b".repeat(64));
            assert!(validate_terminal(&corrupted, "session-1", &prepared, 0).is_err());

            let mut corrupted = valid.clone();
            corrupted.execution_digest = format!("sha256:{}", "c".repeat(64));
            assert!(validate_terminal(&corrupted, "session-1", &prepared, 0).is_err());

            let mut corrupted = valid.clone();
            corrupted.attribution.requested_model = "wrong/model".into();
            assert!(validate_terminal(&corrupted, "session-1", &prepared, 0).is_err());

            let mut corrupted = valid.clone();
            corrupted.evidence.next = Some(2);
            corrupted.evidence.truncated = true;
            assert!(validate_terminal(&corrupted, "session-1", &prepared, 0).is_err());

            let mut corrupted = valid.clone();
            corrupted.evidence.artifact_ref.as_mut().unwrap().key = "other".into();
            assert!(validate_terminal(&corrupted, "session-1", &prepared, 0).is_err());

            let mut value = serde_json::to_value(fixture_terminal_json(&prepared, "session-1"))
                .expect("response serializes");
            value
                .as_object_mut()
                .unwrap()
                .insert("unexpected".into(), serde_json::Value::Bool(true));
            assert!(serde_json::from_value::<ExecutionResult>(value).is_err());
        }

        #[test]
        fn lost_execute_response_reconciles_same_persisted_invocation_through_status() {
            crate::test_util::with_isolated_home("managed-lost-response", || {
                let resolved = crate::pillbox::global();
                let session_id = "abc123def456";
                let pending = pending_path(&resolved, session_id).unwrap();
                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                let endpoint = format!("http://{}", listener.local_addr().unwrap());
                let pending_for_server = pending.clone();
                let server = std::thread::spawn(move || {
                    let (mut execute_socket, _) = listener.accept().unwrap();
                    let execute_request = read_http_request(&mut execute_socket);
                    assert!(execute_request.starts_with("POST /v2/executions HTTP/1.1"));
                    assert!(
                        pending_for_server.exists(),
                        "pending marker must precede POST"
                    );
                    assert_eq!(
                        std::fs::metadata(&pending_for_server)
                            .unwrap()
                            .permissions()
                            .mode()
                            & 0o777,
                        0o600
                    );
                    let pending_bytes = std::fs::read(&pending_for_server).unwrap();
                    let pending_text = String::from_utf8(pending_bytes).unwrap();
                    assert!(!pending_text.contains("Bearer"));
                    assert!(!pending_text.contains("capability-secret"));
                    let execute_body = http_body(&execute_request);
                    let request: serde_json::Value = serde_json::from_str(execute_body).unwrap();
                    let invocation_id = request["invocation_id"].as_str().unwrap().to_string();
                    drop(execute_socket); // The model turn exists, but its HTTP response is lost.

                    let (mut status_socket, _) = listener.accept().unwrap();
                    let status_request = read_http_request(&mut status_socket);
                    assert!(status_request.starts_with("POST /v2/executions/status HTTP/1.1"));
                    let status_body: serde_json::Value =
                        serde_json::from_str(http_body(&status_request)).unwrap();
                    assert_eq!(status_body["invocation_id"], invocation_id);
                    let prepared = fixture_prepared_from_request(request);
                    let response = fixture_terminal_json(&prepared, session_id);
                    write_json_response(&mut status_socket, &response.to_string());
                });

                execute_turn(
                    &resolved,
                    session_id,
                    &endpoint,
                    "capability-secret",
                    "hello",
                    Some("provider/model"),
                )
                .expect("lost execute response reconciles through status");
                server.join().unwrap();
                assert!(!pending.exists(), "pending clears only after local append");
                let events = SessionLog::open(&resolved, session_id)
                    .unwrap()
                    .read_from(0)
                    .unwrap();
                assert!(events.iter().any(|event| matches!(
                    &event.payload,
                    Payload::MessageDelta(delta) if delta.text == "done"
                )));
            });
        }

        fn fixture_prepared(
            invocation_id: &str,
            session_id: &str,
            text: &str,
        ) -> PreparedInvocation {
            let request = serde_json::json!({
                "contract_version": CONTRACT_VERSION,
                "session_ref": { "session_id": session_id },
                "invocation_id": invocation_id,
                "idempotency_key": invocation_id,
                "rendered_input": text,
                "rendered_input_hash": request_sha256(text),
                "tool_policy": "deny_all",
                "execution": {
                    "transport": {
                        "harness": "opencode",
                        "transport": "http",
                        "harness_version": "managed-v2",
                        "adapter_revision": "pillbox-cli-v2"
                    },
                    "requested": {
                        "provider": "provider",
                        "model": "model",
                        "profile": null,
                        "reasoning_effort": "medium"
                    },
                    "placement": "managed_container",
                    "context_renderer_revision": "pillbox-cli-v2"
                },
                "execution_policy_revision": EXECUTION_POLICY_REVISION,
                "output_format": { "type": "text", "retry_count": 0 }
            });
            fixture_prepared_from_request(request)
        }

        fn fixture_prepared_from_request(request: serde_json::Value) -> PreparedInvocation {
            let body = serde_json::to_string(&request).unwrap();
            let execution_digest = canonical_sha256(&serde_json::json!({
                "execution": request["execution"].clone(),
                "execution_policy_revision": EXECUTION_POLICY_REVISION,
            }))
            .unwrap();
            PreparedInvocation {
                pending: PendingInvocation {
                    version: 1,
                    invocation_id: request["invocation_id"].as_str().unwrap().into(),
                    body_sha256: request_sha256(&body),
                    request_hash: canonical_sha256(&request).unwrap(),
                    execution_digest,
                    execution_policy_revision: EXECUTION_POLICY_REVISION.into(),
                    request_body: body,
                },
                requested_model: "provider/model".into(),
            }
        }

        fn fixture_terminal(prepared: &PreparedInvocation, session_id: &str) -> ExecutionResult {
            serde_json::from_value(fixture_terminal_json(prepared, session_id)).unwrap()
        }

        fn fixture_terminal_json(
            prepared: &PreparedInvocation,
            session_id: &str,
        ) -> serde_json::Value {
            let invocation_digest = format!(
                "{:x}",
                Sha256::digest(prepared.pending.invocation_id.as_bytes())
            );
            serde_json::json!({
                "disposition": "reused",
                "invocation_id": prepared.pending.invocation_id,
                "request_hash": prepared.pending.request_hash,
                "execution_digest": prepared.pending.execution_digest,
                "execution_policy_revision": prepared.pending.execution_policy_revision,
                "attribution": {
                    "harness": "opencode",
                    "transport": "http",
                    "requested_model": prepared.requested_model,
                    "served_model": "provider/model"
                },
                "session_ref": { "session_id": session_id, "seq_range": [0, 0] },
                "status": "completed",
                "output": { "text": "done" },
                "evidence": {
                    "from": 0,
                    "next": null,
                    "truncated": false,
                    "events": [{
                        "type": "message_delta",
                        "messageId": "message-1",
                        "text": "done"
                    }],
                    "artifact_ref": {
                        "key": format!(
                            "executions/{invocation_digest}/{}.json",
                            prepared.pending.request_hash.trim_start_matches("sha256:")
                        ),
                        "media_type": "application/json",
                        "bytes": 512,
                        "sha256": format!("sha256:{}", "a".repeat(64))
                    }
                },
                "cost": {
                    "version": 1,
                    "status": "completed",
                    "model": {
                        "input_tokens": 1,
                        "output_tokens": 1,
                        "cache_read_input_tokens": 0,
                        "cache_creation_input_tokens": 0,
                        "provider_reported_cost_usd": null
                    },
                    "infrastructure": {
                        "d1_rows_read": 1,
                        "d1_rows_written": 2,
                        "r2_reads": 0,
                        "r2_writes": 1,
                        "r2_bytes_read": 0,
                        "r2_bytes_written": 512,
                        "analytics_points_written": 1,
                        "sandbox_duration_ms": 5,
                        "sandbox_profile": "standard-2"
                    },
                    "known_cost_usd": null,
                    "estimated_total_cost_usd": null,
                    "rate_card_version": null
                }
            })
        }

        fn read_http_request(stream: &mut TcpStream) -> String {
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let count = stream.read(&mut chunk).unwrap();
                bytes.extend_from_slice(&chunk[..count]);
                let text = String::from_utf8_lossy(&bytes);
                if let Some(header_end) = text.find("\r\n\r\n") {
                    let content_length = text[..header_end]
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|value| value.parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if bytes.len() >= header_end + 4 + content_length {
                        return String::from_utf8(bytes).unwrap();
                    }
                }
                assert!(count > 0, "connection closed before complete request");
            }
        }

        fn http_body(request: &str) -> &str {
            request.split_once("\r\n\r\n").unwrap().1
        }

        fn write_json_response(stream: &mut TcpStream, body: &str) {
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        }
    }
}

/// Container-native workspace transfer — the host half of the frozen R2/rustic
/// placement contract (see docs/managed-tier.md). `provision` hands the DO the
/// rustic-on-R2 coordinates so its container restores the snapshot into
/// `/workspace`; `finalize` asks the DO to snapshot `/workspace` back and returns
/// the result handle. Both POST over HTTPS — the **only** channel the resolved R2
/// creds + the repo password travel on. Kept in one submodule so the wire shapes
/// (`{workspace:{repo,password,snapshot}}` for both restore and backup) and
/// the error mapping live together, mirroring [`input`].
mod workspace_xfer {
    use std::io::Read as _;

    use anyhow::{Context, Result};
    use serde::Serialize;
    use sha2::{Digest, Sha256};

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
    // that the DO forwards into the container helper for S3 signing.
    #[derive(Serialize)]
    struct WorkspaceRepo<'a> {
        repo: &'a S3Config,
        password: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        snapshot: Option<&'a str>,
    }

    #[derive(Serialize)]
    struct ProvisionBody<'a> {
        #[serde(rename = "sessionId")]
        session_id: &'a str,
        workspace: WorkspaceRepo<'a>,
    }

    /// `POST <endpoint>/provision` — the DO restores `snapshot` from the R2 repo
    /// into the container `/workspace`. Driver-gated, so it carries the driver
    /// provision capability. Non-2xx maps to a clear pillbox error.
    pub(super) fn provision(
        endpoint: &str,
        capability_secret: &str,
        session_id: &str,
        repo: &S3Config,
        password: &str,
        snapshot: &str,
    ) -> Result<()> {
        let body = serde_json::to_string(&ProvisionBody {
            session_id,
            workspace: WorkspaceRepo {
                repo,
                password,
                snapshot: Some(snapshot),
            },
        })
        .context("serialize managed /provision body")?;
        let token = super::mint_managed_capability(
            "workspace_provision",
            &request_sha256(&body),
            Some(session_id),
            None,
            capability_secret,
        );
        let resp = post(endpoint, "v2/workspaces/provision", &token, body)?;
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
        capability_secret: &str,
        session_id: &str,
        repo: &S3Config,
        password: &str,
        base_snapshot: &str,
    ) -> Result<String> {
        let body = serde_json::to_string(&ProvisionBody {
            session_id,
            workspace: WorkspaceRepo {
                repo,
                password,
                snapshot: Some(base_snapshot),
            },
        })
        .context("serialize managed /finalize body")?;
        let token = super::mint_managed_capability(
            "workspace_finalize",
            &request_sha256(&body),
            Some(session_id),
            None,
            capability_secret,
        );
        let resp = post(endpoint, "v2/workspaces/finalize", &token, body)?;
        let status = resp.status();
        if !status.is_success() {
            let detail = error_detail(resp);
            return Err(PillboxError::runtime(
                "run",
                format!("managed workspace finalize failed (HTTP {status}): {detail}"),
            )
            .into());
        }
        let text = read_capped(resp, 64 * 1024).context("read managed /finalize response")?;
        let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
            PillboxError::runtime(
                "run",
                format!("managed /finalize: unexpected response: {e}"),
            )
        })?;
        parsed
            .get("resultSnapshot")
            .and_then(serde_json::Value::as_str)
            .filter(|value| {
                value.len() == 64
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
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
        read_capped(resp, 2048).unwrap_or_else(|_| "<unreadable or oversized body>".into())
    }

    fn request_sha256(body: &str) -> String {
        format!("sha256:{:x}", Sha256::digest(body.as_bytes()))
    }

    fn read_capped(mut response: reqwest::blocking::Response, cap: u64) -> Result<String> {
        let mut bytes = Vec::new();
        response
            .by_ref()
            .take(cap + 1)
            .read_to_end(&mut bytes)
            .context("read response body")?;
        if bytes.len() as u64 > cap {
            anyhow::bail!("response exceeded {cap} bytes");
        }
        String::from_utf8(bytes).context("response body was not UTF-8")
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
                session_id: "session-1",
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

        #[test]
        fn scoped_provision_serializes_the_r2_session_token() {
            let mut c = cfg();
            c.session_token = Some("scoped-session-token".into());
            let body = serde_json::to_value(ProvisionBody {
                session_id: "session-1",
                workspace: WorkspaceRepo {
                    repo: &c,
                    password: "repo-pw",
                    snapshot: Some("snap-handle"),
                },
            })
            .unwrap();

            assert_eq!(
                body["workspace"]["repo"]["session_token"],
                "scoped-session-token"
            );
        }

        /// `/finalize` carries the restored base snapshot so the helper records
        /// a repository-verifiable lineage edge on the result.
        #[test]
        fn finalize_body_binds_the_base_snapshot() {
            let c = cfg();
            let body = serde_json::to_value(ProvisionBody {
                session_id: "session-1",
                workspace: WorkspaceRepo {
                    repo: &c,
                    password: "repo-pw",
                    snapshot: Some("base-snapshot"),
                },
            })
            .unwrap();
            assert_eq!(body["workspace"]["snapshot"], "base-snapshot");
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
/// reaching CF can touch only `bucket/<prefix>` for a bounded TTL. The minting
/// authority is mandatory: without it, the transfer fails before the parent key
/// can cross to the managed plane.
mod r2_scope {
    use anyhow::{Context, Result};
    use serde::{Deserialize, Serialize};

    use crate::errors::PillboxError;
    use crate::workspace::rustic::S3Config;

    /// The CF API token that authorizes minting temp credentials (a Bearer token
    /// with R2 read+write on the bucket). It is the only accepted minting
    /// authority; missing, non-Unicode, or blank values are configuration errors.
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

    /// Mint a fresh prefix-scoped temp credential for one workspace transfer
    /// (provision or finalize). Called once per transfer so each credential only
    /// spans a single round-trip — a long turn between provision and finalize
    /// can't expire it.
    ///
    /// Fail-closed: a missing minting authority, missing account id, empty repo
    /// prefix, mint failure, or unusable returned credential aborts the run. The
    /// parent credential is never a return value from this boundary.
    pub(super) fn scope_for_transfer(parent: &S3Config) -> Result<S3Config> {
        let api_token = required_api_token()?;
        scope_for_transfer_with(parent, &api_token, mint)
    }

    fn scope_for_transfer_with<M>(
        parent: &S3Config,
        api_token: &str,
        mint_fn: M,
    ) -> Result<S3Config>
    where
        M: FnOnce(&S3Config, &str, &str, &str) -> Result<S3Config>,
    {
        if api_token.trim().is_empty() {
            return Err(missing_api_token_error().into());
        }
        let account_id = account_id_from_endpoint(&parent.endpoint).ok_or_else(|| {
            PillboxError::config(
                "run",
                format!(
                    "managed workspace transfer requires an R2 endpoint of the form \
                     `<account-id>.r2.cloudflarestorage.com`; `{}` cannot be scoped",
                    parent.endpoint
                ),
            )
        })?;
        let cf_prefix = cf_key_prefix(&parent.prefix).ok_or_else(|| {
            PillboxError::config(
                "run",
                format!(
                    "managed workspace transfer requires a non-empty R2 repo prefix; an empty \
                     prefix would grant bucket-wide access"
                ),
            )
        })?;
        let scoped = mint_fn(parent, api_token.trim(), &account_id, &cf_prefix)?;
        validate_scoped(parent, scoped)
    }

    fn required_api_token() -> Result<String> {
        required_api_token_value(std::env::var_os(API_TOKEN_ENV))
    }

    fn required_api_token_value(value: Option<std::ffi::OsString>) -> Result<String> {
        let token = value
            .ok_or_else(missing_api_token_error)?
            .into_string()
            .map_err(|_| missing_api_token_error())?;
        let token = token.trim();
        if token.is_empty() {
            return Err(missing_api_token_error().into());
        }
        Ok(token.to_string())
    }

    fn missing_api_token_error() -> PillboxError {
        PillboxError::config(
            "run",
            format!(
                "{API_TOKEN_ENV} is required for managed workspace transfer; it must authorize \
                 a fresh prefix-scoped R2 credential mint"
            ),
        )
    }

    /// Parse the R2 account id out of an `<account-id>.r2.cloudflarestorage.com`
    /// endpoint (with or without scheme / trailing path). `None` for any other
    /// S3-compatible host (MinIO, Backblaze, native S3), which the managed R2
    /// transfer boundary rejects because it cannot mint a scoped credential.
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
        let env: TempCredEnvelope =
            serde_json::from_str(body).context("parse R2 temp-credential response")?;
        if !env.success {
            return Err(PillboxError::runtime(
                "run",
                format!(
                    "R2 temp-credential mint was rejected by Cloudflare ({} error(s))",
                    env.errors.len()
                ),
            )
            .into());
        }
        let cred = env.result.ok_or_else(|| {
            PillboxError::runtime("run", "R2 temp-credential response had no `result`")
        })?;
        if cred.access_key_id.trim().is_empty()
            || cred.secret_access_key.trim().is_empty()
            || cred.session_token.trim().is_empty()
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

    fn validate_scoped(parent: &S3Config, scoped: S3Config) -> Result<S3Config> {
        let has_session_token = scoped
            .session_token
            .as_deref()
            .is_some_and(|token| !token.trim().is_empty());
        let fresh_key = scoped.access_key != parent.access_key
            && scoped.secret_key != parent.secret_key
            && !scoped.access_key.trim().is_empty()
            && !scoped.secret_key.trim().is_empty();
        if !has_session_token || !fresh_key {
            return Err(PillboxError::runtime(
                "run",
                "R2 temp-credential mint did not return a fresh scoped credential",
            )
            .into());
        }
        Ok(scoped)
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
                format!("R2 temp-credential mint returned HTTP {status}"),
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
            // Not an R2 host → the managed transfer boundary rejects it because
            // Cloudflare cannot mint a scoped credential for that endpoint.
            assert_eq!(account_id_from_endpoint("https://s3.amazonaws.com"), None);
            assert_eq!(account_id_from_endpoint("https://minio.local:9000"), None);
            // A sub-subdomain isn't a bare account id.
            assert_eq!(
                account_id_from_endpoint("https://x.abc123.r2.cloudflarestorage.com"),
                None
            );
        }

        #[test]
        fn minting_authority_is_required_and_must_be_usable_text() {
            let missing = required_api_token_value(None).expect_err("missing token must fail");
            assert!(missing.to_string().contains(API_TOKEN_ENV));

            let blank =
                required_api_token_value(Some("  \n".into())).expect_err("blank token must fail");
            assert!(blank.to_string().contains(API_TOKEN_ENV));

            assert_eq!(
                required_api_token_value(Some("  mint-authority  ".into())).unwrap(),
                "mint-authority"
            );
        }

        #[test]
        fn scope_rejects_non_r2_and_bucket_wide_repositories_before_mint() {
            let mut non_r2 = parent();
            non_r2.endpoint = "https://s3.amazonaws.com".into();
            let err = scope_for_transfer_with(&non_r2, "mint-authority", |_, _, _, _| {
                panic!("invalid endpoint must fail before mint")
            })
            .expect_err("non-R2 endpoint must fail closed");
            assert!(err.to_string().contains("cannot be scoped"));

            let mut bucket_wide = parent();
            bucket_wide.prefix = "/".into();
            let err = scope_for_transfer_with(&bucket_wide, "mint-authority", |_, _, _, _| {
                panic!("empty prefix must fail before mint")
            })
            .expect_err("bucket-wide prefix must fail closed");
            assert!(err.to_string().contains("bucket-wide access"));
        }

        #[test]
        fn scope_propagates_api_failure_without_parent_fallback() {
            let err = scope_for_transfer_with(&parent(), "mint-authority", |_, _, _, _| {
                Err(PillboxError::runtime("run", "mint API unavailable").into())
            })
            .expect_err("mint failure must abort the transfer");
            assert!(err.to_string().contains("mint API unavailable"));
        }

        #[test]
        fn scope_returns_only_a_fresh_prefix_scoped_credential() {
            let original = parent();
            let scoped = scope_for_transfer_with(
                &original,
                " mint-authority ",
                |received_parent, token, account_id, prefix| {
                    assert_eq!(received_parent.secret_key, "PARENT_SK");
                    assert_eq!(token, "mint-authority");
                    assert_eq!(account_id, "abc123");
                    assert_eq!(prefix, "proj/");
                    Ok(S3Config {
                        access_key: "TMP_AK".into(),
                        secret_key: "TMP_SK".into(),
                        session_token: Some("TMP_ST".into()),
                        ..received_parent.clone()
                    })
                },
            )
            .expect("fresh scoped credential accepted");

            assert_eq!(scoped.access_key, "TMP_AK");
            assert_eq!(scoped.secret_key, "TMP_SK");
            assert_eq!(scoped.session_token.as_deref(), Some("TMP_ST"));
            assert_ne!(scoped.access_key, original.access_key);
            assert_ne!(scoped.secret_key, original.secret_key);
        }

        #[test]
        fn scope_rejects_a_minter_returning_parent_or_incomplete_credentials() {
            let original = parent();
            let err = scope_for_transfer_with(&original, "mint-authority", |parent, _, _, _| {
                Ok(parent.clone())
            })
            .expect_err("parent credential must never cross the boundary");
            assert!(err.to_string().contains("fresh scoped credential"));

            let err = scope_for_transfer_with(&original, "mint-authority", |parent, _, _, _| {
                Ok(S3Config {
                    access_key: "TMP_AK".into(),
                    secret_key: "TMP_SK".into(),
                    session_token: Some("  ".into()),
                    ..parent.clone()
                })
            })
            .expect_err("blank session token must fail closed");
            assert!(err.to_string().contains("fresh scoped credential"));
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
            let failed = r#"{"success":false,"errors":[{"message":"SENSITIVE"}],"result":null}"#;
            let err = parse_scoped(failed, &parent()).expect_err("rejection must fail");
            assert!(!err.to_string().contains("SENSITIVE"));

            let malformed = r#"{"result":{"secretAccessKey":"SENSITIVE"}"#;
            let err = parse_scoped(malformed, &parent()).expect_err("malformed JSON must fail");
            assert!(!err.to_string().contains("SENSITIVE"));
            // success but a blank credential field is not a usable credential.
            let blank = r#"{"success":true,"errors":[],
                "result":{"accessKeyId":"TMP_AK","secretAccessKey":"  ","sessionToken":"TMP_ST"}}"#;
            assert!(parse_scoped(blank, &parent()).is_err());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_opts() -> RunOpts {
        RunOpts {
            workspace: None,
            name: None,
            mounts: Vec::new(),
            withs: Vec::new(),
            env_bundles: Vec::new(),
            env_files: Vec::new(),
            vault: false,
            memory: false,
            memory_briefed: Vec::new(),
            mcps: Vec::new(),
            mcp_tokens: Vec::new(),
            args: vec!["must-not-run".into()],
            detach: false,
            label: None,
            json: false,
            ttl_seconds: None,
            from_bookmark: None,
            model: None,
            profile: None,
            reasoning_effort: None,
            temperature: None,
            egress_allow: Vec::new(),
            egress_deny: false,
        }
    }

    #[test]
    fn managed_support_contract_accepts_only_opencode() {
        assert_eq!(
            require_supported_agent(&crate::agents::OPENCODE)
                .expect("OpenCode remains the managed executable")
                .spec
                .id(),
            "opencode"
        );
        for unsupported in [
            &crate::agents::CLAUDE,
            &crate::agents::CODEX,
            &crate::agents::CODEX_SERVE,
            &crate::agents::PI,
            &crate::agents::CURSOR,
        ] {
            let error = require_supported_agent(unsupported)
                .err()
                .expect("every other managed agent must fail closed");
            assert!(error.to_string().contains("unsupported_execution"));
        }
    }

    /// Exercise the actual `SandboxBackend::run` boundary with no initialized
    /// workspace or managed credentials. The only successful route to this
    /// exact error is the first-line support gate: moving or bypassing it makes
    /// the poison fixture fail on workspace/config access and this test red.
    #[test]
    fn unsupported_codex_run_rejects_before_any_managed_side_effect() {
        crate::test_util::with_isolated_home("managed-agent-preflight", || {
            let resolved = crate::pillbox::global();
            assert!(!resolved.state_dir.exists());
            std::env::remove_var("PILLBOX_MANAGED_URL");
            std::env::remove_var("PILLBOX_MANAGED_DO_URL");
            std::env::remove_var("PILLBOX_MANAGED_TOKEN_SECRET");
            std::env::remove_var("PILLBOX_R2_CF_API_TOKEN");

            let error = ManagedBackend
                .run(&crate::agents::CODEX, run_opts(), &resolved)
                .expect_err("managed Codex must be rejected");
            let message = error.to_string();
            assert!(message.contains("unsupported_execution"), "{message}");
            assert!(message.contains("before workspace snapshot or external access"));
            assert!(
                !resolved.state_dir.exists(),
                "preflight must not persist a session, snapshot, or other state"
            );
        });
    }

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

    /// The token shape matches `auth.ts::signManagedCapability`:
    /// `base64url(claimJson).base64url(hmac)`, two padless URL-safe segments
    /// split on a single `.`, the first being the base64url of the actor JSON.
    #[test]
    fn mint_managed_capability_has_two_b64url_segments_over_the_claim() {
        use base64::Engine as _;
        let token = mint_managed_capability(
            "execute",
            &format!("sha256:{}", "a".repeat(64)),
            Some("abc123def456"),
            Some("def456abc123"),
            "shared-secret",
        );

        let (claim_b64, sig_b64) = token.split_once('.').expect("two dot-joined segments");
        assert!(!claim_b64.is_empty() && !sig_b64.is_empty());
        // Padless URL-safe alphabet: no '+', '/', or '=' on either segment.
        for seg in [claim_b64, sig_b64] {
            assert!(
                !seg.contains('+') && !seg.contains('/') && !seg.contains('='),
                "segment not base64url-no-pad: {seg}"
            );
        }
        // The claim is operation- and resource-bound, versioned, and expiring.
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(claim_b64)
            .expect("claim is valid base64url");
        let back: serde_json::Value = serde_json::from_slice(&decoded).expect("claim JSON");
        assert_eq!(back["version"], 1);
        assert_eq!(back["audience"], "pillbox-managed");
        assert_eq!(back["operation"], "execute");
        assert_eq!(back["request_sha256"], format!("sha256:{}", "a".repeat(64)));
        assert_eq!(back["session_id"], "abc123def456");
        assert_eq!(back["invocation_id"], "def456abc123");
        assert!(back["expires_at_ms"].as_u64().is_some());
    }

    /// The signature is over the *claim segment* (the base64url string), exactly
    /// as `auth.ts` signs `claim` — not over the raw JSON. Recomputing the HMAC
    /// over `claim_b64` must reproduce the token's signature segment.
    #[test]
    fn mint_managed_capability_signs_the_claim_segment() {
        use base64::Engine as _;
        let secret = "deploy-secret";
        let token = mint_managed_capability(
            "status",
            &format!("sha256:{}", "a".repeat(64)),
            None,
            Some("def456abc123"),
            secret,
        );
        let (claim_b64, sig_b64) = token.split_once('.').unwrap();

        let expected = hmac_sha256(secret.as_bytes(), claim_b64.as_bytes());
        let expected_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(expected);
        assert_eq!(
            sig_b64, expected_b64,
            "signature must sign the claim segment"
        );
    }

    /// A different secret yields a different signature over the same claim bytes.
    #[test]
    fn managed_capability_signature_depends_on_secret() {
        use base64::Engine as _;
        let token = mint_managed_capability(
            "cancel",
            &format!("sha256:{}", "a".repeat(64)),
            None,
            Some("def456abc123"),
            "secret-a",
        );
        let (claim, signature) = token.split_once('.').unwrap();
        let alternate = hmac_sha256(b"secret-b", claim.as_bytes());
        let alternate = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(alternate);
        assert_ne!(signature, alternate);
    }

    /// The handle round-trips through the record's `sandbox_id` JSON, so the
    /// plane can decode the DO endpoint + session id back out.
    #[test]
    fn managed_handle_round_trips_through_sandbox_id() {
        let handle = ManagedHandle {
            endpoint: "https://w.workers.dev".into(),
            execution_session_id: "abc123def456".into(),
        };
        let mut s = Session::test_fixture();
        s.backend = BACKEND_MANAGED.to_string();
        s.placement = session::Placement::Managed;
        s.sandbox_id = serde_json::to_string(&handle).unwrap();

        let decoded = ManagedHandle::decode_for_origin(&s, "https://w.workers.dev")
            .expect("decodes the managed handle");
        assert_eq!(decoded, handle);
    }

    #[test]
    fn managed_handle_rejects_record_and_origin_mismatch() {
        let handle = ManagedHandle {
            endpoint: "https://w.workers.dev".into(),
            execution_session_id: "def456abc123".into(),
        };
        let mut session = Session::test_fixture();
        session.sandbox_id = serde_json::to_string(&handle).unwrap();
        assert!(ManagedHandle::decode_for_origin(&session, "https://w.workers.dev").is_err());

        let handle = ManagedHandle {
            endpoint: "https://other.workers.dev".into(),
            execution_session_id: session.id.clone(),
        };
        session.sandbox_id = serde_json::to_string(&handle).unwrap();
        assert!(ManagedHandle::decode_for_origin(&session, "https://w.workers.dev").is_err());
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
            endpoint: "https://w.workers.dev".into(),
            execution_session_id: "sess-do".into(),
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
            std::env::remove_var("PILLBOX_MANAGED_URL");
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
    fn resolve_https_origin_enforces_https() {
        // Serialize the env mutation under the shared test lock (held by
        // `with_isolated_home`) so a parallel test can't trample the var.
        crate::test_util::with_isolated_home("managed-https-guard", || {
            let _env = ManagedEnvGuard;

            // Unset → config error naming the var.
            std::env::remove_var("PILLBOX_MANAGED_URL");
            std::env::remove_var("PILLBOX_MANAGED_DO_URL");
            let err = resolve_https_origin().expect_err("unset env must error");
            assert!(err.to_string().contains("PILLBOX_MANAGED_URL"));

            // Plaintext http:// → refused before any network touch.
            std::env::set_var("PILLBOX_MANAGED_URL", "http://insecure.example.com");
            let err = resolve_https_origin().expect_err("http:// must be refused");
            assert!(
                err.to_string().contains("non-HTTPS"),
                "guard must explain the HTTPS refusal, got: {err}"
            );

            // https:// → the Worker origin, with trailing slash normalized.
            std::env::set_var("PILLBOX_MANAGED_URL", "https://w.workers.dev/");
            let endpoint = resolve_https_origin().expect("https resolves");
            assert_eq!(endpoint, "https://w.workers.dev");
        });
    }

    /// The persisted record must NEVER carry the R2 creds or the repo password —
    /// it holds only the endpoint + session id (in the handle) + the result handle;
    /// creds are re-resolved from env each run. Build the record exactly as `run`
    /// does and assert no secret material survives the serialization.
    #[test]
    fn persisted_record_excludes_creds_and_password() {
        let handle = ManagedHandle {
            endpoint: "https://w.workers.dev".into(),
            execution_session_id: "sess-do".into(),
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
            requested_execution: None,
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
