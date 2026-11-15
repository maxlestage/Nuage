//! `/api/apps`: the management API. Creates applications and their tokens
//! with one HTTP request, from a script, a CI job or `nuage apps create`,
//! instead of the console's form.
//!
//! | Method | Path | What |
//! |---|---|---|
//! | `GET` | `/api/apps` | every application |
//! | `POST` | `/api/apps` | a new application `{"name", "description"?}`, with its first token |
//! | `GET` | `/api/apps/{name}` | one application |
//! | `POST` | `/api/apps/{name}/tokens` | a new token `{"read_only"?}` |
//! | `DELETE` | `/api/apps/{name}/tokens/{id}` | revokes a token |
//! | `POST` | `/api/apps/{name}/memory` | the memory of one application: `{"cache_mb", "kv_mb"}`, a number of MiB each, or `"auto"` |
//! | `GET` | `/api/settings` | the settings: each with its value, where it comes from, and what the environment says |
//! | `POST` | `/api/settings` | saves settings `{"memory.cache_mb": 512, "status.public": true}`, in force at once; `null` gives one back to the environment's value; one that does not fit and nothing is saved |
//! | `DELETE` | `/api/settings/{key}` | gives a setting back to the environment's value, or the default |
//!
//! Security: the requests carry `Authorization: Bearer <NUAGE_ADMIN_TOKEN>`,
//! a secret of its own (32 characters at least, checked at startup), compared
//! in constant time. Without `NUAGE_ADMIN_TOKEN`, the API is off and answers
//! 403. An application's token never opens it: a token opens its own
//! database, and nothing else. Deleting an application, which deletes its
//! data and backups, stays in the console, behind a typed confirmation.
//! A token is in the answer that makes it, once: the server keeps only its
//! digest.

use std::sync::Arc;

use hyper::body::Incoming;
use hyper::header::{self, HeaderMap};
use hyper::{Method, Request, Response};
use memoire_orm::Error;
use serde_json::{Value as Json, json};

use super::apps::{App, AppInfo, TokenInfo};
use super::{HttpBody, State, json_response, read_json, same_secret};

/// The shortest `NUAGE_ADMIN_TOKEN` the server accepts: a guess must be out
/// of reach, since this token makes applications.
pub const MIN_ADMIN_TOKEN: usize = 32;

const OFF: &str = "the management API is off: set NUAGE_ADMIN_TOKEN on the server (`nuage genkey` makes one)";
const REFUSED: &str =
    "the management API needs `Authorization: Bearer <NUAGE_ADMIN_TOKEN>` (an application's token does not open it)";

/// A refused request: its status, and the `{"error": {"kind", "message"}}`
/// the client reads.
struct Refus {
    status: u16,
    kind: &'static str,
    message: String,
}

fn error(status: u16, kind: &'static str, message: &str) -> Refus {
    Refus { status, kind, message: message.to_owned() }
}

impl Refus {
    fn response(&self) -> Response<HttpBody> {
        let mut error = json!({ "kind": self.kind, "message": self.message });
        if self.kind == "unique_violation" {
            error["table"] = json!("applications");
            error["column"] = json!("name");
        }
        json_response(self.status, &json!({ "error": error }))
    }
}

/// Does the request carry the management token?
fn allowed(state: &State, headers: &HeaderMap) -> Result<(), Refus> {
    let Some(expected) = state.options.admin_token.as_deref() else {
        return Err(error(403, "forbidden", OFF));
    };
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or_default();
    if given.is_empty() || !same_secret(given.as_bytes(), expected.as_bytes()) {
        return Err(error(401, "unauthorized", REFUSED));
    }
    Ok(())
}

fn token_json(t: &TokenInfo) -> Json {
    json!({ "id": t.id, "created_at": t.created_at, "read_only": t.read_only })
}

fn app_json(info: &AppInfo) -> Json {
    json!({
        "name": info.name,
        "description": info.description,
        "first": info.first,
        "created_at": info.created_at,
        "tokens": info.tokens.iter().map(token_json).collect::<Vec<_>>(),
        "env_token": info.env_token,
        "origins": info.origins,
        "memory": { "cache_mb": amount_json(info.cache_mb), "kv_mb": amount_json(info.kv_mb) },
    })
}

/// An amount of memory an owner may set: a number of MiB, or `"auto"`.
fn amount_json(mib: Option<u64>) -> Json {
    mib.map_or(json!("auto"), |m| json!(m))
}

/// An amount of memory in a JSON body: `Some(None)` for `"auto"` or `null`,
/// `Some(Some(bytes))` for a number of MiB or a text with its unit (`"2 GiB"`),
/// `None` when the body does not name it (it stays as it is).
fn amount_of(body: &Json, key: &str) -> Result<Option<Option<usize>>, Refus> {
    let mib = match body.get(key) {
        None => return Ok(None),
        Some(Json::Null) => return Ok(Some(None)),
        Some(Json::String(s)) if s.trim().eq_ignore_ascii_case("auto") => return Ok(Some(None)),
        Some(Json::Number(n)) => n.as_u64(),
        Some(Json::String(s)) => super::settings::parse_mib(s),
        Some(_) => None,
    };
    match mib {
        Some(m) if m <= 1 << 24 => Ok(Some(Some((m as usize) << 20))),
        _ => Err(error(400, "validation", &format!("`{key}` is a number of MiB, an amount (\"2 GiB\"), or \"auto\""))),
    }
}

/// The settings, as `GET /api/settings` answers them. Never a secret.
pub(super) fn settings_json(state: &State) -> Json {
    let settings: Vec<Json> = super::settings::DEFS
        .iter()
        .map(|d| {
            let (value, source) = state.settings.value(d.key);
            json!({
                "key": d.key,
                "variable": d.var,
                "value": value,
                "source": source.name(),
                "saved": state.settings.saved(d.key),
                "environment": state.settings.environment(d.key),
                "default": super::settings::default_text(&state.options, d.key),
            })
        })
        .collect();
    let memory = crate::memory::MemoryInfo::now();
    let applications: Vec<Json> = state
        .apps
        .all()
        .iter()
        .map(|a| {
            let i = a.info(false);
            json!({
                "name": i.name,
                "cache_mb": amount_json(i.cache_mb),
                "kv_mb": amount_json(i.kv_mb),
                "cache_bytes": a.engine.cache_used(),
                "cache_capacity_bytes": a.engine.cache_capacity(),
            })
        })
        .collect();
    let startup: Vec<Json> =
        state.options.startup.iter().map(|s| json!({ "variable": s.var, "value": s.value })).collect();
    json!({
        "read_only": state.settings.read_only(),
        "ignored": state.settings.ignored().into_iter().collect::<std::collections::BTreeMap<_, _>>(),
        // Amounts of applications kept aside while the environment rules: POST
        // {"cache_mb": "auto", "kv_mb": "auto"} to /api/apps/{name}/memory removes them.
        "ignored_applications": state
            .apps
            .all()
            .iter()
            .filter(|a| a.kept_amounts() != (0, 0))
            .map(|a| {
                let (cache, kv) = a.kept_amounts();
                let mib = |b: usize| if b == 0 { json!("auto") } else { json!(b >> 20) };
                (a.name.clone(), json!({ "cache_mb": mib(cache), "kv_mb": mib(kv) }))
            })
            .collect::<serde_json::Map<String, Json>>(),
        "settings": settings,
        "memory": {
            "total_bytes": memory.total_bytes,
            "detected_bytes": crate::memory::detected().total_bytes,
            "used_bytes": memory.used_bytes,
            "cache_budget_bytes": state.apps.cache_budget(),
            "cache_ceiling_bytes": state.apps.cache_ceiling(),
        },
        "applications": applications,
        "startup": startup,
        "settings_file": state.options.settings_file,
    })
}

/// The answer to a change, with a `warning` when it is saved and in force
/// but did not reach the backup yet.
fn with_warning(mut answer: Json, unshipped: Option<String>) -> Json {
    if let Some(why) = unshipped {
        answer["warning"] = json!(format!(
            "saved and in force, but not yet in the backup ({why}): a restart of the server now would bring the earlier value back"
        ));
    }
    answer
}

/// A setting's value in a JSON body, as the text the settings keep.
fn setting_text(key: &str, value: &Json) -> Result<Option<String>, Refus> {
    match value {
        Json::Null => Ok(None),
        Json::String(s) => Ok(Some(s.clone())),
        Json::Number(n) => Ok(Some(n.to_string())),
        Json::Bool(b) => Ok(Some(b.to_string())),
        _ => Err(error(400, "validation", &format!("`{key}`: a number, a text, true or false, or null"))),
    }
}

/// A refused change of the settings: 400, or 403 when the server takes its
/// settings from its environment only. Said in the language the client asks
/// for (`Accept-Language`: the editor sends its own), else in English.
fn settings_refusal(r: &super::settings::Refusal, l: super::i18n::Lang) -> Refus {
    let message = r.said(l);
    if r.forbidden { error(403, "forbidden", &message) } else { error(400, "validation", &message) }
}

async fn settings_answer(state: &Arc<State>, req: Request<Incoming>, path: &str) -> Result<Response<HttpBody>, Refus> {
    let method = req.method().clone();
    let l = super::i18n::Lang::of(req.headers());
    let rest = path.trim_end_matches('/').trim_start_matches("/api/settings");
    let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    match (&method, segments.as_slice()) {
        (&Method::GET, []) => Ok(json_response(200, &settings_json(state))),
        (&Method::POST, []) => {
            let body = read_json(state, req).await.map_err(|e| bad_request(&e))?;
            let Some(object) = body.as_object() else {
                return Err(error(
                    400,
                    "validation",
                    "send a JSON object: {\"memory.cache_mb\": 512, \"status.public\": true}",
                ));
            };
            let mut changes = Vec::new();
            for (key, value) in object {
                changes.push((key.clone(), setting_text(key, value)?));
            }
            let s = state.clone();
            let saved = tokio::task::spawn_blocking(move || {
                let changes: Vec<(&str, Option<&str>)> =
                    changes.iter().map(|(k, v)| (k.as_str(), v.as_deref())).collect();
                s.save_settings(&changes)
            })
            .await
            .map_err(|e| error(500, "internal", &e.to_string()))?;
            let unshipped = saved.map_err(|e| settings_refusal(&e, l))?;
            Ok(json_response(200, &with_warning(settings_json(state), unshipped)))
        }
        (&Method::DELETE, [key]) => {
            let key = (*key).to_owned();
            let s = state.clone();
            let saved = tokio::task::spawn_blocking(move || s.save_settings(&[(key.as_str(), None)]))
                .await
                .map_err(|e| error(500, "internal", &e.to_string()))?;
            let unshipped = saved.map_err(|e| settings_refusal(&e, l))?;
            Ok(json_response(200, &with_warning(settings_json(state), unshipped)))
        }
        (_, [] | [_]) => Err(error(405, "method_not_allowed", "method not allowed")),
        _ => Err(error(404, "not_found", "no such route: see /v1/openapi.json")),
    }
}

fn info(state: &State, app: &App) -> AppInfo {
    app.info(app.first && state.options.token.is_some())
}

/// What an application needs to connect: `NUAGE_URL` and `NUAGE_TOKEN`.
fn env_json(origin: &str, token: &str) -> Json {
    json!({ "NUAGE_URL": origin, "NUAGE_TOKEN": token })
}

/// `https://host` of the request (behind a proxy, `X-Forwarded-Proto`).
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

fn bad_request(e: &Error) -> Refus {
    let message = match e {
        Error::Validation(m) | Error::Conversion(m) => m.clone(),
        other => other.to_string(),
    };
    error(400, "validation", &message)
}

/// A string field of a JSON body: absent and `null` are `""`.
fn text(body: &Json, key: &str) -> Result<String, Refus> {
    match &body[key] {
        Json::Null => Ok(String::new()),
        Json::String(s) => Ok(s.clone()),
        _ => Err(error(400, "validation", &format!("`{key}` is a string"))),
    }
}

/// Runs `f`, which writes the applications' list on disk, off the async
/// threads.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> memoire_orm::Result<T> + Send + 'static) -> Result<T, Refus> {
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(bad_request(&e)),
        Err(e) => Err(error(500, "internal", &e.to_string())),
    }
}

pub(super) async fn route(state: &Arc<State>, req: Request<Incoming>, path: &str) -> Response<HttpBody> {
    if let Err(refused) = allowed(state, req.headers()) {
        return refused.response();
    }
    if path == "/api/settings" || path.starts_with("/api/settings/") {
        return settings_answer(state, req, path).await.unwrap_or_else(|refused| refused.response());
    }
    answer(state, req, path).await.unwrap_or_else(|refused| refused.response())
}

async fn answer(state: &Arc<State>, req: Request<Incoming>, path: &str) -> Result<Response<HttpBody>, Refus> {
    let method = req.method().clone();
    let origin = origin_of(req.headers());
    let rest = path.trim_end_matches('/').trim_start_matches("/api/apps");
    let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    let app_named = |name: &str| {
        state.apps.get(name).ok_or_else(|| error(404, "not_found", &format!("there is no application `{name}`")))
    };
    match (&method, segments.as_slice()) {
        (&Method::GET, []) => {
            let apps: Vec<Json> = state.apps.all().iter().map(|a| app_json(&info(state, a))).collect();
            Ok(json_response(200, &json!({ "applications": apps })))
        }
        (&Method::POST, []) => {
            let body = read_json(state, req).await.map_err(|e| bad_request(&e))?;
            if !body.is_object() {
                return Err(error(
                    400,
                    "validation",
                    "send a JSON object: {\"name\": \"shop\", \"description\": \"…\"}",
                ));
            }
            let name = text(&body, "name")?.trim().to_owned();
            let description = text(&body, "description")?;
            if name.is_empty() {
                return Err(error(
                    400,
                    "validation",
                    "`name` is required: 1 to 63 lowercase letters, digits, `-` and `_`",
                ));
            }
            if state.apps.get(&name).is_some() {
                // Memoire reads `unique_violation` as `Error::UniqueViolation` (`conflict`
                // would be a transaction to retry).
                return Err(Refus {
                    status: 409,
                    kind: "unique_violation",
                    message: format!("there is already an application named `{name}`"),
                });
            }
            let s = state.clone();
            let (app, token) = blocking(move || s.apps.create(&name, &description)).await?;
            Ok(json_response(
                201,
                &json!({
                    "application": app_json(&info(state, &app)),
                    "token": token,
                    "env": env_json(&origin, &token),
                    "message": format!(
                        "Application `{}` created, with its own encrypted database. Copy its token now: it is not shown again.",
                        app.name
                    ),
                }),
            ))
        }
        (&Method::GET, [name]) => {
            let app = app_named(name)?;
            Ok(json_response(200, &app_json(&info(state, &app))))
        }
        (&Method::POST, [name, "tokens"]) => {
            let app = app_named(name)?;
            let body = read_json(state, req).await.map_err(|e| bad_request(&e))?;
            let read_only = match &body["read_only"] {
                Json::Null => false,
                Json::Bool(b) => *b,
                _ => return Err(error(400, "validation", "`read_only` is true or false")),
            };
            let s = state.clone();
            let a = app.clone();
            let token = blocking(move || s.apps.new_token(&a, read_only)).await?;
            Ok(json_response(
                201,
                &json!({
                    "application": app.name,
                    "token": token,
                    "read_only": read_only,
                    "env": env_json(&origin, &token),
                }),
            ))
        }
        (&Method::DELETE, [name, "tokens", id]) => {
            let app = app_named(name)?;
            let id = (*id).to_owned();
            let s = state.clone();
            let a = app.clone();
            if blocking(move || s.apps.revoke_token(&a, &id)).await? {
                Ok(json_response(200, &json!({ "revoked": true })))
            } else {
                Err(error(404, "not_found", &format!("application `{}` has no token with this id", app.name)))
            }
        }
        (&Method::POST, [name, "memory"]) => {
            let app = app_named(name)?;
            let l = super::i18n::Lang::of(req.headers());
            let body = read_json(state, req).await.map_err(|e| bad_request(&e))?;
            if !body.is_object() {
                return Err(error(400, "validation", "send a JSON object: {\"cache_mb\": 256, \"kv_mb\": \"auto\"}"));
            }
            // What the body does not name stays as it is.
            let cache = amount_of(&body, "cache_mb")?.unwrap_or(Some(app.cache_reserved()).filter(|b| *b > 0));
            let kv = amount_of(&body, "kv_mb")?.unwrap_or(Some(app.kv_fixed()).filter(|b| *b > 0));
            let s = state.clone();
            let a = app.clone();
            let (_, unshipped) = tokio::task::spawn_blocking(move || s.set_app_memory(&a, cache, kv))
                .await
                .map_err(|e| error(500, "internal", &e.to_string()))?
                .map_err(|r| settings_refusal(&r, l))?;
            Ok(json_response(200, &with_warning(app_json(&info(state, &app)), unshipped)))
        }
        (_, [] | [_] | [_, "tokens"] | [_, "tokens", _] | [_, "memory"]) => {
            Err(error(405, "method_not_allowed", "method not allowed"))
        }
        _ => Err(error(404, "not_found", "no such route: see /v1/openapi.json")),
    }
}
