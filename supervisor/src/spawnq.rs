//! Spawns waiting for room (`cluster_spawn_agent` with `if_busy: wait`).
//!
//! When every node that has the agent is full, or its provider allowance is
//! used up everywhere, the request is queued here instead of failing. Every
//! few seconds the queue is tried in order; a request that starts tells its
//! caller the new session id, one that times out (or whose caller ended) is
//! dropped with a message. The caller can cancel with `cluster_stop_session`
//! on the `q-…` id.

use crate::state::{Actor, Sup};
use anyhow::{anyhow, bail, Result};
use moocher_protocol::link::{AgentCall, IfBusy};
use moocher_protocol::model::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;

const DEFAULT_WAIT_S: u64 = 3600;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedSpawn {
    pub id: String,
    pub agent: String,
    /// The session that asked.
    pub from: String,
    pub call: AgentCall,
    pub created_at: Ts,
    pub until: Ts,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub last_error: Option<String>,
    /// Who goes first when room frees up.
    #[serde(default)]
    pub urgency: TaskLabel,
}

impl Sup {
    /// Why `agent` cannot start right now but could later (busy, or waiting
    /// for an allowance), or `None` when it is ready or can never start.
    pub fn agent_waits(&self, agent: &str) -> Option<String> {
        let a = self.availability().into_iter().find(|a| a["agent"] == agent)?;
        matches!(a["status"].as_str(), Some("busy" | "wait_allowance")).then(|| a["detail"].as_str().unwrap_or("").to_string())
    }

    pub fn queue_spawn(&self, from: &str, call: &AgentCall, wait_max_s: Option<u64>, urgency: TaskLabel) -> Result<Value> {
        let AgentCall::Spawn { agent, .. } = call else { bail!("only spawns can be queued") };
        let mut call = call.clone();
        if let AgentCall::Spawn { if_busy, .. } = &mut call {
            *if_busy = IfBusy::Fail;
        }
        let now = now_ms();
        let q = QueuedSpawn {
            id: format!("q-{}", hex::encode(rand::random::<[u8; 5]>())),
            agent: agent.clone(),
            from: from.into(),
            call,
            created_at: now,
            until: now + wait_max_s.unwrap_or(DEFAULT_WAIT_S).clamp(10, 7 * 86_400) as i64 * 1000,
            attempts: 0,
            last_error: None,
            urgency,
        };
        self.db.put_queued_spawn(&q.id, &serde_json::to_value(&q)?)?;
        let why = self.agent_waits(agent).unwrap_or_default();
        let ahead = self.queued_spawns().iter().filter(|x| x.agent == q.agent && x.id != q.id && x.urgency.level() >= q.urgency.level()).count();
        self.log("spawn.queued", None, Some(from), json!({"queue_id": q.id, "agent": agent, "why": why, "ahead": ahead}));
        Ok(json!({
            "queued": q.id,
            "why": why,
            "ahead_of_you": ahead,
            "note": format!("{agent} starts as soon as it can; you will get a message with its session id. Cancel with stop_session(\"{}\").", q.id),
        }))
    }

    pub fn queued_spawns(&self) -> Vec<QueuedSpawn> {
        let mut v: Vec<QueuedSpawn> = self.db.queued_spawns().into_iter().filter_map(|v| serde_json::from_value(v).ok()).collect();
        v.sort_by_key(|q| q.created_at);
        v
    }

    pub fn cancel_queued_spawn(&self, from: &str, id: &str) -> Result<()> {
        let q = self.queued_spawns().into_iter().find(|q| q.id == id).ok_or_else(|| anyhow!("no queued spawn {id}"))?;
        if q.from != from {
            bail!("you can only cancel spawns you queued");
        }
        self.db.del_queued_spawn(id)?;
        self.log("spawn.cancelled", None, Some(from), json!({"queue_id": id, "agent": q.agent}));
        Ok(())
    }

    /// Try the queue in order (called every few seconds).
    pub async fn retry_queued_spawns(self: &Arc<Self>) {
        let now = now_ms();
        let mut queue = self.queued_spawns();
        queue.sort_by_key(|q| (std::cmp::Reverse(q.urgency.level()), q.created_at));
        for mut q in queue {
            let caller = self.db.session(&q.from).filter(|s| s.report.state.is_live() || self.holds_messages(s));
            let Some(caller) = caller else {
                let _ = self.db.del_queued_spawn(&q.id);
                self.log("spawn.dropped", None, Some(&q.from), json!({"queue_id": q.id, "agent": q.agent, "why": "the session that asked has ended"}));
                continue;
            };
            if now > q.until {
                let _ = self.db.del_queued_spawn(&q.id);
                let msg = format!("[cluster] Gave up waiting to start {} (queued as {}): {}", q.agent, q.id, q.last_error.clone().unwrap_or_else(|| self.agent_waits(&q.agent).unwrap_or_else(|| "still not possible".into())));
                let _ = self.message(&Actor::System, &q.from, &msg, Delivery::Queue).await;
                self.log("spawn.expired", None, Some(&q.from), json!({"queue_id": q.id, "agent": q.agent}));
                continue;
            }
            // Its request waits while it is paused.
            if self.holds_messages(&caller) {
                continue;
            }
            // right_now goes ahead and makes room.
            if q.urgency != TaskLabel::RightNow && self.agent_waits(&q.agent).is_some() {
                continue;
            }
            match self.agent_call(&caller.report.node, &q.from, q.call.clone()).await {
                Ok(v) => {
                    let _ = self.db.del_queued_spawn(&q.id);
                    let sid = v["session"].as_str().unwrap_or("?").to_string();
                    let msg = format!(
                        "[cluster] {} (queued as {}) has started: session {sid} on {}. You will receive a message when it finishes a turn.",
                        q.agent,
                        q.id,
                        v["node"].as_str().unwrap_or("?")
                    );
                    let _ = self.message(&Actor::System, &q.from, &msg, Delivery::Queue).await;
                    self.log("spawn.dequeued", v["node"].as_str(), Some(&q.from), json!({"queue_id": q.id, "agent": q.agent, "session": sid, "waited_s": (now - q.created_at) / 1000}));
                }
                Err(e) => {
                    q.attempts += 1;
                    q.last_error = Some(format!("{e:#}"));
                    if let Ok(v) = serde_json::to_value(&q) {
                        let _ = self.db.put_queued_spawn(&q.id, &v);
                    }
                }
            }
        }
    }
}
