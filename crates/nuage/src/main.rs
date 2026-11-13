//! The `nuage` binary: a Nuage DB server configured by environment
//! variables (see `nuage::config`). With the `site` feature (on by default) it
//! also serves the website of Nuage DB and its live dashboard at `/app`.

use std::process::ExitCode;

use nuage::{Engine, Server};

fn log(message: &str) {
    println!("nuage: {message}");
}

fn main() -> ExitCode {
    // `nuage genkey`: a new encryption key, for NUAGE_ENCRYPTION_KEY.
    if std::env::args().nth(1).as_deref() == Some("genkey") {
        println!("{}", nuage::Encryption::generate_key());
        return ExitCode::SUCCESS;
    }
    if matches!(std::env::args().nth(1).as_deref(), Some("version" | "--version" | "-V")) {
        println!("nuage {}", nuage::VERSION);
        return ExitCode::SUCCESS;
    }
    let config = match nuage::config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("nuage: configuration error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("nuage: cannot start: {e}");
            return ExitCode::FAILURE;
        }
    };
    if config.engine.encryption.is_none() {
        eprintln!(
            "nuage: NUAGE_ENCRYPTION_KEY is not set. Nuage DB encrypts everything it stores (pages, log, backups).\n\
             nuage: generate a key with `nuage genkey` or `openssl rand -hex 32`, set it, and keep it safe:\n\
             nuage: without it the data and its backups cannot be read."
        );
        return ExitCode::FAILURE;
    }
    runtime.block_on(async move {
        let backup = config.engine.backup.as_ref().map(|b| b.store.describe());
        let engine = match Engine::open(&config.data_dir, config.engine) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("nuage: cannot open the data directory {}: {e}", config.data_dir.display());
                return ExitCode::FAILURE;
            }
        };
        let dir = std::fs::canonicalize(&config.data_dir).unwrap_or(config.data_dir.clone());
        log(&format!("Nuage DB {} — database `{}`", nuage::VERSION, engine.options().name));
        log(&format!("data is stored in {} on this server's disk", dir.display()));
        if let Some(kind) = engine.encryption() {
            log(&format!("everything is encrypted ({}), with the {kind} of NUAGE_ENCRYPTION_KEY", nuage::crypto::SCHEME));
        }
        match &backup {
            Some(target) => {
                log(&format!("backups go to {target}; an empty data directory is restored from there at startup"));
                if engine.backup_status().restored {
                    log("the data directory was empty: restored from the backup");
                }
            }
            None => log(
                "no backup is configured: if this disk is ephemeral (Heroku dynos), set NUAGE_BACKUP_S3_* or NUAGE_BACKUP_DIR, or the data is lost at the next restart",
            ),
        }
        if config.server.token.is_none() {
            log("NUAGE_TOKEN is not set: the database API (/v1) refuses every request");
        }
        if config.server.admin_password.is_none() {
            log("NUAGE_ADMIN_PASSWORD is not set: the console (/admin) is off");
        }
        // The website (pages, the live dashboard at /app, PWA) on the same port.
        #[cfg(feature = "site")]
        let options = nuage::ServerOptions { site: Some(nuage::site()), ..config.server };
        #[cfg(not(feature = "site"))]
        let options = config.server;
        let server = Server::new(engine.clone(), options);
        let running = match server.listen((config.bind.as_str(), config.port)).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("nuage: {e}");
                return ExitCode::FAILURE;
            }
        };
        log(&format!("listening on {}", running.local_addr()));
        let keep_awake = config.keep_awake.map(|k| {
            let hours = k.hours.map_or("always".to_owned(), |(a, b)| format!("from {a}:00 to {b}:00 UTC"));
            log(&format!("keep-awake: {} every {} min, {hours}", k.url, k.every.as_secs() / 60));
            tokio::spawn(k.run(log))
        });
        shutdown_signal().await;
        if let Some(task) = keep_awake {
            task.abort();
        }
        log("stopping: last checkpoint and backup");
        running.shutdown().await;
        let _ = tokio::task::spawn_blocking(move || {
            let _ = engine.checkpoint();
            if engine.options().backup.is_some() {
                let _ = engine.backup_now(false);
            }
            let _ = engine.close();
        })
        .await;
        ExitCode::SUCCESS
    })
}

/// SIGTERM (what Heroku and container hosts send) or Ctrl-C.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
