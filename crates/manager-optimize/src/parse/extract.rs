//! Balanced-object extraction: find the JSON object(s) inside a
//! consultation response that may carry fences and prose.

use serde_json::Value;

/// Most `{` positions [`json_object_candidates`] will try as an
/// object start; bounds the quadratic worst case on brace-heavy text.
const MAX_OBJECT_STARTS: usize = 256;

/// The balanced `{...}` span starting at byte `start` (which must be a
/// `{`), tracking string literals and escapes so braces inside strings
/// do not count. `None` when the text ends before the span closes.
fn balanced_object_at(text: &str, start: usize) -> Option<&str> {
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    // `{`/`}` are ASCII, so both ends are char
                    // boundaries even in multibyte text.
                    return Some(&text[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Every balanced `{...}` span in `text` that parses as a JSON object,
/// in order of its opening brace (finding P-8,
/// docs/code-review-v0.1.60.md). Trying each `{` — not only the first —
/// is what lets the salvage survive prose such as "use the {ctx}
/// setting" or a stray unbalanced `{` ahead of the fenced plan, which
/// previously failed the whole consultation as `NoJsonObject`.
pub fn json_object_candidates(text: &str) -> impl Iterator<Item = &str> {
    text.match_indices('{')
        .take(MAX_OBJECT_STARTS)
        .filter_map(move |(start, _)| balanced_object_at(text, start))
        .filter(|cand| serde_json::from_str::<Value>(cand).is_ok_and(|v| v.is_object()))
}

/// Extract the first balanced `{...}` span from response text that
/// parses as a JSON object. Code fences and surrounding prose are
/// expected. String literals and escapes are tracked so braces inside
/// strings do not count.
pub fn extract_json_object(text: &str) -> Option<String> {
    json_object_candidates(text).next().map(str::to_string)
}
