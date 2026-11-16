//! The second copy of the backups (la copie de secours): chosen over the
//! management API, written by the server on its own, never holding a secret
//! in what it answers, and a run folder starts a fresh server by itself.

#![cfg(feature = "server")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use nuage::{BackupOptions, DirectoryStore, Encryption, Engine, EngineOptions, Server, ServerOptions};
use serde_json::{Value as Json, json};
use tokio::net::TcpStream;

const ADMIN: &str = "0123456789abcdef0123456789abcdef-second";
/// The encryption key of these tests.
const KEY: [u8; 32] = [7; 32];

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("nuage-second-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

async fn http(addr: SocketAddr, method: &str, path: &str, token: &str, body: &str) -> (u16, String) {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await.unwrap();
    tokio::spawn(conn);
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("host", addr.to_string())
        .header("authorization", format!("Bearer {token}"))
        .header("accept-language", "fr")
        .body(Full::new(Bytes::from(body.to_owned())))
        .unwrap();
    let res = sender.send_request(req).await.unwrap();
    let status = res.status().as_u16();
    (status, String::from_utf8_lossy(&res.into_body().collect().await.unwrap().to_bytes()).into_owned())
}

fn options(key: [u8; 32]) -> EngineOptions {
    EngineOptions { sync: false, encryption: Some(Encryption::from_key(key)), ..EngineOptions::default() }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_copy_is_written_and_starts_a_server() {
    let data = temp_dir("data");
    let disk = temp_dir("disk");
    std::fs::create_dir_all(&disk).unwrap();
    let place = disk.join("nuage");
    let engine = Engine::open(&data, options(KEY)).unwrap();
    let server = Arc::new(Server::new(
        engine,
        ServerOptions { admin_token: Some(ADMIN.into()), token: Some("t0ken".into()), ..ServerOptions::default() },
    ));
    let (_, shop) = server.create_app("shop", "").unwrap();
    let running = server.listen("127.0.0.1:0").await.unwrap();
    let addr = running.local_addr();
    for (token, table) in [("t0ken", "notes"), (shop.as_str(), "orders")] {
        let create = json!({ "sql": format!("CREATE TABLE {table} (id INTEGER PRIMARY KEY, what TEXT)") }).to_string();
        assert_eq!(http(addr, "POST", "/v1/sql", token, &create).await.0, 200);
        let insert = json!({ "sql": format!("INSERT INTO {table} (id, what) VALUES (1, 'kept')") }).to_string();
        assert_eq!(http(addr, "POST", "/v1/sql", token, &insert).await.0, 200);
    }

    // Nothing yet; then refusals, in French: a folder inside the data, a
    // parent that is not there, a body that is no place.
    let (status, body) = http(addr, "GET", "/api/backup", ADMIN, "").await;
    assert_eq!(status, 200, "{body}");
    let state: Json = serde_json::from_str(&body).unwrap();
    assert!(state["second"].is_null() && state["environment"].is_null(), "{state}");
    let inside = json!({ "kind": "dir", "path": data.join("copy") }).to_string();
    let (status, body) = http(addr, "POST", "/api/backup/second", ADMIN, &inside).await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("dossier des données") || body.contains("données"), "{body}");
    let nowhere = json!({ "kind": "dir", "path": disk.join("not-plugged/nuage") }).to_string();
    assert_eq!(http(addr, "POST", "/api/backup/second", ADMIN, &nowhere).await.0, 400);
    assert_eq!(http(addr, "POST", "/api/backup/second", ADMIN, "[1]").await.0, 400);
    assert_eq!(http(addr, "POST", "/api/backup/second", "t0ken", &inside).await.0, 401, "the management token only");

    // A folder on another disk: saved, and a complete copy starts.
    let chosen = json!({ "kind": "dir", "path": place }).to_string();
    let (status, body) = http(addr, "POST", "/api/backup/second", ADMIN, &chosen).await;
    assert_eq!(status, 200, "{body}");
    let started = Instant::now();
    let state = loop {
        let state: Json = serde_json::from_str(&http(addr, "GET", "/api/backup", ADMIN, "").await.1).unwrap();
        if state["second"]["last_at"].is_i64() {
            break state;
        }
        assert!(state["second"]["last_error"].is_null(), "{state}");
        assert!(started.elapsed() < Duration::from_secs(20), "no copy: {state}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let run = state["second"]["run"].as_str().unwrap().to_owned();
    let lines: Vec<&str> = state["second"]["restore"].as_array().unwrap().iter().filter_map(Json::as_str).collect();
    let run_dir = place.join("lives").join(&run);
    assert_eq!(lines, [format!("NUAGE_BACKUP_DIR={}", run_dir.display())]);
    assert!(run_dir.join("manifest.json").is_file() && run_dir.join("apps/shop/manifest.json").is_file());

    // A fresh server from the run folder, with the same key: both databases come back.
    drop(running);
    let fresh = temp_dir("fresh");
    let restored = Engine::open(
        &fresh,
        EngineOptions { backup: Some(BackupOptions::new(Arc::new(DirectoryStore::new(&run_dir)))), ..options(KEY) },
    )
    .unwrap();
    assert!(restored.backup_status().restored);
    let again = Arc::new(Server::new(
        restored,
        ServerOptions { admin_token: Some(ADMIN.into()), token: Some("t0ken".into()), ..ServerOptions::default() },
    ));
    let running = again.listen("127.0.0.1:0").await.unwrap();
    let addr = running.local_addr();
    for (token, table) in [("t0ken", "notes"), (shop.as_str(), "orders")] {
        let select = json!({ "sql": format!("SELECT what FROM {table}") }).to_string();
        let (status, body) = http(addr, "POST", "/v1/sql", token, &select).await;
        assert_eq!(status, 200, "{table}: {body}");
        assert!(body.contains("kept"), "{table}: {body}");
    }
    // The second copy itself came back with the settings: the fresh server writes its own run.
    let (_, body) = http(addr, "GET", "/api/backup", ADMIN, "").await;
    let state: Json = serde_json::from_str(&body).unwrap();
    assert_ne!(state["second"]["run"].as_str(), Some(run.as_str()), "a folder of its own: {state}");
    assert!(!body.contains("secret_access_key"), "never a secret in the answer: {body}");

    // Removed: the copies there stay.
    let (status, body) = http(addr, "DELETE", "/api/backup/second", ADMIN, "").await;
    assert_eq!(status, 200, "{body}");
    assert!(serde_json::from_str::<Json>(&body).unwrap()["second"].is_null(), "{body}");
    assert!(run_dir.join("manifest.json").is_file(), "the copies there stay");
    drop(running);
    for dir in [data, disk, fresh] {
        let _ = std::fs::remove_dir_all(dir);
    }
}
