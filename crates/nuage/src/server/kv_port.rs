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
//! A client has 15 seconds to log in, with small commands; the port serves
//! [`ServerOptions::kv_max_connections`](super::ServerOptions::kv_max_connections)
//! connections at once, and a transaction (`MULTI`) queues what a request
//! body may hold.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

use super::State;
use super::apps::App;
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
/// Answers held before they are sent, in a long pipeline.
const SEND_AT: usize = 1 << 20;
/// A client that takes nothing of its answers this long, or sends nothing
/// more of a command it started, is gone: its connection is closed.
const STALLED: Duration = Duration::from_secs(60);
/// Commands a transaction (`MULTI`) may queue. What they hold together is
/// bounded too, by the largest request body.
const QUEUE_COMMANDS: usize = 100_000;
/// What a queued argument costs besides its bytes.
const QUEUED_ARG_COST: usize = 32;

/// The answer to a connection past [`ServerOptions::kv_max_connections`](super::ServerOptions).
pub(super) const TOO_MANY_CLIENTS: &[u8] = b"-ERR max number of clients reached\r\n";

/// A connection's place among those the listener serves at once: given back
/// when the connection ends, however it ends.
pub(super) struct Place(Arc<AtomicUsize>);

impl Place {
    pub(super) fn take(open: &Arc<AtomicUsize>) -> Place {
        open.fetch_add(1, Ordering::Relaxed);
        Place(open.clone())
    }
}

impl Drop for Place {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
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
    peer: String,
    app: Option<(Arc<App>, bool)>,
    name: String,
    /// The client asked for RESP3 (`HELLO 3`).
    resp3: bool,
    /// The commands queued since `MULTI`.
    queued: Option<Queue>,
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
    fn answer(&mut self, args: &[Vec<u8>], out: &mut Vec<u8>) -> bool {
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
                    let replies = app.kv.transaction(&commands, *read_only);
                    app.touch();
                    app.metrics.record(started.elapsed(), false);
                    self.state.metrics.record(started.elapsed(), false);
                    Reply::Array(replies)
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
                } else {
                    queue.bytes += bytes;
                    queue.commands.push(args.to_vec());
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
                let reply = app.kv.command(args, *read_only);
                app.touch();
                app.metrics.record(started.elapsed(), false);
                self.state.metrics.record(started.elapsed(), false);
                reply
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

/// Writes `out` and empties it; `false` when the client is gone, or took
/// nothing of it for [`STALLED`].
async fn send<S: AsyncWrite + Unpin>(stream: &mut S, out: &mut Vec<u8>) -> bool {
    let mut at = 0;
    while at < out.len() {
        match tokio::time::timeout(STALLED, stream.write(&out[at..])).await {
            Ok(Ok(n)) if n > 0 => at += n,
            _ => return false,
        }
    }
    out.clear();
    // The room a large answer took is not kept by an idle connection.
    if out.capacity() > 2 * SEND_AT {
        *out = Vec::with_capacity(4096);
    }
    matches!(tokio::time::timeout(STALLED, stream.flush()).await, Ok(Ok(())))
}

async fn session<S: AsyncRead + AsyncWrite + Unpin>(
    state: Arc<State>,
    mut stream: S,
    peer: SocketAddr,
    login_by: tokio::time::Instant,
) {
    let max = state.options.max_body_bytes;
    let mut session =
        Session { state, peer: peer.ip().to_string(), app: None, name: String::new(), resp3: false, queued: None };
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut out: Vec<u8> = Vec::with_capacity(4096);
    loop {
        // Every command already there, then one write: a pipeline costs one
        // round trip. A long one is answered as it goes, so that its answers
        // are not all held at once.
        let mut at = 0;
        let mut open = true;
        while open {
            let (max_args, max_bytes) =
                if session.app.is_some() { (MAX_ARGS, max) } else { (LOGIN_ARGS, LOGIN_ARG_BYTES) };
            match parse(&buf[at..], max_args, max_bytes) {
                Ok(Some((args, used))) => {
                    at += used;
                    if !args.is_empty() {
                        open = session.answer(&args, &mut out);
                    }
                    if out.len() >= SEND_AT && !send(&mut stream, &mut out).await {
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
        if !out.is_empty() && !send(&mut stream, &mut out).await {
            return;
        }
        // A command larger than a request body may be: refused. Before login,
        // a few KiB at most.
        let most = if session.app.is_some() { max + (64 << 10) } else { LOGIN_BUFFER };
        if open && buf.len() > most {
            Reply::Error("ERR Protocol error: this command is longer than the server reads".into())
                .write_resp(&mut out, session.resp3);
            let _ = send(&mut stream, &mut out).await;
            open = false;
        }
        if !open {
            let _ = stream.shutdown().await;
            return;
        }
        // The room a large command took is not kept by an idle connection.
        if buf.capacity() > 64 << 10 && buf.capacity() > 4 * buf.len() {
            buf.shrink_to(buf.len().max(4096));
        }
        let partial = !buf.is_empty();
        let read = stream.read_buf(&mut buf);
        let read = if session.app.is_none() {
            // Not logged in yet: the time to do it is counted from the connection.
            match tokio::time::timeout_at(login_by, read).await {
                Ok(read) => read,
                Err(_) => return too_late(&mut stream).await,
            }
        } else if partial {
            // Half a command: the rest is expected to come.
            match tokio::time::timeout(STALLED, read).await {
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
                session(state.clone(), stream, peer, login_by).await;
            }
        }
        (true, None) => {}
        (false, _) => session(state.clone(), stream, peer, login_by).await,
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
}
