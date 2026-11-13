//! `/api/status` and `/api/status/stream`: the live state of the server.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};

use bytes_helper::sse;
use hyper::body::Incoming;
use hyper::header::{self, HeaderValue};
use hyper::{Request, Response};
use serde_json::{Value as Json, json};
use tokio::sync::mpsc;

use super::apps::App;
use super::{ChannelBody, HttpBody, State, json_response, unauthorized};
use crate::embedded::Embedded;
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
    } else if state.options.public_status {
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
}

fn requests_json(metrics: &super::Metrics) -> Json {
    let (per_s, errors_per_s) = metrics.rates();
    let (p50, p99) = metrics.percentiles();
    json!({ "per_s": per_s, "total": metrics.total(), "errors_per_s": errors_per_s, "p50_ms": p50, "p99_ms": p99 })
}

/// One application, as `databases[]` lists it.
fn app_json(app: &App, env_token: bool) -> Json {
    let engine = &app.engine;
    let stats = engine.stats();
    let backup = engine.backup_status();
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
        "requests": requests_json(&app.metrics),
        "branches": branches,
    })
}

/// The status JSON (the shape the site and the console read).
pub(crate) fn status(state: &State, view: &View) -> Json {
    let engine = &state.engine;
    let all = state.apps.all();
    let every: Vec<_> = all.iter().map(|a| (a.engine.stats(), a.engine.backup_status())).collect();
    let (per_s, _) = state.metrics.rates();
    let uptime = engine.uptime().as_secs();
    let open = state.metrics.open_connections.load(Ordering::Relaxed);
    let active = state.metrics.active_requests.load(Ordering::Relaxed);

    let backup = engine.backup_status();
    let failing: Vec<String> = all
        .iter()
        .zip(&every)
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
    let databases: Vec<Json> = view.apps(state).iter().map(|a| app_json(a, env_token)).collect();
    let sum = |f: fn(&crate::engine::StorageStats) -> u64| every.iter().map(|(s, _)| f(s)).sum::<u64>();
    let first = &every[0].0;
    json!({
        "name": "Nuage DB",
        "version": crate::VERSION,
        "status": overall,
        "uptime_s": uptime,
        "started_at": rfc3339(engine.started_at()),
        "requests": requests_json(&state.metrics),
        "connections": { "active": active, "idle": open.saturating_sub(active) },
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
        "services": services,
    })
}

pub(crate) async fn route(state: &Arc<State>, req: Request<Incoming>, path: &str) -> Response<HttpBody> {
    let Some(view) = access(state, &req) else {
        return unauthorized("the status needs an application's token or a console session");
    };
    if path == "/api/status" {
        let state = state.clone();
        let body = tokio::task::spawn_blocking(move || status(&state, &view)).await.unwrap_or(Json::Null);
        return json_response(200, &body);
    }
    stream(state.clone(), Arc::new(view))
}

/// Server-Sent Events: `status` every second, `log` for each engine event
/// (of the applications in view, each line naming its application), a
/// comment every 15 seconds so proxies keep the connection.
fn stream(state: Arc<State>, view: Arc<View>) -> Response<HttpBody> {
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        if tx.send(hyper::body::Bytes::from_static(b"retry: 2000\n\n")).await.is_err() {
            return;
        }
        // Events already there are history: only new ones are sent.
        let mut seen: HashMap<String, u64> =
            view.apps(&state).iter().map(|a| (a.name.clone(), a.engine.events().1)).collect();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut n: u64 = 0;
        loop {
            tick.tick().await;
            let (s, v) = (state.clone(), view.clone());
            let body = tokio::task::spawn_blocking(move || status(&s, &v)).await.unwrap_or(Json::Null);
            if tx.send(sse("status", &body.to_string())).await.is_err() {
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
