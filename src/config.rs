//! `pillbox.toml` — the v0.6 pillbox descriptor.
//!
//! A `pillbox.toml` at the project root marks a directory as a pillbox.
//! In PR 3 the descriptor grows a `[workspace]` table that selects a
//! rustic-backed snapshot store; vault config will follow later.
//!
//! ```toml
//! # required (project descriptor; optional in the global defaults file)
//! name = "my-project"
//!
//! # optional run-config defaults for `pillbox run`
//! agent = "claude"          # or "codex" / "opencode"
//! model = "zai-coding-plan/glm-4.5-air"   # provider/model; None → agent's own default
//!
//! [runner]
//! image = "pillbox-runner:dev"   # the sandbox image (else the published default)
//!
//! [workspace]
//! backend = "local"        # or "s3"
//! # s3-only:
//! # endpoint = "https://<acct>.r2.cloudflarestorage.com"
//! # region = "auto"
//! # bucket = "my-bucket"
//! # prefix = "pillbox/"
//! # access_key_env = "R2_ACCESS_KEY"
//! # secret_key_env = "R2_SECRET_KEY"
//! ```
//!
//! **Discovery + cascade** (CLAUDE.md-style): walk up from cwd until a
//! `pillbox.toml` is found (first match = the project descriptor), then
//! overlay it field-by-field on the user-global defaults at
//! `~/.pillbox/global/pillbox.toml`. Per field, precedence is
//! `CLI flag > env > project pillbox.toml > ~/.pillbox/global/pillbox.toml >
//! built-in default`. Set `agent`/`model`/`[runner] image` once globally;
//! a project descriptor overrides only what's repo-specific. See
//! [`resolve_run_config`].

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::errors::PillboxError;

/// `[workspace]` table — selects the rustic backend variant for this
/// pillbox. PR 3 introduces two variants: `local` (default) and `s3`.
///
/// S3 credentials are referenced **indirectly** via env-var names so
/// the descriptor stays safe to check into git. The values land in the
/// process env at pillbox-creation time + at every push/pull.
///
/// We deliberately **don't** set `deny_unknown_fields` here: this
/// struct is embedded in `meta.json` (see [`crate::pillbox::ProjectMeta`])
/// which has a documented forward-compat contract — older binaries
/// must keep parsing meta.json after v0.7+ adds fields. The top-level
/// `Config` is stricter because it backs a user-edited `pillbox.toml`
/// where catching typos matters more than reading future descriptors.
#[derive(Debug, Default, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceConfig {
    /// `local` (default) or `s3`. Missing → `local` so existing
    /// descriptors written before PR 3 keep working.
    #[serde(default)]
    pub(crate) backend: Option<String>,
    #[serde(default)]
    pub(crate) endpoint: Option<String>,
    #[serde(default)]
    pub(crate) region: Option<String>,
    #[serde(default)]
    pub(crate) bucket: Option<String>,
    #[serde(default)]
    pub(crate) prefix: Option<String>,
    #[serde(default)]
    pub(crate) access_key_env: Option<String>,
    #[serde(default)]
    pub(crate) secret_key_env: Option<String>,
}

/// Normalized backend selector. Centralizes the `local` / `s3` /
/// unknown decision so callers `match` on a closed set rather than on
/// stringly-typed values. New variants land here first; serialization
/// (`as_str`) stays stable so `pillbox.toml` text doesn't drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackendKind {
    Local,
    S3,
}

impl BackendKind {
    /// Wire-format name written into `pillbox.toml` + read back by older
    /// binaries. Keep in sync with the documented schema in
    /// `docs/config.md`.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            BackendKind::Local => "local",
            BackendKind::S3 => "s3",
        }
    }
}

impl WorkspaceConfig {
    /// Normalize the backend selector. Returns [`BackendKind::Local`]
    /// for missing / empty / unknown values so callers always see one
    /// of the two allowed variants. Validation happens at `pillbox new`
    /// time, not at every load — older binaries reading a future
    /// descriptor should degrade gracefully.
    pub(crate) fn backend_kind(&self) -> BackendKind {
        match self.backend.as_deref() {
            Some("s3") => BackendKind::S3,
            _ => BackendKind::Local,
        }
    }
}

/// `[runner]` table — selects the docker image pillbox launches
/// sandboxes from. Pinned per-pillbox so a project that needs a
/// newer harness can bump independently of `pillbox` releases.
///
/// Deliberately **not** mirrored into `meta.json` the way
/// [`WorkspaceConfig`] is. The runner image is an operational
/// choice that should take effect the moment the user edits
/// `pillbox.toml` — no `pillbox upgrade-meta` step, no "edited
/// toml but old meta wins" footgun. We pay one TOML re-parse per
/// `pillbox run`, which is invisible against the docker spawn
/// that follows. `Deserialize`-only for the same reason: the
/// value flows in from the descriptor and never back out.
#[derive(Debug, Default, Deserialize, Clone, PartialEq, Eq)]
pub(crate) struct RunnerConfig {
    /// Docker image reference (e.g. `ghcr.io/vu1n/pillbox-runner:vX.Y.Z`
    /// or a locally-built `pillbox:latest`). `None` falls back to the
    /// CLI default — see [`crate::docker::default_runner_image`].
    /// Overridden by the `PILLBOX_RUNNER_IMAGE` env var.
    #[serde(default)]
    pub(crate) image: Option<String>,
}

#[derive(Debug, Default, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // `agent`, `workspace`, `source` consumed by PR 3 + later
pub(crate) struct Config {
    /// Display name for the pillbox. Required in well-formed configs but
    /// optional here so a half-written file still parses (the loader
    /// enforces presence — see `Config::load_from`).
    #[serde(default)]
    pub(crate) name: Option<String>,
    /// Default agent for `pillbox run` (`claude` | `codex` | `opencode`). `None` falls
    /// back to a built-in default at run time.
    #[serde(default)]
    pub(crate) agent: Option<String>,
    /// Default model for `pillbox run` (provider/model, e.g. `zai-coding-plan/glm-4.5-air`).
    /// `None` → the agent's own default. Overridden by `--model`.
    #[serde(default)]
    pub(crate) model: Option<String>,
    /// Workspace backend selector (`local` | `s3`). See [`WorkspaceConfig`].
    #[serde(default)]
    pub(crate) workspace: WorkspaceConfig,
    /// Per-pillbox runner image override. See [`RunnerConfig`].
    #[serde(default)]
    pub(crate) runner: RunnerConfig,

    /// Path the config was loaded from. Useful for `info` output.
    #[serde(skip)]
    pub(crate) source: Option<PathBuf>,
}

#[allow(dead_code)] // `load_from` is exercised by tests + PR 3
impl Config {
    pub(crate) fn load_from(path: &Path) -> Result<Config> {
        let raw = fs::read_to_string(path).map_err(|e| {
            PillboxError::runtime(
                "config load",
                format!("could not read {}: {e}", path.display()),
            )
        })?;
        let mut cfg: Config = toml::from_str(&raw)
            .map_err(|e| PillboxError::config("config load", format!("{}: {e}", path.display())))?;
        if cfg.name.as_deref().map(str::trim).unwrap_or("").is_empty() {
            return Err(PillboxError::config(
                "config load",
                format!("{}: missing required field `name`", path.display()),
            )
            .into());
        }
        cfg.source = Some(path.to_path_buf());
        Ok(cfg)
    }

    /// Load the user-global defaults (`~/.pillbox/global/pillbox.toml`) — the BASE
    /// layer of the cascade. Lenient: it's a defaults file, not a project descriptor,
    /// so `name` isn't required and any absent/unreadable/malformed file yields empty
    /// defaults (`Config::default`) rather than an error — a missing global file is
    /// the normal case, not a failure.
    pub(crate) fn global_defaults() -> Config {
        fs::read_to_string(crate::pillbox::global_config_path())
            .ok()
            .and_then(|raw| toml::from_str::<Config>(&raw).ok())
            .unwrap_or_default()
    }

    /// Overlay `self` (higher precedence — e.g. a project descriptor) onto `base`
    /// (lower — e.g. global defaults), field-by-field: a field set in `self` wins;
    /// an unset field falls through to `base`. This is the pillbox.toml cascade
    /// (CLAUDE.md-style), scoped to the run-config fields that callers resolve.
    pub(crate) fn overlay_on(self, base: Config) -> Config {
        Config {
            name: self.name.or(base.name),
            agent: self.agent.or(base.agent),
            model: self.model.or(base.model),
            runner: RunnerConfig {
                image: self.runner.image.or(base.runner.image),
            },
            // Workspace (store backend) is project-scoped — take the project's if it
            // declares one, else inherit global. Per-field merge isn't needed: a
            // descriptor either owns its store config or has none.
            workspace: if self.workspace == WorkspaceConfig::default() {
                base.workspace
            } else {
                self.workspace
            },
            source: self.source.or(base.source),
        }
    }
}

/// Resolve the effective run-config for a pillbox via the descriptor cascade:
/// the project `pillbox.toml` (found by walking up from cwd, read FRESH so edits
/// take effect immediately) overlaid on `~/.pillbox/global/pillbox.toml`. Callers
/// apply the higher-precedence layers (CLI flag, env) and the built-in default on
/// top of the field they read (`agent` / `model` / `runner.image`).
pub(crate) fn resolve_run_config(resolved: &crate::pillbox::Pillbox) -> Config {
    let global = Config::global_defaults();
    match &resolved.scope {
        crate::pillbox::Scope::Project { source_dir, .. } => {
            match Config::load_from(&source_dir.join("pillbox.toml")) {
                Ok(project) => project.overlay_on(global),
                Err(_) => global,
            }
        }
        _ => global,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write_config(dir: &Path, body: &str) {
        let mut f = fs::File::create(dir.join("pillbox.toml")).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    #[test]
    fn load_accepts_minimal_config() {
        let root = TempDir::new().unwrap();
        write_config(root.path(), "name = \"alpha\"\n");
        let cfg = Config::load_from(&root.path().join("pillbox.toml")).unwrap();
        assert_eq!(cfg.name.as_deref(), Some("alpha"));
        assert_eq!(cfg.agent, None);
    }

    #[test]
    fn load_parses_agent_field() {
        let root = TempDir::new().unwrap();
        write_config(root.path(), "name = \"a\"\nagent = \"claude\"\n");
        let cfg = Config::load_from(&root.path().join("pillbox.toml")).unwrap();
        assert_eq!(cfg.agent.as_deref(), Some("claude"));
    }

    #[test]
    fn load_parses_model_field() {
        let root = TempDir::new().unwrap();
        write_config(root.path(), "name = \"a\"\nmodel = \"prov/m\"\n");
        let cfg = Config::load_from(&root.path().join("pillbox.toml")).unwrap();
        assert_eq!(cfg.model.as_deref(), Some("prov/m"));
    }

    #[test]
    fn overlay_project_wins_global_fills_gaps() {
        // global = the user-wide defaults (~/.pillbox/global/pillbox.toml)
        let global = Config {
            agent: Some("claude".into()),
            model: Some("global/model".into()),
            runner: RunnerConfig {
                image: Some("global-img".into()),
            },
            ..Default::default()
        };
        // project overrides agent, leaves model + image unset → inherit global
        let project = Config {
            name: Some("proj".into()),
            agent: Some("codex".into()),
            ..Default::default()
        };
        let m = project.overlay_on(global);
        assert_eq!(m.agent.as_deref(), Some("codex")); // project wins
        assert_eq!(m.model.as_deref(), Some("global/model")); // inherited
        assert_eq!(m.runner.image.as_deref(), Some("global-img")); // inherited
        assert_eq!(m.name.as_deref(), Some("proj"));
    }

    #[test]
    fn load_rejects_missing_name() {
        let root = TempDir::new().unwrap();
        write_config(root.path(), "agent = \"claude\"\n");
        let err = Config::load_from(&root.path().join("pillbox.toml")).unwrap_err();
        let s = format!("{err}");
        assert!(s.contains("name"));
    }

    #[test]
    fn load_rejects_unknown_field() {
        let root = TempDir::new().unwrap();
        write_config(root.path(), "name = \"x\"\nbogus = 1\n");
        let err = Config::load_from(&root.path().join("pillbox.toml")).unwrap_err();
        let s = format!("{err}");
        assert!(s.contains("bogus") || s.contains("unknown"));
    }

    #[test]
    fn load_rejects_legacy_with_field() {
        // v0.5 had `with = [...]`; v0.6 drops it. Old configs fail loud.
        let root = TempDir::new().unwrap();
        write_config(
            root.path(),
            "name = \"x\"\nwith = [\"ANTHROPIC_API_KEY\"]\n",
        );
        let err = Config::load_from(&root.path().join("pillbox.toml")).unwrap_err();
        let s = format!("{err}");
        assert!(s.contains("with") || s.contains("unknown"));
    }

    #[test]
    fn load_accepts_empty_workspace_table() {
        let root = TempDir::new().unwrap();
        write_config(root.path(), "name = \"x\"\n\n[workspace]\n");
        let cfg = Config::load_from(&root.path().join("pillbox.toml")).unwrap();
        // Missing backend → defaults to local.
        assert_eq!(cfg.workspace.backend_kind(), BackendKind::Local);
    }

    #[test]
    fn load_parses_workspace_local_backend() {
        let root = TempDir::new().unwrap();
        write_config(
            root.path(),
            "name = \"x\"\n\n[workspace]\nbackend = \"local\"\n",
        );
        let cfg = Config::load_from(&root.path().join("pillbox.toml")).unwrap();
        assert_eq!(cfg.workspace.backend_kind(), BackendKind::Local);
    }

    #[test]
    fn load_parses_workspace_s3_backend() {
        let root = TempDir::new().unwrap();
        write_config(
            root.path(),
            r#"name = "x"

[workspace]
backend = "s3"
endpoint = "https://acct.r2.cloudflarestorage.com"
region = "auto"
bucket = "my-bucket"
prefix = "pillbox/"
access_key_env = "R2_ACCESS_KEY"
secret_key_env = "R2_SECRET_KEY"
"#,
        );
        let cfg = Config::load_from(&root.path().join("pillbox.toml")).unwrap();
        assert_eq!(cfg.workspace.backend_kind(), BackendKind::S3);
        assert_eq!(
            cfg.workspace.endpoint.as_deref(),
            Some("https://acct.r2.cloudflarestorage.com")
        );
        assert_eq!(cfg.workspace.bucket.as_deref(), Some("my-bucket"));
        assert_eq!(cfg.workspace.prefix.as_deref(), Some("pillbox/"));
        assert_eq!(
            cfg.workspace.access_key_env.as_deref(),
            Some("R2_ACCESS_KEY")
        );
    }

    #[test]
    fn workspace_backend_kind_normalizes_unknown_to_local() {
        let w = WorkspaceConfig {
            backend: Some("bogus".into()),
            ..Default::default()
        };
        assert_eq!(w.backend_kind(), BackendKind::Local);
        let w = WorkspaceConfig {
            backend: Some("s3".into()),
            ..Default::default()
        };
        assert_eq!(w.backend_kind(), BackendKind::S3);
        let w = WorkspaceConfig::default();
        assert_eq!(w.backend_kind(), BackendKind::Local);
    }
}
