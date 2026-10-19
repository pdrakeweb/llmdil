//! Validation warnings for hand-edited raw llama-server flags
//! (tracker #60, the advanced flags editor).
//!
//! The editor's rule is warn, never block: any string saves, and
//! these warnings are how the app says what it noticed. The check
//! tokenizes with the same whitespace split every flag decoder in
//! this crate uses ([`super::memory::expert_offload`],
//! [`super::mtp::mtp_draft`]) and judges tokens against the flag
//! set the codebase itself recognizes — the flags its decoders
//! own, the flags its launch builder emits or interprets, and the
//! placement/GPU flags its placement code decodes. A flag outside
//! that set is reported as unrecognized, NOT as invalid: llama.cpp
//! knows more flags than this app models, and a hand editor exists
//! precisely for those. The UI mirrors this function for live
//! feedback while typing; this implementation is the authority
//! (the save op returns its result).

use serde::Serialize;

/// One validation warning about a raw-flags string. `token` is
/// the flag (or bare token) the warning is about, spelled as the
/// operator wrote it; `message` is the full warning text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FlagWarning {
    pub token: String,
    pub message: String,
}

/// How a known flag takes its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arity {
    /// No value (`--cpu-moe`, `--jinja`).
    None,
    /// The next token (or an `=` suffix) is the flag's value.
    Value(ValueKind),
    /// Like `Value`, but the flag may repeat without the
    /// duplicates warning (llama.cpp accumulates repeated
    /// `-ot` assignments).
    MultiValue(ValueKind),
}

/// The value shape a flag's value is checked against. Only the
/// shapes the app itself relies on are checked; everything else
/// is `Any` (presence is all the app can judge).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValueKind {
    /// Must parse as an integer (`--parallel`, `--n-cpu-moe`).
    Int,
    /// Any single token.
    Any,
}

/// One recognized flag: its canonical name (duplicates are
/// judged across spellings), whether it is app-reserved (the
/// launch builder filters it out, so it never reaches
/// llama-server), and its arity.
struct FlagSpec {
    canonical: &'static str,
    reserved: bool,
    arity: Arity,
}

/// Look a token's flag name up in the recognized set. Aliases
/// map to one canonical spec so `--parallel` and `-np` count as
/// the same flag for duplicates.
fn flag_spec(name: &str) -> Option<&'static FlagSpec> {
    use Arity::*;
    use ValueKind::*;
    static SPECS: &[(&str, &FlagSpec)] = &[
        // Owned by this crate's decoders (memory.rs / mtp.rs).
        ("--n-cpu-moe", &FlagSpec { canonical: "--n-cpu-moe", reserved: false, arity: Value(Int) }),
        ("-ncmoe", &FlagSpec { canonical: "--n-cpu-moe", reserved: false, arity: Value(Int) }),
        ("--cpu-moe", &FlagSpec { canonical: "--cpu-moe", reserved: false, arity: None }),
        ("-cmoe", &FlagSpec { canonical: "--cpu-moe", reserved: false, arity: None }),
        ("--override-tensor", &FlagSpec { canonical: "--override-tensor", reserved: false, arity: MultiValue(Any) }),
        ("-ot", &FlagSpec { canonical: "--override-tensor", reserved: false, arity: MultiValue(Any) }),
        ("--spec-type", &FlagSpec { canonical: "--spec-type", reserved: false, arity: Value(Any) }),
        ("--spec-draft-n-max", &FlagSpec { canonical: "--spec-draft-n-max", reserved: false, arity: Value(Int) }),
        ("--parallel", &FlagSpec { canonical: "--parallel", reserved: false, arity: Value(Int) }),
        ("-np", &FlagSpec { canonical: "--parallel", reserved: false, arity: Value(Int) }),
        // Emitted from the structured flags by the launch builder
        // (backend_spec): repeating one in raw flags overrides the
        // structured value at llama.cpp's sequential parse.
        ("--model", &FlagSpec { canonical: "--model", reserved: false, arity: Value(Any) }),
        ("-m", &FlagSpec { canonical: "--model", reserved: false, arity: Value(Any) }),
        ("--port", &FlagSpec { canonical: "--port", reserved: false, arity: Value(Int) }),
        ("--ctx-size", &FlagSpec { canonical: "--ctx-size", reserved: false, arity: Value(Int) }),
        ("-c", &FlagSpec { canonical: "--ctx-size", reserved: false, arity: Value(Int) }),
        ("--batch-size", &FlagSpec { canonical: "--batch-size", reserved: false, arity: Value(Int) }),
        ("-b", &FlagSpec { canonical: "--batch-size", reserved: false, arity: Value(Int) }),
        ("--tensor-split", &FlagSpec { canonical: "--tensor-split", reserved: false, arity: Value(Any) }),
        ("-ts", &FlagSpec { canonical: "--tensor-split", reserved: false, arity: Value(Any) }),
        ("--split-mode", &FlagSpec { canonical: "--split-mode", reserved: false, arity: Value(Any) }),
        // Interpreted or remapped by the launch builder's
        // placement code (device selection follows placement).
        ("--device", &FlagSpec { canonical: "--device", reserved: false, arity: Value(Any) }),
        ("-dev", &FlagSpec { canonical: "--device", reserved: false, arity: Value(Any) }),
        ("--main-gpu", &FlagSpec { canonical: "--main-gpu", reserved: false, arity: Value(Int) }),
        ("--gpu-layers", &FlagSpec { canonical: "--gpu-layers", reserved: false, arity: Value(Int) }),
        ("--n-gpu-layers", &FlagSpec { canonical: "--gpu-layers", reserved: false, arity: Value(Int) }),
        ("-ngl", &FlagSpec { canonical: "--gpu-layers", reserved: false, arity: Value(Int) }),
        ("--threads", &FlagSpec { canonical: "--threads", reserved: false, arity: Value(Int) }),
        ("-t", &FlagSpec { canonical: "--threads", reserved: false, arity: Value(Int) }),
        // App-owned launch flags: --jinja is emitted by the app
        // itself; its negation and --host are filtered out of raw
        // flags at launch (backends bind loopback behind the
        // gateway, and tool calling depends on --jinja), so
        // writing them is a no-op the operator should hear about.
        ("--jinja", &FlagSpec { canonical: "--jinja", reserved: false, arity: None }),
        ("--no-jinja", &FlagSpec { canonical: "--no-jinja", reserved: true, arity: None }),
        ("--no_jinja", &FlagSpec { canonical: "--no-jinja", reserved: true, arity: None }),
        ("--host", &FlagSpec { canonical: "--host", reserved: true, arity: Value(Any) }),
    ];
    SPECS.iter().find(|(n, _)| *n == name).map(|(_, s)| *s)
}

/// Warnings for a raw-flags string, in token order. An empty or
/// absent string is valid and warns about nothing. Duplicate
/// and value warnings follow llama.cpp's sequential-parse rule:
/// for a repeated single-value flag the LAST occurrence is the
/// one that takes effect, which is what the warning says.
pub fn raw_flag_warnings(raw_flags: Option<&str>) -> Vec<FlagWarning> {
    let Some(raw) = raw_flags else {
        return Vec::new();
    };
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    let mut warnings: Vec<FlagWarning> = Vec::new();
    let mut seen: Vec<&'static str> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i];
        if !t.starts_with('-') || t == "-" {
            // A bare token the walk did not consume as a value.
            warnings.push(FlagWarning {
                token: t.to_string(),
                message: format!(
                    "`{t}` is not a flag and no flag takes it as a value; \
                     llama-server will reject it"
                ),
            });
            i += 1;
            continue;
        }
        // `--jinja=false` is the launch builder's third filtered
        // no-jinja spelling; judge it as that flag, not as --jinja
        // carrying a value.
        let (name, inline_value) = if t == "--jinja=false" {
            ("--no-jinja", None)
        } else {
            match t.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (t, None),
            }
        };
        let Some(spec) = flag_spec(name) else {
            warnings.push(FlagWarning {
                token: t.to_string(),
                message: format!(
                    "`{name}` is not a flag this app recognizes; \
                     it is passed to llama-server unchanged"
                ),
            });
            // An unrecognized flag's arity is unknowable; assume
            // the common one-value shape so its value is not
            // misreported as a stray token.
            if inline_value.is_none()
                && tokens
                    .get(i + 1)
                    .map(|v| !v.starts_with('-'))
                    .unwrap_or(false)
            {
                i += 1;
            }
            i += 1;
            continue;
        };
        if spec.reserved {
            warnings.push(FlagWarning {
                token: t.to_string(),
                message: format!(
                    "`{name}` is reserved: the app filters it out before launch, \
                     so it has no effect here"
                ),
            });
        }
        match spec.arity {
            Arity::None => {
                if inline_value.is_some() {
                    warnings.push(FlagWarning {
                        token: t.to_string(),
                        message: format!("`{name}` takes no value"),
                    });
                }
                if seen.contains(&spec.canonical) {
                    warnings.push(FlagWarning {
                        token: t.to_string(),
                        message: format!("`{}` is repeated", spec.canonical),
                    });
                }
                seen.push(spec.canonical);
                i += 1;
            }
            Arity::Value(kind) | Arity::MultiValue(kind) => {
                if seen.contains(&spec.canonical) && matches!(spec.arity, Arity::Value(_)) {
                    warnings.push(FlagWarning {
                        token: t.to_string(),
                        message: format!(
                            "`{}` is set more than once; the last value wins",
                            spec.canonical
                        ),
                    });
                }
                seen.push(spec.canonical);
                let value = match inline_value {
                    Some(v) => {
                        i += 1;
                        Some(v)
                    }
                    None => {
                        let v = tokens.get(i + 1).copied();
                        i += if v.is_some() { 2 } else { 1 };
                        v
                    }
                };
                match value {
                    None => warnings.push(FlagWarning {
                        token: t.to_string(),
                        message: format!("`{name}` has no value"),
                    }),
                    Some(v) => {
                        if kind == ValueKind::Int && v.parse::<i64>().is_err() {
                            warnings.push(FlagWarning {
                                token: t.to_string(),
                                message: format!(
                                    "`{name}` expects a number; `{v}` is not one"
                                ),
                            });
                        }
                    }
                }
            }
        }
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;

    fn messages(raw: &str) -> Vec<String> {
        raw_flag_warnings(Some(raw))
            .into_iter()
            .map(|w| w.message)
            .collect()
    }

    #[test]
    fn empty_and_absent_warn_about_nothing() {
        assert!(raw_flag_warnings(None).is_empty());
        assert!(raw_flag_warnings(Some("")).is_empty());
        assert!(raw_flag_warnings(Some("   ")).is_empty());
    }

    #[test]
    fn recognized_clean_flags_warn_about_nothing() {
        // Pete's planner-mtp string: the exact flags the #53
        // warning sent him to set.
        assert!(messages("--parallel 1 --spec-type draft-mtp --spec-draft-n-max 2").is_empty());
        assert!(messages("-ot exps=CPU").is_empty());
        assert!(messages("--n-cpu-moe=46 --threads 8").is_empty());
        assert!(messages("-np 1 --spec-type=draft-mtp").is_empty());
        // Negative values parse: --parallel -1 is llama.cpp's auto.
        assert!(messages("--parallel -1").is_empty());
    }

    #[test]
    fn unknown_flag_warns_and_consumes_a_value() {
        let w = raw_flag_warnings(Some("--flash-attn on --parallel 1"));
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].token, "--flash-attn");
        assert!(w[0].message.contains("not a flag this app recognizes"));
        // An unknown `=` spelling warns once, on the flag.
        let w = raw_flag_warnings(Some("--cache-type-k=q8_0"));
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].token, "--cache-type-k=q8_0");
    }

    #[test]
    fn malformed_values_for_known_flags_warn() {
        let w = raw_flag_warnings(Some("--parallel abc"));
        assert_eq!(w.len(), 1);
        assert!(w[0].message.contains("expects a number"));
        let w = raw_flag_warnings(Some("--spec-draft-n-max"));
        assert_eq!(w.len(), 1);
        assert!(w[0].message.contains("has no value"));
    }

    #[test]
    fn duplicates_warn_once_per_repeat_except_multi_value() {
        let w = raw_flag_warnings(Some("--parallel 4 --parallel 1"));
        assert_eq!(w.len(), 1);
        assert!(w[0].message.contains("last value wins"));
        // Aliases are the same flag for duplicates.
        let w = raw_flag_warnings(Some("--parallel 4 -np 1"));
        assert_eq!(w.len(), 1);
        // -ot accumulates: no duplicates warning.
        assert!(messages("-ot exps=CPU -ot blk.1.=CPU").is_empty());
    }

    #[test]
    fn reserved_flags_warn_they_are_filtered() {
        let w = raw_flag_warnings(Some("--host 0.0.0.0"));
        assert_eq!(w.len(), 1);
        assert!(w[0].message.contains("reserved"));
        let w = raw_flag_warnings(Some("--no-jinja"));
        assert_eq!(w.len(), 1);
        assert!(w[0].message.contains("reserved"));
        // The `--jinja=false` spelling is the same filtered flag.
        let w = raw_flag_warnings(Some("--jinja=false"));
        assert_eq!(w.len(), 1);
        assert!(w[0].message.contains("reserved"));
        // --jinja itself is the app's own launch flag: recognized.
        assert!(messages("--jinja").is_empty());
    }

    #[test]
    fn stray_bare_token_warns() {
        let w = raw_flag_warnings(Some("--parallel 1 frobnicate"));
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].token, "frobnicate");
    }

    #[test]
    fn valueless_flag_with_inline_value_warns() {
        let w = raw_flag_warnings(Some("--cpu-moe=yes"));
        assert_eq!(w.len(), 1);
        assert!(w[0].message.contains("takes no value"));
    }
}
