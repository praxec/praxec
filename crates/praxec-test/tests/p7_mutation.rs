//! P7: Mutation testing — quantitative proof that the workflow-checking tool
//! catches defects.
//!
//! Strategy: take known-good configs, programmatically inject one defect at a
//! time (mutation operators), run the full check pipeline, and measure what
//! fraction of mutants the tool KILLS.  A surviving mutant = a tool blind spot.
//!
//! ## Operators and expected outcomes
//!
//! | Operator                  | Kind      | Expected | Detector                     |
//! |---------------------------|-----------|----------|------------------------------|
//! | orphan_state              | Structural| KILLED   | BFS reachability             |
//! | dangle_target             | Structural| KILLED   | validate: unknown target     |
//! | deadend                   | Structural| KILLED   | validate: non-terminal+empty |
//! | det_cycle                 | Structural| KILLED   | deterministic_loops()        |
//! | literal_type_break        | Type      | KILLED   | runtime type check           |
//! | drop_output_write         | Contract  | KILLED   | V24 UNWRITTEN_DECLARED_OUTPUT|
//! | drop_initial_context_seed | Contract  | KILLED   | V24 UNWRITTEN_DECLARED_OUTPUT|
//! | delete_guard              | Semantic  | KILLED   | violating-context probe      |
//! | flip_guard_op             | Semantic  | KILLED   | violating-context probe      |
//!
//! ## A mutant is KILLED only by a complaint it CAUSED
//!
//! Every gate is diffed against what it already said about the UNMUTATED config.
//! Without that subtraction the score is a lie — run against any real corpus with
//! one pre-existing warning (the live pack has four, plus 116 known fuzz
//! findings) and every mutant is "killed" by a defect that was already there.
//! `mutate::an_unmutated_config_survives_its_own_baseline` is the regression guard.
//!
//! ## The CONTRACT operators are why this file matters
//!
//! They did not exist, and the gap shipped a bug: a definition that reaches a
//! terminal owing a declared output it never wrote. Nothing generated that
//! mutant, so nothing measured that the tool was blind to it. A gate you never
//! attack is a gate you are only ASSUMING works.

use std::path::Path;

use praxec_test::mutation_score;

// ── Fixture paths ─────────────────────────────────────────────────────────────

const GUARDED_YAML: &str = "fixtures/guarded.yaml";
const CAPFLOW_YAML: &str = "fixtures/capflow.yaml";

/// Env var pointing at an external cognitive-architectures library (a sibling
/// repo, not vendored here). The `p7_cog_arch_mutation_report` test reads it and
/// skips when unset, so the full report stays available to maintainers without
/// hardcoding a machine-specific path.
const COG_ARCH_YAML_ENV: &str = "PRAXEC_COGARCH_YAML";

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Load + resolve a fixture into a `serde_json::Value`.
fn load(path: &str) -> serde_json::Value {
    praxec_core::config::load_resolved_with_repos(Path::new(path))
        .unwrap_or_else(|e| panic!("failed to load {path}: {e}"))
        .0
}

// ── Structural/type operator kill-rate assertions ─────────────────────────────

/// Structural operators must kill ≥ 90 % of their mutants across the good fixtures.
/// We run both fixtures together so the combined sample is large enough to be
/// statistically meaningful.
#[tokio::test]
async fn structural_operators_kill_rate_high_on_good_fixtures() {
    let paths = [GUARDED_YAML, CAPFLOW_YAML];

    // Accumulate per-operator totals across both fixtures.
    let structural_ops = [
        "orphan_state",
        "dangle_target",
        "deadend",
        "det_cycle",
        "literal_type_break",
    ];
    let mut totals: std::collections::HashMap<&str, (usize, usize)> =
        structural_ops.iter().map(|op| (*op, (0, 0))).collect();

    for path in &paths {
        let resolved = load(path);
        let report = mutation_score(&resolved)
            .await
            .unwrap_or_else(|e| panic!("mutation_score failed for {path}: {e}"));

        println!(
            "\n=== Mutation report: {path} ===\n{}",
            report.render_text()
        );

        for r in &report.per_operator {
            if let Some(entry) = totals.get_mut(r.operator.as_str()) {
                entry.0 += r.total;
                entry.1 += r.killed;
            }
        }
    }

    for op in &structural_ops {
        let (total, killed) = totals[op];
        let rate = if total == 0 {
            1.0
        } else {
            killed as f64 / total as f64
        };
        println!("  {op}: {killed}/{total} killed ({:.0}%)", rate * 100.0);

        if total == 0 {
            // No applicable sites in these small fixtures — not a failure
            println!("  (no mutants generated for {op} on these fixtures — skipping assertion)");
            continue;
        }

        assert!(
            rate >= 0.9,
            "STRUCTURAL operator '{op}' kill rate {:.1}% < 90% on good fixtures \
             ({killed}/{total} killed) — this is a tool regression",
            rate * 100.0
        );
    }
}

/// CONTRACT operators — the output-contract class.
///
/// This is the one that shipped a bug. No operator modeled "a definition reaches
/// a terminal owing a declared output it never wrote", so nothing measured that
/// the tool was blind to it — and it was: the runtime terminal check catches the
/// scalar cases, but the deterministic-repair rung silently coerces a missing
/// `array`/`object` output to `[]`/`{}`, so those went GREEN with an empty result.
///
/// V24 (`UNWRITTEN_DECLARED_OUTPUT`) closes it statically, and THIS is the number
/// that says so. If someone weakens V24, this test is what fails.
#[tokio::test]
async fn contract_operators_are_killed_on_good_fixtures() {
    let paths = [GUARDED_YAML, CAPFLOW_YAML];
    let contract_ops = ["drop_output_write", "drop_initial_context_seed"];

    let mut totals: std::collections::HashMap<&str, (usize, usize)> =
        contract_ops.iter().map(|op| (*op, (0, 0))).collect();

    for path in &paths {
        let resolved = load(path);
        let report = mutation_score(&resolved)
            .await
            .unwrap_or_else(|e| panic!("mutation_score failed for {path}: {e}"));
        for r in &report.per_operator {
            if let Some(entry) = totals.get_mut(r.operator.as_str()) {
                entry.0 += r.total;
                entry.1 += r.killed;
            }
        }
    }

    println!("\n=== CONTRACT operator kill rates (good fixtures) ===");
    for op in &contract_ops {
        let (total, killed) = totals[op];
        if total == 0 {
            println!("  {op}: no applicable sites in these fixtures — skipping");
            continue;
        }
        let rate = killed as f64 / total as f64;
        println!("  {op}: {killed}/{total} killed ({:.0}%)", rate * 100.0);
        assert!(
            rate >= 0.9,
            "CONTRACT operator '{op}' kill rate {:.1}% < 90% ({killed}/{total}) — a definition \
             can drop a declared output's only writer and the tool does not notice. That is \
             exactly the hole V24 exists to close.",
            rate * 100.0
        );
    }
}

/// Semantic operators (delete_guard, flip_guard_op) are EXPECTED to produce
/// surviving mutants — these are the tool's documented blind spots.
/// This test RECORDS the rates but does NOT assert high kill rates.
#[tokio::test]
async fn semantic_operators_blind_spot_recorded_on_good_fixtures() {
    let paths = [GUARDED_YAML, CAPFLOW_YAML];

    let semantic_ops = ["delete_guard", "flip_guard_op"];
    let mut totals: std::collections::HashMap<&str, (usize, usize)> =
        semantic_ops.iter().map(|op| (*op, (0, 0))).collect();

    for path in &paths {
        let resolved = load(path);
        let report = mutation_score(&resolved)
            .await
            .unwrap_or_else(|e| panic!("mutation_score failed for {path}: {e}"));

        for r in &report.per_operator {
            if let Some(entry) = totals.get_mut(r.operator.as_str()) {
                entry.0 += r.total;
                entry.1 += r.killed;
            }
        }
    }

    println!("\n=== Semantic operator blind spots (good fixtures) ===");
    for op in &semantic_ops {
        let (total, killed) = totals[op];
        let survived = total - killed;
        let rate = if total == 0 {
            1.0
        } else {
            killed as f64 / total as f64
        };
        println!(
            "  BLIND SPOT {op}: {survived}/{total} survived ({:.0}% kill rate) — \
             expected, closed by `praxec test --scenarios`",
            rate * 100.0
        );
        // No kill-rate assertion — semantic survival is expected and documented.
    }
}

// ── Full per-operator table on the cog-arch library ───────────────────────────

/// Run the full mutation harness over the cognitive-architectures library.
///
/// Prints a detailed per-operator table and the overall mutation score.
/// Marked `#[ignore]` because it loads 28 workflows + runs fuzz_coverage per
/// mutant (~seconds on fast hardware, potentially minutes on CI).
///
/// Run with: `cargo test -p praxec-test p7_cog_arch -- --ignored`
#[tokio::test]
#[ignore = "slow: loads 28-definition cog-arch library; run with --ignored for the full report"]
async fn p7_cog_arch_mutation_report() {
    let Some(path) = std::env::var(COG_ARCH_YAML_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
    else {
        eprintln!("{COG_ARCH_YAML_ENV} not set — skipping cog-arch mutation report");
        return;
    };
    let resolved = load(&path);
    let report = mutation_score(&resolved)
        .await
        .expect("mutation_score on cog-arch");

    println!("\n╔══════════════════════════════════════════════════════════════╗");
    println!("║  Mutation Report — cognitive-architectures library           ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!("{}", report.render_text());

    println!("Per-operator detail:");
    for r in &report.per_operator {
        let note = match r.operator.as_str() {
            "orphan_state" | "dangle_target" | "deadend" | "det_cycle" | "literal_type_break" => {
                "← structural / validate"
            }
            "drop_output_write" | "drop_initial_context_seed" => "← V24 must-write",
            "retarget_guard_scope" => "← V25 guard scope",
            // Once documented blind spots. The per-transition fuzz submits a
            // deliberately VIOLATING context to every edge and asserts the guard
            // rejects it, so a deleted/flipped guard is now caught, not survived.
            "delete_guard" | "flip_guard_op" => "← violating-context probe",
            _ => "",
        };
        println!(
            "  {:<26} {:>3}/{:<3} killed  ({:>5.1}%)  {}",
            r.operator,
            r.killed,
            r.total,
            r.kill_rate() * 100.0,
            note
        );
        if let Some(ref survivor) = r.sample_survivor {
            println!("    sample survivor: {survivor}");
        }
    }

    // Structural assertions even on the large corpus
    let structural_ops = [
        "orphan_state",
        "dangle_target",
        "deadend",
        "det_cycle",
        "literal_type_break",
    ];
    for r in &report.per_operator {
        if structural_ops.contains(&r.operator.as_str()) && r.total > 0 {
            assert!(
                r.kill_rate() >= 0.9,
                "STRUCTURAL operator '{}' kill rate {:.1}% < 90% on cog-arch corpus",
                r.operator,
                r.kill_rate() * 100.0
            );
        }
    }
}
