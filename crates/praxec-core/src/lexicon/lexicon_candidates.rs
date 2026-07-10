//! SPEC §30.10.10.4 — Candidate ranking for `SUBJECT_NEEDS_DEFINITION`.
//!
//! When a workflow encounters a placeholder subject, the runtime asks: "what
//! lexicon entries might be the one the author meant?" This module walks the
//! *current bounded context* in the merged lexicon and scores each entry across
//! four tiers:
//!
//! - **Tier 1 — exact canonical**: `entry.term == unknown_subject`
//! - **Tier 2 — exact alias**: any alias of the entry matches exactly
//! - **Tier 3 — semantic similarity**: cosine similarity ≥ 0.85 between the
//!   unknown subject's embedding and each entry's stored embedding vector.
//!   Only active when an embedding backend is configured (`backend != none`).
//! - **Tier 4 — Levenshtein fuzzy**: edit distance ≤ 1 (close) or ≤ 2 (loose)
//!   against the canonical term or any alias
//!
//! Results are sorted: exact → alias → semantic → fuzzy_close → fuzzy_loose,
//! then by distance ascending within each tier. The top 5 are returned.

use serde_json::{Map, Value, json};

use crate::embeddings::{EMBEDDING_COSINE_THRESHOLD, EmbeddingProvider, cosine_similarity};

/// Sort priority for each match kind (lower = higher priority in the ranking).
const PRIORITY_EXACT: u8 = 0;
const PRIORITY_ALIAS: u8 = 1;
const PRIORITY_SEMANTIC: u8 = 2;
const PRIORITY_FUZZY_CLOSE: u8 = 3;
const PRIORITY_FUZZY_LOOSE: u8 = 4;

/// A single candidate entry returned in the `candidates` array of
/// `SUBJECT_NEEDS_DEFINITION`.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// The canonical term name of the lexicon entry (never the alias).
    pub term: String,
    /// Edit distance; 0.0 for exact matches.
    pub distance: f32,
    /// One of: `"exact"`, `"alias"`, `"fuzzy_close"`, `"fuzzy_loose"`.
    pub match_kind: &'static str,
    /// First 100 chars of `definition_short` for the entry.
    pub definition_preview: String,
}

impl Candidate {
    fn priority(&self) -> u8 {
        match self.match_kind {
            "exact" => PRIORITY_EXACT,
            "alias" => PRIORITY_ALIAS,
            "semantic" => PRIORITY_SEMANTIC,
            "fuzzy_close" => PRIORITY_FUZZY_CLOSE,
            _ => PRIORITY_FUZZY_LOOSE,
        }
    }

    /// Convert to the JSON wire shape.
    pub fn to_json(&self) -> Value {
        json!({
            "term": self.term,
            "distance": self.distance,
            "match_kind": self.match_kind,
            "definition_preview": self.definition_preview,
        })
    }
}

/// Compute the Levenshtein (edit) distance between two strings.
///
/// Classic O(m×n) DP — Unicode-aware (operates on chars, not bytes).
/// Returns `usize::MAX` when both strings are empty (degenerate; never
/// reached in practice because empty subjects are rejected upstream).
pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let m = a.len();
    let n = b.len();

    // Fast paths.
    if m == 0 {
        return n;
    }
    if n == 0 {
        return m;
    }

    // Two-row DP: only need previous and current.
    let mut prev: Vec<usize> = (0..=n).collect();
    let mut curr: Vec<usize> = vec![0; n + 1];

    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[n]
}

/// SPEC §30.10.10.4 — rank candidates for an unknown subject.
///
/// `unknown_subject` — the placeholder term to look up.
/// `lexicon_map` — the `_lexiconLibrary` map from the merged workflow definition
///                 (keyed by canonical term, values are lexicon entry objects).
/// `bounded_context` — optional; when `Some`, only entries with a matching
///                      `bounded_context` field (or `""`) are considered.
///
/// Returns at most 5 candidates, sorted by tier then distance.
///
/// Entries are deduplicated by canonical term: only the best match per entry is
/// retained (e.g. if both the canonical and an alias match fuzzy, only the
/// closer one is returned).
pub fn rank_candidates(
    unknown_subject: &str,
    lexicon_map: &Map<String, Value>,
    bounded_context: Option<&str>,
) -> Vec<Candidate> {
    let mut candidates: Vec<Candidate> = Vec::new();

    for (term, entry) in lexicon_map {
        let entry_obj = match entry.as_object() {
            Some(o) => o,
            None => continue,
        };

        // Skip placeholder entries (PENDING_DEFINITION) — they are not candidates.
        if entry_obj.get("state").and_then(Value::as_str) == Some("PENDING_DEFINITION") {
            continue;
        }

        // Bounded context filter.
        if let Some(filter_ctx) = bounded_context {
            let entry_ctx = entry_obj
                .get("bounded_context")
                .and_then(Value::as_str)
                .unwrap_or("");
            if entry_ctx != filter_ctx {
                continue;
            }
        }

        let definition_short = entry_obj
            .get("definition_short")
            .and_then(Value::as_str)
            .unwrap_or("");
        let preview: String = definition_short.chars().take(100).collect();

        // Build all surface forms for this entry: canonical + all aliases.
        let mut surface_forms: Vec<&str> = vec![term.as_str()];
        if let Some(aliases) = entry_obj.get("aliases").and_then(Value::as_array) {
            for alias_val in aliases {
                if let Some(alias) = alias_val.as_str() {
                    surface_forms.push(alias);
                }
            }
        }

        // Find the best match across all surface forms for this entry.
        let mut best: Option<Candidate> = None;

        for form in &surface_forms {
            let is_canonical = *form == term.as_str();

            // Tier 1 — exact canonical.
            if is_canonical && *form == unknown_subject {
                let c = Candidate {
                    term: term.clone(),
                    distance: 0.0,
                    match_kind: "exact",
                    definition_preview: preview.clone(),
                };
                best = Some(c);
                break; // exact canonical is the best possible match
            }

            // Tier 2 — exact alias.
            if !is_canonical && *form == unknown_subject {
                let c = Candidate {
                    term: term.clone(),
                    distance: 0.0,
                    match_kind: "alias",
                    definition_preview: preview.clone(),
                };
                // Alias exact is better than semantic/fuzzy; replace if current best is lower priority.
                match &best {
                    None => best = Some(c),
                    Some(existing) if existing.priority() > PRIORITY_ALIAS => best = Some(c),
                    _ => {}
                }
                continue;
            }

            // Tier 4 — Levenshtein fuzzy (≤ 2).
            let dist = levenshtein(unknown_subject, form);
            let match_kind: Option<&'static str> = match dist {
                1 => Some("fuzzy_close"),
                2 => Some("fuzzy_loose"),
                _ => None,
            };
            if let Some(kind) = match_kind {
                let c = Candidate {
                    term: term.clone(),
                    distance: dist as f32,
                    match_kind: kind,
                    definition_preview: preview.clone(),
                };
                let c_prio = c.priority();
                match &best {
                    None => best = Some(c),
                    Some(existing) => {
                        // Replace if: lower priority rank, or same rank but closer.
                        if c_prio < existing.priority()
                            || (c_prio == existing.priority() && c.distance < existing.distance)
                        {
                            best = Some(c);
                        }
                    }
                }
            }
        }

        if let Some(c) = best {
            candidates.push(c);
        }
    }

    // Sort: priority tier first, then by distance ascending, then by term
    // alphabetically for determinism.
    candidates.sort_by(|a, b| {
        a.priority()
            .cmp(&b.priority())
            .then(
                a.distance
                    .partial_cmp(&b.distance)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(a.term.cmp(&b.term))
    });

    // Keep top 5.
    candidates.truncate(5);
    candidates
}

/// Convenience: extract the `_lexiconLibrary` map from a synthetic definition
/// value (as produced by `PraxecServer::lexicon_merged_definition`) and call
/// `rank_candidates`. Returns an empty Vec when the library is absent or
/// malformed.
pub fn rank_candidates_from_definition(
    unknown_subject: &str,
    workflow_definition: &Value,
    bounded_context: Option<&str>,
) -> Vec<Candidate> {
    let lib = match workflow_definition
        .get("_lexiconLibrary")
        .and_then(Value::as_object)
    {
        Some(m) => m,
        None => return Vec::new(),
    };
    rank_candidates(unknown_subject, lib, bounded_context)
}

/// SPEC §30.10.10 — rank candidates including optional Tier 3 (semantic).
///
/// Calls `rank_candidates` for Tiers 1/2/4, then — when `embedder` is
/// `Some` and produces a non-empty vector — augments the result with Tier 3
/// semantic candidates (cosine similarity ≥ `EMBEDDING_COSINE_THRESHOLD`).
///
/// Entries must have a `_embedding` JSON array stored by the write path to
/// participate in Tier 3 scoring. Entries without a stored vector are silently
/// skipped.
///
/// The final list is sorted: exact → alias → semantic → fuzzy_close →
/// fuzzy_loose, then by distance/similarity within each tier. Top 5 returned.
pub async fn rank_candidates_with_embedding(
    unknown_subject: &str,
    lexicon_map: &Map<String, Value>,
    bounded_context: Option<&str>,
    embedder: Option<&dyn EmbeddingProvider>,
) -> Vec<Candidate> {
    // Tiers 1/2/4 — always computed.
    let mut base = rank_candidates(unknown_subject, lexicon_map, bounded_context);

    // Tier 3 — semantic, only when an embedder is supplied.
    let Some(emb) = embedder else {
        return base;
    };

    let unknown_vec = match emb.embed(unknown_subject).await {
        Ok(v) if !v.is_empty() => v,
        _ => return base, // no embedding or backend error → fall back to Tiers 1/2/4
    };

    // Set of terms already ranked by Tiers 1/2/4 so we don't add duplicates.
    // Use owned Strings so we don't hold an immutable borrow on `base` while
    // later pushing into it.
    let already_ranked: std::collections::HashSet<String> =
        base.iter().map(|c| c.term.clone()).collect();

    for (term, entry) in lexicon_map {
        let entry_obj = match entry.as_object() {
            Some(o) => o,
            None => continue,
        };

        // Skip PENDING_DEFINITION placeholders.
        if entry_obj.get("state").and_then(Value::as_str) == Some("PENDING_DEFINITION") {
            continue;
        }

        // Bounded context filter.
        if let Some(filter_ctx) = bounded_context {
            let entry_ctx = entry_obj
                .get("bounded_context")
                .and_then(Value::as_str)
                .unwrap_or("");
            if entry_ctx != filter_ctx {
                continue;
            }
        }

        // Already have a better match from Tiers 1/2/4.
        if already_ranked.contains(term.as_str()) {
            continue;
        }

        // Retrieve the stored embedding vector. CMP-024(b) — a `filter_map`
        // here would silently drop any non-numeric element, yielding a SHORTER
        // vector that then compares as a different-dimensioned (and thus
        // meaningless) vector. Instead, detect a corrupt element and skip the
        // whole entry LOUDLY so the data-integrity problem is observable rather
        // than producing a silently-wrong similarity score.
        let stored_vec: Vec<f32> = match entry_obj.get("_embedding").and_then(Value::as_array) {
            Some(arr) => {
                let mut v = Vec::with_capacity(arr.len());
                let mut corrupt = false;
                for elem in arr {
                    match elem.as_f64() {
                        Some(f) => v.push(f as f32),
                        None => {
                            corrupt = true;
                            break;
                        }
                    }
                }
                if corrupt {
                    tracing::warn!(
                        term = %term,
                        "skipping Tier-3 semantic candidate: stored `_embedding` \
                         contains a non-numeric element (corrupt vector)"
                    );
                    continue;
                }
                v
            }
            None => continue,
        };

        if stored_vec.is_empty() {
            continue;
        }

        let sim = cosine_similarity(&unknown_vec, &stored_vec);
        if sim >= EMBEDDING_COSINE_THRESHOLD {
            let definition_short = entry_obj
                .get("definition_short")
                .and_then(Value::as_str)
                .unwrap_or("");
            let preview: String = definition_short.chars().take(100).collect();
            // Distance for semantic = 1.0 - similarity (lower = better).
            base.push(Candidate {
                term: term.clone(),
                distance: 1.0 - sim,
                match_kind: "semantic",
                definition_preview: preview,
            });
        }
    }

    // Re-sort with semantic tier in place.
    base.sort_by(|a, b| {
        a.priority()
            .cmp(&b.priority())
            .then(
                a.distance
                    .partial_cmp(&b.distance)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(a.term.cmp(&b.term))
    });

    base.truncate(5);
    base
}

/// Convenience wrapper: extract `_lexiconLibrary` and call
/// `rank_candidates_with_embedding`.
pub async fn rank_candidates_from_definition_with_embedding(
    unknown_subject: &str,
    workflow_definition: &Value,
    bounded_context: Option<&str>,
    embedder: Option<&dyn EmbeddingProvider>,
) -> Vec<Candidate> {
    let lib = match workflow_definition
        .get("_lexiconLibrary")
        .and_then(Value::as_object)
    {
        Some(m) => m,
        None => return Vec::new(),
    };
    rank_candidates_with_embedding(unknown_subject, lib, bounded_context, embedder).await
}

/// Convert a slice of `Candidate`s to the JSON array used in the MCP response.
pub fn candidates_to_json(candidates: &[Candidate]) -> Value {
    Value::Array(candidates.iter().map(Candidate::to_json).collect())
}
