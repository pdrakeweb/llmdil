//! Rewriting a backend's raw flags for one arm: strip every token
//! that sets a swept axis (cache types, flash attention, the whole
//! spec family, and — when the arm carries its own offload
//! expression — the offload tokens), keep everything else verbatim
//! in order, then append the arm's resolved tokens.

use crate::arm::ArmSpec;

/// Flags that take one value and are stripped from the base raw
/// flags because the arm re-resolves them. Covers both the
/// `--flag value` pair form and the `--flag=value` form; short
/// aliases strip identically.
const STRIPPED_VALUE_FLAGS: &[&str] = &[
    "--cache-type-k",
    "-ctk",
    "--cache-type-v",
    "-ctv",
    "--flash-attn",
    "--spec-type",
    "--spec-ngram-mod-n-min",
    "--spec-ngram-mod-n-max",
    "--spec-ngram-mod-n-match",
    "--spec-draft-model",
    "-md",
    "--model-draft",
    "--spec-draft-n-max",
    "--spec-draft-n-min",
    "--spec-draft-p-min",
    "--spec-draft-p-split",
    "--spec-draft-device",
    "-devd",
    "--spec-draft-ngl",
    "-ngld",
    "--spec-draft-type-k",
    "-ctkd",
    "--spec-draft-type-v",
    "-ctvd",
];

/// Offload flags, stripped only when the arm supplies its own
/// offload expression to append in their place.
const OFFLOAD_VALUE_FLAGS: &[&str] = &["-ot", "--override-tensor", "-ncmoe", "--n-cpu-moe"];

/// The flag name of a token: the part before `=` if present.
fn flag_name(token: &str) -> &str {
    token.split('=').next().unwrap_or(token)
}

/// Filter `tokens`, dropping every token that sets one of `flags`
/// (pair and `=` forms, including the value token of a pair).
fn strip_flags(tokens: &[&str], flags: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let token = tokens[i];
        if flags.contains(&flag_name(token)) {
            // `--flag=value` is one token; `--flag value` also
            // consumes the following token as its value.
            if !token.contains('=') && i + 1 < tokens.len() {
                i += 1;
            }
            i += 1;
            continue;
        }
        out.push(token.to_string());
        i += 1;
    }
    out
}

/// Build the raw-flags string an arm's bench backend launches with.
///
/// `base_raw` is the target backend's existing raw flags (`None` =
/// empty). `draft_path` is the resolved filesystem path of the arm's
/// draft model and `draft_device_arg` its resolved device argument
/// (precomputed by the caller, e.g. `CUDA0` for same-GPU under a
/// visibility restriction or `none` for CPU). Token order of the
/// result: kept base tokens, then the arm's offload expression
/// tokens verbatim when it has one, then the resolved cache/flash
/// tokens, then the spec tokens — `--spec-type` first, then the
/// ngram parameters, then the draft parameters. A draft-bearing arm
/// whose draft path or device argument is missing gets its spec
/// type but no draft parameter tokens (such an arm is pruned
/// upstream; this never invents values for it).
pub fn arm_raw_flags(
    base_raw: Option<&str>,
    arm: &ArmSpec,
    draft_path: Option<&str>,
    draft_device_arg: Option<&str>,
) -> String {
    let base = base_raw.unwrap_or("");
    let tokens: Vec<&str> = base.split_whitespace().collect();
    let mut out = strip_flags(&tokens, STRIPPED_VALUE_FLAGS);
    if arm.offload_expr.is_some() {
        let kept: Vec<&str> = out.iter().map(|s| s.as_str()).collect();
        out = strip_flags(&kept, OFFLOAD_VALUE_FLAGS);
        if let Some(expr) = &arm.offload_expr {
            out.extend(expr.split_whitespace().map(|s| s.to_string()));
        }
    }

    out.push("--cache-type-k".to_string());
    out.push(arm.cache_k.flag().to_string());
    out.push("--cache-type-v".to_string());
    out.push(arm.cache_v.flag().to_string());
    out.push("--flash-attn".to_string());
    out.push(arm.flash_attn.flag().to_string());

    if let Some(spec_type) = arm.spec.spec_type_arg() {
        out.push("--spec-type".to_string());
        out.push(spec_type.to_string());
        if arm.spec.has_ngram() {
            if let Some(n) = arm.ngram_n_min {
                out.push("--spec-ngram-mod-n-min".to_string());
                out.push(n.to_string());
            }
        }
        if arm.spec.has_draft() {
            if let (Some(path), Some(device)) = (draft_path, draft_device_arg) {
                out.push("--spec-draft-model".to_string());
                out.push(path.to_string());
                if let Some(n) = arm.draft_n_max {
                    out.push("--spec-draft-n-max".to_string());
                    out.push(n.to_string());
                }
                out.push("--spec-draft-device".to_string());
                out.push(device.to_string());
                out.push("--spec-draft-type-k".to_string());
                out.push(arm.draft_cache_k.flag().to_string());
                out.push("--spec-draft-type-v".to_string());
                out.push(arm.draft_cache_v.flag().to_string());
            }
        }
    }

    out.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arm::{CacheType, FlashAttn, SpecKind};

    fn arm() -> ArmSpec {
        ArmSpec {
            cache_k: CacheType::Q8_0,
            cache_v: CacheType::Q8_0,
            flash_attn: FlashAttn::Auto,
            spec: SpecKind::None,
            ..ArmSpec::default()
        }
    }

    #[test]
    fn none_spec_appends_only_cache_and_flash() {
        let out = arm_raw_flags(Some("-ot exps=CPU --parallel 1"), &arm(), None, None);
        assert_eq!(
            out,
            "-ot exps=CPU --parallel 1 --cache-type-k q8_0 --cache-type-v q8_0 --flash-attn auto"
        );
        assert!(!out.contains("--spec-type"));
    }

    #[test]
    fn strips_old_tokens_in_pair_and_equals_forms() {
        let base = "--cache-type-k f16 -ctv=q4_0 --flash-attn=off --spec-type draft-simple \
                    --spec-draft-model old.gguf --spec-draft-n-max 4 -devd CUDA1 \
                    -ctkd f16 --spec-ngram-mod-n-min=48 --keep-me yes";
        let out = arm_raw_flags(Some(base), &arm(), None, None);
        assert!(out.starts_with("--keep-me yes "), "{out}");
        for gone in [
            "old.gguf",
            "draft-simple",
            "--spec-draft-n-max",
            "-devd",
            "-ctkd",
            "--spec-ngram-mod-n-min",
        ] {
            assert!(!out.contains(gone), "{gone} survived in {out}");
        }
        assert!(out.contains("--cache-type-k q8_0"));
    }

    #[test]
    fn combined_order_is_spec_type_then_ngram_then_draft() {
        let mut a = arm();
        a.spec = SpecKind::Combined;
        a.ngram_n_min = Some(16);
        a.draft_n_max = Some(3);
        a.draft_cache_k = CacheType::Q8_0;
        a.draft_cache_v = CacheType::Q8_0;
        let out = arm_raw_flags(None, &a, Some("draft.gguf"), Some("none"));
        assert_eq!(
            out,
            "--cache-type-k q8_0 --cache-type-v q8_0 --flash-attn auto \
             --spec-type ngram-mod,draft-simple --spec-ngram-mod-n-min 16 \
             --spec-draft-model draft.gguf --spec-draft-n-max 3 \
             --spec-draft-device none --spec-draft-type-k q8_0 --spec-draft-type-v q8_0"
        );
    }

    #[test]
    fn draft_tokens_skipped_when_path_or_device_missing() {
        let mut a = arm();
        a.spec = SpecKind::DraftSimple;
        a.draft_n_max = Some(3);
        let out = arm_raw_flags(None, &a, None, None);
        assert!(out.contains("--spec-type draft-simple"), "{out}");
        assert!(!out.contains("--spec-draft-model"), "{out}");
    }

    #[test]
    fn offload_expr_replaces_existing_offload_tokens() {
        let mut a = arm();
        a.offload_expr = Some("-ncmoe 46".to_string());
        let out = arm_raw_flags(
            Some("-ot exps=CPU --n-cpu-moe=12 --parallel 1"),
            &a,
            None,
            None,
        );
        assert_eq!(
            out,
            "--parallel 1 -ncmoe 46 --cache-type-k q8_0 --cache-type-v q8_0 --flash-attn auto"
        );
        // Without an arm offload expression the base offload is kept.
        let out = arm_raw_flags(Some("-ot exps=CPU"), &arm(), None, None);
        assert!(out.starts_with("-ot exps=CPU "), "{out}");
    }

    #[test]
    fn empty_base_yields_only_resolved_tokens() {
        let out = arm_raw_flags(None, &arm(), None, None);
        assert_eq!(
            out,
            "--cache-type-k q8_0 --cache-type-v q8_0 --flash-attn auto"
        );
    }
}
