//! llama.cpp tensor-buffer overrides, modeled closely enough to
//! price expert offload from a backend's raw flags (P-5, and the
//! `--n-cpu-moe` semantics check, code review v0.1.60 item 11).
//!
//! # What llama.cpp does
//!
//! Verified against ggml-org/llama.cpp `master` at commit
//! `0a66ac7189a6f25f3c4760bf1bec620ed537f4bb`, fetched 2026-11-12:
//!
//! - `common/arg.cpp` (`-ot` / `--override-tensor`):
//!   `parse_tensor_buffer_overrides` splits the value on EVERY `,`
//!   (`string_split<std::string>(value, ',')`), splits each part at
//!   its FIRST `=` into `<pattern>=<buffer type>`, and appends one
//!   override per part to `params.tensor_buft_overrides`, in order.
//!   Repeated `-ot` flags append further overrides.
//! - `common/arg.cpp` (`-cmoe` / `--cpu-moe`): appends
//!   `llm_ffn_exps_cpu_override()` — the pattern
//!   [`LLM_FFN_EXPS_REGEX`] on the CPU buffer type.
//! - `common/arg.cpp` (`-ncmoe` / `--n-cpu-moe N`): calls
//!   `llm_add_n_cpu_ffn_overrides(N, LLM_FFN_EXPS_REGEX, …)`, which
//!   (`common/common.h`) appends, for `i in 0..N`, the pattern
//!   `blk\.<i>` + [`LLM_FFN_EXPS_REGEX`] on the CPU buffer type.
//!   So `N` is a BLOCK INDEX bound — experts of blocks with index
//!   `< N` go to the CPU, and leading dense blocks count toward
//!   `N` — and a repeated `--n-cpu-moe` APPENDS more overrides
//!   (all CPU), so the effective bound is the maximum, not the
//!   last value.
//! - `src/llama-model-loader.cpp`: for each tensor, the overrides
//!   are tried IN ORDER with `std::regex_search(tensor_name,
//!   std::regex(pattern))` (unanchored ECMAScript search) and the
//!   FIRST match decides the buffer type (`break` after the first
//!   hit). All three flag families share the one ordered list, so
//!   precedence follows command-line order across them.
//!
//! Sources:
//! <https://raw.githubusercontent.com/ggml-org/llama.cpp/master/common/arg.cpp>,
//! <https://raw.githubusercontent.com/ggml-org/llama.cpp/master/common/common.h>,
//! <https://raw.githubusercontent.com/ggml-org/llama.cpp/master/src/llama-model-loader.cpp>.
//!
//! # Where this model departs from llama.cpp, deliberately
//!
//! - Surrounding shell quotes (`'…'` / `"…"`) are stripped from
//!   values: operators paste recipes written for a shell. (The app
//!   launches llama-server with whitespace-split argv and no shell,
//!   so llama.cpp itself would see the quotes.)
//! - Overrides are split on `,` only at the top level of the
//!   pattern — never inside `{m,n}` quantifiers, character classes
//!   or groups — pricing what the operator meant. llama.cpp's
//!   naive split rejects such a value at startup ("invalid value"),
//!   so no running server disagrees with this reading.
//! - Patterns compile with Rust's linear-time `regex` crate under
//!   size limits (they are operator input). ECMAScript features it
//!   lacks (backreferences, lookaround) fail to compile here; the
//!   caller then treats the offload as unresolvable (all weights
//!   on the GPU, [`super::MemoryBasis::Estimate`]).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use regex::{Regex, RegexBuilder};

use crate::gguf::ExpertTensor;

/// llama.cpp's routed-expert tensor pattern (`common/common.h`,
/// `LLM_FFN_EXPS_REGEX`): what `--cpu-moe` and `--n-cpu-moe` move.
pub const LLM_FFN_EXPS_REGEX: &str = r"\.ffn_(up|down|gate|gate_up)_(ch|)exps";

/// `--n-cpu-moe`'s per-block patterns (`blk\.<i>` +
/// [`LLM_FFN_EXPS_REGEX`]) folded into one regex that captures the
/// block index, so a bound of N is one search, not N patterns.
const BLOCK_EXPS_REGEX: &str = r"blk\.([0-9]+)\.ffn_(up|down|gate|gate_up)_(ch|)exps";

/// Longest operator pattern compiled; longer ones are treated as
/// unsupported (a real override is a few dozen characters).
const MAX_PATTERN_LEN: usize = 4096;
/// Compiled-program and lazy-DFA cache limits for operator
/// patterns: generous for any real override, small enough that a
/// pathological pattern fails fast instead of allocating.
const REGEX_SIZE_LIMIT: usize = 1 << 20;
/// Compiled patterns kept in the process-wide cache before it is
/// cleared (expert offload is resolved on every backend listing).
const REGEX_CACHE_CAP: usize = 256;

/// One tensor-buffer override, in the order llama.cpp would apply
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverrideRule {
    /// `-ot <pattern>=<buffer>`: `cpu` when the buffer type is the
    /// CPU (case-insensitive `CPU`); any other buffer is a device.
    Pattern { pattern: String, cpu: bool },
    /// `--cpu-moe` / `-cmoe`: [`LLM_FFN_EXPS_REGEX`] on the CPU.
    CpuMoe,
    /// `--n-cpu-moe N` / `-ncmoe N` (N > 0): experts of blocks with
    /// index `< N` on the CPU.
    CpuMoeBlocks(u32),
}

impl OverrideRule {
    fn is_cpu(&self) -> bool {
        match self {
            OverrideRule::Pattern { cpu, .. } => *cpu,
            OverrideRule::CpuMoe | OverrideRule::CpuMoeBlocks(_) => true,
        }
    }
}

/// Strip one layer of matching surrounding shell quotes.
pub fn strip_quotes(s: &str) -> &str {
    for q in ['"', '\''] {
        if s.len() >= 2 && s.starts_with(q) && s.ends_with(q) {
            return &s[1..s.len() - 1];
        }
    }
    s
}

/// Split an `-ot` value into its overrides on top-level commas
/// only: a comma inside a `{m,n}` quantifier, a `[...]` character
/// class or a `(...)` group belongs to the pattern. Backslash
/// escapes are honoured (`\,` `\[` never open or split anything).
pub fn split_overrides(value: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut brace, mut paren) = (0u32, 0u32);
    let mut in_class = false;
    let mut escaped = false;
    let mut start = 0;
    for (i, c) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            ']' if in_class => in_class = false,
            _ if in_class => {}
            '[' => in_class = true,
            '{' => brace += 1,
            '}' => brace = brace.saturating_sub(1),
            '(' => paren += 1,
            ')' => paren = paren.saturating_sub(1),
            ',' if brace == 0 && paren == 0 => {
                parts.push(&value[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&value[start..]);
    parts
}

/// Append the overrides of one `-ot` value.
fn push_ot_value(value: &str, rules: &mut Vec<OverrideRule>) {
    for part in split_overrides(strip_quotes(value)) {
        let part = strip_quotes(part);
        // llama.cpp splits at the FIRST '=' (and rejects a part
        // with none — such a server never starts; nothing to
        // price).
        if let Some((pattern, buffer)) = part.split_once('=') {
            rules.push(OverrideRule::Pattern {
                pattern: pattern.to_string(),
                cpu: strip_quotes(buffer).eq_ignore_ascii_case("CPU"),
            });
        }
    }
}

fn parse_count(v: &str) -> Option<u32> {
    strip_quotes(v).parse::<u32>().ok()
}

/// Every tensor-buffer override in raw llama-server flags, in
/// command-line order: `-ot` / `--override-tensor` (space or `=`
/// spelling, repeatable, comma lists), `--cpu-moe` / `-cmoe`, and
/// `--n-cpu-moe N` / `-ncmoe N` (space or `=` spelling; `0` adds
/// nothing, as in llama.cpp).
pub fn parse_override_rules(raw: &str) -> Vec<OverrideRule> {
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    let mut rules = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i];
        if t == "-ot" || t == "--override-tensor" {
            if let Some(v) = tokens.get(i + 1) {
                push_ot_value(v, &mut rules);
            }
            i += 2;
            continue;
        }
        if let Some(v) = t
            .strip_prefix("--override-tensor=")
            .or_else(|| t.strip_prefix("-ot="))
        {
            push_ot_value(v, &mut rules);
        } else if t == "--cpu-moe" || t == "-cmoe" {
            rules.push(OverrideRule::CpuMoe);
        } else if t == "--n-cpu-moe" || t == "-ncmoe" {
            if let Some(n) = tokens.get(i + 1).and_then(|v| parse_count(v)) {
                if n > 0 {
                    rules.push(OverrideRule::CpuMoeBlocks(n));
                }
                i += 2;
                continue;
            }
        } else if let Some(v) = t
            .strip_prefix("--n-cpu-moe=")
            .or_else(|| t.strip_prefix("-ncmoe="))
        {
            if let Some(n) = parse_count(v) {
                if n > 0 {
                    rules.push(OverrideRule::CpuMoeBlocks(n));
                }
            }
        }
        i += 1;
    }
    rules
}

/// Compile an operator pattern through a small process-wide cache
/// (keyed by the pattern text; a compiled `Regex` is a cheap
/// clone). `None` when the pattern is too long, uses syntax the
/// linear-time engine does not support, or exceeds the size limits.
pub fn compile_pattern(pattern: &str) -> Option<Regex> {
    if pattern.len() > MAX_PATTERN_LEN {
        return None;
    }
    static CACHE: OnceLock<Mutex<HashMap<String, Option<Regex>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(guard) = cache.lock() {
        if let Some(hit) = guard.get(pattern) {
            return hit.clone();
        }
    }
    let compiled = RegexBuilder::new(pattern)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_SIZE_LIMIT)
        .build()
        .ok();
    if let Ok(mut guard) = cache.lock() {
        if guard.len() >= REGEX_CACHE_CAP {
            guard.clear();
        }
        guard.insert(pattern.to_string(), compiled.clone());
    }
    compiled
}

fn exps_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(LLM_FFN_EXPS_REGEX).expect("static pattern"))
}

fn block_exps_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(BLOCK_EXPS_REGEX).expect("static pattern"))
}

/// The smallest block index `i` for which llama.cpp's
/// `blk\.<i>` + [`LLM_FFN_EXPS_REGEX`] pattern matches `name`
/// (`%d` formatting: no leading zeros), if any.
pub fn exps_block_index(name: &str) -> Option<u64> {
    block_exps_regex()
        .captures_iter(name)
        .filter_map(|c| {
            let digits = c.get(1)?.as_str();
            if digits.len() > 1 && digits.starts_with('0') {
                return None;
            }
            digits.parse::<u64>().ok()
        })
        .min()
}

enum Compiled {
    Pattern { re: Regex, cpu: bool },
    CpuMoe,
    CpuMoeBlocks(u32),
}

impl Compiled {
    fn matches(&self, name: &str) -> bool {
        match self {
            Compiled::Pattern { re, .. } => re.is_match(name),
            Compiled::CpuMoe => exps_regex().is_match(name),
            Compiled::CpuMoeBlocks(n) => exps_block_index(name).is_some_and(|i| i < *n as u64),
        }
    }

    fn cpu(&self) -> bool {
        match self {
            Compiled::Pattern { cpu, .. } => *cpu,
            Compiled::CpuMoe | Compiled::CpuMoeBlocks(_) => true,
        }
    }
}

/// An ordered override list ready to place tensors.
pub struct CompiledOverrides {
    rules: Vec<Compiled>,
    /// True when at least one `-ot` pattern compiled and is in the
    /// list (as opposed to only `--cpu-moe` / `--n-cpu-moe`).
    has_patterns: bool,
}

/// Why an override list cannot be resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unresolvable {
    /// An `-ot` pattern did not compile (unsupported syntax such as
    /// a backreference or lookaround, or over the size limits).
    Pattern(String),
}

impl CompiledOverrides {
    /// Compile `rules`. An uncompilable pattern makes the list
    /// unresolvable when the list moves anything to the CPU at all
    /// (with no CPU override, nothing lands in system RAM whatever
    /// the failing pattern matches).
    pub fn new(rules: &[OverrideRule]) -> Result<Self, Unresolvable> {
        let any_cpu = rules.iter().any(OverrideRule::is_cpu);
        let mut out = Vec::with_capacity(rules.len());
        let mut has_patterns = false;
        for rule in rules {
            match rule {
                OverrideRule::Pattern { pattern, cpu } => match compile_pattern(pattern) {
                    Some(re) => {
                        has_patterns = true;
                        out.push(Compiled::Pattern { re, cpu: *cpu });
                    }
                    None if any_cpu => return Err(Unresolvable::Pattern(pattern.clone())),
                    None => {}
                },
                OverrideRule::CpuMoe => out.push(Compiled::CpuMoe),
                OverrideRule::CpuMoeBlocks(n) => out.push(Compiled::CpuMoeBlocks(*n)),
            }
        }
        Ok(Self {
            rules: out,
            has_patterns,
        })
    }

    /// Whether `name` lands on the CPU: the FIRST override whose
    /// pattern matches decides (llama.cpp's loader breaks on the
    /// first hit); no match leaves the tensor on its default
    /// (device) buffer.
    pub fn on_cpu(&self, name: &str) -> bool {
        self.rules
            .iter()
            .find(|r| r.matches(name))
            .is_some_and(Compiled::cpu)
    }

    /// Index of the first override matching `name`, if any.
    fn first_match(&self, name: &str) -> Option<usize> {
        self.rules.iter().position(|r| r.matches(name))
    }

    /// The largest `--n-cpu-moe` bound in the list (repeats
    /// accumulate: every bound adds CPU overrides).
    pub fn max_block_bound(&self) -> u32 {
        self.rules
            .iter()
            .filter_map(|r| match r {
                Compiled::CpuMoeBlocks(n) => Some(*n),
                _ => None,
            })
            .max()
            .unwrap_or(0)
    }

    /// Bytes of `tensors` the list places on the CPU.
    pub fn cpu_bytes(&self, tensors: &[ExpertTensor]) -> u64 {
        tensors
            .iter()
            .filter(|t| self.on_cpu(&t.name))
            .fold(0u64, |acc, t| acc.saturating_add(t.bytes))
    }
}

/// Representative routed-expert tensor names for classifying an
/// override list when the model's own names are not at hand:
/// blocks 0..128 in each of the three common stacked-expert
/// spellings.
fn probe_names() -> &'static [String] {
    static PROBES: OnceLock<Vec<String>> = OnceLock::new();
    PROBES.get_or_init(|| {
        (0..128)
            .flat_map(|b| {
                ["ffn_up_exps", "ffn_down_exps", "ffn_gate_exps"]
                    .into_iter()
                    .map(move |k| format!("blk.{b}.{k}.weight"))
            })
            .collect()
    })
}

/// Coarse classification of an override list without the model's
/// tensor names (see [`super::expert_offload`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classified {
    /// Nothing moves to the CPU.
    None,
    /// Every routed expert moves to the CPU.
    AllExperts,
    /// Only `--n-cpu-moe` bounds move experts (no `-ot` pattern
    /// touches them): the effective (maximum) bound.
    BlockBound(u32),
    /// `-ot` patterns place some but not all experts, or a pattern
    /// could not be compiled: resolvable only against the model's
    /// tensor names.
    Partial,
}

/// Classify raw flags' override list against [`probe_names`].
pub fn classify(raw: &str) -> Classified {
    let rules = parse_override_rules(raw);
    if !rules.iter().any(OverrideRule::is_cpu) {
        return Classified::None;
    }
    let compiled = match CompiledOverrides::new(&rules) {
        Ok(c) => c,
        Err(_) => return Classified::Partial,
    };
    let probes = probe_names();
    let mut cpu = 0usize;
    let mut pattern_decides = false;
    for name in probes {
        if let Some(idx) = compiled.first_match(name) {
            if matches!(compiled.rules[idx], Compiled::Pattern { .. }) {
                pattern_decides = true;
            }
            if compiled.rules[idx].cpu() {
                cpu += 1;
            }
        }
    }
    if cpu == probes.len() {
        Classified::AllExperts
    } else if cpu == 0 {
        Classified::None
    } else if !compiled.has_patterns || !pattern_decides {
        Classified::BlockBound(compiled.max_block_bound())
    } else {
        Classified::Partial
    }
}

/// System-RAM expert bytes under raw flags' overrides, resolved
/// against the model's real expert tensors with llama.cpp's
/// first-match precedence. `Err` when a pattern cannot be compiled
/// (the caller degrades to all-on-GPU at `Estimate`).
pub fn resolve_cpu_bytes(raw: &str, tensors: &[ExpertTensor]) -> Result<u64, Unresolvable> {
    let rules = parse_override_rules(raw);
    if !rules.iter().any(OverrideRule::is_cpu) {
        return Ok(0);
    }
    Ok(CompiledOverrides::new(&rules)?.cpu_bytes(tensors))
}

/// System-RAM expert bytes under `--n-cpu-moe n` alone: the
/// tensors whose llama.cpp block index is `< n`.
pub fn block_bound_bytes(tensors: &[ExpertTensor], n: u32) -> u64 {
    tensors
        .iter()
        .filter(|t| exps_block_index(&t.name).is_some_and(|i| i < n as u64))
        .fold(0u64, |acc, t| acc.saturating_add(t.bytes))
}

/// System-RAM expert bytes for every `--n-cpu-moe k`, `k` in
/// `0..=max_bound`: entry `k` sums the tensors whose llama.cpp
/// block index is `< k`. Prefix sums over the tensors sorted by
/// block index — O(n log n) however many positions are asked for
/// (header-derived input; P-10).
pub fn block_bound_prefix_bytes(tensors: &[ExpertTensor], max_bound: u32) -> Vec<u64> {
    let mut by_index: Vec<(u64, u64)> = tensors
        .iter()
        .filter_map(|t| exps_block_index(&t.name).map(|i| (i, t.bytes)))
        .collect();
    by_index.sort_unstable_by_key(|&(i, _)| i);
    let mut out = Vec::with_capacity(max_bound as usize + 1);
    let mut acc = 0u64;
    let mut it = by_index.iter().peekable();
    for k in 0..=max_bound as u64 {
        while let Some(&&(i, b)) = it.peek() {
            if i < k {
                acc = acc.saturating_add(b);
                it.next();
            } else {
                break;
            }
        }
        out.push(acc);
    }
    out
}
