//! `--mcp NAME=URL` shared-MCP attachment plumbing.
//!
//! Pillbox's job here is wiring only: parse the flag, rewrite
//! host-local URLs so they're reachable from inside the sandbox,
//! and render the per-agent config the user's persistent home
//! would never see. The MCP server itself is somebody else's
//! problem (host process, lifecycle, supervision, auth at the
//! provider — all out of scope). See `docs/shared-mcp.md`.

use std::io::Write;
use std::str::FromStr;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::errors::PillboxError;
use crate::url_safety;

/// One `--mcp NAME=URL` after parsing + URL rewriting. `token` is
/// `None` at CLI-parse time and gets populated by
/// [`resolve_tokens`] when a matching `--mcp-token NAME=SECRET_NAME`
/// is present, by reading the value from the pillbox secret store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpAttachment {
    pub(crate) name: String,
    pub(crate) url: String,
    pub(crate) token: Option<String>,
}

/// One `--mcp-token NAME=SECRET_NAME` after parsing. `mcp_name`
/// must match a `--mcp NAME=URL`; `secret_name` is looked up in
/// the pillbox secret store at run time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpTokenSpec {
    pub(crate) mcp_name: String,
    pub(crate) secret_name: String,
}

impl McpAttachment {
    /// Parse a raw `--mcp` value. Returns the attachment with the
    /// URL rewritten to `host.docker.internal` if the host
    /// component was `localhost` or `127.0.0.1`.
    pub(crate) fn parse(raw: &str) -> Result<Self> {
        let (name, url) = raw
            .split_once('=')
            .ok_or_else(|| PillboxError::usage("run", format!("--mcp `{raw}` must be NAME=URL")))?;
        validate_name(name)
            .map_err(|e| PillboxError::usage("run", format!("--mcp `{raw}` {e}")))?;
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(PillboxError::usage(
                "run",
                format!("--mcp `{raw}` URL must start with http:// or https://"),
            )
            .into());
        }
        Ok(Self {
            name: name.to_string(),
            url: rewrite_localhost(url),
            token: None,
        })
    }
}

impl McpTokenSpec {
    pub(crate) fn parse(raw: &str) -> Result<Self> {
        let (mcp_name, secret_name) = raw.split_once('=').ok_or_else(|| {
            PillboxError::usage(
                "run",
                format!("--mcp-token `{raw}` must be NAME=SECRET_NAME"),
            )
        })?;
        validate_name(mcp_name)
            .map_err(|e| PillboxError::usage("run", format!("--mcp-token `{raw}` {e}")))?;
        if secret_name.is_empty() {
            return Err(PillboxError::usage(
                "run",
                format!("--mcp-token `{raw}` SECRET_NAME must be non-empty"),
            )
            .into());
        }
        Ok(Self {
            mcp_name: mcp_name.to_string(),
            secret_name: secret_name.to_string(),
        })
    }
}

impl FromStr for McpTokenSpec {
    type Err = String;

    fn from_str(raw: &str) -> std::result::Result<Self, Self::Err> {
        Self::parse(raw).map_err(|e| e.to_string())
    }
}

/// Resolve `--mcp-token NAME=SECRET_NAME` specs against the
/// `--mcp NAME=URL` set: validate every token references a real
/// attachment, look up secret values from the pillbox store, and
/// return a new attachment list with `token` populated where
/// applicable. Errors on unmatched mcp_name, unknown secret, or
/// env-var-name collision in the codex transform.
pub(crate) fn resolve_tokens(
    resolved: &crate::pillbox::Pillbox,
    mut attachments: Vec<McpAttachment>,
    tokens: &[McpTokenSpec],
) -> Result<Vec<McpAttachment>> {
    // Reject duplicate `--mcp NAME` up front. Every adapter keys its
    // config by NAME (a JSON object key for Claude/OpenCode, a TOML
    // table segment for Codex), so a repeated name would silently
    // last-write-win — and worse, a `--mcp-token` would attach to the
    // first entry only to have it overwritten by the un-tokened
    // duplicate. Fail loudly instead.
    let mut name_seen: std::collections::HashSet<&str> =
        std::collections::HashSet::with_capacity(attachments.len());
    for a in &attachments {
        if !name_seen.insert(a.name.as_str()) {
            return Err(PillboxError::usage(
                "run",
                format!("--mcp NAME `{}` given more than once", a.name),
            )
            .into());
        }
    }

    // Codex needs each token in a distinct env var. The transform
    // `NAME → uppercase + '-' → '_'` can collide (`code-search`
    // vs `code_search`); detect that early so the user gets a
    // clear error instead of a silent overwrite at run time.
    let mut env_var_seen: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for t in tokens {
        let env = codex_env_var_name(&t.mcp_name);
        if let Some(other) = env_var_seen.insert(env.clone(), t.mcp_name.clone()) {
            if other != t.mcp_name {
                return Err(PillboxError::usage(
                    "run",
                    format!(
                        "--mcp-token names `{}` and `{}` both collapse to env var `{env}` \
                         in the codex injection — pick one or rename so they differ after \
                         uppercasing and `-`→`_` substitution",
                        other, t.mcp_name
                    ),
                )
                .into());
            }
        }
    }

    for t in tokens {
        let target = attachments
            .iter_mut()
            .find(|a| a.name == t.mcp_name)
            .ok_or_else(|| {
                PillboxError::usage(
                    "run",
                    format!(
                        "--mcp-token `{}={}` doesn't match any --mcp NAME=URL",
                        t.mcp_name, t.secret_name
                    ),
                )
            })?;
        let value = crate::secrets::read(resolved, &t.secret_name)?.ok_or_else(|| {
            PillboxError::runtime("run", format!("secret `{}` not found", t.secret_name))
                .with_next(format!("pillbox secret add {}", t.secret_name))
        })?;
        target.token = Some(value.trim().to_string());
    }
    Ok(attachments)
}

/// Env var name codex's `bearer_token_env_var` references and that
/// pillbox sets in the container env. Uppercases NAME and maps
/// `-` to `_` so the result is a portable env-var identifier.
fn codex_env_var_name(name: &str) -> String {
    let mut out = String::with_capacity("PILLBOX_MCP_TOKEN_".len() + name.len());
    out.push_str("PILLBOX_MCP_TOKEN_");
    for c in name.chars() {
        out.push(match c {
            '-' => '_',
            other => other.to_ascii_uppercase(),
        });
    }
    out
}

/// NAME is used unquoted in agent-side config keys — JSON object
/// keys for Claude, TOML table segments for Codex. Restrict to a
/// safe identifier shape so neither adapter has to escape, and so
/// a stray dot doesn't accidentally nest a Codex `-c` override.
fn validate_name(name: &str) -> std::result::Result<(), &'static str> {
    if name.is_empty() {
        return Err("NAME must be non-empty");
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_alphabetic() {
        return Err("NAME must start with a letter (a-z, A-Z)");
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err("NAME must be ASCII alphanumeric / `_` / `-` only");
    }
    Ok(())
}

impl FromStr for McpAttachment {
    type Err = String;

    /// `FromStr` so clap's `value_parser` can parse `Vec<McpAttachment>`
    /// directly. We surrender the `PillboxError::with_next` context
    /// at this boundary — clap renders the plain string, and that's
    /// fine for what's currently a one-line "NAME=URL" usage hint.
    fn from_str(raw: &str) -> std::result::Result<Self, Self::Err> {
        Self::parse(raw).map_err(|e| e.to_string())
    }
}

/// What an agent adapter hands back to `DockerBackend` when `--mcp …`
/// flags were passed. The shape covers both file-based loaders
/// (Claude reads a JSON config file, so the adapter produces a
/// tempfile + docker mount + `--mcp-config` argv) and CLI-flag
/// loaders (Codex takes `-c mcp_servers.NAME.url=...`, so just
/// argv — no tempfile, no mount). Live tempfile is held to keep
/// the file on disk until the docker call returns.
#[derive(Debug)]
pub(crate) struct McpInjection {
    _tempfile: Option<tempfile::NamedTempFile>,
    pub(crate) docker_mount: Option<String>,
    pub(crate) extra_argv: Vec<String>,
    /// Container env vars the backend should `-e K=V`. Used by the
    /// codex adapter to land bearer tokens out-of-band so they
    /// never appear in argv (and so `ps` on the host can't see
    /// them). Claude folds tokens into the 0600 tempfile JSON
    /// directly and leaves this empty.
    pub(crate) env_vars: Vec<(String, String)>,
}

/// Adapter for Claude: write `--mcp` attachments to a tempfile, mount
/// at `/etc/pillbox/mcp.json`, point `claude` at it with
/// `--mcp-config`. Additive with the persistent `~/.claude.json`
/// config Claude loads from `/home/pillbox`.
///
/// The tempfile defaults to mode 0600 (mkstemp); today's
/// `--mcp NAME=URL` values don't carry secrets, but a future
/// `--mcp-token` flow would fold bearer headers into the rendered
/// JSON, so the perms need to hold even before then.
pub(crate) fn claude_inject(attachments: &[McpAttachment]) -> Result<McpInjection> {
    let guest_path = "/etc/pillbox/mcp.json";
    let mut tempfile = tempfile::Builder::new()
        .prefix("pillbox-mcp-")
        .suffix(".json")
        .tempfile()
        .context("create per-run MCP config tempfile")?;
    tempfile
        .as_file_mut()
        .write_all(&claude_config_bytes(attachments))
        .context("write per-run MCP config")?;
    Ok(McpInjection {
        docker_mount: Some(format!("{}:{guest_path}:ro", tempfile.path().display())),
        extra_argv: vec!["--mcp-config".into(), guest_path.into()],
        _tempfile: Some(tempfile),
        env_vars: Vec::new(),
    })
}

/// Adapter for Codex: pass each attachment as `-c
/// mcp_servers.NAME.url="URL"`. Codex's `-c` flag merges with
/// `~/.codex/config.toml` (CLI overrides take highest precedence)
/// so this is additive without touching the persistent home, and
/// the URL never lands on disk.
///
/// Codex MCP servers natively support HTTP — the same `url` field
/// the smoke test uses for Claude. Bearer auth for a future
/// `--mcp-token` flow lines up with codex's `bearer_token_env_var`
/// natively, no extra plumbing needed.
pub(crate) fn codex_inject(attachments: &[McpAttachment]) -> Result<McpInjection> {
    let mut extra_argv = Vec::with_capacity(attachments.len() * 2);
    let mut env_vars = Vec::new();
    for a in attachments {
        // The URL goes into a TOML basic string at codex's `-c`
        // parser. Bare HTTP URLs don't contain `"`, `\`, or control
        // chars, but reject anything that does so we can't get
        // tricked into escaping out of the value.
        if a.url.contains('"') || a.url.contains('\\') || a.url.chars().any(|c| c.is_control()) {
            return Err(PillboxError::usage(
                "run",
                format!(
                    "--mcp `{}` URL contains characters that can't go in a codex `-c` override (\", \\, or control chars)",
                    a.name
                ),
            )
            .into());
        }
        extra_argv.push("-c".into());
        extra_argv.push(format!(r#"mcp_servers.{}.url="{}""#, a.name, a.url));
        if let Some(token) = &a.token {
            // Stash the token in an env var instead of inlining it
            // in `-c http_headers.Authorization="Bearer …"` — the
            // env-var-indirection keeps the secret out of `ps` argv
            // on the host. The env var name is unique per
            // attachment (collision-checked in `resolve_tokens`).
            let env = codex_env_var_name(&a.name);
            extra_argv.push("-c".into());
            extra_argv.push(format!(
                r#"mcp_servers.{}.bearer_token_env_var="{}""#,
                a.name, env
            ));
            env_vars.push((env, token.clone()));
        }
    }
    Ok(McpInjection {
        _tempfile: None,
        docker_mount: None,
        extra_argv,
        env_vars,
    })
}

/// Adapter for OpenCode: write `--mcp` attachments to a tempfile as an
/// `opencode.json` config with `mcp` entries, mount it read-only, and
/// set `OPENCODE_CONFIG` env var to point at the guest path. OpenCode
/// merges configs rather than replacing them, so this is additive with
/// the user's global `~/.config/opencode/opencode.json` (which loads at
/// lower precedence). Caveat: a project `opencode.json` in the mounted
/// workspace loads at *higher* precedence than `OPENCODE_CONFIG`, so a
/// workspace MCP entry sharing a `--mcp` NAME would override ours.
pub(crate) fn opencode_inject(attachments: &[McpAttachment]) -> Result<McpInjection> {
    let guest_path = "/etc/pillbox/opencode-mcp.json";
    let mut tempfile = tempfile::Builder::new()
        .prefix("pillbox-opencode-mcp-")
        .suffix(".json")
        .tempfile()
        .context("create per-run OpenCode MCP config tempfile")?;
    tempfile
        .as_file_mut()
        .write_all(&opencode_config_bytes(attachments))
        .context("write per-run OpenCode MCP config")?;
    Ok(McpInjection {
        docker_mount: Some(format!("{}:{guest_path}:ro", tempfile.path().display())),
        extra_argv: Vec::new(),
        _tempfile: Some(tempfile),
        env_vars: vec![("OPENCODE_CONFIG".into(), guest_path.into())],
    })
}

/// JSON config OpenCode loads via `OPENCODE_CONFIG`. Remote MCP
/// entries take `type: "remote"`, `url`, and optional `headers`.
/// When an attachment carries a token, it's folded into
/// `headers.Authorization: Bearer <value>`.
pub(crate) fn opencode_config_bytes(attachments: &[McpAttachment]) -> Vec<u8> {
    let mut servers = serde_json::Map::new();
    for a in attachments {
        let mut entry = serde_json::Map::new();
        entry.insert("type".into(), Value::String("remote".into()));
        entry.insert("url".into(), Value::String(a.url.clone()));
        if let Some(token) = &a.token {
            let mut headers = serde_json::Map::new();
            headers.insert(
                "Authorization".into(),
                Value::String(format!("Bearer {token}")),
            );
            entry.insert("headers".into(), Value::Object(headers));
        }
        servers.insert(a.name.clone(), Value::Object(entry));
    }
    let config: Value = json!({ "mcp": Value::Object(servers) });
    // serde_json on a Map<String, Value> only fails on non-string keys
    // (we have none) or a writer error (in-memory Vec doesn't fail).
    serde_json::to_vec_pretty(&config).expect("in-memory JSON serialization is infallible")
}

/// Rewrite a loopback host in `url` to `host.docker.internal` so
/// the sandbox can reach a host-bound server. Loopback set comes
/// from [`url_safety::is_loopback_host`] (localhost, 127.0.0.0/8,
/// `::1` / `[::1]`, `*.localhost`). Everything else — paths,
/// query, fragment, userinfo, port, non-loopback host — passes
/// through unchanged.
fn rewrite_localhost(url: &str) -> String {
    let Some(host) = url_safety::host_of(url) else {
        return url.to_string();
    };
    if !url_safety::is_loopback_host(host) {
        return url.to_string();
    }
    // `host` is a borrowed sub-slice of `url`, so pointer arithmetic
    // gives us the exact splice points without re-scanning.
    let host_start = host.as_ptr() as usize - url.as_ptr() as usize;
    let host_end = host_start + host.len();
    let mut out = String::with_capacity(url.len() + 16);
    out.push_str(&url[..host_start]);
    out.push_str("host.docker.internal");
    out.push_str(&url[host_end..]);
    out
}

/// JSON config Claude loads via `--mcp-config <path>`. Format
/// per Claude Code's documented `mcpServers` schema; HTTP
/// transport entries take `type: "http"` and `url`. When an
/// attachment carries a token, it's folded into a `headers`
/// map as `Authorization: Bearer <value>` — the tempfile is
/// 0600 so the token stays off-disk-for-other-users and
/// out-of-argv.
pub(crate) fn claude_config_bytes(attachments: &[McpAttachment]) -> Vec<u8> {
    let mut servers = serde_json::Map::new();
    for a in attachments {
        let mut entry = serde_json::Map::new();
        entry.insert("type".into(), Value::String("http".into()));
        entry.insert("url".into(), Value::String(a.url.clone()));
        if let Some(token) = &a.token {
            let mut headers = serde_json::Map::new();
            headers.insert(
                "Authorization".into(),
                Value::String(format!("Bearer {token}")),
            );
            entry.insert("headers".into(), Value::Object(headers));
        }
        servers.insert(a.name.clone(), Value::Object(entry));
    }
    let config: Value = json!({ "mcpServers": Value::Object(servers) });
    // serde_json on a Map<String, Value> only fails on non-string keys
    // (we have none) or a writer error (in-memory Vec doesn't fail).
    serde_json::to_vec_pretty(&config).expect("in-memory JSON serialization is infallible")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_http_and_https() {
        let a = McpAttachment::parse("mem0=http://localhost:7777").unwrap();
        assert_eq!(a.name, "mem0");
        assert_eq!(a.url, "http://host.docker.internal:7777");

        let b = McpAttachment::parse("memo=https://mem0.example.com/v1").unwrap();
        assert_eq!(b.url, "https://mem0.example.com/v1");
    }

    #[test]
    fn parse_rewrites_127_0_0_1_and_localhost() {
        assert_eq!(
            McpAttachment::parse("x=http://127.0.0.1:8000").unwrap().url,
            "http://host.docker.internal:8000"
        );
        assert_eq!(
            McpAttachment::parse("x=http://localhost:8000/path?q=1")
                .unwrap()
                .url,
            "http://host.docker.internal:8000/path?q=1"
        );
    }

    #[test]
    fn parse_rewrites_wider_loopback_set() {
        // Anything in 127.0.0.0/8 is loopback — `--mcp x=127.5.5.5`
        // used to silently pass through and break inside the sandbox.
        assert_eq!(
            McpAttachment::parse("x=http://127.5.5.5:7000/")
                .unwrap()
                .url,
            "http://host.docker.internal:7000/"
        );
        // IPv6 loopback, bracketed with port.
        assert_eq!(
            McpAttachment::parse("x=http://[::1]:8080/mcp").unwrap().url,
            "http://host.docker.internal:8080/mcp"
        );
        // IPv6 loopback, bracketed, no port.
        assert_eq!(
            McpAttachment::parse("x=http://[::1]/").unwrap().url,
            "http://host.docker.internal/"
        );
        // `*.localhost` per RFC 6761 also resolves to loopback.
        assert_eq!(
            McpAttachment::parse("x=http://mem0.localhost:7777")
                .unwrap()
                .url,
            "http://host.docker.internal:7777"
        );
    }

    #[test]
    fn parse_leaves_other_hosts_alone() {
        assert_eq!(
            McpAttachment::parse("x=http://canopy.internal:9000/")
                .unwrap()
                .url,
            "http://canopy.internal:9000/"
        );
        assert_eq!(
            McpAttachment::parse("x=https://api.mem0.ai/mcp")
                .unwrap()
                .url,
            "https://api.mem0.ai/mcp"
        );
    }

    #[test]
    fn parse_rejects_missing_equals() {
        let err = McpAttachment::parse("just-a-name").unwrap_err();
        assert!(err.to_string().contains("must be NAME=URL"), "{err}");
    }

    #[test]
    fn parse_rejects_empty_name() {
        let err = McpAttachment::parse("=http://x").unwrap_err();
        assert!(err.to_string().contains("NAME must be non-empty"), "{err}");
    }

    #[test]
    fn parse_rejects_unsafe_names() {
        // Dots would nest the codex `-c mcp_servers.NAME.url=…`
        // override into a sub-table.
        let err = McpAttachment::parse("mem0.local=http://x:1/").unwrap_err();
        assert!(err.to_string().contains("alphanumeric"), "{err}");
        // Brackets / quotes / `=` would break TOML or JSON keys.
        for bad in ["a=b=http://x", r#"a"b=http://x"#, "[a]=http://x"] {
            let err = McpAttachment::parse(bad).unwrap_err();
            assert!(
                err.to_string().contains("NAME") || err.to_string().contains("must"),
                "{bad}: {err}"
            );
        }
        // Must start with a letter, not a digit.
        let err = McpAttachment::parse("9mem=http://x:1/").unwrap_err();
        assert!(err.to_string().contains("start with a letter"), "{err}");
    }

    #[test]
    fn codex_inject_emits_one_c_override_per_attachment() {
        let attachments = vec![
            McpAttachment {
                name: "smoke".into(),
                url: "http://host.docker.internal:8000/mcp/".into(),
                token: None,
            },
            McpAttachment {
                name: "mem0".into(),
                url: "http://host.docker.internal:7777".into(),
                token: None,
            },
        ];
        let injection = codex_inject(&attachments).unwrap();
        assert!(injection.docker_mount.is_none(), "codex needs no mount");
        assert_eq!(
            injection.extra_argv,
            vec![
                "-c".to_string(),
                r#"mcp_servers.smoke.url="http://host.docker.internal:8000/mcp/""#.to_string(),
                "-c".to_string(),
                r#"mcp_servers.mem0.url="http://host.docker.internal:7777""#.to_string(),
            ]
        );
    }

    #[test]
    fn codex_inject_rejects_toml_meta_in_url() {
        // Quote in the URL would let an attacker close the TOML
        // basic string and inject further keys via `-c`. The URL
        // parser would normally reject these, but defense in depth.
        let bad = vec![McpAttachment {
            name: "x".into(),
            url: r#"http://host"injected.toml=1/"#.into(),
            token: None,
        }];
        let err = codex_inject(&bad).unwrap_err();
        assert!(err.to_string().contains("codex `-c`"), "{err}");
    }

    #[test]
    fn token_spec_parse_valid_and_invalid() {
        let ok = McpTokenSpec::parse("mem0=MEM0_API_KEY").unwrap();
        assert_eq!(ok.mcp_name, "mem0");
        assert_eq!(ok.secret_name, "MEM0_API_KEY");

        for bad in [
            "missing-equals",
            "=NO_NAME",
            "ok-name=", // empty SECRET_NAME
            "9bad=X",   // NAME must start with letter
            "bad.name=X",
        ] {
            assert!(McpTokenSpec::parse(bad).is_err(), "{bad} should fail");
        }
    }

    #[test]
    fn codex_env_var_name_uppercases_and_dashes_to_underscores() {
        assert_eq!(codex_env_var_name("smoke"), "PILLBOX_MCP_TOKEN_SMOKE");
        assert_eq!(codex_env_var_name("mem0"), "PILLBOX_MCP_TOKEN_MEM0");
        assert_eq!(
            codex_env_var_name("code-search"),
            "PILLBOX_MCP_TOKEN_CODE_SEARCH"
        );
        // Underscore stays as-is and collides with hyphen form — the
        // resolver catches that as an explicit error.
        assert_eq!(
            codex_env_var_name("code_search"),
            "PILLBOX_MCP_TOKEN_CODE_SEARCH"
        );
    }

    #[test]
    fn claude_config_includes_authorization_header_when_token_present() {
        let attachments = vec![
            McpAttachment {
                name: "with_token".into(),
                url: "http://host.docker.internal:7777".into(),
                token: Some("sk-test-1234".into()),
            },
            McpAttachment {
                name: "no_token".into(),
                url: "http://host.docker.internal:8888".into(),
                token: None,
            },
        ];
        let bytes = claude_config_bytes(&attachments);
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let servers = parsed["mcpServers"].as_object().unwrap();
        assert_eq!(
            servers["with_token"]["headers"]["Authorization"],
            "Bearer sk-test-1234"
        );
        assert!(
            servers["no_token"].get("headers").is_none(),
            "no_token must not get a headers field"
        );
    }

    #[test]
    fn codex_inject_with_token_emits_bearer_env_var_and_arg() {
        let attachments = vec![McpAttachment {
            name: "mem0".into(),
            url: "http://host.docker.internal:7777".into(),
            token: Some("sk-test-1234".into()),
        }];
        let injection = codex_inject(&attachments).unwrap();
        // Argv has url override AND bearer_token_env_var override.
        assert!(
            injection
                .extra_argv
                .iter()
                .any(|s| s.contains(r#"mcp_servers.mem0.url="#)),
            "argv missing url override: {:?}",
            injection.extra_argv
        );
        assert!(
            injection
                .extra_argv
                .iter()
                .any(|s| s
                    .contains(r#"mcp_servers.mem0.bearer_token_env_var="PILLBOX_MCP_TOKEN_MEM0""#)),
            "argv missing bearer_token_env_var override: {:?}",
            injection.extra_argv
        );
        // The token value lands in env_vars, NOT in argv.
        assert_eq!(
            injection.env_vars,
            vec![(
                "PILLBOX_MCP_TOKEN_MEM0".to_string(),
                "sk-test-1234".to_string()
            )]
        );
        for arg in &injection.extra_argv {
            assert!(
                !arg.contains("sk-test-1234"),
                "token leaked into argv: {arg}"
            );
        }
    }

    #[test]
    fn resolve_tokens_populates_matching_attachment() {
        use crate::pillbox;
        use crate::secrets::{self, AddSource, WriteScope};
        use crate::test_util::with_isolated_home;
        with_isolated_home("mcp-resolve-tokens-ok", || {
            let g = pillbox::global();
            std::env::set_var("__PB_TEST_MCP_TOKEN", "raw-secret-value");
            secrets::add(
                &g,
                WriteScope::Resolved,
                "MEM0_TOKEN",
                AddSource::EnvVar("__PB_TEST_MCP_TOKEN".into()),
                false,
                None,
            )
            .unwrap();
            std::env::remove_var("__PB_TEST_MCP_TOKEN");

            let attachments = vec![McpAttachment {
                name: "mem0".into(),
                url: "http://host.docker.internal:7777".into(),
                token: None,
            }];
            let tokens = vec![McpTokenSpec {
                mcp_name: "mem0".into(),
                secret_name: "MEM0_TOKEN".into(),
            }];
            let resolved = resolve_tokens(&g, attachments, &tokens).unwrap();
            assert_eq!(resolved[0].token.as_deref(), Some("raw-secret-value"));
        });
    }

    #[test]
    fn resolve_tokens_errors_on_duplicate_mcp_name() {
        use crate::pillbox;
        use crate::test_util::with_isolated_home;
        with_isolated_home("mcp-resolve-tokens-dup", || {
            let g = pillbox::global();
            let attachments = vec![
                McpAttachment {
                    name: "mem0".into(),
                    url: "http://x:1/".into(),
                    token: None,
                },
                McpAttachment {
                    name: "mem0".into(),
                    url: "http://x:2/".into(),
                    token: None,
                },
            ];
            let err = resolve_tokens(&g, attachments, &[]).unwrap_err();
            assert!(err.to_string().contains("given more than once"), "{err}");
        });
    }

    #[test]
    fn resolve_tokens_errors_on_unknown_mcp_name() {
        use crate::pillbox;
        use crate::test_util::with_isolated_home;
        with_isolated_home("mcp-resolve-tokens-unknown", || {
            let g = pillbox::global();
            let tokens = vec![McpTokenSpec {
                mcp_name: "ghost".into(),
                secret_name: "X".into(),
            }];
            let err = resolve_tokens(&g, Vec::new(), &tokens).unwrap_err();
            assert!(err.to_string().contains("doesn't match any --mcp"), "{err}");
        });
    }

    #[test]
    fn resolve_tokens_errors_on_env_var_collision() {
        use crate::pillbox;
        use crate::test_util::with_isolated_home;
        with_isolated_home("mcp-resolve-tokens-collision", || {
            let g = pillbox::global();
            let attachments = vec![
                McpAttachment {
                    name: "code-search".into(),
                    url: "http://x:1/".into(),
                    token: None,
                },
                McpAttachment {
                    name: "code_search".into(),
                    url: "http://x:2/".into(),
                    token: None,
                },
            ];
            let tokens = vec![
                McpTokenSpec {
                    mcp_name: "code-search".into(),
                    secret_name: "A".into(),
                },
                McpTokenSpec {
                    mcp_name: "code_search".into(),
                    secret_name: "B".into(),
                },
            ];
            let err = resolve_tokens(&g, attachments, &tokens).unwrap_err();
            assert!(err.to_string().contains("collapse to env var"), "{err}");
        });
    }

    #[test]
    fn parse_rejects_non_http_url() {
        let err = McpAttachment::parse("x=ftp://nope").unwrap_err();
        assert!(err.to_string().contains("must start with http"), "{err}");
        let err = McpAttachment::parse("x=stdio://something").unwrap_err();
        assert!(err.to_string().contains("must start with http"), "{err}");
    }

    #[test]
    fn claude_config_shape() {
        let attachments = vec![
            McpAttachment {
                name: "mem0".into(),
                url: "http://host.docker.internal:7777".into(),
                token: None,
            },
            McpAttachment {
                name: "canopy".into(),
                url: "http://host.docker.internal:7000".into(),
                token: None,
            },
        ];
        let bytes = claude_config_bytes(&attachments);
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let servers = parsed.get("mcpServers").unwrap().as_object().unwrap();
        assert_eq!(servers.len(), 2);
        assert_eq!(servers["mem0"]["type"], "http");
        assert_eq!(servers["mem0"]["url"], "http://host.docker.internal:7777");
        assert_eq!(servers["canopy"]["url"], "http://host.docker.internal:7000");
    }

    #[test]
    fn opencode_config_shape() {
        let attachments = vec![
            McpAttachment {
                name: "mem0".into(),
                url: "http://host.docker.internal:7777".into(),
                token: None,
            },
            McpAttachment {
                name: "canopy".into(),
                url: "http://host.docker.internal:7000".into(),
                token: None,
            },
        ];
        let bytes = opencode_config_bytes(&attachments);
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let servers = parsed.get("mcp").unwrap().as_object().unwrap();
        assert_eq!(servers.len(), 2);
        assert_eq!(servers["mem0"]["type"], "remote");
        assert_eq!(servers["mem0"]["url"], "http://host.docker.internal:7777");
        assert_eq!(servers["canopy"]["url"], "http://host.docker.internal:7000");
    }

    #[test]
    fn opencode_config_includes_authorization_header_when_token_present() {
        let attachments = vec![
            McpAttachment {
                name: "with_token".into(),
                url: "http://host.docker.internal:7777".into(),
                token: Some("sk-test-1234".into()),
            },
            McpAttachment {
                name: "no_token".into(),
                url: "http://host.docker.internal:8888".into(),
                token: None,
            },
        ];
        let bytes = opencode_config_bytes(&attachments);
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let servers = parsed["mcp"].as_object().unwrap();
        assert_eq!(
            servers["with_token"]["headers"]["Authorization"],
            "Bearer sk-test-1234"
        );
        assert!(
            servers["no_token"].get("headers").is_none(),
            "no_token must not get a headers field"
        );
    }

    #[test]
    fn opencode_inject_sets_opencode_config_env_and_mount() {
        let attachments = vec![McpAttachment {
            name: "smoke".into(),
            url: "http://host.docker.internal:8000/mcp/".into(),
            token: None,
        }];
        let injection = opencode_inject(&attachments).unwrap();
        assert!(injection.docker_mount.is_some());
        assert!(
            injection
                .docker_mount
                .as_ref()
                .unwrap()
                .contains("/etc/pillbox/opencode-mcp.json"),
            "mount should target guest path: {:?}",
            injection.docker_mount
        );
        assert_eq!(injection.extra_argv, Vec::<String>::new());
        assert_eq!(
            injection.env_vars,
            vec![(
                "OPENCODE_CONFIG".into(),
                "/etc/pillbox/opencode-mcp.json".into()
            )]
        );
    }
}
