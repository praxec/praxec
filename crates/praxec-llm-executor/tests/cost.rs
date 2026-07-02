//! SPEC §33 D8 — cost catalog public-surface tests.
//!
//! Unit coverage in `src/cost.rs` pins a few internal invariants
//! (parseable `verified_at`, zero-token math, no-cap-unknown is `Ok`);
//! this file exercises the load-time rejection paths the brief calls
//! out as the FMECA F8 mitigation — including the standalone doctor
//! helper.

use chrono::NaiveDate;
use praxec_core::validate::Diagnostic;
use praxec_llm_executor::cost::{
    compute_cost_usd, doctor_check, lookup, validate_for_workflow, CostCatalogError,
    COST_CATALOG_MISSING_ENTRY, COST_CATALOG_STALE, LAST_VERIFIED, STALENESS_THRESHOLD_DAYS,
};
use serde_json::json;

fn today() -> NaiveDate {
    // Pinned reference date matches the shipped LAST_VERIFIED so
    // freshness math is deterministic in tests.
    NaiveDate::from_ymd_opt(2026, 5, 29).expect("static date")
}

#[test]
fn lookup_finds_anthropic_sonnet() {
    let entry =
        lookup("anthropic:claude-sonnet-4-6").expect("anthropic:claude-sonnet-4-6 must be shipped");
    assert!(entry.input_usd_per_million_tokens > 0.0);
    assert!(entry.output_usd_per_million_tokens > 0.0);
    assert!(entry.verified_at_date().is_some());
}

#[test]
fn lookup_returns_missing_for_unknown_model() {
    let err = lookup("vendor:fake-model").expect_err("unknown model must surface Missing");
    match err {
        CostCatalogError::Missing { model } => {
            assert_eq!(model, "vendor:fake-model");
        }
        other => panic!("expected Missing, got {other:?}"),
    }
}

#[test]
fn compute_cost_usd_multiplies_correctly() {
    // Sonnet ships at $3.00 / $15.00 per million.
    // 1000 in + 1000 out => 1000 * 3e-6 + 1000 * 15e-6 = 0.003 + 0.015 = 0.018
    let cost =
        compute_cost_usd("anthropic:claude-sonnet-4-6", 1000, 1000).expect("known model must cost");
    let expected = 0.018_f64;
    assert!(
        (cost - expected).abs() < 1e-9,
        "expected {expected}, got {cost}"
    );
}

#[test]
fn compute_cost_usd_returns_missing_for_unknown_model() {
    let err = compute_cost_usd("vendor:fake-model", 100, 100)
        .expect_err("unknown model must propagate Missing");
    assert!(matches!(err, CostCatalogError::Missing { .. }));
}

#[test]
fn validate_for_workflow_passes_for_known_fresh_model() {
    validate_for_workflow("anthropic:claude-sonnet-4-6", true, today())
        .expect("fresh known model with budget cap must pass");
}

#[test]
fn validate_for_workflow_rejects_unknown_with_budget_cap() {
    let err = validate_for_workflow("vendor:fake-model", true, today())
        .expect_err("unknown model + cap must be rejected");
    assert!(matches!(err, CostCatalogError::Missing { .. }));
}

#[test]
fn validate_for_workflow_warns_unknown_without_budget_cap() {
    // The bare validate function returns Ok in the no-cap case; the
    // Warning is emitted at the doctor_check layer.
    validate_for_workflow("vendor:fake-model", false, today())
        .expect("unknown model without budget cap must pass validate");

    // The doctor walker is where the operator-facing Warning surfaces.
    let registry = json!({
        "workflows": {
            "flow.demo": {
                "states": {
                    "thinking": {
                        "transitions": {
                            "advance": {
                                "target": "done",
                                "executor": {
                                    "kind": "llm",
                                    "config": {
                                        "model": "vendor:fake-model",
                                        "prompt_template": "x"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    });
    let diags = doctor_check(&registry, today(), None);
    let warnings: Vec<&Diagnostic> = diags
        .iter()
        .filter(|d| matches!(d, Diagnostic::Warning(_)))
        .collect();
    assert_eq!(
        warnings.len(),
        1,
        "expected exactly one Warning for unknown model w/o cap, got {diags:?}"
    );
    assert!(!diags.iter().any(Diagnostic::is_error));
}

#[test]
fn validate_for_workflow_rejects_stale_with_budget_cap() {
    // The shipped sonnet entry was verified on `today()`. Drive the
    // clock forward past the staleness threshold to trigger the gate.
    let future = today() + chrono::Duration::days(STALENESS_THRESHOLD_DAYS + 1);
    let err = validate_for_workflow("anthropic:claude-sonnet-4-6", true, future)
        .expect_err("stale entry + cap must be rejected");
    match err {
        CostCatalogError::Stale {
            model,
            verified_at,
            threshold_days,
        } => {
            assert_eq!(model, "anthropic:claude-sonnet-4-6");
            assert_eq!(threshold_days, STALENESS_THRESHOLD_DAYS);
            assert!(
                NaiveDate::parse_from_str(&verified_at, "%Y-%m-%d").is_ok(),
                "verified_at should be ISO 8601"
            );
        }
        other => panic!("expected Stale, got {other:?}"),
    }
}

#[test]
fn validate_for_workflow_warns_stale_without_budget_cap() {
    let future = today() + chrono::Duration::days(STALENESS_THRESHOLD_DAYS + 1);
    validate_for_workflow("anthropic:claude-sonnet-4-6", false, future)
        .expect("stale entry without budget cap must pass validate");

    let registry = json!({
        "workflows": {
            "flow.demo": {
                "states": {
                    "thinking": {
                        "transitions": {
                            "advance": {
                                "target": "done",
                                "executor": {
                                    "kind": "llm",
                                    "config": {
                                        "model": "anthropic:claude-sonnet-4-6",
                                        "prompt_template": "x"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    });
    let diags = doctor_check(&registry, future, None);
    let warnings: Vec<&Diagnostic> = diags
        .iter()
        .filter(|d| matches!(d, Diagnostic::Warning(_)))
        .collect();
    assert_eq!(
        warnings.len(),
        1,
        "expected exactly one Warning for stale entry w/o cap, got {diags:?}"
    );
    assert!(!diags.iter().any(Diagnostic::is_error));
}

#[test]
fn catalog_last_verified_is_recent() {
    // Sanity check on the catalog itself: the shipped `LAST_VERIFIED`
    // constant must be within 365 days of test compile time. Catches
    // a release that forgets to refresh the catalog and ships a year+
    // out-of-date pricing table.
    let last = NaiveDate::parse_from_str(LAST_VERIFIED, "%Y-%m-%d")
        .expect("LAST_VERIFIED must be ISO 8601");
    let now = chrono::Utc::now().date_naive();
    let age = now.signed_duration_since(last).num_days();
    assert!(
        age <= 365,
        "LAST_VERIFIED ({LAST_VERIFIED}) is {age} days old; refresh the catalog"
    );
}

#[test]
fn doctor_check_emits_error_with_wire_code_for_missing_with_cap() {
    let registry = json!({
        "workflows": {
            "flow.demo": {
                "states": {
                    "thinking": {
                        "transitions": {
                            "advance": {
                                "target": "done",
                                "executor": {
                                    "kind": "llm",
                                    "config": {
                                        "model": "vendor:fake-model",
                                        "prompt_template": "x",
                                        "max_cost_usd": 1.0
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    });
    let diags = doctor_check(&registry, today(), None);
    let errors: Vec<&Diagnostic> = diags.iter().filter(|d| d.is_error()).collect();
    assert_eq!(errors.len(), 1, "expected exactly one Error, got {diags:?}");
    let msg = errors[0].message();
    assert!(
        msg.contains(COST_CATALOG_MISSING_ENTRY),
        "error must carry wire code {COST_CATALOG_MISSING_ENTRY}: got {msg}"
    );
    assert!(msg.contains("vendor:fake-model"));
}

#[test]
fn doctor_check_emits_error_with_wire_code_for_stale_with_cap() {
    let future = today() + chrono::Duration::days(STALENESS_THRESHOLD_DAYS + 1);
    let registry = json!({
        "workflows": {
            "flow.demo": {
                "states": {
                    "thinking": {
                        "transitions": {
                            "advance": {
                                "target": "done",
                                "executor": {
                                    "kind": "llm",
                                    "config": {
                                        "model": "anthropic:claude-sonnet-4-6",
                                        "prompt_template": "x",
                                        "max_cost_usd": 1.0
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    });
    let diags = doctor_check(&registry, future, None);
    let errors: Vec<&Diagnostic> = diags.iter().filter(|d| d.is_error()).collect();
    assert_eq!(errors.len(), 1, "expected exactly one Error, got {diags:?}");
    let msg = errors[0].message();
    assert!(
        msg.contains(COST_CATALOG_STALE),
        "error must carry wire code {COST_CATALOG_STALE}: got {msg}"
    );
}

#[test]
fn doctor_check_skips_non_llm_executor_kinds() {
    let registry = json!({
        "workflows": {
            "flow.demo": {
                "states": {
                    "thinking": {
                        "transitions": {
                            "advance": {
                                "target": "done",
                                "executor": {
                                    "kind": "noop"
                                }
                            }
                        }
                    }
                }
            }
        }
    });
    let diags = doctor_check(&registry, today(), None);
    assert!(
        diags.is_empty(),
        "non-llm executors must not produce cost diagnostics: {diags:?}"
    );
}

/// SPEC §33 audit fixup (F6 STUB-009): affinity-only configs with
/// `max_cost_usd` set used to be silently skipped by the load-time
/// gate, giving operators false confidence that the cap was
/// validated. Doctor now emits a `Diagnostic::Warning` naming the gap
/// so the operator sees a clear signal that the cap relies on
/// runtime enforcement until the models.yaml resolver lands.
#[test]
fn doctor_check_warns_on_affinity_only_with_budget_cap() {
    let registry = json!({
        "workflows": {
            "flow.demo": {
                "states": {
                    "thinking": {
                        "transitions": {
                            "advance": {
                                "target": "done",
                                "executor": {
                                    "kind": "llm",
                                    "config": {
                                        "affinity": "reasoning-heavy",
                                        "prompt_template": "x",
                                        "max_cost_usd": 1.0
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    });
    let diags = doctor_check(&registry, today(), None);
    let errors = diags.iter().filter(|d| d.is_error()).count();
    let warnings = diags.iter().filter(|d| !d.is_error()).count();
    assert_eq!(
        errors, 0,
        "affinity-only must NOT produce a load-time Error: {diags:?}"
    );
    assert_eq!(
        warnings, 1,
        "affinity-only + max_cost_usd must produce exactly one Warning: {diags:?}"
    );
    let msg = diags[0].message();
    assert!(
        msg.contains("affinity") && msg.contains("max_cost_usd"),
        "warning must name both the affinity path and the cap: {msg}"
    );
}

#[test]
fn doctor_check_skips_affinity_only_without_budget_cap() {
    // No `max_cost_usd` set → nothing to false-confidence about.
    // Doctor stays silent so authors don't see a spurious warning
    // on every affinity-using workflow.
    let registry = json!({
        "workflows": {
            "flow.demo": {
                "states": {
                    "thinking": {
                        "transitions": {
                            "advance": {
                                "target": "done",
                                "executor": {
                                    "kind": "llm",
                                    "config": {
                                        "affinity": "reasoning-heavy",
                                        "prompt_template": "x"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    });
    let diags = doctor_check(&registry, today(), None);
    assert!(
        diags.is_empty(),
        "affinity-only without budget cap must stay silent: {diags:?}"
    );
}

/// SPEC §33 D9 — once an affinity resolver is available at load time
/// (the binary builds a SYNC closure off models.yaml), an affinity that
/// resolves to a model NOT in the cost catalog, under a `max_cost_usd`
/// cap, must produce the SAME catalog Error the literal-`model:` path
/// produces — not a soft Warning. This is the F8 budget-cap guarantee
/// extended to affinity-resolved models.
#[test]
fn affinity_resolved_uncatalogued_model_with_cap_errors() {
    let registry = json!({
        "workflows": {
            "flow.demo": {
                "states": {
                    "thinking": {
                        "transitions": {
                            "advance": {
                                "target": "done",
                                "executor": {
                                    "kind": "llm",
                                    "config": {
                                        "affinity": "x",
                                        "prompt_template": "p",
                                        "max_cost_usd": 1.0
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    });
    let resolve = |a: &str| (a == "x").then(|| "anthropic:totally-made-up-model".to_string());
    let diags = doctor_check(&registry, today(), Some(&resolve));
    let errors: Vec<&Diagnostic> = diags.iter().filter(|d| d.is_error()).collect();
    assert_eq!(
        errors.len(),
        1,
        "affinity-resolved uncatalogued model + cap must Error exactly once: {diags:?}"
    );
    let msg = errors[0].message();
    assert!(
        msg.contains(COST_CATALOG_MISSING_ENTRY),
        "error must carry the same wire code as the literal-model path: {msg}"
    );
    assert!(
        msg.contains("anthropic:totally-made-up-model"),
        "error must name the RESOLVED model, not the affinity string: {msg}"
    );
}

/// Companion to the above: when the closure declines to resolve (returns
/// `None`, e.g. models.yaml has no matching delegate), the doctor falls
/// back to the existing warn-only behavior — load is not blocked, the
/// runtime F8 path still enforces.
#[test]
fn affinity_unresolved_by_closure_only_warns() {
    let registry = json!({
        "workflows": {
            "flow.demo": {
                "states": {
                    "thinking": {
                        "transitions": {
                            "advance": {
                                "target": "done",
                                "executor": {
                                    "kind": "llm",
                                    "config": {
                                        "affinity": "y",
                                        "prompt_template": "p",
                                        "max_cost_usd": 1.0
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    });
    // Closure resolves "x" only — "y" returns None.
    let resolve = |a: &str| (a == "x").then(|| "anthropic:totally-made-up-model".to_string());
    let diags = doctor_check(&registry, today(), Some(&resolve));
    let errors = diags.iter().filter(|d| d.is_error()).count();
    let warnings = diags.iter().filter(|d| !d.is_error()).count();
    assert_eq!(errors, 0, "unresolved affinity must NOT Error: {diags:?}");
    assert_eq!(
        warnings, 1,
        "unresolved affinity + cap must keep the warn-only behavior: {diags:?}"
    );
}
