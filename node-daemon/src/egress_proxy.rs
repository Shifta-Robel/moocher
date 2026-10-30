//! `container.egress: allowlist` — an HTTP(S) proxy for one instance.
//!
//! The container still has no network. The bridge forwards
//! `127.0.0.1:3128` inside it to `proxy.sock` here, and `HTTP(S)_PROXY`
//! point at that port. Only two request forms are accepted:
//!
//! * `CONNECT host:port` (HTTPS and anything else over TLS): a byte tunnel
//!   once the host is allowed, so TLS stays end to end;
//! * `GET http://host/path` (plain HTTP): forwarded with the request line in
//!   origin form, one request per connection.
//!
//! The list is read from the live config on every connection. A host name
//! that resolves to a loopback, private or link-local address is refused
//! unless the allowlist names that address itself, so an allowed name cannot
//! be pointed at the node or the LAN.

use crate::cfgstore::ConfigStore;
use crate::link::LinkHandle;
use moocher_protocol::config::HostPattern;
use moocher_protocol::link::NodeEvent;
use moocher_protocol::model::now_ms;
use serde_json::json;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpStream, UnixListener};

const MAX_HEAD: usize = 16 * 1024;

#[derive(Clone)]
pub struct EgressProxy {
    pub session: String,
    pub agent: String,
    pub cfg: ConfigStore,
    pub link: LinkHandle,
    /// host -> last time a denial was reported (once per 10 minutes).
    denied: Arc<Mutex<HashMap<String, i64>>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Target {
    Connect { host: String, port: u16 },
    /// Plain HTTP: the head rewritten to origin form.
    Http { host: String, port: u16, head: Vec<u8> },
}

/// Parse a proxy request head (everything up to and including `\r\n\r\n`).
pub fn parse_head(head: &[u8]) -> Result<Target, String> {
    let text = std::str::from_utf8(head).map_err(|_| "request head is not UTF-8".to_string())?;
    let mut lines = text.split("\r\n");
    let first = lines.next().unwrap_or("");
    let mut parts = first.split(' ');
    let (method, target, version) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    if !version.starts_with("HTTP/1.") {
        return Err(format!("not an HTTP/1 proxy request: {first:?}"));
    }
    if method == "CONNECT" {
        let (host, port) = split_host_port(target, None)?;
        return Ok(Target::Connect { host, port });
    }
    let rest = target.strip_prefix("http://").ok_or_else(|| {
        if target.starts_with("https://") {
            "https:// URLs go through CONNECT".to_string()
        } else {
            format!("proxy requests need an absolute http:// URL, got {target:?}")
        }
    })?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = split_host_port(authority, Some(80))?;
    let mut out = format!("{method} {path} {version}\r\n");
    for l in lines {
        if l.is_empty() {
            continue;
        }
        let name = l.split(':').next().unwrap_or("").trim().to_ascii_lowercase();
        if name.starts_with("proxy-") || name == "connection" || name == "keep-alive" {
            continue;
        }
        out.push_str(l);
        out.push_str("\r\n");
    }
    // One request per connection: the next one may be for another host.
    out.push_str("Connection: close\r\n\r\n");
    Ok(Target::Http { host, port, head: out.into_bytes() })
}

fn split_host_port(s: &str, default: Option<u16>) -> Result<(String, u16), String> {
    let s = s.rsplit_once('@').map(|(_, h)| h).unwrap_or(s); // no credentials in the authority
    let (host, port) = if let Some(rest) = s.strip_prefix('[') {
        let (h, after) = rest.split_once(']').ok_or("unclosed [")?;
        (h.to_string(), after.strip_prefix(':'))
    } else {
        match s.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), Some(p)),
            None => (s.to_string(), None),
        }
    };
    let port = match (port, default) {
        (Some(p), _) => p.parse::<u16>().map_err(|_| format!("bad port in {s:?}"))?,
        (None, Some(d)) => d,
        (None, None) => return Err(format!("CONNECT needs host:port, got {s:?}")),
    };
    if host.is_empty() {
        return Err("empty host".into());
    }
    Ok((host.to_ascii_lowercase(), port))
}

/// Addresses a public name must not lead to.
pub fn is_internal(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v.is_loopback() || v.is_private() || v.is_link_local() || v.is_unspecified() || v.is_broadcast() || v.is_multicast() || v.octets()[0] == 100 && (v.octets()[1] & 0xc0) == 64,
        IpAddr::V6(v) => {
            if let Some(m) = v.to_ipv4_mapped() {
                return is_internal(IpAddr::V4(m));
            }
            let seg = v.segments()[0];
            v.is_loopback() || v.is_unspecified() || v.is_multicast() || (seg & 0xfe00) == 0xfc00 || (seg & 0xffc0) == 0xfe80
        }
    }
}

/// The allowlist decision for `host:port`: Ok(addresses to try) or why not.
pub async fn decide(allow: &[HostPattern], host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
    let Some(p) = allow.iter().find(|p| p.matches(host, port)) else {
        return Err(format!("{host}:{port} is not in this agent's container.allow_hosts"));
    };
    let addrs: Vec<SocketAddr> = tokio::time::timeout(Duration::from_secs(10), tokio::net::lookup_host((host, port)))
        .await
        .map_err(|_| format!("resolving {host} timed out"))?
        .map_err(|e| format!("resolving {host}: {e}"))?
        .collect();
    if p.is_ip() {
        return Ok(addrs);
    }
    let public: Vec<SocketAddr> = addrs.iter().copied().filter(|a| !is_internal(a.ip())).collect();
    if public.is_empty() {
        return Err(format!("{host} resolves to an internal address ({}); list the address itself to allow it", addrs.first().map(|a| a.ip().to_string()).unwrap_or_default()));
    }
    Ok(public)
}

impl EgressProxy {
    pub fn new(session: &str, agent: &str, cfg: ConfigStore, link: LinkHandle) -> EgressProxy {
        EgressProxy { session: session.into(), agent: agent.into(), cfg, link, denied: Default::default() }
    }

    pub fn serve(self, sock: PathBuf) -> anyhow::Result<tokio::task::JoinHandle<()>> {
        let _ = std::fs::remove_file(&sock);
        let l = UnixListener::bind(&sock)?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o666))?;
        }
        Ok(tokio::spawn(async move {
            while let Ok((c, _)) = l.accept().await {
                let me = self.clone();
                tokio::spawn(async move { me.handle(c).await });
            }
        }))
    }

    fn allow(&self) -> Vec<HostPattern> {
        let snap = self.cfg.get();
        snap.agents.get(&self.agent).map(|a| a.cfg.container.allow_hosts.iter().filter_map(|h| HostPattern::parse(h).ok()).collect()).unwrap_or_default()
    }

    fn report_denied(&self, host: &str, port: u16, why: &str) {
        tracing::info!(session = %self.session, host, port, why, "egress denied");
        let now = now_ms();
        let mut d = self.denied.lock().unwrap();
        if d.get(host).is_some_and(|t| now - t < 600_000) {
            return;
        }
        d.insert(host.to_string(), now);
        self.link.emit(NodeEvent::Activity { session: self.session.clone(), kind: "egress.denied".into(), detail: json!({"host": host, "port": port, "why": why}) });
    }

    async fn handle<S: AsyncRead + AsyncWrite + Unpin>(&self, mut c: S) {
        let mut buf = Vec::with_capacity(1024);
        let end = loop {
            let mut chunk = [0u8; 4096];
            let n = match tokio::time::timeout(Duration::from_secs(30), c.read(&mut chunk)).await {
                Ok(Ok(n)) if n > 0 => n,
                _ => return,
            };
            buf.extend_from_slice(&chunk[..n]);
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            if buf.len() > MAX_HEAD {
                return reply(&mut c, 431, "request head too large").await;
            }
        };
        let target = match parse_head(&buf[..end]) {
            Ok(t) => t,
            Err(e) => return reply(&mut c, 400, &e).await,
        };
        let (host, port) = match &target {
            Target::Connect { host, port } | Target::Http { host, port, .. } => (host.clone(), *port),
        };
        let addrs = match decide(&self.allow(), &host, port).await {
            Ok(a) => a,
            Err(why) => {
                self.report_denied(&host, port, &why);
                return reply(&mut c, 403, &format!("[moocher] egress refused: {why}")).await;
            }
        };
        let mut up = None;
        for a in &addrs {
            if let Ok(Ok(s)) = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(a)).await {
                up = Some(s);
                break;
            }
        }
        let Some(mut up) = up else {
            return reply(&mut c, 502, &format!("[moocher] could not connect to {host}:{port}")).await;
        };
        tracing::debug!(session = %self.session, host, port, "egress allowed");
        let rest = &buf[end..];
        let ok = match &target {
            Target::Connect { .. } => c.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").await.is_ok() && up.write_all(rest).await.is_ok(),
            Target::Http { head, .. } => up.write_all(head).await.is_ok() && up.write_all(rest).await.is_ok(),
        };
        if ok {
            let _ = tokio::io::copy_bidirectional(&mut c, &mut up).await;
        }
    }
}

async fn reply<S: AsyncWrite + Unpin>(c: &mut S, code: u16, msg: &str) {
    let reason = match code {
        400 => "Bad Request",
        403 => "Forbidden",
        431 => "Request Header Fields Too Large",
        _ => "Bad Gateway",
    };
    let body = format!("{msg}\n");
    let _ = c.write_all(format!("HTTP/1.1 {code} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heads() {
        assert_eq!(parse_head(b"CONNECT pypi.org:443 HTTP/1.1\r\nHost: pypi.org:443\r\n\r\n"), Ok(Target::Connect { host: "pypi.org".into(), port: 443 }));
        assert_eq!(parse_head(b"CONNECT [::1]:8443 HTTP/1.1\r\n\r\n"), Ok(Target::Connect { host: "::1".into(), port: 8443 }));
        assert!(parse_head(b"CONNECT pypi.org HTTP/1.1\r\n\r\n").is_err());
        let Ok(Target::Http { host, port, head }) = parse_head(b"GET http://u:p@Example.com:8080/a?b=1 HTTP/1.1\r\nHost: example.com\r\nProxy-Authorization: x\r\nConnection: keep-alive\r\n\r\n") else { panic!() };
        assert_eq!((host.as_str(), port), ("example.com", 8080));
        assert_eq!(String::from_utf8(head).unwrap(), "GET /a?b=1 HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n");
        assert!(parse_head(b"GET /local HTTP/1.1\r\n\r\n").is_err());
        assert!(parse_head(b"GET https://x/ HTTP/1.1\r\n\r\n").unwrap_err().contains("CONNECT"));
    }

    #[test]
    fn internal_addresses() {
        for a in ["127.0.0.1", "10.1.2.3", "192.168.0.1", "172.16.0.1", "169.254.169.254", "100.64.0.1", "0.0.0.0", "::1", "fd00::1", "fe80::1", "::ffff:127.0.0.1"] {
            assert!(is_internal(a.parse().unwrap()), "{a}");
        }
        for a in ["1.1.1.1", "151.101.0.223", "2606:4700::1111"] {
            assert!(!is_internal(a.parse().unwrap()), "{a}");
        }
    }

    #[tokio::test]
    async fn decisions() {
        let allow: Vec<HostPattern> = ["localhost", "127.0.0.1:9"].iter().map(|s| HostPattern::parse(s).unwrap()).collect();
        assert!(decide(&allow, "example.org", 443).await.unwrap_err().contains("not in"));
        assert!(decide(&allow, "localhost", 443).await.unwrap_err().contains("internal"), "a name may not lead to loopback");
        assert_eq!(decide(&allow, "127.0.0.1", 9).await.unwrap().len(), 1, "a listed address may");
    }
}
