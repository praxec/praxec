//! D4a — the governed connections-write primitives: `add` (stage) + `grant`.
//!
//! The trust boundary is the SPEC §9.5 operator grant gate. A connection is only
//! trusted/spawnable once GRANTED; until then it is inert. These two primitives
//! keep that boundary intact while giving the operator an ergonomic CLI:
//!
//!   - [`add_connection`] writes ONLY the connection body, into a top-level
//!     `stagedConnections:` block. A staged connection is never in the live
//!     `/connections` registry, so it is treated identically to a repo-declared
//!     connection the operator has not granted: the config-load gate diverts it
//!     to `/praxec/_ungrantedConnections` and any spawn attempt fails typed with
//!     the grant remedy. `add` NEVER grants — so a non-human (an agent or pack
//!     shelling out to the CLI) cannot silently obtain a trusted connection.
//!
//!   - [`grant_connection`] is the separate, explicit, auditable operator act:
//!     it appends the connection's name to the top-level `grant_connections:`
//!     list (the host-level analog of a `repos:` entry's `grant_connections:`).
//!     On the next load the gate promotes the staged body into the live
//!     `/connections` registry.
//!
//! Both primitives round-trip the config through `serde_yaml::Value` (preserving
//! the operator's existing key order — only the touched block changes) and never
//! hand-roll YAML text. Every failure is fail-fast and typed; the config is
//! never silently overwritten.

use std::path::Path;

// F12 — the staged/grant key names are owned by praxec-core (the reader side:
// `apply_staged_connection_grants`). This writer imports them so the two sides
// can never drift apart.
use praxec_core::config::{GRANT_CONNECTIONS_KEY, STAGED_CONNECTIONS_KEY};
use serde_json::{Map, Value, json};
use thiserror::Error;

/// The three connection kinds a connection entry may take. Typed so every writer
/// matches exhaustively (poka-yoke) instead of switching on a loose string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionKind {
    Mcp,
    Cli,
    Rest,
}

impl ConnectionKind {
    /// The `kind:` discriminant string as it appears in the config.
    pub fn as_str(self) -> &'static str {
        match self {
            ConnectionKind::Mcp => "mcp",
            ConnectionKind::Cli => "cli",
            ConnectionKind::Rest => "rest",
        }
    }

    /// Parse the operator-facing `--kind` value. Fail-fast on anything else —
    /// there is no default kind.
    pub fn parse(s: &str) -> Result<Self, ConnWriteError> {
        match s {
            "mcp" => Ok(ConnectionKind::Mcp),
            "cli" => Ok(ConnectionKind::Cli),
            "rest" => Ok(ConnectionKind::Rest),
            other => Err(ConnWriteError::InvalidKind(other.to_string())),
        }
    }
}

/// The kind-specific payload for a new connection. Modeled as an enum so a REST
/// connection can never carry `args`, a CLI connection can never carry a
/// `baseUrl`, etc. — the field set is fixed by the kind at construction.
#[derive(Debug, Clone)]
pub enum ConnectionSpec {
    /// `kind: mcp` — a child MCP server, spawned over stdio (`command` + `args`)
    /// or reached over streamable HTTP (`url`). At least one of the two is
    /// required (a connection with neither can never be spawned).
    Mcp {
        command: Option<String>,
        args: Vec<String>,
        url: Option<String>,
        env: Vec<(String, String)>,
    },
    /// `kind: cli` — a governed shell command. `command` is required.
    Cli {
        command: String,
        working_directory: Option<String>,
        env: Vec<(String, String)>,
    },
    /// `kind: rest` — an HTTP endpoint. `base_url` is required; `headers` carry
    /// any auth (e.g. `Authorization`).
    Rest {
        base_url: String,
        headers: Vec<(String, String)>,
    },
}

impl ConnectionSpec {
    /// The kind this spec is for.
    pub fn kind(&self) -> ConnectionKind {
        match self {
            ConnectionSpec::Mcp { .. } => ConnectionKind::Mcp,
            ConnectionSpec::Cli { .. } => ConnectionKind::Cli,
            ConnectionSpec::Rest { .. } => ConnectionKind::Rest,
        }
    }
}

/// Every way the write primitives can refuse — each is a fail-fast, none is a
/// silent fallback.
#[derive(Debug, Error)]
pub enum ConnWriteError {
    #[error("INVALID_CONNECTION_KIND: '{0}' is not a connection kind (expected: mcp | cli | rest)")]
    InvalidKind(String),
    #[error("INVALID_CONNECTION_NAME: {0}")]
    InvalidName(String),
    #[error("INVALID_CONNECTION_FIELDS: {0}")]
    InvalidFields(String),
    #[error(
        "DUPLICATE_CONNECTION: a connection named '{0}' already exists (staged or live); refusing \
         to overwrite (remove the existing entry first to replace it)"
    )]
    DuplicateName(String),
    #[error(
        "CONNECTION_NOT_STAGED: no staged connection named '{0}' — add it first with \
         `px connections add {0} --kind <kind> ...` before granting"
    )]
    NotStaged(String),
    #[error("CONNECTION_ALREADY_GRANTED: connection '{0}' is already granted")]
    AlreadyGranted(String),
    #[error("CONNECTION_CONFIG_PARSE: {0}")]
    Parse(String),
    #[error("CONNECTION_CONFIG_IO: {0}")]
    Io(String),
}

/// Turn `(key, value)` pairs into a JSON object (later duplicates win). Used for
/// `env` / `headers` blocks.
fn pairs_to_value(pairs: &[(String, String)]) -> Value {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert(k.clone(), json!(v));
    }
    Value::Object(m)
}

/// Render a validated [`ConnectionSpec`] into the JSON object shape a connection
/// entry takes (matching `schemas/gateway-config.schema.json`). Kind-specific
/// field validation lives here — the single place a bad field set is rejected.
fn spec_to_entry(spec: &ConnectionSpec) -> Result<Value, ConnWriteError> {
    let mut m = Map::new();
    match spec {
        ConnectionSpec::Mcp {
            command,
            args,
            url,
            env,
        } => {
            let has_command = command.as_deref().is_some_and(|c| !c.trim().is_empty());
            let has_url = url.as_deref().is_some_and(|u| !u.trim().is_empty());
            if !has_command && !has_url {
                return Err(ConnWriteError::InvalidFields(
                    "an mcp connection requires a `command` (stdio server) or a `url` \
                     (streamable-http server)"
                        .into(),
                ));
            }
            m.insert("kind".into(), json!("mcp"));
            if let Some(c) = command.as_deref().filter(|c| !c.trim().is_empty()) {
                m.insert("command".into(), json!(c));
            }
            if !args.is_empty() {
                m.insert("args".into(), json!(args));
            }
            if let Some(u) = url.as_deref().filter(|u| !u.trim().is_empty()) {
                m.insert("url".into(), json!(u));
            }
            if !env.is_empty() {
                m.insert("env".into(), pairs_to_value(env));
            }
        }
        ConnectionSpec::Cli {
            command,
            working_directory,
            env,
        } => {
            if command.trim().is_empty() {
                return Err(ConnWriteError::InvalidFields(
                    "a cli connection requires a non-empty `command`".into(),
                ));
            }
            m.insert("kind".into(), json!("cli"));
            m.insert("command".into(), json!(command));
            if let Some(w) = working_directory
                .as_deref()
                .filter(|w| !w.trim().is_empty())
            {
                m.insert("workingDirectory".into(), json!(w));
            }
            if !env.is_empty() {
                m.insert("env".into(), pairs_to_value(env));
            }
        }
        ConnectionSpec::Rest { base_url, headers } => {
            if base_url.trim().is_empty() {
                return Err(ConnWriteError::InvalidFields(
                    "a rest connection requires a non-empty `baseUrl`".into(),
                ));
            }
            m.insert("kind".into(), json!("rest"));
            m.insert("baseUrl".into(), json!(base_url));
            if !headers.is_empty() {
                m.insert("headers".into(), pairs_to_value(headers));
            }
        }
    }
    Ok(Value::Object(m))
}

/// Validate a proposed connection name. Empty is rejected, and a `/` is rejected
/// because the `<namespace>/<name>` form is reserved for pack-contributed
/// connection keys resolved through the grant gate.
fn validate_name(name: &str) -> Result<(), ConnWriteError> {
    if name.trim().is_empty() {
        return Err(ConnWriteError::InvalidName(
            "a connection name must not be empty".into(),
        ));
    }
    if name.contains('/') {
        return Err(ConnWriteError::InvalidName(format!(
            "connection name '{name}' must not contain '/': the `<namespace>/<name>` form is \
             reserved for pack-contributed connections resolved through the grant gate (SPEC §9.5)"
        )));
    }
    Ok(())
}

/// Load a config file into a mutable `serde_yaml` mapping. Fail-fast on an
/// unreadable file, unparseable YAML, or a non-mapping top level.
fn load_doc(config_path: &Path) -> Result<serde_yaml::Value, ConnWriteError> {
    let raw = std::fs::read_to_string(config_path).map_err(|e| {
        ConnWriteError::Io(format!("reading config {}: {e}", config_path.display()))
    })?;
    let doc: serde_yaml::Value = serde_yaml::from_str(&raw).map_err(|e| {
        ConnWriteError::Parse(format!("parsing config {}: {e}", config_path.display()))
    })?;
    if !doc.is_mapping() {
        return Err(ConnWriteError::Parse(format!(
            "config {} is not a YAML mapping at the top level",
            config_path.display()
        )));
    }
    Ok(doc)
}

/// Serialize `doc` back to `config_path`. Fail-fast on a serialize or write
/// error.
fn write_doc(config_path: &Path, doc: &serde_yaml::Value) -> Result<(), ConnWriteError> {
    let serialized = serde_yaml::to_string(doc)
        .map_err(|e| ConnWriteError::Parse(format!("serializing updated config: {e}")))?;
    std::fs::write(config_path, serialized)
        .map_err(|e| ConnWriteError::Io(format!("writing config {}: {e}", config_path.display())))
}

/// Does the top-level mapping key `block` contain `name`?
fn block_has(root: &serde_yaml::Mapping, block: &str, name: &str) -> bool {
    root.get(serde_yaml::Value::String(block.to_string()))
        .and_then(serde_yaml::Value::as_mapping)
        .map(|m| m.contains_key(serde_yaml::Value::String(name.to_string())))
        .unwrap_or(false)
}

/// D4a — stage a new UNGRANTED connection named `name` under
/// `stagedConnections:`, returning the JSON entry that was written (for the
/// caller to echo back).
///
/// This writes ONLY the connection body — it does NOT grant. A staged connection
/// is inert: it is never in the live `/connections` registry, so the config-load
/// gate diverts it to `/praxec/_ungrantedConnections` and any spawn attempt fails
/// typed. Fail-fast — never a silent overwrite — on:
///   - an empty or `/`-bearing `name` ([`ConnWriteError::InvalidName`]),
///   - a kind-specific field violation ([`ConnWriteError::InvalidFields`]),
///   - a `name` already present under `stagedConnections:` OR `connections:`
///     ([`ConnWriteError::DuplicateName`]),
///   - an unreadable / unwritable file, or a non-mapping config
///     ([`ConnWriteError::Io`] / [`ConnWriteError::Parse`]).
pub fn add_connection(
    config_path: &Path,
    name: &str,
    spec: &ConnectionSpec,
) -> Result<Value, ConnWriteError> {
    validate_name(name)?;
    let entry = spec_to_entry(spec)?;

    let mut doc = load_doc(config_path)?;
    let root = doc
        .as_mapping_mut()
        .expect("load_doc guarantees a top-level mapping");

    // A name may not collide with a live OR an already-staged connection.
    if block_has(root, "connections", name) || block_has(root, STAGED_CONNECTIONS_KEY, name) {
        return Err(ConnWriteError::DuplicateName(name.to_string()));
    }

    let staged_key = serde_yaml::Value::String(STAGED_CONNECTIONS_KEY.to_string());
    if !root.contains_key(&staged_key) {
        root.insert(
            staged_key.clone(),
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        );
    }
    let staged = root
        .get_mut(&staged_key)
        .and_then(serde_yaml::Value::as_mapping_mut)
        .ok_or_else(|| {
            ConnWriteError::Parse(format!(
                "`{STAGED_CONNECTIONS_KEY}:` is present but is not a mapping"
            ))
        })?;
    let entry_yaml: serde_yaml::Value = serde_yaml::to_value(&entry)
        .map_err(|e| ConnWriteError::Parse(format!("building connection entry: {e}")))?;
    staged.insert(serde_yaml::Value::String(name.to_string()), entry_yaml);

    write_doc(config_path, &doc)?;
    Ok(entry)
}

/// D4a — GRANT a previously-staged connection: the separate, explicit operator
/// trust act. Appends `name` to the top-level `grant_connections:` list so the
/// config-load gate promotes the staged body into the live `/connections`
/// registry on the next load. Returns the granted connection body (for the
/// caller to echo + audit).
///
/// Fail-fast on: a `name` with no `stagedConnections:` entry
/// ([`ConnWriteError::NotStaged`]), a `name` already granted
/// ([`ConnWriteError::AlreadyGranted`]), or an unreadable / unwritable / non-
/// mapping config.
pub fn grant_connection(config_path: &Path, name: &str) -> Result<Value, ConnWriteError> {
    let mut doc = load_doc(config_path)?;
    let root = doc
        .as_mapping_mut()
        .expect("load_doc guarantees a top-level mapping");

    // The connection must be staged (nothing else is grantable through here).
    let body_yaml = root
        .get(serde_yaml::Value::String(
            STAGED_CONNECTIONS_KEY.to_string(),
        ))
        .and_then(serde_yaml::Value::as_mapping)
        .and_then(|m| m.get(serde_yaml::Value::String(name.to_string())))
        .cloned()
        .ok_or_else(|| ConnWriteError::NotStaged(name.to_string()))?;

    let grant_key = serde_yaml::Value::String(GRANT_CONNECTIONS_KEY.to_string());
    if !root.contains_key(&grant_key) {
        root.insert(grant_key.clone(), serde_yaml::Value::Sequence(Vec::new()));
    }
    let grants = root
        .get_mut(&grant_key)
        .and_then(|v| match v {
            serde_yaml::Value::Sequence(s) => Some(s),
            _ => None,
        })
        .ok_or_else(|| {
            ConnWriteError::Parse(format!(
                "top-level `{GRANT_CONNECTIONS_KEY}:` is present but is not an array"
            ))
        })?;
    if grants.iter().any(|v| v.as_str() == Some(name)) {
        return Err(ConnWriteError::AlreadyGranted(name.to_string()));
    }
    grants.push(serde_yaml::Value::String(name.to_string()));

    write_doc(config_path, &doc)?;

    let body_json: Value = serde_json::to_value(&body_yaml)
        .map_err(|e| ConnWriteError::Parse(format!("reading staged connection body: {e}")))?;
    Ok(body_json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_parse_round_trips() {
        assert_eq!(ConnectionKind::parse("mcp").unwrap(), ConnectionKind::Mcp);
        assert_eq!(ConnectionKind::parse("cli").unwrap(), ConnectionKind::Cli);
        assert_eq!(ConnectionKind::parse("rest").unwrap(), ConnectionKind::Rest);
        assert_eq!(ConnectionKind::Mcp.as_str(), "mcp");
    }

    #[test]
    fn kind_parse_rejects_unknown() {
        let err = ConnectionKind::parse("grpc").unwrap_err();
        assert!(matches!(err, ConnWriteError::InvalidKind(k) if k == "grpc"));
    }

    #[test]
    fn mcp_entry_requires_command_or_url() {
        let spec = ConnectionSpec::Mcp {
            command: None,
            args: vec![],
            url: None,
            env: vec![],
        };
        assert!(matches!(
            spec_to_entry(&spec).unwrap_err(),
            ConnWriteError::InvalidFields(_)
        ));
    }

    #[test]
    fn cli_entry_requires_command() {
        let spec = ConnectionSpec::Cli {
            command: "  ".into(),
            working_directory: None,
            env: vec![],
        };
        assert!(matches!(
            spec_to_entry(&spec).unwrap_err(),
            ConnWriteError::InvalidFields(_)
        ));
    }

    #[test]
    fn rest_entry_requires_base_url() {
        let spec = ConnectionSpec::Rest {
            base_url: "".into(),
            headers: vec![],
        };
        assert!(matches!(
            spec_to_entry(&spec).unwrap_err(),
            ConnWriteError::InvalidFields(_)
        ));
    }

    #[test]
    fn name_with_slash_is_rejected() {
        assert!(matches!(
            validate_name("pack/tool"),
            Err(ConnWriteError::InvalidName(_))
        ));
        assert!(validate_name("github_api").is_ok());
    }
}
