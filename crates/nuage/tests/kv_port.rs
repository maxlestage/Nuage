//! The key-value port with clients that misbehave or go quiet: what they may
//! hold, and for how long.

#![cfg(feature = "server")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nuage::{Engine, EngineOptions, KvOptions, Server, ServerOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

async fn start(name: &str, options: ServerOptions) -> (Server, SocketAddr) {
    let dir = std::env::temp_dir().join(format!("nuage-kv-port-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let engine: Arc<Engine> = Engine::open(&dir, EngineOptions { sync: false, ..EngineOptions::default() }).unwrap();
    let server = Server::new(engine, ServerOptions { token: Some("t0ken".into()), ..options });
    let running = server.listen_kv("127.0.0.1:0", KvOptions::default()).await.unwrap();
    let addr = running.local_addr();
    std::mem::forget(running);
    (server, addr)
}

/// A command as clients write it.
fn command(args: &[&[u8]]) -> Vec<u8> {
    let mut wire = format!("*{}\r\n", args.len()).into_bytes();
    for arg in args {
        wire.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        wire.extend_from_slice(arg);
        wire.extend_from_slice(b"\r\n");
    }
    wire
}

/// What the server sent within `within`, until it closed or `until` came.
async fn answer(conn: &mut TcpStream, until: &str, within: Duration) -> (String, bool) {
    let deadline = tokio::time::Instant::now() + within;
    let mut got = Vec::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        if String::from_utf8_lossy(&got).contains(until) {
            return (String::from_utf8_lossy(&got).into_owned(), false);
        }
        match tokio::time::timeout_at(deadline, conn.read(&mut buf)).await {
            Ok(Ok(0) | Err(_)) => return (String::from_utf8_lossy(&got).into_owned(), true),
            Ok(Ok(n)) => got.extend_from_slice(&buf[..n]),
            Err(_) => return (String::from_utf8_lossy(&got).into_owned(), false),
        }
    }
}

async fn say(conn: &mut TcpStream, args: &[&[u8]]) -> String {
    conn.write_all(&command(args)).await.unwrap();
    answer(conn, "\r\n", Duration::from_secs(5)).await.0
}

/// Before login a command is small and a client has a few seconds to log in;
/// once logged in, it may stay idle.
#[tokio::test]
async fn a_client_nobody_knows_holds_little_and_not_for_long() {
    let (_server, addr) = start("login", ServerOptions::default()).await;

    // A 16 MiB argument announced before AUTH: refused at its length.
    let mut big = TcpStream::connect(addr).await.unwrap();
    big.write_all(b"*2\r\n$4\r\nECHO\r\n$16777216\r\n").await.unwrap();
    let _ = big.write_all(&vec![b'x'; 256 << 10]).await;
    let (said, closed) = answer(&mut big, "\u{0}", Duration::from_secs(3)).await;
    assert!(closed && said.starts_with("-ERR Protocol error"), "{said:?}, closed: {closed}");

    // An inline line that never ends, before AUTH.
    let mut long = TcpStream::connect(addr).await.unwrap();
    let _ = long.write_all(&vec![b'a'; 64 << 10]).await;
    let (said, closed) = answer(&mut long, "\u{0}", Duration::from_secs(3)).await;
    assert!(closed && said.starts_with("-ERR"), "{said:?}, closed: {closed}");

    // One that says nothing, one that logs in and waits.
    let mut silent = TcpStream::connect(addr).await.unwrap();
    let mut idle = TcpStream::connect(addr).await.unwrap();
    assert_eq!(say(&mut idle, &[b"AUTH", b"t0ken"]).await, "+OK\r\n");
    let started = Instant::now();
    let (said, closed) = answer(&mut silent, "\u{0}", Duration::from_secs(25)).await;
    assert!(closed, "a client that does not log in keeps its connection");
    assert!(said.starts_with("-ERR no AUTH within"), "{said:?}");
    assert!(started.elapsed() > Duration::from_secs(5), "closed after the time to log in, not before");
    assert_eq!(say(&mut idle, &[b"PING"]).await, "+PONG\r\n", "a logged-in client stays");
}

/// Past the limit a connection is told so, as Redis tells it, and closed; a
/// place freed is a place to take.
#[tokio::test]
async fn connections_past_the_limit_are_told_so() {
    let (_server, addr) = start("limit", ServerOptions { kv_max_connections: 2, ..ServerOptions::default() }).await;
    let mut first = TcpStream::connect(addr).await.unwrap();
    let mut second = TcpStream::connect(addr).await.unwrap();
    assert_eq!(say(&mut first, &[b"AUTH", b"t0ken"]).await, "+OK\r\n");
    assert_eq!(say(&mut second, &[b"PING"]).await.get(..7), Some("-NOAUTH"));
    let mut third = TcpStream::connect(addr).await.unwrap();
    let (said, closed) = answer(&mut third, "\u{0}", Duration::from_secs(3)).await;
    assert!(closed && said == "-ERR max number of clients reached\r\n", "{said:?}");
    assert_eq!(say(&mut first, &[b"PING"]).await, "+PONG\r\n", "the others go on");
    drop(second);
    let mut again = None;
    for _ in 0..50 {
        let mut conn = TcpStream::connect(addr).await.unwrap();
        if say(&mut conn, &[b"PING"]).await.starts_with("-NOAUTH") {
            again = Some(conn);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(again.is_some(), "a closed connection frees its place");
}

/// A transaction queues what a request body may hold, no more: past it, it
/// is refused and `EXEC` discards it; a read-only token's queue too.
#[tokio::test]
async fn a_transaction_holds_what_a_request_may() {
    let (server, addr) = start("multi", ServerOptions { max_body_bytes: 256 << 10, ..ServerOptions::default() }).await;
    let read_only = server.new_app_read_only_token("nuage").unwrap();
    let mut conn = TcpStream::connect(addr).await.unwrap();
    assert_eq!(say(&mut conn, &[b"AUTH", read_only.as_bytes()]).await, "+OK\r\n");
    assert_eq!(say(&mut conn, &[b"MULTI"]).await, "+OK\r\n");
    let key = vec![b'k'; 100 << 10];
    assert_eq!(say(&mut conn, &[b"GET", &key]).await, "+QUEUED\r\n");
    assert_eq!(say(&mut conn, &[b"GET", &key]).await, "+QUEUED\r\n");
    let refused = say(&mut conn, &[b"GET", &key]).await;
    assert!(refused.starts_with("-ERR this transaction is too large"), "{refused}");
    assert_eq!(say(&mut conn, &[b"GET", b"small"]).await, "+QUEUED\r\n");
    assert!(say(&mut conn, &[b"EXEC"]).await.starts_with("-EXECABORT"));
    // The next one starts afresh.
    assert_eq!(say(&mut conn, &[b"MULTI"]).await, "+OK\r\n");
    assert_eq!(say(&mut conn, &[b"GET", b"small"]).await, "+QUEUED\r\n");
    assert_eq!(say(&mut conn, &[b"EXEC"]).await, "*1\r\n$-1\r\n");
}

/// A long pipeline is answered as it goes, every answer in order.
#[tokio::test]
async fn a_long_pipeline_is_answered_in_order() {
    let (_server, addr) = start("pipeline", ServerOptions::default()).await;
    let mut conn = TcpStream::connect(addr).await.unwrap();
    assert_eq!(say(&mut conn, &[b"AUTH", b"t0ken"]).await, "+OK\r\n");
    let value = vec![b'v'; 64 << 10];
    assert_eq!(say(&mut conn, &[b"SET", b"k", &value]).await, "+OK\r\n");
    let mut wire = Vec::new();
    for i in 0..100 {
        wire.extend(command(&[b"GET", b"k"]));
        wire.extend(command(&[b"ECHO", format!("end {i:03}").as_bytes()]));
    }
    conn.write_all(&wire).await.unwrap();
    let (said, closed) = answer(&mut conn, "end 099\r\n", Duration::from_secs(20)).await;
    assert!(!closed);
    let one = format!("$65536\r\n{}\r\n", "v".repeat(64 << 10));
    let expected: String = (0..100).map(|i| format!("{one}$7\r\nend {i:03}\r\n")).collect();
    assert_eq!(said.len(), expected.len());
    assert!(said == expected, "the answers came out of order");
}
