//! The management console, under `/admin`: server-rendered pages and plain
//! forms, no JavaScript, embedded in the binary.
//!
//! Security: one password (`NUAGE_ADMIN_PASSWORD`), compared in constant
//! time; a random session cookie (`HttpOnly`, `Secure`, `SameSite=Strict`,
//! 12 hours); a CSRF token in every form (and a double-submit cookie on the
//! login form); at most 5 failed logins per address and 30 in all per 15
//! minutes.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hyper::body::Incoming;
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use memoire_orm::ast::SelectAst;
use memoire_orm::{Error, Order, Query, TableSchema, Value};

use super::query::{self, Statement};
use super::{HttpBody, State, full, random_token, read_body, same_secret};
use crate::embedded::{self, Embedded};
use crate::engine::MAIN;

const SESSION_COOKIE: &str = "nuage_session";
const LOGIN_COOKIE: &str = "nuage_login";
const SESSION_TTL: Duration = Duration::from_secs(12 * 3600);
const FAILURE_WINDOW: Duration = Duration::from_secs(15 * 60);
const MAX_FAILURES_PER_ADDRESS: usize = 5;
const MAX_FAILURES: usize = 30;
const PAGE_ROWS: u64 = 50;

struct Session {
    expires: Instant,
    csrf: String,
}

/// Console sessions and failed logins.
#[derive(Default)]
pub(crate) struct Sessions {
    sessions: Mutex<HashMap<String, Session>>,
    failures: Mutex<Vec<(IpAddr, Instant)>>,
}

fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(';')).find_map(|c| {
        let (k, v) = c.trim().split_once('=')?;
        (k == name).then_some(v)
    })
}

impl Sessions {
    /// The session of a request: its token and CSRF token.
    fn current(&self, headers: &HeaderMap) -> Option<(String, String)> {
        let token = cookie(headers, SESSION_COOKIE)?;
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions.retain(|_, s| s.expires > Instant::now());
        let s = sessions.iter().find(|(k, _)| same_secret(k.as_bytes(), token.as_bytes()))?;
        Some((s.0.clone(), s.1.csrf.clone()))
    }

    pub(crate) fn is_logged_in(&self, headers: &HeaderMap) -> bool {
        self.current(headers).is_some()
    }

    fn create(&self) -> String {
        let token = random_token();
        let session = Session { expires: Instant::now() + SESSION_TTL, csrf: random_token() };
        self.sessions.lock().unwrap_or_else(|e| e.into_inner()).insert(token.clone(), session);
        token
    }

    fn remove(&self, token: &str) {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner()).remove(token);
    }

    /// `false` when this address (or everyone) failed too often lately.
    fn may_try(&self, ip: IpAddr) -> bool {
        let mut f = self.failures.lock().unwrap_or_else(|e| e.into_inner());
        f.retain(|(_, at)| at.elapsed() < FAILURE_WINDOW);
        f.iter().filter(|(a, _)| *a == ip).count() < MAX_FAILURES_PER_ADDRESS && f.len() < MAX_FAILURES
    }

    fn failed(&self, ip: IpAddr) {
        self.failures.lock().unwrap_or_else(|e| e.into_inner()).push((ip, Instant::now()));
    }
}

/// The client's address: behind a router (Heroku), the last address it
/// added to `X-Forwarded-For`.
fn client_ip(headers: &HeaderMap, peer: SocketAddr) -> IpAddr {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit(',').next())
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(peer.ip())
}

// ---------------------------------------------------------------------------
// HTML
// ---------------------------------------------------------------------------

pub(crate) fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

const STYLE: &str = r#"
:root{--bg:#f7f8fb;--fg:#17202a;--muted:#5d6b7a;--card:#fff;--line:#dfe4ea;--accent:#2563eb;--bad:#b42318;--ok:#067647}
@media (prefers-color-scheme:dark){:root{--bg:#0f141a;--fg:#e6edf3;--muted:#9aa7b4;--card:#161d25;--line:#2a3441;--accent:#6ea8fe;--bad:#ff8a80;--ok:#5fd38d}}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--fg);font:15px/1.5 system-ui,-apple-system,Segoe UI,Roboto,sans-serif}
header{display:flex;flex-wrap:wrap;gap:.5rem 1rem;align-items:center;padding:.75rem 1rem;border-bottom:1px solid var(--line);background:var(--card)}
header strong{font-size:1.05rem}nav{display:flex;flex-wrap:wrap;gap:.25rem .75rem}nav a{color:var(--fg);text-decoration:none;padding:.2rem 0}nav a:hover{color:var(--accent)}
main{max-width:1100px;margin:0 auto;padding:1rem}h1{font-size:1.35rem;margin:.25rem 0 1rem}h2{font-size:1.1rem;margin:1.5rem 0 .5rem}
.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(200px,1fr));gap:.75rem}.card{background:var(--card);border:1px solid var(--line);border-radius:10px;padding:.75rem 1rem}
.card small{display:block;color:var(--muted)}.card b{font-size:1.15rem}.scroll{overflow-x:auto;background:var(--card);border:1px solid var(--line);border-radius:10px}
table{border-collapse:collapse;width:100%}th,td{text-align:left;padding:.45rem .7rem;border-bottom:1px solid var(--line);white-space:nowrap;max-width:28rem;overflow:hidden;text-overflow:ellipsis}th{color:var(--muted);font-weight:600}
form{margin:0}input,textarea,select{font:inherit;color:var(--fg);background:var(--card);border:1px solid var(--line);border-radius:8px;padding:.5rem .6rem;width:100%}
textarea{min-height:7rem;font-family:ui-monospace,SFMono-Regular,Menlo,monospace}button{font:inherit;border:0;border-radius:8px;padding:.5rem .9rem;background:var(--accent);color:#fff;cursor:pointer}
button.danger{background:var(--bad)}button.plain{background:transparent;color:var(--accent);padding:0}.row{display:flex;flex-wrap:wrap;gap:.5rem;align-items:end}.row>*{flex:1 1 12rem}.row>button{flex:0 0 auto}
.msg{padding:.6rem .8rem;border-radius:8px;border:1px solid var(--line);background:var(--card);margin:.75rem 0}.bad{color:var(--bad)}.ok{color:var(--ok)}.muted{color:var(--muted)}
.login{max-width:22rem;margin:12vh auto}code{font-family:ui-monospace,SFMono-Regular,Menlo,monospace}
"#;

fn page(title: &str, csrf: Option<&str>, body: &str) -> String {
    let nav = match csrf {
        Some(csrf) => format!(
            r#"<nav><a href="/admin">Overview</a><a href="/admin/tables">Tables</a><a href="/admin/query">Query</a><a href="/admin/branches">Branches</a><a href="/admin/storage">Storage</a></nav>
<form method="post" action="/admin/logout" style="margin-left:auto"><input type="hidden" name="csrf" value="{}"><button class="plain">Log out</button></form>"#,
            esc(csrf)
        ),
        None => String::new(),
    };
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="robots" content="noindex"><meta name="color-scheme" content="light dark"><title>{} · Nuage DB</title><style>{STYLE}</style></head>
<body><header><strong>Nuage DB</strong>{nav}</header><main>{body}</main></body></html>"#,
        esc(title)
    )
}

fn html(status: u16, body: String) -> Response<HttpBody> {
    let mut res = Response::new(full(body));
    *res.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    h.insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'",
        ),
    );
    h.insert("referrer-policy", HeaderValue::from_static("same-origin"));
    res
}

fn redirect(to: &str) -> Response<HttpBody> {
    let mut res = Response::new(full(""));
    *res.status_mut() = StatusCode::SEE_OTHER;
    if let Ok(v) = HeaderValue::from_str(to) {
        res.headers_mut().insert(header::LOCATION, v);
    }
    res
}

fn set_cookie(res: &mut Response<HttpBody>, name: &str, value: &str, max_age: u64) {
    let c = format!("{name}={value}; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age={max_age}");
    if let Ok(v) = HeaderValue::from_str(&c) {
        res.headers_mut().append(header::SET_COOKIE, v);
    }
}

fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{n} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

fn duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{s} s"),
        60..=3599 => format!("{} min", s / 60),
        3600..=86_399 => format!("{} h {} min", s / 3600, s / 60 % 60),
        _ => format!("{} d {} h", s / 86_400, s / 3600 % 24),
    }
}

fn card(label: &str, value: &str) -> String {
    format!(r#"<div class="card"><small>{}</small><b>{}</b></div>"#, esc(label), esc(value))
}

fn cell(v: &Value) -> String {
    match v {
        Value::Null => r#"<span class="muted">NULL</span>"#.into(),
        Value::Text(s) => esc(s),
        Value::Bytes(b) => esc(&format!("{} bytes", b.len())),
        other => esc(&other.to_string().trim_matches('\'').replace("''", "'")),
    }
}

fn table_html(headers: &[String], rows: &[Vec<Value>]) -> String {
    let mut out = String::from(r#"<div class="scroll"><table><thead><tr>"#);
    for h in headers {
        let _ = write!(out, "<th>{}</th>", esc(h));
    }
    out.push_str("</tr></thead><tbody>");
    for r in rows {
        out.push_str("<tr>");
        for v in r {
            let _ = write!(out, "<td>{}</td>", cell(v));
        }
        out.push_str("</tr>");
    }
    if rows.is_empty() {
        let _ = write!(out, r#"<tr><td colspan="{}" class="muted">No rows.</td></tr>"#, headers.len().max(1));
    }
    out.push_str("</tbody></table></div>");
    out
}

// ---------------------------------------------------------------------------
// Forms
// ---------------------------------------------------------------------------

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                match u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("zz"), 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_form(body: &[u8]) -> HashMap<String, String> {
    String::from_utf8_lossy(body)
        .split('&')
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (!k.is_empty()).then(|| (url_decode(k), url_decode(v)))
        })
        .collect()
}

fn query_param(req: &Request<Incoming>, name: &str) -> Option<String> {
    parse_form(req.uri().query().unwrap_or_default().as_bytes()).remove(name)
}

// ---------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------

/// `/` when no site is served: a pointer to the console.
pub(crate) fn home_page(state: &State) -> Response<HttpBody> {
    let body = format!(
        r#"<h1>Nuage DB is running</h1><p>Database <b>{}</b>, version {}.</p><p><a href="/admin">Open the console</a> · <a href="/api/health">Health</a></p>"#,
        esc(&state.engine.options().name),
        crate::VERSION
    );
    html(200, page("Home", None, &body))
}

/// Where to go after logging in: only a path of this site (`/fr/app/`), never
/// an absolute or protocol-relative URL (`https://...`, `//host`, `/\host`),
/// which would make the console an open redirect.
pub(crate) fn safe_next(next: &str) -> Option<&str> {
    let ok = next.starts_with('/')
        && !next.starts_with("//")
        && !next.contains('\\')
        && next.len() <= 2048
        && !next.chars().any(|c| c.is_control() || c.is_whitespace());
    ok.then_some(next)
}

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn login_page(message: Option<&str>, status: u16, next: Option<&str>) -> Response<HttpBody> {
    let token = random_token();
    let msg = message.map_or(String::new(), |m| format!(r#"<p class="msg bad">{}</p>"#, esc(m)));
    let next = next
        .and_then(safe_next)
        .map_or(String::new(), |n| format!(r#"<input type="hidden" name="next" value="{}">"#, esc(n)));
    let body = format!(
        r#"<div class="login card"><h1>Console</h1>{msg}<form method="post" action="/admin/login">
<input type="hidden" name="csrf" value="{token}">{next}<p><label>Password<br><input type="password" name="password" autocomplete="current-password" required autofocus></label></p>
<button>Log in</button></form></div>"#
    );
    let mut res = html(status, page("Log in", None, &body));
    set_cookie(&mut res, LOGIN_COOKIE, &token, 3600);
    res
}

pub(crate) async fn route(state: &Arc<State>, req: Request<Incoming>, peer: SocketAddr) -> Response<HttpBody> {
    let Some(password) = state.options.admin_password.clone() else {
        return html(
            503,
            page(
                "Console off",
                None,
                "<h1>The console is off</h1><p>Set <code>NUAGE_ADMIN_PASSWORD</code> on the server to turn it on.</p>",
            ),
        );
    };
    let path = req.uri().path().trim_end_matches('/').to_owned();
    let method = req.method().clone();

    let next_param = query_param(&req, "next");
    if path == "/admin/login" {
        if method != Method::POST {
            return login_page(None, 200, next_param.as_deref());
        }
        let ip = client_ip(req.headers(), peer);
        let expected = cookie(req.headers(), LOGIN_COOKIE).map(str::to_owned);
        let Ok(body) = read_body(state, req).await else { return login_page(Some("Invalid request."), 400, None) };
        let form = parse_form(&body);
        let next = form.get("next").map(String::as_str);
        if !state.admin.may_try(ip) {
            return login_page(Some("Too many failed attempts. Wait a few minutes."), 429, next);
        }
        let csrf_ok =
            matches!((&expected, form.get("csrf")), (Some(a), Some(b)) if same_secret(a.as_bytes(), b.as_bytes()));
        if !csrf_ok {
            return login_page(Some("The form expired. Try again."), 403, next);
        }
        let given = form.get("password").map(String::as_str).unwrap_or_default();
        if !same_secret(given.as_bytes(), password.as_bytes()) {
            state.admin.failed(ip);
            return login_page(Some("Wrong password."), 401, next);
        }
        let mut res = redirect(next.and_then(safe_next).unwrap_or("/admin"));
        set_cookie(&mut res, SESSION_COOKIE, &state.admin.create(), SESSION_TTL.as_secs());
        set_cookie(&mut res, LOGIN_COOKIE, "", 0);
        return res;
    }

    let Some((session, csrf)) = state.admin.current(req.headers()) else {
        if method != Method::GET {
            return login_page(Some("Log in first."), 401, None);
        }
        // `/admin?next=/fr/app/` (from the site's dashboard) keeps its target;
        // any other console page comes back to itself.
        let target = match next_param.as_deref().and_then(safe_next) {
            Some(n) if path == "/admin" => n.to_owned(),
            _ => req.uri().path_and_query().map_or("/admin".to_owned(), |p| p.as_str().to_owned()),
        };
        return redirect(&format!("/admin/login?next={}", url_encode(&target)));
    };
    if method == Method::GET
        && path == "/admin"
        && let Some(n) = next_param.as_deref().and_then(safe_next)
    {
        return redirect(n);
    }

    if method == Method::POST {
        let Ok(body) = read_body(state, req).await else {
            return html(400, page("Error", Some(&csrf), "<p>Invalid request.</p>"));
        };
        let form = parse_form(&body);
        if !form.get("csrf").is_some_and(|c| same_secret(c.as_bytes(), csrf.as_bytes())) {
            return html(
                403,
                page(
                    "Refused",
                    Some(&csrf),
                    r#"<h1>Refused</h1><p class="msg bad">Invalid form token (CSRF). Reload the page and try again.</p>"#,
                ),
            );
        }
        return post(state, &path, &form, &session, &csrf).await;
    }

    let state2 = state.clone();
    let csrf2 = csrf.clone();
    let page_no = query_param(&req, "page").and_then(|p| p.parse().ok()).unwrap_or(0u64);
    tokio::task::spawn_blocking(move || get(&state2, &path, &csrf2, page_no))
        .await
        .unwrap_or_else(|e| html(500, page("Error", Some(&csrf), &format!("<p>{}</p>", esc(&e.to_string())))))
}

fn error_page(csrf: &str, e: &Error) -> Response<HttpBody> {
    html(400, page("Error", Some(csrf), &format!(r#"<h1>Error</h1><p class="msg bad">{}</p>"#, esc(&e.to_string()))))
}

fn schema_of(state: &State, db: &Embedded, name: &str) -> memoire_orm::Result<&'static TableSchema> {
    let info = db.tables()?.into_iter().find(|t| t.name == name).ok_or_else(|| Error::NoSuchTable(name.to_owned()))?;
    state.schemas.schema(&info.schema)
}

fn get(state: &State, path: &str, csrf: &str, page_no: u64) -> Response<HttpBody> {
    let engine = &state.engine;
    let db = Embedded::from_engine(engine.clone());
    match path {
        "/admin" => {
            let stats = engine.stats();
            let tables = db.tables().unwrap_or_default();
            let backup = engine.backup_status();
            let open = state.metrics.open_connections.load(std::sync::atomic::Ordering::Relaxed);
            let active = state.metrics.active_requests.load(std::sync::atomic::Ordering::Relaxed);
            let (rate, _) = state.metrics.rates();
            let mut body = format!("<h1>{}</h1><div class=\"grid\">", esc(&engine.options().name));
            for (l, v) in [
                ("Size on disk", bytes(stats.file_bytes)),
                ("Tables", tables.len().to_string()),
                ("Rows", tables.iter().map(|t| t.rows).sum::<i64>().to_string()),
                ("Branches", engine.branches().len().to_string()),
                ("Version", format!("Nuage DB {}", crate::VERSION)),
                ("Uptime", duration(engine.uptime())),
                ("Connections", format!("{} active, {} idle", active, open.saturating_sub(active))),
                ("Transactions open", state.open_transactions().to_string()),
                ("Requests", format!("{rate:.1}/s, {} in all", state.metrics.total())),
                (
                    "Cache",
                    format!(
                        "{} of {} ({:.0} % hits)",
                        bytes(stats.cache_bytes),
                        bytes(stats.cache_capacity_bytes),
                        stats.cache_hit_rate * 100.0
                    ),
                ),
                (
                    "Last checkpoint",
                    stats
                        .last_checkpoint
                        .and_then(|t| t.elapsed().ok())
                        .map_or("none yet".into(), |d| format!("{} ago", duration(d))),
                ),
                (
                    "Backup",
                    match (&backup.target, backup.last_ok) {
                        (None, _) => "off".into(),
                        (Some(t), true) => format!("to {t}"),
                        (Some(_), false) => "failing".into(),
                    },
                ),
            ] {
                body.push_str(&card(l, &v));
            }
            let _ = write!(
                body,
                "</div><p class=\"muted\">Data is stored in <code>{}</code>.</p>",
                esc(&engine.dir().display().to_string())
            );
            html(200, page("Overview", Some(csrf), &body))
        }
        "/admin/tables" => match db.tables() {
            Ok(tables) => {
                let rows: Vec<Vec<Value>> = tables
                    .iter()
                    .map(|t| {
                        vec![
                            Value::Text(t.name.clone()),
                            Value::I64(t.rows),
                            Value::Text(bytes(t.bytes as u64)),
                            Value::Text(t.columns.join(", ")),
                        ]
                    })
                    .collect();
                let mut body = String::from("<h1>Tables</h1>");
                body.push_str(&table_html(&["Table".into(), "Rows".into(), "Size".into(), "Columns".into()], &rows));
                body.push_str("<p>");
                for t in &tables {
                    let _ = write!(body, r#"<a href="/admin/tables/{0}">Browse {0}</a> · "#, esc(&t.name));
                }
                body.push_str("</p>");
                html(200, page("Tables", Some(csrf), &body))
            }
            Err(e) => error_page(csrf, &e),
        },
        p if p.starts_with("/admin/tables/") => {
            let name = &p["/admin/tables/".len()..];
            let schema = match schema_of(state, &db, name) {
                Ok(s) => s,
                Err(e) => return error_page(csrf, &e),
            };
            let pk = &schema.columns[schema.primary_key];
            let mut select = SelectAst::new(schema);
            select.order = vec![Order {
                expr: memoire_orm::Expr::Column(memoire_orm::ColumnRef {
                    table: schema.name,
                    name: pk.name,
                    index: schema.primary_key,
                    kind: pk.kind,
                }),
                desc: false,
                nulls: None,
            }];
            select.limit = Some(PAGE_ROWS);
            select.offset = Some(page_no * PAGE_ROWS);
            match db.fetch_rows(Query::Select(&select)) {
                Ok(rows) => {
                    let headers: Vec<String> = schema.columns.iter().map(|c| c.name.to_owned()).collect();
                    let mut body = format!("<h1>{}</h1>", esc(name));
                    body.push_str(&table_html(&headers, &rows));
                    body.push_str("<p>");
                    if page_no > 0 {
                        let _ = write!(body, r#"<a href="?page={}">← Previous</a> "#, page_no - 1);
                    }
                    let _ = write!(body, "Page {} ", page_no + 1);
                    if rows.len() as u64 == PAGE_ROWS {
                        let _ = write!(body, r#"<a href="?page={}">Next →</a>"#, page_no + 1);
                    }
                    body.push_str("</p>");
                    html(200, page(name, Some(csrf), &body))
                }
                Err(e) => error_page(csrf, &e),
            }
        }
        "/admin/query" => html(200, page("Query", Some(csrf), &query_form(csrf, "", None))),
        "/admin/branches" => html(200, page("Branches", Some(csrf), &branches_body(state, csrf, None))),
        "/admin/storage" => html(200, page("Storage", Some(csrf), &storage_body(state, csrf, None))),
        _ => html(404, page("Not found", Some(csrf), "<h1>Not found</h1>")),
    }
}

fn query_form(csrf: &str, sql: &str, extra: Option<&str>) -> String {
    format!(
        r#"<h1>Query</h1><form method="post" action="/admin/query"><input type="hidden" name="csrf" value="{}">
<textarea name="sql" spellcheck="false" placeholder="SELECT * FROM users WHERE age >= 18 ORDER BY name LIMIT 20">{}</textarea>
<p class="row"><button>Run</button></p></form>
<p class="muted">SELECT, UPDATE and DELETE on one table, with WHERE, ORDER BY, LIMIT and OFFSET. Statements that change data ask for a confirmation first.</p>{}"#,
        esc(csrf),
        esc(sql),
        extra.unwrap_or_default()
    )
}

fn branches_body(state: &State, csrf: &str, message: Option<String>) -> String {
    let engine = &state.engine;
    let mut body = String::from("<h1>Branches</h1>");
    if let Some(m) = message {
        body.push_str(&m);
    }
    body.push_str(r#"<p class="muted">A branch is an instant copy of the database: it shares the data and stores only its own changes.</p>"#);
    let branches = engine.branches();
    let mut out = String::from(
        r#"<div class="scroll"><table><thead><tr><th>Branch</th><th>From</th><th>Own data</th><th></th></tr></thead><tbody>"#,
    );
    for b in &branches {
        let _ = write!(
            out,
            r#"<tr><td>{n}</td><td>{p}</td><td>{s}</td><td><form method="post" action="/admin/branches/{n}/reset" style="display:inline"><input type="hidden" name="csrf" value="{c}"><button class="plain">Reset</button></form> · <form method="post" action="/admin/branches/{n}/delete" style="display:inline"><input type="hidden" name="csrf" value="{c}"><button class="plain">Delete</button></form></td></tr>"#,
            n = esc(&b.name),
            p = esc(b.parent.as_deref().unwrap_or(&engine.options().name)),
            s = bytes(b.own_bytes),
            c = esc(csrf)
        );
    }
    if branches.is_empty() {
        out.push_str(r#"<tr><td colspan="4" class="muted">No branch yet.</td></tr>"#);
    }
    out.push_str("</tbody></table></div>");
    body.push_str(&out);
    let mut options = format!(r#"<option value="">{}</option>"#, esc(&engine.options().name));
    for b in &branches {
        let _ = write!(options, r#"<option>{}</option>"#, esc(&b.name));
    }
    let _ = write!(
        body,
        r#"<h2>New branch</h2><form method="post" action="/admin/branches" class="row"><input type="hidden" name="csrf" value="{}">
<label>Name<input name="name" pattern="[a-z0-9_\-]{{1,63}}" required placeholder="preview-42"></label><label>From<select name="from">{options}</select></label><button>Create</button></form>"#,
        esc(csrf)
    );
    body
}

fn storage_body(state: &State, csrf: &str, message: Option<String>) -> String {
    let engine = &state.engine;
    let s = engine.stats();
    let b = engine.backup_status();
    let mut body = String::from("<h1>Storage</h1>");
    if let Some(m) = message {
        body.push_str(&m);
    }
    body.push_str(r#"<div class="grid">"#);
    for (l, v) in [
        ("File", bytes(s.file_bytes)),
        ("Data", bytes(s.stored_bytes)),
        ("Free space in the file", bytes(s.fragmented_bytes)),
        ("Pages", format!("{} of {}", s.pages, bytes(s.page_size))),
        ("Write-ahead log", bytes(s.wal_bytes)),
        ("Free disk", s.disk_free_bytes.map_or("unknown".into(), bytes)),
        ("Cache", format!("{} / {}", bytes(s.cache_bytes), bytes(s.cache_capacity_bytes))),
        ("Cache hits", format!("{:.1} %", s.cache_hit_rate * 100.0)),
        ("Last commit", s.lsn.to_string()),
        (
            "Last checkpoint",
            format!(
                "{} ({})",
                s.checkpoint_lsn,
                s.last_checkpoint
                    .and_then(|t| t.elapsed().ok())
                    .map_or("not yet".into(), |d| format!("{} ago", duration(d)))
            ),
        ),
        ("Backup target", b.target.clone().unwrap_or_else(|| "off".into())),
        (
            "Last backup",
            match (b.last_at, b.last_ok) {
                (_, false) => format!("failed: {}", b.last_error.clone().unwrap_or_default()),
                (Some(at), true) => format!(
                    "{} ago",
                    duration(Duration::from_millis((crate::engine::store::now_millis() - at).max(0) as u64))
                ),
                (None, true) => "none yet".into(),
            },
        ),
    ] {
        body.push_str(&card(l, &v));
    }
    let _ = write!(
        body,
        r#"</div><p class="muted">Data is stored in <code>{}</code>.</p><h2>Maintenance</h2><div class="row">"#,
        esc(&engine.dir().display().to_string())
    );
    for (action, label) in [
        ("checkpoint", "Checkpoint now"),
        ("backup", "Back up now"),
        ("integrity", "Check integrity"),
        ("compact", "Compact"),
    ] {
        let _ = write!(
            body,
            r#"<form method="post" action="/admin/storage/{action}"><input type="hidden" name="csrf" value="{}"><button>{label}</button></form>"#,
            esc(csrf)
        );
    }
    body.push_str("</div>");
    body
}

fn note(ok: bool, text: &str) -> String {
    format!(r#"<p class="msg {}">{}</p>"#, if ok { "ok" } else { "bad" }, esc(text))
}

async fn post(
    state: &Arc<State>,
    path: &str,
    form: &HashMap<String, String>,
    session: &str,
    csrf: &str,
) -> Response<HttpBody> {
    let field = |k: &str| form.get(k).map(String::as_str).unwrap_or_default().to_owned();
    if path == "/admin/logout" {
        state.admin.remove(session);
        let mut res = redirect("/admin/login");
        set_cookie(&mut res, SESSION_COOKIE, "", 0);
        return res;
    }
    let state2 = state.clone();
    let path2 = path.to_owned();
    let csrf2 = csrf.to_owned();
    let (sql, confirmed, name, from) = (field("sql"), field("confirm") == "yes", field("name"), field("from"));
    tokio::task::spawn_blocking(move || {
        let state = &state2;
        let engine = &state.engine;
        let csrf = csrf2.as_str();
        match path2.as_str() {
            "/admin/query" => {
                let db = Embedded::from_engine(engine.clone());
                let lookup = |n: &str| schema_of(state, &db, n);
                let stmt = match query::parse(&sql, &lookup) {
                    Ok(s) => s,
                    Err(e) => {
                        return html(
                            400,
                            page("Query", Some(csrf), &query_form(csrf, &sql, Some(&note(false, &e.to_string())))),
                        );
                    }
                };
                let result = run_statement(state, &db, &stmt, confirmed, csrf, &sql);
                match result {
                    Ok(extra) => html(200, page("Query", Some(csrf), &query_form(csrf, &sql, Some(&extra)))),
                    Err(e) => html(
                        400,
                        page("Query", Some(csrf), &query_form(csrf, &sql, Some(&note(false, &e.to_string())))),
                    ),
                }
            }
            "/admin/branches" => {
                let from = (!from.is_empty()).then_some(from.as_str());
                let msg = match engine.create_branch(&name, from) {
                    Ok(b) => note(true, &format!("Branch {} created.", b.name)),
                    Err(e) => note(false, &e.to_string()),
                };
                html(200, page("Branches", Some(csrf), &branches_body(state, csrf, Some(msg))))
            }
            p if p.starts_with("/admin/branches/") => {
                let rest = &p["/admin/branches/".len()..];
                let msg = match rest.rsplit_once('/') {
                    Some((n, "delete")) => match engine.delete_branch(n) {
                        Ok(true) => note(true, &format!("Branch {n} deleted.")),
                        Ok(false) => note(false, &format!("There is no branch {n}.")),
                        Err(e) => note(false, &e.to_string()),
                    },
                    Some((n, "reset")) => match engine.reset_branch(n) {
                        Ok(true) => note(true, &format!("Branch {n} reset to its parent.")),
                        Ok(false) => note(false, &format!("There is no branch {n}.")),
                        Err(e) => note(false, &e.to_string()),
                    },
                    _ => note(false, "Unknown action."),
                };
                html(200, page("Branches", Some(csrf), &branches_body(state, csrf, Some(msg))))
            }
            p if p.starts_with("/admin/storage/") => {
                let msg = match &p["/admin/storage/".len()..] {
                    "checkpoint" => engine.checkpoint().map(|l| note(true, &format!("Checkpoint at {l}."))),
                    "backup" => engine.backup_now(true).map(|()| note(true, "Backup done.")),
                    "integrity" => engine.check_integrity().map(|ok| {
                        note(
                            ok,
                            if ok { "Every page checksum is right." } else { "Damaged pages were found and repaired." },
                        )
                    }),
                    "compact" => engine.compact().map(|done| {
                        note(done, if done { "Compaction done." } else { "Busy: compaction will run later." })
                    }),
                    _ => Ok(note(false, "Unknown action.")),
                }
                .unwrap_or_else(|e| note(false, &e.to_string()));
                html(200, page("Storage", Some(csrf), &storage_body(state, csrf, Some(msg))))
            }
            _ => html(404, page("Not found", Some(csrf), "<h1>Not found</h1>")),
        }
    })
    .await
    .unwrap_or_else(|e| html(500, page("Error", Some(csrf), &format!("<p>{}</p>", esc(&e.to_string())))))
}

/// Runs a console statement: reads at once, writes after a confirmation.
fn run_statement(
    state: &State,
    db: &Embedded,
    stmt: &Statement,
    confirmed: bool,
    csrf: &str,
    sql: &str,
) -> memoire_orm::Result<String> {
    let _ = state;
    if stmt.writes() && !confirmed {
        let preview = stmt.preview().ok_or_else(|| Error::Unsupported("no preview".into()))?;
        let n = db
            .fetch_rows(Query::Select(&preview))?
            .first()
            .and_then(|r| r.first())
            .and_then(Value::as_i64)
            .unwrap_or(0);
        return Ok(format!(
            r#"<div class="msg"><p><b>This statement changes data: {n} row(s) would be affected.</b></p>
<form method="post" action="/admin/query"><input type="hidden" name="csrf" value="{}"><input type="hidden" name="sql" value="{}"><input type="hidden" name="confirm" value="yes"><button class="danger">Confirm and run</button></form></div>"#,
            esc(csrf),
            esc(sql)
        ));
    }
    let engine = db.engine();
    match stmt {
        Statement::Select(s) => {
            let rows = embedded::fetch_rows(engine, MAIN, &Query::Select(s))?;
            let headers: Vec<String> = match &s.projection {
                memoire_orm::ast::Projection::Tables(t) => {
                    t.iter().flat_map(|t| t.columns.iter().map(|c| c.name.to_owned())).collect()
                }
                memoire_orm::ast::Projection::Exprs(e) => e
                    .iter()
                    .map(|e| match e {
                        memoire_orm::Expr::Column(c) => c.name.to_owned(),
                        _ => "count".to_owned(),
                    })
                    .collect(),
            };
            Ok(format!("<p class=\"muted\">{} row(s).</p>{}", rows.len(), table_html(&headers, &rows)))
        }
        Statement::Update(u) => {
            let r = embedded::execute_now(engine, MAIN, &Query::Update(u))?;
            Ok(note(true, &format!("{} row(s) updated.", r.rows_affected)))
        }
        Statement::Delete(d) => {
            let r = embedded::execute_now(engine, MAIN, &Query::Delete(d))?;
            Ok(note(true, &format!("{} row(s) deleted.", r.rows_affected)))
        }
    }
}
