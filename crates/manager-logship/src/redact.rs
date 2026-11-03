//! Redaction (design §5) — runs in the shipper, on the machine,
//! before anything is serialized to the wire. There is no
//! configuration in which unredacted lines leave the machine; the
//! redactor is not a per-destination option.
//!
//! Two layers, in order:
//! 1. **Literal secret values** — the deployment's known secrets
//!    (Credential Manager values the app already reads: the LiteLLM
//!    master key, the Postgres password, armed MCP credential
//!    values, the streaming token itself), replaced wherever they
//!    appear, in any shape. This catches the case pattern rules
//!    cannot: a secret echoed in an unexpected form.
//! 2. **Pattern classes** — hand-rolled scanners for the well-known
//!    shapes: `Authorization:` header values, `Bearer <token>`,
//!    `sk-` keys, key–value pairs with secret-bearing keys (JSON,
//!    `key=value`, URL query parameters, DSN parameters), JWT-shaped
//!    strings, and URI userinfo passwords (Postgres connection
//!    strings). Replacement preserves the key and the shape
//!    (`"api_key": "«redacted»"`) so a redacted line still reads as
//!    evidence; lines are replaced in place, never dropped.

/// The replacement marker.
pub const REDACTED: &str = "«redacted»";

/// What one `redact` call changed, for the shipper status counters
/// (values are counted, never logged — design §5.2).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RedactionCounts {
    /// Lines in which at least one literal secret value was found.
    pub literal_lines: u64,
    /// Pattern-class replacements across all lines.
    pub pattern_hits: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Redactor {
    /// Literal secret values, longest first so a value that is a
    /// prefix of another cannot shadow it. Values shorter than 4
    /// chars are ignored (they would redact half the alphabet).
    literals: Vec<String>,
}

impl Redactor {
    pub fn new<I, S>(literals: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut literals: Vec<String> = literals
            .into_iter()
            .map(|s| s.as_ref().to_string())
            .filter(|s| s.chars().count() >= 4)
            .collect();
        literals.sort_by_key(|s| std::cmp::Reverse(s.len()));
        literals.dedup();
        Self { literals }
    }

    pub fn literal_count(&self) -> usize {
        self.literals.len()
    }

    /// Redact one line. Returns the redacted line and what changed.
    pub fn redact(&self, line: &str) -> (String, RedactionCounts) {
        let mut counts = RedactionCounts::default();
        let mut out = line.to_string();
        // Layer 1 — literal values.
        let mut literal_hit = false;
        for lit in &self.literals {
            if out.contains(lit.as_str()) {
                out = out.replace(lit.as_str(), REDACTED);
                literal_hit = true;
            }
        }
        if literal_hit {
            counts.literal_lines = 1;
        }
        // Layer 2 — pattern classes.
        let (out2, hits) = pattern_redact(&out);
        counts.pattern_hits = hits;
        (out2, counts)
    }
}

// ---------------------------------------------------------------------------
// Layer 2 — pattern scanners. All operate on byte indices at ASCII
// boundaries, which is always UTF-8 safe: every pattern character is
// ASCII and multi-byte sequences never contain ASCII bytes.
// ---------------------------------------------------------------------------

fn pattern_redact(line: &str) -> (String, u64) {
    let mut hits = 0;
    let s = redact_jwts(line, &mut hits);
    let s = redact_bearer(&s, &mut hits);
    let s = redact_authorization(&s, &mut hits);
    let s = redact_sk_keys(&s, &mut hits);
    let s = redact_uri_userinfo(&s, &mut hits);
    let s = redact_secret_key_values(&s, &mut hits);
    (s, hits)
}

fn is_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'~' | b'+' | b'/' | b'=')
}

fn find_ci(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    let last = haystack.len() - needle.len();
    let mut i = from;
    while i <= last {
        if haystack[i..i + needle.len()]
            .iter()
            .zip(needle.iter())
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
        {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// JWT-shaped strings: `eyJ` followed by two more base64url segments
/// separated by dots.
fn redact_jwts(line: &str, hits: &mut u64) -> String {
    let b = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    let mut copied = 0;
    while i + 3 <= b.len() {
        if &b[i..i + 3] == b"eyJ" {
            // Walk segment(.segment)(.segment) of base64url chars.
            let mut j = i;
            let mut segments = 0;
            let mut ok = true;
            while segments < 3 {
                let start = j;
                while j < b.len()
                    && (b[j].is_ascii_alphanumeric() || matches!(b[j], b'-' | b'_'))
                {
                    j += 1;
                }
                if j - start < 4 {
                    ok = false;
                    break;
                }
                segments += 1;
                if segments < 3 {
                    if j < b.len() && b[j] == b'.' {
                        j += 1;
                    } else {
                        ok = false;
                        break;
                    }
                }
            }
            if ok && segments == 3 {
                out.push_str(&line[copied..i]);
                out.push_str(REDACTED);
                *hits += 1;
                i = j;
                copied = j;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&line[copied..]);
    out
}

/// `Bearer <token>` (any casing): keep the scheme, redact the token.
fn redact_bearer(line: &str, hits: &mut u64) -> String {
    let b = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut copied = 0;
    let mut from = 0;
    while let Some(pos) = find_ci(b, b"bearer", from) {
        let after = pos + 6;
        // Must be a standalone scheme word followed by a space.
        let boundary_before = pos == 0 || !is_token_char(b[pos - 1]);
        if boundary_before && after < b.len() && b[after] == b' ' {
            let mut j = after;
            while j < b.len() && b[j] == b' ' {
                j += 1;
            }
            let tok_start = j;
            while j < b.len() && is_token_char(b[j]) {
                j += 1;
            }
            if j > tok_start {
                out.push_str(&line[copied..tok_start]);
                out.push_str(REDACTED);
                *hits += 1;
                copied = j;
                from = j;
                continue;
            }
        }
        from = pos + 1;
    }
    out.push_str(&line[copied..]);
    out
}

/// `Authorization: <value>` header lines (any casing): keep an
/// optional scheme word, redact the remainder of the value (up to a
/// closing quote, comma, or end of line).
fn redact_authorization(line: &str, hits: &mut u64) -> String {
    let b = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut copied = 0;
    let mut from = 0;
    while let Some(pos) = find_ci(b, b"authorization", from) {
        let mut j = pos + 13;
        while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
            j += 1;
        }
        if j < b.len() && b[j] == b':' {
            j += 1;
            while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
                j += 1;
            }
            // Optional scheme word (Basic/Bearer/Digest…) preserved.
            let word_start = j;
            while j < b.len() && b[j].is_ascii_alphabetic() {
                j += 1;
            }
            if j > word_start && j < b.len() && b[j] == b' ' {
                j += 1;
            } else {
                j = word_start;
            }
            let val_start = j;
            while j < b.len() && !matches!(b[j], b'"' | b',' | b'\r' | b'\n') {
                j += 1;
            }
            if j > val_start {
                out.push_str(&line[copied..val_start]);
                out.push_str(REDACTED);
                *hits += 1;
                copied = j;
                from = j;
                continue;
            }
        }
        from = pos + 1;
    }
    out.push_str(&line[copied..]);
    out
}

/// `sk-…` provider/LiteLLM key shapes: keep the prefix, redact the
/// body (only when the body is long enough to be a key, ≥ 8 chars).
fn redact_sk_keys(line: &str, hits: &mut u64) -> String {
    let b = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut copied = 0;
    let mut from = 0;
    while let Some(pos) = find_ci(b, b"sk-", from) {
        let body = pos + 3;
        let mut j = body;
        while j < b.len() && is_token_char(b[j]) && b[j] != b'.' {
            j += 1;
        }
        if j - body >= 8 {
            out.push_str(&line[copied..body]);
            out.push_str(REDACTED);
            *hits += 1;
            copied = j;
            from = j;
            continue;
        }
        from = pos + 1;
    }
    out.push_str(&line[copied..]);
    out
}

/// URI userinfo passwords: `scheme://user:password@host` — covers
/// Postgres connection strings and any other credentialed URL.
fn redact_uri_userinfo(line: &str, hits: &mut u64) -> String {
    let b = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut copied = 0;
    let mut from = 0;
    while let Some(pos) = find_ci(b, b"://", from) {
        let auth_start = pos + 3;
        // Authority runs to the first '/', '?', '#', space, or quote.
        let mut end = auth_start;
        while end < b.len() && !matches!(b[end], b'/' | b'?' | b'#' | b' ' | b'"' | b'\'') {
            end += 1;
        }
        let authority = &b[auth_start..end];
        if let Some(at_rel) = authority.iter().position(|&c| c == b'@') {
            if let Some(colon_rel) = authority[..at_rel].iter().position(|&c| c == b':') {
                let pass_start = auth_start + colon_rel + 1;
                let pass_end = auth_start + at_rel;
                if pass_end > pass_start {
                    out.push_str(&line[copied..pass_start]);
                    out.push_str(REDACTED);
                    *hits += 1;
                    copied = pass_end;
                }
            }
        }
        from = end.max(pos + 3);
    }
    out.push_str(&line[copied..]);
    out
}

/// Is this key secret-bearing? Segments (split on `_`, `-`, `.`)
/// decide: any segment in the secret set, or the whole key exactly
/// `auth`/`authorization`. `author` is deliberately not a hit.
fn key_is_secret(key: &str) -> bool {
    let lk = key.to_lowercase();
    if matches!(lk.as_str(), "auth" | "authorization" | "apikey") {
        return true;
    }
    // Compound key names with separators removed (master_key,
    // api-key, private key shapes). `apikey` anywhere in the
    // squashed name counts (BRAVE_API_KEY, api[_-]?key per §5.2).
    let squashed: String = lk.chars().filter(|c| !matches!(c, '_' | '-' | '.')).collect();
    if squashed.contains("apikey")
        || matches!(
            squashed.as_str(),
            "masterkey" | "privatekey" | "accesskey" | "secretkey"
        )
    {
        return true;
    }
    lk.split(['_', '-', '.']).any(|seg| {
        matches!(
            seg,
            "token" | "secret" | "password" | "passwd" | "credential" | "credentials" | "apikey"
        )
    })
}

/// Key–value pairs with secret-bearing keys: `"api_key": "…"`,
/// `token=…`, URL query parameters (`?token=…&`), DSN parameters
/// (`password=… `). The key and shape are preserved; the value is
/// replaced. Bare values run to the next value delimiter.
fn redact_secret_key_values(line: &str, hits: &mut u64) -> String {
    let b = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut copied = 0;
    let mut i = 0;
    while i < b.len() {
        // Candidate key: a run of key characters.
        if b[i].is_ascii_alphanumeric() || b[i] == b'_' {
            let kstart = i;
            let mut kend = i;
            while kend < b.len()
                && (b[kend].is_ascii_alphanumeric() || matches!(b[kend], b'_' | b'-' | b'.'))
            {
                kend += 1;
            }
            let key = &line[kstart..kend];
            // Optional closing quote for JSON keys.
            let mut j = kend;
            if j < b.len() && b[j] == b'"' {
                j += 1;
            }
            let mut k = j;
            while k < b.len() && (b[k] == b' ' || b[k] == b'\t') {
                k += 1;
            }
            if k < b.len() && (b[k] == b'=' || b[k] == b':') && key_is_secret(key) {
                let sep = b[k];
                k += 1;
                while k < b.len() && (b[k] == b' ' || b[k] == b'\t') {
                    k += 1;
                }
                // Header exception: an unquoted `Authorization:`
                // line is owned by the header pass above, which
                // preserves the scheme word and redacts the
                // credential. Treating the scheme word as this
                // key's bare value would redact "Bearer" itself,
                // double-marking the line. Quoted (JSON)
                // authorization values are not headers and still
                // redact below.
                if sep == b':'
                    && key.eq_ignore_ascii_case("authorization")
                    && !(k < b.len() && b[k] == b'"')
                {
                    i = kend.max(i + 1);
                    continue;
                }
                // Value: quoted or bare.
                if k < b.len() && b[k] == b'"' {
                    let vstart = k + 1;
                    let mut vend = vstart;
                    while vend < b.len() && b[vend] != b'"' {
                        vend += 1;
                    }
                    if vend > vstart {
                        out.push_str(&line[copied..vstart]);
                        out.push_str(REDACTED);
                        *hits += 1;
                        copied = vend;
                        i = vend;
                        continue;
                    }
                } else {
                    let vstart = k;
                    let mut vend = vstart;
                    while vend < b.len()
                        && !matches!(
                            b[vend],
                            b' ' | b'\t' | b',' | b';' | b'&' | b'"' | b'\'' | b'}' | b']' | b')'
                        )
                    {
                        vend += 1;
                    }
                    if vend > vstart {
                        out.push_str(&line[copied..vstart]);
                        out.push_str(REDACTED);
                        *hits += 1;
                        copied = vend;
                        i = vend;
                        continue;
                    }
                }
            }
            i = kend.max(i + 1);
            continue;
        }
        i += 1;
    }
    out.push_str(&line[copied..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(line: &str) -> String {
        Redactor::new(Vec::<&str>::new()).redact(line).0
    }

    #[test]
    fn literal_values_redact_in_any_shape() {
        let red = Redactor::new(["master-key-value-1234"]);
        let (out, counts) = red.redact("connecting with master-key-value-1234 ok");
        assert_eq!(out, format!("connecting with {REDACTED} ok"));
        assert_eq!(counts.literal_lines, 1);
    }

    #[test]
    fn json_secret_pairs_keep_shape() {
        assert_eq!(
            r(r#"{"api_key": "abc123def456", "model": "x"}"#),
            format!(r#"{{"api_key": "{REDACTED}", "model": "x"}}"#)
        );
        assert_eq!(
            r(r#""postgres_password": "hunter2hunter2""#),
            format!(r#""postgres_password": "{REDACTED}""#)
        );
    }

    #[test]
    fn bearer_and_authorization_headers() {
        assert_eq!(
            r("Authorization: Bearer abc.def-123"),
            format!("Authorization: Bearer {REDACTED}")
        );
        assert_eq!(
            r("authorization: Basic dXNlcjpwYXNz"),
            format!("authorization: Basic {REDACTED}")
        );
        assert_eq!(r("send Bearer xyz12345 now"), format!("send Bearer {REDACTED} now"));
    }

    #[test]
    fn sk_keys_redact_body() {
        assert_eq!(r("key sk-abcdefgh12345678 end"), format!("key sk-{REDACTED} end"));
        // Too short to be a key: untouched.
        assert_eq!(r("sk-abc is fine"), "sk-abc is fine");
    }

    #[test]
    fn jwt_shapes_redact() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abcdefghij-klmnop_qrstuv";
        assert_eq!(r(&format!("t={jwt}")), format!("t={REDACTED}"));
    }

    #[test]
    fn url_query_and_dsn_params() {
        assert_eq!(
            r("GET https://h.test/v1?token=abc12345&model=q HTTP/1.1"),
            format!("GET https://h.test/v1?token={REDACTED}&model=q HTTP/1.1")
        );
        assert_eq!(
            r("postgres://user:p4ssw0rd@db.host:5432/app"),
            format!("postgres://user:{REDACTED}@db.host:5432/app")
        );
        assert_eq!(
            r("host=db port=5432 password=p4ssw0rd user=app"),
            format!("host=db port=5432 password={REDACTED} user=app")
        );
    }

    #[test]
    fn innocent_lines_untouched() {
        assert_eq!(r("the author wrote a token bucket note"), "the author wrote a token bucket note");
        assert_eq!(r("step start-gateway (install): started"), "step start-gateway (install): started");
        assert_eq!(r("monkey business as usual"), "monkey business as usual");
    }
}
