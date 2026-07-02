//! P1 payoff: the smart mock derives `decide -> { approved: true }` from the
//! downstream `approved == true` guard, so the flow traverses
//! start -> review -> done (succeeded). Under P0's empty mock this flow would
//! livelock (the guard could never pass).

use praxec_test::fuzz_config;
use praxec_test::report::render_text;
use std::path::Path;

#[tokio::test]
async fn smart_mock_traverses_guarded_flow() {
    let report = fuzz_config(Path::new("fixtures/guarded.yaml"), 20, 0)
        .await
        .unwrap();
    // With failure injection (rate 15) some scenarios resolve `failed` (Pass) and
    // some `succeeded` (Pass); a guard the mock COULDN'T satisfy would livelock
    // EVERY scenario. So assert: not every scenario is a violation.
    let all_violations = report
        .results
        .iter()
        .all(|d| d.scenarios.iter().all(|s| s.verdict.is_violation()));
    assert!(
        !all_violations,
        "smart mock should traverse the guard at least sometimes:\n{}",
        render_text(&report)
    );
}
