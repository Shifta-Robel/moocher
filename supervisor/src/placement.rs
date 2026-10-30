//! Where a new session runs (PHASE1_5 §5).
//!
//! Hard filters: the node is attached, has the agent with a resolvable
//! secret, and has a free slot. The rest is a weighted score, each part 0..1:
//! * capacity — free session slots;
//! * quota — provider allowance left on the node's routes for this agent
//!   (nodes where every route is used up are dropped while any other node
//!   still has headroom);
//! * latency — provider response time over the node's preferred route;
//! * locality — same node as the parent session / the shared workspace;
//! * link — hub <-> node round-trip time.
//!
//! Unknown measurements score as neutral, not as bad.

use crate::state::Sup;
use anyhow::{anyhow, bail, Result};
use moocher_protocol::api::QuotaView;
use moocher_protocol::model::*;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// What a new session should be close to.
#[derive(Debug, Clone, Default)]
pub struct Near {
    pub parent_node: Option<String>,
    /// Shared workspace name.
    pub workspace: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Scored {
    pub node: String,
    pub score: f64,
    pub parts: BTreeMap<&'static str, f64>,
    /// Every route's allowance for the agent's provider is used up here.
    pub exhausted: bool,
    pub notes: Vec<String>,
}

impl Scored {
    pub fn summary(&self) -> String {
        let parts: Vec<String> = self.parts.iter().map(|(k, v)| format!("{k} {v:.2}")).collect();
        let mut s = format!("{} {:.2} ({})", self.node, self.score, parts.join(", "));
        if !self.notes.is_empty() {
            s.push_str(&format!(" [{}]", self.notes.join("; ")));
        }
        s
    }
}

fn human_in(ms: i64) -> String {
    let s = (ms.max(0) + 999) / 1000;
    match s {
        s if s >= 3600 => format!("{}h {}m", s / 3600, s % 3600 / 60),
        s if s >= 60 => format!("{}m {}s", s / 60, s % 60),
        s => format!("{s}s"),
    }
}

impl Sup {
    /// Candidate nodes for `agent`, best first.
    pub fn rank(&self, agent: &str, near: &Near) -> Vec<Scored> {
        let w = &self.cfg.placement;
        let quotas: Vec<QuotaView> = self.quota_views();
        let stats = self.db.route_stats();
        let sessions = self.db.sessions(false);
        let egress = self.db.egress();
        let now = now_ms();
        let mut out: Vec<Scored> = vec![];
        for n in self.node_views().into_iter().filter(|n| n.connected && self.nodes.connected(&n.name)) {
            let Some(a) = n.agents.iter().find(|a| a.name == agent && a.secret_ok && a.errors.is_empty()) else { continue };
            if n.running >= n.max_sessions {
                continue;
            }
            let mut notes = vec![];
            let capacity = 1.0 - n.running as f64 / n.max_sessions.max(1) as f64;

            // Allowance on each of the agent's routes on this node.
            let routes: Vec<String> = if a.routes.is_empty() { vec!["direct".into()] } else { a.routes.clone() };
            let mut best: Option<f64> = None;
            let mut usable_route: Option<String> = None;
            let mut soonest: Option<Ts> = None;
            let mut all_exhausted = true;
            for r in &routes {
                let key = format!("{}/{r}", n.name);
                // A per-IP bucket applies to every route leaving from that IP,
                // whichever node used it up.
                let ip = egress.iter().find(|e| e.node == n.name && &e.route == r).and_then(|e| e.ip.clone());
                let bs: Vec<&QuotaView> = quotas
                    .iter()
                    .filter(|q| q.provider == a.provider && q.kind == "requests")
                    .filter(|q| q.routes.contains(&key) || (q.scope == "ip" && ip.as_deref() == Some(q.scope_id.as_str())))
                    .collect();
                if let Some(e) = bs.iter().find(|q| q.exhausted) {
                    soonest = e.resets_at.map(|t| soonest.map_or(t, |s: Ts| s.min(t)));
                    continue;
                }
                all_exhausted = false;
                usable_route.get_or_insert(r.clone());
                let h = bs.iter().filter_map(|q| Some(q.remaining? as f64 / q.limit?.max(1) as f64)).fold(None, |m: Option<f64>, x| Some(m.map_or(x, |m| m.min(x))));
                best = Some(best.map_or(h.unwrap_or(0.7), |b| b.max(h.unwrap_or(0.7))));
            }
            let quota = if all_exhausted {
                notes.push(format!(
                    "every route's {} allowance is used up{}",
                    a.provider,
                    soonest.map(|t| format!(" (resets in {})", human_in(t - now))).unwrap_or_default()
                ));
                0.0
            } else {
                best.unwrap_or(0.7)
            };

            // Provider latency over the route it would use first.
            let lat_route = usable_route.clone().unwrap_or_else(|| routes[0].clone());
            let latency = match stats.iter().find(|s| s.node == n.name && s.route == lat_route && s.provider == a.provider).and_then(|s| s.ttfb_ms) {
                Some(ms) => {
                    notes.push(format!("{} via {lat_route}: {:.0} ms", a.provider, ms));
                    1.0 / (1.0 + ms / 2000.0)
                }
                None => 0.6,
            };

            let locality = match (&near.parent_node, &near.workspace) {
                (None, None) => 0.5,
                (p, ws) => {
                    let here = p.as_deref() == Some(n.name.as_str())
                        || ws.as_ref().is_some_and(|w| sessions.iter().any(|s| s.report.node == n.name && s.report.workspace.as_deref() == Some(&format!("shared:{w}"))));
                    if here {
                        1.0
                    } else {
                        0.0
                    }
                }
            };
            let link = n.link_rtt_ms.map_or(0.6, |ms| 1.0 / (1.0 + ms / 200.0));
            let score = w.capacity * capacity + w.quota * quota + w.latency * latency + w.locality * locality + w.link * link;
            let parts = BTreeMap::from([("capacity", capacity), ("quota", quota), ("latency", latency), ("locality", locality), ("link", link)]);
            out.push(Scored { node: n.name.clone(), score, parts, exhausted: all_exhausted, notes });
        }
        // Nodes that can make progress now come first; within that, by score.
        out.sort_by(|a, b| a.exhausted.cmp(&b.exhausted).then(b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal)).then(a.node.cmp(&b.node)));
        out
    }

    /// Nodes that can run `agent` now, best first. Nodes whose allowance is used
    /// up are left out while any other node has some.
    pub fn candidates(&self, agent: &str) -> Vec<String> {
        let ranked = self.rank(agent, &Near::default());
        let any_usable = ranked.iter().any(|s| !s.exhausted);
        ranked.into_iter().filter(|s| !any_usable || !s.exhausted).map(|s| s.node).collect()
    }

    /// Why no node can take `agent`, node by node.
    pub fn why_not(&self, agent: &str) -> String {
        let nodes = self.node_views();
        let why: Vec<String> = nodes
            .iter()
            .filter(|n| !n.relay)
            .map(|n| {
                let reason = match n.agents.iter().find(|a| a.name == agent) {
                    _ if !n.connected || !self.nodes.connected(&n.name) => "not connected".to_string(),
                    None => "does not have this agent (check `moo agents` and the node's config errors)".into(),
                    Some(a) if !a.errors.is_empty() => format!("config errors: {}", a.errors.join("; ")),
                    Some(a) if !a.secret_ok => format!("the {} key does not resolve (node.yaml `secrets:` alias or its env var; restart the node after editing node.env)", a.provider),
                    Some(_) if n.running >= n.max_sessions => format!("at capacity ({}/{} sessions)", n.running, n.max_sessions),
                    Some(_) => "unavailable".into(),
                };
                format!("{}: {reason}", n.name)
            })
            .collect();
        if why.is_empty() {
            "no worker nodes are enrolled".into()
        } else {
            why.join("; ")
        }
    }

    /// Pick a node; returns it with a one-line explanation.
    pub fn place(&self, agent: &str, hint: Option<&str>, near: &Near) -> Result<(String, String)> {
        let ranked = self.rank(agent, near);
        let chosen = match hint {
            Some(h) => match ranked.iter().find(|s| s.node == h) {
                Some(s) => s.clone(),
                None => bail!("node {h} cannot run agent {agent} right now (not connected, agent missing/misconfigured, or at capacity)"),
            },
            None => ranked.first().cloned().ok_or_else(|| anyhow!("no connected node can run agent {agent:?} right now: {}", self.why_not(agent)))?,
        };
        let mut why = if hint.is_some() { format!("pinned to {}: {}", chosen.node, chosen.summary()) } else { chosen.summary() };
        if let Some(next) = ranked.iter().find(|s| s.node != chosen.node && !s.exhausted) {
            why.push_str(&format!("; next best {}", next.summary()));
        }
        let skipped: Vec<String> = ranked.iter().filter(|s| s.node != chosen.node && s.exhausted).map(|s| s.summary()).collect();
        if !skipped.is_empty() {
            why.push_str(&format!("; skipped {}", skipped.join(", ")));
        }
        let scores: Vec<Value> = ranked.iter().map(|s| json!({"node": s.node, "score": s.score, "parts": s.parts, "notes": s.notes})).collect();
        self.log("placement.decided", Some(&chosen.node), None, json!({"agent": agent, "node": chosen.node, "pinned": hint.is_some(), "candidates": scores}));
        Ok((chosen.node, why))
    }

    /// For the `cluster_list_nodes` tool: what a coordinator needs to choose a node.
    pub fn nodes_for_agents(&self) -> Value {
        let quotas = self.quota_views();
        let stats = self.db.route_stats();
        let egress = self.db.egress();
        let nodes: Vec<Value> = self
            .node_views()
            .into_iter()
            .filter(|n| n.connected)
            .map(|n| {
                let q: Vec<Value> = quotas
                    .iter()
                    .filter(|q| q.nodes.contains(&n.name) && q.kind == "requests")
                    .map(|q| json!({"provider": q.provider, "scope_id": q.scope_id, "remaining": q.remaining, "limit": q.limit, "exhausted": q.exhausted, "resets_in_s": q.resets_at.map(|t| ((t - now_ms()) / 1000).max(0))}))
                    .collect();
                let lat: Vec<Value> = stats
                    .iter()
                    .filter(|s| s.node == n.name)
                    .map(|s| json!({"route": s.route, "provider": s.provider, "ttfb_ms": s.ttfb_ms.map(|x| x.round()), "errors": s.errors}))
                    .collect();
                let ips: Vec<Value> = egress.iter().filter(|e| e.node == n.name).map(|e| json!({"route": e.route, "ip": e.ip})).collect();
                json!({
                    "node": n.name,
                    "role": n.role,
                    "free_slots": n.max_sessions.saturating_sub(n.running),
                    "agents": n.agents.iter().map(|a| a.name.clone()).collect::<Vec<_>>(),
                    "link_rtt_ms": n.link_rtt_ms.map(|x| x.round()),
                    "egress": ips,
                    "allowances": q,
                    "provider_latency": lat,
                })
            })
            .collect();
        Value::Array(nodes)
    }
}
