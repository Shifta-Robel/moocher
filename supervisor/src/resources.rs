//! Resource samples from nodes (PHASE1_5 §12): the latest per host and
//! instance, an hour of 10 s samples in memory, and one sample per minute in
//! SQLite for a week.

use crate::state::Sup;
use ocl_protocol::api::StreamEvent;
use ocl_protocol::model::*;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};

const RECENT: usize = 360; // 1 h of 10 s samples
const PERSIST_EVERY_MS: Ts = 60_000;
const KEEP_MS: Ts = 7 * 86_400_000;

#[derive(Default)]
pub struct StatsMem {
    hosts: HashMap<String, HostStats>,
    recent: HashMap<String, VecDeque<InstanceStats>>,
    persisted: HashMap<String, Ts>,
    pruned: Ts,
}

impl Sup {
    pub fn on_stats(&self, node: &str, host: HostStats, instances: Vec<InstanceStats>) {
        let now = now_ms();
        // A node may only report its own sessions.
        let mine: std::collections::HashSet<String> = self.db.sessions_on(node).into_iter().map(|s| s.report.id).collect();
        let instances: Vec<InstanceStats> = instances.into_iter().filter(|s| s.node == node && mine.contains(&s.session)).collect();
        let host = HostStats { node: node.to_string(), ..host };
        let mut to_store = vec![];
        let prune = {
            let mut m = self.stats.lock().unwrap();
            m.hosts.insert(node.to_string(), host);
            for s in instances {
                if m.persisted.get(&s.session).is_none_or(|t| now - t >= PERSIST_EVERY_MS) {
                    m.persisted.insert(s.session.clone(), now);
                    to_store.push(s.clone());
                }
                let q = m.recent.entry(s.session.clone()).or_default();
                q.push_back(s);
                while q.len() > RECENT {
                    q.pop_front();
                }
            }
            let due = now - m.pruned > 3_600_000;
            if due {
                m.pruned = now;
                // Forget in-memory history of sessions that ended long ago.
                m.recent.retain(|_, q| q.back().is_some_and(|s| now - s.ts < 3_600_000));
                m.persisted.retain(|_, t| now - *t < 3_600_000);
            }
            due
        };
        for s in &to_store {
            let _ = self.db.add_instance_stats(s);
        }
        if prune {
            let _ = self.db.prune_instance_stats(now - KEEP_MS);
        }
        let _ = self.stream.send(StreamEvent::Stats { node: node.into() });
    }

    /// Latest host and live-instance samples.
    pub fn stats_view(&self) -> Value {
        let live: std::collections::HashSet<String> = self.db.sessions(false).into_iter().filter(|s| s.report.state.is_live()).map(|s| s.report.id).collect();
        let m = self.stats.lock().unwrap();
        let mut hosts: Vec<&HostStats> = m.hosts.values().collect();
        hosts.sort_by(|a, b| a.node.cmp(&b.node));
        let mut inst: Vec<&InstanceStats> = m.recent.iter().filter(|(id, _)| live.contains(*id)).filter_map(|(_, q)| q.back()).collect();
        inst.sort_by(|a, b| (&a.node, &a.session).cmp(&(&b.node, &b.session)));
        json!({"hosts": hosts, "instances": inst})
    }

    /// One session: latest, the last hour (10 s) and the last day (per minute).
    pub fn session_stats(&self, id: &str) -> Value {
        let recent: Vec<InstanceStats> = self.stats.lock().unwrap().recent.get(id).map(|q| q.iter().cloned().collect()).unwrap_or_default();
        let minutes = self.db.instance_stats(id, now_ms() - 86_400_000);
        json!({"session": id, "latest": recent.last(), "recent": recent, "minutes": minutes})
    }
}
