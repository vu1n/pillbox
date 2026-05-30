//! The vault-stdin **blob protocol** — construction half: the wire format
//! every remote backend ships one run-worth of state in (auth, secrets, env,
//! workspace base), plus the host-side builder that assembles it.
//!
//! Extracted from `remote_ssh.rs` so the backends (e2b, docker://) consume the
//! shared contract from a neutral module instead of importing it from a sibling
//! backend — and so `remote_ssh.rs` shrinks toward the ssh-transport-only thing
//! the redesign says it should become. The *receiving* half
//! (`dispatch_vault_stdin{,_direct}`, reached via the `pillbox run
//! --vault-stdin[-direct]` CLI flags) still lives in `remote_ssh.rs` and imports
//! these types; relocating it is a follow-up.

use std::fmt;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::agents::{workspace_mount_name, AgentSpec, RunOpts};
use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::vault::VaultMeta;
use crate::workspace::rustic::{RusticBackend, RusticVariant, S3Config};
use crate::workspace::{PushOptions, WorkspaceBackend};

/// Wire format for `pillbox run --vault-stdin`. See module docs.
///
/// `Debug` is implemented by hand to keep secret-bearing `InlineSecret`
/// values out of any accidental `{:?}` / `dbg!` / tracing output.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct VaultStdinBlob {
    pub(crate) version: u32,
    pub(crate) agent_id: String,
    #[serde(default)]
    pub(crate) agent_args: Vec<String>,
    pub(crate) workspace_mount_name: String,
    #[serde(default)]
    pub(crate) vault: bool,
    #[serde(default)]
    pub(crate) secrets: Vec<InlineSecret>,
    #[serde(default)]
    pub(crate) env: std::collections::BTreeMap<String, String>,
    /// Per-run telemetry context (session_id, mode, workspace_id).
    /// `#[serde(flatten)]` keeps the wire shape identical to the
    /// previous one-field-each layout — older host binaries that
    /// only knew `session_id` still produce a wire-compatible blob,
    /// and new context fields land here automatically as
    /// [`crate::vault::RunContext`] grows. The sandbox-resident
    /// vault reads this back out via [`Self::run_context`] to seed
    /// its gen_ai span emission.
    #[serde(flatten)]
    pub(crate) context: crate::vault::RunContext,
    /// S3/rustic workspace material for the **hydrate-from-S3** receivers
    /// (e2b/ssh): the sandbox pulls the base snapshot and pushes the result
    /// through the shared repo. `None` for the **docker://** path, which
    /// pre-stages the workspace via tar-cp ([`workspace_dir`]) and pulls
    /// results back host-side — no S3. `#[serde(default)]` keeps the wire
    /// backward-compatible; exactly one of `workspace` / `workspace_dir` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) workspace: Option<InlineWorkspace>,
    /// Container path of a workspace pre-staged via tar-cp (the docker://
    /// path). When set, the direct dispatch runs the agent against it and
    /// skips S3 hydrate + S3 result-push (results come back host-side).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) workspace_dir: Option<String>,
    /// Files copied from the host's `<auth_pillbox>/auth/<agent_id>/`,
    /// to be re-materialized under the agent's `$HOME` on the receiver.
    /// Populated for the direct-exec path ([`dispatch_vault_stdin_direct`])
    /// where the sandbox has no pre-existing agent login; Docker-shelled
    /// receivers (the SSH backend's [`dispatch_vault_stdin`]) ignore it
    /// because they mount `home_dir` directly. `#[serde(default)]` keeps
    /// the wire backward-compatible.
    #[serde(default)]
    pub(crate) agent_auth: Vec<AuthFile>,
}

impl fmt::Debug for VaultStdinBlob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let auth_bytes: usize = self.agent_auth.iter().map(|f| f.contents.len()).sum();
        f.debug_struct("VaultStdinBlob")
            .field("version", &self.version)
            .field("agent_id", &self.agent_id)
            .field("agent_args", &self.agent_args)
            .field("workspace_mount_name", &self.workspace_mount_name)
            .field("vault", &self.vault)
            .field("secrets", &self.secrets)
            .field("env_keys", &self.env.keys().collect::<Vec<_>>())
            .field("workspace", &self.workspace)
            .field("workspace_dir", &self.workspace_dir)
            .field(
                "agent_auth",
                &format_args!(
                    "<{} files, {}B redacted>",
                    self.agent_auth.len(),
                    auth_bytes
                ),
            )
            .finish()
    }
}

/// Workspace material needed by a remote host to hydrate and publish through
/// the same S3/R2 rustic repository as the local pillbox.
///
/// `s3` is the repo config verbatim — it slots straight back into a
/// [`RusticBackend`] on the remote, no field-by-field copy. This is
/// secret-bearing (`s3` credentials + `repo_password` are real values,
/// not env var names); `Debug` redacts them.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct InlineWorkspace {
    pub(crate) s3: S3Config,
    pub(crate) repo_password: String,
    #[serde(default)]
    pub(crate) base_snapshot: Option<String>,
}

impl fmt::Debug for InlineWorkspace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `s3` redacts its own credentials via its `Debug` impl.
        f.debug_struct("InlineWorkspace")
            .field("s3", &self.s3)
            .field("repo_password", &"<redacted>")
            .field("base_snapshot", &self.base_snapshot)
            .finish()
    }
}

/// One `--with` entry's resolved form on the wire. `vault_meta` is
/// `None` for plain secrets; `Some` for vaulted ones (the remote
/// re-leases through its own vault session at swap time).
///
/// `Debug` is implemented by hand so `value` (real secret material)
/// never lands in logs / panic backtraces / tracing spans.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct InlineSecret {
    pub(crate) name: String,
    pub(crate) env_var: String,
    pub(crate) value: String,
    #[serde(default)]
    pub(crate) vault_meta: Option<VaultMeta>,
}

impl fmt::Debug for InlineSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InlineSecret")
            .field("name", &self.name)
            .field("env_var", &self.env_var)
            .field("value", &"<redacted>")
            .field("vault_meta", &self.vault_meta)
            .finish()
    }
}

/// One file lifted from the LOCAL host's `<auth_pillbox>/auth/<agent_id>/`
/// and serialized into the blob so the receiving sandbox can rehydrate the
/// agent's `$HOME` before exec. `rel_path` is normalized to forward slashes
/// and validated against `..` / absolute components — the receiver joins it
/// onto `$HOME` directly.
///
/// `Debug` redacts `contents` so accidental tracing of the blob doesn't
/// surface OAuth tokens / session keys.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct AuthFile {
    pub(crate) rel_path: String,
    /// 0o600 (single-file) / 0o700 (directory) on extract. Mode 0o644 is
    /// upcast to 0o600; we never widen perms relative to the source dir.
    pub(crate) mode: u32,
    #[serde(with = "base64_bytes")]
    pub(crate) contents: Vec<u8>,
}

impl fmt::Debug for AuthFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthFile")
            .field("rel_path", &self.rel_path)
            .field("mode", &format_args!("0o{:o}", self.mode))
            .field(
                "contents",
                &format_args!("<{}B redacted>", self.contents.len()),
            )
            .finish()
    }
}

/// Bytes ↔ base64 (standard alphabet, padded) for the JSON wire. The
/// receiver doesn't need to interpret these — `from_bytes` just decodes
/// and writes back — so we use the standard engine, not the URL variant.
mod base64_bytes {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(b: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(b))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let raw = String::deserialize(d)?;
        STANDARD
            .decode(raw.as_bytes())
            .map_err(serde::de::Error::custom)
    }
}

/// Current blob version. Bumped only on breaking changes; the remote
/// rejects unknown future versions explicitly so a newer-CLI / older-
/// remote combo fails loudly instead of silently dropping required
/// fields. Unknown JSON keys WITHIN a known version are still tolerated
/// (serde default) — version bumps are reserved for semantic changes.
pub(crate) const BLOB_VERSION: u32 = 2;

pub(crate) const RESULT_SNAPSHOT_FILE_ENV: &str = "PILLBOX_RESULT_SNAPSHOT_FILE";

impl VaultStdinBlob {
    pub(crate) fn to_bytes(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self).with_context(|| "serialize vault-stdin blob".to_string())
    }

    pub(crate) fn from_bytes(buf: &[u8]) -> Result<Self> {
        // We deliberately don't surface serde_json's error detail: it
        // includes line/column/snippet of the offending bytes, which
        // could echo back partial secret material if the blob is
        // corrupted or if a user accidentally pipes a credential file
        // into `pillbox run --vault-stdin`.
        let blob: VaultStdinBlob = serde_json::from_slice(buf).map_err(|_| {
            PillboxError::config("vault-stdin", "invalid JSON blob on stdin".to_string())
        })?;
        if blob.version != BLOB_VERSION {
            return Err(PillboxError::config(
                "vault-stdin",
                format!(
                    "blob version {} not supported by this pillbox (expected {})",
                    blob.version, BLOB_VERSION
                ),
            )
            .with_next("upgrade pillbox on the remote host to match the client version")
            .into());
        }
        Ok(blob)
    }
}

/// Build a [`VaultStdinBlob`] from `RunOpts` against the resolved pillbox.
/// Shared between the SSH backend (sends inline over stdin) and the E2B
/// backend (stages the same struct to a sandbox tmp file via the Files
/// API). Both backends consume identical wire shape on the remote side
/// (`pillbox run --vault-stdin`), so the resolution logic lives here in
/// one place — secret material crosses the host boundary once, in a
/// known JSON wrapping with `BLOB_VERSION`.
///
/// `action` is the diagnostic label threaded into error messages (e.g.
/// `"run --remote"` vs `"run --remote (e2b)"`) so the user sees which
/// backend they were on when a `--with` / `--env` resolution failed.
/// Defense-in-depth cap on total forwarded auth bytes. The narrowed walk
/// below (non-recursive over `cred_sentinel`'s parent dir) keeps real
/// auth state well under this — typical case is a few hundred KB — but a
/// pathological config-file size should error loudly rather than smuggle
/// the agent's whole dir into the sandbox blob.
const AGENT_AUTH_MAX_BYTES: u64 = 10 * 1024 * 1024;
/// Per-file ceiling (matches `AGENT_AUTH_MAX_BYTES` for now): a single
/// runaway config file would dominate the blob just as badly as many.
const AGENT_AUTH_MAX_FILE_BYTES: u64 = AGENT_AUTH_MAX_BYTES;

/// Collect the agent's canonical auth state from `home` for forwarding to
/// a sandbox without a pre-existing login. The walk is intentionally
/// narrow: only **regular files in `cred_sentinel`'s parent directory**,
/// non-recursive. That covers
///
///   - claude:   `.claude/.credentials.json` + sibling config(s)
///   - codex:    `.codex/auth.json` + sibling config(s)
///   - opencode: `.local/share/opencode/auth.json` + siblings
///   - pi:       `.pi/agent/auth.json` + siblings
///
/// and excludes the noisy cache/history subdirs adjacent to it (e.g.
/// `.claude/projects/` per-project session histories, easily many MiB)
/// which a fresh remote `-p`-style run does not need. Symlinks are
/// skipped so a hostile link can't pull in arbitrary host bytes.
fn collect_agent_auth(
    home: &Path,
    cred_sentinel: &str,
    action: &'static str,
) -> Result<Vec<AuthFile>> {
    let sentinel_rel = Path::new(cred_sentinel);
    let parent_rel = sentinel_rel.parent().ok_or_else(|| {
        PillboxError::runtime(
            action,
            format!("cred_sentinel `{cred_sentinel}` has no parent directory"),
        )
    })?;
    let parent_abs = home.join(parent_rel);
    if !parent_abs.exists() {
        // No login on this host yet — return empty; the receiving direct
        // dispatcher decides whether that's an error for its mode.
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    let mut total: u64 = 0;
    let entries = fs::read_dir(&parent_abs)
        .with_context(|| format!("read agent auth dir {}", parent_abs.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("walk {}", parent_abs.display()))?;
        // `DirEntry::metadata` is `lstat`-equivalent on Unix — does NOT
        // follow symlinks, so a symlink check skips them safely.
        let meta = entry
            .metadata()
            .with_context(|| format!("stat {}", entry.path().display()))?;
        let ty = meta.file_type();
        if ty.is_symlink() || !ty.is_file() {
            continue;
        }
        if meta.len() > AGENT_AUTH_MAX_FILE_BYTES {
            return Err(PillboxError::runtime(
                action,
                format!(
                    "agent auth file {} exceeds {} MiB",
                    entry.path().display(),
                    AGENT_AUTH_MAX_FILE_BYTES / (1024 * 1024)
                ),
            )
            .into());
        }
        total = total.saturating_add(meta.len());
        if total > AGENT_AUTH_MAX_BYTES {
            return Err(PillboxError::runtime(
                action,
                format!(
                    "agent auth dir {} exceeds {} MiB across forwarded files",
                    parent_abs.display(),
                    AGENT_AUTH_MAX_BYTES / (1024 * 1024)
                ),
            )
            .into());
        }
        // rel_path is relative to `home` (the agent's mounted HOME on the
        // receiver) so the sandbox writes back at the canonical location.
        let rel = parent_rel.join(entry.file_name());
        let rel_str = rel
            .to_str()
            .ok_or_else(|| {
                PillboxError::runtime(
                    action,
                    format!("non-UTF8 path in agent auth dir: {}", rel.display()),
                )
            })?
            .replace(std::path::MAIN_SEPARATOR, "/");
        let contents =
            fs::read(entry.path()).with_context(|| format!("read {}", entry.path().display()))?;
        // Don't widen perms on extract: 0o600 for every forwarded file —
        // a fresh sandbox HOME shouldn't carry group/world-readable agent
        // state even if the local file was loose.
        out.push(AuthFile {
            rel_path: rel_str,
            mode: 0o600,
            contents,
        });
    }
    Ok(out)
}

/// How the receiving sandbox gets its workspace — the one axis that differs
/// between the S3-hydrate backends and docker://.
pub(super) enum WorkspaceProvision {
    /// e2b/ssh: build the S3/rustic [`InlineWorkspace`] so the sandbox
    /// hydrates the base snapshot and pushes the result through the shared
    /// repo. Errors if the pillbox isn't on an S3-shaped backend.
    S3,
    /// docker://: the workspace is tar-cp'd into the container at this path;
    /// no S3, results pulled host-side. Carries the container workspace path.
    // Constructed by `RemoteDockerSandbox::run` (the next slice).
    #[allow(dead_code)]
    PreStaged { container_dir: String },
}

pub(super) fn build_vault_stdin_blob(
    spec: &AgentSpec,
    opts: &RunOpts,
    resolved: &Pillbox,
    action: &'static str,
    provision: WorkspaceProvision,
) -> Result<VaultStdinBlob> {
    // Resolve --with entries locally. Real secret values come from THIS
    // host's vault; only the resolved values cross the wire (once, into
    // the remote pillbox's vault session memory or the sandbox's tmp blob).
    let withs = crate::agents::resolve_with_entries(resolved, &opts.withs)?;
    if opts.vault && !spec.vault_capable {
        return Err(PillboxError::usage(
            action,
            format!("--vault is not supported for `{}`", spec.id),
        )
        .into());
    }
    // A vaulted `--with` secret drives the stub-swap proxy just like
    // `--vault`; reject it for agents that can't reach the proxy so the
    // stub never ships to the provider.
    let vaulted: Vec<&str> = withs
        .iter()
        .filter(|w| w.meta.is_some())
        .map(|w| w.secret_name.as_str())
        .collect();
    if !vaulted.is_empty() && !spec.vault_capable {
        return Err(PillboxError::usage(
            action,
            format!(
                "agent `{}` does not support the vault proxy, so it can't use vaulted secret(s): {}",
                spec.id,
                vaulted.join(", ")
            ),
        )
        .into());
    }

    let mut secrets = Vec::with_capacity(withs.len());
    for w in &withs {
        let real = crate::secrets::read(resolved, &w.secret_name)?.ok_or_else(|| {
            PillboxError::runtime(action, format!("secret `{}` not found", w.secret_name))
                .with_next(format!("pillbox secret add {}", w.secret_name))
        })?;
        secrets.push(InlineSecret {
            name: w.secret_name.clone(),
            env_var: w.env_var.clone(),
            value: real.trim_end().to_string(),
            vault_meta: w.meta.clone(),
        });
    }

    // Pre-resolve --env / --env-file the same way LocalDocker would, so
    // the remote doesn't have to know anything about the host's env
    // bundles or local files.
    let mut env = std::collections::BTreeMap::new();
    for bundle in &opts.env_bundles {
        let vars = crate::envs::read(resolved, bundle)?.ok_or_else(|| {
            PillboxError::runtime(action, format!("env bundle `{bundle}` not found"))
        })?;
        for (k, v) in vars {
            env.insert(k, v);
        }
    }
    for path in &opts.env_files {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            PillboxError::runtime(
                action,
                format!("could not read --env-file {}: {e}", path.display()),
            )
        })?;
        let vars = crate::envs::parse_dotenv(&raw, &path.display().to_string())?;
        for (k, v) in vars {
            env.insert(k, v);
        }
    }

    let workspace_host = match &opts.workspace {
        Some(p) => p.clone(),
        None => std::env::current_dir().context("resolve current working directory")?,
    };
    let workspace_name = workspace_mount_name(&workspace_host, opts.name.as_deref())?;
    // The S3 path snapshots cwd as the base (a write); the pre-staged path
    // does no S3 work at all — the host tar-cp's the cwd into the container.
    let (workspace, workspace_dir) = match provision {
        WorkspaceProvision::S3 => (
            Some(build_inline_workspace(
                resolved,
                opts,
                &workspace_host,
                action,
            )?),
            None,
        ),
        WorkspaceProvision::PreStaged { container_dir } => (None, Some(container_dir)),
    };
    // Forward the agent's local auth state (OAuth tokens, config) so a
    // sandbox without a pre-existing login (e.g. e2b) can rehydrate it
    // before exec. The Docker-shelled path ignores `agent_auth` and mounts
    // `home_dir` directly, so this is wasted bytes for SSH today — small
    // enough not to fight about, and unifying on the forward keeps a
    // future "ssh without pre-login" path open.
    let agent_auth = collect_agent_auth(&spec.home_dir(resolved)?, spec.cred_sentinel, action)?;

    Ok(VaultStdinBlob {
        version: BLOB_VERSION,
        agent_id: spec.id.to_string(),
        agent_args: opts.args.clone(),
        workspace_mount_name: workspace_name,
        vault: opts.vault,
        secrets,
        env,
        // Run context is filled in by the launcher after this
        // returns — separation keeps the builder pure (no
        // dependency on the run-id generator or per-launcher opts).
        context: crate::vault::RunContext::default(),
        workspace,
        workspace_dir,
        agent_auth,
    })
}

fn build_inline_workspace(
    resolved: &Pillbox,
    opts: &RunOpts,
    workspace_host: &Path,
    action: &'static str,
) -> Result<InlineWorkspace> {
    let backend = resolved.workspace()?;
    let s3 = match &backend.variant {
        RusticVariant::S3(cfg) => cfg.clone(),
        RusticVariant::Local { .. } => {
            return Err(PillboxError::usage(
                action,
                "remote runs require an S3-shaped workspace backend",
            )
            .into());
        }
    };
    let repo_password = fs::read_to_string(&backend.password_file)
        .with_context(|| format!("read {}", backend.password_file.display()))?
        .trim_end()
        .to_string();
    let base_snapshot = resolve_base_snapshot(resolved, opts, &backend, workspace_host)?;
    Ok(InlineWorkspace {
        s3,
        repo_password,
        base_snapshot,
    })
}

/// The snapshot a remote run forks from: an explicit `--from-bookmark`,
/// or — by default — a fresh snapshot of the current workspace pushed to
/// the shared repo. The default branch performs a `push` (a write), so
/// this is kept separate from the credential-gathering above.
fn resolve_base_snapshot(
    resolved: &Pillbox,
    opts: &RunOpts,
    backend: &RusticBackend,
    workspace_host: &Path,
) -> Result<Option<String>> {
    let handle = match opts.from_bookmark.as_deref() {
        Some(name) => crate::bookmarks::resolve_existing(resolved, name)?,
        None => {
            backend
                .push(
                    workspace_host,
                    PushOptions {
                        tag: Some("remote-base".into()),
                        message: Some("remote run base".into()),
                    },
                )?
                .handle
        }
    };
    Ok(Some(handle.as_str().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::HeaderScheme;

    fn test_workspace() -> InlineWorkspace {
        InlineWorkspace {
            s3: S3Config {
                endpoint: "https://r2.example".into(),
                region: "auto".into(),
                bucket: "bucket".into(),
                prefix: "pillbox/".into(),
                access_key: "access".into(),
                secret_key: "secret".into(),
            },
            repo_password: "repo-password".into(),
            base_snapshot: Some("a".repeat(64)),
        }
    }

    #[test]
    fn blob_round_trips_through_json() {
        let blob = VaultStdinBlob {
            version: BLOB_VERSION,
            agent_id: "claude".into(),
            agent_args: vec!["--continue".into()],
            workspace_mount_name: "my-app".into(),
            vault: true,
            secrets: vec![
                InlineSecret {
                    name: "PLAIN".into(),
                    env_var: "PLAIN".into(),
                    value: "raw-value".into(),
                    vault_meta: None,
                },
                InlineSecret {
                    name: "ANTHROPIC_API_KEY".into(),
                    env_var: "ANTHROPIC_API_KEY".into(),
                    value: "sk-ant-api03-REAL".into(),
                    vault_meta: Some(VaultMeta::new(
                        "api.anthropic.com".into(),
                        HeaderScheme::XApiKey,
                        "sk-ant-api03-".into(),
                    )),
                },
            ],
            env: [("LOG_LEVEL".to_string(), "debug".to_string())]
                .into_iter()
                .collect(),
            context: crate::vault::RunContext {
                session_id: Some("sess-abc123".into()),
                mode: Some("interactive".into()),
                workspace_id: Some("-test-workspace".into()),
            },
            workspace: Some(test_workspace()),
            workspace_dir: None,
            agent_auth: vec![AuthFile {
                rel_path: ".claude/.credentials.json".into(),
                mode: 0o600,
                contents: br#"{"oauth_token":"redacted"}"#.to_vec(),
            }],
        };
        let bytes = blob.to_bytes().unwrap();
        let back = VaultStdinBlob::from_bytes(&bytes).unwrap();
        assert_eq!(back.version, BLOB_VERSION);
        assert_eq!(back.agent_id, "claude");
        assert_eq!(back.agent_auth.len(), 1);
        assert_eq!(back.agent_auth[0].rel_path, ".claude/.credentials.json");
        assert_eq!(
            back.agent_auth[0].contents,
            br#"{"oauth_token":"redacted"}"#
        );
        assert_eq!(back.agent_args, vec!["--continue"]);
        assert_eq!(back.workspace_mount_name, "my-app");
        assert!(back.vault);
        assert_eq!(back.context.session_id.as_deref(), Some("sess-abc123"));
        assert_eq!(back.context.mode.as_deref(), Some("interactive"));
        assert_eq!(
            back.context.workspace_id.as_deref(),
            Some("-test-workspace"),
        );
        // Wire format stays flat (the three context fields appear
        // at the top level via `#[serde(flatten)]`). Pin that so a
        // future struct refactor can't silently break older host
        // binaries reading the JSON.
        let raw: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(raw["session_id"], "sess-abc123");
        assert_eq!(raw["mode"], "interactive");
        assert_eq!(raw["workspace_id"], "-test-workspace");
        assert_eq!(back.secrets.len(), 2);
        assert!(back.secrets[0].vault_meta.is_none());
        assert!(back.secrets[1].vault_meta.is_some());
        assert_eq!(back.env.get("LOG_LEVEL").map(String::as_str), Some("debug"));
        let ws = back.workspace.as_ref().expect("S3 workspace present");
        assert_eq!(ws.s3.bucket, "bucket");
        assert_eq!(ws.base_snapshot.as_deref().map(str::len), Some(64));
        assert!(
            back.workspace_dir.is_none(),
            "S3 blob has no pre-staged dir"
        );
    }

    /// The docker:// shape: a pre-staged `workspace_dir`, no S3 workspace.
    /// `skip_serializing_if` keeps the absent `workspace` off the wire.
    #[test]
    fn blob_round_trips_pre_staged_workspace() {
        let blob = VaultStdinBlob {
            version: BLOB_VERSION,
            agent_id: "claude".into(),
            agent_args: vec![],
            workspace_mount_name: "my-app".into(),
            vault: false,
            secrets: vec![],
            env: Default::default(),
            context: crate::vault::RunContext::default(),
            workspace: None,
            workspace_dir: Some("/workspace/my-app".into()),
            agent_auth: vec![],
        };
        let bytes = blob.to_bytes().unwrap();
        let raw: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            raw.get("workspace").is_none(),
            "absent S3 workspace is omitted"
        );
        assert_eq!(raw["workspace_dir"], "/workspace/my-app");
        let back = VaultStdinBlob::from_bytes(&bytes).unwrap();
        assert!(back.workspace.is_none());
        assert_eq!(back.workspace_dir.as_deref(), Some("/workspace/my-app"));
    }

    #[test]
    fn blob_rejects_invalid_json() {
        let err = VaultStdinBlob::from_bytes(b"not json").unwrap_err();
        assert!(format!("{err}").contains("vault-stdin"));
    }

    #[test]
    fn blob_ignores_unknown_fields() {
        // Forward compatibility: a newer pillbox writing extra fields
        // shouldn't break older deserialize paths.
        let raw = serde_json::json!({
            "version": BLOB_VERSION,
            "agent_id": "claude",
            "agent_args": [],
            "workspace_mount_name": "x",
            "vault": false,
            "secrets": [],
            "env": {},
            "workspace": test_workspace(),
            "future_field": {"nested": true},
        });
        let bytes = serde_json::to_vec(&raw).unwrap();
        let back = VaultStdinBlob::from_bytes(&bytes).unwrap();
        assert_eq!(back.agent_id, "claude");
    }

    #[test]
    fn blob_defaults_for_missing_optional_fields() {
        // A minimal blob (no secrets, no env, no agent_args) should
        // deserialize cleanly so trivial agent runs don't need to write
        // empty arrays/objects.
        let raw = serde_json::json!({
            "version": BLOB_VERSION,
            "agent_id": "codex",
            "workspace_mount_name": "p",
            "workspace": test_workspace(),
        });
        let bytes = serde_json::to_vec(&raw).unwrap();
        let back = VaultStdinBlob::from_bytes(&bytes).unwrap();
        assert_eq!(back.agent_id, "codex");
        assert!(back.secrets.is_empty());
        assert!(back.env.is_empty());
        assert!(back.agent_args.is_empty());
        assert!(!back.vault);
    }
}
