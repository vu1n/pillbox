//! Owned TLS MITM for the libkrun backend (L5b) — the credential vault on the
//! smoltcp egress stack.
//!
//! The DNS fence ([`super::egress`]) points allowlisted provider hosts at the
//! gateway and **pins** the names the guest resolved. This module terminates the
//! guest's TLS to those hosts on our own stack, swaps a stub credential for the
//! real one, and forwards to the real upstream — reimplementing on smoltcp +
//! rustls what the hudsucker-based vault does for the docker/ssh backends.
//!
//! **This slice (L5b-1): the cert layer.** The CA is *reused* — the VMM child is
//! a host process, so it loads [`crate::vault::Ca`] from disk directly (the CA
//! key never nears the guest). Leaf minting is *reimplemented* (hudsucker's
//! `RcgenAuthority` owns it internally): mint a per-SNI leaf signed by the CA
//! issuer and wrap it in a rustls [`ServerConfig`], ALPN-pinned to `http/1.1`
//! (so the guest negotiates h1 with us — we only ever parse h1 to do the swap,
//! sidestepping HTTP/2 framing). The TLS terminate + the SNI∩pin gate + the
//! credential swap + the forward leg are the following L5b slices.

use std::sync::Arc;

use anyhow::{Context, Result};
use rcgen::{CertificateParams, DnType, Issuer, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::ServerConfig;
use time::{Duration, OffsetDateTime};

/// Leaf validity. Short — these are minted per run and never persisted; the only
/// verifier is the guest, which trusts the CA for the whole session.
const LEAF_VALIDITY_DAYS: i64 = 7;

/// Mint a leaf cert for `sni` signed by the vault CA `issuer` and build a rustls
/// [`ServerConfig`] that presents it. ALPN is pinned to `http/1.1` so the guest's
/// client negotiates h1 with the MITM (the swap parser only handles h1; we speak
/// h1/h2 to the real upstream independently). Caller loads the issuer once
/// (`Ca::ensure(ca_dir)?.issuer()?`) and caches the result per SNI.
#[allow(dead_code)] // consumed by the L5b TLS terminate (next slice)
pub(super) fn leaf_server_config(issuer: &Issuer<'_, KeyPair>, sni: &str) -> Result<Arc<ServerConfig>> {
    let leaf_key = KeyPair::generate().map_err(|e| anyhow::anyhow!("generate leaf key: {e}"))?;
    let mut params = CertificateParams::new(vec![sni.to_string()])
        .map_err(|e| anyhow::anyhow!("leaf params for {sni}: {e}"))?;
    params.distinguished_name.push(DnType::CommonName, sni);
    let now = OffsetDateTime::now_utc();
    params.not_before = now - Duration::hours(1);
    params.not_after = now + Duration::days(LEAF_VALIDITY_DAYS);

    let leaf = params
        .signed_by(&leaf_key, issuer)
        .map_err(|e| anyhow::anyhow!("sign leaf for {sni}: {e}"))?;
    let leaf_der: CertificateDer<'static> = leaf.der().clone();
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));

    // The guest trusts the CA as a root, so presenting just the leaf is enough
    // (the CA signs the leaf directly — no intermediate).
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("rustls protocol versions")?
        .with_no_client_auth()
        .with_single_cert(vec![leaf_der], key_der)
        .context("rustls server config")?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::Ca;

    #[test]
    fn mints_a_leaf_server_config_for_an_sni() {
        let dir = std::env::temp_dir().join(format!("pillbox-krun-vault-{}", uuid::Uuid::now_v7()));
        let ca = Ca::ensure(&dir).expect("ca");
        let issuer = ca.issuer().expect("issuer");

        // Minting must succeed and the config must be ALPN-pinned to h1 (so the
        // guest never negotiates h2 with us).
        let cfg = leaf_server_config(&issuer, "api.anthropic.com").expect("server config");
        assert_eq!(cfg.alpn_protocols, vec![b"http/1.1".to_vec()]);

        // A second SNI mints an independent leaf (the terminate slice caches these).
        let cfg2 = leaf_server_config(&issuer, "api.openai.com").expect("server config 2");
        assert_eq!(cfg2.alpn_protocols, vec![b"http/1.1".to_vec()]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
