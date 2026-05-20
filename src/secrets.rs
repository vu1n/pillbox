//! `pillbox secret …` — secrets scoped to the current pillbox.
//!
//! v0.6 storage: one file per secret at `<pillbox>/secrets/<name>` (0600).
//! A pillbox is the resolved scope (`~/.pillbox/global/` or
//! `~/.pillbox/projects/<key>/`).
//!
//! ## Inheritance
//!
//! Reads merge global + project (project wins on key conflict).
//! Writes default to project; `--global` forces global.
//!
//! ## Idempotency
//!
//! `secret add` overwrites silently. Pass `--if-not-exists` to fail when
//! the secret is already present in the **chosen** scope. Inherited
//! secrets in a different scope don't block — that's the point of layering.
//!
//! ## Reveal model
//!
//! `secret show` masks values by default. `--reveal` plus a TTY on stdout
//! unmasks. Piping `--reveal` somewhere refuses unless the caller adds
//! `--to-stdout` to ack the leak.

use std::{
    collections::BTreeMap,
    fs,
    io::{IsTerminal, Read},
    path::PathBuf,
};

use anyhow::{Context, Result};

use crate::errors::PillboxError;
use crate::paths::{validate_name, write_private_file};
#[cfg(test)]
use crate::pillbox;
use crate::pillbox::{Pillbox, Scope};
use crate::vault::VaultMeta;

// Re-export so callers that already use `secrets::WriteScope` keep
// compiling; the canonical home is `pillbox::WriteScope` since the
// scope/inheritance model is a property of the pillbox, not the
// particular kind of stored data (secrets vs env bundles).
pub(crate) use crate::pillbox::WriteScope;

/// Write-side: creates `<pillbox>/secrets/` if absent and pins 0700.
fn secrets_dir(pb: &Pillbox) -> Result<PathBuf> {
    pb.subdir("secrets")
}

/// Read-side: just the path, no `mkdir`/`chmod`. `pillbox run` walks
/// secrets per `--with` entry across two scopes — paying dir-creation
/// syscalls on every lookup adds up quickly.
fn secrets_dir_read(pb: &Pillbox) -> PathBuf {
    pb.subdir_path("secrets")
}

fn secret_path(pb: &Pillbox, name: &str) -> Result<PathBuf> {
    Ok(secrets_dir(pb)?.join(name))
}

fn secret_path_read(pb: &Pillbox, name: &str) -> PathBuf {
    secrets_dir_read(pb).join(name)
}

fn meta_path(pb: &Pillbox, name: &str) -> Result<PathBuf> {
    Ok(secrets_dir(pb)?.join(format!("{name}.meta.json")))
}

fn meta_path_read(pb: &Pillbox, name: &str) -> PathBuf {
    secrets_dir_read(pb).join(format!("{name}.meta.json"))
}

/// Read a secret's stored value, walking scopes project → global. Returns
/// `(value, source_pillbox_name)` so callers can surface where the secret
/// came from. Returns `None` if neither scope has it.
pub(crate) fn read_inherited(resolved: &Pillbox, name: &str) -> Result<Option<(String, String)>> {
    validate_name("secret read", name)?;
    for pb in resolved.read_chain() {
        let path = secret_path_read(&pb, name);
        match fs::read_to_string(&path) {
            Ok(v) => return Ok(Some((v, pb.display_name().to_string()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        }
    }
    Ok(None)
}

/// Read just the value (no source). Used by run-path code that doesn't
/// care which scope provided the secret.
pub(crate) fn read(resolved: &Pillbox, name: &str) -> Result<Option<String>> {
    Ok(read_inherited(resolved, name)?.map(|(v, _)| v))
}

/// Names of every stored secret across both scopes, deduplicated. Project
/// names shadow global. Sidecar `.meta.json` files are filtered out.
pub(crate) fn names_merged(resolved: &Pillbox) -> Result<Vec<MergedEntry>> {
    let mut map: BTreeMap<String, MergedEntry> = BTreeMap::new();
    for pb in resolved.read_chain() {
        let dir = secrets_dir_read(&pb);
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
            if fname.ends_with(".meta.json") {
                continue;
            }
            map.entry(fname.clone()).or_insert_with(|| MergedEntry {
                name: fname,
                scope: pb.display_name().to_string(),
                from_project: matches!(pb.scope, Scope::Project { .. }),
            });
        }
    }
    Ok(map.into_values().collect())
}

#[derive(Debug, Clone)]
pub(crate) struct MergedEntry {
    pub(crate) name: String,
    pub(crate) scope: String,
    pub(crate) from_project: bool,
}

pub(crate) fn read_meta(resolved: &Pillbox, name: &str) -> Result<Option<VaultMeta>> {
    validate_name("secret read", name)?;
    for pb in resolved.read_chain() {
        let path = meta_path_read(&pb, name);
        let raw = match fs::read_to_string(&path) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        let meta: VaultMeta = serde_json::from_str(&raw).map_err(|e| {
            PillboxError::config("secret read", format!("parse {}: {e}", path.display()))
        })?;
        return Ok(Some(meta));
    }
    Ok(None)
}

fn write_meta(pb: &Pillbox, name: &str, meta: &VaultMeta) -> Result<()> {
    validate_name("secret meta", name)?;
    let path = meta_path(pb, name)?;
    let body = serde_json::to_string_pretty(meta)
        .with_context(|| format!("serialize meta for `{name}`"))?;
    write_private_file(&path, body.as_bytes())
}

pub(crate) fn add(
    resolved: &Pillbox,
    scope: WriteScope,
    name: &str,
    source: AddSource,
    if_not_exists: bool,
    vault_meta: Option<VaultMeta>,
) -> Result<()> {
    validate_name("secret add", name)?;
    let target = resolved.write_target(scope);
    let path = secret_path(&target, name)?;
    if if_not_exists && path.exists() {
        return Err(PillboxError::runtime(
            "secret add",
            format!("`{name}` already exists in `{}`", target.display_name()),
        )
        .with_next(format!(
            "pillbox secret rm {name}  # then re-add  (or drop --if-not-exists)"
        ))
        .into());
    }
    let value = read_value(name, source)?;
    write_private_file(&path, value.as_bytes())?;
    if let Some(meta) = vault_meta.as_ref() {
        write_meta(&target, name, meta)?;
    } else {
        let m = meta_path(&target, name)?;
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
            "pillbox: ✓ secret `{name}` stored in `{}` (vaulted: {} / {})",
            target.display_name(),
            meta.vault.host,
            meta.vault.header_scheme.as_str()
        );
    } else {
        println!(
            "pillbox: ✓ secret `{name}` stored in `{}` ({})",
            target.display_name(),
            path.display()
        );
    }
    Ok(())
}

pub(crate) fn list(resolved: &Pillbox, json: bool) -> Result<()> {
    let names = names_merged(resolved)?;
    if json {
        let payload = build_list_json(resolved, &names)?;
        println!("{payload}");
        return Ok(());
    }
    if names.is_empty() {
        println!("(no secrets stored for `{}`)", resolved.display_name());
        println!();
        println!("Add one with: pillbox secret add <NAME>");
        return Ok(());
    }
    println!(
        "Secrets visible from `{}` (project shadows global on conflict):",
        resolved.display_name()
    );
    for entry in names {
        let meta = read_meta(resolved, &entry.name)?;
        let scope_tag = if entry.from_project {
            "project"
        } else {
            "global"
        };
        match meta {
            Some(m) => println!(
                "  {:<20}  [{scope_tag}]  (vaulted: {} / {})",
                entry.name,
                m.vault.host,
                m.vault.header_scheme.as_str()
            ),
            None => println!("  {:<20}  [{scope_tag}]", entry.name),
        }
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
    let (value, source) = read_inherited(resolved, name)?.ok_or_else(|| {
        PillboxError::runtime("secret show", format!("`{name}` not found"))
            .with_next(format!("pillbox secret add {name}"))
    })?;
    let meta = read_meta(resolved, name)?;
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
        println!(
            "{}",
            build_show_json(name, &display, reveal, &source, meta.as_ref())
        );
    } else {
        match meta.as_ref() {
            Some(m) => println!(
                "{name}={display}  [from {source}]  (vaulted: {} / {})",
                m.vault.host,
                m.vault.header_scheme.as_str()
            ),
            None => println!("{name}={display}  [from {source}]"),
        }
    }
    Ok(())
}

pub(crate) fn rm(resolved: &Pillbox, scope: WriteScope, name: &str) -> Result<()> {
    validate_name("secret rm", name)?;
    let target = resolved.write_target(scope);
    let path = secret_path(&target, name)?;
    let removed = match fs::remove_file(&path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(e).with_context(|| format!("remove {}", path.display())),
    };
    let m = meta_path(&target, name)?;
    match fs::remove_file(&m) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("remove {}", m.display())),
    }
    if removed {
        println!(
            "pillbox: ✓ secret `{name}` removed from `{}`",
            target.display_name()
        );
    } else {
        println!(
            "(no secret named `{name}` was stored in `{}`)",
            target.display_name()
        );
    }
    Ok(())
}

/// Source for `secret add`.
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
    Ok(raw.trim_end().to_string())
}

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

// ── JSON ────────────────────────────────────────────────────────────────

fn build_list_json(resolved: &Pillbox, entries: &[MergedEntry]) -> Result<String> {
    let mut arr: Vec<serde_json::Value> = Vec::with_capacity(entries.len());
    for e in entries {
        let mut o = serde_json::Map::new();
        o.insert("name".into(), serde_json::Value::String(e.name.clone()));
        o.insert("scope".into(), serde_json::Value::String(e.scope.clone()));
        if let Some(meta) = read_meta(resolved, &e.name)? {
            o.insert("vault".into(), vault_meta_json_short(&meta));
        }
        arr.push(serde_json::Value::Object(o));
    }
    Ok(crate::paths::json_v1(vec![
        (
            "pillbox",
            serde_json::Value::String(resolved.display_name().into()),
        ),
        ("secrets", serde_json::Value::Array(arr)),
    ]))
}

fn build_show_json(
    name: &str,
    value: &str,
    revealed: bool,
    source: &str,
    meta: Option<&VaultMeta>,
) -> String {
    let mut fields: Vec<(&'static str, serde_json::Value)> = vec![
        ("name", serde_json::Value::String(name.into())),
        ("value", serde_json::Value::String(value.into())),
        ("revealed", serde_json::Value::Bool(revealed)),
        ("source", serde_json::Value::String(source.into())),
    ];
    if let Some(m) = meta {
        fields.push(("vault", vault_meta_json_short(m)));
    }
    crate::paths::json_v1(fields)
}

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

    fn add_plain(pb: &Pillbox, scope: WriteScope, name: &str, value: &str) {
        std::env::set_var("__PB_TEST_PLAIN", value);
        add(
            pb,
            scope,
            name,
            AddSource::EnvVar("__PB_TEST_PLAIN".into()),
            false,
            None,
        )
        .unwrap();
        std::env::remove_var("__PB_TEST_PLAIN");
    }

    #[test]
    fn mask_short_values_fully() {
        assert_eq!(mask("abc"), "***");
        assert_eq!(mask("abcd"), "****");
    }

    #[test]
    fn mask_shows_last_four_chars() {
        assert_eq!(mask("abcdefgh"), "****efgh");
    }

    #[test]
    fn mask_ignores_trailing_whitespace() {
        assert_eq!(mask("abcdefgh\n"), "****efgh");
    }

    #[test]
    fn add_and_read_global_secret() {
        with_isolated_home("secrets-global", || {
            let g = pillbox::global();
            add_plain(&g, WriteScope::Resolved, "A", "alpha");
            let v = read(&g, "A").unwrap();
            assert_eq!(v.as_deref(), Some("alpha"));
        });
    }

    #[test]
    fn project_secret_shadows_global() {
        with_isolated_home("secrets-shadow", || {
            // Set up a project pillbox.
            let tmp = tempfile::tempdir().unwrap();
            let saved = std::env::current_dir().ok();
            std::env::set_current_dir(tmp.path()).unwrap();
            pillbox::new(
                Some("proj".into()),
                None,
                pillbox::NewWorkspaceArgs::default(),
            )
            .unwrap();
            let proj = Pillbox::resolve(None).unwrap();
            assert!(!proj.is_global());

            // Stash one in global, one with the same name in project.
            let g = pillbox::global();
            add_plain(&g, WriteScope::Resolved, "OVERLAP", "global-val");
            add_plain(&proj, WriteScope::Resolved, "OVERLAP", "project-val");

            // From the project, the project value wins.
            let v = read(&proj, "OVERLAP").unwrap();
            assert_eq!(v.as_deref(), Some("project-val"));

            // From global, only the global value is visible.
            let v = read(&g, "OVERLAP").unwrap();
            assert_eq!(v.as_deref(), Some("global-val"));

            if let Some(c) = saved {
                let _ = std::env::set_current_dir(c);
            }
        });
    }

    #[test]
    fn project_inherits_global_when_unique() {
        with_isolated_home("secrets-inherit", || {
            let tmp = tempfile::tempdir().unwrap();
            let saved = std::env::current_dir().ok();
            std::env::set_current_dir(tmp.path()).unwrap();
            pillbox::new(
                Some("proj".into()),
                None,
                pillbox::NewWorkspaceArgs::default(),
            )
            .unwrap();
            let proj = Pillbox::resolve(None).unwrap();

            let g = pillbox::global();
            add_plain(&g, WriteScope::Resolved, "ONLY_GLOBAL", "g-val");
            let v = read(&proj, "ONLY_GLOBAL").unwrap();
            assert_eq!(v.as_deref(), Some("g-val"));

            if let Some(c) = saved {
                let _ = std::env::set_current_dir(c);
            }
        });
    }

    #[test]
    fn write_scope_global_forces_global_target() {
        with_isolated_home("secrets-write-global", || {
            let tmp = tempfile::tempdir().unwrap();
            let saved = std::env::current_dir().ok();
            std::env::set_current_dir(tmp.path()).unwrap();
            pillbox::new(
                Some("proj".into()),
                None,
                pillbox::NewWorkspaceArgs::default(),
            )
            .unwrap();
            let proj = Pillbox::resolve(None).unwrap();

            // Pass WriteScope::Global from a project context — value lands
            // in global, not project.
            add_plain(&proj, WriteScope::Global, "EXPLICIT_GLOBAL", "g-val");
            let g = pillbox::global();
            let v = read(&g, "EXPLICIT_GLOBAL").unwrap();
            assert_eq!(v.as_deref(), Some("g-val"));

            // And project alone has no value (would only see it via inheritance).
            let path = secret_path(&proj, "EXPLICIT_GLOBAL").unwrap();
            assert!(!path.exists());

            if let Some(c) = saved {
                let _ = std::env::set_current_dir(c);
            }
        });
    }

    #[test]
    fn names_merged_lists_both_scopes() {
        with_isolated_home("secrets-list-merged", || {
            let tmp = tempfile::tempdir().unwrap();
            let saved = std::env::current_dir().ok();
            std::env::set_current_dir(tmp.path()).unwrap();
            pillbox::new(
                Some("proj".into()),
                None,
                pillbox::NewWorkspaceArgs::default(),
            )
            .unwrap();
            let proj = Pillbox::resolve(None).unwrap();
            let g = pillbox::global();
            add_plain(&g, WriteScope::Resolved, "G_ONLY", "x");
            add_plain(&proj, WriteScope::Resolved, "P_ONLY", "x");
            add_plain(&g, WriteScope::Resolved, "BOTH", "g");
            add_plain(&proj, WriteScope::Resolved, "BOTH", "p");

            let names = names_merged(&proj).unwrap();
            let g_only = names.iter().find(|e| e.name == "G_ONLY").unwrap();
            let p_only = names.iter().find(|e| e.name == "P_ONLY").unwrap();
            let both = names.iter().find(|e| e.name == "BOTH").unwrap();
            assert!(!g_only.from_project);
            assert!(p_only.from_project);
            // BOTH must show as project (project shadows global).
            assert!(both.from_project);

            if let Some(c) = saved {
                let _ = std::env::set_current_dir(c);
            }
        });
    }

    #[test]
    fn vault_meta_round_trip_via_disk() {
        with_isolated_home("secrets-meta", || {
            let g = pillbox::global();
            let meta = VaultMeta::new(
                "api.example.com".into(),
                HeaderScheme::XApiKey,
                "ex-".into(),
            );
            // We need the file to exist for path resolution but write_meta
            // creates the dir lazily.
            write_meta(&g, "TEST_META_KEY", &meta).unwrap();
            let back = read_meta(&g, "TEST_META_KEY")
                .unwrap()
                .expect("meta present");
            assert_eq!(back, meta);
        });
    }

    #[test]
    fn missing_meta_means_not_vaulted() {
        with_isolated_home("secrets-nometa", || {
            let g = pillbox::global();
            let dir = secrets_dir(&g).unwrap();
            std::fs::write(dir.join("X"), b"x").unwrap();
            assert!(read_meta(&g, "X").unwrap().is_none());
        });
    }
}
