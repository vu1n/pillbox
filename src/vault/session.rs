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
use crate::vault::{providers, SandboxLease, Server, ServerConfig, VaultMeta};

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
    pub(crate) fn start(oauth: Option<OAuthAgent<'_>>, pillbox: &Pillbox) -> Result<Self> {
        let ca_dir = pillbox.subdir("vault")?;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| PillboxError::runtime("vault", format!("tokio runtime: {e}")))?;

        let server = runtime
            .block_on(Server::start(ServerConfig {
                bind: Some(SocketAddr::from(([0, 0, 0, 0], 0))),
                ca_dir: ca_dir.clone(),
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
    /// `-v cacert:/etc/pillbox-ca.crt:ro`,
    /// `-e NODE_EXTRA_CA_CERTS=...`, `-e HTTPS_PROXY=...`,
    /// `-e HTTP_PROXY=...`, plus one `-v stubfile:<creds>:ro` per OAuth
    /// mount.
    pub(crate) fn docker_extras(&self, guest_home: &str) -> Vec<String> {
        let port = self.listen_addr.port();
        // The `--add-host host.docker.internal:host-gateway` line that
        // makes this alias resolve on Linux lives in `base_docker_args`
        // (Docker Desktop ignores it harmlessly), so vault + MCP + any
        // future host-reachable feature all get it without each having
        // to remember.
        let proxy_url = format!("http://host.docker.internal:{port}");
        let guest_ca = "/etc/pillbox-ca.crt";

        let mut out = vec![
            "-v".into(),
            format!("{}:{guest_ca}:ro", self.ca_cert_path.display()),
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
    let real: serde_json::Value = serde_json::from_slice(&real_bytes).map_err(|e| {
        PillboxError::runtime("vault", format!("parse {}: {e}", creds_path.display())).with_next(
            format!(
                "pillbox {} login   # credentials file is malformed",
                agent.agent_id
            ),
        )
    })?;

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
