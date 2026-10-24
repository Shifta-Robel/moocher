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
    /// Cluster CA (set at start when remote nodes are enabled).
    pub ca: std::sync::OnceLock<crate::pki::Ca>,
    /// Last audit time per (node, rejection reason): a rejected node retries
    /// for as long as it runs, which should not flood the audit log.
    pub rejections: std::sync::Mutex<std::collections::HashMap<String, Ts>>,
    pub stats: std::sync::Mutex<crate::resources::StatsMem>,
    pub blobs: crate::artifacts::BlobStore,
    /// Serialises death handling and halt resolution.
    pub recovery_lock: tokio::sync::Mutex<()>,
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
    pub overrides: ocl_protocol::config::SpawnOverrides,
    /// Resume elsewhere: the dead session this one replaces.
    pub replaces: Option<String>,
}

impl Sup {
    pub fn new(db: Db, cfg: SupervisorConfig, config_dir: PathBuf) -> Arc<Sup> {
        let (stream, _) = broadcast::channel(1024);
        let data = cfg.data_dir.clone().unwrap_or_else(|| config_dir.join("supervisor"));
        Arc::new(Sup {
            db,
            cfg,
            config_dir,
            nodes: Registry::default(),
            stream,
            orch_lock: tokio::sync::Mutex::new(()),
            ca: std::sync::OnceLock::new(),
            rejections: Default::default(),
            stats: Default::default(),
            blobs: crate::artifacts::BlobStore::new(data.join("artifacts")),
            recovery_lock: tokio::sync::Mutex::new(()),
        })
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
            Some(s) if s.report.state == SessionState::Stopped && report.state != SessionState::Stopped => {
                // `stopped` is terminal: a late report from before the stop must not revive it.
                tracing::debug!(session = %report.id, state = report.state.as_str(), "ignoring report for stopped session");
                return s;
            }
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
                placement: None,
                waiting_on: vec![],
                deaths: 0,
                replaces: None,
                prompt: None,
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
        let near = crate::placement::Near {
            parent_node: o.parent.as_deref().and_then(|p| self.db.session(p)).map(|p| p.report.node),
            workspace: o.workspace.as_ref().filter(|w| w.mode == WorkspaceMode::Shared).and_then(|w| w.name.clone()),
        };
        // Cluster-wide instance cap from the template.
        let max = self.db.nodes().iter().flat_map(|n| n.agents.iter()).filter(|a| a.name == o.agent).filter_map(|a| a.max_instances).min();
        if let Some(max) = max {
            let live = self.db.sessions(false).iter().filter(|s| s.report.agent == o.agent && s.report.state.is_live()).count() as u32;
            if live >= max {
                bail!("agent {} already has {live} live instance(s), its template allows {max}", o.agent);
            }
        }
        let (node, why) = self.place(&o.agent, o.node.as_deref(), &near)?;
        // A shared workspace that lives on other machines: bring this node's copy up to date.
        let mut why = why;
        if let Some(name) = o.workspace.as_ref().filter(|w| w.mode == WorkspaceMode::Shared).and_then(|w| w.name.clone()) {
            let notes = self.ws_prepare(&name, &node).await;
            if !notes.is_empty() {
                why.push_str(&format!("; workspace {name}: {}", notes.join(", ")));
            }
        }
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
                context_tokens: 0,
                dead_reason: None,
                turns: 0,
            },
            title: o.title.clone(),
            task_id: o.task_id,
            parent: o.parent.clone(),
            run_id: o.run_id.clone(),
            step: o.step.clone(),
            verdict: None,
            role: o.role,
            placement: Some(why),
            waiting_on: vec![],
            deaths: 0,
            replaces: o.replaces.clone(),
            prompt: o.prompt.as_deref().map(|p| truncate(p, 8000)),
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
            overrides: o.overrides.clone(),
            parent: o.parent.clone(),
            task: o.task_id,
            question_mode: Some(self.question_mode(o.task_id)),
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
        self.stop_with(actor, id, remove_workspace, "stop").await
    }

    /// Stop (and harvest) a session; `trigger` is recorded in the harvest.
    pub async fn stop_with(&self, actor: &Actor, id: &str, remove_workspace: bool, trigger: &str) -> Result<()> {
        let mut s = self.session(id)?;
        let res = if self.nodes.connected(&s.report.node) {
            // Harvesting may upload outputs: allow more than a plain command.
            self.nodes
                .call(&s.report.node, NodeCommand::Stop { session: id.into(), remove_workspace, trigger: Some(trigger.into()) }, Duration::from_secs(300))
                .await
                .map(|_| ())
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
        if res.is_ok() {
            if let Some(mut q) = self.db.question_by_request(id, request) {
                if q.status == QuestionStatus::Open {
                    q.status = if reply == "reject" { QuestionStatus::Rejected } else { QuestionStatus::Answered };
                    q.answer = Some(reply.into());
                    q.answered_by = Some(actor.to_string());
                    q.updated_at = now_ms();
                    let _ = self.db.put_question(&q);
                    let _ = self.stream.send(StreamEvent::Question(q));
                }
            }
        }
        res
    }

    pub async fn preamble(&self, id: &str) -> Result<String> {
        let s = self.session(id)?;
        let v = self.nodes.call(&s.report.node, NodeCommand::ReadPreamble { session: id.into() }, CMD_TIMEOUT).await?;
        Ok(v["preamble"].as_str().unwrap_or_default().to_string())
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
            NodeEvent::Egress { ips } => {
                let _ = self.db.put_egress(node, &ips);
                let summary: Vec<Value> = ips.iter().map(|e| json!({"route": e.route, "ip": e.ip, "source": e.source, "error": e.error})).collect();
                self.log("node.egress", Some(node), None, json!({"ips": summary}));
                let _ = self.stream.send(StreamEvent::Egress { node: node.into(), ips });
            }
            NodeEvent::RouteStats { stats } => {
                let _ = self.db.put_route_stats(node, &stats);
            }
            NodeEvent::Quotas { buckets } => {
                let _ = self.db.put_quotas(node, &buckets);
                let _ = self.stream.send(StreamEvent::Quotas { node: node.into() });
            }
            NodeEvent::QuestionAsked { session, request, kind, text, options, multiple, detail } => {
                let me = self.clone();
                let node = node.to_string();
                tokio::spawn(async move {
                    let Ok(asker) = me.session(&session) else { return };
                    let context = match kind {
                        QuestionKind::Permission => Some(format!("OpenCode permission request {request}: {}", truncate(&detail.to_string(), 600))),
                        _ => None,
                    };
                    if let Err(e) = me.ask(&asker, kind, text, options, multiple, Importance::Normal, context, Some(request)).await {
                        tracing::warn!(node, session, error = %e, "could not route question");
                    }
                });
            }
            NodeEvent::QuestionGone { session, request } => self.question_gone(&session, &request),
            NodeEvent::BudgetExhausted { session, budget_usd, spent_usd } => {
                let me = self.clone();
                tokio::spawn(async move {
                    let Ok(asker) = me.session(&session) else { return };
                    let open = me.db.questions(true).into_iter().any(|q| q.asker == session && q.kind == QuestionKind::Budget);
                    if open {
                        return;
                    }
                    let text = format!("Session {session} ({}) used its whole budget (${spent_usd:.4} of ${budget_usd:.2}). Raise the budget (e.g. `raise 2.00`) or stop it?", asker.report.agent);
                    let options = vec![
                        QuestionOption { label: format!("raise {:.2}", budget_usd * 2.0), description: "double the budget and continue".into() },
                        QuestionOption { label: "stop".into(), description: "stop the session (its outputs are harvested)".into() },
                    ];
                    let _ = me.ask(&asker, QuestionKind::Budget, text, options, false, Importance::Vital, Some(format!("budget={budget_usd}")), None).await;
                });
            }
            NodeEvent::Died { session, reason, detail } => {
                let me = self.clone();
                tokio::spawn(async move { me.on_died(&session, &reason, &detail).await });
            }
            NodeEvent::Checkpoint(c) => {
                if let Some(name) = self.db.session(&c.session).and_then(|s| s.report.workspace).and_then(|w| w.strip_prefix("shared:").map(String::from)) {
                    if let Some(r) = &c.workspace_ref {
                        let mut info = self.db.workspace(&name).unwrap_or_else(|| json!({"name": name, "version": 0, "nodes": [node]}));
                        info["latest_ref"] = json!(r);
                        info["latest_node"] = json!(node);
                        info["updated_at"] = json!(now_ms());
                        let _ = self.db.put_workspace(&name, &info);
                    }
                }
                self.log("checkpoint", Some(node), Some(&c.session), json!({"turn": c.turn, "workspace": c.workspace_ref, "transcript": c.transcript_ref}));
                let _ = self.db.put_checkpoint(&c);
            }
            NodeEvent::Harvested(h) => {
                let me = self.clone();
                tokio::spawn(async move { me.on_harvested(h).await });
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
                        out.push(json!({
                            "agent": a.name,
                            "description": a.description,
                            "model": format!("{}/{}", a.provider, a.model),
                            "node": n.name,
                            "tier": a.tier,
                            "source": a.source,
                            "max_instances": a.max_instances,
                            "overridable": a.overridable,
                        }));
                    }
                }
                Ok(Value::Array(out))
            }
            AgentCall::ListNodes => Ok(self.nodes_for_agents()),
            AgentCall::ListSessions => Ok(Value::Array(self.db.sessions(false).iter().filter(|s| s.report.state.is_live()).map(session_brief).collect())),
            AgentCall::SessionStatus { session } => {
                let s = self.session(&session)?;
                let mut v = session_brief(&s);
                v["output"] = json!(s.report.last_text.as_deref().map(|t| truncate(t, 4000)));
                v["todo_list"] = json!(s.report.todos);
                v["error"] = json!(s.report.error);
                Ok(v)
            }
            AgentCall::Spawn { agent, prompt, node: hint, title, share_workspace, overrides } => {
                let (workspace, node_hint) = if share_workspace {
                    let ws = caller.report.workspace.as_deref().and_then(|w| w.strip_prefix("shared:")).map(|n| WorkspaceSpec { mode: WorkspaceMode::Shared, name: Some(n.into()), seed: None });
                    if ws.is_none() {
                        bail!("your workspace is not shareable");
                    }
                    // Placement prefers the node that holds the workspace; on
                    // another node it is synced through the artifact store.
                    (ws, hint)
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
                            overrides,
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
            AgentCall::Ask { question, options, importance, context } => {
                if question.trim().is_empty() {
                    bail!("question must not be empty");
                }
                let opts = options.into_iter().map(|o| QuestionOption { label: o, description: String::new() }).collect();
                let q = self.ask(&caller, QuestionKind::Ask, question, opts, false, importance.unwrap_or_default(), context, None).await?;
                let note = match q.status {
                    QuestionStatus::Decided => format!("Nobody will answer it (mode {}, importance {}): decide yourself and state your assumption.", q.mode.as_str(), q.importance.as_str()),
                    _ if q.holder == crate::questions::OPERATOR => "It went to the operator. Continue with other work if you can; the answer arrives as a message.".to_string(),
                    _ => format!("It went to {} (the agent that started you). Continue with other work if you can; the answer arrives as a message.", q.holder),
                };
                Ok(json!({"question": q.id, "importance": q.importance, "raised_because": q.raised_because, "holder": q.holder, "note": note}))
            }
            AgentCall::Answer { id, answer } => {
                let q = self.answer(&actor, &id, ocl_protocol::api::AnswerRequest { answer: Some(answer), choice: None, let_agent_decide: false, reject: false }).await?;
                Ok(json!({"question": q.id, "status": q.status, "delivered_to": q.asker}))
            }
            AgentCall::Escalate { id, note } => {
                let q = self.db.question(&id).ok_or_else(|| anyhow!("no such question {id}"))?;
                if q.holder != from {
                    bail!("question {id} is held by {}, not by you", q.holder);
                }
                let q = self.escalate(&id, from, note).await?;
                Ok(json!({"question": q.id, "status": q.status, "now_held_by": q.holder}))
            }
            AgentCall::Questions => Ok(Value::Array(self.questions_held(from))),
            AgentCall::ReportUp { message } => {
                let text = format!("[report from session {from} ({})]\n{message}", caller.report.agent);
                match caller.parent.as_deref().and_then(|p| self.db.session(p)).filter(|p| p.report.state.is_live()) {
                    Some(p) => {
                        self.message(&actor, &p.report.id, &text, Delivery::Queue).await?;
                        Ok(json!(format!("reported to {}", p.report.id)))
                    }
                    None => {
                        // Top of the chain: the operator reads it (task note + notification).
                        if let Some(mut t) = caller.task_id.and_then(|t| self.db.task(t)) {
                            t.notes.push(format!("[report {from}] {}", truncate(&message, 2000)));
                            t.updated_at = now_ms();
                            self.save_task(&t);
                        }
                        self.notify("agent.report", "info", format!("report from {} ({from})", caller.report.agent), truncate(&message, 1000), Some(from.into()));
                        self.log("agent.report", Some(node), Some(from), json!({"message": truncate(&message, 2000)}));
                        Ok(json!("reported to the operator"))
                    }
                }
            }
            AgentCall::LogAssumption { text } => {
                let a = Assumption {
                    id: 0,
                    ts: now_ms(),
                    task_id: caller.task_id,
                    run_id: caller.run_id.clone(),
                    session: from.into(),
                    question: None,
                    decided_by: from.into(),
                    text: truncate(&text, 4000),
                    mode: self.question_mode(caller.task_id),
                };
                let id = self.db.add_assumption(&a)?;
                self.log("assumption.logged", Some(node), Some(from), json!({"id": id}));
                Ok(json!(format!("assumption #{id} logged for the operator to review")))
            }
            AgentCall::ArtifactList { query, task_only } => {
                let task = if task_only { caller.task_id } else { None };
                let v: Vec<Value> = self
                    .db
                    .artifacts(query.as_deref(), task, 100)
                    .into_iter()
                    .map(|a| json!({"ref": a.reference, "name": a.name, "kind": a.kind, "size": a.size, "producer": a.producer, "task": a.task_id, "tags": a.tags}))
                    .collect();
                Ok(Value::Array(v))
            }
            AgentCall::ArtifactInfo { reference } => self.artifact_view(&reference),
            AgentCall::SyncWorkspace => self.ws_sync_for(&caller).await,
        }
    }

    /// Nodes with `running` recomputed from live sessions (the stored value is
    /// only refreshed when a node attaches).
    pub fn node_views(&self) -> Vec<NodeInfo> {
        let mut nodes = self.db.nodes();
        for n in &mut nodes {
            n.running = self.live_on(&n.name);
            n.link_rtt_ms = self.nodes.rtt(&n.name);
        }
        nodes
    }

    pub fn overview(&self) -> Overview {
        let sessions = self.db.sessions(false);
        let mut totals = TokenUsage::default();
        for s in self.db.sessions(true) {
            totals.add(&s.report.tokens);
        }
        Overview { nodes: self.node_views(), sessions, tasks: self.db.tasks(), totals }
    }

    /// Housekeeping: sessions on long-disconnected nodes become `lost`, which
    /// counts as a death (`node_lost`) for the recovery policy.
    pub fn expire_unreachable(self: &Arc<Self>) {
        let grace = self.cfg.disconnect_grace_secs as i64 * 1000;
        for n in self.db.nodes().into_iter().filter(|n| !n.connected && now_ms() - n.last_seen > grace) {
            for mut s in self.db.sessions_on(&n.name) {
                if s.report.state == SessionState::Unreachable {
                    s.report.state = SessionState::Lost;
                    s.report.dead_reason = Some("node_lost".into());
                    s.report.error = Some(format!("node {} unreachable for more than {}s", n.name, self.cfg.disconnect_grace_secs));
                    s.report.updated_at = now_ms();
                    self.save_session(&s);
                    let me = self.clone();
                    let id = s.report.id.clone();
                    let detail = format!("node {} unreachable for more than {}s", n.name, self.cfg.disconnect_grace_secs);
                    tokio::spawn(async move { me.on_died(&id, "node_lost", &detail).await });
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
        "waiting_on": s.waiting_on,
    })
}

pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(id: &str, state: SessionState) -> SessionReport {
        SessionReport {
            id: id.into(),
            node: "hub".into(),
            agent: "builder".into(),
            state,
            opencode_id: None,
            tokens: TokenUsage::default(),
            todos: vec![],
            current_task: None,
            last_text: None,
            error: None,
            model: None,
            workspace: None,
            started_at: 0,
            updated_at: 0,
            context_tokens: 0,
            dead_reason: None,
            turns: 0,
        }
    }

    #[test]
    fn late_report_does_not_revive_stopped_session() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let sup = Sup::new(db, SupervisorConfig::default(), dir.path().into());
        sup.merge_report(report("s-1", SessionState::Running));
        sup.merge_report(report("s-1", SessionState::Stopped));
        // A turn that ended just as the stop arrived reports idle afterwards.
        sup.merge_report(report("s-1", SessionState::Idle));
        assert_eq!(sup.session("s-1").unwrap().report.state, SessionState::Stopped);
        // Other transitions are unaffected.
        sup.merge_report(report("s-2", SessionState::Running));
        sup.merge_report(report("s-2", SessionState::Idle));
        assert_eq!(sup.session("s-2").unwrap().report.state, SessionState::Idle);
    }
}
