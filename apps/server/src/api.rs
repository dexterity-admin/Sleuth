//! The HTTPS API under `/api/v1`, for `sleuth_engine::remote::Client`.
//!
//! Every request except `/health` carries `Authorization: Bearer <key>`.
//! The key's SHA-256 picks the user; the user's projects give the folders
//! they may see, and every answer is limited to those folders:
//!
//! - searches and tag, revision and occurrence lists pass the folders to the
//!   engine, which limits them in SQL; the rows are checked again here;
//! - a document id is turned into its path and the path checked before
//!   anything about it is returned;
//! - a project that is not the user's (or does not exist) is `forbidden`;
//! - nothing is read from disk except indexed documents, by id.
//!
//! Searches and page images are "expensive": each user may run 4 at once,
//! and page images are also limited to [`PAGES_PER_MINUTE`]. Beyond that the
//! answer is 429 `rate_limited`.
//!
//! Errors are `{code, message}` with a stable code: `unauthorized`,
//! `forbidden`, `not_found`, `bad_request`, `rate_limited`, `unavailable`.
//! Refused requests and administrator actions go to the audit log.

use crate::audit::Audit;
use crate::config::{Config, Role, path_allowed, roots_within};
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use sleuth_core::protocol::{Event, NormBox, TextSource};
use sleuth_core::settings::Project;
use sleuth_engine::remote::{ApiError, Capabilities, CompareReply, DocLookup, DocReply, JoinReply, JoinRequest, PageReply, SearchReply};
use sleuth_engine::service::Engine;
use sleuth_engine::store::Scope;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub const API_VERSION: u32 = sleuth_engine::remote::API_MAJOR;
/// Searches and page images one user may have running at once.
pub const CONCURRENT_EXPENSIVE: usize = 4;
pub const PAGES_PER_MINUTE: f64 = 120.0;
pub const MAX_QUERY: usize = 500;

type Params = HashMap<String, String>;

pub struct AppState {
    pub engine: Arc<Engine>,
    pub data: PathBuf,
    pub cfg: RwLock<Arc<Config>>,
    pub audit: Audit,
    limits: Mutex<HashMap<String, Limit>>,
    /// One join at a time: each reads, changes and saves `server.json`.
    join_lock: Mutex<()>,
    /// Joins asked per address in the current minute.
    joins: Mutex<HashMap<std::net::IpAddr, (Instant, u32)>>,
}

/// Joins one address may ask for in a minute (a PC needs one).
pub const JOINS_PER_MINUTE: u32 = 6;

struct Limit {
    busy: Arc<Semaphore>,
    pages: f64,
    at: Instant,
}

impl AppState {
    pub fn new(engine: Arc<Engine>, data: PathBuf, cfg: Config, audit: Audit) -> AppState {
        AppState { engine, data, cfg: RwLock::new(Arc::new(cfg)), audit, limits: Mutex::new(HashMap::new()), join_lock: Mutex::new(()), joins: Mutex::new(HashMap::new()) }
    }

    /// A PC that showed the join code from the office index folder gets
    /// its own access key (a new one each time it asks; the old one stops).
    /// Only the projects set for joining are given, and never the admin role.
    fn join(&self, req: &JoinRequest, peer: SocketAddr) -> Result<JoinReply, ApiErr> {
        {
            let mut m = self.joins.lock().unwrap();
            m.retain(|_, (at, _)| at.elapsed().as_secs() < 60);
            let e = m.entry(peer.ip()).or_insert((Instant::now(), 0));
            e.1 += 1;
            if e.1 > JOINS_PER_MINUTE {
                return Err(ApiErr::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited", "too many tries from this PC; wait a minute"));
            }
        }
        let clean = |s: &str, max: usize| -> String { s.trim().chars().filter(|c| c.is_alphanumeric() || "-_. $".contains(*c)).take(max).collect::<String>().trim().to_string() };
        let (pc, user) = (clean(&req.pc, 63), clean(&req.user, 64));
        if pc.is_empty() {
            return Err(ApiErr::bad("the PC's name is missing"));
        }
        let _g = self.join_lock.lock().unwrap();
        let mut cfg = Config::load(&self.data)?;
        let from = format!("{pc} at {}", peer.ip());
        if !cfg.self_registration {
            self.audit.record(&format!("{pc}\\{user}"), "join", &from, "refused: PCs may not connect themselves");
            return Err(ApiErr::forbidden("this office server does not let PCs connect themselves; ask IT for an access key"));
        }
        if !crate::config::join_code_matches(&cfg, &req.code) {
            self.audit.record(&format!("{pc}\\{user}"), "join", &from, "refused: wrong join code");
            return Err(ApiErr::new(StatusCode::UNAUTHORIZED, "unauthorized", "the join code is not this server's; the office index folder may hold an old one"));
        }
        let name = if user.is_empty() { pc.clone() } else { format!("{pc}\\{user}") };
        if cfg.user(&name).is_some_and(|u| u.pc.is_empty()) {
            self.audit.record(&name, "join", &from, "refused: a person of that name was added by hand");
            return Err(ApiErr::forbidden(format!("\"{name}\" was added by hand on the server; ask IT for its key")));
        }
        let key = crate::config::new_key()?;
        cfg.users.retain(|u| !u.name.eq_ignore_ascii_case(&name));
        cfg.users.push(crate::config::UserCfg {
            name: name.clone(),
            token_sha256: crate::config::hash_key(&key),
            role: Role::User,
            projects: cfg.join_projects.clone(),
            pc: pc.clone(),
            since: sleuth_engine::service::now(),
        });
        cfg.save(&self.data)?;
        let projects = cfg.join_projects.clone();
        *self.cfg.write().unwrap() = Arc::new(cfg);
        self.audit.record(&name, "join", &from, "ok");
        Ok(JoinReply { key, name, projects, server_version: env!("CARGO_PKG_VERSION").into() })
    }

    pub fn config(&self) -> Arc<Config> {
        Arc::clone(&self.cfg.read().unwrap())
    }

    /// A slot for a search or page image, or `rate_limited`.
    fn expensive(&self, who: &Caller, page: bool) -> Result<OwnedSemaphorePermit, ApiErr> {
        let mut map = self.limits.lock().unwrap();
        let l = map.entry(who.name.to_lowercase()).or_insert_with(|| Limit { busy: Arc::new(Semaphore::new(CONCURRENT_EXPENSIVE)), pages: PAGES_PER_MINUTE, at: Instant::now() });
        let refused = |why: &str| {
            self.audit.record(&who.name, if page { "page" } else { "search" }, "", why);
            ApiErr::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited", why)
        };
        if page {
            l.pages = (l.pages + l.at.elapsed().as_secs_f64() * PAGES_PER_MINUTE / 60.0).min(PAGES_PER_MINUTE);
            l.at = Instant::now();
            if l.pages < 1.0 {
                return Err(refused("too many page images this minute; wait a little"));
            }
        }
        let permit = Arc::clone(&l.busy).try_acquire_owned().map_err(|_| refused("too many requests at once; wait for the others to finish"))?;
        if page {
            l.pages -= 1.0;
        }
        Ok(permit)
    }
}

pub struct ApiErr {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiErr {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> ApiErr {
        ApiErr { status, code, message: message.into() }
    }
    fn bad(m: impl Into<String>) -> ApiErr {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", m)
    }
    fn forbidden(m: impl Into<String>) -> ApiErr {
        Self::new(StatusCode::FORBIDDEN, "forbidden", m)
    }
    fn not_found(m: impl Into<String>) -> ApiErr {
        Self::new(StatusCode::NOT_FOUND, "not_found", m)
    }
    fn unavailable(e: impl std::fmt::Display) -> ApiErr {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "unavailable", format!("the office server could not do this: {e}"))
    }
}

impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        (self.status, Json(ApiError { code: self.code.into(), message: self.message })).into_response()
    }
}

impl From<anyhow::Error> for ApiErr {
    fn from(e: anyhow::Error) -> ApiErr {
        ApiErr::unavailable(format!("{e:#}"))
    }
}

type Reply<T> = Result<Json<T>, ApiErr>;

/// The signed-in user of one request.
struct Caller {
    name: String,
    role: Role,
    projects: Vec<Project>,
    /// Every folder the user may see.
    roots: Vec<String>,
}

fn caller(st: &AppState, headers: &HeaderMap, peer: SocketAddr, uri: &Uri) -> Result<Caller, ApiErr> {
    let key = headers.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer "))).map(str::trim).unwrap_or("");
    let cfg = st.config();
    match cfg.user_by_key(key).filter(|_| !key.is_empty()) {
        // Folders as the index stores paths (a share by its server's
        // address), so every comparison with a stored path holds whatever
        // name or drive letter `server.json` uses.
        Some(u) => Ok(Caller { name: u.name.clone(), role: u.role, projects: cfg.user_projects(u).into_iter().cloned().collect(), roots: cfg.user_roots(u).iter().map(|r| st.engine.stored_path(r)).collect() }),
        None => {
            st.audit.record("-", uri.path(), &format!("from {}", peer.ip()), if key.is_empty() { "unauthorized: no access key" } else { "unauthorized: unknown access key" });
            Err(ApiErr::new(StatusCode::UNAUTHORIZED, "unauthorized", "the access key was not accepted; ask IT for a new one"))
        }
    }
}

fn admin(st: &AppState, c: &Caller, action: &str) -> Result<(), ApiErr> {
    if c.role == Role::Admin {
        return Ok(());
    }
    st.audit.record(&c.name, action, "", "forbidden: not an administrator");
    Err(ApiErr::forbidden("you are not allowed to do this (administrators only)"))
}

fn params(q: Result<Query<Params>, QueryRejection>) -> Result<Params, ApiErr> {
    q.map(|Query(p)| p).map_err(|e| ApiErr::bad(format!("the request is not understood: {e}")))
}

fn text<'a>(p: &'a Params, k: &str, max: usize) -> Result<Option<&'a str>, ApiErr> {
    match p.get(k).map(|v| v.trim()).filter(|v| !v.is_empty()) {
        Some(v) if v.chars().count() > max => Err(ApiErr::bad(format!("{k} is longer than {max} characters"))),
        v => Ok(v),
    }
}

fn number(p: &Params, k: &str, default: i64, lo: i64, hi: i64) -> Result<i64, ApiErr> {
    match p.get(k).map(|v| v.trim()).filter(|v| !v.is_empty()) {
        None => Ok(default),
        Some(v) => v.parse::<i64>().map(|n| n.clamp(lo, hi)).map_err(|_| ApiErr::bad(format!("{k} must be a whole number"))),
    }
}

fn id_of(p: Result<Path<String>, PathRejection>) -> Result<i64, ApiErr> {
    let Path(s) = p.map_err(|e| ApiErr::bad(format!("the request is not understood: {e}")))?;
    s.parse::<i64>().ok().filter(|n| *n > 0).ok_or_else(|| ApiErr::bad("a document id is a positive whole number"))
}

/// Run engine work on a blocking thread.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, ApiErr> + Send + 'static) -> Result<T, ApiErr> {
    tokio::task::spawn_blocking(f).await.map_err(|e| ApiErr::unavailable(format!("the request stopped: {e}")))?
}

/// Folders to look in: all of the user's, or one of their projects.
fn scoped_roots(st: &AppState, c: &Caller, project: Option<&str>, action: &str) -> Result<Vec<String>, ApiErr> {
    let Some(name) = project else { return Ok(c.roots.clone()) };
    match c.projects.iter().find(|p| p.name.eq_ignore_ascii_case(name)) {
        Some(p) => Ok(roots_within(&p.roots.iter().map(|r| st.engine.stored_path(r)).collect::<Vec<_>>(), &c.roots)),
        None => {
            st.audit.record(&c.name, action, &format!("project {name}"), "forbidden: not the user's project");
            Err(ApiErr::forbidden(format!("you are not allowed to see project \"{name}\"")))
        }
    }
}

/// The path of an indexed document the user may see.
fn doc_path(st: &AppState, c: &Caller, id: i64, action: &str) -> Result<String, ApiErr> {
    let path = st.engine.file_path(id)?.ok_or_else(|| ApiErr::not_found("no such document"))?;
    if !path_allowed(&path, &c.roots) {
        st.audit.record(&c.name, action, &format!("document {id}"), "forbidden: outside the user's projects");
        return Err(ApiErr::forbidden("you are not allowed to see this document"));
    }
    Ok(path)
}

fn is_pdf(path: &str) -> bool {
    sleuth_core::kinds::FileKind::of(path) == sleuth_core::kinds::FileKind::Pdf
}

// ---- handlers ----------------------------------------------------------------

#[derive(Serialize)]
struct Health {
    status: &'static str,
    version: &'static str,
}

async fn health() -> Json<Health> {
    Json(Health { status: "ok", version: env!("CARGO_PKG_VERSION") })
}

async fn capabilities(State(st): State<Arc<AppState>>, ConnectInfo(peer): ConnectInfo<SocketAddr>, uri: Uri, h: HeaderMap) -> Reply<Capabilities> {
    let c = caller(&st, &h, peer, &uri)?;
    let engine = Arc::clone(&st.engine);
    let syncing = blocking(move || Ok(engine.syncing())).await?;
    Ok(Json(Capabilities {
        api_version: API_VERSION,
        server_version: env!("CARGO_PKG_VERSION").into(),
        user: c.name,
        role: c.role.as_str().into(),
        projects: c.projects.iter().map(|p| p.name.clone()).collect(),
        roots: c.roots,
        syncing,
    }))
}

async fn projects(State(st): State<Arc<AppState>>, ConnectInfo(peer): ConnectInfo<SocketAddr>, uri: Uri, h: HeaderMap) -> Reply<Vec<Project>> {
    let c = caller(&st, &h, peer, &uri)?;
    Ok(Json(c.projects))
}

async fn search(State(st): State<Arc<AppState>>, ConnectInfo(peer): ConnectInfo<SocketAddr>, uri: Uri, h: HeaderMap, q: Result<Query<Params>, QueryRejection>) -> Reply<SearchReply> {
    let started = Instant::now();
    let c = caller(&st, &h, peer, &uri)?;
    let p = params(q)?;
    let query = text(&p, "q", MAX_QUERY)?.unwrap_or("").to_string();
    let limit = number(&p, "limit", 50, 1, 200)? as usize;
    let offset = number(&p, "offset", 0, 0, 5000)? as usize;
    let roots = scoped_roots(&st, &c, text(&p, "project", 200)?, "search")?;
    if query.is_empty() || roots.is_empty() {
        return Ok(Json(SearchReply { hits: Vec::new(), more: false, millis: 0 }));
    }
    let _slot = st.expensive(&c, false)?;
    let engine = Arc::clone(&st.engine);
    let all = blocking(move || Ok(engine.search_hits(&query, &roots)?)).await?;
    let allowed: Vec<_> = all.iter().filter(|h| path_allowed(&h.path, &c.roots)).collect();
    let more = allowed.len() > offset + limit;
    let hits = allowed.into_iter().skip(offset).take(limit).cloned().collect();
    Ok(Json(SearchReply { hits, more, millis: started.elapsed().as_millis() as u64 }))
}

async fn lookup(State(st): State<Arc<AppState>>, ConnectInfo(peer): ConnectInfo<SocketAddr>, uri: Uri, h: HeaderMap, q: Result<Query<Params>, QueryRejection>) -> Reply<DocLookup> {
    let c = caller(&st, &h, peer, &uri)?;
    let p = params(q)?;
    let path = text(&p, "path", 1000)?.ok_or_else(|| ApiErr::bad("path is needed"))?.to_string();
    let path = st.engine.stored_path(&path);
    if !path_allowed(&path, &c.roots) {
        st.audit.record(&c.name, "lookup", &path, "forbidden: outside the user's projects");
        return Err(ApiErr::forbidden("you are not allowed to see this document"));
    }
    let engine = Arc::clone(&st.engine);
    blocking(move || {
        let id = engine.file_id(&path)?.ok_or_else(|| ApiErr::not_found("that file is not in the office index"))?;
        // The index's own spelling of the path.
        let stored = engine.file_path(id)?.ok_or_else(|| ApiErr::not_found("that file is not in the office index"))?;
        if !path_allowed(&stored, &c.roots) {
            return Err(ApiErr::forbidden("you are not allowed to see this document"));
        }
        Ok(Json(DocLookup { id, path: stored }))
    })
    .await
}

async fn document(State(st): State<Arc<AppState>>, ConnectInfo(peer): ConnectInfo<SocketAddr>, uri: Uri, h: HeaderMap, id: Result<Path<String>, PathRejection>) -> Reply<DocReply> {
    let c = caller(&st, &h, peer, &uri)?;
    let id = id_of(id)?;
    let path = doc_path(&st, &c, id, "document")?;
    let engine = Arc::clone(&st.engine);
    blocking(move || {
        let (metadata, pages, provenance, tags) = engine.doc_detail(&path).map_err(|_| ApiErr::not_found("no such document"))?;
        let name = path.rsplit('\\').next().unwrap_or(&path).to_string();
        Ok(Json(DocReply { id, path, name, metadata, pages, provenance, tags }))
    })
    .await
}

async fn page(
    State(st): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    uri: Uri,
    h: HeaderMap,
    ids: Result<Path<(String, String)>, PathRejection>,
    q: Result<Query<Params>, QueryRejection>,
) -> Reply<PageReply> {
    let c = caller(&st, &h, peer, &uri)?;
    let Path((id, n)) = ids.map_err(|e| ApiErr::bad(format!("the request is not understood: {e}")))?;
    let id = id.parse::<i64>().ok().filter(|n| *n > 0).ok_or_else(|| ApiErr::bad("a document id is a positive whole number"))?;
    let n = n.parse::<u32>().ok().filter(|n| *n >= 1).ok_or_else(|| ApiErr::bad("pages are numbered from 1"))?;
    let p = params(q)?;
    let query = text(&p, "q", MAX_QUERY)?.unwrap_or("").to_string();
    let tag = text(&p, "tag", 100)?.map(str::to_string);
    let max_px = number(&p, "max_px", 1600, 400, 3000)? as u32;
    let path = doc_path(&st, &c, id, "page")?;
    if !is_pdf(&path) {
        return Err(ApiErr::bad("only PDF documents have page images"));
    }
    let _slot = st.expensive(&c, true)?;
    let engine = Arc::clone(&st.engine);
    blocking(move || match engine.render_page(&path, n, &query, max_px, tag.as_deref())? {
        Event::Preview { png_base64, width, height, pages, boxes, source } => Ok(Json(PageReply { png_base64, width, height, pages, boxes, source })),
        _ => Err(ApiErr::unavailable("the page could not be drawn")),
    })
    .await
}

#[derive(Serialize)]
struct DocTag {
    norm: String,
    category: String,
    raw: String,
    page: u32,
    conf: f32,
    source: TextSource,
    bbox: NormBox,
}

async fn document_tags(State(st): State<Arc<AppState>>, ConnectInfo(peer): ConnectInfo<SocketAddr>, uri: Uri, h: HeaderMap, id: Result<Path<String>, PathRejection>) -> Reply<Vec<DocTag>> {
    let c = caller(&st, &h, peer, &uri)?;
    let id = id_of(id)?;
    let path = doc_path(&st, &c, id, "document tags")?;
    let engine = Arc::clone(&st.engine);
    blocking(move || {
        let rows = engine.doc_tags(&path).map_err(|_| ApiErr::not_found("no such document"))?;
        Ok(Json(rows.into_iter().map(|(norm, category, raw, page, conf, source, bbox)| DocTag { norm, category, raw, page, conf, source, bbox }).collect()))
    })
    .await
}

async fn related(
    State(st): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    uri: Uri,
    h: HeaderMap,
    id: Result<Path<String>, PathRejection>,
) -> Reply<Vec<sleuth_core::protocol::Related>> {
    let c = caller(&st, &h, peer, &uri)?;
    let id = id_of(id)?;
    let path = doc_path(&st, &c, id, "related")?;
    let engine = Arc::clone(&st.engine);
    blocking(move || {
        let items = engine.related_in(&path, &sleuth_engine::store::Scope::roots(&c.roots)).map_err(|e| ApiErr::bad(format!("{e:#}")))?;
        // Limited in SQL to the user's folders; checked again here.
        Ok(Json(items.into_iter().filter(|r| path_allowed(&r.path, &c.roots)).collect()))
    })
    .await
}

#[derive(Serialize)]
struct TagsReply {
    tags: Vec<sleuth_core::protocol::TagSummary>,
    total: u64,
}

async fn tags(State(st): State<Arc<AppState>>, ConnectInfo(peer): ConnectInfo<SocketAddr>, uri: Uri, h: HeaderMap, q: Result<Query<Params>, QueryRejection>) -> Reply<TagsReply> {
    let c = caller(&st, &h, peer, &uri)?;
    let p = params(q)?;
    let category = text(&p, "category", 40)?.map(str::to_string);
    let contains = text(&p, "contains", 100)?.map(str::to_string);
    let limit = number(&p, "limit", 200, 1, 2000)? as usize;
    let offset = number(&p, "offset", 0, 0, 1_000_000)? as usize;
    let roots = scoped_roots(&st, &c, text(&p, "project", 200)?, "tags")?;
    if roots.is_empty() {
        return Ok(Json(TagsReply { tags: Vec::new(), total: 0 }));
    }
    let engine = Arc::clone(&st.engine);
    blocking(move || {
        let (tags, total) = engine.tags_in(category.as_deref(), contains.as_deref(), &Scope::roots(&roots), limit, offset)?;
        Ok(Json(TagsReply { tags, total }))
    })
    .await
}

async fn occurrences(
    State(st): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    uri: Uri,
    h: HeaderMap,
    norm: Result<Path<String>, PathRejection>,
    q: Result<Query<Params>, QueryRejection>,
) -> Reply<Vec<sleuth_core::protocol::Occurrence>> {
    let c = caller(&st, &h, peer, &uri)?;
    let Path(norm) = norm.map_err(|e| ApiErr::bad(format!("the request is not understood: {e}")))?;
    if norm.trim().is_empty() || norm.chars().count() > 100 {
        return Err(ApiErr::bad("a tag is 1 to 100 characters"));
    }
    let p = params(q)?;
    let roots = scoped_roots(&st, &c, text(&p, "project", 200)?, "occurrences")?;
    if roots.is_empty() {
        return Ok(Json(Vec::new()));
    }
    let engine = Arc::clone(&st.engine);
    blocking(move || {
        let items = engine.occurrences_in(&norm, &Scope::roots(&roots))?;
        Ok(Json(items.into_iter().filter(|o| path_allowed(&o.path, &c.roots)).collect()))
    })
    .await
}

async fn revisions(
    State(st): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    uri: Uri,
    h: HeaderMap,
    q: Result<Query<Params>, QueryRejection>,
) -> Reply<Vec<sleuth_core::protocol::RevisionGroup>> {
    let c = caller(&st, &h, peer, &uri)?;
    let p = params(q)?;
    let contains = text(&p, "contains", 100)?.map(str::to_string);
    let limit = number(&p, "limit", 200, 1, 2000)? as usize;
    let roots = scoped_roots(&st, &c, text(&p, "project", 200)?, "revisions")?;
    if roots.is_empty() {
        return Ok(Json(Vec::new()));
    }
    let engine = Arc::clone(&st.engine);
    blocking(move || {
        let mut groups = engine.revisions_in(contains.as_deref(), &Scope::roots(&roots), limit)?;
        for g in &mut groups {
            g.docs.retain(|d| path_allowed(&d.path, &c.roots));
        }
        groups.retain(|g| !g.docs.is_empty());
        Ok(Json(groups))
    })
    .await
}

async fn compare(State(st): State<Arc<AppState>>, ConnectInfo(peer): ConnectInfo<SocketAddr>, uri: Uri, h: HeaderMap, q: Result<Query<Params>, QueryRejection>) -> Reply<CompareReply> {
    let c = caller(&st, &h, peer, &uri)?;
    let p = params(q)?;
    let id = |k: &str| -> Result<i64, ApiErr> { p.get(k).and_then(|v| v.trim().parse::<i64>().ok()).filter(|n| *n > 0).ok_or_else(|| ApiErr::bad(format!("{k} must be a document id"))) };
    let (old, new) = (id("old")?, id("new")?);
    let (old_path, new_path) = (doc_path(&st, &c, old, "compare")?, doc_path(&st, &c, new, "compare")?);
    let engine = Arc::clone(&st.engine);
    blocking(move || {
        let (old, new, changes, unchanged) = engine.compare_paths(&old_path, &new_path).map_err(|e| ApiErr::bad(format!("{e:#}")))?;
        Ok(Json(CompareReply { old, new, changes, unchanged }))
    })
    .await
}

#[derive(Serialize)]
struct Job {
    id: i64,
    path: String,
    reason: String,
}

async fn jobs(State(st): State<Arc<AppState>>, ConnectInfo(peer): ConnectInfo<SocketAddr>, uri: Uri, h: HeaderMap, q: Result<Query<Params>, QueryRejection>) -> Reply<Vec<Job>> {
    let c = caller(&st, &h, peer, &uri)?;
    let p = params(q)?;
    let failed = match p.get("state").map(|s| s.trim()) {
        None | Some("") | Some("waiting") => false,
        Some("failed") => true,
        Some(_) => return Err(ApiErr::bad("state is waiting or failed")),
    };
    let limit = number(&p, "limit", 200, 1, 1000)? as usize;
    let engine = Arc::clone(&st.engine);
    blocking(move || {
        let rows = engine.jobs(failed, limit)?;
        Ok(Json(rows.into_iter().filter(|(_, path, _)| path_allowed(path, &c.roots)).map(|(id, path, reason)| Job { id, path, reason }).collect()))
    })
    .await
}

#[derive(Serialize)]
struct Started {
    status: &'static str,
}

async fn scan(State(st): State<Arc<AppState>>, ConnectInfo(peer): ConnectInfo<SocketAddr>, uri: Uri, h: HeaderMap) -> Result<(StatusCode, Json<Started>), ApiErr> {
    let c = caller(&st, &h, peer, &uri)?;
    admin(&st, &c, "scan")?;
    st.engine.rescan_now(None);
    st.audit.record(&c.name, "scan", "all project folders", "started");
    Ok((StatusCode::ACCEPTED, Json(Started { status: "started" })))
}

#[derive(Serialize)]
struct Cancelled {
    cancelled: bool,
}

async fn cancel(State(st): State<Arc<AppState>>, ConnectInfo(peer): ConnectInfo<SocketAddr>, uri: Uri, h: HeaderMap, id: Result<Path<String>, PathRejection>) -> Reply<Cancelled> {
    let c = caller(&st, &h, peer, &uri)?;
    admin(&st, &c, "cancel job")?;
    let id = id_of(id)?;
    let path = doc_path(&st, &c, id, "cancel job")?;
    let engine = Arc::clone(&st.engine);
    let cancelled = blocking(move || Ok(engine.cancel_job(id)?)).await?;
    st.audit.record(&c.name, "cancel job", &path, if cancelled { "cancelled" } else { "not waiting" });
    Ok(Json(Cancelled { cancelled }))
}

async fn monitor(State(st): State<Arc<AppState>>, ConnectInfo(peer): ConnectInfo<SocketAddr>, uri: Uri, h: HeaderMap) -> Reply<sleuth_core::protocol::Monitor> {
    let c = caller(&st, &h, peer, &uri)?;
    admin(&st, &c, "monitor")?;
    let engine = Arc::clone(&st.engine);
    blocking(move || Ok(Json(engine.monitor()?))).await
}

async fn audit_log(State(st): State<Arc<AppState>>, ConnectInfo(peer): ConnectInfo<SocketAddr>, uri: Uri, h: HeaderMap, q: Result<Query<Params>, QueryRejection>) -> Reply<Vec<crate::audit::Row>> {
    let c = caller(&st, &h, peer, &uri)?;
    admin(&st, &c, "audit")?;
    let limit = number(&params(q)?, "limit", 200, 1, 1000)? as usize;
    Ok(Json(st.audit.recent(limit)?))
}

/// `POST /join`: no access key; the join code from the office index folder
/// instead. Answers with the PC's new key.
async fn join(State(st): State<Arc<AppState>>, ConnectInfo(peer): ConnectInfo<SocketAddr>, body: Result<Json<JoinRequest>, axum::extract::rejection::JsonRejection>) -> Reply<JoinReply> {
    let Json(req) = body.map_err(|e| ApiErr::bad(format!("the request is not understood: {e}")))?;
    blocking(move || st.join(&req, peer)).await.map(Json)
}

async fn no_route() -> ApiErr {
    ApiErr::not_found("no such address in the office server API (v1)")
}

pub fn router(state: Arc<AppState>) -> Router {
    let v1 = Router::new()
        .route("/health", get(health))
        .route("/join", post(join))
        .route("/capabilities", get(capabilities))
        .route("/projects", get(projects))
        .route("/search", get(search))
        .route("/documents", get(lookup))
        .route("/documents/{id}", get(document))
        .route("/documents/{id}/pages/{n}", get(page))
        .route("/documents/{id}/tags", get(document_tags))
        .route("/documents/{id}/related", get(related))
        .route("/tags", get(tags))
        .route("/tags/{norm}/occurrences", get(occurrences))
        .route("/revisions", get(revisions))
        .route("/compare", get(compare))
        .route("/jobs", get(jobs))
        .route("/jobs/{id}/cancel", post(cancel))
        .route("/indexing/scans", post(scan))
        .route("/monitor", get(monitor))
        .route("/audit", get(audit_log));
    Router::new().nest("/api/v1", v1).fallback(no_route).with_state(state)
}
