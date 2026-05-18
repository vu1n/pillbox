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
use crate::paths;
use crate::vault::{providers, SandboxLease, Server, ServerConfig};

pub(crate) struct VaultSession {
    // Order matters — see module doc.
    _lease: SandboxLease,
    _server: Server,
    _runtime: tokio::runtime::Runtime,
    stub_file: tempfile::NamedTempFile,
    ca_cert_path: PathBuf,
    listen_addr: SocketAddr,
    /// Guest-relative path the stub file is mounted at (e.g.
    /// `.claude/.credentials.json` or `.codex/auth.json`). The agent
    /// provider tells us where.
    creds_path: PathBuf,
}

impl VaultSession {
    /// Spin up the vault proxy for one sandbox run.
    ///
    /// `agent_id` selects the provider (must match an entry in
    /// [`providers::registry`]). `agent_home` is the host directory
    /// bind-mounted at `/home/lum` inside the guest (e.g.
    /// `~/.pillbox/data/<agent>/`). The real creds are loaded from
    /// `<agent_home>/<provider creds_path>`.
    ///
    /// Caller is responsible for checking `AgentSpec::vault_capable`
    /// before invoking.
    pub(crate) fn start(agent_id: &str, agent_home: &Path) -> Result<Self> {
        let provider = providers::provider_for(agent_id).ok_or_else(|| {
            PillboxError::runtime(
                "vault",
                format!("no vault provider for agent `{agent_id}`"),
            )
        })?;

        let creds_rel = provider.creds_path().to_path_buf();
        let creds_path = agent_home.join(&creds_rel);

        let real_bytes = fs::read(&creds_path).map_err(|e| {
            PillboxError::runtime(
                "vault",
                format!("read {}: {e}", creds_path.display()),
            )
            .with_next(format!(
                "pillbox {agent_id} login   # refresh credentials"
            ))
        })?;
        let real: serde_json::Value = serde_json::from_slice(&real_bytes).map_err(|e| {
            PillboxError::runtime(
                "vault",
                format!("parse {}: {e}", creds_path.display()),
            )
            .with_next(format!(
                "pillbox {agent_id} login   # credentials file is malformed"
            ))
        })?;

        let ca_dir = paths::data_subdir("vault")?;

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

        Ok(Self {
            _lease: lease,
            _server: server,
            _runtime: runtime,
            stub_file,
            ca_cert_path,
            listen_addr,
            creds_path: creds_rel,
        })
    }

    /// Extra docker args to layer onto a normal `<agent> run`:
    /// `-v stubfile:<guest_home>/<creds_path>:ro`,
    /// `-v cacert:/etc/pillbox-ca.crt:ro`,
    /// `-e NODE_EXTRA_CA_CERTS=...`, `-e HTTPS_PROXY=...`, `-e HTTP_PROXY=...`.
    pub(crate) fn docker_extras(&self, guest_home: &str) -> Vec<String> {
        let port = self.listen_addr.port();
        // host.docker.internal works on Docker Desktop (macOS/Windows). Linux
        // needs --add-host=host.docker.internal:host-gateway; we add it
        // unconditionally — Docker Desktop ignores it harmlessly.
        let proxy_url = format!("http://host.docker.internal:{port}");
        let guest_ca = "/etc/pillbox-ca.crt";
        let guest_creds = format!("{guest_home}/{}", self.creds_path.display());

        vec![
            "--add-host".into(),
            "host.docker.internal:host-gateway".into(),
            "-v".into(),
            format!("{}:{guest_ca}:ro", self.ca_cert_path.display()),
            "-v".into(),
            format!("{}:{guest_creds}:ro", self.stub_file.path().display()),
            "-e".into(),
            format!("NODE_EXTRA_CA_CERTS={guest_ca}"),
            "-e".into(),
            format!("HTTPS_PROXY={proxy_url}"),
            "-e".into(),
            format!("HTTP_PROXY={proxy_url}"),
        ]
    }

    pub(crate) fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    pub(crate) fn ca_cert_path(&self) -> &Path {
        &self.ca_cert_path
    }
}

fn write_private(path: &Path, content: &str) -> Result<()> {
    let mut f = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| {
            PillboxError::runtime("vault", format!("open {}: {e}", path.display()))
        })?;
    f.write_all(content.as_bytes()).map_err(|e| {
        PillboxError::runtime("vault", format!("write {}: {e}", path.display()))
    })?;
    Ok(())
}
