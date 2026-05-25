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
        if name.is_empty() {
            return Err(PillboxError::usage(
                "run",
                format!("--mcp `{raw}` NAME must be non-empty"),
            )
            .into());
        }
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
/// flags were passed. Carries the live tempfile (kept alive via the
/// struct's lifetime — drop deletes the file), the pre-formed docker
/// `-v <host>:<guest>:ro` mount string, and the per-invocation argv
/// the agent CLI needs to load the config (e.g. Claude's
/// `--mcp-config <path>`). The caller doesn't need to know the guest
/// path — the adapter has already baked it into both fields.
pub(crate) struct McpInjection {
    _tempfile: tempfile::NamedTempFile,
    pub(crate) docker_mount: String,
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
        docker_mount: format!("{}:{guest_path}:ro", tempfile.path().display()),
        extra_argv: vec!["--mcp-config".into(), guest_path.into()],
        _tempfile: tempfile,
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
