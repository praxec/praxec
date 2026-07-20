//! SPEC §22 — script snapshot stamping. Tests that:
//!   - Inline-body scripts stamp verbatim with computed hash.
//!   - URI-body scripts resolve at load + materialize body + verify hash.
//!   - Hash mismatch surfaces SCRIPT_HASH_MISMATCH at load.
//!   - Only workflows that REFERENCE a script via `script` executor get the
//!     scripts library stamped (no bloat for irrelevant workflows).
//!   - SPEC §8.2 invariant: snapshot is self-contained; editing the
//!     top-level scripts: block after stamping is invisible to instances.

use praxec_core::config::{compute_script_hash, load_resolved, resolve_str};
use serde_json::Value;
use std::io::Write;
use tempfile::NamedTempFile;

// ── Inline body — stamps verbatim with computed hash ──────────────────────

#[test]
fn inline_body_script_stamps_into_referencing_workflow() {
    let yaml = r#"
version: "1.0.0"
scripts:
  build.cargo.release:
    verb: build
    lifecycle: stable
    body: |
      cargo build --release --locked
workflows:
  demo:
    initialState: build
    states:
      build:
        transitions:
          run:
            target: done
            executor:
              kind: script
              subject: build.cargo.release
      done: { terminal: true }
"#;
    let resolved = resolve_str(yaml).expect("resolves");
    let lib = resolved
        .pointer("/workflows/demo/_scriptsLibrary")
        .expect("workflow should carry stamped _scriptsLibrary");
    let entry = &lib["build.cargo.release"];
    assert_eq!(entry["verb"], "build");
    assert_eq!(entry["lifecycle"], "stable");
    assert_eq!(entry["source"], "config");
    assert_eq!(
        entry["body"].as_str().unwrap().trim(),
        "cargo build --release --locked"
    );
    let h = entry["hash"].as_str().unwrap();
    assert!(h.starts_with("sha256:"), "hash must carry prefix; got: {h}");
}

// ── Workflows that don't reference the script don't get the library ─────

#[test]
fn workflow_without_script_reference_gets_no_scripts_library() {
    let yaml = r#"
version: "1.0.0"
scripts:
  build.cargo.release:
    verb: build
    lifecycle: stable
    body: |
      cargo build --release
workflows:
  uses_script:
    initialState: s
    states:
      s:
        transitions:
          go:
            target: done
            executor: { kind: script, subject: build.cargo.release }
      done: { terminal: true }
  ignores_script:
    initialState: s
    states:
      s:
        transitions:
          go:
            target: done
            executor: { kind: noop }
      done: { terminal: true }
"#;
    let resolved = resolve_str(yaml).expect("resolves");
    assert!(
        resolved
            .pointer("/workflows/uses_script/_scriptsLibrary")
            .is_some(),
        "referencing workflow MUST get _scriptsLibrary"
    );
    assert!(
        resolved
            .pointer("/workflows/ignores_script/_scriptsLibrary")
            .is_none(),
        "non-referencing workflow MUST NOT get _scriptsLibrary"
    );
}

// ── URI body resolved at load (file://) ──────────────────────────────────

#[test]
fn file_uri_script_resolves_and_verifies_hash_at_load() {
    let script_body = "#!/usr/bin/env bash\necho hello from file\n";
    let mut tmp = NamedTempFile::new().expect("temp file");
    tmp.write_all(script_body.as_bytes()).unwrap();
    let absolute = tmp.path().to_path_buf();
    let hash = compute_script_hash(script_body);

    // Build a config file that references the temp script via file://.
    // We need a real config file (not resolve_str) because uri resolution
    // happens at load_yaml_inner time.
    let cfg_yaml = format!(
        r#"version: "1.0.0"
scripts:
  build.shell.echo:
    verb: build
    lifecycle: stable
    uri: file://{path}
    hash: {hash}
workflows:
  demo:
    initialState: s
    states:
      s:
        transitions:
          go:
            target: done
            executor: {{ kind: script, subject: build.shell.echo }}
      done: {{ terminal: true }}
"#,
        path = absolute.display(),
    );
    let cfg_file = NamedTempFile::new().expect("cfg file");
    std::fs::write(cfg_file.path(), cfg_yaml).unwrap();

    let resolved = load_resolved(cfg_file.path()).expect("config loads");
    let entry = resolved
        .pointer("/workflows/demo/_scriptsLibrary/build.shell.echo")
        .expect("uri-sourced script stamped into library");
    assert_eq!(entry["body"].as_str().unwrap(), script_body);
    assert_eq!(entry["hash"].as_str().unwrap(), hash);
}

#[test]
fn file_uri_with_drifted_body_rejects_at_load_with_script_hash_mismatch() {
    let actual_body = "echo actual\n";
    let mut tmp = NamedTempFile::new().expect("temp file");
    tmp.write_all(actual_body.as_bytes()).unwrap();
    let absolute = tmp.path().to_path_buf();

    let cfg_yaml = format!(
        r#"version: "1.0.0"
scripts:
  build.shell.echo:
    verb: build
    lifecycle: stable
    uri: file://{path}
    hash: sha256:0000000000000000000000000000000000000000000000000000000000000000
"#,
        path = absolute.display(),
    );
    let cfg_file = NamedTempFile::new().expect("cfg file");
    std::fs::write(cfg_file.path(), cfg_yaml).unwrap();

    let err = load_resolved(cfg_file.path()).expect_err("hash mismatch must reject");
    let s = format!("{err:?}");
    assert!(
        s.contains("SCRIPT_HASH_MISMATCH"),
        "expected SCRIPT_HASH_MISMATCH; got: {s}"
    );
    assert!(
        s.contains("build.shell.echo"),
        "error must name the script subject; got: {s}"
    );
}

#[test]
fn file_uri_with_missing_file_surfaces_clear_error() {
    let cfg_yaml = r#"version: "1.0.0"
scripts:
  build.shell.nope:
    verb: build
    lifecycle: stable
    uri: file:///tmp/does/not/exist/script.sh
    hash: sha256:0000000000000000000000000000000000000000000000000000000000000000
"#;
    let cfg_file = NamedTempFile::new().expect("cfg file");
    std::fs::write(cfg_file.path(), cfg_yaml).unwrap();

    let err = load_resolved(cfg_file.path()).expect_err("missing file must reject");
    let s = format!("{err:?}");
    assert!(
        s.contains("build.shell.nope"),
        "error must name script subject; got: {s}"
    );
    assert!(
        s.to_lowercase().contains("script.sh")
            || s.to_lowercase().contains("no such file")
            || s.to_lowercase().contains("not found"),
        "error must mention the file path or 'not found'; got: {s}"
    );
}

// ── Relative file:// URIs resolve relative to config file's directory ────

#[test]
fn relative_file_uri_resolves_relative_to_config_file_directory() {
    let dir = tempfile::tempdir().expect("temp dir");
    let script_path = dir.path().join("scripts").join("hello.sh");
    std::fs::create_dir_all(script_path.parent().unwrap()).unwrap();
    let body = "echo relative\n";
    std::fs::write(&script_path, body).unwrap();
    let hash = compute_script_hash(body);

    // Config file in dir/; script in dir/scripts/hello.sh; reference via
    // `file://scripts/hello.sh` — relative to the config file's directory.
    let cfg_path = dir.path().join("gateway.yaml");
    let cfg_yaml = format!(
        r#"version: "1.0.0"
scripts:
  build.local.hello:
    verb: build
    lifecycle: stable
    uri: file://scripts/hello.sh
    hash: {hash}
"#
    );
    std::fs::write(&cfg_path, cfg_yaml).unwrap();

    let resolved = load_resolved(&cfg_path).expect("relative file:// must resolve via config dir");
    // No workflows reference the script, so no stamping; but the source
    // top-level entry should have its uri rewritten to an absolute path.
    let top_uri = resolved
        .pointer("/scripts/build.local.hello/uri")
        .and_then(Value::as_str)
        .expect("uri preserved on top-level entry");
    assert!(
        top_uri.starts_with("file:///"),
        "relative file:// must be rewritten to absolute; got: {top_uri}"
    );
}

// ── Unknown subject fails at LOAD, not at run ─────────────────────────────

/// A `kind: script` executor naming a subject that no `scripts:` entry
/// defines is an authoring typo. `stamp_scripts_library` used to skip it
/// silently (`if let Some(entry) = full_library.get(subject)`), so
/// `praxec check` reported `validation: ok` and the run failed much later
/// with `SCRIPT_NOT_IN_SNAPSHOT` — whose own message blames collection,
/// pointing straight back here.
///
/// This is the same poka-yoke class as V32: a reference that resolves to
/// nothing must fail at load, before the human gate and before any lease.
#[test]
fn unknown_script_subject_is_rejected_at_load() {
    let yaml = r#"
version: "1.0.0"
scripts:
  build.cargo.release:
    verb: build
    lifecycle: stable
    body: |
      cargo build --release --locked
workflows:
  demo:
    initialState: build
    states:
      build:
        transitions:
          run:
            target: done
            executor:
              kind: script
              subject: build.cargo.releas
      done: { terminal: true }
"#;
    let err = resolve_str(yaml).expect_err("a typo'd script subject must not load");
    let msg = err.to_string();
    assert!(
        msg.contains("SCRIPT_SUBJECT_UNKNOWN"),
        "must carry the coded error; got: {msg}"
    );
    assert!(
        msg.contains("build.cargo.releas"),
        "must name the unresolvable subject; got: {msg}"
    );
    assert!(
        msg.contains("demo"),
        "must name the workflow it was referenced from; got: {msg}"
    );
    assert!(
        msg.contains("build.cargo.release"),
        "must list the declared subjects so the typo is obvious; got: {msg}"
    );
}

/// Fence: the happy path is untouched — a workflow whose subjects all
/// resolve still loads and still gets its library stamped.
#[test]
fn every_declared_script_subject_still_resolves() {
    let yaml = r#"
version: "1.0.0"
scripts:
  build.one:
    verb: build
    lifecycle: stable
    body: "echo one"
  verify.two:
    verb: verify
    lifecycle: stable
    body: "echo two"
workflows:
  demo:
    initialState: s
    states:
      s:
        transitions:
          x:
            target: t
            executor: { kind: script, subject: build.one }
      t:
        transitions:
          y:
            target: done
            executor: { kind: script, subject: verify.two }
      done: { terminal: true }
"#;
    let resolved = resolve_str(yaml).expect("all subjects resolve → loads");
    let lib = resolved
        .pointer("/workflows/demo/_scriptsLibrary")
        .expect("library stamped");
    assert!(lib.get("build.one").is_some());
    assert!(lib.get("verify.two").is_some());
}
