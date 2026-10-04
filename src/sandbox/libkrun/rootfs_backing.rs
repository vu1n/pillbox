//! Private native rootfs storage. A seed and every clone must stay on one
//! case-sensitive filesystem; a cache marker alone cannot establish that.

use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};

const PREPARATION_LIMIT: Duration = Duration::from_secs(120);

pub(super) struct RootfsBacking {
    root: PathBuf,
    device: u64,
}

impl RootfsBacking {
    pub(super) fn krun_dir() -> Result<PathBuf> {
        let home =
            std::env::var_os("HOME").context("could not resolve $HOME for rootfs backing")?;
        let pillbox = PathBuf::from(home).join(".pillbox");
        ensure!(
            pillbox.is_absolute(),
            "rootfs backing home must be absolute"
        );
        owned_directory(&pillbox, true)?;
        fs::set_permissions(&pillbox, fs::Permissions::from_mode(0o700))?;
        let krun = pillbox.join("krun");
        owned_directory(&krun, true)?;
        fs::set_permissions(&krun, fs::Permissions::from_mode(0o700))?;
        Ok(krun)
    }

    pub(super) fn prepare(
        krun: &Path,
        deadline: Instant,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self> {
        prepare(krun, deadline, cancelled, false)
    }

    pub(super) fn reopen(krun: &Path) -> Result<Self> {
        let deadline = Instant::now()
            .checked_add(PREPARATION_LIMIT)
            .context("rootfs backing deadline overflow")?;
        prepare(krun, deadline, &|| false, true)
    }

    pub(super) fn reopen_bounded(
        krun: &Path,
        deadline: Instant,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self> {
        prepare(krun, deadline, cancelled, true)
    }

    pub(super) fn ordinary_deadline() -> Result<Instant> {
        Instant::now()
            .checked_add(PREPARATION_LIMIT)
            .context("rootfs backing deadline overflow")
    }

    pub(super) fn expected_root(krun: &Path) -> PathBuf {
        #[cfg(target_os = "macos")]
        {
            krun.join("case-sensitive-rootfs")
        }
        #[cfg(not(target_os = "macos"))]
        {
            krun.to_path_buf()
        }
    }

    pub(super) fn namespace(
        &self,
        name: &str,
        deadline: Instant,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<PathBuf> {
        self.namespace_with_probe(name, deadline, cancelled, probe_case_sensitive)
    }

    fn namespace_with_probe(
        &self,
        name: &str,
        deadline: Instant,
        cancelled: &dyn Fn() -> bool,
        probe: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<PathBuf> {
        check_live(deadline, cancelled)?;
        ensure!(
            !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
            "invalid rootfs backing namespace"
        );
        let path = self.root.join(name);
        owned_directory(&path, true)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        self.ensure_same_device(&path)?;
        // Linux can enable case folding on one directory without changing st_dev.
        // Probe a disposable child of the namespace before admitting its contents.
        probe(&path).with_context(|| format!("probe rootfs namespace {}", path.display()))?;
        check_live(deadline, cancelled)?;
        Ok(path)
    }

    pub(super) fn ensure_same_device(&self, path: &Path) -> Result<()> {
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("inspect rootfs backing path {}", path.display()))?;
        ensure!(
            metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && metadata.dev() == self.device,
            "rootfs path is not a plain directory on the verified backing: {}",
            path.display()
        );
        Ok(())
    }
}

fn check_live(deadline: Instant, cancelled: &dyn Fn() -> bool) -> Result<()> {
    ensure!(!cancelled(), "rootfs backing preparation cancelled");
    ensure!(
        Instant::now() < deadline,
        "rootfs backing preparation deadline exceeded"
    );
    Ok(())
}

fn owned_directory(path: &Path, create: bool) -> Result<()> {
    let owner = unsafe { libc::geteuid() };
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                ensure!(
                    metadata.is_dir() && !metadata.file_type().is_symlink(),
                    "rootfs backing path contains a non-directory or symlink: {}",
                    current.display()
                );
                if current == path {
                    ensure!(
                        metadata.uid() == owner,
                        "rootfs backing directory has a foreign owner: {}",
                        path.display()
                    );
                }
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound && current == path && create =>
            {
                fs::DirBuilder::new()
                    .mode(0o700)
                    .create(path)
                    .with_context(|| {
                        format!("create private rootfs backing directory {}", path.display())
                    })?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect rootfs backing path {}", current.display()))
            }
        }
    }
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.uid() == owner,
        "rootfs backing directory has a foreign owner: {}",
        path.display()
    );
    Ok(())
}

fn probe_case_sensitive(root: &Path) -> Result<()> {
    let probe = tempfile::Builder::new()
        .prefix(".pillbox-case-probe-")
        .tempdir_in(root)
        .context("create private rootfs case probe")?;
    let lower = probe.path().join("case");
    let upper = probe.path().join("CASE");
    fs::write(&lower, b"lower").context("write lower-case rootfs probe")?;
    fs::write(&upper, b"upper").context("write upper-case rootfs probe")?;
    let lower_meta = fs::symlink_metadata(&lower)?;
    let upper_meta = fs::symlink_metadata(&upper)?;
    check_case_observation(
        lower_meta.ino(),
        upper_meta.ino(),
        &fs::read(&lower)?,
        &fs::read(&upper)?,
    )?;
    probe.close().context("remove rootfs case probe")
}

fn check_case_observation(
    lower_ino: u64,
    upper_ino: u64,
    lower: &[u8],
    upper: &[u8],
) -> Result<()> {
    ensure!(
        lower_ino != upper_ino && lower == b"lower" && upper == b"upper",
        "native rootfs backing folds case; distinct names alias"
    );
    Ok(())
}

fn require_headroom(host: &Path, volume: &Path) -> Result<()> {
    check_headroom(
        super::host::disk_headroom(host),
        super::host::disk_headroom(volume),
    )
}

fn check_headroom(host_free: u64, volume_free: u64) -> Result<()> {
    let floor = super::host::MIN_HEADROOM_BYTES;
    ensure!(
        host_free >= floor,
        "insufficient or unknown host space for rootfs backing (2 GiB required)"
    );
    ensure!(
        volume_free >= floor,
        "insufficient or unknown rootfs volume space (2 GiB required)"
    );
    Ok(())
}

fn prepare(
    krun: &Path,
    deadline: Instant,
    cancelled: &dyn Fn() -> bool,
    existing_only: bool,
) -> Result<RootfsBacking> {
    check_live(deadline, cancelled)?;
    ensure!(krun.is_absolute(), "rootfs backing path must be absolute");
    owned_directory(krun, !existing_only)?;
    fs::set_permissions(krun, fs::Permissions::from_mode(0o700))
        .context("keep owned rootfs backing parent private")?;

    #[cfg(target_os = "macos")]
    let root = macos::prepare(krun, deadline, cancelled, existing_only)?;
    #[cfg(not(target_os = "macos"))]
    let root = krun.to_path_buf();

    check_live(deadline, cancelled)?;
    probe_case_sensitive(&root)?;
    require_headroom(krun, &root)?;
    let device = fs::symlink_metadata(&root)?.dev();
    Ok(RootfsBacking { root, device })
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::ffi::{CStr, CString};
    use std::fs::OpenOptions;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    use std::process::Command;

    use serde::{Deserialize, Serialize};

    const LOGICAL_BYTES: u64 = 64 * 1024 * 1024 * 1024;
    const MARKER_MAGIC: &str = "pillbox-case-sensitive-rootfs/v1";
    const POLL: Duration = Duration::from_millis(20);

    #[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    struct Identity {
        magic: String,
        token: String,
        image_device: u64,
        image_inode: u64,
        logical_bytes: u64,
    }

    pub(super) struct Mount {
        pub(super) source: String,
        pub(super) filesystem: String,
        pub(super) mountpoint: String,
    }

    pub(super) fn prepare(
        krun: &Path,
        deadline: Instant,
        cancelled: &dyn Fn() -> bool,
        existing_only: bool,
    ) -> Result<PathBuf> {
        let lock_path = krun.join("rootfs-backing.lock");
        let lock = OpenOptions::new()
            .create(!existing_only)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&lock_path)
            .context("open private rootfs backing lock")?;
        let lock_meta = lock.metadata()?;
        ensure!(
            lock_meta.is_file()
                && lock_meta.uid() == unsafe { libc::geteuid() }
                && lock_meta.permissions().mode() & 0o077 == 0,
            "rootfs backing lock is not an owned private regular file"
        );
        loop {
            check_live(deadline, cancelled)?;
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if !matches!(
                error.raw_os_error(),
                Some(libc::EWOULDBLOCK) | Some(libc::EINTR)
            ) {
                return Err(error).context("lock rootfs backing creation and attach");
            }
            std::thread::sleep(POLL);
        }

        let image = krun.join("case-sensitive-rootfs.sparsebundle");
        let mountpoint = krun.join("case-sensitive-rootfs");
        let identity_path = krun.join("rootfs-backing.identity.json");
        let image_exists = present(&image)?;
        let identity_exists = present(&identity_path)?;
        ensure!(
            image_exists == identity_exists
                || (!image_exists && !identity_exists && !existing_only),
            "rootfs backing image and identity are incomplete; refusing unknown state"
        );
        let fresh = !image_exists;
        if fresh {
            ensure!(
                !existing_only,
                "rootfs backing does not exist for recorded session"
            );
            require_headroom(krun, krun)?;
            let mut create = Command::new("hdiutil");
            create
                .args([
                    "create",
                    "-size",
                    "64g",
                    "-type",
                    "SPARSEBUNDLE",
                    "-fs",
                    "Case-sensitive APFS",
                    "-volname",
                    "PillboxRootfs",
                    "-nospotlight",
                ])
                .arg(&image);
            super::super::repository::run_command(
                &mut create,
                deadline,
                cancelled,
                "create 64 GiB case-sensitive rootfs image",
            )?;
        }
        let image_meta = fs::symlink_metadata(&image).context("inspect rootfs sparse bundle")?;
        ensure!(
            image_meta.is_dir()
                && !image_meta.file_type().is_symlink()
                && image_meta.uid() == unsafe { libc::geteuid() },
            "rootfs sparse bundle is not an owned plain directory"
        );
        if fresh {
            fs::set_permissions(&image, fs::Permissions::from_mode(0o700))?;
        } else {
            ensure!(
                image_meta.permissions().mode() & 0o077 == 0,
                "rootfs sparse bundle is not private"
            );
        }
        let expected = if fresh {
            None
        } else {
            let identity: Identity = read_marker(&identity_path)?;
            ensure!(
                identity.magic == MARKER_MAGIC
                    && identity.logical_bytes == LOGICAL_BYTES
                    && identity.image_device == image_meta.dev()
                    && identity.image_inode == image_meta.ino()
                    && !identity.token.is_empty(),
                "rootfs sparse bundle identity or logical capacity changed"
            );
            Some(identity)
        };
        owned_directory(&mountpoint, !existing_only)?;

        let info = hdiutil_info(krun, deadline, cancelled)?;
        let mapped = image_mount(&info, &image, &mountpoint)?;
        let mounted = mounted_at(&mountpoint)?;
        match (mapped, mounted) {
            (Some(source), Some(mount)) => verify_mount(&source, &mountpoint, &mount)?,
            (None, None) => {
                let mut attach = Command::new("hdiutil");
                attach
                    .args([
                        "attach",
                        "-nobrowse",
                        "-noautoopen",
                        "-owners",
                        "on",
                        "-mountpoint",
                    ])
                    .arg(&mountpoint)
                    .arg(&image);
                super::super::repository::run_command(
                    &mut attach,
                    deadline,
                    cancelled,
                    "attach rootfs backing",
                )?;
                let info = hdiutil_info(krun, deadline, cancelled)?;
                let source = image_mount(&info, &image, &mountpoint)?
                    .context("attached rootfs image has no expected mount")?;
                let mount =
                    mounted_at(&mountpoint)?.context("attached rootfs image is not mounted")?;
                verify_mount(&source, &mountpoint, &mount)?;
            }
            _ => bail!("rootfs backing mount and sparse bundle identity disagree"),
        }
        check_live(deadline, cancelled)?;
        let volume_meta = fs::symlink_metadata(&mountpoint)?;
        ensure!(
            volume_meta.uid() == unsafe { libc::geteuid() },
            "rootfs volume has a foreign owner"
        );
        if let Some(expected) = expected.as_ref() {
            ensure!(
                volume_meta.permissions().mode() & 0o077 == 0,
                "rootfs volume root is not private"
            );
            ensure!(
                read_marker(&mountpoint.join(".pillbox-rootfs-backing"))? == *expected,
                "mounted rootfs volume identity mismatch"
            );
        } else {
            fs::set_permissions(&mountpoint, fs::Permissions::from_mode(0o700))?;
        }
        probe_case_sensitive(&mountpoint)?;
        require_headroom(krun, &mountpoint)?;

        let marker = mountpoint.join(".pillbox-rootfs-backing");
        if fresh {
            let identity = Identity {
                magic: MARKER_MAGIC.into(),
                token: uuid::Uuid::now_v7().to_string(),
                image_device: image_meta.dev(),
                image_inode: image_meta.ino(),
                logical_bytes: LOGICAL_BYTES,
            };
            write_new(&marker, &serde_json::to_vec(&identity)?)?;
            write_new(&identity_path, &serde_json::to_vec(&identity)?)?;
        }
        Ok(mountpoint)
    }

    fn present(path: &Path) -> Result<bool> {
        match fs::symlink_metadata(path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => {
                Err(error).with_context(|| format!("inspect rootfs backing {}", path.display()))
            }
        }
    }

    fn read_marker(path: &Path) -> Result<Identity> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .with_context(|| format!("open rootfs identity {}", path.display()))?;
        let meta = file.metadata()?;
        ensure!(
            meta.is_file()
                && meta.uid() == unsafe { libc::geteuid() }
                && meta.permissions().mode() & 0o077 == 0
                && meta.len() <= 512,
            "rootfs backing identity is not an owned private bounded file"
        );
        let mut bytes = Vec::new();
        file.take(513).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 512, "rootfs backing identity exceeds limit");
        serde_json::from_slice(&bytes).context("parse rootfs backing identity")
    }

    #[cfg(test)]
    pub(super) fn validate_marker_for_test(path: &Path) -> Result<()> {
        read_marker(path).map(|_| ())
    }

    fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .with_context(|| format!("create rootfs backing identity {}", path.display()))?;
        file.write_all(bytes)?;
        file.sync_all().context("sync rootfs backing identity")
    }

    fn hdiutil_info(
        krun: &Path,
        deadline: Instant,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<serde_json::Value> {
        let mut command = Command::new("hdiutil");
        command.args(["info", "-plist"]);
        let plist = super::super::repository::run_command(
            &mut command,
            deadline,
            cancelled,
            "inspect attached rootfs images",
        )?;
        let temporary = tempfile::Builder::new()
            .prefix("rootfs-plist-")
            .tempdir_in(krun)?;
        let path = temporary.path().join("info.plist");
        crate::paths::write_private_file(&path, &plist)?;
        let mut convert = Command::new("plutil");
        convert
            .args(["-convert", "json", "-o", "-", "--"])
            .arg(&path);
        let json = super::super::repository::run_command(
            &mut convert,
            deadline,
            cancelled,
            "parse attached rootfs images",
        )?;
        serde_json::from_slice(&json).context("decode attached rootfs images")
    }

    pub(super) fn image_mount(
        info: &serde_json::Value,
        image: &Path,
        mountpoint: &Path,
    ) -> Result<Option<String>> {
        let images = info
            .get("images")
            .and_then(serde_json::Value::as_array)
            .context("hdiutil info has no images array")?;
        let mut match_source = None;
        let mut seen_image = false;
        for record in images {
            let image_path = record.get("image-path").and_then(serde_json::Value::as_str);
            let ours = image_path.is_some_and(|path| Path::new(path) == image);
            if ours {
                ensure!(
                    !seen_image,
                    "rootfs sparse bundle has duplicate attachments"
                );
                seen_image = true;
                let count = record
                    .get("blockcount")
                    .and_then(serde_json::Value::as_u64)
                    .context("rootfs image has no block count")?;
                let size = record
                    .get("blocksize")
                    .and_then(serde_json::Value::as_u64)
                    .context("rootfs image has no block size")?;
                ensure!(
                    count.checked_mul(size) == Some(LOGICAL_BYTES),
                    "rootfs sparse bundle logical capacity is not 64 GiB"
                );
            }
            if let Some(entities) = record
                .get("system-entities")
                .and_then(serde_json::Value::as_array)
            {
                for entity in entities {
                    let mounted = entity
                        .get("mount-point")
                        .and_then(serde_json::Value::as_str);
                    if mounted.is_some_and(|path| Path::new(path) == mountpoint) {
                        ensure!(ours, "foreign image occupies rootfs backing mountpoint");
                        let source = entity
                            .get("dev-entry")
                            .and_then(serde_json::Value::as_str)
                            .context("rootfs mount has no device entry")?;
                        ensure!(
                            match_source.is_none(),
                            "rootfs image has duplicate mount entries"
                        );
                        match_source = Some(source.to_owned());
                    } else if ours && mounted.is_some() {
                        bail!("rootfs sparse bundle is mounted at a different path");
                    }
                }
            }
        }
        ensure!(
            !seen_image || match_source.is_some(),
            "rootfs sparse bundle is attached without the expected mount"
        );
        Ok(match_source)
    }

    fn mounted_at(path: &Path) -> Result<Option<Mount>> {
        let parent = path.parent().context("rootfs mountpoint has no parent")?;
        if fs::metadata(path)?.dev() == fs::metadata(parent)?.dev() {
            return Ok(None);
        }
        let cpath = CString::new(path.as_os_str().as_bytes())?;
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(cpath.as_ptr(), &mut stat) } != 0 {
            return Err(std::io::Error::last_os_error()).context("stat rootfs mount");
        }
        let field = |value: &[libc::c_char]| {
            unsafe { CStr::from_ptr(value.as_ptr()) }
                .to_string_lossy()
                .into_owned()
        };
        Ok(Some(Mount {
            source: field(&stat.f_mntfromname),
            filesystem: field(&stat.f_fstypename),
            mountpoint: field(&stat.f_mntonname),
        }))
    }

    pub(super) fn verify_mount(source: &str, mountpoint: &Path, mount: &Mount) -> Result<()> {
        ensure!(
            mount.filesystem == "apfs"
                && mount.source == source
                && Path::new(&mount.mountpoint) == mountpoint,
            "rootfs backing mount device, filesystem, or mountpoint mismatch"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_probe_distinguishes_spelling_without_touching_other_files() {
        let fixture = tempfile::tempdir().unwrap();
        fs::write(fixture.path().join("keep"), b"unchanged").unwrap();
        // The host test volume may itself be case-folded; both observations
        // still prove the probe leaves unrelated files alone.
        let result = probe_case_sensitive(fixture.path());
        assert!(result.is_ok() || result.unwrap_err().to_string().contains("folds case"));
        assert_eq!(fs::read(fixture.path().join("keep")).unwrap(), b"unchanged");
        assert_eq!(fs::read_dir(fixture.path()).unwrap().count(), 1);
        assert!(check_case_observation(1, 1, b"upper", b"upper").is_err());
        check_case_observation(1, 2, b"lower", b"upper").unwrap();
    }

    #[test]
    fn namespace_rejects_failed_case_probe_without_touching_existing_contents() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        let namespace = root.join("rootfs");
        fs::create_dir(&namespace).unwrap();
        fs::write(namespace.join("seed"), b"unchanged").unwrap();
        let backing = RootfsBacking {
            device: fs::symlink_metadata(&root).unwrap().dev(),
            root,
        };
        let mut probed = false;
        let error = backing
            .namespace_with_probe(
                "rootfs",
                RootfsBacking::ordinary_deadline().unwrap(),
                &|| false,
                |path| {
                    assert_eq!(path, namespace);
                    probed = true;
                    bail!("case folding observed inside namespace")
                },
            )
            .unwrap_err();
        assert!(probed);
        assert!(format!("{error:#}").contains("case folding observed inside namespace"));
        assert_eq!(fs::read(namespace.join("seed")).unwrap(), b"unchanged");

        let mut probed_after_cancel = false;
        assert!(backing
            .namespace_with_probe(
                "rootfs",
                RootfsBacking::ordinary_deadline().unwrap(),
                &|| true,
                |_| {
                    probed_after_cancel = true;
                    Ok(())
                },
            )
            .is_err());
        assert!(!probed_after_cancel);
    }

    #[test]
    fn unknown_or_low_space_on_either_filesystem_blocks_backing() {
        let floor = super::super::host::MIN_HEADROOM_BYTES;
        assert!(check_headroom(floor, floor).is_ok());
        assert!(check_headroom(0, floor).is_err());
        assert!(check_headroom(floor, 0).is_err());
        assert!(check_headroom(floor, floor - 1).is_err());
    }

    #[test]
    fn backing_path_rejects_symlink_and_foreign_shape() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        let plain = root.join("plain");
        fs::create_dir(&plain).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&plain, &link).unwrap();
        assert!(owned_directory(&link, false).is_err());
        assert!(owned_directory(&plain, false).is_ok());
        let file = root.join("file");
        fs::write(&file, b"not a directory").unwrap();
        assert!(owned_directory(&file, false).is_err());
        if unsafe { libc::geteuid() } != 0 {
            assert!(owned_directory(Path::new("/"), false).is_err());
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn fifo_marker_fixture() {
        let Some(path) = std::env::var_os("PILLBOX_TEST_FIFO_MARKER") else {
            return;
        };
        let error = macos::validate_marker_for_test(Path::new(&path)).unwrap_err();
        assert!(
            format!("{error:#}").contains("not an owned private bounded file"),
            "unexpected marker rejection: {error:#}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn private_fifo_marker_is_rejected_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::process::{Command, Stdio};

        let fixture = tempfile::tempdir().unwrap();
        let fifo = fixture.path().join("identity.json");
        let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "sandbox::libkrun::rootfs_backing::tests::fifo_marker_fixture",
                "--nocapture",
            ])
            .env("PILLBOX_TEST_FIFO_MARKER", &fifo)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if Instant::now() >= deadline {
                let stopped = child.kill();
                let reaped = child.wait();
                panic!(
                    "FIFO marker admission blocked past deadline; stop={stopped:?}, reap={reaped:?}"
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "FIFO marker child failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("fifo_marker_fixture ... ok"),
            "FIFO marker fixture did not run: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn wrong_and_duplicate_mount_identity_fail_closed() {
        let image = Path::new("/private/rootfs.sparsebundle");
        let mount = Path::new("/private/rootfs");
        let wrong = serde_json::json!({"images":[{"image-path":"/private/foreign.sparsebundle","system-entities":[{"dev-entry":"/dev/disk5s1","mount-point":"/private/rootfs"}]}]});
        assert!(macos::image_mount(&wrong, image, mount).is_err());
        let valid = serde_json::json!({"images":[{"image-path":"/private/rootfs.sparsebundle","blockcount":134217728,"blocksize":512,"system-entities":[{"dev-entry":"/dev/disk5s1","mount-point":"/private/rootfs"}]}]});
        assert_eq!(
            macos::image_mount(&valid, image, mount).unwrap().as_deref(),
            Some("/dev/disk5s1")
        );
        let wrong_capacity = serde_json::json!({"images":[{"image-path":"/private/rootfs.sparsebundle","blockcount":1024,"blocksize":512,"system-entities":[{"dev-entry":"/dev/disk5s1","mount-point":"/private/rootfs"}]}]});
        assert!(macos::image_mount(&wrong_capacity, image, mount).is_err());
        let observed = macos::Mount {
            source: "/dev/disk6s1".into(),
            filesystem: "apfs".into(),
            mountpoint: "/private/rootfs".into(),
        };
        assert!(macos::verify_mount("/dev/disk5s1", mount, &observed).is_err());
    }
}
