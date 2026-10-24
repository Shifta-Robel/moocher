//! Questions, escalation and the assumption log (PHASE1_5 §6).
//!
//! Sources: `cluster_ask`, OpenCode's own `question` tool, `ask` permissions
//! and budget stops. A question goes to the asker's parent first; a holder
//! answers (`cluster_answer`), passes it up (`cluster_escalate`), or passes it
//! up by doing nothing for `answer_timeout_secs`. At the top of the chain the
//! question mode decides whether it reaches the operator; if not, the top
//! agent decides and the decision goes to the assumption log.
//!
//! Importance is set by the asker and can only be raised by the rules
//! (keywords about money, credentials, deletion, irreversible or external
//! actions). Operator-only questions (secrets, budgets) skip the agents.

use crate::state::{truncate, Actor, Sup};
use anyhow::{anyhow, bail, Result};
use ocl_protocol::api::{AnswerRequest, StreamEvent};
use ocl_protocol::link::NodeCommand;
use ocl_protocol::model::*;
use serde_json::json;
use std::sync::Arc;

pub const OPERATOR: &str = "operator";

/// Raise importance by rule; never lower it. Returns (importance, reasons, operator_only).
pub fn classify(cfg: &ocl_protocol::config::QuestionsConfig, kind: QuestionKind, text: &str, asked: Importance) -> (Importance, Vec<String>, bool) {
    let t = text.to_lowercase();
    let mut imp = asked;
    let mut why = vec![];
    for w in &cfg.vital_keywords {
        if t.contains(&w.to_lowercase()) {
            why.push(format!("mentions {w:?}"));
        }
    }
    if !why.is_empty() {
        imp = Importance::Vital;
    }
    let mut operator_only = cfg.operator_only_keywords.iter().any(|w| t.contains(&w.to_lowercase()));
    match kind {
        QuestionKind::Budget => {
            imp = Importance::Vital;
            operator_only = true;
            why.push("spending past a budget".into());
        }
        QuestionKind::Permission if imp < Importance::Normal => {
            imp = Importance::Normal;
            why.push("permission request".into());
        }
        _ => {}
    }
    if operator_only && imp < Importance::Vital {
        imp = Importance::Vital;
    }
    (imp, why, operator_only)
}

fn render_options(q: &Question) -> String {
    if q.options.is_empty() {
        return String::new();
    }
    let opts: Vec<String> = q
        .options
        .iter()
        .enumerate()
        .map(|(i, o)| if o.description.is_empty() { format!("{}) {}", i + 1, o.label) } else { format!("{}) {} — {}", i + 1, o.label, o.description) })
        .collect();
    format!("\nOptions: {}", opts.join("; "))
}

impl Sup {
    /// The mode for a task's agents (task setting, else the live cluster mode).
    pub fn question_mode(&self, task: Option<i64>) -> QuestionMode {
        task.and_then(|t| self.db.task(t)).and_then(|t| t.question_mode).unwrap_or_else(|| self.cluster_question_mode())
    }

    pub fn cluster_question_mode(&self) -> QuestionMode {
        self.db.meta("question_mode").and_then(|m| QuestionMode::parse(&m)).unwrap_or(self.cfg.questions.mode)
    }

    pub fn set_question_mode(&self, actor: &Actor, mode: QuestionMode, task: Option<i64>) -> Result<()> {
        match task {
            Some(id) => {
                let mut t = self.db.task(id).ok_or_else(|| anyhow!("no task {id}"))?;
                t.question_mode = Some(mode);
                t.updated_at = now_ms();
                self.save_task(&t);
            }
            None => self.db.set_meta("question_mode", mode.as_str())?,
        }
        self.audit(actor, "questions.mode", &task.map(|t| format!("task:{t}")).unwrap_or_else(|| "cluster".into()), json!({"mode": mode}), true);
        Ok(())
    }

    fn save_question(&self, q: &Question) {
        let _ = self.db.put_question(q);
        let mut live = q.clone();
        live.blocked = self.blocked_on(&q.asker, &self.db.sessions(false));
        let _ = self.stream.send(StreamEvent::Question(live));
    }

    /// The asker and every live session below it (they all wait for the answer).
    fn blocked_on(&self, asker: &str, sessions: &[SessionInfo]) -> u32 {
        let mut n = 1u32;
        let mut frontier = vec![asker.to_string()];
        while let Some(p) = frontier.pop() {
            for c in sessions.iter().filter(|c| c.parent.as_deref() == Some(p.as_str()) && c.report.state.is_live()) {
                n += 1;
                frontier.push(c.report.id.clone());
            }
        }
        n
    }

    pub fn notify(&self, kind: &str, severity: &str, title: String, body: String, target: Option<String>) {
        let n = Notification { id: 0, ts: now_ms(), kind: kind.into(), severity: severity.into(), title, body, target, read: false };
        if let Ok(n) = self.db.add_notification(&n) {
            if let Some(me) = self.me.upgrade() {
                self.notifier.dispatch(&me, &n);
            }
            let _ = self.stream.send(StreamEvent::Notification(n));
        }
    }

    fn live_parent(&self, session: &str) -> Option<SessionInfo> {
        let s = self.db.session(session)?;
        let p = self.db.session(s.parent.as_deref()?)?;
        p.report.state.is_live().then_some(p)
    }

    /// Ask a question on behalf of `asker`. Returns it after routing.
    #[allow(clippy::too_many_arguments)]
    pub async fn ask(
        self: &Arc<Self>,
        asker: &SessionInfo,
        kind: QuestionKind,
        text: String,
        options: Vec<QuestionOption>,
        multiple: bool,
        asked: Importance,
        context: Option<String>,
        request: Option<String>,
    ) -> Result<Question> {
        if let Some(r) = &request {
            if let Some(q) = self.db.question_by_request(&asker.report.id, r) {
                return Ok(q); // the node reports a pending request again after a reconnect
            }
        }
        let (importance, raised_because, operator_only) = classify(&self.cfg.questions, kind, &format!("{text}\n{}", context.as_deref().unwrap_or("")), asked);
        let now = now_ms();
        let mut q = Question {
            id: ocl_protocol::new_id("q"),
            kind,
            asker: asker.report.id.clone(),
            asker_agent: asker.report.agent.clone(),
            node: asker.report.node.clone(),
            task_id: asker.task_id,
            run_id: asker.run_id.clone(),
            text,
            options,
            multiple,
            context,
            importance,
            asked_importance: asked,
            raised_because,
            operator_only,
            mode: self.question_mode(asker.task_id),
            holder: asker.report.id.clone(),
            holder_since: now,
            top_decides: false,
            chain: vec![],
            status: QuestionStatus::Open,
            answer: None,
            answered_by: None,
            request,
            blocked: 0,
            created_at: now,
            updated_at: now,
        };
        self.log("question.asked", Some(&q.node), Some(&q.asker), json!({"id": q.id, "kind": q.kind, "importance": q.importance, "raised": q.raised_because, "text": truncate(&q.text, 300)}));
        if kind == QuestionKind::Ask {
            let mut s = asker.clone();
            s.waiting_on.push(q.id.clone());
            self.save_session(&s);
        }
        let parent = self.live_parent(&q.asker);
        if q.operator_only || q.mode == QuestionMode::AskMeEverything {
            let why = if q.operator_only { "operator only" } else { "mode ask_me_everything" };
            if let Some(p) = &parent {
                // The chain sees it, but does not answer.
                let fyi = format!(
                    "[cluster] FYI: session {} ({}) asked the operator directly ({why}), question {}: {}\nYou do not need to answer it.",
                    q.asker,
                    q.asker_agent,
                    q.id,
                    truncate(&q.text, 500)
                );
                let _ = self.db.inbox_push(&p.report.id, "cluster", &fyi);
            }
            self.hand_to_operator(&mut q, why).await;
        } else if let Some(p) = parent {
            self.hand_to_agent(&mut q, &p, "asked").await;
        } else {
            self.at_top(&mut q, None, "asked").await;
        }
        self.save_question(&q);
        Ok(q)
    }

    async fn hand_to_operator(&self, q: &mut Question, why: &str) {
        q.holder = OPERATOR.into();
        q.holder_since = now_ms();
        q.top_decides = false;
        q.chain.push(Hop { holder: OPERATOR.into(), at: now_ms(), why: why.into(), note: None });
        q.updated_at = now_ms();
        let sev = if q.importance == Importance::Vital { "vital" } else { "warning" };
        self.notify(
            "question.waiting",
            sev,
            format!("{} question from {} ({})", q.importance.as_str(), q.asker_agent, q.asker),
            truncate(&q.text, 300),
            Some(q.id.clone()),
        );
        self.log("question.to_operator", Some(&q.node), Some(&q.asker), json!({"id": q.id, "why": why}));
    }

    async fn hand_to_agent(&self, q: &mut Question, holder: &SessionInfo, why: &str) {
        q.holder = holder.report.id.clone();
        q.holder_since = now_ms();
        q.chain.push(Hop { holder: holder.report.id.clone(), at: now_ms(), why: why.into(), note: None });
        q.updated_at = now_ms();
        let notes: Vec<String> = q.chain.iter().filter_map(|h| h.note.as_ref().map(|n| format!("- {}: {n}", h.why))).collect();
        let decide = if q.top_decides {
            format!(
                "Nobody above you will answer this (question mode {}, importance {}). Decide yourself with cluster_answer; your decision is written to the assumption log for the operator to review.",
                q.mode.as_str(),
                q.importance.as_str()
            )
        } else {
            format!(
                "Answer it with cluster_answer(id=\"{}\", answer=\"...\") if you can, or pass it up with cluster_escalate(id=\"{}\", note=\"what you know\"). If you do neither within {}s it goes up by itself.",
                q.id, q.id, self.cfg.questions.answer_timeout_secs
            )
        };
        let msg = format!(
            "[cluster] Question {} from session {} ({}), importance {}{}:\n{}{}{}{}\n\n{decide}",
            q.id,
            q.asker,
            q.asker_agent,
            q.importance.as_str(),
            match q.kind {
                QuestionKind::Permission => ", a permission request (answer once, always or reject)",
                QuestionKind::Opencode => ", it is waiting for the answer",
                _ => "",
            },
            q.text,
            render_options(q),
            q.context.as_deref().map(|c| format!("\nContext: {c}")).unwrap_or_default(),
            if notes.is_empty() { String::new() } else { format!("\nNotes from the chain:\n{}", notes.join("\n")) },
        );
        if let Err(e) = self.message(&Actor::System, &holder.report.id, &msg, Delivery::Queue).await {
            tracing::warn!(question = q.id, holder = holder.report.id, error = %e, "could not deliver question");
        }
        self.log("question.routed", Some(&holder.report.node), Some(&holder.report.id), json!({"id": q.id, "why": why, "top_decides": q.top_decides}));
    }

    /// Nobody above `top` (the current agent holder, or the asker): the mode decides.
    async fn at_top(self: &Arc<Self>, q: &mut Question, top: Option<SessionInfo>, via: &str) {
        if q.operator_only || q.mode.reaches_operator(q.importance) {
            self.hand_to_operator(q, &format!("top of the chain, mode {}, {via}", q.mode.as_str())).await;
            return;
        }
        match top {
            Some(t) if t.report.id != q.asker && !q.top_decides => {
                q.top_decides = true;
                self.hand_to_agent(q, &t, "top of the chain decides").await;
            }
            _ => self.asker_decides(q, "nobody in the chain may pass it to the operator").await,
        }
    }

    /// The asker must decide by itself; it is told so and the fact is logged.
    async fn asker_decides(self: &Arc<Self>, q: &mut Question, why: &str) {
        let text = format!(
            "Nobody will answer this question (question mode {}, importance {}: {why}). Decide yourself using your best judgment and state the assumption you made.",
            q.mode.as_str(),
            q.importance.as_str()
        );
        q.status = QuestionStatus::Decided;
        q.answer = Some(text.clone());
        q.answered_by = Some("asker".into());
        q.updated_at = now_ms();
        self.assume(q, "asker", &format!("Q: {}\nThe asker decided by itself ({why}).", truncate(&q.text, 500)));
        if let Err(e) = self.deliver(q, &text, true).await {
            tracing::warn!(question = q.id, error = %e, "could not deliver the decide-yourself answer");
        }
    }

    fn assume(&self, q: &Question, decided_by: &str, text: &str) {
        let a = Assumption {
            id: 0,
            ts: now_ms(),
            task_id: q.task_id,
            run_id: q.run_id.clone(),
            session: q.asker.clone(),
            question: Some(q.id.clone()),
            decided_by: decided_by.into(),
            text: text.into(),
            mode: q.mode,
        };
        let _ = self.db.add_assumption(&a);
        self.log("assumption.logged", Some(&q.node), Some(&q.asker), json!({"question": q.id, "decided_by": decided_by}));
    }

    /// Pass a question one level up (from its current agent holder).
    pub async fn escalate(self: &Arc<Self>, id: &str, by: &str, note: Option<String>) -> Result<Question> {
        let mut q = self.db.question(id).ok_or_else(|| anyhow!("no such question {id}"))?;
        if q.status != QuestionStatus::Open {
            bail!("question {id} is already {:?}", q.status);
        }
        if let Some(h) = q.chain.last_mut() {
            h.note = note.clone().or(h.note.take());
        }
        let current = self.db.session(&q.holder);
        if q.top_decides {
            // The top agent will not decide either: back to the asker.
            self.asker_decides(&mut q, &format!("the top of the chain passed ({by})")).await;
        } else {
            let above = current.as_ref().and_then(|c| self.live_parent(&c.report.id));
            match above {
                Some(p) => self.hand_to_agent(&mut q, &p, &format!("escalated by {by}")).await,
                None => self.at_top(&mut q, current.filter(|c| c.report.state.is_live()), &format!("escalated by {by}")).await,
            }
        }
        self.log("question.escalated", None, Some(&q.asker), json!({"id": q.id, "by": by, "note": note, "holder": q.holder}));
        self.save_question(&q);
        Ok(q)
    }

    /// Answer from an agent (`by` = its session) or the operator.
    pub async fn answer(self: &Arc<Self>, actor: &Actor, id: &str, req: AnswerRequest) -> Result<Question> {
        let mut q = self.db.question(id).ok_or_else(|| anyhow!("no such question {id}"))?;
        if q.status != QuestionStatus::Open {
            bail!("question {id} is already {:?}", q.status);
        }
        let by = match actor {
            Actor::Session(s) => {
                if q.operator_only {
                    bail!("question {id} is for the operator only; pass it up with cluster_escalate");
                }
                if &q.holder != s {
                    bail!("question {id} is held by {}, not by you", q.holder);
                }
                s.clone()
            }
            other => other.to_string(),
        };
        let mut text = match (&req.answer, req.choice) {
            (_, Some(n)) => q.options.get(n.wrapping_sub(1)).map(|o| o.label.clone()).ok_or_else(|| anyhow!("choice {n} is not between 1 and {}", q.options.len()))?,
            (Some(a), None) if !a.trim().is_empty() => a.trim().to_string(),
            _ if req.let_agent_decide || req.reject => String::new(),
            _ => bail!("give an answer, a choice, let_agent_decide or reject"),
        };
        let decided = q.top_decides && matches!(actor, Actor::Session(_));
        if req.let_agent_decide {
            text = "The operator leaves this to you: decide using your best judgment and state the assumption you made.".into();
            q.status = QuestionStatus::Decided;
            self.assume(&q, "operator-delegated", &format!("Q: {}\nThe operator let the agent decide.", truncate(&q.text, 500)));
        } else if req.reject {
            q.status = QuestionStatus::Rejected;
        } else if decided {
            q.status = QuestionStatus::Decided;
            self.assume(&q, &by, &format!("Q: {}\nA: {}", truncate(&q.text, 500), truncate(&text, 1000)));
        } else {
            q.status = QuestionStatus::Answered;
        }
        q.answer = Some(if req.reject { "rejected".into() } else { text.clone() });
        q.answered_by = Some(by.clone());
        q.updated_at = now_ms();
        let res = self.deliver(&q, &text, req.let_agent_decide).await;
        if let Err(e) = &res {
            // Not delivered: keep it open so someone can try again.
            q.status = QuestionStatus::Open;
            q.answer = None;
            q.answered_by = None;
            self.save_question(&q);
            bail!("could not deliver the answer to {}: {e:#}", q.asker);
        }
        // Everyone who passed it up sees the outcome (in their inbox, not waking them).
        for h in q.chain.iter().filter(|h| h.holder != OPERATOR && h.holder != by && h.holder != q.asker) {
            let _ = self.db.inbox_push(&h.holder, "cluster", &format!("[cluster] Question {} you handled was answered by {by}: {}", q.id, truncate(q.answer.as_deref().unwrap_or(""), 500)));
        }
        self.audit(actor, "question.answer", id, json!({"status": q.status, "kind": q.kind}), true);
        self.log("question.answered", Some(&q.node), Some(&q.asker), json!({"id": q.id, "by": by, "status": q.status}));
        self.save_question(&q);
        Ok(q)
    }

    /// Hand the outcome to the asker, the way its kind needs.
    async fn deliver(&self, q: &Question, text: &str, self_decides: bool) -> Result<()> {
        let s = self.session(&q.asker)?;
        match q.kind {
            QuestionKind::Ask => {
                let body = if q.status == QuestionStatus::Rejected {
                    format!("[cluster] Your question {} was declined; continue without it, stating your assumption.", q.id)
                } else if self_decides {
                    format!("[cluster] About your question {}: {text}", q.id)
                } else {
                    format!("[cluster] Answer to your question {} (from {}):\n{text}", q.id, q.answered_by.as_deref().unwrap_or("?"))
                };
                let mut s2 = s.clone();
                s2.waiting_on.retain(|w| w != &q.id);
                self.save_session(&s2);
                if s.report.state.is_live() {
                    self.message(&Actor::System, &q.asker, &body, Delivery::Queue).await?;
                }
            }
            QuestionKind::Opencode => {
                let req = q.request.clone().ok_or_else(|| anyhow!("question has no OpenCode request id"))?;
                let rejected = q.status == QuestionStatus::Rejected;
                let cmd = if rejected {
                    NodeCommand::QuestionReject { session: q.asker.clone(), request: req }
                } else {
                    NodeCommand::QuestionReply { session: q.asker.clone(), request: req, answers: vec![vec![text.to_string()]] }
                };
                self.nodes.call(&s.report.node, cmd, crate::state::CMD_TIMEOUT).await?;
                if rejected {
                    // OpenCode ends the turn on a dismissed question; nobody may be watching.
                    let msg = format!("[cluster] Your question was declined by {}. Continue without the answer, stating the assumption you make.", q.answered_by.as_deref().unwrap_or("the operator"));
                    self.message(&Actor::System, &q.asker, &msg, Delivery::Queue).await?;
                }
            }
            QuestionKind::Permission => {
                let req = q.request.clone().ok_or_else(|| anyhow!("permission question has no request id"))?;
                let t = text.trim().to_lowercase();
                let reply = if q.status == QuestionStatus::Rejected || self_decides {
                    "reject" // nobody approved it
                } else if t.starts_with("always") {
                    "always"
                } else if ["once", "yes", "allow", "approve", "ok", "grant"].iter().any(|w| t.starts_with(w)) {
                    "once"
                } else if ["reject", "no", "deny"].iter().any(|w| t.starts_with(w)) {
                    "reject"
                } else {
                    bail!("a permission answer must be once, always or reject");
                };
                self.nodes.call(&s.report.node, NodeCommand::PermissionReply { session: q.asker.clone(), request: req, reply: reply.into() }, crate::state::CMD_TIMEOUT).await?;
                if reply == "reject" {
                    // OpenCode stops the turn on a rejection; in a cluster nobody may be
                    // watching, so tell the agent and let it carry on without it.
                    let msg = format!(
                        "[cluster] Your permission request was refused by {}: {}. Continue without it; if you cannot, say why or ask with cluster_ask.",
                        q.answered_by.as_deref().unwrap_or("the operator"),
                        truncate(&q.text, 300)
                    );
                    self.message(&Actor::System, &q.asker, &msg, Delivery::Queue).await?;
                }
            }
            QuestionKind::Budget => {
                let t = text.trim().to_lowercase();
                if q.status == QuestionStatus::Rejected || self_decides || t.starts_with("stop") || t.starts_with("no") {
                    self.stop(&Actor::System, &q.asker, false).await?;
                } else {
                    let cur: f64 = q.context.as_deref().and_then(|c| c.split_whitespace().find_map(|w| w.strip_prefix("budget=").and_then(|v| v.parse().ok()))).unwrap_or(1.0);
                    let new = t.split_whitespace().find_map(|w| w.trim_start_matches('$').parse::<f64>().ok()).unwrap_or(cur * 2.0);
                    if new <= cur {
                        bail!("the new budget ${new:.2} must be above the current ${cur:.2}");
                    }
                    self.nodes.call(&s.report.node, NodeCommand::SetBudget { session: q.asker.clone(), budget_usd: new }, crate::state::CMD_TIMEOUT).await?;
                    self.message(&Actor::System, &q.asker, &format!("[cluster] The operator raised your budget to ${new:.2}. Continue."), Delivery::Queue).await?;
                }
            }
        }
        Ok(())
    }

    /// A pending OpenCode question/permission was settled inside OpenCode.
    pub fn question_gone(&self, session: &str, request: &str) {
        if let Some(mut q) = self.db.question_by_request(session, request) {
            if q.status == QuestionStatus::Open {
                q.status = QuestionStatus::Cancelled;
                q.answer = Some("settled inside OpenCode (answered there, or the turn was aborted)".into());
                q.updated_at = now_ms();
                self.save_question(&q);
            }
        }
    }

    /// Open questions with blocked counts, operator inbox order: importance,
    /// then how many instances wait on it, then age.
    pub fn questions_view(&self, open_only: bool, operator_only: bool) -> Vec<Question> {
        let sessions = self.db.sessions(false);
        let mut qs: Vec<Question> = self.db.questions(open_only).into_iter().filter(|q| !operator_only || q.holder == OPERATOR).collect();
        for q in &mut qs {
            q.blocked = self.blocked_on(&q.asker, &sessions);
        }
        if open_only {
            qs.sort_by(|a, b| b.importance.cmp(&a.importance).then(b.blocked.cmp(&a.blocked)).then(a.created_at.cmp(&b.created_at)));
        }
        qs
    }

    /// Scheduler tick: holders that sat on a question too long, holders and
    /// askers that went away.
    pub async fn question_tick(self: &Arc<Self>) {
        let timeout = self.cfg.questions.answer_timeout_secs as i64 * 1000;
        for q in self.db.questions(true) {
            let asker_live = self.db.session(&q.asker).is_some_and(|s| s.report.state.is_live());
            if !asker_live {
                let mut q = q;
                q.status = QuestionStatus::Cancelled;
                q.answer = Some("the asker is no longer running".into());
                q.updated_at = now_ms();
                self.save_question(&q);
                continue;
            }
            if q.holder == OPERATOR {
                continue;
            }
            let holder_live = self.db.session(&q.holder).is_some_and(|s| s.report.state.is_live());
            if q.holder == q.asker && holder_live {
                continue;
            }
            if !holder_live || now_ms() - q.holder_since > timeout {
                let by = if holder_live { "timeout" } else { "holder gone" };
                let _ = self.escalate(&q.id, by, None).await;
            }
        }
    }

    /// Questions held by `session` (for its `cluster_questions` tool).
    pub fn questions_held(&self, session: &str) -> Vec<serde_json::Value> {
        self.db
            .questions(true)
            .into_iter()
            .filter(|q| q.holder == session)
            .map(|q| json!({"id": q.id, "from": q.asker, "agent": q.asker_agent, "importance": q.importance, "kind": q.kind, "question": q.text, "options": q.options, "you_decide": q.top_decides, "context": q.context}))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_only_raise() {
        let cfg = ocl_protocol::config::QuestionsConfig::default();
        let (i, why, op) = classify(&cfg, QuestionKind::Ask, "Should I rm -rf the build dir?", Importance::Low);
        assert_eq!(i, Importance::Vital);
        assert!(why.iter().any(|w| w.contains("rm -rf")));
        assert!(!op);
        let (i, _, _) = classify(&cfg, QuestionKind::Ask, "Tabs or spaces?", Importance::Vital);
        assert_eq!(i, Importance::Vital, "never lowered");
        let (i, _, _) = classify(&cfg, QuestionKind::Ask, "Tabs or spaces?", Importance::Low);
        assert_eq!(i, Importance::Low);
        let (i, _, op) = classify(&cfg, QuestionKind::Ask, "What is the database password?", Importance::Low);
        assert!(op && i == Importance::Vital, "secrets are operator-only");
        let (i, _, op) = classify(&cfg, QuestionKind::Budget, "budget reached", Importance::Low);
        assert!(op && i == Importance::Vital);
        let (i, _, _) = classify(&cfg, QuestionKind::Permission, "bash: ls", Importance::Low);
        assert_eq!(i, Importance::Normal);
    }

    #[test]
    fn modes() {
        use Importance::*;
        use QuestionMode::*;
        assert!(AskMeEverything.reaches_operator(Low));
        assert!(!ImportantAndVital.reaches_operator(Low) && ImportantAndVital.reaches_operator(Normal));
        assert!(!VitalOnly.reaches_operator(Normal) && VitalOnly.reaches_operator(Vital));
        assert!(!Autonomous.reaches_operator(Vital));
    }
}
