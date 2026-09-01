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
    /// Holds the per-run ephemeral CA's tempdir alive until teardown (the cert
    /// file is bind-mounted into the guest for the run's duration). `None` when a
    /// stable persistent CA is in use. Last field → dropped last, after the
    /// server, so the cert outlives anything reading it.
    _ca_tempdir: Option<tempfile::TempDir>,
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
    let mut real: serde_json::Value = serde_json::from_slice(&real_bytes).map_err(|e| {
        PillboxError::runtime("vault", format!("parse {}: {e}", creds_path.display())).with_next(
            format!(
                "pillbox {} login   # credentials file is malformed",
                agent.agent_id
            ),
        )
    })?;

    // Establish a fresh OAuth token before leasing — coordinated across concurrent
    // sessions via the single-writer TokenStore (see `super::refresh`). In the broker
    // model the guest's stub carries a far-future expiry and never self-refreshes, so
    // this is the *only* refresher: a stale token here would 401 with no recovery.
    // Fail closed rather than lease a doomed credential — `pre_refresh` returns
    // `Ok(None)` only for agents without a registered OAuth broker codec.
    if let Some(fresh) = super::refresh::pre_refresh(&creds_path, agent.agent_id)? {
        real = fresh;
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

/// Write `content` to `path` as a 0600 file, creating-or-truncating. Used to drop
/// the per-run stub creds into its temp file. (Rotated *real* creds are written by
/// the [`super::token_store::TokenStore`] under the rotation lock, not here.)
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
