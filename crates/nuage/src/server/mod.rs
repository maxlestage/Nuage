//! The Nuage DB server: the database over HTTP(S), its live status and its
//! management console, on one port.
//!
//! | Path | What | Access |
//! |---|---|---|
//! | `/v1/*` | the database protocol (`memoire_orm::nuage::wire`) | `Authorization: Bearer $NUAGE_TOKEN` |
//! | `/api/health` | `{"status":"ok"}` | public |
//! | `/api/status`, `/api/status/stream` | live status, as JSON and as Server-Sent Events | token or console session; a public subset with `public_status` |
//! | `/admin` | the management console | `NUAGE_ADMIN_PASSWORD` |
//! | anything else (`GET`) | the site handler, if any, else a small home page | public |

mod admin;
pub(crate) mod query;
mod status;

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use http_body_util::{BodyExt, Full, Limited, combinators::BoxBody};
use hyper::body::{Body, Bytes, Frame, Incoming};
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use memoire_orm::nuage::wire::{self, SchemaCache};
use memoire_orm::nuage::{BRANCH_HEADER, TRANSACTION_HEADER};
use memoire_orm::{Error, Result};
use serde_json::{Value as Json, json};
use tokio::net::TcpListener;
use tokio::sync::{Notify, mpsc};

use crate::embedded::{self, Embedded, EmbeddedTransaction};
use crate::engine::{Engine, MAIN};

pub(crate) type HttpBody = BoxBody<Bytes, Infallible>;

/// A `GET` request handed to the [`SiteHandler`].
#[derive(Clone, Debug)]
pub struct SiteRequest {
    /// Path (`/`, `/app`, `/static/app.css`...).
    pub path: String,
    /// Query string, without `?`.
    pub query: Option<String>,
    /// Request headers (names in lower case).
    pub headers: Vec<(String, String)>,
}

/// The answer of a [`SiteHandler`].
#[derive(Clone, Debug)]
pub struct SiteResponse {
    /// HTTP status.
    pub status: u16,
    /// `Content-Type`.
    pub content_type: String,
    /// Other headers (`Cache-Control`...).
    pub headers: Vec<(String, String)>,
    /// Body.
    pub body: Vec<u8>,
}

/// Serves a website from the same port as the database: tried first for
/// every `GET` outside `/api`, `/admin` and `/v1`. Returning `None` falls back
/// to the server's own pages.
pub trait SiteHandler: Send + Sync {
    /// The answer to `request`, or `None` to let the server answer.
    fn handle(&self, request: &SiteRequest) -> Option<SiteResponse>;
}

impl<F> SiteHandler for F
where
    F: Fn(&SiteRequest) -> Option<SiteResponse> + Send + Sync,
{
    fn handle(&self, request: &SiteRequest) -> Option<SiteResponse> {
        self(request)
    }
}

/// Settings of a [`Server`].
#[derive(Clone)]
pub struct ServerOptions {
    /// Token of the database protocol and of the status API
    /// (`NUAGE_TOKEN`). Without it, `/v1` refuses every request.
    pub token: Option<String>,
    /// Password of the console (`NUAGE_ADMIN_PASSWORD`). Without it, the
    /// console is off.
    pub admin_password: Option<String>,
    /// Status without credentials: the public subset (`NUAGE_PUBLIC_STATUS`).
    pub public_status: bool,
    /// An interactive transaction left without a request this long is
    /// rolled back. Default: 30 seconds.
    pub transaction_timeout: Duration,
    /// Largest request body. Default: 16 MiB.
    pub max_body_bytes: usize,
    /// The website served next to the database, if any.
    pub site: Option<Arc<dyn SiteHandler>>,
}

impl std::fmt::Debug for ServerOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerOptions")
            .field("token", &self.token.as_ref().map(|_| "***"))
            .field("admin_password", &self.admin_password.as_ref().map(|_| "***"))
            .field("public_status", &self.public_status)
            .field("site", &self.site.is_some())
            .finish()
    }
}

impl Default for ServerOptions {
    fn default() -> Self {
        ServerOptions {
            token: None,
            admin_password: None,
            public_status: false,
            transaction_timeout: Duration::from_secs(30),
            max_body_bytes: 16 << 20,
            site: None,
        }
    }
}

/// Request counters for the live status.
pub(crate) struct Metrics {
    total: AtomicU64,
    errors: AtomicU64,
    /// Requests and errors per second, over the last minute: `(second, n, errors)`.
    seconds: Mutex<Vec<(u64, u64, u64)>>,
    /// The last latencies, in microseconds.
    latencies: Mutex<Vec<u64>>,
    latency_at: AtomicU64,
    pub(crate) open_connections: AtomicU64,
    pub(crate) active_requests: AtomicU64,
}

const LATENCY_SAMPLES: usize = 2048;

impl Metrics {
    fn new() -> Self {
        Metrics {
            total: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            seconds: Mutex::new(Vec::new()),
            latencies: Mutex::new(Vec::with_capacity(LATENCY_SAMPLES)),
            latency_at: AtomicU64::new(0),
            open_connections: AtomicU64::new(0),
            active_requests: AtomicU64::new(0),
        }
    }

    fn record(&self, elapsed: Duration, error: bool) {
        self.total.fetch_add(1, Ordering::Relaxed);
        if error {
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
        let now = crate::engine::store::now_millis() as u64 / 1000;
        {
            let mut s = self.seconds.lock().unwrap_or_else(|e| e.into_inner());
            match s.last_mut() {
                Some(last) if last.0 == now => {
                    last.1 += 1;
                    last.2 += u64::from(error);
                }
                _ => s.push((now, 1, u64::from(error))),
            }
            s.retain(|(t, _, _)| now - t < 60);
        }
        let micros = elapsed.as_micros() as u64;
        let mut l = self.latencies.lock().unwrap_or_else(|e| e.into_inner());
        if l.len() < LATENCY_SAMPLES {
            l.push(micros);
        } else {
            let at = self.latency_at.fetch_add(1, Ordering::Relaxed) as usize % LATENCY_SAMPLES;
            l[at] = micros;
        }
    }

    /// Requests and errors per second over the last ten full seconds.
    pub(crate) fn rates(&self) -> (f64, f64) {
        let now = crate::engine::store::now_millis() as u64 / 1000;
        let s = self.seconds.lock().unwrap_or_else(|e| e.into_inner());
        let (n, e) =
            s.iter().filter(|(t, _, _)| *t < now && now - t <= 10).fold((0, 0), |(n, e), (_, a, b)| (n + a, e + b));
        (n as f64 / 10.0, e as f64 / 10.0)
    }

    /// p50 and p99 latencies, in milliseconds.
    pub(crate) fn percentiles(&self) -> (f64, f64) {
        let mut l = self.latencies.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if l.is_empty() {
            return (0.0, 0.0);
        }
        l.sort_unstable();
        let at = |p: f64| l[((l.len() - 1) as f64 * p).round() as usize] as f64 / 1000.0;
        (at(0.50), at(0.99))
    }

    pub(crate) fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }
}

struct OpenTx {
    tx: EmbeddedTransaction,
    branch: String,
    last_used: Mutex<Instant>,
}

pub(crate) struct State {
    pub(crate) engine: Arc<Engine>,
    pub(crate) options: ServerOptions,
    pub(crate) metrics: Metrics,
    pub(crate) schemas: SchemaCache,
    transactions: Mutex<HashMap<String, Arc<OpenTx>>>,
    pub(crate) admin: admin::Sessions,
}

/// A Nuage DB server. See the [module](self) docs.
///
/// ```no_run
/// # async fn demo() -> memoire_orm::Result<()> {
/// use nuage::{Engine, EngineOptions, Server, ServerOptions};
/// let engine = Engine::open("nuage-data", EngineOptions::default())?;
/// let options = ServerOptions { token: Some("s3cret".into()), ..ServerOptions::default() };
/// let running = Server::new(engine, options).listen("0.0.0.0:8080").await?;
/// running.wait().await;
/// # Ok(()) }
/// ```
#[derive(Clone)]
pub struct Server {
    state: Arc<State>,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server").field("engine", &self.state.engine).field("options", &self.state.options).finish()
    }
}

/// A server accepting connections, from [`Server::listen`].
#[derive(Debug)]
pub struct RunningServer {
    addr: SocketAddr,
    stop: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl RunningServer {
    /// The address it listens on.
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// `http://<address>`.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Stops accepting connections.
    pub async fn shutdown(self) {
        self.stop.notify_one();
        let _ = self.task.await;
    }

    /// Serves until the process stops.
    pub async fn wait(self) {
        let _ = self.task.await;
    }
}

impl Server {
    /// A server for `engine`.
    pub fn new(engine: Arc<Engine>, options: ServerOptions) -> Server {
        Server {
            state: Arc::new(State {
                engine,
                options,
                metrics: Metrics::new(),
                schemas: SchemaCache::default(),
                transactions: Mutex::new(HashMap::new()),
                admin: admin::Sessions::default(),
            }),
        }
    }

    /// The engine it serves.
    pub fn engine(&self) -> &Arc<Engine> {
        &self.state.engine
    }

    /// The live status, as `/api/status` answers it to an authenticated
    /// client.
    pub fn status(&self) -> Json {
        status::status(&self.state, true)
    }

    /// Binds `addr` and serves in the background on the current Tokio
    /// runtime.
    pub async fn listen(&self, addr: impl tokio::net::ToSocketAddrs) -> Result<RunningServer> {
        let listener = TcpListener::bind(addr).await.map_err(|e| Error::Pool(format!("cannot listen: {e}")))?;
        let addr = listener.local_addr().map_err(|e| Error::Pool(e.to_string()))?;
        let stop = Arc::new(Notify::new());
        let state = self.state.clone();
        let stopped = stop.clone();
        let task = tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = stopped.notified() => break,
                    _ = tick.tick() => state.expire_transactions(),
                    accepted = listener.accept() => {
                        let Ok((stream, peer)) = accepted else { continue };
                        let _ = stream.set_nodelay(true);
                        let state = state.clone();
                        tokio::spawn(async move {
                            state.metrics.open_connections.fetch_add(1, Ordering::Relaxed);
                            let conn_state = state.clone();
                            let service = service_fn(move |req| {
                                let state = conn_state.clone();
                                async move { Ok::<_, Infallible>(handle(state, req, peer).await) }
                            });
                            let _ = http1::Builder::new().serve_connection(TokioIo::new(stream), service).await;
                            state.metrics.open_connections.fetch_sub(1, Ordering::Relaxed);
                        });
                    }
                }
            }
        });
        Ok(RunningServer { addr, stop, task })
    }
}

impl State {
    fn expire_transactions(&self) {
        let timeout = self.options.transaction_timeout;
        let mut txs = self.transactions.lock().unwrap_or_else(|e| e.into_inner());
        txs.retain(|_, tx| tx.last_used.lock().unwrap_or_else(|e| e.into_inner()).elapsed() < timeout);
    }

    pub(crate) fn open_transactions(&self) -> usize {
        self.transactions.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub(crate) fn db(&self, branch: &str) -> Result<Embedded> {
        Embedded::from_engine(self.engine.clone()).on_branch(branch)
    }
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

pub(crate) fn full(body: impl Into<Bytes>) -> HttpBody {
    Full::new(body.into()).boxed()
}

pub(crate) fn json_response(status: u16, body: &Json) -> Response<HttpBody> {
    let mut res = Response::new(full(body.to_string()));
    *res.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

pub(crate) fn error_response(e: &Error) -> Response<HttpBody> {
    let (status, body) = wire::error_to_json(e);
    json_response(status, &body)
}

pub(crate) fn unauthorized(message: &str) -> Response<HttpBody> {
    json_response(401, &json!({ "error": { "kind": "unauthorized", "message": message } }))
}

/// Compares in constant time: the answer's timing tells nothing about how
/// much of a secret was right.
pub(crate) fn same_secret(a: &[u8], b: &[u8]) -> bool {
    // Digests first: equal lengths, so not even the length leaks.
    let (x, y) = (ring::digest::digest(&ring::digest::SHA256, a), ring::digest::digest(&ring::digest::SHA256, b));
    x.as_ref().iter().zip(y.as_ref()).fold(0u8, |acc, (p, q)| acc | (p ^ q)) == 0
}

pub(crate) fn random_token() -> String {
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes).expect("the operating system has no random source");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ").map(str::trim)
}

impl State {
    /// The request carries the token.
    pub(crate) fn has_token(&self, headers: &HeaderMap) -> bool {
        match (&self.options.token, bearer(headers)) {
            (Some(t), Some(given)) => same_secret(t.as_bytes(), given.as_bytes()),
            _ => false,
        }
    }
}

async fn read_body(state: &State, req: Request<Incoming>) -> Result<Bytes> {
    Ok(Limited::new(req.into_body(), state.options.max_body_bytes)
        .collect()
        .await
        .map_err(|e| Error::Validation(format!("cannot read the request: {e}")))?
        .to_bytes())
}

async fn read_json(state: &State, req: Request<Incoming>) -> Result<Json> {
    let body = read_body(state, req).await?;
    if body.is_empty() {
        return Ok(Json::Null);
    }
    serde_json::from_slice(&body).map_err(|e| Error::Conversion(format!("invalid JSON: {e}")))
}

/// A body fed by a channel: Server-Sent Events.
pub(crate) struct ChannelBody(pub mpsc::Receiver<Bytes>);

impl Body for ChannelBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<Frame<Bytes>, Infallible>>> {
        self.0.poll_recv(cx).map(|b| b.map(|b| Ok(Frame::data(b))))
    }
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

async fn handle(state: Arc<State>, req: Request<Incoming>, peer: SocketAddr) -> Response<HttpBody> {
    let started = Instant::now();
    state.metrics.active_requests.fetch_add(1, Ordering::Relaxed);
    let path = req.uri().path().to_owned();
    let mut res = route(&state, req, peer).await;
    state.metrics.active_requests.fetch_sub(1, Ordering::Relaxed);
    // The status stream would count as one never-ending request.
    if path != "/api/status/stream" {
        state.metrics.record(started.elapsed(), res.status().is_server_error());
    }
    let h = res.headers_mut();
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert(header::SERVER, HeaderValue::from_static("Nuage DB"));
    res
}

async fn route(state: &Arc<State>, req: Request<Incoming>, peer: SocketAddr) -> Response<HttpBody> {
    let path = req.uri().path().to_owned();
    let method = req.method().clone();
    if path == "/api/health" {
        return json_response(200, &json!({ "status": "ok" }));
    }
    if path == "/api/status" || path == "/api/status/stream" {
        return status::route(state, req, &path).await;
    }
    if path == "/admin" || path.starts_with("/admin/") {
        return admin::route(state, req, peer).await;
    }
    if path.starts_with("/v1/") {
        return match protocol(state, req).await {
            Ok(res) => res,
            Err(e) => error_response(&e),
        };
    }
    if method == Method::GET || method == Method::HEAD {
        if let Some(site) = &state.options.site {
            let request = SiteRequest {
                path: path.clone(),
                query: req.uri().query().map(str::to_owned),
                headers: req
                    .headers()
                    .iter()
                    .filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned())))
                    .collect(),
            };
            if let Some(answer) = site.handle(&request) {
                let mut res = Response::new(full(answer.body));
                *res.status_mut() = StatusCode::from_u16(answer.status).unwrap_or(StatusCode::OK);
                if let Ok(v) = HeaderValue::from_str(&answer.content_type) {
                    res.headers_mut().insert(header::CONTENT_TYPE, v);
                }
                for (k, v) in answer.headers {
                    if let (Ok(k), Ok(v)) = (header::HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(&v)) {
                        res.headers_mut().insert(k, v);
                    }
                }
                return res;
            }
        }
        if path == "/" {
            return admin::home_page(state);
        }
    }
    json_response(404, &json!({ "error": { "kind": "not_found", "message": format!("no such page: {path}") } }))
}

/// `/v1/*`: the database protocol.
async fn protocol(state: &Arc<State>, req: Request<Incoming>) -> Result<Response<HttpBody>> {
    if state.options.token.is_none() {
        return Ok(unauthorized("the database API is off: set NUAGE_TOKEN on the server"));
    }
    if !state.has_token(req.headers()) {
        return Ok(unauthorized("missing or invalid bearer token"));
    }
    let method = req.method().clone();
    let path = req.uri().path().trim_end_matches('/').to_owned();
    let segments: Vec<&str> = path.trim_start_matches("/v1/").split('/').collect();
    let header = |name: &str| req.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let branch = header(BRANCH_HEADER).unwrap_or_else(|| MAIN.to_owned());
    let tx_id = header(TRANSACTION_HEADER);
    let engine = &state.engine;

    match (method, segments.as_slice()) {
        (Method::GET, ["info"]) => Ok(json_response(
            200,
            &json!({ "name": "Nuage DB", "version": crate::VERSION, "database": engine.options().name }),
        )),
        (Method::POST, [op @ ("fetch" | "execute")]) => {
            let fetch = *op == "fetch";
            let body = read_json(state, req).await?;
            let query = wire::query_from_json(body.get("query").unwrap_or(&Json::Null), &state.schemas)?;
            let engine = engine.clone();
            let tx = match &tx_id {
                Some(id) => Some(state.transaction(id, &branch)?),
                None => None,
            };
            // The engine blocks on disk: off the async workers.
            let answer = tokio::task::spawn_blocking(move || -> Result<Json> {
                let q = query.as_query();
                match (tx, fetch) {
                    (Some(tx), true) => Ok(json!({ "rows": wire::rows_to_json(&tx.tx.fetch_rows(&q)?) })),
                    (Some(tx), false) => Ok(wire::exec_result_to_json(&tx.tx.execute_now(&q)?)),
                    (None, true) => {
                        check_branch(&engine, &branch)?;
                        Ok(json!({ "rows": wire::rows_to_json(&embedded::fetch_rows(&engine, &branch, &q)?) }))
                    }
                    (None, false) => {
                        check_branch(&engine, &branch)?;
                        Ok(wire::exec_result_to_json(&embedded::execute_now(&engine, &branch, &q)?))
                    }
                }
            })
            .await
            .map_err(|e| Error::Pool(e.to_string()))??;
            Ok(json_response(200, &answer))
        }
        (Method::POST, ["batch"]) => {
            let body = read_json(state, req).await?;
            let mut statements = Vec::new();
            for s in body
                .get("statements")
                .and_then(Json::as_array)
                .ok_or_else(|| Error::Conversion("missing `statements`".into()))?
            {
                let (fetch, q) = match (s.get("fetch"), s.get("execute")) {
                    (Some(q), _) => (true, q),
                    (_, Some(q)) => (false, q),
                    _ => {
                        return Err(Error::Conversion(
                            "a statement is `{\"fetch\": ...}` or `{\"execute\": ...}`".into(),
                        ));
                    }
                };
                statements.push((fetch, wire::query_from_json(q, &state.schemas)?));
            }
            let db = state.db(&branch)?;
            let results = tokio::task::spawn_blocking(move || {
                let list: Vec<(bool, memoire_orm::Query<'_>)> =
                    statements.iter().map(|(f, q)| (*f, q.as_query())).collect();
                db.batch(&list)
            })
            .await
            .map_err(|e| Error::Pool(e.to_string()))??;
            let results: Vec<Json> = results
                .into_iter()
                .map(|r| match r {
                    Ok(rows) => json!({ "rows": wire::rows_to_json(&rows) }),
                    Err(done) => wire::exec_result_to_json(&done),
                })
                .collect();
            Ok(json_response(200, &json!({ "results": results })))
        }
        (Method::POST, ["transactions"]) => {
            let tx = state.db(&branch)?.begin_now()?;
            let id = random_token();
            let open = OpenTx { tx, branch, last_used: Mutex::new(Instant::now()) };
            state.transactions.lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone(), Arc::new(open));
            Ok(json_response(200, &json!({ "id": id })))
        }
        (Method::POST, ["transactions", id, action @ ("commit" | "rollback")]) => {
            let open = state
                .transactions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(*id)
                .ok_or(Error::TransactionFinished)?;
            let commit = *action == "commit";
            tokio::task::spawn_blocking(move || if commit { open.tx.commit_now() } else { open.tx.rollback_now() })
                .await
                .map_err(|e| Error::Pool(e.to_string()))??;
            Ok(json_response(200, &json!({ "ok": true })))
        }
        (Method::GET, ["branches"]) => {
            let list: Vec<Json> = engine.branches().iter().map(memoire_orm::nuage::Branch::to_json).collect();
            Ok(json_response(200, &json!({ "branches": list })))
        }
        (Method::POST, ["branches"]) => {
            let body = read_json(state, req).await?;
            let name = body.get("name").and_then(Json::as_str).unwrap_or_default();
            let from = body.get("from").and_then(Json::as_str);
            Ok(json_response(201, &engine.create_branch(name, from)?.to_json()))
        }
        (Method::DELETE, ["branches", name]) => {
            if engine.delete_branch(name)? {
                Ok(json_response(200, &json!({ "deleted": name })))
            } else {
                Err(Error::NotFound)
            }
        }
        (Method::POST, ["branches", name, "reset"]) => {
            if engine.reset_branch(name)? {
                Ok(json_response(200, &json!({ "reset": name })))
            } else {
                Err(Error::NotFound)
            }
        }
        _ => Err(Error::NotFound),
    }
}

fn check_branch(engine: &Engine, branch: &str) -> Result<()> {
    if engine.has_branch(branch) { Ok(()) } else { Err(Error::NotFound) }
}

impl State {
    fn transaction(&self, id: &str, branch: &str) -> Result<Arc<OpenTx>> {
        let tx = self
            .transactions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
            .ok_or(Error::TransactionFinished)?;
        if tx.branch != branch {
            return Err(Error::Validation("this transaction belongs to another branch".into()));
        }
        *tx.last_used.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
        Ok(tx)
    }
}
