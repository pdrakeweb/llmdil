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

use std::time::Duration;

use serde::{Deserialize, Serialize};

mod candidates;
mod client;
mod lookup;
mod pool;
mod records;
mod seeding;
#[cfg(test)]
mod test_fixtures;

pub use candidates::{candidates_from_model, fit_verdict};
pub use client::{HfClient, SearchPage};
pub use lookup::{
    id_slug_from_file, lookup_repo, params_b_rounded, quant_token_from_file_name, LookupQuant,
    LookupRepo,
};
pub use pool::{
    collapse_candidates, family_filter_keeps, family_tokens_for_terms, is_stale, merge_pool_hits,
    pool_order, rank_and_admit, search_layer_filter, shortlist_is_full, PoolRepo,
};
pub use records::{
    parse_model_record, parse_search_results, parse_search_results_capped, quant_files,
    HfModelFacts, HfSearchHit, HfSibling, QuantFile,
};
pub use seeding::{
    query_variants_for_directive, search_query_for_directive, search_terms_for_directive,
    seat_for_directive,
};

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
    "qwen", "llama", "mistral", "mixtral", "gemma", "phi", "deepseek", "nemotron", "falcon", "yi",
    "glm", "internlm",
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
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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

/// The result of one deep discovery pass: the ranked
/// shortlist plus its funnel.
#[derive(Debug, Clone, PartialEq)]
pub struct DeepDiscovery {
    pub candidates: Vec<CandidateFact>,
    pub funnel: DiscoveryFunnel,
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(
            parse_rate_limit_remaining(Some("\"api\";r=0;t=12")),
            Some(0)
        );
        assert_eq!(parse_rate_limit_remaining(Some("garbage")), None);
        assert_eq!(parse_rate_limit_remaining(None), None);
    }
}
