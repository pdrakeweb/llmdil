//! Golden-payload harness (docs/testing-protocol.md, "Golden payloads").
//!
//! A golden test serializes an operation's JSON payload from a
//! deterministic fixture, normalises the volatile parts (timestamps,
//! ids, tokens, scratch paths), and compares the pretty-printed result
//! with a checked-in file under `tests/golden/`. The UI, CLI and
//! Android client parse these payloads; a golden diff is the review
//! point for any change to their shape.
//!
//! Regenerate after an intended payload change:
//!
//! ```text
//! UPDATE_GOLDEN=1 cargo test -p manager-app golden
//! UPDATE_GOLDEN=1 cargo test -p manager-api --test golden_auth
//! ```
//!
//! Shared by manager-app (a unit-test module, which needs private
//! access to pin the telemetry store) and manager-api (an integration
//! test) via `#[path]`; it depends on serde_json and std only.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use serde_json::Value;

/// The environment variable that switches comparison to rewriting.
pub const UPDATE_ENV: &str = "UPDATE_GOLDEN";

/// How to scrub a payload before comparison.
#[derive(Default)]
pub struct Normalizer {
    /// Object keys whose values are volatile (wall-clock times, random
    /// ids and tokens, the build version). The value is replaced with
    /// a placeholder naming its JSON type, so a field changing from a
    /// number to null still shows up as a diff.
    pub volatile_keys: Vec<&'static str>,
    /// Literal substrings (scratch directories) replaced in every
    /// string value, with the placeholder to use. A string that had a
    /// substitution also has its `\` separators turned into `/`, so a
    /// golden recorded on Windows matches one recorded on Linux.
    pub paths: Vec<(String, &'static str)>,
    /// Object keys whose string values are file paths built with the
    /// platform separator (e.g. a config-relative `gguf_path`): their
    /// `\` separators become `/`, so goldens are platform-neutral.
    pub separator_keys: Vec<&'static str>,
}

impl Normalizer {
    pub fn keys(keys: &[&'static str]) -> Self {
        Self {
            volatile_keys: keys.to_vec(),
            ..Self::default()
        }
    }

    pub fn separators(mut self, keys: &[&'static str]) -> Self {
        self.separator_keys.extend_from_slice(keys);
        self
    }

    pub fn path(mut self, dir: &Path, placeholder: &'static str) -> Self {
        let s = dir.to_string_lossy().to_string();
        // Both spellings: as-is, and with forward slashes (a payload
        // may have built the path with `/` joins on Windows).
        self.paths.push((s.replace('\\', "/"), placeholder));
        self.paths.push((s, placeholder));
        self
    }

    pub fn apply(&self, v: &mut Value) {
        match v {
            Value::Object(map) => {
                for (k, child) in map.iter_mut() {
                    if self.volatile_keys.contains(&k.as_str()) {
                        *child = placeholder(child);
                    } else if self.separator_keys.contains(&k.as_str()) {
                        self.apply(child);
                        if let Value::String(s) = child {
                            *s = s.replace('\\', "/");
                        }
                    } else {
                        self.apply(child);
                    }
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|c| self.apply(c)),
            Value::String(s) => {
                let mut out = s.clone();
                for (needle, ph) in &self.paths {
                    if !needle.is_empty() && out.contains(needle.as_str()) {
                        out = out.replace(needle.as_str(), ph);
                    }
                }
                if out != *s {
                    *s = out.replace('\\', "/");
                }
            }
            _ => {}
        }
    }
}

fn placeholder(v: &Value) -> Value {
    Value::String(
        match v {
            Value::Null => "<null>",
            Value::Bool(_) => "<bool>",
            Value::Number(_) => "<number>",
            Value::String(_) => "<string>",
            Value::Array(_) => "<array>",
            Value::Object(_) => "<object>",
        }
        .to_string(),
    )
}

/// The checked-in golden file `<crate>/tests/golden/<name>.json`.
pub fn golden_path(manifest_dir: &str, name: &str) -> PathBuf {
    Path::new(manifest_dir)
        .join("tests")
        .join("golden")
        .join(format!("{name}.json"))
}

/// Compare `actual` (already normalised) with the golden file, or
/// rewrite the file when `UPDATE_GOLDEN=1`. Panics with a line diff
/// on mismatch.
pub fn assert_golden(manifest_dir: &str, name: &str, actual: &Value) {
    let path = golden_path(manifest_dir, name);
    // serde_json's map is ordered (no preserve_order feature in the
    // workspace), so the rendering is canonical.
    let rendered = format!(
        "{}\n",
        serde_json::to_string_pretty(actual).expect("payload serializes")
    );
    if std::env::var(UPDATE_ENV).is_ok_and(|v| v == "1") {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &rendered).unwrap();
        eprintln!("golden: wrote {}", path.display());
        return;
    }
    let expected = match std::fs::read_to_string(&path) {
        // A Windows checkout may carry CRLF line endings.
        Ok(s) => s.replace("\r\n", "\n"),
        Err(e) => panic!(
            "golden file {} is missing ({e}); run with {UPDATE_ENV}=1 to create it",
            path.display()
        ),
    };
    if expected != rendered {
        panic!(
            "golden payload `{name}` differs from {}\n\
             (- expected, + actual; rerun with {UPDATE_ENV}=1 to accept an intended change)\n{}",
            path.display(),
            line_diff(&expected, &rendered)
        );
    }
}

/// A minimal unified-style line diff (LCS over lines) with three lines
/// of context around each change; enough to read a payload change.
pub fn line_diff(expected: &str, actual: &str) -> String {
    let a: Vec<&str> = expected.lines().collect();
    let b: Vec<&str> = actual.lines().collect();
    let (n, m) = (a.len(), b.len());
    let mut lcs = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    // Edit script: (' ' | '-' | '+', line).
    let mut ops: Vec<(char, &str)> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            ops.push((' ', a[i]));
            i += 1;
            j += 1;
        } else if i < n && (j == m || lcs[i + 1][j] >= lcs[i][j + 1]) {
            ops.push(('-', a[i]));
            i += 1;
        } else {
            ops.push(('+', b[j]));
            j += 1;
        }
    }
    const CONTEXT: usize = 3;
    let changed: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, (c, _))| *c != ' ')
        .map(|(k, _)| k)
        .collect();
    let mut out = String::new();
    let mut last_printed: Option<usize> = None;
    for (k, (c, line)) in ops.iter().enumerate() {
        let near = changed.iter().any(|&x| x.abs_diff(k) <= CONTEXT);
        if !near {
            continue;
        }
        if last_printed.is_some_and(|p| p + 1 != k) || (last_printed.is_none() && k > 0) {
            out.push_str("@@\n");
        }
        out.push(*c);
        out.push(' ');
        out.push_str(line);
        out.push('\n');
        last_printed = Some(k);
    }
    out
}

#[cfg(test)]
mod harness_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalizer_scrubs_keys_and_paths() {
        let n = Normalizer::keys(&["at_ms", "id"]).path(Path::new("/tmp/x"), "<DIR>");
        let mut v = json!({
            "at_ms": 123, "id": null,
            "nested": [{ "id": "abc", "path": "/tmp/x/models/a.gguf", "keep": 5 }],
        });
        n.apply(&mut v);
        assert_eq!(
            v,
            json!({
                "at_ms": "<number>", "id": "<null>",
                "nested": [{ "id": "<string>", "path": "<DIR>/models/a.gguf", "keep": 5 }],
            })
        );
    }

    #[test]
    fn windows_paths_normalise_to_forward_slashes() {
        let n = Normalizer::default().path(Path::new(r"C:\Temp\x"), "<DIR>");
        let mut v = json!(r"C:\Temp\x\models\a.gguf");
        n.apply(&mut v);
        assert_eq!(v, json!("<DIR>/models/a.gguf"));
    }

    #[test]
    fn separator_keys_are_platform_neutral() {
        let n = Normalizer::default().separators(&["gguf_path"]);
        let mut v = json!({ "gguf_path": r"models\a.gguf", "other": r"a\b" });
        n.apply(&mut v);
        assert_eq!(v, json!({ "gguf_path": "models/a.gguf", "other": r"a\b" }));
    }

    #[test]
    fn line_diff_marks_changes_with_context() {
        let d = line_diff("a\nb\nc\n", "a\nB\nc\n");
        assert!(d.contains("- b\n"), "{d}");
        assert!(d.contains("+ B\n"), "{d}");
        assert!(d.contains("  a\n"), "{d}");
        assert!(d.find("- b").unwrap() < d.find("+ B").unwrap(), "{d}");
    }
}
