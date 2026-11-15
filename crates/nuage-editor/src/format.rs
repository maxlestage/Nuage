//! Figures as a person reads them, in the editor's language.

use crate::i18n::Lang;

/// An amount of memory: `0.50 GiB`, `128 MiB` (`Gio`, `Mio` in French; a
/// decimal comma outside English).
pub fn bytes(l: Lang, n: u64) -> String {
    let (mib, gib) = if l == Lang::Fr { ("Mio", "Gio") } else { ("MiB", "GiB") };
    let text = if n >= 1 << 30 {
        format!("{:.2} {gib}", n as f64 / (1u64 << 30) as f64)
    } else if n >= 10 << 20 {
        format!("{} {mib}", n >> 20)
    } else {
        format!("{:.1} {mib}", n as f64 / (1u64 << 20) as f64)
    };
    if l == Lang::En { text } else { text.replace('.', ",") }
}

/// An amount set by its owner, in MiB, as a field shows it: `512 MiB`,
/// `2 GiB` (in GiB when that says it exactly), or `auto`.
pub fn amount(l: Lang, mib: Option<u64>) -> String {
    let Some(mib) = mib else { return "auto".into() };
    let (m, g) = if l == Lang::Fr { ("Mio", "Gio") } else { ("MiB", "GiB") };
    if mib >= 1024 && (mib * 100).is_multiple_of(1024) {
        let hundredths = mib * 100 / 1024;
        let text = match (hundredths / 100, hundredths % 100) {
            (whole, 0) => whole.to_string(),
            (whole, part) if part % 10 == 0 => format!("{whole}.{}", part / 10),
            (whole, part) => format!("{whole}.{part:02}"),
        };
        format!("{} {g}", if l == Lang::En { text } else { text.replace('.', ",") })
    } else {
        format!("{mib} {m}")
    }
}

/// A day, from Unix milliseconds: `2026-11-15`.
pub fn date(ms: i64) -> String {
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let days = ms.div_euclid(86_400_000);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
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
        assert_eq!(amount(Lang::En, None), "auto");
        assert_eq!(amount(Lang::En, Some(2048)), "2 GiB");
        assert_eq!(amount(Lang::Fr, Some(1536)), "1,5 Gio");
        assert_eq!(amount(Lang::Es, Some(300)), "300 MiB");
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(1_794_773_779_008), "2026-11-15");
        assert_eq!(date(951_782_400_000), "2000-02-29");
    }
}
