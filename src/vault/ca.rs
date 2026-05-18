//! CA generation and persistence for the credential vault.
//!
//! On first use, generates a self-signed root CA used by the MITM proxy to
//! mint per-host leaf certs. The CA is persisted to a caller-provided
//! directory so subsequent sandboxes reuse the same trust root (avoids
//! reinstalling the cert on every boot).

use std::{
    fs,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use rcgen::{
    BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose,
};
use time::{Duration, OffsetDateTime};

const CA_CERT_FILE: &str = "pillbox-vault-ca.crt";
const CA_KEY_FILE: &str = "pillbox-vault-ca.key";
const CA_VALIDITY_DAYS: i64 = 365 * 5;
const CA_COMMON_NAME: &str = "Pillbox Vault Local CA";

/// Path the CA cert would be written to inside `dir`. Doesn't touch the
/// filesystem — use [`Ca::ensure`] to generate, or check `.exists()` on
/// the returned path to probe state without creating anything.
pub(crate) fn cert_path_in(dir: &Path) -> PathBuf {
    dir.join(CA_CERT_FILE)
}

/// Materialized CA on disk. Holds PEM bytes for cert + key.
#[derive(Debug)]
pub struct Ca {
    cert_pem: String,
    key_pem: String,
    cert_path: PathBuf,
}

impl Ca {
    /// Load the CA from `dir`, or generate a new one if absent.
    ///
    /// `dir` must exist or be creatable. The CA cert is written at
    /// `dir/pillbox-vault-ca.crt` and the private key at `dir/pillbox-vault-ca.key`
    /// (mode 0600).
    pub fn ensure(dir: &Path) -> Result<Self, String> {
        fs::create_dir_all(dir).map_err(|error| format!("create ca dir: {error}"))?;
        let cert_path = dir.join(CA_CERT_FILE);
        let key_path = dir.join(CA_KEY_FILE);

        if cert_path.exists() && key_path.exists() {
            let cert_pem = fs::read_to_string(&cert_path)
                .map_err(|error| format!("read ca cert: {error}"))?;
            let key_pem = fs::read_to_string(&key_path)
                .map_err(|error| format!("read ca key: {error}"))?;
            return Ok(Self {
                cert_pem,
                key_pem,
                cert_path,
            });
        }

        let key_pair = KeyPair::generate().map_err(|error| format!("generate ca key: {error}"))?;
        let mut params = CertificateParams::new(Vec::<String>::new())
            .map_err(|error| format!("ca params: {error}"))?;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(DnType::CommonName, CA_COMMON_NAME);
        params
            .distinguished_name
            .push(DnType::OrganizationName, "Pillbox");
        params.key_usages.push(KeyUsagePurpose::DigitalSignature);
        params.key_usages.push(KeyUsagePurpose::KeyCertSign);
        params.key_usages.push(KeyUsagePurpose::CrlSign);
        let now = OffsetDateTime::now_utc();
        params.not_before = now - Duration::hours(1);
        params.not_after = now + Duration::days(CA_VALIDITY_DAYS);

        let cert = params
            .self_signed(&key_pair)
            .map_err(|error| format!("self-sign ca: {error}"))?;
        let cert_pem = cert.pem();
        let key_pem = key_pair.serialize_pem();

        write_private_file(&key_path, &key_pem)?;
        fs::write(&cert_path, &cert_pem).map_err(|error| format!("write ca cert: {error}"))?;

        Ok(Self {
            cert_pem,
            key_pem,
            cert_path,
        })
    }

    /// Path the CA cert was written to.
    pub fn cert_path(&self) -> &Path {
        &self.cert_path
    }

    /// Build an rcgen `Issuer` so the proxy can mint leaf certs.
    pub fn issuer(&self) -> Result<Issuer<'static, KeyPair>, String> {
        let key_pair = KeyPair::from_pem(&self.key_pem)
            .map_err(|error| format!("parse ca key: {error}"))?;
        Issuer::from_ca_cert_pem(&self.cert_pem, key_pair)
            .map_err(|error| format!("parse ca cert: {error}"))
    }
}

fn write_private_file(path: &Path, content: &str) -> Result<(), String> {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| format!("open private file {}: {error}", path.display()))?;
    file.write_all(content.as_bytes())
        .map_err(|error| format!("write private file: {error}"))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("chmod private file: {error}"))
}

#[cfg(test)]
mod tests {
    use std::env::temp_dir;
    use uuid::Uuid;

    use super::Ca;

    #[test]
    fn ensure_generates_then_loads_existing() {
        let dir = temp_dir().join(format!("pillbox-vault-ca-{}", Uuid::now_v7()));
        let _ca1 = Ca::ensure(&dir).expect("generate ca");
        let _ca2 = Ca::ensure(&dir).expect("load ca");
        let cert = std::fs::read_to_string(dir.join("pillbox-vault-ca.crt")).unwrap();
        let key = std::fs::read_to_string(dir.join("pillbox-vault-ca.key")).unwrap();
        assert!(cert.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(key.contains("PRIVATE KEY"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn issuer_can_be_built() {
        let dir = temp_dir().join(format!("pillbox-vault-ca-issuer-{}", Uuid::now_v7()));
        let ca = Ca::ensure(&dir).expect("generate ca");
        let _issuer = ca.issuer().expect("build issuer");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
