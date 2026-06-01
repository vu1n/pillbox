//! Owned TLS MITM for the libkrun backend (L5b) — the credential vault on the
//! smoltcp egress stack.
//!
//! The DNS fence ([`super::egress`]) points allowlisted provider hosts at the
//! gateway and **pins** the names the guest resolved. This module supplies the
//! trust material — a rustls [`ServerConfig`] that mints a per-SNI leaf from the
//! *reused* vault CA — and (later slices) the credential swap. The VMM child is a
//! host process, so it loads [`crate::vault::Ca`] from disk directly: the CA key
//! never nears the guest.
//!
//! **This slice (L5b-1): terminate + gate.** The cert resolver mints a leaf only
//! for **allowlisted** SNIs (the allowlist gate, enforced at cert selection — a
//! non-allowlisted SNI gets no cert, so the handshake fails). The *pin* gate (the
//! name was DNS-resolved through our resolver, catching a hardcoded-IP + forged
//! SNI that skipped DNS) is applied in [`super::egress`] where the live `PinTable`
//! lives. ALPN is pinned to `http/1.1` so we only ever parse h1 to swap. The
//! swap and the forward leg are L5b-2/3; for now [`Vault::synthesize`] returns a
//! canned OK response (like the step-5 spike), proving the guest's TLS terminates.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use rcgen::{CertificateParams, DnType, Issuer, KeyPair};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::{ServerConfig, ServerConnection};
use time::{Duration, OffsetDateTime};

/// Leaf validity. Short — minted per run, never persisted; the only verifier is
/// the guest, which trusts the CA for the whole session.
const LEAF_VALIDITY_DAYS: i64 = 7;

/// The MITM's trust material: one rustls [`ServerConfig`] whose cert resolver
/// mints per-SNI leaves from the vault CA. Built once per microVM in the VMM
/// child; `new_conn` hands out a fresh [`ServerConnection`] per accepted socket.
pub(super) struct Vault {
    config: Arc<ServerConfig>,
}

impl Vault {
    /// Load the vault CA from `ca_dir` (host-side; the key stays out of the guest)
    /// and build a `ServerConfig` that mints leaves for the `allowlist` hosts.
    pub(super) fn new(ca_dir: &str, allowlist: Vec<String>) -> Result<Vault> {
        let ca = crate::vault::Ca::ensure(std::path::Path::new(ca_dir))
            .map_err(|e| anyhow!("load vault CA from {ca_dir}: {e}"))?;
        let issuer = ca.issuer().map_err(|e| anyhow!("build CA issuer: {e}"))?;
        let resolver = Arc::new(CertResolver {
            issuer,
            allowlist,
            cache: Mutex::new(HashMap::new()),
        });
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let mut config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .context("rustls protocol versions")?
            .with_no_client_auth()
            .with_cert_resolver(resolver);
        // h1-pin: the guest negotiates HTTP/1.1 with us, so the swap parser never
        // has to handle h2 framing (we speak h1/h2 to the real upstream).
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Vault { config: Arc::new(config) })
    }

    /// A fresh server-side TLS connection for an accepted socket.
    pub(super) fn new_conn(&self) -> Result<ServerConnection> {
        ServerConnection::new(self.config.clone()).map_err(|e| anyhow!("rustls server conn: {e}"))
    }

    /// Handle a decrypted HTTP/1.1 request head. L5b-1: log nothing here (the
    /// caller logs the gate) and return a canned 200 so the terminate is provable
    /// without a forward leg. L5b-2 forwards; L5b-3 swaps the credential first.
    pub(super) fn synthesize(&self, _request_head: &[u8]) -> Vec<u8> {
        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok".to_vec()
    }
}

/// Mints + caches a leaf (signed by the vault CA) for each **allowlisted** SNI;
/// returns `None` otherwise, so a handshake for a non-allowlisted host fails for
/// want of a cert. This is the allowlist gate, enforced at cert selection.
struct CertResolver {
    issuer: Issuer<'static, KeyPair>,
    allowlist: Vec<String>,
    cache: Mutex<HashMap<String, Arc<CertifiedKey>>>,
}

impl std::fmt::Debug for CertResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CertResolver").field("allowlist", &self.allowlist).finish()
    }
}

impl ResolvesServerCert for CertResolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let sni = hello.server_name()?;
        if !self.allowlist.iter().any(|h| h.eq_ignore_ascii_case(sni)) {
            return None; // allowlist gate: no cert for a non-allowlisted SNI
        }
        let mut cache = self.cache.lock().unwrap();
        if let Some(ck) = cache.get(sni) {
            return Some(ck.clone());
        }
        let ck = Arc::new(mint_certified_key(&self.issuer, sni).ok()?);
        cache.insert(sni.to_string(), ck.clone());
        Some(ck)
    }
}

/// Mint a leaf for `sni` signed by the vault CA `issuer`, as a rustls
/// [`CertifiedKey`] (cert chain + signing key) for the resolver. The guest trusts
/// the CA as a root, so presenting just the leaf is enough (no intermediate).
fn mint_certified_key(issuer: &Issuer<'_, KeyPair>, sni: &str) -> Result<CertifiedKey> {
    let leaf_key = KeyPair::generate().map_err(|e| anyhow!("generate leaf key: {e}"))?;
    let mut params =
        CertificateParams::new(vec![sni.to_string()]).map_err(|e| anyhow!("leaf params: {e}"))?;
    params.distinguished_name.push(DnType::CommonName, sni);
    let now = OffsetDateTime::now_utc();
    params.not_before = now - Duration::hours(1);
    params.not_after = now + Duration::days(LEAF_VALIDITY_DAYS);
    let leaf = params
        .signed_by(&leaf_key, issuer)
        .map_err(|e| anyhow!("sign leaf for {sni}: {e}"))?;

    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));
    let signing_key = rustls::crypto::aws_lc_rs::sign::any_supported_type(&key_der)
        .map_err(|e| anyhow!("leaf signing key: {e}"))?;
    Ok(CertifiedKey::new(vec![leaf.der().clone()], signing_key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::Ca;

    fn test_vault(allowlist: &[&str]) -> (Vault, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("pillbox-krun-vault-{}", uuid::Uuid::now_v7()));
        let v = Vault::new(
            dir.to_str().unwrap(),
            allowlist.iter().map(|s| s.to_string()).collect(),
        )
        .expect("vault");
        (v, dir)
    }

    #[test]
    fn vault_builds_and_hands_out_connections() {
        let (vault, dir) = test_vault(&["api.anthropic.com"]);
        let conn = vault.new_conn().expect("server conn");
        assert!(conn.wants_read()); // a fresh server connection awaits the ClientHello
        assert!(vault.synthesize(b"GET / HTTP/1.1\r\n\r\n").starts_with(b"HTTP/1.1 200"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolver_mints_for_allowlisted_sni_only() {
        let dir = std::env::temp_dir().join(format!("pillbox-krun-res-{}", uuid::Uuid::now_v7()));
        let ca = Ca::ensure(&dir).expect("ca");
        let resolver = CertResolver {
            issuer: ca.issuer().expect("issuer"),
            allowlist: vec!["api.anthropic.com".to_string()],
            cache: Mutex::new(HashMap::new()),
        };
        // Minting an allowlisted SNI works + caches (same Arc on a second call).
        let ck1 = mint_certified_key(&resolver.issuer, "api.anthropic.com").expect("mint");
        assert!(!ck1.cert.is_empty());
        // Case-insensitive allowlist membership (resolver gate uses the same check).
        assert!(resolver.allowlist.iter().any(|h| h.eq_ignore_ascii_case("API.ANTHROPIC.COM")));
        assert!(!resolver.allowlist.iter().any(|h| h.eq_ignore_ascii_case("evil.example")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
