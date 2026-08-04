//! onboarding-hardening Cluster 1 — config-readiness at the CLI surface.
//!
//! These are the report repros as end-to-end exit-code assertions (the
//! load-bearing "turns silent into loud" behavior), plus the no-false-positive
//! direction for each:
//!
//! - **D1** — a DECLARED but unreadable `gateway.models_yaml` makes `check` AND
//!   `doctor` exit NON-ZERO with `MODELS_YAML_LOAD_FAILED` (today both exit 0).
//! - **Keystone** — a MOUNTED pack whose agent step uses an UNBOUND affinity
//!   makes `check` exit non-zero with `AFFINITY_UNBOUND` + the pack's
//!   recommendation; binding it makes `check` exit 0.
//! - No false positives — a config with NO agent/affinity step and a loadable
//!   models.yaml exits 0; a bound (incl. default-chain) affinity does not fire.
//! - `models bind` writes the recommended binding and is idempotent.
//! - `init` terminates in a `doctor` readiness verdict.

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

/// A commodity-first models.yaml with only a `default:` chain (no `design` key).
const MODELS_DEFAULT_ONLY: &str =
    "version: 1\ndefault:\n  - { provider: { name: openrouter }, model: z-ai/glm-5.2 }\n";

/// A host config declaring `models_yaml` + one agent step on `affinity`.
fn agent_config(models_yaml: &str, affinity: &str) -> String {
    format!(
        "version: \"1.0.0\"\ngateway:\n  allow_ephemeral: true\n  models_yaml: \"{models_yaml}\"\n\
         workflows:\n  wf.demo:\n    title: Demo\n    initialState: start\n    states:\n      \
         start:\n        transitions:\n          go:\n            target: done\n            \
         actor: agent\n            executor: {{ kind: agent, affinity: {affinity}, goal: \"do it\" }}\n      \
         done: {{ terminal: true }}\n"
    )
}

// ── D1 ──────────────────────────────────────────────────────────────────────

#[test]
fn d1_declared_but_missing_models_yaml_fails_check_and_doctor() {
    let td = tempfile::tempdir().unwrap();
    let cfg = td.path().join("gw.yaml");
    // A MODEL-CONSUMING config (a `kind: agent` step) whose declared models.yaml
    // does not exist — the field-report design-annealing shape. D1 is a hard error
    // here (every agent step would fail at dispatch).
    write(
        &cfg,
        "version: \"1.0.0\"\ngateway:\n  allow_ephemeral: true\n  models_yaml: \"NONEXISTENT.yaml\"\n\
         workflows:\n  wf.agent:\n    title: Agent\n    initialState: start\n    states:\n      \
         start:\n        transitions:\n          go:\n            target: done\n            actor: agent\n            \
         executor: { kind: agent, affinity: coding, goal: \"x\" }\n      \
         done: { terminal: true }\n",
    );

    let check = run(&["check", "--config", cfg.to_str().unwrap()]);
    assert!(
        !check.status.success(),
        "check must FAIL on a dangling models.yaml (the D1 bug was exit 0):\n{}",
        String::from_utf8_lossy(&check.stdout)
    );
    assert!(
        String::from_utf8_lossy(&check.stdout).contains("MODELS_YAML_LOAD_FAILED"),
        "check names the failure:\n{}",
        String::from_utf8_lossy(&check.stdout)
    );

    let doctor = run(&["doctor", "--config", cfg.to_str().unwrap()]);
    assert!(
        !doctor.status.success(),
        "doctor must FAIL on a dangling models.yaml too (was exit 0):\n{}",
        String::from_utf8_lossy(&doctor.stdout)
    );
    assert!(
        String::from_utf8_lossy(&doctor.stdout).contains("MODELS_YAML_LOAD_FAILED"),
        "doctor names the failure:\n{}",
        String::from_utf8_lossy(&doctor.stdout)
    );
}

#[test]
fn d1_dangling_models_yaml_without_agent_steps_is_a_nonfatal_warning() {
    // C2 / frictionless-upgrade: a config that consumes NO models (only `kind: noop`)
    // must NOT hard-fail on a broken models_yaml — a valid 0.0.47 config upgrades
    // clean. The broken key is surfaced as a warning; check exits 0.
    let td = tempfile::tempdir().unwrap();
    let cfg = td.path().join("gw.yaml");
    write(
        &cfg,
        "version: \"1.0.0\"\ngateway:\n  allow_ephemeral: true\n  models_yaml: \"NONEXISTENT.yaml\"\n\
         workflows:\n  wf.plain:\n    title: Plain\n    initialState: start\n    states:\n      \
         start:\n        transitions:\n          go: { target: done, executor: { kind: noop } }\n      \
         done: { terminal: true }\n",
    );
    let check = run(&["check", "--config", cfg.to_str().unwrap()]);
    let out = String::from_utf8_lossy(&check.stdout);
    assert!(
        check.status.success(),
        "a dangling models.yaml with no model-consuming step must NOT fail check:\n{out}"
    );
    assert!(
        out.contains("MODELS_YAML_LOAD_FAILED"),
        "but it is still surfaced as a warning:\n{out}"
    );
}

#[test]
fn no_agent_steps_with_loadable_models_yaml_passes_check() {
    // No false positive: a config with no agent/affinity step and a loadable
    // models.yaml is clean — D1/keystone must not newly error.
    let td = tempfile::tempdir().unwrap();
    let models = td.path().join("models.yaml");
    write(&models, MODELS_DEFAULT_ONLY);
    let cfg = td.path().join("gw.yaml");
    write(
        &cfg,
        &format!(
            "version: \"1.0.0\"\ngateway:\n  allow_ephemeral: true\n  models_yaml: \"{}\"\n\
             workflows:\n  wf.plain:\n    title: Plain\n    initialState: start\n    states:\n      \
             start:\n        transitions:\n          go: {{ target: done, executor: {{ kind: noop }} }}\n      \
             done: {{ terminal: true }}\n",
            models.display()
        ),
    );
    let check = run(&["check", "--config", cfg.to_str().unwrap()]);
    let out = String::from_utf8_lossy(&check.stdout);
    assert!(check.status.success(), "must exit 0:\n{out}");
    assert!(
        !out.contains("AFFINITY_UNBOUND") && !out.contains("MODELS_YAML_LOAD_FAILED"),
        "no readiness error on a clean no-agent config:\n{out}"
    );
}

// ── keystone ─────────────────────────────────────────────────────────────────

#[test]
fn unbound_agent_affinity_fails_check_bound_passes() {
    let td = tempfile::tempdir().unwrap();
    let models = td.path().join("models.yaml");
    let cfg = td.path().join("gw.yaml");
    write(&cfg, &agent_config(models.to_str().unwrap(), "design"));

    // Unbound (`design` is an open key with no activity entry, no default match) → fail.
    write(&models, MODELS_DEFAULT_ONLY);
    let unbound = run(&["check", "--config", cfg.to_str().unwrap()]);
    let out = String::from_utf8_lossy(&unbound.stdout);
    assert!(
        !unbound.status.success(),
        "unbound affinity must FAIL check:\n{out}"
    );
    assert!(
        out.contains("AFFINITY_UNBOUND"),
        "names the invariant:\n{out}"
    );
    assert!(out.contains("`design`"), "names the affinity:\n{out}");

    // Bound via an `activity:` entry → pass.
    write(
        &models,
        "version: 1\ndefault:\n  - { provider: { name: openrouter }, model: z-ai/glm-5.2 }\n\
         activity:\n  design:\n    - { provider: { name: openrouter }, model: anthropic/claude-sonnet-4-5 }\n",
    );
    let bound = run(&["check", "--config", cfg.to_str().unwrap()]);
    assert!(
        bound.status.success(),
        "a bound affinity must PASS check:\n{}",
        String::from_utf8_lossy(&bound.stdout)
    );
}

#[test]
fn default_chain_bound_known_affinity_does_not_fire() {
    // A KNOWN affinity token (`coding`) with no override/activity entry resolves
    // via the `default:` chain — bound, must NOT be flagged (no false positive).
    let td = tempfile::tempdir().unwrap();
    let models = td.path().join("models.yaml");
    write(&models, MODELS_DEFAULT_ONLY);
    let cfg = td.path().join("gw.yaml");
    write(&cfg, &agent_config(models.to_str().unwrap(), "coding"));
    let check = run(&["check", "--config", cfg.to_str().unwrap()]);
    assert!(
        check.status.success(),
        "default-chain-bound affinity must pass:\n{}",
        String::from_utf8_lossy(&check.stdout)
    );
}

// ── pack recommendation + models bind ────────────────────────────────────────

/// Write a local-path pack that declares the `design` affinity (+ recommendation)
/// and a capability that uses it. Returns the config path.
fn pack_config(td: &Path) -> std::path::PathBuf {
    let pack = td.join("pack");
    std::fs::create_dir_all(pack.join("capabilities")).unwrap();
    write(
        &pack.join("praxec.repo.yaml"),
        "schema: praxec.repo/v1\nname: design-pack\nnamespace: design\nversion: 0.1.0\n\
         affinities:\n  design:\n    tier: frontier\n    capability: UI design annealing\n    \
         recommended: openrouter/anthropic/claude-sonnet-4-5\n",
    );
    write(
        &pack.join("capabilities/flow.anneal.yaml"),
        "workflows:\n  flow.anneal:\n    title: Anneal\n    initialState: start\n    states:\n      \
         start:\n        transitions:\n          go:\n            target: done\n            actor: agent\n            \
         executor: { kind: agent, affinity: design, goal: \"anneal\" }\n      done: { terminal: true }\n",
    );
    let models = td.join("models.yaml");
    write(&models, MODELS_DEFAULT_ONLY);
    let cfg = td.join("packcfg.yaml");
    write(
        &cfg,
        &format!(
            "version: \"1.0.0\"\ngateway:\n  allow_ephemeral: true\n  models_yaml: \"{}\"\n\
             repos:\n  - path: \"{}\"\n",
            models.display(),
            pack.display()
        ),
    );
    cfg
}

#[test]
fn check_surfaces_pack_recommendation_for_unbound_mounted_affinity() {
    let td = tempfile::tempdir().unwrap();
    let cfg = pack_config(td.path());
    let check = run(&["check", "--config", cfg.to_str().unwrap()]);
    let out = String::from_utf8_lossy(&check.stdout);
    assert!(
        !check.status.success(),
        "unbound mounted affinity fails:\n{out}"
    );
    assert!(out.contains("AFFINITY_UNBOUND"), "{out}");
    assert!(
        out.contains("design/flow.anneal"),
        "names the mounted def:\n{out}"
    );
    assert!(
        out.contains("openrouter/anthropic/claude-sonnet-4-5"),
        "surfaces the pack recommendation:\n{out}"
    );
    assert!(
        out.contains("praxec models bind design"),
        "offers the fix:\n{out}"
    );
}

#[test]
fn models_bind_writes_recommended_binding_and_is_idempotent() {
    let td = tempfile::tempdir().unwrap();
    let cfg = pack_config(td.path());
    let models = td.path().join("models.yaml");

    // First bind writes the recommended binding (OPENROUTER_API_KEY is set in the
    // test env, so the write path is taken).
    let first = run(&[
        "models",
        "bind",
        "design",
        "--config",
        cfg.to_str().unwrap(),
    ]);
    assert!(
        first.status.success(),
        "bind should succeed:\n{}",
        String::from_utf8_lossy(&first.stdout)
    );
    let after = std::fs::read_to_string(&models).unwrap();
    assert!(
        after.contains("anthropic/claude-sonnet-4-5"),
        "binding written into models.yaml:\n{after}"
    );

    // Second bind is idempotent + non-clobbering.
    let second = run(&[
        "models",
        "bind",
        "design",
        "--config",
        cfg.to_str().unwrap(),
    ]);
    assert!(second.status.success());
    assert!(
        String::from_utf8_lossy(&second.stdout).contains("already bound"),
        "second bind reports already-bound:\n{}",
        String::from_utf8_lossy(&second.stdout)
    );

    // And the config now passes check (self-wired).
    let check = run(&["check", "--config", cfg.to_str().unwrap()]);
    assert!(
        check.status.success(),
        "config runnable after bind:\n{}",
        String::from_utf8_lossy(&check.stdout)
    );
}

#[test]
fn models_bind_unrecommended_affinity_exits_nonzero() {
    let td = tempfile::tempdir().unwrap();
    let cfg = pack_config(td.path());
    let out = run(&[
        "models",
        "bind",
        "no-such-affinity",
        "--config",
        cfg.to_str().unwrap(),
    ]);
    assert!(
        !out.status.success(),
        "binding an affinity no pack recommends must exit non-zero:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

// ── D7: init ends in doctor ──────────────────────────────────────────────────

#[test]
fn init_ends_in_a_doctor_verdict() {
    let td = tempfile::tempdir().unwrap();
    let dir = td.path().join("scaffold");
    let out = run(&[
        "init",
        "--dir",
        dir.to_str().unwrap(),
        "--editor",
        "none",
        "--yes",
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("running doctor on the scaffolded config"),
        "init must run doctor on the result:\n{stdout}"
    );
    // A readiness verdict line from doctor (preflight is part of the doctor run).
    assert!(
        stdout.contains("preflight:"),
        "init's doctor epilogue prints a readiness verdict:\n{stdout}"
    );
}
