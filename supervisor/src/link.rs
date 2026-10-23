//! Supervisor side of the node link (gRPC `NodeLink.Attach`).

use crate::state::Sup;
use anyhow::{anyhow, bail, Result};
use ocl_protocol::link::*;
use ocl_protocol::model::*;
use ocl_protocol::proto::node_link_server::NodeLink;
use ocl_protocol::proto::Envelope;
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
}

#[derive(Default)]
pub struct Registry {
    conns: Mutex<HashMap<String, Arc<NodeConn>>>,
    next: AtomicU64,
}

impl Registry {
    pub fn connected(&self, node: &str) -> bool {
        self.conns.lock().unwrap().contains_key(node)
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

    async fn attach(&self, req: Request<Streaming<Envelope>>) -> Result<Response<OutStream>, Status> {
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
        // Phase 1: only same-machine nodes over a 0600 unix socket, so the
        // socket's file permissions are the authentication. Phase 2 binds the
        // node name to the mTLS client certificate here.
        let sup = self.sup.clone();
        let node = hello.node.clone();
        let (tx, rx) = mpsc::channel::<Result<Envelope, Status>>(256);
        let epoch = sup.nodes.next.fetch_add(1, Ordering::Relaxed) + 1;
        let conn = Arc::new(NodeConn { tx: tx.clone(), pending: Mutex::new(HashMap::new()), epoch });
        if let Some(old) = sup.nodes.conns.lock().unwrap().insert(node.clone(), conn.clone()) {
            tracing::warn!(node = %node, "replacing existing connection");
            drop(old);
        }
        let last_seq = sup.db.node_seq(&node);
        sup.on_hello(&hello);
        let welcome = serde_json::to_vec(&SupMsg::Welcome { last_seq }).unwrap();
        let _ = tx.send(Ok(Envelope { seq: 0, ack: 0, json: welcome })).await;
        tracing::info!(node = %node, last_seq, sessions = hello.sessions.len(), "node attached");

        tokio::spawn(async move {
            let mut acked = last_seq;
            let mut processed = last_seq;
            let mut ack_tick = tokio::time::interval(Duration::from_millis(500));
            loop {
                tokio::select! {
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
                            NodeMsg::Hello(_) => {}
                        }
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
    pub fn on_hello(&self, h: &Hello) {
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
        };
        let _ = self.db.put_node(&info);
        self.publish_node(info);
        let reported: std::collections::HashSet<&str> = h.sessions.iter().map(|s| s.id.as_str()).collect();
        for s in &h.sessions {
            self.merge_report(s.clone());
        }
        // Sessions we believed were on this node but it no longer has.
        for mut s in self.db.sessions_on(&h.node) {
            let missing = !reported.contains(s.report.id.as_str());
            if missing && (s.report.state.is_live() || s.report.state == SessionState::Unreachable) {
                s.report.state = SessionState::Lost;
                s.report.error = Some("node reconnected without this session".into());
                s.report.updated_at = now_ms();
                self.save_session(&s);
            }
        }
        let _ = self.db.log("node.attached", Some(&h.node), None, &serde_json::json!({"sessions": h.sessions.len(), "agents": h.agents.len()}));
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
