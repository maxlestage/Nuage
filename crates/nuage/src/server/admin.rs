//! The management console, under `/admin`: server-rendered pages and plain
//! forms, no JavaScript, embedded in the binary. `/admin` lists the
//! applications (and makes new ones); `/admin/apps/<name>/...` are the pages
//! of one application (tables, query, branches, storage, tokens, settings);
//! `/admin/server` is the server and the events of every application.
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

use super::apps::App;
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
.login{max-width:22rem;margin:12vh auto}code,pre{font-family:ui-monospace,SFMono-Regular,Menlo,monospace}
pre{background:var(--card);border:1px solid var(--line);border-radius:8px;padding:.6rem .8rem;overflow-x:auto;margin:.5rem 0}
.pill{display:inline-block;font-size:.78rem;line-height:1.4;padding:.05rem .5rem;border-radius:999px;border:1px solid var(--line);color:var(--muted);white-space:nowrap}.pill.ok{color:var(--ok);border-color:currentColor}.pill.bad{color:var(--bad);border-color:currentColor}
.row-title{display:flex;flex-wrap:wrap;align-items:center;gap:.4rem .6rem}.row-title h1{margin:0}.apphead{margin-bottom:1rem}
.tabs{display:flex;flex-wrap:wrap;gap:.25rem;border-bottom:1px solid var(--line);margin-top:.75rem}.tabs a{padding:.4rem .7rem;color:var(--muted);text-decoration:none;border-bottom:2px solid transparent;margin-bottom:-1px}.tabs a:hover{color:var(--fg)}.tabs a[aria-current]{color:var(--accent);border-color:var(--accent)}
.switch{margin:.5rem 0 0}.chip{display:inline-block;padding:.1rem .55rem;border-radius:999px;border:1px solid var(--line);background:var(--card);color:var(--fg);text-decoration:none;margin:.1rem 0}.chip:hover{border-color:var(--accent);color:var(--accent)}
.apps{display:grid;grid-template-columns:repeat(auto-fill,minmax(min(100%,19rem),1fr));gap:.75rem}a.card.app{display:block;color:var(--fg);text-decoration:none}a.card.app:hover{border-color:var(--accent)}.card.app.is-bad{border-color:var(--bad)}
.mini{display:grid;grid-template-columns:repeat(3,1fr);gap:.35rem .75rem;margin:.6rem 0 0}.mini dt{color:var(--muted);font-size:.78rem}.mini dd{margin:0;font-variant-numeric:tabular-nums}
.reveal pre{user-select:all}
"#;

fn page(title: &str, csrf: Option<&str>, body: &str) -> String {
    let nav = match csrf {
        Some(csrf) => format!(
            r#"<nav><a href="/admin">Applications</a><a href="/admin/server">Server</a></nav>
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
        r#"<h1>Nuage DB is running</h1><p>{} application(s), version {}. Everything stored is encrypted when <code>NUAGE_ENCRYPTION_KEY</code> is set.</p><p><a href="/admin">Open the console</a> · <a href="/api/health">Health</a></p>"#,
        state.apps.all().len(),
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
    let mut path = req.uri().path().trim_end_matches('/').to_owned();
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

    // The pages of one database before there were applications: now those
    // of the first application.
    if let Some(rest) = ["/admin/tables", "/admin/query", "/admin/branches", "/admin/storage"]
        .iter()
        .find(|p| path == **p || path.starts_with(&format!("{p}/")))
        .map(|_| path["/admin".len()..].to_owned())
    {
        let moved = format!("{}{rest}", app_base(&state.apps.first().name));
        if method == Method::GET {
            let query = req.uri().query().map_or(String::new(), |q| format!("?{q}"));
            return redirect(&format!("{moved}{query}"));
        }
        path = moved;
    }

    let origin = origin_of(req.headers());
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
        return post(state, &path, form, &session, &csrf, origin).await;
    }

    let state2 = state.clone();
    let csrf2 = csrf.clone();
    let page_no = query_param(&req, "page").and_then(|p| p.parse().ok()).unwrap_or(0u64);
    tokio::task::spawn_blocking(move || get(&state2, &path, &csrf2, page_no, &origin))
        .await
        .unwrap_or_else(|e| html(500, page("Error", Some(&csrf), &format!("<p>{}</p>", esc(&e.to_string())))))
}

/// `https://host` of the request, for the connection examples.
fn origin_of(headers: &HeaderMap) -> String {
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("localhost:8080");
    let local = host.starts_with("localhost") || host.starts_with("127.") || host.starts_with("[::1]");
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|p| p.split(',').next().unwrap_or("https").trim().to_owned())
        .unwrap_or_else(|| if local { "http".into() } else { "https".into() });
    format!("{proto}://{host}")
}

fn schema_of(state: &State, db: &Embedded, name: &str) -> memoire_orm::Result<&'static TableSchema> {
    let info = db.tables()?.into_iter().find(|t| t.name == name).ok_or_else(|| Error::NoSuchTable(name.to_owned()))?;
    state.schemas.schema(&info.schema)
}

/// `/admin/apps/<name>`.
fn app_base(name: &str) -> String {
    format!("/admin/apps/{}", url_encode(name))
}

/// `/admin/apps/<name>/<rest>` → the application and `/<rest>`.
fn split_app(path: &str) -> Option<(String, &str)> {
    let rest = path.strip_prefix("/admin/apps/")?;
    let (name, sub) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    Some((url_decode(name), sub))
}

fn ago_ms(ms: i64) -> String {
    if ms <= 0 {
        return "never".into();
    }
    format!("{} ago", duration(Duration::from_millis((crate::engine::store::now_millis() - ms).max(0) as u64)))
}

fn date_ms(ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .map_or_else(|| "—".into(), |t| t.format("%Y-%m-%d %H:%M UTC").to_string())
}

fn lock_badge(engine: &crate::engine::Engine) -> String {
    match engine.encryption() {
        Some(_) => {
            r#"<span class="pill ok" title="XTS-AES-256 pages, AES-256-GCM log and backups">&#128274; encrypted</span>"#
                .into()
        }
        None => r#"<span class="pill bad">in clear</span>"#.into(),
    }
}

/// The application's header: its name, badges, the other applications and
/// its tabs.
fn app_header(state: &State, app: &App, tab: &str) -> String {
    let base = app_base(&app.name);
    let mut out = String::from(r#"<div class="apphead"><div class="row-title">"#);
    let _ = write!(out, "<h1>{}</h1>", esc(&app.name));
    if app.first {
        out.push_str(r#"<span class="pill">first application</span>"#);
    }
    out.push_str(&lock_badge(&app.engine));
    out.push_str("</div>");
    let others: Vec<Arc<App>> = state.apps.all().into_iter().filter(|a| a.name != app.name).collect();
    if !others.is_empty() {
        out.push_str(r#"<p class="switch"><span class="muted">Other applications:</span> "#);
        for o in &others {
            let _ = write!(out, r#"<a class="chip" href="{}{}">{}</a> "#, app_base(&o.name), esc(tab), esc(&o.name));
        }
        out.push_str("</p>");
    }
    out.push_str(r#"<nav class="tabs">"#);
    for (sub, label) in [
        ("", "Overview"),
        ("/tables", "Tables"),
        ("/query", "Query"),
        ("/branches", "Branches"),
        ("/storage", "Storage"),
        ("/tokens", "Tokens"),
        ("/settings", "Settings"),
    ] {
        let current = if sub == tab { r#" aria-current="page""# } else { "" };
        let _ = write!(out, r#"<a href="{base}{sub}"{current}>{label}</a>"#);
    }
    out.push_str("</nav></div>");
    out
}

fn app_page(state: &State, app: &App, tab: &str, title: &str, csrf: &str, body: &str) -> String {
    page(&format!("{title} · {}", app.name), Some(csrf), &format!("{}{body}", app_header(state, app, tab)))
}

/// A token, shown once, and how to connect with it.
fn token_reveal(origin: &str, app: &str, token: &str) -> String {
    format!(
        r#"<div class="msg ok reveal"><p><b>New token of {app}: copy it now, it is not shown again</b> (the server keeps only its digest).</p>
<pre><code>{token}</code></pre><p>Connect the application with it:</p>
<pre><code>NUAGE_URL={origin}
NUAGE_TOKEN={token}</code></pre>
<pre><code>let db = NuageDb::connect("{origin}", "{token}").await?;</code></pre></div>"#,
        app = esc(app),
        token = esc(token),
        origin = esc(origin)
    )
}

fn connect_help(origin: &str) -> String {
    format!(
        r#"<h2>Connect</h2><p class="muted">The token an application sends picks its database: it reads and writes only its own.</p>
<pre><code>NUAGE_URL={o}
NUAGE_TOKEN=&lt;a token of this application&gt;</code></pre>
<pre><code>let db = nuage::Db::from_env().await?;                      // or
let db = NuageDb::connect("{o}", token).await?;</code></pre>"#,
        o = esc(origin)
    )
}

/// The list of applications, as cards.
fn apps_body(state: &State, csrf: &str, message: Option<String>) -> String {
    let mut body = String::from("<h1>Applications</h1>");
    if let Some(m) = message {
        body.push_str(&m);
    }
    let all = state.apps.all();
    let _ = write!(
        body,
        r#"<p class="muted">{} application(s) on this server. Each one has its own database, tokens, encryption keys and backups.</p><div class="apps">"#,
        all.len()
    );
    let env = state.options.token.is_some();
    for app in &all {
        let engine = &app.engine;
        let stats = engine.stats();
        let tables = Embedded::from_engine(engine.clone()).tables().unwrap_or_default();
        let (rate, errors) = app.metrics.rates();
        let (_, p99) = app.metrics.percentiles();
        let backup = engine.backup_status();
        let info = app.info(env);
        let tokens = info.tokens.len() + usize::from(info.env_token);
        let _ = write!(
            body,
            r#"<a class="card app{bad}" href="{href}"><div class="row-title"><b>{name}</b>{first}{lock}</div>{desc}<dl class="mini">
<div><dt>Tables</dt><dd>{tables}</dd></div><div><dt>Rows</dt><dd>{rows}</dd></div><div><dt>Size</dt><dd>{size}</dd></div>
<div><dt>Requests</dt><dd>{rate:.1}/s</dd></div><div><dt>p99</dt><dd>{p99:.1} ms</dd></div><div><dt>Errors</dt><dd>{errors:.1}/s</dd></div>
<div><dt>Branches</dt><dd>{branches}</dd></div><div><dt>Tokens</dt><dd>{tokens}</dd></div><div><dt>Last request</dt><dd>{last}</dd></div>
</dl>{backup_note}</a>"#,
            bad = if backup.last_ok { "" } else { " is-bad" },
            href = app_base(&app.name),
            name = esc(&app.name),
            first = if app.first { r#"<span class="pill">first</span>"# } else { "" },
            lock = lock_badge(engine),
            desc = if info.description.is_empty() {
                String::new()
            } else {
                format!(r#"<small>{}</small>"#, esc(&info.description))
            },
            tables = tables.len(),
            rows = tables.iter().map(|t| t.rows).sum::<i64>(),
            size = bytes(stats.file_bytes),
            branches = engine.branches().len(),
            last = ago_ms(app.last_request.load(std::sync::atomic::Ordering::Relaxed)),
            backup_note = if backup.last_ok {
                String::new()
            } else {
                format!(r#"<small class="bad">Backup failing: {}</small>"#, esc(&backup.last_error.unwrap_or_default()))
            },
        );
    }
    for (name, error) in state.apps.failed() {
        let _ = write!(
            body,
            r#"<a class="card app is-bad" href="{}/settings"><div class="row-title"><b>{}</b><span class="pill bad">cannot be opened</span></div><small>{}</small></a>"#,
            app_base(&name),
            esc(&name),
            esc(&error)
        );
    }
    let _ = write!(
        body,
        r#"</div><h2>New application</h2><form method="post" action="/admin/apps" class="row"><input type="hidden" name="csrf" value="{}">
<label>Name<input name="name" pattern="[a-z0-9_\-]{{1,63}}" required placeholder="shop"></label><label>Description<input name="description" placeholder="optional"></label><button>Create</button></form>
<p class="muted">Its database is created at once, encrypted with keys of its own, with a first token.</p>"#,
        esc(csrf)
    );
    body
}

fn app_overview(state: &State, app: &App, csrf: &str, origin: &str) -> String {
    let engine = &app.engine;
    let stats = engine.stats();
    let tables = Embedded::from_engine(engine.clone()).tables().unwrap_or_default();
    let backup = engine.backup_status();
    let (rate, errors) = app.metrics.rates();
    let (p50, p99) = app.metrics.percentiles();
    let info = app.info(state.options.token.is_some());
    let mut body = String::new();
    if !info.description.is_empty() {
        let _ = write!(body, r#"<p class="muted">{}</p>"#, esc(&info.description));
    }
    body.push_str(r#"<div class="grid">"#);
    for (l, v) in [
        ("Size on disk", bytes(stats.file_bytes)),
        ("Tables", tables.len().to_string()),
        ("Rows", tables.iter().map(|t| t.rows).sum::<i64>().to_string()),
        ("Branches", engine.branches().len().to_string()),
        ("Requests", format!("{rate:.1}/s, {} in all", app.metrics.total())),
        ("Latency", format!("p50 {p50:.2} ms, p99 {p99:.2} ms")),
        ("Errors", format!("{errors:.1}/s")),
        ("Last request", ago_ms(app.last_request.load(std::sync::atomic::Ordering::Relaxed))),
        ("Tokens", (info.tokens.len() + usize::from(info.env_token)).to_string()),
        (
            "Encryption",
            engine.encryption().map_or("off: stored in clear".into(), |k| format!("on ({k}), keys of its own")),
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
        ("Created", if app.first { "with the server".into() } else { date_ms(app.created_at) }),
    ] {
        body.push_str(&card(l, &v));
    }
    body.push_str("</div>");
    body.push_str(&connect_help(origin));
    let _ = write!(
        body,
        r#"<p class="muted">Data is stored in <code>{}</code>.</p>"#,
        esc(&engine.dir().display().to_string())
    );
    let _ = csrf;
    body
}

fn tables_body(app: &App) -> Result<String, Error> {
    let db = Embedded::from_engine(app.engine.clone());
    let tables = db.tables()?;
    let base = app_base(&app.name);
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
    let mut body = table_html(&["Table".into(), "Rows".into(), "Size".into(), "Columns".into()], &rows);
    body.push_str("<p>");
    for t in &tables {
        let _ = write!(body, r#"<a href="{base}/tables/{0}">Browse {0}</a> · "#, esc(&t.name));
    }
    body.push_str("</p>");
    Ok(body)
}

fn rows_body(state: &State, app: &App, name: &str, page_no: u64) -> Result<String, Error> {
    let db = Embedded::from_engine(app.engine.clone());
    let schema = schema_of(state, &db, name)?;
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
    let rows = db.fetch_rows(Query::Select(&select))?;
    let headers: Vec<String> = schema.columns.iter().map(|c| c.name.to_owned()).collect();
    let mut body = format!("<h2>{}</h2>", esc(name));
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
    Ok(body)
}

fn server_body(state: &State) -> String {
    let engine = &state.engine;
    let open = state.metrics.open_connections.load(std::sync::atomic::Ordering::Relaxed);
    let active = state.metrics.active_requests.load(std::sync::atomic::Ordering::Relaxed);
    let (rate, _) = state.metrics.rates();
    let (p50, p99) = state.metrics.percentiles();
    let all = state.apps.all();
    let disk: u64 = all.iter().map(|a| a.engine.stats().file_bytes).sum();
    let mut body = String::from(r#"<h1>Server</h1><div class="grid">"#);
    for (l, v) in [
        ("Version", format!("Nuage DB {}", crate::VERSION)),
        ("Uptime", duration(engine.uptime())),
        ("Applications", all.len().to_string()),
        ("Size on disk", bytes(disk)),
        ("Connections", format!("{} active, {} idle", active, open.saturating_sub(active))),
        ("Transactions open", state.open_transactions().to_string()),
        ("Requests", format!("{rate:.1}/s, {} in all", state.metrics.total())),
        ("Latency", format!("p50 {p50:.2} ms, p99 {p99:.2} ms")),
        (
            "Encryption",
            if all.iter().all(|a| a.engine.encryption().is_some()) {
                format!("{} ({})", crate::crypto::SCHEME, engine.encryption().unwrap_or("key"))
            } else {
                "off for some data: set NUAGE_ENCRYPTION_KEY".into()
            },
        ),
        ("Backup target", engine.backup_status().target.unwrap_or_else(|| "off".into())),
        ("Free disk", engine.stats().disk_free_bytes.map_or("unknown".into(), bytes)),
    ] {
        body.push_str(&card(l, &v));
    }
    let _ = write!(
        body,
        r#"</div><p class="muted">Data is stored in <code>{}</code> (the others in <code>apps/</code>).</p><h2>Events</h2>"#,
        esc(&engine.dir().display().to_string())
    );
    let mut events: Vec<(i64, String, &'static str, String)> = all
        .iter()
        .flat_map(|a| a.engine.events().0.into_iter().map(|e| (e.at, a.name.clone(), e.level, e.message)))
        .collect();
    events.sort_by_key(|e| std::cmp::Reverse(e.0));
    events.truncate(100);
    let rows: Vec<Vec<Value>> = events
        .into_iter()
        .map(|(at, app, level, message)| {
            vec![Value::Text(date_ms(at)), Value::Text(app), Value::Text(level.into()), Value::Text(message)]
        })
        .collect();
    body.push_str(&table_html(&["Time".into(), "Application".into(), "Level".into(), "Event".into()], &rows));
    body
}

fn get(state: &State, path: &str, csrf: &str, page_no: u64, origin: &str) -> Response<HttpBody> {
    match path {
        "/admin" | "/admin/apps" => return html(200, page("Applications", Some(csrf), &apps_body(state, csrf, None))),
        "/admin/server" => return html(200, page("Server", Some(csrf), &server_body(state))),
        _ => {}
    }
    let Some((name, sub)) = split_app(path) else {
        return html(404, page("Not found", Some(csrf), "<h1>Not found</h1>"));
    };
    let Some(app) = state.apps.get(&name) else {
        if sub == "/settings" && state.apps.failed().iter().any(|(n, _)| *n == name) {
            return html(200, page("Settings", Some(csrf), &failed_settings(&name, csrf)));
        }
        return html(404, page("Not found", Some(csrf), "<h1>No such application</h1>"));
    };
    let (tab, title, body): (&str, &str, Result<String, Error>) = match sub {
        "" => ("", "Overview", Ok(app_overview(state, &app, csrf, origin))),
        "/tables" => ("/tables", "Tables", tables_body(&app)),
        t if t.starts_with("/tables/") => ("/tables", "Rows", rows_body(state, &app, &t["/tables/".len()..], page_no)),
        "/query" => ("/query", "Query", Ok(query_form(&app, csrf, "", None))),
        "/branches" => ("/branches", "Branches", Ok(branches_body(&app, csrf, None))),
        "/storage" => ("/storage", "Storage", Ok(storage_body(&app, csrf, None))),
        "/tokens" => ("/tokens", "Tokens", Ok(tokens_body(state, &app, csrf, None))),
        "/settings" => ("/settings", "Settings", Ok(settings_body(&app, csrf, None))),
        _ => return html(404, page("Not found", Some(csrf), "<h1>Not found</h1>")),
    };
    match body {
        Ok(b) => html(200, app_page(state, &app, tab, title, csrf, &b)),
        Err(e) => html(400, app_page(state, &app, tab, "Error", csrf, &note(false, &e.to_string()))),
    }
}

fn query_form(app: &App, csrf: &str, sql: &str, extra: Option<&str>) -> String {
    format!(
        r#"<form method="post" action="{}/query"><input type="hidden" name="csrf" value="{}">
<textarea name="sql" spellcheck="false" placeholder="SELECT * FROM users WHERE age >= 18 ORDER BY name LIMIT 20">{}</textarea>
<p class="row"><button>Run</button></p></form>
<p class="muted">SELECT, UPDATE and DELETE on one table of this application, with WHERE, ORDER BY, LIMIT and OFFSET. Statements that change data ask for a confirmation first.</p>{}"#,
        app_base(&app.name),
        esc(csrf),
        esc(sql),
        extra.unwrap_or_default()
    )
}

fn branches_body(app: &App, csrf: &str, message: Option<String>) -> String {
    let engine = &app.engine;
    let base = app_base(&app.name);
    let mut body = message.unwrap_or_default();
    body.push_str(r#"<p class="muted">A branch is an instant copy of the database: it shares the data and stores only its own changes.</p>"#);
    let branches = engine.branches();
    let mut out = String::from(
        r#"<div class="scroll"><table><thead><tr><th>Branch</th><th>From</th><th>Own data</th><th></th></tr></thead><tbody>"#,
    );
    for b in &branches {
        let _ = write!(
            out,
            r#"<tr><td>{n}</td><td>{p}</td><td>{s}</td><td><form method="post" action="{base}/branches/{n}/reset" style="display:inline"><input type="hidden" name="csrf" value="{c}"><button class="plain">Reset</button></form> · <form method="post" action="{base}/branches/{n}/delete" style="display:inline"><input type="hidden" name="csrf" value="{c}"><button class="plain">Delete</button></form></td></tr>"#,
            n = esc(&b.name),
            p = esc(b.parent.as_deref().unwrap_or(&app.name)),
            s = bytes(b.own_bytes),
            c = esc(csrf)
        );
    }
    if branches.is_empty() {
        out.push_str(r#"<tr><td colspan="4" class="muted">No branch yet.</td></tr>"#);
    }
    out.push_str("</tbody></table></div>");
    body.push_str(&out);
    let mut options = format!(r#"<option value="">{}</option>"#, esc(&app.name));
    for b in &branches {
        let _ = write!(options, r#"<option>{}</option>"#, esc(&b.name));
    }
    let _ = write!(
        body,
        r#"<h2>New branch</h2><form method="post" action="{base}/branches" class="row"><input type="hidden" name="csrf" value="{}">
<label>Name<input name="name" pattern="[a-z0-9_\-]{{1,63}}" required placeholder="preview-42"></label><label>From<select name="from">{options}</select></label><button>Create</button></form>"#,
        esc(csrf)
    );
    body
}

fn storage_body(app: &App, csrf: &str, message: Option<String>) -> String {
    let engine = &app.engine;
    let s = engine.stats();
    let b = engine.backup_status();
    let mut body = message.unwrap_or_default();
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
        ("Encryption", engine.encryption().map_or("off".into(), |_| crate::crypto::SCHEME.into())),
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
                (Some(at), true) => ago_ms(at),
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
            r#"<form method="post" action="{}/storage/{action}"><input type="hidden" name="csrf" value="{}"><button>{label}</button></form>"#,
            app_base(&app.name),
            esc(csrf)
        );
    }
    body.push_str("</div>");
    body
}

fn tokens_body(state: &State, app: &App, csrf: &str, message: Option<String>) -> String {
    let base = app_base(&app.name);
    let info = app.info(state.options.token.is_some());
    let mut body = message.unwrap_or_default();
    body.push_str(r#"<p class="muted">A token opens this application's database, and only it. The server keeps a SHA-256 digest of each token, never the token: a lost token cannot be shown again, make a new one.</p>"#);
    let mut out = String::from(
        r#"<div class="scroll"><table><thead><tr><th>Token</th><th>Created</th><th></th></tr></thead><tbody>"#,
    );
    if info.env_token {
        out.push_str(r#"<tr><td><code>NUAGE_TOKEN</code></td><td class="muted">set on the server</td><td class="muted">change it in the environment</td></tr>"#);
    }
    for t in &info.tokens {
        let _ = write!(
            out,
            r#"<tr><td><code>nuage_{id}…</code></td><td>{at}</td><td><form method="post" action="{base}/tokens/{id}/revoke" style="display:inline"><input type="hidden" name="csrf" value="{c}"><button class="plain">Revoke</button></form></td></tr>"#,
            id = esc(&t.id),
            at = date_ms(t.created_at),
            c = esc(csrf)
        );
    }
    if info.tokens.is_empty() && !info.env_token {
        out.push_str(r#"<tr><td colspan="3" class="muted">No token: no application can connect.</td></tr>"#);
    }
    out.push_str("</tbody></table></div>");
    body.push_str(&out);
    let _ = write!(
        body,
        r#"<form method="post" action="{base}/tokens" class="row" style="margin-top:.75rem"><input type="hidden" name="csrf" value="{}"><button>New token</button></form>"#,
        esc(csrf)
    );
    body
}

fn settings_body(app: &App, csrf: &str, message: Option<String>) -> String {
    let base = app_base(&app.name);
    let mut body = message.unwrap_or_default();
    let description = app.description.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let _ = write!(
        body,
        r#"<h2>Description</h2><form method="post" action="{base}/description" class="row"><input type="hidden" name="csrf" value="{c}">
<label>Description<input name="description" value="{d}" placeholder="what this application is"></label><button>Save</button></form>"#,
        c = esc(csrf),
        d = esc(&description)
    );
    if app.first {
        body.push_str(r#"<h2>Delete</h2><p class="muted">The first application is the server's own database: it cannot be deleted.</p>"#);
    } else {
        body.push_str(&delete_form(&app.name, csrf));
    }
    body
}

fn delete_form(name: &str, csrf: &str) -> String {
    format!(
        r#"<h2>Delete</h2><p>Deleting <b>{n}</b> removes its tokens, its data and its backups. It cannot be undone.</p>
<form method="post" action="{base}/delete" class="row"><input type="hidden" name="csrf" value="{c}">
<label>Type <code>{n}</code> to confirm<input name="confirm" required autocomplete="off"></label><button class="danger">Delete {n}</button></form>"#,
        n = esc(name),
        base = app_base(name),
        c = esc(csrf)
    )
}

fn failed_settings(name: &str, csrf: &str) -> String {
    format!(
        r#"<h1>{}</h1><p class="msg bad">This application could not be opened at startup (see the server's events).</p>{}"#,
        esc(name),
        delete_form(name, csrf)
    )
}

fn note(ok: bool, text: &str) -> String {
    format!(r#"<p class="msg {}">{}</p>"#, if ok { "ok" } else { "bad" }, esc(text))
}

async fn post(
    state: &Arc<State>,
    path: &str,
    form: HashMap<String, String>,
    session: &str,
    csrf: &str,
    origin: String,
) -> Response<HttpBody> {
    if path == "/admin/logout" {
        state.admin.remove(session);
        let mut res = redirect("/admin/login");
        set_cookie(&mut res, SESSION_COOKIE, "", 0);
        return res;
    }
    let state2 = state.clone();
    let path2 = path.to_owned();
    let csrf2 = csrf.to_owned();
    tokio::task::spawn_blocking(move || post_now(&state2, &path2, &form, &csrf2, &origin))
        .await
        .unwrap_or_else(|e| html(500, page("Error", Some(csrf), &format!("<p>{}</p>", esc(&e.to_string())))))
}

fn post_now(state: &State, path: &str, form: &HashMap<String, String>, csrf: &str, origin: &str) -> Response<HttpBody> {
    let field = |k: &str| form.get(k).map(String::as_str).unwrap_or_default().to_owned();
    if path == "/admin/apps" {
        let msg = match state.apps.create(field("name").trim(), &field("description")) {
            Ok((app, token)) => format!(
                "{}{}",
                note(true, &format!("Application {} created, with its own encrypted database.", app.name)),
                token_reveal(origin, &app.name, &token)
            ),
            Err(e) => note(false, &e.to_string()),
        };
        return html(200, page("Applications", Some(csrf), &apps_body(state, csrf, Some(msg))));
    }
    let Some((name, sub)) = split_app(path) else {
        return html(404, page("Not found", Some(csrf), "<h1>Not found</h1>"));
    };
    if sub == "/delete" {
        if field("confirm").trim() != name {
            let msg = note(false, &format!("Type {name} to confirm: nothing was deleted."));
            return match state.apps.get(&name) {
                Some(app) => html(
                    400,
                    app_page(state, &app, "/settings", "Settings", csrf, &settings_body(&app, csrf, Some(msg))),
                ),
                None => html(400, page("Settings", Some(csrf), &format!("{msg}{}", failed_settings(&name, csrf)))),
            };
        }
        let msg = match state.apps.delete(&name) {
            Ok(true) => note(true, &format!("Application {name} deleted, with its data and backups.")),
            Ok(false) => note(false, &format!("There is no application {name}.")),
            Err(e) => note(false, &e.to_string()),
        };
        return html(200, page("Applications", Some(csrf), &apps_body(state, csrf, Some(msg))));
    }
    let Some(app) = state.apps.get(&name) else {
        return html(404, page("Not found", Some(csrf), "<h1>No such application</h1>"));
    };
    let engine = &app.engine;
    let (sql, confirmed, branch, from) = (field("sql"), field("confirm") == "yes", field("name"), field("from"));
    match sub {
        "/query" => {
            let db = Embedded::from_engine(engine.clone());
            let lookup = |n: &str| schema_of(state, &db, n);
            let result =
                query::parse(&sql, &lookup).and_then(|stmt| run_statement(&app, &db, &stmt, confirmed, csrf, &sql));
            let (status, extra) = match result {
                Ok(extra) => (200, extra),
                Err(e) => (400, note(false, &e.to_string())),
            };
            html(status, app_page(state, &app, "/query", "Query", csrf, &query_form(&app, csrf, &sql, Some(&extra))))
        }
        "/branches" => {
            let from = (!from.is_empty()).then_some(from.as_str());
            let msg = match engine.create_branch(&branch, from) {
                Ok(b) => note(true, &format!("Branch {} created.", b.name)),
                Err(e) => note(false, &e.to_string()),
            };
            html(200, app_page(state, &app, "/branches", "Branches", csrf, &branches_body(&app, csrf, Some(msg))))
        }
        p if p.starts_with("/branches/") => {
            let rest = &p["/branches/".len()..];
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
            html(200, app_page(state, &app, "/branches", "Branches", csrf, &branches_body(&app, csrf, Some(msg))))
        }
        p if p.starts_with("/storage/") => {
            let msg = match &p["/storage/".len()..] {
                "checkpoint" => engine.checkpoint().map(|l| note(true, &format!("Checkpoint at {l}."))),
                "backup" => engine.backup_now(true).map(|()| note(true, "Backup done.")),
                "integrity" => engine.check_integrity().map(|ok| {
                    note(
                        ok,
                        if ok { "Every page checksum is right." } else { "Damaged pages were found and repaired." },
                    )
                }),
                "compact" => engine
                    .compact()
                    .map(|done| note(done, if done { "Compaction done." } else { "Busy: compaction will run later." })),
                _ => Ok(note(false, "Unknown action.")),
            }
            .unwrap_or_else(|e| note(false, &e.to_string()));
            html(200, app_page(state, &app, "/storage", "Storage", csrf, &storage_body(&app, csrf, Some(msg))))
        }
        "/tokens" => {
            let msg = match state.apps.new_token(&app) {
                Ok(token) => token_reveal(origin, &app.name, &token),
                Err(e) => note(false, &e.to_string()),
            };
            html(200, app_page(state, &app, "/tokens", "Tokens", csrf, &tokens_body(state, &app, csrf, Some(msg))))
        }
        p if p.starts_with("/tokens/") && p.ends_with("/revoke") => {
            let id = &p["/tokens/".len()..p.len() - "/revoke".len()];
            let msg = match state.apps.revoke_token(&app, id) {
                Ok(true) => note(true, &format!("Token nuage_{id}… revoked: it opens nothing any more.")),
                Ok(false) => note(false, "There is no such token."),
                Err(e) => note(false, &e.to_string()),
            };
            html(200, app_page(state, &app, "/tokens", "Tokens", csrf, &tokens_body(state, &app, csrf, Some(msg))))
        }
        "/description" => {
            let msg = match state.apps.set_description(&app, &field("description")) {
                Ok(()) => note(true, "Description saved."),
                Err(e) => note(false, &e.to_string()),
            };
            html(200, app_page(state, &app, "/settings", "Settings", csrf, &settings_body(&app, csrf, Some(msg))))
        }
        _ => html(404, page("Not found", Some(csrf), "<h1>Not found</h1>")),
    }
}

/// Runs a console statement: reads at once, writes after a confirmation.
fn run_statement(
    app: &App,
    db: &Embedded,
    stmt: &Statement,
    confirmed: bool,
    csrf: &str,
    sql: &str,
) -> memoire_orm::Result<String> {
    if stmt.writes() && !confirmed {
        let preview = stmt.preview().ok_or_else(|| Error::Unsupported("no preview".into()))?;
        let n = db
            .fetch_rows(Query::Select(&preview))?
            .first()
            .and_then(|r| r.first())
            .and_then(Value::as_i64)
            .unwrap_or(0);
        return Ok(format!(
            r#"<div class="msg"><p><b>This statement changes data of {app}: {n} row(s) would be affected.</b></p>
<form method="post" action="{base}/query"><input type="hidden" name="csrf" value="{}"><input type="hidden" name="sql" value="{}"><input type="hidden" name="confirm" value="yes"><button class="danger">Confirm and run</button></form></div>"#,
            esc(csrf),
            esc(sql),
            app = esc(&app.name),
            base = app_base(&app.name),
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
