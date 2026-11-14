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
    // `nuage apps create <name> [description]`, `nuage apps list`: the management
    // API of a server (NUAGE_URL, NUAGE_ADMIN_TOKEN).
    if std::env::args().nth(1).as_deref() == Some("apps") {
        return apps(std::env::args().skip(2).collect());
    }
    if matches!(std::env::args().nth(1).as_deref(), Some("help" | "--help" | "-h")) {
        println!("{HELP}");
        return ExitCode::SUCCESS;
    }
    if matches!(std::env::args().nth(1).as_deref(), Some("version" | "--version" | "-V")) {
        println!("nuage {}", nuage::VERSION);
        return ExitCode::SUCCESS;
    }
    // `nuage --env-file <file>` (or NUAGE_ENV_FILE): the settings in a file, as the
    // services of the installers start it on macOS and Windows.
    let args: Vec<String> = std::env::args().collect();
    let env_file = match args.iter().position(|a| a == "--env-file") {
        Some(i) => match args.get(i + 1) {
            Some(path) => Some(path.clone()),
            None => {
                eprintln!("usage: nuage --env-file <file>");
                return ExitCode::FAILURE;
            }
        },
        None => std::env::var("NUAGE_ENV_FILE").ok().filter(|v| !v.trim().is_empty()),
    };
    let config = match &env_file {
        Some(path) => nuage::config::from_env_file(std::path::Path::new(path.trim())),
        None => nuage::config::from_env(),
    };
    let config = match config {
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
        if let Some(path) = &env_file {
            log(&format!("settings read from {path}"));
        }
        match &backup {
            Some(target) => {
                log(&format!("backups go to {target}; an empty data directory is restored from there at startup"));
                if let Some(folder) = &config.backup_dir
                    && nuage::config::same_disk(&config.data_dir, folder) == Some(true)
                {
                    log(&format!(
                        "warning: the backups ({}) are on the same disk as the data: if this disk fails, both are lost. \
                         Put NUAGE_BACKUP_DIR on another disk (and plug it in: an external disk that is not plugged in \
                         leaves its folder on this disk), or back up to S3 (NUAGE_BACKUP_S3_*)",
                        folder.display()
                    ));
                }
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
        if config.server.admin_token.is_some() {
            log("the management API (/api/apps) is on, with NUAGE_ADMIN_TOKEN");
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

const APPS_USAGE: &str = "usage: nuage apps create <name> [description]
       nuage apps list
On the server's computer, nothing else: the settings file of the installer gives the token and
the port (sudo reads it). From anywhere else: NUAGE_URL (the server, https://…) and
NUAGE_ADMIN_TOKEN (its management token).";

/// Where the installers put the settings file.
fn installed_settings() -> Vec<std::path::PathBuf> {
    let mut files = vec![
        std::path::PathBuf::from("/etc/nuage/nuage.env"),
        std::path::PathBuf::from("/usr/local/etc/nuage/nuage.env"),
    ];
    if let Ok(data) = std::env::var("ProgramData") {
        files.push(std::path::Path::new(&data).join("Nuage").join("nuage.env"));
    }
    files
}

/// The server and the management token of `nuage apps`, and the address to show to the
/// applications: NUAGE_URL and NUAGE_ADMIN_TOKEN, else the settings file (`--env-file`,
/// NUAGE_ENV_FILE, or the installers' one), whose server is on this computer.
fn apps_target() -> Result<(String, String, Option<String>), String> {
    let var = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
    let args: Vec<String> = std::env::args().collect();
    let explicit = args
        .iter()
        .position(|a| a == "--env-file")
        .and_then(|i| args.get(i + 1).cloned())
        .or_else(|| var("NUAGE_ENV_FILE"))
        .map(std::path::PathBuf::from);
    let settings = match explicit {
        Some(path) => Some(path),
        None if var("NUAGE_ADMIN_TOKEN").is_some() => None,
        None => installed_settings().into_iter().find(|p| p.exists()),
    };
    let file: std::collections::HashMap<String, String> = match &settings {
        Some(path) => {
            let text = std::fs::read_to_string(path).map_err(|e| {
                if e.kind() == std::io::ErrorKind::PermissionDenied {
                    format!(
                        "{} is readable by administrators only: run it with sudo (Windows: as administrator)",
                        path.display()
                    )
                } else {
                    format!("cannot read {}: {e}", path.display())
                }
            })?;
            nuage::config::parse_env_file(&text).map_err(|e| format!("{}: {e}", path.display()))?.into_iter().collect()
        }
        None => Default::default(),
    };
    let get = |k: &str| var(k).or_else(|| file.get(k).cloned().filter(|v| !v.is_empty()));
    let Some(token) = get("NUAGE_ADMIN_TOKEN") else {
        return Err(match &settings {
            Some(path) => format!(
                "{} has no NUAGE_ADMIN_TOKEN: add one (`nuage genkey`), restart the server, and run this again",
                path.display()
            ),
            None => APPS_USAGE.to_owned(),
        });
    };
    // A server of this computer is reached on its own port; its applications, elsewhere,
    // use its public address when the settings know it.
    let local = settings.is_some() && var("NUAGE_URL").is_none();
    let url = match get("NUAGE_URL") {
        Some(url) if !local => url,
        _ => format!("http://127.0.0.1:{}", get("PORT").unwrap_or_else(|| "8080".into())),
    };
    let public = if local { get("SITE_URL") } else { None };
    Ok((url, token, public))
}

/// `nuage apps …`: creates or lists the applications of a server.
fn apps(args: Vec<String>) -> ExitCode {
    let (url, token, public) = match apps_target() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("✗ {e}");
            return ExitCode::FAILURE;
        }
    };
    let args: Vec<&str> = args.iter().map(String::as_str).take_while(|a| *a != "--env-file").collect();
    let answer = match args.as_slice() {
        ["create", name, description @ ..] => runtime
            .block_on(nuage::keepawake::create_app(&url, &token, name, &description.join(" ")))
            .map(|lines| match &public {
                // The address the applications use, not this computer's loopback.
                Some(site) => {
                    lines.replacen(&format!("NUAGE_URL={url}"), &format!("NUAGE_URL={}", site.trim_end_matches('/')), 1)
                }
                None if url.starts_with("http://127.0.0.1") => format!(
                    "{lines}\n(an application on another computer replaces 127.0.0.1 by this computer's address)"
                ),
                None => lines,
            }),
        ["list"] => runtime.block_on(nuage::keepawake::list_apps(&url, &token)),
        _ => {
            eprintln!("{APPS_USAGE}");
            return ExitCode::FAILURE;
        }
    };
    match answer {
        Ok(out) => {
            println!("{out}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("✗ {e}");
            ExitCode::FAILURE
        }
    }
}

const HELP: &str = "Nuage DB: a database server, configured by environment variables (see the README).

usage: nuage                              start the server
       nuage --env-file <file>            start it with the settings of <file> (KEY=value lines;
                                          the environment wins), as NUAGE_ENV_FILE does
       nuage genkey                       a new key (NUAGE_ENCRYPTION_KEY, NUAGE_ADMIN_TOKEN)
       nuage check [url] [token]          does a connection work? (NUAGE_URL, NUAGE_TOKEN)
       nuage apps create <name> [desc]    a new application: on the server's computer with nothing else
                                          (sudo), from anywhere with NUAGE_URL and NUAGE_ADMIN_TOKEN
       nuage apps list                    the applications of a server
       nuage version                      the version";
