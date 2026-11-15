//! The memory to count on, when the environment says it (`NUAGE_MEMORY_MB`,
//! for a host that limits the memory without saying so) and an owner saves
//! a value: saving only lowers it. In a test binary of its own: the figure is
//! one for the whole process.

#![cfg(feature = "server")]

use std::collections::HashMap;
use std::sync::Arc;

use nuage::{Engine, EngineOptions, Server, ServerOptions};

#[test]
fn a_saved_memory_never_goes_past_the_environment() {
    let dir = std::env::temp_dir().join(format!("nuage-settings-memory-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let engine = Engine::open(&dir, EngineOptions { sync: false, ..EngineOptions::default() }).unwrap();
    let options = ServerOptions {
        admin_token: Some("a".repeat(40)),
        settings_env: Some(HashMap::from([("NUAGE_MEMORY_MB".to_owned(), "512".to_owned())])),
        ..ServerOptions::default()
    };
    let server = Arc::new(Server::new(engine, options));
    let total = || server.status()["memory"]["total_bytes"].as_u64();
    assert_eq!(total(), Some(512 << 20), "the environment's figure");
    // `auto` is what the system reports, under the environment's figure.
    server.save_setting("memory.total_mb", Some("auto")).unwrap();
    assert_eq!(total(), Some(512 << 20), "auto keeps the environment's figure");
    // Lower: in force; higher: refused.
    server.save_setting("memory.total_mb", Some("384")).unwrap();
    assert_eq!(total(), Some(384 << 20));
    let refused = server.save_setting("memory.total_mb", Some("4096")).unwrap_err();
    assert!(refused.to_string().contains("more than this server has (512 MiB)"), "{refused}");
    // Given back: the environment's figure again.
    server.save_setting("memory.total_mb", None).unwrap();
    assert_eq!(total(), Some(512 << 20));
    std::fs::remove_dir_all(&dir).unwrap();
}
