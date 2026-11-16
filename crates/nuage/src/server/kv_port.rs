//! The key-value store on a port of its own (`NUAGE_KV_PORT`): an
//! application's store ([`crate::kv`]), reached over a socket that stays
//! open, with a token of the application as the password.
//!
//! The wire is RESP (RESP2, and RESP3 after `HELLO 3`), the protocol Redis
//! clients speak: no client to write, each language already has one.
//!
//! ```text
//! redis-cli -u redis://default:<token>@db.example.com:6379
//! ```
//!
//! The token picks the application, as everywhere else; a read-only token
//! reads. With a certificate (`NUAGE_TLS_CERT`, `NUAGE_TLS_KEY`) the same port
//! also answers TLS: the first byte of a connection tells which. Commands sent
//! together (a pipeline) are answered together.
//!
//! A client has 15 seconds to log in, with a few small commands; the port
//! serves
//! [`ServerOptions::kv_max_connections`](super::ServerOptions::kv_max_connections)
//! connections at once (read-only tokens, half of them at most), a command
//! comes at a pace, a transaction (`MULTI`) queues what a request body may
//! hold, and what the connections hold together past 64 KiB each (commands
//! being read, transactions being queued) is bounded: an eighth of the
//! server's memory, and room for four of the largest request bodies.
//! With the store in the application's database (`NUAGE_KV_STORAGE=database`)
//! a command waits for the disk off the async threads, a transaction
//! (`MULTI` ... `EXEC`) is one commit, and a write the disk refuses answers
//! `-ERR storage: <reason>`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

use super::State;
use super::apps::App;
use super::kv::on;
use super::net::{PortMemory, Share};
use crate::kv::Reply;

/// Settings of the key-value port.
#[derive(Clone, Default)]
pub struct KvOptions {
    /// The server's certificate, for TLS.
    pub tls: Option<Arc<rustls::ServerConfig>>,
}

impl std::fmt::Debug for KvOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KvOptions").field("tls", &self.tls.is_some()).finish()
    }
}

/// Most arguments a command may have.
const MAX_ARGS: usize = 100_000;

/// Time a client has to log in (`AUTH`, or `HELLO ... AUTH`), TLS included,
/// from its connection on.
const LOGIN_TIME: Duration = Duration::from_secs(15);
/// Before login a command has at most this many arguments, of at most this
/// many bytes each, and at most this many bytes wait to be read: room to log
/// in, none to hold memory for a client nobody knows.
const LOGIN_ARGS: usize = 16;
const LOGIN_ARG_BYTES: usize = 4 << 10;
const LOGIN_BUFFER: usize = 8 << 10;
/// Commands a client may send before it logs in: logging in takes one to
/// three, and each refused one has an answer to hold.
const LOGIN_COMMANDS: usize = 10;
/// Answers held before they are sent, in a long pipeline.
const SEND_AT: usize = 1 << 20;
/// A client that has not taken its answers in this long (and a second more
/// for each [`COMMAND_PACE`] bytes of them) is gone: its connection is
/// closed. A command it started has as long to come whole, and its next
/// bytes come within this long.
const STALLED: Duration = Duration::from_secs(60);
const COMMAND_PACE: usize = 64 << 10;
/// What a connection holds of its port's memory for nothing: a command or a
/// transaction past this takes its share of the port's.
const FREE: usize = 64 << 10;
/// Once a connection is closed for a reason it was told, what the client
/// still sends is read (and let go) this long, this much at most, so that
/// the reason reaches it instead of a reset.
const LINGER: Duration = Duration::from_secs(1);
const LINGER_BYTES: u64 = 1 << 20;
/// Commands a transaction (`MULTI`) may queue. What they hold together is
/// bounded too, by the largest request body.
const QUEUE_COMMANDS: usize = 100_000;
/// What a queued argument costs besides its bytes.
const QUEUED_ARG_COST: usize = 32;

/// The answer to a connection past [`ServerOptions::kv_max_connections`](super::ServerOptions).
pub(super) const TOO_MANY_CLIENTS: &[u8] = b"-ERR max number of clients reached\r\n";

/// What the connections of one key-value port share.
pub(crate) struct Port {
    memory: Arc<PortMemory>,
    /// Connections served at once at most.
    most: usize,
    open: AtomicUsize,
    /// Connections logged in with a read-only token.
    read_only: AtomicUsize,
}

impl Port {
    /// `memory`: see [`PortMemory::new`].
    pub(crate) fn new(memory: Option<usize>, max_body: usize, most: usize) -> Arc<Port> {
        Arc::new(Port {
            memory: PortMemory::new(memory, max_body),
            most: most.max(1),
            open: AtomicUsize::new(0),
            read_only: AtomicUsize::new(0),
        })
    }

    /// One more of `count` (connections, or read-only ones) if fewer than `most`.
    fn take(count: &AtomicUsize, most: usize) -> bool {
        super::net::add_within(count, 1, most)
    }
}

/// A connection's place among those the listener serves at once: given back
/// when the connection ends, however it ends.
pub(super) struct Place(Arc<Port>);

impl Place {
    /// A place, if one is left.
    pub(super) fn take(port: &Arc<Port>) -> Option<Place> {
        Port::take(&port.open, port.most).then(|| Place(port.clone()))
    }
}

impl Drop for Place {
    fn drop(&mut self) {
        self.0.open.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A session's place among those read-only tokens may have: half of the
/// port's, so that their holders (a token in a web page is anyone's) never
/// keep the application's own clients out.
struct ReadOnlyPlace(Arc<Port>);

impl ReadOnlyPlace {
    fn take(port: &Arc<Port>) -> Option<ReadOnlyPlace> {
        Port::take(&port.read_only, port.most.div_ceil(2)).then(|| ReadOnlyPlace(port.clone()))
    }
}

impl Drop for ReadOnlyPlace {
    fn drop(&mut self) {
        self.0.read_only.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A command read from the wire: its arguments, and how many bytes it took.
pub(crate) type Parsed = Option<(Vec<Vec<u8>>, usize)>;

/// One command at the start of `buf`, of at most `max_args` arguments of at
/// most `max_bytes` bytes each; `None` when it is not all there yet.
pub(crate) fn parse(buf: &[u8], max_args: usize, max_bytes: usize) -> Result<Parsed, String> {
    fn line(buf: &[u8], from: usize) -> Option<(&[u8], usize)> {
        let end = buf[from..].iter().position(|b| *b == b'\n')? + from;
        let text = &buf[from..end];
        Some((text.strip_suffix(b"\r").unwrap_or(text), end + 1))
    }
    fn count(text: &[u8], max: usize, what: &str) -> Result<usize, String> {
        std::str::from_utf8(text)
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|n| *n <= max)
            .ok_or_else(|| format!("Protocol error: invalid {what} length"))
    }
    if buf.is_empty() {
        return Ok(None);
    }
    if buf[0] != b'*' {
        // An inline command: words on a line (telnet, health checks).
        let Some((text, next)) = line(buf, 0) else {
            return if buf.len() > 64 << 10 { Err("Protocol error: too big inline request".into()) } else { Ok(None) };
        };
        let args = text.split(|b| b.is_ascii_whitespace()).filter(|w| !w.is_empty()).map(<[u8]>::to_vec).collect();
        return Ok(Some((args, next)));
    }
    let Some((header, mut at)) = line(buf, 1) else { return Ok(None) };
    let n = count(header, max_args, "multibulk")?;
    let mut args = Vec::with_capacity(n.min(64));
    for _ in 0..n {
        if at >= buf.len() {
            return Ok(None);
        }
        if buf[at] != b'$' {
            return Err(format!("Protocol error: expected '$', got '{}'", buf[at] as char));
        }
        let Some((header, data)) = line(buf, at + 1) else { return Ok(None) };
        let len = count(header, max_bytes, "bulk")?;
        if buf.len() < data + len + 2 {
            return Ok(None);
        }
        args.push(buf[data..data + len].to_vec());
        at = data + len + 2;
    }
    Ok(Some((args, at)))
}

fn text(arg: &[u8]) -> String {
    String::from_utf8_lossy(arg).into_owned()
}

struct Session {
    state: Arc<State>,
    port: Arc<Port>,
    peer: String,
    app: Option<(Arc<App>, bool)>,
    /// Its place among read-only sessions, when its token only reads.
    read_only_place: Option<ReadOnlyPlace>,
    name: String,
    /// The client asked for RESP3 (`HELLO 3`).
    resp3: bool,
    /// The commands queued since `MULTI`.
    queued: Option<Queue>,
    /// What the command being read and the queue hold past [`FREE`].
    share: Share,
    /// The part of it for the command being read.
    buffer_held: usize,
}

/// The commands of a transaction, waiting for `EXEC`.
#[derive(Default)]
struct Queue {
    commands: Vec<Vec<Vec<u8>>>,
    /// What they hold, about.
    bytes: usize,
    /// One was refused: `EXEC` runs none.
    spoiled: bool,
}

impl Queue {
    /// Refuses the transaction: what it held is let go at once.
    fn spoil(&mut self) {
        *self = Queue { spoiled: true, ..Queue::default() };
    }
}

impl Session {
    fn auth(&mut self, token: &[u8]) -> Reply {
        let token = text(token);
        match self.state.apps.authenticate(&token, self.state.options.token.as_deref()) {
            Some((app, read_only)) => {
                let place = match (read_only, self.read_only_place.take()) {
                    (false, _) => None,
                    (true, Some(place)) => Some(place),
                    (true, None) => match ReadOnlyPlace::take(&self.port) {
                        Some(place) => Some(place),
                        None => {
                            return Reply::Error(format!(
                                "ERR max number of read-only clients reached: {} of the {} connections at most, so that read-write clients always find a place",
                                self.port.most.div_ceil(2),
                                self.port.most
                            ));
                        }
                    },
                };
                self.read_only_place = place;
                app.touch();
                app.connected("Key-value port", &self.name, &self.peer, read_only);
                self.app = Some((app, read_only));
                Reply::Ok
            }
            None => Reply::Error(
                "WRONGPASS this token opens no application: use a token of an application as the password".into(),
            ),
        }
    }

    /// Answers one command; `false` when the connection ends after it.
    async fn answer(&mut self, args: Vec<Vec<u8>>, out: &mut Vec<u8>) -> bool {
        let name = text(&args[0]).to_ascii_uppercase();
        let reply = match (name.as_str(), &args[1..]) {
            ("QUIT", _) => {
                Reply::Ok.write_resp(out, self.resp3);
                return false;
            }
            ("AUTH", [token] | [_, token]) => self.auth(token),
            ("AUTH", _) => Reply::Error("ERR wrong number of arguments for 'auth' command".into()),
            ("HELLO", rest) => self.hello(rest),
            (_, _) if self.app.is_none() => Reply::Error(
                "NOAUTH Authentication required: AUTH <token of an application> (redis://default:<token>@host:port)"
                    .into(),
            ),
            ("MULTI", []) if self.queued.is_some() => Reply::Error("ERR MULTI calls can not be nested".into()),
            ("MULTI", []) => {
                self.queued = Some(Queue::default());
                Reply::Ok
            }
            ("DISCARD", []) => match self.queued.take() {
                Some(_) => Reply::Ok,
                None => Reply::Error("ERR DISCARD without MULTI".into()),
            },
            ("EXEC", []) => match self.queued.take() {
                None => Reply::Error("ERR EXEC without MULTI".into()),
                Some(Queue { spoiled: true, .. }) => {
                    Reply::Error("EXECABORT Transaction discarded because of previous errors.".into())
                }
                Some(Queue { commands, .. }) => {
                    let started = Instant::now();
                    let (app, read_only) = self.app.as_ref().expect("checked above");
                    let read_only = *read_only;
                    let replies = on(app, move |kv| kv.transaction(&commands, read_only)).await.and_then(|r| r);
                    app.touch();
                    app.metrics.record(started.elapsed(), replies.is_err());
                    self.state.metrics.record(started.elapsed(), replies.is_err());
                    match replies {
                        Ok(replies) => Reply::Array(replies),
                        Err(e) => Reply::Error(format!("ERR storage: {e}")),
                    }
                }
            },
            // Nothing is watched: a transaction runs alone, there is no race to detect.
            ("WATCH", [_, ..]) | ("UNWATCH", []) => Reply::Ok,
            // Between MULTI and EXEC, commands wait; one a read-only token may not send spoils the lot,
            // and so does one past what a transaction may hold.
            (_, _) if self.queued.is_some() => {
                let (_, read_only) = self.app.as_ref().expect("checked above");
                let refused = *read_only && crate::kv::writes(&name);
                let most = self.state.options.max_body_bytes;
                let queue = self.queued.as_mut().expect("checked above");
                let bytes = args.iter().map(|a| a.len() + QUEUED_ARG_COST).sum::<usize>();
                if refused {
                    queue.spoil();
                    Reply::Error("READONLY this token only reads: use a read-write token".into())
                } else if queue.spoiled {
                    Reply::Status("QUEUED".into())
                } else if queue.commands.len() >= QUEUE_COMMANDS || queue.bytes + bytes > most {
                    queue.spoil();
                    Reply::Error(format!(
                        "ERR this transaction is too large: it may queue {QUEUE_COMMANDS} commands and {} MiB, EXEC will discard it",
                        most >> 20
                    ))
                } else if !self.share.hold(held(queue.bytes + bytes, self.buffer_held)) {
                    queue.spoil();
                    Reply::Error(format!(
                        "ERR the server holds many large commands and transactions at once ({} MiB in all): EXEC will discard this one, try again in a moment",
                        self.share.total() >> 20
                    ))
                } else {
                    queue.bytes += bytes;
                    queue.commands.push(args);
                    Reply::Status("QUEUED".into())
                }
            }
            ("SELECT", [db]) if db.as_slice() == b"0" => Reply::Ok,
            ("SELECT", _) => Reply::Error(
                "ERR DB index is out of range: an application has one key-value store, its token picks it".into(),
            ),
            ("CLIENT", [sub, rest @ ..]) => match (text(sub).to_ascii_uppercase().as_str(), rest) {
                ("SETNAME", [name]) => {
                    self.name = text(name);
                    Reply::Ok
                }
                ("GETNAME", []) if self.name.is_empty() => Reply::Nil,
                ("GETNAME", []) => Reply::Bulk(self.name.clone().into_bytes()),
                ("ID", []) => Reply::Int(1),
                _ => Reply::Ok,
            },
            // What clients ask at startup to learn the commands: none to describe.
            ("COMMAND", _) => Reply::Array(Vec::new()),
            _ => {
                let started = Instant::now();
                let (app, read_only) = self.app.as_ref().expect("checked above");
                let read_only = *read_only;
                let reply = on(app, move |kv| kv.command(&args, read_only)).await.and_then(|r| r);
                app.touch();
                app.metrics.record(started.elapsed(), reply.is_err());
                self.state.metrics.record(started.elapsed(), reply.is_err());
                reply.unwrap_or_else(|e| Reply::Error(format!("ERR storage: {e}")))
            }
        };
        reply.write_resp(out, self.resp3);
        true
    }

    /// `HELLO [2|3 [AUTH user token] [SETNAME name]]`.
    fn hello(&mut self, args: &[Vec<u8>]) -> Reply {
        let resp3 = match args.first().map(Vec::as_slice) {
            None => self.resp3,
            Some(b"2") => false,
            Some(b"3") => true,
            Some(_) => {
                return Reply::Error(
                    "NOPROTO sorry, this protocol version is not supported: Nuage DB speaks RESP2 and RESP3".into(),
                );
            }
        };
        let mut rest = args.get(1..).unwrap_or_default();
        while let Some(option) = rest.first() {
            match (text(option).to_ascii_uppercase().as_str(), rest) {
                ("AUTH", [_, _, token, ..]) => {
                    if let Reply::Error(e) = self.auth(token) {
                        return Reply::Error(e);
                    }
                    rest = &rest[3..];
                }
                ("SETNAME", [_, name, ..]) => {
                    self.name = text(name);
                    rest = &rest[2..];
                }
                _ => return Reply::Error("ERR Syntax error in HELLO option".into()),
            }
        }
        if self.app.is_none() {
            return Reply::Error(
                "NOAUTH HELLO must be called with the client already authenticated, otherwise the HELLO <proto> AUTH \
                 <user> <pass> option can be used to authenticate the client and select the RESP protocol version at \
                 the same time"
                    .into(),
            );
        }
        self.resp3 = resp3;
        let bulk = |s: &str| Reply::Bulk(s.as_bytes().to_vec());
        Reply::Map(vec![
            (bulk("server"), bulk("redis")),
            (bulk("version"), bulk("7.0.0")),
            (bulk("proto"), Reply::Int(if resp3 { 3 } else { 2 })),
            (bulk("id"), Reply::Int(1)),
            (bulk("mode"), bulk("standalone")),
            (bulk("role"), bulk("master")),
            (bulk("modules"), Reply::Array(Vec::new())),
        ])
    }
}

/// What a connection holds past [`FREE`]: its queue's bytes past it, and
/// `buffer` for the command being read.
fn held(queue: usize, buffer: usize) -> usize {
    queue.saturating_sub(FREE) + buffer
}

impl Session {
    /// Holds what the queue and the command being read take now (less once
    /// a transaction ended or a large command was answered).
    fn settle(&mut self) {
        let queue = self.queued.as_ref().map_or(0, |q| q.bytes);
        self.share.hold(held(queue, self.buffer_held));
    }
}

/// Writes `out` and empties it; `false` when the client is gone, or did not
/// take it in [`STALLED`] and a second more for each [`COMMAND_PACE`] bytes
/// of it (however little it takes at a time), or (before login) when its
/// time to log in is over.
async fn send<S: AsyncWrite + Unpin>(stream: &mut S, out: &mut Vec<u8>, by: Option<tokio::time::Instant>) -> bool {
    let paced = tokio::time::Instant::now() + STALLED + Duration::from_secs((out.len() / COMMAND_PACE) as u64);
    let deadline = by.map_or(paced, |by| by.min(paced));
    let mut at = 0;
    while at < out.len() {
        match tokio::time::timeout_at(deadline, stream.write(&out[at..])).await {
            Ok(Ok(n)) if n > 0 => at += n,
            _ => return false,
        }
    }
    out.clear();
    // The room a large answer took is not kept by an idle connection.
    if out.capacity() > 2 * SEND_AT {
        *out = Vec::with_capacity(4096);
    }
    matches!(tokio::time::timeout_at(deadline, stream.flush()).await, Ok(Ok(())))
}

/// Closes a connection whose client was told why: what it still sends is
/// read and let go for a moment, so that the reason reaches it (a socket
/// closed with unread data is reset, and the answer may be lost with it).
async fn close<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S) {
    let _ = tokio::time::timeout(LINGER, stream.shutdown()).await;
    let mut sink = [0u8; 4096];
    let mut drained = 0;
    let until = tokio::time::Instant::now() + LINGER;
    while drained < LINGER_BYTES {
        match tokio::time::timeout_at(until, stream.read(&mut sink)).await {
            Ok(Ok(n)) if n > 0 => drained += n as u64,
            _ => return,
        }
    }
}

async fn session<S: AsyncRead + AsyncWrite + Unpin>(
    state: Arc<State>,
    mut stream: S,
    peer: SocketAddr,
    login_by: tokio::time::Instant,
    port: Arc<Port>,
) {
    let max = state.options.max_body_bytes;
    let share = Share::new(&port.memory);
    let mut session = Session {
        state,
        port,
        peer: peer.ip().to_string(),
        app: None,
        read_only_place: None,
        name: String::new(),
        resp3: false,
        queued: None,
        share,
        buffer_held: 0,
    };
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut out: Vec<u8> = Vec::with_capacity(4096);
    // Commands answered before login.
    let mut before_login = 0;
    // When the command still coming started to.
    let mut started: Option<tokio::time::Instant> = None;
    loop {
        // Every command already there, then one write: a pipeline costs one
        // round trip. A long one is answered as it goes, so that its answers
        // are not all held at once.
        let mut at = 0;
        let mut open = true;
        while open {
            let by = session.app.is_none().then_some(login_by);
            let (max_args, max_bytes) = if by.is_none() { (MAX_ARGS, max) } else { (LOGIN_ARGS, LOGIN_ARG_BYTES) };
            match parse(&buf[at..], max_args, max_bytes) {
                Ok(Some((args, used))) => {
                    at += used;
                    if !args.is_empty() {
                        open = session.answer(args, &mut out).await;
                        // Each refused command has an answer: a client that
                        // does not log in gets a few, not a stream of them.
                        if session.app.is_none() {
                            before_login += 1;
                            if open && before_login >= LOGIN_COMMANDS {
                                Reply::Error(format!(
                                    "ERR {LOGIN_COMMANDS} commands without AUTH: the connection is closed"
                                ))
                                .write_resp(&mut out, session.resp3);
                                open = false;
                            }
                        }
                    }
                    if out.len() >= SEND_AT && !send(&mut stream, &mut out, by).await {
                        return;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    Reply::Error(format!("ERR {e}")).write_resp(&mut out, session.resp3);
                    open = false;
                }
            }
        }
        buf.drain(..at);
        let by = session.app.is_none().then_some(login_by);
        if !out.is_empty() && !send(&mut stream, &mut out, by).await {
            return;
        }
        // A command larger than a request body may be: refused. Before login,
        // a few KiB at most.
        let most = if by.is_none() { max + (64 << 10) } else { LOGIN_BUFFER };
        if open && buf.len() > most {
            Reply::Error("ERR Protocol error: this command is longer than the server reads".into())
                .write_resp(&mut out, session.resp3);
            let _ = send(&mut stream, &mut out, by).await;
            open = false;
        }
        if !open {
            close(&mut stream).await;
            return;
        }
        // The room a large command took is not kept by an idle connection.
        if buf.capacity() > 64 << 10 && buf.capacity() > 4 * buf.len() {
            buf.shrink_to(buf.len().max(4096));
        }
        // Room to read into, doubling as a command grows: past what is free,
        // taken from the port's memory first.
        if buf.capacity() - buf.len() < 4096 {
            let room = (buf.capacity() * 2).min(most + 4096).max(buf.len() + 4096);
            session.buffer_held = room.saturating_sub(FREE);
            let queue = session.queued.as_ref().map_or(0, |q| q.bytes);
            if !session.share.hold_soon(held(queue, session.buffer_held)).await {
                Reply::Error(format!(
                    "ERR the server holds many large commands and transactions at once ({} MiB in all): try again in a moment",
                    session.share.total() >> 20
                ))
                .write_resp(&mut out, session.resp3);
                let _ = send(&mut stream, &mut out, by).await;
                close(&mut stream).await;
                return;
            }
            buf.reserve_exact(room - buf.len());
        } else {
            session.buffer_held = buf.capacity().saturating_sub(FREE);
        }
        session.settle();
        // A command that started must come whole at a pace; one that ended
        // (or none started) has no clock.
        if at > 0 || buf.is_empty() {
            started = None;
        }
        if !buf.is_empty() && started.is_none() {
            started = Some(tokio::time::Instant::now());
        }
        let pace = Duration::from_secs((buf.len() / COMMAND_PACE) as u64);
        let read = stream.read_buf(&mut buf);
        let read = if let Some(by) = by {
            // Not logged in yet: the time to do it is counted from the connection.
            match tokio::time::timeout_at(by, read).await {
                Ok(read) => read,
                Err(_) => return too_late(&mut stream).await,
            }
        } else if let Some(since) = started {
            // Part of a command: the rest is expected to come, a byte within
            // a minute, all of it at a pace.
            let by = (since + STALLED + pace).min(tokio::time::Instant::now() + STALLED);
            match tokio::time::timeout_at(by, read).await {
                Ok(read) => read,
                Err(_) => return,
            }
        } else {
            // Idle between commands: a connection of a pool may stay so for hours.
            read.await
        };
        match read {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

/// Tells a client that did not log in in time why its connection closes.
async fn too_late<S: AsyncWrite + Unpin>(stream: &mut S) {
    let mut late = Vec::new();
    Reply::Error(format!("ERR no AUTH within {} seconds: the connection is closed", LOGIN_TIME.as_secs()))
        .write_resp(&mut late, false);
    let _ = tokio::time::timeout(Duration::from_secs(1), stream.write_all(&late)).await;
}

/// Counts a connection for as long as it lives, a panic included.
struct Open<'a>(&'a AtomicU64);

impl Drop for Open<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Serves one connection until the client leaves; `place` is given back then.
pub(crate) async fn serve(state: Arc<State>, stream: TcpStream, peer: SocketAddr, options: KvOptions, place: Place) {
    let port = place.0.clone();
    let _place = place;
    let login_by = tokio::time::Instant::now() + LOGIN_TIME;
    state.metrics.open_connections.fetch_add(1, Ordering::Relaxed);
    let _open = Open(&state.metrics.open_connections);
    let _ = stream.set_nodelay(true);
    super::net::keep_alive(&stream);
    // A TLS handshake starts with the byte 0x16; a command never does.
    let mut first = [0u8; 1];
    let mut stream = stream;
    let tls = match tokio::time::timeout_at(login_by, stream.peek(&mut first)).await {
        Ok(peeked) => matches!(peeked, Ok(1) if first[0] == 0x16),
        // Not a byte in the time to log in.
        Err(_) => return too_late(&mut stream).await,
    };
    match (tls, &options.tls) {
        (true, Some(config)) => {
            let accept = tokio_rustls::TlsAcceptor::from(config.clone()).accept(stream);
            if let Ok(Ok(stream)) = tokio::time::timeout_at(login_by, accept).await {
                session(state.clone(), stream, peer, login_by, port).await;
            }
        }
        (true, None) => {}
        (false, _) => session(state.clone(), stream, peer, login_by, port).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_on_the_wire() {
        let parse = |buf: &[u8], max: usize| super::parse(buf, MAX_ARGS, max);
        let max = 1 << 20;
        let whole = b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$5\r\nh\r\nlo\r\n*1\r\n$4\r\nPING\r\n";
        let (args, used) = parse(whole, max).unwrap().unwrap();
        assert_eq!(args, [&b"SET"[..], b"k", b"h\r\nlo"]);
        assert_eq!(&whole[used..], b"*1\r\n$4\r\nPING\r\n");
        // Cut anywhere, it waits for the rest.
        for cut in 0..used {
            assert_eq!(parse(&whole[..cut], max), Ok(None), "cut at {cut}");
        }
        assert_eq!(parse(b"PING\r\n", max), Ok(Some((vec![b"PING".to_vec()], 6))));
        assert_eq!(parse(b"  get   k \nrest", max), Ok(Some((vec![b"get".to_vec(), b"k".to_vec()], 11))));
        assert_eq!(parse(b"\r\n", max), Ok(Some((vec![], 2))));
        assert_eq!(parse(b"*0\r\n", max), Ok(Some((vec![], 4))));
        assert!(parse(b"*1\r\n:1\r\n", max).is_err());
        assert!(parse(b"*x\r\n", max).is_err());
        assert!(parse(b"*1\r\n$99999999\r\n", 1024).is_err(), "larger than a value may be");
    }

    /// Before login a command is small: its length is refused as soon as it
    /// is announced, before any of it is held.
    #[test]
    fn small_commands_before_login() {
        let login = |buf: &[u8]| parse(buf, LOGIN_ARGS, LOGIN_ARG_BYTES);
        let auth = b"*7\r\n$5\r\nHELLO\r\n$1\r\n3\r\n$4\r\nAUTH\r\n$7\r\ndefault\r\n$40\r\ntttttttttttttttttttttttttttttttttttttttt\r\n$7\r\nSETNAME\r\n$1\r\nx\r\n";
        assert_eq!(login(auth).unwrap().unwrap().0.len(), 7);
        assert!(login(b"*2\r\n$4\r\nECHO\r\n$16777216\r\n").is_err(), "a 16 MiB argument");
        assert!(login(b"*17\r\n").is_err(), "17 arguments");
        assert!(parse(b"*2\r\n$4\r\nECHO\r\n$16777216\r\n", MAX_ARGS, 16 << 20).unwrap().is_none(), "logged in");
    }

    /// With the keys in the database, a write the disk refuses answers
    /// `-ERR storage: ...` on the port and the server's storage error over
    /// HTTP, never a missing key; what it wrote is never read.
    #[tokio::test]
    async fn a_write_the_disk_refuses_is_an_error_on_the_port_and_over_http() {
        let dir = std::env::temp_dir().join(format!("nuage-kv-port-refused-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let options = crate::EngineOptions { kv_in_database: true, ..crate::EngineOptions::default() };
        let engine = crate::Engine::open(&dir, options).unwrap();
        let server = crate::Server::new(
            engine.clone(),
            crate::ServerOptions { token: Some("t0ken".into()), ..crate::ServerOptions::default() },
        );
        let port = server.listen_kv("127.0.0.1:0", KvOptions::default()).await.unwrap();
        let web = server.listen("127.0.0.1:0").await.unwrap();
        let mut conn = TcpStream::connect(port.local_addr()).await.unwrap();
        async fn say(conn: &mut TcpStream, command: &[&str]) -> String {
            let mut wire = format!("*{}\r\n", command.len());
            for arg in command {
                wire.push_str(&format!("${}\r\n{arg}\r\n", arg.len()));
            }
            conn.write_all(wire.as_bytes()).await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = conn.read(&mut buf).await.unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        }
        async fn request(addr: SocketAddr, method: &str, path: &str, body: &str) -> String {
            let mut conn = TcpStream::connect(addr).await.unwrap();
            let head = format!(
                "{method} {path} HTTP/1.1\r\nHost: nuage\r\nAuthorization: Bearer t0ken\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
            conn.write_all(head.as_bytes()).await.unwrap();
            let mut answer = Vec::new();
            conn.read_to_end(&mut answer).await.unwrap();
            String::from_utf8_lossy(&answer).into_owned()
        }
        assert_eq!(say(&mut conn, &["AUTH", "t0ken"]).await, "+OK\r\n");
        assert_eq!(say(&mut conn, &["SET", "kept", "v"]).await, "+OK\r\n");
        engine.store.fail_next_sync();
        let refused = say(&mut conn, &["SET", "lost", "v"]).await;
        assert!(refused.starts_with("-ERR storage: ") && refused.contains("write-ahead log"), "{refused}");
        assert_eq!(say(&mut conn, &["GET", "lost"]).await, "$-1\r\n");
        let multi = [say(&mut conn, &["MULTI"]).await, say(&mut conn, &["INCR", "n"]).await];
        assert_eq!(multi, ["+OK\r\n", "+QUEUED\r\n"]);
        assert!(say(&mut conn, &["EXEC"]).await.starts_with("-ERR storage: "));
        // Over HTTP, the server's storage error; reads go on.
        let put = request(web.local_addr(), "PUT", "/v1/kv/other", "v").await;
        assert!(put.starts_with("HTTP/1.1 500") && put.contains("write-ahead log"), "{put}");
        let get = request(web.local_addr(), "GET", "/v1/kv/kept", "").await;
        assert!(get.starts_with("HTTP/1.1 200") && get.ends_with("\r\n\r\nv"), "{get}");
        assert!(request(web.local_addr(), "GET", "/v1/kv/lost", "").await.starts_with("HTTP/1.1 404"));
        port.shutdown().await;
        web.shutdown().await;
        drop((server, engine));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
