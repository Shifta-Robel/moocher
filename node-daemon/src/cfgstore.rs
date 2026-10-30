//! The node's view of its config directory, reloaded on change.
//!
//! Hot-reload semantics: provider/route/limit changes apply to running sessions
//! immediately (keyproxy reads them per request); role/tools/container changes
//! apply to sessions started afterwards.

use anyhow::{Context, Result};
use moocher_protocol::config::*;
use moocher_protocol::model::AgentInfo;
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
    /// `local` (agents/) or `hub` (the hub's template library).
    pub source: &'static str,
}

/// The hub's template library as last received (cached in the data dir so
/// sessions keep working while the hub is unreachable).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct HubSet {
    pub templates: Vec<AgentConfig>,
    pub preamble: Option<String>,
    #[serde(default)]
    pub git: Option<moocher_protocol::config::GitConfig>,
    pub hash: String,
}

pub fn data_dir(dir: &Path, node: &NodeConfig) -> PathBuf {
    node.runtime.data_dir.clone().unwrap_or_else(|| dir.join("data"))
}

fn hub_cache(dir: &Path, node: &NodeConfig) -> PathBuf {
    data_dir(dir, node).join("hub-templates.json")
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub dir: PathBuf,
    pub node: NodeConfig,
    pub providers: BTreeMap<String, ProviderDef>,
    pub routes: BTreeMap<String, Route>,
    /// Live changes from the UI/CLI (`overrides.yaml`).
    pub overrides: OverridesFile,
    pub agents: BTreeMap<String, LoadedAgent>,
    /// Files that failed to parse: path -> error (kept so the UI can show them).
    pub file_errors: BTreeMap<String, String>,
    /// Cluster preamble from the hub (None: the built-in default).
    pub hub_preamble: Option<String>,
    pub hub_git: Option<moocher_protocol::config::GitConfig>,
    pub hub_hash: String,
}

impl Snapshot {
    /// The hub's `git:`, else this node's, else the defaults.
    pub fn git(&self) -> moocher_protocol::config::GitConfig {
        self.hub_git.clone().or_else(|| self.node.git.clone()).unwrap_or_default()
    }

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
                routes: {
                    let p = self.route_plan(&a.cfg);
                    std::iter::once(p.prefer).chain(p.failover).collect()
                },
                source: a.source.into(),
                max_instances: a.cfg.instances.max,
                overridable: a.cfg.overridable.clone(),
                generic: a.cfg.is_generic(),
                min_urgency: a.cfg.min_urgency,
                price_in_per_mtok: self.providers.get(&a.cfg.provider.id).and_then(|p| p.prices.get(&a.cfg.provider.model)).map(|p| p.input_per_mtok).unwrap_or(0.0),
                price_out_per_mtok: self.providers.get(&a.cfg.provider.id).and_then(|p| p.prices.get(&a.cfg.provider.model)).map(|p| p.output_per_mtok).unwrap_or(0.0),
                on_death: a.cfg.on_death.clone(),
                context: a.cfg.provider.context,
            })
            .collect()
    }

    /// Resolve an agent's key: (secret, account label). Aliases go through node.yaml `secrets:`.
    pub fn secret(&self, key: &SecretRef) -> Result<(String, Option<String>)> {
        resolve_key(&self.node.secrets, key)
    }

    /// Route plan for an agent: overrides.yaml, then the agent's `route`,
    /// then the node's `default_route`, else `direct`.
    pub fn route_plan(&self, agent: &AgentConfig) -> RoutePlan {
        self.overrides
            .routes
            .get(&agent.name)
            .or(agent.route.as_ref())
            .cloned()
            .or_else(|| self.node.default_route.clone().map(RouteSpec::Name))
            .unwrap_or_else(|| RouteSpec::Name("direct".into()))
            .plan()
    }

    /// `direct` always exists (unless routes.yaml redefines it).
    pub fn route(&self, name: &str) -> Option<Route> {
        self.routes.get(name).cloned().or_else(|| (name == "direct").then_some(Route::Direct))
    }

    /// Route names any agent on this node may use (for egress IP probing).
    pub fn used_routes(&self) -> Vec<String> {
        let mut v: Vec<String> = vec!["direct".into()];
        for a in self.agents.values() {
            let p = self.route_plan(&a.cfg);
            for n in std::iter::once(p.prefer).chain(p.failover) {
                if !v.contains(&n) {
                    v.push(n);
                }
            }
        }
        v
    }
}

fn resolve_key(aliases: &BTreeMap<String, SecretRef>, key: &SecretRef) -> Result<(String, Option<String>)> {
    match &key.secret {
        Some(alias) => {
            let target = aliases.get(alias).ok_or_else(|| anyhow::anyhow!("secret alias {alias:?} is not defined in this node's node.yaml `secrets:`"))?;
            if target.secret.is_some() {
                anyhow::bail!("secret alias {alias:?} must point at a ref, not another alias");
            }
            Ok((crate::secrets::resolve(target)?, target.account.clone().or_else(|| key.account.clone())))
        }
        None => Ok((crate::secrets::resolve(key)?, key.account.clone())),
    }
}

pub fn load(dir: &Path) -> Result<Snapshot> {
    let node: NodeConfig = load_yaml(&dir.join("node.yaml")).context("node.yaml is required")?;
    if !valid_name(&node.name) {
        anyhow::bail!("node name {:?} must match [a-z0-9-]{{1,48}}", node.name);
    }
    let hub: HubSet = if node.hub_templates {
        std::fs::read(hub_cache(dir, &node)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    } else {
        HubSet::default()
    };
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
    let mut overrides = OverridesFile::default();
    let of = dir.join("overrides.yaml");
    if of.exists() {
        match load_yaml::<OverridesFile>(&of) {
            Ok(f) => overrides = f,
            Err(e) => {
                file_errors.insert(of.display().to_string(), e.to_string());
            }
        }
    }
    let mut agents = BTreeMap::new();
    // Local agents first; hub templates fill in names the node does not define itself.
    let local = load_dir::<AgentConfig>(&dir.join("agents")).into_iter().map(|(p, r)| (p, r, "local"));
    let from_hub = hub.templates.iter().cloned().map(|t| (PathBuf::from(format!("hub:templates/{}", t.name)), Ok(t), "hub"));
    for (path, res, source) in local.chain(from_hub) {
        let cfg = match res {
            Ok(c) => c,
            Err(e) => {
                file_errors.insert(path.display().to_string(), e.to_string());
                continue;
            }
        };
        if source == "hub" && agents.contains_key(&cfg.name) {
            continue; // a local agent of the same name wins
        }
        let mut errors = cfg.validate(&providers);
        let spec = overrides.routes.get(&cfg.name).or(cfg.route.as_ref());
        for r in spec.map(|s| s.names()).unwrap_or_default() {
            if r != "direct" && !routes.contains_key(&r) {
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
            Some(r) => resolve_key(&node.secrets, r).is_ok(),
        };
        let mut h = Sha256::new();
        h.update(serde_json::to_vec(&cfg).unwrap_or_default());
        h.update(role_text.as_bytes());
        let hash = hex::encode(&h.finalize()[..8]);
        if agents.contains_key(&cfg.name) {
            file_errors.insert(path.display().to_string(), format!("duplicate agent name {:?}", cfg.name));
            continue;
        }
        agents.insert(cfg.name.clone(), LoadedAgent { cfg, role_text, hash, errors, secret_ok, source });
    }
    Ok(Snapshot { dir: dir.to_path_buf(), node, providers, routes, overrides, agents, file_errors, hub_preamble: hub.preamble, hub_git: hub.git, hub_hash: hub.hash })
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

    /// Store the hub's template library (cached on disk) and reload.
    pub fn set_hub(&self, set: &HubSet) -> Result<Arc<Snapshot>> {
        let snap = self.get();
        let path = hub_cache(&snap.dir, &snap.node);
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(&path, serde_json::to_vec(set)?)?;
        self.reload()
    }

    /// Reload now (after the node itself wrote a file); the watcher also fires.
    pub fn reload(&self) -> Result<Arc<Snapshot>> {
        let s = Arc::new(load(&self.get().dir)?);
        *self.inner.write().unwrap() = s.clone();
        Ok(s)
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
