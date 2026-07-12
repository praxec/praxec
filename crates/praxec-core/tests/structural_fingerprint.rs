//! v0.0.18 D5 — structural fingerprints (mechanism #2).
//!
//! The contract under test (see `structural_fingerprint`'s module docs):
//!
//! - **Same fingerprint** for two flows that describe the same machine, however
//!   differently they are *written*: declaration order, key order, and every
//!   piece of prose (`title`, `description`, `goal`, `guidance`, `tags`) are
//!   projected away before hashing.
//! - **Different fingerprint** the moment the *graph* changes: a new state, a new
//!   transition, a re-pointed target, a different executor kind.
//! - **Exact duplicates** = identical fingerprint; **near duplicates** =
//!   different fingerprint but a Jaccard similarity ≥ threshold over the graph's
//!   feature set. Both directions are asserted: a one-edit clone IS grouped, two
//!   unrelated flows are NOT.
//! - Every workflow in the shipped cognitive pack is fingerprinted at catalog
//!   time (D5-T6), and the slot round-trips through serde (E2E-2.4).

use std::collections::BTreeMap;
use std::path::PathBuf;

use praxec_core::config::load_resolved_with_repos;
use praxec_core::discovery::{DiscoveryItem, DiscoveryKind, index_from_config};
use praxec_core::structural_fingerprint::{
    FlowFingerprint, NEAR_DUPLICATE_THRESHOLD, exact_duplicate_groups, features, fingerprint,
    fingerprint_catalog, near_duplicate_pairs, similarity, structural_form,
};
use serde_json::{Value, json};
use tempfile::TempDir;

/// The shared subject: a small but realistic flow — a human gate, a guard, a
/// composed capability, a terminal outcome.
fn review_flow() -> Value {
    json!({
        "title": "Review flow",
        "description": "Draft, then a human approves.",
        "tags": ["review", "gate"],
        "initialState": "drafting",
        "blackboard": { "verdict": { "type": "string" } },
        "states": {
            "drafting": {
                "goal": "Draft the change.",
                "transitions": {
                    "draft": {
                        "target": "reviewing",
                        "actor": "deterministic",
                        "executor": {
                            "kind": "workflow",
                            "definitionId": "cap.plan.draft",
                            "use": { "outputs": { "$.context.verdict": "verdict" } }
                        }
                    }
                }
            },
            "reviewing": {
                "actor": "human",
                "guidance": "Approve or reject.",
                "transitions": {
                    "approve": {
                        "target": "done",
                        "actor": "human",
                        "guards": [{ "kind": "expr", "expr": "$.context.verdict == 'pass'" }],
                        "executor": { "kind": "noop" }
                    },
                    "reject": {
                        "target": "drafting",
                        "actor": "human",
                        "executor": { "kind": "noop" }
                    }
                }
            },
            "done": { "terminal": true, "outcome": "success" }
        }
    })
}

/// A flow with no structural relationship to [`review_flow`]: different states,
/// different edges, different executors.
fn unrelated_flow() -> Value {
    json!({
        "initialState": "ingesting",
        "states": {
            "ingesting": {
                "transitions": {
                    "ingest": {
                        "target": "indexing",
                        "executor": { "kind": "script", "subject": "ingest.corpus" }
                    }
                }
            },
            "indexing": {
                "transitions": {
                    "index": {
                        "target": "published",
                        "executor": { "kind": "script", "subject": "index.build" }
                    }
                }
            },
            "published": { "terminal": true }
        }
    })
}

/// Mutate one JSON pointer in a deep copy — the "one genuine graph change" lever.
fn mutated(base: &Value, pointer: &str, value: Value) -> Value {
    let mut copy = base.clone();
    *copy.pointer_mut(pointer).expect("pointer exists") = value;
    copy
}

// ── D5-T1 — determinism ─────────────────────────────────────────────────────

#[test]
fn fingerprint_is_deterministic_and_sha256_shaped() {
    let a = fingerprint(&review_flow());
    let b = fingerprint(&review_flow());
    assert_eq!(a, b, "same definition must fingerprint identically");
    assert!(a.starts_with("sha256:"), "unexpected format: {a}");
    assert_eq!(a.len(), "sha256:".len() + 64);
}

// ── D5-T2 — canonicalization invariance (what must NOT move the hash) ───────

#[test]
fn declaration_order_does_not_change_the_fingerprint() {
    // Same machine, written back-to-front: states declared in reverse, the
    // transitions of `reviewing` swapped, and top-level keys shuffled. The
    // canonicalizer (shared with `contract_hash`) sorts keys at every depth, so
    // a re-ordered file is the same file.
    let reordered = json!({
        "states": {
            "done": { "terminal": true, "outcome": "success" },
            "reviewing": {
                "guidance": "Approve or reject.",
                "actor": "human",
                "transitions": {
                    "reject": {
                        "actor": "human",
                        "target": "drafting",
                        "executor": { "kind": "noop" }
                    },
                    "approve": {
                        "executor": { "kind": "noop" },
                        "guards": [{ "expr": "$.context.verdict == 'pass'", "kind": "expr" }],
                        "actor": "human",
                        "target": "done"
                    }
                }
            },
            "drafting": {
                "goal": "Draft the change.",
                "transitions": {
                    "draft": {
                        "executor": {
                            "use": { "outputs": { "$.context.verdict": "verdict" } },
                            "definitionId": "cap.plan.draft",
                            "kind": "workflow"
                        },
                        "actor": "deterministic",
                        "target": "reviewing"
                    }
                }
            }
        },
        "blackboard": { "verdict": { "type": "string" } },
        "initialState": "drafting",
        "tags": ["review", "gate"],
        "description": "Draft, then a human approves.",
        "title": "Review flow"
    });
    assert_eq!(fingerprint(&review_flow()), fingerprint(&reordered));
}

#[test]
fn prose_and_metadata_do_not_change_the_fingerprint() {
    // Rewrite every human-facing string and re-classify the flow. Same machine
    // ⇒ same fingerprint. This is the case that makes dedup work at all: the
    // most likely duplicate in a minted corpus is "the same flow, reworded".
    let reworded = json!({
        "title": "Completely different title",
        "description": "Utterly different words.",
        "tags": ["nothing", "in", "common"],
        "aliases": ["ship-it"],
        "examples": ["review my change"],
        "process": "code-review",
        "initialState": "drafting",
        "blackboard": { "verdict": { "type": "string" } },
        "states": {
            "drafting": {
                "goal": "Some other goal entirely.",
                "description": "And a state description.",
                "transitions": {
                    "draft": {
                        "title": "Draft it",
                        "description": "Transition prose.",
                        "target": "reviewing",
                        "actor": "deterministic",
                        "executor": {
                            "kind": "workflow",
                            "definitionId": "cap.plan.draft",
                            "use": { "outputs": { "$.context.verdict": "verdict" } }
                        }
                    }
                }
            },
            "reviewing": {
                "actor": "human",
                "guidance": "Totally rewritten guidance.",
                "transitions": {
                    "approve": {
                        "target": "done",
                        "actor": "human",
                        "guards": [{ "kind": "expr", "expr": "$.context.verdict == 'pass'" }],
                        "executor": { "kind": "noop" }
                    },
                    "reject": {
                        "target": "drafting",
                        "actor": "human",
                        "executor": { "kind": "noop" }
                    }
                }
            },
            "done": { "terminal": true, "outcome": "success" }
        }
    });
    assert_eq!(fingerprint(&review_flow()), fingerprint(&reworded));
    // ... and the projection says why: no prose survives into the hashed form.
    let form = structural_form(&review_flow()).to_string();
    for prose in ["Review flow", "Draft the change.", "Approve or reject."] {
        assert!(
            !form.contains(prose),
            "prose leaked into the structural form: {prose}"
        );
    }
}

// ── D5-T3 — the mutations that MUST change the hash ─────────────────────────

#[test]
fn a_changed_target_changes_the_fingerprint() {
    let base = review_flow();
    let repointed = mutated(
        &base,
        "/states/reviewing/transitions/reject/target",
        json!("done"),
    );
    assert_ne!(fingerprint(&base), fingerprint(&repointed));
}

#[test]
fn a_changed_executor_kind_changes_the_fingerprint() {
    let base = review_flow();
    let re_kinded = mutated(
        &base,
        "/states/reviewing/transitions/approve/executor/kind",
        json!("script"),
    );
    assert_ne!(fingerprint(&base), fingerprint(&re_kinded));
}

#[test]
fn a_changed_composition_target_changes_the_fingerprint() {
    // Same executor kind, different capability composed — the crossmatrix edge
    // moved, so the graph moved.
    let base = review_flow();
    let re_composed = mutated(
        &base,
        "/states/drafting/transitions/draft/executor/definitionId",
        json!("cap.plan.vet"),
    );
    assert_ne!(fingerprint(&base), fingerprint(&re_composed));
}

#[test]
fn an_added_transition_changes_the_fingerprint() {
    let mut extra = review_flow();
    extra["states"]["reviewing"]["transitions"]["escalate"] =
        json!({ "target": "done", "executor": { "kind": "noop" } });
    assert_ne!(fingerprint(&review_flow()), fingerprint(&extra));
}

#[test]
fn an_added_state_changes_the_fingerprint() {
    let mut extra = review_flow();
    extra["states"]["failed"] = json!({ "terminal": true, "outcome": "failure" });
    assert_ne!(fingerprint(&review_flow()), fingerprint(&extra));
}

#[test]
fn a_removed_state_changes_the_fingerprint() {
    let mut fewer = review_flow();
    fewer["states"].as_object_mut().unwrap().remove("done");
    assert_ne!(fingerprint(&review_flow()), fingerprint(&fewer));
}

#[test]
fn a_changed_initial_state_changes_the_fingerprint() {
    let base = review_flow();
    let restarted = mutated(&base, "/initialState", json!("reviewing"));
    assert_ne!(fingerprint(&base), fingerprint(&restarted));
}

#[test]
fn an_added_guard_changes_the_fingerprint() {
    // Guards are edge conditions: adding one changes which edge is takeable.
    let base = review_flow();
    let guarded = mutated(
        &base,
        "/states/reviewing/transitions/reject",
        json!({
            "target": "drafting",
            "actor": "human",
            "guards": [{ "kind": "role", "role": "reviewer" }],
            "executor": { "kind": "noop" }
        }),
    );
    assert_ne!(fingerprint(&base), fingerprint(&guarded));
}

// ── D5-T4 — exact-duplicate detection ───────────────────────────────────────

#[test]
fn exact_duplicate_detection_finds_a_copied_flow_under_a_new_id() {
    // E2E-2.2 — the same YAML cataloged twice under different ids (the shape of
    // a real duplicate: someone copied `hello-flow.yaml` and renamed it).
    let hello = hello_flow_definition();
    let catalog = fingerprint_catalog(&BTreeMap::from([
        ("hello_flow".to_string(), hello.clone()),
        ("hello_flow_copy".to_string(), hello),
        ("review".to_string(), review_flow()),
        ("ingest".to_string(), unrelated_flow()),
    ]));

    let groups = exact_duplicate_groups(&catalog);
    assert_eq!(
        groups,
        vec![vec![
            "hello_flow".to_string(),
            "hello_flow_copy".to_string()
        ]],
        "exactly one duplicate pair expected"
    );
    // Exact duplicates are NOT double-reported as near-duplicates.
    let near = near_duplicate_pairs(&catalog, NEAR_DUPLICATE_THRESHOLD);
    assert!(
        !near
            .iter()
            .any(|n| n.a == "hello_flow" && n.b == "hello_flow_copy"),
        "an exact duplicate must not also be reported as near: {near:?}"
    );
}

// ── D5-T5 — near-duplicate detection, both directions ───────────────────────

#[test]
fn near_duplicate_detection_groups_a_one_edit_clone_and_ignores_unrelated_flows() {
    // The clone differs from `review` by exactly one transition target.
    let clone = mutated(
        &review_flow(),
        "/states/reviewing/transitions/reject/target",
        json!("done"),
    );
    let catalog = fingerprint_catalog(&BTreeMap::from([
        ("review".to_string(), review_flow()),
        ("review_variant".to_string(), clone),
        ("ingest".to_string(), unrelated_flow()),
    ]));

    let near = near_duplicate_pairs(&catalog, NEAR_DUPLICATE_THRESHOLD);
    assert_eq!(
        near.len(),
        1,
        "expected exactly one near-dup pair: {near:?}"
    );
    assert_eq!(
        (near[0].a.as_str(), near[0].b.as_str()),
        ("review", "review_variant")
    );
    assert!(
        near[0].similarity >= NEAR_DUPLICATE_THRESHOLD && near[0].similarity < 1.0,
        "similar but not identical: {}",
        near[0].similarity
    );
    // No exact-dup claim on a genuinely different graph.
    assert!(exact_duplicate_groups(&catalog).is_empty());

    // The negative direction, stated as a number rather than a vibe: two
    // unrelated flows share almost no structure.
    let unrelated = similarity(&features(&review_flow()), &features(&unrelated_flow()));
    assert!(
        unrelated < 0.2,
        "unrelated flows should be nowhere near the {NEAR_DUPLICATE_THRESHOLD} cut-off, got {unrelated}"
    );
}

#[test]
fn near_duplicate_similarity_is_explainable_as_a_feature_diff() {
    // The measure is not a black box: the score is |A ∩ B| / |A ∪ B| over
    // readable structural features, so a caller can always say WHICH facts
    // differ. Here: exactly one edge moved.
    let base = features(&review_flow());
    let clone = features(&mutated(
        &review_flow(),
        "/states/reviewing/transitions/reject/target",
        json!("done"),
    ));
    let only_in_base: Vec<&String> = base.difference(&clone).collect();
    let only_in_clone: Vec<&String> = clone.difference(&base).collect();
    assert_eq!(only_in_base, vec!["edge:reviewing-reject->drafting"]);
    assert_eq!(only_in_clone, vec!["edge:reviewing-reject->done"]);
    assert!(similarity(&base, &clone) >= NEAR_DUPLICATE_THRESHOLD);
}

// ── D5-T6 / E2E-2.1 — every cataloged workflow is fingerprinted ─────────────

#[test]
fn every_workflow_in_the_cognitive_pack_is_fingerprinted_at_catalog_time() {
    let td = TempDir::new().unwrap();
    let mut pack = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    pack.push("tests/fixtures/cognitive-architectures");
    let host = td.path().join("praxec.yaml");
    std::fs::write(
        &host,
        format!(
            "version: \"1.0.0\"\nrepos:\n  - path: \"{}\"\n",
            pack.display()
        ),
    )
    .unwrap();

    let (config, _diagnostics) = load_resolved_with_repos(&host).expect("cognitive pack loads");
    let items = index_from_config(&config).expect("catalog builds");

    let workflows: Vec<&DiscoveryItem> = items
        .iter()
        .filter(|i| i.kind == DiscoveryKind::Workflow)
        .collect();
    assert!(
        workflows.len() > 5,
        "expected the pack's flows + caps in the catalog, got {}",
        workflows.len()
    );
    for item in &workflows {
        let fp = item
            .structural_fingerprint
            .as_deref()
            .unwrap_or_else(|| panic!("{} has no structural_fingerprint", item.id));
        assert!(
            fp.starts_with("sha256:") && fp.len() == "sha256:".len() + 64,
            "{} has a malformed fingerprint: {fp}",
            item.id
        );
    }

    // Non-workflow items must NOT carry a fabricated fingerprint — a skill or a
    // script has no state machine to fingerprint.
    for item in items.iter().filter(|i| i.kind != DiscoveryKind::Workflow) {
        assert!(
            item.structural_fingerprint.is_none(),
            "{} ({:?}) must not carry a structural fingerprint",
            item.id,
            item.kind
        );
    }
}

#[test]
fn cataloged_fingerprint_matches_the_pure_function_over_the_same_definition() {
    // The catalog is not computing something *else* — it is the same hash.
    let config = json!({ "workflows": { "review": review_flow() } });
    let items = index_from_config(&config).unwrap();
    let item = items.iter().find(|i| i.id == "review").unwrap();
    assert_eq!(
        item.structural_fingerprint.as_deref(),
        Some(fingerprint(&review_flow()).as_str())
    );
}

// ── E2E-2.4 — the slot round-trips through serde ────────────────────────────

#[test]
fn the_structural_fingerprint_slot_round_trips() {
    let config = json!({ "workflows": { "review": review_flow() } });
    let item = index_from_config(&config)
        .unwrap()
        .into_iter()
        .find(|i| i.id == "review")
        .unwrap();

    let wire = serde_json::to_value(&item).unwrap();
    assert_eq!(
        wire["structural_fingerprint"],
        json!(fingerprint(&review_flow())),
        "the fingerprint must ride the serialized catalog entry (praxec.query search)"
    );
    let back: DiscoveryItem = serde_json::from_value(wire).unwrap();
    assert_eq!(back.structural_fingerprint, item.structural_fingerprint);

    // Forward/backward compat: a descriptor serialized WITHOUT the slot still
    // deserializes (`#[serde(default)]`), and an item without a fingerprint
    // does not emit a null key.
    let mut legacy = serde_json::to_value(&item).unwrap();
    legacy
        .as_object_mut()
        .unwrap()
        .remove("structural_fingerprint");
    let parsed: DiscoveryItem = serde_json::from_value(legacy).unwrap();
    assert_eq!(parsed.structural_fingerprint, None);
    let tool_shaped = serde_json::to_value(DiscoveryItem {
        structural_fingerprint: None,
        ..parsed
    })
    .unwrap();
    assert!(tool_shaped.get("structural_fingerprint").is_none());
}

// ── the near-dup measure's own guard rails ──────────────────────────────────

#[test]
fn a_flow_is_its_own_exact_duplicate_but_never_its_own_near_duplicate() {
    let one = FlowFingerprint::compute("review", &review_flow());
    let two = FlowFingerprint::compute("review_again", &review_flow());
    assert_eq!(one.fingerprint, two.fingerprint);
    assert_eq!(similarity(&one.features, &two.features), 1.0);
    assert!(near_duplicate_pairs(&[one, two], NEAR_DUPLICATE_THRESHOLD).is_empty());
}

/// `examples/hello-flow.yaml` — the repo's shared fixture flow, read from disk so
/// the duplicate-detection test runs against a REAL definition, not a mock.
fn hello_flow_definition() -> Value {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("../../examples/hello-flow.yaml");
    let config = praxec_core::config::load_yaml(&path).expect("hello-flow.yaml loads");
    config
        .pointer("/workflows/hello_flow")
        .expect("hello_flow present")
        .clone()
}
