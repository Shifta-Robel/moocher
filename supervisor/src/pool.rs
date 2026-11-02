//! The cluster's registry of keys and allowances.
//!
//! Nodes report their key pools and who uses what. The hub sends each node the
//! combined view (what others used up, how busy each allowance is), so a node
//! picks a key with the whole cluster in mind, and it decides who gives way:
//! when `right_now` work hits a limit, less urgent sessions keep off that
//! allowance until it is done. They move to a free key or IP, or their next
//! call waits. Provider keys never reach the hub; a loan between nodes passes
//! through it encrypted.

use crate::state::{Actor, Sup};
use anyhow::{anyhow, Result};
use moocher_protocol::api::QuotaView;
use moocher_protocol::link::NodeCommand;
use moocher_protocol::model::*;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

/// A yield lapses unless renewed, so a lost hub cannot hold work back for long.
const YIELD_TTL_MS: i64 = 90_000;
/// Send the view at least this often, for nodes that reconnected.
const RESEND_MS: i64 = 30_000;
const REGISTRY_TIMEOUT: Duration = Duration::from_secs(5);
const LEND_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Default)]
pub struct PoolState {
    keys: HashMap<String, Vec<PoolKeyInfo>>,
    uses: HashMap<String, Vec<KeyUse>>,
    /// (provider, scope id) -> the right_now session that hit its limit.
    pressure: BTreeMap<(String, String), String>,
    sent: HashMap<String, (Vec<RegistryBucket>, Vec<KeyYield>, BTreeMap<String, u8>, Ts)>,
}

impl Sup {
    pub fn pool_report(&self, node: &str, keys: Vec<PoolKeyInfo>, in_use: Vec<KeyUse>) {
        let mut p = self.pool.lock().unwrap();
        p.keys.insert(node.to_string(), keys);
        p.uses.insert(node.to_string(), in_use);
    }

    /// A session hit a limit. Only `right_now` work makes others give way.
    pub fn on_pressure(&self, session: &str, detail: &Value) {
        let Some(s) = self.db.session(session).filter(|s| self.urgency_of(s) == TaskLabel::RightNow) else { return };
        let (Some(provider), Some(scope_id)) = (detail["provider"].as_str(), detail["scope_id"].as_str()) else { return };
        let new = self.pool.lock().unwrap().pressure.insert((provider.to_string(), scope_id.to_string()), session.to_string()).is_none();
        if new {
            self.log("quota.yield", Some(&s.report.node), Some(session), json!({"provider": provider, "scope_id": scope_id}));
            self.notify("quota.yield", "info", format!("Less urgent work is keeping off {provider} {scope_id}"), format!("for {} ({session}), until it is done", s.report.agent), Some(session.to_string()));
        }
    }

    /// Who must keep off what now. Pressure ends with the work that caused it.
    fn yields(&self) -> Vec<KeyYield> {
        let sessions = self.db.sessions(false);
        let urgent: Vec<&SessionInfo> = sessions.iter().filter(|s| s.report.state.is_live() && self.urgency_of(s) == TaskLabel::RightNow).collect();
        let exempt: Vec<String> = urgent.iter().map(|s| s.report.id.clone()).collect();
        let mut p = self.pool.lock().unwrap();
        p.pressure.retain(|_, session| exempt.contains(session));
        let by = |session: &String| urgent.iter().find(|s| &s.report.id == session).map(|s| s.task_id.map(|t| format!("task #{t}")).unwrap_or_else(|| format!("{} ({})", s.report.agent, s.report.id))).unwrap_or_default();
        p.pressure.iter().map(|((provider, scope_id), session)| KeyYield { provider: provider.clone(), scope_id: scope_id.clone(), exempt: exempt.clone(), expires_at: 0, by: by(session) }).collect()
    }

    /// The allowances as `node` should see them.
    fn registry_for(&self, node: &str, views: &[QuotaView]) -> Vec<RegistryBucket> {
        let now = now_ms();
        let mut m: BTreeMap<(String, String), RegistryBucket> = BTreeMap::new();
        for q in views {
            let b = m.entry((q.provider.clone(), q.scope_id.clone())).or_insert_with(|| RegistryBucket { provider: q.provider.clone(), scope_id: q.scope_id.clone(), ..Default::default() });
            b.used = b.used.max(q.used);
            // Units can be per model, which an allowance as a whole does not say.
            if q.unit.is_none() && q.exhausted {
                b.exhausted_until = q.resets_at.filter(|r| *r > now).max(b.exhausted_until);
            }
        }
        let p = self.pool.lock().unwrap();
        for (_, keys) in p.keys.iter().filter(|(n, _)| n.as_str() != node) {
            for k in keys.iter().filter(|k| k.in_flight > 0) {
                let b = m.entry((k.provider.clone(), k.scope_id.clone())).or_insert_with(|| RegistryBucket { provider: k.provider.clone(), scope_id: k.scope_id.clone(), ..Default::default() });
                b.in_flight += k.in_flight;
            }
        }
        for (_, uses) in p.uses.iter().filter(|(n, _)| n.as_str() != node) {
            for u in uses {
                let b = m.entry((u.provider.clone(), u.scope_id.clone())).or_insert_with(|| RegistryBucket { provider: u.provider.clone(), scope_id: u.scope_id.clone(), ..Default::default() });
                b.sessions += 1;
            }
        }
        m.into_values().collect()
    }

    /// Send every node its view, when it changed.
    pub async fn push_registry(&self) {
        let now = now_ms();
        let views = self.quota_views();
        let yields = self.yields();
        let levels: BTreeMap<String, u8> = self.db.sessions(false).iter().filter(|s| s.report.state.is_live()).map(|s| (s.report.id.clone(), self.urgency_of(s).level())).filter(|(_, l)| *l != TaskLabel::Normal.level()).collect();
        for n in self.node_views().into_iter().filter(|n| n.connected && !n.relay) {
            let buckets = self.registry_for(&n.name, &views);
            {
                let mut p = self.pool.lock().unwrap();
                if p.sent.get(&n.name).is_some_and(|(b, y, l, at)| *b == buckets && *y == yields && *l == levels && now - at < RESEND_MS) {
                    continue;
                }
                p.sent.insert(n.name.clone(), (buckets.clone(), yields.clone(), levels.clone(), now));
            }
            let live = yields.iter().cloned().map(|mut y| {
                y.expires_at = now + YIELD_TTL_MS;
                y
            });
            let cmd = NodeCommand::Registry { buckets, yields: live.collect(), levels: levels.clone() };
            if let Err(e) = self.nodes.call(&n.name, cmd, REGISTRY_TIMEOUT).await {
                tracing::debug!(node = %n.name, error = %format!("{e:#}"), "registry not delivered");
                self.pool.lock().unwrap().sent.remove(&n.name);
            }
        }
    }

    /// Find a node to lend `from_node` a key. The reply is encrypted for the borrower.
    pub async fn borrow_key(&self, from_session: &str, from_node: &str, provider: &str, public_key: &str, have: &[String]) -> Result<Value> {
        let now = now_ms();
        let exhausted: Vec<String> = self.quota_views().into_iter().filter(|q| q.provider == provider && q.exhausted && q.resets_at.is_none_or(|r| r > now)).map(|q| q.scope_id).collect();
        let mut offers: Vec<(String, PoolKeyInfo)> = {
            let p = self.pool.lock().unwrap();
            p.keys.iter().filter(|(n, _)| n.as_str() != from_node && self.nodes.connected(n)).flat_map(|(n, keys)| keys.iter().map(move |k| (n.clone(), k.clone()))).collect()
        };
        offers.retain(|(_, k)| k.provider == provider && k.lendable && !k.borrowed && k.bad_until.is_none() && !have.contains(&k.key_id) && !exhausted.contains(&k.scope_id));
        offers.sort_by_key(|(_, k)| k.in_flight);
        for (node, k) in offers {
            let cmd = NodeCommand::LendKey { provider: provider.to_string(), key_id: k.key_id.clone(), to: from_node.to_string(), public_key: public_key.to_string() };
            match self.nodes.call(&node, cmd, LEND_TIMEOUT).await {
                Ok(v) => {
                    self.audit(&Actor::Session(from_session.to_string()), "key.lent", &k.key_id, json!({"provider": provider, "from": node, "to": from_node}), true);
                    self.notify("key.lent", "info", format!("{node} lent a {provider} key to {from_node}"), format!("{} has run out of its own", from_node), Some(from_session.to_string()));
                    return Ok(v);
                }
                Err(e) => tracing::warn!(node = %node, error = %format!("{e:#}"), "could not borrow"),
            }
        }
        Err(anyhow!("no node has a {provider} key to lend"))
    }

    /// Could another node lend `node` a key with room?
    pub fn lender_for(&self, node: &str, provider: &str, views: &[QuotaView]) -> bool {
        let now = now_ms();
        let exhausted = |id: &str| views.iter().any(|q| q.provider == provider && q.scope_id == id && q.exhausted && q.resets_at.is_none_or(|r| r > now));
        self.pool.lock().unwrap().keys.iter().filter(|(n, _)| n.as_str() != node && self.nodes.connected(n)).any(|(_, keys)| keys.iter().any(|k| k.provider == provider && k.lendable && !k.borrowed && k.bad_until.is_none() && !exhausted(&k.scope_id)))
    }

    /// `GET /pool`.
    pub fn pool_view(&self) -> PoolView {
        let now = now_ms();
        let views = self.quota_views();
        let yields = self.yields().into_iter().map(|mut y| {
            y.expires_at = now + YIELD_TTL_MS;
            y
        });
        let p = self.pool.lock().unwrap();
        let state = |k: &PoolKeyInfo| {
            let scope = views.iter().filter(|q| q.provider == k.provider && q.scope_id == k.scope_id);
            (scope.clone().filter(|q| q.exhausted).filter_map(|q| q.resets_at).filter(|r| *r > now).max(), scope.map(|q| q.used).max().unwrap_or(0))
        };
        let mut keys: Vec<PoolNodeKey> = p.keys.iter().flat_map(|(n, ks)| ks.iter().map(|k| {
            let (exhausted_until, used) = state(k);
            PoolNodeKey { node: n.clone(), key: k.clone(), exhausted_until, used }
        })).collect();
        keys.sort_by(|a, b| (&a.key.provider, &a.key.key_id, &a.node).cmp(&(&b.key.provider, &b.key.key_id, &b.node)));
        let mut in_use: Vec<PoolNodeUse> = p.uses.iter().flat_map(|(n, us)| us.iter().map(|u| PoolNodeUse { node: n.clone(), used: u.clone() })).collect();
        in_use.sort_by(|a, b| a.used.session.cmp(&b.used.session));
        PoolView { keys, in_use, yields: yields.collect() }
    }
}
