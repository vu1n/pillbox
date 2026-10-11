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
//! The walks never follow a symlink in the worker's tree, and every directory
//! the restore writes through is checked to be a real directory, so a
//! worker-planted link cannot redirect the restore outside the copy. A base
//! link inside the set that points at an unprotected path elsewhere in the base
//! is copied as the base target's content, so the worker's copy of that target
//! cannot decide the grade.
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
use std::ffi::{CStr, CString};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

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
    "*_test.rs",
    "test_*.py",
    "*_test.py",
    "*.test.*",
    "*.spec.*",
    // Test-runner config that can skip or force-pass tests.
    "conftest.py",
    "pytest.ini",
    "tox.ini",
    ".cargo/",
    "jest.config.*",
    "vitest.config.*",
    // Root-level modules that would shadow a Python test runner: `python3 -m
    // pytest` puts the workspace first on sys.path, so a worker's `pytest.py`
    // would run instead of the real one. (Not PYTHONSAFEPATH: that also stops
    // tests importing the project from the root, which `-m pytest` relies on.)
    "/pytest.py",
    "/pytest/",
    "/_pytest/",
    "/unittest.py",
    "/unittest/",
    "sitecustomize.py",
    "usercustomize.py",
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
        // A 0o000 directory is still owned by us: chmod before listing, or one
        // unreadable dir turns the grade into an error instead of a verdict.
        make_owner_writable(&dir)?;
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
    let root =
        std::fs::canonicalize(base).with_context(|| format!("resolve {}", base.display()))?;
    for rel in &reference.tops {
        if let Some(parent) = rel.parent() {
            ensure_real_dirs(tree, parent, &mut reverted)?;
        }
        copy_protected(
            &root,
            set,
            &base.join(rel),
            &tree.join(rel),
            rel,
            &mut Vec::new(),
        )?;
    }
    Ok(RestoreReport {
        reverted: reverted.into_iter().collect(),
    })
}

/// Copy the protected base entry `src` to `dst` like [`copy_node`], except
/// that a symlink resolving to an unprotected path elsewhere in the base
/// (`tests -> shared-tests`, `tests/unit -> ../shared-tests`) is materialized
/// from the base target, at any depth. Restoring just the link would let the
/// worker's copy of the target decide the grade. A link is copied as a link
/// only when, read as text from where it lands in the grade tree (`at`), it
/// names exactly its protected host target: no `..` above the root and no
/// symlink on the way, either of which the worker could steer. A dangling
/// link, or one that leaves the bookmark, is an error, since the grader VM
/// mounts the tree elsewhere and could resolve it to worker files. `active`
/// holds the directories being copied, so a link back into one is an error
/// rather than an endless copy.
fn copy_protected(
    root: &Path,
    set: &ProtectSet,
    src: &Path,
    dst: &Path,
    at: &Path,
    active: &mut Vec<PathBuf>,
) -> Result<()> {
    let meta = std::fs::symlink_metadata(src).with_context(|| format!("stat {}", src.display()))?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        let Some(target) = in_tree_target(root, src) else {
            bail!(
                "protected path {} is a symlink that does not resolve inside the bookmark; \
                 the grader VM could resolve it to worker files, so replace it with its \
                 content or remove it",
                src.display()
            );
        };
        let rel = target.strip_prefix(root).unwrap_or(&target);
        if set.covers(rel) && textual_target(at, src)?.as_deref() == Some(rel) {
            return copy_node(src, dst);
        }
        if active.contains(&target) {
            bail!(
                "protected path {} links back into {}; protect the target or break the cycle",
                src.display(),
                target.display()
            );
        }
        return copy_protected(root, set, &target, dst, at, active);
    }
    if !ft.is_dir() {
        return copy_node(src, dst);
    }
    let here = std::fs::canonicalize(src).with_context(|| format!("resolve {}", src.display()))?;
    active.push(here);
    std::fs::create_dir(dst).with_context(|| format!("create {}", dst.display()))?;
    for entry in std::fs::read_dir(src).with_context(|| format!("read {}", src.display()))? {
        let entry = entry?;
        copy_protected(
            root,
            set,
            &entry.path(),
            &dst.join(entry.file_name()),
            &at.join(entry.file_name()),
            active,
        )?;
    }
    std::fs::set_permissions(dst, meta.permissions())?;
    active.pop();
    Ok(())
}

/// Where the symlink `link` points on the host, when that is a path strictly
/// inside `root` (the canonical base).
fn in_tree_target(root: &Path, link: &Path) -> Option<PathBuf> {
    let target = std::fs::canonicalize(link).ok()?;
    let rel = target.strip_prefix(root).ok()?;
    (!rel.as_os_str().is_empty()).then_some(target)
}

/// The tree-relative path the symlink `link`, landing at `at`, names when its
/// text is resolved lexically from `at`'s directory; `None` when it is absolute
/// or climbs above the root. Lexical means no symlink is followed on the way.
fn textual_target(at: &Path, link: &Path) -> Result<Option<PathBuf>> {
    let text = std::fs::read_link(link).with_context(|| format!("read {}", link.display()))?;
    let mut out: Vec<&std::ffi::OsStr> =
        at.parent().map(|p| p.iter().collect()).unwrap_or_default();
    for comp in text.components() {
        match comp {
            Component::Normal(name) => out.push(name),
            Component::CurDir => {}
            Component::ParentDir if out.pop().is_some() => {}
            _ => return Ok(None),
        }
    }
    Ok(Some(out.iter().collect()))
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

/// Relaxed directory modes, put back when this drops — including on failure.
///
/// This touches the worker's LIVE tree, which a still-running worker can change
/// underneath it, so below the root every step is relative to an open directory
/// and never follows a symlink: a directory raced into a link to a host path
/// gets its mode change refused (or applied to the link itself), never to the
/// path it names.
struct ModeGuard {
    root: Option<(PathBuf, u32)>,
    saved: Vec<(Rc<OwnedFd>, CString, u32)>,
}

impl ModeGuard {
    /// Make every real directory under `root` owner-traversable, remembering
    /// the modes to restore. Symlinks are not followed.
    fn relax(root: &Path) -> Result<Self> {
        let mut guard = Self {
            root: None,
            saved: Vec::new(),
        };
        let meta =
            std::fs::symlink_metadata(root).with_context(|| format!("stat {}", root.display()))?;
        if !meta.file_type().is_dir() {
            return Ok(guard);
        }
        let mode = meta.permissions().mode();
        if mode & 0o700 != 0o700 {
            std::fs::set_permissions(root, std::fs::Permissions::from_mode(mode | 0o700))
                .with_context(|| format!("chmod {}", root.display()))?;
            guard.root = Some((root.to_path_buf(), mode));
        }
        let c_root = CString::new(root.as_os_str().as_bytes())
            .with_context(|| format!("path with NUL {}", root.display()))?;
        let fd = open_dir_at(libc::AT_FDCWD, &c_root)
            .with_context(|| format!("open {}", root.display()))?;
        relax_dirs_at(&Rc::new(fd), root, &mut guard.saved)?;
        Ok(guard)
    }
}

impl Drop for ModeGuard {
    fn drop(&mut self) {
        // Children first: restoring a parent's 0o000 mode before its children
        // would make the rest unrestorable.
        for (dir, name, mode) in self.saved.iter().rev() {
            let _ = chmod_at(dir, name, *mode);
        }
        if let Some((root, mode)) = &self.root {
            let _ = std::fs::set_permissions(root, std::fs::Permissions::from_mode(*mode));
        }
    }
}

const DIR_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

fn open_dir_at(dir: libc::c_int, name: &CStr) -> std::io::Result<OwnedFd> {
    let fd = unsafe { libc::openat(dir, name.as_ptr(), DIR_FLAGS) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `chmod` of the entry `name` in `dir`, never through a symlink.
fn chmod_at(dir: &OwnedFd, name: &CStr, mode: u32) -> std::io::Result<()> {
    let rc = unsafe {
        libc::fchmodat(
            dir.as_raw_fd(),
            name.as_ptr(),
            mode as libc::mode_t,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// The names in the open directory `dir`, without `.` and `..`.
fn dir_names(dir: &OwnedFd) -> std::io::Result<Vec<CString>> {
    let dup = unsafe { libc::fcntl(dir.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if dup < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let stream = unsafe { libc::fdopendir(dup) };
    if stream.is_null() {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(dup) };
        return Err(err);
    }
    let mut names = Vec::new();
    loop {
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name != c"." && name != c".." {
            names.push(name.to_owned());
        }
    }
    unsafe { libc::closedir(stream) };
    Ok(names)
}

fn relax_dirs_at(
    dir: &Rc<OwnedFd>,
    path: &Path,
    saved: &mut Vec<(Rc<OwnedFd>, CString, u32)>,
) -> Result<()> {
    for name in dir_names(dir).with_context(|| format!("read {}", path.display()))? {
        let child = path.join(std::ffi::OsStr::from_bytes(name.as_bytes()));
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::fstatat(
                dir.as_raw_fd(),
                name.as_ptr(),
                &mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("stat {}", child.display()));
        }
        if st.st_mode & libc::S_IFMT != libc::S_IFDIR {
            continue;
        }
        #[allow(clippy::useless_conversion)] // `mode_t` is u16 on macOS
        let mode = u32::from(st.st_mode) & 0o7777;
        if mode & 0o700 != 0o700 {
            chmod_at(dir, &name, mode | 0o700)
                .with_context(|| format!("chmod {}", child.display()))?;
            saved.push((Rc::clone(dir), name.clone(), mode));
        }
        // A directory swapped for a link since the stat is not descended into.
        match open_dir_at(dir.as_raw_fd(), &name) {
            Ok(fd) => relax_dirs_at(&Rc::new(fd), &child, saved)?,
            Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) => {}
            Err(e) => return Err(e).with_context(|| format!("open {}", child.display())),
        }
    }
    Ok(())
}

/// After the restore: protected files and directories lose every write bit
/// (the owner included — that is who the virtiofs server writes as), and
/// everything else becomes world-writable so the unprivileged grader can
/// emit build output. Symlinks are not followed, so a protected link cannot
/// change the mode of the file it names.
///
/// Every directory on the way to a protected entry also gets the sticky bit:
/// its own write bit stays (build output can land there), but `nobody` can no
/// longer rename or unlink an entry it doesn't own, so worker code running
/// during the grade (`build.rs`) can't move a restored `tests/` aside and put
/// a passing one in its place. Only those ancestors: a sticky `target/` would
/// stop the build replacing artifacts the host user owns.
fn lock_grade_tree(root: &Path, set: &ProtectSet) -> Result<()> {
    if set.is_empty() {
        return Ok(());
    }
    let mut sticky = BTreeSet::new();
    for top in collect(root, set)?.tops {
        sticky.extend(top.ancestors().skip(1).map(Path::to_path_buf));
    }
    lock_node(root, root, set, &sticky)
}

fn lock_node(root: &Path, path: &Path, set: &ProtectSet, sticky: &BTreeSet<PathBuf>) -> Result<()> {
    let meta =
        std::fs::symlink_metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        return Ok(());
    }
    let rel = path.strip_prefix(root).unwrap_or(path);
    let covered = rel != Path::new("") && set.covers(rel);
    // On macOS the guest's chmod lives in the override xattr, not the backing
    // mode: keep the exec bit the worker gave a file, never its owner or set-ID.
    let shown = guest_override_mode(path).unwrap_or(meta.permissions().mode());
    let exec = shown & 0o111 != 0;
    let mode = if ft.is_dir() {
        if covered {
            0o555
        } else if sticky.contains(rel) {
            0o1777
        } else {
            0o777
        }
    } else if covered {
        if exec {
            0o555
        } else {
            0o444
        }
    } else if exec {
        0o777
    } else {
        0o666
    };
    if ft.is_file() || ft.is_dir() {
        let chmod = |m: u32| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(m))
                .with_context(|| format!("chmod {}", path.display()))
        };
        // Removing a user xattr needs write access to the inode.
        chmod(mode | 0o700)?;
        clear_guest_override(path)?;
        chmod(mode)?;
    }
    if ft.is_dir() {
        for entry in std::fs::read_dir(path).with_context(|| format!("read {}", path.display()))? {
            lock_node(root, &entry?.path(), set, sticky)?;
        }
    }
    Ok(())
}

/// The xattr through which macOS libkrun shows the guest a mode and owner other
/// than the backing file's. The clone keeps the worker's copy of it, so without
/// clearing it a guest `chmod 0777 .` would outlive [`lock_grade_tree`], and a
/// root-owned setuid file would hand the `nobody` grader back root.
const GUEST_OVERRIDE_XATTR: &std::ffi::CStr = c"user.containers.override_stat";

/// The mode `path`'s guest stat override shows (`uid:gid:mode`, mode in octal),
/// if it has one that sets a mode.
fn guest_override_mode(path: &Path) -> Option<u32> {
    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut value = [0u8; 64];
    #[cfg(target_os = "macos")]
    let n = unsafe {
        libc::getxattr(
            c_path.as_ptr(),
            GUEST_OVERRIDE_XATTR.as_ptr(),
            value.as_mut_ptr().cast(),
            value.len(),
            0,
            libc::XATTR_NOFOLLOW,
        )
    };
    #[cfg(not(target_os = "macos"))]
    let n = unsafe {
        libc::lgetxattr(
            c_path.as_ptr(),
            GUEST_OVERRIDE_XATTR.as_ptr(),
            value.as_mut_ptr().cast(),
            value.len(),
        )
    };
    let text = std::str::from_utf8(value.get(..usize::try_from(n).ok()?)?).ok()?;
    u32::from_str_radix(text.split(':').nth(2)?, 8).ok()
}

/// Drop `path`'s guest stat override, so the grader VM sees the locked backing
/// mode. Fails closed on anything but "not set" or "xattrs unsupported here".
fn clear_guest_override(path: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("path with NUL {}", path.display()))?;
    #[cfg(target_os = "macos")]
    let rc = unsafe {
        libc::removexattr(
            c_path.as_ptr(),
            GUEST_OVERRIDE_XATTR.as_ptr(),
            libc::XATTR_NOFOLLOW,
        )
    };
    #[cfg(not(target_os = "macos"))]
    let rc = unsafe { libc::lremovexattr(c_path.as_ptr(), GUEST_OVERRIDE_XATTR.as_ptr()) };
    if rc == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    #[cfg(target_os = "macos")]
    let absent = libc::ENOATTR;
    #[cfg(not(target_os = "macos"))]
    let absent = libc::ENODATA;
    match err.raw_os_error() {
        Some(code) if code == absent || code == libc::ENOTSUP => Ok(()),
        _ => Err(err).with_context(|| format!("clear guest metadata on {}", path.display())),
    }
}

/// Where `dispatch --grader-files DIR` lands in a grade tree: hidden grader
/// files the worker never sees, referenced by the reward as
/// `.pillbox-grader/…`. Dispatch protects this path, so a worker's own copy is
/// removed and the injected one is locked like any protected path.
pub(crate) const GRADER_FILES_DIR: &str = ".pillbox-grader";

/// Copy the operator's hidden grader files into `tree` at [`GRADER_FILES_DIR`],
/// replacing anything there. Symlinks in `src` are recreated, not followed.
pub(crate) fn inject_grader_files(tree: &Path, src: &Path) -> Result<()> {
    // Resolve a symlinked DIR itself: copying the link would leave a host path
    // (or a link relative to the grade tree) that doesn't exist in the VM.
    let src = std::fs::canonicalize(src)
        .with_context(|| format!("resolve grader files {}", src.display()))?;
    let dst = tree.join(GRADER_FILES_DIR);
    force_remove(&dst)?;
    copy_grader_files(&src, &src, &dst, &mut Vec::new())
        .with_context(|| format!("inject grader files from {}", src.display()))
}

/// Copy the grader files under `root` (canonical) with every symlink replaced by
/// its target's content. A link copied as a link would resolve from inside the
/// grade tree, where the worker controls what it names; a link that leaves
/// `root`, dangles, or loops is an error.
fn copy_grader_files(root: &Path, src: &Path, dst: &Path, active: &mut Vec<PathBuf>) -> Result<()> {
    let meta = std::fs::symlink_metadata(src).with_context(|| format!("stat {}", src.display()))?;
    if meta.file_type().is_symlink() {
        let target = std::fs::canonicalize(src)
            .ok()
            .filter(|t| t.starts_with(root))
            .with_context(|| {
                format!(
                    "grader file {} is a symlink that does not resolve inside {}",
                    src.display(),
                    root.display()
                )
            })?;
        if active.contains(&target) {
            bail!(
                "grader file {} links back into {}",
                src.display(),
                target.display()
            );
        }
        return copy_grader_files(root, &target, dst, active);
    }
    if !meta.file_type().is_dir() {
        return copy_node(src, dst);
    }
    active.push(std::fs::canonicalize(src).with_context(|| format!("resolve {}", src.display()))?);
    std::fs::create_dir(dst).with_context(|| format!("create {}", dst.display()))?;
    for entry in std::fs::read_dir(src).with_context(|| format!("read {}", src.display()))? {
        let entry = entry?;
        copy_grader_files(root, &entry.path(), &dst.join(entry.file_name()), active)?;
    }
    std::fs::set_permissions(dst, meta.permissions())?;
    active.pop();
    Ok(())
}

/// A scrubbed, protected-path-restored copy of a worker's workspace, removed on
/// drop. This — never the live workspace — is what a dispatch grader sees.
pub(crate) struct GradeTree {
    _tmp: tempfile::TempDir,
    path: PathBuf,
    pub(crate) report: RestoreReport,
    /// `set` was non-empty, so [`lock_grade_tree`] ran and the grader must
    /// drop privileges (root could otherwise chmod the lock off).
    pub(crate) locked: bool,
}

impl GradeTree {
    /// CoW-clone `workspace`, scrub secrets (the same denylist as a worker's
    /// clone), restore `set` from `base`, inject `grader_files` (if any), then
    /// lock protected paths against the owner. The live tree's modes are put
    /// back even if the copy fails.
    pub(crate) fn prepare(
        workspace: &Path,
        base: &Path,
        set: &ProtectSet,
        grader_files: Option<&Path>,
    ) -> Result<Self> {
        let tmp = tempfile::Builder::new()
            .prefix("pillbox-grade-")
            .tempdir()
            .context("temp dir for the grade tree")?;
        let path = tmp.path().join("ws");
        let modes = ModeGuard::relax(workspace)?;
        super::cow::cow_clone_dir(workspace, &path).with_context(|| {
            format!("copy worker workspace {} for grading", workspace.display())
        })?;
        drop(modes);
        super::ingest::scrub_secrets(&path)?;
        let report = restore_protected(&path, base, set)?;
        if let Some(src) = grader_files {
            inject_grader_files(&path, src)?;
        }
        lock_grade_tree(&path, set)?;
        Ok(Self {
            _tmp: tmp,
            path,
            report,
            locked: !set.is_empty(),
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
        assert!(s.covers(Path::new("src/foo_test.rs")));
        assert!(s.covers(Path::new(".cargo/config.toml")));
        assert!(s.covers(Path::new("web/app.test.tsx")));
        assert!(s.covers(Path::new(".github/workflows/ci.yml")));
        assert!(s.covers(Path::new("rubrics/grade.txt")));
        assert!(!s.covers(Path::new("src/lib.rs")));
        assert!(!s.covers(Path::new("src/testing.rs")));
        assert!(!s.covers(Path::new("contest.py")));
        assert!(s.covers(Path::new("pytest.py")));
        assert!(s.covers(Path::new("pytest/__init__.py")));
        assert!(s.covers(Path::new("_pytest/main.py")));
        assert!(s.covers(Path::new("src/sitecustomize.py")));
        assert!(
            !s.covers(Path::new("src/pytest.py")),
            "only a root shadow is protected"
        );
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
        let tree = GradeTree::prepare(&worker, &base, &defaults(), None).unwrap();
        assert_eq!(read(tree.path(), "tests/check.sh"), "exit 1\n");
        assert!(!tree.path().join(".env").exists(), "secrets scrubbed");
        assert_eq!(
            read(&worker, "tests/check.sh"),
            "exit 0\n",
            "live workspace untouched"
        );
        assert_eq!(tree.report.reverted, vec![PathBuf::from("tests/check.sh")]);
        assert!(tree.locked);
        let p = tree.path().to_path_buf();
        drop(tree);
        assert!(!p.exists(), "removed on drop");
    }

    #[test]
    fn a_protected_top_that_links_inside_the_base_is_materialized() {
        let (_t, base, copy) = base_and_copy();
        fs::remove_dir_all(base.join("tests")).unwrap();
        write(&base, "shared-tests/check.sh", "exit 1\n");
        std::os::unix::fs::symlink("shared-tests", base.join("tests")).unwrap();
        fs::remove_dir_all(copy.join("tests")).unwrap();
        write(&copy, "shared-tests/check.sh", "exit 0\n"); // worker edits the target
        std::os::unix::fs::symlink("shared-tests", copy.join("tests")).unwrap();
        restore_protected(&copy, &base, &defaults()).unwrap();
        let tests = copy.join("tests");
        assert!(fs::symlink_metadata(&tests).unwrap().file_type().is_dir());
        assert_eq!(
            read(&copy, "tests/check.sh"),
            "exit 1\n",
            "the base's test, not the worker's"
        );
    }

    #[test]
    fn a_nested_protected_link_into_the_base_is_materialized() {
        let (_t, base, copy) = base_and_copy();
        write(&base, "shared-tests/check.sh", "exit 1\n");
        symlink("../shared-tests", base.join("tests/unit")).unwrap();
        symlink("check.sh", base.join("tests/again")).unwrap(); // stays inside the set
        write(&copy, "shared-tests/check.sh", "exit 0\n"); // worker edits the target
        symlink("../shared-tests", copy.join("tests/unit")).unwrap();
        restore_protected(&copy, &base, &defaults()).unwrap();
        let unit = copy.join("tests/unit");
        assert!(fs::symlink_metadata(&unit).unwrap().file_type().is_dir());
        assert_eq!(read(&copy, "tests/unit/check.sh"), "exit 1\n");
        assert_eq!(
            fs::read_link(copy.join("tests/again")).unwrap(),
            Path::new("check.sh"),
            "a link to a protected path stays a link"
        );
    }

    #[test]
    fn a_protected_link_cycle_is_an_error_not_an_endless_copy() {
        let (_t, base, copy) = base_and_copy();
        write(&base, "shared/a.sh", "exit 1\n");
        symlink("../shared", base.join("shared/loop")).unwrap();
        symlink("../shared", base.join("tests/unit")).unwrap();
        let err = restore_protected(&copy, &base, &defaults()).unwrap_err();
        assert!(format!("{err:#}").contains("links back into"), "{err:#}");
    }

    #[test]
    fn a_protected_link_that_could_resolve_differently_in_the_vm_is_an_error() {
        for target in [
            "/grade/shared-tests",
            "../missing/check.sh",
            "../../outside",
        ] {
            let (_t, base, copy) = base_and_copy();
            symlink(target, base.join("tests/unit")).unwrap();
            let err = restore_protected(&copy, &base, &defaults()).unwrap_err();
            assert!(
                format!("{err:#}").contains("does not resolve inside the bookmark"),
                "{target}: {err:#}"
            );
        }
        // An absolute link into the bookmark is materialized, not kept.
        let (_t, base, copy) = base_and_copy();
        symlink(base.join("tests/check.sh"), base.join("tests/abs")).unwrap();
        restore_protected(&copy, &base, &defaults()).unwrap();
        let abs = copy.join("tests/abs");
        assert!(fs::symlink_metadata(&abs).unwrap().file_type().is_file());
        assert_eq!(read(&copy, "tests/abs"), "exit 1\n");
    }

    #[test]
    fn a_worker_guest_mode_override_does_not_survive_the_lock() {
        let (_t, base, worker) = base_and_copy();
        let set_value = |p: &Path, v: &[u8]| {
            use std::os::unix::ffi::OsStrExt;
            let c = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
            #[cfg(target_os = "macos")]
            let rc = unsafe {
                libc::setxattr(
                    c.as_ptr(),
                    GUEST_OVERRIDE_XATTR.as_ptr(),
                    v.as_ptr().cast(),
                    v.len(),
                    0,
                    0,
                )
            };
            #[cfg(not(target_os = "macos"))]
            let rc = unsafe {
                libc::setxattr(
                    c.as_ptr(),
                    GUEST_OVERRIDE_XATTR.as_ptr(),
                    v.as_ptr().cast(),
                    v.len(),
                    0,
                )
            };
            rc == 0
        };
        let set_override = |p: &Path| set_value(p, b"0:0:040777");
        // The worker's guest `chmod 0777 .` / `chmod 0777 tests`, and a
        // root-owned setuid binary outside the protected set.
        let check = worker.join("tests/check.sh");
        let suid = worker.join("src/lib.rs");
        if !set_override(&worker) || !set_override(&worker.join("tests")) || !set_override(&check) {
            eprintln!("skipping: user xattrs unsupported on this filesystem");
            return;
        }
        // Lock the worker tree in place: on Linux the CoW clone drops xattrs,
        // so going through GradeTree would not exercise the clear.
        let _ = &base;
        assert!(set_value(&suid, b"0:0:0104755"));
        assert_eq!(guest_override_mode(&suid), Some(0o104755));
        lock_grade_tree(&worker, &defaults()).unwrap();
        assert_eq!(mode_of(&suid), 0o777, "exec kept, set-ID and owner dropped");
        for rel in ["", "tests", "tests/check.sh", "src/lib.rs"] {
            let p = worker.join(rel);
            let c = std::ffi::CString::new(p.to_str().unwrap()).unwrap();
            #[cfg(target_os = "macos")]
            let n = unsafe {
                libc::getxattr(
                    c.as_ptr(),
                    GUEST_OVERRIDE_XATTR.as_ptr(),
                    std::ptr::null_mut(),
                    0,
                    0,
                    0,
                )
            };
            #[cfg(not(target_os = "macos"))]
            let n = unsafe {
                libc::getxattr(
                    c.as_ptr(),
                    GUEST_OVERRIDE_XATTR.as_ptr(),
                    std::ptr::null_mut(),
                    0,
                )
            };
            assert!(n < 0, "override left on {rel:?}");
        }
        force_remove(&worker).unwrap();
    }

    #[test]
    fn a_protected_link_through_an_unprotected_alias_is_materialized() {
        let (_t, base, copy) = base_and_copy();
        // tests/check -> ../alias/foo_test.py, alias -> real: the target is
        // protected, but the worker controls `alias`.
        write(&base, "real/foo_test.py", "assert False\n");
        symlink("real", base.join("alias")).unwrap();
        symlink("../alias/foo_test.py", base.join("tests/check")).unwrap();
        write(&copy, "real/foo_test.py", "assert False\n");
        write(&copy, "evil/foo_test.py", "pass\n");
        symlink("evil", copy.join("alias")).unwrap(); // worker retargets the alias
        restore_protected(&copy, &base, &defaults()).unwrap();
        let check = copy.join("tests/check");
        assert!(fs::symlink_metadata(&check).unwrap().file_type().is_file());
        assert_eq!(read(&copy, "tests/check"), "assert False\n");
        // Through a materialized top, a link is judged from where it lands.
        let (_t, base, copy) = base_and_copy();
        fs::remove_dir_all(base.join("tests")).unwrap();
        write(&base, "deep/suite/check.sh", "exit 1\n");
        symlink("check.sh", base.join("deep/suite/again")).unwrap();
        symlink("deep/suite", base.join("tests")).unwrap();
        restore_protected(&copy, &base, &defaults()).unwrap();
        assert!(fs::symlink_metadata(copy.join("tests/again"))
            .unwrap()
            .file_type()
            .is_file());
    }

    #[test]
    fn ancestors_of_protected_paths_are_sticky_and_build_dirs_are_not() {
        let (_t, base, worker) = base_and_copy();
        write(&base, "src/foo_test.rs", "#[test] fn t() {}\n");
        write(&worker, "src/foo_test.rs", "#[test] fn t() {}\n");
        write(&worker, "target/debug/out.bin", "x");
        let tree = GradeTree::prepare(&worker, &base, &defaults(), None).unwrap();
        let mode = |rel: &str| {
            fs::symlink_metadata(tree.path().join(rel))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777
        };
        // The namespaces holding protected entries: no rename/unlink by nobody.
        assert_eq!(mode(""), 0o1777, "tree root holds tests/ and .github/");
        assert_eq!(mode("src"), 0o1777, "src/ holds foo_test.rs");
        assert_eq!(mode("pkg"), 0o1777, "pkg/ holds pkg/tests/");
        // Build output stays plain world-writable.
        assert_eq!(mode("target"), 0o777);
        assert_eq!(mode("target/debug"), 0o777);
        assert_eq!(mode("tests"), 0o555);
    }

    #[test]
    fn grader_files_are_injected_locked_and_replace_a_worker_copy() {
        let (t, base, worker) = base_and_copy();
        let hidden = t.path().join("hidden");
        write(&hidden, "check.sh", "grep -q fixed src/lib.rs\n");
        // A worker planting its own `.pillbox-grader/check.sh` gets it replaced.
        write(&worker, ".pillbox-grader/check.sh", "exit 0\n");
        let set = ProtectSet::new(true, &[format!("/{GRADER_FILES_DIR}/")]).unwrap();
        let tree = GradeTree::prepare(&worker, &base, &set, Some(&hidden)).unwrap();
        let injected = tree.path().join(GRADER_FILES_DIR).join("check.sh");
        assert_eq!(
            fs::read_to_string(&injected).unwrap(),
            "grep -q fixed src/lib.rs\n"
        );
        assert_eq!(mode_of(&injected) & 0o222, 0, "injected grader is locked");
        assert!(
            tree.report
                .reverted
                .contains(&PathBuf::from(".pillbox-grader/check.sh")),
            "{:?}",
            tree.report.reverted
        );
        assert!(
            !base.join(GRADER_FILES_DIR).exists() && worker.join(GRADER_FILES_DIR).exists(),
            "base and live workspace are left alone"
        );
        // A symlink to the grader dir is copied as its contents, not as a link.
        let link = t.path().join("hidden-link");
        std::os::unix::fs::symlink(&hidden, &link).unwrap();
        let tree = GradeTree::prepare(&worker, &base, &set, Some(&link)).unwrap();
        let dir = tree.path().join(GRADER_FILES_DIR);
        assert!(fs::symlink_metadata(&dir).unwrap().file_type().is_dir());
        assert!(dir.join("check.sh").is_file());
        drop(tree);

        // A link inside the grader files is copied as its target's content…
        symlink("check.sh", hidden.join("again.sh")).unwrap();
        let tree = GradeTree::prepare(&worker, &base, &set, Some(&hidden)).unwrap();
        let again = tree.path().join(GRADER_FILES_DIR).join("again.sh");
        assert!(fs::symlink_metadata(&again).unwrap().file_type().is_file());
        drop(tree);
        // …and one that leaves them (here into the worker-controlled tree) is refused.
        write(&t.path().join("shared-tests"), "check.sh", "exit 1\n");
        symlink("../shared-tests/check.sh", hidden.join("escape.sh")).unwrap();
        let err = GradeTree::prepare(&worker, &base, &set, Some(&hidden))
            .err()
            .expect("an escaping grader link is refused");
        assert!(
            format!("{err:#}").contains("does not resolve inside"),
            "{err:#}"
        );
    }

    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn the_owner_cannot_rewrite_a_locked_protected_path() {
        // The virtiofs server writes as the host user, who owns this tree.
        // Stripping the owner's write bit is what stops a root guest whose
        // chmod we refused (the grader runs as nobody) from sticking.
        let (_t, base, worker) = base_and_copy();
        write(&worker, "src/lib.rs", "pub fn f() -> u8 { 1 }\n");
        write(&base, "tests/link-out", "");
        std::os::unix::fs::symlink("../src/lib.rs", base.join("tests/via")).unwrap();
        let tree = GradeTree::prepare(&worker, &base, &defaults(), None).unwrap();
        let tests = tree.path().join("tests/check.sh");
        assert_eq!(mode_of(&tests) & 0o222, 0, "protected file is not writable");
        assert!(
            fs::write(&tests, b"exit 0\n").is_err(),
            "owner write refused"
        );
        assert!(
            fs::write(tree.path().join("tests/extra.sh"), b"exit 0\n").is_err(),
            "cannot add a file under a protected directory"
        );
        // A link out of the set to worker code is materialized from the base
        // and locked, and locking it did not make the worker's file immutable.
        let via = tree.path().join("tests/via");
        assert!(fs::symlink_metadata(&via).unwrap().file_type().is_file());
        assert_eq!(mode_of(&via) & 0o222, 0, "materialized copy is locked");
        fs::write(tree.path().join("src/lib.rs"), b"pub fn f() -> u8 { 2 }\n").unwrap();
        assert_ne!(
            mode_of(&worker.join("tests")) & 0o222,
            0,
            "the live tree keeps its write bit"
        );
    }

    #[test]
    fn an_unreadable_directory_does_not_fail_the_grade_or_stick() {
        let (_t, base, worker) = base_and_copy();
        write(&worker, "sealed/note.txt", "hi\n");
        fs::set_permissions(worker.join("sealed"), fs::Permissions::from_mode(0o000)).unwrap();
        fs::set_permissions(worker.join("tests"), fs::Permissions::from_mode(0o000)).unwrap();
        let tree = GradeTree::prepare(&worker, &base, &defaults(), None).unwrap();
        assert_eq!(read(tree.path(), "tests/check.sh"), "exit 1\n");
        assert_eq!(read(tree.path(), "sealed/note.txt"), "hi\n");
        assert_eq!(mode_of(&worker.join("tests")), 0o000);
        assert_eq!(mode_of(&worker.join("sealed")), 0o000);
        // The tempdir has to be able to remove what the worker sealed.
        for dir in [worker.join("tests"), worker.join("sealed")] {
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn restoring_relaxed_modes_never_follows_a_swapped_in_symlink() {
        let t = TempDir::new().unwrap();
        let live = t.path().join("live");
        let outside = t.path().join("outside");
        fs::create_dir_all(live.join("sealed")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o750)).unwrap();
        fs::set_permissions(live.join("sealed"), fs::Permissions::from_mode(0o077)).unwrap();
        let guard = ModeGuard::relax(&live).unwrap();
        assert_eq!(mode_of(&live.join("sealed")) & 0o700, 0o700);
        // A still-running worker swaps the directory for a link to a host path.
        fs::remove_dir(live.join("sealed")).unwrap();
        symlink(&outside, live.join("sealed")).unwrap();
        drop(guard);
        assert_eq!(mode_of(&outside), 0o750, "the host path keeps its mode");
    }
}
