//! Proves the harness both ways: the sound fixture passes; the wedge fixture
//! is flagged. This is the conformance test for the oracle itself.

use std::path::Path;

use praxec_test::fuzz_config;
use praxec_test::report::{has_violations, render_text};

#[tokio::test]
async fn sound_fixture_has_no_violations() {
    let report = fuzz_config(Path::new("fixtures/sound.yaml"), 10, 0)
        .await
        .expect("fuzz runs");
    assert!(
        !has_violations(&report),
        "sound fixture should pass, got:\n{}",
        render_text(&report)
    );
}

#[tokio::test]
async fn wedge_fixture_is_flagged() {
    let report = fuzz_config(Path::new("fixtures/wedge.yaml"), 10, 0)
        .await
        .expect("fuzz runs");
    assert!(
        has_violations(&report),
        "wedge fixture should be flagged as a violation, got:\n{}",
        render_text(&report)
    );
}
