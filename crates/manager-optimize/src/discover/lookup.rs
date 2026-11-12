//! Form lookup + prefill mapping (deep-discovery design Part 2).

use super::{fit_verdict, quant_files, HfModelFacts, QuantFile};

/// One repo row of the form lookup: the record's facts plus
/// one entry per quant file, each with its fit verdict
/// against the snapshot the lookup ran with.
#[derive(Debug, Clone, PartialEq)]
pub struct LookupRepo {
    pub repo_id: String,
    pub downloads: u64,
    pub likes: u64,
    pub architecture: Option<String>,
    /// Total parameter count (not billions).
    pub params_total: Option<u64>,
    pub ctx_max: Option<u32>,
    pub quants: Vec<LookupQuant>,
}

/// One quant of a [`LookupRepo`], with the fit verdict the
/// picker row shows (`File · Size · Fit`, §P2.4).
#[derive(Debug, Clone, PartialEq)]
pub struct LookupQuant {
    pub quant: QuantFile,
    pub fit: String,
}

/// Assemble one lookup row from a model record: quants from
/// the sibling grouping, fit from the same evaluator the
/// consultation pre-pass uses.
pub fn lookup_repo(facts: &HfModelFacts, free_vram_mib_per_gpu: &[u64]) -> LookupRepo {
    let params_b = facts
        .params_total
        .map(|t| t as f32 / 1_000_000_000.0)
        .unwrap_or(0.0);
    let ctx_max = facts.ctx_max.unwrap_or(0);
    LookupRepo {
        repo_id: facts.repo_id.clone(),
        downloads: facts.downloads,
        likes: facts.likes,
        architecture: facts.architecture.clone(),
        params_total: facts.params_total,
        ctx_max: facts.ctx_max,
        quants: quant_files(&facts.siblings)
            .into_iter()
            .map(|q| LookupQuant {
                fit: fit_verdict(q.size_bytes, ctx_max, params_b, free_vram_mib_per_gpu),
                quant: q,
            })
            .collect(),
    }
}

/// The `params_b` a pick prefills: the record's total
/// parameter count in billions at one decimal (§P2.5; the
/// planner family's display convention, principle 14).
/// 79,674,391,296 → 79.7.
pub fn params_b_rounded(params_total: u64) -> f32 {
    ((params_total as f64 / 1_000_000_000.0) * 10.0).round() as f32 / 10.0
}

/// The model id a pick prefills, derived from the picked
/// file's stem (§P2.5): lowercased, with runs of separator
/// characters collapsed to single `-`. Underscores are part
/// of quant names and survive: `Qwen3-Coder-Next-Q4_K_M.gguf`
/// → `qwen3-coder-next-q4_k_m`.
pub fn id_slug_from_file(file: &str) -> String {
    let name = file.rsplit('/').next().unwrap_or(file);
    let stem = name
        .strip_suffix(".gguf")
        .or_else(|| name.strip_suffix(".GGUF"))
        .unwrap_or(name);
    let mut out = String::new();
    let mut pending_dash = false;
    for ch in stem.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(ch.to_ascii_lowercase());
        } else {
            pending_dash = true;
        }
    }
    out
}

/// One `-`-separated segment that names a GGUF quantization
/// on its own: `BF16` / `F16` / `F32`, or the `Q`/`IQ` family
/// (`Q4_K_M`, `Q8_0`, `IQ3_S`, `IQ2_XXS`, …) — a Q, an
/// optional leading I, a digit run, an underscore, and a
/// suffix of letters, digits, and underscores.
fn is_quant_token(seg: &str) -> bool {
    if matches!(seg, "BF16" | "F16" | "F32") {
        return true;
    }
    let body = seg.strip_prefix('I').unwrap_or(seg);
    let Some(rest) = body.strip_prefix('Q') else {
        return false;
    };
    let Some((digits, suffix)) = rest.split_once('_') else {
        return false;
    };
    !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && !suffix.is_empty()
        && suffix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// The quant label a pick prefills, derived locally from the
/// file name's quant token (§P2.5): the last `-` segment that
/// matches the GGUF quant vocabulary (one trailing non-quant
/// segment, e.g. a `-mtp` marker, is skipped), with a `UD-`
/// segment directly before it kept as part of the label.
/// Derived, never authoritative — the label is a form
/// prefill the operator can edit. No match → `None`, and the
/// form leaves the field empty (the add defaults it to
/// "unknown").
pub fn quant_token_from_file_name(file: &str) -> Option<String> {
    let name = file.rsplit('/').next().unwrap_or(file);
    let stem = name
        .strip_suffix(".gguf")
        .or_else(|| name.strip_suffix(".GGUF"))
        .unwrap_or(name);
    let upper = stem.to_uppercase();
    let segs: Vec<&str> = upper.split('-').collect();
    for back in 0..segs.len().min(2) {
        let idx = segs.len() - 1 - back;
        let seg = segs[idx];
        if is_quant_token(seg) {
            if idx > 0 && segs[idx - 1] == "UD" {
                return Some(format!("UD-{seg}"));
            }
            return Some(seg.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discover::test_fixtures::*;
    use crate::discover::*;

    // -- prefill mapping (Part 2, §P2.5) --------------------------------------

    #[test]
    fn id_slug_matches_the_design_example() {
        assert_eq!(
            id_slug_from_file("Qwen3-Coder-Next-Q4_K_M.gguf"),
            "qwen3-coder-next-q4_k_m"
        );
        // Dots are separators and collapse; underscores survive.
        assert_eq!(
            id_slug_from_file("qwen2.5-coder-7b-instruct-q4_k_m.gguf"),
            "qwen2-5-coder-7b-instruct-q4_k_m"
        );
        // Subdirectory paths reduce to the file's own stem.
        assert_eq!(id_slug_from_file("q4/Model Q8_0.gguf"), "model-q8_0");
    }

    #[test]
    fn quant_token_reads_the_vocabulary_forms() {
        assert_eq!(
            quant_token_from_file_name("Qwen3-8B-Q4_K_M.gguf").as_deref(),
            Some("Q4_K_M")
        );
        assert_eq!(
            quant_token_from_file_name("Qwen3-Coder-Next-UD-Q4_K_M.gguf").as_deref(),
            Some("UD-Q4_K_M")
        );
        assert_eq!(
            quant_token_from_file_name("model-IQ3_S.gguf").as_deref(),
            Some("IQ3_S")
        );
        assert_eq!(
            quant_token_from_file_name("model-UD-IQ2_XXS.gguf").as_deref(),
            Some("UD-IQ2_XXS")
        );
        assert_eq!(
            quant_token_from_file_name("model-BF16.gguf").as_deref(),
            Some("BF16")
        );
        // Lowercase file names normalize to the label's case.
        assert_eq!(
            quant_token_from_file_name("qwen2.5-coder-7b-instruct-q4_k_m.gguf").as_deref(),
            Some("Q4_K_M")
        );
        // A trailing -mtp marker is not the quant; the token
        // before it is.
        assert_eq!(
            quant_token_from_file_name("Qwen3.8-27B-GSQ-RCO-IQ3_S-mtp.gguf").as_deref(),
            Some("IQ3_S")
        );
    }

    #[test]
    fn quant_token_unmatched_leaves_the_field_empty() {
        assert_eq!(quant_token_from_file_name("model.gguf"), None);
        assert_eq!(quant_token_from_file_name("model-fp16.gguf"), None);
        assert_eq!(quant_token_from_file_name("model-7B.gguf"), None);
    }

    #[test]
    fn params_b_rounds_to_one_decimal() {
        // The design's worked figure: Coder-Next's total.
        assert_eq!(params_b_rounded(79_674_391_296), 79.7);
        assert_eq!(params_b_rounded(8_190_735_360), 8.2);
        assert_eq!(params_b_rounded(27_000_000_000), 27.0);
    }

    #[test]
    fn lookup_repo_rows_carry_facts_quants_and_fit() {
        let facts = parse_model_record(&record_body()).unwrap();
        let row = lookup_repo(&facts, &[16 * 1024]);
        assert_eq!(row.repo_id, "Qwen/Qwen3-8B-GGUF");
        assert_eq!(row.downloads, 594664);
        assert_eq!(row.architecture.as_deref(), Some("qwen3"));
        assert_eq!(row.params_total, Some(8190735360));
        assert_eq!(row.ctx_max, Some(40960));
        assert_eq!(row.quants.len(), 2);
        assert_eq!(row.quants[0].quant.file, "Qwen3-8B-Q4_K_M.gguf");
        assert_eq!(row.quants[0].quant.sha256.as_deref(), Some("d98c"));
        assert_eq!(row.quants[0].fit, FIT_FITS);
        // No telemetry: every fit is honestly unchecked.
        let row = lookup_repo(&facts, &[]);
        assert!(row.quants.iter().all(|q| q.fit == FIT_UNCHECKED));
    }
}
