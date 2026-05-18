//! Vault providers — one per remote service (Anthropic, OpenAI/codex, …).
//!
//! The vault server holds N providers and dispatches by host. Each provider
//! owns:
//!  - host predicate (`intercept`)
//!  - credentials shape (`provision`: parse real creds → produce stubs +
//!    register them in the [`Registry`])
//!  - request/response rewriting (`handle_request` / `handle_response`)
//!  - on-disk credentials path inside the guest (`creds_path`)
//!
//! State that's the same for every provider — the stub → sandbox lookup
//! table, the per-sandbox real-creds blob — lives on a single shared
//! [`Registry`]. Providers don't define their own map type; they push and
//! pull strings.

use std::collections::HashMap;
use std::path::Path;

use async_trait::async_trait;
use hudsucker::{
    hyper::{Request, Response, StatusCode},
    Body, RequestOrResponse,
};

pub(crate) mod anthropic;
pub(crate) mod codex;

use super::server::ServerInner;

/// In-flight OAuth refresh the handler is mid-way through processing.
/// Set in `handle_request`, consumed in `handle_response` so the response-
/// side swap knows which sandbox to update.
#[derive(Clone, Debug)]
pub(crate) struct PendingFlow {
    pub provider_id: &'static str,
    pub sandbox_id: String,
}

/// Provider-agnostic state for one active sandbox.
#[derive(Clone, Debug)]
pub(crate) struct SandboxData {
    /// Which provider owns this entry. Tracked for diagnostics and
    /// future cross-provider sanity checks (e.g. asserting a response is
    /// being routed back to the provider that handled the request).
    /// Not yet read in production code paths.
    #[allow(dead_code)]
    pub provider_id: &'static str,
    /// Full real creds JSON, kept so providers can read whichever fields
    /// they need and write rotated values back in place.
    pub real: serde_json::Value,
    /// Stub tokens minted for this sandbox. Tracked so [`Registry::remove`]
    /// can clean up `by_stub` reverse-lookups without re-parsing JSON.
    pub stubs: Vec<String>,
}

/// Provider-agnostic vault map. The single source of truth for
/// stub→sandbox→real mappings, shared across all providers.
pub(crate) struct Registry {
    by_sandbox: HashMap<String, SandboxData>,
    /// stub token (any provider) → sandbox_id
    by_stub: HashMap<String, String>,
}

impl Registry {
    pub(crate) fn new() -> Self {
        Self {
            by_sandbox: HashMap::new(),
            by_stub: HashMap::new(),
        }
    }

    pub(crate) fn insert(&mut self, sandbox_id: String, data: SandboxData) {
        for stub in &data.stubs {
            self.by_stub.insert(stub.clone(), sandbox_id.clone());
        }
        self.by_sandbox.insert(sandbox_id, data);
    }

    pub(crate) fn remove(&mut self, sandbox_id: &str) -> Option<SandboxData> {
        let data = self.by_sandbox.remove(sandbox_id)?;
        for stub in &data.stubs {
            self.by_stub.remove(stub);
        }
        Some(data)
    }

    pub(crate) fn sandbox_for_stub(&self, stub: &str) -> Option<&str> {
        self.by_stub.get(stub).map(String::as_str)
    }

    pub(crate) fn real(&self, sandbox_id: &str) -> Option<&serde_json::Value> {
        self.by_sandbox.get(sandbox_id).map(|d| &d.real)
    }

    /// Walk into the stored real JSON via an RFC 6901 JSON pointer (e.g.
    /// `/claudeAiOauth/accessToken`) and overwrite the leaf with a
    /// string. Used by provider response-handlers to persist rotated
    /// tokens. Quietly no-ops if the path is missing — rotation responses
    /// don't always include both fields.
    pub(crate) fn rotate_real_field(
        &mut self,
        sandbox_id: &str,
        json_pointer: &str,
        new_value: String,
    ) {
        let Some(data) = self.by_sandbox.get_mut(sandbox_id) else {
            return;
        };
        if let Some(leaf) = data.real.pointer_mut(json_pointer) {
            *leaf = serde_json::Value::String(new_value);
        }
    }

    pub(crate) fn stubs_for(&self, sandbox_id: &str) -> Option<&[String]> {
        self.by_sandbox.get(sandbox_id).map(|d| d.stubs.as_slice())
    }
}

/// Provider contract. A provider is a stateless dispatcher: it takes the
/// shared [`Registry`] (via `&ServerInner`) and the in-flight request /
/// response, and returns the swapped version.
///
/// `'static` because `Server` holds providers behind `Arc<dyn …>` for the
/// lifetime of the proxy task.
#[async_trait]
pub(crate) trait VaultProvider: Send + Sync + 'static {
    /// Stable id, also used by AgentSpec → provider lookup.
    fn id(&self) -> &'static str;

    /// Decide whether this provider handles the given host. The server
    /// asks each provider in order; first to match wins. Other hosts pass
    /// through without MITM (so unrelated traffic isn't exposed to our
    /// CA).
    fn intercept(&self, host: &str) -> bool;

    /// Path (relative to guest HOME) where the agent expects to find its
    /// credentials file. Pillbox mounts the stub at this location.
    fn creds_path(&self) -> &'static Path;

    /// Validate the loaded real creds, register stub tokens into
    /// `registry`, and return the stub credentials file body that should
    /// be written into the guest at [`Self::creds_path`].
    fn provision(
        &self,
        sandbox_id: &str,
        real: &serde_json::Value,
        registry: &mut Registry,
    ) -> Result<String, String>;

    /// Inspect/rewrite an outbound request. Sets `*pending` if a follow-
    /// up response-side swap is required.
    async fn handle_request(
        &self,
        req: Request<Body>,
        server: &ServerInner,
        pending: &mut Option<PendingFlow>,
    ) -> RequestOrResponse;

    /// Inspect/rewrite an inbound response. Only invoked when `pending`
    /// was set by this provider's `handle_request`. Providers should
    /// clear `pending` themselves before returning.
    async fn handle_response(
        &self,
        res: Response<Body>,
        server: &ServerInner,
        pending: &mut Option<PendingFlow>,
    ) -> Response<Body>;
}

/// Build the full provider list. The list is fixed at compile-time —
/// adding a provider means editing this function plus the per-provider
/// module.
pub(crate) fn registry() -> Vec<Box<dyn VaultProvider>> {
    vec![
        Box::new(anthropic::AnthropicProvider),
        Box::new(codex::CodexProvider),
    ]
}

/// Look up a provider by id (for `VaultSession::start`, which is called
/// with an `AgentSpec.id` and needs the matching provider).
pub(crate) fn provider_for(id: &str) -> Option<Box<dyn VaultProvider>> {
    registry().into_iter().find(|p| p.id() == id)
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

/// Build a 401 response with a small JSON body — used by providers to
/// reject malformed or unrecognised stub credentials at the proxy edge
/// rather than letting them reach the upstream.
pub(crate) fn unauthorized(detail: &str) -> Response<Body> {
    let body = format!(r#"{{"vault":"unauthorized","detail":"{detail}"}}"#);
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .expect("build vault unauthorized response")
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

    fn data(provider: &'static str, stubs: Vec<&str>) -> SandboxData {
        SandboxData {
            provider_id: provider,
            real: serde_json::json!({"k": "v"}),
            stubs: stubs.into_iter().map(String::from).collect(),
        }
    }

    #[test]
    fn insert_then_remove_clears_stub_lookups() {
        let mut r = Registry::new();
        r.insert(
            "sbx-1".into(),
            data("claude", vec!["stub-a", "stub-b"]),
        );
        assert_eq!(r.sandbox_for_stub("stub-a"), Some("sbx-1"));
        assert_eq!(r.sandbox_for_stub("stub-b"), Some("sbx-1"));

        r.remove("sbx-1");
        assert!(r.sandbox_for_stub("stub-a").is_none());
        assert!(r.sandbox_for_stub("stub-b").is_none());
        assert!(r.real("sbx-1").is_none());
    }

    #[test]
    fn rotate_real_field_walks_nested_path() {
        let mut r = Registry::new();
        let mut d = data("claude", vec!["s"]);
        d.real = serde_json::json!({
            "claudeAiOauth": {"accessToken": "old", "refreshToken": "old-r"}
        });
        r.insert("sbx-1".into(), d);

        r.rotate_real_field(
            "sbx-1",
            "/claudeAiOauth/accessToken",
            "new-access".into(),
        );

        let v = r.real("sbx-1").unwrap();
        assert_eq!(
            v.pointer("/claudeAiOauth/accessToken")
                .and_then(|v| v.as_str()),
            Some("new-access")
        );
        // Other fields untouched.
        assert_eq!(
            v.pointer("/claudeAiOauth/refreshToken")
                .and_then(|v| v.as_str()),
            Some("old-r")
        );
    }

    #[test]
    fn rotate_real_field_missing_path_is_noop() {
        let mut r = Registry::new();
        r.insert("sbx-1".into(), data("claude", vec!["s"]));
        // Should not panic.
        r.rotate_real_field("sbx-1", "/nope/missing", "x".into());
        r.rotate_real_field("missing-sandbox", "/k", "x".into());
    }

    #[test]
    fn provider_for_returns_known_ids() {
        // Provider ids match AgentSpec ids so VaultSession can do a
        // direct lookup. `claude` for the anthropic provider,
        // `codex` for the codex provider.
        assert!(provider_for("claude").is_some());
        assert!(provider_for("codex").is_some());
        assert!(provider_for("nope").is_none());
    }
}
