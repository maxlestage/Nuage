//! Backups to S3-compatible object storage (AWS S3, Cloudflare R2, MinIO,
//! Scaleway, OVHcloud...): a small client signing its requests with AWS
//! Signature Version 4, over HTTP/1.1 and rustls.
//!
//! Every request has a time limit, so a bucket that takes the connection and
//! then says nothing cannot hold a backup round, a restore, the shutdown or
//! `/v1/files` for ever. The connection, the TLS handshake, sending the
//! request and receiving the head of the answer must be done within the
//! store's timeout (60 seconds) plus one second per 64 KiB of body sent; the
//! body of the answer is then read piece by piece, and a wait of more than
//! the timeout for the next piece is an error too. A large snapshot sent at
//! 64 KiB/s or more, or received at any speed while bytes keep coming, is
//! never cut.

use std::sync::Arc;
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::client::conn::http1;
use hyper::{Method, Request, Response};
use hyper_util::rt::TokioIo;
use memoire_orm::{Error, Result};
use tokio::net::TcpStream;

use crate::engine::BackupStore;
use crate::engine::storage_err;

/// Where and how to reach the bucket.
#[derive(Clone)]
pub struct S3Options {
    /// Endpoint URL: `https://s3.eu-west-3.amazonaws.com`,
    /// `https://<account>.r2.cloudflarestorage.com`, `http://127.0.0.1:9000`...
    pub endpoint: String,
    /// Bucket name.
    pub bucket: String,
    /// Region (`us-east-1`, `auto` for R2...).
    pub region: String,
    /// Prefix of every object (`nuage/prod/`), may be empty.
    pub prefix: String,
    /// Access key id.
    pub access_key_id: String,
    /// Secret access key.
    pub secret_access_key: String,
}

impl std::fmt::Debug for S3Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Options")
            .field("endpoint", &self.endpoint)
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

/// How long a request may wait for the bucket, unless set otherwise with
/// [`S3Store::with_timeout`].
const TIMEOUT: Duration = Duration::from_secs(60);

/// The time a request may take on top of the timeout for each 64 KiB of the
/// body it sends: the upload of a large snapshot needs it on a slow line.
const BYTES_PER_EXTRA_SECOND: u64 = 64 * 1024;

/// A [`BackupStore`] in an S3 bucket (path-style requests:
/// `<endpoint>/<bucket>/<key>`, which every S3-compatible service accepts).
pub struct S3Store {
    options: S3Options,
    https: bool,
    host: String,
    port: u16,
    tls: tokio_rustls::TlsConnector,
    timeout: Duration,
}

impl std::fmt::Debug for S3Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Store").field("options", &self.options).field("timeout", &self.timeout).finish()
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_hex(data: &[u8]) -> String {
    hex(&crate::hash::sha256(data))
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    crate::hash::hmac_sha256(key, data).to_vec()
}

/// Percent-encoding of SigV4: everything but `A-Z a-z 0-9 - _ . ~` (and
/// `/` in paths).
pub(crate) fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') || (keep_slash && b == b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// What a request signature covers.
pub(crate) struct ToSign<'a> {
    pub method: &'a str,
    pub path: &'a str,
    /// Query parameters, unencoded.
    pub query: &'a [(&'a str, &'a str)],
    /// Headers to sign (names in lower case), `host` included.
    pub headers: &'a [(&'a str, &'a str)],
    pub payload_sha256: &'a str,
    /// `20150830T123600Z`.
    pub amz_date: &'a str,
    pub region: &'a str,
    pub service: &'a str,
}

/// The canonical query string of SigV4: encoded pairs, sorted.
fn canonical_query(query: &[(&str, &str)]) -> String {
    let mut query: Vec<(String, String)> =
        query.iter().map(|(k, v)| (uri_encode(k, false), uri_encode(v, false))).collect();
    query.sort();
    query.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

/// The scope, the signed header names and the signature of a request.
fn signature(s: &ToSign<'_>, secret: &str) -> (String, String, String) {
    let mut headers: Vec<(String, String)> =
        s.headers.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_owned())).collect();
    headers.sort();
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed = headers.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let canonical = format!(
        "{}\n{}\n{}\n{canonical_headers}\n{signed}\n{}",
        s.method,
        uri_encode(s.path, true),
        canonical_query(s.query),
        s.payload_sha256
    );
    let date = &s.amz_date[..8];
    let scope = format!("{date}/{}/{}/aws4_request", s.region, s.service);
    let to_sign = format!("AWS4-HMAC-SHA256\n{}\n{scope}\n{}", s.amz_date, sha256_hex(canonical.as_bytes()));
    let mut key = hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    for part in [s.region, s.service, "aws4_request"] {
        key = hmac_sha256(&key, part.as_bytes());
    }
    (scope, signed, hex(&hmac_sha256(&key, to_sign.as_bytes())))
}

/// The `Authorization` header of AWS Signature Version 4.
pub(crate) fn authorization(s: &ToSign<'_>, access_key_id: &str, secret: &str) -> String {
    let (scope, signed, signature) = signature(s, secret);
    format!("AWS4-HMAC-SHA256 Credential={access_key_id}/{scope}, SignedHeaders={signed}, Signature={signature}")
}

/// A presigned request (SigV4 in the query string): what goes after `?` in
/// the URL of `method path` on `host`, valid `expires` seconds from
/// `amz_date`. Only `host` is signed, with an unsigned payload: whoever holds
/// the URL sends the bytes, without any key.
#[allow(clippy::too_many_arguments)]
pub(crate) fn presign(
    method: &str,
    host: &str,
    path: &str,
    extra: &[(&str, &str)],
    amz_date: &str,
    expires: u64,
    region: &str,
    access_key_id: &str,
    secret: &str,
) -> String {
    let credential = format!("{access_key_id}/{}/{region}/s3/aws4_request", &amz_date[..8]);
    let expires = expires.to_string();
    let mut query: Vec<(&str, &str)> = vec![
        ("X-Amz-Algorithm", "AWS4-HMAC-SHA256"),
        ("X-Amz-Credential", &credential),
        ("X-Amz-Date", amz_date),
        ("X-Amz-Expires", &expires),
        ("X-Amz-SignedHeaders", "host"),
    ];
    query.extend_from_slice(extra);
    let (_, _, signature) = signature(
        &ToSign {
            method,
            path,
            query: &query,
            headers: &[("host", host)],
            payload_sha256: "UNSIGNED-PAYLOAD",
            amz_date,
            region,
            service: "s3",
        },
        secret,
    );
    format!("{}&X-Amz-Signature={signature}", canonical_query(&query))
}

/// An object of a bucket listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ObjectInfo {
    /// Its key, without the store's prefix.
    pub key: String,
    pub size: u64,
    /// `2026-11-15T07:16:00.000Z`.
    pub last_modified: String,
    /// The ETag, without its quotes.
    pub etag: String,
}

fn s3_err(e: impl std::fmt::Display) -> Error {
    storage_err(format!("S3 backup: {e}"))
}

/// Text between `<tag>` and `</tag>`, each time.
fn xml_values(xml: &str, tag: &str) -> Vec<String> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let Some(end) = after.find(&close) else { break };
        out.push(
            after[..end]
                .replace("&amp;", "&")
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&quot;", "\"")
                .replace("&apos;", "'"),
        );
        rest = &after[end + close.len()..];
    }
    out
}

/// The raw text between `<tag>` and `</tag>`, each time (entities kept).
fn xml_blocks<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let Some(end) = after.find(&close) else { break };
        out.push(&after[..end]);
        rest = &after[end + close.len()..];
    }
    out
}

impl S3Store {
    /// A store for `options`.
    pub fn new(options: S3Options) -> Result<S3Store> {
        let (https, rest) = if let Some(r) = options.endpoint.strip_prefix("https://") {
            (true, r)
        } else if let Some(r) = options.endpoint.strip_prefix("http://") {
            (false, r)
        } else {
            return Err(s3_err("the endpoint must start with https:// or http://"));
        };
        let authority = rest.trim_end_matches('/');
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_owned(), p.parse().map_err(|_| s3_err("invalid endpoint port"))?),
            None => (authority.to_owned(), if https { 443 } else { 80 }),
        };
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
        let mut config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(s3_err)?
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(S3Store {
            options,
            https,
            host,
            port,
            tls: tokio_rustls::TlsConnector::from(Arc::new(config)),
            timeout: TIMEOUT,
        })
    }

    /// The same store with another time limit (60 seconds by default): how
    /// long the bucket may take to answer a request with no body, and the
    /// longest wait for the next piece of an answer. A request that sends a
    /// body has one more second per 64 KiB of it to be answered.
    pub fn with_timeout(self, timeout: Duration) -> S3Store {
        S3Store { timeout, ..self }
    }

    fn key(&self, name: &str) -> String {
        format!("{}{name}", self.options.prefix)
    }

    fn host_header(&self) -> String {
        if self.port == if self.https { 443 } else { 80 } {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// The URL of `method` on the object `name` (under the prefix), signed
    /// for `expires` seconds (7 days at most, the limit of SigV4), with the
    /// `extra` query parameters (`response-content-disposition`...).
    pub(crate) fn presigned_url(&self, method: &str, name: &str, expires: u64, extra: &[(&str, &str)]) -> String {
        let path = format!("/{}/{}", self.options.bucket, self.key(name));
        let amz_date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let query = presign(
            method,
            &self.host_header(),
            &path,
            extra,
            &amz_date,
            expires.clamp(1, 7 * 24 * 3600),
            &self.options.region,
            &self.options.access_key_id,
            &self.options.secret_access_key,
        );
        let scheme = if self.https { "https" } else { "http" };
        format!("{scheme}://{}{}?{query}", self.host_header(), uri_encode(&path, true))
    }

    /// The objects whose name (under the prefix) starts with `prefix`, sorted.
    pub(crate) async fn objects(&self, prefix: &str) -> Result<Vec<ObjectInfo>> {
        let full = self.key(prefix);
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query = vec![("list-type", "2"), ("prefix", full.as_str())];
            if let Some(t) = &token {
                query.push(("continuation-token", t.as_str()));
            }
            let (status, body) = self.send(Method::GET, "", &query, Vec::new()).await?;
            Self::check(status, &body, "LIST")?;
            let xml = String::from_utf8_lossy(&body);
            for block in xml_blocks(&xml, "Contents") {
                let value = |tag: &str| xml_values(block, tag).into_iter().next().unwrap_or_default();
                let Some(key) = value("Key").strip_prefix(&self.options.prefix).map(str::to_owned) else {
                    continue;
                };
                out.push(ObjectInfo {
                    key,
                    size: value("Size").parse().unwrap_or(0),
                    last_modified: value("LastModified"),
                    etag: value("ETag").trim_matches('"').to_owned(),
                });
            }
            token = xml_values(&xml, "NextContinuationToken").into_iter().next();
            if token.is_none() || xml_values(&xml, "IsTruncated").first().map(String::as_str) != Some("true") {
                break;
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    /// Deletes the object `name` (under the prefix); a missing one is no error.
    pub(crate) async fn remove(&self, name: &str) -> Result<()> {
        let key = self.key(name);
        let (status, body) = self.send(Method::DELETE, &key, &[], Vec::new()).await?;
        if status == 404 {
            return Ok(());
        }
        Self::check(status, &body, &format!("DELETE {key}"))
    }

    /// Sends one signed request; returns the status and the body. It fails
    /// when the bucket does not answer in time (see the module's text).
    async fn send(&self, method: Method, key: &str, query: &[(&str, &str)], body: Vec<u8>) -> Result<(u16, Vec<u8>)> {
        let what = if key.is_empty() { "LIST".to_owned() } else { format!("{method} {key}") };
        let late = |how: &str, after: Duration| {
            s3_err(format!("{what}: {} {how} {} seconds", self.describe(), after.as_secs_f64()))
        };
        let limit = self.timeout + Duration::from_millis(body.len() as u64 * 1000 / BYTES_PER_EXTRA_SECOND);
        let request = self.request(method, key, query, body)?;
        let response = tokio::time::timeout(limit, self.open(request))
            .await
            .map_err(|_| late("did not answer within", limit))??;
        let status = response.status().as_u16();
        let mut answer = response.into_body();
        let mut body = Vec::new();
        while let Some(frame) = tokio::time::timeout(self.timeout, answer.frame())
            .await
            .map_err(|_| late("stopped answering for", self.timeout))?
        {
            if let Ok(data) = frame.map_err(s3_err)?.into_data() {
                body.extend_from_slice(&data);
            }
        }
        Ok((status, body))
    }

    /// The signed request `method` on `key` (the bucket itself when empty).
    fn request(
        &self,
        method: Method,
        key: &str,
        query: &[(&str, &str)],
        body: Vec<u8>,
    ) -> Result<Request<Full<Bytes>>> {
        let path = if key.is_empty() {
            format!("/{}", self.options.bucket)
        } else {
            format!("/{}/{key}", self.options.bucket)
        };
        let payload = sha256_hex(&body);
        let amz_date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let host = self.host_header();
        let auth = authorization(
            &ToSign {
                method: method.as_str(),
                path: &path,
                query,
                headers: &[("host", &host), ("x-amz-content-sha256", &payload), ("x-amz-date", &amz_date)],
                payload_sha256: &payload,
                amz_date: &amz_date,
                region: &self.options.region,
                service: "s3",
            },
            &self.options.access_key_id,
            &self.options.secret_access_key,
        );
        let mut uri = uri_encode(&path, true);
        if !query.is_empty() {
            let q: Vec<String> =
                query.iter().map(|(k, v)| format!("{}={}", uri_encode(k, false), uri_encode(v, false))).collect();
            uri = format!("{uri}?{}", q.join("&"));
        }
        Request::builder()
            .method(method)
            .uri(uri)
            .header("host", &host)
            .header("x-amz-content-sha256", &payload)
            .header("x-amz-date", &amz_date)
            .header("authorization", auth)
            .header("content-length", body.len())
            .body(Full::new(Bytes::from(body)))
            .map_err(s3_err)
    }

    /// Connects to the bucket, sends `request` and waits for the head of the
    /// answer; its body comes after. A request given up (its future or its
    /// body dropped) closes the connection: hyper's connection task stops
    /// when nobody waits for the answer any more.
    async fn open(&self, request: Request<Full<Bytes>>) -> Result<Response<Incoming>> {
        let tcp = TcpStream::connect((self.host.as_str(), self.port)).await.map_err(s3_err)?;
        if self.https {
            let name = rustls::pki_types::ServerName::try_from(self.host.clone()).map_err(s3_err)?;
            let stream = self.tls.connect(name, tcp).await.map_err(s3_err)?;
            let (mut sender, conn) = http1::handshake(TokioIo::new(stream)).await.map_err(s3_err)?;
            tokio::spawn(conn);
            sender.send_request(request).await.map_err(s3_err)
        } else {
            let (mut sender, conn) = http1::handshake(TokioIo::new(tcp)).await.map_err(s3_err)?;
            tokio::spawn(conn);
            sender.send_request(request).await.map_err(s3_err)
        }
    }

    /// Runs `f` on a private runtime, on its own thread: backups are called
    /// from synchronous code, which may itself run inside another runtime.
    /// The runtime is shut down without waiting for its blocking pool, so a
    /// name lookup that hangs cannot hold the caller past the time limit.
    fn block<T: Send>(&self, f: impl std::future::Future<Output = Result<T>> + Send) -> Result<T> {
        std::thread::scope(|s| {
            s.spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(s3_err)?;
                let out = runtime.block_on(f);
                runtime.shutdown_background();
                out
            })
            .join()
            .unwrap_or_else(|_| Err(s3_err("the request panicked")))
        })
    }

    fn check(status: u16, body: &[u8], what: &str) -> Result<()> {
        if (200..300).contains(&status) {
            Ok(())
        } else {
            let text = String::from_utf8_lossy(body);
            let code = xml_values(&text, "Code").into_iter().next().unwrap_or_default();
            Err(s3_err(format!("{what}: HTTP {status} {code}")))
        }
    }
}

impl BackupStore for S3Store {
    fn describe(&self) -> String {
        format!("s3://{}/{}", self.options.bucket, self.options.prefix)
    }

    fn put(&self, name: &str, data: &[u8]) -> Result<()> {
        let key = self.key(name);
        let (status, body) = self.block(self.send(Method::PUT, &key, &[], data.to_vec()))?;
        Self::check(status, &body, &format!("PUT {key}"))
    }

    // A missing object is `None`; a missing bucket is an error, never an
    // empty backup: a mistyped bucket name must not start an empty database.
    fn get(&self, name: &str) -> Result<Option<Vec<u8>>> {
        let key = self.key(name);
        let (status, body) = self.block(self.send(Method::GET, &key, &[], Vec::new()))?;
        if status == 404 {
            let code = xml_values(&String::from_utf8_lossy(&body), "Code").into_iter().next();
            if code.as_deref() == Some("NoSuchBucket") {
                return Err(s3_err(format!(
                    "GET {key}: the bucket `{}` does not exist (HTTP 404 NoSuchBucket)",
                    self.options.bucket
                )));
            }
            return Ok(None);
        }
        Self::check(status, &body, &format!("GET {key}"))?;
        Ok(Some(body))
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>> {
        Ok(self.block(self.objects(prefix))?.into_iter().map(|o| o.key).collect())
    }

    fn delete(&self, name: &str) -> Result<()> {
        self.block(self.remove(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example of the AWS Signature Version 4 documentation.
    #[test]
    fn signature_matches_the_aws_example() {
        let auth = authorization(
            &ToSign {
                method: "GET",
                path: "/",
                query: &[("Action", "ListUsers"), ("Version", "2010-05-08")],
                headers: &[
                    ("content-type", "application/x-www-form-urlencoded; charset=utf-8"),
                    ("host", "iam.amazonaws.com"),
                    ("x-amz-date", "20150830T123600Z"),
                ],
                payload_sha256: &sha256_hex(b""),
                amz_date: "20150830T123600Z",
                region: "us-east-1",
                service: "iam",
            },
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
        );
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, \
             SignedHeaders=content-type;host;x-amz-date, \
             Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
        );
    }

    /// The presigned URL example of the S3 documentation ("Authenticating
    /// Requests: Using Query Parameters").
    #[test]
    fn presigned_url_matches_the_aws_example() {
        let query = presign(
            "GET",
            "examplebucket.s3.amazonaws.com",
            "/test.txt",
            &[],
            "20130524T000000Z",
            86400,
            "us-east-1",
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        );
        assert_eq!(
            query,
            "X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request\
             &X-Amz-Date=20130524T000000Z&X-Amz-Expires=86400&X-Amz-SignedHeaders=host\
             &X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"
        );
    }

    #[test]
    fn listing_blocks() {
        let xml = "<ListBucketResult><Contents><Key>files/a/x.tar</Key><Size>12</Size>\
                   <LastModified>2026-11-15T07:00:00.000Z</LastModified><ETag>&quot;e1&quot;</ETag></Contents>\
                   <Contents><Key>files/a/y</Key><Size>3</Size></Contents></ListBucketResult>";
        let blocks = xml_blocks(xml, "Contents");
        assert_eq!(blocks.len(), 2);
        assert_eq!(xml_values(blocks[0], "ETag"), vec!["\"e1\""]);
        assert_eq!(xml_values(blocks[1], "Size"), vec!["3"]);
    }

    /// A bucket that takes connections and never answers: it reads what it
    /// is sent, and says on the channel each time a client hangs up.
    fn stalled_bucket() -> (String, tokio::sync::mpsc::UnboundedReceiver<()>) {
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (hung_up, closed) = tokio::sync::mpsc::unbounded_channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let (mut stream, hung_up) = (stream.unwrap(), hung_up.clone());
                std::thread::spawn(move || {
                    let mut buf = [0; 4096];
                    while stream.read(&mut buf).is_ok_and(|n| n > 0) {}
                    let _ = hung_up.send(());
                });
            }
        });
        (endpoint, closed)
    }

    /// `/v1/files` asks the bucket from the server's own runtime: a bucket
    /// that stalls fails the request in time, and the connection is closed.
    #[tokio::test]
    async fn a_stalled_bucket_times_out_inside_a_runtime() {
        let (endpoint, mut closed) = stalled_bucket();
        let store = S3Store::new(S3Options {
            endpoint,
            bucket: "bucket".into(),
            region: "auto".into(),
            prefix: "files/".into(),
            access_key_id: "key".into(),
            secret_access_key: "secret".into(),
        })
        .unwrap()
        .with_timeout(Duration::from_millis(200));
        let started = std::time::Instant::now();
        let e = store.objects("app/").await.unwrap_err().to_string();
        assert!(e.contains("LIST: s3://bucket/files/ did not answer within 0.2 seconds"), "{e}");
        let e = store.remove("app/a.txt").await.unwrap_err().to_string();
        assert!(e.contains("DELETE files/app/a.txt: s3://bucket/files/ did not answer within 0.2 seconds"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
        for _ in 0..2 {
            let hung_up = tokio::time::timeout(Duration::from_secs(1), closed.recv()).await;
            assert_eq!(hung_up, Ok(Some(())), "the connection of a request that timed out is closed");
        }
    }

    #[test]
    fn xml_and_encoding() {
        let xml = "<ListBucketResult><Contents><Key>a/b&amp;c</Key></Contents><Contents><Key>d</Key></Contents><IsTruncated>false</IsTruncated></ListBucketResult>";
        assert_eq!(xml_values(xml, "Key"), vec!["a/b&c", "d"]);
        assert_eq!(uri_encode("wal/0001 x+y.log", true), "wal/0001%20x%2By.log");
    }
}
