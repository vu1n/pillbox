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

/// One `--mcp NAME=URL` after parsing + URL rewriting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpAttachment {
    pub(crate) name: String,
    pub(crate) url: String,
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
        })
    }
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

/// What an agent adapter hands back to `LocalDocker` when `--mcp …`
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
}

/// Adapter for Claude: write `--mcp` attachments to a tempfile, mount
/// at `/etc/pillbox/mcp.json`, point `claude` at it with
/// `--mcp-config`. Additive with the persistent `~/.claude.json`
/// config Claude loads from `/home/lum`.
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
    }
    Ok(McpInjection {
        _tempfile: None,
        docker_mount: None,
        extra_argv,
    })
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
/// transport entries take `type: "http"` and `url`.
pub(crate) fn claude_config_bytes(attachments: &[McpAttachment]) -> Vec<u8> {
    let mut servers = serde_json::Map::new();
    for a in attachments {
        servers.insert(
            a.name.clone(),
            json!({
                "type": "http",
                "url": a.url,
            }),
        );
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
            },
            McpAttachment {
                name: "mem0".into(),
                url: "http://host.docker.internal:7777".into(),
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
        }];
        let err = codex_inject(&bad).unwrap_err();
        assert!(err.to_string().contains("codex `-c`"), "{err}");
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
            },
            McpAttachment {
                name: "canopy".into(),
                url: "http://host.docker.internal:7000".into(),
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
}
