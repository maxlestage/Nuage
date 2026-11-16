//! The `nuage` binary: a Nuage DB server configured by environment
//! variables (see `nuage::config`). With the `site` feature (on by default) it
//! also serves the website of Nuage DB and its live dashboard at `/app`.

use std::process::ExitCode;

use nuage::{Engine, Server};

fn log(message: &str) {
    println!("nuage: {message}");
}

/// The end of the engine's error when `Engine::open_existing` refuses an
/// empty data directory.
const EMPTY_REFUSED: &str = "an empty database is not started here";

/// The settings that name where the backup was looked for (`backups`, as
/// `Config::backups` lists them), with what to check in each.
fn settings_to_check(backups: &[(&str, Option<std::path::PathBuf>)]) -> String {
    let places: Vec<String> = backups
        .iter()
        .map(|(base, folder)| match folder {
            Some(_) => {
                format!("{base}_DIR (an external disk must be plugged in: without it, its folder is missing or empty)")
            }
            None => {
                format!("{base}_S3_BUCKET and {base}_S3_PREFIX (used as written: `nuage` and `nuage/` are two places)")
            }
        })
        .collect();
    places.join("; and ")
}

/// What to do when NUAGE_RESTORE_REQUIRED refused an empty start: where the
/// backup was looked for (`backups`, as `Config::backups` lists them), and how
/// to get out.
fn restore_required_help(backups: &[(&str, Option<std::path::PathBuf>)]) -> String {
    let check = if backups.is_empty() {
        "No backup destination is set: put the backup settings (NUAGE_BACKUP_DIR or NUAGE_BACKUP_S3_*) back as they \
         were"
            .to_owned()
    } else {
        format!("Check {}, or put the backup settings back as they were", settings_to_check(backups))
    };
    format!(
        "NUAGE_RESTORE_REQUIRED is set. Nothing was created and nothing was written. {check}; for the first start \
         of a new server, remove NUAGE_RESTORE_REQUIRED"
    )
}

/// What the engine's error says when a backup destination could not be read
/// at a restore, and the settings to remove to start from the other alone.
const TAKE_OUT: [(&str, &str); 2] = [
    ("take the first out of the settings", "NUAGE_BACKUP_DIR or NUAGE_BACKUP_S3_*"),
    ("take the second out of the settings", "NUAGE_BACKUP2_DIR or NUAGE_BACKUP2_S3_*"),
];

/// How long the stop gives the second backup destination of every database,
/// counted from the start of the last backups (it starts with the first's):
/// one that does not answer does not keep the process from ending.
const SECOND_PASS: std::time::Duration = std::time::Duration::from_secs(20);

/// How long the process waits, once the stop's work is done, for the threads
/// of requests that still wait on something (a backup destination that does
/// not answer): it then ends without them.
const LEFT_BEHIND: std::time::Duration = std::time::Duration::from_secs(1);

/// The last backups of `engines` to destination `destination`, on a thread
/// of its own, one database after the other; the receiver says when they
/// are done. `None` when none of them has that destination.
fn last_backups(engines: &[std::sync::Arc<Engine>], destination: usize) -> Option<tokio::sync::oneshot::Receiver<()>> {
    let engines: Vec<_> = engines
        .iter()
        .filter(|e| e.options().backup.as_ref().is_some_and(|b| destination == 0 || b.store.parts().is_some()))
        .cloned()
        .collect();
    if engines.is_empty() {
        return None;
    }
    let (done, finished) = tokio::sync::oneshot::channel();
    let spawned = std::thread::Builder::new().name(format!("nuage-stop-{}", destination + 1)).spawn(move || {
        for engine in &engines {
            if let Err(e) = engine.backup_to(destination, false) {
                eprintln!("nuage: database `{}`: last backup: {e}", engine.options().name);
            }
        }
        let _ = done.send(());
    });
    match spawned {
        Ok(_) => Some(finished),
        Err(e) => {
            eprintln!("nuage: cannot start the last backups: {e}");
            None
        }
    }
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
    // `nuage settings`, `nuage settings set <key> <value>`, `nuage settings unset <key>`:
    // what the console's Settings page changes, from a terminal.
    if std::env::args().nth(1).as_deref() == Some("settings") {
        return settings(std::env::args().skip(2).collect());
    }
    // `nuage backup`, `nuage backup second dir <folder>`, `… s3 …`, `… remove`.
    if std::env::args().nth(1).as_deref() == Some("backup") {
        return backup(std::env::args().skip(2).collect());
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
    nuage::memory::tune_allocator();
    // Blocking work (statements, backups) on a bounded number of threads: more
    // requests wait their turn instead of starting threads without end (a
    // Heroku dyno allows 256 in all). Statements run on these threads, and a
    // deep one recurses: 8 MiB of stack each, a margin over the default 2
    // (address space, not memory, until it is used).
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(8 << 20)
        .max_blocking_threads(nuage::memory::blocking_threads())
        .build()
    {
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
    let code = runtime.block_on(async move {
        let backup = config.engine.backup.as_ref().map(|b| b.store.describe());
        // With two destinations, each one's place.
        let parts = config.engine.backup.as_ref().and_then(|b| b.store.parts()).map(|(a, b)| (a.describe(), b.describe()));
        let opened = if config.restore_required {
            Engine::open_existing(&config.data_dir, config.engine)
        } else {
            Engine::open(&config.data_dir, config.engine)
        };
        let engine = match opened {
            Ok(e) => e,
            Err(e) => {
                let e = e.to_string();
                eprintln!("nuage: cannot open the data directory {}: {e}", config.data_dir.display());
                if config.restore_required && e.contains(EMPTY_REFUSED) {
                    eprintln!("nuage: {}", restore_required_help(&config.backups));
                }
                for (said, settings) in TAKE_OUT {
                    if e.contains(said) {
                        eprintln!("nuage: that destination is {settings}: remove them to start without it");
                    }
                }
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
                if parts.is_some() {
                    log(&format!(
                        "backups go to {target}, kept in step; an empty data directory is restored from the one that \
                         reaches furthest"
                    ));
                } else {
                    log(&format!("backups go to {target}; an empty data directory is restored from there at startup"));
                }
                // Every destination a folder on the data's disk: one disk that fails loses it all.
                let on_this_disk: Vec<_> = config
                    .backups
                    .iter()
                    .filter_map(|(base, folder)| {
                        let folder = folder.as_ref()?;
                        (nuage::config::same_disk(&config.data_dir, folder) == Some(true)).then_some((base, folder))
                    })
                    .collect();
                if !on_this_disk.is_empty() && on_this_disk.len() == config.backups.len() {
                    let folders: Vec<String> = on_this_disk.iter().map(|(_, f)| f.display().to_string()).collect();
                    let names: Vec<String> = on_this_disk.iter().map(|(base, _)| format!("{base}_DIR")).collect();
                    log(&format!(
                        "warning: the backups ({}) are on the same disk as the data: if this disk fails, both are lost. \
                         Put {} on another disk (and plug it in: an external disk that is not plugged in leaves its \
                         folder on this disk), or back up to S3 (NUAGE_BACKUP_S3_*)",
                        folders.join(", "),
                        names.join(" or ")
                    ));
                }
                let status = engine.backup_status();
                // Printed here, like the lines below: the log of the engines' events starts after them.
                match (&status.restored_from, &parts) {
                    (Some(from), Some(_)) => log(&format!(
                        "the data directory was empty: restored from the backup in {from}, the destination that \
                         reaches furthest"
                    )),
                    (Some(_), None) => log("the data directory was empty: restored from the backup"),
                    (None, _) => {}
                }
                if status.started_empty {
                    let nothing = match &parts {
                        Some((first, second)) => format!("neither {first} nor {second} holds a backup"),
                        None => format!("{target} holds no backup"),
                    };
                    log(&format!(
                        "warning: the data directory was empty and {nothing}: this is a NEW, EMPTY database. If this \
                         server had data, stop it now and check {}: its backups are still where they were. \
                         NUAGE_RESTORE_REQUIRED=true refuses to start in this case",
                        settings_to_check(&config.backups)
                    ));
                }
            }
            None => log(
                "no backup is configured: if this disk is ephemeral (Heroku dynos), set NUAGE_BACKUP_S3_* or NUAGE_BACKUP_DIR, or the data is lost at the next restart",
            ),
        }
        if engine.options().kv_in_database {
            log(
                "key-value stores are kept in each application's database (NUAGE_KV_STORAGE=database): they survive a \
                 restart and are in the backups; a write waits for the disk",
            );
        }
        if let Some(files) = &config.server.files {
            log(&format!(
                "files (/v1/files) go to {}: the bytes travel between the clients and the bucket, through signed links",
                files.describe()
            ));
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
        for line in server.setting_overrides() {
            log(&line);
        }
        // What each application holds, and what came from an earlier version.
        for app in server.apps() {
            let Some(e) = server.app_engine(&app.name) else { continue };
            let tables = nuage::Embedded::from_engine(e.clone()).tables().unwrap_or_default();
            let rows: i64 = tables.iter().map(|t| t.rows).sum();
            log(&format!("application `{}`: {} table(s), {rows} row(s)", app.name, tables.len()));
            // The first application is the server's own engine, said above.
            let status = e.backup_status();
            if !app.first && status.started_empty {
                log(&format!(
                    "warning: application `{}` is registered but its backup ({}) holds nothing: it starts empty",
                    app.name,
                    status.target.unwrap_or_default()
                ));
            }
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
                match server.listen_pg((config.pg_bind.as_str(), port), nuage::PgOptions { tls, ..Default::default() }).await {
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
        // The key-value stores on a port of their own, over the protocol Redis clients speak.
        let kv_port = match config.kv_port {
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
                match server.listen_kv((config.kv_bind.as_str(), port), nuage::KvOptions { tls }).await {
                    Ok(r) => {
                        let how = if encrypted {
                            "in clear and with TLS"
                        } else {
                            "in clear (NUAGE_TLS_CERT and NUAGE_TLS_KEY add TLS)"
                        };
                        log(&format!(
                            "key-value store on {} {how}: clients that speak the Redis protocol connect to an application's store, their password a token",
                            r.local_addr()
                        ));
                        Some(r)
                    }
                    Err(e) => {
                        eprintln!("nuage: key-value port: {e}");
                        return ExitCode::FAILURE;
                    }
                }
            }
            None => None,
        };
        // The engines' warnings and errors (a backup or a checkpoint that failed...) go to the
        // log too, not only to the dashboard: `heroku logs` and the services' log files show them.
        let server = std::sync::Arc::new(server);
        let watched = server.clone();
        let events = tokio::spawn(async move {
            let mut seen: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
            let started = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as i64);
            loop {
                for app in watched.apps() {
                    let Some(engine) = watched.app_engine(&app.name) else { continue };
                    let last = seen.entry(app.name.clone()).or_insert(started);
                    for event in engine.events().0 {
                        if event.at > *last && matches!(event.level, "warn" | "error") {
                            log(&format!("{} ({}): {}", event.level, app.name, event.message));
                        }
                        *last = (*last).max(event.at);
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        });
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
        events.abort();
        running.shutdown().await;
        if let Some(pg) = pg {
            pg.shutdown().await;
        }
        if let Some(kv_port) = kv_port {
            kv_port.shutdown().await;
        }
        // Every application, the server's own last (it holds the list of the others):
        // what was written since the last checkpoint goes to the backup before the disk is gone.
        // Connections already open are still served: from here, every commit is refused, so
        // that none is acknowledged after the last checkpoint and never sent.
        let mut engines: Vec<_> = server.apps().iter().filter_map(|a| server.app_engine(&a.name)).collect();
        engines.reverse();
        let engines = tokio::task::spawn_blocking(move || {
            // Every one first (it waits for a commit under way), then a checkpoint of each: the
            // last commits are in sealed log segments, which both destinations read.
            for engine in &engines {
                engine.stop_writes();
            }
            for engine in &engines {
                let _ = engine.checkpoint();
            }
            engines
        })
        .await
        .unwrap_or_default();
        // To both destinations at the same time, each on a thread of its own: neither waits for
        // the other, whichever is slow or does not answer. The first, as long as it takes (the
        // host ends the process when it must); the second, SECOND_PASS at most.
        let started = tokio::time::Instant::now();
        let (first, second) = (last_backups(&engines, 0), last_backups(&engines, 1));
        let second_done = match second {
            Some(second) => Some(tokio::time::timeout_at(started + SECOND_PASS, second).await.is_ok()),
            None => None,
        };
        if let Some(mut first) = first
            && tokio::time::timeout_at(started + SECOND_PASS, &mut first).await.is_err()
        {
            let second = if second_done == Some(true) { ", and the second has everything" } else { "" };
            log(&format!(
                "the first backup destination has not got everything after {} seconds{second}: the stop waits for \
                 it",
                SECOND_PASS.as_secs()
            ));
            let _ = first.await;
        }
        if second_done == Some(false) {
            log(&format!(
                "the second backup destination did not get everything within {} seconds: the first has it, \
                 and the second gets a whole snapshot at the next start",
                SECOND_PASS.as_secs()
            ));
        }
        let _ = tokio::task::spawn_blocking(move || {
            for engine in engines {
                let _ = engine.close();
            }
        })
        .await;
        ExitCode::SUCCESS
    });
    // A request still waiting on a destination that does not answer (Back up now, creating or
    // deleting an application) does not keep the process from ending.
    runtime.shutdown_timeout(LEFT_BEHIND);
    code
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
       nuage apps memory <name> <page cache> [key-value store]   (amounts: 256, 2G, auto)
       nuage settings
       nuage settings set <key> <value>
       nuage settings unset <key>
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
        ["memory", name, cache] => runtime.block_on(nuage::keepawake::set_app_memory(&url, &token, name, cache, None)),
        ["memory", name, cache, kv] => {
            runtime.block_on(nuage::keepawake::set_app_memory(&url, &token, name, cache, Some(kv)))
        }
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

/// `nuage settings …`: shows or changes the settings of a server.
fn settings(args: Vec<String>) -> ExitCode {
    let (url, token, _) = match apps_target() {
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
        [] | ["list"] => runtime.block_on(nuage::keepawake::list_settings(&url, &token)),
        // `2 GiB` may come as two words.
        ["set", key, value @ ..] if !value.is_empty() => {
            runtime.block_on(nuage::keepawake::set_setting(&url, &token, key, Some(&value.join(" "))))
        }
        ["unset", key] => runtime.block_on(nuage::keepawake::set_setting(&url, &token, key, None)),
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

const BACKUP_USAGE: &str = "usage: nuage backup                          where the backups go
       nuage backup second dir <folder>        a second copy in a folder (another disk)
       nuage backup second s3 <bucket> [endpoint] [region] [prefix]
                                               a second copy in S3-compatible storage; the keys
                                               from NUAGE_SECOND_S3_ACCESS_KEY_ID and
                                               NUAGE_SECOND_S3_SECRET_ACCESS_KEY (never arguments)
       nuage backup second remove              no second copy (the copies there stay)";

/// `nuage backup …`: the backups of a server, and its second copy.
fn backup(args: Vec<String>) -> ExitCode {
    let (url, token, _) = match apps_target() {
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
    let env = |name: &str| std::env::var(name).unwrap_or_default();
    let answer = match args.as_slice() {
        [] | ["status"] => runtime.block_on(nuage::keepawake::backup_status(&url, &token)),
        ["second", "dir", path @ ..] if !path.is_empty() => {
            let place = serde_json::json!({ "kind": "dir", "path": path.join(" ") });
            runtime.block_on(nuage::keepawake::set_second_copy(&url, &token, Some(place)))
        }
        ["second", "s3", bucket, rest @ ..] if rest.len() <= 3 => {
            let at = |i: usize| rest.get(i).copied().unwrap_or_default();
            let place = serde_json::json!({
                "kind": "s3",
                "bucket": bucket,
                "endpoint": at(0),
                "region": at(1),
                "prefix": at(2),
                "access_key_id": env("NUAGE_SECOND_S3_ACCESS_KEY_ID"),
                "secret_access_key": env("NUAGE_SECOND_S3_SECRET_ACCESS_KEY"),
            });
            runtime.block_on(nuage::keepawake::set_second_copy(&url, &token, Some(place)))
        }
        ["second", "remove"] => runtime.block_on(nuage::keepawake::set_second_copy(&url, &token, None)),
        _ => {
            eprintln!("{BACKUP_USAGE}");
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
       nuage apps memory <name> <cache> [kv]
                                          the memory of one application: amounts (256, 2G) or auto
       nuage settings                     the settings of a server, and where each comes from
       nuage settings set <key> <value>   changes one, in force at once (memory.cache_mb 2G)
       nuage settings unset <key>         gives one back to the environment's value
       nuage backup                       where the backups go, and the second copy
       nuage backup second dir <folder>   a second copy of the backups (or: s3 <bucket> ...,
                                          remove); see `nuage backup --help`
       nuage version                      the version";
