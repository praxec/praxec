//! SPEC §28 — slot constraint tests.
//!
//! FMECA atomic assertions for the path_allowlist + subset_of constraint
//! kinds, load-time validation, and write-time rejection at the
//! transition boundary.

use praxec_core::slot_constraint::{evaluate_constraints, validate_constraints_in_definition};
use serde_json::json;

// ── path_allowlist runtime evaluation ─────────────────────────────────────

#[test]
fn path_allowlist_passes_when_all_paths_match_allow() {
    let definition = json!({
        "blackboard": {
            "changed_files": {
                "type": "array",
                "constraint": {
                    "path_allowlist": {
                        "allow": ["auth/**", "tests/auth/**"]
                    }
                }
            }
        }
    });
    let context = json!({
        "changed_files": [
            "auth/middleware.rs",
            "auth/session.rs",
            "tests/auth/session_test.rs"
        ]
    });
    assert!(evaluate_constraints(&definition, "s", &context).is_ok());
}

#[test]
fn path_allowlist_rejects_path_outside_allow() {
    let definition = json!({
        "blackboard": {
            "changed_files": {
                "type": "array",
                "constraint": {
                    "path_allowlist": {
                        "allow": ["auth/**"]
                    }
                }
            }
        }
    });
    let context = json!({
        "changed_files": ["auth/session.rs", "db/migrations/0001.sql"]
    });
    let v = evaluate_constraints(&definition, "s", &context).expect_err("must reject");
    assert_eq!(v.slot, "changed_files");
    assert_eq!(v.constraint_kind, "path_allowlist");
    assert!(
        v.message.contains("db/migrations/0001.sql"),
        "got: {}",
        v.message
    );
    assert!(v.message.contains("SLOT_CONSTRAINT_VIOLATED"));
}

#[test]
fn path_allowlist_deny_overrides_allow_for_excluded_subtree() {
    let definition = json!({
        "blackboard": {
            "changed_files": {
                "type": "array",
                "constraint": {
                    "path_allowlist": {
                        "allow": ["auth/**"],
                        "deny":  ["auth/legacy/**"]
                    }
                }
            }
        }
    });
    let context = json!({
        "changed_files": ["auth/session.rs", "auth/legacy/v1.rs"]
    });
    let v = evaluate_constraints(&definition, "s", &context).expect_err("deny must reject");
    assert!(
        v.message.contains("auth/legacy/v1.rs"),
        "got: {}",
        v.message
    );
    assert!(v.message.contains("`deny:`"), "got: {}", v.message);
}

#[test]
fn path_allowlist_rejects_empty_allow_at_runtime() {
    // Defense-in-depth — load-time validation should reject empty
    // allow too, but the runtime evaluator must not silently
    // pass-everything if a malformed config slipped through.
    let definition = json!({
        "blackboard": {
            "changed_files": {
                "type": "array",
                "constraint": {
                    "path_allowlist": { "allow": [] }
                }
            }
        }
    });
    let context = json!({ "changed_files": ["anything.rs"] });
    let v = evaluate_constraints(&definition, "s", &context).expect_err("must reject");
    assert!(v.message.contains("empty `allow:`"), "got: {}", v.message);
}

#[test]
fn path_allowlist_rejects_non_array_slot_value() {
    let definition = json!({
        "blackboard": {
            "changed_files": {
                "constraint": {
                    "path_allowlist": { "allow": ["**"] }
                }
            }
        }
    });
    let context = json!({ "changed_files": "auth/session.rs" }); // string, not array
    let v = evaluate_constraints(&definition, "s", &context).expect_err("must reject");
    assert!(v.message.contains("not an array"), "got: {}", v.message);
}

#[test]
fn path_allowlist_passes_with_no_constraint_block() {
    let definition = json!({
        "blackboard": {
            "changed_files": { "type": "array" }
        }
    });
    let context = json!({ "changed_files": ["anywhere/at/all.rs"] });
    assert!(evaluate_constraints(&definition, "s", &context).is_ok());
}

// ── subset_of runtime evaluation ──────────────────────────────────────────

#[test]
fn subset_of_passes_when_all_elements_in_reference() {
    let definition = json!({
        "blackboard": {
            "active_features": {
                "type": "array",
                "constraint": { "subset_of": "$.context.declared_features" }
            },
            "declared_features": { "type": "array" }
        }
    });
    let context = json!({
        "declared_features": ["auth", "billing", "search", "admin"],
        "active_features":   ["auth", "billing"]
    });
    assert!(evaluate_constraints(&definition, "s", &context).is_ok());
}

#[test]
fn subset_of_rejects_element_not_in_reference() {
    let definition = json!({
        "blackboard": {
            "active_features": {
                "type": "array",
                "constraint": { "subset_of": "$.context.declared_features" }
            },
            "declared_features": { "type": "array" }
        }
    });
    let context = json!({
        "declared_features": ["auth", "billing"],
        "active_features":   ["auth", "experimental_thing"]
    });
    let v = evaluate_constraints(&definition, "s", &context).expect_err("must reject");
    assert!(
        v.message.contains("experimental_thing"),
        "got: {}",
        v.message
    );
    assert!(v.message.contains("not present in the reference array"));
}

#[test]
fn subset_of_fail_fast_when_reference_unset() {
    let definition = json!({
        "blackboard": {
            "active_features": {
                "type": "array",
                "constraint": { "subset_of": "$.context.declared_features" }
            }
        }
    });
    let context = json!({ "active_features": ["auth"] }); // no declared_features
    let v = evaluate_constraints(&definition, "s", &context).expect_err("must reject");
    assert!(
        v.message.contains("unset / unresolvable"),
        "got: {}",
        v.message
    );
}

// ── load-time constraint declaration validation ───────────────────────────

#[test]
fn load_time_rejects_empty_allow() {
    let workflow = json!({
        "blackboard": {
            "x": { "constraint": { "path_allowlist": { "allow": [] } } }
        }
    });
    let err = validate_constraints_in_definition(&workflow).expect_err("empty allow must reject");
    let s = format!("{err:?}");
    assert!(
        s.contains("INVALID_CONSTRAINT_DECLARATION") && s.contains("empty"),
        "got: {s}"
    );
}

#[test]
fn load_time_rejects_malformed_glob() {
    let workflow = json!({
        "blackboard": {
            "x": {
                "constraint": {
                    "path_allowlist": { "allow": ["["] }  // unclosed bracket
                }
            }
        }
    });
    let err =
        validate_constraints_in_definition(&workflow).expect_err("malformed glob must reject");
    let s = format!("{err:?}");
    assert!(
        s.contains("INVALID_CONSTRAINT_DECLARATION") && s.contains("glob"),
        "got: {s}"
    );
}

#[test]
fn load_time_rejects_unknown_constraint_kind() {
    let workflow = json!({
        "blackboard": {
            "x": { "constraint": { "wat": "value" } }
        }
    });
    let err = validate_constraints_in_definition(&workflow).expect_err("unknown kind must reject");
    let s = format!("{err:?}");
    assert!(
        s.contains("unknown constraint kind") && s.contains("wat"),
        "got: {s}"
    );
}

#[test]
fn load_time_rejects_subset_of_with_non_string_value() {
    let workflow = json!({
        "blackboard": {
            "x": { "constraint": { "subset_of": ["not", "a", "path"] } }
        }
    });
    let err =
        validate_constraints_in_definition(&workflow).expect_err("subset_of array must reject");
    let s = format!("{err:?}");
    assert!(s.contains("subset_of must be a string"), "got: {s}");
}

#[test]
fn load_time_accepts_well_formed_constraints() {
    let workflow = json!({
        "blackboard": {
            "changed_files": {
                "type": "array",
                "constraint": {
                    "path_allowlist": {
                        "allow": ["src/**", "tests/**"],
                        "deny":  ["src/generated/**"]
                    }
                }
            },
            "active_features": {
                "type": "array",
                "constraint": { "subset_of": "$.context.declared_features" }
            }
        }
    });
    assert!(validate_constraints_in_definition(&workflow).is_ok());
}

// ── state-local slot constraint evaluation ────────────────────────────────

#[test]
fn state_local_slot_constraint_is_evaluated_when_in_that_state() {
    let definition = json!({
        "states": {
            "editing": {
                "slots": {
                    "edited_files": {
                        "type": "array",
                        "scope": "state",
                        "constraint": {
                            "path_allowlist": { "allow": ["src/auth/**"] }
                        }
                    }
                }
            }
        }
    });
    let context = json!({ "edited_files": ["src/auth/login.rs"] });
    assert!(evaluate_constraints(&definition, "editing", &context).is_ok());

    let bad_ctx = json!({ "edited_files": ["src/db/migrations/0001.sql"] });
    let v = evaluate_constraints(&definition, "editing", &bad_ctx).expect_err("must reject");
    assert_eq!(v.slot, "edited_files");
}

// ── composition: multiple constraint kinds on one slot ────────────────────

#[test]
fn multiple_constraint_kinds_compose_conjunctively() {
    let definition = json!({
        "blackboard": {
            "active": {
                "type": "array",
                "constraint": {
                    "path_allowlist": { "allow": ["modules/**"] },
                    "subset_of":     "$.context.enabled"
                }
            },
            "enabled": { "type": "array" }
        }
    });
    let context = json!({
        "enabled": ["modules/a", "modules/b", "modules/c"],
        "active":  ["modules/a"]                     // passes both
    });
    assert!(evaluate_constraints(&definition, "s", &context).is_ok());

    let bad_ctx = json!({
        "enabled": ["modules/a", "modules/b"],
        "active":  ["modules/a", "modules/c"]        // passes glob, fails subset
    });
    let v = evaluate_constraints(&definition, "s", &bad_ctx).expect_err("must reject");
    // Order: path_allowlist gets checked first via BTreeMap iteration,
    // so 'modules/c' (matches glob) passes there, then subset_of rejects.
    assert!(
        v.message.contains("modules/c"),
        "expected modules/c rejection; got: {}",
        v.message
    );
}
