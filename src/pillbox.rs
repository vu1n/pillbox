//! Pillbox lifecycle — the v0.6 "pillbox-as-bundle" identity.
//!
//! A pillbox is a self-contained bundle of (workspace + code + vault +
//! config) that an agent runs against. There are two flavors:
//!
//! - **Global** — single bundle at `~/.pillbox/global/`. Acts as the
//!   fallback for secret / env lookups; holds the agent auth shared across
//!   all projects.
//! - **Project** — one per directory that contains a `pillbox.toml`. State
//!   lives at `~/.pillbox/projects/<dash-encoded-path>/`, where the key
//!   is the absolute path of the directory holding `pillbox.toml` with
//!   `/` replaced by `-`. Human-readable, unique on a single machine.
//!
//! ## Discovery
//!
//! `Pillbox::current()` walks up from cwd looking for `pillbox.toml`.
//! First match wins → project pillbox. Nothing found → global. The
//! `--pillbox NAME` flag overrides discovery by name (`meta.json.name`)
//! or path-encoded key.
//!
//! ## Inheritance
//!
//! Secrets and env bundles inherit: reads merge global + project (project
//! overrides on key conflict); writes go to project unless `--global`.
//! Auth defaults to global (one `claude login` shared across projects);
//! per-project auth is deferred to v0.7. Vault state and workspace
//! configuration are per-pillbox only.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{BackendKind, WorkspaceConfig};
use crate::errors::PillboxError;
use crate::paths::{detect_legacy_subdirs, ensure_mode_0700, pillbox_root, write_private_file};
use crate::workspace::rustic::{RusticBackend, RusticVariant, PASSWORD_FILE, REPO_DIR};
use crate::workspace::WorkspaceBackend;

/// The descriptor file that marks a directory as a project pillbox.
pub(crate) const PILLBOX_TOML: &str = "pillbox.toml";

/// Display name reserved for the global pillbox. Matches the directory
/// segment, which makes `--pillbox global` work as expected.
pub(crate) const GLOBAL_NAME: &str = "global";

/// Where a write goes when both `--pillbox` and `--global` may be in play.
/// Lives here (not in `secrets` or `envs`) because both modules share the
/// rule and the inheritance model is a property of the pillbox scope, not
/// of a particular kind of stored data.
#[derive(Debug, Clone, Copy)]
pub(crate) enum WriteScope {
    /// Default: write to the resolved pillbox (project if discovered,
    /// otherwise global).
    Resolved,
    /// `--global` was passed: write to the global pillbox regardless of
    /// the resolved scope.
    Global,
}

impl WriteScope {
    /// Convert a CLI `--global` boolean into the matching scope. Every
    /// `dispatch_*` arm that maps a `--global` flag uses this so the
    /// "true → Global, false → Resolved" rule lives in one place.
    pub(crate) fn from_global_flag(global: bool) -> Self {
        if global {
            Self::Global
        } else {
            Self::Resolved
        }
    }
}

/// On-disk per-project state record. Forward-compatible: unknown fields
/// (added by a newer pillbox version writing the same `meta.json`) are
/// ignored on load rather than rejected, so an older binary can still
/// inspect the bundle without crashing on PR 3+ additions like workspace
/// backend config. `name`/`created_at` are the only fields older binaries
/// rely on; future additions must keep them present.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ProjectMeta {
    /// Display name (from `pillbox.toml`). The path-encoded state-dir key
    /// is the durable identifier; `name` is what the user sees.
    pub(crate) name: String,
    /// RFC3339 timestamp set on `pillbox new`. Informational.
    pub(crate) created_at: String,
    /// Default agent for `pillbox run`. Mirrors `pillbox.toml`'s `agent`.
    /// `None` falls back to the built-in default at run time.
    #[serde(default)]
    pub(crate) agent_default: Option<String>,
    /// Workspace backend selector. Mirrors `pillbox.toml`'s `[workspace]`
    /// table. Persisted in `meta.json` so runtime callers (`pillbox push`
    /// / `pillbox pull`) don't have to re-read + re-validate the
    /// descriptor on every command — the state dir already has the
    /// resolved values.
    #[serde(default)]
    pub(crate) workspace: WorkspaceConfig,
}

/// Either the global pillbox or one specific project. The scope drives
/// every secrets/env/auth/vault path lookup; a `Pillbox` value is the
/// outcome of discovery, not a flag.
#[derive(Debug, Clone)]
pub(crate) enum Scope {
    Global,
    Project {
        /// Path-encoded key (`-Users-vuln-code-aeon-v2`).
        key: String,
        /// Absolute path the key was derived from (the directory holding
        /// `pillbox.toml`). Stored so callers can show the user where
        /// the pillbox lives.
        source_dir: PathBuf,
    },
}

/// One resolved pillbox — the result of discovery. Holds the scope plus
/// the on-disk state directory (`~/.pillbox/global/` or
/// `~/.pillbox/projects/<key>/`). Every command that operates on a
/// pillbox takes one of these.
#[derive(Debug, Clone)]
pub(crate) struct Pillbox {
    pub(crate) scope: Scope,
    /// State directory on disk. Created lazily by callers via
    /// `paths::ensure_mode_0700`; not guaranteed to exist when the
    /// `Pillbox` is constructed.
    pub(crate) state_dir: PathBuf,
    /// `meta.json` body for project pillboxes. `None` for global.
    pub(crate) meta: Option<ProjectMeta>,
}

impl Pillbox {
    /// Human-readable name. Project pillboxes use `meta.json.name`; the
    /// global pillbox is always `"global"`.
    pub(crate) fn display_name(&self) -> &str {
        match (&self.scope, self.meta.as_ref()) {
            (Scope::Global, _) => GLOBAL_NAME,
            (Scope::Project { .. }, Some(m)) => &m.name,
            // Project with missing meta — show the key so the user can
            // still identify it. Shouldn't normally happen.
            (Scope::Project { key, .. }, None) => key,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn is_global(&self) -> bool {
        matches!(self.scope, Scope::Global)
    }

    /// Resolve the configured workspace backend. Reads from
    /// `meta.json.workspace` (populated by `pillbox new`) so we don't
    /// re-parse `pillbox.toml` every time. Returns `None` for the
    /// global pillbox (which doesn't own a workspace).
    ///
    /// The S3 variant pulls credentials from the env vars named in
    /// `meta.json.workspace.{access_key_env,secret_key_env}`. Missing
    /// envs surface as a runtime error rather than panic.
    pub(crate) fn workspace(&self) -> Result<RusticBackend> {
        let meta = self.meta.as_ref().ok_or_else(|| {
            PillboxError::usage(
                "workspace",
                "the global pillbox has no workspace; cd into a project pillbox",
            )
        })?;
        let password_file = self.state_dir.join(PASSWORD_FILE);
        let variant = match meta.workspace.backend_kind() {
            BackendKind::Local => RusticVariant::Local {
                repo_path: self.state_dir.join(REPO_DIR),
            },
            BackendKind::S3 => {
                let w = &meta.workspace;
                let endpoint = require_field(w.endpoint.as_deref(), "endpoint")?;
                let region = w.region.clone().unwrap_or_else(|| "auto".to_string());
                let bucket = require_field(w.bucket.as_deref(), "bucket")?;
                let prefix = w.prefix.clone().unwrap_or_default();
                let access_key_env = require_field(w.access_key_env.as_deref(), "access_key_env")?;
                let secret_key_env = require_field(w.secret_key_env.as_deref(), "secret_key_env")?;
                let access_key = read_env_or_err(&access_key_env)?;
                let secret_key = read_env_or_err(&secret_key_env)?;
                RusticVariant::S3 {
                    endpoint,
                    region,
                    bucket,
                    prefix,
                    access_key,
                    secret_key,
                }
            }
        };
        Ok(RusticBackend {
            variant,
            password_file,
        })
    }

    /// Per-pillbox subdirectory under the state dir, idempotently 0700.
    /// Use this for `secrets/`, `env/`, `auth/`, `vault/` on **write**
    /// paths — every call costs a `create_dir_all` (stat) + `chmod`.
    /// For reads, prefer [`Self::subdir_path`].
    pub(crate) fn subdir(&self, name: &str) -> Result<PathBuf> {
        let dir = self.state_dir.join(name);
        fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        ensure_mode_0700(&dir)?;
        Ok(dir)
    }

    /// Read-only variant of [`Self::subdir`]: just joins the path, no
    /// `create_dir_all` and no `chmod`. Hot-path reads (`secret read`,
    /// `env read` during `pillbox run`) call this so they don't pay
    /// dir-creation syscalls per lookup. Callers must handle `NotFound`
    /// gracefully — the dir may legitimately not exist yet.
    pub(crate) fn subdir_path(&self, name: &str) -> PathBuf {
        self.state_dir.join(name)
    }

    /// Walk order for inherited reads. Project pillboxes return
    /// `[self, global]`; the global pillbox returns `[self]`. Callers iterate
    /// in order so the project value shadows the global value on conflict.
    ///
    /// Centralized here because every per-scope module (`secrets`, `env`,
    /// later `auth` per-project) needs the same chain — keeping the rule in
    /// one place keeps "what overrides what" consistent.
    pub(crate) fn read_chain(&self) -> Vec<Pillbox> {
        match &self.scope {
            Scope::Global => vec![self.clone()],
            Scope::Project { .. } => vec![self.clone(), global()],
        }
    }

    /// Pick the write target given a [`WriteScope`]. `Resolved` writes go to
    /// `self`; `Global` always forces the global pillbox regardless of the
    /// resolved scope. Mirrors the read-side rule in [`Self::read_chain`].
    pub(crate) fn write_target(&self, scope: WriteScope) -> Pillbox {
        match scope {
            WriteScope::Resolved => self.clone(),
            WriteScope::Global => global(),
        }
    }

    /// Resolve the current pillbox.
    ///
    /// 1. `--pillbox NAME` (explicit) — name lookup against meta.json or path key.
    /// 2. Discover: walk up cwd looking for `pillbox.toml`.
    /// 3. Fall back to global.
    pub(crate) fn resolve(explicit: Option<&str>) -> Result<Self> {
        Self::resolve_with_source(explicit).map(|(pb, _)| pb)
    }

    /// Resolve, also reporting whether the result came from an explicit
    /// `--pillbox` flag or a `pillbox.toml` discovery (vs the global
    /// fallback). `pillbox info` uses the flag to emit a "falling back to
    /// global" hint; other callers consume just the `Pillbox`.
    pub(crate) fn resolve_with_source(explicit: Option<&str>) -> Result<(Self, bool)> {
        check_legacy_layout()?;
        if let Some(name) = explicit {
            return Ok((lookup_by_name(name)?, true));
        }
        let cwd = std::env::current_dir().map_err(|e| {
            PillboxError::runtime("pillbox resolve", format!("could not resolve cwd: {e}"))
        })?;
        if let Some(project) = discover_from(&cwd)? {
            return Ok((project, true));
        }
        Ok((global(), false))
    }
}

/// Encode an absolute path as the state-dir key. `/Users/vuln/code/aeon` →
/// `-Users-vuln-code-aeon`. Guarantees uniqueness for a given absolute
/// path on the host (we never decode, but the encoding is reversible to
/// human inspection). Non-`/` characters pass through unchanged so the
/// dir name is greppable.
pub(crate) fn path_to_key(p: &Path) -> Result<String> {
    if !p.is_absolute() {
        return Err(PillboxError::runtime(
            "pillbox key",
            format!("expected absolute path, got `{}`", p.display()),
        )
        .into());
    }
    // Canonicalize lossily — symlinks resolved before encoding so two
    // different access paths to the same dir collapse to one key. If the
    // path doesn't exist (we're encoding before creating it), fall back
    // to the raw absolute path.
    let canon = fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let s = canon.to_string_lossy().into_owned();
    Ok(s.replace('/', "-"))
}

/// `~/.pillbox/global/`, created lazily, 0700.
fn global_state_dir() -> Result<PathBuf> {
    let dir = pillbox_root()?.join(GLOBAL_NAME);
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    ensure_mode_0700(&dir)?;
    Ok(dir)
}

/// `~/.pillbox/projects/`, created lazily, 0700.
fn projects_root() -> Result<PathBuf> {
    let dir = pillbox_root()?.join("projects");
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    ensure_mode_0700(&dir)?;
    Ok(dir)
}

/// Build the canonical global `Pillbox`. Infallible so callers don't have
/// to thread `?` through code paths that only need a scope token; the
/// state dir is created lazily on first write via `Pillbox::subdir`.
/// Falls back to a relative `.pillbox` if `$HOME` is unset so tests in
/// odd environments don't panic.
pub(crate) fn global() -> Pillbox {
    let root = std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".pillbox"))
        .unwrap_or_else(|_| PathBuf::from(".pillbox"));
    Pillbox {
        scope: Scope::Global,
        state_dir: root.join(GLOBAL_NAME),
        meta: None,
    }
}

/// Walk up from `start` looking for `pillbox.toml`. Returns the resolved
/// project `Pillbox` (with its state dir + meta loaded) or `None`.
pub(crate) fn discover_from(start: &Path) -> Result<Option<Pillbox>> {
    let mut dir = start;
    loop {
        let candidate = dir.join(PILLBOX_TOML);
        if candidate.is_file() {
            return Ok(Some(load_project(dir)?));
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return Ok(None),
        }
    }
}

/// Look up a pillbox by name. Matches in order:
/// 1. `"global"` → the global pillbox.
/// 2. A project `meta.json.name` equal to `name`.
/// 3. A project state-dir key equal to `name` (so the user can paste
///    `-Users-vuln-code-aeon` from `pillbox list`).
fn lookup_by_name(name: &str) -> Result<Pillbox> {
    if name == GLOBAL_NAME {
        return Ok(global());
    }
    let projects = projects_root()?;
    if !projects.exists() {
        return Err(name_not_found(name).into());
    }
    let mut by_meta: Option<Pillbox> = None;
    let mut by_key: Option<Pillbox> = None;
    for entry in fs::read_dir(&projects).with_context(|| format!("read {}", projects.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let key = match entry.file_name().into_string() {
            Ok(s) => s,
            Err(_) => continue,
        };
        let state_dir = entry.path();
        let meta = load_project_meta(&state_dir).ok().flatten();
        let pb = Pillbox {
            scope: Scope::Project {
                key: key.clone(),
                source_dir: decode_key_to_path(&key),
            },
            state_dir,
            meta: meta.clone(),
        };
        if key == name {
            by_key = Some(pb.clone());
        }
        if let Some(m) = meta {
            if m.name == name {
                by_meta = Some(pb);
            }
        }
    }
    by_meta
        .or(by_key)
        .ok_or_else(|| name_not_found(name).into())
}

fn name_not_found(name: &str) -> PillboxError {
    PillboxError::runtime("pillbox lookup", format!("no pillbox named `{name}`"))
        .with_next("pillbox list  # see what's available")
}

/// Best-effort key → path inverse (replace `-` with `/`). Used only for
/// display; never for filesystem ops, so a lossy decode is fine.
fn decode_key_to_path(key: &str) -> PathBuf {
    PathBuf::from(key.replace('-', "/"))
}

fn load_project(source_dir: &Path) -> Result<Pillbox> {
    let key = path_to_key(source_dir)?;
    let state_dir = projects_root()?.join(&key);
    fs::create_dir_all(&state_dir).with_context(|| format!("create {}", state_dir.display()))?;
    ensure_mode_0700(&state_dir)?;
    let meta = load_project_meta(&state_dir)?;
    Ok(Pillbox {
        scope: Scope::Project {
            key,
            source_dir: source_dir.to_path_buf(),
        },
        state_dir,
        meta,
    })
}

fn load_project_meta(state_dir: &Path) -> Result<Option<ProjectMeta>> {
    let path = state_dir.join("meta.json");
    match fs::read_to_string(&path) {
        Ok(s) => {
            let m: ProjectMeta = serde_json::from_str(&s).map_err(|e| {
                PillboxError::config("pillbox meta", format!("parse {}: {e}", path.display()))
            })?;
            Ok(Some(m))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

fn write_project_meta(state_dir: &Path, meta: &ProjectMeta) -> Result<()> {
    let path = state_dir.join("meta.json");
    let body = serde_json::to_string_pretty(meta)
        .with_context(|| format!("serialize meta for `{}`", meta.name))?;
    write_private_file(&path, body.as_bytes())
}

/// `pillbox init` — create the global pillbox. Idempotent: subsequent
/// calls do nothing and exit 0. Detects the legacy v0.5 layout first.
pub(crate) fn init() -> Result<()> {
    check_legacy_layout()?;
    let dir = global_state_dir()?;
    println!("pillbox: ✓ global pillbox initialized at {}", dir.display());
    println!();
    println!("Add the agents you'll use:");
    println!("  pillbox auth login --agent claude   # or codex");
    println!();
    println!("Then create a project pillbox in any directory with `pillbox new`.");
    Ok(())
}

/// Workspace flags supplied to `pillbox new` (PR 3). All optional —
/// the default is a local rustic repo under
/// `~/.pillbox/projects/<key>/repo/`.
#[derive(Debug, Default)]
pub(crate) struct NewWorkspaceArgs {
    pub(crate) backend: Option<String>,
    pub(crate) endpoint: Option<String>,
    pub(crate) region: Option<String>,
    pub(crate) bucket: Option<String>,
    pub(crate) prefix: Option<String>,
    pub(crate) access_key_env: Option<String>,
    pub(crate) secret_key_env: Option<String>,
    pub(crate) from_git: Option<String>,
    pub(crate) git_ref: Option<String>,
}

/// `pillbox new` — create a project pillbox in the current directory.
/// Writes `pillbox.toml` next to the cwd, creates the state dir under
/// `~/.pillbox/projects/<key>/`, initializes a rustic repo, and
/// optionally clones from `--from-git`. Fails if a pillbox already
/// exists at either location.
pub(crate) fn new(name: Option<String>, agent: Option<String>, ws: NewWorkspaceArgs) -> Result<()> {
    check_legacy_layout()?;
    let cwd = std::env::current_dir()
        .map_err(|e| PillboxError::runtime("pillbox new", format!("could not resolve cwd: {e}")))?;
    let descriptor = cwd.join(PILLBOX_TOML);
    if descriptor.exists() {
        return Err(PillboxError::runtime(
            "pillbox new",
            format!("`{}` already exists", descriptor.display()),
        )
        .with_next("pillbox info  # inspect the existing pillbox")
        .into());
    }
    let display_name = match name {
        Some(n) => n,
        None => default_name_from_dir(&cwd),
    };
    if let Some(a) = agent.as_deref() {
        validate_agent(a)?;
    }
    let workspace_cfg = build_workspace_config(&ws)?;
    let key = path_to_key(&cwd)?;
    let state_dir = projects_root()?.join(&key);
    if state_dir.exists() {
        return Err(PillboxError::runtime(
            "pillbox new",
            format!("project state already exists at {}", state_dir.display()),
        )
        .with_next(format!("pillbox info --pillbox {key}"))
        .into());
    }
    fs::create_dir_all(&state_dir).with_context(|| format!("create {}", state_dir.display()))?;
    ensure_mode_0700(&state_dir)?;

    // --from-git: clone before initializing rustic, so the workspace
    // has content. Clone target is cwd itself; refuse if cwd isn't
    // empty (mirrors `git clone`'s own behavior).
    if let Some(url) = ws.from_git.as_deref() {
        clone_git_into_cwd(&cwd, url, ws.git_ref.as_deref())?;
    }

    // Write the descriptor first so a failure on meta.json leaves the
    // user with a half-set-up directory they can re-run against.
    write_descriptor(&descriptor, &display_name, agent.as_deref(), &workspace_cfg)?;
    let meta = ProjectMeta {
        name: display_name.clone(),
        created_at: rfc3339_now(),
        agent_default: agent,
        workspace: workspace_cfg.clone(),
    };
    write_project_meta(&state_dir, &meta)?;

    // Initialize the rustic repo + password file. Built from the
    // resolved pillbox so the variant logic stays in one place.
    let pb = Pillbox {
        scope: Scope::Project {
            key: key.clone(),
            source_dir: cwd.clone(),
        },
        state_dir: state_dir.clone(),
        meta: Some(meta),
    };
    // For S3 the env vars must be set NOW so init can talk to the
    // bucket. We don't actually push a snapshot here, just init the
    // repo skeleton.
    //
    // Test escape hatch: rustic's scrypt key derivation takes ~5s by
    // design (deliberately slow vs. brute force). Set
    // `PILLBOX_SKIP_WORKSPACE_INIT=1` to suppress repo init for tests
    // that exercise non-workspace surfaces (secrets, auth, …). Tests
    // that DO want a real repo (`tests/workspace.rs`, the workspace
    // unit tests) just don't set the var. Never set it in production.
    if std::env::var_os("PILLBOX_SKIP_WORKSPACE_INIT").is_none() {
        let backend = pb.workspace()?;
        backend.init_for_pillbox()?;
    }

    println!("pillbox: ✓ project pillbox `{display_name}` created");
    println!("  descriptor: {}", descriptor.display());
    println!("  state dir:  {}", state_dir.display());
    match workspace_cfg.backend_kind() {
        BackendKind::Local => {
            println!(
                "  workspace:  local rustic repo at {}",
                state_dir.join(REPO_DIR).display()
            );
        }
        BackendKind::S3 => {
            println!(
                "  workspace:  s3://{}/{} ({})",
                workspace_cfg.bucket.as_deref().unwrap_or(""),
                workspace_cfg.prefix.as_deref().unwrap_or(""),
                workspace_cfg.endpoint.as_deref().unwrap_or(""),
            );
        }
    }
    println!(
        "  password:   {} (0600, local only)",
        state_dir.join(PASSWORD_FILE).display()
    );
    println!();
    println!("Run agents with `pillbox run` from inside this directory.");
    println!("Snapshot the workspace with `pillbox push`.");
    Ok(())
}

/// Validate + assemble a [`WorkspaceConfig`] from CLI args. Local
/// backend is the default; S3 demands the full opt set so we fail
/// early with one error rather than per-field.
fn build_workspace_config(ws: &NewWorkspaceArgs) -> Result<WorkspaceConfig> {
    let kind = match ws.backend.as_deref() {
        Some("local") | None => BackendKind::Local,
        Some("s3") => BackendKind::S3,
        Some(other) => {
            return Err(PillboxError::usage(
                "pillbox new",
                format!("--workspace-backend `{other}` (known: local, s3)"),
            )
            .into())
        }
    };
    if kind == BackendKind::Local {
        let stray = [
            ("--bucket", ws.bucket.is_some()),
            ("--endpoint", ws.endpoint.is_some()),
            ("--region", ws.region.is_some()),
            ("--prefix", ws.prefix.is_some()),
            ("--access-key-env", ws.access_key_env.is_some()),
            ("--secret-key-env", ws.secret_key_env.is_some()),
        ];
        if let Some((flag, _)) = stray.iter().find(|(_, set)| *set) {
            return Err(PillboxError::usage(
                "pillbox new",
                format!("{flag} is only valid with --workspace-backend s3"),
            )
            .into());
        }
        return Ok(WorkspaceConfig {
            backend: Some(BackendKind::Local.as_str().into()),
            ..Default::default()
        });
    }

    // s3
    let bucket = ws.bucket.clone().ok_or_else(|| {
        PillboxError::usage(
            "pillbox new",
            "--workspace-backend s3 requires --bucket BUCKET",
        )
    })?;
    let endpoint = ws.endpoint.clone().ok_or_else(|| {
        PillboxError::usage(
            "pillbox new",
            "--workspace-backend s3 requires --endpoint URL",
        )
    })?;
    let access_key_env = ws.access_key_env.clone().ok_or_else(|| {
        PillboxError::usage(
            "pillbox new",
            "--workspace-backend s3 requires --access-key-env VAR",
        )
    })?;
    let secret_key_env = ws.secret_key_env.clone().ok_or_else(|| {
        PillboxError::usage(
            "pillbox new",
            "--workspace-backend s3 requires --secret-key-env VAR",
        )
    })?;
    Ok(WorkspaceConfig {
        backend: Some(BackendKind::S3.as_str().into()),
        endpoint: Some(endpoint),
        region: Some(ws.region.clone().unwrap_or_else(|| "auto".to_string())),
        bucket: Some(bucket),
        prefix: ws.prefix.clone(),
        access_key_env: Some(access_key_env),
        secret_key_env: Some(secret_key_env),
    })
}

fn clone_git_into_cwd(cwd: &Path, url: &str, git_ref: Option<&str>) -> Result<()> {
    // Refuse to clone into a non-empty dir — git clone would itself
    // refuse, but we want a pillbox-flavored error first.
    let any = fs::read_dir(cwd)
        .with_context(|| format!("read {}", cwd.display()))?
        .next()
        .is_some();
    if any {
        return Err(PillboxError::usage(
            "pillbox new",
            format!(
                "--from-git requires an empty directory (cwd `{}` is not empty)",
                cwd.display()
            ),
        )
        .into());
    }
    // Clone into a scratch sibling and merge so the descriptor write
    // doesn't fight git over an existing `.git`. We clone DIRECTLY
    // into cwd; git accepts this when the dir is empty.
    crate::workspace::git_inflow::clone_into(url, cwd, git_ref)?;
    Ok(())
}

/// `pillbox list` — every pillbox on disk.
pub(crate) fn list(json: bool) -> Result<()> {
    check_legacy_layout()?;
    let entries = collect_all()?;
    if json {
        println!("{}", build_list_json(&entries));
        return Ok(());
    }
    if entries.is_empty() {
        println!("(no pillboxes initialized)");
        println!();
        println!("Run `pillbox init` to create the global pillbox.");
        return Ok(());
    }
    println!("Pillboxes:");
    for pb in &entries {
        match &pb.scope {
            Scope::Global => {
                println!("  global  ({})", pb.state_dir.display());
            }
            Scope::Project { key, source_dir } => {
                let name = pb.display_name();
                println!("  {name:<20}  ({})", source_dir.display());
                println!("    key:   {key}");
                println!("    state: {}", pb.state_dir.display());
            }
        }
    }
    Ok(())
}

/// `pillbox rm NAME` — delete a pillbox by name. The global pillbox is
/// not removable through this command (would clobber every project's
/// inherited auth).
pub(crate) fn rm(name: &str) -> Result<()> {
    check_legacy_layout()?;
    if name == GLOBAL_NAME {
        return Err(PillboxError::usage(
            "pillbox rm",
            "refusing to remove the global pillbox via `rm` (would orphan inherited state)",
        )
        .with_next("rm -rf ~/.pillbox/global  # only if you mean it")
        .into());
    }
    let pb = lookup_by_name(name)?;
    let state_dir = pb.state_dir.clone();
    fs::remove_dir_all(&state_dir).with_context(|| format!("remove {}", state_dir.display()))?;
    println!(
        "pillbox: ✓ removed pillbox `{name}` ({})",
        state_dir.display()
    );
    Ok(())
}

/// `pillbox info` — show the resolved pillbox for cwd (or `--pillbox`).
pub(crate) fn info(explicit: Option<&str>, json: bool) -> Result<()> {
    let (pb, explicit_or_project) = Pillbox::resolve_with_source(explicit)?;
    if json {
        println!("{}", build_info_json(&pb, explicit_or_project));
        return Ok(());
    }
    match &pb.scope {
        Scope::Global => {
            println!("Current pillbox: global");
            println!("  state dir: {}", pb.state_dir.display());
            if !explicit_or_project {
                println!();
                println!("(no pillbox.toml found in cwd or any ancestor — falling back to global)");
                println!("Create a project pillbox with `pillbox new`.");
            }
        }
        Scope::Project { key, source_dir } => {
            println!("Current pillbox: {}", pb.display_name());
            println!("  source dir: {}", source_dir.display());
            println!("  state dir:  {}", pb.state_dir.display());
            println!("  key:        {key}");
            if let Some(m) = &pb.meta {
                if let Some(a) = &m.agent_default {
                    println!("  agent:      {a}");
                }
                println!("  created:    {}", m.created_at);
            }
            // Snapshot count — best-effort: opening the workspace can
            // fail on an in-flight repo or a missing password (e.g.
            // user just ran `pillbox new` and the repo dir was wiped
            // out of band). Failure isn't fatal for `info`; we just
            // skip the section. Keeps `info` always-printable.
            match pb.workspace().and_then(|b| b.snapshots()) {
                Ok(snaps) if snaps.is_empty() => {
                    println!("  snapshots:  none yet — run `pillbox push` to take the first");
                }
                Ok(snaps) => {
                    let latest = snaps.last().unwrap();
                    println!(
                        "  snapshots:  {} (latest: {} {})",
                        snaps.len(),
                        latest.handle.short(),
                        latest.created_at
                    );
                }
                Err(_) => {}
            }
        }
    }
    Ok(())
}

/// Detect the v0.5 layout (`~/.pillbox/data/`, `~/.pillbox/secrets/`,
/// `~/.pillbox/env/`, or `~/.pillbox/vault/` at the top level — NOT
/// inside `global/` or `projects/`) and error out with a migration
/// pointer. v0.6 is a hard reset, intentionally — no silent migration.
///
/// The list of detected names lives in `paths::V0_5_LEGACY_SUBDIRS` so
/// `doctor` flags the same set; do not inline the list here.
fn check_legacy_layout() -> Result<()> {
    let home = match std::env::var("HOME") {
        Ok(h) => PathBuf::from(h),
        Err(_) => return Ok(()),
    };
    let root = home.join(".pillbox");
    if !root.exists() {
        return Ok(());
    }
    let legacy = detect_legacy_subdirs(&root);
    if legacy.is_empty() {
        return Ok(());
    }
    let summary = legacy
        .iter()
        .map(|n| format!("~/.pillbox/{n}/"))
        .collect::<Vec<_>>()
        .join(", ");
    Err(PillboxError::config(
        "pillbox init",
        format!(
            "detected v0.5 pillbox state ({summary}). v0.6 is a hard reset — no migration shim."
        ),
    )
    .with_next(
        "mv ~/.pillbox ~/.pillbox.v0.5-backup && pillbox init  # then re-add secrets / login",
    )
    .into())
}

fn collect_all() -> Result<Vec<Pillbox>> {
    let mut out = Vec::new();
    let root = pillbox_root()?;
    if root.join(GLOBAL_NAME).is_dir() {
        out.push(global());
    }
    let projects = root.join("projects");
    if projects.is_dir() {
        for entry in
            fs::read_dir(&projects).with_context(|| format!("read {}", projects.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let key = match entry.file_name().into_string() {
                Ok(s) => s,
                Err(_) => continue,
            };
            let state_dir = entry.path();
            let meta = load_project_meta(&state_dir).ok().flatten();
            out.push(Pillbox {
                scope: Scope::Project {
                    key: key.clone(),
                    source_dir: decode_key_to_path(&key),
                },
                state_dir,
                meta,
            });
        }
    }
    // Stable order: global first, then projects sorted by display name.
    out.sort_by(|a, b| match (&a.scope, &b.scope) {
        (Scope::Global, Scope::Global) => std::cmp::Ordering::Equal,
        (Scope::Global, _) => std::cmp::Ordering::Less,
        (_, Scope::Global) => std::cmp::Ordering::Greater,
        _ => a.display_name().cmp(b.display_name()),
    });
    Ok(out)
}

fn default_name_from_dir(p: &Path) -> String {
    p.file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("pillbox")
        .to_string()
}

fn validate_agent(a: &str) -> Result<()> {
    crate::agents::lookup("pillbox new", a).map(|_| ())
}

fn write_descriptor(
    path: &Path,
    name: &str,
    agent: Option<&str>,
    workspace: &WorkspaceConfig,
) -> Result<()> {
    use std::io::Write;
    let mut body = format!("# v0.6 pillbox descriptor — see docs/config.md\n\nname = \"{name}\"\n");
    if let Some(a) = agent {
        body.push_str(&format!("agent = \"{a}\"\n"));
    }
    // Always emit a `[workspace]` table so the user sees the resolved
    // backend even when it's the default.
    body.push_str("\n[workspace]\n");
    let backend = workspace.backend_kind();
    body.push_str(&format!("backend = \"{}\"\n", backend.as_str()));
    if backend == BackendKind::S3 {
        if let Some(v) = workspace.endpoint.as_deref() {
            body.push_str(&format!("endpoint = \"{v}\"\n"));
        }
        if let Some(v) = workspace.region.as_deref() {
            body.push_str(&format!("region = \"{v}\"\n"));
        }
        if let Some(v) = workspace.bucket.as_deref() {
            body.push_str(&format!("bucket = \"{v}\"\n"));
        }
        if let Some(v) = workspace.prefix.as_deref() {
            body.push_str(&format!("prefix = \"{v}\"\n"));
        }
        if let Some(v) = workspace.access_key_env.as_deref() {
            body.push_str(&format!("access_key_env = \"{v}\"\n"));
        }
        if let Some(v) = workspace.secret_key_env.as_deref() {
            body.push_str(&format!("secret_key_env = \"{v}\"\n"));
        }
    }
    let mut f = fs::File::create(path).with_context(|| format!("create {}", path.display()))?;
    f.write_all(body.as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn require_field(v: Option<&str>, name: &'static str) -> Result<String> {
    v.map(str::to_string).ok_or_else(|| {
        PillboxError::config(
            "workspace",
            format!("[workspace] is missing required field `{name}` for the s3 backend"),
        )
        .into()
    })
}

fn read_env_or_err(name: &str) -> Result<String> {
    std::env::var(name).map_err(|_| {
        PillboxError::runtime(
            "workspace",
            format!("env var `{name}` is not set (referenced by [workspace] in pillbox.toml)"),
        )
        .with_next(format!("export {name}=...  # then re-run"))
        .into()
    })
}

fn rfc3339_now() -> String {
    use time::OffsetDateTime;
    let now = OffsetDateTime::now_utc();
    now.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

// ── JSON ───────────────────────────────────────────────────────────────────

fn build_list_json(entries: &[Pillbox]) -> String {
    let arr: Vec<serde_json::Value> = entries.iter().map(pillbox_json).collect();
    crate::paths::json_v1(vec![("pillboxes", serde_json::Value::Array(arr))])
}

fn build_info_json(pb: &Pillbox, explicit_or_project: bool) -> String {
    crate::paths::json_v1(vec![
        ("pillbox", pillbox_json(pb)),
        (
            "from_pillbox_toml",
            serde_json::Value::Bool(explicit_or_project),
        ),
    ])
}

fn pillbox_json(pb: &Pillbox) -> serde_json::Value {
    let mut o = serde_json::Map::new();
    o.insert(
        "name".into(),
        serde_json::Value::String(pb.display_name().into()),
    );
    let scope = match &pb.scope {
        Scope::Global => "global",
        Scope::Project { .. } => "project",
    };
    o.insert("scope".into(), serde_json::Value::String(scope.into()));
    o.insert(
        "state_dir".into(),
        serde_json::Value::String(pb.state_dir.display().to_string()),
    );
    if let Scope::Project { key, source_dir } = &pb.scope {
        o.insert("key".into(), serde_json::Value::String(key.clone()));
        o.insert(
            "source_dir".into(),
            serde_json::Value::String(source_dir.display().to_string()),
        );
    }
    if let Some(m) = &pb.meta {
        if let Some(a) = &m.agent_default {
            o.insert("agent".into(), serde_json::Value::String(a.clone()));
        }
        o.insert(
            "created_at".into(),
            serde_json::Value::String(m.created_at.clone()),
        );
    }
    serde_json::Value::Object(o)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::with_isolated_home;

    #[test]
    fn path_to_key_dash_encodes_absolute_path() {
        // Use a path that exists post-canonicalize fallback. Spec example:
        // /Users/vuln/code/aeon-v2 → -Users-vuln-code-aeon-v2.
        let p = Path::new("/Users/vuln/code/aeon-v2");
        let key = path_to_key(p).unwrap();
        assert_eq!(key, "-Users-vuln-code-aeon-v2");
    }

    #[test]
    fn path_to_key_rejects_relative_path() {
        let err = path_to_key(Path::new("relative/path")).unwrap_err();
        let s = format!("{err}");
        assert!(s.contains("absolute"));
    }

    #[test]
    fn init_creates_global_state_dir() {
        with_isolated_home("pillbox-init", || {
            init().unwrap();
            let home = std::env::var("HOME").unwrap();
            let global = PathBuf::from(home).join(".pillbox/global");
            assert!(global.is_dir(), "global dir not created");
        });
    }

    #[test]
    fn init_is_idempotent() {
        with_isolated_home("pillbox-init-idempotent", || {
            init().unwrap();
            // Second init should not error.
            init().unwrap();
        });
    }

    #[test]
    fn init_errors_on_legacy_layout() {
        with_isolated_home("pillbox-legacy", || {
            let home = std::env::var("HOME").unwrap();
            let legacy = PathBuf::from(&home).join(".pillbox/data/claude");
            fs::create_dir_all(&legacy).unwrap();
            let err = init().unwrap_err();
            let s = format!("{err}");
            assert!(s.contains("v0.5") || s.contains("legacy"), "got: {s}");
        });
    }

    #[test]
    fn new_creates_project_state_and_descriptor() {
        with_isolated_home("pillbox-new", || {
            let tmp = tempfile::tempdir().unwrap();
            let saved_cwd = std::env::current_dir().ok();
            std::env::set_current_dir(tmp.path()).unwrap();

            new(
                Some("alpha".into()),
                Some("claude".into()),
                NewWorkspaceArgs::default(),
            )
            .unwrap();

            let descriptor = tmp.path().join(PILLBOX_TOML);
            assert!(descriptor.is_file());
            let body = fs::read_to_string(&descriptor).unwrap();
            assert!(body.contains("name = \"alpha\""));
            assert!(body.contains("agent = \"claude\""));

            // State dir under projects/<key>.
            let key = path_to_key(tmp.path()).unwrap();
            let home = std::env::var("HOME").unwrap();
            let state = PathBuf::from(home).join(".pillbox/projects").join(&key);
            assert!(state.is_dir());
            assert!(state.join("meta.json").is_file());

            if let Some(c) = saved_cwd {
                let _ = std::env::set_current_dir(c);
            }
        });
    }

    #[test]
    fn new_refuses_when_descriptor_exists() {
        with_isolated_home("pillbox-new-exists", || {
            let tmp = tempfile::tempdir().unwrap();
            let saved_cwd = std::env::current_dir().ok();
            std::env::set_current_dir(tmp.path()).unwrap();
            fs::write(tmp.path().join(PILLBOX_TOML), "name = \"x\"\n").unwrap();

            let err = new(None, None, NewWorkspaceArgs::default()).unwrap_err();
            let s = format!("{err}");
            assert!(s.contains("already exists"), "got: {s}");

            if let Some(c) = saved_cwd {
                let _ = std::env::set_current_dir(c);
            }
        });
    }

    #[test]
    fn discover_walks_up_to_find_descriptor() {
        with_isolated_home("pillbox-discover", || {
            let root = tempfile::tempdir().unwrap();
            let nested = root.path().join("a/b/c");
            fs::create_dir_all(&nested).unwrap();
            fs::write(root.path().join(PILLBOX_TOML), "name = \"frommarker\"\n").unwrap();

            let found = discover_from(&nested).unwrap().unwrap();
            match &found.scope {
                Scope::Project { source_dir, .. } => {
                    // canonicalize the temp root for comparison —
                    // `discover_from` may have canonicalized while
                    // computing the key.
                    let expected = fs::canonicalize(root.path()).unwrap();
                    let got = fs::canonicalize(source_dir).unwrap();
                    assert_eq!(got, expected);
                }
                _ => panic!("expected project scope"),
            }
        });
    }

    #[test]
    fn resolve_falls_back_to_global() {
        with_isolated_home("pillbox-fallback", || {
            let root = tempfile::tempdir().unwrap();
            let saved_cwd = std::env::current_dir().ok();
            std::env::set_current_dir(root.path()).unwrap();

            let pb = Pillbox::resolve(None).unwrap();
            assert!(pb.is_global());

            if let Some(c) = saved_cwd {
                let _ = std::env::set_current_dir(c);
            }
        });
    }

    #[test]
    fn resolve_explicit_finds_by_name() {
        with_isolated_home("pillbox-resolve-name", || {
            let tmp = tempfile::tempdir().unwrap();
            let saved_cwd = std::env::current_dir().ok();
            std::env::set_current_dir(tmp.path()).unwrap();
            new(Some("alpha".into()), None, NewWorkspaceArgs::default()).unwrap();

            // Now from /tmp (no descriptor), resolve by name.
            std::env::set_current_dir("/tmp").unwrap();
            let pb = Pillbox::resolve(Some("alpha")).unwrap();
            assert_eq!(pb.display_name(), "alpha");

            if let Some(c) = saved_cwd {
                let _ = std::env::set_current_dir(c);
            }
        });
    }

    #[test]
    fn resolve_explicit_global() {
        with_isolated_home("pillbox-resolve-global", || {
            init().unwrap();
            let pb = Pillbox::resolve(Some("global")).unwrap();
            assert!(pb.is_global());
        });
    }

    #[test]
    fn list_includes_global_and_projects() {
        with_isolated_home("pillbox-list", || {
            init().unwrap();
            let tmp = tempfile::tempdir().unwrap();
            let saved_cwd = std::env::current_dir().ok();
            std::env::set_current_dir(tmp.path()).unwrap();
            new(Some("beta".into()), None, NewWorkspaceArgs::default()).unwrap();

            let all = collect_all().unwrap();
            assert!(all.iter().any(|p| p.is_global()));
            assert!(all.iter().any(|p| p.display_name() == "beta"));

            if let Some(c) = saved_cwd {
                let _ = std::env::set_current_dir(c);
            }
        });
    }

    #[test]
    fn rm_removes_project_pillbox() {
        with_isolated_home("pillbox-rm", || {
            let tmp = tempfile::tempdir().unwrap();
            let saved_cwd = std::env::current_dir().ok();
            std::env::set_current_dir(tmp.path()).unwrap();
            new(Some("gamma".into()), None, NewWorkspaceArgs::default()).unwrap();

            let key = path_to_key(tmp.path()).unwrap();
            let home = std::env::var("HOME").unwrap();
            let state = PathBuf::from(&home).join(".pillbox/projects").join(&key);
            assert!(state.is_dir());

            rm("gamma").unwrap();
            assert!(!state.exists());

            if let Some(c) = saved_cwd {
                let _ = std::env::set_current_dir(c);
            }
        });
    }

    #[test]
    fn rm_refuses_global() {
        with_isolated_home("pillbox-rm-global", || {
            init().unwrap();
            let err = rm("global").unwrap_err();
            let s = format!("{err}");
            assert!(s.contains("refusing") || s.contains("global"), "got: {s}");
        });
    }
}
