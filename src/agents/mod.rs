//! Per-agent adapters.
//!
//! Each agent is described by a small data struct ([`AgentSpec`]) — the
//! provider id, the guest paths it reads/writes, the argv to invoke for
//! login vs run, and an optional OAuth callback port to forward. The
//! login + run flows are generic over that spec; per-agent files are
//! reserved for adapters that grow agent-specific quirks beyond the
//! spec (none yet).

use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Result};

use crate::{docker, keychain};

/// Hard cap on the total bytes pillbox will stash in a keychain entry.
/// Real auth bundles for the agents we ship are a few KB each. If we see
/// 1 MB+, the agent has probably leaked unrelated state (a sessions/
/// directory, history, etc.) into the cred dir — refuse rather than
/// overflow some keychain backend's per-entry limit.
const MAX_BUNDLE_BYTES: usize = 1024 * 1024;

const GUEST_HOME: &str = "/home/lum";
const GUEST_WORKSPACE: &str = "/workspace";

/// Static description of a coding-agent adapter. Adding a new agent =
/// adding one `AgentSpec` constant below and one entry in `ALL`.
#[derive(Clone, Copy)]
pub struct AgentSpec {
    /// Provider id — keychain account name + CLI subject (e.g. `claude`).
    pub(crate) id: &'static str,
    /// Paths (relative to the guest's HOME) where the agent stores its
    /// persistent state. Files OR directories. Pillbox bind-mounts a
    /// single host tempdir as HOME and captures only the contents of
    /// these paths — ephemeral chatter (history, sessions, cache) that
    /// the agent writes elsewhere in HOME is NOT persisted.
    ///
    /// Claude in particular writes both `.claude/.credentials.json` (auth)
    /// AND `.claude.json` (profile config, sibling of the cred dir) —
    /// missing the latter is what makes its first-run wizard fire
    /// every session.
    pub(crate) state_paths: &'static [&'static str],
    /// Path (relative to HOME) of the file pillbox checks for after
    /// `login` to confirm the agent actually wrote credentials.
    pub(crate) cred_sentinel: &'static str,
    /// argv for the login flow (runs after the standard `docker run`
    /// flags + image name).
    pub(crate) login_argv: &'static [&'static str],
    /// argv prefix for the run flow. User-supplied args are appended.
    pub(crate) run_argv: &'static [&'static str],
    /// OAuth callback port the agent's login server binds inside the
    /// sandbox. `None` for agents that use device-code flow (no
    /// callback, no port forward needed). For callback-based agents we
    /// match the agent's hardcoded port and rely on the user overriding
    /// via `PILLBOX_<ID>_OAUTH_PORT` if the agent ever moves it.
    pub(crate) oauth_port: Option<u16>,
}

pub const CLAUDE: AgentSpec = AgentSpec {
    id: "claude",
    state_paths: &[".claude", ".claude.json"],
    cred_sentinel: ".claude/.credentials.json",
    login_argv: &["claude", "auth", "login", "--claudeai"],
    run_argv: &["claude"],
    oauth_port: Some(54545),
};

// codex's default login starts a localhost callback server on a port it
// picks itself (observed: 1455). Inside a sandbox that port isn't bound on
// the host, so the browser redirect 404s. codex ships `--device-auth`
// specifically for headless/sandbox use: it shows a URL + code in the
// terminal, user pastes the code in their browser, codex polls for
// completion. No port forwarding, no callback server.
pub const CODEX: AgentSpec = AgentSpec {
    id: "codex",
    state_paths: &[".codex"],
    cred_sentinel: ".codex/auth.json",
    login_argv: &["codex", "login", "--device-auth"],
    run_argv: &["codex"],
    oauth_port: None,
};

/// Every shipped agent adapter. `pillbox auth list` and other
/// keychain-iterating callers walk this so they stay in sync as new
/// agents are added.
pub const ALL: &[&AgentSpec] = &[&CLAUDE, &CODEX];

impl AgentSpec {
    pub fn id(&self) -> &'static str {
        self.id
    }

    /// OAuth callback port to forward, after applying any user override.
    /// `PILLBOX_<UPPERCASE_ID>_OAUTH_PORT` lets a user patch around the
    /// agent moving its hardcoded port without rebuilding pillbox.
    /// Unparseable override values silently fall back to the hardcoded
    /// port — pillbox doesn't surface a warning because users typically
    /// only set this when something's already gone wrong with login.
    fn resolved_oauth_port(&self) -> Option<u16> {
        let default = self.oauth_port?;
        let var = format!("PILLBOX_{}_OAUTH_PORT", self.id.to_uppercase());
        let port = std::env::var(&var)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(default);
        Some(port)
    }
}

impl AgentSpec {
    pub fn login(&self) -> Result<()> {
        docker::check_ready()?;

        let tmp = TempDir::create(&format!("pillbox-{}-login", self.id))?;
        let mount = format!("{}:{GUEST_HOME}", tmp.path().display());

        let mut args = base_docker_args();
        if let Some(port) = self.resolved_oauth_port() {
            args.push("-p".into());
            args.push(format!("{port}:{port}"));
        }
        args.push("-v".into());
        args.push(mount);
        args.push(docker::RUNNER_IMAGE.into());
        args.extend(self.login_argv.iter().map(|s| s.to_string()));

        println!("pillbox: starting {} login inside a sandbox.", self.id);
        println!("pillbox: follow the prompts (and any URL) printed by the sandbox below.");
        println!();

        let status = docker::run_interactive(&args)?;
        if !status.success() {
            return Err(anyhow!(
                "{} login exited with status {status}. Re-run `pillbox {} login`.",
                self.id, self.id
            ));
        }

        let bundle = capture_bundle(tmp.path(), self.state_paths)?;
        if !bundle.files.contains_key(self.cred_sentinel) {
            return Err(anyhow!(
                "{} login completed but `{}` is missing under {}.\n\
                 Check the sandbox output above for hints.",
                self.id,
                self.cred_sentinel,
                tmp.path().display()
            ));
        }
        let payload = bundle.to_json()?;
        keychain::save(self.id, &payload)?;
        drop(tmp);

        println!();
        println!(
            "pillbox: ✓ credentials stored in your OS keychain (service `pillbox`, account `{}`).",
            self.id
        );
        println!("pillbox: try `pillbox {} run` to launch it in a sandboxed shell.", self.id);
        Ok(())
    }

    pub fn run(&self, opts: RunOpts) -> Result<()> {
        docker::check_ready()?;

        let payload = keychain::load(self.id)?.ok_or_else(|| {
            anyhow!(
                "no stored credentials for `{}`. Run `pillbox {} login` first.",
                self.id, self.id
            )
        })?;
        let bundle = Bundle::from_json(&payload, self.cred_sentinel)?;

        let tmp = TempDir::create(&format!("pillbox-{}-creds", self.id))?;
        bundle.restore_into(tmp.path())?;

        let workspace_host = match opts.workspace {
            Some(p) => p,
            None => std::env::current_dir().context("resolve current working directory")?,
        };
        let workspace_name = workspace_mount_name(&workspace_host, opts.name.as_deref())?;
        let guest_workspace = format!("{GUEST_WORKSPACE}/{workspace_name}");

        let mut args = base_docker_args();
        args.extend([
            "-v".into(),
            format!("{}:{GUEST_HOME}", tmp.path().display()),
            "-v".into(),
            format!("{}:{guest_workspace}", workspace_host.display()),
            "-w".into(),
            guest_workspace,
        ]);
        for m in &opts.mounts {
            args.push("-v".into());
            args.push(m.clone());
        }
        args.push(docker::RUNNER_IMAGE.into());
        args.extend(self.run_argv.iter().map(|s| s.to_string()));
        args.extend(opts.args);

        let status = docker::run_interactive(&args)?;

        // Persist any rotated tokens / updated state back to keychain.
        // Only paths in `state_paths` are captured — random session/
        // history files the agent wrote elsewhere in HOME are dropped
        // with the tempdir.
        write_back_bundle(self.id, tmp.path(), &payload, self.state_paths, self.cred_sentinel);
        drop(tmp);

        if !status.success() {
            return Err(anyhow!("{} exited with status {status}", self.id));
        }
        Ok(())
    }
}

/// Capture the agent's tracked state paths after the agent exits and
/// persist back to keychain if anything changed. Best-effort: warns on
/// failure rather than failing the whole `run`, since the user has
/// already gotten value from the session and the next run will
/// rediscover any genuinely broken state.
fn write_back_bundle(
    provider: &str,
    tmp: &Path,
    original: &str,
    state_paths: &[&str],
    cred_sentinel: &str,
) {
    let bundle = match capture_bundle(tmp, state_paths) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("pillbox: warning: could not snapshot {provider} state for write-back: {e}");
            return;
        }
    };
    // Sentinel must still be present for the bundle to be useful.
    // Agents sometimes transiently remove/replace the cred file during
    // refresh; if it's missing at exit, refuse to overwrite keychain
    // with a partial bundle.
    if !bundle.files.contains_key(cred_sentinel) {
        eprintln!(
            "pillbox: warning: {provider} state at exit is missing `{cred_sentinel}`; \
             keychain not updated."
        );
        return;
    }
    let refreshed = match bundle.to_json() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("pillbox: warning: could not serialize {provider} state: {e}");
            return;
        }
    };
    if refreshed == original {
        return;
    }
    if let Err(e) = keychain::save(provider, &refreshed) {
        eprintln!(
            "pillbox: warning: failed to write refreshed {provider} state to keychain: {e}"
        );
    }
}

/// On-host representation of an agent's full credential-dir state.
/// `files` keys are POSIX-style relative paths under `guest_cred_dir`;
/// values are the raw file contents. We treat everything as UTF-8 — the
/// cred files we know about (claude `.credentials.json`, codex
/// `auth.json`, claude `settings.json`) are text. If an agent ever
/// writes binary auth state, this'll need a base64 escape hatch.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Bundle {
    files: BTreeMap<String, String>,
}

impl Bundle {
    fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).context("serialize credential bundle")
    }

    fn from_json(payload: &str, cred_sentinel: &str) -> Result<Self> {
        // Back-compat: pillbox v0.2-alpha stored the raw .credentials.json
        // contents directly. If the payload looks like a single-file
        // bare cred (an object that's NOT a `{files:{...}}` bundle), wrap
        // it on the fly using the sentinel path. Detection heuristic:
        // bundles start with the literal `{"files":`.
        let trimmed = payload.trim_start();
        if !trimmed.starts_with("{\"files\":") {
            let mut files = BTreeMap::new();
            files.insert(cred_sentinel.to_string(), payload.to_string());
            return Ok(Self { files });
        }
        serde_json::from_str(payload).context("parse credential bundle")
    }

    fn restore_into(&self, dir: &Path) -> Result<()> {
        for (rel, contents) in &self.files {
            let dest = dir.join(rel);
            if let Some(parent) = dest.parent() {
                if parent != dir {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("create {}", parent.display()))?;
                }
            }
            write_secret(&dest, contents)?;
        }
        Ok(())
    }
}

/// Snapshot the named `state_paths` (relative to `root`) into a `Bundle`.
/// Each path may be a file or a directory; directories are walked
/// recursively. Missing paths are skipped silently — an agent on first
/// login may not have written every path yet. Refuses bundles over
/// [`MAX_BUNDLE_BYTES`] so a session/history leak doesn't overflow a
/// keychain backend.
fn capture_bundle(root: &Path, state_paths: &[&str]) -> Result<Bundle> {
    let mut files = BTreeMap::new();
    let mut total_bytes = 0usize;
    for state in state_paths {
        let abs = root.join(state);
        let meta = match fs::symlink_metadata(&abs) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("stat {}", abs.display())),
        };
        if meta.is_file() {
            capture_file(root, &abs, &mut files, &mut total_bytes)?;
        } else if meta.is_dir() {
            walk_files(root, &abs, &mut files, &mut total_bytes)?;
        }
        // Symlinks / other types intentionally skipped — auth state shouldn't include them.
    }
    Ok(Bundle { files })
}

fn capture_file(
    root: &Path,
    path: &Path,
    out: &mut BTreeMap<String, String>,
    total: &mut usize,
) -> Result<()> {
    let rel = path
        .strip_prefix(root)
        .with_context(|| format!("relativize {}", path.display()))?
        .to_string_lossy()
        .into_owned();
    let contents =
        fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    *total += contents.len();
    if *total > MAX_BUNDLE_BYTES {
        return Err(anyhow!(
            "credential bundle exceeds {MAX_BUNDLE_BYTES} bytes \
             — refusing to stash that much state in the keychain"
        ));
    }
    out.insert(rel, contents);
    Ok(())
}

fn walk_files(
    root: &Path,
    cur: &Path,
    out: &mut BTreeMap<String, String>,
    total: &mut usize,
) -> Result<()> {
    for entry in fs::read_dir(cur).with_context(|| format!("read {}", cur.display()))? {
        let entry = entry.with_context(|| format!("read entry under {}", cur.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("stat {}", path.display()))?;
        if file_type.is_dir() {
            walk_files(root, &path, out, total)?;
        } else if file_type.is_file() {
            capture_file(root, &path, out, total)?;
        }
        // Symlinks/sockets/etc. skipped.
    }
    Ok(())
}

/// Caller-supplied options for `AgentSpec::run`. Built from CLI args in
/// [`crate::main`] but kept as a plain struct so the agents layer doesn't
/// depend on clap.
pub struct RunOpts {
    /// Host path to mount as the workspace. `None` = use the current
    /// working directory.
    pub workspace: Option<PathBuf>,
    /// Override the basename used as the workspace mount point inside the
    /// guest. `None` = derive from `workspace.file_name()`.
    pub name: Option<String>,
    /// Extra bind mounts as `HOST:GUEST` strings, passed through to
    /// `docker run -v` verbatim. Repeatable.
    pub mounts: Vec<String>,
    /// Args forwarded to the agent CLI inside the guest.
    pub args: Vec<String>,
}

/// Resolve the directory name we use as the workspace mount point inside
/// the guest (`/workspace/<name>`).
///
/// Priority:
///   1. `--name` override if provided (validated as a single path component)
///   2. The host workspace dir's basename
///   3. Fall back to `workspace` for unusual paths (e.g. `/` has no basename)
fn workspace_mount_name(host: &Path, override_name: Option<&str>) -> Result<String> {
    if let Some(name) = override_name {
        if name.is_empty() || name.contains('/') || name.contains('\0') {
            return Err(anyhow!(
                "--name `{name}` must be a non-empty single path component (no `/` or NUL)"
            ));
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

/// Shared flags + env every pillbox docker invocation needs.
fn base_docker_args() -> Vec<String> {
    vec![
        "-it".into(),
        "--rm".into(),
        "-e".into(),
        format!("HOME={GUEST_HOME}"),
        "-e".into(),
        "TERM=xterm-256color".into(),
    ]
}

/// Create a file with 0600 perms from the start (no world-readable window).
fn write_secret(path: &Path, payload: &str) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("open {} for write", path.display()))?;
    file.write_all(payload.as_bytes())
        .with_context(|| format!("write to {}", path.display()))?;
    Ok(())
}

/// RAII tempdir guard. Removes the directory (and any contents — including
/// captured credentials) on drop, whether the caller exits via Ok, Err, or
/// panic. Primary defense against leaving credentials on disk when
/// something fails between login and keychain save.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn create(prefix: &str) -> Result<Self> {
        // Avoid the `tempfile` crate to keep the dep tree minimal.
        // SystemTime nanos are unique enough for normal sequential use,
        // but two pillbox invocations launched in the same nanosecond
        // (CI bursts, coarse-clock VMs) would collide. Cheap retry on
        // EEXIST handles that.
        let base = std::env::temp_dir();
        for attempt in 0u32..16 {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = base.join(format!("{prefix}-{nanos:x}-{attempt}"));
            match fs::create_dir(&dir) {
                Ok(()) => {
                    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
                        .with_context(|| format!("chmod {} 0700", dir.display()))?;
                    return Ok(Self { path: dir });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("create tempdir {}", dir.display()));
                }
            }
        }
        Err(anyhow!("could not create a unique tempdir under {} after 16 attempts", base.display()))
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
