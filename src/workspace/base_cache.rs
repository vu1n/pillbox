//! Content-addressed "materialize once" cache — the base half of the workspace
//! fork mechanism (the CoW-clone half is [`super::cow`]).
//!
//! `k` workers forked off one immutable snapshot should pay a single restore,
//! not `k`. [`materialize_once`] populates a shared cache dir keyed by a stable
//! content id exactly once — flock-guarded, with a completion marker so an
//! interrupted restore is discarded and rebuilt rather than served truncated.
//! The restore itself is a caller-supplied closure, so this core carries no
//! backend or storage dependency; the libkrun run path wraps it with the cache
//! location + the rustic pull.
//!
//! Compiled unconditionally (like [`super::cow`] / [`super::ingest`]) so its
//! flock/concurrency tests run on every CI target, not only the libkrun build.
//! Context: doc://pillbox/workspace-cow-fork@latest#workspace-cow-fork
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Materialize a value into `root/<key>` exactly once and return that dir.
/// `restore` populates a freshly-created target; it runs only when the entry
/// isn't already complete. Racers are serialized by an `flock` on a per-key
/// lockfile (a separate open file description, so it contends across threads of
/// one process too) and double-check the completion marker. An entry left
/// partial by an interrupted restore (dir present, marker absent) is discarded
/// and rebuilt, so a crash mid-restore never yields a truncated result.
///
/// `key` MUST be a stable, content-addressed id (e.g. a snapshot handle): the
/// cache never invalidates, so a mutable key would serve stale data forever.
pub(crate) fn materialize_once(
    root: &Path,
    key: &str,
    restore: impl FnOnce(&Path) -> Result<()>,
) -> Result<PathBuf> {
    use std::os::unix::io::AsRawFd;

    let entry = root.join(key);
    let marker = root.join(format!("{key}.complete"));
    // Fast path: already materialized — no lock, no restore. Require the entry
    // dir too, not just the marker, so an externally-deleted entry (a cleanup
    // tool that clears big dirs but leaves the tiny marker) self-heals via a
    // rebuild instead of returning a path that no longer exists.
    if marker.exists() && entry.is_dir() {
        return Ok(entry);
    }
    std::fs::create_dir_all(root).with_context(|| format!("create cache {}", root.display()))?;

    // Hold the per-key lock for the whole build (the File drops → fd closes →
    // lock releases on every return path below).
    let lock_path = root.join(format!("{key}.lock"));
    let lock = std::fs::File::create(&lock_path)
        .with_context(|| format!("open cache lock {}", lock_path.display()))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error()).context("flock cache entry");
    }
    // Double-check: a racer may have completed the build while we waited.
    if marker.exists() && entry.is_dir() {
        return Ok(entry);
    }
    // We own the build. Discard any partial entry from an interrupted prior run.
    if entry.exists() {
        std::fs::remove_dir_all(&entry)
            .with_context(|| format!("clear partial cache entry {}", entry.display()))?;
    }
    std::fs::create_dir_all(&entry).with_context(|| format!("create {}", entry.display()))?;
    restore(&entry).with_context(|| format!("restore into {}", entry.display()))?;
    // Marker written last: its presence is the "fully materialized" signal every
    // reader gates on, so an entry is only ever observed complete.
    std::fs::File::create(&marker)
        .with_context(|| format!("mark complete {}", marker.display()))?;
    Ok(entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn runs_restore_exactly_once() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().join("cache");
        let calls = AtomicUsize::new(0);

        let a = materialize_once(&root, "h", |dst| {
            calls.fetch_add(1, Ordering::SeqCst);
            std::fs::write(dst.join("f.txt"), b"snap")?;
            Ok(())
        })
        .unwrap();
        // Second call for the same key must reuse the cache, not re-restore.
        let b = materialize_once(&root, "h", |dst| {
            calls.fetch_add(1, Ordering::SeqCst);
            std::fs::write(dst.join("f.txt"), b"AGAIN").unwrap();
            Ok(())
        })
        .unwrap();

        assert_eq!(a, b, "same key → same cached dir");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "restore runs exactly once");
        assert_eq!(
            std::fs::read(a.join("f.txt")).unwrap(),
            b"snap",
            "the second call did not re-restore over the cache"
        );
    }

    #[test]
    fn rebuilds_a_partial_entry() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().join("cache");
        // Simulate a crash mid-restore: the entry dir exists with stale content
        // but no completion marker.
        std::fs::create_dir_all(root.join("h")).unwrap();
        std::fs::write(root.join("h").join("stale.txt"), b"partial").unwrap();

        let calls = AtomicUsize::new(0);
        let entry = materialize_once(&root, "h", |dst| {
            calls.fetch_add(1, Ordering::SeqCst);
            std::fs::write(dst.join("fresh.txt"), b"ok")?;
            Ok(())
        })
        .unwrap();

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a markerless entry is rebuilt"
        );
        assert!(entry.join("fresh.txt").exists(), "fresh content present");
        assert!(!entry.join("stale.txt").exists(), "stale partial discarded");
    }

    #[test]
    fn rebuilds_when_entry_deleted_under_marker() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().join("cache");
        let calls = AtomicUsize::new(0);
        materialize_once(&root, "h", |dst| {
            calls.fetch_add(1, Ordering::SeqCst);
            std::fs::write(dst.join("f.txt"), b"snap")?;
            Ok(())
        })
        .unwrap();
        // External cleanup removes the entry dir but leaves the marker sibling.
        std::fs::remove_dir_all(root.join("h")).unwrap();
        assert!(
            root.join("h.complete").exists(),
            "marker survived the delete"
        );

        let entry = materialize_once(&root, "h", |dst| {
            calls.fetch_add(1, Ordering::SeqCst);
            std::fs::write(dst.join("f.txt"), b"snap")?;
            Ok(())
        })
        .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "a marker with no entry rebuilds"
        );
        assert_eq!(std::fs::read(entry.join("f.txt")).unwrap(), b"snap");
    }

    #[test]
    fn restores_once_under_concurrency() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().join("cache");
        let calls = AtomicUsize::new(0);
        let results = std::sync::Mutex::new(Vec::new());

        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    let p = materialize_once(&root, "h", |dst| {
                        calls.fetch_add(1, Ordering::SeqCst);
                        // Widen the window so a non-locked impl would double-restore.
                        std::thread::sleep(std::time::Duration::from_millis(40));
                        std::fs::write(dst.join("f.txt"), b"snap")?;
                        Ok(())
                    })
                    .unwrap();
                    results.lock().unwrap().push(p);
                });
            }
        });

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the flock serializes racers → exactly one restore"
        );
        let rs = results.lock().unwrap();
        assert_eq!(rs.len(), 8);
        assert!(
            rs.iter().all(|p| *p == rs[0]),
            "all workers get the same dir"
        );
        assert_eq!(std::fs::read(rs[0].join("f.txt")).unwrap(), b"snap");
    }
}
