//! Host-side metadata preparation for disposable libkrun virtio-fs clones.
//!
//! macOS libkrun's passthrough backend reads `user.containers.override_stat`
//! as `uid:gid:mode` (decimal UID/GID, octal mode).  Writing that override from
//! the guest is not reliable for host-created read-only inodes, so macOS clones
//! are prepared before they are mounted.  The non-macOS backend keeps the
//! existing guest-side path in `boot.rs`.

use std::path::Path;

use anyhow::Result;

#[cfg(not(target_os = "macos"))]
pub(super) fn prepare_guest_clone_metadata(_root: &Path) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::CString;
    use std::fs::{self, File, OpenOptions};
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::path::Path;

    use anyhow::{bail, Context, Result};

    const OVERRIDE_STAT: &[u8] = b"user.containers.override_stat\0";
    const MAX_OVERRIDE_STAT: usize = 32;
    const ROOT_UID: u32 = 0;
    const ROOT_GID: u32 = 0;

    type OverrideStat = (Option<u32>, Option<u32>, Option<u32>);

    pub(super) fn prepare_guest_clone_metadata(root: &Path) -> Result<()> {
        let metadata = fs::symlink_metadata(root)
            .with_context(|| format!("stat clone root {}", root.display()))?;
        if !metadata.file_type().is_dir() {
            bail!("clone metadata root {} is not a directory", root.display());
        }
        // O_NOFOLLOW_ANY also rejects harmless symlinks in the host's parent
        // path (for example macOS's `/var` → `/private/var`). Resolve only the
        // already-verified clone root; entries below it remain no-follow.
        let root = fs::canonicalize(root)
            .with_context(|| format!("resolve clone root {}", root.display()))?;
        normalize_directory(&root)
    }

    fn normalize_directory(path: &Path) -> Result<()> {
        let file = open_directory(path)
            .with_context(|| format!("open clone directory {}", path.display()))?;
        let metadata = file
            .metadata()
            .with_context(|| format!("fstat clone directory {}", path.display()))?;
        if !metadata.file_type().is_dir() {
            bail!(
                "clone directory {} changed type while opening",
                path.display()
            );
        }
        let backing_mode = metadata.mode();
        let effective_mode = effective_mode(path, Some(&file), None, backing_mode)?;

        with_temporary_owner_write(&file, path, backing_mode, || {
            set_override_fd(&file, effective_mode, path)?;
            for entry in fs::read_dir(path)
                .with_context(|| format!("read clone directory {}", path.display()))?
            {
                let entry = entry.with_context(|| format!("read entry in {}", path.display()))?;
                normalize_entry(&entry.path())?;
            }
            Ok(())
        })
    }

    fn normalize_entry(path: &Path) -> Result<()> {
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("lstat clone entry {}", path.display()))?;
        let file_type = metadata.file_type();

        if file_type.is_symlink() {
            let effective_mode = effective_mode(path, None, Some(path), metadata.mode())?;
            set_override_path(path, effective_mode)
        } else if file_type.is_dir() {
            normalize_directory(path)
        } else if file_type.is_file() {
            normalize_regular_file(path)
        } else {
            bail!(
                "unsupported inode type in disposable clone at {}",
                path.display()
            );
        }
    }

    fn normalize_regular_file(path: &Path) -> Result<()> {
        let file = open_regular_file(path)
            .with_context(|| format!("open clone file {}", path.display()))?;
        let metadata = file
            .metadata()
            .with_context(|| format!("fstat clone file {}", path.display()))?;
        if !metadata.file_type().is_file() {
            bail!("clone file {} changed type while opening", path.display());
        }
        let backing_mode = metadata.mode();
        let effective_mode = effective_mode(path, Some(&file), None, backing_mode)?;
        with_temporary_owner_write(&file, path, backing_mode, || {
            set_override_fd(&file, effective_mode, path)
        })
    }

    fn open_directory(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW_ANY)
            .open(path)
    }

    fn open_regular_file(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW_ANY)
            .open(path)
    }

    fn effective_mode(
        path: &Path,
        file: Option<&File>,
        symlink_path: Option<&Path>,
        backing_mode: u32,
    ) -> Result<u32> {
        let override_stat = read_override(path, file, symlink_path)?;
        let Some((_, _, Some(override_mode))) = override_stat else {
            return Ok(backing_mode);
        };

        const TYPE_MASK: u32 = libc::S_IFMT as u32;
        let override_type = override_mode & TYPE_MASK;
        let backing_type = backing_mode & TYPE_MASK;
        if override_type != 0 && override_type != backing_type {
            bail!(
                "clone metadata type conflict at {}: override {:o}, backing {:o}",
                path.display(),
                override_type,
                backing_type
            );
        }
        Ok((if override_type == 0 {
            backing_type
        } else {
            override_type
        }) | (override_mode & !TYPE_MASK))
    }

    fn read_override(
        path: &Path,
        file: Option<&File>,
        symlink_path: Option<&Path>,
    ) -> Result<Option<OverrideStat>> {
        let mut value = [0_u8; MAX_OVERRIDE_STAT + 1];
        let size = if let Some(file) = file {
            unsafe {
                libc::fgetxattr(
                    file.as_raw_fd(),
                    OVERRIDE_STAT.as_ptr().cast(),
                    value.as_mut_ptr().cast(),
                    value.len(),
                    0,
                    0,
                )
            }
        } else {
            let symlink_path = symlink_path.expect("symlink path required without descriptor");
            let c_path = c_path(symlink_path)?;
            unsafe {
                libc::getxattr(
                    c_path.as_ptr(),
                    OVERRIDE_STAT.as_ptr().cast(),
                    value.as_mut_ptr().cast(),
                    value.len(),
                    0,
                    libc::XATTR_NOFOLLOW,
                )
            }
        };
        if size < 0 {
            let error = io::Error::last_os_error();
            if matches!(
                error.raw_os_error(),
                Some(code) if code == libc::ENOATTR || code == libc::ENODATA
            ) {
                return Ok(None);
            }
            return Err(error)
                .with_context(|| format!("read override metadata at {}", path.display()));
        }
        let size = usize::try_from(size).context("override metadata size overflow")?;
        if size == 0 || size > MAX_OVERRIDE_STAT {
            bail!(
                "override metadata at {} is outside the {}-byte parser bound",
                path.display(),
                MAX_OVERRIDE_STAT
            );
        }
        parse_override(path, &value[..size]).map(Some)
    }

    fn parse_override(path: &Path, value: &[u8]) -> Result<OverrideStat> {
        let text = std::str::from_utf8(value)
            .with_context(|| format!("override metadata at {} is not UTF-8", path.display()))?;
        let mut parts = text.split(':');
        let uid = parse_decimal(path, parts.next(), "uid")?;
        let gid = parse_decimal(path, parts.next(), "gid")?;
        let mode = parse_octal(path, parts.next(), "mode")?;
        if parts.next().is_some() {
            bail!(
                "override metadata at {} has too many fields",
                path.display()
            );
        }
        if mode.is_some_and(|mode| mode > 0o177777) {
            bail!(
                "override metadata at {} has an invalid mode",
                path.display()
            );
        }
        Ok((uid, gid, mode))
    }

    fn parse_decimal(path: &Path, value: Option<&str>, field: &str) -> Result<Option<u32>> {
        let value = value.ok_or_else(|| {
            anyhow::anyhow!("override metadata at {} is missing {field}", path.display())
        })?;
        if value == "x" {
            return Ok(None);
        }
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            bail!(
                "override metadata at {} has invalid {field}",
                path.display()
            );
        }
        value.parse::<u32>().map(Some).with_context(|| {
            format!(
                "override metadata at {} has overflowing {field}",
                path.display()
            )
        })
    }

    fn parse_octal(path: &Path, value: Option<&str>, field: &str) -> Result<Option<u32>> {
        let value = value.ok_or_else(|| {
            anyhow::anyhow!("override metadata at {} is missing {field}", path.display())
        })?;
        if value == "x" {
            return Ok(None);
        }
        if value.is_empty() || !value.bytes().all(|byte| matches!(byte, b'0'..=b'7')) {
            bail!(
                "override metadata at {} has invalid {field}",
                path.display()
            );
        }
        u32::from_str_radix(value, 8).map(Some).with_context(|| {
            format!(
                "override metadata at {} has overflowing {field}",
                path.display()
            )
        })
    }

    fn set_override_fd(file: &File, mode: u32, path: &Path) -> Result<()> {
        let value = override_value(mode);
        let rc = unsafe {
            libc::fsetxattr(
                file.as_raw_fd(),
                OVERRIDE_STAT.as_ptr().cast(),
                value.as_ptr().cast(),
                value.len(),
                0,
                0,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("write override metadata at {}", path.display()));
        }
        Ok(())
    }

    fn set_override_path(path: &Path, mode: u32) -> Result<()> {
        let c_path = c_path(path)?;
        let value = override_value(mode);
        let rc = unsafe {
            libc::setxattr(
                c_path.as_ptr(),
                OVERRIDE_STAT.as_ptr().cast(),
                value.as_ptr().cast(),
                value.len(),
                0,
                libc::XATTR_NOFOLLOW,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("write override metadata at {}", path.display()));
        }
        Ok(())
    }

    fn override_value(mode: u32) -> Vec<u8> {
        format!("{ROOT_UID}:{ROOT_GID}:0{mode:o}").into_bytes()
    }

    fn c_path(path: &Path) -> Result<CString> {
        CString::new(path.as_os_str().as_bytes())
            .with_context(|| format!("clone path contains NUL: {}", path.display()))
    }

    fn with_temporary_owner_write<T>(
        file: &File,
        path: &Path,
        backing_mode: u32,
        operation: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let needs_write = backing_mode & libc::S_IWUSR as u32 == 0;
        if needs_write {
            fchmod(file, backing_mode | libc::S_IWUSR as u32, path)
                .context("temporarily add owner-write for clone metadata")?;
        }

        let result = operation();
        let restore = if needs_write {
            fchmod(file, backing_mode, path)
        } else {
            Ok(())
        };
        match (result, restore) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(error)) => Err(error).context("restore clone backing mode"),
            (Err(error), Err(restore_error)) => Err(error).context(format!(
                "restore clone backing mode at {} also failed: {restore_error}",
                path.display()
            )),
        }
    }

    fn fchmod(file: &File, mode: u32, path: &Path) -> Result<()> {
        let rc = unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) };
        if rc != 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("chmod clone entry {}", path.display()));
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::symlink;
        use std::os::unix::fs::PermissionsExt;

        fn mode(path: &Path) -> u32 {
            fs::symlink_metadata(path).unwrap().mode()
        }

        fn write_override(path: &Path, value: &str) {
            let c_path = c_path(path).unwrap();
            let rc = unsafe {
                libc::setxattr(
                    c_path.as_ptr(),
                    OVERRIDE_STAT.as_ptr().cast(),
                    value.as_ptr().cast(),
                    value.len(),
                    0,
                    0,
                )
            };
            assert_eq!(
                rc,
                0,
                "seed override metadata: {}",
                io::Error::last_os_error()
            );
        }

        fn read_override_text(path: &Path) -> String {
            let c_path = c_path(path).unwrap();
            let mut value = [0_u8; MAX_OVERRIDE_STAT + 1];
            let size = unsafe {
                libc::getxattr(
                    c_path.as_ptr(),
                    OVERRIDE_STAT.as_ptr().cast(),
                    value.as_mut_ptr().cast(),
                    value.len(),
                    0,
                    0,
                )
            };
            assert!(
                size >= 0,
                "read override metadata: {}",
                io::Error::last_os_error()
            );
            String::from_utf8(value[..size as usize].to_vec()).unwrap()
        }

        #[test]
        fn normalizes_read_only_clone_and_preserves_effective_modes() {
            let fixture = tempfile::tempdir().unwrap();
            let source = fixture.path().join("source");
            let clone = fixture.path().join("clone");
            let outside = fixture.path().join("outside");
            fs::create_dir_all(&source).unwrap();
            fs::create_dir_all(&clone).unwrap();
            fs::write(source.join("original"), b"source").unwrap();
            fs::set_permissions(source.join("original"), fs::Permissions::from_mode(0o444))
                .unwrap();

            let host_readonly = clone.join("host-readonly");
            fs::copy(source.join("original"), &host_readonly).unwrap();
            fs::set_permissions(&host_readonly, fs::Permissions::from_mode(0o444)).unwrap();
            let virtual_readonly = clone.join("virtual-readonly");
            fs::write(&virtual_readonly, b"virtual").unwrap();
            fs::set_permissions(&virtual_readonly, fs::Permissions::from_mode(0o600)).unwrap();
            write_override(&virtual_readonly, "501:20:0100444");
            let x_fields = clone.join("x-fields");
            fs::write(&x_fields, b"x-fields").unwrap();
            fs::set_permissions(&x_fields, fs::Permissions::from_mode(0o600)).unwrap();
            write_override(&x_fields, "x:x:0644");
            let x_mode = clone.join("x-mode");
            fs::write(&x_mode, b"x-mode").unwrap();
            write_override(&x_mode, "0:0:x");
            fs::set_permissions(&x_mode, fs::Permissions::from_mode(0o555)).unwrap();

            let private = clone.join("private");
            fs::create_dir(&private).unwrap();
            let setid = private.join("setid");
            fs::write(&setid, b"setid").unwrap();
            fs::set_permissions(&setid, fs::Permissions::from_mode(0o6750)).unwrap();
            fs::set_permissions(&private, fs::Permissions::from_mode(0o555)).unwrap();
            let setid_mode = mode(&setid);

            fs::write(&outside, b"outside").unwrap();
            let outside_mode = mode(&outside);
            symlink(&outside, clone.join("escape")).unwrap();

            let source_bytes = fs::read(source.join("original")).unwrap();
            let source_mode = mode(&source.join("original"));
            prepare_guest_clone_metadata(&clone).unwrap();

            assert_eq!(mode(&host_readonly) & 0o7777, 0o444);
            assert_eq!(read_override_text(&host_readonly), "0:0:0100444");
            assert_eq!(read_override_text(&virtual_readonly), "0:0:0100444");
            assert_eq!(read_override_text(&x_fields), "0:0:0100644");
            assert_eq!(read_override_text(&x_mode), "0:0:0100555");
            assert_eq!(mode(&private) & 0o7777, 0o555);
            assert_eq!(mode(&setid), setid_mode);
            assert_eq!(read_override_text(&setid), format!("0:0:0{setid_mode:o}"));
            assert_eq!(fs::read_link(clone.join("escape")).unwrap(), outside);
            assert_eq!(fs::read(&outside).unwrap(), b"outside");
            assert_eq!(mode(&outside), outside_mode);
            assert_eq!(fs::read(source.join("original")).unwrap(), source_bytes);
            assert_eq!(mode(&source.join("original")), source_mode);
        }

        #[test]
        fn rejects_malformed_and_type_conflicting_overrides() {
            for (name, value) in [("malformed", "not-metadata"), ("conflict", "501:20:040700")] {
                let fixture = tempfile::tempdir().unwrap();
                let clone = fixture.path().join("clone");
                fs::create_dir(&clone).unwrap();
                let file = clone.join(name);
                fs::write(&file, b"fixture").unwrap();
                write_override(&file, value);
                fs::set_permissions(&file, fs::Permissions::from_mode(0o444)).unwrap();
                assert!(prepare_guest_clone_metadata(&clone).is_err());
                assert_eq!(mode(&file) & 0o7777, 0o444);
            }
        }

        #[test]
        fn restores_directory_mode_when_an_unsupported_entry_aborts() {
            let fixture = tempfile::tempdir().unwrap();
            let clone = fixture.path().join("clone");
            fs::create_dir(&clone).unwrap();
            let fifo = clone.join("fifo");
            let c_fifo = c_path(&fifo).unwrap();
            assert_eq!(unsafe { libc::mkfifo(c_fifo.as_ptr(), 0o600) }, 0);
            fs::set_permissions(&clone, fs::Permissions::from_mode(0o555)).unwrap();
            assert!(prepare_guest_clone_metadata(&clone).is_err());
            assert_eq!(mode(&clone) & 0o7777, 0o555);
        }
    }
}

#[cfg(target_os = "macos")]
pub(super) fn prepare_guest_clone_metadata(root: &Path) -> Result<()> {
    macos::prepare_guest_clone_metadata(root)
}
