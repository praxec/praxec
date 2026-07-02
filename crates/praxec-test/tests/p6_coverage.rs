//! P6: the coverage layer passes on a sound flow and flags both an orphan state
//! and an un-driveable transition on a bad one.
use praxec_test::fuzz_coverage;
use std::path::Path;

#[tokio::test]
async fn coverage_passes_on_sound_flow() {
    let cov = fuzz_coverage(Path::new("fixtures/guarded.yaml"))
        .await
        .unwrap();
    assert!(!cov.has_failures(), "{}", cov.render_text());
}

#[tokio::test]
async fn coverage_flags_orphan_and_bad_transition() {
    let cov = fuzz_coverage(Path::new("fixtures/badtransition.yaml"))
        .await
        .unwrap();
    assert!(
        cov.has_failures(),
        "should flag the orphan + un-driveable transition"
    );
    let text = cov.render_text();
    assert!(
        text.contains("orphan"),
        "orphan state should be reported:\n{text}"
    );
    assert!(
        text.contains("finish"),
        "the un-driveable transition should be flagged:\n{text}"
    );
}
