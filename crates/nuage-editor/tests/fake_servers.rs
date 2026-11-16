//! The editor against servers that answer as no current Nuage DB does: an
//! older version, a redirect, a website, a host that never answers, a plain
//! port reached over https. Each fake is a thread on 127.0.0.1 that records
//! what it receives, so the tests also see where the token went.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{click, has, press, type_into, wait_for, window};
use eframe::egui::accesskit::Toggled;
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

    fn method(&self) -> &str {
        self.head.split_whitespace().next().unwrap_or_default()
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
    // No status for the management token: what updating would show, said instead.
    assert!(has(&harness, "Les versions plus récentes de Nuage DB disent à l'éditeur leur version"));
    assert!(has(&harness, "si la dernière copie a fonctionné"));
    click(&mut harness, "Mémoire");
    assert!(has(&harness, "Mettez le serveur à jour pour voir ceci ici"));
    click(&mut harness, "Applications");
    wait_for(&mut harness, "its applications", |h| has(h, "La boutique"));
    // Its websites change in the console only, which it opens on the application.
    click(&mut harness, "shop");
    assert!(has(&harness, "Cette version de Nuage DB ne change les sites web que dans la console web"));
    assert!(harness.query_by_label("L'ouvrir sur cette application").is_some());
    assert!(harness.query_by_label("Site à autoriser").is_none(), "no field the server would refuse");
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

/// A server whose management token opens the status, but that has no route for
/// the websites (a version between the two).
fn without_websites(request: &Request, _base: &str) -> Option<Vec<u8>> {
    let no_route = |path: &str| serde_json::json!({ "error": { "kind": "not_found", "message": format!("no such route: {path}") } });
    match request.path.as_str() {
        "/api/status" if request.bearer() => json(
            "200 OK",
            serde_json::json!({ "version": "0.4.6", "uptime_s": 7200, "backup": { "enabled": false, "last_ok": true },
                                 "databases": [{ "name": "shop", "status": "ok" }] }),
        ),
        "/api/apps/shop/origins" => json("404 Not Found", no_route(&request.path)),
        _ => older(request, _base),
    }
}

#[test]
fn a_server_without_a_route_says_what_to_update() {
    let fake = fake(without_websites);
    let connected = connect(&fake.address).unwrap();
    let caps = connected.server.caps();
    assert!(caps.status && !caps.settings, "{caps:?}");
    assert_eq!(
        connected.server.set_origins("shop", &["https://monsite.fr".into()]).unwrap_err(),
        ApiError::Unsupported,
        "a missing route is told apart from a refusal"
    );
    let prefs = Prefs { lang: Some(Lang::Es), ..Prefs::default() };
    let mut harness = window(prefs, Arc::new(MemoryVault::default()), FakeSink::default());
    type_into(&mut harness, "Dirección del servidor", &fake.address);
    type_into(&mut harness, "Token de gestión", ADMIN);
    harness.get_by_label("Conectar").click();
    wait_for(&mut harness, "the connection", |h| h.state().connected_to().is_some());
    // Its status: the version, and no backup said in words.
    assert!(has(&harness, "versión 0.4.6") && has(&harness, "Ninguna copia de seguridad"));
    click(&mut harness, "Aplicaciones");
    click(&mut harness, "shop");
    type_into(&mut harness, "Sitio que autorizar", "https://misitio.es");
    harness.get_by_role_and_label(eframe::egui::accesskit::Role::Button, "Añadir").click();
    wait_for(&mut harness, "the answer", |h| {
        has(h, "Esta versión de Nuage DB solo cambia los sitios web en la consola web")
    });
    assert!(harness.query_by_label("Abrirla en esta aplicación").is_some());
    assert!(fake.requests().iter().any(|r| r.path == "/api/apps/shop/origins" && r.bearer()));
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
    let prefs =
        Prefs { lang: Some(Lang::Fr), servers: vec![SavedServer::at("http://nuage.example.com")], ..Prefs::default() };
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

/// The window in French, connected to the fake at `address`.
fn connected_in_french(address: &str) -> egui_kittest::Harness<'static, nuage_editor::Editor> {
    let prefs = Prefs { lang: Some(Lang::Fr), ..Prefs::default() };
    let mut harness = window(prefs, Arc::new(MemoryVault::default()), FakeSink::default());
    type_into(&mut harness, "Adresse du serveur", address);
    type_into(&mut harness, "Jeton de gestion", ADMIN);
    harness.get_by_label("Se connecter").click();
    wait_for(&mut harness, "the connection", |h| h.state().connected_to().is_some());
    harness
}

/// The test run again in a program of its own, whose environment has a proxy
/// (`HTTPS_PROXY`, no `NO_PROXY`): its address, given to the child run.
const PROXY_CHILD: &str = "NUAGE_EDITOR_TEST_SERVER_BEHIND_A_PROXY";

#[test]
fn a_proxy_is_for_the_internet_only() {
    if let Ok(address) = std::env::var(PROXY_CHILD) {
        // This computer is called directly: the proxy's own computer is not this one.
        let connected = connect(&address).unwrap();
        assert_eq!(connected.apps[0].name, "shop");
        // An address on the Internet goes through it (the proxy here refuses it).
        let public = client("https://nuage.example.com", &Transport::default()).health();
        assert!(matches!(public, Err(ApiError::Network(_))), "{public:?}");
        return;
    }
    let server = fake(older);
    // A proxy that notes the first line of each connection, and refuses it.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let noted = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            // The request may come in pieces: up to the end of its headers.
            let (mut head, mut buffer) = (Vec::new(), [0u8; 4096]);
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => head.extend_from_slice(&buffer[..n]),
                }
            }
            let head = String::from_utf8_lossy(&head).into_owned();
            noted.lock().unwrap().push(head.lines().next().unwrap_or_default().to_owned());
            let _ = stream.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        }
    });
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child.args(["--exact", "a_proxy_is_for_the_internet_only", "--test-threads=1"]).env(PROXY_CHILD, &server.address);
    for name in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"] {
        child.env(name, &proxy);
    }
    for name in ["NO_PROXY", "no_proxy"] {
        child.env_remove(name);
    }
    let out = child.output().unwrap();
    assert!(out.status.success(), "{}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("1 passed"), "the child ran the test");
    assert_eq!(*seen.lock().unwrap(), ["CONNECT nuage.example.com:443 HTTP/1.1"], "only the Internet's address");
    let requests = server.requests();
    assert!(requests.iter().any(|r| r.path == "/api/apps" && r.bearer()), "{requests:?}");
}

#[test]
fn a_status_that_fails_on_the_way_is_asked_again() {
    // A server that has the status, whose router answers its page once (it was waking).
    let fail = Arc::new(AtomicBool::new(true));
    let failing = fail.clone();
    let fake = fake(move |request, base| match request.path.as_str() {
        "/api/status" if failing.swap(false, Ordering::SeqCst) => {
            reply("503 Service Unavailable", "text/html", "<html>Application error</html>")
        }
        "/api/status" if request.bearer() => json(
            "200 OK",
            serde_json::json!({ "version": "0.4.7", "uptime_s": 60, "backup": { "enabled": false, "last_ok": true },
                                 "databases": [{ "name": "shop", "status": "ok" }] }),
        ),
        _ => older(request, base),
    });
    let connected = connect(&fake.address).unwrap();
    assert!(connected.status.is_none() && connected.server.caps().status, "{:?}", connected.server.caps());
    assert_eq!(connected.status_error, Some(ApiError::HostAnswered(503)));

    // In the window: said, never taken for an older server, and asked again at Refresh.
    fail.store(true, Ordering::SeqCst);
    let mut harness = connected_in_french(&fake.address);
    assert!(has(&harness, "Actualisation impossible : L'hébergeur a répondu 503"));
    assert!(!has(&harness, "Les versions plus récentes de Nuage DB"), "not an older server");
    press(&mut harness, "Actualiser");
    wait_for(&mut harness, "the status", |h| has(h, "version 0.4.7"));
    assert!(!has(&harness, "Actualisation impossible"));
    click(&mut harness, "Applications");
    click(&mut harness, "shop");
    assert!(harness.query_by_label("Site à autoriser").is_some(), "its websites change here");
    let asked = fake.requests().iter().filter(|r| r.path == "/api/status").count();
    assert_eq!(asked, 3, "once by the API test, then at the connection and at Refresh");
}

#[test]
fn a_change_whose_answer_is_lost_is_read_again() {
    // Heroku's router gives up after 30 s with its own page, while the server goes on.
    let made = Arc::new(Mutex::new((false, false)));
    let state = made.clone();
    let fake = fake(move |request, base| {
        let router = || reply("503 Service Unavailable", "text/html", "<html>Application error</html>");
        match (request.method(), request.path.as_str()) {
            ("POST", "/api/apps") => {
                state.lock().unwrap().0 = true;
                router()
            }
            ("POST", "/api/apps/shop/tokens") => {
                state.lock().unwrap().1 = true;
                router()
            }
            ("GET", "/api/apps") if request.bearer() => {
                let (app, token) = *state.lock().unwrap();
                let mut shop_tokens =
                    vec![serde_json::json!({ "id": "ab12cd34", "created_at": 1, "read_only": false })];
                if token {
                    shop_tokens.push(serde_json::json!({ "id": "99ff00aa", "created_at": 2, "read_only": false }));
                }
                let mut apps = vec![serde_json::json!({ "name": "shop", "first": true, "tokens": shop_tokens })];
                if app {
                    apps.push(serde_json::json!({ "name": "blog", "tokens": [{ "id": "ef567890", "created_at": 3 }] }));
                }
                json("200 OK", serde_json::json!({ "applications": apps }))
            }
            _ => older(request, base),
        }
    });
    let mut harness = connected_in_french(&fake.address);
    click(&mut harness, "Applications");
    type_into(&mut harness, "À quoi sert-elle ?", "Blog");
    press(&mut harness, "Créer l'application");
    wait_for(&mut harness, "the answer", |h| {
        has(h, "L'application a peut-être été créée quand même : la liste est relue")
    });
    // Read again: it was made, and its token never came.
    assert!(has(&harness, "Elle a été créée, mais son jeton n'est jamais arrivé jusqu'à l'éditeur"));
    click(&mut harness, "Ouvrir blog");
    assert!(has(&harness, "jamais montré") && has(&harness, "Un jeton a été créé, mais il n'est jamais arrivé"));

    // Another token, the same way: the one made, which nobody has, is marked.
    click(&mut harness, "Applications");
    click(&mut harness, "shop");
    assert!(!has(&harness, "jamais montré"));
    press(&mut harness, "Nouveau jeton (lecture et écriture)");
    wait_for(&mut harness, "the answer", |h| has(h, "Le changement a peut-être été fait, ou peut-être pas"));
    assert!(has(&harness, "jamais montré"), "the token made while the editor waited");
    let marked = harness.query_all_by_label("jamais montré").count();
    assert_eq!(marked, 1, "not the token it had before");
}

#[test]
fn a_refused_change_of_the_public_status_puts_the_box_back() {
    // Refused first (a value it does not take), then failing on the server's side.
    let posts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = posts.clone();
    let fake = fake(move |request, base| match (request.method(), request.path.as_str()) {
        ("GET", "/api/settings") if request.bearer() => json(
            "200 OK",
            serde_json::json!({ "read_only": false, "memory": {}, "applications": [], "startup": [],
                "settings": [{ "key": "status.public", "variable": "NUAGE_PUBLIC_STATUS", "value": "false",
                               "source": "default", "default": "false" }] }),
        ),
        ("POST", "/api/settings") if counted.fetch_add(1, Ordering::SeqCst) == 0 => {
            json("400 Bad Request", serde_json::json!({ "error": { "kind": "invalid", "message": "not this one" } }))
        }
        ("POST", "/api/settings") => json(
            "500 Internal Server Error",
            serde_json::json!({ "error": { "kind": "internal", "message": "disk full" } }),
        ),
        _ => older(request, base),
    });
    let mut harness = connected_in_french(&fake.address);
    click(&mut harness, "Limites et visibilité");
    let label = "État public : /api/status et le tableau de bord montrent l'essentiel sans connexion";
    assert_eq!(harness.get_by_label(label).accesskit_node().toggled(), Some(Toggled::False));
    for answer in ["Non enregistré : (not this one)", "Le serveur a échoué (HTTP 500). (disk full)"] {
        harness.get_by_label(label).click();
        harness.step();
        wait_for(&mut harness, answer, |h| has(h, answer));
        // What the server has: still not public.
        assert_eq!(harness.get_by_label(label).accesskit_node().toggled(), Some(Toggled::False), "{answer}");
    }
    assert_eq!(posts.load(Ordering::SeqCst), 2);
}

#[test]
fn stop_waiting_keeps_nothing() {
    // A server that wakes slowly; "Remember" ticked; the person stops waiting.
    let fake = fake(|request, base| {
        if request.path == "/api/apps" {
            std::thread::sleep(Duration::from_secs(4));
        }
        older(request, base)
    });
    let vault = Arc::new(MemoryVault::default());
    let prefs = Prefs { lang: Some(Lang::Fr), ..Prefs::default() };
    let mut harness = window(prefs, vault.clone(), FakeSink::default());
    type_into(&mut harness, "Adresse du serveur", &fake.address);
    type_into(&mut harness, "Jeton de gestion", ADMIN);
    harness.get_by_label("Retenir sur cet ordinateur").click();
    harness.run();
    harness.get_by_label("Se connecter").click();
    harness.step();
    // Offered after 3 s (the window keeps repainting meanwhile: it shows the seconds).
    let started = Instant::now();
    while harness.query_by_label("Ne plus attendre").is_none() {
        assert!(started.elapsed() < Duration::from_secs(10), "no Stop waiting");
        std::thread::sleep(Duration::from_millis(50));
        harness.step();
    }
    harness.get_by_role_and_label(eframe::egui::accesskit::Role::Button, "Ne plus attendre").click();
    harness.step();
    harness.run();
    assert!(has(&harness, "Attente arrêtée"));
    // The server answers after all: its answer is dropped, and nothing is kept.
    let answered = Instant::now();
    while fake.requests().iter().filter(|r| r.path == "/api/apps").count() == 0
        || answered.elapsed() < Duration::from_secs(3)
    {
        std::thread::sleep(Duration::from_millis(50));
        harness.step();
    }
    harness.run();
    assert!(vault.addresses().is_empty(), "no token written");
    assert!(harness.state().connected_to().is_none() && harness.state().prefs.servers.is_empty());
}

#[test]
fn servers_whose_token_is_kept_stay_listed() {
    // Eight servers whose tokens are kept: a ninth connection pushes none out (its store
    // entry would be left behind, with nothing in the window to remove it).
    let fake = fake(older);
    let kept: Vec<SavedServer> = (1..=8)
        .map(|i| SavedServer { token_kept: true, ..SavedServer::at(&format!("https://n{i}.example.com")) })
        .collect();
    let prefs = Prefs { lang: Some(Lang::Fr), servers: kept, ..Prefs::default() };
    let mut harness = window(prefs, Arc::new(MemoryVault::default()), FakeSink::default());
    // Another address: its token is typed.
    type_into(&mut harness, "Adresse du serveur", &fake.address);
    type_into(&mut harness, "Jeton de gestion", ADMIN);
    harness.get_by_label("Se connecter").click();
    wait_for(&mut harness, "the connection", |h| h.state().connected_to().is_some());
    let servers = &harness.state().prefs.servers;
    assert_eq!(servers.len(), 9);
    assert!(servers[0].address == fake.address && servers.iter().skip(1).all(|s| s.token_kept));
}

/// A token whose answer is lost, and the list that cannot be read again
/// either: the page says it does not know yet, never that no token was made.
#[test]
fn a_lost_token_is_not_said_missing_before_the_list_is_read_again() {
    let asked = Arc::new(Mutex::new(false));
    let state = asked.clone();
    let fake = fake(move |request, base| {
        let router = || reply("503 Service Unavailable", "text/html", "<html>Application error</html>");
        match (request.method(), request.path.as_str()) {
            ("POST", "/api/apps/shop/tokens") => {
                *state.lock().unwrap() = true;
                router()
            }
            // Once the token was asked, the list cannot be read either.
            ("GET", "/api/apps") if request.bearer() && *state.lock().unwrap() => router(),
            ("GET", "/api/apps") if request.bearer() => json(
                "200 OK",
                serde_json::json!({ "applications": [{ "name": "shop", "first": true, "tokens": [{ "id": "ab12cd34", "created_at": 1 }] }] }),
            ),
            _ => older(request, base),
        }
    });
    let mut harness = connected_in_french(&fake.address);
    click(&mut harness, "Applications");
    click(&mut harness, "shop");
    press(&mut harness, "Nouveau jeton (lecture et écriture)");
    wait_for(&mut harness, "the answer", |h| has(h, "Le changement a peut-être été fait, ou peut-être pas"));
    wait_for(&mut harness, "the list", |h| has(h, "On ne sait pas encore si le jeton a été créé"));
    assert!(!has(&harness, "Aucun nouveau jeton n'a été créé"), "not said before the list is read again");
}
