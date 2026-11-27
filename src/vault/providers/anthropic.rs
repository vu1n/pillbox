//! Anthropic provider — Claude Code OAuth.
//!
//! Anthropic API hosts are intercepted for access-token release. OAuth token
//! endpoints are intercepted only to reject guest-owned rotation locally; the
//! host broker is the sole rotation authority.

use serde::Deserialize;

use super::{mint_stub, HostProxyOAuthStub, OAuthCodec, OAuthRefreshRequest, VaultProvider};
use super::{LibkrunOAuthStub, OAuthRelease};
use crate::vault::token_store::RefreshDecider;
use crate::vault::STUB_FAR_FUTURE_EXPIRES_AT_MS as STUB_EXPIRES_AT_MS;

// Provider id matches the AgentSpec auth id (`claude`) so `provider_for`
// can look up the right provider directly. The module is named `anthropic` because that's the
// underlying upstream service this provider knows how to swap creds
// for; the *agent* using it happens to be Claude Code.
const PROVIDER_ID: &str = "claude";

const API_HOST: &str = "api.anthropic.com";
const CONSOLE_HOST: &str = "console.anthropic.com";
/// Newer Anthropic OAuth host. It remains intercepted so the guest token
/// endpoint can be rejected locally instead of reaching the provider.
const PLATFORM_HOST: &str = "platform.claude.com";
const OAUTH_TOKEN_PATH_SUFFIX: &str = "/oauth/token";
const OAUTH_CLIENT_ID: &str = "claude_code";
const OAUTH_ENDPOINT: &str = "https://platform.claude.com/oauth/token";
const FALLBACK_EXPIRES_IN_SECS: u64 = 3600;

// Stub tokens mimic Anthropic's `sk-ant-oat01-` / `sk-ant-ort01-`
// prefixes so Claude Code's local format validation accepts them. The
// suffix is pure alphanumeric (no dashes/underscores) for the same
// reason. Anthropic doesn't see these — by the time a request hits the
// wire the proxy has swapped them for the real values.
pub(crate) const STUB_ACCESS_PREFIX: &str = "sk-ant-oat01-";
pub(crate) const STUB_REFRESH_PREFIX: &str = "sk-ant-ort01-";

pub(crate) struct AnthropicProvider;

impl VaultProvider for AnthropicProvider {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn intercept(&self, host: &str) -> bool {
        host == API_HOST || host == CONSOLE_HOST || host == PLATFORM_HOST
    }

    fn hosts(&self) -> &'static [&'static str] {
        &[API_HOST, CONSOLE_HOST, PLATFORM_HOST]
    }

    fn oauth_codec(&self) -> Option<&dyn OAuthCodec> {
        Some(self)
    }

    fn is_oauth_token_endpoint(&self, host: &str, path: &str) -> bool {
        (host == CONSOLE_HOST || host == PLATFORM_HOST) && path.ends_with(OAUTH_TOKEN_PATH_SUFFIX)
    }
}

impl RefreshDecider for AnthropicProvider {
    fn needs_refresh(&self, creds: &serde_json::Value) -> bool {
        self.expiry_ms(creds)
            .map(crate::vault::refresh::is_expired)
            .unwrap_or(false)
    }

    fn refresh_token(&self, creds: &serde_json::Value) -> Option<String> {
        creds
            .pointer("/claudeAiOauth/refreshToken")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    }

    fn access_token(&self, creds: &serde_json::Value) -> Option<String> {
        creds
            .pointer("/claudeAiOauth/accessToken")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    }

    fn access_usable(&self, creds: &serde_json::Value) -> bool {
        self.access_token(creds).is_some()
            && self
                .expiry_ms(creds)
                .is_some_and(|expiry| expiry > crate::vault::refresh::unix_now_ms())
    }
}

impl OAuthCodec for AnthropicProvider {
    fn refresh_request(&self, refresh_token: &str) -> OAuthRefreshRequest {
        OAuthRefreshRequest {
            endpoint: OAUTH_ENDPOINT,
            body: serde_json::json!({
                "grant_type": "refresh_token",
                "refresh_token": refresh_token,
                "client_id": OAUTH_CLIENT_ID,
            }),
        }
    }

    fn apply_refresh_response(
        &self,
        real: &mut serde_json::Value,
        response: &serde_json::Value,
        old_refresh: &str,
        now_ms: u64,
    ) -> anyhow::Result<()> {
        let access = response
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                crate::errors::PillboxError::runtime(
                    "vault",
                    "refresh response missing access_token",
                )
            })?;
        let refresh = response
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .unwrap_or(old_refresh);
        let expires_in = response
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(FALLBACK_EXPIRES_IN_SECS);
        let oauth = real
            .get_mut("claudeAiOauth")
            .and_then(|v| v.as_object_mut())
            .ok_or_else(|| {
                crate::errors::PillboxError::runtime("vault", "claudeAiOauth block disappeared")
            })?;
        oauth.insert("accessToken".into(), access.into());
        oauth.insert("refreshToken".into(), refresh.into());
        oauth.insert(
            "expiresAt".into(),
            serde_json::Value::Number(serde_json::Number::from(
                now_ms.saturating_add(expires_in.saturating_mul(1000)),
            )),
        );
        Ok(())
    }

    fn expiry_ms(&self, real: &serde_json::Value) -> Option<u64> {
        real.pointer("/claudeAiOauth/expiresAt")
            .and_then(|v| v.as_u64())
            .map(crate::vault::refresh::normalize_expiry_ms)
    }

    fn host_proxy_stub(
        &self,
        sandbox_id: &str,
        real: &serde_json::Value,
    ) -> Result<HostProxyOAuthStub, String> {
        let oauth = real
            .get("claudeAiOauth")
            .ok_or_else(|| "anthropic creds missing claudeAiOauth field".to_string())?;
        let _block: OauthBlock = serde_json::from_value(oauth.clone())
            .map_err(|error| format!("parse claudeAiOauth: {error}"))?;
        let access = mint_stub(STUB_ACCESS_PREFIX, sandbox_id);
        let refresh = mint_stub(STUB_REFRESH_PREFIX, sandbox_id);
        let mut credentials = real.clone();
        let oauth = credentials
            .get_mut("claudeAiOauth")
            .and_then(|v| v.as_object_mut())
            .ok_or_else(|| "claudeAiOauth block missing".to_string())?;
        oauth.insert("accessToken".into(), access.into());
        oauth.insert("refreshToken".into(), refresh.into());
        oauth.insert(
            "expiresAt".into(),
            serde_json::Value::Number(serde_json::Number::from(STUB_EXPIRES_AT_MS)),
        );
        Ok(HostProxyOAuthStub { credentials })
    }

    fn libkrun_stub(&self, real: &mut serde_json::Value) -> LibkrunOAuthStub {
        let Some(oauth) = real
            .get_mut("claudeAiOauth")
            .and_then(|v| v.as_object_mut())
        else {
            return LibkrunOAuthStub {
                access_stub: None,
                releases: Vec::new(),
            };
        };
        let mut releases = Vec::new();
        let mut access_stub = None;
        for field in ["accessToken", "refreshToken"] {
            let Some(token) = oauth
                .get(field)
                .and_then(|v| v.as_str())
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
            else {
                continue;
            };
            let stub = mint_libkrun_stub(&token);
            oauth.insert(field.into(), stub.clone().into());
            if field == "accessToken" {
                access_stub = Some(stub.clone());
            }
            // The guest needs a shape-valid refresh stub in its file, but only the
            // access stub is releasable on egress. Refresh rotation belongs to the
            // broker and token-endpoint requests are locally rejected.
            if field == "accessToken" {
                releases.push(OAuthRelease { stub, real: token });
            }
        }
        if !releases.is_empty() {
            oauth.insert(
                "expiresAt".into(),
                serde_json::Value::Number(serde_json::Number::from(STUB_EXPIRES_AT_MS)),
            );
        }
        LibkrunOAuthStub {
            access_stub,
            releases,
        }
    }
}

fn mint_libkrun_stub(real: &str) -> String {
    let prefix = if real.split('-').count() >= 4 {
        real.splitn(4, '-').take(3).collect::<Vec<_>>().join("-")
    } else {
        "pllbx".to_string()
    };
    format!("{prefix}-pllbxstub{}", uuid::Uuid::now_v7().simple())
}

#[derive(Debug, Deserialize)]
struct OauthBlock {
    #[serde(rename = "accessToken")]
    _access_token: String,
    #[serde(rename = "refreshToken")]
    _refresh_token: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_real() -> serde_json::Value {
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "REAL_ACCESS",
                "refreshToken": "REAL_REFRESH",
                "expiresAt": 1700000000_u64,
                "subscriptionType": "pro"
            }
        })
    }

    #[test]
    fn host_proxy_stub_swaps_tokens_for_stubs() {
        let parsed = AnthropicProvider
            .host_proxy_stub("sbx-abc", &sample_real())
            .expect("stub")
            .credentials;
        let stub_json = parsed.to_string();
        let oauth = parsed.get("claudeAiOauth").unwrap();
        let access = oauth.get("accessToken").and_then(|v| v.as_str()).unwrap();
        let refresh = oauth.get("refreshToken").and_then(|v| v.as_str()).unwrap();

        // Stub format.
        assert!(access.starts_with(STUB_ACCESS_PREFIX));
        assert!(refresh.starts_with(STUB_REFRESH_PREFIX));
        let tail_a = access.strip_prefix(STUB_ACCESS_PREFIX).unwrap();
        let tail_r = refresh.strip_prefix(STUB_REFRESH_PREFIX).unwrap();
        assert!(tail_a.chars().all(|c| c.is_ascii_alphanumeric()));
        assert!(tail_r.chars().all(|c| c.is_ascii_alphanumeric()));
        // Sandbox id encoded in the tail (dashes stripped).
        assert!(access.contains("sbxabc"));
        assert!(refresh.contains("sbxabc"));

        // Real tokens never appear in the stub.
        assert!(!stub_json.contains("REAL_ACCESS"));
        assert!(!stub_json.contains("REAL_REFRESH"));

        // Unknown fields preserved.
        assert_eq!(
            oauth.get("subscriptionType").and_then(|v| v.as_str()),
            Some("pro")
        );
        // Broker move: the stub's expiry is post-dated to the far-future sentinel
        // (NOT the real `1700000000`), so the guest never refreshes itself.
        assert_eq!(
            oauth.get("expiresAt").and_then(|v| v.as_u64()),
            Some(STUB_EXPIRES_AT_MS)
        );
    }

    #[test]
    fn host_proxy_stub_rejects_missing_oauth_block() {
        let bad = serde_json::json!({"apiKey": "sk-ant-real"});
        let Err(err) = AnthropicProvider.host_proxy_stub("sbx-1", &bad) else {
            panic!("creds without claudeAiOauth must be refused");
        };
        assert!(err.contains("claudeAiOauth"), "got: {err}");
    }

    #[test]
    fn intercept_matches_anthropic_hosts_only() {
        let p = AnthropicProvider;
        assert!(p.intercept("api.anthropic.com"));
        assert!(p.intercept("console.anthropic.com"));
        assert!(p.intercept("platform.claude.com"));
        assert!(!p.intercept("anthropic.com"));
        assert!(!p.intercept("claude.com"));
        assert!(!p.intercept("chatgpt.com"));
    }
}
