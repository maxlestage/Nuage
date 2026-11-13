//! The dashboard's connection to the server: the first status with `GET /api/status`, then
//! the live stream `GET /api/status/stream` (Server-Sent Events).
//!
//! * `401`: the status is private, the dashboard asks the visitor to sign in;
//! * the stream drops: "Reconnecting…" while the browser reconnects (the server asks for
//!   `retry: 2000`), a new connection if the browser gives up;
//! * no network: "Offline", the last status stays on screen marked stale, and the
//!   dashboard reconnects as soon as the network is back;
//! * the last status is saved on the device (`localStorage`), so that the dashboard opens on
//!   it, marked stale, even offline.

use std::cell::{Cell, RefCell};

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{EventSource, MessageEvent, RequestCache, RequestCredentials, RequestInit};

use super::{after, every, on, window};
use crate::dashboard::{Conn, Live};
use crate::status::{LogLine, Status};

const STATUS_URL: &str = "/api/status";
const STREAM_URL: &str = "/api/status/stream";
/// The `localStorage` key of the last status: `{"at": ms, "status": {…}}`.
const SAVED: &str = "nuage-site:last-status";

thread_local! {
    static STREAM: RefCell<Option<EventSource>> = const { RefCell::new(None) };
    /// A connection attempt is running or scheduled.
    static PENDING: Cell<bool> = const { Cell::new(false) };
    static LAST_SAVE: Cell<f64> = const { Cell::new(0.0) };
    /// Connection attempts that failed in a row for want of network.
    static FAILURES: Cell<u32> = const { Cell::new(0) };
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Saved {
    at: f64,
    status: Status,
    /// The sparkline's last minute.
    #[serde(default)]
    history: Vec<f64>,
}

fn now() -> f64 {
    js_sys::Date::now()
}

fn online() -> bool {
    window().navigator().on_line()
}

pub fn start(live: Live) {
    live.now.set(now());
    restore(live);
    every(1000, move || live.now.set(now()));
    let win: web_sys::EventTarget = window().into();
    on(&win, "online", move |_| {
        if live.conn.get() != Conn::Live {
            connect(live);
        }
    });
    on(&win, "offline", move |_| {
        if live.conn.get() != Conn::Unauthorized {
            live.conn.set(Conn::Offline);
        }
    });
    connect(live);
}

/// The status saved during an earlier visit, shown (stale) until the server answers.
fn restore(live: Live) {
    let Some(storage) = window().local_storage().ok().flatten() else { return };
    let Some(saved) = storage.get_item(SAVED).ok().flatten() else { return };
    if let Ok(saved) = serde_json::from_str::<Saved>(&saved) {
        live.history.set(saved.history);
        live.data.set(Some(saved.status));
        live.updated.set(Some(saved.at));
        live.from_cache.set(true);
    }
}

/// Saves the status on the device, at most every 5 seconds.
fn save(live: Live, status: &Status, at: f64) {
    if at - LAST_SAVE.get() < 5_000.0 {
        return;
    }
    LAST_SAVE.set(at);
    let Some(storage) = window().local_storage().ok().flatten() else { return };
    if let Ok(json) = serde_json::to_string(&Saved { at, status: status.clone(), history: live.history.get() }) {
        let _ = storage.set_item(SAVED, &json);
    }
}

fn apply(live: Live, status: Status) {
    let at = now();
    save(live, &status, at);
    live.apply(status, at);
}

/// Tries again in `ms` milliseconds (once, whatever asks for it meanwhile).
fn retry(live: Live, ms: i32) {
    if PENDING.replace(true) {
        return;
    }
    after(ms, move || {
        PENDING.set(false);
        connect(live);
    });
}

fn close_stream() {
    STREAM.with(|s| {
        if let Some(stream) = s.borrow_mut().take() {
            stream.close();
        }
    });
}

/// Reads the status once, then follows the stream.
fn connect(live: Live) {
    close_stream();
    if live.conn.get() == Conn::Live {
        live.conn.set(Conn::Reconnecting);
    }
    wasm_bindgen_futures::spawn_local(async move {
        match fetch_status().await {
            Ok((200, body)) => match serde_json::from_str::<Status>(&body) {
                Ok(status) => {
                    FAILURES.set(0);
                    apply(live, status);
                    open_stream(live);
                }
                Err(_) => {
                    live.conn.set(Conn::Reconnecting);
                    retry(live, 5_000);
                }
            },
            Ok((401 | 403, _)) => {
                live.conn.set(Conn::Unauthorized);
            }
            Ok(_) => {
                // The server answers but not with a status (starting, restarting…).
                live.conn.set(Conn::Reconnecting);
                retry(live, 3_000);
            }
            Err(()) => {
                // No answer at all: offline, or the server is unreachable for a while.
                let failures = FAILURES.get() + 1;
                FAILURES.set(failures);
                live.conn.set(if !online() || failures >= 2 { Conn::Offline } else { Conn::Reconnecting });
                retry(live, if failures >= 2 { 5_000 } else { 2_000 });
            }
        }
    });
}

/// `GET /api/status`: (status code, body), `Err` without network.
async fn fetch_status() -> Result<(u16, String), ()> {
    let init = RequestInit::new();
    init.set_method("GET");
    init.set_cache(RequestCache::NoStore);
    init.set_credentials(RequestCredentials::SameOrigin);
    let request = web_sys::Request::new_with_str_and_init(STATUS_URL, &init).map_err(|_| ())?;
    let _ = request.headers().set("accept", "application/json");
    let response = JsFuture::from(window().fetch_with_request(&request)).await.map_err(|_| ())?;
    let response: web_sys::Response = response.dyn_into().map_err(|_| ())?;
    let status = response.status();
    let body = match response.text() {
        Ok(text) => JsFuture::from(text).await.ok().and_then(|t| t.as_string()).unwrap_or_default(),
        Err(_) => String::new(),
    };
    Ok((status, body))
}

fn listen(stream: &EventSource, event: &str, f: impl FnMut(MessageEvent) + 'static) {
    let mut f = f;
    let closure = Closure::<dyn FnMut(web_sys::Event)>::new(move |e: web_sys::Event| {
        if let Ok(message) = e.dyn_into::<MessageEvent>() {
            f(message);
        }
    });
    let _ = stream.add_event_listener_with_callback(event, closure.as_ref().unchecked_ref());
    closure.forget();
}

fn open_stream(live: Live) {
    let Ok(stream) = EventSource::new(STREAM_URL) else {
        retry(live, 5_000);
        return;
    };
    listen(&stream, "status", move |e| {
        if let Some(status) = e.data().as_string().and_then(|d| serde_json::from_str::<Status>(&d).ok()) {
            apply(live, status);
        }
    });
    listen(&stream, "log", move |e| {
        if let Some(line) = e.data().as_string().and_then(|d| serde_json::from_str::<LogLine>(&d).ok()) {
            live.push_log(line);
        }
    });
    let opened: web_sys::EventTarget = stream.clone().into();
    on(&opened, "open", move |_| live.conn.set(Conn::Live));
    let failing = stream.clone();
    on(&opened, "error", move |_| {
        if !online() {
            live.conn.set(Conn::Offline);
        } else if live.conn.get() != Conn::Unauthorized {
            live.conn.set(Conn::Reconnecting);
        }
        // CLOSED: the browser gave up (an error status, 401…). Start over, which also
        // tells a lost session from a server that is down.
        if failing.ready_state() == EventSource::CLOSED {
            failing.close();
            retry(live, 3_000);
        }
    });
    STREAM.with(|s| *s.borrow_mut() = Some(stream));
}
