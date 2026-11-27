//! OpenAI API-key provider.
//!
//! Claims `api.openai.com`, where a `--with NAME=ENV_VAR --vault`'d key's
//! pillbox-minted stub is released for the real key.
//!
//! This is **API-key only** — codex's ChatGPT OAuth lives on
//! `chatgpt.com` / `auth.openai.com` and is handled by `codex.rs`. They
//! never collide because the hosts differ.
//!
//! This provider doesn't own a credentials file; the stub lives in a
//! `--with NAME=ENV_VAR` env var inside the guest.

use super::VaultProvider;

const PROVIDER_ID: &str = "openai-api-key";
const API_HOST: &str = "api.openai.com";

pub(crate) struct OpenAiApiKeyProvider;

impl VaultProvider for OpenAiApiKeyProvider {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn intercept(&self, host: &str) -> bool {
        host == API_HOST
    }

    fn hosts(&self) -> &'static [&'static str] {
        &[API_HOST]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intercept_only_api_openai() {
        let p = OpenAiApiKeyProvider;
        assert!(p.intercept("api.openai.com"));
        // codex's hosts must NOT be intercepted by this provider.
        assert!(!p.intercept("chatgpt.com"));
        assert!(!p.intercept("chat.openai.com"));
        assert!(!p.intercept("auth.openai.com"));
        assert!(!p.intercept("api.anthropic.com"));
    }
}
