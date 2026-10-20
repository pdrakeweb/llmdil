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
//! No auth: public models only. No tool loop, no web search.
//! The HTTP layer is thin on purpose; the parsers are pure
//! functions over the response text so the risk-bearing logic
//! (field shapes, quant grouping, fit verdicts, query seeding)
//! is tested without any network.

use std::collections::BTreeMap;
use std::time::Duration;

use manager_models::{ExpertOffload, MemoryBasis};
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
}

/// Parse a search response body into hits, capped at
/// [`MAX_SEARCH_REPOS`]. The endpoint is asked for GGUF repos
/// sorted by downloads; the cap is also enforced here so a
/// response can never widen the pre-pass.
pub fn parse_search_results(body: &str) -> Result<Vec<HfSearchHit>, DiscoverError> {
    let mut hits: Vec<HfSearchHit> = serde_json::from_str(body)
        .map_err(|e| DiscoverError::Parse(format!("search response: {e}")))?;
    hits.truncate(MAX_SEARCH_REPOS);
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
}

#[derive(Debug, Deserialize)]
struct RawLfs {
    size: Option<u64>,
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
    let mut groups: BTreeMap<String, (u64, usize)> = BTreeMap::new();
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
        let entry = groups.entry(key).or_insert((0, 0));
        entry.0 = entry.0.saturating_add(s.size_bytes);
        entry.1 += 1;
    }
    groups
        .into_iter()
        .map(|(file, (size_bytes, shard_count))| QuantFile {
            file,
            size_bytes,
            shard_count,
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
        rs_bytes_per_seq: None,
        basis: MemoryBasis::Estimate,
        ctx_max,
        params_b,
    };
    // No offload is proposed for a candidate, and with no KV or
    // recurrent-state geometry the requirement is weights-only
    // at any context and any slot count.
    let req = layout_requirement(&facts, ExpertOffload::None, 0, 1);
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
    if let Some(family) = seat_family_term(seat_for_directive(directive)) {
        if !terms.iter().any(|t| t == family) && terms.len() < MAX_QUERY_TERMS {
            terms.push(family.to_string());
        }
    }
    terms.join(" ")
}

// ---------------------------------------------------------------------------
// The HF client (thin HTTP over the pure parsers)
// ---------------------------------------------------------------------------

/// A small Hugging Face API client. The base URL is injectable
/// (default [`HF_DEFAULT_BASE_URL`]) so tests run against a
/// local mock server; production passes the default. No auth
/// header is ever sent — public models only (§A.5).
#[derive(Debug)]
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
                .append_pair("limit", &MAX_SEARCH_REPOS.to_string());
        }
        let body = self.get_text(url.as_str()).await?;
        parse_search_results(&body)
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
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| DiscoverError::Http(e.to_string()))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| DiscoverError::Http(e.to_string()))?;
        if !status.is_success() {
            let excerpt: String = text.chars().take(200).collect();
            return Err(DiscoverError::Http(format!("HTTP {status}: {excerpt}")));
        }
        Ok(text)
    }
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
}
