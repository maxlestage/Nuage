//! Backups to S3-compatible object storage (AWS S3, Cloudflare R2, MinIO,
//! Scaleway, OVHcloud...): a small client signing its requests with AWS
//! Signature Version 4, over HTTP/1.1 and rustls.
//!
//! No request waits without end: the connection has a time limit, and a
//! request is given up when its bytes stop moving (see [`Limits`]). A file is
//! sent and received a piece at a time, never held whole in memory.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use http_body_util::BodyExt;
use hyper::body::{Body, Bytes, Frame, SizeHint};
use hyper::client::conn::http1;
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use memoire_orm::{Error, Result};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

use crate::engine::BackupStore;
use crate::engine::storage_err;

/// How long a request is waited for. A bucket that stops answering is an
/// error like any other: the backup is tried again at the next round.
#[derive(Clone, Copy, Debug)]
struct Limits {
    /// The connection: name lookup, TCP, then TLS.
    connect: Duration,
    /// A request is given up when no byte went through its connection, either
    /// way, for this long (the wait for the first byte of the answer included).
    stall: Duration,
    /// ... and when it has lasted longer than this, plus the time its bytes
    /// take on the slowest link that is waited for ([`SLOWEST`]).
    total: Duration,
}

const LIMITS: Limits =
    Limits { connect: Duration::from_secs(10), stall: Duration::from_secs(30), total: Duration::from_secs(60) };

/// Bytes per second of the slowest transfer that is waited for to its end.
const SLOWEST: u64 = 16 << 10;

/// Bytes of a body handed to the connection at a time.
const PIECE: usize = 256 << 10;

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

/// A [`BackupStore`] in an S3 bucket (path-style requests:
/// `<endpoint>/<bucket>/<key>`, which every S3-compatible service accepts).
pub struct S3Store {
    options: S3Options,
    https: bool,
    host: String,
    port: u16,
    tls: tokio_rustls::TlsConnector,
    limits: Limits,
}

impl std::fmt::Debug for S3Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Store").field("options", &self.options).finish()
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_hex(data: &[u8]) -> String {
    hex(&crate::hash::sha256(data))
}

/// The body of a request: bytes in memory, or a file read as it is sent.
enum Source {
    Memory(Bytes),
    File(File),
}

/// A request body handed to the connection a piece at a time: a file is
/// never in memory whole.
struct Upload {
    source: Source,
    /// Bytes not handed over yet.
    left: u64,
    /// SHA-256 of the whole body, in hexadecimal: the request signs it.
    sha256: String,
}

impl Upload {
    fn memory(data: Vec<u8>) -> Upload {
        let sha256 = sha256_hex(&data);
        Upload { left: data.len() as u64, source: Source::Memory(Bytes::from(data)), sha256 }
    }

    /// The file at `path`: read once for its digest, then again as it is sent.
    fn file(path: &Path) -> Result<Upload> {
        let fail = |e: std::io::Error| storage_err(format!("cannot read {}: {e}", path.display()));
        let mut file = File::open(path).map_err(fail)?;
        let mut digest = Sha256::new();
        let mut buf = vec![0u8; PIECE];
        let mut left = 0u64;
        loop {
            let n = file.read(&mut buf).map_err(fail)?;
            if n == 0 {
                break;
            }
            digest.update(&buf[..n]);
            left += n as u64;
        }
        file.seek(SeekFrom::Start(0)).map_err(fail)?;
        Ok(Upload { source: Source::File(file), left, sha256: hex(&digest.finalize()) })
    }
}

impl Body for Upload {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<Frame<Bytes>, Self::Error>>> {
        let this = self.get_mut();
        if this.left == 0 {
            return Poll::Ready(None);
        }
        let n = this.left.min(PIECE as u64) as usize;
        let piece = match &mut this.source {
            Source::Memory(bytes) => bytes.split_to(n),
            Source::File(file) => {
                // Read where it is asked for: a piece of a local file, on the
                // thread this one request has to itself (see `S3Store::block`).
                // A file shorter than when its digest was taken ends the request.
                let mut buf = vec![0u8; n];
                if let Err(e) = file.read_exact(&mut buf) {
                    return Poll::Ready(Some(Err(e)));
                }
                Bytes::from(buf)
            }
        };
        this.left -= n as u64;
        Poll::Ready(Some(Ok(Frame::data(piece))))
    }

    fn is_end_stream(&self) -> bool {
        self.left == 0
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.left)
    }
}

/// What the endpoint answered.
struct Answer {
    status: u16,
    /// Its `Content-Length`.
    length: Option<u64>,
    /// Its body, unless it went to a file.
    body: Vec<u8>,
}

/// The connection to the endpoint, in clear or under TLS.
trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

/// A socket that counts the bytes it takes and gives: whoever waits for a
/// request sees whether its connection still moves, whatever is in transit
/// (a body being sent, an answer being received, a TLS handshake).
struct Counted {
    socket: TcpStream,
    moved: Arc<AtomicU64>,
}

impl AsyncRead for Counted {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let read = Pin::new(&mut this.socket).poll_read(cx, buf);
        this.moved.fetch_add((buf.filled().len() - before) as u64, Ordering::Relaxed);
        read
    }
}

impl AsyncWrite for Counted {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, data: &[u8]) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let written = Pin::new(&mut this.socket).poll_write(cx, data);
        if let Poll::Ready(Ok(n)) = &written {
            this.moved.fetch_add(*n as u64, Ordering::Relaxed);
        }
        written
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let written = Pin::new(&mut this.socket).poll_write_vectored(cx, data);
        if let Poll::Ready(Ok(n)) = &written {
            this.moved.fetch_add(*n as u64, Ordering::Relaxed);
        }
        written
    }

    fn is_write_vectored(&self) -> bool {
        self.socket.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().socket).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().socket).poll_shutdown(cx)
    }
}

/// Ends the task that drives a connection when the request that owns it is
/// done or given up: a connection that went silent is closed, not left behind.
struct Driver(tokio::task::JoinHandle<()>);

impl Drop for Driver {
    fn drop(&mut self) {
        self.0.abort();
    }
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
            limits: LIMITS,
        })
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
            let answer = self.send(Method::GET, "", &query, Upload::memory(Vec::new()), None).await?;
            Self::check(&answer, "LIST")?;
            let xml = String::from_utf8_lossy(&answer.body);
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
        let answer = self.send(Method::DELETE, &key, &[], Upload::memory(Vec::new()), None).await?;
        if answer.status == 404 {
            return Ok(());
        }
        Self::check(&answer, &format!("DELETE {key}"))
    }

    /// Connects to the endpoint: TCP, then TLS when it is `https`. `moved`
    /// counts the bytes that go through the connection, either way.
    async fn connect(&self, moved: Arc<AtomicU64>) -> Result<Box<dyn Stream>> {
        let socket = TcpStream::connect((self.host.as_str(), self.port)).await.map_err(s3_err)?;
        let socket = Counted { socket, moved };
        if !self.https {
            return Ok(Box::new(socket));
        }
        let name = rustls::pki_types::ServerName::try_from(self.host.clone()).map_err(s3_err)?;
        Ok(Box::new(self.tls.connect(name, socket).await.map_err(s3_err)?))
    }

    /// Sends one signed request and returns the answer. `into`: where the
    /// body of a successful answer is written as it comes, instead of being
    /// kept in memory. The request is given up, with an error, when the
    /// endpoint does not answer within the [`Limits`].
    async fn send(
        &self,
        method: Method,
        key: &str,
        query: &[(&str, &str)],
        body: Upload,
        into: Option<&Path>,
    ) -> Result<Answer> {
        let what = if key.is_empty() { "LIST".to_owned() } else { format!("{method} {key}") };
        let path = if key.is_empty() {
            format!("/{}", self.options.bucket)
        } else {
            format!("/{}/{key}", self.options.bucket)
        };
        let amz_date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let host = self.host_header();
        let auth = authorization(
            &ToSign {
                method: method.as_str(),
                path: &path,
                query,
                headers: &[("host", &host), ("x-amz-content-sha256", &body.sha256), ("x-amz-date", &amz_date)],
                payload_sha256: &body.sha256,
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
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("host", &host)
            .header("x-amz-content-sha256", &body.sha256)
            .header("x-amz-date", &amz_date)
            .header("authorization", auth)
            .header("content-length", body.left)
            .body(body)
            .map_err(s3_err)?;
        let moved = Arc::new(AtomicU64::new(0));
        let stream = tokio::time::timeout(self.limits.connect, self.connect(moved.clone())).await.map_err(|_| {
            s3_err(format!("{what}: no connection to {host} within {} s", self.limits.connect.as_secs_f32()))
        })??;
        let exchange = async {
            let (mut sender, conn) = http1::handshake(TokioIo::new(stream)).await.map_err(s3_err)?;
            let _driver = Driver(tokio::spawn(async move {
                let _ = conn.await;
            }));
            let response = sender.send_request(request).await.map_err(s3_err)?;
            let status = response.status().as_u16();
            let length = response
                .headers()
                .get(hyper::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok());
            // Only what was asked for goes to the file: the text of an error stays here.
            let mut file = match into {
                Some(path) if (200..300).contains(&status) => {
                    Some(File::create(path).map_err(|e| storage_err(format!("cannot write {}: {e}", path.display())))?)
                }
                _ => None,
            };
            let mut incoming = response.into_body();
            let mut body = Vec::new();
            while let Some(frame) = incoming.frame().await {
                let Ok(data) = frame.map_err(s3_err)?.into_data() else { continue };
                match &mut file {
                    Some(file) => file.write_all(&data).map_err(|e| storage_err(format!("{what}: {e}")))?,
                    None => body.extend_from_slice(&data),
                }
            }
            Ok(Answer { status, length, body })
        };
        self.watched(&what, &moved, exchange).await
    }

    /// Waits for `exchange`, a request on a connection that counts in `moved`
    /// the bytes it carries, and gives it up when they stop moving or when it
    /// is slower than the slowest link waited for (see [`Limits`]).
    async fn watched<T>(
        &self,
        what: &str,
        moved: &AtomicU64,
        exchange: impl std::future::Future<Output = Result<T>>,
    ) -> Result<T> {
        let mut exchange = std::pin::pin!(exchange);
        let started = Instant::now();
        // What had moved when it last did, and when that was.
        let mut last = (moved.load(Ordering::Relaxed), started);
        loop {
            if let Ok(done) = tokio::time::timeout(self.limits.stall / 4, &mut exchange).await {
                return done;
            }
            let now = moved.load(Ordering::Relaxed);
            let waited = started.elapsed();
            if now != last.0 {
                last = (now, Instant::now());
            } else if last.1.elapsed() >= self.limits.stall {
                return Err(s3_err(format!(
                    "{what}: {} stopped answering ({now} bytes in {} s)",
                    self.host,
                    waited.as_secs()
                )));
            }
            if waited > self.limits.total + Duration::from_secs(now / SLOWEST) {
                return Err(s3_err(format!(
                    "{what}: the transfer is too slow ({now} bytes in {} s)",
                    waited.as_secs()
                )));
            }
        }
    }

    /// Runs `f` on a private runtime, on its own thread: backups are called
    /// from synchronous code, which may itself run inside another runtime.
    fn block<T: Send>(&self, f: impl std::future::Future<Output = Result<T>> + Send) -> Result<T> {
        let run = || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(s3_err)?;
            let out = runtime.block_on(f);
            // A name lookup that was given up still runs on a thread of the
            // runtime (it cannot be interrupted): it is not waited for.
            runtime.shutdown_background();
            out
        };
        std::thread::scope(|s| match std::thread::Builder::new().name("nuage-s3".into()).spawn_scoped(s, run) {
            Ok(thread) => thread.join().unwrap_or_else(|_| Err(s3_err("the request panicked"))),
            Err(e) => Err(s3_err(format!("cannot start a thread for the request: {e}"))),
        })
    }

    fn check(answer: &Answer, what: &str) -> Result<()> {
        if (200..300).contains(&answer.status) {
            Ok(())
        } else {
            let text = String::from_utf8_lossy(&answer.body);
            let code = xml_values(&text, "Code").into_iter().next().unwrap_or_default();
            Err(s3_err(format!("{what}: HTTP {} {code}", answer.status)))
        }
    }
}

impl BackupStore for S3Store {
    fn describe(&self) -> String {
        format!("s3://{}/{}", self.options.bucket, self.options.prefix)
    }

    fn put(&self, name: &str, data: &[u8]) -> Result<()> {
        let key = self.key(name);
        let answer = self.block(self.send(Method::PUT, &key, &[], Upload::memory(data.to_vec()), None))?;
        Self::check(&answer, &format!("PUT {key}"))
    }

    /// The file is read as it is sent: never whole in memory.
    fn put_file(&self, name: &str, path: &Path) -> Result<()> {
        let key = self.key(name);
        let answer = self.block(self.send(Method::PUT, &key, &[], Upload::file(path)?, None))?;
        Self::check(&answer, &format!("PUT {key}"))
    }

    fn get(&self, name: &str) -> Result<Option<Vec<u8>>> {
        let key = self.key(name);
        let answer = self.block(self.send(Method::GET, &key, &[], Upload::memory(Vec::new()), None))?;
        if answer.status == 404 {
            return Ok(None);
        }
        Self::check(&answer, &format!("GET {key}"))?;
        Ok(Some(answer.body))
    }

    /// The object is written to the file as it comes: never whole in memory.
    fn get_file(&self, name: &str, dest: &Path) -> Result<bool> {
        let key = self.key(name);
        let fetched =
            self.block(self.send(Method::GET, &key, &[], Upload::memory(Vec::new()), Some(dest))).and_then(|answer| {
                match answer.status {
                    404 => Ok(false),
                    _ => Self::check(&answer, &format!("GET {key}")).map(|()| true),
                }
            });
        if !matches!(fetched, Ok(true)) {
            // Not a part of it either.
            let _ = std::fs::remove_file(dest);
        }
        fetched
    }

    fn size(&self, name: &str) -> Result<Option<u64>> {
        let key = self.key(name);
        let answer = self.block(self.send(Method::HEAD, &key, &[], Upload::memory(Vec::new()), None))?;
        match answer.status {
            200 => Ok(answer.length),
            404 => Err(s3_err(format!("HEAD {key}: the object is not there"))),
            // A service that does not answer HEAD cannot tell.
            _ => Ok(None),
        }
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
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader};
    use std::net::{SocketAddr, TcpListener};
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;

    use super::*;
    use crate::{BackupOptions, Embedded, EngineOptions};

    /// An S3 endpoint in memory, a thread per connection: what this client
    /// asks of one. Like the real ones, it refuses a body that is not the one
    /// the request signed.
    struct StandIn {
        addr: SocketAddr,
        objects: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        /// It takes the requests and answers none.
        silent: Arc<AtomicBool>,
    }

    impl StandIn {
        fn start() -> StandIn {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let (objects, silent) = (Arc::<Mutex<HashMap<_, _>>>::default(), Arc::<AtomicBool>::default());
            let (o, s) = (objects.clone(), silent.clone());
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let (o, s) = (o.clone(), s.clone());
                    std::thread::spawn(move || serve(stream, &o, &s));
                }
            });
            StandIn { addr, objects, silent }
        }

        fn store(&self) -> S3Store {
            S3Store::new(S3Options {
                endpoint: format!("http://{}", self.addr),
                bucket: "bucket".into(),
                region: "us-east-1".into(),
                prefix: "prod/".into(),
                access_key_id: "key".into(),
                secret_access_key: "secret".into(),
            })
            .unwrap()
        }

        fn stored(&self, key: &str) -> Option<Vec<u8>> {
            self.objects.lock().unwrap().get(key).cloned()
        }
    }

    /// One request of a connection to a [`StandIn`].
    fn serve(mut stream: std::net::TcpStream, objects: &Mutex<HashMap<String, Vec<u8>>>, silent: &AtomicBool) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let mut parts = line.split_whitespace().map(str::to_owned);
        let (method, target) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default());
        let (mut length, mut digest, mut signed) = (0usize, String::new(), false);
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).unwrap();
            let Some((name, value)) = header.trim_end().split_once(':') else { break };
            match name.to_ascii_lowercase().as_str() {
                "content-length" => length = value.trim().parse().unwrap(),
                "x-amz-content-sha256" => digest = value.trim().to_owned(),
                "authorization" => signed = value.trim().starts_with("AWS4-HMAC-SHA256 Credential=key/"),
                _ => {}
            }
        }
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body).unwrap();
        if silent.load(Ordering::SeqCst) {
            // The connection stays open, without a word, until the client leaves.
            let _ = reader.read(&mut [0u8; 1]);
            return;
        }
        let (path, query) = target.split_once('?').unwrap_or((&target, ""));
        let key = path.trim_start_matches("/bucket").trim_start_matches('/').to_owned();
        let mut objects = objects.lock().unwrap();
        let error = |code: &str| format!("<Error><Code>{code}</Code></Error>").into_bytes();
        // The status, the body, and the length announced (of the object, for HEAD).
        let (status, answer, announced) = match method.as_str() {
            _ if !signed => (403, error("AccessDenied"), None),
            _ if sha256_hex(&body) != digest => (400, error("XAmzContentSHA256Mismatch"), None),
            "PUT" => {
                objects.insert(key, body);
                (200, Vec::new(), None)
            }
            "GET" if key.is_empty() => {
                let prefix =
                    query.split('&').find_map(|kv| kv.strip_prefix("prefix=")).unwrap_or_default().replace("%2F", "/");
                let mut keys: Vec<&String> = objects.keys().filter(|k| k.starts_with(&prefix)).collect();
                keys.sort();
                let xml: String = keys
                    .iter()
                    .map(|k| format!("<Contents><Key>{k}</Key><Size>{}</Size></Contents>", objects[*k].len()))
                    .collect();
                (
                    200,
                    format!("<ListBucketResult>{xml}<IsTruncated>false</IsTruncated></ListBucketResult>").into(),
                    None,
                )
            }
            "GET" => match objects.get(&key) {
                Some(object) => (200, object.clone(), None),
                None => (404, error("NoSuchKey"), None),
            },
            "HEAD" => match objects.get(&key) {
                Some(object) => (200, Vec::new(), Some(object.len())),
                None => (404, Vec::new(), None),
            },
            "DELETE" => {
                objects.remove(&key);
                (204, Vec::new(), None)
            }
            _ => (405, Vec::new(), None),
        };
        let announced = announced.unwrap_or(answer.len());
        let head = format!("HTTP/1.1 {status} -\r\ncontent-length: {announced}\r\nconnection: close\r\n\r\n");
        let _ = stream.write_all(head.as_bytes()).and_then(|()| stream.write_all(&answer));
    }

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nuage-s3-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A file is sent and fetched a piece at a time, and arrives whole: its
    /// digest signs the request, the bucket tells its size, and what comes
    /// back is what was sent.
    #[test]
    fn a_file_travels_whole_both_ways() {
        let s3 = StandIn::start();
        let store = s3.store();
        let dir = temp("file");
        // Several pieces, the last one not full; then no piece at all.
        let data: Vec<u8> = (0..3 * PIECE + 12_345).map(|i| (i * 31 % 251) as u8).collect();
        for (name, data) in [("snapshots/1.db", data.as_slice()), ("snapshots/empty.db", &[])] {
            std::fs::write(dir.join("sent"), data).unwrap();
            store.put_file(name, &dir.join("sent")).unwrap();
            assert!(s3.stored(&format!("prod/{name}")).as_deref() == Some(data), "{name} is stored as it was sent");
            assert_eq!(store.size(name).unwrap(), Some(data.len() as u64));
            assert!(store.get_file(name, &dir.join("back")).unwrap());
            assert!(std::fs::read(dir.join("back")).unwrap() == data, "{name} comes back as it was sent");
            assert!(store.get(name).unwrap().as_deref() == Some(data));
        }
        // An object that is not there: no file, not even an empty one, and no size.
        std::fs::write(dir.join("none"), b"left by an earlier try").unwrap();
        assert!(!store.get_file("snapshots/2.db", &dir.join("none")).unwrap());
        assert!(!dir.join("none").exists());
        assert!(store.size("snapshots/2.db").unwrap_err().to_string().contains("is not there"));
        assert_eq!(store.get("snapshots/2.db").unwrap(), None);
        // Bytes in memory, listing and removal.
        store.put("wal/1.log", b"one").unwrap();
        assert_eq!(store.list("wal/").unwrap(), ["wal/1.log"]);
        assert_eq!(store.list("snapshots/").unwrap(), ["snapshots/1.db", "snapshots/empty.db"]);
        store.delete("wal/1.log").unwrap();
        store.delete("wal/1.log").unwrap();
        assert!(store.list("wal/").unwrap().is_empty());
        // A file that is not there to send is an error, and nothing is stored.
        assert!(store.put_file("snapshots/3.db", &dir.join("missing")).is_err());
        assert_eq!(s3.stored("prod/snapshots/3.db"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A bucket that takes a request and never answers, or a connection that
    /// never finishes its TLS handshake, is an error once its time is up, not
    /// a wait without end; and the next request, answered, succeeds.
    #[test]
    fn a_silent_endpoint_is_an_error_not_a_wait() {
        let s3 = StandIn::start();
        let mut store = s3.store();
        store.limits.stall = Duration::from_millis(300);
        let dir = temp("silent");
        std::fs::write(dir.join("sent"), vec![7u8; PIECE + 1]).unwrap();
        s3.silent.store(true, Ordering::SeqCst);
        let started = Instant::now();
        for answer in [
            store.put("manifest.json", b"{}"),
            store.put_file("snapshots/1.db", &dir.join("sent")),
            store.get("manifest.json").map(|_| ()),
            store.get_file("snapshots/1.db", &dir.join("back")).map(|_| ()),
            store.list("wal/").map(|_| ()),
            store.delete("manifest.json"),
            store.size("manifest.json").map(|_| ()),
        ] {
            let e = answer.unwrap_err().to_string();
            assert!(e.contains("stopped answering"), "{e}");
        }
        assert!(started.elapsed() < Duration::from_secs(20), "{:?}", started.elapsed());
        assert!(!dir.join("back").exists());
        s3.silent.store(false, Ordering::SeqCst);
        let store = s3.store();
        store.put("manifest.json", b"{}").unwrap();
        assert_eq!(store.get("manifest.json").unwrap(), Some(b"{}".to_vec()));

        // Connections are accepted (by the system) and nothing ever answers the TLS hello.
        let mute = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut tls = S3Store::new(S3Options {
            endpoint: format!("https://{}", mute.local_addr().unwrap()),
            ..store.options.clone()
        })
        .unwrap();
        tls.limits.connect = Duration::from_millis(300);
        let started = Instant::now();
        let e = tls.put("manifest.json", b"{}").unwrap_err().to_string();
        assert!(e.contains("no connection to 127.0.0.1"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A transfer that keeps moving is waited for past the time a request
    /// has without a body, as long as its size takes at the slowest speed
    /// waited for; a slower one is given up.
    #[test]
    fn a_transfer_is_given_the_time_its_size_takes() {
        let store = StandIn::start().store();
        let limits = Limits { stall: Duration::from_secs(5), total: Duration::from_millis(200), ..LIMITS };
        let store = S3Store { limits, ..store };
        let run = |pace: Duration, step: u64, steps: u64| {
            let moved = AtomicU64::new(0);
            let transfer = async {
                for _ in 0..steps {
                    tokio::time::sleep(pace).await;
                    moved.fetch_add(step, Ordering::Relaxed);
                }
                Ok(())
            };
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            runtime.block_on(store.watched("PUT snapshots/1.db", &moved, transfer))
        };
        // A second in all, five times what a request without a body has: its bytes pay for it.
        run(Duration::from_millis(20), SLOWEST, 50).unwrap();
        // It moves, a few bytes at a time: far too slowly to ever end.
        let e = run(Duration::from_millis(20), 1, 50).unwrap_err().to_string();
        assert!(e.contains("too slow"), "{e}");
    }

    /// A backup to a bucket that went silent fails and says so, instead of
    /// holding the upkeep thread; once the bucket answers again the next one
    /// goes through, and an empty directory restored from the bucket holds
    /// every row.
    #[test]
    fn a_backup_to_a_silent_bucket_fails_then_the_next_one_succeeds() {
        let s3 = StandIn::start();
        let mut store = s3.store();
        store.limits.stall = Duration::from_secs(1);
        let store = Arc::new(store);
        let options = || EngineOptions {
            backup: Some(BackupOptions::new(store.clone())),
            sync: false,
            ..EngineOptions::default()
        };
        let (src, dst) = (temp("backup-src"), temp("backup-dst"));
        let insert = |db: &Embedded, ids: std::ops::Range<i64>| {
            for id in ids {
                db.sql("INSERT INTO t (id, v) VALUES (?, ?)", &[id.into(), format!("row {id}").into()]).unwrap();
            }
        };
        {
            let db = Embedded::open_with(&src, options()).unwrap();
            db.sql("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[]).unwrap();
            insert(&db, 0..50);
            db.engine().backup_now(true).unwrap();
            s3.silent.store(true, Ordering::SeqCst);
            insert(&db, 50..100);
            db.engine().checkpoint().unwrap();
            let started = Instant::now();
            let e = db.engine().backup_now(false).unwrap_err().to_string();
            assert!(e.contains("stopped answering"), "{e}");
            assert!(started.elapsed() < Duration::from_secs(20), "{:?}", started.elapsed());
            let status = db.engine().backup_status();
            assert!(!status.last_ok && status.last_error.is_some_and(|e| e.contains("stopped answering")));
            s3.silent.store(false, Ordering::SeqCst);
            db.engine().backup_now(false).unwrap();
            assert!(db.engine().backup_status().last_ok);
        }
        let db = Embedded::open_with(&dst, options()).unwrap();
        assert!(db.engine().backup_status().restored);
        let rows = &db.sql("SELECT count(*), min(v), max(id) FROM t", &[]).unwrap()[0].rows[0];
        assert_eq!(format!("{rows:?}"), r#"[I64(100), Text("row 0"), I32(99)]"#);
        drop(db);
        for dir in [src, dst] {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

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

    #[test]
    fn xml_and_encoding() {
        let xml = "<ListBucketResult><Contents><Key>a/b&amp;c</Key></Contents><Contents><Key>d</Key></Contents><IsTruncated>false</IsTruncated></ListBucketResult>";
        assert_eq!(xml_values(xml, "Key"), vec!["a/b&c", "d"]);
        assert_eq!(uri_encode("wal/0001 x+y.log", true), "wal/0001%20x%2By.log");
    }
}
