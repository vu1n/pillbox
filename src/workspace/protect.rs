//! Protected paths — the part of a worker's tree that decides its grade and that
//! the worker therefore must not be able to change.
//!
//! `dispatch` never grades a worker's live workspace. It grades a **grade tree**:
//! a copy of the workspace, secret-scrubbed, in which every path the
//! [`ProtectSet`] covers has been put back to exactly what the base (the
//! `--from-bookmark` snapshot the worker forked from) holds. Worker edits,
//! additions, deletions, renames and symlink swaps inside the set are all undone;
//! everything outside it is graded as the worker left it.
//!
//! The walks never follow a symlink, and every directory the restore writes
//! through is checked to be a real directory, so a worker-planted link cannot
//! redirect the restore outside the copy.
//!
//! Pattern syntax (a gitignore-flavoured subset):
//! - a pattern with no `/` (other than a trailing one) matches that name at any
//!   depth: `tests/` covers `tests/` and `pkg/tests/`, `*_test.go` any Go test;
//! - a pattern containing `/` is anchored at the tree root (`ci/run.sh`); a
//!   leading `/` anchors a single name (`/Makefile`);
//! - `*` and `?` match within one path segment, a whole `**` segment matches any
//!   number of segments;
//! - a covered directory covers everything beneath it. A trailing `/` only reads
//!   as "directory" — matching ignores the entry type, so a file or symlink the
//!   worker put where a protected directory was is covered too.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};

/// Covered unless `--no-default-protect`: test trees and test files, test-runner
/// config, CI config, and rubrics — the usual places a grade is decided.
pub(crate) const DEFAULT_PROTECTED: &[&str] = &[
    // Test trees and fixtures.
    "tests/",
    "test/",
    "spec/",
    "__tests__/",
    "testdata/",
    // Test files that live next to the code.
    "*_test.go",
    "test_*.py",
    "*_test.py",
    "*.test.*",
    "*.spec.*",
    // Test-runner config that can skip or force-pass tests.
    "conftest.py",
    "pytest.ini",
    "tox.ini",
    "jest.config.*",
    "vitest.config.*",
    // CI config.
    ".github/",
    ".gitlab-ci.yml",
    ".circleci/",
    ".buildkite/",
    ".travis.yml",
    "azure-pipelines.yml",
    "Jenkinsfile",
    // Rubrics.
    "rubrics/",
    "*.rubric",
];

#[derive(Debug, Clone)]
struct Pattern {
    /// Glob segments, anchored at the root. An unanchored name is stored as
    /// `["**", name]`.
    segs: Vec<String>,
}

impl Pattern {
    fn parse(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        let body = trimmed.trim_end_matches('/');
        if body.is_empty() {
            bail!("protected-path pattern `{raw}` is empty");
        }
        let anchored = body.contains('/');
        let body = body.trim_start_matches('/');
        let mut segs = Vec::new();
        for seg in body.split('/') {
            if seg.is_empty() || seg == "." || seg == ".." {
                bail!("protected-path pattern `{raw}` has an empty, `.` or `..` segment");
            }
            segs.push(seg.to_string());
        }
        if !anchored {
            segs.insert(0, "**".into());
        }
        Ok(Self { segs })
    }

    fn matches(&self, path: &[String]) -> bool {
        match_segs(&self.segs, path)
    }
}

fn match_segs(pat: &[String], path: &[String]) -> bool {
    match pat.split_first() {
        None => path.is_empty(),
        Some((p, rest)) if p == "**" => (0..=path.len()).any(|i| match_segs(rest, &path[i..])),
        Some((p, rest)) => match path.split_first() {
            Some((name, tail)) => match_glob(p, name) && match_segs(rest, tail),
            None => false,
        },
    }
}

/// `*` / `?` wildcard match of one path segment.
fn match_glob(pat: &str, name: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ni));
            pi += 1;
        } else if let Some((sp, sn)) = star {
            pi = sp + 1;
            ni = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// The set of protected-path patterns a grade tree is restored against.
#[derive(Debug, Clone, Default)]
pub(crate) struct ProtectSet {
    patterns: Vec<Pattern>,
}

impl ProtectSet {
    /// [`DEFAULT_PROTECTED`] (when `defaults`) plus `extra`, each validated.
    pub(crate) fn new(defaults: bool, extra: &[String]) -> Result<Self> {
        let base: &[&str] = if defaults { DEFAULT_PROTECTED } else { &[] };
        let patterns = base
            .iter()
            .copied()
            .chain(extra.iter().map(String::as_str))
            .map(Pattern::parse)
            .collect::<Result<_>>()?;
        Ok(Self { patterns })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Whether `rel` (relative to the tree root) is protected: it, or one of
    /// its ancestors, matches a pattern.
    pub(crate) fn covers(&self, rel: &Path) -> bool {
        let segs: Vec<String> = rel
            .components()
            .filter_map(|c| match c {
                Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect();
        (1..=segs.len()).any(|n| self.patterns.iter().any(|p| p.matches(&segs[..n])))
    }
}

/// What a restore changed in the grade tree.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct RestoreReport {
    /// Paths (relative, sorted) where the worker's tree differed from the base
    /// inside the protected set — edited, added, deleted or retyped — plus any
    /// directory on the way to a protected path that the worker had replaced
    /// with a symlink or file. Empty when the worker left the set untouched.
    pub(crate) reverted: Vec<PathBuf>,
}

#[derive(Debug, PartialEq, Eq)]
enum Node {
    Dir,
    File { len: u64, mode: u32 },
    Symlink(PathBuf),
    Other,
}

/// Every protected entry under one root, without following symlinks.
#[derive(Default)]
struct Protected {
    nodes: BTreeMap<PathBuf, Node>,
    /// Covered entries whose parent is not covered — the units removed/copied.
    tops: Vec<PathBuf>,
}

fn collect(root: &Path, set: &ProtectSet) -> Result<Protected> {
    let mut out = Protected::default();
    let mut stack: Vec<(PathBuf, bool)> = vec![(PathBuf::new(), false)];
    while let Some((dir_rel, dir_covered)) = stack.pop() {
        let dir = root.join(&dir_rel);
        let entries = std::fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("entry under {}", dir.display()))?;
            let rel = dir_rel.join(entry.file_name());
            let covered = dir_covered || set.covers(&rel);
            let meta = std::fs::symlink_metadata(entry.path())
                .with_context(|| format!("stat {}", entry.path().display()))?;
            let ft = meta.file_type();
            if covered {
                let node = if ft.is_symlink() {
                    Node::Symlink(std::fs::read_link(entry.path())?)
                } else if ft.is_dir() {
                    Node::Dir
                } else if ft.is_file() {
                    Node::File {
                        len: meta.len(),
                        mode: meta.permissions().mode() & 0o7777,
                    }
                } else {
                    Node::Other
                };
                if !dir_covered {
                    out.tops.push(rel.clone());
                }
                out.nodes.insert(rel.clone(), node);
            }
            if ft.is_dir() {
                stack.push((rel, covered));
            }
        }
    }
    out.tops.sort();
    Ok(out)
}

fn same_file_bytes(a: &Path, b: &Path) -> Result<bool> {
    let x = std::fs::read(a).with_context(|| format!("read {}", a.display()))?;
    let y = std::fs::read(b).with_context(|| format!("read {}", b.display()))?;
    Ok(x == y)
}

/// Put every path `set` covers in `tree` back to exactly what `base` holds:
/// the worker's protected entries are removed, then the base's are copied in.
/// Symlinks are never followed, on either side.
pub(crate) fn restore_protected(
    tree: &Path,
    base: &Path,
    set: &ProtectSet,
) -> Result<RestoreReport> {
    if set.is_empty() {
        return Ok(RestoreReport::default());
    }
    let worker = collect(tree, set)?;
    let reference = collect(base, set)?;

    let mut reverted: BTreeSet<PathBuf> = BTreeSet::new();
    let keys: BTreeSet<&PathBuf> = worker.nodes.keys().chain(reference.nodes.keys()).collect();
    for rel in keys {
        let differs = match (worker.nodes.get(rel), reference.nodes.get(rel)) {
            (Some(w @ Node::File { .. }), Some(r @ Node::File { .. })) => {
                w != r || !same_file_bytes(&tree.join(rel), &base.join(rel))?
            }
            (Some(w), Some(r)) => w != r,
            _ => true,
        };
        if differs {
            reverted.insert(rel.clone());
        }
    }

    for rel in &worker.tops {
        force_remove(&tree.join(rel))?;
    }
    for rel in &reference.tops {
        if let Some(parent) = rel.parent() {
            ensure_real_dirs(tree, parent, &mut reverted)?;
        }
        copy_node(&base.join(rel), &tree.join(rel))?;
    }
    Ok(RestoreReport {
        reverted: reverted.into_iter().collect(),
    })
}

/// Make every component of `rel` under `root` a real, writable directory. A
/// symlink or file in the way (the worker swapped a directory that holds
/// protected paths) is removed and recorded in `reverted`.
fn ensure_real_dirs(root: &Path, rel: &Path, reverted: &mut BTreeSet<PathBuf>) -> Result<()> {
    let mut cur = root.to_path_buf();
    let mut cur_rel = PathBuf::new();
    for comp in rel.components() {
        let Component::Normal(name) = comp else {
            bail!("unexpected component in protected path {}", rel.display());
        };
        make_owner_writable(&cur)?;
        cur.push(name);
        cur_rel.push(name);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_dir() => continue,
            Ok(_) => {
                force_remove(&cur)?;
                reverted.insert(cur_rel.clone());
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("stat {}", cur.display())),
        }
        std::fs::create_dir(&cur).with_context(|| format!("create {}", cur.display()))?;
    }
    make_owner_writable(&cur)
}

/// Copy one base entry (recursively for a directory) to `dst`, which must not
/// exist. Symlinks are recreated, never followed; modes are preserved (a
/// directory's after it is populated, so a read-only one still gets children).
fn copy_node(src: &Path, dst: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(src).with_context(|| format!("stat {}", src.display()))?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        let target = std::fs::read_link(src)?;
        std::os::unix::fs::symlink(&target, dst)
            .with_context(|| format!("symlink {}", dst.display()))?;
    } else if ft.is_dir() {
        std::fs::create_dir(dst).with_context(|| format!("create {}", dst.display()))?;
        for entry in std::fs::read_dir(src).with_context(|| format!("read {}", src.display()))? {
            let entry = entry?;
            copy_node(&entry.path(), &dst.join(entry.file_name()))?;
        }
        std::fs::set_permissions(dst, meta.permissions())?;
    } else if ft.is_file() {
        std::fs::copy(src, dst)
            .with_context(|| format!("copy {} → {}", src.display(), dst.display()))?;
    }
    Ok(())
}

fn make_owner_writable(dir: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(dir).with_context(|| format!("stat {}", dir.display()))?;
    let mode = meta.permissions().mode();
    if meta.file_type().is_dir() && mode & 0o700 != 0o700 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode | 0o700))
            .with_context(|| format!("chmod {}", dir.display()))?;
    }
    Ok(())
}

/// Remove `path` (never following a symlink), first making its parent and any
/// read-only directories inside it writable so a 0o555 tree can't block removal.
pub(crate) fn force_remove(path: &Path) -> Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("stat {}", path.display())),
    };
    if let Some(parent) = path.parent() {
        make_owner_writable(parent)?;
    }
    if meta.file_type().is_dir() {
        let mut stack = vec![path.to_path_buf()];
        while let Some(d) = stack.pop() {
            make_owner_writable(&d)?;
            for entry in std::fs::read_dir(&d).with_context(|| format!("read {}", d.display()))? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    stack.push(entry.path());
                }
            }
        }
        std::fs::remove_dir_all(path).with_context(|| format!("remove {}", path.display()))
    } else {
        std::fs::remove_file(path).with_context(|| format!("remove {}", path.display()))
    }
}

/// A scrubbed, protected-path-restored copy of a worker's workspace, removed on
/// drop. This — never the live workspace — is what a dispatch grader sees.
pub(crate) struct GradeTree {
    _tmp: tempfile::TempDir,
    path: PathBuf,
    pub(crate) report: RestoreReport,
}

impl GradeTree {
    /// CoW-clone `workspace`, scrub secrets (the same denylist as a worker's
    /// clone), then restore `set` from `base`.
    pub(crate) fn prepare(workspace: &Path, base: &Path, set: &ProtectSet) -> Result<Self> {
        let tmp = tempfile::Builder::new()
            .prefix("pillbox-grade-")
            .tempdir()
            .context("temp dir for the grade tree")?;
        let path = tmp.path().join("ws");
        super::cow::cow_clone_dir(workspace, &path).with_context(|| {
            format!("copy worker workspace {} for grading", workspace.display())
        })?;
        super::ingest::scrub_secrets(&path)?;
        let report = restore_protected(&path, base, set)?;
        Ok(Self {
            _tmp: tmp,
            path,
            report,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for GradeTree {
    fn drop(&mut self) {
        let _ = force_remove(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    fn defaults() -> ProtectSet {
        ProtectSet::new(true, &[]).unwrap()
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    /// A base tree and a worker copy of it, side by side.
    fn base_and_copy() -> (TempDir, PathBuf, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("base");
        write(&base, "src/lib.rs", "pub fn f() -> u8 { 0 }\n");
        write(&base, "tests/check.sh", "exit 1\n");
        write(&base, "tests/data/input.txt", "42\n");
        write(&base, ".github/workflows/ci.yml", "on: push\n");
        write(&base, "pkg/tests/inner.sh", "exit 1\n");
        let copy = tmp.path().join("copy");
        crate::workspace::cow::cow_clone_dir(&base, &copy).unwrap();
        (tmp, base, copy)
    }

    fn read(root: &Path, rel: &str) -> String {
        fs::read_to_string(root.join(rel)).unwrap()
    }

    #[test]
    fn unanchored_names_match_at_any_depth() {
        let s = defaults();
        assert!(s.covers(Path::new("tests")));
        assert!(s.covers(Path::new("tests/a/b.rs")));
        assert!(s.covers(Path::new("crates/x/tests/it.rs")));
        assert!(s.covers(Path::new("pkg/foo_test.go")));
        assert!(s.covers(Path::new("web/app.test.tsx")));
        assert!(s.covers(Path::new(".github/workflows/ci.yml")));
        assert!(s.covers(Path::new("rubrics/grade.txt")));
        assert!(!s.covers(Path::new("src/lib.rs")));
        assert!(!s.covers(Path::new("src/testing.rs")));
        assert!(!s.covers(Path::new("contest.py")));
    }

    #[test]
    fn anchored_patterns_and_globs() {
        let s = ProtectSet::new(
            false,
            &[
                "ci/run.sh".into(),
                "/Makefile".into(),
                "docs/**/golden.*".into(),
            ],
        )
        .unwrap();
        assert!(s.covers(Path::new("ci/run.sh")));
        assert!(!s.covers(Path::new("x/ci/run.sh")));
        assert!(s.covers(Path::new("Makefile")));
        assert!(!s.covers(Path::new("sub/Makefile")));
        assert!(s.covers(Path::new("docs/golden.md")));
        assert!(s.covers(Path::new("docs/a/b/golden.json")));
        assert!(!s.covers(Path::new("docs/a/silver.json")));
    }

    #[test]
    fn rejects_bad_patterns() {
        for bad in ["", "/", "a//b", "../x", "a/./b"] {
            assert!(
                ProtectSet::new(false, &[bad.to_string()]).is_err(),
                "`{bad}` should be rejected"
            );
        }
    }

    #[test]
    fn no_default_protect_is_empty() {
        let s = ProtectSet::new(false, &[]).unwrap();
        assert!(s.is_empty());
        assert!(!s.covers(Path::new("tests/check.sh")));
    }

    #[test]
    fn untouched_tree_reports_nothing() {
        let (_t, base, copy) = base_and_copy();
        let r = restore_protected(&copy, &base, &defaults()).unwrap();
        assert!(r.reverted.is_empty(), "{:?}", r.reverted);
        assert_eq!(read(&copy, "tests/check.sh"), "exit 1\n");
    }

    #[test]
    fn edited_protected_file_is_reverted_and_code_kept() {
        let (_t, base, copy) = base_and_copy();
        write(&copy, "tests/check.sh", "exit 0\n");
        write(&copy, "src/lib.rs", "pub fn f() -> u8 { 1 }\n");
        let r = restore_protected(&copy, &base, &defaults()).unwrap();
        assert_eq!(read(&copy, "tests/check.sh"), "exit 1\n");
        assert_eq!(read(&copy, "src/lib.rs"), "pub fn f() -> u8 { 1 }\n");
        assert_eq!(r.reverted, vec![PathBuf::from("tests/check.sh")]);
    }

    #[test]
    fn deleted_protected_paths_come_back() {
        let (_t, base, copy) = base_and_copy();
        fs::remove_file(copy.join("tests/check.sh")).unwrap();
        fs::remove_dir_all(copy.join(".github")).unwrap();
        fs::remove_dir_all(copy.join("pkg/tests")).unwrap();
        restore_protected(&copy, &base, &defaults()).unwrap();
        assert_eq!(read(&copy, "tests/check.sh"), "exit 1\n");
        assert_eq!(read(&copy, ".github/workflows/ci.yml"), "on: push\n");
        assert_eq!(read(&copy, "pkg/tests/inner.sh"), "exit 1\n");
    }

    #[test]
    fn whole_protected_tree_deleted_with_parent_comes_back() {
        let (_t, base, copy) = base_and_copy();
        fs::remove_dir_all(copy.join("pkg")).unwrap();
        restore_protected(&copy, &base, &defaults()).unwrap();
        assert_eq!(read(&copy, "pkg/tests/inner.sh"), "exit 1\n");
    }

    #[test]
    fn added_protected_files_are_removed() {
        let (_t, base, copy) = base_and_copy();
        write(&copy, "tests/extra.sh", "exit 0\n");
        write(&copy, "src/foo_test.go", "package src\n");
        let r = restore_protected(&copy, &base, &defaults()).unwrap();
        assert!(!copy.join("tests/extra.sh").exists());
        assert!(!copy.join("src/foo_test.go").exists());
        assert!(r.reverted.contains(&PathBuf::from("tests/extra.sh")));
        assert!(r.reverted.contains(&PathBuf::from("src/foo_test.go")));
    }

    #[test]
    fn renames_within_and_out_of_the_set_are_undone() {
        let (_t, base, copy) = base_and_copy();
        // Rename inside the set: check.sh → renamed.sh.
        fs::rename(copy.join("tests/check.sh"), copy.join("tests/renamed.sh")).unwrap();
        // Rename the whole protected dir away, out of the set.
        fs::rename(copy.join(".github"), copy.join("old-ci")).unwrap();
        restore_protected(&copy, &base, &defaults()).unwrap();
        assert_eq!(read(&copy, "tests/check.sh"), "exit 1\n");
        assert!(!copy.join("tests/renamed.sh").exists());
        assert_eq!(read(&copy, ".github/workflows/ci.yml"), "on: push\n");
        // Outside the set the worker's rename target survives, as worker code.
        assert!(copy.join("old-ci/workflows/ci.yml").exists());
    }

    #[test]
    fn rename_into_the_set_is_dropped() {
        let (_t, base, copy) = base_and_copy();
        fs::rename(copy.join("src/lib.rs"), copy.join("tests/lib.rs")).unwrap();
        restore_protected(&copy, &base, &defaults()).unwrap();
        assert!(!copy.join("tests/lib.rs").exists());
        assert!(
            !copy.join("src/lib.rs").exists(),
            "outside the set, the worker's deletion stands"
        );
    }

    #[test]
    fn protected_file_swapped_for_symlink_is_restored_as_a_file() {
        let (_t, base, copy) = base_and_copy();
        write(&copy, "src/pass.sh", "exit 0\n");
        fs::remove_file(copy.join("tests/check.sh")).unwrap();
        symlink("../src/pass.sh", copy.join("tests/check.sh")).unwrap();
        restore_protected(&copy, &base, &defaults()).unwrap();
        let meta = fs::symlink_metadata(copy.join("tests/check.sh")).unwrap();
        assert!(meta.file_type().is_file());
        assert_eq!(read(&copy, "tests/check.sh"), "exit 1\n");
        assert_eq!(read(&copy, "src/pass.sh"), "exit 0\n");
    }

    #[test]
    fn protected_dir_swapped_for_symlink_does_not_redirect_the_restore() {
        let (tmp, base, copy) = base_and_copy();
        // The worker points `tests` at a dir outside the copy; restoring through
        // the link would overwrite that dir.
        let outside = tmp.path().join("outside");
        write(&outside, "check.sh", "exit 0\n");
        fs::remove_dir_all(copy.join("tests")).unwrap();
        symlink(&outside, copy.join("tests")).unwrap();
        restore_protected(&copy, &base, &defaults()).unwrap();
        assert!(fs::symlink_metadata(copy.join("tests"))
            .unwrap()
            .file_type()
            .is_dir());
        assert_eq!(read(&copy, "tests/check.sh"), "exit 1\n");
        assert_eq!(read(&outside, "check.sh"), "exit 0\n", "outside untouched");
    }

    #[test]
    fn unprotected_ancestor_swapped_for_symlink_is_replaced() {
        let (tmp, base, copy) = base_and_copy();
        // `pkg` is not protected but holds `pkg/tests`. A symlink there would
        // send the restore of pkg/tests/ to the link target.
        let outside = tmp.path().join("elsewhere");
        write(&outside, "tests/inner.sh", "exit 0\n");
        fs::remove_dir_all(copy.join("pkg")).unwrap();
        symlink(&outside, copy.join("pkg")).unwrap();
        let r = restore_protected(&copy, &base, &defaults()).unwrap();
        assert!(fs::symlink_metadata(copy.join("pkg"))
            .unwrap()
            .file_type()
            .is_dir());
        assert_eq!(read(&copy, "pkg/tests/inner.sh"), "exit 1\n");
        assert_eq!(
            read(&outside, "tests/inner.sh"),
            "exit 0\n",
            "link target untouched"
        );
        assert!(r.reverted.contains(&PathBuf::from("pkg")));
    }

    #[test]
    fn base_symlinks_are_restored_as_symlinks() {
        let (_t, base, copy) = base_and_copy();
        symlink("data/input.txt", base.join("tests/link")).unwrap();
        restore_protected(&copy, &base, &defaults()).unwrap();
        assert_eq!(
            fs::read_link(copy.join("tests/link")).unwrap(),
            Path::new("data/input.txt")
        );
    }

    #[test]
    fn read_only_protected_dirs_do_not_block_the_restore() {
        let (_t, base, copy) = base_and_copy();
        write(&copy, "tests/data/extra.txt", "x\n");
        fs::set_permissions(copy.join("tests/data"), fs::Permissions::from_mode(0o555)).unwrap();
        fs::set_permissions(copy.join("tests"), fs::Permissions::from_mode(0o555)).unwrap();
        restore_protected(&copy, &base, &defaults()).unwrap();
        assert!(!copy.join("tests/data/extra.txt").exists());
        assert_eq!(read(&copy, "tests/data/input.txt"), "42\n");
    }

    #[test]
    fn mode_change_counts_as_tampering() {
        let (_t, base, copy) = base_and_copy();
        fs::set_permissions(
            copy.join("tests/check.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let r = restore_protected(&copy, &base, &defaults()).unwrap();
        assert_eq!(r.reverted, vec![PathBuf::from("tests/check.sh")]);
        let mode = fs::metadata(copy.join("tests/check.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            fs::metadata(base.join("tests/check.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        );
    }

    #[test]
    fn grade_tree_is_a_copy_and_leaves_the_workspace_alone() {
        let (_t, base, worker) = base_and_copy();
        write(&worker, "tests/check.sh", "exit 0\n");
        write(&worker, ".env", "SECRET=1\n");
        let tree = GradeTree::prepare(&worker, &base, &defaults()).unwrap();
        assert_eq!(read(tree.path(), "tests/check.sh"), "exit 1\n");
        assert!(!tree.path().join(".env").exists(), "secrets scrubbed");
        assert_eq!(
            read(&worker, "tests/check.sh"),
            "exit 0\n",
            "live workspace untouched"
        );
        assert_eq!(tree.report.reverted, vec![PathBuf::from("tests/check.sh")]);
        let p = tree.path().to_path_buf();
        drop(tree);
        assert!(!p.exists(), "removed on drop");
    }
}
