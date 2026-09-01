//! `pillbox secret …` handlers — add/list/show/rm the per-pillbox
//! secret store. Vault-aware: secrets marked `--vault` carry the
//! metadata the stub-swap proxy needs at injection time.

use anyhow::Result;

use crate::cli::SecretAction;
use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::WriteScope;
use crate::{secrets, vault};

pub(crate) fn dispatch(resolved: &Pillbox, action: SecretAction) -> Result<()> {
    match action {
        SecretAction::Add {
            name,
            from_env,
            if_not_exists,
            global,
            vault,
            maps_to,
            host,
            header_scheme,
            prefix,
        } => {
            let source = match from_env {
                Some(var) => secrets::AddSource::EnvVar(var),
                None => secrets::AddSource::Stdin,
            };
            let vault_meta = resolve_vault_meta(
                &name,
                vault,
                maps_to.as_deref(),
                host.as_deref(),
                header_scheme.as_deref(),
                prefix.as_deref(),
            )?;
            secrets::add(
                resolved,
                WriteScope::from_global_flag(global),
                &name,
                source,
                if_not_exists,
                vault_meta,
            )
        }
        SecretAction::List { json } => secrets::list(resolved, json),
        SecretAction::Show {
            name,
            reveal,
            to_stdout,
            json,
        } => secrets::show(resolved, &name, reveal, to_stdout, json),
        SecretAction::Rm { name, global } => {
            secrets::rm(resolved, WriteScope::from_global_flag(global), &name)
        }
    }
}

/// Resolve `pillbox secret add --vault …` CLI flags into the
/// `vault::VaultMeta` the secrets layer persists. Lives here (not in
/// `commands::vault`) because the validation rules are specific to
/// the secret-add CLI surface; the vault module just consumes the
/// resulting `VaultMeta` value object.
#[allow(clippy::too_many_arguments)]
fn resolve_vault_meta(
    name: &str,
    vault: bool,
    maps_to: Option<&str>,
    host: Option<&str>,
    header_scheme: Option<&str>,
    prefix: Option<&str>,
) -> Result<Option<vault::VaultMeta>> {
    if !vault {
        if maps_to.is_some() || host.is_some() || header_scheme.is_some() || prefix.is_some() {
            return Err(PillboxError::usage(
                "secret add",
                "--maps-to / --host / --header-scheme / --prefix require --vault",
            )
            .into());
        }
        return Ok(None);
    }

    if let Some(alias) = maps_to {
        let known = vault::known_secrets::lookup(alias).ok_or_else(|| {
            PillboxError::usage(
                "secret add",
                format!(
                    "--maps-to `{alias}` is not a known secret name. \
                     Known: ANTHROPIC_API_KEY, OPENAI_API_KEY, GITHUB_TOKEN (alias GH_TOKEN)"
                ),
            )
        })?;
        return Ok(Some(known.to_meta()));
    }

    let manual_count = [host.is_some(), header_scheme.is_some(), prefix.is_some()]
        .iter()
        .filter(|b| **b)
        .count();

    if manual_count == 0 {
        let known = vault::known_secrets::lookup(name).ok_or_else(|| {
            PillboxError::usage(
                "secret add",
                format!(
                    "`{name}` is not a known secret. Pass `--maps-to KNOWN` to alias \
                     it, or `--host H --header-scheme {{x-api-key|authorization-bearer}} --prefix P` \
                     to spell out the vault config."
                ),
            )
            .with_next(format!(
                "pillbox secret add {name} --vault --maps-to ANTHROPIC_API_KEY"
            ))
        })?;
        return Ok(Some(known.to_meta()));
    }

    if manual_count != 3 {
        return Err(PillboxError::usage(
            "secret add",
            "--host, --header-scheme, and --prefix must all be passed together",
        )
        .into());
    }

    // The vault swaps stub→real only when the request's destination host matches
    // this `host` exactly (the host-bound resolver). An empty or URL-shaped host
    // can never match a bare request host, so the secret would store fine but
    // silently never vault — fail closed *here*, at creation, not at swap time.
    let host = validate_vault_host(host.unwrap())?;
    let scheme = vault::HeaderScheme::parse(header_scheme.unwrap())
        .map_err(|e| PillboxError::usage("secret add", e))?;
    Ok(Some(vault::VaultMeta::new(
        host,
        scheme,
        prefix.unwrap().to_string(),
    )))
}

/// Reject a `--host` that can't match a request's destination host. The vault
/// compares against a bare host (`api.example.com`, optionally `:port`), so a
/// scheme, path, or whitespace guarantees the swap never fires.
fn validate_vault_host(host: &str) -> Result<String> {
    let trimmed = host.trim();
    let malformed = trimmed.is_empty()
        || trimmed.contains("://")
        || trimmed.contains('/')
        || trimmed.chars().any(char::is_whitespace);
    if malformed {
        return Err(PillboxError::usage(
            "secret add",
            format!(
                "--host `{host}` must be a bare host like `api.example.com` (no scheme, path, or \
                 spaces) — the vault matches it against the request's destination host exactly"
            ),
        )
        .into());
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_vault_host_accepts_bare_host_and_trims() {
        assert_eq!(
            validate_vault_host("api.example.com").unwrap(),
            "api.example.com"
        );
        // Surrounding whitespace is a paste artifact, not part of the host.
        assert_eq!(
            validate_vault_host("  api.example.com  ").unwrap(),
            "api.example.com"
        );
        // A port is a legitimate bare-host form (self-hosted endpoints).
        assert_eq!(
            validate_vault_host("model.local:8443").unwrap(),
            "model.local:8443"
        );
    }

    #[test]
    fn validate_vault_host_rejects_unmatchable_hosts() {
        // Each of these can never equal a request's bare destination host, so
        // the vault would silently never swap — reject at creation instead.
        for bad in [
            "",
            "   ",
            "https://api.example.com",
            "api.example.com/v1",
            "api example.com",
        ] {
            assert!(
                validate_vault_host(bad).is_err(),
                "expected `{bad}` to be rejected"
            );
        }
    }

    /// The manual `--host/--header-scheme/--prefix` path runs the host guard:
    /// an empty host is a usage error, not a stored-but-dead vault config.
    #[test]
    fn resolve_vault_meta_rejects_empty_manual_host() {
        let err = resolve_vault_meta(
            "CUSTOM_KEY",
            true,
            None,
            Some(""),
            Some("authorization-bearer"),
            Some("sk-"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("--host"), "got: {err}");
    }

    #[test]
    fn resolve_vault_meta_accepts_valid_manual_host() {
        let meta = resolve_vault_meta(
            "CUSTOM_KEY",
            true,
            None,
            Some("api.example.com"),
            Some("authorization-bearer"),
            Some("sk-"),
        )
        .unwrap()
        .expect("manual vault meta");
        assert_eq!(meta.vault.host, "api.example.com");
    }
}
