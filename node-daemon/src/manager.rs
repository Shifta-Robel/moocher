//! Session instances on this node: spawn, observe, command, reconcile.
//!
//! One instance = one container = one OpenCode session. The instance's
//! `SessionReport` is the single source of truth sent to the supervisor. It is
//! maintained from OpenCode's v1 instance bus (`/event`) and rebuilt from the
//! session's full message history whenever that stream (re)connects, so a lost
//! event never leaves tokens or todos wrong for long. Reports are persisted so
//! a node restart can re-attach to still-running containers.

use crate::cfgstore::{ConfigStore, LoadedAgent, Snapshot};
use crate::gateway::Gateway;
use crate::link::LinkHandle;
use crate::oc;
use crate::runtime::{InstancePaths, LaunchSpec, Runtime};
use crate::store::{InstanceRow, Store};
use anyhow::{anyhow, bail, Context, Result};
use ocl_protocol::config::*;
use ocl_protocol::link::{Hello, NodeCommand, NodeEvent, SpawnSpec, PROTOCOL_VERSION};
use ocl_protocol::model::*;
use rand::Rng;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub struct Manager {
    pub cfg: ConfigStore,
    /// Shared by every session's gateway: config, link, egress routes, quotas.
    pub gw: crate::gateway::GatewayDeps,
    store: Arc<Store>,
    rt: Runtime,
    pub link: LinkHandle,
    node: String,
    run_root: PathBuf,
    data_root: PathBuf,
    instances: Mutex<HashMap<String, Arc<Instance>>>,
}

pub struct Instance {
    id: String,
    agent: String,
    role: SessionRole,
    paths: InstancePaths,
    oc: oc::Client,
    /// OpenCode provider id (`ocl-<provider>`), model id and agent mode.
    oc_provider: String,
    oc_model: String,
    oc_agent: String,
    oc_id: Mutex<Option<String>>,
    report: Mutex<SessionReport>,
    /// Usage per assistant message id; the session total is their sum, so
    /// replaying a message update never double counts.
    msg_usage: Mutex<HashMap<String, TokenUsage>>,
    usage: Arc<Mutex<TokenUsage>>,
    /// Set by the gateway while every route's allowance is used up.
    parked: crate::gateway::Parked,
    gateway: Mutex<Option<Gateway>>,
    overrides: Mutex<SpawnOverrides>,
    /// Tool calls already reported as activity (part ids).
    seen_tools: Mutex<HashSet<String>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    pending_prompts: Mutex<Vec<(String, Delivery)>>,
    stopping: AtomicBool,
    /// A turn is in progress and its end has not been reported yet.
    turn_open: AtomicBool,
}

pub fn random_hex(bytes: usize) -> String {
    let b: Vec<u8> = (0..bytes).map(|_| rand::thread_rng().gen()).collect();
    hex::encode(b)
}

fn random_token(prefix: &str) -> String {
    let b: [u8; 24] = rand::thread_rng().gen();
    format!("{prefix}{}", hex::encode(b))
}

impl Manager {
    pub fn new(cfg: ConfigStore, store: Arc<Store>, rt: Runtime, link: LinkHandle, gw: crate::gateway::GatewayDeps) -> Result<Arc<Manager>> {
        let snap = cfg.get();
        let data_root = snap.node.runtime.data_dir.clone().unwrap_or_else(|| snap.dir.join("data"));
        let run_root = snap.node.runtime.run_dir.clone().unwrap_or_else(|| {
            std::env::var_os("XDG_RUNTIME_DIR").map(|d| PathBuf::from(d).join("ocl")).unwrap_or_else(|| PathBuf::from("/run/ocl"))
        });
        // Relay-only nodes run no sessions and need no socket dir.
        if snap.node.role != NodeRole::Relay {
            // Session sockets live at <run_dir>/<s-xxxxxxxxxx>/node.sock; unix socket
            // paths are limited to 107 bytes, so catch a too-long run_dir now.
            let longest = run_root.join("s-0123456789").join("node.sock");
            if longest.as_os_str().len() > 107 {
                bail!(
                    "runtime.run_dir {} is too long for unix sockets ({} bytes with the session socket, max 107); use a shorter path such as /run/ocl",
                    run_root.display(),
                    longest.as_os_str().len()
                );
            }
            std::fs::create_dir_all(&run_root).with_context(|| format!("creating {}", run_root.display()))?;
        }
        std::fs::create_dir_all(data_root.join("workspaces"))?;
        std::fs::create_dir_all(data_root.join("instances"))?;
        Ok(Arc::new(Manager {
            node: snap.node.name.clone(),
            cfg,
            gw,
            store,
            rt,
            link,
            run_root,
            data_root,
            instances: Mutex::new(HashMap::new()),
        }))
    }

    pub fn data_root(&self) -> &PathBuf {
        &self.data_root
    }

    fn get(&self, id: &str) -> Result<Arc<Instance>> {
        self.instances.lock().unwrap().get(id).cloned().ok_or_else(|| anyhow!("no session {id} on node {}", self.node))
    }

    pub fn reports(&self) -> Vec<SessionReport> {
        self.instances.lock().unwrap().values().map(|i| i.report.lock().unwrap().clone()).collect()
    }

    pub async fn hello(&self) -> Hello {
        let snap = self.cfg.get();
        let relay_only = snap.node.role == NodeRole::Relay;
        Hello {
            protocol: PROTOCOL_VERSION,
            node: self.node.clone(),
            version: ocl_protocol::VERSION.into(),
            max_sessions: if relay_only { 0 } else { snap.node.max_sessions },
            relay: snap.node.relay || relay_only,
            agents: if relay_only { vec![] } else { snap.agent_infos() },
            sessions: self.reports(),
            labels: snap.node.labels.clone(),
            role: snap.node.role,
        }
    }

    pub async fn handle(self: &Arc<Self>, cmd: NodeCommand) -> Result<Value> {
        match cmd {
            NodeCommand::Spawn(spec) => self.spawn(spec).await,
            NodeCommand::Prompt { session, text, delivery } => {
                let inst = self.get(&session)?;
                self.prompt(&inst, &text, delivery).await?;
                Ok(json!({"ok": true}))
            }
            NodeCommand::Interrupt { session } => {
                let inst = self.get(&session)?;
                let oc_id = inst.oc_id.lock().unwrap().clone().ok_or_else(|| anyhow!("session still starting"))?;
                inst.oc.abort(&oc_id).await?;
                Ok(json!({"ok": true}))
            }
            NodeCommand::Stop { session, remove_workspace } => {
                self.stop(&session, remove_workspace).await?;
                Ok(json!({"ok": true}))
            }
            NodeCommand::PermissionReply { session, request, reply } => {
                let inst = self.get(&session)?;
                inst.oc.reply_permission(&request, &reply).await?;
                Ok(json!({"ok": true}))
            }
            NodeCommand::SetRoute { agent, route, by } => {
                let snap = self.cfg.get();
                if let Some(r) = &route {
                    for n in r.names() {
                        if snap.route(&n).is_none() {
                            bail!("node {} has no route {n:?} (routes.yaml)", self.node);
                        }
                    }
                }
                if !snap.agents.contains_key(&agent) {
                    bail!("node {} has no agent {agent:?}", self.node);
                }
                self.write_overrides(|o| {
                    match route {
                        Some(r) => {
                            o.routes.insert(agent.clone(), r);
                        }
                        None => {
                            o.routes.remove(&agent);
                        }
                    }
                    tracing::info!(agent, by, "route override changed");
                })?;
                self.gw.egress.probe().await;
                Ok(json!({"ok": true}))
            }
            NodeCommand::SetQuota { ov, clear } => {
                self.write_overrides(|o| {
                    let now = now_ms();
                    o.quotas.retain(|x| x.resets_at.is_none_or(|r| r > now) && !(x.provider == ov.provider && x.scope_id == ov.scope_id));
                    if !clear {
                        o.quotas.push(ov.clone());
                    }
                })?;
                self.gw.quotas.touch();
                Ok(json!({"ok": true}))
            }
            NodeCommand::SyncTemplates { templates, preamble, hash } => {
                if self.cfg.get().hub_hash != hash {
                    let n = templates.len();
                    let s = self.cfg.set_hub(&crate::cfgstore::HubSet { templates, preamble, hash })?;
                    tracing::info!(templates = n, "hub template library updated");
                    let agents = if s.node.role == NodeRole::Relay { vec![] } else { s.agent_infos() };
                    self.link.emit(NodeEvent::Agents { agents });
                }
                Ok(json!({"ok": true}))
            }
            NodeCommand::ReadPreamble { session } => {
                let inst = self.get(&session)?;
                let text = std::fs::read_to_string(inst.paths.etc.join("ROLE.md")).context("reading the session's instructions")?;
                Ok(json!({"preamble": text}))
            }
            NodeCommand::ProbeEgress => {
                self.gw.egress.probe().await;
                Ok(json!({"ips": self.gw.egress.snapshot()}))
            }
            NodeCommand::OpenTunnel { id, host, port, from } => {
                let snap = self.cfg.get();
                crate::relay::accept(&snap.node, &self.link, &id, &host, port, &from).await?;
                Ok(json!({"ok": true}))
            }
            NodeCommand::Exec { session, command, timeout_secs } => {
                let inst = self.get(&session)?;
                let (code, output) = self.rt.exec(&session, &inst.paths.workspace, &command, Duration::from_secs(timeout_secs.clamp(1, 1800))).await?;
                Ok(json!({"code": code, "output": output}))
            }
        }
    }

    /// Apply a change to overrides.yaml and reload the config now.
    fn write_overrides(&self, f: impl FnOnce(&mut OverridesFile)) -> Result<()> {
        let snap = self.cfg.get();
        let mut o = snap.overrides.clone();
        f(&mut o);
        let path = snap.dir.join("overrides.yaml");
        let body = format!(
            "# Live changes made from the opencode-cluster UI/CLI, written by opencode-node.\n# Kept apart from your version-controlled files; safe to edit or delete.\n{}",
            serde_yaml::to_string(&o)?
        );
        let tmp = path.with_extension("yaml.tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, &path)?;
        self.cfg.reload()?;
        Ok(())
    }

    fn workspace_dir(&self, id: &str, ws: &WorkspaceSpec) -> Result<(PathBuf, String)> {
        match ws.mode {
            WorkspaceMode::Isolated => Ok((self.data_root.join("workspaces").join(id), format!("isolated:{id}"))),
            WorkspaceMode::Shared => {
                let name = ws.name.clone().ok_or_else(|| anyhow!("shared workspace needs a name"))?;
                if !valid_name(&name) {
                    bail!("workspace name {name:?} must match [a-z0-9-]{{1,48}}");
                }
                Ok((self.data_root.join("workspaces").join(format!("shared-{name}")), format!("shared:{name}")))
            }
        }
    }

    fn new_instance(&self, id: &str, agent: &str, role: SessionRole, paths: InstancePaths, password: &str, report: SessionReport) -> Arc<Instance> {
        let snap = self.cfg.get();
        // report.model is "<provider>/<model>" (the model id itself may contain '/').
        let (prov, model) = report.model.as_deref().and_then(|m| m.split_once('/')).map(|(p, m)| (p.to_string(), m.to_string())).unwrap_or_default();
        let oc_agent = snap.agents.get(agent).map(|a| a.cfg.role.opencode_agent.clone()).unwrap_or_else(|| "build".into());
        let dir = self.rt.workspace_in_runtime(&paths);
        Arc::new(Instance {
            id: id.into(),
            agent: agent.into(),
            role,
            oc: oc::Client::new(paths.oc_sock(), password, &dir),
            paths,
            oc_provider: format!("ocl-{prov}"),
            oc_model: model,
            oc_agent,
            oc_id: Mutex::new(report.opencode_id.clone()),
            usage: Arc::new(Mutex::new(report.tokens)),
            parked: Arc::new(Mutex::new(None)),
            gateway: Mutex::new(None),
            overrides: Mutex::new(SpawnOverrides::default()),
            turn_open: AtomicBool::new(matches!(report.state, SessionState::Running | SessionState::Blocked | SessionState::Parked)),
            report: Mutex::new(report),
            msg_usage: Mutex::new(HashMap::new()),
            seen_tools: Mutex::new(HashSet::new()),
            tasks: Mutex::new(vec![]),
            pending_prompts: Mutex::new(vec![]),
            stopping: AtomicBool::new(false),
        })
    }

    async fn spawn(self: &Arc<Self>, spec: SpawnSpec) -> Result<Value> {
        let snap = self.cfg.get();
        if snap.node.role == NodeRole::Relay {
            bail!("node {} is a relay-only node and runs no agents", self.node);
        }
        let agent = snap.agents.get(&spec.agent).cloned().ok_or_else(|| anyhow!("agent {:?} is not configured on node {}", spec.agent, self.node))?;
        if !agent.errors.is_empty() {
            bail!("agent {} has config errors: {}", spec.agent, agent.errors.join("; "));
        }
        if !agent.secret_ok {
            bail!("agent {}: provider secret does not resolve on node {}", spec.agent, self.node);
        }
        if !valid_id(&spec.id) {
            bail!("invalid session id {:?}", spec.id);
        }
        spec.overrides.check(&agent.cfg).map_err(|e| anyhow!(e))?;
        if let Some(r) = &spec.overrides.route {
            if snap.route(r).is_none() {
                bail!("node {} has no route {r:?}", self.node);
            }
        }
        {
            let map = self.instances.lock().unwrap();
            if map.contains_key(&spec.id) {
                return Ok(json!({"id": spec.id, "existing": true}));
            }
            let live: Vec<_> = map.values().filter(|i| i.report.lock().unwrap().state.is_live()).collect();
            if live.len() as u32 >= snap.node.max_sessions {
                bail!("node {} is at capacity ({} sessions)", self.node, snap.node.max_sessions);
            }
            if live.iter().filter(|i| i.agent == spec.agent).count() as u32 >= agent.cfg.limits.max_sessions {
                bail!("agent {} is at its max_sessions ({}) on node {}", spec.agent, agent.cfg.limits.max_sessions, self.node);
            }
        }
        let ws_spec = spec.workspace.clone().unwrap_or_else(|| agent.cfg.workspace.clone());
        let (workspace, ws_label) = self.workspace_dir(&spec.id, &ws_spec)?;
        let inst_dir = self.data_root.join("instances").join(&spec.id);
        let paths = InstancePaths { run: self.run_root.join(&spec.id), etc: inst_dir.join("etc"), home: inst_dir.join("home"), workspace };
        let cap = random_token("ocl_cap_");
        let password = random_token("");
        self.rt.prepare_dirs(&paths)?;
        let gw_port = self.rt.gateway_port(&spec.id)?;
        write_opencode_config(&snap, &agent, &spec, &paths, gw_port, &cap, self.rt.kind)?;

        let now = now_ms();
        let report = SessionReport {
            id: spec.id.clone(),
            node: self.node.clone(),
            agent: spec.agent.clone(),
            state: SessionState::Starting,
            opencode_id: None,
            tokens: TokenUsage::default(),
            todos: vec![],
            current_task: Some("starting".into()),
            last_text: None,
            error: None,
            model: Some(format!("{}/{}", agent.cfg.provider.id, agent.cfg.provider.model)),
            workspace: Some(ws_label),
            started_at: now,
            updated_at: now,
        };
        let inst = self.new_instance(&spec.id, &spec.agent, spec.role, paths.clone(), &password, report.clone());
        *inst.overrides.lock().unwrap() = spec.overrides.clone();
        self.store.put_instance(&InstanceRow {
            id: spec.id.clone(),
            report: report.clone(),
            cap: cap.clone(),
            password: password.clone(),
            role: role_str(spec.role).into(),
            last_seq: 0,
            spec_json: serde_json::to_string(&spec)?,
        })?;
        self.instances.lock().unwrap().insert(spec.id.clone(), inst.clone());
        self.start_gateway(&inst, &cap)?;
        self.link.emit(NodeEvent::Session(report));

        let image = agent.cfg.container.image.clone().unwrap_or_else(|| snap.node.runtime.image.clone());
        let launch = LaunchSpec {
            node: &self.node,
            id: &spec.id,
            agent: &spec.agent,
            image: &image,
            container: &agent.cfg.container,
            paths: &paths,
            password: &password,
        };
        if let Err(e) = self.rt.launch(&launch).await {
            self.fail(&inst, format!("launch failed: {e:#}"));
            return Err(e);
        }
        let me = self.clone();
        let title = spec.title.clone().unwrap_or_else(|| format!("{} ({})", spec.agent, spec.id));
        let prompt = spec.prompt.clone();
        tokio::spawn(async move {
            if let Err(e) = me.boot(&inst, &title, prompt).await {
                me.fail(&inst, format!("{e:#}"));
            }
        });
        Ok(json!({"id": spec.id}))
    }

    fn start_gateway(&self, inst: &Arc<Instance>, cap: &str) -> Result<()> {
        let ov = inst.overrides.lock().unwrap().clone();
        let gw = Gateway::new(&inst.id, &inst.agent, inst.role, cap, inst.usage.clone(), inst.parked.clone(), ov, &self.gw);
        *inst.gateway.lock().unwrap() = Some(gw.clone());
        let h = gw.serve(inst.paths.node_sock())?;
        inst.tasks.lock().unwrap().push(h);
        Ok(())
    }

    async fn boot(self: &Arc<Self>, inst: &Arc<Instance>, title: &str, prompt: Option<String>) -> Result<()> {
        inst.oc.wait_healthy(Duration::from_secs(120)).await?;
        inst.oc.wait_model(&inst.oc_provider, &inst.oc_model, Duration::from_secs(60)).await?;
        let oc_id = inst.oc.create_session(title).await.context("creating OpenCode session")?;
        *inst.oc_id.lock().unwrap() = Some(oc_id.clone());
        self.update(inst, |r| {
            r.opencode_id = Some(oc_id.clone());
            r.state = SessionState::Idle;
            r.current_task = None;
        });
        self.start_tail(inst);
        if let Some(p) = prompt {
            self.prompt(inst, &p, Delivery::Queue).await?;
        }
        let queued: Vec<_> = inst.pending_prompts.lock().unwrap().drain(..).collect();
        for (t, d) in queued {
            self.prompt(inst, &t, d).await?;
        }
        Ok(())
    }

    /// `queue`: OpenCode runs it after the current turn. `steer`: abort the
    /// current turn first, so the message takes effect now.
    async fn prompt(&self, inst: &Arc<Instance>, text: &str, delivery: Delivery) -> Result<()> {
        let Some(oc_id) = inst.oc_id.lock().unwrap().clone() else {
            inst.pending_prompts.lock().unwrap().push((text.to_string(), delivery));
            return Ok(());
        };
        if delivery == Delivery::Steer && inst.oc.busy(&oc_id).await.unwrap_or(false) {
            inst.oc.abort(&oc_id).await?;
            // The aborted turn must not be reported as finished work.
            for _ in 0..20 {
                if !inst.oc.busy(&oc_id).await.unwrap_or(false) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        inst.turn_open.store(true, Ordering::Relaxed);
        inst.oc.prompt(&oc_id, text, &inst.oc_provider, &inst.oc_model, &inst.oc_agent).await?;
        self.update(inst, |r| {
            r.state = SessionState::Running;
            r.error = None;
        });
        Ok(())
    }

    fn fail(&self, inst: &Arc<Instance>, msg: String) {
        tracing::warn!(session = %inst.id, error = %msg, "session failed");
        self.update(inst, |r| {
            r.state = SessionState::Error;
            r.error = Some(msg.clone());
        });
        self.link.emit(NodeEvent::TurnEnded { session: inst.id.clone(), last_text: None, todos: vec![], error: Some(msg) });
    }

    /// Mutate the report, persist it and emit a snapshot. Once an instance is
    /// stopping, late updates from in-flight tasks (a turn that ended just as
    /// the stop arrived) may not move it out of `stopped`.
    fn update(&self, inst: &Arc<Instance>, f: impl FnOnce(&mut SessionReport)) {
        let snap = {
            let mut r = inst.report.lock().unwrap();
            let before = r.state;
            f(&mut r);
            if inst.stopping.load(Ordering::Relaxed) && r.state != SessionState::Stopped {
                r.state = before;
            }
            r.tokens = *inst.usage.lock().unwrap();
            r.updated_at = now_ms();
            r.clone()
        };
        let _ = self.store.update_report(&snap, 0);
        self.link.emit(NodeEvent::Session(snap));
    }

    fn start_tail(self: &Arc<Self>, inst: &Arc<Instance>) {
        let (tx, mut rx) = mpsc::channel::<Value>(512);
        let reader = {
            let me = self.clone();
            let inst = inst.clone();
            tokio::spawn(async move {
                let path = inst.oc.events_path();
                while !inst.stopping.load(Ordering::Relaxed) {
                    // Anything missed while disconnected is recovered from history.
                    me.resync(&inst).await;
                    if let Err(e) = inst.oc.sse(&path, &tx).await {
                        tracing::debug!(session = %inst.id, error = %e, "event stream ended");
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            })
        };
        let handler = {
            let me = self.clone();
            let inst = inst.clone();
            tokio::spawn(async move {
                while let Some(ev) = rx.recv().await {
                    me.on_event(&inst, ev);
                }
            })
        };
        inst.tasks.lock().unwrap().extend([reader, handler]);
    }

    /// Rebuild usage, output, todos and state from OpenCode's own records.
    async fn resync(self: &Arc<Self>, inst: &Arc<Instance>) {
        let Some(oc_id) = inst.oc_id.lock().unwrap().clone() else { return };
        if let Ok(msgs) = inst.oc.messages(&oc_id).await {
            self.absorb_history(inst, &msgs);
        }
        let todos = inst.oc.todos(&oc_id).await.ok();
        let perms = inst.oc.pending_permissions().await.unwrap_or_default();
        let busy = inst.oc.busy(&oc_id).await.unwrap_or(false);
        self.update(inst, |r| {
            if let Some(t) = todos {
                r.todos = t;
                r.current_task = current_task(&r.todos);
            }
            if !perms.is_empty() {
                r.state = SessionState::Blocked;
            } else if busy {
                r.state = SessionState::Running;
            }
        });
        if !busy && perms.is_empty() && inst.turn_open.load(Ordering::Relaxed) {
            self.check_turn_end(inst);
        }
    }

    fn absorb_history(&self, inst: &Arc<Instance>, msgs: &[Value]) {
        let mut map = HashMap::new();
        let mut last_text = None;
        let mut last_error = None;
        for m in msgs {
            let info = &m["info"];
            if info["role"] != "assistant" {
                continue;
            }
            if let Some(id) = info["id"].as_str() {
                map.insert(id.to_string(), self.priced(&inst.agent, oc::message_usage(info)));
            }
            let text: Vec<&str> = m["parts"].as_array().into_iter().flatten().filter(|p| p["type"] == "text").filter_map(|p| p["text"].as_str()).collect();
            if !text.is_empty() {
                last_text = Some(text.join("\n"));
            }
            last_error = error_message(&info["error"]);
        }
        let total = map.values().fold(TokenUsage::default(), |mut a, u| {
            a.add(u);
            a
        });
        *inst.msg_usage.lock().unwrap() = map;
        *inst.usage.lock().unwrap() = total;
        self.update(inst, |r| {
            if last_text.is_some() {
                r.last_text = last_text;
            }
            if r.state != SessionState::Running {
                r.error = last_error;
            }
        });
    }

    fn priced(&self, agent: &str, mut u: TokenUsage) -> TokenUsage {
        if u.cost_usd == 0.0 {
            let snap = self.cfg.get();
            if let Some(p) = snap
                .agents
                .get(agent)
                .and_then(|a| snap.providers.get(&a.cfg.provider.id).and_then(|p| p.prices.get(&a.cfg.provider.model)))
            {
                u.cost_usd = (u.input as f64 * p.input_per_mtok + (u.output + u.reasoning) as f64 * p.output_per_mtok) / 1e6;
            }
        }
        u
    }

    fn on_event(self: &Arc<Self>, inst: &Arc<Instance>, ev: Value) {
        let ty = ev["type"].as_str().unwrap_or("");
        let p = &ev["properties"];
        let oc_id = inst.oc_id.lock().unwrap().clone();
        let sid = p["sessionID"].as_str().or(p["info"]["sessionID"].as_str()).or(p["part"]["sessionID"].as_str());
        if sid.is_some() && sid != oc_id.as_deref() {
            return; // another session in the same server (should not happen: one per container)
        }
        match ty {
            "session.status" => match p["status"]["type"].as_str() {
                Some("idle") => self.check_turn_end(inst),
                Some("retry") => {
                    // OpenCode backs off and retries failed provider calls by itself.
                    inst.turn_open.store(true, Ordering::Relaxed);
                    let st = &p["status"];
                    let wait = st["next"].as_i64().map(|n| ((n - now_ms()) / 1000).max(0)).unwrap_or(0);
                    let msg = format!(
                        "retrying (attempt {}, next in {wait}s): {}",
                        st["attempt"].as_u64().unwrap_or(0),
                        st["message"].as_str().unwrap_or("provider error").chars().take(200).collect::<String>()
                    );
                    // While parked, the countdown explains more than OpenCode's retry note.
                    let parked = inst.parked.lock().unwrap().clone();
                    let task = match &parked {
                        Some((until, why)) => format!("resets in {}: {why}", crate::gateway::human_in(until - now_ms())).chars().take(300).collect(),
                        None => msg,
                    };
                    self.update(inst, |r| {
                        r.state = if parked.is_some() { SessionState::Parked } else { SessionState::Running };
                        r.current_task = Some(task);
                    });
                }
                Some(_) => {
                    inst.turn_open.store(true, Ordering::Relaxed);
                    let changed = inst.report.lock().unwrap().state == SessionState::Idle;
                    if changed {
                        self.update(inst, |r| r.state = SessionState::Running);
                    }
                }
                None => {}
            },
            "session.idle" => self.check_turn_end(inst),
            "todo.updated" => {
                if let Ok(t) = serde_json::from_value::<Vec<Todo>>(p["todos"].clone()) {
                    self.update(inst, |r| {
                        r.todos = t;
                        r.current_task = current_task(&r.todos);
                    });
                }
            }
            "message.updated" if p["info"]["role"] == "assistant" => {
                let info = &p["info"];
                let Some(id) = info["id"].as_str() else { return };
                let u = self.priced(&inst.agent, oc::message_usage(info));
                let total = {
                    let mut m = inst.msg_usage.lock().unwrap();
                    m.insert(id.to_string(), u);
                    m.values().fold(TokenUsage::default(), |mut a, u| {
                        a.add(u);
                        a
                    })
                };
                let changed = *inst.usage.lock().unwrap() != total;
                *inst.usage.lock().unwrap() = total;
                let err = error_message(&info["error"]);
                if changed || err.is_some() {
                    self.update(inst, |r| {
                        if err.is_some() {
                            r.error = err;
                        }
                    });
                }
            }
            "message.part.updated" if p["part"]["type"] == "tool" => {
                let part = &p["part"];
                let tool = part["tool"].as_str().unwrap_or("?").to_string();
                let status = part["state"]["status"].as_str().unwrap_or("");
                let id = part["id"].as_str().unwrap_or("").to_string();
                match status {
                    "running" => {
                        let in_todo = current_task(&inst.report.lock().unwrap().todos);
                        if in_todo.is_none() {
                            self.update(inst, |r| r.current_task = Some(format!("tool: {tool}")));
                        }
                    }
                    "completed" | "error" => {
                        if inst.seen_tools.lock().unwrap().insert(id) {
                            let detail = json!({
                                "tool": tool,
                                "input": truncate_json(&part["state"]["input"]),
                                "error": part["state"]["error"],
                            });
                            let kind = if status == "error" { "tool.failed" } else { "tool.called" };
                            self.link.emit(NodeEvent::Activity { session: inst.id.clone(), kind: kind.into(), detail });
                        }
                    }
                    _ => {}
                }
            }
            "permission.asked" | "permission.updated" => {
                let what = format!("permission: {} {}", p["permission"].as_str().or(p["type"].as_str()).unwrap_or("?"), p["patterns"]);
                let req = p["id"].as_str().unwrap_or("").to_string();
                self.update(inst, |r| {
                    r.state = SessionState::Blocked;
                    r.current_task = Some(format!("{what} (request {req})"));
                });
                self.link.emit(NodeEvent::Activity { session: inst.id.clone(), kind: "permission.asked".into(), detail: p.clone() });
            }
            "permission.replied" => {
                self.update(inst, |r| {
                    if r.state == SessionState::Blocked {
                        r.state = SessionState::Running;
                    }
                    r.current_task = current_task(&r.todos);
                });
            }
            "session.error" => {
                let msg = error_message(&p["error"]).unwrap_or_else(|| "unknown error".into());
                self.update(inst, |r| r.error = Some(msg));
            }
            _ => {}
        }
    }

    fn check_turn_end(self: &Arc<Self>, inst: &Arc<Instance>) {
        let me = self.clone();
        let inst = inst.clone();
        tokio::spawn(async move { me.turn_ended(&inst).await });
    }

    /// Report the end of a turn once OpenCode confirms the session is idle.
    /// Triggered by `session.idle`/`session.status`, a resync, or the poll in
    /// `tick`; `turn_open` makes sure each turn is reported exactly once.
    async fn turn_ended(self: &Arc<Self>, inst: &Arc<Instance>) {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let Some(oc_id) = inst.oc_id.lock().unwrap().clone() else { return };
        if inst.oc.busy(&oc_id).await.unwrap_or(true) {
            return; // still running (or a queued prompt started)
        }
        if !inst.oc.pending_permissions().await.unwrap_or_default().is_empty() {
            return; // blocked, not finished
        }
        if !inst.turn_open.swap(false, Ordering::Relaxed) {
            return; // already reported
        }
        if let Ok(msgs) = inst.oc.messages(&oc_id).await {
            self.absorb_history(inst, &msgs);
        }
        if let Ok(t) = inst.oc.todos(&oc_id).await {
            self.update(inst, |r| r.todos = t);
        }
        if let Some(gw) = inst.gateway.lock().unwrap().clone() {
            gw.settle_tokens();
        }
        if inst.stopping.load(Ordering::Relaxed) {
            return; // stopped while we were collecting; not a finished turn
        }
        let (last_text, todos, error) = {
            let r = inst.report.lock().unwrap();
            (r.last_text.clone(), r.todos.clone(), r.error.clone())
        };
        self.update(inst, |r| {
            r.state = if r.error.is_some() { SessionState::Error } else { SessionState::Idle };
            r.current_task = current_task(&r.todos);
        });
        self.link.emit(NodeEvent::TurnEnded { session: inst.id.clone(), last_text, todos, error });
    }

    pub async fn stop(&self, id: &str, remove_workspace: bool) -> Result<()> {
        let inst = self.get(id)?;
        inst.stopping.store(true, Ordering::Relaxed);
        for t in inst.tasks.lock().unwrap().drain(..) {
            t.abort();
        }
        let _ = self.rt.stop(id).await;
        let _ = self.rt.remove(id).await;
        let _ = std::fs::remove_dir_all(&inst.paths.run);
        let ws_isolated = inst.report.lock().unwrap().workspace.as_deref().is_some_and(|w| w.starts_with("isolated:"));
        if remove_workspace && ws_isolated {
            let _ = std::fs::remove_dir_all(&inst.paths.workspace);
        }
        let _ = std::fs::remove_dir_all(self.data_root.join("instances").join(id));
        self.update(&inst, |r| {
            r.state = SessionState::Stopped;
            r.current_task = None;
        });
        self.instances.lock().unwrap().remove(id);
        self.store.delete_instance(id)?;
        Ok(())
    }

    /// Node restart: re-attach to containers that are still there.
    pub async fn reconcile(self: &Arc<Self>) -> Result<()> {
        let rows = self.store.instances()?;
        let found = self.rt.list(&self.node).await.unwrap_or_default();
        for row in rows {
            let f = found.iter().find(|f| f.id == row.id);
            let inst_dir = self.data_root.join("instances").join(&row.id);
            let ws = match row.report.workspace.as_deref() {
                Some(w) if w.starts_with("shared:") => self.data_root.join("workspaces").join(format!("shared-{}", &w[7..])),
                _ => self.data_root.join("workspaces").join(&row.id),
            };
            let paths = InstancePaths { run: self.run_root.join(&row.id), etc: inst_dir.join("etc"), home: inst_dir.join("home"), workspace: ws };
            let role = if row.role == "coordinator" { SessionRole::Coordinator } else { SessionRole::Worker };
            let inst = self.new_instance(&row.id, &row.report.agent, role, paths, &row.password, row.report.clone());
            if let Ok(spec) = serde_json::from_str::<SpawnSpec>(&row.spec_json) {
                *inst.overrides.lock().unwrap() = spec.overrides;
            }
            self.instances.lock().unwrap().insert(row.id.clone(), inst.clone());
            let Some(f) = f else {
                tracing::warn!(session = %row.id, "container gone; marking lost");
                self.update(&inst, |r| {
                    r.state = SessionState::Lost;
                    r.error = Some("container disappeared while the node was down".into());
                });
                self.instances.lock().unwrap().remove(&row.id);
                let _ = self.store.delete_instance(&row.id);
                continue;
            };
            std::fs::create_dir_all(&inst.paths.run)?;
            if let Err(e) = self.start_gateway(&inst, &row.cap) {
                tracing::warn!(session = %row.id, error = %e, "gateway restart failed");
            }
            if !f.running {
                tracing::info!(session = %row.id, "restarting stopped container");
                if let Err(e) = self.rt.start_existing(&row.id).await {
                    self.fail(&inst, format!("could not restart container: {e:#}"));
                    continue;
                }
            }
            let me = self.clone();
            tokio::spawn(async move {
                if let Err(e) = inst.oc.wait_healthy(Duration::from_secs(120)).await {
                    me.fail(&inst, format!("reattach: {e:#}"));
                    return;
                }
                if inst.oc_id.lock().unwrap().is_none() {
                    me.fail(&inst, "node restarted before the OpenCode session was created".into());
                    return;
                }
                tracing::info!(session = %inst.id, "re-attached");
                me.start_tail(&inst);
            });
        }
        Ok(())
    }

    /// Periodic housekeeping: idle/blocked safety net and wall-clock limits.
    pub async fn tick(self: &Arc<Self>) {
        let snap = self.cfg.get();
        let insts: Vec<_> = self.instances.lock().unwrap().values().cloned().collect();
        for inst in &insts {
            let state = inst.report.lock().unwrap().state;
            if !matches!(state, SessionState::Running | SessionState::Blocked | SessionState::Parked) {
                continue;
            }
            // Parked: show the countdown; leave once a request goes through again.
            let parked = inst.parked.lock().unwrap().clone();
            match parked {
                Some((until, why)) => {
                    let task = format!("resets in {}: {why}", crate::gateway::human_in(until - now_ms()));
                    self.update(inst, |r| {
                        r.state = SessionState::Parked;
                        r.current_task = Some(task.chars().take(300).collect());
                    });
                    continue;
                }
                None if state == SessionState::Parked => self.update(inst, |r| {
                    r.state = SessionState::Running;
                    r.current_task = current_task(&r.todos);
                }),
                None => {}
            }
            let perms = inst.oc.pending_permissions().await.unwrap_or_default();
            if !perms.is_empty() && state != SessionState::Blocked {
                let what = perms[0]["permission"].as_str().unwrap_or("?").to_string();
                let req = perms[0]["id"].as_str().unwrap_or("").to_string();
                self.update(inst, |r| {
                    r.state = SessionState::Blocked;
                    r.current_task = Some(format!("permission: {what} (request {req})"));
                });
                self.link.emit(NodeEvent::Activity { session: inst.id.clone(), kind: "permission.asked".into(), detail: perms[0].clone() });
            } else if perms.is_empty() && state == SessionState::Blocked {
                self.update(inst, |r| r.state = SessionState::Running);
            }
            if perms.is_empty() && inst.turn_open.load(Ordering::Relaxed) {
                self.turn_ended(inst).await;
            }
            let started = inst.report.lock().unwrap().started_at;
            if let Some(limit) = snap.agents.get(&inst.agent).and_then(|a| a.cfg.limits.wall_clock_secs) {
                if now_ms() - started > (limit as i64) * 1000 {
                    let oc_id = inst.oc_id.lock().unwrap().clone();
                    if let Some(oc_id) = oc_id {
                        let _ = inst.oc.abort(&oc_id).await;
                    }
                    self.update(inst, |r| r.error = Some(format!("wall-clock limit of {limit}s exceeded")));
                }
            }
        }
    }
}

fn error_message(e: &Value) -> Option<String> {
    if e.is_null() {
        return None;
    }
    Some(e["data"]["message"].as_str().or(e["message"].as_str()).map(String::from).unwrap_or_else(|| e.to_string()))
}

fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn role_str(r: SessionRole) -> &'static str {
    match r {
        SessionRole::Coordinator => "coordinator",
        SessionRole::Worker => "worker",
    }
}

fn current_task(todos: &[Todo]) -> Option<String> {
    todos.iter().find(|t| t.status == "in_progress").map(|t| t.content.clone())
}

fn truncate_json(v: &Value) -> Value {
    let s = v.to_string();
    if s.len() <= 500 {
        v.clone()
    } else {
        Value::String(format!("{}…", &s[..s.char_indices().nth(500).map(|(i, _)| i).unwrap_or(s.len())]))
    }
}

fn perm_json(p: &PermRule) -> Value {
    let s = |p: &Perm| match p {
        Perm::Allow => "allow",
        Perm::Ask => "ask",
        Perm::Deny => "deny",
    };
    match p {
        PermRule::Simple(x) => json!(s(x)),
        PermRule::Patterns { default, allow, ask, deny } => {
            let mut m = serde_json::Map::new();
            m.insert("*".into(), json!(s(default)));
            for (list, v) in [(allow, "allow"), (ask, "ask"), (deny, "deny")] {
                for pat in list {
                    m.insert(pat.clone(), json!(v));
                }
            }
            Value::Object(m)
        }
    }
}

/// Built-in cluster preamble, used unless the hub ships `preambles/cluster.md`.
/// Variables: {{instance.id}} {{template}} {{node}} {{parent}} {{task}}
/// {{budget}} {{model}} {{tools}} {{question_mode}}.
pub const DEFAULT_PREAMBLE: &str = r#"## Operating in opencode-cluster

You are instance `{{instance.id}}` of the `{{template}}` agent, running on node
`{{node}}` with model {{model}}. You answer to {{parent}}. Task: {{task}}.

- Keep a todo list with the `todowrite` tool from the start of every task and keep
  it current. Your work is judged by it: a task counts as done only when every
  item is `completed`. Mark items `cancelled` only if they truly cannot be done.
- Cluster tools available to you: {{tools}} (called as `cluster_<name>`).
  Messages from other agents and the operator may arrive between turns.
- Budget: {{budget}}. If a provider allowance runs out you may be paused until it
  resets; that is expected, just continue when you are resumed.
- Questions: the operator's mode is `{{question_mode}}`. Decide what you can
  yourself; ask (or tell whoever started you) only about what you cannot.
- Your workspace is the current directory. Other agents only see files you
  share explicitly or that live in a shared workspace.
- Never try to read or print credentials; you do not have any.
"#;

fn write_opencode_config(
    snap: &Snapshot,
    agent: &LoadedAgent,
    spec: &SpawnSpec,
    paths: &InstancePaths,
    gw_port: u16,
    cap: &str,
    kind: RuntimeKind,
) -> Result<()> {
    let a = &agent.cfg;
    let pdef = snap.providers.get(&a.provider.id).ok_or_else(|| anyhow!("unknown provider {}", a.provider.id))?;
    let pid = format!("ocl-{}", a.provider.id);
    let gw = format!("http://127.0.0.1:{gw_port}");
    let etc_in_runtime = match kind {
        RuntimeKind::Podman => "/etc/ocl".to_string(),
        RuntimeKind::Process => paths.etc.display().to_string(),
    };
    // Layered instructions: cluster preamble, template preamble, role prompt,
    // orchestration briefing (e.g. coordinator), the spawner's additions.
    let budget = spec.overrides.budget_usd.or(a.limits.budget_usd).map(|b| format!("${b:.2} for this session")).unwrap_or_else(|| "no fixed limit".into());
    let tools = crate::gateway::tool_names(spec.role, a.tools.cluster.as_deref()).join(", ");
    let vars: [(&str, String); 9] = [
        ("{{instance.id}}", spec.id.clone()),
        ("{{template}}", a.name.clone()),
        ("{{node}}", snap.node.name.clone()),
        ("{{parent}}", spec.parent.as_ref().map(|p| format!("session `{p}`")).unwrap_or_else(|| "the operator".into())),
        ("{{task}}", spec.task.map(|t| format!("#{t}")).unwrap_or_else(|| "none".into())),
        ("{{budget}}", budget),
        ("{{model}}", format!("{}/{}", a.provider.id, a.provider.model)),
        ("{{tools}}", tools),
        ("{{question_mode}}", "important_and_vital".into()),
    ];
    let render = |s: &str| vars.iter().fold(s.to_string(), |acc, (k, v)| acc.replace(k, v));
    let mut role = String::new();
    role.push_str(&render(snap.hub_preamble.as_deref().unwrap_or(DEFAULT_PREAMBLE)));
    role.push_str(&format!("\n# Role: {}\n\n{}\n\n", a.name, a.description));
    if let Some(p) = &a.role.preamble {
        role.push_str(&render(p));
        role.push_str("\n\n");
    }
    role.push_str(&agent.role_text);
    role.push('\n');
    if let Some(extra) = &spec.extra_instructions {
        role.push('\n');
        role.push_str(extra);
        role.push('\n');
    }
    if let Some(extra) = &spec.overrides.role_append {
        role.push_str("\n## Instructions for this instance\n\n");
        role.push_str(extra);
        role.push('\n');
    }
    std::fs::write(paths.etc.join("ROLE.md"), role)?;
    let cfg = json!({
        "$schema": "https://opencode.ai/config.json",
        "autoupdate": false,
        "share": "disabled",
        "provider": {
            pid.clone(): {
                "npm": pdef.api.npm_package(),
                "name": format!("{} via opencode-cluster", a.provider.id),
                "options": {"baseURL": format!("{gw}/llm/{}", a.provider.id), "apiKey": cap},
                "models": {
                    a.provider.model.clone(): {
                        "name": a.provider.model,
                        "tool_call": true,
                        "limit": {"context": a.provider.context.unwrap_or(128_000), "output": a.provider.max_output.unwrap_or(16_384)}
                    }
                }
            }
        },
        "model": format!("{pid}/{}", a.provider.model),
        "small_model": format!("{pid}/{}", a.provider.model),
        "permission": {"edit": perm_json(&a.tools.edit), "bash": perm_json(&a.tools.bash), "webfetch": perm_json(&a.tools.webfetch)},
        "mcp": {"cluster": {"type": "remote", "url": format!("{gw}/mcp"), "headers": {"Authorization": format!("Bearer {cap}")}, "enabled": true}},
        "instructions": [format!("{etc_in_runtime}/ROLE.md")],
        "plugin": a.plugins,
    });
    // OpenCode writes a .gitignore into its config dir at boot; the dir is
    // mounted read-only, so provide it up front.
    std::fs::write(paths.etc.join(".gitignore"), "node_modules\npackage.json\nbun.lock\n.gitignore\n")?;
    let path = paths.etc.join("opencode.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&cfg)?)?;
    {
        use std::os::unix::fs::PermissionsExt;
        // Readable by the container user (it holds only the capability token).
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))?;
    }
    Ok(())
}
