//! JSON payloads carried in `Envelope.json` on the node<->supervisor stream.

use crate::model::*;
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

/// Node -> supervisor.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum NodeMsg {
    /// First message on every connection: full inventory for reconciliation.
    Hello(Hello),
    /// Durable telemetry (sent with an outbox `seq`).
    Event(NodeEvent),
    /// Reply to a supervisor `Command`.
    Reply { req: u64, ok: bool, #[serde(default)] value: serde_json::Value, #[serde(default)] error: Option<String> },
    /// An agent called a cluster tool (MCP); the supervisor answers with `SupMsg::Reply`.
    Request { req: u64, from_session: String, call: AgentCall },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    pub node: String,
    pub version: String,
    pub max_sessions: u32,
    pub relay: bool,
    pub agents: Vec<AgentInfo>,
    pub sessions: Vec<SessionReport>,
    #[serde(default)]
    pub labels: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "e", rename_all = "snake_case")]
pub enum NodeEvent {
    /// Full snapshot of a session after any change.
    Session(SessionReport),
    /// The agent loop went idle (turn finished, possibly with an error).
    TurnEnded { session: String, last_text: Option<String>, todos: Vec<Todo>, error: Option<String> },
    /// Agent profiles changed on disk (hot reload).
    Agents { agents: Vec<AgentInfo> },
    /// Fine-grained activity for the event log (tool calls, permission asks...).
    Activity { session: String, kind: String, detail: serde_json::Value },
}

/// Supervisor -> node.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum SupMsg {
    /// Reply to `Hello`: the highest event seq already persisted from this node.
    Welcome { last_seq: u64 },
    Command { req: u64, cmd: NodeCommand },
    /// Reply to a node `Request`.
    Reply { req: u64, ok: bool, #[serde(default)] value: serde_json::Value, #[serde(default)] error: Option<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnSpec {
    /// Cluster-wide session id chosen by the supervisor (`s-…`).
    pub id: String,
    pub agent: String,
    #[serde(default)]
    pub prompt: Option<String>,
    /// Overrides the agent's workspace setting (workflows share one workspace).
    #[serde(default)]
    pub workspace: Option<crate::config::WorkspaceSpec>,
    #[serde(default)]
    pub title: Option<String>,
    /// Extra instructions appended to the role (coordinator briefing, seeded todos).
    #[serde(default)]
    pub extra_instructions: Option<String>,
    /// Coordinator sessions get the coordinator-only cluster tools.
    #[serde(default)]
    pub role: SessionRole,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "c", rename_all = "snake_case")]
pub enum NodeCommand {
    Spawn(SpawnSpec),
    Prompt { session: String, text: String, delivery: Delivery },
    Interrupt { session: String },
    Stop { session: String, #[serde(default)] remove_workspace: bool },
    PermissionReply { session: String, request: String, reply: String },
    /// Run a command in the session's container (success checks).
    Exec { session: String, command: String, timeout_secs: u64 },
}

/// Cluster tools available to agents over MCP. `from_session` is added by the node.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "tool", rename_all = "snake_case")]
pub enum AgentCall {
    Send { to: String, text: String, #[serde(default)] delivery: Delivery },
    Inbox,
    ListAgents,
    ListSessions,
    SessionStatus { session: String },
    // Coordinator-only below.
    Spawn { agent: String, prompt: String, #[serde(default)] node: Option<String>, #[serde(default)] title: Option<String>, #[serde(default)] share_workspace: bool },
    Stop { session: String },
    TaskGet,
    TaskNote { note: String },
    TaskComplete { summary: String },
    TaskFail { reason: String },
}

impl AgentCall {
    pub fn coordinator_only(&self) -> bool {
        matches!(
            self,
            AgentCall::Spawn { .. }
                | AgentCall::Stop { .. }
                | AgentCall::TaskGet
                | AgentCall::TaskNote { .. }
                | AgentCall::TaskComplete { .. }
                | AgentCall::TaskFail { .. }
        )
    }
}
