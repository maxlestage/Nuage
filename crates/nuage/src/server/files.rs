//! `/v1/files`: the files of an application, in an S3-compatible bucket
//! (Cloudflare R2, AWS S3, MinIO...), one folder per application.
//!
//! The bytes never go through the server: it lists the folder, and signs
//! links that the client follows to the bucket itself — a download link
//! (`GET /v1/files/<name>` answers `307` to it, `?link` gives it as JSON),
//! an upload link (`POST /v1/files/<name>`, then `PUT` the bytes to it). A
//! download link takes `Range`: a client resumes a large download where it
//! stopped. A read-only token lists and downloads; uploads and deletes take a
//! read-write token.
//!
//! The server is set with `NUAGE_FILES_S3_BUCKET` and the other
//! `NUAGE_FILES_S3_*` variables (see [`crate::config`]); without them,
//! `/v1/files` answers `501`.

use std::sync::Arc;

use hyper::body::Incoming;
use hyper::header::{self, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use memoire_orm::{Error, Result};
use serde_json::{Value as Json, json};

use super::{HttpBody, READ_ONLY, State, forbidden, full, json_response, read_json};
use crate::s3::{S3Options, S3Store};

/// Longest file name, in bytes.
pub const MAX_NAME: usize = 512;

/// The bucket of the applications' files.
pub struct FileStore {
    store: S3Store,
    /// How long a link works.
    link_seconds: u64,
}

impl std::fmt::Debug for FileStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileStore").field("store", &self.store).field("link_seconds", &self.link_seconds).finish()
    }
}

impl FileStore {
    /// The files of every application go in the bucket of `options`, under
    /// `<prefix><application>/`; a link works `link_seconds` (7 days at most).
    pub fn new(options: S3Options, link_seconds: u64) -> Result<FileStore> {
        Ok(FileStore { store: S3Store::new(options)?, link_seconds: link_seconds.clamp(60, 7 * 24 * 3600) })
    }

    /// Where the files are, for the logs: `s3://bucket/prefix`.
    pub fn describe(&self) -> String {
        use crate::engine::BackupStore;
        self.store.describe()
    }
}

/// `name` is a file name: `a-z A-Z 0-9 . _ -` in parts separated by `/`, no
/// empty part, no `.` or `..` part, [`MAX_NAME`] bytes at most.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        })
}

fn refusal(status: u16, kind: &str, message: impl Into<String>) -> Response<HttpBody> {
    json_response(status, &json!({ "error": { "kind": kind, "message": message.into() } }))
}

fn unreachable_bucket(e: &Error) -> Response<HttpBody> {
    refusal(502, "storage", format!("the file bucket did not answer as it should: {e}"))
}

fn query_param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query.split('&').find_map(|kv| match kv.split_once('=') {
        Some((k, v)) if k == name => Some(v),
        None if kv == name => Some(""),
        _ => None,
    })
}

/// `%XX` decoding of a query value (`+` is a space).
fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                match std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(b) => {
                        out.push(b);
                        i += 3;
                        continue;
                    }
                    None => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn expires_at(seconds: u64) -> String {
    (chrono::Utc::now() + chrono::Duration::seconds(seconds as i64)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// `/v1/files` and `/v1/files/<name>`, for the application `app`.
pub(super) async fn route(
    state: &Arc<State>,
    app: &str,
    read_only: bool,
    req: Request<Incoming>,
    name: &str,
) -> Result<Response<HttpBody>> {
    let Some(files) = state.options.files.as_deref() else {
        return Ok(refusal(
            501,
            "not_configured",
            "this server keeps no files: set NUAGE_FILES_S3_BUCKET and the other NUAGE_FILES_S3_* variables \
             (an S3-compatible bucket: Cloudflare R2, AWS S3, MinIO...)",
        ));
    };
    let folder = format!("{app}/");
    let query = req.uri().query().unwrap_or_default().to_owned();
    let method = req.method().clone();
    if name.is_empty() {
        if method != Method::GET {
            return Err(Error::NotFound);
        }
        let prefix = query_param(&query, "prefix").map(decode).unwrap_or_default();
        let objects = match files.store.objects(&format!("{folder}{prefix}")).await {
            Ok(o) => o,
            Err(e) => return Ok(unreachable_bucket(&e)),
        };
        let list: Vec<Json> = objects
            .iter()
            .filter_map(|o| {
                let name = o.key.strip_prefix(&folder)?;
                Some(json!({ "name": name, "size": o.size, "updated": o.last_modified, "etag": o.etag }))
            })
            .collect();
        let bytes: u64 = objects.iter().map(|o| o.size).sum();
        return Ok(json_response(200, &json!({ "files": list, "count": list.len(), "bytes": bytes })));
    }
    if !valid_name(name) {
        return Ok(refusal(
            400,
            "validation",
            format!(
                "`{name}` is not a file name: letters, digits, `.`, `_` and `-`, in parts separated by `/`, \
                 {MAX_NAME} bytes at most"
            ),
        ));
    }
    let key = format!("{folder}{name}");
    let seconds = files.link_seconds;
    match method {
        Method::GET | Method::HEAD => {
            let disposition;
            let mut extra: Vec<(&str, &str)> = Vec::new();
            if let Some(as_name) = query_param(&query, "download") {
                let as_name = decode(as_name);
                let as_name =
                    if as_name.is_empty() { name.rsplit('/').next().unwrap_or(name).to_owned() } else { as_name };
                disposition = format!("attachment; filename=\"{}\"", as_name.replace(['"', '\\'], "_"));
                extra.push(("response-content-disposition", &disposition));
            }
            let url = files.store.presigned_url(method.as_str(), &key, seconds, &extra);
            if query_param(&query, "link").is_some() {
                return Ok(json_response(200, &json!({ "name": name, "url": url, "expires_at": expires_at(seconds) })));
            }
            let mut res = Response::new(full(""));
            *res.status_mut() = StatusCode::TEMPORARY_REDIRECT;
            res.headers_mut()
                .insert(header::LOCATION, HeaderValue::from_str(&url).map_err(|e| Error::Validation(e.to_string()))?);
            res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            Ok(res)
        }
        Method::POST => {
            if read_only {
                return Ok(forbidden(READ_ONLY));
            }
            let _ = read_json(state, req).await?;
            let url = files.store.presigned_url("PUT", &key, seconds, &[]);
            Ok(json_response(
                200,
                &json!({
                    "name": name,
                    "method": "PUT",
                    "url": url,
                    "expires_at": expires_at(seconds),
                    "note": "PUT the bytes to url, with their Content-Length (5 GB at most): they go straight to the bucket",
                }),
            ))
        }
        Method::DELETE => {
            if read_only {
                return Ok(forbidden(READ_ONLY));
            }
            match files.store.remove(&key).await {
                Ok(()) => Ok(json_response(200, &json!({ "name": name, "deleted": true }))),
                Err(e) => Ok(unreachable_bucket(&e)),
            }
        }
        _ => Err(Error::NotFound),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names() {
        for ok in ["tuiles-eu-france.tar", "regions/fr/zone-sud.pmtiles", "a", "v1.2_x"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", "/a", "a/", "a//b", "../a", "a/./b", "a b", "é", "a?b", "a%2Fb", &"x".repeat(MAX_NAME + 1)] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn query_values() {
        assert_eq!(query_param("link&prefix=fr%2F", "prefix").map(decode).as_deref(), Some("fr/"));
        assert_eq!(query_param("link&prefix=fr", "link"), Some(""));
        assert_eq!(query_param("linked=1", "link"), None);
        assert_eq!(decode("a+b%20c%zz%4"), "a b c%zz%4");
    }
}
