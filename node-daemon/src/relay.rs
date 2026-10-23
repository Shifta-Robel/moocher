//! `via_node` egress (ARCHITECTURE D10): send an LLM call out through another
//! cluster machine's IP. TLS to the provider stays end to end, so the relay
//! (and the hub in between) only ever see encrypted bytes.
//!
//! Dialing side (this node's keyproxy):
//!   reqwest --socks5h--> local listener (127.0.0.1, per-daemon password; the
//!   username names the relay) --Tunnel RPC, Dial{relay,host,port}--> hub.
//! Relay side:
//!   hub --NodeCommand::OpenTunnel{id,host,port}--> relay connects to
//!   host:port (from `egress_bind` if set), then opens its own Tunnel RPC with
//!   Accept{id}; the hub splices the two streams.
//!
//! Relay limits: `relay_ports` (default 443) and optional `relay_allow_hosts`.

use crate::link::LinkHandle;
use anyhow::{anyhow, bail, Context, Result};
use ocl_protocol::link::TunnelOpen;
use ocl_protocol::proto::TunnelFrame;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Streaming;

const CHUNK: usize = 32 * 1024;

/// Local SOCKS5 entry point for `via_node` routes.
#[derive(Clone)]
pub struct Dialer {
    port: u16,
    password: Arc<String>,
    /// Last failure per relay. SOCKS cannot carry a reason, so keyproxy
    /// looks it up here to explain a failed upstream call.
    errors: Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
}

impl Dialer {
    /// Bind 127.0.0.1:0 and serve SOCKS5 CONNECT requests through the hub.
    pub async fn start(node: String, link: LinkHandle) -> Result<Dialer> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let password = Arc::new(crate::manager::random_hex(16));
        let errors: Arc<std::sync::Mutex<std::collections::HashMap<String, String>>> = Default::default();
        let (pw, errs) = (password.clone(), errors.clone());
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else { continue };
                let (link, pw, node, errs) = (link.clone(), pw.clone(), node.clone(), errs.clone());
                tokio::spawn(async move {
                    match socks_session(sock, &pw, &node, &link).await {
                        Ok(Some(relay)) => {
                            errs.lock().unwrap().remove(&relay);
                        }
                        Ok(None) => {}
                        Err((relay, e)) => {
                            let msg = format!("{e:#}");
                            tracing::warn!(relay = ?relay, error = %msg, "via_node connection failed");
                            if let Some(r) = relay {
                                errs.lock().unwrap().insert(r, msg);
                            }
                        }
                    }
                });
            }
        });
        Ok(Dialer { port, password, errors })
    }

    pub fn last_error(&self, relay: &str) -> Option<String> {
        self.errors.lock().unwrap().get(relay).cloned()
    }

    /// Proxy URL for reqwest: the username selects the relay node.
    pub fn proxy_url(&self, relay: &str) -> String {
        format!("socks5h://{relay}:{}@127.0.0.1:{}", self.password, self.port)
    }
}

/// Ok(Some(relay)) after a relayed connection, Err((relay if known, reason)).
async fn socks_session(s: TcpStream, password: &str, me: &str, link: &LinkHandle) -> Result<Option<String>, (Option<String>, anyhow::Error)> {
    let mut s = s;
    let (relay, host, port) = socks_handshake(&mut s, password).await.map_err(|e| (None, e))?;
    let started = std::time::Instant::now();
    let (tx, inbound) = match open(link, TunnelOpen::Dial { relay: relay.clone(), host: host.clone(), port }, me).await {
        Ok(x) => x,
        Err(e) => {
            let _ = reply(&mut s, 5).await;
            return Err((Some(relay.clone()), e.context(format!("via {relay} to {host}:{port}"))));
        }
    };
    link.note_tunnel(&relay, started.elapsed());
    reply(&mut s, 0).await.map_err(|e| (Some(relay.clone()), e.into()))?;
    pump(s, tx, inbound).await;
    Ok(Some(relay))
}

/// SOCKS5 with username/password: returns (relay = username, host, port).
async fn socks_handshake(s: &mut TcpStream, password: &str) -> Result<(String, String, u16)> {
    // Greeting: VER NMETHODS METHODS...; we only offer username/password (0x02).
    let mut hdr = [0u8; 2];
    s.read_exact(&mut hdr).await?;
    let mut methods = vec![0u8; hdr[1] as usize];
    s.read_exact(&mut methods).await?;
    if hdr[0] != 5 || !methods.contains(&2) {
        s.write_all(&[5, 0xff]).await?;
        bail!("client did not offer username/password auth");
    }
    s.write_all(&[5, 2]).await?;
    // RFC 1929: VER ULEN UNAME PLEN PASSWD
    let mut v = [0u8; 2];
    s.read_exact(&mut v).await?;
    let mut user = vec![0u8; v[1] as usize];
    s.read_exact(&mut user).await?;
    let mut pl = [0u8; 1];
    s.read_exact(&mut pl).await?;
    let mut pass = vec![0u8; pl[0] as usize];
    s.read_exact(&mut pass).await?;
    let ok = pass.len() == password.len() && pass.iter().zip(password.as_bytes()).fold(0u8, |a, (x, y)| a | (x ^ y)) == 0;
    if !ok {
        s.write_all(&[1, 1]).await?;
        bail!("bad proxy password");
    }
    s.write_all(&[1, 0]).await?;
    let relay = String::from_utf8(user)?;
    // Request: VER CMD RSV ATYP DST.ADDR DST.PORT (CONNECT only)
    let mut req = [0u8; 4];
    s.read_exact(&mut req).await?;
    if req[1] != 1 {
        reply(s, 7).await?;
        bail!("only CONNECT is supported");
    }
    let host = match req[3] {
        1 => {
            let mut a = [0u8; 4];
            s.read_exact(&mut a).await?;
            std::net::Ipv4Addr::from(a).to_string()
        }
        3 => {
            let mut l = [0u8; 1];
            s.read_exact(&mut l).await?;
            let mut d = vec![0u8; l[0] as usize];
            s.read_exact(&mut d).await?;
            String::from_utf8(d)?
        }
        4 => {
            let mut a = [0u8; 16];
            s.read_exact(&mut a).await?;
            std::net::Ipv6Addr::from(a).to_string()
        }
        _ => {
            reply(s, 8).await?;
            bail!("bad address type");
        }
    };
    let mut p = [0u8; 2];
    s.read_exact(&mut p).await?;
    Ok((relay, host, u16::from_be_bytes(p)))
}

async fn reply(s: &mut TcpStream, code: u8) -> std::io::Result<()> {
    s.write_all(&[5, code, 0, 1, 0, 0, 0, 0, 0, 0]).await
}

/// Open a Tunnel RPC and wait for the hub's "ready" (the far end is connected).
async fn open(link: &LinkHandle, open: TunnelOpen, me: &str) -> Result<(mpsc::Sender<TunnelFrame>, Streaming<TunnelFrame>)> {
    let ch = link.channel().ok_or_else(|| anyhow!("not connected to the hub"))?;
    let mut client = ocl_protocol::proto::node_link_client::NodeLinkClient::new(ch);
    let (tx, rx) = mpsc::channel::<TunnelFrame>(64);
    let mut header = serde_json::to_value(&open)?;
    header["from"] = serde_json::json!(me);
    tx.send(TunnelFrame { header: serde_json::to_vec(&header)?, data: vec![], eof: false }).await?;
    let mut inbound = client.tunnel(ReceiverStream::new(rx)).await.map_err(|s| anyhow!("{}", s.message()))?.into_inner();
    let first = tokio::time::timeout(Duration::from_secs(30), inbound.message())
        .await
        .map_err(|_| anyhow!("relay did not connect within 30s"))?
        .map_err(|s| anyhow!("{}", s.message()))?
        .ok_or_else(|| anyhow!("tunnel closed"))?;
    if first.header != b"ready" {
        bail!("unexpected tunnel reply");
    }
    Ok((tx, inbound))
}

/// Copy bytes between a TCP stream and a tunnel until both directions end.
pub async fn pump(tcp: TcpStream, tx: mpsc::Sender<TunnelFrame>, mut inbound: Streaming<TunnelFrame>) {
    let (mut rd, mut wr) = tcp.into_split();
    let up = async move {
        let mut buf = vec![0u8; CHUNK];
        loop {
            match rd.read(&mut buf).await {
                Ok(0) | Err(_) => {
                    let _ = tx.send(TunnelFrame { header: vec![], data: vec![], eof: true }).await;
                    break;
                }
                Ok(n) => {
                    if tx.send(TunnelFrame { header: vec![], data: buf[..n].to_vec(), eof: false }).await.is_err() {
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
}

/// Relay side of `NodeCommand::OpenTunnel`: connect to the target from this
/// machine, then hand the connection to the hub.
pub async fn accept(cfg: &ocl_protocol::config::NodeConfig, link: &LinkHandle, id: &str, host: &str, port: u16, from: &str) -> Result<()> {
    if !(cfg.relay || cfg.role == ocl_protocol::config::NodeRole::Relay) {
        bail!("node {} does not relay traffic (set relay: true or role: relay)", cfg.name);
    }
    if !cfg.relay_ports.contains(&port) {
        bail!("port {port} is not in this relay's relay_ports {:?}", cfg.relay_ports);
    }
    if !cfg.relay_allow_hosts.is_empty() && !cfg.relay_allow_hosts.iter().any(|h| host_matches(h, host)) {
        bail!("host {host} is not in this relay's relay_allow_hosts");
    }
    let tcp = connect_from(cfg.egress_bind.as_deref(), host, port).await.with_context(|| format!("relay {} connecting to {host}:{port}", cfg.name))?;
    let (tx, inbound) = open(link, TunnelOpen::Accept { id: id.into() }, &cfg.name).await?;
    tracing::debug!(id, host, port, from, "relaying");
    tokio::spawn(pump(tcp, tx, inbound));
    Ok(())
}

/// `*.example.com` matches subdomains; anything else must match exactly.
fn host_matches(pattern: &str, host: &str) -> bool {
    match pattern.strip_prefix("*.") {
        Some(suffix) => host.ends_with(&format!(".{suffix}")),
        None => pattern.eq_ignore_ascii_case(host),
    }
}

/// TCP connect, optionally from a specific local address (multi-IP hosts).
pub async fn connect_from(bind: Option<&str>, host: &str, port: u16) -> Result<TcpStream> {
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host, port)).await?.collect();
    let mut last = anyhow!("{host} did not resolve");
    for addr in addrs {
        let sock = if addr.is_ipv4() { tokio::net::TcpSocket::new_v4()? } else { tokio::net::TcpSocket::new_v6()? };
        if let Some(b) = bind {
            let ip: std::net::IpAddr = b.parse().with_context(|| format!("egress_bind {b:?} is not an IP"))?;
            if ip.is_ipv4() != addr.is_ipv4() {
                continue;
            }
            sock.bind(std::net::SocketAddr::new(ip, 0))?;
        }
        match tokio::time::timeout(Duration::from_secs(10), sock.connect(addr)).await {
            Ok(Ok(s)) => {
                let _ = s.set_nodelay(true);
                return Ok(s);
            }
            Ok(Err(e)) => last = e.into(),
            Err(_) => last = anyhow!("connect to {addr} timed out"),
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    #[test]
    fn host_patterns() {
        assert!(super::host_matches("*.groq.com", "api.groq.com"));
        assert!(!super::host_matches("*.groq.com", "groq.com.evil.net"));
        assert!(super::host_matches("api.kilo.ai", "API.kilo.ai"));
    }
}
