//! Supervisor side of the node link (gRPC `NodeLink.Attach`).

use crate::nodes::Admission;
use crate::state::Sup;
use crate::tlslink::LinkPeer;
use anyhow::{anyhow, bail, Result};
use ocl_protocol::link::*;
use ocl_protocol::model::*;
use ocl_protocol::proto::node_link_server::NodeLink;
use ocl_protocol::proto::{EnrollReply, EnrollRequest, Envelope, TunnelFrame};
use serde_json::Value;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{Stream, StreamExt};
use tonic::{Request, Response, Status, Streaming};

pub struct NodeConn {
    tx: mpsc::Sender<Result<Envelope, Status>>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    /// Distinguishes this connection from a newer one for the same node.
    pub epoch: u64,
    /// Remote (TLS) connections: the peer address.
    pub peer: Option<std::net::IpAddr>,
    /// Set by `Registry::disconnect` to end the stream with a reason.
    close: tokio::sync::Notify,
    close_reason: Mutex<Option<String>>,
    /// Link round-trip time, moving average (ms).
    rtt: Mutex<Option<f64>>,
}

#[derive(Default)]
pub struct Registry {
    conns: Mutex<HashMap<String, Arc<NodeConn>>>,
    next: AtomicU64,
    /// `via_node` tunnels waiting for the relay to accept, by tunnel id.
    tunnels: Mutex<HashMap<String, PendingTunnel>>,
    /// Per relay: (tunnels, bytes out to the provider, bytes back).
    pub relay_stats: Mutex<HashMap<String, (u64, u64, u64)>>,
}

pub struct PendingTunnel {
    relay: String,
    inbound: Streaming<TunnelFrame>,
    tx: mpsc::Sender<Result<TunnelFrame, Status>>,
}

type FrameTx = mpsc::Sender<Result<TunnelFrame, Status>>;

/// Forward frames until the source ends; returns the payload bytes moved.
async fn splice(mut from: Streaming<TunnelFrame>, to: FrameTx) -> u64 {
    let mut n = 0u64;
    while let Ok(Some(f)) = from.message().await {
        n += f.data.len() as u64;
        if to.send(Ok(f)).await.is_err() {
            break;
        }
    }
    n
}

fn ready() -> TunnelFrame {
    TunnelFrame { header: b"ready".to_vec(), data: vec![], eof: false }
}

impl Registry {
    pub fn connected(&self, node: &str) -> bool {
        self.conns.lock().unwrap().contains_key(node)
    }

    /// Close a node's live connection (revocation, allowlist change).
    pub fn disconnect(&self, node: &str, reason: &str) {
        if let Some(c) = self.conns.lock().unwrap().get(node) {
            *c.close_reason.lock().unwrap() = Some(reason.to_string());
            c.close.notify_one();
        }
    }

    pub fn rtt(&self, node: &str) -> Option<f64> {
        self.conns.lock().unwrap().get(node).and_then(|c| *c.rtt.lock().unwrap())
    }

    /// Connected remote nodes and their peer addresses.
    pub fn remote_peers(&self) -> Vec<(String, std::net::IpAddr)> {
        self.conns.lock().unwrap().iter().filter_map(|(n, c)| c.peer.map(|ip| (n.clone(), ip))).collect()
    }

    /// Send a command to a node and wait for its reply.
    pub async fn call(&self, node: &str, cmd: NodeCommand, timeout: Duration) -> Result<Value> {
        let conn = self.conns.lock().unwrap().get(node).cloned().ok_or_else(|| anyhow!("node {node} is not connected"))?;
        let req = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = oneshot::channel();
        conn.pending.lock().unwrap().insert(req, tx);
        let json = serde_json::to_vec(&SupMsg::Command { req, cmd })?;
        conn.tx.send(Ok(Envelope { seq: 0, ack: 0, json })).await.map_err(|_| anyhow!("node {node} disconnected"))?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => bail!("{node}: {e}"),
            Ok(Err(_)) => bail!("node {node} disconnected"),
            Err(_) => {
                conn.pending.lock().unwrap().remove(&req);
                bail!("node {node} did not answer within {timeout:?}")
            }
        }
    }
}

pub struct LinkService {
    pub sup: Arc<Sup>,
}

type OutStream = Pin<Box<dyn Stream<Item = Result<Envelope, Status>> + Send>>;

#[tonic::async_trait]
impl NodeLink for LinkService {
    type AttachStream = OutStream;

    async fn enroll(&self, req: Request<EnrollRequest>) -> Result<Response<EnrollReply>, Status> {
        let peer = req.extensions().get::<LinkPeer>().cloned();
        let r = req.into_inner();
        match self.sup.enroll(peer.as_ref(), &r.token, &r.csr_pem).await {
            Ok(reply) => Ok(Response::new(reply)),
            Err(e) => Err(Status::permission_denied(format!("{e:#}"))),
        }
    }

    type TunnelStream = Pin<Box<dyn Stream<Item = Result<TunnelFrame, Status>> + Send>>;

    async fn tunnel(&self, req: Request<Streaming<TunnelFrame>>) -> Result<Response<Self::TunnelStream>, Status> {
        let peer = req.extensions().get::<LinkPeer>().cloned();
        let mut inbound = req.into_inner();
        let first = inbound.message().await?.ok_or_else(|| Status::invalid_argument("empty tunnel"))?;
        let hdr: Value = serde_json::from_slice(&first.header).map_err(|_| Status::invalid_argument("bad tunnel header"))?;
        let claimed = hdr["from"].as_str().unwrap_or_default().to_string();
        let open: TunnelOpen = serde_json::from_value(hdr).map_err(|e| Status::invalid_argument(format!("bad tunnel header: {e}")))?;
        // Remote callers are who their certificate says; unix-socket callers are local.
        let caller = match &peer {
            Some(p) => {
                let who = self.sup.tunnel_identity(p)?;
                if who != claimed {
                    return Err(Status::permission_denied("tunnel header does not match the client certificate"));
                }
                who
            }
            None => claimed,
        };
        if !self.sup.nodes.connected(&caller) {
            return Err(Status::permission_denied(format!("node {caller} is not attached")));
        }
        let sup = self.sup.clone();
        match open {
            TunnelOpen::Dial { relay, host, port } => {
                let relay_ok = sup.db.node(&relay).is_some_and(|n| n.relay && n.connected);
                if !relay_ok || !sup.nodes.connected(&relay) {
                    return Err(Status::unavailable(format!("relay node {relay} is not connected or does not relay")));
                }
                let id = ocl_protocol::new_id("t");
                let (tx, rx) = mpsc::channel::<Result<TunnelFrame, Status>>(64);
                sup.nodes.tunnels.lock().unwrap().insert(id.clone(), PendingTunnel { relay: relay.clone(), inbound, tx: tx.clone() });
                tokio::spawn(async move {
                    let cmd = NodeCommand::OpenTunnel { id: id.clone(), host: host.clone(), port, from: caller.clone() };
                    let res = sup.nodes.call(&relay, cmd, Duration::from_secs(20)).await;
                    if let Err(e) = res {
                        if sup.nodes.tunnels.lock().unwrap().remove(&id).is_some() {
                            tracing::info!(relay, host, port, from = %caller, error = %e, "tunnel refused");
                            let _ = tx.send(Err(Status::unavailable(format!("{e:#}")))).await;
                        }
                        return;
                    }
                    // The relay accepts right after connecting; give up if it never does.
                    tokio::time::sleep(Duration::from_secs(15)).await;
                    if sup.nodes.tunnels.lock().unwrap().remove(&id).is_some() {
                        let _ = tx.send(Err(Status::deadline_exceeded("relay did not accept the tunnel"))).await;
                    }
                });
                Ok(Response::new(Box::pin(ReceiverStream::new(rx)) as Self::TunnelStream))
            }
            TunnelOpen::Accept { id } => {
                let p = {
                    let mut t = sup.nodes.tunnels.lock().unwrap();
                    match t.get(&id) {
                        Some(p) if p.relay == caller => t.remove(&id),
                        Some(_) => return Err(Status::permission_denied("tunnel belongs to another relay")),
                        None => None,
                    }
                };
                let p = p.ok_or_else(|| Status::not_found("no such tunnel (timed out?)"))?;
                let (tx, rx) = mpsc::channel::<Result<TunnelFrame, Status>>(64);
                let _ = p.tx.send(Ok(ready())).await;
                let _ = tx.send(Ok(ready())).await;
                let relay = caller.clone();
                tokio::spawn(async move {
                    let (up, down) = tokio::join!(splice(p.inbound, tx), splice(inbound, p.tx));
                    let mut st = sup.nodes.relay_stats.lock().unwrap();
                    let e = st.entry(relay).or_default();
                    e.0 += 1;
                    e.1 += up;
                    e.2 += down;
                });
                Ok(Response::new(Box::pin(ReceiverStream::new(rx)) as Self::TunnelStream))
            }
        }
    }

    async fn attach(&self, req: Request<Streaming<Envelope>>) -> Result<Response<OutStream>, Status> {
        let peer = req.extensions().get::<LinkPeer>().cloned();
        let mut inbound = req.into_inner();
        let first = inbound.message().await?.ok_or_else(|| Status::invalid_argument("empty stream"))?;
        let hello = match serde_json::from_slice::<NodeMsg>(&first.json) {
            Ok(NodeMsg::Hello(h)) => h,
            _ => return Err(Status::invalid_argument("first message must be hello")),
        };
        if hello.protocol != PROTOCOL_VERSION {
            return Err(Status::failed_precondition(format!("protocol {} unsupported (want {PROTOCOL_VERSION})", hello.protocol)));
        }
        if !ocl_protocol::config::valid_name(&hello.node) {
            return Err(Status::invalid_argument("bad node name"));
        }
        // Same-machine nodes use the 0600 unix socket (file permissions are the
        // authentication); remote nodes must present an enrolled certificate.
        let admission = match &peer {
            Some(p) => self.sup.admit(p, &hello)?,
            None => Admission::local(&hello),
        };
        let sup = self.sup.clone();
        let node = hello.node.clone();
        let (tx, rx) = mpsc::channel::<Result<Envelope, Status>>(256);
        let epoch = sup.nodes.next.fetch_add(1, Ordering::Relaxed) + 1;
        let conn = Arc::new(NodeConn {
            tx: tx.clone(),
            pending: Mutex::new(HashMap::new()),
            epoch,
            peer: peer.as_ref().map(|p| p.ip()),
            close: tokio::sync::Notify::new(),
            close_reason: Mutex::new(None),
            rtt: Mutex::new(None),
        });
        if let Some(old) = sup.nodes.conns.lock().unwrap().insert(node.clone(), conn.clone()) {
            tracing::warn!(node = %node, "replacing existing connection");
            drop(old);
        }
        let last_seq = sup.db.node_seq(&node);
        sup.on_hello(&hello, &admission);
        let welcome = serde_json::to_vec(&SupMsg::Welcome { last_seq, observed_ip: admission.observed_ip.clone() }).unwrap();
        let _ = tx.send(Ok(Envelope { seq: 0, ack: 0, json: welcome })).await;
        tracing::info!(node = %node, last_seq, sessions = hello.sessions.len(), peer = ?admission.observed_ip, "node attached");
        {
            let sup = sup.clone();
            let node = node.clone();
            tokio::spawn(async move {
                let lib = sup.load_library();
                sup.sync_templates(&node, &lib).await;
            });
        }

        tokio::spawn(async move {
            let mut acked = last_seq;
            let mut processed = last_seq;
            let mut ack_tick = tokio::time::interval(Duration::from_millis(500));
            let mut ping_tick = tokio::time::interval(Duration::from_secs(15));
            loop {
                tokio::select! {
                    _ = ping_tick.tick() => {
                        let ping = serde_json::to_vec(&SupMsg::Ping { ts: now_ms() }).unwrap();
                        if tx.send(Ok(Envelope { seq: 0, ack: 0, json: ping })).await.is_err() { break; }
                    }
                    msg = inbound.next() => {
                        let Some(Ok(env)) = msg else { break };
                        let parsed: NodeMsg = match serde_json::from_slice(&env.json) {
                            Ok(m) => m,
                            Err(e) => { tracing::warn!(node = %node, error = %e, "bad node message"); continue; }
                        };
                        match parsed {
                            NodeMsg::Event(ev) => {
                                if env.seq != 0 && env.seq <= processed {
                                    continue; // replay of something we already have
                                }
                                sup.on_node_event(&node, ev).await;
                                if env.seq != 0 {
                                    processed = env.seq;
                                    let _ = sup.db.set_node_seq(&node, processed);
                                }
                            }
                            NodeMsg::Reply { req, ok, value, error } => {
                                if let Some(p) = conn.pending.lock().unwrap().remove(&req) {
                                    let _ = p.send(if ok { Ok(value) } else { Err(error.unwrap_or_default()) });
                                }
                            }
                            NodeMsg::Request { req, from_session, call } => {
                                let sup = sup.clone();
                                let tx = tx.clone();
                                let node = node.clone();
                                tokio::spawn(async move {
                                    let reply = match sup.agent_call(&node, &from_session, call).await {
                                        Ok(value) => SupMsg::Reply { req, ok: true, value, error: None },
                                        Err(e) => SupMsg::Reply { req, ok: false, value: Value::Null, error: Some(format!("{e:#}")) },
                                    };
                                    let _ = tx.send(Ok(Envelope { seq: 0, ack: 0, json: serde_json::to_vec(&reply).unwrap() })).await;
                                });
                            }
                            NodeMsg::RenewCert { req, csr_pem } => {
                                let reply = match sup.renew_cert(&node, &csr_pem) {
                                    Ok(value) => SupMsg::Reply { req, ok: true, value, error: None },
                                    Err(e) => SupMsg::Reply { req, ok: false, value: Value::Null, error: Some(format!("{e:#}")) },
                                };
                                let _ = tx.send(Ok(Envelope { seq: 0, ack: 0, json: serde_json::to_vec(&reply).unwrap() })).await;
                            }
                            NodeMsg::Stats { host, instances } => sup.on_stats(&node, host, instances),
                            NodeMsg::Pong { ts } => {
                                let ms = (now_ms() - ts) as f64;
                                let mut r = conn.rtt.lock().unwrap();
                                *r = Some(r.map_or(ms, |a| a * 0.7 + ms * 0.3));
                            }
                            NodeMsg::Hello(_) => {}
                        }
                    }
                    _ = conn.close.notified() => {
                        let why = conn.close_reason.lock().unwrap().clone().unwrap_or_else(|| "closed by supervisor".into());
                        tracing::warn!(node = %node, reason = %why, "closing node connection");
                        let _ = tx.send(Err(Status::permission_denied(why))).await;
                        break;
                    }
                    _ = ack_tick.tick() => {
                        if processed > acked {
                            if tx.send(Ok(Envelope { seq: 0, ack: processed, json: vec![] })).await.is_err() { break; }
                            acked = processed;
                        }
                    }
                }
            }
            // Only clean up if we are still the current connection for this node.
            let current = {
                let mut conns = sup.nodes.conns.lock().unwrap();
                if conns.get(&node).map(|c| c.epoch) == Some(epoch) {
                    conns.remove(&node);
                    true
                } else {
                    false
                }
            };
            for (_, p) in conn.pending.lock().unwrap().drain() {
                let _ = p.send(Err("node disconnected".into()));
            }
            if current {
                tracing::warn!(node = %node, "node detached");
                sup.on_node_gone(&node);
            }
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx)) as OutStream))
    }
}

impl Sup {
    /// Reconcile the supervisor's view with a node's full inventory.
    pub fn on_hello(self: &Arc<Self>, h: &Hello, adm: &Admission) {
        let info = NodeInfo {
            name: h.node.clone(),
            connected: true,
            version: h.version.clone(),
            max_sessions: h.max_sessions,
            running: h.sessions.iter().filter(|s| s.state.is_live()).count() as u32,
            relay: h.relay,
            agents: h.agents.clone(),
            last_seen: now_ms(),
            labels: h.labels.clone(),
            role: adm.role.as_str().into(),
            enrollment: adm.enrollment.clone(),
            observed_ip: adm.observed_ip.clone(),
            cert_expires_at: adm.cert_expires_at,
            link_rtt_ms: None,
        };
        let _ = self.db.put_node(&info);
        self.publish_node(info);
        let reported: std::collections::HashSet<&str> = h.sessions.iter().map(|s| s.id.as_str()).collect();
        let all = self.db.sessions(true);
        for s in &h.sessions {
            // A node that comes back after its session was replaced elsewhere
            // must not keep working on the same thing twice.
            if let Some(new) = all.iter().find(|n| n.replaces.as_deref() == Some(s.id.as_str())) {
                if s.state.is_live() {
                    tracing::warn!(session = s.id, replacement = new.report.id, "node came back with a replaced session; stopping it");
                    let me = self.clone();
                    let id = s.id.clone();
                    tokio::spawn(async move {
                        let _ = me.stop_with(&crate::state::Actor::System, &id, false, "replaced").await;
                    });
                }
                continue;
            }
            self.merge_report(s.clone());
        }
        // Sessions we believed were on this node but it no longer has.
        for mut s in self.db.sessions_on(&h.node) {
            let missing = !reported.contains(s.report.id.as_str());
            if missing && (s.report.state.is_live() || s.report.state == SessionState::Unreachable) {
                s.report.state = SessionState::Lost;
                s.report.dead_reason = Some("container_gone".into());
                s.report.error = Some("node reconnected without this session".into());
                s.report.updated_at = now_ms();
                self.save_session(&s);
                let me = self.clone();
                let id = s.report.id.clone();
                tokio::spawn(async move { me.on_died(&id, "container_gone", "the node reconnected without this session").await });
            }
        }
        let _ = self.db.log(
            "node.attached",
            Some(&h.node),
            None,
            &serde_json::json!({"sessions": h.sessions.len(), "agents": h.agents.len(), "role": adm.role.as_str(), "peer": adm.observed_ip}),
        );
    }

    pub fn on_node_gone(&self, node: &str) {
        if let Some(mut n) = self.db.node(node) {
            n.connected = false;
            n.last_seen = now_ms();
            let _ = self.db.put_node(&n);
            self.publish_node(n);
        }
        for mut s in self.db.sessions_on(node) {
            if s.report.state.is_live() {
                s.report.state = SessionState::Unreachable;
                s.report.updated_at = now_ms();
                self.save_session(&s);
            }
        }
        let _ = self.db.log("node.detached", Some(node), None, &serde_json::json!({}));
    }
}
