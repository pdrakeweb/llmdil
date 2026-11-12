//! Config diff (library utility; no UI flow currently calls it).

// ---------------------------------------------------------------------------
// Config diff (library utility; no UI flow currently calls it)
// ---------------------------------------------------------------------------

/// One line of a config diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffKind {
    Same,
    Added,
    Removed,
}

/// One line of a config diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffKind,
    pub text: String,
}

/// Line-based diff of two YAML documents (old vs new), via LCS.
/// Small inputs (configs); O(n*m) is fine. Library utility: currently
/// unwired — no command or UI flow calls it.
pub fn diff_config_yaml(old: &str, new: &str) -> Vec<DiffLine> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let n = a.len();
    let m = b.len();

    // LCS length table.
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }

    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push(DiffLine {
                kind: DiffKind::Same,
                text: a[i].to_string(),
            });
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            out.push(DiffLine {
                kind: DiffKind::Removed,
                text: a[i].to_string(),
            });
            i += 1;
        } else {
            out.push(DiffLine {
                kind: DiffKind::Added,
                text: b[j].to_string(),
            });
            j += 1;
        }
    }
    while i < n {
        out.push(DiffLine {
            kind: DiffKind::Removed,
            text: a[i].to_string(),
        });
        i += 1;
    }
    while j < m {
        out.push(DiffLine {
            kind: DiffKind::Added,
            text: b[j].to_string(),
        });
        j += 1;
    }
    out
}

/// (added, removed) line counts for a diff.
pub fn diff_summary(lines: &[DiffLine]) -> (usize, usize) {
    let added = lines.iter().filter(|l| l.kind == DiffKind::Added).count();
    let removed = lines.iter().filter(|l| l.kind == DiffKind::Removed).count();
    (added, removed)
}

/// Render a diff in unified-ish `+`/`-`/` ` form for the review UI.
pub fn render_unified_diff(lines: &[DiffLine]) -> String {
    let mut out = String::new();
    for l in lines {
        let prefix = match l.kind {
            DiffKind::Same => " ",
            DiffKind::Added => "+",
            DiffKind::Removed => "-",
        };
        out.push_str(prefix);
        out.push_str(&l.text);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_identical_is_all_same() {
        let doc = "a: 1\nb: 2\n";
        let lines = diff_config_yaml(doc, doc);
        assert!(lines.iter().all(|l| l.kind == DiffKind::Same));
        assert_eq!(diff_summary(&lines), (0, 0));
    }

    #[test]
    fn diff_detects_changed_port() {
        let old = "model_list:\n  - api_base: http://127.0.0.1:8081/v1\n";
        let new = "model_list:\n  - api_base: http://127.0.0.1:8082/v1\n";
        let lines = diff_config_yaml(old, new);
        let (added, removed) = diff_summary(&lines);
        assert_eq!((added, removed), (1, 1));
        let rendered = render_unified_diff(&lines);
        assert!(rendered.contains("-  - api_base: http://127.0.0.1:8081/v1"));
        assert!(rendered.contains("+  - api_base: http://127.0.0.1:8082/v1"));
        assert!(rendered.contains(" model_list:"));
    }

    #[test]
    fn diff_detects_appended_block() {
        let old = "a: 1\n";
        let new = "a: 1\nb: 2\nc: 3\n";
        let (added, removed) = diff_summary(&diff_config_yaml(old, new));
        assert_eq!((added, removed), (2, 0));
    }
}
