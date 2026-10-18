//! The screening evaluation suite (design addendum A.4,
//! tracker #28 Phase 5d): a small fixed set of checkable tasks,
//! shipped with the app and versioned, run identically
//! (temperature 0, per-task token caps) against an incumbent
//! backend to establish a baseline and against each experiment
//! candidate to screen it.
//!
//! Every task scores by machine, never by a model's judgment:
//! each task carries a [`Checker`] whose verdict is a pure
//! function of the model's output text. Executing generated
//! code is out for v1 (decision A.8.4) — the code-adjacent
//! tasks are structured transformations and extraction whose
//! results are exactly checkable as text.
//!
//! Scores are a screen, not a ranking: [`SCREENING_CAVEAT`] is
//! stated wherever scores appear.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::context::estimate_tokens;
use crate::measure::LONG_PROMPT;

/// The caveat stated wherever screening scores appear (§A.4),
/// verbatim.
pub const SCREENING_CAVEAT: &str = "Screening suite — a small fixed set of checkable tasks. It screens out regressions; it is not a benchmark and does not rank models generally.";

/// Version of the shipped suite. Any change to a task's prompt,
/// cap, or checker is a suite change and bumps this — scores
/// from different suite versions are not comparable, and a
/// stored baseline from another version is stale.
pub const EVAL_SUITE_VERSION: u32 = 1;

/// Temperature every eval run uses (§A.4: identical settings).
pub const EVAL_TEMPERATURE: f32 = 0.0;
/// Per-task output-token cap ceiling (§A.4 budget).
pub const MAX_TASK_OUTPUT_TOKENS: u32 = 2048;
/// Whole-suite token budget per backend (§A.4 budget): the
/// worst case — every task's estimated prompt tokens plus its
/// output cap — must stay within it.
pub const SUITE_TOKEN_BUDGET: usize = 30_000;
/// Per-backend time-box for a full suite run (§A.4 budget),
/// enforced by the runner in the src-tauri glue.
pub const EVAL_TIME_BOX_SECS: u64 = 600;

// ---------------------------------------------------------------------------
// Categories
// ---------------------------------------------------------------------------

/// The seven task categories of §A.4. Serialized snake_case —
/// those strings are the `per_category` keys in every payload
/// and stored record, so they are part of the wire contract
/// the experiment phase (5e) renders deltas against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalCategory {
    InstructionFollowing,
    FactualRecall,
    ArithmeticReasoning,
    CodeAdjacent,
    Summarization,
    ToolCallFormatting,
    LongContextRetrieval,
}

impl EvalCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            EvalCategory::InstructionFollowing => "instruction_following",
            EvalCategory::FactualRecall => "factual_recall",
            EvalCategory::ArithmeticReasoning => "arithmetic_reasoning",
            EvalCategory::CodeAdjacent => "code_adjacent",
            EvalCategory::Summarization => "summarization",
            EvalCategory::ToolCallFormatting => "tool_call_formatting",
            EvalCategory::LongContextRetrieval => "long_context_retrieval",
        }
    }

    pub fn all() -> [EvalCategory; 7] {
        [
            EvalCategory::InstructionFollowing,
            EvalCategory::FactualRecall,
            EvalCategory::ArithmeticReasoning,
            EvalCategory::CodeAdjacent,
            EvalCategory::Summarization,
            EvalCategory::ToolCallFormatting,
            EvalCategory::LongContextRetrieval,
        ]
    }
}

// ---------------------------------------------------------------------------
// Checkers
// ---------------------------------------------------------------------------

/// The JSON value type a [`Checker::JsonShape`] field must have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JsonType {
    String,
    Number,
    Boolean,
    Object,
    Array,
    Null,
    Any,
}

impl JsonType {
    fn matches(&self, value: &serde_json::Value) -> bool {
        match self {
            JsonType::String => value.is_string(),
            JsonType::Number => value.is_number(),
            JsonType::Boolean => value.is_boolean(),
            JsonType::Object => value.is_object(),
            JsonType::Array => value.is_array(),
            JsonType::Null => value.is_null(),
            JsonType::Any => true,
        }
    }
}

/// One required key of a [`Checker::JsonShape`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonFieldSpec {
    pub key: String,
    #[serde(rename = "type")]
    pub ty: JsonType,
}

/// A per-line rule for [`Checker::LineCount`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum LinePattern {
    /// Every line is non-empty after trimming.
    NonEmpty,
    /// Every line is a single whitespace-free token.
    SingleToken,
    /// Every line is one of the given strings (exact, trimmed).
    OneOf { values: Vec<String> },
    /// Line `i` is exactly `values[i]` (exact, trimmed) — a
    /// positional per-line pattern.
    Sequence { values: Vec<String> },
    /// Every line starts with the given prefix.
    StartsWith { prefix: String },
    /// Every line contains the given substring.
    Contains { needle: String },
}

impl LinePattern {
    fn matches(&self, line: &str, index: usize) -> bool {
        let line = line.trim();
        match self {
            LinePattern::NonEmpty => !line.is_empty(),
            LinePattern::SingleToken => !line.is_empty() && !line.chars().any(char::is_whitespace),
            LinePattern::OneOf { values } => values.iter().any(|v| v == line),
            LinePattern::Sequence { values } => {
                values.get(index).map(|v| v == line).unwrap_or(false)
            }
            LinePattern::StartsWith { prefix } => line.starts_with(prefix.as_str()),
            LinePattern::Contains { needle } => line.contains(needle.as_str()),
        }
    }
}

/// How one task's output is scored. Every variant is a pure
/// function of the output text — no model judgment anywhere.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Checker {
    /// Normalized exact match. Normalization (applied to both
    /// sides): trim, strip one layer of surrounding quotes,
    /// lowercase, collapse all whitespace runs to single
    /// spaces, drop spaces adjacent to JSON punctuation
    /// (`{ } [ ] : ,` — so compact and spaced JSON spellings
    /// of the same value compare equal), strip trailing
    /// punctuation (`. ! ? ; : ,`). So `Paris.` and ` paris `
    /// pass for an expected `Paris`; `Paris, France` does
    /// not — interior content must match exactly.
    ExactMatch { expected: String },
    /// The first numeric token in the output (digits with
    /// optional sign, thousands separators, and one decimal
    /// point — `1,408`, `408.0`, and `The answer is 408.` all
    /// yield a number) must be within `tolerance` of
    /// `expected`.
    Numeric { expected: f64, tolerance: f64 },
    /// The output's first JSON value (prose or code fences
    /// around it are tolerated, matching how the plan parser
    /// salvages model output) must be an object carrying every
    /// required key with the required value type. Values
    /// themselves are not checked — shape only.
    JsonShape { required: Vec<JsonFieldSpec> },
    /// The output (trailing blank lines ignored) has exactly
    /// `count` lines, and every line satisfies `per_line` when
    /// one is set.
    LineCount {
        count: usize,
        #[serde(default)]
        per_line: Option<LinePattern>,
    },
    /// At least `min_fraction` of `keywords` appear in the
    /// output (case-insensitive substring match).
    KeywordCoverage {
        keywords: Vec<String>,
        min_fraction: f64,
    },
    /// The output matches `pattern`. The matcher is a small
    /// hand-rolled engine (this workspace has no `regex`
    /// dependency, and adding one for a fixed suite is not
    /// warranted) supporting exactly: `^` / `$` anchors,
    /// literal characters, `.`, character classes (`[A-Z]`,
    /// `[0-9a-z]`, negated `[^...]`), the escapes `\d \w \s`
    /// (and escaped literals), and the quantifiers `{n}`,
    /// `{n,m}`, `{n,}`, `*`, `+`, `?` applied to the preceding
    /// atom. Anything outside that subset fails to parse and
    /// the check fails closed.
    Regex { pattern: String },
    /// The output's first JSON value is a tool call for
    /// `function` carrying every required argument key.
    /// Accepted shapes (the forms local models actually emit):
    /// `{"name": fn, "arguments": {...}}`,
    /// `{"function": fn, "parameters": {...}}`, and a
    /// `{"tool_calls": [ {...}, ... ]}` wrapper whose first
    /// entry (or its nested `function` object) names the
    /// function. `arguments` given as a JSON string is parsed.
    ToolCallSchema {
        function: String,
        required_arguments: Vec<String>,
    },
}

// ---------------------------------------------------------------------------
// The suite
// ---------------------------------------------------------------------------

/// One screening task: the exact prompt sent, the output-token
/// cap it runs under, and the machine checker that scores it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalTask {
    pub id: String,
    pub category: EvalCategory,
    pub prompt: String,
    pub max_tokens: u32,
    pub checker: Checker,
}

fn task(
    id: &str,
    category: EvalCategory,
    prompt: impl Into<String>,
    max_tokens: u32,
    checker: Checker,
) -> EvalTask {
    EvalTask {
        id: id.to_string(),
        category,
        prompt: prompt.into(),
        max_tokens,
        checker,
    }
}

/// The long-context retrieval prompt (§A.4): the §A.1 long
/// prompt's filler with a unique needle sentence embedded at
/// its midpoint, then the needle question. Deterministic —
/// the split is at a fixed character offset rounded down to a
/// word boundary.
fn long_context_prompt() -> String {
    let mid = LONG_PROMPT.len() / 2;
    let split = LONG_PROMPT[..mid].rfind(' ').map(|i| i + 1).unwrap_or(mid);
    format!(
        "{}The station archive code for the winter inspection is QUARTZ-7319. {} Question: what is the station archive code for the winter inspection stated in the records above? Reply with the code only.",
        &LONG_PROMPT[..split],
        &LONG_PROMPT[split..]
    )
}

/// The shipped suite (v1): exactly 12 tasks spanning all seven
/// §A.4 categories. Fixed text, fixed caps, fixed checkers —
/// a suite change is a version bump ([`EVAL_SUITE_VERSION`]).
pub fn eval_suite() -> Vec<EvalTask> {
    use EvalCategory::*;
    vec![
        task(
            "if-json-shape",
            InstructionFollowing,
            "Reply with a single JSON object and nothing else. The object must have exactly these keys: \"name\" (a string), \"kind\" (a string), and \"crew\" (a number). Use these values: name is Sea Whisper, kind is sailboat, crew is 4.",
            128,
            Checker::JsonShape {
                required: vec![
                    JsonFieldSpec { key: "name".to_string(), ty: JsonType::String },
                    JsonFieldSpec { key: "kind".to_string(), ty: JsonType::String },
                    JsonFieldSpec { key: "crew".to_string(), ty: JsonType::Number },
                ],
            },
        ),
        task(
            "if-line-count",
            InstructionFollowing,
            "Reply with exactly three lines, one word per line, in this order: north, south, west. No other text.",
            32,
            Checker::LineCount {
                count: 3,
                per_line: Some(LinePattern::Sequence {
                    values: vec![
                        "north".to_string(),
                        "south".to_string(),
                        "west".to_string(),
                    ],
                }),
            },
        ),
        task(
            "if-pattern",
            InstructionFollowing,
            "Reply with exactly one code and nothing else. The code is the letters XYZ, then a hyphen, then the digits 7429.",
            32,
            Checker::Regex {
                pattern: "^[A-Z]{3}-[0-9]{4}$".to_string(),
            },
        ),
        task(
            "fact-capital",
            FactualRecall,
            "What is the capital of France? Reply with the city name only.",
            16,
            Checker::ExactMatch {
                expected: "Paris".to_string(),
            },
        ),
        task(
            "fact-water-formula",
            FactualRecall,
            "What is the chemical formula for water? Reply with the formula only.",
            16,
            Checker::ExactMatch {
                expected: "H2O".to_string(),
            },
        ),
        task(
            "math-multiply",
            ArithmeticReasoning,
            "Compute 17 * 24. Reply with the number only.",
            16,
            Checker::Numeric {
                expected: 408.0,
                tolerance: 0.001,
            },
        ),
        task(
            "math-word-problem",
            ArithmeticReasoning,
            "A van carries 3 crates with 14 bottles in each crate. 5 bottles are removed. How many bottles remain? Reply with the number only.",
            32,
            Checker::Numeric {
                expected: 37.0,
                tolerance: 0.001,
            },
        ),
        task(
            "code-extract-json",
            CodeAdjacent,
            "Extract the ages from this text into a single JSON object and nothing else, with keys in this order: ada, grace, lin. Text: Ada is 36 years old. Grace is 85 years old. Lin is 29 years old.",
            96,
            Checker::ExactMatch {
                expected: "{\"ada\": 36, \"grace\": 85, \"lin\": 29}".to_string(),
            },
        ),
        task(
            "code-sort-words",
            CodeAdjacent,
            "Sort these words alphabetically and reply with them as a single comma-separated line and nothing else: banana, apple, cherry.",
            32,
            Checker::ExactMatch {
                expected: "apple, banana, cherry".to_string(),
            },
        ),
        task(
            "summary-garden",
            Summarization,
            "Summarize the following passage in at most two sentences. Your summary must cover the passage's main points.\n\nPassage: The community garden collects rainwater in barrels beside the shed, and volunteers turn kitchen scraps into compost for the raised beds. Bees and other pollinators visit the flower borders planted along the fence. By late summer the raised beds produce a harvest of tomatoes, beans, and squash that is shared among the neighborhood families.",
            180,
            Checker::KeywordCoverage {
                keywords: vec![
                    "rainwater".to_string(),
                    "compost".to_string(),
                    "pollinators".to_string(),
                    "raised beds".to_string(),
                    "harvest".to_string(),
                ],
                min_fraction: 0.8,
            },
        ),
        task(
            "tool-call-weather",
            ToolCallFormatting,
            "You have a function available: get_weather(city: string, units: string) — returns the current weather for a city. Emit a single JSON tool call and nothing else: call get_weather for the city Orrville with units imperial, as a JSON object with the function name and an arguments object.",
            128,
            Checker::ToolCallSchema {
                function: "get_weather".to_string(),
                required_arguments: vec!["city".to_string(), "units".to_string()],
            },
        ),
        task(
            "long-context-needle",
            LongContextRetrieval,
            long_context_prompt(),
            32,
            Checker::ExactMatch {
                expected: "QUARTZ-7319".to_string(),
            },
        ),
    ]
}

/// The suite's total worst-case token cost for one backend:
/// every task's estimated prompt tokens (the app's rough
/// 4-characters-per-token rule) plus its output cap. The
/// budget test pins this under [`SUITE_TOKEN_BUDGET`].
pub fn suite_worst_case_tokens(suite: &[EvalTask]) -> usize {
    suite
        .iter()
        .map(|t| estimate_tokens(&t.prompt) + t.max_tokens as usize)
        .sum()
}

// ---------------------------------------------------------------------------
// Scoring
// ---------------------------------------------------------------------------

/// Normalized form for [`Checker::ExactMatch`] — see the
/// variant's documentation for the exact rules.
fn normalize_exact(text: &str) -> String {
    let trimmed = text.trim();
    let unquoted = match trimmed.as_bytes() {
        [b'"', .., b'"'] if trimmed.len() >= 2 => &trimmed[1..trimmed.len() - 1],
        [b'\'', .., b'\''] if trimmed.len() >= 2 => &trimmed[1..trimmed.len() - 1],
        _ => trimmed,
    };
    let collapsed = unquoted.split_whitespace().collect::<Vec<_>>().join(" ");
    let lowered = collapsed.to_lowercase();
    // Drop spaces adjacent to JSON punctuation so the compact
    // and spaced spellings of one JSON value normalize alike.
    let chars: Vec<char> = lowered.chars().collect();
    let mut despaced = String::with_capacity(lowered.len());
    for (i, c) in chars.iter().enumerate() {
        if *c == ' ' {
            let prev = i.checked_sub(1).map(|j| chars[j]);
            let next = chars.get(i + 1).copied();
            let json_punct = |p: Option<char>| {
                matches!(
                    p,
                    Some('{') | Some('}') | Some('[') | Some(']') | Some(':') | Some(',')
                )
            };
            if json_punct(prev) || json_punct(next) {
                continue;
            }
        }
        despaced.push(*c);
    }
    despaced
        .trim_end_matches(['.', '!', '?', ';', ':', ','])
        .to_string()
}

/// The first JSON value in `text`, if any: scans for the first
/// `{` or `[` and stream-parses one value from there, so prose
/// and code fences around the value are tolerated.
fn first_json_value(text: &str) -> Option<serde_json::Value> {
    for (i, c) in text.char_indices() {
        if c == '{' || c == '[' {
            let mut stream =
                serde_json::Deserializer::from_str(&text[i..]).into_iter::<serde_json::Value>();
            if let Some(Ok(value)) = stream.next() {
                return Some(value);
            }
        }
    }
    None
}

/// The first numeric token in `text` (see [`Checker::Numeric`]
/// for the accepted forms), if any.
fn first_number(text: &str) -> Option<f64> {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let starts = chars[i].is_ascii_digit()
            || (chars[i] == '-'
                && i + 1 < chars.len()
                && (chars[i + 1].is_ascii_digit() || chars[i + 1] == '.'))
            || (chars[i] == '.' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit());
        if !starts {
            i += 1;
            continue;
        }
        let mut token = String::new();
        let mut seen_dot = false;
        let mut j = i;
        while j < chars.len() {
            let c = chars[j];
            if c.is_ascii_digit() || c == ',' {
                token.push(c);
                j += 1;
            } else if c == '.' && !seen_dot {
                seen_dot = true;
                token.push(c);
                j += 1;
            } else if c == '-' && j == i {
                token.push(c);
                j += 1;
            } else {
                break;
            }
        }
        let cleaned: String = token.chars().filter(|c| *c != ',').collect();
        if let Ok(n) = cleaned.parse::<f64>() {
            return Some(n);
        }
        i = j.max(i + 1);
    }
    None
}

/// Whether `output` satisfies `checker`. Pure; the single
/// scoring entry point the runner and the tests share.
pub fn passes(checker: &Checker, output: &str) -> bool {
    match checker {
        Checker::ExactMatch { expected } => normalize_exact(output) == normalize_exact(expected),
        Checker::Numeric {
            expected,
            tolerance,
        } => first_number(output)
            .map(|n| (n - expected).abs() <= *tolerance)
            .unwrap_or(false),
        Checker::JsonShape { required } => {
            let Some(value) = first_json_value(output) else {
                return false;
            };
            let Some(obj) = value.as_object() else {
                return false;
            };
            required
                .iter()
                .all(|f| obj.get(&f.key).map(|v| f.ty.matches(v)).unwrap_or(false))
        }
        Checker::LineCount { count, per_line } => {
            let lines: Vec<&str> = output.trim().lines().collect();
            lines.len() == *count
                && per_line
                    .as_ref()
                    .map(|p| lines.iter().enumerate().all(|(i, l)| p.matches(l, i)))
                    .unwrap_or(true)
        }
        Checker::KeywordCoverage {
            keywords,
            min_fraction,
        } => {
            if keywords.is_empty() {
                return false;
            }
            let haystack = output.to_lowercase();
            let hits = keywords
                .iter()
                .filter(|k| haystack.contains(&k.to_lowercase()))
                .count();
            (hits as f64 / keywords.len() as f64) >= *min_fraction - 1e-9
        }
        Checker::Regex { pattern } => regex_matches(pattern, output.trim()),
        Checker::ToolCallSchema {
            function,
            required_arguments,
        } => tool_call_matches(output, function, required_arguments),
    }
}

fn tool_call_matches(output: &str, function: &str, required_arguments: &[String]) -> bool {
    let Some(value) = first_json_value(output) else {
        return false;
    };
    // Unwrap a {"tool_calls": [ ... ]} envelope to its first call.
    let call = match value.get("tool_calls").and_then(|v| v.as_array()) {
        Some(calls) => match calls.first() {
            Some(first) => first.clone(),
            None => return false,
        },
        None => value,
    };
    let Some(obj) = call.as_object() else {
        return false;
    };
    // llama.cpp-style nested form: {"function": {"name": ..., "arguments": ...}}.
    if let Some(inner) = obj.get("function").and_then(|v| v.as_object()) {
        if inner.get("name").and_then(|v| v.as_str()) == Some(function) {
            return arguments_cover(inner.get("arguments"), required_arguments);
        }
    }
    let name = obj
        .get("name")
        .or_else(|| obj.get("function"))
        .or_else(|| obj.get("tool_name"))
        .and_then(|v| v.as_str());
    if name != Some(function) {
        return false;
    }
    let args = obj
        .get("arguments")
        .or_else(|| obj.get("parameters"))
        .or_else(|| obj.get("args"));
    arguments_cover(args, required_arguments)
}

fn arguments_cover(args: Option<&serde_json::Value>, required: &[String]) -> bool {
    let parsed;
    let args_obj = match args {
        Some(serde_json::Value::Object(map)) => Some(map),
        // Arguments emitted as a JSON string (a common llama.cpp form).
        Some(serde_json::Value::String(s)) => {
            parsed = serde_json::from_str::<serde_json::Value>(s).ok();
            parsed.as_ref().and_then(|v| v.as_object())
        }
        _ => None,
    };
    match args_obj {
        Some(map) => required.iter().all(|k| map.contains_key(k)),
        None => false,
    }
}

// ---------------------------------------------------------------------------
// The hand-rolled pattern matcher (Checker::Regex)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Atom {
    Literal(char),
    Any,
    Digit,
    Word,
    Space,
    Class {
        negated: bool,
        ranges: Vec<(char, char)>,
    },
}

impl Atom {
    fn matches(&self, c: char) -> bool {
        match self {
            Atom::Literal(l) => *l == c,
            Atom::Any => true,
            Atom::Digit => c.is_ascii_digit(),
            Atom::Word => c.is_alphanumeric() || c == '_',
            Atom::Space => c.is_whitespace(),
            Atom::Class { negated, ranges } => {
                let inside = ranges.iter().any(|(lo, hi)| *lo <= c && c <= *hi);
                inside != *negated
            }
        }
    }
}

#[derive(Debug, Clone)]
struct RegexAtom {
    atom: Atom,
    min: usize,
    max: Option<usize>, // None = unbounded
}

/// Parse the supported pattern subset (see [`Checker::Regex`]).
/// `None` for anything outside it — the check then fails closed.
fn parse_pattern(pattern: &str) -> Option<(bool, bool, Vec<RegexAtom>)> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    let anchor_start = chars.first() == Some(&'^');
    if anchor_start {
        i = 1;
    }
    let mut anchor_end = false;
    let mut atoms: Vec<RegexAtom> = Vec::new();
    while i < chars.len() {
        if chars[i] == '$' && i == chars.len() - 1 {
            anchor_end = true;
            break;
        }
        let atom = match chars[i] {
            '.' => {
                i += 1;
                Atom::Any
            }
            '[' => {
                i += 1;
                let negated = chars.get(i) == Some(&'^');
                if negated {
                    i += 1;
                }
                let mut ranges = Vec::new();
                while i < chars.len() && chars[i] != ']' {
                    let lo = chars[i];
                    if chars.get(i + 1) == Some(&'-')
                        && chars.get(i + 2).map(|c| *c != ']').unwrap_or(false)
                    {
                        ranges.push((lo, chars[i + 2]));
                        i += 3;
                    } else {
                        ranges.push((lo, lo));
                        i += 1;
                    }
                }
                if i >= chars.len() {
                    return None; // unterminated class
                }
                i += 1; // ']'
                Atom::Class { negated, ranges }
            }
            '\\' => {
                i += 1;
                let escaped = *chars.get(i)?;
                i += 1;
                match escaped {
                    'd' => Atom::Digit,
                    'w' => Atom::Word,
                    's' => Atom::Space,
                    other => Atom::Literal(other),
                }
            }
            c => {
                i += 1;
                Atom::Literal(c)
            }
        };
        // Quantifier on the atom just parsed.
        let (min, max) = match chars.get(i) {
            Some('*') => {
                i += 1;
                (0, None)
            }
            Some('+') => {
                i += 1;
                (1, None)
            }
            Some('?') => {
                i += 1;
                (0, Some(1))
            }
            Some('{') => {
                let mut j = i + 1;
                let mut spec = String::new();
                while j < chars.len() && chars[j] != '}' {
                    spec.push(chars[j]);
                    j += 1;
                }
                if j >= chars.len() {
                    return None;
                }
                i = j + 1;
                let (lo, hi) = match spec.split_once(',') {
                    Some((lo, "")) => (lo.parse::<usize>().ok()?, None),
                    Some((lo, hi)) => (lo.parse::<usize>().ok()?, Some(hi.parse::<usize>().ok()?)),
                    None => {
                        let n = spec.parse::<usize>().ok()?;
                        (n, Some(n))
                    }
                };
                (lo, hi)
            }
            _ => (1, Some(1)),
        };
        atoms.push(RegexAtom { atom, min, max });
    }
    Some((anchor_start, anchor_end, atoms))
}

fn match_here(atoms: &[RegexAtom], text: &[char], pos: usize, anchor_end: bool) -> bool {
    if atoms.is_empty() {
        return !anchor_end || pos == text.len();
    }
    let first = &atoms[0];
    // How many consecutive characters this atom can consume.
    let mut run = 0;
    while pos + run < text.len()
        && first.max.map(|m| run < m).unwrap_or(true)
        && first.atom.matches(text[pos + run])
    {
        run += 1;
    }
    // Try the longest consumption first, backtracking down to min.
    let mut take = run;
    loop {
        if take >= first.min && match_here(&atoms[1..], text, pos + take, anchor_end) {
            return true;
        }
        if take <= first.min {
            break;
        }
        take -= 1;
    }
    false
}

fn regex_matches(pattern: &str, text: &str) -> bool {
    let Some((anchor_start, anchor_end, atoms)) = parse_pattern(pattern) else {
        return false;
    };
    let chars: Vec<char> = text.chars().collect();
    if anchor_start {
        return match_here(&atoms, &chars, 0, anchor_end);
    }
    (0..=chars.len()).any(|start| match_here(&atoms, &chars, start, anchor_end))
}

// ---------------------------------------------------------------------------
// Run records, aggregates, comparison
// ---------------------------------------------------------------------------

/// One task's outcome in a run. `reason` is set when the task
/// did not produce a scorable output (transport failure,
/// time-box expiry, cancellation) or when the checker failed —
/// it is the operator-facing explanation beside the excerpt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalTaskResult {
    pub task_id: String,
    pub category: EvalCategory,
    pub passed: bool,
    pub output_excerpt: String,
    #[serde(default)]
    pub reason: Option<String>,
}

/// An output excerpt for a task result: the first 240
/// characters, with an ellipsis when truncated.
pub fn output_excerpt(output: &str) -> String {
    let trimmed = output.trim();
    if trimmed.chars().count() <= 240 {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(240).collect();
    format!("{head}…")
}

/// Score one task's output against the task's checker.
pub fn score_task(task: &EvalTask, output: &str) -> EvalTaskResult {
    let passed = passes(&task.checker, output);
    EvalTaskResult {
        task_id: task.id.clone(),
        category: task.category,
        passed,
        output_excerpt: output_excerpt(output),
        reason: if passed {
            None
        } else {
            Some("output did not satisfy the task checker".to_string())
        },
    }
}

/// A failed task result with no scorable output (the runner's
/// transport-failure and time-box paths).
pub fn failed_task(task: &EvalTask, reason: impl Into<String>) -> EvalTaskResult {
    EvalTaskResult {
        task_id: task.id.clone(),
        category: task.category,
        passed: false,
        output_excerpt: String::new(),
        reason: Some(reason.into()),
    }
}

/// Passed/total for one score line (overall or one category).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Score {
    pub passed: u32,
    pub total: u32,
}

impl Score {
    pub fn fraction(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.passed as f64 / self.total as f64
        }
    }
}

/// A run's aggregate: the overall score plus per-category
/// scores (keys are [`EvalCategory`] strings). Scores are
/// reported beside speed, never merged with it (§A.4).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EvalAggregate {
    pub overall: Score,
    pub per_category: BTreeMap<String, Score>,
}

/// Aggregate a set of task results.
pub fn aggregate_results(results: &[EvalTaskResult]) -> EvalAggregate {
    let mut aggregate = EvalAggregate::default();
    for result in results {
        aggregate.overall.total += 1;
        if result.passed {
            aggregate.overall.passed += 1;
        }
        let entry = aggregate
            .per_category
            .entry(result.category.as_str().to_string())
            .or_default();
        entry.total += 1;
        if result.passed {
            entry.passed += 1;
        }
    }
    aggregate
}

/// One full suite run against one backend — the record an
/// experiment (5e) carries and a baseline is built from.
/// `scores` is the aggregate of `results`, stored alongside so
/// payloads can render without re-deriving.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalRunRecord {
    pub backend_id: String,
    pub suite_version: u32,
    /// RFC 3339 timestamp of the run's start.
    pub run_at: String,
    pub results: Vec<EvalTaskResult>,
    pub scores: EvalAggregate,
}

impl EvalRunRecord {
    pub fn new(backend_id: String, run_at: String, results: Vec<EvalTaskResult>) -> Self {
        let scores = aggregate_results(&results);
        Self {
            backend_id,
            suite_version: EVAL_SUITE_VERSION,
            run_at,
            results,
            scores,
        }
    }
}

/// The stored baseline (`<data_dir>/optimize-eval.json`):
/// the incumbent's run, keyed by the backend's measurement
/// fingerprint (§A.1) at run time — a fingerprint change makes
/// the baseline stale, it is never silently re-keyed. `scores`
/// mirrors the run's aggregate for direct payload rendering;
/// `results` keeps the per-task evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BaselineRecord {
    pub backend_id: String,
    pub config_fingerprint: String,
    pub suite_version: u32,
    pub run_at: String,
    pub scores: EvalAggregate,
    #[serde(default)]
    pub results: Vec<EvalTaskResult>,
}

impl BaselineRecord {
    pub fn from_run(run: &EvalRunRecord, config_fingerprint: String) -> Self {
        Self {
            backend_id: run.backend_id.clone(),
            config_fingerprint,
            suite_version: run.suite_version,
            run_at: run.run_at.clone(),
            scores: run.scores.clone(),
            results: run.results.clone(),
        }
    }
}

/// Baseline-vs-candidate score movement for one line
/// (overall or one category).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ScoreDelta {
    pub baseline: Score,
    pub candidate: Score,
    /// Candidate passed minus baseline passed.
    pub delta_passed: i32,
    /// Candidate fraction minus baseline fraction.
    pub delta_fraction: f64,
}

fn score_delta(baseline: Score, candidate: Score) -> ScoreDelta {
    ScoreDelta {
        baseline,
        candidate,
        delta_passed: candidate.passed as i32 - baseline.passed as i32,
        delta_fraction: candidate.fraction() - baseline.fraction(),
    }
}

/// A candidate aggregate compared against the baseline: the
/// per-category and overall deltas, plus the §A.4
/// replacement-claim predicates as data — computed and
/// presented, never used to gate a plan (the operator
/// decides, as everywhere in this app).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalComparison {
    pub overall: ScoreDelta,
    pub per_category: BTreeMap<String, ScoreDelta>,
    /// Replacement-claim predicate (§A.4): the candidate's
    /// overall screen is at or above the baseline's.
    pub screen_at_or_above_baseline: bool,
    /// Replacement-claim predicate (§A.4): some category is at
    /// zero in the candidate that the baseline passes.
    pub any_category_zero_that_baseline_passes: bool,
    /// The categories behind `any_category_zero_that_baseline_passes`.
    pub zero_categories_that_baseline_passes: Vec<String>,
}

/// Compare a candidate's aggregate against the baseline's.
pub fn compare(candidate: &EvalAggregate, baseline: &EvalAggregate) -> EvalComparison {
    let mut per_category = BTreeMap::new();
    let mut keys: Vec<&String> = baseline
        .per_category
        .keys()
        .chain(candidate.per_category.keys())
        .collect();
    keys.sort();
    keys.dedup();
    for key in keys {
        let b = baseline.per_category.get(key).copied().unwrap_or_default();
        let c = candidate.per_category.get(key).copied().unwrap_or_default();
        per_category.insert(key.clone(), score_delta(b, c));
    }
    let zero_categories: Vec<String> = per_category
        .iter()
        .filter(|(_, d)| d.baseline.passed > 0 && d.candidate.passed == 0 && d.candidate.total > 0)
        .map(|(k, _)| k.clone())
        .collect();
    EvalComparison {
        overall: score_delta(baseline.overall, candidate.overall),
        per_category,
        screen_at_or_above_baseline: candidate.overall.fraction()
            >= baseline.overall.fraction() - 1e-9,
        any_category_zero_that_baseline_passes: !zero_categories.is_empty(),
        zero_categories_that_baseline_passes: zero_categories,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn suite_task(id: &str) -> EvalTask {
        eval_suite()
            .into_iter()
            .find(|t| t.id == id)
            .unwrap_or_else(|| panic!("no task {id}"))
    }

    // -- Suite shape and budgets -------------------------------------------

    #[test]
    fn suite_has_twelve_tasks_spanning_all_categories() {
        let suite = eval_suite();
        assert_eq!(suite.len(), 12);
        let mut ids: Vec<&str> = suite.iter().map(|t| t.id.as_str()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 12, "task ids are unique");
        for category in EvalCategory::all() {
            assert!(
                suite.iter().any(|t| t.category == category),
                "category {} is covered",
                category.as_str()
            );
        }
    }

    #[test]
    fn suite_respects_the_a4_budgets() {
        let suite = eval_suite();
        for t in &suite {
            assert!(
                t.max_tokens <= MAX_TASK_OUTPUT_TOKENS,
                "task {} cap {} exceeds the per-task budget",
                t.id,
                t.max_tokens
            );
        }
        let output_caps: u32 = suite.iter().map(|t| t.max_tokens).sum();
        assert!(
            (output_caps as usize) <= SUITE_TOKEN_BUDGET,
            "output caps alone total {output_caps}"
        );
        let worst = suite_worst_case_tokens(&suite);
        assert!(
            worst <= SUITE_TOKEN_BUDGET,
            "worst case (prompt estimates + output caps) is {worst}, over the {SUITE_TOKEN_BUDGET} budget"
        );
    }

    #[test]
    fn long_context_prompt_embeds_the_needle_midway() {
        let t = suite_task("long-context-needle");
        assert!(t.prompt.contains("QUARTZ-7319"));
        assert!(
            t.prompt.len() > 12_000,
            "prompt is {} chars",
            t.prompt.len()
        );
        let needle_at = t.prompt.find("QUARTZ-7319").unwrap();
        assert!(
            needle_at > t.prompt.len() / 4 && needle_at < (t.prompt.len() * 3) / 4,
            "needle sits in the middle stretch, at {needle_at}"
        );
        assert!(t.prompt.trim_end().ends_with("Reply with the code only."));
    }

    // -- ExactMatch ----------------------------------------------------------

    #[test]
    fn exact_match_normalization_pass_and_fail() {
        let checker = Checker::ExactMatch {
            expected: "Paris".to_string(),
        };
        assert!(passes(&checker, "Paris"));
        assert!(passes(&checker, " paris "));
        assert!(passes(&checker, "Paris."));
        assert!(passes(&checker, "\"PARIS\""));
        assert!(!passes(&checker, "Paris, France"));
        assert!(!passes(&checker, "Lyon"));
        assert!(!passes(&checker, ""));
    }

    #[test]
    fn fact_tasks_pass_and_fail() {
        let capital = suite_task("fact-capital");
        assert!(passes(&capital.checker, "Paris"));
        assert!(!passes(&capital.checker, "The capital is Paris"));
        let water = suite_task("fact-water-formula");
        assert!(passes(&water.checker, "H2O"));
        assert!(passes(&water.checker, "h2o"));
        assert!(!passes(&water.checker, "HO2"));
    }

    // -- Numeric -------------------------------------------------------------

    #[test]
    fn numeric_formatting_variants() {
        let checker = Checker::Numeric {
            expected: 1408.0,
            tolerance: 0.001,
        };
        assert!(passes(&checker, "1408"));
        assert!(passes(&checker, "1,408"));
        assert!(passes(&checker, "1408.0"));
        assert!(passes(&checker, "The answer is 1408."));
        assert!(!passes(&checker, "1409"));
        assert!(!passes(&checker, "no number here"));
        let task = suite_task("math-multiply");
        assert!(passes(&task.checker, "408"));
        assert!(!passes(&task.checker, "480"));
        let word = suite_task("math-word-problem");
        assert!(passes(&word.checker, "37"));
        assert!(passes(&word.checker, "37 bottles"));
        assert!(!passes(&word.checker, "42"));
    }

    // -- JsonShape -----------------------------------------------------------

    #[test]
    fn json_shape_pass_and_fail() {
        let task = suite_task("if-json-shape");
        assert!(passes(
            &task.checker,
            "{\"name\": \"Sea Whisper\", \"kind\": \"sailboat\", \"crew\": 4}"
        ));
        // Prose around the JSON is salvaged, as in the plan parser.
        assert!(passes(
            &task.checker,
            "Here is the object:\n```json\n{\"name\": \"Sea Whisper\", \"kind\": \"sailboat\", \"crew\": 4}\n```"
        ));
        // Missing key, wrong type, and non-JSON all fail.
        assert!(!passes(
            &task.checker,
            "{\"name\": \"Sea Whisper\", \"kind\": \"sailboat\"}"
        ));
        assert!(!passes(
            &task.checker,
            "{\"name\": \"Sea Whisper\", \"kind\": \"sailboat\", \"crew\": \"four\"}"
        ));
        assert!(!passes(&task.checker, "name: Sea Whisper, crew: 4"));
    }

    // -- LineCount -----------------------------------------------------------

    #[test]
    fn line_count_pass_and_fail() {
        let task = suite_task("if-line-count");
        assert!(passes(&task.checker, "north\nsouth\nwest"));
        assert!(passes(&task.checker, "north\nsouth\nwest\n"));
        assert!(!passes(&task.checker, "north\nsouth"));
        assert!(!passes(&task.checker, "north\nsouth\nwest\neast"));
        // Right count, but a line breaks the one-word pattern.
        assert!(!passes(&task.checker, "north\nsouth west\nwest"));
        // Right count and words, wrong membership on one line.
        assert!(!passes(&task.checker, "north\nsouth\neast"));
        // Right words, wrong order — the pattern is positional.
        assert!(!passes(&task.checker, "south\nnorth\nwest"));
    }

    // -- Regex ---------------------------------------------------------------

    #[test]
    fn regex_task_and_engine() {
        let task = suite_task("if-pattern");
        assert!(passes(&task.checker, "XYZ-7429"));
        assert!(!passes(&task.checker, "XYZ-742"));
        assert!(!passes(&task.checker, "xyz-7429"));
        assert!(!passes(&task.checker, "Code: XYZ-7429"));

        let date = Checker::Regex {
            pattern: "^\\d{2}/\\d{2}$".to_string(),
        };
        assert!(passes(&date, "10/17"));
        assert!(!passes(&date, "10/7"));

        let search = Checker::Regex {
            pattern: "[0-9]{4}".to_string(),
        };
        assert!(passes(&search, "the year 2026 ended"));
        assert!(!passes(&search, "the year 26 ended"));

        let plus = Checker::Regex {
            pattern: "^ab+c$".to_string(),
        };
        assert!(passes(&plus, "abbbc"));
        assert!(passes(&plus, "abc"));
        assert!(!passes(&plus, "ac"));

        // Outside the supported subset: fails closed.
        let unsupported = Checker::Regex {
            pattern: "^(a|b)$".to_string(),
        };
        assert!(!passes(&unsupported, "a"));
    }

    // -- Code-adjacent ---------------------------------------------------------

    #[test]
    fn code_adjacent_pass_and_fail() {
        let extract = suite_task("code-extract-json");
        assert!(passes(
            &extract.checker,
            "{\"ada\":36,\"grace\":85,\"lin\":29}"
        ));
        assert!(passes(
            &extract.checker,
            "{ \"ada\": 36, \"grace\": 85, \"lin\": 29 }"
        ));
        assert!(!passes(
            &extract.checker,
            "{\"ada\": 36, \"grace\": 85, \"lin\": 30}"
        ));
        let sort = suite_task("code-sort-words");
        assert!(passes(&sort.checker, "apple, banana, cherry"));
        assert!(!passes(&sort.checker, "banana, apple, cherry"));
        assert!(!passes(&sort.checker, "apple, banana"));
    }

    // -- KeywordCoverage -------------------------------------------------------

    #[test]
    fn keyword_coverage_pass_and_fail() {
        let task = suite_task("summary-garden");
        // 5 of 5 keywords.
        assert!(passes(
            &task.checker,
            "The garden gathers rainwater and makes compost for its raised beds, where pollinators work the flower borders before a shared harvest."
        ));
        // 4 of 5 = the 0.8 threshold.
        assert!(passes(
            &task.checker,
            "Rainwater collection and compost feed the raised beds, and the harvest is shared."
        ));
        // 2 of 5 fails.
        assert!(!passes(
            &task.checker,
            "The garden collects rainwater and shares its harvest."
        ));
        assert!(!passes(&task.checker, "A garden is a nice place."));
    }

    // -- ToolCallSchema --------------------------------------------------------

    #[test]
    fn tool_call_schema_pass_and_fail() {
        let task = suite_task("tool-call-weather");
        assert!(passes(
            &task.checker,
            "{\"name\": \"get_weather\", \"arguments\": {\"city\": \"Orrville\", \"units\": \"imperial\"}}"
        ));
        // Fenced, with prose — salvaged like other JSON outputs.
        assert!(passes(
            &task.checker,
            "```json\n{\"function\": \"get_weather\", \"parameters\": {\"city\": \"Orrville\", \"units\": \"imperial\"}}\n```"
        ));
        // Arguments as a JSON string.
        assert!(passes(
            &task.checker,
            "{\"name\": \"get_weather\", \"arguments\": \"{\\\"city\\\": \\\"Orrville\\\", \\\"units\\\": \\\"imperial\\\"}\"}"
        ));
        // Missing a required argument, wrong function, no JSON.
        assert!(!passes(
            &task.checker,
            "{\"name\": \"get_weather\", \"arguments\": {\"city\": \"Orrville\"}}"
        ));
        assert!(!passes(
            &task.checker,
            "{\"name\": \"set_weather\", \"arguments\": {\"city\": \"Orrville\", \"units\": \"imperial\"}}"
        ));
        assert!(!passes(
            &task.checker,
            "I would call get_weather for Orrville in imperial units."
        ));
    }

    // -- Long context ------------------------------------------------------------

    #[test]
    fn long_context_task_pass_and_fail() {
        let task = suite_task("long-context-needle");
        assert!(passes(&task.checker, "QUARTZ-7319"));
        assert!(passes(&task.checker, "quartz-7319"));
        assert!(!passes(&task.checker, "QUARTZ-7318"));
        assert!(!passes(&task.checker, "The code is in the records."));
    }

    // -- Aggregation, records, comparison ---------------------------------------

    fn results(spec: &[(&str, EvalCategory, bool)]) -> Vec<EvalTaskResult> {
        spec.iter()
            .map(|(id, category, passed)| EvalTaskResult {
                task_id: id.to_string(),
                category: *category,
                passed: *passed,
                output_excerpt: String::new(),
                reason: None,
            })
            .collect()
    }

    #[test]
    fn aggregate_counts_overall_and_per_category() {
        let agg = aggregate_results(&results(&[
            ("a", EvalCategory::FactualRecall, true),
            ("b", EvalCategory::FactualRecall, false),
            ("c", EvalCategory::Summarization, true),
        ]));
        assert_eq!(
            agg.overall,
            Score {
                passed: 2,
                total: 3
            }
        );
        assert_eq!(
            agg.per_category["factual_recall"],
            Score {
                passed: 1,
                total: 2
            }
        );
        assert_eq!(
            agg.per_category["summarization"],
            Score {
                passed: 1,
                total: 1
            }
        );
        assert_eq!(agg.overall.fraction(), 2.0 / 3.0);
    }

    #[test]
    fn run_and_baseline_records_round_trip() {
        let suite = eval_suite();
        let scored: Vec<EvalTaskResult> = suite
            .iter()
            .map(|t| EvalTaskResult {
                task_id: t.id.clone(),
                category: t.category,
                passed: true,
                output_excerpt: "ok".to_string(),
                reason: None,
            })
            .collect();
        let run = EvalRunRecord::new(
            "planner".to_string(),
            "2026-10-17T12:00:00Z".to_string(),
            scored,
        );
        assert_eq!(
            run.scores.overall,
            Score {
                passed: 12,
                total: 12
            }
        );
        assert_eq!(run.suite_version, EVAL_SUITE_VERSION);
        let baseline = BaselineRecord::from_run(&run, "fp-123".to_string());
        assert_eq!(baseline.config_fingerprint, "fp-123");
        assert_eq!(baseline.backend_id, "planner");
        let text = serde_json::to_string(&baseline).unwrap();
        let back: BaselineRecord = serde_json::from_str(&text).unwrap();
        assert_eq!(back, baseline);
        // Scores serialize beside (never merged with) any speed
        // figure: the record carries passed/total lines only.
        assert!(text.contains("\"overall\":{\"passed\":12,\"total\":12}"));
    }

    #[test]
    fn comparison_predicates_and_deltas() {
        let baseline = aggregate_results(&results(&[
            ("a", EvalCategory::FactualRecall, true),
            ("b", EvalCategory::FactualRecall, false),
            ("c", EvalCategory::Summarization, true),
            ("d", EvalCategory::Summarization, false),
        ]));
        // Candidate: same overall fraction (2/4), but factual
        // recall collapsed to zero — the zero-category
        // predicate fires.
        let candidate = aggregate_results(&results(&[
            ("a", EvalCategory::FactualRecall, false),
            ("b", EvalCategory::FactualRecall, false),
            ("c", EvalCategory::Summarization, true),
            ("d", EvalCategory::Summarization, true),
        ]));
        let cmp = compare(&candidate, &baseline);
        assert!(cmp.screen_at_or_above_baseline);
        assert!(cmp.any_category_zero_that_baseline_passes);
        assert_eq!(
            cmp.zero_categories_that_baseline_passes,
            vec!["factual_recall".to_string()]
        );
        assert_eq!(cmp.overall.delta_passed, 0);
        assert_eq!(cmp.per_category["factual_recall"].delta_passed, -1);

        // A strictly better candidate: above baseline, no zeroed
        // category the baseline passes.
        let better = aggregate_results(&results(&[
            ("a", EvalCategory::FactualRecall, true),
            ("b", EvalCategory::FactualRecall, true),
            ("c", EvalCategory::Summarization, true),
            ("d", EvalCategory::Summarization, true),
        ]));
        let cmp2 = compare(&better, &baseline);
        assert!(cmp2.screen_at_or_above_baseline);
        assert!(!cmp2.any_category_zero_that_baseline_passes);
        assert_eq!(cmp2.overall.delta_passed, 2);
        assert!(cmp2.overall.delta_fraction > 0.0);

        // A worse candidate fails the at-or-above predicate.
        let worse = aggregate_results(&results(&[
            ("a", EvalCategory::FactualRecall, true),
            ("b", EvalCategory::FactualRecall, false),
            ("c", EvalCategory::Summarization, false),
            ("d", EvalCategory::Summarization, false),
        ]));
        assert!(!compare(&worse, &baseline).screen_at_or_above_baseline);
    }

    #[test]
    fn score_task_records_excerpt_and_reason() {
        let task = suite_task("fact-capital");
        let ok = score_task(&task, "Paris");
        assert!(ok.passed);
        assert_eq!(ok.output_excerpt, "Paris");
        assert!(ok.reason.is_none());
        let bad = score_task(&task, "Lyon");
        assert!(!bad.passed);
        assert!(bad.reason.is_some());
        let failed = failed_task(&task, "time-box expired before this task ran");
        assert!(!failed.passed);
        assert_eq!(
            failed.reason.as_deref(),
            Some("time-box expired before this task ran")
        );
    }
}
