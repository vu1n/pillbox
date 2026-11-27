//! Codex provider — OpenAI ChatGPT-mode OAuth.
//!
//! v0.5 scope: ChatGPT mode only. The codex `auth.json` schema is:
//!
//! ```jsonc
//! {
//!   "auth_mode": "ChatGPT",
//!   "tokens": {
//!     "id_token": "<JWT>",        // identity claims; copied verbatim into the stub
//!     "access_token": "<JWT>",    // stubbed; released only to API hosts
//!     "refresh_token": "<opaque>",// shape-valid stub; never released
//!     "account_id": "<id>"        // copied verbatim
//!   },
//!   "last_refresh": "<ISO ts>",
//!   "agent_identity": null
//! }
//! ```
//!
//! ApiKey-mode auth.json (with `OPENAI_API_KEY` set instead of `tokens`)
//! is rejected — that path needs the API-key vault track and isn't ready
//! yet (pillbox task #26).
//!
//! Intercepted hosts:
//!  - `chatgpt.com` (exact + any subdomain) — bearer-token release.
//!  - `chat.openai.com` — same.
//!  - `auth.openai.com` — `/oauth/token` is intercepted and rejected locally
//!    (verified against `codex-rs/login/src/auth/manager.rs`:
//!    `const REFRESH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token"`).
//!
//! `api.openai.com` is **not** intercepted in v0.5 — that's the API-key
//! path, deferred until the ApiKey vault track lands.

use super::{mint_stub, HostProxyOAuthStub, OAuthCodec, OAuthRefreshRequest, VaultProvider};
use super::{LibkrunOAuthStub, OAuthRelease};
use crate::vault::token_store::RefreshDecider;

const PROVIDER_ID: &str = "codex";

const CHATGPT_HOST: &str = "chatgpt.com";
const CHATGPT_HOST_DOT: &str = ".chatgpt.com";
const CHAT_OPENAI_HOST: &str = "chat.openai.com";
const AUTH_OPENAI_HOST: &str = "auth.openai.com";
const OAUTH_TOKEN_PATH_SUFFIX: &str = "/oauth/token";
const OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OAUTH_ENDPOINT: &str = "https://auth.openai.com/oauth/token";

// Codex doesn't ship a public stub prefix convention (tokens are opaque
// JWTs / random strings). The `pb-codex-` family makes any logged stub
// visibly a pillbox artifact rather than a real OpenAI token.
pub(crate) const STUB_ACCESS_PREFIX: &str = "pb-codex-oat-";
pub(crate) const STUB_REFRESH_PREFIX: &str = "pb-codex-ort-";

pub(crate) struct CodexProvider;

impl VaultProvider for CodexProvider {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn intercept(&self, host: &str) -> bool {
        host == CHATGPT_HOST
            || host.ends_with(CHATGPT_HOST_DOT)
            || host == CHAT_OPENAI_HOST
            || host == AUTH_OPENAI_HOST
    }

    /// Concrete hosts for the DNS-fence allowlist. The `*.chatgpt.com` wildcard
    /// `intercept` also matches isn't enumerable for an exact-match DNS allowlist
    /// — a known gap for codex subdomains (claude/the api hosts are exact).
    fn hosts(&self) -> &'static [&'static str] {
        &[CHATGPT_HOST, CHAT_OPENAI_HOST, AUTH_OPENAI_HOST]
    }

    fn oauth_codec(&self) -> Option<&dyn OAuthCodec> {
        Some(self)
    }

    fn is_oauth_token_endpoint(&self, host: &str, path: &str) -> bool {
        host == AUTH_OPENAI_HOST && path.ends_with(OAUTH_TOKEN_PATH_SUFFIX)
    }
}

impl RefreshDecider for CodexProvider {
    fn needs_refresh(&self, creds: &serde_json::Value) -> bool {
        self.expiry_ms(creds)
            .map(crate::vault::refresh::is_expired)
            .unwrap_or(true)
    }

    fn refresh_token(&self, creds: &serde_json::Value) -> Option<String> {
        creds
            .pointer("/tokens/refresh_token")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    }

    fn access_token(&self, creds: &serde_json::Value) -> Option<String> {
        creds
            .pointer("/tokens/access_token")
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

impl OAuthCodec for CodexProvider {
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
        let tokens = real
            .get_mut("tokens")
            .and_then(|v| v.as_object_mut())
            .ok_or_else(|| {
                crate::errors::PillboxError::runtime("vault", "codex tokens block disappeared")
            })?;
        tokens.insert("access_token".into(), access.into());
        tokens.insert("refresh_token".into(), refresh.into());
        if let Some(id_token) = response.get("id_token").and_then(|v| v.as_str()) {
            tokens.insert("id_token".into(), id_token.into());
        }
        if self.expiry_ms(real).is_none() {
            return Err(crate::errors::PillboxError::runtime(
                "vault",
                "codex refresh response access_token has no usable JWT expiry",
            )
            .into());
        }
        let refreshed_at = time::OffsetDateTime::from_unix_timestamp_nanos(
            i128::from(now_ms).saturating_mul(1_000_000),
        )
        .map_err(|_| {
            crate::errors::PillboxError::runtime("vault", "refresh timestamp out of range")
        })?
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|_| crate::errors::PillboxError::runtime("vault", "format refresh timestamp"))?;
        real.as_object_mut()
            .ok_or_else(|| {
                crate::errors::PillboxError::runtime("vault", "codex auth.json is not an object")
            })?
            .insert("last_refresh".into(), refreshed_at.into());
        Ok(())
    }

    fn expiry_ms(&self, real: &serde_json::Value) -> Option<u64> {
        use base64::Engine as _;
        let token = real.pointer("/tokens/access_token")?.as_str()?;
        let payload = token.split('.').nth(1)?;
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .ok()?;
        let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
        Some(claims.get("exp")?.as_u64()?.saturating_mul(1000))
    }

    fn host_proxy_stub(
        &self,
        sandbox_id: &str,
        real: &serde_json::Value,
    ) -> Result<HostProxyOAuthStub, String> {
        let obj = real
            .as_object()
            .ok_or_else(|| "codex auth.json must be a JSON object".to_string())?;
        let tokens = match obj.get("tokens") {
            Some(serde_json::Value::Object(map)) => map,
            Some(serde_json::Value::Null) | None => {
                return Err("codex auth.json has no `tokens` block (ApiKey mode). \
                     v0.5 vault supports ChatGPT mode only. \
                     The API-key path lands with the API-key vault track \
                     (pillbox task #26)."
                    .into());
            }
            Some(_) => return Err("codex auth.json `tokens` is not an object".into()),
        };
        let access = tokens
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "codex auth.json: tokens.access_token missing".to_string())?;
        tokens
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "codex auth.json: tokens.refresh_token missing".to_string())?;
        if access.split('.').count() != 3 {
            return Err(
                "codex auth.json: tokens.access_token is not a JWT (expected 3 dot-separated parts)"
                    .into(),
            );
        }
        let access = mint_stub(STUB_ACCESS_PREFIX, sandbox_id);
        let refresh = mint_stub(STUB_REFRESH_PREFIX, sandbox_id);
        let mut credentials = real.clone();
        let tokens = credentials
            .get_mut("tokens")
            .and_then(|v| v.as_object_mut())
            .ok_or_else(|| "tokens block missing during stub build".to_string())?;
        tokens.insert("access_token".into(), access.into());
        tokens.insert("refresh_token".into(), refresh.into());
        Ok(HostProxyOAuthStub { credentials })
    }

    fn libkrun_stub(&self, real: &mut serde_json::Value) -> LibkrunOAuthStub {
        let Some(tokens) = real.get_mut("tokens").and_then(|v| v.as_object_mut()) else {
            return LibkrunOAuthStub {
                access_stub: None,
                releases: Vec::new(),
            };
        };
        let mut releases = Vec::new();
        let access_stub = tokens
            .get("access_token")
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .map(|real| {
                let stub = mint_libkrun_access_stub();
                tokens.insert("access_token".into(), stub.clone().into());
                releases.push(OAuthRelease {
                    stub: stub.clone(),
                    real,
                });
                stub
            });
        if tokens
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .is_some_and(|v| !v.is_empty())
        {
            tokens.insert(
                "refresh_token".into(),
                format!(
                    "{STUB_REFRESH_PREFIX}pllbxstub{}",
                    uuid::Uuid::now_v7().simple()
                )
                .into(),
            );
        }
        LibkrunOAuthStub {
            access_stub,
            releases,
        }
    }
}

fn mint_libkrun_access_stub() -> String {
    use base64::Engine as _;
    let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
    let payload = serde_json::to_vec(&serde_json::json!({
        "exp": crate::vault::STUB_FAR_FUTURE_EXPIRES_AT_MS / 1000,
    }))
    .expect("static codex stub claims serialize");
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload);
    let signature = format!(
        "{STUB_ACCESS_PREFIX}pllbxstub{}",
        uuid::Uuid::now_v7().simple()
    );
    format!("{header}.{payload}.{signature}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::providers::test_support::FAKE_JWT;

    fn sample_chatgpt_real() -> serde_json::Value {
        serde_json::json!({
            "auth_mode": "ChatGPT",
            "tokens": {
                "id_token": FAKE_JWT,
                "access_token": FAKE_JWT,
                "refresh_token": "rt_opaque_real_xyz",
                "account_id": "acct_abc"
            },
            "last_refresh": "2026-05-18T00:00:00Z",
            "agent_identity": serde_json::Value::Null,
            // Pretend an unknown field codex might add in the future.
            "future_feature": "preserve_me"
        })
    }

    fn sample_apikey_real() -> serde_json::Value {
        serde_json::json!({
            "OPENAI_API_KEY": "sk-real-api-key",
            "tokens": serde_json::Value::Null
        })
    }

    #[test]
    fn host_proxy_stub_chatgpt_mode_mints_stubs_and_preserves_unknown_fields() {
        let parsed = CodexProvider
            .host_proxy_stub("sbx-xyz", &sample_chatgpt_real())
            .expect("stub")
            .credentials;
        let stub_json = parsed.to_string();
        let tokens = parsed.get("tokens").unwrap().as_object().unwrap();
        let access = tokens.get("access_token").and_then(|v| v.as_str()).unwrap();
        let refresh = tokens
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .unwrap();

        // Stub format.
        assert!(access.starts_with(STUB_ACCESS_PREFIX), "got {access}");
        assert!(refresh.starts_with(STUB_REFRESH_PREFIX), "got {refresh}");
        let tail_a = access.strip_prefix(STUB_ACCESS_PREFIX).unwrap();
        let tail_r = refresh.strip_prefix(STUB_REFRESH_PREFIX).unwrap();
        assert!(tail_a.chars().all(|c| c.is_ascii_alphanumeric()));
        assert!(tail_r.chars().all(|c| c.is_ascii_alphanumeric()));
        // Sandbox id encoded (dashes stripped).
        assert!(access.contains("sbxxyz"));

        // id_token + account_id preserved verbatim.
        assert_eq!(
            tokens.get("id_token").and_then(|v| v.as_str()),
            Some(FAKE_JWT)
        );
        assert_eq!(
            tokens.get("account_id").and_then(|v| v.as_str()),
            Some("acct_abc")
        );

        // Real refresh token never appears in stub.
        assert!(!stub_json.contains("rt_opaque_real_xyz"));

        // Outer-level unknown fields preserved.
        assert_eq!(
            parsed.get("future_feature").and_then(|v| v.as_str()),
            Some("preserve_me")
        );
        assert_eq!(
            parsed.get("auth_mode").and_then(|v| v.as_str()),
            Some("ChatGPT")
        );
    }

    #[test]
    fn host_proxy_stub_apikey_mode_is_rejected_with_clear_message() {
        let Err(err) = CodexProvider.host_proxy_stub("sbx-1", &sample_apikey_real()) else {
            panic!("ApiKey-mode auth.json must be refused");
        };
        assert!(
            err.contains("ApiKey mode") && err.contains("task #26"),
            "expected ApiKey + task #26 hint, got: {err}"
        );
    }

    #[test]
    fn host_proxy_stub_rejects_non_jwt_access_token() {
        let mut bad = sample_chatgpt_real();
        bad["tokens"]["access_token"] = serde_json::Value::String("notajwt".into());
        let Err(err) = CodexProvider.host_proxy_stub("sbx", &bad) else {
            panic!("a non-JWT access token must be refused");
        };
        assert!(err.contains("JWT"), "got: {err}");
    }

    #[test]
    fn intercept_covers_chatgpt_and_oauth_only() {
        let p = CodexProvider;
        assert!(p.intercept("chatgpt.com"));
        assert!(p.intercept("backend-api.chatgpt.com"));
        assert!(p.intercept("chat.openai.com"));
        assert!(p.intercept("auth.openai.com"));
        // api.openai.com is *not* intercepted in v0.5 — API-key path.
        assert!(!p.intercept("api.openai.com"));
        assert!(!p.intercept("api.anthropic.com"));
        assert!(!p.intercept("openai.com"));
    }
}
