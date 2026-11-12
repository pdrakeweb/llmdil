//! HF response types and their pure parsers (search hits, model
//! records, siblings), plus quant grouping of sibling files.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::{DiscoverError, MAX_SEARCH_REPOS};

/// One entry of the HF search response (`GET /api/models`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct HfSearchHit {
    pub id: String,
    #[serde(default)]
    pub downloads: u64,
    #[serde(default)]
    pub likes: u64,
    /// `lastModified` (RFC 3339): present only on
    /// `full=true` search hits and on model records — the
    /// default search fields do not carry it (§P1.3). The
    /// deep pass searches with `full=true` so the
    /// search-layer staleness filter has it without any
    /// record fetch.
    #[serde(default, rename = "lastModified")]
    pub last_modified: Option<String>,
}

/// Parse a search response body into hits, capped at
/// [`MAX_SEARCH_REPOS`]. The endpoint is asked for GGUF repos
/// sorted by downloads; the cap is also enforced here so a
/// response can never widen the pre-pass.
pub fn parse_search_results(body: &str) -> Result<Vec<HfSearchHit>, DiscoverError> {
    parse_search_results_capped(body, MAX_SEARCH_REPOS)
}

/// [`parse_search_results`] with a caller-chosen cap: the
/// consultation pre-pass keeps its depth of
/// [`MAX_SEARCH_REPOS`]; the form lookup (P2-D2) asks for
/// [`MAX_LOOKUP_REPOS`](super::MAX_LOOKUP_REPOS). The cap is enforced here as well as
/// in the request, so a response can never widen a surface
/// past the depth it was designed for.
pub fn parse_search_results_capped(
    body: &str,
    cap: usize,
) -> Result<Vec<HfSearchHit>, DiscoverError> {
    let mut hits: Vec<HfSearchHit> = serde_json::from_str(body)
        .map_err(|e| DiscoverError::Parse(format!("search response: {e}")))?;
    hits.truncate(cap);
    Ok(hits)
}

/// One sibling file of a model record, with its exact size:
/// the sibling's own `size` (present with `?blobs=true`), or
/// the LFS object's size when only that is reported. A sibling
/// with no size anywhere parses as 0 and is dropped by the
/// quant grouping — an unknown size is not a candidate fact.
#[derive(Debug, Clone, PartialEq)]
pub struct HfSibling {
    pub rfilename: String,
    pub size_bytes: u64,
    /// The LFS object's SHA-256 when HF reports one
    /// (sibling `lfs.sha256`, present with `?blobs=true`):
    /// the per-file hash available *before* any download,
    /// which is what lets an HF-picked add start pinned
    /// (design Part 2, P2-D6).
    pub sha256: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawLfs {
    size: Option<u64>,
    sha256: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawSibling {
    rfilename: String,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    lfs: Option<RawLfs>,
}

/// The `gguf` block of a model record (§A.6): total parameter
/// count, architecture, rated context length, and the total
/// file size of the repo's primary GGUF (`totalFileSize`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
struct RawGguf {
    #[serde(default)]
    architecture: Option<String>,
    #[serde(default)]
    total: Option<u64>,
    #[serde(default)]
    context_length: Option<u64>,
    #[serde(default, rename = "totalFileSize")]
    total_file_size: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RawModelRecord {
    id: String,
    #[serde(default)]
    downloads: u64,
    #[serde(default)]
    likes: u64,
    #[serde(default, rename = "lastModified")]
    last_modified: Option<String>,
    #[serde(default)]
    gguf: Option<RawGguf>,
    #[serde(default)]
    siblings: Vec<RawSibling>,
}

/// The facts one model record carries, before quant grouping
/// and fit: everything the `gguf` block and the sibling list
/// report, in the app's units.
#[derive(Debug, Clone, PartialEq)]
pub struct HfModelFacts {
    pub repo_id: String,
    pub downloads: u64,
    pub likes: u64,
    /// The record's `lastModified` (RFC 3339), when HF
    /// reports it: the recency input to shortlist ranking
    /// (§P1.8) for repos whose record was fetched.
    pub last_modified: Option<String>,
    pub architecture: Option<String>,
    /// Total parameter count (not billions).
    pub params_total: Option<u64>,
    pub ctx_max: Option<u32>,
    pub total_file_size: Option<u64>,
    pub siblings: Vec<HfSibling>,
}

/// Parse a model-record response body (`GET /api/models/{id}`,
/// with or without `?blobs=true`).
pub fn parse_model_record(body: &str) -> Result<HfModelFacts, DiscoverError> {
    let raw: RawModelRecord = serde_json::from_str(body)
        .map_err(|e| DiscoverError::Parse(format!("model record: {e}")))?;
    Ok(HfModelFacts {
        repo_id: raw.id,
        downloads: raw.downloads,
        likes: raw.likes,
        last_modified: raw.last_modified,
        architecture: raw.gguf.as_ref().and_then(|g| g.architecture.clone()),
        params_total: raw.gguf.as_ref().and_then(|g| g.total),
        ctx_max: raw
            .gguf
            .as_ref()
            .and_then(|g| g.context_length)
            .and_then(|c| u32::try_from(c).ok()),
        total_file_size: raw.gguf.as_ref().and_then(|g| g.total_file_size),
        siblings: raw
            .siblings
            .into_iter()
            .map(|s| HfSibling {
                size_bytes: s
                    .size
                    .or_else(|| s.lfs.as_ref().and_then(|l| l.size))
                    .unwrap_or(0),
                sha256: s.lfs.as_ref().and_then(|l| l.sha256.clone()),
                rfilename: s.rfilename,
            })
            .collect(),
    })
}

// ---------------------------------------------------------------------------
// Quant grouping
// ---------------------------------------------------------------------------

/// One quant of a repo: a single GGUF file, or one multi-shard
/// series summed into a single entry.
#[derive(Debug, Clone, PartialEq)]
pub struct QuantFile {
    /// The file name, or the series' logical name for shards.
    pub file: String,
    pub size_bytes: u64,
    /// How many sibling files the entry sums (1 for a plain
    /// single-file quant).
    pub shard_count: usize,
    /// The file's LFS SHA-256, for single-file quants only:
    /// one record field cannot name N shard hashes, so a
    /// grouped series carries none (design Part 2, P2-D6).
    pub sha256: Option<String>,
}

/// Parse the shard suffix of a GGUF stem: `…-00001-of-00003`
/// returns the shared stem (`…`). Both numbers must be pure
/// digit runs; anything else is not a shard name.
fn shard_stem(stem: &str) -> Option<&str> {
    let (head, total) = stem.rsplit_once("-of-")?;
    if total.is_empty() || !total.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (base, index) = head.rsplit_once('-')?;
    if index.is_empty() || !index.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(base)
}

/// Group a record's siblings into quant entries (§A.5):
///
/// - only `*.gguf` files (case-insensitive extension);
/// - `mmproj*` projector files and every non-model sibling
///   (READMEs, configs, `.gitattributes`) are excluded;
/// - siblings with no exact size are excluded — the pre-pass
///   deals in real numbers;
/// - a multi-shard series (`-00001-of-0000N.gguf`, …) groups
///   into one entry whose size is the shards' sum and whose
///   file is the logical quant name (shared stem + `.gguf`).
///
/// Entries come out sorted by file name, so the candidate
/// order is deterministic regardless of sibling order.
pub fn quant_files(siblings: &[HfSibling]) -> Vec<QuantFile> {
    let mut groups: BTreeMap<String, (u64, usize, Option<String>)> = BTreeMap::new();
    for s in siblings {
        let name = s.rfilename.rsplit('/').next().unwrap_or(&s.rfilename);
        let lower = name.to_lowercase();
        if !lower.ends_with(".gguf") || lower.contains("mmproj") || s.size_bytes == 0 {
            continue;
        }
        let stem = &s.rfilename[..s.rfilename.len() - ".gguf".len()];
        let key = match shard_stem(stem) {
            Some(base) => format!("{base}.gguf"),
            None => s.rfilename.clone(),
        };
        let entry = groups.entry(key).or_insert((0, 0, None));
        entry.0 = entry.0.saturating_add(s.size_bytes);
        entry.1 += 1;
        // The hash survives only while the group is a single
        // file; the moment a second sibling joins, the entry
        // names a series and no one hash describes it.
        entry.2 = if entry.1 == 1 { s.sha256.clone() } else { None };
    }
    groups
        .into_iter()
        .map(|(file, (size_bytes, shard_count, sha256))| QuantFile {
            file,
            size_bytes,
            shard_count,
            sha256,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discover::test_fixtures::*;
    use crate::discover::*;

    // -- search parsing -----------------------------------------------------

    #[test]
    fn search_results_parse_and_cap_at_eight() {
        let body = serde_json::json!((0..10)
            .map(|i| serde_json::json!({ "id": format!("org/model-{i}"), "downloads": 1000 - i, "likes": i }))
            .collect::<Vec<_>>())
        .to_string();
        let hits = parse_search_results(&body).unwrap();
        assert_eq!(hits.len(), MAX_SEARCH_REPOS);
        assert_eq!(hits[0].id, "org/model-0");
        assert_eq!(hits[0].downloads, 1000);
        assert_eq!(hits[7].likes, 7);
    }

    #[test]
    fn search_results_reject_non_array() {
        assert!(parse_search_results("{\"id\":\"x\"}").is_err());
        assert!(parse_search_results("not json").is_err());
    }

    // -- model record parsing ------------------------------------------------

    #[test]
    fn model_record_parses_gguf_block_and_siblings() {
        let facts = parse_model_record(&record_body()).unwrap();
        assert_eq!(facts.repo_id, "Qwen/Qwen3-8B-GGUF");
        assert_eq!(facts.downloads, 594664);
        assert_eq!(facts.architecture.as_deref(), Some("qwen3"));
        assert_eq!(facts.params_total, Some(8190735360));
        assert_eq!(facts.ctx_max, Some(40960));
        assert_eq!(facts.total_file_size, Some(5027783488));
        assert_eq!(facts.siblings.len(), 5);
        // The per-file LFS hash is carried through (P2-D6):
        // present pre-download, it is what pins a picked add.
        assert_eq!(facts.siblings[1].sha256.as_deref(), Some("d98c"));
        assert_eq!(facts.siblings[2].sha256.as_deref(), Some("408b"));
        assert_eq!(facts.siblings[0].sha256, None);
    }

    #[test]
    fn search_results_capped_at_the_lookup_depth() {
        let body = serde_json::json!((0..30)
            .map(|i| serde_json::json!({ "id": format!("org/model-{i}"), "downloads": 1000 - i }))
            .collect::<Vec<_>>())
        .to_string();
        let hits = parse_search_results_capped(&body, MAX_LOOKUP_REPOS).unwrap();
        assert_eq!(hits.len(), MAX_LOOKUP_REPOS);
        // The discovery cap is unaffected by the capped variant.
        assert_eq!(parse_search_results(&body).unwrap().len(), MAX_SEARCH_REPOS);
    }

    #[test]
    fn model_record_falls_back_to_lfs_size() {
        let body = serde_json::json!({
            "id": "org/m",
            "siblings": [
                { "rfilename": "m-Q4.gguf", "lfs": { "size": 12345u64 } },
                { "rfilename": "m-Q5.gguf" },
            ],
        })
        .to_string();
        let facts = parse_model_record(&body).unwrap();
        assert_eq!(facts.siblings[0].size_bytes, 12345);
        assert_eq!(facts.siblings[1].size_bytes, 0);
        // No gguf block at all is a parseable record with no facts.
        assert!(facts.architecture.is_none());
        assert!(facts.params_total.is_none());
    }

    // -- quant grouping -------------------------------------------------------

    fn sibling(rfilename: &str, size_bytes: u64) -> HfSibling {
        HfSibling {
            rfilename: rfilename.to_string(),
            size_bytes,
            sha256: None,
        }
    }

    fn sibling_sha(rfilename: &str, size_bytes: u64, sha256: &str) -> HfSibling {
        HfSibling {
            rfilename: rfilename.to_string(),
            size_bytes,
            sha256: Some(sha256.to_string()),
        }
    }

    #[test]
    fn quant_files_exclude_mmproj_and_non_models_and_unknown_sizes() {
        let quants = quant_files(&[
            sibling("model-Q4_K_M.gguf", 5_000_000_000),
            sibling("mmproj-model-f16.gguf", 600_000_000),
            sibling("README.md", 10_000),
            sibling(".gitattributes", 100),
            sibling("model-Q5_K_M.gguf", 0),
        ]);
        assert_eq!(quants.len(), 1);
        assert_eq!(quants[0].file, "model-Q4_K_M.gguf");
        assert_eq!(quants[0].shard_count, 1);
    }

    #[test]
    fn quant_files_group_shards_with_summed_size_and_logical_name() {
        let quants = quant_files(&[
            sibling("big-Q4_K_M-00002-of-00003.gguf", 2_000_000_000),
            sibling("big-Q4_K_M-00001-of-00003.gguf", 2_500_000_000),
            sibling("big-Q4_K_M-00003-of-00003.gguf", 1_500_000_000),
            sibling("big-Q8_0.gguf", 9_000_000_000),
        ]);
        assert_eq!(quants.len(), 2);
        // Sorted by file name: the shard series' logical name first.
        assert_eq!(quants[0].file, "big-Q4_K_M.gguf");
        assert_eq!(quants[0].size_bytes, 6_000_000_000);
        assert_eq!(quants[0].shard_count, 3);
        assert_eq!(quants[1].file, "big-Q8_0.gguf");
        assert_eq!(quants[1].shard_count, 1);
    }

    #[test]
    fn quant_files_keep_subdirectory_paths() {
        let quants = quant_files(&[sibling("q4/model-Q4.gguf", 100)]);
        assert_eq!(quants[0].file, "q4/model-Q4.gguf");
    }

    #[test]
    fn quant_files_carry_the_single_file_hash_only() {
        let quants = quant_files(&[
            sibling_sha("one-Q4_K_M.gguf", 100, "aaa"),
            sibling_sha("big-Q8_0-00001-of-00002.gguf", 100, "bbb"),
            sibling_sha("big-Q8_0-00002-of-00002.gguf", 100, "ccc"),
        ]);
        assert_eq!(quants.len(), 2);
        // Single file: the sibling's hash rides along (P2-D6).
        let one = quants.iter().find(|q| q.file == "one-Q4_K_M.gguf").unwrap();
        assert_eq!(one.sha256.as_deref(), Some("aaa"));
        // Shard series: one field cannot name two hashes.
        let big = quants.iter().find(|q| q.file == "big-Q8_0.gguf").unwrap();
        assert_eq!(big.shard_count, 2);
        assert_eq!(big.sha256, None);
    }

    #[test]
    fn search_hits_parse_last_modified_from_full_responses() {
        let body = serde_json::json!([
            { "id": "org/a", "downloads": 10, "lastModified": "2026-09-01T00:00:00.000Z" },
            { "id": "org/b", "downloads": 9 },
        ])
        .to_string();
        let hits = parse_search_results_capped(&body, SEARCH_PAGE_LIMIT).unwrap();
        assert_eq!(
            hits[0].last_modified.as_deref(),
            Some("2026-09-01T00:00:00.000Z")
        );
        assert_eq!(hits[1].last_modified, None);
    }
}
