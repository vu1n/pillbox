//! Built-in registry of known secret names.
//!
//! When a user runs `pillbox secret add ANTHROPIC_API_KEY --vault`, we
//! need to know which host the secret talks to, which auth header scheme
//! is used, and what shape a real value (and therefore the stub) takes.
//! For the common API keys we ship those mappings with pillbox so the
//! user doesn't have to spell out `--host` / `--header-scheme` / `--prefix`
//! every time.
//!
//! Unknown names are still allowed via `--vault --host … --header-scheme …
//! --prefix …`. Aliases (`GH_TOKEN` → `GITHUB_TOKEN`) resolve to the
//! canonical entry so we don't duplicate metadata.

use serde::{Deserialize, Serialize};

/// Which Authorization scheme the upstream uses.
///
/// `XApiKey` puts the bearer in the `x-api-key` header (Anthropic style).
/// `AuthorizationBearer` is the conventional `Authorization: Bearer <key>`
/// (OpenAI, modern GitHub).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum HeaderScheme {
    /// `x-api-key: <value>`
    XApiKey,
    /// `Authorization: Bearer <value>`
    AuthorizationBearer,
}

impl HeaderScheme {
    pub(crate) fn parse(s: &str) -> Result<Self, String> {
        match s {
            "x-api-key" => Ok(Self::XApiKey),
            "authorization-bearer" => Ok(Self::AuthorizationBearer),
            other => Err(format!(
                "unknown header scheme `{other}` (expected `x-api-key` or `authorization-bearer`)"
            )),
        }
    }

    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::XApiKey => "x-api-key",
            Self::AuthorizationBearer => "authorization-bearer",
        }
    }
}

/// Vault metadata for a single secret. Persisted as
/// `~/.pillbox/secrets/<name>.meta.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct VaultMeta {
    /// Schema version. Bump on any breaking change.
    pub(crate) version: u32,
    pub(crate) vault: VaultMetaBody,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct VaultMetaBody {
    /// Host the secret talks to (e.g. `api.anthropic.com`). Matches what
    /// some vault provider's `intercept` accepts; if no provider claims
    /// it, the stub-swap will never fire at request time.
    pub(crate) host: String,
    /// Which header carries the secret.
    pub(crate) header_scheme: HeaderScheme,
    /// Real-key prefix the stub should mimic (e.g. `sk-ant-api03-`).
    pub(crate) prefix: String,
}

impl VaultMeta {
    pub(crate) fn new(host: String, header_scheme: HeaderScheme, prefix: String) -> Self {
        Self {
            version: 1,
            vault: VaultMetaBody {
                host,
                header_scheme,
                prefix,
            },
        }
    }
}

/// A built-in known secret entry.
#[derive(Clone, Copy, Debug)]
pub(crate) struct KnownSecret {
    /// Canonical name (e.g. `ANTHROPIC_API_KEY`).
    pub(crate) name: &'static str,
    pub(crate) host: &'static str,
    pub(crate) header_scheme: HeaderScheme,
    pub(crate) prefix: &'static str,
}

impl KnownSecret {
    /// `KnownSecret` is `Copy`, so taking `self` by value is cheap and
    /// matches clippy's `wrong_self_convention` lint for `to_*` methods.
    pub(crate) fn to_meta(self) -> VaultMeta {
        VaultMeta::new(
            self.host.to_string(),
            self.header_scheme,
            self.prefix.to_string(),
        )
    }
}

// Canonical registry. Aliases resolve through `lookup` so the metadata
// stays in one place.
const KNOWN: &[KnownSecret] = &[
    KnownSecret {
        name: "ANTHROPIC_API_KEY",
        host: "api.anthropic.com",
        header_scheme: HeaderScheme::XApiKey,
        prefix: "sk-ant-api03-",
    },
    KnownSecret {
        name: "OPENAI_API_KEY",
        host: "api.openai.com",
        header_scheme: HeaderScheme::AuthorizationBearer,
        prefix: "sk-",
    },
    KnownSecret {
        name: "GITHUB_TOKEN",
        host: "api.github.com",
        header_scheme: HeaderScheme::AuthorizationBearer,
        prefix: "ghp_",
    },
];

/// Aliases that resolve to a canonical name. Keep small — most users
/// should learn the canonical name.
const ALIASES: &[(&str, &str)] = &[
    // `gh` CLI uses GH_TOKEN; many actions docs prefer GITHUB_TOKEN.
    ("GH_TOKEN", "GITHUB_TOKEN"),
];

/// Look up a known secret by name (or alias).
pub(crate) fn lookup(name: &str) -> Option<KnownSecret> {
    let canonical = ALIASES
        .iter()
        .find(|(alias, _)| *alias == name)
        .map(|(_, target)| *target)
        .unwrap_or(name);
    KNOWN.iter().find(|k| k.name == canonical).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_finds_canonical_names() {
        let ak = lookup("ANTHROPIC_API_KEY").expect("anthropic known");
        assert_eq!(ak.host, "api.anthropic.com");
        assert_eq!(ak.header_scheme, HeaderScheme::XApiKey);
        assert_eq!(ak.prefix, "sk-ant-api03-");

        let oa = lookup("OPENAI_API_KEY").expect("openai known");
        assert_eq!(oa.host, "api.openai.com");
        assert_eq!(oa.header_scheme, HeaderScheme::AuthorizationBearer);

        let gh = lookup("GITHUB_TOKEN").expect("github known");
        assert_eq!(gh.host, "api.github.com");
        assert_eq!(gh.prefix, "ghp_");
    }

    #[test]
    fn lookup_resolves_aliases() {
        let gh = lookup("GH_TOKEN").expect("gh_token alias");
        // Should resolve to GITHUB_TOKEN's row.
        assert_eq!(gh.name, "GITHUB_TOKEN");
        assert_eq!(gh.host, "api.github.com");
    }

    #[test]
    fn lookup_misses_unknown_names() {
        assert!(lookup("MY_CUSTOM_API_KEY").is_none());
        assert!(lookup("").is_none());
    }

    #[test]
    fn header_scheme_round_trips_through_string() {
        assert_eq!(
            HeaderScheme::parse("x-api-key").unwrap(),
            HeaderScheme::XApiKey
        );
        assert_eq!(
            HeaderScheme::parse("authorization-bearer").unwrap(),
            HeaderScheme::AuthorizationBearer
        );
        assert!(HeaderScheme::parse("oauth-other").is_err());

        assert_eq!(HeaderScheme::XApiKey.as_str(), "x-api-key");
        assert_eq!(
            HeaderScheme::AuthorizationBearer.as_str(),
            "authorization-bearer"
        );
    }

    #[test]
    fn vault_meta_round_trips_through_json() {
        let meta = VaultMeta::new(
            "api.anthropic.com".into(),
            HeaderScheme::XApiKey,
            "sk-ant-api03-".into(),
        );
        let s = serde_json::to_string(&meta).expect("serialize");
        let back: VaultMeta = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(back, meta);
        assert_eq!(back.version, 1);
        assert_eq!(back.vault.header_scheme, HeaderScheme::XApiKey);
    }
}
