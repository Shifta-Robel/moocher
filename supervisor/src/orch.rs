//! Orchestration: workflow runs (turn-based handoffs) and the task bucket.
//!
//! Task bucket semantics (operator-facing):
//! * `urgent`    — started as soon as a task slot is free (one extra slot beyond
//!                 `max_concurrent_tasks` is reserved for urgent work).
//! * `when_free` — started only when no task is running and every session in
//!                 the cluster has been idle for `idle_grace_secs`.
//! * `on_demand` — never started automatically; the operator presses Run.
//! Within a label, lower `rank` goes first (the UI reorders by rewriting ranks).
//!
//! Starting a task spawns a *coordinator* session (the "main agent") with the
//! task as its prompt and coordinator-only cluster tools (spawn_agent,
//! task_complete, ...). The coordinator decides how to split the work across
//! agents. A task tagged `workflow:<name>` runs that workflow instead.

use crate::state::{truncate, Actor, SpawnOpts, Sup};
use anyhow::{anyhow, bail, Result};
use moocher_protocol::api::*;
use moocher_protocol::config::*;
use moocher_protocol::model::*;
use serde_json::json;
use std::sync::Arc;

const COORDINATOR_BRIEFING: &str = r#"
## You are the coordinator for one task

The operator's task is in your first message. You coordinate other agents; do
the work through them where it helps, or yourself if it is small.

1. Plan with `todowrite` (keep it current; it is how the operator tracks you).
2. `cluster_list_agents` shows the agent profiles you can start.
3. `cluster_spawn_agent` starts one with a prompt (set share_workspace=true if
   it must see or produce files in your workspace). Tell it exactly what to do
   and what "done" means.
4. You get a message whenever an agent you started finishes a turn. Inspect
   results with `cluster_session_status`, reply with `cluster_send_message`.
5. Finish with `cluster_task_complete` (summary of the outcome) or
   `cluster_task_fail` (reason). Do not stop before calling one of them.
"#;

impl Sup {
    // ================================================================ turns

    pub async fn on_turn_ended(self: &Arc<Self>, id: &str, last_text: Option<String>, todos: Vec<Todo>, error: Option<String>) {
        let _g = self.orch_lock.lock().await;
        let Some(mut s) = self.db.session(id) else { return };
        if s.run_id.is_none() {
            s.verdict = Some(if error.is_some() { Verdict::Fail } else { todo_verdict(&todos, &[], true) });
            self.save_session(&s);
        }
        // Other machines working in the same shared workspace get this turn's changes.
        self.ws_after_turn(&s).await;
        if let Some(run) = s.run_id.clone() {
            if let Err(e) = self.advance_run(&run, &s, last_text.clone(), &todos, error.clone()).await {
                tracing::warn!(run, error = %e, "workflow step handling failed");
                self.log("run.error", None, Some(id), json!({"run": run, "error": format!("{e:#}")}));
            }
        }
        if s.role == SessionRole::Coordinator {
            self.coordinator_idle(&s, error.as_deref()).await;
        }
        if let Some(parent) = s.parent.clone() {
            let done = todos.iter().filter(|t| t.status == "completed").count();
            let msg = format!(
                "[cluster] Session {} ({}{}) finished a turn. Todos {}/{} completed.{}\nOutput:\n{}",
                s.report.id,
                s.report.agent,
                s.title.as_deref().map(|t| format!(", \"{t}\"")).unwrap_or_default(),
                done,
                todos.len(),
                error.as_deref().map(|e| format!(" ERROR: {e}")).unwrap_or_default(),
                last_text.as_deref().map(|t| truncate(t, 3000)).unwrap_or_else(|| "(no text)".into())
            );
            if let Ok(p) = self.session(&parent) {
                if p.report.state.is_live() {
                    if let Err(e) = self.message(&Actor::System, &parent, &msg, Delivery::Queue).await {
                        tracing::warn!(parent, error = %e, "could not notify parent session");
                    }
                }
            }
        }
    }

    // ============================================================ workflows

    pub async fn start_run(self: &Arc<Self>, actor: &Actor, workflow: &str, input: &str, task_id: Option<i64>) -> Result<WorkflowRun> {
        let wf = self.workflows().into_iter().find(|w| w.name == workflow).ok_or_else(|| anyhow!("no workflow named {workflow:?}"))?;
        let first = wf.steps.first().ok_or_else(|| anyhow!("workflow {workflow} has no steps"))?.clone();
        // Prefer one node that has every step's agent (no workspace copies).
        // Otherwise each step is placed on its own and the run's shared
        // workspace follows it through the artifact store.
        let mut node = first.node.clone();
        if node.is_none() {
            let mut cands = self.candidates(&first.agent);
            for st in &wf.steps {
                let c = self.candidates(&st.agent);
                cands.retain(|n| c.contains(n) || st.node.as_deref() == Some(n.as_str()));
            }
            node = cands.into_iter().next();
            for st in &wf.steps {
                if st.node.is_none() && self.candidates(&st.agent).is_empty() {
                    bail!("no connected node can run step {} (agent {})", st.name, st.agent);
                }
            }
        }
        let id = moocher_protocol::new_id("r");
        let ws_name = format!("run-{}", &id[2..]);
        let now = now_ms();
        let mut run = WorkflowRun {
            id: id.clone(),
            workflow: workflow.into(),
            status: RunStatus::Running,
            input: input.into(),
            steps: vec![],
            workspace: ws_name,
            created_at: now,
            updated_at: now,
            task_id,
        };
        self.save_run(&run);
        self.audit(actor, "workflow.run", &id, json!({"workflow": workflow, "node": node}), true);
        self.log("run.started", node.as_deref(), None, json!({"run": id, "workflow": workflow}));
        if let Err(e) = self.start_step(&wf, &mut run, &first.name, None, node).await {
            run.status = RunStatus::Failed;
            run.updated_at = now_ms();
            self.save_run(&run);
            return Err(e);
        }
        Ok(run)
    }

    async fn start_step(&self, wf: &WorkflowConfig, run: &mut WorkflowRun, step_name: &str, feedback: Option<String>, node: Option<String>) -> Result<()> {
        let step = wf.steps.iter().find(|s| s.name == step_name).ok_or_else(|| anyhow!("workflow {} has no step {step_name}", wf.name))?;
        let prev = run.steps.last();
        let prev_todos = prev
            .and_then(|p| self.db.session(&p.session))
            .map(|s| s.report.todos.iter().map(|t| format!("- [{}] {}", t.status, t.content)).collect::<Vec<_>>().join("\n"))
            .unwrap_or_default();
        let mut prompt = step
            .prompt
            .replace("{{input}}", &run.input)
            .replace("{{prev.output}}", prev.and_then(|p| p.output.as_deref()).unwrap_or(""))
            .replace("{{prev.step}}", prev.map(|p| p.step.as_str()).unwrap_or(""))
            .replace("{{prev.todos}}", &prev_todos)
            .replace("{{run.id}}", &run.id)
            .replace("{{feedback}}", feedback.as_deref().unwrap_or(""));
        let success = step.success.clone().unwrap_or_default();
        if !success.todos.is_empty() {
            prompt.push_str("\n\nYour todo list must contain these items (add your own as needed) and you are done when all are completed:\n");
            for t in &success.todos {
                prompt.push_str(&format!("- {t}\n"));
            }
        }
        // Stay on the previous step's node when it can run this step too.
        let prev_node = run.steps.last().and_then(|p| self.db.session(&p.session)).map(|s| s.report.node).filter(|n| self.candidates(&step.agent).contains(n));
        let node = step.node.clone().or(node.filter(|n| self.candidates(&step.agent).contains(n))).or(prev_node);
        let s = self
            .spawn(
                &Actor::System,
                SpawnOpts {
                    agent: step.agent.clone(),
                    prompt: Some(prompt),
                    node,
                    title: Some(format!("{} · {}", wf.name, step.name)),
                    workspace: Some(wf.workspace.clone().unwrap_or(WorkspaceSpec { mode: WorkspaceMode::Shared, name: Some(run.workspace.clone()), seed: None })),
                    task_id: run.task_id,
                    run_id: Some(run.id.clone()),
                    step: Some(step.name.clone()),
                    ..Default::default()
                },
            )
            .await?;
        run.steps.push(StepRun {
            step: step.name.clone(),
            session: s.report.id.clone(),
            verdict: None,
            nudges: 0,
            output: None,
            started_at: now_ms(),
            finished_at: None,
            check_results: vec![],
        });
        run.updated_at = now_ms();
        self.save_run(run);
        self.log("run.step", Some(&s.report.node), Some(&s.report.id), json!({"run": run.id, "step": step.name}));
        Ok(())
    }

    async fn advance_run(self: &Arc<Self>, run_id: &str, s: &SessionInfo, last_text: Option<String>, todos: &[Todo], error: Option<String>) -> Result<()> {
        let mut run = self.db.run(run_id).ok_or_else(|| anyhow!("run {run_id} missing"))?;
        if run.status != RunStatus::Running {
            return Ok(());
        }
        let wf = self.workflows().into_iter().find(|w| w.name == run.workflow).ok_or_else(|| anyhow!("workflow {} no longer exists", run.workflow))?;
        let Some(idx) = run.steps.iter().rposition(|st| st.session == s.report.id && st.finished_at.is_none()) else { return Ok(()) };
        let step_cfg = wf.steps.iter().find(|x| x.name == run.steps[idx].step).cloned().ok_or_else(|| anyhow!("step vanished"))?;
        let success = step_cfg.success.clone().unwrap_or_default();

        // 1) todos
        let mut verdict = if error.is_some() { Verdict::Fail } else { todo_verdict(todos, &success.todos, success.todos_required) };
        if verdict == Verdict::Incomplete && run.steps[idx].nudges < success.max_nudges {
            run.steps[idx].nudges += 1;
            let open: Vec<String> = todos.iter().filter(|t| t.status != "completed").map(|t| format!("- [{}] {}", t.status, t.content)).collect();
            let missing: Vec<String> = success.todos.iter().filter(|w| !todos.iter().any(|t| t.content.to_lowercase().contains(&w.to_lowercase()))).map(|w| format!("- (missing) {w}")).collect();
            let msg = format!(
                "[cluster] You stopped with unfinished todos. Continue until every item is completed (or cancel items that cannot be done and say why):\n{}\n{}",
                open.join("\n"),
                missing.join("\n")
            );
            self.save_run(&run);
            self.message(&Actor::System, &s.report.id, &msg, Delivery::Queue).await?;
            self.log("run.nudge", Some(&s.report.node), Some(&s.report.id), json!({"run": run.id, "n": run.steps[idx].nudges}));
            return Ok(());
        }
        // 2) hard checks (only if todos passed)
        let mut check_results = vec![];
        if verdict == Verdict::Pass {
            for c in &success.checks {
                let (ok, note) = match c {
                    Check::TextContains(needle) => {
                        let ok = last_text.as_deref().is_some_and(|t| t.contains(needle.as_str()));
                        (ok, format!("text_contains {needle:?}: {}", if ok { "pass" } else { "fail" }))
                    }
                    Check::ExitCode(cmd) => match self.exec(&s.report.id, cmd, 600).await {
                        Ok((code, out)) => (code == 0, format!("exit_code `{cmd}` -> {code}\n{}", truncate(&out, 1500))),
                        Err(e) => (false, format!("exit_code `{cmd}` could not run: {e:#}")),
                    },
                };
                check_results.push(note);
                if !ok {
                    verdict = Verdict::Fail;
                }
            }
        }
        if verdict == Verdict::Incomplete {
            verdict = Verdict::Fail; // nudges exhausted
        }
        {
            let st = &mut run.steps[idx];
            st.verdict = Some(verdict);
            st.output = last_text.clone();
            st.finished_at = Some(now_ms());
            st.check_results = check_results.clone();
        }
        let mut sess = self.session(&s.report.id)?;
        sess.verdict = Some(verdict);
        self.save_session(&sess);
        self.log("run.verdict", Some(&s.report.node), Some(&s.report.id), json!({"run": run.id, "step": step_cfg.name, "verdict": verdict, "checks": check_results}));

        let pos = wf.steps.iter().position(|x| x.name == step_cfg.name).unwrap_or(0);
        let next = match verdict {
            Verdict::Pass => match step_cfg.next.as_deref() {
                Some("end") => None,
                Some(n) => Some((n.to_string(), None)),
                None => wf.steps.get(pos + 1).map(|x| (x.name.clone(), None)),
            },
            _ => step_cfg.on_fail.clone().map(|n| {
                let fb = format!(
                    "Feedback from step `{}` (verdict {:?}):\n{}\n{}",
                    step_cfg.name,
                    verdict,
                    last_text.as_deref().unwrap_or(""),
                    check_results.join("\n")
                );
                (n, Some(fb))
            }),
        };
        let finished_run = match (&next, verdict) {
            (None, Verdict::Pass) => Some(RunStatus::Passed),
            (None, _) => Some(RunStatus::Failed),
            (Some(_), _) if run.steps.len() as u32 >= wf.max_step_runs => Some(RunStatus::Failed),
            _ => None,
        };
        if let Some(status) = finished_run {
            run.status = status;
            run.updated_at = now_ms();
            self.save_run(&run);
            self.log("run.finished", None, None, json!({"run": run.id, "status": status}));
            let passed = status == RunStatus::Passed;
            self.notify(if passed { "run.passed" } else { "run.failed" }, if passed { "info" } else { "warning" }, format!("workflow {} {}", run.workflow, if passed { "passed" } else { "failed" }), truncate(&run.input, 200), Some(run.id.clone()));
            self.end_run_sessions(&run).await;
            if let Some(tid) = run.task_id {
                let summary = format!("workflow {} {:?}: {}", run.workflow, status, run.steps.last().and_then(|s| s.output.as_deref()).map(|t| truncate(t, 1000)).unwrap_or_default());
                let st = if status == RunStatus::Passed { TaskStatus::Done } else { TaskStatus::Failed };
                let _ = self.finish_task(&Actor::System, tid, st, summary).await;
            }
            return Ok(());
        }
        let (next_step, feedback) = next.unwrap();
        // The finished step's session is no longer needed; its workspace is shared.
        let _ = self.stop(&Actor::System, &s.report.id, false).await;
        self.start_step(&wf, &mut run, &next_step, feedback, Some(s.report.node.clone())).await
    }

    async fn end_run_sessions(&self, run: &WorkflowRun) {
        for st in &run.steps {
            if let Some(s) = self.db.session(&st.session) {
                if s.report.state.is_live() {
                    let _ = self.stop(&Actor::System, &st.session, false).await;
                }
            }
        }
    }

    pub async fn cancel_run(&self, actor: &Actor, id: &str) -> Result<WorkflowRun> {
        let mut run = self.db.run(id).ok_or_else(|| anyhow!("no run {id}"))?;
        if run.status == RunStatus::Running {
            run.status = RunStatus::Cancelled;
            run.updated_at = now_ms();
            self.save_run(&run);
            self.end_run_sessions(&run).await;
        }
        self.audit(actor, "workflow.cancel", id, json!({}), true);
        Ok(run)
    }

    // ================================================================ tasks

    pub fn create_task(&self, actor: &Actor, c: TaskCreate) -> Result<Task> {
        if c.title.trim().is_empty() {
            bail!("title must not be empty");
        }
        let now = now_ms();
        let mut t = Task {
            id: 0,
            title: c.title,
            body: c.body,
            label: c.label,
            rank: c.rank.unwrap_or_else(|| self.db.max_rank() + 1.0),
            status: TaskStatus::Queued,
            tags: c.tags,
            coordinator_session: None,
            result: None,
            notes: vec![],
            created_at: now,
            updated_at: now,
            started_at: None,
            finished_at: None,
            question_mode: c.question_mode,
        };
        self.db.insert_task(&mut t)?;
        let _ = self.stream.send(StreamEvent::Task(t.clone()));
        self.audit(actor, "task.create", &t.id.to_string(), json!({"title": t.title, "label": t.label}), true);
        Ok(t)
    }

    pub fn patch_task(&self, actor: &Actor, id: i64, p: TaskPatch) -> Result<Task> {
        let mut t = self.db.task(id).ok_or_else(|| anyhow!("no task {id}"))?;
        if let Some(v) = p.title {
            t.title = v;
        }
        if let Some(v) = p.body {
            t.body = v;
        }
        if let Some(v) = p.label {
            t.label = v;
        }
        if let Some(v) = p.tags {
            t.tags = v;
        }
        if let Some(v) = p.rank {
            t.rank = v;
        }
        if let Some(v) = p.question_mode {
            t.question_mode = Some(v);
        }
        t.updated_at = now_ms();
        self.save_task(&t);
        self.audit(actor, "task.update", &id.to_string(), json!({}), true);
        Ok(t)
    }

    /// Rewrite ranks so `ids` come first in the given order, others keep their order after.
    pub fn reorder_tasks(&self, actor: &Actor, ids: &[i64]) -> Result<Vec<Task>> {
        let all: Vec<Task> = self.db.tasks().into_iter().filter(|t| matches!(t.status, TaskStatus::Queued | TaskStatus::Running)).collect();
        let mut ordered: Vec<Task> = ids.iter().filter_map(|id| all.iter().find(|t| t.id == *id).cloned()).collect();
        ordered.extend(all.iter().filter(|t| !ids.contains(&t.id)).cloned());
        for (i, mut t) in ordered.into_iter().enumerate() {
            let r = (i + 1) as f64;
            if t.rank != r {
                t.rank = r;
                t.updated_at = now_ms();
                self.save_task(&t);
            }
        }
        self.audit(actor, "task.reorder", "-", json!({"ids": ids}), true);
        Ok(self.db.tasks())
    }

    pub async fn delete_task(&self, actor: &Actor, id: i64) -> Result<()> {
        let t = self.db.task(id).ok_or_else(|| anyhow!("no task {id}"))?;
        if t.status == TaskStatus::Running {
            bail!("task {id} is running; cancel it first");
        }
        self.db.delete_task(id)?;
        self.audit(actor, "task.delete", &id.to_string(), json!({"title": t.title}), true);
        Ok(())
    }

    /// Start a task now regardless of label (operator "Run" button).
    pub async fn run_task(self: &Arc<Self>, actor: &Actor, id: i64) -> Result<Task> {
        let _g = self.orch_lock.lock().await;
        self.start_task(actor, id).await
    }

    async fn start_task(self: &Arc<Self>, actor: &Actor, id: i64) -> Result<Task> {
        let mut t = self.db.task(id).ok_or_else(|| anyhow!("no task {id}"))?;
        if t.status != TaskStatus::Queued {
            bail!("task {id} is {}", t.status.as_str());
        }
        let wf = t.tags.iter().find_map(|g| g.strip_prefix("workflow:")).map(String::from);
        let res: Result<()> = async {
            if let Some(wf) = wf {
                let input = if t.body.is_empty() { t.title.clone() } else { format!("{}\n\n{}", t.title, t.body) };
                // start_run takes no lock itself; it is safe under orch_lock.
                self.clone().start_run(actor, &wf, &input, Some(t.id)).await.map(|_| ())
            } else {
                let sched = &self.cfg.tasks;
                let prompt = format!(
                    "Task #{} [{}]: {}\n\n{}\n\nTags: {}",
                    t.id,
                    t.label.as_str(),
                    t.title,
                    t.body,
                    if t.tags.is_empty() { "-".into() } else { t.tags.join(", ") }
                );
                let s = self
                    .spawn(
                        actor,
                        SpawnOpts {
                            agent: sched.coordinator_agent.clone(),
                            prompt: Some(prompt),
                            node: sched.coordinator_node.clone(),
                            title: Some(format!("task #{} coordinator", t.id)),
                            workspace: Some(WorkspaceSpec { mode: WorkspaceMode::Shared, name: Some(format!("task-{}", t.id)), seed: None }),
                            task_id: Some(t.id),
                            role: SessionRole::Coordinator,
                            extra_instructions: Some(COORDINATOR_BRIEFING.into()),
                            ..Default::default()
                        },
                    )
                    .await?;
                t.coordinator_session = Some(s.report.id);
                Ok(())
            }
        }
        .await;
        match res {
            Ok(()) => {
                t.status = TaskStatus::Running;
                t.started_at = Some(now_ms());
                t.updated_at = now_ms();
                self.save_task(&t);
                self.log("task.started", None, t.coordinator_session.as_deref(), json!({"task": t.id, "title": t.title}));
                Ok(t)
            }
            Err(e) => {
                let note = format!("could not start: {e:#}");
                if t.notes.last() != Some(&note) {
                    t.notes.push(note);
                    t.updated_at = now_ms();
                    self.save_task(&t);
                }
                Err(e)
            }
        }
    }

    pub async fn cancel_task(self: &Arc<Self>, actor: &Actor, id: i64) -> Result<Task> {
        self.finish_task(actor, id, TaskStatus::Cancelled, "cancelled by operator".into()).await?;
        self.db.task(id).ok_or_else(|| anyhow!("no task {id}"))
    }

    /// Terminal transition: record the result and stop the task's sessions.
    pub async fn finish_task(self: &Arc<Self>, actor: &Actor, id: i64, status: TaskStatus, result: String) -> Result<()> {
        let mut t = self.db.task(id).ok_or_else(|| anyhow!("no task {id}"))?;
        if matches!(t.status, TaskStatus::Done | TaskStatus::Failed | TaskStatus::Cancelled) {
            return Ok(());
        }
        // Halted sessions are kept for a resume; ending the task ends them too.
        let halted: Vec<String> = self.db.sessions(true).into_iter().filter(|s| s.task_id == Some(id) && s.report.state == SessionState::Halted).map(|s| s.report.id).collect();
        for s in halted {
            let _ = self.stop_with(&Actor::System, &s, false, "task_end").await;
        }
        for mut h in self.db.halts(true).into_iter().filter(|h| h.scope == format!("task:{id}")) {
            h.status = "resolved".into();
            h.resolution = Some(format!("task ended ({})", status.as_str()));
            h.resolved_at = Some(now_ms());
            let _ = self.db.put_halt(&h);
        }
        t.status = status;
        t.result = Some(result);
        t.finished_at = Some(now_ms());
        t.updated_at = now_ms();
        self.save_task(&t);
        self.audit(actor, &format!("task.{}", status.as_str()), &id.to_string(), json!({}), true);
        self.log("task.finished", None, t.coordinator_session.as_deref(), json!({"task": id, "status": status}));
        let sev = if status == TaskStatus::Failed { "warning" } else { "info" };
        self.notify(&format!("task.{}", status.as_str()), sev, format!("task #{id} {}: {}", status.as_str(), truncate(&t.title, 80)), truncate(t.result.as_deref().unwrap_or(""), 400), Some(format!("task:{id}")));
        // Stop everything that belongs to the task, after the caller's MCP reply is out.
        let sessions: Vec<String> = self.db.sessions(true).into_iter().filter(|s| s.task_id == Some(id) && s.report.state.is_live()).map(|s| s.report.id).collect();
        let runs: Vec<String> = self.db.runs().into_iter().filter(|r| r.task_id == Some(id) && r.status == RunStatus::Running).map(|r| r.id).collect();
        let me = self.clone();
        tokio::spawn(async move {
            // Let the caller's MCP reply reach the agent before its container stops.
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            for r in runs {
                let _ = me.cancel_run(&Actor::System, &r).await;
            }
            for s in sessions {
                let _ = me.stop_with(&Actor::System, &s, false, "task_end").await;
            }
        });
        Ok(())
    }

    async fn coordinator_idle(self: &Arc<Self>, s: &SessionInfo, error: Option<&str>) {
        let Some(tid) = s.task_id else { return };
        let Some(mut t) = self.db.task(tid) else { return };
        if t.status != TaskStatus::Running || t.coordinator_session.as_deref() != Some(&s.report.id) {
            return;
        }
        // Children still working? Then the coordinator is just waiting for them.
        let busy_children = self.db.sessions(false).iter().any(|c| c.parent.as_deref() == Some(&s.report.id) && matches!(c.report.state, SessionState::Running | SessionState::Starting | SessionState::Blocked | SessionState::Parked));
        if busy_children && error.is_none() {
            return;
        }
        let nudges = t.notes.iter().filter(|n| n.starts_with("[nudge]")).count() as u32;
        if error.is_some() || nudges >= self.cfg.tasks.max_nudges {
            let why = error.map(|e| format!("coordinator error: {e}")).unwrap_or_else(|| "coordinator stopped without calling task_complete/task_fail".into());
            let _ = self.finish_task(&Actor::System, tid, TaskStatus::Failed, why).await;
            return;
        }
        t.notes.push(format!("[nudge] {}", nudges + 1));
        t.updated_at = now_ms();
        self.save_task(&t);
        let _ = self
            .message(
                &Actor::System,
                &s.report.id,
                "[cluster] You stopped, but your task is still open. Continue, or call cluster_task_complete with a summary (or cluster_task_fail with a reason).",
                Delivery::Queue,
            )
            .await;
    }

    /// Scheduler tick (every ~2s).
    pub async fn schedule(self: &Arc<Self>, idle_since: &mut Option<Ts>) {
        self.expire_unreachable();
        self.question_tick().await;
        if !self.cfg.tasks.enabled {
            return;
        }
        let _g = self.orch_lock.lock().await;
        let tasks = self.db.tasks();
        let running = tasks.iter().filter(|t| t.status == TaskStatus::Running).count() as u32;
        let max = self.cfg.tasks.max_concurrent_tasks;
        let mut started = 0;
        for t in tasks.iter().filter(|t| t.status == TaskStatus::Queued && t.label == TaskLabel::Urgent) {
            if running + started >= max + 1 {
                break;
            }
            if self.start_task(&Actor::System, t.id).await.is_ok() {
                started += 1;
            }
        }
        let busy = self.db.sessions(false).iter().any(|s| matches!(s.report.state, SessionState::Running | SessionState::Starting | SessionState::Blocked | SessionState::Parked));
        let now = now_ms();
        if busy || running + started > 0 {
            *idle_since = None;
            return;
        }
        let since = *idle_since.get_or_insert(now);
        if now - since < self.cfg.tasks.idle_grace_secs as i64 * 1000 {
            return;
        }
        if let Some(t) = tasks.iter().find(|t| t.status == TaskStatus::Queued && t.label == TaskLabel::WhenFree) {
            if self.start_task(&Actor::System, t.id).await.is_ok() {
                *idle_since = None;
            }
        }
    }
}
