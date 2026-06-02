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
//! lives. ALPN is pinned to `http/1.1` so we only ever parse h1 to swap.
//!
//! **Forward leg (L5b-2):** [`Vault::connect_upstream`] opens a *real* host socket
//! to the pinned provider, validates its real cert against the Mozilla roots, and
//! the egress pump relays decrypted bytes between the two TLS sessions
//! transparently. The credential swap (parse h1 head → substitute the auth header)
//! is L5b-3, inserted between the guest decrypt and the upstream send.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration as StdDuration;

use anyhow::{anyhow, Context, Result};
use rcgen::{CertificateParams, DnType, Issuer, KeyPair};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::{ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection};
use time::{Duration, OffsetDateTime};

/// Leaf validity. Short — minted per run, never persisted; the only verifier is
/// the guest, which trusts the CA for the whole session.
const LEAF_VALIDITY_DAYS: i64 = 7;

/// The MITM's trust material: one rustls [`ServerConfig`] whose cert resolver
/// mints per-SNI leaves from the vault CA. Built once per microVM in the VMM
/// child; `new_conn` hands out a fresh [`ServerConnection`] per accepted socket.
pub(super) struct Vault {
    config: Arc<ServerConfig>,
    client_config: Arc<ClientConfig>,
}

impl Vault {
    /// Load the vault CA from `ca_dir` (host-side; the key stays out of the guest)
    /// and build the guest-side `ServerConfig` (mints leaves for `allowlist`) plus
    /// the upstream-side `ClientConfig` (validates real provider certs).
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
        let mut config = ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .context("rustls protocol versions")?
            .with_no_client_auth()
            .with_cert_resolver(resolver);
        // h1-pin: the guest negotiates HTTP/1.1 with us, so the swap parser never
        // has to handle h2 framing (we speak h1/h2 to the real upstream).
        config.alpn_protocols = vec![b"http/1.1".to_vec()];

        // Upstream side: validate the real provider cert against the Mozilla roots
        // (so we aren't ourselves MITM'd), h1 to match the guest.
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let mut client_config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .context("rustls client protocol versions")?
            .with_root_certificates(roots)
            .with_no_client_auth();
        client_config.alpn_protocols = vec![b"http/1.1".to_vec()];

        Ok(Vault {
            config: Arc::new(config),
            client_config: Arc::new(client_config),
        })
    }

    /// A fresh server-side TLS connection for an accepted socket.
    pub(super) fn new_conn(&self) -> Result<ServerConnection> {
        ServerConnection::new(self.config.clone()).map_err(|e| anyhow!("rustls server conn: {e}"))
    }

    /// Connect to the real `host` on a **background thread** and return a receiver
    /// the egress poll loop polls each tick. The resolve + `connect_timeout` are
    /// blocking; running them off the poll-loop thread keeps a slow/hung upstream
    /// from stalling the guest's whole stack (DNS + other connections).
    pub(super) fn spawn_connect(&self, host: String) -> mpsc::Receiver<Result<Upstream, String>> {
        let client_config = self.client_config.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(connect_upstream(client_config, &host).map_err(|e| format!("{e:#}")));
        });
        rx
    }
}

/// Resolve + connect to `host:443` and start a rustls client validating the real
/// cert. Blocking — run off the poll loop via [`Vault::spawn_connect`]. The socket
/// is left non-blocking so the poll loop can then drive its I/O in-thread.
fn connect_upstream(client_config: Arc<ClientConfig>, host: &str) -> Result<Upstream> {
    let addr = (host, 443u16)
        .to_socket_addrs()
        .with_context(|| format!("resolve {host}"))?
        .next()
        .ok_or_else(|| anyhow!("no address for {host}"))?;
    let sock = TcpStream::connect_timeout(&addr, StdDuration::from_secs(10))
        .with_context(|| format!("connect {host}"))?;
    sock.set_nonblocking(true).context("set upstream non-blocking")?;
    let server_name = ServerName::try_from(host.to_string())
        .with_context(|| format!("invalid upstream name {host}"))?;
    let tls = ClientConnection::new(client_config, server_name)
        .map_err(|e| anyhow!("rustls client conn: {e}"))?;
    Ok(Upstream { sock, tls })
}

/// The forward (upstream) half of a MITM session: a real non-blocking host socket
/// with a rustls client validating the provider's real cert. The egress poll loop
/// bridges plaintext between this and the guest-side `ServerConnection`.
pub(super) struct Upstream {
    sock: TcpStream,
    tls: ClientConnection,
}

impl Upstream {
    /// Queue decrypted request bytes (from the guest) to send to the upstream.
    pub(super) fn send(&mut self, buf: &[u8]) {
        let _ = self.tls.writer().write_all(buf);
    }

    /// Drive the socket: flush queued ciphertext out, pull any response ciphertext
    /// in. Returns `false` when the upstream has closed or errored.
    pub(super) fn pump(&mut self) -> bool {
        while self.tls.wants_write() {
            match self.tls.write_tls(&mut self.sock) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => return false,
            }
        }
        match self.tls.read_tls(&mut self.sock) {
            Ok(0) => return false, // upstream closed
            Ok(_) => {
                if self.tls.process_new_packets().is_err() {
                    return false;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => return false,
        }
        true
    }

    /// Drain decrypted response bytes (from the upstream) into `out`.
    pub(super) fn recv_into(&mut self, out: &mut Vec<u8>) {
        let mut buf = [0u8; 4096];
        loop {
            match self.tls.reader().read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
    }
}

/// Streaming stub→real credential substitution for the guest→upstream plaintext.
///
/// The env fork puts a **stub** secret in the guest (its env, or a stubbed creds
/// file) so the real value never enters the VM. The agent sends the stub in its
/// auth header on every request; this replaces the stub bytes with the real value
/// as the decrypted stream flows to the upstream — auth-mode-agnostic (works for
/// `x-api-key`, `Authorization: Bearer`, keep-alive, h1 or otherwise) because it
/// matches the high-entropy stub token wherever it appears, not the HTTP framing.
///
/// Stubs can straddle TLS-record boundaries, so [`push`](Self::push) holds back a
/// tail that could begin a partial match and emits it once resolved (or at
/// [`flush`](Self::flush)).
/// One credential substitution: the agent sends `stub`, the MITM puts `real` on
/// the wire. Named fields (not a `(Vec, Vec)` tuple) so the direction can't be
/// transposed — swapping them would replace the real *backward* into the guest.
#[derive(Clone)]
pub(super) struct CredSwap {
    pub(super) stub: Vec<u8>,
    pub(super) real: Vec<u8>,
}

pub(super) struct StubSwap {
    /// The substitutions. Empty → a transparent pass-through.
    pairs: Vec<CredSwap>,
    carry: Vec<u8>,
    max_stub: usize,
}

impl StubSwap {
    pub(super) fn new(pairs: Vec<CredSwap>) -> Self {
        let max_stub = pairs.iter().map(|p| p.stub.len()).max().unwrap_or(0);
        Self { pairs, carry: Vec::new(), max_stub }
    }

    /// Whether any substitution is configured (else the relay can skip the copy).
    pub(super) fn is_noop(&self) -> bool {
        self.pairs.is_empty()
    }

    /// Feed a decrypted chunk; return the bytes safe to forward now (a possible
    /// partial-stub tail is held in `carry` until the next chunk or `flush`).
    pub(super) fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.carry.extend_from_slice(chunk);
        // Everything before `safe` cannot be the start of a stub that only
        // completes in a later chunk, so it's final; keep the tail as carry.
        let safe = self.carry.len().saturating_sub(self.max_stub.saturating_sub(1));
        let (out, rest) = self.scan(safe);
        self.carry = rest;
        out
    }

    /// Flush the held tail at end-of-stream (no more bytes can complete a stub).
    pub(super) fn flush(&mut self) -> Vec<u8> {
        let (out, _) = self.scan(self.carry.len());
        self.carry.clear();
        out
    }

    /// Replace stubs in `carry[..limit]`, returning `(emitted, leftover_carry)`.
    /// A stub straddling `limit` is matched in full (it's wholly in `carry`).
    fn scan(&self, limit: usize) -> (Vec<u8>, Vec<u8>) {
        let mut out = Vec::with_capacity(limit);
        let mut i = 0;
        while i < limit {
            if let Some(p) = self.pairs.iter().find(|p| self.carry[i..].starts_with(&p.stub)) {
                out.extend_from_slice(&p.real);
                i += p.stub.len();
            } else {
                out.push(self.carry[i]);
                i += 1;
            }
        }
        (out, self.carry[i..].to_vec())
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

    fn swap(pairs: &[(&str, &str)], chunks: &[&[u8]]) -> Vec<u8> {
        let mut s = StubSwap::new(
            pairs
                .iter()
                .map(|(stub, real)| CredSwap { stub: stub.as_bytes().to_vec(), real: real.as_bytes().to_vec() })
                .collect(),
        );
        let mut out = Vec::new();
        for c in chunks {
            out.extend(s.push(c));
        }
        out.extend(s.flush());
        out
    }

    #[test]
    fn stub_swap_replaces_in_a_single_chunk() {
        let out = swap(&[("STUB123", "real-key")], &[b"x-api-key: STUB123\r\n"]);
        assert_eq!(out, b"x-api-key: real-key\r\n");
    }

    #[test]
    fn stub_swap_matches_across_a_chunk_boundary() {
        // The stub is split across two pushes — the carry must bridge it.
        let out = swap(&[("STUB123", "real-key")], &[b"Bearer STU", b"B123 done"]);
        assert_eq!(out, b"Bearer real-key done");
    }

    #[test]
    fn stub_swap_passes_through_when_no_match() {
        let out = swap(&[("STUB123", "real-key")], &[b"no secret here", b", really"]);
        assert_eq!(out, b"no secret here, really");
    }

    #[test]
    fn stub_swap_handles_multiple_pairs_and_repeats() {
        let out = swap(
            &[("AAA", "1"), ("BBB", "22")],
            &[b"AAA BBB AAA"],
        );
        assert_eq!(out, b"1 22 1");
    }

    #[test]
    fn stub_swap_noop_is_transparent() {
        let mut s = StubSwap::new(vec![]);
        assert!(s.is_noop());
        assert_eq!(s.push(b"anything at all"), b"anything at all");
        assert!(s.flush().is_empty());
    }

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
