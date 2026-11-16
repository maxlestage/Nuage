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
//! | `/api/status`, `/api/status/stream` | live status, as JSON and as Server-Sent Events | an application's token (that application), the management token or a console session (all of them); a public subset with `public_status` |
//! | `/api/apps`, `/api/apps/{name}`, `/api/apps/{name}/tokens`, `/api/apps/{name}/origins` | the management API: applications, their tokens and websites, as JSON (see `manage`) | `Authorization: Bearer <NUAGE_ADMIN_TOKEN>` |
//! | `/admin` | the management console | `NUAGE_ADMIN_PASSWORD` |
//! | anything else (`GET`) | the site handler, if any, else a small home page | public |

mod admin;
pub(crate) mod apps;
mod fast;
pub mod files;
mod i18n;
mod kv;
mod kv_port;
mod limits;
mod manage;
mod net;
mod pg;
mod second_copy;
pub(crate) mod settings;
mod status;

pub use files::FileStore;
pub use kv_port::KvOptions;
pub use manage::MIN_ADMIN_TOKEN;
pub use pg::{PgOptions, tls_config};

use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::body::{Body, Bytes, Frame, Incoming};
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use memoire_orm::nuage::wire::{self, SchemaCache};
use memoire_orm::nuage::{BRANCH_HEADER, TRANSACTION_HEADER};
use memoire_orm::{Error, Result};
use serde_json::{Value as Json, json};
use tokio::net::TcpListener;
use tokio::sync::{Notify, Semaphore, mpsc};

use self::limits::{
    Asker, MAX_BATCH_ANSWER, MAX_PARAMS, MAX_SQL_TEXT, MAX_STATEMENTS, refused, too_large, too_many, unblocked,
};

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
    /// Token of the management API (`NUAGE_ADMIN_TOKEN`, at least
    /// [`MIN_ADMIN_TOKEN`] characters): `/api/apps` creates applications and
    /// their tokens with it. Without it, the API is off.
    pub admin_token: Option<String>,
    /// Status without credentials: the public subset (`NUAGE_PUBLIC_STATUS`).
    pub public_status: bool,
    /// An interactive transaction left without a request this long is
    /// rolled back. Default: 30 seconds.
    pub transaction_timeout: Duration,
    /// An interactive transaction is rolled back this long after it began,
    /// however busy: an open one keeps the pages it sees, and the database's
    /// file grows meanwhile. Default: 5 minutes.
    pub transaction_lifetime: Duration,
    /// Interactive transactions open at once, per application; the next
    /// one is refused (429) until one ends. Default: 64.
    pub max_transactions: usize,
    /// Largest request body. Default: 16 MiB.
    pub max_body_bytes: usize,
    /// How long a client has to send a request's headers, and how long a
    /// kept-alive connection waits for its next request. Default: 30 seconds.
    pub header_timeout: Duration,
    /// How long a client has to send a request's body (408 after it).
    /// Default: 60 seconds.
    pub body_timeout: Duration,
    /// How long an answer waits for a client that takes none of it before
    /// its connection is closed. Default: 60 seconds.
    pub send_timeout: Duration,
    /// HTTP connections served at once; the next ones are answered 503 and
    /// closed at once. Default: half the files the system lets the process
    /// open, between 32 and 4096.
    pub max_connections: usize,
    /// Memory the requests being read may hold together (their bodies, and
    /// the JSON they are read into): a request that would pass it waits a
    /// moment, then is answered 503 (413 when it alone would). `None`: a
    /// quarter of the memory, and twice `max_body_bytes` at least.
    pub request_memory: Option<usize>,
    /// Statements run at once, each on a thread of its own (a quarter of
    /// them, two at least, for writes: reads never keep a write waiting).
    /// The next ones wait a moment, then are answered 503. Default: the
    /// blocking pool of the `nuage` binary, less 4.
    pub statement_threads: usize,
    /// The website served next to the database, if any.
    pub site: Option<Arc<dyn SiteHandler>>,
    /// Port of the PostgreSQL protocol ([`Server::listen_pg`]), for the
    /// console to show how to connect.
    pub pg_port: Option<u16>,
    /// The PostgreSQL protocol has TLS.
    pub pg_tls: bool,
    /// The bucket of the applications' files (`/v1/files`), if any
    /// (`NUAGE_FILES_S3_*`).
    pub files: Option<Arc<FileStore>>,
    /// Memory of each application's key-value store (`/v1/kv`, the key-value
    /// port), in bytes (`NUAGE_KV_MB`). Default: 32 MiB.
    pub kv_bytes: usize,
    /// Memory the key-value stores of all the applications may hold
    /// together: each has the smaller of `kv_bytes` and its equal part of
    /// this (`None`: `kv_bytes` each, whatever their number).
    pub kv_budget: Option<usize>,
    /// Memory the page caches of all the applications may hold together,
    /// shared equally among them (`None`: each engine keeps its
    /// [`EngineOptions::cache_bytes`](crate::EngineOptions)).
    pub cache_budget: Option<usize>,
    /// The most that budget may grow to while memory has room (`None`: it
    /// does not grow past `cache_budget`).
    pub cache_ceiling: Option<usize>,
    /// PostgreSQL connections served at once (each has a thread); the next
    /// ones are refused as PostgreSQL does (`too_many_connections`).
    /// Read-only tokens have half of them at most.
    pub pg_max_connections: usize,
    /// Key-value port connections served at once ([`Server::listen_kv`]),
    /// and no more than an eighth of the files the process may open; the
    /// next ones are told `max number of clients reached`, as Redis says it,
    /// and closed. Read-only tokens have half of them at most. Default: 256.
    pub kv_max_connections: usize,
    /// What the environment (or the settings file) says for the settings the
    /// owner may change while the server runs, by variable (`NUAGE_CACHE_MB`
    /// → `auto`): the console shows it next to what was saved, and a value
    /// goes back to it. `None`: a server built by hand, whose options above
    /// say everything.
    pub settings_env: Option<HashMap<String, String>>,
    /// The environment is the only source of the settings: what was saved
    /// from the console is ignored and nothing can be saved
    /// (`NUAGE_SETTINGS=environment`).
    pub settings_read_only: bool,
    /// The settings read once at startup, for the console to show: each
    /// with its variable and its value (a secret: only whether it is set).
    pub startup: Vec<StartupSetting>,
    /// The settings file the server was started with, if any.
    pub settings_file: Option<String>,
}

/// A setting read once, when the server starts (a port, the data directory,
/// a password...): the console shows it and where it comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartupSetting {
    /// Its variable (`PORT`).
    pub var: &'static str,
    /// Its value, as it can be shown: `None` when nothing was said (the
    /// default applies). A secret shows as [`StartupSetting::SECRET`].
    pub value: Option<String>,
}

impl StartupSetting {
    /// What stands for the value of a secret that is set.
    pub const SECRET: &'static str = "***";
}

impl std::fmt::Debug for ServerOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerOptions")
            .field("token", &self.token.as_ref().map(|_| "***"))
            .field("admin_password", &self.admin_password.as_ref().map(|_| "***"))
            .field("admin_token", &self.admin_token.as_ref().map(|_| "***"))
            .field("public_status", &self.public_status)
            .field("site", &self.site.is_some())
            .field("pg_port", &self.pg_port)
            .field("pg_tls", &self.pg_tls)
            .field("files", &self.files.as_ref().map(|f| f.describe()))
            .field("kv_bytes", &self.kv_bytes)
            .field("cache_budget", &self.cache_budget)
            .field("pg_max_connections", &self.pg_max_connections)
            .field("settings_read_only", &self.settings_read_only)
            .field("max_body_bytes", &self.max_body_bytes)
            .field("max_connections", &self.max_connections)
            .field("request_memory", &self.request_memory)
            .field("statement_threads", &self.statement_threads)
            .field("max_transactions", &self.max_transactions)
            .finish()
    }
}

impl Default for ServerOptions {
    fn default() -> Self {
        ServerOptions {
            token: None,
            admin_password: None,
            admin_token: None,
            public_status: false,
            transaction_timeout: Duration::from_secs(30),
            transaction_lifetime: Duration::from_secs(300),
            max_transactions: 64,
            max_body_bytes: 16 << 20,
            header_timeout: Duration::from_secs(30),
            body_timeout: Duration::from_secs(60),
            send_timeout: Duration::from_secs(60),
            max_connections: limits::default_max_connections(),
            request_memory: None,
            statement_threads: limits::statement_threads(),
            site: None,
            pg_port: None,
            pg_tls: false,
            files: None,
            kv_bytes: 32 << 20,
            kv_budget: None,
            cache_budget: None,
            cache_ceiling: None,
            pg_max_connections: 100,
            kv_max_connections: net::KV_CONNECTIONS,
            settings_env: None,
            settings_read_only: false,
            startup: Vec::new(),
            settings_file: None,
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

/// One more in a counter for as long as it lives: a request dropped half-way
/// (its client left) or a task that panicked is no longer counted.
struct Counted<'a>(&'a AtomicU64);

impl<'a> Counted<'a> {
    fn new(counter: &'a AtomicU64) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Counted(counter)
    }
}

impl Drop for Counted<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A future whose panic comes back as `Err`: a bug met by one request
/// answers 500, and the connection and its counters stay right.
struct CatchUnwind<F>(Pin<Box<F>>);

impl<F: Future> Future for CatchUnwind<F> {
    type Output = std::thread::Result<F::Output>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = self.0.as_mut();
        match std::panic::catch_unwind(AssertUnwindSafe(|| inner.poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(out)) => Poll::Ready(Ok(out)),
            Err(panic) => Poll::Ready(Err(panic)),
        }
    }
}

struct OpenTx {
    tx: EmbeddedTransaction,
    app: String,
    branch: String,
    opened: Instant,
    last_used: Mutex<Instant>,
}

pub(crate) struct State {
    /// The first application's engine (the server's own).
    pub(crate) engine: Arc<Engine>,
    pub(crate) apps: apps::Apps,
    pub(crate) options: ServerOptions,
    /// What the owner changes while the server runs.
    pub(crate) settings: settings::Settings,
    /// One change of the settings at a time.
    settings_changes: Mutex<()>,
    pub(crate) metrics: Metrics,
    /// The schemas of the tables, for SQL (the stored ones).
    pub(crate) schemas: SchemaCache,
    /// The schemas the clients of each application describe, by its name:
    /// apart from the tables' own and from the other applications'.
    client_schemas: Mutex<HashMap<String, Arc<limits::ClientSchemas>>>,
    /// The memory those schemas take, all the applications together.
    pub(crate) described_total: Arc<limits::DescribedTotal>,
    /// How long reads take, by shape: the short ones run where they are.
    pub(crate) read_costs: Arc<fast::ReadCosts>,
    /// Reads that run where they are, and their watch (set by the first
    /// listener; `None` on a runtime with one thread).
    inline: std::sync::OnceLock<Option<Arc<fast::Inline>>>,
    /// Threads for statements, reads and writes apart.
    pub(crate) admission: limits::Admission,
    /// Memory for the requests being read.
    pub(crate) request_memory: limits::RequestMemory,
    /// HTTP connections that may still open.
    connections: Arc<Semaphore>,
    /// The status of the views that have a stream open, made once a second.
    pub(crate) status_hub: status::Hub,
    /// `/api/status` computed at once.
    pub(crate) status_slots: Arc<Semaphore>,
    transactions: Mutex<HashMap<String, Arc<OpenTx>>>,
    pub(crate) admin: admin::Sessions,
    /// PostgreSQL connections open now.
    pub(crate) pg_connections: std::sync::atomic::AtomicUsize,
    memory_ticks: AtomicU64,
    memory_reliefs: AtomicU64,
    /// The tick of the last cut of the cache budget, plus one (0: none yet).
    memory_last_relief: AtomicU64,
    /// What the budget may grow back to since that cut (0: its ceiling).
    memory_learned: AtomicU64,
    /// The tick, plus one, at which the guard last said that memory is
    /// short without the caches being the cause (0: never).
    memory_noted: AtomicU64,
}

/// Seconds between two looks at the memory.
const MEMORY_CHECK_EVERY: u64 = 2;
/// Seconds between two adjustments of the cache budget (grow, shrink).
const MEMORY_ADJUST_EVERY: u64 = 10;
/// Seconds the budget waits after a cut before it grows again.
const MEMORY_COOL: u64 = 300;
/// Seconds a cut is remembered: until then the budget stays under three
/// quarters of the one that did not fit.
const MEMORY_LESSON: u64 = 3600;

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
        // What the owner saved comes first: the memory to count on, then the
        // budgets that follow from it.
        let settings =
            settings::Settings::open(engine.clone(), options.settings_env.clone(), options.settings_read_only);
        settings.declare_memory();
        let plan = settings.memory_plan(&options);
        let memory = crate::memory::MemoryInfo::now().total_bytes;
        // A quarter of the memory for the requests being read, and room for two of the largest.
        let request_memory = options
            .request_memory
            .unwrap_or_else(|| memory.map_or(256 << 20, |m| (m / 4) as usize))
            .max(options.max_body_bytes.saturating_mul(2));
        Server {
            state: Arc::new(State {
                apps: apps::Apps::open(engine.clone(), plan, options.settings_read_only),
                engine,
                settings,
                settings_changes: Mutex::new(()),
                metrics: Metrics::new(),
                schemas: SchemaCache::default(),
                client_schemas: Mutex::new(HashMap::new()),
                described_total: limits::DescribedTotal::for_memory(memory),
                read_costs: Arc::new(fast::ReadCosts::new()),
                inline: std::sync::OnceLock::new(),
                admission: limits::Admission::new(options.statement_threads),
                request_memory: limits::RequestMemory::new(request_memory),
                connections: Arc::new(Semaphore::new(options.max_connections.max(1))),
                status_hub: status::Hub::default(),
                status_slots: Arc::new(Semaphore::new(2)),
                options,
                transactions: Mutex::new(HashMap::new()),
                admin: admin::Sessions::default(),
                pg_connections: std::sync::atomic::AtomicUsize::new(0),
                memory_ticks: AtomicU64::new(0),
                memory_reliefs: AtomicU64::new(0),
                memory_last_relief: AtomicU64::new(0),
                memory_learned: AtomicU64::new(0),
                memory_noted: AtomicU64::new(0),
            }),
        }
    }

    /// The settings saved from the console that win over a different value
    /// of the environment, one sentence each (for the log at startup).
    pub fn setting_overrides(&self) -> Vec<String> {
        self.state.settings.overrides()
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

    /// The applications that could not be opened when the server started,
    /// each with why: their data is left as it was.
    pub fn failed_apps(&self) -> Vec<(String, String)> {
        self.state.apps.failed()
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
        self.state.apps.set_origins(&app, &origins.join("\n")).map_err(Error::from)
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

    /// Saves a setting, as the console's Settings page does (`None` gives it
    /// back to the environment's value, or the default), in force at once.
    /// The settings are listed by `GET /api/settings` (`memory.cache_mb`,
    /// `status.public`...); a value that does not fit is refused with the
    /// reason.
    pub fn save_setting(&self, key: &str, value: Option<&str>) -> Result<()> {
        self.state.save_settings(&[(key, value)]).map(|_| ()).map_err(Error::from)
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
        self.state.inline.get_or_init(|| fast::Inline::start(self.state.read_costs.clone()));
        let state = self.state.clone();
        let stopped = stop.clone();
        // The upkeep of every second runs on a thread of its own, whatever the
        // listener and the runtime's workers are doing; it stops with the
        // listener (when `ticking` is dropped).
        let (ticking, ticks) = std::sync::mpsc::channel::<()>();
        let threaded = start_ticks(&self.state, ticks);
        let task = tokio::spawn(async move {
            let _ticking = ticking;
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            // Headers within `header_timeout` (hyper needs a timer to keep
            // time), and a kept-alive connection closes after as long idle.
            let mut http = http1::Builder::new();
            http.timer(TokioTimer::new()).header_read_timeout(state.options.header_timeout);
            loop {
                tokio::select! {
                    _ = stopped.notified() => break,
                    // Only when no thread could be started for it.
                    _ = tick.tick(), if !threaded => state.tick(),
                    accepted = listener.accept() => {
                        let (stream, peer) = match accepted {
                            Ok(accepted) => accepted,
                            // Out of file descriptors (or memory): the connection
                            // stays in the backlog and the next accept would fail at
                            // once; a pause, not a loop that burns a core.
                            Err(e) => {
                                if !matches!(
                                    e.kind(),
                                    std::io::ErrorKind::ConnectionAborted
                                        | std::io::ErrorKind::ConnectionReset
                                        | std::io::ErrorKind::Interrupted
                                ) {
                                    tokio::time::sleep(ACCEPT_PAUSE).await;
                                }
                                continue;
                            }
                        };
                        // Past the connections it may hold, the server says so and
                        // closes at once: the socket holds nothing.
                        let Ok(slot) = state.connections.clone().try_acquire_owned() else {
                            let _ = stream.try_write(&limits::full_answer());
                            continue;
                        };
                        let _ = stream.set_nodelay(true);
                        let state = state.clone();
                        let http = http.clone();
                        tokio::spawn(async move {
                            let _slot = slot;
                            let _open = Counted::new(&state.metrics.open_connections);
                            let conn_state = state.clone();
                            let service = service_fn(move |req| {
                                let state = conn_state.clone();
                                async move { Ok::<_, Infallible>(handle(state, req, peer).await) }
                            });
                            let stream = limits::Patient::new(stream, state.options.send_timeout);
                            let _ = http.serve_connection(TokioIo::new(stream), service).await;
                        });
                    }
                }
            }
        });
        Ok(RunningServer { addr, stop, task })
    }
}

/// A pause after an accept that failed for want of resources.
const ACCEPT_PAUSE: Duration = Duration::from_millis(100);

/// Starts the upkeep of every second on a thread of its own: it goes on when
/// the runtime's workers are busy, which is when memory is highest. It stops
/// when `ticks` is closed or the server is gone. `false` when no thread
/// could be started (the listener then keeps the time itself).
fn start_ticks(state: &Arc<State>, ticks: std::sync::mpsc::Receiver<()>) -> bool {
    let state = Arc::downgrade(state);
    std::thread::Builder::new()
        .name("nuage-tick".into())
        .spawn(move || {
            while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = ticks.recv_timeout(Duration::from_secs(1)) {
                let Some(state) = state.upgrade() else { return };
                state.tick();
            }
        })
        .is_ok()
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
        net::raise_open_files_limit();
        // What its sessions share: memory, places for read-only tokens.
        let port = pg::Port::new(&options, state.options.max_body_bytes);
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stopped.notified() => break,
                    accepted = listener.accept() => {
                        let stream = match accepted {
                            Ok((stream, _)) => stream,
                            Err(e) => {
                                net::accept_failed(&e).await;
                                continue;
                            }
                        };
                        let Ok(stream) = stream.into_std() else { continue };
                        if stream.set_nonblocking(false).is_err() {
                            continue;
                        }
                        // Each connection has a thread: past the limit they are
                        // told so (a short-lived thread), and far past it,
                        // dropped, so that a flood cannot use up the threads.
                        // Counted here, before its thread starts (the thread
                        // only counts it out): the limit holds however fast
                        // connections come.
                        let open = state.pg_connections.fetch_add(1, Ordering::Relaxed);
                        let max = state.pg_max_connections();
                        if open >= max + 16 {
                            state.pg_connections.fetch_sub(1, Ordering::Relaxed);
                            continue;
                        }
                        let (state, mut options) = (state.clone(), options.clone());
                        options.refuse = open >= max;
                        // A thread per connection, as PostgreSQL has a process:
                        // its statements block on the engine. Not a runtime
                        // task, so an idle client never delays a shutdown.
                        // Its stack has room for deeply nested statements.
                        let (counted, port) = (state.clone(), port.clone());
                        let spawned = std::thread::Builder::new()
                            .name("nuage-pg".into())
                            .stack_size(pg::THREAD_STACK)
                            .spawn(move || pg::serve(state, stream, options, port));
                        if spawned.is_err() {
                            counted.pg_connections.fetch_sub(1, Ordering::Relaxed);
                        }
                    }
                }
            }
        });
        Ok(RunningServer { addr, stop, task })
    }
}

impl Server {
    /// Binds `addr` and serves the key-value stores there, in the background,
    /// over the protocol Redis clients speak: one connects to an application's
    /// store, with a token of the application as its password (see
    /// [`KvOptions`]).
    pub async fn listen_kv(&self, addr: impl tokio::net::ToSocketAddrs, options: KvOptions) -> Result<RunningServer> {
        let listener = TcpListener::bind(addr).await.map_err(|e| Error::Pool(format!("cannot listen: {e}")))?;
        let addr = listener.local_addr().map_err(|e| Error::Pool(e.to_string()))?;
        let stop = Arc::new(Notify::new());
        let state = self.state.clone();
        let stopped = stop.clone();
        net::raise_open_files_limit();
        // What its connections share: places (each holds a socket and its
        // buffers), memory.
        let most = net::connections_within_files(state.options.kv_max_connections);
        let port = kv_port::Port::new(None, state.options.max_body_bytes, most);
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stopped.notified() => break,
                    accepted = listener.accept() => {
                        let (stream, peer) = match accepted {
                            Ok(accepted) => accepted,
                            Err(e) => {
                                net::accept_failed(&e).await;
                                continue;
                            }
                        };
                        // Past the limit the client is told so, as Redis tells
                        // it, and the socket closed at once.
                        let Some(place) = kv_port::Place::take(&port) else {
                            let _ = stream.try_write(kv_port::TOO_MANY_CLIENTS);
                            continue;
                        };
                        tokio::spawn(kv_port::serve(state.clone(), stream, peer, options.clone(), place));
                    }
                }
            }
        });
        Ok(RunningServer { addr, stop, task })
    }
}

impl State {
    /// The upkeep of every second: transactions that expired, keys whose
    /// time came, the memory guard. A panic in one round is told, and the
    /// next second runs again.
    fn tick(&self) {
        let round = std::panic::catch_unwind(AssertUnwindSafe(|| {
            self.expire_transactions();
            self.sweep_keys();
            self.watch_memory();
            self.forget_deleted_apps();
        }));
        if let Err(panic) = round {
            self.engine.event("error", format!("the server's upkeep failed: {}", limits::panic_text(&panic)));
        }
    }

    /// Keeps the memory in use where it should be, whatever the server: the
    /// page caches take the memory there is and no more. Every two seconds:
    /// past [`MEMORY_HIGH`](crate::memory::MEMORY_HIGH) the caches are emptied,
    /// their budget halved and the freed memory given back to the system.
    /// Every ten: the budget shrinks a step when memory gets tight, and grows
    /// a step when there is room and some cache is full, up to its ceiling.
    /// Statements go on throughout; they only read the disk more, or less.
    fn watch_memory(&self) {
        use crate::memory::CacheMove;
        let tick = self.memory_ticks.fetch_add(1, Ordering::Relaxed);
        if !tick.is_multiple_of(MEMORY_CHECK_EVERY) {
            return;
        }
        let m = crate::memory::MemoryInfo::now();
        let (Some(pressure), Some(total)) = (m.pressure(), m.total_bytes) else { return };
        let gib = total as f64 / (1u64 << 30) as f64;
        let Some(budget) = self.apps.cache_budget() else {
            // No shared budget (a server built by hand): the caches are still
            // emptied when memory runs short.
            if pressure >= crate::memory::MEMORY_HIGH {
                self.apps.drop_caches();
                crate::memory::release_free_memory();
                self.memory_reliefs.fetch_add(1, Ordering::Relaxed);
            }
            return;
        };
        // The last cut: growth waits five minutes after it, and for an hour
        // stays under three quarters of the budget that did not fit.
        let last = self.memory_last_relief.load(Ordering::Relaxed);
        let since = if last == 0 { u64::MAX } else { tick.saturating_sub(last - 1) };
        if since >= MEMORY_LESSON {
            self.memory_learned.store(0, Ordering::Relaxed);
            self.apps.set_cache_lesson(0);
        }
        let learned = self.memory_learned.load(Ordering::Relaxed);
        let hard = self.apps.cache_ceiling().unwrap_or(budget);
        let ceiling = if learned == 0 { hard } else { hard.min(learned as usize) };
        let slow = tick.is_multiple_of(MEMORY_ADJUST_EVERY);
        if slow {
            // The budget goes again to the applications that use it.
            self.apps.share_cache();
        }
        let demand = slow && self.apps.cache_demand();
        // The memory in use once the caches have filled what they may still take.
        let unfilled = budget.saturating_sub(self.apps.cache_used()) as f64;
        let committed = pressure + unfilled / total as f64;
        match crate::memory::cache_move(budget, ceiling, total, pressure, committed, demand, since >= MEMORY_COOL) {
            CacheMove::Relieve(to) => {
                // First what the allocator keeps of memory freed since (a
                // large answer, a burst of statements): often that is all
                // there was, and the caches have nothing to do with it.
                crate::memory::release_free_memory();
                let pressure = crate::memory::MemoryInfo::now().pressure().unwrap_or(pressure);
                if pressure < crate::memory::MEMORY_HIGH {
                    return;
                }
                let held = self.apps.cache_used();
                // Past 95 % the caches are emptied whatever they hold: the last resort.
                if (held as u64) < total / 10 && pressure < 0.95 {
                    // The caches are not what fills the memory (statements
                    // or answers in progress are): emptying them would slow
                    // every read and free little. They keep what they hold
                    // and take no more while memory is short.
                    // (A quarter above it: a cache sits a little under its limit.)
                    let frozen = (held + held / 4).max((16 << 20).min(hard));
                    if frozen < budget {
                        self.apps.set_cache_budget(frozen);
                        // Counted as the guard stepping in; growth waits, as after a cut.
                        self.memory_last_relief.store(tick + 1, Ordering::Relaxed);
                        self.memory_reliefs.fetch_add(1, Ordering::Relaxed);
                    }
                    // Said once a minute, not every two seconds.
                    let noted = self.memory_noted.load(Ordering::Relaxed);
                    if noted == 0 || tick.saturating_sub(noted - 1) >= 60 {
                        self.memory_noted.store(tick + 1, Ordering::Relaxed);
                        let message = format!(
                            "memory: {:.0} % of {gib:.2} GiB in use, of which the page caches hold {} MiB: statements or answers in progress hold the rest; the caches take no more for now",
                            pressure * 100.0,
                            held >> 20
                        );
                        // An event: the binary prints warnings in its log too.
                        self.engine.event("warn", message);
                    }
                    return;
                }
                self.apps.set_cache_budget(to);
                self.apps.drop_caches();
                crate::memory::release_free_memory();
                // The budget is at its floor already and memory is still
                // short (something else holds it): the caches stay empty, and
                // it is said once a minute, not every two seconds.
                if to == budget && since < 60 {
                    return;
                }
                self.memory_learned.store((budget / 4 * 3).max(to) as u64, Ordering::Relaxed);
                self.apps.set_cache_lesson((budget / 4 * 3).max(to));
                self.memory_last_relief.store(tick + 1, Ordering::Relaxed);
                self.memory_reliefs.fetch_add(1, Ordering::Relaxed);
                let message = format!(
                    "memory: {:.0} % of {gib:.2} GiB in use: the page caches were emptied, their budget cut from {} to {} MiB and the freed memory returned to the system",
                    pressure * 100.0,
                    budget >> 20,
                    to >> 20
                );
                // An event: the binary prints warnings in its log too.
                self.engine.event("warn", message);
            }
            CacheMove::Shrink(to) if slow => {
                self.apps.set_cache_budget(to);
                crate::memory::release_free_memory();
            }
            CacheMove::Grow(to) if slow => self.apps.set_cache_budget(to),
            // The caches would take more and there is no room for it, or
            // memory is past comfortable: what the allocator keeps of memory
            // freed since (a large answer, a burst) goes back to the system,
            // so that the figures say what is really in use and the room goes
            // to the caches.
            _ if slow && (demand || pressure >= crate::memory::MEMORY_ROOM) => crate::memory::release_free_memory(),
            _ => {}
        }
    }

    /// Puts the settings in force again, after one was saved or removed:
    /// the memory to count on, the budgets that follow from it, shared anew
    /// among the applications. When the memory to count on changed, the guard
    /// forgets what it learned under the earlier one.
    pub(crate) fn apply_settings(&self) {
        let before = crate::memory::MemoryInfo::now().total_bytes;
        self.settings.declare_memory();
        if crate::memory::MemoryInfo::now().total_bytes != before {
            self.memory_last_relief.store(0, Ordering::Relaxed);
            self.memory_learned.store(0, Ordering::Relaxed);
            self.apps.set_cache_lesson(0);
        }
        self.apps.set_plan(self.settings.memory_plan(&self.options));
    }

    /// Saves settings and puts them in force at once: `(key, value)` pairs,
    /// `None` giving a setting back to the environment's value (or the
    /// default). All are checked first, one by one and together with what
    /// stays as it is: one that does not fit, and nothing is saved. Returns
    /// what kept them from reaching the backup yet, if anything did.
    pub(crate) fn save_settings(
        &self,
        changes: &[(&str, Option<&str>)],
    ) -> std::result::Result<Option<String>, settings::Refusal> {
        use settings::{Refusal, refused};
        let _guard = self.settings_changes.lock().unwrap_or_else(|e| e.into_inner());
        let mut checked: Vec<(&str, Option<String>)> = Vec::new();
        for (key, value) in changes {
            checked.push((key, self.settings.check(key, *value)?));
        }
        // The amounts of memory are checked together when one of them changes.
        if let Some(changed) = checked.iter().map(|(key, _)| *key).find(|key| key.starts_with("memory.")) {
            // What would be in force: the change, else what is now.
            let would = |key: &str| match checked.iter().rev().find(|(k, _)| *k == key) {
                Some((_, Some(v))) => Some(v.clone()),
                Some((_, None)) => self.settings.environment(key),
                None => self.settings.value(key).0,
            };
            let mib = |key: &str| would(key).and_then(|v| v.parse::<u64>().ok());
            // Saying the memory is for lowering it: under what the system
            // reports, and under the environment's figure when it gives one
            // (a host that limits the memory without saying so).
            let detected = crate::memory::detected().total_bytes.map(|b| b >> 20);
            let saving_total = checked.iter().any(|(key, value)| *key == "memory.total_mb" && value.is_some());
            let has = match (detected, self.settings.environment_memory().filter(|_| saving_total)) {
                (Some(d), Some(e)) => Some(d.min(e)),
                (d, e) => d.or(e),
            };
            if let (Some(said), Some(has)) = (mib("memory.total_mb").filter(|_| saving_total), has)
                && said > has
            {
                return Err(Refusal::new(refused::MORE_THAN_THE_SERVER, &[&said, &has]));
            }
            if let Some(said) = mib("memory.total_mb").filter(|_| saving_total) {
                // Under what it already uses without its caches, the guard would cut for ever.
                let now = crate::memory::MemoryInfo::now();
                let held = self.apps.cache_used() as u64;
                let uses = now.process_bytes.map_or(0, |p| p.saturating_sub(held));
                if said < (uses / 3 * 4) >> 20 {
                    return Err(Refusal::new(refused::MEMORY_TOO_LITTLE, &[&(uses >> 20), &((uses / 3 * 4) >> 20)]));
                }
            }
            let all = self.apps.all();
            let now = self.apps.by_hand(&all, &self.apps.plan());
            let mut then = now;
            // The memory counted on afterwards: what is saved now, else what is in force.
            then.total = match mib("memory.total_mb").filter(|_| saving_total) {
                Some(mib) => Some(mib << 20),
                None if saving_total => detected.map(|d| d.min(self.settings.environment_memory().unwrap_or(d)) << 20),
                None => crate::memory::MemoryInfo::now().total_bytes,
            };
            then.cache = mib("memory.cache_mb").map(|mib| (mib as usize) << 20);
            then.kv_each = mib("memory.kv_mb").map(|mib| (mib as usize) << 20);
            if let Some(cache) = then.cache
                && cache < then.apart + then.floor
                && now.cache.is_none_or(|before| cache < before)
            {
                return Err(Refusal::new(
                    refused::CACHE_UNDER_WHAT_IS_APART,
                    &[&(cache >> 20), &(then.apart >> 20), &(then.floor >> 20), &((then.apart + then.floor) >> 20)],
                ));
            }
            // What does not fit already is not refused a change that lowers it,
            // and giving a setting back is never refused.
            let removing = checked.iter().all(|(_, value)| value.is_none());
            let memory_changes: Vec<&(&str, Option<String>)> =
                checked.iter().filter(|(key, _)| key.starts_with("memory.")).collect();
            let figure = match memory_changes.as_slice() {
                [("memory.total_mb", Some(_))] => apps::Changed::Total,
                [("memory.cache_mb", Some(_))] => apps::Changed::CacheBudget,
                [("memory.kv_mb", Some(_))] => apps::Changed::KvEach,
                _ => apps::Changed::Several,
            };
            if let Err(refusal) = then.check(&format!("`{changed}`"), figure)
                && !removing
                && (now.fits() || then.amount() > now.amount() || then.total < now.total)
            {
                return Err(refusal);
            }
        }
        if let Some(Some(n)) = checked.iter().find(|(key, _)| *key == "postgres.connections").map(|(_, v)| v)
            && let Ok(n) = n.parse::<usize>()
            && n > pg_connections_most()
        {
            return Err(Refusal::new(refused::PG_MORE_THAN_THREADS, &[&n, &pg_connections_most()]));
        }
        let list: Vec<(&str, Option<&str>)> = checked.iter().map(|(key, value)| (*key, value.as_deref())).collect();
        if !self.settings.save(&list)? {
            return Ok(None);
        }
        self.apply_settings();
        Ok(self.ship_settings())
    }

    /// Sets the memory of one application (see [`apps::Apps::set_memory`]),
    /// one change of the settings at a time. Returns whether anything
    /// changed, and what kept it from reaching the backup yet, if anything did.
    pub(crate) fn set_app_memory(
        &self,
        app: &apps::App,
        cache: Option<usize>,
        kv: Option<usize>,
    ) -> std::result::Result<(bool, Option<String>), settings::Refusal> {
        let _guard = self.settings_changes.lock().unwrap_or_else(|e| e.into_inner());
        if self.settings.read_only() {
            // Where the environment rules, the amounts kept aside can still be removed.
            if (cache, kv) == (None, None) {
                return Ok(if self.apps.forget_kept_amounts(app)? {
                    (true, self.ship_settings())
                } else {
                    (false, None)
                });
            }
            return Err(settings::Refusal {
                forbidden: true,
                ..settings::Refusal::new(settings::refused::READ_ONLY, &[])
            });
        }
        if !self.apps.set_memory(app, cache, kv)? {
            return Ok((false, None));
        }
        Ok((true, self.ship_settings()))
    }

    /// Sends what was just saved (a setting, an application's memory) to the
    /// backup at once: on a disk the host erases at each restart, a restart
    /// in the next half minute would bring the earlier value back. Returns
    /// why it could not, when there is a backup and it failed.
    fn ship_settings(&self) -> Option<String> {
        self.engine.options().backup.as_ref()?;
        let failed = self.engine.checkpoint().and_then(|_| self.engine.backup_now(false)).err()?.to_string();
        self.engine.event("warn", format!("settings saved, but not yet in the backup: {failed}"));
        Some(failed)
    }

    /// Status without credentials shows the public subset.
    pub(crate) fn public_status(&self) -> bool {
        // A saved value, else what the configuration read in the environment (as before).
        match self.settings.saved("status.public") {
            Some(saved) => saved == "true",
            None => self.options.public_status,
        }
    }

    /// PostgreSQL connections served at once.
    pub(crate) fn pg_max_connections(&self) -> usize {
        match self.settings.saved("postgres.connections").and_then(|v| v.parse::<u64>().ok()) {
            // Each has a thread: a saved number keeps to half of what the
            // system lets this user start, whatever server it was saved on.
            Some(saved) => (saved as usize).clamp(1, pg_connections_most()),
            None => self.options.pg_max_connections.max(1),
        }
    }

    /// Times the memory guard emptied the caches.
    pub(crate) fn memory_reliefs(&self) -> u64 {
        self.memory_reliefs.load(Ordering::Relaxed)
    }

    /// Rolls back the interactive transactions left idle too long, or open
    /// too long however busy (each keeps the pages it sees).
    fn expire_transactions(&self) {
        let (idle, lifetime) = (self.options.transaction_timeout, self.options.transaction_lifetime);
        let mut txs = self.transactions.lock().unwrap_or_else(|e| e.into_inner());
        txs.retain(|_, tx| {
            tx.opened.elapsed() < lifetime && tx.last_used.lock().unwrap_or_else(|e| e.into_inner()).elapsed() < idle
        });
    }

    /// The schemas the clients of application `app` describe.
    pub(crate) fn client_schemas(&self, app: &str) -> Arc<limits::ClientSchemas> {
        let mut all = self.client_schemas.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(described) = all.get(app) {
            return described.clone();
        }
        let described = Arc::new(limits::ClientSchemas::new(self.described_total.clone()));
        all.insert(app.to_owned(), described.clone());
        described
    }

    /// Lets go of what was kept for the applications deleted since: their
    /// shares of the threads, the maps of their clients' schemas (the
    /// schemas themselves stay counted in the server's total, as they stay
    /// in memory).
    fn forget_deleted_apps(&self) {
        let exists = |app: &str| self.apps.get(app).is_some();
        self.admission.forget(exists);
        self.client_schemas.lock().unwrap_or_else(|e| e.into_inner()).retain(|app, _| exists(app));
    }

    /// The schemas the clients of application `app` described: how many, and their bytes.
    pub(crate) fn described_schemas(&self, app: &str) -> (usize, usize) {
        let all = self.client_schemas.lock().unwrap_or_else(|e| e.into_inner());
        all.get(app).map_or((0, 0), |c| c.usage())
    }

    /// A thread for a statement of `who` that reads or writes (see [`limits::Admission`]).
    pub(crate) async fn enter(&self, write: bool, who: Asker<'_>) -> Result<limits::Thread> {
        self.admission.enter(write, who, || self.apps.all().len() <= 1).await
    }

    /// Runs `f`, a statement of `who`, on a thread for reads or for writes
    /// (see [`limits::Admission`]), off the runtime.
    pub(crate) async fn statement<T: Send + 'static>(
        &self,
        write: bool,
        who: Asker<'_>,
        f: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let _thread = self.enter(write, who).await?;
        unblocked(f).await?
    }

    /// Runs `f`, a read of `shape`, for which the caller holds a thread for
    /// reads: right here when reads of its shape have proved short and a seat
    /// is free (see [`fast::Inline`]: a point read answers in microseconds,
    /// less than handing the worker's duties over would cost), else off the
    /// runtime. Either way its time is recorded for its shape.
    async fn read<T: Send + 'static>(&self, shape: Option<u64>, f: impl FnOnce() -> T + Send + 'static) -> Result<T> {
        if let Some(shape) = shape.filter(|s| self.read_costs.short(*s))
            && let Some(inline) = self.inline.get().and_then(Option::as_ref)
            && let Some(seat) = inline.seat(shape)
        {
            let started = Instant::now();
            let out = std::panic::catch_unwind(AssertUnwindSafe(f));
            drop(seat);
            self.read_costs.record(shape, started.elapsed());
            return out.map_err(|panic| Error::Pool(format!("the request failed: {}", limits::panic_text(&panic))));
        }
        let costs = self.read_costs.clone();
        unblocked(move || {
            let started = Instant::now();
            let out = f();
            if let Some(shape) = shape {
                costs.record(shape, started.elapsed());
            }
            out
        })
        .await
    }

    /// Reads a request of `who` (its JSON, its SQL; `read` says whether it
    /// writes) on a thread of the kind it looks like (`looks_like_write`, a
    /// guess on its first words), then runs it right there when the guess was
    /// right. Else it waits for a thread of its kind. A thread for writes only
    /// ever reads what looks like a write of a token that writes: slow reads,
    /// or a text that takes long to read, never keep the writes from their
    /// threads.
    async fn read_then_run<P, T, F>(
        &self,
        who: Asker<'_>,
        shape: Option<u64>,
        looks_like_write: bool,
        read: impl FnOnce() -> Result<(P, bool)> + Send + 'static,
        run: F,
    ) -> Result<T>
    where
        P: Send + 'static,
        T: Send + 'static,
        F: FnOnce(P) -> Result<T> + Send + 'static,
    {
        enum Step<T, P, F> {
            Done(T),
            Later(P, F, bool),
        }
        let for_writes = looks_like_write && !who.read_only;
        let thread = self.enter(for_writes, who).await?;
        // A write waits for the disk: never where it is.
        let step = self
            .read(shape.filter(|_| !for_writes), move || -> Result<Step<T, P, F>> {
                let (read, writes) = read()?;
                if writes != for_writes { Ok(Step::Later(read, run, writes)) } else { run(read).map(Step::Done) }
            })
            .await??;
        drop(thread);
        match step {
            Step::Done(out) => Ok(out),
            Step::Later(read, run, true) => self.statement(true, who, move || run(read)).await,
            Step::Later(read, run, false) => {
                let _thread = self.enter(false, who).await?;
                self.read(shape, move || run(read)).await?
            }
        }
    }

    /// Drops the keys of the key-value stores whose time has come.
    fn sweep_keys(&self) {
        for app in self.apps.all() {
            app.kv.sweep();
        }
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
    // The same bytes as `body.to_string()`, written straight into the buffer instead of
    // through `fmt::Display`: half the time for a page of rows. A JSON value always
    // serializes (its keys are strings).
    let mut res = Response::new(full(serde_json::to_vec(body).unwrap_or_default()));
    *res.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

pub(crate) fn error_response(e: &Error) -> Response<HttpBody> {
    if let Some(refusal) = limits::refusal(e) {
        return limits::refusal_response(refusal);
    }
    let (status, body) = wire::error_to_json(e);
    json_response(status, &body)
}

/// The HTTP status of the answer to `e`.
fn error_status(e: &Error) -> u16 {
    limits::refusal(e).map_or_else(|| wire::error_to_json(e).0, |r| r.status)
}

/// A read-only token's attempt at writing, as an error.
fn read_only_error() -> Error {
    refused(403, "forbidden", READ_ONLY)
}

pub(crate) fn unauthorized(message: &str) -> Response<HttpBody> {
    json_response(401, &json!({ "error": { "kind": "unauthorized", "message": message } }))
}

/// Compares in constant time: the answer's timing tells nothing about how
/// much of a secret was right.
pub(crate) use crate::hash::same_secret;

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

/// The body of `req`, `limit` bytes at most, with its share of the memory of
/// the requests being read (see [`limits::read_body`]).
async fn body_of(state: &State, req: Request<Incoming>, limit: usize) -> Result<limits::Body> {
    limits::read_body(&state.request_memory, req, limit, state.options.body_timeout).await
}

/// The body of `req`, as JSON, with the memory of its tree: for a large
/// one, read off the runtime.
async fn json_body(state: &State, req: Request<Incoming>) -> Result<(Json, limits::Held)> {
    let mut body = body_of(state, req, state.options.max_body_bytes).await?;
    body.for_json(&state.request_memory).await?;
    if body.bytes.len() < limits::LARGE_BODY {
        let json = body.json()?;
        return Ok((json, body.into_held()));
    }
    unblocked(move || body.json().map(|json| (json, body.into_held()))).await?
}

async fn read_body(state: &State, req: Request<Incoming>) -> Result<Bytes> {
    Ok(body_of(state, req, state.options.max_body_bytes).await?.bytes)
}

async fn read_json(state: &State, req: Request<Incoming>) -> Result<Json> {
    Ok(json_body(state, req).await?.0)
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
    let active = Counted::new(&state.metrics.active_requests);
    let path = req.uri().path().to_owned();
    let gzip = accepts_gzip(req.headers());
    let mut res = match CatchUnwind(Box::pin(route(&state, req, peer))).await {
        Ok(res) => res,
        Err(panic) => error_response(&Error::Pool(format!("the request failed: {}", limits::panic_text(&panic)))),
    };
    if gzip {
        res = compress(res).await;
    }
    drop(active);
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
    if path == "/api/apps"
        || path.starts_with("/api/apps/")
        || path == "/api/settings"
        || path.starts_with("/api/settings/")
    {
        return manage::route(state, req, &path).await;
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
    // A large answer takes milliseconds to compress: off the runtime.
    let packed = if bytes.len() < 256 << 10 {
        fast::gzip(&bytes)
    } else {
        let text = bytes.clone();
        unblocked(move || fast::gzip(&text)).await.ok().flatten()
    };
    match packed {
        Some(packed) => {
            parts.headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
            parts.headers.remove(header::CONTENT_LENGTH);
            Response::from_parts(parts, full(packed))
        }
        None => Response::from_parts(parts, full(bytes)),
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
        Err(e) => error_status(e) >= 500,
    };
    app.metrics.record(started.elapsed(), failed);
    let res = result.unwrap_or_else(|e| error_response(&e));
    Ok((res, origin.map(str::to_owned)))
}

/// A JSON answer whose text is already written.
pub(crate) fn json_bytes(status: u16, body: Vec<u8>) -> Response<HttpBody> {
    let mut res = Response::new(full(body));
    *res.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    res
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
        (&Method::POST, ["execute" | "transactions" | "branches" | "files", ..]) | (&Method::DELETE, _) => true,
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
    let who = Asker { app: &app.name, read_only };

    if segments.first() == Some(&"kv") {
        return kv::route(state, app, read_only, req).await;
    }
    if segments.first() == Some(&"files") {
        let name = segments[1..].join("/");
        return files::route(state, &app.name, read_only, req, &name).await;
    }
    match (method, segments.as_slice()) {
        // "Does my connection work?": which application the token opens. It
        // waits for no thread (it answers when statements fill them all), and
        // reads the catalog off the runtime.
        (Method::GET, ["ping"]) => {
            let (engine, at) = (engine.clone(), branch.clone());
            let tables = unblocked(move || -> Result<usize> {
                check_branch(&engine, &at)?;
                Ok(Embedded::from_engine(engine).tables().map(|t| t.len()).unwrap_or(0))
            })
            .await??;
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
                // A client may send a table's schema once, then its reference
                // (`memoire_orm::nuage::wire::schema_ref`) in its place.
                "schemaRefs": true,
                // `/v1/files`: the server keeps files (in a bucket).
                "files": state.options.files.is_some(),
                // `/v1/kv`: a key-value store in memory.
                "kv": true,
            }),
        )),
        (Method::POST, [op @ ("fetch" | "execute")]) => {
            let fetch = *op == "fetch";
            let (body, held) = json_body(state, req).await?;
            let large = held.large();
            let tx = match &tx_id {
                Some(id) => Some(state.transaction(id, &app.name, &branch)?),
                None => None,
            };
            let write = !fetch || tx.is_some();
            let (described, name) = (state.client_schemas(&app.name), app.name.clone());
            // The statement, its schemas checked before the cache keeps them.
            let decode = move || -> Result<(wire::OwnedQuery, limits::Held)> {
                let q = body.get("query").unwrap_or(&Json::Null);
                described.admit(q, &name)?;
                Ok((wire::query_from_json(q, &described.cache)?, held))
            };
            let (engine, s) = (engine.clone(), state.clone());
            let run = move |(query, _held): (wire::OwnedQuery, limits::Held)| {
                answer_statement(&engine, &branch, &s.schemas, query, tx, fetch, read_only)
            };
            let (answer, cached) = if write {
                // Writes, and a transaction's statements: on a thread for writes.
                state.statement(true, who, move || run(decode()?)).await?
            } else {
                // A read: decoded first (here, unless its body is large), for
                // its shape, which tells where it runs.
                let decoded = if large { unblocked(decode).await?? } else { decode()? };
                let shape = match &decoded.0 {
                    wire::OwnedQuery::Raw(sql, _) | wire::OwnedQuery::Script(sql) => {
                        Some(fast::sql_shape(std::iter::once(sql.as_str())))
                    }
                    query => fast::shape(query),
                };
                let _thread = state.enter(false, who).await?;
                state.read(shape, move || run(decoded)).await??
            };
            let mut res = json_bytes(200, answer);
            if !cached {
                res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            }
            Ok(res)
        }
        (Method::POST, ["batch"]) => {
            let (body, held) = json_body(state, req).await?;
            let db = state.db(engine, &branch)?;
            let (described, name) = (state.client_schemas(&app.name), app.name.clone());
            let looks_like_write = body
                .get("statements")
                .and_then(Json::as_array)
                .is_some_and(|list| list.iter().take(MAX_STATEMENTS + 1).any(|s| s.get("execute").is_some()));
            let read = move || -> Result<(_, bool)> {
                let list = body
                    .get("statements")
                    .and_then(Json::as_array)
                    .ok_or_else(|| Error::Conversion("missing `statements`".into()))?;
                if list.len() > MAX_STATEMENTS {
                    return Err(too_many_statements(list.len()));
                }
                let mut statements = Vec::with_capacity(list.len());
                for s in list {
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
                        return Err(read_only_error());
                    }
                    described.admit(q, &name)?;
                    statements.push((fetch, wire::query_from_json(q, &described.cache)?));
                }
                let writes = statements.iter().any(|(fetch, _)| !fetch);
                Ok(((statements, held), writes))
            };
            let run = move |(statements, _held): (Vec<(bool, wire::OwnedQuery)>, limits::Held)| {
                // `{"results": [...]}`, each answer written as it comes.
                let mut out = b"{\"results\":[".to_vec();
                if statements.iter().all(|(fetch, _)| *fetch) {
                    // Reads alone, as `Embedded::batch` runs them (one
                    // transaction that only reads), stopped once their answers
                    // pass what one answer may be.
                    let tx = db.begin_read()?;
                    for (i, (_, q)) in statements.iter().enumerate() {
                        if i > 0 {
                            out.push(b',');
                        }
                        out.extend_from_slice(&wire::rows_to_vec(&tx.fetch_rows(&q.as_query())?));
                        if out.len() > MAX_BATCH_ANSWER {
                            return Err(too_large(format!(
                                "the answers to this batch pass {} MiB after {} of its {} statements: ask for fewer rows at once",
                                MAX_BATCH_ANSWER >> 20,
                                i + 1,
                                statements.len()
                            )));
                        }
                    }
                } else {
                    let list: Vec<(bool, memoire_orm::Query<'_>)> =
                        statements.iter().map(|(f, q)| (*f, q.as_query())).collect();
                    for (i, r) in db.batch(&list)?.into_iter().enumerate() {
                        if i > 0 {
                            out.push(b',');
                        }
                        match r {
                            Ok(rows) => out.extend_from_slice(&wire::rows_to_vec(&rows)),
                            Err(done) => serde_json::to_writer(&mut out, &wire::exec_result_to_json(&done))
                                .map_err(|e| Error::Conversion(e.to_string()))?,
                        }
                    }
                }
                out.extend_from_slice(b"]}");
                Ok(out)
            };
            let answer = state.read_then_run(who, None, looks_like_write, read, run).await?;
            let mut res = json_bytes(200, answer);
            res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            Ok(res)
        }
        (Method::POST, ["transactions"]) => {
            let open = || {
                let txs = state.transactions.lock().unwrap_or_else(|e| e.into_inner());
                txs.values().filter(|t| t.app == app.name).count()
            };
            let max = state.options.max_transactions.max(1);
            let full = || {
                too_many(format!(
                    "application `{}` has {max} transactions open, the most it may: commit or roll one back first",
                    app.name
                ))
            };
            if open() >= max {
                return Err(full());
            }
            let db = state.db(engine, &branch)?;
            let tx = state.statement(true, who, move || db.begin_now()).await?;
            let id = random_token();
            let now = Instant::now();
            let open_tx = OpenTx { tx, app: app.name.clone(), branch, opened: now, last_used: Mutex::new(now) };
            {
                let mut txs = state.transactions.lock().unwrap_or_else(|e| e.into_inner());
                if txs.values().filter(|t| t.app == app.name).count() >= max {
                    drop(txs);
                    return Err(full());
                }
                txs.insert(id.clone(), Arc::new(open_tx));
            }
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
            state
                .statement(true, who, move || if commit { open.tx.commit_now() } else { open.tx.rollback_now() })
                .await?;
            Ok(json_response(200, &json!({ "ok": true })))
        }
        (Method::GET, ["branches"]) => {
            let engine = engine.clone();
            let list: Vec<Json> = state
                .statement(false, who, move || {
                    Ok(engine.branches().iter().map(memoire_orm::nuage::Branch::to_json).collect())
                })
                .await?;
            Ok(json_response(200, &json!({ "branches": list })))
        }
        // Branches change under the writer's lock, with a sync to disk: on a
        // thread for writes.
        (Method::POST, ["branches"]) => {
            let body = read_json(state, req).await?;
            let engine = engine.clone();
            let branch = state
                .statement(true, who, move || {
                    let name = body.get("name").and_then(Json::as_str).unwrap_or_default();
                    Ok(engine.create_branch(name, body.get("from").and_then(Json::as_str))?.to_json())
                })
                .await?;
            Ok(json_response(201, &branch))
        }
        (Method::DELETE, ["branches", name]) => {
            let (engine, at) = (engine.clone(), (*name).to_owned());
            if state.statement(true, who, move || engine.delete_branch(&at)).await? {
                Ok(json_response(200, &json!({ "deleted": name })))
            } else {
                Err(Error::NotFound)
            }
        }
        (Method::POST, ["branches", name, "reset"]) => {
            let (engine, at) = (engine.clone(), (*name).to_owned());
            if state.statement(true, who, move || engine.reset_branch(&at)).await? {
                Ok(json_response(200, &json!({ "reset": name })))
            } else {
                Err(Error::NotFound)
            }
        }
        (Method::POST, ["sql"]) => {
            let (body, held) = json_body(state, req).await?;
            let texts = || {
                let one = body.get("sql").and_then(Json::as_str);
                let several = body.get("statements").and_then(Json::as_array).map(Vec::as_slice).unwrap_or_default();
                let texts = several.iter().take(MAX_STATEMENTS + 1).filter_map(|s| s.get("sql").and_then(Json::as_str));
                one.into_iter().chain(texts)
            };
            // Its shape: its SQL texts (a large request runs off the runtime).
            let shape = (!held.large()).then(|| fast::sql_shape(texts()));
            let looks_like_write = !read_only && texts().any(|sql| !reads_by_its_first_word(sql));
            let (engine, s) = (engine.clone(), state.clone());
            // Reading the SQL is part of the read (off the runtime unless its
            // shape has proved short), on a thread of the kind its first words
            // tell: a statement that turns out to be of the other kind then
            // waits for a thread of its own.
            let (answer, cached) = state
                .read_then_run(
                    who,
                    shape,
                    looks_like_write,
                    move || {
                        let request = sql_request(body, held, read_only)?;
                        let writes = request.writes;
                        Ok((request, writes))
                    },
                    move |request| run_sql(&engine, &branch, &s.schemas, request),
                )
                .await?;
            let mut res = json_bytes(200, answer);
            if !cached {
                res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            }
            Ok(res)
        }
        _ => Err(Error::NotFound),
    }
}

/// Runs a statement of Memoire's protocol (`/v1/fetch`, `/v1/execute`), in
/// the open transaction when there is one, and writes its answer: rows, or
/// what was done. `false` with an answer that is not to be kept by a cache.
fn answer_statement(
    engine: &Arc<Engine>,
    branch: &str,
    schemas: &SchemaCache,
    query: wire::OwnedQuery,
    tx: Option<Arc<OpenTx>>,
    fetch: bool,
    read_only: bool,
) -> Result<(Vec<u8>, bool)> {
    // `db.raw(...)` and `db.raw_execute(...)` of Memoire's client: the SQL
    // layer runs them, as for `/v1/sql`, inside the open transaction when
    // there is one.
    if let Some((script, params)) = raw_script(&query)? {
        if read_only && script.writes() {
            return Err(read_only_error());
        }
        let results = match tx {
            Some(tx) => crate::sql::run_in(&tx.tx, schemas, &script, &params)?,
            None => {
                check_branch(engine, branch)?;
                crate::sql::run_values(engine, branch, schemas, &[(&script, &params)], true)?.pop().unwrap_or_default()
            }
        };
        let answer = if fetch {
            let rows = results.into_iter().rev().find(|r| !r.columns.is_empty()).map(|r| r.rows);
            json!({ "rows": wire::rows_to_json(&rows.unwrap_or_default()) })
        } else {
            let done = memoire_orm::ExecResult {
                rows_affected: results.iter().map(|r| r.rows_affected).sum(),
                last_insert_id: results.iter().rev().find_map(|r| r.last_insert_id.clone()),
            };
            wire::exec_result_to_json(&done)
        };
        return Ok((serde_json::to_vec(&answer).unwrap_or_default(), false));
    }
    let q = query.as_query();
    let done = |r: memoire_orm::ExecResult| {
        serde_json::to_vec(&wire::exec_result_to_json(&r)).map_err(|e| Error::Conversion(e.to_string()))
    };
    let answer = match (tx, fetch) {
        (Some(tx), true) => wire::rows_to_vec(&tx.tx.fetch_rows(&q)?),
        (Some(tx), false) => done(tx.tx.execute_now(&q)?)?,
        (None, true) => {
            check_branch(engine, branch)?;
            wire::rows_to_vec(&embedded::fetch_rows(engine, branch, &q)?)
        }
        (None, false) => {
            check_branch(engine, branch)?;
            done(embedded::execute_now(engine, branch, &q)?)?
        }
    };
    Ok((answer, true))
}

fn too_many_statements(n: usize) -> Error {
    too_large(format!("{n} statements in one request: a request takes {MAX_STATEMENTS} at most; send them in several"))
}

/// The SQL of a raw statement of Memoire's protocol (`Query::Raw`,
/// `Query::Script`), parsed, with its parameters; `None` for the others.
fn raw_script(query: &wire::OwnedQuery) -> Result<Option<(crate::sql::Script, Vec<memoire_orm::Value>)>> {
    let (sql, params) = match query {
        wire::OwnedQuery::Raw(sql, params) => (sql, params.clone()),
        wire::OwnedQuery::Script(sql) => (sql, Vec::new()),
        _ => return Ok(None),
    };
    check_sql(sql, params.len())?;
    Ok(Some((crate::sql::Script::parse(sql)?, params)))
}

/// Refuses an SQL text or a list of parameters that no statement may have.
fn check_sql(sql: &str, params: usize) -> Result<()> {
    if sql.len() > MAX_SQL_TEXT {
        return Err(too_large(format!(
            "the SQL text is {} bytes: a statement is {} MiB at most; send a long script in several requests",
            sql.len(),
            MAX_SQL_TEXT >> 20
        )));
    }
    if params > MAX_PARAMS {
        return Err(Error::Validation(format!(
            "SQL: {params} parameters given: a statement takes {MAX_PARAMS} at most"
        )));
    }
    Ok(())
}

/// The text `sql` reads, by its first word, before it is parsed (a guess
/// that only says on which thread it is parsed: the parse decides). Only the
/// start of the text is looked at, on the runtime.
fn reads_by_its_first_word(sql: &str) -> bool {
    let mut rest = &sql.as_bytes()[..sql.len().min(256)];
    loop {
        rest = rest.trim_ascii_start();
        if let Some(after) = rest.strip_prefix(b"--") {
            rest = after.iter().position(|b| *b == b'\n').map_or(&[], |n| &after[n..]);
        } else if let Some(after) = rest.strip_prefix(b"/*") {
            rest = after.windows(2).position(|w| w == b"*/").map_or(&[], |n| &after[n + 2..]);
        } else if let Some(after) = rest.strip_prefix(b"(") {
            rest = after;
        } else {
            break;
        }
    }
    let word = &rest[..rest.iter().position(|b| !b.is_ascii_alphabetic()).unwrap_or(rest.len())];
    // The statements that change nothing (`Stmt::writes`), and a text with no word.
    ["", "select", "with", "values", "explain", "begin", "start", "commit", "end", "rollback"]
        .iter()
        .any(|w| word.eq_ignore_ascii_case(w.as_bytes()))
}

/// A `/v1/sql` request, read and checked.
struct SqlRequest {
    batch: Vec<(crate::sql::Script, Vec<Json>)>,
    /// The answer lists the results (`{"results": [...]}`).
    several: bool,
    /// Rows as arrays, not objects.
    arrays: bool,
    writes: bool,
    /// The memory of the JSON the request was read into.
    _held: limits::Held,
}

/// Reads `{"sql", "params"}`, or `{"statements": [{"sql", "params"}, ...]}`:
/// every statement and its parameters are counted before anything is copied
/// or converted.
fn sql_request(mut body: Json, held: limits::Held, read_only: bool) -> Result<SqlRequest> {
    let arrays = body.get("arrays").and_then(Json::as_bool).unwrap_or(false);
    let mut batch = Vec::new();
    let several = match body.get_mut("statements") {
        Some(Json::Array(list)) => {
            if list.len() > MAX_STATEMENTS {
                return Err(too_many_statements(list.len()));
            }
            for s in list.iter_mut() {
                batch.push(script_of(s)?);
            }
            true
        }
        _ => {
            let (script, params) = script_of(&mut body)?;
            let several = script.len() > 1;
            batch.push((script, params));
            several
        }
    };
    let writes = batch.iter().any(|(s, _)| s.writes());
    if read_only && writes {
        return Err(read_only_error());
    }
    Ok(SqlRequest { batch, several, arrays, writes, _held: held })
}

/// Runs a `/v1/sql` request, one transaction, and writes its answer:
/// straight as its text when it can be (`true`), else through JSON values.
fn run_sql(engine: &Arc<Engine>, branch: &str, schemas: &SchemaCache, request: SqlRequest) -> Result<(Vec<u8>, bool)> {
    let results = crate::sql::run(engine, branch, schemas, &request.batch, true)?;
    let all: Vec<crate::sql::SqlResult> = results.into_iter().flatten().collect();
    let mut out = Vec::with_capacity(256);
    let single = !request.several && all.len() == 1;
    if !single {
        out.extend_from_slice(b"{\"results\":[");
    }
    let mut written = true;
    for (i, r) in all.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        written = written && fast::write_sql_result(&mut out, r, request.arrays);
    }
    if !single {
        out.extend_from_slice(b"]}");
    }
    if written {
        return Ok((out, true));
    }
    let mut all: Vec<Json> = all.iter().map(|r| sql_json(r, request.arrays)).collect();
    let answer = if single { all.remove(0) } else { json!({ "results": all }) };
    Ok((serde_json::to_vec(&answer).unwrap_or_default(), false))
}

/// A script of a `/v1/sql` request: `{"sql": "...", "params": [...]}`. Its
/// parameters are taken out of the request, not copied, once counted.
fn script_of(j: &mut Json) -> Result<(crate::sql::Script, Vec<Json>)> {
    if j.get("sql").and_then(Json::as_str).is_none() {
        return Err(Error::Conversion("missing `sql`".into()));
    }
    let params = match j.get_mut("params") {
        None | Some(Json::Null) => Vec::new(),
        Some(Json::Array(list)) => std::mem::take(list),
        Some(_) => return Err(Error::Conversion("`params` is a list of values".into())),
    };
    let sql = j.get("sql").and_then(Json::as_str).unwrap_or_default();
    check_sql(sql, params.len())?;
    let script = crate::sql::Script::parse(sql)?;
    if params.len() != script.params() {
        return Err(Error::Validation(format!(
            "SQL: {} parameter(s) given, the statement uses {}",
            params.len(),
            script.params()
        )));
    }
    Ok((script, params))
}

/// A statement's result as `/v1/sql` answers it: rows as objects (or arrays).
pub(crate) fn sql_json(r: &crate::sql::SqlResult, arrays: bool) -> Json {
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

/// The most PostgreSQL connections this server can serve at once: each has
/// a thread, and half of what the system lets this user start is kept for them.
pub(crate) fn pg_connections_most() -> usize {
    crate::memory::thread_limit().map_or(usize::MAX, |limit| (limit / 2).max(8))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request that panics answers 500, and is no longer counted as active.
    #[tokio::test]
    async fn a_panic_in_a_request_is_an_answer() {
        let active = AtomicU64::new(0);
        let request = async {
            let _counted = Counted::new(&active);
            assert_eq!(active.load(Ordering::Relaxed), 1);
            tokio::task::yield_now().await;
            panic!("a bug");
        };
        let caught = CatchUnwind(Box::pin(request)).await;
        let panic = caught.expect_err("the panic comes back");
        assert_eq!(limits::panic_text(&panic), "a bug");
        assert_eq!(active.load(Ordering::Relaxed), 0);
        // And a request dropped half-way (its client left).
        let pending = async {
            let _counted = Counted::new(&active);
            std::future::pending::<()>().await;
        };
        let mut pending = Box::pin(pending);
        assert!(futures_poll_once(pending.as_mut()).await.is_none());
        assert_eq!(active.load(Ordering::Relaxed), 1);
        drop(pending);
        assert_eq!(active.load(Ordering::Relaxed), 0);
    }

    /// Polls `f` once.
    async fn futures_poll_once<F: Future>(mut f: Pin<&mut F>) -> Option<F::Output> {
        std::future::poll_fn(|cx| {
            Poll::Ready(match f.as_mut().poll(cx) {
                Poll::Ready(out) => Some(out),
                Poll::Pending => None,
            })
        })
        .await
    }

    /// The SQL of a request is counted before it is read: parameters taken
    /// out, not copied, and refused past the limits.
    #[test]
    fn sql_requests_are_counted_before_they_are_read() {
        let held = || limits::Body::empty().into_held();
        let r = sql_request(json!({ "sql": "SELECT ? + ?", "params": [1, 2] }), held(), true).unwrap();
        assert_eq!((r.batch.len(), r.batch[0].1.len(), r.several, r.writes), (1, 2, false, false));
        let refused = |body: Json, read_only: bool| sql_request(body, held(), read_only).err().unwrap();
        let status = |e: &Error| error_status(e);
        assert_eq!(status(&refused(json!({ "sql": "SELECT 1", "params": vec![0; MAX_PARAMS + 1] }), false)), 400);
        assert_eq!(status(&refused(json!({ "sql": "SELECT 1", "params": [1] }), false)), 400);
        let many: Vec<Json> = (0..=MAX_STATEMENTS).map(|_| json!({ "sql": "SELECT 1" })).collect();
        assert_eq!(status(&refused(json!({ "statements": many }), false)), 413);
        assert_eq!(status(&refused(json!({ "sql": " ".repeat(MAX_SQL_TEXT + 1) }), false)), 413);
        assert_eq!(status(&refused(json!({ "sql": "DELETE FROM t" }), true)), 403);
        assert_eq!(status(&refused(json!({ "params": "x" }), false)), 400);
        let several = sql_request(json!({ "sql": "SELECT 1; SELECT 2" }), held(), false).unwrap();
        assert!(several.several);
    }

    /// The upkeep of every second runs on a thread of its own: it needs no
    /// worker of a runtime (here there is no runtime at all), so it goes on
    /// when every worker is busy.
    #[test]
    fn the_upkeep_needs_no_runtime() {
        let dir = std::env::temp_dir().join(format!("nuage-upkeep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let engine = Engine::open(&dir, crate::EngineOptions { sync: false, ..Default::default() }).unwrap();
        let options = ServerOptions { transaction_lifetime: Duration::from_millis(100), ..ServerOptions::default() };
        let state = Server::new(engine.clone(), options).state;
        let tx = state.db(&engine, MAIN).unwrap().begin_now().unwrap();
        let now = Instant::now();
        let open = OpenTx { tx, app: "a".into(), branch: MAIN.into(), opened: now, last_used: Mutex::new(now) };
        state.transactions.lock().unwrap().insert("t".into(), Arc::new(open));
        let (ticking, ticks) = std::sync::mpsc::channel();
        assert!(start_ticks(&state, ticks));
        let started = Instant::now();
        while state.open_transactions() > 0 {
            assert!(started.elapsed() < Duration::from_secs(10), "the upkeep did not run");
            std::thread::sleep(Duration::from_millis(50));
        }
        drop(ticking);
        drop(state);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The first word of an SQL text says on which thread it is parsed; the
    /// parse decides what it is.
    #[test]
    fn sql_texts_are_told_apart_by_their_first_word() {
        for reads in [
            "SELECT 1",
            "  select * from t",
            "-- a comment\nWITH x AS (SELECT 1) SELECT * FROM x",
            "/* a comment */ (SELECT 1) UNION (SELECT 2)",
            "VALUES (1)",
            "EXPLAIN SELECT 1",
            "BEGIN",
            "",
            "-- nothing but a comment",
        ] {
            assert!(reads_by_its_first_word(reads), "{reads}");
        }
        for writes in [
            "INSERT INTO t VALUES (1)",
            "\n\tupdate t set v = 1",
            "/* SELECT */ DELETE FROM t",
            "CREATE TABLE t (id INTEGER)",
            "DROP TABLE t",
            "selection",
            "SELECTION",
        ] {
            assert!(!reads_by_its_first_word(writes), "{writes}");
        }
        // Only the start of a text is looked at: a long comment says nothing.
        assert!(reads_by_its_first_word(&format!("/*{}*/ DELETE FROM t", " ".repeat(1000))));
    }
}
