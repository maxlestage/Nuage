//! The editor against servers that answer as no current Nuage DB does: an
//! older version, a redirect, a website, a host that never answers, a plain
//! port reached over https. Each fake is a thread on 127.0.0.1 that records
//! what it receives, so the tests also see where the token went.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{click, has, type_into, wait_for, window};
use egui_kittest::kittest::{NodeT as _, Queryable as _};
use nuage_editor::api::{self, ApiError, Caps, Transport};
use nuage_editor::clip::FakeSink;
use nuage_editor::i18n::Lang;
use nuage_editor::secret::Secret;
use nuage_editor::vault::MemoryVault;
use nuage_editor::{Prefs, SavedServer};

const ADMIN: &str = "made-up-management-token-0123456789abcdef";

/// A request as received: its line and headers (lowercase names), and its path.
#[derive(Clone, Debug)]
struct Request {
    head: String,
    path: String,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.head.lines().skip(1).find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
        })
    }

    fn bearer(&self) -> bool {
        self.header("authorization") == Some(format!("Bearer {ADMIN}").as_str())
    }
}

/// What a fake answers (`None`: nothing, ever).
type Answer = dyn Fn(&Request, &str) -> Option<Vec<u8>> + Send + Sync;

struct Fake {
    address: String,
    requests: Arc<Mutex<Vec<Request>>>,
}

impl Fake {
    fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

/// A fake server; `answer` gets each request and the fake's own address.
fn fake(answer: impl Fn(&Request, &str) -> Option<Vec<u8>> + Send + Sync + 'static) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let answer: Arc<Answer> = Arc::new(answer);
    let (seen, base) = (requests.clone(), address.clone());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let (seen, base, answer) = (seen.clone(), base.clone(), answer.clone());
            std::thread::spawn(move || serve(stream, &seen, &base, &*answer));
        }
    });
    Fake { address, requests }
}

fn serve(mut stream: TcpStream, seen: &Mutex<Vec<Request>>, base: &str, answer: &Answer) {
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 4096];
    let end = loop {
        if let Some(at) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break at;
        }
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(n) => bytes.extend_from_slice(&buffer[..n]),
        }
    };
    let head = String::from_utf8_lossy(&bytes[..end]).into_owned();
    let path = head.split_whitespace().nth(1).unwrap_or_default().to_owned();
    let request = Request { head, path };
    // The body, if any, is read and dropped.
    let length: usize = request.header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut body = bytes.len() - end - 4;
    while body < length {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(n) => body += n,
        }
    }
    seen.lock().unwrap().push(request.clone());
    match answer(&request, base) {
        Some(reply) => {
            let _ = stream.write_all(&reply);
        }
        // Accepted, then silence, until the test is long over.
        None => std::thread::sleep(Duration::from_secs(60)),
    }
}

fn reply(status: &str, content_type: &str, body: &str) -> Option<Vec<u8>> {
    let mut reply = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    reply.extend_from_slice(body.as_bytes());
    Some(reply)
}

fn json(status: &str, body: serde_json::Value) -> Option<Vec<u8>> {
    reply(status, "application/json", &body.to_string())
}

/// A server of 0.4.2 to 0.4.6: applications and tokens, no settings, and a
/// status for applications' tokens only.
fn older(request: &Request, _base: &str) -> Option<Vec<u8>> {
    let no_page = |path: &str| serde_json::json!({ "error": { "kind": "not_found", "message": format!("no such page: {path}") } });
    match request.path.as_str() {
        "/api/health" => json("200 OK", serde_json::json!({ "status": "ok" })),
        "/api/apps" if request.bearer() => json(
            "200 OK",
            serde_json::json!({ "applications": [{
                "name": "shop", "description": "La boutique", "first": true, "created_at": 1,
                "tokens": [{ "id": "ab12cd34", "created_at": 1, "read_only": false }], "env_token": false
            }] }),
        ),
        "/api/apps" | "/api/status" => json(
            "401 Unauthorized",
            serde_json::json!({ "error": { "kind": "unauthorized", "message": "the status needs an application's token" } }),
        ),
        path => json("404 Not Found", no_page(path)),
    }
}

fn client(address: &str, transport: &Transport) -> api::Server {
    api::Server::new(address, Secret::new(ADMIN.into()), Lang::En, transport).unwrap()
}

fn connect(address: &str) -> Result<api::Connected, ApiError> {
    client(address, &Transport::default()).connect()
}

#[test]
fn a_server_deployed_before_the_settings_connects() {
    let fake = fake(older);
    let connected = connect(&fake.address).unwrap();
    assert_eq!(connected.server.caps(), &Caps::default(), "no settings, no status");
    assert_eq!(connected.apps.len(), 1);
    assert_eq!((connected.apps[0].name.as_str(), connected.apps[0].tokens[0].id.as_str()), ("shop", "ab12cd34"));
    assert!(connected.settings.is_none() && connected.settings_error.is_none());
    let requests = fake.requests();
    let health = requests.iter().find(|r| r.path == "/api/health").unwrap();
    assert_eq!(health.header("authorization"), None, "the health check goes without the token");
    assert_eq!(health.header("accept-language"), Some("en"));
    assert!(health.header("user-agent").is_some_and(|ua| ua.starts_with("nuage-editor/")));
    assert!(requests.iter().filter(|r| r.path != "/api/health").all(Request::bearer));

    // In the window: applications and tokens, and what to update for the rest.
    let prefs = Prefs { lang: Some(Lang::Fr), ..Prefs::default() };
    let mut harness = window(prefs, Arc::new(MemoryVault::default()), FakeSink::default());
    type_into(&mut harness, "Adresse du serveur", &fake.address);
    type_into(&mut harness, "Jeton de gestion", ADMIN);
    harness.get_by_label("Se connecter").click();
    wait_for(&mut harness, "the connection", |h| h.state().connected_to().is_some());
    assert!(has(&harness, "ne laisse l'éditeur gérer que les applications et les jetons"));
    click(&mut harness, "Mémoire");
    assert!(has(&harness, "Mettez le serveur à jour pour voir ceci ici"));
    click(&mut harness, "Applications");
    wait_for(&mut harness, "its applications", |h| has(h, "La boutique"));
    // No settings were asked for again: an older server has none.
    let settings_calls = fake.requests().iter().filter(|r| r.path == "/api/settings").count();
    assert_eq!(settings_calls, 2, "once by the API test, once by the window, never on reload");
}

#[test]
fn a_newer_server_with_fields_this_editor_does_not_know() {
    let fake = fake(|request, _| match request.path.as_str() {
        "/api/health" => json("200 OK", serde_json::json!({ "status": "ok", "uptime": 3 })),
        "/api/apps" => json(
            "200 OK",
            serde_json::json!({ "applications": [{ "name": "shop", "sites": [{ "url": "https://x" }] }], "more": true }),
        ),
        "/api/settings" => json("200 OK", serde_json::json!({ "settings": [], "memory": {}, "flavour": "new" })),
        "/api/status" => json("200 OK", serde_json::json!({ "version": "0.9.0", "applications": [] })),
        _ => json("404 Not Found", serde_json::json!({})),
    });
    let connected = connect(&fake.address).unwrap();
    let caps = connected.server.caps();
    assert!(caps.settings && caps.status && caps.origins, "{caps:?}");
    assert_eq!(caps.version.as_deref(), Some("0.9.0"));
    assert_eq!(connected.apps[0].name, "shop");
}

#[test]
fn a_redirect_is_not_followed_with_the_token() {
    // http to https on the same host: offered as is.
    let moved = fake(|request, base| {
        let to = base.replacen("http://", "https://", 1);
        reply(&format!("301 Moved Permanently\r\nLocation: {to}{}", request.path), "text/html", "")
    });
    let error = connect(&moved.address).unwrap_err();
    let https = moved.address.replacen("http://", "https://", 1);
    assert_eq!(
        error,
        ApiError::Redirect {
            from: moved.address.clone(),
            to: format!("{https}/api/health"),
            upgrade: Some(https.clone())
        }
    );
    let requests = moved.requests();
    assert_eq!(requests.len(), 1, "no second request");
    assert_eq!(requests[0].header("authorization"), None);

    // A page of the server's own (a login page): not followed either, the token stays here.
    let login = fake(|request, _| match request.path.as_str() {
        "/api/health" => json("200 OK", serde_json::json!({ "status": "ok" })),
        _ => reply("302 Found\r\nLocation: /admin/login", "text/html", ""),
    });
    let error = connect(&login.address).unwrap_err();
    assert!(
        matches!(&error, ApiError::Redirect { to, upgrade: None, .. } if *to == format!("{}/admin/login", login.address)),
        "{error:?}"
    );
    assert_eq!(login.requests().len(), 2, "health, then apps: nothing followed");

    // In the window: said, with the https address to use.
    let prefs = Prefs { lang: Some(Lang::Fr), ..Prefs::default() };
    let mut harness = window(prefs, Arc::new(MemoryVault::default()), FakeSink::default());
    type_into(&mut harness, "Adresse du serveur", &moved.address);
    type_into(&mut harness, "Jeton de gestion", ADMIN);
    harness.get_by_label("Se connecter").click();
    wait_for(&mut harness, "the redirect", |h| has(h, "renvoie l'éditeur vers"));
    click(&mut harness, &format!("Utiliser {https}"));
    assert_eq!(harness.state().typed_address(), https);
}

#[test]
fn a_website_never_gets_the_token() {
    let site = fake(|_, _| reply("200 OK", "text/html; charset=utf-8", "<html><h1>Mon site</h1></html>"));
    assert_eq!(connect(&site.address).unwrap_err(), ApiError::NotNuage(site.address.clone()));
    let other = fake(|_, _| json("200 OK", serde_json::json!({ "ok": true })));
    assert_eq!(connect(&other.address).unwrap_err(), ApiError::NotNuage(other.address.clone()));
    let gone = fake(|_, _| reply("404 Not Found", "text/html", "<html>Not found</html>"));
    assert_eq!(connect(&gone.address).unwrap_err(), ApiError::NotNuage(gone.address.clone()));
    for fake in [&site, &other, &gone] {
        assert!(fake.requests().iter().all(|r| r.header("authorization").is_none()), "{:?}", fake.requests());
    }
}

#[test]
fn hosts_that_answer_for_the_server() {
    // Heroku's router while the server sleeps or crashed.
    let router = fake(|_, _| reply("503 Service Unavailable", "text/html", "<html>Application error</html>"));
    assert_eq!(connect(&router.address).unwrap_err(), ApiError::HostAnswered(503));
    // Management off, and a server before 0.4.2.
    let off = fake(|request, _| match request.path.as_str() {
        "/api/health" => json("200 OK", serde_json::json!({ "status": "ok" })),
        _ => json("403 Forbidden", serde_json::json!({ "error": { "kind": "forbidden", "message": "off" } })),
    });
    assert_eq!(connect(&off.address).unwrap_err(), ApiError::ManagementOff);
    let old = fake(|request, _| match request.path.as_str() {
        "/api/health" => json("200 OK", serde_json::json!({ "status": "ok" })),
        path => json(
            "404 Not Found",
            serde_json::json!({ "error": { "kind": "not_found", "message": format!("no such page: {path}") } }),
        ),
    });
    assert_eq!(connect(&old.address).unwrap_err(), ApiError::TooOld);
    // An answer larger than any of Nuage DB's.
    let large = fake(|_, _| json("200 OK", serde_json::json!({ "status": "ok", "pad": "x".repeat(2 << 20) })));
    assert_eq!(connect(&large.address).unwrap_err(), ApiError::TooLarge);
}

#[test]
fn a_server_that_never_answers() {
    let silent = fake(|_, _| None);
    let transport = Transport { global: Duration::from_millis(600), ..Transport::default() };
    let started = Instant::now();
    let error = client(&silent.address, &transport).connect().unwrap_err();
    assert_eq!(error, ApiError::Timeout { address: silent.address.clone(), after: Duration::from_millis(600) });
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    assert!(!error.explain(Lang::Fr).is_empty());
}

#[test]
fn nothing_listens_there() {
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let error = connect(&format!("127.0.0.1:{port}")).unwrap_err();
    assert_eq!(error, ApiError::Refused { address: format!("http://127.0.0.1:{port}"), loopback: true, port });
    assert!(error.explain(Lang::Fr).contains(&format!("port {port}")));
}

#[test]
fn https_to_a_plain_port() {
    // Whatever arrives (a TLS hello), the answer is plain HTTP.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut buffer = [0u8; 4096];
            let _ = stream.read(&mut buffer);
            let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        }
    });
    let error = connect(&format!("https://127.0.0.1:{port}")).unwrap_err();
    assert!(matches!(error, ApiError::Tls(_)), "{error:?}");
}

#[test]
fn plain_http_to_the_internet_waits_for_consent() {
    let prefs = Prefs {
        lang: Some(Lang::Fr),
        servers: vec![SavedServer { address: "http://nuage.example.com".into(), token_kept: false }],
        ..Prefs::default()
    };
    let mut harness = window(prefs, Arc::new(MemoryVault::default()), FakeSink::default());
    assert!(has(&harness, "Non chiffré : le jeton traverserait Internet en clair"));
    assert!(harness.get_by_label("Se connecter").accesskit_node().is_disabled(), "not until consented");
    harness.get_by_label("Se connecter quand même").click();
    harness.run();
    assert!(!harness.get_by_label("Se connecter").accesskit_node().is_disabled());
    // The consent is for this time only: never in the preferences.
    let written = serde_json::to_string(&harness.state().prefs).unwrap();
    assert!(!written.contains("anyway") && !written.contains("cleartext"), "{written}");
}
