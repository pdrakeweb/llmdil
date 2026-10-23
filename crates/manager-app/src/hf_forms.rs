//! Add/Edit Model form population from Hugging Face
//! (deep-discovery design Part 2, §P2.11): the two form-helper
//! operations behind the Add Model pick list and its
//! type-ahead and the Edit form's re-fetch, plus the pure
//! proposal logic the forms render — the default-offload
//! suggestion (P2-D5), the post-download MoE reconciliation
//! (P2-D7), and the re-fetch proposals (P2-D8).
//!
//! Everything here is proposal-grade: a lookup or header
//! read fills form fields and editor lines, and nothing is
//! stored until the operator submits the form or saves the
//! editor (P2-D1). A failed lookup or header read surfaces
//! as an error or an absent header — never as a blocked add:
//! the manual form works with Hugging Face unreachable.

use std::sync::Arc;

use serde_json::{json, Value};

use manager_models::GgufLayout;
use manager_optimize::{
    id_slug_from_file, params_b_rounded, quant_files, quant_token_from_file_name, HfClient,
    LookupRepo, MAX_LOOKUP_REPOS,
};

use crate::{ApiError, ApiResult, AppState, EventSink};

/// Per-GPU VRAM from the live telemetry snapshot, in GPU
/// index order: (free MiB per GPU, total MiB per GPU). Empty
/// vectors before the first poll — fit verdicts then report
/// "unchecked" and the offload suggestion is absent, both
/// honest unknowns rather than zeros.
async fn gpu_vram(state: &Arc<AppState>) -> (Vec<u64>, Vec<u64>) {
    let store = state.telemetry().await;
    let guard = crate::lock_telemetry(&store);
    let mut samples = guard.snapshot();
    samples.sort_by_key(|s| s.index);
    let free = samples
        .iter()
        .map(|s| s.vram_total_mib.saturating_sub(s.vram_used_mib))
        .collect();
    let total = samples.iter().map(|s| s.vram_total_mib).collect();
    (free, total)
}

/// The curated-catalog model whose source file lives in
/// `repo_id`, if any: the picker's `Catalog` chip (§P2.4).
/// A repo match is by the spec's source URL prefix — the
/// catalog names repos, and a repo hosting the catalog file
/// is a catalog repo for the picker even when it also hosts
/// other files.
pub fn catalog_repo_match(repo_id: &str) -> Option<String> {
    let prefix = format!("https://huggingface.co/{repo_id}/");
    manager_models::curated_models()
        .into_iter()
        .find(|s| s.url.starts_with(&prefix))
        .map(|s| s.id)
}

/// Split an HF resolve URL back into (repo id, file) — the
/// inverse of the source URL a pick constructs (§P2.5). The
/// Edit form's re-fetch is offered only when this recovers
/// both (P2-D8). The file keeps any subdirectory path.
pub fn parse_hf_source_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("https://huggingface.co/")?;
    let (repo, tail) = rest.split_once("/resolve/")?;
    if repo.is_empty() || !repo.contains('/') {
        return None;
    }
    let (_branch, file) = tail.split_once('/')?;
    if file.is_empty() {
        return None;
    }
    Some((repo.to_string(), file.to_string()))
}

/// The default-offload suggestion for a parsed header
/// (P2-D5): the smallest N in `[0, moe_layers]` whose
/// GPU-resident weight bytes fit the machine's largest
/// single GPU's *total* VRAM — the add flow has no backend
/// placement yet, so the suggestion is a property of the
/// model and the machine, not of the moment's free memory.
/// Returns `(suggestion, needs_split)`: `needs_split` is
/// true when even full offload does not fit, in which case
/// the suggestion is full offload and the picker notes the
/// model needs a tensor split. No MoE geometry or no
/// telemetry → `(None, false)` — no suggestion is shown
/// rather than a fabricated one. The rule optimizes for
/// *fits*, not *fastest*; it is a starting point the
/// operator edits.
pub fn suggested_default_offload(
    layout: &GgufLayout,
    gpu_totals_mib: &[u64],
) -> (Option<u32>, bool) {
    let Some(moe_layers) = layout.moe_block_count else {
        return (None, false);
    };
    let Some(largest_mib) = gpu_totals_mib.iter().max() else {
        return (None, false);
    };
    let largest_bytes = largest_mib * 1024 * 1024;
    let sys = manager_models::sys_bytes_by_position(
        layout.expert_bytes,
        Some(&layout.expert_bytes_by_block),
        moe_layers,
    );
    for n in 0..=moe_layers {
        let gpu_bytes = layout.tensor_bytes.saturating_sub(sys[n as usize]);
        if gpu_bytes <= largest_bytes {
            return (Some(n), false);
        }
    }
    (Some(moe_layers), true)
}

/// The post-download MoE reconciliation proposal (P2-D7):
/// `Some((declared, file))` when a downloaded model's stored
/// declaration disagrees with its own file's parsed MoE block
/// count. No proposal when the catalog pins the figure (a
/// parse/catalog disagreement there is a catalog defect, not
/// an operator edit), when nothing is declared, or when the
/// file has not been parsed. The record never self-corrects:
/// the editor shows both figures and the operator applies
/// the file value explicitly.
pub fn moe_reconciliation(
    declared_layers: Option<u32>,
    catalog_layers: Option<u32>,
    file_layers: Option<u32>,
) -> Option<(u32, u32)> {
    if catalog_layers.is_some() {
        return None;
    }
    let declared = declared_layers?;
    let file = file_layers?;
    (declared != file).then_some((declared, file))
}

/// The stored facts a re-fetch compares against (P2-D8).
pub struct StoredFacts {
    /// The record's declared MoE layer count.
    pub moe_layers: Option<u32>,
    /// The record's MTP declaration.
    pub mtp_declared: Option<bool>,
    /// The record's pinned SHA-256 (empty when unpinned).
    pub sha256: String,
    /// Any catalog pin on this model: a pinned model is on
    /// the never-touch list wholesale.
    pub catalog_pinned: bool,
}

/// The fetched facts a re-fetch compares (P2-D8): the header
/// parse plus the record's current `lfs.sha256` for the file.
pub struct FetchedFacts {
    pub moe_layers: Option<u32>,
    pub mtp: bool,
    pub sha256: Option<String>,
}

/// The proposals a re-fetch presents: each is an explicit
/// per-field apply in the editor, never an automatic write.
#[derive(Debug, Clone, PartialEq)]
pub struct RefetchProposals {
    /// Fetched MoE layer count differing from the stored
    /// declaration (or filling an empty one).
    pub moe_file_layers: Option<u32>,
    /// The header carries an MTP head the record does not
    /// declare.
    pub mtp_proposed: bool,
    /// The repository's current hash for the file differs
    /// from the stored pin: `(stored, fetched)`. Surfaced
    /// as a warning line only — a re-fetch never re-pins.
    pub hash_changed: Option<(String, String)>,
}

/// Compare stored vs fetched facts into re-fetch proposals
/// (P2-D8). The never-touch list is structural here: the
/// inputs carry no display name, enabled flag, backend
/// assignments, MoE *default*, or verified state, so no
/// proposal can name them; a catalog-pinned model yields no
/// proposals at all.
pub fn refetch_proposals(stored: &StoredFacts, fetched: &FetchedFacts) -> RefetchProposals {
    if stored.catalog_pinned {
        return RefetchProposals {
            moe_file_layers: None,
            mtp_proposed: false,
            hash_changed: None,
        };
    }
    let moe_file_layers = match fetched.moe_layers {
        Some(f) if stored.moe_layers != Some(f) => Some(f),
        _ => None,
    };
    let mtp_proposed = fetched.mtp && stored.mtp_declared != Some(true);
    let hash_changed = match (&stored.sha256, &fetched.sha256) {
        (pin, Some(f)) if crate::is_real_pin(pin) && pin != f => {
            Some((pin.clone(), f.clone()))
        }
        _ => None,
    };
    RefetchProposals {
        moe_file_layers,
        mtp_proposed,
        hash_changed,
    }
}

/// One lookup row as the picker renders it (§P2.4): repo
/// facts as `Label: Value` data, the `Catalog` chip state,
/// and per-quant rows with the prefill derivations (§P2.5)
/// computed here so the form copies values rather than
/// re-deriving them.
fn repo_json(row: &LookupRepo) -> Value {
    json!({
        "repo_id": row.repo_id,
        "downloads": row.downloads,
        "likes": row.likes,
        "architecture": row.architecture,
        "params_b": row.params_total.map(params_b_rounded),
        "ctx_max": row.ctx_max,
        "catalog_model_id": catalog_repo_match(&row.repo_id),
        "quants": row.quants.iter().map(|q| json!({
            "file": q.quant.file,
            "size_bytes": q.quant.size_bytes,
            "shard_count": q.quant.shard_count,
            "sha256": q.quant.sha256,
            "fit": q.fit,
            "id_slug": id_slug_from_file(&q.quant.file),
            "quant": quant_token_from_file_name(&q.quant.file),
            "source_url": format!(
                "https://huggingface.co/{}/resolve/main/{}",
                row.repo_id, q.quant.file
            ),
        })).collect::<Vec<_>>(),
    })
}

/// The Add Model form's Hugging Face lookup (§P2.11): with
/// `suggest`, the type-ahead's name-only search at the same
/// shallow depth (P2-D2) — repo ids and download counts, no
/// record fetches, one search per debounce window. Without
/// it, the full pick list: repo rows with per-repo facts and
/// quant files with fit against the live free-VRAM snapshot.
/// A failed search is an error the form shows with manual
/// entry untouched.
pub async fn op_model_hf_lookup(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    query: String,
    suggest: Option<bool>,
) -> ApiResult {
    let query = query.trim().to_string();
    if query.is_empty() {
        return Err(ApiError::InvalidInput(
            "a model name or search term is required".to_string(),
        ));
    }
    let client = HfClient::with_default_base()
        .map_err(|e| ApiError::InvalidInput(e.to_string()))?;
    if suggest.unwrap_or(false) {
        let hits = client
            .search_with_limit(&query, MAX_LOOKUP_REPOS)
            .await
            .map_err(|e| ApiError::InvalidInput(e.to_string()))?;
        return Ok(json!({
            "suggest": true,
            "repos": hits.iter().map(|h| json!({
                "repo_id": h.id,
                "downloads": h.downloads,
            })).collect::<Vec<_>>(),
        }));
    }
    let (free, _totals) = gpu_vram(state).await;
    let rows = client
        .lookup(&query, &free)
        .await
        .map_err(|e| ApiError::InvalidInput(e.to_string()))?;
    Ok(json!({
        "suggest": false,
        "repos": rows.iter().map(repo_json).collect::<Vec<_>>(),
    }))
}

/// The pick-time GGUF header read for one picked file
/// (§P2.11): a range read of the file's header, parsed with
/// the local GGUF parser (P2-D4, Option B). The payload
/// carries the geometry the prefill consumes — MoE block
/// count, total blocks, expert bytes, the in-file MTP
/// signal — plus the P2-D5 default-offload suggestion
/// against the largest GPU's total VRAM, whether this exact
/// file is a curated-catalog file (its pins then govern and
/// no fetched value is prefilled over them, §P2.8 rule 1),
/// and the record's current `lfs.sha256` for the file (the
/// Edit form's re-fetch reads it for the changed-hash
/// warning). The header read itself is best-effort by
/// contract: any failure yields `"header": null` and the
/// form proceeds unprefilled for geometry — the add is never
/// blocked by this operation.
pub async fn op_model_hf_header(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    repo_id: String,
    file: String,
) -> ApiResult {
    let repo_id = repo_id.trim().to_string();
    let file = file.trim().to_string();
    if repo_id.is_empty() || file.is_empty() {
        return Err(ApiError::InvalidInput(
            "a repository id and a file name are required".to_string(),
        ));
    }
    let client = HfClient::with_default_base()
        .map_err(|e| ApiError::InvalidInput(e.to_string()))?;
    let source_url = format!("https://huggingface.co/{repo_id}/resolve/main/{file}");
    // Catalog detection is file-exact (the resolvers match
    // the constructed source URL): a catalog repo hosting
    // other files does not pin those files.
    let catalog = manager_models::catalog_params_b("", &source_url).is_some();
    // The record's current hash for this file. Best-effort:
    // a failed record fetch leaves it absent, exactly like a
    // record that reports no hash.
    let sha256 = match client.model_facts(&repo_id).await {
        Ok(facts) => quant_files(&facts.siblings)
            .into_iter()
            .find(|q| q.file == file)
            .and_then(|q| q.sha256),
        Err(_) => None,
    };
    let (_free, totals) = gpu_vram(state).await;
    let header = match client.fetch_header(&repo_id, &file).await {
        Ok(layout) => {
            let (suggestion, needs_split) = suggested_default_offload(&layout, &totals);
            Some(json!({
                "moe_layers": layout.moe_block_count,
                "block_count": layout.block_count,
                "expert_bytes": layout.expert_bytes,
                "total_bytes": layout.tensor_bytes,
                "mtp": layout.nextn_predict_layers.map(|n| n > 0).unwrap_or(false)
                    || layout.has_nextn_tensors,
                "suggested_default_offload_layers": suggestion,
                "needs_split": needs_split,
            }))
        }
        Err(e) => {
            tracing::warn!(
                "HF header read for {repo_id}/{file} unavailable: {e}"
            );
            None
        }
    };
    Ok(json!({
        "repo_id": repo_id,
        "file": file,
        "catalog": catalog,
        "sha256": sha256,
        "header": header,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(moe_blocks: u32, per_block: u64, dense_bytes: u64) -> GgufLayout {
        let expert_bytes = per_block * moe_blocks as u64;
        GgufLayout {
            tensor_bytes: dense_bytes + expert_bytes,
            expert_bytes,
            expert_bytes_by_block: vec![per_block; moe_blocks as usize],
            block_count: Some(moe_blocks),
            moe_block_count: (moe_blocks > 0).then_some(moe_blocks),
            nextn_predict_layers: None,
            has_nextn_tensors: false,
            kv: None,
            rs: None,
        }
    }

    #[test]
    fn suggestion_is_the_smallest_fitting_offload() {
        // 14.4 GB of experts over 48 blocks + 6 GB dense
        // against one 16,376 MiB GPU (~17.2 GB): the whole
        // model does not fit resident, full offload is not
        // needed — the suggestion is the first N whose
        // resident bytes fit, not a fixed endpoint.
        let l = layout(48, 300_000_000, 6_000_000_000);
        let (s, split) = suggested_default_offload(&l, &[16_376]);
        assert!(!split);
        let n = s.unwrap();
        let sys = manager_models::sys_bytes_by_position(
            l.expert_bytes,
            Some(&l.expert_bytes_by_block),
            48,
        );
        assert!(l.tensor_bytes - sys[n as usize] <= 16_376 * 1024 * 1024);
        assert!(n == 0 || l.tensor_bytes - sys[(n - 1) as usize] > 16_376 * 1024 * 1024);
    }

    #[test]
    fn suggestion_zero_when_the_model_fits_whole() {
        let l = layout(48, 100_000_000, 2_000_000_000);
        assert_eq!(suggested_default_offload(&l, &[16_376]), (Some(0), false));
    }

    #[test]
    fn suggestion_full_offload_with_split_note_when_nothing_fits() {
        // 30 GiB dense alone exceeds the largest GPU: even
        // full expert offload cannot fit it on one card.
        let l = layout(48, 100_000_000, 30_000_000_000);
        assert_eq!(
            suggested_default_offload(&l, &[16_376]),
            (Some(48), true)
        );
    }

    #[test]
    fn suggestion_absent_without_geometry_or_telemetry() {
        let dense = layout(0, 0, 5_000_000_000);
        assert_eq!(suggested_default_offload(&dense, &[16_376]), (None, false));
        let moe = layout(48, 100_000_000, 2_000_000_000);
        assert_eq!(suggested_default_offload(&moe, &[]), (None, false));
    }

    #[test]
    fn suggestion_prices_against_the_largest_gpu() {
        // Fits the 16 GiB card whole only because the largest
        // total governs — the 8 GiB card alone would suggest
        // offload.
        let l = layout(48, 100_000_000, 9_000_000_000);
        assert_eq!(suggested_default_offload(&l, &[8_192, 16_376]), (Some(0), false));
    }

    #[test]
    fn reconciliation_proposes_only_a_real_mismatch() {
        assert_eq!(moe_reconciliation(Some(48), None, Some(46)), Some((48, 46)));
        assert_eq!(moe_reconciliation(Some(48), None, Some(48)), None);
        // Nothing declared: no stored figure to disagree.
        assert_eq!(moe_reconciliation(None, None, Some(46)), None);
        // Not downloaded / unparseable: no file figure.
        assert_eq!(moe_reconciliation(Some(48), None, None), None);
        // Catalog-pinned: never a proposal (a catalog defect
        // is reported, not edited around).
        assert_eq!(moe_reconciliation(Some(48), Some(48), Some(46)), None);
    }

    #[test]
    fn refetch_never_proposes_over_stored_values_or_pins() {
        let stored = StoredFacts {
            moe_layers: Some(48),
            mtp_declared: Some(true),
            sha256: "a".repeat(64),
            catalog_pinned: false,
        };
        // Fetched facts agree with everything stored: the
        // change set is empty — operator-held values never
        // appear in it.
        let fetched = FetchedFacts {
            moe_layers: Some(48),
            mtp: true,
            sha256: Some("a".repeat(64)),
        };
        assert_eq!(
            refetch_proposals(&stored, &fetched),
            RefetchProposals {
                moe_file_layers: None,
                mtp_proposed: false,
                hash_changed: None,
            }
        );
        // A catalog-pinned model yields nothing at all, even
        // with every fetched fact different.
        let pinned = StoredFacts {
            moe_layers: Some(48),
            mtp_declared: None,
            sha256: "a".repeat(64),
            catalog_pinned: true,
        };
        let different = FetchedFacts {
            moe_layers: Some(46),
            mtp: true,
            sha256: Some("b".repeat(64)),
        };
        assert_eq!(
            refetch_proposals(&pinned, &different),
            RefetchProposals {
                moe_file_layers: None,
                mtp_proposed: false,
                hash_changed: None,
            }
        );
    }

    #[test]
    fn refetch_proposes_geometry_and_warns_on_a_changed_hash() {
        let stored = StoredFacts {
            moe_layers: Some(48),
            mtp_declared: None,
            sha256: "a".repeat(64),
            catalog_pinned: false,
        };
        let fetched = FetchedFacts {
            moe_layers: Some(46),
            mtp: true,
            sha256: Some("b".repeat(64)),
        };
        let p = refetch_proposals(&stored, &fetched);
        assert_eq!(p.moe_file_layers, Some(46));
        assert!(p.mtp_proposed);
        assert_eq!(
            p.hash_changed,
            Some(("a".repeat(64), "b".repeat(64)))
        );
        // An unpinned record has no hash to change.
        let unpinned = StoredFacts {
            moe_layers: None,
            mtp_declared: None,
            sha256: String::new(),
            catalog_pinned: false,
        };
        assert_eq!(refetch_proposals(&unpinned, &fetched).hash_changed, None);
        // A stored-empty MoE declaration takes the fetched
        // value as a proposal (P2-D8).
        assert_eq!(
            refetch_proposals(&unpinned, &fetched).moe_file_layers,
            Some(46)
        );
    }

    #[test]
    fn source_url_parsing_recovers_repo_and_file() {
        assert_eq!(
            parse_hf_source_url(
                "https://huggingface.co/unsloth/Qwen3-Coder-Next-GGUF/resolve/main/Qwen3-Coder-Next-UD-Q4_K_M.gguf"
            ),
            Some((
                "unsloth/Qwen3-Coder-Next-GGUF".to_string(),
                "Qwen3-Coder-Next-UD-Q4_K_M.gguf".to_string()
            ))
        );
        // Subdirectory files keep their path.
        assert_eq!(
            parse_hf_source_url("https://huggingface.co/org/repo/resolve/main/q4/model.gguf"),
            Some(("org/repo".to_string(), "q4/model.gguf".to_string()))
        );
        // Not an HF resolve URL: no re-fetch is offered.
        assert_eq!(parse_hf_source_url("https://example.com/model.gguf"), None);
        assert_eq!(
            parse_hf_source_url("https://huggingface.co/org/repo/model.gguf"),
            None
        );
    }

    #[test]
    fn catalog_repo_match_names_the_catalog_model() {
        assert_eq!(
            catalog_repo_match("unsloth/Qwen3-Coder-Next-GGUF").as_deref(),
            Some("qwen3-coder-30b")
        );
        assert_eq!(catalog_repo_match("org/not-a-catalog-repo"), None);
    }
}
