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
    // `nuage check [url] [token]`: does a connection work? (NUAGE_URL, NUAGE_TOKEN)
    if std::env::args().nth(1).as_deref() == Some("check") {
        let arg = |i: usize, var: &str| std::env::args().nth(i).or_else(|| std::env::var(var).ok()).unwrap_or_default();
        let (url, token) = (arg(2, "NUAGE_URL"), arg(3, "NUAGE_TOKEN"));
        if url.is_empty() || token.is_empty() {
            eprintln!("usage: nuage check <url> <token>   (or NUAGE_URL and NUAGE_TOKEN)");
            return ExitCode::FAILURE;
        }
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build();
        let answer = match runtime {
            Ok(r) => r.block_on(nuage::keepawake::check(&url, &token)),
            Err(e) => Err(e.to_string()),
        };
        return match answer {
            Ok(line) => {
                println!("✓ {line}");
                ExitCode::SUCCESS
            }
            Err(line) => {
                eprintln!("✗ {line}");
                ExitCode::FAILURE
            }
        };
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
        // What each application holds, and what came from an earlier version.
        for app in server.apps() {
            let Some(e) = server.app_engine(&app.name) else { continue };
            let tables = nuage::Embedded::from_engine(e.clone()).tables().unwrap_or_default();
            let rows: i64 = tables.iter().map(|t| t.rows).sum();
            log(&format!("application `{}`: {} table(s), {rows} row(s)", app.name, tables.len()));
            if let Some(i) = e.imported() {
                log(&format!(
                    "application `{}`: its data was written by an earlier version; it was imported and compared \
                     with it, entry by entry ({} entries, {} bytes)",
                    app.name, i.entries, i.bytes
                ));
            }
        }
        for (name, error) in server.failed_apps() {
            eprintln!("nuage: application `{name}` cannot be opened, its data is left as it was: {error}");
        }
        let running = match server.listen((config.bind.as_str(), config.port)).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("nuage: {e}");
                return ExitCode::FAILURE;
            }
        };
        log(&format!("listening on {}", running.local_addr()));
        // The PostgreSQL protocol: every language's PostgreSQL driver.
        let pg = match config.pg_port {
            Some(port) => {
                let tls = match &config.tls {
                    Some((cert, key)) => match nuage::tls_config(cert, key) {
                        Ok(t) => Some(t),
                        Err(e) => {
                            eprintln!("nuage: {e}");
                            return ExitCode::FAILURE;
                        }
                    },
                    None => None,
                };
                let encrypted = tls.is_some();
                match server.listen_pg((config.pg_bind.as_str(), port), nuage::PgOptions { tls }).await {
                    Ok(r) => {
                        let how = if encrypted { "with TLS" } else { "in clear (NUAGE_TLS_CERT and NUAGE_TLS_KEY add TLS)" };
                        log(&format!(
                            "PostgreSQL protocol on {} {how}: any PostgreSQL driver connects, its password a token",
                            r.local_addr()
                        ));
                        Some(r)
                    }
                    Err(e) => {
                        eprintln!("nuage: PostgreSQL protocol: {e}");
                        return ExitCode::FAILURE;
                    }
                }
            }
            None => None,
        };
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
        if let Some(pg) = pg {
            pg.shutdown().await;
        }
        // Every application, the server's own last (it holds the list of the others):
        // what was written since the last checkpoint goes to the backup before the disk is gone.
        let mut engines: Vec<_> = server.apps().iter().filter_map(|a| server.app_engine(&a.name)).collect();
        engines.reverse();
        let _ = tokio::task::spawn_blocking(move || {
            for engine in engines {
                let _ = engine.checkpoint();
                if engine.options().backup.is_some()
                    && let Err(e) = engine.backup_now(false)
                {
                    eprintln!("nuage: database `{}`: last backup: {e}", engine.options().name);
                }
                let _ = engine.close();
            }
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
