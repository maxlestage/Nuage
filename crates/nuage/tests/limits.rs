//! What one client may take of the server: a slow statement holds a thread,
//! never the server; a request is refused, clearly, before it can take more
//! memory than it should; nothing a client sends stays in memory for good.

#![cfg(feature = "server")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use nuage::{Engine, EngineOptions, Server, ServerOptions};
use serde_json::{Value as Json, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("nuage-limits-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn engine(name: &str) -> Arc<Engine> {
    Engine::open(temp_dir(name), EngineOptions { sync: false, ..EngineOptions::default() }).unwrap()
}

fn options() -> ServerOptions {
    ServerOptions {
        token: Some("t0ken".into()),
        admin_password: Some("correct horse".into()),
        ..ServerOptions::default()
    }
}

struct Http {
    status: u16,
    headers: hyper::HeaderMap,
    body: String,
}

impl Http {
    fn json(&self) -> Json {
        serde_json::from_str(&self.body).unwrap_or(Json::Null)
    }
}

async fn http(addr: SocketAddr, method: &str, path: &str, headers: &[(&str, &str)], body: impl Into<Bytes>) -> Http {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await.unwrap();
    tokio::spawn(conn);
    let mut req = Request::builder().method(method).uri(path).header("host", addr.to_string());
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let res = sender.send_request(req.body(Full::new(body.into())).unwrap()).await.unwrap();
    let status = res.status().as_u16();
    let headers = res.headers().clone();
    let body = String::from_utf8_lossy(&res.into_body().collect().await.unwrap().to_bytes()).into_owned();
    Http { status, headers, body }
}

const AUTH: (&str, &str) = ("authorization", "Bearer t0ken");

/// Sends `head` (and nothing more) on a connection of its own: the start of
/// the answer, which must come within 5 seconds.
async fn answer_to(addr: SocketAddr, head: &str) -> String {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(head.as_bytes()).await.unwrap();
    let mut answer = vec![0u8; 4096];
    let n =
        tokio::time::timeout(Duration::from_secs(5), s.read(&mut answer)).await.expect("an answer in time").unwrap();
    String::from_utf8_lossy(&answer[..n]).into_owned()
}

async fn sql(addr: SocketAddr, auth: &str, body: &Json) -> Http {
    let auth = format!("Bearer {auth}");
    http(addr, "POST", "/v1/sql", &[("authorization", &auth)], body.to_string()).await
}

/// A table of `rows` rows, `t (id, v)`.
async fn table(addr: SocketAddr, rows: usize) {
    let made = sql(addr, "t0ken", &json!({ "sql": "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)" })).await;
    assert_eq!(made.status, 200, "{}", made.body);
    let values: Vec<String> = (1..=rows).map(|i| format!("({i}, {})", i * 7 % 1000)).collect();
    let filled = sql(addr, "t0ken", &json!({ "sql": format!("INSERT INTO t VALUES {}", values.join(",")) })).await;
    assert_eq!(filled.status, 200, "{}", filled.body);
}

/// A read that takes a while: every row against every other.
const SLOW: &str = "SELECT count(*) FROM t a WHERE (SELECT count(*) FROM t b WHERE b.v < a.v + 0 * ?) >= 0";

/// With one worker, a read that ran on it without handing its duties over
/// would stop everything: the health check, the other connections, the
/// timers. It must not, and the parse of a text never seen is part of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn a_slow_read_holds_a_thread_never_the_server() {
    let server = Server::new(engine("slow"), options());
    let running = server.listen("127.0.0.1:0").await.unwrap();
    let addr = running.local_addr();
    table(addr, 1500).await;
    let slow = tokio::spawn(async move {
        let started = Instant::now();
        let r = sql(addr, "t0ken", &json!({ "sql": SLOW, "params": [1] })).await;
        (r.status, r.body, started.elapsed())
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let started = Instant::now();
    let health = tokio::time::timeout(Duration::from_secs(20), http(addr, "GET", "/api/health", &[], "")).await;
    let waited = started.elapsed();
    assert_eq!(health.expect("the health check answers").status, 200);
    let ping = http(addr, "GET", "/v1/ping", &[AUTH], "").await;
    assert_eq!(ping.status, 200, "{}", ping.body);
    assert!(!slow.is_finished(), "the read should still run (it took less than the test assumes)");
    let (status, body, took) = slow.await.unwrap();
    assert_eq!(status, 200, "{body}");
    assert!(waited < took / 2, "health waited {waited:?} for a read of {took:?}");
}

/// What a request cannot be is refused before anything of it is copied or
/// converted: parameters past PostgreSQL's limit, a thousand statements and
/// more, a megabyte of SQL, JSON whose tree would take more than the memory
/// for requests.
#[tokio::test]
async fn sql_requests_that_cannot_be_are_refused_early() {
    let server = Server::new(
        engine("sql-limits"),
        ServerOptions { max_body_bytes: 2 << 20, request_memory: Some(8 << 20), ..options() },
    );
    let running = server.listen("127.0.0.1:0").await.unwrap();
    let addr = running.local_addr();
    let kind = |r: &Http| r.json()["error"]["kind"].as_str().unwrap_or_default().to_owned();

    let params = sql(addr, "t0ken", &json!({ "sql": "SELECT 1", "params": vec![0; 65_536] })).await;
    assert_eq!((params.status, kind(&params)), (400, "validation".into()), "{}", params.body);
    assert!(params.body.contains("65535 at most"), "{}", params.body);
    let given = sql(addr, "t0ken", &json!({ "sql": "SELECT ?", "params": [1, 2] })).await;
    assert!(given.status == 400 && given.body.contains("2 parameter(s) given"), "{}", given.body);

    let many: Vec<Json> = (0..1001).map(|_| json!({ "sql": "SELECT 1" })).collect();
    let many = sql(addr, "t0ken", &json!({ "statements": many })).await;
    assert_eq!((many.status, kind(&many)), (413, "too_large".into()), "{}", many.body);
    let fine: Vec<Json> = (0..1000).map(|_| json!({ "sql": "SELECT 1" })).collect();
    assert_eq!(sql(addr, "t0ken", &json!({ "statements": fine })).await.status, 200);

    let long = format!("SELECT 1{}", " ".repeat(1 << 20));
    let long = sql(addr, "t0ken", &json!({ "sql": long })).await;
    assert_eq!((long.status, kind(&long)), (413, "too_large".into()), "{}", long.body);

    // 1.5 MiB of zeros: some fifty MiB once parsed, more than the server reads at once.
    let zeros = format!(r#"{{"sql":"SELECT 1","params":[{}0]}}"#, "0,".repeat(750_000));
    let heavy = http(addr, "POST", "/v1/sql", &[AUTH], zeros).await;
    assert_eq!((heavy.status, kind(&heavy)), (413, "too_large".into()), "{}", heavy.body);
    assert!(heavy.body.contains("memory once read"), "{}", heavy.body);

    // Larger than any body: refused on its length, before it is read.
    let big = answer_to(
        addr,
        "POST /v1/sql HTTP/1.1\r\nhost: x\r\nauthorization: Bearer t0ken\r\ncontent-length: 2097153\r\n\r\n",
    )
    .await;
    assert!(big.starts_with("HTTP/1.1 413") && big.contains("too_large"), "{big}");

    // What is left works as it did.
    let ok = sql(addr, "t0ken", &json!({ "sql": "SELECT ? AS n", "params": [41] })).await;
    assert_eq!((ok.status, ok.body.as_str()), (200, r#"{"columns":["n"],"rows":[{"n":41}]}"#));
}

/// The login form takes a few KiB, and the limit of failed attempts is
/// checked before it is read: a client without the password costs nothing.
#[tokio::test]
async fn the_login_form_is_small() {
    let server = Server::new(engine("login"), options());
    let running = server.listen("127.0.0.1:0").await.unwrap();
    let addr = running.local_addr();
    // 16 MiB announced, none of it sent: the answer comes at once.
    let form = "POST /admin/login HTTP/1.1\r\nhost: x\r\ncontent-type: application/x-www-form-urlencoded\r\n";
    let answer = answer_to(addr, &format!("{form}content-length: 16777216\r\n\r\n")).await;
    assert!(answer.starts_with("HTTP/1.1 413"), "{answer}");
    // A form of the right size is read as before (without its cookie: expired).
    let form_type = [("content-type", "application/x-www-form-urlencoded")];
    let wrong = http(addr, "POST", "/admin/login", &form_type, "password=nope").await;
    assert_eq!(wrong.status, 403);
    // Past the failed attempts, refused before anything is read.
    let login = http(addr, "GET", "/admin/login", &[], "").await;
    let cookie = login.headers["set-cookie"].to_str().unwrap().split(';').next().unwrap().to_owned();
    let csrf = cookie.split_once('=').unwrap().1.to_owned();
    for _ in 0..5 {
        let headers = [form_type[0], ("cookie", cookie.as_str())];
        let tried = http(addr, "POST", "/admin/login", &headers, format!("csrf={csrf}&password=nope")).await;
        assert_eq!(tried.status, 401);
    }
    let answer = answer_to(addr, &format!("{form}cookie: {cookie}\r\ncontent-length: 4000\r\n\r\n")).await;
    assert!(answer.starts_with("HTTP/1.1 429"), "{answer}");
}

/// The schemas that clients describe are checked before the server keeps
/// them, and kept apart for each application: filling one application's
/// cannot stop SQL, nor another application.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn described_schemas_are_bounded_and_kept_apart() {
    let server = Server::new(engine("schemas"), options());
    let (_, shop) = server.create_app("shop", "").unwrap();
    let reader = server.new_app_read_only_token("shop").unwrap();
    let running = server.listen("127.0.0.1:0").await.unwrap();
    let addr = running.local_addr();
    let made = sql(addr, &shop, &json!({ "sql": "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT)" })).await;
    assert_eq!(made.status, 200, "{}", made.body);
    let schema = |name: &str, column: &str| json!({ "name": name, "pk": 0, "columns": [{ "name": column, "kind": "bigint" }], "indexes": [] });
    let find = |schema: Json| json!({ "query": { "findByKey": schema, "key": 1 } });
    let reader_auth = format!("Bearer {reader}");
    let as_reader: &[(&str, &str)] = &[("authorization", &reader_auth), ("content-type", "application/json")];

    // A name of a megabyte: refused, nothing kept.
    let huge = http(addr, "POST", "/v1/fetch", as_reader, find(schema(&"n".repeat(1 << 20), "id")).to_string()).await;
    assert_eq!(huge.status, 400, "{}", huge.body);
    assert!(huge.body.contains("63 bytes"), "{}", huge.body);
    let described = |status: &Json, app: &str| {
        status["databases"].as_array().unwrap().iter().find(|d| d["name"] == app).unwrap()["described_schemas"].clone()
    };
    let shop_auth = format!("Bearer {shop}");
    let as_shop = [("authorization", shop_auth.as_str())];
    let status = || http(addr, "GET", "/api/status", &as_shop, "");
    assert_eq!(described(&status().await.json(), "shop")["count"], 0);

    // Ever new schemas from a read-only token: its application's clients are
    // stopped at some point, the rest goes on.
    let mut refused = None;
    for round in 0..3 {
        let statements: Vec<Json> =
            (0..1000).map(|i| json!({ "fetch": find(schema(&format!("ghost{round}_{i}"), "id"))["query"] })).collect();
        let batch = http(addr, "POST", "/v1/batch", as_reader, json!({ "statements": statements }).to_string()).await;
        if batch.body.contains("application `shop`") {
            refused = Some(batch);
            break;
        }
        assert!(batch.body.contains("no_such_table"), "{}", batch.body);
    }
    let refused = refused.expect("refused at some point");
    assert_eq!(refused.status, 400, "{}", refused.body);
    let kept = described(&status().await.json(), "shop");
    assert!((1..=1000).contains(&kept["count"].as_u64().unwrap()), "{kept}");
    // SQL on the same application, a new table included, and another application.
    let read = sql(addr, &shop, &json!({ "sql": "SELECT count(*) AS n FROM items" })).await;
    assert_eq!(read.status, 200, "{}", read.body);
    let new = sql(addr, &shop, &json!({ "sql": "CREATE TABLE more (id INTEGER PRIMARY KEY)" })).await;
    assert_eq!(new.status, 200, "{}", new.body);
    let other = http(
        addr,
        "POST",
        "/v1/fetch",
        &[AUTH, ("content-type", "application/json")],
        find(schema("elsewhere", "id")).to_string(),
    )
    .await;
    assert_eq!(other.status, 404, "no such table, but read: {}", other.body);
    assert!(other.body.contains("no_such_table"), "{}", other.body);
}

/// The answers of the key-value store are bounded: a request of a few bytes
/// cannot ask for the same large value a thousand times.
#[tokio::test]
async fn key_value_answers_are_bounded() {
    let server = Server::new(engine("kv"), ServerOptions { kv_bytes: 64 << 20, ..options() });
    let running = server.listen("127.0.0.1:0").await.unwrap();
    let addr = running.local_addr();
    assert_eq!(http(addr, "PUT", "/v1/kv/big", &[AUTH], vec![b'x'; 1 << 20]).await.status, 200);
    let mut mget = vec![json!("MGET")];
    mget.extend(std::iter::repeat_n(json!("big"), 400));
    let refused = http(addr, "POST", "/v1/kv", &[AUTH], Json::Array(mget).to_string()).await;
    assert_eq!(refused.status, 400, "{}", refused.body.len());
    assert!(refused.body.contains("fewer keys"), "{}", refused.body);
    let gets: Vec<Json> = (0..20).map(|_| json!(["GET", "big"])).collect();
    let listed = http(addr, "POST", "/v1/kv", &[AUTH], Json::Array(gets).to_string()).await;
    assert_eq!(listed.status, 200);
    let results = listed.json()["results"].as_array().unwrap().clone();
    assert_eq!(results.len(), 20);
    assert_eq!(results[0].as_str().map(str::len), Some(1 << 20));
    assert!(results[19]["error"].as_str().unwrap().contains("not run"), "{}", results[19]);
    let commands: Vec<Json> = (0..10_001).map(|_| json!(["PING"])).collect();
    let too_many = http(addr, "POST", "/v1/kv", &[AUTH], Json::Array(commands).to_string()).await;
    assert_eq!(too_many.status, 413, "{}", too_many.body);
}

/// Past the connections it may hold, the server says so and closes; an idle
/// connection is closed after the time for headers; a body that does not
/// come is given up, and so is an answer that the client does not take.
#[tokio::test]
async fn connections_are_bounded_in_number_and_time() {
    let server = Server::new(
        engine("conns"),
        ServerOptions {
            max_connections: 2,
            header_timeout: Duration::from_millis(400),
            body_timeout: Duration::from_millis(300),
            send_timeout: Duration::from_millis(300),
            max_body_bytes: 32 << 20,
            kv_bytes: 64 << 20,
            ..options()
        },
    );
    let running = server.listen("127.0.0.1:0").await.unwrap();
    let addr = running.local_addr();
    let mut idle = [TcpStream::connect(addr).await.unwrap(), TcpStream::connect(addr).await.unwrap()];
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut third = TcpStream::connect(addr).await.unwrap();
    let mut answer = String::new();
    tokio::time::timeout(Duration::from_secs(5), third.read_to_string(&mut answer)).await.expect("closed").unwrap();
    assert!(answer.starts_with("HTTP/1.1 503") && answer.contains("retry-after: 1"), "{answer}");
    let body = answer.split("\r\n\r\n").nth(1).unwrap();
    assert_eq!(serde_json::from_str::<Json>(body).unwrap()["error"]["kind"], "unavailable");
    // The idle ones are closed by the server, and there is room again.
    for s in &mut idle {
        let mut rest = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut rest)).await.expect("closed").unwrap();
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(http(addr, "GET", "/api/health", &[], "").await.status, 200);
    // Half a body, then nothing.
    let half =
        "POST /v1/sql HTTP/1.1\r\nhost: x\r\nauthorization: Bearer t0ken\r\ncontent-length: 100\r\n\r\n{\"sql\":";
    let answer = answer_to(addr, half).await;
    assert!(answer.starts_with("HTTP/1.1 408") && answer.contains("did not arrive"), "{answer}");
    // 30 MiB asked for, nothing read: the server lets go.
    assert_eq!(http(addr, "PUT", "/v1/kv/big", &[AUTH], vec![b'x'; 30 << 20]).await.status, 200);
    let mut reader = TcpStream::connect(addr).await.unwrap();
    reader.write_all(b"GET /v1/kv/big HTTP/1.1\r\nhost: x\r\nauthorization: Bearer t0ken\r\n\r\n").await.unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let mut taken = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), reader.read_to_end(&mut taken)).await.expect("closed");
    assert!(taken.len() < 30 << 20, "{} bytes: the whole answer waited", taken.len());
}

/// Interactive transactions are bounded in number and in age, however busy.
#[tokio::test]
async fn transactions_are_bounded() {
    let server = Server::new(
        engine("tx"),
        ServerOptions { max_transactions: 2, transaction_lifetime: Duration::from_millis(1500), ..options() },
    );
    let running = server.listen("127.0.0.1:0").await.unwrap();
    let addr = running.local_addr();
    let begin = || http(addr, "POST", "/v1/transactions", &[AUTH], "");
    let first = begin().await.json()["id"].as_str().unwrap().to_owned();
    assert_eq!(begin().await.status, 200);
    let third = begin().await;
    assert_eq!(third.status, 429, "{}", third.body);
    assert_eq!(third.headers["retry-after"], "1");
    // Kept busy, the first one still ends at its lifetime.
    let started = Instant::now();
    let mut ended = None;
    while started.elapsed() < Duration::from_secs(10) {
        let raw = json!({ "query": { "raw": "SELECT 1", "params": [] } }).to_string();
        let r = http(addr, "POST", "/v1/fetch", &[AUTH, ("nuage-transaction", &first)], raw).await;
        if r.status != 200 {
            ended = Some((r, started.elapsed()));
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let (r, after) = ended.expect("the transaction ends");
    assert!(r.body.contains("transaction_finished"), "{}", r.body);
    assert!(after >= Duration::from_millis(1200), "{after:?}");
    assert_eq!(begin().await.status, 200, "room for one again");
}

/// A request whose client leaves half-way is no longer counted as active.
#[tokio::test]
async fn abandoned_requests_are_not_counted() {
    let server = Server::new(engine("abandoned"), options());
    let running = server.listen("127.0.0.1:0").await.unwrap();
    let addr = running.local_addr();
    table(addr, 1200).await;
    let active = || async move {
        http(addr, "GET", "/api/status", &[AUTH], "").await.json()["connections"]["active"].as_u64().unwrap()
    };
    assert_eq!(active().await, 1, "the status request itself");
    // A slow write, and its client gone before the answer.
    let update = "UPDATE t SET v = v WHERE id IN \
                  (SELECT a.id FROM t a WHERE (SELECT count(*) FROM t b WHERE b.v < a.v + 0 * ?) >= 0)";
    let body = json!({ "sql": update, "params": [1] }).to_string();
    let mut s = TcpStream::connect(addr).await.unwrap();
    let head = format!(
        "POST /v1/sql HTTP/1.1\r\nhost: x\r\nauthorization: Bearer t0ken\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).await.unwrap();
    s.write_all(body.as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(s);
    let started = Instant::now();
    while active().await != 1 {
        assert!(started.elapsed() < Duration::from_secs(20), "still counted");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Statements take threads of their own, reads and writes apart: when slow
/// reads hold every thread for reads, the next read is turned away with 503
/// and `Retry-After` instead of queueing, a write still goes through, and
/// the health check and `/v1/ping` answer at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_reads_never_queue_without_end_nor_hold_up_writes() {
    let server = Server::new(engine("admission"), ServerOptions { statement_threads: 4, ..options() });
    let running = server.listen("127.0.0.1:0").await.unwrap();
    let addr = running.local_addr();
    table(addr, 2500).await;
    let slow: Vec<_> = (0..2)
        .map(|i| tokio::spawn(async move { sql(addr, "t0ken", &json!({ "sql": SLOW, "params": [i] })).await.status }))
        .collect();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = Instant::now();
    let turned_away = sql(addr, "t0ken", &json!({ "sql": "SELECT count(*) FROM t WHERE v > ?", "params": [1] })).await;
    assert_eq!(turned_away.status, 503, "{}", turned_away.body);
    assert_eq!(turned_away.headers["retry-after"], "1");
    assert_eq!(turned_away.json()["error"]["kind"], "unavailable");
    assert!(started.elapsed() < Duration::from_secs(4), "{:?}", started.elapsed());
    let started = Instant::now();
    let write = sql(addr, "t0ken", &json!({ "sql": "INSERT INTO t VALUES (?, 1)", "params": [99_999] })).await;
    assert_eq!(write.status, 200, "{}", write.body);
    assert_eq!(http(addr, "GET", "/api/health", &[], "").await.status, 200);
    assert_eq!(http(addr, "GET", "/v1/ping", &[AUTH], "").await.status, 200);
    assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
    assert!(slow.iter().all(|s| !s.is_finished()), "the slow reads ran for less than the test assumes");
    for s in slow {
        assert_eq!(s.await.unwrap(), 200);
    }
}
