//! GitHub PAT (personal access token) provider.
//!
//! Claims `api.github.com`, where a `--with NAME=ENV_VAR --vault`'d PAT's
//! pillbox-minted stub is released for the real token. GitHub accepts both
//! `Authorization: Bearer <token>` and `Authorization: token <token>`; the
//! release is a scheme-agnostic stub substitution, so both work.
//!
//! Like the OpenAI provider, the value travels via `--with NAME=ENV_VAR`
//! in the guest env — no credentials file on disk.

use super::VaultProvider;

const PROVIDER_ID: &str = "github-pat";
const API_HOST: &str = "api.github.com";

pub(crate) struct GithubProvider;

impl VaultProvider for GithubProvider {
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
    fn intercept_matches_api_github_only() {
        let p = GithubProvider;
        assert!(p.intercept("api.github.com"));
        assert!(!p.intercept("github.com"));
        assert!(!p.intercept("raw.githubusercontent.com"));
    }
}
