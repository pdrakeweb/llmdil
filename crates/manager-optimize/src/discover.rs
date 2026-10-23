//! Hugging Face discovery pre-pass (design addendum A.5 option
//! (b), tracker #28 Phase 5c): when a consultation carries an
//! operator directive, the app — not the consulted model —
//! gathers candidate models *before* the consultation.
//!
//! The flow is deterministic and app-side: an HF API search
//! seeded from the directive (`GET /api/models?search=…
//! &filter=gguf&sort=downloads`, §A.6), then for each of the top
//! repos the model record (`GET /api/models/{id}?blobs=true`)
//! with its `gguf` block (architecture, total parameter count,
//! rated context length) and every sibling file's exact size.
//! Each quant file becomes one [`CandidateFact`], with fit
//! computed locally through the existing evaluator
//! ([`crate::fit::layout_requirement`] + [`crate::fit::evaluate_fit`])
//! against a caller-supplied free-VRAM snapshot. The facts ride
//! into the consultation as the context document's `candidates`
//! block — distinct from the curated `catalog_candidates` —
//! carrying provenance per entry (HF repo id, retrieved-at).
//!
//! The pass the op runs is the deep pool
//! ([`HfClient::discover_deep`], deep-discovery design Part 1):
//! conjunctive-search query variants merged into a 600-repo
//! pool, search-layer and record-layer filters counted in a
//! [`DiscoveryFunnel`], quant collapsing to ≤3 per repo, and a
//! lexicographically ranked shortlist capped at 30. The shallow
//! single-search [`HfClient::discover`] remains for the
//! original §A.5 shape.
//!
//! No auth: public models only. No tool loop, no web search.
//! The HTTP layer is thin on purpose; the parsers are pure
//! functions over the response text so the risk-bearing logic
//! (field shapes, quant grouping, fit verdicts, query seeding)
//! is tested without any network.

use std::collections::BTreeMap;
use std::time::Duration;

use manager_models::{ExpertOffload, GgufError, MemoryBasis};
use serde::{Deserialize, Serialize};

use crate::fit::{evaluate_fit, layout_requirement, LayoutFacts};

/// Default Hugging Face origin; the client takes any base URL
/// so tests can point it at a local mock server.
pub const HF_DEFAULT_BASE_URL: &str = "https://huggingface.co";

/// Candidate cap: the top repos from one search (§A.5's "top
/// candidates"). Every quant file of a kept repo becomes a
/// candidate — per-repo quant count is not capped, because the
/// fit verdict differs per quant and that difference is the
/// point of the pre-pass.
pub const MAX_SEARCH_REPOS: usize = 8;

/// Cap on the directive-derived search terms (see
/// [`search_query_for_directive`]).
pub const MAX_QUERY_TERMS: usize = 4;

// ---------------------------------------------------------------------------
// Deep discovery pool (deep-discovery design Part 1, D1–D10)
// ---------------------------------------------------------------------------

/// Unique-repo cap on the merged search pool (D1): variant
/// results merge by repo id up to this many repos, ordered
/// by downloads; that order drives record fetching.
pub const MAX_POOL_REPOS: usize = 600;

/// Record-fetch depth (D1): at most this many repos have
/// their model record fetched in one deep pass. The request
/// budget is the binding constraint — worst case 12 search
/// requests + 200 records ≈ 42% of HF's anonymous
/// 500-per-5-minutes window (D10: no HF token).
pub const MAX_RECORD_FETCHES: usize = 200;

/// Cap on the query-variant set (D2): the joined query, one
/// query per term, and the seat-family query.
pub const MAX_QUERY_VARIANTS: usize = 6;

/// Search page size for the deep pass (D3): `limit=100`
/// pages followed via the `Link` cursor, at most
/// [`MAX_PAGES_PER_VARIANT`] pages per variant — incremental
/// pages are what make the early exit possible.
pub const SEARCH_PAGE_LIMIT: usize = 100;
pub const MAX_PAGES_PER_VARIANT: usize = 2;

/// Record-fetch concurrency (D4): a `JoinSet` gated by a
/// semaphore at this width.
pub const DISCOVERY_CONCURRENCY: usize = 8;

/// Per-request timeout for the deep pass (D4), down from the
/// client's 30 s interactive default: at depth, a 30 s
/// straggler under a semaphore slot is a real budget cost.
pub const DISCOVERY_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Whole-pass deadline (D4): on expiry, fetching stops and
/// the partial result is stored (D5).
pub const DISCOVERY_DEADLINE: Duration = Duration::from_secs(120);

/// Search-layer popularity floor (D6): a repo below this
/// many downloads carries no usage signal.
pub const MIN_POOL_DOWNLOADS: u64 = 1_000;

/// Search-layer staleness rule (D6): `lastModified` older
/// than this many days **and** downloads below
/// [`STALE_DOWNLOADS`] — both conditions, so a two-year-old
/// repo with a large audience stays a candidate.
pub const STALE_DAYS: i64 = 730;
pub const STALE_DOWNLOADS: u64 = 10_000;

/// Per-repo candidate cap in the ranking pool and in the
/// admitted shortlist (D7/D8): the largest `fits`, the
/// largest `fits_with_split`, and the smallest overall.
pub const MAX_CANDIDATES_PER_REPO: usize = 3;

/// Shortlist cap (D8): the bound on the stored list, the
/// prune surface, and the consultation candidates block
/// (~60 tokens per entry ⇒ ≈1,800 tokens at the cap).
pub const SHORTLIST_CAP: usize = 30;

/// Minimum distinct repos in a full shortlist for the early
/// exit (§P1.9): a cap-full shortlist of checked fits from
/// fewer repos is not yet a real choice.
pub const EARLY_EXIT_MIN_REPOS: usize = 10;

/// Model-family vocabulary for the record-layer family
/// filter (D6): when a query term names one of these, a
/// record whose `gguf.architecture` starts with none of the
/// named tokens is dropped.
pub const FAMILY_TOKENS: &[&str] = &[
    "qwen",
    "llama",
    "mistral",
    "mixtral",
    "gemma",
    "phi",
    "deepseek",
    "nemotron",
    "falcon",
    "yi",
    "glm",
    "internlm",
];

/// Repo cap for the Add Model form's shallow lookup
/// (deep-discovery design Part 2, P2-D2): one search at this
/// depth plus one record fetch per repo — no pagination, no
/// pool. The form's type-ahead uses the same search at the
/// same depth.
pub const MAX_LOOKUP_REPOS: usize = 25;

/// First prefix size the GGUF header range read asks for, and
/// the cap the extension ladder stops at (deep-discovery
/// design Part 2, P2-D4). The cap is set from measurement:
/// every curated catalog file's header — metadata plus the
/// full tensor directory, the bytes
/// [`manager_models::parse_gguf_layout`] consumes — was
/// range-read on 2026-10-22 and lands between 5.8 and 11.0
/// MiB (largest: Qwen3.8-27B-GSQ-RCO-IQ3_S-mtp.gguf at
/// 10,995,772 bytes). 16 MiB covers the catalog with ~45%
/// headroom; the design records the constant as a proposal
/// to be set from exactly this evidence.
pub const HF_HEADER_RANGE_INITIAL_BYTES: u64 = 4 * 1024 * 1024;
pub const HF_HEADER_RANGE_CAP_BYTES: u64 = 16 * 1024 * 1024;

/// Fit verdict tokens carried on a [`CandidateFact`]. Stable
/// strings: the context block, the UI's labels, and later
/// sub-phases key off them.
pub const FIT_FITS: &str = "fits";
pub const FIT_FITS_WITH_SPLIT: &str = "fits_with_split";
pub const FIT_DOES_NOT_FIT: &str = "does_not_fit";
pub const FIT_UNCHECKED: &str = "unchecked";

/// One discovered candidate: one quant file of one HF repo,
/// with the facts HF reported and the app's own fit verdict.
/// This is the shape the context block carries (§A.5) and the
/// shape persisted in `<data_dir>/optimize-candidates.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateFact {
    /// Provenance: the Hugging Face repository id.
    pub repo_id: String,
    /// The quant file's name. For a multi-shard quant this is
    /// the logical quant name — the shard series' shared stem
    /// plus `.gguf` (see [`quant_files`]) — not a literal
    /// sibling path; the shards are one quant, not several.
    pub file: String,
    /// GGUF architecture from the record's `gguf` block
    /// ("qwen3", "llama", …); empty when HF reports none.
    pub architecture: String,
    /// Total parameters in billions, from the `gguf` block's
    /// total count; 0 when unknown.
    pub params_b: f32,
    /// Rated maximum context from the `gguf` block; 0 when
    /// unknown (the catalog's convention).
    pub ctx_max: u32,
    /// Exact size in bytes: the quant file's sibling size, or
    /// the summed shard sizes for a multi-shard quant.
    pub size_bytes: u64,
    /// The repo's lifetime download count at retrieval.
    pub downloads: u64,
    /// The app's fit verdict against the free-VRAM snapshot
    /// the discovery ran with: one of the `FIT_*` tokens.
    pub fit: String,
    /// Provenance: when the facts were retrieved (RFC 3339),
    /// supplied by the caller so a whole pre-pass shares one
    /// timestamp.
    pub retrieved_at: String,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum DiscoverError {
    #[error("Hugging Face request failed: {0}")]
    Http(String),
    #[error("could not parse Hugging Face response: {0}")]
    Parse(String),
    /// HTTP 429, or a `RateLimit` header reporting no
    /// requests remaining: the deep pass stops fetching and
    /// stores its partial result (D5) instead of failing or
    /// sleeping inside the op.
    #[error("Hugging Face rate limit reached")]
    RateLimited,
}

/// The `rel="next"` target of an RFC 8288 `Link` header:
/// the opaque-cursor URL the next search page is fetched
/// from (§P1.3, D3). Header shape:
/// `<url>; rel="next", <url>; rel="last", …`.
pub fn parse_link_next(link_header: Option<&str>) -> Option<String> {
    let header = link_header?;
    for part in header.split(',') {
        let part = part.trim();
        let Some(rest) = part.strip_prefix('<') else {
            continue;
        };
        let Some((url, params)) = rest.split_once('>') else {
            continue;
        };
        if params.contains("rel=\"next\"") {
            return Some(url.to_string());
        }
    }
    None
}

/// The remaining-requests figure of a `RateLimit` header
/// (`"api";r=<remaining>;t=<seconds to reset>`, §P1.3).
/// `Some(0)` is the deep pass's stop signal (§P1.5).
pub fn parse_rate_limit_remaining(rate_limit_header: Option<&str>) -> Option<u64> {
    let header = rate_limit_header?;
    for field in header.split(';') {
        if let Some(value) = field.trim().strip_prefix("r=") {
            return value.trim().trim_matches('"').parse().ok();
        }
    }
    None
}

/// The accounting of one deep discovery pass (§P1.10): every
/// drop is counted, nothing is dropped silently. Stored
/// beside the shortlist in `optimize-candidates.json` and
/// exposed in the candidates payload; the Optimize page
/// renders its summary (`Searched / Filtered / Kept`) with
/// the full counter set in the line's tooltip. Counts report
/// what was fetched only — HF sends no total match count,
/// so no "of N total" figure exists to show.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscoveryFunnel {
    /// Query variants actually run (of the derived set).
    pub queries_run: usize,
    /// Unique repos merged into the pool (cap
    /// [`MAX_POOL_REPOS`]).
    pub pool_repos: usize,
    /// Repos whose model record was fetched successfully.
    pub repos_fetched: usize,
    /// Repos whose record fetch failed (skipped).
    pub repos_failed: usize,
    /// Search-layer drops: downloads below
    /// [`MIN_POOL_DOWNLOADS`].
    pub dropped_unpopular: usize,
    /// Search-layer drops: stale by the §P1.6 rule.
    pub dropped_stale: usize,
    /// Record-layer drops: architecture contradicts a
    /// family term the directive named.
    pub dropped_family: usize,
    /// Candidate drops: fit verdict `does_not_fit`, counted
    /// per candidate after quant grouping (D8).
    pub dropped_fit: usize,
    /// Candidates produced by quant grouping across all
    /// fetched records, before fit filtering and collapsing
    /// (`does_not_fit` counted in `dropped_fit`).
    pub quants_considered: usize,
    /// Candidates removed by the ≤3-per-repo collapse (D7).
    pub quants_collapsed: usize,
    /// Candidates admitted to the shortlist (D8).
    pub kept: usize,
    /// The pass stopped before natural completion
    /// (deadline or rate limit); the stored result is
    /// partial (D5).
    pub partial: bool,
    /// Why the pass is partial: `"deadline"` or
    /// `"rate_limited"`; `None` on a complete pass.
    pub partial_reason: Option<String>,
    /// Fetching stopped early because the shortlist was
    /// full of checked fits from enough distinct repos
    /// (§P1.9).
    pub stopped_early: bool,
}

impl Default for DiscoveryFunnel {
    fn default() -> Self {
        Self {
            queries_run: 0,
            pool_repos: 0,
            repos_fetched: 0,
            repos_failed: 0,
            dropped_unpopular: 0,
            dropped_stale: 0,
            dropped_family: 0,
            dropped_fit: 0,
            quants_considered: 0,
            quants_collapsed: 0,
            kept: 0,
            partial: false,
            partial_reason: None,
            stopped_early: false,
        }
    }
}

/// The result of one deep discovery pass: the ranked
/// shortlist plus its funnel.
#[derive(Debug, Clone, PartialEq)]
pub struct DeepDiscovery {
    pub candidates: Vec<CandidateFact>,
    pub funnel: DiscoveryFunnel,
}

// ---------------------------------------------------------------------------
// HF response types + pure parsers
// ---------------------------------------------------------------------------

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
/// [`MAX_LOOKUP_REPOS`]. The cap is enforced here as well as
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
        entry.2 = if entry.1 == 1 {
            s.sha256.clone()
        } else {
            None
        };
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

// ---------------------------------------------------------------------------
// Fit
// ---------------------------------------------------------------------------

/// The fit verdict for one candidate size against a free-VRAM
/// snapshot (per GPU, MiB, GPU-index order) — the existing
/// evaluator over a [`LayoutFacts`] built from HF facts: the
/// quant file's exact size as the total, no expert/KV geometry
/// (HF does not report it), and an `Estimate` basis label —
/// the honest one, because a weights-only requirement with
/// unknown KV is an estimate-grade figure even though the
/// file size itself is exact. The snapshot is the telemetry
/// free VRAM, which already excludes running backends'
/// residency, so no other residency is subtracted here.
pub fn fit_verdict(
    size_bytes: u64,
    ctx_max: u32,
    params_b: f32,
    free_vram_mib_per_gpu: &[u64],
) -> String {
    if free_vram_mib_per_gpu.is_empty() {
        return FIT_UNCHECKED.to_string();
    }
    let facts = LayoutFacts {
        total_bytes: size_bytes,
        expert_bytes: None,
        expert_bytes_by_block: None,
        moe_layers: None,
        kv_per_token: None,
        kv_geometry: None,
        rs_bytes_per_seq: None,
        basis: MemoryBasis::Estimate,
        ctx_max,
        params_b,
    };
    // No offload is proposed for a candidate, and with no KV or
    // recurrent-state geometry the requirement is weights-only
    // at any context and any slot count.
    let req = layout_requirement(&facts, ExpertOffload::None, manager_models::CacheSelection::default(), 0, 1, 0);
    let zeros = vec![0u64; free_vram_mib_per_gpu.len()];
    let outcome = evaluate_fit(&req, &[], free_vram_mib_per_gpu, &zeros, &zeros);
    if !outcome.placeable {
        return FIT_DOES_NOT_FIT.to_string();
    }
    let total_mib = crate::fit::gpu_total_mib(&req);
    if free_vram_mib_per_gpu.iter().any(|&f| f >= total_mib) {
        FIT_FITS.to_string()
    } else {
        FIT_FITS_WITH_SPLIT.to_string()
    }
}

/// Assemble one repo's candidate facts from its model record:
/// one entry per quant file, each with the record's shared
/// facts and its own fit verdict.
pub fn candidates_from_model(
    facts: &HfModelFacts,
    free_vram_mib_per_gpu: &[u64],
    retrieved_at: &str,
) -> Vec<CandidateFact> {
    let params_b = facts
        .params_total
        .map(|t| t as f32 / 1_000_000_000.0)
        .unwrap_or(0.0);
    let ctx_max = facts.ctx_max.unwrap_or(0);
    quant_files(&facts.siblings)
        .into_iter()
        .map(|q| CandidateFact {
            repo_id: facts.repo_id.clone(),
            fit: fit_verdict(q.size_bytes, ctx_max, params_b, free_vram_mib_per_gpu),
            file: q.file,
            architecture: facts.architecture.clone().unwrap_or_default(),
            params_b,
            ctx_max,
            size_bytes: q.size_bytes,
            downloads: facts.downloads,
            retrieved_at: retrieved_at.to_string(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Deep pool: merge, filters, quant collapsing, ranking (Part 1)
// ---------------------------------------------------------------------------

/// One repo in the merged pool: the best facts the search
/// layer saw for it across every variant and page.
#[derive(Debug, Clone, PartialEq)]
pub struct PoolRepo {
    pub repo_id: String,
    pub downloads: u64,
    pub last_modified: Option<String>,
}

/// Merge search hits into the pool (§P1.4): a repo hit by
/// several variants is kept once, with the maximum
/// `downloads` / `lastModified` seen. Insertion order is
/// preserved for repos already present; the pool order the
/// fetcher drives off is [`pool_order`] (downloads
/// descending). The caller enforces [`MAX_POOL_REPOS`] by
/// not merging past the cap.
pub fn merge_pool_hits(pool: &mut Vec<PoolRepo>, hits: &[HfSearchHit]) {
    for hit in hits {
        if let Some(existing) = pool.iter_mut().find(|p| p.repo_id == hit.id) {
            existing.downloads = existing.downloads.max(hit.downloads);
            existing.last_modified = match (&existing.last_modified, &hit.last_modified) {
                (Some(a), Some(b)) => Some(a.max(b).clone()),
                (Some(a), None) => Some(a.clone()),
                (None, b) => b.clone(),
            };
        } else if pool.len() < MAX_POOL_REPOS {
            pool.push(PoolRepo {
                repo_id: hit.id.clone(),
                downloads: hit.downloads,
                last_modified: hit.last_modified.clone(),
            });
        }
    }
}

/// The pool in fetch order: downloads descending, repo id
/// ascending on ties (deterministic from the same pool).
pub fn pool_order(pool: &[PoolRepo]) -> Vec<PoolRepo> {
    let mut ordered = pool.to_vec();
    ordered.sort_by(|a, b| {
        b.downloads
            .cmp(&a.downloads)
            .then_with(|| a.repo_id.cmp(&b.repo_id))
    });
    ordered
}

/// Parse an RFC 3339 timestamp to Unix seconds for the
/// staleness and recency comparisons. Unparseable input is
/// `None` — a repo whose recency cannot be read is never
/// dropped as stale on a guess.
fn rfc3339_unix(text: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|dt| dt.timestamp())
}

/// The §P1.6 staleness rule: `lastModified` older than
/// [`STALE_DAYS`] **and** downloads below
/// [`STALE_DOWNLOADS`], judged against `now_unix`.
pub fn is_stale(downloads: u64, last_modified: Option<&str>, now_unix: i64) -> bool {
    if downloads >= STALE_DOWNLOADS {
        return false;
    }
    match last_modified.and_then(rfc3339_unix) {
        Some(ts) => now_unix - ts > STALE_DAYS * 86_400,
        None => false,
    }
}

/// Search-layer filtering (§P1.6, steps 1–2), in listed
/// order so each drop is counted once: below the downloads
/// floor is `dropped_unpopular`; the staleness rule is
/// `dropped_stale`. Returns the kept repos in input order.
pub fn search_layer_filter(
    repos: &[PoolRepo],
    now_unix: i64,
    funnel: &mut DiscoveryFunnel,
) -> Vec<PoolRepo> {
    let mut kept = Vec::new();
    for repo in repos {
        if repo.downloads < MIN_POOL_DOWNLOADS {
            funnel.dropped_unpopular += 1;
        } else if is_stale(repo.downloads, repo.last_modified.as_deref(), now_unix) {
            funnel.dropped_stale += 1;
        } else {
            kept.push(repo.clone());
        }
    }
    kept
}

/// The family tokens a directive's terms name (§P1.6 step
/// 3): a term matches a token exactly, or as a prefix
/// followed by a digit — the versioned family names
/// (`qwen3`, `llama3`, `gemma2`, `phi4`, `glm4`) name the
/// family the vocabulary lists as `qwen`, `llama`, … .
/// (Interpretation, named: the design says "any query term
/// is a known family token"; strict equality would leave
/// the filter inert for the versioned names directives
/// actually use.)
pub fn family_tokens_for_terms(terms: &[String]) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for token in FAMILY_TOKENS {
        let named = terms.iter().any(|t| {
            t == token
                || t.strip_prefix(token)
                    .is_some_and(|rest| rest.bytes().next().is_some_and(|b| b.is_ascii_digit()))
        });
        if named {
            out.push(token);
        }
    }
    out
}

/// The record-layer family rule (§P1.6 step 3): with no
/// family token named, every record is kept; a record with
/// no `gguf` architecture has nothing to contradict the
/// directive and is kept; otherwise the architecture must
/// start with one of the named tokens.
pub fn family_filter_keeps(architecture: Option<&str>, family_tokens: &[&str]) -> bool {
    if family_tokens.is_empty() {
        return true;
    }
    match architecture {
        None => true,
        Some(arch) => {
            let arch = arch.to_lowercase();
            family_tokens.iter().any(|t| arch.starts_with(t))
        }
    }
}

/// Quant collapsing (D7): per repo, at most
/// [`MAX_CANDIDATES_PER_REPO`] candidates enter the ranking
/// pool — the largest-size candidate with fit `fits`, the
/// largest-size candidate with fit `fits_with_split`, and
/// the smallest-size candidate overall — deduplicated when
/// one candidate fills two slots. Returns the collapsed
/// pool and the number of candidates removed.
pub fn collapse_candidates(candidates: &[CandidateFact]) -> (Vec<CandidateFact>, usize) {
    let mut by_repo: BTreeMap<&str, Vec<&CandidateFact>> = BTreeMap::new();
    for c in candidates {
        by_repo.entry(c.repo_id.as_str()).or_default().push(c);
    }
    fn pick<'a>(picked: &mut Vec<&'a CandidateFact>, c: Option<&'a CandidateFact>) {
        if let Some(c) = c {
            if !picked.iter().any(|p| p.file == c.file) {
                picked.push(c);
            }
        }
    }
    let mut out: Vec<CandidateFact> = Vec::new();
    for (_, repo_candidates) in by_repo {
        let mut picked: Vec<&CandidateFact> = Vec::new();
        pick(
            &mut picked,
            repo_candidates
                .iter()
                .filter(|c| c.fit == FIT_FITS)
                .max_by_key(|c| c.size_bytes)
                .copied(),
        );
        pick(
            &mut picked,
            repo_candidates
                .iter()
                .filter(|c| c.fit == FIT_FITS_WITH_SPLIT)
                .max_by_key(|c| c.size_bytes)
                .copied(),
        );
        pick(
            &mut picked,
            repo_candidates.iter().min_by_key(|c| c.size_bytes).copied(),
        );
        out.extend(picked.into_iter().cloned());
    }
    let removed = candidates.len() - out.len();
    (out, removed)
}

/// Fit-tier rank for the lexicographic shortlist order
/// (§P1.8): `fits` < `fits_with_split` < `unchecked`.
/// `does_not_fit` never reaches ranking (D8 drops it).
fn fit_tier(fit: &str) -> u8 {
    match fit {
        FIT_FITS => 0,
        FIT_FITS_WITH_SPLIT => 1,
        _ => 2,
    }
}

/// Rank the collapsed pool and admit the shortlist (§P1.8):
/// lexicographic over (fit tier, downloads descending,
/// `lastModified` descending, repo id ascending, size
/// descending) — a deterministic total order, so a pass is
/// reproducible from the same responses. Admission walks
/// the ranked list, admitting a candidate while its repo
/// has fewer than [`MAX_CANDIDATES_PER_REPO`] admitted and
/// the shortlist has fewer than [`SHORTLIST_CAP`].
/// `last_modified_for` supplies each repo's recency (the
/// record's, else the pool hit's).
pub fn rank_and_admit(
    collapsed: &[CandidateFact],
    last_modified_for: &dyn Fn(&str) -> Option<String>,
) -> Vec<CandidateFact> {
    let mut ranked: Vec<(CandidateFact, Option<i64>)> = collapsed
        .iter()
        .map(|c| {
            let lm = last_modified_for(&c.repo_id);
            (c.clone(), lm.as_deref().and_then(rfc3339_unix))
        })
        .collect();
    ranked.sort_by(|(a, a_lm), (b, b_lm)| {
        fit_tier(&a.fit)
            .cmp(&fit_tier(&b.fit))
            .then_with(|| b.downloads.cmp(&a.downloads))
            .then_with(|| b_lm.cmp(a_lm))
            .then_with(|| a.repo_id.cmp(&b.repo_id))
            .then_with(|| b.size_bytes.cmp(&a.size_bytes))
    });
    let mut admitted: Vec<CandidateFact> = Vec::new();
    let mut per_repo: BTreeMap<String, usize> = BTreeMap::new();
    for (candidate, _) in ranked {
        if admitted.len() >= SHORTLIST_CAP {
            break;
        }
        let count = per_repo.entry(candidate.repo_id.clone()).or_insert(0);
        if *count >= MAX_CANDIDATES_PER_REPO {
            continue;
        }
        *count += 1;
        admitted.push(candidate);
    }
    admitted
}

/// The §P1.9 early-exit predicate over an admitted
/// shortlist: full ([`SHORTLIST_CAP`]), every candidate
/// checked (`fits` / `fits_with_split` — `unchecked` never
/// triggers the exit), from at least [`EARLY_EXIT_MIN_REPOS`]
/// distinct repos.
pub fn shortlist_is_full(shortlist: &[CandidateFact]) -> bool {
    if shortlist.len() < SHORTLIST_CAP {
        return false;
    }
    if !shortlist
        .iter()
        .all(|c| c.fit == FIT_FITS || c.fit == FIT_FITS_WITH_SPLIT)
    {
        return false;
    }
    let repos: std::collections::BTreeSet<&str> =
        shortlist.iter().map(|c| c.repo_id.as_str()).collect();
    repos.len() >= EARLY_EXIT_MIN_REPOS
}

// ---------------------------------------------------------------------------
// Form lookup + prefill mapping (deep-discovery design Part 2)
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Search seeding: the directive -> HF search query extraction
// ---------------------------------------------------------------------------

/// Words that carry intent but no model identity: articles and
/// prepositions, the verbs/adjectives a directive uses to ask
/// for an improvement, and generic model nouns. Dropping them
/// leaves the terms HF can match against repo names.
const STOPWORDS: &[&str] = &[
    "a",
    "an",
    "the",
    "and",
    "or",
    "for",
    "to",
    "of",
    "in",
    "on",
    "with",
    "at",
    "by",
    "from",
    "i",
    "we",
    "my",
    "our",
    "it",
    "its",
    "this",
    "that",
    "explore",
    "exploring",
    "better",
    "best",
    "improve",
    "improves",
    "improving",
    "improvement",
    "reduce",
    "reduces",
    "reducing",
    "reduction",
    "find",
    "finding",
    "look",
    "looking",
    "want",
    "wants",
    "need",
    "needs",
    "please",
    "make",
    "makes",
    "model",
    "models",
    "llm",
    "capable",
    "capability",
    "capabilities",
    "faster",
    "fast",
    "slow",
    "slower",
    "latency",
    "speed",
    "feels",
    "feel",
    "new",
    "alternative",
    "alternatives",
    "replacement",
    "replace",
    "use",
    "using",
    "try",
    "get",
    "more",
    "much",
    "really",
    "very",
    "just",
    "seat",
    "role",
];

/// Seat role words: a directive names the seat it scopes to
/// ("…for the planner"), but the seat's name is not a model
/// term, so the words are dropped from the query and the seat
/// is reported separately by [`seat_for_directive`].
const SEAT_WORDS: &[&str] = &["planner", "planning", "coder", "coding", "tool", "runner"];

/// The seat a directive names, if any: "planner",
/// "coder", or "tool-runner". Detection is a plain substring
/// check on the lowercased directive (tool-runner first, its
/// spellings contain "tool" alone otherwise).
pub fn seat_for_directive(directive: &str) -> Option<&'static str> {
    let lower = directive.to_lowercase();
    if lower.contains("tool-runner")
        || lower.contains("tool runner")
        || lower.contains("toolrunner")
    {
        Some("tool-runner")
    } else if lower.contains("planner") || lower.contains("planning") {
        Some("planner")
    } else if lower.contains("coder") || lower.contains("coding") {
        Some("coder")
    } else {
        None
    }
}

/// The model-family term a seat contributes to the query when
/// the directive's own terms do not already carry it. The
/// planner seat has none — planning is a function many model
/// families serve, and "planner" matches almost no repo — so
/// a planner directive searches on its own salient terms.
/// Coder repos carry "coder" in their names; tool-runner
/// seats run general instruct models.
fn seat_family_term(seat: Option<&str>) -> Option<&'static str> {
    match seat {
        Some("coder") => Some("coder"),
        Some("tool-runner") => Some("instruct"),
        _ => None,
    }
}

/// The salient terms of a directive (steps 1–3 of
/// [`search_query_for_directive`]'s rule), returned as a
/// list: the deep pass's variant builder (§P1.4) needs the
/// terms themselves, not only their join. The seat's family
/// term is *not* part of this list — it is a query-level
/// addition, applied by the query builders.
pub fn search_terms_for_directive(directive: &str) -> Vec<String> {
    let lower = directive.to_lowercase();
    let mut terms: Vec<String> = Vec::new();
    for token in lower.split(|c: char| !c.is_alphanumeric()) {
        if token.is_empty()
            || STOPWORDS.contains(&token)
            || SEAT_WORDS.contains(&token)
            || terms.iter().any(|t| t == token)
        {
            continue;
        }
        terms.push(token.to_string());
        if terms.len() >= MAX_QUERY_TERMS {
            break;
        }
    }
    terms
}

/// Derive the HF search query from a directive,
/// deterministically. The rule, in full:
///
/// 1. Lowercase; split on every non-alphanumeric character.
/// 2. Drop the stopwords ([`STOPWORDS`]) and the seat words
///    ([`SEAT_WORDS`]).
/// 3. Deduplicate, preserving first-seen order; cap at
///    [`MAX_QUERY_TERMS`] terms.
/// 4. Append the named seat's family term
///    ([`seat_family_term`]) when it is not already present
///    and the cap leaves room — so a purely qualitative
///    directive ("reduce coder latency") still seeds a
///    search, from its seat.
/// 5. Join with single spaces. "GGUF" is never a query term:
///    the search filters to GGUF repos with the `filter`
///    parameter instead.
///
/// The result can be empty (a directive of pure stopwords
/// naming no seat); the client then searches unseeded — top
/// GGUF repos by downloads — which is the honest fallback for
/// a directive with no model-identifying terms.
pub fn search_query_for_directive(directive: &str) -> String {
    let mut terms = search_terms_for_directive(directive);
    if let Some(family) = seat_family_term(seat_for_directive(directive)) {
        if !terms.iter().any(|t| t == family) && terms.len() < MAX_QUERY_TERMS {
            terms.push(family.to_string());
        }
    }
    terms.join(" ")
}

/// The deep pass's query-variant set (§P1.4, D2),
/// deduplicated, in run order, capped at
/// [`MAX_QUERY_VARIANTS`]:
///
/// 1. All terms joined — today's query, seat-family term
///    included per [`search_query_for_directive`].
/// 2. Each salient term alone (≤ [`MAX_QUERY_TERMS`]).
/// 3. The seat-family term alone, when the seat contributes
///    one.
///
/// Why variants: HF search is conjunctive (§P1.3), so one
/// unmatchable term in the joined query zeroes the whole
/// pass, and a joined query can only narrow. Single-term
/// variants recover the recall; the pool merge restores
/// order. (Interpretation, named: the design's variant 3,
/// "the seat-family variant", is read as the family term as
/// its own query — the reading under which the set's cap of
/// 6 is exactly 1 joined + 4 singles + 1 family.)
pub fn query_variants_for_directive(directive: &str) -> Vec<String> {
    let mut variants: Vec<String> = Vec::new();
    let push = |q: String, variants: &mut Vec<String>| {
        if !variants.contains(&q) && variants.len() < MAX_QUERY_VARIANTS {
            variants.push(q);
        }
    };
    push(search_query_for_directive(directive), &mut variants);
    for term in search_terms_for_directive(directive) {
        push(term, &mut variants);
    }
    if let Some(family) = seat_family_term(seat_for_directive(directive)) {
        push(family.to_string(), &mut variants);
    }
    variants
}

// ---------------------------------------------------------------------------
// The HF client (thin HTTP over the pure parsers)
// ---------------------------------------------------------------------------

/// A small Hugging Face API client. The base URL is injectable
/// (default [`HF_DEFAULT_BASE_URL`]) so tests run against a
/// local mock server; production passes the default. No auth
/// header is ever sent — public models only (§A.5).
#[derive(Debug, Clone)]
pub struct HfClient {
    base_url: String,
    client: reqwest::Client,
}

impl HfClient {
    pub fn new(base_url: impl Into<String>) -> Result<Self, DiscoverError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| DiscoverError::Http(e.to_string()))?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client,
        })
    }

    pub fn with_default_base() -> Result<Self, DiscoverError> {
        Self::new(HF_DEFAULT_BASE_URL)
    }

    /// `GET /api/models?search=…&filter=gguf&sort=downloads` —
    /// the discovery search (§A.6). An empty query omits the
    /// `search` parameter: top GGUF repos by downloads.
    pub async fn search(&self, query: &str) -> Result<Vec<HfSearchHit>, DiscoverError> {
        self.search_with_limit(query, MAX_SEARCH_REPOS).await
    }

    /// The search at a caller-chosen depth: the discovery
    /// pre-pass runs at [`MAX_SEARCH_REPOS`]; the form lookup
    /// and its type-ahead run at [`MAX_LOOKUP_REPOS`] (P2-D2).
    /// The result is capped in the parser as well as in the
    /// request, so the depth a surface was designed for is
    /// the depth it gets.
    pub async fn search_with_limit(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<HfSearchHit>, DiscoverError> {
        let mut url = reqwest::Url::parse(&format!("{}/api/models", self.base_url))
            .map_err(|e| DiscoverError::Http(format!("invalid base URL: {e}")))?;
        {
            let mut pairs = url.query_pairs_mut();
            if !query.is_empty() {
                pairs.append_pair("search", query);
            }
            pairs
                .append_pair("filter", "gguf")
                .append_pair("sort", "downloads")
                .append_pair("direction", "-1")
                .append_pair("limit", &limit.to_string());
        }
        let body = self.get_text(url.as_str()).await?;
        parse_search_results_capped(&body, limit)
    }

    /// The Add Model form's shallow lookup (P2-D2): one
    /// search at [`MAX_LOOKUP_REPOS`], then one record fetch
    /// per repo, each assembled into a picker row with its
    /// quant files and their fit verdicts. A repo whose
    /// record fails is skipped, as in the discovery pre-pass;
    /// a failed search fails the lookup — the form reports
    /// it and manual entry carries on.
    pub async fn lookup(
        &self,
        query: &str,
        free_vram_mib_per_gpu: &[u64],
    ) -> Result<Vec<LookupRepo>, DiscoverError> {
        let hits = self.search_with_limit(query, MAX_LOOKUP_REPOS).await?;
        let mut out: Vec<LookupRepo> = Vec::new();
        for hit in &hits {
            if let Ok(facts) = self.model_facts(&hit.id).await {
                out.push(lookup_repo(&facts, free_vram_mib_per_gpu));
            }
        }
        Ok(out)
    }

    /// Read one GGUF file's header over HTTP Range and parse
    /// its layout (P2-D4, Option B): `GET
    /// /{repo}/resolve/main/{file}` with a growing prefix —
    /// [`HF_HEADER_RANGE_INITIAL_BYTES`], doubling to
    /// [`HF_HEADER_RANGE_CAP_BYTES`] — re-parsing after each
    /// extension until [`manager_models::parse_gguf_layout`]
    /// completes. A server that ignores `Range` and answers
    /// 200 is read incrementally from that same response,
    /// never buffered whole. Any failure — transport, a
    /// non-2xx status, a parse error, or a header past the
    /// cap — is an `Err`: the caller treats every one as
    /// "header unavailable" and the add falls back to the
    /// post-download proposal (Option A); this read never
    /// blocks an add.
    pub async fn fetch_header(
        &self,
        repo_id: &str,
        file: &str,
    ) -> Result<manager_models::GgufLayout, DiscoverError> {
        let url = format!("{}/{repo_id}/resolve/main/{file}", self.base_url);
        let mut buf: Vec<u8> = Vec::new();
        // A 200 response being consumed incrementally: the
        // server ignored Range, so the rungs extend the read
        // of this one body instead of re-requesting.
        let mut streaming: Option<reqwest::Response> = None;
        let mut stream_eof = false;
        let mut rung = HF_HEADER_RANGE_INITIAL_BYTES;
        loop {
            if (buf.len() as u64) < rung && !stream_eof {
                if let Some(resp) = streaming.as_mut() {
                    while (buf.len() as u64) < rung {
                        match resp.chunk().await {
                            Ok(Some(chunk)) => buf.extend_from_slice(&chunk),
                            Ok(None) => {
                                stream_eof = true;
                                break;
                            }
                            Err(e) => return Err(DiscoverError::Http(e.to_string())),
                        }
                    }
                } else {
                    let resp = self
                        .client
                        .get(&url)
                        .header("Range", format!("bytes=0-{}", rung - 1))
                        .send()
                        .await
                        .map_err(|e| DiscoverError::Http(e.to_string()))?;
                    match resp.status() {
                        reqwest::StatusCode::PARTIAL_CONTENT => {
                            let bytes = resp
                                .bytes()
                                .await
                                .map_err(|e| DiscoverError::Http(e.to_string()))?;
                            // A short 206 body means the file
                            // itself ends there: no rung will
                            // ever extend it further.
                            stream_eof = (bytes.len() as u64) < rung;
                            buf = bytes.to_vec();
                        }
                        reqwest::StatusCode::OK => {
                            streaming = Some(resp);
                            continue;
                        }
                        status => {
                            return Err(DiscoverError::Http(format!(
                                "HTTP {status} reading the file header"
                            )));
                        }
                    }
                }
            }
            match manager_models::parse_gguf_layout(&buf) {
                Ok(layout) => return Ok(layout),
                Err(GgufError::Incomplete) => {
                    if stream_eof || rung >= HF_HEADER_RANGE_CAP_BYTES {
                        return Err(DiscoverError::Parse(format!(
                            "GGUF header did not complete within the {} MiB range-read cap",
                            HF_HEADER_RANGE_CAP_BYTES / (1024 * 1024)
                        )));
                    }
                    rung = (rung * 2).min(HF_HEADER_RANGE_CAP_BYTES);
                }
                Err(e) => {
                    return Err(DiscoverError::Parse(format!(
                        "GGUF header parse failed: {e:?}"
                    )));
                }
            }
        }
    }

    /// `GET /api/models/{id}?blobs=true` — one repo's record:
    /// the `gguf` block plus siblings with exact sizes (§A.6).
    pub async fn model_facts(&self, repo_id: &str) -> Result<HfModelFacts, DiscoverError> {
        let url = format!("{}/api/models/{repo_id}?blobs=true", self.base_url);
        let body = self.get_text(&url).await?;
        parse_model_record(&body)
    }

    /// The whole pre-pass for one directive: seed the search
    /// from the directive, fetch each top repo's record, and
    /// assemble the candidate facts with fit verdicts against
    /// the supplied free-VRAM snapshot. A repo whose record
    /// fails to load or parse is skipped — breadth survives
    /// one bad repo — while a failed *search* fails the pass:
    /// with no search results there is nothing to be
    /// deterministic about. Order is the search's order
    /// (downloads), each repo's quants sorted by file name.
    pub async fn discover(
        &self,
        directive: &str,
        free_vram_mib_per_gpu: &[u64],
        retrieved_at: &str,
    ) -> Result<Vec<CandidateFact>, DiscoverError> {
        let query = search_query_for_directive(directive);
        let hits = self.search(&query).await?;
        let mut out: Vec<CandidateFact> = Vec::new();
        for hit in &hits {
            if let Ok(facts) = self.model_facts(&hit.id).await {
                out.extend(candidates_from_model(
                    &facts,
                    free_vram_mib_per_gpu,
                    retrieved_at,
                ));
            }
        }
        Ok(out)
    }

    async fn get_text(&self, url: &str) -> Result<String, DiscoverError> {
        Ok(self
            .get_checked(url, Duration::from_secs(30))
            .await?
            .1)
    }

    /// GET with a per-request timeout, returning the
    /// response headers beside the body (the deep pass reads
    /// `Link` and `RateLimit` off search responses). HTTP 429
    /// maps to [`DiscoverError::RateLimited`]; other
    /// non-success statuses keep the excerpted-body `Http`
    /// error.
    async fn get_checked(
        &self,
        url: &str,
        timeout: Duration,
    ) -> Result<(reqwest::header::HeaderMap, String), DiscoverError> {
        let resp = self
            .client
            .get(url)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| DiscoverError::Http(e.to_string()))?;
        let status = resp.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(DiscoverError::RateLimited);
        }
        let headers = resp.headers().clone();
        let text = resp
            .text()
            .await
            .map_err(|e| DiscoverError::Http(e.to_string()))?;
        if !status.is_success() {
            let excerpt: String = text.chars().take(200).collect();
            return Err(DiscoverError::Http(format!("HTTP {status}: {excerpt}")));
        }
        Ok((headers, text))
    }

    /// The first-page URL of one deep-pass variant search
    /// (§P1.4): `full=true` (so hits carry `lastModified` for
    /// the search-layer filters) at [`SEARCH_PAGE_LIMIT`].
    fn deep_search_url(&self, query: &str) -> String {
        let mut url = reqwest::Url::parse(&format!("{}/api/models", self.base_url))
            .expect("base URL was validated at client construction");
        {
            let mut pairs = url.query_pairs_mut();
            if !query.is_empty() {
                pairs.append_pair("search", query);
            }
            pairs
                .append_pair("filter", "gguf")
                .append_pair("sort", "downloads")
                .append_pair("direction", "-1")
                .append_pair("limit", &SEARCH_PAGE_LIMIT.to_string())
                .append_pair("full", "true");
        }
        url.to_string()
    }

    /// Fetch one deep-pass search page by URL (the first
    /// page from [`Self::deep_search_url`], later pages from
    /// the previous page's `Link` cursor), with the deep
    /// per-request timeout.
    async fn search_page(&self, url: &str) -> Result<SearchPage, DiscoverError> {
        let (headers, body) = self
            .get_checked(url, DISCOVERY_REQUEST_TIMEOUT)
            .await?;
        let link = headers
            .get(reqwest::header::LINK)
            .and_then(|v| v.to_str().ok());
        let rate_limit = headers
            .get("ratelimit")
            .and_then(|v| v.to_str().ok());
        Ok(SearchPage {
            hits: parse_search_results_capped(&body, SEARCH_PAGE_LIMIT)?,
            next_url: parse_link_next(link),
            rate_limit_remaining: parse_rate_limit_remaining(rate_limit),
        })
    }

    /// One repo's record for the deep pass, with the
    /// response's rate-limit remainder beside the result.
    async fn model_facts_deep(
        &self,
        repo_id: &str,
    ) -> (Result<HfModelFacts, DiscoverError>, Option<u64>) {
        let url = format!("{}/api/models/{repo_id}?blobs=true", self.base_url);
        match self.get_checked(&url, DISCOVERY_REQUEST_TIMEOUT).await {
            Ok((headers, body)) => {
                let remaining = parse_rate_limit_remaining(
                    headers.get("ratelimit").and_then(|v| v.to_str().ok()),
                );
                (parse_model_record(&body), remaining)
            }
            Err(DiscoverError::RateLimited) => (Err(DiscoverError::RateLimited), Some(0)),
            Err(e) => (Err(e), None),
        }
    }

    /// Fetch one batch of records with bounded concurrency
    /// (D4): a `JoinSet` gated by a semaphore at
    /// [`DISCOVERY_CONCURRENCY`]. Returns the completed
    /// results (unordered) and whether the pass deadline
    /// cut the batch short — on expiry the remaining tasks
    /// are aborted and the caller stores a partial result.
    async fn fetch_records_batch(
        &self,
        repos: &[PoolRepo],
        deadline: tokio::time::Instant,
    ) -> (Vec<(String, Result<HfModelFacts, DiscoverError>, Option<u64>)>, bool) {
        let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(DISCOVERY_CONCURRENCY));
        let mut set = tokio::task::JoinSet::new();
        for repo in repos {
            let client = self.clone();
            let semaphore = semaphore.clone();
            let repo_id = repo.repo_id.clone();
            set.spawn(async move {
                let _permit = semaphore
                    .acquire()
                    .await
                    .expect("discovery semaphore is never closed");
                let (result, remaining) = client.model_facts_deep(&repo_id).await;
                (repo_id, result, remaining)
            });
        }
        let mut results = Vec::new();
        let mut deadline_hit = false;
        loop {
            match tokio::time::timeout_at(deadline, set.join_next()).await {
                Ok(Some(Ok(item))) => results.push(item),
                Ok(Some(Err(_))) => continue, // a panicked task: skip, as a failed fetch
                Ok(None) => break,
                Err(_) => {
                    deadline_hit = true;
                    set.abort_all();
                    break;
                }
            }
        }
        (results, deadline_hit)
    }

    /// The deep discovery pass (Part 1, D1–D10): query
    /// variants searched in pages, merged into the
    /// 600-repo pool, filtered at the search layer, then
    /// records fetched in page batches at bounded
    /// concurrency up to the 200-repo depth — each record
    /// family- and fit-filtered, its candidates collapsed
    /// to ≤3 per repo, ranked lexicographically, and
    /// admitted to the 30-candidate shortlist. Fetching
    /// stops at the §P1.9 early exit, pool exhaustion, the
    /// depth cap, the 120 s deadline, or a 429 — the last
    /// two store a partial result with its reason (D5).
    pub async fn discover_deep(
        &self,
        directive: &str,
        free_vram_mib_per_gpu: &[u64],
        retrieved_at: &str,
    ) -> Result<DeepDiscovery, DiscoverError> {
        self.discover_deep_with_deadline(
            directive,
            free_vram_mib_per_gpu,
            retrieved_at,
            DISCOVERY_DEADLINE,
        )
        .await
    }

    /// [`Self::discover_deep`] with a caller-chosen deadline
    /// (the op uses [`DISCOVERY_DEADLINE`]; tests use a short
    /// one against a delaying mock server).
    pub async fn discover_deep_with_deadline(
        &self,
        directive: &str,
        free_vram_mib_per_gpu: &[u64],
        retrieved_at: &str,
        deadline_after: Duration,
    ) -> Result<DeepDiscovery, DiscoverError> {
        let deadline = tokio::time::Instant::now() + deadline_after;
        let now_unix = chrono::Utc::now().timestamp();
        let terms = search_terms_for_directive(directive);
        let family_tokens = family_tokens_for_terms(&terms);
        let variants = query_variants_for_directive(directive);

        let mut funnel = DiscoveryFunnel::default();
        let mut pool: Vec<PoolRepo> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut kept_candidates: Vec<CandidateFact> = Vec::new();
        let mut last_modified_by_repo: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let mut attempts = 0usize;
        let mut any_variant_ok = false;
        let mut first_error: Option<DiscoverError> = None;

        'variants: for variant in &variants {
            if tokio::time::Instant::now() >= deadline {
                funnel.partial = true;
                funnel.partial_reason = Some("deadline".to_string());
                break;
            }
            funnel.queries_run += 1;
            let mut page_url = Some(self.deep_search_url(variant));
            for _page in 0..MAX_PAGES_PER_VARIANT {
                let Some(url) = page_url.take() else { break };
                if tokio::time::Instant::now() >= deadline {
                    funnel.partial = true;
                    funnel.partial_reason = Some("deadline".to_string());
                    break 'variants;
                }
                let page = match self.search_page(&url).await {
                    Ok(page) => page,
                    Err(DiscoverError::RateLimited) => {
                        funnel.partial = true;
                        funnel.partial_reason = Some("rate_limited".to_string());
                        break 'variants;
                    }
                    Err(e) => {
                        // A failed variant does not fail the
                        // pass (§P1.5); all variants failing
                        // does, decided after the loop.
                        if first_error.is_none() {
                            first_error = Some(e);
                        }
                        break;
                    }
                };
                any_variant_ok = true;
                merge_pool_hits(&mut pool, &page.hits);
                funnel.pool_repos = pool.len();

                // This page's new repos, in pool order,
                // through the search-layer filters.
                let mut page_repos: Vec<PoolRepo> = Vec::new();
                for hit in &page.hits {
                    if seen.contains(&hit.id) {
                        continue;
                    }
                    if let Some(repo) = pool.iter().find(|p| p.repo_id == hit.id) {
                        if !page_repos.iter().any(|p: &PoolRepo| p.repo_id == repo.repo_id) {
                            page_repos.push(repo.clone());
                        }
                    }
                }
                page_repos.sort_by(|a, b| {
                    b.downloads
                        .cmp(&a.downloads)
                        .then_with(|| a.repo_id.cmp(&b.repo_id))
                });
                for repo in &page_repos {
                    seen.insert(repo.repo_id.clone());
                }
                let survivors = search_layer_filter(&page_repos, now_unix, &mut funnel);

                // The depth cap trims the batch; repos past
                // it are simply never fetched.
                let remaining = MAX_RECORD_FETCHES.saturating_sub(attempts);
                let batch: Vec<PoolRepo> =
                    survivors.into_iter().take(remaining).collect();
                if !batch.is_empty() {
                    let (results, deadline_hit) =
                        self.fetch_records_batch(&batch, deadline).await;
                    attempts += results.len();
                    let mut rate_limited = false;
                    // Process in batch (pool) order, not
                    // completion order: the pass is
                    // reproducible from the same responses.
                    for repo in &batch {
                        let Some((_, result, remaining)) =
                            results.iter().find(|(id, _, _)| *id == repo.repo_id)
                        else {
                            continue; // aborted at the deadline
                        };
                        if *remaining == Some(0) {
                            rate_limited = true;
                        }
                        match result {
                            Ok(facts) => {
                                funnel.repos_fetched += 1;
                                if let Some(lm) = facts
                                    .last_modified
                                    .clone()
                                    .or_else(|| repo.last_modified.clone())
                                {
                                    last_modified_by_repo.insert(repo.repo_id.clone(), lm);
                                }
                                if !family_filter_keeps(
                                    facts.architecture.as_deref(),
                                    &family_tokens,
                                ) {
                                    funnel.dropped_family += 1;
                                    continue;
                                }
                                for candidate in candidates_from_model(
                                    facts,
                                    free_vram_mib_per_gpu,
                                    retrieved_at,
                                ) {
                                    funnel.quants_considered += 1;
                                    if candidate.fit == FIT_DOES_NOT_FIT {
                                        funnel.dropped_fit += 1;
                                    } else {
                                        kept_candidates.push(candidate);
                                    }
                                }
                            }
                            Err(DiscoverError::RateLimited) => {
                                funnel.repos_failed += 1;
                                rate_limited = true;
                            }
                            Err(_) => {
                                funnel.repos_failed += 1;
                            }
                        }
                    }
                    if deadline_hit {
                        funnel.partial = true;
                        funnel.partial_reason = Some("deadline".to_string());
                        break 'variants;
                    }
                    if rate_limited {
                        funnel.partial = true;
                        funnel.partial_reason = Some("rate_limited".to_string());
                        break 'variants;
                    }
                    // §P1.9 early exit, judged on the
                    // shortlist as it now stands.
                    let (collapsed_now, _) = collapse_candidates(&kept_candidates);
                    let shortlist_now = rank_and_admit(&collapsed_now, &|repo: &str| {
                        last_modified_by_repo.get(repo).cloned()
                    });
                    if shortlist_is_full(&shortlist_now) {
                        funnel.stopped_early = true;
                        break 'variants;
                    }
                }
                if page.rate_limit_remaining == Some(0) {
                    funnel.partial = true;
                    funnel.partial_reason = Some("rate_limited".to_string());
                    break 'variants;
                }
                if attempts >= MAX_RECORD_FETCHES || pool.len() >= MAX_POOL_REPOS {
                    break 'variants;
                }
                page_url = page.next_url;
            }
        }

        if !any_variant_ok && !funnel.partial {
            return Err(first_error.unwrap_or_else(|| {
                DiscoverError::Http("every search variant failed".to_string())
            }));
        }
        let (collapsed, removed) = collapse_candidates(&kept_candidates);
        funnel.quants_collapsed = removed;
        let shortlist = rank_and_admit(&collapsed, &|repo: &str| {
            last_modified_by_repo.get(repo).cloned()
        });
        funnel.kept = shortlist.len();
        funnel.pool_repos = pool.len();
        Ok(DeepDiscovery {
            candidates: shortlist,
            funnel,
        })
    }
}

/// One page of a deep-pass variant search: its hits (capped
/// at [`SEARCH_PAGE_LIMIT`] by the parser as in the request),
/// the `Link` cursor for the next page, and the `RateLimit`
/// remainder the response reported.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchPage {
    pub hits: Vec<HfSearchHit>,
    pub next_url: Option<String>,
    pub rate_limit_remaining: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB64: u64 = 1024 * 1024;

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

    /// Trimmed shape of the real Qwen/Qwen3-8B-GGUF record
    /// (verified against the live API, §A.6): gguf block with
    /// total / architecture / context_length / totalFileSize,
    /// siblings with size + lfs.
    fn record_body() -> String {
        serde_json::json!({
            "id": "Qwen/Qwen3-8B-GGUF",
            "downloads": 594664,
            "likes": 330,
            "gguf": {
                "total": 8190735360u64,
                "architecture": "qwen3",
                "context_length": 40960,
                "totalFileSize": 5027783488u64,
            },
            "siblings": [
                { "rfilename": ".gitattributes", "size": 1798 },
                { "rfilename": "Qwen3-8B-Q4_K_M.gguf", "size": 5027783488u64,
                  "lfs": { "sha256": "d98c", "size": 5027783488u64, "pointerSize": 135 } },
                { "rfilename": "Qwen3-8B-Q8_0.gguf", "size": 8709518112u64,
                  "lfs": { "sha256": "408b", "size": 8709518112u64, "pointerSize": 135 } },
                { "rfilename": "mmproj-Qwen3-8B.gguf", "size": 640000000u64 },
                { "rfilename": "README.md", "size": 7577 },
            ],
        })
        .to_string()
    }

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

    // -- candidate assembly + fit ---------------------------------------------

    #[test]
    fn candidates_carry_record_facts_and_provenance() {
        let facts = parse_model_record(&record_body()).unwrap();
        let free = [16 * 1024];
        let cands = candidates_from_model(&facts, &free, "2026-10-17T12:00:00+00:00");
        // mmproj excluded; the two real quants remain.
        assert_eq!(cands.len(), 2);
        let q4 = &cands[0];
        assert_eq!(q4.repo_id, "Qwen/Qwen3-8B-GGUF");
        assert_eq!(q4.file, "Qwen3-8B-Q4_K_M.gguf");
        assert_eq!(q4.architecture, "qwen3");
        assert!((q4.params_b - 8.190735).abs() < 0.001, "{}", q4.params_b);
        assert_eq!(q4.ctx_max, 40960);
        assert_eq!(q4.size_bytes, 5027783488);
        assert_eq!(q4.downloads, 594664);
        assert_eq!(q4.retrieved_at, "2026-10-17T12:00:00+00:00");
        // ~4.7 GiB in 16 GiB free on one GPU.
        assert_eq!(q4.fit, FIT_FITS);
        // ~8.1 GiB also fits the single GPU.
        assert_eq!(cands[1].fit, FIT_FITS);
    }

    #[test]
    fn fit_verdicts_cover_split_too_large_and_unchecked() {
        let gib = MIB64 * 1024;
        // 12 GiB across two 8 GiB GPUs: split only.
        assert_eq!(
            fit_verdict(12 * gib, 0, 12.0, &[8 * 1024, 8 * 1024]),
            FIT_FITS_WITH_SPLIT
        );
        // 20 GiB against 16 GiB total free: no fit.
        assert_eq!(
            fit_verdict(20 * gib, 0, 20.0, &[8 * 1024, 8 * 1024]),
            FIT_DOES_NOT_FIT
        );
        // Fits on the second GPU alone even when the first is full.
        assert_eq!(fit_verdict(4 * gib, 0, 4.0, &[0, 16 * 1024]), FIT_FITS);
        // No telemetry: no fit claim either way.
        assert_eq!(fit_verdict(4 * gib, 0, 4.0, &[]), FIT_UNCHECKED);
    }

    // -- query extraction -------------------------------------------------------

    #[test]
    fn query_extraction_keeps_salient_terms_and_drops_intent() {
        // MoE is the salient term; "explore/better/capable/models"
        // are intent, "planner" is the seat, and the planner seat
        // contributes no family term.
        assert_eq!(
            search_query_for_directive("Explore better MoE capable models for the planner"),
            "moe"
        );
        // A model name survives verbatim (tokenized), stopwords go.
        assert_eq!(
            search_query_for_directive("look for a Qwen3 instruct alternative"),
            "qwen3 instruct"
        );
    }

    #[test]
    fn query_extraction_seat_fallback_and_cap() {
        // Purely qualitative coder directive: the seat's family
        // term is the only seed.
        assert_eq!(search_query_for_directive("reduce coder latency"), "coder");
        // Tool-runner seat falls back to the instruct family.
        assert_eq!(
            search_query_for_directive("the tool-runner feels slow"),
            "instruct"
        );
        // Dedup + the 4-term cap, first-seen order.
        assert_eq!(
            search_query_for_directive("qwen qwen llama gemma phi mistral extra"),
            "qwen llama gemma phi"
        );
        // Nothing salient and no seat: empty (unseeded search).
        assert_eq!(search_query_for_directive("make it better please"), "");
    }

    #[test]
    fn seat_detection_names_each_seat() {
        assert_eq!(
            seat_for_directive("for the tool-runner seat"),
            Some("tool-runner")
        );
        assert_eq!(
            seat_for_directive("tool runner is slow"),
            Some("tool-runner")
        );
        assert_eq!(seat_for_directive("the planner"), Some("planner"));
        assert_eq!(seat_for_directive("coding help"), Some("coder"));
        assert_eq!(seat_for_directive("qwen3 moe"), None);
    }

    // -- client against a local mock server -------------------------------------

    /// Minimal one-request-per-connection HTTP responder, in
    /// the spirit of manager-supervisor's mock servers: routes
    /// on the request path's prefix and answers canned HF JSON.
    async fn start_mock_hf() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 8192];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let path = req
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("")
                        .to_string();
                    let body = if path.starts_with("/api/models/org/model-gguf") {
                        serde_json::json!({
                            "id": "org/model-gguf",
                            "downloads": 42,
                            "gguf": { "total": 8000000000u64, "architecture": "qwen3",
                                       "context_length": 32768, "totalFileSize": 1000u64 },
                            "siblings": [
                                { "rfilename": "model-Q4_K_M.gguf", "size": 1000u64 },
                                { "rfilename": "mmproj.gguf", "size": 10u64 },
                            ],
                        })
                        .to_string()
                    } else if path.starts_with("/api/models") {
                        // Assert the discovery parameters ride along.
                        assert!(path.contains("filter=gguf"), "{path}");
                        assert!(path.contains("sort=downloads"), "{path}");
                        serde_json::json!([
                            { "id": "org/model-gguf", "downloads": 42, "likes": 3 },
                            { "id": "org/broken-gguf", "downloads": 1, "likes": 0 },
                        ])
                        .to_string()
                    } else {
                        String::new()
                    };
                    let status = if body.is_empty() {
                        "404 Not Found"
                    } else {
                        "200 OK"
                    };
                    let resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn discover_runs_search_then_records_against_mock_server() {
        let base = start_mock_hf().await;
        let client = HfClient::new(base).unwrap();
        let cands = client
            .discover(
                "explore moe models",
                &[16 * 1024],
                "2026-10-17T12:00:00+00:00",
            )
            .await
            .unwrap();
        // The broken repo (404 record) is skipped; the good repo
        // yields its one real quant (mmproj excluded).
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].repo_id, "org/model-gguf");
        assert_eq!(cands[0].file, "model-Q4_K_M.gguf");
        assert_eq!(cands[0].fit, FIT_FITS);
        assert_eq!(cands[0].downloads, 42);
    }

    // -- deep pool (Part 1): variants, merge, filters, collapse, rank ---------

    #[test]
    fn query_variants_cover_joined_singles_and_family() {
        // Joined + singles; the planner seat contributes no
        // family term, and "moe" alone deduplicates.
        assert_eq!(
            query_variants_for_directive("Explore better MoE capable models for the planner"),
            vec!["moe"]
        );
        assert_eq!(
            query_variants_for_directive("look for a Qwen3 instruct alternative"),
            vec!["qwen3 instruct", "qwen3", "instruct"]
        );
        // A qualitative coder directive: today's query is the
        // seat's family term; the family variant deduplicates.
        assert_eq!(
            query_variants_for_directive("reduce coder latency"),
            vec!["coder"]
        );
        // Family term appended to the join, present as its
        // own variant too.
        assert_eq!(
            query_variants_for_directive("qwen3 for the coder"),
            vec!["qwen3 coder", "qwen3", "coder"]
        );
        // The cap is exactly 1 joined + 4 singles + 1 family.
        assert_eq!(
            query_variants_for_directive("qwen llama gemma phi for the coder"),
            vec!["qwen llama gemma phi", "qwen", "llama", "gemma", "phi", "coder"]
        );
        // No salient terms and no seat: one unseeded variant.
        assert_eq!(query_variants_for_directive("make it better please"), vec![""]);
        // search_terms excludes the family term by contract.
        assert_eq!(
            search_terms_for_directive("qwen3 for the coder"),
            vec!["qwen3"]
        );
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

    #[test]
    fn pool_merge_dedupes_with_max_facts_and_caps() {
        let hit = |id: &str, downloads: u64, lm: Option<&str>| HfSearchHit {
            id: id.to_string(),
            downloads,
            likes: 0,
            last_modified: lm.map(str::to_string),
        };
        let mut pool: Vec<PoolRepo> = Vec::new();
        merge_pool_hits(
            &mut pool,
            &[
                hit("org/a", 100, Some("2026-01-01T00:00:00Z")),
                hit("org/b", 200, None),
            ],
        );
        merge_pool_hits(
            &mut pool,
            &[
                hit("org/a", 300, Some("2025-06-01T00:00:00Z")),
                hit("org/c", 50, None),
            ],
        );
        assert_eq!(pool.len(), 3);
        let a = pool.iter().find(|p| p.repo_id == "org/a").unwrap();
        assert_eq!(a.downloads, 300);
        // The maximum lastModified seen wins, not the latest
        // page's.
        assert_eq!(a.last_modified.as_deref(), Some("2026-01-01T00:00:00Z"));
        // Fetch order: downloads descending.
        assert_eq!(
            pool_order(&pool)
                .iter()
                .map(|p| p.repo_id.as_str())
                .collect::<Vec<_>>(),
            vec!["org/a", "org/b", "org/c"]
        );
        // The pool cap holds.
        let many: Vec<HfSearchHit> = (0..(MAX_POOL_REPOS + 5))
            .map(|i| hit(&format!("org/m{i}"), 1, None))
            .collect();
        let mut capped: Vec<PoolRepo> = Vec::new();
        merge_pool_hits(&mut capped, &many);
        assert_eq!(capped.len(), MAX_POOL_REPOS);
    }

    #[test]
    fn search_layer_filters_unpopular_then_stale() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-23T00:00:00Z")
            .unwrap()
            .timestamp();
        let repo = |id: &str, downloads: u64, lm: Option<&str>| PoolRepo {
            repo_id: id.to_string(),
            downloads,
            last_modified: lm.map(str::to_string),
        };
        let repos = vec![
            repo("org/unpop", 999, Some("2026-10-01T00:00:00Z")),
            repo("org/stale", 5_000, Some("2020-01-01T00:00:00Z")),
            repo("org/old-big", 50_000, Some("2020-01-01T00:00:00Z")),
            repo("org/fresh", 5_000, Some("2026-10-01T00:00:00Z")),
            repo("org/undated", 5_000, None),
        ];
        let mut funnel = DiscoveryFunnel::default();
        let kept = search_layer_filter(&repos, now, &mut funnel);
        assert_eq!(
            kept.iter().map(|p| p.repo_id.as_str()).collect::<Vec<_>>(),
            vec!["org/old-big", "org/fresh", "org/undated"]
        );
        assert_eq!(funnel.dropped_unpopular, 1);
        assert_eq!(funnel.dropped_stale, 1);
        // The staleness boundary is strict: exactly 730 days
        // is not stale; 731 is.
        let exactly = format!(
            "{}",
            chrono::DateTime::from_timestamp(now - STALE_DAYS * 86_400, 0)
                .unwrap()
                .to_rfc3339()
        );
        assert!(!is_stale(5_000, Some(&exactly), now));
        let older = format!(
            "{}",
            chrono::DateTime::from_timestamp(now - (STALE_DAYS + 1) * 86_400, 0)
                .unwrap()
                .to_rfc3339()
        );
        assert!(is_stale(5_000, Some(&older), now));
    }

    #[test]
    fn family_filter_matches_versioned_terms_and_architectures() {
        // Versioned names in the directive name the family.
        assert_eq!(
            family_tokens_for_terms(&["qwen3".to_string()]),
            vec!["qwen"]
        );
        assert_eq!(
            family_tokens_for_terms(&["llama3".to_string(), "moe".to_string()]),
            vec!["llama"]
        );
        assert!(family_tokens_for_terms(&["moe".to_string()]).is_empty());
        // A non-family prefix does not name a token ("yoke"
        // is not "yi").
        assert!(family_tokens_for_terms(&["yoke".to_string()]).is_empty());

        let qwen = family_tokens_for_terms(&["qwen3".to_string()]);
        assert!(family_filter_keeps(Some("qwen3moe"), &qwen));
        assert!(family_filter_keeps(Some("qwen3next"), &qwen));
        assert!(!family_filter_keeps(Some("llama"), &qwen));
        // No architecture: nothing to contradict, kept.
        assert!(family_filter_keeps(None, &qwen));
        // No family named: no filter.
        assert!(family_filter_keeps(Some("llama"), &[]));
        // Hybrid architectures keep their family prefix.
        let nemotron = family_tokens_for_terms(&["nemotron".to_string()]);
        assert!(family_filter_keeps(Some("nemotron_h_moe"), &nemotron));
        assert!(!family_filter_keeps(Some("whisper"), &nemotron));
    }

    fn cand(repo: &str, file: &str, size_mib: u64, fit: &str, downloads: u64) -> CandidateFact {
        CandidateFact {
            repo_id: repo.to_string(),
            file: file.to_string(),
            architecture: "qwen3".to_string(),
            params_b: 8.0,
            ctx_max: 32768,
            size_bytes: size_mib * MIB64,
            downloads,
            fit: fit.to_string(),
            retrieved_at: "t".to_string(),
        }
    }

    #[test]
    fn collapse_admits_exactly_the_three_specified_entries() {
        // The bartowski shape (§P1.14 fixture 4): a
        // quant-heavy repo — 29 quants.
        let mut candidates: Vec<CandidateFact> = Vec::new();
        for i in 0..24 {
            candidates.push(cand(
                "bartowski/big",
                &format!("big-UQ{i}.gguf"),
                100 + i as u64,
                FIT_UNCHECKED,
                7_777,
            ));
        }
        candidates.push(cand("bartowski/big", "big-Q4.gguf", 4_000, FIT_FITS, 7_777));
        candidates.push(cand("bartowski/big", "big-Q8.gguf", 9_000, FIT_FITS, 7_777));
        candidates.push(cand("bartowski/big", "big-S1.gguf", 8_000, FIT_FITS_WITH_SPLIT, 7_777));
        candidates.push(cand("bartowski/big", "big-S2.gguf", 12_000, FIT_FITS_WITH_SPLIT, 7_777));
        candidates.push(cand("bartowski/big", "big-tiny.gguf", 50, FIT_UNCHECKED, 7_777));
        assert_eq!(candidates.len(), 29);
        let (collapsed, removed) = collapse_candidates(&candidates);
        assert_eq!(collapsed.len(), 3);
        assert_eq!(removed, 26);
        let files: Vec<&str> = collapsed.iter().map(|c| c.file.as_str()).collect();
        // Largest fits, largest fits_with_split, smallest
        // overall.
        assert!(files.contains(&"big-Q8.gguf"), "{files:?}");
        assert!(files.contains(&"big-S2.gguf"), "{files:?}");
        assert!(files.contains(&"big-tiny.gguf"), "{files:?}");
        // One candidate filling two slots deduplicates.
        let single = vec![cand("org/one", "one-Q4.gguf", 100, FIT_FITS, 5)];
        let (collapsed, removed) = collapse_candidates(&single);
        assert_eq!(collapsed.len(), 1);
        assert_eq!(removed, 0);
    }

    #[test]
    fn rank_and_admit_orders_lexicographically_and_caps() {
        let lm = |repo: &str| -> Option<String> {
            match repo {
                "org/old" => Some("2024-01-01T00:00:00Z".to_string()),
                "org/new" => Some("2026-01-01T00:00:00Z".to_string()),
                _ => None,
            }
        };
        let candidates = vec![
            cand("org/old", "a.gguf", 100, FIT_FITS, 500),
            cand("org/new", "b.gguf", 100, FIT_FITS, 500),
            cand("org/rich", "c.gguf", 100, FIT_FITS, 900),
            cand("org/unchecked", "d.gguf", 100, FIT_UNCHECKED, 99_999),
            cand("org/split", "e.gguf", 100, FIT_FITS_WITH_SPLIT, 99_999),
        ];
        let admitted = rank_and_admit(&candidates, &lm);
        let order: Vec<&str> = admitted.iter().map(|c| c.repo_id.as_str()).collect();
        // Tier first (fits < split < unchecked), then
        // downloads, then recency.
        assert_eq!(
            order,
            vec!["org/rich", "org/new", "org/old", "org/split", "org/unchecked"]
        );
        // The shortlist cap binds.
        let many: Vec<CandidateFact> = (0..40)
            .map(|i| {
                cand(
                    &format!("org/m{i}"),
                    "m.gguf",
                    100,
                    FIT_FITS,
                    1_000 + i as u64,
                )
            })
            .collect();
        assert_eq!(rank_and_admit(&many, &|_| None).len(), SHORTLIST_CAP);
        // The per-repo cap binds inside admission too.
        let one_repo: Vec<CandidateFact> = (0..5)
            .map(|i| cand("org/x", &format!("x{i}.gguf"), 100 + i as u64, FIT_FITS, 10))
            .collect();
        assert_eq!(
            rank_and_admit(&one_repo, &|_| None).len(),
            MAX_CANDIDATES_PER_REPO
        );
    }

    #[test]
    fn full_shortlist_predicate_needs_checked_fits_from_enough_repos() {
        let full: Vec<CandidateFact> = (0..SHORTLIST_CAP)
            .map(|i| {
                cand(
                    &format!("org/r{}", i % 10),
                    &format!("f{i}.gguf"),
                    100,
                    FIT_FITS,
                    10,
                )
            })
            .collect();
        assert!(shortlist_is_full(&full));
        // Fewer than 10 distinct repos: not full.
        let narrow: Vec<CandidateFact> = (0..SHORTLIST_CAP)
            .map(|i| {
                cand(
                    &format!("org/r{}", i % 9),
                    &format!("f{i}.gguf"),
                    100,
                    FIT_FITS,
                    10,
                )
            })
            .collect();
        assert!(!shortlist_is_full(&narrow));
        // An unchecked entry blocks the exit.
        let mut with_unchecked = full.clone();
        with_unchecked[0].fit = FIT_UNCHECKED.to_string();
        assert!(!shortlist_is_full(&with_unchecked));
        // Below the cap: not full.
        assert!(!shortlist_is_full(&full[..SHORTLIST_CAP - 1]));
    }

    #[test]
    fn link_and_rate_limit_headers_parse() {
        let link = "<https://huggingface.co/api/models?cursor=abc>; rel=\"next\", <https://huggingface.co/api/models?cursor=xyz>; rel=\"last\"";
        assert_eq!(
            parse_link_next(Some(link)).as_deref(),
            Some("https://huggingface.co/api/models?cursor=abc")
        );
        assert_eq!(parse_link_next(Some("<https://x>; rel=\"last\"")), None);
        assert_eq!(parse_link_next(None), None);
        assert_eq!(
            parse_rate_limit_remaining(Some("\"api\";r=499;t=299")),
            Some(499)
        );
        assert_eq!(parse_rate_limit_remaining(Some("\"api\";r=0;t=12")), Some(0));
        assert_eq!(parse_rate_limit_remaining(Some("garbage")), None);
        assert_eq!(parse_rate_limit_remaining(None), None);
    }

    // -- form lookup + header range read (Part 2) ------------------------------

    /// A synthetic GGUF header in the shape the pick-time
    /// parse reads: `moe_blocks` expert blocks, an optional
    /// nextn prediction block, and a chat-template string
    /// padding the header to a chosen size class so the
    /// range-extension ladder has something to climb.
    fn gguf_header(pad_to: usize, moe_blocks: u32, nextn: bool) -> Vec<u8> {
        fn w_str(out: &mut Vec<u8>, s: &str) {
            out.extend_from_slice(&(s.len() as u64).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        fn w_kv_u32(out: &mut Vec<u8>, key: &str, v: u32) {
            w_str(out, key);
            out.extend_from_slice(&4u32.to_le_bytes());
            out.extend_from_slice(&v.to_le_bytes());
        }
        fn w_kv_str(out: &mut Vec<u8>, key: &str, v: &str) {
            w_str(out, key);
            out.extend_from_slice(&8u32.to_le_bytes());
            w_str(out, v);
        }
        fn w_tensor(out: &mut Vec<u8>, name: &str, dims: &[u64], ty: u32) {
            w_str(out, name);
            out.extend_from_slice(&(dims.len() as u32).to_le_bytes());
            for d in dims {
                out.extend_from_slice(&d.to_le_bytes());
            }
            out.extend_from_slice(&ty.to_le_bytes());
            out.extend_from_slice(&0u64.to_le_bytes());
        }
        let mut b = Vec::new();
        b.extend_from_slice(b"GGUF");
        b.extend_from_slice(&3u32.to_le_bytes());
        let tensors = moe_blocks + u32::from(nextn);
        b.extend_from_slice(&(tensors as u64).to_le_bytes());
        b.extend_from_slice(&4u64.to_le_bytes());
        w_kv_str(&mut b, "general.architecture", "testarch");
        w_kv_u32(&mut b, "testarch.block_count", moe_blocks + u32::from(nextn));
        w_kv_u32(&mut b, "testarch.nextn_predict_layers", u32::from(nextn));
        // The template value pads the header: its length
        // prefix plus content is what the parser must walk.
        let fixed = b.len() + 8 + 24; // key + type + length prefix, approx.
        let pad = pad_to.saturating_sub(fixed + tensors as usize * 64);
        w_kv_str(&mut b, "general.chat_template", &"x".repeat(pad));
        for blk in 0..moe_blocks {
            w_tensor(&mut b, &format!("blk.{blk}.ffn_gate_exps.weight"), &[256], 12);
        }
        if nextn {
            w_tensor(&mut b, &format!("blk.{moe_blocks}.nextn.eh_proj.weight"), &[16], 1);
        }
        b
    }

    /// Mock HF for the form surface: the search + record
    /// routes for one MoE+MTP repo, and resolve routes that
    /// honor Range (counting requests), ignore it (200 with
    /// the whole body), or serve a header past the cap.
    async fn start_mock_hf_forms() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let range_requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = range_requests.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let counter = counter.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 16384];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = req
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("")
                        .to_string();
                    let range_end = req
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("range:"))
                        .and_then(|l| l.split('=').nth(1))
                        .and_then(|r| r.trim().split('-').nth(1))
                        .and_then(|e| e.trim().parse::<usize>().ok());
                    let (status, body): (&str, Vec<u8>) =
                        if path.starts_with("/api/models/org/moe-mtp-gguf") {
                            (
                                "200 OK",
                                serde_json::json!({
                                    "id": "org/moe-mtp-gguf",
                                    "downloads": 777,
                                    "gguf": { "total": 48000000000u64, "architecture": "testarch",
                                               "context_length": 32768, "totalFileSize": 1000u64 },
                                    "siblings": [
                                        { "rfilename": "model-Q4_K_M.gguf", "size": 1000u64,
                                          "lfs": { "sha256": "feedface", "size": 1000u64 } },
                                    ],
                                })
                                .to_string()
                                .into_bytes(),
                            )
                        } else if path.starts_with("/api/models") {
                            (
                                "200 OK",
                                serde_json::json!([
                                    { "id": "org/moe-mtp-gguf", "downloads": 777, "likes": 5 },
                                ])
                                .to_string()
                                .into_bytes(),
                            )
                        } else if path.starts_with("/org/moe-mtp-gguf/resolve/main/") {
                            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            let header = gguf_header(5 * MIB64 as usize, 48, true);
                            match range_end {
                                Some(end) => (
                                    "206 Partial Content",
                                    header[..(end + 1).min(header.len())].to_vec(),
                                ),
                                None => ("200 OK", header),
                            }
                        } else if path.starts_with("/org/plain-gguf/resolve/main/") {
                            // Range ignored on purpose: the
                            // client must stream this body.
                            ("200 OK", gguf_header(0, 2, false))
                        } else if path.starts_with("/org/huge-gguf/resolve/main/") {
                            let header = gguf_header(17 * MIB64 as usize, 1, false);
                            match range_end {
                                Some(end) => (
                                    "206 Partial Content",
                                    header[..(end + 1).min(header.len())].to_vec(),
                                ),
                                None => ("200 OK", header),
                            }
                        } else {
                            ("404 Not Found", Vec::new())
                        };
                    let resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                });
            }
        });
        (format!("http://{addr}"), range_requests)
    }

    #[tokio::test]
    async fn lookup_against_mock_server_returns_repo_rows() {
        let (base, _) = start_mock_hf_forms().await;
        let client = HfClient::new(base).unwrap();
        let rows = client.lookup("moe", &[16 * 1024]).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].repo_id, "org/moe-mtp-gguf");
        assert_eq!(rows[0].downloads, 777);
        assert_eq!(rows[0].params_total, Some(48_000_000_000));
        assert_eq!(rows[0].quants.len(), 1);
        assert_eq!(rows[0].quants[0].quant.sha256.as_deref(), Some("feedface"));
        assert_eq!(rows[0].quants[0].fit, FIT_FITS);
    }

    #[tokio::test]
    async fn fetch_header_extends_the_range_until_the_parse_completes() {
        let (base, counter) = start_mock_hf_forms().await;
        let client = HfClient::new(base).unwrap();
        // The fixture header is ~5 MiB: the 4 MiB first rung
        // parses Incomplete, the 8 MiB rung completes it.
        let layout = client
            .fetch_header("org/moe-mtp-gguf", "model-Q4_K_M.gguf")
            .await
            .unwrap();
        assert_eq!(layout.moe_block_count, Some(48));
        assert_eq!(layout.block_count, Some(49));
        assert_eq!(layout.nextn_predict_layers, Some(1));
        assert!(layout.has_nextn_tensors);
        assert_eq!(layout.expert_bytes_by_block.len(), 48);
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn fetch_header_streams_a_range_ignoring_server() {
        let (base, _) = start_mock_hf_forms().await;
        let client = HfClient::new(base).unwrap();
        let layout = client
            .fetch_header("org/plain-gguf", "model.gguf")
            .await
            .unwrap();
        assert_eq!(layout.moe_block_count, Some(2));
        assert_eq!(layout.nextn_predict_layers, Some(0));
        assert!(!layout.has_nextn_tensors);
    }

    #[tokio::test]
    async fn fetch_header_past_the_cap_is_a_soft_failure() {
        let (base, _) = start_mock_hf_forms().await;
        let client = HfClient::new(base).unwrap();
        // ~17 MiB of header: past the 16 MiB cap, the read
        // gives up — an Err the op renders as "header
        // unavailable", never a blocked add.
        let err = client
            .fetch_header("org/huge-gguf", "model.gguf")
            .await
            .unwrap_err();
        assert!(matches!(err, DiscoverError::Parse(_)), "{err}");
    }

    #[tokio::test]
    async fn search_failure_propagates() {
        // A base URL whose server path 404s everything: the
        // search itself fails, and discover reports it rather
        // than returning an empty pre-pass.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 4096];
                    let _ = socket.read(&mut buf).await;
                    let _ = socket
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                });
            }
        });
        let client = HfClient::new(format!("http://{addr}")).unwrap();
        assert!(client.discover("moe", &[1024], "t").await.is_err());
    }

    // -- deep pass against a local mock server (Part 1, §P1.14) ---------------

    fn deep_record(
        id: &str,
        downloads: u64,
        last_modified: &str,
        files: &[(&str, u64)],
    ) -> String {
        serde_json::json!({
            "id": id,
            "downloads": downloads,
            "lastModified": last_modified,
            "gguf": { "total": 8000000000u64, "architecture": "qwen3",
                      "context_length": 32768, "totalFileSize": 1000u64 },
            "siblings": files
                .iter()
                .map(|(name, size)| serde_json::json!({ "rfilename": name, "size": size }))
                .collect::<Vec<_>>(),
        })
        .to_string()
    }

    /// Mock HF for the deep pass (§P1.14 fixtures 1–2): the
    /// "alpha beta" variant paginates via a `Link` cursor
    /// (page 2 re-reports org/a1 with higher downloads — the
    /// merge must dedupe), "alpha" returns one more repo,
    /// "beta" fails (a failed variant must not fail the
    /// pass). Search hits carry `lastModified` (`full=true`
    /// shape); the pool includes one unpopular and one stale
    /// repo the search layer must drop before any fetch.
    async fn start_mock_hf_deep() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        let base_for_link = base.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let base_for_link = base_for_link.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 16384];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = req
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("")
                        .to_string();
                    let param = |name: &str| -> Option<String> {
                        path.split('?').nth(1).and_then(|q| {
                            q.split('&').find_map(|pair| {
                                let (k, v) = pair.split_once('=')?;
                                (k == name).then(|| v.replace('+', " ").replace("%20", " "))
                            })
                        })
                    };
                    let mib = MIB64;
                    let (status, extra_headers, body): (&str, String, String) =
                        if path.contains("blobs=true") {
                            let id = path
                                .trim_start_matches("/api/models/")
                                .split('?')
                                .next()
                                .unwrap_or("")
                                .to_string();
                            let record = match id.as_str() {
                                "org/a1" => Some(deep_record(
                                    "org/a1",
                                    60_000,
                                    "2026-10-01T00:00:00.000Z",
                                    &[
                                        ("a1-Q4.gguf", 4_000 * mib),
                                        ("a1-Q8.gguf", 20_000 * mib),
                                        ("a1-IQ.gguf", 1_000 * mib),
                                    ],
                                )),
                                "org/oldbig" => Some(deep_record(
                                    "org/oldbig",
                                    20_000,
                                    "2020-01-01T00:00:00.000Z",
                                    &[("old-Q4.gguf", 2_000 * mib)],
                                )),
                                "org/a2" => Some(deep_record(
                                    "org/a2",
                                    30_000,
                                    "2026-08-01T00:00:00.000Z",
                                    &[("a2-Q4.gguf", 5_000 * mib)],
                                )),
                                "org/a3" => Some(deep_record(
                                    "org/a3",
                                    15_000,
                                    "2026-07-01T00:00:00.000Z",
                                    &[("a3-Q8.gguf", 6_000 * mib), ("a3-Q4.gguf", 500 * mib)],
                                )),
                                _ => None,
                            };
                            match record {
                                Some(body) => ("200 OK", String::new(), body),
                                None => ("404 Not Found", String::new(), String::new()),
                            }
                        } else if path.starts_with("/api/models") {
                            let search = param("search").unwrap_or_default();
                            let cursor = param("cursor");
                            match (search.as_str(), cursor.as_deref()) {
                                ("alpha beta", None) => {
                                    let next = format!(
                                        "{base_for_link}/api/models?search=alpha+beta&filter=gguf&sort=downloads&direction=-1&limit=100&full=true&cursor=p2"
                                    );
                                    (
                                        "200 OK",
                                        format!("Link: <{next}>; rel=\"next\"\r\n"),
                                        serde_json::json!([
                                            { "id": "org/a1", "downloads": 50000, "lastModified": "2026-09-01T00:00:00.000Z" },
                                            { "id": "org/unpop", "downloads": 500, "lastModified": "2026-10-01T00:00:00.000Z" },
                                            { "id": "org/stale", "downloads": 5000, "lastModified": "2020-01-01T00:00:00.000Z" },
                                            { "id": "org/oldbig", "downloads": 20000, "lastModified": "2020-01-01T00:00:00.000Z" },
                                        ])
                                        .to_string(),
                                    )
                                }
                                ("alpha beta", Some("p2")) => (
                                    "200 OK",
                                    String::new(),
                                    serde_json::json!([
                                        { "id": "org/a1", "downloads": 60000, "lastModified": "2026-10-01T00:00:00.000Z" },
                                        { "id": "org/a2", "downloads": 30000, "lastModified": "2026-08-01T00:00:00.000Z" },
                                    ])
                                    .to_string(),
                                ),
                                ("alpha", _) => (
                                    "200 OK",
                                    String::new(),
                                    serde_json::json!([
                                        { "id": "org/a3", "downloads": 15000, "lastModified": "2026-07-01T00:00:00.000Z" },
                                    ])
                                    .to_string(),
                                ),
                                ("beta", _) => {
                                    ("500 Internal Server Error", String::new(), String::new())
                                }
                                _ => ("200 OK", String::new(), "[]".to_string()),
                            }
                        } else {
                            ("404 Not Found", String::new(), String::new())
                        };
                    let resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                });
            }
        });
        base
    }

    #[tokio::test]
    async fn discover_deep_runs_variants_pages_filters_and_ranks() {
        let base = start_mock_hf_deep().await;
        let client = HfClient::new(base).unwrap();
        let out = client
            .discover_deep("alpha beta", &[16 * 1024], "2026-10-23T00:00:00Z")
            .await
            .unwrap();
        let f = &out.funnel;
        // Three variants ran ("beta" failed without failing
        // the pass); the pool merged 6 unique repos across
        // both pages and both successful variants.
        assert_eq!(f.queries_run, 3);
        assert_eq!(f.pool_repos, 6);
        assert_eq!(f.dropped_unpopular, 1);
        assert_eq!(f.dropped_stale, 1);
        assert_eq!(f.repos_fetched, 4);
        assert_eq!(f.repos_failed, 0);
        assert_eq!(f.quants_considered, 7);
        assert_eq!(f.dropped_fit, 1);
        assert_eq!(f.quants_collapsed, 0);
        assert_eq!(f.kept, 6);
        assert!(!f.partial, "{f:?}");
        assert!(!f.stopped_early, "{f:?}");
        // Ranked: fit tier, then downloads (the records'),
        // then size. org/a1's two fits lead; its 20 GiB
        // quant was dropped on fit.
        let order: Vec<(&str, &str)> = out
            .candidates
            .iter()
            .map(|c| (c.repo_id.as_str(), c.file.as_str()))
            .collect();
        assert_eq!(
            order,
            vec![
                ("org/a1", "a1-Q4.gguf"),
                ("org/a1", "a1-IQ.gguf"),
                ("org/a2", "a2-Q4.gguf"),
                ("org/oldbig", "old-Q4.gguf"),
                ("org/a3", "a3-Q8.gguf"),
                ("org/a3", "a3-Q4.gguf"),
            ]
        );
    }

    #[tokio::test]
    async fn discover_deep_stops_on_429_and_stores_partial() {
        // §P1.14 fixture 3: a 429 with RateLimit headers —
        // partial pass, rate_limited, and no retry storm
        // (exactly one request).
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter2 = counter.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let counter2 = counter2.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    counter2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let mut buf = vec![0u8; 4096];
                    let _ = socket.read(&mut buf).await;
                    let _ = socket
                        .write_all(b"HTTP/1.1 429 Too Many Requests\r\nRateLimit: \"api\";r=0;t=300\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                });
            }
        });
        let client = HfClient::new(format!("http://{addr}")).unwrap();
        let out = client
            .discover_deep("alpha beta", &[1024], "t")
            .await
            .unwrap();
        assert!(out.funnel.partial);
        assert_eq!(
            out.funnel.partial_reason.as_deref(),
            Some("rate_limited")
        );
        assert_eq!(out.funnel.queries_run, 1);
        assert!(out.candidates.is_empty());
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn discover_deep_deadline_stores_partial() {
        // §P1.14 fixture 6: the record fetch outlives the
        // pass deadline — partial pass, reason deadline.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 16384];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = req
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("")
                        .to_string();
                    let body = if path.contains("blobs=true") {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        deep_record("org/slow", 50_000, "2026-10-01T00:00:00.000Z", &[])
                    } else {
                        serde_json::json!([
                            { "id": "org/slow", "downloads": 50000, "lastModified": "2026-10-01T00:00:00.000Z" },
                        ])
                        .to_string()
                    };
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                });
            }
        });
        let client = HfClient::new(format!("http://{addr}")).unwrap();
        let out = client
            .discover_deep_with_deadline("alpha", &[1024], "t", Duration::from_millis(300))
            .await
            .unwrap();
        assert!(out.funnel.partial);
        assert_eq!(out.funnel.partial_reason.as_deref(), Some("deadline"));
        assert_eq!(out.funnel.repos_fetched, 0);
        assert!(out.candidates.is_empty());
    }

    #[tokio::test]
    async fn discover_deep_all_variants_failing_is_an_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 4096];
                    let _ = socket.read(&mut buf).await;
                    let _ = socket
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                });
            }
        });
        let client = HfClient::new(format!("http://{addr}")).unwrap();
        assert!(client.discover_deep("moe", &[1024], "t").await.is_err());
    }
}
