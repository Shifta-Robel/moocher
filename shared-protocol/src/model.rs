//! Domain types shared by the supervisor, node daemon, CLI and UI.

use serde::{Deserialize, Serialize};

pub type Ts = i64; // unix millis

pub fn now_ms() -> Ts {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// Container/OpenCode booting.
    Starting,
    Idle,
    Running,
    /// Waiting on a permission/question reply.
    Blocked,
    Error,
    Stopped,
    /// Node disconnected; metadata retained.
    Unreachable,
    /// Node came back without it, or grace period expired.
    Lost,
}

impl SessionState {
    pub fn is_live(self) -> bool {
        matches!(self, SessionState::Starting | SessionState::Idle | SessionState::Running | SessionState::Blocked)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            SessionState::Starting => "starting",
            SessionState::Idle => "idle",
            SessionState::Running => "running",
            SessionState::Blocked => "blocked",
            SessionState::Error => "error",
            SessionState::Stopped => "stopped",
            SessionState::Unreachable => "unreachable",
            SessionState::Lost => "lost",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost_usd: f64,
}

impl TokenUsage {
    pub fn total(&self) -> u64 {
        self.input + self.output + self.reasoning
    }
    pub fn add(&mut self, o: &TokenUsage) {
        self.input += o.input;
        self.output += o.output;
        self.reasoning += o.reasoning;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
        self.cost_usd += o.cost_usd;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Todo {
    pub content: String,
    /// pending | in_progress | completed | cancelled
    pub status: String,
    #[serde(default)]
    pub priority: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    Fail,
    Incomplete,
}

/// Todo-based verdict (ARCHITECTURE §6a). `seeded` items must exist and be completed.
pub fn todo_verdict(todos: &[Todo], seeded: &[String], required: bool) -> Verdict {
    if !required {
        return Verdict::Pass;
    }
    if todos.iter().any(|t| t.status == "cancelled" && seeded.iter().any(|s| same_todo(s, &t.content))) {
        return Verdict::Fail;
    }
    let open = todos.iter().any(|t| t.status == "pending" || t.status == "in_progress");
    let seeded_done = seeded
        .iter()
        .all(|s| todos.iter().any(|t| same_todo(s, &t.content) && t.status == "completed"));
    if open || !seeded_done {
        Verdict::Incomplete
    } else {
        Verdict::Pass
    }
}

fn same_todo(a: &str, b: &str) -> bool {
    let n = |s: &str| s.trim().to_lowercase();
    n(a) == n(b) || n(b).contains(&n(a))
}

/// Everything a node knows about one session; sent as a full snapshot on change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionReport {
    pub id: String,
    pub node: String,
    pub agent: String,
    pub state: SessionState,
    #[serde(default)]
    pub opencode_id: Option<String>,
    #[serde(default)]
    pub tokens: TokenUsage,
    #[serde(default)]
    pub todos: Vec<Todo>,
    /// The `in_progress` todo, or the last tool in use.
    #[serde(default)]
    pub current_task: Option<String>,
    #[serde(default)]
    pub last_text: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    pub started_at: Ts,
    pub updated_at: Ts,
}

/// Supervisor-side view of a session (report + orchestration metadata).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    #[serde(flatten)]
    pub report: SessionReport,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub task_id: Option<i64>,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub step: Option<String>,
    #[serde(default)]
    pub verdict: Option<Verdict>,
    #[serde(default)]
    pub role: SessionRole,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRole {
    #[default]
    Worker,
    Coordinator,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub name: String,
    pub description: String,
    pub provider: String,
    pub model: String,
    pub tier: String,
    /// The secret reference resolves on this node (the secret itself never leaves).
    pub secret_ok: bool,
    pub config_hash: String,
    #[serde(default)]
    pub errors: Vec<String>,
    #[serde(default)]
    pub labels: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    pub name: String,
    pub connected: bool,
    pub version: String,
    pub max_sessions: u32,
    pub running: u32,
    pub relay: bool,
    pub agents: Vec<AgentInfo>,
    pub last_seen: Ts,
    #[serde(default)]
    pub labels: std::collections::BTreeMap<String, String>,
    /// `worker` or `relay` (proxy-only).
    #[serde(default = "default_role")]
    pub role: String,
    /// `local` (same machine, unix socket), `pending`, `approved` or `revoked`.
    #[serde(default = "default_enrollment")]
    pub enrollment: String,
    /// Source address of the node's link connection, as seen by the hub.
    #[serde(default)]
    pub observed_ip: Option<String>,
    /// Expiry of the node's link certificate (remote nodes).
    #[serde(default)]
    pub cert_expires_at: Option<Ts>,
}

fn default_role() -> String {
    "worker".into()
}
fn default_enrollment() -> String {
    "local".into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// Inject into the current run (OpenCode `steer`).
    Steer,
    /// Run after the current turn (OpenCode `queue`).
    #[default]
    Queue,
    /// Store in the session's mailbox; the agent pulls it with `inbox`.
    Inbox,
}

// ------------------------------------------------------------------- tasks

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskLabel {
    /// Start as soon as a slot is free, even if other tasks are running.
    Urgent,
    /// Start only when the cluster has nothing else running.
    WhenFree,
    /// Never start automatically; the operator presses Run.
    OnDemand,
}

impl TaskLabel {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskLabel::Urgent => "urgent",
            TaskLabel::WhenFree => "when_free",
            TaskLabel::OnDemand => "on_demand",
        }
    }
    pub fn parse(s: &str) -> Option<TaskLabel> {
        match s {
            "urgent" => Some(TaskLabel::Urgent),
            "when_free" | "when-free" => Some(TaskLabel::WhenFree),
            "on_demand" | "on-demand" => Some(TaskLabel::OnDemand),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Queued => "queued",
            TaskStatus::Running => "running",
            TaskStatus::Done => "done",
            TaskStatus::Failed => "failed",
            TaskStatus::Cancelled => "cancelled",
        }
    }
    pub fn parse(s: &str) -> Option<TaskStatus> {
        Some(match s {
            "queued" => TaskStatus::Queued,
            "running" => TaskStatus::Running,
            "done" => TaskStatus::Done,
            "failed" => TaskStatus::Failed,
            "cancelled" => TaskStatus::Cancelled,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: i64,
    pub title: String,
    #[serde(default)]
    pub body: String,
    pub label: TaskLabel,
    /// Lower rank = earlier. The UI reorders by rewriting ranks.
    pub rank: f64,
    pub status: TaskStatus,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub coordinator_session: Option<String>,
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub notes: Vec<String>,
    pub created_at: Ts,
    pub updated_at: Ts,
    #[serde(default)]
    pub started_at: Option<Ts>,
    #[serde(default)]
    pub finished_at: Option<Ts>,
}

// --------------------------------------------------------------- workflows

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Passed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepRun {
    pub step: String,
    pub session: String,
    pub verdict: Option<Verdict>,
    pub nudges: u32,
    pub output: Option<String>,
    pub started_at: Ts,
    pub finished_at: Option<Ts>,
    #[serde(default)]
    pub check_results: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowRun {
    pub id: String,
    pub workflow: String,
    pub status: RunStatus,
    pub input: String,
    pub steps: Vec<StepRun>,
    pub workspace: String,
    pub created_at: Ts,
    pub updated_at: Ts,
    #[serde(default)]
    pub task_id: Option<i64>,
}

// ------------------------------------------------------------- event log

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEvent {
    pub id: i64,
    pub ts: Ts,
    pub kind: String,
    #[serde(default)]
    pub node: Option<String>,
    #[serde(default)]
    pub session: Option<String>,
    pub data: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    fn t(c: &str, s: &str) -> Todo {
        Todo { content: c.into(), status: s.into(), priority: "high".into() }
    }

    #[test]
    fn verdicts() {
        let seeded = vec!["write module".to_string()];
        assert_eq!(todo_verdict(&[t("Write module", "completed")], &seeded, true), Verdict::Pass);
        assert_eq!(todo_verdict(&[t("write module", "in_progress")], &seeded, true), Verdict::Incomplete);
        assert_eq!(todo_verdict(&[], &seeded, true), Verdict::Incomplete);
        assert_eq!(todo_verdict(&[t("write module", "cancelled")], &seeded, true), Verdict::Fail);
        assert_eq!(todo_verdict(&[t("x", "completed"), t("y", "pending")], &[], true), Verdict::Incomplete);
        assert_eq!(todo_verdict(&[], &[], true), Verdict::Pass);
    }
}
