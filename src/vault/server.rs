//! HTTPS MITM proxy that swaps stub credentials for real ones at the host
//! boundary.
//!
//! Behaviour:
//!  - Hosts `api.anthropic.com` and `console.anthropic.com` → MITM,
//!    inspect/rewrite headers and bodies.
//!  - All other hosts → pass through (no MITM), so unrelated traffic from
//!    the guest is not exposed to our CA.
//!
//! Stub binding: stubs encode `sandbox_id` (see `lease.rs`), so any
//! incoming stub can be resolved to its sandbox regardless of the TCP
//! source. A sandbox whose lease has been dropped no longer resolves, so
//! reused stubs from a dead sandbox fail with 401.

use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use http_body_util::BodyExt;
use hudsucker::{
    certificate_authority::RcgenAuthority,
    hyper::{
        header::{HeaderValue, AUTHORIZATION},
        Request, Response, StatusCode,
    },
    rustls::crypto::aws_lc_rs,
    Body, HttpContext, HttpHandler, Proxy, RequestOrResponse,
};
use tokio::net::TcpListener;

use super::{
    ca::Ca,
    lease::{build_stub_json, SandboxEntry, SandboxLease},
    secrets::AnthropicCreds,
};

const ANTHROPIC_API_HOST: &str = "api.anthropic.com";
const ANTHROPIC_CONSOLE_HOST: &str = "console.anthropic.com";
const OAUTH_TOKEN_PATH_SUFFIX: &str = "/oauth/token";

/// Configuration for [`Server::start`].
#[derive(Debug)]
pub struct ServerConfig {
    /// Optional bind address. Defaults to `127.0.0.1:0` (pick a free port).
    pub bind: Option<SocketAddr>,
    /// Directory the CA cert + key are persisted in.
    pub ca_dir: PathBuf,
}

/// In-memory mapping shared by the server and all handler clones.
pub(crate) struct ServerInner {
    listen_addr: SocketAddr,
    ca: Ca,
    map: Mutex<VaultMap>,
    /// Sender side of a shutdown signal. Dropping this triggers proxy
    /// graceful shutdown via the receiver held by the spawned task.
    shutdown_tx: tokio::sync::watch::Sender<bool>,
}

struct VaultMap {
    /// sandbox_id → entry. Removing this entry invalidates all stubs
    /// belonging to the sandbox.
    by_sandbox: HashMap<String, SandboxEntry>,
    /// stub_refresh → sandbox_id
    stub_refresh_to_sandbox: HashMap<String, String>,
    /// stub_access → sandbox_id
    stub_access_to_sandbox: HashMap<String, String>,
}

impl VaultMap {
    fn new() -> Self {
        Self {
            by_sandbox: HashMap::new(),
            stub_refresh_to_sandbox: HashMap::new(),
            stub_access_to_sandbox: HashMap::new(),
        }
    }

    fn insert(&mut self, sandbox_id: String, entry: SandboxEntry) {
        self.stub_refresh_to_sandbox
            .insert(entry.stub_refresh.clone(), sandbox_id.clone());
        self.stub_access_to_sandbox
            .insert(entry.stub_access.clone(), sandbox_id.clone());
        self.by_sandbox.insert(sandbox_id, entry);
    }

    fn remove(&mut self, sandbox_id: &str) -> Option<SandboxEntry> {
        let entry = self.by_sandbox.remove(sandbox_id)?;
        self.stub_refresh_to_sandbox.remove(&entry.stub_refresh);
        self.stub_access_to_sandbox.remove(&entry.stub_access);
        Some(entry)
    }

    fn sandbox_for_stub_access(&self, stub: &str) -> Option<&str> {
        self.stub_access_to_sandbox.get(stub).map(String::as_str)
    }

    fn sandbox_for_stub_refresh(&self, stub: &str) -> Option<&str> {
        self.stub_refresh_to_sandbox.get(stub).map(String::as_str)
    }

    fn real_access(&self, sandbox_id: &str) -> Option<&str> {
        self.by_sandbox
            .get(sandbox_id)
            .map(|entry| entry.real.real_access())
    }

    fn real_refresh(&self, sandbox_id: &str) -> Option<&str> {
        self.by_sandbox
            .get(sandbox_id)
            .map(|entry| entry.real.real_refresh())
    }

    fn stub_access(&self, sandbox_id: &str) -> Option<&str> {
        self.by_sandbox
            .get(sandbox_id)
            .map(|entry| entry.stub_access.as_str())
    }

    fn stub_refresh(&self, sandbox_id: &str) -> Option<&str> {
        self.by_sandbox
            .get(sandbox_id)
            .map(|entry| entry.stub_refresh.as_str())
    }

    fn rotate_real_access(&mut self, sandbox_id: &str, new_real_access: String) {
        if let Some(entry) = self.by_sandbox.get_mut(sandbox_id) {
            entry.real.real_access = new_real_access;
        }
    }

    fn rotate_real_refresh(&mut self, sandbox_id: &str, new_real_refresh: String) {
        if let Some(entry) = self.by_sandbox.get_mut(sandbox_id) {
            entry.real.real_refresh = new_real_refresh;
        }
    }
}

impl ServerInner {
    pub(crate) fn drop_sandbox(&self, sandbox_id: &str) {
        let mut map = self.map.lock().expect("vault map mutex poisoned");
        map.remove(sandbox_id);
    }
}

/// Public handle to a running vault proxy server.
pub struct Server {
    inner: Arc<ServerInner>,
    /// Kept so the proxy task isn't dropped before `Server` is. Shutdown
    /// is signalled separately via `inner.shutdown_tx` in `Drop`.
    _proxy_task: tokio::task::JoinHandle<()>,
}

impl Server {
    /// Start a vault proxy server. Returns once the listener is bound and
    /// the proxy task is spawned.
    pub async fn start(config: ServerConfig) -> Result<Self, String> {
        let ca = Ca::ensure(&config.ca_dir)?;
        let issuer = ca.issuer()?;
        let authority = RcgenAuthority::new(issuer, 1_000, aws_lc_rs::default_provider());

        let bind_addr = config
            .bind
            .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 0)));
        let listener = TcpListener::bind(bind_addr)
            .await
            .map_err(|error| format!("vault proxy bind {bind_addr}: {error}"))?;
        let listen_addr = listener
            .local_addr()
            .map_err(|error| format!("vault proxy local_addr: {error}"))?;

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

        let inner = Arc::new(ServerInner {
            listen_addr,
            ca,
            map: Mutex::new(VaultMap::new()),
            shutdown_tx,
        });

        let handler = VaultHandler {
            server: Arc::clone(&inner),
            pending_oauth_sandbox: None,
        };

        let shutdown_signal = async move {
            // Watch for the sender being signalled OR dropped.
            let _ = shutdown_rx.changed().await;
        };

        let proxy = Proxy::builder()
            .with_listener(listener)
            .with_ca(authority)
            .with_rustls_connector(aws_lc_rs::default_provider())
            .with_http_handler(handler)
            .with_graceful_shutdown(shutdown_signal)
            .build()
            .map_err(|error| format!("build vault proxy: {error}"))?;

        let proxy_task = tokio::spawn(async move {
            if let Err(error) = proxy.start().await {
                eprintln!("pillbox: vault proxy stopped with error: {error}");
            }
        });

        Ok(Self {
            inner,
            _proxy_task: proxy_task,
        })
    }

    pub fn listen_addr(&self) -> SocketAddr {
        self.inner.listen_addr
    }

    pub fn ca_cert_path(&self) -> &std::path::Path {
        self.inner.ca.cert_path()
    }

    /// Lease a per-sandbox stub mapping. The returned [`SandboxLease`]
    /// must outlive the sandbox; dropping it removes the stub mapping.
    pub fn lease(
        &self,
        sandbox_id: &str,
        real: AnthropicCreds,
    ) -> Result<SandboxLease, String> {
        let entry = SandboxEntry::new(sandbox_id, real.clone());
        let stub_refresh = entry.stub_refresh.clone();
        let stub_access = entry.stub_access.clone();
        let stub_json = build_stub_json(&entry.real, &stub_access, &stub_refresh)?;

        {
            let mut map = self.inner.map.lock().expect("vault map mutex poisoned");
            map.insert(sandbox_id.to_string(), entry);
        }

        Ok(SandboxLease::new(
            sandbox_id.to_string(),
            stub_refresh,
            stub_access,
            stub_json,
            Arc::clone(&self.inner),
        ))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.inner.shutdown_tx.send(true);
    }
}

// ── Handler ──────────────────────────────────────────────────────────────

#[derive(Clone)]
struct VaultHandler {
    server: Arc<ServerInner>,
    /// Sandbox id of the in-flight OAuth refresh request, set in
    /// `handle_request` and consumed in `handle_response` so we can swap
    /// the returned real tokens back to stubs.
    pending_oauth_sandbox: Option<String>,
}

impl VaultHandler {
    fn is_anthropic_host(host: &str) -> bool {
        host == ANTHROPIC_API_HOST || host == ANTHROPIC_CONSOLE_HOST
    }

    fn host_from_uri(req: &Request<Body>) -> Option<String> {
        req.uri()
            .host()
            .map(str::to_owned)
            .or_else(|| {
                req.headers()
                    .get("host")
                    .and_then(|h| h.to_str().ok())
                    .map(|s| s.split(':').next().unwrap_or(s).to_string())
            })
    }

    fn unauthorized(detail: &str) -> Response<Body> {
        let body = format!(r#"{{"vault":"unauthorized","detail":"{detail}"}}"#);
        Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .expect("build vault unauthorized response")
    }
}

impl HttpHandler for VaultHandler {
    async fn should_intercept(&mut self, _ctx: &HttpContext, req: &Request<Body>) -> bool {
        // CONNECT requests use authority form: host:port in the URI.
        let host = req.uri().host().map(str::to_owned).unwrap_or_default();
        Self::is_anthropic_host(&host)
    }

    async fn handle_request(
        &mut self,
        _ctx: &HttpContext,
        req: Request<Body>,
    ) -> RequestOrResponse {
        let Some(host) = Self::host_from_uri(&req) else {
            return req.into();
        };
        if !Self::is_anthropic_host(&host) {
            return req.into();
        }
        if host == ANTHROPIC_CONSOLE_HOST && req.uri().path().ends_with(OAUTH_TOKEN_PATH_SUFFIX) {
            return self.handle_oauth_request(req).await;
        }
        if host == ANTHROPIC_API_HOST {
            return self.handle_api_request(req).await;
        }
        req.into()
    }

    async fn handle_response(
        &mut self,
        _ctx: &HttpContext,
        res: Response<Body>,
    ) -> Response<Body> {
        let Some(sandbox_id) = self.pending_oauth_sandbox.take() else {
            return res;
        };

        // OAuth response: swap real_access/real_refresh in body with stubs,
        // and persist any rotated values into the vault map.
        let (parts, body) = res.into_parts();
        let collected = match body.collect().await {
            Ok(c) => c.to_bytes(),
            Err(error) => {
                eprintln!("pillbox: vault: failed to collect oauth response body: {error}");
                return Response::from_parts(parts, Body::empty());
            }
        };

        let mut value: serde_json::Value = match serde_json::from_slice(&collected) {
            Ok(v) => v,
            Err(error) => {
                eprintln!("pillbox: vault: oauth response not JSON; passing through: {error}");
                return Response::from_parts(parts, Body::from(collected));
            }
        };

        let (stub_access, stub_refresh) = {
            let mut map = self.server.map.lock().expect("vault map mutex poisoned");
            if let Some(obj) = value.as_object_mut() {
                if let Some(new_access) = obj.get("access_token").and_then(|v| v.as_str()).map(str::to_owned)
                {
                    map.rotate_real_access(&sandbox_id, new_access);
                }
                if let Some(new_refresh) = obj
                    .get("refresh_token")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
                {
                    map.rotate_real_refresh(&sandbox_id, new_refresh);
                }
            }
            (
                map.stub_access(&sandbox_id).map(str::to_owned),
                map.stub_refresh(&sandbox_id).map(str::to_owned),
            )
        };

        if let Some(obj) = value.as_object_mut() {
            if let (Some(s), true) = (stub_access.as_ref(), obj.contains_key("access_token")) {
                obj.insert("access_token".to_string(), serde_json::Value::String(s.clone()));
            }
            if let (Some(s), true) = (stub_refresh.as_ref(), obj.contains_key("refresh_token")) {
                obj.insert("refresh_token".to_string(), serde_json::Value::String(s.clone()));
            }
        }

        let new_body = serde_json::to_vec(&value).unwrap_or(collected.to_vec());
        let new_len = new_body.len();
        let mut parts = parts;
        parts.headers.remove("content-length");
        parts
            .headers
            .insert("content-length", HeaderValue::from(new_len));
        Response::from_parts(parts, Body::from(new_body))
    }
}

impl VaultHandler {
    async fn handle_oauth_request(&mut self, req: Request<Body>) -> RequestOrResponse {
        let (mut parts, body) = req.into_parts();
        let collected = match body.collect().await {
            Ok(c) => c.to_bytes(),
            Err(error) => {
                eprintln!("pillbox: vault: failed to collect oauth request body: {error}");
                return Self::unauthorized("body read error").into();
            }
        };

        let mut value: serde_json::Value = match serde_json::from_slice(&collected) {
            Ok(v) => v,
            Err(_) => {
                // Not JSON; we can't rewrite. Reject — refusing is safer
                // than leaking the real token by accident.
                return Self::unauthorized("non-json oauth body").into();
            }
        };

        let stub_refresh = value
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .map(str::to_owned);

        let real_refresh = if let Some(stub) = stub_refresh.as_deref() {
            let map = self.server.map.lock().expect("vault map mutex poisoned");
            let sandbox_id = match map.sandbox_for_stub_refresh(stub) {
                Some(s) => s.to_string(),
                None => return Self::unauthorized("unknown stub refresh token").into(),
            };
            self.pending_oauth_sandbox = Some(sandbox_id.clone());
            map.real_refresh(&sandbox_id).map(str::to_owned)
        } else {
            None
        };

        if let (Some(obj), Some(real)) = (value.as_object_mut(), real_refresh) {
            obj.insert(
                "refresh_token".to_string(),
                serde_json::Value::String(real),
            );
        }

        let new_body = serde_json::to_vec(&value).unwrap_or(collected.to_vec());
        let new_len = new_body.len();
        parts.headers.remove("content-length");
        parts
            .headers
            .insert("content-length", HeaderValue::from(new_len));
        Request::from_parts(parts, Body::from(new_body)).into()
    }

    async fn handle_api_request(&mut self, req: Request<Body>) -> RequestOrResponse {
        let (mut parts, body) = req.into_parts();

        let Some(auth_value) = parts.headers.get(AUTHORIZATION).cloned() else {
            // No Authorization header — let upstream return its own error.
            return Request::from_parts(parts, body).into();
        };
        let Ok(auth_str) = auth_value.to_str() else {
            return Self::unauthorized("non-utf8 authorization").into();
        };
        let Some(stub) = auth_str.strip_prefix("Bearer ") else {
            return Self::unauthorized("non-bearer authorization").into();
        };

        let real_access = {
            let map = self.server.map.lock().expect("vault map mutex poisoned");
            let sandbox_id = match map.sandbox_for_stub_access(stub) {
                Some(s) => s.to_string(),
                None => return Self::unauthorized("unknown stub access token").into(),
            };
            map.real_access(&sandbox_id).map(str::to_owned)
        };

        if let Some(real) = real_access {
            let new_value = format!("Bearer {real}");
            match HeaderValue::from_str(&new_value) {
                Ok(hv) => {
                    parts.headers.insert(AUTHORIZATION, hv);
                }
                Err(error) => {
                    eprintln!("pillbox: vault: invalid real access token header: {error}");
                    return Self::unauthorized("invalid real token").into();
                }
            }
        }

        Request::from_parts(parts, body).into()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::vault::{secrets::AnthropicCreds, server::ServerConfig, Server};

    fn sample_creds() -> AnthropicCreds {
        AnthropicCreds::from_bytes(
            br#"{
                "claudeAiOauth": {
                    "accessToken": "REAL_ACCESS",
                    "refreshToken": "REAL_REFRESH",
                    "expiresAt": 1700000000
                }
            }"#,
        )
        .expect("parse")
    }

    async fn fresh_server() -> (Server, PathBuf) {
        let dir = std::env::temp_dir()
            .join(format!("pillbox-vault-server-{}", uuid::Uuid::now_v7()));
        let server = Server::start(ServerConfig {
            bind: None,
            ca_dir: dir.clone(),
        })
        .await
        .expect("server start");
        (server, dir)
    }

    #[tokio::test]
    async fn server_binds_and_writes_ca_cert() {
        let (server, dir) = fresh_server().await;
        let addr = server.listen_addr();
        assert!(addr.port() > 0);
        let cert = std::fs::read_to_string(server.ca_cert_path()).unwrap();
        assert!(cert.starts_with("-----BEGIN CERTIFICATE-----"));
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn lease_then_drop_removes_mapping() {
        let (server, dir) = fresh_server().await;
        let lease = server.lease("sbx-1", sample_creds()).expect("lease");
        let stub_refresh = lease.stub_refresh().to_string();
        // Dashes stripped — "sbx-1" becomes "sbx1" inside the token tail.
        assert!(stub_refresh.contains("sbx1"));
        assert!(lease.stub_credentials_json().contains(&stub_refresh));
        // Real values must not appear in the stub JSON.
        assert!(!lease.stub_credentials_json().contains("REAL_ACCESS"));
        assert!(!lease.stub_credentials_json().contains("REAL_REFRESH"));

        // While lease is alive, stub resolves
        {
            let map = server.inner.map.lock().unwrap();
            assert!(map.sandbox_for_stub_refresh(&stub_refresh).is_some());
        }

        drop(lease);

        // After lease drop, stub no longer resolves
        {
            let map = server.inner.map.lock().unwrap();
            assert!(map.sandbox_for_stub_refresh(&stub_refresh).is_none());
        }

        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn two_concurrent_leases_have_distinct_stubs() {
        let (server, dir) = fresh_server().await;
        let a = server.lease("sbx-a", sample_creds()).expect("a");
        let b = server.lease("sbx-b", sample_creds()).expect("b");
        assert_ne!(a.stub_refresh(), b.stub_refresh());
        assert_ne!(a.stub_access(), b.stub_access());
        assert!(a.stub_refresh().contains("sbxa"));
        assert!(b.stub_refresh().contains("sbxb"));

        // Snapshot a's stub before dropping the lease.
        let a_stub_refresh = a.stub_refresh().to_string();
        let b_stub_refresh = b.stub_refresh().to_string();

        drop(a);
        // After dropping a, b still works; a's stub no longer resolves.
        {
            let map = server.inner.map.lock().unwrap();
            assert!(map.sandbox_for_stub_refresh(&b_stub_refresh).is_some());
            assert!(map.sandbox_for_stub_refresh(&a_stub_refresh).is_none());
        }

        drop(b);
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
