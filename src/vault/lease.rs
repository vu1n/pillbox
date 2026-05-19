//! Per-sandbox vault lease — RAII handle that owns the stub→real mapping
//! lifetime.
//!
//! A [`SandboxLease`] is created by [`crate::vault::Server::lease`] and
//! held by the caller (Pillbox's orchestrator) for the sandbox's
//! lifetime. When the lease is dropped, the entry is removed from the
//! [`crate::vault::providers::Registry`], so future requests carrying
//! the stub tokens fail.
//!
//! The lease is provider-agnostic: it knows the sandbox id and the stub
//! credentials body to mount, nothing about Anthropic vs codex
//! specifics. The provider produced the body during
//! [`crate::vault::providers::VaultProvider::provision`].

use std::sync::{Arc, Weak};

use super::server::ServerInner;

/// RAII handle for a per-sandbox vault entry.
///
/// Dropping this removes the stub→real mapping from the server's
/// registry. Held by the caller for the duration of the sandbox
/// lifetime. Use [`Self::stub_credentials_body`] to produce the file
/// body that gets written into the guest's credentials file.
#[derive(Debug)]
pub struct SandboxLease {
    sandbox_id: String,
    stub_body: String,
    server: Weak<ServerInner>,
}

impl SandboxLease {
    pub(crate) fn new(sandbox_id: String, stub_body: String, server: Arc<ServerInner>) -> Self {
        Self {
            sandbox_id,
            stub_body,
            server: Arc::downgrade(&server),
        }
    }

    /// The body of the stub credentials file (e.g. claude
    /// `.credentials.json` or codex `auth.json`) the caller should mount
    /// over the guest's real file.
    pub(crate) fn stub_credentials_body(&self) -> &str {
        &self.stub_body
    }
}

impl Drop for SandboxLease {
    fn drop(&mut self) {
        if let Some(server) = self.server.upgrade() {
            server.drop_sandbox(&self.sandbox_id);
        }
    }
}
