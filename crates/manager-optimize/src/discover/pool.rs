//! Deep discovery pool (deep-discovery design Part 1): merge,
//! search-layer and family filters, quant collapsing, ranking.

use std::collections::BTreeMap;

use super::{
    CandidateFact, DiscoveryFunnel, HfSearchHit, EARLY_EXIT_MIN_REPOS, FAMILY_TOKENS, FIT_FITS,
    FIT_FITS_WITH_SPLIT, MAX_CANDIDATES_PER_REPO, MAX_POOL_REPOS, MIN_POOL_DOWNLOADS,
    SHORTLIST_CAP, STALE_DAYS, STALE_DOWNLOADS,
};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discover::test_fixtures::*;
    use crate::discover::*;

    // -- deep pool (Part 1): variants, merge, filters, collapse, rank ---------

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
        let exactly = chrono::DateTime::from_timestamp(now - STALE_DAYS * 86_400, 0)
            .unwrap()
            .to_rfc3339();
        assert!(!is_stale(5_000, Some(&exactly), now));
        let older = chrono::DateTime::from_timestamp(now - (STALE_DAYS + 1) * 86_400, 0)
            .unwrap()
            .to_rfc3339();
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
        candidates.push(cand(
            "bartowski/big",
            "big-S1.gguf",
            8_000,
            FIT_FITS_WITH_SPLIT,
            7_777,
        ));
        candidates.push(cand(
            "bartowski/big",
            "big-S2.gguf",
            12_000,
            FIT_FITS_WITH_SPLIT,
            7_777,
        ));
        candidates.push(cand(
            "bartowski/big",
            "big-tiny.gguf",
            50,
            FIT_UNCHECKED,
            7_777,
        ));
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
            vec![
                "org/rich",
                "org/new",
                "org/old",
                "org/split",
                "org/unchecked"
            ]
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
}
