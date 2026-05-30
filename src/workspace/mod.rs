//! Workspace backends — content-addressed, versioned snapshots of the
//! files the agent edits.
//!
//! v0.6 PR 3 makes workspaces a first-class entity. Each pillbox owns
//! exactly one [`WorkspaceBackend`]: either a local on-disk rustic
//! repository or an S3-shaped one (R2/MinIO/Backblaze) addressed by
//! `--endpoint`. The trait abstracts over the storage location; the
//! concrete implementation in [`rustic`] does the heavy lifting via the
//! `rustic_core` crate.
//!
//! Git is an **inflow at creation time** (`pillbox new --from-git URL`),
//! not a storage backend. Once a workspace exists, rustic owns
//! versioning end-to-end; the snapshot metadata carries the git anchor
//! SHA so the user can correlate. See [`git_inflow`].
//!
//! ## Lifecycle
//!
//! 1. `pillbox new` runs [`rustic::RusticBackend::init_for_pillbox`],
//!    generating an encryption password (stored 0600 locally), writing
//!    `repo-password` next to the pillbox state dir, and initializing
//!    the rustic repository.
//! 2. `pillbox push` runs [`WorkspaceBackend::push`] over the cwd. The
//!    backend auto-fills `git_anchor` + `git_dirty` if cwd is a git
//!    working tree.
//! 3. `pillbox pull` restores cwd from the latest snapshot, or a
//!    specific handle when `--snapshot HANDLE` is passed.
//! 4. `pillbox snapshot list/show/rm` and `pillbox workspace rekey`
//!    round out the surface.
//!
//! The trait deliberately keeps remote-sandbox provisioning out — that
//! lands in PR 4 (RemoteSsh) once we know what shape the SSH transport
//! wants. For PR 3 every backend operates on a host cwd.

pub(crate) mod git_inflow;
pub(crate) mod ingest;
pub(crate) mod rustic;

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// A snapshot identifier. Rustic IDs are 32-byte BLAKE2b hashes
/// rendered as 64-character hex strings. We accept user-facing prefixes
/// (≥ 4 chars) and resolve to the unique full ID at lookup time, the
/// same UX as `git rev-parse`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SnapshotHandle(pub(crate) String);

impl SnapshotHandle {
    pub(crate) fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// First 8 hex characters — what we render in human-facing output
    /// (mirrors git's `--abbrev`). For machine-readable output use the
    /// full ID via [`Self::as_str`].
    pub(crate) fn short(&self) -> &str {
        let n = self.0.len().min(8);
        &self.0[..n]
    }
}

impl std::fmt::Display for SnapshotHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Options for [`WorkspaceBackend::push`].
///
/// `git_anchor` / `git_dirty` are filled in by the backend itself when
/// cwd happens to be a git working tree (see
/// [`git_inflow::resolve_git_anchor`]). Callers don't pass them; they
/// surface here on the way back out via [`Snapshot::git_anchor`] /
/// [`Snapshot::git_dirty`] so JSON consumers and humans can correlate
/// a rustic snapshot with the commit it was taken at.
#[derive(Debug, Clone, Default)]
pub(crate) struct PushOptions {
    pub(crate) tag: Option<String>,
    pub(crate) message: Option<String>,
}

/// One snapshot record returned by [`WorkspaceBackend::push`] /
/// [`WorkspaceBackend::snapshots`] / [`WorkspaceBackend::snapshot_show`].
///
/// `bytes` is the **total data processed** for the underlying backup
/// run as reported by rustic's snapshot summary — not the deduplicated
/// store size. Informational; deduplication numbers live in rustic
/// itself.
///
/// `files_new` / `files_changed` / `files_total` come from the same
/// rustic summary. On a snapshot loaded back from the repo, rustic
/// doesn't repopulate these (the summary only fires for the run that
/// produced the snapshot), so reads from `snapshots()` / `snapshot_show`
/// may report zeros. They're populated on the `push` return value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    pub(crate) handle: SnapshotHandle,
    pub(crate) created_at: String,
    pub(crate) tag: Option<String>,
    pub(crate) message: Option<String>,
    pub(crate) git_anchor: Option<String>,
    pub(crate) git_dirty: bool,
    pub(crate) bytes: u64,
    pub(crate) files_new: u64,
    pub(crate) files_changed: u64,
    pub(crate) files_total: u64,
}

/// One backend = one rustic repository the pillbox owns. The trait
/// shape keeps the abstraction open for v0.7+ non-rustic backends; the
/// only concrete impl in PR 3 is [`rustic::RusticBackend`].
pub(crate) trait WorkspaceBackend {
    /// Create a new snapshot of `cwd`. Returns the snapshot record.
    fn push(&self, cwd: &Path, opts: PushOptions) -> Result<Snapshot>;

    /// Restore `cwd` from a snapshot. `None` = latest.
    fn pull(&self, cwd: &Path, snapshot: Option<&SnapshotHandle>) -> Result<()>;

    /// All snapshots, oldest first.
    fn snapshots(&self) -> Result<Vec<Snapshot>>;

    /// One snapshot by handle. Accepts prefix matches the same way
    /// `git show <prefix>` does.
    fn snapshot_show(&self, handle: &SnapshotHandle) -> Result<Snapshot>;

    /// Remove a snapshot. The underlying rustic repo still holds the
    /// data packs until a future `prune` (not exposed in PR 3) — this
    /// is the equivalent of `restic forget`, not `restic forget --prune`.
    fn snapshot_rm(&self, handle: &SnapshotHandle) -> Result<()>;

    /// Rotate the repository password. Generates a fresh password,
    /// writes it via [`rustic::RusticBackend::add_key`], then deletes
    /// the prior key. Subsequent operations use the new password.
    fn rekey(&self) -> Result<()>;
}
