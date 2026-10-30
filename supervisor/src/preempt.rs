//! Urgency of sessions, and making room for `right_now` work.
//!
//! A session's urgency is its own (set when it was spawned at another level
//! than its task), else its task's, else `normal`. When a `right_now` spawn
//! finds no room, the least urgent session in its way is halted (harvested,
//! container kept) and resumed once no `right_now` work is left. Only as many
//! as needed go. Other `right_now` work, the spawner's own task and sessions
//! started by hand are left alone (`tasks.preempt_operator_sessions` opts the
//! last ones in).

use crate::state::{Actor, SpawnOpts, Sup, CMD_TIMEOUT};
use moocher_protocol::link::NodeCommand;
use moocher_protocol::model::*;
use serde_json::json;
use std::cmp::Reverse;
use std::time::Duration;

/// A halt takes the container down, which takes a while.
const HALT_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_VICTIMS: usize = 16;

/// What a session is worth keeping, for choosing a victim.
#[derive(Debug, Clone)]
struct Candidate {
    id: String,
    level: TaskLabel,
    idle: bool,
    coordinator: bool,
    started_at: Ts,
}

/// Idle sessions first, then the least urgent; workers before the coordinator
/// that waits for them; among equals, the one with the least work to lose.
fn pick_victim(mut c: Vec<Candidate>) -> Option<Candidate> {
    c.retain(|x| x.level.level() < TaskLabel::RightNow.level());
    c.sort_by_key(|x| (!x.idle, x.level.level(), x.coordinator, Reverse(x.started_at)));
    c.into_iter().next()
}

impl Sup {
    pub fn urgency_of(&self, s: &SessionInfo) -> TaskLabel {
        s.urgency.or_else(|| s.task_id.and_then(|t| self.db.task(t)).map(|t| t.label)).unwrap_or(TaskLabel::Normal)
    }

    pub fn spawn_urgency(&self, o: &SpawnOpts) -> TaskLabel {
        o.urgency.or_else(|| o.task_id.and_then(|t| self.db.task(t)).map(|t| t.label)).unwrap_or(TaskLabel::Normal)
    }

    /// Is a lack of slots what stops this spawn? Then: only sessions of this
    /// agent help (`Some(true)`, its instance cap) or any on its nodes (`Some(false)`).
    fn out_of_room(&self, o: &SpawnOpts) -> Option<(bool, Vec<String>)> {
        let nodes: Vec<NodeInfo> = self
            .node_views()
            .into_iter()
            .filter(|n| n.connected && !n.relay && o.node.as_ref().is_none_or(|h| h == &n.name))
            .filter(|n| n.agents.iter().any(|a| a.name == o.agent && a.errors.is_empty() && a.secret_ok))
            .collect();
        let names = nodes.iter().map(|n| n.name.clone()).collect::<Vec<_>>();
        let max = nodes.iter().flat_map(|n| n.agents.iter()).filter(|a| a.name == o.agent).filter_map(|a| a.max_instances).min();
        if let Some(max) = max {
            let live = self.db.sessions(false).iter().filter(|s| s.report.agent == o.agent && s.report.state.is_live()).count() as u32;
            if live >= max {
                return Some((true, names));
            }
        }
        (!nodes.is_empty() && nodes.iter().all(|n| n.running >= n.max_sessions)).then_some((false, names))
    }

    /// Halt one session in the way of a `right_now` spawn. Returns false when
    /// nothing can be done, and waits until the hub has seen the slot free up.
    pub async fn make_room(&self, o: &SpawnOpts) -> bool {
        let Some((same_agent, nodes)) = self.out_of_room(o) else { return false };
        let by = o.task_id.map(|t| format!("task #{t}")).unwrap_or_else(|| format!("a right_now {} instance", o.agent));
        let sessions = self.db.sessions(false);
        let mine = |s: &SessionInfo| (o.task_id.is_some() && s.task_id == o.task_id) || o.parent.as_deref().is_some_and(|p| p == s.report.id || s.parent.as_deref() == Some(p));
        let candidates = sessions
            .iter()
            .filter(|s| matches!(s.report.state, SessionState::Idle | SessionState::Running | SessionState::Parked) && s.waiting_on.is_empty())
            .filter(|s| !mine(s) && nodes.contains(&s.report.node) && (!same_agent || s.report.agent == o.agent))
            .filter(|s| s.task_id.is_some() || self.cfg.tasks.preempt_operator_sessions)
            .map(|s| Candidate { id: s.report.id.clone(), level: self.urgency_of(s), idle: s.report.state == SessionState::Idle, coordinator: s.role == SessionRole::Coordinator, started_at: s.report.started_at })
            .collect();
        let Some(v) = pick_victim(candidates) else { return false };
        let Some(mut s) = self.db.session(&v.id) else { return false };
        // Marked first: from here on its messages are held, not sent into the halt.
        s.preempted = Some(Preempted { by: by.clone(), at: now_ms() });
        self.save_session(&s);
        if let Err(e) = self.nodes.call(&s.report.node, NodeCommand::Halt { session: v.id.clone() }, HALT_TIMEOUT).await {
            tracing::warn!(session = %v.id, error = %format!("{e:#}"), "could not preempt");
            s = self.db.session(&v.id).unwrap_or(s);
            s.preempted = None;
            self.save_session(&s);
            self.flush_held(&v.id).await;
            return false;
        }
        s = self.db.session(&v.id).unwrap_or(s);
        s.report.state = SessionState::Halted;
        s.report.current_task = Some(format!("paused for {by}; resumes when it is done"));
        let node = s.report.node.clone();
        self.save_session(&s);
        self.log("session.preempted", Some(&node), Some(&v.id), json!({"by": by, "task": o.task_id, "urgency": v.level.as_str()}));
        self.notify("session.preempted", "info", format!("{} paused for {by}", s.report.agent), format!("{} on {node} resumes when {by} is done", v.id), Some(v.id.clone()));
        // Its parent should wait for it, not replace it.
        if let Some(parent) = s.parent.clone() {
            let msg = format!("[cluster] Session {} ({}) was paused to make room for {by}. Nothing is lost and it resumes by itself; do not replace it.", v.id, s.report.agent);
            let _ = self.message(&Actor::System, &parent, &msg, Delivery::Queue).await;
        }
        true
    }

    /// Try a `right_now` spawn again after each session made way.
    pub async fn spawn_with_room(&self, o: &SpawnOpts) -> anyhow::Result<SessionInfo> {
        let mut res = self.spawn_inner(o).await;
        if self.spawn_urgency(o) != TaskLabel::RightNow {
            return res;
        }
        for _ in 0..MAX_VICTIMS {
            if res.is_ok() || !self.make_room(o).await {
                break;
            }
            res = self.spawn_inner(o).await;
        }
        res
    }

    /// Is `right_now` work still around? Then nothing lower starts or resumes.
    pub fn right_now_active(&self) -> bool {
        self.db.tasks().iter().any(|t| t.label == TaskLabel::RightNow && t.status == TaskStatus::Queued)
            || self.queued_spawns().iter().any(|q| q.urgency == TaskLabel::RightNow)
            || self.db.sessions(false).iter().any(|s| s.report.state.is_live() && self.urgency_of(s) == TaskLabel::RightNow)
    }

    /// Bring back what was paused, most urgent first, where there is room.
    pub async fn resume_preempted(&self) {
        let mut paused: Vec<SessionInfo> = self.db.sessions(false).into_iter().filter(|s| s.preempted.is_some() && s.report.state == SessionState::Halted).collect();
        if paused.is_empty() || self.right_now_active() {
            return;
        }
        paused.sort_by_key(|s| Reverse(self.urgency_of(s).level()));
        for mut s in paused {
            let free = self.node_views().iter().any(|n| n.name == s.report.node && n.connected && n.running < n.max_sessions);
            if !free {
                continue;
            }
            let id = s.report.id.clone();
            let by = s.preempted.as_ref().map(|p| p.by.clone()).unwrap_or_default();
            let prompt = format!("[cluster] You were paused to make room for {by} and have resumed. The turn you were in was cut off, so a command that was running did not finish: check what it left before relying on it, then carry on. Anything sent to you meanwhile comes in a separate message marked as past events.");
            if let Err(e) = self.nodes.call(&s.report.node, NodeCommand::Restart { session: id.clone(), prompt }, HALT_TIMEOUT.max(CMD_TIMEOUT)).await {
                tracing::warn!(session = %id, error = %format!("{e:#}"), "could not resume");
                continue;
            }
            s = self.db.session(&id).unwrap_or(s);
            s.preempted = None;
            self.save_session(&s);
            self.log("session.resumed", Some(&s.report.node), Some(&id), json!({"was_paused_for": by}));
            self.notify("session.resumed", "info", format!("{} resumed", s.report.agent), format!("{id}, paused for {by}"), Some(id.clone()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(id: &str, level: TaskLabel, idle: bool, coordinator: bool, started_at: Ts) -> Candidate {
        Candidate { id: id.into(), level, idle, coordinator, started_at }
    }

    #[test]
    fn victims_in_order() {
        use TaskLabel::*;
        let pick = |v: Vec<Candidate>| pick_victim(v).map(|x| x.id);
        // Idle beats anything running, even more urgent work.
        assert_eq!(pick(vec![c("run", WhenFree, false, false, 1), c("idle", Urgent, true, false, 1)]).unwrap(), "idle");
        // Running: when_free, then normal, then urgent.
        assert_eq!(pick(vec![c("u", Urgent, false, false, 1), c("n", Normal, false, false, 1), c("w", WhenFree, false, false, 1)]).unwrap(), "w");
        assert_eq!(pick(vec![c("u", Urgent, false, false, 1), c("n", OnDemand, false, false, 1)]).unwrap(), "n");
        // A worker before its coordinator; the newest loses least.
        assert_eq!(pick(vec![c("co", Normal, false, true, 9), c("wk", Normal, false, false, 1)]).unwrap(), "wk");
        assert_eq!(pick(vec![c("old", Normal, false, false, 1), c("new", Normal, false, false, 9)]).unwrap(), "new");
        // Other right_now work is never touched.
        assert_eq!(pick(vec![c("r", RightNow, true, false, 1)]), None);
    }
}
