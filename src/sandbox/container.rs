//! The docker:// container launch spine — `create → stage → start` — that the
//! run assembly composes. This is the cred-independent, live-verified core of
//! the run-assembly lifecycle (see the state machine in
//! docs/remotes-redesign.md). The vault/auth blob *contents*, the attach pump,
//! result extraction, and teardown live in the backend; this owns only the
//! ordering that must not be gotten wrong:
//!
//! - **stage before start** — the workspace (and the auth/vault blob) must be
//!   in the container before the agent process launches, so create-then-cp-then-
//!   start, never `docker run` (which would start the agent first);
//! - **failure after Created reaps** — a create that succeeds followed by a
//!   stage/start failure must not leave an orphan container.
//!
//! Built ahead of its caller (`RemoteDockerSandbox::run`, the cred-gated next
//! slice); the live test below proves the happy path against a real daemon.
#![allow(dead_code)]

use std::path::Path;

use anyhow::Result;

use super::workspace_stage::{stage_workspace, StageReport};
use crate::docker::{self, DockerEndpoint};

/// A staged-and-started container, plus what the workspace ingest shipped (so
/// the caller can log dropped secrets — the "no silent caps" rule).
pub(crate) struct Launched {
    pub(crate) container: String,
    pub(crate) stage: StageReport,
}

/// `docker create` a container from `create_args` (image + command, no `-d`),
/// stage the workspace (tar-cp) and the optional auth/vault `blob` into it,
/// then `docker start`. Returns the started container id. On any failure after
/// the container is created, the container is reaped before returning the error
/// — no orphan (the run-assembly SM's failure edge).
pub(crate) fn launch_staged_container(
    endpoint: &DockerEndpoint,
    create_args: &[String],
    workspace_root: &Path,
    workspace_dest: &str,
    blob: Option<(&Path, &str)>,
) -> Result<Launched> {
    let container = docker::create_at(endpoint, create_args)?;
    // Everything from here can fail; reap the created container if it does.
    let staged = (|| {
        let stage = stage_workspace(endpoint, &container, workspace_root, workspace_dest)?;
        if let Some((src, dest)) = blob {
            docker::cp_file_at(endpoint, src, &container, dest)?;
        }
        docker::start_at(endpoint, &container)?;
        Ok::<StageReport, anyhow::Error>(stage)
    })();
    match staged {
        Ok(stage) => Ok(Launched { container, stage }),
        Err(e) => {
            let _ = docker::rm_force_at(endpoint, &container);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn test_image() -> String {
        std::env::var("PILLBOX_TEST_RUNNER_IMAGE")
            .unwrap_or_else(|_| crate::docker::DEFAULT_RUNNER_IMAGE.to_string())
    }

    struct RmContainer(String);
    impl Drop for RmContainer {
        fn drop(&mut self) {
            let _ = Command::new("docker").args(["rm", "-f", &self.0]).output();
        }
    }

    /// The launch spine on the wire: `create → stage → start` makes the staged
    /// workspace visible to the started container's processes, secrets still
    /// excluded — the ordering `RemoteDockerSandbox::run` depends on.
    #[test]
    #[ignore = "requires docker + the runner image"]
    fn launch_staged_container_create_stage_start() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("hello.txt"), b"hi").unwrap();
        std::fs::write(root.path().join(".env"), b"SECRET=1").unwrap();

        let ep = DockerEndpoint::local();
        let launched = launch_staged_container(
            &ep,
            &[test_image(), "sleep".into(), "120".into()],
            root.path(),
            "/tmp",
            None,
        )
        .expect("launch");
        let _rm = RmContainer(launched.container.clone());

        assert_eq!(launched.stage.files, 1, "hello.txt (not .env)");
        let present = |path: &str| {
            Command::new("docker")
                .args(["exec", &launched.container, "test", "-e", path])
                .status()
                .expect("test -e")
                .success()
        };
        assert!(present("/tmp/hello.txt"), "staged file visible after start");
        assert!(!present("/tmp/.env"), ".env must not be staged");
    }

    /// Result extraction: `docker cp` the container's workspace back to the
    /// host so the run "feels like local" — what the agent wrote in the
    /// container lands in cwd. Works on a stopped container (the docker://
    /// flow extracts before reap). Deterministic (no agent): we write the
    /// file ourselves, then pull it.
    #[test]
    #[ignore = "requires docker + the runner image"]
    fn cp_out_pulls_container_workspace_to_host() {
        use crate::docker;
        let ep = DockerEndpoint::local();
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
        let _rm = RmContainer(cid.clone());
        // The container writes a file under /workspace (mimicking the agent).
        let wrote = Command::new("docker")
            .args([
                "exec",
                &cid,
                "sh",
                "-c",
                "mkdir -p /workspace && printf agent-wrote-this > /workspace/RESULT.txt",
            ])
            .status()
            .expect("write in container");
        assert!(wrote.success());

        let host = tempfile::tempdir().unwrap();
        docker::cp_out_at(&ep, &cid, "/workspace", host.path()).expect("cp out");
        let pulled = std::fs::read_to_string(host.path().join("RESULT.txt")).expect("RESULT.txt");
        assert_eq!(
            pulled.trim(),
            "agent-wrote-this",
            "the container's workspace edit must land on the host"
        );
    }
}
