//! D6 selector — evidence + topology aware candidate ranking.
//!
//! Covers: relevance-only ordering (no evidence, no registry), evidence
//! lifting a proven template above a higher-lexical unproven one, the
//! registry-topology boost (with linked-tool annotation), the
//! neutrality of absent evidence (above proven-bad, below proven-good,
//! never read as 0%), the cost tie-breaker among evidence-clearing
//! candidates, totality (annotation never drops a candidate), and
//! determinism (same inputs ⇒ same order + same `why`).

use praxec_core::discovery::{
    DiscoveryItem, DiscoveryKind, EvidenceSignal, SearchHit, rank_candidates,
};
use praxec_core::intent_index::IntentEvidence;
use praxec_core::registry_v3::Registry;

// ── fixtures ──────────────────────────────────────────────────────────────

/// A minimal v3 registry whose crossmatrix links two tools to
/// `cognitive/flow.derisk` and one to `cognitive/flow.inspect-repo`.
const REGISTRY: &str = r#"
schema: praxec.packs/v3
tools:
  - id: cpm-planner
    name: CPM Planner
    command: cpm-planner
    suggested_workflows: [cognitive/flow.derisk]
  - id: log-mcp
    name: Log MCP
    command: log-mcp
    suggested_workflows: [cognitive/flow.derisk]
  - id: ripgrep
    name: ripgrep
    command: rg
    suggested_workflows: [cognitive/flow.inspect-repo]
crossmatrix:
  - { tool: cpm-planner, workflow: cognitive/flow.derisk, role: suggested }
  - { tool: log-mcp, workflow: cognitive/flow.derisk, role: suggested }
  - { tool: ripgrep, workflow: cognitive/flow.inspect-repo, role: dependency }
"#;

fn registry() -> Registry {
    Registry::load_str(REGISTRY).expect("selector test registry loads")
}

fn item(id: &str, kind: DiscoveryKind) -> DiscoveryItem {
    DiscoveryItem {
        id: id.into(),
        kind,
        title: id.into(),
        description: String::new(),
        tags: vec![],
        examples: vec![],
        aliases: vec![],
        text: String::new(),
        links: vec![],
        verb: None,
        body: None,
        source: None,
    }
}

fn hit(id: &str, score: f32) -> SearchHit {
    SearchHit {
        score,
        item: item(id, DiscoveryKind::Workflow),
        evidence: None,
    }
}

fn hit_with_evidence(id: &str, score: f32, runs: usize, rate: f64, cost: Option<f64>) -> SearchHit {
    SearchHit {
        score,
        item: item(id, DiscoveryKind::Workflow),
        evidence: Some(IntentEvidence {
            runs,
            success_rate: rate,
            mean_cost_usd: cost,
        }),
    }
}

// ── relevance ─────────────────────────────────────────────────────────────

#[test]
fn relevance_only_preserves_lexical_order() {
    // No evidence, no registry: ranking is pure normalized relevance.
    let hits = vec![
        hit("flow.low", 1.0),
        hit("flow.high", 4.0),
        hit("flow.mid", 2.0),
    ];
    let ranked = rank_candidates(&hits, None);
    let ids: Vec<&str> = ranked.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["flow.high", "flow.mid", "flow.low"]);
    // The best hit normalizes to 1.0; components are exposed.
    assert_eq!(ranked[0].relevance, 1.0);
    assert_eq!(ranked[2].relevance, 0.25);
    assert_eq!(ranked[0].evidence, EvidenceSignal::Absent);
    assert!(ranked[0].topology.linked_tools.is_empty());
}

// ── evidence ──────────────────────────────────────────────────────────────

#[test]
fn proven_template_lifts_above_higher_lexical_but_unproven() {
    // `flow.proven` scores lower lexically (0.8 normalized) but carries a
    // 100%-success track record; `flow.unproven` is the top lexical hit with
    // no evidence. The proven template must win:
    //   proven:   0.5·0.8 + 0.3·1.0 = 0.70
    //   unproven: 0.5·1.0 + 0.3·0.5 = 0.65
    let hits = vec![
        hit("flow.unproven", 5.0),
        hit_with_evidence("flow.proven", 4.0, 6, 1.0, Some(0.02)),
    ];
    let ranked = rank_candidates(&hits, None);
    assert_eq!(ranked[0].id, "flow.proven");
    assert_eq!(ranked[1].id, "flow.unproven");
    assert_eq!(
        ranked[0].evidence,
        EvidenceSignal::Proven {
            runs: 6,
            success_rate: 1.0,
            mean_cost_usd: Some(0.02),
        }
    );
}

#[test]
fn absent_evidence_is_neutral_not_penalizing() {
    // Identical relevance. Absence of evidence must rank ABOVE a proven-bad
    // record (it is not read as 0% — FM6) and BELOW a proven-good one.
    let hits = vec![
        hit_with_evidence("flow.proven-bad", 3.0, 5, 0.2, None),
        hit("flow.no-evidence", 3.0),
        hit_with_evidence("flow.proven-good", 3.0, 5, 0.9, None),
    ];
    let ranked = rank_candidates(&hits, None);
    let ids: Vec<&str> = ranked.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(
        ids,
        ["flow.proven-good", "flow.no-evidence", "flow.proven-bad"]
    );
    // The neutral term is the midpoint, never a fabricated success rate.
    assert_eq!(ranked[1].evidence, EvidenceSignal::Absent);
    assert!(
        ranked[1].why.contains("neutral, not failure"),
        "{}",
        ranked[1].why
    );
}

#[test]
fn cheaper_evidence_breaks_ties_among_equally_proven() {
    // §D6: (success_rate desc, mean_cost_usd asc) among evidence-clearing
    // candidates. Same relevance + same rate ⇒ the cheaper template first;
    // an unpriced record sorts after priced ones.
    let hits = vec![
        hit_with_evidence("flow.unpriced", 3.0, 4, 1.0, None),
        hit_with_evidence("flow.pricey", 3.0, 4, 1.0, Some(0.50)),
        hit_with_evidence("flow.cheap", 3.0, 4, 1.0, Some(0.01)),
    ];
    let ranked = rank_candidates(&hits, None);
    let ids: Vec<&str> = ranked.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["flow.cheap", "flow.pricey", "flow.unpriced"]);
}

// ── topology ──────────────────────────────────────────────────────────────

#[test]
fn registry_suggestion_boosts_and_annotates_linked_tools() {
    // Equal relevance, no evidence. `cognitive/flow.derisk` is suggested by
    // two registry tools (suggested_workflows + crossmatrix), so it must
    // rank above the unlinked candidate and expose the linked tool ids.
    let reg = registry();
    let hits = vec![hit("flow.unlinked", 3.0), hit("cognitive/flow.derisk", 3.0)];
    let ranked = rank_candidates(&hits, Some(&reg));
    assert_eq!(ranked[0].id, "cognitive/flow.derisk");
    assert_eq!(ranked[0].topology.linked_tools, ["cpm-planner", "log-mcp"]);
    assert!(ranked[0].topology.component > 0.0);
    assert!(
        ranked[0].why.contains("cpm-planner, log-mcp"),
        "{}",
        ranked[0].why
    );
    assert_eq!(ranked[1].topology.linked_tools, Vec::<String>::new());
    assert_eq!(ranked[1].topology.component, 0.0);
}

#[test]
fn crossmatrix_dependency_edge_also_links() {
    // `cognitive/flow.inspect-repo` is linked only via a `dependency`
    // crossmatrix row — both edge roles count as topology.
    let reg = registry();
    let hits = vec![hit("cognitive/flow.inspect-repo", 3.0)];
    let ranked = rank_candidates(&hits, Some(&reg));
    assert_eq!(ranked[0].topology.linked_tools, ["ripgrep"]);
}

#[test]
fn non_workflow_candidates_get_no_topology_and_are_never_dropped() {
    // Annotation, not gatekeeping: every hit ranks, and only workflow
    // candidates read the crossmatrix (a capability sharing a linked id
    // must not inherit the boost).
    let reg = registry();
    let hits = vec![
        SearchHit {
            score: 3.0,
            item: item("cognitive/flow.derisk", DiscoveryKind::Capability),
            evidence: None,
        },
        hit("flow.other", 3.0),
    ];
    let ranked = rank_candidates(&hits, Some(&reg));
    assert_eq!(
        ranked.len(),
        hits.len(),
        "ranking is total — no candidate dropped"
    );
    let capability = ranked
        .iter()
        .find(|r| r.kind == DiscoveryKind::Capability)
        .expect("capability ranked");
    assert!(capability.topology.linked_tools.is_empty());
    assert_eq!(capability.topology.component, 0.0);
}

// ── determinism + explainability ──────────────────────────────────────────

#[test]
fn ranking_is_deterministic_same_order_same_why() {
    let reg = registry();
    let hits = vec![
        hit("cognitive/flow.derisk", 2.0),
        hit_with_evidence("flow.proven", 4.0, 6, 0.83, Some(0.02)),
        hit("flow.plain", 5.0),
        hit("cognitive/flow.inspect-repo", 2.0),
    ];
    let first = rank_candidates(&hits, Some(&reg));
    let second = rank_candidates(&hits, Some(&reg));
    assert_eq!(first, second, "same inputs ⇒ same order, scores, and why");
    // Every ranked item explains its own arithmetic.
    for r in &first {
        assert!(r.why.contains("relevance"), "{}", r.why);
        assert!(r.why.contains("evidence"), "{}", r.why);
        assert!(r.why.contains("topology"), "{}", r.why);
        assert!(r.why.contains(&format!("{:.3}", r.score)), "{}", r.why);
    }
}

#[test]
fn score_is_the_documented_weighted_sum() {
    // The combination is auditable arithmetic, not a black box: recompute
    // each score from the exposed components and the published weights.
    use praxec_core::discovery::{EVIDENCE_WEIGHT, RELEVANCE_WEIGHT, TOPOLOGY_WEIGHT};
    let reg = registry();
    let hits = vec![
        hit("cognitive/flow.derisk", 2.0),
        hit_with_evidence("flow.proven", 4.0, 6, 0.83, Some(0.02)),
        hit("flow.plain", 5.0),
    ];
    for r in rank_candidates(&hits, Some(&reg)) {
        let expected = RELEVANCE_WEIGHT * r.relevance
            + EVIDENCE_WEIGHT * r.evidence.component()
            + TOPOLOGY_WEIGHT * r.topology.component;
        assert!((r.score - expected).abs() < 1e-12, "{}: {}", r.id, r.why);
    }
}

#[test]
fn zero_score_batch_ranks_by_id_without_panicking() {
    // A want-nothing batch (all-zero lexical scores) must not divide by the
    // zero max; ordering falls back to the deterministic id tie-break.
    let hits = vec![hit("flow.b", 0.0), hit("flow.a", 0.0)];
    let ranked = rank_candidates(&hits, None);
    let ids: Vec<&str> = ranked.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["flow.a", "flow.b"]);
    assert_eq!(ranked[0].relevance, 0.0);
}
