//! Per-session gateway, served on `<run>/<id>/node.sock` and reachable from the
//! container as `http://127.0.0.1:7777` through ocl-bridge.
//!
//! * `/llm/<provider>/...` — **keyproxy**: checks the session capability token,
//!   model allowlist and budget, swaps in the real API key (resolved on this
//!   node, per request) and forwards over the agent's egress route. The agent
//!   only ever holds the capability token.
//! * `/mcp` — **cluster MCP server** (streamable HTTP, JSON responses): the
//!   tools agents use to message, spawn and report to the cluster.

use crate::cfgstore::ConfigStore;
use crate::egress::Egress;
use crate::link::LinkHandle;
use crate::quota::{self, Quotas, Target};
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use futures::StreamExt;
use ocl_protocol::config::{AuthStyle, OnExhausted, Route, SpawnOverrides};
use ocl_protocol::link::{AgentCall, NodeEvent};
use ocl_protocol::model::{now_ms, SessionRole, TokenUsage, Ts};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::net::UnixListener;

/// Set while every route's allowance is used up: (until, explanation).
pub type Parked = Arc<Mutex<Option<(Ts, String)>>>;

#[derive(Clone)]
pub struct Gateway {
    pub id: String,
    pub agent: String,
    pub role: SessionRole,
    cap: Arc<String>,
    cfg: ConfigStore,
    pub usage: Arc<Mutex<TokenUsage>>,
    link: LinkHandle,
    egress: Arc<Egress>,
    quotas: Arc<Quotas>,
    pub parked: Parked,
    /// The bucket that served the previous request, and the session's token
    /// total when it was sent: its tokens are attributed on the next request.
    prev: Arc<Mutex<Option<(Target, String, u64)>>>,
    /// Route of the last successful call (to report failovers once).
    last_route: Arc<Mutex<Option<String>>>,
    /// Per-instance changes allowed by the template (budget, preferred route).
    overrides: Arc<SpawnOverrides>,
}

const MAX_BODY: usize = 32 * 1024 * 1024;

pub struct GatewayDeps {
    pub cfg: ConfigStore,
    pub link: LinkHandle,
    pub egress: Arc<Egress>,
    pub quotas: Arc<Quotas>,
}

impl Gateway {
    #[allow(clippy::too_many_arguments)]
    pub fn new(id: &str, agent: &str, role: SessionRole, cap: &str, usage: Arc<Mutex<TokenUsage>>, parked: Parked, overrides: SpawnOverrides, deps: &GatewayDeps) -> Self {
        Gateway {
            id: id.into(),
            agent: agent.into(),
            role,
            cap: Arc::new(cap.into()),
            cfg: deps.cfg.clone(),
            usage,
            link: deps.link.clone(),
            egress: deps.egress.clone(),
            quotas: deps.quotas.clone(),
            parked,
            prev: Arc::new(Mutex::new(None)),
            last_route: Arc::new(Mutex::new(None)),
            overrides: Arc::new(overrides),
        }
    }

    /// Bind the socket and serve until the returned task is aborted.
    pub fn serve(self, sock: PathBuf) -> anyhow::Result<tokio::task::JoinHandle<()>> {
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock)?;
        {
            use std::os::unix::fs::PermissionsExt;
            // The 0700 parent dir is the boundary; the container user must connect.
            std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o666))?;
        }
        let app = Router::new()
            .route("/healthz", get(|| async { "ok" }))
            .route("/llm/{provider}/{*rest}", any(keyproxy))
            .route("/mcp", post(mcp).get(|| async { StatusCode::METHOD_NOT_ALLOWED }))
            .with_state(self);
        Ok(tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                tracing::warn!(error = %e, "gateway stopped");
            }
        }))
    }

    fn check_cap(&self, headers: &HeaderMap, query: Option<&str>) -> bool {
        let mut presented: Vec<String> = vec![];
        if let Some(v) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
            presented.push(v.trim_start_matches("Bearer ").trim().to_string());
        }
        for h in ["x-api-key", "x-goog-api-key"] {
            if let Some(v) = headers.get(h).and_then(|v| v.to_str().ok()) {
                presented.push(v.trim().to_string());
            }
        }
        if let Some(q) = query {
            for kv in q.split('&') {
                if let Some(v) = kv.strip_prefix("key=") {
                    presented.push(v.to_string());
                }
            }
        }
        presented.iter().any(|p| ct_eq(p.as_bytes(), self.cap.as_bytes()))
    }
}

fn error_chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut cur = e.source();
    while let Some(c) = cur {
        s.push_str(": ");
        s.push_str(&c.to_string());
        cur = c.source();
    }
    s
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn llm_error(status: StatusCode, msg: &str) -> Response {
    // Shaped like an OpenAI error so OpenCode surfaces the message.
    (status, Json(json!({"error": {"message": format!("[opencode-cluster] {msg}"), "type": "ocl_gateway"}}))).into_response()
}

const HOP: &[&str] = &[
    "host",
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "te",
    "upgrade",
    "content-length",
    "authorization",
    "x-api-key",
    "x-goog-api-key",
];

async fn keyproxy(State(g): State<Gateway>, Path((provider, rest)): Path<(String, String)>, req: Request) -> Response {
    let query = req.uri().query().map(String::from);
    if !g.check_cap(req.headers(), query.as_deref()) {
        return llm_error(StatusCode::UNAUTHORIZED, "invalid capability token");
    }
    let snap = g.cfg.get();
    let Some(agent) = snap.agents.get(&g.agent) else {
        return llm_error(StatusCode::FORBIDDEN, "agent profile no longer exists on this node");
    };
    if provider != agent.cfg.provider.id {
        return llm_error(StatusCode::FORBIDDEN, &format!("provider {provider} not allowed for agent {}", g.agent));
    }
    let Some(pdef) = snap.providers.get(&provider).cloned() else {
        return llm_error(StatusCode::BAD_GATEWAY, "provider not configured");
    };
    let usage_total = {
        let u = *g.usage.lock().unwrap();
        if let Some(b) = g.overrides.budget_usd.or(agent.cfg.limits.budget_usd) {
            if u.cost_usd >= b {
                return llm_error(StatusCode::TOO_MANY_REQUESTS, &format!("session budget ${b:.2} exhausted"));
            }
        }
        if let Some(m) = agent.cfg.limits.max_tokens {
            if u.total() >= m {
                return llm_error(StatusCode::TOO_MANY_REQUESTS, &format!("session token cap {m} reached"));
            }
        }
        u.total()
    };
    let method = req.method().clone();
    let mut headers = req.headers().clone();
    let body = match axum::body::to_bytes(req.into_body(), MAX_BODY).await {
        Ok(b) => b,
        Err(_) => return llm_error(StatusCode::PAYLOAD_TOO_LARGE, "request too large"),
    };
    // Model allowlist: JSON bodies name the model (OpenAI/Anthropic); Gemini puts it in the path.
    if let Ok(v) = serde_json::from_slice::<Value>(&body) {
        if let Some(m) = v.get("model").and_then(|m| m.as_str()) {
            if m != agent.cfg.provider.model {
                return llm_error(StatusCode::FORBIDDEN, &format!("model {m} not allowed (profile model is {})", agent.cfg.provider.model));
            }
        }
    }
    let (secret, account) = match &agent.cfg.provider.key {
        None => (None, None),
        Some(r) => match snap.secret(r) {
            Ok((s, account)) => (Some(s), account),
            Err(e) => {
                tracing::warn!(agent = %g.agent, error = %e, "secret unresolvable");
                return llm_error(StatusCode::BAD_GATEWAY, "provider credential unavailable on this node");
            }
        },
    };
    let mut url = format!("{}/{}", pdef.base_url.trim_end_matches('/'), rest);
    let mut q: Vec<String> = query.unwrap_or_default().split('&').filter(|kv| !kv.is_empty() && !kv.starts_with("key=")).map(String::from).collect();
    for h in HOP {
        headers.remove(*h);
    }
    for (k, v) in &pdef.headers {
        if let (Ok(k), Ok(v)) = (HeaderName::try_from(k.as_str()), HeaderValue::try_from(v.as_str())) {
            headers.insert(k, v);
        }
    }
    if let Some(s) = &secret {
        match &pdef.auth {
            AuthStyle::Bearer => {
                headers.insert("authorization", HeaderValue::try_from(format!("Bearer {s}")).unwrap());
            }
            AuthStyle::Header { name } => {
                if let Ok(n) = HeaderName::try_from(name.as_str()) {
                    headers.insert(n, HeaderValue::try_from(s.as_str()).unwrap());
                }
            }
            AuthStyle::Query { name } => q.push(format!("{name}={s}")),
            AuthStyle::None => {}
        }
    }
    if !q.is_empty() {
        url.push('?');
        url.push_str(&q.join("&"));
    }

    // The previous request's tokens are known now; account them where it went.
    let spec = quota::spec_for(&pdef);
    let limits = spec.limits.clone();
    g.settle_tokens();

    // Try the preferred route, then the failovers, skipping exhausted allowances.
    let mut plan = snap.route_plan(&agent.cfg);
    if let Some(r) = &g.overrides.route {
        // The spawner chose where this instance prefers to go; failovers stay.
        plan.failover.retain(|f| f != r);
        if plan.prefer != *r {
            plan.failover.insert(0, std::mem::replace(&mut plan.prefer, r.clone()));
        }
    }
    let mut names: Vec<String> = vec![];
    for n in std::iter::once(plan.prefer.clone()).chain(plan.failover.iter().cloned()) {
        if !names.contains(&n) {
            names.push(n);
        }
    }
    let scope = quota::default_scope(&pdef, secret.is_some());
    let mut skipped: Vec<String> = vec![];
    let mut soonest: Option<Ts> = None;
    let mut all_exhausted = true;
    for (i, name) in names.iter().enumerate() {
        let Some(route) = snap.route(name) else {
            skipped.push(format!("{name}: unknown route"));
            all_exhausted = false;
            continue;
        };
        let ip = g.egress.ip_of(name);
        let target = Target { provider: provider.clone(), scope, scope_id: quota::scope_id(scope, &snap.node.name, name, ip.as_deref(), secret.as_deref(), account.as_deref()) };
        g.quotas.ensure(&target, name, &limits);
        let now = now_ms();
        if let Some(until) = g.quotas.exhausted(&target, &limits, &snap.overrides.quotas, now) {
            skipped.push(format!("{name} ({}): allowance used up, resets in {}", target.scope_id, human_in(until - now)));
            soonest = Some(soonest.map_or(until, |s: Ts| s.min(until)));
            continue;
        }
        let client = match g.egress.client(&route) {
            Ok(c) => c,
            Err(e) => {
                skipped.push(format!("{name}: {e}"));
                all_exhausted = false;
                continue;
            }
        };
        g.quotas.record_request(&target, name, &g.id, &limits);
        *g.prev.lock().unwrap() = Some((target.clone(), name.clone(), usage_total));
        let sent = std::time::Instant::now();
        let upstream = match client.request(method.clone(), &url).headers(headers.clone()).body(body.clone()).send().await {
            Ok(r) => {
                g.egress.observe_call(name, &provider, Some(sent.elapsed()), None);
                r
            }
            Err(e) => {
                let mut why = error_chain(&e.without_url());
                if let (Route::ViaNode { node }, Some(d)) = (&route, g.egress.dialer()) {
                    if let Some(r) = d.last_error(node) {
                        why = format!("relay {node}: {r}");
                    }
                }
                tracing::warn!(agent = %g.agent, provider, route = %name, error = %why, "upstream request failed");
                g.egress.observe_call(name, &provider, None, Some(why.clone()));
                skipped.push(format!("{name}: upstream unreachable: {why}"));
                all_exhausted = false;
                continue;
            }
        };
        let status = upstream.status();
        if status.as_u16() == 429 {
            let rh = upstream.headers().clone();
            let rb = upstream.bytes().await.unwrap_or_default();
            let until = g.quotas.observe(&target, 429, &rh, &rb, &spec.learn).unwrap_or(now_ms() + 60_000);
            soonest = Some(soonest.map_or(until, |s: Ts| s.min(until)));
            skipped.push(format!("{name} ({}): provider says rate limited, resets in {}", target.scope_id, human_in(until - now_ms())));
            if i + 1 < names.len() {
                continue; // nothing has reached OpenCode yet: try the next route
            }
            if plan.on_exhausted == OnExhausted::Wait {
                g.park(until, &provider, &skipped);
            }
            let mut out = Response::builder().status(429);
            for (k, v) in &rh {
                if !HOP.contains(&k.as_str()) {
                    out = out.header(k, v);
                }
            }
            return out.body(Body::from(rb)).unwrap_or_else(|_| llm_error(StatusCode::TOO_MANY_REQUESTS, "rate limited"));
        }
        g.quotas.observe(&target, status.as_u16(), upstream.headers(), b"", &spec.learn);
        g.unpark();
        g.note_route(name, &names[0], &skipped, &target.scope_id);
        let mut out = Response::builder().status(status.as_u16());
        for (k, v) in upstream.headers() {
            if !HOP.contains(&k.as_str()) {
                out = out.header(k, v);
            }
        }
        let stream = upstream.bytes_stream().map(|r| r.map_err(std::io::Error::other));
        return out.body(Body::from_stream(stream)).unwrap_or_else(|_| llm_error(StatusCode::BAD_GATEWAY, "bad upstream response"));
    }

    // Nothing could serve the request.
    let detail = skipped.join("; ");
    match (all_exhausted, soonest, plan.on_exhausted) {
        (true, Some(until), OnExhausted::Wait) => {
            g.park(until, &provider, &skipped);
            let secs = ((until - now_ms()) / 1000).max(1);
            let mut r = llm_error(StatusCode::TOO_MANY_REQUESTS, &format!("every route's allowance for {provider} is used up; waiting {}: {detail}", human_in(secs * 1000)));
            r.headers_mut().insert("retry-after", HeaderValue::from(secs as u64));
            r
        }
        (true, Some(_), OnExhausted::Fail) => llm_error(StatusCode::FORBIDDEN, &format!("every route's allowance for {provider} is used up (on_exhausted: fail): {detail}")),
        _ => llm_error(StatusCode::BAD_GATEWAY, &format!("upstream unreachable: {detail}")),
    }
}

/// `3h 24m`, `5m 10s`, `42s`.
pub fn human_in(ms: i64) -> String {
    let s = (ms.max(0) + 999) / 1000;
    match s {
        s if s >= 86_400 => format!("{}d {}h", s / 86_400, s % 86_400 / 3600),
        s if s >= 3600 => format!("{}h {}m", s / 3600, s % 3600 / 60),
        s if s >= 60 => format!("{}m {}s", s / 60, s % 60),
        s => format!("{s}s"),
    }
}

impl Gateway {
    /// Attribute tokens used since the last request to the bucket that served
    /// it (called before each request and when a turn ends).
    pub fn settle_tokens(&self) {
        let Some((t, route, before)) = self.prev.lock().unwrap().take() else { return };
        let now_total = self.usage.lock().unwrap().total();
        let limits = self.cfg.get().providers.get(&t.provider).map(|p| quota::spec_for(p).limits).unwrap_or_default();
        self.quotas.record_tokens(&t, &route, &self.id, &limits, now_total.saturating_sub(before));
        // Later tokens of the same request (e.g. a resync) still belong there.
        *self.prev.lock().unwrap() = Some((t, route, now_total.max(before)));
    }

    fn park(&self, until: Ts, provider: &str, why: &[String]) {
        let msg = format!("waiting for the {provider} allowance on every route: {}", why.join("; "));
        let was = self.parked.lock().unwrap().replace((until, msg.clone()));
        if was.is_none() {
            tracing::info!(session = %self.id, provider, until, "parked: every route's allowance is used up");
            self.link.emit(NodeEvent::Activity { session: self.id.clone(), kind: "quota.parked".into(), detail: json!({"provider": provider, "until": until, "why": why}) });
        }
    }

    fn unpark(&self) {
        if self.parked.lock().unwrap().take().is_some() {
            self.link.emit(NodeEvent::Activity { session: self.id.clone(), kind: "quota.resumed".into(), detail: json!({}) });
        }
    }

    /// Report when the route serving this session changes (failover and back).
    fn note_route(&self, route: &str, preferred: &str, skipped: &[String], scope_id: &str) {
        let prev = self.last_route.lock().unwrap().replace(route.to_string());
        if prev.as_deref() != Some(route) && (prev.is_some() || route != preferred) {
            let kind = if route == preferred { "route.restored" } else { "route.failover" };
            tracing::info!(session = %self.id, route, from = ?prev, "{kind}");
            self.link.emit(NodeEvent::Activity {
                session: self.id.clone(),
                kind: kind.into(),
                detail: json!({"route": route, "from": prev, "scope_id": scope_id, "skipped": skipped}),
            });
        }
    }
}

// ---------------------------------------------------------------------- MCP

struct ToolDef {
    name: &'static str,
    desc: &'static str,
    schema: fn() -> Value,
    coordinator: bool,
}

fn obj(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": props, "required": required})
}

const TOOLS: &[ToolDef] = &[
    ToolDef {
        name: "send_message",
        desc: "Send a message to another session in the cluster. delivery: queue (after its current turn, default), steer (inject now), inbox (it reads it with read_inbox).",
        schema: || obj(json!({"to": {"type": "string", "description": "session id"}, "text": {"type": "string"}, "delivery": {"type": "string", "enum": ["queue", "steer", "inbox"]}}), &["to", "text"]),
        coordinator: false,
    },
    ToolDef { name: "read_inbox", desc: "Read and clear messages left for you by other sessions.", schema: || obj(json!({}), &[]), coordinator: false },
    ToolDef { name: "list_agents", desc: "List agent profiles available in the cluster (name, description, model, node).", schema: || obj(json!({}), &[]), coordinator: false },
    ToolDef {
        name: "list_nodes",
        desc: "List cluster machines: free slots, agents, link latency, egress IPs, provider allowances left (with reset times) and provider latency per route. Use it to choose where to start an agent (spawn_agent's node).",
        schema: || obj(json!({}), &[]),
        coordinator: false,
    },
    ToolDef { name: "list_sessions", desc: "List live sessions with status, todos progress and token usage.", schema: || obj(json!({}), &[]), coordinator: false },
    ToolDef {
        name: "session_status",
        desc: "Status, todos, token usage and last output of one session.",
        schema: || obj(json!({"session": {"type": "string"}}), &["session"]),
        coordinator: false,
    },
    ToolDef {
        name: "spawn_agent",
        desc: "Start a new agent instance from a template (see list_agents) with an initial prompt. Returns its session id. You are notified when it finishes a turn. share_workspace=true gives it your workspace. role_append / budget_usd / route customise this instance where the template's `overridable` allows it.",
        schema: || {
            obj(
                json!({
                    "agent": {"type": "string"},
                    "prompt": {"type": "string"},
                    "title": {"type": "string"},
                    "node": {"type": "string", "description": "pin to a node (see list_nodes); default: the supervisor chooses"},
                    "share_workspace": {"type": "boolean"},
                    "role_append": {"type": "string", "description": "extra standing instructions for this instance"},
                    "budget_usd": {"type": "number"},
                    "route": {"type": "string", "description": "preferred egress route"}
                }),
                &["agent", "prompt"],
            )
        },
        coordinator: true,
    },
    ToolDef { name: "stop_session", desc: "Stop a session you spawned.", schema: || obj(json!({"session": {"type": "string"}}), &["session"]), coordinator: true },
    ToolDef { name: "task_get", desc: "Get the task you are coordinating.", schema: || obj(json!({}), &[]), coordinator: true },
    ToolDef { name: "task_note", desc: "Append a progress note to your task (visible to the operator).", schema: || obj(json!({"note": {"type": "string"}}), &["note"]), coordinator: true },
    ToolDef {
        name: "task_complete",
        desc: "Mark your task done with a summary of the outcome. Stops the sessions you spawned.",
        schema: || obj(json!({"summary": {"type": "string"}}), &["summary"]),
        coordinator: true,
    },
    ToolDef { name: "task_fail", desc: "Give up on your task with a reason.", schema: || obj(json!({"reason": {"type": "string"}}), &["reason"]), coordinator: true },
];

/// Cluster tools a session with this role and allowlist can call (for the preamble).
pub fn tool_names(role: SessionRole, allow: Option<&[String]>) -> Vec<&'static str> {
    TOOLS
        .iter()
        .filter(|t| !t.coordinator || role == SessionRole::Coordinator)
        .filter(|t| allow.is_none_or(|l| l.iter().any(|n| n == t.name)))
        .map(|t| t.name)
        .collect()
}

impl Gateway {
    fn tool_allowed(&self, t: &ToolDef) -> bool {
        if t.coordinator && self.role != SessionRole::Coordinator {
            return false;
        }
        let snap = self.cfg.get();
        match snap.agents.get(&self.agent).and_then(|a| a.cfg.tools.cluster.clone()) {
            Some(list) => list.iter().any(|n| n == t.name),
            None => true,
        }
    }
}

fn to_call(name: &str, a: &Value) -> Result<AgentCall, String> {
    let s = |k: &str| a.get(k).and_then(|v| v.as_str()).map(String::from).ok_or_else(|| format!("missing string argument {k:?}"));
    Ok(match name {
        "send_message" => AgentCall::Send {
            to: s("to")?,
            text: s("text")?,
            delivery: a.get("delivery").and_then(|d| serde_json::from_value(d.clone()).ok()).unwrap_or_default(),
        },
        "read_inbox" => AgentCall::Inbox,
        "list_agents" => AgentCall::ListAgents,
        "list_nodes" => AgentCall::ListNodes,
        "list_sessions" => AgentCall::ListSessions,
        "session_status" => AgentCall::SessionStatus { session: s("session")? },
        "spawn_agent" => AgentCall::Spawn {
            agent: s("agent")?,
            prompt: s("prompt")?,
            node: s("node").ok(),
            title: s("title").ok(),
            share_workspace: a.get("share_workspace").and_then(|v| v.as_bool()).unwrap_or(false),
            overrides: SpawnOverrides { role_append: s("role_append").ok(), budget_usd: a.get("budget_usd").and_then(|v| v.as_f64()), route: s("route").ok() },
        },
        "stop_session" => AgentCall::Stop { session: s("session")? },
        "task_get" => AgentCall::TaskGet,
        "task_note" => AgentCall::TaskNote { note: s("note")? },
        "task_complete" => AgentCall::TaskComplete { summary: s("summary")? },
        "task_fail" => AgentCall::TaskFail { reason: s("reason")? },
        _ => return Err(format!("unknown tool {name}")),
    })
}

async fn mcp(State(g): State<Gateway>, headers: HeaderMap, Json(msg): Json<Value>) -> Response {
    if !g.check_cap(&headers, None) {
        return (StatusCode::UNAUTHORIZED, "invalid capability token").into_response();
    }
    // Batches are allowed by older spec revisions; handle both.
    if let Some(batch) = msg.as_array() {
        let mut out = vec![];
        for m in batch {
            if let Some(r) = handle_rpc(&g, m).await {
                out.push(r);
            }
        }
        return if out.is_empty() { StatusCode::ACCEPTED.into_response() } else { Json(Value::Array(out)).into_response() };
    }
    match handle_rpc(&g, &msg).await {
        Some(r) => Json(r).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

async fn handle_rpc(g: &Gateway, m: &Value) -> Option<Value> {
    let id = m.get("id").cloned();
    let method = m.get("method").and_then(|v| v.as_str()).unwrap_or("");
    let id = id?; // notifications get no response
    let ok = |result: Value| json!({"jsonrpc": "2.0", "id": id.clone(), "result": result});
    let err = |code: i64, message: String| json!({"jsonrpc": "2.0", "id": id.clone(), "error": {"code": code, "message": message}});
    Some(match method {
        "initialize" => {
            let pv = m["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18").to_string();
            ok(json!({
                "protocolVersion": pv,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "opencode-cluster", "version": ocl_protocol::VERSION},
                "instructions": "Cluster tools for coordinating with other OpenCode agents. Your session id is available via list_sessions."
            }))
        }
        "ping" => ok(json!({})),
        "tools/list" => {
            let tools: Vec<Value> = TOOLS
                .iter()
                .filter(|t| g.tool_allowed(t))
                .map(|t| json!({"name": t.name, "description": t.desc, "inputSchema": (t.schema)()}))
                .collect();
            ok(json!({"tools": tools}))
        }
        "tools/call" => {
            let name = m["params"]["name"].as_str().unwrap_or("");
            let args = m["params"].get("arguments").cloned().unwrap_or(json!({}));
            let Some(def) = TOOLS.iter().find(|t| t.name == name) else {
                return Some(err(-32602, format!("unknown tool {name}")));
            };
            if !g.tool_allowed(def) {
                return Some(ok(json!({"content": [{"type": "text", "text": format!("tool {name} is not permitted for this session")}], "isError": true})));
            }
            let call = match to_call(name, &args) {
                Ok(c) => c,
                Err(e) => return Some(ok(json!({"content": [{"type": "text", "text": e}], "isError": true}))),
            };
            match g.link.request(&g.id, call).await {
                Ok(v) => {
                    let text = if let Some(s) = v.as_str() { s.to_string() } else { serde_json::to_string_pretty(&v).unwrap_or_default() };
                    ok(json!({"content": [{"type": "text", "text": text}], "isError": false}))
                }
                Err(e) => ok(json!({"content": [{"type": "text", "text": format!("cluster error: {e}")}], "isError": true})),
            }
        }
        _ => err(-32601, format!("method not found: {method}")),
    })
}
