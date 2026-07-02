//! P3: the mock satisfies a capability's snippet.outputs (severity=S1), so the
//! orchestrator's downstream `sev == "S1"` gate passes and the flow traverses.
use praxec_test::{fuzz_config, report::render_text};
use std::path::Path;

#[tokio::test]
async fn orchestrator_traverses_via_capability_output() {
    let report = fuzz_config(Path::new("fixtures/capflow.yaml"), 20, 0)
        .await
        .unwrap();
    let orch = report
        .results
        .iter()
        .find(|d| d.definition_id.contains("orch"))
        .expect("orch definition present");
    let all_viol = orch.scenarios.iter().all(|s| s.verdict.is_violation());
    assert!(
        !all_viol,
        "orchestrator should traverse via the capability output:\n{}",
        render_text(&report)
    );
}
