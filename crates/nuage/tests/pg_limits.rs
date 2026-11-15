//! The PostgreSQL port with clients that misbehave or go quiet: what they may
//! hold, and for how long; and what preparing a statement costs.

#![cfg(feature = "server")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nuage::{Engine, EngineOptions, PgOptions, Server, ServerOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_postgres::{Client, NoTls, error::SqlState};

async fn start(name: &str, options: ServerOptions, pg: PgOptions) -> (Server, SocketAddr) {
    let dir = std::env::temp_dir().join(format!("nuage-pg-limits-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let engine: Arc<Engine> = Engine::open(&dir, EngineOptions { sync: false, ..EngineOptions::default() }).unwrap();
    let server = Server::new(engine, ServerOptions { token: Some("t0ken".into()), ..options });
    let running = server.listen_pg("127.0.0.1:0", pg).await.unwrap();
    let addr = running.local_addr();
    std::mem::forget(running);
    (server, addr)
}

async fn connect(addr: SocketAddr) -> Result<Client, tokio_postgres::Error> {
    let (client, conn) = tokio_postgres::connect(
        &format!("host={} port={} user=anyone password=t0ken dbname=nuage", addr.ip(), addr.port()),
        NoTls,
    )
    .await?;
    tokio::spawn(conn);
    Ok(client)
}

/// A message of the protocol: its type, its length, its body.
fn msg(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut m = vec![kind];
    m.extend_from_slice(&(body.len() as u32 + 4).to_be_bytes());
    m.extend_from_slice(body);
    m
}

/// Connects and sends the startup packet, then reads the password request.
async fn startup(addr: SocketAddr) -> TcpStream {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut body = 196608i32.to_be_bytes().to_vec();
    body.extend_from_slice(b"user\0anyone\0database\0nuage\0\0");
    let mut packet = (body.len() as u32 + 4).to_be_bytes().to_vec();
    packet.extend_from_slice(&body);
    s.write_all(&packet).await.unwrap();
    let (kind, _) = read(&mut s).await.unwrap();
    assert_eq!(kind, b'R');
    s
}

/// The server's next message (`None`: the connection was closed).
async fn read(s: &mut TcpStream) -> Option<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    s.read_exact(&mut head).await.ok()?;
    let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
    let mut body = vec![0u8; len - 4];
    s.read_exact(&mut body).await.ok()?;
    Some((head[0], body))
}

/// Messages up to `ReadyForQuery`.
async fn until_ready(s: &mut TcpStream) -> Vec<(u8, Vec<u8>)> {
    let mut got = Vec::new();
    while let Some((kind, body)) = read(s).await {
        got.push((kind, body));
        if kind == b'Z' {
            break;
        }
    }
    got
}

/// The connection is closed by the server within `within`.
async fn closed_within(s: &mut TcpStream, within: Duration) -> bool {
    let mut buf = [0u8; 4096];
    let deadline = tokio::time::Instant::now() + within;
    loop {
        match tokio::time::timeout_at(deadline, s.read(&mut buf)).await {
            Err(_) => return false,
            Ok(Ok(0) | Err(_)) => return true,
            Ok(Ok(_)) => {}
        }
    }
}

/// Before its password is checked a client may send only a small message,
/// and has a few seconds to log in: a client nobody knows holds neither
/// memory nor a place for long.
#[tokio::test]
async fn a_client_nobody_knows_holds_little_and_not_for_long() {
    let options = ServerOptions { pg_max_connections: 2, ..ServerOptions::default() };
    let (_server, addr) = start("login", options, PgOptions::default()).await;

    // A password message announced as 64 MiB: refused at its length.
    let mut s = startup(addr).await;
    let mut big = vec![b'p'];
    big.extend_from_slice(&(64u32 << 20).to_be_bytes());
    big.extend_from_slice(&vec![b'x'; 256 << 10]);
    let _ = s.write_all(&big).await;
    assert!(closed_within(&mut s, Duration::from_secs(3)).await, "the server waits for 64 MiB");
    drop(s);

    // Two clients that say nothing take the two places...
    let mut silent = Vec::new();
    for _ in 0..2 {
        silent.push(TcpStream::connect(addr).await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let refused = connect(addr).await.expect_err("no place left");
    assert_eq!(refused.code(), Some(&SqlState::TOO_MANY_CONNECTIONS));
    // ... only for the time to log in.
    let started = Instant::now();
    for s in &mut silent {
        assert!(closed_within(s, Duration::from_secs(25)).await, "a silent client keeps its place");
    }
    assert!(started.elapsed() > Duration::from_secs(5), "closed after the time to log in, not before");
    let mut again = None;
    for _ in 0..50 {
        if let Ok(c) = connect(addr).await {
            again = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let db = again.expect("the places were given back");
    // Logged in, a client may stay idle: a pool's connections wait for hours.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(db.query_one("SELECT 1 + 1", &[]).await.unwrap().get::<_, i64>(0), 2);
}

/// Connections are counted before their thread starts: a burst of them
/// never has more threads than the limit allows.
#[tokio::test]
async fn a_burst_of_connections_keeps_to_the_limit() {
    let options = ServerOptions { pg_max_connections: 2, ..ServerOptions::default() };
    let (server, addr) = start("burst", options, PgOptions::default()).await;
    let mut open = Vec::new();
    for _ in 0..150 {
        open.push(TcpStream::connect(addr).await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let status = server.status();
    let connections = &status["connections"];
    let served = connections["active"].as_u64().unwrap() + connections["idle"].as_u64().unwrap();
    // The limit, and the 16 that are told why they are refused.
    assert!(served <= 2 + 16, "{served} connections served at once");
}

/// A transaction its client leaves idle is rolled back, and the client hears
/// of it at its next statement; a busy one, or an idle connection outside a
/// transaction, is left alone.
#[tokio::test]
async fn a_transaction_left_idle_is_rolled_back() {
    let pg = PgOptions { idle_in_transaction: Some(Duration::from_millis(400)), ..PgOptions::default() };
    let (_server, addr) = start("idle", ServerOptions::default(), pg).await;
    let db = connect(addr).await.unwrap();
    db.batch_execute("CREATE TABLE t (id SERIAL PRIMARY KEY, n BIGINT)").await.unwrap();

    db.batch_execute("BEGIN").await.unwrap();
    db.execute("INSERT INTO t (n) VALUES ($1)", &[&1i64]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let late = db.batch_execute("COMMIT").await.unwrap_err();
    assert_eq!(late.code(), Some(&SqlState::IDLE_IN_TRANSACTION_SESSION_TIMEOUT), "{late}");
    // Nothing was written, and the session goes on.
    let n: i64 = db.query_one("SELECT count(*) FROM t", &[]).await.unwrap().get(0);
    assert_eq!(n, 0);

    // Busy for longer than the limit, but never idle that long.
    db.batch_execute("BEGIN").await.unwrap();
    for i in 0..6i64 {
        db.execute("INSERT INTO t (n) VALUES ($1)", &[&i]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    db.batch_execute("COMMIT").await.unwrap();
    // Outside a transaction, idle is fine.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let n: i64 = db.query_one("SELECT count(*) FROM t", &[]).await.unwrap().get(0);
    assert_eq!(n, 6);

    // The extended protocol hears of it too, at its first message.
    db.batch_execute("BEGIN").await.unwrap();
    db.execute("INSERT INTO t (n) VALUES ($1)", &[&100i64]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let late = db.query("SELECT n FROM t WHERE n = $1", &[&100i64]).await.unwrap_err();
    assert_eq!(late.code(), Some(&SqlState::IDLE_IN_TRANSACTION_SESSION_TIMEOUT), "{late}");
    db.batch_execute("ROLLBACK").await.unwrap();
    assert!(db.query("SELECT n FROM t WHERE n = $1", &[&100i64]).await.unwrap().is_empty());
}

/// After login a message may be as long as a request body, not longer; one
/// that is longer is refused and the session goes on.
#[tokio::test]
async fn messages_are_as_long_as_a_request_body() {
    let options = ServerOptions { max_body_bytes: 1 << 20, ..ServerOptions::default() };
    let (_server, addr) = start("long", options, PgOptions::default()).await;
    let db = connect(addr).await.unwrap();
    let fits = format!("SELECT length('{}')", "x".repeat(900 << 10));
    assert_eq!(db.simple_query(&fits).await.unwrap().len(), 3);
    let long = format!("SELECT length('{}')", "x".repeat(2 << 20));
    let refused = db.simple_query(&long).await.unwrap_err();
    assert_eq!(refused.code(), Some(&SqlState::PROGRAM_LIMIT_EXCEEDED), "{refused}");
    assert_eq!(db.query_one("SELECT 1 + 1", &[]).await.unwrap().get::<_, i64>(0), 2);
}

/// A value PostgreSQL has and Nuage DB does not (`infinity`), sent in
/// binary, is refused as a bad value: the session goes on.
#[tokio::test]
async fn binary_infinity_is_refused_not_a_crash() {
    let (_server, addr) = start("infinity", ServerOptions::default(), PgOptions::default()).await;
    let mut s = startup(addr).await;
    s.write_all(&msg(b'p', b"t0ken\0")).await.unwrap();
    assert_eq!(until_ready(&mut s).await.last().map(|m| m.0), Some(b'Z'));
    for (oid, raw) in [(1114i32, i64::MAX.to_be_bytes().to_vec()), (1082, i32::MAX.to_be_bytes().to_vec())] {
        let mut parse = b"\0select $1\0".to_vec();
        parse.extend_from_slice(&1i16.to_be_bytes());
        parse.extend_from_slice(&oid.to_be_bytes());
        let mut bind = b"\0\0".to_vec();
        bind.extend_from_slice(&1i16.to_be_bytes());
        bind.extend_from_slice(&1i16.to_be_bytes());
        bind.extend_from_slice(&1i16.to_be_bytes());
        bind.extend_from_slice(&(raw.len() as i32).to_be_bytes());
        bind.extend_from_slice(&raw);
        bind.extend_from_slice(&0i16.to_be_bytes());
        let mut wire = msg(b'P', &parse);
        wire.extend(msg(b'B', &bind));
        wire.extend(msg(b'E', b"\0\0\0\0\0"));
        wire.extend(msg(b'S', b""));
        s.write_all(&wire).await.unwrap();
        let got = until_ready(&mut s).await;
        let error = got.iter().find(|m| m.0 == b'E').map(|m| String::from_utf8_lossy(&m.1).into_owned());
        assert!(error.as_deref().is_some_and(|e| e.contains("22P02")), "{oid}: {got:?}");
        assert_eq!(got.last().map(|m| m.0), Some(b'Z'), "{oid}: the session goes on");
    }
    s.write_all(&msg(b'Q', b"select 1\0")).await.unwrap();
    assert!(until_ready(&mut s).await.iter().any(|m| m.0 == b'D'));
}

/// Preparing a statement asks for its columns: they come from the tables,
/// without running the statement (which would cost as much as running it).
/// Its rows, read in batches, come in order and are let go as they go.
#[tokio::test]
async fn preparing_a_statement_does_not_run_it() {
    let (_server, addr) = start("prepare", ServerOptions::default(), PgOptions::default()).await;
    let mut db = connect(addr).await.unwrap();
    db.batch_execute("CREATE TABLE big (id SERIAL PRIMARY KEY, pad TEXT NOT NULL)").await.unwrap();
    for chunk in 0..20 {
        let values: Vec<String> =
            (0..1000).map(|i| format!("('{:0>200}')", (chunk * 1000 + i) * 7919 % 20000)).collect();
        db.batch_execute(&format!("INSERT INTO big (pad) VALUES {}", values.join(", "))).await.unwrap();
    }
    let sql = "SELECT * FROM big ORDER BY pad";

    let started = Instant::now();
    for _ in 0..5 {
        db.prepare(sql).await.unwrap();
    }
    let prepared = started.elapsed();
    let statement = db.prepare(sql).await.unwrap();
    let names: Vec<(&str, &tokio_postgres::types::Type)> =
        statement.columns().iter().map(|c| (c.name(), c.type_())).collect();
    assert_eq!(names, [("id", &tokio_postgres::types::Type::INT8), ("pad", &tokio_postgres::types::Type::TEXT)]);
    let started = Instant::now();
    let rows = db.query(&statement, &[]).await.unwrap();
    let ran = started.elapsed();
    assert_eq!(rows.len(), 20_000);
    assert!(prepared < ran, "5 statements prepared in {prepared:?}, one run in {ran:?}");

    // A portal read 7,000 rows at a time.
    let tx = db.transaction().await.unwrap();
    let portal = tx.bind("SELECT id FROM big ORDER BY id", &[]).await.unwrap();
    let mut seen = Vec::new();
    loop {
        let batch = tx.query_portal(&portal, 7000).await.unwrap();
        if batch.is_empty() {
            break;
        }
        assert!(batch.len() <= 7000);
        seen.extend(batch.iter().map(|r| r.get::<_, i64>(0)));
    }
    assert_eq!(seen, (1..=20_000).collect::<Vec<i64>>());
    tx.commit().await.unwrap();
}
