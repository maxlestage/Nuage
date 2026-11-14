//! The Nuage DB server: the database over HTTP(S), its live status and its
//! management console, on one port.
//!
//! | Path | What | Access |
//! |---|---|---|
//! | `/v1/*` | the database protocol (`memoire_orm::nuage::wire`) of the application the token opens | `Authorization: Bearer <token of an application>` |
//! | `POST /v1/sql` | SQL with `?` parameters, rows as JSON objects: for websites in any language | the same; a read-only token reads only |
//! | port `NUAGE_PG_PORT` | the PostgreSQL protocol ([`Server::listen_pg`]): every language's PostgreSQL driver | a token of an application as the password |
//! | `GET /v1/ping` | "does my connection work?": the application the token opens, its access, branch and tables | a token of an application |
//! | `GET /v1/openapi.json` | the OpenAPI description of `/v1/sql`, for code generators of every language | public |
//!
//! Websites: a page calls `/v1` from the browser when its origin is one of
//! its application's websites (CORS, preflight cached for a day); JSON
//! answers are gzip-compressed for clients that accept it.
//! | `/api/health` | `{"status":"ok"}` | public |
//! | `/api/status`, `/api/status/stream` | live status, as JSON and as Server-Sent Events | token or console session; a public subset with `public_status` |
//! | `/admin` | the management console | `NUAGE_ADMIN_PASSWORD` |
//! | anything else (`GET`) | the site handler, if any, else a small home page | public |

mod admin;
pub(crate) mod apps;
mod i18n;
mod pg;
mod status;

pub use pg::{PgOptions, tls_config};

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

pub use self::apps::{AppInfo, TokenInfo};
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
    /// A token of the first application, the server's own engine
    /// (`NUAGE_TOKEN`). The console makes the tokens of the other
    /// applications (and more for the first one).
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
    /// Port of the PostgreSQL protocol ([`Server::listen_pg`]), for the
    /// console to show how to connect.
    pub pg_port: Option<u16>,
    /// The PostgreSQL protocol has TLS.
    pub pg_tls: bool,
}

impl std::fmt::Debug for ServerOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerOptions")
            .field("token", &self.token.as_ref().map(|_| "***"))
            .field("admin_password", &self.admin_password.as_ref().map(|_| "***"))
            .field("public_status", &self.public_status)
            .field("site", &self.site.is_some())
            .field("pg_port", &self.pg_port)
            .field("pg_tls", &self.pg_tls)
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
            pg_port: None,
            pg_tls: false,
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
    pub(crate) fn new() -> Self {
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
    app: String,
    branch: String,
    last_used: Mutex<Instant>,
}

pub(crate) struct State {
    /// The first application's engine (the server's own).
    pub(crate) engine: Arc<Engine>,
    pub(crate) apps: apps::Apps,
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
    /// A server for `engine`, its first application, and every other
    /// application `engine` lists (opened from `<engine dir>/apps/<name>`).
    pub fn new(engine: Arc<Engine>, options: ServerOptions) -> Server {
        Server {
            state: Arc::new(State {
                apps: apps::Apps::open(engine.clone()),
                engine,
                options,
                metrics: Metrics::new(),
                schemas: SchemaCache::default(),
                transactions: Mutex::new(HashMap::new()),
                admin: admin::Sessions::default(),
            }),
        }
    }

    /// The engine of the first application (the server's own).
    pub fn engine(&self) -> &Arc<Engine> {
        &self.state.engine
    }

    /// The applications, the first one first.
    pub fn apps(&self) -> Vec<AppInfo> {
        let env = self.state.options.token.is_some();
        self.state.apps.all().iter().map(|a| a.info(env)).collect()
    }

    /// The engine of the application `name`.
    pub fn app_engine(&self, name: &str) -> Option<Arc<Engine>> {
        self.state.apps.get(name).map(|a| a.engine.clone())
    }

    /// Creates an application (a database of its own) and its first token.
    /// The token is returned once: the server keeps only its digest.
    pub fn create_app(&self, name: &str, description: &str) -> Result<(AppInfo, String)> {
        let (app, token) = self.state.apps.create(name, description)?;
        Ok((app.info(false), token))
    }

    /// A new token for the application `name`, returned once.
    pub fn new_app_token(&self, name: &str) -> Result<String> {
        let app = self.state.apps.get(name).ok_or(Error::NotFound)?;
        self.state.apps.new_token(&app, false)
    }

    /// A new read-only token of application `name`: it reads, never writes.
    pub fn new_app_read_only_token(&self, name: &str) -> Result<String> {
        let app = self.state.apps.get(name).ok_or(Error::NotFound)?;
        self.state.apps.new_token(&app, true)
    }

    /// A new read-only token for the application `name` (for the JavaScript
    /// of a website: it reads, never writes), returned once.
    pub fn new_app_read_token(&self, name: &str) -> Result<String> {
        let app = self.state.apps.get(name).ok_or(Error::NotFound)?;
        self.state.apps.new_token(&app, true)
    }

    /// The websites whose pages may call the application `name` from the
    /// browser (CORS): `https://site.fr`, `http://localhost:3000`, or `*`.
    pub fn set_app_origins(&self, name: &str, origins: &[&str]) -> Result<Vec<String>> {
        let app = self.state.apps.get(name).ok_or(Error::NotFound)?;
        self.state.apps.set_origins(&app, &origins.join("\n"))
    }

    /// Revokes the token `id` of the application `name`.
    pub fn revoke_app_token(&self, name: &str, id: &str) -> Result<bool> {
        let app = self.state.apps.get(name).ok_or(Error::NotFound)?;
        self.state.apps.revoke_token(&app, id)
    }

    /// Deletes the application `name`, its data and its backups (not the
    /// first one).
    pub fn delete_app(&self, name: &str) -> Result<bool> {
        self.state.apps.delete(name)
    }

    /// The live status, as `/api/status` answers it to an authenticated
    /// client.
    pub fn status(&self) -> Json {
        status::status(&self.state, &status::View::All)
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

impl Server {
    /// Binds `addr` and serves the PostgreSQL protocol there, in the
    /// background: every language's PostgreSQL driver connects, with a token
    /// of an application as its password (see [`PgOptions`]).
    pub async fn listen_pg(&self, addr: impl tokio::net::ToSocketAddrs, options: PgOptions) -> Result<RunningServer> {
        let listener = TcpListener::bind(addr).await.map_err(|e| Error::Pool(format!("cannot listen: {e}")))?;
        let addr = listener.local_addr().map_err(|e| Error::Pool(e.to_string()))?;
        let stop = Arc::new(Notify::new());
        let state = self.state.clone();
        let stopped = stop.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stopped.notified() => break,
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { continue };
                        let Ok(stream) = stream.into_std() else { continue };
                        if stream.set_nonblocking(false).is_err() {
                            continue;
                        }
                        let (state, options) = (state.clone(), options.clone());
                        // A thread per connection, as PostgreSQL has a process:
                        // its statements block on the engine. Not a runtime
                        // task, so an idle client never delays a shutdown.
                        let _ = std::thread::Builder::new()
                            .name("nuage-pg".into())
                            .spawn(move || pg::serve(state, stream, options));
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

    pub(crate) fn db(&self, engine: &Arc<Engine>, branch: &str) -> Result<Embedded> {
        Embedded::from_engine(engine.clone()).on_branch(branch)
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
    /// The application whose token the request carries, and whether the
    /// token only reads.
    pub(crate) fn app_of(&self, headers: &HeaderMap) -> Option<(Arc<apps::App>, bool)> {
        self.apps.authenticate(bearer(headers)?, self.options.token.as_deref())
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
    let gzip = accepts_gzip(req.headers());
    let mut res = route(&state, req, peer).await;
    if gzip {
        res = compress(res).await;
    }
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

/// The OpenAPI description of `/v1/sql` and `/api/*` (`openapi.json`).
const OPENAPI: &str = include_str!("../../openapi.json");

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
    if path == "/v1/openapi.json" {
        // The description of this API, for code generators of every language.
        let mut res = json_response(200, &json!(null));
        *res.body_mut() = full(OPENAPI);
        res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=3600"));
        res.headers_mut().insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
        return res;
    }
    if path.starts_with("/v1/") {
        let origin = req.headers().get(header::ORIGIN).and_then(|v| v.to_str().ok()).map(str::to_owned);
        if method == Method::OPTIONS {
            return preflight(state, origin.as_deref());
        }
        let (mut res, allowed) = match protocol(state, req, origin.as_deref(), peer).await {
            Ok(answer) => answer,
            Err(e) => (error_response(&e), None),
        };
        if let Some(origin) = allowed.as_deref().or(origin.as_deref().filter(|o| state.apps.any_allows(o))) {
            cors_headers(&mut res, origin);
        }
        return res;
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
            let res = admin::home_page(state, req.headers());
            return admin::prefs(req.headers(), res).await;
        }
    }
    json_response(404, &json!({ "error": { "kind": "not_found", "message": format!("no such page: {path}") } }))
}

// ---------------------------------------------------------------------------
// Websites: CORS and compression
// ---------------------------------------------------------------------------

/// Headers a page may send to `/v1` (those of `memoire_orm::NuageDb`).
const CORS_HEADERS: &str = "authorization, content-type, accept, nuage-branch, nuage-transaction";

/// The answer to a browser's preflight (`OPTIONS`, no token): yes for an
/// origin some application lets in, cached by the browser for a day, so a
/// page pays it once, not before every request.
fn preflight(state: &State, origin: Option<&str>) -> Response<HttpBody> {
    let mut res = Response::new(full(""));
    *res.status_mut() = StatusCode::NO_CONTENT;
    if let Some(origin) = origin.filter(|o| state.apps.any_allows(o)) {
        cors_headers(&mut res, origin);
        let h = res.headers_mut();
        h.insert("access-control-allow-methods", HeaderValue::from_static("GET, POST, DELETE"));
        h.insert("access-control-allow-headers", HeaderValue::from_static(CORS_HEADERS));
        h.insert("access-control-max-age", HeaderValue::from_static("86400"));
    } else {
        *res.status_mut() = StatusCode::FORBIDDEN;
    }
    res
}

fn cors_headers(res: &mut Response<HttpBody>, origin: &str) {
    if let Ok(v) = HeaderValue::from_str(origin) {
        let h = res.headers_mut();
        h.insert("access-control-allow-origin", v);
        h.append(header::VARY, HeaderValue::from_static("Origin"));
    }
}

fn accepts_gzip(headers: &HeaderMap) -> bool {
    headers.get_all(header::ACCEPT_ENCODING).iter().filter_map(|v| v.to_str().ok()).any(|v| {
        v.split(',').any(|e| {
            let mut parts = e.split(';');
            parts.next().is_some_and(|n| n.trim().eq_ignore_ascii_case("gzip"))
                && !parts.any(|q| q.trim().replace(' ', "") == "q=0")
        })
    })
}

/// JSON answers of 1 KiB or more, gzip-compressed (fast level: rows compress
/// tenfold, for a fraction of a millisecond). Streams and pages are left as
/// they are (the site compresses its own files).
async fn compress(res: Response<HttpBody>) -> Response<HttpBody> {
    use std::io::Write as _;
    let json = res.headers().get(header::CONTENT_TYPE).is_some_and(|v| v.as_bytes().starts_with(b"application/json"));
    if !json || res.headers().contains_key(header::CONTENT_ENCODING) {
        return res;
    }
    let (mut parts, body) = res.into_parts();
    let Ok(collected) = body.collect().await;
    let bytes = collected.to_bytes();
    parts.headers.append(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    if bytes.len() < 1024 {
        return Response::from_parts(parts, full(bytes));
    }
    let mut gz = flate2::write::GzEncoder::new(Vec::with_capacity(bytes.len() / 4), flate2::Compression::fast());
    match gz.write_all(&bytes).and_then(|()| gz.finish()) {
        Ok(packed) => {
            parts.headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
            parts.headers.remove(header::CONTENT_LENGTH);
            Response::from_parts(parts, full(packed))
        }
        Err(_) => Response::from_parts(parts, full(bytes)),
    }
}

fn forbidden(message: &str) -> Response<HttpBody> {
    json_response(403, &json!({ "error": { "kind": "forbidden", "message": message } }))
}

/// `/v1/*`: the database protocol, on the database of the application the
/// token opens. Returns the answer, and the origin to let read it (a page of
/// one of the application's websites).
async fn protocol(
    state: &Arc<State>,
    req: Request<Incoming>,
    origin: Option<&str>,
    peer: SocketAddr,
) -> Result<(Response<HttpBody>, Option<String>)> {
    if state.options.token.is_none() && !state.apps.any_token() {
        return Ok((
            unauthorized("the database API is off: create an application in the console (/admin), or set NUAGE_TOKEN"),
            None,
        ));
    }
    let Some((app, read_only)) = state.app_of(req.headers()) else {
        let message = if bearer(req.headers()).is_some() {
            "this token opens no application: it was revoked, or mistyped. Copy a token from the console (the application's Tokens tab), or use NUAGE_TOKEN"
        } else {
            "no token: send the header `Authorization: Bearer <token>`, with a token of an application (the console's Tokens tab, or NUAGE_TOKEN)"
        };
        return Ok((unauthorized(message), None));
    };
    // A page of a website the application does not list: refused (the
    // browser would not let it read the answer anyway).
    if let Some(o) = origin
        && !app.allows_origin(o)
    {
        return Ok((
            forbidden(&format!(
                "{o} is not one of the websites of application `{}`: add it in the console (Settings)",
                app.name
            )),
            None,
        ));
    }
    let started = Instant::now();
    app.touch();
    let via = if req.uri().path().starts_with("/v1/sql") { "SQL over HTTP" } else { "HTTP API" };
    let client = origin
        .map(str::to_owned)
        .or_else(|| req.headers().get(header::USER_AGENT).and_then(|v| v.to_str().ok()).map(str::to_owned))
        .unwrap_or_default();
    let address = req
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|v| v.trim().to_owned())
        .unwrap_or_else(|| peer.ip().to_string());
    app.connected(via, &client, &address, read_only);
    let result = app_protocol(state, &app, read_only, req).await;
    let failed = match &result {
        Ok(res) => res.status().is_server_error(),
        Err(e) => wire::error_to_json(e).0 >= 500,
    };
    app.metrics.record(started.elapsed(), failed);
    let res = result.unwrap_or_else(|e| error_response(&e));
    Ok((res, origin.map(str::to_owned)))
}

const READ_ONLY: &str = "this token only reads: use a read-write token (from your server, never in a page)";

async fn app_protocol(
    state: &Arc<State>,
    app: &Arc<apps::App>,
    read_only: bool,
    req: Request<Incoming>,
) -> Result<Response<HttpBody>> {
    let method = req.method().clone();
    let path = req.uri().path().trim_end_matches('/').to_owned();
    let segments: Vec<&str> = path.trim_start_matches("/v1/").split('/').collect();
    let writes = match (&method, segments.as_slice()) {
        (&Method::POST, ["execute" | "transactions" | "branches", ..]) | (&Method::DELETE, _) => true,
        // A transaction's statements, even its reads: a read-only token opens none.
        _ => req.headers().contains_key(TRANSACTION_HEADER),
    };
    if read_only && writes {
        return Ok(forbidden(READ_ONLY));
    }
    let header = |name: &str| req.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let branch = header(BRANCH_HEADER).unwrap_or_else(|| MAIN.to_owned());
    let tx_id = header(TRANSACTION_HEADER);
    let engine = &app.engine;

    match (method, segments.as_slice()) {
        // "Does my connection work?": which application the token opens.
        (Method::GET, ["ping"]) => {
            check_branch(engine, &branch)?;
            let tables = Embedded::from_engine(engine.clone()).tables().map(|t| t.len()).unwrap_or(0);
            let access = if read_only { "read-only" } else { "read-write" };
            Ok(json_response(
                200,
                &json!({
                    "ok": true,
                    "message": format!(
                        "Connected to Nuage DB {}: application `{}`, {}, branch `{branch}`, {tables} table(s).",
                        crate::VERSION,
                        app.name,
                        if read_only { "read only" } else { "read and write" },
                    ),
                    "server": "Nuage DB",
                    "version": crate::VERSION,
                    "application": app.name,
                    "access": access,
                    "branch": branch,
                    "tables": tables,
                }),
            ))
        }
        (Method::GET, ["info"]) => Ok(json_response(
            200,
            &json!({
                "name": "Nuage DB",
                "version": crate::VERSION,
                "database": engine.options().name,
                "application": app.name,
                "encrypted": engine.encryption().is_some(),
            }),
        )),
        (Method::POST, [op @ ("fetch" | "execute")]) => {
            let fetch = *op == "fetch";
            let body = read_json(state, req).await?;
            let query = wire::query_from_json(body.get("query").unwrap_or(&Json::Null), &state.schemas)?;
            let engine = engine.clone();
            let tx = match &tx_id {
                Some(id) => Some(state.transaction(id, &app.name, &branch)?),
                None => None,
            };
            // `db.raw(...)` and `db.raw_execute(...)` of Memoire's client: the
            // SQL layer runs them, as for `/v1/sql`, inside the open transaction
            // when there is one.
            if let Some((script, params)) = raw_script(&query)? {
                if read_only && script.writes() {
                    return Ok(forbidden(READ_ONLY));
                }
                let schemas = state.clone();
                let answer = tokio::task::spawn_blocking(move || -> Result<Json> {
                    let results = match tx {
                        Some(tx) => crate::sql::run_in(&tx.tx, &schemas.schemas, &script, &params)?,
                        None => {
                            check_branch(&engine, &branch)?;
                            crate::sql::run_values(&engine, &branch, &schemas.schemas, &[(&script, &params)], true)?
                                .pop()
                                .unwrap_or_default()
                        }
                    };
                    Ok(if fetch {
                        let rows = results.into_iter().rev().find(|r| !r.columns.is_empty()).map(|r| r.rows);
                        json!({ "rows": wire::rows_to_json(&rows.unwrap_or_default()) })
                    } else {
                        let done = memoire_orm::ExecResult {
                            rows_affected: results.iter().map(|r| r.rows_affected).sum(),
                            last_insert_id: results.iter().rev().find_map(|r| r.last_insert_id.clone()),
                        };
                        wire::exec_result_to_json(&done)
                    })
                })
                .await
                .map_err(|e| Error::Pool(e.to_string()))??;
                return Ok(json_response(200, &answer));
            }
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
                if read_only && !fetch {
                    return Ok(forbidden(READ_ONLY));
                }
                statements.push((fetch, wire::query_from_json(q, &state.schemas)?));
            }
            let db = state.db(engine, &branch)?;
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
            let tx = state.db(engine, &branch)?.begin_now()?;
            let id = random_token();
            let open = OpenTx { tx, app: app.name.clone(), branch, last_used: Mutex::new(Instant::now()) };
            state.transactions.lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone(), Arc::new(open));
            Ok(json_response(200, &json!({ "id": id })))
        }
        (Method::POST, ["transactions", id, action @ ("commit" | "rollback")]) => {
            let open = {
                let mut txs = state.transactions.lock().unwrap_or_else(|e| e.into_inner());
                // Another application's transaction does not exist for this one.
                if txs.get(*id).is_none_or(|t| t.app != app.name) {
                    return Err(Error::TransactionFinished);
                }
                txs.remove(*id).ok_or(Error::TransactionFinished)?
            };
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
        (Method::POST, ["sql"]) => {
            let body = read_json(state, req).await?;
            // `{"sql", "params"}`, or `{"statements": [{"sql", "params"}, ...]}`:
            // one transaction either way.
            let mut batch = Vec::new();
            let several = match body.get("statements").and_then(Json::as_array) {
                Some(list) => {
                    for s in list {
                        batch.push(script_of(s)?);
                    }
                    true
                }
                None => {
                    let (script, params) = script_of(&body)?;
                    let several = script.len() > 1;
                    batch.push((script, params));
                    several
                }
            };
            if read_only && batch.iter().any(|(s, _)| s.writes()) {
                return Ok(forbidden(READ_ONLY));
            }
            let arrays = body.get("arrays").and_then(Json::as_bool).unwrap_or(false);
            let engine = engine.clone();
            let schemas = state.clone();
            let results =
                tokio::task::spawn_blocking(move || crate::sql::run(&engine, &branch, &schemas.schemas, &batch, true))
                    .await
                    .map_err(|e| Error::Pool(e.to_string()))??;
            let mut all: Vec<Json> = results.into_iter().flatten().map(|r| sql_json(&r, arrays)).collect();
            Ok(json_response(200, &if several || all.len() != 1 { json!({ "results": all }) } else { all.remove(0) }))
        }
        _ => Err(Error::NotFound),
    }
}

/// The SQL of a raw statement of Memoire's protocol (`Query::Raw`,
/// `Query::Script`), parsed, with its parameters; `None` for the others.
fn raw_script(query: &wire::OwnedQuery) -> Result<Option<(crate::sql::Script, Vec<memoire_orm::Value>)>> {
    Ok(match query {
        wire::OwnedQuery::Raw(sql, params) => Some((crate::sql::Script::parse(sql)?, params.clone())),
        wire::OwnedQuery::Script(sql) => Some((crate::sql::Script::parse(sql)?, Vec::new())),
        _ => None,
    })
}

/// A script of a `/v1/sql` request: `{"sql": "...", "params": [...]}`.
fn script_of(j: &Json) -> Result<(crate::sql::Script, Vec<Json>)> {
    let sql = j.get("sql").and_then(Json::as_str).ok_or_else(|| Error::Conversion("missing `sql`".into()))?;
    let params = match j.get("params") {
        None | Some(Json::Null) => Vec::new(),
        Some(Json::Array(list)) => list.clone(),
        Some(_) => return Err(Error::Conversion("`params` is a list of values".into())),
    };
    Ok((crate::sql::Script::parse(sql)?, params))
}

/// A statement's result as `/v1/sql` answers it: rows as objects (or arrays).
fn sql_json(r: &crate::sql::SqlResult, arrays: bool) -> Json {
    use crate::sql::value_to_json as val;
    let mut out = serde_json::Map::new();
    if r.returns_rows {
        out.insert("columns".into(), json!(r.columns));
        let rows: Vec<Json> = r
            .rows
            .iter()
            .map(|row| {
                if arrays {
                    Json::Array(row.iter().map(val).collect())
                } else {
                    Json::Object(r.columns.iter().cloned().zip(row.iter().map(val)).collect())
                }
            })
            .collect();
        out.insert("rows".into(), Json::Array(rows));
    }
    if !r.returns_rows || r.rows_affected > 0 {
        out.insert("rows_affected".into(), json!(r.rows_affected));
        out.insert("last_insert_id".into(), r.last_insert_id.as_ref().map_or(Json::Null, val));
    }
    Json::Object(out)
}

fn check_branch(engine: &Engine, branch: &str) -> Result<()> {
    if engine.has_branch(branch) { Ok(()) } else { Err(Error::NotFound) }
}

impl State {
    fn transaction(&self, id: &str, app: &str, branch: &str) -> Result<Arc<OpenTx>> {
        let tx = self
            .transactions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .filter(|t| t.app == app)
            .cloned()
            .ok_or(Error::TransactionFinished)?;
        if tx.branch != branch {
            return Err(Error::Validation("this transaction belongs to another branch".into()));
        }
        *tx.last_used.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
        Ok(tx)
    }
}
