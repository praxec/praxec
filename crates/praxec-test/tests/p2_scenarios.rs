//! P2: declared-property scenarios pass on a real workflow, and a bad assertion
//! is correctly reported as failed.

use praxec_test::scenario::Expect;
use std::path::Path;

#[tokio::test]
async fn scenarios_pass_on_guarded_flow() {
    let report = praxec_test::run_scenarios(
        Path::new("fixtures/guarded.yaml"),
        Path::new("fixtures/scenarios.yaml"),
    )
    .await
    .expect("runs");
    assert!(!report.failed(), "{}", report.render_text());
}

#[tokio::test]
async fn false_assertion_is_caught() {
    use praxec_core::config::load_resolved_with_repos;
    use praxec_test::driver::fuzz_definition;
    let (resolved, _d) = load_resolved_with_repos(Path::new("fixtures/guarded.yaml")).unwrap();
    let runs = fuzz_definition(&resolved, "guarded_flow", 30, 0)
        .await
        .unwrap();
    let bad = Expect {
        never_reaches: vec!["done".into()],
        ..Default::default()
    };
    let out = praxec_test::assert::evaluate(&bad, &runs);
    assert!(
        out.iter().any(|a| !a.passed),
        "expected a failed assertion: {out:?}"
    );
}
