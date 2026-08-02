//! Task 4 — `praxec doctor` resolve-and-offer for missing `kind: mcp` tools.
//!
//! End-to-end through the real `praxec` binary. The offer path (no `--fix`) is
//! driven here because it performs ZERO network IO (`resolve_provider` only
//! reads); the `--fix` install path hits the network for real assets, so it is
//! unit-tested at the praxec-core layer (T2/T3, fake IO) and at the doctor-helper
//! layer (`gateway.rs` unit tests, fake IO) — never against the live network here.
//!
//! Covered:
//! - a missing `kind: mcp` connection whose `command` matches a registry tool →
//!   doctor prints the provisioning section naming tool + provider + command, and
//!   performs NO install (offer-only, no `--fix`);
//! - a missing connection with no matching registry tool → doctor says it cannot
//!   offer (unknown tool) and does not crash;
//! - a connection whose command IS on PATH → not offered.

use std::path::Path;
use std::process::{Command, Output};

fn run_doctor(config: &Path, fix: bool) -> Output {
    let bin = env!("CARGO_BIN_EXE_praxec");
    let mut cmd = Command::new(bin);
    cmd.arg("doctor").arg("--config").arg(config);
    if fix {
        cmd.arg("--fix");
    }
    cmd.output().expect("run praxec doctor")
}

/// A `praxec.packs/v3` registry declaring one release-provider tool whose
/// `command` is `cpm-planner-testtool`.
const REGISTRY: &str = r#"schema: praxec.packs/v3
tools:
  - id: cpm-planner
    name: cpm-planner
    description: Critical path planner.
    command: cpm-planner-testtool
    version: 0.0.2
    providers:
      release: https://github.com/praxec/cpm-planner/releases
"#;

#[test]
fn doctor_offers_a_matching_missing_tool_without_installing() {
    let td = tempfile::tempdir().unwrap();
    let registry_path = td.path().join("packs.yaml");
    std::fs::write(&registry_path, REGISTRY).unwrap();

    let config_path = td.path().join("gateway.yaml");
    std::fs::write(
        &config_path,
        format!(
            "version: \"1.0.0\"\n\
             gateway:\n  allow_ephemeral: true\n\
             connections:\n  planner:\n    kind: mcp\n    command: cpm-planner-testtool\n\
             discovery:\n  registry: {}\n",
            registry_path.display()
        ),
    )
    .unwrap();

    let out = run_doctor(&config_path, false);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "doctor must exit success (offer is advisory):\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Section names the tool, the resolved provider, and the exact command.
    assert!(
        stdout.contains("tool cpm-planner:"),
        "offer names the registry tool id:\n{stdout}"
    );
    assert!(
        stdout.contains("provider=release"),
        "offer names the resolved provider:\n{stdout}"
    );
    assert!(
        stdout.contains("result=offered"),
        "offer is offer-only (not installed):\n{stdout}"
    );
    assert!(
        stdout.contains("cpm-planner-testtool-x86_64-unknown-linux-gnu.tar.gz")
            || stdout.contains("/download/v0.0.2/"),
        "offer names the exact release command/URL:\n{stdout}"
    );
    // Consent by construction: no `--fix` → nothing was installed.
    assert!(
        !stdout.contains("result=installed"),
        "no install may happen without --fix:\n{stdout}"
    );
}

#[test]
fn doctor_cannot_offer_an_unknown_missing_tool() {
    let td = tempfile::tempdir().unwrap();
    // No `discovery.registry` → the missing command matches no registry tool.
    let config_path = td.path().join("gateway.yaml");
    std::fs::write(
        &config_path,
        "version: \"1.0.0\"\n\
         gateway:\n  allow_ephemeral: true\n\
         connections:\n  mystery:\n    kind: mcp\n    command: totally-unknown-mcp-binary\n",
    )
    .unwrap();

    let out = run_doctor(&config_path, false);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "an unknown missing tool must not crash doctor:\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("totally-unknown-mcp-binary") && stdout.contains("unknown tool"),
        "doctor must say it cannot offer an unknown tool:\n{stdout}"
    );
}

#[test]
fn doctor_does_not_offer_a_command_already_on_path() {
    let td = tempfile::tempdir().unwrap();
    // `cargo` is on PATH in the build environment → classified present → never
    // offered. A registry that (irrelevantly) declares it must not change that.
    let registry_path = td.path().join("packs.yaml");
    std::fs::write(
        &registry_path,
        "schema: praxec.packs/v3\n\
         tools:\n  - id: cargo-tool\n    name: cargo\n    command: cargo\n    version: 1.0.0\n    providers:\n      release: https://example.com/releases\n",
    )
    .unwrap();

    let config_path = td.path().join("gateway.yaml");
    std::fs::write(
        &config_path,
        format!(
            "version: \"1.0.0\"\n\
             gateway:\n  allow_ephemeral: true\n\
             connections:\n  builder:\n    kind: mcp\n    command: cargo\n\
             discovery:\n  registry: {}\n",
            registry_path.display()
        ),
    )
    .unwrap();

    let out = run_doctor(&config_path, false);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout:\n{stdout}");
    assert!(
        !stdout.contains("tool cargo-tool:"),
        "a command already on PATH must not be offered:\n{stdout}"
    );
}
