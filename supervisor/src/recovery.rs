//! Dead agents: retry, reassign or halt (PHASE1_5 §8), checkpoints and
//! harvests (§9).
//!
//! When an instance dies (its node reports `Died`, or the node itself is lost)
//! the hub prices each way back:
//! * **restart** in place: same node, same OpenCode home, history intact —
//!   the cost of re-reading the context;
//! * **resume elsewhere**: a fresh instance of the same template, workspace
//!   from the latest checkpoint (or a snapshot taken now), briefed with the
//!   original instructions, todos and last output;
//! * **handoff**: the parent spawns a replacement with its own summary;
//! * **abandon**.
//!
//! The cheapest runs by itself at most `auto_retry` times, unless it costs more
//! than `max_auto_cost_usd` or `max_auto_fraction` of what the task has spent
//! (for $0 providers: more than `keyless_quota_fraction` of the allowance
//! left). Otherwise the task tree is **halted**: every instance in it is
//! interrupted, harvested and stopped with its container kept, and the
//! operator picks an option; one call resumes everything.

use crate::state::{truncate, Actor, SpawnOpts, Sup};
use anyhow::{anyhow, bail, Result};
use ocl_protocol::api::StreamEvent;
use ocl_protocol::config::{DeathPolicy, WorkspaceMode, WorkspaceSpec};
use ocl_protocol::link::NodeCommand;
use ocl_protocol::model::*;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

const LONG: Duration = Duration::from_secs(300);

impl Sup {
    fn agent_info(&self, node: &str, agent: &str) -> Option<AgentInfo> {
        self.db.node(node).and_then(|n| n.agents.into_iter().find(|a| a.name == agent)).or_else(|| self.db.nodes().into_iter().flat_map(|n| n.agents).find(|a| a.name == agent))
    }

    pub fn death_policy(&self, s: &SessionInfo) -> DeathPolicy {
        self.agent_info(&s.report.node, &s.report.agent).and_then(|a| a.on_death).unwrap_or_else(|| self.cfg.on_death.clone())
    }

    /// The halt scope of a session and the sessions in it.
    fn scope_of(&self, s: &SessionInfo) -> (String, Vec<SessionInfo>) {
        let all = self.db.sessions(true);
        if let Some(t) = s.task_id {
            return (format!("task:{t}"), all.into_iter().filter(|x| x.task_id == Some(t)).collect());
        }
        if let Some(r) = &s.run_id {
            return (format!("run:{r}"), all.into_iter().filter(|x| x.run_id.as_deref() == Some(r)).collect());
        }
        // The whole tree: from the topmost ancestor down.
        let mut root = s.clone();
        while let Some(p) = root.parent.as_deref().and_then(|p| all.iter().find(|x| x.report.id == p)) {
            root = p.clone();
        }
        let mut tree = vec![root.clone()];
        let mut i = 0;
        while i < tree.len() {
            let id = tree[i].report.id.clone();
            tree.extend(all.iter().filter(|x| x.parent.as_deref() == Some(id.as_str())).cloned());
            i += 1;
        }
        (format!("session:{}", root.report.id), tree)
    }

    /// Smallest share of a provider allowance left on a node (keyless providers).
    fn allowance_left(&self, node: &str, provider: &str) -> Option<(u64, u64)> {
        self.quota_views()
            .into_iter()
            .filter(|q| q.provider == provider && q.kind == "requests" && q.nodes.iter().any(|n| n == node))
            .filter_map(|q| Some((q.remaining.unwrap_or(0), q.limit?)))
            .min_by_key(|(r, _)| *r)
    }

    /// Recovery options with estimated costs.
    pub fn recovery_options(&self, s: &SessionInfo) -> Vec<RecoveryOption> {
        let pol = self.death_policy(s);
        let a = self.agent_info(&s.report.node, &s.report.agent);
        let price = a.as_ref().map(|a| a.price_in_per_mtok).unwrap_or(0.0);
        let ctx = s.report.context_tokens.max(s.report.tokens.input / s.report.turns.max(1) as u64);
        let reread = ctx as f64 * price / 1e6;
        let node_up = self.nodes.connected(&s.report.node) && s.report.dead_reason.as_deref() != Some("node_lost");
        let cp = self.db.checkpoint(&s.report.id);
        let shared = s.report.workspace.as_deref().is_some_and(|w| w.starts_with("shared:"));
        let elsewhere = self.candidates(&s.report.agent);
        let parent_live = s.parent.as_deref().and_then(|p| self.db.session(p)).is_some_and(|p| p.report.state.is_live());
        let workspace_note = match (&cp, node_up, shared) {
            (_, _, true) => "workspace: the shared copy (synced from its other holders)".to_string(),
            (_, true, _) => "workspace: snapshot taken now from the dead instance's volume".to_string(),
            (Some(c), false, _) => format!("workspace: checkpoint from turn {} ({}); later changes are lost", c.turn, ago(c.at)),
            (None, false, _) => "workspace: no checkpoint; starts empty".to_string(),
        };
        let handoff_cost = pol.handoff_factor * s.report.tokens.cost_usd;
        vec![
            RecoveryOption {
                kind: "restart".into(),
                available: node_up,
                est_cost_usd: reread,
                est_requests: Some(1),
                note: if node_up { format!("same node {}, history intact; re-reads ~{ctx} tokens of context", s.report.node) } else { format!("node {} is not reachable", s.report.node) },
            },
            RecoveryOption {
                kind: "resume_elsewhere".into(),
                available: !elsewhere.is_empty(),
                est_cost_usd: reread,
                est_requests: Some(1),
                note: if elsewhere.is_empty() { format!("no connected node can run {} now", s.report.agent) } else { format!("on {} (best placement); {workspace_note}", elsewhere[0]) },
            },
            RecoveryOption {
                kind: "handoff".into(),
                available: parent_live,
                est_cost_usd: handoff_cost,
                est_requests: Some(((s.report.turns as f64) * pol.handoff_factor).ceil().max(1.0) as u64),
                note: if parent_live { format!("parent {} spawns a replacement with its own summary", s.parent.as_deref().unwrap_or("")) } else { "no live parent to hand off to".into() },
            },
            RecoveryOption { kind: "abandon".into(), available: true, est_cost_usd: 0.0, est_requests: None, note: "give up: the step/task fails with the death reason".into() },
        ]
    }

    /// A session died (`reason`). Recover automatically or halt.
    pub async fn on_died(self: &Arc<Self>, id: &str, reason: &str, detail: &str) {
        let _g = self.recovery_lock.lock().await;
        let Some(mut s) = self.db.session(id) else { return };
        if self.db.halts(true).iter().any(|h| h.dead_session == id || h.sessions.iter().any(|x| x == id)) {
            return; // already part of a halt
        }
        s.report.dead_reason = Some(reason.into());
        if s.report.error.is_none() {
            s.report.error = Some(format!("died: {reason}{}", if detail.is_empty() { String::new() } else { format!(" ({detail})") }));
        }
        self.save_session(&s);
        self.log("session.dead", Some(&s.report.node), Some(id), json!({"reason": reason, "detail": detail, "deaths": s.deaths}));
        if matches!(reason, "node_lost" | "container_gone") {
            // Nothing left to read on the node: record what the checkpoint holds.
            self.harvest_dead(&s, "node_lost").await;
        }
        let pol = self.death_policy(&s);
        let recent = self.db.events(0, Some(id), 10_000).into_iter().filter(|e| e.kind == "session.dead" && now_ms() - e.ts < 600_000).count() as u32;
        let crashloop = recent >= pol.crashloop_deaths;
        let opts = self.recovery_options(&s);
        let (scope, tree) = self.scope_of(&s);
        let spent: f64 = tree.iter().map(|x| x.report.tokens.cost_usd).sum();
        let provider = self.agent_info(&s.report.node, &s.report.agent).map(|a| a.provider).unwrap_or_default();
        let mut why_not = vec![];
        let best = opts.iter().filter(|o| o.available && o.kind != "abandon").min_by(|a, b| a.est_cost_usd.partial_cmp(&b.est_cost_usd).unwrap_or(std::cmp::Ordering::Equal)).cloned();
        let auto = match &best {
            _ if crashloop => {
                why_not.push(format!("crash loop: {recent} deaths in 10 min"));
                None
            }
            _ if reason == "wall_clock" => {
                why_not.push("it reached its wall-clock limit; more time is the operator's call".into());
                None
            }
            _ if s.deaths >= pol.auto_retry => {
                why_not.push(format!("already recovered automatically {} time(s) (auto_retry {})", s.deaths, pol.auto_retry));
                None
            }
            None => {
                why_not.push("no recovery option is available".into());
                None
            }
            Some(o) => {
                if o.est_cost_usd > pol.max_auto_cost_usd {
                    why_not.push(format!("{} would cost ~${:.4}, above max_auto_cost_usd ${:.2}", o.kind, o.est_cost_usd, pol.max_auto_cost_usd));
                }
                if spent > 0.0 && o.est_cost_usd > pol.max_auto_fraction * spent {
                    why_not.push(format!("{} would cost ~${:.4}, above {:.0}% of the ${spent:.4} spent so far", o.kind, o.est_cost_usd, pol.max_auto_fraction * 100.0));
                }
                if o.est_cost_usd == 0.0 {
                    if let (Some((left, limit)), Some(req)) = (self.allowance_left(&s.report.node, &provider), o.est_requests) {
                        if left == 0 || req as f64 > pol.keyless_quota_fraction * left as f64 {
                            why_not.push(format!("{} needs ~{req} request(s), above {:.0}% of the {left}/{limit} left", o.kind, pol.keyless_quota_fraction * 100.0));
                        }
                    }
                }
                why_not.is_empty().then(|| o.clone())
            }
        };
        if let Some(o) = auto {
            self.notify("recovery.auto", "info", format!("{} ({id}) died ({reason}); {}", s.report.agent, o.kind), format!("estimated ${:.4}: {}", o.est_cost_usd, o.note), Some(id.into()));
            self.log("recovery.auto", Some(&s.report.node), Some(id), json!({"option": o.kind, "est_cost_usd": o.est_cost_usd, "spent": spent}));
            match self.recover_session(&Actor::System, &s, &o.kind, None).await {
                Ok(_) => return,
                Err(e) => why_not.push(format!("{} failed: {e:#}", o.kind)),
            }
        }
        if let Err(e) = self.halt(&s, &scope, &tree, reason, spent, opts, &why_not).await {
            tracing::error!(session = id, error = %e, "halt failed");
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn halt(self: &Arc<Self>, dead: &SessionInfo, scope: &str, tree: &[SessionInfo], reason: &str, spent: f64, options: Vec<RecoveryOption>, why: &[String]) -> Result<Halt> {
        let mut halted = vec![];
        for x in tree.iter().filter(|x| x.report.id != dead.report.id && x.report.state.is_live()) {
            match self.nodes.call(&x.report.node, NodeCommand::Halt { session: x.report.id.clone() }, LONG).await {
                Ok(_) => halted.push(x.report.id.clone()),
                Err(e) => tracing::warn!(session = x.report.id, error = %e, "could not halt"),
            }
        }
        // The dead one: collect its outputs from the kept volumes (a lost
        // node's checkpoint was recorded when the death was noticed).
        if dead.report.state != SessionState::Lost {
            self.harvest_dead(dead, "death").await;
        }
        if let Some(t) = dead.task_id {
            if let Some(mut task) = self.db.task(t) {
                if task.status == TaskStatus::Running {
                    task.status = TaskStatus::Halted;
                    task.updated_at = now_ms();
                    task.notes.push(format!("[halt] {} ({}) died: {reason}", dead.report.agent, dead.report.id));
                    self.save_task(&task);
                }
            }
        }
        if let Some(r) = &dead.run_id {
            if let Some(mut run) = self.db.run(r) {
                if run.status == RunStatus::Running {
                    run.status = RunStatus::Halted;
                    run.updated_at = now_ms();
                    self.save_run(&run);
                }
            }
        }
        let h = Halt {
            id: ocl_protocol::new_id("h"),
            scope: scope.into(),
            dead_session: dead.report.id.clone(),
            reason: reason.into(),
            at: now_ms(),
            spent_usd: spent,
            current_todo: dead.report.todos.iter().find(|t| t.status == "in_progress").or_else(|| dead.report.todos.iter().find(|t| t.status == "pending")).map(|t| t.content.clone()),
            options,
            sessions: halted,
            status: "open".into(),
            resolution: None,
            resolved_at: None,
        };
        self.db.put_halt(&h)?;
        let _ = self.stream.send(StreamEvent::Halt(h.clone()));
        let opts: Vec<String> = h.options.iter().filter(|o| o.available).map(|o| format!("{} ~${:.4}", o.kind, o.est_cost_usd)).collect();
        self.notify(
            "task.halted",
            "vital",
            format!("{scope} halted: {} ({}) died ({reason})", dead.report.agent, dead.report.id),
            format!(
                "Was doing: {}. Spent so far ${spent:.4}. Not recovered automatically: {}. Options: {}. Resume with `ocl recover {} <option>`.",
                h.current_todo.as_deref().unwrap_or("-"),
                if why.is_empty() { "-".into() } else { why.join("; ") },
                opts.join(", "),
                h.id
            ),
            Some(h.id.clone()),
        );
        self.log("halt.created", Some(&dead.report.node), Some(&dead.report.id), json!({"halt": h.id, "scope": scope, "halted": h.sessions, "why": why}));
        Ok(h)
    }

    /// Operator: apply a recovery option to a halt (or directly to a dead session).
    pub async fn resolve(self: &Arc<Self>, actor: &Actor, target: &str, option: &str, node: Option<String>) -> Result<serde_json::Value> {
        let _g = self.recovery_lock.lock().await;
        let mut h = match self.db.halt(target) {
            Some(h) => h,
            None => {
                // A dead session without a halt (e.g. auto-recovery exhausted on a lone session).
                let s = self.session(target)?;
                if !matches!(s.report.state, SessionState::Dead | SessionState::Lost) {
                    bail!("{target} is neither an open halt nor a dead session");
                }
                let out = self.recover_session(actor, &s, option, node).await?;
                self.audit(actor, "session.recover", target, json!({"option": option}), true);
                return Ok(out);
            }
        };
        if h.status != "open" {
            bail!("halt {} is already {}", h.id, h.status);
        }
        let dead = self.session(&h.dead_session)?;
        let out = self.recover_session(actor, &dead, option, node).await?;
        let mut resumed = vec![];
        if option != "abandon" {
            for id in &h.sessions {
                if let Ok(s) = self.session(id) {
                    let prompt = format!(
                        "[cluster] Work was paused because {} ({}) died ({}). The operator chose `{option}`; continue where you left off.",
                        dead.report.agent, dead.report.id, h.reason
                    );
                    match self.nodes.call(&s.report.node, NodeCommand::Restart { session: id.clone(), prompt }, LONG).await {
                        Ok(_) => resumed.push(id.clone()),
                        Err(e) => tracing::warn!(session = id, error = %e, "resume after halt failed"),
                    }
                }
            }
            if let Some(t) = h.scope.strip_prefix("task:").and_then(|t| t.parse::<i64>().ok()) {
                if let Some(mut task) = self.db.task(t) {
                    if task.status == TaskStatus::Halted {
                        task.status = TaskStatus::Running;
                        task.notes.push(format!("[resumed] {option} by {actor}"));
                        task.updated_at = now_ms();
                        self.save_task(&task);
                    }
                }
            }
            if let Some(r) = h.scope.strip_prefix("run:").or(dead.run_id.as_deref()) {
                if let Some(mut run) = self.db.run(r) {
                    if run.status == RunStatus::Halted {
                        run.status = RunStatus::Running;
                        run.updated_at = now_ms();
                        self.save_run(&run);
                    }
                }
            }
        } else {
            for id in &h.sessions {
                let _ = self.stop(&Actor::System, id, false).await;
            }
            if let Some(t) = h.scope.strip_prefix("task:").and_then(|t| t.parse::<i64>().ok()) {
                if let Some(mut task) = self.db.task(t) {
                    // finish_task only moves running tasks.
                    task.status = TaskStatus::Running;
                    self.save_task(&task);
                }
                let _ = self.finish_task(actor, t, TaskStatus::Failed, format!("abandoned after {} died ({})", dead.report.agent, h.reason)).await;
            }
        }
        h.status = "resolved".into();
        h.resolution = Some(format!("{option} by {actor}"));
        h.resolved_at = Some(now_ms());
        self.db.put_halt(&h)?;
        let _ = self.stream.send(StreamEvent::Halt(h.clone()));
        self.audit(actor, "halt.resolve", &h.id, json!({"option": option, "resumed": resumed}), true);
        self.log("halt.resolved", None, Some(&dead.report.id), json!({"halt": h.id, "option": option, "resumed": resumed}));
        Ok(json!({"halt": h.id, "option": option, "result": out, "resumed": resumed}))
    }

    /// Carry out one recovery for a dead session.
    pub async fn recover_session(self: &Arc<Self>, actor: &Actor, s: &SessionInfo, option: &str, node: Option<String>) -> Result<serde_json::Value> {
        let opt = self.recovery_options(s).into_iter().find(|o| o.kind == option).ok_or_else(|| anyhow!("unknown option {option:?} (restart, resume_elsewhere, handoff, abandon)"))?;
        if !opt.available {
            bail!("{option} is not available: {}", opt.note);
        }
        let id = s.report.id.clone();
        let reason = s.report.dead_reason.clone().unwrap_or_else(|| "unknown".into());
        if option == "abandon" {
            // The turn-end logic below only moves running runs.
            if let Some(mut run) = s.run_id.as_deref().and_then(|r| self.db.run(r)).filter(|r| r.status == RunStatus::Halted) {
                run.status = RunStatus::Running;
                self.save_run(&run);
            }
        }
        let out = match option {
            "restart" => {
                let prompt = format!(
                    "[cluster] Your instance stopped unexpectedly ({reason}) and was restarted with your history intact. Check your todo list and continue where you left off."
                );
                self.nodes.call(&s.report.node, NodeCommand::Restart { session: id.clone(), prompt }, LONG).await?;
                let mut s2 = self.session(&id)?;
                s2.deaths += 1;
                s2.report.dead_reason = None;
                s2.report.error = None;
                self.save_session(&s2);
                json!({"restarted": id, "node": s.report.node})
            }
            "resume_elsewhere" => {
                let new = self.resume_elsewhere(actor, s, node).await?;
                json!({"replacement": new.report.id, "node": new.report.node})
            }
            "handoff" => {
                let parent = s.parent.clone().ok_or_else(|| anyhow!("no parent"))?;
                let todos: Vec<String> = s.report.todos.iter().map(|t| format!("- [{}] {}", t.status, t.content)).collect();
                let msg = format!(
                    "[cluster] Session {id} ({}) that you started died ({reason}) and will not be restarted. Start a replacement with cluster_spawn_agent, and give it a summary of what is done and what it should continue with.\nIts todo list:\n{}\nIts last output:\n{}",
                    s.report.agent,
                    if todos.is_empty() { "(none)".into() } else { todos.join("\n") },
                    s.report.last_text.as_deref().map(|t| truncate(t, 2000)).unwrap_or_else(|| "(none)".into())
                );
                self.message(&Actor::System, &parent, &msg, Delivery::Queue).await?;
                json!({"handed_to": parent})
            }
            "abandon" => {
                let me = self.clone();
                let (sid, todos, text) = (id.clone(), s.report.todos.clone(), s.report.last_text.clone());
                let why = format!("abandoned after dying ({reason})");
                // Drives the normal turn-end logic: the run step fails, a coordinator's task fails.
                tokio::spawn(async move { me.on_turn_ended(&sid, text, todos, Some(why)).await });
                json!({"abandoned": id})
            }
            _ => unreachable!(),
        };
        self.log("recovery.applied", Some(&s.report.node), Some(&id), json!({"option": option, "by": actor.to_string(), "result": out}));
        if option != "restart" && self.nodes.connected(&s.report.node) && s.report.state != SessionState::Lost {
            // It will not run again: stop it so its outputs are harvested and
            // its volumes follow `retain:` (after resume_elsewhere they were
            // already copied into the replacement).
            let me = self.clone();
            let trigger = if option == "abandon" { "abandoned" } else { "replaced" };
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let _ = me.stop_with(&Actor::System, &id, false, trigger).await;
            });
        }
        Ok(out)
    }

    /// A fresh instance of the same template, briefed and with the workspace restored.
    async fn resume_elsewhere(self: &Arc<Self>, actor: &Actor, s: &SessionInfo, node: Option<String>) -> Result<SessionInfo> {
        let id = &s.report.id;
        let mut notes = vec![];
        let workspace = match s.report.workspace.as_deref() {
            Some(w) if w.starts_with("shared:") => Some(WorkspaceSpec { mode: WorkspaceMode::Shared, name: Some(w[7..].to_string()), seed: None }),
            _ => {
                // Isolated: snapshot the dead volume if its node is up, else the checkpoint.
                let label = format!("isolated:{id}");
                let snap = if self.nodes.connected(&s.report.node) {
                    self.ws_snapshot(&s.report.node, &label, Some(id), "resume").await.ok().and_then(|v| v["reference"].as_str().map(String::from))
                } else {
                    None
                };
                let seed = match snap {
                    Some(r) => {
                        notes.push(format!("workspace from a snapshot of the dead instance ({r})"));
                        Some(r)
                    }
                    None => match self.db.checkpoint(id).and_then(|c| c.workspace_ref.map(|r| (r, c.turn, c.at))) {
                        Some((r, turn, at)) => {
                            notes.push(format!("workspace from the turn-{turn} checkpoint {r} ({}); later changes are lost", ago(at)));
                            Some(r)
                        }
                        None => {
                            notes.push("no checkpoint: the workspace starts empty".into());
                            None
                        }
                    },
                };
                Some(WorkspaceSpec { mode: WorkspaceMode::Isolated, name: None, seed })
            }
        };
        let todos: Vec<String> = s.report.todos.iter().map(|t| format!("- [{}] {}", t.status, t.content)).collect();
        let prompt = format!(
            "[cluster] You are taking over from session {id} ({}), which died on node {} ({}). {}.\n\nIts original instructions:\n{}\n\nIts todo list when it died (recreate it with todowrite and continue):\n{}\n\nIts last output:\n{}\n\nCheck the workspace, then finish the remaining work.",
            s.report.agent,
            s.report.node,
            s.report.dead_reason.as_deref().unwrap_or("unknown"),
            notes.join("; "),
            s.prompt.as_deref().unwrap_or("(not recorded)"),
            if todos.is_empty() { "(none)".into() } else { todos.join("\n") },
            s.report.last_text.as_deref().map(|t| truncate(t, 2000)).unwrap_or_else(|| "(none)".into()),
        );
        let new = self
            .spawn(
                actor,
                SpawnOpts {
                    agent: s.report.agent.clone(),
                    prompt: Some(prompt),
                    node,
                    title: s.title.clone().map(|t| format!("{t} (resumed)")),
                    workspace,
                    parent: s.parent.clone(),
                    task_id: s.task_id,
                    run_id: s.run_id.clone(),
                    step: s.step.clone(),
                    role: s.role,
                    replaces: Some(id.clone()),
                    ..Default::default()
                },
            )
            .await?;
        // Point everything that referred to the dead session at its replacement.
        if let Some(r) = &s.run_id {
            if let Some(mut run) = self.db.run(r) {
                for st in run.steps.iter_mut().filter(|st| &st.session == id) {
                    st.session = new.report.id.clone();
                }
                self.save_run(&run);
            }
        }
        if let Some(t) = s.task_id {
            if let Some(mut task) = self.db.task(t) {
                if task.coordinator_session.as_deref() == Some(id.as_str()) {
                    task.coordinator_session = Some(new.report.id.clone());
                    self.save_task(&task);
                }
            }
        }
        for mut c in self.db.sessions(true).into_iter().filter(|c| c.parent.as_deref() == Some(id.as_str())) {
            c.parent = Some(new.report.id.clone());
            self.save_session(&c);
        }
        Ok(new)
    }

    // ------------------------------------------------------ harvests

    /// Harvest a dead/lost session: from its kept volumes if the node is up,
    /// otherwise record what the latest checkpoint holds.
    pub async fn harvest_dead(&self, s: &SessionInfo, trigger: &str) {
        if self.nodes.connected(&s.report.node) && s.report.state != SessionState::Lost {
            if let Err(e) = self.nodes.call(&s.report.node, NodeCommand::Harvest { session: s.report.id.clone(), trigger: trigger.into() }, LONG).await {
                tracing::warn!(session = s.report.id, error = %e, "harvest of a dead instance failed");
            }
            return;
        }
        let cp = self.db.checkpoint(&s.report.id);
        let h = Harvest {
            session: s.report.id.clone(),
            node: s.report.node.clone(),
            at: now_ms(),
            trigger: "node_lost".into(),
            outputs: cp
                .as_ref()
                .and_then(|c| c.workspace_ref.clone())
                .map(|r| HarvestedOutput { pattern: "(checkpoint)".into(), path: ".".into(), reference: r, size: 0, kind: "tree".into(), to: "artifact".into(), dest: None, delivered: None })
                .into_iter()
                .collect(),
            transcript_ref: cp.as_ref().and_then(|c| c.transcript_ref.clone()),
            missing: vec![],
            from_checkpoint: true,
            checkpoint_at: cp.as_ref().map(|c| c.at),
            notes: vec![match &cp {
                Some(c) => format!("node {} is gone; outputs come from the turn-{} checkpoint; anything written after {} is unrecoverable", s.report.node, c.turn, fmt_ts(c.at)),
                None => format!("node {} is gone and there is no checkpoint: outputs are unrecoverable", s.report.node),
            }],
        };
        self.on_harvested(h).await;
    }

    /// A node harvested an instance: deliver outputs to their destinations.
    pub async fn on_harvested(&self, mut h: Harvest) {
        let s = self.db.session(&h.session);
        for o in &mut h.outputs {
            let target: Option<(String, String)> = match o.to.as_str() {
                "parent_workspace" => s.as_ref().and_then(|s| s.parent.as_deref()).and_then(|p| self.db.session(p)).and_then(|p| Some((p.report.node.clone(), p.report.workspace.clone()?))),
                "task_workspace" => s.as_ref().and_then(|s| s.task_id).map(|t| format!("shared:task-{t}")).and_then(|w| self.ws_home(&w).map(|n| (n, w))),
                t if t.starts_with("workspace:") => {
                    let w = format!("shared:{}", &t[10..]);
                    self.ws_home(&w).map(|n| (n, w))
                }
                _ => None,
            };
            let Some((node, ws)) = target else {
                if o.to != "artifact" {
                    o.delivered = Some(format!("not delivered: no {} to deliver to (kept as artifact)", o.to));
                }
                continue;
            };
            // Files keep their workspace path by default; trees already hold relative paths.
            let dest = o.dest.clone().unwrap_or_else(|| if o.kind == "tree" { String::new() } else { o.path.clone() });
            match self.nodes.call(&node, NodeCommand::PlaceArtifact { workspace: ws.clone(), reference: o.reference.clone(), dest: dest.clone() }, LONG).await {
                Ok(_) => o.delivered = Some(format!("{ws} on {node}: {dest}")),
                Err(e) => o.delivered = Some(format!("delivery to {ws} on {node} failed: {e:#}")),
            }
        }
        let _ = self.db.add_harvest(&h);
        self.log(
            "harvest",
            Some(&h.node),
            Some(&h.session),
            json!({"trigger": h.trigger, "outputs": h.outputs.iter().map(|o| json!({"path": o.path, "ref": o.reference, "to": o.to, "delivered": o.delivered})).collect::<Vec<_>>(), "transcript": h.transcript_ref, "missing": h.missing, "from_checkpoint": h.from_checkpoint, "notes": h.notes}),
        );
    }

    /// The node holding a shared workspace: a live session's node, else the
    /// last publisher, else the last node that ran a session in it (shared
    /// workspace directories stay after their sessions stop).
    fn ws_home(&self, label: &str) -> Option<String> {
        let sessions = self.db.sessions(true);
        let live = sessions.iter().find(|s| s.report.state.is_live() && s.report.workspace.as_deref() == Some(label)).map(|s| s.report.node.clone());
        live.or_else(|| self.db.workspace(label.strip_prefix("shared:")?).and_then(|i| i["latest_node"].as_str().map(String::from)))
            .or_else(|| sessions.iter().find(|s| s.report.workspace.as_deref() == Some(label)).map(|s| s.report.node.clone()))
            .filter(|n| self.nodes.connected(n))
    }
}

fn ago(ts: Ts) -> String {
    let s = (now_ms() - ts) / 1000;
    match s {
        s if s < 60 => format!("{s}s ago"),
        s if s < 3600 => format!("{}m ago", s / 60),
        s => format!("{}h ago", s / 3600),
    }
}

fn fmt_ts(ts: Ts) -> String {
    let secs = ts / 1000;
    format!("{:02}:{:02}:{:02} UTC", secs / 3600 % 24, secs / 60 % 60, secs % 60)
}
