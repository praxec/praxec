//! SPEC §22 — `scripts:` config validation. FMECA atomic assertions for
//! every rejection path in `validate_scripts`, plus the script hash
//! normalization contract (stricter than skill hash).
//!
//! Each test asserts one behavior — the error code surfaced + the offending
//! subject named — so failures point straight at the missing rule.

use praxec_core::config::{
    Diagnostic, compute_script_hash, normalize_for_script_hash, resolve_str,
    resolve_with_diagnostics,
};

fn resolve_yaml_with_diagnostics(
    yaml: &str,
) -> anyhow::Result<(serde_json::Value, Vec<Diagnostic>)> {
    let value: serde_json::Value = serde_yaml::from_str(yaml)?;
    resolve_with_diagnostics(value)
}

// ── helpers ────────────────────────────────────────────────────────────────

fn cfg(scripts_block: &str) -> String {
    format!(
        r#"version: "1.0.0"
scripts:
{scripts_block}
"#
    )
}

fn assert_load_error(yaml: &str, code: &str, must_contain: &str) {
    let err = resolve_str(yaml).expect_err("config must reject");
    let s = format!("{err:?}");
    assert!(s.contains(code), "missing code '{code}'; got: {s}");
    assert!(
        s.contains(must_contain),
        "missing snippet '{must_contain}'; got: {s}"
    );
}

// ── verb enum ──────────────────────────────────────────────────────────────

#[test]
fn closed_verb_enum_accepts_all_twelve() {
    // SPEC §22.3 v0.3 — 8 original action verbs plus 4 reconnaissance /
    // graded-findings additions (inspect/search/fetch/audit). Every token
    // in the closed enum must load with a subject under its own blessed
    // verb-mirror root.
    for verb in [
        "build", "test", "deploy", "format", "lint", "install", "verify", "run", "inspect",
        "search", "fetch", "audit",
    ] {
        let yaml = cfg(&format!(
            r#"  {verb}.script.fixture:
    verb: {verb}
    lifecycle: stable
    body: |
      echo hi
"#
        ));
        resolve_str(&yaml).expect("each verb in the closed enum loads cleanly");
    }
}

#[test]
fn unknown_verb_rejects_with_invalid_script_verb_naming_subject() {
    let yaml = cfg(r#"  build.cargo.release:
    verb: launch
    lifecycle: stable
    body: |
      echo hi
"#);
    assert_load_error(&yaml, "INVALID_SCRIPT_VERB", "build.cargo.release");
}

#[test]
fn missing_verb_rejects_with_missing_script_verb() {
    let yaml = cfg(r#"  build.cargo.release:
    lifecycle: stable
    body: |
      echo hi
"#);
    assert_load_error(&yaml, "MISSING_SCRIPT_VERB", "build.cargo.release");
}

// ── SPEC §22.4 v0.3 expansion: new blessed script roots ──────────────────

#[test]
fn blessed_script_root_inspect_accepted() {
    let yaml = cfg(r#"  inspect.deps.tree:
    verb: inspect
    lifecycle: stable
    body: |
      cargo tree
"#);
    resolve_str(&yaml).expect("inspect.* root must accept under strict_namespacing");
}

#[test]
fn blessed_script_root_search_accepted() {
    let yaml = cfg(r#"  search.codebase.ripgrep:
    verb: search
    lifecycle: stable
    body: |
      rg "$1"
"#);
    resolve_str(&yaml).expect("search.* root must accept under strict_namespacing");
}

#[test]
fn blessed_script_root_fetch_accepted() {
    let yaml = cfg(r#"  fetch.url.curl:
    verb: fetch
    lifecycle: stable
    body: |
      curl -sSL "$1"
"#);
    resolve_str(&yaml).expect("fetch.* root must accept under strict_namespacing");
}

#[test]
fn blessed_script_root_audit_accepted() {
    let yaml = cfg(r#"  audit.deps.cargo-audit:
    verb: audit
    lifecycle: stable
    body: |
      cargo audit
"#);
    resolve_str(&yaml).expect("audit.* root must accept under strict_namespacing");
}

// ── blessed roots ──────────────────────────────────────────────────────────

#[test]
fn unblessed_root_under_strict_namespacing_rejects() {
    // Strict is the default.
    let yaml = cfg(r#"  rocket.fuel.injection:
    verb: build
    lifecycle: stable
    body: |
      echo hi
"#);
    assert_load_error(&yaml, "INVALID_SCRIPT_SUBJECT_ROOT", "rocket");
}

#[test]
fn unblessed_root_under_lenient_namespacing_produces_warning_with_suggestion() {
    let yaml = r#"version: "1.0.0"
praxec:
  strict_namespacing: false
scripts:
  builder.cargo.release:
    verb: build
    lifecycle: stable
    body: |
      echo hi
"#;
    let (_resolved, diagnostics): (serde_json::Value, Vec<Diagnostic>) =
        resolve_yaml_with_diagnostics(yaml).expect("lenient mode must NOT bail; only warn");
    let warn = diagnostics
        .iter()
        .find(|d| d.code == "INVALID_SCRIPT_SUBJECT_ROOT")
        .expect("INVALID_SCRIPT_SUBJECT_ROOT diagnostic must be present");
    assert_eq!(warn.severity, praxec_core::config::DiagnosticSeverity::Warn);
    assert!(
        warn.suggestion
            .as_deref()
            .map(|s: &str| s.contains("build"))
            .unwrap_or(false),
        "expected closest-blessed-root suggestion 'build' for 'builder'; got: {:?}",
        warn.suggestion
    );
}

#[test]
fn empty_subject_rejects() {
    // YAML can't express an empty key cleanly, but a single-segment key
    // fails the is_subject_pattern check (which requires >=2 segments).
    let yaml = cfg(r#"  build:
    verb: build
    lifecycle: stable
    body: |
      echo hi
"#);
    let err = resolve_str(&yaml).expect_err("single-segment subject must reject");
    let s = format!("{err:?}");
    assert!(
        s.contains("at least two segments"),
        "expected dotted-pattern error; got: {s}"
    );
}

// ── lifecycle ──────────────────────────────────────────────────────────────

#[test]
fn missing_lifecycle_rejects_with_missing_script_lifecycle() {
    let yaml = cfg(r#"  build.cargo.release:
    verb: build
    body: |
      echo hi
"#);
    assert_load_error(&yaml, "MISSING_SCRIPT_LIFECYCLE", "build.cargo.release");
}

#[test]
fn invalid_lifecycle_rejects() {
    let yaml = cfg(r#"  build.cargo.release:
    verb: build
    lifecycle: graduated
    body: |
      echo hi
"#);
    assert_load_error(&yaml, "INVALID_SCRIPT_LIFECYCLE", "graduated");
}

// ── body source XOR ───────────────────────────────────────────────────────

#[test]
fn both_body_and_uri_rejects_as_ambiguous() {
    let yaml = cfg(r#"  build.cargo.release:
    verb: build
    lifecycle: stable
    body: |
      echo hi
    uri: file://./build.sh
    hash: sha256:0000000000000000000000000000000000000000000000000000000000000000
"#);
    assert_load_error(&yaml, "SCRIPT_BODY_SOURCE_AMBIGUOUS", "build.cargo.release");
}

#[test]
fn neither_body_nor_uri_rejects_as_ambiguous() {
    let yaml = cfg(r#"  build.cargo.release:
    verb: build
    lifecycle: stable
"#);
    assert_load_error(&yaml, "SCRIPT_BODY_SOURCE_AMBIGUOUS", "build.cargo.release");
}

#[test]
fn uri_without_hash_rejects_with_missing_script_hash() {
    let yaml = cfg(r#"  build.cargo.release:
    verb: build
    lifecycle: stable
    uri: file://./build.sh
"#);
    assert_load_error(&yaml, "MISSING_SCRIPT_HASH", "build.cargo.release");
}

#[test]
fn uri_with_unsupported_scheme_rejects() {
    // SPEC §22.2 — supported schemes: file://, https://, git+https://.
    // Other schemes (s3, ftp, gs, ssh, etc.) reject at validate time.
    let yaml = cfg(r#"  build.cargo.release:
    verb: build
    lifecycle: stable
    uri: s3://example-bucket/build.sh
    hash: sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
"#);
    assert_load_error(&yaml, "UNSUPPORTED_SCRIPT_URI_SCHEME", "s3");
}

// ── hash format ────────────────────────────────────────────────────────────

#[test]
fn malformed_hash_missing_prefix_rejects() {
    let yaml = cfg(r#"  build.cargo.release:
    verb: build
    lifecycle: stable
    body: |
      echo hi
    hash: e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
"#);
    assert_load_error(&yaml, "INVALID_SCRIPT_HASH_FORMAT", "sha256:");
}

#[test]
fn malformed_hash_wrong_length_rejects() {
    let yaml = cfg(r#"  build.cargo.release:
    verb: build
    lifecycle: stable
    body: |
      echo hi
    hash: sha256:abc123
"#);
    assert_load_error(&yaml, "INVALID_SCRIPT_HASH_FORMAT", "build.cargo.release");
}

#[test]
fn malformed_hash_uppercase_hex_rejects() {
    let yaml = cfg(r#"  build.cargo.release:
    verb: build
    lifecycle: stable
    body: |
      echo hi
    hash: sha256:E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855
"#);
    assert_load_error(&yaml, "INVALID_SCRIPT_HASH_FORMAT", "build.cargo.release");
}

#[test]
fn inline_body_with_matching_hash_loads_cleanly() {
    let body = "echo hi";
    let computed = compute_script_hash(body);
    let yaml = cfg(&format!(
        r#"  build.cargo.release:
    verb: build
    lifecycle: stable
    body: |
      {body}
    hash: {computed}
"#
    ));
    resolve_str(&yaml).expect("matching declared hash must load cleanly");
}

#[test]
fn inline_body_with_mismatched_hash_rejects() {
    // Author-declared hash that doesn't match what compute_script_hash(body)
    // would produce. The runtime must catch this as SCRIPT_HASH_MISMATCH.
    let yaml = cfg(r#"  build.cargo.release:
    verb: build
    lifecycle: stable
    body: |
      echo hi
    hash: sha256:0000000000000000000000000000000000000000000000000000000000000000
"#);
    assert_load_error(&yaml, "SCRIPT_HASH_MISMATCH", "build.cargo.release");
}

// ── normalize_for_script_hash contract ────────────────────────────────────

#[test]
fn normalize_preserves_internal_whitespace() {
    assert_eq!(normalize_for_script_hash("if [[  x ]]"), "if [[  x ]]\n");
    assert_eq!(
        normalize_for_script_hash("\tindented\twith\ttabs"),
        "\tindented\twith\ttabs\n"
    );
    assert_eq!(
        normalize_for_script_hash("multiple\n\ninternal\n\nnewlines"),
        "multiple\n\ninternal\n\nnewlines\n"
    );
}

#[test]
fn normalize_collapses_trailing_newlines_to_one() {
    assert_eq!(normalize_for_script_hash("echo hi\n\n\n"), "echo hi\n");
    assert_eq!(normalize_for_script_hash("echo hi\n"), "echo hi\n");
    assert_eq!(normalize_for_script_hash("echo hi"), "echo hi\n");
}

#[test]
fn script_hash_is_stable_across_trailing_newline_drift() {
    assert_eq!(
        compute_script_hash("echo hi\n"),
        compute_script_hash("echo hi\n\n\n"),
    );
    assert_eq!(
        compute_script_hash("echo hi"),
        compute_script_hash("echo hi\n"),
    );
}

#[test]
fn script_hash_distinguishes_internal_whitespace_changes() {
    assert_ne!(
        compute_script_hash("if [[ x ]]"),
        compute_script_hash("if [[  x  ]]"),
    );
}

// ── https:// + git+https:// URI schemes (SPEC §22.2) ──────────────────────

#[test]
fn https_uri_passes_validate_phase_when_hash_declared() {
    // Validation accepts the scheme. The actual fetch+hash check
    // happens at stamp time — resolve_str will fail with a network
    // error (invalid host), but NOT with UNSUPPORTED_SCRIPT_URI_SCHEME,
    // proving the validator accepted https://.
    let yaml = cfg(r#"  build.example.from-net:
    verb: build
    lifecycle: stable
    uri: https://example.invalid/path/to/script.sh
    hash: sha256:0000000000000000000000000000000000000000000000000000000000000000
"#);
    let err = resolve_str(&yaml).expect_err("network fetch must fail for invalid host");
    let s = format!("{err:?}");
    assert!(
        !s.contains("UNSUPPORTED_SCRIPT_URI_SCHEME"),
        "validator wrongly rejected https://; got: {s}"
    );
}

#[test]
fn https_uri_rejects_without_hash() {
    let yaml = cfg(r#"  build.example.from-net:
    verb: build
    lifecycle: stable
    uri: https://example.invalid/path/to/script.sh
"#);
    assert_load_error(&yaml, "MISSING_SCRIPT_HASH", "build.example.from-net");
}

#[test]
fn git_https_uri_passes_validate_phase_when_well_formed() {
    // Same pattern as the https test: validator must accept the
    // shape; stamp-time `git archive` against a nonexistent repo
    // will fail with GIT_ARCHIVE_NOT_SUPPORTED (or git-spawn error),
    // but NOT with INVALID_GIT_HTTPS_URI / UNSUPPORTED_SCRIPT_URI_SCHEME.
    let yaml = cfg(r#"  build.example.from-git:
    verb: build
    lifecycle: stable
    uri: git+https://github.invalid/example/repo.git@v1.2.3#scripts/build.sh
    hash: sha256:0000000000000000000000000000000000000000000000000000000000000000
"#);
    let err = resolve_str(&yaml).expect_err("git archive must fail for invalid host");
    let s = format!("{err:?}");
    assert!(
        !s.contains("UNSUPPORTED_SCRIPT_URI_SCHEME") && !s.contains("INVALID_GIT_HTTPS_URI"),
        "validator wrongly rejected git+https shape; got: {s}"
    );
}

#[test]
fn git_https_uri_rejects_without_path_fragment() {
    let yaml = cfg(r#"  build.example.no-frag:
    verb: build
    lifecycle: stable
    uri: git+https://github.com/example/repo.git@v1.2.3
    hash: sha256:0000000000000000000000000000000000000000000000000000000000000000
"#);
    assert_load_error(&yaml, "INVALID_GIT_HTTPS_URI", "missing the `#<path>`");
}

#[test]
fn git_https_uri_rejects_without_ref() {
    let yaml = cfg(r#"  build.example.no-ref:
    verb: build
    lifecycle: stable
    uri: git+https://github.com/example/repo.git#scripts/build.sh
    hash: sha256:0000000000000000000000000000000000000000000000000000000000000000
"#);
    assert_load_error(&yaml, "INVALID_GIT_HTTPS_URI", "missing the `@<ref>`");
}

#[test]
fn unsupported_uri_scheme_rejects() {
    let yaml = cfg(r#"  build.example.bad-scheme:
    verb: build
    lifecycle: stable
    uri: ftp://example.invalid/path/to/script.sh
    hash: sha256:0000000000000000000000000000000000000000000000000000000000000000
"#);
    assert_load_error(&yaml, "UNSUPPORTED_SCRIPT_URI_SCHEME", "ftp://");
}
