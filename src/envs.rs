//! `pillbox env …` — named bundles of environment variables, sourced
//! from `.env`-formatted files.
//!
//! Storage: one file per bundle at `~/.pillbox/env/<name>` (0600). The
//! stored file is the `.env` content verbatim (after we've validated
//! it parses).
//!
//! The parser is intentionally minimal — KEY=VALUE per line, `#`
//! comments, blank lines, optional `export` prefix, single/double quotes
//! around values. No variable interpolation, no command substitution,
//! no multi-line values. That's enough for the 95% case and predictable
//! across machines.

use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use crate::errors::PillboxError;
use crate::paths::validate_name;

fn env_dir() -> Result<PathBuf> {
    crate::paths::data_subdir("env")
}

fn bundle_path(name: &str) -> Result<PathBuf> {
    Ok(env_dir()?.join(name))
}

/// Read a bundle's parsed key/value pairs.
pub(crate) fn read(name: &str) -> Result<Option<BTreeMap<String, String>>> {
    validate_name("env show", name)?;
    let path = bundle_path(name)?;
    let content = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    parse_dotenv(&content, &path.display().to_string()).map(Some)
}

pub(crate) fn names() -> Result<Vec<String>> {
    let dir = env_dir()?;
    let mut out = Vec::new();
    for entry in fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            if let Some(name) = entry.file_name().to_str() {
                out.push(name.to_string());
            }
        }
    }
    out.sort();
    Ok(out)
}

pub(crate) fn load(name: &str, source_path: &Path, if_not_exists: bool) -> Result<()> {
    validate_name("env load", name)?;
    let dest = bundle_path(name)?;
    if if_not_exists && dest.exists() {
        return Err(PillboxError::runtime(
            "env load",
            format!("bundle `{name}` already exists"),
        )
        .with_next(format!(
            "pillbox env rm {name}  # then re-load  (or drop --if-not-exists)"
        ))
        .into());
    }
    let content = fs::read_to_string(source_path).map_err(|e| {
        PillboxError::runtime(
            "env load",
            format!("could not read {}: {e}", source_path.display()),
        )
    })?;
    let parsed = parse_dotenv(&content, &source_path.display().to_string())?;
    write_bundle_file(&dest, &content)?;
    println!(
        "pillbox: ✓ env bundle `{name}` stored ({} variables) at {}",
        parsed.len(),
        dest.display()
    );
    Ok(())
}

pub(crate) fn list(json: bool) -> Result<()> {
    let names = names()?;
    if json {
        println!("{}", build_list_json(&names)?);
        return Ok(());
    }
    if names.is_empty() {
        println!("(no env bundles stored)");
        println!();
        println!("Load one with: pillbox env load <NAME> <PATH>");
        return Ok(());
    }
    println!("Stored env bundles under ~/.pillbox/env/:");
    for name in &names {
        let count = read(name)?
            .map(|m| m.len())
            .unwrap_or(0);
        println!("  {name:<20} ({count} variables)");
    }
    Ok(())
}

pub(crate) fn show(name: &str, reveal: bool, to_stdout: bool, json: bool) -> Result<()> {
    let vars = read(name)?.ok_or_else(|| {
        PillboxError::runtime("env show", format!("bundle `{name}` not found"))
            .with_next(format!(
                "pillbox env load {name} <PATH>  # load one from a .env file"
            ))
    })?;
    if reveal && !std::io::IsTerminal::is_terminal(&std::io::stdout()) && !to_stdout {
        return Err(PillboxError::usage(
            "env show",
            "refusing to reveal env bundle to non-TTY stdout (would leak into a pipe/log)",
        )
        .with_next(format!(
            "pillbox env show {name} --reveal --to-stdout  # only if you really mean it"
        ))
        .into());
    }
    if json {
        println!("{}", build_show_json(name, &vars, reveal)?);
        return Ok(());
    }
    println!("Bundle `{name}`:");
    for (k, v) in &vars {
        let display = if reveal { v.clone() } else { mask(v) };
        println!("  {k}={display}");
    }
    Ok(())
}

pub(crate) fn rm(name: &str) -> Result<()> {
    validate_name("env rm", name)?;
    let path = bundle_path(name)?;
    match fs::remove_file(&path) {
        Ok(()) => {
            println!("pillbox: ✓ env bundle `{name}` removed");
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("(no env bundle named `{name}` was stored)");
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("remove {}", path.display())),
    }
}

// ── .env parsing ────────────────────────────────────────────────────────────

/// Minimal `.env` parser. Documented grammar:
///   - One assignment per line. Lines may have leading whitespace.
///   - `# ...` after whitespace = comment; the rest of the line is ignored.
///   - Blank lines ignored.
///   - Optional leading `export ` is allowed and dropped.
///   - Key: ASCII alphanumeric + `_`, must start with a letter or `_`.
///   - Value: from the first `=` to end of line. If wrapped in single
///     OR double quotes, the quotes are stripped (one quote pair only).
///     No escape sequences, no interpolation, no multi-line.
pub(crate) fn parse_dotenv(content: &str, source: &str) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for (lineno, raw) in content.lines().enumerate() {
        let lineno = lineno + 1;
        let line = raw.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some(eq) = line.find('=') else {
            return Err(PillboxError::config(
                "env parse",
                format!("{source}:{lineno}: expected KEY=VALUE, got `{raw}`"),
            )
            .into());
        };
        let key = line[..eq].trim().to_string();
        if !is_valid_env_key(&key) {
            return Err(PillboxError::config(
                "env parse",
                format!("{source}:{lineno}: invalid env var name `{key}`"),
            )
            .into());
        }
        let raw_value = &line[eq + 1..];
        let value = unquote(raw_value);
        out.insert(key, value);
    }
    Ok(out)
}

fn is_valid_env_key(k: &str) -> bool {
    let mut chars = k.chars();
    let Some(first) = chars.next() else { return false };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn unquote(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.len() >= 2 {
        let bytes = trimmed.as_bytes();
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return trimmed[1..trimmed.len() - 1].to_string();
        }
    }
    trimmed.to_string()
}

fn write_bundle_file(path: &Path, content: &str) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("open {} for write", path.display()))?;
    file.write_all(content.as_bytes())
        .with_context(|| format!("write to {}", path.display()))?;
    Ok(())
}

fn mask(value: &str) -> String {
    crate::secrets::mask(value)
}

// ── JSON output ─────────────────────────────────────────────────────────────

fn build_list_json(names: &[String]) -> Result<String> {
    let mut bundles = Vec::new();
    for name in names {
        let count = read(name)?.map(|m| m.len()).unwrap_or(0);
        let mut o = serde_json::Map::new();
        o.insert("name".into(), serde_json::Value::String(name.clone()));
        o.insert(
            "variable_count".into(),
            serde_json::Value::Number(count.into()),
        );
        bundles.push(serde_json::Value::Object(o));
    }
    let mut root = serde_json::Map::new();
    root.insert("version".into(), serde_json::Value::Number(1.into()));
    root.insert("bundles".into(), serde_json::Value::Array(bundles));
    Ok(serde_json::Value::Object(root).to_string())
}

fn build_show_json(
    name: &str,
    vars: &BTreeMap<String, String>,
    revealed: bool,
) -> Result<String> {
    let variables: Vec<serde_json::Value> = vars
        .iter()
        .map(|(k, v)| {
            let display = if revealed { v.clone() } else { mask(v) };
            let mut o = serde_json::Map::new();
            o.insert("key".into(), serde_json::Value::String(k.clone()));
            o.insert("value".into(), serde_json::Value::String(display));
            serde_json::Value::Object(o)
        })
        .collect();
    let mut root = serde_json::Map::new();
    root.insert("version".into(), serde_json::Value::Number(1.into()));
    root.insert("name".into(), serde_json::Value::String(name.into()));
    root.insert("revealed".into(), serde_json::Value::Bool(revealed));
    root.insert("variables".into(), serde_json::Value::Array(variables));
    Ok(serde_json::Value::Object(root).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_kv_lines() {
        let out = parse_dotenv("FOO=bar\nBAZ=qux\n", "test").unwrap();
        assert_eq!(out.get("FOO").unwrap(), "bar");
        assert_eq!(out.get("BAZ").unwrap(), "qux");
    }

    #[test]
    fn ignores_comments_and_blanks() {
        let out = parse_dotenv("# top comment\n\nFOO=bar\n  # indented comment\nBAZ=qux\n", "t")
            .unwrap();
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn strips_export_prefix() {
        let out = parse_dotenv("export FOO=bar\n", "t").unwrap();
        assert_eq!(out.get("FOO").unwrap(), "bar");
    }

    #[test]
    fn unwraps_quoted_values() {
        let out =
            parse_dotenv("A=\"hello world\"\nB='single quoted'\nC=plain\n", "t").unwrap();
        assert_eq!(out.get("A").unwrap(), "hello world");
        assert_eq!(out.get("B").unwrap(), "single quoted");
        assert_eq!(out.get("C").unwrap(), "plain");
    }

    #[test]
    fn rejects_no_equals() {
        let err = parse_dotenv("JUST_A_NAME\n", "t").unwrap_err();
        assert!(format!("{err}").contains("expected KEY=VALUE"));
    }

    #[test]
    fn rejects_invalid_keys() {
        assert!(parse_dotenv("9LEADING_DIGIT=x\n", "t").is_err());
        assert!(parse_dotenv("HAS-DASH=x\n", "t").is_err());
    }
}
