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

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

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

/// A command read from the wire: its arguments, and how many bytes it took.
pub(crate) type Parsed = Option<(Vec<Vec<u8>>, usize)>;

/// One command at the start of `buf`; `None` when it is not all there yet.
pub(crate) fn parse(buf: &[u8], max_bytes: usize) -> Result<Parsed, String> {
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
    let n = count(header, MAX_ARGS, "multibulk")?;
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
    /// The commands queued since `MULTI`, and whether one was refused.
    queued: Option<(Vec<Vec<Vec<u8>>>, bool)>,
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
                self.queued = Some((Vec::new(), false));
                Reply::Ok
            }
            ("DISCARD", []) => match self.queued.take() {
                Some(_) => Reply::Ok,
                None => Reply::Error("ERR DISCARD without MULTI".into()),
            },
            ("EXEC", []) => match self.queued.take() {
                None => Reply::Error("ERR EXEC without MULTI".into()),
                Some((_, true)) => Reply::Error("EXECABORT Transaction discarded because of previous errors.".into()),
                Some((commands, false)) => {
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
            // Between MULTI and EXEC, commands wait; one a read-only token may not send spoils the lot.
            (_, _) if self.queued.is_some() => {
                let (_, read_only) = self.app.as_ref().expect("checked above");
                let refused = *read_only && crate::kv::writes(&name);
                let (commands, spoiled) = self.queued.as_mut().expect("checked above");
                if refused {
                    *spoiled = true;
                    Reply::Error("READONLY this token only reads: use a read-write token".into())
                } else {
                    commands.push(args.to_vec());
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

async fn session<S: AsyncRead + AsyncWrite + Unpin>(state: Arc<State>, mut stream: S, peer: SocketAddr) {
    let max = state.options.max_body_bytes;
    let mut session =
        Session { state, peer: peer.ip().to_string(), app: None, name: String::new(), resp3: false, queued: None };
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut out: Vec<u8> = Vec::with_capacity(4096);
    loop {
        // Every command already there, then one write: a pipeline costs one
        // round trip.
        let mut at = 0;
        let mut open = true;
        while open {
            match parse(&buf[at..], max) {
                Ok(Some((args, used))) => {
                    at += used;
                    if !args.is_empty() {
                        open = session.answer(&args, &mut out);
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
        if !out.is_empty() {
            if stream.write_all(&out).await.is_err() {
                return;
            }
            out.clear();
        }
        // A command larger than a request body may be: refused.
        if !open || buf.len() > max + (64 << 10) {
            let _ = stream.shutdown().await;
            return;
        }
        match stream.read_buf(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

/// Serves one connection until the client leaves.
pub(crate) async fn serve(state: Arc<State>, stream: TcpStream, peer: SocketAddr, options: KvOptions) {
    let _ = stream.set_nodelay(true);
    state.metrics.open_connections.fetch_add(1, Ordering::Relaxed);
    // A TLS handshake starts with the byte 0x16; a command never does.
    let mut first = [0u8; 1];
    let tls = matches!(stream.peek(&mut first).await, Ok(1) if first[0] == 0x16);
    match (tls, &options.tls) {
        (true, Some(config)) => {
            if let Ok(stream) = tokio_rustls::TlsAcceptor::from(config.clone()).accept(stream).await {
                session(state.clone(), stream, peer).await;
            }
        }
        (true, None) => {}
        (false, _) => session(state.clone(), stream, peer).await,
    }
    state.metrics.open_connections.fetch_sub(1, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_on_the_wire() {
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
}
