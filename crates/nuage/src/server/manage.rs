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
        json_response(self.status, &json!({ "error": { "kind": self.kind, "message": self.message } }))
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
    })
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
                return Err(error(409, "conflict", &format!("there is already an application named `{name}`")));
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
        (_, [] | [_] | [_, "tokens"] | [_, "tokens", _]) => Err(error(405, "method_not_allowed", "method not allowed")),
        _ => Err(error(404, "not_found", "no such route: see /v1/openapi.json")),
    }
}
