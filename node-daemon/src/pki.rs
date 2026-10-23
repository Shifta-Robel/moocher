//! The node's link identity: a key generated here (it never leaves this
//! machine), the certificate the hub signed for it, and the hub CA.
//!
//! Files in the pki dir (default `<config dir>/pki`):
//! `node.key` (0600), `node.crt`, `ca.crt`, `node.json` (name, expiry).

use anyhow::{anyhow, bail, Context, Result};
use ocl_protocol::model::{now_ms, Ts};
use ocl_protocol::tls;
use rcgen::{CertificateParams, KeyPair, PKCS_ED25519};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CertMeta {
    pub node: String,
    pub issued_at: Ts,
    pub expires_at: Ts,
}

pub struct Pki {
    pub dir: PathBuf,
}

/// A fresh key pair and a CSR for it. The hub ignores the CSR's subject and
/// names the certificate itself.
pub fn new_key_and_csr() -> Result<(String, String)> {
    let key = KeyPair::generate_for(&PKCS_ED25519)?;
    let csr = CertificateParams::default().serialize_request(&key)?.pem()?;
    Ok((key.serialize_pem(), csr))
}

fn write_private(path: &Path, data: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    f.write_all(data.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

impl Pki {
    pub fn new(dir: PathBuf) -> Pki {
        Pki { dir }
    }

    pub fn exists(&self) -> bool {
        self.dir.join("node.crt").exists() && self.dir.join("node.key").exists() && self.dir.join("ca.crt").exists()
    }

    pub fn meta(&self) -> Option<CertMeta> {
        serde_json::from_str(&std::fs::read_to_string(self.dir.join("node.json")).ok()?).ok()
    }

    /// Store a (new) identity. The key and certificate are replaced together.
    pub fn store(&self, key_pem: &str, cert_pem: &str, ca_pem: Option<&str>, meta: &CertMeta) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))?;
        }
        if let Some(ca) = ca_pem {
            std::fs::write(self.dir.join("ca.crt"), ca)?;
        }
        write_private(&self.dir.join("node.key"), key_pem)?;
        std::fs::write(self.dir.join("node.crt"), cert_pem)?;
        std::fs::write(self.dir.join("node.json"), serde_json::to_vec_pretty(meta)?)?;
        Ok(())
    }

    pub fn client_config(&self) -> Result<Arc<rustls::ClientConfig>> {
        if !self.exists() {
            bail!("no node certificate in {} — enroll this machine first: opencode-node join https://<hub>:7443 --token <join token>", self.dir.display());
        }
        let read = |f: &str| std::fs::read_to_string(self.dir.join(f)).with_context(|| format!("reading {}", self.dir.join(f).display()));
        let ca = tls::certs_from_pem(&read("ca.crt")?)?;
        let chain = tls::certs_from_pem(&read("node.crt")?)?;
        let key = tls::key_from_pem(&read("node.key")?)?;
        Ok(Arc::new(tls::link_client(&ca, chain, key)?))
    }

    /// Renew once two thirds of the lifetime have passed.
    pub fn needs_renewal(&self) -> bool {
        match self.meta() {
            Some(m) => now_ms() > m.issued_at + (m.expires_at - m.issued_at) * 2 / 3,
            None => false,
        }
    }
}

/// gRPC channel to the supervisor over TLS with our own connector, so the same
/// code serves enrollment (pinned CA, no client cert) and normal links (mTLS).
pub async fn tls_channel(url: &str, cfg: Arc<rustls::ClientConfig>) -> Result<tonic::transport::Channel> {
    use hyper_util::rt::TokioIo;
    use std::time::Duration;
    let (host, port) = tls::host_port(url)?;
    let sn = tls::server_name(&host)?;
    let connector = tokio_rustls::TlsConnector::from(cfg);
    let authority = if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
    let ep = tonic::transport::Endpoint::try_from(format!("http://{authority}"))?
        .connect_timeout(Duration::from_secs(15))
        .http2_keep_alive_interval(Duration::from_secs(20))
        .keep_alive_timeout(Duration::from_secs(10))
        .keep_alive_while_idle(true);
    let ch = ep
        .connect_with_connector(tower::service_fn(move |_: tonic::transport::Uri| {
            let (host, sn, connector) = (host.clone(), sn.clone(), connector.clone());
            async move {
                let tcp = tokio::net::TcpStream::connect((host.as_str(), port)).await?;
                tcp.set_nodelay(true)?;
                let tls = connector.connect(sn, tcp).await?;
                Ok::<_, std::io::Error>(TokioIo::new(tls))
            }
        }))
        .await
        .map_err(|e| anyhow!("connecting to {url}: {}", error_chain(&e)))?;
    Ok(ch)
}

fn error_chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut cur = e.source();
    while let Some(c) = cur {
        s.push_str(": ");
        s.push_str(&c.to_string());
        cur = c.source();
    }
    s
}

pub struct Enrolled {
    pub node: String,
    pub role: String,
    pub status: String,
    pub expires_at: Ts,
}

/// `opencode-node join`: enroll with a one-time join token.
pub async fn join(pki: &Pki, url: &str, token: &str) -> Result<Enrolled> {
    let t = tls::JoinToken::parse(token)?;
    let (key_pem, csr_pem) = new_key_and_csr()?;
    let ch = tls_channel(url, Arc::new(tls::pinned_ca_client(&t.ca_fingerprint)?)).await?;
    let mut client = ocl_protocol::proto::node_link_client::NodeLinkClient::new(ch);
    let r = client
        .enroll(ocl_protocol::proto::EnrollRequest { token: t.secret.clone(), csr_pem })
        .await
        .map_err(|s| anyhow!("enrollment refused: {}", s.message()))?
        .into_inner();
    // The handshake already pinned the CA; check the CA we are told to store too.
    let ca = tls::certs_from_pem(&r.ca_pem)?;
    if tls::fingerprint(&ca[0]) != t.ca_fingerprint {
        bail!("hub returned a CA certificate that does not match the join token");
    }
    pki.store(&key_pem, &r.cert_pem, Some(&r.ca_pem), &CertMeta { node: r.node.clone(), issued_at: now_ms(), expires_at: r.expires_at })?;
    Ok(Enrolled { node: r.node, role: r.role, status: r.status, expires_at: r.expires_at })
}
