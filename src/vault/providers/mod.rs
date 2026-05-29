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
    hyper::{
        header::{HeaderValue, AUTHORIZATION},
        Request, Response, StatusCode,
    },
    Body, RequestOrResponse,
};

pub(crate) mod anthropic;
pub(crate) mod codex;
pub(crate) mod github;
pub(crate) mod openai;

/// Marker provider_id for entries minted by `Server::lease_api_key`.
/// These don't correspond to a [`VaultProvider`] — they're plain
/// stub→real string mappings used by the per-host providers'
/// `handle_request` to swap `x-api-key`/`Authorization: Bearer` headers.
///
/// The shape of `SandboxData::real` for these entries is:
/// `{"name": "<env var name>", "value": "<real key>"}`.
pub(crate) const API_KEY_PROVIDER_ID: &str = "api-key";

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

    /// Find the unique sandbox provisioned by `provider_id`. Used by
    /// the OAuth handler to attribute *authorization-code* grant
    /// responses (Claude Code's `/login`) — those requests carry a
    /// `code`, not a refresh_token, so [`Self::sandbox_for_stub`]
    /// can't resolve them. Returns `None` when zero or multiple
    /// sandboxes match; pillbox v0 has one OAuth sandbox per
    /// `pillbox run --vault` so ambiguity isn't a concern, but the
    /// conservative `None` signal lets a multi-tenant future extend
    /// without silently misattributing tokens.
    pub(crate) fn unique_sandbox_for_provider(&self, provider_id: &'static str) -> Option<String> {
        let mut found = None;
        for (id, data) in &self.by_sandbox {
            if data.provider_id != provider_id {
                continue;
            }
            if found.is_some() {
                return None;
            }
            found = Some(id.clone());
        }
        found
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

    /// Resolve `stub` to the real API-key value previously registered via
    /// `Server::lease_api_key`. Returns `None` if the stub isn't known, or
    /// if it's known but belongs to a non-API-key (e.g. OAuth) entry.
    pub(crate) fn api_key_real_for_stub(&self, stub: &str) -> Option<&str> {
        let sandbox_id = self.by_stub.get(stub)?;
        let data = self.by_sandbox.get(sandbox_id)?;
        if data.provider_id != API_KEY_PROVIDER_ID {
            return None;
        }
        data.real.get("value").and_then(|v| v.as_str())
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

    /// Whether `(method, path)` is this provider's chat/generation
    /// endpoint. Gates emitting a `gen_ai` *usage* span: the proxy sees
    /// many non-generation calls to the same host (telemetry batches,
    /// bootstrap, MCP registry, `count_tokens`), and a `chat` span for
    /// those would mislabel them as LLM generations with empty usage.
    /// Default `false` so a provider only produces gen_ai spans for
    /// endpoints it recognizes as generations. Conversation content is
    /// reconstructed from the transcript (see `transcripts::synth`), not
    /// captured here.
    fn is_chat_request(&self, _method: &str, _path: &str) -> bool {
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

/// Extract the inbound stub from any of the provider-recognized auth
/// header shapes. Sibling of [`swap_bearer_style`] / [`swap_raw_header`]
/// for callers that need the stub value *before* a swap happens (e.g.
/// the central handler's gen_ai span emission, which resolves
/// stub→sandbox_id without owning the swap).
///
/// Tries each family the registered providers accept, in this order:
/// `Authorization: Bearer …` (Anthropic/codex OAuth, OpenAI / modern
/// GitHub PATs), `Authorization: token …` (legacy GitHub), then the
/// raw `x-api-key` header (Anthropic API key, OpenAI alternate). New
/// auth families land here next to the swap helpers, not in callers.
pub(crate) fn extract_inbound_stub(req: &Request<Body>) -> Option<String> {
    if let Some(auth) = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
    {
        for prefix in ["Bearer ", "token "] {
            if let Some(stub) = auth.strip_prefix(prefix) {
                return Some(stub.to_string());
            }
        }
    }
    req.headers()
        .get("x-api-key")
        .and_then(|h| h.to_str().ok())
        .map(str::to_owned)
}

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

/// Guest-relative path the API-key providers (openai, github,
/// anthropic-api-key branch) would mount their creds file to. None of
/// them actually mount a file — the real key travels via `--with`-
/// injected env vars — so this is purely here to satisfy the trait's
/// `creds_path`. Pillbox never reads it for API-key entries.
pub(crate) const API_KEY_UNUSED_CREDS_PATH: &str = ".pillbox/api-key-no-file";

/// Error returned from an API-key provider's `provision`. These
/// providers are intentionally not OAuth-shaped — callers must use
/// [`super::server::Server::lease_api_key`] instead.
pub(crate) fn provision_is_api_key_only(provider_label: &str) -> Result<String, String> {
    Err(format!(
        "{provider_label} provider is API-key only: use Server::lease_api_key, \
         not the OAuth-shaped provision path"
    ))
}

/// Outcome of an API-key header swap.
pub(crate) enum ApiKeySwap {
    /// Header rewritten in place with the supplied scheme/value.
    Swapped(HeaderValue),
    /// Bearer/token-shaped, but the stub isn't in the registry.
    /// The provider should pass the request through unchanged — a
    /// `--with`'d real key (no vault meta) is a legitimate case.
    PassThrough,
    /// The header value isn't valid UTF-8 (or the rebuilt value isn't a
    /// valid HTTP header). Caller should 401 with the supplied detail.
    Unauthorized(&'static str),
}

/// Resolve a stub → real lookup for an `Authorization`-style header and
/// return the rebuilt header value with the supplied `scheme` ("Bearer"
/// for OpenAI / modern GitHub PATs, "token" for legacy GitHub clients).
///
/// `header_value` is the raw inbound header. If it doesn't start with
/// `scheme + " "` the caller must dispatch differently — this helper
/// is the swap step, not the parse step.
pub(crate) fn swap_bearer_style(
    header_value: &HeaderValue,
    scheme: &str,
    server: &ServerInner,
) -> ApiKeySwap {
    let Ok(auth_str) = header_value.to_str() else {
        return ApiKeySwap::Unauthorized("non-utf8 authorization");
    };
    let prefix = format!("{scheme} ");
    let Some(stub) = auth_str.strip_prefix(prefix.as_str()) else {
        return ApiKeySwap::PassThrough;
    };
    let real = {
        let registry = server.registry_lock();
        registry.api_key_real_for_stub(stub).map(str::to_owned)
    };
    let Some(real) = real else {
        return ApiKeySwap::PassThrough;
    };
    match HeaderValue::from_str(&format!("{scheme} {real}")) {
        Ok(hv) => ApiKeySwap::Swapped(hv),
        Err(_) => ApiKeySwap::Unauthorized("invalid real token"),
    }
}

/// Equivalent of `swap_bearer_style` for raw-value headers like
/// Anthropic's `x-api-key`, where the header value IS the stub (no
/// scheme prefix).
pub(crate) fn swap_raw_header(header_value: &HeaderValue, server: &ServerInner) -> ApiKeySwap {
    let Ok(stub) = header_value.to_str() else {
        return ApiKeySwap::Unauthorized("non-utf8 header");
    };
    let real = {
        let registry = server.registry_lock();
        registry.api_key_real_for_stub(stub).map(str::to_owned)
    };
    let Some(real) = real else {
        return ApiKeySwap::PassThrough;
    };
    match HeaderValue::from_str(&real) {
        Ok(hv) => ApiKeySwap::Swapped(hv),
        Err(_) => ApiKeySwap::Unauthorized("invalid real token"),
    }
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
        r.insert("sbx-1".into(), data("claude", vec!["stub-a", "stub-b"]));
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

        r.rotate_real_field("sbx-1", "/claudeAiOauth/accessToken", "new-access".into());

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

// ── Shared integration-test helpers ────────────────────────────────────
//
// Per-provider integration tests construct hyper `Request<Body>` /
// `Response<Body>` objects and feed them straight to the matching
// provider's `handle_request` / `handle_response`. The plumbing
// (booting a `Server`, draining bodies, asserting on stubbed/swapped
// headers) is identical across providers — collect it here so each
// provider's test module stays focused on its own header/body shape.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::PathBuf;

    use http_body_util::BodyExt;
    use hudsucker::{
        hyper::{Request, Response},
        Body, RequestOrResponse,
    };

    use crate::vault::server::{Server, ServerConfig};

    /// Spin up a fresh vault `Server` bound to an ephemeral local port,
    /// plus the tempdir the CA was written into (for cleanup).
    pub(crate) async fn fresh_server() -> (Server, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("pillbox-vault-itest-{}", uuid::Uuid::now_v7()));
        let server = Server::start(ServerConfig {
            bind: None,
            ca_dir: dir.clone(),
            context: super::super::server::RunContext::default(),
        })
        .await
        .expect("server start");
        (server, dir)
    }

    /// Drop the server (graceful proxy shutdown) and best-effort remove
    /// the CA tempdir. Tests pair this with `fresh_server`.
    pub(crate) fn cleanup(server: Server, dir: PathBuf) {
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Sample real anthropic creds blob used by lease() in tests.
    pub(crate) fn sample_anthropic_real() -> serde_json::Value {
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "REAL_ACCESS",
                "refreshToken": "REAL_REFRESH",
                "expiresAt": 1_700_000_000_u64,
                "subscriptionType": "pro"
            }
        })
    }

    /// JWT-shaped placeholder used for codex tests (3 dot-separated
    /// parts so `provision` accepts it).
    pub(crate) const FAKE_JWT: &str = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMifQ.signature_part_here";

    pub(crate) fn sample_codex_real() -> serde_json::Value {
        serde_json::json!({
            "auth_mode": "ChatGPT",
            "tokens": {
                "id_token": FAKE_JWT,
                "access_token": FAKE_JWT,
                "refresh_token": "rt_codex_real",
                "account_id": "acct_x"
            },
            "last_refresh": "2026-05-18T00:00:00Z",
            "agent_identity": serde_json::Value::Null
        })
    }

    /// Build a minimal `Request<Body>` aimed at `uri`. Caller adds the
    /// auth header(s) it cares about; this just sets the URI (so
    /// `host_from_uri` resolves) and method.
    pub(crate) fn build_request(method: &str, uri: &str, body: Body) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .body(body)
            .expect("build request")
    }

    /// Build a 200-OK `Response<Body>` with a JSON body. Used for the
    /// OAuth refresh-response swap tests.
    pub(crate) fn build_json_response(body_json: serde_json::Value) -> Response<Body> {
        let bytes = serde_json::to_vec(&body_json).expect("serialize json body");
        let len = bytes.len();
        Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .header("content-length", len)
            .body(Body::from(bytes))
            .expect("build response")
    }

    /// Collect a body's bytes into a `Vec<u8>`. Tests use this to read
    /// the rewritten body of the returned Request/Response.
    pub(crate) async fn body_bytes(body: Body) -> Vec<u8> {
        body.collect()
            .await
            .expect("collect body")
            .to_bytes()
            .to_vec()
    }

    /// Collect a body's bytes and parse as JSON. Convenience wrapper for
    /// the OAuth refresh body assertions.
    pub(crate) async fn body_json(body: Body) -> serde_json::Value {
        let bytes = body_bytes(body).await;
        serde_json::from_slice(&bytes).expect("body should be json")
    }

    /// Destructure a `RequestOrResponse` into the `Request` variant or
    /// panic with `label`. Tests use this to assert the provider chose
    /// pass-through / swap rather than 401.
    pub(crate) fn expect_request(rr: RequestOrResponse, label: &str) -> Request<Body> {
        match rr {
            RequestOrResponse::Request(req) => req,
            RequestOrResponse::Response(res) => {
                panic!(
                    "{label}: expected Request, got Response with status {}",
                    res.status()
                )
            }
        }
    }

    /// Destructure a `RequestOrResponse` into the `Response` variant or
    /// panic with `label`. Tests use this to assert the provider
    /// rejected an unknown stub at the proxy edge.
    pub(crate) fn expect_response(rr: RequestOrResponse, label: &str) -> Response<Body> {
        match rr {
            RequestOrResponse::Request(_) => panic!("{label}: expected Response, got Request"),
            RequestOrResponse::Response(res) => res,
        }
    }
}
