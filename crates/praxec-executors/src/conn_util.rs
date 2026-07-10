//! Small shared helpers for parsing connection definitions out of the
//! gateway config. The connection *structs* themselves (`CliConnection`,
//! `McpConnection`, `RestConnection`) stay distinct data contracts — this
//! module only deduplicates the behaviour they happen to share.

use std::collections::HashMap;

use praxec_core::error::ExecutorError;
use serde_json::Value;

/// Convert a JSON object of string-valued fields into a
/// `HashMap<String, String>`, dropping any non-string values.
///
/// Used to extract `env` / `headers` blocks from a connection definition.
/// A `None` input (absent field) or a non-object value yields an empty map,
/// matching the previous per-executor `.unwrap_or_default()` behaviour.
pub(crate) fn json_object_to_string_map(value: Option<&Value>) -> HashMap<String, String> {
    value
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// SPEC §9.5 — one pack-declared connection the operator has not granted, as
/// stamped by the config merge under `/praxec/_ungrantedConnections`.
#[derive(Debug, Clone)]
pub(crate) struct UngrantedConnection {
    /// Manifest `name` of the pack that declared the connection.
    pub repo: String,
    /// Exact YAML remedy authored by the stamping site.
    pub remedy: String,
}

/// Parse the `/praxec/_ungrantedConnections` stamp (written by
/// `merge_declared_repos` in praxec-core) into a lookup map keyed by the
/// fully-qualified connection key. Absent stamp → empty map (no repos, or
/// every pack connection granted).
pub(crate) fn ungranted_from_config(config: &Value) -> HashMap<String, UngrantedConnection> {
    config
        .pointer("/praxec/_ungrantedConnections")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(key, v)| {
                    let repo = v.get("repo").and_then(Value::as_str)?.to_string();
                    let remedy = v.get("remedy").and_then(Value::as_str)?.to_string();
                    Some((key.clone(), UngrantedConnection { repo, remedy }))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// SPEC §9.5 — classify a failed connection lookup. An UNGRANTED pack
/// connection fails typed (`UNGRANTED_PACK_CONNECTION`) with the pack name and
/// the exact grant remedy — never a bare not-found, never a silent fallback.
/// A genuinely unknown name keeps the existing `<kind> connection '<name>' not
/// found` message byte-for-byte.
pub(crate) fn connection_not_found_error(
    kind: &str,
    name: &str,
    ungranted: &HashMap<String, UngrantedConnection>,
) -> ExecutorError {
    match ungranted.get(name) {
        Some(u) => ExecutorError::Permanent(format!(
            "UNGRANTED_PACK_CONNECTION: {kind} connection '{name}' is declared by pack \
             '{repo}' but has not been granted by the operator; {remedy}. A pack connection \
             is never spawnable without an explicit operator grant (SPEC §9.5).",
            repo = u.repo,
            remedy = u.remedy
        )),
        None => ExecutorError::Permanent(format!("{kind} connection '{name}' not found")),
    }
}
