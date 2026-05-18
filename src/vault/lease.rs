//! Per-sandbox vault lease — RAII handle that owns the stub→real mapping.
//!
//! A [`SandboxLease`] is created by [`crate::vault::Server::lease`] and
//! held by the caller (Pillbox's orchestrator) for the sandbox's lifetime.
//! When the lease is dropped the mapping is removed from the server's
//! map, so future requests carrying the stub tokens fail.
//!
//! Stubs encode the sandbox id inside the token tail (after the
//! `sk-ant-oat01-` / `sk-ant-ort01-` prefix), which gives the server a
//! fast lookup and means we don't need to bind by source-port or
//! TLS-session-id. The full token is opaque to anything upstream of the
//! proxy.

use std::sync::{Arc, Weak};

use super::secrets::AnthropicCreds;
use super::server::ServerInner;

// Stub tokens mimic Anthropic's `sk-ant-oat01-` / `sk-ant-ort01-` prefixes
// so Claude Code's local format validation accepts them. The suffix is
// pure alphanumeric (no dashes/underscores) for the same reason. Anthropic
// doesn't see these — by the time a request hits the wire the proxy has
// swapped them for the real values.
pub(crate) const STUB_ACCESS_PREFIX: &str = "sk-ant-oat01-";
pub(crate) const STUB_REFRESH_PREFIX: &str = "sk-ant-ort01-";

/// Active vault entry for one sandbox.
pub(crate) struct SandboxEntry {
    pub real: AnthropicCreds,
    pub stub_refresh: String,
    pub stub_access: String,
}

impl SandboxEntry {
    pub(crate) fn new(sandbox_id: &str, real: AnthropicCreds) -> Self {
        // Pack sandbox_id (UUID v7 without dashes = 32 hex chars) plus
        // two random suffixes into a single alphanumeric blob with no
        // dashes or underscores. Real Anthropic OAuth tokens are mixed-
        // case alphanumeric, ~95 chars after the `sk-ant-oat01-` prefix;
        // claude's local validator appears to reject non-alphanumeric
        // characters in that suffix.
        let id_compact = sandbox_id.replace('-', "");
        let stub_refresh = format!(
            "{}{}{}{}",
            STUB_REFRESH_PREFIX,
            id_compact,
            random_suffix(),
            random_suffix()
        );
        let stub_access = format!(
            "{}{}{}{}",
            STUB_ACCESS_PREFIX,
            id_compact,
            random_suffix(),
            random_suffix()
        );
        Self {
            real,
            stub_refresh,
            stub_access,
        }
    }
}

/// RAII handle for a per-sandbox vault entry.
///
/// Dropping this removes the stub→real mapping. Held by the caller for the
/// duration of the sandbox lifetime. Use [`Self::stub_credentials_json`] to
/// produce the JSON payload that gets written into the guest's
/// `~/.claude/.credentials.json`.
pub struct SandboxLease {
    sandbox_id: String,
    stub_refresh: String,
    stub_access: String,
    stub_json: String,
    server: Weak<ServerInner>,
}

impl SandboxLease {
    pub(crate) fn new(
        sandbox_id: String,
        stub_refresh: String,
        stub_access: String,
        stub_json: String,
        server: Arc<ServerInner>,
    ) -> Self {
        Self {
            sandbox_id,
            stub_refresh,
            stub_access,
            stub_json,
            server: Arc::downgrade(&server),
        }
    }

    /// Stub OAuth refresh token visible to the guest.
    #[allow(dead_code)]
    pub(crate) fn stub_refresh(&self) -> &str {
        &self.stub_refresh
    }

    /// Stub access token visible to the guest.
    #[allow(dead_code)]
    pub(crate) fn stub_access(&self) -> &str {
        &self.stub_access
    }

    /// Serialized stub `.credentials.json` body. Write this into the guest's
    /// `~/.claude/.credentials.json` in place of the real file.
    pub(crate) fn stub_credentials_json(&self) -> &str {
        &self.stub_json
    }
}

impl Drop for SandboxLease {
    fn drop(&mut self) {
        if let Some(server) = self.server.upgrade() {
            server.drop_sandbox(&self.sandbox_id);
        }
    }
}

/// Build the stub `.credentials.json` payload by cloning the real JSON and
/// swapping just the token fields. Preserves unknown fields verbatim.
pub(crate) fn build_stub_json(
    real: &AnthropicCreds,
    stub_access: &str,
    stub_refresh: &str,
) -> Result<String, String> {
    let mut value = real.raw().clone();
    let oauth = value
        .get_mut("claudeAiOauth")
        .and_then(|v| v.as_object_mut())
        .ok_or_else(|| "claudeAiOauth block missing".to_string())?;
    oauth.insert(
        "accessToken".to_string(),
        serde_json::Value::String(stub_access.to_string()),
    );
    oauth.insert(
        "refreshToken".to_string(),
        serde_json::Value::String(stub_refresh.to_string()),
    );
    serde_json::to_string_pretty(&value).map_err(|error| format!("serialize stub creds: {error}"))
}

fn random_suffix() -> String {
    // uuid v7 is already a dep and gives us a high-entropy hex suffix
    // without pulling rand in directly. The timestamp prefix is irrelevant
    // here — we just want something unique enough to make the stub
    // unguessable.
    uuid::Uuid::now_v7().simple().to_string()
}

#[cfg(test)]
mod tests {
    use super::{build_stub_json, random_suffix, SandboxEntry, STUB_ACCESS_PREFIX, STUB_REFRESH_PREFIX};
    use crate::vault::secrets::AnthropicCreds;

    fn sample_creds() -> AnthropicCreds {
        AnthropicCreds::from_bytes(
            br#"{
                "claudeAiOauth": {
                    "accessToken": "REAL_ACCESS",
                    "refreshToken": "REAL_REFRESH",
                    "expiresAt": 1700000000,
                    "subscriptionType": "pro"
                }
            }"#,
        )
        .expect("parse")
    }

    #[test]
    fn entry_has_distinct_stubs_with_prefix_and_compact_sandbox_id() {
        // SandboxEntry strips dashes from the sandbox_id so the stub stays
        // pure alphanumeric (claude's validator rejects non-alphanumeric
        // tail chars). The bare token alone identifies the sandbox.
        let entry = SandboxEntry::new("sbx-abc", sample_creds());
        assert!(entry.stub_refresh.starts_with(STUB_REFRESH_PREFIX));
        assert!(entry.stub_access.starts_with(STUB_ACCESS_PREFIX));
        // Dashes stripped, so the marker is "sbxabc"
        assert!(entry.stub_refresh.contains("sbxabc"));
        assert!(entry.stub_access.contains("sbxabc"));
        // Tails after the prefix are pure hex (no dashes/underscores)
        let tail_a = entry.stub_access.strip_prefix(STUB_ACCESS_PREFIX).unwrap();
        let tail_r = entry.stub_refresh.strip_prefix(STUB_REFRESH_PREFIX).unwrap();
        assert!(tail_a.chars().all(|c| c.is_ascii_alphanumeric()), "{tail_a}");
        assert!(tail_r.chars().all(|c| c.is_ascii_alphanumeric()), "{tail_r}");
        assert_ne!(entry.stub_refresh, entry.stub_access);
    }

    #[test]
    fn stub_json_swaps_tokens_preserves_unknown_fields() {
        let creds = sample_creds();
        let stub = build_stub_json(&creds, "STUB_ACCESS", "STUB_REFRESH").expect("build");
        let parsed: serde_json::Value = serde_json::from_str(&stub).expect("parse stub");
        let oauth = parsed.get("claudeAiOauth").expect("oauth");
        assert_eq!(
            oauth.get("accessToken").and_then(|v| v.as_str()),
            Some("STUB_ACCESS")
        );
        assert_eq!(
            oauth.get("refreshToken").and_then(|v| v.as_str()),
            Some("STUB_REFRESH")
        );
        assert_eq!(
            oauth.get("expiresAt").and_then(|v| v.as_i64()),
            Some(1700000000)
        );
        assert_eq!(
            oauth.get("subscriptionType").and_then(|v| v.as_str()),
            Some("pro")
        );
        // Sanity: real tokens never appear in the stub
        assert!(!stub.contains("REAL_ACCESS"));
        assert!(!stub.contains("REAL_REFRESH"));
    }

    #[test]
    fn random_suffix_is_unique_hex() {
        let a = random_suffix();
        let b = random_suffix();
        assert_eq!(a.len(), 32); // uuid simple = 32 hex chars
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
