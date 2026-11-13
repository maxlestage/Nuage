//! The live dashboard of `/app`: an island. The server renders it waiting for its first
//! status ("Connecting…", dashes); in the browser it hydrates from the same code, shows the
//! last known state saved on the device (marked stale), reads `GET /api/status`, then
//! follows `GET /api/status/stream` (Server-Sent Events). The connection logic lives in
//! `client::live` and only runs in the browser.
//!
//! A server holds several applications, each with a database of its own: the dashboard
//! shows one card per application (its requests, latency, data, branches, encryption), and
//! the chips above them narrow the cards and the event log to one application.

use active::html::svg;
use active::prelude::*;

use crate::i18n::{Texts, fill, t};
use crate::icons::{Icon, icon};
use crate::routes::{Lang, Page, href};
use crate::status::Database;
use crate::status::{self, LogLine, Service, Status};

/// The state of the connection to the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conn {
    Connecting,
    Live,
    Reconnecting,
    Offline,
    /// `401`: the status is private and the visitor is not signed in.
    Unauthorized,
}

/// Requests per second kept for the sparkline (one per second: the last minute).
pub const HISTORY: usize = 60;
/// Lines kept in the event log.
pub const LOG_LINES: usize = 50;
/// The services the contract lists, in order (shown before the first status arrives).
pub const SERVICES: [&str; 5] = ["engine", "http", "wal", "backup", "compaction"];

/// Everything the dashboard shows, as reactive state.
#[derive(Clone, Copy)]
pub struct Live {
    pub data: State<Option<Status>>,
    pub conn: State<Conn>,
    /// The data comes from the device (saved during an earlier visit), not from the server.
    pub from_cache: State<bool>,
    /// When the last status arrived (ms since the epoch).
    pub updated: State<Option<f64>>,
    /// The clock, ticking every second (ms since the epoch).
    pub now: State<f64>,
    pub history: State<Vec<f64>>,
    /// Requests per second of each application (the last minute), in the server's order.
    pub app_history: State<Vec<(String, Vec<f64>)>>,
    /// The application the cards and the log are narrowed to (`None`: all of them).
    pub filter: State<Option<String>>,
    pub log: State<Vec<LogLine>>,
}

/// Keeps the last [`HISTORY`] values.
fn push_capped(h: &mut Vec<f64>, value: f64) {
    h.push(value);
    if h.len() > HISTORY {
        let extra = h.len() - HISTORY;
        h.drain(..extra);
    }
}

impl Live {
    pub fn new() -> Live {
        Live {
            data: use_state(None),
            conn: use_state(Conn::Connecting),
            from_cache: use_state(false),
            updated: use_state(None),
            now: use_state(0.0),
            history: use_state(Vec::new()),
            app_history: use_state(Vec::new()),
            filter: use_state(None),
            log: use_state(Vec::new()),
        }
    }

    /// A new status from the server.
    pub fn apply(self, status: Status, now_ms: f64) {
        if let Some(requests) = &status.requests {
            let per_s = requests.per_s;
            self.history.update(|h| push_capped(h, per_s));
        }
        if let Some(dbs) = &status.databases {
            self.app_history.update(|history| {
                let next = dbs
                    .iter()
                    .map(|db| {
                        let mut values =
                            history.iter().find(|(n, _)| *n == db.name).map(|(_, v)| v.clone()).unwrap_or_default();
                        push_capped(&mut values, db.requests.as_ref().map_or(0.0, |r| r.per_s));
                        (db.name.clone(), values)
                    })
                    .collect();
                *history = next;
            });
            // An application that is gone cannot stay selected.
            if self.filter.with(|f| f.as_ref().is_some_and(|f| !dbs.iter().any(|d| d.name == *f))) {
                self.filter.set(None);
            }
        }
        self.data.set(Some(status));
        self.updated.set(Some(now_ms));
        self.now.set(now_ms);
        self.from_cache.set(false);
        if self.conn.get() != Conn::Live {
            self.conn.set(Conn::Live);
        }
    }

    /// A line of the event log, newest first.
    pub fn push_log(self, line: LogLine) {
        self.log.update(|log| {
            log.insert(0, line);
            log.truncate(LOG_LINES);
        });
    }

    /// The last status is not current: from the device, or not refreshed for a while.
    pub fn is_stale(self) -> bool {
        if self.data.with(Option::is_none) {
            return false;
        }
        let late = match self.updated.get() {
            Some(at) => self.now.get() - at > 5_000.0,
            None => true,
        };
        self.from_cache.get() || self.conn.get() != Conn::Live || late
    }
}

impl Default for Live {
    fn default() -> Live {
        Live::new()
    }
}

fn lang_of(props: &str) -> Lang {
    Lang::parse(props).unwrap_or(Lang::En)
}

/// `/admin?next=/fr/app/`: sign in, then come back to the dashboard.
pub fn login_href(lang: Lang) -> String {
    format!("/admin?next={}", href(lang, Page::App).replace('/', "%2F"))
}

/// `3 s ago`, `5 min ago`… from two times in milliseconds.
pub fn ago(tx: &Texts, now: f64, then: f64) -> String {
    let s = ((now - then) / 1000.0).max(0.0) as u64;
    match s {
        0..=2 => tx.just_now.to_string(),
        3..=59 => fill(tx.ago[0], s),
        60..=3599 => fill(tx.ago[1], s / 60),
        3600..=86_399 => fill(tx.ago[2], s / 3600),
        _ => fill(tx.ago[3], s / 86_400),
    }
}

/// [`ago`] of an RFC 3339 time, `never` without one.
fn ago_of(tx: &Texts, now: f64, time: Option<&str>) -> String {
    match time.and_then(status::parse_time) {
        Some(then) if now > 0.0 => ago(tx, now, then),
        Some(_) => time.map(status::clock).unwrap_or_default(),
        None => tx.never.to_string(),
    }
}

fn status_class(status: &str) -> &'static str {
    match status {
        "ok" => "is-ok",
        "degraded" => "is-degraded",
        "down" => "is-down",
        _ => "is-unknown",
    }
}

fn status_text(tx: &Texts, status: &str) -> &'static str {
    match status {
        "ok" => tx.status_ok,
        "degraded" => tx.status_degraded,
        "down" => tx.status_down,
        _ => tx.status_unknown,
    }
}

fn service_state(tx: &Texts, status: &str) -> &'static str {
    match status {
        "ok" => tx.service_states[0],
        "degraded" => tx.service_states[1],
        "down" => tx.service_states[2],
        _ => tx.service_states[3],
    }
}

fn service_label(tx: &Texts, name: &str) -> String {
    SERVICES.iter().position(|s| *s == name).map_or_else(|| name.to_string(), |i| tx.service_names[i].to_string())
}

const DASH: &str = "—";

/// The sparkline's points (`x,y …`) in a 120×36 box.
pub fn spark_points(values: &[f64]) -> String {
    if values.is_empty() {
        return "0,35 120,35".into();
    }
    let max = values.iter().copied().fold(f64::MIN, f64::max);
    let min = values.iter().copied().fold(f64::MAX, f64::min);
    // Room above and below, and a flat line in the middle when nothing changes.
    let (low, high) = if max - min < 0.5 { (min - 1.0, max + 1.0) } else { (min - (max - min) * 0.15, max) };
    let y = |v: f64| 34.0 - (v - low) / (high - low) * 30.0;
    // The line spans the whole width, however many seconds it holds yet.
    let values = if values.len() == 1 { vec![values[0], values[0]] } else { values.to_vec() };
    let step = 120.0 / (values.len() - 1) as f64;
    let mut out = String::new();
    for (i, v) in values.iter().enumerate() {
        out.push_str(&format!("{:.1},{:.1} ", i as f64 * step, y(*v)));
    }
    out.trim_end().to_string()
}

/// The island: the whole dashboard.
pub fn dashboard(props: &str) -> Node {
    let lang = lang_of(props);
    let tx = t(lang);
    let live = Live::new();
    let Live { data, conn, now, history, app_history, filter, log, updated, .. } = live;

    // A value of the status, or a dash.
    let val =
        move |f: fn(&Status) -> Option<String>| move || data.with(|d| d.as_ref().and_then(f)).unwrap_or(DASH.into());
    let private = move || data.with(|d| d.as_ref().is_none_or(|d| !d.is_public()));

    let badge = div()
        .class("dash-badge")
        .attr("role", "status")
        .class_if("is-ok", move || data.with(|d| d.as_ref().is_some_and(|d| d.status == "ok")))
        .class_if("is-degraded", move || data.with(|d| d.as_ref().is_some_and(|d| d.status == "degraded")))
        .class_if("is-down", move || data.with(|d| d.as_ref().is_some_and(|d| d.status == "down")))
        .class_if("is-stale", move || live.is_stale())
        .child(span().class("dot").attr("aria-hidden", "true"))
        .child(span().class("dash-badge-text").text_dyn(move || {
            data.with(|d| d.as_ref().map_or(tx.status_unknown, |d| status_text(tx, &d.status)).to_string())
        }));

    let conn_pill = div()
        .class("dash-conn")
        .class_if("is-live", move || conn.get() == Conn::Live)
        .class_if("is-offline", move || conn.get() == Conn::Offline)
        .class_if("is-reconnecting", move || matches!(conn.get(), Conn::Reconnecting | Conn::Connecting))
        .child(span().class("pulse").attr("aria-hidden", "true"))
        .child(span().text_dyn(move || {
            match conn.get() {
                Conn::Connecting => tx.conn_connecting,
                Conn::Live => tx.conn_live,
                Conn::Reconnecting => tx.conn_reconnecting,
                Conn::Offline => tx.conn_offline,
                Conn::Unauthorized => tx.login_title,
            }
            .to_string()
        }))
        .child(span().class("stale-tag").bool_attr("hidden", move || !live.is_stale()).text(tx.stale));

    let updated_line =
        p().class("dash-updated").child(icon(Icon::Clock)).child(span().text(format!("{} ", tx.updated))).child(
            el("time")
                .attr_dyn("datetime", move || updated.get().map(status::format_time).unwrap_or_default())
                .text_dyn(move || match updated.get() {
                    Some(at) => ago(tx, now.get(), at),
                    None => tx.never.to_string(),
                }),
        );

    let meta = div()
        .class("dash-meta")
        .child(span().child(span().class("muted").text(format!("{} ", tx.uptime))).child(strong().text_dyn(
            move || {
                data.with(|d| {
                    d.as_ref().and_then(|d| d.uptime_s).map_or(DASH.into(), |s| status::duration(s, tx.day_unit))
                })
            },
        )))
        .child(
            span()
                .bool_attr("hidden", move || data.with(|d| d.as_ref().is_none_or(|d| d.version.is_none())))
                .child(span().class("muted").text(format!("{} ", tx.version)))
                .child(strong().text_dyn(val(|d| d.version.clone()))),
        )
        .child(
            span()
                .class("enc-pill")
                .class_if("is-off", move || {
                    data.with(|d| d.as_ref().and_then(|d| d.encryption.as_ref()).is_some_and(|e| !e.enabled))
                })
                .bool_attr("hidden", move || data.with(|d| d.as_ref().is_none_or(|d| d.encryption.is_none())))
                .attr_dyn("title", move || {
                    data.with(|d| d.as_ref().and_then(|d| d.encryption.as_ref()).and_then(|e| e.scheme.clone()))
                        .unwrap_or_default()
                })
                .child(icon(Icon::Lock))
                .child(span().text_dyn(move || {
                    let on = data.with(|d| d.as_ref().and_then(|d| d.encryption.as_ref()).is_some_and(|e| e.enabled));
                    (if on { tx.enc_on } else { tx.enc_off }).to_string()
                })),
        );

    let login = div()
        .class("dash-login card")
        .bool_attr("hidden", move || conn.get() != Conn::Unauthorized)
        .child(span().class("dash-login-icon").child(icon(Icon::Lock)))
        .child(h2().text(tx.login_title))
        .child(p().text(tx.login_text))
        .child(a().class("btn btn-primary").href(login_href(lang)).text(tx.login_button).child(icon(Icon::ArrowRight)));

    let public_note = p()
        .class("dash-public")
        .bool_attr("hidden", move || data.with(|d| d.as_ref().is_none_or(|d| !d.is_public())))
        .child(icon(Icon::Lock))
        .child(span().text(tx.public_note))
        .child(a().href(login_href(lang)).text(tx.login_button));

    let kpis = div()
        .class("kpis")
        .bool_attr("hidden", move || !private())
        .child(
            div()
                .class("kpi card kpi-requests")
                .child(h3().class("kpi-label").text(tx.kpi_requests))
                .child(p().class("kpi-value").text_dyn(val(|d| d.requests.as_ref().map(|r| status::rate(r.per_s)))))
                .child(
                    svg()
                        .class("spark")
                        .attr("viewBox", "0 0 120 36")
                        .attr("preserveAspectRatio", "none")
                        .attr("role", "img")
                        .attr("aria-label", tx.chart_label)
                        .child(
                            polyline()
                                .class("spark-line")
                                .attr("fill", "none")
                                .attr_dyn("points", move || history.with(|h| spark_points(h))),
                        ),
                )
                .child(
                    p().class("kpi-sub")
                        .child(span().text_dyn(val(|d| d.requests.as_ref().map(|r| status::group(r.total)))))
                        .child(span().class("muted").text(format!(" {} · ", tx.kpi_requests_total)))
                        .child(span().text_dyn(val(|d| d.requests.as_ref().map(|r| format!("{:.2}", r.errors_per_s)))))
                        .child(span().class("muted").text(format!(" {}", tx.kpi_errors))),
                ),
        )
        .child(
            div().class("kpi card").child(h3().class("kpi-label").text(tx.kpi_latency)).child(
                div()
                    .class("kpi-pair")
                    .child(
                        p().child(span().class("kpi-tag").text("p50")).child(
                            span()
                                .class("kpi-value sm")
                                .text_dyn(val(|d| d.requests.as_ref().map(|r| status::millis(r.p50_ms)))),
                        ),
                    )
                    .child(
                        p().child(span().class("kpi-tag").text("p99")).child(
                            span()
                                .class("kpi-value sm")
                                .text_dyn(val(|d| d.requests.as_ref().map(|r| status::millis(r.p99_ms)))),
                        ),
                    ),
            ),
        )
        .child(
            div().class("kpi card").child(h3().class("kpi-label").text(tx.kpi_connections)).child(
                div()
                    .class("kpi-pair")
                    .child(
                        p().child(span().class("kpi-tag").text(tx.conn_active)).child(
                            span()
                                .class("kpi-value sm")
                                .text_dyn(val(|d| d.connections.as_ref().map(|c| c.active.to_string()))),
                        ),
                    )
                    .child(
                        p().child(span().class("kpi-tag").text(tx.conn_idle)).child(
                            span()
                                .class("kpi-value sm")
                                .text_dyn(val(|d| d.connections.as_ref().map(|c| c.idle.to_string()))),
                        ),
                    ),
            ),
        );

    let services = section()
        .class("dash-section")
        .attr("aria-labelledby", "services-title")
        .child(h2().id("services-title").class("dash-h2").text(tx.services_title))
        .child(ul().class("services").children_dyn(move || {
            let list: Vec<Service> = data.with(|d| match d {
                Some(d) if !d.services.is_empty() => d.services.clone(),
                _ => SERVICES
                    .iter()
                    .map(|name| Service { name: name.to_string(), status: String::new(), detail: None })
                    .collect(),
            });
            list.into_iter()
                .map(|s| {
                    li().class(format!("service card {}", status_class(&s.status)))
                        .child(
                            div()
                                .class("service-head")
                                .child(span().class("dot").attr("aria-hidden", "true"))
                                .child(h3().text(service_label(tx, &s.name)))
                                .child(span().class("service-state").text(service_state(tx, &s.status))),
                        )
                        .child(p().class("service-detail").text(s.detail.unwrap_or_else(|| DASH.into())))
                        .into()
                })
                .collect()
        }));

    let storage = div()
        .class("panel card")
        .child(h2().class("dash-h2").child(icon(Icon::HardDrive)).text(tx.storage_title))
        .child(div().class("bar").attr("aria-hidden", "true").child(span().class("bar-fill").style_dyn(
            "width",
            move || {
                data.with(|d| {
                    let s = d.as_ref().and_then(|d| d.storage.as_ref());
                    let pct = s.map_or(0.0, |s| {
                        let total = (s.bytes + s.disk_free_bytes).max(1) as f64;
                        s.bytes as f64 / total * 100.0
                    });
                    format!("{pct:.1}%")
                })
            },
        )))
        .child(
            dl().class("facts")
                .child(
                    div()
                        .child(dt().text(tx.storage_data))
                        .child(dd().text_dyn(val(|d| d.storage.as_ref().map(|s| status::bytes(s.bytes))))),
                )
                .child(
                    div()
                        .child(dt().text(tx.storage_free))
                        .child(dd().text_dyn(val(|d| d.storage.as_ref().map(|s| status::bytes(s.disk_free_bytes))))),
                )
                .child(div().child(dt().text(tx.cache_hit)).child(
                    dd().class("big").text_dyn(val(|d| d.storage.as_ref().map(|s| status::percent(s.cache_hit_rate)))),
                ))
                .child(div().child(dt().text(tx.cache_size)).child(dd().text_dyn(val(|d| {
                    d.storage.as_ref().map(|s| {
                        format!("{} / {}", status::bytes(s.cache_bytes), status::bytes(s.cache_capacity_bytes))
                    })
                }))))
                .child(
                    div()
                        .child(dt().text(tx.storage_pages))
                        .child(dd().text_dyn(val(|d| d.storage.as_ref().map(|s| status::group(s.pages))))),
                ),
        );

    let wal =
        div().class("panel card").child(h2().class("dash-h2").child(icon(Icon::Refresh)).text(tx.wal_title)).child(
            dl().class("facts")
                .child(div().child(dt().text(tx.last_checkpoint)).child(dd().class("big").text_dyn(move || {
                    let n = now.get();
                    data.with(|d| match d.as_ref().and_then(|d| d.wal.as_ref()) {
                        Some(w) => ago_of(tx, n, w.last_checkpoint_at.as_deref()),
                        None => DASH.into(),
                    })
                })))
                .child(
                    div()
                        .child(dt().text(tx.wal_size))
                        .child(dd().text_dyn(val(|d| d.wal.as_ref().map(|w| status::bytes(w.bytes))))),
                ),
        );

    let backup = div()
        .class("panel card")
        .class_if("is-error", move || {
            data.with(|d| d.as_ref().and_then(|d| d.backup.as_ref()).is_some_and(|b| b.last_ok == Some(false)))
        })
        .child(h2().class("dash-h2").child(icon(Icon::Save)).text(tx.backup_title))
        .child(
            dl().class("facts")
                .child(
                    div().child(dt().text(tx.backup_last)).child(
                        dd().class("big")
                            .child(span().class("backup-result").text_dyn(move || {
                                data.with(|d| match d.as_ref().and_then(|d| d.backup.as_ref()) {
                                    Some(b) if !b.enabled => tx.backup_off.to_string(),
                                    Some(b) => match b.last_ok {
                                        Some(true) => tx.backup_ok.to_string(),
                                        Some(false) => tx.backup_failed.to_string(),
                                        None => tx.never.to_string(),
                                    },
                                    None => DASH.into(),
                                })
                            }))
                            .child(span().class("muted backup-when").text_dyn(move || {
                                let n = now.get();
                                data.with(|d| match d.as_ref().and_then(|d| d.backup.as_ref()) {
                                    Some(b) if b.enabled && b.last_at.is_some() => {
                                        format!(" · {}", ago_of(tx, n, b.last_at.as_deref()))
                                    }
                                    _ => String::new(),
                                })
                            })),
                    ),
                )
                .child(
                    div()
                        .class("backup-error")
                        .child(dd().text_dyn(val(|d| d.backup.as_ref().and_then(|b| b.last_error.clone()))))
                        .bool_attr("hidden", move || {
                            data.with(|d| {
                                d.as_ref().and_then(|d| d.backup.as_ref()).is_none_or(|b| b.last_error.is_none())
                            })
                        }),
                )
                .child(
                    div()
                        .child(dt().text(tx.backup_target))
                        .child(dd().class("mono").text_dyn(val(|d| d.backup.as_ref().and_then(|b| b.target.clone())))),
                ),
        );

    let chip = move |label: String, value: Option<String>, current: &Option<String>| -> Node {
        let active = *current == value;
        button()
            .attr("type", "button")
            .class(if active { "chip is-active" } else { "chip" })
            .attr("aria-pressed", if active { "true" } else { "false" })
            .text(label)
            .on_click(move |_| filter.set(value.clone()))
            .into()
    };

    let applications = section()
        .class("dash-section apps-section")
        .attr("aria-labelledby", "apps-title")
        .bool_attr("hidden", move || !private())
        .child(
            div()
                .class("apps-head")
                .child(h2().id("apps-title").class("dash-h2").child(icon(Icon::Layers)).text(tx.apps_title))
                .child(p().class("muted apps-count").text_dyn(move || {
                    data.with(|d| {
                        d.as_ref()
                            .and_then(|d| d.applications.or_else(|| d.databases.as_ref().map(|v| v.len() as u64)))
                            .map_or(String::new(), |n| fill(tx.apps_count, n))
                    })
                })),
        )
        .child(div().class("chips").attr("role", "group").attr("aria-label", tx.apps_filter).children_dyn(move || {
            let names: Vec<String> = data.with(|d| {
                d.as_ref()
                    .and_then(|d| d.databases.as_ref())
                    .map_or_else(Vec::new, |v| v.iter().map(|d| d.name.clone()).collect())
            });
            if names.len() < 2 {
                return Vec::new();
            }
            let current = filter.get();
            let mut out = vec![chip(tx.apps_all.to_string(), None, &current)];
            out.extend(names.into_iter().map(|n| chip(n.clone(), Some(n), &current)));
            out
        }))
        .child(ul().class("app-cards").children_dyn(move || {
            let dbs = data.with(|d| d.as_ref().and_then(|d| d.databases.clone()).unwrap_or_default());
            if dbs.is_empty() {
                return vec![li().class("muted").text(DASH).into()];
            }
            let current = filter.get();
            let n = now.get();
            let history = app_history.get();
            dbs.into_iter()
                .filter(|db| current.as_ref().is_none_or(|f| *f == db.name))
                .map(|db| {
                    let values =
                        history.iter().find(|(name, _)| *name == db.name).map(|(_, v)| v.clone()).unwrap_or_default();
                    app_card(tx, &db, &values, n)
                })
                .collect()
        }));

    let events = section()
        .class("panel card panel-wide")
        .attr("aria-labelledby", "log-title")
        .child(h2().id("log-title").class("dash-h2").child(icon(Icon::Activity)).text(tx.log_title))
        .child(ol().class("log").attr("aria-live", "polite").attr("aria-relevant", "additions").children_dyn(
            move || {
                let lines = log.get();
                if lines.is_empty() {
                    return vec![li().class("log-empty muted").text(tx.log_empty).into()];
                }
                let current = filter.get();
                let lines: Vec<LogLine> = lines.into_iter().filter(|l| current.is_none() || l.app == current).collect();
                if lines.is_empty() {
                    return vec![li().class("log-empty muted").text(tx.log_empty).into()];
                }
                lines
                    .into_iter()
                    .map(|line| {
                        let level = match line.level.as_str() {
                            "warn" => "warn",
                            "error" => "error",
                            _ => "info",
                        };
                        li().class(format!("log-line is-{level}"))
                            .child(el("time").attr("datetime", line.at.clone()).text(status::clock(&line.at)))
                            .child(span().class("log-level").text(level))
                            .child(
                                span()
                                    .class("log-msg")
                                    .children(match &line.app {
                                        Some(app) => vec![span().class("log-app mono").text(app.clone()).into()],
                                        None => Vec::<Node>::new(),
                                    })
                                    .child(span().text(line.message)),
                            )
                            .into()
                    })
                    .collect()
            },
        ));

    div()
        .class("dash")
        .class_if("is-locked", move || conn.get() == Conn::Unauthorized)
        .on_mount(move |_| {
            #[cfg(target_arch = "wasm32")]
            crate::client::live::start(live);
        })
        .child(
            div()
                .class("dash-top card")
                .child(div().class("dash-top-row").child(badge).child(conn_pill))
                .child(div().class("dash-top-row").child(updated_line).child(meta)),
        )
        .child(login)
        .child(
            div()
                .class("dash-body")
                .bool_attr("hidden", move || conn.get() == Conn::Unauthorized)
                .child(public_note)
                .child(kpis)
                .child(applications)
                .child(services)
                .child(
                    div()
                        .class("panels")
                        .bool_attr("hidden", move || !private())
                        .child(storage)
                        .child(wal)
                        .child(backup),
                )
                .child(events),
        )
        .into()
}

/// The card of one application.
fn app_card(tx: &'static Texts, db: &Database, history: &[f64], now: f64) -> Node {
    let degraded = db.status.as_deref() == Some("degraded");
    let mut head = div().class("app-card-head").child(strong().class("mono app-name").text(db.name.clone()));
    if db.first {
        head = head.child(span().class("pill").text(tx.app_first));
    }
    head = match db.encrypted {
        Some(true) => {
            head.child(span().class("pill is-ok").child(icon(Icon::Lock)).child(span().text(tx.app_encrypted)))
        }
        Some(false) => head.child(span().class("pill is-bad").text(tx.app_clear)),
        None => head,
    };
    if degraded {
        head = head.child(span().class("pill is-bad").text(tx.app_degraded));
    }
    let requests = db.requests.clone().unwrap_or_default();
    let fact = |label: &str, value: String| div().child(dt().text(label.to_string())).child(dd().text(value));
    let mut card = li()
        .class(if degraded { "app-card card is-error" } else { "app-card card" })
        .attr("data-app", db.name.clone())
        .child(head);
    if let Some(d) = db.description.as_ref().filter(|d| !d.is_empty()) {
        card = card.child(p().class("app-desc muted").text(d.clone()));
    }
    card = card
        .child(
            div()
                .class("app-rate")
                .child(
                    p().child(span().class("kpi-value sm").text(status::rate(requests.per_s)))
                        .child(span().class("muted").text(format!(" {}", tx.kpi_requests))),
                )
                .child(
                    svg()
                        .class("spark spark-sm")
                        .attr("viewBox", "0 0 120 36")
                        .attr("preserveAspectRatio", "none")
                        .attr("role", "img")
                        .attr("aria-label", tx.app_chart)
                        .child(
                            polyline().class("spark-line").attr("fill", "none").attr("points", spark_points(history)),
                        ),
                ),
        )
        .child(
            dl().class("facts app-facts")
                .child(fact("p99", status::millis(requests.p99_ms)))
                .child(fact(tx.kpi_errors, format!("{:.2}", requests.errors_per_s)))
                .child(fact(tx.db_tables, db.tables.to_string()))
                .child(fact(tx.db_rows, status::group(db.rows)))
                .child(fact(tx.app_size, status::bytes(db.file_bytes.max(db.bytes))))
                .child(fact(tx.app_last_request, ago_of(tx, now, db.last_request_at.as_deref()))),
        );
    if !db.branches.is_empty() {
        card = card.child(
            ul().class("branches").children(
                db.branches
                    .iter()
                    .map(|b| {
                        li().class("branch")
                            .child(icon(Icon::Branch))
                            .child(span().class("mono").text(b.name.clone()))
                            .child(span().class("muted").text(format!(
                                " · {} {}",
                                status::bytes(b.own_bytes),
                                tx.db_branch_own
                            )))
                            .into()
                    })
                    .collect::<Vec<Node>>(),
            ),
        );
    }
    card.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_waiting_for_the_first_status() {
        let html = active::island("dashboard", "fr", dashboard).render();
        assert!(html.contains("Connexion…"), "{html}");
        assert!(html.contains("Moteur") && html.contains("Compaction"));
        assert!(html.contains(r#"href="/admin?next=%2Ffr%2Fapp%2F""#));
    }

    #[test]
    fn keeps_a_minute_of_history_and_fifty_log_lines() {
        let live = Live::new();
        for i in 0..80 {
            let status = Status {
                status: "ok".into(),
                requests: Some(crate::status::Requests { per_s: f64::from(i), ..Default::default() }),
                ..Default::default()
            };
            live.apply(status, f64::from(i) * 1000.0);
            live.push_log(LogLine { message: format!("{i}"), ..Default::default() });
        }
        assert_eq!(live.history.get().len(), HISTORY);
        assert_eq!(live.history.get()[HISTORY - 1], 79.0);
        assert_eq!(live.log.get().len(), LOG_LINES);
        assert_eq!(live.log.get()[0].message, "79");
        assert_eq!(live.conn.get(), Conn::Live);
        assert!(!live.is_stale());
        live.now.set(90_000.0);
        assert!(live.is_stale());
    }

    #[test]
    fn one_card_per_application_and_a_filter() {
        let live = Live::new();
        let db = |name: &str, per_s: f64| Database {
            name: name.into(),
            encrypted: Some(true),
            requests: Some(crate::status::Requests { per_s, ..Default::default() }),
            ..Default::default()
        };
        for i in 0..3 {
            let status = Status {
                status: "ok".into(),
                databases: Some(vec![db("nuage", 1.0), db("shop", f64::from(i))]),
                ..Default::default()
            };
            live.apply(status, f64::from(i) * 1000.0);
        }
        let history = live.app_history.get();
        assert_eq!(history.len(), 2);
        assert_eq!(history[1], ("shop".to_string(), vec![0.0, 1.0, 2.0]));
        live.filter.set(Some("shop".into()));
        // The selected application disappears: back to all of them.
        live.apply(
            Status { status: "ok".into(), databases: Some(vec![db("nuage", 1.0)]), ..Default::default() },
            4000.0,
        );
        assert_eq!(live.filter.get(), None);
        let html = app_card(t(Lang::Fr), &db("shop", 2.5), &[1.0, 2.5], 0.0).render();
        assert!(html.contains("shop") && html.contains("chiffrée") && html.contains("2.5"), "{html}");
    }

    #[test]
    fn sparkline_and_relative_times() {
        assert_eq!(spark_points(&[]), "0,35 120,35");
        let points = spark_points(&[1.0, 2.0]);
        assert!(points.ends_with("120.0,4.0"), "{points}");
        assert!(points.starts_with("0.0,30.1"), "{points}");
        let tx = t(Lang::En);
        assert_eq!(ago(tx, 10_000.0, 9_000.0), "just now");
        assert_eq!(ago(tx, 70_000.0, 0.0), "1 min ago");
        assert_eq!(login_href(Lang::En), "/admin?next=%2Fen%2Fapp%2F");
    }
}
