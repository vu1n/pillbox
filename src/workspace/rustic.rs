//! Rustic-backed workspace implementation.
//!
//! One [`RusticBackend`] = one rustic repository. Two storage variants:
//!
//! - [`RusticVariant::Local`] — repo lives at
//!   `~/.pillbox/projects/<key>/repo/`. Default for `pillbox new`.
//! - [`RusticVariant::S3`] — repo lives in a user-owned S3-compatible
//!   bucket addressed by `--endpoint` (R2, MinIO, Backblaze, native S3).
//!   The encryption password ALWAYS stays local at
//!   `~/.pillbox/projects/<key>/repo-password`, so a stolen bucket
//!   alone can't be decrypted.
//!
//! ## Why one file for ~600 LOC
//!
//! Everything in this module is "talk to `rustic_core`". Splitting it
//! by operation (`push.rs`, `pull.rs`, …) just spreads the same
//! `Repository::new → open → to_indexed_ids` boilerplate across files
//! and forces every caller to chase three jumps to read one flow.
//! Cohesion wins.

use std::{
    fmt, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use rand::{distr::Alphanumeric, Rng};
use rustic_backend::BackendOptions;
use rustic_core::{
    repofile::SnapshotFile, BackupOptions, ConfigOptions, Credentials, KeyOptions,
    LocalDestination, LsOptions, PathList, Repository, RepositoryOptions, RestoreOptions,
    SnapshotOptions,
};
use serde::{Deserialize, Serialize};

use super::{git_inflow, PushOptions, Snapshot, SnapshotHandle, WorkspaceBackend};
use crate::errors::PillboxError;
use crate::paths::write_private_file;

/// Filename of the per-pillbox password file inside the state dir.
/// 0600, local-only, never travels with the repo.
pub(crate) const PASSWORD_FILE: &str = "repo-password";

/// Filename of the local rustic repo directory inside the state dir.
/// Only meaningful for [`RusticVariant::Local`].
pub(crate) const REPO_DIR: &str = "repo";

/// Length of the auto-generated repository password (alphanumeric).
/// 32 chars * log2(62) ≈ 190 bits, comfortably above the 128-bit floor
/// needed to make brute-forcing rustic's key-derivation infeasible.
const PASSWORD_LEN: usize = 32;

/// The two backend variants pillbox supports in v0.6. Other rustic
/// backends (rest, rclone, SFTP) could plug in later; intentionally
/// scoped down for PR 3.
#[derive(Debug, Clone)]
pub(crate) enum RusticVariant {
    /// Rustic repository on the local filesystem. Default.
    Local { repo_path: PathBuf },
    /// Rustic repository in an S3-compatible bucket. `endpoint` is
    /// required so the same code path covers R2 / MinIO / Backblaze
    /// via opendal's S3 driver.
    S3(S3Config),
}

/// S3-compatible repository coordinates + resolved credentials.
///
/// `Serialize`/`Deserialize` so it can travel verbatim in the remote
/// run protocol blob (see `InlineWorkspace`); the same struct is the
/// repo config for the local backend, so no field shuffling is needed
/// across the boundary. `access_key`/`secret_key` are resolved values
/// (NOT env var names) — `Debug` redacts them.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct S3Config {
    pub(crate) endpoint: String,
    pub(crate) region: String,
    pub(crate) bucket: String,
    pub(crate) prefix: String,
    pub(crate) access_key: String,
    pub(crate) secret_key: String,
}

impl fmt::Debug for S3Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Config")
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("access_key", &"<redacted>")
            .field("secret_key", &"<redacted>")
            .finish()
    }
}

/// The configured backend for a single pillbox. Holds the variant + the
/// path to the local password file (always local, even for S3 — see
/// module docs).
#[derive(Debug, Clone)]
pub(crate) struct RusticBackend {
    pub(crate) variant: RusticVariant,
    /// Path to the per-pillbox password file (0600). The password
    /// itself is read lazily on every operation so a future `rekey`
    /// is observed without process restart.
    pub(crate) password_file: PathBuf,
}

impl RusticBackend {
    /// Idempotently initialize the rustic repo + password file for a
    /// freshly-created pillbox. Used by `pillbox new`.
    ///
    /// The password is generated once and written 0600. If
    /// `password_file` already exists (e.g. `pillbox new` was re-run
    /// after a partial setup), the existing value is reused.
    pub(crate) fn init_for_pillbox(&self) -> Result<()> {
        let password = self.ensure_password()?;

        // Local variant: pre-create the repo directory so rustic's
        // BackendOptions doesn't complain about a missing parent.
        if let RusticVariant::Local { repo_path } = &self.variant {
            fs::create_dir_all(repo_path)
                .with_context(|| format!("create rustic repo dir {}", repo_path.display()))?;
        }

        let backends = self.backends()?;
        let repo_opts = RepositoryOptions::default().no_cache(true);
        let repo =
            Repository::new(&repo_opts, &backends).map_err(|e| rustic_err("workspace init", e))?;
        // `config_id()` is a cheap backend HEAD on the repo config blob
        // (no scrypt). If the config already exists we treat init as a
        // no-op so `pillbox new` is idempotent on a half-set-up state
        // dir — without paying ~5s to derive a key just to find out.
        if repo
            .config_id()
            .map_err(|e| rustic_err("workspace init", e))?
            .is_some()
        {
            return Ok(());
        }
        let credentials = Credentials::password(&password);
        let key_opts = KeyOptions::default();
        let config_opts = ConfigOptions::default();
        repo.init(&credentials, &key_opts, &config_opts)
            .map_err(|e| rustic_err("workspace init", e))?;
        Ok(())
    }

    /// Read the password from the on-disk file, generating + persisting
    /// one if missing. Returns the password as a string so callers can
    /// hand it to `Credentials::password`.
    fn ensure_password(&self) -> Result<String> {
        if self.password_file.exists() {
            let s = fs::read_to_string(&self.password_file)
                .with_context(|| format!("read password file {}", self.password_file.display()))?;
            return Ok(s.trim_end().to_string());
        }
        if let Some(parent) = self.password_file.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        let pw = generate_password();
        write_password_file(&self.password_file, &pw)?;
        Ok(pw)
    }

    fn read_password(&self) -> Result<String> {
        if !self.password_file.exists() {
            return Err(PillboxError::config(
                "workspace",
                format!("no repository password at {}", self.password_file.display()),
            )
            .with_next("pillbox new  # re-create the pillbox")
            .into());
        }
        let s = fs::read_to_string(&self.password_file)
            .with_context(|| format!("read password file {}", self.password_file.display()))?;
        Ok(s.trim_end().to_string())
    }

    /// Build the [`rustic_backend::BackendOptions`] for this variant.
    fn backends(&self) -> Result<rustic_core::RepositoryBackends> {
        match &self.variant {
            RusticVariant::Local { repo_path } => {
                let opts = BackendOptions::default().repository(repo_path.display().to_string());
                opts.to_backends()
                    .map_err(|e| rustic_err("workspace backend", e).into())
            }
            RusticVariant::S3(cfg) => {
                // opendal's S3 driver wants flat key/value options; the
                // repository URL is `opendal:s3` (type=opendal, path=s3).
                let mut options = std::collections::BTreeMap::new();
                options.insert("endpoint".into(), cfg.endpoint.clone());
                options.insert("region".into(), cfg.region.clone());
                options.insert("bucket".into(), cfg.bucket.clone());
                if !cfg.prefix.is_empty() {
                    options.insert("root".into(), normalize_prefix(&cfg.prefix));
                }
                options.insert("access_key_id".into(), cfg.access_key.clone());
                options.insert("secret_access_key".into(), cfg.secret_key.clone());
                let opts = BackendOptions::default()
                    .repository("opendal:s3".to_string())
                    .options(options);
                opts.to_backends()
                    .map_err(|e| rustic_err("workspace backend", e).into())
            }
        }
    }

    fn open(&self) -> Result<rustic_core::Repository<rustic_core::OpenStatus>> {
        let password = self.read_password()?;
        let backends = self.backends()?;
        let repo_opts = RepositoryOptions::default().no_cache(true);
        let repo =
            Repository::new(&repo_opts, &backends).map_err(|e| rustic_err("workspace open", e))?;
        let opened = repo
            .open(&Credentials::password(&password))
            .map_err(|e| rustic_err("workspace open", e))?;
        Ok(opened)
    }

    /// Resolve a user-supplied snapshot prefix to a full ID. Opens the
    /// repo internally (one ~5s scrypt pass). Callers that already hold
    /// an opened repo should call [`resolve_in`] to avoid the second
    /// open.
    fn resolve_snapshot(&self, handle: &SnapshotHandle) -> Result<SnapshotFile> {
        let repo = self.open()?;
        resolve_in(&repo, handle)
    }

    /// The resolved S3 coordinates + creds for an S3-backed pillbox, or
    /// `None` for the local-filesystem variant. Lets a caller (the managed
    /// backend's container-native placement) hand the same repo config to a
    /// remote restorer without re-plumbing the meta.json → env resolution.
    /// The returned creds are the resolved values, so a caller must treat
    /// this as secret material — never log or persist it.
    pub(crate) fn s3_config(&self) -> Option<&S3Config> {
        match &self.variant {
            RusticVariant::S3(cfg) => Some(cfg),
            RusticVariant::Local { .. } => None,
        }
    }

    /// The repo encryption password, read from the local 0600 file. The
    /// password ALWAYS stays local (it never travels with the repo), so this
    /// is the single read-back point for a caller that must hand it to a
    /// trusted restorer over a confidential channel. Secret material — never
    /// log or persist it. Errors (with a `Next:`) if the file is missing.
    pub(crate) fn resolved_password(&self) -> Result<String> {
        self.read_password()
    }
}

/// Resolve a handle (prefix or full ID) against an already-opened repo.
/// Mirrors `git`'s prefix-resolution UX: shortest unique prefix wins;
/// ambiguity is an error.
fn resolve_in(
    repo: &rustic_core::Repository<rustic_core::OpenStatus>,
    handle: &SnapshotHandle,
) -> Result<SnapshotFile> {
    let all = repo
        .get_all_snapshots()
        .map_err(|e| rustic_err("snapshot lookup", e))?;
    let needle = handle.as_str();
    let matches: Vec<&SnapshotFile> = all
        .iter()
        .filter(|s| s.id.to_hex().as_str().starts_with(needle))
        .collect();
    match matches.as_slice() {
        [] => Err(PillboxError::runtime(
            "snapshot lookup",
            format!("no snapshot matches `{needle}`"),
        )
        .with_next("pillbox snapshot list  # see available handles")
        .into()),
        [one] => Ok((*one).clone()),
        many => {
            let preview = many
                .iter()
                .take(5)
                .map(|s| s.id.to_hex().as_str()[..12].to_string())
                .collect::<Vec<_>>()
                .join(", ");
            Err(PillboxError::usage(
                "snapshot lookup",
                format!("`{needle}` is ambiguous (matches: {preview}, …)"),
            )
            .into())
        }
    }
}

impl WorkspaceBackend for RusticBackend {
    fn push(&self, cwd: &Path, opts: PushOptions) -> Result<Snapshot> {
        // Reject sentinel-laden messages *before* paying the cost of
        // opening the repo (~5s scrypt). The check itself prevents
        // forged trailer metadata; the early position prevents the
        // user from waiting 5 seconds just to learn their message
        // is invalid.
        if let Some(m) = opts.message.as_deref() {
            validate_user_message("workspace push", m)?;
        }
        let repo = self
            .open()?
            .to_indexed_ids()
            .map_err(|e| rustic_err("workspace push", e))?;

        let cwd_str = cwd_as_str("workspace push", cwd)?;
        let source = PathList::from_string(cwd_str)
            .map_err(|e| rustic_err("workspace push", e))?
            .sanitize()
            .map_err(|e| rustic_err("workspace push", e))?;

        // Git anchor — auto-filled if cwd happens to be a git working
        // tree. Failures are swallowed: a workspace doesn't have to be
        // a git repo for push to work.
        let (git_anchor, git_dirty) = git_inflow::resolve_git_anchor(cwd).unwrap_or((None, false));

        let mut snap_opts = SnapshotOptions::default();
        // Build the tag list. We accept a single tag (the CLI takes
        // `--tag NAME` once); rustic StringList stores multiple but
        // pillbox surfaces only the first in the Snapshot record.
        if let Some(tag) = opts.tag.as_deref() {
            snap_opts = snap_opts
                .add_tags(tag)
                .map_err(|e| rustic_err("workspace push", e))?;
        }
        // Encode message + git anchor + dirty bit into rustic's
        // description field (a free-form blob). `parse_description`
        // round-trips this on read so `snapshot show` reports the
        // same metadata.
        if let Some(desc) =
            encode_description(opts.message.as_deref(), git_anchor.as_deref(), git_dirty)
        {
            snap_opts = snap_opts.description(desc);
        }
        // Pin the host name to a stable value so snapshots taken from
        // different machines aren't grouped separately by rustic's
        // "group by host" default. (Pillbox treats one repo as one
        // logical workspace; the human running rustic against the same
        // repo from elsewhere can still see hostname via `snapshot
        // show`.)
        snap_opts = snap_opts.host("pillbox".to_string());

        let snap = snap_opts
            .to_snapshot()
            .map_err(|e| rustic_err("workspace push", e))?;

        let mut backup_opts = BackupOptions::default();
        // Store the workspace as a relative tree instead of baking the
        // host's absolute cwd into the snapshot. Older snapshots may still
        // carry absolute paths, but new pushes can be restored directly into
        // a remote mount directory without creating `/tmp/...` prefixes.
        backup_opts.as_path = Some(PathBuf::from("."));
        let snap = repo
            .backup(&backup_opts, &source, snap)
            .map_err(|e| rustic_err("workspace push", e))?;

        Ok(snapshot_to_record(&snap, git_anchor, git_dirty))
    }

    fn pull(&self, cwd: &Path, snapshot: Option<&SnapshotHandle>) -> Result<()> {
        // Open once, resolve the handle on the open repo, THEN convert
        // to indexed for the restore. Going via `resolve_snapshot`
        // would open a second time and pay scrypt twice.
        let open = self.open()?;
        let snap = match snapshot {
            Some(h) => resolve_in(&open, h)?,
            None => {
                let mut snaps = open
                    .get_all_snapshots()
                    .map_err(|e| rustic_err("workspace pull", e))?;
                snaps.sort_by(|a, b| a.time.cmp(&b.time));
                snaps.pop().ok_or_else(|| {
                    PillboxError::runtime(
                        "workspace pull",
                        "no snapshots exist yet; run `pillbox push` first",
                    )
                })?
            }
        };
        let snap_spec = restore_spec_for_snapshot(&snap);
        let repo = open
            .to_indexed()
            .map_err(|e| rustic_err("workspace pull", e))?;

        let node = repo
            .node_from_snapshot_path(&snap_spec, |_| true)
            .map_err(|e| rustic_err("workspace pull", e))?;
        let streamer_opts = LsOptions::default();
        let ls = repo
            .ls(&node, &streamer_opts)
            .map_err(|e| rustic_err("workspace pull", e))?;

        let dest = LocalDestination::new(cwd_as_str("workspace pull", cwd)?, true, !node.is_dir())
            .map_err(|e| rustic_err("workspace pull", e))?;

        let restore_opts = RestoreOptions::default();
        let plan = repo
            .prepare_restore(&restore_opts, ls.clone(), &dest, false)
            .map_err(|e| rustic_err("workspace pull", e))?;
        repo.restore(plan, &restore_opts, ls, &dest)
            .map_err(|e| rustic_err("workspace pull", e))?;
        Ok(())
    }

    fn snapshots(&self) -> Result<Vec<Snapshot>> {
        let repo = self.open()?;
        let mut snaps = repo
            .get_all_snapshots()
            .map_err(|e| rustic_err("snapshot list", e))?;
        snaps.sort_by(|a, b| a.time.cmp(&b.time));
        Ok(snaps
            .into_iter()
            .map(|s| snapshot_to_record(&s, None, false))
            .collect())
    }

    fn snapshot_show(&self, handle: &SnapshotHandle) -> Result<Snapshot> {
        let snap = self.resolve_snapshot(handle)?;
        Ok(snapshot_to_record(&snap, None, false))
    }

    fn snapshot_rm(&self, handle: &SnapshotHandle) -> Result<()> {
        // Open once: resolve + delete on the same opened repo so we pay
        // one scrypt pass, not two.
        let repo = self.open()?;
        let snap = resolve_in(&repo, handle)?;
        repo.delete_snapshots(&[snap.id])
            .map_err(|e| rustic_err("snapshot rm", e))?;
        Ok(())
    }

    fn rekey(&self) -> Result<()> {
        // Add a new key with a fresh password, persist it, then remove
        // the old one — operations using the new password file will
        // open with the new key without any other change.
        //
        // We hold the *old* password to authenticate the add_key call,
        // then atomically swap the on-disk file.
        let old = self.read_password()?;
        let backends = self.backends()?;
        let repo_opts = RepositoryOptions::default().no_cache(true);
        let repo = Repository::new(&repo_opts, &backends)
            .map_err(|e| rustic_err("workspace rekey", e))?
            .open(&Credentials::password(&old))
            .map_err(|e| rustic_err("workspace rekey", e))?;
        let new = generate_password();
        let key_opts = KeyOptions::default();
        repo.add_key(&new, &key_opts)
            .map_err(|e| rustic_err("workspace rekey", e))?;
        write_password_file(&self.password_file, &new)?;
        // NOTE: rustic_core 0.11 exposes `add_key` but not a stable
        // single-call "remove old key by password". The old key
        // remains valid until a future `rustic_core::delete_key` lands
        // in the public surface — tracked for PR 4. The new password
        // is now the authoritative one because pillbox itself only
        // reads from `password_file`.
        Ok(())
    }
}

// ── helpers ────────────────────────────────────────────────────────────────

fn restore_spec_for_snapshot(snap: &SnapshotFile) -> String {
    let id = snap.id.to_hex().as_str().to_string();
    let Some(path) = snap.paths.iter().next() else {
        return id;
    };
    if path == "." || path == "/" || path.is_empty() {
        id
    } else {
        format!("{id}:{path}")
    }
}

/// Borrow `cwd` as a UTF-8 string for rustic's `PathList` /
/// `LocalDestination` APIs (both want `&str`). On non-UTF-8 cwd —
/// vanishingly rare in practice — surface a clean pillbox usage error
/// labeled with the calling action.
fn cwd_as_str<'a>(action: &'static str, cwd: &'a Path) -> Result<&'a str> {
    cwd.to_str().ok_or_else(|| {
        PillboxError::usage(
            action,
            format!("cwd `{}` is not valid UTF-8", cwd.display()),
        )
        .into()
    })
}

/// Persist the rustic repo password at `path` with 0600 perms and a
/// trailing newline (so `cat repo-password` from a shell looks sane).
/// Wraps [`crate::paths::write_private_file`] so the on-disk perms
/// invariant stays in one spot.
fn write_password_file(path: &Path, body: &str) -> Result<()> {
    let mut out = Vec::with_capacity(body.len() + 1);
    out.extend_from_slice(body.as_bytes());
    out.push(b'\n');
    write_private_file(path, &out)
}

fn generate_password() -> String {
    let mut rng = rand::rng();
    (0..PASSWORD_LEN)
        .map(|_| char::from(rng.sample(Alphanumeric)))
        .collect()
}

/// Normalize an S3 prefix so it ends in `/` and doesn't start with one.
/// opendal's S3 driver uses `root` as a path prefix; the join semantics
/// are easier to reason about with a trailing slash.
fn normalize_prefix(p: &str) -> String {
    let trimmed = p.trim_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        format!("/{trimmed}/")
    }
}

/// Project a rustic `SnapshotFile` into the pillbox-public [`Snapshot`].
/// `git_anchor` / `git_dirty` are passed in by the caller because
/// rustic doesn't track them — pillbox carries them in the snapshot
/// description (see [`encode_description`]) and re-parses on read.
fn snapshot_to_record(
    snap: &SnapshotFile,
    extra_git_anchor: Option<String>,
    extra_git_dirty: bool,
) -> Snapshot {
    let handle = SnapshotHandle::new(snap.id.to_hex().as_str().to_string());
    let created_at = zoned_to_rfc3339(&snap.time);
    let tag = snap
        .tags
        .iter()
        .next()
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty());
    let (message, parsed_git_anchor, parsed_git_dirty) =
        parse_description(snap.description.as_deref());
    // If the caller passed git info explicitly (push path), prefer
    // that. Otherwise, fall back to what we encoded in the
    // description.
    let git_anchor = extra_git_anchor.or(parsed_git_anchor);
    let git_dirty = if extra_git_dirty {
        true
    } else {
        parsed_git_dirty
    };
    let summary = snap.summary.as_ref();
    let bytes = summary.map(|s| s.total_bytes_processed).unwrap_or(0);
    // rustic only writes the summary on the run that produces the
    // snapshot. Snapshots loaded back from the repo (by `snapshots()` /
    // `snapshot_show`) report zeros here — that's expected, not a bug.
    let files_new = summary.map(|s| s.files_new).unwrap_or(0);
    let files_changed = summary.map(|s| s.files_changed).unwrap_or(0);
    let files_total = summary.map(|s| s.total_files_processed).unwrap_or(0);
    Snapshot {
        handle,
        created_at,
        tag,
        message,
        git_anchor,
        git_dirty,
        bytes,
        files_new,
        files_changed,
        files_total,
    }
}

/// Sentinels that bracket the pillbox-managed trailer inside rustic's
/// description field. The encoder writes exactly one block between
/// `BEGIN` and `END`; the parser only reads `pillbox-…:` keys inside
/// that block. Anything outside is treated as opaque user text — even
/// if a malicious `--message` includes lines that *look* like
/// `pillbox-git-anchor: …`. See [`encode_description`].
const TRAILER_BEGIN: &str = "-----BEGIN PILLBOX METADATA-----";
const TRAILER_END: &str = "-----END PILLBOX METADATA-----";

/// Encode the pillbox-side metadata (message, git anchor, dirty bit)
/// into rustic's free-form description field. The user's message is
/// written verbatim, followed by a sentinel-bracketed trailer that
/// only the parser reads:
///
/// ```text
/// <message lines...>
///
/// -----BEGIN PILLBOX METADATA-----
/// pillbox-git-anchor: <sha>
/// pillbox-git-dirty: true|false
/// -----END PILLBOX METADATA-----
/// ```
///
/// To prevent metadata injection, any user message that itself
/// contains a sentinel line is rejected at the boundary — see
/// [`validate_user_message`]. Round-trips through [`parse_description`].
fn encode_description(
    message: Option<&str>,
    git_anchor: Option<&str>,
    git_dirty: bool,
) -> Option<String> {
    if message.is_none() && git_anchor.is_none() && !git_dirty {
        return None;
    }
    let mut out = String::new();
    if let Some(m) = message {
        out.push_str(m);
    }
    if git_anchor.is_some() || git_dirty {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(TRAILER_BEGIN);
        out.push('\n');
        if let Some(a) = git_anchor {
            out.push_str(&format!("pillbox-git-anchor: {a}\n"));
        }
        out.push_str(&format!("pillbox-git-dirty: {git_dirty}\n"));
        out.push_str(TRAILER_END);
        out.push('\n');
    }
    Some(out)
}

/// Reject user-supplied messages that try to forge pillbox metadata.
/// The check is intentionally strict — any line equal to one of the
/// sentinels short-circuits with a usage error so the user sees the
/// reason rather than silently mangled metadata downstream.
pub(crate) fn validate_user_message(action: &'static str, msg: &str) -> Result<()> {
    for line in msg.lines() {
        if line == TRAILER_BEGIN || line == TRAILER_END {
            return Err(PillboxError::usage(
                action,
                "message cannot contain pillbox metadata sentinels",
            )
            .into());
        }
    }
    Ok(())
}

fn parse_description(s: Option<&str>) -> (Option<String>, Option<String>, bool) {
    let Some(s) = s else {
        return (None, None, false);
    };
    // Locate the sentinel-bracketed trailer (if any). Everything
    // outside is the user message; only the block between the
    // sentinels yields `pillbox-…:` keys. Falling back to "no
    // trailer" on an unmatched END means a corrupted description
    // surfaces as message-only, not as forged metadata.
    let (message_part, trailer_part) = match (s.find(TRAILER_BEGIN), s.find(TRAILER_END)) {
        (Some(b), Some(e)) if e > b => {
            let msg = &s[..b];
            let inner_start = b + TRAILER_BEGIN.len();
            let trailer = &s[inner_start..e];
            (msg, Some(trailer))
        }
        _ => (s, None),
    };
    let mut git_anchor: Option<String> = None;
    let mut git_dirty = false;
    if let Some(trailer) = trailer_part {
        for line in trailer.lines() {
            if let Some(rest) = line.strip_prefix("pillbox-git-anchor: ") {
                git_anchor = Some(rest.to_string());
                continue;
            }
            if let Some(rest) = line.strip_prefix("pillbox-git-dirty: ") {
                git_dirty = matches!(rest.trim(), "true" | "True" | "1");
            }
        }
    }
    let message = {
        let trimmed = message_part.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    };
    (message, git_anchor, git_dirty)
}

/// Rustic stores time as `jiff::Zoned`, whose `Display` is RFC3339
/// **plus** an `[IANA/zone]` suffix that breaks strict consumers. Strip
/// the suffix so JSON output stays parseable by everything that handles
/// RFC3339.
fn zoned_to_rfc3339(z: &jiff::Zoned) -> String {
    let s = z.to_string();
    match s.find('[') {
        Some(i) => s[..i].to_string(),
        None => s,
    }
}

/// Wrap a `rustic_core::Error` into a [`PillboxError`] with our
/// standard "action failed: reason" format. Display includes the full
/// error chain so the user can still see what rustic complained about.
fn rustic_err<E: std::fmt::Display>(action: &'static str, e: E) -> PillboxError {
    PillboxError::runtime(action, format!("rustic_core: {e}"))
}

// `FromStr` re-export so tests can build a `SnapshotHandle` via the
// rustic id type without depending on rustic_core directly.
#[cfg(test)]
pub(crate) fn parse_full_handle(s: &str) -> Result<SnapshotHandle> {
    use rustic_core::repofile::SnapshotId;
    use std::str::FromStr;
    SnapshotId::from_str(s).map_err(|e| {
        PillboxError::usage("snapshot lookup", format!("invalid handle `{s}`: {e}"))
    })?;
    Ok(SnapshotHandle::new(s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Build a [`RusticBackend`] pointed at a fresh tempdir + an
    /// initialized rustic repo. Returns the backend, the password
    /// file path, and the tempdir guard.
    ///
    /// Each call is ~5s due to scrypt key derivation — use sparingly.
    fn fresh_local_backend() -> (RusticBackend, TempDir) {
        let dir = TempDir::new().unwrap();
        let backend = RusticBackend {
            variant: RusticVariant::Local {
                repo_path: dir.path().join("repo"),
            },
            password_file: dir.path().join("repo-password"),
        };
        backend.init_for_pillbox().unwrap();
        (backend, dir)
    }

    #[test]
    fn init_creates_repo_config_file() {
        let (_, dir) = fresh_local_backend();
        // rustic writes a `config` blob at the root of the repo on init.
        let cfg = dir.path().join("repo/config");
        assert!(cfg.exists(), "expected {} to exist", cfg.display());
        // password file is 0600 + has 32 alphanumeric chars + newline.
        let pw = dir.path().join("repo-password");
        let body = fs::read_to_string(&pw).unwrap();
        let trimmed = body.trim_end_matches('\n');
        assert_eq!(trimmed.len(), PASSWORD_LEN);
        assert!(trimmed.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn init_for_pillbox_is_idempotent() {
        let (backend, _dir) = fresh_local_backend();
        // Second call must not error.
        backend.init_for_pillbox().unwrap();
    }

    #[test]
    fn push_then_snapshots_returns_one_entry() {
        let (backend, dir) = fresh_local_backend();
        // Make a workspace dir with a file.
        let ws = dir.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        fs::write(ws.join("hello.txt"), b"hello world").unwrap();

        let snap = backend.push(&ws, PushOptions::default()).unwrap();
        assert_eq!(snap.handle.as_str().len(), 64);
        assert!(snap.bytes > 0);

        let all = backend.snapshots().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].handle, snap.handle);
    }

    #[test]
    fn push_preserves_tag_and_message() {
        let (backend, dir) = fresh_local_backend();
        let ws = dir.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        fs::write(ws.join("a"), b"x").unwrap();

        let snap = backend
            .push(
                &ws,
                PushOptions {
                    tag: Some("v1".into()),
                    message: Some("first cut".into()),
                },
            )
            .unwrap();
        assert_eq!(snap.tag.as_deref(), Some("v1"));
        assert_eq!(snap.message.as_deref(), Some("first cut"));

        // Re-read via snapshot_show to prove the description trailer
        // round-trips through rustic.
        let shown = backend.snapshot_show(&snap.handle).unwrap();
        assert_eq!(shown.tag.as_deref(), Some("v1"));
        assert_eq!(shown.message.as_deref(), Some("first cut"));
    }

    #[test]
    fn push_from_git_repo_fills_anchor_and_dirty() {
        // Build a tiny git repo as the workspace.
        let (backend, dir) = fresh_local_backend();
        let ws = dir.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&ws)
                .args(args)
                .output()
                .unwrap()
        };
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return; // CI without git → skip
        }
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@example"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        fs::write(ws.join("a.txt"), b"x").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
        // Dirty it.
        fs::write(ws.join("a.txt"), b"y").unwrap();

        let snap = backend.push(&ws, PushOptions::default()).unwrap();
        assert!(snap.git_anchor.is_some(), "expected git anchor");
        assert_eq!(snap.git_anchor.as_ref().unwrap().len(), 40);
        assert!(snap.git_dirty);

        // Verify it round-trips on read.
        let shown = backend.snapshot_show(&snap.handle).unwrap();
        assert_eq!(shown.git_anchor, snap.git_anchor);
        assert!(shown.git_dirty);
    }

    #[test]
    fn pull_restores_files_to_a_fresh_dir() {
        let (backend, dir) = fresh_local_backend();
        let ws = dir.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        fs::write(ws.join("a.txt"), b"hello").unwrap();
        fs::write(ws.join("b.txt"), b"world").unwrap();

        let _ = backend.push(&ws, PushOptions::default()).unwrap();
        // Pull into a fresh dir.
        let restore = dir.path().join("restore");
        fs::create_dir_all(&restore).unwrap();
        backend.pull(&restore, None).unwrap();

        // The restored tree mirrors the original workspace under its
        // absolute path. rustic stores the absolute source path, so
        // restoring into `restore/` places the files at
        // `restore/<absolute-source-path>/a.txt`. Locate them by walk.
        let mut found_a = false;
        let mut found_b = false;
        for entry in walkdir(&restore) {
            let name = entry
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            if name == "a.txt" {
                found_a = true;
            }
            if name == "b.txt" {
                found_b = true;
            }
        }
        assert!(
            found_a && found_b,
            "restored files not found under {}",
            restore.display()
        );
    }

    #[test]
    fn pull_specific_snapshot_restores_that_one() {
        let (backend, dir) = fresh_local_backend();
        let ws = dir.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        fs::write(ws.join("a.txt"), b"v1").unwrap();
        let first = backend.push(&ws, PushOptions::default()).unwrap();
        // Make sure the second snapshot has a different timestamp by
        // touching a different file content.
        fs::write(ws.join("a.txt"), b"v2 longer content").unwrap();
        let _second = backend.push(&ws, PushOptions::default()).unwrap();

        let restore = dir.path().join("restore-v1");
        fs::create_dir_all(&restore).unwrap();
        backend.pull(&restore, Some(&first.handle)).unwrap();

        // The restored a.txt must match the first version.
        let mut found = None;
        for entry in walkdir(&restore) {
            if entry.file_name().unwrap_or_default() == "a.txt" {
                found = Some(entry);
                break;
            }
        }
        let body = fs::read(found.expect("a.txt restored")).unwrap();
        assert_eq!(body, b"v1");
    }

    #[test]
    fn snapshot_rm_removes_entry() {
        let (backend, dir) = fresh_local_backend();
        let ws = dir.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        fs::write(ws.join("a"), b"x").unwrap();

        let snap = backend.push(&ws, PushOptions::default()).unwrap();
        assert_eq!(backend.snapshots().unwrap().len(), 1);
        backend.snapshot_rm(&snap.handle).unwrap();
        assert_eq!(backend.snapshots().unwrap().len(), 0);
    }

    #[test]
    fn rekey_changes_password_and_keeps_repo_readable() {
        let (backend, dir) = fresh_local_backend();
        let ws = dir.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        fs::write(ws.join("a"), b"x").unwrap();
        let _ = backend.push(&ws, PushOptions::default()).unwrap();

        let pw_before = fs::read_to_string(dir.path().join("repo-password")).unwrap();
        backend.rekey().unwrap();
        let pw_after = fs::read_to_string(dir.path().join("repo-password")).unwrap();
        assert_ne!(pw_before, pw_after, "password should have changed");
        // Repo still readable.
        let snaps = backend.snapshots().unwrap();
        assert_eq!(snaps.len(), 1);
    }

    #[test]
    fn snapshot_show_prefix_resolution() {
        let (backend, dir) = fresh_local_backend();
        let ws = dir.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        fs::write(ws.join("a"), b"x").unwrap();
        let snap = backend.push(&ws, PushOptions::default()).unwrap();

        // 8-char prefix must resolve.
        let prefix = SnapshotHandle::new(snap.handle.short().to_string());
        let shown = backend.snapshot_show(&prefix).unwrap();
        assert_eq!(shown.handle, snap.handle);
    }

    fn walkdir(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(p) = stack.pop() {
            let Ok(rd) = fs::read_dir(&p) else { continue };
            for e in rd.flatten() {
                let path = e.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    out.push(path);
                }
            }
        }
        out
    }

    #[test]
    fn generate_password_is_alphanumeric() {
        let pw = generate_password();
        assert_eq!(pw.len(), PASSWORD_LEN);
        assert!(pw.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn normalize_prefix_handles_edges() {
        assert_eq!(normalize_prefix(""), "/");
        assert_eq!(normalize_prefix("/"), "/");
        assert_eq!(normalize_prefix("foo"), "/foo/");
        assert_eq!(normalize_prefix("/foo/"), "/foo/");
        assert_eq!(normalize_prefix("/foo/bar"), "/foo/bar/");
    }

    #[test]
    fn encode_decode_description_roundtrip() {
        // No metadata → None.
        assert!(encode_description(None, None, false).is_none());

        // Message only.
        let s = encode_description(Some("hello"), None, false).unwrap();
        let (m, a, d) = parse_description(Some(&s));
        assert_eq!(m.as_deref(), Some("hello"));
        assert!(a.is_none());
        assert!(!d);

        // Git anchor + dirty.
        let s = encode_description(Some("msg"), Some("abc123"), true).unwrap();
        let (m, a, d) = parse_description(Some(&s));
        assert_eq!(m.as_deref(), Some("msg"));
        assert_eq!(a.as_deref(), Some("abc123"));
        assert!(d);

        // Just git anchor.
        let s = encode_description(None, Some("def456"), false).unwrap();
        let (m, a, d) = parse_description(Some(&s));
        assert!(m.is_none());
        assert_eq!(a.as_deref(), Some("def456"));
        assert!(!d);
    }

    #[test]
    fn zoned_to_rfc3339_strips_bracketed_zone() {
        let z: jiff::Zoned = "2026-05-19T12:00:00Z[UTC]".parse().unwrap();
        let s = zoned_to_rfc3339(&z);
        assert!(!s.contains('['), "got: {s}");
        assert!(s.starts_with("2026-05-19T"), "got: {s}");
    }

    #[test]
    fn snapshot_handle_short_is_8_chars() {
        let h = SnapshotHandle::new("0123456789abcdef0123456789abcdef");
        assert_eq!(h.short(), "01234567");
    }

    #[test]
    fn snapshot_handle_short_is_clamped_for_shorter_input() {
        let h = SnapshotHandle::new("abc");
        assert_eq!(h.short(), "abc");
    }

    #[test]
    fn parse_full_handle_accepts_valid_hex() {
        let id = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let h = parse_full_handle(id).unwrap();
        assert_eq!(h.as_str(), id);
    }

    #[test]
    fn parse_description_treats_trailer_keys_in_message_as_literal() {
        // Injection vector: a user message that *contains* the
        // trailer-shaped lines, but outside the sentinel block. The
        // parser must NOT interpret these as real metadata.
        let forged = "hello\npillbox-git-anchor: deadbeef\npillbox-git-dirty: true";
        let (msg, anchor, dirty) = parse_description(Some(forged));
        assert_eq!(msg.as_deref(), Some(forged.trim()));
        assert!(anchor.is_none(), "anchor must not be forged from message");
        assert!(!dirty, "dirty must not be forged from message");
    }

    #[test]
    fn validate_user_message_rejects_sentinels() {
        // Plain text — fine.
        validate_user_message("test", "hello world").unwrap();
        // Looks-like-trailer-but-outside-sentinels — fine; encoder
        // wraps it as opaque text.
        validate_user_message("test", "pillbox-git-anchor: x").unwrap();
        // Actual sentinel line — rejected.
        let err =
            validate_user_message("test", &format!("oops\n{TRAILER_BEGIN}\nfoo")).unwrap_err();
        assert!(err.to_string().contains("sentinel"), "got: {err}");
        let err = validate_user_message("test", &format!("oops\n{TRAILER_END}\nfoo")).unwrap_err();
        assert!(err.to_string().contains("sentinel"), "got: {err}");
    }

    #[test]
    fn encode_with_quoted_trailer_in_message_roundtrips_safely() {
        // Message contains the *literal text* of a fake trailer key —
        // encode + parse must return it as the message, with no
        // forged anchor.
        let user_msg = "release notes:\npillbox-git-anchor: not-real";
        let encoded = encode_description(Some(user_msg), Some("realsha"), true).unwrap();
        let (m, a, d) = parse_description(Some(&encoded));
        assert_eq!(m.as_deref(), Some(user_msg));
        assert_eq!(a.as_deref(), Some("realsha"));
        assert!(d);
    }
}
