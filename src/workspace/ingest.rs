//! One-time workspace **ingest** for remote placements (`docker://`, k8s,
//! managed): decide what crosses the wire the first time a workspace enters a
//! remote store. Per docs/remotes-redesign.md ("Workspace I/O" + Addendum),
//! this is the *ingest* path only — the per-run path is rustic-fork-from-store
//! + overlay-CoW, which moves no per-run bytes.
//!
//! The contract, refined from the original "respect `.gitignore`":
//!
//! - **Secret denylist, NOT a `.gitignore` allowlist.** `.gitignore` answers
//!   "what shouldn't rustic *back up*," not "what the container *needs*."
//!   Conflating them either leaks secrets or breaks the agent.
//! - **Keep `.git`.** Coding agents run `log`/`blame`/`diff` and commit;
//!   stripping history makes remote strictly worse than local.
//! - **Keep derived dirs** (`node_modules`, `target/`): the agent may need them,
//!   and regenerate-in-container is gated on the egress decision (deferred). The
//!   **size guard** — not silent exclusion — handles big trees.
//! - **Size threshold → fail loud.** Above the threshold the caller falls back
//!   to S3/rustic; never silently ship GBs nor silently truncate.
//! - **No silent caps**: [`IngestPlan`] reports the secrets it dropped so the
//!   caller can emit a note.
//!
//! Upholds invariant **I6 (sovereignty)**: nothing the user didn't intend leaves
//! the machine.
//!
//! Built contract-first: the runtime consumer is the `docker://` container
//! lifecycle (tar `plan_ingest().files` → `docker cp` into the container), which
//! lands in the next slice. Tested now so the secret denylist is proven before
//! anything ships bytes.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Default ceiling for the tar-cp ingest path. Above this, the caller should
/// fall back to S3/rustic (content-addressed delta) rather than shipping a
/// full tarball over the wire. 256 MiB comfortably holds a source tree with
/// `.git`; multi-GB `node_modules`/`target` trees trip it on purpose.
pub(crate) const DEFAULT_INGEST_THRESHOLD_BYTES: u64 = 256 * 1024 * 1024;

/// Directory names whose entire subtree never crosses the wire — credential
/// stores that have no business in a workspace transfer. (`.git` is **not**
/// here on purpose; see the module docs.)
const SECRET_DIR_NAMES: &[&str] = &[".ssh", ".aws", ".gnupg"];

/// Exact secret file basenames.
const SECRET_FILE_NAMES: &[&str] = &[
    ".netrc",
    "_netrc",
    ".npmrc",
    ".pypirc",
    ".git-credentials",
    ".pgpass",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
];

/// Secret file extensions — key material / key stores. `.key` is included
/// despite the small risk of dropping a legitimately-named data file: a stray
/// private key on a remote host is the worse outcome, and pillbox's whole point
/// is that the vault injects credentials rather than the workspace carrying
/// them.
const SECRET_FILE_EXTS: &[&str] = &["pem", "key", "p12", "pfx"];

/// `.env*` basenames that are **safe** to ship — templates meant to be shared.
/// Everything else matching `.env`/`.env.*` is treated as a real secret file.
const ENV_TEMPLATE_SUFFIXES: &[&str] = &[".example", ".sample", ".template", ".dist", ".defaults"];

/// Is `name` (a path component) a secret directory whose subtree we skip?
fn is_secret_dir(name: &str) -> bool {
    SECRET_DIR_NAMES.contains(&name)
}

/// Is `name` (a file basename) a secret file that must not cross the wire?
// Context: doc://pillbox/workspace-ingest-sovereignty@0001#workspace-ingest-sovereignty
pub(crate) fn is_secret_basename(name: &str) -> bool {
    if SECRET_FILE_NAMES.contains(&name) {
        return true;
    }
    if let Some(ext) = name.rsplit_once('.').map(|(_, e)| e) {
        if SECRET_FILE_EXTS.contains(&ext) {
            return true;
        }
    }
    // `.env` family: `.env` and `.env.<x>` are secrets, except shared templates
    // (`.env.example`, `.env.sample`, …).
    if name == ".env" {
        return true;
    }
    if name.starts_with(".env.") {
        let is_template = ENV_TEMPLATE_SUFFIXES.iter().any(|suf| name.ends_with(suf));
        return !is_template;
    }
    false
}

/// The result of planning an ingest: the files that will be tarred, the total
/// byte count, and the secrets that were dropped (so the caller can log them —
/// no silent caps).
#[derive(Debug, Default, Clone)]
pub(crate) struct IngestPlan {
    /// Included files, relative to the ingest root.
    pub(crate) files: Vec<PathBuf>,
    /// Sum of included file sizes (bytes).
    pub(crate) total_bytes: u64,
    /// Secret paths excluded, relative to the root — surfaced, never silent.
    pub(crate) excluded_secrets: Vec<PathBuf>,
}

impl IngestPlan {
    /// Does the planned transfer exceed `threshold` bytes? When true the caller
    /// should fall back to S3/rustic rather than tar-cp.
    pub(crate) fn exceeds(&self, threshold: u64) -> bool {
        self.total_bytes > threshold
    }
}

/// Walk `root`, applying the secret denylist, and return the [`IngestPlan`].
/// Does **not** follow symlinks (they're recorded as included entries with no
/// size, never traversed — bounds the walk and avoids loops). Secret
/// directories are pruned wholesale; `.git` and derived dirs are kept.
pub(crate) fn plan_ingest(root: &Path) -> Result<IngestPlan> {
    let mut plan = IngestPlan::default();
    // Explicit stack instead of recursion — bounded, no walkdir dep.
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir)
            .with_context(|| format!("reading {} for workspace ingest", dir.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("reading entry under {}", dir.display()))?;
            let path = entry.path();
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n,
                None => continue, // non-UTF-8 name — skip rather than guess
            };
            let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            // `DirEntry::file_type` classifies the link itself (no symlink
            // follow) and often needs no syscall at all — cheaper than
            // re-stat'ing a rebuilt path. We pay for `metadata()` (also
            // no-follow) only on the regular-file branch, where `len()` is
            // actually used.
            let file_type = entry
                .file_type()
                .with_context(|| format!("file type of {}", path.display()))?;
            if file_type.is_symlink() {
                // Record but don't traverse; tar will capture the link itself.
                plan.files.push(rel);
                continue;
            }
            if file_type.is_dir() {
                if is_secret_dir(name) {
                    plan.excluded_secrets.push(rel);
                    continue; // prune the whole subtree
                }
                stack.push(path);
                continue;
            }
            // Regular file.
            if is_secret_basename(name) {
                plan.excluded_secrets.push(rel);
                continue;
            }
            let len = entry
                .metadata()
                .with_context(|| format!("stat {}", path.display()))?
                .len();
            plan.total_bytes = plan.total_bytes.saturating_add(len);
            plan.files.push(rel);
        }
    }
    // Stable order so output/logs are deterministic across runs.
    plan.files.sort();
    plan.excluded_secrets.sort();
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_files_are_secret_except_templates() {
        assert!(is_secret_basename(".env"));
        assert!(is_secret_basename(".env.local"));
        assert!(is_secret_basename(".env.production"));
        // Shared templates are safe to ship.
        assert!(!is_secret_basename(".env.example"));
        assert!(!is_secret_basename(".env.sample"));
        assert!(!is_secret_basename(".env.template"));
        assert!(!is_secret_basename(".env.dist"));
    }

    #[test]
    fn key_material_is_secret() {
        assert!(is_secret_basename("server.pem"));
        assert!(is_secret_basename("privkey.key"));
        assert!(is_secret_basename("keystore.p12"));
        assert!(is_secret_basename("cert.pfx"));
        assert!(is_secret_basename("id_ed25519"));
        assert!(is_secret_basename("id_rsa"));
    }

    #[test]
    fn credential_files_are_secret() {
        assert!(is_secret_basename(".netrc"));
        assert!(is_secret_basename(".npmrc"));
        assert!(is_secret_basename(".pypirc"));
        assert!(is_secret_basename(".git-credentials"));
        assert!(is_secret_basename(".pgpass"));
    }

    #[test]
    fn ordinary_source_files_are_kept() {
        assert!(!is_secret_basename("main.rs"));
        assert!(!is_secret_basename("README.md"));
        assert!(!is_secret_basename("Cargo.toml"));
        assert!(!is_secret_basename("package.json"));
        assert!(!is_secret_basename("config.yaml"));
    }

    #[test]
    fn secret_dirs_are_pruned_git_is_kept() {
        assert!(is_secret_dir(".ssh"));
        assert!(is_secret_dir(".aws"));
        assert!(is_secret_dir(".gnupg"));
        // .git and derived dirs are NOT secret dirs — agents need them.
        assert!(!is_secret_dir(".git"));
        assert!(!is_secret_dir("node_modules"));
        assert!(!is_secret_dir("target"));
    }

    #[test]
    fn plan_includes_git_and_node_modules_excludes_secrets() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // Source + the things we must KEEP.
        std::fs::write(root.join("main.rs"), b"fn main() {}").unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/config"), b"[core]").unwrap();
        std::fs::create_dir_all(root.join("node_modules/left-pad")).unwrap();
        std::fs::write(root.join("node_modules/left-pad/index.js"), b"//").unwrap();
        // Secrets we must DROP.
        std::fs::write(root.join(".env"), b"SECRET=1").unwrap();
        std::fs::write(root.join(".env.example"), b"SECRET=").unwrap();
        std::fs::write(root.join("tls.key"), b"-----BEGIN").unwrap();
        std::fs::create_dir_all(root.join(".ssh")).unwrap();
        std::fs::write(root.join(".ssh/id_ed25519"), b"key").unwrap();

        let plan = plan_ingest(root).unwrap();
        let inc: Vec<String> = plan
            .files
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();
        let exc: Vec<String> = plan
            .excluded_secrets
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();

        assert!(inc.contains(&"main.rs".to_string()));
        assert!(inc.contains(&".git/config".to_string()), "keep .git");
        assert!(
            inc.contains(&"node_modules/left-pad/index.js".to_string()),
            "keep derived dirs"
        );
        assert!(inc.contains(&".env.example".to_string()), "keep templates");

        assert!(exc.contains(&".env".to_string()));
        assert!(exc.contains(&"tls.key".to_string()));
        assert!(exc.contains(&".ssh".to_string()), "prune secret dir");
        // The pruned .ssh subtree must not leak as an included file.
        assert!(!inc.iter().any(|f| f.starts_with(".ssh")));
    }

    #[test]
    fn threshold_fires_loud_on_large_trees() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("big.bin"), vec![0u8; 4096]).unwrap();
        let plan = plan_ingest(tmp.path()).unwrap();
        assert_eq!(plan.total_bytes, 4096);
        assert!(plan.exceeds(1024), "4KB tree exceeds a 1KB threshold");
        assert!(!plan.exceeds(DEFAULT_INGEST_THRESHOLD_BYTES));
    }
}
