//! Shared workspaces across machines (ARCHITECTURE §6b).
//!
//! A shared workspace is one directory per node. When sessions using it run
//! on more than one machine, the hub moves changes between the copies through
//! the artifact store: the holder node snapshots its copy (tar.zst of the
//! tree) and the other node merges it file by file against the last state both
//! saw (see `ocl_protocol::archive::merge3`). This happens
//! * before a session starts on a node whose copy may be behind,
//! * after a session's turn, towards the other machines with live sessions,
//! * when an agent calls `cluster_sync_workspace`.
//!
//! The hub remembers the latest snapshot of each workspace, so a node that
//! never had it (or replaces a dead one) can start from there.

use crate::state::{Actor, Sup};
use anyhow::{anyhow, Result};
use ocl_protocol::link::NodeCommand;
use ocl_protocol::model::*;
use serde_json::{json, Value};
use std::time::Duration;

const SYNC_TIMEOUT: Duration = Duration::from_secs(300);

impl Sup {
    /// Nodes with a live session in shared workspace `name`.
    fn ws_live_nodes(&self, name: &str) -> Vec<String> {
        let label = format!("shared:{name}");
        let mut v: Vec<String> =
            self.db.sessions(false).into_iter().filter(|s| s.report.state.is_live() && s.report.workspace.as_deref() == Some(&label)).map(|s| s.report.node).collect();
        v.sort();
        v.dedup();
        v
    }

    fn ws_record(&self, name: &str, reference: &str, node: &str) {
        let mut info = self.db.workspace(name).unwrap_or_else(|| json!({"name": name, "version": 0, "nodes": []}));
        info["latest_ref"] = json!(reference);
        info["latest_node"] = json!(node);
        info["version"] = json!(info["version"].as_u64().unwrap_or(0) + 1);
        info["updated_at"] = json!(now_ms());
        let mut nodes: Vec<String> = serde_json::from_value(info["nodes"].clone()).unwrap_or_default();
        if !nodes.iter().any(|n| n == node) {
            nodes.push(node.into());
        }
        info["nodes"] = json!(nodes);
        let _ = self.db.put_workspace(name, &info);
    }

    fn ws_add_node(&self, name: &str, node: &str) {
        let mut info = self.db.workspace(name).unwrap_or_else(|| json!({"name": name, "version": 0, "nodes": []}));
        let mut nodes: Vec<String> = serde_json::from_value(info["nodes"].clone()).unwrap_or_default();
        if !nodes.iter().any(|n| n == node) {
            nodes.push(node.into());
            info["nodes"] = json!(nodes);
            let _ = self.db.put_workspace(name, &info);
        }
    }

    /// Snapshot `label` on `node`; returns the artifact reference.
    pub async fn ws_snapshot(&self, node: &str, label: &str, session: Option<&str>, reason: &str) -> Result<Value> {
        let v = self
            .nodes
            .call(node, NodeCommand::SnapshotWorkspace { workspace: label.into(), session: session.map(String::from), reason: reason.into() }, SYNC_TIMEOUT)
            .await?;
        if let Some(name) = label.strip_prefix("shared:") {
            if let Some(r) = v["reference"].as_str() {
                self.ws_record(name, r, node);
            }
        }
        Ok(v)
    }

    async fn ws_apply(&self, node: &str, name: &str, reference: &str, from: Option<&str>) -> Result<Value> {
        let r = self
            .nodes
            .call(node, NodeCommand::ApplyWorkspace { workspace: format!("shared:{name}"), reference: reference.into(), from_node: from.map(String::from) }, SYNC_TIMEOUT)
            .await?;
        self.ws_add_node(name, node);
        let conflicts: Vec<String> = serde_json::from_value(r["conflicts"].clone()).unwrap_or_default();
        self.log("workspace.synced", Some(node), None, json!({"workspace": name, "from": from, "ref": reference, "report": r}));
        if !conflicts.is_empty() {
            // Tell the agents working in this copy which files need a look.
            let label = format!("shared:{name}");
            let msg = format!(
                "[cluster] Workspace sync from {} brought changes that conflict with yours in: {}. Your versions were kept; theirs are next to them as `<file>.theirs`. Reconcile them and delete the .theirs files.",
                from.unwrap_or("the hub"),
                conflicts.join(", ")
            );
            for s in self.db.sessions_on(node).into_iter().filter(|s| s.report.state.is_live() && s.report.workspace.as_deref() == Some(&label)) {
                let _ = self.message(&Actor::System, &s.report.id, &msg, Delivery::Queue).await;
            }
        }
        Ok(r)
    }

    /// Move `name` from node `from` to node `to`.
    pub async fn ws_transfer(&self, name: &str, from: &str, to: &str, reason: &str) -> Result<Value> {
        let snap = self.ws_snapshot(from, &format!("shared:{name}"), None, reason).await?;
        let reference = snap["reference"].as_str().ok_or_else(|| anyhow!("snapshot on {from} returned no reference"))?.to_string();
        self.ws_apply(to, name, &reference, Some(from)).await
    }

    /// Before a session in shared workspace `name` starts on `node`: bring the
    /// node's copy up to date with every other machine that holds it, or with
    /// the hub's latest snapshot when none of them is reachable.
    pub async fn ws_prepare(&self, name: &str, node: &str) -> Vec<String> {
        let mut notes = vec![];
        let info = self.db.workspace(name);
        let known: Vec<String> = info.as_ref().and_then(|i| serde_json::from_value(i["nodes"].clone()).ok()).unwrap_or_default();
        let latest_node = info.as_ref().and_then(|i| i["latest_node"].as_str().map(String::from));
        // Every machine that holds a copy: live sessions, earlier sessions
        // (shared workspace directories outlive their sessions), the last
        // publisher. Merging from each of them is safe: it is three-way.
        let label = format!("shared:{name}");
        let mut sources: Vec<String> = self.ws_live_nodes(name);
        sources.extend(known.iter().cloned());
        sources.extend(self.db.sessions(true).into_iter().filter(|s| s.report.workspace.as_deref() == Some(&label)).map(|s| s.report.node));
        if let Some(l) = &latest_node {
            sources.push(l.clone());
        }
        sources.sort();
        sources.dedup();
        sources.retain(|n| n != node);
        let mut synced = false;
        for src in sources.iter().filter(|n| self.nodes.connected(n)) {
            match self.ws_transfer(name, src, node, "handoff").await {
                Ok(_) => {
                    synced = true;
                    notes.push(format!("synced from {src}"));
                }
                Err(e) => notes.push(format!("sync from {src} failed: {e:#}")),
            }
        }
        if !synced && !sources.is_empty() {
            // Every holder is down: start from the last snapshot the hub has.
            if let Some(r) = info.as_ref().and_then(|i| i["latest_ref"].as_str()) {
                match self.ws_apply(node, name, r, None).await {
                    Ok(_) => notes.push(format!("restored from the hub's latest snapshot {r}")),
                    Err(e) => notes.push(format!("restore from {r} failed: {e:#}")),
                }
            }
        }
        if !known.iter().any(|n| n == node) {
            self.ws_add_node(name, node);
        }
        notes
    }

    /// After a turn on `node`: push its copy to the other machines that have
    /// live sessions in the same workspace.
    pub async fn ws_after_turn(&self, s: &SessionInfo) {
        let Some(name) = s.report.workspace.as_deref().and_then(|w| w.strip_prefix("shared:")) else { return };
        let others: Vec<String> = self.ws_live_nodes(name).into_iter().filter(|n| n != &s.report.node && self.nodes.connected(n)).collect();
        if others.is_empty() {
            return;
        }
        let snap = match self.ws_snapshot(&s.report.node, &format!("shared:{name}"), Some(&s.report.id), "turn").await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(workspace = name, error = %e, "workspace snapshot failed");
                return;
            }
        };
        // Applying an unchanged snapshot is a no-op merge on the other side.
        let Some(reference) = snap["reference"].as_str() else { return };
        for n in others {
            if let Err(e) = self.ws_apply(&n, name, reference, Some(&s.report.node)).await {
                tracing::warn!(workspace = name, node = n, error = %e, "workspace push failed");
            }
        }
    }

    /// `cluster_sync_workspace`: pull every other copy into the caller's, then
    /// push the result back out.
    pub async fn ws_sync_for(&self, caller: &SessionInfo) -> Result<Value> {
        let name = caller.report.workspace.as_deref().and_then(|w| w.strip_prefix("shared:")).ok_or_else(|| anyhow!("your workspace is not shared; use cluster_artifact_put to hand files over"))?.to_string();
        let me = caller.report.node.clone();
        let others: Vec<String> = self.ws_live_nodes(&name).into_iter().filter(|n| n != &me && self.nodes.connected(n)).collect();
        if others.is_empty() {
            return Ok(json!({"workspace": name, "note": "no other machine holds this workspace right now; nothing to sync"}));
        }
        let mut pulled = vec![];
        for n in &others {
            pulled.push(json!({"from": n, "report": self.ws_transfer(&name, n, &me, "sync").await?}));
        }
        let mut pushed = vec![];
        for n in &others {
            pushed.push(json!({"to": n, "report": self.ws_transfer(&name, &me, n, "sync").await?}));
        }
        Ok(json!({"workspace": name, "pulled": pulled, "pushed": pushed}))
    }
}
