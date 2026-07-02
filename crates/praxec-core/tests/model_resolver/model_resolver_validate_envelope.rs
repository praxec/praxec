//! PR2 — `validate_models_config_envelope` tests.
//!
//! Backs round-trip validation of an authored `models.yaml`: the meta
//! library's `cap.implement.write-agents-config` capability validates its
//! output through this envelope after writing (FMECA U3).
//!
//! Contract pinned here:
//! - On valid file → `{ok: true, summary: <string>}`.
//! - On invalid file → `{ok: false, error_kind: <stable code>, detail: <string>}`.
//!   `error_kind` is the stable shape scripts switch on; `detail` is
//!   for humans.

use std::io::Write;

use praxec_core::model_resolver::validate_models_config_envelope;
use tempfile::NamedTempFile;

fn yaml_to_tempfile(body: &str) -> NamedTempFile {
    let mut f = NamedTempFile::new().expect("temp file");
    f.write_all(body.as_bytes()).expect("write");
    f.flush().expect("flush");
    f
}

#[test]
fn envelope_ok_true_on_valid_file() {
    let f = yaml_to_tempfile(
        r#"
version: 1
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
"#,
    );
    let env = validate_models_config_envelope(f.path());
    assert_eq!(env["ok"], serde_json::json!(true), "envelope: {env:#}");
    assert!(
        env["summary"]
            .as_str()
            .unwrap()
            .contains("1 default binding"),
        "envelope: {env:#}",
    );
}

#[test]
fn envelope_missing_default_surfaces_named_kind() {
    let f = yaml_to_tempfile("version: 1\n");
    let env = validate_models_config_envelope(f.path());
    assert_eq!(env["ok"], serde_json::json!(false), "envelope: {env:#}");
    assert_eq!(
        env["error_kind"],
        serde_json::json!("MISSING_DEFAULT"),
        "envelope: {env:#}",
    );
}

#[test]
fn envelope_unknown_override_key_surfaces_named_kind() {
    let f = yaml_to_tempfile(
        r#"
version: 1
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
overrides:
  vision-frontier:
    - provider: { name: anthropic }
      model: claude-opus-4-7
"#,
    );
    let env = validate_models_config_envelope(f.path());
    assert_eq!(env["ok"], serde_json::json!(false), "envelope: {env:#}");
    assert_eq!(
        env["error_kind"],
        serde_json::json!("UNKNOWN_OVERRIDE_KEY"),
        "envelope: {env:#}",
    );
    assert!(
        env["detail"].as_str().unwrap().contains("vision-frontier"),
        "envelope: {env:#}",
    );
}

#[test]
fn envelope_unknown_feature_key_surfaces_named_kind() {
    // Typo in an Anthropic feature key — should surface
    // UNKNOWN_FEATURE_KEY via `deny_unknown_fields` on the variant
    // struct (FMECA T3).
    let f = yaml_to_tempfile(
        r#"
version: 1
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
    features:
      reasoning_effrt: high
"#,
    );
    let env = validate_models_config_envelope(f.path());
    assert_eq!(env["ok"], serde_json::json!(false), "envelope: {env:#}");
    assert_eq!(
        env["error_kind"],
        serde_json::json!("UNKNOWN_FEATURE_KEY"),
        "envelope: {env:#}",
    );
}

#[test]
fn envelope_io_error_surfaces_named_kind_on_missing_path() {
    let env = validate_models_config_envelope(std::path::Path::new(
        "/definitely/does/not/exist/models.yaml",
    ));
    assert_eq!(env["ok"], serde_json::json!(false), "envelope: {env:#}");
    assert_eq!(
        env["error_kind"],
        serde_json::json!("IO"),
        "envelope: {env:#}",
    );
}
