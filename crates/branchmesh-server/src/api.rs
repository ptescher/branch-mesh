use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use branchmesh::core::journal;
use branchmesh::core::media::{self, MediaRef};
use branchmesh::core::store::ns;
use branchmesh::{
    AgentPersona, BranchStatus, Change, CommitOptions, EnvelopeStatus, Error, Hash, MergeOptions, MergeStrategy,
    Runtime,
};
use futures::Stream;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Instant;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

/// What an application-level token store says about a bearer token that is
/// not the operator token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenCheck {
    /// Not one of the application's tokens.
    Unknown,
    /// A valid application token (e.g. an end-user or device token) that must
    /// not reach the operator API.
    NotOperator,
    /// A valid application token with operator rights (e.g. a session of an
    /// administrator who logged in with a password).
    Operator,
}

/// Hook for applications that issue their own bearer tokens: those tokens
/// are refused on the operator API (even when no operator token is set), or
/// granted operator access.
pub trait TokenAuthority: Send + Sync + 'static {
    fn check(&self, token: &str) -> TokenCheck;
}

#[derive(Clone)]
pub struct AppState {
    rt: Runtime,
    token: Option<Arc<String>>,
    authority: Option<Arc<dyn TokenAuthority>>,
    started: Instant,
}

impl AppState {
    pub fn new(rt: Runtime, token: Option<String>) -> Self {
        AppState { rt, token: token.filter(|t| !t.is_empty()).map(Arc::new), authority: None, started: Instant::now() }
    }

    pub fn with_authority(mut self, authority: Arc<dyn TokenAuthority>) -> Self {
        self.authority = Some(authority);
        self
    }

    pub fn runtime(&self) -> &Runtime {
        &self.rt
    }

    /// The configured operator token, if any.
    pub fn operator_token(&self) -> Option<&str> {
        self.token.as_deref().map(|s| s.as_str())
    }

    /// Bearer token from the Authorization header or `?token=` (EventSource
    /// cannot set headers).
    pub fn presented_token(headers: &axum::http::HeaderMap, uri: &axum::http::Uri) -> Option<String> {
        headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|t| t.trim().to_string())
            .or_else(|| {
                uri.query().and_then(|q| {
                    q.split('&').find_map(|kv| kv.strip_prefix("token=").map(|t| t.to_string()))
                })
            })
            .filter(|t| !t.is_empty())
    }

    /// `Some(true)`: operator; `Some(false)`: authenticated, but not an
    /// operator; `None`: unauthenticated. With no operator token configured,
    /// any caller that is not a known application token is an operator.
    pub fn operator_access(&self, headers: &axum::http::HeaderMap, uri: &axum::http::Uri) -> Option<bool> {
        let open = self.token.is_none();
        match Self::presented_token(headers, uri) {
            Some(t) if self.operator_token() == Some(t.as_str()) => Some(true),
            Some(t) => match self.authority.as_ref().map(|a| a.check(&t)) {
                Some(TokenCheck::Operator) => Some(true),
                Some(TokenCheck::NotOperator) => Some(false),
                _ if open => Some(true),
                _ => None,
            },
            None if open => Some(true),
            None => None,
        }
    }
}

// ------------------------------------------------------------------ errors

pub struct ApiError(pub StatusCode, pub Value);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(self.1)).into_response()
    }
}

impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        let msg = e.to_string();
        match e {
            Error::NotFound(_) | Error::BranchNotFound(_) | Error::ObjectNotFound(_) => {
                ApiError(StatusCode::NOT_FOUND, json!({ "error": msg }))
            }
            Error::Invalid(_) => ApiError(StatusCode::BAD_REQUEST, json!({ "error": msg })),
            Error::BranchExists(_) | Error::RefConflict(_) => ApiError(StatusCode::CONFLICT, json!({ "error": msg })),
            Error::MergeConflict(conflicts) => {
                ApiError(StatusCode::CONFLICT, json!({ "error": msg, "conflicts": conflicts }))
            }
            _ => ApiError(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": msg })),
        }
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(e: serde_json::Error) -> Self {
        ApiError(StatusCode::BAD_REQUEST, json!({ "error": e.to_string() }))
    }
}

pub fn bad(msg: impl std::fmt::Display) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, json!({ "error": msg.to_string() }))
}

pub type ApiResult = Result<Json<Value>, ApiError>;

// ------------------------------------------------------------------ router

/// The operator API (`/api/...`) and `/api/health`, without a UI fallback.
/// Applications can `merge` their own routes before adding
/// [`crate::ui::static_handler`] (or their own) as the fallback.
pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/stats", get(stats))
        .route("/events", get(events_sse))
        .route("/events/recent", get(events_recent))
        .route("/tools", get(tools))
        .route("/personas", get(list_personas).post(save_persona))
        .route("/personas/{id}", get(get_persona).put(update_persona).delete(delete_persona))
        .route("/threads", get(list_threads).post(create_thread))
        .route("/branches", get(list_branches).post(create_branch))
        .route("/branch", get(get_branch).delete(delete_branch))
        .route("/branch/log", get(branch_log))
        .route("/branch/state", get(branch_state).put(put_state))
        .route("/branch/messages", get(branch_messages))
        .route("/branch/fork", post(fork_branch))
        .route("/branch/rollback", post(rollback_branch))
        .route("/branch/abandon", post(abandon_branch))
        .route("/branch/send", post(send_message))
        .route("/commit/{hash}", get(get_commit))
        .route("/commit/{hash}/state", get(commit_state))
        .route("/diff", get(diff))
        .route("/merge/preview", post(merge_preview))
        .route("/merge", post(merge))
        .route("/merge-requests", get(list_mrs).post(create_mr))
        .route("/merge-requests/{id}/approve", post(approve_mr))
        .route("/merge-requests/{id}/reject", post(reject_mr))
        .route("/executions", get(list_executions))
        .route("/executions/{id}", get(get_execution))
        .route("/actors", get(actors))
        .route("/mailbox", get(mailbox))
        .route("/mailbox/{id}/retry", post(retry_envelope))
        .route("/topology", get(topology))
        .route("/media", post(upload_media).layer(DefaultBodyLimit::max(MEDIA_BODY_LIMIT)))
        .route("/media/{hash}", get(get_media))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth));

    Router::new()
        .route("/api/health", get(health))
        .nest("/api", api)
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(state)
}

/// Operator endpoints.
async fn auth(State(s): State<AppState>, req: Request, next: Next) -> Response {
    match s.operator_access(req.headers(), req.uri()) {
        Some(true) => next.run(req).await,
        Some(false) => (StatusCode::FORBIDDEN, Json(json!({ "error": "operator access required" }))).into_response(),
        None => (StatusCode::UNAUTHORIZED, Json(json!({ "error": "missing or invalid bearer token" }))).into_response(),
    }
}

/// Accept either a 64-char commit hash or a branch name.
fn resolve_ref(rt: &Runtime, r: &str) -> Result<Hash, ApiError> {
    if r.len() == 64 && r.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(Hash::from_hex(r)?);
    }
    Ok(rt.repo().head(r)?)
}

// ------------------------------------------------------------------ system

async fn health(State(s): State<AppState>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "provider": s.rt.mesh().llm().name(),
        "speech": { "stt": s.rt.speech().stt.name(), "tts": s.rt.speech().tts.name() },
        "uptime_s": s.started.elapsed().as_secs(),
        "auth": s.token.is_some(),
    }))
}

async fn stats(State(s): State<AppState>) -> ApiResult {
    let repo = s.rt.repo();
    let branches = repo.branches()?;
    let envelopes = s.rt.mesh().envelopes(None)?;
    let mut by_status: BTreeMap<String, usize> = BTreeMap::new();
    for e in &envelopes {
        *by_status.entry(serde_json::to_value(e.status)?.as_str().unwrap_or("").to_string()).or_default() += 1;
    }
    let actors = s.rt.mesh().actors()?;
    Ok(Json(json!({
        "provider": s.rt.mesh().llm().name(),
        "branches": branches.len(),
        "active_branches": branches.iter().filter(|b| b.status == BranchStatus::Active).count(),
        "personas": s.rt.mesh().personas()?.len(),
        "actors": actors.len(),
        "actors_active": actors.iter().filter(|a| a.active).count(),
        "executions": repo.store().count_docs(ns::EXECUTIONS)?,
        "objects": repo.store().object_count()?,
        "mailbox": by_status,
        "uptime_s": s.started.elapsed().as_secs(),
    })))
}

async fn events_sse(State(s): State<AppState>) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let stream = BroadcastStream::new(s.rt.events().subscribe()).filter_map(|ev| {
        ev.ok().map(|ev| Ok(SseEvent::default().event(ev.kind.clone()).json_data(&ev).unwrap_or_default()))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[derive(Deserialize)]
struct LimitQ {
    limit: Option<usize>,
}

async fn events_recent(State(s): State<AppState>, Query(q): Query<LimitQ>) -> ApiResult {
    Ok(Json(json!(s.rt.events().recent(q.limit.unwrap_or(200)))))
}

async fn tools(State(s): State<AppState>) -> ApiResult {
    let list: Vec<Value> = s
        .rt
        .mesh()
        .tools()
        .iter()
        .map(|t| json!({ "name": t.name(), "description": t.description(), "server": t.server_definition().is_some() }))
        .collect();
    Ok(Json(json!(list)))
}

// ------------------------------------------------------------------ personas

async fn list_personas(State(s): State<AppState>) -> ApiResult {
    Ok(Json(json!(s.rt.mesh().personas()?)))
}

async fn get_persona(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    Ok(Json(json!(s.rt.mesh().persona(&id)?)))
}

async fn save_persona(State(s): State<AppState>, Json(p): Json<AgentPersona>) -> ApiResult {
    Ok(Json(json!(s.rt.register_persona(&p)?)))
}

async fn update_persona(State(s): State<AppState>, Path(id): Path<String>, Json(mut p): Json<AgentPersona>) -> ApiResult {
    p.id = id;
    Ok(Json(json!(s.rt.register_persona(&p)?)))
}

async fn delete_persona(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    s.rt.mesh().delete_persona(&id)?;
    Ok(Json(json!({ "ok": true })))
}

// ------------------------------------------------------------------ branches

async fn list_threads(State(s): State<AppState>) -> ApiResult {
    let threads: Vec<_> = s.rt.repo().branches()?.into_iter().filter(|b| b.meta["kind"] == "thread").collect();
    Ok(Json(json!(threads)))
}

#[derive(Deserialize)]
struct CreateThread {
    persona: String,
    thread_id: Option<String>,
}

async fn create_thread(State(s): State<AppState>, Json(b): Json<CreateThread>) -> ApiResult {
    let persona = s.rt.mesh().persona(&b.persona)?;
    let id = b.thread_id.filter(|t| !t.is_empty()).unwrap_or_else(|| branchmesh::core::new_id()[..10].to_string());
    let t = s.rt.spawn_thread(&persona, &id).await?;
    Ok(Json(json!(s.rt.repo().branch(t.branch())?)))
}

async fn list_branches(State(s): State<AppState>) -> ApiResult {
    Ok(Json(json!(s.rt.repo().branches()?)))
}

#[derive(Deserialize)]
struct CreateBranch {
    name: String,
    /// Fork from this branch's head...
    from: Option<String>,
    /// ...or from this commit.
    at: Option<String>,
    #[serde(default)]
    meta: Value,
}

async fn create_branch(State(s): State<AppState>, Json(b): Json<CreateBranch>) -> ApiResult {
    let repo = s.rt.repo();
    let meta = if b.meta.is_null() { json!({ "kind": "manual" }) } else { b.meta };
    let info = match (b.from, b.at) {
        (_, Some(at)) => repo.fork_at(&Hash::from_hex(&at)?, &b.name, None, meta)?,
        (Some(from), None) => repo.fork(&from, &b.name, meta)?,
        (None, None) => repo.create_branch(&b.name, meta)?,
    };
    Ok(Json(json!(info)))
}

#[derive(Deserialize)]
struct NameQ {
    name: String,
    commit: Option<String>,
    limit: Option<usize>,
}

async fn get_branch(State(s): State<AppState>, Query(q): Query<NameQ>) -> ApiResult {
    let repo = s.rt.repo();
    let b = repo.branch(&q.name)?;
    let head = repo.commit_object(&b.head)?;
    let ahead = match &b.parent {
        Some(p) => match repo.try_branch(p)? {
            Some(parent) => {
                let base = repo.merge_base(&b.head, &parent.head)?;
                let behind = repo.log(&parent.head, 10_000)?.iter().take_while(|(h, _)| *h != base).count();
                let ahead = repo.log(&b.head, 10_000)?.iter().take_while(|(h, _)| *h != base).count();
                json!({ "parent": p, "ahead": ahead, "behind": behind, "base": base })
            }
            None => Value::Null,
        },
        None => Value::Null,
    };
    Ok(Json(json!({ "branch": b, "head_commit": head, "divergence": ahead })))
}

async fn delete_branch(State(s): State<AppState>, Query(q): Query<NameQ>) -> ApiResult {
    s.rt.repo().delete_branch(&q.name)?;
    Ok(Json(json!({ "ok": true })))
}

async fn branch_log(State(s): State<AppState>, Query(q): Query<NameQ>) -> ApiResult {
    let repo = s.rt.repo();
    let head = repo.head(&q.name)?;
    let log: Vec<Value> = repo
        .log(&head, q.limit.unwrap_or(200))?
        .into_iter()
        .map(|(h, c)| json!({ "hash": h, "commit": c }))
        .collect();
    Ok(Json(json!(log)))
}

async fn branch_state(State(s): State<AppState>, Query(q): Query<NameQ>) -> ApiResult {
    let commit = match &q.commit {
        Some(c) => Hash::from_hex(c)?,
        None => s.rt.repo().head(&q.name)?,
    };
    Ok(Json(json!({ "commit": commit, "state": s.rt.repo().state_at(&commit)? })))
}

#[derive(Deserialize)]
struct PutState {
    name: String,
    path: String,
    /// `null` deletes the path.
    value: Value,
    #[serde(default)]
    message: Option<String>,
}

async fn put_state(State(s): State<AppState>, Json(b): Json<PutState>) -> ApiResult {
    let change = if b.value.is_null() { Change::delete(&b.path) } else { Change::set(&b.path, b.value) };
    let msg = b.message.unwrap_or_else(|| format!("manual edit of {}", b.path));
    let commit = s.rt.repo().commit(&b.name, &[change], CommitOptions::new("operator", msg))?;
    Ok(Json(json!({ "commit": commit })))
}

async fn branch_messages(State(s): State<AppState>, Query(q): Query<NameQ>) -> ApiResult {
    let commit = match &q.commit {
        Some(c) => Hash::from_hex(c)?,
        None => s.rt.repo().head(&q.name)?,
    };
    let msgs: Vec<Value> = s.rt.repo().list_at(&commit, "messages")?.into_iter().map(|(_, v)| v).collect();
    Ok(Json(json!(msgs)))
}

#[derive(Deserialize)]
struct ForkBody {
    source: String,
    name: String,
    #[serde(default)]
    meta: Value,
}

async fn fork_branch(State(s): State<AppState>, Json(b): Json<ForkBody>) -> ApiResult {
    let src = s.rt.repo().branch(&b.source)?;
    let meta = if b.meta.is_null() {
        json!({ "kind": "fork", "persona": src.meta["persona"] })
    } else {
        b.meta
    };
    Ok(Json(json!(s.rt.repo().fork(&b.source, &b.name, meta)?)))
}

#[derive(Deserialize)]
struct RollbackBody {
    name: String,
    commit: String,
}

async fn rollback_branch(State(s): State<AppState>, Json(b): Json<RollbackBody>) -> ApiResult {
    let commit = s.rt.repo().rollback(&b.name, &Hash::from_hex(&b.commit)?, "operator")?;
    Ok(Json(json!({ "commit": commit })))
}

#[derive(Deserialize)]
struct NameBody {
    name: String,
}

async fn abandon_branch(State(s): State<AppState>, Json(b): Json<NameBody>) -> ApiResult {
    let info = s.rt.repo().set_status(&b.name, BranchStatus::Abandoned { at: branchmesh::core::now_ms() })?;
    Ok(Json(json!(info)))
}

// ------------------------------------------------------------------ media

/// Request body limit for image uploads: the image limit plus slack.
pub const MEDIA_BODY_LIMIT: usize = media::MAX_IMAGE_BYTES + 64 * 1024;

/// Validate and store an uploaded image (the raw request body). Shared with
/// applications that expose their own upload routes.
pub fn store_image(rt: &Runtime, bytes: &[u8]) -> Result<MediaRef, ApiError> {
    let media_type = media::validate_image(bytes)?;
    Ok(rt.repo().store().put_media(bytes, media_type)?)
}

/// Look up stored media by hash (for attaching it to a message).
pub fn media_ref(rt: &Runtime, hash: &str) -> Result<MediaRef, ApiError> {
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(bad(format!("invalid media hash '{hash}'")));
    }
    rt.repo()
        .store()
        .media_info(hash)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, json!({ "error": format!("media '{hash}' not found") })))
}

/// Serve stored media. Content-addressed, so it can be cached forever.
pub fn media_response(rt: &Runtime, hash: &str) -> Result<Response, ApiError> {
    let info = media_ref(rt, hash)?;
    let bytes = rt.repo().store().get_media(hash)?.ok_or_else(|| ApiError(StatusCode::NOT_FOUND, json!({ "error": "media not found" })))?;
    Ok((
        [
            (header::CONTENT_TYPE, info.media_type),
            (header::CACHE_CONTROL, "private, max-age=31536000, immutable".into()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".into()),
        ],
        bytes,
    )
        .into_response())
}

/// `POST /api/media` with the image as the raw body: returns its [`MediaRef`].
async fn upload_media(State(s): State<AppState>, body: Bytes) -> ApiResult {
    Ok(Json(json!(store_image(&s.rt, &body)?)))
}

async fn get_media(State(s): State<AppState>, Path(hash): Path<String>) -> Result<Response, ApiError> {
    media_response(&s.rt, &hash)
}

#[derive(Deserialize)]
struct SendBody {
    branch: String,
    /// Defaults to the branch's persona.
    persona: Option<String>,
    #[serde(default)]
    content: String,
    /// Hashes of uploaded images (`POST /api/media`) to send with the message.
    #[serde(default)]
    attachments: Vec<String>,
    #[serde(default)]
    from: Option<String>,
    /// Wait for the reply instead of returning the envelope immediately.
    #[serde(default)]
    wait: bool,
}

async fn send_message(State(s): State<AppState>, Json(b): Json<SendBody>) -> ApiResult {
    let branch = s.rt.repo().branch(&b.branch)?;
    let persona = b
        .persona
        .or_else(|| branch.meta["persona"].as_str().map(String::from))
        .ok_or_else(|| bad("no persona given and the branch has no default persona"))?;
    let attachments = b.attachments.iter().map(|h| media_ref(&s.rt, h)).collect::<Result<Vec<_>, _>>()?;
    if b.content.trim().is_empty() && attachments.is_empty() {
        return Err(bad("empty message"));
    }
    let from = b.from.unwrap_or_else(|| "user".into());
    let mesh = s.rt.mesh();
    if b.wait {
        let reply = mesh.ask_with(&persona, &b.branch, &from, &b.content, attachments, Value::Null, None).await?;
        Ok(Json(json!({ "reply": reply })))
    } else {
        let env = mesh.send_with(&persona, &b.branch, &from, &b.content, attachments, Value::Null, None)?;
        Ok(Json(json!({ "envelope": env })))
    }
}

// ------------------------------------------------------------------ commits & diff

async fn get_commit(State(s): State<AppState>, Path(hash): Path<String>) -> ApiResult {
    let h = Hash::from_hex(&hash)?;
    let c = s.rt.repo().commit_object(&h)?;
    let diff = match c.parents.first() {
        Some(p) => s.rt.repo().diff(p, &h)?,
        None => vec![],
    };
    Ok(Json(json!({ "hash": h, "commit": c, "diff": diff })))
}

async fn commit_state(State(s): State<AppState>, Path(hash): Path<String>) -> ApiResult {
    let h = Hash::from_hex(&hash)?;
    Ok(Json(json!({ "commit": h, "state": s.rt.repo().state_at(&h)? })))
}

#[derive(Deserialize)]
struct DiffQ {
    from: String,
    to: String,
}

async fn diff(State(s): State<AppState>, Query(q): Query<DiffQ>) -> ApiResult {
    let from = resolve_ref(&s.rt, &q.from)?;
    let to = resolve_ref(&s.rt, &q.to)?;
    Ok(Json(json!({ "from": from, "to": to, "changes": s.rt.repo().diff(&from, &to)? })))
}

// ------------------------------------------------------------------ merging

#[derive(Deserialize)]
struct MergeBody {
    source: String,
    target: String,
    #[serde(default)]
    strategy: MergeStrategy,
    /// Manual resolutions: path -> value (`null` deletes).
    #[serde(default)]
    resolutions: BTreeMap<String, Value>,
    #[serde(default = "yes")]
    close_source: bool,
}

fn yes() -> bool {
    true
}

async fn merge_preview(State(s): State<AppState>, Json(b): Json<MergeBody>) -> ApiResult {
    Ok(Json(json!(s.rt.repo().merge_preview(&b.source, &b.target)?)))
}

async fn do_merge(rt: &Runtime, b: &MergeBody) -> Result<Value, ApiError> {
    let mut opts = MergeOptions::new(b.strategy);
    opts.author = "operator".into();
    opts.close_source = b.close_source;
    opts.resolutions = b
        .resolutions
        .iter()
        .map(|(k, v)| (k.clone(), if v.is_null() { None } else { Some(v.clone()) }))
        .collect();
    Ok(json!(rt.merge(&b.source, &b.target, opts).await?))
}

async fn merge(State(s): State<AppState>, Json(b): Json<MergeBody>) -> ApiResult {
    Ok(Json(do_merge(&s.rt, &b).await?))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MergeRequest {
    id: String,
    title: String,
    source: String,
    target: String,
    strategy: MergeStrategy,
    /// open | merged | rejected
    status: String,
    created_at: i64,
    updated_at: i64,
    #[serde(default)]
    outcome: Option<Value>,
}

async fn list_mrs(State(s): State<AppState>) -> ApiResult {
    let mut mrs: Vec<MergeRequest> =
        s.rt.repo().store().list_docs(ns::MERGE_REQUESTS, "")?.into_iter().map(|(_, m)| m).collect();
    mrs.sort_by_key(|m| std::cmp::Reverse(m.created_at));
    Ok(Json(json!(mrs)))
}

#[derive(Deserialize)]
struct CreateMr {
    source: String,
    target: String,
    #[serde(default)]
    strategy: MergeStrategy,
    #[serde(default)]
    title: Option<String>,
}

async fn create_mr(State(s): State<AppState>, Json(b): Json<CreateMr>) -> ApiResult {
    s.rt.repo().branch(&b.source)?;
    s.rt.repo().branch(&b.target)?;
    let now = branchmesh::core::now_ms();
    let mr = MergeRequest {
        id: format!("mr-{}", &branchmesh::core::new_id()[..10]),
        title: b.title.unwrap_or_else(|| format!("Merge {} into {}", b.source, b.target)),
        source: b.source,
        target: b.target,
        strategy: b.strategy,
        status: "open".into(),
        created_at: now,
        updated_at: now,
        outcome: None,
    };
    s.rt.repo().store().put_doc(ns::MERGE_REQUESTS, &mr.id, &mr)?;
    s.rt.events().emit("merge_request", json!({ "merge_request": mr }));
    Ok(Json(json!(mr)))
}

#[derive(Deserialize, Default)]
struct ApproveBody {
    strategy: Option<MergeStrategy>,
    #[serde(default)]
    resolutions: BTreeMap<String, Value>,
}

fn load_mr(rt: &Runtime, id: &str) -> Result<MergeRequest, ApiError> {
    rt.repo()
        .store()
        .get_doc(ns::MERGE_REQUESTS, id)?
        .ok_or_else(|| Error::NotFound(format!("merge request {id}")).into())
}

async fn approve_mr(State(s): State<AppState>, Path(id): Path<String>, body: Option<Json<ApproveBody>>) -> ApiResult {
    let mut mr = load_mr(&s.rt, &id)?;
    if mr.status != "open" {
        return Err(bad(format!("merge request is {}", mr.status)));
    }
    let body = body.map(|b| b.0).unwrap_or_default();
    let mb = MergeBody {
        source: mr.source.clone(),
        target: mr.target.clone(),
        strategy: body.strategy.unwrap_or(mr.strategy),
        resolutions: body.resolutions,
        close_source: true,
    };
    let outcome = do_merge(&s.rt, &mb).await?;
    mr.status = "merged".into();
    mr.updated_at = branchmesh::core::now_ms();
    mr.outcome = Some(outcome);
    s.rt.repo().store().put_doc(ns::MERGE_REQUESTS, &mr.id, &mr)?;
    s.rt.events().emit("merge_request", json!({ "merge_request": mr }));
    Ok(Json(json!(mr)))
}

async fn reject_mr(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let mut mr = load_mr(&s.rt, &id)?;
    mr.status = "rejected".into();
    mr.updated_at = branchmesh::core::now_ms();
    s.rt.repo().store().put_doc(ns::MERGE_REQUESTS, &mr.id, &mr)?;
    s.rt.events().emit("merge_request", json!({ "merge_request": mr }));
    Ok(Json(json!(mr)))
}

// ------------------------------------------------------------------ journal & mailbox

#[derive(Deserialize)]
struct ExecQ {
    branch: Option<String>,
    actor: Option<String>,
    limit: Option<usize>,
}

async fn list_executions(State(s): State<AppState>, Query(q): Query<ExecQ>) -> ApiResult {
    let list: Vec<_> = journal::executions(s.rt.repo())?
        .into_iter()
        .filter(|e| q.branch.as_ref().map(|b| &e.branch == b).unwrap_or(true))
        .filter(|e| q.actor.as_ref().map(|a| &e.actor == a).unwrap_or(true))
        .take(q.limit.unwrap_or(200))
        .collect();
    Ok(Json(json!(list)))
}

async fn get_execution(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let exec = journal::execution(s.rt.repo(), &id)?.ok_or_else(|| Error::NotFound(format!("execution {id}")))?;
    let entries = journal::entries(s.rt.repo(), &id)?;
    Ok(Json(json!({ "execution": exec, "entries": entries })))
}

async fn actors(State(s): State<AppState>) -> ApiResult {
    Ok(Json(json!(s.rt.mesh().actors()?)))
}

#[derive(Deserialize)]
struct MailboxQ {
    status: Option<EnvelopeStatus>,
    limit: Option<usize>,
}

async fn mailbox(State(s): State<AppState>, Query(q): Query<MailboxQ>) -> ApiResult {
    let list: Vec<_> = s.rt.mesh().envelopes(q.status)?.into_iter().take(q.limit.unwrap_or(500)).collect();
    Ok(Json(json!(list)))
}

async fn retry_envelope(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    Ok(Json(json!(s.rt.mesh().retry(&id)?)))
}

// ------------------------------------------------------------------ topology

async fn topology(State(s): State<AppState>) -> ApiResult {
    let mesh = s.rt.mesh();
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    for p in mesh.personas()? {
        nodes.push(json!({ "id": format!("p:{}", p.id), "type": "persona", "label": p.name, "persona": p.id }));
    }
    let branches = s.rt.repo().branches()?;
    for b in &branches {
        let status = match &b.status {
            BranchStatus::Active => "active",
            BranchStatus::Merged { .. } => "merged",
            BranchStatus::Abandoned { .. } => "abandoned",
        };
        nodes.push(json!({
            "id": format!("b:{}", b.name), "type": "branch", "label": b.name, "status": status,
            "kind": b.meta["kind"], "head": b.head, "updated_at": b.updated_at,
        }));
        if let Some(parent) = &b.parent {
            edges.push(json!({ "source": format!("b:{parent}"), "target": format!("b:{}", b.name), "type": "fork" }));
        }
        if let BranchStatus::Merged { into, .. } = &b.status {
            edges.push(json!({ "source": format!("b:{}", b.name), "target": format!("b:{into}"), "type": "merge" }));
        }
    }
    for a in mesh.actors()? {
        nodes.push(json!({
            "id": format!("a:{}", a.key), "type": "actor", "label": a.persona, "active": a.active,
            "busy": a.current.is_some(), "branch": a.branch, "persona": a.persona,
            "pending": a.pending + a.processing, "dead_letter": a.dead_letter,
        }));
        edges.push(json!({ "source": format!("p:{}", a.persona), "target": format!("a:{}", a.key), "type": "persona" }));
        edges.push(json!({ "source": format!("a:{}", a.key), "target": format!("b:{}", a.branch), "type": "runs_on" }));
    }
    // Agent -> agent message links (aggregated from the mailbox).
    let mut links: HashMap<(String, String), usize> = HashMap::new();
    for e in mesh.envelopes(None)? {
        if let Some(from) = e.from.strip_prefix("agent:") {
            *links.entry((from.to_string(), e.to.clone())).or_default() += 1;
        }
    }
    for ((from, to), count) in links {
        edges.push(json!({ "source": format!("p:{from}"), "target": format!("p:{to}"), "type": "message", "count": count }));
    }
    Ok(Json(json!({ "nodes": nodes, "edges": edges })))
}
