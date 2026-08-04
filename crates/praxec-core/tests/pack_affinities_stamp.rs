//! onboarding-hardening — a pack's optional `affinities:` block TRAVELS: when a
//! `repos:` entry declares one, `merge_declared_repos` stamps it into the
//! resolved config under `/praxec/_packAffinities`, keyed by namespace. The
//! config-readiness keystone (`check`/`doctor`) reads this to surface a pack's
//! RECOMMENDATION when one of its MOUNTED affinities resolves to no binding.
//!
//! Fixtures are throwaway local-path packs under a fresh `tempfile::TempDir`
//! (no git needed — stamping is a property of the manifest, not the checkout).

use std::path::Path;

use praxec_core::config::load_resolved_with_repos;
use serde_json::Value;
use tempfile::TempDir;

/// A minimal local-path pack: a `praxec.repo.yaml` under `namespace`, plus one
/// trivial capability so the repo contributes a definition. `affinities` is the
/// literal `affinities:` YAML block (may be empty string for "none declared").
fn write_pack(dir: &Path, namespace: &str, affinities: &str) {
    std::fs::create_dir_all(dir.join("capabilities")).unwrap();
    std::fs::write(
        dir.join("praxec.repo.yaml"),
        format!(
            "schema: praxec.repo/v1\nname: {namespace}-pack\nnamespace: {namespace}\nversion: 0.0.1\n{affinities}"
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join("capabilities/cap.demo.yaml"),
        "workflows:\n  cap.demo:\n    title: Demo\n",
    )
    .unwrap();
}

fn write_host(td: &TempDir, body: &str) -> std::path::PathBuf {
    let p = td.path().join("gateway.yaml");
    std::fs::write(&p, body).unwrap();
    p
}

#[test]
fn declared_affinities_are_stamped_into_pack_affinities_by_namespace() {
    let td = TempDir::new().unwrap();
    let pack = td.path().join("design-pack");
    write_pack(
        &pack,
        "design",
        "affinities:\n  design:\n    tier: frontier\n    capability: UI design annealing\n    recommended: openrouter/anthropic/claude-sonnet-4-5\n",
    );
    let host = format!(
        "version: \"1.0.0\"\nrepos:\n  - path: \"{}\"\n",
        pack.display()
    );
    let path = write_host(&td, &host);
    let (config, _diags) = load_resolved_with_repos(&path).expect("pack loads");

    let rec = config
        .pointer("/praxec/_packAffinities/design/design")
        .expect("design/design affinity stamped");
    assert_eq!(
        rec.pointer("/recommended").and_then(Value::as_str),
        Some("openrouter/anthropic/claude-sonnet-4-5")
    );
    assert_eq!(
        rec.pointer("/tier").and_then(Value::as_str),
        Some("frontier")
    );
    assert_eq!(
        rec.pointer("/capability").and_then(Value::as_str),
        Some("UI design annealing")
    );
}

#[test]
fn a_pack_without_affinities_stamps_nothing() {
    // Additive + no false positives: a pack that declares no `affinities:` must
    // not synthesize a `_packAffinities` entry (the key is absent entirely when
    // no loaded pack declares one).
    let td = TempDir::new().unwrap();
    let pack = td.path().join("plain-pack");
    write_pack(&pack, "plain", "");
    let host = format!(
        "version: \"1.0.0\"\nrepos:\n  - path: \"{}\"\n",
        pack.display()
    );
    let path = write_host(&td, &host);
    let (config, _diags) = load_resolved_with_repos(&path).expect("pack loads");
    assert!(
        config.pointer("/praxec/_packAffinities").is_none(),
        "no pack declared affinities → no _packAffinities stamp"
    );
}
