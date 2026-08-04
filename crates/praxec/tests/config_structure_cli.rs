//! onboarding-hardening Cluster 2 — config as closed structure + discovery.
//!
//! The report repros as end-to-end CLI assertions, each with its
//! no-false-positive direction:
//!
//! - **D2** — a `models_yaml` misplaced under `praxec:` (or top-level), or an
//!   unknown key in the closed `gateway:` block, makes `check` exit NON-ZERO
//!   with a "did you mean `gateway.models_yaml`?" hint / the allowed-key set
//!   (today: silently accepted + ignored). A correct `gateway.models_yaml`
//!   still passes.
//! - **D3** — `praxec schema models-config` prints a non-empty JSON Schema
//!   documenting `default`/`overrides`/`activity` + the binding entry (today
//!   `schema` exposes only `audit-event`).
//! - **D4** — `health` and `doctor` echo the ABSOLUTE, in-force config path.

use std::path::Path;
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_praxec")
}

fn run(args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .env("OPENROUTER_API_KEY", "sk-test-key")
        .output()
        .expect("run praxec")
}

fn write(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
}

const MODELS_DEFAULT_ONLY: &str =
    "version: 1\ndefault:\n  - { provider: { name: openrouter }, model: z-ai/glm-5.2 }\n";

/// A minimal, loadable config with a single plain `kind: noop` workflow and a
/// custom `gateway:`/`praxec:` prelude spliced in.
fn config_with_prelude(prelude: &str) -> String {
    format!(
        "version: \"1.0.0\"\n{prelude}\
         workflows:\n  wf.plain:\n    title: Plain\n    initialState: start\n    states:\n      \
         start:\n        transitions:\n          go: {{ target: done, executor: {{ kind: noop }} }}\n      \
         done: {{ terminal: true }}\n"
    )
}

// ── D2 — misplaced / unknown key ─────────────────────────────────────────────

#[test]
fn models_yaml_under_praxec_fails_check_with_hint() {
    let td = tempfile::tempdir().unwrap();
    let models = td.path().join("models.yaml");
    write(&models, MODELS_DEFAULT_ONLY);
    let cfg = td.path().join("gw.yaml");
    // The exact report repro: `models_yaml` misplaced under `praxec:` (ignored
    // by the loader) — no `gateway.models_yaml` at all.
    write(
        &cfg,
        &config_with_prelude(&format!(
            "gateway:\n  allow_ephemeral: true\npraxec:\n  models_yaml: \"{}\"\n",
            models.display()
        )),
    );
    let check = run(&["check", "--config", cfg.to_str().unwrap()]);
    let out = String::from_utf8_lossy(&check.stdout);
    assert!(
        !check.status.success(),
        "misplaced models_yaml must FAIL check (was silently ignored):\n{out}"
    );
    assert!(
        out.contains("MODELS_YAML_MISPLACED"),
        "names the defect:\n{out}"
    );
    assert!(
        out.contains("gateway.models_yaml"),
        "carries the did-you-mean hint:\n{out}"
    );
}

#[test]
fn unknown_gateway_key_fails_check_with_allowed_set() {
    let td = tempfile::tempdir().unwrap();
    let cfg = td.path().join("gw.yaml");
    // A typo'd gateway key — silently ignored by the loader, now a hard error.
    write(
        &cfg,
        &config_with_prelude("gateway:\n  allow_ephemeral: true\n  strict_validaton: true\n"),
    );
    let check = run(&["check", "--config", cfg.to_str().unwrap()]);
    let out = String::from_utf8_lossy(&check.stdout);
    assert!(
        !check.status.success(),
        "unknown gateway key must FAIL check:\n{out}"
    );
    assert!(
        out.contains("UNKNOWN_GATEWAY_KEY"),
        "names the defect:\n{out}"
    );
    assert!(
        out.contains("strict_validaton"),
        "names the offending key:\n{out}"
    );
    assert!(
        out.contains("strict_validation"),
        "lists the allowed set:\n{out}"
    );
}

#[test]
fn correct_gateway_models_yaml_still_passes_check() {
    // No false positive — the CORRECT location must not be flagged.
    let td = tempfile::tempdir().unwrap();
    let models = td.path().join("models.yaml");
    write(&models, MODELS_DEFAULT_ONLY);
    let cfg = td.path().join("gw.yaml");
    write(
        &cfg,
        &config_with_prelude(&format!(
            "gateway:\n  allow_ephemeral: true\n  models_yaml: \"{}\"\n",
            models.display()
        )),
    );
    let check = run(&["check", "--config", cfg.to_str().unwrap()]);
    let out = String::from_utf8_lossy(&check.stdout);
    assert!(check.status.success(), "correct config must pass:\n{out}");
    assert!(
        !out.contains("MODELS_YAML_MISPLACED") && !out.contains("UNKNOWN_GATEWAY_KEY"),
        "no D2 false positive:\n{out}"
    );
}

// ── D3 — schema models-config ────────────────────────────────────────────────

#[test]
fn schema_models_config_prints_the_shape() {
    let out = run(&["schema", "models-config"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "schema models-config must exit 0:\n{stdout}"
    );
    for key in [
        "version",
        "default",
        "overrides",
        "activity",
        "provider",
        "model",
    ] {
        assert!(stdout.contains(key), "schema documents `{key}`:\n{stdout}");
    }
    // It is valid JSON.
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("schema output is valid JSON");
    assert!(parsed.pointer("/properties/default").is_some(), "{stdout}");
}

// ── D4 — echo the in-force config path ───────────────────────────────────────

#[test]
fn health_echoes_absolute_config_path() {
    let td = tempfile::tempdir().unwrap();
    let cfg = td.path().join("gw.yaml");
    write(
        &cfg,
        &config_with_prelude("gateway:\n  allow_ephemeral: true\n"),
    );
    let out = run(&["health", "--config", cfg.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "health must exit 0:\n{stdout}");
    let snapshot: serde_json::Value =
        serde_json::from_str(&stdout).expect("health prints one JSON line");
    let path = snapshot
        .get("config_path")
        .and_then(|v| v.as_str())
        .expect("health snapshot carries config_path");
    assert!(
        Path::new(path).is_absolute(),
        "config_path is absolute: {path}"
    );
    // Names THIS config file (canonicalized basename survives).
    assert!(path.ends_with("gw.yaml"), "names the in-force file: {path}");
}

#[test]
fn doctor_echoes_absolute_config_path() {
    let td = tempfile::tempdir().unwrap();
    let models = td.path().join("models.yaml");
    write(&models, MODELS_DEFAULT_ONLY);
    let cfg = td.path().join("gw.yaml");
    write(
        &cfg,
        &config_with_prelude(&format!(
            "gateway:\n  allow_ephemeral: true\n  models_yaml: \"{}\"\n",
            models.display()
        )),
    );
    // doctor may exit non-zero on durability/credentials; we only assert it
    // ECHOES the absolute in-force path (D4).
    let out = run(&["doctor", "--config", cfg.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("config in force:"),
        "doctor labels the path:\n{stdout}"
    );
    let canon = std::fs::canonicalize(&cfg).unwrap();
    assert!(
        stdout.contains(&canon.display().to_string()),
        "doctor echoes the ABSOLUTE path {}:\n{stdout}",
        canon.display()
    );
}
