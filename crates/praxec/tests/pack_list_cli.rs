//! `praxec pack list <repo>` — end-to-end through the real `praxec` binary.
//!
//! The subcommand enumerates a pack's `flow.*` (orchestrator) and `cap.*`
//! (capability) definition ids WITHOUT building a full gateway (no store, no
//! runtime): it reuses `praxec_core::repo::load_repo` to walk the pack's layout
//! dirs and reports the namespace-prefixed ids, grouped and counted. See the
//! `pack_list` handler in `crates/praxec/src/gateway.rs`.
//!
//! Fixture: the checked-in `praxec-meta` pack (namespace `meta`) ships exactly
//! 5 flows and 16 capabilities — the counts asserted below are pinned to that
//! fixture (assert-don't-derive).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn meta_fixture() -> PathBuf {
    // crates/praxec/ -> ../praxec-core/tests/fixtures/praxec-meta
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../praxec-core/tests/fixtures/praxec-meta")
}

fn run_pack_list(repo: &Path) -> Output {
    let bin = env!("CARGO_BIN_EXE_praxec");
    Command::new(bin)
        .arg("pack")
        .arg("list")
        .arg(repo)
        .output()
        .expect("run praxec pack list")
}

#[test]
fn pack_list_enumerates_flows_and_caps_namespace_prefixed() {
    let out = run_pack_list(&meta_fixture());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "pack list on a valid pack must exit success:\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Manifest identity line.
    assert!(
        stdout.contains("praxec-meta") && stdout.contains("meta"),
        "must print the manifest name + namespace:\n{stdout}"
    );

    // Grouped counts pinned to the fixture: 5 flows, 16 caps.
    assert!(
        stdout.contains("flows (5)"),
        "expected 5 flows counted:\n{stdout}"
    );
    assert!(
        stdout.contains("caps (16)"),
        "expected 16 caps counted:\n{stdout}"
    );

    // Ids are namespace-prefixed as loaded.
    assert!(
        stdout.contains("meta/flow.author-capability"),
        "expected a namespace-prefixed flow id:\n{stdout}"
    );
    assert!(
        stdout.contains("meta/cap.plan.compose-implementation"),
        "expected a namespace-prefixed cap id:\n{stdout}"
    );

    // A cap id must NOT appear inside the flows group (grouping is by id, not
    // just presence): flows should not carry `cap.` ids and vice versa. Cheap
    // structural check — the flow ids all begin `meta/flow.`.
    assert!(
        !stdout.contains("meta/flow.plan.compose-implementation"),
        "a cap must not be reclassified as a flow:\n{stdout}"
    );
}

#[test]
fn pack_list_on_non_pack_dir_fails_fast() {
    let td = tempfile::tempdir().unwrap();
    // An existing directory that is NOT a pack (no praxec.repo.yaml).
    let out = run_pack_list(td.path());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "a non-pack dir must fail-fast (non-zero):\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("praxec.repo.yaml") || stderr.contains("not a pack"),
        "error must clearly name the missing manifest:\nstderr:\n{stderr}"
    );
}

#[test]
fn pack_list_on_missing_dir_fails_fast() {
    let td = tempfile::tempdir().unwrap();
    let missing = td.path().join("does-not-exist");
    let out = run_pack_list(&missing);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "a missing dir must fail-fast (non-zero); stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("does not exist") || stderr.contains("does-not-exist"),
        "error must name the missing directory:\nstderr:\n{stderr}"
    );
}
