//! The recipe document: a named, ordered list of steps with
//! recipe-level requirements, environment and parameters.
//!
//! Stored as JSON (matching `config.json`) under
//! `<data_dir>/diagnostics/recipes/<name>.json`. A step may carry
//! variants — one step template plus a list of flag overrides — so an
//! A/B test is one step with N variants, not N copy-pasted steps.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::argv;

/// The current recipe document version.
pub const RECIPE_VERSION: u32 = 1;

/// Default per-step timeout (seconds) when a step names none.
pub const DEFAULT_STEP_TIMEOUT_SECS: u64 = 600;

/// Hard ceiling on a step timeout (6 h): a typo'd timeout must not
/// hold the GPUs for days.
pub const MAX_STEP_TIMEOUT_SECS: u64 = 6 * 3600;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recipe {
    /// Identifier and file stem: `[a-z0-9][a-z0-9_-]*`, at most 64.
    pub name: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_version")]
    pub version: u32,
    /// User-defined template parameters (`{m}`, `{backend}`).
    #[serde(default)]
    pub params: Vec<Param>,
    /// Environment applied to every step (step and variant env win).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub requires: Requires,
    pub steps: Vec<Step>,
    /// Set on listings for shipped recipes; never stored.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub builtin: bool,
    /// Where the recipe came from (`paste`, `builtin`, `api`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

fn default_version() -> u32 {
    RECIPE_VERSION
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Param {
    pub name: String,
    /// Default value; may reference other templates
    /// (`{models_dir}/x.gguf`). A parameter without a default must be
    /// supplied at run time.
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub description: String,
}

/// What a run needs from the machine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Requires {
    /// GPUs the run needs to itself, by index. `None`: inferred from
    /// the resolved command lines (`-ngl`, `-ts`, `-mg`, `-dev`,
    /// `CUDA_VISIBLE_DEVICES`). When declared, the declared set is
    /// used and inference is reported alongside it.
    #[serde(default)]
    pub gpus: Option<Vec<u32>>,
    /// Backends to stop before the run regardless of placement
    /// (templates allowed: `{backend}`).
    #[serde(default)]
    pub stop_backends: Vec<String>,
    /// Backends that must stay up (a step talks to them).
    #[serde(default)]
    pub keep_running: Vec<String>,
    /// Restart every backend the run stopped once it ends — success,
    /// crash, timeout or cancel alike.
    #[serde(default = "yes")]
    pub restore: bool,
}

fn yes() -> bool {
    true
}

impl Default for Requires {
    fn default() -> Self {
        Self {
            gpus: None,
            stop_backends: Vec::new(),
            keep_running: Vec::new(),
            restore: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    /// Run a process (allowlisted executable, argv array).
    #[default]
    Exec,
    /// One HTTP request to a local server (a backend, or a
    /// background step's server), e.g. a long synthetic prompt.
    Http,
    /// Read GGUF header metadata in-process (no process spawned).
    Gguf,
}

/// How a step's outcome is judged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Expect {
    /// Exit 0 is success; anything else fails the step.
    #[default]
    Success,
    /// A crash (NTSTATUS / signal exit) is the expected evidence,
    /// recorded as data rather than a failure.
    MayCrash,
    /// Any exit is data.
    MayFail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParserKind {
    LlamaBench,
    ServerTimings,
    CudaError,
    Raw,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HttpSpec {
    /// Template URL; must resolve to a loopback host.
    pub url: String,
    #[serde(default = "post")]
    pub method: String,
    /// JSON body. String leaves are templates; the leaf
    /// `"{synthetic_prompt}"` is replaced by a generated prompt of
    /// about `synthetic_prompt_tokens` tokens.
    #[serde(default)]
    pub body: Option<serde_json::Value>,
    /// Template for the synthetic prompt's size in tokens
    /// (`"{prompt_tokens}"`, `"8192"`).
    #[serde(default)]
    pub synthetic_prompt_tokens: Option<String>,
}

fn post() -> String {
    "POST".to_string()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GgufSpec {
    /// Template path of the GGUF file.
    pub path: String,
    /// Key filters (substring match); empty = every scalar key.
    #[serde(default)]
    pub keys: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub kind: StepKind,
    /// Executable template (`{llamacpp_bin}/llama-bench.exe`,
    /// `nvidia-smi`). Resolved against the managed llama.cpp install
    /// first, then PATH; it must be on the allowlist.
    #[serde(default)]
    pub exe: String,
    /// Argument templates. A token that is exactly a list template
    /// (`{backend.x.raw_flags}`) splices into several arguments.
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub workdir: Option<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub parser: Option<ParserKind>,
    #[serde(default)]
    pub expect: Expect,
    #[serde(default)]
    pub variants: Vec<Variant>,
    /// Keep running while later steps run (a scratch llama-server);
    /// stopped when the recipe ends. Waits for `ready_url` first.
    #[serde(default)]
    pub background: bool,
    #[serde(default)]
    pub ready_url: Option<String>,
    #[serde(default)]
    pub ready_timeout_secs: Option<u64>,
    #[serde(default)]
    pub http: Option<HttpSpec>,
    #[serde(default)]
    pub gguf: Option<GgufSpec>,
    /// llama-bench steps: append `-o json` when no `-o` is given, so
    /// results parse structurally (partial output survives a crash).
    #[serde(default = "yes")]
    pub bench_json: bool,
}

impl Step {
    /// A plain exec step.
    pub fn exec(name: &str, exe: &str, args: &[&str]) -> Self {
        Step {
            name: name.to_string(),
            description: String::new(),
            kind: StepKind::Exec,
            exe: exe.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            env: BTreeMap::new(),
            workdir: None,
            timeout_secs: None,
            parser: None,
            expect: Expect::Success,
            variants: Vec::new(),
            background: false,
            ready_url: None,
            ready_timeout_secs: None,
            http: None,
            gguf: None,
            bench_json: true,
        }
    }
}

/// One variant of a step: flag overrides on the step template.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Variant {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Flag → value (an empty value is a bare flag). Replaces the
    /// flag's first occurrence, or appends it.
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Flags removed from the template.
    #[serde(default)]
    pub unset: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub expect: Option<Expect>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecipeError {
    Invalid(String),
}

impl std::fmt::Display for RecipeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecipeError::Invalid(m) => write!(f, "invalid recipe: {m}"),
        }
    }
}

impl std::error::Error for RecipeError {}

/// Is `name` a valid recipe/run identifier?
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Turn free text into a valid recipe name.
pub fn slugify(text: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
        if out.len() >= 60 {
            break;
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() {
        "recipe".to_string()
    } else {
        out
    }
}

fn valid_env_key(k: &str) -> bool {
    !k.is_empty()
        && k.len() <= 128
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !k.chars().next().is_some_and(|c| c.is_ascii_digit())
}

impl Recipe {
    /// Structural validation (templates are checked at plan time,
    /// when the context exists).
    pub fn validate(&self) -> Result<(), RecipeError> {
        let bad = |m: String| Err(RecipeError::Invalid(m));
        if !valid_name(&self.name) {
            return bad(format!(
                "name {:?} must be 1-64 chars of [a-z0-9_-], starting with a letter or digit",
                self.name
            ));
        }
        if self.version != RECIPE_VERSION {
            return bad(format!("unsupported recipe version {}", self.version));
        }
        if self.steps.is_empty() {
            return bad("a recipe needs at least one step".into());
        }
        if self.steps.len() > 64 {
            return bad("a recipe has at most 64 steps".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        for p in &self.params {
            if p.name.is_empty()
                || p.name.contains('.')
                || !p
                    .name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                return bad(format!("parameter name {:?} must be [A-Za-z0-9_]", p.name));
            }
            if !seen.insert(p.name.clone()) {
                return bad(format!("duplicate parameter {:?}", p.name));
            }
        }
        for k in self.env.keys() {
            if !valid_env_key(k) {
                return bad(format!("environment key {k:?} is not a valid name"));
            }
        }
        for (i, s) in self.steps.iter().enumerate() {
            let at = format!("step {} ({:?})", i + 1, s.name);
            if s.name.trim().is_empty() {
                return bad(format!("step {} needs a name", i + 1));
            }
            match s.kind {
                StepKind::Exec => {
                    if s.exe.trim().is_empty() {
                        return bad(format!("{at}: exec step needs an executable"));
                    }
                    if s.http.is_some() || s.gguf.is_some() {
                        return bad(format!("{at}: exec step carries http/gguf settings"));
                    }
                }
                StepKind::Http => {
                    if s.http.is_none() {
                        return bad(format!("{at}: http step needs `http`"));
                    }
                    if s.background {
                        return bad(format!("{at}: only exec steps run in the background"));
                    }
                }
                StepKind::Gguf => {
                    if s.gguf.is_none() {
                        return bad(format!("{at}: gguf step needs `gguf`"));
                    }
                    if s.background {
                        return bad(format!("{at}: only exec steps run in the background"));
                    }
                }
            }
            if let Some(t) = s.timeout_secs {
                if t == 0 || t > MAX_STEP_TIMEOUT_SECS {
                    return bad(format!(
                        "{at}: timeout_secs must be 1..={MAX_STEP_TIMEOUT_SECS}"
                    ));
                }
            }
            for k in s.env.keys() {
                if !valid_env_key(k) {
                    return bad(format!("{at}: environment key {k:?} is not a valid name"));
                }
            }
            if s.variants.len() > 32 {
                return bad(format!("{at}: at most 32 variants"));
            }
            if s.background && !s.variants.is_empty() {
                return bad(format!("{at}: a background step cannot have variants"));
            }
            for v in &s.variants {
                if v.name.trim().is_empty() {
                    return bad(format!("{at}: every variant needs a name"));
                }
                for k in v.env.keys() {
                    if !valid_env_key(k) {
                        return bad(format!("{at}: environment key {k:?} is not a valid name"));
                    }
                }
                for f in v.set.keys().chain(v.unset.iter()) {
                    if !argv::is_flag(f) {
                        return bad(format!(
                            "{at}: variant {:?} overrides {f:?}, which is not a flag",
                            v.name
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// The largest variant count across steps (1 when no step has
    /// variants): `run --variant N` selects N in 1..=this.
    pub fn variant_count(&self) -> usize {
        self.steps
            .iter()
            .map(|s| s.variants.len())
            .max()
            .unwrap_or(0)
            .max(1)
    }

    /// Expand to the ordered list of planned executions. A step with
    /// variants contributes one entry per variant (or only variant
    /// `only` — 1-based — when given; steps with fewer variants than
    /// `only` are skipped, steps without variants always run).
    pub fn expand(&self, only: Option<usize>) -> Result<Vec<PlannedStep>, RecipeError> {
        if let Some(n) = only {
            let max = self.variant_count();
            if n == 0 || n > max {
                return Err(RecipeError::Invalid(format!(
                    "variant {n} out of range (this recipe has {max})"
                )));
            }
        }
        let mut out = Vec::new();
        for (si, s) in self.steps.iter().enumerate() {
            let mut env = self.env.clone();
            env.extend(s.env.clone());
            if s.variants.is_empty() {
                out.push(PlannedStep {
                    step_index: si,
                    variant_index: None,
                    label: s.name.clone(),
                    variant_name: None,
                    description: s.description.clone(),
                    args: s.args.clone(),
                    env,
                    expect: s.expect,
                });
                continue;
            }
            for (vi, v) in s.variants.iter().enumerate() {
                if only.is_some_and(|n| n != vi + 1) {
                    continue;
                }
                let mut venv = env.clone();
                venv.extend(v.env.clone());
                out.push(PlannedStep {
                    step_index: si,
                    variant_index: Some(vi),
                    label: format!("{} [{}] {}", s.name, vi + 1, v.name),
                    variant_name: Some(v.name.clone()),
                    description: if v.description.is_empty() {
                        s.description.clone()
                    } else {
                        v.description.clone()
                    },
                    args: argv::apply_overrides(&s.args, &v.set, &v.unset),
                    env: venv,
                    expect: v.expect.unwrap_or(s.expect),
                });
            }
        }
        Ok(out)
    }
}

/// One planned execution: a step (or one variant of it) with the
/// variant overrides applied and the environment merged, still in
/// template form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlannedStep {
    pub step_index: usize,
    pub variant_index: Option<usize>,
    pub label: String,
    pub variant_name: Option<String>,
    pub description: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub expect: Expect,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ab() -> Recipe {
        let mut s = Step::exec(
            "bench",
            "{llamacpp_bin}/llama-bench.exe",
            &["-m", "{m}", "-d", "0,4096,8192", "-fa", "1"],
        );
        s.variants = vec![
            Variant {
                name: "base".into(),
                expect: Some(Expect::MayCrash),
                ..Default::default()
            },
            Variant {
                name: "fa0".into(),
                set: [("-fa".to_string(), "0".to_string())].into(),
                ..Default::default()
            },
        ];
        Recipe {
            name: "ab".into(),
            title: String::new(),
            description: String::new(),
            version: 1,
            params: vec![Param {
                name: "m".into(),
                default: None,
                description: String::new(),
            }],
            env: [("CUDA_LAUNCH_BLOCKING".to_string(), "1".to_string())].into(),
            requires: Requires::default(),
            steps: vec![s, Step::exec("smi", "nvidia-smi", &[])],
            builtin: false,
            source: None,
        }
    }

    #[test]
    fn expands_variants_and_merges_env() {
        let r = ab();
        r.validate().unwrap();
        let p = r.expand(None).unwrap();
        assert_eq!(p.len(), 3);
        assert_eq!(p[0].expect, Expect::MayCrash);
        assert_eq!(p[1].args[5], "0");
        assert_eq!(p[1].env["CUDA_LAUNCH_BLOCKING"], "1");
        assert_eq!(p[2].env["CUDA_LAUNCH_BLOCKING"], "1");
        let only = r.expand(Some(2)).unwrap();
        assert_eq!(only.len(), 2, "variant 2 plus the variant-less step");
        assert!(r.expand(Some(3)).is_err());
    }

    #[test]
    fn validation_rejects_bad_shapes() {
        let mut r = ab();
        r.name = "Bad Name".into();
        assert!(r.validate().is_err());
        let mut r = ab();
        r.steps[0].variants[1]
            .set
            .insert("notaflag".into(), "x".into());
        assert!(r.validate().is_err());
        let mut r = ab();
        r.env.insert("A=B".into(), "x".into());
        assert!(r.validate().is_err());
        assert_eq!(slugify("Flash attn A/B test!"), "flash-attn-a-b-test");
    }

    #[test]
    fn json_round_trip_uses_defaults() {
        let r: Recipe =
            serde_json::from_str(r#"{"name":"x","steps":[{"name":"s","exe":"nvidia-smi"}]}"#)
                .unwrap();
        assert!(r.requires.restore);
        assert!(r.steps[0].bench_json);
        r.validate().unwrap();
    }
}
