//! `pillbox sandbox spawn|exec|agent|destroy` on the libkrun microVM.
//!
//! A sandbox is a detached VM whose guest runs `pillbox exec-host` on a listening
//! vsock port; each `exec` dials the libkrun-bound host socket and speaks
//! [`exec_channel`](crate::sandbox::exec_channel). Its record stores the
//! [`LibkrunHandle`](super::session::LibkrunHandle) JSON in `backend_ref`, and
//! destroy is [`teardown_handle`](super::session::teardown_handle) plus deleting
//! the record. Commit-guard the reparented VMM against
//! [`Sandbox::record_path`] with `session::arm_commit_guard_at`.
//!
//! Wired in: `LibkrunBackend::spawn_sandbox` calls [`spawn`], and
//! `sandbox::sandbox_target` calls [`target`] for a libkrun record.

use anyhow::Result;

use crate::pillbox::Pillbox;
use crate::sandbox::{SandboxBackend as _, SandboxSpawn, SandboxTarget};
use crate::sandboxes::Sandbox;

use super::LibkrunBackend;

/// Boot a sandbox VM for `req` and write its record.
pub(super) fn spawn(_resolved: &Pillbox, _req: &SandboxSpawn) -> Result<Sandbox> {
    Err(LibkrunBackend.capabilities().unsupported("sandbox spawn"))
}

/// The live target for a libkrun sandbox record.
pub(crate) fn target(_record: &Sandbox) -> Result<Box<dyn SandboxTarget>> {
    Err(LibkrunBackend.capabilities().unsupported("sandbox exec"))
}
