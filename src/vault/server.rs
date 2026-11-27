//! HTTPS MITM proxy behind `pillbox sidecar`: the host-side egress broker
//! for traffic that does not run inside a libkrun microVM.
//!
//! Behaviour:
//!  - For each request, ask each provider whether it claims the host.
//!    Claimed hosts are MITM'd; a denied host gets a local 403; other hosts
//!    pass through (no MITM), so unrelated traffic is not exposed to our CA.
//!  - On a claimed host, a provider OAuth token endpoint is refused locally:
//!    the host broker is the sole rotation authority.
//!  - Nothing else is rewritten. The proxy holds no credentials and leases no
//!    stubs; credential release lives in the libkrun MITM.

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use hudsucker::{
    certificate_authority::RcgenAuthority,
    hyper::{Request, Response, StatusCode},
    rustls::crypto::aws_lc_rs,
    Body, HttpContext, HttpHandler, Proxy, RequestOrResponse,
};
use tokio::net::TcpListener;

use super::{
    ca::Ca,
    egress::{EgressDecision, EgressPolicy},
    providers::{self, host_from_uri, oauth_rotation_forbidden, VaultProvider},
};

/// Configuration for [`Server::start`].
#[derive(Debug)]
pub struct ServerConfig {
    /// Optional bind address. Defaults to `127.0.0.1:0` (pick a free port).
    pub bind: Option<SocketAddr>,
    /// Directory the CA cert + key are persisted in.
    pub ca_dir: PathBuf,
    /// Egress policy — default-deny + allowlist. Default is permissive
    /// (unmatched hosts pass through). See [`EgressPolicy`].
    pub egress: EgressPolicy,
}

/// State shared by every handler clone.
struct ServerInner {
    listen_addr: SocketAddr,
    ca: Ca,
    providers: Vec<Arc<dyn VaultProvider>>,
    /// See [`ServerConfig::egress`].
    egress: EgressPolicy,
    /// Sender side of a shutdown signal. Dropping this triggers proxy
    /// graceful shutdown via the receiver held by the spawned task.
    shutdown_tx: tokio::sync::watch::Sender<bool>,
}

impl ServerInner {
    fn provider_for_host(&self, host: &str) -> Option<&Arc<dyn VaultProvider>> {
        self.providers.iter().find(|p| p.intercept(host))
    }

    /// Broker decision for an outbound `host`: intercept (a provider claims
    /// it), pass through (allowlisted, or permissive mode), or deny
    /// (default-deny + unmatched). The single chokepoint `should_intercept` +
    /// `handle_request` both consult.
    fn egress_decision(&self, host: &str) -> EgressDecision {
        self.egress
            .decide(host, self.provider_for_host(host).is_some())
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
            providers,
            egress: config.egress,
            shutdown_tx,
        });

        let handler = VaultHandler {
            server: Arc::clone(&inner),
        };

        let shutdown_signal = async move {
            // Watch for the sender being signalled OR dropped.
            let _ = shutdown_rx.changed().await;
        };

        // SSRF-guarded forward connector instead of hudsucker's stock
        // `with_rustls_connector`: a sidecar client has no network fence, so
        // the dial is the only block on a rebound/internal upstream. See
        // super::forward.
        let (https, ws_connector) = super::forward::guarded_connector();
        let proxy = Proxy::builder()
            .with_listener(listener)
            .with_ca(authority)
            .with_http_connector(https)
            .with_websocket_connector(ws_connector)
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
}

/// A 403 returned to the guest when the broker denies egress (default-deny:
/// the host has no credential provider and isn't on the allowlist). The request
/// never reaches the network — this is the exfiltration guard.
fn denied_response(host: &str) -> RequestOrResponse {
    let body = format!(
        "pillbox vault: egress to `{host}` denied — no credential provider for \
         this host and it is not on the egress allowlist (default-deny).\n"
    );
    let res = Response::builder()
        .status(StatusCode::FORBIDDEN)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Body::from(body))
        .expect("static 403 response always builds");
    RequestOrResponse::Response(res)
}

/// Route a request to a provider-claimed host.
fn route_claimed(
    provider: &dyn VaultProvider,
    host: &str,
    mut req: Request<Body>,
) -> RequestOrResponse {
    // A guest never drives a provider token endpoint: rotation is broker-owned.
    if provider.is_oauth_token_endpoint(host, req.uri().path()) {
        return oauth_rotation_forbidden().into();
    }
    // Refuse permessage-deflate on any WebSocket we intercept. The
    // MITM terminates and *re-frames* the WS (separate tungstenite
    // handshakes client↔proxy and proxy↔upstream), and this relay
    // path has no deflate support — so if we forwarded the client's
    // `Sec-WebSocket-Extensions: permessage-deflate` upstream, the
    // server would send RSV1-compressed frames the relay can't
    // decode ("Reserved bits are non-zero") and the socket dies.
    // Dropping the header negotiates an uncompressed WS on both hops.
    // Harmless on non-upgrade requests (header simply absent).
    req.headers_mut().remove("sec-websocket-extensions");
    req.into()
}

impl HttpHandler for VaultHandler {
    async fn should_intercept(&mut self, _ctx: &HttpContext, req: &Request<Body>) -> bool {
        // CONNECT requests use authority form: host:port in the URI.
        let host = req.uri().host().map(str::to_owned).unwrap_or_default();
        // Intercept (MITM) when a provider claims the host OR to deny — both
        // require terminating the connection. Pass-through hosts are tunneled.
        matches!(
            self.server.egress_decision(&host),
            EgressDecision::Swap | EgressDecision::Deny
        )
    }

    async fn handle_request(
        &mut self,
        _ctx: &HttpContext,
        req: Request<Body>,
    ) -> RequestOrResponse {
        let Some(host) = host_from_uri(&req) else {
            return req.into();
        };
        match self.server.egress_decision(&host) {
            EgressDecision::Deny => denied_response(&host),
            EgressDecision::AllowPassthrough => req.into(),
            EgressDecision::Swap => match self.server.provider_for_host(&host) {
                Some(provider) => route_claimed(provider.as_ref(), &host, req),
                // `Swap` implies a provider claims the host; both read the same
                // immutable provider set.
                None => req.into(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use http_body_util::BodyExt;
    use hudsucker::{hyper::Request, Body, RequestOrResponse};

    use super::{route_claimed, EgressPolicy, Server, ServerConfig};
    use crate::vault::providers::{anthropic, codex, test_support::fresh_server};

    /// The broker's default-deny returns a 403 (not a forward) for a host with no
    /// credential provider — exercised through the real proxy `handle_request`
    /// path via an HTTP request routed through the proxy. The host (`.invalid`,
    /// unresolvable) is never dialed: the 403 short-circuits before any forward,
    /// which is the point — the request can't leave. (HTTP, not HTTPS, so the
    /// deny is observed directly at `handle_request` without the CONNECT/TLS
    /// dance; HTTPS denied hosts are also blocked but surface as a failed tunnel
    /// rather than a clean 403.)
    #[tokio::test]
    async fn default_deny_returns_403_for_unmatched_host() {
        let dir = std::env::temp_dir().join(format!("pillbox-vault-deny-{}", uuid::Uuid::now_v7()));
        let server = Server::start(ServerConfig {
            bind: None,
            ca_dir: dir.clone(),
            egress: EgressPolicy {
                default_deny: true,
                allow_hosts: vec![],
            },
        })
        .await
        .expect("server start");

        let proxy_url = format!("http://{}", server.listen_addr());
        let client = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(&proxy_url).unwrap())
            .build()
            .unwrap();

        let resp = client
            .get("http://blocked.invalid/")
            .send()
            .await
            .expect("proxy answers (no upstream dial)");
        assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);

        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
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

    fn token_request(uri: &str, body: &'static str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap()
    }

    async fn refused_body(rr: RequestOrResponse) -> String {
        let RequestOrResponse::Response(res) = rr else {
            panic!("a guest token-endpoint request must be refused locally");
        };
        assert_eq!(res.status(), 403);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn anthropic_oauth_refresh_is_refused_without_reflecting_the_token() {
        let req = token_request(
            "https://console.anthropic.com/oauth/token",
            r#"{"grant_type":"refresh_token","refresh_token":"sk-ant-ort01-guest"}"#,
        );
        let body = refused_body(route_claimed(
            &anthropic::AnthropicProvider,
            "console.anthropic.com",
            req,
        ))
        .await;
        assert!(body.contains("OAuth rotation is broker-owned"));
        assert!(!body.contains("sk-ant-ort01-guest"));
    }

    #[tokio::test]
    async fn anthropic_authorization_code_grant_is_also_refused() {
        let req = token_request(
            "https://platform.claude.com/v1/oauth/token",
            "not even json",
        );
        refused_body(route_claimed(
            &anthropic::AnthropicProvider,
            "platform.claude.com",
            req,
        ))
        .await;
    }

    #[tokio::test]
    async fn codex_oauth_refresh_is_refused_without_reflecting_the_token() {
        let req = token_request(
            "https://auth.openai.com/oauth/token",
            r#"{"grant_type":"refresh_token","refresh_token":"pb-codex-ort-guest"}"#,
        );
        let body = refused_body(route_claimed(&codex::CodexProvider, "auth.openai.com", req)).await;
        assert!(!body.contains("pb-codex-ort-guest"));
    }

    #[test]
    fn a_claimed_api_request_is_forwarded_untouched_but_websocket_deflate() {
        let req = Request::builder()
            .method("POST")
            .uri("https://api.anthropic.com/v1/messages")
            .header("authorization", "Bearer sk-ant-oat01-guest")
            .header("sec-websocket-extensions", "permessage-deflate")
            .body(Body::empty())
            .unwrap();
        let RequestOrResponse::Request(out) =
            route_claimed(&anthropic::AnthropicProvider, "api.anthropic.com", req)
        else {
            panic!("an API request must be forwarded");
        };
        assert_eq!(out.headers()["authorization"], "Bearer sk-ant-oat01-guest");
        assert!(out.headers().get("sec-websocket-extensions").is_none());
    }
}
