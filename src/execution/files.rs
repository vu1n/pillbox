//! Exact-file repository operations for the host-owned execution broker.
//! Admission supplies the complete tree and policy; no host paths or filesystem
//! access enter this boundary. See docs/local-repository-execution.md.

use anyhow::{bail, ensure, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(crate) const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
pub(crate) const MAX_SNAPSHOT_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_TOOL_CALLS: u64 = 10_000;
pub(crate) const MAX_OUTPUT_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_FILES: usize = 4_096;
pub(crate) const MAX_PATH_BYTES: usize = 1_024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileEntry {
    pub(crate) path: String,
    pub(crate) executable: bool,
    pub(crate) bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileLimits {
    pub(crate) max_file_bytes: u64,
    pub(crate) max_snapshot_bytes: u64,
    pub(crate) max_tool_calls: u64,
    /// Cumulative file data returned by reads, excluding outer RPC framing.
    pub(crate) max_output_bytes: u64,
}

impl FileLimits {
    pub(crate) fn validate(&self) -> Result<()> {
        for (name, value, ceiling) in [
            ("max_file_bytes", self.max_file_bytes, MAX_FILE_BYTES),
            (
                "max_snapshot_bytes",
                self.max_snapshot_bytes,
                MAX_SNAPSHOT_BYTES,
            ),
            ("max_tool_calls", self.max_tool_calls, MAX_TOOL_CALLS),
            ("max_output_bytes", self.max_output_bytes, MAX_OUTPUT_BYTES),
        ] {
            ensure!(
                value > 0 && value <= ceiling,
                "{name} must be in 1..={ceiling}"
            );
        }
        ensure!(
            self.max_file_bytes <= self.max_snapshot_bytes,
            "max_file_bytes exceeds max_snapshot_bytes"
        );
        Ok(())
    }
}

/// Declaration order is the canonical lexical order of operation names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FileOperation {
    Read,
    Remove,
    Write,
}

#[derive(Clone, Debug)]
pub(crate) struct FilePolicy {
    read_paths: Vec<String>,
    write_paths: Vec<String>,
    operations: Vec<FileOperation>,
    limits: FileLimits,
}

impl FilePolicy {
    pub(crate) fn new(
        read_paths: Vec<String>,
        write_paths: Vec<String>,
        operations: Vec<FileOperation>,
        limits: FileLimits,
    ) -> Result<Self> {
        limits.validate()?;
        validate_paths(&read_paths)?;
        validate_paths(&write_paths)?;
        ensure!(
            operations.len() <= 3 && operations.windows(2).all(|pair| pair[0] < pair[1]),
            "file operations must be sorted and unique"
        );
        Ok(Self {
            read_paths,
            write_paths,
            operations,
            limits,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileTree {
    entries: Vec<FileEntry>,
    total_bytes: u64,
    digest: String,
}

impl FileTree {
    pub(crate) fn new(entries: Vec<FileEntry>, limits: &FileLimits) -> Result<Self> {
        limits.validate()?;
        validate_entries(&entries, limits)?;
        let total_bytes = entries.iter().map(|entry| entry.bytes.len() as u64).sum();
        let digest = tree_digest(&entries)?;
        Ok(Self {
            entries,
            total_bytes,
            digest,
        })
    }

    pub(crate) fn entries(&self) -> &[FileEntry] {
        &self.entries
    }

    pub(crate) fn digest(&self) -> &str {
        &self.digest
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct BrokerUsage {
    pub(crate) tool_calls: u64,
    pub(crate) output_bytes: u64,
}

pub(crate) struct FileBroker {
    base: FileTree,
    entries: Vec<FileEntry>,
    total_bytes: u64,
    policy: FilePolicy,
    usage: BrokerUsage,
}

/// Host-only capture. Returning this tree to the agent would bypass read grants.
#[derive(Debug)]
pub(crate) struct FileResult {
    pub(crate) base_digest: String,
    pub(crate) result_digest: String,
    pub(crate) changed_paths: Vec<String>,
    pub(crate) tree: FileTree,
}

impl FileBroker {
    pub(crate) fn new(base: FileTree, policy: FilePolicy) -> Result<Self> {
        // A tree can have been admitted under larger limits than this invocation.
        validate_entries(&base.entries, &policy.limits)?;
        Ok(Self {
            entries: base.entries.clone(),
            total_bytes: base.total_bytes,
            base,
            policy,
            usage: BrokerUsage::default(),
        })
    }

    pub(crate) fn read(&mut self, path: &str) -> Result<FileEntry> {
        self.authorize(FileOperation::Read, path)?;
        let index = self
            .find(path)
            .map_err(|_| anyhow::anyhow!("file does not exist"))?;
        let entry = &self.entries[index];
        let bytes = entry.bytes.len() as u64;
        ensure!(
            bytes <= self.policy.limits.max_output_bytes - self.usage.output_bytes,
            "file read exceeds output byte limit"
        );
        self.usage.output_bytes += bytes;
        Ok(entry.clone())
    }

    pub(crate) fn write(&mut self, path: &str, executable: bool, bytes: &[u8]) -> Result<()> {
        self.authorize(FileOperation::Write, path)?;
        ensure!(
            bytes.len() as u64 <= self.policy.limits.max_file_bytes,
            "file write exceeds per-file byte limit"
        );
        let position = self.find(path);
        let old_bytes = position
            .as_ref()
            .map_or(0, |index| self.entries[*index].bytes.len() as u64);
        let total_bytes = self.total_bytes - old_bytes + bytes.len() as u64;
        ensure!(
            total_bytes <= self.policy.limits.max_snapshot_bytes,
            "file write exceeds snapshot byte limit"
        );
        if position.is_err() {
            ensure!(
                self.entries.len() < MAX_FILES,
                "file count exceeds {MAX_FILES}"
            );
            ensure!(
                self.entries
                    .iter()
                    .all(|entry| !paths_overlap(&entry.path, path)),
                "file path conflicts with an existing file ancestor or descendant"
            );
        }
        // Check every fallible bound before allocating or replacing any bytes.
        let entry = FileEntry {
            path: path.to_string(),
            executable,
            bytes: bytes.to_vec(),
        };
        match position {
            Ok(index) => self.entries[index] = entry,
            Err(index) => self.entries.insert(index, entry),
        }
        self.total_bytes = total_bytes;
        Ok(())
    }

    pub(crate) fn remove(&mut self, path: &str) -> Result<()> {
        self.authorize(FileOperation::Remove, path)?;
        if let Ok(index) = self.find(path) {
            self.total_bytes -= self.entries.remove(index).bytes.len() as u64;
        }
        // Absence is success too: write permission must not disclose existence.
        Ok(())
    }

    pub(crate) fn usage(&self) -> BrokerUsage {
        self.usage
    }

    pub(crate) fn finish(self) -> Result<FileResult> {
        let mut changed_paths = Vec::new();
        for entry in &self.base.entries {
            if self
                .find(&entry.path)
                .ok()
                .map(|index| &self.entries[index])
                != Some(entry)
            {
                changed_paths.push(entry.path.clone());
            }
        }
        for entry in &self.entries {
            if self
                .base
                .entries
                .binary_search_by(|base| base.path.as_str().cmp(&entry.path))
                .is_err()
            {
                changed_paths.push(entry.path.clone());
            }
        }
        changed_paths.sort();
        let tree = FileTree::new(self.entries, &self.policy.limits)?;
        Ok(FileResult {
            base_digest: self.base.digest,
            result_digest: tree.digest.clone(),
            changed_paths,
            tree,
        })
    }

    fn find(&self, path: &str) -> std::result::Result<usize, usize> {
        self.entries
            .binary_search_by(|entry| entry.path.as_str().cmp(path))
    }

    fn authorize(&mut self, operation: FileOperation, path: &str) -> Result<()> {
        ensure!(
            self.usage.tool_calls < self.policy.limits.max_tool_calls,
            "file tool-call limit exhausted"
        );
        // Rejected attempts consume calls, otherwise invalid calls can run forever.
        self.usage.tool_calls += 1;
        validate_path(path)?;
        ensure!(
            self.policy.operations.binary_search(&operation).is_ok(),
            "file operation is not granted"
        );
        let paths = match operation {
            FileOperation::Read => &self.policy.read_paths,
            FileOperation::Write | FileOperation::Remove => &self.policy.write_paths,
        };
        ensure!(
            paths
                .binary_search_by(|allowed| allowed.as_str().cmp(path))
                .is_ok(),
            "exact file path is not granted"
        );
        Ok(())
    }
}

pub(crate) fn validate_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty() && path.len() <= MAX_PATH_BYTES,
        "file path is empty or exceeds {MAX_PATH_BYTES} bytes"
    );
    ensure!(
        path.is_ascii(),
        "non-ASCII file paths are unsupported by this policy revision"
    );
    ensure!(
        !path.bytes().any(|byte| byte.is_ascii_control()
            || matches!(byte, b'\\' | b':' | b'*' | b'?' | b'[' | b']' | b'{' | b'}')),
        "file path contains a forbidden character"
    );
    for component in path.split('/') {
        ensure!(
            !matches!(component, "" | "." | ".." | ".git"),
            "file path contains an invalid component"
        );
        // These are the immutable directory exclusions in workspace::ingest.
        ensure!(
            !matches!(component, ".ssh" | ".aws" | ".gnupg"),
            "file path names a secret directory"
        );
    }
    ensure!(
        !crate::workspace::ingest::is_secret_basename(path.rsplit('/').next().unwrap_or(path)),
        "file path names an excluded secret file"
    );
    Ok(())
}

fn validate_paths(paths: &[String]) -> Result<()> {
    ensure!(
        paths.len() <= MAX_FILES,
        "file path count exceeds {MAX_FILES}"
    );
    ensure!(
        paths.windows(2).all(|pair| pair[0] < pair[1]),
        "file paths must be sorted and unique"
    );
    for path in paths {
        validate_path(path)?;
    }
    Ok(())
}

fn validate_entries(entries: &[FileEntry], limits: &FileLimits) -> Result<()> {
    ensure!(entries.len() <= MAX_FILES, "file count exceeds {MAX_FILES}");
    ensure!(
        entries.windows(2).all(|pair| pair[0].path < pair[1].path),
        "file entries must be sorted and unique"
    );
    let mut total_bytes = 0_u64;
    for entry in entries {
        validate_path(&entry.path)?;
        ensure!(
            entry.bytes.len() as u64 <= limits.max_file_bytes,
            "file exceeds per-file byte limit"
        );
        total_bytes += entry.bytes.len() as u64;
        ensure!(
            total_bytes <= limits.max_snapshot_bytes,
            "files exceed snapshot byte limit"
        );
        for (index, _) in entry.path.match_indices('/') {
            if entries
                .binary_search_by(|other| other.path.as_str().cmp(&entry.path[..index]))
                .is_ok()
            {
                bail!("file path conflicts with an existing file ancestor");
            }
        }
    }
    Ok(())
}

fn paths_overlap(left: &str, right: &str) -> bool {
    left.strip_prefix(right)
        .is_some_and(|rest| rest.starts_with('/'))
        || right
            .strip_prefix(left)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn tree_digest(entries: &[FileEntry]) -> Result<String> {
    // These fields are already in UTF-16 key order. No raw bytes or host metadata
    // enter the tree manifest, and ASCII paths preserve UTF-16 entry ordering.
    #[derive(Serialize)]
    struct ManifestEntry<'a> {
        executable: bool,
        path: &'a str,
        sha256: String,
    }
    let manifest: Vec<_> = entries
        .iter()
        .map(|entry| ManifestEntry {
            executable: entry.executable,
            path: &entry.path,
            sha256: digest(&entry.bytes),
        })
        .collect();
    Ok(digest(&serde_json::to_vec(&manifest)?))
}

fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> FileLimits {
        FileLimits {
            max_file_bytes: 8,
            max_snapshot_bytes: 16,
            max_tool_calls: 20,
            max_output_bytes: 8,
        }
    }

    fn entry(path: &str, bytes: &[u8]) -> FileEntry {
        FileEntry {
            path: path.into(),
            executable: false,
            bytes: bytes.into(),
        }
    }

    fn policy(
        read: &[&str],
        write: &[&str],
        operations: Vec<FileOperation>,
        limits: FileLimits,
    ) -> FilePolicy {
        FilePolicy::new(
            read.iter().map(|path| (*path).into()).collect(),
            write.iter().map(|path| (*path).into()).collect(),
            operations,
            limits,
        )
        .unwrap()
    }

    fn broker(entries: Vec<FileEntry>, read: &[&str], write: &[&str]) -> FileBroker {
        let limits = limits();
        FileBroker::new(
            FileTree::new(entries, &limits).unwrap(),
            policy(
                read,
                write,
                vec![
                    FileOperation::Read,
                    FileOperation::Remove,
                    FileOperation::Write,
                ],
                limits,
            ),
        )
        .unwrap()
    }

    #[test]
    fn tree_digest_matches_canonical_manifest_and_includes_mode() {
        let tree = FileTree::new(vec![entry("file", b"a")], &limits()).unwrap();
        let manifest = br#"[{"executable":false,"path":"file","sha256":"sha256:ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb"}]"#;
        assert_eq!(tree.digest(), digest(manifest));
        let mut executable = entry("file", b"a");
        executable.executable = true;
        assert_ne!(
            tree.digest(),
            FileTree::new(vec![executable], &limits()).unwrap().digest()
        );
        assert_eq!(
            FileTree::new(vec![], &limits()).unwrap().digest(),
            digest(b"[]")
        );
    }

    #[test]
    fn rejects_noncanonical_secret_and_unsupported_paths() {
        for path in [
            "",
            "/x",
            "x/",
            "a//b",
            ".",
            "..",
            "a/../b",
            "a/./b",
            "a\\b",
            "C:x",
            "x\n",
            "x\u{7f}",
            "café",
            "a/*",
            "a/?",
            "a/[x]",
            "a/{x}",
            ".git/config",
            "a/.git",
            ".ssh/id",
            "a/.aws/config",
            ".gnupg/keyring",
            ".env",
            "x/.env.local",
            "x/.npmrc",
            "a.pem",
            "id_rsa",
        ] {
            assert!(validate_path(path).is_err(), "accepted {path:?}");
        }
        for path in [
            "src/main.rs",
            ".env.example",
            ".env.sample",
            "a b",
            "a\"b",
            "A",
            "a",
        ] {
            validate_path(path).unwrap();
        }
        assert!(validate_path(&"x".repeat(MAX_PATH_BYTES + 1)).is_err());
    }

    #[test]
    fn rejects_unsorted_duplicate_and_file_directory_collisions() {
        for entries in [
            vec![entry("b", b""), entry("a", b"")],
            vec![entry("a", b""), entry("a", b"")],
            vec![entry("a", b""), entry("a-b", b""), entry("a/b", b"")],
        ] {
            assert!(FileTree::new(entries, &limits()).is_err());
        }
        assert!(FilePolicy::new(vec!["b".into(), "a".into()], vec![], vec![], limits()).is_err());
        assert!(FilePolicy::new(vec![], vec!["a".into(), "a".into()], vec![], limits()).is_err());
        assert!(FilePolicy::new(
            vec![],
            vec![],
            vec![FileOperation::Write, FileOperation::Read],
            limits()
        )
        .is_err());
        assert!(FilePolicy::new(
            vec![],
            vec![],
            vec![FileOperation::Read, FileOperation::Read],
            limits()
        )
        .is_err());
        assert!(serde_json::from_str::<FileOperation>("\"execute\"").is_err());
    }

    #[test]
    fn exact_grants_and_operations_are_independent() {
        let mut broker = broker(
            vec![entry("a", b"old"), entry("ab", b"secret")],
            &["a"],
            &["b"],
        );
        assert_eq!(broker.read("a").unwrap().bytes, b"old");
        assert!(broker.read("ab").is_err());
        assert!(broker.read("a/x").is_err());
        assert!(broker.write("a", false, b"no").is_err());
        assert!(broker.remove("a").is_err());
        broker.write("b", false, b"new").unwrap();
        assert!(broker.read("b").is_err());
        let mut no_ops = FileBroker::new(
            FileTree::new(vec![entry("a", b"x")], &limits()).unwrap(),
            policy(&["a"], &["a"], vec![], limits()),
        )
        .unwrap();
        assert!(no_ops.read("a").is_err());
        assert!(no_ops.write("a", false, b"x").is_err());
        assert!(no_ops.remove("a").is_err());
    }

    #[test]
    fn write_only_does_not_return_old_bytes_or_existence() {
        let mut present = broker(vec![entry("a", b"secret")], &[], &["a"]);
        let mut absent = broker(vec![], &[], &["a"]);
        for broker in [&mut present, &mut absent] {
            assert_eq!(
                broker.read("a").unwrap_err().to_string(),
                "exact file path is not granted"
            );
            broker.write("a", false, b"new").unwrap();
            broker.remove("a").unwrap();
            broker.remove("a").unwrap();
            assert_eq!(broker.usage().output_bytes, 0);
        }
    }

    #[test]
    fn finish_tracks_deletes_additions_modes_and_preserves_input() {
        let base = FileTree::new(
            vec![entry("a", b"old"), entry("b", b"same"), entry("c", b"gone")],
            &limits(),
        )
        .unwrap();
        let original = base.clone();
        let mut broker = FileBroker::new(
            base,
            policy(
                &[],
                &["a", "b", "c", "d"],
                vec![FileOperation::Remove, FileOperation::Write],
                limits(),
            ),
        )
        .unwrap();
        broker.write("a", false, b"new").unwrap();
        broker.write("b", true, b"same").unwrap();
        broker.remove("c").unwrap();
        broker.write("d", false, b"added").unwrap();
        let result = broker.finish().unwrap();
        assert_eq!(result.base_digest, original.digest());
        assert_eq!(result.result_digest, result.tree.digest());
        assert_eq!(result.changed_paths, ["a", "b", "c", "d"]);
        assert_eq!(original.entries()[0].bytes, b"old");
        assert!(!original.entries()[1].executable);
        assert_eq!(original.entries()[2].path, "c");
    }

    #[test]
    fn delete_recreate_unchanged_and_noop_write_leave_no_change() {
        let mut broker = broker(vec![entry("a", b"old")], &[], &["a", "b"]);
        broker.remove("a").unwrap();
        broker.write("a", false, b"old").unwrap();
        broker.write("a", false, b"old").unwrap();
        broker.write("b", false, b"new").unwrap();
        broker.remove("b").unwrap();
        let result = broker.finish().unwrap();
        assert!(result.changed_paths.is_empty());
        assert_eq!(result.base_digest, result.result_digest);
    }

    #[test]
    fn failed_mutations_are_atomic_and_rejected_calls_are_bounded() {
        let mut bounds = limits();
        bounds.max_snapshot_bytes = 8;
        bounds.max_tool_calls = 4;
        let base = FileTree::new(vec![entry("a", b"12345678")], &bounds).unwrap();
        let digest = base.digest().to_string();
        let mut broker = FileBroker::new(
            base,
            policy(&[], &["a", "b"], vec![FileOperation::Write], bounds),
        )
        .unwrap();
        assert!(broker.write("a", true, b"123456789").is_err());
        assert!(broker.write("b", false, b"1").is_err());
        assert!(broker.write("not-granted", false, b"x").is_err());
        assert!(broker.remove("a").is_err());
        assert!(broker.write("a", false, b"ok").is_err());
        assert_eq!(broker.usage().tool_calls, 4);
        let result = broker.finish().unwrap();
        assert_eq!(result.result_digest, digest);
        assert!(result.changed_paths.is_empty());
    }

    #[test]
    fn output_limit_checks_precede_reads_and_failures_do_not_spend_output() {
        let mut broker = broker(vec![entry("a", b"12345")], &["a"], &[]);
        assert_eq!(broker.read("a").unwrap().bytes, b"12345");
        assert!(broker.read("a").is_err());
        assert_eq!(
            broker.usage(),
            BrokerUsage {
                tool_calls: 2,
                output_bytes: 5
            }
        );
        assert!(broker.finish().unwrap().changed_paths.is_empty());
    }

    #[test]
    fn zero_and_over_ceiling_limits_fail_before_broker_construction() {
        for limits in [
            FileLimits {
                max_file_bytes: 0,
                ..limits()
            },
            FileLimits {
                max_snapshot_bytes: 0,
                ..limits()
            },
            FileLimits {
                max_tool_calls: 0,
                ..limits()
            },
            FileLimits {
                max_output_bytes: 0,
                ..limits()
            },
            FileLimits {
                max_file_bytes: MAX_FILE_BYTES + 1,
                ..limits()
            },
            FileLimits {
                max_snapshot_bytes: MAX_SNAPSHOT_BYTES + 1,
                ..limits()
            },
            FileLimits {
                max_tool_calls: MAX_TOOL_CALLS + 1,
                ..limits()
            },
            FileLimits {
                max_output_bytes: MAX_OUTPUT_BYTES + 1,
                ..limits()
            },
            FileLimits {
                max_snapshot_bytes: 7,
                ..limits()
            },
        ] {
            assert!(limits.validate().is_err());
        }
        let base = FileTree::new(vec![entry("a", b"12345678")], &limits()).unwrap();
        let smaller = FileLimits {
            max_file_bytes: 4,
            ..limits()
        };
        assert!(FileBroker::new(base, policy(&[], &[], vec![], smaller)).is_err());
    }

    #[test]
    fn metadata_and_input_byte_limits_reject_whole_trees() {
        let too_many: Vec<_> = (0..=MAX_FILES)
            .map(|index| entry(&format!("file-{index:04}"), b""))
            .collect();
        assert!(FileTree::new(too_many.clone(), &limits()).is_err());
        assert!(FilePolicy::new(
            too_many.into_iter().map(|entry| entry.path).collect(),
            vec![],
            vec![FileOperation::Read],
            limits(),
        )
        .is_err());
        assert!(FileTree::new(vec![entry("a", b"123456789")], &limits()).is_err());
        assert!(FileTree::new(
            vec![
                entry("a", b"12345678"),
                entry("b", b"12345678"),
                entry("c", b"x")
            ],
            &limits(),
        )
        .is_err());
    }

    #[test]
    fn reads_use_case_sensitive_identity_and_binary_bytes() {
        let mut broker = broker(
            vec![entry("A", &[0, 255]), entry("a", b"lower")],
            &["A"],
            &[],
        );
        assert_eq!(broker.read("A").unwrap().bytes, [0, 255]);
        assert!(broker.read("a").is_err());
    }

    #[test]
    fn new_paths_cannot_replace_implicit_directories_or_descend_into_files() {
        let mut broker = broker(
            vec![entry("a", b"x"), entry("b/c", b"x")],
            &[],
            &["a/b", "b"],
        );
        assert!(broker.write("a/b", false, b"y").is_err());
        assert!(broker.write("b", false, b"y").is_err());
        assert!(broker.finish().unwrap().changed_paths.is_empty());
    }
}
