//! The PostgreSQL protocol (version 3): every language's PostgreSQL driver,
//! ORM and tool (psql, psycopg, JDBC, node-postgres, pgx, Npgsql, the `pg`
//! gem, PDO, libpq and everything built on it, DBeaver...) talks to Nuage DB.
//!
//! * **Login**: any user name; the **password is a token** of an application,
//!   which picks its database, as on `/v1/sql`. The database name picks a
//!   branch (`main`, the application's name or empty: the main branch).
//! * **TLS**: with a certificate ([`PgOptions::tls`]), `sslmode=require` and
//!   above work; without one the server answers that it has no TLS, and
//!   clients that prefer TLS go on in clear.
//! * **Queries**: the simple protocol (several statements in one transaction)
//!   and the extended one (`Parse`, `Bind`, `Describe`, `Execute`, `Sync`:
//!   prepared statements, `$1` parameters, text and binary formats).
//! * **Transactions**: `BEGIN` ... `COMMIT` / `ROLLBACK` hold one transaction
//!   across messages; outside them each statement commits on its own. A write
//!   conflict answers SQLSTATE `40001`: run the transaction again.
//! * The questions drivers ask when they connect (`SET`, `SHOW`, `version()`,
//!   `current_schema()`...) get the answers PostgreSQL would give.
//!
//! The SQL is Nuage DB's (see [`crate::sql`]); the system catalogs
//! (`pg_catalog`, `information_schema`) are not there.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use chrono::{NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use memoire_orm::nuage::wire;
use memoire_orm::{Error, Result, Value};

use super::State;
use super::apps::App;
use crate::embedded::{Embedded, EmbeddedTransaction, Savepoint};
use crate::sql::{Script, SqlResult};

/// Settings of the PostgreSQL protocol.
#[derive(Clone, Default)]
pub struct PgOptions {
    /// The server's certificate, for `sslmode=require`.
    pub tls: Option<Arc<rustls::ServerConfig>>,
}

impl std::fmt::Debug for PgOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgOptions").field("tls", &self.tls.is_some()).finish()
    }
}

/// A TLS configuration from a certificate chain and its private key, as PEM
/// files (those of Let's Encrypt, for instance).
pub fn tls_config(cert_pem: &std::path::Path, key_pem: &std::path::Path) -> Result<Arc<rustls::ServerConfig>> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let bad = |what: &str, e: &dyn std::fmt::Display| Error::Validation(format!("TLS: {what}: {e}"));
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert_pem)
        .map_err(|e| bad(&cert_pem.display().to_string(), &e))?
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| bad(&cert_pem.display().to_string(), &e))?;
    let key = PrivateKeyDer::from_pem_file(key_pem).map_err(|e| bad(&key_pem.display().to_string(), &e))?;
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| bad("protocol versions", &e))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| bad("certificate", &e))?;
    Ok(Arc::new(config))
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

enum Conn {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ServerConnection, TcpStream>>),
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(s) => s.read(buf),
            Conn::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(s) => s.write(buf),
            Conn::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Conn::Plain(s) => s.flush(),
            Conn::Tls(s) => s.flush(),
        }
    }
}

/// Messages to the client, sent at `flush`.
#[derive(Default)]
struct Out(Vec<u8>);

impl Out {
    fn msg(&mut self, kind: u8, body: impl FnOnce(&mut Vec<u8>)) {
        self.0.push(kind);
        let at = self.0.len();
        self.0.extend_from_slice(&[0; 4]);
        body(&mut self.0);
        let len = (self.0.len() - at) as u32;
        self.0[at..at + 4].copy_from_slice(&len.to_be_bytes());
    }
}

fn cstr(b: &mut Vec<u8>, s: &str) {
    b.extend_from_slice(s.as_bytes());
    b.push(0);
}

/// Reads a client message: its type and body.
fn read_msg(conn: &mut Conn) -> io::Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    conn.read_exact(&mut head)?;
    let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if !(4..=64 << 20).contains(&len) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad message length"));
    }
    let mut body = vec![0u8; len - 4];
    conn.read_exact(&mut body)?;
    Ok((head[0], body))
}

/// A cursor over a message body.
struct Body<'a>(&'a [u8]);

impl<'a> Body<'a> {
    fn bad() -> Error {
        Error::Validation("malformed message".into())
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.0.len() < n {
            return Err(Self::bad());
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn i16(&mut self) -> Result<i16> {
        Ok(i16::from_be_bytes(self.take(2)?.try_into().map_err(|_| Self::bad())?))
    }
    fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().map_err(|_| Self::bad())?))
    }
    fn str(&mut self) -> Result<String> {
        let end = self.0.iter().position(|b| *b == 0).ok_or_else(Self::bad)?;
        let s = String::from_utf8(self.0[..end].to_vec()).map_err(|_| Self::bad())?;
        self.0 = &self.0[end + 1..];
        Ok(s)
    }
}

const SSL_REQUEST: i32 = 80877103;
const GSS_REQUEST: i32 = 80877104;
const CANCEL_REQUEST: i32 = 80877102;
const PROTOCOL_3: i32 = 196608;

/// Serves one connection until the client leaves (blocking).
pub(crate) fn serve(state: Arc<State>, stream: TcpStream, options: PgOptions) {
    let _ = stream.set_nodelay(true);
    state.metrics.open_connections.fetch_add(1, Ordering::Relaxed);
    let _ = session(&state, stream, &options);
    state.metrics.open_connections.fetch_sub(1, Ordering::Relaxed);
}

fn session(state: &Arc<State>, stream: TcpStream, options: &PgOptions) -> io::Result<()> {
    let peer = stream.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default();
    let mut conn = Conn::Plain(stream);
    // The startup packet, after an optional TLS (or GSS) request.
    let params = loop {
        let mut len = [0u8; 4];
        conn.read_exact(&mut len)?;
        let len = u32::from_be_bytes(len) as usize;
        if !(8..=10_000).contains(&len) {
            return Ok(());
        }
        let mut body = vec![0u8; len - 4];
        conn.read_exact(&mut body)?;
        let code = i32::from_be_bytes([body[0], body[1], body[2], body[3]]);
        match code {
            SSL_REQUEST => match (&options.tls, conn) {
                (Some(tls), Conn::Plain(mut s)) => {
                    s.write_all(b"S")?;
                    let server = rustls::ServerConnection::new(tls.clone()).map_err(io::Error::other)?;
                    conn = Conn::Tls(Box::new(rustls::StreamOwned::new(server, s)));
                }
                (_, mut c) => {
                    c.write_all(b"N")?;
                    conn = c;
                }
            },
            GSS_REQUEST => conn.write_all(b"N")?,
            // Queries are short: there is nothing to cancel.
            CANCEL_REQUEST => return Ok(()),
            PROTOCOL_3 => {
                let mut b = Body(&body[4..]);
                let mut params = HashMap::new();
                while let Ok(k) = b.str() {
                    if k.is_empty() {
                        break;
                    }
                    params.insert(k, b.str().unwrap_or_default());
                }
                break params;
            }
            _ => {
                let mut out = Out::default();
                error(&mut out, "0A000", "this server speaks version 3 of the PostgreSQL protocol");
                return conn.write_all(&out.0);
            }
        }
    };

    // The password is a token of an application.
    let mut out = Out::default();
    out.msg(b'R', |b| b.extend_from_slice(&3i32.to_be_bytes()));
    conn.write_all(&out.0)?;
    let (kind, body) = read_msg(&mut conn)?;
    let token = if kind == b'p' { Body(&body).str().unwrap_or_default() } else { String::new() };
    let mut out = Out::default();
    let Some((app, read_only)) = state.apps.authenticate(&token, state.options.token.as_deref()) else {
        error(
            &mut out,
            "28P01",
            "password authentication failed: the password is a token of an application (the console's Tokens tab, or NUAGE_TOKEN)",
        );
        return conn.write_all(&out.0);
    };
    let database = params.get("database").map(String::as_str).unwrap_or_default();
    let branch = if database.is_empty() || database == app.name || database == super::MAIN || database == "postgres" {
        super::MAIN.to_owned()
    } else if app.engine.has_branch(database) {
        database.to_owned()
    } else {
        error(
            &mut out,
            "3D000",
            &format!("database \"{database}\" does not exist: use the application's name, or one of its branches"),
        );
        return conn.write_all(&out.0);
    };
    out.msg(b'R', |b| b.extend_from_slice(&0i32.to_be_bytes()));
    let app_name = params.get("application_name").cloned().unwrap_or_default();
    for (k, v) in server_params(&app_name) {
        out.msg(b'S', |b| {
            cstr(b, k);
            cstr(b, &v);
        });
    }
    // Say where the client is: psql shows it, drivers pass it on (a notice).
    let access = if read_only { "read only" } else { "read and write" };
    let hello =
        format!("Connected to Nuage DB {}: application `{}`, {access}, branch `{branch}`.", crate::VERSION, app.name);
    out.msg(b'N', |b| {
        for (f, v) in [(b'S', "NOTICE"), (b'V', "NOTICE"), (b'C', "00000"), (b'M', hello.as_str())] {
            b.push(f);
            cstr(b, v);
        }
        b.push(0);
    });
    let client = if app_name.is_empty() { "a PostgreSQL driver" } else { app_name.as_str() };
    app.connected("PostgreSQL", client, &peer, read_only);
    let pid = rand_i32();
    out.msg(b'K', |b| {
        b.extend_from_slice(&pid.to_be_bytes());
        b.extend_from_slice(&rand_i32().to_be_bytes());
    });
    let mut s = Session {
        state: state.clone(),
        app,
        read_only,
        branch,
        tx: None,
        failed: false,
        savepoints: Vec::new(),
        statements: HashMap::new(),
        portals: HashMap::new(),
        skip_to_sync: false,
    };
    s.ready(&mut out);
    conn.write_all(&out.0)?;
    loop {
        let (kind, body) = read_msg(&mut conn)?;
        let mut out = Out::default();
        let flush = match kind {
            b'X' => return Ok(()),
            b'S' => {
                s.skip_to_sync = false;
                if s.tx.is_none() {
                    // An implicit transaction ends at Sync: nothing to keep.
                    s.portals.clear();
                }
                s.ready(&mut out);
                true
            }
            b'H' => true,
            _ if s.skip_to_sync => false,
            b'Q' => {
                let sql = Body(&body).str().unwrap_or_default();
                s.simple(&mut out, &sql);
                s.ready(&mut out);
                true
            }
            _ => {
                if let Err(e) = s.extended(&mut out, kind, &body) {
                    s.fail(&mut out, &e);
                    s.skip_to_sync = true;
                }
                false
            }
        };
        if flush || out.0.len() > 1 << 16 {
            conn.write_all(&out.0)?;
            conn.flush()?;
        } else if !out.0.is_empty() {
            // Sent with the next Sync or Flush; most clients send them at once.
            conn.write_all(&out.0)?;
        }
    }
}

/// Parses SQL; a question about PostgreSQL's catalogs (`pg_type`,
/// `::regtype`...), which Nuage DB does not have, answers as PostgreSQL does
/// for an unknown object (SQLSTATE 42704): drivers then go on without it.
fn parse(sql: &str, norm: &str) -> Result<Script> {
    const CATALOG: [&str; 8] = [
        "pg_catalog",
        "pg_type",
        "pg_namespace",
        "pg_class",
        "pg_attribute",
        "information_schema",
        "regtype",
        "regclass",
    ];
    let script = Script::parse(sql);
    match script {
        Err(_) if CATALOG.iter().any(|c| norm.contains(c)) => Err(Error::NotFound),
        other => other,
    }
}

fn word_static(w: &str) -> &'static str {
    match w {
        "savepoint" => "savepoint",
        "release" => "release",
        _ => "rollback",
    }
}

fn rand_i32() -> i32 {
    let mut b = [0u8; 4];
    let _ = getrandom::fill(&mut b);
    i32::from_be_bytes(b) & 0x7fff_ffff
}

fn server_params(app_name: &str) -> Vec<(&'static str, String)> {
    vec![
        ("server_version", "16.0".into()),
        ("server_encoding", "UTF8".into()),
        ("client_encoding", "UTF8".into()),
        ("DateStyle", "ISO, MDY".into()),
        ("IntervalStyle", "postgres".into()),
        ("TimeZone", "UTC".into()),
        ("integer_datetimes", "on".into()),
        ("standard_conforming_strings", "on".into()),
        ("is_superuser", "off".into()),
        ("application_name", app_name.into()),
    ]
}

fn error(out: &mut Out, code: &str, message: &str) {
    out.msg(b'E', |b| {
        for (f, v) in [(b'S', "ERROR"), (b'V', "ERROR"), (b'C', code), (b'M', message)] {
            b.push(f);
            cstr(b, v);
        }
        b.push(0);
    });
}

/// The SQLSTATE of an error.
fn sqlstate(e: &Error) -> &'static str {
    let kind = wire::error_to_json(e).1["error"]["kind"].as_str().unwrap_or_default().to_owned();
    let message = e.to_string();
    if message.contains("current transaction is aborted") {
        return "25P02";
    }
    if message.contains("can only be used in transaction blocks") {
        return "25P01";
    }
    match kind.as_str() {
        "unique_violation" => "23505",
        "constraint" if message.contains("NULL") || message.contains("null") => "23502",
        "constraint" => "23514",
        "no_such_table" => "42P01",
        "table_exists" => "42P07",
        "conflict" | "stale_row" => "40001",
        "unsupported" => "0A000",
        "conversion" | "decode" => "22P02",
        "transaction_finished" => "25P01",
        "forbidden" => "42501",
        "not_found" => "42704",
        "validation" if message.contains("no column") => "42703",
        "validation" => "42601",
        _ => "XX000",
    }
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

struct Prepared {
    sql: String,
    script: Option<Arc<Script>>,
    /// Types of the parameters (0: the client did not say).
    param_types: Vec<i32>,
    /// Types of the parameters from the columns they meet (`id = $1`): told to
    /// clients that describe the statement, and how their binary values read.
    inferred: Vec<i32>,
    /// Types of the result columns from the tables' columns (0: unknown).
    typed_columns: Vec<i32>,
    /// Names of the result columns, read from the statement (empty: unknown).
    typed_names: Vec<String>,
    /// Types of the result columns, once described: execution sends these.
    columns: Option<Vec<(String, i32)>>,
}

struct Portal {
    stmt: Arc<Prepared>,
    params: Vec<Value>,
    formats: Vec<i16>,
    /// Results, once run (at Describe or Execute), and the rows already sent.
    results: Option<Vec<SqlResult>>,
    sent: usize,
}

struct Session {
    state: Arc<State>,
    app: Arc<App>,
    read_only: bool,
    branch: String,
    /// The transaction `BEGIN` opened.
    tx: Option<EmbeddedTransaction>,
    /// A statement of that transaction failed: only `ROLLBACK` is accepted.
    failed: bool,
    /// Its savepoints, in order.
    savepoints: Vec<(String, Savepoint)>,
    statements: HashMap<String, Arc<Prepared>>,
    portals: HashMap<String, Portal>,
    skip_to_sync: bool,
}

/// What the session does with a statement, before the SQL engine.
enum Plan {
    Run(Arc<Script>),
    /// `BEGIN`, `COMMIT`, `ROLLBACK` (with their variants).
    Control(&'static str),
    /// `SAVEPOINT x`, `RELEASE [SAVEPOINT] x`, `ROLLBACK TO [SAVEPOINT] x`.
    Savepoint(&'static str, String),
    /// A command answered at once (`SET`, `DISCARD ALL`...): its tag.
    Tag(String),
    /// A query answered at once (`SHOW`, `version()`...): column and value.
    Value(String, String),
    Empty,
}

impl Session {
    fn ready(&self, out: &mut Out) {
        let status = match (&self.tx, self.failed) {
            (None, _) => b'I',
            (Some(_), false) => b'T',
            (Some(_), true) => b'E',
        };
        out.msg(b'Z', |b| b.push(status));
    }

    fn fail(&mut self, out: &mut Out, e: &Error) {
        if self.tx.is_some() {
            self.failed = true;
        }
        error(out, sqlstate(e), &e.to_string());
    }

    /// Statements PostgreSQL drivers send that are not Nuage DB's SQL.
    fn plan(&self, sql: &str) -> Result<Plan> {
        let norm = sql.trim().trim_end_matches(';').trim().to_lowercase();
        let norm = norm.split_whitespace().collect::<Vec<_>>().join(" ");
        let word = norm.split(' ').next().unwrap_or_default();
        Ok(match word {
            "" => Plan::Empty,
            "begin" | "start" => Plan::Control("begin"),
            "commit" | "end" => Plan::Control("commit"),
            "rollback" | "abort" if !norm.contains(" to ") => Plan::Control("rollback"),
            "savepoint" | "release" | "rollback" => {
                let name = norm.rsplit(' ').next().unwrap_or_default().trim_matches('"').to_owned();
                Plan::Savepoint(if word == "rollback" { "rollback" } else { word_static(word) }, name)
            }
            "set" | "reset" => Plan::Tag(word.to_uppercase()),
            "discard" => Plan::Tag(norm.to_uppercase()),
            "deallocate" => Plan::Tag("DEALLOCATE".into()),
            "show" => {
                let name = norm.trim_start_matches("show ").replace(' ', "_");
                let value = match name.as_str() {
                    "server_version" => "16.0".to_owned(),
                    "server_version_num" => "160000".to_owned(),
                    "transaction_isolation" | "transaction_isolation_level" => "repeatable read".to_owned(),
                    "default_transaction_isolation" => "repeatable read".to_owned(),
                    "search_path" => "public".to_owned(),
                    "max_identifier_length" => "63".to_owned(),
                    "lc_collate" | "lc_ctype" => "C.UTF-8".to_owned(),
                    "transaction_read_only" | "default_transaction_read_only" => {
                        if self.read_only { "on" } else { "off" }.to_owned()
                    }
                    _ => server_params("")
                        .into_iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case(&name))
                        .map(|(_, v)| v)
                        .ok_or_else(|| Error::Validation(format!("unrecognized configuration parameter \"{name}\"")))?,
                };
                Plan::Value(name, value)
            }
            "select" => match norm.as_str() {
                "select version()" | "select pg_catalog.version()" => {
                    Plan::Value("version".into(), format!("PostgreSQL 16.0 (Nuage DB {})", crate::VERSION))
                }
                "select current_schema()" | "select current_schema" => {
                    Plan::Value("current_schema".into(), "public".into())
                }
                "select current_database()" => Plan::Value("current_database".into(), self.app.name.clone()),
                "select current_user" | "select session_user" | "select user" => {
                    Plan::Value("current_user".into(), self.app.name.clone())
                }
                "select pg_backend_pid()" => Plan::Value("pg_backend_pid".into(), "1".into()),
                _ => Plan::Run(Arc::new(parse(sql, &norm)?)),
            },
            _ => {
                let script = parse(sql, &norm)?;
                match script.control() {
                    Some(c) => Plan::Control(c),
                    None => Plan::Run(Arc::new(script)),
                }
            }
        })
    }

    /// Runs one statement: in the open transaction, or committed on its own.
    fn run(&mut self, script: &Script, params: &[Value]) -> Result<Vec<SqlResult>> {
        if self.failed {
            return Err(Error::Validation(
                "current transaction is aborted, commands ignored until end of transaction block".into(),
            ));
        }
        if self.read_only && script.writes() {
            return Err(Error::Validation(super::READ_ONLY.into()));
        }
        let started = Instant::now();
        self.app.touch();
        let result = match &self.tx {
            Some(tx) => crate::sql::run_in(tx, &self.state.schemas, script, params),
            None => {
                crate::sql::run_values(&self.app.engine, &self.branch, &self.state.schemas, &[(script, params)], true)
                    .map(|mut r| r.pop().unwrap_or_default())
            }
        };
        let failed = matches!(&result, Err(e) if wire::error_to_json(e).0 >= 500);
        self.app.metrics.record(started.elapsed(), failed);
        self.state.metrics.record(started.elapsed(), failed);
        result
    }

    fn savepoint(&mut self, what: &str, name: &str) -> Result<String> {
        let Some(tx) = &self.tx else {
            return Err(Error::Validation(format!("{} can only be used in transaction blocks", what.to_uppercase())));
        };
        let at = || self.savepoints.iter().rposition(|(n, _)| n == name);
        match what {
            "savepoint" => {
                if self.failed {
                    return Err(Error::Validation("current transaction is aborted".into()));
                }
                let sp = tx.savepoint()?;
                self.savepoints.push((name.to_owned(), sp));
                Ok("SAVEPOINT".into())
            }
            "release" => {
                let i = at().ok_or_else(|| Error::Validation(format!("savepoint \"{name}\" does not exist")))?;
                self.savepoints.truncate(i);
                Ok("RELEASE".into())
            }
            _ => {
                let i = at().ok_or_else(|| Error::Validation(format!("savepoint \"{name}\" does not exist")))?;
                tx.rollback_to(&self.savepoints[i].1)?;
                self.savepoints.truncate(i + 1);
                self.failed = false;
                Ok("ROLLBACK".into())
            }
        }
    }

    fn control(&mut self, what: &str) -> Result<String> {
        self.savepoints.clear();
        match what {
            "begin" => {
                if self.tx.is_none() {
                    self.tx =
                        Some(Embedded::from_engine(self.app.engine.clone()).on_branch(&self.branch)?.begin_now()?);
                    self.failed = false;
                }
                Ok("BEGIN".into())
            }
            "commit" => {
                let failed = std::mem::take(&mut self.failed);
                match self.tx.take() {
                    Some(tx) if failed => {
                        let _ = tx.rollback_now();
                        Ok("ROLLBACK".into())
                    }
                    Some(tx) => tx.commit_now().map(|()| "COMMIT".into()),
                    None => Ok("COMMIT".into()),
                }
            }
            _ => {
                self.failed = false;
                if let Some(tx) = self.tx.take() {
                    let _ = tx.rollback_now();
                }
                Ok("ROLLBACK".into())
            }
        }
    }

    // --- simple protocol --------------------------------------------------

    fn simple(&mut self, out: &mut Out, sql: &str) {
        if let Err(e) = self.simple_inner(out, sql) {
            self.fail(out, &e);
        }
    }

    fn simple_inner(&mut self, out: &mut Out, sql: &str) -> Result<()> {
        match self.plan(sql) {
            Ok(Plan::Empty) => {
                out.msg(b'I', |_| {});
                return Ok(());
            }
            Ok(Plan::Run(script)) if script.len() > 1 && self.tx.is_none() && !self.failed => {
                // Several statements: one implicit transaction, all or nothing,
                // unless they open or close transactions themselves.
                let parts = script.split();
                if parts.iter().all(|p| p.control().is_none()) {
                    if self.read_only && script.writes() {
                        return Err(Error::Validation(super::READ_ONLY.into()));
                    }
                    let results = crate::sql::run_values(
                        &self.app.engine,
                        &self.branch,
                        &self.state.schemas,
                        &[(&script, &[])],
                        true,
                    )?
                    .pop()
                    .unwrap_or_default();
                    for (i, r) in results.iter().enumerate() {
                        send_result(out, r, None, &[], &script.tag(i, r))?;
                    }
                    return Ok(());
                }
                for p in parts {
                    self.simple_one(out, Plan::Run(Arc::new(p)))?;
                }
                return Ok(());
            }
            Ok(Plan::Run(script)) if script.len() > 1 => {
                for p in script.split() {
                    let plan = match p.control() {
                        Some(c) => Plan::Control(c),
                        None => Plan::Run(Arc::new(p)),
                    };
                    self.simple_one(out, plan)?;
                }
                return Ok(());
            }
            Ok(plan) => self.simple_one(out, plan)?,
            Err(e) => return Err(e),
        }
        Ok(())
    }

    fn simple_one(&mut self, out: &mut Out, plan: Plan) -> Result<()> {
        match plan {
            Plan::Empty => out.msg(b'I', |_| {}),
            Plan::Control(c) => {
                let tag = self.control(c)?;
                complete(out, &tag);
            }
            Plan::Savepoint(what, name) => {
                let tag = self.savepoint(what, &name)?;
                complete(out, &tag);
            }
            Plan::Tag(tag) => {
                if self.failed {
                    return Err(Error::Validation("current transaction is aborted".into()));
                }
                complete(out, &tag);
            }
            Plan::Value(name, value) => {
                row_description(out, &[(name, 25)], &[]);
                data_row(out, &[Value::Text(value)], &[25], &[])?;
                complete(out, "SHOW");
            }
            Plan::Run(script) => {
                let results = self.run(&script, &[])?;
                for (i, r) in results.iter().enumerate() {
                    send_result(out, r, None, &[], &script.tag(i, r))?;
                }
            }
        }
        Ok(())
    }

    // --- extended protocol ------------------------------------------------

    fn extended(&mut self, out: &mut Out, kind: u8, body: &[u8]) -> Result<()> {
        let mut b = Body(body);
        match kind {
            b'P' => {
                let name = b.str()?;
                let sql = b.str()?;
                let n = b.i16()?.max(0) as usize;
                let mut param_types = Vec::with_capacity(n);
                for _ in 0..n {
                    param_types.push(b.i32()?);
                }
                let script = match self.plan(&sql)? {
                    Plan::Run(s) => {
                        if s.len() > 1 {
                            return Err(Error::Validation(
                                "cannot insert multiple commands into a prepared statement".into(),
                            ));
                        }
                        param_types.resize(param_types.len().max(s.params()), 0);
                        Some(s)
                    }
                    _ => None,
                };
                let (mut inferred, typed_columns, typed_names) = match &script {
                    Some(s) => self.typing(s),
                    None => Default::default(),
                };
                inferred.resize(param_types.len(), 0);
                for (i, t) in param_types.iter().enumerate() {
                    if *t != 0 {
                        inferred[i] = *t;
                    }
                }
                let stmt = Prepared { sql, script, param_types, inferred, typed_columns, typed_names, columns: None };
                self.statements.insert(name, Arc::new(stmt));
                out.msg(b'1', |_| {});
            }
            b'B' => {
                let portal = b.str()?;
                let name = b.str()?;
                let stmt = self
                    .statements
                    .get(&name)
                    .cloned()
                    .ok_or_else(|| Error::Validation(format!("prepared statement \"{name}\" does not exist")))?;
                let nf = b.i16()?.max(0) as usize;
                let mut pformats = Vec::with_capacity(nf);
                for _ in 0..nf {
                    pformats.push(b.i16()?);
                }
                let np = b.i16()?.max(0) as usize;
                let mut params = Vec::with_capacity(np);
                for i in 0..np {
                    let len = b.i32()?;
                    let format = match pformats.as_slice() {
                        [] => 0,
                        [f] => *f,
                        all => all.get(i).copied().unwrap_or(0),
                    };
                    // The client's type; else, for a binary value, the type it was
                    // told; a text value of unsaid type stays text, which the
                    // columns convert.
                    let oid = match stmt.param_types.get(i).copied().unwrap_or(0) {
                        0 if format == 1 => stmt.inferred.get(i).copied().unwrap_or(0),
                        t => t,
                    };
                    params.push(if len < 0 { Value::Null } else { decode_param(b.take(len as usize)?, oid, format)? });
                }
                let nr = b.i16()?.max(0) as usize;
                let mut formats = Vec::with_capacity(nr);
                for _ in 0..nr {
                    formats.push(b.i16()?);
                }
                self.portals.insert(portal, Portal { stmt, params, formats, results: None, sent: 0 });
                out.msg(b'2', |_| {});
            }
            b'D' => {
                let what = b.u8()?;
                let name = b.str()?;
                if what == b'S' {
                    let stmt =
                        self.statements.get(&name).cloned().ok_or_else(|| {
                            Error::Validation(format!("prepared statement \"{name}\" does not exist"))
                        })?;
                    let types: Vec<i32> = stmt.inferred.iter().map(|t| if *t == 0 { TEXT } else { *t }).collect();
                    out.msg(b't', |b| {
                        b.extend_from_slice(&(types.len() as i16).to_be_bytes());
                        for t in &types {
                            b.extend_from_slice(&t.to_be_bytes());
                        }
                    });
                    let columns = self.describe_statement(&stmt);
                    match &columns {
                        Some(cols) => row_description(out, cols, &[]),
                        None => out.msg(b'n', |_| {}),
                    }
                    let described = Prepared {
                        sql: stmt.sql.clone(),
                        script: stmt.script.clone(),
                        param_types: stmt.param_types.clone(),
                        inferred: types,
                        typed_columns: stmt.typed_columns.clone(),
                        typed_names: stmt.typed_names.clone(),
                        columns: Some(columns.unwrap_or_default()),
                    };
                    self.statements.insert(name, Arc::new(described));
                } else {
                    self.run_portal(&name)?;
                    let p = &self.portals[&name];
                    match portal_columns(p) {
                        Some(cols) => row_description(out, &cols, &p.formats),
                        None => out.msg(b'n', |_| {}),
                    }
                }
            }
            b'E' => {
                let name = b.str()?;
                let max = b.i32()?.max(0) as usize;
                self.run_portal(&name)?;
                let p = self.portals.get_mut(&name).ok_or_else(|| Error::Validation("no such portal".into()))?;
                let results = p.results.as_deref().unwrap_or_default();
                match (&p.stmt.script, results.first()) {
                    (Some(script), Some(r)) => {
                        let cols = portal_columns(p).unwrap_or_default();
                        let types: Vec<i32> = cols.iter().map(|c| c.1).collect();
                        let rows = &r.rows[p.sent..];
                        let n = if max > 0 { rows.len().min(max) } else { rows.len() };
                        for row in &rows[..n] {
                            data_row(out, row, &types, &p.formats)?;
                        }
                        p.sent += n;
                        if max > 0 && p.sent < r.rows.len() {
                            out.msg(b's', |_| {});
                        } else {
                            complete(out, &script.tag(0, r));
                        }
                    }
                    _ => {
                        let sql = p.stmt.sql.clone();
                        match self.plan(&sql)? {
                            Plan::Empty => out.msg(b'I', |_| {}),
                            Plan::Control(c) => {
                                let tag = self.control(c)?;
                                complete(out, &tag);
                            }
                            Plan::Savepoint(what, name) => {
                                let tag = self.savepoint(what, &name)?;
                                complete(out, &tag);
                            }
                            Plan::Tag(tag) => complete(out, &tag),
                            Plan::Value(_, value) => {
                                let formats = self.portals[&name].formats.clone();
                                data_row(out, &[Value::Text(value)], &[25], &formats)?;
                                complete(out, "SHOW");
                            }
                            Plan::Run(_) => complete(out, "SELECT 0"),
                        }
                    }
                }
            }
            b'C' => {
                let what = b.u8()?;
                let name = b.str()?;
                if what == b'S' {
                    self.statements.remove(&name);
                } else {
                    self.portals.remove(&name);
                }
                out.msg(b'3', |_| {});
            }
            other => {
                return Err(Error::Unsupported(format!("message `{}` of the PostgreSQL protocol", other as char)));
            }
        }
        Ok(())
    }

    /// Runs a portal's statement once (Describe and Execute share the result).
    fn run_portal(&mut self, name: &str) -> Result<()> {
        let p = self.portals.get(name).ok_or_else(|| Error::Validation(format!("portal \"{name}\" does not exist")))?;
        if p.results.is_some() {
            return Ok(());
        }
        let Some(script) = p.stmt.script.clone() else { return Ok(()) };
        let params = p.params.clone();
        let results = self.run(&script, &params)?;
        if let Some(p) = self.portals.get_mut(name) {
            p.results = Some(results);
        }
        Ok(())
    }

    /// The columns a prepared statement returns, from a run that changes
    /// nothing, with its parameters `NULL`.
    fn describe_statement(&self, stmt: &Prepared) -> Option<Vec<(String, i32)>> {
        if let Some(plan_value) = match self.plan(&stmt.sql) {
            Ok(Plan::Value(name, _)) => Some(name),
            _ => None,
        } {
            return Some(vec![(plan_value, 25)]);
        }
        let script = stmt.script.as_ref()?;
        if !script.returns_rows() {
            return None;
        }
        let nulls = vec![Value::Null; script.params()];
        let dry = match &self.tx {
            Some(tx) => crate::sql::run_in(tx, &self.state.schemas, script, &nulls),
            None => {
                crate::sql::run_values(&self.app.engine, &self.branch, &self.state.schemas, &[(script, &nulls)], false)
                    .map(|mut r| r.pop().unwrap_or_default())
            }
        };
        match dry.ok().and_then(|r| r.into_iter().next()) {
            Some(r) => Some(columns_of(&r, &stmt.typed_columns)),
            // The run failed (a NOT NULL column and NULL parameters...): the
            // columns as the statement names them.
            None if !stmt.typed_names.is_empty() && stmt.typed_names.len() == stmt.typed_columns.len() => Some(
                stmt.typed_names
                    .iter()
                    .zip(&stmt.typed_columns)
                    .map(|(n, t)| (n.clone(), if *t == 0 { TEXT } else { *t }))
                    .collect(),
            ),
            None => None,
        }
    }

    /// Types of a statement's parameters and result columns, from its tables.
    fn typing(&self, script: &Script) -> (Vec<i32>, Vec<i32>, Vec<String>) {
        let typing = |tx: &EmbeddedTransaction| crate::sql::typing(tx, &self.state.schemas, script);
        let t = match &self.tx {
            Some(tx) => typing(tx),
            None => match Embedded::from_engine(self.app.engine.clone())
                .on_branch(&self.branch)
                .and_then(|db| db.begin_now())
            {
                Ok(tx) => {
                    let t = typing(&tx);
                    let _ = tx.rollback_now();
                    t
                }
                Err(_) => return (vec![0; script.params()], Vec::new(), Vec::new()),
            },
        };
        let oid = |c: &Option<memoire_orm::ColumnType>| c.map_or(0, oid_of_type);
        (t.params.iter().map(oid).collect(), t.columns.iter().map(oid).collect(), t.names)
    }
}

/// A portal's columns: as described, else as found.
fn portal_columns(p: &Portal) -> Option<Vec<(String, i32)>> {
    let r = p.results.as_ref()?.first()?;
    if !r.returns_rows {
        return None;
    }
    match &p.stmt.columns {
        Some(cols) if cols.len() == r.columns.len() => Some(cols.clone()),
        _ => Some(columns_of(r, &p.stmt.typed_columns)),
    }
}

/// The type of each result column: of its table's column when known, else
/// of its values, else text.
fn columns_of(r: &SqlResult, typed: &[i32]) -> Vec<(String, i32)> {
    let typed = if typed.len() == r.columns.len() { typed } else { &[] };
    r.columns
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let from_values = || r.rows.iter().map(|row| oid_of(&row[i])).find(|o| *o != 0);
            let oid = typed.get(i).copied().filter(|t| *t != 0).or_else(from_values).unwrap_or(TEXT);
            (c.clone(), oid)
        })
        .collect()
}

fn complete(out: &mut Out, tag: &str) {
    out.msg(b'C', |b| cstr(b, tag));
}

fn send_result(out: &mut Out, r: &SqlResult, cols: Option<&[(String, i32)]>, formats: &[i16], tag: &str) -> Result<()> {
    if r.returns_rows {
        let cols = cols.map(<[_]>::to_vec).unwrap_or_else(|| columns_of(r, &[]));
        row_description(out, &cols, formats);
        let types: Vec<i32> = cols.iter().map(|c| c.1).collect();
        for row in &r.rows {
            data_row(out, row, &types, formats)?;
        }
    }
    complete(out, tag);
    Ok(())
}

fn format_of(formats: &[i16], i: usize) -> i16 {
    match formats {
        [] => 0,
        [f] => *f,
        all => all.get(i).copied().unwrap_or(0),
    }
}

fn row_description(out: &mut Out, cols: &[(String, i32)], formats: &[i16]) {
    out.msg(b'T', |b| {
        b.extend_from_slice(&(cols.len() as i16).to_be_bytes());
        for (i, (name, oid)) in cols.iter().enumerate() {
            cstr(b, name);
            b.extend_from_slice(&0i32.to_be_bytes());
            b.extend_from_slice(&0i16.to_be_bytes());
            b.extend_from_slice(&oid.to_be_bytes());
            b.extend_from_slice(&type_len(*oid).to_be_bytes());
            b.extend_from_slice(&(-1i32).to_be_bytes());
            b.extend_from_slice(&format_of(formats, i).to_be_bytes());
        }
    });
}

fn data_row(out: &mut Out, row: &[Value], types: &[i32], formats: &[i16]) -> Result<()> {
    let mut cells = Vec::with_capacity(row.len());
    for (i, v) in row.iter().enumerate() {
        let oid = types.get(i).copied().unwrap_or(25);
        cells.push(if v.is_null() { None } else { Some(encode(v, oid, format_of(formats, i))?) });
    }
    out.msg(b'D', |b| {
        b.extend_from_slice(&(cells.len() as i16).to_be_bytes());
        for c in &cells {
            match c {
                None => b.extend_from_slice(&(-1i32).to_be_bytes()),
                Some(bytes) => {
                    b.extend_from_slice(&(bytes.len() as i32).to_be_bytes());
                    b.extend_from_slice(bytes);
                }
            }
        }
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

const BOOL: i32 = 16;
const BYTEA: i32 = 17;
const INT8: i32 = 20;
const INT2: i32 = 21;
const INT4: i32 = 23;
const TEXT: i32 = 25;
const OID: i32 = 26;
const JSON: i32 = 114;
const FLOAT4: i32 = 700;
const FLOAT8: i32 = 701;
const UNKNOWN: i32 = 705;
const BPCHAR: i32 = 1042;
const VARCHAR: i32 = 1043;
const DATE: i32 = 1082;
const TIME: i32 = 1083;
const TIMESTAMP: i32 = 1114;
const TIMESTAMPTZ: i32 = 1184;
const NUMERIC: i32 = 1700;
const UUID: i32 = 2950;
const JSONB: i32 = 3802;
const NAME: i32 = 19;

/// The PostgreSQL type of a value (0 for `NULL`).
fn oid_of(v: &Value) -> i32 {
    match v {
        Value::Null => 0,
        Value::Bool(_) => BOOL,
        Value::I32(_) | Value::I64(_) => INT8,
        Value::F64(_) => FLOAT8,
        Value::Text(_) => TEXT,
        Value::Bytes(_) => BYTEA,
        Value::Uuid(_) => UUID,
        Value::Timestamp(_) => TIMESTAMP,
        Value::TimestampTz(_) => TIMESTAMPTZ,
        Value::Date(_) => DATE,
        Value::Time(_) => TIME,
        Value::Json(_) => JSONB,
        _ => TEXT,
    }
}

/// The PostgreSQL type of a column type.
fn oid_of_type(t: memoire_orm::ColumnType) -> i32 {
    use memoire_orm::ColumnType as C;
    match t {
        C::Bool => BOOL,
        C::SmallInt | C::Int | C::BigInt => INT8,
        C::Real | C::Double => FLOAT8,
        C::Text => TEXT,
        C::Bytes => BYTEA,
        C::Uuid => UUID,
        C::Timestamp => TIMESTAMP,
        C::TimestampTz => TIMESTAMPTZ,
        C::Date => DATE,
        C::Time => TIME,
        C::Json => JSONB,
        _ => TEXT,
    }
}

fn type_len(oid: i32) -> i16 {
    match oid {
        BOOL => 1,
        INT2 => 2,
        INT4 | FLOAT4 | DATE | OID => 4,
        INT8 | FLOAT8 | TIME | TIMESTAMP | TIMESTAMPTZ => 8,
        UUID => 16,
        _ => -1,
    }
}

fn epoch() -> NaiveDateTime {
    NaiveDate::from_ymd_opt(2000, 1, 1).unwrap_or_default().and_hms_opt(0, 0, 0).unwrap_or_default()
}

/// PostgreSQL's text form of a value.
fn text_of(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => if *b { "t" } else { "f" }.into(),
        Value::I32(n) => n.to_string(),
        Value::I64(n) => n.to_string(),
        Value::F64(f) if f.is_nan() => "NaN".into(),
        Value::F64(f) if f.is_infinite() => if *f > 0.0 { "Infinity" } else { "-Infinity" }.into(),
        Value::F64(f) => f.to_string(),
        Value::Text(s) => s.clone(),
        Value::Bytes(b) => format!("\\x{}", b.iter().map(|x| format!("{x:02x}")).collect::<String>()),
        Value::Uuid(u) => u.hyphenated().to_string(),
        Value::Timestamp(t) => t.format("%Y-%m-%d %H:%M:%S%.f").to_string(),
        Value::TimestampTz(t) => t.format("%Y-%m-%d %H:%M:%S%.f+00").to_string(),
        Value::Date(d) => d.format("%Y-%m-%d").to_string(),
        Value::Time(t) => t.format("%H:%M:%S%.f").to_string(),
        Value::Json(j) => j.to_string(),
        other => format!("{other:?}"),
    }
}

/// A value as a column of type `oid` sends it, in text (0) or binary (1).
fn encode(v: &Value, oid: i32, format: i16) -> Result<Vec<u8>> {
    if format == 0 {
        return Ok(text_of(v).into_bytes());
    }
    let int = || match v {
        Value::I32(n) => Some(*n as i64),
        Value::I64(n) => Some(*n),
        Value::Bool(b) => Some(*b as i64),
        Value::F64(f) => Some(*f as i64),
        Value::Text(s) => s.parse().ok(),
        _ => None,
    };
    let float = || match v {
        Value::F64(f) => Some(*f),
        Value::I32(n) => Some(*n as f64),
        Value::I64(n) => Some(*n as f64),
        Value::Text(s) => s.parse().ok(),
        _ => None,
    };
    let mismatch = || Error::Conversion(format!("cannot send {} as PostgreSQL type {oid}", v.type_name()));
    Ok(match oid {
        BOOL => vec![matches!(v, Value::Bool(true)) as u8],
        INT2 => (int().ok_or_else(mismatch)? as i16).to_be_bytes().to_vec(),
        INT4 => (int().ok_or_else(mismatch)? as i32).to_be_bytes().to_vec(),
        INT8 | OID => int().ok_or_else(mismatch)?.to_be_bytes().to_vec(),
        FLOAT4 => (float().ok_or_else(mismatch)? as f32).to_be_bytes().to_vec(),
        FLOAT8 => float().ok_or_else(mismatch)?.to_be_bytes().to_vec(),
        BYTEA => match v {
            Value::Bytes(b) => b.clone(),
            other => text_of(other).into_bytes(),
        },
        UUID => match v {
            Value::Uuid(u) => u.as_bytes().to_vec(),
            Value::Text(s) => uuid::Uuid::parse_str(s).map_err(|_| mismatch())?.as_bytes().to_vec(),
            _ => return Err(mismatch()),
        },
        TIMESTAMP | TIMESTAMPTZ => {
            let t = match v {
                Value::Timestamp(t) => *t,
                Value::TimestampTz(t) => t.naive_utc(),
                Value::Date(d) => d.and_hms_opt(0, 0, 0).unwrap_or_default(),
                _ => return Err(mismatch()),
            };
            let micros = (t - epoch()).num_microseconds().ok_or_else(mismatch)?;
            micros.to_be_bytes().to_vec()
        }
        DATE => {
            let d = match v {
                Value::Date(d) => *d,
                Value::Timestamp(t) => t.date(),
                Value::TimestampTz(t) => t.date_naive(),
                _ => return Err(mismatch()),
            };
            ((d - epoch().date()).num_days() as i32).to_be_bytes().to_vec()
        }
        TIME => match v {
            Value::Time(t) => {
                let micros = t.num_seconds_from_midnight() as i64 * 1_000_000 + (t.nanosecond() / 1000) as i64;
                micros.to_be_bytes().to_vec()
            }
            _ => return Err(mismatch()),
        },
        JSONB => {
            let mut b = vec![1u8];
            b.extend_from_slice(text_of(v).as_bytes());
            b
        }
        _ => text_of(v).into_bytes(),
    })
}

/// A parameter the client sent, of type `oid` (0: unsaid), in text (0) or binary (1).
fn decode_param(raw: &[u8], oid: i32, format: i16) -> Result<Value> {
    let bad = || Error::Conversion(format!("invalid input for a parameter of PostgreSQL type {oid}"));
    let text = || String::from_utf8(raw.to_vec()).map_err(|_| bad());
    if format == 0 {
        let s = text()?;
        return Ok(match oid {
            BOOL => Value::Bool(matches!(s.to_ascii_lowercase().as_str(), "t" | "true" | "1" | "on" | "yes" | "y")),
            INT2 | INT4 | INT8 | OID => Value::I64(s.trim().parse().map_err(|_| bad())?),
            FLOAT4 | FLOAT8 => Value::F64(s.trim().parse().map_err(|_| bad())?),
            BYTEA => match s.strip_prefix("\\x") {
                Some(hex) => Value::Bytes(
                    (0..hex.len() / 2)
                        .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).map_err(|_| bad()))
                        .collect::<Result<_>>()?,
                ),
                None => Value::Bytes(s.into_bytes()),
            },
            // Text, or a type the column converts from text (dates, UUIDs,
            // numbers of unsaid type...).
            _ => Value::Text(s),
        });
    }
    let n = |len: usize| -> Result<&[u8]> { if raw.len() == len { Ok(raw) } else { Err(bad()) } };
    Ok(match oid {
        BOOL => Value::Bool(n(1)?[0] != 0),
        INT2 => Value::I64(i16::from_be_bytes(n(2)?.try_into().map_err(|_| bad())?) as i64),
        INT4 | OID => Value::I64(i32::from_be_bytes(n(4)?.try_into().map_err(|_| bad())?) as i64),
        INT8 => Value::I64(i64::from_be_bytes(n(8)?.try_into().map_err(|_| bad())?)),
        FLOAT4 => Value::F64(f32::from_be_bytes(n(4)?.try_into().map_err(|_| bad())?) as f64),
        FLOAT8 => Value::F64(f64::from_be_bytes(n(8)?.try_into().map_err(|_| bad())?)),
        BYTEA => Value::Bytes(raw.to_vec()),
        UUID => Value::Uuid(uuid::Uuid::from_slice(n(16)?).map_err(|_| bad())?),
        TIMESTAMP | TIMESTAMPTZ => {
            let micros = i64::from_be_bytes(n(8)?.try_into().map_err(|_| bad())?);
            let t = epoch() + chrono::Duration::microseconds(micros);
            if oid == TIMESTAMP { Value::Timestamp(t) } else { Value::TimestampTz(t.and_utc()) }
        }
        DATE => {
            let days = i32::from_be_bytes(n(4)?.try_into().map_err(|_| bad())?);
            Value::Date(epoch().date() + chrono::Duration::days(days as i64))
        }
        TIME => {
            let micros = i64::from_be_bytes(n(8)?.try_into().map_err(|_| bad())?);
            let t = NaiveTime::from_num_seconds_from_midnight_opt(
                (micros / 1_000_000) as u32,
                ((micros % 1_000_000) * 1000) as u32,
            )
            .ok_or_else(bad)?;
            Value::Time(t)
        }
        JSONB => Value::Text(String::from_utf8(raw.get(1..).unwrap_or_default().to_vec()).map_err(|_| bad())?),
        0 | TEXT | VARCHAR | BPCHAR | NAME | UNKNOWN | JSON => Value::Text(text()?),
        NUMERIC => return Err(Error::Unsupported("binary NUMERIC parameters: send them as text".into())),
        other => {
            return Err(Error::Unsupported(format!("binary parameters of PostgreSQL type {other}: send them as text")));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_round_trip() {
        let t = NaiveDate::from_ymd_opt(2026, 11, 13).unwrap().and_hms_micro_opt(10, 30, 0, 250).unwrap();
        for (v, oid) in [
            (Value::I64(-42), INT8),
            (Value::F64(1.5), FLOAT8),
            (Value::Bool(true), BOOL),
            (Value::Text("héllo".into()), TEXT),
            (Value::Bytes(vec![0, 255, 7]), BYTEA),
            (Value::Timestamp(t), TIMESTAMP),
            (Value::Date(t.date()), DATE),
            (Value::Uuid(uuid::Uuid::from_u128(7)), UUID),
        ] {
            let bin = encode(&v, oid, 1).unwrap();
            assert_eq!(decode_param(&bin, oid, 1).unwrap(), v, "binary {oid}");
        }
        assert_eq!(encode(&Value::I64(5), INT4, 1).unwrap(), 5i32.to_be_bytes());
        assert_eq!(text_of(&Value::Bytes(vec![1, 171])), "\\x01ab");
        assert_eq!(text_of(&Value::Timestamp(t)), "2026-11-13 10:30:00.000250");
        assert_eq!(decode_param(b"\\x01ab", BYTEA, 0).unwrap(), Value::Bytes(vec![1, 171]));
        assert_eq!(decode_param(b"12", INT4, 0).unwrap(), Value::I64(12));
        assert_eq!(decode_param(b"12", 0, 0).unwrap(), Value::Text("12".into()));
    }
}
