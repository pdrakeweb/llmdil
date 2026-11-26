//! Paste import: a block of PowerShell or cmd diagnostic commands →
//! a draft [`Recipe`] for review.
//!
//! Nothing here executes anything, and pasted text never becomes a
//! shell string: every command is tokenized into an argv array.
//! Understood syntax:
//!
//! - PowerShell: `$env:X=1` (recipe env), `$m="..."` (a recipe
//!   parameter; `$m`, `$($m)`, `${m}` references become `{m}`),
//!   `$env:X` references inside values, `.\tool.exe` / `tool` /
//!   `& "C:\path\tool.exe"` invocations, `#` comments, trailing
//!   backtick continuations, `;` separators, `cd` / `Set-Location`.
//! - cmd: `set X=1` / `set "X=1"` (recipe env), `%X%` references,
//!   `REM` / `::` comments, trailing `^` continuations, `&&`
//!   separators, `cd /d`.
//!
//! A comment line describes the next command; an inline comment
//! describes its own line. A comment mentioning a crash or failure
//! (`# expect crash at d=8192`) marks that command `may_crash`.
//! Pipes and redirections are dropped with a warning (output is
//! captured anyway). Paths under the managed install are rewritten to
//! templates (`{models_dir}`, `{data_dir}`, `{llamacpp_bin}`).
//!
//! Consecutive invocations of the same tool that differ only in flag
//! values are folded into one step with variants when `fold` is set;
//! the fold is reported so the UI can offer to undo it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::argv::{self, ArgItem};
use crate::recipe::{Expect, Param, ParserKind, Recipe, Requires, Step, StepKind, Variant};

/// What the importer knows about the machine.
#[derive(Debug, Clone, Default)]
pub struct ImportContext {
    pub data_dir: String,
    pub models_dir: String,
    pub llamacpp_dir: String,
    /// File names present in the managed llama.cpp directory
    /// (`llama-bench.exe`, …).
    pub managed_tools: Vec<String>,
    /// The process environment (for `$env:X` path templating).
    pub env: BTreeMap<String, String>,
    /// Executable names found on PATH (lowercase file names, with
    /// and without `.exe`), for import-time warnings only.
    pub path_tools: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dialect {
    Powershell,
    Cmd,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FoldInfo {
    /// Index of the folded step in the folded recipe.
    pub step_index: usize,
    /// How many pasted invocations it absorbed.
    pub folded: usize,
    pub exe: String,
    /// The flags that vary across variants.
    pub varying: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportResult {
    pub dialect: Dialect,
    /// The draft recipe (folded when `fold` was requested).
    pub recipe: Recipe,
    /// The same draft without folding, for an "unfold" toggle.
    pub unfolded: Recipe,
    pub folds: Vec<FoldInfo>,
    pub warnings: Vec<String>,
}

/// One logical line after continuation joining, with its comment.
#[derive(Debug)]
struct Logical {
    text: String,
    comment: Option<String>,
    line_no: usize,
}

fn detect_dialect(text: &str) -> Dialect {
    let mut ps = 0;
    let mut cmd = 0;
    for l in text.lines() {
        let t = l.trim();
        let lower = t.to_ascii_lowercase();
        if t.starts_with('$') || t.contains("$env:") || t.ends_with('`') || t.starts_with("& ") {
            ps += 2;
        }
        if t.starts_with('#') {
            ps += 1;
        }
        if lower.starts_with("set ")
            || lower.starts_with("rem ")
            || t.starts_with("::")
            || t.ends_with('^')
            || t.contains('%')
        {
            cmd += 2;
        }
    }
    if cmd > ps {
        Dialect::Cmd
    } else {
        Dialect::Powershell
    }
}

/// Split a physical line into code and trailing comment, respecting
/// quotes. PowerShell: `#` starts a comment at a token boundary.
/// cmd has no inline comments except `&REM`/`& ::` (treated as text).
fn split_comment(line: &str, dialect: Dialect) -> (String, Option<String>) {
    if dialect == Dialect::Cmd {
        return (line.to_string(), None);
    }
    let mut in_d = false;
    let mut in_s = false;
    let mut prev_ws = true;
    for (i, c) in line.char_indices() {
        match c {
            '"' if !in_s => in_d = !in_d,
            '\'' if !in_d => in_s = !in_s,
            '#' if !in_d && !in_s && prev_ws => {
                let comment = line[i + 1..].trim().to_string();
                return (
                    line[..i].to_string(),
                    if comment.is_empty() {
                        None
                    } else {
                        Some(comment)
                    },
                );
            }
            _ => {}
        }
        prev_ws = c.is_whitespace();
    }
    (line.to_string(), None)
}

fn logical_lines(text: &str, dialect: Dialect) -> Vec<Logical> {
    let cont = match dialect {
        Dialect::Powershell => '`',
        Dialect::Cmd => '^',
    };
    let mut out: Vec<Logical> = Vec::new();
    let mut acc: Option<Logical> = None;
    for (n, raw) in text.lines().enumerate() {
        let (code, comment) = split_comment(raw, dialect);
        let trimmed = code.trim_end();
        let (body, continues) = match trimmed.strip_suffix(cont) {
            Some(b) => (b.to_string(), true),
            // A trailing backslash continuation (bash-style paste) too.
            None => match trimmed.strip_suffix(" \\") {
                Some(b) => (b.to_string(), true),
                None => (trimmed.to_string(), false),
            },
        };
        match acc.as_mut() {
            Some(a) => {
                a.text.push(' ');
                a.text.push_str(body.trim());
                if a.comment.is_none() {
                    a.comment = comment;
                }
            }
            None => {
                acc = Some(Logical {
                    text: body.trim().to_string(),
                    comment,
                    line_no: n + 1,
                });
            }
        }
        if !continues {
            if let Some(a) = acc.take() {
                out.push(a);
            }
        }
    }
    if let Some(a) = acc.take() {
        out.push(a);
    }
    out
}

/// A token with whether any of it was quoted, and which quote style
/// (single-quoted PowerShell text is literal: no `$` expansion).
#[derive(Debug, Clone, PartialEq)]
struct Tok {
    text: String,
    literal: bool,
    /// An operator token (`|`, `>`, `2>&1`, `;`, `&&`, `&`).
    op: bool,
}

/// Tokenize one command into words. Double quotes group (and allow
/// `$var` expansion in PowerShell); single quotes group literally in
/// PowerShell (`''` is an escaped quote) and are ordinary characters
/// in cmd. Backslashes are never escapes (Windows paths). Unquoted
/// `|`, `>`, `<`, `;`, `&&`, `||` and `&` (cmd) end the word and are
/// returned as operator tokens.
fn tokenize(s: &str, dialect: Dialect) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut have = false;
    let mut literal = false;
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let flush = |cur: &mut String, have: &mut bool, literal: &mut bool, out: &mut Vec<Tok>| {
        if *have {
            out.push(Tok {
                text: std::mem::take(cur),
                literal: *literal,
                op: false,
            });
        }
        *have = false;
        *literal = false;
    };
    while i < chars.len() {
        let c = chars[i];
        match c {
            c if c.is_whitespace() => {
                flush(&mut cur, &mut have, &mut literal, &mut out);
                i += 1;
            }
            '"' => {
                have = true;
                i += 1;
                while i < chars.len() {
                    if chars[i] == '"' {
                        // "" inside a double-quoted string is a quote.
                        if chars.get(i + 1) == Some(&'"') {
                            cur.push('"');
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    // PowerShell backtick escape inside double quotes.
                    if dialect == Dialect::Powershell && chars[i] == '`' && i + 1 < chars.len() {
                        cur.push(chars[i + 1]);
                        i += 2;
                        continue;
                    }
                    cur.push(chars[i]);
                    i += 1;
                }
                i += 1; // closing quote (or end)
            }
            '\'' if dialect == Dialect::Powershell => {
                have = true;
                literal = true;
                i += 1;
                while i < chars.len() {
                    if chars[i] == '\'' {
                        if chars.get(i + 1) == Some(&'\'') {
                            cur.push('\'');
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    cur.push(chars[i]);
                    i += 1;
                }
                i += 1;
            }
            '|' | '>' | '<' | ';' => {
                // A stream-numbered redirection: 2>, 1>, *>, 2>&1.
                let mut op = String::new();
                if c == '>' && have && !literal && matches!(cur.as_str(), "1" | "2" | "*") {
                    op.push_str(&cur);
                    cur.clear();
                    have = false;
                }
                flush(&mut cur, &mut have, &mut literal, &mut out);
                op.push(c);
                if c == '>'
                    && chars.get(i + 1) == Some(&'&')
                    && chars.get(i + 2).is_some_and(|d| d.is_ascii_digit())
                {
                    op.push('&');
                    op.push(chars[i + 2]);
                    i += 2;
                }
                // 2>&1, >>, ||
                if c == '>' && chars.get(i + 1) == Some(&'>') {
                    op.push('>');
                    i += 1;
                }
                if c == '|' && chars.get(i + 1) == Some(&'|') {
                    op.push('|');
                    i += 1;
                }
                out.push(Tok {
                    text: op,
                    literal: true,
                    op: true,
                });
                i += 1;
            }
            '&' => {
                if chars.get(i + 1) == Some(&'&') {
                    flush(&mut cur, &mut have, &mut literal, &mut out);
                    out.push(Tok {
                        text: "&&".into(),
                        literal: true,
                        op: true,
                    });
                    i += 2;
                } else if dialect == Dialect::Cmd {
                    flush(&mut cur, &mut have, &mut literal, &mut out);
                    out.push(Tok {
                        text: "&&".into(),
                        literal: true,
                        op: true,
                    });
                    i += 1;
                } else {
                    // PowerShell call operator: its own token.
                    flush(&mut cur, &mut have, &mut literal, &mut out);
                    out.push(Tok {
                        text: "&".into(),
                        literal: true,
                        op: true,
                    });
                    i += 1;
                }
            }
            '`' if dialect == Dialect::Powershell && i + 1 < chars.len() => {
                cur.push(chars[i + 1]);
                have = true;
                i += 2;
            }
            _ => {
                cur.push(c);
                have = true;
                i += 1;
            }
        }
    }
    flush(&mut cur, &mut have, &mut literal, &mut out);
    out
}

fn is_op(t: &Tok) -> bool {
    t.op && t.text != "&"
}

/// A redirection that names no target (`2>&1`): dropped alone.
fn is_self_contained_redirect(t: &Tok) -> bool {
    t.op && t.text.contains(">&")
}

/// Split a token list on statement separators (`;`, `&&`).
fn statements(toks: Vec<Tok>) -> Vec<Vec<Tok>> {
    let mut out = vec![Vec::new()];
    for t in toks {
        if t.op && (t.text == ";" || t.text == "&&" || t.text == "||") {
            out.push(Vec::new());
        } else if let Some(last) = out.last_mut() {
            last.push(t);
        }
    }
    out.into_iter().filter(|s| !s.is_empty()).collect()
}

struct State<'a> {
    ctx: &'a ImportContext,
    dialect: Dialect,
    /// PowerShell variables → parameter names.
    vars: BTreeMap<String, String>,
    /// cmd `set` values (for `%X%` inline substitution).
    cmd_vars: BTreeMap<String, String>,
    params: Vec<Param>,
    env: BTreeMap<String, String>,
    workdir: Option<String>,
    warnings: Vec<String>,
}

fn norm_path(s: &str) -> String {
    s.replace('\\', "/")
        .trim_end_matches('/')
        .to_ascii_lowercase()
}

impl State<'_> {
    /// Rewrite `$env:X` / `%X%` / `$var` references inside a value.
    fn expand_refs(&mut self, s: &str, literal: bool) -> String {
        if literal {
            return s.replace('{', "{{").replace('}', "}}");
        }
        let mut out = String::new();
        let chars: Vec<char> = s.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if self.dialect == Dialect::Powershell && c == '$' {
                // $env:NAME
                let rest: String = chars[i + 1..].iter().collect();
                if rest.len() >= 4 && rest[..4].eq_ignore_ascii_case("env:") {
                    let name: String = rest[4..]
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                        .collect();
                    if !name.is_empty() {
                        out.push_str(&format!("{{env.{name}}}"));
                        i += 1 + 4 + name.len();
                        continue;
                    }
                }
                // ${name} / $($name)
                let (name, consumed) = if rest.starts_with('{') {
                    match rest.find('}') {
                        Some(e) => (rest[1..e].to_string(), e + 1),
                        None => (String::new(), 0),
                    }
                } else if rest.starts_with("($") {
                    match rest.find(')') {
                        Some(e) => (rest[2..e].to_string(), e + 1),
                        None => (String::new(), 0),
                    }
                } else {
                    let n: String = rest
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                        .collect();
                    let len = n.len();
                    (n, len)
                };
                if !name.is_empty() {
                    let key = name.to_ascii_lowercase();
                    if let Some(p) = self.vars.get(&key) {
                        out.push_str(&format!("{{{p}}}"));
                    } else {
                        self.warnings.push(format!(
                            "${name} is not assigned in the paste; kept as parameter {{{name}}}"
                        ));
                        let p = name.clone();
                        if !self.params.iter().any(|x| x.name == p) {
                            self.params.push(Param {
                                name: p.clone(),
                                default: None,
                                description: "referenced by the paste but never assigned".into(),
                            });
                        }
                        self.vars.insert(key, p.clone());
                        out.push_str(&format!("{{{p}}}"));
                    }
                    i += 1 + consumed;
                    continue;
                }
            }
            if self.dialect == Dialect::Cmd && c == '%' {
                let rest: String = chars[i + 1..].iter().collect();
                if let Some(e) = rest.find('%') {
                    let name = &rest[..e];
                    if !name.is_empty()
                        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    {
                        match self.cmd_vars.get(&name.to_ascii_uppercase()) {
                            Some(v) => out.push_str(v),
                            None => out.push_str(&format!("{{env.{name}}}")),
                        }
                        i += e + 2;
                        continue;
                    }
                }
            }
            if c == '{' {
                out.push_str("{{");
            } else if c == '}' {
                out.push_str("}}");
            } else {
                out.push(c);
            }
            i += 1;
        }
        self.templatize_path(&out)
    }

    /// Rewrite a path under the managed install to its template.
    fn templatize_path(&self, s: &str) -> String {
        let mut probe = TemplateProbe { env: &self.ctx.env };
        let expanded = probe.expand_env(s);
        let low = norm_path(&expanded);
        let roots = [
            (&self.ctx.llamacpp_dir, "{llamacpp_bin}"),
            (&self.ctx.models_dir, "{models_dir}"),
            (&self.ctx.data_dir, "{data_dir}"),
        ];
        for (root, tpl) in roots {
            if root.is_empty() {
                continue;
            }
            let r = norm_path(root);
            if low == r {
                return tpl.to_string();
            }
            if low.starts_with(&format!("{r}/")) {
                let tail = &expanded.replace('\\', "/")[r.len()..];
                return format!("{tpl}{tail}");
            }
        }
        s.to_string()
    }

    fn resolve_exe(&mut self, raw: &str, line_no: usize) -> String {
        let cleaned = raw.trim();
        let file = cleaned
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(cleaned)
            .to_string();
        let lower = file.to_ascii_lowercase();
        let with_exe = if lower.ends_with(".exe") {
            lower.clone()
        } else {
            format!("{lower}.exe")
        };
        let bare = lower.trim_end_matches(".exe").to_string();
        let is_relative = cleaned.starts_with(".\\") || cleaned.starts_with("./");
        let has_dir = cleaned.contains(['/', '\\']) && !is_relative;
        // A path that already resolves under the managed install.
        if has_dir {
            let t = self.templatize_path(cleaned);
            if t != cleaned {
                return t;
            }
        }
        if let Some(tool) = self
            .ctx
            .managed_tools
            .iter()
            .find(|t| {
                let tl = t.to_ascii_lowercase();
                tl == with_exe || tl == lower || tl.trim_end_matches(".exe") == bare
            })
            .cloned()
        {
            if has_dir {
                self.warnings.push(format!(
                    "line {line_no}: {cleaned} points outside the managed llama.cpp install; using the managed {tool}"
                ));
            }
            return format!("{{llamacpp_bin}}/{tool}");
        }
        if has_dir {
            return cleaned.to_string();
        }
        if !self
            .ctx
            .path_tools
            .iter()
            .any(|t| t == &lower || t == &with_exe || t == &bare)
        {
            self.warnings.push(format!(
                "line {line_no}: {file} is neither in the managed llama.cpp install nor on PATH"
            ));
        }
        bare
    }
}

/// `{env.X}` expansion for path templating only.
struct TemplateProbe<'a> {
    env: &'a BTreeMap<String, String>,
}

impl TemplateProbe<'_> {
    fn expand_env(&mut self, s: &str) -> String {
        let mut out = s.to_string();
        while let Some(start) = out.find("{env.") {
            let Some(end) = out[start..].find('}') else {
                break;
            };
            let name = &out[start + 5..start + end];
            let Some(v) = self
                .env
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone())
            else {
                break;
            };
            out.replace_range(start..start + end + 1, &v);
        }
        out
    }
}

fn comment_expect(c: &str) -> Option<Expect> {
    let l = c.to_ascii_lowercase();
    if l.contains("crash") || l.contains("abort") || l.contains("segfault") {
        Some(Expect::MayCrash)
    } else if l.contains("fail") || l.contains("error") {
        Some(Expect::MayFail)
    } else {
        None
    }
}

fn parser_for(exe: &str) -> Option<ParserKind> {
    let l = exe.to_ascii_lowercase();
    if l.contains("llama-bench") {
        Some(ParserKind::LlamaBench)
    } else if l.contains("llama-server") || l.contains("llama-cli") {
        Some(ParserKind::ServerTimings)
    } else {
        None
    }
}

fn step_name(exe: &str, n: usize) -> String {
    let file = exe.rsplit(['/', '\\']).next().unwrap_or(exe);
    format!("{}-{n}", file.trim_end_matches(".exe"))
}

/// Parse a pasted block. `fold` folds same-tool runs into variants.
pub fn import(
    text: &str,
    ctx: &ImportContext,
    name_hint: Option<&str>,
    fold: bool,
) -> ImportResult {
    let dialect = detect_dialect(text);
    let mut st = State {
        ctx,
        dialect,
        vars: BTreeMap::new(),
        cmd_vars: BTreeMap::new(),
        params: Vec::new(),
        env: BTreeMap::new(),
        workdir: None,
        warnings: Vec::new(),
    };
    let mut steps: Vec<Step> = Vec::new();
    let mut pending_comment: Vec<String> = Vec::new();
    for l in logical_lines(text, dialect) {
        let t = l.text.trim();
        if t.is_empty() {
            if let Some(c) = &l.comment {
                pending_comment.push(c.clone());
            }
            continue;
        }
        let lower = t.to_ascii_lowercase();
        if dialect == Dialect::Cmd
            && (lower == "rem" || lower.starts_with("rem ") || t.starts_with("::"))
        {
            let c = match t.strip_prefix("::") {
                Some(rest) => rest.trim(),
                None => t[3..].trim(),
            };
            if !c.is_empty() {
                pending_comment.push(c.to_string());
            }
            continue;
        }
        if dialect == Dialect::Cmd && lower.starts_with("@echo") {
            continue;
        }
        for stmt in statements(tokenize(t, dialect)) {
            handle_statement(&mut st, stmt, &l, &mut pending_comment, &mut steps);
        }
    }
    if steps.is_empty() {
        st.warnings
            .push("no runnable command found in the paste".to_string());
    }
    let name = name_hint.map(crate::recipe::slugify).unwrap_or_else(|| {
        steps
            .first()
            .map(|s| format!("paste-{}", crate::recipe::slugify(&step_name(&s.exe, 0))))
            .unwrap_or_else(|| "paste".to_string())
            .trim_end_matches("-0")
            .to_string()
    });
    let unfolded = Recipe {
        name: name.clone(),
        title: name_hint.unwrap_or("Imported paste").to_string(),
        description: "Imported from a pasted command block; review before running.".into(),
        version: crate::recipe::RECIPE_VERSION,
        params: st.params.clone(),
        env: st.env.clone(),
        requires: Requires::default(),
        steps: steps.clone(),
        builtin: false,
        source: Some("paste".into()),
    };
    let (folded_steps, folds) = if fold {
        fold_steps(&steps)
    } else {
        (steps, Vec::new())
    };
    let recipe = Recipe {
        steps: folded_steps,
        ..unfolded.clone()
    };
    ImportResult {
        dialect,
        recipe,
        unfolded,
        folds,
        warnings: st.warnings,
    }
}

fn handle_statement(
    st: &mut State<'_>,
    stmt: Vec<Tok>,
    l: &Logical,
    pending_comment: &mut Vec<String>,
    steps: &mut Vec<Step>,
) {
    let line_no = l.line_no;
    let first = &stmt[0];
    let first_lower = first.text.to_ascii_lowercase();
    // --- assignments -----------------------------------------------------
    if st.dialect == Dialect::Powershell && first.text.starts_with('$') && !first.literal {
        // Re-join: `$x = "v"`, `$x="v"`, `$env:X=1`.
        let (lhs, rhs_tok) = split_assignment(&stmt);
        if let Some((lhs, rhs)) = lhs.zip(rhs_tok) {
            let value = st.expand_refs(&rhs.text, rhs.literal);
            if lhs.len() > 5 && lhs[..5].eq_ignore_ascii_case("$env:") {
                // An environment setting applies to every step; a
                // comment above it still describes the next command.
                let key = lhs[5..].to_string();
                st.env.insert(key, value);
                return;
            } else {
                let name = lhs.trim_start_matches('$').to_string();
                let key = name.to_ascii_lowercase();
                let pname = name.clone();
                st.vars.insert(key, pname.clone());
                match st.params.iter_mut().find(|p| p.name == pname) {
                    Some(p) => p.default = Some(value),
                    None => st.params.push(Param {
                        name: pname,
                        default: Some(value),
                        description: pending_comment.join(" "),
                    }),
                }
            }
            pending_comment.clear();
            return;
        }
    }
    if st.dialect == Dialect::Cmd && first_lower == "set" && stmt.len() >= 2 {
        let joined: String = stmt[1..]
            .iter()
            .map(|t| t.text.clone())
            .collect::<Vec<_>>()
            .join(" ");
        if let Some((k, v)) = joined.split_once('=') {
            let k = k.trim().to_string();
            let v = st.expand_refs(v.trim(), false);
            st.cmd_vars.insert(k.to_ascii_uppercase(), v.clone());
            st.env.insert(k, v);
            return;
        }
    }
    // --- directory changes ------------------------------------------------
    if matches!(
        first_lower.as_str(),
        "cd" | "set-location" | "pushd" | "sl" | "chdir"
    ) {
        let target: Vec<&Tok> = stmt[1..]
            .iter()
            .filter(|t| !t.text.eq_ignore_ascii_case("/d"))
            .collect();
        if let Some(t) = target.first() {
            let d = st.expand_refs(&t.text, t.literal);
            st.workdir = Some(d);
        }
        return;
    }
    if matches!(
        first_lower.as_str(),
        "echo" | "write-host" | "write-output" | "cls" | "clear"
    ) {
        st.warnings.push(format!(
            "line {line_no}: `{}` skipped (no effect on a recipe)",
            first.text
        ));
        return;
    }
    // --- invocation -------------------------------------------------------
    let mut toks: Vec<Tok> = stmt;
    if toks[0].op && toks[0].text == "&" {
        toks.remove(0);
        if toks.is_empty() {
            return;
        }
    }
    // Drop pipes/redirections (and everything after a pipe).
    let mut clean: Vec<Tok> = Vec::new();
    let mut skip_next = false;
    for t in toks {
        if skip_next {
            skip_next = false;
            continue;
        }
        if is_self_contained_redirect(&t) {
            st.warnings
                .push(format!("line {line_no}: redirection `{}` dropped", t.text));
            continue;
        }
        if is_op(&t) {
            if t.text == "|" {
                st.warnings.push(format!(
                    "line {line_no}: pipeline after `|` dropped (output is captured and parsed instead)"
                ));
                break;
            }
            st.warnings.push(format!(
                "line {line_no}: redirection `{}` dropped (output is captured per step)",
                t.text
            ));
            skip_next = true;
            continue;
        }
        clean.push(t);
    }
    if clean.is_empty() {
        return;
    }
    let exe = st.resolve_exe(&clean[0].text, line_no);
    let args: Vec<String> = clean[1..]
        .iter()
        .map(|t| st.expand_refs(&t.text, t.literal))
        .collect();
    let mut desc: Vec<String> = std::mem::take(pending_comment);
    if let Some(c) = &l.comment {
        desc.push(c.clone());
    }
    let description = desc.join(" ");
    let expect = l
        .comment
        .as_deref()
        .and_then(comment_expect)
        .or_else(|| {
            if description.is_empty() {
                None
            } else {
                comment_expect(&description)
            }
        })
        .unwrap_or(Expect::Success);
    let mut step = Step::exec(&step_name(&exe, steps.len() + 1), &exe, &[]);
    step.args = args;
    step.description = description;
    step.expect = expect;
    step.workdir = st.workdir.clone();
    step.parser = parser_for(&exe);
    step.kind = StepKind::Exec;
    steps.push(step);
}

/// Split `$x = "v"` / `$x="v"` / `$x= v` into (lhs, value token).
fn split_assignment(stmt: &[Tok]) -> (Option<String>, Option<Tok>) {
    let first = &stmt[0].text;
    if let Some((l, r)) = first.split_once('=') {
        if !r.is_empty() {
            return (
                Some(l.to_string()),
                Some(Tok {
                    text: r.to_string(),
                    literal: stmt[0].literal,
                    op: false,
                }),
            );
        }
        // `$x= "v"`
        return (Some(l.to_string()), stmt.get(1).cloned());
    }
    if stmt.get(1).map(|t| t.text.as_str()) == Some("=") {
        return (Some(first.clone()), stmt.get(2).cloned());
    }
    if let Some(t) = stmt.get(1) {
        if let Some(rest) = t.text.strip_prefix('=') {
            if rest.is_empty() {
                return (Some(first.clone()), stmt.get(2).cloned());
            }
            return (
                Some(first.clone()),
                Some(Tok {
                    text: rest.to_string(),
                    literal: t.literal,
                    op: false,
                }),
            );
        }
    }
    (None, None)
}

fn positional(args: &[String]) -> Vec<String> {
    argv::group(args)
        .into_iter()
        .filter_map(|i| match i {
            ArgItem::Positional(p) => Some(p),
            _ => None,
        })
        .collect()
}

fn flag_map(args: &[String]) -> Vec<(String, Option<String>)> {
    argv::group(args)
        .into_iter()
        .filter_map(|i| match i {
            ArgItem::Flag { name, value, .. } => Some((name, value)),
            _ => None,
        })
        .collect()
}

/// Can `b` be expressed as overrides on `a`? Same executable, same
/// working dir, same positionals, no repeated flags.
fn foldable(a: &Step, b: &Step) -> bool {
    if a.exe != b.exe || a.workdir != b.workdir || a.kind != b.kind || a.env != b.env {
        return false;
    }
    if positional(&a.args) != positional(&b.args) {
        return false;
    }
    let dup = |s: &Step| {
        let f = flag_map(&s.args);
        let mut names: Vec<&String> = f.iter().map(|(n, _)| n).collect();
        names.sort();
        let len = names.len();
        names.dedup();
        names.len() != len
    };
    !dup(a) && !dup(b)
}

fn overrides(base: &Step, other: &Step) -> (BTreeMap<String, String>, Vec<String>) {
    let bf = flag_map(&base.args);
    let of = flag_map(&other.args);
    let mut set = BTreeMap::new();
    let mut unset = Vec::new();
    for (n, v) in &of {
        match bf.iter().find(|(bn, _)| bn == n) {
            Some((_, bv)) if bv == v => {}
            _ => {
                set.insert(n.clone(), v.clone().unwrap_or_default());
            }
        }
    }
    for (n, _) in &bf {
        if !of.iter().any(|(on, _)| on == n) {
            unset.push(n.clone());
        }
    }
    (set, unset)
}

fn fold_steps(steps: &[Step]) -> (Vec<Step>, Vec<FoldInfo>) {
    let mut out: Vec<Step> = Vec::new();
    let mut folds = Vec::new();
    let mut i = 0;
    while i < steps.len() {
        let mut j = i + 1;
        while j < steps.len() && foldable(&steps[i], &steps[j]) {
            j += 1;
        }
        if j - i < 2 {
            out.push(steps[i].clone());
            i += 1;
            continue;
        }
        let group = &steps[i..j];
        let base = &group[0];
        // The flags that vary anywhere in the group name each variant.
        let mut varying: Vec<String> = Vec::new();
        for s in &group[1..] {
            let (set, unset) = overrides(base, s);
            for k in set.keys().chain(unset.iter()) {
                if !varying.contains(k) {
                    varying.push(k.clone());
                }
            }
        }
        let label = |s: &Step| -> String {
            let fm = flag_map(&s.args);
            let parts: Vec<String> = varying
                .iter()
                .map(|f| match fm.iter().find(|(n, _)| n == f) {
                    Some((_, Some(v))) => format!("{f} {v}"),
                    Some((_, None)) => f.clone(),
                    None => format!("no {f}"),
                })
                .collect();
            parts.join(" ")
        };
        let mut folded = base.clone();
        folded.name =
            step_name(&base.exe, out.len() + 1).replace(&format!("-{}", out.len() + 1), "");
        folded.description = format!(
            "{} variants folded from the paste (they differ only in {})",
            group.len(),
            varying.join(", ")
        );
        folded.expect = Expect::Success;
        folded.variants = group
            .iter()
            .map(|s| {
                let (set, unset) = overrides(base, s);
                Variant {
                    name: label(s),
                    description: s.description.clone(),
                    set,
                    unset,
                    env: BTreeMap::new(),
                    expect: Some(s.expect),
                }
            })
            .collect();
        folds.push(FoldInfo {
            step_index: out.len(),
            folded: group.len(),
            exe: base.exe.clone(),
            varying,
        });
        out.push(folded);
        i = j;
    }
    (out, folds)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) const EXAMPLE: &str = r#"$env:CUDA_LAUNCH_BLOCKING=1
$m="$env:APPDATA\local-llm-service-manager\models\Whittle-MoE-27B-A18B-v2.2.1-Q4_K_M.gguf"
.\llama-bench.exe -m $m -ngl 99 -ot exps=CPU -p 512 -n 0 -d 0,4096,8192 -fa 1 -ctk q8_0 -ctv q8_0   # expect crash at d=8192
.\llama-bench.exe -m $m -ngl 99 -ot exps=CPU -p 512 -n 0 -d 8192 -fa 1 -ctk f16 -ctv f16
.\llama-bench.exe -m $m -ngl 99 -ot exps=CPU -p 512 -n 0 -d 8192 -fa 0 -ctk q8_0 -ctv f16
"#;

    pub(crate) fn ctx() -> ImportContext {
        let appdata = "C:\\Users\\Peter\\AppData\\Roaming".to_string();
        let data = format!("{appdata}\\local-llm-service-manager");
        ImportContext {
            data_dir: data.clone(),
            models_dir: format!("{data}\\models"),
            llamacpp_dir: format!("{data}\\llamacpp"),
            managed_tools: vec![
                "llama-bench.exe".into(),
                "llama-server.exe".into(),
                "llama-gguf.exe".into(),
            ],
            env: [("APPDATA".to_string(), appdata)].into(),
            path_tools: vec!["nvidia-smi".into(), "nvidia-smi.exe".into()],
        }
    }

    #[test]
    fn motivating_example_becomes_a_three_variant_recipe() {
        let r = import(EXAMPLE, &ctx(), Some("fa kv depth ab"), true);
        assert_eq!(r.dialect, Dialect::Powershell);
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
        let rec = &r.recipe;
        rec.validate().unwrap();
        assert_eq!(rec.name, "fa-kv-depth-ab");
        assert_eq!(
            rec.env.get("CUDA_LAUNCH_BLOCKING").map(String::as_str),
            Some("1")
        );
        assert_eq!(rec.params.len(), 1);
        assert_eq!(rec.params[0].name, "m");
        assert_eq!(
            rec.params[0].default.as_deref(),
            Some("{models_dir}/Whittle-MoE-27B-A18B-v2.2.1-Q4_K_M.gguf")
        );
        assert_eq!(rec.steps.len(), 1);
        let s = &rec.steps[0];
        assert_eq!(s.exe, "{llamacpp_bin}/llama-bench.exe");
        assert_eq!(s.parser, Some(ParserKind::LlamaBench));
        assert_eq!(s.args[1], "{m}");
        assert_eq!(s.variants.len(), 3);
        assert_eq!(s.variants[0].expect, Some(Expect::MayCrash));
        assert!(s.variants[0].description.contains("expect crash"));
        assert!(s.variants[0].set.is_empty());
        assert_eq!(
            s.variants[1].set.get("-d").map(String::as_str),
            Some("8192")
        );
        assert_eq!(
            s.variants[1].set.get("-ctk").map(String::as_str),
            Some("f16")
        );
        assert_eq!(s.variants[2].set.get("-fa").map(String::as_str), Some("0"));
        assert!(!s.variants[2].set.contains_key("-ctk"));
        assert_eq!(r.folds.len(), 1);
        assert_eq!(r.folds[0].folded, 3);
        assert_eq!(r.unfolded.steps.len(), 3);
        // Every planned execution carries the env.
        let planned = rec.expand(None).unwrap();
        assert_eq!(planned.len(), 3);
        for p in &planned {
            assert_eq!(p.env["CUDA_LAUNCH_BLOCKING"], "1");
        }
        assert_eq!(
            planned[2].args,
            "-m {m} -ngl 99 -ot exps=CPU -p 512 -n 0 -d 8192 -fa 0 -ctk q8_0 -ctv f16"
                .split(' ')
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn cmd_syntax_with_set_rem_and_caret_continuation() {
        let text = "@echo off\r\nREM snapshot first\r\nset CUDA_LAUNCH_BLOCKING=1\r\nset M=C:\\Users\\Peter\\AppData\\Roaming\\local-llm-service-manager\\models\\x.gguf\r\nllama-bench.exe -m %M% ^\r\n  -ngl 99 -p 512 > out.txt\r\nnvidia-smi && nvidia-smi -q\r\n";
        let r = import(text, &ctx(), None, false);
        assert_eq!(r.dialect, Dialect::Cmd);
        let rec = &r.recipe;
        assert_eq!(rec.env["CUDA_LAUNCH_BLOCKING"], "1");
        assert_eq!(rec.steps.len(), 3);
        assert_eq!(rec.steps[0].description, "snapshot first");
        assert_eq!(
            rec.steps[0].args,
            vec!["-m", "{models_dir}/x.gguf", "-ngl", "99", "-p", "512"]
        );
        assert!(r.warnings.iter().any(|w| w.contains("redirection")));
        assert_eq!(rec.steps[1].exe, "nvidia-smi");
        assert_eq!(rec.steps[2].args, vec!["-q"]);
    }

    #[test]
    fn powershell_continuations_quotes_and_call_operator() {
        let text = "# dump metadata\n& \"C:\\Users\\Peter\\AppData\\Roaming\\local-llm-service-manager\\llamacpp\\llama-gguf.exe\" `\n  'C:\\odd path\\it''s.gguf' r n\n.\\llama-server.exe --host 0.0.0.0 -p \"a \"\"quoted\"\" word\" 2>&1 | Select-String error";
        let r = import(text, &ctx(), None, true);
        let rec = &r.recipe;
        assert_eq!(rec.steps.len(), 2, "{:?}", rec.steps);
        assert_eq!(rec.steps[0].exe, "{llamacpp_bin}/llama-gguf.exe");
        assert_eq!(rec.steps[0].args, vec!["C:\\odd path\\it's.gguf", "r", "n"]);
        assert_eq!(rec.steps[0].description, "dump metadata");
        assert_eq!(
            rec.steps[1].args,
            vec!["--host", "0.0.0.0", "-p", "a \"quoted\" word"]
        );
        assert!(r.warnings.iter().any(|w| w.contains("pipeline")));
        assert!(r.folds.is_empty(), "different tools never fold");
    }

    #[test]
    fn unknown_vars_become_required_params_and_braces_escape() {
        let text = "llama-server -m $model --chat-template-kwargs '{\"x\":1}'";
        let r = import(text, &ctx(), None, true);
        let rec = &r.recipe;
        assert_eq!(rec.params[0].name, "model");
        assert!(rec.params[0].default.is_none());
        assert_eq!(rec.steps[0].args[1], "{model}");
        // Literal braces are escaped for the template engine and
        // resolve back to themselves.
        assert_eq!(rec.steps[0].args[3], "{{\"x\":1}}");
        let r2 = import("nvidia-smi --format=\"{x}\"", &ctx(), None, true);
        assert_eq!(r2.recipe.steps[0].args[0], "--format={{x}}");
    }

    #[test]
    fn folding_needs_same_positionals() {
        let text = "nvidia-smi -q -d MEMORY\nnvidia-smi -q -d UTILIZATION\nnvidia-smi topo -m";
        let r = import(text, &ctx(), None, true);
        assert_eq!(r.recipe.steps.len(), 2);
        assert_eq!(r.recipe.steps[0].variants.len(), 2);
        assert_eq!(r.recipe.steps[0].variants[1].name, "-d UTILIZATION");
    }
}
