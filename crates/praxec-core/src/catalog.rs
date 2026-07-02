//! Shared loader for catalog **data** files (models, prices, benchmarks, …).
//!
//! Catalog data changes as fast as the market and must not require a code
//! release. Each catalog ships a dated default (embedded via `include_str!`) and
//! is overridable at runtime. This is the one mechanism every catalog uses so the
//! override precedence is identical everywhere:
//!
//! `$<ENV_VAR>` → `~/.praxec/<file>` → `./.praxec/<file>` → shipped default.

use serde::de::DeserializeOwned;
use std::path::PathBuf;

/// Resolve an override file for catalog `file_name`, if one exists.
pub fn override_path(env_var: &str, file_name: &str) -> Option<PathBuf> {
    if let Ok(p) = std::env::var(env_var) {
        if !p.trim().is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    if let Some(d) = dirs::home_dir() {
        let p = d.join(".praxec").join(file_name);
        if p.exists() {
            return Some(p);
        }
    }
    let proj = PathBuf::from(".praxec").join(file_name);
    proj.exists().then_some(proj)
}

/// Parse the shipped default catalog. Panics with a clear message if the shipped
/// JSON is malformed — a build-time mistake caught by each catalog's parse test.
pub fn load_default<T: DeserializeOwned>(default_json: &str) -> T {
    serde_json::from_str(default_json).expect("shipped catalog default must be valid JSON")
}

/// Load a catalog: a user/project override if present and valid, else the shipped
/// default. A missing/unreadable/malformed override warns and falls back — it
/// never fails the load (the shipped default is always available).
pub fn load_catalog<T: DeserializeOwned>(env_var: &str, file_name: &str, default_json: &str) -> T {
    if let Some(path) = override_path(env_var, file_name) {
        match std::fs::read_to_string(&path) {
            Ok(raw) => match serde_json::from_str::<T>(&raw) {
                Ok(v) => return v,
                Err(e) => tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "invalid catalog override; using the shipped default"
                ),
            },
            Err(e) => tracing::warn!(
                path = %path.display(),
                error = %e,
                "cannot read catalog override; using the shipped default"
            ),
        }
    }
    load_default(default_json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const DEFAULT: &str = r#"{ "a": 1, "b": 2 }"#;

    #[test]
    fn falls_back_to_shipped_default_when_no_override() {
        // SAFETY: test-local env override.
        unsafe { std::env::set_var("PRAXEC_TEST_CATALOG_NOPE", "") };
        let v: BTreeMap<String, i32> = load_catalog(
            "PRAXEC_TEST_CATALOG_NOPE",
            "definitely-absent.json",
            DEFAULT,
        );
        assert_eq!(v.get("a"), Some(&1));
    }

    #[test]
    fn reads_a_valid_override_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cat.json");
        std::fs::write(&path, r#"{ "a": 9 }"#).unwrap();
        // SAFETY: test-local env override.
        unsafe { std::env::set_var("PRAXEC_TEST_CATALOG_OK", path.to_str().unwrap()) };
        let v: BTreeMap<String, i32> = load_catalog("PRAXEC_TEST_CATALOG_OK", "cat.json", DEFAULT);
        assert_eq!(v.get("a"), Some(&9));
        unsafe { std::env::remove_var("PRAXEC_TEST_CATALOG_OK") };
    }

    #[test]
    fn malformed_override_falls_back_to_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, "not json").unwrap();
        // SAFETY: test-local env override.
        unsafe { std::env::set_var("PRAXEC_TEST_CATALOG_BAD", path.to_str().unwrap()) };
        let v: BTreeMap<String, i32> = load_catalog("PRAXEC_TEST_CATALOG_BAD", "bad.json", DEFAULT);
        assert_eq!(v.get("a"), Some(&1)); // default, not the malformed override
        unsafe { std::env::remove_var("PRAXEC_TEST_CATALOG_BAD") };
    }
}
