//! Credential vault — keeps real OAuth creds on the host while the
//! sandboxed guest sees only stubs.
//!
//! ## Flow
//!
//! 1. Pillbox spawns a [`Server`] per `pillbox <agent> run --vault`. The
//!    server registers all known [`providers::VaultProvider`]s.
//! 2. The orchestrator picks the provider matching the agent (claude →
//!    `anthropic`, codex → `codex`) and calls [`Server::lease`] with the
//!    real credentials JSON loaded from
//!    `~/.pillbox/data/<agent>/<provider-creds-path>`.
//! 3. The provider mints stub tokens and returns a stub credentials
//!    file body. Pillbox writes it to a host temp file and bind-mounts
//!    it over the guest's real credentials file (at the path the
//!    provider's `creds_path()` reports).
//! 4. The CA cert is mounted into the guest, and `NODE_EXTRA_CA_CERTS`
//!    plus `HTTPS_PROXY=http://host.docker.internal:<port>` are wired into
//!    the guest env.
//! 5. Guest hits an intercepted host; the proxy dispatches to the
//!    provider that owns the host, which swaps stubs for real tokens
//!    outbound and rotated real tokens for stubs inbound.
//! 6. When the agent exits, pillbox drops the lease (mapping removed)
//!    and the server (graceful proxy shutdown).

mod ca;
pub(crate) mod known_secrets;
mod lease;
pub(crate) mod providers;
mod server;
mod session;

pub(crate) use ca::{cert_path_in as ca_cert_path_in, Ca};
pub(crate) use known_secrets::{HeaderScheme, VaultMeta};
pub(crate) use lease::SandboxLease;
pub(crate) use server::{Server, ServerConfig};
pub(crate) use session::{OAuthAgent, VaultSession};
