//! Prime Agent's managed Prime Inference API-key store. Only the owning
//! provider entry survives the guest clone; unrelated provider credentials and
//! executable key references never enter the VM.

use std::path::Path;

use async_trait::async_trait;
use hudsucker::{
    hyper::{
        header::{HeaderValue, AUTHORIZATION},
        Request,
    },
    Body, RequestOrResponse,
};
use serde_json::{json, Value};

use super::{host_from_uri, mint_stub, unauthorized, Registry, SandboxData, VaultProvider};
#[cfg(feature = "libkrun")]
use super::{LibkrunOAuthStub, OAuthRelease};
use crate::vault::server::ServerInner;

const PROVIDER_ID: &str = "prime-agent";
const AUTH_ENTRY: &str = "prime-inference";
pub(crate) const API_HOST: &str = "api.pinference.ai";
pub(crate) const CREDS_PATH: &str = ".prime/agent/auth.json";
const STUB_PREFIX: &str = "pb-prime-key-";
#[cfg(any(feature = "libkrun", test))]
pub(crate) const ISOLATED_SETTINGS: &str = r#"{"compaction":{"enabled":false,"agentCallable":false},"autoRefine":{"enabled":false,"compact":false},"retry":{"enabled":false,"provider":{"waitForUsage":{"enabled":false}}},"agentTraces":{"enabled":false},"telemetry":{"enabled":false,"noticeShown":true}}"#;

pub(crate) struct PrimeProvider;

/// Literal API keys only. Prime's `!command` references are rejected. The host
/// never executes references or resolves environment indirections from this file;
/// only a freshly minted stub is visible to the guest.
pub(crate) fn credential_key(real: &Value) -> Result<&str, String> {
    let entry = real
        .get(AUTH_ENTRY)
        .ok_or("missing Prime Inference credential")?;
    if entry.get("type").and_then(Value::as_str) != Some("api_key") {
        return Err("Prime Inference requires a managed API-key credential".into());
    }
    let key = entry
        .get("key")
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty() && key.len() <= 16 * 1024)
        .ok_or("missing Prime Inference API key")?;
    if key.starts_with('!')
        || key.chars().any(char::is_whitespace)
        || HeaderValue::from_str(&format!("Bearer {key}")).is_err()
    {
        return Err("invalid literal Prime Inference API key".into());
    }
    Ok(key)
}

fn stub_credentials(stub: &str) -> Value {
    // primeTeam is deliberately absent. Public text models need no team header,
    // and keeping only this shape prevents credential/config injection.
    json!({AUTH_ENTRY: {"type": "api_key", "key": stub}})
}

#[async_trait]
impl VaultProvider for PrimeProvider {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn intercept(&self, host: &str) -> bool {
        host == API_HOST
    }

    fn hosts(&self) -> &'static [&'static str] {
        &[API_HOST]
    }

    fn creds_path(&self) -> &'static Path {
        Path::new(CREDS_PATH)
    }

    fn provision(
        &self,
        sandbox_id: &str,
        real: &Value,
        registry: &mut Registry,
    ) -> Result<String, String> {
        let key = credential_key(real)?;
        let stub = mint_stub(STUB_PREFIX, sandbox_id);
        let credentials = serde_json::to_string(&stub_credentials(&stub))
            .map_err(|_| "serialize Prime Inference stub".to_string())?;
        registry.insert(
            sandbox_id.to_string(),
            SandboxData {
                provider_id: PROVIDER_ID,
                real: json!({AUTH_ENTRY: {"type": "api_key", "key": key}}),
                stubs: vec![stub],
            },
        );
        Ok(credentials)
    }

    #[cfg(feature = "libkrun")]
    fn libkrun_credentials(&self, real: &mut Value) -> Result<LibkrunOAuthStub, String> {
        let key = credential_key(real)?.to_string();
        let stub = mint_stub(STUB_PREFIX, "libkrun");
        *real = stub_credentials(&stub);
        Ok(LibkrunOAuthStub {
            access_stub: Some(stub.clone()),
            releases: vec![OAuthRelease { stub, real: key }],
        })
    }

    #[cfg(any(feature = "libkrun", test))]
    fn sanitize_cloned_home(&self, home: &Path) -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        // Context: doc://pillbox/libkrun-env-fork-substrate@0002#libkrun-env-fork-substrate — only managed stubs survive in the throwaway guest HOME.
        let config = home.join(".prime");
        match std::fs::symlink_metadata(&config) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                std::fs::remove_dir_all(&config)?
            }
            Ok(_) => std::fs::remove_file(&config)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let agent = config.join("agent");
        std::fs::create_dir_all(&agent)?;
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o700))?;
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o700))?;
        let settings = agent.join("settings.json");
        std::fs::write(&settings, ISOLATED_SETTINGS)?;
        std::fs::set_permissions(settings, std::fs::Permissions::from_mode(0o600))?;
        Ok(())
    }

    async fn handle_request(&self, req: Request<Body>, server: &ServerInner) -> RequestOrResponse {
        if host_from_uri(&req).as_deref() != Some(API_HOST) {
            return req.into();
        }
        let (mut parts, body) = req.into_parts();
        let Some(auth) = parts.headers.get(AUTHORIZATION) else {
            return Request::from_parts(parts, body).into();
        };
        let Some(stub) = auth
            .to_str()
            .ok()
            .and_then(|value| value.strip_prefix("Bearer "))
        else {
            return unauthorized("invalid Prime authorization").into();
        };
        let key = {
            let registry = server.registry_lock();
            stub.starts_with(STUB_PREFIX)
                .then(|| registry.sandbox_for_stub(stub))
                .flatten()
                .and_then(|sandbox| registry.real(sandbox))
                .and_then(|real| credential_key(real).ok())
                .map(str::to_string)
                .or_else(|| {
                    registry
                        .api_key_real_for_stub(stub, API_HOST)
                        .map(str::to_string)
                })
        };
        match key {
            Some(key) => match HeaderValue::from_str(&format!("Bearer {key}")) {
                Ok(header) => {
                    parts.headers.insert(AUTHORIZATION, header);
                    Request::from_parts(parts, body).into()
                }
                Err(_) => unauthorized("invalid Prime API-key header").into(),
            },
            None if stub.starts_with(STUB_PREFIX) => unauthorized("unknown Prime stub").into(),
            None => Request::from_parts(parts, body).into(),
        }
    }

    fn is_chat_request(&self, method: &str, path: &str) -> bool {
        method == "POST" && path == "/api/v1/chat/completions"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::providers::test_support::{
        build_request, cleanup, expect_request, fresh_server,
    };

    #[test]
    fn provision_scrubs_other_provider_entries_and_key_commands_are_rejected() {
        let mut registry = Registry::new();
        let original = json!({"prime-inference": {"type": "api_key", "key": "fixture-prime-key", "primeTeam": {"teamId": "team"}},
            "anthropic": {"type": "oauth", "access": "fixture-unrelated-key"}});
        let stub: Value = serde_json::from_str(
            &PrimeProvider
                .provision("s1", &original, &mut registry)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(stub.as_object().unwrap().len(), 1);
        assert_eq!(stub[AUTH_ENTRY].as_object().unwrap().len(), 2);
        assert_ne!(stub[AUTH_ENTRY]["key"], original[AUTH_ENTRY]["key"]);
        for key in ["", "!printf key", "bad\nkey"] {
            assert!(PrimeProvider
                .provision(
                    "bad",
                    &json!({AUTH_ENTRY: {"type": "api_key", "key": key}}),
                    &mut registry
                )
                .is_err());
        }
    }

    #[tokio::test]
    async fn managed_key_is_released_only_on_the_prime_host() {
        let (server, dir) = fresh_server().await;
        let lease = server
            .lease(
                PROVIDER_ID,
                "s1",
                json!({AUTH_ENTRY: {"type": "api_key", "key": "fixture-prime-key"}}),
            )
            .unwrap();
        let stub = server.registry_lock_for_test().stubs_for("s1").unwrap()[0].clone();
        for (host, expected) in [
            (API_HOST, "fixture-prime-key"),
            ("api.openai.com", stub.as_str()),
        ] {
            let mut request = build_request(
                "POST",
                &format!("https://{host}/api/v1/chat/completions"),
                Body::empty(),
            );
            request
                .headers_mut()
                .insert(AUTHORIZATION, format!("Bearer {stub}").parse().unwrap());
            let request = expect_request(
                PrimeProvider
                    .handle_request(request, server.inner_for_test())
                    .await,
                "Prime swap",
            );
            assert_eq!(
                request.headers()[AUTHORIZATION],
                format!("Bearer {expected}")
            );
        }
        drop(lease);
        cleanup(server, dir);
    }

    #[cfg(feature = "libkrun")]
    #[test]
    fn libkrun_clone_contains_only_one_stub_and_one_release() {
        let mut real = json!({AUTH_ENTRY: {"type": "api_key", "key": "fixture-prime-key"}, "openai": {"key": "fixture-other-key"}});
        let stubbed = PrimeProvider.libkrun_credentials(&mut real).unwrap();
        assert_eq!(
            real,
            stub_credentials(stubbed.access_stub.as_deref().unwrap())
        );
        assert_eq!(stubbed.releases.len(), 1);
        assert_eq!(stubbed.releases[0].real, "fixture-prime-key");
    }

    #[test]
    fn cloned_home_retains_only_private_settings() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".prime/agent")).unwrap();
        for file in [
            ".prime/config.json",
            ".prime/agent/auth.json",
            ".prime/agent/models.json",
            ".prime/agent/settings.json",
        ] {
            std::fs::write(home.path().join(file), "fixture-real-key").unwrap();
        }
        PrimeProvider.sanitize_cloned_home(home.path()).unwrap();
        assert!(home.path().join(".prime/agent").is_dir());
        assert_eq!(
            std::fs::read_dir(home.path().join(".prime/agent"))
                .unwrap()
                .count(),
            1
        );
        assert_eq!(
            std::fs::read_to_string(home.path().join(".prime/agent/settings.json")).unwrap(),
            ISOLATED_SETTINGS
        );
        assert!(!home.path().join(".prime/config.json").exists());
    }

    #[test]
    fn cloned_home_scrub_unlinks_config_symlink_without_following_it() {
        use std::os::unix::fs::symlink;

        let home = tempfile::tempdir().unwrap();
        let original = tempfile::tempdir().unwrap();
        let original_file = original.path().join("auth.json");
        std::fs::write(&original_file, "fixture-real-key").unwrap();
        symlink(original.path(), home.path().join(".prime")).unwrap();
        PrimeProvider.sanitize_cloned_home(home.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(original_file).unwrap(),
            "fixture-real-key"
        );
        assert!(home.path().join(".prime/agent/settings.json").is_file());
        assert!(!std::fs::symlink_metadata(home.path().join(".prime"))
            .unwrap()
            .file_type()
            .is_symlink());
    }
}
