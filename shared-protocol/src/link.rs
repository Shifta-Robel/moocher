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
    /// Certificate renewal; answered with `SupMsg::Reply { value: {cert_pem, expires_at} }`.
    RenewCert { req: u64, csr_pem: String },
    /// Answer to `SupMsg::Ping` (link round-trip time).
    Pong { ts: i64 },
    /// Resource samples, best effort (not kept in the outbox).
    Stats { host: HostStats, instances: Vec<InstanceStats> },
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
    #[serde(default)]
    pub role: crate::config::NodeRole,
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
    /// Egress IP per route (on change).
    Egress { ips: Vec<EgressIp> },
    /// Current quota buckets of this node (full snapshot, debounced).
    Quotas { buckets: Vec<QuotaBucket> },
    /// Provider latency per route (full snapshot, at most once a minute).
    RouteStats { stats: Vec<RouteStat> },
    /// OpenCode is waiting on someone: its `question` tool or an `ask` permission.
    QuestionAsked {
        session: String,
        /// OpenCode request id (`que_…` / `per_…`).
        request: String,
        kind: QuestionKind,
        text: String,
        #[serde(default)]
        options: Vec<QuestionOption>,
        #[serde(default)]
        multiple: bool,
        #[serde(default)]
        detail: serde_json::Value,
    },
    /// The request was answered or dropped inside OpenCode (e.g. the turn was aborted).
    QuestionGone { session: String, request: String },
    /// keyproxy refused a request: the session's budget is used up.
    BudgetExhausted { session: String, budget_usd: f64, spent_usd: f64 },
    /// The container/process is gone without being stopped.
    Died {
        session: String,
        /// `oom`, `exit:<code>`, `crashloop`, `wall_clock`.
        reason: String,
        #[serde(default)]
        detail: String,
    },
    Checkpoint(Checkpoint),
    Harvested(Harvest),
}

/// Supervisor -> node.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum SupMsg {
    /// Reply to `Hello`: the highest event seq already persisted from this node,
    /// and (remote nodes) the source address the hub sees for the link.
    Welcome {
        last_seq: u64,
        #[serde(default)]
        observed_ip: Option<String>,
    },
    Command { req: u64, cmd: NodeCommand },
    /// Reply to a node `Request`.
    Reply { req: u64, ok: bool, #[serde(default)] value: serde_json::Value, #[serde(default)] error: Option<String> },
    /// Link round-trip measurement; the node answers `Pong { ts }` at once.
    Ping { ts: i64 },
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
    /// Checked against the agent's `overridable` on the node.
    #[serde(default)]
    pub overrides: crate::config::SpawnOverrides,
    /// For the preamble.
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub task: Option<i64>,
    /// For the preamble.
    #[serde(default)]
    pub question_mode: Option<QuestionMode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "c", rename_all = "snake_case")]
pub enum NodeCommand {
    Spawn(SpawnSpec),
    Prompt { session: String, text: String, delivery: Delivery },
    Interrupt { session: String },
    /// Stop and harvest; `trigger` (`stop`, `task_end`) goes into the harvest record.
    Stop {
        session: String,
        #[serde(default)]
        remove_workspace: bool,
        #[serde(default)]
        trigger: Option<String>,
    },
    PermissionReply { session: String, request: String, reply: String },
    /// Run a command in the session's container (success checks).
    Exec { session: String, command: String, timeout_secs: u64 },
    /// Relay nodes: connect to `host:port`, then open a Tunnel RPC with `Accept { id }`.
    OpenTunnel { id: String, host: String, port: u16, from: String },
    /// Live change written to the node's overrides.yaml (`None` clears it).
    SetRoute { agent: String, route: Option<crate::config::RouteSpec>, by: String },
    /// Add/replace a quota override (matched on provider + scope_id); `clear` removes it.
    SetQuota {
        #[serde(rename = "override")]
        ov: crate::config::QuotaOverride,
        #[serde(default)]
        clear: bool,
    },
    /// Re-probe egress IPs now.
    ProbeEgress,
    /// The hub's template library and cluster preamble (replaces the previous set).
    SyncTemplates {
        templates: Vec<crate::config::AgentConfig>,
        #[serde(default)]
        preamble: Option<String>,
        hash: String,
    },
    /// The rendered instructions a session was started with.
    ReadPreamble { session: String },
    /// Answer OpenCode's `question` tool (one answer list per question).
    QuestionReply { session: String, request: String, answers: Vec<Vec<String>> },
    QuestionReject { session: String, request: String },
    /// Raise (or lower) a live session's budget.
    SetBudget { session: String, budget_usd: f64 },
    /// Pack a workspace (`shared:<name>` / `isolated:<session>`) into the
    /// artifact store; replies `{reference, files, bytes, unchanged}`.
    SnapshotWorkspace {
        workspace: String,
        #[serde(default)]
        session: Option<String>,
        #[serde(default)]
        reason: String,
    },
    /// Merge a snapshot into this node's copy of a workspace (three-way, by
    /// file, against the last state both sides saw). Replies `{applied, conflicts}`.
    ApplyWorkspace { workspace: String, reference: String, #[serde(default)] from_node: Option<String> },
    /// Put an artifact into a workspace at `dest` (a file path, or a directory for trees).
    PlaceArtifact { workspace: String, reference: String, dest: String },
    /// Start a dead or halted instance again from its kept volumes, then send `prompt`.
    Restart { session: String, prompt: String },
    /// Interrupt, harvest, and stop the container but keep it (state `halted`).
    Halt { session: String },
    /// Harvest a stopped/dead instance whose volumes are still here.
    Harvest { session: String, trigger: String },
    /// "Open in OpenCode": open a Tunnel RPC with `Accept { id }` and serve
    /// HTTP on it, forwarded to the session's OpenCode server with its password.
    OpenAttach {
        id: String,
        session: String,
        /// SHA-256 (hex) of the browser's attach cookie: the node checks it on
        /// every request of the connection, not only the first.
        #[serde(default)]
        cookie_sha256: Option<String>,
        #[serde(default)]
        expires_at: Option<Ts>,
    },
    /// Write the live route override of `agent` into its YAML file and drop the override.
    PromoteRoute { agent: String, by: String },
}

/// Cluster tools available to agents over MCP. `from_session` is added by the node.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "tool", rename_all = "snake_case")]
pub enum AgentCall {
    Send { to: String, text: String, #[serde(default)] delivery: Delivery },
    Inbox,
    ListAgents,
    /// Nodes with free slots, egress IPs, provider allowances and latency.
    ListNodes,
    ListSessions,
    SessionStatus { session: String },
    // Coordinator-only below.
    Spawn {
        agent: String,
        prompt: String,
        #[serde(default)]
        node: Option<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        share_workspace: bool,
        #[serde(default)]
        overrides: crate::config::SpawnOverrides,
    },
    Stop { session: String },
    TaskGet,
    TaskNote { note: String },
    TaskComplete { summary: String },
    TaskFail { reason: String },
    // Questions and upward communication (PHASE1_5 §6).
    Ask {
        question: String,
        #[serde(default)]
        options: Vec<String>,
        #[serde(default)]
        importance: Option<Importance>,
        #[serde(default)]
        context: Option<String>,
    },
    Answer { id: String, answer: String },
    Escalate { id: String, #[serde(default)] note: Option<String> },
    /// Questions you hold (asked of you by agents you started).
    Questions,
    ReportUp { message: String },
    LogAssumption { text: String },
    // Artifacts. Upload/download of bytes runs over the Blobs RPC on the node;
    // these are the metadata calls.
    ArtifactList {
        #[serde(default)]
        query: Option<String>,
        #[serde(default)]
        task_only: bool,
    },
    ArtifactInfo { reference: String },
    /// Exchange changes with the other machines that hold your shared workspace.
    SyncWorkspace,
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

/// First frame of a `Tunnel` RPC (JSON in `TunnelFrame.header`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum TunnelOpen {
    /// From the node that wants out: connect `host:port` from `relay`'s IP.
    Dial { relay: String, host: String, port: u16 },
    /// From the relay node, answering `NodeCommand::OpenTunnel { id }`.
    Accept { id: String },
}
