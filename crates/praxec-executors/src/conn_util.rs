//! Small shared helpers for parsing connection definitions out of the
//! gateway config. The connection *structs* themselves (`CliConnection`,
//! `McpConnection`, `RestConnection`) stay distinct data contracts — this
//! module only deduplicates the behaviour they happen to share.

use std::collections::HashMap;

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
