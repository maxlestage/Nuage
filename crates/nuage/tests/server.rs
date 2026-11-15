//! The server end to end: Memoire's client against a local Nuage DB server,
//! the status API, the console and S3 backups.

#![cfg(feature = "server")]
#[path = "common/mod.rs"]
mod common;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::HeaderMap;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use memoire_orm::nuage::NuageError;
use memoire_orm::prelude::*;
use memoire_orm::{Query, Value};
use nuage::{Engine, EngineOptions, Server, ServerOptions, SiteResponse};
use tokio::net::{TcpListener, TcpStream};

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("nuage-server-{name}-{}", std::process::id()));
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
    headers: HeaderMap,
    body: String,
}

async fn http(addr: SocketAddr, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Http {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await.unwrap();
    tokio::spawn(conn);
    let mut req = Request::builder().method(method).uri(path).header("host", addr.to_string());
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let res = sender.send_request(req.body(Full::new(Bytes::from(body.to_owned()))).unwrap()).await.unwrap();
    let status = res.status().as_u16();
    let headers = res.headers().clone();
    let body = String::from_utf8_lossy(&res.into_body().collect().await.unwrap().to_bytes()).into_owned();
    Http { status, headers, body }
}

fn unauthorized(e: &Error) -> bool {
    matches!(e, Error::Database(d) if d.downcast_ref::<NuageError>().is_some_and(NuageError::is_unauthorized))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_and_server_end_to_end() {
    let server = Server::new(engine("e2e"), options());
    let running = server.listen("127.0.0.1:0").await.unwrap();
    let db = NuageDb::connect(&running.url(), "t0ken").await.unwrap();

    // Memoire's backend suite, over HTTP.
    common::run_suite(&db).await.unwrap();
    common::advanced::run(&db).await.unwrap();
    common::lifecycle::run(&db).await.unwrap();

    // Errors come back as Memoire's own.
    use common::{User, user};
    db.drop_table_if_exists::<User>().await.unwrap();
    db.create_table::<User>().await.unwrap();

    // A table's schema travels whole once, then as a reference: the suite above ran that way
    // (the server says in `/v1/info` that it keeps schemas by reference). The server knows the
    // reference of a schema it was sent, and says so of one it never saw.
    let auth = [("authorization", "Bearer t0ken"), ("content-type", "application/json")];
    let info = http(running.local_addr(), "GET", "/v1/info", &auth, "").await;
    assert!(info.body.contains(r#""schemaRefs":true"#), "{}", info.body);
    let by_reference = |reference: &str| {
        let key = memoire_orm::nuage::wire::value_to_json(&Value::I64(1));
        serde_json::json!({ "query": { "findByKey": reference, "key": key } }).to_string()
    };
    let known = memoire_orm::nuage::wire::schema_ref(User::SCHEMA);
    let found = http(running.local_addr(), "POST", "/v1/fetch", &auth, &by_reference(&known)).await;
    assert_eq!((found.status, found.body.as_str()), (200, r#"{"rows":[]}"#));
    let never = http(running.local_addr(), "POST", "/v1/fetch", &auth, &by_reference("users#0000000000000000")).await;
    assert_eq!(never.status, 400, "{}", never.body);
    assert!(never.body.contains("unknown schema reference"), "{}", never.body);
    let mut a = user("a@x.dev", "A", Some(30), 1.0);
    a.insert(&db).await.unwrap();
    let err = user("a@x.dev", "B", None, 0.0).insert(&db).await.unwrap_err();
    assert!(
        matches!(&err, Error::UniqueViolation { table, column } if table == "users" && column == "email"),
        "{err:?}"
    );
    assert!(matches!(db.create_table::<User>().await.unwrap_err(), Error::TableExists(_)));
    // Raw SQL (`db.raw`, `db.raw_execute`): the SQL layer runs it, as `/v1/sql`
    // does, with PostgreSQL's `$1` parameters.
    let n: Vec<i64> =
        db.raw("SELECT count(*) FROM users WHERE email = $1", &[Value::Text("a@x.dev".into())]).await.unwrap();
    assert_eq!(n, vec![1]);
    let names: Vec<String> = db
        .raw(
            "SELECT name FROM users WHERE id NOT IN (SELECT jsonb_array_elements_text($1::jsonb)::bigint)",
            &[Value::Text("[]".into())],
        )
        .await
        .unwrap();
    assert_eq!(names, vec!["A".to_owned()]);
    assert_eq!(
        db.raw_execute(
            "UPDATE users SET name = $1 WHERE email = $2",
            &[Value::Text("A2".into()), Value::Text("a@x.dev".into())]
        )
        .await
        .unwrap(),
        1
    );
    assert!(db.raw::<i64>("SELECT nope FROM nowhere", &[]).await.is_err());
    // Inside a transaction: its writes, seen by it alone until the commit.
    let tx = db.begin().await.unwrap();
    let locked: Vec<i64> =
        tx.raw("SELECT id FROM users WHERE email = $1 FOR UPDATE", &[Value::Text("a@x.dev".into())]).await.unwrap();
    assert_eq!(locked.len(), 1);
    assert_eq!(
        tx.raw_execute("UPDATE users SET name = 'A3' WHERE id = $1", &[memoire_orm::Value::I64(locked[0])])
            .await
            .unwrap(),
        1
    );
    let seen: Vec<String> = tx.raw("SELECT name FROM users", &[]).await.unwrap();
    assert_eq!(seen, vec!["A3".to_owned()]);
    let outside: Vec<String> = db.raw("SELECT name FROM users", &[]).await.unwrap();
    assert_eq!(outside, vec!["A2".to_owned()]);
    tx.rollback().await.unwrap();
    let after: Vec<String> = db.raw("SELECT name FROM users", &[]).await.unwrap();
    assert_eq!(after, vec!["A2".to_owned()]);
    db.raw_execute("UPDATE users SET name = 'A' WHERE email = 'a@x.dev'", &[]).await.unwrap();

    // Batches: all or nothing.
    let b = user("b@x.dev", "B", None, 0.0);
    let dup = user("a@x.dev", "Dup", None, 0.0);
    let (bv, dv) = (b.to_values(), dup.to_values());
    let err = db
        .batch()
        .execute(Query::InsertRow { table: User::SCHEMA, values: &bv })
        .execute(Query::InsertRow { table: User::SCHEMA, values: &dv })
        .run()
        .await
        .unwrap_err();
    assert!(err.is_unique_violation());
    assert_eq!(User::count(&db).await.unwrap(), 1);
    let select = User::query().order_by(User::ID.asc());
    let results = db
        .batch()
        .execute(Query::InsertRow { table: User::SCHEMA, values: &bv })
        .fetch(Query::Select(select.ast()))
        .run()
        .await
        .unwrap();
    assert_eq!(results[0].rows_affected(), 1);
    assert_eq!(results[1].clone().decode::<User>().unwrap().len(), 2);

    // Interactive transactions: isolation and rollback.
    let tx = db.begin().await.unwrap();
    user("c@x.dev", "C", None, 0.0).insert(&tx).await.unwrap();
    assert_eq!(User::count(&tx).await.unwrap(), 3);
    assert_eq!(User::count(&db).await.unwrap(), 2, "not visible before commit");
    tx.rollback().await.unwrap();
    assert_eq!(User::count(&db).await.unwrap(), 2);

    // Branches through the protocol.
    let branch = db.create_branch("preview", None).await.unwrap();
    assert_eq!(branch.name, "preview");
    let preview = db.on_branch("preview");
    user("p@x.dev", "P", None, 0.0).insert(&preview).await.unwrap();
    assert_eq!(User::count(&preview).await.unwrap(), 3);
    assert_eq!(User::count(&db).await.unwrap(), 2);
    let tx = preview.begin().await.unwrap();
    User::delete_many().execute(&tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(User::count(&preview).await.unwrap(), 0);
    assert_eq!(User::count(&db).await.unwrap(), 2);
    assert_eq!(db.branches().await.unwrap().len(), 1);
    db.reset_branch("preview").await.unwrap();
    assert_eq!(User::count(&preview).await.unwrap(), 2);
    assert!(db.delete_branch("preview").await.unwrap());
    assert!(!db.delete_branch("preview").await.unwrap());
    assert!(matches!(User::count(&preview).await.unwrap_err(), Error::NotFound));

    // Authentication.
    assert!(unauthorized(&NuageDb::connect(&running.url(), "wrong").await.unwrap_err()));
    assert!(unauthorized(&NuageDb::builder(&running.url()).connect().await.unwrap_err()));
    running.shutdown().await;

    // Without a token on the server, the API is closed.
    let closed = Server::new(engine("e2e-closed"), ServerOptions::default()).listen("127.0.0.1:0").await.unwrap();
    assert!(unauthorized(&NuageDb::connect(&closed.url(), "anything").await.unwrap_err()));
    closed.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_api() {
    let site = |req: &nuage::SiteRequest| {
        (req.path == "/app").then(|| SiteResponse {
            status: 200,
            content_type: "text/html".into(),
            headers: vec![("cache-control".into(), "no-cache".into())],
            body: b"<h1>dashboard</h1>".to_vec(),
        })
    };
    let server = Server::new(engine("status"), ServerOptions { site: Some(Arc::new(site)), ..options() });
    let running = server.listen("127.0.0.1:0").await.unwrap();
    let addr = running.local_addr();
    let db = NuageDb::connect(&running.url(), "t0ken").await.unwrap();
    db.create_table::<common::User>().await.unwrap();
    server.engine().create_branch("preview-42", None).unwrap();

    let health = http(addr, "GET", "/api/health", &[], "").await;
    assert_eq!((health.status, health.body.as_str()), (200, r#"{"status":"ok"}"#));
    assert_eq!(http(addr, "GET", "/api/status", &[], "").await.status, 401);
    assert_eq!(http(addr, "GET", "/api/status/stream", &[], "").await.status, 401);
    // "Does my connection work?": a clear answer either way.
    let ping = http(addr, "GET", "/v1/ping", &[("authorization", "Bearer t0ken")], "").await;
    assert_eq!(ping.status, 200, "{}", ping.body);
    let ping: serde_json::Value = serde_json::from_str(&ping.body).unwrap();
    assert_eq!((ping["application"].as_str(), ping["access"].as_str()), (Some("nuage"), Some("read-write")));
    assert!(ping["message"].as_str().unwrap().starts_with("Connected to Nuage DB"));
    let none = http(addr, "GET", "/v1/ping", &[], "").await;
    assert!(none.status == 401 && none.body.contains("no token"), "{}", none.body);
    let wrong = http(addr, "GET", "/v1/ping", &[("authorization", "Bearer nope")], "").await;
    assert!(wrong.status == 401 && wrong.body.contains("opens no application"), "{}", wrong.body);
    // The description of the API, public, for code generators of every language.
    let openapi = http(addr, "GET", "/v1/openapi.json", &[], "").await;
    assert_eq!(openapi.status, 200);
    let spec: serde_json::Value = serde_json::from_str(&openapi.body).unwrap();
    assert!(spec["openapi"].as_str().unwrap().starts_with("3.") && spec["paths"]["/v1/sql"]["post"].is_object());
    assert_eq!(spec["info"]["version"], nuage::VERSION, "openapi.json follows the version");

    let full = http(addr, "GET", "/api/status", &[("authorization", "Bearer t0ken")], "").await;
    assert_eq!(full.status, 200);
    let j: serde_json::Value = serde_json::from_str(&full.body).unwrap();
    assert_eq!(j["name"], "Nuage DB");
    assert_eq!(j["version"], nuage::VERSION);
    assert_eq!(j["status"], "ok");
    for key in
        ["uptime_s", "started_at", "requests", "connections", "storage", "wal", "backup", "databases", "services"]
    {
        assert!(!j[key].is_null(), "missing {key}");
    }
    for key in ["per_s", "total", "errors_per_s", "p50_ms", "p99_ms"] {
        assert!(j["requests"][key].is_number(), "requests.{key}");
    }
    for key in ["bytes", "pages", "cache_bytes", "cache_capacity_bytes", "cache_hit_rate"] {
        assert!(j["storage"][key].is_number(), "storage.{key}");
    }
    assert!(j["requests"]["total"].as_u64().unwrap() >= 3);
    assert_eq!(j["databases"][0]["name"], "nuage");
    assert_eq!(j["databases"][0]["tables"], 1);
    assert_eq!(j["databases"][0]["branches"][0]["name"], "preview-42");
    assert_eq!(j["databases"][0]["branches"][0]["parent"], "nuage");
    assert_eq!(j["backup"]["enabled"], false);
    let services: Vec<&str> = j["services"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert_eq!(services, vec!["engine", "http", "wal", "backup", "compaction"]);

    // The site handler serves its pages; `/` falls back to the server's.
    let app = http(addr, "GET", "/app", &[], "").await;
    assert_eq!((app.status, app.body.as_str()), (200, "<h1>dashboard</h1>"));
    assert!(http(addr, "GET", "/", &[], "").await.body.contains("/admin"));

    // Server-Sent Events: at least two `status` events.
    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await.unwrap();
    tokio::spawn(conn);
    let req = Request::get("/api/status/stream")
        .header("host", addr.to_string())
        .header("authorization", "Bearer t0ken")
        .body(Full::new(Bytes::new()))
        .unwrap();
    let res = sender.send_request(req).await.unwrap();
    assert_eq!(res.headers()["content-type"], "text/event-stream");
    let mut body = res.into_body();
    let mut text = String::new();
    let mut statuses = 0;
    let mut logs = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while statuses < 2 || !text.contains("event: log") {
        let frame =
            tokio::time::timeout_at(deadline, body.frame()).await.expect("two events within 10 s").unwrap().unwrap();
        if let Some(data) = frame.data_ref() {
            text.push_str(&String::from_utf8_lossy(data));
        }
        statuses = text.matches("event: status\n").count();
        if statuses == 1 && logs == 0 {
            // An event to report in the next round.
            server.engine().create_branch("from-stream", None).unwrap();
            logs = 1;
        }
    }
    assert!(text.starts_with("retry: 2000\n\n"), "{text}");
    let first = text.split("event: status\ndata: ").nth(1).unwrap().split("\n\n").next().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(first).unwrap();
    assert_eq!(parsed["name"], "Nuage DB");
    assert!(text.contains("event: log\ndata: ") && text.contains("from-stream"), "{text}");
    drop(body);
    running.shutdown().await;

    // The public subset.
    let public = Server::new(engine("status-public"), ServerOptions { public_status: true, ..options() });
    let running = public.listen("127.0.0.1:0").await.unwrap();
    let j: serde_json::Value =
        serde_json::from_str(&http(running.local_addr(), "GET", "/api/status", &[], "").await.body).unwrap();
    assert_eq!(j["status"], "ok");
    assert!(j["uptime_s"].is_number() && j["started_at"].is_string());
    assert!(j.get("storage").is_none() && j.get("databases").is_none() && j.get("backup").is_none());
    assert!(j["services"][0].get("detail").is_none());
    running.shutdown().await;
}

/// A console client: keeps the cookies and reads the CSRF token of pages.
struct Console {
    addr: SocketAddr,
    cookies: HashMap<String, String>,
}

impl Console {
    async fn request(&mut self, method: &str, path: &str, form: &[(&str, &str)]) -> Http {
        let cookie = self.cookies.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; ");
        let body: String = form
            .iter()
            .map(|(k, v)| {
                format!(
                    "{k}={}",
                    v.bytes()
                        .map(|b| if b.is_ascii_alphanumeric() { (b as char).to_string() } else { format!("%{b:02X}") })
                        .collect::<String>()
                )
            })
            .collect::<Vec<_>>()
            .join("&");
        let mut headers = vec![("content-type", "application/x-www-form-urlencoded")];
        if !cookie.is_empty() {
            headers.push(("cookie", cookie.as_str()));
        }
        let res = http(self.addr, method, path, &headers, &body).await;
        for c in res.headers.get_all("set-cookie") {
            let c = c.to_str().unwrap();
            assert!(c.contains("HttpOnly") && c.contains("Secure") && c.contains("SameSite=Strict"), "{c}");
            let (k, v) = c.split(';').next().unwrap().split_once('=').unwrap();
            if v.is_empty() {
                self.cookies.remove(k);
            } else {
                self.cookies.insert(k.to_owned(), v.to_owned());
            }
        }
        res
    }
}

fn csrf(page: &str) -> String {
    page.split(r#"name="csrf" value=""#).nth(1).unwrap().split('"').next().unwrap().to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn console() {
    let server = Server::new(engine("console"), options());
    let running = server.listen("127.0.0.1:0").await.unwrap();
    let db = NuageDb::connect(&running.url(), "t0ken").await.unwrap();
    use common::{User, user};
    db.create_table::<User>().await.unwrap();
    for (e, n, a) in [("ada@x.dev", "Ada", 36), ("bob@x.dev", "Bob", 17), ("cy@x.dev", "Cy", 52)] {
        user(e, n, Some(a), 1.0).insert(&db).await.unwrap();
    }
    let mut c = Console { addr: running.local_addr(), cookies: HashMap::new() };

    // Login required.
    let res = c.request("GET", "/admin", &[]).await;
    assert_eq!((res.status, res.headers["location"].to_str().unwrap()), (303, "/admin/login?next=%2Fadmin"));
    assert_eq!(c.request("POST", "/admin/query", &[("sql", "DELETE FROM users")]).await.status, 401);
    // The status API too wants a session or the token.
    assert_eq!(c.request("GET", "/api/status", &[]).await.status, 401);

    // A login form without its CSRF token, then a wrong password.
    let login = c.request("GET", "/admin/login", &[]).await;
    let token = csrf(&login.body);
    assert_eq!(c.request("POST", "/admin/login", &[("password", "correct horse")]).await.status, 403);
    let login = c.request("GET", "/admin/login", &[]).await;
    let token2 = csrf(&login.body);
    assert_ne!(token, token2);
    assert_eq!(c.request("POST", "/admin/login", &[("csrf", &token2), ("password", "wrong")]).await.status, 401);
    let login = c.request("GET", "/admin/login", &[]).await;
    let res = c.request("POST", "/admin/login", &[("csrf", &csrf(&login.body)), ("password", "correct horse")]).await;
    assert_eq!(res.status, 303);
    assert!(c.cookies.contains_key("nuage_session"));

    let overview = c.request("GET", "/admin", &[]).await;
    assert_eq!(overview.status, 200);
    assert!(overview.body.contains("Applications") && overview.body.contains(r#"href="/admin/apps/nuage""#));
    let app = c.request("GET", "/admin/apps/nuage", &[]).await;
    assert!(app.body.contains("Size on disk") && app.body.contains("NUAGE_URL="), "{}", app.body);
    assert!(app.body.contains("NUAGE_PG_PORT=5432"), "PostgreSQL is off here: the console says how to turn it on");
    assert!(app.body.contains("✓ Connected: last connection"), "the console shows the last connection");
    // The language: EN, FR, ES or as the browser, like the theme.
    assert!(app.body.contains(r#"class="lg-auto""#) && app.body.contains(r#"class="lg-fr""#));
    c.request("GET", "/admin/apps/nuage?lang=fr", &[]).await;
    let fr = c.request("GET", "/admin/apps/nuage", &[]).await;
    assert!(fr.body.contains(r#"lang="fr""#) && !fr.body.contains(r#"data-lang-choice="auto" lang"#), "French, chosen");
    c.request("GET", "/admin/apps/nuage?lang=auto", &[]).await;
    let auto = c.request("GET", "/admin/apps/nuage", &[]).await;
    assert!(auto.body.contains(r#"data-lang-choice="auto""#), "as the browser again");
    // The theme: light, dark or as the device, kept in a cookie.
    assert!(
        app.body.contains(r#"class="th-dark""#) && !app.body.contains("<html data-theme="),
        "as the device by default"
    );
    let chosen = c.request("GET", "/admin/apps/nuage?theme=dark", &[]).await;
    assert_eq!(chosen.status, 303);
    let dark = c.request("GET", "/admin/apps/nuage", &[]).await;
    assert!(dark.body.contains(r#"<html data-theme="dark" "#), "the dark theme is kept");
    c.request("GET", "/admin/apps/nuage?theme=auto", &[]).await;
    assert!(
        !c.request("GET", "/admin/apps/nuage", &[]).await.body.contains("<html data-theme="),
        "back to the device's"
    );
    for language in [
        "JavaScript",
        "Python",
        "PHP",
        "Go",
        "Ruby",
        "Java",
        "C#",
        "Rust",
        "Swift",
        "Dart / Flutter",
        "Kotlin",
        "Elixir",
        "curl",
    ] {
        assert!(app.body.contains(&format!("<summary>{language}</summary>")), "{language}");
    }
    assert_eq!(c.request("GET", "/api/status", &[]).await.status, 200, "the session opens the status API");
    let csrf_token = csrf(&overview.body);
    // The pages of before the applications lead to the first one's.
    let moved = c.request("GET", "/admin/tables", &[]).await;
    assert_eq!((moved.status, moved.headers["location"].to_str().unwrap()), (303, "/admin/apps/nuage/tables"));
    let tables = c.request("GET", "/admin/apps/nuage/tables", &[]).await;
    assert!(tables.body.contains("users") && tables.body.contains(">3<"));
    let rows = c.request("GET", "/admin/apps/nuage/tables/users", &[]).await;
    assert!(rows.body.contains("ada@x.dev") && rows.body.contains("cy@x.dev"));

    // Forms without the CSRF token are refused.
    assert_eq!(c.request("POST", "/admin/query", &[("sql", "DELETE FROM users")]).await.status, 403);
    assert_eq!(
        c.request("POST", "/admin/query", &[("csrf", "forged"), ("sql", "DELETE FROM users")]).await.status,
        403
    );
    assert_eq!(User::count(&db).await.unwrap(), 3);

    // The query console: reads at once...
    let res = c
        .request(
            "POST",
            "/admin/query",
            &[("csrf", &csrf_token), ("sql", "SELECT name, age FROM users WHERE age >= 18 ORDER BY name")],
        )
        .await;
    assert_eq!(res.status, 200);
    assert!(
        res.body.contains("2 row(s)")
            && res.body.contains("Ada")
            && res.body.contains("Cy")
            && !res.body.contains(">Bob<")
    );
    // ...writes after a confirmation.
    let res =
        c.request("POST", "/admin/query", &[("csrf", &csrf_token), ("sql", "DELETE FROM users WHERE age < 18")]).await;
    assert!(res.body.contains("1 row(s) would be affected") && res.body.contains("Confirm"));
    assert_eq!(User::count(&db).await.unwrap(), 3, "nothing before the confirmation");
    let res = c
        .request(
            "POST",
            "/admin/query",
            &[("csrf", &csrf_token), ("sql", "DELETE FROM users WHERE age < 18"), ("confirm", "yes")],
        )
        .await;
    assert!(res.body.contains("1 row(s) affected"), "{}", res.body);
    assert_eq!(User::count(&db).await.unwrap(), 2);
    let res = c.request("POST", "/admin/query", &[("csrf", &csrf_token), ("sql", "SELECT * FROM nowhere")]).await;
    assert_eq!(res.status, 400);

    // Branches.
    let res = c
        .request("POST", "/admin/apps/nuage/branches", &[("csrf", &csrf_token), ("name", "review-1"), ("from", "")])
        .await;
    assert!(res.body.contains("Branch review-1 created"), "{}", res.body);
    assert_eq!(db.branches().await.unwrap().len(), 1);
    let res = c.request("POST", "/admin/apps/nuage/branches/review-1/delete", &[("csrf", &csrf_token)]).await;
    assert!(res.body.contains("Branch review-1 deleted"));
    assert!(db.branches().await.unwrap().is_empty());

    // Storage actions.
    let res = c.request("POST", "/admin/apps/nuage/storage/checkpoint", &[("csrf", &csrf_token)]).await;
    assert!(res.body.contains("Checkpoint at"));
    // (The old address still works for forms.)
    let res = c.request("POST", "/admin/storage/integrity", &[("csrf", &csrf_token)]).await;
    assert!(res.body.contains("checksum"), "{}", res.body);

    // Log out: the session is gone.
    c.request("POST", "/admin/logout", &[("csrf", &csrf_token)]).await;
    assert_eq!(c.request("GET", "/admin", &[]).await.status, 303);

    // After the login, back to where the site sent the user: only paths of
    // this site, never another origin (no open redirect).
    c.cookies.clear();
    let res = c.request("GET", "/admin?next=%2Ffr%2Fapp%2F", &[]).await;
    assert_eq!(res.headers["location"].to_str().unwrap(), "/admin/login?next=%2Ffr%2Fapp%2F");
    for (next, expected) in [
        ("/fr/app/", "/fr/app/"),
        ("/fr/app/?tab=wal", "/fr/app/?tab=wal"),
        ("https://evil.example/", "/admin"),
        ("//evil.example/", "/admin"),
        ("/\\evil.example/", "/admin"),
        ("javascript:alert(1)", "/admin"),
    ] {
        let login = c
            .request(
                "GET",
                &format!(
                    "/admin/login?next={}",
                    next.replace('/', "%2F").replace('?', "%3F").replace('=', "%3D").replace(':', "%3A")
                ),
                &[],
            )
            .await;
        let res = c
            .request(
                "POST",
                "/admin/login",
                &[("csrf", &csrf(&login.body)), ("password", "correct horse"), ("next", next)],
            )
            .await;
        assert_eq!((res.status, res.headers["location"].to_str().unwrap()), (303, expected), "next={next}");
    }
    // Already logged in: `/admin?next=` goes straight there.
    let res = c.request("GET", "/admin?next=%2Fapp", &[]).await;
    assert_eq!((res.status, res.headers["location"].to_str().unwrap()), (303, "/app"));
    // Too many failures from one address: refused for a while.
    let mut statuses = Vec::new();
    for _ in 0..6 {
        let login = c.request("GET", "/admin/login", &[]).await;
        statuses.push(
            c.request("POST", "/admin/login", &[("csrf", &csrf(&login.body)), ("password", "nope")]).await.status,
        );
    }
    assert_eq!(statuses.last(), Some(&429), "{statuses:?}");
    let login = c.request("GET", "/admin/login", &[]).await;
    let res = c.request("POST", "/admin/login", &[("csrf", &csrf(&login.body)), ("password", "correct horse")]).await;
    assert_eq!(res.status, 429, "even the right password waits");

    running.shutdown().await;

    // No password: no console.
    let off = Server::new(engine("console-off"), ServerOptions { admin_password: None, ..options() });
    let running = off.listen("127.0.0.1:0").await.unwrap();
    assert_eq!(http(running.local_addr(), "GET", "/admin", &[], "").await.status, 503);
    running.shutdown().await;
}

/// A minimal S3 server in memory (it checks that requests are signed, not
/// the signatures themselves).
async fn fake_s3() -> (SocketAddr, Arc<Mutex<HashMap<String, Vec<u8>>>>) {
    let objects: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::default();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let store = objects.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let store = store.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |req: Request<Incoming>| {
                    let store = store.clone();
                    async move {
                        // Signed in a header, or in the query (a presigned link).
                        let signed = req
                            .headers()
                            .get("authorization")
                            .is_some_and(|v| v.to_str().unwrap().starts_with("AWS4-HMAC-SHA256 Credential=key/"))
                            || req.uri().query().is_some_and(|q| {
                                q.contains("X-Amz-Credential=key%2F") && q.contains("&X-Amz-Signature=")
                            });
                        let path = req.uri().path().trim_start_matches("/bucket").trim_start_matches('/').to_owned();
                        let query = req.uri().query().unwrap_or_default().to_owned();
                        let method = req.method().clone();
                        let body = req.into_body().collect().await.unwrap().to_bytes().to_vec();
                        let (status, out) = if !signed {
                            (403, b"<Error><Code>AccessDenied</Code></Error>".to_vec())
                        } else {
                            let mut objects = store.lock().unwrap();
                            match method.as_str() {
                                "PUT" => {
                                    objects.insert(path, body);
                                    (200, Vec::new())
                                }
                                "GET" if path.is_empty() => {
                                    let prefix = query
                                        .split('&')
                                        .find_map(|kv| kv.strip_prefix("prefix="))
                                        .unwrap_or_default()
                                        .replace("%2F", "/");
                                    let mut keys: Vec<&String> =
                                        objects.keys().filter(|k| k.starts_with(&prefix)).collect();
                                    keys.sort();
                                    let xml: String = keys
                                        .iter()
                                        .map(|k| {
                                            format!(
                                                "<Contents><Key>{k}</Key><Size>{}</Size><ETag>&quot;e&quot;</ETag></Contents>",
                                                objects[*k].len()
                                            )
                                        })
                                        .collect();
                                    (
                                        200,
                                        format!(
                                            "<ListBucketResult>{xml}<IsTruncated>false</IsTruncated></ListBucketResult>"
                                        )
                                        .into_bytes(),
                                    )
                                }
                                "GET" => match objects.get(&path) {
                                    Some(b) => (200, b.clone()),
                                    None => (404, b"<Error><Code>NoSuchKey</Code></Error>".to_vec()),
                                },
                                "DELETE" => {
                                    objects.remove(&path);
                                    (204, Vec::new())
                                }
                                _ => (405, Vec::new()),
                            }
                        };
                        let mut res = Response::new(Full::new(Bytes::from(out)));
                        *res.status_mut() = hyper::StatusCode::from_u16(status).unwrap();
                        Ok::<_, std::convert::Infallible>(res)
                    }
                });
                let _ =
                    hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream), service).await;
            });
        }
    });
    (addr, objects)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s3_backup_and_restore() {
    use nuage::s3::{S3Options, S3Store};
    use nuage::{BackupOptions, BackupStore, Embedded};
    let (addr, objects) = fake_s3().await;
    let s3 = |secret: &str| {
        Arc::new(
            S3Store::new(S3Options {
                endpoint: format!("http://{addr}"),
                bucket: "bucket".into(),
                region: "us-east-1".into(),
                prefix: "prod/".into(),
                access_key_id: "key".into(),
                secret_access_key: secret.into(),
            })
            .unwrap(),
        )
    };
    let store = s3("secret");
    store.put("a/one", b"1").unwrap();
    store.put("a/two", b"22").unwrap();
    assert_eq!(store.get("a/two").unwrap(), Some(b"22".to_vec()));
    assert_eq!(store.get("a/none").unwrap(), None);
    assert_eq!(store.list("a/").unwrap(), vec!["a/one", "a/two"]);
    store.delete("a/one").unwrap();
    assert_eq!(store.list("a/").unwrap(), vec!["a/two"]);
    assert!(objects.lock().unwrap().contains_key("prod/a/two"));

    let src = temp_dir("s3-src");
    let dst = temp_dir("s3-dst");
    {
        let db = Embedded::open_with(
            &src,
            EngineOptions { backup: Some(BackupOptions::new(store.clone())), ..EngineOptions::default() },
        )
        .unwrap();
        db.create_table::<common::User>().await.unwrap();
        common::user("s3@x.dev", "S3", None, 0.0).insert(&db).await.unwrap();
        let engine = db.engine().clone();
        tokio::task::spawn_blocking(move || engine.backup_now(true)).await.unwrap().unwrap();
        common::user("s3b@x.dev", "S3b", None, 0.0).insert(&db).await.unwrap();
        let engine = db.engine().clone();
        tokio::task::spawn_blocking(move || {
            engine.checkpoint().unwrap();
            engine.backup_now(false)
        })
        .await
        .unwrap()
        .unwrap();
    }
    assert!(objects.lock().unwrap().contains_key("prod/manifest.json"));
    let db = Embedded::open_with(
        &dst,
        EngineOptions { backup: Some(BackupOptions::new(s3("secret"))), ..EngineOptions::default() },
    )
    .unwrap();
    assert!(db.engine().backup_status().restored);
    assert_eq!(common::User::count(&db).await.unwrap(), 2);
}

const NEEDLE: &str = "ONLY-IN-SHOP";

fn files_with(dir: &std::path::Path, needle: &str) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    entries
        .flatten()
        .map(|e| {
            let p = e.path();
            if p.is_dir() {
                files_with(&p, needle)
            } else {
                usize::from(std::fs::read(&p).unwrap().windows(needle.len()).any(|w| w == needle.as_bytes()))
            }
        })
        .sum()
}

fn token_in(page: &str) -> String {
    let at = page.find("<code>nuage_").expect("a token on the page") + "<code>".len();
    page[at..].split('<').next().unwrap().to_owned()
}

/// Several applications on one server: each its own encrypted database,
/// tokens and backups; the console makes, lists and deletes them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn applications() {
    use common::{User, user};
    let dir = temp_dir("apps");
    let backups = temp_dir("apps-backup");
    let open = |dir: &std::path::Path| {
        let mut backup = nuage::BackupOptions::new(Arc::new(nuage::DirectoryStore::new(&backups)));
        backup.snapshot_every = Duration::from_secs(3600);
        let options = EngineOptions {
            sync: false,
            encryption: Some(nuage::Encryption::from_key([7; 32])),
            backup: Some(backup),
            ..EngineOptions::default()
        };
        // The connections of the previous server let go of it in a moment.
        for _ in 0..100 {
            match Engine::open(dir, options.clone()) {
                Ok(e) => return e,
                Err(e) if e.to_string().contains("already open") => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => panic!("{e}"),
            }
        }
        panic!("{} stays open", dir.display())
    };
    let server = Server::new(open(&dir), options());
    let running = server.listen("127.0.0.1:0").await.unwrap();
    let addr = running.local_addr();
    let mut c = Console { addr, cookies: HashMap::new() };
    let login = c.request("GET", "/admin/login", &[]).await;
    c.request("POST", "/admin/login", &[("csrf", &csrf(&login.body)), ("password", "correct horse")]).await;
    let list = c.request("GET", "/admin", &[]).await;
    let token = csrf(&list.body);

    // A new application, from the console: its token is shown once.
    let made =
        c.request("POST", "/admin/apps", &[("csrf", &token), ("name", "shop"), ("description", "La boutique")]).await;
    assert!(made.body.contains("Application shop created"), "{}", made.body);
    let shop_token = token_in(&made.body);
    assert!(made.body.contains(&format!("NUAGE_TOKEN={shop_token}")));
    let refused = c.request("POST", "/admin/apps", &[("csrf", &token), ("name", "shop")]).await;
    assert!(refused.body.contains("already an application"));
    let refused = c.request("POST", "/admin/apps", &[("csrf", &token), ("name", "Not Valid")]).await;
    assert!(refused.body.contains("invalid application name"));
    let (_, blog_token) = server.create_app("blog", "").unwrap();
    // The first one first, then by name.
    assert_eq!(server.apps().iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["nuage", "blog", "shop"]);

    // Each token opens its own database, and only it.
    let first = NuageDb::connect(&running.url(), "t0ken").await.unwrap();
    let shop = NuageDb::connect(&running.url(), &shop_token).await.unwrap();
    let blog = NuageDb::connect(&running.url(), &blog_token).await.unwrap();
    shop.create_table::<User>().await.unwrap();
    for n in 0..5 {
        user(&format!("{NEEDLE}-{n}@x.dev"), NEEDLE, Some(30), 1.0).insert(&shop).await.unwrap();
    }
    blog.create_table::<User>().await.unwrap();
    user("only@blog.dev", "Blog", None, 1.0).insert(&blog).await.unwrap();
    assert_eq!(User::count(&shop).await.unwrap(), 5);
    assert_eq!(User::count(&blog).await.unwrap(), 1);
    assert!(User::count(&first).await.is_err(), "the first application has no `users`");
    assert!(dir.join("apps/shop/nuage.db").exists() && dir.join("apps/shop/encryption.json").exists());
    server.app_engine("shop").unwrap().checkpoint().unwrap();
    assert_eq!(files_with(&dir, NEEDLE), 0, "every application is encrypted");

    // A transaction belongs to its application.
    let tx = http(addr, "POST", "/v1/transactions", &[("authorization", &format!("Bearer {shop_token}"))], "").await;
    let id = serde_json::from_str::<serde_json::Value>(&tx.body).unwrap()["id"].as_str().unwrap().to_owned();
    let other =
        http(addr, "POST", &format!("/v1/transactions/{id}/commit"), &[("authorization", "Bearer t0ken")], "").await;
    assert!(other.status >= 400, "{}", other.body);
    let own = http(
        addr,
        "POST",
        &format!("/v1/transactions/{id}/commit"),
        &[("authorization", &format!("Bearer {shop_token}"))],
        "",
    )
    .await;
    assert_eq!(own.status, 200, "{}", own.body);

    // The status: a session sees every application, a token its own.
    let all: serde_json::Value = serde_json::from_str(&c.request("GET", "/api/status", &[]).await.body).unwrap();
    let names: Vec<&str> = all["databases"].as_array().unwrap().iter().map(|d| d["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["nuage", "blog", "shop"]);
    assert_eq!(all["applications"], 3);
    assert_eq!(all["encryption"]["enabled"], true);
    let shop_status = &all["databases"][2];
    assert_eq!((shop_status["rows"].as_i64(), shop_status["encrypted"].as_bool()), (Some(5), Some(true)));
    assert!(shop_status["requests"]["total"].as_u64().unwrap() >= 6);
    assert_eq!(shop_status["description"], "La boutique");
    let own: serde_json::Value = serde_json::from_str(
        &http(addr, "GET", "/api/status", &[("authorization", &format!("Bearer {shop_token}"))], "").await.body,
    )
    .unwrap();
    assert_eq!(own["databases"].as_array().unwrap().len(), 1);
    assert_eq!(own["databases"][0]["name"], "shop");

    // The console shows them all, and each one's pages.
    let list = c.request("GET", "/admin", &[]).await;
    for name in ["nuage", "shop", "blog", "La boutique", "encrypted"] {
        assert!(list.body.contains(name), "{name}");
    }
    let tables = c.request("GET", "/admin/apps/shop/tables", &[]).await;
    assert!(tables.body.contains("users") && tables.body.contains(">5<") && tables.body.contains("/admin/apps/blog"));
    let res =
        c.request("POST", "/admin/apps/blog/query", &[("csrf", &token), ("sql", "SELECT email FROM users")]).await;
    assert!(res.body.contains("only@blog.dev") && !res.body.contains(NEEDLE), "{}", res.body);

    // Tokens: a new one, and a revoked one opens nothing.
    let page = c.request("POST", "/admin/apps/shop/tokens", &[("csrf", &token)]).await;
    let second = token_in(&page.body);
    let first_id = server.apps().iter().find(|a| a.name == "shop").unwrap().tokens[0].id.clone();
    let res = c.request("POST", &format!("/admin/apps/shop/tokens/{first_id}/revoke"), &[("csrf", &token)]).await;
    assert!(res.body.contains("revoked"), "{}", res.body);
    assert_eq!(
        http(addr, "GET", "/v1/info", &[("authorization", &format!("Bearer {shop_token}"))], "").await.status,
        401
    );
    let info = http(addr, "GET", "/v1/info", &[("authorization", &format!("Bearer {second}"))], "").await;
    let info: serde_json::Value = serde_json::from_str(&info.body).unwrap();
    assert_eq!((info["application"].as_str(), info["encrypted"].as_bool()), (Some("shop"), Some(true)));

    // Backups: each application under its own prefix.
    for app in server.apps() {
        server.app_engine(&app.name).unwrap().backup_now(true).unwrap();
    }
    assert!(backups.join("apps/shop/manifest.json").exists() && backups.join("apps/blog/encryption.json").exists());
    assert_eq!(files_with(&backups, NEEDLE), 0);

    // A restart keeps them; an empty disk restores them all.
    running.shutdown().await;
    drop((server, first, shop, blog));
    for d in [dir.clone(), temp_dir("apps-restored")] {
        let server = Server::new(open(&d), options());
        assert_eq!(server.apps().len(), 3, "{}", d.display());
        let running = server.listen("127.0.0.1:0").await.unwrap();
        let shop = NuageDb::connect(&running.url(), &second).await.unwrap();
        assert_eq!(User::count(&shop).await.unwrap(), 5);
        running.shutdown().await;
    }

    // Deleting: only with its name typed, never the first application.
    let server = Server::new(open(&dir), options());
    let running = server.listen("127.0.0.1:0").await.unwrap();
    let mut c = Console { addr: running.local_addr(), cookies: HashMap::new() };
    let login = c.request("GET", "/admin/login", &[]).await;
    c.request("POST", "/admin/login", &[("csrf", &csrf(&login.body)), ("password", "correct horse")]).await;
    let token = csrf(&c.request("GET", "/admin", &[]).await.body);
    let res = c.request("POST", "/admin/apps/shop/delete", &[("csrf", &token), ("confirm", "nope")]).await;
    assert!(res.body.contains("nothing was deleted") && server.app_engine("shop").is_some());
    let res = c.request("POST", "/admin/apps/shop/delete", &[("csrf", &token), ("confirm", "shop")]).await;
    assert!(res.body.contains("Application shop deleted"), "{}", res.body);
    assert!(server.app_engine("shop").is_none() && !dir.join("apps/shop").exists());
    assert!(!backups.join("apps/shop/manifest.json").exists());
    assert!(server.delete_app("nuage").is_err());
    let settings = c.request("GET", "/admin/apps/nuage/settings", &[]).await;
    assert!(settings.body.contains("cannot be deleted"));
    assert_eq!(
        http(running.local_addr(), "GET", "/v1/info", &[("authorization", &format!("Bearer {second}"))], "")
            .await
            .status,
        401
    );
    running.shutdown().await;
    for d in [dir, backups, temp_dir("apps-restored")] {
        let _ = std::fs::remove_dir_all(&d);
    }
}

async fn raw(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, HeaderMap, Vec<u8>) {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await.unwrap();
    tokio::spawn(conn);
    let mut req = Request::builder().method(method).uri(path).header("host", addr.to_string());
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let res = sender.send_request(req.body(Full::new(Bytes::from(body.to_owned()))).unwrap()).await.unwrap();
    let (status, headers) = (res.status().as_u16(), res.headers().clone());
    (status, headers, res.into_body().collect().await.unwrap().to_bytes().to_vec())
}

/// Websites: SQL with parameters from any language, read-only tokens for
/// pages, CORS for the websites of an application, compressed answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn websites() {
    let server = Server::new(engine("websites"), options());
    let running = server.listen("127.0.0.1:0").await.unwrap();
    let addr = running.local_addr();
    let (_, write) = server.create_app("blog", "").unwrap();
    let read = server.new_app_read_token("blog").unwrap();
    let auth = |t: &str| format!("Bearer {t}");
    let (w, r) = (auth(&write), auth(&read));
    let sql = |sql: &str, params: serde_json::Value| serde_json::json!({ "sql": sql, "params": params }).to_string();
    let json = |body: &str| serde_json::from_str::<serde_json::Value>(body).unwrap();

    // Tables, rows and reads in SQL, values passed as parameters.
    let create = "CREATE TABLE posts (id BIGINT PRIMARY KEY AUTOINCREMENT, slug TEXT NOT NULL UNIQUE, category INT INDEX, title TEXT, published BOOLEAN, at TIMESTAMPTZ)";
    let res = http(addr, "POST", "/v1/sql", &[("authorization", &w)], &sql(create, serde_json::json!([]))).await;
    assert_eq!(res.status, 200, "{}", res.body);
    for n in 0..200 {
        let res = http(
            addr,
            "POST",
            "/v1/sql",
            &[("authorization", &w)],
            &sql(
                "INSERT INTO posts (slug, category, title, published, at) VALUES (?, ?, ?, ?, ?)",
                serde_json::json!([
                    format!("post-{n}"),
                    n % 4,
                    format!("Post {n} — l'été"),
                    n % 2 == 0,
                    "2026-11-13T10:00:00Z"
                ]),
            ),
        )
        .await;
        assert_eq!(res.status, 200, "{}", res.body);
        assert_eq!(json(&res.body)["last_insert_id"], n + 1);
    }
    let page = http(
        addr,
        "POST",
        "/v1/sql",
        &[("authorization", &r)],
        &sql(
            "SELECT id, title, published FROM posts WHERE category = ? ORDER BY id DESC LIMIT ?",
            serde_json::json!([1, 3]),
        ),
    )
    .await;
    assert_eq!(page.status, 200, "{}", page.body);
    let page = json(&page.body);
    assert_eq!(page["columns"], serde_json::json!(["id", "title", "published"]));
    assert_eq!(page["rows"][0], serde_json::json!({ "id": 198, "title": "Post 197 — l'été", "published": false }));
    assert_eq!(page["rows"].as_array().unwrap().len(), 3);
    // A parameter is never SQL.
    let none = http(
        addr,
        "POST",
        "/v1/sql",
        &[("authorization", &r)],
        &sql("SELECT * FROM posts WHERE slug = ?", serde_json::json!(["x' OR '1'='1"])),
    )
    .await;
    assert_eq!(json(&none.body)["rows"], serde_json::json!([]));
    let bad =
        http(addr, "POST", "/v1/sql", &[("authorization", &r)], &sql("SELECT * FROM nowhere", serde_json::json!([])))
            .await;
    assert_eq!(bad.status, 404, "{}", bad.body);

    // Several statements, one transaction; rows as arrays on request.
    let res = http(
        addr,
        "POST",
        "/v1/sql",
        &[("authorization", &w)],
        &serde_json::json!({
            "statements": [
                { "sql": "CREATE TABLE tags (id SERIAL PRIMARY KEY, post_id BIGINT NOT NULL, name TEXT NOT NULL)" },
                { "sql": "INSERT INTO tags (post_id, name) VALUES (?, ?), (?, ?)", "params": [1, "rust", 2, "web"] },
                { "sql": "SELECT p.title, group_concat(t.name, ',') AS tags FROM posts p JOIN tags t ON t.post_id = p.id GROUP BY p.id, p.title ORDER BY p.id" },
            ],
            "arrays": true,
        })
        .to_string(),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.body);
    let all = json(&res.body);
    assert_eq!(all["results"][1]["rows_affected"], 2);
    assert_eq!(all["results"][2]["rows"], serde_json::json!([["Post 0 — l'été", "rust"], ["Post 1 — l'été", "web"]]));
    // A failing statement undoes the batch.
    let res = http(
        addr,
        "POST",
        "/v1/sql",
        &[("authorization", &w)],
        &sql("INSERT INTO tags (post_id, name) VALUES (3, 'x'); INSERT INTO nowhere VALUES (1)", serde_json::json!([])),
    )
    .await;
    assert!(res.status >= 400);
    let count = http(
        addr,
        "POST",
        "/v1/sql",
        &[("authorization", &r)],
        &sql("SELECT count(*) AS n FROM tags", serde_json::json!([])),
    )
    .await;
    assert_eq!(json(&count.body)["rows"][0]["n"], 2);

    // A read-only token reads, and nothing else.
    for (path, body) in [
        ("/v1/sql", sql("DELETE FROM posts", serde_json::json!([]))),
        ("/v1/sql", sql("DROP TABLE posts", serde_json::json!([]))),
        ("/v1/execute", "{}".to_owned()),
        ("/v1/transactions", String::new()),
        ("/v1/branches", r#"{"name":"x"}"#.to_owned()),
    ] {
        let res = http(addr, "POST", path, &[("authorization", &r)], &body).await;
        assert_eq!(res.status, 403, "{path}: {}", res.body);
        assert!(res.body.contains("only reads"), "{}", res.body);
    }
    let reader = NuageDb::connect(&running.url(), &read).await.unwrap();
    assert!(reader.branches().await.is_ok(), "reads work with the client too");
    let n: Vec<i64> = reader.raw("SELECT count(*) FROM posts", &[]).await.unwrap();
    assert_eq!(n, vec![200], "raw reads too");
    assert!(reader.raw::<i64>("DELETE FROM posts RETURNING id", &[]).await.is_err(), "not raw writes");
    let count = http(
        addr,
        "POST",
        "/v1/sql",
        &[("authorization", &w)],
        &sql("SELECT COUNT(*) FROM posts", serde_json::json!([])),
    )
    .await;
    assert_eq!(json(&count.body)["rows"][0]["count"], 200);

    // CORS: only the application's websites, and the preflight is cached.
    let site = "https://blog.example";
    let pre = [
        ("origin", site),
        ("access-control-request-method", "POST"),
        ("access-control-request-headers", "authorization,content-type"),
    ];
    assert_eq!(http(addr, "OPTIONS", "/v1/sql", &pre, "").await.status, 403, "no website yet");
    let from_site = http(
        addr,
        "POST",
        "/v1/sql",
        &[("authorization", &r), ("origin", site)],
        &sql("SELECT id FROM posts LIMIT 1", serde_json::json!([])),
    )
    .await;
    assert_eq!(from_site.status, 403);
    assert_eq!(
        server.set_app_origins("blog", &["https://Blog.example/", "http://localhost:3000"]).unwrap(),
        [site, "http://localhost:3000"]
    );
    assert!(server.set_app_origins("blog", &["blog.example/path"]).is_err());
    let res = http(addr, "OPTIONS", "/v1/sql", &pre, "").await;
    assert_eq!(res.status, 204);
    assert_eq!(res.headers["access-control-allow-origin"], site);
    assert_eq!(res.headers["access-control-max-age"], "86400");
    assert!(res.headers["access-control-allow-headers"].to_str().unwrap().contains("authorization"));
    let from_site = http(
        addr,
        "POST",
        "/v1/sql",
        &[("authorization", &r), ("origin", site)],
        &sql("SELECT id FROM posts LIMIT 1", serde_json::json!([])),
    )
    .await;
    assert_eq!((from_site.status, from_site.headers["access-control-allow-origin"].to_str().unwrap()), (200, site));
    // Errors are readable by the page too; other websites get nothing.
    let denied = http(
        addr,
        "POST",
        "/v1/sql",
        &[("authorization", &r), ("origin", site)],
        &sql("DELETE FROM posts", serde_json::json!([])),
    )
    .await;
    assert_eq!((denied.status, denied.headers.get("access-control-allow-origin").is_some()), (403, true));
    let evil = http(
        addr,
        "POST",
        "/v1/sql",
        &[("authorization", &r), ("origin", "https://evil.example")],
        &sql("SELECT id FROM posts", serde_json::json!([])),
    )
    .await;
    assert_eq!((evil.status, evil.headers.get("access-control-allow-origin").is_none()), (403, true));
    // Another application's token from the blog's website: refused.
    let other = http(
        addr,
        "POST",
        "/v1/sql",
        &[("authorization", "Bearer t0ken"), ("origin", site)],
        &sql("SELECT 1 FROM posts", serde_json::json!([])),
    )
    .await;
    assert_eq!(other.status, 403);

    // Compressed for clients that accept it, plain for the others.
    let all = sql("SELECT * FROM posts", serde_json::json!([]));
    let (status, headers, body) =
        raw(addr, "POST", "/v1/sql", &[("authorization", &r), ("accept-encoding", "gzip, br")], &all).await;
    assert_eq!((status, headers["content-encoding"].to_str().unwrap()), (200, "gzip"));
    let mut plain = String::new();
    std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(body.as_slice()), &mut plain).unwrap();
    assert_eq!(json(&plain)["rows"].as_array().unwrap().len(), 200);
    let (_, headers, body2) = raw(addr, "POST", "/v1/sql", &[("authorization", &r)], &all).await;
    assert!(headers.get("content-encoding").is_none());
    assert_eq!(String::from_utf8(body2).unwrap(), plain);
    assert!(body.len() * 4 < plain.len(), "{} → {}", plain.len(), body.len());

    // The console shows the rights and the websites.
    let mut c = Console { addr, cookies: HashMap::new() };
    let login = c.request("GET", "/admin/login", &[]).await;
    c.request("POST", "/admin/login", &[("csrf", &csrf(&login.body)), ("password", "correct horse")]).await;
    let tokens = c.request("GET", "/admin/apps/blog/tokens", &[]).await;
    assert!(tokens.body.contains("read only") && tokens.body.contains("New read-only token"));
    let token = csrf(&tokens.body);
    let res = c
        .request(
            "POST",
            "/admin/apps/blog/origins",
            &[("csrf", &token), ("origins", "https://a.example\nhttps://b.example")],
        )
        .await;
    assert!(res.body.contains("https://a.example, https://b.example"), "{}", res.body);
    let overview = c.request("GET", "/admin/apps/blog", &[]).await;
    assert!(overview.body.contains("/v1/sql") && overview.body.contains("https://a.example"));
    let made = c.request("POST", "/admin/apps/blog/tokens", &[("csrf", &token), ("scope", "read")]).await;
    let new_read = token_in(&made.body);
    let res = http(
        addr,
        "POST",
        "/v1/sql",
        &[("authorization", &auth(&new_read))],
        &sql("DELETE FROM posts", serde_json::json!([])),
    )
    .await;
    assert_eq!(res.status, 403);
    running.shutdown().await;
}

/// The console in French and Spanish: every page, form and message (a text
/// without its translation stops a debug build: an error page here).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn console_in_three_languages() {
    let server = Server::new(engine("langs"), options());
    let running = server.listen("127.0.0.1:0").await.unwrap();
    let addr = running.local_addr();
    let (_, token) = server.create_app("shop", "La boutique").unwrap();
    let shop = NuageDb::connect(&running.url(), &token).await.unwrap();
    shop.create_table::<common::User>().await.unwrap();
    common::user("ada@x.dev", "Ada", Some(36), 1.0).insert(&shop).await.unwrap();

    // The browser's language, before any choice.
    let page = http(addr, "GET", "/admin/login", &[("accept-language", "es-ES,es;q=0.9,en;q=0.5")], "").await;
    assert!(
        page.body.contains(r#"<html data-lang-choice="auto" lang="es">"#) && page.body.contains("Contraseña"),
        "{}",
        page.body
    );

    for (code, words) in [
        (
            "fr",
            [
                "Applications",
                "Nouvelle application",
                "Jetons",
                "Réglages",
                "Sites web",
                "Mémoire par application",
                "trop peu : 16 Mio au moins",
                "pas une quantité de mémoire",
                "en vigueur dès maintenant",
                "Réglés au démarrage du serveur",
            ],
        ),
        (
            "es",
            [
                "Aplicaciones",
                "Nueva aplicación",
                "Tokens",
                "Ajustes",
                "Sitios web",
                "Memoria por aplicación",
                "es demasiado poco",
                "no es una cantidad de memoria",
                "en vigor desde ahora",
                "Definidos al arrancar el servidor",
            ],
        ),
    ] {
        let mut c = Console { addr, cookies: HashMap::new() };
        // `?lang=` sets the language, for every page after.
        let res = c.request("GET", &format!("/admin/login?lang={code}"), &[]).await;
        assert_eq!(res.status, 303);
        let set = res.headers["set-cookie"].to_str().unwrap();
        assert!(set.starts_with(&format!("nuage_lang={code}")), "{set}");
        c.cookies.insert("nuage_lang".into(), code.into());
        let login = c.request("GET", "/admin/login", &[]).await;
        assert!(login.body.contains(&format!(r#"<html lang="{code}">"#)));
        c.request("POST", "/admin/login", &[("csrf", &csrf(&login.body)), ("password", "correct horse")]).await;
        let list = c.request("GET", "/admin", &[]).await;
        let token = csrf(&list.body);
        let mut all = list.body.clone();
        for path in [
            "/admin/server",
            "/admin/settings",
            "/admin/apps/shop",
            "/admin/apps/shop/tables",
            "/admin/apps/shop/tables/users",
            "/admin/apps/shop/query",
            "/admin/apps/shop/branches",
            "/admin/apps/shop/storage",
            "/admin/apps/shop/tokens",
            "/admin/apps/shop/settings",
            "/admin/apps/nuage/settings",
            "/admin/apps/nowhere",
        ] {
            let res = c.request("GET", path, &[]).await;
            assert!(res.status < 500, "{code} {path}: {}", res.body);
            all.push_str(&res.body);
        }
        let f = |extra: &[(&'static str, &'static str)]| {
            let mut v = vec![("csrf", token.as_str())];
            v.extend_from_slice(extra);
            v
        };
        for (path, form) in [
            ("/admin/apps", f(&[("name", "blog"), ("description", "")])),
            ("/admin/apps", f(&[("name", "Bad Name")])),
            ("/admin/apps/shop/query", f(&[("sql", "SELECT name FROM users")])),
            ("/admin/apps/shop/query", f(&[("sql", "DELETE FROM users WHERE age > 100")])),
            ("/admin/apps/shop/query", f(&[("sql", "DELETE FROM users WHERE age > 100"), ("confirm", "yes")])),
            ("/admin/apps/shop/query", f(&[("sql", "SELECT nope")])),
            ("/admin/apps/shop/branches", f(&[("name", "preview"), ("from", "")])),
            ("/admin/apps/shop/branches/preview/reset", f(&[])),
            ("/admin/apps/shop/branches/preview/delete", f(&[])),
            ("/admin/apps/shop/branches/nope/delete", f(&[])),
            ("/admin/apps/shop/storage/checkpoint", f(&[])),
            ("/admin/apps/shop/storage/integrity", f(&[])),
            ("/admin/apps/shop/storage/compact", f(&[])),
            ("/admin/apps/shop/storage/backup", f(&[])),
            ("/admin/apps/shop/tokens", f(&[("scope", "read")])),
            ("/admin/apps/shop/tokens/zzzz/revoke", f(&[])),
            ("/admin/apps/shop/origins", f(&[("origins", "https://shop.example")])),
            ("/admin/apps/shop/origins", f(&[("origins", "")])),
            ("/admin/apps/shop/description", f(&[("description", "Boutique")])),
            ("/admin/apps/blog/delete", f(&[("confirm", "no")])),
            ("/admin/apps/blog/delete", f(&[("confirm", "blog")])),
            // The settings: saved, refused in the console's language, given back.
            (
                "/admin/settings/memory",
                f(&[("memory.total_mb", "auto"), ("memory.cache_mb", "64 Mio"), ("memory.kv_mb", "auto")]),
            ),
            ("/admin/settings/memory", f(&[("memory.cache_mb", "1")])),
            ("/admin/settings/memory", f(&[("memory.cache_mb", "64 MiB")])),
            (
                "/admin/settings/limits",
                f(&[("statement.memory_mb", "auto"), ("statement.seconds", "30"), ("postgres.connections", "50")]),
            ),
            ("/admin/settings/visibility", f(&[("status.public", "true")])),
            ("/admin/settings/visibility", f(&[])),
            ("/admin/settings/reset", f(&[("key", "memory.cache_mb")])),
            ("/admin/settings/reset", f(&[("key", "nothing")])),
            ("/admin/settings/nothing", f(&[])),
            ("/admin/apps/shop/memory", f(&[("cache_mb", "8 Mio"), ("kv_mb", "auto"), ("back", "settings")])),
            ("/admin/apps/shop/memory", f(&[("cache_mb", "beaucoup"), ("kv_mb", "")])),
            ("/admin/apps/shop/memory", f(&[("cache_mb", "auto"), ("kv_mb", "auto")])),
        ] {
            let res = c.request("POST", path, &form).await;
            assert!(res.status < 500, "{code} POST {path}: {}", res.body);
            all.push_str(&res.body);
        }
        for w in words {
            assert!(all.contains(w), "{code}: {w}");
        }
        for english in [
            "Log out",
            "New application",
            "Size on disk",
            "Save the websites",
            "Checkpoint now",
            "Save the memory",
            "Set when the server starts",
            "Nothing changed.",
            "is too little",
        ] {
            assert!(!all.contains(english), "{code}: `{english}` left in English");
        }
        c.request("POST", "/admin/logout", &[("csrf", &token)]).await;
    }
    running.shutdown().await;
}

/// The management API: `/api/apps` creates applications and their tokens with
/// `NUAGE_ADMIN_TOKEN`, and nothing else opens it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn management_api() {
    use common::{User, user};
    let admin = "a".repeat(nuage::server::MIN_ADMIN_TOKEN);
    let bearer = format!("Bearer {admin}");
    let auth: &[(&str, &str)] = &[("authorization", &bearer)];
    let json = |r: &Http| serde_json::from_str::<serde_json::Value>(&r.body).unwrap();

    // Without NUAGE_ADMIN_TOKEN, the API is off, and says how to turn it on.
    let off = Server::new(engine("manage-off"), options());
    let running = off.listen("127.0.0.1:0").await.unwrap();
    let r = http(running.local_addr(), "GET", "/api/apps", &[("authorization", "Bearer t0ken")], "").await;
    assert_eq!(r.status, 403, "{}", r.body);
    assert!(json(&r)["error"]["message"].as_str().unwrap().contains("NUAGE_ADMIN_TOKEN"));
    running.shutdown().await;

    let server = Server::new(engine("manage"), ServerOptions { admin_token: Some(admin.clone()), ..options() });
    let running = server.listen("127.0.0.1:0").await.unwrap();
    let addr = running.local_addr();

    // No token, a wrong one, an application's token: refused.
    for headers in [&[][..], &[("authorization", "Bearer nope")][..], &[("authorization", "Bearer t0ken")][..]] {
        let r = http(addr, "GET", "/api/apps", headers, "").await;
        assert_eq!(r.status, 401, "{headers:?}: {}", r.body);
        assert_eq!(json(&r)["error"]["kind"], "unauthorized");
    }

    // A new application: 201, its token once, and what to paste in its configuration.
    let made = http(addr, "POST", "/api/apps", auth, r#"{"name": "shop", "description": "La boutique"}"#).await;
    assert_eq!(made.status, 201, "{}", made.body);
    let made = json(&made);
    let token = made["token"].as_str().unwrap().to_owned();
    assert!(token.starts_with("nuage_"));
    assert_eq!(made["application"]["name"], "shop");
    assert_eq!(made["application"]["description"], "La boutique");
    assert_eq!(made["env"]["NUAGE_TOKEN"], token.as_str());
    assert_eq!(made["env"]["NUAGE_URL"], running.url().trim_end_matches('/'));
    assert_eq!(made["application"]["tokens"].as_array().unwrap().len(), 1);
    assert!(!made.to_string().contains("digest"));

    // The token opens the new database, and only it.
    let shop = NuageDb::connect(&running.url(), &token).await.unwrap();
    shop.create_table::<User>().await.unwrap();
    let mut ada = user("ada@shop.fr", "Ada", None, 1.0);
    ada.insert(&shop).await.unwrap();
    let first = NuageDb::connect(&running.url(), "t0ken").await.unwrap();
    assert!(first.create_table_if_not_exists::<User>().await.is_ok());
    assert_eq!(User::query().count(&first).await.unwrap(), 0);
    assert_eq!(User::query().count(&shop).await.unwrap(), 1);

    // A name already taken: what Memoire reads as `Error::UniqueViolation`.
    let taken = json(&http(addr, "POST", "/api/apps", auth, r#"{"name": "shop"}"#).await);
    assert_eq!(taken["error"]["kind"], "unique_violation");
    assert_eq!(
        (taken["error"]["table"].as_str(), taken["error"]["column"].as_str()),
        (Some("applications"), Some("name"))
    );

    // Refusals, with a reason.
    for (body, status, words) in [
        (r#"{"name": "shop"}"#, 409, "already an application"),
        (r#"{"name": "Not Valid"}"#, 400, "invalid application name"),
        (r#"{"description": "x"}"#, 400, "`name` is required"),
        (r#"{"name": 7}"#, 400, "`name` is a string"),
        ("[]", 400, "JSON object"),
        ("{", 400, "invalid JSON"),
    ] {
        let r = http(addr, "POST", "/api/apps", auth, body).await;
        assert_eq!(r.status, status, "{body}: {}", r.body);
        assert!(json(&r)["error"]["message"].as_str().unwrap().contains(words), "{body}: {}", r.body);
    }

    // The list, and one application.
    let list = json(&http(addr, "GET", "/api/apps", auth, "").await);
    let names: Vec<&str> =
        list["applications"].as_array().unwrap().iter().map(|a| a["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["nuage", "shop"]);
    assert_eq!(list["applications"][0]["first"], true);
    assert_eq!(list["applications"][0]["env_token"], true);
    let one = http(addr, "GET", "/api/apps/shop", auth, "").await;
    assert_eq!((one.status, json(&one)["name"].as_str()), (200, Some("shop")));
    assert_eq!(http(addr, "GET", "/api/apps/nothing", auth, "").await.status, 404);
    assert_eq!(http(addr, "DELETE", "/api/apps/shop", auth, "").await.status, 405);

    // More tokens: a read-only one reads and never writes; revoked, it opens nothing.
    let r = http(addr, "POST", "/api/apps/shop/tokens", auth, r#"{"read_only": true}"#).await;
    assert_eq!(r.status, 201, "{}", r.body);
    let reader = json(&r)["token"].as_str().unwrap().to_owned();
    assert_eq!(json(&r)["read_only"], true);
    let read = NuageDb::connect(&running.url(), &reader).await.unwrap();
    assert_eq!(User::query().count(&read).await.unwrap(), 1);
    assert!(user("bob@shop.fr", "Bob", None, 2.0).insert(&read).await.is_err());
    let r = http(addr, "POST", "/api/apps/shop/tokens", auth, "").await;
    assert_eq!((r.status, json(&r)["read_only"].as_bool()), (201, Some(false)));
    let tokens = json(&http(addr, "GET", "/api/apps/shop", auth, "").await)["tokens"].clone();
    assert_eq!(tokens.as_array().unwrap().len(), 3);
    let id =
        tokens.as_array().unwrap().iter().find(|t| t["read_only"] == true).unwrap()["id"].as_str().unwrap().to_owned();
    assert!(reader.contains(&id));
    let gone = http(addr, "DELETE", &format!("/api/apps/shop/tokens/{id}"), auth, "").await;
    assert_eq!(gone.status, 200, "{}", gone.body);
    assert_eq!(http(addr, "DELETE", &format!("/api/apps/shop/tokens/{id}"), auth, "").await.status, 404);
    let r = http(addr, "GET", "/v1/ping", &[("authorization", &format!("Bearer {reader}"))], "").await;
    assert_eq!(r.status, 401);
    assert_eq!(http(addr, "POST", "/api/apps/nothing/tokens", auth, "").await.status, 404);

    // `nuage apps create` and `nuage apps list`, over the same API.
    let lines = nuage::keepawake::create_app(&running.url(), &admin, "blog", "Le blog").await.unwrap();
    let blog_token = lines.lines().find_map(|l| l.strip_prefix("NUAGE_TOKEN=")).unwrap().to_owned();
    assert!(lines.starts_with(&format!("NUAGE_URL={}", running.url().trim_end_matches('/'))), "{lines}");
    assert!(NuageDb::connect(&running.url(), &blog_token).await.is_ok());
    let listed = nuage::keepawake::list_apps(&running.url(), &admin).await.unwrap();
    assert!(listed.contains("blog\t1 token(s)\tLe blog"), "{listed}");
    assert!(listed.starts_with("nuage\t1 token(s)"), "NUAGE_TOKEN counts: {listed}");
    let refused = nuage::keepawake::create_app(&running.url(), "wrong", "x", "").await.unwrap_err();
    assert!(refused.contains("NUAGE_ADMIN_TOKEN"), "{refused}");
    let twice = nuage::keepawake::create_app(&running.url(), &admin, "blog", "").await.unwrap_err();
    assert!(twice.contains("already an application"), "{twice}");

    // Described for code generators, with a security scheme of its own.
    let spec: serde_json::Value =
        serde_json::from_str(&http(addr, "GET", "/v1/openapi.json", &[], "").await.body).unwrap();
    for path in ["/api/apps", "/api/apps/{name}", "/api/apps/{name}/tokens", "/api/apps/{name}/tokens/{id}"] {
        assert!(spec["paths"][path].is_object(), "{path}");
    }
    assert_eq!(spec["paths"]["/api/apps"]["post"]["security"][0]["adminToken"], serde_json::json!([]));
    running.shutdown().await;
}

/// `/v1/files`: each application's files in a bucket, reached through signed links.
#[tokio::test]
async fn files_through_signed_links() {
    let json = |r: &Http| serde_json::from_str::<serde_json::Value>(&r.body).unwrap();

    // Without a bucket, the API says how to set one.
    let off = Server::new(engine("files-off"), options());
    let running = off.listen("127.0.0.1:0").await.unwrap();
    let r = http(running.local_addr(), "GET", "/v1/files", &[("authorization", "Bearer t0ken")], "").await;
    assert_eq!(r.status, 501, "{}", r.body);
    assert!(json(&r)["error"]["message"].as_str().unwrap().contains("NUAGE_FILES_S3_BUCKET"));
    running.shutdown().await;

    let (s3, objects) = fake_s3().await;
    let files = nuage::server::FileStore::new(
        nuage::s3::S3Options {
            endpoint: format!("http://{s3}"),
            bucket: "bucket".into(),
            region: "auto".into(),
            prefix: "files/".into(),
            access_key_id: "key".into(),
            secret_access_key: "secret".into(),
        },
        600,
    )
    .unwrap();
    let server = Server::new(engine("files"), ServerOptions { files: Some(Arc::new(files)), ..options() });
    let (_, token) = server.create_app("maps", "").unwrap();
    let read = server.new_app_read_only_token("maps").unwrap();
    let (_, other) = server.create_app("other", "").unwrap();
    let running = server.listen("127.0.0.1:0").await.unwrap();
    let addr = running.local_addr();
    let write = format!("Bearer {token}");
    let write: &[(&str, &str)] = &[("authorization", &write)];
    let read = format!("Bearer {read}");
    let read: &[(&str, &str)] = &[("authorization", &read)];
    let other = format!("Bearer {other}");
    let other: &[(&str, &str)] = &[("authorization", &other)];
    // A link of the fake bucket: its path and query, on `s3`.
    let on_s3 = |url: &str| url.strip_prefix(&format!("http://{s3}")).unwrap().to_owned();

    let info = http(addr, "GET", "/v1/info", read, "").await;
    assert_eq!(json(&info)["files"], true);

    // An upload link, then the bytes straight to the bucket.
    let link = http(addr, "POST", "/v1/files/regions/fr.tar", write, "").await;
    assert_eq!(link.status, 200, "{}", link.body);
    let link = json(&link);
    assert_eq!(link["method"], "PUT");
    let url = link["url"].as_str().unwrap();
    assert!(url.contains("/bucket/files/maps/regions/fr.tar?") && url.contains("X-Amz-Expires=600"), "{url}");
    let put = http(s3, "PUT", &on_s3(url), &[], "des tuiles").await;
    assert_eq!(put.status, 200, "{}", put.body);
    assert_eq!(objects.lock().unwrap()["files/maps/regions/fr.tar"], b"des tuiles");

    // A read-only token lists, and downloads through a redirect.
    let list = http(addr, "GET", "/v1/files", read, "").await;
    assert_eq!(list.status, 200, "{}", list.body);
    let list = json(&list);
    assert_eq!(list["count"], 1);
    assert_eq!(list["bytes"], 10);
    assert_eq!(list["files"][0]["name"], "regions/fr.tar");
    assert_eq!(list["files"][0]["size"], 10);
    assert_eq!(list["files"][0]["etag"], "e");
    assert_eq!(json(&http(addr, "GET", "/v1/files?prefix=regions%2Ff", read, "").await)["count"], 1);
    assert_eq!(json(&http(addr, "GET", "/v1/files?prefix=routes", read, "").await)["count"], 0);
    let get = http(addr, "GET", "/v1/files/regions/fr.tar", read, "").await;
    assert_eq!(get.status, 307, "{}", get.body);
    let location = get.headers["location"].to_str().unwrap().to_owned();
    assert_eq!(http(s3, "GET", &on_s3(&location), &[], "").await.body, "des tuiles");
    let as_json = json(&http(addr, "GET", "/v1/files/regions/fr.tar?link", read, "").await);
    assert!(as_json["url"].as_str().unwrap().contains("X-Amz-Signature="));
    assert!(as_json["expires_at"].as_str().unwrap().ends_with('Z'));
    let named = json(&http(addr, "GET", "/v1/files/regions/fr.tar?link&download=france.tar", read, "").await);
    assert!(named["url"].as_str().unwrap().contains("response-content-disposition=attachment"), "{named}");

    // A read-only token neither uploads nor deletes; bad names are refused.
    assert_eq!(http(addr, "POST", "/v1/files/x", read, "").await.status, 403);
    assert_eq!(http(addr, "DELETE", "/v1/files/regions/fr.tar", read, "").await.status, 403);
    for bad in ["/v1/files/a%20b", "/v1/files/../other/x", "/v1/files/a//b"] {
        let r = http(addr, "POST", bad, write, "").await;
        assert_eq!(r.status, 400, "{bad}: {}", r.body);
    }
    // No token: no files.
    assert_eq!(http(addr, "GET", "/v1/files", &[], "").await.status, 401);

    // The live status says what the application keeps: counted in the
    // background, so the first answer may come before the count.
    let mut counted = serde_json::Value::Null;
    for _ in 0..100 {
        let status = json(&http(addr, "GET", "/api/status", write, "").await);
        counted = status["databases"][0]["files"].clone();
        if !counted.is_null() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(counted, serde_json::json!({ "count": 1, "bytes": 10 }));

    // Another application sees none of them.
    assert_eq!(json(&http(addr, "GET", "/v1/files", other, "").await)["count"], 0);
    assert_eq!(json(&http(addr, "GET", "/v1/files?prefix=..%2Fmaps", other, "").await)["count"], 0);

    // Deleted.
    let gone = http(addr, "DELETE", "/v1/files/regions/fr.tar", write, "").await;
    assert_eq!(gone.status, 200, "{}", gone.body);
    assert!(objects.lock().unwrap().is_empty());
    assert_eq!(json(&http(addr, "GET", "/v1/files", read, "").await)["count"], 0);
    running.shutdown().await;
}

/// `/v1/kv` and the key-value port: the key-value store of each application.
#[tokio::test]
async fn key_value_store() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let json = |r: &Http| serde_json::from_str::<serde_json::Value>(&r.body).unwrap();
    let server = Server::new(engine("kv"), ServerOptions { kv_bytes: 1 << 20, ..options() });
    let (_, token) = server.create_app("shop", "").unwrap();
    let read = server.new_app_read_only_token("shop").unwrap();
    let running = server.listen("127.0.0.1:0").await.unwrap();
    let port = server.listen_kv("127.0.0.1:0", nuage::KvOptions::default()).await.unwrap();
    let addr = running.local_addr();
    let write = format!("Bearer {token}");
    let write: &[(&str, &str)] = &[("authorization", &write)];
    let read_header = format!("Bearer {read}");
    let read_only: &[(&str, &str)] = &[("authorization", &read_header)];
    let first: &[(&str, &str)] = &[("authorization", "Bearer t0ken")];

    assert_eq!(json(&http(addr, "GET", "/v1/info", write, "").await)["kv"], true);

    // A value, as it was written, with its expiry.
    let set = http(addr, "PUT", "/v1/kv/session%3A42?ttl=60", write, "ada\n\u{e9}").await;
    assert_eq!(set.status, 200, "{}", set.body);
    assert_eq!(json(&set)["set"], true);
    let got = http(addr, "GET", "/v1/kv/session:42", read_only, "").await;
    assert_eq!((got.status, got.body.as_str()), (200, "ada\n\u{e9}"));
    let ttl: u64 = got.headers["nuage-ttl-ms"].to_str().unwrap().parse().unwrap();
    assert!(ttl > 50_000 && ttl <= 60_000, "{ttl}");
    assert_eq!(http(addr, "GET", "/v1/kv/nope", read_only, "").await.status, 404);

    // A lock, a counter with its window, an expiry changed.
    assert_eq!(json(&http(addr, "PUT", "/v1/kv/lock?nx&ttl=0.5", write, "me").await)["set"], true);
    assert_eq!(json(&http(addr, "PUT", "/v1/kv/lock?nx&ttl=0.5", write, "you").await)["set"], false);
    assert_eq!(json(&http(addr, "POST", "/v1/kv/hits?incr&ttl=60", write, "").await)["value"], 1);
    assert_eq!(json(&http(addr, "POST", "/v1/kv/hits?incr=9", write, "").await)["value"], 10);
    assert_eq!(http(addr, "POST", "/v1/kv/session:42?incr", write, "").await.status, 400);
    assert_eq!(json(&http(addr, "POST", "/v1/kv/session:42?persist", write, "").await)["set"], true);
    assert!(!http(addr, "GET", "/v1/kv/session:42", write, "").await.headers.contains_key("nuage-ttl-ms"));
    assert_eq!(json(&http(addr, "POST", "/v1/kv/session:42?expire=30", write, "").await)["set"], true);
    assert_eq!(http(addr, "PUT", "/v1/kv/k?ttl=never", write, "v").await.status, 400);

    // Commands as JSON: one, or several in one round trip.
    let one = http(addr, "POST", "/v1/kv", write, r#"["SET", "greeting", "bonjour", "EX", 60]"#).await;
    assert_eq!(json(&one)["result"], "OK", "{}", one.body);
    let many = http(
        addr,
        "POST",
        "/v1/kv",
        write,
        r#"[["GET", "greeting"], ["INCRBY", "hits", 5], ["TTL", "greeting"], ["GET", "nope"], ["LPUSH", "l", "x"]]"#,
    )
    .await;
    let results = &json(&many)["results"];
    assert_eq!(results[0], "bonjour");
    assert_eq!(results[1], 15);
    assert_eq!(results[2], 60);
    assert!(results[3].is_null());
    assert!(results[4]["error"].as_str().unwrap().contains("unknown command"));
    assert_eq!(http(addr, "POST", "/v1/kv", write, r#"["GET"]"#).await.status, 400);
    assert_eq!(http(addr, "POST", "/v1/kv", write, r#"{"commands": "GET"}"#).await.status, 400);

    // The keys and the figures.
    let list = json(&http(addr, "GET", "/v1/kv?prefix=s", read_only, "").await);
    assert_eq!(list["count"], 1);
    assert_eq!(list["keys"][0]["key"], "session:42");
    assert_eq!(list["keys"][0]["size"], 6);
    assert_eq!(list["stats"]["keys"], 4);
    assert_eq!(list["stats"]["max_bytes"], 1 << 20);

    // A read-only token reads, and nothing else; no token, nothing.
    assert_eq!(http(addr, "PUT", "/v1/kv/k", read_only, "v").await.status, 403);
    assert_eq!(http(addr, "DELETE", "/v1/kv/hits", read_only, "").await.status, 403);
    assert_eq!(http(addr, "POST", "/v1/kv/hits?incr", read_only, "").await.status, 403);
    assert_eq!(http(addr, "POST", "/v1/kv", read_only, r#"["DEL", "hits"]"#).await.status, 403);
    assert_eq!(json(&http(addr, "POST", "/v1/kv", read_only, r#"["GET", "hits"]"#).await)["result"], "15");
    assert_eq!(http(addr, "GET", "/v1/kv/hits", &[], "").await.status, 401);

    // Another application has its own store.
    assert_eq!(http(addr, "GET", "/v1/kv/hits", first, "").await.status, 404);
    assert_eq!(json(&http(addr, "GET", "/v1/kv", first, "").await)["count"], 0);

    // The key-value port (RESP), on the same store: the token is the password.
    let mut conn = TcpStream::connect(port.local_addr()).await.unwrap();
    let say = async |conn: &mut TcpStream, command: &[&str]| -> String {
        let mut wire = format!("*{}\r\n", command.len());
        for arg in command {
            wire.push_str(&format!("${}\r\n{arg}\r\n", arg.len()));
        }
        conn.write_all(wire.as_bytes()).await.unwrap();
        let mut buf = vec![0u8; 4096];
        let n = conn.read(&mut buf).await.unwrap();
        String::from_utf8_lossy(&buf[..n]).into_owned()
    };
    assert!(say(&mut conn, &["GET", "hits"]).await.starts_with("-NOAUTH"));
    assert!(say(&mut conn, &["AUTH", "nope"]).await.starts_with("-WRONGPASS"));
    assert_eq!(say(&mut conn, &["AUTH", "default", &token]).await, "+OK\r\n");
    assert_eq!(say(&mut conn, &["PING"]).await, "+PONG\r\n");
    assert_eq!(say(&mut conn, &["GET", "hits"]).await, "$2\r\n15\r\n");
    assert_eq!(say(&mut conn, &["SET", "from-the-port", "1", "EX", "30"]).await, "+OK\r\n");
    assert_eq!(say(&mut conn, &["SELECT", "0"]).await, "+OK\r\n");
    assert!(say(&mut conn, &["SELECT", "1"]).await.starts_with("-ERR"));
    assert_eq!(say(&mut conn, &["CLIENT", "SETNAME", "essai"]).await, "+OK\r\n");
    assert!(say(&mut conn, &["HELLO", "4"]).await.starts_with("-NOPROTO"));
    assert!(say(&mut conn, &["HELLO", "2"]).await.starts_with("*14\r\n$6\r\nserver\r\n"));
    assert_eq!(say(&mut conn, &["GET", "nope"]).await, "$-1\r\n");
    // RESP3: a map, and its own "nothing".
    assert!(say(&mut conn, &["HELLO", "3"]).await.starts_with("%7\r\n$6\r\nserver\r\n"));
    assert_eq!(say(&mut conn, &["GET", "nope"]).await, "_\r\n");
    assert!(say(&mut conn, &["HELLO", "2"]).await.starts_with("*14\r\n"));
    // A pipeline: two commands sent together, answered together.
    conn.write_all(b"*2\r\n$4\r\nINCR\r\n$4\r\nhits\r\nGET from-the-port\r\n").await.unwrap();
    let mut answer = String::new();
    while !answer.ends_with("$1\r\n1\r\n") {
        let mut buf = [0u8; 256];
        let n = conn.read(&mut buf).await.unwrap();
        assert!(n > 0, "{answer:?}");
        answer.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    assert_eq!(answer, ":16\r\n$1\r\n1\r\n");
    // A transaction: queued, then run as one.
    assert!(say(&mut conn, &["EXEC"]).await.starts_with("-ERR EXEC without MULTI"));
    assert_eq!(say(&mut conn, &["MULTI"]).await, "+OK\r\n");
    assert_eq!(say(&mut conn, &["INCR", "hits"]).await, "+QUEUED\r\n");
    assert_eq!(say(&mut conn, &["GET", "hits"]).await, "+QUEUED\r\n");
    assert_eq!(http(addr, "GET", "/v1/kv/hits", read_only, "").await.body, "16", "not run before EXEC");
    assert_eq!(say(&mut conn, &["EXEC"]).await, "*2\r\n:17\r\n$2\r\n17\r\n");
    assert_eq!(say(&mut conn, &["MULTI"]).await, "+OK\r\n");
    assert_eq!(say(&mut conn, &["DECR", "hits"]).await, "+QUEUED\r\n");
    assert_eq!(say(&mut conn, &["DISCARD"]).await, "+OK\r\n");
    assert_eq!(say(&mut conn, &["DECR", "hits"]).await, ":16\r\n");
    assert_eq!(say(&mut conn, &["QUIT"]).await, "+OK\r\n");
    // What the port wrote, HTTP reads.
    assert_eq!(http(addr, "GET", "/v1/kv/from-the-port", read_only, "").await.body, "1");

    // A read-only token over the port; the HELLO that signs in.
    let mut conn = TcpStream::connect(port.local_addr()).await.unwrap();
    assert!(say(&mut conn, &["HELLO", "2", "AUTH", "default", &read]).await.starts_with("*14\r\n"));
    assert_eq!(say(&mut conn, &["GET", "hits"]).await, "$2\r\n16\r\n");
    assert!(say(&mut conn, &["DEL", "hits"]).await.starts_with("-READONLY"));
    drop(conn);

    // Memoire's client, `NuageKv`, on the same store.
    {
        use memoire_orm::NuageKv;
        use std::time::Duration;
        let kv = NuageKv::new(&running.url(), &token).unwrap();
        assert_eq!(kv.get_string("hits").await.unwrap().as_deref(), Some("16"));
        assert_eq!(kv.get("nope").await.unwrap(), None);
        kv.set("a/b c?d", [0u8, 159, 255]).await.unwrap();
        assert_eq!(kv.get("a/b c?d").await.unwrap(), Some(vec![0, 159, 255]));
        assert!(kv.get_string("a/b c?d").await.is_err(), "not text");
        kv.set_for("short", "lived", Duration::from_millis(40)).await.unwrap();
        let left = kv.ttl("short").await.unwrap().unwrap();
        assert!(left <= Duration::from_millis(40), "{left:?}");
        assert_eq!(kv.ttl("nope").await.unwrap(), None);
        assert!(kv.set_if_absent("job", "me", Some(Duration::from_secs(30))).await.unwrap());
        assert!(!kv.set_if_absent("job", "you", None).await.unwrap());
        assert_eq!(kv.incr("visits").await.unwrap(), 1);
        assert_eq!(kv.incr_by("visits", 41, Some(Duration::from_secs(60))).await.unwrap(), 42);
        assert_eq!(kv.ttl("visits").await.unwrap(), None, "the window is for a counter the call creates");
        assert_eq!(kv.incr_by("window", 1, Some(Duration::from_secs(60))).await.unwrap(), 1);
        assert!(kv.ttl("window").await.unwrap().is_some());
        assert!(kv.incr("job").await.is_err(), "not a number");
        assert!(kv.expire("visits", Duration::from_secs(60)).await.unwrap());
        assert!(kv.persist("visits").await.unwrap());
        assert!(!kv.expire("nope", Duration::from_secs(60)).await.unwrap());
        assert!(kv.exists("visits").await.unwrap() && !kv.exists("nope").await.unwrap());
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(kv.get("short").await.unwrap(), None, "expired");
        assert_eq!(kv.keys("vi").await.unwrap(), ["visits"]);
        assert_eq!(kv.command(&["STRLEN", "job"]).await.unwrap(), 2);
        let answers = kv.pipeline(&[vec!["SET", "p", "1"], vec!["INCR", "p"], vec!["NOPE"]]).await.unwrap();
        assert_eq!((answers[0].as_str(), answers[1].as_i64()), (Some("OK"), Some(2)));
        assert!(answers[2]["error"].is_string());
        // JSON values, and a cache in one call.
        kv.set_json("user", &serde_json::json!({ "name": "Ada" }), None).await.unwrap();
        assert_eq!(kv.get_json::<serde_json::Value>("user").await.unwrap().unwrap()["name"], "Ada");
        let mut computed = 0;
        for _ in 0..3 {
            let report: Vec<u32> = kv
                .remember("report", Duration::from_secs(60), || {
                    computed += 1;
                    async { Ok(vec![1, 2, 3]) }
                })
                .await
                .unwrap();
            assert_eq!(report, [1, 2, 3]);
        }
        assert_eq!(computed, 1, "computed once, then read from the store");
        // A read-only token reads; the database client gives the same store.
        let reader = NuageKv::new(&running.url(), &read).unwrap();
        assert_eq!(reader.get_string("visits").await.unwrap().as_deref(), Some("42"));
        assert!(reader.set("visits", "0").await.is_err());
        let db = NuageDb::connect(&running.url(), &token).await.unwrap();
        assert_eq!(db.kv().get_string("visits").await.unwrap().as_deref(), Some("42"));
        for key in ["a/b c?d", "job", "visits", "window", "p", "user", "report"] {
            assert!(kv.delete(key).await.unwrap(), "{key}");
        }
        assert!(!kv.delete("job").await.unwrap());
    }

    // The status says what the store holds.
    let status = json(&http(addr, "GET", "/api/status", write, "").await);
    assert!(status.to_string().contains("\"kv\""), "{status}");

    // Everything deleted.
    assert_eq!(json(&http(addr, "DELETE", "/v1/kv/hits", write, "").await)["deleted"], 1);
    assert_eq!(json(&http(addr, "DELETE", "/v1/kv", write, "").await)["deleted"], 4);
    assert_eq!(json(&http(addr, "GET", "/v1/kv", write, "").await)["count"], 0);
    port.shutdown().await;
    running.shutdown().await;
}

#[tokio::test]
async fn applications_share_one_cache_budget() {
    let options = ServerOptions { cache_budget: Some(48 << 20), ..options() };
    let server = Server::new(engine("cache-budget"), options);
    let capacity = |name: &str| server.app_engine(name).unwrap().cache_capacity();
    assert_eq!(capacity("nuage"), 48 << 20, "alone, it has the budget");
    server.create_app("shop", "").unwrap();
    server.create_app("blog", "").unwrap();
    // Three applications: a third each, and never more than the budget in all.
    let total: usize = ["nuage", "shop", "blog"].iter().map(|n| capacity(n)).sum();
    assert_eq!(capacity("shop"), (48 << 20) / 3 / 16 * 16);
    assert!(total <= 48 << 20, "{total}");
    server.delete_app("blog").unwrap();
    assert_eq!(capacity("shop"), 24 << 20, "its part goes back to the others");
    // The status says what the memory is, and what the caches may hold.
    let status = server.status();
    let memory = &status["memory"];
    assert_eq!(memory["cache_budget_bytes"], 48 << 20);
    assert_eq!(memory["cache_per_app_bytes"], 24 << 20);
    assert_eq!(status["storage"]["cache_capacity_bytes"], 48 << 20);
    if cfg!(target_os = "linux") {
        let (total, used, free) = (
            memory["total_bytes"].as_u64().unwrap(),
            memory["used_bytes"].as_u64().unwrap(),
            memory["available_bytes"].as_u64().unwrap(),
        );
        assert!(used > 0 && free < total && used + free <= total + (64 << 20), "{memory}");
    }
}

/// The settings an owner changes while the server runs (`/api/settings`,
/// the memory of one application): in force at once, kept with the data,
/// given back to the environment's value, and left alone when the
/// environment is said to rule.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_change_while_the_server_runs() {
    let admin = "a".repeat(40);
    let dir = temp_dir("settings");
    let open = || {
        let options = || EngineOptions { sync: false, ..EngineOptions::default() };
        // A server that just stopped lets go of its engine within moments
        // (its upkeep thread, its last connections).
        for _ in 0..200 {
            match Engine::open(&dir, options()) {
                Ok(engine) => return engine,
                Err(_) => std::thread::sleep(Duration::from_millis(25)),
            }
        }
        Engine::open(&dir, options()).unwrap()
    };
    // A server as the binary builds it: its environment says the cache budget.
    let env = HashMap::from([("NUAGE_CACHE_MB".to_owned(), "48".to_owned())]);
    let opts = || ServerOptions {
        admin_token: Some(admin.clone()),
        cache_budget: Some(48 << 20),
        cache_ceiling: Some(48 << 20),
        settings_env: Some(env.clone()),
        ..options()
    };
    let json = |r: &Http| serde_json::from_str::<serde_json::Value>(&r.body).unwrap_or_else(|_| panic!("{}", r.body));
    let setting = |list: &serde_json::Value, key: &str| {
        list["settings"].as_array().unwrap().iter().find(|s| s["key"] == key).cloned().unwrap()
    };
    let bearer = format!("Bearer {admin}");
    let auth: &[(&str, &str)] = &[("authorization", &bearer), ("content-type", "application/json")];
    const MIB: usize = 1 << 20;
    {
        let server = Server::new(open(), opts());
        let running = server.listen("127.0.0.1:0").await.unwrap();
        let addr = running.local_addr();
        server.create_app("shop", "").unwrap();
        server.create_app("blog", "").unwrap();
        let capacity = |name: &str| server.app_engine(name).unwrap().cache_capacity();
        let total = || ["nuage", "shop", "blog"].iter().map(|n| capacity(n)).sum::<usize>();
        let budget = || server.status()["memory"]["cache_budget_bytes"].as_u64().unwrap() as usize;

        // An application's token does not open the settings; the management token does.
        let refused = http(addr, "GET", "/api/settings", &[("authorization", "Bearer t0ken")], "").await;
        assert_eq!(refused.status, 401, "{}", refused.body);
        let list = json(&http(addr, "GET", "/api/settings", auth, "").await);
        let cache = setting(&list, "memory.cache_mb");
        assert_eq!(
            (&cache["value"], &cache["source"], &cache["variable"]),
            (&"48".into(), &"environment".into(), &"NUAGE_CACHE_MB".into())
        );
        assert_eq!(setting(&list, "status.public")["source"], "default");
        assert_eq!(list["read_only"], false);

        // Saved: in force at once, without a restart.
        assert_eq!(http(addr, "GET", "/api/status", &[], "").await.status, 401, "the status is not public yet");
        let saved =
            http(addr, "POST", "/api/settings", auth, r#"{"memory.cache_mb": 32, "status.public": true}"#).await;
        assert_eq!(saved.status, 200, "{}", saved.body);
        let cache = setting(&json(&saved), "memory.cache_mb");
        assert_eq!(
            (&cache["value"], &cache["source"], &cache["environment"]),
            (&"32".into(), &"saved".into(), &"48".into())
        );
        assert_eq!(budget(), 32 * MIB);
        assert!(total() <= 32 * MIB && total() > 30 * MIB, "{}", total());
        assert_eq!(http(addr, "GET", "/api/status", &[], "").await.status, 200, "public at once");

        // One value that does not fit: nothing is saved.
        for (body, said) in [
            (r#"{"memory.cache_mb": 40, "memory.kv_mb": 0}"#, "memory.kv_mb"),
            (r#"{"memory.cache_mb": 40, "nothing": 1}"#, "there is no setting `nothing`"),
            (r#"{"memory.cache_mb": "many"}"#, "not an amount of memory"),
            (r#"{"memory.total_mb": 16000000}"#, "memory.total_mb"),
            (r#"[1]"#, "JSON object"),
        ] {
            let r = http(addr, "POST", "/api/settings", auth, body).await;
            assert_eq!(r.status, 400, "{body}: {}", r.body);
            assert!(r.body.contains(said), "{body}: {}", r.body);
            assert_eq!(budget(), 32 * MIB, "{body}");
        }

        // Memory by database: 12 MiB set apart for the shop, the others share the rest.
        let apart = http(addr, "POST", "/api/apps/shop/memory", auth, r#"{"cache_mb": 12, "kv_mb": 2}"#).await;
        assert_eq!(apart.status, 200, "{}", apart.body);
        assert_eq!(json(&apart)["memory"], serde_json::json!({ "cache_mb": 12, "kv_mb": 2 }));
        assert_eq!(capacity("shop"), 12 * MIB);
        assert_eq!(capacity("nuage") + capacity("blog"), 20 * MIB, "the rest of the 32");
        assert_eq!(budget(), 32 * MIB);
        // What the body does not name stays as it is.
        let kv = http(addr, "POST", "/api/apps/shop/memory", auth, r#"{"kv_mb": "auto"}"#).await;
        assert_eq!(json(&kv)["memory"], serde_json::json!({ "cache_mb": 12, "kv_mb": "auto" }));
        // More than is left once the others have their floor (32 - 16): refused, with the figures.
        let much = http(addr, "POST", "/api/apps/blog/memory", auth, r#"{"cache_mb": 8}"#).await;
        assert_eq!(much.status, 400, "{}", much.body);
        assert!(much.body.contains("4 MiB at most"), "{}", much.body);
        // The budget cannot be lowered under what is set apart.
        let low = http(addr, "POST", "/api/settings", auth, r#"{"memory.cache_mb": 24}"#).await;
        assert_eq!(low.status, 400, "{}", low.body);
        assert!(low.body.contains("28 MiB at least"), "{}", low.body);
        assert_eq!(http(addr, "POST", "/api/apps/nothing/memory", auth, "{}").await.status, 404);
        running.shutdown().await;
    }

    // Started again: what was saved is still in force (it is kept with the data).
    {
        let server = Server::new(open(), opts());
        let running = server.listen("127.0.0.1:0").await.unwrap();
        let addr = running.local_addr();
        let capacity = |name: &str| server.app_engine(name).unwrap().cache_capacity();
        let budget = || server.status()["memory"]["cache_budget_bytes"].as_u64().unwrap() as usize;
        assert_eq!((budget(), capacity("shop")), (32 * MIB, 12 * MIB));
        assert_eq!(http(addr, "GET", "/api/status", &[], "").await.status, 200);
        let apps = json(&http(addr, "GET", "/api/apps/shop", auth, "").await);
        assert_eq!(apps["memory"]["cache_mb"], 12);
        // Given back: the environment's value is in force again, at once.
        let back = http(addr, "DELETE", "/api/settings/memory.cache_mb", auth, "").await;
        assert_eq!(back.status, 200, "{}", back.body);
        assert_eq!(setting(&json(&back), "memory.cache_mb")["source"], "environment");
        assert_eq!(budget(), 48 * MIB);
        assert_eq!(capacity("shop"), 12 * MIB, "what is set apart stays");
        assert_eq!(capacity("nuage") + capacity("blog"), 36 * MIB);
        // An application back on the shared budget.
        let auto = http(addr, "POST", "/api/apps/shop/memory", auth, r#"{"cache_mb": null}"#).await;
        assert_eq!(json(&auto)["memory"]["cache_mb"], "auto");
        assert_eq!(capacity("shop"), 16 * MIB, "a third of the 48");
        let again = http(addr, "POST", "/api/apps/shop/memory", auth, r#"{"cache_mb": "12 MiB"}"#).await;
        assert_eq!(json(&again)["memory"]["cache_mb"], 12);
        running.shutdown().await;
    }

    // `NUAGE_SETTINGS=environment`: what was saved is ignored, and nothing can be saved.
    {
        let server = Server::new(open(), ServerOptions { settings_read_only: true, ..opts() });
        let running = server.listen("127.0.0.1:0").await.unwrap();
        let addr = running.local_addr();
        assert_eq!(http(addr, "GET", "/api/status", &[], "").await.status, 401, "the saved `status.public` is ignored");
        let list = json(&http(addr, "GET", "/api/settings", auth, "").await);
        assert_eq!((&list["read_only"], &setting(&list, "status.public")["source"]), (&true.into(), &"default".into()));
        let r = http(addr, "POST", "/api/settings", auth, r#"{"status.public": true}"#).await;
        assert_eq!(r.status, 403, "{}", r.body);
        let r = http(addr, "POST", "/api/apps/shop/memory", auth, r#"{"cache_mb": 12}"#).await;
        assert_eq!(r.status, 403, "{}", r.body);
        // The amount of the shop is not in force either; it stays in its record.
        assert_eq!(server.app_engine("shop").unwrap().cache_capacity(), 16 * MIB);
        server.new_app_token("shop").unwrap();
        // What was saved is shown, and can be removed from there.
        assert_eq!(list["ignored"], serde_json::json!({ "status.public": "true" }));
        let removed = http(addr, "DELETE", "/api/settings/status.public", auth, "").await;
        assert_eq!(removed.status, 200, "{}", removed.body);
        assert_eq!(json(&removed)["ignored"], serde_json::json!({}));
        running.shutdown().await;
    }

    // The environment no longer rules alone: the shop's amount is in force again,
    // and what was removed stays removed.
    {
        let server = Server::new(open(), opts());
        let running = server.listen("127.0.0.1:0").await.unwrap();
        assert_eq!(server.app_engine("shop").unwrap().cache_capacity(), 12 * MIB);
        assert_eq!(http(running.local_addr(), "GET", "/api/status", &[], "").await.status, 401);
        running.shutdown().await;
    }
}
