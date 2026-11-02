//! Pausing a task: every agent stops with everything kept, and carries on later.
//!
//! By default a session stops at its next model call: keyproxy holds the call,
//! so the reply and the command under way get to finish, and the hub then halts
//! it. `tasks.pause_grace_secs` limits how long that may take. `--now` halts at
//! once, the coordinator first so it cannot react to its workers stopping.
//!
//! Being paused with the task (`task_paused`) and being preempted (`preempted`)
//! are separate reasons; a session runs again only when neither applies. Resume
//! goes through the same rules as starting work: `right_now` work and the task
//! cap come first.

use crate::state::{Actor, Sup, CMD_TIMEOUT};
use anyhow::{anyhow, bail, Result};
use moocher_protocol::link::NodeCommand;
use moocher_protocol::model::*;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

const HALT_TIMEOUT: Duration = Duration::from_secs(120);

impl Sup {
    pub fn task_is_paused(&self, s: &SessionInfo) -> bool {
        s.task_id.and_then(|t| self.db.task(t)).is_some_and(|t| matches!(t.status, TaskStatus::Pausing | TaskStatus::Paused))
    }

    pub async fn pause_task(&self, actor: &Actor, id: i64, now: bool) -> Result<Task> {
        let _g = self.orch_lock.lock().await;
        let mut t = self.db.task(id).ok_or_else(|| anyhow!("no task {id}"))?;
        let at = now_ms();
        let deadline = at + self.cfg.tasks.pause_grace_secs as i64 * 1000;
        let pause = TaskPause { at, by: actor.to_string(), now, deadline, ..Default::default() };
        match t.status {
            TaskStatus::Queued => {
                t.status = TaskStatus::Paused;
                t.pause = Some(TaskPause { was_queued: true, ..pause });
            }
            TaskStatus::Running => {
                t.status = TaskStatus::Pausing;
                t.pause = Some(pause);
            }
            // Already on its way: only ever hurry it up.
            TaskStatus::Pausing if now => {
                if let Some(p) = t.pause.as_mut() {
                    p.now = true;
                }
            }
            TaskStatus::Pausing => return Ok(t),
            other => bail!("task {id} is {}", other.as_str()),
        }
        t.updated_at = at;
        self.save_task(&t);
        self.audit(actor, "task.pause", &id.to_string(), json!({"now": now}), true);
        self.log("task.pausing", None, t.coordinator_session.as_deref(), json!({"task": id, "now": now}));
        Ok(t)
    }

    pub async fn resume_task(&self, actor: &Actor, id: i64) -> Result<Task> {
        let _g = self.orch_lock.lock().await;
        let mut t = self.db.task(id).ok_or_else(|| anyhow!("no task {id}"))?;
        if !matches!(t.status, TaskStatus::Pausing | TaskStatus::Paused) {
            bail!("task {id} is {}", t.status.as_str());
        }
        match t.pause.as_mut() {
            Some(p) if !p.was_queued => p.resuming = true,
            _ => {
                t.status = TaskStatus::Queued;
                t.pause = None;
            }
        }
        t.updated_at = now_ms();
        self.save_task(&t);
        self.audit(actor, "task.resume", &id.to_string(), json!({}), true);
        Ok(t)
    }

    /// Move every pausing or resuming task along. A slow halt or restart holds
    /// up only its own task.
    pub fn pause_tick(self: &Arc<Self>) {
        for t in self.db.tasks().into_iter().filter(|t| t.status == TaskStatus::Pausing || t.pause.as_ref().is_some_and(|p| p.resuming)) {
            if !self.pause_busy.lock().unwrap().insert(t.id) {
                continue;
            }
            let me = self.clone();
            tokio::spawn(async move {
                if t.pause.as_ref().is_some_and(|p| p.resuming) {
                    me.resume_step(t.id).await;
                } else {
                    me.pause_step(t.id).await;
                }
                me.pause_busy.lock().unwrap().remove(&t.id);
            });
        }
    }

    fn sessions_of(&self, task: i64) -> Vec<SessionInfo> {
        self.db.sessions(true).into_iter().filter(|s| s.task_id == Some(task)).collect()
    }

    async fn pause_step(&self, id: i64) {
        let Some(mut t) = self.db.task(id).filter(|t| t.status == TaskStatus::Pausing) else { return };
        let Some(p) = t.pause.clone() else { return };
        let mut live: Vec<SessionInfo> = self.sessions_of(id).into_iter().filter(|s| s.report.state.is_live()).collect();
        live.sort_by_key(|s| s.role != SessionRole::Coordinator);
        // Marked first: from here on their messages are held, not sent into the halt.
        for s in live.iter_mut().filter(|s| s.task_paused.is_none()) {
            s.task_paused = Some(SessionPause { at: now_ms(), cut_off: false });
            self.save_session(s);
        }
        for s in &live {
            let sid = &s.report.id;
            // Nothing of it is under way, or it waits for others that are stopping.
            let between = matches!(s.report.state, SessionState::Idle | SessionState::Parked | SessionState::Blocked) || !s.waiting_on.is_empty();
            let mut clean = between || self.pause_holding.lock().unwrap().contains(sid);
            if !clean && !p.now {
                match self.nodes.call(&s.report.node, NodeCommand::Hold { session: sid.clone(), hold: true }, CMD_TIMEOUT).await {
                    Ok(v) => clean = v["holding"].as_bool().unwrap_or(false),
                    Err(e) => tracing::debug!(session = %sid, error = %format!("{e:#}"), "no hold yet"),
                }
            }
            if clean || p.now || now_ms() >= p.deadline {
                self.halt_paused(id, sid, !clean).await;
            }
        }
        // Once it comes back, a session on an unreachable node gets paused too.
        let waiting = self.sessions_of(id).iter().any(|s| s.report.state.is_live() || s.report.state == SessionState::Unreachable);
        if waiting {
            return;
        }
        let _g = self.orch_lock.lock().await;
        let Some(now) = self.db.task(id).filter(|x| x.status == TaskStatus::Pausing && !x.pause.as_ref().is_some_and(|p| p.resuming)) else { return };
        t = now;
        t.status = TaskStatus::Paused;
        t.updated_at = now_ms();
        self.save_task(&t);
        self.log("task.paused", None, t.coordinator_session.as_deref(), json!({"task": id}));
        self.notify("task.paused", "info", format!("task #{id} paused: {}", t.title), "Its agents are stopped with everything kept. `moo task resume` carries on.".to_string(), Some(format!("task:{id}")));
    }

    async fn halt_paused(&self, task: i64, sid: &str, cut_off: bool) {
        let Some(s) = self.db.session(sid) else { return };
        if let Err(e) = self.nodes.call(&s.report.node, NodeCommand::Halt { session: sid.to_string() }, HALT_TIMEOUT).await {
            tracing::warn!(session = %sid, error = %format!("{e:#}"), "could not pause; trying again");
            return;
        }
        self.pause_holding.lock().unwrap().remove(sid);
        let mut s = self.db.session(sid).unwrap_or(s);
        s.report.state = SessionState::Halted;
        s.report.current_task = Some(format!("paused with task #{task}"));
        s.task_paused = Some(SessionPause { at: s.task_paused.as_ref().map_or(now_ms(), |p| p.at), cut_off });
        let node = s.report.node.clone();
        self.save_session(&s);
        self.log("session.paused", Some(&node), Some(sid), json!({"task": task, "cut_off": cut_off}));
    }

    /// Bring a task back once there is room, workers before the coordinator
    /// that would otherwise ask after them.
    async fn resume_step(&self, id: i64) {
        let Some(mut t) = self.db.task(id).filter(|t| t.pause.as_ref().is_some_and(|p| p.resuming)) else { return };
        if t.label != TaskLabel::RightNow {
            let running = self.db.tasks().iter().filter(|x| x.status == TaskStatus::Running).count() as u32;
            if self.right_now_active() || running >= self.cfg.tasks.max_concurrent_tasks {
                return;
            }
        }
        let mut marked: Vec<SessionInfo> = self.sessions_of(id).into_iter().filter(|s| s.task_paused.is_some()).collect();
        marked.sort_by_key(|s| s.role == SessionRole::Coordinator);
        let mut left = false;
        for mut s in marked {
            let sid = s.report.id.clone();
            match s.report.state {
                SessionState::Halted if s.preempted.is_none() => {
                    let free = self.node_views().iter().any(|n| n.name == s.report.node && n.connected && n.running < n.max_sessions);
                    if !free {
                        left = true;
                        continue;
                    }
                    let cut = s.task_paused.as_ref().is_some_and(|p| p.cut_off);
                    let what = if cut { "The turn you were in was cut off, so a command that was running did not finish: check what it left before relying on it, then carry on." } else { "You stopped between model calls, so nothing was cut off: carry on from where you were." };
                    let prompt = format!("[cluster] Task #{id} was paused by the operator and has resumed. {what} Other agents sharing your workspace may have changed files meanwhile. Anything sent to you during the pause comes in a separate message marked as past events.");
                    if let Err(e) = self.nodes.call(&s.report.node, NodeCommand::Restart { session: sid.clone(), prompt }, HALT_TIMEOUT).await {
                        tracing::warn!(session = %sid, error = %format!("{e:#}"), "could not resume");
                        left = true;
                        continue;
                    }
                    s = self.db.session(&sid).unwrap_or(s);
                    self.log("session.resumed", Some(&s.report.node), Some(&sid), json!({"was_paused_for": format!("task #{id}")}));
                }
                // Pause was called off before it stopped.
                state if state.is_live() => {
                    let _ = self.nodes.call(&s.report.node, NodeCommand::Hold { session: sid.clone(), hold: false }, CMD_TIMEOUT).await;
                    self.pause_holding.lock().unwrap().remove(&sid);
                }
                // Preempted as well: `resume_preempted` takes it from here. Or it ended.
                _ => {}
            }
            s.task_paused = None;
            self.save_session(&s);
            self.flush_held(&sid).await;
        }
        if left {
            return;
        }
        let _g = self.orch_lock.lock().await;
        let Some(now) = self.db.task(id) else { return };
        if !matches!(now.status, TaskStatus::Pausing | TaskStatus::Paused) {
            return;
        }
        t = now;
        t.status = TaskStatus::Running;
        t.pause = None;
        t.updated_at = now_ms();
        self.save_task(&t);
        self.log("task.resumed", None, t.coordinator_session.as_deref(), json!({"task": id}));
        self.notify("task.resumed", "info", format!("task #{id} resumed: {}", t.title), String::new(), Some(format!("task:{id}")));
    }
}
