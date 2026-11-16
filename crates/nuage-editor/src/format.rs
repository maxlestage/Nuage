//! Figures as a person reads them, in the editor's language: amounts of
//! memory as the console's settings page writes them (Mio and Gio in French,
//! a decimal comma outside English), durations, and dates in this
//! computer's time zone.

use chrono::TimeZone as _;

use crate::i18n::{Lang, t, tf};

/// An amount of memory: `0.50 GiB`, `128 MiB` (`Gio`, `Mio` in French; a
/// decimal comma outside English).
pub fn bytes(l: Lang, n: u64) -> String {
    let (mib, gib) = units(l);
    let text = if n >= 1 << 30 {
        format!("{:.2} {gib}", n as f64 / (1u64 << 30) as f64)
    } else if n >= 10 << 20 {
        format!("{} {mib}", n >> 20)
    } else {
        format!("{:.1} {mib}", n as f64 / (1u64 << 20) as f64)
    };
    comma(l, text)
}

/// An amount of memory in GiB with one decimal (`0.8 GiB`), or in MiB under
/// a tenth of a GiB, where GiB would say nothing (`48 MiB`).
pub fn gib(l: Lang, n: u64) -> String {
    if n < 100 << 20 {
        return bytes(l, n);
    }
    let (_, gib) = units(l);
    comma(l, format!("{:.1} {gib}", n as f64 / (1u64 << 30) as f64))
}

fn units(l: Lang) -> (&'static str, &'static str) {
    if l == Lang::Fr { ("Mio", "Gio") } else { ("MiB", "GiB") }
}

fn comma(l: Lang, text: String) -> String {
    if l == Lang::En { text } else { text.replace('.', ",") }
}

/// An amount set by its owner, in MiB, as a field shows it: `512 MiB`,
/// `2 GiB` (in GiB when that says it exactly), or `auto`.
pub fn amount(l: Lang, mib: Option<u64>) -> String {
    let Some(mib) = mib else { return "auto".into() };
    let (m, g) = units(l);
    if mib >= 1024 && (mib * 100).is_multiple_of(1024) {
        let hundredths = mib * 100 / 1024;
        let text = match (hundredths / 100, hundredths % 100) {
            (whole, 0) => whole.to_string(),
            (whole, part) if part % 10 == 0 => format!("{whole}.{}", part / 10),
            (whole, part) => format!("{whole}.{part:02}"),
        };
        format!("{} {g}", comma(l, text))
    } else {
        format!("{mib} {m}")
    }
}

/// A number with its thousands apart (`1 203`, `1,203` in English).
pub fn count(l: Lang, n: u64) -> String {
    let digits = n.to_string();
    let sep = if l == Lang::En { ',' } else { '\u{a0}' };
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(sep);
        }
        out.push(c);
    }
    out
}

/// Whether `n` takes the singular: below 2 in French, 1 only in English and Spanish.
pub fn singular(l: Lang, n: u64) -> bool {
    if l == Lang::Fr { n < 2 } else { n == 1 }
}

/// A length of time: `40 s`, `12 min`, `5 h`, `3 days`.
pub fn span(l: Lang, seconds: u64) -> String {
    match seconds {
        0..60 => tf(l, "{} s", &[&seconds]),
        60..3600 => tf(l, "{} min", &[&(seconds / 60)]),
        3600..86_400 => tf(l, "{} h", &[&(seconds / 3600)]),
        _ => {
            let days = seconds / 86_400;
            if singular(l, days) { tf(l, "{} day", &[&days]) } else { tf(l, "{} days", &[&days]) }
        }
    }
}

/// How long ago `ms` (Unix milliseconds) was, from `now`: `4 min ago`.
pub fn ago(l: Lang, ms: i64, now: i64) -> String {
    let seconds = u64::try_from(now.saturating_sub(ms) / 1000).unwrap_or(0);
    if seconds < 5 {
        return t(l, "just now").to_owned();
    }
    tf(l, "{} ago", &[&span(l, seconds)])
}

/// Now, in Unix milliseconds.
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// An RFC 3339 time (`2026-11-15T21:04:00Z`, as the server writes them) in Unix milliseconds.
pub fn parse_time(text: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(text.trim()).ok().map(|t| t.timestamp_millis())
}

fn local(ms: i64) -> Option<chrono::DateTime<chrono::Local>> {
    chrono::Local.timestamp_millis_opt(ms).single()
}

/// A day, in this computer's time zone: `15/11/2026` in French and Spanish, `15 Nov 2026` in English.
pub fn date(l: Lang, ms: i64) -> String {
    let Some(t) = local(ms) else { return String::new() };
    match l {
        Lang::En => t.format("%-d %b %Y").to_string(),
        Lang::Fr | Lang::Es => t.format("%d/%m/%Y").to_string(),
    }
}

/// A time of day, in this computer's time zone: `21:04`.
pub fn clock(ms: i64) -> String {
    local(ms).map(|t| t.format("%H:%M").to_string()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn figures() {
        assert_eq!(bytes(Lang::En, 512 << 20), "512 MiB");
        assert_eq!(bytes(Lang::Fr, 1 << 29), "512 Mio");
        assert_eq!(bytes(Lang::Fr, 3 << 29), "1,50 Gio");
        assert_eq!(bytes(Lang::Es, 5 << 19), "2,5 MiB");
        assert_eq!(gib(Lang::En, 16 << 30), "16.0 GiB");
        assert_eq!(gib(Lang::Fr, 858_993_459), "0,8 Gio");
        assert_eq!(gib(Lang::Es, 48 << 20), "48 MiB");
        assert_eq!(amount(Lang::En, None), "auto");
        assert_eq!(amount(Lang::En, Some(2048)), "2 GiB");
        assert_eq!(amount(Lang::Fr, Some(1536)), "1,5 Gio");
        assert_eq!(amount(Lang::Es, Some(300)), "300 MiB");
        assert_eq!(count(Lang::Fr, 1203), "1\u{a0}203");
        assert_eq!(count(Lang::En, 1_234_567), "1,234,567");
        assert_eq!(count(Lang::Es, 12), "12");
    }

    #[test]
    fn times() {
        assert!(singular(Lang::Fr, 0) && singular(Lang::Fr, 1) && !singular(Lang::Fr, 2));
        assert!(!singular(Lang::En, 0) && singular(Lang::Es, 1));
        assert_eq!(span(Lang::En, 40), "40 s");
        assert_eq!(span(Lang::Fr, 3 * 86_400 + 5), "3 jours");
        assert_eq!(span(Lang::Fr, 86_400), "1 jour");
        assert_eq!(span(Lang::Es, 7200), "2 h");
        assert_eq!(ago(Lang::Fr, 0, 240_000), "il y a 4 min");
        assert_eq!(ago(Lang::Es, 0, 240_000), "hace 4 min");
        assert_eq!(ago(Lang::En, 0, 240_000), "4 min ago");
        assert_eq!(ago(Lang::Fr, 0, 2_000), "à l'instant", "never « il y a 0 s »");
        assert_eq!(parse_time("2026-11-15T21:04:00Z"), Some(1_794_776_640_000));
        assert_eq!(parse_time("yesterday"), None);
        let noon = parse_time("2026-11-12T12:00:00Z").unwrap();
        // Noon UTC is 12 November in every time zone from UTC-11 to UTC+11.
        assert!(date(Lang::Fr, noon).ends_with("/11/2026") && date(Lang::Fr, noon).len() == 10);
        assert!(date(Lang::En, noon).ends_with("Nov 2026"), "{}", date(Lang::En, noon));
        assert_eq!(clock(noon).len(), 5);
    }
}
