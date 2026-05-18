//! Real-credential loader for the vault.
//!
//! The vault is path-agnostic: callers (Pillbox's orchestrator) decide where
//! the real `.credentials.json` lives on the host. The vault loads it once
//! per sandbox into memory and never writes it back out — guests only see
//! the stubbed version produced by [`crate::vault::lease`].

use std::{fs, path::Path};

use serde::Deserialize;

/// Real Anthropic OAuth credentials loaded from disk.
///
/// Today only the OAuth flow is supported (Claude Code 2.x default). The
/// outer JSON object is opaque so that fields we don't know about (e.g.
/// `subscriptionType`, future metadata) survive a round-trip through the
/// vault unchanged.
#[derive(Debug, Clone)]
pub struct AnthropicCreds {
    /// Full original JSON, kept so unknown fields are preserved in the stub.
    pub(crate) raw: serde_json::Value,
    /// The real refresh token from `claudeAiOauth.refreshToken`.
    pub(crate) real_refresh: String,
    /// The real access token from `claudeAiOauth.accessToken`.
    pub(crate) real_access: String,
}

#[derive(Debug, Deserialize)]
struct OauthBlock {
    #[serde(rename = "accessToken")]
    access_token: String,
    #[serde(rename = "refreshToken")]
    refresh_token: String,
}

impl AnthropicCreds {
    /// Load from a file. The expected schema is the one Claude Code 2.x
    /// writes to `~/.claude/.credentials.json`:
    /// ```json
    /// { "claudeAiOauth": { "accessToken": "...", "refreshToken": "...", ... } }
    /// ```
    pub fn load_from_file(path: &Path) -> Result<Self, String> {
        let bytes = fs::read(path)
            .map_err(|error| format!("read anthropic creds {}: {error}", path.display()))?;
        Self::from_bytes(&bytes)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let raw: serde_json::Value = serde_json::from_slice(bytes)
            .map_err(|error| format!("parse anthropic creds: {error}"))?;
        let oauth = raw
            .get("claudeAiOauth")
            .ok_or_else(|| "anthropic creds missing claudeAiOauth field".to_string())?;
        let block: OauthBlock = serde_json::from_value(oauth.clone())
            .map_err(|error| format!("parse claudeAiOauth: {error}"))?;
        Ok(Self {
            raw,
            real_refresh: block.refresh_token,
            real_access: block.access_token,
        })
    }

    pub fn real_refresh(&self) -> &str {
        &self.real_refresh
    }

    pub fn real_access(&self) -> &str {
        &self.real_access
    }

    /// Borrow the raw JSON. Used by [`crate::vault::lease`] to produce the
    /// stub JSON with unknown fields preserved.
    pub(crate) fn raw(&self) -> &serde_json::Value {
        &self.raw
    }
}

#[cfg(test)]
mod tests {
    use super::AnthropicCreds;

    #[test]
    fn parses_claude_credentials_json() {
        let body = br#"{
            "claudeAiOauth": {
                "accessToken": "real-access-abc",
                "refreshToken": "real-refresh-xyz",
                "expiresAt": 1700000000,
                "scopes": ["chat"],
                "subscriptionType": "pro"
            }
        }"#;
        let creds = AnthropicCreds::from_bytes(body).expect("parse");
        assert_eq!(creds.real_access(), "real-access-abc");
        assert_eq!(creds.real_refresh(), "real-refresh-xyz");
        // Unknown fields preserved in raw
        let oauth = creds
            .raw()
            .get("claudeAiOauth")
            .expect("oauth block")
            .as_object()
            .expect("object");
        assert_eq!(oauth.get("subscriptionType").and_then(|v| v.as_str()), Some("pro"));
    }

    #[test]
    fn rejects_unrecognized_shape() {
        let body = br#"{ "apiKey": "sk-ant-real-...." }"#;
        let err = AnthropicCreds::from_bytes(body).unwrap_err();
        assert!(err.contains("claudeAiOauth"), "got: {err}");
    }
}
