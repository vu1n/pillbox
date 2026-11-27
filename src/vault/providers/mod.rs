//! Vault providers — one per remote service (Anthropic, OpenAI/codex, …).
//!
//! Each provider owns its host predicate (`intercept`), the hosts its egress
//! fence allows (`hosts`), its OAuth token-endpoint matcher, and its OAuth codec.
//! Credential release happens in the libkrun MITM; the sidecar proxy only routes.

use hudsucker::{
    hyper::{Request, Response, StatusCode},
    Body,
};

pub(crate) mod anthropic;
pub(crate) mod codex;
pub(crate) mod codex_execution;
pub(crate) mod github;
pub(crate) mod openai;

use super::token_store::RefreshDecider;

/// Provider-owned OAuth wire contract. The broker owns coordination and transport;
/// the codec owns every credential-shape decision that differs by provider.
pub(crate) trait OAuthCodec: RefreshDecider {
    /// Build the upstream refresh request without exposing token-field knowledge to
    /// the generic broker.
    fn refresh_request(&self, refresh_token: &str) -> OAuthRefreshRequest;

    /// Apply a successful token response to the provider's persisted credential
    /// shape. A response may omit a rotated refresh token; codecs preserve the old
    /// value in that case.
    fn apply_refresh_response(
        &self,
        real: &mut serde_json::Value,
        response: &serde_json::Value,
        old_refresh: &str,
        now_ms: u64,
    ) -> anyhow::Result<()>;

    /// Real access-token expiry in unix milliseconds, used only as the host broker's
    /// JIT scheduling hint.
    fn expiry_ms(&self, real: &serde_json::Value) -> Option<u64>;

    /// Build the host-proxy credential file. It may need shape-valid access and
    /// refresh stubs even though guest token-endpoint requests are rejected.
    fn host_proxy_stub(
        &self,
        sandbox_id: &str,
        real: &serde_json::Value,
    ) -> Result<HostProxyOAuthStub, String>;

    /// Build the libkrun env-fork in place. Its release pairs are intentionally a
    /// separate operation: providers may refuse to release a guest refresh stub.
    fn libkrun_stub(&self, real: &mut serde_json::Value) -> LibkrunOAuthStub;
}

pub(crate) struct OAuthRefreshRequest {
    pub endpoint: &'static str,
    pub body: serde_json::Value,
}

pub(crate) struct HostProxyOAuthStub {
    pub credentials: serde_json::Value,
}

pub(crate) struct LibkrunOAuthStub {
    pub access_stub: Option<String>,
    pub releases: Vec<OAuthRelease>,
}

pub(crate) struct OAuthRelease {
    pub stub: String,
    pub real: String,
}

/// Provider contract: a stateless description of one upstream service.
///
/// `'static` because `Server` holds providers behind `Arc<dyn …>` for the
/// lifetime of the proxy task.
pub(crate) trait VaultProvider: Send + Sync + 'static {
    /// Stable id, also used by AgentSpec → provider lookup.
    fn id(&self) -> &'static str;

    /// Decide whether this provider handles the given host. The server
    /// asks each provider in order; first to match wins. Other hosts pass
    /// through without MITM (so unrelated traffic isn't exposed to our
    /// CA).
    fn intercept(&self, host: &str) -> bool;

    /// The concrete hosts this provider intercepts (API + any OAuth/platform
    /// endpoints). Used as the egress DNS-fence allowlist so the agent can reach
    /// its provider's API *and* refresh a token; default empty. Consumed only by
    /// the libkrun egress fence (`intercepted_hosts`).
    fn hosts(&self) -> &'static [&'static str] {
        &[]
    }

    /// Typed OAuth operations for this provider. API-key-only providers leave this
    /// absent; all OAuth dispatch flows through this one registry-owned seam.
    fn oauth_codec(&self) -> Option<&dyn OAuthCodec> {
        None
    }

    /// Whether this host/path is a provider token endpoint that a guest must
    /// never drive. Both proxy implementations use this provider-owned matcher
    /// to reject rotation before any credential substitution or network send.
    fn is_oauth_token_endpoint(&self, _host: &str, _path: &str) -> bool {
        false
    }
}

/// Build the full provider list. The list is fixed at compile-time —
/// adding a provider means editing this function plus the per-provider
/// module.
pub(crate) fn registry() -> Vec<Box<dyn VaultProvider>> {
    vec![
        Box::new(anthropic::AnthropicProvider),
        Box::new(codex::CodexProvider),
        Box::new(openai::OpenAiApiKeyProvider),
        Box::new(github::GithubProvider),
    ]
}

/// Every host the registered providers intercept — the egress DNS-fence allowlist
/// for a sandbox (each provider's API + OAuth/platform endpoints; token endpoints
/// stay intercepted so the local broker can reject guest rotation).
pub(crate) fn intercepted_hosts() -> Vec<&'static str> {
    registry()
        .iter()
        .flat_map(|p| p.hosts().iter().copied())
        .collect()
}

/// Look up a provider by id (an `AgentSpec.auth_id`).
pub(crate) fn provider_for(id: &str) -> Option<Box<dyn VaultProvider>> {
    registry().into_iter().find(|p| p.id() == id)
}

/// Provider-owned token-endpoint classification shared by the sidecar proxy and
/// the libkrun MITM.
pub(crate) fn is_oauth_token_endpoint(host: &str, path: &str) -> bool {
    registry()
        .iter()
        .any(|provider| provider.is_oauth_token_endpoint(host, path))
}

// ── Shared helpers ────────────────────────────────────────────────────
//
// Common request/response plumbing every provider needs. Lives here so
// providers stay focused on their host-specific JSON shape; the proxy-
// level mechanics (host extraction, error response shape, stub minting)
// have one source of truth.

/// Extract the host for an inbound request. Prefers the URI authority
/// (set on tunnelled HTTPS) and falls back to the `Host` header (set on
/// plain HTTP).
pub(crate) fn host_from_uri(req: &Request<Body>) -> Option<String> {
    req.uri().host().map(str::to_owned).or_else(|| {
        req.headers()
            .get("host")
            .and_then(|h| h.to_str().ok())
            .map(|s| s.split(':').next().unwrap_or(s).to_string())
    })
}

/// Refuse a guest-owned OAuth grant before it reaches the provider. The response
/// deliberately contains no request data: token bodies are credential material
/// and must not be reflected or logged.
pub(crate) fn oauth_rotation_forbidden() -> Response<Body> {
    Response::builder()
        .status(StatusCode::FORBIDDEN)
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"vault":"forbidden","detail":"OAuth rotation is broker-owned"}"#,
        ))
        .expect("build vault forbidden response")
}

/// Mint a stub token: `<prefix><sandbox_id_compact><uuid1><uuid2>`.
/// Local format validators (e.g. Claude Code's) accept only alphanumerics
/// in the suffix — UUID-simple is hex, sandbox id with dashes stripped is
/// alphanumeric.
pub(crate) fn mint_stub(prefix: &str, sandbox_id: &str) -> String {
    let id_compact = sandbox_id.replace('-', "");
    format!(
        "{}{}{}{}",
        prefix,
        id_compact,
        uuid::Uuid::now_v7().simple(),
        uuid::Uuid::now_v7().simple()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_for_returns_known_ids() {
        // Provider ids match AgentSpec ids so `provider_for` can do a
        // direct lookup. `claude` for the anthropic provider,
        // `codex` for the codex provider.
        assert!(provider_for("claude").is_some());
        assert!(provider_for("codex").is_some());
        assert!(provider_for("nope").is_none());
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::PathBuf;

    use crate::vault::server::{Server, ServerConfig};

    /// Spin up a fresh vault `Server` bound to an ephemeral local port,
    /// plus the tempdir the CA was written into (for cleanup).
    pub(crate) async fn fresh_server() -> (Server, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("pillbox-vault-itest-{}", uuid::Uuid::now_v7()));
        let server = Server::start(ServerConfig {
            bind: None,
            ca_dir: dir.clone(),
            egress: super::super::egress::EgressPolicy::default(),
        })
        .await
        .expect("server start");
        (server, dir)
    }

    /// JWT-shaped placeholder used for codex tests (3 dot-separated
    /// parts so `host_proxy_stub` accepts it).
    pub(crate) const FAKE_JWT: &str = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMifQ.signature_part_here";
}
