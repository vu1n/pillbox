//! Stage a workspace into a container with **no bind-mount** — the
//! docker:// / remote path, where the daemon can't see the host's cwd.
//!
//! Mechanism (per docs/remotes-redesign.md, the tar-cp *ingest* path):
//! [`crate::workspace::ingest::plan_ingest`] decides what crosses the wire
//! (secret-denylist, keep `.git`, size guard), then we stream a `tar` of
//! exactly those files into `docker cp -`. The transfer streams (tar's
//! stdout pipes straight into docker's stdin) rather than buffering the
//! tree, and the file list rides a NUL-delimited `tar -T` manifest so a
//! huge or oddly-named tree never hits an argv limit.
//!
//! This is the one-time ingest; the per-run fast path (overlay-CoW over a
//! rustic-local-on-remote base) lands in a later slice. The runtime caller
//! is the docker:// container lifecycle (also a later slice), so this is
//! built ahead of it — the live integration test below proves the
//! mechanism against a real daemon now.
#![allow(dead_code)]

use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use crate::docker::{self, DockerEndpoint};
use crate::errors::PillboxError;
use crate::workspace::ingest::{plan_ingest, DEFAULT_INGEST_THRESHOLD_BYTES};

/// What [`stage_workspace`] shipped — surfaced so the caller can log it
/// (the "no silent caps" rule: report dropped secrets, never hide them).
#[derive(Debug, Clone)]
pub(crate) struct StageReport {
    /// Number of files (and symlinks) transferred.
    pub(crate) files: usize,
    /// Total bytes transferred.
    pub(crate) bytes: u64,
    /// Secret paths excluded from the transfer, relative to `root`.
    pub(crate) excluded_secrets: Vec<PathBuf>,
}

/// Tar the non-secret contents of `root` and `docker cp` them into
/// `container:dest` on `endpoint`'s daemon. `dest` must already exist in the
/// container. Fails loud (rather than silently shipping GBs) when the tree
/// exceeds the ingest threshold — that's the caller's cue to fall back to
/// S3/rustic.
pub(crate) fn stage_workspace(
    endpoint: &DockerEndpoint,
    container: &str,
    root: &Path,
    dest: &str,
) -> Result<StageReport> {
    let plan = plan_ingest(root)?;
    if plan.exceeds(DEFAULT_INGEST_THRESHOLD_BYTES) {
        return Err(PillboxError::usage(
            "workspace stage",
            format!(
                "workspace is {} MiB, over the {} MiB tar-cp ingest limit",
                plan.total_bytes / (1024 * 1024),
                DEFAULT_INGEST_THRESHOLD_BYTES / (1024 * 1024),
            ),
        )
        .with_next("use an S3/rustic workspace backend for large/persistent trees")
        .into());
    }

    // NUL-delimited manifest for `tar -T`: bytes, not strings, so non-UTF-8
    // names survive, and a file (not argv) so a huge tree can't blow the
    // command-line limit. Absolute path so `tar -C root` doesn't relocate it.
    let mut manifest = Vec::new();
    for rel in &plan.files {
        manifest.extend_from_slice(rel.as_os_str().as_bytes());
        manifest.push(0);
    }
    let list = tempfile::NamedTempFile::new().context("creating tar manifest tempfile")?;
    std::fs::write(list.path(), &manifest).context("writing tar manifest")?;

    // `tar -C root --null -T manifest --no-xattrs -c -f -`: archive exactly the
    // planned files (relative to root) to stdout, which we pipe into
    // `docker cp -`. `--no-xattrs` (accepted by both GNU tar and macOS bsdtar)
    // keeps the archive free of extended attributes — without it, a macOS host
    // embeds Apple xattrs (`com.apple.provenance`) that make `docker cp`
    // extraction on a Linux container fail with `lsetxattr ... not supported`.
    // `COPYFILE_DISABLE=1` additionally suppresses macOS's `._*` AppleDouble
    // sidecar files (GNU tar ignores the env). xattrs aren't part of a
    // workspace transfer's contract, so dropping them is correct.
    let mut tar = Command::new("tar")
        .env("COPYFILE_DISABLE", "1")
        .arg("-C")
        .arg(root)
        .arg("--null")
        .arg("-T")
        .arg(list.path())
        .arg("--no-xattrs")
        .arg("-c")
        .arg("-f")
        .arg("-")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("invoking `tar` to stage workspace")?;
    let tar_stdout = tar.stdout.take().expect("piped tar stdout");
    // Drain tar's stderr on a thread. Otherwise a chatty tar (a per-file
    // warning like "file changed as we read it" on an active tree) fills the
    // stderr pipe buffer, tar blocks writing it, stops producing stdout, and
    // docker — still reading stdin — deadlocks. Draining concurrently is the
    // only safe way to consume two pipes from one child.
    let mut tar_stderr = tar.stderr.take().expect("piped tar stderr");
    let stderr_drain = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = tar_stderr.read_to_end(&mut buf);
        buf
    });

    let cp = docker::cp_stdin_at(endpoint, container, dest, Stdio::from(tar_stdout));
    let tar_status = tar.wait().context("waiting on `tar`")?;
    let tar_stderr = stderr_drain.join().unwrap_or_default();

    // A docker cp failure is the root cause (e.g. `dest` missing) — surface it,
    // not the EPIPE tar gets when docker closes the stream early. Only when
    // docker succeeded do we treat a non-zero tar as the failure.
    cp?;
    if !tar_status.success() {
        return Err(PillboxError::runtime(
            "workspace stage",
            format!(
                "tar failed: {}",
                String::from_utf8_lossy(&tar_stderr).trim()
            ),
        )
        .into());
    }

    Ok(StageReport {
        files: plan.files.len(),
        bytes: plan.total_bytes,
        excluded_secrets: plan.excluded_secrets,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Image with `tar`/`ls` for the live test. The runner image is the
    /// default (it's what a docker:// run uses); override for CI.
    fn test_image() -> String {
        std::env::var("PILLBOX_TEST_RUNNER_IMAGE")
            .unwrap_or_else(|_| "ghcr.io/vu1n/pillbox-runner:latest".into())
    }

    struct RmContainer(String);
    impl Drop for RmContainer {
        fn drop(&mut self) {
            let _ = Command::new("docker").args(["rm", "-f", &self.0]).output();
        }
    }

    /// End-to-end against a real daemon: stage a workspace holding source +
    /// `.git` + secrets, and assert the kept files land while every secret is
    /// excluded. This is the I6-sovereignty guarantee proven on the wire, not
    /// just in the planner unit tests.
    #[test]
    #[ignore = "requires docker + the runner image"]
    fn stages_kept_files_and_excludes_secrets() {
        let root = tempfile::tempdir().unwrap();
        let p = root.path();
        std::fs::write(p.join("main.rs"), b"fn main() {}").unwrap();
        std::fs::create_dir_all(p.join(".git")).unwrap();
        std::fs::write(p.join(".git/config"), b"[core]").unwrap();
        std::fs::write(p.join(".env"), b"SECRET=1").unwrap();
        std::fs::write(p.join(".env.example"), b"SECRET=").unwrap();
        std::fs::write(p.join("tls.key"), b"-----BEGIN").unwrap();

        let cid = String::from_utf8(
            Command::new("docker")
                .args(["run", "-d", &test_image(), "sleep", "120"])
                .output()
                .expect("docker run")
                .stdout,
        )
        .expect("utf8 cid")
        .trim()
        .to_string();
        assert!(!cid.is_empty(), "no container id");
        let _rm = RmContainer(cid.clone());

        let mkdir = Command::new("docker")
            .args(["exec", &cid, "mkdir", "-p", "/ws"])
            .output()
            .expect("mkdir");
        assert!(mkdir.status.success(), "mkdir /ws failed");

        let report = stage_workspace(&DockerEndpoint::local(), &cid, p, "/ws").expect("stage");
        assert_eq!(report.files, 3, "main.rs + .git/config + .env.example");

        let present = |path: &str| {
            Command::new("docker")
                .args(["exec", &cid, "test", "-e", path])
                .status()
                .expect("test -e")
                .success()
        };
        assert!(present("/ws/main.rs"), "source must land");
        assert!(present("/ws/.git/config"), ".git must be kept");
        assert!(present("/ws/.env.example"), "templates are safe to ship");
        assert!(!present("/ws/.env"), ".env must be excluded");
        assert!(!present("/ws/tls.key"), "key material must be excluded");
    }
    // The create→stage→start ordering test moved to `sandbox::container`,
    // where it exercises the reusable `launch_staged_container` spine.
}
