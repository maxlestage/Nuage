//! `/api/status` and `/api/status/stream`: the live state of the server.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use bytes_helper::sse;
use hyper::body::Incoming;
use hyper::header::{self, HeaderValue};
use hyper::{Request, Response};
use serde_json::{Value as Json, json};
use tokio::sync::{mpsc, watch};

use super::apps::App;
use super::limits::{ADMISSION_WAIT, busy, unblocked};
use super::{ChannelBody, HttpBody, State, error_response, json_response, unauthorized};
use crate::embedded::Embedded;
use crate::engine::{BackupStatus, StorageStats};
use http_body_util::BodyExt;

mod bytes_helper {
    use hyper::body::Bytes;

    /// One Server-Sent Event.
    pub(super) fn sse(event: &str, data: &str) -> Bytes {
        Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
    }
}

fn rfc3339(t: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn rfc3339_ms(ms: i64) -> Option<String> {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

fn ago(t: SystemTime) -> String {
    let s = t.elapsed().unwrap_or_default().as_secs();
    match s {
        0..=59 => format!("{s} s ago"),
        60..=3599 => format!("{} min ago", s / 60),
        _ => format!("{} h ago", s / 3600),
    }
}

/// Who may see what.
#[derive(Clone)]
pub(crate) enum View {
    /// A console session: every application.
    All,
    /// An application's token: the server, and that application only.
    App(Arc<App>),
    /// No credentials, `public_status`: the public subset.
    Public,
}

fn access(state: &State, req: &Request<Incoming>) -> Option<View> {
    if state.admin.is_logged_in(req.headers()) {
        Some(View::All)
    } else if let Some((app, _)) = state.app_of(req.headers()) {
        Some(View::App(app))
    } else if state.public_status() {
        Some(View::Public)
    } else {
        None
    }
}

impl View {
    fn apps(&self, state: &State) -> Vec<Arc<App>> {
        match self {
            View::All => state.apps.all(),
            View::App(app) => vec![app.clone()],
            View::Public => Vec::new(),
        }
    }

    /// The two show the same thing.
    fn same(&self, other: &View) -> bool {
        match (self, other) {
            (View::All, View::All) | (View::Public, View::Public) => true,
            (View::App(a), View::App(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

/// What the status reads of every application: once for all the views
/// made at the same time.
struct Every {
    all: Vec<Arc<App>>,
    stats: Vec<(StorageStats, BackupStatus)>,
}

impl Every {
    fn of(state: &State) -> Every {
        let all = state.apps.all();
        let stats = all.iter().map(|a| (a.engine.stats(), a.engine.backup_status())).collect();
        Every { all, stats }
    }
}

fn requests_json(metrics: &super::Metrics) -> Json {
    let (per_s, errors_per_s) = metrics.rates();
    let (p50, p99) = metrics.percentiles();
    json!({ "per_s": per_s, "total": metrics.total(), "errors_per_s": errors_per_s, "p50_ms": p50, "p99_ms": p99 })
}

/// One application, as `databases[]` lists it.
fn app_json(state: &State, app: &App, env_token: bool, every: &Every) -> Json {
    let engine = &app.engine;
    let read = every.all.iter().position(|a| std::ptr::eq(a.as_ref(), app)).map(|i| every.stats[i].clone());
    let (stats, backup) = read.unwrap_or_else(|| (engine.stats(), engine.backup_status()));
    let db = Embedded::from_engine(engine.clone());
    let tables = db.tables().unwrap_or_default();
    let branches: Vec<Json> = engine
        .branches()
        .iter()
        .map(|b| {
            json!({
                "name": b.name,
                "parent": b.parent.clone().unwrap_or_else(|| app.name.clone()),
                "created_at": rfc3339_ms(b.created_at),
                "own_bytes": b.own_bytes,
            })
        })
        .collect();
    let last = app.last_request.load(Ordering::Relaxed);
    let info = app.info(env_token);
    let kv = app.kv.stats();
    let described = state.described_schemas(&app.name);
    let kv = json!({
        "keys": kv.keys, "bytes": kv.bytes, "max_bytes": kv.max_bytes,
        "hits": kv.hits, "misses": kv.misses, "evictions": kv.evictions, "expired": kv.expired,
    });
    json!({
        "name": app.name,
        "first": app.first,
        "description": info.description,
        "status": if backup.last_ok { "ok" } else { "degraded" },
        "encrypted": engine.encryption().is_some(),
        "tables": tables.len(),
        "rows": tables.iter().map(|t| t.rows).sum::<i64>(),
        "bytes": stats.stored_bytes,
        "file_bytes": stats.file_bytes,
        "tokens": info.tokens.len() + usize::from(info.env_token),
        "read_only_tokens": info.tokens.iter().filter(|t| t.read_only).count(),
        "websites": info.origins,
        "created_at": rfc3339_ms(app.created_at),
        "last_request_at": (last > 0).then(|| rfc3339_ms(last)).flatten(),
        "last_connection": app.last_connection().map(|c| json!({
            "at": rfc3339_ms(c.at),
            "via": c.via,
            "client": c.client,
            "read_only": c.read_only,
        })),
        "requests": requests_json(&app.metrics),
        "kv": kv,
        // The table schemas its clients described, kept while the server runs.
        "described_schemas": { "count": described.0, "bytes": described.1 },
        // What it keeps in the bucket (`/v1/files`), as last counted: null
        // without a bucket, or until the first count is back.
        "files": state
            .options
            .files
            .as_ref()
            .and_then(|f| f.usage(&app.name))
            .map(|(count, bytes)| json!({ "count": count, "bytes": bytes })),
        "branches": branches,
    })
}

/// The status JSON (the shape the site and the console read).
pub(crate) fn status(state: &State, view: &View) -> Json {
    status_of(state, view, &Every::of(state))
}

fn status_of(state: &State, view: &View, read: &Every) -> Json {
    let engine = &state.engine;
    let (all, every) = (&read.all, &read.stats);
    let (per_s, _) = state.metrics.rates();
    let uptime = engine.uptime().as_secs();
    let open = state.metrics.open_connections.load(Ordering::Relaxed);
    let active = state.metrics.active_requests.load(Ordering::Relaxed);

    let backup = engine.backup_status();
    let failing: Vec<String> = all
        .iter()
        .zip(every)
        .filter(|(_, (_, b))| !b.last_ok)
        .map(|(a, (_, b))| format!("{}: {}", a.name, b.last_error.clone().unwrap_or_else(|| "failed".into())))
        .collect();
    // The application furthest behind tells how fresh the backups are.
    let oldest_backup = every.iter().map(|(_, b)| b.last_at).min().flatten();
    let (backup_state, backup_detail) = match (&backup.target, failing.first()) {
        (None, _) => ("ok", "off".to_owned()),
        (Some(_), None) => (
            "ok",
            oldest_backup.map_or("waiting for the first copy".to_owned(), |at| {
                format!("last {}", ago(SystemTime::UNIX_EPOCH + Duration::from_millis(at as u64)))
            }),
        ),
        (Some(_), Some(error)) => ("degraded", error.clone()),
    };
    let oldest_checkpoint = every.iter().map(|(s, _)| s.last_checkpoint).min().flatten();
    let wal_detail = oldest_checkpoint.map_or("no checkpoint yet".to_owned(), |t| format!("checkpoint {}", ago(t)));
    let compaction = match all.iter().filter_map(|a| a.engine.last_compaction()).filter(|(_, done)| *done).max() {
        Some((at, _)) => rfc3339_ms(at).map_or("done".into(), |t| format!("last run {t}")),
        _ => "idle".into(),
    };
    let pages: u64 = every.iter().map(|(s, _)| s.pages).sum();
    let hit_rate = every.iter().map(|(s, _)| s.cache_hit_rate).sum::<f64>() / every.len().max(1) as f64;
    let encrypted = all.iter().all(|a| a.engine.encryption().is_some());
    let services = vec![
        json!({ "name": "engine", "status": "ok", "detail": format!("{} application(s), {pages} pages, cache {:.0} %", all.len(), hit_rate * 100.0) }),
        json!({ "name": "http", "status": "ok", "detail": format!("{per_s:.1} req/s") }),
        json!({ "name": "wal", "status": "ok", "detail": wal_detail }),
        json!({ "name": "backup", "status": backup_state, "detail": backup_detail }),
        json!({ "name": "compaction", "status": "ok", "detail": compaction }),
    ];
    let overall = if services.iter().all(|s| s["status"] == "ok") { "ok" } else { "degraded" };

    if matches!(view, View::Public) {
        return json!({
            "name": "Nuage DB",
            "status": overall,
            "uptime_s": uptime,
            "started_at": rfc3339(engine.started_at()),
            "services": services.iter().map(|s| json!({ "name": s["name"], "status": s["status"] })).collect::<Vec<_>>(),
        });
    }

    let env_token = state.options.token.is_some();
    let databases: Vec<Json> = view.apps(state).iter().map(|a| app_json(state, a, env_token, read)).collect();
    let sum = |f: fn(&crate::engine::StorageStats) -> u64| every.iter().map(|(s, _)| f(s)).sum::<u64>();
    let first = &every[0].0;
    let m = crate::memory::MemoryInfo::now();
    let memory = json!({
        "total_bytes": m.total_bytes,
        "used_bytes": m.used_bytes,
        "available_bytes": m.available_bytes,
        "limited": m.limited,
        "cache_budget_bytes": state.apps.cache_budget(),
        "cache_ceiling_bytes": state.apps.cache_ceiling(),
        "reliefs": state.memory_reliefs(),
        // What the requests being read may hold together.
        "request_bytes": state.request_memory.total(),
        "process_bytes": m.process_bytes,
        "swap_total_bytes": m.swap_total_bytes,
        "swap_free_bytes": m.swap_free_bytes,
        "cache_per_app_bytes": engine.cache_capacity(),
    });
    json!({
        "name": "Nuage DB",
        "version": crate::VERSION,
        "status": overall,
        "uptime_s": uptime,
        "started_at": rfc3339(engine.started_at()),
        "requests": requests_json(&state.metrics),
        "connections": { "active": active, "idle": open.saturating_sub(active) },
        "memory": memory,
        "storage": {
            "bytes": sum(|s| s.file_bytes),
            "disk_free_bytes": first.disk_free_bytes,
            "pages": pages,
            "cache_bytes": sum(|s| s.cache_bytes),
            "cache_capacity_bytes": sum(|s| s.cache_capacity_bytes),
            "cache_hit_rate": hit_rate,
        },
        "wal": { "bytes": sum(|s| s.wal_bytes), "last_checkpoint_at": oldest_checkpoint.map(rfc3339) },
        "backup": {
            "enabled": backup.target.is_some(),
            "target": backup.target,
            "last_at": oldest_backup.and_then(rfc3339_ms),
            "last_ok": failing.is_empty(),
            "last_error": failing.first(),
        },
        "encryption": {
            "enabled": encrypted,
            "scheme": encrypted.then_some(crate::crypto::SCHEME),
            "secret": engine.encryption(),
        },
        "applications": all.len(),
        "databases": databases,
        "postgresql": state.options.pg_port.map(|port| json!({ "port": port, "tls": state.options.pg_tls })),
        "services": services,
    })
}

pub(crate) async fn route(state: &Arc<State>, req: Request<Incoming>, path: &str) -> Response<HttpBody> {
    let Some(view) = access(state, &req) else {
        return unauthorized("the status needs an application's token or a console session");
    };
    if path == "/api/status" {
        // A few at once, off the runtime: the status waits for each
        // application's writer, which a long write holds.
        let Ok(Ok(_slot)) = tokio::time::timeout(ADMISSION_WAIT, state.status_slots.clone().acquire_owned()).await
        else {
            return error_response(&busy("the status is being made for many clients: try again in a moment"));
        };
        let state = state.clone();
        let body = unblocked(move || status(&state, &view)).await.unwrap_or(Json::Null);
        return json_response(200, &body);
    }
    stream(state.clone(), view)
}

/// The status of each view that has a stream open, made once a second for
/// all of them on one thread: however many dashboards are open, each
/// engine is looked at once a second, and a long write in one application
/// holds one thread, not one per stream.
#[derive(Default)]
pub(crate) struct Hub {
    inner: Mutex<Views>,
}

#[derive(Default)]
struct Views {
    /// Each view watched, and its last status as text.
    list: Vec<(View, watch::Sender<Option<Arc<str>>>)>,
    /// A task makes them.
    running: bool,
}

impl Hub {
    /// The status of `view`, as it is made.
    fn watch(state: &Arc<State>, view: View) -> watch::Receiver<Option<Arc<str>>> {
        let mut views = state.status_hub.inner.lock().unwrap_or_else(|e| e.into_inner());
        let known = views.list.iter().find(|(v, _)| v.same(&view)).map(|(_, tx)| tx.subscribe());
        let rx = known.unwrap_or_else(|| {
            let (tx, rx) = watch::channel(None);
            views.list.push((view, tx));
            rx
        });
        if !views.running {
            views.running = true;
            tokio::spawn(make_statuses(state.clone()));
        }
        rx
    }
}

/// Makes the status of every view watched, once a second, until none is.
async fn make_statuses(state: Arc<State>) {
    /// The task is marked as gone however it ends (a panic included).
    struct Gone<'a>(&'a Hub, bool);
    impl Drop for Gone<'_> {
        fn drop(&mut self) {
            if self.1 {
                self.0.inner.lock().unwrap_or_else(|e| e.into_inner()).running = false;
            }
        }
    }
    let mut gone = Gone(&state.status_hub, true);
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let watched: Vec<(View, watch::Sender<Option<Arc<str>>>)> = {
            let mut views = state.status_hub.inner.lock().unwrap_or_else(|e| e.into_inner());
            views.list.retain(|(_, tx)| tx.receiver_count() > 0);
            if views.list.is_empty() {
                views.running = false;
                gone.1 = false;
                return;
            }
            views.list.clone()
        };
        let (s, list) = (state.clone(), watched.clone());
        let made = unblocked(move || {
            let every = Every::of(&s);
            list.iter().map(|(view, _)| Arc::<str>::from(status_of(&s, view, &every).to_string())).collect::<Vec<_>>()
        })
        .await;
        if let Ok(made) = made {
            for ((_, tx), text) in watched.iter().zip(made) {
                tx.send_replace(Some(text));
            }
        }
    }
}

/// Server-Sent Events: `status` every second, `log` for each engine event
/// (of the applications in view, each line naming its application), a
/// comment every 15 seconds so proxies keep the connection.
fn stream(state: Arc<State>, view: View) -> Response<HttpBody> {
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        if tx.send(hyper::body::Bytes::from_static(b"retry: 2000\n\n")).await.is_err() {
            return;
        }
        // Events already there are history: only new ones are sent.
        let mut seen: HashMap<String, u64> =
            view.apps(&state).iter().map(|a| (a.name.clone(), a.engine.events().1)).collect();
        let mut status = Hub::watch(&state, view.clone());
        // A status made already goes at once.
        if status.borrow().is_some() {
            status.mark_changed();
        }
        let mut n: u64 = 0;
        while status.changed().await.is_ok() {
            let Some(body) = status.borrow_and_update().clone() else { continue };
            if tx.send(sse("status", &body)).await.is_err() {
                return;
            }
            for app in view.apps(&state) {
                let (events, seq) = app.engine.events();
                // An application created since the stream opened: from its start.
                let last = seen.get(&app.name).copied().unwrap_or(0);
                let fresh = (seq - last).min(events.len() as u64) as usize;
                for e in &events[events.len() - fresh..] {
                    let line =
                        json!({ "at": rfc3339_ms(e.at), "level": e.level, "message": e.message, "app": app.name });
                    if tx.send(sse("log", &line.to_string())).await.is_err() {
                        return;
                    }
                }
                seen.insert(app.name.clone(), seq);
            }
            n += 1;
            if n.is_multiple_of(15) && tx.send(hyper::body::Bytes::from_static(b": ping\n\n")).await.is_err() {
                return;
            }
        }
    });
    let mut res = Response::new(ChannelBody(rx).boxed());
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert("x-accel-buffering", HeaderValue::from_static("no"));
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    /// However many streams show a view, its status is made once a second,
    /// for all of them; once nobody watches, nothing is made.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_streams_of_a_view_share_one_status() {
        let dir = std::env::temp_dir().join(format!("nuage-status-hub-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let options = crate::EngineOptions { sync: false, ..crate::EngineOptions::default() };
        let server = super::super::Server::new(crate::Engine::open(&dir, options).unwrap(), Default::default());
        let state = &server.state;
        let mut a = Hub::watch(state, View::All);
        let mut b = Hub::watch(state, View::All);
        let public = Hub::watch(state, View::Public);
        assert_eq!(state.status_hub.inner.lock().unwrap().list.len(), 2, "two views, not three streams");
        for rx in [&mut a, &mut b] {
            tokio::time::timeout(Duration::from_secs(5), rx.changed()).await.expect("a status").unwrap();
        }
        assert!(Arc::ptr_eq(a.borrow().as_ref().unwrap(), b.borrow().as_ref().unwrap()), "made once");
        assert!(a.borrow().as_ref().unwrap().contains("\"databases\""));
        drop((a, b, public));
        let started = std::time::Instant::now();
        while state.status_hub.inner.lock().unwrap().running {
            assert!(started.elapsed() < Duration::from_secs(5), "still made for nobody");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(state.status_hub.inner.lock().unwrap().list.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
