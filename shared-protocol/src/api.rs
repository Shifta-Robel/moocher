//! Operator REST API request/response bodies (`/api/v1/...`).

use crate::model::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnRequest {
    pub agent: String,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub node: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub workspace: Option<crate::config::WorkspaceSpec>,
    #[serde(default)]
    pub overrides: crate::config::SpawnOverrides,
    /// `coordinator` gives the session the coordinator-only tools
    /// (spawn_agent, stop_session, ...) without a task.
    #[serde(default)]
    pub role: SessionRole,
    /// `right_now` may pause other work; default `normal`.
    #[serde(default)]
    pub urgency: Option<TaskLabel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRequest {
    pub text: String,
    #[serde(default)]
    pub delivery: Delivery,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionReplyRequest {
    /// once | always | reject
    pub reply: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskCreate {
    pub title: String,
    #[serde(default)]
    pub body: String,
    pub label: TaskLabel,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Default: end of the list.
    #[serde(default)]
    pub rank: Option<f64>,
    #[serde(default)]
    pub question_mode: Option<QuestionMode>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskPatch {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub label: Option<TaskLabel>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub rank: Option<f64>,
    #[serde(default)]
    pub question_mode: Option<QuestionMode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReorderRequest {
    /// Task ids in the desired order; ids not listed keep their relative order after these.
    pub ids: Vec<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunWorkflowRequest {
    pub input: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Overview {
    pub nodes: Vec<NodeInfo>,
    pub sessions: Vec<SessionInfo>,
    pub tasks: Vec<Task>,
    pub totals: TokenUsage,
}

/// Pushed on `/api/v1/stream` (SSE `data:` lines).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    Session(SessionInfo),
    Node(NodeInfo),
    Task(Task),
    Run(WorkflowRun),
    Log(LogEvent),
    /// A node's quota buckets changed (refetch `/quotas` for the merged view).
    Quotas { node: String },
    Egress { node: String, ips: Vec<EgressIp> },
    /// New resource samples from a node (`/stats` has them).
    Stats { node: String },
    Question(Question),
    Notification(Notification),
    Halt(Halt),
    Artifact(ArtifactMeta),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnswerRequest {
    /// Free text, or the label of one of the options.
    #[serde(default)]
    pub answer: Option<String>,
    /// 1-based option number.
    #[serde(default)]
    pub choice: Option<usize>,
    /// "Let the agent decide": the asker decides; logged as an assumption.
    #[serde(default)]
    pub let_agent_decide: bool,
    #[serde(default)]
    pub reject: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModeRequest {
    pub mode: QuestionMode,
    /// Only this task (default: the whole cluster).
    #[serde(default)]
    pub task: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoverRequest {
    /// `restart`, `resume_elsewhere`, `handoff`, `abandon`.
    pub option: String,
    /// For resume_elsewhere: preferred node.
    #[serde(default)]
    pub node: Option<String>,
}

/// A quota bucket merged across the nodes that report it (same provider and
/// scope id: e.g. two machines behind one IP share a keyless allowance).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaView {
    pub provider: String,
    pub scope: String,
    pub scope_id: String,
    pub kind: String,
    pub limit: Option<u64>,
    pub used: u64,
    pub remaining: Option<u64>,
    pub resets_at: Option<Ts>,
    pub exhausted: bool,
    pub source: String,
    pub window: Option<String>,
    pub nodes: Vec<String>,
    /// `node/route`
    pub routes: Vec<String>,
    pub sessions: Vec<String>,
    pub updated_at: Ts,
    /// Set on provider units; see `QuotaBucket::unit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<UnitInfo>,
}

impl QuotaView {
    /// Does an agent on `model` draw from this? Requests do; a unit does unless
    /// it is another model's pool. (Token buckets are informational.)
    pub fn gates(&self, model: &str) -> bool {
        match &self.unit {
            Some(u) => u.model.as_deref().is_none_or(|m| m == model),
            // `<scope id>|<model>` is one model's own allowance (`quota.models`).
            None => self.kind == "requests" && self.scope_id.split_once('|').is_none_or(|(_, m)| m == model),
        }
    }
}

/// Egress IPs and who shares each one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedIp {
    pub ip: String,
    pub nodes: Vec<String>,
    /// `node/route`
    pub routes: Vec<String>,
    /// Live sessions that recently sent traffic from this IP.
    pub sessions: Vec<String>,
    /// Providers with a per-IP bucket on this address.
    pub providers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EgressView {
    pub ips: Vec<EgressIp>,
    pub shared: Vec<SharedIp>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaOverrideRequest {
    /// Default: every node that reports this provider's bucket.
    #[serde(default)]
    pub node: Option<String>,
    pub provider: String,
    #[serde(default)]
    pub scope_id: Option<String>,
    /// Unix millis.
    #[serde(default)]
    pub resets_at: Option<Ts>,
    #[serde(default)]
    pub remaining: Option<u64>,
    /// Calibrate a provider unit: how much of it the provider says is used
    /// in the current window (see `quota.units`).
    #[serde(default)]
    pub unit: Option<String>,
    #[serde(default)]
    pub used: Option<f64>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub clear: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteOverrideRequest {
    pub agent: String,
    /// `null` removes the override (back to the agent's YAML).
    #[serde(default)]
    pub route: Option<crate::config::RouteSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InviteRequest {
    pub name: String,
    #[serde(default)]
    pub role: crate::config::NodeRole,
    #[serde(default)]
    pub labels: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AllowSourceRequest {
    /// IP address or CIDR.
    pub source: String,
    #[serde(default)]
    pub note: String,
}
