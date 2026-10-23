//! On-disk configuration formats (YAML). See docs/CONFIG.md.
//!
//! Layout of a config directory (default `~/.opencode-cluster`):
//!
//! ```text
//! node.yaml            NodeConfig        (every machine running opencode-node)
//! supervisor.yaml      SupervisorConfig  (hub only)
//! providers.yaml       ProvidersFile     (optional; merged over built-in defaults)
//! routes.yaml          RoutesFile        (optional)
//! agents/*.yaml        AgentConfig       (one file per agent profile)
//! workflows/*.yaml     WorkflowConfig    (hub only)
//! prompts/*.md         referenced by agents via role.system_prompt_file
//! ```
//!
//! Secrets never appear in these files, only [`SecretRef`]s that are resolved
//! on the node that owns the agent.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------- node.yaml

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    /// Unique node name within the cluster (`[a-z0-9-]+`).
    pub name: String,
    /// Where the supervisor is: `unix:///run/ocl/supervisor.sock` (same machine)
    /// or `https://hub.example:7443` (remote, mTLS; set up by `opencode-node join`).
    pub supervisor: String,
    /// `worker` (default) runs agents. `relay` runs no agents and only carries
    /// other nodes' LLM traffic out through this machine's IP (`via_node` routes).
    #[serde(default)]
    pub role: NodeRole,
    /// Node key, certificate and the hub CA (default: `<config dir>/pki`).
    #[serde(default)]
    pub pki_dir: Option<PathBuf>,
    #[serde(default)]
    pub runtime: RuntimeConfig,
    /// Maximum concurrently running sessions on this node.
    #[serde(default = "default_node_max_sessions")]
    pub max_sessions: u32,
    /// Allow other nodes to send LLM traffic out through this machine (`via_node` routes).
    #[serde(default)]
    pub relay: bool,
    /// Default egress route for agents that do not set one.
    #[serde(default)]
    pub default_route: Option<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

fn default_node_max_sessions() -> u32 {
    6
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NodeRole {
    #[default]
    Worker,
    Relay,
}

impl NodeRole {
    pub fn as_str(self) -> &'static str {
        match self {
            NodeRole::Worker => "worker",
            NodeRole::Relay => "relay",
        }
    }
    pub fn parse(s: &str) -> Option<NodeRole> {
        match s {
            "worker" => Some(NodeRole::Worker),
            "relay" => Some(NodeRole::Relay),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    /// `podman` (default, required for production) or `process` (dev/tests only:
    /// runs opencode directly on the host without isolation).
    #[serde(default = "default_runtime_kind")]
    pub kind: RuntimeKind,
    #[serde(default = "default_podman")]
    pub podman: String,
    /// Default agent image.
    #[serde(default = "default_image")]
    pub image: String,
    /// Directory for per-session sockets. Default: `$XDG_RUNTIME_DIR/ocl` or `/run/ocl`.
    #[serde(default)]
    pub run_dir: Option<PathBuf>,
    /// Directory for workspaces and node state. Default: `<config dir>/data`.
    #[serde(default)]
    pub data_dir: Option<PathBuf>,
    /// `process` runtime only: path to the opencode binary.
    #[serde(default)]
    pub opencode_bin: Option<String>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        RuntimeConfig {
            kind: default_runtime_kind(),
            podman: default_podman(),
            image: default_image(),
            run_dir: None,
            data_dir: None,
            opencode_bin: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeKind {
    Podman,
    Process,
}

fn default_runtime_kind() -> RuntimeKind {
    RuntimeKind::Podman
}
fn default_podman() -> String {
    "podman".into()
}
pub const DEFAULT_IMAGE: &str = "localhost/ocl-agent:1.19.4";
fn default_image() -> String {
    DEFAULT_IMAGE.into()
}

// ---------------------------------------------------------- supervisor.yaml

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SupervisorConfig {
    /// Operator API listen address (HTTP; put TLS in front or use Phase 4 built-in TLS).
    #[serde(default = "default_api_listen")]
    pub api_listen: String,
    /// Unix socket that same-machine nodes (the hub's own node) connect to.
    #[serde(default)]
    pub link_socket: Option<PathBuf>,
    /// TCP listener for remote nodes (mTLS), e.g. `0.0.0.0:7443`. Off when unset.
    #[serde(default)]
    pub link_listen: Option<String>,
    /// Host names / IPs remote nodes use to reach the hub. They go into the
    /// link certificate; the first one also forms the join command.
    #[serde(default)]
    pub public_names: Vec<String>,
    #[serde(default)]
    pub enrollment: EnrollmentConfig,
    /// Database and artifact directory. Default: `<config dir>/supervisor`.
    #[serde(default)]
    pub data_dir: Option<PathBuf>,
    #[serde(default)]
    pub tasks: TaskSchedulerConfig,
    /// Seconds a disconnected node's sessions stay `unreachable` before becoming `lost`.
    #[serde(default = "default_grace")]
    pub disconnect_grace_secs: u64,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        SupervisorConfig {
            api_listen: default_api_listen(),
            link_socket: None,
            link_listen: None,
            public_names: vec![],
            enrollment: EnrollmentConfig::default(),
            data_dir: None,
            tasks: TaskSchedulerConfig::default(),
            disconnect_grace_secs: default_grace(),
        }
    }
}

/// How remote machines join (ARCHITECTURE D5, PHASE1_5 §4).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentConfig {
    /// A newly enrolled node stays `pending` until the operator approves it.
    #[serde(default = "default_true")]
    pub require_approval: bool,
    /// Source addresses (IP or CIDR) allowed to enroll and connect. Seeds the
    /// list on first start; afterwards it is edited through the API / UI.
    #[serde(default)]
    pub allowed_sources: Vec<String>,
    /// Which nodes the allowlist applies to.
    #[serde(default)]
    pub enforce_for: AllowlistScope,
    /// Join token lifetime.
    #[serde(default = "default_token_ttl")]
    pub token_ttl_secs: u64,
    /// Node certificate lifetime; nodes renew at two thirds of it.
    #[serde(default = "default_cert_days")]
    pub cert_days: u32,
}

impl Default for EnrollmentConfig {
    fn default() -> Self {
        EnrollmentConfig {
            require_approval: true,
            allowed_sources: vec![],
            enforce_for: AllowlistScope::default(),
            token_ttl_secs: default_token_ttl(),
            cert_days: default_cert_days(),
        }
    }
}

fn default_token_ttl() -> u64 {
    900
}
fn default_cert_days() -> u32 {
    30
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AllowlistScope {
    /// Only relay (proxy-only) nodes must connect from an allowed source.
    #[default]
    Relay,
    /// Every remote node must.
    All,
    /// The allowlist is not enforced.
    None,
}

fn default_api_listen() -> String {
    "127.0.0.1:7700".into()
}
fn default_grace() -> u64 {
    600
}

/// Task bucket scheduler: how the coordinator ("main agent") is woken up.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskSchedulerConfig {
    /// Agent profile used for the per-task coordinator session.
    #[serde(default = "default_coordinator")]
    pub coordinator_agent: String,
    /// Pin coordinators to a node (default: any node that has the profile).
    #[serde(default)]
    pub coordinator_node: Option<String>,
    /// Upper bound on tasks running at once. `urgent` tasks may use one extra slot.
    #[serde(default = "default_max_tasks")]
    pub max_concurrent_tasks: u32,
    /// `when_free` tasks start only after the cluster has been idle this long.
    #[serde(default = "default_idle_grace")]
    pub idle_grace_secs: u64,
    /// How many times a coordinator that goes idle without calling
    /// `task_complete`/`task_fail` is nudged before the task is marked failed.
    #[serde(default = "default_nudges")]
    pub max_nudges: u32,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for TaskSchedulerConfig {
    fn default() -> Self {
        TaskSchedulerConfig {
            coordinator_agent: default_coordinator(),
            coordinator_node: None,
            max_concurrent_tasks: default_max_tasks(),
            idle_grace_secs: default_idle_grace(),
            max_nudges: default_nudges(),
            enabled: true,
        }
    }
}

fn default_coordinator() -> String {
    "coordinator".into()
}
fn default_max_tasks() -> u32 {
    2
}
fn default_idle_grace() -> u64 {
    30
}
fn default_nudges() -> u32 {
    2
}
fn default_true() -> bool {
    true
}

// ----------------------------------------------------------- providers.yaml

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProvidersFile {
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProviderDef {
    /// Upstream base URL; OpenCode appends e.g. `/chat/completions` to it.
    pub base_url: String,
    /// Wire format, which selects the AI SDK package OpenCode uses.
    #[serde(default)]
    pub api: ApiKind,
    /// How the real credential is attached upstream.
    #[serde(default)]
    pub auth: AuthStyle,
    /// Extra static headers sent upstream (e.g. `HTTP-Referer` for OpenRouter).
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// `best_effort` marks free/keyless gateways without an SLA.
    #[serde(default)]
    pub tier: ProviderTier,
    /// USD per million tokens, used when OpenCode reports no cost.
    #[serde(default)]
    pub prices: BTreeMap<String, Price>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApiKind {
    #[default]
    Openai,
    Anthropic,
    Google,
}

impl ApiKind {
    pub fn npm_package(self) -> &'static str {
        match self {
            ApiKind::Openai => "@ai-sdk/openai-compatible",
            ApiKind::Anthropic => "@ai-sdk/anthropic",
            ApiKind::Google => "@ai-sdk/google",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum AuthStyle {
    /// `Authorization: Bearer <key>`
    #[default]
    Bearer,
    /// `<name>: <key>` (e.g. `x-api-key`, `x-goog-api-key`)
    Header { name: String },
    /// `?<name>=<key>`
    Query { name: String },
    /// Keyless gateway; nothing is attached.
    None,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderTier {
    #[default]
    Standard,
    BestEffort,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Price {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
}

/// Built-in provider templates. `providers.yaml` entries override these by id.
pub fn builtin_providers() -> BTreeMap<String, ProviderDef> {
    fn p(base: &str, api: ApiKind, auth: AuthStyle, tier: ProviderTier, desc: &str) -> ProviderDef {
        ProviderDef {
            base_url: base.into(),
            api,
            auth,
            headers: BTreeMap::new(),
            tier,
            prices: BTreeMap::new(),
            description: Some(desc.into()),
        }
    }
    use ApiKind::*;
    use ProviderTier::*;
    let mut m = BTreeMap::new();
    m.insert("openrouter".into(), p("https://openrouter.ai/api/v1", Openai, AuthStyle::Bearer, Standard, "OpenRouter (BYOK)"));
    m.insert("groq".into(), p("https://api.groq.com/openai/v1", Openai, AuthStyle::Bearer, Standard, "Groq (BYOK)"));
    m.insert("openai".into(), p("https://api.openai.com/v1", Openai, AuthStyle::Bearer, Standard, "OpenAI (BYOK)"));
    m.insert(
        "anthropic".into(),
        p("https://api.anthropic.com/v1", Anthropic, AuthStyle::Header { name: "x-api-key".into() }, Standard, "Anthropic (BYOK)"),
    );
    m.insert(
        "gemini".into(),
        p(
            "https://generativelanguage.googleapis.com/v1beta",
            Google,
            AuthStyle::Header { name: "x-goog-api-key".into() },
            Standard,
            "Google Gemini (BYOK)",
        ),
    );
    m.insert(
        "kilo-free".into(),
        p("https://api.kilo.ai/api/openrouter", Openai, AuthStyle::None, BestEffort, "Kilo gateway free models (keyless, as used by OpenKilo)"),
    );
    m.insert(
        "pollinations".into(),
        p("https://gen.pollinations.ai/v1", Openai, AuthStyle::Bearer, BestEffort, "Pollinations; key optional (omit `key` for anonymous tier)"),
    );
    m.insert(
        "keylessai".into(),
        p("https://keylessai.thryx.workers.dev/v1", Openai, AuthStyle::None, BestEffort, "KeylessAI third-party worker (no SLA)"),
    );
    m
}

// -------------------------------------------------------------- routes.yaml

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoutesFile {
    #[serde(default)]
    pub routes: BTreeMap<String, Route>,
}

/// Which IP the LLM provider sees. Applies only to keyproxy's upstream calls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum Route {
    Direct,
    /// Tunnel out through another cluster node (which must have `relay: true`). Phase 2.
    ViaNode { node: String },
    /// Any HTTP(S)/SOCKS5 proxy, e.g. `socks5h://127.0.0.1:1080` from `ssh -D`.
    Proxy { url: String },
}

// ------------------------------------------------------------- agents/*.yaml

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    #[serde(rename = "apiVersion", default = "default_api_version")]
    pub api_version: String,
    #[serde(default = "default_kind_agent")]
    pub kind: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub provider: ProviderRef,
    /// Egress route name from routes.yaml (default: node's default_route, else direct).
    #[serde(default)]
    pub route: Option<String>,
    #[serde(default)]
    pub role: Role,
    #[serde(default)]
    pub tools: ToolPolicy,
    /// Extra OpenCode plugins (npm specs) — they must be present in the image.
    #[serde(default)]
    pub plugins: Vec<String>,
    #[serde(default)]
    pub container: ContainerSpec,
    #[serde(default)]
    pub workspace: WorkspaceSpec,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub success: SuccessSpec,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

fn default_api_version() -> String {
    "ocl/v1".into()
}
fn default_kind_agent() -> String {
    "Agent".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProviderRef {
    /// Provider id from providers.yaml / built-ins.
    pub id: String,
    pub model: String,
    /// Where the API key comes from. Omit for keyless providers.
    #[serde(default)]
    pub key: Option<SecretRef>,
    /// Model context window / output limits advertised to OpenCode.
    #[serde(default)]
    pub context: Option<u64>,
    #[serde(default)]
    pub max_output: Option<u64>,
}

/// Reference to a secret held on the node: `env:NAME`, `file:/path`,
/// `systemd-creds:NAME` (from `$CREDENTIALS_DIRECTORY`), `pass:entry`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SecretRef {
    #[serde(rename = "ref")]
    pub reference: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Role {
    /// Appended to OpenCode's own system prompt as instructions.
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Path relative to the config dir.
    #[serde(default)]
    pub system_prompt_file: Option<PathBuf>,
    /// Built-in OpenCode agent to start in (`build`, `plan`, ...).
    #[serde(default = "default_oc_agent")]
    pub opencode_agent: String,
}

fn default_oc_agent() -> String {
    "build".into()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Perm {
    #[default]
    Allow,
    Ask,
    Deny,
}

/// A permission: either one value for everything, or per-pattern rules (bash).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum PermRule {
    Simple(Perm),
    Patterns {
        #[serde(default)]
        default: Perm,
        #[serde(default)]
        allow: Vec<String>,
        #[serde(default)]
        ask: Vec<String>,
        #[serde(default)]
        deny: Vec<String>,
    },
}

impl Default for PermRule {
    fn default() -> Self {
        PermRule::Simple(Perm::Allow)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolPolicy {
    #[serde(default)]
    pub edit: PermRule,
    #[serde(default)]
    pub bash: PermRule,
    #[serde(default = "deny_rule")]
    pub webfetch: PermRule,
    /// Cluster MCP tools this agent may call. Default: all except coordinator-only ones.
    #[serde(default)]
    pub cluster: Option<Vec<String>>,
}

fn deny_rule() -> PermRule {
    PermRule::Simple(Perm::Deny)
}

impl Default for ToolPolicy {
    fn default() -> Self {
        ToolPolicy { edit: PermRule::default(), bash: PermRule::default(), webfetch: deny_rule(), cluster: None }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContainerSpec {
    /// Image override (default: node runtime.image).
    #[serde(default)]
    pub image: Option<String>,
    /// Podman memory limit, e.g. `1g`.
    #[serde(default = "default_memory")]
    pub memory: String,
    #[serde(default = "default_cpus")]
    pub cpus: f32,
    #[serde(default = "default_pids")]
    pub pids: u32,
    /// `none` (default): no network at all, everything goes through the node socket.
    #[serde(default)]
    pub egress: Egress,
}

impl Default for ContainerSpec {
    fn default() -> Self {
        ContainerSpec { image: None, memory: default_memory(), cpus: default_cpus(), pids: default_pids(), egress: Egress::None }
    }
}

fn default_memory() -> String {
    "1g".into()
}
fn default_cpus() -> f32 {
    1.0
}
fn default_pids() -> u32 {
    512
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Egress {
    #[default]
    None,
    /// Container gets normal networking (package installs, webfetch). Use sparingly.
    Full,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSpec {
    #[serde(default)]
    pub mode: WorkspaceMode,
    /// `shared` mode: sessions with the same name share one workspace (per node).
    #[serde(default)]
    pub name: Option<String>,
}

impl Default for WorkspaceSpec {
    fn default() -> Self {
        WorkspaceSpec { mode: WorkspaceMode::Isolated, name: None }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMode {
    #[default]
    Isolated,
    Shared,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Concurrent sessions of this agent on one node.
    #[serde(default = "default_agent_sessions")]
    pub max_sessions: u32,
    /// Per-session spend cap, enforced by keyproxy.
    #[serde(default)]
    pub budget_usd: Option<f64>,
    /// Per-session token cap (input + output), enforced by keyproxy.
    #[serde(default)]
    pub max_tokens: Option<u64>,
    /// Wall-clock limit per session in seconds.
    #[serde(default)]
    pub wall_clock_secs: Option<u64>,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_sessions: default_agent_sessions(), budget_usd: None, max_tokens: None, wall_clock_secs: None }
    }
}

fn default_agent_sessions() -> u32 {
    2
}

/// How a session's (or workflow step's) success is judged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SuccessSpec {
    /// Require every todo to be `completed` (none pending/in progress/cancelled).
    #[serde(default = "default_true")]
    pub todos_required: bool,
    /// Todos the agent must adopt; they are seeded into its prompt.
    #[serde(default)]
    pub todos: Vec<String>,
    /// Re-prompts when the agent stops with open todos.
    #[serde(default = "default_nudges")]
    pub max_nudges: u32,
    /// e.g. `[{exit_code: "pytest -q"}, {text_contains: "APPROVED"}]`
    #[serde(default, with = "serde_yaml::with::singleton_map_recursive")]
    #[schemars(with = "Vec<Check>")]
    pub checks: Vec<Check>,
}

impl Default for SuccessSpec {
    fn default() -> Self {
        SuccessSpec { todos_required: true, todos: vec![], max_nudges: default_nudges(), checks: vec![] }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Check {
    /// Run a shell command inside the session's container; exit 0 = pass.
    ExitCode(String),
    /// The agent's final message must contain this text (e.g. `VERDICT: APPROVED`).
    TextContains(String),
}

// ---------------------------------------------------------- workflows/*.yaml

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowConfig {
    #[serde(rename = "apiVersion", default = "default_api_version")]
    pub api_version: String,
    #[serde(default = "default_kind_workflow")]
    pub kind: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Workspace used by every step (default: shared, named after the run).
    #[serde(default)]
    pub workspace: Option<WorkspaceSpec>,
    pub steps: Vec<WorkflowStep>,
    /// Upper bound on step executions per run (guards review<->build loops).
    #[serde(default = "default_max_steps")]
    pub max_step_runs: u32,
}

fn default_kind_workflow() -> String {
    "Workflow".into()
}
fn default_max_steps() -> u32 {
    8
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowStep {
    pub name: String,
    pub agent: String,
    /// Template. Variables: `{{input}}`, `{{prev.output}}`, `{{prev.step}}`,
    /// `{{prev.todos}}`, `{{run.id}}`, `{{feedback}}` (set when re-entered via on_fail).
    pub prompt: String,
    #[serde(default)]
    pub node: Option<String>,
    #[serde(default)]
    pub success: Option<SuccessSpec>,
    /// Next step on PASS (default: the following step; `end` finishes the run).
    #[serde(default)]
    pub next: Option<String>,
    /// Step to go to on FAIL (default: the run fails).
    #[serde(default)]
    pub on_fail: Option<String>,
}

// ------------------------------------------------------------------ loading

pub fn load_yaml<T: serde::de::DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
    let text = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
    serde_yaml::from_str(&text).map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))
}

/// Load every `*.yaml`/`*.yml` in `dir`, returning (path, parsed) or per-file errors.
pub fn load_dir<T: serde::de::DeserializeOwned>(dir: &Path) -> Vec<(PathBuf, anyhow::Result<T>)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    let mut paths: Vec<PathBuf> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("yaml" | "yml")))
        .collect();
    paths.sort();
    for p in paths {
        let r = load_yaml(&p);
        out.push((p, r));
    }
    out
}

pub fn valid_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 48 && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

impl AgentConfig {
    /// Static validation that does not need node context.
    pub fn validate(&self, providers: &BTreeMap<String, ProviderDef>) -> Vec<String> {
        let mut errs = vec![];
        if !valid_name(&self.name) {
            errs.push(format!("agent name {:?} must match [a-z0-9-]{{1,48}}", self.name));
        }
        match providers.get(&self.provider.id) {
            None => errs.push(format!("unknown provider {:?}", self.provider.id)),
            Some(p) => {
                if p.auth != AuthStyle::None && self.provider.key.is_none() && self.provider.id != "pollinations" {
                    errs.push(format!("provider {:?} needs provider.key (a secret ref)", self.provider.id));
                }
            }
        }
        if self.workspace.mode == WorkspaceMode::Shared && self.workspace.name.is_none() {
            errs.push("workspace.mode=shared requires workspace.name".into());
        }
        errs
    }
}

/// JSON Schemas for editor validation (`opencode-node schema`).
pub fn schemas() -> BTreeMap<&'static str, serde_json::Value> {
    let mut m = BTreeMap::new();
    m.insert("agent", serde_json::to_value(schemars::schema_for!(AgentConfig)).unwrap());
    m.insert("node", serde_json::to_value(schemars::schema_for!(NodeConfig)).unwrap());
    m.insert("supervisor", serde_json::to_value(schemars::schema_for!(SupervisorConfig)).unwrap());
    m.insert("providers", serde_json::to_value(schemars::schema_for!(ProvidersFile)).unwrap());
    m.insert("routes", serde_json::to_value(schemars::schema_for!(RoutesFile)).unwrap());
    m.insert("workflow", serde_json::to_value(schemars::schema_for!(WorkflowConfig)).unwrap());
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_yaml_roundtrip() {
        let y = r#"
name: builder
description: writes code
provider: { id: openrouter, model: qwen/qwen3-coder, key: { ref: "env:OPENROUTER_KEY" } }
route: via-vps
tools:
  bash: { default: ask, allow: ["pytest *"] }
workspace: { mode: shared, name: demo }
success: { todos: ["write module"], checks: [ { exit_code: "pytest -q" } ] }
"#;
        let a: AgentConfig = serde_yaml::from_str(y).unwrap();
        assert_eq!(a.provider.model, "qwen/qwen3-coder");
        assert!(matches!(a.tools.bash, PermRule::Patterns { default: Perm::Ask, .. }));
        assert_eq!(a.tools.webfetch, PermRule::Simple(Perm::Deny));
        assert_eq!(a.success.checks, vec![Check::ExitCode("pytest -q".into())]);
        assert!(a.validate(&builtin_providers()).is_empty());
    }

    #[test]
    fn keyless_needs_no_key() {
        let a: AgentConfig = serde_yaml::from_str("name: free\nprovider: { id: kilo-free, model: auto }").unwrap();
        assert!(a.validate(&builtin_providers()).is_empty());
        let b: AgentConfig = serde_yaml::from_str("name: paid\nprovider: { id: groq, model: x }").unwrap();
        assert_eq!(b.validate(&builtin_providers()).len(), 1);
    }

    #[test]
    fn routes_parse() {
        let r: RoutesFile = serde_yaml::from_str(
            "routes:\n  a: { type: direct }\n  b: { type: via_node, node: vps-de }\n  c: { type: proxy, url: 'socks5h://x:1' }",
        )
        .unwrap();
        assert_eq!(r.routes["b"], Route::ViaNode { node: "vps-de".into() });
    }
}
