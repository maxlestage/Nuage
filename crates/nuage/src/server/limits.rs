//! What one request may take of the server: a thread for its statements,
//! memory for its body, the schemas its client describes. Past a limit the
//! answer is a clear refusal (`{"error": {"kind", "message"}}`, with 400, 413,
//! 429 or 503 and `Retry-After`), never a queue without end, nor memory that
//! only grows.

use std::collections::HashSet;
use std::future::Future;
use std::hash::{BuildHasher, Hash, Hasher};
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use http_body_util::BodyExt;
use hyper::Request;
use hyper::body::{Bytes, Incoming};
use hyper::header::{self, HeaderValue};
use memoire_orm::nuage::wire::SchemaCache;
use memoire_orm::{Error, Result};
use serde_json::{Value as Json, json};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};

use super::{HttpBody, json_response};

/// Longest SQL text of one statement of `/v1/sql` (and of the raw SQL of the
/// protocol): a longer one is a dump, to send in parts.
pub(crate) const MAX_SQL_TEXT: usize = 1 << 20;
/// Most parameters of one statement: PostgreSQL's own limit.
pub(crate) const MAX_PARAMS: usize = 65_535;
/// Most statements in one request (`/v1/sql`, `/v1/batch`).
pub(crate) const MAX_STATEMENTS: usize = 1_000;
/// Most commands in one request to `/v1/kv`.
pub(crate) const MAX_KV_COMMANDS: usize = 10_000;
/// Longest form of the console's login page: a password and two tokens.
pub(crate) const LOGIN_FORM_BYTES: usize = 8 << 10;
/// Longest form of the console's other pages (the SQL of the query page).
pub(crate) const FORM_BYTES: usize = 1 << 20;
/// How long a request waits for a thread (or for memory to read its body)
/// before it is turned away with 503: long enough for a burst to pass, short
/// enough that nothing queues for good.
pub(crate) const ADMISSION_WAIT: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

/// A refusal with its own HTTP status, carried as an [`Error`] through `?`.
#[derive(Debug)]
pub(crate) struct Refusal {
    pub(crate) status: u16,
    pub(crate) kind: &'static str,
    pub(crate) message: String,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Refusal {}

pub(crate) fn refused(status: u16, kind: &'static str, message: impl Into<String>) -> Error {
    Error::Database(Box::new(Refusal { status, kind, message: message.into() }))
}

/// 413: the request is larger than the server takes.
pub(crate) fn too_large(message: impl Into<String>) -> Error {
    refused(413, "too_large", message)
}

/// 503: no room right now; the client tries again in a moment.
pub(crate) fn busy(message: impl Into<String>) -> Error {
    refused(503, "unavailable", message)
}

/// 429: this client holds too much already.
pub(crate) fn too_many(message: impl Into<String>) -> Error {
    refused(429, "too_many_requests", message)
}

/// The refusal `e` carries, if it is one.
pub(crate) fn refusal(e: &Error) -> Option<&Refusal> {
    match e {
        Error::Database(d) => d.downcast_ref::<Refusal>(),
        _ => None,
    }
}

/// The answer to a refusal.
pub(crate) fn refusal_response(r: &Refusal) -> hyper::Response<HttpBody> {
    let mut res = json_response(r.status, &json!({ "error": { "kind": r.kind, "message": r.message } }));
    if matches!(r.status, 429 | 503) {
        res.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    }
    res
}

/// What a client sees when the server holds as many connections as it may:
/// written as it is to the socket, which is closed at once.
pub(crate) fn full_answer() -> Vec<u8> {
    let body = json!({ "error": {
        "kind": "unavailable",
        "message": "the server holds as many connections as it may: try again in a moment",
    } })
    .to_string();
    format!(
        "HTTP/1.1 503 Service Unavailable\r\ncontent-type: application/json\r\nretry-after: 1\r\nconnection: close\r\n\
         content-length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

// ---------------------------------------------------------------------------
// Work off the runtime
// ---------------------------------------------------------------------------

/// The runtime has worker threads to hand a blocked one's duties to.
fn multi_thread() -> bool {
    tokio::runtime::Handle::try_current()
        .is_ok_and(|h| matches!(h.runtime_flavor(), tokio::runtime::RuntimeFlavor::MultiThread))
}

/// Runs `f`, work that may take long (reading SQL, a statement, an answer to
/// write), so that it never holds up the runtime: a worker busy with it would
/// stop polling sockets and timers for every other connection. On the
/// multi-threaded runtime it runs right here, and the worker's duties go to
/// another thread meanwhile (no hop for `f`, which keeps a point read at a
/// few microseconds); elsewhere, on the blocking pool. A panic in `f` comes
/// back as an error, as from the pool.
pub(crate) async fn unblocked<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T> {
    if multi_thread() {
        return std::panic::catch_unwind(AssertUnwindSafe(|| tokio::task::block_in_place(f)))
            .map_err(|panic| Error::Pool(format!("the request failed: {}", panic_text(&panic))));
    }
    tokio::task::spawn_blocking(f).await.map_err(|e| Error::Pool(e.to_string()))
}

/// What a panic said.
pub(crate) fn panic_text(panic: &Box<dyn std::any::Any + Send>) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a bug in the server".to_owned())
}

/// Threads for statements: reads and writes have their own, so that a burst
/// of slow reads never keeps a write waiting, and neither queues without
/// end. They stay under the blocking pool's size, with room left for the
/// status, the console and the workers' duties.
pub(crate) struct Admission {
    reads: Arc<Semaphore>,
    writes: Arc<Semaphore>,
}

impl Admission {
    /// `threads` for statements in all, a quarter of them (two at least) for writes.
    pub(crate) fn new(threads: usize) -> Admission {
        let threads = threads.max(4);
        let writes = (threads / 4).max(2);
        Admission { reads: Arc::new(Semaphore::new(threads - writes)), writes: Arc::new(Semaphore::new(writes)) }
    }

    /// A thread for a statement that reads (`false`) or writes (`true`): at
    /// once when one is free, else after a short wait, else a 503.
    pub(crate) async fn enter(&self, write: bool) -> Result<OwnedSemaphorePermit> {
        let threads = if write { &self.writes } else { &self.reads };
        if let Ok(permit) = threads.clone().try_acquire_owned() {
            return Ok(permit);
        }
        match tokio::time::timeout(ADMISSION_WAIT, threads.clone().acquire_owned()).await {
            Ok(Ok(permit)) => Ok(permit),
            _ => Err(busy(format!(
                "every thread for {} is busy (slow statements): try again in a moment",
                if write { "writes" } else { "reads" }
            ))),
        }
    }

    /// A thread to read a request with (its JSON, its SQL) before it is
    /// known whether it writes: one for reads, or else one for writes when
    /// one is free (`true`), so that a write is never kept from being read by
    /// slow reads holding all theirs.
    pub(crate) async fn enter_to_read(&self) -> Result<(OwnedSemaphorePermit, bool)> {
        if let Ok(permit) = self.reads.clone().try_acquire_owned() {
            return Ok((permit, false));
        }
        if let Ok(permit) = self.writes.clone().try_acquire_owned() {
            return Ok((permit, true));
        }
        Ok((self.enter(false).await?, false))
    }

    /// Threads free now, for reads and for writes.
    #[cfg(test)]
    pub(crate) fn free(&self) -> (usize, usize) {
        (self.reads.available_permits(), self.writes.available_permits())
    }
}

/// Threads for statements by default: the blocking pool the binary gives
/// the runtime, less a few for the status, the console and the files.
pub(crate) fn statement_threads() -> usize {
    crate::memory::blocking_threads().saturating_sub(4).max(4)
}

/// Connections served at once by default: half the files the system lets
/// the process open (the rest are the databases' files and the other ports),
/// between 32 and 4096.
pub(crate) fn default_max_connections() -> usize {
    #[cfg(unix)]
    {
        let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        // SAFETY: `getrlimit` fills the struct it is given.
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0 && limit.rlim_cur != libc::RLIM_INFINITY {
            return usize::try_from(limit.rlim_cur / 2).unwrap_or(4096).clamp(32, 4096);
        }
    }
    1024
}

/// A connection whose writes give up once the client has taken nothing of
/// them for `limit`: an answer that a client does not read is not kept in
/// memory, with its connection, for as long as the client likes.
pub(crate) struct Patient<S> {
    inner: S,
    limit: Duration,
    /// Since when the client takes nothing.
    stalled: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl<S> Patient<S> {
    pub(crate) fn new(inner: S, limit: Duration) -> Self {
        Patient { inner, limit, stalled: None }
    }

    /// A write that could not go on: an error once it has waited `limit`.
    fn waited(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Error> {
        let limit = self.limit;
        let stalled = self.stalled.get_or_insert_with(|| Box::pin(tokio::time::sleep(limit)));
        stalled.as_mut().poll(cx).map(|()| {
            std::io::Error::new(std::io::ErrorKind::TimedOut, "the client took nothing of the answer for too long")
        })
    }

    fn went_on<T>(&mut self, done: Poll<std::io::Result<T>>, cx: &mut Context<'_>) -> Poll<std::io::Result<T>> {
        match done {
            Poll::Ready(done) => {
                self.stalled = None;
                Poll::Ready(done)
            }
            Poll::Pending => self.waited(cx).map(Err),
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Patient<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Patient<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        let done = Pin::new(&mut self.inner).poll_write(cx, buf);
        self.went_on(done, cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let done = Pin::new(&mut self.inner).poll_write_vectored(cx, bufs);
        self.went_on(done, cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let done = Pin::new(&mut self.inner).poll_flush(cx);
        self.went_on(done, cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

// ---------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------

/// The memory that the requests being read may hold together (their bodies,
/// and the JSON they are read into), counted in KiB: past it a request waits
/// a moment, then is turned away, instead of the server running out.
pub(crate) struct RequestMemory {
    kib: Arc<Semaphore>,
    total: usize,
}

impl RequestMemory {
    pub(crate) fn new(bytes: usize) -> RequestMemory {
        let total = (bytes >> 10).clamp(1 << 10, Semaphore::MAX_PERMITS);
        RequestMemory { kib: Arc::new(Semaphore::new(total)), total }
    }

    /// The memory that the requests being read may hold together, in bytes.
    pub(crate) fn total(&self) -> usize {
        self.total << 10
    }

    /// `bytes` more for a request, added to what it `held`.
    async fn reserve(&self, held: &mut Option<OwnedSemaphorePermit>, bytes: usize) -> Result<()> {
        let kib = bytes.div_ceil(1024);
        let has = held.as_ref().map_or(0, OwnedSemaphorePermit::num_permits);
        if has + kib > self.total {
            return Err(too_large(format!(
                "this request would take {} MiB of memory once read: the server reads {} MiB of requests at once; send it in smaller parts",
                ((has + kib) >> 10).max(1),
                self.total >> 10
            )));
        }
        let n = u32::try_from(kib).unwrap_or(u32::MAX);
        let permit = match self.kib.clone().try_acquire_many_owned(n) {
            Ok(p) => p,
            Err(TryAcquireError::NoPermits) => {
                match tokio::time::timeout(ADMISSION_WAIT, self.kib.clone().acquire_many_owned(n)).await {
                    Ok(Ok(p)) => p,
                    _ => return Err(busy("the server is reading many large requests at once: try again in a moment")),
                }
            }
            Err(TryAcquireError::Closed) => return Err(busy("the server is stopping")),
        };
        match held {
            Some(h) => h.merge(permit),
            None => *held = Some(permit),
        }
        Ok(())
    }
}

/// A request's body, holding its share of the memory of the requests being
/// read until it is dropped.
pub(crate) struct Body {
    pub(crate) bytes: Bytes,
    held: Option<OwnedSemaphorePermit>,
}

impl Body {
    /// Keeps also the memory of the JSON tree the body is about to be read
    /// into (see [`json_weight`]): a few bytes of text can take a hundred
    /// times as much once parsed.
    pub(crate) async fn for_json(&mut self, memory: &RequestMemory) -> Result<()> {
        let more = json_weight(&self.bytes).saturating_sub(self.bytes.len());
        memory.reserve(&mut self.held, more).await
    }

    /// No body.
    #[cfg(test)]
    pub(crate) fn empty() -> Body {
        Body { bytes: Bytes::new(), held: None }
    }

    /// The body as JSON (`null` when empty).
    pub(crate) fn json(&self) -> Result<Json> {
        if self.bytes.is_empty() {
            return Ok(Json::Null);
        }
        serde_json::from_slice(&self.bytes).map_err(|e| Error::Conversion(format!("invalid JSON: {e}")))
    }

    /// Lets go of the text, keeping the memory it was given for what was read from it.
    pub(crate) fn into_held(self) -> Held {
        Held { _permit: self.held, large: self.bytes.len() >= LARGE_BODY }
    }
}

/// A body this large is read (and what is in it decoded) off the runtime.
pub(crate) const LARGE_BODY: usize = 64 << 10;

/// The memory a request was given, until it is dropped.
pub(crate) struct Held {
    _permit: Option<OwnedSemaphorePermit>,
    large: bool,
}

impl Held {
    /// The body was large: what is read from it is read off the runtime.
    pub(crate) fn large(&self) -> bool {
        self.large
    }
}

/// Reads the body of `req`: `limit` bytes at most (413 past it, as soon as
/// `Content-Length` says so), within `timeout` (408 after it), each part
/// counted in the memory of the requests being read as it arrives (a client
/// that announces a large body and sends nothing holds nothing).
pub(crate) async fn read_body(
    memory: &RequestMemory,
    req: Request<Incoming>,
    limit: usize,
    timeout: Duration,
) -> Result<Body> {
    let declared = req.headers().get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()?.trim().parse::<u64>().ok());
    if let Some(n) = declared.filter(|n| *n > limit as u64) {
        return Err(too_large(format!("the request's body is {n} bytes: this takes {limit} at most")));
    }
    let deadline = tokio::time::Instant::now() + timeout;
    let mut body = req.into_body();
    let mut held = None;
    let mut parts: Vec<Bytes> = Vec::new();
    let mut size = 0usize;
    loop {
        let frame = match tokio::time::timeout_at(deadline, body.frame()).await {
            Err(_) => {
                return Err(refused(
                    408,
                    "timeout",
                    format!("the request's body did not arrive within {} s", timeout.as_secs()),
                ));
            }
            Ok(None) => break,
            Ok(Some(Err(e))) => return Err(Error::Validation(format!("cannot read the request: {e}"))),
            Ok(Some(Ok(frame))) => frame,
        };
        let Ok(data) = frame.into_data() else { continue };
        if size + data.len() > limit {
            return Err(too_large(format!("the request's body is larger than {limit} bytes, the most this takes")));
        }
        memory.reserve(&mut held, data.len()).await?;
        size += data.len();
        parts.push(data);
    }
    let bytes = match parts.len() {
        0 => Bytes::new(),
        1 => parts.pop().unwrap_or_default(),
        _ => {
            let mut all = Vec::with_capacity(size);
            for part in &parts {
                all.extend_from_slice(part);
            }
            Bytes::from(all)
        }
    };
    Ok(Body { bytes, held })
}

/// What a JSON text takes in memory once parsed into a tree of
/// `serde_json::Value`, counted on its bytes without building it: its text,
/// a slot (and the spare room of a growing list) per value, a node per
/// object. `[0,0,0,...]` takes some thirty times its size, `{"i32":1}` some
/// seventy.
pub(crate) fn json_weight(text: &[u8]) -> usize {
    let (mut values, mut objects) = (0usize, 0usize);
    let mut in_scalar = false;
    let mut i = 0;
    while i < text.len() {
        match text[i] {
            b'"' => {
                values += 1;
                in_scalar = false;
                i += 1;
                while i < text.len() && text[i] != b'"' {
                    i += if text[i] == b'\\' { 2 } else { 1 };
                }
            }
            b'{' => {
                objects += 1;
                values += 1;
                in_scalar = false;
            }
            b'[' => {
                values += 1;
                in_scalar = false;
            }
            b',' | b':' | b']' | b'}' | b' ' | b'\t' | b'\n' | b'\r' => in_scalar = false,
            _ if !in_scalar => {
                values += 1;
                in_scalar = true;
            }
            _ => {}
        }
        i += 1;
    }
    text.len().saturating_add(values.saturating_mul(64)).saturating_add(objects.saturating_mul(640))
}

// ---------------------------------------------------------------------------
// Schemas described by clients
// ---------------------------------------------------------------------------

/// Most distinct schemas the clients of one application may describe while
/// the server runs (a client describes each table once, then sends its
/// reference).
pub(crate) const MAX_CLIENT_SCHEMAS: usize = 1_000;
/// Most text those schemas and the names of their statements may hold, for
/// one application: what is kept stays for as long as the process (the
/// protocol's statements point to schemas that live that long).
pub(crate) const MAX_CLIENT_SCHEMA_BYTES: usize = 4 << 20;
/// Longest name of a table, a column or an index: the engine's own rule
/// (and PostgreSQL's).
pub(crate) const MAX_NAME: usize = 63;
/// Longest SQL type or default of a column.
const MAX_COLUMN_TEXT: usize = 4 << 10;
/// Most columns of a table, and most indexes: PostgreSQL's limit.
const MAX_COLUMNS: usize = 1_600;
/// Longest description of one table.
const MAX_SCHEMA_TEXT: usize = 512 << 10;

/// The schemas that the clients of one application describe, kept apart
/// from the tables' own (that the SQL layer and the other applications use):
/// filling this one can never stop another application, nor SQL. Each new
/// schema and name is checked before it is kept, and the whole is bounded.
pub(crate) struct ClientSchemas {
    pub(crate) cache: SchemaCache,
    seen: Mutex<Seen>,
    hasher: std::collections::hash_map::RandomState,
}

#[derive(Default)]
struct Seen {
    kept: HashSet<u64>,
    bytes: usize,
    schemas: usize,
}

/// What one statement would add to the cache: schemas described whole, and
/// the names of its columns.
#[derive(Default)]
struct Found<'a> {
    schemas: Vec<&'a Json>,
    names: Vec<&'a str>,
}

fn check_name(what: &str, name: &str, empty: bool) -> Result<()> {
    if (name.is_empty() && !empty) || name.len() > MAX_NAME || name.chars().any(char::is_control) {
        return Err(Error::Validation(format!(
            "{what} `{}`: a name is 1 to {MAX_NAME} bytes, without control characters",
            name.chars().take(80).collect::<String>()
        )));
    }
    Ok(())
}

fn text_of<'a>(j: &'a Json, key: &str) -> Option<&'a str> {
    j.get(key).and_then(Json::as_str)
}

/// The length of `j` as text, counted without writing it.
fn text_len(j: &Json) -> usize {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    let _ = serde_json::to_writer(&mut count, j);
    count.0
}

/// Checks the description of a table before anything of it is kept (nor
/// even copied).
fn check_schema(j: &Json) -> Result<()> {
    let table = text_of(j, "name").unwrap_or_default();
    check_name("table", table, false)?;
    let bad = |message: String| Err(Error::Validation(format!("the schema of table `{table}`: {message}")));
    let len = text_len(j);
    if len > MAX_SCHEMA_TEXT {
        return bad(format!("its description is {len} bytes, more than the {MAX_SCHEMA_TEXT} it may be"));
    }
    let columns = j.get("columns").and_then(Json::as_array).map(Vec::as_slice).unwrap_or_default();
    let indexes = j.get("indexes").and_then(Json::as_array).map(Vec::as_slice).unwrap_or_default();
    if columns.len() > MAX_COLUMNS || indexes.len() > MAX_COLUMNS {
        return bad(format!("a table has {MAX_COLUMNS} columns and {MAX_COLUMNS} indexes at most"));
    }
    for c in columns {
        for key in ["name", "field"] {
            if let Some(name) = text_of(c, key) {
                check_name("column", name, false)?;
            }
        }
        for key in ["sqlType", "default"] {
            if text_of(c, key).is_some_and(|t| t.len() > MAX_COLUMN_TEXT) {
                return bad(format!("a column's `{key}` is {MAX_COLUMN_TEXT} bytes at most"));
            }
        }
        if let Some(r) = c.get("references") {
            for key in ["table", "column", "onDelete", "onUpdate"] {
                if let Some(name) = text_of(r, key) {
                    check_name("reference", name, false)?;
                }
            }
        }
    }
    for ix in indexes {
        check_name("index", text_of(ix, "name").unwrap_or_default(), true)?;
    }
    Ok(())
}

impl<'a> Found<'a> {
    fn schema(&mut self, j: &'a Json) {
        // A reference (a string) names a schema already kept: nothing new.
        if j.is_object() {
            self.schemas.push(j);
        }
    }

    /// A statement of the protocol, read the way `wire::query_from_json` reads it.
    fn statement(&mut self, j: &'a Json) {
        let Some(obj) = j.as_object() else { return };
        if let Some(s) = obj.get("select") {
            return self.select(s);
        }
        for key in [
            "insert",
            "update",
            "delete",
            "findByKey",
            "insertRow",
            "upsertRow",
            "updateRow",
            "deleteByKey",
            "createTable",
        ] {
            if let Some(t) = obj.get(key) {
                self.schema(t);
            }
        }
        for set in obj.get("sets").and_then(Json::as_array).map(Vec::as_slice).unwrap_or_default() {
            if let Some([_, e]) = set.as_array().map(Vec::as_slice) {
                self.expr(e);
            }
        }
        if let Some(f) = obj.get("filter") {
            self.expr(f);
        }
    }

    fn select(&mut self, j: &'a Json) {
        if let Some(t) = j.get("from") {
            self.schema(t);
        }
        for join in j.get("joins").and_then(Json::as_array).map(Vec::as_slice).unwrap_or_default() {
            if let Some(t) = join.get("table") {
                self.schema(t);
            }
            if let Some(on) = join.get("on") {
                self.expr(on);
            }
        }
        let exprs = |j: Option<&'a Json>| j.and_then(Json::as_array).map(Vec::as_slice).unwrap_or_default();
        for e in exprs(j.get("projection").and_then(|p| p.get("exprs"))).iter().chain(exprs(j.get("groupBy"))) {
            self.expr(e);
        }
        for o in exprs(j.get("order")) {
            if let Some(e) = o.get("expr") {
                self.expr(e);
            }
        }
        for key in ["filter", "having"] {
            if let Some(e) = j.get(key) {
                self.expr(e);
            }
        }
    }

    /// An expression, read in the order `wire` reads it (the first key it knows wins).
    fn expr(&mut self, j: &'a Json) {
        let Some(obj) = j.as_object() else { return };
        if let Some(c) = obj.get("col") {
            if let Some(a) = c.as_array().filter(|a| a.len() == 4) {
                self.names.extend(a[..2].iter().filter_map(Json::as_str));
            }
            return;
        }
        if obj.contains_key("val") {
            return;
        }
        if let Some(b) = obj.get("bin").and_then(Json::as_array).filter(|a| a.len() == 3) {
            self.expr(&b[1]);
            self.expr(&b[2]);
            return;
        }
        for key in ["not", "isNull", "in", "between", "low", "high", "like", "pattern", "match", "arg", "inQuery"] {
            if let Some(e) = obj.get(key) {
                self.expr(e);
            }
        }
        if obj.contains_key("inQuery")
            && let Some(q) = obj.get("query")
        {
            self.select(q);
        }
    }
}

impl ClientSchemas {
    pub(crate) fn new() -> ClientSchemas {
        ClientSchemas {
            cache: SchemaCache::new(MAX_CLIENT_SCHEMAS),
            seen: Mutex::new(Seen::default()),
            hasher: std::collections::hash_map::RandomState::new(),
        }
    }

    fn hash(&self, kind: u8, text: &str) -> u64 {
        let mut h = self.hasher.build_hasher();
        kind.hash(&mut h);
        text.hash(&mut h);
        h.finish()
    }

    /// Checks what the statement `j` would add to the cache (its schemas
    /// described whole, the names of its columns) and counts it: refused when
    /// a name or a schema is out of bounds, or when the application's clients
    /// described too much already.
    pub(crate) fn admit(&self, j: &Json, app: &str) -> Result<()> {
        let mut found = Found::default();
        found.statement(j);
        let mut new: Vec<(u64, usize, bool)> = Vec::new();
        for schema in &found.schemas {
            check_schema(schema)?;
            // The text the cache keys it by.
            let text = serde_json::to_string(schema).map_err(|e| Error::Conversion(e.to_string()))?;
            new.push((self.hash(0, &text), text.len(), true));
        }
        for name in &found.names {
            check_name("column", name, true)?;
            new.push((self.hash(1, name), name.len() + 64, false));
        }
        if new.is_empty() {
            return Ok(());
        }
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        new.retain(|(h, _, _)| !seen.kept.contains(h));
        new.sort_unstable();
        new.dedup_by_key(|(h, _, _)| *h);
        let bytes: usize = new.iter().map(|(_, n, _)| n).sum();
        let schemas = new.iter().filter(|(_, _, s)| *s).count();
        if seen.bytes + bytes > MAX_CLIENT_SCHEMA_BYTES || seen.schemas + schemas > MAX_CLIENT_SCHEMAS {
            return Err(Error::Unsupported(format!(
                "the clients of application `{app}` described more distinct table schemas than the server keeps \
                 ({MAX_CLIENT_SCHEMAS}, {} MiB) since it started: a client describes a table once, then sends its reference",
                MAX_CLIENT_SCHEMA_BYTES >> 20
            )));
        }
        seen.bytes += bytes;
        seen.schemas += schemas;
        seen.kept.extend(new.iter().map(|(h, _, _)| *h));
        Ok(())
    }

    /// Schemas kept, and the bytes counted for them.
    pub(crate) fn usage(&self) -> (usize, usize) {
        let seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        (seen.schemas, seen.bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_weights() {
        let zeros = format!("[{}0]", "0,".repeat(1000));
        let weight = json_weight(zeros.as_bytes());
        assert!((1001 * 64..1001 * 64 + 4000).contains(&weight), "{weight}");
        let objects = format!("[{}]", vec![r#"{"i32":1}"#; 100].join(","));
        assert!(json_weight(objects.as_bytes()) > 100 * 640);
        // Strings are skipped whole, escapes included: their brackets count for nothing.
        let text = r#"{"sql":"SELECT '[{\"x\": 1}]'"}"#;
        assert_eq!(json_weight(text.as_bytes()), text.len() + 3 * 64 + 640);
        assert_eq!(json_weight(b""), 0);
        let scalars = br#"{"a": true, "b": -1.5e3, "c": null}"#;
        assert_eq!(json_weight(scalars), scalars.len() + 7 * 64 + 640);
    }

    fn schema(name: &str, columns: usize) -> Json {
        let columns: Vec<Json> = (0..columns).map(|i| json!({ "name": format!("c{i}"), "kind": "int" })).collect();
        json!({ "name": name, "pk": 0, "columns": columns, "indexes": [] })
    }

    #[test]
    fn described_schemas_are_checked_and_bounded() {
        let described = ClientSchemas::new();
        let fetch = |t: Json| json!({ "findByKey": t, "key": 1 });
        described.admit(&fetch(schema("users", 3)), "a").unwrap();
        // The same schema again, or by reference: nothing more is counted.
        let once = described.usage();
        described.admit(&fetch(schema("users", 3)), "a").unwrap();
        described.admit(&fetch(json!("users#0123")), "a").unwrap();
        assert_eq!(described.usage(), once);
        assert_eq!(once.0, 1);

        // Names out of the engine's rule, or descriptions too large, are refused before anything is kept.
        for bad in [
            fetch(schema(&"n".repeat(64), 1)),
            fetch(schema("", 1)),
            fetch(schema("t", MAX_COLUMNS + 1)),
            fetch(
                json!({ "name": "t", "pk": 0, "columns": [{ "name": "x", "kind": "text", "default": "d".repeat(5000) }] }),
            ),
            json!({ "select": { "from": schema("t", 1), "projection": { "exprs": [] },
                "filter": { "bin": ["=", { "col": ["t", "x".repeat(100), 0, "int"] }, { "val": 1 }] } } }),
            json!({ "select": { "from": "t#00", "projection": { "exprs": [] },
                "filter": { "inQuery": { "col": ["t", "x", 0, "int"] }, "query": { "from": schema(&"q".repeat(70), 1) } } } }),
        ] {
            let err = described.admit(&bad, "a").unwrap_err();
            assert!(matches!(&err, Error::Validation(m) if m.contains("63 bytes") || m.contains("at most")), "{err:?}");
        }
        assert_eq!(described.usage(), once);

        // A client that describes ever new schemas is stopped, its application's alone.
        let mut refused = None;
        for i in 0..2 * MAX_CLIENT_SCHEMAS {
            if let Err(e) = described.admit(&fetch(schema(&format!("t{i}"), 40)), "a") {
                refused = Some((i, e));
                break;
            }
        }
        let (i, e) = refused.expect("refused at some point");
        assert!(i < MAX_CLIENT_SCHEMAS, "{i}");
        assert!(matches!(&e, Error::Unsupported(m) if m.contains("application `a`")), "{e:?}");
        let (schemas, bytes) = described.usage();
        assert!(schemas <= MAX_CLIENT_SCHEMAS && bytes <= MAX_CLIENT_SCHEMA_BYTES);
        // What was kept before still works.
        described.admit(&fetch(schema("users", 3)), "a").unwrap();
        ClientSchemas::new().admit(&fetch(schema("t0", 40)), "b").unwrap();
    }

    #[tokio::test]
    async fn admission_keeps_threads_for_writes_and_turns_away() {
        assert_eq!(Admission::new(12).free(), (9, 3));
        let a = Admission::new(1);
        assert_eq!(a.free(), (2, 2));
        let held = (a.enter(false).await.unwrap(), a.enter(false).await.unwrap());
        // No read thread free: a short wait, then a clear 503.
        let started = std::time::Instant::now();
        let e = a.enter(false).await.unwrap_err();
        assert!(started.elapsed() >= ADMISSION_WAIT);
        let r = refusal(&e).expect("a refusal");
        assert_eq!((r.status, r.kind), (503, "unavailable"));
        assert_eq!(refusal_response(r).headers()[header::RETRY_AFTER], "1");
        // Writes still have theirs, and a read thread given back is taken at once.
        let write = a.enter(true).await.unwrap();
        drop(held.0);
        let read = a.enter(false).await.unwrap();
        drop((held.1, write, read));
        assert_eq!(a.free(), (2, 2));
    }

    #[tokio::test]
    async fn request_memory_is_shared_and_bounded() {
        let memory = RequestMemory::new(2 << 20);
        let mut a = None;
        memory.reserve(&mut a, 1 << 20).await.unwrap();
        memory.reserve(&mut a, 512 << 10).await.unwrap();
        // More than all of it: refused at once.
        let mut big = None;
        let e = memory.reserve(&mut big, 3 << 20).await.unwrap_err();
        assert_eq!(refusal(&e).unwrap().status, 413);
        // More than what is left: a wait, then 503; room again once given back.
        let mut b = None;
        let e = memory.reserve(&mut b, 1 << 20).await.unwrap_err();
        assert_eq!(refusal(&e).unwrap().status, 503);
        drop(a);
        memory.reserve(&mut b, 2 << 20).await.unwrap();
    }

    #[test]
    fn the_answer_to_a_full_server_is_whole() {
        let answer = String::from_utf8(full_answer()).unwrap();
        let (head, body) = answer.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with("HTTP/1.1 503") && head.contains(&format!("content-length: {}", body.len())));
        assert_eq!(serde_json::from_str::<Json>(body).unwrap()["error"]["kind"], "unavailable");
    }
}
