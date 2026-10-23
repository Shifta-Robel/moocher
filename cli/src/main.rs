//! ocl — scriptable client for the supervisor API (mirrors the web UI).
//!
//! Auth: `OCL_TOKEN`, else `~/.opencode-cluster/cli-token`. Server: `OCL_URL`
//! (default http://127.0.0.1:7700). Add `--json` to any command for raw output.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use futures::StreamExt;
use ocl_protocol::api::*;
use ocl_protocol::model::*;
use serde_json::{json, Value};

#[derive(Parser)]
#[command(name = "ocl", version, about = "opencode-cluster CLI")]
struct Cli {
    #[arg(long, env = "OCL_URL", default_value = "http://127.0.0.1:7700", global = true)]
    url: String,
    #[arg(long, env = "OCL_TOKEN", hide_env_values = true, global = true)]
    token: Option<String>,
    /// Print raw JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Nodes, live sessions, tasks and token totals.
    Status,
    /// List nodes (same as `ocl node ls`).
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
    /// Provider allowances per IP / key / account, with reset countdowns.
    Quotas {
        /// Only this provider.
        provider: Option<String>,
    },
    /// Egress IP of every route, and which nodes/sessions share an IP.
    Ips,
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
    Show { session: String },
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
}

#[derive(Subcommand)]
enum TaskCmd {
    /// Add a task: labels urgent | when_free | on_demand.
    Add {
        title: String,
        #[arg(short, long, default_value = "")]
        body: String,
        #[arg(short, long, default_value = "when_free")]
        label: String,
        /// Repeatable. `workflow:<name>` runs that workflow instead of a coordinator.
        #[arg(short, long)]
        tag: Vec<String>,
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
    /// e.g. `ocl quota set kilo-free --scope-id 203.0.113.7 --resets-at 03:15 --remaining 0`
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
    /// e.g. `ocl route set worker-a builder --prefer direct --failover via-vps,home-proxy`
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

struct Api {
    base: String,
    token: String,
    http: reqwest::Client,
}

impl Api {
    async fn req(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value> {
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
    async fn get(&self, p: &str) -> Result<Value> {
        self.req(reqwest::Method::GET, p, None).await
    }
    async fn post(&self, p: &str, b: Value) -> Result<Value> {
        self.req(reqwest::Method::POST, p, Some(b)).await
    }
}

fn token(cli: &Cli) -> Result<String> {
    if let Some(t) = &cli.token {
        return Ok(t.clone());
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let p = std::path::Path::new(&home).join(".opencode-cluster/cli-token");
    Ok(std::fs::read_to_string(&p).with_context(|| format!("no OCL_TOKEN and cannot read {}", p.display()))?.trim().to_string())
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
            r.tokens.total(),
            format!("${:.4}", r.tokens.cost_usd),
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
        let ms = ocl_protocol::config::parse_duration_ms(d).context("bad duration (e.g. +2h30m)")?;
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
    // `ocl events | head` should just stop, like other CLI tools, instead of
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
    let api = Api { base: cli.url.trim_end_matches('/').to_string(), token: token(&cli)?, http: reqwest::Client::new() };
    let raw = cli.json;
    let out = |v: &Value| println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
    match cli.cmd {
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
                    println!("After it enrolls, approve it with: ocl node approve {name}");
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
            QuotaCmd::Set { provider, scope_id, node, resets_at, remaining, note } => {
                let resets_at = resets_at.as_deref().map(parse_when).transpose()?;
                let v = api
                    .post("/quotas/override", json!({"provider": provider, "scope_id": scope_id, "node": node, "resets_at": resets_at, "remaining": remaining, "note": note}))
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
            println!("{:<12} {:<16} {:<34} {:<6}  {}", "NODE", "AGENT", "MODEL", "READY", "DESCRIPTION");
            for a in v.as_array().into_iter().flatten() {
                let ag = &a["agent"];
                let ready = ag["secret_ok"].as_bool() == Some(true) && ag["errors"].as_array().is_none_or(|e| e.is_empty()) && a["connected"].as_bool() == Some(true);
                println!(
                    "{:<12} {:<16} {:<34} {:<6}  {}",
                    a["node"].as_str().unwrap_or(""),
                    ag["name"].as_str().unwrap_or(""),
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
        Cmd::Show { session } => {
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
            if let Some(v) = s.verdict {
                println!("verdict {v:?}");
            }
            if let Some(e) = &r.error {
                println!("ERROR {e}");
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
        Cmd::Spawn { agent, prompt, node, title, shared } => {
            let ws = shared.map(|n| json!({"mode": "shared", "name": n}));
            let v = api.post("/sessions", json!({"agent": agent, "prompt": prompt, "node": node, "title": title, "workspace": ws})).await?;
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
            TaskCmd::Add { title, body, label, tag } => {
                let l = TaskLabel::parse(&label).context("label must be urgent, when_free or on_demand")?;
                let v = api.post("/tasks", json!({"title": title, "body": body, "label": l, "tags": tag})).await?;
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
            TaskCmd::Edit { id, title, body, label } => {
                let label = match label {
                    Some(l) => Some(TaskLabel::parse(&l).context("bad label")?),
                    None => None,
                };
                let v = api.req(reqwest::Method::PATCH, &format!("/tasks/{id}"), Some(json!({"title": title, "body": body, "label": label}))).await?;
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
        Cmd::Token { cmd } => match cmd {
            TokenCmd::Create { name, scope } => out(&api.post("/tokens", json!({"name": name, "scope": scope})).await?),
            TokenCmd::Ls => out(&api.get("/tokens").await?),
            TokenCmd::Revoke { name } => out(&api.req(reqwest::Method::DELETE, &format!("/tokens/{name}"), None).await?),
        },
    }
    Ok(())
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
