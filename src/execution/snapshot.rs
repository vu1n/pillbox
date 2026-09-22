//! Host-only immutable Git input and private verifier/patch materialization.
//! No checkout, credentials, repository mount, or repository-defined command reaches a guest.

use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::fs::{DirBuilder, File};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};

use super::files::{
    validate_path, FileEntry, FileLimits, FileTree, MAX_FILES, MAX_OUTPUT_BYTES, MAX_PATH_BYTES,
};

const GIT_TIMEOUT: Duration = Duration::from_secs(30);
const STDERR_LIMIT: usize = 64 * 1024;
const TREE_LIST_LIMIT: usize = MAX_FILES * (MAX_PATH_BYTES + 128);

pub(crate) fn read_git_tree(repo: &Path, commit: &str, limits: &FileLimits) -> Result<FileTree> {
    limits.validate()?;
    ensure!(
        valid_oid(commit),
        "snapshot requires a full nonzero SHA-1 or SHA-256 commit ID"
    );
    let isolated_home = private_directory().context("create isolated Git home")?;
    let deadline = Instant::now() + GIT_TIMEOUT;
    let git =
        |args: &[&str], cap| git_output(repo, isolated_home.path(), args, cap, deadline, false);
    let format = git(&["rev-parse", "--show-object-format=storage"], 32)?;
    let oid_length = match format.as_slice() {
        b"sha1\n" => 40,
        b"sha256\n" => 64,
        _ => bail!("unsupported Git object format"),
    };
    ensure!(
        commit.len() == oid_length,
        "commit ID does not match repository object format"
    );
    let resolved = git(&["rev-parse", "--verify", "--end-of-options", commit], 128)?;
    ensure!(
        std::str::from_utf8(&resolved)?
            .trim_end()
            .eq_ignore_ascii_case(commit),
        "Git resolved a different object identity"
    );
    ensure!(
        git(&["cat-file", "-t", commit], 32)? == b"commit\n",
        "snapshot object is not a commit"
    );
    let listing = git(
        &["ls-tree", "-r", "-z", "-l", "--full-tree", commit],
        TREE_LIST_LIMIT,
    )?;
    let mut objects = Vec::new();
    let mut total = 0_u64;
    ensure!(
        listing.is_empty() || listing.last() == Some(&0),
        "unterminated Git tree listing"
    );
    for terminated in listing.split_inclusive(|byte| *byte == 0) {
        let record = &terminated[..terminated.len() - 1];
        ensure!(
            objects.len() < MAX_FILES,
            "snapshot file count exceeds {MAX_FILES}"
        );
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .context("Git tree entry has no path")?;
        let header: Vec<_> = std::str::from_utf8(&record[..tab])?
            .split_ascii_whitespace()
            .collect();
        ensure!(header.len() == 4, "malformed Git tree entry metadata");
        ensure!(
            matches!(header[0], "100644" | "100755") && header[1] == "blob",
            "snapshot rejects symlinks, submodules and non-regular Git entries"
        );
        ensure!(
            header[2].len() == oid_length && valid_oid(header[2]),
            "malformed Git blob identity"
        );
        let path = std::str::from_utf8(&record[tab + 1..])?;
        validate_path(path)?;
        let size: u64 = header[3].parse().context("invalid Git blob size")?;
        ensure!(
            size <= limits.max_file_bytes,
            "Git blob exceeds per-file byte limit"
        );
        total = total
            .checked_add(size)
            .context("Git snapshot size overflow")?;
        ensure!(
            total <= limits.max_snapshot_bytes,
            "Git tree exceeds snapshot byte limit"
        );
        objects.push((
            path.to_owned(),
            header[0] == "100755",
            header[2].to_owned(),
            size,
        ));
    }
    objects.sort_by(|left, right| left.0.cmp(&right.0));
    validate_disk_paths(objects.iter().map(|entry| entry.0.as_str()))?;
    let mut entries = Vec::with_capacity(objects.len());
    for (path, executable, oid, size) in objects {
        let bytes = git(&["cat-file", "blob", &oid], usize::try_from(size)?)?;
        ensure!(
            bytes.len() as u64 == size,
            "Git blob size changed during snapshot read"
        );
        entries.push(FileEntry {
            path,
            executable,
            bytes,
        });
    }
    FileTree::new(entries, limits)
}

/// `destination` must already be an empty, private directory owned by this user.
/// Every traversal is descriptor-relative and refuses symlinks, including destination parents.
pub(crate) fn materialize(tree: &FileTree, destination: &Path) -> Result<()> {
    validate_disk_paths(tree.entries().iter().map(|entry| entry.path.as_str()))?;
    let root = open_directory_path(destination)?;
    let metadata = root.metadata()?;
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
        "materialization destination must be private and runtime-owned"
    );
    ensure!(
        directory_is_empty(&root)?,
        "materialization destination is not empty"
    );
    for entry in tree.entries() {
        let mut directory = root.try_clone()?;
        let mut components = entry.path.split('/').peekable();
        while let Some(component) = components.next() {
            let name = CString::new(component)?;
            if components.peek().is_some() {
                let result = unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) };
                if result < 0
                    && std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST)
                {
                    return Err(std::io::Error::last_os_error())
                        .context("create materialization directory");
                }
                directory = open_at(
                    directory.as_raw_fd(),
                    &name,
                    libc::O_RDONLY | libc::O_DIRECTORY,
                    0,
                )?;
            } else {
                let mode = if entry.executable { 0o700 } else { 0o600 };
                let mut file = open_at(
                    directory.as_raw_fd(),
                    &name,
                    libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                    mode,
                )?;
                file.write_all(&entry.bytes)
                    .context("write materialized file")?;
                ensure!(
                    unsafe { libc::fchmod(file.as_raw_fd(), mode) } == 0,
                    "set materialized file permissions: {}",
                    std::io::Error::last_os_error()
                );
            }
        }
    }
    Ok(())
}

/// Patch paths are `a/<path>` and `b/<path>`; apply with Git's normal `-p1` behavior.
pub(crate) fn capture_patch(
    base: &FileTree,
    result: &FileTree,
    max_patch_bytes: u64,
) -> Result<Vec<u8>> {
    ensure!(
        max_patch_bytes > 0 && max_patch_bytes <= MAX_OUTPUT_BYTES,
        "patch byte limit must be in 1..={MAX_OUTPUT_BYTES}"
    );
    validate_disk_paths(base.entries().iter().map(|entry| entry.path.as_str()))?;
    validate_disk_paths(result.entries().iter().map(|entry| entry.path.as_str()))?;
    let scratch = private_directory().context("create private patch directory")?;
    let scratch_path = scratch.path().canonicalize()?;
    for (name, tree) in [("a", base), ("b", result)] {
        let destination = scratch_path.join(name);
        DirBuilder::new().mode(0o700).create(&destination)?;
        materialize(tree, &destination)?;
    }
    git_output(
        &scratch_path,
        &scratch_path,
        &[
            "diff",
            "--no-index",
            "--binary",
            "--no-ext-diff",
            "--no-textconv",
            "--no-prefix",
            "--full-index",
            "--no-renames",
            "--no-color",
            "--diff-algorithm=myers",
            "--no-indent-heuristic",
            "--unified=3",
            "--",
            "a",
            "b",
        ],
        usize::try_from(max_patch_bytes)?,
        Instant::now() + GIT_TIMEOUT,
        true,
    )
}

fn valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64)
        && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
        && oid.bytes().any(|byte| byte != b'0')
}

fn private_directory() -> Result<tempfile::TempDir> {
    Ok(tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?)
}

fn validate_disk_paths<'a>(paths: impl Iterator<Item = &'a str>) -> Result<()> {
    let mut names = BTreeMap::new();
    for path in paths {
        validate_path(path)?;
        // Case-insensitive hosts cannot safely represent secret or .git aliases either.
        validate_path(&path.to_ascii_lowercase())?;
        let mut prefix = String::new();
        let mut parts = path.split('/').peekable();
        while let Some(part) = parts.next() {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            let directory = parts.peek().is_some();
            if let Some((previous, was_directory)) =
                names.insert(prefix.to_ascii_lowercase(), (prefix.clone(), directory))
            {
                ensure!(
                    previous == prefix && was_directory && directory,
                    "snapshot has a case collision or conflicting file/directory paths"
                );
            }
        }
    }
    Ok(())
}

fn open_directory_path(path: &Path) -> Result<File> {
    ensure!(
        path.is_absolute(),
        "materialization destination must be absolute"
    );
    let mut directory = open_at(
        libc::AT_FDCWD,
        &CString::new("/")?,
        libc::O_RDONLY | libc::O_DIRECTORY,
        0,
    )?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                use std::os::unix::ffi::OsStrExt;
                directory = open_at(
                    directory.as_raw_fd(),
                    &CString::new(name.as_bytes())?,
                    libc::O_RDONLY | libc::O_DIRECTORY,
                    0,
                )?;
            }
            _ => bail!("materialization destination has a noncanonical component"),
        }
    }
    Ok(directory)
}

fn open_at(parent: RawFd, name: &CStr, flags: i32, mode: libc::mode_t) -> Result<File> {
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    };
    ensure!(
        fd >= 0,
        "open materialization path without following links: {}",
        std::io::Error::last_os_error()
    );
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn directory_is_empty(directory: &File) -> Result<bool> {
    let fd = unsafe { libc::dup(directory.as_raw_fd()) };
    ensure!(fd >= 0, "duplicate materialization directory descriptor");
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        unsafe { libc::close(fd) };
        bail!(
            "open materialization directory stream: {}",
            std::io::Error::last_os_error()
        );
    }
    let result = loop {
        #[cfg(target_os = "macos")]
        unsafe {
            *libc::__error() = 0;
        }
        #[cfg(target_os = "linux")]
        unsafe {
            *libc::__errno_location() = 0;
        }
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            let error = std::io::Error::last_os_error();
            break if error.raw_os_error() == Some(0) {
                Ok(true)
            } else {
                Err(error.into())
            };
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            break Ok(false);
        }
    };
    ensure!(
        unsafe { libc::closedir(stream) } == 0,
        "close materialization directory stream: {}",
        std::io::Error::last_os_error()
    );
    result
}

fn git_output(
    cwd: &Path,
    home: &Path,
    args: &[&str],
    cap: usize,
    deadline: Instant,
    allow_diff: bool,
) -> Result<Vec<u8>> {
    ensure!(
        Instant::now() < deadline,
        "Git snapshot operation timed out"
    );
    let mut command = Command::new("/usr/bin/git");
    command
        .current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home)
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_ALLOW_PROTOCOL", "")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args([
            "--no-replace-objects",
            "-c",
            "protocol.allow=never",
            "-c",
            "credential.helper=",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.attributesFile=/dev/null",
        ])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut process = GitChild {
        child: command.spawn().context("start isolated Git plumbing")?,
        finished: false,
    };
    let mut stdout = process
        .child
        .stdout
        .take()
        .context("Git stdout pipe missing")?;
    let mut stderr = process
        .child
        .stderr
        .take()
        .context("Git stderr pipe missing")?;
    for fd in [stdout.as_raw_fd(), stderr.as_raw_fd()] {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        ensure!(
            flags >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } >= 0,
            "configure bounded Git pipe"
        );
    }
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let mut status: Option<ExitStatus> = None;
    loop {
        ensure!(
            Instant::now() < deadline,
            "Git snapshot operation timed out"
        );
        let out_closed = read_pipe(&mut stdout, &mut out, cap, deadline)?;
        let err_closed = read_pipe(&mut stderr, &mut err, STDERR_LIMIT, deadline)?;
        if status.is_none() {
            status = process.child.try_wait()?;
        }
        if let Some(status) = status {
            if out_closed && err_closed {
                process.finished = true;
                ensure!(
                    status.success() || (allow_diff && status.code() == Some(1)),
                    "Git {} failed with {status}: {}",
                    args[0],
                    String::from_utf8_lossy(&err)
                );
                return Ok(out);
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn read_pipe(
    reader: &mut impl Read,
    output: &mut Vec<u8>,
    cap: usize,
    deadline: Instant,
) -> Result<bool> {
    let mut buffer = [0; 8192];
    loop {
        ensure!(
            Instant::now() < deadline,
            "Git snapshot operation timed out"
        );
        let count = buffer
            .len()
            .min(cap.saturating_sub(output.len()).saturating_add(1));
        match reader.read(&mut buffer[..count]) {
            Ok(0) => return Ok(true),
            Ok(read) => {
                ensure!(
                    read <= cap.saturating_sub(output.len()),
                    "Git output exceeds byte limit"
                );
                output.extend_from_slice(&buffer[..read]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error).context("read bounded Git output"),
        }
    }
}

struct GitChild {
    child: Child,
    finished: bool,
}

impl Drop for GitChild {
    fn drop(&mut self) {
        if !self.finished {
            // The process group was created by this adapter; reap it on every bounded-I/O error.
            let killed = unsafe { libc::kill(-(self.child.id() as i32), libc::SIGKILL) };
            if killed < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
                eprintln!(
                    "failed to terminate Git process group: {}",
                    std::io::Error::last_os_error()
                );
            }
            if let Err(error) = self.child.wait() {
                eprintln!("failed to reap Git process: {error}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn limits() -> FileLimits {
        FileLimits {
            max_file_bytes: 1024,
            max_snapshot_bytes: 4096,
            max_tool_calls: 10,
            max_output_bytes: 4096,
        }
    }

    fn fixture_git(path: &Path, args: &[&str]) -> Vec<u8> {
        git_output(
            path,
            path,
            args,
            1024 * 1024,
            Instant::now() + GIT_TIMEOUT,
            false,
        )
        .unwrap()
    }

    fn repository() -> tempfile::TempDir {
        let dir = private_directory().unwrap();
        fixture_git(dir.path(), &["init", "-q", "--object-format=sha1"]);
        dir
    }

    fn commit(path: &Path) -> String {
        fixture_git(path, &["add", "--all"]);
        commit_index(path)
    }

    fn commit_index(path: &Path) -> String {
        fixture_git(
            path,
            &[
                "-c",
                "user.name=Snapshot Test",
                "-c",
                "user.email=snapshot@example.invalid",
                "commit",
                "--no-gpg-sign",
                "--allow-empty",
                "-qm",
                "fixture",
            ],
        );
        String::from_utf8(fixture_git(path, &["rev-parse", "HEAD"]))
            .unwrap()
            .trim()
            .to_owned()
    }

    fn tree(entries: &[(&str, bool, &[u8])]) -> FileTree {
        FileTree::new(
            entries
                .iter()
                .map(|(path, executable, bytes)| FileEntry {
                    path: (*path).into(),
                    executable: *executable,
                    bytes: bytes.to_vec(),
                })
                .collect(),
            &limits(),
        )
        .unwrap()
    }

    #[test]
    fn exact_commit_ignores_dirty_checkout_moved_head_and_replacements() {
        let repo = repository();
        std::fs::write(repo.path().join("file"), b"original").unwrap();
        let first = commit(repo.path());
        let expected = read_git_tree(repo.path(), &first, &limits()).unwrap();
        std::fs::write(repo.path().join("file"), b"second").unwrap();
        let second = commit(repo.path());
        fixture_git(repo.path(), &["replace", &first, &second]);
        std::fs::write(repo.path().join("file"), b"dirty").unwrap();
        std::fs::write(repo.path().join("untracked"), b"not in snapshot").unwrap();
        assert_eq!(
            read_git_tree(repo.path(), &first, &limits()).unwrap(),
            expected
        );
        assert_eq!(expected.entries()[0].bytes, b"original");
        assert_eq!(
            read_git_tree(repo.path(), &second, &limits())
                .unwrap()
                .entries()[0]
                .bytes,
            b"second"
        );
    }

    #[test]
    fn rejects_partial_zero_wrong_format_and_non_commit_objects() {
        let repo = repository();
        std::fs::write(repo.path().join("file"), b"contents").unwrap();
        let oid = commit(repo.path());
        let blob =
            String::from_utf8(fixture_git(repo.path(), &["rev-parse", "HEAD:file"])).unwrap();
        for invalid in [
            "HEAD",
            "--help",
            &oid[..12],
            &"0".repeat(40),
            &"1".repeat(64),
            blob.trim(),
        ] {
            assert!(
                read_git_tree(repo.path(), invalid, &limits()).is_err(),
                "accepted {invalid}"
            );
        }
        let sha256 = private_directory().unwrap();
        fixture_git(sha256.path(), &["init", "-q", "--object-format=sha256"]);
        std::fs::write(sha256.path().join("file"), b"sha256").unwrap();
        let oid = commit(sha256.path());
        assert_eq!(oid.len(), 64);
        assert_eq!(
            read_git_tree(sha256.path(), &oid, &limits())
                .unwrap()
                .entries()[0]
                .bytes,
            b"sha256"
        );
    }

    #[test]
    fn rejects_symlinks_submodules_secrets_and_oversized_blobs() {
        for forbidden in ["link", "submodule", ".env.local", "oversized"] {
            let repo = repository();
            std::fs::write(repo.path().join("file"), b"contents").unwrap();
            let original = commit(repo.path());
            let oid = match forbidden {
                "link" => {
                    symlink("file", repo.path().join("link")).unwrap();
                    commit(repo.path())
                }
                "submodule" => {
                    fixture_git(
                        repo.path(),
                        &[
                            "update-index",
                            "--add",
                            "--cacheinfo",
                            &format!("160000,{original},submodule"),
                        ],
                    );
                    commit_index(repo.path())
                }
                ".env.local" => {
                    std::fs::write(repo.path().join(forbidden), b"synthetic secret").unwrap();
                    commit(repo.path())
                }
                _ => {
                    std::fs::write(repo.path().join(forbidden), vec![0; 1025]).unwrap();
                    commit(repo.path())
                }
            };
            assert!(
                read_git_tree(repo.path(), &oid, &limits()).is_err(),
                "accepted {forbidden}"
            );
        }
    }

    #[test]
    fn rejects_aggregate_size_file_count_and_missing_promised_objects() {
        let repo = repository();
        std::fs::write(repo.path().join("a"), vec![0; 900]).unwrap();
        std::fs::write(repo.path().join("b"), vec![0; 900]).unwrap();
        let oid = commit(repo.path());
        let small = FileLimits {
            max_snapshot_bytes: 1024,
            ..limits()
        };
        assert!(read_git_tree(repo.path(), &oid, &small).is_err());
        for index in 0..MAX_FILES {
            std::fs::write(repo.path().join(format!("file-{index:04}")), b"").unwrap();
        }
        let many = commit(repo.path());
        assert!(read_git_tree(repo.path(), &many, &limits()).is_err());

        let blob = String::from_utf8(fixture_git(repo.path(), &["rev-parse", "HEAD:a"])).unwrap();
        let blob = blob.trim();
        std::fs::remove_file(
            repo.path()
                .join(".git/objects")
                .join(&blob[..2])
                .join(&blob[2..]),
        )
        .unwrap();
        let marker = repo.path().join("external-helper-ran");
        fixture_git(repo.path(), &["config", "remote.origin.promisor", "true"]);
        fixture_git(
            repo.path(),
            &[
                "config",
                "remote.origin.url",
                &format!("ext::/usr/bin/touch {}", marker.display()),
            ],
        );
        fixture_git(repo.path(), &["config", "protocol.ext.allow", "always"]);
        assert!(read_git_tree(repo.path(), &oid, &limits()).is_err());
        assert!(!marker.exists());
    }

    #[test]
    fn rejects_case_collisions_before_materialization_writes() {
        for entries in [
            vec![
                ("A", false, b"one".as_slice()),
                ("a", false, b"two".as_slice()),
            ],
            vec![
                ("Dir/a", false, b"one".as_slice()),
                ("dir/b", false, b"two".as_slice()),
            ],
            vec![
                ("A", false, b"one".as_slice()),
                ("a/b", false, b"two".as_slice()),
            ],
        ] {
            let tree = tree(&entries);
            let destination = private_directory().unwrap();
            assert!(materialize(&tree, &destination.path().canonicalize().unwrap()).is_err());
            assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 0);
        }
        let repo = repository();
        std::fs::write(repo.path().join("file"), b"contents").unwrap();
        commit(repo.path());
        let blob =
            String::from_utf8(fixture_git(repo.path(), &["rev-parse", "HEAD:file"])).unwrap();
        for path in ["A", "a"] {
            fixture_git(
                repo.path(),
                &[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("100644,{},{}", blob.trim(), path),
                ],
            );
        }
        let oid = commit_index(repo.path());
        assert!(read_git_tree(repo.path(), &oid, &limits()).is_err());
    }

    #[test]
    fn materialization_requires_empty_private_non_symlink_directory() {
        let input = tree(&[("nested/file", true, &[0, 255, 1])]);
        let root = private_directory().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let destination = root_path.join("destination");
        DirBuilder::new().mode(0o700).create(&destination).unwrap();
        materialize(&input, &destination).unwrap();
        let file = destination.join("nested/file");
        assert_eq!(std::fs::read(&file).unwrap(), [0, 255, 1]);
        assert_eq!(std::fs::metadata(&file).unwrap().mode() & 0o777, 0o700);
        assert!(materialize(&input, &destination).is_err());
        symlink(&destination, root_path.join("linked")).unwrap();
        assert!(materialize(&input, &root_path.join("linked")).is_err());
        let empty = root_path.join("empty");
        DirBuilder::new().mode(0o700).create(&empty).unwrap();
        symlink(&root_path, root_path.join("parent-link")).unwrap();
        assert!(materialize(&input, &root_path.join("parent-link/empty")).is_err());
        std::fs::set_permissions(&empty, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(materialize(&input, &empty).is_err());
    }

    #[test]
    fn binary_executable_delete_and_create_patch_reproduces_exact_result() {
        let base = tree(&[
            ("binary", false, &[0, 255, 0]),
            ("deleted", false, b"gone"),
            ("mode", false, b"unchanged"),
        ]);
        let result = tree(&[
            ("binary", false, &[0, 42, 0, 254]),
            ("created file", false, b"new\n"),
            ("mode", true, b"unchanged"),
        ]);
        let patch = capture_patch(&base, &result, 8192).unwrap();
        assert_eq!(capture_patch(&base, &result, 8192).unwrap(), patch);
        assert!(String::from_utf8_lossy(&patch).contains("GIT binary patch"));
        let repo = private_directory().unwrap();
        materialize(&base, &repo.path().canonicalize().unwrap()).unwrap();
        fixture_git(repo.path(), &["init", "-q"]);
        commit(repo.path());
        let patch_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(patch_file.path(), &patch).unwrap();
        fixture_git(
            repo.path(),
            &["apply", "--binary", patch_file.path().to_str().unwrap()],
        );
        let oid = commit(repo.path());
        let applied = read_git_tree(repo.path(), &oid, &limits()).unwrap();
        assert_eq!(applied.digest(), result.digest());
        assert_eq!(applied, result);
        assert!(capture_patch(&base, &result, 8).is_err());
        assert!(capture_patch(&base, &result, 0).is_err());
        assert!(capture_patch(&base, &result, MAX_OUTPUT_BYTES + 1).is_err());
        assert!(capture_patch(&base, &base, 1).unwrap().is_empty());
    }

    #[test]
    fn bounded_io_and_deadline_fail_closed() {
        let mut out = Vec::new();
        assert!(read_pipe(
            &mut std::io::Cursor::new(b"oversized"),
            &mut out,
            2,
            Instant::now() + GIT_TIMEOUT
        )
        .is_err());
        assert!(out.len() <= 2);
        let dir = private_directory().unwrap();
        assert!(git_output(
            dir.path(),
            dir.path(),
            &["version"],
            4096,
            Instant::now(),
            false
        )
        .is_err());
        let started = Instant::now();
        let error = git_output(
            dir.path(),
            dir.path(),
            &["-c", "alias.stall=!sleep 10", "stall"],
            4096,
            started + Duration::from_millis(50),
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
