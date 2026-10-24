//! Serving the web UI and API (PHASE1_5 §13e).
//!
//! * HTTPS with a certificate from files or from ACME (HTTP-01, e.g. Let's
//!   Encrypt). The certificate is swapped in place when it renews; no restart.
//! * Plain HTTP when no `tls:` is configured (loopback, or behind a proxy).
//! * Every request carries [`ConnInfo`]: the client address (from
//!   `X-Forwarded-For` only when the peer is a trusted proxy) and whether the
//!   browser used HTTPS (for `Secure` cookies).
//! * The SolidJS UI is embedded in the binary (`supervisor/ui-dist`).

use crate::state::Sup;
use anyhow::{anyhow, bail, Context, Result};
use axum::body::Body;
use axum::http::{header, HeaderMap, Request, Response, StatusCode};
use ocl_protocol::config::{AcmeConfig, TlsConfig};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

/// Who is on the other end of a request.
#[derive(Debug, Clone, Copy)]
pub struct ConnInfo {
    pub peer: SocketAddr,
    pub tls: bool,
}

/// Client IP and whether the browser used HTTPS, honouring trusted proxies.
pub fn client(sup: &Sup, info: Option<&ConnInfo>, headers: &HeaderMap) -> (IpAddr, bool) {
    let Some(c) = info else { return ("127.0.0.1".parse().unwrap(), false) };
    let trusted = |ip: IpAddr| sup.cfg.trusted_proxies.iter().any(|p| p.parse::<ipnet::IpNet>().map(|n| n.contains(&ip)).unwrap_or_else(|_| p.parse::<IpAddr>().ok() == Some(ip)));
    if !trusted(c.peer.ip()) {
        return (c.peer.ip(), c.tls);
    }
    // Rightmost address that is not one of our proxies.
    let fwd: Vec<IpAddr> = headers.get_all("x-forwarded-for").iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(',')).filter_map(|s| s.trim().parse().ok()).collect();
    let ip = fwd.iter().rev().find(|ip| !trusted(**ip)).copied().unwrap_or(c.peer.ip());
    let https = headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()).map(|v| v.eq_ignore_ascii_case("https")).unwrap_or(c.tls);
    (ip, https)
}

// --------------------------------------------------------------- certificates

/// The current certificate, replaceable while serving.
#[derive(Debug, Default)]
pub struct CertStore {
    current: RwLock<Option<Arc<CertifiedKey>>>,
}

impl ResolvesServerCert for CertStore {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.current.read().unwrap().clone()
    }
}

impl CertStore {
    pub fn set_pem(&self, chain_pem: &str, key_pem: &str) -> Result<Ts> {
        let certs = ocl_protocol::tls::certs_from_pem(chain_pem)?;
        let key = ocl_protocol::tls::key_from_pem(key_pem)?;
        let signer = rustls::crypto::ring::sign::any_supported_type(&key).map_err(|e| anyhow!("unsupported key: {e}"))?;
        let expires = cert_expiry(chain_pem).unwrap_or(0);
        *self.current.write().unwrap() = Some(Arc::new(CertifiedKey::new(certs, signer)));
        Ok(expires)
    }
    pub fn ready(&self) -> bool {
        self.current.read().unwrap().is_some()
    }
}

type Ts = i64;

/// notAfter of the first certificate in a PEM chain, unix ms.
pub fn cert_expiry(pem: &str) -> Option<Ts> {
    let der = ocl_protocol::tls::certs_from_pem(pem).ok()?.into_iter().next()?;
    let (_, cert) = x509_parser::parse_x509_certificate(der.as_ref()).ok()?;
    Some(cert.validity().not_after.timestamp() * 1000)
}

pub fn server_config(store: Arc<CertStore>, alpn: &[&[u8]]) -> Result<Arc<rustls::ServerConfig>> {
    let mut c = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])?
        .with_no_client_auth()
        .with_cert_resolver(store);
    c.alpn_protocols = alpn.iter().map(|a| a.to_vec()).collect();
    Ok(Arc::new(c))
}

/// Load `tls.cert_file`/`key_file` (and watch them: a replaced certificate is
/// picked up within a minute), or start the ACME client.
pub async fn start_tls(sup: &Arc<Sup>, tls: &TlsConfig, data: &Path) -> Result<Arc<CertStore>> {
    let store = Arc::new(CertStore::default());
    match (&tls.cert_file, &tls.key_file, &tls.acme) {
        (Some(c), Some(k), None) => {
            let load = |c: &PathBuf, k: &PathBuf| -> Result<Ts> {
                let chain = std::fs::read_to_string(c).with_context(|| format!("reading {}", c.display()))?;
                let key = std::fs::read_to_string(k).with_context(|| format!("reading {}", k.display()))?;
                store.set_pem(&chain, &key)
            };
            let exp = load(c, k)?;
            tracing::info!(cert = %c.display(), expires = exp, "TLS certificate loaded");
            let (c, k, st) = (c.clone(), k.clone(), store.clone());
            tokio::spawn(async move {
                let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
                let mut last = (mtime(&c), mtime(&k));
                loop {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    let now = (mtime(&c), mtime(&k));
                    if now != last {
                        last = now;
                        match (std::fs::read_to_string(&c), std::fs::read_to_string(&k)) {
                            (Ok(a), Ok(b)) => match st.set_pem(&a, &b) {
                                Ok(_) => tracing::info!(cert = %c.display(), "TLS certificate reloaded"),
                                Err(e) => tracing::warn!(error = %e, "new TLS certificate rejected; keeping the old one"),
                            },
                            _ => tracing::warn!("could not read the new TLS certificate"),
                        }
                    }
                }
            });
        }
        (None, None, Some(acme)) => {
            let challenges: Arc<Mutex<HashMap<String, String>>> = Default::default();
            let dir = data.join("acme");
            std::fs::create_dir_all(&dir)?;
            // The HTTP-01 responder (and HTTP -> HTTPS redirect) must be up before ordering.
            http01_server(&acme.http01_listen, challenges.clone()).await?;
            let cached = (std::fs::read_to_string(dir.join("cert.pem")), std::fs::read_to_string(dir.join("key.pem")));
            if let (Ok(c), Ok(k)) = cached {
                if let Ok(exp) = store.set_pem(&c, &k) {
                    tracing::info!(expires = exp, "ACME certificate loaded from cache");
                }
            }
            let (sup, acme, st) = (sup.clone(), acme.clone(), store.clone());
            tokio::spawn(async move { acme_loop(sup, acme, st, challenges, dir).await });
        }
        _ => bail!("tls: set either cert_file + key_file, or acme"),
    }
    Ok(store)
}

/// Port 80: answer ACME HTTP-01 challenges, redirect everything else to HTTPS.
async fn http01_server(listen: &str, challenges: Arc<Mutex<HashMap<String, String>>>) -> Result<()> {
    use axum::extract::Path as P;
    use axum::routing::get;
    let ch = challenges.clone();
    let app = axum::Router::new()
        .route(
            "/.well-known/acme-challenge/{token}",
            get(move |P(token): P<String>| {
                let ch = ch.clone();
                async move {
                    match ch.lock().unwrap().get(&token).cloned() {
                        Some(ka) => (StatusCode::OK, ka).into_response(),
                        None => StatusCode::NOT_FOUND.into_response(),
                    }
                }
            }),
        )
        .fallback(|req: Request<Body>| async move {
            let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("").split(':').next().unwrap_or("").to_string();
            let path = req.uri().path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into());
            if host.is_empty() {
                return StatusCode::BAD_REQUEST.into_response();
            }
            (StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, format!("https://{host}{path}"))]).into_response()
        });
    let l = tokio::net::TcpListener::bind(listen).await.with_context(|| format!("binding the ACME HTTP-01 listener {listen}"))?;
    tracing::info!(listen, "ACME HTTP-01 responder and HTTPS redirect");
    tokio::spawn(async move {
        let _ = axum::serve(l, app).await;
    });
    Ok(())
}

use axum::response::IntoResponse;

/// A reqwest-backed client for instant-acme (it may trust an extra CA).
struct AcmeHttp(reqwest::Client);

impl instant_acme::HttpClient for AcmeHttp {
    fn request(
        &self,
        req: axum::http::Request<http_body_util::Full<bytes::Bytes>>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<instant_acme::BytesResponse, instant_acme::Error>> + Send>> {
        let client = self.0.clone();
        Box::pin(async move {
            use http_body_util::BodyExt;
            let (parts, body) = req.into_parts();
            let body = body.collect().await.map(|b| b.to_bytes()).unwrap_or_default();
            let mut r = client.request(parts.method, parts.uri.to_string()).body(body);
            for (k, v) in &parts.headers {
                r = r.header(k, v);
            }
            let resp = r.send().await.map_err(|e| instant_acme::Error::Other(Box::new(e)))?;
            let mut out = axum::http::Response::builder().status(resp.status().as_u16());
            for (k, v) in resp.headers() {
                out = out.header(k, v);
            }
            let bytes = resp.bytes().await.map_err(|e| instant_acme::Error::Other(Box::new(e)))?;
            let resp = out.body(http_body_util::Full::new(bytes)).map_err(|e| instant_acme::Error::Other(Box::new(e)))?;
            Ok(instant_acme::BytesResponse::from(resp))
        })
    }
}

fn acme_http(cfg: &AcmeConfig) -> Result<Box<dyn instant_acme::HttpClient>> {
    let mut b = reqwest::Client::builder().timeout(Duration::from_secs(30));
    if let Some(ca) = &cfg.directory_ca {
        let pem = std::fs::read(ca).with_context(|| format!("reading {}", ca.display()))?;
        b = b.add_root_certificate(reqwest::Certificate::from_pem(&pem)?);
    }
    Ok(Box::new(AcmeHttp(b.build()?)))
}

async fn acme_loop(sup: Arc<Sup>, cfg: AcmeConfig, store: Arc<CertStore>, challenges: Arc<Mutex<HashMap<String, String>>>, dir: PathBuf) {
    let mut backoff = 60u64;
    loop {
        let due = match std::fs::read_to_string(dir.join("cert.pem")).ok().and_then(|c| cert_expiry(&c)) {
            Some(exp) if store.ready() => exp - ocl_protocol::model::now_ms() < 30 * 86_400_000,
            _ => true,
        };
        if due {
            match acme_order(&cfg, &challenges, &dir).await {
                Ok((chain, key)) => match store.set_pem(&chain, &key) {
                    Ok(exp) => {
                        backoff = 60;
                        tracing::info!(domains = ?cfg.domains, expires = exp, "ACME certificate issued");
                        sup.audit(&crate::state::Actor::System, "tls.certificate", &cfg.domains.join(","), serde_json::json!({"expires": exp}), true);
                    }
                    Err(e) => tracing::error!(error = %e, "ACME certificate unusable"),
                },
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), retry_in_s = backoff, "ACME order failed");
                    sup.notify("tls.acme_failed", "warning", "certificate order failed".into(), format!("{e:#}"), None);
                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                    backoff = (backoff * 2).min(6 * 3600);
                    continue;
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(12 * 3600)).await;
    }
}

async fn acme_order(cfg: &AcmeConfig, challenges: &Arc<Mutex<HashMap<String, String>>>, dir: &Path) -> Result<(String, String)> {
    use instant_acme::*;
    let creds_path = dir.join("account.json");
    let account = match std::fs::read_to_string(&creds_path).ok().and_then(|s| serde_json::from_str::<AccountCredentials>(&s).ok()) {
        Some(c) => Account::from_credentials_and_http(c, acme_http(cfg)?).await?,
        None => {
            let contact: Vec<String> = cfg.email.iter().map(|e| format!("mailto:{e}")).collect();
            let contact: Vec<&str> = contact.iter().map(|s| s.as_str()).collect();
            let (a, creds) = Account::create_with_http(&NewAccount { contact: &contact, terms_of_service_agreed: true, only_return_existing: false }, &cfg.directory, None, acme_http(cfg)?).await?;
            std::fs::write(&creds_path, serde_json::to_string(&creds)?)?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&creds_path, std::fs::Permissions::from_mode(0o600))?;
            a
        }
    };
    let ids: Vec<Identifier> = cfg.domains.iter().map(|d| Identifier::Dns(d.clone())).collect();
    let mut order = account.new_order(&NewOrder { identifiers: &ids }).await?;
    let mut ready_urls = vec![];
    for a in order.authorizations().await? {
        if matches!(a.status, AuthorizationStatus::Valid) {
            continue;
        }
        let ch = a.challenges.iter().find(|c| c.r#type == ChallengeType::Http01).ok_or_else(|| anyhow!("the CA offers no HTTP-01 challenge for {:?}", a.identifier))?;
        let ka = order.key_authorization(ch);
        challenges.lock().unwrap().insert(ch.token.clone(), ka.as_str().to_string());
        ready_urls.push(ch.url.clone());
    }
    for u in &ready_urls {
        order.set_challenge_ready(u).await?;
    }
    let mut tries = 0;
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let st = order.refresh().await?;
        match st.status {
            OrderStatus::Ready | OrderStatus::Valid => break,
            OrderStatus::Invalid => bail!("the CA could not validate {:?} (is port 80 of that name reaching {}?)", cfg.domains, cfg.http01_listen),
            _ if tries > 60 => bail!("timed out waiting for the CA"),
            _ => tries += 1,
        }
    }
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
    let mut params = rcgen::CertificateParams::new(cfg.domains.clone())?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    let csr = params.serialize_request(&key)?;
    order.finalize(csr.der()).await?;
    let chain = loop {
        if let Some(c) = order.certificate().await? {
            break c;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    };
    let key_pem = key.serialize_pem();
    std::fs::write(dir.join("cert.pem"), &chain)?;
    std::fs::write(dir.join("key.pem"), &key_pem)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir.join("key.pem"), std::fs::Permissions::from_mode(0o600))?;
    challenges.lock().unwrap().clear();
    Ok((chain, key_pem))
}

// ------------------------------------------------------------------ serving

/// Serve `app` on `listener`, over TLS when `tls` is set; each request gets [`ConnInfo`].
pub async fn serve(listener: tokio::net::TcpListener, app: axum::Router, tls: Option<Arc<rustls::ServerConfig>>) -> Result<()> {
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use tower::ServiceExt;
    let acceptor = tls.map(tokio_rustls::TlsAcceptor::from);
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(error = %e, "accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let app = app.clone();
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            let info = ConnInfo { peer, tls: acceptor.is_some() };
            let svc = app.map_request(move |mut r: Request<hyper::body::Incoming>| {
                r.extensions_mut().insert(info);
                r
            });
            let svc = hyper_util::service::TowerToHyperService::new(svc);
            let builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
            let res = match acceptor {
                Some(a) => match tokio::time::timeout(Duration::from_secs(15), a.accept(tcp)).await {
                    Ok(Ok(s)) => builder.serve_connection_with_upgrades(TokioIo::new(s), svc).await,
                    _ => return,
                },
                None => builder.serve_connection_with_upgrades(TokioIo::new(tcp), svc).await,
            };
            if let Err(e) = res {
                tracing::debug!(error = %e, "connection ended");
            }
        });
    }
}

// --------------------------------------------------------------------- UI

#[derive(rust_embed::RustEmbed)]
#[folder = "ui-dist/"]
struct Ui;

/// The embedded single-page app: assets by path, `index.html` for app routes.
pub fn ui_asset(path: &str) -> Option<Response<Body>> {
    let path = path.trim_start_matches('/');
    let (file, name) = match Ui::get(path) {
        Some(f) if !path.is_empty() => (f, path.to_string()),
        _ => {
            if path.starts_with("assets/") || path.contains('.') && !path.ends_with(".html") {
                return None; // a missing asset is a 404, not the app
            }
            (Ui::get("index.html")?, "index.html".to_string())
        }
    };
    let mime = match name.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript",
        "css" => "text/css",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "webmanifest" | "json" => "application/manifest+json",
        "woff2" => "font/woff2",
        "txt" => "text/plain",
        _ => "application/octet-stream",
    };
    // Hashed assets never change; the shell and the service worker must be fresh.
    let cache = if name.starts_with("assets/") { "public, max-age=31536000, immutable" } else { "no-cache" };
    Response::builder()
        .header(header::CONTENT_TYPE, mime)
        .header(header::CACHE_CONTROL, cache)
        .header("x-content-type-options", "nosniff")
        .header("referrer-policy", "same-origin")
        .header("x-frame-options", "DENY")
        .header(
            "content-security-policy",
            "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; connect-src 'self'; worker-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        )
        .body(Body::from(file.data.into_owned()))
        .ok()
}
