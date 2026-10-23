//! The node's view of its config directory, reloaded on change.
//!
//! Hot-reload semantics: provider/route/limit changes apply to running sessions
//! immediately (keyproxy reads them per request); role/tools/container changes
//! apply to sessions started afterwards.

use anyhow::{Context, Result};
use ocl_protocol::config::*;
use ocl_protocol::model::AgentInfo;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct LoadedAgent {
    pub cfg: AgentConfig,
    pub role_text: String,
    pub hash: String,
    pub errors: Vec<String>,
    pub secret_ok: bool,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub dir: PathBuf,
    pub node: NodeConfig,
    pub providers: BTreeMap<String, ProviderDef>,
    pub routes: BTreeMap<String, Route>,
    pub agents: BTreeMap<String, LoadedAgent>,
    /// Files that failed to parse: path -> error (kept so the UI can show them).
    pub file_errors: BTreeMap<String, String>,
}

impl Snapshot {
    pub fn agent_infos(&self) -> Vec<AgentInfo> {
        self.agents
            .values()
            .map(|a| AgentInfo {
                name: a.cfg.name.clone(),
                description: a.cfg.description.clone(),
                provider: a.cfg.provider.id.clone(),
                model: a.cfg.provider.model.clone(),
                tier: self
                    .providers
                    .get(&a.cfg.provider.id)
                    .map(|p| if p.tier == ProviderTier::BestEffort { "best_effort" } else { "standard" })
                    .unwrap_or("unknown")
                    .into(),
                secret_ok: a.secret_ok,
                config_hash: a.hash.clone(),
                errors: a.errors.clone(),
                labels: a.cfg.labels.clone(),
            })
            .collect()
    }

    pub fn route_for(&self, agent: &AgentConfig) -> Route {
        let name = agent.route.clone().or_else(|| self.node.default_route.clone());
        match name {
            None => Route::Direct,
            Some(n) => self.routes.get(&n).cloned().unwrap_or(Route::Direct),
        }
    }
}

pub fn load(dir: &Path) -> Result<Snapshot> {
    let node: NodeConfig = load_yaml(&dir.join("node.yaml")).context("node.yaml is required")?;
    if !valid_name(&node.name) {
        anyhow::bail!("node name {:?} must match [a-z0-9-]{{1,48}}", node.name);
    }
    let mut file_errors = BTreeMap::new();
    let mut providers = builtin_providers();
    let pf = dir.join("providers.yaml");
    if pf.exists() {
        match load_yaml::<ProvidersFile>(&pf) {
            Ok(f) => providers.extend(f.providers),
            Err(e) => {
                file_errors.insert(pf.display().to_string(), e.to_string());
            }
        }
    }
    let mut routes = BTreeMap::new();
    let rf = dir.join("routes.yaml");
    if rf.exists() {
        match load_yaml::<RoutesFile>(&rf) {
            Ok(f) => routes = f.routes,
            Err(e) => {
                file_errors.insert(rf.display().to_string(), e.to_string());
            }
        }
    }
    let mut agents = BTreeMap::new();
    for (path, res) in load_dir::<AgentConfig>(&dir.join("agents")) {
        let cfg = match res {
            Ok(c) => c,
            Err(e) => {
                file_errors.insert(path.display().to_string(), e.to_string());
                continue;
            }
        };
        let mut errors = cfg.validate(&providers);
        if let Some(r) = &cfg.route {
            if !routes.contains_key(r) {
                errors.push(format!("unknown route {r:?}"));
            }
        }
        let mut role_text = cfg.role.system_prompt.clone().unwrap_or_default();
        if let Some(f) = &cfg.role.system_prompt_file {
            let p = if f.is_absolute() { f.clone() } else { dir.join(f) };
            match std::fs::read_to_string(&p) {
                Ok(t) => {
                    if !role_text.is_empty() {
                        role_text.push_str("\n\n");
                    }
                    role_text.push_str(&t);
                }
                Err(e) => errors.push(format!("system_prompt_file {}: {e}", p.display())),
            }
        }
        let secret_ok = match &cfg.provider.key {
            None => true,
            Some(r) => crate::secrets::resolve(r).is_ok(),
        };
        let mut h = Sha256::new();
        h.update(serde_json::to_vec(&cfg).unwrap_or_default());
        h.update(role_text.as_bytes());
        let hash = hex::encode(&h.finalize()[..8]);
        if agents.contains_key(&cfg.name) {
            file_errors.insert(path.display().to_string(), format!("duplicate agent name {:?}", cfg.name));
            continue;
        }
        agents.insert(cfg.name.clone(), LoadedAgent { cfg, role_text, hash, errors, secret_ok });
    }
    Ok(Snapshot { dir: dir.to_path_buf(), node, providers, routes, agents, file_errors })
}

/// Shared, swappable config snapshot.
#[derive(Clone)]
pub struct ConfigStore {
    inner: Arc<RwLock<Arc<Snapshot>>>,
}

impl ConfigStore {
    pub fn new(s: Snapshot) -> Self {
        ConfigStore { inner: Arc::new(RwLock::new(Arc::new(s))) }
    }
    pub fn get(&self) -> Arc<Snapshot> {
        self.inner.read().unwrap().clone()
    }

    /// Watch the directory; on change reload and send the new snapshot. A broken
    /// node.yaml keeps the previous snapshot (and logs), it never takes the node down.
    pub fn watch(&self) -> Result<mpsc::Receiver<Arc<Snapshot>>> {
        use notify::{RecursiveMode, Watcher};
        let (tx, rx) = mpsc::channel(4);
        let (ftx, mut frx) = mpsc::unbounded_channel();
        let dir = self.get().dir.clone();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                if !matches!(ev.kind, notify::EventKind::Access(_)) {
                    let _ = ftx.send(());
                }
            }
        })?;
        // Only config files: the data dir (workspaces) may live under `dir` too.
        watcher.watch(&dir, RecursiveMode::NonRecursive)?;
        for sub in ["agents", "prompts", "workflows"] {
            let p = dir.join(sub);
            if p.is_dir() {
                watcher.watch(&p, RecursiveMode::Recursive)?;
            }
        }
        let store = self.clone();
        tokio::spawn(async move {
            let _keep = watcher;
            while frx.recv().await.is_some() {
                // Debounce editor write bursts.
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                while frx.try_recv().is_ok() {}
                match load(&dir) {
                    Ok(s) => {
                        let s = Arc::new(s);
                        *store.inner.write().unwrap() = s.clone();
                        tracing::info!(agents = s.agents.len(), errors = s.file_errors.len(), "config reloaded");
                        let _ = tx.send(s).await;
                    }
                    Err(e) => tracing::warn!(error = %e, "config reload failed; keeping previous config"),
                }
            }
        });
        Ok(rx)
    }
}
