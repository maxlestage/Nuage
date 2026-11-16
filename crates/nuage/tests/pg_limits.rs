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
    startup_on(TcpStream::connect(addr).await.unwrap()).await
}

/// Sends the startup packet on `s`, then reads the password request.
async fn startup_on(mut s: TcpStream) -> TcpStream {
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

/// A session holds a bounded number of prepared statements; one closed, or
/// forgotten with `DEALLOCATE`, frees its place.
#[tokio::test]
async fn prepared_statements_are_bounded() {
    let (_server, addr) = start("statements", ServerOptions::default(), PgOptions::default()).await;
    let db = connect(addr).await.unwrap();
    let mut kept = Vec::new();
    for _ in 0..10_000 {
        kept.push(db.prepare("SELECT 1").await.unwrap());
    }
    let refused = db.prepare("SELECT 1").await.unwrap_err();
    assert_eq!(refused.code(), Some(&SqlState::PROGRAM_LIMIT_EXCEEDED), "{refused}");
    // Dropped, a statement is closed (the driver says so with its next message).
    kept.truncate(9_990);
    db.batch_execute("SELECT 1").await.unwrap();
    assert_eq!(db.query_one(&db.prepare("SELECT 2").await.unwrap(), &[]).await.unwrap().get::<_, i64>(0), 2);

    // DEALLOCATE forgets a statement by its name, as PostgreSQL does.
    let mut s = startup(addr).await;
    s.write_all(&msg(b'p', b"t0ken\0")).await.unwrap();
    until_ready(&mut s).await;
    let mut wire = msg(b'P', b"stmt_1\0select 1\0\0\0");
    wire.extend(msg(b'S', b""));
    wire.extend(msg(b'Q', b"DEALLOCATE stmt_1\0"));
    wire.extend(msg(b'B', b"\0stmt_1\0\0\0\0\0\0\0"));
    wire.extend(msg(b'S', b""));
    s.write_all(&wire).await.unwrap();
    assert!(until_ready(&mut s).await.iter().any(|m| m.0 == b'1'));
    assert!(until_ready(&mut s).await.iter().any(|m| m.0 == b'C' && m.1.starts_with(b"DEALLOCATE")));
    let bind = until_ready(&mut s).await;
    let error = bind.iter().find(|m| m.0 == b'E').map(|m| String::from_utf8_lossy(&m.1).into_owned());
    assert!(error.as_deref().is_some_and(|e| e.contains("does not exist")), "{bind:?}");
}

/// Logs in over the raw protocol, with `token`.
async fn logged_in(addr: SocketAddr, token: &str) -> TcpStream {
    let mut s = startup(addr).await;
    s.write_all(&msg(b'p', format!("{token}\0").as_bytes())).await.unwrap();
    assert_eq!(until_ready(&mut s).await.last().map(|m| m.0), Some(b'Z'));
    s
}

/// The SQLSTATE of the first error among `got`.
fn sqlstate_of(got: &[(u8, Vec<u8>)]) -> Option<String> {
    let (_, body) = got.iter().find(|m| m.0 == b'E')?;
    // Fields: a byte, then a text ending with 0; a 0 ends them.
    let mut rest = body.as_slice();
    while let [field, more @ ..] = rest {
        let end = more.iter().position(|b| *b == 0)?;
        if *field == b'C' {
            return Some(String::from_utf8_lossy(&more[..end]).into_owned());
        }
        rest = &more[end + 1..];
    }
    None
}

/// A Parse of `sql`, unnamed, with no parameter types.
fn parse_msg(name: &str, sql: &str) -> Vec<u8> {
    msg(b'P', format!("{name}\0{sql}\0\0\0").as_bytes())
}

/// A Bind of statement `stmt` to portal `portal`, its parameters in text
/// (`None`: `NULL`).
fn bind_msg(portal: &str, stmt: &str, params: &[Option<&[u8]>]) -> Vec<u8> {
    let mut b = format!("{portal}\0{stmt}\0").into_bytes();
    b.extend_from_slice(&0u16.to_be_bytes());
    b.extend_from_slice(&(params.len() as u16).to_be_bytes());
    for p in params {
        match p {
            None => b.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(v) => {
                b.extend_from_slice(&(v.len() as i32).to_be_bytes());
                b.extend_from_slice(v);
            }
        }
    }
    b.extend_from_slice(&0u16.to_be_bytes());
    msg(b'B', &b)
}

/// An Execute of `portal`, `max` rows at most (0: all).
fn execute_msg(portal: &str, max: i32) -> Vec<u8> {
    let mut b = format!("{portal}\0").into_bytes();
    b.extend_from_slice(&max.to_be_bytes());
    msg(b'E', &b)
}

/// A Bind gives as many values as the statement has parameters, as
/// PostgreSQL wants: thousands of `NULL`s for a statement that has one are
/// refused, not held.
#[tokio::test]
async fn a_bind_gives_the_parameters_its_statement_has() {
    let (_server, addr) = start("bind", ServerOptions::default(), PgOptions::default()).await;
    let mut s = logged_in(addr, "t0ken").await;
    let nulls = vec![None; 32_767];
    for params in [&[][..], &nulls[..], &[Some(&b"1"[..]), Some(b"2")][..]] {
        let mut wire = parse_msg("", "SELECT $1");
        wire.extend(bind_msg("", "", params));
        wire.extend(execute_msg("", 0));
        wire.extend(msg(b'S', b""));
        s.write_all(&wire).await.unwrap();
        let got = until_ready(&mut s).await;
        assert_eq!(sqlstate_of(&got).as_deref(), Some("08P01"), "{} parameters: {got:?}", params.len());
    }
    let mut wire = parse_msg("", "SELECT $1");
    wire.extend(bind_msg("", "", &[Some(b"7")]));
    wire.extend(execute_msg("", 0));
    wire.extend(msg(b'S', b""));
    s.write_all(&wire).await.unwrap();
    let got = until_ready(&mut s).await;
    assert!(got.iter().any(|m| m.0 == b'D' && m.1.ends_with(b"7")), "{got:?}");
}

/// What a session holds is counted in memory, not in bytes on the wire: a
/// statement too large to keep is refused before it is parsed, portals and
/// the rows they keep count, and a transaction's portals end with it.
#[tokio::test]
async fn what_a_session_holds_is_bounded() {
    // A session may hold half of the port's 8 MiB.
    let options = ServerOptions { max_body_bytes: 2 << 20, ..ServerOptions::default() };
    let pg = PgOptions { memory: Some(8 << 20), ..PgOptions::default() };
    let (_server, addr) = start("held", options, pg).await;
    big_table(addr, 5).await;
    let mut s = logged_in(addr, "t0ken").await;

    // A list of 80,000 numbers takes some 10 MiB once parsed: refused at
    // once (not after it is built), and the session goes on.
    let list = format!("SELECT id FROM big WHERE id IN ({})", vec!["1"; 80_000].join(","));
    let mut wire = parse_msg("long", &list);
    wire.extend(msg(b'S', b""));
    let started = Instant::now();
    s.write_all(&wire).await.unwrap();
    let got = until_ready(&mut s).await;
    assert_eq!(sqlstate_of(&got).as_deref(), Some("54000"), "{got:?}");
    assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
    // An SQL text past 1 MiB, whatever it holds.
    let text = format!("SELECT '{}'", "x".repeat((1 << 20) + 10));
    s.write_all(&msg(b'Q', format!("{text}\0").as_bytes())).await.unwrap();
    assert_eq!(sqlstate_of(&until_ready(&mut s).await).as_deref(), Some("54000"));

    // Portals of 100 KiB parameters: some forty fit in a session.
    let value = vec![b'v'; 100 << 10];
    let mut wire = parse_msg("one", "SELECT length($1)");
    wire.extend(msg(b'S', b""));
    s.write_all(&wire).await.unwrap();
    until_ready(&mut s).await;
    s.write_all(&msg(b'Q', b"BEGIN\0")).await.unwrap();
    until_ready(&mut s).await;
    let mut refused_at = None;
    for i in 0..100 {
        let mut wire = bind_msg(&format!("p{i}"), "one", &[Some(&value)]);
        wire.extend(msg(b'S', b""));
        s.write_all(&wire).await.unwrap();
        let got = until_ready(&mut s).await;
        if let Some(code) = sqlstate_of(&got) {
            assert_eq!(code, "54000", "{got:?}");
            refused_at = Some(i);
            break;
        }
    }
    let refused_at = refused_at.expect("portals without end");
    assert!((20..60).contains(&refused_at), "refused at portal {refused_at}");
    // The transaction ends, its portals with it: their room is free again,
    // and they are gone.
    s.write_all(&msg(b'Q', b"ROLLBACK\0")).await.unwrap();
    until_ready(&mut s).await;
    let mut wire = execute_msg("p0", 0);
    wire.extend(msg(b'S', b""));
    s.write_all(&wire).await.unwrap();
    let got = until_ready(&mut s).await;
    let error = got.iter().find(|m| m.0 == b'E').map(|m| String::from_utf8_lossy(&m.1).into_owned());
    assert!(error.is_some_and(|e| e.contains("does not exist")), "{got:?}");

    // A portal read one row at a time keeps the rest, counted: past what a
    // session may hold, the next one is refused and its rows let go.
    s.write_all(&msg(b'Q', b"BEGIN\0")).await.unwrap();
    until_ready(&mut s).await;
    let mut codes = Vec::new();
    for i in 0..10 {
        let got = suspend(&mut s, &format!("c{i}")).await;
        codes.push(sqlstate_of(&got));
        if codes.last().unwrap().is_some() {
            break;
        }
    }
    assert!(codes.len() > 1 && codes.last().unwrap().as_deref() == Some("54000"), "{codes:?}");
    // Read to its end, a portal lets go of what it kept.
    s.write_all(&msg(b'Q', b"ROLLBACK\0")).await.unwrap();
    until_ready(&mut s).await;
    s.write_all(&msg(b'Q', b"BEGIN\0")).await.unwrap();
    until_ready(&mut s).await;
    for i in 0..4 {
        let portal = format!("r{i}");
        assert_eq!(sqlstate_of(&suspend(&mut s, &portal).await), None, "portal {i}");
        let mut wire = execute_msg(&portal, 0);
        wire.extend(msg(b'S', b""));
        s.write_all(&wire).await.unwrap();
        let got = until_ready(&mut s).await;
        assert!(got.iter().any(|m| m.0 == b'C'), "{i}: {:?}", sqlstate_of(&got));
    }
}

/// The sessions of a port share its memory: when large ones hold it all,
/// the next is refused (53200) for what goes past its own little, and finds
/// room once they let go.
#[tokio::test]
async fn the_sessions_of_a_port_share_its_memory() {
    let options = ServerOptions { max_body_bytes: 2 << 20, ..ServerOptions::default() };
    let pg = PgOptions { memory: Some(8 << 20), ..PgOptions::default() };
    let (_server, addr) = start("shared", options, pg).await;
    big_table(addr, 5).await;
    let mut sessions = Vec::new();
    let mut refused = None;
    for i in 0..12 {
        let mut s = logged_in(addr, "t0ken").await;
        s.write_all(&msg(b'Q', b"BEGIN\0")).await.unwrap();
        until_ready(&mut s).await;
        let code = sqlstate_of(&suspend(&mut s, "held").await);
        sessions.push(s);
        if let Some(code) = code {
            refused = Some((i, code));
            break;
        }
    }
    let (at, code) = refused.expect("sessions hold without end");
    assert_eq!(code, "53200");
    assert!(at >= 2, "refused at session {at}");
    // One lets go: the last finds room.
    let mut first = sessions.remove(0);
    first.write_all(&msg(b'Q', b"ROLLBACK\0")).await.unwrap();
    until_ready(&mut first).await;
    let last = sessions.last_mut().unwrap();
    last.write_all(&msg(b'Q', b"ROLLBACK\0")).await.unwrap();
    until_ready(last).await;
    last.write_all(&msg(b'Q', b"BEGIN\0")).await.unwrap();
    until_ready(last).await;
    assert_eq!(sqlstate_of(&suspend(last, "again").await), None);
}

/// A table `big` of `thousands` thousand rows of 300 bytes.
async fn big_table(addr: SocketAddr, thousands: usize) {
    let db = connect(addr).await.unwrap();
    db.batch_execute("CREATE TABLE big (id SERIAL PRIMARY KEY, pad TEXT NOT NULL)").await.unwrap();
    for _ in 0..thousands {
        let values: Vec<String> = (0..1000).map(|_| format!("('{}')", "p".repeat(300))).collect();
        db.batch_execute(&format!("INSERT INTO big (pad) VALUES {}", values.join(", "))).await.unwrap();
    }
}

/// Binds `SELECT * FROM big` to `portal` and reads one row of it: the rest
/// waits in the portal. What the server answered, to `ReadyForQuery`.
async fn suspend(s: &mut TcpStream, portal: &str) -> Vec<(u8, Vec<u8>)> {
    let mut wire = parse_msg("", "SELECT * FROM big");
    wire.extend(bind_msg(portal, "", &[]));
    wire.extend(execute_msg(portal, 1));
    wire.extend(msg(b'S', b""));
    s.write_all(&wire).await.unwrap();
    until_ready(s).await
}

/// A transaction kept busy is rolled back all the same past its lifetime;
/// the session goes on.
#[tokio::test]
async fn a_transaction_has_a_lifetime() {
    let pg = PgOptions {
        idle_in_transaction: Some(Duration::from_secs(30)),
        transaction_timeout: Some(Duration::from_millis(700)),
        ..PgOptions::default()
    };
    let (_server, addr) = start("lifetime", ServerOptions::default(), pg).await;
    let db = connect(addr).await.unwrap();
    db.batch_execute("CREATE TABLE t (id SERIAL PRIMARY KEY, n BIGINT)").await.unwrap();
    db.batch_execute("BEGIN").await.unwrap();
    db.execute("INSERT INTO t (n) VALUES (1)", &[]).await.unwrap();
    let started = Instant::now();
    let ended = loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Err(e) = db.query_one("SELECT 1 + 1", &[]).await {
            break e;
        }
        assert!(started.elapsed() < Duration::from_secs(10), "the transaction lives on");
    };
    assert_eq!(ended.code().map(|c| c.code()), Some("25P04"), "{ended}");
    assert!(started.elapsed() >= Duration::from_millis(500));
    // Rolled back: nothing written; the session goes on.
    db.batch_execute("COMMIT").await.unwrap();
    assert_eq!(db.query_one("SELECT count(*) FROM t", &[]).await.unwrap().get::<_, i64>(0), 0);
    // A transaction left waiting reaches its lifetime too.
    db.batch_execute("BEGIN").await.unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let late = db.batch_execute("COMMIT").await.unwrap_err();
    assert_eq!(late.code().map(|c| c.code()), Some("25P04"), "{late}");
}

/// Read-only tokens (which may be in web pages, and so anyone's) have half
/// of the connections at most: read-write clients always find a place.
#[tokio::test]
async fn read_only_tokens_have_half_of_the_places() {
    let options = ServerOptions { pg_max_connections: 4, ..ServerOptions::default() };
    let (server, addr) = start("read-only", options, PgOptions::default()).await;
    let read_only = server.new_app_read_only_token("nuage").unwrap();
    let connect_with = |token: String| async move {
        let (client, conn) = tokio_postgres::connect(
            &format!("host={} port={} user=anyone password={token} dbname=nuage", addr.ip(), addr.port()),
            NoTls,
        )
        .await?;
        tokio::spawn(conn);
        Ok::<_, tokio_postgres::Error>(client)
    };
    let first = connect_with(read_only.clone()).await.unwrap();
    let _second = connect_with(read_only.clone()).await.unwrap();
    let refused = connect_with(read_only.clone()).await.expect_err("a third read-only connection");
    assert_eq!(refused.code(), Some(&SqlState::TOO_MANY_CONNECTIONS), "{refused}");
    let message = refused.as_db_error().map(|e| e.message().to_owned()).unwrap_or_default();
    assert!(message.contains("read-only"), "{message}");
    let writer = connect(addr).await.expect("a read-write client finds a place");
    assert_eq!(writer.query_one("SELECT 1 + 1", &[]).await.unwrap().get::<_, i64>(0), 2);
    // A place given back is a place to take.
    drop(first);
    let mut again = None;
    for _ in 0..50 {
        if let Ok(c) = connect_with(read_only.clone()).await {
            again = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(again.is_some(), "the read-only place was given back");
}

/// Describing a statement names and types its columns as a run does,
/// without running it, for expressions, joins and derived tables too.
#[tokio::test]
async fn statements_are_described_as_they_run() {
    let (_server, addr) = start("describe", ServerOptions::default(), PgOptions::default()).await;
    let db = connect(addr).await.unwrap();
    db.batch_execute("CREATE TABLE big (id SERIAL PRIMARY KEY, pad TEXT NOT NULL, n BIGINT)").await.unwrap();
    for chunk in 0..20 {
        let values: Vec<String> =
            (0..1000).map(|i| format!("('{:0>200}', {i})", (chunk * 1000 + i) * 7919 % 20000)).collect();
        db.batch_execute(&format!("INSERT INTO big (pad, n) VALUES {}", values.join(", "))).await.unwrap();
    }
    let slow = [
        "SELECT *, pad || 'x' FROM big ORDER BY pad",
        "SELECT * FROM (SELECT * FROM big) s ORDER BY pad",
        "SELECT a.id, b.id FROM big a JOIN big b ON a.id = b.id ORDER BY a.pad",
        "SELECT id, n + 1, count(*), CAST(n AS TEXT), lower(pad) AS low FROM big GROUP BY id, n, pad ORDER BY pad",
    ];
    for sql in slow {
        let started = Instant::now();
        for _ in 0..5 {
            db.prepare(sql).await.unwrap();
        }
        let prepared = started.elapsed();
        let statement = db.prepare(sql).await.unwrap();
        let started = Instant::now();
        let rows = db.query(&statement, &[]).await.unwrap();
        let ran = started.elapsed();
        assert_eq!(rows.len(), 20_000, "{sql}");
        assert!(prepared < ran, "{sql}: 5 statements prepared in {prepared:?}, one run in {ran:?}");
        // The names a run gives (the simple protocol has no Describe).
        let simple = db.simple_query(&format!("{sql} LIMIT 1")).await.unwrap();
        let ran_names: Vec<String> = simple
            .iter()
            .find_map(|m| match m {
                tokio_postgres::SimpleQueryMessage::Row(r) => {
                    Some(r.columns().iter().map(|c| c.name().to_owned()).collect())
                }
                _ => None,
            })
            .unwrap();
        let described: Vec<String> = statement.columns().iter().map(|c| c.name().to_owned()).collect();
        assert_eq!(described, ran_names, "{sql}");
    }
}

/// A client that takes nothing of its answers, or stops halfway through a
/// message, is disconnected after a minute: its transaction, and the result
/// it was sent, are let go.
#[tokio::test]
async fn a_client_that_stalls_is_disconnected() {
    let (server, addr) = start("stall", ServerOptions::default(), PgOptions::default()).await;
    let db = connect(addr).await.unwrap();
    db.batch_execute("CREATE TABLE big (id SERIAL PRIMARY KEY, pad TEXT NOT NULL)").await.unwrap();
    let pad = "p".repeat(8000);
    for _ in 0..40 {
        let values: Vec<String> = (0..100).map(|_| format!("('{pad}')")).collect();
        db.batch_execute(&format!("INSERT INTO big (pad) VALUES {}", values.join(", "))).await.unwrap();
    }
    drop(db);
    // 32 MB of answer, never read, in a transaction; the client's socket
    // takes little of it (the system would let it take as much).
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(16 << 10).unwrap();
    let mut deaf = startup_on(socket.connect(addr).await.unwrap()).await;
    deaf.write_all(&msg(b'p', b"t0ken\0")).await.unwrap();
    until_ready(&mut deaf).await;
    deaf.write_all(&msg(b'Q', b"BEGIN\0")).await.unwrap();
    until_ready(&mut deaf).await;
    deaf.write_all(&msg(b'Q', b"SELECT * FROM big\0")).await.unwrap();
    // Half a message, and nothing more.
    let mut halfway = logged_in(addr, "t0ken").await;
    halfway.write_all(&[b'Q', 0, 0, 0, 100, b's', b'e']).await.unwrap();
    let open = |server: &Server| {
        let status = server.status();
        status["connections"]["active"].as_u64().unwrap() + status["connections"]["idle"].as_u64().unwrap()
    };
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(open(&server), 2);
    let started = Instant::now();
    while open(&server) > 0 {
        assert!(started.elapsed() < Duration::from_secs(80), "{} stalled clients still served", open(&server));
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(started.elapsed() > Duration::from_secs(40), "disconnected after a minute, not before");
    assert!(closed_within(&mut halfway, Duration::from_secs(5)).await);
}
