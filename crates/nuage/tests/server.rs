//! The server end to end: Memoire's client against a local Nuage DB server,
//! the status API, the console and S3 backups.
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
use memoire_orm::Query;
use memoire_orm::nuage::NuageError;
use memoire_orm::prelude::*;
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
    assert!(matches!(db.raw::<i64>("SELECT 1", &[]).await.unwrap_err(), Error::Unsupported(_)));

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
    assert!(overview.body.contains("nuage") && overview.body.contains("Size on disk"));
    assert_eq!(c.request("GET", "/api/status", &[]).await.status, 200, "the session opens the status API");
    let csrf_token = csrf(&overview.body);
    let tables = c.request("GET", "/admin/tables", &[]).await;
    assert!(tables.body.contains("users") && tables.body.contains(">3<"));
    let rows = c.request("GET", "/admin/tables/users", &[]).await;
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
    assert!(res.body.contains("1 row(s) deleted"), "{}", res.body);
    assert_eq!(User::count(&db).await.unwrap(), 2);
    let res = c.request("POST", "/admin/query", &[("csrf", &csrf_token), ("sql", "SELECT * FROM nowhere")]).await;
    assert_eq!(res.status, 400);

    // Branches.
    let res = c.request("POST", "/admin/branches", &[("csrf", &csrf_token), ("name", "review-1"), ("from", "")]).await;
    assert!(res.body.contains("Branch review-1 created"), "{}", res.body);
    assert_eq!(db.branches().await.unwrap().len(), 1);
    let res = c.request("POST", "/admin/branches/review-1/delete", &[("csrf", &csrf_token)]).await;
    assert!(res.body.contains("Branch review-1 deleted"));
    assert!(db.branches().await.unwrap().is_empty());

    // Storage actions.
    let res = c.request("POST", "/admin/storage/checkpoint", &[("csrf", &csrf_token)]).await;
    assert!(res.body.contains("Checkpoint at"));
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
