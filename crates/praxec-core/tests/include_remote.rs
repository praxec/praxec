use praxec_core::config::{load_yaml, raw_content_sha256};
use std::io::Write;

#[test]
fn raw_content_sha256_has_sha256_prefix_and_64_hex() {
    let h = raw_content_sha256("a: 1\n");
    assert_eq!(h.len(), "sha256:".len() + 64);
    assert!(
        h.strip_prefix("sha256:")
            .unwrap()
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );
}

#[test]
fn object_include_file_uri_with_correct_hash_merges() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("base.yaml");
    let base_body = "shared:\n  from_base: true\n";
    std::fs::File::create(&base)
        .unwrap()
        .write_all(base_body.as_bytes())
        .unwrap();

    let hash = raw_content_sha256(base_body);
    let main = dir.path().join("main.yaml");
    let main_body = format!(
        "include:\n  - uri: \"file://{}\"\n    hash: \"{}\"\nlocal: ok\n",
        base.display(),
        hash
    );
    std::fs::File::create(&main)
        .unwrap()
        .write_all(main_body.as_bytes())
        .unwrap();

    let merged = load_yaml(&main).unwrap();
    assert_eq!(
        merged.pointer("/shared/from_base"),
        Some(&serde_json::json!(true))
    );
}

#[test]
fn object_include_wrong_hash_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("base.yaml");
    std::fs::write(&base, "shared:\n  from_base: true\n").unwrap();

    let bad = format!("sha256:{}", "0".repeat(64));
    let main = dir.path().join("main.yaml");
    std::fs::write(
        &main,
        format!(
            "include:\n  - uri: \"file://{}\"\n    hash: \"{}\"\n",
            base.display(),
            bad
        ),
    )
    .unwrap();

    let err = load_yaml(&main).unwrap_err().to_string();
    assert!(err.contains("INCLUDE_HASH_MISMATCH"), "got: {err}");
}

#[test]
fn https_include_without_hash_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.yaml");
    std::fs::write(
        &main,
        "include:\n  - uri: \"https://example.com/pack.yaml\"\n",
    )
    .unwrap();

    let err = load_yaml(&main).unwrap_err().to_string();
    assert!(err.contains("has no `hash`"), "got: {err}");
}

#[test]
fn string_path_include_still_merges() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("base.yaml"), "shared:\n  from_base: true\n").unwrap();
    let main = dir.path().join("main.yaml");
    std::fs::write(&main, "include:\n  - base.yaml\nlocal: ok\n").unwrap();

    let merged = load_yaml(&main).unwrap();
    assert_eq!(
        merged.pointer("/shared/from_base"),
        Some(&serde_json::json!(true))
    );
}

#[test]
fn object_include_relative_file_uri_resolves_against_parent_dir() {
    let dir = tempfile::tempdir().unwrap();
    let base_body = "shared:\n  from_base: true\n";
    std::fs::write(dir.path().join("base.yaml"), base_body).unwrap();

    let hash = raw_content_sha256(base_body);
    let main = dir.path().join("main.yaml");
    // RELATIVE file:// uri (no leading slash, no dir) — must resolve next to main.yaml
    std::fs::write(
        &main,
        format!(
            "include:\n  - uri: \"file://base.yaml\"\n    hash: \"{}\"\n",
            hash
        ),
    )
    .unwrap();

    let merged = load_yaml(&main).unwrap();
    assert_eq!(
        merged.pointer("/shared/from_base"),
        Some(&serde_json::json!(true))
    );
}

// FIX 1: cycle/diamond detection key mismatch for file:// includes
#[test]
fn object_and_string_include_of_same_file_is_deduped_as_cycle() {
    let dir = tempfile::tempdir().unwrap();
    let base_body = "shared:\n  from_base: true\n";
    let base = dir.path().join("base.yaml");
    std::fs::write(&base, base_body).unwrap();
    let hash = raw_content_sha256(base_body);
    let main = dir.path().join("main.yaml");
    std::fs::write(
        &main,
        format!(
            "include:\n  - base.yaml\n  - uri: \"file://{}\"\n    hash: \"{}\"\n",
            base.display(),
            hash
        ),
    )
    .unwrap();
    let err = load_yaml(&main).unwrap_err().to_string();
    assert!(err.contains("cycle detected"), "got: {err}");
}

// FIX 2: explicit include scheme whitelist
#[test]
fn unsupported_scheme_include_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.yaml");
    std::fs::write(
        &main,
        format!(
            "include:\n  - uri: \"ftp://x/y.yaml\"\n    hash: \"sha256:{}\"\n",
            "0".repeat(64)
        ),
    )
    .unwrap();
    let err = load_yaml(&main).unwrap_err().to_string();
    assert!(err.contains("UNSUPPORTED_INCLUDE_URI_SCHEME"), "got: {err}");
}

// FIX 3: validate git+https URI shape for includes
#[test]
fn malformed_git_https_include_is_rejected_with_shape_error() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.yaml");
    std::fs::write(
        &main,
        format!(
            "include:\n  - uri: \"git+https://github.com/org/repo\"\n    hash: \"sha256:{}\"\n",
            "0".repeat(64)
        ),
    )
    .unwrap();
    let err = load_yaml(&main).unwrap_err().to_string();
    assert!(err.contains("INVALID_GIT_HTTPS_URI"), "got: {err}");
}

// FIX 4: strengthen the hash hex assertion
#[test]
fn raw_content_sha256_has_sha256_prefix_and_64_lowercase_hex() {
    let h = raw_content_sha256("a: 1\n");
    assert_eq!(h.len(), "sha256:".len() + 64);
    assert!(
        h.strip_prefix("sha256:")
            .unwrap()
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );
}
