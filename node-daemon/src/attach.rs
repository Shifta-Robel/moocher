//! Node side of "Open in OpenCode" (PHASE1_5 §13d).
//!
//! The hub asks for a tunnel (`NodeCommand::OpenAttach`); the node opens a
//! Tunnel RPC with `Accept { id }` and serves HTTP/1.1 on it. Each request is
//! forwarded to the session's `opencode serve` over its unix socket with the
//! container's OpenCode password added, so the browser never sees it.
//! WebSocket upgrades (OpenCode's terminal) are passed through.

use crate::link::LinkHandle;
use anyhow::Result;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use moocher_protocol::link::TunnelOpen;
use std::path::PathBuf;

/// What every request on an attach connection must present.
#[derive(Clone)]
pub struct Guard {
    pub cookie_sha256: String,
    pub expires_at: i64,
}

impl Guard {
    fn allows<B>(&self, req: &Request<B>) -> bool {
        use sha2::Digest;
        if moocher_protocol::model::now_ms() > self.expires_at {
            return false;
        }
        req.headers()
            .get_all(hyper::header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(';'))
            .filter_map(|kv| kv.trim().strip_prefix("moo_attach="))
            .any(|c| hex::encode(sha2::Sha256::digest(c.as_bytes())) == self.cookie_sha256)
    }
}

pub async fn serve(link: &LinkHandle, me: &str, id: &str, oc_sock: PathBuf, password: &str, guard: Guard) -> Result<()> {
    let (tx, inbound) = crate::relay::open(link, TunnelOpen::Accept { id: id.into() }, me).await?;
    let (ours, theirs) = tokio::io::duplex(256 * 1024);
    tokio::spawn(crate::relay::pump(theirs, tx, inbound));
    let auth = format!("Basic {}", base64::Engine::encode(&base64::engine::general_purpose::STANDARD, format!("opencode:{password}")));
    tokio::spawn(async move {
        let svc = hyper::service::service_fn(move |req: Request<Incoming>| {
            let ok = guard.allows(&req);
            let (sock, auth) = (oc_sock.clone(), auth.clone());
            async move {
                if !ok {
                    // Checked per request: a kept-alive connection is no way around the cookie.
                    return Ok(Response::builder()
                        .status(401)
                        .header("content-type", "text/plain")
                        .body(http_body_util::Full::new(bytes::Bytes::from_static(b"attach cookie missing or expired")).map_err(|e| match e {}).boxed())
                        .unwrap());
                }
                forward(req, sock, auth).await
            }
        });
        if let Err(e) = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(ours), svc).with_upgrades().await {
            tracing::debug!(error = %e, "attach connection ended");
        }
    });
    Ok(())
}

type Body = http_body_util::combinators::BoxBody<bytes::Bytes, hyper::Error>;

async fn forward(mut req: Request<Incoming>, sock: PathBuf, auth: String) -> Result<Response<Body>, hyper::Error> {
    let upgrade = req.headers().contains_key(hyper::header::UPGRADE).then(|| hyper::upgrade::on(&mut req));
    {
        let h = req.headers_mut();
        h.insert(hyper::header::AUTHORIZATION, auth.parse().unwrap());
        // The hub's attach cookie is for the hub only.
        let kept: Vec<String> = h
            .get_all(hyper::header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(';'))
            .map(str::trim)
            .filter(|kv| !kv.is_empty() && !kv.starts_with("moo_attach="))
            .map(String::from)
            .collect();
        h.remove(hyper::header::COOKIE);
        if !kept.is_empty() {
            h.insert(hyper::header::COOKIE, kept.join("; ").parse().unwrap());
        }
    }
    let fail = |msg: String| {
        Response::builder()
            .status(502)
            .header("content-type", "text/plain")
            .body(http_body_util::Full::new(bytes::Bytes::from(msg)).map_err(|e| match e {}).boxed())
            .unwrap()
    };
    let stream = match tokio::net::UnixStream::connect(&sock).await {
        Ok(s) => s,
        Err(e) => return Ok(fail(format!("OpenCode is not reachable: {e}"))),
    };
    let (mut sender, conn) = match hyper::client::conn::http1::handshake(TokioIo::new(stream)).await {
        Ok(x) => x,
        Err(e) => return Ok(fail(format!("OpenCode handshake failed: {e}"))),
    };
    tokio::spawn(async move {
        let _ = conn.with_upgrades().await;
    });
    let mut resp = sender.send_request(req).await?;
    if resp.status() == hyper::StatusCode::SWITCHING_PROTOCOLS {
        if let Some(client_up) = upgrade {
            let server_up = hyper::upgrade::on(&mut resp);
            tokio::spawn(async move {
                if let (Ok(c), Ok(s)) = (client_up.await, server_up.await) {
                    let _ = tokio::io::copy_bidirectional(&mut TokioIo::new(c), &mut TokioIo::new(s)).await;
                }
            });
        }
    }
    Ok(resp.map(|b| b.boxed()))
}
