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

// ── Task A1 — docker connection-body recipe (clears BUILD_RECIPE_UNAVAILABLE) ─

/// The gateway-config schema bytes, single-sourced from the shipped file, so
/// the docker recipe's output is validated against the SAME `mcpConnection`
/// def a granted connection is validated against.
const GATEWAY_CONFIG_SCHEMA: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../schemas/gateway-config.schema.json"
));

/// Extract `scripts.build.connection-body.body` (the real recipe bash) from the
/// provision flow, run it for the given `transport`/`command` inputs with empty
/// secrets/config, and return the emitted connection-body JSON. Runs the actual
/// shipped script — not a hand-copied approximation — so the test tracks the
/// recipe, not a derivation of it.
fn run_build_recipe(transport: &str, command: &str) -> serde_json::Value {
    let flow = examples_dir().join("tool-provision/flow.tools.provision.yaml");
    let doc: serde_yaml::Value =
        serde_yaml::from_str(&std::fs::read_to_string(&flow).expect("flow readable"))
            .expect("flow parses as YAML");
    let body = doc["scripts"]["build.connection-body"]["body"]
        .as_str()
        .expect("build.connection-body.body is a string");

    let dir = tempfile::tempdir().expect("tempdir");
    let script = dir.path().join("build.sh");
    std::fs::write(&script, body).expect("write script");

    // Positional args mirror the flow's `build_*` executor `args:`:
    //   $1 transport  $2 command/image  $3 source_url  $4 secretEnvNames  $5 config
    let out = std::process::Command::new("bash")
        .arg(&script)
        .args([transport, command, "", "[]", "{}"])
        .output()
        .expect("bash runs the recipe");
    assert!(
        out.status.success(),
        "recipe for transport `{transport}` exited non-zero: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "recipe output is not JSON ({e}): {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

/// Same as [`run_build_recipe`] but asserts the recipe FAILS (non-zero exit)
/// and emits NO connection body on stdout, returning its stderr for the
/// caller to assert the typed error marker on.
fn run_build_recipe_expect_fail(transport: &str, command: &str) -> String {
    let flow = examples_dir().join("tool-provision/flow.tools.provision.yaml");
    let doc: serde_yaml::Value =
        serde_yaml::from_str(&std::fs::read_to_string(&flow).expect("flow readable"))
            .expect("flow parses as YAML");
    let body = doc["scripts"]["build.connection-body"]["body"]
        .as_str()
        .expect("build.connection-body.body is a string");

    let dir = tempfile::tempdir().expect("tempdir");
    let script = dir.path().join("build.sh");
    std::fs::write(&script, body).expect("write script");

    let out = std::process::Command::new("bash")
        .arg(&script)
        .args([transport, command, "", "[]", "{}"])
        .output()
        .expect("bash runs the recipe");
    assert!(
        !out.status.success(),
        "recipe for transport `{transport}` / `{command}` was expected to fail but succeeded; stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        out.stdout.is_empty(),
        "a failing build recipe must emit NO connection body, got stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    String::from_utf8_lossy(&out.stderr).into_owned()
}

// ── FIX I1 — docker connection body must run the EXACT pulled/validated image ─

/// Poka-yoke: a docker candidate whose image is UNPINNED (no `:tag`, no
/// `@digest` — discovery's `mcp_registry` adapter sets
/// `ToolSource::Image{image:<pkg-name>}` with no tag) fails fast with
/// `BUILD_REQUIRES_PINNED_IMAGE` and emits no body — `docker run <image>`
/// would otherwise resolve `:latest` at spawn, a different image than
/// `docker pull <image>:<version>` fetched.
#[test]
fn docker_unpinned_image_fails_fast_with_typed_error() {
    let stderr = run_build_recipe_expect_fail("docker", "ghcr.io/praxec/corpus");
    assert!(
        stderr.contains("BUILD_REQUIRES_PINNED_IMAGE"),
        "unpinned docker image must fail with BUILD_REQUIRES_PINNED_IMAGE, got: {stderr}"
    );
}

/// A bare `<pkg-name>` (the untagged mcp_registry shape, no registry host at
/// all) is likewise rejected — a lone name resolves `:latest` on `docker run`.
#[test]
fn docker_bare_untagged_name_fails_fast() {
    let stderr = run_build_recipe_expect_fail("docker", "corpus");
    assert!(
        stderr.contains("BUILD_REQUIRES_PINNED_IMAGE"),
        "bare untagged docker name must fail with BUILD_REQUIRES_PINNED_IMAGE, got: {stderr}"
    );
}

/// A digest-pinned image (`@sha256:…`) is accepted — that is the strongest
/// pin, and it must NOT be mistaken for unpinned just because it lacks `:tag`.
#[test]
fn docker_digest_pinned_image_is_accepted() {
    let image = "ghcr.io/praxec/corpus@sha256:0000000000000000000000000000000000000000000000000000000000000000";
    let body = run_build_recipe("docker", image);
    assert_eq!(body["command"], "docker");
    assert_eq!(
        body["args"],
        serde_json::json!(["run", "--rm", "-i", image]),
        "digest-pinned docker body runs the exact pinned ref"
    );
}

// ── FIX M1 — npm-source stdio candidate wires the npx runnable form ──────────

/// An npm-source candidate (`ToolSource::Npm{pkg}` — the A5 `Provider::Npx`
/// no-op-install path) wires `{command:"npx", args:["-y",<pkg>]}` rather than a
/// bare `<name>` that was never placed on PATH, and that body is schema-valid.
#[test]
fn npm_source_wires_npx_connection_body() {
    let pkg = "@playwright/mcp";
    let body = run_build_recipe("npx", pkg);

    assert_eq!(body["kind"], "mcp", "npx body is an mcp connection");
    assert_eq!(body["command"], "npx", "npm-source body invokes `npx`");
    assert_eq!(
        body["args"],
        serde_json::json!(["-y", pkg]),
        "npx body runs the npm package via `npx -y <pkg>`"
    );

    let schema: serde_json::Value =
        serde_json::from_str(GATEWAY_CONFIG_SCHEMA).expect("schema parses");
    let mcp_conn = schema["$defs"]["mcpConnection"].clone();
    let validator = jsonschema::validator_for(&mcp_conn).expect("mcpConnection def compiles");
    assert!(
        validator.is_valid(&body),
        "npx connection body must satisfy mcpConnection: {body}"
    );
}

/// The `building` step routes an npm-source stdio candidate to the npx recipe
/// (a `build_stdio_npx` transition guarded on `npmPkg != null`), while a plain
/// binary stdio source (`npmPkg == null`) keeps the bare-`{command}` recipe.
#[test]
fn stdio_lane_splits_npm_source_from_binary() {
    let flow = examples_dir().join("tool-provision/flow.tools.provision.yaml");
    let text = std::fs::read_to_string(&flow).expect("flow readable");

    assert!(
        text.contains("build_stdio_npx"),
        "a `build_stdio_npx` transition must wire the npx recipe for an npm source"
    );
    assert!(
        text.contains("$.context.npmPkg != null"),
        "the npx lane must be guarded on the candidate carrying an npm package"
    );
    assert!(
        text.contains("$.context.npmPkg == null"),
        "the bare-binary stdio lane must exclude the npm-source case"
    );
    // The stale claim that npx is not a provider must be gone.
    assert!(
        !text.contains("no npm/npx provider anymore"),
        "the stale `no npm/npx provider anymore` comment must be corrected"
    );
}

/// A docker-transport candidate reaches a valid connection body — `docker run
/// --rm -i <image>` as an mcp command — instead of the old
/// `BUILD_RECIPE_UNAVAILABLE` dead-end, and that body is schema-valid against
/// `gateway-config.schema.json#/$defs/mcpConnection`.
#[test]
fn docker_build_recipe_produces_schema_valid_mcp_body() {
    let image = "ghcr.io/praxec/corpus:0.0.2";
    let body = run_build_recipe("docker", image);

    assert_eq!(body["kind"], "mcp", "docker body is an mcp connection");
    assert_eq!(body["command"], "docker", "docker body invokes `docker`");
    assert_eq!(
        body["args"],
        serde_json::json!(["run", "--rm", "-i", image]),
        "docker body runs the pulled image via `docker run --rm -i <image>`"
    );

    // Schema-valid against the real mcpConnection def (self-contained — no
    // external `$ref`s), the same def a granted staged connection must pass.
    let schema: serde_json::Value =
        serde_json::from_str(GATEWAY_CONFIG_SCHEMA).expect("schema parses");
    let mcp_conn = schema["$defs"]["mcpConnection"].clone();
    let validator = jsonschema::validator_for(&mcp_conn).expect("mcpConnection def compiles");
    assert!(
        validator.is_valid(&body),
        "docker connection body must satisfy mcpConnection: {body}"
    );
}

/// Grep-clean: the docker lane no longer dead-ends at
/// `BUILD_RECIPE_UNAVAILABLE`. A `build_docker_mcp` transition exists and the
/// `build_unsupported` fall-through explicitly excludes a docker candidate that
/// carries an image (the rest-with-secrets typed error is a separate, genuine
/// unsupported case and is intentionally left intact).
#[test]
fn docker_transport_has_a_build_recipe_no_dead_end() {
    let flow = examples_dir().join("tool-provision/flow.tools.provision.yaml");
    let text = std::fs::read_to_string(&flow).expect("flow readable");

    assert!(
        text.contains("build_docker_mcp"),
        "a `build_docker_mcp` transition must wire the docker recipe"
    );
    assert!(
        text.contains("dockerImage"),
        "the docker image must be projected into context as `dockerImage`"
    );
    // The `building` step's docker lane routes to the recipe, not to the
    // BUILD_RECIPE_UNAVAILABLE fall-through: the fall-through guard now excludes
    // `transport == 'docker' && dockerImage != null`.
    assert!(
        text.contains("$.context.transport == 'docker'")
            && text.contains("$.context.dockerImage != null"),
        "build_unsupported must exclude the docker+image case"
    );
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
