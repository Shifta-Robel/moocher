//! The hub's template library (PHASE1_5 §2) and cluster preamble (§10).
//!
//! `templates/*.yaml` are agent templates any node may run once it can
//! resolve their secret alias (`key: { secret: <alias> }` against its own
//! node.yaml `secrets:`), so the hub never needs a key. `preambles/cluster.md`
//! replaces the built-in cluster preamble. Prompt files are inlined here so
//! nodes receive self-contained templates. The set is pushed to every worker
//! node on attach and whenever the files change.

use crate::state::{Sup, CMD_TIMEOUT};
use moocher_protocol::config::{load_dir, valid_name, AgentConfig, NodeRole};
use moocher_protocol::link::NodeCommand;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;

#[derive(Debug, Clone, Default)]
pub struct Library {
    pub templates: Vec<AgentConfig>,
    pub preamble: Option<String>,
    pub errors: Vec<String>,
    pub hash: String,
}

impl Sup {
    pub fn load_library(&self) -> Library {
        let dir = &self.config_dir;
        let mut lib = Library::default();
        for (path, res) in load_dir::<AgentConfig>(&dir.join("templates")) {
            match res {
                Ok(mut t) => {
                    if !valid_name(&t.name) {
                        lib.errors.push(format!("{}: bad template name {:?}", path.display(), t.name));
                        continue;
                    }
                    if let Some(f) = t.role.system_prompt_file.take() {
                        let p = if f.is_absolute() { f } else { dir.join(f) };
                        match std::fs::read_to_string(&p) {
                            Ok(text) => {
                                let mut s = t.role.system_prompt.take().unwrap_or_default();
                                if !s.is_empty() {
                                    s.push_str("\n\n");
                                }
                                s.push_str(&text);
                                t.role.system_prompt = Some(s);
                            }
                            Err(e) => {
                                lib.errors.push(format!("{}: system_prompt_file {}: {e}", path.display(), p.display()));
                                continue;
                            }
                        }
                    }
                    if lib.templates.iter().any(|x| x.name == t.name) {
                        lib.errors.push(format!("{}: duplicate template {:?}", path.display(), t.name));
                        continue;
                    }
                    lib.templates.push(t);
                }
                Err(e) => lib.errors.push(e.to_string()),
            }
        }
        lib.preamble = std::fs::read_to_string(dir.join("preambles").join("cluster.md")).ok();
        let mut h = Sha256::new();
        h.update(serde_json::to_vec(&lib.templates).unwrap_or_default());
        h.update(lib.preamble.as_deref().unwrap_or("").as_bytes());
        lib.hash = hex::encode(&h.finalize()[..8]);
        lib
    }

    /// Push the library to one node (worker nodes only).
    pub async fn sync_templates(&self, node: &str, lib: &Library) {
        let is_relay = self.db.node(node).is_some_and(|n| NodeRole::parse(&n.role) == Some(NodeRole::Relay));
        if is_relay {
            return;
        }
        let cmd = NodeCommand::SyncTemplates { templates: lib.templates.clone(), preamble: lib.preamble.clone(), hash: lib.hash.clone() };
        match self.nodes.call(node, cmd, CMD_TIMEOUT).await {
            Ok(_) => tracing::debug!(node, hash = %lib.hash, "templates synced"),
            Err(e) => tracing::warn!(node, error = %e, "template sync failed"),
        }
    }

    /// Every 10 s: reload the files; on change push to every connected node.
    pub async fn watch_library(self: Arc<Self>) {
        let mut last = String::new();
        loop {
            let lib = self.load_library();
            if lib.hash != last {
                for e in &lib.errors {
                    tracing::warn!(error = %e, "template library");
                }
                tracing::info!(templates = lib.templates.len(), hash = %lib.hash, "template library loaded");
                for n in self.db.nodes().into_iter().filter(|n| n.connected) {
                    self.sync_templates(&n.name, &lib).await;
                }
                last = lib.hash.clone();
            }
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        }
    }

    /// `GET /templates`: the library and where each template can run.
    pub fn templates_view(&self) -> Value {
        let lib = self.load_library();
        let nodes = self.node_views();
        let items: Vec<Value> = lib
            .templates
            .iter()
            .map(|t| {
                let runnable: Vec<&str> = nodes
                    .iter()
                    .filter(|n| n.connected && n.agents.iter().any(|a| a.name == t.name && a.source == "hub" && a.secret_ok && a.errors.is_empty()))
                    .map(|n| n.name.as_str())
                    .collect();
                let blocked: Vec<Value> = nodes
                    .iter()
                    .filter_map(|n| {
                        let a = n.agents.iter().find(|a| a.name == t.name && a.source == "hub")?;
                        let mut why = a.errors.clone();
                        if !a.secret_ok {
                            why.push("secret alias does not resolve on this node".into());
                        }
                        (!why.is_empty()).then(|| json!({"node": n.name, "problems": why}))
                    })
                    .collect();
                let live = self.db.sessions(false).iter().filter(|s| s.report.agent == t.name && s.report.state.is_live()).count();
                json!({
                    "name": t.name,
                    "description": t.description,
                    "model": format!("{}/{}", t.provider.id, t.provider.model),
                    "secret": t.provider.key.as_ref().and_then(|k| k.secret.clone()),
                    "instances": {"live": live, "max": t.instances.max},
                    "overridable": t.overridable,
                    "runnable_on": runnable,
                    "not_runnable": blocked,
                })
            })
            .collect();
        json!({"hash": lib.hash, "templates": items, "errors": lib.errors, "preamble": lib.preamble.is_some()})
    }
}
