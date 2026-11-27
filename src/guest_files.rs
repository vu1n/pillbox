//! Host reads and writes of files inside a directory a guest could have written:
//! an agent home (mounted read-write into the `auth login` VM) and every clone of
//! one. A guest can plant a symlink anywhere in such a tree, so every access here
//! walks the path one component at a time with `O_NOFOLLOW` and refuses a link.

use std::ffi::CString;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};

use anyhow::{bail, ensure, Context, Result};

fn c_name(name: &std::ffi::OsStr) -> Result<CString> {
    CString::new(name.as_bytes()).context("path component contains a NUL byte")
}

fn open_dir_at(dir: Option<&OwnedFd>, name: &CString) -> std::io::Result<OwnedFd> {
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let fd = unsafe {
        match dir {
            Some(dir) => libc::openat(dir.as_raw_fd(), name.as_ptr(), flags),
            None => libc::open(name.as_ptr(), flags),
        }
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The relative path's components, refusing anything but plain names.
fn plain_components(rel: &Path) -> Result<Vec<CString>> {
    let names = rel
        .components()
        .map(|c| match c {
            Component::Normal(name) => c_name(name),
            other => bail!("`{}` has a non-plain component {other:?}", rel.display()),
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(!names.is_empty(), "empty relative path");
    Ok(names)
}

/// Open the directory that holds `rel`'s leaf, never following a link. With
/// `create`, missing directories are made (0700); without it, a missing one is `None`.
fn open_parent(root: &Path, names: &[CString], create: bool) -> Result<Option<OwnedFd>> {
    let mut dir = open_dir_at(None, &c_name(root.as_os_str())?)
        .with_context(|| format!("open {} without following a link", root.display()))?;
    for name in &names[..names.len() - 1] {
        let shown = name.to_string_lossy();
        dir = match open_dir_at(Some(&dir), name) {
            Ok(next) => next,
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {
                if !create {
                    return Ok(None);
                }
                if unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
                    return Err(std::io::Error::last_os_error())
                        .with_context(|| format!("create `{shown}` under {}", root.display()));
                }
                open_dir_at(Some(&dir), name)
                    .with_context(|| format!("open new `{shown}` under {}", root.display()))?
            }
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "`{shown}` under {} is not a plain directory (a link is refused)",
                        root.display()
                    )
                })
            }
        };
    }
    Ok(Some(dir))
}

/// The opened leaf must be a lone regular file: not a link (O_NOFOLLOW), a
/// device, a FIFO, or a hard link to something else.
fn checked_file(fd: libc::c_int, root: &Path, rel: &Path) -> Result<File> {
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "open `{}` under {} without following a link",
                rel.display(),
                root.display()
            )
        });
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let meta = file.metadata()?;
    use std::os::unix::fs::MetadataExt;
    ensure!(
        meta.is_file() && meta.nlink() == 1,
        "`{}` under {} is not a plain file",
        rel.display(),
        root.display()
    );
    Ok(file)
}

// The host never follows a link when it writes into a clone a guest could have written.
/// Write `bytes` to `root/rel` with `mode`, creating missing parent directories,
/// replacing an existing plain file's contents, and refusing any link on the way.
pub(crate) fn write(root: &Path, rel: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let names = plain_components(rel)?;
    let dir = open_parent(root, &names, true)?.expect("open_parent creates when asked");
    let leaf = names.last().expect("non-empty");
    let flags =
        libc::O_WRONLY | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
    let fd = unsafe { libc::openat(dir.as_raw_fd(), leaf.as_ptr(), flags, mode) };
    let mut file = checked_file(fd, root, rel)?;
    // Mode first, then truncate: the content never sits under the old mode.
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    file.set_len(0)?;
    file.write_all(bytes)
        .with_context(|| format!("write `{}` under {}", rel.display(), root.display()))
}

/// Read `root/rel` without following a link; `None` when it (or a parent) is absent.
pub(crate) fn read(root: &Path, rel: &Path) -> Result<Option<Vec<u8>>> {
    let names = plain_components(rel)?;
    let Some(dir) = open_parent(root, &names, false)? else {
        return Ok(None);
    };
    let leaf = names.last().expect("non-empty");
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
    let fd = unsafe { libc::openat(dir.as_raw_fd(), leaf.as_ptr(), flags) };
    if fd < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
        return Ok(None);
    }
    let mut file = checked_file(fd, root, rel)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

/// Remove the tree at `root/rel` if present, refusing a link among its parents
/// (the tree itself is removed without following links inside it).
pub(crate) fn remove_tree(root: &Path, rel: &Path) -> Result<()> {
    let names = plain_components(rel)?;
    if open_parent(root, &names, false)?.is_none() {
        return Ok(());
    }
    // Every parent is a plain directory, so this path names the entry itself.
    let path = root.join(rel);
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("inspect {}", path.display())),
    };
    if meta.is_dir() {
        std::fs::remove_dir_all(&path)
    } else {
        std::fs::remove_file(&path)
    }
    .with_context(|| format!("remove {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn setup() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let clone = tmp.path().join("clone");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&clone).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("target"), b"host secret").unwrap();
        (tmp, clone, outside)
    }

    #[test]
    fn writes_a_new_file_and_its_parents_owner_only() {
        let (_tmp, clone, _) = setup();
        write(&clone, Path::new(".pillbox/mcp.json"), b"{}", 0o600).unwrap();
        let path = clone.join(".pillbox/mcp.json");
        assert_eq!(std::fs::read(&path).unwrap(), b"{}");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        // Rewriting replaces the contents.
        write(&clone, Path::new(".pillbox/mcp.json"), b"[]", 0o600).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"[]");
        assert_eq!(
            read(&clone, Path::new(".pillbox/mcp.json")).unwrap(),
            Some(b"[]".to_vec())
        );
        assert_eq!(read(&clone, Path::new(".pillbox/none")).unwrap(), None);
        assert_eq!(read(&clone, Path::new("none/x")).unwrap(), None);
    }

    #[test]
    fn a_planted_link_at_a_parent_or_the_leaf_is_refused_and_the_target_untouched() {
        let (_tmp, clone, outside) = setup();
        let target = outside.join("target");
        // A parent directory that is a link to a host directory.
        symlink(&outside, clone.join(".pillbox")).unwrap();
        assert!(write(&clone, Path::new(".pillbox/target"), b"x", 0o600).is_err());
        assert!(read(&clone, Path::new(".pillbox/target")).is_err());
        assert!(remove_tree(&clone, Path::new(".pillbox/target")).is_err());
        // A leaf that is a link to a host file.
        symlink(&target, clone.join("leaf")).unwrap();
        assert!(write(&clone, Path::new("leaf"), b"x", 0o600).is_err());
        assert!(read(&clone, Path::new("leaf")).is_err());
        // A hard link to a host file is refused too.
        std::fs::hard_link(&target, clone.join("hard")).unwrap();
        assert!(write(&clone, Path::new("hard"), b"x", 0o600).is_err());

        assert_eq!(std::fs::read(&target).unwrap(), b"host secret");
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_ne!(mode & 0o777, 0o600, "the target's mode was not changed");
        assert!(outside.join("target").exists());
    }

    #[test]
    fn a_non_plain_relative_path_is_refused() {
        let (_tmp, clone, _) = setup();
        for rel in ["../outside/target", "/etc/passwd", "a/../../b", ""] {
            assert!(write(&clone, Path::new(rel), b"x", 0o600).is_err(), "{rel}");
        }
    }

    #[test]
    fn remove_tree_removes_a_plain_tree_and_tolerates_absence() {
        let (_tmp, clone, outside) = setup();
        write(&clone, Path::new(".codex/sessions/a/b.jsonl"), b"x", 0o600).unwrap();
        // A link inside the tree is removed as a link, never followed.
        symlink(&outside, clone.join(".codex/sessions/link")).unwrap();
        remove_tree(&clone, Path::new(".codex/sessions")).unwrap();
        assert!(!clone.join(".codex/sessions").exists());
        assert!(outside.join("target").exists());
        remove_tree(&clone, Path::new(".codex/sessions")).unwrap();
        remove_tree(&clone, Path::new("missing/tree")).unwrap();
    }
}
