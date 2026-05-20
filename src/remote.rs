//! Remote-VPS registry — names + SSH URLs the user can target with
//! `pillbox run --remote NAME`. Pillbox itself doesn't deploy the
//! binary; users `brew install pillbox` / `cargo install pillbox` on
//! the VPS, then register it here so we know how to reach it.
//!
//! ## Storage
//!
//! Remotes are pillbox-scoped. One TOML file per remote at
//! `<pillbox>/remotes/<name>.toml`:
//!
//! ```toml
//! name = "my-vps"
//! url = "ssh://user@host"
//! ```
//!
//! ## Inheritance
//!
//! Reads walk project → global so a remote registered against the
//! global pillbox is visible from every project. Writes default to the
//! resolved pillbox; `--global` forces global. Mirrors the secret / env
//! inheritance rule (see `pillbox::WriteScope`).
//!
//! ## URL shape
//!
//! Only `ssh://user@host[:port]` is accepted today. We could later add
//! `ssh+e2b://...` etc., but PR 4 is openssh-only and we want clear
//! error messages when the user mistypes.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::errors::PillboxError;
use crate::paths::{validate_name, write_private_file};
use crate::pillbox::{Pillbox, Scope, WriteScope};

/// Subdirectory under a pillbox's state dir that holds remote-registry
/// TOML files. One file per remote — easy to grep, easy to `rm`.
pub(crate) const REMOTES_DIR: &str = "remotes";

/// On-disk shape for one remote. Forward-compatible: unknown fields are
/// ignored on load so a future binary writing extra config (host
/// fingerprint, pinned pillbox path on remote, etc.) doesn't break older
/// readers. The required fields are `name` + `url`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Remote {
    /// Display name. Must match the file stem (`<name>.toml`).
    pub(crate) name: String,
    /// SSH URL — `ssh://user@host[:port]`. Parsed by [`parse_ssh_url`]
    /// at register and connect time.
    pub(crate) url: String,
    /// Default agent for `pillbox run --remote NAME` (overrides the
    /// pillbox's own `agent` field). Optional.
    #[serde(default)]
    pub(crate) default_agent: Option<String>,
}

/// Decomposed `ssh://user@host[:port]` URL. Stored as a plain struct so
/// callers can pass `user`/`host`/`port` to the openssh API without
/// re-parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SshUrl {
    pub(crate) user: String,
    pub(crate) host: String,
    pub(crate) port: Option<u16>,
}

impl SshUrl {
    /// Render in the canonical `user@host[:port]` form openssh accepts
    /// as a destination. Strips the `ssh://` scheme.
    pub(crate) fn destination(&self) -> String {
        match self.port {
            Some(p) => format!("{}@{}:{p}", self.user, self.host),
            None => format!("{}@{}", self.user, self.host),
        }
    }
}

/// Validate + parse an `ssh://user@host[:port]` URL. The whole URL
/// surface we support today; richer schemes (key path overrides, etc.)
/// can go through `~/.ssh/config` instead of being baked into the
/// registry.
pub(crate) fn parse_ssh_url(url: &str) -> Result<SshUrl, String> {
    let rest = url
        .strip_prefix("ssh://")
        .ok_or_else(|| format!("expected `ssh://user@host[:port]`, got `{url}`"))?;
    let (user, host_port) = rest
        .split_once('@')
        .ok_or_else(|| format!("missing `user@` in `{url}`"))?;
    if user.is_empty() {
        return Err(format!("empty user in `{url}`"));
    }
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) => {
            let parsed: u16 = p
                .parse()
                .map_err(|_| format!("invalid port `{p}` in `{url}`"))?;
            (h, Some(parsed))
        }
        None => (host_port, None),
    };
    if host.is_empty() {
        return Err(format!("empty host in `{url}`"));
    }
    Ok(SshUrl {
        user: user.to_string(),
        host: host.to_string(),
        port,
    })
}

/// Write-side: ensure `<pillbox>/remotes/` exists at 0700, return its
/// path. Mirrors `secrets::secrets_dir` so the perms invariant lives in
/// one place (`Pillbox::subdir`).
fn remotes_dir(pb: &Pillbox) -> Result<PathBuf> {
    pb.subdir(REMOTES_DIR)
}

/// Read-side: just join the path. Read code paths walk the scope chain
/// and may encounter pillboxes with no remotes/ dir yet — paying a
/// `create_dir_all` per lookup would be wasteful.
fn remotes_dir_read(pb: &Pillbox) -> PathBuf {
    pb.subdir_path(REMOTES_DIR)
}

fn remote_path(pb: &Pillbox, name: &str) -> Result<PathBuf> {
    Ok(remotes_dir(pb)?.join(format!("{name}.toml")))
}

fn remote_path_read(pb: &Pillbox, name: &str) -> PathBuf {
    remotes_dir_read(pb).join(format!("{name}.toml"))
}

/// Look up a remote by name. Walks project → global so a remote
/// registered globally is visible from every project pillbox, but a
/// project may shadow a global remote of the same name with its own.
/// Returns `(remote, source_pillbox_name)`.
pub(crate) fn read_inherited(resolved: &Pillbox, name: &str) -> Result<Option<(Remote, String)>> {
    validate_name("remote lookup", name)?;
    for pb in resolved.read_chain() {
        let path = remote_path_read(&pb, name);
        match fs::read_to_string(&path) {
            Ok(raw) => {
                let remote = parse_remote(&raw, &path)?;
                return Ok(Some((remote, pb.display_name().to_string())));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        }
    }
    Ok(None)
}

/// Same as [`read_inherited`] but only returns the [`Remote`].
pub(crate) fn read(resolved: &Pillbox, name: &str) -> Result<Option<Remote>> {
    Ok(read_inherited(resolved, name)?.map(|(r, _)| r))
}

/// One remote, with the scope it came from. Used by `pillbox remote list`.
#[derive(Debug, Clone)]
pub(crate) struct MergedRemote {
    pub(crate) remote: Remote,
    pub(crate) scope: String,
    pub(crate) from_project: bool,
}

/// All remotes visible from `resolved`, deduplicated. Project entries
/// shadow global ones of the same name.
pub(crate) fn list_merged(resolved: &Pillbox) -> Result<Vec<MergedRemote>> {
    use std::collections::BTreeMap;
    let mut map: BTreeMap<String, MergedRemote> = BTreeMap::new();
    for pb in resolved.read_chain() {
        let dir = remotes_dir_read(&pb);
        if !dir.exists() {
            continue;
        }
        let from_project = matches!(pb.scope, Scope::Project { .. });
        for entry in fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let fname = match entry.file_name().into_string() {
                Ok(s) => s,
                Err(_) => continue,
            };
            let name = match fname.strip_suffix(".toml") {
                Some(s) => s.to_string(),
                None => continue,
            };
            // Already populated from an earlier (project) scope —
            // global is the fallback, so don't clobber.
            if map.contains_key(&name) {
                continue;
            }
            let raw = fs::read_to_string(entry.path())
                .with_context(|| format!("read {}", entry.path().display()))?;
            let remote = parse_remote(&raw, &entry.path())?;
            map.insert(
                name.clone(),
                MergedRemote {
                    remote,
                    scope: pb.display_name().to_string(),
                    from_project,
                },
            );
        }
    }
    Ok(map.into_values().collect())
}

fn parse_remote(raw: &str, source: &Path) -> Result<Remote> {
    let r: Remote = toml::from_str(raw)
        .map_err(|e| PillboxError::config("remote read", format!("{}: {e}", source.display())))?;
    // Belt-and-suspenders: a TOML file written by an older / hand-edited
    // pillbox without a valid URL should fail at load time, not at
    // connect time. parse_ssh_url's diagnostic is more actionable than
    // openssh's "could not connect".
    parse_ssh_url(&r.url)
        .map_err(|e| PillboxError::config("remote read", format!("{}: {e}", source.display())))?;
    Ok(r)
}

/// `pillbox remote add NAME --url ssh://user@host`.
pub(crate) fn add(
    resolved: &Pillbox,
    scope: WriteScope,
    name: &str,
    url: &str,
    default_agent: Option<String>,
    if_not_exists: bool,
) -> Result<()> {
    validate_name("remote add", name)?;
    parse_ssh_url(url).map_err(|e| PillboxError::usage("remote add", e))?;
    if let Some(agent) = default_agent.as_deref() {
        crate::agents::lookup("remote add", agent)?;
    }
    let target = resolved.write_target(scope);
    let path = remote_path(&target, name)?;
    if if_not_exists && path.exists() {
        return Err(PillboxError::runtime(
            "remote add",
            format!(
                "remote `{name}` already exists in `{}`",
                target.display_name()
            ),
        )
        .with_next(format!("pillbox remote rm {name}  # then re-add"))
        .into());
    }
    let remote = Remote {
        name: name.to_string(),
        url: url.to_string(),
        default_agent,
    };
    let body = render_toml(&remote);
    write_private_file(&path, body.as_bytes())?;
    println!(
        "pillbox: ✓ remote `{name}` -> {url} (stored in `{}`)",
        target.display_name()
    );
    Ok(())
}

/// `pillbox remote rm NAME`.
pub(crate) fn rm(resolved: &Pillbox, scope: WriteScope, name: &str) -> Result<()> {
    validate_name("remote rm", name)?;
    let target = resolved.write_target(scope);
    let path = remote_path(&target, name)?;
    match fs::remove_file(&path) {
        Ok(()) => {
            println!(
                "pillbox: ✓ remote `{name}` removed from `{}`",
                target.display_name()
            );
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!(
                "(no remote named `{name}` was stored in `{}`)",
                target.display_name()
            );
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("remove {}", path.display())),
    }
}

/// `pillbox remote list`.
pub(crate) fn list(resolved: &Pillbox, json: bool) -> Result<()> {
    let entries = list_merged(resolved)?;
    if json {
        println!("{}", build_list_json(resolved, &entries));
        return Ok(());
    }
    if entries.is_empty() {
        println!("(no remotes registered for `{}`)", resolved.display_name());
        println!();
        println!("Add one with: pillbox remote add NAME ssh://user@host");
        return Ok(());
    }
    println!(
        "Remotes visible from `{}` (project shadows global on conflict):",
        resolved.display_name()
    );
    for entry in entries {
        let scope_tag = if entry.from_project {
            "project"
        } else {
            "global"
        };
        let agent = entry
            .remote
            .default_agent
            .as_deref()
            .map(|a| format!(" agent={a}"))
            .unwrap_or_default();
        println!(
            "  {:<20}  [{scope_tag}]  {}{}",
            entry.remote.name, entry.remote.url, agent,
        );
    }
    Ok(())
}

/// `pillbox remote info NAME`.
pub(crate) fn info(resolved: &Pillbox, name: &str, json: bool) -> Result<()> {
    let (remote, source) = read_inherited(resolved, name)?.ok_or_else(|| {
        PillboxError::runtime("remote info", format!("`{name}` not found"))
            .with_next(format!("pillbox remote add {name} ssh://user@host"))
    })?;
    if json {
        println!("{}", build_info_json(&remote, &source));
        return Ok(());
    }
    println!("Remote: {}", remote.name);
    println!("  url:    {}", remote.url);
    println!("  source: {source}");
    if let Some(a) = remote.default_agent.as_deref() {
        println!("  agent:  {a}");
    }
    Ok(())
}

fn render_toml(remote: &Remote) -> String {
    let mut out = String::new();
    // We render by hand (rather than `toml::to_string`) so the file stays
    // tidy: comments, predictable field order, no surprise pretty-print
    // differences across toml versions. Round-trips through `parse_remote`
    // either way — `Remote` is the source of truth.
    out.push_str("# v0.6 pillbox remote — see docs/remotes.md\n\n");
    out.push_str(&format!("name = \"{}\"\n", remote.name));
    out.push_str(&format!("url = \"{}\"\n", remote.url));
    if let Some(a) = remote.default_agent.as_deref() {
        out.push_str(&format!("default_agent = \"{a}\"\n"));
    }
    out
}

fn build_list_json(resolved: &Pillbox, entries: &[MergedRemote]) -> String {
    let arr: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            let mut o = serde_json::Map::new();
            o.insert(
                "name".into(),
                serde_json::Value::String(e.remote.name.clone()),
            );
            o.insert(
                "url".into(),
                serde_json::Value::String(e.remote.url.clone()),
            );
            o.insert("scope".into(), serde_json::Value::String(e.scope.clone()));
            if let Some(a) = e.remote.default_agent.as_deref() {
                o.insert(
                    "default_agent".into(),
                    serde_json::Value::String(a.to_string()),
                );
            }
            serde_json::Value::Object(o)
        })
        .collect();
    crate::paths::json_v1(vec![
        (
            "pillbox",
            serde_json::Value::String(resolved.display_name().into()),
        ),
        ("remotes", serde_json::Value::Array(arr)),
    ])
}

fn build_info_json(remote: &Remote, source: &str) -> String {
    let mut o = serde_json::Map::new();
    o.insert(
        "name".into(),
        serde_json::Value::String(remote.name.clone()),
    );
    o.insert("url".into(), serde_json::Value::String(remote.url.clone()));
    o.insert("source".into(), serde_json::Value::String(source.into()));
    if let Some(a) = remote.default_agent.as_deref() {
        o.insert(
            "default_agent".into(),
            serde_json::Value::String(a.to_string()),
        );
    }
    crate::paths::json_v1(vec![("remote", serde_json::Value::Object(o))])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pillbox;
    use crate::test_util::with_isolated_home;

    #[test]
    fn parse_ssh_url_basic() {
        let u = parse_ssh_url("ssh://alice@example.com").unwrap();
        assert_eq!(u.user, "alice");
        assert_eq!(u.host, "example.com");
        assert_eq!(u.port, None);
        assert_eq!(u.destination(), "alice@example.com");
    }

    #[test]
    fn parse_ssh_url_with_port() {
        let u = parse_ssh_url("ssh://bob@10.0.0.1:2222").unwrap();
        assert_eq!(u.user, "bob");
        assert_eq!(u.host, "10.0.0.1");
        assert_eq!(u.port, Some(2222));
        assert_eq!(u.destination(), "bob@10.0.0.1:2222");
    }

    #[test]
    fn parse_ssh_url_rejects_missing_scheme() {
        assert!(parse_ssh_url("alice@example.com").is_err());
    }

    #[test]
    fn parse_ssh_url_rejects_missing_user() {
        assert!(parse_ssh_url("ssh://example.com").is_err());
        assert!(parse_ssh_url("ssh://@example.com").is_err());
    }

    #[test]
    fn parse_ssh_url_rejects_bad_port() {
        assert!(parse_ssh_url("ssh://alice@host:notaport").is_err());
        assert!(parse_ssh_url("ssh://alice@host:99999").is_err());
    }

    #[test]
    fn parse_ssh_url_rejects_empty_host() {
        assert!(parse_ssh_url("ssh://alice@").is_err());
    }

    #[test]
    fn add_and_read_round_trip() {
        with_isolated_home("remote-rt", || {
            let g = pillbox::global();
            add(
                &g,
                WriteScope::Resolved,
                "vps1",
                "ssh://alice@example.com",
                None,
                false,
            )
            .unwrap();
            let r = read(&g, "vps1").unwrap().expect("present");
            assert_eq!(r.name, "vps1");
            assert_eq!(r.url, "ssh://alice@example.com");
            assert_eq!(r.default_agent, None);
        });
    }

    #[test]
    fn add_preserves_default_agent() {
        with_isolated_home("remote-agent", || {
            let g = pillbox::global();
            add(
                &g,
                WriteScope::Resolved,
                "vps2",
                "ssh://b@h:2222",
                Some("claude".into()),
                false,
            )
            .unwrap();
            let r = read(&g, "vps2").unwrap().expect("present");
            assert_eq!(r.default_agent.as_deref(), Some("claude"));
        });
    }

    #[test]
    fn add_rejects_bad_url() {
        with_isolated_home("remote-badurl", || {
            let g = pillbox::global();
            let err = add(&g, WriteScope::Resolved, "bad", "http://nope", None, false).unwrap_err();
            assert!(format!("{err}").contains("ssh://"));
        });
    }

    #[test]
    fn add_rejects_unknown_agent() {
        with_isolated_home("remote-badagent", || {
            let g = pillbox::global();
            let err = add(
                &g,
                WriteScope::Resolved,
                "vps",
                "ssh://a@h",
                Some("bogus".into()),
                false,
            )
            .unwrap_err();
            assert!(format!("{err}").contains("bogus"));
        });
    }

    #[test]
    fn if_not_exists_blocks_overwrite() {
        with_isolated_home("remote-ine", || {
            let g = pillbox::global();
            add(&g, WriteScope::Resolved, "vps", "ssh://a@h", None, false).unwrap();
            let err = add(&g, WriteScope::Resolved, "vps", "ssh://a@h", None, true).unwrap_err();
            assert!(format!("{err}").contains("already exists"));
        });
    }

    #[test]
    fn project_shadows_global() {
        with_isolated_home("remote-shadow", || {
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
            add(
                &g,
                WriteScope::Resolved,
                "shared",
                "ssh://g@global.example",
                None,
                false,
            )
            .unwrap();
            add(
                &proj,
                WriteScope::Resolved,
                "shared",
                "ssh://p@proj.example",
                None,
                false,
            )
            .unwrap();

            let (r, src) = read_inherited(&proj, "shared").unwrap().unwrap();
            assert_eq!(r.url, "ssh://p@proj.example");
            assert_eq!(src, "proj");

            // From global, only the global value.
            let (r, src) = read_inherited(&g, "shared").unwrap().unwrap();
            assert_eq!(r.url, "ssh://g@global.example");
            assert_eq!(src, "global");

            if let Some(c) = saved {
                let _ = std::env::set_current_dir(c);
            }
        });
    }

    #[test]
    fn project_inherits_global_when_unique() {
        with_isolated_home("remote-inherit", || {
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
            add(
                &g,
                WriteScope::Resolved,
                "g-only",
                "ssh://x@only-global",
                None,
                false,
            )
            .unwrap();

            let r = read(&proj, "g-only").unwrap().expect("inherited");
            assert_eq!(r.url, "ssh://x@only-global");

            if let Some(c) = saved {
                let _ = std::env::set_current_dir(c);
            }
        });
    }

    #[test]
    fn list_merged_dedupes_and_marks_scope() {
        with_isolated_home("remote-list", || {
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
            add(&g, WriteScope::Resolved, "g_only", "ssh://a@g", None, false).unwrap();
            add(
                &proj,
                WriteScope::Resolved,
                "p_only",
                "ssh://a@p",
                None,
                false,
            )
            .unwrap();
            add(
                &g,
                WriteScope::Resolved,
                "both",
                "ssh://a@g-both",
                None,
                false,
            )
            .unwrap();
            add(
                &proj,
                WriteScope::Resolved,
                "both",
                "ssh://a@p-both",
                None,
                false,
            )
            .unwrap();

            let entries = list_merged(&proj).unwrap();
            let g_only = entries.iter().find(|e| e.remote.name == "g_only").unwrap();
            let p_only = entries.iter().find(|e| e.remote.name == "p_only").unwrap();
            let both = entries.iter().find(|e| e.remote.name == "both").unwrap();
            assert!(!g_only.from_project);
            assert!(p_only.from_project);
            assert!(both.from_project, "project should shadow global");
            assert_eq!(both.remote.url, "ssh://a@p-both");

            if let Some(c) = saved {
                let _ = std::env::set_current_dir(c);
            }
        });
    }

    #[test]
    fn rm_removes_and_is_idempotent() {
        with_isolated_home("remote-rm", || {
            let g = pillbox::global();
            add(&g, WriteScope::Resolved, "vps", "ssh://a@h", None, false).unwrap();
            assert!(read(&g, "vps").unwrap().is_some());
            rm(&g, WriteScope::Resolved, "vps").unwrap();
            assert!(read(&g, "vps").unwrap().is_none());
            // Idempotent: removing twice doesn't error.
            rm(&g, WriteScope::Resolved, "vps").unwrap();
        });
    }

    #[test]
    fn write_scope_global_forces_global_target() {
        with_isolated_home("remote-write-global", || {
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
            add(
                &proj,
                WriteScope::Global,
                "explicit",
                "ssh://a@h",
                None,
                false,
            )
            .unwrap();
            let g = pillbox::global();
            // Project alone has no remote.
            let local_path = remote_path(&proj, "explicit").unwrap();
            assert!(!local_path.exists());
            // Global has it.
            assert!(read(&g, "explicit").unwrap().is_some());

            if let Some(c) = saved {
                let _ = std::env::set_current_dir(c);
            }
        });
    }

    #[test]
    fn corrupt_toml_surfaces_config_error() {
        with_isolated_home("remote-corrupt", || {
            let g = pillbox::global();
            let dir = remotes_dir(&g).unwrap();
            fs::write(dir.join("bad.toml"), b"not valid toml = !!!").unwrap();
            let err = read(&g, "bad").unwrap_err();
            assert!(format!("{err}").contains("remote read"));
        });
    }
}
