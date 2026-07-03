//! Copy-on-write directory clone — the **workspace fork** primitive.
//!
//! A swarm forks `k` workers from one base workspace; the fork must be near-free
//! ("moves no per-run bytes" — see [`super::ingest`]). On APFS that's `clonefile`;
//! on a Linux CoW filesystem (Btrfs / XFS / bcachefs / ZFS ≥ 2.2) it's a
//! `FICLONE` reflink. This module is the single cross-platform seam so the fork
//! mechanism isn't locked to one VMM backend or one OS: the macOS/HVF libkrun
//! backend and the intended Linux/KVM QEMU backend call the same [`cow_clone_dir`].
//!
//! Where the filesystem can't reflink (ext4, tmpfs, cross-device), the clone
//! degrades to a full byte copy — correct, but O(tree) I/O rather than CoW. The
//! outcome is **reported, not silent** ([`CloneMethod`]) so the caller can warn.
//!
//! Compiled unconditionally (like [`super::ingest`]) so both CI targets
//! (ubuntu + macOS) exercise it; used behind the `libkrun` feature today.
//! Context: doc://pillbox/workspace-cow-fork@latest#workspace-cow-fork
#![allow(dead_code)]

use std::path::Path;

use anyhow::{bail, Result};

/// How a CoW clone was materialized — surfaced so a caller can note when the
/// filesystem forced a full copy (the "free fork" promise silently broke).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloneMethod {
    /// Copy-on-write: APFS `clonefile` (macOS) or a `FICLONE` reflink (Linux CoW
    /// filesystem). No data blocks copied — the fork is near-free.
    Reflink,
    /// Full byte copy: the filesystem has no reflink support (ext4, tmpfs) or the
    /// clone crossed a device boundary. Correct, but not CoW.
    Copied,
}

/// CoW-clone the directory tree at `src` to `dst`, recursively. `dst` **must not
/// exist** — the primitive creates it (matching APFS `clonefile`'s semantics).
///
/// Returns whether the tree was reflinked or fell back to a byte copy. Regular
/// files, directories, and symlinks are cloned; other node types (sockets,
/// fifos, devices) are skipped — a workspace fork has no business copying them.
pub(crate) fn cow_clone_dir(src: &Path, dst: &Path) -> Result<CloneMethod> {
    if dst.exists() {
        bail!("cow clone target {} already exists", dst.display());
    }
    clone_impl(src, dst)
}

// ── macOS: APFS clonefile (recursive, native) ──────────────────────────────

/// macOS APFS copy-on-write clone (recursive for directories), from libSystem.
#[cfg(target_os = "macos")]
fn clone_impl(src: &Path, dst: &Path) -> Result<CloneMethod> {
    use std::ffi::CString;
    use std::os::raw::{c_char, c_int};
    use std::os::unix::ffi::OsStrExt;

    extern "C" {
        fn clonefile(src: *const c_char, dst: *const c_char, flags: u32) -> c_int;
    }

    // as_bytes (not to_string_lossy) so a non-UTF-8 path clones verbatim rather
    // than being mangled into a wrong target.
    let src_c = CString::new(src.as_os_str().as_bytes())
        .map_err(|_| anyhow::anyhow!("src path {} has an interior NUL", src.display()))?;
    let dst_c = CString::new(dst.as_os_str().as_bytes())
        .map_err(|_| anyhow::anyhow!("dst path {} has an interior NUL", dst.display()))?;
    let rc = unsafe { clonefile(src_c.as_ptr(), dst_c.as_ptr(), 0) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        let _ = std::fs::remove_dir_all(dst); // don't leave a half clone behind
        bail!("clonefile {} → {} failed: {err}", src.display(), dst.display());
    }
    Ok(CloneMethod::Reflink)
}

// ── Linux / other unix: recursive walk + per-file reflink-or-copy ───────────

#[cfg(all(unix, not(target_os = "macos")))]
fn clone_impl(src: &Path, dst: &Path) -> Result<CloneMethod> {
    // Start optimistic; any per-file copy fallback downgrades the whole clone.
    let mut method = CloneMethod::Reflink;
    if let Err(e) = clone_tree(src, dst, &mut method) {
        let _ = std::fs::remove_dir_all(dst); // don't leave a half clone behind
        return Err(e);
    }
    Ok(method)
}

/// Recreate the tree at `src` under `dst`: directories are made, symlinks are
/// recreated (link target copied, never followed), and regular files are
/// reflinked when the filesystem supports it, else byte-copied (downgrading
/// `method`). Uses `symlink_metadata` throughout so a symlink is never followed.
#[cfg(all(unix, not(target_os = "macos")))]
fn clone_tree(src: &Path, dst: &Path, method: &mut CloneMethod) -> Result<()> {
    use anyhow::Context;

    let meta = std::fs::symlink_metadata(src)
        .with_context(|| format!("stat {} for cow clone", src.display()))?;
    let ft = meta.file_type();

    if ft.is_dir() {
        std::fs::create_dir(dst).with_context(|| format!("create dir {}", dst.display()))?;
        for entry in std::fs::read_dir(src).with_context(|| format!("read dir {}", src.display()))? {
            let entry = entry.with_context(|| format!("entry under {}", src.display()))?;
            clone_tree(&entry.path(), &dst.join(entry.file_name()), method)?;
        }
        // Set the dir's mode AFTER populating it. A read-only source dir (a 0o555
        // cargo-registry / go-mod-cache / Nix-store entry is common) must stay
        // writable while we create its children, or those creations fail EACCES.
        std::fs::set_permissions(dst, meta.permissions())
            .with_context(|| format!("chmod {}", dst.display()))?;
    } else if ft.is_symlink() {
        let target =
            std::fs::read_link(src).with_context(|| format!("readlink {}", src.display()))?;
        std::os::unix::fs::symlink(&target, dst)
            .with_context(|| format!("symlink {} → {}", dst.display(), target.display()))?;
    } else if ft.is_file() {
        // Pass the mode we already stat'd so the reflink path doesn't re-stat src.
        if !try_reflink(src, dst, meta.permissions())? {
            std::fs::copy(src, dst)
                .with_context(|| format!("copy {} → {}", src.display(), dst.display()))?;
            *method = CloneMethod::Copied;
        }
    }
    // Sockets / fifos / block+char devices: intentionally skipped.
    Ok(())
}

/// Reflink `src` → `dst` via the `FICLONE` ioctl. `Ok(true)` = reflinked;
/// `Ok(false)` = the filesystem/device can't reflink and the caller should
/// byte-copy (any leftover `dst` has been removed). Never corrupts: a failed or
/// unsupported ioctl falls through to copy.
#[cfg(target_os = "linux")]
fn try_reflink(src: &Path, dst: &Path, mode: std::fs::Permissions) -> Result<bool> {
    use anyhow::Context;
    use std::os::unix::io::AsRawFd;

    // FICLONE == _IOW(0x94, 9, int) under the asm-generic ioctl ABI (x86, arm,
    // arm64, riscv, s390 — the arches pillbox targets). The few arches with a
    // different _IOC layout (mips/powerpc/sparc/alpha) get EINVAL/ENOTTY and fall
    // through to copy — safe: a wrong ioctl number fails, it never corrupts.
    // Untyped so the `as _` at the call site adapts to `ioctl`'s request arg,
    // which is `c_ulong` on glibc but `c_int` on musl (the value fits both).
    const FICLONE: u64 = 0x4004_9409;

    let src_f = std::fs::File::open(src).with_context(|| format!("open {}", src.display()))?;
    // create_new: dst must not pre-exist (it doesn't — the tree is fresh).
    let dst_f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)
        .with_context(|| format!("create {}", dst.display()))?;

    let rc = unsafe { libc::ioctl(dst_f.as_raw_fd(), FICLONE as _, src_f.as_raw_fd()) };
    if rc == 0 {
        // FICLONE clones data only; the new file kept its create-time (umask'd)
        // mode. Restore src's mode so 0600 creds / +x binaries survive the fork.
        dst_f
            .set_permissions(mode)
            .with_context(|| format!("chmod {}", dst.display()))?;
        return Ok(true);
    }
    // Unsupported (EXDEV/EOPNOTSUPP/EINVAL/…): drop the empty file and let the
    // caller copy. Close the fd before removing.
    drop(dst_f);
    let _ = std::fs::remove_file(dst);
    Ok(false)
}

/// Non-Linux unix (BSD/illumos): no portable reflink primitive here, so always
/// signal "copy". `dst` is untouched, so the caller creates it via the copy.
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn try_reflink(_src: &Path, _dst: &Path, _mode: std::fs::Permissions) -> Result<bool> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    /// Build a small source tree: a file, a nested dir + file, and (unix) a
    /// symlink. Returns the tempdir + the `src` root inside it.
    fn sample_tree() -> (TempDir, std::path::PathBuf) {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("top.txt"), b"AAAA").unwrap();
        fs::create_dir(src.join("sub")).unwrap();
        fs::write(src.join("sub/nested.txt"), b"nested").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("top.txt", src.join("link")).unwrap();
        (tmp, src)
    }

    #[test]
    fn clones_tree_contents() {
        let (tmp, src) = sample_tree();
        let dst = tmp.path().join("dst");
        let method = cow_clone_dir(&src, &dst).unwrap();
        // Either mechanism is acceptable — CI filesystems (ext4/tmpfs/APFS) vary;
        // the contract is a correct clone, not necessarily a reflink here.
        assert!(matches!(method, CloneMethod::Reflink | CloneMethod::Copied));
        assert_eq!(fs::read(dst.join("top.txt")).unwrap(), b"AAAA");
        assert_eq!(fs::read(dst.join("sub/nested.txt")).unwrap(), b"nested");
    }

    /// The CoW invariant: the clone is an *independent* copy, not a shared-storage
    /// alias. Writing one side must not disturb the other — this is what makes a
    /// reflink fork safe (a hardlink would fail this).
    #[test]
    fn clone_is_independent() {
        let (tmp, src) = sample_tree();
        let dst = tmp.path().join("dst");
        cow_clone_dir(&src, &dst).unwrap();

        // Overwrite the clone with a different length → forces a block divergence.
        fs::write(dst.join("top.txt"), b"BBBBBBBB").unwrap();
        assert_eq!(fs::read(src.join("top.txt")).unwrap(), b"AAAA", "src unchanged");

        // And the reverse direction.
        fs::write(src.join("top.txt"), b"CC").unwrap();
        assert_eq!(fs::read(dst.join("top.txt")).unwrap(), b"BBBBBBBB", "dst unchanged");
    }

    #[cfg(unix)]
    #[test]
    fn preserves_mode() {
        let (tmp, src) = sample_tree();
        fs::set_permissions(src.join("top.txt"), fs::Permissions::from_mode(0o600)).unwrap();
        let dst = tmp.path().join("dst");
        cow_clone_dir(&src, &dst).unwrap();
        let mode = fs::metadata(dst.join("top.txt")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "0600 must survive the fork");
    }

    #[cfg(unix)]
    #[test]
    fn clones_a_read_only_source_dir() {
        // A read-only source dir (0o555 — cargo registry / go mod cache shape)
        // must still get its children: perms are applied after the dir is
        // populated, not before (else child creation EACCEs on the Linux path).
        let (tmp, src) = sample_tree();
        fs::set_permissions(src.join("sub"), fs::Permissions::from_mode(0o555)).unwrap();
        let dst = tmp.path().join("dst");
        cow_clone_dir(&src, &dst).unwrap();

        assert_eq!(fs::read(dst.join("sub/nested.txt")).unwrap(), b"nested");
        assert_eq!(
            fs::metadata(dst.join("sub")).unwrap().permissions().mode() & 0o777,
            0o555,
            "restrictive dir mode preserved on the clone"
        );
        // Restore write so both trees can be torn down by the tempdir guard.
        for d in [src.join("sub"), dst.join("sub")] {
            fs::set_permissions(&d, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn preserves_symlinks() {
        let (tmp, src) = sample_tree();
        let dst = tmp.path().join("dst");
        cow_clone_dir(&src, &dst).unwrap();
        let link = dst.join("link");
        let meta = fs::symlink_metadata(&link).unwrap();
        assert!(meta.file_type().is_symlink(), "clone must keep the symlink as a link");
        assert_eq!(fs::read_link(&link).unwrap(), std::path::Path::new("top.txt"));
    }

    #[test]
    fn rejects_existing_dst() {
        let (tmp, src) = sample_tree();
        let dst = tmp.path().join("dst");
        fs::create_dir(&dst).unwrap();
        let err = cow_clone_dir(&src, &dst).unwrap_err().to_string();
        assert!(err.contains("already exists"), "got: {err}");
    }
}
