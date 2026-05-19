//! `pillbox secret …` subcommands.
//!
//! Storage: one file per secret at `~/.pillbox/secrets/<name>` (0600).
//! Same posture as `~/.aws/credentials`, `~/.docker/config.json`, etc. —
//! see AGENTS.md "What pillbox is NOT" for the threat-model honesty.
//!
//! Idempotent by default: `secret add` overwrites silently. Pass
//! `--if-not-exists` to fail when the secret is already present (lets
//! agents do "create-only" setup).
//!
//! Reveal model: `secret show` masks values by default. `--reveal` plus
//! a TTY on stdout unmasks. Piping `--reveal` somewhere refuses unless
//! the caller adds `--to-stdout` to ack the leak.

use std::{
    fs,
    io::{IsTerminal, Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
};

use anyhow::{Context, Result};

use crate::errors::PillboxError;
use crate::paths::validate_name;
use crate::vault::VaultMeta;

/// `~/.pillbox/secrets/` — created on first use, 0700.
fn secrets_dir() -> Result<PathBuf> {
    crate::paths::data_subdir("secrets")
}

fn secret_path(name: &str) -> Result<PathBuf> {
    Ok(secrets_dir()?.join(name))
}

/// Path to the sidecar `<name>.meta.json` (vault metadata for the secret).
/// Always under the same dir as the value file so a missing parent means
/// the secret itself doesn't exist either.
fn meta_path(name: &str) -> Result<PathBuf> {
    Ok(secrets_dir()?.join(format!("{name}.meta.json")))
}

/// Read a secret's stored value. Returns `None` if not present.
pub(crate) fn read(name: &str) -> Result<Option<String>> {
    validate_name("secret read", name)?;
    let path = secret_path(name)?;
    match fs::read_to_string(&path) {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Names of every stored secret, sorted. Sidecar `.meta.json` files are
/// filtered out — they're metadata, not secrets themselves.
pub(crate) fn names() -> Result<Vec<String>> {
    let dir = secrets_dir()?;
    let mut out = Vec::new();
    for entry in fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            if let Some(name) = entry.file_name().to_str() {
                if name.ends_with(".meta.json") {
                    continue;
                }
                out.push(name.to_string());
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Read the vault metadata sidecar for `name`. Returns `None` when the
/// secret is not vaulted (no sidecar present). A malformed sidecar is a
/// hard config error — silently ignoring it would defeat the vault.
pub(crate) fn read_meta(name: &str) -> Result<Option<VaultMeta>> {
    validate_name("secret read", name)?;
    let path = meta_path(name)?;
    let raw = match fs::read_to_string(&path) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let meta: VaultMeta = serde_json::from_str(&raw).map_err(|e| {
        PillboxError::config("secret read", format!("parse {}: {e}", path.display()))
    })?;
    Ok(Some(meta))
}

/// Write the vault metadata sidecar for `name`.
pub(crate) fn write_meta(name: &str, meta: &VaultMeta) -> Result<()> {
    validate_name("secret meta", name)?;
    let path = meta_path(name)?;
    let body = serde_json::to_string_pretty(meta)
        .with_context(|| format!("serialize meta for `{name}`"))?;
    write_secret_file(&path, &body)
}

/// Convenience: load vault metadata for `name`, silently returning
/// `None` if anything fails (used on hot paths where a corrupt sidecar
/// shouldn't crash a normal `--with` injection).
pub(crate) fn vault_meta_for(name: &str) -> Option<VaultMeta> {
    read_meta(name).ok().flatten()
}

pub(crate) fn add(
    name: &str,
    source: AddSource,
    if_not_exists: bool,
    vault_meta: Option<VaultMeta>,
) -> Result<()> {
    validate_name("secret add", name)?;
    let path = secret_path(name)?;
    if if_not_exists && path.exists() {
        return Err(
            PillboxError::runtime("secret add", format!("`{name}` already exists"))
                .with_next(format!(
                    "pillbox secret rm {name}  # then re-add  (or drop --if-not-exists)"
                ))
                .into(),
        );
    }
    let value = read_value(name, source)?;
    write_secret_file(&path, &value)?;
    if let Some(meta) = vault_meta.as_ref() {
        write_meta(name, meta)?;
    } else {
        // No `--vault` this time: clean up any stale sidecar from a prior
        // vault-mode add so the secret reverts to "not vaulted". Quietly
        // ignore "not found" — that's the common no-op case.
        let m = meta_path(name)?;
        match fs::remove_file(&m) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| format!("remove {}", m.display()));
            }
        }
    }
    if let Some(meta) = vault_meta {
        println!(
            "pillbox: ✓ secret `{name}` stored at {} (vaulted: {} / {})",
            path.display(),
            meta.vault.host,
            meta.vault.header_scheme.as_str()
        );
    } else {
        println!("pillbox: ✓ secret `{name}` stored at {}", path.display());
    }
    Ok(())
}

pub(crate) fn list(json: bool) -> Result<()> {
    let names = names()?;
    if json {
        let payload = build_list_json(&names)?;
        println!("{payload}");
        return Ok(());
    }
    if names.is_empty() {
        println!("(no secrets stored)");
        println!();
        println!("Add one with: pillbox secret add <NAME>");
        return Ok(());
    }
    println!("Stored secrets under ~/.pillbox/secrets/:");
    for name in names {
        match vault_meta_for(&name) {
            Some(meta) => println!(
                "  {name}  (vaulted: {} / {})",
                meta.vault.host,
                meta.vault.header_scheme.as_str()
            ),
            None => println!("  {name}"),
        }
    }
    Ok(())
}

pub(crate) fn show(name: &str, reveal: bool, to_stdout: bool, json: bool) -> Result<()> {
    let value = read(name)?.ok_or_else(|| {
        PillboxError::runtime("secret show", format!("`{name}` not found"))
            .with_next(format!("pillbox secret add {name}"))
    })?;
    let meta = read_meta(name)?;
    let display = if reveal {
        if !std::io::stdout().is_terminal() && !to_stdout {
            return Err(PillboxError::usage(
                "secret show",
                "refusing to reveal secret to non-TTY stdout (would leak into a pipe/log)",
            )
            .with_next(format!(
                "pillbox secret show {name} --reveal --to-stdout  # only if you really mean it"
            ))
            .into());
        }
        value.clone()
    } else {
        mask(&value)
    };
    if json {
        println!("{}", build_show_json(name, &display, reveal, meta.as_ref()));
    } else {
        match meta.as_ref() {
            Some(m) => println!(
                "{name}={display}  (vaulted: {} / {})",
                m.vault.host,
                m.vault.header_scheme.as_str()
            ),
            None => println!("{name}={display}"),
        }
    }
    Ok(())
}

pub(crate) fn rm(name: &str) -> Result<()> {
    validate_name("secret rm", name)?;
    let path = secret_path(name)?;
    let removed = match fs::remove_file(&path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(e).with_context(|| format!("remove {}", path.display())),
    };
    // Also clean up the sidecar metadata if it exists. Independent of the
    // value being there — a stale sidecar without a value is invalid state
    // we want to be tolerant of.
    let m = meta_path(name)?;
    match fs::remove_file(&m) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("remove {}", m.display())),
    }
    if removed {
        println!("pillbox: ✓ secret `{name}` removed");
    } else {
        println!("(no secret named `{name}` was stored)");
    }
    Ok(())
}

/// Source for `secret add`: stdin or a host env var.
#[derive(Debug)]
pub(crate) enum AddSource {
    Stdin,
    EnvVar(String),
}

fn read_value(name: &str, source: AddSource) -> Result<String> {
    let raw = match source {
        AddSource::Stdin => {
            if std::io::stdin().is_terminal() {
                eprint!("paste value for `{name}` (then Ctrl-D): ");
            }
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("read value from stdin")?;
            buf
        }
        AddSource::EnvVar(var) => std::env::var(&var).map_err(|_| {
            PillboxError::usage(
                "secret add",
                format!("env var `{var}` is not set on the host"),
            )
        })?,
    };
    // Trim trailing whitespace (the universal CLI gotcha — copied tokens
    // often pick up a newline from clipboards/heredocs). Leading
    // whitespace is preserved in case it's load-bearing.
    Ok(raw.trim_end().to_string())
}

fn write_secret_file(path: &std::path::Path, value: &str) -> Result<()> {
    // Write at 0600 from inode creation. Truncate any pre-existing file
    // (idempotent overwrite — that's the documented contract).
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("open {} for write", path.display()))?;
    file.write_all(value.as_bytes())
        .with_context(|| format!("write to {}", path.display()))?;
    Ok(())
}

/// Mask all but the last 4 chars of `value`. Short values are fully
/// masked. Trailing whitespace is ignored when deciding what counts as
/// "last 4 chars" so newline-suffixed tokens don't leak.
///
/// Shared with envs.rs.
pub(crate) fn mask(value: &str) -> String {
    let core = value.trim_end();
    let chars: Vec<char> = core.chars().collect();
    if chars.len() <= 4 {
        return "*".repeat(chars.len().max(3));
    }
    let mut out = String::new();
    for _ in 0..chars.len().saturating_sub(4) {
        out.push('*');
    }
    out.extend(&chars[chars.len() - 4..]);
    out
}

// ── JSON output ─────────────────────────────────────────────────────────────

fn build_list_json(names: &[String]) -> Result<String> {
    let mut arr: Vec<serde_json::Value> = Vec::with_capacity(names.len());
    for n in names {
        let mut o = serde_json::Map::new();
        o.insert("name".into(), serde_json::Value::String(n.clone()));
        if let Some(meta) = read_meta(n)? {
            o.insert("vault".into(), vault_meta_json_short(&meta));
        }
        arr.push(serde_json::Value::Object(o));
    }
    Ok(crate::paths::json_v1(vec![(
        "secrets",
        serde_json::Value::Array(arr),
    )]))
}

fn build_show_json(name: &str, value: &str, revealed: bool, meta: Option<&VaultMeta>) -> String {
    let mut fields: Vec<(&'static str, serde_json::Value)> = vec![
        ("name", serde_json::Value::String(name.into())),
        ("value", serde_json::Value::String(value.into())),
        ("revealed", serde_json::Value::Bool(revealed)),
    ];
    if let Some(m) = meta {
        fields.push(("vault", vault_meta_json_short(m)));
    }
    crate::paths::json_v1(fields)
}

/// Compact JSON projection of `VaultMeta` for `--json` outputs. Only the
/// two fields callers actually consume (host + header scheme); the prefix
/// is an implementation detail of the stub minter.
fn vault_meta_json_short(meta: &VaultMeta) -> serde_json::Value {
    let mut o = serde_json::Map::new();
    o.insert(
        "host".into(),
        serde_json::Value::String(meta.vault.host.clone()),
    );
    o.insert(
        "scheme".into(),
        serde_json::Value::String(meta.vault.header_scheme.as_str().into()),
    );
    serde_json::Value::Object(o)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::with_isolated_home;
    use crate::vault::HeaderScheme;

    #[test]
    fn mask_short_values_fully() {
        assert_eq!(mask("abc"), "***");
        assert_eq!(mask("abcd"), "****");
    }

    #[test]
    fn mask_shows_last_four_chars() {
        assert_eq!(mask("abcdefgh"), "****efgh");
        assert_eq!(
            mask("sk-ant-api03-secretvalue123"),
            "***********************e123"
        );
    }

    #[test]
    fn mask_ignores_trailing_whitespace() {
        assert_eq!(mask("abcdefgh\n"), "****efgh");
    }

    #[test]
    fn vault_meta_round_trip_via_disk() {
        with_isolated_home("secrets-meta", || {
            let name = "TEST_META_KEY";
            let meta = VaultMeta::new(
                "api.example.com".into(),
                HeaderScheme::XApiKey,
                "ex-".into(),
            );
            write_meta(name, &meta).unwrap();
            let back = read_meta(name).unwrap().expect("meta present");
            assert_eq!(back, meta);
        });
    }

    #[test]
    fn missing_meta_means_not_vaulted() {
        with_isolated_home("secrets-nometa", || {
            // Even with the parent dir present, no .meta.json = None.
            let name = "TEST_NO_META";
            // Touch the secret file so secrets_dir() exists.
            let dir = secrets_dir().unwrap();
            std::fs::write(dir.join(name), b"x").unwrap();
            assert!(read_meta(name).unwrap().is_none());
            assert!(vault_meta_for(name).is_none());
        });
    }

    #[test]
    fn list_skips_sidecar_files() {
        with_isolated_home("secrets-list", || {
            let dir = secrets_dir().unwrap();
            std::fs::write(dir.join("ALPHA"), b"v").unwrap();
            std::fs::write(dir.join("ALPHA.meta.json"), b"{}").unwrap();
            std::fs::write(dir.join("BRAVO"), b"v").unwrap();

            let names = names().unwrap();
            assert_eq!(names, vec!["ALPHA".to_string(), "BRAVO".to_string()]);
        });
    }
}
