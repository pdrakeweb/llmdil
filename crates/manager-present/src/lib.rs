//! Number presentation: the canonical rounding rules for every
//! user-facing quantity in the app (wizard messages, logs, and — by
//! mirror — the TypeScript UI, whose `trim1`/`fmtGb` in
//! `ui/src/charts.ts` implement the same rules).
//!
//! The rules (Pete, 2026-10-15: raw floats like 15.9921875 GB are not
//! a useful presentation):
//! - At most one decimal place; a trailing ".0" is dropped
//!   (16.0 -> "16", 15.1 -> "15.1").
//! - Magnitudes >= 100 round to whole numbers.
//! - Counts under 10,000 are exact with thousands separators; larger
//!   counts use K/M/B suffixes under the same one-decimal rule.
//! - Percents are whole numbers. Latencies under a second are whole
//!   milliseconds; longer ones are seconds under the one-decimal rule.
//! - Non-finite values render as an em dash rather than "NaN"/"inf".

/// Core rule: one decimal at most, trailing ".0" dropped, whole
/// numbers at magnitude >= 100.
pub fn trim1(v: f64) -> String {
    if !v.is_finite() {
        return "\u{2014}".to_string();
    }
    if v.abs() >= 100.0 {
        return format!("{}", v.round() as i64);
    }
    let s = format!("{v:.1}");
    s.strip_suffix(".0").map(str::to_string).unwrap_or(s)
}

/// A quantity already expressed in GB/GiB: `gb(15.9921875) == "16"`,
/// `gb(15.1) == "15.1"`. Callers append the unit.
pub fn gb(v: f64) -> String {
    trim1(v)
}

/// A rate (tokens/s, requests/s): same one-decimal rule as [`gb`].
pub fn rate(v: f64) -> String {
    trim1(v)
}

/// Byte count scaled by 1024 with a unit suffix: `bytes(4_635_476)`
/// is "4.4 MB"; whole bytes under 1 KB stay exact.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    format!("{} {}", trim1(v), UNITS[u])
}

/// MiB quantity: exact MiB under 1 GiB, GiB (one-decimal rule) above.
pub fn mib(n: u64) -> String {
    if n < 1024 {
        format!("{n} MiB")
    } else {
        format!("{} GiB", trim1(n as f64 / 1024.0))
    }
}

/// A count: exact with thousands separators under 10,000
/// (`count(1_234) == "1,234"`); K/M/B suffixes above
/// (`count(12_345) == "12.3K"`, `count(1_500_000) == "1.5M"`).
pub fn count(n: u64) -> String {
    if n < 10_000 {
        let digits = n.to_string();
        let mut out = String::with_capacity(digits.len() + digits.len() / 3);
        for (i, ch) in digits.chars().enumerate() {
            if i > 0 && (digits.len() - i) % 3 == 0 {
                out.push(',');
            }
            out.push(ch);
        }
        return out;
    }
    const SUFFIX: [(f64, &str); 3] = [(1e9, "B"), (1e6, "M"), (1e3, "K")];
    for (scale, suffix) in SUFFIX {
        if (n as f64) >= scale {
            return format!("{}{}", trim1(n as f64 / scale), suffix);
        }
    }
    unreachable!()
}

/// A percentage, whole numbers: `percent(41.6) == "42%"`.
pub fn percent(v: f64) -> String {
    if !v.is_finite() {
        return "\u{2014}".to_string();
    }
    format!("{}%", v.round() as i64)
}

/// A latency in milliseconds: whole ms under a second
/// (`latency_ms(812.4) == "812 ms"`), seconds above
/// (`latency_ms(2_340.0) == "2.3 s"`).
pub fn latency_ms(v: f64) -> String {
    if !v.is_finite() {
        return "\u{2014}".to_string();
    }
    if v < 1000.0 {
        format!("{} ms", v.round() as i64)
    } else {
        format!("{} s", trim1(v / 1000.0))
    }
}

/// A duration in seconds, largest two units: "12s", "5m 12s",
/// "2h 5m", "3d 2h". Mirrors the UI's fmtUptime.
pub fn duration_secs(secs: u64) -> String {
    let d = secs / 86_400;
    let h = (secs % 86_400) / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim1_drops_trailing_zero_and_rounds_large() {
        assert_eq!(trim1(15.992_187_5), "16");
        assert_eq!(trim1(15.1), "15.1");
        assert_eq!(trim1(4.0), "4");
        assert_eq!(trim1(0.25), "0.2");
        assert_eq!(trim1(123.75), "124");
        assert_eq!(trim1(f64::NAN), "\u{2014}");
    }

    #[test]
    fn gb_matches_the_dashboard_example() {
        // 15.1 used of a 16376 MiB card: "15.1/16", not "15.1/15.9921875".
        assert_eq!(gb(15.1), "15.1");
        assert_eq!(gb(15.992_187_5), "16");
    }

    #[test]
    fn bytes_scales_and_trims() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(4_635_476), "4.4 MB");
        assert_eq!(bytes(343_808_005), "328 MB");
        assert_eq!(bytes(17_179_869_184), "16 GB");
    }

    #[test]
    fn mib_switches_to_gib_at_one_gib() {
        assert_eq!(mib(256), "256 MiB");
        assert_eq!(mib(16_384), "16 GiB");
        assert_eq!(mib(1_536), "1.5 GiB");
    }

    #[test]
    fn count_groups_then_suffixes() {
        assert_eq!(count(999), "999");
        assert_eq!(count(1_234), "1,234");
        assert_eq!(count(12_345), "12.3K");
        assert_eq!(count(1_500_000), "1.5M");
        assert_eq!(count(2_000_000_000), "2B");
    }

    #[test]
    fn percent_latency_duration() {
        assert_eq!(percent(41.6), "42%");
        assert_eq!(latency_ms(812.4), "812 ms");
        assert_eq!(latency_ms(2_340.0), "2.3 s");
        assert_eq!(duration_secs(12), "12s");
        assert_eq!(duration_secs(312), "5m 12s");
        assert_eq!(duration_secs(7_500), "2h 5m");
        assert_eq!(duration_secs(262_800), "3d 1h");
    }
}
