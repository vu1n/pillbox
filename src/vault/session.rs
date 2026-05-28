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
    _lease: SandboxLease,
}

/// What [`VaultSession::direct_extras`] returns: env vars to set on the
/// spawned agent + OAuth stub files to lay down before spawn. Symmetric
/// to the data behind `docker_extras`, just shaped for in-process exec.
pub(crate) struct DirectVaultExtras {
    pub(crate) env: Vec<(String, String)>,
    pub(crate) oauth_stub_writes: Vec<DirectOAuthStub>,
}

/// One stub-file overwrite the direct-exec caller must perform before
/// spawning the agent: copy `stub_source`'s contents over
/// `$HOME/<creds_rel>`, replacing the real OAuth credentials we
/// materialized into the sandbox HOME earlier.
pub(crate) struct DirectOAuthStub {
    pub(crate) creds_rel: PathBuf,
    pub(crate) stub_source: PathBuf,
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

    pub(crate) fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    pub(crate) fn ca_cert_path(&self) -> &Path {
        &self.ca_cert_path
    }

    /// Direct-exec sibling of [`Self::docker_extras`] for environments
    /// where the agent runs in the **same process tree** as the vault
    /// session (e.g. inside an e2b sandbox via `dispatch_vault_stdin_direct`).
    /// Returns the env vars to layer onto the spawned agent and the
    /// stub-file → cred-path copies the caller must perform before
    /// spawning (overwriting the materialized real OAuth with the stub).
    ///
    ///   - `NODE_EXTRA_CA_CERTS` points at the session's CA on disk
    ///     directly (no docker bind mount).
    ///   - `HTTPS_PROXY` / `HTTP_PROXY` point at `127.0.0.1:<port>` — same
    ///     host, so no `host.docker.internal` indirection.
    ///   - Each `oauth_stub_writes` entry tells the caller to overwrite
    ///     `$HOME/<creds_rel>` with `stub_source`'s bytes; the stub keeps
    ///     the file shape the agent expects while the proxy holds the
    ///     real value in memory.
    pub(crate) fn direct_extras(&self) -> DirectVaultExtras {
        let proxy = format!("http://127.0.0.1:{}", self.listen_addr.port());
        let env = vec![
            (
                "NODE_EXTRA_CA_CERTS".into(),
                self.ca_cert_path.display().to_string(),
            ),
            ("HTTPS_PROXY".into(), proxy.clone()),
            ("HTTP_PROXY".into(), proxy),
            // Node 22+'s default fetch (undici) IGNORES `HTTPS_PROXY` unless
            // the runtime is told to honor env. Without this an agent's
            // outbound `fetch` bypasses the proxy → real server sees the
            // stub token → 401. (Recognized by Node 24+; older Nodes
            // ignore it harmlessly. Agents using non-fetch clients that
            // already read `HTTPS_PROXY` get the same behavior twice.)
            ("NODE_USE_ENV_PROXY".into(), "1".into()),
        ];
        let oauth_stub_writes = self
            .oauth_mounts
            .iter()
            .map(|m| DirectOAuthStub {
                creds_rel: m.creds_path.clone(),
                stub_source: m.stub_file.path().to_path_buf(),
            })
            .collect();
        DirectVaultExtras {
            env,
            oauth_stub_writes,
        }
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

    // Proactively refresh if the stored access token is past expiry.
    // Matches Claude Code's local-machine behavior: when you run claude
    // on your laptop, you never see a 401 because Claude Code reads
    // ~/.claude/.credentials.json, sees the token's expired, refreshes,
    // and persists the new tokens — all transparent. In the sandbox the
    // stub-file mount is read-only and the agent can't persist anything
    // across sessions, so pillbox does the refresh + persist itself.
    // Failure is non-fatal: we fall back to the stored tokens and let
    // the vault swap + agent retry handle it (one wasted 401 per
    // session, same as before this lands).
    if let Err(e) = refresh_real_if_expired(&mut real, agent.agent_id, &creds_path) {
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
        _lease: lease,
    })
}

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

/// Anthropic OAuth `client_id` for the Claude Code CLI flow. The same
/// value Claude Code itself sends when *it* refreshes. Hardcoded
/// because there's no public API to discover it; pillbox just needs
/// the canonical one Anthropic expects.
const CLAUDE_OAUTH_CLIENT_ID: &str = "claude_code";

/// Anthropic OAuth `/oauth/token` endpoint Claude Code's current
/// release talks to. The vault provider intercepts both this and the
/// legacy `console.anthropic.com` host; the pre-refresh path goes
/// straight to the canonical host so it can run *before* the proxy is
/// even up.
const CLAUDE_OAUTH_ENDPOINT: &str = "https://platform.claude.com/oauth/token";

/// Refresh the stored `claudeAiOauth` tokens in `real` if `expiresAt`
/// has passed (or is within a 5-minute safety buffer), and persist the
/// new pair to `creds_path` so subsequent sessions also start fresh.
/// No-op for non-Claude agents — Codex / OpenAI / GitHub each have
/// their own refresh shapes and we haven't generalized this yet.
///
/// All failures bubble up; the caller logs and falls back to the
/// stored tokens (the vault's stub-swap still works as long as the
/// access token is somehow valid OR the agent's own retry-on-401
/// triggers a fresh refresh through the proxy).
fn refresh_real_if_expired(
    real: &mut serde_json::Value,
    agent_id: &str,
    creds_path: &Path,
) -> Result<()> {
    if agent_id != "claude" {
        return Ok(());
    }
    let Some(expires_at) = real
        .pointer("/claudeAiOauth/expiresAt")
        .and_then(|v| v.as_u64())
    else {
        return Ok(());
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    // Claude Code stores expiresAt in milliseconds (Node Date.now()
    // convention). Older / hand-rolled credential files might be in
    // seconds; normalize by scaling anything that fits in the seconds
    // range up to milliseconds. The boundary (1e11 ms ≈ year 5138)
    // is far enough out that no real seconds-encoded timestamp will
    // ever pass it.
    let expires_at_ms = if expires_at < 100_000_000_000 {
        expires_at.saturating_mul(1000)
    } else {
        expires_at
    };
    // 5-minute pre-expiry buffer so we don't race a near-expiry token
    // against round-trip latency.
    if expires_at_ms.saturating_sub(5 * 60 * 1000) > now_ms {
        return Ok(());
    }

    let refresh_token = real
        .pointer("/claudeAiOauth/refreshToken")
        .and_then(|v| v.as_str())
        .ok_or_else(|| PillboxError::runtime("vault", "no refreshToken in claudeAiOauth block"))?
        .to_string();

    let body = serde_json::to_vec(&serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": CLAUDE_OAUTH_CLIENT_ID,
    }))
    .map_err(|e| PillboxError::runtime("vault", format!("serialize refresh body: {e}")))?;
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| PillboxError::runtime("vault", format!("build refresh client: {e}")))?;
    let resp = client
        .post(CLAUDE_OAUTH_ENDPOINT)
        .header("content-type", "application/json")
        .header("accept-encoding", "identity") // see anthropic.rs gzip note
        .body(body)
        .send()
        .map_err(|e| PillboxError::runtime("vault", format!("refresh request: {e}")))?;

    let status = resp.status();
    let resp_bytes = resp
        .bytes()
        .map_err(|e| PillboxError::runtime("vault", format!("refresh response read: {e}")))?;
    if !status.is_success() {
        return Err(PillboxError::runtime(
            "vault",
            format!(
                "refresh returned HTTP {status}: {}",
                String::from_utf8_lossy(&resp_bytes)
            ),
        )
        .with_next("pillbox auth login --agent claude   # re-authenticate".to_string())
        .into());
    }
    let resp_value: serde_json::Value = serde_json::from_slice(&resp_bytes)
        .map_err(|e| PillboxError::runtime("vault", format!("refresh response not JSON: {e}")))?;

    let new_access = resp_value
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| PillboxError::runtime("vault", "refresh response missing access_token"))?
        .to_string();
    // Anthropic may or may not rotate the refresh token; preserve the
    // old one if the response doesn't include a new one.
    let new_refresh = resp_value
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .unwrap_or(&refresh_token)
        .to_string();
    let expires_in = resp_value
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .unwrap_or(3600);
    let new_expires_at_ms = now_ms.saturating_add(expires_in.saturating_mul(1000));

    let oauth = real
        .get_mut("claudeAiOauth")
        .and_then(|v| v.as_object_mut())
        .ok_or_else(|| PillboxError::runtime("vault", "claudeAiOauth block disappeared"))?;
    oauth.insert(
        "accessToken".to_string(),
        serde_json::Value::String(new_access),
    );
    oauth.insert(
        "refreshToken".to_string(),
        serde_json::Value::String(new_refresh),
    );
    oauth.insert(
        "expiresAt".to_string(),
        serde_json::Value::Number(serde_json::Number::from(new_expires_at_ms)),
    );

    let new_bytes = serde_json::to_string_pretty(real)
        .map_err(|e| PillboxError::runtime("vault", format!("serialize refreshed creds: {e}")))?;
    write_private(creds_path, &new_bytes)?;
    Ok(())
}
