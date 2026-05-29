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
    time::SystemTime,
};

use http_body_util::combinators::BoxBody;
use hudsucker::{
    certificate_authority::RcgenAuthority,
    hyper::{Request, Response},
    rustls::crypto::aws_lc_rs,
    Body, HttpContext, HttpHandler, Proxy, RequestOrResponse,
};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

use super::{
    ca::Ca,
    genai_tap::TappedBody,
    known_secrets::VaultMeta,
    lease::SandboxLease,
    providers::{
        self, extract_inbound_stub, host_from_uri, mint_stub, PendingFlow, Registry, SandboxData,
        VaultProvider, API_KEY_PROVIDER_ID,
    },
};
use crate::events::{emit_genai_call_span, GenAiCallSpan};

/// Per-run context surfaced as attributes on telemetry spans the
/// vault emits. Each field is `Option<String>` so callers without a
/// value (test fixtures, the ad-hoc `sidecar` command) just leave it
/// — the attribute is omitted from emitted spans, not emitted as
/// empty.
///
/// Threaded through [`ServerConfig`] / [`super::VaultSession::start`]
/// and `#[serde(flatten)]`'d into
/// [`crate::sandbox::remote_ssh::VaultStdinBlob`] so the wire shape
/// gains a new field automatically when [`RunContext`] does, without
/// per-launcher / per-dispatcher updates.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct RunContext {
    /// Pillbox-run session id. When `Some`, gen_ai spans share a
    /// trace with the sandbox-side session span (same
    /// `derive_trace_id`) and parent it.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Orchestration mode — `"interactive"` (foreground attach),
    /// `"detached"` (run-and-detach), future modes as we grow them.
    /// Surfaces as `pillbox.mode` on gen_ai spans; lets eval
    /// scoring stratify by attentiveness regime.
    #[serde(default)]
    pub mode: Option<String>,
    /// Path-encoded pillbox key (e.g. `-Users-vuln-code-foo`) or
    /// `"global"`. Surfaces as `pillbox.workspace_id`; lets
    /// consumers group runs by project.
    #[serde(default)]
    pub workspace_id: Option<String>,
}

impl RunContext {
    /// Wire string for the `pillbox.mode` attribute, derived from the
    /// `--detach` flag. Centralized here so launchers across all
    /// sandbox backends share the same vocabulary (no `"detach"` vs
    /// `"detached"` drift).
    pub(crate) fn mode_for(detach: bool) -> &'static str {
        if detach {
            "detached"
        } else {
            "interactive"
        }
    }
}

/// Configuration for [`Server::start`].
#[derive(Debug)]
pub struct ServerConfig {
    /// Optional bind address. Defaults to `127.0.0.1:0` (pick a free port).
    pub bind: Option<SocketAddr>,
    /// Directory the CA cert + key are persisted in.
    pub ca_dir: PathBuf,
    /// Per-run context surfaced as attributes on gen_ai spans.
    pub context: RunContext,
}

/// In-memory shared state. Handler clones share the registry behind a
/// mutex; providers access it via [`Self::registry_lock`].
pub(crate) struct ServerInner {
    listen_addr: SocketAddr,
    ca: Ca,
    registry: Mutex<Registry>,
    providers: Vec<Arc<dyn VaultProvider>>,
    /// See [`ServerConfig::context`].
    context: RunContext,
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

    pub(crate) fn context(&self) -> &RunContext {
        &self.context
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
            context: config.context,
            shutdown_tx,
        });

        let handler = VaultHandler {
            server: Arc::clone(&inner),
            pending: None,
            in_flight: None,
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

    /// Snapshot the current real credentials for `sandbox_id`, cloned out
    /// of the registry. Returns `None` if no lease for that sandbox is
    /// live. Used at session teardown to persist tokens the in-proxy
    /// refresh rotated during the run (the registry holds the rotated
    /// values; the on-disk creds file would otherwise keep the stale —
    /// and, post-rotation, invalidated — refresh token).
    pub(crate) fn snapshot_real(&self, sandbox_id: &str) -> Option<serde_json::Value> {
        self.inner.registry_lock().real(sandbox_id).cloned()
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
    /// Telemetry shadow of the in-flight request. Captures wall-clock
    /// start + the inbound auth stub so `handle_response` can emit a
    /// `gen_ai` span tagged with the resolved sandbox_id. Independent
    /// of `pending` — bearer-token calls (the hot path for chat) don't
    /// set `pending` but we still want a span for them.
    in_flight: Option<InFlightCall>,
}

/// Per-request telemetry capture. Lives one request/response pair on
/// the handler instance; cleared by `handle_response` after the span
/// is emitted.
#[derive(Clone, Debug)]
struct InFlightCall {
    start: SystemTime,
    host: String,
    method: String,
    path: String,
    /// Stub token extracted from `Authorization: Bearer …` /
    /// `Authorization: token …` / `x-api-key` *before* the provider
    /// rewrites it. `handle_response` resolves this to a sandbox_id
    /// via the registry; if the lookup fails (legitimate `--with`'d
    /// real key, no header at all) the span is skipped.
    auth_stub: Option<String>,
    /// Whether this request is the provider's chat/generation endpoint
    /// (`is_chat_request`). Gates BOTH input capture and gen_ai span
    /// emission: the proxy sees many non-generation calls to the same
    /// host (telemetry batches, bootstrap, MCP registry, count_tokens),
    /// and emitting a `chat` span for those mislabels them as LLM
    /// generations with empty messages/usage. We only emit for real
    /// generations.
    is_chat: bool,
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
        mut req: Request<Body>,
    ) -> RequestOrResponse {
        let Some(host) = host_from_uri(&req) else {
            return req.into();
        };
        let provider = match self.server.provider_for_host(&host) {
            Some(p) => Arc::clone(p),
            None => return req.into(),
        };

        // Refuse permessage-deflate on any WebSocket we intercept. The
        // MITM terminates and *re-frames* the WS (separate tungstenite
        // handshakes client↔proxy and proxy↔upstream), and this relay
        // path has no deflate support — so if we forwarded the client's
        // `Sec-WebSocket-Extensions: permessage-deflate` upstream, the
        // server would send RSV1-compressed frames the relay can't
        // decode ("Reserved bits are non-zero") and the socket dies.
        // Dropping the header negotiates an uncompressed WS on both hops.
        // Harmless on non-upgrade requests (header simply absent). This
        // is what makes codex's WebSocket transport work through the
        // vault instead of falling back to HTTPS.
        req.headers_mut().remove("sec-websocket-extensions");

        // Capture the in-flight shape BEFORE the provider rewrites the
        // Authorization header — we need the *stub* value to resolve
        // sandbox_id at response time. Set synchronously (no await
        // before it) so the request→response pairing matches the
        // pre-existing model: an `await` here would widen the window in
        // which an interleaved request (HTTP/2 multiplexes chat calls on
        // one connection) overwrites this slot before our response lands.
        let method = req.method().as_str().to_string();
        let path = req.uri().path().to_string();
        // `is_chat` gates the gen_ai span to the provider's generation
        // endpoint (so telemetry/bootstrap/etc. calls don't become "chat"
        // spans). The span carries the wire-observed USAGE (tokens/model);
        // the *conversation* now comes from the transcript synthesizer
        // (see `transcripts::synth`), so the proxy no longer buffers or
        // parses the request body — it streams through untouched.
        let is_chat = provider.is_chat_request(&method, &path);
        self.in_flight = Some(InFlightCall {
            start: SystemTime::now(),
            host,
            method,
            path,
            auth_stub: extract_inbound_stub(&req),
            is_chat,
        });

        provider
            .handle_request(req, &self.server, &mut self.pending)
            .await
    }

    async fn handle_response(&mut self, _ctx: &HttpContext, res: Response<Body>) -> Response<Body> {
        // Dispatch first so the gen_ai span sees the *final* status
        // code (after any provider-side response rewriting). The body
        // is then wrapped to tap SSE usage events as they stream past
        // to the guest; the span fires when the wrapped body ends.
        let res = self.dispatch_response(res).await;
        self.wrap_for_telemetry(res)
    }
}

impl VaultHandler {
    /// Run the response through the provider that claimed a pending
    /// flow on the request side. No-op when no provider needs response-
    /// side rewriting (the bearer/api-key swap path doesn't set
    /// `pending`).
    async fn dispatch_response(&mut self, res: Response<Body>) -> Response<Body> {
        let Some(provider_id) = self.pending.as_ref().map(|p| p.provider_id) else {
            return res;
        };
        match self.server.provider_by_id(provider_id) {
            Some(p) => {
                Arc::clone(p)
                    .handle_response(res, &self.server, &mut self.pending)
                    .await
            }
            None => {
                // Provider disappeared (shouldn't happen — registry is
                // fixed for the server's lifetime). Drop the pending
                // flow so it doesn't poison the next response and pass
                // the body through.
                self.pending = None;
                res
            }
        }
    }

    /// Wrap the response body in a [`TappedBody`] so SSE usage events
    /// flowing to the guest are also parsed into a [`GenAiCallSpan`].
    /// The span fires when the wrapped body ends (natural completion
    /// or consumer drop). Returns the response unchanged when:
    ///  - the request wasn't the provider's chat/generation endpoint
    ///    (`is_chat`) — the proxy sees many non-generation calls to the
    ///    same host and emitting a `chat` gen_ai span for those mislabels
    ///    them as LLM generations with empty messages/usage; or
    ///  - the captured auth stub doesn't resolve to a sandbox (legitimate
    ///    `--with`'d real key, missing header, lease already dropped) —
    ///    we don't emit anonymous spans we can't attribute.
    fn wrap_for_telemetry(&mut self, res: Response<Body>) -> Response<Body> {
        let Some(call) = self.in_flight.take() else {
            return res;
        };
        if !call.is_chat {
            return res;
        }
        let Some(sandbox_id) = call.auth_stub.as_deref().and_then(|stub| {
            self.server
                .registry_lock()
                .sandbox_for_stub(stub)
                .map(str::to_owned)
        }) else {
            return res;
        };

        let status_code = res.status().as_u16();
        let ctx = self.server.context().clone();
        let (parts, body) = res.into_parts();
        let tapped = TappedBody::new(body, move |usage| {
            emit_genai_call_span(GenAiCallSpan {
                sandbox_id,
                session_id: ctx.session_id,
                mode: ctx.mode,
                workspace_id: ctx.workspace_id,
                start: call.start,
                end: SystemTime::now(),
                host: call.host,
                method: call.method,
                path: call.path,
                status_code,
                usage,
                // Conversation comes from the transcript synthesizer now;
                // the MITM span carries only the wire-observed usage.
                input_messages: None,
                system_instructions: None,
            });
        });
        Response::from_parts(parts, Body::from(BoxBody::new(tapped)))
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
    async fn snapshot_real_returns_rotated_tokens_until_lease_drops() {
        // The teardown persist path (VaultSession::drop) reads the
        // registry's real creds via snapshot_real and writes them back
        // to disk. Pin that it reflects an in-proxy rotation and goes
        // None once the lease is gone.
        let (server, dir) = fresh_server().await;
        let lease = server
            .lease("claude", "sbx-rot", sample_anthropic_real())
            .expect("lease");

        // Simulate the in-proxy refresh rotating the stored refresh token.
        server.inner_for_test().registry_lock().rotate_real_field(
            "sbx-rot",
            "/claudeAiOauth/refreshToken",
            "ROTATED_REFRESH".to_string(),
        );

        let snap = server.snapshot_real("sbx-rot").expect("snapshot");
        assert_eq!(
            snap.pointer("/claudeAiOauth/refreshToken")
                .and_then(|v| v.as_str()),
            Some("ROTATED_REFRESH"),
            "snapshot must reflect the rotation the teardown persist will write back"
        );
        // Unknown sandbox → None (nothing to persist).
        assert!(server.snapshot_real("sbx-missing").is_none());

        drop(lease);
        assert!(
            server.snapshot_real("sbx-rot").is_none(),
            "after the lease drops the registry entry is gone — teardown must persist before drop"
        );

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
