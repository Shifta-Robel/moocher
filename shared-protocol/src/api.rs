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
