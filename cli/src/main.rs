//! moo — scriptable client for the supervisor API (mirrors the web UI).
//!
//! Auth: `MOO_TOKEN`, else `~/.moocher/cli-token`. Server: `MOO_URL`
//! (default http://127.0.0.1:7700). Add `--json` to any command for raw output.

mod attach;
mod tui;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use futures::StreamExt;
use moocher_protocol::api::*;
use moocher_protocol::model::*;
use serde_json::{json, Value};

#[derive(Parser)]
#[command(name = "moo", version, about = "moocher CLI")]
struct Cli {
    #[arg(long, env = "MOO_URL", default_value = "http://127.0.0.1:7700", global = true)]
    url: String,
    #[arg(long, env = "MOO_TOKEN", hide_env_values = true, global = true)]
    token: Option<String>,
    /// Extra CA certificate (PEM) to trust for the hub's HTTPS (self-signed or private CA).
    #[arg(long, env = "MOO_CA_CERT", global = true)]
    ca_cert: Option<std::path::PathBuf>,
    /// Print raw JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Ask the manager about the cluster (needs `manager.agent` in supervisor.yaml).
    Ask { question: Vec<String> },
    /// Nodes, live sessions, tasks and token totals.
    Status,
    /// List nodes (same as `moo node ls`).
    Nodes,
    /// Nodes: list, invite (join token), approve, revoke, forget.
    Node {
        #[command(subcommand)]
        cmd: NodeCmd,
    },
    /// Source addresses allowed to enroll/connect (relay nodes, or all nodes).
    Allow {
        #[command(subcommand)]
        cmd: AllowCmd,
    },
    /// What the cluster has seen of each provider model: speed, errors,
    /// learned context windows, when credits ran out.
    Providers {
        /// Only this provider.
        provider: Option<String>,
        /// Look back this many hours.
        #[arg(long, default_value_t = 24)]
        hours: i64,
        /// Recent events (context overflows, credits, auth) instead.
        #[arg(long)]
        events: bool,
    },
    /// Provider allowances per IP / key / account, with reset countdowns.
    Quotas {
        /// Only this provider.
        provider: Option<String>,
    },
    /// Key pools: each key's state, who uses what, and what is kept for urgent work.
    Pool,
    /// Egress IP of every route, and which nodes/sessions share an IP.
    Ips,
    /// Host and per-instance CPU, memory, processes and disk.
    Stats,
    /// Tell the cluster when a provider allowance resets / how much is left.
    Quota {
        #[command(subcommand)]
        cmd: QuotaCmd,
    },
    /// Live route changes for an agent on a node (kept in that node's overrides.yaml).
    Route {
        #[command(subcommand)]
        cmd: RouteCmd,
    },
    /// Agent profiles available per node.
    Agents,
    Sessions {
        #[arg(long)]
        all: bool,
    },
    /// Details of one session (todos, output, tokens).
    Show {
        session: String,
        /// Print the rendered instructions it was started with.
        #[arg(long)]
        preamble: bool,
    },
    /// The hub's template library and where each template can run.
    Templates,
    /// Start a session from an agent profile.
    Spawn {
        agent: String,
        #[arg(short, long)]
        prompt: Option<String>,
        #[arg(long)]
        node: Option<String>,
        #[arg(long)]
        title: Option<String>,
        /// Use (or create) a shared workspace with this name.
        #[arg(long)]
        shared: Option<String>,
        /// The specialization of a generic agent (`kind: generic`).
        #[arg(long)]
        system_prompt: Option<String>,
        /// `right_now` may pause less urgent work to make room.
        #[arg(long)]
        urgency: Option<String>,
        /// Extra instructions for this instance (template must allow role_append).
        #[arg(long)]
        append: Option<String>,
        /// Budget for this instance in USD (up to the template's overridable.budget_usd).
        #[arg(long)]
        budget: Option<f64>,
        /// Preferred egress route (one of the template's overridable.routes).
        #[arg(long)]
        route: Option<String>,
        /// Give it the coordinator tools (it may start and stop other agents).
        #[arg(long)]
        coordinator: bool,
    },
    /// Send a message to a session (default: picked up at its next step).
    Send {
        session: String,
        text: String,
        /// Abort the current step and deliver now.
        #[arg(long, conflicts_with = "inbox")]
        steer: bool,
        /// Leave in the session's mailbox.
        #[arg(long)]
        inbox: bool,
    },
    Interrupt { session: String },
    Stop {
        session: String,
        #[arg(long)]
        rm_workspace: bool,
    },
    /// Answer a blocked session's permission request: once | always | reject.
    Permit { session: String, request: String, reply: String },
    /// The task bucket.
    Task {
        #[command(subcommand)]
        cmd: TaskCmd,
    },
    Workflow {
        #[command(subcommand)]
        cmd: WorkflowCmd,
    },
    /// Event log (`-f` to follow live).
    Events {
        #[arg(short, long)]
        follow: bool,
        #[arg(long)]
        session: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
    Audit {
        #[arg(long)]
        verify: bool,
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
    Token {
        #[command(subcommand)]
        cmd: TokenCmd,
    },
    /// Questions from agents (open ones: importance, then how many wait, then age).
    Questions {
        /// Include answered/decided ones.
        #[arg(long)]
        all: bool,
        #[command(subcommand)]
        cmd: Option<QuestionsCmd>,
    },
    /// Answer a question: free text, `--choice N`, `--decide` (let the agent decide) or `--reject`.
    Answer {
        id: String,
        text: Option<String>,
        #[arg(long)]
        choice: Option<usize>,
        #[arg(long)]
        decide: bool,
        #[arg(long)]
        reject: bool,
    },
    /// Decisions agents made without you (for review).
    Assumptions {
        #[arg(long)]
        task: Option<i64>,
    },
    /// The artifact store (content-addressed files and trees, with lineage).
    Artifact {
        #[command(subcommand)]
        cmd: ArtifactCmd,
    },
    /// Halted tasks/runs waiting for a recovery decision.
    Halts {
        #[arg(long)]
        all: bool,
    },
    /// Resume after a death: restart | resume_elsewhere | handoff | abandon.
    Recover {
        /// A halt id (h-…) or a dead session id.
        target: String,
        option: String,
        /// resume_elsewhere: preferred node.
        #[arg(long)]
        node: Option<String>,
    },
    /// What was collected from instances when they ended.
    Harvests {
        #[arg(long)]
        session: Option<String>,
    },
    /// OpenCode's terminal UI for an instance, through the hub (needs attach_listen).
    Attach {
        session: String,
        /// Local port (default: any free one).
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// Only print the local address; do not start `opencode attach`.
        #[arg(long)]
        no_launch: bool,
    },
    /// Dashboard in the terminal.
    Tui,
    /// The notification centre (newest first).
    Notifications {
        #[arg(long)]
        unread: bool,
        /// Mark everything shown as read.
        #[arg(long)]
        read: bool,
        #[arg(long, default_value_t = 30)]
        limit: i64,
    },
    /// Run housekeeping now: export events, prune, expire artifacts (admin).
    Housekeeping {
        /// Only report what would happen.
        #[arg(long)]
        dry_run: bool,
        /// Act as if this much later (e.g. `1d`: what tomorrow's run would do).
        #[arg(long)]
        ahead: Option<String>,
    },
}

#[derive(Subcommand)]
enum QuestionsCmd {
    /// Show or set the question mode: ask_me_everything | important_and_vital | vital_only | autonomous.
    Mode {
        mode: Option<String>,
        /// Only for this task.
        #[arg(long)]
        task: Option<i64>,
    },
}

#[derive(Subcommand)]
enum ArtifactCmd {
    /// Upload a file, or a directory (packed as a tree).
    Put {
        path: std::path::PathBuf,
        #[arg(long)]
        name: Option<String>,
        /// Repeatable.
        #[arg(long)]
        tag: Vec<String>,
        /// Lineage: artifacts this one was made from (repeatable).
        #[arg(long)]
        parent: Vec<String>,
    },
    /// Download: a file to `-o` (default: its name), a tree unpacked into the directory `-o`.
    Get {
        reference: String,
        #[arg(short, long)]
        out: Option<std::path::PathBuf>,
    },
    Ls {
        query: Option<String>,
        #[arg(long)]
        task: Option<i64>,
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
    /// Metadata and lineage (parents and children).
    Show { reference: String },
}

#[derive(Subcommand)]
enum TaskCmd {
    /// Add a task: labels right_now | urgent | normal | when_free | on_demand.
    Add {
        title: String,
        #[arg(short, long, default_value = "")]
        body: String,
        #[arg(short, long, default_value = "when_free")]
        label: String,
        /// Repeatable. `workflow:<name>` runs that workflow instead of a coordinator.
        #[arg(short, long)]
        tag: Vec<String>,
        /// Question mode for this task's agents (default: the cluster's).
        #[arg(long)]
        question_mode: Option<String>,
    },
    Ls,
    Show { id: i64 },
    Edit {
        id: i64,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        question_mode: Option<String>,
    },
    /// Put these task ids first, in this order.
    Order { ids: Vec<i64> },
    /// Start now regardless of label.
    Run { id: i64 },
    Cancel { id: i64 },
    Rm { id: i64 },
}

#[derive(Subcommand)]
enum WorkflowCmd {
    Ls,
    Run { name: String, input: String },
    Runs,
    Show { id: String },
    Cancel { id: String },
}

#[derive(Subcommand)]
enum NodeCmd {
    Ls,
    /// Create a one-time join token and print the command to run on the new machine.
    Invite {
        name: String,
        /// Proxy-only machine: runs no agents, relays other nodes' LLM traffic.
        #[arg(long)]
        relay: bool,
        /// Repeatable `key=value`.
        #[arg(long)]
        label: Vec<String>,
    },
    /// Let a pending node take part in the cluster.
    Approve { name: String },
    /// Revoke the node's certificates and disconnect it.
    Revoke { name: String },
    /// Remove a disconnected node from the list (history is kept).
    Forget { name: String },
    /// Re-learn the node's egress IPs now.
    Probe { name: String },
}

#[derive(Subcommand)]
enum QuotaCmd {
    /// e.g. `moo quota set kilo-free --scope-id 203.0.113.7 --resets-at 03:15 --remaining 0`
    Set {
        provider: String,
        /// An IP, `key:…` or `account:…` (default: every bucket of the provider).
        #[arg(long)]
        scope_id: Option<String>,
        /// Default: every node reporting this provider.
        #[arg(long)]
        node: Option<String>,
        /// `03:15` (next occurrence, local time), `03:15Z` (UTC), `+2h30m`, or RFC 3339.
        #[arg(long)]
        resets_at: Option<String>,
        #[arg(long)]
        remaining: Option<u64>,
        /// Calibrate a provider unit (with `--used`): `--unit neurons --used 4210`
        /// is what the provider's dashboard shows for the current window.
        #[arg(long, requires = "used")]
        unit: Option<String>,
        #[arg(long, requires = "unit")]
        used: Option<f64>,
        /// With `--unit`: which model's pool (`per_model` units).
        #[arg(long, requires = "unit")]
        model: Option<String>,
        #[arg(long)]
        note: Option<String>,
    },
    Clear {
        provider: String,
        #[arg(long)]
        scope_id: Option<String>,
        #[arg(long)]
        node: Option<String>,
    },
}

#[derive(Subcommand)]
enum RouteCmd {
    /// e.g. `moo route set worker-a builder --prefer direct --failover via-vps,home-proxy`
    Set {
        node: String,
        agent: String,
        #[arg(long, default_value = "direct")]
        prefer: String,
        /// Comma-separated route names, tried in order when the allowance runs out.
        #[arg(long, value_delimiter = ',')]
        failover: Vec<String>,
        /// wait (park until reset) or fail (end the turn with an error).
        #[arg(long, default_value = "wait")]
        on_exhausted: String,
    },
    /// Back to the agent's own YAML.
    Clear { node: String, agent: String },
}

#[derive(Subcommand)]
enum AllowCmd {
    Ls,
    /// Allow an IP or CIDR, e.g. `203.0.113.7` or `10.0.0.0/8`.
    Add {
        source: String,
        #[arg(long, default_value = "")]
        note: String,
    },
    /// Remove an entry; connected nodes it no longer covers are disconnected.
    Rm { source: String },
}

#[derive(Subcommand)]
enum TokenCmd {
    Create {
        name: String,
        #[arg(long, default_value = "operate")]
        scope: String,
    },
    Ls,
    Revoke { name: String },
}

pub struct Api {
    pub base: String,
    pub token: String,
    pub http: reqwest::Client,
}

impl Api {
    pub async fn req(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value> {
        let mut r = self.http.request(method, format!("{}/api/v1{path}", self.base)).bearer_auth(&self.token);
        if let Some(b) = body {
            r = r.json(&b);
        }
        let resp = r.send().await.with_context(|| format!("contacting supervisor at {}", self.base))?;
        let status = resp.status();
        let v: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            bail!("{}: {}", status, v["error"].as_str().unwrap_or("request failed"));
        }
        Ok(v)
    }
    pub async fn get(&self, p: &str) -> Result<Value> {
        self.req(reqwest::Method::GET, p, None).await
    }
    pub async fn post(&self, p: &str, b: Value) -> Result<Value> {
        self.req(reqwest::Method::POST, p, Some(b)).await
    }
}

fn token(cli: &Cli) -> Result<String> {
    if let Some(t) = &cli.token {
        return Ok(t.clone());
    }
    let p = moocher_protocol::config::default_config_dir().join("cli-token");
    Ok(std::fs::read_to_string(&p).with_context(|| format!("no MOO_TOKEN and cannot read {}", p.display()))?.trim().to_string())
}

fn ago(ts: i64) -> String {
    let s = (now_ms() - ts) / 1000;
    match s {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

fn todos(t: &[Todo]) -> String {
    if t.is_empty() {
        "-".into()
    } else {
        format!("{}/{}", t.iter().filter(|x| x.status == "completed").count(), t.len())
    }
}

fn print_sessions(ss: &[SessionInfo]) {
    println!("{:<14} {:<12} {:<10} {:<11} {:>6} {:>9} {:>8} {:>5}  {}", "SESSION", "AGENT", "NODE", "STATE", "TODOS", "TOKENS", "COST", "AGE", "CURRENT / TITLE");
    for s in ss {
        let r = &s.report;
        let cur = r.error.as_deref().map(|e| format!("ERROR {e}")).or(r.current_task.clone()).or(s.title.clone()).unwrap_or_default();
        println!(
            "{:<14} {:<12} {:<10} {:<11} {:>6} {:>9} {:>8} {:>5}  {}",
            r.id,
            r.agent,
            r.node,
            r.state.as_str(),
            todos(&r.todos),
            r.billed().total(),
            format!("${:.4}", r.billed().cost_usd),
            ago(r.started_at),
            cur.chars().take(60).collect::<String>()
        );
    }
}

fn print_nodes(nodes: &[NodeInfo]) {
    println!("{:<12} {:<8} {:<9} {:<13} {:>6} {:<16} {:>8}  {}", "NODE", "ROLE", "ENROLL", "LINK", "RTT", "IP", "SESSIONS", "AGENTS");
    for n in nodes {
        println!(
            "{:<12} {:<8} {:<9} {:<13} {:>6} {:<16} {:>8}  {}",
            n.name,
            n.role,
            n.enrollment,
            if n.connected { "connected".to_string() } else { format!("down {}", ago(n.last_seen)) },
            n.link_rtt_ms.map(|r| format!("{r:.0}ms")).unwrap_or("-".into()),
            n.observed_ip.as_deref().unwrap_or("-"),
            format!("{}/{}", n.running, n.max_sessions),
            n.agents.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ")
        );
    }
}

/// `3h 24m` from now (or `now` / `-` when unknown).
fn until(ts: Option<i64>) -> String {
    match ts {
        None => "-".into(),
        Some(t) if t <= now_ms() => "now".into(),
        Some(t) => {
            let s = (t - now_ms() + 999) / 1000;
            match s {
                s if s >= 86_400 => format!("{}d {}h", s / 86_400, s % 86_400 / 3600),
                s if s >= 3600 => format!("{}h {}m", s / 3600, s % 3600 / 60),
                s if s >= 60 => format!("{}m {}s", s / 60, s % 60),
                s => format!("{s}s"),
            }
        }
    }
}

/// `03:15` (next occurrence, local), `03:15Z` (UTC), `+2h30m`, RFC 3339 -> unix ms.
fn parse_when(s: &str) -> Result<i64> {
    use chrono::{Local, NaiveTime, TimeZone, Utc};
    let s = s.trim();
    if let Some(d) = s.strip_prefix('+').or_else(|| s.strip_prefix("in ")) {
        let ms = moocher_protocol::config::parse_duration_ms(d).context("bad duration (e.g. +2h30m)")?;
        return Ok(now_ms() + ms);
    }
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(t.timestamp_millis());
    }
    let (clock, utc) = match s.strip_suffix('Z').or_else(|| s.strip_suffix(" UTC")) {
        Some(c) => (c, true),
        None => (s, false),
    };
    let t = NaiveTime::parse_from_str(clock, "%H:%M").context("expected HH:MM, +duration or RFC 3339")?;
    let next = |today: chrono::NaiveDate| today.and_time(t);
    let ms = if utc {
        let now = Utc::now();
        let mut at = Utc.from_utc_datetime(&next(now.date_naive()));
        if at <= now {
            at += chrono::Duration::days(1);
        }
        at.timestamp_millis()
    } else {
        let now = Local::now();
        let mut at = Local.from_local_datetime(&next(now.date_naive())).single().context("ambiguous local time")?;
        if at <= now {
            at += chrono::Duration::days(1);
        }
        at.timestamp_millis()
    };
    Ok(ms)
}

fn print_tasks(ts: &[Task]) {
    println!("{:>4} {:<10} {:<10} {:>5}  {}", "ID", "STATUS", "LABEL", "RANK", "TITLE");
    for t in ts {
        println!("{:>4} {:<10} {:<10} {:>5}  {}", t.id, t.status.as_str(), t.label.as_str(), t.rank, t.title);
    }
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    // `moo events | head` should just stop, like other CLI tools, instead of
    // panicking on a closed stdout.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    match run(Cli::parse()).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    // No redirects: `moo attach` needs the cookie from the ticket's 302 itself.
    let mut hb = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none());
    if let Some(ca) = &cli.ca_cert {
        let pem = std::fs::read(ca).with_context(|| format!("reading {}", ca.display()))?;
        hb = hb.add_root_certificate(reqwest::Certificate::from_pem(&pem)?);
    }
    let api = Api { base: cli.url.trim_end_matches('/').to_string(), token: token(&cli)?, http: hb.build()? };
    let raw = cli.json;
    let out = |v: &Value| println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
    match cli.cmd {
        Cmd::Ask { question } => {
            let v = api.post("/manager/ask", json!({"question": question.join(" ")})).await?;
            println!("{}", v["answer"].as_str().unwrap_or(""));
            for l in v["links"].as_array().into_iter().flatten() {
                println!("  -> {} ({})", l["path"].as_str().unwrap_or(""), l["label"].as_str().unwrap_or(""));
            }
        }
        Cmd::Status => {
            let v = api.get("/overview").await?;
            if raw {
                return Ok(out(&v));
            }
            let o: Overview = serde_json::from_value(v)?;
            println!("NODES");
            print_nodes(&o.nodes);
            println!("\nSESSIONS");
            print_sessions(&o.sessions);
            println!("\nTASKS");
            print_tasks(&o.tasks);
            println!("\nTOTAL  tokens {}  (in {} / out {})  cost ${:.4}", o.totals.total(), o.totals.input, o.totals.output, o.totals.cost_usd);
        }
        Cmd::Nodes | Cmd::Node { cmd: NodeCmd::Ls } => {
            let v = api.get("/nodes").await?;
            if raw {
                return Ok(out(&v));
            }
            print_nodes(&serde_json::from_value::<Vec<NodeInfo>>(v)?);
        }
        Cmd::Node { cmd } => match cmd {
            NodeCmd::Ls => unreachable!(),
            NodeCmd::Invite { name, relay, label } => {
                let mut labels = serde_json::Map::new();
                for l in label {
                    let (k, v) = l.split_once('=').context("labels are key=value")?;
                    labels.insert(k.into(), json!(v));
                }
                let v = api.post("/nodes/invite", json!({"name": name, "role": if relay { "relay" } else { "worker" }, "labels": labels})).await?;
                if raw {
                    return Ok(out(&v));
                }
                println!("Join token for {} ({}), single use, valid until {}:", name, v["role"].as_str().unwrap_or(""), fmt_ts(v["expires_at"].as_i64().unwrap_or(0)));
                println!("\n  {}\n", v["command"].as_str().unwrap_or(""));
                if v["approval_required"].as_bool() == Some(true) {
                    println!("After it enrolls, approve it with: moo node approve {name}");
                }
            }
            NodeCmd::Approve { name } => {
                api.post(&format!("/nodes/{name}/approve"), json!({})).await?;
                println!("{name} approved");
            }
            NodeCmd::Revoke { name } => {
                let v = api.post(&format!("/nodes/{name}/revoke"), json!({})).await?;
                println!("{name} revoked ({} certificate(s)); it has been disconnected", v["certificates_revoked"]);
            }
            NodeCmd::Forget { name } => {
                api.req(reqwest::Method::DELETE, &format!("/nodes/{name}"), None).await?;
                println!("{name} removed");
            }
            NodeCmd::Probe { name } => out(&api.post(&format!("/nodes/{name}/probe"), json!({})).await?),
        },
        Cmd::Quotas { provider } => {
            let v = api.get("/quotas").await?;
            if raw {
                return Ok(out(&v));
            }
            let qs: Vec<QuotaView> = serde_json::from_value(v)?;
            println!("{:<12} {:<6} {:<22} {:<8} {:>11} {:>9} {:<10} {:<8} {:<20} {}", "PROVIDER", "SCOPE", "ID", "KIND", "USED/LIMIT", "LEFT", "RESETS IN", "SOURCE", "NODES", "SESSIONS");
            for q in qs.iter().filter(|q| provider.as_deref().is_none_or(|p| p == q.provider)) {
                println!(
                    "{:<12} {:<6} {:<22} {:<8} {:>11} {:>9} {:<10} {:<8} {:<20} {}",
                    q.provider,
                    q.scope,
                    q.scope_id,
                    q.kind,
                    format!("{}/{}", q.used, q.limit.map(|l| l.to_string()).unwrap_or("?".into())),
                    match (q.exhausted, q.remaining) {
                        (true, _) => "EXHAUSTED".to_string(),
                        (false, Some(r)) => r.to_string(),
                        (false, None) => "-".into(),
                    },
                    until(q.resets_at),
                    q.source,
                    q.nodes.join(","),
                    q.sessions.join(",")
                );
                for r in q.unit.iter().flat_map(|u| &u.rates) {
                    let name = q.unit.as_ref().map_or("unit", |u| u.name.as_str());
                    println!("    {}: {:.1} {name}/Mtok, 1 {name} buys ~{:.0} tokens ({})", r.model, r.units_per_mtok, 1e6 / r.units_per_mtok, r.source);
                }
            }
        }
        Cmd::Providers { provider, hours, events } => {
            let q = provider.as_deref().map(|p| format!("&provider={p}")).unwrap_or_default();
            if events {
                let v = api.get(&format!("/providers/events?limit=50{q}")).await?;
                if raw {
                    return Ok(out(&v));
                }
                for e in v.as_array().into_iter().flatten() {
                    let d = &e["detail"];
                    let what = match e["kind"].as_str().unwrap_or("") {
                        "context_overflow" => d["limit"].as_u64().map(|l| format!("window {l} tokens")).unwrap_or_else(|| "no limit named".into()),
                        _ => d["message"].as_str().unwrap_or("").chars().take(80).collect(),
                    };
                    println!("{} {:<18} {:<34} {:<10} {}", ago(e["ts"].as_i64().unwrap_or(0)), e["kind"].as_str().unwrap_or(""), format!("{}/{}", e["provider"].as_str().unwrap_or(""), e["model"].as_str().unwrap_or("")), e["node"].as_str().unwrap_or(""), what);
                }
                return Ok(());
            }
            let v = api.get(&format!("/providers/stats?hours={hours}{q}")).await?;
            if raw {
                return Ok(out(&v));
            }
            println!("{:<40} {:>7} {:>6} {:>9} {:>9} {:>8} {:>10} {:<24}", "MODEL", "CALLS", "ERR%", "1ST BYTE", "p95", "TOK/S", "COST", "LEARNED");
            for m in v["models"].as_array().into_iter().flatten() {
                let secs = |k: &str| m[k].as_f64().map(|t| format!("≤{:.1}s", t / 1000.0)).unwrap_or("-".into());
                let l = &m["learned"];
                let mut learned = vec![];
                if let Some(c) = l["context"].as_u64() {
                    learned.push(format!("window {c}"));
                }
                if let Some(ts) = l["last_credits_exhausted"].as_i64() {
                    learned.push(format!("out of credits {}", ago(ts)));
                }
                let errs: Vec<String> = m["errors"].as_object().into_iter().flatten().map(|(k, n)| format!("{k}:{n}")).collect();
                println!(
                    "{:<40} {:>7} {:>5.1}% {:>9} {:>9} {:>8} {:>10} {:<24} {}",
                    format!("{}/{}", m["provider"].as_str().unwrap_or(""), m["model"].as_str().unwrap_or("")),
                    m["requests"],
                    m["error_rate"].as_f64().unwrap_or(0.0) * 100.0,
                    secs("ttft_ms_p50"),
                    secs("ttft_ms_p95"),
                    m["tokens_per_s_mean"].as_f64().map(|t| format!("{t:.0}")).unwrap_or("-".into()),
                    format!("${:.4}", m["cost_usd"].as_f64().unwrap_or(0.0)),
                    learned.join(", "),
                    errs.join(" ")
                );
            }
        }
        Cmd::Stats => {
            let v = api.get("/stats").await?;
            if raw {
                return Ok(out(&v));
            }
            let mib = |b: Option<u64>| b.map(|b| format!("{:.0}M", b as f64 / (1 << 20) as f64)).unwrap_or("-".into());
            println!("{:<12} {:>5} {:>6} {:>17} {:>19}", "HOST", "CPUS", "LOAD", "MEM AVAIL/TOTAL", "DISK FREE/TOTAL");
            for h in v["hosts"].as_array().into_iter().flatten() {
                println!(
                    "{:<12} {:>5} {:>6.2} {:>17} {:>19}",
                    h["node"].as_str().unwrap_or(""),
                    h["cpus"].as_u64().unwrap_or(0),
                    h["load1"].as_f64().unwrap_or(0.0),
                    format!("{}/{}", mib(h["mem_available"].as_u64()), mib(h["mem_total"].as_u64())),
                    format!("{}/{}", mib(h["disk_free"].as_u64()), mib(h["disk_total"].as_u64()))
                );
            }
            println!("\n{:<14} {:<12} {:>6} {:>13} {:>5} {:>13}", "SESSION", "NODE", "CPU%", "MEM/LIMIT", "PIDS", "DISK/LIMIT");
            for s in v["instances"].as_array().into_iter().flatten() {
                println!(
                    "{:<14} {:<12} {:>6} {:>13} {:>5} {:>13}",
                    s["session"].as_str().unwrap_or(""),
                    s["node"].as_str().unwrap_or(""),
                    s["cpu_pct"].as_f64().map(|c| format!("{c:.1}")).unwrap_or("-".into()),
                    format!("{}/{}", mib(s["mem_bytes"].as_u64()), mib(s["mem_limit"].as_u64())),
                    s["pids"].as_u64().map(|p| p.to_string()).unwrap_or("-".into()),
                    format!("{}/{}", mib(s["disk_bytes"].as_u64()), mib(s["disk_limit"].as_u64()))
                );
            }
        }
        Cmd::Pool => {
            let v = api.get("/pool").await?;
            if raw {
                return Ok(out(&v));
            }
            let p: PoolView = serde_json::from_value(v)?;
            println!("{:<14} {:<12} {:<18} {:<10} {:<9} {:<7} {}", "PROVIDER", "NODE", "KEY", "USED", "CALLS", "LEND", "STATE");
            for k in &p.keys {
                let state = match (k.key.bad_until, k.exhausted_until) {
                    (Some(_), _) => "refused by the provider".to_string(),
                    (_, Some(t)) => format!("used up, resets in {}s", (t - now_ms()).max(0) / 1000),
                    _ if k.key.borrowed => "ok (borrowed)".to_string(),
                    _ => "ok".to_string(),
                };
                println!("{:<14} {:<12} {:<18} {:<10} {:<9} {:<7} {}", k.key.provider, k.node, k.key.key_id, k.used, k.key.in_flight, if k.key.lendable { "yes" } else { "no" }, state);
            }
            if !p.in_use.is_empty() {
                println!("\nIN USE");
                for u in &p.in_use {
                    println!("  {:<14} {:<12} {:<14} {:<18} {} {}", u.used.session, u.node, u.used.provider, u.used.key_id, u.used.route, u.used.ip.as_deref().unwrap_or(""));
                }
            }
            if !p.yields.is_empty() {
                println!("\nKEPT FOR URGENT WORK");
                for y in &p.yields {
                    println!("  {:<14} {:<18} for {}", y.provider, y.scope_id, y.by);
                }
            }
        }
        Cmd::Ips => {
            let v = api.get("/egress").await?;
            if raw {
                return Ok(out(&v));
            }
            let e: EgressView = serde_json::from_value(v)?;
            println!("{:<12} {:<14} {:<18} {:<7} {}", "NODE", "ROUTE", "IP", "SOURCE", "NOTE");
            for i in &e.ips {
                println!("{:<12} {:<14} {:<18} {:<7} {}", i.node, i.route, i.ip.as_deref().unwrap_or("?"), i.source, i.error.as_deref().unwrap_or(""));
            }
            println!("\nSHARED IPS");
            for s in &e.shared {
                println!(
                    "  {:<18} nodes {:<20} routes {:<34} providers {:<16} sessions {}",
                    s.ip,
                    s.nodes.join(","),
                    s.routes.join(","),
                    if s.providers.is_empty() { "-".into() } else { s.providers.join(",") },
                    if s.sessions.is_empty() { "-".into() } else { s.sessions.join(",") }
                );
            }
        }
        Cmd::Quota { cmd } => match cmd {
            QuotaCmd::Set { provider, scope_id, node, resets_at, remaining, unit, used, model, note } => {
                let resets_at = resets_at.as_deref().map(parse_when).transpose()?;
                let v = api
                    .post("/quotas/override", json!({"provider": provider, "scope_id": scope_id, "node": node, "resets_at": resets_at, "remaining": remaining, "unit": unit, "used": used, "model": model, "note": note}))
                    .await?;
                if raw {
                    return Ok(out(&v));
                }
                println!("override set on {}{}", v["nodes"].as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default(), resets_at.map(|t| format!("; resets in {}", until(Some(t)))).unwrap_or_default());
            }
            QuotaCmd::Clear { provider, scope_id, node } => {
                let v = api.post("/quotas/override", json!({"provider": provider, "scope_id": scope_id, "node": node, "clear": true})).await?;
                if raw {
                    return Ok(out(&v));
                }
                println!("override cleared");
            }
        },
        Cmd::Route { cmd } => match cmd {
            RouteCmd::Set { node, agent, prefer, failover, on_exhausted } => {
                api.post(&format!("/nodes/{node}/routes"), json!({"agent": agent, "route": {"prefer": prefer, "failover": failover, "on_exhausted": on_exhausted}})).await?;
                println!("{node}/{agent}: prefer {prefer}{}", if failover.is_empty() { String::new() } else { format!(", then {}", failover.join(" → ")) });
            }
            RouteCmd::Clear { node, agent } => {
                api.post(&format!("/nodes/{node}/routes"), json!({"agent": agent, "route": null})).await?;
                println!("{node}/{agent}: route override removed");
            }
        },
        Cmd::Allow { cmd } => match cmd {
            AllowCmd::Ls => {
                let v = api.get("/allowed-sources").await?;
                if raw {
                    return Ok(out(&v));
                }
                println!("enforced for: {}", v["enforce_for"].as_str().unwrap_or("?"));
                for e in v["entries"].as_array().into_iter().flatten() {
                    println!("  {:<20} {:<24} {}", e["cidr"].as_str().unwrap_or(""), e["added_by"].as_str().unwrap_or(""), e["note"].as_str().unwrap_or(""));
                }
            }
            AllowCmd::Add { source, note } => {
                let v = api.post("/allowed-sources", json!({"source": source, "note": note})).await?;
                println!("allowed {}", v["cidr"].as_str().unwrap_or(""));
            }
            AllowCmd::Rm { source } => {
                let v = api.post("/allowed-sources/remove", json!({"source": source})).await?;
                let d: Vec<&str> = v["disconnected"].as_array().into_iter().flatten().filter_map(|x| x.as_str()).collect();
                println!("removed {source}{}", if d.is_empty() { String::new() } else { format!("; disconnected {}", d.join(", ")) });
            }
        },
        Cmd::Agents => {
            let v = api.get("/agents").await?;
            if raw {
                return Ok(out(&v));
            }
            println!("{:<12} {:<16} {:<6} {:<34} {:<6}  {}", "NODE", "AGENT", "SOURCE", "MODEL", "READY", "DESCRIPTION");
            for a in v.as_array().into_iter().flatten() {
                let ag = &a["agent"];
                let ready = ag["secret_ok"].as_bool() == Some(true) && ag["errors"].as_array().is_none_or(|e| e.is_empty()) && a["connected"].as_bool() == Some(true);
                println!(
                    "{:<12} {:<16} {:<6} {:<34} {:<6}  {}",
                    a["node"].as_str().unwrap_or(""),
                    ag["name"].as_str().unwrap_or(""),
                    ag["source"].as_str().unwrap_or("local"),
                    format!("{}/{}", ag["provider"].as_str().unwrap_or(""), ag["model"].as_str().unwrap_or("")),
                    if ready { "yes" } else { "no" },
                    ag["description"].as_str().unwrap_or("")
                );
            }
        }
        Cmd::Sessions { all } => {
            let v = api.get(&format!("/sessions?all={all}")).await?;
            if raw {
                return Ok(out(&v));
            }
            print_sessions(&serde_json::from_value::<Vec<SessionInfo>>(v)?);
        }
        Cmd::Show { session, preamble: true } => {
            let v = api.get(&format!("/sessions/{session}/preamble")).await?;
            if raw {
                return Ok(out(&v));
            }
            println!("{}", v["preamble"].as_str().unwrap_or_default());
        }
        Cmd::Templates => {
            let v = api.get("/templates").await?;
            if raw {
                return Ok(out(&v));
            }
            println!("{:<16} {:<30} {:<12} {:>9}  {:<24} {}", "TEMPLATE", "MODEL", "SECRET", "LIVE/MAX", "RUNNABLE ON", "OVERRIDABLE");
            for t in v["templates"].as_array().into_iter().flatten() {
                let o = &t["overridable"];
                let mut ov = vec![];
                if o["role_append"].as_bool() == Some(true) {
                    ov.push("role_append".to_string());
                }
                if let Some(b) = o["budget_usd"].as_f64() {
                    ov.push(format!("budget<=${b}"));
                }
                if let Some(r) = o["routes"].as_array().filter(|r| !r.is_empty()) {
                    ov.push(format!("routes {}", r.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join("|")));
                }
                println!(
                    "{:<16} {:<30} {:<12} {:>9}  {:<24} {}",
                    t["name"].as_str().unwrap_or(""),
                    t["model"].as_str().unwrap_or(""),
                    t["secret"].as_str().unwrap_or("-"),
                    format!("{}/{}", t["instances"]["live"], t["instances"]["max"].as_u64().map(|m| m.to_string()).unwrap_or("∞".into())),
                    t["runnable_on"].as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(",")).filter(|s| !s.is_empty()).unwrap_or("-".into()),
                    if ov.is_empty() { "-".into() } else { ov.join(", ") }
                );
                for b in t["not_runnable"].as_array().into_iter().flatten() {
                    println!("    not on {}: {}", b["node"].as_str().unwrap_or(""), b["problems"].as_array().map(|p| p.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join("; ")).unwrap_or_default());
                }
            }
            for e in v["errors"].as_array().into_iter().flatten() {
                println!("error: {}", e.as_str().unwrap_or(""));
            }
        }
        Cmd::Show { session, preamble: false } => {
            let v = api.get(&format!("/sessions/{session}")).await?;
            if raw {
                return Ok(out(&v));
            }
            let s: SessionInfo = serde_json::from_value(v)?;
            let r = &s.report;
            println!("{} · agent {} · node {} · {}", r.id, r.agent, r.node, r.state.as_str());
            if let Some(p) = &s.placement {
                println!("placed: {p}");
            }
            println!("model {}  workspace {}", r.model.as_deref().unwrap_or("-"), r.workspace.as_deref().unwrap_or("-"));
            println!(
                "tokens in {} out {} reasoning {} cache r/w {}/{}  cost ${:.4}",
                r.tokens.input, r.tokens.output, r.tokens.reasoning, r.tokens.cache_read, r.tokens.cache_write, r.tokens.cost_usd
            );
            if r.metered.total() != r.tokens.total() {
                let m = &r.metered;
                println!("metered by keyproxy: in {} out {} reasoning {} cache r/w {}/{}  cost ${:.4}", m.input, m.output, m.reasoning, m.cache_read, m.cache_write, m.cost_usd);
            }
            if let Some(v) = s.verdict {
                println!("verdict {v:?}");
            }
            if let Some(e) = &r.error {
                println!("ERROR {e}");
            }
            if !s.waiting_on.is_empty() {
                println!("waiting on question(s): {}", s.waiting_on.join(", "));
            }
            if s.deaths > 0 || s.replaces.is_some() {
                println!("deaths recovered {}{}", s.deaths, s.replaces.as_deref().map(|r| format!(" · replaces {r}")).unwrap_or_default());
            }
            println!("todos:");
            for t in &r.todos {
                let mark = match t.status.as_str() {
                    "completed" => "x",
                    "in_progress" => ">",
                    "cancelled" => "-",
                    _ => " ",
                };
                println!("  [{mark}] {}", t.content);
            }
            if let Some(t) = &r.last_text {
                println!("last output:\n{t}");
            }
        }
        Cmd::Spawn { agent, prompt, node, title, shared, system_prompt, urgency, append, budget, route, coordinator } => {
            let urgency = urgency.as_deref().map(|u| TaskLabel::parse(u).context("urgency must be right_now, urgent, normal or when_free")).transpose()?;
            let ws = shared.map(|n| json!({"mode": "shared", "name": n}));
            let mut ov = serde_json::Map::new();
            if let Some(a) = append {
                ov.insert("role_append".into(), json!(a));
            }
            if let Some(p) = system_prompt {
                ov.insert("system_prompt".into(), json!(p));
            }
            if let Some(b) = budget {
                ov.insert("budget_usd".into(), json!(b));
            }
            if let Some(r) = route {
                ov.insert("route".into(), json!(r));
            }
            let role = if coordinator { "coordinator" } else { "worker" };
            let v = api.post("/sessions", json!({"agent": agent, "prompt": prompt, "node": node, "title": title, "workspace": ws, "urgency": urgency, "overrides": ov, "role": role})).await?;
            if raw {
                return Ok(out(&v));
            }
            println!("{} on {}", v["id"].as_str().unwrap_or(""), v["node"].as_str().unwrap_or(""));
        }
        Cmd::Send { session, text, steer, inbox } => {
            let d = if steer {
                "steer"
            } else if inbox {
                "inbox"
            } else {
                "queue"
            };
            api.post(&format!("/sessions/{session}/message"), json!({"text": text, "delivery": d})).await?;
            println!("sent ({d})");
        }
        Cmd::Interrupt { session } => {
            api.post(&format!("/sessions/{session}/interrupt"), json!({})).await?;
            println!("interrupted");
        }
        Cmd::Stop { session, rm_workspace } => {
            api.req(reqwest::Method::DELETE, &format!("/sessions/{session}?remove_workspace={rm_workspace}"), None).await?;
            println!("stopped");
        }
        Cmd::Permit { session, request, reply } => {
            api.post(&format!("/sessions/{session}/permissions/{request}"), json!({"reply": reply})).await?;
            println!("ok");
        }
        Cmd::Task { cmd } => match cmd {
            TaskCmd::Add { title, body, label, tag, question_mode } => {
                let l = TaskLabel::parse(&label).context("label must be right_now, urgent, normal, when_free or on_demand")?;
                let qm = question_mode.as_deref().map(|m| QuestionMode::parse(m).context("question mode must be ask_me_everything, important_and_vital, vital_only or autonomous")).transpose()?;
                let v = api.post("/tasks", json!({"title": title, "body": body, "label": l, "tags": tag, "question_mode": qm})).await?;
                if raw {
                    return Ok(out(&v));
                }
                println!("task #{} queued ({})", v["id"], l.as_str());
            }
            TaskCmd::Ls => {
                let v = api.get("/tasks").await?;
                if raw {
                    return Ok(out(&v));
                }
                print_tasks(&serde_json::from_value::<Vec<Task>>(v)?);
            }
            TaskCmd::Show { id } => out(&api.get(&format!("/tasks/{id}")).await?),
            TaskCmd::Edit { id, title, body, label, question_mode } => {
                let label = match label {
                    Some(l) => Some(TaskLabel::parse(&l).context("bad label")?),
                    None => None,
                };
                let qm = question_mode.as_deref().map(|m| QuestionMode::parse(m).context("bad question mode")).transpose()?;
                let v = api.req(reqwest::Method::PATCH, &format!("/tasks/{id}"), Some(json!({"title": title, "body": body, "label": label, "question_mode": qm}))).await?;
                out(&v);
            }
            TaskCmd::Order { ids } => {
                let v = api.post("/tasks/reorder", json!({"ids": ids})).await?;
                if raw {
                    return Ok(out(&v));
                }
                print_tasks(&serde_json::from_value::<Vec<Task>>(v)?);
            }
            TaskCmd::Run { id } => {
                api.post(&format!("/tasks/{id}/run"), json!({})).await?;
                println!("task #{id} started");
            }
            TaskCmd::Cancel { id } => {
                api.post(&format!("/tasks/{id}/cancel"), json!({})).await?;
                println!("task #{id} cancelled");
            }
            TaskCmd::Rm { id } => {
                api.req(reqwest::Method::DELETE, &format!("/tasks/{id}"), None).await?;
                println!("task #{id} deleted");
            }
        },
        Cmd::Workflow { cmd } => match cmd {
            WorkflowCmd::Ls => {
                let v = api.get("/workflows").await?;
                if raw {
                    return Ok(out(&v));
                }
                for w in v.as_array().into_iter().flatten() {
                    let steps: Vec<&str> = w["steps"].as_array().into_iter().flatten().filter_map(|s| s["name"].as_str()).collect();
                    println!("{:<20} {}  [{}]", w["name"].as_str().unwrap_or(""), w["description"].as_str().unwrap_or(""), steps.join(" → "));
                }
            }
            WorkflowCmd::Run { name, input } => {
                let v = api.post(&format!("/workflows/{name}/run"), json!({"input": input})).await?;
                if raw {
                    return Ok(out(&v));
                }
                println!("run {} started", v["id"].as_str().unwrap_or(""));
            }
            WorkflowCmd::Runs => {
                let v = api.get("/runs").await?;
                if raw {
                    return Ok(out(&v));
                }
                for r in serde_json::from_value::<Vec<WorkflowRun>>(v)? {
                    let steps: Vec<String> = r.steps.iter().map(|s| format!("{}:{}", s.step, s.verdict.map(|v| format!("{v:?}")).unwrap_or("…".into()))).collect();
                    println!("{:<14} {:<16} {:<9} {:>5}  {}", r.id, r.workflow, format!("{:?}", r.status), ago(r.created_at), steps.join(" → "));
                }
            }
            WorkflowCmd::Show { id } => out(&api.get(&format!("/runs/{id}")).await?),
            WorkflowCmd::Cancel { id } => {
                api.post(&format!("/runs/{id}/cancel"), json!({})).await?;
                println!("cancelled");
            }
        },
        Cmd::Events { follow, session, limit } => {
            let q = session.as_ref().map(|s| format!("&session={s}")).unwrap_or_default();
            let v = api.get(&format!("/events?after=0&limit=100000{q}")).await?;
            let all: Vec<LogEvent> = serde_json::from_value(v)?;
            let start = all.len().saturating_sub(limit as usize);
            for e in &all[start..] {
                print_event(e, raw);
            }
            if follow {
                let resp = api.http.get(format!("{}/api/v1/stream", api.base)).bearer_auth(&api.token).send().await?;
                let mut body = resp.bytes_stream();
                let mut buf = String::new();
                while let Some(chunk) = body.next().await {
                    buf.push_str(&String::from_utf8_lossy(&chunk?));
                    while let Some(i) = buf.find("\n\n") {
                        let block: String = buf.drain(..i + 2).collect();
                        for line in block.lines().filter_map(|l| l.strip_prefix("data:")) {
                            if let Ok(StreamEvent::Log(e)) = serde_json::from_str::<StreamEvent>(line.trim()) {
                                if session.as_deref().is_none_or(|s| e.session.as_deref() == Some(s)) {
                                    print_event(&e, raw);
                                }
                            }
                        }
                    }
                }
            }
        }
        Cmd::Audit { verify, limit } => {
            if verify {
                out(&api.get("/audit/verify").await?);
            } else {
                let v = api.get(&format!("/audit?limit={limit}")).await?;
                if raw {
                    return Ok(out(&v));
                }
                for e in v.as_array().into_iter().flatten().rev() {
                    println!(
                        "{} {:<24} {:<20} {:<16} {}",
                        fmt_ts(e["ts"].as_i64().unwrap_or(0)),
                        e["actor"].as_str().unwrap_or(""),
                        e["action"].as_str().unwrap_or(""),
                        e["target"].as_str().unwrap_or(""),
                        if e["ok"].as_bool() == Some(true) { "ok" } else { "FAILED" }
                    );
                }
            }
        }
        Cmd::Questions { all, cmd: Some(QuestionsCmd::Mode { mode, task }) } => {
            let _ = all;
            match mode {
                Some(m) => {
                    let m = QuestionMode::parse(&m).context("mode must be ask_me_everything, important_and_vital, vital_only or autonomous")?;
                    api.post("/questions/mode", json!({"mode": m, "task": task})).await?;
                    println!("question mode {}{}", m.as_str(), task.map(|t| format!(" for task #{t}")).unwrap_or_else(|| " for the cluster".into()));
                }
                None => {
                    let v = api.get("/questions/mode").await?;
                    if raw {
                        return Ok(out(&v));
                    }
                    println!("cluster: {}", v["mode"].as_str().unwrap_or("?"));
                    for t in v["tasks"].as_array().into_iter().flatten() {
                        println!("task #{}: {}", t["task"], t["mode"].as_str().unwrap_or("?"));
                    }
                }
            }
        }
        Cmd::Questions { all, cmd: None } => {
            let v = api.get(&format!("/questions?all={all}")).await?;
            if raw {
                return Ok(out(&v));
            }
            let qs: Vec<Question> = serde_json::from_value(v)?;
            if qs.is_empty() {
                println!("no {}questions", if all { "" } else { "open " });
            }
            for q in &qs {
                let chain: Vec<String> = std::iter::once(format!("{} {}", q.asker_agent, q.asker)).chain(q.chain.iter().map(|h| format!("{} ({})", h.holder, h.why))).collect();
                println!(
                    "{}  {:<6} {:<10} {:<9} blocked {}  {}  on {}{}",
                    q.id,
                    q.importance.as_str(),
                    format!("{:?}", q.status).to_lowercase(),
                    format!("{:?}", q.kind).to_lowercase(),
                    q.blocked,
                    ago(q.created_at),
                    q.node,
                    q.task_id.map(|t| format!(" task #{t}")).unwrap_or_default()
                );
                println!("    {}", q.text.lines().collect::<Vec<_>>().join("\n    "));
                for (i, o) in q.options.iter().enumerate() {
                    println!("      {}) {}{}", i + 1, o.label, if o.description.is_empty() { String::new() } else { format!(" — {}", o.description) });
                }
                println!("    chain: {}   held by {}{}", chain.join(" → "), q.holder, if q.top_decides { " (decides)" } else { "" });
                for h in q.chain.iter().filter(|h| h.note.is_some()) {
                    println!("    note ({}): {}", h.holder, h.note.as_deref().unwrap_or(""));
                }
                if !q.raised_because.is_empty() {
                    println!("    raised to {}: {}", q.importance.as_str(), q.raised_because.join(", "));
                }
                if let Some(a) = &q.answer {
                    println!("    answer ({}): {}", q.answered_by.as_deref().unwrap_or("?"), a.chars().take(300).collect::<String>());
                }
            }
        }
        Cmd::Answer { id, text, choice, decide, reject } => {
            let v = api.post(&format!("/questions/{id}/answer"), json!({"answer": text, "choice": choice, "let_agent_decide": decide, "reject": reject})).await?;
            if raw {
                return Ok(out(&v));
            }
            println!("{} {} → delivered to {}", id, v["status"].as_str().unwrap_or("?"), v["asker"].as_str().unwrap_or("?"));
        }
        Cmd::Assumptions { task } => {
            let v = api.get(&format!("/assumptions{}", task.map(|t| format!("?task={t}")).unwrap_or_default())).await?;
            if raw {
                return Ok(out(&v));
            }
            for a in serde_json::from_value::<Vec<Assumption>>(v)? {
                println!(
                    "#{:<4} {} {:<14} by {:<14} {}{}",
                    a.id,
                    fmt_ts(a.ts),
                    a.session,
                    a.decided_by,
                    a.mode.as_str(),
                    a.task_id.map(|t| format!(" task #{t}")).unwrap_or_default()
                );
                println!("      {}", a.text.lines().collect::<Vec<_>>().join("\n      "));
            }
        }
        Cmd::Artifact { cmd } => artifact_cmd(&api, cmd, raw).await?,
        Cmd::Attach { session, port, no_launch } => {
            let v = api.post(&format!("/sessions/{session}/attach"), json!({})).await?;
            let url = v["url"].as_str().context("no attach URL")?.to_string();
            attach::run(&api.http, &url, port, !no_launch).await?;
        }
        Cmd::Tui => tui::run(&api).await?,
        Cmd::Halts { all } => {
            let v = api.get(&format!("/halts?all={all}")).await?;
            if raw {
                return Ok(out(&v));
            }
            let hs: Vec<Halt> = serde_json::from_value(v)?;
            if hs.is_empty() {
                println!("no {}halts", if all { "" } else { "open " });
            }
            for h in hs {
                println!("{}  {}  {}  {} died ({})  spent ${:.4}  {}", h.id, h.status, h.scope, h.dead_session, h.reason, h.spent_usd, ago(h.at));
                if let Some(t) = &h.current_todo {
                    println!("    was doing: {t}");
                }
                if !h.sessions.is_empty() {
                    println!("    halted: {}", h.sessions.join(", "));
                }
                for o in &h.options {
                    println!("    {} {:<17} ~${:<8.4} {}", if o.available { "*" } else { " " }, o.kind, o.est_cost_usd, o.note);
                }
                if let Some(r) = &h.resolution {
                    println!("    resolved: {r}");
                }
            }
        }
        Cmd::Recover { target, option, node } => {
            let v = api.post(&if target.starts_with("h-") { format!("/halts/{target}/recover") } else { format!("/sessions/{target}/recover") }, json!({"option": option, "node": node})).await?;
            out(&v);
        }
        Cmd::Harvests { session } => {
            let v = api.get(&format!("/harvests{}", session.map(|s| format!("?session={s}")).unwrap_or_default())).await?;
            if raw {
                return Ok(out(&v));
            }
            for h in serde_json::from_value::<Vec<Harvest>>(v)? {
                println!("{} {} on {} ({}){}", fmt_ts(h.at), h.session, h.node, h.trigger, if h.from_checkpoint { " FROM CHECKPOINT" } else { "" });
                for o in &h.outputs {
                    println!("    {:<24} {} {:>8}B → {} {}", o.path, o.reference, o.size, o.to, o.delivered.as_deref().unwrap_or(""));
                }
                for m in &h.missing {
                    println!("    {m:<24} (matched nothing)");
                }
                if let Some(t) = &h.transcript_ref {
                    println!("    transcript {t}");
                }
                for n in &h.notes {
                    println!("    note: {n}");
                }
            }
        }
        Cmd::Notifications { unread, read, limit } => {
            let v = api.get(&format!("/notifications?unread={unread}&limit={limit}")).await?;
            let ns: Vec<Notification> = serde_json::from_value(v.clone())?;
            if read {
                if let Some(max) = ns.iter().map(|n| n.id).max() {
                    api.post("/notifications/read", json!({"upto": max})).await?;
                }
            }
            if raw {
                return Ok(out(&v));
            }
            for n in ns {
                println!("{}{:>4} {} {:<7} {:<18} {}", if n.read { " " } else { "●" }, n.id, fmt_ts(n.ts), n.severity, n.kind, n.title);
                if !n.body.is_empty() {
                    println!("       {}", n.body);
                }
            }
        }
        Cmd::Housekeeping { dry_run, ahead } => {
            let v = api.post("/housekeeping", json!({"dry_run": dry_run, "ahead": ahead})).await?;
            if raw {
                return Ok(out(&v));
            }
            let n = |k: &str| v[k].as_u64().unwrap_or(0);
            let list = |k: &str| v[k].as_array().map(|a| a.len()).unwrap_or(0);
            println!("{}events exported {} (files: {}), pruned {}", if dry_run { "(dry run) " } else { "" }, n("events_exported"), v["export_files"].as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default(), n("events_pruned"));
            println!(
                "artifacts removed {} ({} bytes), kept: {} referenced, {} tagged; old export files removed {}",
                list("artifacts_removed"),
                n("artifact_bytes_freed"),
                n("artifacts_kept_referenced"),
                n("artifacts_kept_tagged"),
                list("export_files_removed")
            );
        }
        Cmd::Token { cmd } => match cmd {
            TokenCmd::Create { name, scope } => out(&api.post("/tokens", json!({"name": name, "scope": scope})).await?),
            TokenCmd::Ls => out(&api.get("/tokens").await?),
            TokenCmd::Revoke { name } => out(&api.req(reqwest::Method::DELETE, &format!("/tokens/{name}"), None).await?),
        },
    }
    Ok(())
}

async fn artifact_cmd(api: &Api, cmd: ArtifactCmd, raw: bool) -> Result<()> {
    use moocher_protocol::archive;
    match cmd {
        ArtifactCmd::Put { path, name, tag, parent } => {
            let meta = std::fs::metadata(&path).with_context(|| format!("{}", path.display()))?;
            let (file, kind, cleanup) = if meta.is_dir() {
                let root = std::fs::canonicalize(&path)?;
                let (m, skipped) = archive::manifest(&root, "", &|_| false)?;
                if !skipped.is_empty() {
                    eprintln!("note: skipped {} link(s)/special file(s)", skipped.len());
                }
                let tmp = std::env::temp_dir().join(format!("moo-put-{}.tar.zst", std::process::id()));
                archive::pack(&root, &m, &tmp)?;
                (tmp, "tree", true)
            } else {
                (path.clone(), "file", false)
            };
            let default_name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "artifact".into());
            let q = format!(
                "name={}&kind={kind}&tags={}&parents={}",
                enc(&name.unwrap_or(default_name)),
                enc(&tag.join(",")),
                enc(&parent.join(","))
            );
            // Stream the file: artifacts can be large.
            let f = tokio::fs::File::open(&file).await?;
            let chunks = futures::stream::unfold(f, |mut f| async move {
                use tokio::io::AsyncReadExt;
                let mut buf = vec![0u8; 256 * 1024];
                match f.read(&mut buf).await {
                    Ok(0) => None,
                    Ok(n) => {
                        buf.truncate(n);
                        Some((Ok::<Vec<u8>, std::io::Error>(buf), f))
                    }
                    Err(e) => Some((Err(e), f)),
                }
            });
            let sent = api.http.post(format!("{}/api/v1/artifacts?{q}", api.base)).bearer_auth(&api.token).body(reqwest::Body::wrap_stream(chunks)).send().await;
            if cleanup {
                let _ = std::fs::remove_file(&file);
            }
            let resp = sent?;
            let status = resp.status();
            let v: Value = resp.json().await.unwrap_or(Value::Null);
            if !status.is_success() {
                bail!("{status}: {}", v["error"].as_str().unwrap_or("upload failed"));
            }
            if raw {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                println!("{}  {} ({}, {} bytes)", v["reference"].as_str().unwrap_or(""), v["name"].as_str().unwrap_or(""), kind, v["size"]);
            }
        }
        ArtifactCmd::Get { reference, out } => {
            let info = api.get(&format!("/artifacts/{}", enc(&reference))).await?;
            let a: ArtifactMeta = serde_json::from_value(info["artifact"].clone())?;
            let resp = api.http.get(format!("{}/api/v1/artifacts/{}/blob", api.base, a.hash)).bearer_auth(&api.token).send().await?;
            if !resp.status().is_success() {
                bail!("{}: download failed", resp.status());
            }
            // Stream to a temporary file next to the destination, verify, then move it in place.
            let to = match (&out, a.kind.as_str()) {
                (_, "tree") => std::env::temp_dir().join(format!("moo-get-{}.tar.zst", std::process::id())),
                (Some(o), _) => o.clone(),
                (None, _) => std::path::PathBuf::from(std::path::Path::new(&a.name).file_name().unwrap_or_default()),
            };
            let part = to.with_extension("moo-part");
            let mut f = tokio::fs::File::create(&part).await?;
            let mut h = blake3::Hasher::new();
            let mut n = 0u64;
            let mut body = resp.bytes_stream();
            while let Some(c) = body.next().await {
                let c = c?;
                h.update(&c);
                n += c.len() as u64;
                tokio::io::AsyncWriteExt::write_all(&mut f, &c).await?;
            }
            tokio::io::AsyncWriteExt::flush(&mut f).await?;
            let got = h.finalize().to_hex().to_string();
            if got != a.hash {
                let _ = std::fs::remove_file(&part);
                bail!("download corrupted (hash {got}, expected {})", a.hash);
            }
            std::fs::rename(&part, &to)?;
            if a.kind == "tree" {
                let dir = out.unwrap_or_else(|| std::path::PathBuf::from(a.name.replace(['/', ' ', '(', ')', ':'], "_")));
                let m = archive::unpack(&to, &dir, u64::MAX);
                let _ = std::fs::remove_file(&to);
                println!("{} files into {}", m?.len(), dir.display());
            } else {
                println!("{n} bytes to {}", to.display());
            }
        }
        ArtifactCmd::Ls { query, task, limit } => {
            let mut q = format!("?limit={limit}");
            if let Some(x) = query {
                q.push_str(&format!("&q={}", enc(&x)));
            }
            if let Some(t) = task {
                q.push_str(&format!("&task={t}"));
            }
            let v = api.get(&format!("/artifacts{q}")).await?;
            if raw {
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            println!("{:<22} {:<5} {:>10} {:<14} {:>5}  {}", "REF", "KIND", "SIZE", "PRODUCER", "AGE", "NAME");
            for a in serde_json::from_value::<Vec<ArtifactMeta>>(v)? {
                println!("{:<22} {:<5} {:>10} {:<14} {:>5}  {}", format!("art://b3/{}", &a.hash[..12]), a.kind, a.size, a.producer.as_deref().unwrap_or("operator"), ago(a.created_at), a.name);
            }
        }
        ArtifactCmd::Show { reference } => {
            let v = api.get(&format!("/artifacts/{}", enc(&reference))).await?;
            if raw {
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            let a: ArtifactMeta = serde_json::from_value(v["artifact"].clone())?;
            println!("{}\n  name {}  kind {}  size {}  type {}", a.reference, a.name, a.kind, a.size, a.media_type);
            println!("  made by {} on {}{}  {}", a.producer.as_deref().unwrap_or("operator"), a.node.as_deref().unwrap_or("-"), a.task_id.map(|t| format!(" (task #{t})")).unwrap_or_default(), fmt_ts(a.created_at));
            if !a.tags.is_empty() {
                println!("  tags {}", a.tags.join(", "));
            }
            for p in v["parents"].as_array().into_iter().flatten() {
                println!("  from    {} {}", p["reference"].as_str().unwrap_or(""), p["name"].as_str().unwrap_or(""));
            }
            for c in v["children"].as_array().into_iter().flatten() {
                println!("  used by {} {}", c["reference"].as_str().unwrap_or(""), c["name"].as_str().unwrap_or(""));
            }
        }
    }
    Ok(())
}

fn enc(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

fn fmt_ts(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map(|d| d.with_timezone(&chrono::Local).format("%H:%M:%S").to_string()).unwrap_or_default()
}

fn print_event(e: &LogEvent, raw: bool) {
    if raw {
        println!("{}", serde_json::to_string(e).unwrap_or_default());
        return;
    }
    let d = e.data.to_string();
    println!(
        "{} {:<20} {:<14} {}",
        fmt_ts(e.ts),
        e.kind,
        e.session.as_deref().or(e.node.as_deref()).unwrap_or("-"),
        d.chars().take(140).collect::<String>()
    );
}
