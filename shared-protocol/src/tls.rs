//! TLS plumbing for the node link (rustls with the ring provider).
//!
//! * The supervisor runs a private CA. Its link listener presents a server
//!   certificate from that CA and *optionally* asks for a client certificate:
//!   `Enroll` works without one (it is authenticated by a join token), every
//!   other RPC requires one.
//! * Nodes are identified by the SHA-256 fingerprint of their certificate,
//!   which the supervisor recorded when it issued it.
//! * Before a node has the CA certificate, it pins the CA by the fingerprint
//!   embedded in its join token ([`pinned_ca_client`]).

use anyhow::{anyhow, bail, Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, ServerConfig, SignatureScheme};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use sha2::{Digest, Sha256};
use std::sync::Arc;

pub fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Lowercase hex SHA-256 of a DER certificate.
pub fn fingerprint(der: &[u8]) -> String {
    hex::encode(Sha256::digest(der))
}

pub fn certs_from_pem(pem: &str) -> Result<Vec<CertificateDer<'static>>> {
    let certs: Vec<_> = CertificateDer::pem_slice_iter(pem.as_bytes()).collect::<Result<_, _>>().map_err(|e| anyhow!("bad certificate PEM: {e:?}"))?;
    if certs.is_empty() {
        bail!("no certificate in PEM");
    }
    Ok(certs)
}

pub fn key_from_pem(pem: &str) -> Result<PrivateKeyDer<'static>> {
    PrivateKeyDer::from_pem_slice(pem.as_bytes()).map_err(|e| anyhow!("bad private key PEM: {e:?}"))
}

fn roots(ca: &[CertificateDer<'static>]) -> Result<Arc<RootCertStore>> {
    let mut r = RootCertStore::empty();
    for c in ca {
        r.add(c.clone()).context("adding CA certificate")?;
    }
    Ok(Arc::new(r))
}

/// Supervisor link listener: server cert from the cluster CA, client certs
/// from the same CA accepted but not required (see module docs).
pub fn link_server(chain: Vec<CertificateDer<'static>>, key: PrivateKeyDer<'static>, ca: &[CertificateDer<'static>]) -> Result<ServerConfig> {
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(roots(ca)?, provider()).allow_unauthenticated().build()?;
    let mut cfg = ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(verifier)
        .with_single_cert(chain, key)?;
    cfg.alpn_protocols = vec![b"h2".to_vec()];
    Ok(cfg)
}

/// Node side: trust only the cluster CA and present the node certificate.
pub fn link_client(ca: &[CertificateDer<'static>], chain: Vec<CertificateDer<'static>>, key: PrivateKeyDer<'static>) -> Result<ClientConfig> {
    let mut cfg = ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_root_certificates(roots(ca)?)
        .with_client_auth_cert(chain, key)?;
    cfg.alpn_protocols = vec![b"h2".to_vec()];
    Ok(cfg)
}

/// Enrollment: the node has no CA certificate yet, only its fingerprint from
/// the join token. Accept the server iff its chain contains a certificate
/// with that fingerprint *and* the server certificate verifies against it
/// (normal WebPKI checks, including the host name).
pub fn pinned_ca_client(ca_fingerprint: &str) -> Result<ClientConfig> {
    let v = PinnedCa { fp: ca_fingerprint.to_lowercase(), provider: provider() };
    let mut cfg = ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(v))
        .with_no_client_auth();
    cfg.alpn_protocols = vec![b"h2".to_vec()];
    Ok(cfg)
}

#[derive(Debug)]
struct PinnedCa {
    fp: String,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedCa {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let ca = intermediates
            .iter()
            .find(|c| fingerprint(c) == self.fp)
            .ok_or_else(|| rustls::Error::General("hub CA fingerprint does not match the join token".into()))?;
        let mut r = RootCertStore::empty();
        r.add(ca.clone().into_owned()).map_err(|e| rustls::Error::General(format!("pinned CA unusable: {e}")))?;
        let inner = rustls::client::WebPkiServerVerifier::builder_with_provider(Arc::new(r), self.provider.clone())
            .build()
            .map_err(|e| rustls::Error::General(e.to_string()))?;
        inner.verify_server_cert(end_entity, intermediates, server_name, ocsp, now)
    }

    fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// `ocljoin.<secret>.<ca fingerprint>`
pub struct JoinToken {
    pub secret: String,
    pub ca_fingerprint: String,
}

pub const JOIN_PREFIX: &str = "ocljoin";

impl JoinToken {
    pub fn parse(s: &str) -> Result<JoinToken> {
        let mut it = s.trim().split('.');
        match (it.next(), it.next(), it.next(), it.next()) {
            (Some(JOIN_PREFIX), Some(secret), Some(fp), None) if !secret.is_empty() && fp.len() == 64 && fp.chars().all(|c| c.is_ascii_hexdigit()) => {
                Ok(JoinToken { secret: secret.into(), ca_fingerprint: fp.to_lowercase() })
            }
            _ => bail!("not a join token (expected ocljoin.<secret>.<64 hex chars>)"),
        }
    }
    pub fn render(&self) -> String {
        format!("{JOIN_PREFIX}.{}.{}", self.secret, self.ca_fingerprint)
    }
}

/// Split `https://host:port` into (host, port). IPv6 hosts may be bracketed.
pub fn host_port(url: &str) -> Result<(String, u16)> {
    let rest = url.strip_prefix("https://").ok_or_else(|| anyhow!("{url}: expected https://host:port"))?;
    let rest = rest.trim_end_matches('/');
    let (host, port) = if let Some(r) = rest.strip_prefix('[') {
        let (h, p) = r.split_once(']').ok_or_else(|| anyhow!("{url}: unterminated [ipv6]"))?;
        (h.to_string(), p.strip_prefix(':').unwrap_or("7443").to_string())
    } else {
        match rest.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.to_string()),
            None => (rest.to_string(), "7443".to_string()),
        }
    };
    Ok((host, port.parse().with_context(|| format!("{url}: bad port"))?))
}

pub fn server_name(host: &str) -> Result<ServerName<'static>> {
    ServerName::try_from(host.to_string()).map_err(|e| anyhow!("invalid server name {host:?}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_token_roundtrip() {
        let fp = "a".repeat(64);
        let t = JoinToken::parse(&format!("ocljoin.s3cr3t.{fp}")).unwrap();
        assert_eq!(t.secret, "s3cr3t");
        assert_eq!(t.render(), format!("ocljoin.s3cr3t.{fp}"));
        assert!(JoinToken::parse("ocljoin.x.short").is_err());
        assert!(JoinToken::parse(&format!("ocl_x.s.{fp}")).is_err());
    }

    #[test]
    fn urls() {
        assert_eq!(host_port("https://hub.example:8443").unwrap(), ("hub.example".into(), 8443));
        assert_eq!(host_port("https://10.0.0.1").unwrap(), ("10.0.0.1".into(), 7443));
        assert_eq!(host_port("https://[::1]:9000/").unwrap(), ("::1".into(), 9000));
        assert!(host_port("http://x:1").is_err());
    }
}
