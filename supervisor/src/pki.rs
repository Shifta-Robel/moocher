//! The cluster's private CA (ARCHITECTURE D5).
//!
//! Files under `<data dir>/pki/` (0600 keys):
//! * `ca.key` / `ca.crt` — created once, 10 years.
//! * `link.key` / `link.crt` — the link listener's server certificate, reissued
//!   when `public_names` change or it is within 30 days of expiry.
//!
//! Node certificates are issued from CSRs: the node's key never leaves it, and
//! the supervisor decides the identity (CN = node name) and lifetime itself,
//! ignoring whatever the CSR asked for.

use anyhow::{Context, Result};
use ocl_protocol::model::Ts;
use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
    SerialNumber, PKCS_ED25519,
};
use std::path::{Path, PathBuf};
use time::{Duration, OffsetDateTime};

pub struct Ca {
    dir: PathBuf,
    cert: rcgen::Certificate,
    key: KeyPair,
    pub cert_pem: String,
}

pub struct Issued {
    pub cert_pem: String,
    pub fingerprint: String,
    pub serial: String,
    pub expires_at: Ts,
}

fn write_private(path: &Path, data: &str) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path).with_context(|| format!("writing {}", path.display()))?;
    f.write_all(data.as_bytes())?;
    Ok(())
}

fn serial() -> SerialNumber {
    use rand::Rng;
    let mut b: [u8; 16] = rand::thread_rng().gen();
    b[0] &= 0x7f; // positive
    SerialNumber::from_slice(&b)
}

fn ts(t: OffsetDateTime) -> Ts {
    (t.unix_timestamp_nanos() / 1_000_000) as Ts
}

impl Ca {
    /// Load the CA from `dir`, creating it on first use.
    pub fn open(dir: &Path) -> Result<Ca> {
        std::fs::create_dir_all(dir)?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let (kp, cp) = (dir.join("ca.key"), dir.join("ca.crt"));
        if !kp.exists() {
            let key = KeyPair::generate_for(&PKCS_ED25519)?;
            let mut p = CertificateParams::default();
            p.distinguished_name = DistinguishedName::new();
            p.distinguished_name.push(DnType::CommonName, "opencode-cluster CA");
            p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
            p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
            let now = OffsetDateTime::now_utc();
            p.not_before = now - Duration::hours(1);
            p.not_after = now + Duration::days(3650);
            p.serial_number = Some(serial());
            let cert = p.self_signed(&key)?;
            write_private(&kp, &key.serialize_pem())?;
            std::fs::write(&cp, cert.pem())?;
            tracing::info!(dir = %dir.display(), "created cluster CA");
        }
        let key = KeyPair::from_pem(&std::fs::read_to_string(&kp)?).context("ca.key")?;
        let cert_pem = std::fs::read_to_string(&cp)?;
        // rcgen signs with the issuer's name, key-id method and key; re-deriving
        // the parameters from the stored certificate gives exactly those.
        let cert = CertificateParams::from_ca_cert_pem(&cert_pem).context("ca.crt")?.self_signed(&key)?;
        Ok(Ca { dir: dir.to_path_buf(), cert, key, cert_pem })
    }

    pub fn fingerprint(&self) -> String {
        let der = ocl_protocol::tls::certs_from_pem(&self.cert_pem).expect("own CA parses");
        ocl_protocol::tls::fingerprint(&der[0])
    }

    /// Server certificate for the link listener (chain PEM = leaf + CA, key PEM).
    pub fn link_cert(&self, names: &[String]) -> Result<(String, String)> {
        let (kp, cp, np) = (self.dir.join("link.key"), self.dir.join("link.crt"), self.dir.join("link.names"));
        let wanted = names.join(",");
        let fresh = || -> Option<()> {
            if std::fs::read_to_string(&np).ok()? != wanted {
                return None;
            }
            let pem = std::fs::read_to_string(&cp).ok()?;
            let p = CertificateParams::from_ca_cert_pem(&pem).ok()?;
            (p.not_after > OffsetDateTime::now_utc() + Duration::days(30)).then_some(())
        };
        if fresh().is_none() {
            let key = KeyPair::generate_for(&PKCS_ED25519)?;
            let mut p = CertificateParams::new(names.to_vec())?;
            p.distinguished_name = DistinguishedName::new();
            p.distinguished_name.push(DnType::CommonName, "opencode-cluster supervisor");
            p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
            p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
            p.use_authority_key_identifier_extension = true;
            let now = OffsetDateTime::now_utc();
            p.not_before = now - Duration::hours(1);
            p.not_after = now + Duration::days(397);
            p.serial_number = Some(serial());
            let cert = p.signed_by(&key, &self.cert, &self.key)?;
            write_private(&kp, &key.serialize_pem())?;
            std::fs::write(&cp, cert.pem())?;
            std::fs::write(&np, &wanted)?;
            tracing::info!(names = %wanted, "issued link server certificate");
        }
        let leaf = std::fs::read_to_string(&cp)?;
        Ok((format!("{leaf}{}", self.cert_pem), std::fs::read_to_string(&kp)?))
    }

    /// Sign a node CSR. Only the CSR's public key is used.
    pub fn issue_node(&self, node: &str, csr_pem: &str, days: u32) -> Result<Issued> {
        let csr = CertificateSigningRequestParams::from_pem(csr_pem).context("invalid CSR")?;
        let mut p = CertificateParams::new(vec![format!("{node}.node.opencode-cluster")])?;
        p.distinguished_name = DistinguishedName::new();
        p.distinguished_name.push(DnType::CommonName, node);
        p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        p.use_authority_key_identifier_extension = true;
        let now = OffsetDateTime::now_utc();
        p.not_before = now - Duration::minutes(5);
        p.not_after = now + Duration::days(days as i64);
        let sn = serial();
        let serial_hex = hex::encode(sn.as_ref());
        p.serial_number = Some(sn);
        let expires_at = ts(p.not_after);
        let signed = CertificateSigningRequestParams { params: p, public_key: csr.public_key }.signed_by(&self.cert, &self.key)?;
        Ok(Issued { fingerprint: ocl_protocol::tls::fingerprint(signed.der()), cert_pem: signed.pem(), serial: serial_hex, expires_at })
    }
}

/// Default certificate names when `public_names` is empty.
pub fn default_names() -> Vec<String> {
    let mut v = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    if let Ok(h) = std::fs::read_to_string("/etc/hostname") {
        let h = h.trim();
        if !h.is_empty() && !v.iter().any(|x| x == h) {
            v.insert(0, h.to_string());
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocl_protocol::model::now_ms;

    #[test]
    fn ca_issues_verifiable_node_certs() {
        let dir = tempfile::tempdir().unwrap();
        let ca = Ca::open(dir.path()).unwrap();
        // Reopening yields the same CA.
        let again = Ca::open(dir.path()).unwrap();
        assert_eq!(ca.fingerprint(), again.fingerprint());

        let node_key = KeyPair::generate_for(&PKCS_ED25519).unwrap();
        let mut req = CertificateParams::new(vec!["evil.example".to_string()]).unwrap();
        req.distinguished_name.push(DnType::CommonName, "pretend-admin");
        let csr = req.serialize_request(&node_key).unwrap().pem().unwrap();
        let issued = again.issue_node("worker-b", &csr, 30).unwrap();
        assert!(issued.expires_at > now_ms() + 29 * 86_400_000);

        // The issued cert chains to the CA and identifies the node by our name, not the CSR's.
        let leaf = ocl_protocol::tls::certs_from_pem(&issued.cert_pem).unwrap().remove(0);
        let ca_der = ocl_protocol::tls::certs_from_pem(&ca.cert_pem).unwrap().remove(0);
        let mut roots = rustls::RootCertStore::empty();
        roots.add(ca_der).unwrap();
        let v = rustls::server::WebPkiClientVerifier::builder_with_provider(std::sync::Arc::new(roots), ocl_protocol::tls::provider()).build().unwrap();
        v.verify_client_cert(&leaf, &[], rustls_pki_types::UnixTime::now()).expect("chains to CA");
        let parsed = CertificateParams::from_ca_cert_pem(&issued.cert_pem).unwrap();
        let cn = parsed.distinguished_name.get(&DnType::CommonName).cloned();
        assert!(matches!(cn, Some(rcgen::DnValue::Utf8String(ref s)) if s == "worker-b") || format!("{cn:?}").contains("worker-b"));
        assert_eq!(issued.fingerprint, ocl_protocol::tls::fingerprint(&leaf));

        let (chain, _key) = again.link_cert(&["localhost".into(), "127.0.0.1".into()]).unwrap();
        assert_eq!(ocl_protocol::tls::certs_from_pem(&chain).unwrap().len(), 2);
    }
}
