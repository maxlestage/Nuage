//! The site alone, with a fake Nuage DB status API that simulates a live server, to
//! develop and test the dashboard without the engine:
//!
//! ```sh
//! ./build-wasm.sh                                   # the browser bundle (pkg/)
//! cargo run --example dev --features vitesse        # http://localhost:8080
//! ```
//!
//! * `GET /api/status`, `GET /api/status/stream` (SSE: `status` every second, `log` events
//!   for checkpoints, backups, branches and compaction), `GET /api/health`;
//! * `NUAGE_FAKE_AUTH=open` (default): everything without credentials;
//!   `private`: `401` until signed in at `/admin`; `public`: the public subset until signed
//!   in (like `NUAGE_PUBLIC_STATUS=true`);
//! * `/admin`: a stand-in sign-in page that sets a session cookie and goes back to `next`;
//! * `NUAGE_DEV_SW_TAG=<anything>`: serves a different `/sw.js`, as a new deployment would,
//!   to try the "new version available" prompt;
//! * `PORT` (default 8080).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use nuage_site::status::{
    Backup, Branch, Connections, Database, Encryption, LogLine, Requests, Service, Status, Storage, Wal, format_time,
};
use vitesse::prelude::*;
use vitesse::sse::{self, Event};
use vitesse::tokio;
use vitesse::tokio::sync::broadcast;

const SESSION: &str = "nuage_admin";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Auth {
    Open,
    Private,
    Public,
}

/// A tiny deterministic random generator (xorshift).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// The simulated server.
struct Sim {
    rng: Rng,
    tick: u64,
    started_ms: f64,
    per_s: f64,
    total: u64,
    bytes: u64,
    disk_free: u64,
    wal: u64,
    last_checkpoint_ms: f64,
    backup_ms: Option<f64>,
    backup_ok: bool,
    compaction_left: u32,
    branches: Vec<Branch>,
    next_branch: u32,
}

fn now_ms() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64() * 1000.0)
}

impl Sim {
    fn new() -> Sim {
        let now = now_ms();
        Sim {
            rng: Rng(0x9e37_79b9_7f4a_7c15),
            tick: 0,
            started_ms: now - 26.0 * 3600.0 * 1000.0,
            per_s: 12.0,
            total: 1_234_567,
            bytes: 52_428_800,
            disk_free: 8_589_934_592,
            wal: 524_288,
            last_checkpoint_ms: now - 40_000.0,
            backup_ms: Some(now - 300_000.0),
            backup_ok: true,
            compaction_left: 0,
            branches: vec![Branch {
                name: "preview-42".into(),
                parent: Some("nuage".into()),
                created_at: Some(format_time(now - 3_600_000.0)),
                own_bytes: 65_536,
            }],
            next_branch: 43,
        }
    }

    fn log(level: &str, message: String) -> LogLine {
        let apps = ["nuage", "shop", "analytics"];
        let app = apps[message.len() % apps.len()];
        LogLine { at: format_time(now_ms()), level: level.into(), message, app: Some(app.into()) }
    }

    /// One second passes: returns the events it produced.
    fn step(&mut self) -> Vec<LogLine> {
        let now = now_ms();
        self.tick += 1;
        let mut events = Vec::new();
        let wave = (self.tick as f64 / 9.0).sin() * 6.0;
        self.per_s = (self.per_s * 0.6 + (14.0 + wave + self.rng.next() * 8.0) * 0.4).max(0.0);
        self.total += self.per_s.round() as u64;
        let written = (self.per_s * 900.0 * self.rng.next()) as u64;
        self.bytes += written / 4;
        self.disk_free = self.disk_free.saturating_sub(written / 4);
        self.wal += written;
        if self.tick.is_multiple_of(20) {
            events.push(Sim::log(
                "info",
                format!("checkpoint: {} folded into the pages", nuage_site::status::bytes(self.wal)),
            ));
            self.wal = 4096;
            self.last_checkpoint_ms = now;
        }
        if self.tick % 30 == 15 {
            self.backup_ms = Some(now);
            self.backup_ok = self.rng.next() > 0.2;
            events.push(if self.backup_ok {
                Sim::log(
                    "info",
                    format!("backup done: {} to s3://nuage-backups/prod", nuage_site::status::bytes(self.bytes)),
                )
            } else {
                Sim::log("error", "backup failed: s3://nuage-backups/prod answered 503, retrying in 60 s".into())
            });
        }
        if self.tick % 25 == 7 {
            self.compaction_left = 4;
            events.push(Sim::log("info", "compaction started on nuage".into()));
        }
        if self.compaction_left > 0 {
            self.compaction_left -= 1;
            if self.compaction_left == 0 {
                let freed = 200_000 + (self.rng.next() * 900_000.0) as u64;
                self.bytes = self.bytes.saturating_sub(freed);
                self.disk_free += freed;
                events
                    .push(Sim::log("info", format!("compaction done: {} reclaimed", nuage_site::status::bytes(freed))));
            }
        }
        if self.tick % 17 == 3 {
            if self.branches.len() < 3 {
                let name = format!("preview-{}", self.next_branch);
                self.next_branch += 1;
                events.push(Sim::log("info", format!("branch {name} created from nuage")));
                self.branches.push(Branch {
                    name,
                    parent: Some("nuage".into()),
                    created_at: Some(format_time(now)),
                    own_bytes: 0,
                });
            } else {
                let gone = self.branches.remove(0);
                events.push(Sim::log("warn", format!("branch {} deleted", gone.name)));
            }
        }
        for branch in &mut self.branches {
            branch.own_bytes += (self.rng.next() * 8192.0) as u64;
        }
        if self.tick.is_multiple_of(11) && self.rng.next() > 0.5 {
            events.push(Sim::log("info", "integrity check: 12 800 pages verified, no error".into()));
        }
        events
    }

    fn status(&mut self) -> Status {
        let now = now_ms();
        let hit = 0.95 + self.rng.next() * 0.04;
        let p50 = 0.6 + self.rng.next() * 0.5;
        let errors = if self.rng.next() > 0.9 { self.rng.next() * 0.3 } else { 0.0 };
        let pages = self.bytes / 4096;
        let backup_status = if self.backup_ok { "ok" } else { "degraded" };
        let ago = |ms: f64| {
            let s = ((now - ms) / 1000.0).max(0.0) as u64;
            if s < 60 { format!("{s} s ago") } else { format!("{} min ago", s / 60) }
        };
        Status {
            name: Some("Nuage DB".into()),
            version: Some("0.2.1".into()),
            status: if self.backup_ok { "ok".into() } else { "degraded".into() },
            uptime_s: Some(((now - self.started_ms) / 1000.0) as u64),
            started_at: Some(format_time(self.started_ms)),
            requests: Some(Requests {
                per_s: (self.per_s * 10.0).round() / 10.0,
                total: self.total,
                errors_per_s: (errors * 100.0).round() / 100.0,
                p50_ms: (p50 * 100.0).round() / 100.0,
                p99_ms: ((p50 * 4.0 + self.rng.next() * 3.0) * 10.0).round() / 10.0,
            }),
            connections: Some(Connections {
                active: 1 + (self.per_s / 4.0) as u64,
                idle: 3 + (self.rng.next() * 4.0) as u64,
            }),
            storage: Some(Storage {
                bytes: self.bytes,
                disk_free_bytes: self.disk_free,
                pages,
                cache_bytes: (pages * 4096).min(67_108_864) / 2 + 16_777_216,
                cache_capacity_bytes: 67_108_864,
                cache_hit_rate: (hit * 1000.0).round() / 1000.0,
            }),
            wal: Some(Wal { bytes: self.wal, last_checkpoint_at: Some(format_time(self.last_checkpoint_ms)) }),
            backup: Some(Backup {
                enabled: true,
                target: Some("s3://nuage-backups/prod".into()),
                last_at: self.backup_ms.map(format_time),
                last_ok: Some(self.backup_ok),
                last_error: (!self.backup_ok).then(|| "s3://nuage-backups/prod answered 503".into()),
            }),
            encryption: Some(Encryption {
                enabled: true,
                scheme: Some("XTS-AES-256 (pages), AES-256-GCM (log)".into()),
                secret: Some("key".into()),
            }),
            applications: Some(3),
            memory: Some(nuage_site::status::Memory {
                total_bytes: Some(1 << 30),
                available_bytes: Some(700 << 20),
                process_bytes: Some(24 << 20),
                swap_total_bytes: Some(1 << 30),
                swap_free_bytes: Some(1 << 30),
                cache_per_app_bytes: Some(128 << 20),
            }),
            postgresql: Some(nuage_site::status::Postgres { port: 5432, tls: true }),
            databases: Some(vec![
                Database {
                    name: "nuage".into(),
                    first: true,
                    encrypted: Some(true),
                    tables: 12,
                    rows: 34_567 + self.tick * 3,
                    bytes: self.bytes * 3 / 5,
                    file_bytes: self.bytes * 3 / 5,
                    tokens: 1,
                    last_request_at: Some(format_time(now - 400.0)),
                    requests: Some(Requests {
                        per_s: (self.per_s * 6.0).round() / 10.0,
                        p99_ms: ((p50 * 4.0) * 10.0).round() / 10.0,
                        ..Requests::default()
                    }),
                    branches: self.branches.clone(),
                    ..Database::default()
                },
                Database {
                    name: "shop".into(),
                    description: Some("La boutique en ligne".into()),
                    encrypted: Some(true),
                    status: Some(backup_status.into()),
                    tables: 7,
                    rows: 5_210 + self.tick,
                    bytes: self.bytes / 5,
                    file_bytes: self.bytes / 5,
                    tokens: 2,
                    last_request_at: Some(format_time(now - 1500.0)),
                    requests: Some(Requests {
                        per_s: (self.per_s * 4.0 * (1.0 + (self.tick as f64 / 5.0).cos() * 0.3)).round() / 10.0,
                        errors_per_s: (errors * 100.0).round() / 100.0,
                        p99_ms: ((p50 * 5.0 + self.rng.next() * 2.0) * 10.0).round() / 10.0,
                        ..Requests::default()
                    }),
                    ..Database::default()
                },
                Database {
                    name: "analytics".into(),
                    description: Some("Batch reports".into()),
                    encrypted: Some(true),
                    tables: 4,
                    rows: 980_112,
                    bytes: self.bytes / 5,
                    file_bytes: self.bytes / 5,
                    tokens: 1,
                    last_request_at: Some(format_time(now - 95_000.0)),
                    requests: Some(Requests::default()),
                    ..Database::default()
                },
            ]),
            services: vec![
                Service {
                    name: "engine".into(),
                    status: "ok".into(),
                    detail: Some(format!("{} pages, cache {:.0} %", nuage_site::status::group(pages), hit * 100.0)),
                },
                Service { name: "http".into(), status: "ok".into(), detail: Some(format!("{:.1} req/s", self.per_s)) },
                Service {
                    name: "wal".into(),
                    status: "ok".into(),
                    detail: Some(format!("checkpoint {}", ago(self.last_checkpoint_ms))),
                },
                Service {
                    name: "backup".into(),
                    status: backup_status.into(),
                    detail: Some(match self.backup_ms {
                        Some(at) if self.backup_ok => format!("last {}", ago(at)),
                        Some(at) => format!("failed {}", ago(at)),
                        None => "never".into(),
                    }),
                },
                Service {
                    name: "compaction".into(),
                    status: "ok".into(),
                    detail: Some(if self.compaction_left > 0 { "running".into() } else { "idle".into() }),
                },
            ],
        }
    }
}

#[derive(Clone)]
enum Push {
    Status(Box<Status>),
    Log(LogLine),
}

struct Fake {
    auth: Auth,
    sim: Mutex<Sim>,
    current: Mutex<Status>,
    tx: broadcast::Sender<Push>,
}

impl Fake {
    /// What `req` may see: `None` for a `401`, else the full status or its public subset.
    fn view(&self, req: &Request) -> Option<bool> {
        let signed_in = req.cookie(SESSION).is_some()
            || req.header("authorization").is_some_and(|h| h.starts_with("Bearer ") && h.len() > 7);
        match (self.auth, signed_in) {
            (Auth::Open, _) | (_, true) => Some(true),
            (Auth::Public, false) => Some(false),
            (Auth::Private, false) => None,
        }
    }
}

fn unauthorized() -> Response {
    res::status(401).header("www-authenticate", "Bearer").json(json!({"error": "unauthorized"}))
}

/// The `next` parameter, when it is a path of this site.
fn next_of(req: &Request) -> String {
    let next = req
        .query_string()
        .unwrap_or("")
        .split('&')
        .find_map(|kv| kv.strip_prefix("next="))
        .map(|v| v.replace("%2F", "/").replace("%2f", "/"))
        .unwrap_or_else(|| "/".into());
    if next.starts_with('/') && !next.starts_with("//") { next } else { "/".into() }
}

fn main() -> std::io::Result<()> {
    let auth = match std::env::var("NUAGE_FAKE_AUTH").as_deref() {
        Ok("private") => Auth::Private,
        Ok("public") => Auth::Public,
        _ => Auth::Open,
    };
    let port = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8080u16);
    let mut sim = Sim::new();
    let first = sim.status();
    let (tx, _) = broadcast::channel(64);
    let fake = Arc::new(Fake { auth, sim: Mutex::new(sim), current: Mutex::new(first), tx });
    let site = nuage_site::vitesse_handler();

    let mut app = App::new();
    app.middleware(middleware::logger());

    let f = fake.clone();
    app.get("/api/health", move |_req: Request| {
        let _ = &f;
        async move { res::json(json!({"status": "ok"})).header("cache-control", "no-store") }
    });

    let f = fake.clone();
    app.get("/api/status", move |req: Request| {
        let f = f.clone();
        async move {
            let Some(full) = f.view(&req) else { return unauthorized() };
            let status = f.current.lock().map(|s| s.clone()).unwrap_or_default();
            let status = if full { status } else { status.public() };
            res::json(status).header("cache-control", "no-store")
        }
    });

    let f = fake.clone();
    app.get("/api/status/stream", move |req: Request| {
        let f = f.clone();
        async move {
            let Some(full) = f.view(&req) else { return unauthorized() };
            let mut rx = f.tx.subscribe();
            let first = f.current.lock().map(|s| s.clone()).unwrap_or_default();
            let (out, stream) = sse::channel(16);
            tokio::spawn(async move {
                let show = |s: Status| if full { s } else { s.public() };
                let hello = Event::new()
                    .event("status")
                    .retry(Duration::from_millis(2000))
                    .data(serde_json::to_string(&show(first)).unwrap_or_default());
                if out.send(hello).await.is_err() {
                    return;
                }
                loop {
                    let event = match rx.recv().await {
                        Ok(Push::Status(s)) => {
                            Event::new().event("status").data(serde_json::to_string(&show(*s)).unwrap_or_default())
                        }
                        Ok(Push::Log(line)) if full => {
                            Event::new().event("log").data(serde_json::to_string(&line).unwrap_or_default())
                        }
                        Ok(Push::Log(_)) => continue,
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    };
                    if out.send(event).await.is_err() {
                        break;
                    }
                }
            });
            stream.into_response().header("cache-control", "no-store")
        }
    });

    // The stand-in console: a sign-in page and its session cookie.
    app.get("/admin", move |req: Request| async move {
        let next = next_of(&req);
        res::html(format!(
            concat!(
                "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\"/>",
                "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"/>",
                "<title>Console (dev) · Nuage DB</title></head><body style=\"font-family:system-ui;max-width:32rem;margin:4rem auto;padding:0 1rem\">",
                "<h1>Nuage DB console (dev stand-in)</h1><p>The real console is served by the Nuage DB server.</p>",
                "<form method=\"post\" action=\"/admin/login?next={next}\"><button type=\"submit\">Sign in</button></form>",
                "<p><a href=\"/admin/logout\">Sign out</a> · <a href=\"{next}\">Back</a></p></body></html>"
            ),
            next = next
        ))
    });
    app.post("/admin/login", move |req: Request| async move {
        let next = next_of(&req);
        Redirect::see_other(next)
            .into_response()
            .header("set-cookie", format!("{SESSION}=dev; Path=/; HttpOnly; SameSite=Lax"))
    });
    app.get("/admin/logout", move |_req: Request| async move {
        Redirect::see_other("/admin")
            .into_response()
            .header("set-cookie", format!("{SESSION}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax"))
    });

    // A new deployment, simulated: the same worker with another version.
    if let Some(tag) = std::env::var("NUAGE_DEV_SW_TAG").ok().filter(|t| !t.is_empty()) {
        let site = nuage_site::Site::new();
        let worker = format!("{}\n// deployment {tag}\n", site.service_worker());
        app.get("/sw.js", move |_req: Request| {
            let worker = worker.clone();
            async move {
                res::send(worker)
                    .header("content-type", "text/javascript; charset=utf-8")
                    .header("cache-control", "no-cache")
            }
        });
    }

    // Everything else: the site.
    app.fallback(move |req: Request| {
        let page = site(&req);
        async move { page.unwrap_or_else(|| res::status(404).text("Not found")) }
    });

    // The simulation: one step per second.
    let f = fake.clone();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let (status, events) = {
                let Ok(mut sim) = f.sim.lock() else { return };
                let events = sim.step();
                (sim.status(), events)
            };
            if let Ok(mut current) = f.current.lock() {
                *current = status.clone();
            }
            for line in events {
                let _ = f.tx.send(Push::Log(line));
            }
            let _ = f.tx.send(Push::Status(Box::new(status)));
        }
    });

    let mode = match auth {
        Auth::Open => "open",
        Auth::Private => "private (sign in at /admin)",
        Auth::Public => "public subset (sign in at /admin for the rest)",
    };
    println!("Nuage DB website (dev, fake status API, {mode}) on http://localhost:{port}");
    app.run(port)
}
