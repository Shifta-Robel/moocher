//! On-disk configuration formats (YAML). See docs/CONFIG.md.
//!
//! Layout of a config directory (default `~/.moocher`):
//!
//! ```text
//! node.yaml            NodeConfig        (every machine running moocher-node)
//! supervisor.yaml      SupervisorConfig  (hub only)
//! providers.yaml       ProvidersFile     (optional; merged over built-in defaults)
//! routes.yaml          RoutesFile        (optional)
//! overrides.yaml       OverridesFile     (optional; written by the node for live UI/CLI changes)
//! agents/*.yaml        AgentConfig       (one file per agent profile)
//! workflows/*.yaml     WorkflowConfig    (hub only)
//! templates/*.yaml     AgentConfig       (hub only; pushed to every node)
//! preambles/cluster.md                   (hub only; first part of every agent's instructions)
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
    /// Where the supervisor is: `unix:///run/moo/supervisor.sock` (same machine)
    /// or `https://hub.example:7443` (remote, mTLS; set up by `moocher-node join`).
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
    /// How `container.disk` is enforced: `soft` (default: measured, the agent
    /// is warned at 90% and interrupted at 100%) or `hard` (also a filesystem
    /// quota: XFS with prjquota or btrfs, node running as root).
    #[serde(default)]
    pub disk_enforce: DiskEnforce,
    /// Allow other nodes to send LLM traffic out through this machine (`via_node` routes).
    #[serde(default)]
    pub relay: bool,
    /// Relaying: destination ports allowed (default 443).
    #[serde(default = "default_relay_ports")]
    pub relay_ports: Vec<u16>,
    /// Relaying: destination hosts allowed (`api.groq.com`, `*.example.com`). Empty = any.
    #[serde(default)]
    pub relay_allow_hosts: Vec<String>,
    /// Local address outbound LLM traffic leaves from, on machines with several
    /// IPs (applies to `direct` routes and to traffic relayed for others).
    #[serde(default)]
    pub egress_bind: Option<String>,
    /// Accept agent templates pushed by the hub (`templates/`). With `false`
    /// this node runs only its own agents/, whatever the hub sends.
    #[serde(default = "default_true")]
    pub hub_templates: bool,
    /// Secret aliases for hub templates: `openrouter: { ref: "env:OPENROUTER_API_KEY", account: personal }`.
    /// A template names the alias (`key: { secret: openrouter }`); each node maps it to its own secret.
    #[serde(default)]
    pub secrets: BTreeMap<String, SecretRef>,
    /// URLs that answer with the caller's public IP, used to learn the egress
    /// IP of each route (per-IP quotas). Empty disables probing; remote nodes
    /// still learn their direct IP from the hub.
    #[serde(default = "default_ip_echo")]
    pub ip_echo: Vec<String>,
    /// Default egress route for agents that do not set one.
    #[serde(default)]
    pub default_route: Option<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Send a checkpoint (workspace snapshot + transcript) to the hub every N
    /// finished turns, so a session can resume elsewhere if this machine dies.
    /// 0 turns checkpoints off.
    #[serde(default = "default_checkpoint_turns")]
    pub checkpoint_every_turns: u32,
    /// Local artifact cache size; least recently used blobs are dropped beyond it.
    #[serde(default = "default_cache_size")]
    pub artifact_cache: String,
}

fn default_checkpoint_turns() -> u32 {
    3
}
fn default_cache_size() -> String {
    "2g".into()
}

fn default_node_max_sessions() -> u32 {
    6
}

fn default_relay_ports() -> Vec<u16> {
    vec![443]
}

fn default_ip_echo() -> Vec<String> {
    vec!["https://api.ipify.org".into()]
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
    /// Directory for per-session sockets. Default: `$XDG_RUNTIME_DIR/moo` or `/run/moo`.
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
pub const DEFAULT_IMAGE: &str = "localhost/moocher-agent:1.19.4";
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
    #[serde(default)]
    pub placement: PlacementWeights,
    /// Database and artifact directory. Default: `<config dir>/supervisor`.
    #[serde(default)]
    pub data_dir: Option<PathBuf>,
    #[serde(default)]
    pub tasks: TaskSchedulerConfig,
    /// Seconds a disconnected node's sessions stay `unreachable` before becoming `lost`.
    #[serde(default = "default_grace")]
    pub disconnect_grace_secs: u64,
    /// Questions from agents: who sees what (PHASE1_5 §6).
    #[serde(default)]
    pub questions: QuestionsConfig,
    /// What happens when an agent dies (PHASE1_5 §8). Templates may override it.
    #[serde(default)]
    pub on_death: DeathPolicy,
    #[serde(default)]
    pub artifacts: ArtifactsConfig,
    #[serde(default)]
    pub events: EventsConfig,
    /// HTTPS for the web UI and API: certificate files, or ACME (Let's
    /// Encrypt or any RFC 8555 CA). Without it the API is plain HTTP (keep it
    /// on loopback or behind a TLS proxy).
    #[serde(default)]
    pub tls: Option<TlsConfig>,
    /// Reverse proxies (IPs/CIDRs) whose `X-Forwarded-For` / `X-Forwarded-Proto` are trusted.
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
    /// Second listener for "Open in OpenCode" (its own origin: same host,
    /// another port), e.g. `0.0.0.0:7444`. Off when unset.
    #[serde(default)]
    pub attach_listen: Option<String>,
    /// Public base URL of the attach listener, e.g. `https://hub.example.org:7444`.
    /// Default: the dashboard's host with the attach port.
    #[serde(default)]
    pub attach_url: Option<String>,
    #[serde(default)]
    pub login: LoginConfig,
    #[serde(default)]
    pub notifications: NotificationsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    /// PEM certificate chain and key (bring your own certificate).
    #[serde(default)]
    pub cert_file: Option<PathBuf>,
    #[serde(default)]
    pub key_file: Option<PathBuf>,
    /// Get and renew the certificate automatically (HTTP-01).
    #[serde(default)]
    pub acme: Option<AcmeConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AcmeConfig {
    /// Names on the certificate; they must resolve to this machine.
    pub domains: Vec<String>,
    #[serde(default)]
    pub email: Option<String>,
    /// ACME directory. Default: Let's Encrypt production.
    #[serde(default = "default_acme_directory")]
    pub directory: String,
    /// Where the CA connects for HTTP-01 challenges (port 80 on the public
    /// address). It also redirects everything else to HTTPS.
    #[serde(default = "default_http01")]
    pub http01_listen: String,
    /// Extra CA certificate (PEM) to trust for the ACME directory (test CAs such as Pebble).
    #[serde(default)]
    pub directory_ca: Option<PathBuf>,
}

fn default_acme_directory() -> String {
    "https://acme-v02.api.letsencrypt.org/directory".into()
}
fn default_http01() -> String {
    "0.0.0.0:80".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LoginConfig {
    /// Lifetime of a browser session.
    #[serde(default = "default_session_hours")]
    pub session_hours: u64,
    /// Failed logins for one account (or one IP: 4×) within 15 min before a lockout.
    #[serde(default = "default_max_failures")]
    pub max_failures: u32,
    /// First lockout; it doubles with every further lockout (max 24 h).
    #[serde(default = "default_lockout")]
    pub lockout_secs: u64,
}

fn default_session_hours() -> u64 {
    12
}
fn default_max_failures() -> u32 {
    5
}
fn default_lockout() -> u64 {
    900
}

impl Default for LoginConfig {
    fn default() -> Self {
        LoginConfig { session_hours: default_session_hours(), max_failures: default_max_failures(), lockout_secs: default_lockout() }
    }
}

/// Who hears about what (PHASE1_5 §11). The in-app centre always gets
/// everything; the rules pick which events also go to push, ntfy and webhooks.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NotificationsConfig {
    /// `{ from: "22:00", to: "07:00" }` local time: only `vital` goes out then.
    #[serde(default)]
    pub quiet_hours: Option<QuietHours>,
    /// First matching rule wins. Default: warning and vital to every configured channel.
    #[serde(default)]
    pub rules: Vec<NotifyRule>,
    /// Web Push (the UI is a PWA). The VAPID key pair is generated on first start.
    #[serde(default)]
    pub web_push: Option<WebPushConfig>,
    /// e.g. `{ url: "https://ntfy.sh/my-topic", token: "env:NTFY_TOKEN" }`
    #[serde(default)]
    pub ntfy: Option<NtfyConfig>,
    /// POST the notification as JSON; signed with HMAC-SHA256 when `secret` is set.
    #[serde(default)]
    pub webhook: Option<WebhookConfig>,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        NotificationsConfig { quiet_hours: None, rules: vec![], web_push: Some(WebPushConfig::default()), ntfy: None, webhook: None }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuietHours {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NotifyRule {
    /// Event kinds (`question.waiting`, `task.halted`, `node.offline`, …); `*` or empty = all.
    #[serde(default)]
    pub kinds: Vec<String>,
    /// `info`, `warning` or `vital`.
    #[serde(default = "default_min_severity")]
    pub min_severity: String,
    /// `push`, `ntfy`, `webhook`; empty = in-app only.
    #[serde(default)]
    pub channels: Vec<String>,
}

fn default_min_severity() -> String {
    "warning".into()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WebPushConfig {
    /// VAPID contact (`mailto:you@example.org` or an https URL).
    #[serde(default)]
    pub subject: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NtfyConfig {
    pub url: String,
    /// Access token reference (`env:NAME` or `file:/path`), optional.
    #[serde(default)]
    pub token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WebhookConfig {
    pub url: String,
    /// HMAC secret reference (`env:NAME` or `file:/path`): adds `X-Moo-Signature: sha256=<hex>`.
    #[serde(default)]
    pub secret: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionsConfig {
    /// Starting mode; switch it live with `moo questions mode` (per task too).
    #[serde(default)]
    pub mode: crate::model::QuestionMode,
    /// An agent holding a question for this long without answering or
    /// escalating passes it up automatically.
    #[serde(default = "default_answer_timeout")]
    pub answer_timeout_secs: u64,
    /// Words that make a question `vital` (the rules only ever raise importance).
    #[serde(default = "default_vital_words")]
    pub vital_keywords: Vec<String>,
    /// Words that make a question operator-only: no agent may answer it.
    #[serde(default = "default_operator_words")]
    pub operator_only_keywords: Vec<String>,
}

fn default_answer_timeout() -> u64 {
    300
}

fn default_vital_words() -> Vec<String> {
    [
        "spend", "money", "pay for", "purchase", "invoice", "billing", "credit card", "budget", "credential", "password", "secret", "api key",
        "delete", "drop table", "rm -rf", "wipe", "irreversible", "cannot be undone", "force push", "push --force", "deploy", "production",
        "publish", "send email",
    ]
    .map(String::from)
    .to_vec()
}

fn default_operator_words() -> Vec<String> {
    ["credential", "password", "secret", "api key", "private key"].map(String::from).to_vec()
}

impl Default for QuestionsConfig {
    fn default() -> Self {
        QuestionsConfig {
            mode: Default::default(),
            answer_timeout_secs: default_answer_timeout(),
            vital_keywords: default_vital_words(),
            operator_only_keywords: default_operator_words(),
        }
    }
}

/// Recovery policy for dead agents. The cheapest recovery runs automatically
/// at most `auto_retry` times, unless it would cost more than
/// `max_auto_cost_usd` or `max_auto_fraction` of what the task has spent;
/// then the task is halted and the operator chooses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeathPolicy {
    #[serde(default = "default_auto_retry")]
    pub auto_retry: u32,
    #[serde(default = "default_max_auto_cost")]
    pub max_auto_cost_usd: f64,
    #[serde(default = "default_max_auto_fraction")]
    pub max_auto_fraction: f64,
    /// $0 providers: halt instead when a retry would use more than this
    /// fraction of the allowance left.
    #[serde(default = "default_max_auto_fraction")]
    pub keyless_quota_fraction: f64,
    /// A fresh agent costs about this fraction of what was spent so far.
    #[serde(default = "default_handoff_factor")]
    pub handoff_factor: f64,
    /// Deaths within 10 minutes that count as a crash loop (no more auto restarts).
    #[serde(default = "default_crashloop")]
    pub crashloop_deaths: u32,
}

fn default_auto_retry() -> u32 {
    1
}
fn default_max_auto_cost() -> f64 {
    0.50
}
fn default_max_auto_fraction() -> f64 {
    0.25
}
fn default_handoff_factor() -> f64 {
    0.5
}
fn default_crashloop() -> u32 {
    3
}

impl Default for DeathPolicy {
    fn default() -> Self {
        DeathPolicy {
            auto_retry: default_auto_retry(),
            max_auto_cost_usd: default_max_auto_cost(),
            max_auto_fraction: default_max_auto_fraction(),
            keyless_quota_fraction: default_max_auto_fraction(),
            handoff_factor: default_handoff_factor(),
            crashloop_deaths: default_crashloop(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtifactsConfig {
    /// Largest single artifact accepted (e.g. `2g`).
    #[serde(default = "default_max_artifact")]
    pub max_size: String,
    /// Delete artifacts older than this (`90d`), unless tagged with one of
    /// `keep_tags` or still referenced (live sessions, open tasks and runs,
    /// halts, checkpoints, shared workspaces, config files). Default: keep all.
    #[serde(default)]
    pub retain: Option<String>,
    #[serde(default = "default_keep_tags")]
    pub keep_tags: Vec<String>,
}

fn default_keep_tags() -> Vec<String> {
    vec!["keep".into()]
}

/// The event log: exported daily, pruned from the database after `retain`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EventsConfig {
    /// Write `exports/events-YYYYMMDD.jsonl.zst` (one JSON event per line).
    #[serde(default = "default_true")]
    pub export: bool,
    /// How long events stay in the database. Only exported events are pruned.
    #[serde(default = "default_events_retain")]
    pub retain: String,
    /// Delete export files older than this. Default: keep them.
    #[serde(default)]
    pub export_keep: Option<String>,
}

fn default_events_retain() -> String {
    "30d".into()
}

impl Default for EventsConfig {
    fn default() -> Self {
        EventsConfig { export: true, retain: default_events_retain(), export_keep: None }
    }
}

fn default_max_artifact() -> String {
    "2g".into()
}

impl Default for ArtifactsConfig {
    fn default() -> Self {
        ArtifactsConfig { max_size: default_max_artifact(), retain: None, keep_tags: default_keep_tags() }
    }
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        SupervisorConfig {
            api_listen: default_api_listen(),
            link_socket: None,
            link_listen: None,
            public_names: vec![],
            enrollment: EnrollmentConfig::default(),
            placement: PlacementWeights::default(),
            data_dir: None,
            tasks: TaskSchedulerConfig::default(),
            disconnect_grace_secs: default_grace(),
            questions: QuestionsConfig::default(),
            on_death: DeathPolicy::default(),
            artifacts: ArtifactsConfig::default(),
            events: EventsConfig::default(),
            tls: None,
            trusted_proxies: vec![],
            attach_listen: None,
            attach_url: None,
            login: LoginConfig::default(),
            notifications: NotificationsConfig::default(),
        }
    }
}

/// Weights of the placement score (PHASE1_5 §5). Each part is 0..1.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlacementWeights {
    /// Free session slots.
    #[serde(default = "w_capacity")]
    pub capacity: f64,
    /// Provider allowance left on the node's routes.
    #[serde(default = "w_quota")]
    pub quota: f64,
    /// Provider latency over the node's preferred route.
    #[serde(default = "w_latency")]
    pub latency: f64,
    /// Same node as the parent session or the shared workspace.
    #[serde(default = "w_locality")]
    pub locality: f64,
    /// Round trip time between hub and node.
    #[serde(default = "w_link")]
    pub link: f64,
}

fn w_capacity() -> f64 {
    0.30
}
fn w_quota() -> f64 {
    0.25
}
fn w_latency() -> f64 {
    0.20
}
fn w_locality() -> f64 {
    0.15
}
fn w_link() -> f64 {
    0.10
}

impl Default for PlacementWeights {
    fn default() -> Self {
        PlacementWeights { capacity: w_capacity(), quota: w_quota(), latency: w_latency(), locality: w_locality(), link: w_link() }
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
    /// Usage limits and how they are tracked (PHASE1_5 §3). Without this,
    /// limits are still learned from rate-limit headers and 429 responses.
    #[serde(default)]
    pub quota: Option<QuotaSpec>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuotaSpec {
    /// What the provider counts against. Default: `ip` for keyless providers, `key` otherwise.
    #[serde(default)]
    pub scope: Option<QuotaScope>,
    #[serde(default)]
    pub limits: Vec<QuotaLimit>,
    /// Signals trusted over the configured limits.
    #[serde(default = "default_learn")]
    pub learn: Vec<QuotaLearn>,
}

fn default_learn() -> Vec<QuotaLearn> {
    vec![QuotaLearn::Headers, QuotaLearn::Http429]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QuotaScope {
    /// Per egress IP (keyless gateways).
    Ip,
    /// Per API key.
    Key,
    /// Per account: keys with the same `account` label share one bucket.
    Account,
}

impl QuotaScope {
    pub fn as_str(self) -> &'static str {
        match self {
            QuotaScope::Ip => "ip",
            QuotaScope::Key => "key",
            QuotaScope::Account => "account",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QuotaLearn {
    /// `x-ratelimit-*`, `anthropic-ratelimit-*`, `retry-after`.
    Headers,
    /// A 429 marks the bucket exhausted until its reset / retry-after.
    #[serde(rename = "http_429")]
    Http429,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QuotaKind {
    Requests,
    Tokens,
}

impl QuotaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            QuotaKind::Requests => "requests",
            QuotaKind::Tokens => "tokens",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuotaLimit {
    pub kind: QuotaKind,
    pub max: u64,
    /// Window length: `60s`, `5h`, `1d`.
    pub window: String,
    /// Fixed windows: wall-clock time a window starts, `HH:MM` (e.g. daily reset at `00:00`).
    #[serde(default)]
    pub at: Option<String>,
    /// Time zone of `at`: `UTC` (default) or an offset like `+02:00` / `-07:00`.
    #[serde(default)]
    pub tz: Option<String>,
    /// Rolling window (usage expires `window` after it happened) instead of fixed.
    #[serde(default)]
    pub rolling: bool,
}

/// `90s`, `5m`, `5h`, `1d`, `1h30m`, `250ms` -> milliseconds.
pub fn parse_duration_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(secs) = s.parse::<f64>() {
        return Some((secs * 1000.0) as i64);
    }
    let mut total = 0f64;
    let mut num = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
            continue;
        }
        let n: f64 = num.parse().ok()?;
        num.clear();
        let unit = if c == 'm' && chars.peek() == Some(&'s') {
            chars.next();
            0.001
        } else {
            match c {
                's' => 1.0,
                'm' => 60.0,
                'h' => 3600.0,
                'd' => 86400.0,
                _ => return None,
            }
        };
        total += n * unit;
    }
    if !num.is_empty() {
        return None;
    }
    Some((total * 1000.0) as i64)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApiKind {
    #[default]
    Openai,
    Anthropic,
    Google,
    /// OpenCode's own built-in `opencode` provider (Zen free models, no key):
    /// OpenCode calls it itself, so traffic skips the keyproxy and the agent
    /// needs `container.egress: allowlist` to opencode.ai.
    Opencode,
}

impl ApiKind {
    pub fn npm_package(self) -> &'static str {
        match self {
            ApiKind::Openai | ApiKind::Opencode => "@ai-sdk/openai-compatible",
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
            quota: None,
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
        "opencode-free".into(),
        p("https://opencode.ai/zen/v1", Opencode, AuthStyle::None, BestEffort, "OpenCode Zen free models, called by OpenCode itself without a key"),
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
    /// Tunnel out through another cluster node (`relay: true` or `role: relay`).
    ViaNode { node: String },
    /// Any HTTP(S)/SOCKS5 proxy, e.g. `socks5h://127.0.0.1:1080` from `ssh -D`.
    Proxy { url: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum RouteSpec {
    Name(String),
    Plan(RoutePlan),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoutePlan {
    #[serde(default = "default_direct")]
    pub prefer: String,
    /// Tried in order when the provider's bucket for the current route is exhausted.
    #[serde(default)]
    pub failover: Vec<String>,
    #[serde(default)]
    pub on_exhausted: OnExhausted,
}

fn default_direct() -> String {
    "direct".into()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OnExhausted {
    /// Park until the soonest reset (OpenCode retries after `retry-after`).
    #[default]
    Wait,
    /// End the turn with an error so a coordinator can reassign the work.
    Fail,
}

impl RouteSpec {
    pub fn plan(&self) -> RoutePlan {
        match self {
            RouteSpec::Name(n) => RoutePlan { prefer: n.clone(), failover: vec![], on_exhausted: OnExhausted::Wait },
            RouteSpec::Plan(p) => p.clone(),
        }
    }
    /// Every route name this spec mentions.
    pub fn names(&self) -> Vec<String> {
        let p = self.plan();
        std::iter::once(p.prefer).chain(p.failover).collect()
    }
}

// ----------------------------------------------------------- overrides.yaml

/// Live changes made from the UI/CLI, kept apart from the version-controlled
/// files. The node writes it; `promote` copies a change into the real YAML.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OverridesFile {
    /// Agent name -> route spec (replaces the agent's `route`).
    #[serde(default)]
    pub routes: BTreeMap<String, RouteSpec>,
    #[serde(default)]
    pub quotas: Vec<QuotaOverride>,
}

/// "On this node, provider X's bucket resets at T / has N left."
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuotaOverride {
    pub provider: String,
    /// One bucket (an IP, `key:…`, `account:…`), or every bucket of the provider on this node.
    #[serde(default)]
    pub scope_id: Option<String>,
    /// Unix millis. Until then the override applies; afterwards it expires.
    #[serde(default)]
    pub resets_at: Option<i64>,
    /// Requests left until `resets_at` (0 = exhausted until then).
    #[serde(default)]
    pub remaining: Option<u64>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub set_by: Option<String>,
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
    /// Egress route: a name from routes.yaml (default: node's default_route,
    /// else `direct`), or `{prefer, failover, on_exhausted}` to switch to
    /// other routes (other IPs) when a provider allowance runs out.
    #[serde(default)]
    pub route: Option<RouteSpec>,
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
    /// Cluster-wide limits on instances of this template.
    #[serde(default)]
    pub instances: InstanceLimits,
    /// What whoever spawns an instance (operator or coordinator) may change.
    #[serde(default)]
    pub overridable: Overridable,
    /// Files collected when the instance stops, dies or its task ends.
    #[serde(default)]
    pub outputs: Vec<OutputSpec>,
    /// Also keep the conversation as an artifact when the instance ends.
    #[serde(default)]
    pub transcript: bool,
    /// How long the container's volumes are kept after it ends: a duration
    /// (`24h`, the default), `on_failure` (24 h, only after a failure or death),
    /// `always` or `never`.
    #[serde(default = "default_retain")]
    pub retain: String,
    /// Overrides the supervisor's `on_death` for this template.
    #[serde(default)]
    pub on_death: Option<DeathPolicy>,
}

fn default_retain() -> String {
    "24h".into()
}

/// One harvested output: `{ path: "dist/**", to: parent_workspace, dest: "from-builder/" }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OutputSpec {
    /// File, directory or glob relative to the workspace (`report.md`, `dist/`, `**/*.whl`).
    pub path: String,
    /// `artifact` (default), `parent_workspace`, `task_workspace` or
    /// `workspace:<name>` (a shared workspace).
    #[serde(default = "default_output_to")]
    pub to: String,
    /// Directory inside the destination workspace (default: same relative path).
    #[serde(default)]
    pub dest: Option<String>,
    /// Artifact name (default: the path).
    #[serde(default)]
    pub name: Option<String>,
}

fn default_output_to() -> String {
    "artifact".into()
}

/// When a stopped instance's volumes are removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retain {
    /// Keep for this many milliseconds.
    For(i64),
    /// Keep for 24 h only if the instance failed or died; otherwise remove at once.
    OnFailure,
    Always,
    Never,
}

impl Retain {
    pub fn parse(s: &str) -> Option<Retain> {
        Some(match s.trim() {
            "always" => Retain::Always,
            "never" | "0" => Retain::Never,
            "on_failure" | "on-failure" => Retain::OnFailure,
            d => Retain::For(parse_duration_ms(d)?),
        })
    }
    /// Milliseconds to keep after the end, `None` = forever.
    pub fn keep_ms(self, failed: bool) -> Option<i64> {
        match self {
            Retain::For(ms) => Some(ms),
            Retain::OnFailure => Some(if failed { 86_400_000 } else { 0 }),
            Retain::Always => None,
            Retain::Never => Some(0),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InstanceLimits {
    /// Live instances across the whole cluster.
    #[serde(default)]
    pub max: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Overridable {
    /// Extra instructions for one instance.
    #[serde(default)]
    pub role_append: bool,
    /// Highest per-instance budget a spawner may set.
    #[serde(default)]
    pub budget_usd: Option<f64>,
    /// Route names a spawner may choose from.
    #[serde(default)]
    pub routes: Vec<String>,
}

/// Per-instance changes requested at spawn, checked against `overridable`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SpawnOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_append: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
}

impl SpawnOverrides {
    pub fn is_empty(&self) -> bool {
        self == &SpawnOverrides::default()
    }

    /// Why these overrides are not allowed for `agent`, if they are not.
    pub fn check(&self, agent: &AgentConfig) -> Result<(), String> {
        let o = &agent.overridable;
        if self.role_append.is_some() && !o.role_append {
            return Err(format!("agent {} does not allow role_append (overridable.role_append)", agent.name));
        }
        if let Some(b) = self.budget_usd {
            match o.budget_usd {
                Some(max) if b <= max && b > 0.0 => {}
                Some(max) => return Err(format!("budget_usd {b} is outside 0..{max} allowed for {}", agent.name)),
                None => return Err(format!("agent {} does not allow a budget override (overridable.budget_usd)", agent.name)),
            }
        }
        if let Some(r) = &self.route {
            if !o.routes.contains(r) {
                return Err(format!("route {r:?} is not in {}'s overridable.routes {:?}", agent.name, o.routes));
            }
        }
        Ok(())
    }
}

fn default_api_version() -> String {
    "moo/v1".into()
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
/// `systemd-creds:NAME` (from `$CREDENTIALS_DIRECTORY`), `pass:entry` —
/// or `secret: <alias>`, resolved through the node's `secrets:` map.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SecretRef {
    #[serde(rename = "ref", default, skip_serializing_if = "String::is_empty")]
    pub reference: String,
    /// Alias from node.yaml `secrets:` (hub templates use these).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    /// Usage of keys with the same label is shown as one account (scope `account`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
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
    /// Template-specific preamble, after the cluster preamble and before the
    /// role prompt. Variables as in preambles/cluster.md.
    #[serde(default)]
    pub preamble: Option<String>,
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
    /// `allowlist`: still no network, but an HTTP(S) proxy on the node lets
    /// through connections to `allow_hosts` only.
    #[serde(default)]
    pub egress: Egress,
    /// For `egress: allowlist`: `pypi.org`, `*.pythonhosted.org` (subdomains),
    /// `host:port` (without a port: 80 and 443), or an IP literal. A host
    /// that resolves to a private or loopback address is refused unless that
    /// address is listed itself.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_hosts: Vec<String>,
    /// Disk the instance may use (workspace + OpenCode home), e.g. `5g`. Soft
    /// limit: at 90% the agent is told, at 100% it is interrupted.
    #[serde(default)]
    pub disk: Option<String>,
    /// Size of the in-memory `/tmp` (tmpfs).
    #[serde(default = "default_tmp")]
    pub tmp: String,
}

fn default_tmp() -> String {
    "512m".into()
}

impl Default for ContainerSpec {
    fn default() -> Self {
        ContainerSpec { image: None, memory: default_memory(), cpus: default_cpus(), pids: default_pids(), egress: Egress::None, allow_hosts: vec![], disk: None, tmp: default_tmp() }
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
pub enum DiskEnforce {
    #[default]
    Soft,
    Hard,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Egress {
    #[default]
    None,
    /// HTTP(S) through the node's proxy, to `allow_hosts` only.
    Allowlist,
    /// Container gets normal networking (package installs, webfetch). Use sparingly.
    Full,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSpec {
    #[serde(default)]
    pub mode: WorkspaceMode,
    /// `shared` mode: sessions with the same name share one workspace. Across
    /// machines it is synced through the artifact store.
    #[serde(default)]
    pub name: Option<String>,
    /// Start a new workspace from an artifact (`art://b3/…`, a file or tree).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<String>,
}

impl Default for WorkspaceSpec {
    fn default() -> Self {
        WorkspaceSpec { mode: WorkspaceMode::Isolated, name: None, seed: None }
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

/// `~/.moocher`, or `~/.opencode-cluster` when only that one exists (installs from
/// before the rename keep working in place). Without `$HOME`: `/etc/moocher`.
pub fn default_config_dir() -> PathBuf {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return PathBuf::from("/etc/moocher");
    };
    let (new, old) = (home.join(".moocher"), home.join(".opencode-cluster"));
    if !new.exists() && old.exists() {
        old
    } else {
        new
    }
}

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
        if let Some(k) = &self.provider.key {
            if k.reference.is_empty() == k.secret.is_none() {
                errs.push("provider.key needs exactly one of `ref` or `secret`".into());
            }
        }
        match providers.get(&self.provider.id) {
            None => errs.push(format!("unknown provider {:?}", self.provider.id)),
            Some(p) => {
                if p.auth != AuthStyle::None && self.provider.key.is_none() && self.provider.id != "pollinations" {
                    errs.push(format!("provider {:?} needs provider.key (a secret ref)", self.provider.id));
                }
                if p.api == ApiKind::Opencode {
                    if self.provider.key.is_some() {
                        errs.push(format!("provider {:?} takes no provider.key: OpenCode uses its free tier", self.provider.id));
                    }
                    let reach = self.container.allow_hosts.iter().filter_map(|h| HostPattern::parse(h).ok()).any(|h| h.matches("opencode.ai", 443));
                    if self.container.egress == Egress::None || (self.container.egress == Egress::Allowlist && !reach) {
                        errs.push(format!("provider {:?} is called from inside the agent: set container: {{ egress: allowlist, allow_hosts: [opencode.ai] }}", self.provider.id));
                    }
                }
            }
        }
        match (self.container.egress, self.container.allow_hosts.is_empty()) {
            (Egress::Allowlist, true) => errs.push("container.egress=allowlist needs container.allow_hosts".into()),
            (Egress::None | Egress::Full, false) => errs.push("container.allow_hosts is only used with container.egress=allowlist".into()),
            _ => {}
        }
        for h in &self.container.allow_hosts {
            if let Err(e) = HostPattern::parse(h) {
                errs.push(format!("container.allow_hosts: {e}"));
            }
        }
        if self.workspace.mode == WorkspaceMode::Shared && self.workspace.name.is_none() {
            errs.push("workspace.mode=shared requires workspace.name".into());
        }
        if Retain::parse(&self.retain).is_none() {
            errs.push(format!("retain {:?} must be a duration (24h), on_failure, always or never", self.retain));
        }
        for o in &self.outputs {
            let ok = matches!(o.to.as_str(), "artifact" | "parent_workspace" | "task_workspace") || o.to.strip_prefix("workspace:").is_some_and(valid_name);
            if !ok {
                errs.push(format!("outputs: to {:?} must be artifact, parent_workspace, task_workspace or workspace:<name>", o.to));
            }
            if o.path.starts_with('/') || o.path.split('/').any(|c| c == "..") {
                errs.push(format!("outputs: path {:?} must be relative to the workspace", o.path));
            }
        }
        errs
    }
}

/// One `allow_hosts` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPattern {
    /// Lower-case host; `suffix` = `*.` form (subdomains of `host`).
    pub host: String,
    pub suffix: bool,
    /// None = 80 and 443.
    pub port: Option<u16>,
}

impl HostPattern {
    pub fn parse(s: &str) -> Result<HostPattern, String> {
        let s = s.trim().to_ascii_lowercase();
        // `[v6]:port`, `v6`, `host:port`, `host`.
        let (host, port) = if let Some(rest) = s.strip_prefix('[') {
            let (h, after) = rest.split_once(']').ok_or_else(|| format!("{s:?}: unclosed ["))?;
            (h.to_string(), after.strip_prefix(':'))
        } else if s.matches(':').count() == 1 {
            let (h, p) = s.split_once(':').unwrap();
            (h.to_string(), Some(p))
        } else {
            (s.clone(), None)
        };
        let port = match port {
            Some(p) => Some(p.parse::<u16>().ok().filter(|p| *p > 0).ok_or_else(|| format!("{s:?}: bad port"))?),
            None => None,
        };
        let (host, suffix) = match host.strip_prefix("*.") {
            Some(h) => (h.to_string(), true),
            None => (host, false),
        };
        let ok = host.parse::<std::net::IpAddr>().is_ok() || (!host.is_empty() && host.split('.').all(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')));
        if !ok || (suffix && host.parse::<std::net::IpAddr>().is_ok()) {
            return Err(format!("{s:?} is not a host name, *.domain or IP address"));
        }
        Ok(HostPattern { host, suffix, port })
    }

    pub fn matches(&self, host: &str, port: u16) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let port_ok = match self.port {
            Some(p) => p == port,
            None => port == 80 || port == 443,
        };
        let host_ok = if self.suffix { host.len() > self.host.len() && host.ends_with(&format!(".{}", self.host)) } else { host == self.host };
        port_ok && host_ok
    }

    /// The entry names an address itself (so private addresses are fine).
    pub fn is_ip(&self) -> bool {
        self.host.parse::<std::net::IpAddr>().is_ok()
    }
}

/// JSON Schemas for editor validation (`moocher-node schema`).
pub fn schemas() -> BTreeMap<&'static str, serde_json::Value> {
    let mut m = BTreeMap::new();
    m.insert("agent", serde_json::to_value(schemars::schema_for!(AgentConfig)).unwrap());
    m.insert("node", serde_json::to_value(schemars::schema_for!(NodeConfig)).unwrap());
    m.insert("supervisor", serde_json::to_value(schemars::schema_for!(SupervisorConfig)).unwrap());
    m.insert("providers", serde_json::to_value(schemars::schema_for!(ProvidersFile)).unwrap());
    m.insert("routes", serde_json::to_value(schemars::schema_for!(RoutesFile)).unwrap());
    m.insert("overrides", serde_json::to_value(schemars::schema_for!(OverridesFile)).unwrap());
    m.insert("workflow", serde_json::to_value(schemars::schema_for!(WorkflowConfig)).unwrap());
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_patterns() {
        let p = |s: &str| HostPattern::parse(s).unwrap();
        assert!(p("PyPI.org").matches("pypi.org", 443) && p("pypi.org").matches("pypi.org.", 80));
        assert!(!p("pypi.org").matches("pypi.org", 8080) && !p("pypi.org").matches("evil-pypi.org", 443));
        assert!(p("*.pythonhosted.org").matches("files.pythonhosted.org", 443));
        assert!(!p("*.pythonhosted.org").matches("pythonhosted.org", 443) && !p("*.pythonhosted.org").matches("xpythonhosted.org", 443));
        assert!(p("127.0.0.1:8099").matches("127.0.0.1", 8099) && !p("127.0.0.1:8099").matches("127.0.0.1", 443));
        assert!(p("[::1]:80").matches("[::1]", 80) && p("::1").matches("::1", 443) && p("::1").is_ip());
        for bad in ["", "*.1.2.3.4", "a b", "host:0", "host:x", "a..b"] {
            assert!(HostPattern::parse(bad).is_err(), "{bad:?}");
        }
    }

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
    fn opencode_free_needs_egress_and_no_key() {
        let p = builtin_providers();
        let ok: AgentConfig = serde_yaml::from_str(
            "name: zen\nprovider: { id: opencode-free, model: big-pickle }\ncontainer: { egress: allowlist, allow_hosts: [opencode.ai] }",
        )
        .unwrap();
        assert!(ok.validate(&p).is_empty(), "{:?}", ok.validate(&p));
        let offline: AgentConfig = serde_yaml::from_str("name: zen\nprovider: { id: opencode-free, model: big-pickle }").unwrap();
        assert!(offline.validate(&p)[0].contains("egress"));
        let keyed: AgentConfig = serde_yaml::from_str(
            "name: zen\nprovider: { id: opencode-free, model: big-pickle, key: { secret: zen } }\ncontainer: { egress: full }",
        )
        .unwrap();
        assert_eq!(keyed.validate(&p).len(), 1);
    }

    #[test]
    fn routes_parse() {
        let r: RoutesFile = serde_yaml::from_str(
            "routes:\n  a: { type: direct }\n  b: { type: via_node, node: vps-de }\n  c: { type: proxy, url: 'socks5h://x:1' }",
        )
        .unwrap();
        assert_eq!(r.routes["b"], Route::ViaNode { node: "vps-de".into() });
    }

    /// The documented examples must stay valid.
    #[test]
    fn examples_parse() {
        let ex = Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples");
        for d in ["remote/hub", "quickstart/hub"] {
            let s: SupervisorConfig = load_yaml(&ex.join(d).join("supervisor.yaml")).unwrap();
            assert!(s.tasks.enabled);
        }
        for d in ["remote/worker", "remote/relay", "quickstart/hub"] {
            let n: NodeConfig = load_yaml(&ex.join(d).join("node.yaml")).unwrap();
            assert!(valid_name(&n.name));
        }
        let r: RoutesFile = load_yaml(&ex.join("remote/worker/routes.yaml")).unwrap();
        assert_eq!(r.routes.len(), 2);
        let p: ProvidersFile = load_yaml(&ex.join("remote/worker/providers.yaml")).unwrap();
        let mut all = builtin_providers();
        all.extend(p.providers);
        for (path, a) in load_dir::<AgentConfig>(&ex.join("remote/hub/templates")).into_iter().chain(load_dir(&ex.join("quickstart/hub/agents"))) {
            let a = a.unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            if path.to_string_lossy().contains("quickstart") {
                continue; // uses the quickstart's own `mock` provider
            }
            assert!(a.validate(&all).is_empty(), "{}: {:?}", path.display(), a.validate(&all));
        }
        for (path, w) in load_dir::<WorkflowConfig>(&ex.join("quickstart/hub/workflows")) {
            w.unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        }
    }

    #[test]
    fn overrides_follow_the_template() {
        let a: AgentConfig = serde_yaml::from_str(
            "name: t\nprovider: { id: groq, model: m, key: { secret: groq } }\noverridable: { role_append: true, budget_usd: 1.0, routes: [direct] }",
        )
        .unwrap();
        assert!(a.validate(&builtin_providers()).is_empty(), "a secret alias counts as a key");
        let ok = SpawnOverrides { role_append: Some("x".into()), budget_usd: Some(0.5), route: Some("direct".into()) };
        assert!(ok.check(&a).is_ok());
        assert!(SpawnOverrides { budget_usd: Some(2.0), ..Default::default() }.check(&a).is_err());
        assert!(SpawnOverrides { route: Some("via-x".into()), ..Default::default() }.check(&a).is_err());
        let strict: AgentConfig = serde_yaml::from_str("name: s\nprovider: { id: kilo-free, model: auto }").unwrap();
        assert!(SpawnOverrides { role_append: Some("x".into()), ..Default::default() }.check(&strict).is_err());
        let both: AgentConfig = serde_yaml::from_str("name: b\nprovider: { id: groq, model: m, key: { ref: 'env:X', secret: y } }").unwrap();
        assert_eq!(both.validate(&builtin_providers()).len(), 1);
    }

    #[test]
    fn retention_and_outputs() {
        assert_eq!(Retain::parse("24h"), Some(Retain::For(86_400_000)));
        assert_eq!(Retain::parse("never").unwrap().keep_ms(false), Some(0));
        assert_eq!(Retain::parse("always").unwrap().keep_ms(true), None);
        let f = Retain::parse("on_failure").unwrap();
        assert_eq!((f.keep_ms(false), f.keep_ms(true)), (Some(0), Some(86_400_000)));
        assert_eq!(Retain::parse("soon"), None);
        let y = "name: a\nprovider: { id: groq, model: m, key: { ref: \"env:K\" } }\noutputs:\n  - { path: report.md, to: parent_workspace }\n  - { path: ../etc/passwd }\n  - { path: x, to: somewhere }\nretain: soon\n";
        let a: AgentConfig = serde_yaml::from_str(y).unwrap();
        let errs = a.validate(&builtin_providers());
        assert!(errs.iter().any(|e| e.contains("relative to the workspace")), "{errs:?}");
        assert!(errs.iter().any(|e| e.contains("to \"somewhere\"")), "{errs:?}");
        assert!(errs.iter().any(|e| e.contains("retain")), "{errs:?}");
    }

    #[test]
    fn route_specs_and_durations() {
        let a: AgentConfig = serde_yaml::from_str("name: a\nprovider: { id: kilo-free, model: auto }\nroute: via-vps").unwrap();
        assert_eq!(a.route.unwrap().names(), vec!["via-vps"]);
        let b: AgentConfig =
            serde_yaml::from_str("name: b\nprovider: { id: kilo-free, model: auto }\nroute: { failover: [via-vps, home], on_exhausted: fail }").unwrap();
        let p = b.route.unwrap().plan();
        assert_eq!((p.prefer.as_str(), p.failover.len(), p.on_exhausted), ("direct", 2, OnExhausted::Fail));
        assert_eq!(parse_duration_ms("1h30m"), Some(5_400_000));
        assert_eq!(parse_duration_ms("6m0s"), Some(360_000));
        assert_eq!(parse_duration_ms("250ms"), Some(250));
        assert_eq!(parse_duration_ms("1.5s"), Some(1500));
        assert_eq!(parse_duration_ms("17"), Some(17_000));
        assert_eq!(parse_duration_ms("1d"), Some(86_400_000));
        assert_eq!(parse_duration_ms("x"), None);
    }
}
