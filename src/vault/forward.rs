//! SSRF-guarded forward connector for the docker vault broker.
//!
//! hudsucker's `with_rustls_connector` dials the real upstream through a default
//! `HttpConnector` whose resolver returns every A/AAAA record — including a name
//! that (host-side) resolves to cloud metadata (169.254.169.254), the LAN,
//! `10.0.0.0/8`, or loopback. The docker backend has no network fence (unlike
//! libkrun, whose smoltcp DNS fence pins names to the gateway), so this forward
//! leg is the only thing between a rebound or allowlisted-internal name and an
//! SSRF. We rebuild the same rustls connector around a resolver that drops
//! denied IPs ([`crate::vault::is_denied_egress_ip`]) before the dial, so the
//! filtered set is exactly what gets connected — no TOCTOU.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use hudsucker::hyper_util::client::legacy::connect::dns::{GaiResolver, Name};
use hudsucker::hyper_util::client::legacy::connect::HttpConnector;
use hudsucker::rustls::crypto::aws_lc_rs;
use hudsucker::rustls::{ClientConfig, RootCertStore};
use hudsucker::tokio_tungstenite::Connector;
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use tower_service::Service;

/// A DNS resolver that wraps the system resolver and drops any address in a
/// private/loopback/link-local/CGNAT/ULA range. A host that resolves *only* to
/// internal addresses surfaces as a connect error rather than a silent dial.
///
/// This fires only for *hostname* targets — `HttpConnector` parses a literal-IP
/// authority before resolving, skipping the resolver. That's sufficient here
/// because this connector serves only `EgressDecision::Swap` (see
/// [`super::server`]), which requires the host to match a provider *domain*; a
/// literal-IP authority gets `AllowPassthrough`/`Deny`, never `Swap`. If a future
/// change ever lets an IP-literal reach `Swap`, this guard would be bypassed —
/// filter the IP at that decision point too.
#[derive(Clone)]
pub(super) struct SsrfGuardResolver {
    inner: GaiResolver,
}

impl Service<Name> for SsrfGuardResolver {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<Self::Response>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, name: Name) -> Self::Future {
        let fut = self.inner.call(name);
        Box::pin(async move {
            let kept: Vec<SocketAddr> = fut
                .await?
                .filter(|addr| !crate::vault::is_denied_egress_ip(addr.ip()))
                .collect();
            if kept.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "vault SSRF guard: host resolves only to private/internal addresses",
                ));
            }
            Ok(kept.into_iter())
        })
    }
}

/// The forward connector hudsucker dials through: an SSRF-filtering
/// `HttpConnector` re-wrapped with rustls TLS, plus the matching WebSocket
/// connector (so vault WS upgrades terminate TLS the same way). Mirrors
/// hudsucker's `with_rustls_connector` (webpki roots, aws-lc-rs, HTTP/1.1-only —
/// no h2 ALPN, which breaks WS upgrade) but swaps in the guarding resolver.
pub(super) fn guarded_connector() -> (HttpsConnector<HttpConnector<SsrfGuardResolver>>, Connector) {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let tls_config = ClientConfig::builder_with_provider(Arc::new(aws_lc_rs::default_provider()))
        .with_safe_default_protocol_versions()
        .expect("aws-lc-rs supports the default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();

    let mut http = HttpConnector::new_with_resolver(SsrfGuardResolver {
        inner: GaiResolver::new(),
    });
    // HttpsConnector hands it https URIs; without this the inner HttpConnector
    // rejects any non-http scheme (matches hyper-rustls's own default build()).
    http.enforce_http(false);

    let https = HttpsConnectorBuilder::new()
        .with_tls_config(tls_config.clone())
        .https_or_http()
        .enable_http1()
        .wrap_connector(http);

    (https, Connector::Rustls(Arc::new(tls_config)))
}
