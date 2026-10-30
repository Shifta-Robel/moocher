//! Provider allowances and egress IPs across the cluster (PHASE1_5 §3).
//!
//! Nodes report full snapshots of their buckets and egress IPs; the
//! supervisor merges buckets with the same provider + scope id (two machines
//! behind one NAT share a keyless allowance) and pushes operator overrides
//! back to the nodes, which keep them in their overrides.yaml.

use crate::state::{Actor, Sup, CMD_TIMEOUT};
use anyhow::{bail, Result};
use moocher_protocol::api::*;
use moocher_protocol::config::QuotaOverride;
use moocher_protocol::link::NodeCommand;
use moocher_protocol::model::*;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

impl Sup {
    pub fn quota_views(&self) -> Vec<QuotaView> {
        let now = now_ms();
        let mut groups: BTreeMap<(String, String, String), Vec<QuotaBucket>> = BTreeMap::new();
        for b in self.db.quotas() {
            groups.entry((b.provider.clone(), b.scope_id.clone(), b.kind.clone())).or_default().push(b);
        }
        let live: BTreeSet<String> = self.db.sessions(false).into_iter().filter(|s| s.report.state.is_live()).map(|s| s.report.id).collect();
        groups
            .into_values()
            .map(|bs| {
                let latest = bs.iter().max_by_key(|b| b.updated_at).cloned().unwrap();
                let exhausted: Vec<&QuotaBucket> = bs.iter().filter(|b| b.exhausted && b.resets_at.is_none_or(|r| r > now)).collect();
                let used: u64 = bs.iter().map(|b| b.used).sum();
                let limit = bs.iter().filter_map(|b| b.limit).max();
                // Counted buckets share the allowance: what is left is the limit minus everyone's use.
                let remaining = if latest.source == "config" || latest.source == "counted" {
                    limit.map(|l| l.saturating_sub(used))
                } else {
                    latest.remaining
                };
                let resets_at = if exhausted.is_empty() { latest.resets_at } else { exhausted.iter().filter_map(|b| b.resets_at).max() };
                QuotaView {
                    provider: latest.provider.clone(),
                    scope: latest.scope.clone(),
                    scope_id: latest.scope_id.clone(),
                    kind: latest.kind.clone(),
                    limit,
                    used,
                    remaining: if exhausted.is_empty() { remaining } else { Some(0) },
                    resets_at,
                    exhausted: !exhausted.is_empty() || (remaining == Some(0) && resets_at.is_some_and(|r| r > now)),
                    source: exhausted.first().map(|b| b.source.clone()).unwrap_or(latest.source.clone()),
                    window: latest.window.clone(),
                    nodes: bs.iter().map(|b| b.node.clone()).collect::<BTreeSet<_>>().into_iter().collect(),
                    routes: bs.iter().flat_map(|b| b.routes.iter().map(move |r| format!("{}/{r}", b.node))).collect::<BTreeSet<_>>().into_iter().collect(),
                    sessions: bs.iter().flat_map(|b| b.sessions.iter().cloned()).filter(|s| live.contains(s)).collect::<BTreeSet<_>>().into_iter().collect(),
                    updated_at: latest.updated_at,
                }
            })
            .collect()
    }

    pub fn egress_view(&self) -> EgressView {
        let ips = self.db.egress();
        let quotas = self.quota_views();
        let mut by_ip: BTreeMap<String, SharedIp> = BTreeMap::new();
        for e in &ips {
            let Some(ip) = &e.ip else { continue };
            let s = by_ip.entry(ip.clone()).or_insert_with(|| SharedIp { ip: ip.clone(), nodes: vec![], routes: vec![], sessions: vec![], providers: vec![] });
            if !s.nodes.contains(&e.node) {
                s.nodes.push(e.node.clone());
            }
            s.routes.push(format!("{}/{}", e.node, e.route));
        }
        for q in quotas.iter().filter(|q| q.scope == "ip") {
            if let Some(s) = by_ip.get_mut(&q.scope_id) {
                if !s.providers.contains(&q.provider) {
                    s.providers.push(q.provider.clone());
                }
                for sid in &q.sessions {
                    if !s.sessions.contains(sid) {
                        s.sessions.push(sid.clone());
                    }
                }
            }
        }
        EgressView { ips, shared: by_ip.into_values().collect() }
    }

    pub async fn set_quota_override(&self, actor: &Actor, r: QuotaOverrideRequest) -> Result<Value> {
        let res: Result<Value> = async {
            let targets: Vec<String> = match &r.node {
                Some(n) => vec![n.clone()],
                None => {
                    let reporting: BTreeSet<String> = self
                        .db
                        .quotas()
                        .into_iter()
                        .filter(|b| b.provider == r.provider && r.scope_id.as_ref().is_none_or(|s| *s == b.scope_id))
                        .map(|b| b.node)
                        .collect();
                    if reporting.is_empty() {
                        bail!("no node reports a {} bucket{}; pass a node", r.provider, r.scope_id.as_deref().map(|s| format!(" for {s}")).unwrap_or_default());
                    }
                    reporting.into_iter().collect()
                }
            };
            if !r.clear && r.resets_at.is_none() && r.remaining.is_none() {
                bail!("set resets_at and/or remaining (or clear)");
            }
            let ov = QuotaOverride {
                provider: r.provider.clone(),
                scope_id: r.scope_id.clone(),
                resets_at: r.resets_at,
                remaining: r.remaining,
                note: r.note.clone(),
                set_by: Some(actor.to_string()),
            };
            let mut done = vec![];
            for n in &targets {
                self.nodes.call(n, NodeCommand::SetQuota { ov: ov.clone(), clear: r.clear }, CMD_TIMEOUT).await?;
                done.push(n.clone());
            }
            Ok(json!({"nodes": done}))
        }
        .await;
        self.audit(actor, if r.clear { "quota.clear" } else { "quota.override" }, &r.provider, json!({"request": r, "error": res.as_ref().err().map(|e| format!("{e:#}"))}), res.is_ok());
        res
    }

    pub async fn set_route_override(&self, actor: &Actor, node: &str, r: RouteOverrideRequest) -> Result<()> {
        let res = self
            .nodes
            .call(node, NodeCommand::SetRoute { agent: r.agent.clone(), route: r.route.clone(), by: actor.to_string() }, CMD_TIMEOUT)
            .await
            .map(|_| ());
        self.audit(actor, "route.override", &format!("{node}/{}", r.agent), json!({"route": r.route, "error": res.as_ref().err().map(|e| format!("{e:#}"))}), res.is_ok());
        res
    }

    pub async fn probe_egress(&self, actor: &Actor, node: &str) -> Result<Value> {
        let res = self.nodes.call(node, NodeCommand::ProbeEgress, CMD_TIMEOUT).await;
        self.audit(actor, "egress.probe", node, json!({}), res.is_ok());
        res
    }
}
