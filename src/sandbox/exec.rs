//! `SandboxExec` — run a command inside a running sandbox and read its stdout.
//!
//! The opencode [`Integration::Server`](crate::agents::Integration) bridge
//! (`sandbox::opencode`) drives a headless `opencode serve` *inside* the
//! sandbox: poll readiness, create a session, push prompts, stream `/event`.
//! Every one of those is the same shape — "run a command in the sandbox, read
//! its stdout." On docker that's `docker exec curl …`; on libkrun (step 7c) a
//! vsock exec channel. This trait is the seam, so the bridge is written once
//! against it and each backend supplies the transport.
//!
//! Considered and rejected: a narrower "HTTP-to-the-in-guest-server" transport.
//! It's tighter, but `docker exec` is the established docker transport for every
//! other channel (pty-relay, transcript-stream), so "run a command" keeps the
//! one seam both backends already share — and host↔sandbox command exec is the
//! same trust as `docker exec` (the owner controls the sandbox either way).

use std::io::Read;

use anyhow::{Context, Result};

use crate::docker::{self, DockerEndpoint};

/// Spawn commands inside one running sandbox.
pub(crate) trait SandboxExec {
    /// Run `argv` in the sandbox with stdin closed and stdout piped back.
    /// stderr handling is the impl's call (docker inherits it for diagnostics).
    fn exec(&self, argv: &[&str]) -> Result<SandboxChild>;
}

/// A command running inside a sandbox: a readable stdout plus an explicit
/// `stopper` that tears down the transport (kills the `docker exec` / closes
/// the vsock). The two fields are separable on purpose — a streaming consumer
/// hands `stdout` to a reader thread and keeps `stopper` elsewhere (a
/// [`TailerHandle`](crate::events::transcripts::TailerHandle)) so the stream
/// stays open until stop is *explicitly* called, not when this struct drops.
/// No `Drop` impl: the lifecycle is explicit so stdout can outlive the handle.
pub(crate) struct SandboxChild {
    pub(crate) stdout: Box<dyn Read + Send>,
    pub(crate) stopper: Box<dyn FnOnce() + Send>,
}

impl SandboxChild {
    /// Read stdout to EOF as UTF-8, then stop the command. The one-shot helper
    /// behind `exec_curl` — request/response with no lingering transport.
    pub(crate) fn read_to_string(mut self) -> String {
        let mut out = String::new();
        self.stdout.read_to_string(&mut out).ok();
        (self.stopper)();
        out
    }
}

/// `SandboxExec` over a docker daemon: `docker exec -i <container> <argv>` on
/// `endpoint` (local or a `docker://` remote, where `DOCKER_HOST=ssh://…`
/// carries the exec stream back over SSH unchanged).
pub(crate) struct DockerExec {
    endpoint: DockerEndpoint,
    container: String,
}

impl DockerExec {
    pub(crate) fn new(endpoint: DockerEndpoint, container: String) -> Self {
        Self { endpoint, container }
    }
}

impl SandboxExec for DockerExec {
    fn exec(&self, argv: &[&str]) -> Result<SandboxChild> {
        let owned: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let mut child = docker::exec_attach_at(&self.endpoint, &self.container, &owned)?;
        drop(child.stdin.take()); // no stdin to an in-sandbox curl
        let stdout = child.stdout.take().context("docker exec stdout")?;
        Ok(SandboxChild {
            stdout: Box::new(stdout),
            // The blocking read can't observe a flag mid-frame, so stop = kill
            // the exec to EOF the pipe, then reap it.
            stopper: Box::new(move || {
                let _ = child.kill();
                let _ = child.wait();
            }),
        })
    }
}
