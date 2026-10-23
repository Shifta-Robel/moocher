//! Node side of the node<->supervisor stream.
//!
//! Durability: every telemetry event is written to the SQLite outbox first and
//! a single pump sends rows in seq order. On (re)connect the supervisor's
//! `Welcome{last_seq}` tells us where to resume, so supervisor downtime loses
//! nothing. Agent tool calls (MCP) are request/response and fail fast while
//! disconnected — an agent should see "cluster unreachable", not hang.

use crate::store::Store;
use anyhow::{anyhow, bail, Result};
use futures::future::BoxFuture;
use hyper_util::rt::TokioIo;
use ocl_protocol::link::*;
use ocl_protocol::proto::node_link_client::NodeLinkClient;
use ocl_protocol::proto::Envelope;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Notify};
use tokio_stream::wrappers::ReceiverStream;

pub type CommandHandler = Arc<dyn Fn(NodeCommand) -> BoxFuture<'static, Result<Value>> + Send + Sync>;
pub type HelloFn = Arc<dyn Fn() -> BoxFuture<'static, Hello> + Send + Sync>;

#[derive(Clone)]
pub struct LinkHandle {
    inner: Arc<Inner>,
}

struct Inner {
    store: Arc<Store>,
    tx: Mutex<Option<mpsc::Sender<Envelope>>>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    next_req: AtomicU64,
    wake: Notify,
    connected: AtomicBool,
}

impl LinkHandle {
    pub fn new(store: Arc<Store>) -> Self {
        LinkHandle {
            inner: Arc::new(Inner {
                store,
                tx: Mutex::new(None),
                pending: Mutex::new(HashMap::new()),
                next_req: AtomicU64::new(1),
                wake: Notify::new(),
                connected: AtomicBool::new(false),
            }),
        }
    }

    /// Queue a durable telemetry event.
    pub fn emit(&self, ev: NodeEvent) {
        if let Err(e) = self.inner.store.outbox_push(&ev) {
            tracing::error!(error = %e, "outbox write failed");
        }
        self.inner.wake.notify_one();
    }

    /// Agent tool call -> supervisor, awaiting its reply.
    pub async fn request(&self, from_session: &str, call: AgentCall) -> Result<Value> {
        let req = self.inner.next_req.fetch_add(1, Ordering::Relaxed);
        let (otx, orx) = oneshot::channel();
        self.inner.pending.lock().unwrap().insert(req, otx);
        let msg = NodeMsg::Request { req, from_session: from_session.into(), call };
        if self.send_now(&msg, 0).await.is_err() {
            self.inner.pending.lock().unwrap().remove(&req);
            bail!("supervisor unreachable (the node will reconnect automatically; try again shortly)");
        }
        match tokio::time::timeout(Duration::from_secs(60), orx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => Err(anyhow!(e)),
            Ok(Err(_)) => bail!("connection to supervisor lost"),
            Err(_) => {
                self.inner.pending.lock().unwrap().remove(&req);
                bail!("supervisor did not answer within 60s")
            }
        }
    }

    async fn send_now(&self, msg: &NodeMsg, seq: u64) -> Result<()> {
        let tx = self.inner.tx.lock().unwrap().clone().ok_or_else(|| anyhow!("not connected"))?;
        tx.send(Envelope { seq, ack: 0, json: serde_json::to_vec(msg)? }).await.map_err(|_| anyhow!("not connected"))
    }

    /// Connect forever with exponential backoff + full jitter.
    pub async fn run(self, supervisor: String, hello: HelloFn, handler: CommandHandler) {
        let mut backoff = 0.5f64;
        loop {
            match self.session(&supervisor, &hello, &handler).await {
                Ok(()) => tracing::warn!("supervisor closed the link"),
                Err(e) => tracing::warn!(error = %e, "supervisor link down"),
            }
            let was_connected = self.inner.connected.swap(false, Ordering::Relaxed);
            *self.inner.tx.lock().unwrap() = None;
            for (_, p) in self.inner.pending.lock().unwrap().drain() {
                let _ = p.send(Err("connection to supervisor lost".into()));
            }
            if was_connected {
                backoff = 0.5;
            }
            let wait = (rand::random::<f64>() * backoff).max(0.2);
            tracing::info!(retry_in_ms = (wait * 1000.0) as u64, outbox = self.inner.store.outbox_depth(), "reconnecting");
            tokio::time::sleep(Duration::from_secs_f64(wait)).await;
            backoff = (backoff * 2.0).min(60.0);
        }
    }

    async fn session(&self, supervisor: &str, hello: &HelloFn, handler: &CommandHandler) -> Result<()> {
        let channel = connect(supervisor).await?;
        let mut client = NodeLinkClient::new(channel);
        let (tx, rx) = mpsc::channel::<Envelope>(256);
        let h = hello().await;
        tx.send(Envelope { seq: 0, ack: 0, json: serde_json::to_vec(&NodeMsg::Hello(h))? }).await?;
        let mut inbound = client.attach(ReceiverStream::new(rx)).await?.into_inner();

        // Expect Welcome first.
        let first = inbound.message().await?.ok_or_else(|| anyhow!("stream closed before welcome"))?;
        let SupMsg::Welcome { last_seq } = serde_json::from_slice(&first.json)? else {
            bail!("expected welcome");
        };
        self.inner.store.outbox_ack(last_seq)?;
        *self.inner.tx.lock().unwrap() = Some(tx.clone());
        self.inner.connected.store(true, Ordering::Relaxed);
        tracing::info!(last_seq, "connected to supervisor");

        // Outbox pump: sends rows in order, waking on emit().
        let pump = {
            let me = self.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut sent = last_seq;
                loop {
                    let rows = me.inner.store.outbox_after(sent, 500).unwrap_or_default();
                    if rows.is_empty() {
                        tokio::select! {
                            _ = me.inner.wake.notified() => {}
                            _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                        }
                        continue;
                    }
                    for (seq, ev) in rows {
                        let Ok(json) = serde_json::to_vec(&NodeMsg::Event(ev)) else { continue };
                        if tx.send(Envelope { seq, ack: 0, json }).await.is_err() {
                            return;
                        }
                        sent = seq;
                    }
                }
            })
        };

        let res: Result<()> = async {
            while let Some(env) = inbound.message().await? {
                if env.ack > 0 {
                    let _ = self.inner.store.outbox_ack(env.ack);
                }
                if env.json.is_empty() {
                    continue;
                }
                match serde_json::from_slice::<SupMsg>(&env.json) {
                    Ok(SupMsg::Command { req, cmd }) => {
                        let handler = handler.clone();
                        let tx = tx.clone();
                        tokio::spawn(async move {
                            let reply = match handler(cmd).await {
                                Ok(value) => NodeMsg::Reply { req, ok: true, value, error: None },
                                Err(e) => NodeMsg::Reply { req, ok: false, value: Value::Null, error: Some(format!("{e:#}")) },
                            };
                            if let Ok(json) = serde_json::to_vec(&reply) {
                                let _ = tx.send(Envelope { seq: 0, ack: 0, json }).await;
                            }
                        });
                    }
                    Ok(SupMsg::Reply { req, ok, value, error }) => {
                        if let Some(p) = self.inner.pending.lock().unwrap().remove(&req) {
                            let _ = p.send(if ok { Ok(value) } else { Err(error.unwrap_or_else(|| "error".into())) });
                        }
                    }
                    Ok(SupMsg::Welcome { .. }) => {}
                    Err(e) => tracing::warn!(error = %e, "undecodable supervisor message"),
                }
            }
            Ok(())
        }
        .await;
        pump.abort();
        res
    }
}

async fn connect(supervisor: &str) -> Result<tonic::transport::Channel> {
    if let Some(path) = supervisor.strip_prefix("unix://") {
        let path = path.to_string();
        let ch = tonic::transport::Endpoint::try_from("http://supervisor.local")?
            .connect_with_connector(tower::service_fn(move |_: tonic::transport::Uri| {
                let path = path.clone();
                async move { Ok::<_, std::io::Error>(TokioIo::new(tokio::net::UnixStream::connect(path).await?)) }
            }))
            .await?;
        return Ok(ch);
    }
    if supervisor.starts_with("https://") {
        bail!("remote supervisors (mTLS) arrive in Phase 2; use unix:// for a same-machine supervisor");
    }
    bail!("supervisor must be unix:///path/to/supervisor.sock (Phase 1) or https://host:port (Phase 2)")
}
