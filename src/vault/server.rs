//! HTTPS MITM proxy that swaps stub credentials for real ones at the host
//! boundary, dispatching to one of N registered [`VaultProvider`]s.
//!
//! Behaviour:
//!  - For each request, ask each provider whether it claims the host.
//!    First match wins; non-matching hosts pass through (no MITM), so
//!    unrelated traffic from the guest is not exposed to our CA.
//!  - The matched provider rewrites the request and may set a
//!    [`PendingFlow`] so the following response is routed back to the
//!    same provider for tear-down (token rotation, real → stub swap).
//!
//! Stubs encode `sandbox_id` (see each provider), so any incoming stub
//! resolves to its sandbox regardless of the TCP source. A sandbox whose
//! lease has been dropped no longer resolves, so reused stubs from a
//! dead sandbox fail with 401.

use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
};

use hudsucker::{
    certificate_authority::RcgenAuthority,
    hyper::{Request, Response},
    rustls::crypto::aws_lc_rs,
    Body, HttpContext, HttpHandler, Proxy, RequestOrResponse,
};
use tokio::net::TcpListener;

use super::{
    ca::Ca,
    known_secrets::VaultMeta,
    lease::SandboxLease,
    providers::{
        self, host_from_uri, mint_stub, PendingFlow, Registry, SandboxData, VaultProvider,
        API_KEY_PROVIDER_ID,
    },
};

/// Configuration for [`Server::start`].
#[derive(Debug)]
pub struct ServerConfig {
    /// Optional bind address. Defaults to `127.0.0.1:0` (pick a free port).
    pub bind: Option<SocketAddr>,
    /// Directory the CA cert + key are persisted in.
    pub ca_dir: PathBuf,
}

/// In-memory shared state. Handler clones share the registry behind a
/// mutex; providers access it via [`Self::registry_lock`].
pub(crate) struct ServerInner {
    listen_addr: SocketAddr,
    ca: Ca,
    registry: Mutex<Registry>,
    providers: Vec<Arc<dyn VaultProvider>>,
    /// Sender side of a shutdown signal. Dropping this triggers proxy
    /// graceful shutdown via the receiver held by the spawned task.
    shutdown_tx: tokio::sync::watch::Sender<bool>,
}

impl ServerInner {
    pub(crate) fn drop_sandbox(&self, sandbox_id: &str) {
        let mut registry = self.registry_lock();
        registry.remove(sandbox_id);
    }

    /// Lock the registry. Provider impls call this from request/response
    /// handlers — a single global mutex is fine because all swaps are
    /// short and CPU-bound (JSON manipulation, no I/O while held).
    pub(crate) fn registry_lock(&self) -> MutexGuard<'_, Registry> {
        self.registry.lock().expect("vault registry mutex poisoned")
    }

    fn provider_for_host(&self, host: &str) -> Option<&Arc<dyn VaultProvider>> {
        self.providers.iter().find(|p| p.intercept(host))
    }

    fn provider_by_id(&self, id: &str) -> Option<&Arc<dyn VaultProvider>> {
        self.providers.iter().find(|p| p.id() == id)
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

        let providers: Vec<Arc<dyn VaultProvider>> =
            providers::registry().into_iter().map(Arc::from).collect();

        let inner = Arc::new(ServerInner {
            listen_addr,
            ca,
            registry: Mutex::new(Registry::new()),
            providers,
            shutdown_tx,
        });

        let handler = VaultHandler {
            server: Arc::clone(&inner),
            pending: None,
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

    /// Lease a per-sandbox stub mapping for the given provider. The
    /// provider's `provision` is invoked to mint stubs, register them in
    /// the registry, and produce the stub credentials body the caller
    /// should mount into the guest.
    pub fn lease(
        &self,
        provider_id: &str,
        sandbox_id: &str,
        real: serde_json::Value,
    ) -> Result<SandboxLease, String> {
        let provider = self
            .inner
            .provider_by_id(provider_id)
            .ok_or_else(|| format!("unknown vault provider `{provider_id}`"))?;

        let stub_body = {
            let mut registry = self.inner.registry_lock();
            provider.provision(sandbox_id, &real, &mut registry)?
        };

        Ok(SandboxLease::new(
            sandbox_id.to_string(),
            stub_body,
            Arc::clone(&self.inner),
        ))
    }

    /// Lease a stub for a `--with NAME=ENV_VAR --vault`'d API key.
    ///
    /// `vault_meta` carries the host the secret talks to, the header
    /// scheme, and the real-key prefix the stub should mimic. The stub
    /// is registered against a freshly-minted `sandbox_id` (separate
    /// from any OAuth lease's sandbox id) so dropping it releases just
    /// this entry.
    ///
    /// Returns the lease *and* the stub string the caller injects into
    /// the guest env. The stub mimics the real-key prefix so
    /// client-side format validators (e.g. an SDK that checks
    /// `key.starts_with("sk-ant-api03-")`) still accept it.
    pub fn lease_api_key(
        &self,
        secret_name: &str,
        real_value: &str,
        vault_meta: &VaultMeta,
    ) -> Result<(SandboxLease, String), String> {
        // Verify that a registered provider claims the host this stub
        // will travel to. Without one, the proxy would never see the
        // request and the swap would never fire — better to fail fast at
        // lease time.
        if self
            .inner
            .provider_for_host(&vault_meta.vault.host)
            .is_none()
        {
            return Err(format!(
                "no vault provider intercepts host `{}`; pillbox can't swap stubs there",
                vault_meta.vault.host
            ));
        }

        let sandbox_id = uuid::Uuid::now_v7().to_string();
        let stub = mint_stub(&vault_meta.vault.prefix, &sandbox_id);

        {
            let mut registry = self.inner.registry_lock();
            registry.insert(
                sandbox_id.clone(),
                SandboxData {
                    provider_id: API_KEY_PROVIDER_ID,
                    real: serde_json::json!({
                        "name": secret_name,
                        "value": real_value,
                        "host": vault_meta.vault.host,
                        "scheme": vault_meta.vault.header_scheme.as_str(),
                    }),
                    stubs: vec![stub.clone()],
                },
            );
        }

        let lease = SandboxLease::new(sandbox_id, stub.clone(), Arc::clone(&self.inner));
        Ok((lease, stub))
    }

    /// Convenience accessor matching `lease_api_key`'s vault-meta input
    /// for callers that only have a [`HeaderScheme`] in hand and don't
    /// want to plumb a full `VaultMeta`.
    #[cfg(test)]
    pub(crate) fn lease_api_key_for_test(
        &self,
        name: &str,
        real: &str,
        host: &str,
        scheme: super::known_secrets::HeaderScheme,
        prefix: &str,
    ) -> Result<(SandboxLease, String), String> {
        let meta = VaultMeta::new(host.into(), scheme, prefix.into());
        self.lease_api_key(name, real, &meta)
    }

    /// Test-only: borrow the inner registry mutex. Use `inner_for_test`
    /// from tests in this module; downstream tests use the lease's
    /// observable side-effects instead.
    #[cfg(test)]
    pub(crate) fn registry_lock_for_test(&self) -> MutexGuard<'_, Registry> {
        self.inner.registry_lock()
    }

    /// Test-only: borrow the shared `ServerInner`. Provider integration
    /// tests use this to call `handle_request` / `handle_response`
    /// directly on a constructed hyper Request/Response, bypassing the
    /// proxy/TLS stack.
    ///
    /// Why bypass instead of driving a real `Proxy`: hudsucker 0.24's
    /// public builder doesn't let us inject a custom upstream hyper
    /// connector, so a real end-to-end run would need either a
    /// `CONNECT`-through stub HTTP server or unsafe internals. Calling
    /// the provider methods directly covers the swap logic; the
    /// `should_intercept` → dispatch chain inside [`VaultHandler`] is a
    /// thin wrapper and is not currently exercised. Worth revisiting if
    /// hudsucker exposes the connector seam (or we change dispatch
    /// policy — provider priority, fall-through on overlapping
    /// `intercept`).
    #[cfg(test)]
    pub(crate) fn inner_for_test(&self) -> &ServerInner {
        &self.inner
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
    /// In-flight provider flow set by `handle_request` and consumed by
    /// `handle_response`. Lives one request/response pair.
    pending: Option<PendingFlow>,
}

impl HttpHandler for VaultHandler {
    async fn should_intercept(&mut self, _ctx: &HttpContext, req: &Request<Body>) -> bool {
        // CONNECT requests use authority form: host:port in the URI.
        let host = req.uri().host().map(str::to_owned).unwrap_or_default();
        self.server.provider_for_host(&host).is_some()
    }

    async fn handle_request(
        &mut self,
        _ctx: &HttpContext,
        req: Request<Body>,
    ) -> RequestOrResponse {
        let Some(host) = host_from_uri(&req) else {
            return req.into();
        };
        let provider = match self.server.provider_for_host(&host) {
            Some(p) => Arc::clone(p),
            None => return req.into(),
        };
        provider
            .handle_request(req, &self.server, &mut self.pending)
            .await
    }

    async fn handle_response(&mut self, _ctx: &HttpContext, res: Response<Body>) -> Response<Body> {
        let provider_id = match &self.pending {
            Some(flow) => flow.provider_id,
            None => return res,
        };
        let provider = match self.server.provider_by_id(provider_id) {
            Some(p) => Arc::clone(p),
            None => {
                // Provider disappeared (shouldn't happen — registry is
                // fixed for the server's lifetime). Drop the pending
                // flow so it doesn't poison the next response and pass
                // the body through.
                self.pending = None;
                return res;
            }
        };
        provider
            .handle_response(res, &self.server, &mut self.pending)
            .await
    }
}

#[cfg(test)]
mod tests {
    use crate::vault::providers::{
        anthropic, codex,
        test_support::{fresh_server, sample_anthropic_real, sample_codex_real},
    };

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
    async fn anthropic_lease_then_drop_removes_mapping() {
        let (server, dir) = fresh_server().await;
        let lease = server
            .lease("claude", "sbx-1", sample_anthropic_real())
            .expect("lease");

        // The stub body should contain the minted stubs (which encode
        // sandbox id "sbx1") and not the real tokens.
        let body = lease.stub_credentials_body();
        assert!(body.contains("sbx1"));
        assert!(!body.contains("REAL_ACCESS"));
        assert!(!body.contains("REAL_REFRESH"));

        // Pluck the stubs out of the registry to test resolve+drop.
        let stubs: Vec<String> = {
            let registry = server.registry_lock_for_test();
            registry.stubs_for("sbx-1").unwrap().to_vec()
        };
        for stub in &stubs {
            let registry = server.registry_lock_for_test();
            assert_eq!(registry.sandbox_for_stub(stub), Some("sbx-1"));
        }

        drop(lease);

        for stub in &stubs {
            let registry = server.registry_lock_for_test();
            assert!(registry.sandbox_for_stub(stub).is_none());
        }
        {
            let registry = server.registry_lock_for_test();
            assert!(registry.real("sbx-1").is_none());
        }

        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn codex_lease_then_drop_removes_mapping() {
        let (server, dir) = fresh_server().await;
        let lease = server
            .lease("codex", "sbx-cx", sample_codex_real())
            .expect("codex lease");

        let body = lease.stub_credentials_body();
        // Codex stubs use the pb-codex- family.
        assert!(body.contains(codex::STUB_ACCESS_PREFIX));
        assert!(body.contains(codex::STUB_REFRESH_PREFIX));
        // Real refresh token never leaks into the stub body.
        assert!(!body.contains("rt_codex_real"));

        let stubs: Vec<String> = {
            let registry = server.registry_lock_for_test();
            registry.stubs_for("sbx-cx").unwrap().to_vec()
        };
        assert_eq!(stubs.len(), 2);
        for stub in &stubs {
            let registry = server.registry_lock_for_test();
            assert_eq!(registry.sandbox_for_stub(stub), Some("sbx-cx"));
        }

        drop(lease);

        for stub in &stubs {
            let registry = server.registry_lock_for_test();
            assert!(registry.sandbox_for_stub(stub).is_none());
        }

        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn mixed_provider_leases_do_not_collide() {
        let (server, dir) = fresh_server().await;
        let a = server
            .lease("claude", "sbx-a", sample_anthropic_real())
            .expect("anthropic lease");
        let c = server
            .lease("codex", "sbx-c", sample_codex_real())
            .expect("codex lease");

        let a_stubs: Vec<String> = {
            let registry = server.registry_lock_for_test();
            registry.stubs_for("sbx-a").unwrap().to_vec()
        };
        let c_stubs: Vec<String> = {
            let registry = server.registry_lock_for_test();
            registry.stubs_for("sbx-c").unwrap().to_vec()
        };

        // No collisions on stub strings.
        for sa in &a_stubs {
            assert!(!c_stubs.contains(sa));
        }
        // Stubs use the right provider's prefix.
        assert!(a_stubs
            .iter()
            .all(|s| s.starts_with(anthropic::STUB_ACCESS_PREFIX)
                || s.starts_with(anthropic::STUB_REFRESH_PREFIX)));
        assert!(c_stubs
            .iter()
            .all(|s| s.starts_with(codex::STUB_ACCESS_PREFIX)
                || s.starts_with(codex::STUB_REFRESH_PREFIX)));

        // Dropping one provider's lease doesn't affect the other.
        drop(a);
        {
            let registry = server.registry_lock_for_test();
            for stub in &a_stubs {
                assert!(registry.sandbox_for_stub(stub).is_none());
            }
            for stub in &c_stubs {
                assert_eq!(registry.sandbox_for_stub(stub), Some("sbx-c"));
            }
        }

        drop(c);
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unknown_provider_id_errors() {
        let (server, dir) = fresh_server().await;
        let err = server
            .lease("nope", "sbx-1", sample_anthropic_real())
            .unwrap_err();
        assert!(err.contains("unknown vault provider"), "got: {err}");
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn lease_api_key_registers_swappable_stub() {
        use crate::vault::known_secrets::HeaderScheme;

        let (server, dir) = fresh_server().await;
        let (lease, stub) = server
            .lease_api_key_for_test(
                "ANTHROPIC_API_KEY",
                "sk-ant-api03-REAL",
                "api.anthropic.com",
                HeaderScheme::XApiKey,
                "sk-ant-api03-",
            )
            .expect("lease api key");

        // Stub mimics the prefix and is alphanumeric in the tail.
        assert!(stub.starts_with("sk-ant-api03-"));
        let tail = stub.strip_prefix("sk-ant-api03-").unwrap();
        assert!(tail.chars().all(|c| c.is_ascii_alphanumeric()));

        // Registry resolves stub → real via the API-key path.
        {
            let registry = server.registry_lock_for_test();
            assert_eq!(
                registry.api_key_real_for_stub(&stub),
                Some("sk-ant-api03-REAL"),
            );
        }

        drop(lease);

        // Stub vanishes from the registry on drop.
        {
            let registry = server.registry_lock_for_test();
            assert!(registry.api_key_real_for_stub(&stub).is_none());
        }

        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn lease_api_key_rejects_unknown_host() {
        use crate::vault::known_secrets::HeaderScheme;

        let (server, dir) = fresh_server().await;
        let err = server
            .lease_api_key_for_test(
                "MY_KEY",
                "real",
                "api.invented.example",
                HeaderScheme::AuthorizationBearer,
                "x-",
            )
            .unwrap_err();
        assert!(err.contains("no vault provider"), "got: {err}");
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn oauth_and_api_key_leases_coexist_on_same_server() {
        use crate::vault::known_secrets::HeaderScheme;

        let (server, dir) = fresh_server().await;
        let oauth = server
            .lease("claude", "sbx-oauth", sample_anthropic_real())
            .expect("oauth lease");
        let (api, stub) = server
            .lease_api_key_for_test(
                "ANTHROPIC_API_KEY",
                "sk-ant-api03-REAL",
                "api.anthropic.com",
                HeaderScheme::XApiKey,
                "sk-ant-api03-",
            )
            .expect("api key lease");

        // Both lookups work independently.
        {
            let registry = server.registry_lock_for_test();
            // OAuth stubs registered against sbx-oauth resolve real
            // accessToken via JSON pointer.
            let oauth_stubs = registry.stubs_for("sbx-oauth").unwrap().to_vec();
            for s in &oauth_stubs {
                assert_eq!(registry.sandbox_for_stub(s), Some("sbx-oauth"));
            }
            // API-key stub resolves real via api_key_real_for_stub.
            assert_eq!(
                registry.api_key_real_for_stub(&stub),
                Some("sk-ant-api03-REAL"),
            );
        }

        // Dropping the API-key lease leaves OAuth intact.
        drop(api);
        {
            let registry = server.registry_lock_for_test();
            assert!(registry.api_key_real_for_stub(&stub).is_none());
            assert!(registry.stubs_for("sbx-oauth").is_some());
        }

        drop(oauth);
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn two_anthropic_leases_have_distinct_stubs() {
        let (server, dir) = fresh_server().await;
        let a = server
            .lease("claude", "sbx-a", sample_anthropic_real())
            .expect("a");
        let b = server
            .lease("claude", "sbx-b", sample_anthropic_real())
            .expect("b");

        let (a_stubs, b_stubs) = {
            let registry = server.registry_lock_for_test();
            (
                registry.stubs_for("sbx-a").unwrap().to_vec(),
                registry.stubs_for("sbx-b").unwrap().to_vec(),
            )
        };
        for sa in &a_stubs {
            assert!(!b_stubs.contains(sa));
            assert!(sa.contains("sbxa"));
        }
        for sb in &b_stubs {
            assert!(sb.contains("sbxb"));
        }

        drop(a);
        {
            let registry = server.registry_lock_for_test();
            for sa in &a_stubs {
                assert!(registry.sandbox_for_stub(sa).is_none());
            }
            for sb in &b_stubs {
                assert_eq!(registry.sandbox_for_stub(sb), Some("sbx-b"));
            }
        }

        drop(b);
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── Cross-provider routing ────────────────────────────────────────
    //
    // The Registry is shared across all providers, so any provider can
    // *resolve* any stub. Provider isolation comes from each provider
    // only acting on stubs whose prefix or shape it recognises. These
    // tests pin that contract from the registry side.

    #[tokio::test]
    async fn registry_resolves_stubs_to_owning_sandbox_regardless_of_provider() {
        let (server, dir) = fresh_server().await;
        let _a = server
            .lease("claude", "sbx-claude", sample_anthropic_real())
            .expect("claude lease");
        let _c = server
            .lease("codex", "sbx-codex", sample_codex_real())
            .expect("codex lease");

        let registry = server.registry_lock_for_test();
        for stub in registry.stubs_for("sbx-claude").unwrap() {
            assert_eq!(registry.sandbox_for_stub(stub), Some("sbx-claude"));
            // Confirm the stub looks anthropic-y, not codex-y.
            assert!(
                stub.starts_with(crate::vault::providers::anthropic::STUB_ACCESS_PREFIX)
                    || stub.starts_with(crate::vault::providers::anthropic::STUB_REFRESH_PREFIX),
                "claude stubs should carry anthropic prefix, got {stub}"
            );
        }
        for stub in registry.stubs_for("sbx-codex").unwrap() {
            assert_eq!(registry.sandbox_for_stub(stub), Some("sbx-codex"));
            assert!(
                stub.starts_with(crate::vault::providers::codex::STUB_ACCESS_PREFIX)
                    || stub.starts_with(crate::vault::providers::codex::STUB_REFRESH_PREFIX),
                "codex stubs should carry codex prefix, got {stub}"
            );
        }
        drop(registry);

        drop(_a);
        drop(_c);
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn anthropic_oauth_stub_seen_by_openai_handler_is_no_op() {
        // The vault server dispatches by host, not by stub — but if an
        // anthropic-prefixed stub ever flows through the openai handler
        // (e.g. a misconfigured guest sends api.openai.com a Bearer
        // sk-ant-oat01-…), the openai handler must NOT swap the
        // header. The anthropic OAuth entry is OAuth-shaped, not
        // API-key-shaped, so `api_key_real_for_stub` returns None →
        // `swap_bearer_style` returns PassThrough.
        use hudsucker::{
            hyper::{header::AUTHORIZATION, Request},
            Body,
        };

        let (server, dir) = fresh_server().await;
        let _lease = server
            .lease("claude", "sbx-x", sample_anthropic_real())
            .expect("lease");
        let stub_access = {
            let registry = server.registry_lock_for_test();
            registry
                .stubs_for("sbx-x")
                .unwrap()
                .iter()
                .find(|s| s.starts_with(crate::vault::providers::anthropic::STUB_ACCESS_PREFIX))
                .cloned()
                .unwrap()
        };

        let req = Request::builder()
            .method("POST")
            .uri("https://api.openai.com/v1/chat/completions")
            .header(AUTHORIZATION, format!("Bearer {stub_access}"))
            .body(Body::empty())
            .unwrap();

        let openai = crate::vault::providers::openai::OpenAiApiKeyProvider;
        let mut pending: Option<crate::vault::providers::PendingFlow> = None;
        let out = crate::vault::providers::VaultProvider::handle_request(
            &openai,
            req,
            server.inner_for_test(),
            &mut pending,
        )
        .await;
        let out_req = match out {
            hudsucker::RequestOrResponse::Request(r) => r,
            hudsucker::RequestOrResponse::Response(r) => {
                panic!("expected pass-through Request, got status {}", r.status())
            }
        };
        // Bearer header preserved verbatim — openai didn't touch the
        // anthropic stub.
        assert_eq!(
            out_req
                .headers()
                .get(AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            format!("Bearer {stub_access}")
        );

        drop(_lease);
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
