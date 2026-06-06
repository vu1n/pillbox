//! Glue between `pillbox <agent> run --vault` and the vault server.
//!
//! Owns the lifetime of the proxy + lease + stub credentials file for
//! one `run` invocation. Drop order is intentional:
//!  1. `lease` — removes the stub mapping from the server registry.
//!  2. `server` — sends graceful-shutdown signal to the proxy task.
//!  3. `runtime` — aborts any remaining tasks, frees resources.
//!  4. `stub_file` — deletes the temp file holding the stub JSON.

use std::{
    fs,
    io::Write,
    net::SocketAddr,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use anyhow::Result;

use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::vault::{providers, RunContext, SandboxLease, Server, ServerConfig, VaultMeta};

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
    _lease: SandboxLease,
}

pub(crate) struct VaultSession {
    // Drop order matters — see module doc. `api_key_leases` and
    // `oauth_mounts` both hold `SandboxLease`s that remove their entries
    // from the server registry on drop; `_server` then signals proxy
    // shutdown; `_runtime` aborts any remaining tasks last.
    api_key_leases: Vec<SandboxLease>,
    oauth_mounts: Vec<OAuthMount>,
    server: Server,
    _runtime: tokio::runtime::Runtime,
    ca_cert_path: PathBuf,
    listen_addr: SocketAddr,
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
    /// Guarded against the concurrent-session clobber: two vault sessions
    /// for the same agent share one global creds file, so a session that
    /// started earlier (older token) must not overwrite a fresher token a
    /// later/overlapping session already persisted. We only write back
    /// when the registry's token is at least as fresh as what's on disk.
    fn drop(&mut self) {
        for mount in &self.oauth_mounts {
            let Some(real) = self.server.snapshot_real(&mount.sandbox_id) else {
                continue;
            };
            if !is_at_least_as_fresh(&real, &mount.host_creds_path) {
                continue;
            }
            match serde_json::to_string_pretty(&real) {
                Ok(body) => {
                    if let Err(e) = write_private(&mount.host_creds_path, &body) {
                        eprintln!(
                            "pillbox: warning: failed to persist refreshed credentials to {}: {e}",
                            mount.host_creds_path.display()
                        );
                    }
                }
                Err(e) => {
                    eprintln!("pillbox: warning: failed to serialize refreshed credentials: {e}")
                }
            }
        }
    }
}

/// Whether the in-registry `real` creds are at least as fresh as what's
/// currently on disk at `creds_path`, by comparing
/// `claudeAiOauth.expiresAt`. Returns `true` (persist) when either side
/// lacks the field (non-Claude shapes, malformed/absent file) — there's
/// no freshness signal to defer to, so preserve the unconditional
/// write-back. Returns `false` only when both timestamps are present and
/// disk is strictly newer, i.e. an overlapping session already wrote a
/// fresher token that this older session would otherwise clobber.
fn is_at_least_as_fresh(real: &serde_json::Value, creds_path: &Path) -> bool {
    let real_exp = expires_at_ms(real);
    let disk_exp = fs::read(creds_path)
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| expires_at_ms(&v));
    match (real_exp, disk_exp) {
        (Some(real_exp), Some(disk_exp)) => real_exp >= disk_exp,
        _ => true,
    }
}

/// Read `claudeAiOauth.expiresAt`, normalized to milliseconds. Creds
/// files appear both seconds- and ms-encoded in the wild (Claude Code
/// writes ms via `Date.now()`; hand-rolled files sometimes use seconds),
/// so the two sides of a freshness comparison must be on the same scale
/// or the `>=` inverts. Mirrors `refresh::is_expired`'s normalization.
fn expires_at_ms(creds: &serde_json::Value) -> Option<u64> {
    /// 1e11 ms ≈ year 5138 — anything smaller is certainly seconds.
    const SECONDS_BOUNDARY_MS: u64 = 100_000_000_000;
    let raw = creds
        .pointer("/claudeAiOauth/expiresAt")
        .and_then(serde_json::Value::as_u64)?;
    Some(if raw < SECONDS_BOUNDARY_MS {
        raw.saturating_mul(1000)
    } else {
        raw
    })
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
    pub(crate) fn start(
        oauth: Option<OAuthAgent<'_>>,
        pillbox: &Pillbox,
        context: RunContext,
    ) -> Result<Self> {
        let ca_dir = pillbox.subdir("vault")?;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| PillboxError::runtime("vault", format!("tokio runtime: {e}")))?;

        let server = runtime
            .block_on(Server::start(ServerConfig {
                bind: Some(SocketAddr::from(([0, 0, 0, 0], 0))),
                ca_dir: ca_dir.clone(),
                context,
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

    /// smolvm equivalent of [`Self::docker_extras`] (SPIKE — see
    /// `sandbox/smolvm.rs`): the same **explicit-proxy broker** wiring — CA
    /// mounts + `NODE_EXTRA_CA_CERTS`/`HTTPS_PROXY`/`HTTP_PROXY` pointing the
    /// guest agent at the host-side MITM proxy + the OAuth stub mounts. The real
    /// credential stays in the host proxy; the guest only ever sees a stub + the
    /// proxy URL (cred-never-in-guest, no transparent network interception, so
    /// no smolvm change needed for proxy-honoring agents).
    ///
    /// `proxy_host` is how the guest addresses the host — the one smolvm-specific
    /// unknown vs docker's `host.docker.internal` alias (live-verify point).
    /// `:ro` mount enforcement is omitted (smolvm virtiofs mount options aren't
    /// wired in the spike). Duplicates `docker_extras`; a shared
    /// proxy-env/CA-mount builder is the ship-review collapse.
    pub(crate) fn smolvm_extras(&self, guest_home: &str, proxy_host: &str) -> Vec<String> {
        let port = self.listen_addr.port();
        let proxy_url = format!("http://{proxy_host}:{port}");
        let guest_ca = "/etc/pillbox-ca.crt";
        let system_trust_ca = "/usr/local/share/ca-certificates/pillbox-vault.crt";

        let mut out = vec![
            "-v".into(),
            format!("{}:{guest_ca}", self.ca_cert_path.display()),
            "-v".into(),
            format!("{}:{system_trust_ca}", self.ca_cert_path.display()),
        ];
        for mount in &self.oauth_mounts {
            let guest_creds = format!("{guest_home}/{}", mount.creds_path.display());
            out.push("-v".into());
            out.push(format!(
                "{}:{guest_creds}",
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
    let mut real: serde_json::Value = serde_json::from_slice(&real_bytes).map_err(|e| {
        PillboxError::runtime("vault", format!("parse {}: {e}", creds_path.display())).with_next(
            format!(
                "pillbox {} login   # credentials file is malformed",
                agent.agent_id
            ),
        )
    })?;

    // Proactively refresh if the stored access token is past expiry —
    // see `super::refresh` for the rationale + wire details. Non-fatal
    // on failure: caller's warning + agent's own retry-on-401 handle
    // it transparently.
    if let Err(e) = super::refresh::refresh_real_if_expired(&mut real, agent.agent_id, &creds_path)
    {
        eprintln!(
            "pillbox: warning: vault token pre-refresh failed for `{}`: {e}; \
             agent will fall back to its own retry-on-401",
            agent.agent_id,
        );
    }

    let sandbox_id = uuid::Uuid::now_v7().to_string();
    let lease = server
        .lease(provider.id(), &sandbox_id, real)
        .map_err(|e| PillboxError::runtime("vault", format!("lease sandbox: {e}")))?;

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
        _lease: lease,
    })
}

/// Write `content` to `path` as a 0600 file, creating-or-truncating.
/// Shared with [`super::refresh`] (which rewrites the stored real
/// credentials after a token refresh); kept here because
/// `provision_oauth_mount` is the original caller and the
/// abstraction doesn't earn its own module.
pub(super) fn write_private(path: &Path, content: &str) -> Result<()> {
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

    fn write_creds(dir: &std::path::Path, expires_at: Option<u64>) -> PathBuf {
        let path = dir.join(".credentials.json");
        let body = match expires_at {
            Some(e) => format!(r#"{{"claudeAiOauth":{{"expiresAt":{e}}}}}"#),
            None => r#"{"claudeAiOauth":{}}"#.to_string(),
        };
        std::fs::write(&path, body).unwrap();
        path
    }

    fn real_with(expires_at: u64) -> serde_json::Value {
        serde_json::json!({ "claudeAiOauth": { "expiresAt": expires_at } })
    }

    #[test]
    fn fresh_guard_blocks_older_token_from_clobbering_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_creds(dir.path(), Some(2_000));
        // Registry token is OLDER than disk (an overlapping session
        // already wrote a fresher one) → must not persist.
        assert!(!is_at_least_as_fresh(&real_with(1_000), &path));
        // Equal or newer → persist.
        assert!(is_at_least_as_fresh(&real_with(2_000), &path));
        assert!(is_at_least_as_fresh(&real_with(3_000), &path));
    }

    #[test]
    fn fresh_guard_normalizes_seconds_vs_ms_before_comparing() {
        let dir = tempfile::tempdir().unwrap();
        // Disk ms-encoded, registry seconds-encoded, SAME instant. Without
        // normalization real(1.7e9) < disk(1.7e12) → wrongly blocks a
        // legitimate write-back; with it both normalize to 1.7e12 → equal
        // → persist.
        let path = write_creds(dir.path(), Some(1_716_000_000_000)); // ms
        let real_seconds = serde_json::json!({
            "claudeAiOauth": { "expiresAt": 1_716_000_000_u64 } // seconds, same instant
        });
        assert!(is_at_least_as_fresh(&real_seconds, &path));
    }

    #[test]
    fn fresh_guard_persists_when_no_timestamp_to_compare() {
        let dir = tempfile::tempdir().unwrap();
        // Disk has no expiresAt → no signal → persist (preserve the
        // unconditional write-back for non-Claude shapes).
        let path = write_creds(dir.path(), None);
        assert!(is_at_least_as_fresh(&real_with(1_000), &path));
        // Missing file entirely → persist.
        let missing = dir.path().join("nope.json");
        assert!(is_at_least_as_fresh(&real_with(1_000), &missing));
        // Registry real lacks the field → persist.
        assert!(is_at_least_as_fresh(&serde_json::json!({}), &path));
    }
}
