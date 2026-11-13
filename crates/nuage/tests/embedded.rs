//! The backend-agnostic test suite of Memoire, on an embedded Nuage DB.
#[path = "common/mod.rs"]
mod common;

use nuage::{Embedded, EngineOptions};

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("nuage-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn embedded_suite() {
    let dir = temp_dir("suite");
    let db = Embedded::open_with(&dir, EngineOptions { sync: false, ..EngineOptions::default() }).unwrap();
    common::run_suite(&db).await.unwrap();
    common::advanced::run(&db).await.unwrap();
    common::lifecycle::run(&db).await.unwrap();
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
