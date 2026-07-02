//! SPEC §30 — lexicon primitive tests.
//!
//! Covers Tier 1: per-config lexicon block, snapshot-stamping onto
//! every workflow, search/lookup/define semantics + governance gating.

use praxec_core::config::resolve;
use praxec_core::lexicon::{
    build_combined_index, build_entry, define_allowed, governance_for, lookup_term, search_terms,
    stamp_lexicon_library, validate_lexicon,
};
use serde_json::json;

fn config_with_lexicon(lexicon: serde_json::Value) -> serde_json::Value {
    json!({
        "version": "1.0.0",
        "lexicon": lexicon,
        "workflows": {
            "demo": {
                "initialState": "idle",
                "states": { "idle": { "terminal": true } }
            }
        }
    })
}

// ── validation ────────────────────────────────────────────────────────────

#[test]
fn validate_accepts_well_formed_lexicon() {
    let cfg = config_with_lexicon(json!({
        "connector": {
            "definition_short": "A unit of integration between the gateway and an external system.",
            "bounded_context": "gateway",
            "refs": ["capability"],
            "governance": "human-only"
        }
    }));
    assert!(validate_lexicon(&cfg).is_ok());
}

#[test]
fn validate_rejects_missing_definition() {
    let cfg = config_with_lexicon(json!({
        "broken": { "bounded_context": "gateway" }
    }));
    let err = validate_lexicon(&cfg).expect_err("must reject");
    assert!(format!("{err:?}").contains("INVALID_LEXICON_ENTRY"));
    assert!(format!("{err:?}").contains("missing the required `definition:`"));
}

#[test]
fn validate_rejects_empty_definition() {
    let cfg = config_with_lexicon(json!({ "broken": { "definition_short": "   " } }));
    let err = validate_lexicon(&cfg).expect_err("must reject");
    assert!(format!("{err:?}").contains("empty `definition:`"));
}

#[test]
fn validate_rejects_unknown_governance() {
    let cfg = config_with_lexicon(json!({
        "x": { "definition_short": "y", "governance": "free-for-all" }
    }));
    let err = validate_lexicon(&cfg).expect_err("must reject");
    assert!(format!("{err:?}").contains("unknown `governance: free-for-all`"));
}

// ── snapshot stamping ─────────────────────────────────────────────────────

#[test]
fn stamping_writes_lexicon_library_onto_every_workflow() {
    let mut cfg = config_with_lexicon(json!({
        "connector": { "definition_short": "Integration unit." }
    }));
    stamp_lexicon_library(&mut cfg);
    let lib = cfg.pointer("/workflows/demo/_lexiconLibrary").unwrap();
    assert!(lib.get("connector").is_some());
    assert_eq!(
        lib.pointer("/connector/definition_short")
            .and_then(|v| v.as_str()),
        Some("Integration unit.")
    );
}

#[test]
fn full_resolve_pipeline_stamps_lexicon() {
    // Through the public resolve() — confirms validate + stamp are wired.
    let cfg = config_with_lexicon(json!({
        "ubiquitous_language": {
            "definition_short": "Shared vocabulary between domain experts and developers.",
            "bounded_context": "ddd"
        }
    }));
    let resolved = resolve(cfg).expect("resolve must succeed");
    let lib = resolved
        .pointer("/workflows/demo/_lexiconLibrary")
        .expect("workflow must have _lexiconLibrary stamped");
    assert!(lib.get("ubiquitous_language").is_some());
}

// ── lookup ────────────────────────────────────────────────────────────────

#[test]
fn lookup_returns_stamped_entry() {
    let mut cfg = config_with_lexicon(json!({
        "x": { "definition_short": "X is X." }
    }));
    stamp_lexicon_library(&mut cfg);
    let def = cfg.pointer("/workflows/demo").unwrap();
    let entry = lookup_term(def, "x", None).expect("term must exist");
    assert_eq!(
        entry.get("definition_short").and_then(|v| v.as_str()),
        Some("X is X.")
    );
}

#[test]
fn lookup_returns_none_for_unknown_term() {
    let mut cfg = config_with_lexicon(json!({}));
    stamp_lexicon_library(&mut cfg);
    let def = cfg.pointer("/workflows/demo").unwrap();
    assert!(lookup_term(def, "unknown", None).is_none());
}

#[test]
fn lookup_with_bounded_context_filter() {
    let mut cfg = config_with_lexicon(json!({
        "x": { "definition_short": "X in A", "bounded_context": "A" }
    }));
    stamp_lexicon_library(&mut cfg);
    let def = cfg.pointer("/workflows/demo").unwrap();
    assert!(lookup_term(def, "x", Some("A")).is_some());
    assert!(lookup_term(def, "x", Some("B")).is_none());
}

// ── search ────────────────────────────────────────────────────────────────

#[test]
fn search_matches_term_name_substring() {
    let mut cfg = config_with_lexicon(json!({
        "connector":  { "definition_short": "Integration." },
        "capability": { "definition_short": "Surface." },
        "executor":   { "definition_short": "Runs work." }
    }));
    stamp_lexicon_library(&mut cfg);
    let def = cfg.pointer("/workflows/demo").unwrap();
    let hits = search_terms(def, "connect", None, None);
    assert_eq!(hits.len(), 1);
    assert_eq!(
        hits[0].get("term").and_then(|v| v.as_str()),
        Some("connector")
    );
}

#[test]
fn search_matches_definition_substring() {
    let mut cfg = config_with_lexicon(json!({
        "alpha": { "definition_short": "The first letter." },
        "beta":  { "definition_short": "The second letter." }
    }));
    stamp_lexicon_library(&mut cfg);
    let def = cfg.pointer("/workflows/demo").unwrap();
    let hits = search_terms(def, "second", None, None);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].get("term").and_then(|v| v.as_str()), Some("beta"));
}

#[test]
fn search_respects_bounded_context_filter() {
    let mut cfg = config_with_lexicon(json!({
        "x_in_a": { "definition_short": "X", "bounded_context": "A" },
        "x_in_b": { "definition_short": "X", "bounded_context": "B" }
    }));
    stamp_lexicon_library(&mut cfg);
    let def = cfg.pointer("/workflows/demo").unwrap();
    let hits = search_terms(def, "X", Some("A"), None);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].get("term").and_then(|v| v.as_str()), Some("x_in_a"));
}

#[test]
fn search_respects_limit() {
    let mut cfg = config_with_lexicon(json!({
        "a": { "definition_short": "match" },
        "b": { "definition_short": "match" },
        "c": { "definition_short": "match" }
    }));
    stamp_lexicon_library(&mut cfg);
    let def = cfg.pointer("/workflows/demo").unwrap();
    let hits = search_terms(def, "match", None, Some(2));
    assert_eq!(hits.len(), 2);
}

// ── governance ────────────────────────────────────────────────────────────

#[test]
fn governance_defaults_to_human_only_when_unset() {
    let mut cfg = config_with_lexicon(json!({
        "term_no_gov": { "definition_short": "no governance field" }
    }));
    stamp_lexicon_library(&mut cfg);
    let def = cfg.pointer("/workflows/demo").unwrap();
    assert_eq!(governance_for(def, "term_no_gov"), "human-only");
}

#[test]
fn agent_rejected_against_human_only_term() {
    let mut cfg = config_with_lexicon(json!({
        "locked": { "definition_short": "x", "governance": "human-only" }
    }));
    stamp_lexicon_library(&mut cfg);
    let def = cfg.pointer("/workflows/demo").unwrap();
    let err = define_allowed(def, "locked", false).expect_err("agent must be rejected");
    assert!(err.contains("LEXICON_DEFINE_REQUIRES_HUMAN"));
    assert!(err.contains("locked"));
}

#[test]
fn human_always_allowed() {
    let mut cfg = config_with_lexicon(json!({
        "locked": { "definition_short": "x", "governance": "human-only" }
    }));
    stamp_lexicon_library(&mut cfg);
    let def = cfg.pointer("/workflows/demo").unwrap();
    assert!(define_allowed(def, "locked", true).is_ok());
}

#[test]
fn agent_allowed_against_agent_may_propose_term() {
    let mut cfg = config_with_lexicon(json!({
        "open": { "definition_short": "x", "governance": "agent-may-propose" }
    }));
    stamp_lexicon_library(&mut cfg);
    let def = cfg.pointer("/workflows/demo").unwrap();
    assert!(define_allowed(def, "open", false).is_ok());
}

// ── build_entry ───────────────────────────────────────────────────────────

#[test]
fn build_entry_sets_defaults() {
    let entry = build_entry("a real def", None, None, None, None).expect("ok");
    assert_eq!(
        entry.pointer("/definition_short").and_then(|v| v.as_str()),
        Some("a real def")
    );
    assert_eq!(
        entry.pointer("/governance").and_then(|v| v.as_str()),
        Some("human-only")
    );
}

#[test]
fn build_entry_rejects_empty_definition() {
    let err = build_entry("  ", None, None, None, None).expect_err("must reject");
    assert!(format!("{err:?}").contains("definition must be non-empty"));
}

#[test]
fn build_entry_rejects_unknown_governance() {
    let err = build_entry("a", None, None, Some("wat"), None).expect_err("must reject");
    assert!(format!("{err:?}").contains("governance must be"));
}

// ── SPEC §30.10.1 — definition_short / definition_long / aliases ──────────

#[test]
fn schema_accepts_definition_short_long_and_aliases() {
    // All three new fields accepted without error.
    let cfg = config_with_lexicon(json!({
        "evidence-pack": {
            "definition_short": "A bundle of artefacts that proves a claim.",
            "definition_long": "An evidence-pack is a versioned, immutable bundle...",
            "aliases": ["evidence-packs", "evidence pack"],
            "bounded_context": "swe-agent"
        }
    }));
    assert!(validate_lexicon(&cfg).is_ok());
}

#[test]
fn alias_lookup_returns_same_entry_as_canonical() {
    // Combined-form index: "evidence-packs" and "evidence pack" resolve to
    // the same entry as the canonical term "evidence-pack".
    let lexicon = json!({
        "evidence-pack": {
            "definition_short": "Proves a claim.",
            "aliases": ["evidence-packs", "evidence pack"],
            "bounded_context": "swe-agent"
        }
    });
    let lib = lexicon.as_object().unwrap();
    let index = build_combined_index(lib, "swe-agent").expect("no collision");
    assert!(
        index.contains_key("evidence-pack"),
        "canonical term must be indexed"
    );
    assert!(
        index.contains_key("evidence-packs"),
        "alias 'evidence-packs' must be indexed"
    );
    assert!(
        index.contains_key("evidence pack"),
        "alias 'evidence pack' must be indexed"
    );
    // All three keys map to the same entry (pointer identity).
    let canonical = index["evidence-pack"] as *const _;
    let alias1 = index["evidence-packs"] as *const _;
    let alias2 = index["evidence pack"] as *const _;
    assert!(
        std::ptr::eq(canonical, alias1),
        "alias1 should point at same entry as canonical"
    );
    assert!(
        std::ptr::eq(canonical, alias2),
        "alias2 should point at same entry as canonical"
    );
}

#[test]
fn cross_bounded_context_alias_overlap_is_allowed() {
    // Two entries in DIFFERENT bounded_contexts may share an alias.
    let cfg = config_with_lexicon(json!({
        "risk_deployment": {
            "definition_short": "Risk of deploy failure.",
            "bounded_context": "deployment",
            "aliases": ["deployment-risk"]
        },
        "risk_billing": {
            "definition_short": "Risk of billing interruption.",
            "bounded_context": "billing",
            "aliases": ["billing-risk"]
        }
    }));
    assert!(validate_lexicon(&cfg).is_ok());
}

#[test]
fn same_bounded_context_alias_collides_with_another_terms_canonical_name() {
    // "blackboard" has alias "evidence-pack" — collides with term "evidence-pack".
    let cfg = config_with_lexicon(json!({
        "evidence-pack": {
            "definition_short": "Proves a claim.",
            "bounded_context": "swe-agent"
        },
        "blackboard": {
            "definition_short": "Shared state store.",
            "bounded_context": "swe-agent",
            "aliases": ["evidence-pack"]
        }
    }));
    let err = validate_lexicon(&cfg).expect_err("must reject alias-term collision");
    let msg = format!("{err:?}");
    assert!(msg.contains("LEXICON_ALIAS_COLLISION"), "got: {msg}");
}

#[test]
fn same_bounded_context_alias_collides_with_another_alias() {
    // X has alias "foo", Y has alias "foo" — alias-alias collision.
    let cfg = config_with_lexicon(json!({
        "x": {
            "definition_short": "Term X.",
            "bounded_context": "swe-agent",
            "aliases": ["foo"]
        },
        "y": {
            "definition_short": "Term Y.",
            "bounded_context": "swe-agent",
            "aliases": ["foo"]
        }
    }));
    let err = validate_lexicon(&cfg).expect_err("must reject alias-alias collision");
    let msg = format!("{err:?}");
    assert!(msg.contains("LEXICON_ALIAS_COLLISION"), "got: {msg}");
}

// ── SPEC §30.10.3 — PENDING_DEFINITION placeholders ───────────────────────

/// Helper: build a config where the scripts block references a subject
/// whose name, after stripping the first verb segment, is `subject_name`.
fn config_with_script_subject(
    script_key: &str,
    subject_lexicon_key: Option<&str>,
) -> serde_json::Value {
    let mut lexicon = serde_json::Map::new();
    if let Some(key) = subject_lexicon_key {
        lexicon.insert(
            key.to_string(),
            json!({ "definition_short": "A real entry." }),
        );
    }
    json!({
        "version": "1.0.0",
        "praxec": { "strict_namespacing": false },
        "lexicon": lexicon,
        "scripts": {
            script_key: {
                "verb": "build",
                "lifecycle": "experimental",
                "body": "#!/usr/bin/env bash\necho hi\n"
            }
        },
        "workflows": {
            "demo": {
                "initialState": "idle",
                "states": { "idle": { "terminal": true } }
            }
        }
    })
}

#[test]
fn unregistered_script_subject_creates_pending_placeholder() {
    // A workflow executor *references* subject "evidence-foo" (via a kind=script
    // executor) but NOTHING defines it — no script/skill/cap, not in the lexicon.
    // That genuinely-unknown vocabulary must get a PENDING_DEFINITION placeholder.
    // (Per the §30.10.3 relaxation, a *defined* script subject would resolve
    // itself; only an undefined reference is pending.)
    let cfg = json!({
        "version": "1.0.0",
        "praxec": { "strict_namespacing": false },
        "lexicon": {},
        "workflows": {
            "demo": {
                "initialState": "idle",
                "states": {
                    "idle": {
                        "transitions": {
                            "go": {
                                "target": "done",
                                "executor": { "kind": "script", "subject": "build.evidence-foo" }
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });
    let resolved = resolve(cfg).expect("resolve must succeed");
    let lib = resolved
        .pointer("/workflows/demo/_lexiconLibrary")
        .expect("_lexiconLibrary must be stamped");
    let entry = lib
        .get("evidence-foo")
        .expect("pending placeholder must exist for evidence-foo");
    assert_eq!(
        entry.get("state").and_then(|v| v.as_str()),
        Some("PENDING_DEFINITION"),
        "entry must have state=PENDING_DEFINITION; got: {entry}"
    );
}

#[test]
fn registered_subject_does_not_get_placeholder() {
    // lexicon has "evidence-foo" AND scripts references it.
    // No placeholder created — real entry should be there.
    let cfg = config_with_script_subject("build.evidence-foo", Some("evidence-foo"));
    let resolved = resolve(cfg).expect("resolve must succeed");
    let lib = resolved
        .pointer("/workflows/demo/_lexiconLibrary")
        .expect("_lexiconLibrary must be stamped");
    let entry = lib.get("evidence-foo").expect("entry must exist");
    // Real entry has definition_short, not state=pending_definition.
    assert_ne!(
        entry.get("state").and_then(|v| v.as_str()),
        Some("PENDING_DEFINITION"),
        "real entry must not have state=PENDING_DEFINITION"
    );
    assert!(
        entry.get("definition_short").is_some(),
        "real entry must carry definition_short"
    );
}

#[test]
fn multiple_unresolved_subjects_each_get_placeholder() {
    // Two workflow executors reference two undefined subjects (neither defined
    // as a script/skill/cap nor lexicon-authored) — each genuinely-unknown
    // subject gets its own PENDING_DEFINITION placeholder.
    let cfg = json!({
        "version": "1.0.0",
        "praxec": { "strict_namespacing": false },
        "lexicon": {},
        "workflows": {
            "demo": {
                "initialState": "a",
                "states": {
                    "a": { "transitions": { "go": {
                        "target": "b",
                        "executor": { "kind": "script", "subject": "build.alpha-thing" }
                    } } },
                    "b": { "transitions": { "go": {
                        "target": "done",
                        "executor": { "kind": "script", "subject": "build.beta-thing" }
                    } } },
                    "done": { "terminal": true }
                }
            }
        }
    });
    let resolved = resolve(cfg).expect("resolve must succeed");
    let lib = resolved
        .pointer("/workflows/demo/_lexiconLibrary")
        .expect("_lexiconLibrary must be stamped");
    let alpha = lib
        .get("alpha-thing")
        .expect("placeholder for alpha-thing must exist");
    let beta = lib
        .get("beta-thing")
        .expect("placeholder for beta-thing must exist");
    assert_eq!(
        alpha.get("state").and_then(|v| v.as_str()),
        Some("PENDING_DEFINITION")
    );
    assert_eq!(
        beta.get("state").and_then(|v| v.as_str()),
        Some("PENDING_DEFINITION")
    );
}

#[test]
fn unregistered_skill_subject_creates_pending_placeholder() {
    // A workflow executor references skill subject "my-feature" but no skill
    // defines it and it is not lexicon-authored → PENDING_DEFINITION placeholder.
    let cfg = json!({
        "version": "1.0.0",
        "praxec": { "strict_namespacing": false },
        "lexicon": {},
        "workflows": {
            "demo": {
                "initialState": "idle",
                "states": {
                    "idle": { "transitions": { "go": {
                        "target": "done",
                        "executor": { "kind": "skill", "subject": "plan.my-feature" }
                    } } },
                    "done": { "terminal": true }
                }
            }
        }
    });
    let resolved = resolve(cfg).expect("resolve must succeed");
    let lib = resolved
        .pointer("/workflows/demo/_lexiconLibrary")
        .expect("_lexiconLibrary must be stamped");
    let entry = lib
        .get("my-feature")
        .expect("pending placeholder must exist for my-feature");
    assert_eq!(
        entry.get("state").and_then(|v| v.as_str()),
        Some("PENDING_DEFINITION"),
        "skill subject 'my-feature' must have a PENDING_DEFINITION placeholder"
    );
}

#[test]
fn a_loaded_definition_subject_resolves_without_a_placeholder() {
    // SPEC §30.10.3 relaxation (commit 900b2f2): a subject that is itself DEFINED
    // as a script/skill/capability is resolved by that definition and must NOT be
    // flagged PENDING_DEFINITION — even with an empty lexicon. Without this, every
    // loaded script/skill/cap would require a hand-authored glossary entry. This
    // is the behavior the relaxation introduced; it previously had no coverage.
    let cfg = config_with_script_subject("build.evidence-foo", None);
    let resolved = resolve(cfg).expect("resolve must succeed");
    let state = resolved
        .pointer("/workflows/demo/_lexiconLibrary")
        .and_then(|lib| lib.get("evidence-foo"))
        .and_then(|entry| entry.get("state"))
        .and_then(|v| v.as_str());
    assert_ne!(
        state,
        Some("PENDING_DEFINITION"),
        "a loaded script subject must resolve itself, not be flagged pending"
    );
}
