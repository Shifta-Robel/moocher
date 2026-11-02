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
use moocher_protocol::api::*;
use moocher_protocol::model::*;
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

fn cookie(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(axum::http::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|kv| kv.trim().strip_prefix(&format!("{name}=")).map(String::from))
}

/// Bearer API token (CLI, scripts), or a browser session cookie. Cookie
/// requests that change anything must echo the session's CSRF token in
/// `X-CSRF-Token` (the UI reads it from the `moo_csrf` cookie).
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
    if let Some(token) = token {
        let Some((name, scope)) = sup.db.token_lookup(&token) else {
            tracing::warn!(path = %req.uri().path(), "rejected invalid API token");
            return Err(ApiError(StatusCode::UNAUTHORIZED, "invalid token".into()));
        };
        req.extensions_mut().insert(Caller { name, scope });
        return Ok(next.run(req).await);
    }
    let Some(ws) = cookie(req.headers(), "moo_session").and_then(|c| sup.web_session(&c)) else {
        return Err(ApiError(StatusCode::UNAUTHORIZED, "log in, or send an API token (Authorization: Bearer …)".into()));
    };
    let safe = matches!(*req.method(), axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS);
    if !safe {
        let sent = req.headers().get("x-csrf-token").and_then(|v| v.to_str().ok()).unwrap_or("");
        if sent.len() != ws.csrf.len() || !sent.bytes().zip(ws.csrf.bytes()).fold(true, |a, (x, y)| a & (x == y)) {
            return Err(ApiError(StatusCode::FORBIDDEN, "missing or wrong CSRF token (X-CSRF-Token)".into()));
        }
    }
    req.extensions_mut().insert(Caller { name: ws.user, scope: ws.scope });
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
        .route("/nodes/{name}/routes", post(route_override))
        .route("/nodes/{name}/probe", post(probe_egress))
        .route("/quotas", get(quotas))
        .route("/pool", get(pool))
        .route("/providers/stats", get(provider_stats))
        .route("/providers/events", get(provider_events))
        .route("/quotas/override", post(quota_override))
        .route("/egress", get(egress))
        .route("/allowed-sources", get(allowed_sources).post(add_allowed_source))
        .route("/allowed-sources/remove", post(remove_allowed_source))
        .route("/agents", get(agents))
        .route("/agents/availability", get(availability))
        .route("/spawn-queue", get(spawn_queue))
        .route("/sessions", get(sessions).post(spawn))
        .route("/sessions/{id}", get(session).delete(stop))
        .route("/sessions/{id}/message", post(message))
        .route("/sessions/{id}/preamble", get(preamble))
        .route("/sessions/{id}/stats", get(session_stats))
        .route("/stats", get(stats))
        .route("/templates", get(templates))
        .route("/sessions/{id}/interrupt", post(interrupt))
        .route("/sessions/{id}/permissions/{req}", post(permission))
        .route("/tasks", get(tasks).post(create_task))
        .route("/tasks/reorder", post(reorder))
        .route("/tasks/{id}", get(task).patch(patch_task).delete(delete_task))
        .route("/tasks/{id}/run", post(run_task))
        .route("/tasks/{id}/cancel", post(cancel_task))
        .route("/tasks/{id}/pause", post(pause_task))
        .route("/tasks/{id}/resume", post(resume_task))
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
        .route("/housekeeping", post(housekeeping))
        .route("/tokens/{name}", axum::routing::delete(revoke_token))
        .route("/questions", get(questions))
        .route("/questions/mode", get(question_mode).post(set_question_mode))
        .route("/questions/{id}", get(question))
        .route("/questions/{id}/answer", post(answer_question))
        .route("/assumptions", get(assumptions))
        .route("/artifacts", get(artifacts).post(upload_artifact).layer(axum::extract::DefaultBodyLimit::disable()))
        .route("/artifacts/{reference}", get(artifact))
        .route("/artifacts/{reference}/blob", get(artifact_blob))
        .route("/halts", get(halts))
        .route("/halts/{id}/recover", post(recover))
        .route("/sessions/{id}/recover", post(recover))
        .route("/sessions/{id}/checkpoint", get(checkpoint))
        .route("/harvests", get(harvests))
        .route("/notifications", get(notifications))
        .route("/notifications/read", post(notifications_read))
        .route("/notifications/test", post(notifications_test))
        .route("/sessions/{id}/attach", post(attach))
        .route("/push/key", get(push_key))
        .route("/push/subscribe", post(push_subscribe))
        .route("/push/unsubscribe", post(push_unsubscribe))
        .route("/users", get(users).post(add_user))
        .route("/users/{name}", axum::routing::delete(delete_user))
        .route("/nodes/{name}/routes/promote", post(promote_route))
        .route("/manager", get(manager_status))
        .route("/manager/ask", post(manager_ask))
        .route("/info", get(info));
    Router::new()
        .nest("/api/v1", api)
        .route("/metrics", get(metrics))
        .layer(axum::middleware::from_fn_with_state(sup.clone(), auth))
        .route("/healthz", get(|| async { "ok" }))
        .route("/auth/login", post(login))
        .route("/auth/logout", post(logout))
        .route("/auth/me", get(me))
        .route("/auth/password", post(change_password))
        .fallback(ui)
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

async fn preamble(State(sup): S, Path(id): Path<String>) -> R<Value> {
    Ok(Json(json!({"session": id, "preamble": sup.preamble(&id).await?})))
}

async fn stats(State(sup): S) -> Json<Value> {
    Json(sup.stats_view())
}

async fn session_stats(State(sup): S, Path(id): Path<String>) -> R<Value> {
    sup.session(&id)?;
    Ok(Json(sup.session_stats(&id)))
}

async fn templates(State(sup): S) -> Json<Value> {
    Json(sup.templates_view())
}

async fn pool(State(sup): S) -> Json<PoolView> {
    Json(sup.pool_view())
}

async fn quotas(State(sup): S) -> Json<Vec<QuotaView>> {
    Json(sup.quota_views())
}

#[derive(Deserialize)]
struct ProvQ {
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    hours: Option<i64>,
    #[serde(default)]
    limit: Option<i64>,
}

async fn provider_stats(State(sup): S, Query(q): Query<ProvQ>) -> Json<Value> {
    Json(sup.provider_stats(q.hours.unwrap_or(24).clamp(1, 24 * 90), q.provider.as_deref()))
}

async fn provider_events(State(sup): S, Query(q): Query<ProvQ>) -> Json<Vec<ProviderEvent>> {
    Json(sup.db.provider_events(q.provider.as_deref(), q.kind.as_deref(), q.limit.unwrap_or(100).clamp(1, 2000)))
}

async fn availability(State(sup): S) -> Json<Vec<Value>> {
    Json(sup.availability())
}

async fn spawn_queue(State(sup): S) -> Json<Vec<crate::spawnq::QueuedSpawn>> {
    Json(sup.queued_spawns())
}

async fn egress(State(sup): S) -> Json<EgressView> {
    Json(sup.egress_view())
}

async fn quota_override(State(sup): S, Extension(c): Extension<Caller>, Json(r): Json<QuotaOverrideRequest>) -> R<Value> {
    c.need(Scope::Operate)?;
    Ok(Json(sup.set_quota_override(&c.actor(), r).await?))
}

async fn route_override(State(sup): S, Extension(c): Extension<Caller>, Path(name): Path<String>, Json(r): Json<RouteOverrideRequest>) -> R<Value> {
    c.need(Scope::Operate)?;
    sup.set_route_override(&c.actor(), &name, r).await?;
    Ok(Json(json!({"ok": true})))
}

async fn probe_egress(State(sup): S, Extension(c): Extension<Caller>, Path(name): Path<String>) -> R<Value> {
    c.need(Scope::Operate)?;
    Ok(Json(sup.probe_egress(&c.actor(), &name).await?))
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
        .spawn(&c.actor(), SpawnOpts { agent: r.agent, prompt: r.prompt, node: r.node, title: r.title, workspace: r.workspace, overrides: r.overrides, role: r.role, urgency: r.urgency, ..Default::default() })
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

#[derive(Deserialize, Default)]
struct PauseReq {
    #[serde(default)]
    now: bool,
}

async fn pause_task(State(sup): S, Extension(c): Extension<Caller>, Path(id): Path<i64>, Json(r): Json<PauseReq>) -> R<Task> {
    c.need(Scope::Operate)?;
    Ok(Json(sup.pause_task(&c.actor(), id, r.now).await?))
}

async fn resume_task(State(sup): S, Extension(c): Extension<Caller>, Path(id): Path<i64>) -> R<Task> {
    c.need(Scope::Operate)?;
    Ok(Json(sup.resume_task(&c.actor(), id).await?))
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

#[derive(Deserialize, Default)]
struct HousekeepingReq {
    /// Run as if this much later (`1d`): what tomorrow's run would do.
    #[serde(default)]
    ahead: Option<String>,
    #[serde(default)]
    dry_run: bool,
}

async fn housekeeping(State(sup): S, Extension(c): Extension<Caller>, Json(r): Json<HousekeepingReq>) -> R<Value> {
    c.need(Scope::Admin)?;
    let ahead = match r.ahead.as_deref() {
        Some(a) => moocher_protocol::config::parse_duration_ms(a).ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, format!("ahead {a:?} is not a duration")))?,
        None => 0,
    };
    let actor = c.actor();
    let rep = tokio::task::spawn_blocking(move || sup.housekeeping(now_ms() + ahead, r.dry_run, &actor)).await.map_err(|e| anyhow::anyhow!("{e}"))??;
    Ok(Json(serde_json::to_value(rep).unwrap_or_default()))
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

#[derive(Deserialize)]
struct QuestionsQ {
    #[serde(default)]
    all: bool,
    /// Only those waiting on the operator.
    #[serde(default)]
    mine: bool,
}

async fn questions(State(sup): S, Query(q): Query<QuestionsQ>) -> Json<Vec<Question>> {
    Json(sup.questions_view(!q.all, q.mine))
}

async fn question(State(sup): S, Path(id): Path<String>) -> R<Question> {
    sup.questions_view(false, false).into_iter().find(|q| q.id == id).map(Json).ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("no such question {id}")))
}

async fn answer_question(State(sup): S, Extension(c): Extension<Caller>, Path(id): Path<String>, Json(r): Json<AnswerRequest>) -> R<Question> {
    c.need(Scope::Operate)?;
    Ok(Json(sup.answer(&c.actor(), &id, r).await?))
}

async fn question_mode(State(sup): S) -> Json<Value> {
    let tasks: Vec<Value> = sup.db.tasks().into_iter().filter_map(|t| t.question_mode.map(|m| json!({"task": t.id, "mode": m}))).collect();
    Json(json!({"mode": sup.cluster_question_mode(), "tasks": tasks}))
}

async fn set_question_mode(State(sup): S, Extension(c): Extension<Caller>, Json(r): Json<ModeRequest>) -> R<Value> {
    c.need(Scope::Operate)?;
    sup.set_question_mode(&c.actor(), r.mode, r.task)?;
    Ok(Json(json!({"ok": true, "mode": r.mode, "task": r.task})))
}

#[derive(Deserialize)]
struct TaskQ {
    #[serde(default)]
    task: Option<i64>,
}

async fn assumptions(State(sup): S, Query(q): Query<TaskQ>) -> Json<Vec<Assumption>> {
    Json(sup.db.assumptions(q.task))
}

#[derive(Deserialize)]
struct ArtQ {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    task: Option<i64>,
    #[serde(default = "default_limit")]
    limit: i64,
}

async fn artifacts(State(sup): S, Query(q): Query<ArtQ>) -> Json<Vec<ArtifactMeta>> {
    Json(sup.db.artifacts(q.q.as_deref(), q.task, q.limit.clamp(1, 5000)))
}

async fn artifact(State(sup): S, Path(r): Path<String>) -> R<Value> {
    Ok(Json(sup.artifact_view(&r)?))
}

#[derive(Deserialize, Default)]
struct UploadQ {
    #[serde(default)]
    name: String,
    #[serde(default)]
    media_type: String,
    /// `file` or `tree` (tar.zst made by `moo artifact put <dir>`).
    #[serde(default)]
    kind: String,
    /// Comma-separated.
    #[serde(default)]
    tags: String,
    #[serde(default)]
    parents: String,
}

async fn upload_artifact(State(sup): S, Extension(c): Extension<Caller>, Query(q): Query<UploadQ>, body: axum::body::Body) -> R<ArtifactMeta> {
    c.need(Scope::Operate)?;
    let stream = tokio_stream::StreamExt::map(body.into_data_stream(), |r: Result<axum::body::Bytes, axum::Error>| r.map(|b| b.to_vec()));
    let (hash, size) = sup.blobs.write(Box::pin(stream), sup.max_artifact()).await?;
    let split = |s: &str| s.split(',').map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect::<Vec<_>>();
    let h = crate::artifacts::PutHeader { name: q.name, media_type: q.media_type, kind: q.kind, tags: split(&q.tags), parents: split(&q.parents), ..Default::default() };
    let meta = sup.register_artifact(&hash, size, &h, None)?;
    sup.audit(&c.actor(), "artifact.put", &meta.reference, json!({"name": meta.name, "size": size}), true);
    Ok(Json(meta))
}

async fn artifact_blob(State(sup): S, Path(r): Path<String>) -> Result<Response, ApiError> {
    let meta = sup.artifact_meta(&r)?;
    let f = tokio::fs::File::open(sup.blobs.path(&meta.hash)).await.map_err(|e| ApiError(StatusCode::NOT_FOUND, format!("blob missing: {e}")))?;
    let body = axum::body::Body::from_stream(tokio_util_stream(f));
    Ok((
        [
            ("content-type", meta.media_type.clone()),
            ("x-moo-kind", meta.kind.clone()),
            ("x-moo-name", meta.name.chars().filter(|c| c.is_ascii_graphic() || *c == ' ').collect::<String>()),
            ("content-length", meta.size.to_string()),
        ],
        body,
    )
        .into_response())
}

/// File -> byte stream without pulling in tokio-util.
fn tokio_util_stream(f: tokio::fs::File) -> impl tokio_stream::Stream<Item = std::io::Result<Vec<u8>>> {
    futures::stream::unfold(f, |mut f| async move {
        use tokio::io::AsyncReadExt;
        let mut buf = vec![0u8; 256 * 1024];
        match f.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok(buf), f))
            }
            Err(e) => Some((Err(e), f)),
        }
    })
}

#[derive(Deserialize)]
struct HaltQ {
    #[serde(default)]
    all: bool,
}

async fn halts(State(sup): S, Query(q): Query<HaltQ>) -> Json<Vec<Halt>> {
    Json(sup.db.halts(!q.all))
}

async fn recover(State(sup): S, Extension(c): Extension<Caller>, Path(id): Path<String>, Json(r): Json<RecoverRequest>) -> R<Value> {
    c.need(Scope::Operate)?;
    Ok(Json(sup.resolve(&c.actor(), &id, &r.option, r.node).await?))
}

async fn checkpoint(State(sup): S, Path(id): Path<String>) -> R<Value> {
    let s = sup.session(&id)?;
    Ok(Json(json!({"checkpoint": sup.db.checkpoint(&id), "recovery_options": sup.recovery_options(&s), "harvests": sup.db.harvests(Some(&id))})))
}

#[derive(Deserialize)]
struct HarvestQ {
    #[serde(default)]
    session: Option<String>,
}

async fn harvests(State(sup): S, Query(q): Query<HarvestQ>) -> Json<Vec<Harvest>> {
    Json(sup.db.harvests(q.session.as_deref()))
}

#[derive(Deserialize)]
struct NotifQ {
    #[serde(default)]
    unread: bool,
    #[serde(default = "default_limit")]
    limit: i64,
}

async fn notifications(State(sup): S, Query(q): Query<NotifQ>) -> Json<Vec<Notification>> {
    Json(sup.db.notifications(q.unread, q.limit.clamp(1, 5000)))
}

#[derive(Deserialize)]
struct ReadReq {
    upto: i64,
}

async fn notifications_read(State(sup): S, Extension(c): Extension<Caller>, Json(r): Json<ReadReq>) -> R<Value> {
    c.need(Scope::Operate)?;
    Ok(Json(json!({"marked": sup.db.mark_notifications_read(r.upto)?})))
}

// ------------------------------------------------------------ web login

#[derive(Deserialize)]
struct LoginRequest {
    user: String,
    password: String,
    #[serde(default)]
    totp: Option<String>,
}

fn session_cookies(token: &str, csrf: &str, max_age_s: i64, secure: bool) -> [(axum::http::HeaderName, String); 2] {
    let sec = if secure { "; Secure" } else { "" };
    [
        (axum::http::header::SET_COOKIE, format!("moo_session={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={max_age_s}{sec}")),
        (axum::http::header::SET_COOKIE, format!("moo_csrf={csrf}; SameSite=Strict; Path=/; Max-Age={max_age_s}{sec}")),
    ]
}

async fn login(State(sup): S, conn: Option<Extension<crate::web::ConnInfo>>, headers: axum::http::HeaderMap, Json(r): Json<LoginRequest>) -> Response {
    let (ip, https) = crate::web::client(&sup, conn.as_ref().map(|c| &c.0), &headers);
    let agent = headers.get("user-agent").and_then(|v| v.to_str().ok()).unwrap_or("");
    match sup.login(r.user.trim(), &r.password, r.totp.as_deref(), ip, agent) {
        Ok((token, csrf, expires)) => {
            let max_age = (expires - now_ms()) / 1000;
            let [a, b] = session_cookies(&token, &csrf, max_age, https);
            let mut resp = Json(json!({"user": r.user.trim(), "csrf": csrf, "expires_at": expires})).into_response();
            resp.headers_mut().append(a.0, a.1.parse().unwrap());
            resp.headers_mut().append(b.0, b.1.parse().unwrap());
            resp
        }
        Err(e) => {
            use crate::auth::LoginError;
            // A little friction for guessing; the lockout does the rest.
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            let (code, totp) = match &e {
                LoginError::NeedTotp => (StatusCode::UNAUTHORIZED, true),
                LoginError::Locked(_) => (StatusCode::TOO_MANY_REQUESTS, false),
                LoginError::Invalid => (StatusCode::UNAUTHORIZED, false),
            };
            (code, Json(json!({"error": e.to_string(), "totp_required": totp}))).into_response()
        }
    }
}

async fn logout(State(sup): S, headers: axum::http::HeaderMap) -> Response {
    if let Some(c) = cookie(&headers, "moo_session") {
        if let Some(ws) = sup.web_session(&c) {
            let sent = headers.get("x-csrf-token").and_then(|v| v.to_str().ok()).unwrap_or("");
            if sent != ws.csrf {
                return ApiError(StatusCode::FORBIDDEN, "missing or wrong CSRF token".into()).into_response();
            }
        }
        sup.logout(&c);
    }
    let [a, b] = session_cookies("", "", 0, false);
    let mut resp = Json(json!({"ok": true})).into_response();
    resp.headers_mut().append(a.0, a.1.parse().unwrap());
    resp.headers_mut().append(b.0, b.1.parse().unwrap());
    resp
}

async fn me(State(sup): S, headers: axum::http::HeaderMap) -> R<Value> {
    match cookie(&headers, "moo_session").and_then(|c| sup.web_session(&c)) {
        Some(s) => Ok(Json(json!({"user": s.user, "scope": s.scope.as_str(), "expires_at": s.expires_at, "csrf": s.csrf}))),
        None => Err(ApiError(StatusCode::UNAUTHORIZED, "not logged in".into())),
    }
}

#[derive(Deserialize)]
struct PasswordChange {
    old: String,
    new: String,
}

async fn change_password(State(sup): S, headers: axum::http::HeaderMap, Json(r): Json<PasswordChange>) -> R<Value> {
    let ws = cookie(&headers, "moo_session").and_then(|c| sup.web_session(&c)).ok_or_else(|| ApiError(StatusCode::UNAUTHORIZED, "not logged in".into()))?;
    if headers.get("x-csrf-token").and_then(|v| v.to_str().ok()) != Some(ws.csrf.as_str()) {
        return Err(ApiError(StatusCode::FORBIDDEN, "missing or wrong CSRF token".into()));
    }
    sup.change_password(&ws.user, &r.old, &r.new)?;
    Ok(Json(json!({"ok": true, "note": "password changed; other sessions were logged out"})))
}

/// The embedded UI. Pages need a session (else `/login`); assets do not.
async fn ui(State(sup): S, req: Request) -> Response {
    let path = req.uri().path().to_string();
    if path.starts_with("/api/") {
        return ApiError(StatusCode::NOT_FOUND, "no such endpoint".into()).into_response();
    }
    let is_page = !path.contains('.') || path.ends_with(".html");
    let public = path == "/login" || !is_page;
    if !public && cookie(req.headers(), "moo_session").and_then(|c| sup.web_session(&c)).is_none() {
        let next = if path == "/" { String::new() } else { format!("?next={}", path.chars().filter(|c| c.is_ascii_alphanumeric() || "/-_".contains(*c)).collect::<String>()) };
        return (StatusCode::FOUND, [(axum::http::header::LOCATION, format!("/login{next}"))]).into_response();
    }
    crate::web::ui_asset(&path).unwrap_or_else(|| StatusCode::NOT_FOUND.into_response())
}

// ------------------------------------------------------------- attach, push

async fn attach(State(sup): S, Extension(c): Extension<Caller>, conn: Option<Extension<crate::web::ConnInfo>>, headers: axum::http::HeaderMap, Path(id): Path<String>) -> R<Value> {
    c.need(Scope::Operate)?;
    let (_, https) = crate::web::client(&sup, conn.as_ref().map(|c| &c.0), &headers);
    let host = headers.get("x-forwarded-host").or(headers.get(axum::http::header::HOST)).and_then(|h| h.to_str().ok());
    Ok(Json(sup.attach_ticket(&c.actor(), &id, host, https)?))
}

async fn push_key(State(sup): S) -> Json<Value> {
    Json(json!({"public_key": sup.notifier.vapid_public(), "enabled": sup.cfg.notifications.web_push.is_some()}))
}

#[derive(Deserialize)]
struct PushKeys {
    p256dh: String,
    auth: String,
}

#[derive(Deserialize)]
struct PushSub {
    endpoint: String,
    keys: PushKeys,
}

async fn push_subscribe(State(sup): S, Extension(c): Extension<Caller>, Json(r): Json<PushSub>) -> R<Value> {
    c.need(Scope::Read)?;
    if !r.endpoint.starts_with("https://") && !r.endpoint.starts_with("http://127.0.0.1") && !r.endpoint.starts_with("http://localhost") {
        return Err(ApiError(StatusCode::BAD_REQUEST, "push endpoints must be https".into()));
    }
    sup.db.put_push_sub(&r.endpoint, &r.keys.p256dh, &r.keys.auth, &c.name)?;
    sup.audit(&c.actor(), "push.subscribe", &r.endpoint.chars().take(80).collect::<String>(), json!({}), true);
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct PushUnsub {
    endpoint: String,
}

async fn push_unsubscribe(State(sup): S, Extension(c): Extension<Caller>, Json(r): Json<PushUnsub>) -> R<Value> {
    c.need(Scope::Read)?;
    Ok(Json(json!({"ok": sup.db.delete_push_sub(&r.endpoint)?})))
}

async fn notifications_test(State(sup): S, Extension(c): Extension<Caller>) -> R<Value> {
    c.need(Scope::Operate)?;
    sup.notify("test", "warning", "Test notification".into(), format!("sent by {} from the dashboard", c.name), None);
    Ok(Json(json!({"ok": true, "push_subscriptions": sup.db.push_subs().len(), "ntfy": sup.cfg.notifications.ntfy.is_some(), "webhook": sup.cfg.notifications.webhook.is_some()})))
}

// ------------------------------------------------------------------ users

async fn users(State(sup): S, Extension(c): Extension<Caller>) -> R<Value> {
    c.need(Scope::Admin)?;
    let v: Vec<Value> = sup.db.users().into_iter().map(|(n, s, t, l)| json!({"name": n, "scope": s, "totp": t, "locked_until": if l > now_ms() { Some(l) } else { None }})).collect();
    Ok(Json(Value::Array(v)))
}

#[derive(Deserialize)]
struct NewUser {
    name: String,
    password: String,
    #[serde(default = "default_scope")]
    scope: String,
}
fn default_scope() -> String {
    "admin".into()
}

async fn add_user(State(sup): S, Extension(c): Extension<Caller>, Json(r): Json<NewUser>) -> R<Value> {
    c.need(Scope::Admin)?;
    if !moocher_protocol::config::valid_name(&r.name) {
        return Err(ApiError(StatusCode::BAD_REQUEST, "user names are [a-z0-9-]".into()));
    }
    let scope = Scope::parse(&r.scope).ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, "scope must be read, operate or admin".into()))?;
    sup.db.put_user(&r.name, &crate::auth::hash_password(&r.password)?, scope)?;
    sup.audit(&c.actor(), "user.set", &r.name, json!({"scope": r.scope}), true);
    Ok(Json(json!({"ok": true})))
}

async fn delete_user(State(sup): S, Extension(c): Extension<Caller>, Path(name): Path<String>) -> R<Value> {
    c.need(Scope::Admin)?;
    let ok = sup.db.delete_user(&name)?;
    sup.audit(&c.actor(), "user.delete", &name, json!({}), ok);
    Ok(Json(json!({"ok": ok})))
}

#[derive(Deserialize)]
struct Promote {
    agent: String,
}

async fn promote_route(State(sup): S, Extension(c): Extension<Caller>, Path(node): Path<String>, Json(r): Json<Promote>) -> R<Value> {
    c.need(Scope::Admin)?;
    let res = sup.nodes.call(&node, moocher_protocol::link::NodeCommand::PromoteRoute { agent: r.agent.clone(), by: c.name.clone() }, crate::state::CMD_TIMEOUT).await;
    sup.audit(&c.actor(), "route.promote", &format!("{node}/{}", r.agent), json!({"error": res.as_ref().err().map(|e| format!("{e:#}"))}), res.is_ok());
    Ok(Json(res?))
}

/// What the UI needs to know about this hub.
async fn manager_status(State(sup): S) -> Json<Value> {
    Json(sup.manager_status())
}

async fn manager_ask(State(sup): S, Extension(c): Extension<Caller>, Json(r): Json<crate::manager::Ask>) -> R<Value> {
    c.need(Scope::Operate)?;
    Ok(Json(sup.manager_ask(&c.actor(), r).await?))
}

async fn info(State(sup): S, Extension(c): Extension<Caller>) -> Json<Value> {
    Json(json!({
        "version": moocher_protocol::VERSION,
        "user": c.name,
        "scope": c.scope.as_str(),
        "attach": sup.cfg.attach_listen.is_some(),
        "tls": sup.cfg.tls.is_some(),
        "push": {"enabled": sup.cfg.notifications.web_push.is_some(), "public_key": sup.notifier.vapid_public()},
        "question_mode": sup.cluster_question_mode(),
        "ntfy": sup.cfg.notifications.ntfy.is_some(),
        "webhook": sup.cfg.notifications.webhook.is_some(),
    }))
}

/// Prometheus text exposition.
async fn metrics(State(sup): S) -> impl IntoResponse {
    use std::collections::BTreeMap;
    use std::fmt::Write;
    let mut out = String::new();
    let nodes = sup.db.nodes();
    let _ = writeln!(out, "# TYPE moo_nodes_connected gauge\nmoo_nodes_connected {}", nodes.iter().filter(|n| n.connected).count());
    let sessions = sup.db.sessions(true);
    let mut by_state: BTreeMap<&str, u64> = BTreeMap::new();
    let mut tokens: BTreeMap<(String, &str), u64> = BTreeMap::new();
    let mut cost: BTreeMap<String, f64> = BTreeMap::new();
    for s in &sessions {
        *by_state.entry(s.report.state.as_str()).or_default() += 1;
        let a = s.report.agent.clone();
        for (k, v) in [("input", s.report.billed().input), ("output", s.report.billed().output), ("reasoning", s.report.billed().reasoning), ("cache_read", s.report.billed().cache_read)] {
            *tokens.entry((a.clone(), k)).or_default() += v;
        }
        *cost.entry(a).or_default() += s.report.billed().cost_usd;
    }
    let _ = writeln!(out, "# TYPE moo_sessions gauge");
    for (k, v) in by_state {
        let _ = writeln!(out, "moo_sessions{{state=\"{k}\"}} {v}");
    }
    let _ = writeln!(out, "# TYPE moo_tokens_total counter");
    for ((a, k), v) in tokens {
        let _ = writeln!(out, "moo_tokens_total{{agent=\"{a}\",kind=\"{k}\"}} {v}");
    }
    let _ = writeln!(out, "# TYPE moo_cost_usd_total counter");
    for (a, v) in cost {
        let _ = writeln!(out, "moo_cost_usd_total{{agent=\"{a}\"}} {v}");
    }
    let relays = sup.nodes.relay_stats.lock().unwrap().clone();
    let _ = writeln!(out, "# TYPE moo_relay_tunnels_total counter");
    for (r, (n, _, _)) in &relays {
        let _ = writeln!(out, "moo_relay_tunnels_total{{relay=\"{r}\"}} {n}");
    }
    let _ = writeln!(out, "# TYPE moo_relay_bytes_total counter");
    for (r, (_, up, down)) in &relays {
        let _ = writeln!(out, "moo_relay_bytes_total{{relay=\"{r}\",direction=\"out\"}} {up}");
        let _ = writeln!(out, "moo_relay_bytes_total{{relay=\"{r}\",direction=\"in\"}} {down}");
    }
    let mut ts: BTreeMap<&str, u64> = BTreeMap::new();
    for t in sup.db.tasks() {
        *ts.entry(t.status.as_str()).or_default() += 1;
    }
    let _ = writeln!(out, "# TYPE moo_tasks gauge");
    for (k, v) in ts {
        let _ = writeln!(out, "moo_tasks{{status=\"{k}\"}} {v}");
    }
    ([("content-type", "text/plain; version=0.0.4")], out)
}
