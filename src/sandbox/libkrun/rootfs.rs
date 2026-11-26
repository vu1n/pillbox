//! Each VM gets an independent writable root. Cached image generations are seed
//! trees only; neither a guest nor a cleanup path may write through to that seed.

use std::ops::Deref;
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use anyhow::{ensure, Context, Result};

pub(super) struct PrivateRootfs {
    directory: Option<tempfile::TempDir>,
    root: PathBuf,
}

impl PrivateRootfs {
    pub(super) fn fork(base: &Path, runtimes: &Path) -> Result<Self> {
        ensure!(
            super::plain_directory(base),
            "rootfs seed must be a plain directory"
        );
        std::fs::create_dir_all(runtimes).context("create private VM rootfs namespace")?;
        ensure!(
            super::plain_directory(runtimes),
            "private rootfs namespace must be a plain directory"
        );
        ensure!(
            std::fs::symlink_metadata(runtimes)?.uid() == unsafe { libc::geteuid() },
            "private rootfs namespace has a foreign owner"
        );
        crate::paths::ensure_mode_0700(runtimes)?;
        ensure!(
            std::fs::symlink_metadata(base)?.dev() == std::fs::symlink_metadata(runtimes)?.dev(),
            "rootfs seed and private clone destination must share one filesystem"
        );
        let directory = tempfile::Builder::new()
            .prefix("vm-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in(runtimes)
            .context("allocate private VM rootfs")?;
        let root = directory.path().join("rootfs");
        let method = crate::workspace::cow::cow_clone_dir(base, &root)
            .context("fork private VM rootfs (shared-root fallback is forbidden)")?;
        if method == crate::workspace::cow::CloneMethod::Copied {
            eprintln!("pillbox: private rootfs fork fell back to a full copy");
        }
        super::metadata::prepare_private_root(&root, directory.path())?;
        Ok(Self {
            directory: Some(directory),
            root,
        })
    }

    /// Transfer lifetime on spawn: CLI death must not unlink a running VM's
    /// files. Persisted sessions record this path; confirmed teardown removes it.
    pub(super) fn preserve(&mut self) {
        if let Some(directory) = self.directory.take() {
            let _ = directory.keep();
        }
    }

    pub(super) fn remove_stopped(&self) -> Result<()> {
        remove_stopped_root(&self.root)
    }
}

/// Remove a private root this process forked (its `vm-*/rootfs` path, as a
/// launch's handle records it) once its VMM is confirmed stopped.
pub(super) fn remove_stopped_root(root: &Path) -> Result<()> {
    let runtimes = root
        .parent()
        .and_then(Path::parent)
        .context("private rootfs namespace missing")?;
    remove_at(root, runtimes)
}

impl Deref for PrivateRootfs {
    type Target = Path;
    fn deref(&self) -> &Self::Target {
        &self.root
    }
}

pub(super) fn remove_owned(root: &Path) -> Result<()> {
    let raw_home = PathBuf::from(
        std::env::var_os("HOME").context("could not resolve $HOME for rootfs cleanup")?,
    );
    remove_owned_under(root, &raw_home)
}

/// [`remove_owned`] against an explicit host home — for the VMM child, whose `HOME`
/// is the guest's. Only a root inside `raw_home`'s private VM namespace is removed.
pub(super) fn remove_owned_under(root: &Path, raw_home: &Path) -> Result<()> {
    let raw_home = raw_home.to_path_buf();
    let krun = super::rootfs_backing::RootfsBacking::krun_dir_for_home(&raw_home)?;
    let canonical_home = krun
        .parent()
        .and_then(Path::parent)
        .context("canonical rootfs home is missing")?;
    let root = normalize_recorded_home_alias(root, &raw_home, canonical_home)?;
    let legacy = krun.join("vm-rootfs");
    let new = krun.join("case-sensitive-rootfs/vm-rootfs");
    if root.parent().and_then(Path::parent) == Some(legacy.as_path()) {
        return remove_at(&root, &legacy);
    }
    ensure!(
        root.parent().and_then(Path::parent) == Some(new.as_path()),
        "refuse to remove a rootfs outside a known private VM namespace"
    );
    let backing = super::rootfs_backing::RootfsBacking::reopen(&krun)?;
    backing.ensure_same_device(&new)?;
    remove_at(&root, &new)
}

fn normalize_recorded_home_alias(
    root: &Path,
    raw_home: &Path,
    canonical_home: &Path,
) -> Result<PathBuf> {
    let raw_krun = raw_home.join(".pillbox/krun");
    let Ok(suffix) = root.strip_prefix(&raw_krun) else {
        return Ok(root.to_path_buf());
    };
    let parts: Vec<_> = suffix.components().collect();
    let known_root = match parts.as_slice() {
        [Component::Normal(namespace), Component::Normal(vm), Component::Normal(name)] => {
            *namespace == "vm-rootfs"
                && vm.to_string_lossy().starts_with("vm-")
                && *name == "rootfs"
        }
        [Component::Normal(backing), Component::Normal(namespace), Component::Normal(vm), Component::Normal(name)] => {
            *backing == "case-sensitive-rootfs"
                && *namespace == "vm-rootfs"
                && vm.to_string_lossy().starts_with("vm-")
                && *name == "rootfs"
        }
        _ => false,
    };
    ensure!(
        known_root,
        "recorded rootfs has an unknown path beneath trusted HOME"
    );
    Ok(canonical_home.join(".pillbox/krun").join(suffix))
}

fn remove_at(root: &Path, runtimes: &Path) -> Result<()> {
    let directory = root.parent().context("private rootfs parent missing")?;
    ensure!(
        root.file_name().is_some_and(|name| name == "rootfs")
            && directory.parent() == Some(runtimes)
            && directory
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("vm-")),
        "refuse to remove a rootfs outside the private VM namespace"
    );
    match std::fs::symlink_metadata(directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        metadata => ensure!(
            metadata.context("stat private rootfs owner")?.is_dir(),
            "private rootfs owner must be a plain directory"
        ),
    }
    // A live guest can rewrite this tree (an abandoned launch scrubs it from inside the
    // still-running VMM), so removal never resolves a path: every step is fd-relative.
    remove_tree_nofollow(directory)
        .with_context(|| format!("remove private VM rootfs {}", directory.display()))
}

/// Remove `dir` and everything under it without following a symlink: each
/// directory is opened `O_NOFOLLOW` relative to its parent's fd, owner access is
/// restored on it by name with `AT_SYMLINK_NOFOLLOW`, and entries are unlinked
/// relative to their directory's fd. An entry swapped for a symlink mid-walk is
/// unlinked as a link, never traversed.
fn remove_tree_nofollow(dir: &Path) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;
    let path = CString::new(dir.as_os_str().as_bytes()).context("rootfs path has a NUL")?;
    // SAFETY: open() of a valid NUL-terminated path.
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("open rootfs owner directory");
    }
    // SAFETY: `fd` was just opened and is owned here.
    let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
    empty_dir_at(&owned, 0)?;
    drop(owned);
    std::fs::remove_dir(dir).context("remove emptied rootfs owner directory")
}

/// Deepest directory nesting [`empty_dir_at`] descends (one open fd per level).
const MAX_REMOVE_DEPTH: usize = 512;

/// Unlink everything inside the directory open at `dir`.
fn empty_dir_at(dir: &std::os::fd::OwnedFd, depth: usize) -> Result<()> {
    ensure!(
        depth < MAX_REMOVE_DEPTH,
        "rootfs nests deeper than {MAX_REMOVE_DEPTH} directories"
    );
    for name in dir_entries(dir)? {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: fstatat() on an owned dir fd and a NUL-terminated name.
        if unsafe {
            libc::fstatat(
                dir.as_raw_fd(),
                name.as_ptr(),
                &mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::NotFound {
                continue;
            }
            return Err(err).with_context(|| format!("stat rootfs entry {name:?}"));
        }
        let is_dir = st.st_mode & libc::S_IFMT == libc::S_IFDIR;
        #[cfg(test)]
        AFTER_STAT.with(|hook| {
            if let Some(hook) = hook.borrow_mut().as_mut() {
                hook(&name)
            }
        });
        if is_dir {
            if st.st_mode & 0o700 != 0o700 {
                let mode = (st.st_mode & 0o7777) | 0o700;
                // SAFETY: fchmodat() by name relative to an owned dir fd; never follows a link.
                if unsafe {
                    libc::fchmodat(
                        dir.as_raw_fd(),
                        name.as_ptr(),
                        mode,
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                } != 0
                {
                    let err = std::io::Error::last_os_error();
                    // Linux refuses a no-follow chmod of a link swapped in since the stat.
                    let swapped = matches!(
                        err.raw_os_error(),
                        Some(libc::ENOTSUP) | Some(libc::EOPNOTSUPP) | Some(libc::ENOENT)
                    );
                    if !swapped {
                        return Err(err).with_context(|| {
                            format!("restore access to rootfs directory {name:?}")
                        });
                    }
                }
            }
            // SAFETY: openat() relative to an owned dir fd; O_NOFOLLOW refuses a swapped-in link.
            let fd = unsafe {
                libc::openat(
                    dir.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd >= 0 {
                // SAFETY: `fd` was just opened and is owned here.
                let child = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
                empty_dir_at(&child, depth + 1)?;
            }
        }
        let flag = if is_dir { libc::AT_REMOVEDIR } else { 0 };
        // SAFETY: unlinkat() by name relative to an owned dir fd.
        if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), flag) } != 0 {
            let err = std::io::Error::last_os_error();
            // A name swapped between kinds mid-walk: retry as the other kind.
            let retried = matches!(
                err.raw_os_error(),
                Some(libc::ENOTDIR) | Some(libc::EISDIR) | Some(libc::EPERM)
            ) && unsafe {
                libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), flag ^ libc::AT_REMOVEDIR)
            } == 0;
            if !retried && err.kind() != std::io::ErrorKind::NotFound {
                return Err(err).with_context(|| format!("remove rootfs entry {name:?}"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// Test seam: runs between an entry's stat and its chmod, as a racing guest would.
    static AFTER_STAT: std::cell::RefCell<Option<Box<dyn FnMut(&std::ffi::CStr)>>> =
        const { std::cell::RefCell::new(None) };
}

/// The names in the directory open at `dir`, without `.` and `..`.
fn dir_entries(dir: &std::os::fd::OwnedFd) -> Result<Vec<std::ffi::CString>> {
    // SAFETY: dup() of an owned fd; fdopendir takes ownership of the duplicate.
    let dup = unsafe { libc::fcntl(dir.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if dup < 0 {
        return Err(std::io::Error::last_os_error()).context("dup rootfs directory fd");
    }
    let stream = unsafe { libc::fdopendir(dup) };
    if stream.is_null() {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(dup) };
        return Err(err).context("read rootfs directory");
    }
    // fdopendir shares the offset with `dir`'s open file description: start at the top.
    unsafe { libc::rewinddir(stream) };
    let mut names = Vec::new();
    loop {
        // SAFETY: readdir() on the stream opened above; the entry is copied before the next call.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() != b"." && name.to_bytes() != b".." {
            names.push(name.to_owned());
        }
    }
    // SAFETY: closes the stream (and the duplicate fd) opened above.
    unsafe { libc::closedir(stream) };
    Ok(names)
}

impl Drop for PrivateRootfs {
    fn drop(&mut self) {
        if self.directory.is_some() {
            if let Err(error) = self.remove_stopped() {
                eprintln!("pillbox: unspawned private rootfs cleanup failed: {error:#}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    #[test]
    fn recorded_home_alias_rewrites_only_known_vm_root_shapes() {
        let fixture = tempfile::tempdir().unwrap();
        let home = fixture.path().join("real-home");
        fs::create_dir(&home).unwrap();
        let alias = fixture.path().join("home-alias");
        std::os::unix::fs::symlink(&home, &alias).unwrap();
        let canonical = fs::canonicalize(&alias).unwrap();
        for suffix in [
            "vm-rootfs/vm-old/rootfs",
            "case-sensitive-rootfs/vm-rootfs/vm-new/rootfs",
        ] {
            let raw = alias.join(".pillbox/krun").join(suffix);
            let expected = canonical.join(".pillbox/krun").join(suffix);
            assert_eq!(
                normalize_recorded_home_alias(&raw, &alias, &canonical).unwrap(),
                expected
            );
            assert_eq!(
                normalize_recorded_home_alias(&expected, &alias, &canonical).unwrap(),
                expected
            );
        }
        let unknown = alias.join(".pillbox/krun/rootfs/v5/seed");
        assert!(normalize_recorded_home_alias(&unknown, &alias, &canonical).is_err());
        let traversal = alias.join(".pillbox/krun/vm-rootfs/../rootfs");
        assert!(normalize_recorded_home_alias(&traversal, &alias, &canonical).is_err());
        let outside = fixture.path().join("outside/vm-rootfs/vm-old/rootfs");
        assert_eq!(
            normalize_recorded_home_alias(&outside, &alias, &canonical).unwrap(),
            outside
        );
    }

    #[test]
    fn concurrent_and_successor_roots_do_not_share_writes_or_inodes() {
        let fixture = tempfile::tempdir().unwrap();
        let base = fixture.path().join("base");
        fs::create_dir_all(base.join("tmp")).unwrap();
        fs::create_dir(base.join("etc")).unwrap();
        fs::write(base.join("etc/config"), "image default").unwrap();
        fs::set_permissions(base.join("tmp"), fs::Permissions::from_mode(0o1777)).unwrap();
        std::os::unix::fs::symlink("etc/config", base.join("config-link")).unwrap();
        fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).unwrap();
        let runtimes = fixture.path().join("runtimes");
        let mut a = PrivateRootfs::fork(&base, &runtimes).unwrap();
        let b = PrivateRootfs::fork(&base, &runtimes).unwrap();
        assert_ne!(
            fs::metadata(a.join("etc/config")).unwrap().ino(),
            fs::metadata(base.join("etc/config")).unwrap().ino()
        );
        assert_ne!(
            fs::metadata(a.join("etc/config")).unwrap().ino(),
            fs::metadata(b.join("etc/config")).unwrap().ino()
        );
        assert_eq!(
            fs::metadata(a.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&base).unwrap().permissions().mode() & 0o7777,
            0o700
        );
        assert_eq!(
            fs::metadata(&*a).unwrap().permissions().mode() & 0o7777,
            0o755
        );
        assert_eq!(
            fs::metadata(&*b).unwrap().permissions().mode() & 0o7777,
            0o755
        );
        assert_eq!(
            fs::metadata(a.join("tmp")).unwrap().permissions().mode() & 0o7777,
            0o1777
        );
        assert_eq!(
            fs::read_link(a.join("config-link")).unwrap(),
            Path::new("etc/config")
        );
        fs::write(a.join("tmp/secret"), "private to A").unwrap();
        fs::write(a.join("etc/config"), "A changed it").unwrap();
        assert!(!b.join("tmp/secret").exists());
        assert!(!base.join("tmp/secret").exists());
        assert_eq!(
            fs::read_to_string(b.join("etc/config")).unwrap(),
            "image default"
        );
        a.preserve();
        let retained = a.root.clone();
        drop(a);
        assert!(retained.join("tmp/secret").exists());
        let successor = PrivateRootfs::fork(&base, &runtimes).unwrap();
        assert!(!successor.join("tmp/secret").exists());
        assert_eq!(
            fs::read_to_string(successor.join("etc/config")).unwrap(),
            "image default"
        );
        remove_at(&retained, &runtimes).unwrap();
        assert!(b.join("etc/config").exists());
        assert!(base.join("etc/config").exists());
        assert!(remove_at(&base, &runtimes).is_err());
        let removed = b.root.clone();
        b.remove_stopped().unwrap();
        assert!(!removed.exists());
    }

    #[test]
    fn cleanup_restores_private_directory_access_without_following_symlinks() {
        let fixture = tempfile::tempdir().unwrap();
        let base = fixture.path().join("base");
        let readonly = base.join("readonly");
        fs::create_dir_all(&readonly).unwrap();
        fs::write(readonly.join("file"), "image file").unwrap();
        fs::set_permissions(&readonly, fs::Permissions::from_mode(0o555)).unwrap();
        let outside = fixture.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("file"), "outside file").unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o555)).unwrap();
        std::os::unix::fs::symlink(&outside, base.join("outside-link")).unwrap();
        let runtimes = fixture.path().join("runtimes");
        let mut root = PrivateRootfs::fork(&base, &runtimes).unwrap();
        fs::set_permissions(root.join("readonly"), fs::Permissions::from_mode(0o000)).unwrap();
        root.preserve();
        root.remove_stopped().unwrap();
        assert!(!root.exists());
        assert_eq!(
            fs::read_to_string(outside.join("file")).unwrap(),
            "outside file"
        );
        for path in [&readonly, &outside] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o555
            );
        }
        let unspawned = PrivateRootfs::fork(&base, &runtimes).unwrap();
        let unspawned_path = unspawned.to_path_buf();
        drop(unspawned);
        assert!(!unspawned_path.exists());
        for path in [&readonly, &outside] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn removal_never_follows_a_link_a_live_guest_swaps_in_mid_walk() {
        let fixture = tempfile::tempdir().unwrap();
        let outside = fixture.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("file"), "outside file").unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o555)).unwrap();
        let tree = fixture.path().join("vm-race");
        let swap = tree.join("rootfs/swap");
        fs::create_dir_all(&swap).unwrap();
        fs::set_permissions(&swap, fs::Permissions::from_mode(0o000)).unwrap();
        // The guest replaces the locked directory with a link to `outside` right after
        // the walk saw it as a directory, before its access is restored.
        let (link_to, at) = (outside.clone(), swap.clone());
        AFTER_STAT.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |name| {
                if name.to_bytes() == b"swap" && fs::symlink_metadata(&at).unwrap().is_dir() {
                    fs::remove_dir(&at).unwrap();
                    std::os::unix::fs::symlink(&link_to, &at).unwrap();
                }
            }))
        });
        let removed = remove_tree_nofollow(&tree);
        AFTER_STAT.with(|hook| hook.borrow_mut().take());
        removed.unwrap();
        assert!(!tree.exists());
        assert_eq!(
            fs::metadata(&outside).unwrap().permissions().mode() & 0o777,
            0o555,
            "a swapped-in link was chmodded through"
        );
        assert_eq!(
            fs::read_to_string(outside.join("file")).unwrap(),
            "outside file"
        );
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn rootfs_fork_rejects_aliases_and_cleans_unspawned_roots() {
        let fixture = tempfile::tempdir().unwrap();
        let base = fixture.path().join("base");
        fs::create_dir(&base).unwrap();
        let alias = fixture.path().join("alias");
        std::os::unix::fs::symlink(&base, &alias).unwrap();
        let runtimes = fixture.path().join("runtimes");
        assert!(PrivateRootfs::fork(&alias, &runtimes).is_err());
        let root = PrivateRootfs::fork(&base, &runtimes).unwrap();
        let path = root.root.clone();
        drop(root);
        assert!(!path.exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn recorded_new_root_is_preserved_when_backing_is_unmounted() {
        crate::test_util::with_isolated_home("unmounted-native-rootfs", || {
            let prior_home = std::env::var_os("HOME").unwrap();
            let canonical_home = fs::canonicalize(&prior_home).unwrap();
            std::env::set_var("HOME", &canonical_home);
            let krun = super::super::rootfs_backing::RootfsBacking::krun_dir().unwrap();
            let root = krun.join("case-sensitive-rootfs/vm-rootfs/vm-test/rootfs");
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join("evidence"), b"retain").unwrap();
            assert!(remove_owned(&root).is_err());
            assert_eq!(fs::read(root.join("evidence")).unwrap(), b"retain");
            let legacy = krun.join("vm-rootfs/vm-legacy/rootfs");
            fs::create_dir_all(&legacy).unwrap();
            fs::write(legacy.join("evidence"), b"old private clone").unwrap();
            remove_owned(&legacy).unwrap();
            assert!(!legacy.exists());
            assert!(!krun.join("case-sensitive-rootfs.sparsebundle").exists());
            std::env::set_var("HOME", prior_home);
        });
    }
    #[test]
    #[ignore = "requires signed libkrun binary and freshly exported offline runner rootfs"]
    fn live_concurrent_and_successor_vm_roots_are_isolated() {
        use std::time::{Duration, Instant};
        let binary = std::env::var_os("PILLBOX_ISOLATION_BINARY").expect("signed binary path");
        let base =
            PathBuf::from(std::env::var_os("PILLBOX_ISOLATION_BASE").expect("fresh rootfs seed"));
        let fixture = tempfile::tempdir().unwrap();
        let probe = fixture.path().join("probe");
        fs::create_dir(&probe).unwrap();
        assert!(!base.join("tmp/pb-isolation-secret").exists());
        assert!(!base.join("etc/pb-isolation-a").exists());
        let backing = super::super::rootfs_backing::RootfsBacking::reopen(
            &super::super::rootfs_backing::RootfsBacking::krun_dir().unwrap(),
        )
        .unwrap();
        backing.ensure_same_device(&base).unwrap();
        let runtimes = backing
            .namespace(
                "vm-rootfs",
                super::super::rootfs_backing::RootfsBacking::ordinary_deadline().unwrap(),
                &|| false,
            )
            .unwrap();
        let a = PrivateRootfs::fork(&base, &runtimes).unwrap();
        let b = PrivateRootfs::fork(&base, &runtimes).unwrap();
        let a_owner = a.root.parent().unwrap().to_path_buf();
        let b_owner = b.root.parent().unwrap().to_path_buf();
        let run = move |mut root: PrivateRootfs, script: &str, spec_path: PathBuf| {
            let owner = super::super::repository::SupervisedOwner::bind().unwrap();
            let spec = super::super::VmSpec {
                offline: false,
                rootfs: root.to_string_lossy().into_owned(),
                vcpus: 1,
                ram_mib: 512,
                shares: vec![super::super::Share {
                    tag: "probe".into(),
                    host_path: probe.to_string_lossy().into_owned(),
                }],
                exec: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    format!("mkdir -p /probe; mount -t virtiofs probe /probe && {script}"),
                ],
                vsock: Vec::new(),
                egress: None,
                ownership: Some(owner.ownership()),
            };
            fs::write(&spec_path, serde_json::to_vec(&spec).unwrap()).unwrap();
            let mut command = super::super::session::vmm_command_for(
                Path::new(&binary),
                &spec_path,
                super::super::session::VmmTether::Supervised,
            );
            root.preserve();
            let output =
                super::super::repository::run_supervised_vmm(&mut command, owner, b"[]").unwrap();
            root.remove_stopped().unwrap();
            assert!(
                output.status.success(),
                "VM failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        let run_a = run.clone();
        let spec_a = fixture.path().join("a.json");
        let owner_a = std::thread::spawn(move || {
            run_a(a,
            "printf 'private' > /tmp/pb-isolation-secret; printf 'A' > /etc/pb-isolation-a; touch /probe/a-ready; i=0; while [ ! -e /probe/release ]; do i=$((i+1)); [ \"$i\" -le 100 ] || exit 9; sleep .1; done; test ! -e /etc/pb-isolation-b",
            spec_a)
        });
        let probe = fixture.path().join("probe");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !probe.join("a-ready").exists() {
            assert!(Instant::now() < deadline, "VM A did not boot");
            std::thread::sleep(Duration::from_millis(20));
        }
        let clean = "test ! -e /tmp/pb-isolation-secret && test ! -e /etc/pb-isolation-a && test ! -e /etc/pb-isolation-b";
        run(
            b,
            &format!("{clean} && printf B > /etc/pb-isolation-b && touch /probe/b-isolated"),
            fixture.path().join("b.json"),
        );
        assert!(probe.join("b-isolated").exists());
        fs::write(probe.join("release"), "stop A").unwrap();
        owner_a.join().unwrap();
        let successor = PrivateRootfs::fork(&base, &runtimes).unwrap();
        let successor_owner = successor.root.parent().unwrap().to_path_buf();
        run(
            successor,
            &format!("{clean} && touch /probe/successor-isolated"),
            fixture.path().join("successor.json"),
        );
        assert!(probe.join("successor-isolated").exists());
        assert!(!base.join("tmp/pb-isolation-secret").exists());
        assert!(!base.join("etc/pb-isolation-a").exists());
        assert!(!base.join("etc/pb-isolation-b").exists());
        for owner in [a_owner, b_owner, successor_owner] {
            assert!(
                !owner.exists(),
                "stopped VM root remains: {}",
                owner.display()
            );
        }
    }
}
