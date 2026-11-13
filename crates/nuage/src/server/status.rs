//! `/api/status` and `/api/status/stream`: the live state of the server.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};

use bytes_helper::sse;
use hyper::body::Incoming;
use hyper::header::{self, HeaderValue};
use hyper::{Request, Response};
use serde_json::{Value as Json, json};
use tokio::sync::mpsc;

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
enum Access {
    Full,
    Public,
    Denied,
}

fn access(state: &State, req: &Request<Incoming>) -> Access {
    if state.has_token(req.headers()) || state.admin.is_logged_in(req.headers()) {
        Access::Full
    } else if state.options.public_status {
        Access::Public
    } else {
        Access::Denied
    }
}

/// The status JSON (the shape the site and the console read).
pub(crate) fn status(state: &State, full: bool) -> Json {
    let engine = &state.engine;
    let stats = engine.stats();
    let backup = engine.backup_status();
    let (per_s, errors_per_s) = state.metrics.rates();
    let (p50, p99) = state.metrics.percentiles();
    let uptime = engine.uptime().as_secs();
    let open = state.metrics.open_connections.load(Ordering::Relaxed);
    let active = state.metrics.active_requests.load(Ordering::Relaxed);

    let (backup_state, backup_detail) = match (&backup.target, backup.last_ok) {
        (None, _) => ("ok", "off".to_owned()),
        (Some(_), true) => (
            "ok",
            backup.last_at.map_or("waiting for the first copy".to_owned(), |at| {
                format!("last {}", ago(SystemTime::UNIX_EPOCH + Duration::from_millis(at as u64)))
            }),
        ),
        (Some(_), false) => ("degraded", backup.last_error.clone().unwrap_or_else(|| "failed".into())),
    };
    let wal_detail = stats.last_checkpoint.map_or("no checkpoint yet".to_owned(), |t| format!("checkpoint {}", ago(t)));
    let compaction = match engine.last_compaction() {
        Some((at, true)) => rfc3339_ms(at).map_or("done".into(), |t| format!("last run {t}")),
        _ => "idle".into(),
    };
    let services = vec![
        json!({ "name": "engine", "status": "ok", "detail": format!("{} pages, cache {:.0} %", stats.pages, stats.cache_hit_rate * 100.0) }),
        json!({ "name": "http", "status": "ok", "detail": format!("{per_s:.1} req/s") }),
        json!({ "name": "wal", "status": "ok", "detail": wal_detail }),
        json!({ "name": "backup", "status": backup_state, "detail": backup_detail }),
        json!({ "name": "compaction", "status": "ok", "detail": compaction }),
    ];
    let overall = if services.iter().all(|s| s["status"] == "ok") { "ok" } else { "degraded" };

    if !full {
        return json!({
            "name": "Nuage DB",
            "status": overall,
            "uptime_s": uptime,
            "started_at": rfc3339(engine.started_at()),
            "services": services.iter().map(|s| json!({ "name": s["name"], "status": s["status"] })).collect::<Vec<_>>(),
        });
    }

    let db = Embedded::from_engine(engine.clone());
    let tables = db.tables().unwrap_or_default();
    let name = engine.options().name.clone();
    let branches: Vec<Json> = engine
        .branches()
        .iter()
        .map(|b| {
            json!({
                "name": b.name,
                "parent": b.parent.clone().unwrap_or_else(|| name.clone()),
                "created_at": rfc3339_ms(b.created_at),
                "own_bytes": b.own_bytes,
            })
        })
        .collect();
    json!({
        "name": "Nuage DB",
        "version": crate::VERSION,
        "status": overall,
        "uptime_s": uptime,
        "started_at": rfc3339(engine.started_at()),
        "requests": {
            "per_s": per_s,
            "total": state.metrics.total(),
            "errors_per_s": errors_per_s,
            "p50_ms": p50,
            "p99_ms": p99,
        },
        "connections": { "active": active, "idle": open.saturating_sub(active) },
        "storage": {
            "bytes": stats.file_bytes,
            "disk_free_bytes": stats.disk_free_bytes,
            "pages": stats.pages,
            "cache_bytes": stats.cache_bytes,
            "cache_capacity_bytes": stats.cache_capacity_bytes,
            "cache_hit_rate": stats.cache_hit_rate,
        },
        "wal": { "bytes": stats.wal_bytes, "last_checkpoint_at": stats.last_checkpoint.map(rfc3339) },
        "backup": {
            "enabled": backup.target.is_some(),
            "target": backup.target,
            "last_at": backup.last_at.and_then(rfc3339_ms),
            "last_ok": backup.last_ok,
            "last_error": backup.last_error,
        },
        "databases": [{
            "name": name,
            "tables": tables.len(),
            "rows": tables.iter().map(|t| t.rows).sum::<i64>(),
            "bytes": stats.stored_bytes,
            "branches": branches,
        }],
        "services": services,
    })
}

pub(crate) async fn route(state: &Arc<State>, req: Request<Incoming>, path: &str) -> Response<HttpBody> {
    let full = match access(state, &req) {
        Access::Full => true,
        Access::Public => false,
        Access::Denied => return unauthorized("the status needs the token or a console session"),
    };
    if path == "/api/status" {
        let state = state.clone();
        let body = tokio::task::spawn_blocking(move || status(&state, full)).await.unwrap_or(Json::Null);
        return json_response(200, &body);
    }
    stream(state.clone(), full)
}

/// Server-Sent Events: `status` every second, `log` for each engine event,
/// a comment every 15 seconds so proxies keep the connection.
fn stream(state: Arc<State>, full: bool) -> Response<HttpBody> {
    let (tx, rx) = mpsc::channel(16);
    tokio::spawn(async move {
        if tx.send(hyper::body::Bytes::from_static(b"retry: 2000\n\n")).await.is_err() {
            return;
        }
        let (_, mut seen) = state.engine.events();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut n: u64 = 0;
        loop {
            tick.tick().await;
            let s = state.clone();
            let body = tokio::task::spawn_blocking(move || status(&s, full)).await.unwrap_or(Json::Null);
            if tx.send(sse("status", &body.to_string())).await.is_err() {
                return;
            }
            if full {
                let (events, seq) = state.engine.events();
                let fresh = (seq - seen).min(events.len() as u64) as usize;
                for e in &events[events.len() - fresh..] {
                    let line = json!({ "at": rfc3339_ms(e.at), "level": e.level, "message": e.message });
                    if tx.send(sse("log", &line.to_string())).await.is_err() {
                        return;
                    }
                }
                seen = seq;
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
