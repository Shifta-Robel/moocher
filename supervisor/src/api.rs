//! Operator API: `/api/v1/*` (REST/JSON + SSE) and `/metrics`.
//!
//! Every request except `/healthz` needs `Authorization: Bearer <token>`.
//! Tokens are stored hashed with a scope: `read` < `operate` < `admin`.
//! Every mutating call is written to the hash-chained audit log by the
//! supervisor methods it invokes.

use crate::db::Scope;
use crate::state::{Actor, SpawnOpts, Sup};
use axum::extract::{Path, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use ocl_protocol::api::*;
use ocl_protocol::model::*;
use serde::Deserialize;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::sync::Arc;
use tokio_stream::StreamExt;

type S = State<Arc<Sup>>;

#[derive(Clone)]
pub struct Caller {
    pub name: String,
    pub scope: Scope,
}

impl Caller {
    fn actor(&self) -> Actor {
        Actor::Operator(self.name.clone())
    }
    fn need(&self, s: Scope) -> Result<(), ApiError> {
        if self.scope >= s {
            Ok(())
        } else {
            Err(ApiError(StatusCode::FORBIDDEN, format!("token scope {} insufficient (needs {})", self.scope.as_str(), s.as_str())))
        }
    }
}

pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        let msg = format!("{e:#}");
        let code = if msg.starts_with("no such") || msg.starts_with("no task") || msg.starts_with("no run") || msg.starts_with("no workflow") {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::BAD_REQUEST
        };
        ApiError(code, msg)
    }
}

type R<T> = Result<Json<T>, ApiError>;

async fn auth(State(sup): S, mut req: Request, next: Next) -> Result<Response, ApiError> {
    let token = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        // EventSource cannot set headers; allow ?token= for the SSE stream only.
        .or_else(|| {
            if req.uri().path().ends_with("/stream") {
                req.uri().query().and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("token=")))
            } else {
                None
            }
        })
        .map(String::from);
    let Some(token) = token else {
        return Err(ApiError(StatusCode::UNAUTHORIZED, "missing bearer token".into()));
    };
    let Some((name, scope)) = sup.db.token_lookup(&token) else {
        tracing::warn!(path = %req.uri().path(), "rejected invalid API token");
        return Err(ApiError(StatusCode::UNAUTHORIZED, "invalid token".into()));
    };
    req.extensions_mut().insert(Caller { name, scope });
    Ok(next.run(req).await)
}

pub fn router(sup: Arc<Sup>) -> Router {
    let api = Router::new()
        .route("/overview", get(overview))
        .route("/nodes", get(nodes))
        .route("/nodes/invite", post(invite_node))
        .route("/nodes/{name}", axum::routing::delete(forget_node))
        .route("/nodes/{name}/approve", post(approve_node))
        .route("/nodes/{name}/revoke", post(revoke_node))
        .route("/allowed-sources", get(allowed_sources).post(add_allowed_source))
        .route("/allowed-sources/remove", post(remove_allowed_source))
        .route("/agents", get(agents))
        .route("/sessions", get(sessions).post(spawn))
        .route("/sessions/{id}", get(session).delete(stop))
        .route("/sessions/{id}/message", post(message))
        .route("/sessions/{id}/interrupt", post(interrupt))
        .route("/sessions/{id}/permissions/{req}", post(permission))
        .route("/tasks", get(tasks).post(create_task))
        .route("/tasks/reorder", post(reorder))
        .route("/tasks/{id}", get(task).patch(patch_task).delete(delete_task))
        .route("/tasks/{id}/run", post(run_task))
        .route("/tasks/{id}/cancel", post(cancel_task))
        .route("/workflows", get(workflows))
        .route("/workflows/{name}/run", post(run_workflow))
        .route("/runs", get(runs))
        .route("/runs/{id}", get(run))
        .route("/runs/{id}/cancel", post(cancel_run))
        .route("/events", get(events))
        .route("/stream", get(stream))
        .route("/audit", get(audit))
        .route("/audit/verify", get(audit_verify))
        .route("/tokens", get(list_tokens).post(create_token))
        .route("/tokens/{name}", axum::routing::delete(revoke_token));
    Router::new()
        .nest("/api/v1", api)
        .route("/metrics", get(metrics))
        .layer(axum::middleware::from_fn_with_state(sup.clone(), auth))
        .route("/healthz", get(|| async { "ok" }))
        .route("/", get(|| async { "opencode-cluster supervisor. API at /api/v1 (bearer token required). Web UI arrives in Phase 4." }))
        .with_state(sup)
}

async fn overview(State(sup): S) -> Json<Overview> {
    Json(sup.overview())
}

async fn nodes(State(sup): S) -> Json<Vec<NodeInfo>> {
    Json(sup.node_views())
}

async fn invite_node(State(sup): S, Extension(c): Extension<Caller>, Json(r): Json<InviteRequest>) -> R<Value> {
    c.need(Scope::Admin)?;
    Ok(Json(sup.invite(&c.actor(), &r.name, r.role, serde_json::to_value(&r.labels).unwrap_or_default())?))
}

async fn approve_node(State(sup): S, Extension(c): Extension<Caller>, Path(name): Path<String>) -> R<Value> {
    c.need(Scope::Admin)?;
    sup.approve_node(&c.actor(), &name)?;
    Ok(Json(json!({"ok": true, "node": name, "enrollment": "approved"})))
}

async fn revoke_node(State(sup): S, Extension(c): Extension<Caller>, Path(name): Path<String>) -> R<Value> {
    c.need(Scope::Admin)?;
    let n = sup.revoke_node(&c.actor(), &name)?;
    Ok(Json(json!({"ok": true, "node": name, "certificates_revoked": n})))
}

async fn forget_node(State(sup): S, Extension(c): Extension<Caller>, Path(name): Path<String>) -> R<Value> {
    c.need(Scope::Admin)?;
    sup.forget_node(&c.actor(), &name)?;
    Ok(Json(json!({"ok": true})))
}

async fn allowed_sources(State(sup): S, Extension(c): Extension<Caller>) -> R<Value> {
    c.need(Scope::Admin)?;
    Ok(Json(sup.allowlist()))
}

async fn add_allowed_source(State(sup): S, Extension(c): Extension<Caller>, Json(r): Json<AllowSourceRequest>) -> R<Value> {
    c.need(Scope::Admin)?;
    let cidr = sup.allowlist_add(&c.actor(), &r.source, &r.note)?;
    Ok(Json(json!({"ok": true, "cidr": cidr})))
}

async fn remove_allowed_source(State(sup): S, Extension(c): Extension<Caller>, Json(r): Json<AllowSourceRequest>) -> R<Value> {
    c.need(Scope::Admin)?;
    let dropped = sup.allowlist_remove(&c.actor(), &r.source)?;
    Ok(Json(json!({"ok": true, "disconnected": dropped})))
}

async fn agents(State(sup): S) -> Json<Value> {
    let mut out = vec![];
    for n in sup.db.nodes() {
        for a in n.agents {
            out.push(json!({"node": n.name, "connected": n.connected, "agent": a}));
        }
    }
    Json(Value::Array(out))
}

#[derive(Deserialize)]
struct SessQ {
    #[serde(default)]
    all: bool,
}

async fn sessions(State(sup): S, Query(q): Query<SessQ>) -> Json<Vec<SessionInfo>> {
    Json(sup.db.sessions(q.all))
}

async fn session(State(sup): S, Path(id): Path<String>) -> R<SessionInfo> {
    Ok(Json(sup.session(&id)?))
}

async fn spawn(State(sup): S, Extension(c): Extension<Caller>, Json(r): Json<SpawnRequest>) -> R<SessionInfo> {
    c.need(Scope::Operate)?;
    let s = sup
        .spawn(&c.actor(), SpawnOpts { agent: r.agent, prompt: r.prompt, node: r.node, title: r.title, workspace: r.workspace, ..Default::default() })
        .await?;
    Ok(Json(s))
}

#[derive(Deserialize)]
struct StopQ {
    #[serde(default)]
    remove_workspace: bool,
}

async fn stop(State(sup): S, Extension(c): Extension<Caller>, Path(id): Path<String>, Query(q): Query<StopQ>) -> R<Value> {
    c.need(Scope::Operate)?;
    sup.stop(&c.actor(), &id, q.remove_workspace).await?;
    Ok(Json(json!({"ok": true})))
}

async fn message(State(sup): S, Extension(c): Extension<Caller>, Path(id): Path<String>, Json(r): Json<MessageRequest>) -> R<Value> {
    c.need(Scope::Operate)?;
    let text = format!("[message from operator {}]\n{}", c.name, r.text);
    sup.message(&c.actor(), &id, &text, r.delivery).await?;
    Ok(Json(json!({"ok": true})))
}

async fn interrupt(State(sup): S, Extension(c): Extension<Caller>, Path(id): Path<String>) -> R<Value> {
    c.need(Scope::Operate)?;
    sup.interrupt(&c.actor(), &id).await?;
    Ok(Json(json!({"ok": true})))
}

async fn permission(State(sup): S, Extension(c): Extension<Caller>, Path((id, req)): Path<(String, String)>, Json(r): Json<PermissionReplyRequest>) -> R<Value> {
    c.need(Scope::Operate)?;
    sup.permission_reply(&c.actor(), &id, &req, &r.reply).await?;
    Ok(Json(json!({"ok": true})))
}

async fn tasks(State(sup): S) -> Json<Vec<Task>> {
    Json(sup.db.tasks())
}

async fn task(State(sup): S, Path(id): Path<i64>) -> R<Task> {
    sup.db.task(id).map(Json).ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("no task {id}")))
}

async fn create_task(State(sup): S, Extension(c): Extension<Caller>, Json(r): Json<TaskCreate>) -> R<Task> {
    c.need(Scope::Operate)?;
    Ok(Json(sup.create_task(&c.actor(), r)?))
}

async fn patch_task(State(sup): S, Extension(c): Extension<Caller>, Path(id): Path<i64>, Json(r): Json<TaskPatch>) -> R<Task> {
    c.need(Scope::Operate)?;
    Ok(Json(sup.patch_task(&c.actor(), id, r)?))
}

async fn delete_task(State(sup): S, Extension(c): Extension<Caller>, Path(id): Path<i64>) -> R<Value> {
    c.need(Scope::Operate)?;
    sup.delete_task(&c.actor(), id).await?;
    Ok(Json(json!({"ok": true})))
}

async fn reorder(State(sup): S, Extension(c): Extension<Caller>, Json(r): Json<ReorderRequest>) -> R<Vec<Task>> {
    c.need(Scope::Operate)?;
    Ok(Json(sup.reorder_tasks(&c.actor(), &r.ids)?))
}

async fn run_task(State(sup): S, Extension(c): Extension<Caller>, Path(id): Path<i64>) -> R<Task> {
    c.need(Scope::Operate)?;
    Ok(Json(sup.run_task(&c.actor(), id).await?))
}

async fn cancel_task(State(sup): S, Extension(c): Extension<Caller>, Path(id): Path<i64>) -> R<Task> {
    c.need(Scope::Operate)?;
    Ok(Json(sup.cancel_task(&c.actor(), id).await?))
}

async fn workflows(State(sup): S) -> Json<Value> {
    Json(serde_json::to_value(sup.workflows()).unwrap_or_default())
}

async fn run_workflow(State(sup): S, Extension(c): Extension<Caller>, Path(name): Path<String>, Json(r): Json<RunWorkflowRequest>) -> R<WorkflowRun> {
    c.need(Scope::Operate)?;
    let run = {
        let _g = sup.orch_lock.lock().await;
        sup.start_run(&c.actor(), &name, &r.input, None).await?
    };
    Ok(Json(run))
}

async fn runs(State(sup): S) -> Json<Vec<WorkflowRun>> {
    Json(sup.db.runs())
}

async fn run(State(sup): S, Path(id): Path<String>) -> R<WorkflowRun> {
    sup.db.run(&id).map(Json).ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("no run {id}")))
}

async fn cancel_run(State(sup): S, Extension(c): Extension<Caller>, Path(id): Path<String>) -> R<WorkflowRun> {
    c.need(Scope::Operate)?;
    Ok(Json(sup.cancel_run(&c.actor(), &id).await?))
}

#[derive(Deserialize)]
struct EvQ {
    #[serde(default)]
    after: i64,
    #[serde(default)]
    session: Option<String>,
    #[serde(default = "default_limit")]
    limit: i64,
}
fn default_limit() -> i64 {
    200
}

async fn events(State(sup): S, Query(q): Query<EvQ>) -> Json<Vec<LogEvent>> {
    Json(sup.db.events(q.after, q.session.as_deref(), q.limit.clamp(1, 5000)))
}

async fn stream(State(sup): S) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = tokio_stream::wrappers::BroadcastStream::new(sup.stream.subscribe());
    let s = rx.filter_map(|r| r.ok()).map(|ev| Ok(Event::default().data(serde_json::to_string(&ev).unwrap_or_default())));
    Sse::new(s).keep_alive(KeepAlive::default())
}

#[derive(Deserialize)]
struct AuditQ {
    #[serde(default = "default_limit")]
    limit: i64,
}

async fn audit(State(sup): S, Extension(c): Extension<Caller>, Query(q): Query<AuditQ>) -> R<Value> {
    c.need(Scope::Admin)?;
    Ok(Json(serde_json::to_value(sup.db.audit_list(q.limit.clamp(1, 5000))).unwrap_or_default()))
}

async fn audit_verify(State(sup): S, Extension(c): Extension<Caller>) -> R<Value> {
    c.need(Scope::Admin)?;
    match sup.db.audit_verify() {
        Ok(n) => Ok(Json(json!({"ok": true, "entries": n}))),
        Err(e) => Ok(Json(json!({"ok": false, "error": e.to_string()}))),
    }
}

async fn list_tokens(State(sup): S, Extension(c): Extension<Caller>) -> R<Value> {
    c.need(Scope::Admin)?;
    let v: Vec<Value> = sup.db.tokens()?.into_iter().map(|(n, s, t)| json!({"name": n, "scope": s, "created_at": t})).collect();
    Ok(Json(Value::Array(v)))
}

#[derive(Deserialize)]
struct NewToken {
    name: String,
    scope: String,
}

async fn create_token(State(sup): S, Extension(c): Extension<Caller>, Json(r): Json<NewToken>) -> R<Value> {
    c.need(Scope::Admin)?;
    let scope = Scope::parse(&r.scope).ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, "scope must be read, operate or admin".into()))?;
    let res = sup.db.create_token(&r.name, scope);
    sup.audit(&c.actor(), "token.create", &r.name, json!({"scope": r.scope}), res.is_ok());
    let token = res?;
    Ok(Json(json!({"name": r.name, "scope": r.scope, "token": token, "note": "shown once; store it securely"})))
}

async fn revoke_token(State(sup): S, Extension(c): Extension<Caller>, Path(name): Path<String>) -> R<Value> {
    c.need(Scope::Admin)?;
    let ok = sup.db.revoke_token(&name)?;
    sup.audit(&c.actor(), "token.revoke", &name, json!({}), ok);
    Ok(Json(json!({"ok": ok})))
}

/// Prometheus text exposition.
async fn metrics(State(sup): S) -> impl IntoResponse {
    use std::collections::BTreeMap;
    use std::fmt::Write;
    let mut out = String::new();
    let nodes = sup.db.nodes();
    let _ = writeln!(out, "# TYPE ocl_nodes_connected gauge\nocl_nodes_connected {}", nodes.iter().filter(|n| n.connected).count());
    let sessions = sup.db.sessions(true);
    let mut by_state: BTreeMap<&str, u64> = BTreeMap::new();
    let mut tokens: BTreeMap<(String, &str), u64> = BTreeMap::new();
    let mut cost: BTreeMap<String, f64> = BTreeMap::new();
    for s in &sessions {
        *by_state.entry(s.report.state.as_str()).or_default() += 1;
        let a = s.report.agent.clone();
        for (k, v) in [("input", s.report.tokens.input), ("output", s.report.tokens.output), ("reasoning", s.report.tokens.reasoning), ("cache_read", s.report.tokens.cache_read)] {
            *tokens.entry((a.clone(), k)).or_default() += v;
        }
        *cost.entry(a).or_default() += s.report.tokens.cost_usd;
    }
    let _ = writeln!(out, "# TYPE ocl_sessions gauge");
    for (k, v) in by_state {
        let _ = writeln!(out, "ocl_sessions{{state=\"{k}\"}} {v}");
    }
    let _ = writeln!(out, "# TYPE ocl_tokens_total counter");
    for ((a, k), v) in tokens {
        let _ = writeln!(out, "ocl_tokens_total{{agent=\"{a}\",kind=\"{k}\"}} {v}");
    }
    let _ = writeln!(out, "# TYPE ocl_cost_usd_total counter");
    for (a, v) in cost {
        let _ = writeln!(out, "ocl_cost_usd_total{{agent=\"{a}\"}} {v}");
    }
    let relays = sup.nodes.relay_stats.lock().unwrap().clone();
    let _ = writeln!(out, "# TYPE ocl_relay_tunnels_total counter");
    for (r, (n, _, _)) in &relays {
        let _ = writeln!(out, "ocl_relay_tunnels_total{{relay=\"{r}\"}} {n}");
    }
    let _ = writeln!(out, "# TYPE ocl_relay_bytes_total counter");
    for (r, (_, up, down)) in &relays {
        let _ = writeln!(out, "ocl_relay_bytes_total{{relay=\"{r}\",direction=\"out\"}} {up}");
        let _ = writeln!(out, "ocl_relay_bytes_total{{relay=\"{r}\",direction=\"in\"}} {down}");
    }
    let mut ts: BTreeMap<&str, u64> = BTreeMap::new();
    for t in sup.db.tasks() {
        *ts.entry(t.status.as_str()).or_default() += 1;
    }
    let _ = writeln!(out, "# TYPE ocl_tasks gauge");
    for (k, v) in ts {
        let _ = writeln!(out, "ocl_tasks{{status=\"{k}\"}} {v}");
    }
    ([("content-type", "text/plain; version=0.0.4")], out)
}
