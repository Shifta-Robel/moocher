//! Supervisor core: session bookkeeping, placement, messaging and the cluster
//! tool calls agents make over MCP. Workflows and the task bucket live in
//! `orch.rs` and build on these primitives.

use crate::db::Db;
use crate::link::Registry;
use anyhow::{anyhow, bail, Result};
use ocl_protocol::api::*;
use ocl_protocol::config::{SupervisorConfig, WorkflowConfig, WorkspaceMode, WorkspaceSpec};
use ocl_protocol::link::*;
use ocl_protocol::model::*;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

pub const CMD_TIMEOUT: Duration = Duration::from_secs(60);

pub struct Sup {
    pub db: Db,
    pub cfg: SupervisorConfig,
    pub config_dir: PathBuf,
    pub nodes: Registry,
    pub stream: broadcast::Sender<StreamEvent>,
    /// Serialises orchestration decisions (workflow steps, task scheduling).
    pub orch_lock: tokio::sync::Mutex<()>,
}

/// Who performed an action, for the audit log.
#[derive(Debug, Clone)]
pub enum Actor {
    Operator(String),
    Session(String),
    System,
}

impl std::fmt::Display for Actor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Actor::Operator(n) => write!(f, "operator:{n}"),
            Actor::Session(s) => write!(f, "session:{s}"),
            Actor::System => write!(f, "system"),
        }
    }
}

/// Everything needed to create a session.
#[derive(Debug, Clone, Default)]
pub struct SpawnOpts {
    pub agent: String,
    pub prompt: Option<String>,
    pub node: Option<String>,
    pub title: Option<String>,
    pub workspace: Option<WorkspaceSpec>,
    pub parent: Option<String>,
    pub task_id: Option<i64>,
    pub run_id: Option<String>,
    pub step: Option<String>,
    pub role: SessionRole,
    pub extra_instructions: Option<String>,
}

impl Sup {
    pub fn new(db: Db, cfg: SupervisorConfig, config_dir: PathBuf) -> Arc<Sup> {
        let (stream, _) = broadcast::channel(1024);
        Arc::new(Sup { db, cfg, config_dir, nodes: Registry::default(), stream, orch_lock: tokio::sync::Mutex::new(()) })
    }

    pub fn audit(&self, actor: &Actor, action: &str, target: &str, detail: Value, ok: bool) {
        if let Err(e) = self.db.audit(&actor.to_string(), action, target, &detail, ok) {
            tracing::error!(error = %e, "audit write failed");
        }
        tracing::info!(target: "audit", actor = %actor, action, target, ok, "control action");
    }

    pub fn publish_node(&self, n: NodeInfo) {
        let _ = self.stream.send(StreamEvent::Node(n));
    }

    pub fn save_session(&self, s: &SessionInfo) {
        let _ = self.db.put_session(s);
        let _ = self.stream.send(StreamEvent::Session(s.clone()));
    }

    pub fn save_task(&self, t: &Task) {
        let _ = self.db.put_task(t);
        let _ = self.stream.send(StreamEvent::Task(t.clone()));
    }

    pub fn save_run(&self, r: &WorkflowRun) {
        let _ = self.db.put_run(r);
        let _ = self.stream.send(StreamEvent::Run(r.clone()));
    }

    pub fn log(&self, kind: &str, node: Option<&str>, session: Option<&str>, data: Value) {
        if let Ok(ev) = self.db.log(kind, node, session, &data) {
            let _ = self.stream.send(StreamEvent::Log(ev));
        }
    }

    /// Merge a node's session report, keeping supervisor-side metadata.
    pub fn merge_report(&self, report: SessionReport) -> SessionInfo {
        let info = match self.db.session(&report.id) {
            Some(mut s) => {
                if s.report.state != report.state {
                    self.log("session.state", Some(&report.node), Some(&report.id), json!({"from": s.report.state, "to": report.state}));
                }
                s.report = report;
                s
            }
            None => SessionInfo {
                report,
                title: None,
                task_id: None,
                parent: None,
                run_id: None,
                step: None,
                verdict: None,
                role: SessionRole::Worker,
            },
        };
        self.save_session(&info);
        info
    }

    pub fn workflows(&self) -> Vec<WorkflowConfig> {
        ocl_protocol::config::load_dir::<WorkflowConfig>(&self.config_dir.join("workflows"))
            .into_iter()
            .filter_map(|(p, r)| match r {
                Ok(w) => Some(w),
                Err(e) => {
                    tracing::warn!(file = %p.display(), error = %e, "bad workflow file");
                    None
                }
            })
            .collect()
    }

    // --------------------------------------------------------- placement

    fn live_on(&self, node: &str) -> u32 {
        self.db.sessions_on(node).iter().filter(|s| s.report.state.is_live()).count() as u32
    }

    /// Nodes that can run `agent` now, least loaded first.
    pub fn candidates(&self, agent: &str) -> Vec<String> {
        let mut c: Vec<(u32, String)> = self
            .db
            .nodes()
            .into_iter()
            .filter(|n| n.connected && self.nodes.connected(&n.name))
            .filter(|n| n.agents.iter().any(|a| a.name == agent && a.secret_ok && a.errors.is_empty()))
            .map(|n| (self.live_on(&n.name), n))
            .filter(|(live, n)| *live < n.max_sessions)
            .map(|(live, n)| (live, n.name))
            .collect();
        c.sort();
        c.into_iter().map(|(_, n)| n).collect()
    }

    pub fn place(&self, agent: &str, hint: Option<&str>) -> Result<String> {
        let c = self.candidates(agent);
        if let Some(h) = hint {
            if c.iter().any(|n| n == h) {
                return Ok(h.to_string());
            }
            bail!("node {h} cannot run agent {agent} right now (not connected, agent missing/misconfigured, or at capacity)");
        }
        c.into_iter().next().ok_or_else(|| anyhow!("no connected node can run agent {agent:?} right now"))
    }

    // ---------------------------------------------------------- sessions

    pub async fn spawn(&self, actor: &Actor, o: SpawnOpts) -> Result<SessionInfo> {
        let res = self.spawn_inner(&o).await;
        self.audit(
            actor,
            "session.spawn",
            res.as_ref().map(|s| s.report.id.as_str()).unwrap_or("-"),
            json!({"agent": o.agent, "node": o.node, "title": o.title, "task": o.task_id, "run": o.run_id, "error": res.as_ref().err().map(|e| format!("{e:#}"))}),
            res.is_ok(),
        );
        res
    }

    async fn spawn_inner(&self, o: &SpawnOpts) -> Result<SessionInfo> {
        let node = self.place(&o.agent, o.node.as_deref())?;
        let id = ocl_protocol::new_id("s");
        let now = now_ms();
        let info = SessionInfo {
            report: SessionReport {
                id: id.clone(),
                node: node.clone(),
                agent: o.agent.clone(),
                state: SessionState::Starting,
                opencode_id: None,
                tokens: TokenUsage::default(),
                todos: vec![],
                current_task: Some("starting".into()),
                last_text: None,
                error: None,
                model: None,
                workspace: None,
                started_at: now,
                updated_at: now,
            },
            title: o.title.clone(),
            task_id: o.task_id,
            parent: o.parent.clone(),
            run_id: o.run_id.clone(),
            step: o.step.clone(),
            verdict: None,
            role: o.role,
        };
        self.save_session(&info);
        let spec = SpawnSpec {
            id: id.clone(),
            agent: o.agent.clone(),
            prompt: o.prompt.clone(),
            workspace: o.workspace.clone(),
            title: o.title.clone(),
            extra_instructions: o.extra_instructions.clone(),
            role: o.role,
        };
        if let Err(e) = self.nodes.call(&node, NodeCommand::Spawn(spec), CMD_TIMEOUT).await {
            let mut s = self.db.session(&id).unwrap_or(info);
            s.report.state = SessionState::Error;
            s.report.error = Some(format!("{e:#}"));
            self.save_session(&s);
            return Err(e);
        }
        Ok(self.db.session(&id).unwrap_or(info))
    }

    pub fn session(&self, id: &str) -> Result<SessionInfo> {
        self.db.session(id).ok_or_else(|| anyhow!("no such session {id}"))
    }

    pub async fn message(&self, actor: &Actor, id: &str, text: &str, delivery: Delivery) -> Result<()> {
        let s = self.session(id)?;
        let res = async {
            if delivery == Delivery::Inbox {
                let from = match actor {
                    Actor::Session(s) => s.clone(),
                    other => other.to_string(),
                };
                return self.db.inbox_push(id, &from, text);
            }
            if !s.report.state.is_live() {
                bail!("session {id} is {}", s.report.state.as_str());
            }
            self.nodes.call(&s.report.node, NodeCommand::Prompt { session: id.into(), text: text.into(), delivery }, CMD_TIMEOUT).await?;
            Ok(())
        }
        .await;
        self.audit(actor, "session.message", id, json!({"delivery": delivery, "chars": text.len(), "error": res.as_ref().err().map(|e| e.to_string())}), res.is_ok());
        res
    }

    pub async fn interrupt(&self, actor: &Actor, id: &str) -> Result<()> {
        let s = self.session(id)?;
        let res = self.nodes.call(&s.report.node, NodeCommand::Interrupt { session: id.into() }, CMD_TIMEOUT).await.map(|_| ());
        self.audit(actor, "session.interrupt", id, json!({}), res.is_ok());
        res
    }

    pub async fn stop(&self, actor: &Actor, id: &str, remove_workspace: bool) -> Result<()> {
        let mut s = self.session(id)?;
        let res = if self.nodes.connected(&s.report.node) {
            self.nodes.call(&s.report.node, NodeCommand::Stop { session: id.into(), remove_workspace }, CMD_TIMEOUT).await.map(|_| ())
        } else if s.report.state.is_live() || s.report.state == SessionState::Unreachable {
            Err(anyhow!("node {} is not connected", s.report.node))
        } else {
            Ok(())
        };
        if res.is_ok() {
            s.report.state = SessionState::Stopped;
            s.report.updated_at = now_ms();
            self.save_session(&s);
        }
        self.audit(actor, "session.stop", id, json!({"remove_workspace": remove_workspace}), res.is_ok());
        res
    }

    pub async fn permission_reply(&self, actor: &Actor, id: &str, request: &str, reply: &str) -> Result<()> {
        if !matches!(reply, "once" | "always" | "reject") {
            bail!("reply must be once, always or reject");
        }
        let s = self.session(id)?;
        let res = self
            .nodes
            .call(&s.report.node, NodeCommand::PermissionReply { session: id.into(), request: request.into(), reply: reply.into() }, CMD_TIMEOUT)
            .await
            .map(|_| ());
        self.audit(actor, "session.permission", id, json!({"request": request, "reply": reply}), res.is_ok());
        res
    }

    pub async fn exec(&self, id: &str, command: &str, timeout_secs: u64) -> Result<(i64, String)> {
        let s = self.session(id)?;
        let v = self
            .nodes
            .call(&s.report.node, NodeCommand::Exec { session: id.into(), command: command.into(), timeout_secs }, Duration::from_secs(timeout_secs + 30))
            .await?;
        Ok((v["code"].as_i64().unwrap_or(-1), v["output"].as_str().unwrap_or("").to_string()))
    }

    // ------------------------------------------------------- node events

    pub async fn on_node_event(self: &Arc<Self>, node: &str, ev: NodeEvent) {
        match ev {
            NodeEvent::Session(r) => {
                self.merge_report(r);
            }
            NodeEvent::Agents { agents } => {
                if let Some(mut n) = self.db.node(node) {
                    n.agents = agents;
                    let _ = self.db.put_node(&n);
                    self.publish_node(n);
                }
                self.log("node.agents", Some(node), None, json!({}));
            }
            NodeEvent::Activity { session, kind, detail } => {
                self.log(&kind, Some(node), Some(&session), detail);
            }
            NodeEvent::TurnEnded { session, last_text, todos, error } => {
                self.log(
                    "session.turn_ended",
                    Some(node),
                    Some(&session),
                    json!({"todos_done": todos.iter().filter(|t| t.status == "completed").count(), "todos": todos.len(), "error": error, "output": last_text.as_deref().map(|t| truncate(t, 400))}),
                );
                let me = self.clone();
                // Orchestration may call back into nodes; never block the link reader.
                tokio::spawn(async move { me.on_turn_ended(&session, last_text, todos, error).await });
            }
        }
    }

    // --------------------------------------------------- agent tool calls

    pub async fn agent_call(self: &Arc<Self>, node: &str, from: &str, call: AgentCall) -> Result<Value> {
        let caller = self.session(from)?;
        if caller.report.node != node {
            bail!("session {from} does not belong to node {node}");
        }
        if call.coordinator_only() && caller.role != SessionRole::Coordinator {
            bail!("only coordinator sessions may use this tool");
        }
        let actor = Actor::Session(from.to_string());
        match call {
            AgentCall::Send { to, text, delivery } => {
                let target = self.session(&to)?;
                let msg = format!("[message from session {from} ({})]\n{text}", caller.report.agent);
                self.message(&actor, &target.report.id, &msg, delivery).await?;
                Ok(json!(format!("delivered to {to} ({:?})", delivery)))
            }
            AgentCall::Inbox => Ok(Value::Array(self.db.inbox_take(from)?)),
            AgentCall::ListAgents => {
                let mut out: Vec<Value> = vec![];
                for n in self.db.nodes().into_iter().filter(|n| n.connected) {
                    for a in n.agents.iter().filter(|a| a.secret_ok && a.errors.is_empty()) {
                        out.push(json!({"agent": a.name, "description": a.description, "model": format!("{}/{}", a.provider, a.model), "node": n.name, "tier": a.tier}));
                    }
                }
                Ok(Value::Array(out))
            }
            AgentCall::ListSessions => Ok(Value::Array(self.db.sessions(false).iter().filter(|s| s.report.state.is_live()).map(session_brief).collect())),
            AgentCall::SessionStatus { session } => {
                let s = self.session(&session)?;
                let mut v = session_brief(&s);
                v["output"] = json!(s.report.last_text.as_deref().map(|t| truncate(t, 4000)));
                v["todo_list"] = json!(s.report.todos);
                v["error"] = json!(s.report.error);
                Ok(v)
            }
            AgentCall::Spawn { agent, prompt, node: hint, title, share_workspace } => {
                let (workspace, node_hint) = if share_workspace {
                    let ws = caller.report.workspace.as_deref().and_then(|w| w.strip_prefix("shared:")).map(|n| WorkspaceSpec { mode: WorkspaceMode::Shared, name: Some(n.into()) });
                    if ws.is_none() {
                        bail!("your workspace is not shareable");
                    }
                    // A shared workspace exists on one node only (cross-node sync: Phase 3).
                    (ws, Some(caller.report.node.clone()))
                } else {
                    (None, hint)
                };
                let s = self
                    .spawn(
                        &actor,
                        SpawnOpts {
                            agent,
                            prompt: Some(prompt),
                            node: node_hint,
                            title,
                            workspace,
                            parent: Some(from.into()),
                            task_id: caller.task_id,
                            ..Default::default()
                        },
                    )
                    .await?;
                Ok(json!({"session": s.report.id, "node": s.report.node, "note": "You will receive a message when it finishes a turn."}))
            }
            AgentCall::Stop { session } => {
                let s = self.session(&session)?;
                if s.parent.as_deref() != Some(from) {
                    bail!("you can only stop sessions you spawned");
                }
                self.stop(&actor, &session, false).await?;
                Ok(json!("stopped"))
            }
            AgentCall::TaskGet => {
                let t = caller.task_id.and_then(|id| self.db.task(id)).ok_or_else(|| anyhow!("no task attached to this session"))?;
                Ok(serde_json::to_value(t)?)
            }
            AgentCall::TaskNote { note } => {
                let mut t = caller.task_id.and_then(|id| self.db.task(id)).ok_or_else(|| anyhow!("no task attached to this session"))?;
                t.notes.push(note);
                t.updated_at = now_ms();
                self.save_task(&t);
                Ok(json!("noted"))
            }
            AgentCall::TaskComplete { summary } => {
                let id = caller.task_id.ok_or_else(|| anyhow!("no task attached"))?;
                self.finish_task(&actor, id, TaskStatus::Done, summary).await?;
                Ok(json!("task marked done; your session will be stopped"))
            }
            AgentCall::TaskFail { reason } => {
                let id = caller.task_id.ok_or_else(|| anyhow!("no task attached"))?;
                self.finish_task(&actor, id, TaskStatus::Failed, reason).await?;
                Ok(json!("task marked failed; your session will be stopped"))
            }
        }
    }

    pub fn overview(&self) -> Overview {
        let sessions = self.db.sessions(false);
        let mut totals = TokenUsage::default();
        for s in self.db.sessions(true) {
            totals.add(&s.report.tokens);
        }
        Overview { nodes: self.db.nodes(), sessions, tasks: self.db.tasks(), totals }
    }

    /// Housekeeping: sessions on long-disconnected nodes become `lost`.
    pub fn expire_unreachable(&self) {
        let grace = self.cfg.disconnect_grace_secs as i64 * 1000;
        for n in self.db.nodes().into_iter().filter(|n| !n.connected && now_ms() - n.last_seen > grace) {
            for mut s in self.db.sessions_on(&n.name) {
                if s.report.state == SessionState::Unreachable {
                    s.report.state = SessionState::Lost;
                    s.report.error = Some(format!("node {} unreachable for more than {}s", n.name, self.cfg.disconnect_grace_secs));
                    s.report.updated_at = now_ms();
                    self.save_session(&s);
                }
            }
        }
    }
}

pub fn session_brief(s: &SessionInfo) -> Value {
    json!({
        "session": s.report.id,
        "agent": s.report.agent,
        "node": s.report.node,
        "state": s.report.state,
        "title": s.title,
        "current_task": s.report.current_task,
        "todos": format!("{}/{}", s.report.todos.iter().filter(|t| t.status == "completed").count(), s.report.todos.len()),
        "tokens": s.report.tokens.total(),
        "cost_usd": s.report.tokens.cost_usd,
        "parent": s.parent,
    })
}

pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}
