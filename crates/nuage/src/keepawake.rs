//! Keeping a server awake on hosts that put idle servers to sleep (Heroku Eco
//! dynos sleep after 30 minutes without a request, and the next request
//! waits for them to start again, seconds).
//!
//! With `NUAGE_KEEP_AWAKE_URL` (the server's own public address, such as
//! `https://my-nuage.herokuapp.com/api/health`), the server requests it every
//! `NUAGE_KEEP_AWAKE_MINUTES` (default 20): through the host's router, which
//! counts it as traffic. `NUAGE_KEEP_AWAKE_HOURS=6-22` limits it to those
//! hours (UTC, the end excluded; `22-6` spans midnight): outside them the
//! server may sleep, and the first visitor after wakes it.
//!
//! A server asleep cannot wake itself: this only keeps an awake one awake.

use std::sync::Arc;
use std::time::Duration;

use http_body_util::{BodyExt, Empty};
use hyper::Request;
use hyper::body::Bytes;
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use memoire_orm::{Error, Result};
use tokio::net::TcpStream;

/// What to request, how often, and when.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeepAwake {
    /// `https://host/path` (or `http://`).
    pub url: String,
    /// Time between two requests: under the host's idle limit.
    pub every: Duration,
    /// Hours of the day (UTC) when it runs: `[start, end)`, `None`: always.
    pub hours: Option<(u8, u8)>,
}

fn invalid(m: impl Into<String>) -> Error {
    Error::Validation(m.into())
}

impl KeepAwake {
    /// From `NUAGE_KEEP_AWAKE_URL`, `_MINUTES` and `_HOURS`.
    pub fn from_vars(url: &str, minutes: Option<&str>, hours: Option<&str>) -> Result<KeepAwake> {
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err(invalid(format!("NUAGE_KEEP_AWAKE_URL=`{url}` is not an http(s) URL")));
        }
        let minutes: u64 = match minutes {
            Some(m) => m.parse().ok().filter(|m| (1..=1440).contains(m)).ok_or_else(|| {
                invalid(format!("NUAGE_KEEP_AWAKE_MINUTES=`{m}` is not a number of minutes (1 to 1440)"))
            })?,
            None => 20,
        };
        let hours = match hours {
            Some(h) => {
                let bad = || invalid(format!("NUAGE_KEEP_AWAKE_HOURS=`{h}`: write `6-22` (UTC hours, 0 to 24)"));
                let (a, b) = h.split_once('-').ok_or_else(bad)?;
                let (a, b): (u8, u8) = (a.trim().parse().map_err(|_| bad())?, b.trim().parse().map_err(|_| bad())?);
                if a > 24 || b > 24 || a == b {
                    return Err(bad());
                }
                Some((a % 24, b % 24))
            }
            None => None,
        };
        Ok(KeepAwake { url: url.to_owned(), every: Duration::from_secs(minutes * 60), hours })
    }

    /// It runs at `hour` (UTC).
    pub fn runs_at(&self, hour: u8) -> bool {
        match self.hours {
            None => true,
            Some((a, b)) if a < b => (a..b).contains(&hour),
            Some((a, b)) => hour >= a || hour < b,
        }
    }

    /// Requests the URL every `every`, in the hours it runs, until the task
    /// is dropped; `log` hears of failures (and of the first success).
    pub async fn run(self, log: impl Fn(&str) + Send + 'static) {
        let mut ok_once = false;
        loop {
            tokio::time::sleep(self.every).await;
            let hour = chrono::Timelike::hour(&chrono::Utc::now()) as u8;
            if !self.runs_at(hour) {
                continue;
            }
            match tokio::time::timeout(Duration::from_secs(30), get(&self.url)).await {
                Ok(Ok(status)) if status < 500 => {
                    if !ok_once {
                        log(&format!("keep-awake: {} answers, every {} min", self.url, self.every.as_secs() / 60));
                        ok_once = true;
                    }
                }
                Ok(Ok(status)) => log(&format!("keep-awake: {} answered {status}", self.url)),
                Ok(Err(e)) => log(&format!("keep-awake: {e}")),
                Err(_) => log(&format!("keep-awake: {} did not answer within 30 s", self.url)),
            }
        }
    }
}

/// `GET url`: its status.
async fn get(url: &str) -> Result<u16> {
    fetch(url, None).await.map(|(status, _)| status)
}

/// `GET url`, with a bearer token: its status and body.
pub(crate) async fn fetch(url: &str, token: Option<&str>) -> Result<(u16, Vec<u8>)> {
    let err = |e: &dyn std::fmt::Display| Error::Pool(format!("cannot reach {url}: {e}"));
    let (https, rest) = match url.split_once("://") {
        Some(("https", r)) => (true, r),
        Some(("http", r)) => (false, r),
        _ => return Err(err(&"not an http(s) URL")),
    };
    let (authority, path) = rest.find('/').map_or((rest, "/"), |i| (&rest[..i], &rest[i..]));
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().map_err(|_| err(&"invalid port"))?),
        None => (authority, if https { 443 } else { 80 }),
    };
    let mut request =
        Request::get(path).header("host", authority).header("user-agent", concat!("nuage/", env!("CARGO_PKG_VERSION")));
    if let Some(t) = token {
        request = request.header("authorization", format!("Bearer {t}"));
    }
    let request = request.body(Empty::<Bytes>::new()).map_err(|e| err(&e))?;
    let tcp = TcpStream::connect((host, port)).await.map_err(|e| err(&e))?;
    let response = if https {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
        let mut config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| err(&e))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let name = rustls::pki_types::ServerName::try_from(host.to_owned()).map_err(|e| err(&e))?;
        let stream =
            tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, tcp).await.map_err(|e| err(&e))?;
        let (mut sender, conn) = http1::handshake(TokioIo::new(stream)).await.map_err(|e| err(&e))?;
        tokio::spawn(conn);
        sender.send_request(request).await.map_err(|e| err(&e))?
    } else {
        let (mut sender, conn) = http1::handshake(TokioIo::new(tcp)).await.map_err(|e| err(&e))?;
        tokio::spawn(conn);
        sender.send_request(request).await.map_err(|e| err(&e))?
    };
    let status = response.status().as_u16();
    let body = response.into_body().collect().await.map(|b| b.to_bytes().to_vec()).unwrap_or_default();
    Ok((status, body))
}

/// `nuage check [url] [token]`: does a connection to a Nuage DB server work?
/// One line of answer, and how to fix what does not.
pub async fn check(url: &str, token: &str) -> std::result::Result<String, String> {
    let base = url.trim_end_matches('/');
    let started = std::time::Instant::now();
    let (status, body) = fetch(&format!("{base}/v1/ping"), Some(token)).await.map_err(|e| {
        let e = e.to_string();
        let e = e.trim_start_matches("connection pool error: ");
        format!("{e}\n  Is the server running, and is {base} its address (scheme, host, port)? Try {base}/api/health in a browser.")
    })?;
    let ms = started.elapsed().as_millis();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
    let message = json["message"].as_str().or_else(|| json["error"]["message"].as_str()).unwrap_or_default().to_owned();
    match status {
        200 => Ok(format!("{message} ({ms} ms)")),
        401 => Err(format!("{base} answered, but refused the token: {message}")),
        404 => Err(format!(
            "{base} answered 404: it is not a Nuage DB server of version 0.4 or later, or not this address."
        )),
        s => Err(format!("{base} answered {s}: {message}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_and_hours() {
        let k = KeepAwake::from_vars("https://x.herokuapp.com/api/health", None, None).unwrap();
        assert_eq!((k.every, k.hours), (Duration::from_secs(1200), None));
        assert!((0..24).all(|h| k.runs_at(h)));
        let day = KeepAwake::from_vars("https://x/api/health", Some("15"), Some("6-22")).unwrap();
        assert_eq!(day.every, Duration::from_secs(900));
        assert!(day.runs_at(6) && day.runs_at(21) && !day.runs_at(22) && !day.runs_at(3));
        let night = KeepAwake::from_vars("https://x/", None, Some("22-6")).unwrap();
        assert!(night.runs_at(23) && night.runs_at(0) && night.runs_at(5) && !night.runs_at(6) && !night.runs_at(12));
        let until_midnight = KeepAwake::from_vars("https://x/", None, Some("7-24")).unwrap();
        assert!(until_midnight.runs_at(7) && until_midnight.runs_at(23) && !until_midnight.runs_at(0));
        for (url, m, h) in [("x.com", None, None), ("https://x", Some("0"), None), ("https://x", None, Some("6"))] {
            assert!(KeepAwake::from_vars(url, m, h).is_err(), "{url} {m:?} {h:?}");
        }
    }

    #[tokio::test]
    async fn requests_a_server() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let n = s.read(&mut buf).unwrap();
            s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok").unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });
        assert_eq!(get(&format!("http://{addr}/api/health")).await.unwrap(), 200);
        assert!(server.join().unwrap().starts_with("GET /api/health HTTP/1.1"));
        assert!(get("http://127.0.0.1:1/").await.is_err());
    }
}
