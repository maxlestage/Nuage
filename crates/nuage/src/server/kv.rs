//! `/v1/kv`: the key-value store of an application, in memory (see
//! [`crate::kv`]): a cache, counters, locks.
//!
//! | Request | What |
//! |---|---|
//! | `GET /v1/kv/<key>` | the value, as it was written (`404` when there is none); `nuage-ttl-ms` says how long it still lives |
//! | `PUT /v1/kv/<key>` (`?ttl=60`, `&nx`, `&xx`, `&keepttl`) | writes the body under the key: `{"set": true}` |
//! | `DELETE /v1/kv/<key>` | deletes it: `{"deleted": 1}` |
//! | `POST /v1/kv/<key>?incr` (`=5`, `&ttl=60`) | adds to a counter: `{"value": 6}`; `ttl` is the expiry of a counter it creates |
//! | `POST /v1/kv/<key>?expire=60`, `?persist` | changes its expiry: `{"set": true}` |
//! | `GET /v1/kv` (`?prefix=`, `&limit=`) | the keys, and the figures of the store |
//! | `POST /v1/kv` | commands as JSON: `["SET", "k", "v", "EX", 60]` answers `{"result": "OK"}`, a list of commands answers `{"results": [...]}` in one round trip |
//! | `DELETE /v1/kv` | deletes every key |
//!
//! A read-only token reads; writing takes a read-write token. A request
//! takes 10,000 commands at most, and its answers 16 MiB
//! ([`MAX_ANSWER`](crate::kv::MAX_ANSWER)): past that, the commands left are
//! not run and answer an error.

use std::sync::Arc;
use std::time::Duration;

use hyper::body::Incoming;
use hyper::header::{self, HeaderValue};
use hyper::{Method, Request, Response};
use memoire_orm::{Error, Result};
use serde_json::{Value as Json, json};

use super::limits::{MAX_KV_COMMANDS, too_large, unblocked};
use super::{HttpBody, READ_ONLY, State, apps::App, forbidden, full, json_body, json_response, read_body};
use crate::kv::{KvError, MAX_ANSWER, Reply, SetOptions, Ttl};

fn refusal(status: u16, kind: &str, message: impl Into<String>) -> Response<HttpBody> {
    json_response(status, &json!({ "error": { "kind": kind, "message": message.into() } }))
}

fn refused(e: &KvError) -> Response<HttpBody> {
    let status = if *e == KvError::TooLarge { 413 } else { 400 };
    refusal(status, "validation", e.to_string())
}

/// `%XX` decoding of a path or a query value.
fn decode(text: &str, plus_is_space: bool) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
        match bytes[i] {
            b'%' => match (bytes.get(i + 1).copied().and_then(hex), bytes.get(i + 2).copied().and_then(hex)) {
                (Some(a), Some(b)) => {
                    out.push(a << 4 | b);
                    i += 3;
                    continue;
                }
                _ => out.push(b'%'),
            },
            b'+' if plus_is_space => out.push(b' '),
            b => out.push(b),
        }
        i += 1;
    }
    out
}

/// The parameter `name` of a query: `Some("")` when it has no value.
fn param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query.split('&').find_map(|kv| match kv.split_once('=') {
        Some((k, v)) if k == name => Some(v),
        None if kv == name => Some(""),
        _ => None,
    })
}

/// A `ttl` in seconds (`60`, `0.5`): at least a millisecond.
fn seconds(text: &str) -> Option<Duration> {
    let s: f64 = text.parse().ok()?;
    (s.is_finite() && s > 0.0 && s < 1e10).then(|| Duration::from_millis(((s * 1000.0) as u64).max(1)))
}

fn ttl_json(ttl: Ttl) -> Json {
    match ttl {
        Ttl::Millis(ms) => json!(ms),
        _ => Json::Null,
    }
}

fn reply_json(reply: Reply) -> Json {
    match reply {
        Reply::Ok => json!("OK"),
        Reply::Status(s) => json!(s),
        Reply::Nil => Json::Null,
        Reply::Int(n) => json!(n),
        Reply::Bulk(b) => json!(String::from_utf8_lossy(&b)),
        Reply::Array(items) => Json::Array(items.into_iter().map(reply_json).collect()),
        Reply::Map(pairs) => Json::Array(pairs.into_iter().flat_map(|(k, v)| [reply_json(k), reply_json(v)]).collect()),
        Reply::Error(e) => json!({ "error": e }),
    }
}

/// An answer to commands, made as JSON from their replies: a large one
/// (values of megabytes) off the runtime.
async fn answer(size: usize, make: impl FnOnce() -> Json + Send + 'static) -> Result<Response<HttpBody>> {
    if size < 256 << 10 {
        return Ok(json_response(200, &make()));
    }
    unblocked(move || json_response(200, &make())).await
}

/// A command as JSON: a list of texts and numbers.
fn command_of(j: &Json) -> Option<Vec<Vec<u8>>> {
    j.as_array()?
        .iter()
        .map(|a| match a {
            Json::String(s) => Some(s.as_bytes().to_vec()),
            Json::Number(n) => Some(n.to_string().into_bytes()),
            Json::Bool(b) => Some(b.to_string().into_bytes()),
            _ => None,
        })
        .collect()
}

const BAD_COMMAND: &str = "send a command as a list, [\"SET\", \"key\", \"value\", \"EX\", 60], or a list of commands \
                           (or {\"commands\": [...]}): each one a list of texts and numbers";

/// `/v1/kv` and `/v1/kv/<key>`, for the application `app`.
pub(super) async fn route(
    state: &Arc<State>,
    app: &Arc<App>,
    read_only: bool,
    req: Request<Incoming>,
) -> Result<Response<HttpBody>> {
    let kv = &app.kv;
    let method = req.method().clone();
    let query = req.uri().query().unwrap_or_default().to_owned();
    let raw = req.uri().path().strip_prefix("/v1/kv").unwrap_or_default().to_owned();
    let key = decode(raw.strip_prefix('/').unwrap_or_default(), false);
    let writes = method != Method::GET && method != Method::HEAD;

    if key.is_empty() {
        return match method {
            Method::GET => {
                let prefix = param(&query, "prefix").map(|p| decode(p, true)).unwrap_or_default();
                let limit = param(&query, "limit").and_then(|l| l.parse::<usize>().ok()).unwrap_or(1000).min(10_000);
                let keys: Vec<Json> = kv
                    .keys(&prefix, limit)
                    .into_iter()
                    .map(|k| json!({ "key": String::from_utf8_lossy(&k.key), "size": k.size, "ttl_ms": ttl_json(k.ttl) }))
                    .collect();
                let s = kv.stats();
                Ok(json_response(
                    200,
                    &json!({
                        "keys": keys,
                        "count": keys.len(),
                        "stats": {
                            "keys": s.keys, "expiring": s.expiring, "bytes": s.bytes, "max_bytes": s.max_bytes,
                            "hits": s.hits, "misses": s.misses, "evictions": s.evictions, "expired": s.expired,
                        },
                    }),
                ))
            }
            Method::DELETE if read_only => Ok(forbidden(READ_ONLY)),
            Method::DELETE => Ok(json_response(200, &json!({ "deleted": kv.clear() }))),
            Method::POST => {
                let (body, _held) = json_body(state, req).await?;
                let list = body.get("commands").unwrap_or(&body);
                let Some(items) = list.as_array() else {
                    return Ok(refusal(400, "validation", BAD_COMMAND));
                };
                // One command, or several: a list whose first item is a list.
                if items.first().is_some_and(Json::is_array) || body.get("commands").is_some() {
                    if items.len() > MAX_KV_COMMANDS {
                        return Err(too_large(format!(
                            "{} commands in one request: a request takes {MAX_KV_COMMANDS} at most",
                            items.len()
                        )));
                    }
                    let Some(commands) = items.iter().map(command_of).collect::<Option<Vec<_>>>() else {
                        return Ok(refusal(400, "validation", BAD_COMMAND));
                    };
                    drop(body);
                    // The answers together are bounded as one command's are: past
                    // the bound, the commands left are not run.
                    let mut size = 0usize;
                    let replies: Vec<Reply> = commands
                        .iter()
                        .map(|c| {
                            if size > MAX_ANSWER {
                                return Reply::Error(format!(
                                    "ERR the answers of this request passed {} MiB: the commands after were not run",
                                    MAX_ANSWER >> 20
                                ));
                            }
                            let reply = kv.command(c, read_only);
                            size = size.saturating_add(reply.weight());
                            reply
                        })
                        .collect();
                    return answer(
                        size,
                        move || json!({ "results": replies.into_iter().map(reply_json).collect::<Vec<_>>() }),
                    )
                    .await;
                }
                let Some(command) = command_of(list) else {
                    return Ok(refusal(400, "validation", BAD_COMMAND));
                };
                drop(body);
                match kv.command(&command, read_only) {
                    Reply::Error(e) if e.starts_with("READONLY") => Ok(forbidden(READ_ONLY)),
                    Reply::Error(e) => Ok(refusal(400, "validation", e.strip_prefix("ERR ").unwrap_or(&e))),
                    reply => answer(reply.weight(), move || json!({ "result": reply_json(reply) })).await,
                }
            }
            _ => Err(Error::NotFound),
        };
    }

    if writes && read_only {
        return Ok(forbidden(READ_ONLY));
    }
    match method {
        Method::GET | Method::HEAD => match kv.get_with_ttl(&key) {
            Some((value, ttl)) => {
                let mut res = Response::new(full(value));
                let h = res.headers_mut();
                h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
                h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
                if let Ttl::Millis(ms) = ttl {
                    h.insert("nuage-ttl-ms", HeaderValue::from(ms));
                }
                Ok(res)
            }
            None => Ok(refusal(404, "not_found", "no such key")),
        },
        Method::PUT => {
            let ttl = match param(&query, "ttl") {
                Some(t) => match seconds(t) {
                    Some(d) => Some(d),
                    None => return Ok(refusal(400, "validation", "`ttl` is a number of seconds, more than 0")),
                },
                None => None,
            };
            let options = SetOptions {
                ttl,
                keep_ttl: param(&query, "keepttl").is_some(),
                only_if_absent: param(&query, "nx").is_some(),
                only_if_present: param(&query, "xx").is_some(),
            };
            let value = read_body(state, req).await?.to_vec();
            Ok(match kv.set(&key, value, &options) {
                Ok(set) => json_response(200, &json!({ "set": set })),
                Err(e) => refused(&e),
            })
        }
        Method::DELETE => Ok(json_response(200, &json!({ "deleted": u8::from(kv.delete(&key)) }))),
        Method::POST => {
            if let Some(by) = param(&query, "incr") {
                let Some(by) = (if by.is_empty() { Some(1) } else { by.parse::<i64>().ok() }) else {
                    return Ok(refusal(400, "validation", "`incr` is a whole number"));
                };
                let ttl = param(&query, "ttl").and_then(seconds);
                return Ok(match kv.incr_by(&key, by, ttl) {
                    Ok(value) => json_response(200, &json!({ "value": value })),
                    Err(e) => refused(&e),
                });
            }
            if let Some(ttl) = param(&query, "expire") {
                let Some(ttl) = seconds(ttl) else {
                    return Ok(refusal(400, "validation", "`expire` is a number of seconds, more than 0"));
                };
                return Ok(json_response(200, &json!({ "set": kv.expire(&key, Some(ttl)) })));
            }
            if param(&query, "persist").is_some() {
                return Ok(json_response(200, &json!({ "set": kv.expire(&key, None) })));
            }
            Ok(refusal(400, "validation", "POST on a key takes `?incr`, `?expire=<seconds>` or `?persist`"))
        }
        _ => Err(Error::NotFound),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_queries_and_commands() {
        assert_eq!(decode("session%3A42/a%2Fb%zz%", false), b"session:42/a/b%zz%");
        assert_eq!(decode("a+b%20c", true), b"a b c");
        assert_eq!(decode("a+b", false), b"a+b");
        assert_eq!(param("nx&ttl=60", "ttl"), Some("60"));
        assert_eq!(param("nx&ttl=60", "nx"), Some(""));
        assert_eq!(param("nxx", "nx"), None);
        assert_eq!(seconds("60"), Some(Duration::from_secs(60)));
        assert_eq!(seconds("0.25"), Some(Duration::from_millis(250)));
        assert_eq!(seconds("0"), None);
        assert_eq!(seconds("soon"), None);
        assert_eq!(
            command_of(&json!(["SET", "k", "v", "EX", 60])),
            Some(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec(), b"EX".to_vec(), b"60".to_vec()])
        );
        assert_eq!(command_of(&json!(["SET", ["k"]])), None);
        assert_eq!(
            reply_json(Reply::Array(vec![Reply::Ok, Reply::Nil, Reply::Int(2), Reply::Bulk(b"v".to_vec())])),
            json!(["OK", null, 2, "v"])
        );
    }
}
