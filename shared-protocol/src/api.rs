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
