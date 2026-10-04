//! Each VM gets an independent writable root. Cached image generations are seed
//! trees only; neither a guest nor a cleanup path may write through to that seed.

use std::ops::Deref;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

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
        crate::paths::ensure_mode_0700(runtimes)?;
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
        remove_at(&self.root, self.root.parent().unwrap().parent().unwrap())
    }
}

impl Deref for PrivateRootfs {
    type Target = Path;
    fn deref(&self) -> &Self::Target {
        &self.root
    }
}

pub(super) fn remove_owned(root: &Path) -> Result<()> {
    remove_at(root, &super::krun_cache_dir()?.join("vm-rootfs"))
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
    std::fs::remove_dir_all(directory).context("remove stopped private VM rootfs")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    #[test]
    fn concurrent_and_successor_roots_do_not_share_writes_or_inodes() {
        let fixture = tempfile::tempdir().unwrap();
        let base = fixture.path().join("base");
        fs::create_dir_all(base.join("tmp")).unwrap();
        fs::create_dir(base.join("etc")).unwrap();
        fs::write(base.join("etc/config"), "image default").unwrap();
        fs::set_permissions(base.join("tmp"), fs::Permissions::from_mode(0o1777)).unwrap();
        std::os::unix::fs::symlink("etc/config", base.join("config-link")).unwrap();
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
    #[test]
    #[ignore = "requires signed libkrun binary and freshly exported offline runner rootfs"]
    fn live_concurrent_and_successor_vm_roots_are_isolated() {
        use std::process::Command;
        use std::time::{Duration, Instant};
        let binary = std::env::var_os("PILLBOX_ISOLATION_BINARY").expect("signed binary path");
        let base =
            PathBuf::from(std::env::var_os("PILLBOX_ISOLATION_BASE").expect("fresh rootfs seed"));
        let fixture = tempfile::tempdir().unwrap();
        let probe = fixture.path().join("probe");
        fs::create_dir(&probe).unwrap();
        assert!(!base.join("tmp/pb-isolation-secret").exists());
        assert!(!base.join("etc/pb-isolation-a").exists());
        let runtimes = fixture.path().join("runtimes");
        let a = PrivateRootfs::fork(&base, &runtimes).unwrap();
        let b = PrivateRootfs::fork(&base, &runtimes).unwrap();
        let run = move |mut root: PrivateRootfs, script: &str, spec_path: PathBuf| {
            let spec = super::super::VmSpec {
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
                vsock: None,
                egress: None,
                ownership: None,
            };
            let mut command = Command::new(&binary);
            command
                .arg("__krun-vmm")
                .arg(&spec_path)
                .env_clear()
                .envs(super::super::boot::static_child_env());
            root.preserve();
            let output =
                super::super::repository::run_supervised_vmm(&mut command, spec, &spec_path)
                    .unwrap();
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
        run(
            successor,
            &format!("{clean} && touch /probe/successor-isolated"),
            fixture.path().join("successor.json"),
        );
        assert!(probe.join("successor-isolated").exists());
        assert!(!base.join("tmp/pb-isolation-secret").exists());
        assert!(!base.join("etc/pb-isolation-a").exists());
        assert!(!base.join("etc/pb-isolation-b").exists());
        assert_eq!(fs::read_dir(&runtimes).unwrap().count(), 0);
    }
}
