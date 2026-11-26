//! `pillbox auth login` on the libkrun microVM — a foreground VM whose guest runs
//! the agent's login under the in-guest `pty-host`, with the agent's real auth home
//! as its read-write creds share (no env-fork: the login writes the credential).
//!
//! Its egress is the one launch that sets [`EgressSpec`](super::EgressSpec)`.login`,
//! which opens the providers' OAuth token endpoints through the MITM; every run,
//! session and sandbox launch leaves it false. A second vsock entry in
//! [`VmSpec`](super::VmSpec)`.vsock` can relay an agent's loopback OAuth callback
//! port. Shared launch helpers live in [`super::session`] (`pub(super)`).
