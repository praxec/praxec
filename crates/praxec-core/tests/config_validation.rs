//! Config validation tests — version field requirements, etc.

use praxec_core::config;
use praxec_core::validate::validate_workflows;
use serde_json::{json, Value};

#[test]
fn valid_config_with_version_field() {
    let yaml = r#"
version: "1.0.0"
proxy:
  expose:
    - name: echo
      executor: { kind: noop }
"#;
    let result = config::resolve_str(yaml);
    assert!(result.is_ok(), "config with version field should parse OK");
}

#[test]
fn config_without_version_field_still_parses() {
    // The resolver doesn't require version; the `check` subcommand does.
    // This test verifies that resolve_str doesn't reject it — the
    // requirement is at the binary layer.
    let yaml = r#"
proxy:
  expose:
    - name: echo
      executor: { kind: noop }
"#;
    let result = config::resolve_str(yaml);
    assert!(
        result.is_ok(),
        "config without version should still resolve"
    );
}

// ── Blackboard slot-check tests ──────────────────────────────────────────────

#[test]
fn undeclared_output_slot_warns() {
    let config = json!({
        "workflows": {
            "ci": {
                "initialState": "lint",
                "blackboard": ["lintPassed"],
                "states": {
                    "lint": {
                        "transitions": {
                            "done": {
                                "target": "finish",
                                "output": { "typo": "$.result.value" }
                            }
                        }
                    },
                    "finish": { "terminal": true }
                }
            }
        }
    });
    let diags = validate_workflows(&config);
    let warnings: Vec<_> = diags
        .iter()
        .filter(|d| {
            !d.is_error() && d.message().contains("typo") && d.message().contains("blackboard")
        })
        .collect();
    assert!(
        !warnings.is_empty(),
        "expected a warning naming 'typo' and referencing 'blackboard', got: {diags:?}"
    );
}

#[test]
fn undeclared_output_slot_warns_object_form() {
    // blackboard declared as object form { "lintPassed": {} }; transition writes
    // undeclared key "typo" — should produce the same warning as the array form.
    let config = json!({
        "workflows": {
            "ci": {
                "initialState": "lint",
                "blackboard": { "lintPassed": {} },
                "states": {
                    "lint": {
                        "transitions": {
                            "done": {
                                "target": "finish",
                                "output": { "typo": "$.result.value" }
                            }
                        }
                    },
                    "finish": { "terminal": true }
                }
            }
        }
    });
    let diags = validate_workflows(&config);
    let warnings: Vec<_> = diags
        .iter()
        .filter(|d| {
            !d.is_error() && d.message().contains("typo") && d.message().contains("blackboard")
        })
        .collect();
    assert!(
        !warnings.is_empty(),
        "expected a warning naming 'typo' and referencing 'blackboard' (object-form blackboard), got: {diags:?}"
    );
}

// ── SPEC §33 D6 — reserved-prefix enforcement ──────────────────────────────

#[test]
fn blackboard_slot_with_reserved_llm_prefix_is_rejected() {
    // SPEC §33 D6 — `_llm.*` is owned by the in-runtime LLM executor's
    // cumulative-cap bookkeeping. A user-declared slot in that namespace
    // would shadow runtime state.
    let config = json!({
        "workflows": {
            "ci": {
                "initialState": "lint",
                "blackboard": { "_llm.foo": { "type": "integer" } },
                "states": {
                    "lint": { "terminal": true }
                }
            }
        }
    });
    let diags = validate_workflows(&config);
    let errors: Vec<_> = diags
        .iter()
        .filter(|d| d.is_error() && d.message().contains("_llm.foo"))
        .collect();
    assert!(
        !errors.is_empty(),
        "expected an error naming the reserved `_llm.` slot, got: {diags:?}"
    );
}

#[test]
fn blackboard_array_slot_with_reserved_fire_count_prefix_is_rejected() {
    // Coverage for the array-form blackboard and for the `_fire_count.*`
    // legacy synthetic prefix (SPEC §29) — D6 extended the same check
    // to cover both.
    let config = json!({
        "workflows": {
            "ci": {
                "initialState": "lint",
                "blackboard": ["_fire_count.lint.bad"],
                "states": {
                    "lint": { "terminal": true }
                }
            }
        }
    });
    let diags = validate_workflows(&config);
    let errors: Vec<_> = diags
        .iter()
        .filter(|d| d.is_error() && d.message().contains("_fire_count.lint.bad"))
        .collect();
    assert!(
        !errors.is_empty(),
        "expected an error naming the reserved `_fire_count.` slot, got: {diags:?}"
    );
}

#[test]
fn blackboard_slot_with_ordinary_underscore_name_is_accepted() {
    // Defensive: only the *specific* reserved prefixes should fail. A
    // user-declared slot that happens to begin with `_` but isn't in a
    // reserved namespace must pass.
    let config = json!({
        "workflows": {
            "ci": {
                "initialState": "lint",
                "blackboard": ["_private"],
                "states": {
                    "lint": { "terminal": true }
                }
            }
        }
    });
    let diags = validate_workflows(&config);
    let errors: Vec<_> = diags
        .iter()
        .filter(|d| d.is_error() && d.message().contains("_private"))
        .collect();
    assert!(
        errors.is_empty(),
        "ordinary underscore-prefixed slot must not be rejected; got: {diags:?}"
    );
}

#[test]
fn declared_blackboard_accepted() {
    let config = json!({
        "workflows": {
            "ci": {
                "initialState": "lint",
                "blackboard": ["lintPassed"],
                "states": {
                    "lint": {
                        "transitions": {
                            "done": {
                                "target": "finish",
                                "output": { "lintPassed": "$.result.value" }
                            }
                        }
                    },
                    "finish": { "terminal": true }
                }
            }
        }
    });
    let diags = validate_workflows(&config);
    let slot_warnings: Vec<_> = diags
        .iter()
        .filter(|d| !d.is_error() && d.message().contains("blackboard"))
        .collect();
    assert!(
        slot_warnings.is_empty(),
        "expected no blackboard slot warnings for declared key 'lintPassed', got: {slot_warnings:?}"
    );
}

#[test]
fn no_blackboard_declared_no_warning() {
    let config = json!({
        "workflows": {
            "ci": {
                "initialState": "lint",
                "states": {
                    "lint": {
                        "transitions": {
                            "done": {
                                "target": "finish",
                                "output": { "anyKey": "$.result.value" }
                            }
                        }
                    },
                    "finish": { "terminal": true }
                }
            }
        }
    });
    let diags = validate_workflows(&config);
    let slot_warnings: Vec<_> = diags
        .iter()
        .filter(|d| !d.is_error() && d.message().contains("blackboard"))
        .collect();
    assert!(
        slot_warnings.is_empty(),
        "expected no blackboard slot warnings when no blackboard is declared, got: {slot_warnings:?}"
    );
}

// ── Workflow definition version discriminator tests ─────────────────────────

/// A workflow definition with an explicit `version` retains that value after resolution.
#[test]
fn workflow_definition_explicit_version_is_preserved() {
    let yaml = r#"
proxy:
  expose:
    - name: echo
      executor: { kind: noop }
workflows:
  ci:
    version: "2026-05-22"
    initialState: lint
    states:
      lint:
        terminal: true
"#;
    let resolved = config::resolve_str(yaml).expect("should resolve");
    let version = resolved
        .pointer("/workflows/ci/version")
        .and_then(|v| v.as_str())
        .expect("workflows.ci.version should be present");
    assert_eq!(
        version, "2026-05-22",
        "explicit version should be preserved; got: {version:?}"
    );
}

/// A workflow definition with no `version` has `version == "0"` after resolution.
#[test]
fn workflow_definition_missing_version_gets_default() {
    let yaml = r#"
proxy:
  expose:
    - name: echo
      executor: { kind: noop }
workflows:
  ci:
    initialState: lint
    states:
      lint:
        terminal: true
"#;
    let resolved = config::resolve_str(yaml).expect("should resolve");
    let version = resolved
        .pointer("/workflows/ci/version")
        .and_then(|v| v.as_str())
        .expect("workflows.ci.version should be present after resolution");
    assert_eq!(
        version, "0",
        "missing version should default to \"0\"; got: {version:?}"
    );
}

/// Sanity: the top-level config `version` field is unaffected by per-workflow defaulting.
#[test]
fn top_level_config_version_unchanged_after_workflow_defaulting() {
    let yaml = r#"
version: "1.0.0"
proxy:
  expose:
    - name: echo
      executor: { kind: noop }
workflows:
  ci:
    initialState: lint
    states:
      lint:
        terminal: true
"#;
    let resolved = config::resolve_str(yaml).expect("should resolve");
    let top_version = resolved
        .pointer("/version")
        .and_then(|v| v.as_str())
        .expect("top-level version should be present");
    assert_eq!(
        top_version, "1.0.0",
        "top-level config version must be unchanged; got: {top_version:?}"
    );
    // Workflow version should still get its default.
    let wf_version = resolved
        .pointer("/workflows/ci/version")
        .and_then(|v| v.as_str())
        .expect("workflows.ci.version should be present after resolution");
    assert_eq!(wf_version, "0", "workflow version should default to \"0\"");
}

// ── Skills poka-yoke (Phase 5.2, SPEC §5.4) ──────────────────────────────────

#[test]
fn verb_with_space_rejected_at_load() {
    // A `verb` containing whitespace must fail config load — not lint-time.
    // The base token is a valid cognitive verb (`review`) so the failure is
    // unambiguously about the whitespace, not the verb value.
    let yaml = r##"
version: "1.0.0"
skills:
  review.style.house-voice:
    verb: "review now"
    lifecycle: stable
    body: "House voice body"
"##;
    let err = config::resolve_str(yaml).expect_err("verb with space must be rejected at load");
    let msg = format!("{err}");
    assert!(
        msg.contains("review now") && msg.contains("verb"),
        "error should name the offending verb; got: {msg}"
    );
}

#[test]
fn skills_key_with_uppercase_rejected_at_load() {
    let yaml = r##"
version: "1.0.0"
skills:
  HouseVoice:
    verb: review
    lifecycle: stable
    body: "House voice body"
"##;
    let err = config::resolve_str(yaml).expect_err("uppercase skills key must be rejected at load");
    let msg = format!("{err}");
    assert!(
        msg.contains("HouseVoice"),
        "error should name the offending subject key; got: {msg}"
    );
}

// ── Phase 6: `check` use-before-def (SPEC §9, §11) ───────────────────────────

#[test]
fn guard_reading_unwritten_slot_errors() {
    // `$.context.X` referenced by an expr guard with no reachable predecessor
    // writer is a `check` error (SPEC §11: use-before-def → error).
    let config = json!({
        "workflows": {
            "demo": {
                "initialState": "start",
                "blackboard": ["needsApproval"],
                "states": {
                    "start": {
                        "transitions": {
                            "go": {
                                "target": "gate",
                                "guards": [
                                    { "kind": "expr", "expr": "$.context.needsApproval == true" }
                                ]
                            }
                        }
                    },
                    "gate": { "terminal": true }
                }
            }
        }
    });
    let diags = validate_workflows(&config);
    let errors: Vec<_> = diags
        .iter()
        .filter(|d| d.is_error() && d.message().contains("needsApproval"))
        .collect();
    assert!(
        !errors.is_empty(),
        "expected an error naming the unwritten slot 'needsApproval'; got: {diags:?}"
    );
}

#[test]
fn guard_reading_summary_errors() {
    // `$.context.summary` is model-authored content — it is never a guard
    // input. Reading it from an `expr` guard is a `check` error regardless
    // of declared blackboard slots (SPEC §6.3, §11).
    let config = json!({
        "workflows": {
            "demo": {
                "initialState": "start",
                "states": {
                    "start": {
                        "transitions": {
                            "go": {
                                "target": "done",
                                "guards": [
                                    { "kind": "expr", "expr": "$.context.summary == 'ok'" }
                                ]
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });
    let diags = validate_workflows(&config);
    let errors: Vec<_> = diags
        .iter()
        .filter(|d| d.is_error() && d.message().contains("summary"))
        .collect();
    assert!(
        !errors.is_empty(),
        "expected an error naming the forbidden `summary` guard read; got: {diags:?}"
    );
}

#[test]
fn template_unknown_slot_errors() {
    // SPEC §11: use-before-def is an error for guards *and* templates. The
    // runtime renders a stub (§5.2) so the live workflow degrades gracefully,
    // but `check` is the static line of defence and reports this as a
    // fail-fast authoring bug.
    let config = json!({
        "workflows": {
            "demo": {
                "initialState": "start",
                "states": {
                    "start": {
                        "guidance": "Hello {{ $.context.unknownSlot }}",
                        "transitions": {
                            "go": { "target": "done" }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });
    let diags = validate_workflows(&config);
    let errors: Vec<_> = diags
        .iter()
        .filter(|d| d.is_error() && d.message().contains("unknownSlot"))
        .collect();
    assert!(
        !errors.is_empty(),
        "expected an error for template reading the undeclared slot 'unknownSlot'; got: {diags:?}"
    );
}

#[test]
fn guard_reading_slot_with_reachable_writer_clean() {
    // Reachable predecessor writer satisfies use-before-def — no diagnostic.
    let config = json!({
        "workflows": {
            "demo": {
                "initialState": "lint",
                "blackboard": ["lintPassed"],
                "states": {
                    "lint": {
                        "transitions": {
                            "done": {
                                "target": "gate",
                                "output": { "lintPassed": "$.result.value" }
                            }
                        }
                    },
                    "gate": {
                        "transitions": {
                            "deploy": {
                                "target": "deployed",
                                "guards": [
                                    { "kind": "expr", "expr": "$.context.lintPassed == true" }
                                ]
                            }
                        }
                    },
                    "deployed": { "terminal": true }
                }
            }
        }
    });
    let diags = validate_workflows(&config);
    let errors: Vec<_> = diags
        .iter()
        .filter(|d| d.is_error() && d.message().contains("lintPassed"))
        .collect();
    assert!(
        errors.is_empty(),
        "no error expected when slot has a reachable writer; got: {errors:?}"
    );
}

#[test]
fn guard_reading_undeclared_slot_errors_when_blackboard_declared() {
    // SPEC §11: when `blackboard:` is declared, reading a slot not in that
    // declaration is an error on the read side — independent of whether a
    // writer exists. The writer to `b` here triggers a separate
    // "undeclared output" warn; the guard read of `b` must error.
    let config = json!({
        "workflows": {
            "demo": {
                "initialState": "start",
                "blackboard": ["a"],
                "states": {
                    "start": {
                        "transitions": {
                            "go": {
                                "target": "gate",
                                "output": { "b": "$.output.v" }
                            }
                        }
                    },
                    "gate": {
                        "transitions": {
                            "use": {
                                "target": "done",
                                "guards": [
                                    { "kind": "expr", "expr": "$.context.b == 1" }
                                ]
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });
    let diags = validate_workflows(&config);
    let read_side_errors: Vec<_> = diags
        .iter()
        .filter(|d| {
            d.is_error()
                && d.message().contains("not a declared blackboard slot")
                && d.message().contains("$.context.b")
        })
        .collect();
    assert!(
        !read_side_errors.is_empty(),
        "expected a read-side error for guard reading undeclared slot 'b'; got: {diags:?}"
    );
}

#[test]
fn template_reading_undeclared_slot_errors_when_blackboard_declared() {
    let config = json!({
        "workflows": {
            "demo": {
                "initialState": "start",
                "blackboard": ["a"],
                "states": {
                    "start": {
                        "guidance": "Stage is {{ $.context.b }}",
                        "transitions": {
                            "go": { "target": "done" }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });
    let diags = validate_workflows(&config);
    let errors: Vec<_> = diags
        .iter()
        .filter(|d| {
            d.is_error()
                && d.message().contains("not a declared blackboard slot")
                && d.message().contains("$.context.b")
        })
        .collect();
    assert!(
        !errors.is_empty(),
        "expected a read-side error for template reading undeclared slot 'b'; got: {diags:?}"
    );
}

#[test]
fn guard_reading_context_clean_when_blackboard_absent() {
    // No `blackboard:` declared → the SPEC §11 declared-slot read check is
    // skipped (SPEC §14 compatibility). use-before-def still applies on
    // guards. With a reachable writer, no diagnostic is raised.
    let config = json!({
        "workflows": {
            "demo": {
                "initialState": "start",
                "states": {
                    "start": {
                        "transitions": {
                            "go": {
                                "target": "gate",
                                "output": { "anySlot": "$.output.v" }
                            }
                        }
                    },
                    "gate": {
                        "transitions": {
                            "use": {
                                "target": "done",
                                "guards": [
                                    { "kind": "expr", "expr": "$.context.anySlot == true" }
                                ]
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });
    let diags = validate_workflows(&config);
    let errs: Vec<_> = diags.iter().filter(|d| d.is_error()).collect();
    assert!(
        errs.is_empty(),
        "no errors expected without blackboard declaration; got: {errs:?}"
    );
}

#[test]
fn ontimeout_target_inherits_writers_from_any_reachable_state() {
    // SPEC §9: onTimeout fires from any reachable state, so its target
    // should see slots written along ANY reachable path. A guard on a
    // transition leaving the timeout target that reads such a slot must
    // not be flagged as use-before-def.
    let config = json!({
        "workflows": {
            "demo": {
                "initialState": "lint",
                "blackboard": ["lintPassed"],
                "onTimeout": { "target": "timed_out" },
                "states": {
                    "lint": {
                        "transitions": {
                            "done": {
                                "target": "deploy",
                                "output": { "lintPassed": "$.result.value" }
                            }
                        }
                    },
                    "deploy": { "terminal": true },
                    "timed_out": {
                        "transitions": {
                            "review": {
                                "target": "reviewed",
                                "guards": [
                                    { "kind": "expr", "expr": "$.context.lintPassed == true" }
                                ]
                            }
                        }
                    },
                    "reviewed": { "terminal": true }
                }
            }
        }
    });
    let diags = validate_workflows(&config);
    let errors: Vec<_> = diags
        .iter()
        .filter(|d| d.is_error() && d.message().contains("lintPassed"))
        .collect();
    assert!(
        errors.is_empty(),
        "onTimeout target should see writers from reachable predecessors; got: {errors:?}"
    );
}

#[test]
fn dangling_skills_ref_errors() {
    // A `skills:` reference to a subject not in the top-level library → error.
    let config = json!({
        "skills": {
            "review.style.house-voice": { "verb": "review", "lifecycle": "stable", "body": "..." }
        },
        "workflows": {
            "demo": {
                "initialState": "start",
                "skills": ["review.style.does-not-exist"],
                "states": {
                    "start": { "terminal": true }
                }
            }
        }
    });
    let diags = validate_workflows(&config);
    let errors: Vec<_> = diags
        .iter()
        .filter(|d| d.is_error() && d.message().contains("review.style.does-not-exist"))
        .collect();
    assert!(
        !errors.is_empty(),
        "expected an error naming the dangling skills ref; got: {diags:?}"
    );
}

#[test]
fn many_skills_refs_at_one_scope_warns() {
    // More than ~4 refs surfaced at a single scope → warn (the menu is itself
    // payload). SPEC §11.
    let config = json!({
        "skills": {
            "review.style.a": { "verb": "review", "lifecycle": "stable", "body": "..." },
            "review.style.b": { "verb": "review", "lifecycle": "stable", "body": "..." },
            "review.style.c": { "verb": "review", "lifecycle": "stable", "body": "..." },
            "review.style.d": { "verb": "review", "lifecycle": "stable", "body": "..." },
            "review.style.e": { "verb": "review", "lifecycle": "stable", "body": "..." }
        },
        "workflows": {
            "demo": {
                "initialState": "start",
                "skills": ["review.style.a", "review.style.b", "review.style.c", "review.style.d", "review.style.e"],
                "states": {
                    "start": { "terminal": true }
                }
            }
        }
    });
    let diags = validate_workflows(&config);
    let warnings: Vec<_> = diags
        .iter()
        .filter(|d| !d.is_error() && d.message().to_lowercase().contains("skills"))
        .collect();
    assert!(
        !warnings.is_empty(),
        "expected a warning about too many refs at one scope; got: {diags:?}"
    );
}

#[test]
fn well_formed_skills_load_clean() {
    let yaml = r##"
version: "1.0.0"
skills:
  review.style.house-voice:
    verb: review
    lifecycle: stable
    body: "House voice body"
  deploy.safety.checklist:
    verb: review
    lifecycle: stable
    body: "Deploy safety body"
"##;
    let resolved = config::resolve_str(yaml).expect("well-formed skills should load");
    let verb = resolved
        .pointer("/skills/review.style.house-voice/verb")
        .and_then(Value::as_str)
        .expect("verb should round-trip through resolve");
    assert_eq!(verb, "review");
}

// ── Workflow inputs: → inputSchema synthesis ─────────────────────────────────

#[test]
fn workflow_inputs_compile_to_input_schema_with_defaults() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "f": {
                "initialState": "s",
                "inputs": {
                    "graph_path": { "type": "string", "default": "-" },
                    "name":       { "type": "string", "required": true }
                },
                "states": { "s": { "terminal": true, "transitions": {} } }
            }
        }
    });
    let resolved = config::resolve(cfg).expect("resolves");
    let schema = resolved
        .pointer("/workflows/f/inputSchema")
        .expect("inputSchema synthesized");
    assert_eq!(
        schema.pointer("/properties/graph_path/default"),
        Some(&json!("-"))
    );
    // per-input `required:true` lifted to schema-level required array; not left on the property
    assert_eq!(schema.pointer("/properties/name/required"), None);
    assert_eq!(schema.pointer("/required"), Some(&json!(["name"])));
}

#[test]
fn explicit_input_schema_is_not_overwritten() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": { "f": {
            "initialState": "s",
            "inputs": { "x": { "default": 1 } },
            "inputSchema": { "type": "object", "properties": { "y": {} } },
            "states": { "s": { "terminal": true, "transitions": {} } }
        }}
    });
    let resolved = config::resolve(cfg).expect("resolves");
    // explicit inputSchema preserved (has `y`, not synthesized from inputs `x`)
    assert!(resolved
        .pointer("/workflows/f/inputSchema/properties/y")
        .is_some());
    assert!(resolved
        .pointer("/workflows/f/inputSchema/properties/x")
        .is_none());
}
