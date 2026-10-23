//! Remote node enrollment and admission (ARCHITECTURE D5, PHASE1_5 §4).
//!
//! 1. `invite` (admin) creates a single-use join token bound to a node name and
//!    role. The token embeds the CA fingerprint, so the new machine can verify
//!    the hub before trusting it.
//! 2. `enroll` (no client cert yet) checks the token and the source allowlist,
//!    signs the node's CSR and records the certificate fingerprint.
//! 3. The node stays `pending` until the operator approves it (default).
//! 4. Every `Attach` over TLS is admitted by `admit`: known, unrevoked
//!    certificate; cert name == claimed name; approved; allowed source.
//! 5. `revoke` flags the certificates and drops the live connection.

use crate::state::{Actor, Sup};
use crate::tlslink::LinkPeer;
use anyhow::{anyhow, bail, Result};
use ocl_protocol::config::{valid_name, AllowlistScope, NodeRole};
use ocl_protocol::link::Hello;
use ocl_protocol::model::*;
use ocl_protocol::proto::EnrollReply;
use serde_json::{json, Value};
use std::net::IpAddr;
use std::str::FromStr;

/// What the hub knows about a connecting node, merged into its `NodeInfo`.
#[derive(Debug, Clone)]
pub struct Admission {
    pub enrollment: String,
    pub role: NodeRole,
    pub observed_ip: Option<String>,
    pub cert_expires_at: Option<Ts>,
}

impl Admission {
    pub fn local(h: &Hello) -> Admission {
        Admission { enrollment: "local".into(), role: h.role, observed_ip: None, cert_expires_at: None }
    }
}

/// Parse `10.0.0.0/8`, `203.0.113.7` or `2001:db8::/32`.
pub fn parse_source(s: &str) -> Result<ipnet::IpNet> {
    let s = s.trim();
    if let Ok(n) = ipnet::IpNet::from_str(s) {
        return Ok(n.trunc());
    }
    let ip = IpAddr::from_str(s).map_err(|_| anyhow!("{s:?} is not an IP address or CIDR"))?;
    Ok(ipnet::IpNet::from(ip))
}

impl Sup {
    fn ca(&self) -> Result<&crate::pki::Ca> {
        self.ca.get().ok_or_else(|| anyhow!("remote nodes are disabled: set link_listen in supervisor.yaml"))
    }

    /// The URL remote nodes use, for the join command.
    pub fn link_url(&self) -> Option<String> {
        let listen = self.cfg.link_listen.as_ref()?;
        let port = listen.rsplit_once(':').map(|(_, p)| p).unwrap_or("7443");
        let names = if self.cfg.public_names.is_empty() { crate::pki::default_names() } else { self.cfg.public_names.clone() };
        let host = names.first().cloned().unwrap_or_else(|| "localhost".into());
        let host = if host.contains(':') { format!("[{host}]") } else { host };
        Some(format!("https://{host}:{port}"))
    }

    pub fn invite(&self, actor: &Actor, node: &str, role: NodeRole, labels: Value) -> Result<Value> {
        let res = (|| {
            if !valid_name(node) {
                bail!("node name {node:?} must match [a-z0-9-]{{1,48}}");
            }
            if self.nodes.connected(node) {
                bail!("node {node} is connected; revoke it first to re-enroll that name");
            }
            let ca = self.ca()?;
            let ttl = self.cfg.enrollment.token_ttl_secs as i64 * 1000;
            let (secret, expires_at) = self.db.create_join_token(node, role.as_str(), &labels, &actor.to_string(), ttl)?;
            let token = ocl_protocol::tls::JoinToken { secret, ca_fingerprint: ca.fingerprint() }.render();
            let url = self.link_url().unwrap_or_default();
            Ok(json!({
                "node": node,
                "role": role.as_str(),
                "token": token,
                "expires_at": expires_at,
                "command": format!("opencode-node join {url} --token {token}"),
                "approval_required": self.cfg.enrollment.require_approval,
            }))
        })();
        self.audit(actor, "node.invite", node, json!({"role": role.as_str(), "error": res.as_ref().err().map(|e| e.to_string())}), res.is_ok());
        res
    }

    /// Whether `ip` may enroll/connect as a node of `role`.
    pub fn source_allowed(&self, ip: IpAddr, role: NodeRole) -> bool {
        let applies = match self.cfg.enrollment.enforce_for {
            AllowlistScope::None => false,
            AllowlistScope::All => true,
            AllowlistScope::Relay => role == NodeRole::Relay,
        };
        if !applies {
            return true;
        }
        let ip = ip.to_canonical();
        self.db.allowed_sources().iter().filter_map(|(c, ..)| parse_source(c).ok()).any(|n| n.contains(&ip))
    }

    pub async fn enroll(&self, peer: Option<&LinkPeer>, secret: &str, csr_pem: &str) -> Result<EnrollReply> {
        let peer = peer.ok_or_else(|| anyhow!("enrollment is only possible over the TLS link listener"))?;
        let res: Result<EnrollReply> = async {
            let (node, role, labels) = match self.db.use_join_token(secret) {
                Ok(x) => x,
                Err(e) => {
                    // Slow down guessing; tokens are 128-bit anyway.
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    return Err(e);
                }
            };
            let role_e = NodeRole::parse(&role).unwrap_or_default();
            if !self.source_allowed(peer.ip(), role_e) {
                bail!("source address {} is not in the allowed sources list", peer.ip());
            }
            let ca = self.ca()?;
            let issued = ca.issue_node(&node, csr_pem, self.cfg.enrollment.cert_days)?;
            self.db.add_node_cert(&node, &issued.fingerprint, &issued.serial, issued.expires_at)?;
            // Re-enrolling a name rotates its identity: older certificates stop working.
            self.db.revoke_node_certs(&node, Some(&issued.fingerprint))?;
            let status = if self.cfg.enrollment.require_approval { "pending" } else { "approved" };
            self.db.set_node_auth(&node, status, &role, &labels)?;
            self.ensure_node_row(&node, role_e, status, Some(peer.ip().to_string()), Some(issued.expires_at));
            Ok(EnrollReply {
                node,
                role,
                cert_pem: issued.cert_pem,
                ca_pem: ca.cert_pem.clone(),
                expires_at: issued.expires_at,
                status: status.into(),
            })
        }
        .await;
        let target = res.as_ref().map(|r| r.node.clone()).unwrap_or_else(|_| "-".into());
        self.audit(
            &Actor::System,
            "node.enroll",
            &target,
            json!({"source": peer.ip().to_string(), "status": res.as_ref().ok().map(|r| r.status.clone()), "error": res.as_ref().err().map(|e| e.to_string())}),
            res.is_ok(),
        );
        if res.is_ok() {
            self.log("node.enrolled", Some(&target), None, json!({"source": peer.ip().to_string()}));
        }
        res
    }

    /// Make an enrolled-but-never-connected node visible (e.g. `pending`).
    fn ensure_node_row(&self, node: &str, role: NodeRole, status: &str, ip: Option<String>, cert_expires_at: Option<Ts>) {
        let mut n = self.db.node(node).unwrap_or(NodeInfo {
            name: node.into(),
            connected: false,
            version: String::new(),
            max_sessions: 0,
            running: 0,
            relay: role == NodeRole::Relay,
            agents: vec![],
            last_seen: now_ms(),
            labels: Default::default(),
            role: role.as_str().into(),
            enrollment: status.into(),
            observed_ip: None,
            cert_expires_at: None,
        });
        n.enrollment = status.into();
        n.role = role.as_str().into();
        if ip.is_some() {
            n.observed_ip = ip;
        }
        if cert_expires_at.is_some() {
            n.cert_expires_at = cert_expires_at;
        }
        let _ = self.db.put_node(&n);
        self.publish_node(n);
    }

    /// Admission check for an `Attach` arriving over TLS.
    pub fn admit(&self, peer: &LinkPeer, hello: &Hello) -> Result<Admission, tonic::Status> {
        use tonic::Status;
        let fp = peer.cert.as_deref().ok_or_else(|| Status::unauthenticated("a client certificate is required (enroll with `opencode-node join`)"))?;
        let (cert_node, revoked, expires) = self.db.node_cert(fp).ok_or_else(|| Status::unauthenticated("unknown client certificate"))?;
        let deny = |why: String| {
            let key = format!("{}|{why}", hello.node);
            let first = {
                let mut m = self.rejections.lock().unwrap();
                let fresh = m.get(&key).is_none_or(|t| now_ms() - t > 600_000);
                if fresh {
                    m.insert(key, now_ms());
                }
                fresh
            };
            if first {
                tracing::warn!(node = %hello.node, peer = %peer.addr, reason = %why, "node rejected");
                self.audit(&Actor::System, "node.rejected", &hello.node, json!({"source": peer.ip().to_string(), "reason": why}), false);
            }
            Status::permission_denied(why)
        };
        if revoked {
            return Err(deny(format!("certificate for {cert_node} has been revoked")));
        }
        if expires < now_ms() {
            return Err(deny(format!("certificate for {cert_node} expired; re-enroll the node")));
        }
        if cert_node != hello.node {
            return Err(deny(format!("certificate belongs to {cert_node}, not {}", hello.node)));
        }
        let (status, role, _) = self.db.node_auth(&cert_node).ok_or_else(|| deny("node is not enrolled".into()))?;
        let enrolled_role = NodeRole::parse(&role).unwrap_or_default();
        match status.as_str() {
            "approved" => {}
            "pending" => return Err(Status::permission_denied(format!("node {cert_node} is awaiting operator approval (`ocl node approve {cert_node}`)"))),
            other => return Err(deny(format!("node {cert_node} is {other}"))),
        }
        // A node cannot dodge the relay allowlist by changing its own role.
        for r in [enrolled_role, hello.role] {
            if !self.source_allowed(peer.ip(), r) {
                return Err(deny(format!("source address {} is not in the allowed sources list", peer.ip())));
            }
        }
        Ok(Admission { enrollment: status, role: hello.role, observed_ip: Some(peer.ip().to_string()), cert_expires_at: Some(expires) })
    }

    /// Who opened a Tunnel RPC over TLS (same checks as `admit`, minus hello).
    pub fn tunnel_identity(&self, peer: &LinkPeer) -> Result<String, tonic::Status> {
        use tonic::Status;
        let fp = peer.cert.as_deref().ok_or_else(|| Status::unauthenticated("a client certificate is required"))?;
        let (node, revoked, expires) = self.db.node_cert(fp).ok_or_else(|| Status::unauthenticated("unknown client certificate"))?;
        if revoked || expires < now_ms() {
            return Err(Status::permission_denied("certificate revoked or expired"));
        }
        match self.db.node_auth(&node) {
            Some((s, ..)) if s == "approved" => Ok(node),
            _ => Err(Status::permission_denied(format!("node {node} is not approved"))),
        }
    }

    pub fn renew_cert(&self, node: &str, csr_pem: &str) -> Result<Value> {
        let ca = self.ca()?;
        let issued = ca.issue_node(node, csr_pem, self.cfg.enrollment.cert_days)?;
        self.db.add_node_cert(node, &issued.fingerprint, &issued.serial, issued.expires_at)?;
        if let Some(mut n) = self.db.node(node) {
            n.cert_expires_at = Some(issued.expires_at);
            let _ = self.db.put_node(&n);
        }
        self.audit(&Actor::System, "node.cert_renewed", node, json!({"expires_at": issued.expires_at}), true);
        Ok(json!({"cert_pem": issued.cert_pem, "expires_at": issued.expires_at}))
    }

    pub fn approve_node(&self, actor: &Actor, node: &str) -> Result<()> {
        let res = (|| {
            let (status, role, labels) = self.db.node_auth(node).ok_or_else(|| anyhow!("no such node {node} (not enrolled)"))?;
            if status == "revoked" {
                bail!("node {node} was revoked; invite it again to re-enroll");
            }
            self.db.set_node_auth(node, "approved", &role, &labels)?;
            self.ensure_node_row(node, NodeRole::parse(&role).unwrap_or_default(), "approved", None, None);
            Ok(())
        })();
        self.audit(actor, "node.approve", node, json!({"error": res.as_ref().err().map(|e| e.to_string())}), res.is_ok());
        res
    }

    pub fn revoke_node(&self, actor: &Actor, node: &str) -> Result<usize> {
        let res: Result<usize> = (|| {
            let (_, role, labels) = self.db.node_auth(node).ok_or_else(|| anyhow!("no such node {node} (not enrolled)"))?;
            let n = self.db.revoke_node_certs(node, None)?;
            self.db.set_node_auth(node, "revoked", &role, &labels)?;
            self.nodes.disconnect(node, "node revoked by operator");
            self.ensure_node_row(node, NodeRole::parse(&role).unwrap_or_default(), "revoked", None, None);
            Ok(n)
        })();
        self.audit(actor, "node.revoke", node, json!({"certs": res.as_ref().ok(), "error": res.as_ref().err().map(|e| e.to_string())}), res.is_ok());
        res
    }

    /// Remove a disconnected node from the registry (its session history stays).
    pub fn forget_node(&self, actor: &Actor, node: &str) -> Result<()> {
        let res = (|| {
            if self.nodes.connected(node) {
                bail!("node {node} is connected");
            }
            self.db.delete_node(node)?;
            Ok(())
        })();
        self.audit(actor, "node.forget", node, json!({}), res.is_ok());
        res
    }

    // ----------------------------------------------------- allowed sources

    pub fn allowlist(&self) -> Value {
        let entries: Vec<Value> = self
            .db
            .allowed_sources()
            .into_iter()
            .map(|(cidr, note, by, at)| json!({"cidr": cidr, "note": note, "added_by": by, "added_at": at}))
            .collect();
        json!({"enforce_for": self.cfg.enrollment.enforce_for, "entries": entries})
    }

    pub fn allowlist_add(&self, actor: &Actor, source: &str, note: &str) -> Result<String> {
        let res = parse_source(source).and_then(|n: ipnet::IpNet| {
            self.db.add_allowed_source(&n.to_string(), note, &actor.to_string())?;
            Ok(n.to_string())
        });
        self.audit(actor, "allowlist.add", source, json!({"note": note, "error": res.as_ref().err().map(|e| e.to_string())}), res.is_ok());
        res
    }

    pub fn allowlist_remove(&self, actor: &Actor, source: &str) -> Result<Vec<String>> {
        let res = (|| {
            let cidr = parse_source(source)?.to_string();
            if !self.db.remove_allowed_source(&cidr)? {
                bail!("{cidr} is not in the list");
            }
            // Connected nodes that are no longer allowed are disconnected now.
            let mut dropped = vec![];
            for (node, ip) in self.nodes.remote_peers() {
                let role = self.db.node(&node).and_then(|n| NodeRole::parse(&n.role)).unwrap_or_default();
                if !self.source_allowed(ip, role) {
                    self.nodes.disconnect(&node, "source address removed from the allowed list");
                    dropped.push(node);
                }
            }
            Ok(dropped)
        })();
        self.audit(actor, "allowlist.remove", source, json!({"disconnected": res.as_ref().ok(), "error": res.as_ref().err().map(|e| e.to_string())}), res.is_ok());
        res
    }

    /// First start: copy `enrollment.allowed_sources` from supervisor.yaml.
    pub fn seed_allowlist(&self) {
        if self.db.meta("allowlist_seeded").is_some() {
            return;
        }
        for s in &self.cfg.enrollment.allowed_sources {
            match parse_source(s) {
                Ok(n) => {
                    let _ = self.db.add_allowed_source(&n.to_string(), "from supervisor.yaml", "config");
                }
                Err(e) => tracing::warn!(entry = %s, error = %e, "ignoring bad allowed_sources entry"),
            }
        }
        let _ = self.db.set_meta("allowlist_seeded", "1");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources() {
        assert!(parse_source("10.1.2.3/8").unwrap().contains(&"10.200.0.1".parse::<IpAddr>().unwrap()));
        assert_eq!(parse_source("203.0.113.7").unwrap().to_string(), "203.0.113.7/32");
        assert!(parse_source("::1").unwrap().contains(&"::1".parse::<IpAddr>().unwrap()));
        assert!(parse_source("bogus").is_err());
    }
}
