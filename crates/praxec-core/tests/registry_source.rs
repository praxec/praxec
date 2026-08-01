//! Task 1 — `discovery.registry` sourced always-latest via the proven `repos:`
//! `{uri, ref}` clone-and-reset machinery.
//!
//! `discovery.registry` may be EITHER a local path string (unchanged) OR an
//! object `{ uri, ref }` resolved — through the exact `repo_git::clone_or_update`
//! path `repos:` entries use — to the local `packs.yaml` of an always-latest
//! clone. These tests exercise the FULL resolve against a real local bare git
//! repo (the transport is mocked with a `file://` bare repo, never skipped),
//! and pin the offline-degrade / fail-fast / no-hash-pin contracts one at a
//! time.

use std::path::{Path, PathBuf};
use std::process::Command;

use praxec_core::config::{self, DiagnosticSeverity};
use serde_json::Value;

// ── fixtures ────────────────────────────────────────────────────────────────

/// A minimal-but-valid `praxec.packs/v3` registry with one searchable tool.
const REGISTRY: &str = r#"
schema: praxec.packs/v3
tools:
  - id: ripgrep
    name: ripgrep
    description: Fast line-oriented search.
    command: rg
    version: 14.1.0
    descriptor:
      schema_version: praxec.tool/v1
      name: ripgrep
      version: 14.1.0
      description: Fast line-oriented search.
      kind: cli
      reach:
        connection_name: rg
        grant_as: rg
        connection:
          kind: cli
          command: rg
          workingDirectory: "."
      operations:
        - id: grep
          verb: search
          input_schema: { type: object }
          output_schema: { type: object }
          cli: { args: ["--json"] }
"#;

fn git(args: &[&str], cwd: &Path) {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawning `git {args:?}` in {}: {e}", cwd.display()));
    assert!(
        out.status.success(),
        "`git {args:?}` in {} failed: {}",
        cwd.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Build a bare git repo at `bare_dir` seeded with a single commit on `main`
/// carrying `packs.yaml` = `body`. Returns the `file://` URL of the bare repo
/// (the exact string `repo_git::clone_url` yields once a `git+https://` uri has
/// been redirected — same shape a CI `url.insteadOf` mock produces).
fn build_bare_registry(bare_dir: &Path, body: &str) -> String {
    let seed = tempfile::tempdir().unwrap();
    std::fs::write(seed.path().join("packs.yaml"), body).unwrap();
    git(&["init", "--quiet", "-b", "main"], seed.path());
    git(&["add", "-A"], seed.path());
    git(
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "--quiet",
            "-m",
            "registry seed",
        ],
        seed.path(),
    );
    let out = Command::new("git")
        .args([
            "clone",
            "--quiet",
            "--bare",
            &seed.path().display().to_string(),
            &bare_dir.display().to_string(),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "bare-cloning registry seed failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    format!("file://{}", bare_dir.display())
}

/// Write a gateway config whose `discovery.registry` is exactly `registry_yaml`
/// (already indented under the `registry:` key), into `dir/praxec.yaml`.
fn write_config(dir: &Path, registry_block: &str) -> PathBuf {
    let config = format!(
        "version: \"1.0.0\"\ngateway:\n  allow_ephemeral: true\ndiscovery:\n  registry: {registry_block}\n"
    );
    let path = dir.join("praxec.yaml");
    std::fs::write(&path, config).unwrap();
    path
}

/// The resolved `discovery.registry` string a load produced.
fn resolved_registry(resolved: &Value) -> &str {
    resolved
        .pointer("/discovery/registry")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("discovery.registry is not a resolved string: {resolved:#?}"))
}

// ── contract 1 — the local-path string form is untouched (regression) ────────

#[test]
fn local_path_string_registry_is_left_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("packs.yaml");
    std::fs::write(&registry_path, REGISTRY).unwrap();

    let cfg = write_config(dir.path(), &registry_path.display().to_string());
    let (resolved, diagnostics) = config::load_resolved_with_repos(&cfg).expect("load");

    assert_eq!(
        resolved_registry(&resolved),
        registry_path.display().to_string(),
        "a local-path `discovery.registry` string must pass through verbatim"
    );
    assert!(
        diagnostics.is_empty(),
        "the local-path form emits no diagnostics: {diagnostics:?}"
    );
}

// ── contract 2 — `{uri, ref}` resolves to the clone's packs.yaml ─────────────

#[test]
fn uri_ref_registry_resolves_to_the_cached_clone_packs_yaml() {
    let dir = tempfile::tempdir().unwrap();
    let bare = dir.path().join("registry.git");
    let uri = build_bare_registry(&bare, REGISTRY);

    let cfg = write_config(dir.path(), &format!("{{ uri: \"{uri}\", ref: main }}"));
    let (resolved, diagnostics) = config::load_resolved_with_repos(&cfg).expect("load");

    let resolved_path = PathBuf::from(resolved_registry(&resolved));
    assert_eq!(
        resolved_path.file_name().and_then(|n| n.to_str()),
        Some("packs.yaml"),
        "the object form resolves to the clone's packs.yaml: {}",
        resolved_path.display()
    );
    // The clone-and-reset machinery actually ran: a working tree exists under the
    // host-local registry cache, and the resolved file is loadable.
    assert!(
        resolved_path.is_file(),
        "resolved packs.yaml must exist on disk: {}",
        resolved_path.display()
    );
    assert!(
        resolved_path
            .components()
            .any(|c| c.as_os_str() == ".praxec"),
        "the clone lands in the host-local `.praxec` cache: {}",
        resolved_path.display()
    );
    praxec_core::registry_v3::Registry::load_path(&resolved_path)
        .expect("the resolved registry loads");
    assert!(
        diagnostics.is_empty(),
        "a reachable uri emits no diagnostics: {diagnostics:?}"
    );
}

// ── contract 3 — offline degrades, does not break ────────────────────────────

#[test]
fn offline_uri_with_an_existing_cache_warns_and_reuses_the_cached_tip() {
    let dir = tempfile::tempdir().unwrap();
    let bare = dir.path().join("registry.git");
    let uri = build_bare_registry(&bare, REGISTRY);
    let cfg = write_config(dir.path(), &format!("{{ uri: \"{uri}\", ref: main }}"));

    // First load: online — seeds the cache.
    let (first, _) = config::load_resolved_with_repos(&cfg).expect("first load seeds cache");
    let cached_path = resolved_registry(&first).to_string();

    // Now make the remote unreachable (delete the bare repo). The cache remains.
    std::fs::remove_dir_all(&bare).unwrap();

    let (second, diagnostics) = config::load_resolved_with_repos(&cfg)
        .expect("offline load must NOT hard-fail with a cache");
    assert_eq!(
        resolved_registry(&second),
        cached_path,
        "offline + cache reuses the last cached tip"
    );
    let warn = diagnostics
        .iter()
        .find(|d| d.code == "DISCOVERY_REGISTRY_OFFLINE")
        .unwrap_or_else(|| panic!("expected a soft offline diagnostic, got: {diagnostics:?}"));
    assert!(matches!(warn.severity, DiagnosticSeverity::Warn));
}

#[test]
fn offline_uri_with_no_cache_fails_fast_naming_the_uri() {
    let dir = tempfile::tempdir().unwrap();
    // A bare repo that never existed — no cache can be seeded.
    let uri = format!("file://{}", dir.path().join("does-not-exist.git").display());
    let cfg = write_config(dir.path(), &format!("{{ uri: \"{uri}\", ref: main }}"));

    let err = config::load_resolved_with_repos(&cfg)
        .expect_err("an unreachable uri with no cache must fail fast");
    let rendered = format!("{err:#}");
    assert!(
        rendered.contains("DISCOVERY_REGISTRY_FETCH_FAILED"),
        "the fail-fast error is typed: {rendered}"
    );
    assert!(
        rendered.contains(&uri),
        "the fail-fast error names the uri: {rendered}"
    );
}

// ── contract 4 — always-latest, NOT hash-pinned ──────────────────────────────

#[test]
fn a_hash_pin_on_the_registry_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_config(
        dir.path(),
        "{ uri: \"git+https://github.com/praxec/packs\", hash: deadbeef }",
    );

    let err = config::load_resolved_with_repos(&cfg)
        .expect_err("a `hash:` freeze on the always-latest registry must be refused");
    assert!(
        format!("{err:#}").contains("DISCOVERY_REGISTRY_HASH_PIN"),
        "the registry rejects hash-pinning (that is the include path): {err:#}"
    );
}
