//! Node side of the node<->supervisor stream.
//!
//! Durability: every telemetry event is written to the SQLite outbox first and
//! a single pump sends rows in seq order. On (re)connect the supervisor's
//! `Welcome{last_seq}` tells us where to resume, so supervisor downtime loses
//! nothing. Agent tool calls (MCP) are request/response and fail fast while
//! disconnected — an agent should see "cluster unreachable", not hang.

use crate::pki::Pki;
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
    /// The current gRPC channel (Tunnel RPCs share it).
    channel: Mutex<Option<tonic::transport::Channel>>,
    /// Latest tunnel setup time per relay node (link metrics).
    tunnel_setup: Mutex<HashMap<String, Duration>>,
    /// Source address the hub sees for our link (remote nodes).
    observed_ip: Mutex<Option<String>>,
    pub ip_changed: Notify,
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
                channel: Mutex::new(None),
                tunnel_setup: Mutex::new(HashMap::new()),
                observed_ip: Mutex::new(None),
                ip_changed: Notify::new(),
                pending: Mutex::new(HashMap::new()),
                next_req: AtomicU64::new(1),
                wake: Notify::new(),
                connected: AtomicBool::new(false),
            }),
        }
    }

    pub fn channel(&self) -> Option<tonic::transport::Channel> {
        if !self.inner.connected.load(Ordering::Relaxed) {
            return None;
        }
        self.inner.channel.lock().unwrap().clone()
    }

    pub fn observed_ip(&self) -> Option<String> {
        self.inner.observed_ip.lock().unwrap().clone()
    }

    /// Resolves when the hub reports a different source address for us.
    pub async fn ip_changed(&self) {
        self.inner.ip_changed.notified().await
    }

    pub fn note_tunnel(&self, relay: &str, setup: Duration) {
        self.inner.tunnel_setup.lock().unwrap().insert(relay.to_string(), setup);
    }

    #[allow(dead_code)]
    pub fn tunnel_setup(&self) -> HashMap<String, Duration> {
        self.inner.tunnel_setup.lock().unwrap().clone()
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
        let from_session = from_session.to_string();
        self.call(move |req| NodeMsg::Request { req, from_session, call }).await
    }

    /// Send a request-style message and await the supervisor's `Reply`.
    async fn call(&self, make: impl FnOnce(u64) -> NodeMsg) -> Result<Value> {
        let req = self.inner.next_req.fetch_add(1, Ordering::Relaxed);
        let (otx, orx) = oneshot::channel();
        self.inner.pending.lock().unwrap().insert(req, otx);
        let msg = make(req);
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

    /// Renew the link certificate if it is past two thirds of its lifetime.
    /// The new key and certificate are used from the next connection on.
    async fn renew_if_due(&self, pki: &Pki) -> Result<()> {
        if !pki.needs_renewal() {
            return Ok(());
        }
        let meta = pki.meta().ok_or_else(|| anyhow!("no certificate metadata"))?;
        let (key_pem, csr_pem) = crate::pki::new_key_and_csr()?;
        let v = self.call(move |req| NodeMsg::RenewCert { req, csr_pem }).await?;
        let cert = v["cert_pem"].as_str().ok_or_else(|| anyhow!("renewal reply without certificate"))?;
        let expires_at = v["expires_at"].as_i64().unwrap_or(0);
        pki.store(&key_pem, cert, None, &crate::pki::CertMeta { node: meta.node, issued_at: ocl_protocol::model::now_ms(), expires_at })?;
        tracing::info!(expires_at, "link certificate renewed");
        Ok(())
    }

    /// Connect forever with exponential backoff + full jitter.
    pub async fn run(self, supervisor: String, pki: Arc<Pki>, hello: HelloFn, handler: CommandHandler) {
        let mut backoff = 0.5f64;
        loop {
            // Waiting for approval: poll steadily so the node joins soon after.
            // Revoked/unknown identity: retrying quickly cannot help.
            let mut fixed = None;
            match self.session(&supervisor, &pki, &hello, &handler).await {
                Ok(()) => tracing::warn!("supervisor closed the link"),
                Err(e) => {
                    let msg = format!("{e:#}");
                    if msg.contains("awaiting operator approval") {
                        fixed = Some(10.0);
                    } else if msg.contains("revoked") || msg.contains("unknown client certificate") || msg.contains("expired") {
                        fixed = Some(600.0);
                    }
                    tracing::warn!(error = %msg, "supervisor link down");
                }
            }
            let was_connected = self.inner.connected.swap(false, Ordering::Relaxed);
            *self.inner.tx.lock().unwrap() = None;
            for (_, p) in self.inner.pending.lock().unwrap().drain() {
                let _ = p.send(Err("connection to supervisor lost".into()));
            }
            if was_connected {
                backoff = 0.5;
            }
            let wait = fixed.unwrap_or_else(|| (rand::random::<f64>() * backoff).max(0.2));
            tracing::info!(retry_in_ms = (wait * 1000.0) as u64, outbox = self.inner.store.outbox_depth(), "reconnecting");
            tokio::time::sleep(Duration::from_secs_f64(wait)).await;
            backoff = (backoff * 2.0).min(60.0);
        }
    }

    async fn session(&self, supervisor: &str, pki: &Arc<Pki>, hello: &HelloFn, handler: &CommandHandler) -> Result<()> {
        let channel = connect(supervisor, pki).await?;
        *self.inner.channel.lock().unwrap() = Some(channel.clone());
        let mut client = NodeLinkClient::new(channel);
        let (tx, rx) = mpsc::channel::<Envelope>(256);
        let h = hello().await;
        tx.send(Envelope { seq: 0, ack: 0, json: serde_json::to_vec(&NodeMsg::Hello(h))? }).await?;
        let mut inbound = client.attach(ReceiverStream::new(rx)).await.map_err(|s| anyhow!("supervisor refused the link: {}", s.message()))?.into_inner();

        // Expect Welcome first.
        let first = inbound.message().await?.ok_or_else(|| anyhow!("stream closed before welcome"))?;
        let SupMsg::Welcome { last_seq, observed_ip } = serde_json::from_slice(&first.json)? else {
            bail!("expected welcome");
        };
        let ip_changed = {
            let mut cur = self.inner.observed_ip.lock().unwrap();
            let changed = *cur != observed_ip;
            *cur = observed_ip;
            changed
        };
        if ip_changed {
            self.inner.ip_changed.notify_one();
        }
        self.inner.store.outbox_ack(last_seq)?;
        *self.inner.tx.lock().unwrap() = Some(tx.clone());
        self.inner.connected.store(true, Ordering::Relaxed);
        tracing::info!(last_seq, "connected to supervisor");
        // Remote nodes: keep the link certificate fresh (checked now and twice a day).
        let renew = if supervisor.starts_with("https://") {
            let me = self.clone();
            let pki = pki.clone();
            Some(tokio::spawn(async move {
                let mut iv = tokio::time::interval(Duration::from_secs(12 * 3600));
                loop {
                    iv.tick().await;
                    if let Err(e) = me.renew_if_due(&pki).await {
                        tracing::warn!(error = %e, "certificate renewal failed; will retry");
                    }
                }
            }))
        } else {
            None
        };

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
        if let Some(r) = renew {
            r.abort();
        }
        res
    }
}

async fn connect(supervisor: &str, pki: &Pki) -> Result<tonic::transport::Channel> {
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
        return crate::pki::tls_channel(supervisor, pki.client_config()?).await;
    }
    bail!("supervisor must be unix:///path/to/supervisor.sock (same machine) or https://host:port (remote)")
}
