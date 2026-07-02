//! P7: Mutation testing — quantitative proof that the workflow-checking tool
//! catches defects.
//!
//! Strategy: take known-good configs, programmatically inject one defect at a
//! time (mutation operators), run the full check pipeline, and measure what
//! fraction of mutants the tool KILLS.  A surviving mutant = a tool blind spot.
//!
//! ## Operators and expected outcomes
//!
//! | Operator           | Kind      | Expected  | Rationale                          |
//! |--------------------|-----------|-----------|------------------------------------|
//! | orphan_state       | Structural| KILLED    | BFS reachability detects orphans   |
//! | dangle_target      | Structural| KILLED    | validate catches unknown targets   |
//! | deadend            | Structural| KILLED    | validate warns non-terminal+empty  |
//! | det_cycle          | Structural| KILLED    | deterministic_loops() detects loop |
//! | literal_type_break | Type      | KILLED    | runtime rejects wrong-typed lit    |
//! | delete_guard       | Semantic  | SURVIVES  | tool can't know guard intent       |
//! | flip_guard_op      | Semantic  | SURVIVES  | tool can't know correct operator   |
//!
//! The structural/type operators have ~100% kill rates; that is the tool's
//! GUARANTEE.  The semantic operators largely survive; those are the tool's
//! DOCUMENTED BLIND SPOTS (closed by `praxec test --scenarios`).

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
                "← GUARANTEED CATCH"
            }
            "delete_guard" | "flip_guard_op" => "← DOCUMENTED BLIND SPOT",
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
