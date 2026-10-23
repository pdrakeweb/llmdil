//! Parsing of llama.cpp's speculative-decoding log lines (kit §1.5).
//!
//! Two line shapes, both emitted per completed request:
//! - the aggregate `draft acceptance = … (A accepted / G generated),
//!   mean len = …` line (INFO), and
//! - the per-implementation `statistics <kind>: #gen drafts = …`
//!   lines (TRACE), which carry the per-drafter attribution a
//!   combined arm needs.
//!
//! Baseline (non-spec) arms emit neither line; that absence is the
//! check that the arm was configured as intended, so both parsers
//! return `None` for anything that is not a well-formed line of
//! their shape — including a bare "no draft lines here" log.

use serde::{Deserialize, Serialize};

/// One parsed `draft acceptance` line: the aggregate acceptance
/// across all enabled drafters for one completed request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AcceptanceSample {
    /// accepted / generated draft tokens, as printed.
    pub rate: f64,
    pub accepted: u64,
    pub generated: u64,
    /// Mean accepted length (1 + accepted / verification steps).
    pub mean_len: f64,
}

/// One parsed per-implementation `statistics <kind>:` line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DrafterStats {
    /// The drafter implementation name, e.g. `ngram-mod`.
    pub kind: String,
    pub gen_drafts: u64,
    pub acc_drafts: u64,
    pub gen_tokens: u64,
    pub acc_tokens: u64,
    /// `#mean acc len`, when the line carries it.
    pub mean_acc_len: Option<f64>,
}

/// Read the numeric value that follows `label`'s `=` in `line`:
/// finds `label`, then the next `=`, then parses the leading run of
/// characters accepted by `take` after trimming whitespace.
fn value_after_label<'a>(line: &'a str, label: &str) -> Option<&'a str> {
    let at = line.find(label)? + label.len();
    let rest = &line[at..];
    let eq = rest.find('=')?;
    let raw = rest[eq + 1..].trim_start();
    let end = raw
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
        .unwrap_or(raw.len());
    if end == 0 {
        None
    } else {
        Some(&raw[..end])
    }
}

fn u64_after_label(line: &str, label: &str) -> Option<u64> {
    value_after_label(line, label)?.parse().ok()
}

fn f64_after_label(line: &str, label: &str) -> Option<f64> {
    value_after_label(line, label)?.parse().ok()
}

/// Parse one `draft acceptance` INFO line, tolerant of the variable
/// spacing llama.cpp pads its counters with, e.g.
/// `draft acceptance = 0.54321 (   123 accepted /     227 generated), mean len =  2.07`.
/// Returns `None` for any other line.
pub fn parse_acceptance_line(line: &str) -> Option<AcceptanceSample> {
    if !line.contains("draft acceptance") {
        return None;
    }
    let rate = f64_after_label(line, "draft acceptance")?;
    // The counts sit inside the parentheses as
    // "( <accepted> accepted / <generated> generated)".
    let open = line.find('(')?;
    let close = line[open..].find(')').map(|i| open + i)?;
    let inner = &line[open + 1..close];
    let mut numbers = inner.split('/').map(|side| {
        side.split_whitespace()
            .next()
            .and_then(|n| n.parse::<u64>().ok())
    });
    let accepted = numbers.next()??;
    let generated = numbers.next()??;
    let mean_len = f64_after_label(line, "mean len")?;
    Some(AcceptanceSample {
        rate,
        accepted,
        generated,
        mean_len,
    })
}

/// Parse one per-implementation `statistics <kind>:` TRACE line.
/// Fields are comma-separated `#name = value` pairs; the
/// `#calls(b,g,a) = …` field and the trailing `#acc rate/pos = (…)`
/// field are tolerated and ignored. Returns `None` for any line
/// that is not a statistics line or is missing a required counter.
pub fn parse_statistics_line(line: &str) -> Option<DrafterStats> {
    let at = line.find("statistics ")? + "statistics ".len();
    let rest = &line[at..];
    let colon = rest.find(':')?;
    let kind = rest[..colon].trim().to_string();
    if kind.is_empty() {
        return None;
    }
    Some(DrafterStats {
        kind,
        gen_drafts: u64_after_label(line, "#gen drafts")?,
        acc_drafts: u64_after_label(line, "#acc drafts")?,
        gen_tokens: u64_after_label(line, "#gen tokens")?,
        acc_tokens: u64_after_label(line, "#acc tokens")?,
        mean_acc_len: f64_after_label(line, "#mean acc len"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acceptance_line_parses_kit_format() {
        let line = "draft acceptance = 0.54321 (   123 accepted /     227 generated), mean len =  2.07";
        let s = parse_acceptance_line(line).expect("kit line parses");
        assert_eq!(s.accepted, 123);
        assert_eq!(s.generated, 227);
        assert!((s.rate - 0.54321).abs() < 1e-9);
        assert!((s.mean_len - 2.07).abs() < 1e-9);
    }

    #[test]
    fn acceptance_line_tolerates_log_prefixes_and_tight_spacing() {
        let line = "2026-10-23T12:00:00 srv  info draft acceptance = 1.00000 (10 accepted / 10 generated), mean len = 3.50";
        let s = parse_acceptance_line(line).expect("prefixed line parses");
        assert_eq!(s.accepted, 10);
        assert_eq!(s.generated, 10);
        assert!((s.mean_len - 3.50).abs() < 1e-9);
    }

    #[test]
    fn non_acceptance_lines_return_none() {
        // A baseline arm emits no draft lines at all; ordinary log
        // lines must not parse as samples.
        assert_eq!(parse_acceptance_line("prompt eval time = 123 ms"), None);
        assert_eq!(parse_acceptance_line("draft acceptance = x"), None);
        assert_eq!(parse_acceptance_line(""), None);
        assert_eq!(
            parse_statistics_line("prompt eval time = 123 ms"),
            None
        );
    }

    #[test]
    fn statistics_line_parses_kit_format() {
        let line = "statistics ngram-mod: #calls(b,g,a) = 100, 90, 10, #gen drafts = 90, \
                    #acc drafts = 40, #gen tokens = 500, #acc tokens = 210, \
                    #mean acc len = 2.33, #acc rate/pos = (0.42, 0.31, 0.20)";
        let s = parse_statistics_line(line).expect("kit line parses");
        assert_eq!(s.kind, "ngram-mod");
        assert_eq!(s.gen_drafts, 90);
        assert_eq!(s.acc_drafts, 40);
        assert_eq!(s.gen_tokens, 500);
        assert_eq!(s.acc_tokens, 210);
        assert_eq!(s.mean_acc_len, Some(2.33));
    }

    #[test]
    fn statistics_line_without_mean_len_still_parses() {
        let line = "statistics draft-simple: #gen drafts = 12, #acc drafts = 5, \
                    #gen tokens = 36, #acc tokens = 15";
        let s = parse_statistics_line(line).expect("line parses");
        assert_eq!(s.kind, "draft-simple");
        assert_eq!(s.mean_acc_len, None);
    }
}
