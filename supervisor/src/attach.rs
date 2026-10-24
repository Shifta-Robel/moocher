//! "Open in OpenCode" (PHASE1_5 §13d): OpenCode's own web app for one
//! instance, on a second port of the hub (its own origin: OpenCode uses
//! absolute asset paths).
//!
//! 1. The dashboard asks `POST /api/v1/sessions/{id}/attach` for a one-time
//!    ticket (60 s).
//! 2. The browser opens `<attach origin>/__ocl/attach?ticket=…`; the hub
//!    swaps the ticket for an `HttpOnly` cookie (8 h) naming the instance.
//! 3. Every further connection is checked against that cookie, then spliced
//!    byte for byte through a tunnel on the node's link to an HTTP proxy on
//!    the node, which adds the container's OpenCode password (it never reaches
//!    the browser) and forwards to `opencode serve`, WebSocket upgrades included.

use crate::state::{Actor, Sup};
use anyhow::{anyhow, bail, Result};
use ocl_protocol::link::NodeCommand;
use ocl_protocol::model::{now_ms, Ts};
#[cfg(test)]
use ocl_protocol::model::SessionState;
use ocl_protocol::proto::TunnelFrame;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tonic::{Status, Streaming};

const TICKET_MS: Ts = 60_000;
const COOKIE_MS: Ts = 8 * 3_600_000;
const COOKIE: &str = "ocl_attach";

pub type TunnelEnds = (Streaming<TunnelFrame>, mpsc::Sender<Result<TunnelFrame, Status>>);

#[derive(Default)]
pub struct AttachState {
    /// ticket -> (session, operator, expires)
    tickets: Mutex<HashMap<String, (String, String, Ts)>>,
    /// sha256(cookie) -> (session, operator, expires)
    cookies: Mutex<HashMap<String, (String, String, Ts)>>,
    /// Tunnels the hub asked a node to open, waiting for its `Accept`.
    pub pending: Mutex<HashMap<String, oneshot::Sender<TunnelEnds>>>,
}

impl Sup {
    /// A one-time ticket and the URL that redeems it.
    pub fn attach_ticket(&self, actor: &Actor, session: &str, dashboard_host: Option<&str>, https: bool) -> Result<serde_json::Value> {
        let s = self.session(session)?;
        if !s.report.state.is_live() {
            bail!("session {session} is {}; only live instances can be opened", s.report.state.as_str());
        }
        let listen = self.cfg.attach_listen.as_deref().ok_or_else(|| anyhow!("attach_listen is not configured on the hub (supervisor.yaml)"))?;
        let base = match &self.cfg.attach_url {
            Some(u) => u.trim_end_matches('/').to_string(),
            None => {
                let port = listen.rsplit(':').next().unwrap_or("7444");
                let host = dashboard_host.map(|h| h.rsplit_once(':').map(|(h, _)| h).unwrap_or(h).to_string()).unwrap_or_else(|| "127.0.0.1".into());
                let scheme = if self.cfg.tls.is_some() || https { "https" } else { "http" };
                format!("{scheme}://{host}:{port}")
            }
        };
        let ticket = crate::auth::random_hex(24);
        let now = now_ms();
        let mut t = self.attach.tickets.lock().unwrap();
        t.retain(|_, v| v.2 > now);
        t.insert(ticket.clone(), (session.into(), actor.to_string(), now + TICKET_MS));
        drop(t);
        self.audit(actor, "session.attach", session, json!({"node": s.report.node}), true);
        Ok(json!({"url": format!("{base}/__ocl/attach?ticket={ticket}"), "ticket": ticket, "expires_in_s": TICKET_MS / 1000}))
    }

    fn redeem_ticket(&self, ticket: &str) -> Option<(String, String)> {
        let (session, who, exp) = self.attach.tickets.lock().unwrap().remove(ticket)?;
        (exp > now_ms()).then_some((session, who))
    }

    fn cookie_session(&self, cookie: &str) -> Option<(String, String, Ts)> {
        let c = self.attach.cookies.lock().unwrap();
        let (s, who, exp) = c.get(&crate::db::hash_token(cookie))?.clone();
        (exp > now_ms()).then_some((s, who, exp))
    }

    /// Ask the session's node for a tunnel to its OpenCode server.
    async fn attach_tunnel(&self, session: &str, cookie: &str, expires_at: Ts) -> Result<TunnelEnds> {
        let s = self.session(session)?;
        if !s.report.state.is_live() {
            bail!("session {session} is {}", s.report.state.as_str());
        }
        let id = ocl_protocol::new_id("a");
        let (tx, rx) = oneshot::channel();
        self.attach.pending.lock().unwrap().insert(id.clone(), tx);
        let res = self.nodes.call(&s.report.node, NodeCommand::OpenAttach { id: id.clone(), session: session.into(), cookie_sha256: Some(crate::db::hash_token(cookie)), expires_at: Some(expires_at) }, Duration::from_secs(15)).await;
        if let Err(e) = res {
            self.attach.pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        match tokio::time::timeout(Duration::from_secs(15), rx).await {
            Ok(Ok(ends)) => Ok(ends),
            _ => {
                self.attach.pending.lock().unwrap().remove(&id);
                bail!("node {} did not open the tunnel", s.report.node)
            }
        }
    }
}

/// Serve the attach origin.
pub async fn serve(sup: Arc<Sup>, listener: tokio::net::TcpListener, tls: Option<Arc<rustls::ServerConfig>>) {
    let acceptor = tls.map(tokio_rustls::TlsAcceptor::from);
    loop {
        let Ok((tcp, peer)) = listener.accept().await else { continue };
        let (sup, acceptor) = (sup.clone(), acceptor.clone());
        tokio::spawn(async move {
            let secure = acceptor.is_some();
            let res = match acceptor {
                Some(a) => match tokio::time::timeout(Duration::from_secs(15), a.accept(tcp)).await {
                    Ok(Ok(s)) => handle(&sup, s, secure).await,
                    _ => return,
                },
                None => handle(&sup, tcp, secure).await,
            };
            if let Err(e) = res {
                tracing::debug!(peer = %peer, error = %e, "attach connection ended");
            }
        });
    }
}

async fn respond<S: AsyncWrite + Unpin>(s: &mut S, status: &str, extra: &str, body: &str) -> Result<()> {
    let msg = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n{extra}\r\n{body}",
        body.len()
    );
    s.write_all(msg.as_bytes()).await?;
    s.shutdown().await.ok();
    Ok(())
}

fn page(title: &str, text: &str) -> String {
    format!("<!doctype html><meta charset=utf-8><title>{title}</title><body style=\"font:16px system-ui;margin:3rem;max-width:40rem\"><h1>{title}</h1><p>{text}</p>")
}

async fn handle<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(sup: &Arc<Sup>, mut s: S, secure: bool) -> Result<()> {
    // Read the first request head (the rest of the connection is spliced).
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let head_len = loop {
        let n = tokio::time::timeout(Duration::from_secs(30), s.read(&mut chunk)).await??;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > 32 * 1024 {
            return respond(&mut s, "431 Request Header Fields Too Large", "", "").await;
        }
    };
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    if req.parse(&buf[..head_len]).is_err() {
        return respond(&mut s, "400 Bad Request", "", "").await;
    }
    let path = req.path.unwrap_or("/").to_string();
    let cookie = req
        .headers
        .iter()
        .filter(|h| h.name.eq_ignore_ascii_case("cookie"))
        .filter_map(|h| std::str::from_utf8(h.value).ok())
        .flat_map(|v| v.split(';'))
        .find_map(|kv| kv.trim().strip_prefix(&format!("{COOKIE}=")).map(String::from));
    if let Some(q) = path.strip_prefix("/__ocl/attach") {
        let ticket = q.trim_start_matches('?').split('&').find_map(|kv| kv.strip_prefix("ticket=")).unwrap_or("");
        let Some((session, who)) = sup.redeem_ticket(ticket) else {
            return respond(&mut s, "403 Forbidden", "", &page("Link expired", "This link works once and for 60 seconds. Use <b>Open in OpenCode</b> on the dashboard again.")).await;
        };
        let token = crate::auth::random_hex(32);
        sup.attach.cookies.lock().unwrap().insert(crate::db::hash_token(&token), (session.clone(), who.clone(), now_ms() + COOKIE_MS));
        if let Ok(mut si) = sup.session(&session) {
            si.attached_by = Some(who.clone());
            si.attached_until = Some(now_ms() + COOKIE_MS);
            sup.save_session(&si);
        }
        sup.log("session.attached", None, Some(&session), json!({"by": who}));
        let cookie = format!(
            "Set-Cookie: {COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}{}\r\nLocation: /\r\n",
            COOKIE_MS / 1000,
            if secure { "; Secure" } else { "" }
        );
        return respond(&mut s, "302 Found", &cookie, "").await;
    }
    let Some((session, _who, exp)) = cookie.as_deref().and_then(|c| sup.cookie_session(c)) else {
        return respond(&mut s, "401 Unauthorized", "", &page("Not attached", "Open an instance from the dashboard with <b>Open in OpenCode</b>.")).await;
    };
    let (mut inbound, tx) = match sup.attach_tunnel(&session, cookie.as_deref().unwrap_or(""), exp).await {
        Ok(t) => t,
        Err(e) => return respond(&mut s, "502 Bad Gateway", "", &page("Instance unavailable", &format!("{e:#}"))).await,
    };
    // Splice: what was already read first, then both directions.
    let (mut rd, mut wr) = tokio::io::split(s);
    let first = buf;
    let up = async move {
        if tx.send(Ok(TunnelFrame { header: vec![], data: first, eof: false })).await.is_err() {
            return;
        }
        let mut b = vec![0u8; 64 * 1024];
        loop {
            match rd.read(&mut b).await {
                Ok(0) | Err(_) => {
                    let _ = tx.send(Ok(TunnelFrame { header: vec![], data: vec![], eof: true })).await;
                    break;
                }
                Ok(n) => {
                    if tx.send(Ok(TunnelFrame { header: vec![], data: b[..n].to_vec(), eof: false })).await.is_err() {
                        break;
                    }
                }
            }
        }
    };
    let down = async move {
        while let Ok(Some(f)) = inbound.message().await {
            if !f.data.is_empty() && wr.write_all(&f.data).await.is_err() {
                break;
            }
            if f.eof {
                break;
            }
        }
        let _ = wr.shutdown().await;
    };
    tokio::join!(up, down);
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn ticket_once() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Db::open(&dir.path().join("t.db")).unwrap();
        let mut cfg = ocl_protocol::config::SupervisorConfig::default();
        cfg.attach_listen = Some("127.0.0.1:7444".into());
        let sup = super::Sup::new(db, cfg, dir.path().into());
        let mut r = crate::state::tests_report("s-1");
        r.state = super::SessionState::Idle;
        sup.merge_report(r);
        let v = sup.attach_ticket(&super::Actor::Operator("op".into()), "s-1", Some("hub.example.org:7700"), true).unwrap();
        assert!(v["url"].as_str().unwrap().starts_with("https://hub.example.org:7444/__ocl/attach?ticket="));
        let t = v["ticket"].as_str().unwrap();
        assert!(sup.redeem_ticket(t).is_some());
        assert!(sup.redeem_ticket(t).is_none(), "a ticket works once");
    }
}
