//! The per-session content-addressed blob store — the §0 spine's large-payload
//! sibling to [`crate::events::log::SessionLog`].
//!
//! Structured artifacts ([`crate::contract::Artifact`]) keep only a small typed
//! reference on the append-only log; the body itself lands here, at
//! `<pillbox>/sessions/<id>/blobs/<sha256>`, content-addressed so identical
//! bodies dedup and a reference can never silently point at changed bytes (see
//! docs/session-event-log.md §Storage layout). A grader report, a judge
//! critique, a dispatch worker summary, a FastContext citation set — anything
//! too big to inline into the log without drowning replay — goes through here.
//!
//! Local-first: a plain `blobs/<hash>` directory (0700 dir, 0600 files),
//! greppable, no extra service. The spec's "reuse the rustic content-addressed
//! store + at-rest encryption" upgrade is for the *sensitive* `raw_body` /
//! `pty_snapshot` capture blobs; these structured artifacts are the simple
//! local case. Pure file I/O — no docker/agent/network — and unit-tested below.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::paths::{ensure_mode_0700, write_private_file};
use crate::pillbox::Pillbox;

/// The blobs subdirectory inside a session's directory (sibling of `log.jsonl`).
const BLOBS_DIR: &str = "blobs";

/// Content-addressed blob store for one session. Handles are lowercase
/// sha256 hex; `put` is idempotent (same bytes → same handle → write skipped),
/// so a re-run of an artifact-producing step never duplicates storage.
pub(crate) struct BlobStore {
    /// `<pillbox>/sessions/<id>/blobs/`. Created lazily on the first `put`.
    dir: PathBuf,
}

impl BlobStore {
    /// Open the store for `session_id` under `pb`'s state dir. Mirrors
    /// [`crate::events::log::SessionLog::open`] — same `sessions/<id>/` parent.
    pub(crate) fn open(pb: &Pillbox, session_id: &str) -> Result<Self> {
        Ok(Self::open_at(crate::session::session_dir(pb, session_id)?))
    }

    /// Open the store at an already-resolved session directory — for a caller
    /// that has the path but not a [`Pillbox`]. Does no I/O; the dir is created
    /// on the first [`put`](Self::put).
    pub(crate) fn open_at(session_dir: PathBuf) -> Self {
        Self {
            dir: session_dir.join(BLOBS_DIR),
        }
    }

    /// Store `body`, returning its content-address handle (sha256 hex). The
    /// write is skipped when the blob already exists (content-addressed dedup),
    /// so calling `put` twice with the same bytes is a no-op the second time.
    pub(crate) fn put(&self, body: &[u8]) -> Result<String> {
        let handle = sha256_hex(body);
        let path = self.dir.join(&handle);
        if path.exists() {
            return Ok(handle);
        }
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("create blob dir {}", self.dir.display()))?;
        ensure_mode_0700(&self.dir)?;
        write_private_file(&path, body)?;
        Ok(handle)
    }

    /// Read the blob with `handle`. Validates the handle is a bare sha256 hex
    /// string first — so a malformed/hostile ref (path separators, `..`) can't
    /// escape the blob dir — then reads, erroring clearly if absent.
    pub(crate) fn get(&self, handle: &str) -> Result<Vec<u8>> {
        let path = self.path(handle)?;
        fs::read(&path).with_context(|| format!("read blob {}", path.display()))
    }

    /// The on-disk path for `handle`, after validating it is a bare sha256 hex
    /// handle. The validation is the path-traversal guard: a handle is always a
    /// 64-char lowercase hex string (the store mints them), so anything else is
    /// rejected before it touches the filesystem.
    pub(crate) fn path(&self, handle: &str) -> Result<PathBuf> {
        if !is_sha256_hex(handle) {
            anyhow::bail!("invalid blob handle (expected 64-char sha256 hex): {handle:?}");
        }
        Ok(self.dir.join(handle))
    }
}

fn sha256_hex(body: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(body);
    let digest = hasher.finalize();
    let mut s = String::with_capacity(64);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// A valid handle is exactly 64 lowercase hex chars — the only shape `put`
/// emits. Rejecting everything else doubles as the path-traversal guard for
/// [`BlobStore::get`]/[`path`](BlobStore::path) (no `/`, no `..`).
fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, BlobStore) {
        let tmp = tempfile::tempdir().unwrap();
        let s = BlobStore::open_at(tmp.path().to_path_buf());
        (tmp, s)
    }

    #[test]
    fn put_then_get_round_trips() {
        let (_tmp, s) = store();
        let body = br#"{"citations":[{"file":"src/auth.rs","line":42}]}"#;
        let handle = s.put(body).unwrap();
        assert!(is_sha256_hex(&handle), "{handle}");
        assert_eq!(s.get(&handle).unwrap(), body);
    }

    #[test]
    fn content_addressing_dedups() {
        let (_tmp, s) = store();
        let a = s.put(b"same bytes").unwrap();
        let b = s.put(b"same bytes").unwrap();
        let c = s.put(b"other bytes").unwrap();
        assert_eq!(a, b, "identical bodies share a handle");
        assert_ne!(a, c, "different bodies differ");
    }

    #[test]
    fn put_is_idempotent_on_existing_blob() {
        let (_tmp, s) = store();
        let h = s.put(b"x").unwrap();
        // Second put of the same bytes returns the same handle without error.
        assert_eq!(s.put(b"x").unwrap(), h);
    }

    #[test]
    fn get_missing_blob_errors() {
        let (_tmp, s) = store();
        // A well-formed but absent handle (64 hex chars) errors on read.
        let absent = "0".repeat(64);
        assert!(s.get(&absent).is_err());
    }

    #[test]
    fn malformed_handle_is_rejected_before_fs() {
        let (_tmp, s) = store();
        // Path-traversal / non-hex handles never reach the filesystem.
        for bad in ["../escape", "abc", &"Z".repeat(64), "subdir/blob"] {
            assert!(s.path(bad).is_err(), "should reject {bad:?}");
            assert!(s.get(bad).is_err(), "should reject {bad:?}");
        }
    }
}
