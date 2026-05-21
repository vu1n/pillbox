//! `pillbox env …` — named bundles of environment variables, scoped to
//! the current pillbox.
//!
//! Storage: one file per bundle at `<pillbox>/env/<name>` (0600). Reads
//! merge global + project (project shadows global on key conflict).
//! Writes default to project; `--global` forces global.
//!
//! The parser is intentionally minimal — KEY=VALUE per line, `#` comments,
//! blank lines, optional `export` prefix, single/double quotes around
//! values. No interpolation, no command substitution, no multi-line. Same
//! grammar as v0.5.

use std::{collections::BTreeMap, fs, path::Path};

use anyhow::{Context, Result};

use crate::errors::PillboxError;
use crate::paths::validate_name;
use crate::pillbox::{Pillbox, Scope, WriteScope};
use crate::registry::{self as reg, InheritedRegistry, Registry};

/// Registry plumbing for env bundles. Records are raw file bodies
/// (a `String`); the `parse_dotenv` KV decode happens at the call
/// site because some callers (run-time injection) want the parsed
/// map while `show` / `list` are happy with the raw text.
struct EnvRegistry;
impl Registry for EnvRegistry {
    type Record = String;
    const SUBDIR: &'static str = "env";
    fn read_action() -> &'static str {
        "env show"
    }
    fn filename(name: &str) -> String {
        // Raw name on disk — env bundles don't append an extension so
        // a user can `cat <name>` directly. Anything that lands in
        // `env/` is assumed to be a bundle; no sidecar files like
        // secrets has.
        name.to_string()
    }
    fn parse(raw: &str, _source: &Path) -> Result<Self::Record> {
        Ok(raw.to_string())
    }
}
impl InheritedRegistry for EnvRegistry {}

/// Read a bundle's parsed key/value pairs, walking project → global.
/// Returns the first scope that has the bundle. Bundles are atomic — we
/// don't merge KV pairs across scopes; one full file wins.
pub(crate) fn read(resolved: &Pillbox, name: &str) -> Result<Option<BTreeMap<String, String>>> {
    validate_name("env show", name)?;
    for pb in resolved.read_chain() {
        let path = EnvRegistry::path_read(&pb, name);
        let content = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(e).with_context(|| format!("read {}", path.display()));
            }
        };
        return parse_dotenv(&content, &path.display().to_string()).map(Some);
    }
    Ok(None)
}

#[derive(Debug, Clone)]
pub(crate) struct MergedBundle {
    pub(crate) name: String,
    pub(crate) scope: String,
    pub(crate) from_project: bool,
}

/// Just the filenames + scope tags — `list` only needs to know which
/// bundles exist. Skips `registry::list_merged` because that reads
/// every record's file body up-front; `pillbox env list` only prints
/// names + counts (and re-reads on demand via `read()`).
pub(crate) fn names_merged(resolved: &Pillbox) -> Result<Vec<MergedBundle>> {
    let mut map: BTreeMap<String, MergedBundle> = BTreeMap::new();
    for pb in resolved.read_chain() {
        let dir = EnvRegistry::dir_read(&pb);
        if !dir.exists() {
            continue;
        }
        for entry in fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let fname = match entry.file_name().into_string() {
                Ok(s) => s,
                Err(_) => continue,
            };
            map.entry(fname.clone()).or_insert(MergedBundle {
                name: fname,
                scope: pb.display_name().to_string(),
                from_project: matches!(pb.scope, Scope::Project { .. }),
            });
        }
    }
    Ok(map.into_values().collect())
}

pub(crate) fn load(
    resolved: &Pillbox,
    scope: WriteScope,
    name: &str,
    source_path: &Path,
    if_not_exists: bool,
) -> Result<()> {
    validate_name("env load", name)?;
    let target = resolved.write_target(scope);
    let dest = EnvRegistry::path(&target, name)?;
    if if_not_exists && dest.exists() {
        return Err(PillboxError::runtime(
            "env load",
            format!(
                "bundle `{name}` already exists in `{}`",
                target.display_name()
            ),
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
    reg::write_record::<EnvRegistry>(&target, name, content.as_bytes())?;
    println!(
        "pillbox: ✓ env bundle `{name}` stored in `{}` ({} variables) at {}",
        target.display_name(),
        parsed.len(),
        dest.display()
    );
    Ok(())
}

pub(crate) fn list(resolved: &Pillbox, json: bool) -> Result<()> {
    let names = names_merged(resolved)?;
    if json {
        println!("{}", build_list_json(resolved, &names)?);
        return Ok(());
    }
    if names.is_empty() {
        println!("(no env bundles stored for `{}`)", resolved.display_name());
        println!();
        println!("Load one with: pillbox env load <NAME> <PATH>");
        return Ok(());
    }
    println!(
        "Env bundles visible from `{}` (project shadows global on conflict):",
        resolved.display_name()
    );
    for entry in &names {
        let count = read(resolved, &entry.name)?.map(|m| m.len()).unwrap_or(0);
        let scope_tag = if entry.from_project {
            "project"
        } else {
            "global"
        };
        println!("  {:<20}  [{scope_tag}]  ({count} variables)", entry.name);
    }
    Ok(())
}

pub(crate) fn show(
    resolved: &Pillbox,
    name: &str,
    reveal: bool,
    to_stdout: bool,
    json: bool,
) -> Result<()> {
    let vars = read(resolved, name)?.ok_or_else(|| {
        PillboxError::runtime("env show", format!("bundle `{name}` not found")).with_next(format!(
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

pub(crate) fn rm(resolved: &Pillbox, scope: WriteScope, name: &str) -> Result<()> {
    let target = resolved.write_target(scope);
    match EnvRegistry::delete(&target, name)? {
        true => println!(
            "pillbox: ✓ env bundle `{name}` removed from `{}`",
            target.display_name()
        ),
        false => println!(
            "(no env bundle named `{name}` was stored in `{}`)",
            target.display_name()
        ),
    }
    Ok(())
}

// ── .env parsing ────────────────────────────────────────────────────────

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
    let Some(first) = chars.next() else {
        return false;
    };
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

fn mask(value: &str) -> String {
    crate::secrets::mask(value)
}

// ── JSON ────────────────────────────────────────────────────────────────

fn build_list_json(resolved: &Pillbox, names: &[MergedBundle]) -> Result<String> {
    let mut bundles = Vec::new();
    for name in names {
        let count = read(resolved, &name.name)?.map(|m| m.len()).unwrap_or(0);
        let mut o = serde_json::Map::new();
        o.insert("name".into(), serde_json::Value::String(name.name.clone()));
        o.insert(
            "scope".into(),
            serde_json::Value::String(name.scope.clone()),
        );
        o.insert(
            "variable_count".into(),
            serde_json::Value::Number(count.into()),
        );
        bundles.push(serde_json::Value::Object(o));
    }
    Ok(crate::paths::json_v1(vec![
        (
            "pillbox",
            serde_json::Value::String(resolved.display_name().into()),
        ),
        ("bundles", serde_json::Value::Array(bundles)),
    ]))
}

fn build_show_json(name: &str, vars: &BTreeMap<String, String>, revealed: bool) -> Result<String> {
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
    Ok(crate::paths::json_v1(vec![
        ("name", serde_json::Value::String(name.into())),
        ("revealed", serde_json::Value::Bool(revealed)),
        ("variables", serde_json::Value::Array(variables)),
    ]))
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
        let out = parse_dotenv(
            "# top comment\n\nFOO=bar\n  # indented comment\nBAZ=qux\n",
            "t",
        )
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
        let out = parse_dotenv("A=\"hello world\"\nB='single quoted'\nC=plain\n", "t").unwrap();
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
