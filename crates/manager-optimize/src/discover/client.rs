//! The HF client (thin HTTP over the pure parsers).

use std::time::Duration;

use manager_models::GgufError;

use super::{
    candidates_from_model, collapse_candidates, family_filter_keeps, family_tokens_for_terms,
    lookup_repo, merge_pool_hits, parse_link_next, parse_model_record, parse_rate_limit_remaining,
    parse_search_results_capped, query_variants_for_directive, rank_and_admit, search_layer_filter,
    search_query_for_directive, search_terms_for_directive, shortlist_is_full, CandidateFact,
    DeepDiscovery, DiscoverError, DiscoveryFunnel, HfModelFacts, HfSearchHit, LookupRepo, PoolRepo,
    DISCOVERY_CONCURRENCY, DISCOVERY_DEADLINE, DISCOVERY_REQUEST_TIMEOUT, FIT_DOES_NOT_FIT,
    HF_DEFAULT_BASE_URL, HF_HEADER_RANGE_CAP_BYTES, HF_HEADER_RANGE_INITIAL_BYTES,
    MAX_LOOKUP_REPOS, MAX_PAGES_PER_VARIANT, MAX_POOL_REPOS, MAX_RECORD_FETCHES, MAX_SEARCH_REPOS,
    SEARCH_PAGE_LIMIT,
};

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
        Ok(self.get_checked(url, Duration::from_secs(30)).await?.1)
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
        let (headers, body) = self.get_checked(url, DISCOVERY_REQUEST_TIMEOUT).await?;
        let link = headers
            .get(reqwest::header::LINK)
            .and_then(|v| v.to_str().ok());
        let rate_limit = headers.get("ratelimit").and_then(|v| v.to_str().ok());
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
    ) -> (
        Vec<(String, Result<HfModelFacts, DiscoverError>, Option<u64>)>,
        bool,
    ) {
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
                        if !page_repos
                            .iter()
                            .any(|p: &PoolRepo| p.repo_id == repo.repo_id)
                        {
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
                let batch: Vec<PoolRepo> = survivors.into_iter().take(remaining).collect();
                if !batch.is_empty() {
                    let (results, deadline_hit) = self.fetch_records_batch(&batch, deadline).await;
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
    use crate::discover::test_fixtures::*;
    use crate::discover::*;

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
        w_kv_u32(
            &mut b,
            "testarch.block_count",
            moe_blocks + u32::from(nextn),
        );
        w_kv_u32(&mut b, "testarch.nextn_predict_layers", u32::from(nextn));
        // The template value pads the header: its length
        // prefix plus content is what the parser must walk.
        let fixed = b.len() + 8 + 24; // key + type + length prefix, approx.
        let pad = pad_to.saturating_sub(fixed + tensors as usize * 64);
        w_kv_str(&mut b, "general.chat_template", &"x".repeat(pad));
        for blk in 0..moe_blocks {
            w_tensor(
                &mut b,
                &format!("blk.{blk}.ffn_gate_exps.weight"),
                &[256],
                12,
            );
        }
        if nextn {
            w_tensor(
                &mut b,
                &format!("blk.{moe_blocks}.nextn.eh_proj.weight"),
                &[16],
                1,
            );
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

    fn deep_record(id: &str, downloads: u64, last_modified: &str, files: &[(&str, u64)]) -> String {
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
                    let (status, extra_headers, body): (&str, String, String) = if path
                        .contains("blobs=true")
                    {
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
        assert_eq!(out.funnel.partial_reason.as_deref(), Some("rate_limited"));
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
