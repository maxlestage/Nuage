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
//! * **Limits**: a client has 15 seconds to log in, with small messages; then
//!   a message is as long as a request body may be
//!   ([`ServerOptions::max_body_bytes`](super::ServerOptions::max_body_bytes))
//!   and comes at a pace, an SQL text is 1 MiB at most, and a transaction
//!   left idle is rolled back after about 5 minutes
//!   ([`PgOptions::idle_in_transaction`]), however busy after an hour
//!   ([`PgOptions::transaction_timeout`]). A client that takes nothing of
//!   its answers for a minute is disconnected. What the sessions hold
//!   together (prepared statements, portals, messages being read) is bounded
//!   ([`PgOptions::memory`]), and read-only tokens have half of the
//!   connections at most. A logged-in connection may stay idle as long as it
//!   likes.
//!
//! The SQL is Nuage DB's (see [`crate::sql`]); the system catalogs
//! (`pg_catalog`, `information_schema`) are not there.

use std::collections::{HashMap, VecDeque};
use std::io::{self, BufReader, Read, Write};
use std::net::TcpStream;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use chrono::{NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use memoire_orm::nuage::wire;
use memoire_orm::{Error, Result, Value};

use super::State;
use super::apps::App;
use super::net::{PortMemory, Share};
use crate::embedded::{Embedded, EmbeddedTransaction, Savepoint};
use crate::sql::{Script, SqlResult};

/// Settings of the PostgreSQL protocol.
#[derive(Clone, Default)]
pub struct PgOptions {
    /// The server's certificate, for `sslmode=require`.
    pub tls: Option<Arc<rustls::ServerConfig>>,
    /// How long a transaction (`BEGIN`) may wait for its client's next
    /// message: past it, it is rolled back and that message is told so
    /// (SQLSTATE `25P03`), as PostgreSQL's `idle_in_transaction_session_timeout`
    /// does. Default (`None`): 5 minutes (a little more at times: the
    /// system's timers have some slack).
    pub idle_in_transaction: Option<Duration>,
    /// How long a transaction may stay open, however busy: an open one keeps
    /// every page it sees, and the database's file grows meanwhile. Past it,
    /// it is rolled back and the client's next message is told so (SQLSTATE
    /// `25P04`), as PostgreSQL's `transaction_timeout` does. Default
    /// (`None`): an hour.
    pub transaction_timeout: Option<Duration>,
    /// What the sessions of the port may hold together beyond a little each,
    /// in bytes: the messages being read past their first 64 KiB, the
    /// prepared statements, the portals and the rows they keep between
    /// messages. Past it a message or a statement is refused (SQLSTATE
    /// `53200`) instead of the server running out of memory; one session
    /// holds half of it at most. Default (`None`): an eighth of the server's
    /// memory, and room for four of the largest messages.
    pub memory: Option<usize>,
    /// Set by the listener for a connection past the limit: it is told
    /// `too_many_connections` and closed.
    #[doc(hidden)]
    pub refuse: bool,
}

impl std::fmt::Debug for PgOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgOptions")
            .field("tls", &self.tls.is_some())
            .field("idle_in_transaction", &self.idle_in_transaction)
            .field("transaction_timeout", &self.transaction_timeout)
            .field("memory", &self.memory)
            .finish()
    }
}

/// What the sessions of one PostgreSQL port share: its memory, and the
/// count of sessions logged in with a read-only token.
pub(crate) struct Port {
    memory: Arc<PortMemory>,
    read_only: AtomicUsize,
}

impl Port {
    pub(crate) fn new(options: &PgOptions, max_body: usize) -> Arc<Port> {
        Arc::new(Port { memory: PortMemory::new(options.memory, max_body), read_only: AtomicUsize::new(0) })
    }
}

/// A read-only session's place among those read-only tokens may have.
struct ReadOnlyPlace<'p>(&'p Port);

impl ReadOnlyPlace<'_> {
    /// A place among `most`, if one is left.
    fn take(port: &Port, most: usize) -> Option<ReadOnlyPlace<'_>> {
        super::net::add_within(&port.read_only, 1, most).then_some(ReadOnlyPlace(port))
    }
}

impl Drop for ReadOnlyPlace<'_> {
    fn drop(&mut self) {
        self.0.read_only.fetch_sub(1, Ordering::Relaxed);
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

/// Time a client has to log in, from its connection to its password checked
/// (TLS included). Once logged in, it may stay idle as long as it likes: the
/// connections of a pool wait for hours.
const LOGIN_TIME: Duration = Duration::from_secs(15);
/// Time a client past the connection limit has to hear why.
const REFUSE_TIME: Duration = Duration::from_secs(5);
/// The longest message before login (PostgreSQL's own limit for a startup
/// packet): a client nobody knows holds no more than that.
const LOGIN_MESSAGE: usize = 10_000;
/// A transaction left this long without a word from its client is rolled
/// back ([`PgOptions::idle_in_transaction`]): an open one keeps every page it
/// sees, and the database's file grows meanwhile.
const IDLE_IN_TRANSACTION: Duration = Duration::from_secs(300);
/// A transaction is rolled back this long after it began, however busy
/// ([`PgOptions::transaction_timeout`]): a client that sends a message now
/// and then would otherwise keep one open for days.
const TRANSACTION_TIMEOUT: Duration = Duration::from_secs(3600);
/// A client that takes nothing of its answers this long, or sends nothing
/// more of a message it started, is gone: its session ends (its transaction
/// rolled back).
const STALLED: Duration = Duration::from_secs(60);
/// The slowest pace a long message may come at, in bytes a second, besides
/// [`STALLED`]: a 16 MiB message has some five minutes.
const MESSAGE_PACE: usize = 64 << 10;
/// Bytes of answers held before they are sent: a long result goes out as it
/// is written, not once all of it is.
const SEND_AT: usize = 1 << 16;
/// What is read from the socket at once, after login.
const READ_AHEAD: usize = 16 << 10;
/// A message body up to this is read at once; past it, it grows as its bytes
/// arrive, each part taken from the port's memory ([`PgOptions::memory`]).
const LARGE_MESSAGE: usize = 64 << 10;
/// The longest SQL text of a statement (a query, a prepared statement), as
/// on `/v1/sql`: a longer one is a dump, to send in parts. Its syntax tree
/// takes some fifty times its size.
const MAX_SQL_TEXT: usize = 1 << 20;
/// The stack of a connection's thread: statements may nest deep.
pub(super) const THREAD_STACK: usize = 8 << 20;
/// Prepared statements, and portals, a session holds at once at most: far
/// more than drivers keep (a few hundred). What they hold in memory is
/// bounded too ([`Session::account`]).
const SESSION_STATEMENTS: usize = 10_000;
const SESSION_PORTALS: usize = 10_000;
/// What a session holds (prepared statements, portals, a statement being
/// read) without taking it from the port's memory: ordinary sessions never
/// need to, and keep working when large ones have used it all.
const SESSION_FREE: usize = 256 << 10;

/// The client's socket. While a deadline is set (to log in), a read waits
/// only for what is left of it; within a message, its next bytes are
/// expected within [`STALLED`] and before `within`; between messages (in a
/// transaction), a read waits `idle` at most and ends by `idle_until`. A read
/// that waited too long fails with [`timed_out`], and nothing was read.
struct Socket {
    stream: TcpStream,
    deadline: Option<Instant>,
    within: Option<Instant>,
    idle: Option<Duration>,
    idle_until: Option<Instant>,
    /// The read timeout set on the socket now.
    armed: Option<Duration>,
    /// An answer being sent must be taken by then: a client that takes a few
    /// bytes now and then keeps each write going, not the whole of it.
    sending: Option<Instant>,
}

impl Socket {
    fn new(stream: TcpStream) -> Socket {
        Socket { stream, deadline: None, within: None, idle: None, idle_until: None, armed: None, sending: None }
    }

    /// How long the next read may wait (`None`: as long as it takes).
    fn wait(&self) -> io::Result<Option<Duration>> {
        let left = |at: Instant| {
            let left = at.saturating_duration_since(Instant::now());
            if left.is_zero() { Err(io::Error::from(io::ErrorKind::TimedOut)) } else { Ok(left) }
        };
        Ok(match (self.deadline, self.within, self.idle_until) {
            (Some(at), _, _) => Some(left(at)?),
            (None, Some(at), _) => Some(left(at)?.min(STALLED)),
            (None, None, Some(at)) => {
                let left = left(at)?;
                Some(self.idle.map_or(left, |idle| idle.min(left)))
            }
            (None, None, None) => self.idle,
        })
    }
}

impl Read for Socket {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let wait = self.wait()?;
        // Set again only when it changes: once per transaction, and per
        // message that does not come in one read, not per read.
        if wait != self.armed {
            self.stream.set_read_timeout(wait)?;
            self.armed = wait;
        }
        self.stream.read(buf)
    }
}

impl Write for Socket {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.sending.is_some_and(|by| Instant::now() >= by) {
            return Err(io::ErrorKind::TimedOut.into());
        }
        self.stream.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

/// A read that ended for its timeout (a would-block error on Unix, a
/// timed-out one on Windows).
fn timed_out(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
}

enum Conn {
    Plain(Socket),
    Tls(Box<rustls::StreamOwned<rustls::ServerConnection, Socket>>),
}

/// What reads from the client's socket, and sets how long its reads wait.
trait Wire: Read {
    fn socket(&mut self) -> &mut Socket;
}

impl Wire for Socket {
    fn socket(&mut self) -> &mut Socket {
        self
    }
}

impl Wire for Conn {
    fn socket(&mut self) -> &mut Socket {
        match self {
            Conn::Plain(s) => s,
            Conn::Tls(s) => &mut s.sock,
        }
    }
}

impl Wire for BufReader<Conn> {
    fn socket(&mut self) -> &mut Socket {
        self.get_mut().socket()
    }
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

/// Messages to the client. Those of a long answer are sent as they pile up
/// (when it has a connection to send them to), the rest by [`Out::end`].
#[derive(Default)]
struct Out<'c> {
    buf: Vec<u8>,
    to: Option<&'c mut Conn>,
    /// Something was sent already.
    sent: bool,
    /// A write failed: the client is gone, nothing more is written.
    failed: Option<io::Error>,
}

impl<'c> Out<'c> {
    fn to(conn: &'c mut Conn) -> Out<'c> {
        Out { to: Some(conn), ..Out::default() }
    }

    fn msg(&mut self, kind: u8, body: impl FnOnce(&mut Vec<u8>)) {
        if self.failed.is_some() {
            return;
        }
        self.buf.push(kind);
        let at = self.buf.len();
        self.buf.extend_from_slice(&[0; 4]);
        body(&mut self.buf);
        let len = (self.buf.len() - at) as u32;
        self.buf[at..at + 4].copy_from_slice(&len.to_be_bytes());
        if self.buf.len() >= SEND_AT
            && let Some(conn) = self.to.as_deref_mut()
        {
            if let Err(e) = send(conn, &self.buf, false) {
                self.failed = Some(e);
            }
            self.buf.clear();
            self.sent = true;
        }
    }

    /// Sends what is left; `flush`: now, not with the next answer.
    fn end(mut self, flush: bool) -> io::Result<()> {
        if let Some(e) = self.failed.take() {
            return Err(e);
        }
        let Some(conn) = self.to.take() else { return Ok(()) };
        send(conn, &self.buf, flush || self.sent)
    }
}

/// Writes `buf` (and flushes it): the client has [`STALLED`] to take it,
/// however little it takes at a time.
fn send(conn: &mut Conn, buf: &[u8], flush: bool) -> io::Result<()> {
    conn.socket().sending = Some(Instant::now() + STALLED);
    let mut sent = if buf.is_empty() { Ok(()) } else { conn.write_all(buf) };
    if flush && sent.is_ok() {
        sent = conn.flush();
    }
    conn.socket().sending = None;
    sent
}

fn cstr(b: &mut Vec<u8>, s: &str) {
    b.extend_from_slice(s.as_bytes());
    b.push(0);
}

/// What the client sent next.
enum Incoming {
    Message(u8, Vec<u8>),
    /// Nothing came in the time a read may wait.
    Quiet,
    /// A message read through, not kept: its type, and why.
    Skipped(u8, Skip),
}

/// Why a message was not kept.
enum Skip {
    /// It is longer than a message may be: its length.
    TooLong(usize),
    /// The port's memory is all held by other messages and sessions.
    NoMemory,
}

/// Reads a client message, of at most `max` bytes: its type and body. The
/// body grows as its bytes arrive, each part past [`LARGE_MESSAGE`] held in
/// `share`, so a message announced and never sent holds no memory. Once its
/// first byte is in, the rest must follow at a pace. A longer message is
/// read through and let go when `skip_long`, else an error (before login: a
/// client nobody knows is not read for long).
fn read_msg(conn: &mut impl Wire, max: usize, share: &mut Share, skip_long: bool) -> io::Result<Incoming> {
    let mut head = [0u8; 5];
    // The first byte apart: a wait that ends before it leaves the stream as it was.
    loop {
        match conn.read(&mut head[..1]) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(_) => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if timed_out(&e) => return Ok(Incoming::Quiet),
            Err(e) => return Err(e),
        }
    }
    let started = Instant::now();
    conn.socket().within = Some(started + STALLED);
    let read = read_rest(conn, head, max, share, started, skip_long);
    conn.socket().within = None;
    read
}

fn read_rest(
    conn: &mut impl Wire,
    mut head: [u8; 5],
    max: usize,
    share: &mut Share,
    started: Instant,
    skip_long: bool,
) -> io::Result<Incoming> {
    conn.read_exact(&mut head[1..])?;
    let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if len < 4 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad message length"));
    }
    // A long message may take its time, at a pace.
    conn.socket().within = Some(started + STALLED + Duration::from_secs((len / MESSAGE_PACE) as u64));
    if len > max {
        if !skip_long {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "message too long"));
        }
        skip(conn, len - 4)?;
        return Ok(Incoming::Skipped(head[0], Skip::TooLong(len)));
    }
    Ok(match read_body(conn, len - 4, share)? {
        Some(body) => Incoming::Message(head[0], body),
        None => Incoming::Skipped(head[0], Skip::NoMemory),
    })
}

/// Reads `len` bytes and lets them go.
fn skip(conn: &mut impl Read, len: usize) -> io::Result<()> {
    let skipped = io::copy(&mut conn.by_ref().take(len as u64), &mut io::sink())?;
    if skipped < len as u64 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(())
}

/// `len` bytes of a message's body (`None`: read through and let go, for
/// want of memory). Past [`LARGE_MESSAGE`], what it takes is held in `share`
/// as it grows, doubling at most up to its length.
fn read_body(conn: &mut impl Read, len: usize, share: &mut Share) -> io::Result<Option<Vec<u8>>> {
    if len <= LARGE_MESSAGE {
        let mut body = vec![0u8; len];
        conn.read_exact(&mut body)?;
        return Ok(Some(body));
    }
    let mut body: Vec<u8> = Vec::new();
    let mut room = 0;
    while body.len() < len {
        if body.len() == room {
            room = (room * 2).clamp(LARGE_MESSAGE, len);
            if !share.hold_waiting(room) {
                let rest = len - body.len();
                drop(body);
                share.hold(0);
                skip(conn, rest)?;
                return Ok(None);
            }
            body.reserve_exact(room - body.len());
        }
        let got = conn.by_ref().take((room - body.len()) as u64).read_to_end(&mut body)?;
        if got == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
    }
    Ok(Some(body))
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
    /// A count (of parameters, of formats): PostgreSQL reads it unsigned.
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().map_err(|_| Self::bad())?))
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

/// Serves one connection until the client leaves (blocking). The listener
/// counted it in `pg_connections`; it is counted out here.
pub(crate) fn serve(state: Arc<State>, stream: TcpStream, options: PgOptions, port: Arc<Port>) {
    /// Counts a connection for as long as it lives, a panic included.
    struct Open<'a>(&'a State);
    impl Drop for Open<'_> {
        fn drop(&mut self) {
            self.0.metrics.open_connections.fetch_sub(1, Ordering::Relaxed);
            self.0.pg_connections.fetch_sub(1, Ordering::Relaxed);
        }
    }
    state.metrics.open_connections.fetch_add(1, Ordering::Relaxed);
    let _open = Open(&state);
    let _ = stream.set_nodelay(true);
    super::net::keep_alive(&stream);
    // A bug met by one session ends that session alone (its client is told
    // why when the session got that far), not the process's thread count.
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| session(&state, stream, &options, &port)));
}

/// The text of a panic.
fn panic_text(panic: &(dyn std::any::Any + Send)) -> &str {
    panic.downcast_ref::<&str>().copied().or_else(|| panic.downcast_ref::<String>().map(String::as_str)).unwrap_or("")
}

fn session(state: &Arc<State>, stream: TcpStream, options: &PgOptions, port: &Port) -> io::Result<()> {
    let peer = stream.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default();
    // Until the password is checked, each read waits only for what is left
    // of the time to log in, and the few answers the client is sent must go
    // out in that time too.
    let mut socket = Socket::new(stream);
    let login = if options.refuse { REFUSE_TIME } else { LOGIN_TIME };
    socket.deadline = Some(Instant::now() + login);
    let _ = socket.stream.set_write_timeout(Some(login));
    let mut conn = Conn::Plain(socket);
    // The startup packet, after an optional TLS (or GSS) request.
    let params = loop {
        let mut len = [0u8; 4];
        conn.read_exact(&mut len)?;
        let len = u32::from_be_bytes(len) as usize;
        if !(8..=LOGIN_MESSAGE).contains(&len) {
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
                return conn.write_all(&out.buf);
            }
        }
    };

    if options.refuse {
        let mut out = Out::default();
        error(
            &mut out,
            "53300",
            &format!(
                "too many connections: this server serves {} PostgreSQL connections at once (NUAGE_PG_MAX_CONNECTIONS); use a connection pool, or close idle ones",
                state.pg_max_connections()
            ),
        );
        return conn.write_all(&out.buf);
    }
    // The password is a token of an application.
    let mut out = Out::default();
    out.msg(b'R', |b| b.extend_from_slice(&3i32.to_be_bytes()));
    conn.write_all(&out.buf)?;
    let mut share = Share::new(&port.memory);
    let Incoming::Message(kind, body) = read_msg(&mut conn, LOGIN_MESSAGE, &mut share, false)? else { return Ok(()) };
    let token = if kind == b'p' { Body(&body).str().unwrap_or_default() } else { String::new() };
    let mut out = Out::default();
    let Some((app, read_only)) = state.apps.authenticate(&token, state.options.token.as_deref()) else {
        error(
            &mut out,
            "28P01",
            "password authentication failed: the password is a token of an application (the console's Tokens tab, or NUAGE_TOKEN)",
        );
        return conn.write_all(&out.buf);
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
        return conn.write_all(&out.buf);
    };
    // Read-only tokens may be public (in web pages): they get half of the
    // connections at most, so that their holders, however many, never keep
    // the application's own clients out.
    let max_connections = state.pg_max_connections();
    let read_only_most = max_connections.div_ceil(2).max(1);
    let _read_only_place = match read_only {
        false => None,
        true => match ReadOnlyPlace::take(port, read_only_most) {
            Some(place) => Some(place),
            None => {
                error(
                    &mut out,
                    "53300",
                    &format!(
                        "too many read-only connections: this server serves {read_only_most} of them at once, half of its {max_connections} PostgreSQL connections (NUAGE_PG_MAX_CONNECTIONS), so that read-write clients always find a place; use a connection pool, or close idle ones"
                    ),
                );
                return conn.write_all(&out.buf);
            }
        },
    };
    // Logged in: no more deadline. A client may stay idle as long as it
    // likes, but not leave its answers unread: a write that makes no
    // progress for a minute ends the session.
    let socket = conn.socket();
    socket.deadline = None;
    let _ = socket.stream.set_write_timeout(Some(STALLED));
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
    // A message is as long as a request body may be.
    let max = state.options.max_body_bytes.max(LARGE_MESSAGE);
    let mut s = Session {
        state: state.clone(),
        app,
        read_only,
        branch,
        tx: None,
        since: None,
        failed: false,
        ended: None,
        savepoints: Vec::new(),
        statements: HashMap::new(),
        portals: HashMap::new(),
        held: 0,
        // Half of the port's memory, and room for a parameter as long as a message.
        most: (port.memory.total() / 2).max(max + (1 << 20)),
        share: Share::new(&port.memory),
        reading: share,
        skip_to_sync: false,
    };
    s.ready(&mut out);
    conn.write_all(&out.buf)?;
    let idle = options.idle_in_transaction.unwrap_or(IDLE_IN_TRANSACTION).max(Duration::from_millis(1));
    let lifetime = options.transaction_timeout.unwrap_or(TRANSACTION_TIMEOUT).max(Duration::from_millis(1));
    let mut conn = BufReader::with_capacity(READ_AHEAD, conn);
    loop {
        let socket = conn.socket();
        socket.idle = s.tx.is_some().then_some(idle);
        socket.idle_until = s.since.map(|since| since + lifetime);
        let (kind, body) = match read_msg(&mut conn, max, &mut s.reading, true)? {
            Incoming::Message(kind, body) => (kind, body),
            Incoming::Quiet => {
                s.timed_out(idle, lifetime);
                // On Windows a receive that timed out leaves its socket in no
                // known state: the session ends there, as PostgreSQL ends it.
                if cfg!(windows) {
                    let mut out = Out::to(conn.get_mut());
                    if let Some(ended) = s.ended.take() {
                        ended.tell(&mut out, "FATAL");
                    }
                    return out.end(true);
                }
                continue;
            }
            Incoming::Skipped(kind, why) => {
                let mut out = Out::to(conn.get_mut());
                s.skipped(&mut out, kind, why, max);
                out.end(true)?;
                continue;
            }
        };
        // A transaction kept busy past its lifetime ends all the same.
        if s.since.is_some_and(|since| since.elapsed() >= lifetime) {
            s.end_transaction(Ended::Lifetime(lifetime));
        }
        let step = answer_guarded(conn.get_mut(), |out| s.message(out, kind, &body))?;
        // The message, and what reading its statement took, are let go.
        drop(body);
        s.reading.hold(0);
        if step == Step::Close {
            return Ok(());
        }
    }
}

/// Answers a message with `answer`, and sends what it wrote. A bug met there
/// (a panic) is told to the client as PostgreSQL tells an internal error
/// (SQLSTATE `XX000`), and ends the session (its transaction rolled back),
/// not the server: [`Step::Close`].
fn answer_guarded(conn: &mut Conn, answer: impl FnOnce(&mut Out<'_>) -> Step) -> io::Result<Step> {
    let answered = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let mut out = Out::to(conn);
        let step = answer(&mut out);
        out.end(step == Step::Flush).map(|()| step)
    }));
    match answered {
        Ok(step) => step,
        Err(panic) => {
            let mut out = Out::default();
            error(
                &mut out,
                "XX000",
                &format!("internal error: {}; this connection is closed, the server goes on", panic_text(&*panic)),
            );
            let _ = conn.write_all(&out.buf).and_then(|()| conn.flush());
            Ok(Step::Close)
        }
    }
}

/// What a message leaves to do.
#[derive(Clone, Copy, PartialEq)]
enum Step {
    /// Its answers go with the next ones.
    Hold,
    /// Its answers go now (`Sync`, `Flush`, a simple query).
    Flush,
    /// The client said goodbye.
    Close,
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

fn error(out: &mut Out<'_>, code: &str, message: &str) {
    report(out, "ERROR", code, message);
}

/// An error of `severity` (`ERROR`, or `FATAL` when the session ends with it).
fn report(out: &mut Out<'_>, severity: &str, code: &str, message: &str) {
    out.msg(b'E', |b| {
        for (f, v) in [(b'S', severity), (b'V', severity), (b'C', code), (b'M', message)] {
            b.push(f);
            cstr(b, v);
        }
        b.push(0);
    });
}

/// Why a transaction was rolled back without its client asking.
#[derive(Clone, Copy)]
enum Ended {
    /// It was left idle this long.
    Idle(Duration),
    /// It was open this long.
    Lifetime(Duration),
}

impl Ended {
    /// Tells the client, as PostgreSQL tells it.
    fn tell(self, out: &mut Out<'_>, severity: &str) {
        let (code, message) = match self {
            Ended::Idle(idle) => (
                "25P03",
                format!(
                    "the transaction was rolled back: it was left idle for more than {} seconds \
                     (idle_in_transaction_session_timeout); run it again",
                    idle.as_secs_f64()
                ),
            ),
            Ended::Lifetime(lifetime) => (
                "25P04",
                format!(
                    "the transaction was rolled back: it was open for more than {} seconds \
                     (transaction_timeout); run it again, in shorter transactions",
                    lifetime.as_secs_f64()
                ),
            ),
        };
        report(out, severity, code, &message);
    }
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
    if message.contains("too many ") && message.contains(" in this session (")
        || message.contains("this session holds ")
        || message.contains("a statement is 1 MiB at most")
        || message.contains(" of memory once read, ")
    {
        return "54000";
    }
    if message.contains("the server holds many ") {
        return "53200";
    }
    if message.contains("bind message supplies ") || message.contains("bind message has ") {
        return "08P01";
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
    /// About what it holds (see [`statement_bytes`]).
    size: usize,
}

struct Portal {
    stmt: Arc<Prepared>,
    /// About what it holds: its parameters until it runs, then the rows it
    /// keeps between messages (counted in [`Session::held`]).
    size: usize,
    /// Its parameters, until it runs.
    params: Vec<Value>,
    formats: Vec<i16>,
    /// Its columns: as told to the client (`Describe`), else as found when
    /// it ran (`None`: it returns no rows).
    columns: Option<Vec<(String, i32)>>,
    /// Once run (at Execute, or at Describe when only a run tells its columns).
    ran: Option<Ran>,
}

/// What a portal's run left to send.
struct Ran {
    /// The rows not sent yet: each is let go once sent.
    rows: VecDeque<Vec<Value>>,
    /// Its command tag (`SELECT 3`), for when the last row is sent.
    tag: String,
    /// What of those rows is counted in the portal's size (none until they
    /// are kept from one message to the next).
    counted: usize,
}

/// A statement's columns, as known before it runs.
enum Columns {
    Known(Vec<(String, i32)>),
    /// It returns no rows.
    None,
    /// Only a run would tell.
    Unknown,
}

struct Session {
    state: Arc<State>,
    app: Arc<App>,
    read_only: bool,
    branch: String,
    /// The transaction `BEGIN` opened.
    tx: Option<EmbeddedTransaction>,
    /// When it began.
    since: Option<Instant>,
    /// A statement of that transaction failed: only `ROLLBACK` is accepted.
    failed: bool,
    /// The transaction was rolled back by a timer: the client's next message
    /// is told why.
    ended: Option<Ended>,
    /// Its savepoints, in order.
    savepoints: Vec<(String, Savepoint)>,
    statements: HashMap<String, Arc<Prepared>>,
    portals: HashMap<String, Portal>,
    /// About what the statements and portals hold in memory, in bytes.
    held: usize,
    /// What `held` may come to.
    most: usize,
    /// The part of `held` past [`SESSION_FREE`], taken from the port's memory.
    share: Share,
    /// The message being answered, and what reading its statement takes,
    /// past what is free: let go once it is answered.
    reading: Share,
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
    /// `DEALLOCATE [PREPARE] name` (`None`: `ALL`).
    Deallocate(Option<String>),
    /// A query answered at once (`SHOW`, `version()`...): column and value.
    Value(String, String),
    Empty,
}

impl Session {
    /// Answers one message of the client.
    fn message(&mut self, out: &mut Out<'_>, kind: u8, body: &[u8]) -> Step {
        match kind {
            b'X' => return Step::Close,
            b'H' => return Step::Flush,
            _ => {}
        }
        if let Some(ended) = self.ended.take() {
            // Whatever it asks (a `COMMIT` above all), the transaction it
            // thinks open is gone: it is told before anything runs.
            ended.tell(out, "ERROR");
            match kind {
                b'Q' => {
                    self.ready(out);
                    return Step::Flush;
                }
                b'S' => {}
                _ => {
                    self.skip_to_sync = true;
                    return Step::Hold;
                }
            }
        }
        match kind {
            b'S' => {
                self.skip_to_sync = false;
                if self.tx.is_none() {
                    // An implicit transaction ends at Sync: nothing to keep.
                    self.forget_portals();
                }
                self.ready(out);
                Step::Flush
            }
            _ if self.skip_to_sync => Step::Hold,
            b'Q' => {
                let sql = Body(body).str().unwrap_or_default();
                self.simple(out, &sql);
                self.ready(out);
                Step::Flush
            }
            _ => {
                if let Err(e) = self.extended(out, kind, body) {
                    self.fail(out, &e);
                    self.skip_to_sync = true;
                }
                Step::Hold
            }
        }
    }

    /// No message came in the time a read may wait in a transaction: it was
    /// left idle too long, or it reached its lifetime.
    fn timed_out(&mut self, idle: Duration, lifetime: Duration) {
        let old = self.since.is_some_and(|since| since.elapsed() >= lifetime);
        self.end_transaction(if old { Ended::Lifetime(lifetime) } else { Ended::Idle(idle) });
    }

    /// Rolls back the transaction its client did not end in time, so that it
    /// keeps no pages from being reused; the client hears of it at its next
    /// message.
    fn end_transaction(&mut self, why: Ended) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.rollback_now();
            self.ended = Some(why);
        }
        self.since = None;
        self.failed = false;
        self.savepoints.clear();
        // The portals of a transaction end with it.
        self.forget_portals();
    }

    /// Holds `after` bytes in place of `before` (a statement or a portal
    /// replaced, grown or let go): refused past what a session may hold, or
    /// when the port's memory is all held by others.
    fn account(&mut self, before: usize, after: usize) -> Result<()> {
        let held = self.held.saturating_sub(before).saturating_add(after);
        if after > before {
            if held > self.most {
                return Err(Error::Validation(format!(
                    "this session holds {} MiB of prepared statements, portals and the rows they keep, and may hold {} MiB: close some (or read results whole) before making more",
                    held >> 20,
                    self.most >> 20
                )));
            }
            if !self.share.hold(held.saturating_sub(SESSION_FREE)) {
                return Err(Error::Validation(format!(
                    "the server holds many prepared statements, portals and results at once ({} MiB in all): close some, or try again in a moment",
                    self.share.total() >> 20
                )));
            }
        } else {
            self.share.hold(held.saturating_sub(SESSION_FREE));
        }
        self.held = held;
        Ok(())
    }

    /// Takes one more statement or portal, among `count` of at most `most`
    /// (`replaced`: the size of the one of the same name it takes the place of).
    fn may_hold(&mut self, replaced: Option<usize>, size: usize, count: usize, most: usize, what: &str) -> Result<()> {
        if replaced.is_none() && count >= most {
            return Err(Error::Validation(format!(
                "too many {what} in this session ({most} at most): close some before making more"
            )));
        }
        self.account(replaced.unwrap_or(0), size)
    }

    /// Reading an SQL text takes about [`statement_bytes`]: refused when
    /// longer than a statement may be, or when it would take more than the
    /// session may hold (`kept`: it is kept, a prepared statement in place
    /// of one of that size) or the port has, before it is parsed; past what
    /// is free, held until the message is answered.
    fn read_sql(&mut self, sql: &str, kept: Option<usize>) -> Result<usize> {
        if sql.len() > MAX_SQL_TEXT {
            return Err(Error::Validation(format!(
                "the SQL text is {} bytes: a statement is 1 MiB at most; send a long script in several parts",
                sql.len()
            )));
        }
        let bytes = statement_bytes(sql);
        let most = match kept {
            Some(replaced) => self.most.saturating_sub(self.held.saturating_sub(replaced)),
            None => self.reading.total(),
        };
        if bytes > most {
            return Err(Error::Validation(format!(
                "this statement would take about {} MiB of memory once read, and this session has {} MiB left for it: send it in smaller parts",
                bytes.div_ceil(1 << 20),
                most >> 20
            )));
        }
        if bytes > SESSION_FREE && !self.reading.hold_waiting(self.reading.held() + bytes) {
            return Err(Error::Validation(format!(
                "the server holds many large statements and messages at once ({} MiB in all): try again in a moment",
                self.reading.total() >> 20
            )));
        }
        Ok(bytes)
    }

    fn forget_portals(&mut self) {
        let sizes: usize = self.portals.drain().map(|(_, p)| p.size).sum();
        let _ = self.account(sizes, 0);
    }

    /// Forgets the prepared statement `name` (`None`: all of them). One that
    /// does not exist is no error: there is nothing to forget.
    fn deallocate(&mut self, name: Option<&str>) {
        let sizes: usize = match name {
            Some(name) => self.statements.remove(name).map_or(0, |p| p.size),
            None => self.statements.drain().map(|(_, p)| p.size).sum(),
        };
        let _ = self.account(sizes, 0);
    }

    /// A message read through and not kept: refused as a statement is.
    fn skipped(&mut self, out: &mut Out<'_>, kind: u8, why: Skip, max: usize) {
        match why {
            Skip::TooLong(len) => error(
                out,
                "54000",
                &format!(
                    "message too long: {len} bytes, this server reads {max} at most (the largest request body); send less at once"
                ),
            ),
            Skip::NoMemory => error(
                out,
                "53200",
                &format!(
                    "the server holds many large messages and statements at once ({} MiB in all): try again in a moment",
                    self.reading.total() >> 20
                ),
            ),
        }
        if self.tx.is_some() {
            self.failed = true;
        }
        if kind == b'Q' {
            self.ready(out);
        } else {
            self.skip_to_sync = true;
        }
    }

    fn ready(&self, out: &mut Out<'_>) {
        let status = match (&self.tx, self.failed) {
            (None, _) => b'I',
            (Some(_), false) => b'T',
            (Some(_), true) => b'E',
        };
        out.msg(b'Z', |b| b.push(status));
    }

    fn fail(&mut self, out: &mut Out<'_>, e: &Error) {
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
            "deallocate" => {
                // The name as it was prepared: as written between double
                // quotes, else in lower case, as PostgreSQL reads a name.
                let name = sql
                    .split_whitespace()
                    .skip(1)
                    .map(|w| w.trim_end_matches(';'))
                    .find(|w| !w.is_empty() && !w.eq_ignore_ascii_case("prepare"));
                match name {
                    Some(all) if all.eq_ignore_ascii_case("all") => Plan::Deallocate(None),
                    Some(n) => Plan::Deallocate(Some(match n.strip_prefix('"').and_then(|n| n.strip_suffix('"')) {
                        Some(quoted) => quoted.replace("\"\"", "\""),
                        None => n.to_lowercase(),
                    })),
                    None => Plan::Tag("DEALLOCATE".into()),
                }
            }
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
        if what == "begin" {
            if self.tx.is_none() {
                self.tx = Some(Embedded::from_engine(self.app.engine.clone()).on_branch(&self.branch)?.begin_now()?);
                self.since = Some(Instant::now());
                self.failed = false;
            }
            return Ok("BEGIN".into());
        }
        // The portals of a transaction end with it, as PostgreSQL's do.
        self.since = None;
        self.forget_portals();
        match what {
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

    fn simple(&mut self, out: &mut Out<'_>, sql: &str) {
        if let Err(e) = self.simple_inner(out, sql) {
            self.fail(out, &e);
        }
    }

    fn simple_inner(&mut self, out: &mut Out<'_>, sql: &str) -> Result<()> {
        self.read_sql(sql, None)?;
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
                    for (i, r) in results.into_iter().enumerate() {
                        let tag = script.tag(i, &r);
                        send_result(out, r, &tag)?;
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

    fn simple_one(&mut self, out: &mut Out<'_>, plan: Plan) -> Result<()> {
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
            Plan::Deallocate(name) => {
                if self.failed {
                    return Err(Error::Validation("current transaction is aborted".into()));
                }
                self.deallocate(name.as_deref());
                complete(out, if name.is_some() { "DEALLOCATE" } else { "DEALLOCATE ALL" });
            }
            Plan::Value(name, value) => {
                row_description(out, &[(name, 25)], &[]);
                data_row(out, &[Value::Text(value)], &[25], &[])?;
                complete(out, "SHOW");
            }
            Plan::Run(script) => {
                let results = self.run(&script, &[])?;
                for (i, r) in results.into_iter().enumerate() {
                    let tag = script.tag(i, &r);
                    send_result(out, r, &tag)?;
                }
            }
        }
        Ok(())
    }

    // --- extended protocol ------------------------------------------------

    fn extended(&mut self, out: &mut Out<'_>, kind: u8, body: &[u8]) -> Result<()> {
        let mut b = Body(body);
        match kind {
            b'P' => {
                let name = b.str()?;
                let sql = b.str()?;
                let n = b.u16()? as usize;
                let mut param_types = Vec::with_capacity(n);
                for _ in 0..n {
                    param_types.push(b.i32()?);
                }
                let size = self.read_sql(&sql, Some(self.statements.get(&name).map_or(0, |p| p.size)))?;
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
                let size = size + 8 * param_types.len();
                let replaced = self.statements.get(&name).map(|p| p.size);
                self.may_hold(replaced, size, self.statements.len(), SESSION_STATEMENTS, "prepared statements")?;
                let stmt =
                    Prepared { sql, script, param_types, inferred, typed_columns, typed_names, columns: None, size };
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
                let nf = b.u16()? as usize;
                let mut pformats = Vec::with_capacity(nf);
                for _ in 0..nf {
                    pformats.push(b.i16()?);
                }
                // As many values as the statement has parameters, as PostgreSQL
                // checks: a few bytes per `NULL` would otherwise hold a slot of
                // memory each, for parameters nothing reads.
                let np = b.u16()? as usize;
                let wanted = stmt.param_types.len();
                if np != wanted {
                    return Err(Error::Validation(format!(
                        "bind message supplies {np} parameters, but prepared statement \"{name}\" requires {wanted}"
                    )));
                }
                if nf > 1 && nf != np {
                    return Err(Error::Validation(format!(
                        "bind message has {nf} parameter formats but {np} parameters"
                    )));
                }
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
                let nr = b.u16()? as usize;
                let mut formats = Vec::with_capacity(nr);
                for _ in 0..nr {
                    formats.push(b.i16()?);
                }
                let size = PORTAL_BYTES + 2 * formats.len() + params.iter().map(value_bytes).sum::<usize>();
                let replaced = self.portals.get(&portal).map(|p| p.size);
                self.may_hold(replaced, size, self.portals.len(), SESSION_PORTALS, "portals")?;
                self.portals.insert(portal, Portal { stmt, size, params, formats, columns: None, ran: None });
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
                    let columns = match self.known_columns(&stmt) {
                        Columns::Known(cols) => Some(cols),
                        Columns::None => None,
                        Columns::Unknown => self.dry_run_columns(&stmt),
                    };
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
                        size: stmt.size,
                    };
                    self.statements.insert(name, Arc::new(described));
                } else {
                    let columns = self.describe_portal(&name)?;
                    match (columns, self.portals.get(&name)) {
                        (Some(cols), Some(p)) => row_description(out, &cols, &p.formats),
                        _ => out.msg(b'n', |_| {}),
                    }
                }
            }
            b'E' => {
                let name = b.str()?;
                let max = b.i32()?.max(0) as usize;
                self.run_portal(&name)?;
                let p = self.portals.get_mut(&name).ok_or_else(|| Error::Validation("no such portal".into()))?;
                match (&p.stmt.script, p.ran.as_mut()) {
                    (Some(_), Some(ran)) => {
                        let types: Vec<i32> = p.columns.iter().flatten().map(|c| c.1).collect();
                        let n = if max > 0 { ran.rows.len().min(max) } else { ran.rows.len() };
                        // Each row is let go once written: a portal read in
                        // batches holds less and less.
                        let mut sent = 0;
                        for row in ran.rows.drain(..n) {
                            if ran.counted > 0 {
                                sent += row_bytes(&row);
                            }
                            data_row(out, &row, &types, &p.formats)?;
                        }
                        let left = if ran.rows.is_empty() {
                            ran.rows = VecDeque::new();
                            complete(out, &ran.tag);
                            0
                        } else {
                            out.msg(b's', |_| {});
                            // The rows left wait for the next Execute: counted until then.
                            if ran.counted == 0 { rows_bytes(&ran.rows) } else { ran.counted.saturating_sub(sent) }
                        };
                        if left != ran.counted {
                            self.keep_rows(&name, left)?;
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
                            Plan::Deallocate(name) => {
                                self.deallocate(name.as_deref());
                                complete(out, if name.is_some() { "DEALLOCATE" } else { "DEALLOCATE ALL" });
                            }
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
                let size = if what == b'S' {
                    self.statements.remove(&name).map(|p| p.size)
                } else {
                    self.portals.remove(&name).map(|p| p.size)
                };
                let _ = self.account(size.unwrap_or(0), 0);
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
        let p =
            self.portals.get_mut(name).ok_or_else(|| Error::Validation(format!("portal \"{name}\" does not exist")))?;
        if p.ran.is_some() {
            return Ok(());
        }
        let Some(script) = p.stmt.script.clone() else { return Ok(()) };
        // It runs once: its parameters are not needed after.
        let params = std::mem::take(&mut p.params);
        let size = p.size.saturating_sub(params.iter().map(value_bytes).sum());
        let results = self.run(&script, &params)?;
        drop(params);
        let Some(p) = self.portals.get_mut(name) else { return Ok(()) };
        let before = std::mem::replace(&mut p.size, size);
        let ran = match results.into_iter().next() {
            Some(r) => {
                let told = p.columns.take().filter(|cols| cols.len() == r.columns.len());
                p.columns = if r.returns_rows { told.or_else(|| Some(found_columns(&p.stmt, &r))) } else { None };
                Ran { tag: script.tag(0, &r), rows: r.rows.into(), counted: 0 }
            }
            None => Ran { rows: VecDeque::new(), tag: "SELECT 0".into(), counted: 0 },
        };
        p.ran = Some(ran);
        let _ = self.account(before, size);
        Ok(())
    }

    /// Counts `counted` bytes for the rows portal `name` keeps for a later
    /// message, in place of what was counted for them. Past what may be
    /// held, the portal ends here (refused), its rows let go.
    fn keep_rows(&mut self, name: &str, counted: usize) -> Result<()> {
        let Some((before, was)) = self.portals.get(name).and_then(|p| Some((p.size, p.ran.as_ref()?.counted))) else {
            return Ok(());
        };
        let after = before.saturating_sub(was) + counted;
        if let Err(e) = self.account(before, after) {
            if let Some(p) = self.portals.remove(name) {
                let _ = self.account(p.size, 0);
            }
            return Err(e);
        }
        if let Some(p) = self.portals.get_mut(name) {
            p.size = after;
            if let Some(ran) = p.ran.as_mut() {
                ran.counted = counted;
            }
        }
        Ok(())
    }

    /// The columns of a portal, for `Describe`: as its statement's say when
    /// they are known; else, for a statement that reads, from its run (which
    /// `Execute` sends, its rows counted until then); for one that writes, as
    /// the statement names them.
    fn describe_portal(&mut self, name: &str) -> Result<Option<Vec<(String, i32)>>> {
        let p = self.portals.get(name).ok_or_else(|| Error::Validation(format!("portal \"{name}\" does not exist")))?;
        if p.ran.is_none() {
            let told = match self.known_columns(&p.stmt) {
                Columns::Known(cols) => Some(Some(cols)),
                Columns::None => Some(None),
                Columns::Unknown if p.stmt.script.as_ref().is_some_and(|s| s.writes()) => Some(named_columns(&p.stmt)),
                Columns::Unknown => None,
            };
            if let Some(columns) = told {
                if let Some(p) = self.portals.get_mut(name) {
                    p.columns = columns.clone();
                }
                return Ok(columns);
            }
            self.run_portal(name)?;
            let rows = self.portals.get(name).and_then(|p| p.ran.as_ref()).map_or(0, |ran| rows_bytes(&ran.rows));
            if rows > 0 {
                self.keep_rows(name, rows)?;
            }
        }
        Ok(self.portals.get(name).and_then(|p| p.columns.clone()))
    }

    /// The columns of a statement known without running it: from the
    /// statement itself (what it reads from tables, how it names them).
    fn known_columns(&self, stmt: &Prepared) -> Columns {
        if let Some(cols) = stmt.columns.as_ref().filter(|c| !c.is_empty()) {
            return Columns::Known(cols.clone());
        }
        let Some(script) = &stmt.script else {
            return match self.plan(&stmt.sql) {
                Ok(Plan::Value(name, _)) => Columns::Known(vec![(name, TEXT)]),
                _ => Columns::None,
            };
        };
        if !script.returns_rows() {
            return Columns::None;
        }
        // The names are a run's (see `sql::typing`); `EXPLAIN` answers with a
        // plan, which only a run tells.
        let (names, types) = (&stmt.typed_names, &stmt.typed_columns);
        let explains = stmt.sql.trim_start().get(..7).is_some_and(|w| w.eq_ignore_ascii_case("explain"));
        let complete = !names.is_empty() && names.len() == types.len() && !types.contains(&0);
        if complete && !explains {
            Columns::Known(names.iter().cloned().zip(types.iter().copied()).collect())
        } else {
            Columns::Unknown
        }
    }

    /// The columns of a statement whose tables do not tell them all: for a
    /// statement that only reads, from a run with its parameters `NULL`; a
    /// statement that writes is never run for that, and has the names it
    /// gives (text where the type is unknown).
    fn dry_run_columns(&self, stmt: &Prepared) -> Option<Vec<(String, i32)>> {
        let script = stmt.script.as_ref()?;
        if script.writes() {
            return named_columns(stmt);
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
            None => named_columns(stmt),
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

/// The columns of a statement's result: as described, else as found.
fn found_columns(stmt: &Prepared, r: &SqlResult) -> Vec<(String, i32)> {
    match &stmt.columns {
        Some(cols) if cols.len() == r.columns.len() => cols.clone(),
        _ => columns_of(r, &stmt.typed_columns),
    }
}

/// The columns as a statement names them, text where their type is unknown
/// (`None`: their names are unknown too).
fn named_columns(stmt: &Prepared) -> Option<Vec<(String, i32)>> {
    let (names, types) = (&stmt.typed_names, &stmt.typed_columns);
    (!names.is_empty() && names.len() == types.len())
        .then(|| names.iter().zip(types).map(|(n, t)| (n.clone(), if *t == 0 { TEXT } else { *t })).collect())
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

// ---------------------------------------------------------------------------
// What sessions hold
// ---------------------------------------------------------------------------

/// What a portal holds besides its parameters and rows.
const PORTAL_BYTES: usize = 256;
/// What the allocator adds to a text or a list it gives room to.
const BLOCK: usize = 16;

/// About what a prepared statement of `sql` holds: its text twice (as sent,
/// and as the statement keeps it), and its syntax tree, some fifty bytes a
/// token (`IN (1,1,1,...)` takes fifty times its text). Counted on the text,
/// before it is parsed: a statement too large to keep is refused without
/// building it.
fn statement_bytes(sql: &str) -> usize {
    1024 + 2 * sql.len() + 64 * tokens(sql)
}

/// About how many tokens an SQL text has: words and numbers, quoted texts
/// and names, and signs, one each.
fn tokens(sql: &str) -> usize {
    let b = sql.as_bytes();
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'.' || c == b'$' || c >= 0x80;
    let (mut n, mut i) = (0, 0);
    while i < b.len() {
        let c = b[i];
        i += 1;
        if c.is_ascii_whitespace() {
            continue;
        }
        n += 1;
        if c == b'\'' || c == b'"' {
            while i < b.len() && b[i] != c {
                i += 1;
            }
            i += 1;
        } else if word(c) {
            while i < b.len() && word(b[i]) {
                i += 1;
            }
        }
    }
    n
}

/// About what a value takes in memory.
fn value_bytes(v: &Value) -> usize {
    std::mem::size_of::<Value>()
        + match v {
            Value::Text(s) => s.len() + BLOCK,
            Value::Bytes(b) => b.len() + BLOCK,
            Value::Json(j) => json_bytes(j) + BLOCK,
            _ => 0,
        }
}

/// About what a JSON value takes besides its slot.
fn json_bytes(j: &serde_json::Value) -> usize {
    use serde_json::Value as J;
    match j {
        J::String(s) => s.len() + BLOCK,
        J::Array(a) => a.iter().map(|x| std::mem::size_of::<J>() + json_bytes(x)).sum::<usize>() + BLOCK,
        J::Object(o) => o.iter().map(|(k, x)| k.len() + 2 * BLOCK + std::mem::size_of::<J>() + json_bytes(x)).sum(),
        _ => 0,
    }
}

/// About what a row takes in memory.
fn row_bytes(row: &[Value]) -> usize {
    std::mem::size_of::<Vec<Value>>() + BLOCK + row.iter().map(value_bytes).sum::<usize>()
}

fn rows_bytes<'r>(rows: impl IntoIterator<Item = &'r Vec<Value>>) -> usize {
    rows.into_iter().map(|r| row_bytes(r)).sum()
}

fn complete(out: &mut Out<'_>, tag: &str) {
    out.msg(b'C', |b| cstr(b, tag));
}

/// A result in the simple protocol (text): each row is let go once written.
fn send_result(out: &mut Out<'_>, r: SqlResult, tag: &str) -> Result<()> {
    if r.returns_rows {
        let cols = columns_of(&r, &[]);
        row_description(out, &cols, &[]);
        let types: Vec<i32> = cols.iter().map(|c| c.1).collect();
        for row in r.rows {
            data_row(out, &row, &types, &[])?;
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

fn row_description(out: &mut Out<'_>, cols: &[(String, i32)], formats: &[i16]) {
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

fn data_row(out: &mut Out<'_>, row: &[Value], types: &[i32], formats: &[i16]) -> Result<()> {
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

/// Bytes written as hexadecimal digits, two each (`None`: not that).
fn unhex(hex: &[u8]) -> Option<Vec<u8>> {
    let digit = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let (pairs, odd) = hex.as_chunks::<2>();
    if !odd.is_empty() {
        return None;
    }
    pairs.iter().map(|[high, low]| Some(digit(*high)? << 4 | digit(*low)?)).collect()
}

/// A parameter the client sent, of type `oid` (0: unsaid), in text (0) or binary (1).
fn decode_param(raw: &[u8], oid: i32, format: i16) -> Result<Value> {
    let bad = || Error::Conversion(format!("invalid input for a parameter of PostgreSQL type {oid}"));
    // PostgreSQL's infinity among them, which no date here stands for.
    let out_of_range = || {
        Error::Conversion(format!(
            "a parameter of PostgreSQL type {oid} is out of the range of dates this server stores"
        ))
    };
    let text = || String::from_utf8(raw.to_vec()).map_err(|_| bad());
    if format == 0 {
        let s = text()?;
        return Ok(match oid {
            BOOL => Value::Bool(matches!(s.to_ascii_lowercase().as_str(), "t" | "true" | "1" | "on" | "yes" | "y")),
            INT2 | INT4 | INT8 | OID => Value::I64(s.trim().parse().map_err(|_| bad())?),
            FLOAT4 | FLOAT8 => Value::F64(s.trim().parse().map_err(|_| bad())?),
            BYTEA => match s.strip_prefix("\\x") {
                Some(hex) => Value::Bytes(unhex(hex.as_bytes()).ok_or_else(bad)?),
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
            let t = epoch().checked_add_signed(chrono::Duration::microseconds(micros)).ok_or_else(out_of_range)?;
            if oid == TIMESTAMP { Value::Timestamp(t) } else { Value::TimestampTz(t.and_utc()) }
        }
        DATE => {
            let days = i32::from_be_bytes(n(4)?.try_into().map_err(|_| bad())?);
            let day = chrono::Duration::try_days(days as i64).and_then(|d| epoch().date().checked_add_signed(d));
            Value::Date(day.ok_or_else(out_of_range)?)
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

    /// A bug met while answering is told to the client, after what was
    /// already written, and ends the session; the thread goes on.
    #[test]
    fn a_panic_is_told_and_ends_the_session() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let mut conn = Conn::Plain(Socket::new(listener.accept().unwrap().0));
        let step = answer_guarded(&mut conn, |out| {
            complete(out, "SELECT 1");
            panic!("a test of the guard");
        })
        .unwrap();
        assert!(step == Step::Close);
        let step = answer_guarded(&mut conn, |out| {
            complete(out, "SELECT 2");
            Step::Flush
        })
        .unwrap();
        assert!(step == Step::Flush);
        drop(conn);
        let mut got = Vec::new();
        client.read_to_end(&mut got).unwrap();
        let text = String::from_utf8_lossy(&got);
        // The error, then nothing of the first answer (it had not been sent), then the second.
        assert_eq!(got[0], b'E', "{text}");
        assert!(text.contains("XX000") && text.contains("internal error: a test of the guard"), "{text}");
        assert!(!text.contains("SELECT 1") && text.contains("SELECT 2"), "{text}");
    }

    /// A read waits only for what is left of its deadline, and a wait that
    /// ends before any byte leaves the stream as it was.
    #[test]
    fn reads_wait_until_their_deadline() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let mut socket = Socket::new(listener.accept().unwrap().0);
        let memory = PortMemory::new(Some(1 << 20), 0);
        let mut share = Share::new(&memory);
        socket.idle = Some(Duration::from_millis(50));
        let started = Instant::now();
        assert!(matches!(read_msg(&mut socket, 1 << 20, &mut share, true).unwrap(), Incoming::Quiet));
        assert!(started.elapsed() < Duration::from_secs(5));
        client.write_all(b"Q\0\0\0\x0dselect 1\0").unwrap();
        let Incoming::Message(kind, body) = read_msg(&mut socket, 1 << 20, &mut share, true).unwrap() else {
            panic!("a message")
        };
        assert_eq!((kind, body.as_slice()), (b'Q', &b"select 1\0"[..]));
        assert!(socket.within.is_none(), "no clock between messages");
        // A message longer than may be read is read through, not kept: the next one is read.
        client.write_all(b"Q\0\0\0\x0aabcdefQ\0\0\0\x05x").unwrap();
        assert!(matches!(
            read_msg(&mut socket, 8, &mut share, true).unwrap(),
            Incoming::Skipped(b'Q', Skip::TooLong(10))
        ));
        assert!(matches!(read_msg(&mut socket, 8, &mut share, true).unwrap(), Incoming::Message(b'Q', b) if b == b"x"));
        // Before login it is not read at all.
        client.write_all(b"p\0\0\0\x0aabcdef").unwrap();
        assert!(read_msg(&mut socket, 8, &mut share, false).is_err());
        // Within a message, the clock runs: its time over, the read fails at once.
        socket.within = Some(Instant::now());
        let e = socket.read(&mut [0u8; 1]).unwrap_err();
        assert!(timed_out(&e), "{e}");
        socket.within = None;
        // A deadline passed: the read fails at once.
        socket.deadline = Some(Instant::now());
        let e = socket.read(&mut [0u8; 1]).unwrap_err();
        assert!(timed_out(&e), "{e}");
    }

    /// A large message takes its memory from the port's as it arrives; when
    /// the port has none left, it is read through and refused, and the
    /// session reads the next one.
    #[test]
    fn large_messages_take_the_ports_memory() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let mut socket = Socket::new(listener.accept().unwrap().0);
        let memory = PortMemory::new(Some(1 << 20), 0);
        let mut share = Share::new(&memory);
        let large = |n: usize| {
            let mut m = vec![b'Q'];
            m.extend_from_slice(&(n as u32 + 4).to_be_bytes());
            m.extend(std::iter::repeat_n(b'x', n));
            m
        };
        let writer = std::thread::spawn({
            let mut client = client.try_clone().unwrap();
            move || {
                client.write_all(&large(700 << 10)).unwrap();
                client.write_all(&large(700 << 10)).unwrap();
            }
        });
        let Incoming::Message(_, body) = read_msg(&mut socket, 4 << 20, &mut share, true).unwrap() else { panic!() };
        assert_eq!(body.len(), 700 << 10);
        assert!(share.held() >= 700 << 10, "held while it is answered: {}", share.held());
        // Another session holds the port's memory meanwhile.
        let mut other = Share::new(&memory);
        assert!(other.hold(1 << 20) || { share.hold(0) && other.hold(1 << 20) });
        assert!(matches!(
            read_msg(&mut socket, 4 << 20, &mut share, true).unwrap(),
            Incoming::Skipped(b'Q', Skip::NoMemory)
        ));
        writer.join().unwrap();
        drop(other);
        client.write_all(&large(100 << 10)).unwrap();
        let Incoming::Message(_, body) = read_msg(&mut socket, 4 << 20, &mut share, true).unwrap() else { panic!() };
        assert_eq!(body.len(), 100 << 10);
    }

    /// What a statement is counted at follows its tokens, not only its
    /// length: a list of numbers counts far more than a text.
    #[test]
    fn statements_are_counted_by_their_tokens() {
        assert_eq!(tokens("SELECT id, name FROM t WHERE id = $1 AND n = 'a b c'"), 14);
        assert_eq!(tokens("select \"Weird name\" from x.y"), 4);
        let list = format!("SELECT id FROM t WHERE id IN ({})", vec!["1"; 1000].join(","));
        let text = format!("SELECT '{}'", "x".repeat(list.len()));
        assert!(
            statement_bytes(&list) > 20 * statement_bytes(&text) / 2,
            "{} {}",
            statement_bytes(&list),
            statement_bytes(&text)
        );
        assert!(statement_bytes(&list) > 60 * list.len());
        assert!(statement_bytes("SELECT 1") < 2048);
    }

    /// Values a driver may send that no value here stands for are refused,
    /// as a bad value is: the session goes on.
    #[test]
    fn parameters_out_of_range_are_refused() {
        // PostgreSQL's infinity: the largest timestamp and date.
        for (raw, oid) in [
            (i64::MAX.to_be_bytes().to_vec(), TIMESTAMP),
            (i64::MIN.to_be_bytes().to_vec(), TIMESTAMPTZ),
            (i32::MAX.to_be_bytes().to_vec(), DATE),
            (i32::MIN.to_be_bytes().to_vec(), DATE),
            ((-1i64).to_be_bytes().to_vec(), TIME),
        ] {
            let e = decode_param(&raw, oid, 1).unwrap_err();
            assert_eq!(sqlstate(&e), "22P02", "{oid}: {e}");
        }
        // Hexadecimal digits cut in the middle of a character, or odd in number.
        for raw in ["\\xa\u{e9}", "\\xabc", "\\x+f", "\\xzz"] {
            assert!(decode_param(raw.as_bytes(), BYTEA, 0).is_err(), "{raw}");
        }
        assert_eq!(decode_param(b"\\x", BYTEA, 0).unwrap(), Value::Bytes(Vec::new()));
        assert_eq!(decode_param(b"\\xFF00", BYTEA, 0).unwrap(), Value::Bytes(vec![255, 0]));
    }
}
