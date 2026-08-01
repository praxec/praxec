//! Audit-resolution C.3 — every shipped example YAML in examples/ must
//! resolve cleanly under the v0.2 validator stack. This is the regression
//! guard against publishing broken reference configs that users would
//! copy-paste.

use praxec_core::config;
use std::path::PathBuf;

fn examples_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR is crates/praxec-core; walk up two parents
    // to the workspace root, then into examples/.
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.push("examples");
    p
}

fn resolve_example(rel: &str) -> serde_json::Value {
    let path = examples_dir().join(rel);
    assert!(
        path.exists(),
        "example file must exist at {}",
        path.display()
    );
    config::load_resolved(&path).unwrap_or_else(|e| {
        panic!("example '{rel}' failed to resolve cleanly: {e}");
    })
}

// ── Other shipped examples must continue to validate ───────────────────────

#[test]
fn authoring_workflow_yaml_resolves_cleanly() {
    let _ = resolve_example("authoring-workflow.yaml");
}

#[test]
fn governed_change_yaml_resolves_cleanly() {
    let _ = resolve_example("governed-change.yaml");
}

#[test]
fn simple_proxy_yaml_resolves_cleanly() {
    let _ = resolve_example("simple-proxy.yaml");
}

// ── Task 6 — the rewired governed provision flow ───────────────────────────

/// The tool-provision example (its `installing` step now DELEGATES to
/// `praxec tools install`) still resolves cleanly through the validator stack —
/// i.e. `praxec check` passes on it.
#[test]
fn tool_provision_gateway_resolves_cleanly() {
    let _ = resolve_example("tool-provision/gateway.yaml");
}

/// The `installing` step routes through the ONE installer delegation, and the
/// deleted dead paths (`npm install -g`, `INSTALL_RECIPE_UNAVAILABLE`,
/// `install_unsupported` / `community_installing`) are grep-clean gone.
#[test]
fn tool_provision_install_delegates_and_dead_paths_are_gone() {
    let flow = examples_dir().join("tool-provision/flow.tools.provision.yaml");
    let text = std::fs::read_to_string(&flow).expect("flow readable");

    // The install step delegates to the one installer.
    assert!(
        text.contains("\"tools\"") && text.contains("\"install\""),
        "installing must delegate via `praxec tools install`"
    );

    // Dead paths removed — no parallel install abstraction left beside it.
    for dead in [
        "npm install -g",
        "INSTALL_RECIPE_UNAVAILABLE",
        "install_unsupported",
        "community_installing",
        "install.community-npm",
        "connection: npm",
    ] {
        assert!(
            !text.contains(dead),
            "dead path `{dead}` must be deleted from the provision flow"
        );
    }
}

// ── Regression guard: every *.yaml at examples/ top level must resolve ─────

#[test]
fn every_top_level_yaml_in_examples_resolves() {
    let dir = examples_dir();
    let entries = std::fs::read_dir(&dir).expect("examples/ dir readable");
    let mut failed: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
            continue;
        };
        if ext != "yaml" && ext != "yml" {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if let Err(e) = config::load_resolved(&path) {
            failed.push(format!("{name}: {e}"));
        }
    }
    assert!(
        failed.is_empty(),
        "top-level example YAML(s) failed to resolve:\n  {}",
        failed.join("\n  ")
    );
}
