//! The status API of the Nuage DB server (`GET /api/status`, `GET /api/status/stream`), as
//! read by the dashboard, and the small formatting helpers it shows them with.
//!
//! Every field but `status` and `services` is optional: without credentials, a server with
//! `NUAGE_PUBLIC_STATUS=true` only sends the public subset (status, uptime, start time and
//! the name and status of each service).

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Status {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// `ok`, `degraded` or `down`.
    #[serde(default)]
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uptime_s: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests: Option<Requests>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connections: Option<Connections>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<Storage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wal: Option<Wal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<Backup>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption: Option<Encryption>,
    /// Applications on the server (each has a database of its own).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applications: Option<u64>,
    /// The applications in view: all of them for a console session, its own for an
    /// application's token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub databases: Option<Vec<Database>>,
    #[serde(default)]
    pub services: Vec<Service>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Encryption {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub scheme: Option<String>,
    /// `key` or `passphrase`.
    #[serde(default)]
    pub secret: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Requests {
    #[serde(default)]
    pub per_s: f64,
    #[serde(default)]
    pub total: u64,
    #[serde(default)]
    pub errors_per_s: f64,
    #[serde(default)]
    pub p50_ms: f64,
    #[serde(default)]
    pub p99_ms: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Connections {
    #[serde(default)]
    pub active: u64,
    #[serde(default)]
    pub idle: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Storage {
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub disk_free_bytes: u64,
    #[serde(default)]
    pub pages: u64,
    #[serde(default)]
    pub cache_bytes: u64,
    #[serde(default)]
    pub cache_capacity_bytes: u64,
    #[serde(default)]
    pub cache_hit_rate: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Wal {
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub last_checkpoint_at: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Backup {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub last_at: Option<String>,
    #[serde(default)]
    pub last_ok: Option<bool>,
    #[serde(default)]
    pub last_error: Option<String>,
}

/// An application and its database.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Database {
    pub name: String,
    /// The server's own database (the first application).
    #[serde(default)]
    pub first: bool,
    #[serde(default)]
    pub description: Option<String>,
    /// `ok` or `degraded` (its backups fail).
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub encrypted: Option<bool>,
    #[serde(default)]
    pub file_bytes: u64,
    #[serde(default)]
    pub tokens: u64,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub last_request_at: Option<String>,
    /// Its own requests (the `requests` of the status are the whole server's).
    #[serde(default)]
    pub requests: Option<Requests>,
    #[serde(default)]
    pub tables: u64,
    #[serde(default)]
    pub rows: u64,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub branches: Vec<Branch>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Branch {
    pub name: String,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub own_bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Service {
    pub name: String,
    #[serde(default)]
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// An `event: log` line of the stream.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LogLine {
    #[serde(default)]
    pub at: String,
    /// `info`, `warn` or `error`.
    #[serde(default)]
    pub level: String,
    #[serde(default)]
    pub message: String,
    /// The application the event is about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
}

impl Status {
    /// Only the public subset (no request, storage or backup figures).
    pub fn is_public(&self) -> bool {
        self.requests.is_none() && self.storage.is_none() && self.databases.is_none()
    }

    /// The public subset of this status: what an anonymous visitor may see.
    pub fn public(&self) -> Status {
        Status {
            status: self.status.clone(),
            uptime_s: self.uptime_s,
            started_at: self.started_at.clone(),
            services: self
                .services
                .iter()
                .map(|s| Service { name: s.name.clone(), status: s.status.clone(), detail: None })
                .collect(),
            ..Status::default()
        }
    }
}

/// `1.5 MB`, `52.4 MB`, `1.07 GB` (powers of 1000, like disk vendors and file managers).
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "kB", "MB", "GB", "TB", "PB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else if value >= 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

/// `97 %`, `99.5 %` from a ratio.
pub fn percent(ratio: f64) -> String {
    let p = (ratio * 100.0).clamp(0.0, 100.0);
    if p >= 99.95 || (p - p.round()).abs() < 0.05 { format!("{p:.0} %") } else { format!("{p:.1} %") }
}

/// `0.80 ms`, `4.2 ms`, `120 ms`.
pub fn millis(ms: f64) -> String {
    if ms < 1.0 {
        format!("{ms:.2} ms")
    } else if ms < 100.0 {
        format!("{ms:.1} ms")
    } else {
        format!("{ms:.0} ms")
    }
}

/// `12.5`, `1 234`, `0` (requests per second).
pub fn rate(per_s: f64) -> String {
    if per_s >= 100.0 { group(per_s.round() as u64) } else { format!("{per_s:.1}") }
}

/// `1 234 567`: thousands separated by a thin space.
pub fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 * 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push('\u{202f}');
        }
        out.push(c);
    }
    out
}

/// `3 d 4 h`, `5 h 12 min`, `42 min`, `30 s` (`day`: the unit of days, `d` or `j`).
pub fn duration(seconds: u64, day: &str) -> String {
    let (d, h, m) = (seconds / 86_400, seconds % 86_400 / 3600, seconds % 3600 / 60);
    if d > 0 {
        format!("{d} {day} {h} h")
    } else if h > 0 {
        format!("{h} h {m} min")
    } else if m > 0 {
        format!("{m} min")
    } else {
        format!("{seconds} s")
    }
}

/// Milliseconds since the Unix epoch of an RFC 3339 time (`2026-11-13T11:59:00Z`,
/// `2026-11-13T11:59:00.250+01:00`), `None` if it is not one.
pub fn parse_time(s: &str) -> Option<f64> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    let num = |range: std::ops::Range<usize>| s.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let mut rest = &s[19..];
    let mut fraction = 0.0;
    if let Some(after) = rest.strip_prefix('.') {
        let end = after.find(|c: char| !c.is_ascii_digit()).unwrap_or(after.len());
        fraction = format!("0.{}", &after[..end]).parse().ok()?;
        rest = &after[end..];
    }
    let offset = match rest {
        "Z" | "z" | "" => 0,
        tz if tz.len() == 6 && (tz.starts_with('+') || tz.starts_with('-')) => {
            let sign = if tz.starts_with('-') { -1 } else { 1 };
            sign * (tz.get(1..3)?.parse::<i64>().ok()? * 3600 + tz.get(4..6)?.parse::<i64>().ok()? * 60)
        }
        _ => return None,
    };
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second - offset;
    Some((secs as f64 + fraction) * 1000.0)
}

/// An RFC 3339 time (`2026-11-13T11:59:00Z`) of milliseconds since the Unix epoch.
pub fn format_time(ms: f64) -> String {
    let secs = (ms / 1000.0).floor() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// `HH:MM:SS` (UTC) of an RFC 3339 time, for the event log.
pub fn clock(time: &str) -> String {
    match time.find('T') {
        Some(t) if time.len() >= t + 9 => time[t + 1..t + 9].to_string(),
        _ => time.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "name": "Nuage DB", "version": "0.2.1", "status": "ok", "uptime_s": 86400,
      "started_at": "2026-11-13T10:00:00Z",
      "requests": { "per_s": 12.5, "total": 123456, "errors_per_s": 0.0, "p50_ms": 0.8, "p99_ms": 4.2 },
      "connections": { "active": 3, "idle": 5 },
      "storage": { "bytes": 52428800, "disk_free_bytes": 1073741824, "pages": 12800,
                   "cache_bytes": 33554432, "cache_capacity_bytes": 67108864, "cache_hit_rate": 0.97 },
      "wal": { "bytes": 1048576, "last_checkpoint_at": "2026-11-13T11:59:00Z" },
      "backup": { "enabled": true, "target": "s3://bucket/prefix", "last_at": "2026-11-13T11:55:00Z",
                  "last_ok": true, "last_error": null },
      "encryption": { "enabled": true, "scheme": "XTS-AES-256 (pages), AES-256-GCM (log)", "secret": "key" },
      "applications": 2,
      "databases": [
        { "name": "nuage", "first": true, "encrypted": true, "tables": 12, "rows": 34567, "bytes": 41943040,
          "requests": { "per_s": 10.0, "total": 100000, "errors_per_s": 0.0, "p50_ms": 0.7, "p99_ms": 3.9 },
          "branches": [ { "name": "preview-42", "parent": "nuage", "created_at": "2026-11-13T09:00:00Z",
                          "own_bytes": 65536 } ] },
        { "name": "shop", "description": "La boutique", "encrypted": true, "tables": 3, "rows": 120,
          "bytes": 65536, "tokens": 2, "last_request_at": "2026-11-13T11:59:58Z", "branches": [] }
      ],
      "services": [
        { "name": "engine", "status": "ok", "detail": "12 800 pages, cache 97 %" },
        { "name": "http", "status": "ok", "detail": "12.5 req/s" },
        { "name": "wal", "status": "ok", "detail": "checkpoint 1 min ago" },
        { "name": "backup", "status": "ok", "detail": "last 5 min ago" },
        { "name": "compaction", "status": "ok", "detail": "idle" }
      ]
    }"#;

    #[test]
    fn reads_the_contract_sample_and_its_public_subset() {
        let status: Status = serde_json::from_str(SAMPLE).unwrap();
        assert_eq!(status.requests.as_ref().unwrap().p99_ms, 4.2);
        assert_eq!(status.databases.as_ref().unwrap()[0].branches[0].own_bytes, 65536);
        assert_eq!(status.databases.as_ref().unwrap()[1].description.as_deref(), Some("La boutique"));
        assert!(status.encryption.as_ref().unwrap().enabled);
        assert_eq!(status.applications, Some(2));
        assert_eq!(status.services.len(), 5);
        assert!(!status.is_public());
        let public = status.public();
        assert!(public.is_public());
        let json = serde_json::to_string(&public).unwrap();
        let back: Status = serde_json::from_str(&json).unwrap();
        assert_eq!(back, public);
        assert!(!json.contains("detail") && !json.contains("requests"), "{json}");
        let minimal: Status =
            serde_json::from_str(r#"{"status":"down","services":[{"name":"engine","status":"down"}]}"#).unwrap();
        assert!(minimal.is_public());
    }

    #[test]
    fn formats_values() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(52_428_800), "52.4 MB");
        assert_eq!(bytes(1_073_741_824), "1.07 GB");
        assert_eq!(percent(0.97), "97 %");
        assert_eq!(percent(0.9734), "97.3 %");
        assert_eq!(millis(0.8), "0.80 ms");
        assert_eq!(millis(4.2), "4.2 ms");
        assert_eq!(group(123_456), "123\u{202f}456");
        assert_eq!(duration(86_400 + 7200, "d"), "1 d 2 h");
        assert_eq!(duration(86_400, "j"), "1 j 0 h");
        assert_eq!(duration(42, "d"), "42 s");
        assert_eq!(rate(12.5), "12.5");
    }

    #[test]
    fn parses_and_formats_times() {
        let t = parse_time("2026-11-13T11:59:00Z").unwrap();
        assert_eq!(format_time(t), "2026-11-13T11:59:00Z");
        assert_eq!(parse_time("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(parse_time("1970-01-01T01:00:00+01:00"), Some(0.0));
        assert_eq!(parse_time("2026-11-13T11:59:00.500Z"), Some(t + 500.0));
        assert_eq!(parse_time("2000-02-29T00:00:00Z").map(format_time).as_deref(), Some("2000-02-29T00:00:00Z"));
        assert_eq!(parse_time("yesterday"), None);
        assert_eq!(clock("2026-11-13T11:59:07Z"), "11:59:07");
    }
}
