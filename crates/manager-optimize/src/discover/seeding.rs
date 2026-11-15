//! Search seeding: the directive -> HF search query extraction.

use super::{MAX_QUERY_TERMS, MAX_QUERY_VARIANTS};

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

#[cfg(test)]
mod tests {
    use super::*;

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
            vec![
                "qwen llama gemma phi",
                "qwen",
                "llama",
                "gemma",
                "phi",
                "coder"
            ]
        );
        // No salient terms and no seat: one unseeded variant.
        assert_eq!(
            query_variants_for_directive("make it better please"),
            vec![""]
        );
        // search_terms excludes the family term by contract.
        assert_eq!(
            search_terms_for_directive("qwen3 for the coder"),
            vec!["qwen3"]
        );
    }
}
