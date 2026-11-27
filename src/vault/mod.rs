//! Credential vault — keeps real OAuth creds on the host while the
//! sandboxed guest sees only stubs.
//!
//! - [`providers`]: one stateless provider per upstream service — the hosts it
//!   claims, its OAuth token-endpoint matcher and its OAuth codec (stub shapes,
//!   refresh wire format).
//! - The libkrun MITM (`sandbox::libkrun::{mitm, vault}`) releases stubs for
//!   real credentials on the hosts each stub is bound to, using these providers.
//! - `refresh` / `token_store`: the host-side broker, the sole OAuth rotation
//!   authority.
//! - [`Server`]: the hudsucker proxy behind `pillbox sidecar` — egress decision
//!   and local token-endpoint refusal only; it holds no credentials.

mod ca;
mod egress;
mod forward;
pub(crate) mod known_secrets;
pub(crate) mod providers;
mod refresh;
mod server;
mod token_store;

pub(crate) use ca::{cert_path_in as ca_cert_path_in, Ca};
pub(crate) use egress::{is_denied_egress_ip, EgressPolicy};
pub(crate) use known_secrets::{HeaderScheme, VaultMeta};
#[cfg(any(feature = "libkrun", test))]
pub(crate) use refresh::pre_refresh_until;
// The broker entry points the libkrun backend reuses: the
// coordinated host-side pre-refresh and the far-future stub-expiry sentinel. Gated to
// libkrun, the only substrate that releases credentials.
#[cfg(feature = "libkrun")]
pub(crate) use refresh::{
    broker_expiry, broker_jit_refresh, pre_refresh, STUB_FAR_FUTURE_EXPIRES_AT_MS,
};
pub(crate) use server::{Server, ServerConfig};
