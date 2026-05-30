//! Remote registry — names + URLs the user can target with
//! `pillbox run --remote NAME`. Pillbox itself doesn't deploy the
//! binary; users install pillbox at the remote end (VPS package /
//! E2B template image), then register a label here so we know how to
//! reach it.
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
//! ## URL shapes
//!
//! - `ssh://user@host[:port]`        → VPS over openssh (legacy backend).
//! - `e2b://TEMPLATE_ID`             → E2B managed sandbox (deprecated).
//! - `docker://[user@]host[:port]`   → remote Docker daemon over SSH
//!   transport (`DOCKER_HOST=ssh://…`); runs the runner image there. The
//!   container-is-the-primitive backend the ssh/e2b paths collapse onto —
//!   see [docs/remotes-redesign.md](../docs/remotes-redesign.md).
//!
//! [`parse_remote_url`] is the single dispatch point; the on-disk shape
//! never grew a `kind` field — keeping `url` as the discriminator means
//! a hand-edited remote tells you what it is at a glance.

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::errors::PillboxError;
use crate::paths::validate_name;
use crate::pillbox::{Pillbox, WriteScope};
use crate::registry::{self as reg, InheritedRegistry, Registry};

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
    /// Remote URL — `docker://[user@]host[:port]`, `ssh://user@host[:port]`,
    /// or `e2b://TEMPLATE_ID`. Parsed by [`parse_remote_url`] at register
    /// and connect time.
    pub(crate) url: String,
    /// Default agent for `pillbox run --remote NAME` (overrides the
    /// pillbox's own `agent` field). Optional.
    #[serde(default)]
    pub(crate) default_agent: Option<String>,
}

/// Tagged URL — what kind of backend the URL targets. Constructed once
/// at registry-load time by [`parse_remote_url`]; the dispatcher in
/// `sandbox::select_backend` matches on the variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RemoteUrl {
    /// VPS reachable over openssh.
    Ssh(SshUrl),
    /// E2B managed cloud sandbox keyed by template id.
    E2b(E2bRef),
    /// Remote Docker daemon reached over SSH transport
    /// (`DOCKER_HOST=ssh://…`). The container-is-the-primitive backend.
    Docker(DockerUrl),
}

impl RemoteUrl {
    /// Short scheme label for diagnostics + `info` JSON.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            RemoteUrl::Ssh(_) => "ssh",
            RemoteUrl::E2b(_) => "e2b",
            RemoteUrl::Docker(_) => "docker",
        }
    }
}

impl Remote {
    /// Parsed URL, re-validated at every call. Cheap; the parser is
    /// stateless and the input is small. Single dispatch point so the
    /// `"ssh://"` / `"e2b://"` literals don't leak into `select_backend`.
    ///
    /// Returns `Err` only if the on-disk URL is malformed — readers
    /// (`parse_remote`) already validate at load time, so this is
    /// belt-and-suspenders for hand-edited TOML.
    pub(crate) fn parsed_url(&self) -> Result<RemoteUrl, String> {
        parse_remote_url(&self.url)
    }
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

/// Decomposed `e2b://TEMPLATE_ID` URL. Stored plain so the dispatcher
/// can hand the template directly to the helper subprocess without
/// re-parsing. Template IDs in E2B's surface are alphanumeric with `-`
/// / `_` separators; we validate that shape locally and let E2B reject
/// at sandbox-creation time if the value is otherwise wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct E2bRef {
    pub(crate) template: String,
}

/// Decomposed `docker://[user@]host[:port]` URL — a remote Docker daemon
/// reached over Docker's SSH transport. The backend sets
/// `DOCKER_HOST=ssh://[user@]host[:port]` and runs the runner image on
/// that daemon, so the placement axis is a first-class Docker feature
/// rather than the 2101 LOC `remote_ssh.rs` hand-rolls (see
/// [docs/remotes-redesign.md](../docs/remotes-redesign.md)).
///
/// `user` is optional: a bare `docker://host` defers to SSH's own default
/// user (`~/.ssh/config` / `$USER`), matching `docker -H ssh://host`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DockerUrl {
    pub(crate) user: Option<String>,
    pub(crate) host: String,
    pub(crate) port: Option<u16>,
}

impl DockerUrl {
    /// Render the `DOCKER_HOST` value Docker's SSH transport accepts —
    /// `ssh://[user@]host[:port]`. This is the env var the backend exports
    /// so every `docker` invocation targets the remote daemon.
    pub(crate) fn docker_host(&self) -> String {
        let mut out = String::from("ssh://");
        if let Some(u) = &self.user {
            out.push_str(u);
            out.push('@');
        }
        out.push_str(&self.host);
        if let Some(p) = self.port {
            out.push(':');
            out.push_str(&p.to_string());
        }
        out
    }
}

/// Dispatch on URL scheme. The only public validator the registry uses
/// on write / read; backend-specific parsers ([`parse_ssh_url`] /
/// [`parse_e2b_url`]) are kept around for callers (e.g. the SSH backend
/// re-parses inside its `run` for belt-and-suspenders against a
/// hand-edited TOML).
pub(crate) fn parse_remote_url(url: &str) -> Result<RemoteUrl, String> {
    if url.starts_with("ssh://") {
        return parse_ssh_url(url).map(RemoteUrl::Ssh);
    }
    if url.starts_with("e2b://") {
        return parse_e2b_url(url).map(RemoteUrl::E2b);
    }
    if url.starts_with("docker://") {
        return parse_docker_url(url).map(RemoteUrl::Docker);
    }
    Err(format!(
        "unsupported URL scheme in `{url}` (expected `docker://[user@]host[:port]`, `ssh://user@host[:port]`, or `e2b://TEMPLATE_ID`)"
    ))
}

/// Validate + parse an `e2b://TEMPLATE_ID` URL. Template IDs are
/// alphanumeric + `-` / `_`; UUIDs and human-readable aliases both
/// satisfy that shape. Empty or punctuation-laden values are rejected
/// at register time so the error message points at the URL, not the
/// helper-subprocess output.
///
/// Signature mirrors [`parse_ssh_url`]: callers pass the full `e2b://…`
/// URL and the prefix is stripped internally, so error messages can
/// always quote the user-facing form without a second `full` arg.
pub(crate) fn parse_e2b_url(url: &str) -> Result<E2bRef, String> {
    let rest = url
        .strip_prefix("e2b://")
        .ok_or_else(|| format!("expected `e2b://TEMPLATE_ID`, got `{url}`"))?;
    if rest.is_empty() {
        return Err(format!("missing template id in `{url}`"));
    }
    if let Some(bad) = rest
        .chars()
        .find(|c| !c.is_ascii_alphanumeric() && *c != '-' && *c != '_')
    {
        return Err(format!(
            "invalid character `{bad}` in e2b template id `{rest}` (expected alphanumeric + `-` / `_`)"
        ));
    }
    Ok(E2bRef {
        template: rest.to_string(),
    })
}

/// Validate + parse a `docker://[user@]host[:port]` URL into the pieces
/// the Docker SSH transport needs. Unlike [`parse_ssh_url`], the `user@`
/// segment is optional — Docker (and openssh) fall back to the default
/// user when it's omitted, so `docker://host` is the common BYO form.
/// Richer transport config (identity files, jump hosts) belongs in
/// `~/.ssh/config`, not the registry.
pub(crate) fn parse_docker_url(url: &str) -> Result<DockerUrl, String> {
    let rest = url
        .strip_prefix("docker://")
        .ok_or_else(|| format!("expected `docker://[user@]host[:port]`, got `{url}`"))?;
    if rest.is_empty() {
        return Err(format!(
            "missing host in `{url}` (expected `docker://[user@]host[:port]`)"
        ));
    }
    let (user, host_port) = match rest.split_once('@') {
        Some(("", _)) => return Err(format!("empty user in `{url}`")),
        Some((u, hp)) => (Some(u.to_string()), hp),
        None => (None, rest),
    };
    let (host, port) = split_host_port(host_port, url)?;
    Ok(DockerUrl { user, host, port })
}

/// Split the `host[:port]` tail shared by the `ssh://` and `docker://`
/// parsers: validate the port and reject an empty host. The two scheme
/// parsers differ only in their (required vs optional) user handling — this
/// is the common tail, so a future fix (IPv6 brackets, a max-port message)
/// lands in one place.
fn split_host_port(host_port: &str, url: &str) -> Result<(String, Option<u16>), String> {
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) => (
            h,
            Some(
                p.parse::<u16>()
                    .map_err(|_| format!("invalid port `{p}` in `{url}`"))?,
            ),
        ),
        None => (host_port, None),
    };
    if host.is_empty() {
        return Err(format!("empty host in `{url}`"));
    }
    Ok((host.to_string(), port))
}

/// Validate + parse an `ssh://user@host[:port]` URL. Richer schemes
/// (key path overrides, etc.) can go through `~/.ssh/config` instead
/// of being baked into the registry.
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
    let (host, port) = split_host_port(host_port, url)?;
    Ok(SshUrl {
        user: user.to_string(),
        host,
        port,
    })
}

/// Registry plumbing for remotes — TOML on disk, project→global
/// inheritance. URL re-validation lives in [`Self::parse`] so a
/// malformed `url =` on disk surfaces at read time, not connect time.
struct RemoteRegistry;
impl Registry for RemoteRegistry {
    type Record = Remote;
    const SUBDIR: &'static str = REMOTES_DIR;
    fn read_action() -> &'static str {
        "remote lookup"
    }
    fn filename(name: &str) -> String {
        format!("{name}.toml")
    }
    fn parse(raw: &str, source: &Path) -> Result<Self::Record> {
        let r: Remote = toml::from_str(raw).map_err(|e| {
            PillboxError::config("remote read", format!("{}: {e}", source.display()))
        })?;
        // Belt-and-suspenders: a TOML file written by an older /
        // hand-edited pillbox without a valid URL should fail at load
        // time, not at connect time. parse_remote_url's diagnostic is
        // more actionable than "could not connect" / "template not found".
        parse_remote_url(&r.url).map_err(|e| {
            PillboxError::config("remote read", format!("{}: {e}", source.display()))
        })?;
        Ok(r)
    }
}
impl InheritedRegistry for RemoteRegistry {}

/// Look up a remote by name. Walks project → global so a remote
/// registered globally is visible from every project pillbox, but a
/// project may shadow a global remote of the same name with its own.
/// Returns `(remote, source_pillbox_name)`.
pub(crate) fn read_inherited(resolved: &Pillbox, name: &str) -> Result<Option<(Remote, String)>> {
    reg::read_inherited::<RemoteRegistry>(resolved, name)
}

/// Same as [`read_inherited`] but only returns the [`Remote`].
pub(crate) fn read(resolved: &Pillbox, name: &str) -> Result<Option<Remote>> {
    Ok(read_inherited(resolved, name)?.map(|(r, _)| r))
}

/// Resolve a `--remote` value to a [`Remote`]. The single home for
/// run-target resolution policy so `pillbox run` (and, later, `session
/// attach/rm` re-resolution) don't each re-derive the name-vs-URL split:
///
/// - A value containing `://` is an **inline URL** — validated and used
///   directly, no `remote add` required. A registered name can never
///   contain `://` ([`validate_name`] rejects it), so the scheme marker is
///   an unambiguous discriminator. The URL doubles as the `name` (there's
///   no registry entry to borrow one from).
/// - Anything else is a **registered remote name**, looked up with
///   project→global inheritance.
///
/// NOTE: a detached run against an inline URL records the URL as its
/// `remote`; re-resolving inline-URL sessions for `session attach/rm` is a
/// follow-on (phase 3).
pub(crate) fn resolve_run_target(resolved: &Pillbox, target: &str) -> Result<Remote> {
    if target.contains("://") {
        parse_remote_url(target).map_err(|e| PillboxError::usage("run --remote", e))?;
        return Ok(Remote {
            name: target.to_string(),
            url: target.to_string(),
            default_agent: None,
        });
    }
    read(resolved, target)?.ok_or_else(|| {
        PillboxError::runtime("run", format!("remote `{target}` not found"))
            .with_next(format!("pillbox remote add {target} docker://user@host"))
            .into()
    })
}

/// One remote, with the scope it came from. Used by `pillbox remote list`.
pub(crate) type MergedRemote = reg::MergedEntry<Remote>;

/// All remotes visible from `resolved`, deduplicated. Project entries
/// shadow global ones of the same name.
pub(crate) fn list_merged(resolved: &Pillbox) -> Result<Vec<MergedRemote>> {
    reg::list_merged::<RemoteRegistry>(resolved)
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
    parse_remote_url(url).map_err(|e| PillboxError::usage("remote add", e))?;
    if let Some(agent) = default_agent.as_deref() {
        crate::agents::lookup("remote add", agent)?;
    }
    let target = resolved.write_target(scope);
    if if_not_exists && RemoteRegistry::path(&target, name)?.exists() {
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
    reg::write_record::<RemoteRegistry>(&target, name, body.as_bytes())?;
    println!(
        "pillbox: ✓ remote `{name}` -> {url} (stored in `{}`)",
        target.display_name()
    );
    Ok(())
}

/// `pillbox remote rm NAME`.
pub(crate) fn rm(resolved: &Pillbox, scope: WriteScope, name: &str) -> Result<()> {
    let target = resolved.write_target(scope);
    match RemoteRegistry::delete(&target, name)? {
        true => println!(
            "pillbox: ✓ remote `{name}` removed from `{}`",
            target.display_name()
        ),
        false => println!(
            "(no remote named `{name}` was stored in `{}`)",
            target.display_name()
        ),
    }
    Ok(())
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
        println!("Add one with:");
        println!("  pillbox remote add NAME docker://user@host    # remote Docker daemon over SSH");
        println!("  pillbox remote add NAME ssh://user@host       # VPS over openssh");
        println!("  pillbox remote add NAME e2b://TEMPLATE_ID     # E2B managed sandbox");
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
            .record
            .default_agent
            .as_deref()
            .map(|a| format!(" agent={a}"))
            .unwrap_or_default();
        println!(
            "  {:<20}  [{scope_tag}]  {}{}",
            entry.record.name, entry.record.url, agent,
        );
    }
    Ok(())
}

/// `pillbox remote info NAME`.
pub(crate) fn info(resolved: &Pillbox, name: &str, json: bool) -> Result<()> {
    let (remote, source) = read_inherited(resolved, name)?.ok_or_else(|| {
        PillboxError::runtime("remote info", format!("`{name}` not found")).with_next(format!(
            "pillbox remote add {name} docker://user@host  # or ssh://… / e2b://TEMPLATE_ID"
        ))
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
                serde_json::Value::String(e.record.name.clone()),
            );
            o.insert(
                "url".into(),
                serde_json::Value::String(e.record.url.clone()),
            );
            o.insert("scope".into(), serde_json::Value::String(e.scope.clone()));
            if let Some(a) = e.record.default_agent.as_deref() {
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
    // The `kind` field is derived from the URL scheme, not stored on
    // disk — but downstream JSON consumers (tooling, scripts) appreciate
    // not having to re-parse to know which backend will be used.
    if let Ok(parsed) = parse_remote_url(&remote.url) {
        o.insert(
            "kind".into(),
            serde_json::Value::String(parsed.kind().to_string()),
        );
    }
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
    fn parse_remote_url_ssh() {
        let url = parse_remote_url("ssh://alice@example.com:2222").unwrap();
        assert_eq!(url.kind(), "ssh");
        match url {
            RemoteUrl::Ssh(s) => assert_eq!(s.destination(), "alice@example.com:2222"),
            _ => panic!("expected ssh"),
        }
    }

    #[test]
    fn parse_remote_url_e2b() {
        let url = parse_remote_url("e2b://pillbox-default-template").unwrap();
        assert_eq!(url.kind(), "e2b");
        match url {
            RemoteUrl::E2b(e) => assert_eq!(e.template, "pillbox-default-template"),
            _ => panic!("expected e2b"),
        }
    }

    #[test]
    fn parse_remote_url_rejects_unknown_scheme() {
        let err = parse_remote_url("http://example.com").unwrap_err();
        assert!(err.contains("unsupported URL scheme"));
        assert!(err.contains("ssh://"));
        assert!(err.contains("e2b://"));
    }

    #[test]
    fn parse_e2b_url_accepts_uuid_and_alias() {
        // Realistic E2B template IDs: human alias or UUID-ish.
        assert!(parse_remote_url("e2b://my-pillbox-runner").is_ok());
        assert!(parse_remote_url("e2b://7f1c4e2a-9b8d-4f3e-a5c6-1d2e3f405060").is_ok());
        assert!(parse_remote_url("e2b://templ_underscored_123").is_ok());
    }

    #[test]
    fn parse_e2b_url_rejects_empty_template() {
        let err = parse_remote_url("e2b://").unwrap_err();
        assert!(err.contains("missing template id"));
    }

    #[test]
    fn parse_e2b_url_rejects_punctuation() {
        let err = parse_remote_url("e2b://bad/template").unwrap_err();
        assert!(err.contains("invalid character"));
    }

    #[test]
    fn parse_remote_url_docker() {
        let url = parse_remote_url("docker://deploy@vps.example:2222").unwrap();
        assert_eq!(url.kind(), "docker");
        match url {
            RemoteUrl::Docker(d) => {
                assert_eq!(d.user.as_deref(), Some("deploy"));
                assert_eq!(d.host, "vps.example");
                assert_eq!(d.port, Some(2222));
                assert_eq!(d.docker_host(), "ssh://deploy@vps.example:2222");
            }
            _ => panic!("expected docker"),
        }
    }

    #[test]
    fn parse_docker_url_userless_and_portless() {
        // The common BYO form: bare host, default ssh user + port.
        let d = parse_docker_url("docker://vps.example").unwrap();
        assert_eq!(d.user, None);
        assert_eq!(d.host, "vps.example");
        assert_eq!(d.port, None);
        assert_eq!(d.docker_host(), "ssh://vps.example");
    }

    #[test]
    fn parse_docker_url_user_no_port() {
        let d = parse_docker_url("docker://deploy@10.0.0.5").unwrap();
        assert_eq!(d.user.as_deref(), Some("deploy"));
        assert_eq!(d.host, "10.0.0.5");
        assert_eq!(d.port, None);
        assert_eq!(d.docker_host(), "ssh://deploy@10.0.0.5");
    }

    #[test]
    fn parse_docker_url_host_with_port_no_user() {
        let d = parse_docker_url("docker://10.0.0.5:2376").unwrap();
        assert_eq!(d.user, None);
        assert_eq!(d.host, "10.0.0.5");
        assert_eq!(d.port, Some(2376));
        assert_eq!(d.docker_host(), "ssh://10.0.0.5:2376");
    }

    #[test]
    fn parse_docker_url_rejects_empty_host() {
        assert!(parse_docker_url("docker://").is_err());
        assert!(parse_docker_url("docker://deploy@").is_err());
    }

    #[test]
    fn parse_docker_url_rejects_empty_user() {
        assert!(parse_docker_url("docker://@vps.example").is_err());
    }

    #[test]
    fn parse_docker_url_rejects_bad_port() {
        assert!(parse_docker_url("docker://host:notaport").is_err());
        assert!(parse_docker_url("docker://host:99999").is_err());
    }

    #[test]
    fn parse_docker_url_rejects_missing_scheme() {
        assert!(parse_docker_url("deploy@vps.example").is_err());
    }

    #[test]
    fn unknown_scheme_error_names_docker() {
        let err = parse_remote_url("http://example.com").unwrap_err();
        assert!(err.contains("docker://"));
    }

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
            let g_only = entries.iter().find(|e| e.record.name == "g_only").unwrap();
            let p_only = entries.iter().find(|e| e.record.name == "p_only").unwrap();
            let both = entries.iter().find(|e| e.record.name == "both").unwrap();
            assert!(!g_only.from_project);
            assert!(p_only.from_project);
            assert!(both.from_project, "project should shadow global");
            assert_eq!(both.record.url, "ssh://a@p-both");

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
            let local_path = RemoteRegistry::path(&proj, "explicit").unwrap();
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
            let dir = RemoteRegistry::dir(&g).unwrap();
            std::fs::write(dir.join("bad.toml"), b"not valid toml = !!!").unwrap();
            let err = read(&g, "bad").unwrap_err();
            assert!(format!("{err}").contains("remote read"));
        });
    }

    #[test]
    fn resolve_run_target_accepts_inline_url() {
        with_isolated_home("remote-resolve-inline", || {
            let g = pillbox::global();
            let r = resolve_run_target(&g, "docker://deploy@vps:2222").unwrap();
            // The URL doubles as the name for an inline (unregistered) remote.
            assert_eq!(r.name, "docker://deploy@vps:2222");
            assert_eq!(r.url, "docker://deploy@vps:2222");
            assert_eq!(r.default_agent, None);
        });
    }

    #[test]
    fn resolve_run_target_rejects_bad_inline_scheme() {
        with_isolated_home("remote-resolve-badscheme", || {
            let g = pillbox::global();
            let err = resolve_run_target(&g, "ftp://nope").unwrap_err();
            assert!(format!("{err}").contains("unsupported URL scheme"));
        });
    }

    #[test]
    fn resolve_run_target_unknown_name_errors() {
        with_isolated_home("remote-resolve-missing", || {
            let g = pillbox::global();
            let err = resolve_run_target(&g, "no-such-remote").unwrap_err();
            assert!(format!("{err}").contains("not found"));
        });
    }
}
