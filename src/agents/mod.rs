//! Per-agent adapters.
//!
//! v0.6: each agent's persistent HOME ("auth state") lives under the
//! resolved auth pillbox. PR 2 always resolves to the **global** pillbox
//! — one `claude login` is shared across every project pillbox. v0.7 may
//! expose a per-project auth override if real signal materializes.
//!
//! Storage shape: `<auth_pillbox>/auth/<provider>/`. That directory is
//! bind-mounted at `/home/pillbox` (the guest's HOME) for both login and run.
//! Whatever the agent writes — `.credentials.json`, settings, refresh
//! tokens — persists there naturally.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Context, Result};

use crate::pillbox::{self, Pillbox};
use crate::{docker, errors::PillboxError};

pub(crate) mod harness;
pub(crate) mod mcp;

pub(crate) use mcp::{McpAttachment, McpInjection, McpTokenSpec};

pub(crate) const GUEST_HOME: &str = "/home/pillbox";
pub(crate) const GUEST_WORKSPACE: &str = "/workspace";

/// Per-agent MCP config builder. Returns a fully-formed injection
/// (live tempfile + docker mount + extra argv); `None` on
/// `AgentSpec` means the agent doesn't support `--mcp` yet.
pub(crate) type McpInjectFn = fn(&[McpAttachment]) -> Result<McpInjection>;

/// How pillbox talks to an agent. Most agents are a TUI we wrap in a PTY and
/// observe by scraping their transcript file ([`Integration::Pty`]); a few
/// (opencode) run as a headless server with a structured event stream + a
/// prompt API, which pillbox drives/reads directly ([`Integration::Server`]) —
/// cleaner and bidirectional. See `events::opencode` + `sandbox::opencode`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Integration {
    /// PTY + transcript-file scrape (claude, codex, pi today).
    Pty,
    /// Headless HTTP server + SSE event stream + prompt API (opencode).
    Server,
}

/// The per-agent data a [`Integration::Server`] agent needs, beyond the common
/// [`AgentSpec`] fields — the §0 capture filename + its wire format, and whether
/// the agent only runs on the libkrun backend. `Some` iff `integration == Server`.
/// Carrying these here (rather than re-deriving `if agent_id == "…"` at each
/// dispatch site) keeps the events-file name, drain-format selection, and
/// docker-capability as one source of truth.
#[derive(Clone, Copy)]
pub(crate) struct ServerProfile {
    /// Capture filename under the agent home (the §0 source the host drains).
    /// Consumed by the libkrun §0 read path; dead on a non-libkrun build.
    #[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
    pub(crate) events_file: &'static str,
    /// Wire format of that capture → selects the drain function. Consumed by the
    /// libkrun §0 read path; dead on a non-libkrun build.
    #[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
    pub(crate) events_format: crate::events::EventsFormat,
    /// True for server agents that only run on libkrun (docker rejects them).
    pub(crate) libkrun_only: bool,
}

#[derive(Clone, Copy)]
pub struct AgentSpec {
    pub(crate) id: &'static str,
    /// Auth-home owner. Normally equals [`id`](Self::id), so each agent has its
    /// own `auth/<id>/`. An alias agent that drives the *same* underlying CLI a
    /// different way (`codex-serve` → `codex`'s `app-server`) points this at the
    /// owner's id so one `auth login` covers both and the alias never needs its
    /// own credential store. [`home_dir`](Self::home_dir) keys on this.
    pub(crate) auth_id: &'static str,
    /// How pillbox runs + observes this agent (PTY-and-scrape vs server-API).
    pub(crate) integration: Integration,
    pub(crate) cred_sentinel: &'static str,
    pub(crate) login_argv: &'static [&'static str],
    pub(crate) run_argv: &'static [&'static str],
    pub(crate) oauth_port: Option<u16>,
    pub(crate) post_login_finalize: Option<fn(&Path) -> Result<()>>,
    pub(crate) vault_capable: bool,
    /// Build a per-run MCP config injection from `--mcp` flags.
    /// `None` means this agent doesn't support `--mcp` yet —
    /// the backend hard-errors at run time. Per-agent config-file
    /// mechanics differ enough (Claude `.mcp.json` vs Codex
    /// `config.toml`) that the rendering lives in the `mcp` module
    /// rather than in shared run code.
    pub(crate) mcp_inject: Option<McpInjectFn>,
    /// Args injected before the user's `-- args` on every sandboxed run. The
    /// sandbox is the isolation boundary, so we default to a less-interactive
    /// permission posture (e.g. claude `--permission-mode auto`) — the user's
    /// own `-- args` come after and override (claude takes the last value).
    pub(crate) sandbox_args: &'static [&'static str],
    /// Per-run prep on the agent home before launch, given the home and the
    /// guest workspace path. Used to pre-accept claude's workspace trust dialog
    /// and mark first-run onboarding complete so an interactive run doesn't
    /// stall on either gate (see [`pretrust_claude_workspace`]). `None` =
    /// nothing to prepare.
    pub(crate) prepare_workspace: Option<fn(&Path, &str) -> Result<()>>,
    /// Server-mode data ([`ServerProfile`]). `Some` iff `integration == Server`;
    /// `None` for PTY agents. The capture-file + drain-format + backend-capability
    /// dispatch reads this instead of matching on `id`.
    pub(crate) server: Option<ServerProfile>,
}

pub const CLAUDE: AgentSpec = AgentSpec {
    id: "claude",
    auth_id: "claude",
    integration: Integration::Pty,
    cred_sentinel: ".claude/.credentials.json",
    login_argv: &["claude", "auth", "login", "--claudeai"],
    run_argv: &["claude"],
    oauth_port: Some(54545),
    post_login_finalize: Some(finalize_claude_onboarding),
    vault_capable: true,
    mcp_inject: Some(mcp::claude_inject),
    // The sandbox is the boundary: default to auto-accepting actions so a
    // driven/interactive session isn't stalled on per-tool prompts. (Full
    // `bypassPermissions`/`--dangerously-skip-permissions` is refused by claude
    // as root, which the runner runs as; `auto` is the strongest mode that
    // works without dropping to a non-root user — a future image change.)
    sandbox_args: &["--permission-mode", "auto"],
    prepare_workspace: Some(pretrust_claude_workspace),
    server: None,
};

pub const CODEX: AgentSpec = AgentSpec {
    id: "codex",
    auth_id: "codex",
    integration: Integration::Pty,
    cred_sentinel: ".codex/auth.json",
    login_argv: &["codex", "login", "--device-auth"],
    run_argv: &["codex"],
    oauth_port: None,
    post_login_finalize: None,
    vault_capable: true,
    mcp_inject: Some(mcp::codex_inject),
    sandbox_args: &[],
    prepare_workspace: None,
    server: None,
};

/// codex driven through `codex app-server` (its JSON-RPC-over-stdio protocol,
/// the channel the VS Code extension uses) instead of the interactive TUI — a
/// structured [`Integration::Server`] sibling of [`CODEX`], the way `opencode`
/// is a server agent. Shares `codex`'s auth home ([`auth_id`](AgentSpec::auth_id)
/// `= "codex"`): one `pillbox auth login --agent codex` covers both. The PTY
/// `codex` stays the default; this is opt-in via `--agent codex-serve`, so if
/// upstream ever closes the app-server surface the TUI path is unaffected.
///
/// libkrun-only today (the server bring-up lives in the microVM run path; docker
/// rejects it via [`ServerProfile::libkrun_only`]). **Non-vault v1**: the
/// app-server's model egress is `wss://api.openai.com/v1/responses` (WebSocket,
/// `api.openai.com`), which the [`codex` vault provider](crate::vault::providers)
/// (chatgpt.com only) doesn't intercept — so `--vault` is rejected until that
/// interception lands. `vault_capable` stays `true` (the intent); the run path
/// gates the rejection.
pub const CODEX_SERVE: AgentSpec = AgentSpec {
    id: "codex-serve",
    auth_id: "codex",
    integration: Integration::Server,
    cred_sentinel: ".codex/auth.json",
    // No standalone login: shares codex's auth home, so `auth login --agent
    // codex` is the path. Point login_argv at codex's flow for the rare direct
    // `auth login --agent codex-serve` (writes the same shared home).
    login_argv: &["codex", "login", "--device-auth"],
    // Unused for a Server agent (the guest entrypoint runs `codex app-server`
    // via the appserver-host bridge, not this argv) — kept honest, not launched.
    run_argv: &["codex", "app-server"],
    oauth_port: None,
    post_login_finalize: None,
    vault_capable: true,
    mcp_inject: Some(mcp::codex_inject),
    sandbox_args: &[],
    prepare_workspace: None,
    server: Some(ServerProfile {
        events_file: crate::sandbox::appserver_client::EVENTS_FILE,
        events_format: crate::events::EventsFormat::Ndjson,
        libkrun_only: true,
    }),
};

pub const OPENCODE: AgentSpec = AgentSpec {
    id: "opencode",
    auth_id: "opencode",
    integration: Integration::Server,
    cred_sentinel: ".local/share/opencode/auth.json",
    login_argv: &["opencode", "auth", "login"],
    run_argv: &["opencode"],
    oauth_port: None,
    post_login_finalize: None,
    vault_capable: false,
    mcp_inject: Some(mcp::opencode_inject),
    sandbox_args: &[],
    prepare_workspace: None,
    server: Some(ServerProfile {
        events_file: crate::sandbox::opencode::EVENTS_FILE,
        events_format: crate::events::EventsFormat::Sse,
        libkrun_only: false,
    }),
};

pub const PI: AgentSpec = AgentSpec {
    id: "pi",
    auth_id: "pi",
    integration: Integration::Pty,
    // pi (npm `@earendil-works/pi-coding-agent`) stores provider credentials —
    // OAuth tokens or API keys saved via `/login` — at `~/.pi/agent/auth.json`
    // (config dir `~/.pi`, agent subdir `agent`). Verified against pi 0.75.5.
    cred_sentinel: ".pi/agent/auth.json",
    // pi has no headless `login` subcommand; authentication is the interactive
    // `/login` slash command inside the TUI. Launching bare `pi` boots that TUI
    // in the sandbox so the user can run `/login` (the login path execs this
    // with a PTY, same as the other agents). The anthropic OAuth flow listens
    // on 127.0.0.1:53692 for its callback (verified in pi-ai's oauth module).
    login_argv: &["pi"],
    run_argv: &["pi"],
    oauth_port: Some(53692),
    post_login_finalize: None,
    // Not wired into the vault stub-swap proxy (no custom-CA / proxy routing
    // integration yet) — mirrors opencode. `--vault` and vaulted secrets are
    // rejected for pi until that lands.
    vault_capable: false,
    // No `--mcp` config injection for pi yet; the sandbox backend hard-errors
    // if `--mcp` is passed with `--agent pi`.
    mcp_inject: None,
    sandbox_args: &[],
    prepare_workspace: None,
    server: None,
};

pub const ALL: &[&AgentSpec] = &[&CLAUDE, &CODEX, &CODEX_SERVE, &OPENCODE, &PI];

/// Look up an agent spec by id, or return a usage error listing the
/// known ids. Centralized so every CLI surface that takes an
/// `--agent` / `agent` argument reports the same diagnostic.
pub(crate) fn lookup(action: &'static str, id: &str) -> Result<&'static AgentSpec> {
    ALL.iter().copied().find(|s| s.id() == id).ok_or_else(|| {
        let known: Vec<&str> = ALL.iter().map(|s| s.id()).collect();
        PillboxError::usage(
            action,
            format!("unknown agent `{id}` (known: {})", known.join(", ")),
        )
        .into()
    })
}

fn finalize_claude_onboarding(home: &Path) -> Result<()> {
    let path = home.join(".claude.json");
    if !path.exists() {
        return Ok(());
    }
    let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let mut value: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    let obj = value
        .as_object_mut()
        .ok_or_else(|| anyhow!("expected top-level JSON object in {}", path.display()))?;
    obj.insert(
        "hasCompletedOnboarding".to_string(),
        serde_json::Value::Bool(true),
    );
    let serialized = serde_json::to_string_pretty(&value)
        .with_context(|| format!("serialize {}", path.display()))?;
    fs::write(&path, serialized).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Pre-accept claude's workspace trust dialog for `guest_workspace` by seeding
/// its `~/.claude.json` project entry, AND mark global onboarding complete so a
/// fresh sandbox `$HOME` doesn't drop into the first-run wizard (theme picker)
/// before the chat prompt. pillbox owns the mounted workspace (the user pointed
/// `run` at it), so an interactive session shouldn't stall on either gate —
/// which `-p` auto-skips but a PTY-attached interactive run shows, and
/// `--dangerously-skip-permissions` can't bypass as root.
///
/// `hasCompletedOnboarding` is the same flag [`finalize_claude_onboarding`]
/// sets at login. Local docker inherits it via the bind-mounted global home;
/// a libkrun guest with a fresh `$HOME` would otherwise hit the wizard.
/// Setting it here (every launch, every backend) is idempotent and
/// backend-agnostic. Merges into any existing entry; creates the file +
/// `projects` map as needed.
/// Get `key` from `obj` as a mutable object, inserting an empty one if absent.
/// Errors if `key` is present but not a JSON object.
fn get_or_create_object<'a>(
    obj: &'a mut serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<&'a mut serde_json::Map<String, serde_json::Value>> {
    obj.entry(key.to_string())
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow!("`{key}` is not a JSON object in .claude.json"))
}

fn pretrust_claude_workspace(home: &Path, guest_workspace: &str) -> Result<()> {
    let path = home.join(".claude.json");
    let mut value: serde_json::Value = if path.exists() {
        let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?
    } else {
        serde_json::json!({})
    };
    let root = value
        .as_object_mut()
        .ok_or_else(|| anyhow!("expected top-level JSON object in {}", path.display()))?;
    // Global first-run gate (suppresses the theme-picker wizard); set before
    // drilling into the per-project entry below.
    root.insert("hasCompletedOnboarding".into(), true.into());
    let entry = get_or_create_object(get_or_create_object(root, "projects")?, guest_workspace)?;
    entry.insert("hasTrustDialogAccepted".into(), true.into());
    entry.insert("hasCompletedProjectOnboarding".into(), true.into());
    let serialized = serde_json::to_string_pretty(&value)
        .with_context(|| format!("serialize {}", path.display()))?;
    fs::write(&path, serialized).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

impl AgentSpec {
    pub(crate) fn id(&self) -> &'static str {
        self.id
    }

    /// True when this agent owns its auth home rather than sharing another's
    /// (`auth_id == id`). An alias like `codex-serve` (`auth_id == "codex"`)
    /// returns false, so auth-list surfaces only own one row per credential store.
    pub(crate) fn owns_auth_home(&self) -> bool {
        self.auth_id == self.id
    }

    /// Run [`prepare_workspace`](Self::prepare_workspace) if set, warning (not
    /// failing) on error — the gate it pre-accepts just reappears in-session, so
    /// a prep failure must never abort the run. Shared by every backend launch
    /// path so the best-effort-and-loud contract lives in one place.
    pub(crate) fn prepare_workspace_or_warn(&self, home: &Path, guest_cwd: &str) {
        if let Some(prepare) = self.prepare_workspace {
            if let Err(e) = prepare(home, guest_cwd) {
                eprintln!("pillbox: warning: workspace pre-trust failed: {e:#}");
            }
        }
    }

    /// Resolve the auth pillbox for this agent. PR 2: always global. PR 3+
    /// may consult the resolved pillbox's `[auth]` config to opt into a
    /// per-project override.
    pub(crate) fn auth_pillbox(&self, _resolved: &Pillbox) -> Pillbox {
        pillbox::global()
    }

    /// `<auth_pillbox>/auth/<auth_id>/` — created on first use, 0700. Keyed on
    /// [`auth_id`](Self::auth_id) (not `id`) so an alias agent (`codex-serve`)
    /// shares the owning agent's (`codex`) credential store.
    pub(crate) fn home_dir(&self, resolved: &Pillbox) -> Result<PathBuf> {
        let auth = self.auth_pillbox(resolved);
        let auth_root = auth.subdir("auth")?;
        let dir = auth_root.join(self.auth_id);
        fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod {} 0700", dir.display()))?;
        Ok(dir)
    }

    pub(crate) fn is_authenticated(&self, resolved: &Pillbox) -> bool {
        match self.home_dir(resolved) {
            Ok(home) => home.join(self.cred_sentinel).exists(),
            Err(_) => false,
        }
    }

    fn resolved_oauth_port(&self) -> Option<u16> {
        let default = self.oauth_port?;
        let var = format!("PILLBOX_{}_OAUTH_PORT", self.id.to_uppercase());
        let port = std::env::var(&var)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(default);
        Some(port)
    }

    pub(crate) fn login(&self, resolved: &Pillbox) -> Result<()> {
        let image = docker::check_ready_for(resolved)?;

        let home = self.home_dir(resolved)?;

        let mut args = base_docker_args();
        if let Some(port) = self.resolved_oauth_port() {
            args.push("-p".into());
            args.push(format!("{port}:{port}"));
        }
        args.push("-v".into());
        args.push(format!("{}:{GUEST_HOME}", home.display()));
        args.push(image);
        args.extend(self.login_argv.iter().map(|s| s.to_string()));

        println!("pillbox: starting {} login inside a sandbox.", self.id);
        println!("pillbox: follow the prompts (and any URL) printed by the sandbox below.");
        println!();

        let status = docker::run_interactive(&args)?;
        if !status.success() {
            return Err(PillboxError::runtime(
                "login",
                format!("{} exited with status {status}", self.id),
            )
            .with_next(format!("pillbox auth login --agent {}", self.id))
            .into());
        }

        if !home.join(self.cred_sentinel).exists() {
            return Err(PillboxError::runtime(
                "login",
                format!(
                    "{} completed but `{}` was not written",
                    self.id,
                    home.join(self.cred_sentinel).display()
                ),
            )
            .with_next(format!(
                "pillbox auth login --agent {}   # check the sandbox output above for clues",
                self.id
            ))
            .into());
        }

        if let Some(finalize) = self.post_login_finalize {
            if let Err(e) = finalize(&home) {
                eprintln!(
                    "pillbox: warning: could not finalize {} setup: {e}",
                    self.id
                );
            }
        }

        println!();
        println!(
            "pillbox: ✓ {} authenticated. State persisted at {}.",
            self.id,
            home.display()
        );
        println!(
            "pillbox: try `pillbox run --agent {}` to launch it.",
            self.id
        );
        Ok(())
    }

    pub(crate) fn forget(&self, resolved: &Pillbox) -> Result<bool> {
        let home = self.home_dir(resolved)?;
        if !home.exists() {
            return Ok(false);
        }
        fs::remove_dir_all(&home).with_context(|| format!("remove {}", home.display()))?;
        Ok(true)
    }
}

pub(crate) struct RunOpts {
    pub(crate) workspace: Option<PathBuf>,
    pub(crate) name: Option<String>,
    pub(crate) mounts: Vec<String>,
    /// `--with NAME[=ENV_VAR]` entries (parsed from raw CLI strings)
    pub(crate) withs: Vec<String>,
    /// `--env BUNDLE` names
    pub(crate) env_bundles: Vec<String>,
    /// `--env-file PATH` paths (ad-hoc, no persistence)
    pub(crate) env_files: Vec<PathBuf>,
    /// `--vault` — route API traffic through the pillbox stub-swap proxy.
    pub(crate) vault: bool,
    /// `--memory` — wire in the external `kypp` swarm-memory engine (host-side, best-effort):
    /// brief the agent from project memory at start, capture the §0 log after. pillbox attaches,
    /// doesn't own; see `crate::memory`.
    pub(crate) memory: bool,
    /// Claim handles `--memory` briefed this run with (set by `dispatch_run` from kypp's digest).
    /// The data channel into the backend so a reparented server agent's bring-up can stash it on the
    /// session for the post-drain usage capture (`session ingest`). Empty when --memory is off/empty.
    pub(crate) memory_briefed: Vec<String>,
    /// `--mcp NAME=URL` shared-MCP attachments, parsed at the CLI
    /// boundary. Resolved against `mcp_tokens` in the sandbox
    /// backend; the backend hard-errors if non-empty and the
    /// resolved agent has no `mcp_inject`.
    pub(crate) mcps: Vec<McpAttachment>,
    /// `--mcp-token NAME=SECRET_NAME` entries. Each references a
    /// `--mcp NAME=URL` and a pillbox-stored secret; values are
    /// read at run time, never inlined at CLI parse.
    pub(crate) mcp_tokens: Vec<McpTokenSpec>,
    pub(crate) args: Vec<String>,
    /// `--detach` — start the session and exit. The session is recorded
    /// in the per-pillbox registry and the user reattaches with `pillbox
    /// session attach <id>`. Supported on local Docker + libkrun backends
    /// (each persists the in-sandbox pty-host).
    pub(crate) detach: bool,
    /// `--label TEXT` — human label for a detached session, surfaced
    /// in `pillbox session list`. Only meaningful with `--detach`.
    pub(crate) label: Option<String>,
    /// `--json` — when `--detach` succeeds, emit the new session as
    /// `{"version":1,"session":{...}}` on stdout instead of the human
    /// "session started" banner. Lets orchestrators capture the id
    /// with `jq -r '.session.id'` instead of regex-scraping the
    /// banner. Only meaningful with `--detach`.
    pub(crate) json: bool,
    /// `--ttl DURATION` — session-retention TTL in seconds. Parsed
    /// from `30m`/`24h`/`7d` shapes at the CLI boundary via
    /// `session::parse_ttl_seconds`. Recorded on the session record
    /// as an absolute `expires_at` RFC3339 timestamp (not the raw
    /// duration), so `pillbox session prune` doesn't have to recompute
    /// from creation time. Only meaningful with `--detach`.
    pub(crate) ttl_seconds: Option<u64>,
    /// `--from-bookmark NAME` — select the snapshot bookmark used as
    /// the run's workspace base, restored into the workspace before launch.
    pub(crate) from_bookmark: Option<String>,
    /// `--model PROVIDER/MODEL` — for a `Server`-integration agent (opencode),
    /// the model to drive with (e.g. `zai-coding-plan/glm-4.5-air`). Recorded on
    /// the session and reused by every `session send`. `None` → a default.
    /// Ignored by PTY agents (they pick their own model interactively).
    pub(crate) model: Option<String>,
    /// `--egress-allow HOST` (repeatable) — hosts allowed through egress beyond
    /// the built-in set (the vault-intercepted providers + the standard
    /// model-provider profile). Two consumers: the libkrun egress fence, and the
    /// vault broker's default-deny allowlist (`egress_deny`). Invoker-set, so an
    /// untrusted workspace can't widen its own egress.
    pub(crate) egress_allow: Vec<String>,
    /// `--egress-deny` — turn on the vault broker's default-deny line: an
    /// outbound host with no credential provider and not on `egress_allow` is
    /// blocked at the proxy. Enforced only when a vault session runs (`--vault`
    /// or a vaulted `--with`); a no-op (with a warning) otherwise.
    pub(crate) egress_deny: bool,
}

#[allow(dead_code)]
impl RunOpts {
    /// The kypp project for `--memory` (the pillbox name, else "default"); `None` when --memory is
    /// off. Single-sources the derivation shared by `dispatch_run` and the server bring-up's stash.
    pub(crate) fn memory_project(&self) -> Option<String> {
        self.memory
            .then(|| self.name.clone().unwrap_or_else(|| "default".to_string()))
    }

    /// In v0.6 PR 2, the only pillbox.toml field that backs a RunOpts
    /// default is `name`. Multi-value defaults from v0.5 (`with`, `mount`,
    /// `env_file`, `env`) are dropped — they didn't carry their weight
    /// and made the descriptor sprawl.
    pub(crate) fn apply_defaults(&mut self, cfg: crate::config::Config) {
        if self.name.is_none() {
            self.name = cfg.name;
        }
    }
}

pub(crate) struct ResolvedWith {
    pub(crate) secret_name: String,
    pub(crate) env_var: String,
    pub(crate) meta: Option<crate::vault::VaultMeta>,
    pub(crate) raw_entry: String,
}

pub(crate) fn resolve_with_entries(
    resolved: &Pillbox,
    withs: &[String],
) -> Result<Vec<ResolvedWith>> {
    let mut out = Vec::with_capacity(withs.len());
    for entry in withs {
        let (secret_name, env_var) = match entry.split_once('=') {
            Some((s, e)) => (s.to_string(), e.to_string()),
            None => (entry.clone(), entry.clone()),
        };
        let meta = crate::secrets::read_meta(resolved, &secret_name)?;
        out.push(ResolvedWith {
            secret_name,
            env_var,
            meta,
            raw_entry: entry.clone(),
        });
    }
    Ok(out)
}

pub(crate) fn resolve_run_env(
    resolved: &Pillbox,
    opts: &RunOpts,
    withs: &[ResolvedWith],
    mut vault: Option<&mut crate::vault::VaultSession>,
) -> Result<std::collections::BTreeMap<String, String>> {
    use std::collections::BTreeMap;
    let mut env: BTreeMap<String, String> = BTreeMap::new();

    // Layer 1: stored env bundles (inheritance applies).
    for bundle_name in &opts.env_bundles {
        let vars = crate::envs::read(resolved, bundle_name)?.ok_or_else(|| {
            PillboxError::runtime("run", format!("env bundle `{bundle_name}` not found"))
                .with_next("pillbox env list  # see what's stored".to_string())
        })?;
        for (k, v) in vars {
            if let Some(prev) = env.insert(k.clone(), v) {
                eprintln!(
                    "pillbox: note: {k} shadowed by --env {bundle_name} (was set to `{}`)",
                    crate::secrets::mask(&prev)
                );
            }
        }
    }

    // Layer 2: ad-hoc .env files.
    for path in &opts.env_files {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            PillboxError::runtime(
                "run",
                format!("could not read --env-file {}: {e}", path.display()),
            )
        })?;
        let vars = crate::envs::parse_dotenv(&raw, &path.display().to_string())?;
        for (k, v) in vars {
            if let Some(_prev) = env.insert(k.clone(), v) {
                eprintln!(
                    "pillbox: note: {k} shadowed by --env-file {}",
                    path.display()
                );
            }
        }
    }

    // Layer 3: --with entries.
    for w in withs {
        let real_value = crate::secrets::read(resolved, &w.secret_name)?.ok_or_else(|| {
            PillboxError::runtime("run", format!("secret `{}` not found", w.secret_name))
                .with_next(format!("pillbox secret add {}", w.secret_name))
        })?;

        let injected = match (w.meta.as_ref(), vault.as_deref_mut()) {
            (Some(meta), Some(session)) => {
                session.lease_api_key(&w.secret_name, real_value.trim_end(), meta)?
            }
            (Some(_meta), None) => {
                return Err(PillboxError::runtime(
                    "run",
                    format!(
                        "secret `{}` is marked vaulted but no vault session is active",
                        w.secret_name
                    ),
                )
                .into());
            }
            (None, _) => real_value,
        };

        if let Some(_prev) = env.insert(w.env_var.clone(), injected) {
            eprintln!(
                "pillbox: note: {} shadowed by --with {}",
                w.env_var, w.raw_entry
            );
        }
    }

    Ok(env)
}

/// `docker run` args common to every local launch: host-gateway alias +
/// the HOME/TERM/PATH env scaffolding. `stdio_prefix` is the per-mode head:
/// `["-it", "--rm"]` for a foreground run, `["-d"]` for the detached
/// attach-transport flow.
fn base_docker_args_with(stdio_prefix: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = stdio_prefix.iter().map(|s| (*s).into()).collect();
    v.extend([
        // Make `host.docker.internal` resolve on Linux. Docker Desktop
        // already provides this alias and ignores the flag; vault and
        // `--mcp` (and any future host-reachable feature) all rely on
        // it being there unconditionally.
        "--add-host".into(),
        "host.docker.internal:host-gateway".into(),
        "-e".into(),
        format!("HOME={GUEST_HOME}"),
        "-e".into(),
        "TERM=xterm-256color".into(),
        "-e".into(),
        format!("PATH=/usr/local/bin:/usr/bin:/bin:{GUEST_HOME}/.local/bin"),
    ]);
    v
}

pub(crate) fn base_docker_args() -> Vec<String> {
    base_docker_args_with(&["-it", "--rm"])
}

/// Detached base args for the attach-transport flow: `-d`, no `--rm`, so the
/// pty-host container outlives the client and can be `docker exec`'d into
/// and explicitly removed.
pub(crate) fn base_docker_args_detached() -> Vec<String> {
    base_docker_args_with(&["-d"])
}

pub(crate) fn workspace_mount_name(host: &Path, override_name: Option<&str>) -> Result<String> {
    if let Some(name) = override_name {
        if name.is_empty() || name.contains('/') || name.contains('\0') {
            return Err(PillboxError::usage(
                "run",
                format!(
                    "--name `{name}` must be a non-empty single path component (no `/` or NUL)"
                ),
            )
            .into());
        }
        return Ok(name.to_string());
    }
    let derived = host
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("workspace");
    Ok(derived.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pillbox;
    use crate::secrets::{AddSource, WriteScope};
    use crate::test_util::with_isolated_home;
    use crate::vault::{HeaderScheme, OAuthAgent, VaultMeta, VaultSession};

    fn run_opts_with_withs(withs: Vec<&str>) -> RunOpts {
        RunOpts {
            workspace: None,
            name: None,
            mounts: Vec::new(),
            withs: withs.into_iter().map(String::from).collect(),
            env_bundles: Vec::new(),
            env_files: Vec::new(),
            vault: false,
            memory: false,
            memory_briefed: Vec::new(),
            mcps: Vec::new(),
            mcp_tokens: Vec::new(),
            args: Vec::new(),
            detach: false,
            label: None,
            json: false,
            ttl_seconds: None,
            from_bookmark: None,
            model: None,
            egress_allow: Vec::new(),
            egress_deny: false,
        }
    }

    #[test]
    fn resolve_run_env_injects_plain_secret_without_vault_session() {
        with_isolated_home("agents-plain", || {
            let g = pillbox::global();
            std::env::set_var("__PILLBOX_TEST_PLAIN", "raw-value");
            crate::secrets::add(
                &g,
                WriteScope::Resolved,
                "PLAIN_KEY",
                AddSource::EnvVar("__PILLBOX_TEST_PLAIN".into()),
                false,
                None,
            )
            .unwrap();
            std::env::remove_var("__PILLBOX_TEST_PLAIN");

            let opts = run_opts_with_withs(vec!["PLAIN_KEY"]);
            let withs = resolve_with_entries(&g, &opts.withs).unwrap();
            let env = resolve_run_env(&g, &opts, &withs, None).unwrap();
            assert_eq!(env.get("PLAIN_KEY").map(String::as_str), Some("raw-value"));
        });
    }

    #[test]
    fn resolve_run_env_swaps_vaulted_secret_for_stub() {
        with_isolated_home("agents-vault", || {
            let g = pillbox::global();
            std::env::set_var("__PILLBOX_TEST_VAULTED", "sk-ant-api03-REAL");
            crate::secrets::add(
                &g,
                WriteScope::Resolved,
                "VAULTED_KEY",
                AddSource::EnvVar("__PILLBOX_TEST_VAULTED".into()),
                false,
                Some(VaultMeta::new(
                    "api.anthropic.com".into(),
                    HeaderScheme::XApiKey,
                    "sk-ant-api03-".into(),
                )),
            )
            .unwrap();
            std::env::remove_var("__PILLBOX_TEST_VAULTED");

            let mut session = VaultSession::start(
                None::<OAuthAgent>,
                &g,
                crate::vault::RunContext::default(),
                crate::vault::EgressPolicy::default(),
            )
            .unwrap();
            let opts = run_opts_with_withs(vec!["VAULTED_KEY"]);
            let withs = resolve_with_entries(&g, &opts.withs).unwrap();
            let env = resolve_run_env(&g, &opts, &withs, Some(&mut session)).unwrap();

            let injected = env.get("VAULTED_KEY").cloned().unwrap();
            assert!(injected.starts_with("sk-ant-api03-"), "got {injected}");
            assert_ne!(injected, "sk-ant-api03-REAL");
        });
    }

    #[test]
    fn resolve_run_env_mixed_vaulted_and_plain() {
        with_isolated_home("agents-mix", || {
            let g = pillbox::global();
            std::env::set_var("__PILLBOX_TEST_MIX_P", "plain-val");
            crate::secrets::add(
                &g,
                WriteScope::Resolved,
                "MIX_PLAIN",
                AddSource::EnvVar("__PILLBOX_TEST_MIX_P".into()),
                false,
                None,
            )
            .unwrap();
            std::env::remove_var("__PILLBOX_TEST_MIX_P");
            std::env::set_var("__PILLBOX_TEST_MIX_V", "ghp_REAL_token");
            crate::secrets::add(
                &g,
                WriteScope::Resolved,
                "MIX_VAULTED",
                AddSource::EnvVar("__PILLBOX_TEST_MIX_V".into()),
                false,
                Some(VaultMeta::new(
                    "api.github.com".into(),
                    HeaderScheme::AuthorizationBearer,
                    "ghp_".into(),
                )),
            )
            .unwrap();
            std::env::remove_var("__PILLBOX_TEST_MIX_V");

            let mut session = VaultSession::start(
                None::<OAuthAgent>,
                &g,
                crate::vault::RunContext::default(),
                crate::vault::EgressPolicy::default(),
            )
            .unwrap();
            let opts = run_opts_with_withs(vec!["MIX_PLAIN", "MIX_VAULTED=GITHUB_TOKEN"]);
            let withs = resolve_with_entries(&g, &opts.withs).unwrap();
            let env = resolve_run_env(&g, &opts, &withs, Some(&mut session)).unwrap();

            assert_eq!(env.get("MIX_PLAIN").map(String::as_str), Some("plain-val"));
            let stub = env.get("GITHUB_TOKEN").cloned().unwrap();
            assert!(stub.starts_with("ghp_"));
            assert_ne!(stub, "ghp_REAL_token");
        });
    }

    #[test]
    fn resolve_run_env_vaulted_without_session_errors_loudly() {
        with_isolated_home("agents-novault", || {
            let g = pillbox::global();
            std::env::set_var("__PILLBOX_TEST_NOV", "REAL");
            crate::secrets::add(
                &g,
                WriteScope::Resolved,
                "NO_SESSION_KEY",
                AddSource::EnvVar("__PILLBOX_TEST_NOV".into()),
                false,
                Some(VaultMeta::new(
                    "api.openai.com".into(),
                    HeaderScheme::AuthorizationBearer,
                    "sk-".into(),
                )),
            )
            .unwrap();
            std::env::remove_var("__PILLBOX_TEST_NOV");

            let opts = run_opts_with_withs(vec!["NO_SESSION_KEY"]);
            let withs = resolve_with_entries(&g, &opts.withs).unwrap();
            let err = resolve_run_env(&g, &opts, &withs, None).unwrap_err();
            let s = format!("{err}");
            assert!(s.contains("vaulted"), "expected vaulted error, got: {s}");
        });
    }

    #[test]
    fn resolve_with_entries_detects_meta_sidecar() {
        with_isolated_home("agents-detect", || {
            let g = pillbox::global();
            std::env::set_var("__PILLBOX_TEST_DETECT_A", "v");
            crate::secrets::add(
                &g,
                WriteScope::Resolved,
                "DETECT_PLAIN",
                AddSource::EnvVar("__PILLBOX_TEST_DETECT_A".into()),
                false,
                None,
            )
            .unwrap();
            std::env::set_var("__PILLBOX_TEST_DETECT_B", "v");
            crate::secrets::add(
                &g,
                WriteScope::Resolved,
                "DETECT_VAULTED",
                AddSource::EnvVar("__PILLBOX_TEST_DETECT_B".into()),
                false,
                Some(VaultMeta::new(
                    "api.anthropic.com".into(),
                    HeaderScheme::XApiKey,
                    "sk-ant-api03-".into(),
                )),
            )
            .unwrap();
            std::env::remove_var("__PILLBOX_TEST_DETECT_A");
            std::env::remove_var("__PILLBOX_TEST_DETECT_B");

            let plain = resolve_with_entries(&g, &["DETECT_PLAIN".into()]).unwrap();
            assert!(plain[0].meta.is_none());

            let vaulted = resolve_with_entries(&g, &["DETECT_VAULTED".into()]).unwrap();
            assert!(vaulted[0].meta.is_some());
        });
    }

    #[test]
    fn home_dir_is_under_global_auth() {
        with_isolated_home("agents-global-auth", || {
            // Construct a project pillbox; the auth scope should still
            // resolve to global per the v0.6 rule.
            let tmp = tempfile::tempdir().unwrap();
            let saved = std::env::current_dir().ok();
            std::env::set_current_dir(tmp.path()).unwrap();
            pillbox::new(Some("p".into()), None, pillbox::NewWorkspaceArgs::default()).unwrap();
            let proj = crate::pillbox::Pillbox::resolve(None).unwrap();
            assert!(!proj.is_global());

            let home = CLAUDE.home_dir(&proj).unwrap();
            // home is <home>/.pillbox/global/auth/claude
            let s = home.display().to_string();
            assert!(s.contains(".pillbox/global/auth/claude"), "got: {s}");

            if let Some(c) = saved {
                let _ = std::env::set_current_dir(c);
            }
        });
    }

    #[test]
    fn pretrust_seeds_workspace_and_preserves_existing_config() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        // Pre-existing config: an unrelated global key + a sibling project.
        // Pre-trust must merge, not clobber.
        std::fs::write(
            home.join(".claude.json"),
            r#"{"someGlobal":1,"projects":{"/workspace/other":{"hasTrustDialogAccepted":true,"foo":"bar"}}}"#,
        )
        .unwrap();

        pretrust_claude_workspace(home, "/workspace/app").unwrap();

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(home.join(".claude.json")).unwrap())
                .unwrap();
        // The mounted workspace is now trusted + onboarded.
        assert_eq!(
            v["projects"]["/workspace/app"]["hasTrustDialogAccepted"],
            true
        );
        assert_eq!(
            v["projects"]["/workspace/app"]["hasCompletedProjectOnboarding"],
            true
        );
        // Global onboarding is marked complete (suppresses the fresh-HOME
        // first-run wizard on a fresh guest home).
        assert_eq!(v["hasCompletedOnboarding"], true);
        // Pre-existing global key + sibling project survive untouched.
        assert_eq!(v["someGlobal"], 1);
        assert_eq!(v["projects"]["/workspace/other"]["foo"], "bar");
    }

    #[test]
    fn pretrust_creates_claude_json_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        pretrust_claude_workspace(dir.path(), "/workspace/app").unwrap();
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join(".claude.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            v["projects"]["/workspace/app"]["hasTrustDialogAccepted"],
            true
        );
        assert_eq!(v["hasCompletedOnboarding"], true);
    }
}
