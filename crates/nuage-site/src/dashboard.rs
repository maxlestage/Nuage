//! The live dashboard of `/app`: an island. The server renders it waiting for its first
//! status ("Connecting…", dashes); in the browser it hydrates from the same code, shows the
//! last known state saved on the device (marked stale), reads `GET /api/status`, then
//! follows `GET /api/status/stream` (Server-Sent Events). The connection logic lives in
//! `client::live` and only runs in the browser.

use active::html::svg;
use active::prelude::*;

use crate::i18n::{Texts, fill, t};
use crate::icons::{Icon, icon};
use crate::routes::{Lang, Page, href};
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
    pub log: State<Vec<LogLine>>,
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
            log: use_state(Vec::new()),
        }
    }

    /// A new status from the server.
    pub fn apply(self, status: Status, now_ms: f64) {
        if let Some(requests) = &status.requests {
            let per_s = requests.per_s;
            self.history.update(|h| {
                h.push(per_s);
                if h.len() > HISTORY {
                    let extra = h.len() - HISTORY;
                    h.drain(..extra);
                }
            });
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
    let Live { data, conn, now, history, log, updated, .. } = live;

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

    let databases = div()
        .class("panel card panel-wide")
        .child(h2().class("dash-h2").child(icon(Icon::Database)).text(tx.db_title))
        .child(ul().class("dbs").children_dyn(move || {
            let dbs = data.with(|d| d.as_ref().and_then(|d| d.databases.clone()).unwrap_or_default());
            if dbs.is_empty() {
                return vec![li().class("muted").text(DASH).into()];
            }
            dbs.into_iter()
                .map(|db| {
                    let branches: Vec<Node> = if db.branches.is_empty() {
                        vec![li().class("muted").text(tx.db_no_branch).into()]
                    } else {
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
                            .collect()
                    };
                    li().class("db")
                        .child(div().class("db-head").child(strong().class("mono").text(db.name.clone())).child(
                            span().class("muted").text(format!(
                                "{} {} · {} {} · {}",
                                db.tables,
                                tx.db_tables,
                                status::group(db.rows),
                                tx.db_rows,
                                status::bytes(db.bytes)
                            )),
                        ))
                        .child(ul().class("branches").children(branches))
                        .into()
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
                            .child(span().class("log-msg").text(line.message))
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
                .child(services)
                .child(
                    div()
                        .class("panels")
                        .bool_attr("hidden", move || !private())
                        .child(storage)
                        .child(wal)
                        .child(backup)
                        .child(databases),
                )
                .child(events),
        )
        .into()
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
