//! SPEC §9 — multi-repo loading. Acceptance milestone M1 + V19–V23
//! accepts/rejects pairs.
//!
//! Fixtures live under `tests/fixtures/repos/`:
//! - `swe-core/` — namespace `swe`, ships `cap.plan.vet` +
//!   `flow.add-feature` (the latter references the capability via an
//!   UNPREFIXED `cap.plan.vet`, which `load_repo` rewrites to
//!   `swe/cap.plan.vet`).
//! - `quality-core/` — namespace `quality`, ships its own
//!   `cap.plan.vet`. Proves two namespaces can share an id without
//!   collision (M1).
//! - `dupe-namespace-{a,b}/` — both declare `namespace: dupe`; used
//!   to assert V20 fires.
//!
//! Tests construct host gateway-config YAML on the fly (via tempfile) so
//! the test owns the `repos:` declarations and any host-level overrides.
//! Repo paths in the host config resolve relative to the host file's
//! directory — we point them at the on-disk fixtures via absolute paths
//! to keep the tests location-agnostic.

use std::path::PathBuf;

use praxec_core::config::load_resolved_with_repos;
use serde_json::Value;
use tempfile::TempDir;

/// Absolute path to `tests/fixtures/repos`.
fn fixtures_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests");
    p.push("fixtures");
    p.push("repos");
    p
}

/// Write `body` to `<tempdir>/praxec.yaml` and return the path.
fn write_host(td: &TempDir, body: &str) -> PathBuf {
    let p = td.path().join("praxec.yaml");
    std::fs::write(&p, body).unwrap();
    p
}

// ---------- M1 acceptance ----------

#[test]
fn two_repos_with_distinct_namespaces_load_both_capabilities() {
    let td = TempDir::new().unwrap();
    let host = format!(
        r#"
version: "1.0.0"
repos:
  - path: "{swe}"
  - path: "{quality}"
"#,
        swe = fixtures_root().join("swe-core").display(),
        quality = fixtures_root().join("quality-core").display(),
    );
    let path = write_host(&td, &host);
    let (config, diagnostics) =
        load_resolved_with_repos(&path).expect("two-repo load should succeed");
    assert!(
        diagnostics.is_empty(),
        "no soft diagnostics expected: {diagnostics:?}"
    );

    let workflows = config
        .pointer("/workflows")
        .and_then(Value::as_object)
        .expect("workflows present");
    assert!(
        workflows.contains_key("swe/cap.plan.vet"),
        "expected swe-prefixed key; got {:?}",
        workflows.keys().collect::<Vec<_>>()
    );
    assert!(
        workflows.contains_key("quality/cap.plan.vet"),
        "expected quality-prefixed key; got {:?}",
        workflows.keys().collect::<Vec<_>>()
    );
    assert!(
        workflows.contains_key("swe/flow.add-feature"),
        "flow from swe-core should load"
    );

    // The unprefixed `definitionId: cap.plan.vet` reference inside
    // `swe/flow.add-feature` should be rewritten to `swe/cap.plan.vet`.
    let resolved_ref = config
        .pointer("/workflows/swe~1flow.add-feature/states/planning/transitions/plan_drafted/executor/definitionId")
        .and_then(Value::as_str)
        .expect("resolved ref present");
    assert_eq!(resolved_ref, "swe/cap.plan.vet");
}

// ---------- V19 — repo manifest schema ----------

#[test]
fn v19_accepts_well_formed_manifest() {
    // Implicitly covered by M1, but assert explicitly so the rule is
    // discoverable by name from the validation-parity script (PR3).
    let td = TempDir::new().unwrap();
    let host = format!(
        r#"
version: "1.0.0"
repos:
  - path: "{swe}"
"#,
        swe = fixtures_root().join("swe-core").display(),
    );
    let path = write_host(&td, &host);
    let (_config, _diagnostics) =
        load_resolved_with_repos(&path).expect("well-formed manifest loads");
}

#[test]
fn v19_rejects_manifest_with_wrong_schema_constant() {
    let td = TempDir::new().unwrap();
    let repo_dir = td.path().join("bad-schema-repo");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::write(
        repo_dir.join("praxec.repo.yaml"),
        "schema: praxec.repo/v999\nname: bad\nnamespace: bad\nversion: 0.1.0\n",
    )
    .unwrap();
    let host = format!(
        "version: \"1.0.0\"\nrepos:\n  - path: \"{}\"\n",
        repo_dir.display()
    );
    let path = write_host(&td, &host);
    let err = load_resolved_with_repos(&path).expect_err("wrong schema must error");
    let msg = format!("{:#}", err);
    assert!(msg.contains("praxec.repo/v1"), "msg: {msg}");
}

// ---------- V20 — two repos sharing a namespace ----------

#[test]
fn v20_accepts_distinct_namespaces() {
    // Covered by M1, but kept as a named test to satisfy parity scanner.
    let td = TempDir::new().unwrap();
    let host = format!(
        r#"
version: "1.0.0"
repos:
  - path: "{swe}"
  - path: "{quality}"
"#,
        swe = fixtures_root().join("swe-core").display(),
        quality = fixtures_root().join("quality-core").display(),
    );
    let path = write_host(&td, &host);
    load_resolved_with_repos(&path).expect("distinct namespaces accepted");
}

#[test]
fn v20_rejects_two_repos_with_same_namespace() {
    let td = TempDir::new().unwrap();
    let host = format!(
        r#"
version: "1.0.0"
repos:
  - path: "{a}"
  - path: "{b}"
"#,
        a = fixtures_root().join("dupe-namespace-a").display(),
        b = fixtures_root().join("dupe-namespace-b").display(),
    );
    let path = write_host(&td, &host);
    let err = load_resolved_with_repos(&path).expect_err("namespace collision must error");
    let msg = format!("{:#}", err);
    assert!(msg.contains("DUPLICATE_REPO_NAMESPACE"), "msg: {msg}");
    assert!(msg.contains("dupe"), "should name the namespace: {msg}");
}

// ---------- V21 — duplicate ids inside one repo ----------

#[test]
fn v21_accepts_single_id_per_repo() {
    let td = TempDir::new().unwrap();
    let host = format!(
        "version: \"1.0.0\"\nrepos:\n  - path: \"{}\"\n",
        fixtures_root().join("swe-core").display(),
    );
    let path = write_host(&td, &host);
    load_resolved_with_repos(&path).expect("unique ids per repo");
}

#[test]
fn v21_rejects_duplicate_definition_within_one_repo() {
    let td = TempDir::new().unwrap();
    let repo_dir = td.path().join("dup-defs-repo");
    std::fs::create_dir_all(repo_dir.join("capabilities")).unwrap();
    std::fs::write(
        repo_dir.join("praxec.repo.yaml"),
        "schema: praxec.repo/v1\nname: dup\nnamespace: dup\nversion: 0.1.0\n",
    )
    .unwrap();
    std::fs::write(
        repo_dir.join("capabilities/a.yaml"),
        "workflows:\n  cap.collide:\n    title: A\n",
    )
    .unwrap();
    std::fs::write(
        repo_dir.join("capabilities/b.yaml"),
        "workflows:\n  cap.collide:\n    title: B\n",
    )
    .unwrap();
    let host = format!(
        "version: \"1.0.0\"\nrepos:\n  - path: \"{}\"\n",
        repo_dir.display()
    );
    let path = write_host(&td, &host);
    let err = load_resolved_with_repos(&path).expect_err("duplicate id must error");
    let msg = format!("{:#}", err);
    assert!(msg.contains("DUPLICATE_REPO_DEF"), "msg: {msg}");
    assert!(msg.contains("dup/cap.collide"), "msg should name id: {msg}");
}

// ---------- V22 — unprefixed cross-repo (unresolved) ref ----------

#[test]
fn v22_accepts_unprefixed_ref_that_resolves_in_current_namespace() {
    // swe/flow.add-feature references `cap.plan.vet` (unprefixed). Repo
    // loading rewrites it to `swe/cap.plan.vet`, which IS loaded. So it
    // resolves. This is the only positive test we need — the rewriting
    // is the mechanism that makes the positive case work.
    let td = TempDir::new().unwrap();
    let host = format!(
        "version: \"1.0.0\"\nrepos:\n  - path: \"{}\"\n",
        fixtures_root().join("swe-core").display(),
    );
    let path = write_host(&td, &host);
    load_resolved_with_repos(&path).expect("intra-namespace ref resolves");
}

#[test]
fn v22_rejects_workflow_ref_that_does_not_resolve() {
    let td = TempDir::new().unwrap();
    let repo_dir = td.path().join("unresolved-ref-repo");
    std::fs::create_dir_all(repo_dir.join("flows")).unwrap();
    std::fs::write(
        repo_dir.join("praxec.repo.yaml"),
        "schema: praxec.repo/v1\nname: ur\nnamespace: ur\nversion: 0.1.0\n",
    )
    .unwrap();
    // References cap.missing — never defined anywhere.
    std::fs::write(
        repo_dir.join("flows/flow.broken.yaml"),
        r#"
workflows:
  flow.broken:
    initial: s
    states:
      s:
        transitions:
          t:
            target: done
            executor:
              kind: workflow
              definitionId: cap.missing
      done:
        terminal: true
"#,
    )
    .unwrap();
    let host = format!(
        "version: \"1.0.0\"\nrepos:\n  - path: \"{}\"\n",
        repo_dir.display()
    );
    let path = write_host(&td, &host);
    let err = load_resolved_with_repos(&path).expect_err("unresolved ref must error");
    let msg = format!("{:#}", err);
    assert!(msg.contains("UNRESOLVED_WORKFLOW_REF"), "msg: {msg}");
    // After namespace-prefixing the unprefixed ref `cap.missing` becomes
    // `ur/cap.missing` — that's the name V22 reports.
    assert!(
        msg.contains("ur/cap.missing"),
        "msg should name the unresolved id: {msg}"
    );
}

// ---------- V23 — anonymous shadowing via host include ----------

#[test]
fn v23_accepts_explicit_override_of_repo_provided_id() {
    let td = TempDir::new().unwrap();
    let host = format!(
        r#"
version: "1.0.0"
repos:
  - path: "{swe}"
overrides:
  - swe/cap.plan.vet
workflows:
  swe/cap.plan.vet:
    title: Operator-customized vet
    initial: ready
    states:
      ready:
        terminal: true
"#,
        swe = fixtures_root().join("swe-core").display(),
    );
    let path = write_host(&td, &host);
    let (config, _diagnostics) =
        load_resolved_with_repos(&path).expect("explicit override should be accepted");
    // Host wins on the explicitly declared override.
    let title = config
        .pointer("/workflows/swe~1cap.plan.vet/title")
        .and_then(Value::as_str)
        .unwrap();
    assert_eq!(title, "Operator-customized vet");
}

#[test]
fn v23_rejects_anonymous_shadowing_without_overrides_declaration() {
    let td = TempDir::new().unwrap();
    let host = format!(
        r#"
version: "1.0.0"
repos:
  - path: "{swe}"
workflows:
  swe/cap.plan.vet:
    title: Silent shadow attempt
    initial: ready
    states:
      ready:
        terminal: true
"#,
        swe = fixtures_root().join("swe-core").display(),
    );
    let path = write_host(&td, &host);
    let err = load_resolved_with_repos(&path).expect_err("anonymous shadow must error");
    let msg = format!("{:#}", err);
    assert!(msg.contains("ANONYMOUS_OVERRIDE"), "msg: {msg}");
    assert!(
        msg.contains("swe/cap.plan.vet"),
        "msg should name the id: {msg}"
    );
}

// ---------- SPEC §8.4 — `writable: true` opt-in carries the authoring target ----------

#[test]
fn writable_repo_is_stamped_into_resolved_config() {
    let td = TempDir::new().unwrap();
    let swe = fixtures_root().join("swe-core");
    let host = format!(
        "version: \"1.0.0\"\nrepos:\n  - path: \"{}\"\n    writable: true\n",
        swe.display(),
    );
    let path = write_host(&td, &host);
    let (config, _diagnostics) = load_resolved_with_repos(&path).expect("writable repo loads");
    let roots = config
        .pointer("/praxec/_writableRepos")
        .and_then(Value::as_array)
        .expect("writable roots stamped");
    assert_eq!(roots.len(), 1, "exactly one writable repo: {roots:?}");
    assert_eq!(
        roots[0]["root"].as_str(),
        Some(swe.display().to_string().as_str()),
        "the swe-core absolute root is recorded"
    );
    assert_eq!(
        roots[0]["push"],
        serde_json::json!(false),
        "push defaults off"
    );
}

#[test]
fn repos_default_to_read_only_with_no_writable_stamp() {
    let td = TempDir::new().unwrap();
    let host = format!(
        "version: \"1.0.0\"\nrepos:\n  - path: \"{}\"\n",
        fixtures_root().join("swe-core").display(),
    );
    let path = write_host(&td, &host);
    let (config, _diagnostics) = load_resolved_with_repos(&path).expect("read-only repo loads");
    assert!(
        config.pointer("/praxec/_writableRepos").is_none(),
        "no writable target should be stamped when none is declared writable"
    );
}

// ---------- SPEC §9 — remote repo import (clone + layer) ----------

/// Build a local git "origin" repo with a manifest (namespace `imported`) and
/// one capability, so it can be cloned over `file://` without a network.
fn seed_origin_repo(dir: &std::path::Path) {
    use std::process::Command;
    std::fs::create_dir_all(dir.join("capabilities")).unwrap();
    std::fs::write(
        dir.join("praxec.repo.yaml"),
        "schema: praxec.repo/v1\nname: imported-core\nnamespace: imported\nversion: 0.1.0\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("capabilities/cap.greet.yaml"),
        "workflows:\n  cap.greet:\n    title: Greet\n    initial: ready\n    states:\n      ready:\n        terminal: true\n",
    )
    .unwrap();
    let git = |args: &[&str]| {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .unwrap()
                .status
                .success()
        );
    };
    Command::new("git")
        .arg("init")
        .arg("-b")
        .arg("main")
        .arg(dir)
        .output()
        .unwrap();
    git(&["add", "."]);
    git(&[
        "-c",
        "user.email=t@t",
        "-c",
        "user.name=t",
        "commit",
        "-m",
        "seed",
    ]);
}

#[test]
fn remote_repo_uri_is_cloned_and_layered() {
    let td = TempDir::new().unwrap();
    let origin = td.path().join("origin");
    seed_origin_repo(&origin);

    let host = format!(
        "version: \"1.0.0\"\nrepos:\n  - uri: \"file://{}\"\n    ref: main\n",
        origin.display()
    );
    let path = write_host(&td, &host);
    let (config, _diag) =
        load_resolved_with_repos(&path).expect("remote repo imports and resolves");

    let workflows = config
        .pointer("/workflows")
        .and_then(Value::as_object)
        .unwrap();
    assert!(
        workflows.contains_key("imported/cap.greet"),
        "imported repo's namespaced id should be present; got {:?}",
        workflows.keys().collect::<Vec<_>>()
    );
    // It was cloned into the host's repo cache.
    assert!(
        td.path().join(".praxec/repos").exists(),
        "clone cache created"
    );
}

#[test]
fn repo_entry_with_both_path_and_uri_is_rejected() {
    let td = TempDir::new().unwrap();
    let host = "version: \"1.0.0\"\nrepos:\n  - path: ./x\n    uri: \"git+https://h/r\"\n";
    let path = write_host(&td, host);
    let err = load_resolved_with_repos(&path).expect_err("both path and uri must error");
    assert!(format!("{:#}", err).contains("both `path` and `uri`"));
}

#[test]
fn non_boolean_writable_is_rejected() {
    let td = TempDir::new().unwrap();
    let host = format!(
        "version: \"1.0.0\"\nrepos:\n  - path: \"{}\"\n    writable: yes-please\n",
        fixtures_root().join("swe-core").display(),
    );
    let path = write_host(&td, &host);
    let err = load_resolved_with_repos(&path).expect_err("non-bool writable must error");
    let msg = format!("{:#}", err);
    assert!(msg.contains("INVALID_REPO_ENTRY"), "msg: {msg}");
    assert!(msg.contains("writable"), "msg should name the field: {msg}");
}

// ---------- SPEC §9.5 — connection GRANT GATE ----------
//
// A layered repo may DECLARE `connections:`, but only the OPERATOR activates
// them via `grant_connections:` on the `repos:` entry. Ungranted declarations
// are never merged live; they are stamped under
// `/praxec/_ungrantedConnections` with the exact YAML remedy.

/// Build a pack repo (namespace `packns`, name `conn-pack`) declaring two
/// connections plus a workflow, so tests can assert the grant gate is
/// connection-scoped (workflows still load either way).
fn seed_conn_repo(dir: &std::path::Path) {
    std::fs::create_dir_all(dir.join("connections")).unwrap();
    std::fs::create_dir_all(dir.join("capabilities")).unwrap();
    std::fs::write(
        dir.join("praxec.repo.yaml"),
        "schema: praxec.repo/v1\nname: conn-pack\nnamespace: packns\nversion: 0.1.0\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("connections/tools.yaml"),
        r#"
connections:
  gh-mcp:
    kind: mcp
    command: gh-mcp-server
  audit-api:
    kind: rest
    baseUrl: "https://audit.example"
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("capabilities/cap.audit.yaml"),
        "workflows:\n  cap.audit:\n    title: Audit\n    initial: ready\n    states:\n      ready:\n        terminal: true\n",
    )
    .unwrap();
}

#[test]
fn ungranted_pack_connection_is_not_live_and_is_stamped_with_remedy() {
    let td = TempDir::new().unwrap();
    let repo_dir = td.path().join("conn-pack");
    seed_conn_repo(&repo_dir);
    let host = format!(
        "version: \"1.0.0\"\nrepos:\n  - path: \"{}\"\n",
        repo_dir.display(),
    );
    let path = write_host(&td, &host);
    let (config, _diagnostics) = load_resolved_with_repos(&path).expect("pack loads");

    // NOT merged live: neither connection reaches the spawnable registry —
    // which is also exactly what the authoring provenance gate seeds from.
    assert!(
        config.pointer("/connections/packns~1gh-mcp").is_none(),
        "ungranted mcp connection must not be live"
    );
    assert!(
        config.pointer("/connections/packns~1audit-api").is_none(),
        "ungranted rest connection must not be live"
    );
    // NOT silently dropped: stamped as self-documenting diagnostic state.
    let stamp = config
        .pointer("/praxec/_ungrantedConnections/packns~1gh-mcp")
        .expect("ungranted connection stamped");
    assert_eq!(stamp["repo"].as_str(), Some("conn-pack"));
    assert_eq!(stamp["namespace"].as_str(), Some("packns"));
    let remedy = stamp["remedy"].as_str().expect("remedy present");
    assert!(
        remedy.contains("grant_connections: [gh-mcp]"),
        "remedy carries the exact YAML fix: {remedy}"
    );
    assert!(
        remedy.contains("conn-pack"),
        "remedy names the repo: {remedy}"
    );
    // The pack's non-connection content still loads — the gate is
    // connection-scoped, not a pack quarantine.
    assert!(
        config.pointer("/workflows/packns~1cap.audit").is_some(),
        "pack workflows load regardless of connection grants"
    );
}

#[test]
fn granted_pack_connection_is_live_and_not_stamped() {
    let td = TempDir::new().unwrap();
    let repo_dir = td.path().join("conn-pack");
    seed_conn_repo(&repo_dir);
    let host = format!(
        "version: \"1.0.0\"\nrepos:\n  - path: \"{}\"\n    grant_connections: [gh-mcp, audit-api]\n",
        repo_dir.display(),
    );
    let path = write_host(&td, &host);
    let (config, _diagnostics) = load_resolved_with_repos(&path).expect("granted pack loads");
    assert!(
        config.pointer("/connections/packns~1gh-mcp").is_some(),
        "granted connection is live under its namespaced key"
    );
    assert!(
        config.pointer("/connections/packns~1audit-api").is_some(),
        "granted connection is live under its namespaced key"
    );
    assert!(
        config.pointer("/praxec/_ungrantedConnections").is_none(),
        "nothing left to stamp when every declaration is granted"
    );
}

#[test]
fn partial_grant_gates_each_connection_independently() {
    let td = TempDir::new().unwrap();
    let repo_dir = td.path().join("conn-pack");
    seed_conn_repo(&repo_dir);
    let host = format!(
        "version: \"1.0.0\"\nrepos:\n  - path: \"{}\"\n    grant_connections: [gh-mcp]\n",
        repo_dir.display(),
    );
    let path = write_host(&td, &host);
    let (config, _diagnostics) = load_resolved_with_repos(&path).expect("partial grant loads");
    assert!(
        config.pointer("/connections/packns~1gh-mcp").is_some(),
        "granted connection is live"
    );
    assert!(
        config.pointer("/connections/packns~1audit-api").is_none(),
        "ungranted sibling stays diverted"
    );
    assert!(
        config
            .pointer("/praxec/_ungrantedConnections/packns~1audit-api")
            .is_some(),
        "ungranted sibling is stamped"
    );
    assert!(
        config
            .pointer("/praxec/_ungrantedConnections/packns~1gh-mcp")
            .is_none(),
        "granted connection is not stamped"
    );
}

#[test]
fn grant_accepts_fully_qualified_connection_name() {
    let td = TempDir::new().unwrap();
    let repo_dir = td.path().join("conn-pack");
    seed_conn_repo(&repo_dir);
    let host = format!(
        "version: \"1.0.0\"\nrepos:\n  - path: \"{}\"\n    grant_connections: [packns/gh-mcp, audit-api]\n",
        repo_dir.display(),
    );
    let path = write_host(&td, &host);
    let (config, _diagnostics) = load_resolved_with_repos(&path).expect("qualified grant loads");
    assert!(
        config.pointer("/connections/packns~1gh-mcp").is_some(),
        "fully-qualified grant activates the connection"
    );
}

#[test]
fn host_declared_connections_are_live_without_any_grant() {
    // Back-compat: the operator writing a connection in the host config IS
    // the grant. Only repo-CONTRIBUTED connections pass through the gate.
    let td = TempDir::new().unwrap();
    let repo_dir = td.path().join("conn-pack");
    seed_conn_repo(&repo_dir);
    let host = format!(
        r#"
version: "1.0.0"
repos:
  - path: "{repo}"
connections:
  my-tool:
    kind: cli
    command: my-tool
"#,
        repo = repo_dir.display(),
    );
    let path = write_host(&td, &host);
    let (config, _diagnostics) = load_resolved_with_repos(&path).expect("host connections load");
    assert!(
        config.pointer("/connections/my-tool").is_some(),
        "host-declared connection is live with no grant machinery"
    );
    assert!(
        config.pointer("/connections/packns~1gh-mcp").is_none(),
        "pack connection still requires its own grant"
    );
}

#[test]
fn stale_connection_grant_is_rejected() {
    let td = TempDir::new().unwrap();
    let repo_dir = td.path().join("conn-pack");
    seed_conn_repo(&repo_dir);
    let host = format!(
        "version: \"1.0.0\"\nrepos:\n  - path: \"{}\"\n    grant_connections: [gh-mpc]\n",
        repo_dir.display(),
    );
    let path = write_host(&td, &host);
    let err = load_resolved_with_repos(&path).expect_err("stale grant must error");
    let msg = format!("{:#}", err);
    assert!(msg.contains("STALE_CONNECTION_GRANT"), "msg: {msg}");
    assert!(msg.contains("gh-mpc"), "msg names the stale grant: {msg}");
    assert!(msg.contains("conn-pack"), "msg names the repo: {msg}");
}

#[test]
fn non_array_grant_connections_is_rejected() {
    let td = TempDir::new().unwrap();
    let repo_dir = td.path().join("conn-pack");
    seed_conn_repo(&repo_dir);
    let host = format!(
        "version: \"1.0.0\"\nrepos:\n  - path: \"{}\"\n    grant_connections: gh-mcp\n",
        repo_dir.display(),
    );
    let path = write_host(&td, &host);
    let err = load_resolved_with_repos(&path).expect_err("non-array grant must error");
    let msg = format!("{:#}", err);
    assert!(msg.contains("INVALID_REPO_ENTRY"), "msg: {msg}");
    assert!(msg.contains("grant_connections"), "msg: {msg}");
}

#[test]
fn granting_a_malformed_staged_connection_fails_fast() {
    // F9 — promotion is a trust boundary: a granted staged body that does not
    // match `$defs/connection` must FAIL the load, never become a live
    // connection. (`px connections add` writes well-formed bodies, but the
    // staged block is operator-editable YAML.)
    let td = TempDir::new().unwrap();
    let host = r#"
version: "1.0.0"
stagedConnections:
  broken:
    kind: carrier-pigeon
grant_connections: [broken]
"#;
    let path = write_host(&td, host);
    let err = load_resolved_with_repos(&path).expect_err("malformed promotion must error");
    let msg = format!("{:#}", err);
    assert!(msg.contains("INVALID_STAGED_CONNECTION"), "msg: {msg}");
    assert!(msg.contains("broken"), "msg names the connection: {msg}");
}

#[test]
fn malformed_staged_connection_without_a_grant_stays_inert() {
    // F9 scope — the validation gate sits on PROMOTION (the trust boundary).
    // An ungranted staged body never goes live, so a malformed one is inert
    // diagnostic state, not a load failure.
    let td = TempDir::new().unwrap();
    let host = r#"
version: "1.0.0"
stagedConnections:
  broken:
    kind: carrier-pigeon
"#;
    let path = write_host(&td, host);
    let (config, _diags) =
        load_resolved_with_repos(&path).expect("ungranted staged body must not fail the load");
    assert!(
        config.pointer("/connections/broken").is_none(),
        "malformed staged body must never be live"
    );
    assert!(
        config
            .pointer("/praxec/_ungrantedConnections/broken")
            .is_some(),
        "staged body is stamped ungranted"
    );
}

#[test]
fn granting_a_well_formed_staged_connection_promotes_it_live() {
    // F9 companion — the validation gate admits a body that DOES match
    // `$defs/connection` (guards against the validator rejecting everything).
    let td = TempDir::new().unwrap();
    let host = r#"
version: "1.0.0"
stagedConnections:
  gh:
    kind: cli
    command: gh
grant_connections: [gh]
"#;
    let path = write_host(&td, host);
    let (config, _diags) = load_resolved_with_repos(&path).expect("valid promotion succeeds");
    assert_eq!(
        config
            .pointer("/connections/gh/kind")
            .and_then(Value::as_str),
        Some("cli")
    );
}

#[test]
fn v23_rejects_stale_override_with_no_collision() {
    // An `overrides:` entry that doesn't actually shadow a repo-provided
    // id is almost certainly an author mistake (renamed id, deleted repo).
    let td = TempDir::new().unwrap();
    let host = format!(
        r#"
version: "1.0.0"
repos:
  - path: "{swe}"
overrides:
  - swe/cap.does-not-exist
"#,
        swe = fixtures_root().join("swe-core").display(),
    );
    let path = write_host(&td, &host);
    let err = load_resolved_with_repos(&path).expect_err("stale override must error");
    let msg = format!("{:#}", err);
    assert!(msg.contains("STALE_OVERRIDE"), "msg: {msg}");
    assert!(msg.contains("swe/cap.does-not-exist"), "msg: {msg}");
}
