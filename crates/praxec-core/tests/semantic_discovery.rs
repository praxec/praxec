//! D4 — semantic discovery surfaces (v0.0.18 "optimization flywheel",
//! `docs/test-plan-v0.0.18.md`).
//!
//! Two surfaces, one index:
//! - **(a)** workflow / capability / skill / script descriptions, and
//! - **(b)** tool descriptors (mcp / rest / cli, `tool_descriptor.rs`),
//!
//! both embedded by the same pass and ranked by the same hybrid score
//! (`LEXICAL_WEIGHT * lexical + SEMANTIC_WEIGHT * cosine`).
//!
//! The regression cases are real: praxec issue #43, observed live while the
//! v0.0.18 plan was being produced *through praxec* —
//! 1. `praxec.query {query: "cpm schedule critical path status"}` ranked the shell
//!    script `cognitive/inspect.git.status` second, on a pure keyword collision
//!    with the word "status"; and
//! 2. the capability that actually reads a submitted plan's status,
//!    `cognitive/cap.coordinate.cpm-status`, could not be found at all.
//!
//! Both are asserted on RANKING, against the *real* pack definitions
//! (`cognitive-architectures/capabilities/cap.coordinate.cpm-*.yaml`,
//! `scripts-library/inspect.git.status.yaml`), not on invented text.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::discovery::{
    DISCOVERY_INDEX_DEGRADED, DiscoveryIndex, DiscoveryItem, DiscoveryKind, InMemoryDiscoveryIndex,
    LEXICAL_WEIGHT, SEMANTIC_WEIGHT, SearchHit, SearchRequest, SemanticDiscoveryIndex,
    build_discovery_index, hybrid_score, index_from_config,
};
use praxec_core::embeddings::{
    EMBEDDING_COSINE_THRESHOLD, EmbeddingError, EmbeddingProvider, NoopEmbedder,
};
use praxec_core::tool_descriptor::ToolDescriptor;

// ── a stand-in embedder ───────────────────────────────────────────────────────

/// A five-topic bag-of-concepts embedder. Each axis counts how many of a topic's
/// words appear in the text; the vector is then L2-normalised, so cosine measures
/// *which topics a text is about*, not how long it is.
///
/// It is a stand-in for a real model, not a rig: the vocabularies are plain topic
/// word-lists applied identically to queries and to items. The one deliberate
/// omission is the word **"status"** — it belongs to no topic, because it is
/// genuinely topic-ambiguous ("git status", "plan status"), and that ambiguity is
/// the whole of the #43 defect. A real embedder disambiguates it from the words
/// around it; so does this one.
struct TopicEmbedder;

const TOPICS: [&[&str]; 5] = [
    // 0 — planning / scheduling
    &[
        "plan",
        "schedule",
        "critical path",
        "deliverable",
        "cohort",
        "bottleneck",
        "milestone",
        "cpm",
        "backlog",
        "progress",
        "coordinate",
        "dependency",
        "effort",
        "estimate",
    ],
    // 1 — version control
    &[
        "git",
        "commit",
        "branch",
        "diff",
        "repository",
        "untracked",
        "checkout",
        "working tree",
    ],
    // 2 — code review
    &["review", "finding", "lint", "standard", "quality"],
    // 3 — identity
    &["auth", "login", "credential", "token"],
    // 4 — observability
    &[
        "metric",
        "alert",
        "telemetry",
        "time-series",
        "monitor",
        "dashboard",
        "health",
        "uptime",
        "observability",
    ],
];

#[async_trait]
impl EmbeddingProvider for TopicEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        let lower = text.to_lowercase();
        let raw: Vec<f32> = TOPICS
            .iter()
            .map(|words| words.iter().filter(|w| lower.contains(**w)).count() as f32)
            .collect();
        let norm = raw.iter().map(|x| x * x).sum::<f32>().sqrt();
        Ok(if norm == 0.0 {
            raw
        } else {
            raw.iter().map(|x| x / norm).collect()
        })
    }
    async fn health_check(&self) -> Result<(), EmbeddingError> {
        Ok(())
    }
    fn dimensions(&self) -> usize {
        TOPICS.len()
    }
    fn backend_name(&self) -> &'static str {
        "topic-fake"
    }
}

/// Healthy for every text except the one poisoned item — the per-item degrade
/// (D4-T4), which must demote that item to lexical without failing the build.
struct PoisonOneEmbedder;

#[async_trait]
impl EmbeddingProvider for PoisonOneEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        if text.contains("inspect.git.status") {
            return Err(EmbeddingError::BackendFailed("poisoned item".into()));
        }
        TopicEmbedder.embed(text).await
    }
    async fn health_check(&self) -> Result<(), EmbeddingError> {
        Ok(())
    }
    fn dimensions(&self) -> usize {
        TOPICS.len()
    }
    fn backend_name(&self) -> &'static str {
        "poison-one"
    }
}

/// Endpoint down — the "embedder unavailable" case.
struct DeadEmbedder;

#[async_trait]
impl EmbeddingProvider for DeadEmbedder {
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbeddingError> {
        Err(EmbeddingError::BackendFailed("connection refused".into()))
    }
    async fn health_check(&self) -> Result<(), EmbeddingError> {
        Err(EmbeddingError::HealthCheckFailed(
            "connection refused".into(),
        ))
    }
    fn dimensions(&self) -> usize {
        TOPICS.len()
    }
    fn backend_name(&self) -> &'static str {
        "dead"
    }
}

// ── the catalog under test (transcribed from the shipped cognitive pack) ──────

const CPM_STATUS: &str = "cognitive/cap.coordinate.cpm-status";
const CPM_PLAN: &str = "cognitive/cap.coordinate.cpm-plan";
const BUILD_GRAPH: &str = "cognitive/cap.plan.build-graph";
const GIT_STATUS: &str = "cognitive/inspect.git.status";

fn pack_config() -> Value {
    json!({
        "workflows": {
            CPM_STATUS: {
                "description": "Read a cpm-planner plan's status: per-deliverable (id, status, \
                                attempt_count) rows, the critical path (ids + summed hours), and \
                                currently held locks.",
                "initialState": "ready",
                "states": {
                    "ready": {
                        "goal": "Read the plan's status snapshot from cpm-planner.",
                        "transitions": { "status": { "target": "done" } }
                    },
                    "done": { "terminal": true }
                }
            },
            CPM_PLAN: {
                "description": "Submit a deliverable graph to the cpm-planner and get back the \
                                computed schedule: critical path, bottlenecks, and parallel \
                                cohorts.",
                "initialState": "ready",
                "states": {
                    "ready": {
                        "goal": "Submit the deliverable graph to cpm-planner.",
                        "transitions": { "submit": { "target": "done" } }
                    },
                    "done": { "terminal": true }
                }
            },
            BUILD_GRAPH: {
                "description": "Decompose a specification into a dependency-ordered deliverable \
                                graph with effort estimates and risk.",
                "initialState": "ready",
                "states": {
                    "ready": {
                        "goal": "Decompose the specification.",
                        "transitions": { "submit_graph": { "target": "done" } }
                    },
                    "done": { "terminal": true }
                }
            },
            "cognitive/flow.review.code": {
                "description": "Review a diff against the project's standards and emit findings.",
                "initialState": "ready",
                "states": {
                    "ready": { "transitions": { "review": { "target": "done" } } },
                    "done": { "terminal": true }
                }
            }
        },
        "scripts": {
            GIT_STATUS: {
                "verb": "inspect",
                "source": "cognitive-architectures",
                "body": "#!/usr/bin/env bash\nset -uo pipefail\ngit status --short\n"
            }
        }
    })
}

fn catalog() -> Vec<DiscoveryItem> {
    index_from_config(&pack_config()).expect("the pack config indexes")
}

async fn ranked(index: &dyn DiscoveryIndex, query: &str) -> Vec<String> {
    index
        .search(SearchRequest {
            query: query.into(),
            kind: None,
            limit: 10,
        })
        .await
        .expect("search")
        .into_iter()
        .map(|hit| hit.item.id)
        .collect()
}

/// Position of `id` in a ranking, or `None` when it was not returned at all.
fn rank_of(ranking: &[String], id: &str) -> Option<usize> {
    ranking.iter().position(|hit| hit == id)
}

async fn semantic_index() -> SemanticDiscoveryIndex {
    SemanticDiscoveryIndex::build(catalog(), Arc::new(TopicEmbedder))
        .await
        .expect("index builds")
}

// ── E2E-1.4 — the live regression, both halves ───────────────────────────────

/// E2E-1.4 (1) — the exact query from the dogfood run. The lexical index ranks
/// the `git status` shell script above `cap.plan.build-graph`, a workflow that is
/// *entirely* about CPM planning but happens to share no keyword with the query;
/// the hybrid index puts the script below every planning item.
#[tokio::test]
async fn e2e_1_4_lexical_collision_no_longer_outranks_the_planning_items() {
    const QUERY: &str = "cpm schedule critical path status";

    // The defect, reproduced: purely lexically, the collision on "status" is worth
    // more than being about planning at all — `build-graph` does not even appear.
    let lexical = InMemoryDiscoveryIndex::new(catalog());
    let before = ranked(&lexical, QUERY).await;
    let git_before = rank_of(&before, GIT_STATUS).expect("the script is a lexical hit");
    assert!(
        rank_of(&before, BUILD_GRAPH).is_none(),
        "precondition (#43): lexically, a keyword-free planning workflow is invisible \
         while the `git status` script is hit {}: {before:?}",
        git_before + 1
    );

    // The fix: meaning re-orders the collision. The script is still *found* (it is
    // a fair keyword hit — this is not censorship), it is simply outranked by
    // everything the query is actually about.
    let after = ranked(&semantic_index().await, QUERY).await;
    let git = rank_of(&after, GIT_STATUS).expect("still findable, just not first");
    for planning in [CPM_STATUS, CPM_PLAN, BUILD_GRAPH] {
        let rank = rank_of(&after, planning)
            .unwrap_or_else(|| panic!("{planning} must be discoverable for {QUERY:?}: {after:?}"));
        assert!(
            rank < git,
            "{planning} (rank {}) must outrank the lexical collision {GIT_STATUS} (rank {}): \
             {after:?}",
            rank + 1,
            git + 1
        );
    }
}

/// E2E-1.4 (2) — "searching for a way to read a submitted plan's status returned
/// NOTHING useful". A paraphrase that shares **no** keyword with the capability
/// finds it now, and finds it by meaning alone.
#[tokio::test]
async fn e2e_1_4_previously_undiscoverable_cap_is_now_discoverable() {
    // No term here appears in any indexed field of the whole catalog — verified by
    // the lexical assertion below, which is the test's own precondition.
    const QUERY: &str = "milestone progress";

    let lexical = InMemoryDiscoveryIndex::new(catalog());
    assert!(
        ranked(&lexical, QUERY).await.is_empty(),
        "precondition (#43): the cap that reads a plan's status is not lexically \
         reachable from a paraphrase — this is the 'returned NOTHING useful' symptom"
    );

    let after = ranked(&semantic_index().await, QUERY).await;
    assert!(
        rank_of(&after, CPM_STATUS).is_some(),
        "the capability that reads a plan's status must be discoverable by meaning: {after:?}"
    );
    assert!(
        rank_of(&after, GIT_STATUS).is_none(),
        "and the `git status` script must NOT be dragged in with it: {after:?}"
    );
}

// ── D4-T1 — find by meaning, with no keyword overlap ─────────────────────────

#[tokio::test]
async fn d4_t1_semantic_hit_with_no_keyword_overlap() {
    // The word appears nowhere in the review flow, which talks about diffs,
    // standards and findings — but that is exactly what it is *for*.
    const QUERY: &str = "quality";

    let lexical = InMemoryDiscoveryIndex::new(catalog());
    assert!(
        ranked(&lexical, QUERY).await.is_empty(),
        "precondition: lexical alone finds nothing for {QUERY:?}"
    );

    let hits = ranked(&semantic_index().await, QUERY).await;
    assert_eq!(
        hits,
        vec!["cognitive/flow.review.code".to_string()],
        "semantic scoring surfaces the review flow, and only it: {hits:?}"
    );
}

/// The other half of the same contract: a keyword that lands squarely on one item
/// must still win. Hybrid must not buy recall with precision.
#[tokio::test]
async fn exact_keyword_match_still_ranks_first() {
    let hits = ranked(&semantic_index().await, "inspect.git.status").await;
    assert_eq!(
        hits.first().map(String::as_str),
        Some(GIT_STATUS),
        "an exact-name query must still return the exact item first: {hits:?}"
    );
}

// ── D4-T2 / D4-T3 — surface (b): the tool catalog ────────────────────────────

fn prometheus_tool() -> ToolDescriptor {
    ToolDescriptor::load_value(json!({
        "schema_version": "praxec.tool/v1",
        "name": "prometheus-rest",
        "version": "2.55.0",
        "source_repo": "https://github.com/prometheus/prometheus",
        "description": "Query time-series metrics and alerting rules from a Prometheus server.",
        "tags": ["observability"],
        "kind": "rest",
        "reach": {
            "connection_name": "prometheus",
            "grant_as": "prometheus",
            "connection": { "kind": "rest", "baseUrl": "http://localhost:9090", "headers": {} }
        },
        "operations": [
            {
                "id": "query-range",
                "verb": "search",
                "input_schema": { "type": "object" },
                "output_schema": { "type": "object" },
                "rest": { "method": "GET", "path": "/api/v1/query_range" }
            }
        ],
        "suggested_workflows": ["cognitive/flow.review.code"]
    }))
    .expect("descriptor loads")
}

/// D4-T2 — a tool is findable by what it DOES, through the same hybrid path as a
/// workflow. The query shares no keyword with the descriptor.
#[tokio::test]
async fn d4_t2_tool_descriptor_is_searchable_by_meaning() {
    let mut tools = vec![prometheus_tool()];

    let lexical = InMemoryDiscoveryIndex::new(vec![DiscoveryItem::from_tool_descriptor(&tools[0])]);
    assert!(
        ranked(&lexical, "watch system health").await.is_empty(),
        "precondition: the tool shares no keyword with the query"
    );

    let index =
        SemanticDiscoveryIndex::build_with_tools(catalog(), &mut tools, Arc::new(TopicEmbedder))
            .await
            .expect("index builds over workflows + tools");

    let hits = ranked(&index, "watch system health").await;
    assert_eq!(
        hits,
        vec!["prometheus-rest".to_string()],
        "the tool catalog is searchable by meaning, in the same index as the workflows"
    );

    // …and it lands as a Tool, with the HATEOAS next-step the descriptor nominates.
    let item = index
        .describe("prometheus-rest")
        .await
        .unwrap()
        .expect("the tool is describable");
    assert_eq!(item.kind, DiscoveryKind::Tool);
    assert_eq!(item.links.len(), 1);
    assert_eq!(item.links[0].rel, "start_suggested_workflow");
    assert_eq!(
        item.links[0].args["definitionId"],
        "cognitive/flow.review.code"
    );
}

/// The tool surface must not have cost the workflow surface anything: the same
/// index still answers the #43 query the same way.
#[tokio::test]
async fn d4_t2_tools_share_the_index_without_displacing_workflows() {
    let mut tools = vec![prometheus_tool()];
    let index =
        SemanticDiscoveryIndex::build_with_tools(catalog(), &mut tools, Arc::new(TopicEmbedder))
            .await
            .unwrap();

    let hits = ranked(&index, "cpm schedule critical path status").await;
    assert_eq!(
        hits.first().map(String::as_str),
        Some(CPM_STATUS),
        "adding a tool catalog must not perturb workflow ranking: {hits:?}"
    );
    assert!(
        rank_of(&hits, "prometheus-rest").is_none(),
        "an unrelated tool must not be dragged into a planning query: {hits:?}"
    );
}

/// D4-T3 — the reserved `embedding` slot is populated at index time, and the
/// stamped descriptor round-trips through serde + the canonical schema.
#[tokio::test]
async fn d4_t3_embedding_slot_is_populated_at_index_time_and_round_trips() {
    let mut tools = vec![prometheus_tool()];
    assert!(
        tools[0].embedding.is_none(),
        "a freshly-loaded descriptor carries no vector"
    );

    SemanticDiscoveryIndex::build_with_tools(catalog(), &mut tools, Arc::new(TopicEmbedder))
        .await
        .unwrap();

    let stamped = tools[0].embedding_vec().expect("the slot is populated");
    assert_eq!(stamped.len(), TOPICS.len(), "the indexed vector, verbatim");
    assert!(
        stamped.iter().any(|x| *x > 0.0),
        "a real vector, not a zero-filled placeholder: {stamped:?}"
    );

    // The stamped descriptor is still a valid descriptor: serialize → schema →
    // deserialize, with the vector intact.
    let json = serde_json::to_value(&tools[0]).unwrap();
    let reloaded = ToolDescriptor::load_value(json).expect("a stamped descriptor still validates");
    assert_eq!(reloaded.embedding_vec(), Some(stamped));
}

/// D4-T3 (regression) — a descriptor serialized WITHOUT the slot still
/// deserializes. The slot is `#[serde(default)]`; populating it must stay
/// non-breaking for every pack that predates v0.0.18.
#[test]
fn d4_t3_descriptor_without_embedding_still_deserializes() {
    let descriptor = prometheus_tool();
    assert_eq!(descriptor.embedding, None);
    assert_eq!(descriptor.embedding_vec(), None);

    let json = serde_json::to_value(&descriptor).unwrap();
    assert!(
        json.get("embedding").is_none(),
        "an unindexed descriptor serializes no `embedding` key at all"
    );
}

/// An empty vector clears the slot rather than storing `[]` — a zero-length
/// vector scores 0.0 against everything, i.e. it *looks* indexed while being
/// semantically inert.
#[test]
fn empty_vector_clears_the_slot_rather_than_faking_an_index() {
    let mut descriptor = prometheus_tool();
    descriptor.set_embedding(&[0.5, 0.5]);
    assert!(descriptor.embedding.is_some());

    descriptor.set_embedding(&[]);
    assert_eq!(descriptor.embedding, None);
    assert_eq!(descriptor.embedding_vec(), None);
}

// ── D4-T4 — per-item degrade, preserved ──────────────────────────────────────

/// D4-T4 — one item fails to embed: the build succeeds, that item degrades to
/// lexical-only (it can no longer earn the semantic half), and every other item
/// keeps its vector. The documented behaviour, unchanged.
#[tokio::test]
async fn d4_t4_per_item_embed_failure_degrades_only_that_item() {
    let items = catalog();
    let total = items.len();
    let index = SemanticDiscoveryIndex::build(items, Arc::new(PoisonOneEmbedder))
        .await
        .expect("one poisoned item must not fail the build");

    assert_eq!(
        index.embedded_count(),
        total - 1,
        "exactly the poisoned item lost its vector"
    );

    // Still lexically findable by its own name…
    let hits = ranked(&index, "inspect.git.status").await;
    assert_eq!(hits.first().map(String::as_str), Some(GIT_STATUS));

    // …and the rest of the index is still semantic.
    let hits = ranked(&index, "milestone progress").await;
    assert!(rank_of(&hits, CPM_STATUS).is_some(), "{hits:?}");
}

/// The tool half of the same contract: a descriptor whose embed failed keeps an
/// EMPTY slot. Never a stale vector, never a fabricated one.
#[tokio::test]
async fn poisoned_tool_leaves_its_embedding_slot_empty() {
    struct FailToolsOnly;
    #[async_trait]
    impl EmbeddingProvider for FailToolsOnly {
        async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
            if text.contains("prometheus") {
                return Err(EmbeddingError::BackendFailed("poisoned tool".into()));
            }
            TopicEmbedder.embed(text).await
        }
        async fn health_check(&self) -> Result<(), EmbeddingError> {
            Ok(())
        }
        fn dimensions(&self) -> usize {
            TOPICS.len()
        }
        fn backend_name(&self) -> &'static str {
            "fail-tools-only"
        }
    }

    let mut tools = vec![prometheus_tool()];
    tools[0].set_embedding(&[9.0, 9.0, 9.0, 9.0, 9.0]); // a stale vector from a previous build
    SemanticDiscoveryIndex::build_with_tools(catalog(), &mut tools, Arc::new(FailToolsOnly))
        .await
        .unwrap();

    assert_eq!(
        tools[0].embedding, None,
        "a failed embed CLEARS the slot — a stale vector is worse than no vector"
    );
}

// ── D4-T5 — the hybrid is monotone in each component ─────────────────────────

/// D4-T5 (property) — raising either component, holding the other fixed, never
/// lowers the score. Checked on a grid; the boundary values are the ones that
/// matter (a zero-lexical item, a perfect keyword match).
#[test]
fn d4_t5_hybrid_score_is_monotone_in_each_component() {
    let grid: Vec<f32> = (0..=10).map(|i| i as f32 / 10.0).collect();

    for &lex in &grid {
        for pair in grid.windows(2) {
            let (lo, hi) = (pair[0], pair[1]);
            assert!(
                hybrid_score(lex, lo) <= hybrid_score(lex, hi),
                "monotone in cosine at lexical={lex}"
            );
            assert!(
                hybrid_score(lo, lex) <= hybrid_score(hi, lex),
                "monotone in lexical at cosine={lex}"
            );
        }
    }

    // The precision guarantee the weights buy: the best lexical hit (normalised to
    // 1.0) always scores at least as much as ANY item with no lexical signal at
    // all, whatever its cosine. Exact keyword matching cannot be outbid by meaning.
    assert!(hybrid_score(1.0, 0.0) >= hybrid_score(0.0, 1.0));
    assert_eq!(LEXICAL_WEIGHT + SEMANTIC_WEIGHT, 1.0);

    // A negative cosine ("actively unrelated") contributes nothing; it does not
    // subtract from a lexical hit the item genuinely earned.
    assert_eq!(hybrid_score(1.0, -1.0), hybrid_score(1.0, 0.0));
}

/// The zero-lexical admission floor is [`EMBEDDING_COSINE_THRESHOLD`], and it is
/// load-bearing, not decorative: a real embedder returns a *positive* cosine for
/// any two texts, so without a floor every item in the catalog would enter the
/// result set of every semantic query.
///
/// `"milestone quality"` is the adversarial case: no lexical signal anywhere, and a
/// respectable cosine (~0.67–0.71) against most of the catalog because it straddles
/// two topics and commits to neither. Strongly related to nothing ⇒ it must return
/// nothing, not five mediocre hits.
#[tokio::test]
async fn zero_lexical_items_are_admitted_only_above_the_cosine_threshold() {
    async fn search(index: &SemanticDiscoveryIndex, query: &str) -> Vec<SearchHit> {
        index
            .search(SearchRequest {
                query: query.into(),
                kind: None,
                limit: 50,
            })
            .await
            .unwrap()
    }

    let index = semantic_index().await;
    let lexical = InMemoryDiscoveryIndex::new(catalog());
    for query in ["milestone quality", "quality"] {
        assert!(
            ranked(&lexical, query).await.is_empty(),
            "precondition: {query:?} carries no lexical signal at all"
        );
    }

    assert!(
        search(&index, "milestone quality").await.is_empty(),
        "a query strongly related to nothing returns nothing — the tail stays out"
    );

    // The same floor still admits a strong match, so it is a filter and not a wall.
    let hits = search(&index, "quality").await;
    assert_eq!(hits.len(), 1, "{hits:?}");
    for hit in &hits {
        // With a zero lexical component, score == SEMANTIC_WEIGHT * cosine.
        let cosine = hit.score / SEMANTIC_WEIGHT;
        assert!(
            cosine >= EMBEDDING_COSINE_THRESHOLD,
            "{} was admitted on cosine {cosine}, below the {EMBEDDING_COSINE_THRESHOLD} floor",
            hit.item.id
        );
    }
}

// ── embedder unavailable → lexical, loudly ───────────────────────────────────

/// The whole feature is additive: with the embedder down, discovery still answers
/// — lexically — and says so. (The seam is D3's `build_discovery_index`; this
/// asserts D4's scoring changes did not turn a degrade into an outage.)
#[tokio::test]
async fn embedder_unavailable_still_returns_lexical_results() {
    let sink = MemoryAuditSink::new();
    let audit: Arc<dyn AuditSink> = Arc::new(sink.clone());
    let dead: Arc<dyn EmbeddingProvider> = Arc::new(DeadEmbedder);

    let index = build_discovery_index(&pack_config(), None, &dead, &audit)
        .await
        .expect("a dead embedder must never fail the index build");

    let hits = ranked(&*index, "cpm schedule critical path status").await;
    assert!(
        rank_of(&hits, CPM_STATUS).is_some(),
        "search still answers lexically: {hits:?}"
    );
    assert!(
        sink.snapshot()
            .iter()
            .any(|event| event.event_type == DISCOVERY_INDEX_DEGRADED),
        "and the degrade is audited, never silent"
    );
}

/// No embedder configured ≡ v0.0.17 behaviour, exactly. The cross-mechanism
/// regression gate: 0.0.18 must be additive.
#[tokio::test]
async fn no_embedder_configured_is_unchanged_lexical_behaviour() {
    let audit: Arc<dyn AuditSink> = Arc::new(MemoryAuditSink::new());
    let noop: Arc<dyn EmbeddingProvider> = Arc::new(NoopEmbedder);

    let index = build_discovery_index(&pack_config(), None, &noop, &audit)
        .await
        .unwrap();

    let baseline = InMemoryDiscoveryIndex::new(catalog());
    for query in [
        "cpm schedule critical path status",
        "milestone progress",
        "inspect.git.status",
    ] {
        assert_eq!(
            ranked(&*index, query).await,
            ranked(&baseline, query).await,
            "no embedder → the lexical ranking, unchanged, for {query:?}"
        );
    }
}
