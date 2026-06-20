//! Glue between `pillbox <agent> run --vault` and the vault server.
//!
//! Owns the lifetime of the proxy + lease + stub credentials file for
//! one `run` invocation. Drop order is intentional:
//!  1. `lease` — removes the stub mapping from the server registry.
//!  2. `server` — sends graceful-shutdown signal to the proxy task.
//!  3. `runtime` — aborts async tasks + frees resources, but WAITS for any
//!     in-flight `spawn_blocking` task (an in-proxy refresh forward) to finish,
//!     since blocking tasks can't be cancelled (intentional — see the field note).
//!  4. `stub_file` — deletes the temp file holding the stub JSON.

use std::{
    fs,
    io::Write,
    net::SocketAddr,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Result;

use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::vault::token_store::TokenStore;
use crate::vault::{providers, RunContext, SandboxLease, Server, ServerConfig, VaultMeta};

/// How long the teardown write-back waits for the rotation lock before giving up.
/// Best-effort + short: if a refresh holds the lock, its writer is the authority,
/// so the teardown defers (`persist_if` → `Ok(false)`) rather than block teardown.
const TEARDOWN_LOCK_WAIT: Duration = Duration::from_secs(5);

/// One OAuth-credentials swap mounted into the guest. Owns the temp file
/// holding the stub creds plus its mount-target path. `VaultSession`
/// keeps a `Vec` of these — currently always 0 or 1 entries (one agent,
/// one creds file), but the shape future-proofs us if a single sandbox
/// ever needs multiple creds files.
struct OAuthMount {
    stub_file: tempfile::NamedTempFile,
    /// Guest-relative path the stub file is mounted at (e.g.
    /// `.claude/.credentials.json` or `.codex/auth.json`). The agent
    /// provider tells us where.
    creds_path: PathBuf,
    /// Host-absolute path to the real credentials file this mount
    /// shadows (the global auth store). At teardown we persist the
    /// registry's real creds back here so an in-proxy token rotation
    /// during the run survives to the next session.
    host_creds_path: PathBuf,
    /// Registry key for this mount's real creds, so teardown can read
    /// the (possibly rotated) tokens back out of the server.
    sandbox_id: String,
    /// The access token this session *leased* (as read from disk at start). The
    /// teardown compare-and-swaps on it: persist the registry creds back only if
    /// the on-disk access token is still this one, i.e. no other writer rotated disk
    /// under us. `None` for an unrecognized creds shape → the teardown skips (don't
    /// clobber what we can't reason about).
    leased_access: Option<String>,
    _lease: SandboxLease,
}

pub(crate) struct VaultSession {
    // Drop order matters — see module doc. `api_key_leases` and
    // `oauth_mounts` both hold `SandboxLease`s that remove their entries
    // from the server registry on drop; `_server` then signals proxy
    // shutdown; `_runtime` is dropped last. Note: dropping the runtime ABORTS
    // async tasks but WAITS for any in-flight `spawn_blocking` task (e.g. an
    // in-proxy refresh forward) to finish — those can't be cancelled. Teardown
    // therefore blocks up to the forward timeout if a refresh is mid-flight; that
    // is intentional (abandoning a mid-commit refresh could leave torn state).
    api_key_leases: Vec<SandboxLease>,
    oauth_mounts: Vec<OAuthMount>,
    server: Server,
    _runtime: tokio::runtime::Runtime,
    ca_cert_path: PathBuf,
    listen_addr: SocketAddr,
    /// Holds the per-run ephemeral CA's tempdir alive until teardown (the cert
    /// file is bind-mounted into the guest for the run's duration). `None` when a
    /// stable persistent CA is in use. Last field → dropped last, after the
    /// server, so the cert outlives anything reading it.
    _ca_tempdir: Option<tempfile::TempDir>,
}

impl Drop for VaultSession {
    /// Persist any in-proxy-rotated real credentials back to the host
    /// auth store. Runs before the fields drop (a manual `Drop` fires
    /// ahead of field destruction), so the leases haven't yet removed
    /// their registry entries and `snapshot_real` still sees them.
    ///
    /// Why this exists: Anthropic rotates refresh tokens single-use. If
    /// the agent refreshes mid-session through the proxy, the new token
    /// lands only in the in-memory registry — the on-disk creds file
    /// keeps the old one, which the rotation just invalidated. The next
    /// run's pre-refresh then sends a dead token and 401s, recurring
    /// every session. Flushing the registry's real creds here closes
    /// that loop (mirrors how Orca reads back + persists CLI-rotated
    /// tokens). Best-effort: a failure only costs a stale token
    /// (recoverable via `pillbox auth login`), so warn, never panic.
    ///
    /// Guarded against the concurrent-session clobber via a compare-and-swap on
    /// the **access token**: persist only if this session actually rotated, and
    /// only if the on-disk access token is still the one we leased at start. If
    /// disk advanced, another writer (a peer broker, or the in-proxy refresh
    /// handler) wrote a fresher token — never clobber it. The access token is the
    /// signal, not `expiresAt`/`last_refresh`, because the in-proxy handlers rotate
    /// only the token fields, not the timestamps — so a timestamp compare can't see
    /// an in-proxy rotation. The read-compare-write runs under the rotation flock
    /// (atomic), closing the prior lock-free TOCTOU.
    fn drop(&mut self) {
        for mount in &self.oauth_mounts {
            let Some(real) = self.server.snapshot_real(&mount.sandbox_id) else {
                continue;
            };
            // Nothing rotated this session (registry == what we leased) → nothing to
            // persist, so skip the lock + write entirely. Also skips unrecognized
            // shapes (both `None`) — for which we have no CAS baseline anyway.
            if access_token_of(&real) == mount.leased_access {
                continue;
            }
            // We rotated to a recognized token; CAS the write-back on the access we
            // leased. (`Some` here for claude/codex — the only vaulted shapes; if we
            // somehow lack a baseline, skip rather than risk a blind overwrite.)
            let Some(leased) = mount.leased_access.clone() else {
                continue;
            };
            let store = TokenStore::new(mount.host_creds_path.clone(), TEARDOWN_LOCK_WAIT);
            let disk_unchanged = |disk: &serde_json::Value| {
                access_token_of(disk).as_deref() == Some(leased.as_str())
            };
            if let Err(e) = store.persist_if(&real, &disk_unchanged) {
                eprintln!(
                    "pillbox: warning: failed to persist refreshed credentials to {}: {e}",
                    mount.host_creds_path.display()
                );
            }
        }
    }
}

/// The OAuth access token from a creds blob, across the shapes pillbox vaults —
/// claude (`claudeAiOauth.accessToken`) and codex (`tokens.access_token`). The
/// access token is the field that rotates on *every* refresh, so it's the
/// compare-and-swap signal the teardown uses. (`expiresAt`/`last_refresh` are NOT
/// updated by the in-proxy rotation handlers, so they can't detect one.)
fn access_token_of(creds: &serde_json::Value) -> Option<String> {
    creds
        .pointer("/claudeAiOauth/accessToken")
        .or_else(|| creds.pointer("/tokens/access_token"))
        .and_then(|v| v.as_str())
        .map(str::to_owned)
}

impl VaultSession {
    /// Spin up the vault proxy server.
    ///
    /// If `oauth` is `Some`, an OAuth lease for that agent is taken and
    /// a stub credentials file is written to a temp path the caller can
    /// mount via [`Self::docker_extras`]. Pass `None` when the agent
    /// itself isn't `vault_capable` but the run still has `--with
    /// FOO --vault`-flagged secrets that need stub swapping — pillbox
    /// still needs a proxy + CA + leases for those.
    ///
    /// `context` carries the orchestration-level signals that
    /// downstream telemetry consumers care about — `session_id` for
    /// trace correlation, `mode` / `workspace_id` as attributes on
    /// the gen_ai spans this server emits. Pass
    /// [`RunContext::default()`] when no signals are available
    /// (tests, the ad-hoc `sidecar` command).
    ///
    /// `egress` is the broker policy (default-deny + allowlist). Pass
    /// [`EgressPolicy::default()`] (permissive) to keep legacy pass-through.
    pub(crate) fn start(
        oauth: Option<OAuthAgent<'_>>,
        pillbox: &Pillbox,
        context: RunContext,
        egress: crate::vault::EgressPolicy,
    ) -> Result<Self> {
        // Per-run ephemeral CA by default: a leaked CA is then valid only for
        // this one run, not every future one. If the user opted into a *stable*
        // CA (`pillbox vault ca`, e.g. to pre-trust it in a browser for
        // debugging) — or a legacy one is already on disk — reuse it. The guest
        // installs the cert per-boot either way (`update-ca-certificates` /
        // `NODE_EXTRA_CA_CERTS`), so ephemeral costs nothing on the reuse side.
        // `subdir_path` (not `subdir`): just probe for a pinned CA — don't create
        // an empty `<pillbox>/vault/` on the ephemeral path. `Ca::ensure` creates
        // the dir when a stable CA is actually written.
        let persistent_dir = pillbox.subdir_path("vault");
        let (ca_dir, ca_tempdir) = if crate::vault::ca_cert_path_in(&persistent_dir).exists() {
            (persistent_dir, None)
        } else {
            let td = tempfile::tempdir()
                .map_err(|e| PillboxError::runtime("vault", format!("ca tempdir: {e}")))?;
            (td.path().to_path_buf(), Some(td))
        };

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| PillboxError::runtime("vault", format!("tokio runtime: {e}")))?;

        let server = runtime
            .block_on(Server::start(ServerConfig {
                bind: Some(SocketAddr::from(([0, 0, 0, 0], 0))),
                ca_dir,
                context,
                egress,
            }))
            .map_err(|e| PillboxError::runtime("vault", format!("start proxy: {e}")))?;

        let listen_addr = server.listen_addr();
        let ca_cert_path = server.ca_cert_path().to_path_buf();

        let mut oauth_mounts = Vec::new();
        if let Some(agent) = oauth {
            oauth_mounts.push(provision_oauth_mount(&server, agent)?);
        }

        Ok(Self {
            api_key_leases: Vec::new(),
            oauth_mounts,
            server,
            _runtime: runtime,
            ca_cert_path,
            listen_addr,
            _ca_tempdir: ca_tempdir,
        })
    }

    /// Lease a stub for one `--with NAME --vault`'d API key. Returns the
    /// stub string the caller should inject into the guest env in place
    /// of the real secret value.
    pub(crate) fn lease_api_key(
        &mut self,
        secret_name: &str,
        real_value: &str,
        meta: &VaultMeta,
    ) -> Result<String> {
        let (lease, stub) = self
            .server
            .lease_api_key(secret_name, real_value, meta)
            .map_err(|e| PillboxError::runtime("vault", format!("lease api key: {e}")))?;
        self.api_key_leases.push(lease);
        Ok(stub)
    }

    /// Extra docker args to layer onto a normal `<agent> run`:
    /// `-v cacert:/etc/pillbox-ca.crt:ro` (the path
    /// `NODE_EXTRA_CA_CERTS` points at, for Node-based agents),
    /// `-v cacert:/usr/local/share/ca-certificates/pillbox-vault.crt:ro`
    /// (the path the runner-image entrypoint feeds to
    /// `update-ca-certificates`, putting the cert into the system
    /// trust store for Rust/Go agents like Codex), env wiring
    /// (`NODE_EXTRA_CA_CERTS`, `HTTPS_PROXY`, `HTTP_PROXY`), plus
    /// one `-v stubfile:<creds>:ro` per OAuth mount.
    pub(crate) fn docker_extras(&self, guest_home: &str) -> Vec<String> {
        let port = self.listen_addr.port();
        // The `--add-host host.docker.internal:host-gateway` line that
        // makes this alias resolve on Linux lives in `base_docker_args`
        // (Docker Desktop ignores it harmlessly), so vault + MCP + any
        // future host-reachable feature all get it without each having
        // to remember.
        let proxy_url = format!("http://host.docker.internal:{port}");
        let guest_ca = "/etc/pillbox-ca.crt";
        // Bind the same source file at the path the runner image's
        // entrypoint scans on boot. Codex's reqwest / native-tls
        // doesn't honor NODE_EXTRA_CA_CERTS — it only reads the
        // system CA bundle — so without this mount it presents
        // `invalid peer certificate: UnknownIssuer` whenever the
        // vault MITMs chatgpt.com.
        let system_trust_ca = "/usr/local/share/ca-certificates/pillbox-vault.crt";

        let mut out = vec![
            "-v".into(),
            format!("{}:{guest_ca}:ro", self.ca_cert_path.display()),
            "-v".into(),
            format!("{}:{system_trust_ca}:ro", self.ca_cert_path.display()),
        ];
        for mount in &self.oauth_mounts {
            let guest_creds = format!("{guest_home}/{}", mount.creds_path.display());
            out.push("-v".into());
            out.push(format!(
                "{}:{guest_creds}:ro",
                mount.stub_file.path().display()
            ));
        }
        out.extend([
            "-e".into(),
            format!("NODE_EXTRA_CA_CERTS={guest_ca}"),
            "-e".into(),
            format!("HTTPS_PROXY={proxy_url}"),
            "-e".into(),
            format!("HTTP_PROXY={proxy_url}"),
        ]);
        out
    }

    pub(crate) fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    pub(crate) fn ca_cert_path(&self) -> &Path {
        &self.ca_cert_path
    }
}

/// Input to `VaultSession::start` when the agent itself needs an OAuth
/// stub. `agent_id` selects the provider (matches `AgentSpec::id`).
pub(crate) struct OAuthAgent<'a> {
    pub(crate) agent_id: &'a str,
    pub(crate) agent_home: &'a Path,
}

fn provision_oauth_mount(server: &Server, agent: OAuthAgent<'_>) -> Result<OAuthMount> {
    let provider = providers::provider_for(agent.agent_id).ok_or_else(|| {
        PillboxError::runtime(
            "vault",
            format!("no vault provider for agent `{}`", agent.agent_id),
        )
    })?;

    let creds_rel = provider.creds_path().to_path_buf();
    let creds_path = agent.agent_home.join(&creds_rel);

    let real_bytes = fs::read(&creds_path).map_err(|e| {
        PillboxError::runtime("vault", format!("read {}: {e}", creds_path.display())).with_next(
            format!("pillbox {} login   # refresh credentials", agent.agent_id),
        )
    })?;
    let real: serde_json::Value = serde_json::from_slice(&real_bytes).map_err(|e| {
        PillboxError::runtime("vault", format!("parse {}: {e}", creds_path.display())).with_next(
            format!(
                "pillbox {} login   # credentials file is malformed",
                agent.agent_id
            ),
        )
    })?;

    // The on-disk access token — the teardown's CAS baseline (what the teardown
    // compares against to decide whether this session rotated). An expired token is
    // NOT pre-refreshed host-side: the agent refreshes on demand through the
    // coordinated in-proxy `/oauth/token` handler, which serializes the rotation
    // across concurrent sessions via the `TokenStore` lock. A host-side pre-refresh
    // would POST the refresh token OUTSIDE that lock — two concurrent runs would
    // forward the same token and trip reuse detection.
    let leased_access = access_token_of(&real);

    let sandbox_id = uuid::Uuid::now_v7().to_string();
    let lease = server
        .lease(provider.id(), &sandbox_id, real)
        .map_err(|e| PillboxError::runtime("vault", format!("lease sandbox: {e}")))?;
    // Point the in-proxy refresh coordinator at the shared host creds file so a
    // mid-session rotation is serialized + persisted through the rotation lock.
    server.set_oauth_creds_path(&sandbox_id, creds_path.clone());

    // Write stub creds to a 0600 temp file the docker mount will overlay
    // onto the guest's real credentials file.
    let stub_file = tempfile::Builder::new()
        .prefix("pillbox-stub-")
        .suffix(".json")
        .tempfile()
        .map_err(|e| PillboxError::runtime("vault", format!("create stub file: {e}")))?;
    write_private(stub_file.path(), lease.stub_credentials_body())?;

    Ok(OAuthMount {
        stub_file,
        creds_path: creds_rel,
        host_creds_path: creds_path,
        sandbox_id,
        leased_access,
        _lease: lease,
    })
}

/// Write `content` to `path` as a 0600 file, creating-or-truncating. Used by
/// `provision_oauth_mount` for the guest stub file.
fn write_private(path: &Path, content: &str) -> Result<()> {
    let mut f = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| PillboxError::runtime("vault", format!("open {}: {e}", path.display())))?;
    f.write_all(content.as_bytes())
        .map_err(|e| PillboxError::runtime("vault", format!("write {}: {e}", path.display())))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_token_of_extracts_claude_and_codex_shapes() {
        let claude = serde_json::json!({
            "claudeAiOauth": { "accessToken": "sk-ant-oat01-AT", "refreshToken": "RT" }
        });
        assert_eq!(access_token_of(&claude).as_deref(), Some("sk-ant-oat01-AT"));

        let codex = serde_json::json!({
            "tokens": { "access_token": "jwt.aaa.bbb", "refresh_token": "rt" },
            "last_refresh": "2026-05-18T00:00:00Z"
        });
        assert_eq!(access_token_of(&codex).as_deref(), Some("jwt.aaa.bbb"));
    }

    #[test]
    fn access_token_of_none_for_unknown_shape() {
        // No recognized access-token field → None → the teardown CAS skips
        // (don't clobber a shape we can't reason about).
        assert_eq!(access_token_of(&serde_json::json!({ "apiKey": "x" })), None);
        assert_eq!(access_token_of(&serde_json::json!({})), None);
        // A present-but-non-string field is also None.
        assert_eq!(
            access_token_of(&serde_json::json!({ "tokens": { "access_token": 5 } })),
            None
        );
    }

    /// The exact compare-and-swap predicate `Drop` installs, over both real creds
    /// shapes: persist iff the on-disk access token still equals the leased one.
    fn cas(leased: &str, disk: &serde_json::Value) -> bool {
        access_token_of(disk).as_deref() == Some(leased)
    }

    #[test]
    fn teardown_cas_persists_only_when_disk_unchanged() {
        // claude: disk still holds the leased access → allow the write-back.
        let claude_same =
            serde_json::json!({ "claudeAiOauth": { "accessToken": "AT0", "refreshToken": "RT" } });
        assert!(cas("AT0", &claude_same));
        // A peer rotated disk → deny (don't clobber the fresher token).
        let claude_peer = serde_json::json!({ "claudeAiOauth": { "accessToken": "AT_PEER" } });
        assert!(!cas("AT0", &claude_peer));

        // codex shape works the same on its own pointer.
        let codex_same = serde_json::json!({ "tokens": { "access_token": "jwt0" } });
        assert!(cas("jwt0", &codex_same));
        let codex_peer = serde_json::json!({ "tokens": { "access_token": "jwt1" } });
        assert!(!cas("jwt0", &codex_peer));

        // An unrecognized on-disk shape → access_token_of None → never matches a
        // recognized leased token → deny (don't clobber what we can't reason about).
        assert!(!cas("AT0", &serde_json::json!({ "apiKey": "x" })));
    }
}
