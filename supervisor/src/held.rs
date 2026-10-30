//! Messages for a session that is not running (paused, restarting, dead or
//! on an unreachable node). The hub keeps them in order and hands them over
//! in one message once it runs again, marked as past events. They are removed
//! only after the node has accepted that message, and a replacement inherits
//! them. A session that ends for good drops them, with an event saying so.

use crate::state::{truncate, Actor, Sup, CMD_TIMEOUT};
use anyhow::Result;
use moocher_protocol::link::NodeCommand;
use moocher_protocol::model::*;
use serde_json::json;

/// What one digest may carry; long messages are cut to fit.
const MAX_DIGEST: usize = 60_000;
const MIN_SHARE: usize = 400;

#[derive(Debug, Clone)]
pub struct Held {
    pub id: i64,
    pub from: String,
    pub text: String,
    pub ts: Ts,
}

fn ago(ms: i64) -> String {
    match ms.max(0) / 1000 {
        s if s < 90 => format!("{s}s ago"),
        s if s < 5400 => format!("{}m ago", s / 60),
        s => format!("{}h ago", s / 3600),
    }
}

/// The message a session gets when it is back.
pub fn digest(held: &[Held], now: Ts) -> String {
    let n = held.len();
    let total: usize = held.iter().map(|h| h.text.len()).sum();
    let share = if total > MAX_DIGEST { (MAX_DIGEST / n.max(1)).max(MIN_SHARE) } else { usize::MAX };
    let cut = held.iter().any(|h| h.text.len() > share);
    let mut out = format!(
        "[cluster] {n} message{} arrived while you were not running. These are PAST events, oldest first, and may be out of date: check current state (cluster_session_status) before acting on them.{}\n",
        if n == 1 { "" } else { "s" },
        if cut { " Long ones are cut; cluster_session_status has a session's latest output." } else { "" },
    );
    for (i, h) in held.iter().enumerate() {
        let body = if h.text.len() > share { truncate(&h.text, share) } else { h.text.clone() };
        out.push_str(&format!("\n--- {} of {n}, {}, from {} ---\n{body}\n", i + 1, ago(now - h.ts), h.from));
    }
    out
}

impl Sup {
    /// Should a message wait instead of going to its node?
    pub fn holds_messages(&self, s: &SessionInfo) -> bool {
        s.preempted.is_some() || s.report.state.may_return() || (s.report.state.is_live() && self.db.held_count(&s.report.id) > 0)
    }

    pub async fn hold_message(&self, actor: &Actor, id: &str, text: &str) -> Result<()> {
        let from = match actor {
            Actor::Session(s) => s.clone(),
            other => other.to_string(),
        };
        self.db.held_push(id, &from, text)?;
        let s = self.session(id)?;
        self.save_session(&s);
        self.log("message.held", Some(&s.report.node), Some(id), json!({"from": from, "state": s.report.state, "chars": text.len()}));
        // Older messages were being delivered as this one came in.
        self.flush_held(id).await;
        Ok(())
    }

    /// Hand over what is held, if it can take it now.
    pub async fn flush_held(&self, id: &str) {
        let _g = self.held_lock.lock().await;
        let Some(s) = self.db.session(id) else { return };
        if s.preempted.is_some() || !matches!(s.report.state, SessionState::Idle | SessionState::Running | SessionState::Blocked | SessionState::Parked) {
            return;
        }
        let held = self.db.held(id).unwrap_or_default();
        let Some(last) = held.last().map(|h| h.id) else { return };
        let text = digest(&held, now_ms());
        if let Err(e) = self.nodes.call(&s.report.node, NodeCommand::Prompt { session: id.into(), text, delivery: Delivery::Queue }, CMD_TIMEOUT).await {
            tracing::warn!(session = id, error = %format!("{e:#}"), "could not deliver held messages; will try again");
            return;
        }
        let _ = self.db.held_clear(id, last);
        self.log("message.delivered", Some(&s.report.node), Some(id), json!({"held": held.len()}));
        if let Some(now) = self.db.session(id) {
            self.save_session(&now);
        }
    }

    /// A report came in for a session with held messages.
    pub async fn after_report(&self, s: &SessionInfo) {
        if s.report.state == SessionState::Stopped {
            let id = &s.report.id;
            let n = self.db.held_count(id);
            if let Some(last) = self.db.held(id).unwrap_or_default().last() {
                let _ = self.db.held_clear(id, last.id);
                self.log("message.dropped", Some(&s.report.node), Some(id), json!({"held": n, "why": "the session ended"}));
                self.save_session(s);
            }
            return;
        }
        self.flush_held(&s.report.id).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(id: i64, from: &str, text: &str, ts: Ts) -> Held {
        Held { id, from: from.into(), text: text.into(), ts }
    }

    #[test]
    fn digest_is_marked_past_and_in_order() {
        let d = digest(&[h(1, "w-a", "first", 1_000), h(2, "system", "second", 61_000)], 121_000);
        assert!(d.contains("2 messages arrived while you were not running"));
        assert!(d.contains("PAST events"));
        assert!(d.contains("--- 1 of 2, 2m ago, from w-a ---\nfirst"));
        assert!(d.contains("--- 2 of 2, 60s ago, from system ---\nsecond"));
        assert!(d.find("first").unwrap() < d.find("second").unwrap());
    }

    #[test]
    fn digest_cuts_long_messages_only_when_too_big() {
        let small = "x".repeat(10_000);
        assert!(digest(&[h(1, "a", &small, 0)], 0).contains(&small));
        let big = "y".repeat(50_000);
        let d = digest(&[h(1, "a", &big, 0), h(2, "b", &big, 0)], 0);
        assert!(d.len() < MAX_DIGEST + 2_000 && d.contains("Long ones are cut"));
        assert!(d.contains("from b"));
    }
}
