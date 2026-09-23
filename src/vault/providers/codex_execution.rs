//! Fresh, in-memory ChatGPT credentials for the bounded Codex execution adapter.
//!
//! Pinned to openai/codex rust-v0.151.0, commit 78c290807ce710180111df227df3b7a4fe845452:
//! login/src/token_data.rs accepts an ID JWT with empty claims and optional account_id;
//! login/src/auth/manager.rs requires tokens plus last_refresh to expose the bearer;
//! model-provider/src/bearer_auth_provider.rs adds an account header only when present.
//! These are parser/transport requirements, not proof of a successful provider request.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// Only `guest_credentials` may be serialized into the fresh guest home.
pub(crate) struct FreshCodexCredentials {
    pub guest_credentials: Value,
    pub access_release: CodexAccessRelease,
}

/// Host-only material; deliberately has no serialization or Debug implementation.
pub(crate) struct CodexAccessRelease {
    pub stub: String,
    pub real: String,
}

/// Accept managed ChatGPT auth only. The caller owns TokenStore refresh, the pinned
/// destination binding, release lifetime, and private guest-file creation.
pub(crate) fn fresh_codex_credentials(
    real: &Value,
    invocation_id: &str,
) -> Result<FreshCodexCredentials, String> {
    if invocation_id.is_empty()
        || invocation_id.trim() != invocation_id
        || invocation_id.chars().any(char::is_control)
    {
        return Err("Codex execution requires a nonempty invocation identity without surrounding whitespace or control characters".into());
    }
    let object = real
        .as_object()
        .ok_or("Codex execution credentials must be an object")?;
    match object.get("auth_mode") {
        None | Some(Value::Null) => {}
        Some(Value::String(mode)) if mode == "chatgpt" => {}
        _ => return Err("Codex execution supports only managed chatgpt authentication".into()),
    }
    for key in [
        "OPENAI_API_KEY",
        "agent_identity",
        "personal_access_token",
        "bedrock_api_key",
        "bedrock_access_keys",
    ] {
        if object.get(key).is_some_and(|value| !value.is_null()) {
            return Err("Codex execution rejects alternate or mixed authentication modes".into());
        }
    }
    let tokens = object
        .get("tokens")
        .and_then(Value::as_object)
        .ok_or("Codex execution requires a ChatGPT tokens object")?;
    let token = |name| {
        tokens
            .get(name)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_graphic()))
            .ok_or_else(|| format!("Codex execution requires a nonempty ASCII tokens.{name}"))
    };
    let access = token("access_token")?;
    token("refresh_token")?;
    validate_id_token(token("id_token")?)?;
    if let Some(account) = tokens.get("account_id").filter(|value| !value.is_null()) {
        if !account.as_str().is_some_and(|account| {
            !account.is_empty() && account.bytes().all(|byte| byte.is_ascii_graphic())
        }) {
            return Err(
                "Codex execution tokens.account_id must be a nonempty ASCII identity".into(),
            );
        }
    }

    let access_stub = synthetic_jwt(
        &json!({"exp": crate::vault::refresh::STUB_FAR_FUTURE_EXPIRES_AT_MS / 1000}),
        &fresh_stub("pb-codex-oat-", invocation_id),
    );
    // No source field is copied into this object. The optional account identity,
    // email, plan, user claims, and real ID/refresh tokens are intentionally absent.
    let guest_credentials = json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": synthetic_jwt(&json!({}), &fresh_stub("pb-codex-id-", invocation_id)),
            "access_token": access_stub,
            "refresh_token": fresh_stub("pb-codex-ort-", invocation_id),
        },
        "last_refresh": "2100-01-01T00:00:00Z",
    });
    Ok(FreshCodexCredentials {
        guest_credentials,
        access_release: CodexAccessRelease {
            stub: access_stub,
            real: access.to_owned(),
        },
    })
}

fn validate_id_token(token: &str) -> Result<(), String> {
    let mut parts = token.split('.');
    let payload = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(header), Some(payload), Some(signature), None)
            if !header.is_empty() && !payload.is_empty() && !signature.is_empty() =>
        {
            payload
        }
        _ => {
            return Err("Codex execution tokens.id_token must have three nonempty JWT parts".into())
        }
    };
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| "Codex execution tokens.id_token has an invalid JWT payload")?;
    let claims: Value = serde_json::from_slice(&bytes)
        .map_err(|_| "Codex execution tokens.id_token has invalid JSON claims")?;
    let claims = claims
        .as_object()
        .ok_or("Codex execution tokens.id_token claims must be an object")?;
    validate_optional_string(claims.get("email"))?;
    for key in [
        "https://api.openai.com/profile",
        "https://api.openai.com/auth",
    ] {
        let Some(value) = claims.get(key).filter(|value| !value.is_null()) else {
            continue;
        };
        let fields = value
            .as_object()
            .ok_or("Codex execution ID token identity claims must be objects")?;
        if key.ends_with("/profile") {
            validate_optional_string(fields.get("email"))?;
        } else {
            for key in [
                "chatgpt_plan_type",
                "chatgpt_user_id",
                "user_id",
                "chatgpt_account_id",
            ] {
                validate_optional_string(fields.get(key))?;
            }
            match fields.get("chatgpt_account_is_fedramp") {
                None | Some(Value::Bool(false)) => {}
                _ => {
                    return Err(
                        "Codex execution does not support FedRAMP or malformed routing claims"
                            .into(),
                    )
                }
            }
        }
    }
    Ok(())
}

fn validate_optional_string(value: Option<&Value>) -> Result<(), String> {
    match value {
        None | Some(Value::Null | Value::String(_)) => Ok(()),
        _ => Err("Codex execution ID token identity claims must be strings".into()),
    }
}

fn fresh_stub(prefix: &str, invocation_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(invocation_id.as_bytes());
    digest.update([0]);
    digest.update(rand::random::<[u8; 32]>());
    format!("{prefix}{}", URL_SAFE_NO_PAD.encode(digest.finalize()))
}

fn synthetic_jwt(claims: &Value, signature: &str) -> String {
    format!(
        "{}.{}.{}",
        URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#),
        URL_SAFE_NO_PAD.encode(claims.to_string()),
        signature,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials() -> Value {
        json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "last_refresh": "2020-01-01T00:00:00Z",
            "unknown_secret": "TOP_LEVEL_SECRET",
            "tokens": {
                "id_token": synthetic_jwt(&json!({
                    "email": "PRIVATE_EMAIL",
                    "https://api.openai.com/auth": {
                        "chatgpt_account_id": "PRIVATE_ACCOUNT",
                        "chatgpt_user_id": "PRIVATE_USER",
                        "chatgpt_plan_type": "PRIVATE_PLAN"
                    },
                    "unknown_claim": "PRIVATE_CLAIM"
                }), "REAL_ID_SIGNATURE"),
                "access_token": "REAL_ACCESS_TOKEN",
                "refresh_token": "REAL_REFRESH_TOKEN",
                "account_id": "PRIVATE_ACCOUNT",
                "unknown_secret": "TOKEN_LEVEL_SECRET"
            }
        })
    }

    fn claims(token: &str) -> Value {
        serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(token.split('.').nth(1).unwrap())
                .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn fresh_guest_shape_contains_only_generated_fields_and_one_host_access_release() {
        let real = credentials();
        let original = real.clone();
        let fresh = fresh_codex_credentials(&real, "invocation:test").unwrap();
        let guest = &fresh.guest_credentials;
        assert_eq!(real, original);
        assert_eq!(guest.as_object().unwrap().len(), 3);
        assert_eq!(guest["auth_mode"], "chatgpt");
        assert_eq!(guest["last_refresh"], "2100-01-01T00:00:00Z");
        assert_eq!(guest["tokens"].as_object().unwrap().len(), 3);
        assert_eq!(
            claims(guest["tokens"]["id_token"].as_str().unwrap()),
            json!({})
        );
        assert_eq!(
            claims(&fresh.access_release.stub),
            json!({"exp": 4_102_444_800_u64})
        );
        assert_eq!(fresh.access_release.stub, guest["tokens"]["access_token"]);
        assert_eq!(fresh.access_release.real, "REAL_ACCESS_TOKEN");
        assert!(guest["tokens"]["refresh_token"]
            .as_str()
            .unwrap()
            .starts_with("pb-codex-ort-"));
        let serialized = guest.to_string();
        for secret in [
            "REAL_",
            "PRIVATE_",
            "TOP_LEVEL_SECRET",
            "TOKEN_LEVEL_SECRET",
            "unknown_secret",
            "unknown_claim",
            "account_id",
            "invocation:test",
        ] {
            assert!(!serialized.contains(secret), "guest exposed {secret}");
        }
        validate_id_token(guest["tokens"]["id_token"].as_str().unwrap()).unwrap();
    }

    #[test]
    fn stubs_are_fresh_even_for_the_same_invocation() {
        let first = fresh_codex_credentials(&credentials(), "invocation:one").unwrap();
        for invocation in ["invocation:one", "invocation:two"] {
            let next = fresh_codex_credentials(&credentials(), invocation).unwrap();
            for key in ["id_token", "access_token", "refresh_token"] {
                assert_ne!(
                    first.guest_credentials["tokens"][key],
                    next.guest_credentials["tokens"][key]
                );
            }
        }
    }

    #[test]
    fn accepts_legacy_managed_mode_and_identity_free_id_token() {
        let mut real = credentials();
        real.as_object_mut().unwrap().remove("auth_mode");
        real["tokens"].as_object_mut().unwrap().remove("account_id");
        real["tokens"]["id_token"] = synthetic_jwt(&json!({}), "synthetic-source").into();
        assert!(fresh_codex_credentials(&real, "invocation:legacy").is_ok());
        real["auth_mode"] = Value::Null;
        assert!(fresh_codex_credentials(&real, "invocation:legacy").is_ok());
    }

    #[test]
    fn rejects_unknown_or_mixed_authentication_modes() {
        for mode in [
            json!("apikey"),
            json!("chatgptAuthTokens"),
            json!("future-mode"),
            json!(42),
        ] {
            let mut real = credentials();
            real["auth_mode"] = mode;
            assert!(fresh_codex_credentials(&real, "invocation:test").is_err());
        }
        for key in [
            "OPENAI_API_KEY",
            "agent_identity",
            "personal_access_token",
            "bedrock_api_key",
            "bedrock_access_keys",
        ] {
            let mut real = credentials();
            real[key] = json!("ALTERNATE_SECRET");
            assert!(fresh_codex_credentials(&real, "invocation:test").is_err());
        }
    }

    #[test]
    fn rejects_malformed_required_credentials_without_echoing_secrets() {
        for field in ["access_token", "refresh_token", "id_token"] {
            for value in [Value::Null, json!(42), json!(""), json!("PRIVATE_TOKEN\n")] {
                let mut real = credentials();
                real["tokens"][field] = value;
                let error = fresh_codex_credentials(&real, "invocation:test")
                    .err()
                    .unwrap();
                assert!(!error.contains("PRIVATE_TOKEN"));
            }
            let mut real = credentials();
            real["tokens"].as_object_mut().unwrap().remove(field);
            assert!(fresh_codex_credentials(&real, "invocation:test").is_err());
        }
        for malformed in [Value::Null, json!([]), json!({"tokens": false})] {
            assert!(fresh_codex_credentials(&malformed, "invocation:test").is_err());
        }
        for invocation in ["", " leading", "trailing ", "control\ninside"] {
            assert!(fresh_codex_credentials(&credentials(), invocation).is_err());
        }
    }

    #[test]
    fn rejects_malformed_identity_and_unsupported_routing() {
        for token in ["not-a-jwt", "h.@@.s", "h.bnVsbA.s", "h.e30.s.extra"] {
            let mut real = credentials();
            real["tokens"]["id_token"] = token.into();
            assert!(fresh_codex_credentials(&real, "invocation:test").is_err());
        }
        for invalid_claims in [
            json!({"email": 7}),
            json!({"https://api.openai.com/profile": "bad"}),
            json!({"https://api.openai.com/auth": {"chatgpt_account_id": []}}),
            json!({"https://api.openai.com/auth": {"chatgpt_account_is_fedramp": true}}),
        ] {
            let mut real = credentials();
            real["tokens"]["id_token"] = synthetic_jwt(&invalid_claims, "source").into();
            assert!(fresh_codex_credentials(&real, "invocation:test").is_err());
        }
        for account in [json!(4), json!(""), json!("invalid\naccount")] {
            let mut real = credentials();
            real["tokens"]["account_id"] = account;
            assert!(fresh_codex_credentials(&real, "invocation:test").is_err());
        }
    }
}
