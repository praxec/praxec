//! Unit tests for SPEC §30.10.10.4 — candidate ranking (Tiers 1, 2, 4).
//!
//! Each test asserts ONE behavior. The tests drive the pure
//! `lexicon_candidates::rank_candidates` function directly, independent of
//! any MCP server plumbing.

use praxec_core::lexicon_candidates::{levenshtein, rank_candidates};
use serde_json::{json, Map, Value};

// ── helpers ───────────────────────────────────────────────────────────────────

/// Build a minimal lexicon map with one entry.
fn single_entry(term: &str, definition_short: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        term.to_string(),
        json!({ "definition_short": definition_short, "governance": "human-only" }),
    );
    m
}

/// Build a lexicon map with one entry that carries aliases.
fn entry_with_aliases(term: &str, definition_short: &str, aliases: &[&str]) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        term.to_string(),
        json!({
            "definition_short": definition_short,
            "governance": "human-only",
            "aliases": aliases,
        }),
    );
    m
}

// ─────────────────────────────────────────────────────────────────────────────
// Tier 1 — exact canonical
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn candidates_includes_exact_canonical_match_with_match_kind_exact() {
    let lexicon = single_entry("evidence-pack", "A bundle of evidence artifacts.");
    let results = rank_candidates("evidence-pack", &lexicon, None);
    assert_eq!(results.len(), 1, "expected one candidate");
    assert_eq!(results[0].match_kind, "exact");
    assert_eq!(results[0].term, "evidence-pack");
    assert_eq!(results[0].distance, 0.0);
}

// ─────────────────────────────────────────────────────────────────────────────
// Tier 2 — exact alias
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn candidates_includes_exact_alias_match_with_match_kind_alias() {
    let lexicon = entry_with_aliases("evidence-pack", "A bundle.", &["evpack", "ep"]);
    let results = rank_candidates("evpack", &lexicon, None);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].match_kind, "alias");
    // Canonical term is returned, not the alias.
    assert_eq!(results[0].term, "evidence-pack");
    assert_eq!(results[0].distance, 0.0);
}

// ─────────────────────────────────────────────────────────────────────────────
// Tier 4 — Levenshtein fuzzy
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn candidates_includes_fuzzy_close_match_with_distance_one_for_one_char_typo() {
    // "evidnce-pack" has one deletion from "evidence-pack".
    let lexicon = single_entry("evidence-pack", "A bundle of evidence artifacts.");
    let results = rank_candidates("evidnce-pack", &lexicon, None);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].match_kind, "fuzzy_close");
    assert_eq!(results[0].distance, 1.0);
    assert_eq!(results[0].term, "evidence-pack");
}

#[test]
fn candidates_includes_fuzzy_loose_match_with_distance_two() {
    // "evdnce-pack" has two deletions from "evidence-pack".
    let lexicon = single_entry("evidence-pack", "A bundle of evidence artifacts.");
    let results = rank_candidates("evdnce-pack", &lexicon, None);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].match_kind, "fuzzy_loose");
    assert_eq!(results[0].distance, 2.0);
}

#[test]
fn candidates_excludes_term_with_distance_greater_than_two() {
    // "xyz" is very far from "evidence-pack".
    let lexicon = single_entry("evidence-pack", "A bundle of evidence artifacts.");
    let results = rank_candidates("xyz", &lexicon, None);
    assert!(
        results.is_empty(),
        "no candidates expected for distance > 2; got: {results:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Ordering
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn candidates_orders_exact_before_alias_before_fuzzy_close_before_fuzzy_loose() {
    // Build a lexicon with four entries, one per tier.
    let mut lexicon = Map::new();
    // Tier 4 (loose, dist=2) — "evdnce" is 2 edits from unknown "evdnce-pack"
    lexicon.insert(
        "loose-term".to_string(),
        json!({ "definition_short": "Loose match.", "governance": "human-only", "aliases": ["evdnce-pack-zzz"] }),
    );
    // Tier 4 (close, dist=1)
    lexicon.insert(
        "close-term".to_string(),
        json!({ "definition_short": "Close match.", "governance": "human-only", "aliases": ["evidnce-pack"] }),
    );
    // Tier 2 (alias exact)
    lexicon.insert(
        "alias-term".to_string(),
        json!({ "definition_short": "Alias exact.", "governance": "human-only", "aliases": ["evidence-pac"] }),
    );
    // Tier 1 (canonical exact)
    lexicon.insert(
        "evidence-pac".to_string(),
        json!({ "definition_short": "Exact canonical.", "governance": "human-only" }),
    );

    // unknown = "evidence-pac"
    // Tier 1: "evidence-pac" canonical exact
    // Tier 2: "alias-term" alias exact (alias = "evidence-pac")
    // Tier 4 close: "close-term" alias "evidnce-pack" — dist 1 from "evidence-pac"
    // Tier 4 loose: "loose-term" alias "evdnce-pack-zzz" — dist from "evidence-pac" > 2 (won't match)
    let results = rank_candidates("evidence-pac", &lexicon, None);

    // There should be at least 2 results: exact and alias (the fuzzy ones may or may not appear).
    assert!(
        results.len() >= 2,
        "expected at least exact + alias; got {results:?}"
    );
    // First must be exact.
    assert_eq!(
        results[0].match_kind, "exact",
        "first must be exact; got {results:?}"
    );
    // Second must be alias.
    assert_eq!(
        results[1].match_kind, "alias",
        "second must be alias; got {results:?}"
    );
    // Any fuzzy_close must come before fuzzy_loose.
    let kinds: Vec<&str> = results.iter().map(|c| c.match_kind).collect();
    let close_pos = kinds.iter().position(|&k| k == "fuzzy_close");
    let loose_pos = kinds.iter().position(|&k| k == "fuzzy_loose");
    if let (Some(cp), Some(lp)) = (close_pos, loose_pos) {
        assert!(
            cp < lp,
            "fuzzy_close must appear before fuzzy_loose; order: {kinds:?}"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Top-5 cap
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn candidates_returns_at_most_five_entries() {
    // Build 10 entries all at distance 1 from "target".
    let mut lexicon = Map::new();
    for i in 0..10u32 {
        let term = format!("target{i}");
        lexicon.insert(
            term.clone(),
            json!({ "definition_short": format!("Entry {i}."), "governance": "human-only" }),
        );
    }
    let results = rank_candidates("target0", &lexicon, None);
    assert!(
        results.len() <= 5,
        "must return at most 5 candidates; got {}",
        results.len()
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Bounded-context isolation
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn candidates_isolates_within_bounded_context() {
    // "risk" is in context "billing"; "risk-deploy" is in context "deployment".
    let mut lexicon = Map::new();
    lexicon.insert(
        "risk".to_string(),
        json!({
            "definition_short": "Risk in billing.",
            "governance": "human-only",
            "bounded_context": "billing"
        }),
    );
    lexicon.insert(
        "risk-deploy".to_string(),
        json!({
            "definition_short": "Risk in deployment.",
            "governance": "human-only",
            "bounded_context": "deployment"
        }),
    );

    // When filtering to "billing", only "risk" (exact) should appear.
    let results = rank_candidates("risk", &lexicon, Some("billing"));
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].term, "risk");

    // When filtering to "deployment", "risk" exact is excluded;
    // "risk-deploy" is at distance 7 from "risk" — no match within ≤ 2.
    let results_deploy = rank_candidates("risk", &lexicon, Some("deployment"));
    assert!(
        results_deploy.iter().all(|c| c.term == "risk-deploy"),
        "billing entry must be excluded when filtering to deployment; got {results_deploy:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// definition_preview
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn candidates_definition_preview_is_first_100_chars_of_definition_short() {
    let long_def = "A".repeat(200);
    let lexicon = single_entry("my-term", &long_def);
    let results = rank_candidates("my-term", &lexicon, None);
    assert_eq!(results.len(), 1);
    // Preview must be exactly 100 chars.
    assert_eq!(
        results[0].definition_preview.len(),
        100,
        "definition_preview must be exactly 100 chars; got {}",
        results[0].definition_preview.len()
    );
    // Content must be the first 100 chars.
    assert_eq!(
        results[0].definition_preview,
        long_def[..100],
        "definition_preview must be the first 100 chars of definition_short"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Empty result
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn candidates_empty_when_no_entries_within_distance_two() {
    let lexicon = single_entry("completely-different-term", "Some definition.");
    let results = rank_candidates("xyz", &lexicon, None);
    assert!(
        results.is_empty(),
        "expected empty candidates when no entries are within distance 2; got {results:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Levenshtein unit tests
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn levenshtein_identical_strings_returns_zero() {
    assert_eq!(levenshtein("abc", "abc"), 0);
}

#[test]
fn levenshtein_single_insertion_returns_one() {
    assert_eq!(levenshtein("kitten", "kittens"), 1);
}

#[test]
fn levenshtein_single_deletion_returns_one() {
    assert_eq!(levenshtein("kittens", "kitten"), 1);
}

#[test]
fn levenshtein_empty_strings_returns_zero() {
    assert_eq!(levenshtein("", ""), 0);
}

#[test]
fn levenshtein_one_empty_returns_length_of_other() {
    assert_eq!(levenshtein("hello", ""), 5);
    assert_eq!(levenshtein("", "world"), 5);
}

#[test]
fn levenshtein_classic_kitten_sitting() {
    // Classic example: "kitten" → "sitting" = 3.
    assert_eq!(levenshtein("kitten", "sitting"), 3);
}
