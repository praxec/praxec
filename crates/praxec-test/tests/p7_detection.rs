//! P7: Detection-proof corpus — one planted defect per class.
//!
//! Each test either:
//!   A) calls `fuzz_coverage` and asserts `has_failures()` (fuzz/graph-walk caught), or
//!   B) loads the config and calls `validate_workflows`, asserting error/warning
//!      diagnostics are present (check-caught), or
//!   C) is `#[ignore]`d — a remaining blind spot that cannot be closed without false positives.
//!
//! Good-twin tests assert `!has_failures()` to confirm no false positives.
//!
//! Good-twin tests assert `!has_failures()` to confirm no false positives.
//!
//! ## Detection map (summary)
//!
//! | Defect class              | Fixture                         | Tool       | Result           |
//! |---------------------------|---------------------------------|------------|------------------|
//! | orphan state              | orphan_state.yaml               | fuzz       | CAUGHT           |
//! | dangling target           | dangling_target.yaml            | check      | CAUGHT           |
//! | dead-end (non-terminal)   | deadend.yaml                    | check+fuzz | CAUGHT           |
//! | type mismatch literal     | type_mismatch_literal.yaml      | fuzz       | CAUGHT           |
//! | unsatisfiable guard       | unsatisfiable_guard.yaml        | fuzz       | CAUGHT           |
//! | infinite det. loop        | infinite_deterministic_loop.yaml| graph walk | CAUGHT           |
//! | required input no default | required_input_no_default.yaml  | fuzz       | BLIND SPOT (fp)  |
//!
//! The required-input blind spot cannot be closed without false positives:
//! the empty-input start check flags both the fixture AND legitimate cog-arch
//! workflows (structurally identical required-field patterns). Detection requires
//! `$.workflow.input.*` path analysis to distinguish intentional inputs from
//! defective unused-required fields — deferred.

use praxec_core::validate::validate_workflows;
use praxec_test::fuzz_coverage;
use std::path::Path;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Load and resolve a fixture config, returning the resolved Value.
fn load(path: &str) -> serde_json::Value {
    let (config, _diagnostics) = praxec_core::config::load_resolved_with_repos(Path::new(path))
        .unwrap_or_else(|e| panic!("failed to load {path}: {e}"));
    config
}

// ── 1. Orphan state ───────────────────────────────────────────────────────────

/// CAUGHT: fuzz_coverage reports orphan_states non-empty for a state that has
/// no incoming transition from the initial-state BFS.
#[tokio::test]
async fn defect_orphan_state_caught_by_fuzz() {
    let cov = fuzz_coverage(Path::new("fixtures/defects/orphan_state.yaml"))
        .await
        .expect("fuzz_coverage");
    assert!(
        cov.has_failures(),
        "orphan state defect should be detected:\n{}",
        cov.render_text()
    );
    let text = cov.render_text();
    assert!(
        text.contains("orphan"),
        "render_text should mention 'orphan':\n{text}"
    );
}

/// GOOD TWIN: no orphan — all states reachable. fuzz_coverage passes.
#[tokio::test]
async fn defect_orphan_state_good_twin_no_false_positive() {
    let cov = fuzz_coverage(Path::new("fixtures/defects/orphan_state_good.yaml"))
        .await
        .expect("fuzz_coverage");
    assert!(
        !cov.has_failures(),
        "good orphan twin should pass:\n{}",
        cov.render_text()
    );
}

// ── 2. Dangling target ────────────────────────────────────────────────────────

/// CAUGHT: validate_workflows produces an Error-level diagnostic when a
/// transition target references a state name that is not declared.
/// The check command (praxec check) exits non-zero for this case.
#[test]
fn defect_dangling_target_caught_by_validate() {
    let config = load("fixtures/defects/dangling_target.yaml");
    let diagnostics = validate_workflows(&config);
    let has_error = diagnostics.iter().any(|d| {
        d.is_error()
            && (d.message().contains("nonexistent") || d.message().contains("not in states"))
    });
    assert!(
        has_error,
        "dangling target should produce an error diagnostic; got: {diagnostics:?}"
    );
}

// ── 3. Dead-end (non-terminal, empty transitions) ─────────────────────────────

/// CAUGHT via validate_workflows Warning: a non-terminal state with no outgoing
/// transitions triggers a "non-terminal with no outgoing transitions" warning.
/// Additionally, 'done' is unreachable (dead-end severs the path), so fuzz
/// also detects it as an orphan.
#[test]
fn defect_deadend_caught_by_validate_warning() {
    let config = load("fixtures/defects/deadend.yaml");
    let diagnostics = validate_workflows(&config);
    // Should have at least a warning about the dead-end state or an unreachable state.
    let has_diagnostic = diagnostics.iter().any(|d| {
        let msg = d.message();
        msg.contains("stuck") || msg.contains("non-terminal") || msg.contains("unreachable")
    });
    assert!(
        has_diagnostic,
        "dead-end should produce at least one diagnostic; got: {diagnostics:?}"
    );
}

/// ALSO CAUGHT by fuzz_coverage: 'done' is unreachable (stuck severs the path),
/// so fuzz reports it as an orphan, meaning has_failures() is true.
#[tokio::test]
async fn defect_deadend_also_caught_by_fuzz_orphan() {
    let cov = fuzz_coverage(Path::new("fixtures/defects/deadend.yaml"))
        .await
        .expect("fuzz_coverage");
    assert!(
        cov.has_failures(),
        "dead-end defect: 'done' is unreachable so fuzz should flag it:\n{}",
        cov.render_text()
    );
}

// ── 4. Type mismatch — literal bool written to string-typed slot ──────────────

/// CAUGHT: a literal bool `x: true` written to a string-typed blackboard slot
/// (`x: { type: string }`) is now detected by the fuzz harness.
///
/// Fix: `all_output_slots_typed` is now computed over ONLY `OutputSource::Field`
/// slots (mock-emitted values). Literal/Other outputs are the workflow's own
/// values — no mock ambiguity — so a BLACKBOARD_TYPE_ERROR from a literal
/// mismatch is always a real defect, never excused. When there are no Field
/// slots (all outputs are literals), the excuse is vacuously "all typed" →
/// BLACKBOARD_TYPE_ERROR is not excused → ok=false.
#[tokio::test]
async fn defect_type_mismatch_literal_blind_spot() {
    let cov = fuzz_coverage(Path::new("fixtures/defects/type_mismatch_literal.yaml"))
        .await
        .expect("fuzz_coverage");
    // This assertion SHOULD pass but DOES NOT because the fuzz excuses the error.
    assert!(
        cov.has_failures(),
        "type mismatch literal SHOULD be caught but is a blind spot:\n{}",
        cov.render_text()
    );
}

/// GOOD TWIN: string literal written to string-typed slot — no BLACKBOARD_TYPE_ERROR.
/// This confirms the good twin passes cleanly (no false positive).
#[tokio::test]
async fn defect_type_mismatch_literal_good_twin_no_false_positive() {
    let cov = fuzz_coverage(Path::new(
        "fixtures/defects/type_mismatch_literal_good.yaml",
    ))
    .await
    .expect("fuzz_coverage");
    assert!(
        !cov.has_failures(),
        "good type-mismatch twin should pass:\n{}",
        cov.render_text()
    );
}

// ── 5. Unsatisfiable guard (contradictory guards on same transition) ───────────

/// CAUGHT: the satisfying_context sets n=1 (first guard), but the second guard
/// requires n==2. The runtime rejects with GUARD_REJECTED → sat_ok=false →
/// verdict ok=false.
#[tokio::test]
async fn defect_unsatisfiable_guard_caught_by_fuzz() {
    let cov = fuzz_coverage(Path::new("fixtures/defects/unsatisfiable_guard.yaml"))
        .await
        .expect("fuzz_coverage");
    assert!(
        cov.has_failures(),
        "unsatisfiable guard should be detected:\n{}",
        cov.render_text()
    );
    let text = cov.render_text();
    assert!(
        text.contains("✗"),
        "render_text should contain ✗ for the failing transition:\n{text}"
    );
    // The detail should mention guard satisfaction failure.
    assert!(
        text.contains("satisfy") || text.contains("GUARD_REJECTED"),
        "render_text should mention guard rejection:\n{text}"
    );
}

/// GOOD TWIN: single consistent guard (n == 1). fuzz satisfies it cleanly.
#[tokio::test]
async fn defect_unsatisfiable_guard_good_twin_no_false_positive() {
    let cov = fuzz_coverage(Path::new("fixtures/defects/unsatisfiable_guard_good.yaml"))
        .await
        .expect("fuzz_coverage");
    assert!(
        !cov.has_failures(),
        "good unsatisfiable-guard twin should pass:\n{}",
        cov.render_text()
    );
}

// ── 6. Infinite deterministic loop ────────────────────────────────────────────

/// CAUGHT: a static graph check now detects states caught in an infinite
/// deterministic loop with no escape.
///
/// Fix: `walk::deterministic_loops` computes which states are in a
/// deterministic cycle with no path to a terminal or non-deterministic state.
/// States A and B both have only `actor: deterministic` transitions forming a
/// cycle (A→B, B→A) with no terminal reachable — both are flagged.
/// `DefCoverage.det_loops` is non-empty → `has_failures()` returns true.
#[tokio::test]
async fn defect_infinite_deterministic_loop_blind_spot() {
    let cov = fuzz_coverage(Path::new(
        "fixtures/defects/infinite_deterministic_loop.yaml",
    ))
    .await
    .expect("fuzz_coverage");
    // This assertion SHOULD pass but DOES NOT — the fuzz passes on the loop.
    assert!(
        cov.has_failures(),
        "infinite deterministic loop SHOULD be caught but is a blind spot:\n{}",
        cov.render_text()
    );
}

// ── 7. Required input with no default ─────────────────────────────────────────

/// BLIND SPOT (UNCLOSEABLE without false positives): the per-transition fuzz
/// harness bypasses the normal workflow start path entirely. It seeds instances
/// directly at the target state with context derived from guard analysis.
///
/// The start-time `inputSchema` check (empty input + defaults + validate) would
/// catch this fixture, but it also false-positively flags legitimate cog-arch
/// workflows that require operator inputs (e.g., `issue`, `feature_brief`).
/// Those workflows use `inputs: { x: { type: string, required: true } }` which
/// synthesizes an identical `inputSchema` pattern — indistinguishable from the
/// defect fixture at the schema level.
///
/// Per spec guidance: "only flag when NO default AND the dummy can't satisfy;
/// keep cog-arch clean." Since `dummy_for_schema` CAN satisfy both the fixture
/// schema AND cog-arch schemas (both have typed required fields), the start check
/// cannot distinguish between defective and legitimate required inputs without
/// tracking which inputSchema fields are actually read via `$.workflow.input.*`.
///
/// Gap remains: inputSchema required-field validation is a start-time check.
/// Catching it without false positives requires reading `$.workflow.input.*`
/// path analysis — deferred.
#[tokio::test]
#[ignore = "UNCLOSEABLE without false positives: start-time required-input check flags \
             both the defect fixture AND legitimate cog-arch workflows that require \
             operator inputs. The fixture schema (required: [thing], type: string) is \
             structurally identical to cog-arch required inputs. Keeping cog-arch clean \
             takes precedence. Detection requires $.workflow.input.* path analysis."]
async fn defect_required_input_no_default_blind_spot() {
    let cov = fuzz_coverage(Path::new("fixtures/defects/required_input_no_default.yaml"))
        .await
        .expect("fuzz_coverage");
    // The per-transition fuzz passes (context seeded directly). The start check
    // would catch this but cannot be enabled without false positives on cog-arch.
    assert!(
        cov.has_failures(),
        "required input no default SHOULD be caught but cannot be without false positives:\n{}",
        cov.render_text()
    );
}
