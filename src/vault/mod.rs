//! Credential vault — keeps real Anthropic creds on the host while the
//! sandboxed guest sees only stubs.
//!
//! ## Flow
//!
//! 1. Pillbox spawns a [`Server`] per `pillbox claude run --vault`.
//! 2. The server leases a stub mapping for the sandbox via
//!    [`Server::lease`], given the real [`AnthropicCreds`] loaded from
//!    `~/.pillbox/data/claude/.claude/.credentials.json`.
//! 3. Pillbox writes [`SandboxLease::stub_credentials_json`] to a temp
//!    file and bind-mounts it over the guest's `.credentials.json`. It
//!    also mounts the CA cert and sets `NODE_EXTRA_CA_CERTS` plus
//!    `HTTPS_PROXY=http://host.docker.internal:<port>` in the guest env.
//! 4. Guest hits `api.anthropic.com` / `console.anthropic.com`; the proxy
//!    swaps stubs for real tokens outbound and rotated real tokens for
//!    stubs inbound.
//! 5. When the agent exits, pillbox drops the lease (mapping is removed)
//!    and the server (graceful proxy shutdown).

mod ca;
mod lease;
mod secrets;
mod server;
mod session;

pub(crate) use ca::{cert_path_in as ca_cert_path_in, Ca};
pub(crate) use lease::SandboxLease;
pub(crate) use secrets::AnthropicCreds;
pub(crate) use server::{Server, ServerConfig};
pub(crate) use session::VaultSession;
