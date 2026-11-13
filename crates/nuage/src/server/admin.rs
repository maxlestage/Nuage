//! The management console, under `/admin`: server-rendered pages and plain
//! forms, no JavaScript, embedded in the binary. `/admin` lists the
//! applications (and makes new ones); `/admin/apps/<name>/...` are the pages
//! of one application (tables, query, branches, storage, tokens, settings);
//! `/admin/server` is the server and the events of every application.
//!
//! In English, French and Spanish: the `nuage_lang` cookie (set by the
//! `?lang=` links of every page), else `Accept-Language`, else English.
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
use super::i18n::{Lang, t, tf};
use super::{HttpBody, State, full, random_token, read_body, same_secret};
use crate::embedded::Embedded;
use crate::engine::MAIN;

const SESSION_COOKIE: &str = "nuage_session";
const LOGIN_COOKIE: &str = "nuage_login";
/// The console's language (`fr`, `en`, `es`), chosen with `?lang=`.
pub(crate) const LANG_COOKIE: &str = "nuage_lang";
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

pub(crate) fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
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
details.lang{border-bottom:1px solid var(--line)}details.lang summary{cursor:pointer;padding:.45rem 0;font-weight:600}
"#;

fn lang_switch(l: Lang, here: &str) -> String {
    let mut out = String::from(r#"<span class="langs">"#);
    for other in Lang::ALL {
        let sep = if other == Lang::En { "" } else { " · " };
        if other == l {
            let _ = write!(out, r#"{sep}<b aria-current="true">{}</b>"#, other.code().to_uppercase());
        } else {
            let _ = write!(
                out,
                r#"{sep}<a href="{}?lang={code}" hreflang="{code}" lang="{code}">{}</a>"#,
                esc(here),
                other.code().to_uppercase(),
                code = other.code()
            );
        }
    }
    out.push_str("</span>");
    out
}

fn page(l: Lang, here: &str, title: &str, csrf: Option<&str>, body: &str) -> String {
    let nav = match csrf {
        Some(csrf) => format!(
            r#"<nav><a href="/admin">{apps}</a><a href="/admin/server">{server}</a></nav>{langs}
<form method="post" action="/admin/logout" style="margin-left:auto"><input type="hidden" name="csrf" value="{c}"><button class="plain">{out}</button></form>"#,
            apps = t(l, "Applications"),
            server = t(l, "Server"),
            langs = lang_switch(l, here),
            c = esc(csrf),
            out = t(l, "Log out")
        ),
        None => format!(r#"<span style="margin-left:auto">{}</span>"#, lang_switch(l, here)),
    };
    format!(
        r#"<!doctype html><html lang="{lang}"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="robots" content="noindex"><meta name="color-scheme" content="light dark"><title>{} · Nuage DB</title><style>{STYLE}</style></head>
<body><header><strong>Nuage DB</strong>{nav}</header><main>{body}</main></body></html>"#,
        esc(title),
        lang = l.code()
    )
}

fn html(status: u16, body: String) -> Response<HttpBody> {
    let mut res = Response::new(full(body));
    *res.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::VARY, HeaderValue::from_static("Accept-Language, Cookie"));
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

/// The request reached the server over HTTPS (behind a proxy that says so),
/// or from this machine: browsers keep `Secure` cookies there only.
fn secure_context(headers: &HeaderMap) -> bool {
    let https = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|p| p.split(',').next().unwrap_or_default().trim().eq_ignore_ascii_case("https"));
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or_default();
    https || host.starts_with("localhost") || host.starts_with("127.") || host.starts_with("[::1]")
}

/// A cookie of the console: `Secure` where the browser can keep it so (a
/// server reached in plain HTTP, freshly installed, could not log in
/// otherwise; its login page says to put HTTPS in front).
fn set_cookie(res: &mut Response<HttpBody>, secure: bool, name: &str, value: &str, max_age: u64) {
    let flag = if secure { " Secure;" } else { "" };
    let c = format!("{name}={value}; Path=/; HttpOnly;{flag} SameSite=Strict; Max-Age={max_age}");
    if let Ok(v) = HeaderValue::from_str(&c) {
        res.headers_mut().append(header::SET_COOKIE, v);
    }
}

fn bytes(l: Lang, n: u64) -> String {
    let units: [&str; 5] =
        if l == Lang::En { ["B", "KiB", "MiB", "GiB", "TiB"] } else { ["o", "Kio", "Mio", "Gio", "Tio"] };
    let units = if l == Lang::Es { ["B", "KiB", "MiB", "GiB", "TiB"] } else { units };
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < units.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    let s = if u == 0 { format!("{n} {}", units[0]) } else { format!("{v:.1} {}", units[u]) };
    if l == Lang::En { s } else { s.replace('.', ",") }
}

fn duration(l: Lang, d: Duration) -> String {
    let s = d.as_secs();
    let day = if l == Lang::En { "d" } else { "j" };
    let day = if l == Lang::Es { "d" } else { day };
    match s {
        0..=59 => format!("{s} s"),
        60..=3599 => format!("{} min", s / 60),
        3600..=86_399 => format!("{} h {} min", s / 3600, s / 60 % 60),
        _ => format!("{} {day} {} h", s / 86_400, s / 3600 % 24),
    }
}

/// A number written with a decimal comma in French and Spanish.
fn dec(l: Lang, s: String) -> String {
    if l == Lang::En { s } else { s.replace('.', ",") }
}

fn card(label: &str, value: &str) -> String {
    format!(r#"<div class="card"><small>{}</small><b>{}</b></div>"#, esc(label), esc(value))
}

fn cell(v: &Value) -> String {
    match v {
        Value::Null => r#"<span class="muted">NULL</span>"#.into(),
        Value::Text(s) => esc(s),
        Value::Bytes(b) => esc(&format!("{} bytes", b.len())),
        other => esc(&crate::sql::value::to_text(other).unwrap_or_default()),
    }
}

fn table_html(l: Lang, headers: &[String], rows: &[Vec<Value>]) -> String {
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
        let _ =
            write!(out, r#"<tr><td colspan="{}" class="muted">{}</td></tr>"#, headers.len().max(1), t(l, "No rows."));
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
pub(crate) fn home_page(state: &State, headers: &HeaderMap) -> Response<HttpBody> {
    let l = Lang::of(headers);
    let body = format!(
        r#"<h1>{}</h1><p>{}</p><p><a href="/admin">{}</a> · <a href="/api/health">{}</a></p>"#,
        t(l, "Nuage DB is running"),
        tf(
            l,
            "{} application(s), version {}. Everything stored is encrypted.",
            &[&state.apps.all().len(), &crate::VERSION]
        ),
        t(l, "Open the console"),
        t(l, "Health")
    );
    html(200, page(l, "/", t(l, "Home"), None, &body))
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

fn login_page(l: Lang, secure: bool, message: Option<&str>, status: u16, next: Option<&str>) -> Response<HttpBody> {
    let token = random_token();
    let msg = message.map_or(String::new(), |m| format!(r#"<p class="msg bad">{}</p>"#, esc(m)));
    let next = next
        .and_then(safe_next)
        .map_or(String::new(), |n| format!(r#"<input type="hidden" name="next" value="{}">"#, esc(n)));
    let clear = if secure {
        String::new()
    } else {
        format!(
            r#"<p class="msg bad">{}</p>"#,
            t(
                l,
                "This connection is not encrypted (HTTP): the password travels in clear. Reach the console over HTTPS (a domain name with the installer's NUAGE_DOMAIN, or a proxy), or through an SSH tunnel to localhost."
            )
        )
    };
    let body = format!(
        r#"<div class="login card"><h1>{console}</h1>{clear}{msg}<form method="post" action="/admin/login">
<input type="hidden" name="csrf" value="{token}">{next}<p><label>{password}<br><input type="password" name="password" autocomplete="current-password" required autofocus></label></p>
<button>{login}</button></form></div>"#,
        console = t(l, "Console"),
        password = t(l, "Password"),
        login = t(l, "Log in")
    );
    let mut res = html(status, page(l, "/admin/login", t(l, "Log in"), None, &body));
    set_cookie(&mut res, secure, LOGIN_COOKIE, &token, 3600);
    res
}

pub(crate) async fn route(state: &Arc<State>, req: Request<Incoming>, peer: SocketAddr) -> Response<HttpBody> {
    // `?lang=fr`: the console in that language from now on (a cookie).
    if let Some(choice) = query_param(&req, "lang").as_deref().and_then(Lang::from_code) {
        let back = req.uri().path().to_owned();
        let mut res = redirect(safe_next(&back).unwrap_or("/admin"));
        set_cookie(&mut res, secure_context(req.headers()), LANG_COOKIE, choice.code(), 365 * 24 * 3600);
        return res;
    }
    let l = Lang::of(req.headers());
    let secure = secure_context(req.headers());
    let Some(password) = state.options.admin_password.clone() else {
        return html(
            503,
            page(
                l,
                "/admin",
                t(l, "Console off"),
                None,
                &format!(
                    "<h1>{}</h1><p>{}</p>",
                    t(l, "The console is off"),
                    t(l, "Set <code>NUAGE_ADMIN_PASSWORD</code> on the server to turn it on.")
                ),
            ),
        );
    };
    let mut path = req.uri().path().trim_end_matches('/').to_owned();
    let method = req.method().clone();

    let next_param = query_param(&req, "next");
    if path == "/admin/login" {
        if method != Method::POST {
            return login_page(l, secure, None, 200, next_param.as_deref());
        }
        let ip = client_ip(req.headers(), peer);
        let expected = cookie(req.headers(), LOGIN_COOKIE).map(str::to_owned);
        let Ok(body) = read_body(state, req).await else {
            return login_page(l, secure, Some(t(l, "Invalid request.")), 400, None);
        };
        let form = parse_form(&body);
        let next = form.get("next").map(String::as_str);
        if !state.admin.may_try(ip) {
            return login_page(l, secure, Some(t(l, "Too many failed attempts. Wait a few minutes.")), 429, next);
        }
        let csrf_ok =
            matches!((&expected, form.get("csrf")), (Some(a), Some(b)) if same_secret(a.as_bytes(), b.as_bytes()));
        if !csrf_ok {
            return login_page(l, secure, Some(t(l, "The form expired. Try again.")), 403, next);
        }
        let given = form.get("password").map(String::as_str).unwrap_or_default();
        if !same_secret(given.as_bytes(), password.as_bytes()) {
            state.admin.failed(ip);
            return login_page(l, secure, Some(t(l, "Wrong password.")), 401, next);
        }
        let mut res = redirect(next.and_then(safe_next).unwrap_or("/admin"));
        set_cookie(&mut res, secure, SESSION_COOKIE, &state.admin.create(), SESSION_TTL.as_secs());
        set_cookie(&mut res, secure, LOGIN_COOKIE, "", 0);
        return res;
    }

    let Some((session, csrf)) = state.admin.current(req.headers()) else {
        if method != Method::GET {
            return login_page(l, secure, Some(t(l, "Log in first.")), 401, None);
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
    let cx = Cx { l, secure, csrf: csrf.clone(), here: path.clone() };
    if method == Method::POST {
        let Ok(body) = read_body(state, req).await else {
            return html(400, cx.page(t(l, "Error"), &format!("<p>{}</p>", t(l, "Invalid request."))));
        };
        let form = parse_form(&body);
        if !form.get("csrf").is_some_and(|c| same_secret(c.as_bytes(), csrf.as_bytes())) {
            return html(
                403,
                cx.page(
                    t(l, "Refused"),
                    &format!(
                        r#"<h1>{}</h1><p class="msg bad">{}</p>"#,
                        t(l, "Refused"),
                        t(l, "Invalid form token (CSRF). Reload the page and try again.")
                    ),
                ),
            );
        }
        return post(state, &path, form, &session, cx, origin).await;
    }

    let state2 = state.clone();
    let page_no = query_param(&req, "page").and_then(|p| p.parse().ok()).unwrap_or(0u64);
    let cx2 = cx.clone();
    tokio::task::spawn_blocking(move || get(&state2, &path, &cx2, page_no, &origin))
        .await
        .unwrap_or_else(|e| html(500, cx.page(t(l, "Error"), &format!("<p>{}</p>", esc(&e.to_string())))))
}

/// What every page of a session needs: its language, the CSRF token, where it is.
#[derive(Clone)]
struct Cx {
    l: Lang,
    secure: bool,
    csrf: String,
    here: String,
}

impl Cx {
    fn page(&self, title: &str, body: &str) -> String {
        page(self.l, &self.here, title, Some(&self.csrf), body)
    }

    fn t(&self, en: &'static str) -> &'static str {
        t(self.l, en)
    }

    fn tf(&self, en: &'static str, args: &[&dyn std::fmt::Display]) -> String {
        tf(self.l, en, args)
    }

    /// The hidden CSRF field of a form.
    fn field(&self) -> String {
        format!(r#"<input type="hidden" name="csrf" value="{}">"#, esc(&self.csrf))
    }
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

pub(crate) fn schema_of(state: &State, db: &Embedded, name: &str) -> memoire_orm::Result<&'static TableSchema> {
    let schema = db.table_schema(name)?.ok_or_else(|| Error::NoSuchTable(name.to_owned()))?;
    state.schemas.schema(&schema)
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

fn ago_ms(l: Lang, ms: i64) -> String {
    if ms <= 0 {
        return t(l, "never").into();
    }
    tf(l, "{} ago", &[&duration(l, Duration::from_millis((crate::engine::store::now_millis() - ms).max(0) as u64))])
}

fn date_ms(ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .map_or_else(|| "—".into(), |t| t.format("%Y-%m-%d %H:%M UTC").to_string())
}

fn lock_badge(l: Lang, engine: &crate::engine::Engine) -> String {
    match engine.encryption() {
        Some(_) => {
            format!(r#"<span class="pill ok" title="XTS-AES-256, AES-256-GCM">&#128274; {}</span>"#, t(l, "encrypted"))
        }
        None => format!(r#"<span class="pill bad">{}</span>"#, t(l, "in clear")),
    }
}

/// The application's header: its name, badges, the other applications and
/// its tabs.
fn app_header(state: &State, cx: &Cx, app: &App, tab: &str) -> String {
    let base = app_base(&app.name);
    let mut out = String::from(r#"<div class="apphead"><div class="row-title">"#);
    let _ = write!(out, "<h1>{}</h1>", esc(&app.name));
    if app.first {
        let _ = write!(out, r#"<span class="pill">{}</span>"#, cx.t("first application"));
    }
    out.push_str(&lock_badge(cx.l, &app.engine));
    out.push_str("</div>");
    let others: Vec<Arc<App>> = state.apps.all().into_iter().filter(|a| a.name != app.name).collect();
    if !others.is_empty() {
        let _ = write!(out, r#"<p class="switch"><span class="muted">{}</span> "#, cx.t("Other applications:"));
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
        let _ = write!(out, r#"<a href="{base}{sub}"{current}>{}</a>"#, cx.t(label));
    }
    out.push_str("</nav></div>");
    out
}

fn app_page(state: &State, cx: &Cx, app: &App, tab: &str, title: &str, body: &str) -> String {
    cx.page(&format!("{title} · {}", app.name), &format!("{}{body}", app_header(state, cx, app, tab)))
}

/// A token, shown once, and how to connect with it.
fn token_reveal(cx: &Cx, origin: &str, app: &str, token: &str) -> String {
    format!(
        r#"<div class="msg ok reveal"><p><b>{title}</b> {digest}</p>
<pre><code>{token}</code></pre><p>{connect}</p>
<pre><code>NUAGE_URL={origin}
NUAGE_TOKEN={token}</code></pre>
<pre><code>let db = NuageDb::connect("{origin}", "{token}").await?;</code></pre>
<p>{test}</p><pre><code>curl {origin}/v1/ping -H "Authorization: Bearer {token}"
nuage check {origin} {token}</code></pre></div>"#,
        test =
            cx.t("Test it: the server answers which application the token opens, and this page shows the connection."),
        title = esc(&cx.tf("New token of {}: copy it now, it is not shown again", &[&app])),
        digest = cx.t("(the server keeps only its digest)."),
        connect = cx.t("Connect the application with it:"),
        token = esc(token),
        origin = esc(origin)
    )
}

/// "2 min ago, PostgreSQL (psql), 203.0.113.7, read only".
fn connection_text(cx: &Cx, c: &super::apps::Connection) -> String {
    let mut parts = vec![ago_ms(cx.l, c.at), cx.t(c.via).to_owned()];
    if !c.client.is_empty() {
        parts[1] = format!("{} ({})", parts[1], c.client);
    }
    if !c.peer.is_empty() {
        parts.push(c.peer.clone());
    }
    if c.read_only {
        parts.push(cx.t("read only").to_owned());
    }
    parts.join(", ")
}

/// Whether a client of the application connected: so that nobody is left
/// guessing whether their connection works.
fn connection_banner(cx: &Cx, origin: &str, app: &App) -> String {
    match app.last_connection() {
        Some(c) => format!(
            r#"<p class="msg ok">✓ {}</p>"#,
            esc(&cx.tf("Connected: last connection {}.", &[&connection_text(cx, &c)]))
        ),
        None => format!(
            r#"<div class="msg"><p><b>{wait}</b> {how}</p><pre><code>curl {o}/v1/ping -H "Authorization: Bearer &lt;{a_token}&gt;"</code></pre><p><a href="">{refresh}</a></p></div>"#,
            wait = cx.t("Waiting for a first connection."),
            how = cx.t("Test yours: the server answers which application the token opens, and this page shows it as soon as it arrives."),
            o = esc(origin),
            a_token = cx.t("a token of this application"),
            refresh = cx.t("Refresh"),
        ),
    }
}

/// How to connect with a PostgreSQL driver, when the server speaks it.
fn postgres_help(cx: &Cx, origin: &str, app: &App, pg: Option<(u16, bool)>) -> String {
    let Some((port, tls)) = pg else {
        return format!(
            "<h2>PostgreSQL</h2><p class=\"muted\">{}</p>",
            cx.t("Set <code>NUAGE_PG_PORT=5432</code> and every language's PostgreSQL driver connects too (psql, psycopg, JDBC, node-postgres, pgx, Npgsql, PDO, the pg gem...).")
        );
    };
    let host = origin.split("://").nth(1).unwrap_or(origin).split(['/', ':']).next().unwrap_or("localhost");
    let host = if host.starts_with('[') { "localhost" } else { host };
    let ssl = if tls { "?sslmode=require" } else { "" };
    format!(
        r#"<h2>PostgreSQL</h2><p class="muted">{about}</p>
<pre><code>postgresql://app:&lt;{a_token}&gt;@{h}:{port}/{db}{ssl}</code></pre>
<pre><code>PGPASSWORD=&lt;{a_token}&gt; psql "host={h} port={port} user=app dbname={db}{ssl_kv}"</code></pre>{clear}"#,
        about = cx.t("Every language's PostgreSQL driver, ORM and tool connects: any user name, a token of this application as the password. The database name picks a branch."),
        a_token = cx.t("a token of this application"),
        h = esc(host),
        db = esc(&app.name),
        ssl_kv = if tls { " sslmode=require" } else { "" },
        clear = if tls {
            String::new()
        } else {
            format!(r#"<p class="muted">{}</p>"#, cx.t("Without TLS the token travels in clear: set NUAGE_TLS_CERT and NUAGE_TLS_KEY (the installer does it with a domain name)."))
        },
    )
}

fn connect_help(cx: &Cx, origin: &str, app: &App) -> String {
    let sites = app.origins();
    let sites = if sites.is_empty() {
        cx.tf(r#"No website yet: add yours in <a href="{}/settings">Settings</a>."#, &[&app_base(&app.name)])
    } else {
        cx.tf("Websites allowed: {}.", &[&esc(&sites.join(", "))])
    };
    format!(
        r#"<h2>{connect}</h2><p class="muted">{picks}</p>
<pre><code>NUAGE_URL={o}
NUAGE_TOKEN=&lt;{a_token}&gt;</code></pre>
<pre><code>let db = nuage::Db::from_env().await?;                      // or
let db = NuageDb::connect("{o}", token).await?;</code></pre>
<h2>{site}</h2><p class="muted">{any} {sites}</p>
{languages}<p class="muted">{more}</p>"#,
        languages = language_snippets(origin),
        more = cx.t(
            r#"Complete examples (also TypeScript, Perl and C), tested against a real server: <a href="https://github.com/maxlestage/Nuage/tree/master/clients">clients/</a>. To generate a client in another language: <a href="/v1/openapi.json">/v1/openapi.json</a> (OpenAPI)."#
        ),
        connect = cx.t("Connect"),
        picks = cx.t("The token an application sends picks its database: it reads and writes only its own."),
        a_token = cx.t("a token of this application"),
        site = cx.t("Connect a website"),
        any = cx.t(
            "Any language, or a page's JavaScript, sends SQL with <code>?</code> parameters to <code>POST /v1/sql</code> and gets rows as JSON. In a page, use a <b>read-only</b> token."
        ),
        o = esc(origin)
    )
}

/// The same request in each language, `{o}` being the server's address.
const SNIPPETS: [(&str, &str); 13] = [
    (
        "JavaScript",
        r#"const res = await fetch("{o}/v1/sql", {
  method: "POST",
  headers: { "Authorization": "Bearer " + TOKEN, "Content-Type": "application/json" },
  body: JSON.stringify({ sql: "SELECT * FROM articles WHERE category = ? ORDER BY id DESC LIMIT 20", params: [3] }),
});
const { rows } = await res.json();   // [{ "id": 42, "title": "…" }, …]"#,
    ),
    (
        "Python",
        r#"import json, os, urllib.request

def sql(query, params=()):
    req = urllib.request.Request("{o}/v1/sql",
        data=json.dumps({"sql": query, "params": list(params)}).encode(),
        headers={"Authorization": "Bearer " + os.environ["NUAGE_TOKEN"], "Content-Type": "application/json"})
    with urllib.request.urlopen(req) as res:
        return json.load(res)

rows = sql("SELECT * FROM articles WHERE category = ?", [3])["rows"]"#,
    ),
    (
        "PHP",
        r#"function sql(string $query, array $params = []): array {
    $ctx = stream_context_create(['http' => ['method' => 'POST', 'ignore_errors' => true,
        'header' => "Authorization: Bearer " . getenv('NUAGE_TOKEN') . "\r\nContent-Type: application/json\r\n",
        'content' => json_encode(['sql' => $query, 'params' => $params])]]);
    return json_decode(file_get_contents('{o}/v1/sql', false, $ctx), true);
}

$rows = sql('SELECT * FROM articles WHERE category = ?', [3])['rows'];"#,
    ),
    (
        "Go",
        r#"body, _ := json.Marshal(map[string]any{"sql": "SELECT * FROM articles WHERE category = ?", "params": []any{3}})
req, _ := http.NewRequest("POST", "{o}/v1/sql", bytes.NewReader(body))
req.Header.Set("Authorization", "Bearer "+os.Getenv("NUAGE_TOKEN"))
req.Header.Set("Content-Type", "application/json")
res, err := http.DefaultClient.Do(req)
// json.NewDecoder(res.Body).Decode(&result): {"columns": [...], "rows": [{...}]}"#,
    ),
    (
        "Ruby",
        r#"require "json"
require "net/http"

res = Net::HTTP.post(URI("{o}/v1/sql"),
  { sql: "SELECT * FROM articles WHERE category = ?", params: [3] }.to_json,
  "Authorization" => "Bearer #{ENV['NUAGE_TOKEN']}", "Content-Type" => "application/json")
rows = JSON.parse(res.body)["rows"]"#,
    ),
    (
        "Java",
        r#"var request = HttpRequest.newBuilder(URI.create("{o}/v1/sql"))
    .header("Authorization", "Bearer " + System.getenv("NUAGE_TOKEN"))
    .header("Content-Type", "application/json")
    .POST(HttpRequest.BodyPublishers.ofString(
        "{\"sql\": \"SELECT * FROM articles WHERE category = ?\", \"params\": [3]}"))
    .build();
String json = HttpClient.newHttpClient().send(request, HttpResponse.BodyHandlers.ofString()).body();"#,
    ),
    (
        "C#",
        r#"var http = new HttpClient();
http.DefaultRequestHeaders.Authorization =
    new AuthenticationHeaderValue("Bearer", Environment.GetEnvironmentVariable("NUAGE_TOKEN"));
var request = new JsonObject { ["sql"] = "SELECT * FROM articles WHERE category = ?", ["params"] = new JsonArray(3) };
var res = await http.PostAsync("{o}/v1/sql", new StringContent(request.ToJsonString(), Encoding.UTF8, "application/json"));
var rows = JsonNode.Parse(await res.Content.ReadAsStringAsync())!["rows"];"#,
    ),
    (
        "Rust",
        r#"// ureq + serde_json; for models and queries, memoire-orm's NuageDb
let mut res = ureq::post("{o}/v1/sql")
    .header("Authorization", format!("Bearer {}", std::env::var("NUAGE_TOKEN")?))
    .send_json(serde_json::json!({ "sql": "SELECT * FROM articles WHERE category = ?", "params": [3] }))?;
let rows = res.body_mut().read_json::<serde_json::Value>()?["rows"].take();"#,
    ),
    (
        "Swift",
        r#"var request = URLRequest(url: URL(string: "{o}/v1/sql")!)
request.httpMethod = "POST"
request.setValue("Bearer " + token, forHTTPHeaderField: "Authorization")
request.setValue("application/json", forHTTPHeaderField: "Content-Type")
request.httpBody = try JSONSerialization.data(withJSONObject:
    ["sql": "SELECT * FROM articles WHERE category = ?", "params": [3]])
let (data, _) = try await URLSession.shared.data(for: request)
let rows = (try JSONSerialization.jsonObject(with: data) as? [String: Any])?["rows"]"#,
    ),
    (
        "Dart / Flutter",
        r#"final request = await HttpClient().postUrl(Uri.parse('{o}/v1/sql'));
request.headers.set('Authorization', 'Bearer $token');
request.headers.contentType = ContentType.json;
request.write(jsonEncode({'sql': 'SELECT * FROM articles WHERE category = ?', 'params': [3]}));
final response = await request.close();
final rows = jsonDecode(await response.transform(utf8.decoder).join())['rows'];"#,
    ),
    (
        "Kotlin",
        r#"val request = HttpRequest.newBuilder(URI.create("{o}/v1/sql"))
    .header("Authorization", "Bearer " + System.getenv("NUAGE_TOKEN"))
    .header("Content-Type", "application/json")
    .POST(HttpRequest.BodyPublishers.ofString(
        """{"sql": "SELECT * FROM articles WHERE category = ?", "params": [3]}"""))
    .build()
val json = HttpClient.newHttpClient().send(request, HttpResponse.BodyHandlers.ofString()).body()"#,
    ),
    (
        "Elixir",
        r#"headers = [{~c"authorization", ~c"Bearer " ++ String.to_charlist(System.fetch_env!("NUAGE_TOKEN"))}]
body = JSON.encode!(%{sql: "SELECT * FROM articles WHERE category = ?", params: [3]})
{:ok, {_, _, answer}} =
  :httpc.request(:post, {~c"{o}/v1/sql", headers, ~c"application/json", body}, [], body_format: :binary)
rows = JSON.decode!(answer)["rows"]"#,
    ),
    (
        "curl",
        r#"curl {o}/v1/sql -H "Authorization: Bearer $NUAGE_TOKEN" \
  -d '{"sql": "INSERT INTO messages (name, text) VALUES (?, ?)", "params": ["Ada", "Bonjour"]}'"#,
    ),
];

/// One folding block per language (no JavaScript needed), the first open.
fn language_snippets(origin: &str) -> String {
    let mut out = String::new();
    for (i, (name, code)) in SNIPPETS.iter().enumerate() {
        let open = if i == 0 { " open" } else { "" };
        let _ = write!(
            out,
            r#"<details class="lang"{open}><summary>{name}</summary><pre><code>{}</code></pre></details>"#,
            esc(&code.replace("{o}", origin))
        );
    }
    out
}

/// The list of applications, as cards.
fn apps_body(state: &State, cx: &Cx, message: Option<String>) -> String {
    let l = cx.l;
    let mut body = format!("<h1>{}</h1>", cx.t("Applications"));
    if let Some(m) = message {
        body.push_str(&m);
    }
    let all = state.apps.all();
    let _ = write!(
        body,
        r#"<p class="muted">{}</p><div class="apps">"#,
        cx.tf(
            "{} application(s) on this server. Each one has its own database, tokens, encryption keys and backups.",
            &[&all.len()]
        )
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
        let fact =
            |label: &'static str, value: String| format!("<div><dt>{}</dt><dd>{}</dd></div>", cx.t(label), esc(&value));
        let _ = write!(
            body,
            r#"<a class="card app{bad}" href="{href}"><div class="row-title"><b>{name}</b>{first}{lock}</div>{desc}<dl class="mini">{facts}</dl>{backup_note}</a>"#,
            bad = if backup.last_ok { "" } else { " is-bad" },
            href = app_base(&app.name),
            name = esc(&app.name),
            first = if app.first { format!(r#"<span class="pill">{}</span>"#, cx.t("first")) } else { String::new() },
            lock = lock_badge(l, engine),
            desc = if info.description.is_empty() {
                String::new()
            } else {
                format!(r#"<small>{}</small>"#, esc(&info.description))
            },
            facts = [
                fact("Tables", tables.len().to_string()),
                fact("Rows", tables.iter().map(|t| t.rows).sum::<i64>().to_string()),
                fact("Size", bytes(l, stats.file_bytes)),
                fact("Requests", dec(l, format!("{rate:.1}/s"))),
                fact("p99", dec(l, format!("{p99:.1} ms"))),
                fact("Errors", dec(l, format!("{errors:.1}/s"))),
                fact("Branches", engine.branches().len().to_string()),
                fact("Tokens", tokens.to_string()),
                fact("Last request", ago_ms(l, app.last_request.load(std::sync::atomic::Ordering::Relaxed))),
            ]
            .concat(),
            backup_note = if backup.last_ok {
                String::new()
            } else {
                format!(
                    r#"<small class="bad">{}</small>"#,
                    esc(&cx.tf("Backup failing: {}", &[&backup.last_error.unwrap_or_default()]))
                )
            },
        );
    }
    for (name, error) in state.apps.failed() {
        let _ = write!(
            body,
            r#"<a class="card app is-bad" href="{}/settings"><div class="row-title"><b>{}</b><span class="pill bad">{}</span></div><small>{}</small></a>"#,
            app_base(&name),
            esc(&name),
            cx.t("cannot be opened"),
            esc(&error)
        );
    }
    let _ = write!(
        body,
        r#"</div><h2>{new}</h2><form method="post" action="/admin/apps" class="row">{f}
<label>{name}<input name="name" pattern="[a-z0-9_\-]{{1,63}}" required placeholder="shop"></label><label>{desc}<input name="description" placeholder="{optional}"></label><button>{create}</button></form>
<p class="muted">{note}</p>"#,
        new = cx.t("New application"),
        f = cx.field(),
        name = cx.t("Name"),
        desc = cx.t("Description"),
        optional = cx.t("optional"),
        create = cx.t("Create"),
        note = cx.t("Its database is created at once, encrypted with keys of its own, with a first token.")
    );
    body
}

fn app_overview(state: &State, cx: &Cx, app: &App, origin: &str) -> String {
    let l = cx.l;
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
    body.push_str(&connection_banner(cx, origin, app));
    body.push_str(r#"<div class="grid">"#);
    for (label, v) in [
        ("Size on disk", bytes(l, stats.file_bytes)),
        ("Tables", tables.len().to_string()),
        ("Rows", tables.iter().map(|t| t.rows).sum::<i64>().to_string()),
        ("Branches", engine.branches().len().to_string()),
        ("Requests", cx.tf("{}/s, {} in all", &[&dec(l, format!("{rate:.1}")), &app.metrics.total()])),
        ("Latency", dec(l, format!("p50 {p50:.2} ms, p99 {p99:.2} ms"))),
        ("Errors", dec(l, format!("{errors:.1}/s"))),
        ("Last connection", app.last_connection().map_or_else(|| cx.t("never").into(), |c| connection_text(cx, &c))),
        ("Tokens", (info.tokens.len() + usize::from(info.env_token)).to_string()),
        (
            "Encryption",
            match engine.encryption() {
                Some(k) => cx.tf("on ({}), keys of its own", &[&cx.t(k)]),
                None => cx.t("off: stored in clear").into(),
            },
        ),
        (
            "Last checkpoint",
            stats
                .last_checkpoint
                .and_then(|t| t.elapsed().ok())
                .map_or_else(|| cx.t("none yet").into(), |d| cx.tf("{} ago", &[&duration(l, d)])),
        ),
        (
            "Backup",
            match (&backup.target, backup.last_ok) {
                (None, _) => cx.t("off").into(),
                (Some(t), true) => cx.tf("to {}", &[t]),
                (Some(_), false) => cx.t("failing").into(),
            },
        ),
        ("Created", if app.first { cx.t("with the server").into() } else { date_ms(app.created_at) }),
    ] {
        body.push_str(&card(cx.t(label), &v));
    }
    body.push_str("</div>");
    body.push_str(&connect_help(cx, origin, app));
    body.push_str(&postgres_help(cx, origin, app, state.options.pg_port.map(|p| (p, state.options.pg_tls))));
    let _ = write!(
        body,
        r#"<p class="muted">{}</p>"#,
        cx.tf("Data is stored in <code>{}</code>.", &[&esc(&engine.dir().display().to_string())])
    );
    body
}

fn tables_body(cx: &Cx, app: &App) -> Result<String, Error> {
    let l = cx.l;
    let db = Embedded::from_engine(app.engine.clone());
    let tables = db.tables()?;
    let base = app_base(&app.name);
    let rows: Vec<Vec<Value>> = tables
        .iter()
        .map(|t| {
            vec![
                Value::Text(t.name.clone()),
                Value::I64(t.rows),
                Value::Text(bytes(l, t.bytes as u64)),
                Value::Text(t.columns.join(", ")),
            ]
        })
        .collect();
    let headers: Vec<String> = ["Table", "Rows", "Size", "Columns"].iter().map(|h| cx.t(h).to_owned()).collect();
    let mut body = table_html(l, &headers, &rows);
    body.push_str("<p>");
    for t in &tables {
        let _ = write!(
            body,
            r#"<a href="{base}/tables/{0}">{1}</a> · "#,
            esc(&t.name),
            esc(&cx.tf("Browse {}", &[&t.name]))
        );
    }
    body.push_str("</p>");
    Ok(body)
}

fn rows_body(state: &State, cx: &Cx, app: &App, name: &str, page_no: u64) -> Result<String, Error> {
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
    body.push_str(&table_html(cx.l, &headers, &rows));
    body.push_str("<p>");
    if page_no > 0 {
        let _ = write!(body, r#"<a href="?page={}">← {}</a> "#, page_no - 1, cx.t("Previous"));
    }
    let _ = write!(body, "{} ", cx.tf("Page {}", &[&(page_no + 1)]));
    if rows.len() as u64 == PAGE_ROWS {
        let _ = write!(body, r#"<a href="?page={}">{} →</a>"#, page_no + 1, cx.t("Next"));
    }
    body.push_str("</p>");
    Ok(body)
}

fn server_body(state: &State, cx: &Cx) -> String {
    let l = cx.l;
    let engine = &state.engine;
    let open = state.metrics.open_connections.load(std::sync::atomic::Ordering::Relaxed);
    let active = state.metrics.active_requests.load(std::sync::atomic::Ordering::Relaxed);
    let (rate, _) = state.metrics.rates();
    let (p50, p99) = state.metrics.percentiles();
    let all = state.apps.all();
    let disk: u64 = all.iter().map(|a| a.engine.stats().file_bytes).sum();
    let memory = crate::memory::MemoryInfo::now();
    let mut body = format!(r#"<h1>{}</h1><div class="grid">"#, cx.t("Server"));
    for (label, v) in [
        ("Version", format!("Nuage DB {}", crate::VERSION)),
        ("Uptime", duration(l, engine.uptime())),
        ("Applications", all.len().to_string()),
        ("Size on disk", bytes(l, disk)),
        ("Connections", cx.tf("{} active, {} idle", &[&active, &open.saturating_sub(active)])),
        ("Transactions open", state.open_transactions().to_string()),
        ("Requests", cx.tf("{}/s, {} in all", &[&dec(l, format!("{rate:.1}")), &state.metrics.total()])),
        ("Latency", dec(l, format!("p50 {p50:.2} ms, p99 {p99:.2} ms"))),
        (
            "Encryption",
            if all.iter().all(|a| a.engine.encryption().is_some()) {
                format!("{} ({})", crate::crypto::SCHEME, cx.t(engine.encryption().unwrap_or("key")))
            } else {
                cx.t("off for some data: set NUAGE_ENCRYPTION_KEY").into()
            },
        ),
        ("Backup target", engine.backup_status().target.unwrap_or_else(|| cx.t("off").into())),
        ("Free disk", engine.stats().disk_free_bytes.map_or_else(|| cx.t("unknown").into(), |b| bytes(l, b))),
        (
            "Memory",
            match (memory.process_bytes, memory.available_bytes, memory.total_bytes) {
                (Some(p), Some(a), Some(t)) => {
                    cx.tf("{} used by Nuage, {} free of {}", &[&bytes(l, p), &bytes(l, a), &bytes(l, t)])
                }
                _ => cx.t("unknown").into(),
            },
        ),
        (
            "Swap",
            match (memory.swap_total_bytes, memory.swap_free_bytes) {
                (Some(0), _) => cx.t("none").into(),
                (Some(t), Some(f)) => cx.tf("{} free of {}", &[&bytes(l, f), &bytes(l, t)]),
                _ => cx.t("unknown").into(),
            },
        ),
        ("Cache of each application", bytes(l, engine.options().cache_bytes as u64)),
    ] {
        body.push_str(&card(cx.t(label), &v));
    }
    let _ = write!(
        body,
        r#"</div><p class="muted">{}</p><h2>{}</h2>"#,
        cx.tf(
            "Data is stored in <code>{}</code> (the others in <code>apps/</code>).",
            &[&esc(&engine.dir().display().to_string())]
        ),
        cx.t("Events")
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
    let headers: Vec<String> = ["Time", "Application", "Level", "Event"].iter().map(|h| cx.t(h).to_owned()).collect();
    body.push_str(&table_html(l, &headers, &rows));
    body
}

fn not_found(cx: &Cx, what: &'static str) -> Response<HttpBody> {
    html(404, cx.page(cx.t("Not found"), &format!("<h1>{}</h1>", cx.t(what))))
}

fn get(state: &State, path: &str, cx: &Cx, page_no: u64, origin: &str) -> Response<HttpBody> {
    match path {
        "/admin" | "/admin/apps" => return html(200, cx.page(cx.t("Applications"), &apps_body(state, cx, None))),
        "/admin/server" => return html(200, cx.page(cx.t("Server"), &server_body(state, cx))),
        _ => {}
    }
    let Some((name, sub)) = split_app(path) else { return not_found(cx, "Not found") };
    let Some(app) = state.apps.get(&name) else {
        if sub == "/settings" && state.apps.failed().iter().any(|(n, _)| *n == name) {
            return html(200, cx.page(cx.t("Settings"), &failed_settings(cx, &name)));
        }
        return not_found(cx, "No such application");
    };
    let (tab, title, body): (&str, &str, Result<String, Error>) = match sub {
        "" => ("", "Overview", Ok(app_overview(state, cx, &app, origin))),
        "/tables" => ("/tables", "Tables", tables_body(cx, &app)),
        t if t.starts_with("/tables/") => {
            ("/tables", "Rows", rows_body(state, cx, &app, &t["/tables/".len()..], page_no))
        }
        "/query" => ("/query", "Query", Ok(query_form(cx, &app, "", None))),
        "/branches" => ("/branches", "Branches", Ok(branches_body(cx, &app, None))),
        "/storage" => ("/storage", "Storage", Ok(storage_body(cx, &app, None))),
        "/tokens" => ("/tokens", "Tokens", Ok(tokens_body(state, cx, &app, None))),
        "/settings" => ("/settings", "Settings", Ok(settings_body(cx, &app, None))),
        _ => return not_found(cx, "Not found"),
    };
    match body {
        Ok(b) => html(200, app_page(state, cx, &app, tab, cx.t(title), &b)),
        Err(e) => html(400, app_page(state, cx, &app, tab, cx.t("Error"), &note(false, &e.to_string()))),
    }
}

fn query_form(cx: &Cx, app: &App, sql: &str, extra: Option<&str>) -> String {
    format!(
        r#"<form method="post" action="{}/query">{}
<textarea name="sql" spellcheck="false" placeholder="SELECT * FROM users WHERE age >= 18 ORDER BY name LIMIT 20">{}</textarea>
<p class="row"><button>{}</button></p></form>
<p class="muted">{}</p>{}"#,
        app_base(&app.name),
        cx.field(),
        esc(sql),
        cx.t("Run"),
        cx.t(
            "SQL: SELECT with joins, subqueries, GROUP BY, UNION, functions; INSERT (ON CONFLICT, RETURNING), UPDATE, DELETE; CREATE TABLE, CREATE INDEX, ALTER TABLE ADD COLUMN, DROP. Several statements separated by <code>;</code> run in one transaction. Changes ask for a confirmation first. Websites send the same SQL to <code>POST /v1/sql</code>."
        ),
        extra.unwrap_or_default()
    )
}

fn branches_body(cx: &Cx, app: &App, message: Option<String>) -> String {
    let engine = &app.engine;
    let base = app_base(&app.name);
    let mut body = message.unwrap_or_default();
    let _ = write!(
        body,
        r#"<p class="muted">{}</p>"#,
        cx.t("A branch is an instant copy of the database: it shares the data and stores only its own changes.")
    );
    let branches = engine.branches();
    let mut out = format!(
        r#"<div class="scroll"><table><thead><tr><th>{}</th><th>{}</th><th>{}</th><th></th></tr></thead><tbody>"#,
        cx.t("Branch"),
        cx.t("From"),
        cx.t("Own data")
    );
    for b in &branches {
        let _ = write!(
            out,
            r#"<tr><td>{n}</td><td>{p}</td><td>{s}</td><td><form method="post" action="{base}/branches/{n}/reset" style="display:inline">{f}<button class="plain">{reset}</button></form> · <form method="post" action="{base}/branches/{n}/delete" style="display:inline">{f}<button class="plain">{delete}</button></form></td></tr>"#,
            n = esc(&b.name),
            p = esc(b.parent.as_deref().unwrap_or(&app.name)),
            s = bytes(cx.l, b.own_bytes),
            f = cx.field(),
            reset = cx.t("Reset"),
            delete = cx.t("Delete")
        );
    }
    if branches.is_empty() {
        let _ = write!(out, r#"<tr><td colspan="4" class="muted">{}</td></tr>"#, cx.t("No branch yet."));
    }
    out.push_str("</tbody></table></div>");
    body.push_str(&out);
    let mut options = format!(r#"<option value="">{}</option>"#, esc(&app.name));
    for b in &branches {
        let _ = write!(options, r#"<option>{}</option>"#, esc(&b.name));
    }
    let _ = write!(
        body,
        r#"<h2>{new}</h2><form method="post" action="{base}/branches" class="row">{f}
<label>{name}<input name="name" pattern="[a-z0-9_\-]{{1,63}}" required placeholder="preview-42"></label><label>{from}<select name="from">{options}</select></label><button>{create}</button></form>"#,
        new = cx.t("New branch"),
        f = cx.field(),
        name = cx.t("Name"),
        from = cx.t("From"),
        create = cx.t("Create")
    );
    body
}

fn storage_body(cx: &Cx, app: &App, message: Option<String>) -> String {
    let l = cx.l;
    let engine = &app.engine;
    let s = engine.stats();
    let b = engine.backup_status();
    let mut body = message.unwrap_or_default();
    body.push_str(r#"<div class="grid">"#);
    for (label, v) in [
        ("File", bytes(l, s.file_bytes)),
        ("Data", bytes(l, s.stored_bytes)),
        ("Free space in the file", bytes(l, s.fragmented_bytes)),
        ("Pages", cx.tf("{} of {}", &[&s.pages, &bytes(l, s.page_size)])),
        ("Write-ahead log", bytes(l, s.wal_bytes)),
        ("Free disk", s.disk_free_bytes.map_or_else(|| cx.t("unknown").into(), |b| bytes(l, b))),
        ("Cache", format!("{} / {}", bytes(l, s.cache_bytes), bytes(l, s.cache_capacity_bytes))),
        ("Cache hits", dec(l, format!("{:.1} %", s.cache_hit_rate * 100.0))),
        ("Encryption", engine.encryption().map_or_else(|| cx.t("off").into(), |_| crate::crypto::SCHEME.into())),
        ("Last commit", s.lsn.to_string()),
        (
            "Last checkpoint",
            format!(
                "{} ({})",
                s.checkpoint_lsn,
                s.last_checkpoint
                    .and_then(|t| t.elapsed().ok())
                    .map_or_else(|| cx.t("not yet").into(), |d| cx.tf("{} ago", &[&duration(l, d)]))
            ),
        ),
        ("Backup target", b.target.clone().unwrap_or_else(|| cx.t("off").into())),
        (
            "Last backup",
            match (b.last_at, b.last_ok) {
                (_, false) => cx.tf("failed: {}", &[&b.last_error.clone().unwrap_or_default()]),
                (Some(at), true) => ago_ms(l, at),
                (None, true) => cx.t("none yet").into(),
            },
        ),
    ] {
        body.push_str(&card(cx.t(label), &v));
    }
    let _ = write!(
        body,
        r#"</div><p class="muted">{}</p><h2>{}</h2><div class="row">"#,
        cx.tf("Data is stored in <code>{}</code>.", &[&esc(&engine.dir().display().to_string())]),
        cx.t("Maintenance")
    );
    for (action, label) in [
        ("checkpoint", "Checkpoint now"),
        ("backup", "Back up now"),
        ("integrity", "Check integrity"),
        ("compact", "Compact"),
    ] {
        let _ = write!(
            body,
            r#"<form method="post" action="{}/storage/{action}">{}<button>{}</button></form>"#,
            app_base(&app.name),
            cx.field(),
            cx.t(label)
        );
    }
    body.push_str("</div>");
    body
}

fn tokens_body(state: &State, cx: &Cx, app: &App, message: Option<String>) -> String {
    let base = app_base(&app.name);
    let info = app.info(state.options.token.is_some());
    let mut body = message.unwrap_or_default();
    let _ = write!(
        body,
        r#"<p class="muted">{}</p>"#,
        cx.t("A token opens this application's database, and only it. The server keeps a SHA-256 digest of each token, never the token: a lost token cannot be shown again, make a new one.")
    );
    let mut out = format!(
        r#"<div class="scroll"><table><thead><tr><th>{}</th><th>{}</th><th>{}</th><th></th></tr></thead><tbody>"#,
        cx.t("Token"),
        cx.t("Rights"),
        cx.t("Created")
    );
    if info.env_token {
        let _ = write!(
            out,
            r#"<tr><td><code>NUAGE_TOKEN</code></td><td>{}</td><td class="muted">{}</td><td class="muted">{}</td></tr>"#,
            cx.t("read and write"),
            cx.t("set on the server"),
            cx.t("change it in the environment")
        );
    }
    for tok in &info.tokens {
        let _ = write!(
            out,
            r#"<tr><td><code>nuage_{id}…</code></td><td>{rights}</td><td>{at}</td><td><form method="post" action="{base}/tokens/{id}/revoke" style="display:inline">{f}<button class="plain">{revoke}</button></form></td></tr>"#,
            id = esc(&tok.id),
            rights = if tok.read_only {
                format!(r#"<span class="pill">{}</span>"#, cx.t("read only"))
            } else {
                cx.t("read and write").into()
            },
            at = date_ms(tok.created_at),
            f = cx.field(),
            revoke = cx.t("Revoke")
        );
    }
    if info.tokens.is_empty() && !info.env_token {
        let _ = write!(
            out,
            r#"<tr><td colspan="4" class="muted">{}</td></tr>"#,
            cx.t("No token: no application can connect.")
        );
    }
    out.push_str("</tbody></table></div>");
    body.push_str(&out);
    let _ = write!(
        body,
        r#"<div class="row" style="margin-top:.75rem"><form method="post" action="{base}/tokens">{f}<input type="hidden" name="scope" value="write"><button>{rw}</button></form>
<form method="post" action="{base}/tokens">{f}<input type="hidden" name="scope" value="read"><button>{ro}</button></form></div>
<p class="muted">{public}</p>"#,
        f = cx.field(),
        rw = cx.t("New token (read and write)"),
        ro = cx.t("New read-only token"),
        public = cx.t("A token in a web page is public: anyone can read it in the page's code. Give pages a <b>read-only</b> token, and keep read-and-write tokens on servers.")
    );
    body
}

fn settings_body(cx: &Cx, app: &App, message: Option<String>) -> String {
    let base = app_base(&app.name);
    let mut body = message.unwrap_or_default();
    let description = app.description.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let _ = write!(
        body,
        r#"<h2>{title}</h2><form method="post" action="{base}/description" class="row">{f}
<label>{title}<input name="description" value="{d}" placeholder="{what}"></label><button>{save}</button></form>"#,
        title = cx.t("Description"),
        f = cx.field(),
        d = esc(&description),
        what = cx.t("what this application is"),
        save = cx.t("Save")
    );
    let _ = write!(
        body,
        r#"<h2>{title}</h2><p class="muted">{help}</p>
<form method="post" action="{base}/origins">{f}<textarea name="origins" spellcheck="false" placeholder="https://site.fr" style="min-height:5rem">{o}</textarea><p class="row"><button>{save}</button></p></form>"#,
        title = cx.t("Websites"),
        help = cx.t("The pages of these websites may call this application from the browser (CORS): one origin per line, scheme and host (<code>https://site.fr</code>, <code>http://localhost:3000</code>), or <code>*</code> for any. Servers, and apps outside a browser, need none."),
        f = cx.field(),
        o = esc(&app.origins().join("\n")),
        save = cx.t("Save the websites")
    );
    if app.first {
        let _ = write!(
            body,
            r#"<h2>{}</h2><p class="muted">{}</p>"#,
            cx.t("Delete"),
            cx.t("The first application is the server's own database: it cannot be deleted.")
        );
    } else {
        body.push_str(&delete_form(cx, &app.name));
    }
    body
}

fn delete_form(cx: &Cx, name: &str) -> String {
    let n = esc(name);
    format!(
        r#"<h2>{title}</h2><p>{warn}</p>
<form method="post" action="{base}/delete" class="row">{f}
<label>{typ}<input name="confirm" required autocomplete="off"></label><button class="danger">{del}</button></form>"#,
        title = cx.t("Delete"),
        warn = cx.tf("Deleting <b>{}</b> removes its tokens, its data and its backups. It cannot be undone.", &[&n]),
        base = app_base(name),
        f = cx.field(),
        typ = cx.tf("Type <code>{}</code> to confirm", &[&n]),
        del = cx.tf("Delete {}", &[&n])
    )
}

fn failed_settings(cx: &Cx, name: &str) -> String {
    format!(
        r#"<h1>{}</h1><p class="msg bad">{}</p>{}"#,
        esc(name),
        cx.t("This application could not be opened at startup (see the server's events)."),
        delete_form(cx, name)
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
    cx: Cx,
    origin: String,
) -> Response<HttpBody> {
    if path == "/admin/logout" {
        state.admin.remove(session);
        let mut res = redirect("/admin/login");
        set_cookie(&mut res, cx.secure, SESSION_COOKIE, "", 0);
        return res;
    }
    let state2 = state.clone();
    let path2 = path.to_owned();
    let cx2 = cx.clone();
    tokio::task::spawn_blocking(move || post_now(&state2, &path2, &form, &cx2, &origin))
        .await
        .unwrap_or_else(|e| html(500, cx.page(cx.t("Error"), &format!("<p>{}</p>", esc(&e.to_string())))))
}

fn post_now(state: &State, path: &str, form: &HashMap<String, String>, cx: &Cx, origin: &str) -> Response<HttpBody> {
    let field = |k: &str| form.get(k).map(String::as_str).unwrap_or_default().to_owned();
    if path == "/admin/apps" {
        let msg = match state.apps.create(field("name").trim(), &field("description")) {
            Ok((app, token)) => format!(
                "{}{}",
                note(true, &cx.tf("Application {} created, with its own encrypted database.", &[&app.name])),
                token_reveal(cx, origin, &app.name, &token)
            ),
            Err(e) => note(false, &e.to_string()),
        };
        return html(200, cx.page(cx.t("Applications"), &apps_body(state, cx, Some(msg))));
    }
    let Some((name, sub)) = split_app(path) else { return not_found(cx, "Not found") };
    if sub == "/delete" {
        if field("confirm").trim() != name {
            let msg = note(false, &cx.tf("Type {} to confirm: nothing was deleted.", &[&name]));
            return match state.apps.get(&name) {
                Some(app) => html(
                    400,
                    app_page(state, cx, &app, "/settings", cx.t("Settings"), &settings_body(cx, &app, Some(msg))),
                ),
                None => html(400, cx.page(cx.t("Settings"), &format!("{msg}{}", failed_settings(cx, &name)))),
            };
        }
        let msg = match state.apps.delete(&name) {
            Ok(true) => note(true, &cx.tf("Application {} deleted, with its data and backups.", &[&name])),
            Ok(false) => note(false, &cx.tf("There is no application {}.", &[&name])),
            Err(e) => note(false, &e.to_string()),
        };
        return html(200, cx.page(cx.t("Applications"), &apps_body(state, cx, Some(msg))));
    }
    let Some(app) = state.apps.get(&name) else { return not_found(cx, "No such application") };
    let engine = &app.engine;
    let (sql, confirmed, branch, from) = (field("sql"), field("confirm") == "yes", field("name"), field("from"));
    match sub {
        "/query" => {
            let result = run_console_sql(state, cx, &app, &sql, confirmed);
            let (status, extra) = match result {
                Ok(extra) => (200, extra),
                Err(e) => (400, note(false, &e.to_string())),
            };
            html(status, app_page(state, cx, &app, "/query", cx.t("Query"), &query_form(cx, &app, &sql, Some(&extra))))
        }
        "/branches" => {
            let from = (!from.is_empty()).then_some(from.as_str());
            let msg = match engine.create_branch(&branch, from) {
                Ok(b) => note(true, &cx.tf("Branch {} created.", &[&b.name])),
                Err(e) => note(false, &e.to_string()),
            };
            html(200, app_page(state, cx, &app, "/branches", cx.t("Branches"), &branches_body(cx, &app, Some(msg))))
        }
        p if p.starts_with("/branches/") => {
            let rest = &p["/branches/".len()..];
            let msg = match rest.rsplit_once('/') {
                Some((n, "delete")) => match engine.delete_branch(n) {
                    Ok(true) => note(true, &cx.tf("Branch {} deleted.", &[&n])),
                    Ok(false) => note(false, &cx.tf("There is no branch {}.", &[&n])),
                    Err(e) => note(false, &e.to_string()),
                },
                Some((n, "reset")) => match engine.reset_branch(n) {
                    Ok(true) => note(true, &cx.tf("Branch {} reset to its parent.", &[&n])),
                    Ok(false) => note(false, &cx.tf("There is no branch {}.", &[&n])),
                    Err(e) => note(false, &e.to_string()),
                },
                _ => note(false, cx.t("Unknown action.")),
            };
            html(200, app_page(state, cx, &app, "/branches", cx.t("Branches"), &branches_body(cx, &app, Some(msg))))
        }
        p if p.starts_with("/storage/") => {
            let msg = match &p["/storage/".len()..] {
                "checkpoint" => engine.checkpoint().map(|lsn| note(true, &cx.tf("Checkpoint at {}.", &[&lsn]))),
                "backup" => engine.backup_now(true).map(|()| note(true, cx.t("Backup done."))),
                "integrity" => engine.check_integrity().map(|ok| {
                    note(
                        ok,
                        if ok {
                            cx.t("Every page checksum is right.")
                        } else {
                            cx.t("Damaged pages were found and repaired.")
                        },
                    )
                }),
                "compact" => engine.compact().map(|done| {
                    note(done, if done { cx.t("Compaction done.") } else { cx.t("Busy: compaction will run later.") })
                }),
                _ => Ok(note(false, cx.t("Unknown action."))),
            }
            .unwrap_or_else(|e| note(false, &e.to_string()));
            html(200, app_page(state, cx, &app, "/storage", cx.t("Storage"), &storage_body(cx, &app, Some(msg))))
        }
        "/tokens" => {
            let msg = match state.apps.new_token(&app, field("scope") == "read") {
                Ok(token) => token_reveal(cx, origin, &app.name, &token),
                Err(e) => note(false, &e.to_string()),
            };
            html(200, app_page(state, cx, &app, "/tokens", cx.t("Tokens"), &tokens_body(state, cx, &app, Some(msg))))
        }
        p if p.starts_with("/tokens/") && p.ends_with("/revoke") => {
            let id = &p["/tokens/".len()..p.len() - "/revoke".len()];
            let msg = match state.apps.revoke_token(&app, id) {
                Ok(true) => note(true, &cx.tf("Token nuage_{}… revoked: it opens nothing any more.", &[&id])),
                Ok(false) => note(false, cx.t("There is no such token.")),
                Err(e) => note(false, &e.to_string()),
            };
            html(200, app_page(state, cx, &app, "/tokens", cx.t("Tokens"), &tokens_body(state, cx, &app, Some(msg))))
        }
        "/origins" => {
            let msg = match state.apps.set_origins(&app, &field("origins")) {
                Ok(o) if o.is_empty() => note(true, cx.t("No website: pages cannot call this application.")),
                Ok(o) => note(true, &cx.tf("Pages of {} may call this application.", &[&o.join(", ")])),
                Err(e) => note(false, &e.to_string()),
            };
            html(200, app_page(state, cx, &app, "/settings", cx.t("Settings"), &settings_body(cx, &app, Some(msg))))
        }
        "/description" => {
            let msg = match state.apps.set_description(&app, &field("description")) {
                Ok(()) => note(true, cx.t("Description saved.")),
                Err(e) => note(false, &e.to_string()),
            };
            html(200, app_page(state, cx, &app, "/settings", cx.t("Settings"), &settings_body(cx, &app, Some(msg))))
        }
        _ => not_found(cx, "Not found"),
    }
}

/// Runs the console's SQL: reads at once; changes are first run without
/// being kept (a dry run, to say what they would change), then on confirmation.
fn run_console_sql(state: &State, cx: &Cx, app: &App, sql: &str, confirmed: bool) -> memoire_orm::Result<String> {
    let script = crate::sql::Script::parse(sql)?;
    let writes = script.writes();
    let batch = [(script, Vec::new())];
    if writes && !confirmed {
        let dry = crate::sql::run(&app.engine, MAIN, &state.schemas, &batch, false)?;
        let n: u64 = dry.iter().flatten().map(|r| r.rows_affected).sum();
        return Ok(format!(
            r#"<div class="msg"><p><b>{}</b></p>
<form method="post" action="{base}/query">{}<input type="hidden" name="sql" value="{}"><input type="hidden" name="confirm" value="yes"><button class="danger">{}</button></form></div>"#,
            esc(&cx.tf("This changes data of {}: {} row(s) would be affected.", &[&app.name, &n])),
            cx.field(),
            esc(sql),
            cx.t("Confirm and run"),
            base = app_base(&app.name),
        ));
    }
    let results = crate::sql::run(&app.engine, MAIN, &state.schemas, &batch, true)?;
    let mut out = String::new();
    for r in results.iter().flatten() {
        if r.returns_rows {
            let _ = write!(
                out,
                "<p class=\"muted\">{}</p>{}",
                cx.tf("{} row(s).", &[&r.rows.len()]),
                table_html(cx.l, &r.columns, &r.rows)
            );
        } else {
            out.push_str(&note(true, &cx.tf("Done: {} row(s) affected.", &[&r.rows_affected])));
        }
    }
    Ok(out)
}
