//! The details of a card of the dashboard: a panel over the page (a sheet from the bottom
//! on a phone, a window on a computer) that follows the live status like the cards do.
//! Every card opens one: the requests, the latency, the connections, each application,
//! each service, the storage, the log and the backups.

use active::html::svg;
use active::prelude::*;

use crate::dashboard::{DASH, ago_of, service_label, service_state, status_class};
use crate::i18n::{Texts, fill};
use crate::icons::{Icon, icon};
use crate::status::{self, Database, LogLine, Status};

/// What the panel shows.
#[derive(Clone, Debug, PartialEq)]
pub enum Detail {
    Requests,
    Latency,
    Connections,
    App(String),
    Service(String),
    Storage,
    Wal,
    Backup,
}

/// An invisible button over a whole card: the card opens its details, from a tap, a click
/// or the keyboard.
pub fn opener(detail: State<Option<Detail>>, which: Detail, label: String) -> Element {
    button()
        .attr("type", "button")
        .class("card-open")
        .attr("aria-label", label)
        .attr("aria-haspopup", "dialog")
        .on_click(move |_| open(detail, which.clone()))
}

/// Opens the panel on `which`, and moves the focus to its close button.
pub fn open(detail: State<Option<Detail>>, which: Detail) {
    detail.set(Some(which));
    #[cfg(target_arch = "wasm32")]
    crate::client::after(20, || {
        if let Ok(Some(el)) = crate::client::document().query_selector(".sheet-close") {
            use wasm_bindgen::JsCast;
            if let Ok(el) = el.dyn_into::<web_sys::HtmlElement>() {
                let _ = el.focus();
            }
        }
    });
}

/// The title of the panel.
pub fn title(tx: &Texts, which: &Detail) -> String {
    match which {
        Detail::Requests => tx.kpi_requests.into(),
        Detail::Latency => tx.kpi_latency.into(),
        Detail::Connections => tx.kpi_connections.into(),
        Detail::App(name) => name.clone(),
        Detail::Service(name) => service_label(tx, name),
        Detail::Storage => tx.storage_title.into(),
        Detail::Wal => tx.wal_title.into(),
        Detail::Backup => tx.backup_title.into(),
    }
}

fn help(text: &str) -> Node {
    p().class("sheet-help").text(text.to_string()).into()
}

fn heading(text: &str) -> Node {
    h3().class("sheet-h3").text(text.to_string()).into()
}

fn facts(rows: Vec<(&str, String)>) -> Node {
    dl().class("facts sheet-facts")
        .children(
            rows.into_iter()
                .map(|(label, value)| div().child(dt().text(label.to_string())).child(dd().text(value)).into())
                .collect::<Vec<Node>>(),
        )
        .into()
}

/// A table whose first column opens an application's details.
fn app_table(detail: State<Option<Detail>>, head: Vec<&str>, rows: Vec<(String, Vec<String>)>) -> Node {
    let header =
        el("tr").children(head.into_iter().map(|h| el("th").text(h.to_string()).into()).collect::<Vec<Node>>());
    let body = rows
        .into_iter()
        .map(|(name, cells)| {
            let link = button()
                .attr("type", "button")
                .class("link-btn mono")
                .text(name.clone())
                .on_click(move |_| open(detail, Detail::App(name.clone())));
            el("tr")
                .child(el("td").child(link))
                .children(cells.into_iter().map(|c| el("td").text(c).into()).collect::<Vec<Node>>())
                .into()
        })
        .collect::<Vec<Node>>();
    div()
        .class("sheet-table-wrap")
        .child(el("table").class("sheet-table").child(el("thead").child(header)).child(el("tbody").children(body)))
        .into()
}

/// The points of a line over `values` in a `w`×`h` box, and of the area under it.
pub fn chart_points(values: &[f64], w: f64, h: f64) -> (String, String) {
    if values.is_empty() {
        let line = format!("0,{:.1} {w:.1},{:.1}", h - 1.0, h - 1.0);
        return (line.clone(), format!("{line} {w:.1},{h:.1} 0,{h:.1}"));
    }
    let max = values.iter().copied().fold(f64::MIN, f64::max);
    let min = values.iter().copied().fold(f64::MAX, f64::min).min(0.0);
    let high = if max - min < 1e-9 { min + 1.0 } else { max };
    let y = |v: f64| (h - 2.0) - (v - min) / (high - min) * (h - 8.0);
    let values = if values.len() == 1 { vec![values[0], values[0]] } else { values.to_vec() };
    let step = w / (values.len() - 1) as f64;
    let line: Vec<String> =
        values.iter().enumerate().map(|(i, v)| format!("{:.1},{:.1}", i as f64 * step, y(*v))).collect();
    let line = line.join(" ");
    let area = format!("0,{h:.1} {line} {w:.1},{h:.1}");
    (line, area)
}

/// The last minute, larger, with its peak.
fn chart(tx: &Texts, values: &[f64], label: &str) -> Node {
    let (line, area) = chart_points(values, 300.0, 90.0);
    let peak = values.iter().copied().fold(0.0, f64::max);
    let avg = if values.is_empty() { 0.0 } else { values.iter().sum::<f64>() / values.len() as f64 };
    div()
        .class("sheet-chart")
        .child(
            svg()
                .class("chart")
                .attr("viewBox", "0 0 300 90")
                .attr("preserveAspectRatio", "none")
                .attr("role", "img")
                .attr("aria-label", label.to_string())
                .child(
                    el("line").attr("x1", "0").attr("x2", "300").attr("y1", "30").attr("y2", "30").class("chart-grid"),
                )
                .child(
                    el("line").attr("x1", "0").attr("x2", "300").attr("y1", "60").attr("y2", "60").class("chart-grid"),
                )
                .child(polygon().class("chart-area").attr("points", area))
                .child(polyline().class("chart-line").attr("fill", "none").attr("points", line)),
        )
        .child(p().class("sheet-chart-legend muted").text(format!(
            "{} · {} {} · {} {}",
            tx.detail_last_minute,
            tx.detail_peak,
            status::rate(peak),
            tx.detail_average,
            status::rate(avg)
        )))
        .into()
}

fn when_and_ago(tx: &Texts, now: f64, time: Option<&str>) -> String {
    match time {
        Some(t) => format!("{} · {}", ago_of(tx, now, Some(t)), status::when(t)),
        None => tx.never.to_string(),
    }
}

fn pill(class: &str, text: &str) -> Node {
    span().class(format!("pill {class}")).text(text.to_string()).into()
}

/// What the panel holds for `which`, from the status as it is now.
#[allow(clippy::too_many_arguments)]
pub fn body(
    tx: &'static Texts,
    which: &Detail,
    data: Option<&Status>,
    now: f64,
    history: &[f64],
    app_history: &[(String, Vec<f64>)],
    log: &[LogLine],
    detail: State<Option<Detail>>,
) -> Vec<Node> {
    let Some(d) = data else {
        return vec![p().class("muted").text(DASH).into()];
    };
    let dbs: Vec<Database> = d.databases.clone().unwrap_or_default();
    let history_of =
        |name: &str| app_history.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone()).unwrap_or_default();
    match which {
        Detail::Requests => {
            let r = d.requests.clone().unwrap_or_default();
            let mut rows: Vec<&Database> = dbs.iter().collect();
            rows.sort_by(|a, b| {
                let rate = |db: &Database| db.requests.as_ref().map_or(0.0, |r| r.per_s);
                rate(b).total_cmp(&rate(a))
            });
            vec![
                chart(tx, history, tx.chart_label),
                facts(vec![
                    (tx.detail_now, format!("{}/s", status::rate(r.per_s))),
                    (tx.kpi_requests_total, status::group(r.total)),
                    (tx.kpi_errors, format!("{:.2}", r.errors_per_s)),
                ]),
                help(tx.help_requests),
                heading(tx.detail_by_app),
                app_table(
                    detail,
                    vec![tx.apps_title, tx.kpi_requests, tx.kpi_requests_total, tx.kpi_errors],
                    rows.into_iter()
                        .map(|db| {
                            let r = db.requests.clone().unwrap_or_default();
                            (
                                db.name.clone(),
                                vec![status::rate(r.per_s), status::group(r.total), format!("{:.2}", r.errors_per_s)],
                            )
                        })
                        .collect(),
                ),
            ]
        }
        Detail::Latency => {
            let r = d.requests.clone().unwrap_or_default();
            vec![
                facts(vec![("p50", status::millis(r.p50_ms)), ("p99", status::millis(r.p99_ms))]),
                help(tx.help_latency),
                heading(tx.detail_by_app),
                app_table(
                    detail,
                    vec![tx.apps_title, "p50", "p99"],
                    dbs.iter()
                        .map(|db| {
                            let r = db.requests.clone().unwrap_or_default();
                            (db.name.clone(), vec![status::millis(r.p50_ms), status::millis(r.p99_ms)])
                        })
                        .collect(),
                ),
            ]
        }
        Detail::Connections => {
            let c = d.connections.clone().unwrap_or_default();
            let pg = match &d.postgresql {
                Some(pg) if pg.tls => fill(tx.detail_port_tls, pg.port),
                Some(pg) => fill(tx.detail_port_clear, pg.port),
                None => tx.detail_pg_off.to_string(),
            };
            vec![
                facts(vec![(tx.conn_active, c.active.to_string()), (tx.conn_idle, c.idle.to_string())]),
                help(tx.help_connections),
                heading(tx.detail_protocols),
                facts(vec![("HTTP", "/v1, /v1/sql".to_string()), ("PostgreSQL", pg)]),
                heading(tx.detail_last_connection),
                app_table(
                    detail,
                    vec![tx.apps_title, tx.detail_last_connection],
                    dbs.iter().map(|db| (db.name.clone(), vec![connection_text(tx, now, db)])).collect(),
                ),
            ]
        }
        Detail::App(name) => {
            let Some(db) = dbs.iter().find(|db| db.name == *name) else {
                return vec![p().class("muted").text(DASH).into()];
            };
            app_body(tx, db, &history_of(name), now, log)
        }
        Detail::Service(name) => {
            let s = d.services.iter().find(|s| s.name == *name);
            let state = s.map_or("", |s| s.status.as_str());
            let mut out = vec![
                div()
                    .class("sheet-pills")
                    .child(span().class(format!("pill {}", status_class(state))).text(service_state(tx, state)))
                    .into(),
            ];
            if let Some(detail_text) = s.and_then(|s| s.detail.clone()) {
                out.push(p().class("sheet-lead mono").text(detail_text).into());
            }
            let (text, related) = match name.as_str() {
                "engine" => (tx.help_engine, Some(Detail::Storage)),
                "http" => (tx.help_http, Some(Detail::Requests)),
                "wal" => (tx.help_wal, Some(Detail::Wal)),
                "backup" => (tx.help_backup, Some(Detail::Backup)),
                "compaction" => (tx.help_compaction, Some(Detail::Storage)),
                _ => ("", None),
            };
            if !text.is_empty() {
                out.push(help(text));
            }
            if let Some(related) = related.filter(|_| d.storage.is_some()) {
                let label = title(tx, &related);
                out.push(
                    button()
                        .attr("type", "button")
                        .class("btn btn-ghost btn-sm")
                        .text(label)
                        .child(icon(Icon::ArrowRight))
                        .on_click(move |_| open(detail, related.clone()))
                        .into(),
                );
            }
            out
        }
        Detail::Storage => {
            let s = d.storage.clone().unwrap_or_default();
            let total = (s.bytes + s.disk_free_bytes).max(1) as f64;
            let mut out = vec![
                div()
                    .class("bar")
                    .attr("aria-hidden", "true")
                    .child(span().class("bar-fill").style(format!("width:{:.1}%", s.bytes as f64 / total * 100.0)))
                    .into(),
                facts(vec![
                    (tx.storage_data, status::bytes(s.bytes)),
                    (tx.storage_free, status::bytes(s.disk_free_bytes)),
                    (tx.storage_pages, status::group(s.pages)),
                    (tx.cache_hit, status::percent(s.cache_hit_rate)),
                    (
                        tx.cache_size,
                        format!("{} / {}", status::bytes(s.cache_bytes), status::bytes(s.cache_capacity_bytes)),
                    ),
                ]),
                help(tx.help_storage),
            ];
            if let Some(m) = &d.memory {
                let b = |v: Option<u64>| v.map_or(DASH.to_string(), status::bytes);
                let free = match (m.available_bytes, m.total_bytes) {
                    (Some(a), Some(t)) => format!("{} / {}", status::bytes(a), status::bytes(t)),
                    (a, _) => b(a),
                };
                let swap = match (m.swap_free_bytes, m.swap_total_bytes) {
                    (_, Some(0)) | (_, None) => tx.detail_none.to_string(),
                    (Some(f), Some(t)) => format!("{} / {}", status::bytes(f), status::bytes(t)),
                    (None, Some(t)) => status::bytes(t),
                };
                out.push(heading(tx.detail_memory));
                out.push(facts(vec![
                    (tx.detail_mem_used, b(m.process_bytes)),
                    (tx.detail_mem_free, free),
                    (tx.detail_swap, swap),
                    (tx.detail_cache_app, b(m.cache_per_app_bytes)),
                ]));
            }
            out.push(heading(tx.detail_by_app));
            out.push(app_table(
                detail,
                vec![tx.apps_title, tx.detail_data, tx.detail_file, tx.db_rows],
                dbs.iter()
                    .map(|db| {
                        (
                            db.name.clone(),
                            vec![status::bytes(db.bytes), status::bytes(db.file_bytes), status::group(db.rows)],
                        )
                    })
                    .collect(),
            ));
            out
        }
        Detail::Wal => {
            let w = d.wal.clone().unwrap_or_default();
            vec![
                facts(vec![
                    (tx.last_checkpoint, when_and_ago(tx, now, w.last_checkpoint_at.as_deref())),
                    (tx.wal_size, status::bytes(w.bytes)),
                ]),
                help(tx.help_wal),
            ]
        }
        Detail::Backup => {
            let b = d.backup.clone().unwrap_or_default();
            let state = if !b.enabled {
                tx.backup_off
            } else {
                match b.last_ok {
                    Some(true) => tx.backup_ok,
                    Some(false) => tx.backup_failed,
                    None => tx.never,
                }
            };
            let mut rows = vec![
                (tx.detail_state, state.to_string()),
                (tx.backup_last, when_and_ago(tx, now, b.last_at.as_deref())),
                (tx.backup_target, b.target.clone().unwrap_or_else(|| DASH.into())),
            ];
            if let Some(e) = &b.last_error {
                rows.push((tx.detail_error, e.clone()));
            }
            vec![facts(rows), help(tx.help_backup)]
        }
    }
}

/// `2 min ago · PostgreSQL (psql) · read-only`, or never.
fn connection_text(tx: &Texts, now: f64, db: &Database) -> String {
    match &db.last_connection {
        Some(c) => {
            let mut parts = vec![ago_of(tx, now, c.at.as_deref())];
            parts.push(if c.client.is_empty() { c.via.clone() } else { format!("{} ({})", c.via, c.client) });
            if c.read_only {
                parts.push(tx.detail_read_only.to_string());
            }
            parts.join(" · ")
        }
        None => tx.never.to_string(),
    }
}

/// Everything about one application.
fn app_body(tx: &'static Texts, db: &Database, history: &[f64], now: f64, log: &[LogLine]) -> Vec<Node> {
    let r = db.requests.clone().unwrap_or_default();
    let mut pills = div().class("sheet-pills");
    if db.first {
        pills = pills.child(pill("", tx.app_first));
    }
    pills = match db.encrypted {
        Some(true) => pills.child(pill("is-ok", tx.app_encrypted)),
        Some(false) => pills.child(pill("is-bad", tx.app_clear)),
        None => pills,
    };
    if db.status.as_deref() == Some("degraded") {
        pills = pills.child(pill("is-bad", tx.app_degraded));
    }
    let mut out: Vec<Node> = vec![pills.into()];
    if let Some(desc) = db.description.as_ref().filter(|d| !d.is_empty()) {
        out.push(p().class("sheet-lead").text(desc.clone()).into());
    }
    out.push(chart(tx, history, tx.app_chart));
    out.push(facts(vec![
        (tx.detail_now, format!("{}/s", status::rate(r.per_s))),
        (tx.kpi_requests_total, status::group(r.total)),
        (tx.kpi_errors, format!("{:.2}", r.errors_per_s)),
        ("p50", status::millis(r.p50_ms)),
        ("p99", status::millis(r.p99_ms)),
        (tx.db_tables, db.tables.to_string()),
        (tx.db_rows, status::group(db.rows)),
        (tx.detail_data, status::bytes(db.bytes)),
        (tx.detail_file, status::bytes(db.file_bytes)),
        (
            tx.app_tokens,
            if db.read_only_tokens > 0 {
                format!("{} · {} {}", db.tokens, db.read_only_tokens, tx.detail_read_only)
            } else {
                db.tokens.to_string()
            },
        ),
        (tx.detail_websites, if db.websites.is_empty() { tx.detail_none.to_string() } else { db.websites.join(", ") }),
        (tx.app_last_request, when_and_ago(tx, now, db.last_request_at.as_deref())),
        (tx.detail_last_connection, connection_text(tx, now, db)),
        (tx.detail_created, db.created_at.as_deref().map_or(DASH.to_string(), status::when)),
    ]));
    if !db.branches.is_empty() {
        out.push(heading(&format!("{} ({})", tx.detail_branches, db.branches.len())));
        out.push(
            ul().class("branches")
                .children(
                    db.branches
                        .iter()
                        .map(|b| {
                            let mut line = format!(" · {} {}", status::bytes(b.own_bytes), tx.db_branch_own);
                            if let Some(parent) = &b.parent {
                                line.push_str(&format!(" · {} {parent}", tx.detail_from));
                            }
                            if let Some(at) = &b.created_at {
                                line.push_str(&format!(" · {}", status::when(at)));
                            }
                            li().class("branch")
                                .child(icon(Icon::Branch))
                                .child(span().class("mono").text(b.name.clone()))
                                .child(span().class("muted").text(line))
                                .into()
                        })
                        .collect::<Vec<Node>>(),
                )
                .into(),
        );
    }
    let recent: Vec<&LogLine> = log.iter().filter(|l| l.app.as_deref() == Some(db.name.as_str())).take(8).collect();
    out.push(heading(tx.detail_recent));
    if recent.is_empty() {
        out.push(p().class("muted").text(tx.log_empty).into());
    } else {
        out.push(
            ol().class("log")
                .children(
                    recent
                        .into_iter()
                        .map(|line| {
                            li().class("log-line")
                                .child(el("time").attr("datetime", line.at.clone()).text(status::clock(&line.at)))
                                .child(span().class("log-msg").text(line.message.clone()))
                                .into()
                        })
                        .collect::<Vec<Node>>(),
                )
                .into(),
        );
    }
    out.push(
        a().class("btn btn-primary btn-sm sheet-cta")
            .href(format!("/admin/apps/{}", db.name))
            .text(tx.detail_open_console)
            .child(icon(Icon::ArrowRight))
            .into(),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chart_spans_the_box() {
        let (line, area) = chart_points(&[0.0, 2.0, 1.0], 300.0, 90.0);
        assert!(line.starts_with("0.0,88.0") && line.contains("150.0,6.0") && line.ends_with("300.0,47.0"), "{line}");
        assert!(area.starts_with("0,90.0 ") && area.ends_with(" 300.0,90.0"), "{area}");
        assert_eq!(chart_points(&[], 300.0, 90.0).0, "0,89.0 300.0,89.0");
        assert_eq!(status::when("2026-11-14T10:30:05Z"), "2026-11-14 10:30 UTC");
    }
}
