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
                        let signed = req
                            .headers()
                            .get("authorization")
                            .is_some_and(|v| v.to_str().unwrap().starts_with("AWS4-HMAC-SHA256 Credential=key/"));
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
                                    let xml: String =
                                        keys.iter().map(|k| format!("<Contents><Key>{k}</Key></Contents>")).collect();
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
        ("fr", ["Applications", "Nouvelle application", "Jetons", "Réglages", "Sites web"]),
        ("es", ["Aplicaciones", "Nueva aplicación", "Tokens", "Ajustes", "Sitios web"]),
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
        ] {
            let res = c.request("POST", path, &form).await;
            assert!(res.status < 500, "{code} POST {path}: {}", res.body);
            all.push_str(&res.body);
        }
        for w in words {
            assert!(all.contains(w), "{code}: {w}");
        }
        for english in ["Log out", "New application", "Size on disk", "Save the websites", "Checkpoint now"] {
            assert!(!all.contains(english), "{code}: `{english}` left in English");
        }
        c.request("POST", "/admin/logout", &[("csrf", &token)]).await;
    }
    running.shutdown().await;
}
