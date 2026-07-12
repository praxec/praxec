//! Tool descriptor — the v0.0.17 tool-source ecosystem contract (D1).
//!
//! One JSON Schema, `schemas/tool-descriptor.schema.json`, is the source of
//! truth; this module is the typed loader over it. The types are
//! hand-authored (not typify-generated) for the same reason as [`crate::hop`]:
//! the descriptor's `reach.connection` cross-`$ref`s
//! `gateway-config.schema.json#/$defs/connection`, which typify cannot
//! resolve — so the schema is registered for **runtime jsonschema
//! validation** and the drift guard lives in
//! `tests/tool_descriptor_schema_snapshot.rs` (mirrors `spec_enum_drift.rs`).
//!
//! # Design invariants (docs/design-0.0.17-tool-source-ecosystem.md)
//!
//! - **`reach.connection` ≡ a literal `connections:` entry.** It is kept as a
//!   raw [`serde_json::Value`] on purpose: install = copy verbatim into
//!   `/connections`, never transform. Typing it here would fork the
//!   connection format the merge + grant gate already govern.
//! - **The descriptor never grants.** [`ToolDescriptor::grant_token`] returns
//!   the bare name the operator must add to `grant_connections:`
//!   (SPEC §9.5); until then the connection is diverted to
//!   `/praxec/_ungrantedConnections` and every operation fails typed
//!   `UNGRANTED_PACK_CONNECTION`. Granting stays a separate operator act.
//! - **`kind` is a closed enum** ([`ToolKind`], exhaustive `match`, no
//!   `Other(String)` escape) mirroring
//!   [`crate::discovery::DiscoveryKind`] / [`crate::discovery::ScriptVerb`].
//!   `validate()` cross-checks it against `reach.connection.kind` and each
//!   operation's dispatch coordinate — a mismatch fails at parse with
//!   `TOOL_KIND_MISMATCH` / `TOOL_OPERATION_DISPATCH_MISMATCH`, never a
//!   partial install (FM2).

use std::path::Path;
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::discovery::ScriptVerb;

/// The canonical descriptor schema bytes, single-sourced from the shipped
/// `schemas/tool-descriptor.schema.json` (same embedding convention as
/// [`crate::hop::HOP_SCHEMA`]).
pub const TOOL_DESCRIPTOR_SCHEMA: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../schemas/tool-descriptor.schema.json"
));

/// The gateway config schema bytes — registered so the descriptor's
/// `reach.connection` `$ref` (`gateway-config.schema.json#/$defs/connection`)
/// resolves against the *exact* shape the config merge already validates.
const GATEWAY_CONFIG_SCHEMA: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../schemas/gateway-config.schema.json"
));

/// The `$id` the gateway config schema declares; the descriptor's relative
/// `$ref` resolves to this URI, so the registry must host the resource there.
const GATEWAY_CONFIG_SCHEMA_URI: &str = "https://praxec.dev/schemas/gateway-config.schema.json";

/// The closed `schema_version` marker every descriptor must carry.
pub const TOOL_DESCRIPTOR_SCHEMA_VERSION: &str = "praxec.tool/v1";

/// Registry hosting the gateway config schema under its `$id`, so a validator
/// compiled from [`TOOL_DESCRIPTOR_SCHEMA`] resolves the `reach.connection`
/// cross-`$ref`. Built once; the `.expect()`s encode the shipped-schema
/// invariant (broken shipped bytes are a build/test failure, not config
/// dependent — see [`crate::hop::HOP_REGISTRY`] for the pattern).
static TOOL_DESCRIPTOR_REGISTRY: LazyLock<jsonschema::Registry> = LazyLock::new(|| {
    let gateway: Value = serde_json::from_str(GATEWAY_CONFIG_SCHEMA)
        .expect("invariant: shipped gateway-config.schema.json parses as JSON");
    jsonschema::Registry::new()
        .add(GATEWAY_CONFIG_SCHEMA_URI, gateway)
        .expect("invariant: gateway config schema URI is valid")
        .prepare()
        .expect("invariant: shipped gateway config schema is a valid registry resource")
});

/// The compiled descriptor validator (schema + registry), built once.
static TOOL_DESCRIPTOR_VALIDATOR: LazyLock<jsonschema::Validator> = LazyLock::new(|| {
    let schema: Value = serde_json::from_str(TOOL_DESCRIPTOR_SCHEMA)
        .expect("invariant: shipped tool-descriptor.schema.json parses as JSON");
    jsonschema::options()
        .with_registry(&TOOL_DESCRIPTOR_REGISTRY)
        .build(&schema)
        .expect("invariant: shipped tool descriptor schema compiles")
});

/// Typed failures from descriptor load + validation. Every variant carries a
/// stable `SCREAMING_SNAKE` code in its message so callers (and operators
/// reading audit trails) can match on it without string archaeology.
#[derive(Debug, thiserror::Error)]
pub enum ToolDescriptorError {
    /// The file could not be read.
    #[error("TOOL_DESCRIPTOR_IO: {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    /// The bytes are not JSON, or the JSON does not deserialize into the
    /// descriptor types (post-schema, this indicates schema/type drift —
    /// see `tests/tool_descriptor_schema_snapshot.rs`).
    #[error("TOOL_DESCRIPTOR_PARSE: {0}")]
    Parse(#[from] serde_json::Error),
    /// The document fails the canonical JSON Schema. Carries every violation,
    /// joined, so the author fixes them all in one pass.
    #[error("TOOL_DESCRIPTOR_SCHEMA: {0}")]
    Schema(String),
    /// FM2 — top-level `kind` and `reach.connection.kind` disagree.
    #[error(
        "TOOL_KIND_MISMATCH: descriptor kind is `{kind}` but reach.connection.kind is \
         `{connection_kind}`; the reach block must embed a connection of the descriptor's kind"
    )]
    KindMismatch {
        kind: &'static str,
        connection_kind: String,
    },
    /// An operation's dispatch coordinate (`mcp_tool` / `rest` / `cli`) is
    /// absent, duplicated, or does not match the descriptor's kind.
    #[error(
        "TOOL_OPERATION_DISPATCH_MISMATCH: operation `{operation}` on a `{kind}` descriptor \
         {detail}"
    )]
    OperationDispatchMismatch {
        operation: String,
        kind: &'static str,
        detail: String,
    },
    /// Two operations share an `id` — dispatch would be ambiguous.
    #[error("TOOL_DUPLICATE_OPERATION_ID: operation id `{0}` appears more than once")]
    DuplicateOperationId(String),
}

/// SPEC-D1 — the closed tool-kind discriminator. Closed enum on purpose
/// (mirrors [`crate::discovery::DiscoveryKind`] / [`ScriptVerb`]): no
/// `Other(String)` escape variant, no `#[serde(other)]`. Adding a fourth kind
/// is a deliberate schema amendment, not a config-time string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolKind {
    /// Local command-line tool; reaches through a `kind: cli` connection.
    Cli,
    /// MCP server; reaches through a `kind: mcp` connection.
    Mcp,
    /// HTTP API; reaches through a `kind: rest` connection.
    Rest,
}

impl ToolKind {
    /// The closed set of allowed kind tokens, in schema order.
    pub const ALL_TOKENS: &'static [&'static str] = &["cli", "mcp", "rest"];

    pub fn as_token(self) -> &'static str {
        match self {
            ToolKind::Cli => "cli",
            ToolKind::Mcp => "mcp",
            ToolKind::Rest => "rest",
        }
    }

    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "cli" => Some(ToolKind::Cli),
            "mcp" => Some(ToolKind::Mcp),
            "rest" => Some(ToolKind::Rest),
            _ => None,
        }
    }
}

/// One entry in the ADR-0013 packs/v2 provider chain (ordered,
/// first-available wins). Closed enum, same idiom as [`ToolKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProvisionProvider {
    Docker,
    Release,
    Cargo,
    Npx,
    Uvx,
}

impl ProvisionProvider {
    pub const ALL_TOKENS: &'static [&'static str] = &["docker", "release", "cargo", "npx", "uvx"];

    pub fn as_token(self) -> &'static str {
        match self {
            ProvisionProvider::Docker => "docker",
            ProvisionProvider::Release => "release",
            ProvisionProvider::Cargo => "cargo",
            ProvisionProvider::Npx => "npx",
            ProvisionProvider::Uvx => "uvx",
        }
    }
}

/// The connection requirement — ties the descriptor to the D3 grant model.
///
/// NOTE (F8, v0.0.18): the descriptor intentionally carries NO `auth` block.
/// A declared-but-unenforced auth requirement shipped in v0.0.17 drafts and
/// was removed: advertising credential requirements the executor never checks
/// or injects is a security footgun. Auth/credential handling returns in
/// v0.0.18 as enforce-then-declare — the field comes back only together with
/// the enforcement (env presence checks / header injection) in the D2
/// tool-source executor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reach {
    /// The `connections:` key the tool's operations reference.
    pub connection_name: String,
    /// Bare pack-local name the operator writes in `grant_connections:`.
    /// Declared, never performed — see [`ToolDescriptor::grant_token`].
    pub grant_as: String,
    /// The LITERAL `connections:` entry — copied verbatim into
    /// `/connections` on install. Kept as a raw [`Value`] so install is
    /// copy-never-transform; shape is enforced by the schema's `$ref` into
    /// `gateway-config.schema.json#/$defs/connection` at load time.
    pub connection: Value,
}

/// Provisioning hint — mirrors the ADR-0013 `praxec.packs/v2` tool entry (no
/// new model). Absent ⇒ the operator supplies reach by hand. Never
/// auto-installs: the doctor *offers with consent* (FM5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provision {
    /// ADR-0013 registry id, e.g. `dev.praxec/<tool>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_registry_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Ordered provider chain, first-available wins.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<ProvisionProvider>,
}

/// `kind: rest` dispatch coordinates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestDispatch {
    pub method: String,
    pub path: String,
}

/// `kind: cli` dispatch coordinates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CliDispatch {
    pub args: Vec<String>,
}

/// One invokable operation. `input_schema` / `output_schema` are the typed
/// I/O contract the selector and the authoring flow read to wire the tool
/// into a workflow's blackboard (consistent with `hop_slot` typed I/O).
/// Exactly one dispatch coordinate must be present, and it must match the
/// descriptor's kind — enforced by [`ToolDescriptor::validate`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Operation {
    pub id: String,
    /// Closed [`ScriptVerb`] vocabulary (SPEC §22.3) — operations classify
    /// into the same action taxonomy scripts already use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verb: Option<ScriptVerb>,
    /// JSON Schema for the operation's arguments.
    pub input_schema: Value,
    /// JSON Schema for the operation's result.
    pub output_schema: Value,
    /// `kind: mcp` — the remote tool name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_tool: Option<String>,
    /// `kind: rest` dispatch coordinates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rest: Option<RestDispatch>,
    /// `kind: cli` dispatch coordinates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli: Option<CliDispatch>,
}

/// The tool descriptor — see module docs and
/// `docs/design-0.0.17-tool-source-ecosystem.md` §D1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDescriptor {
    /// Always [`TOOL_DESCRIPTOR_SCHEMA_VERSION`] (`praxec.tool/v1`) —
    /// enforced by the schema's `const` at load time.
    pub schema_version: String,
    pub name: String,
    /// Tool version (semver-ish, pinned).
    pub version: String,
    /// Git URL / registry coordinate of origin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_repo: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    pub kind: ToolKind,
    pub reach: Reach,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provision: Option<Provision>,
    pub operations: Vec<Operation>,
    /// Workflow definitionIds (namespace-qualified) that compose this tool.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggested_workflows: Vec<String>,
    /// The vector this tool was last indexed by — surface (b) of semantic
    /// discovery (v0.0.18 D4). Written at index time by
    /// [`SemanticDiscoveryIndex::build_with_tools`], read back by
    /// [`embedding_vec`](Self::embedding_vec).
    ///
    /// `f64` because JSON has one number type and the schema slot is
    /// `number[]`; the index compares in `f32` (what
    /// [`cosine_similarity`](crate::embeddings::cosine_similarity) takes).
    /// Absent on a descriptor that has never been indexed, or whose embed
    /// failed — never fabricated.
    ///
    /// [`SemanticDiscoveryIndex::build_with_tools`]: crate::discovery::SemanticDiscoveryIndex::build_with_tools
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<f64>>,
    /// Reserved forward-compat slot (structural dedup fingerprint).
    ///
    /// Still `None` after v0.0.18 D5, deliberately: the structural fingerprint is
    /// the hash of a *workflow's control-flow graph*
    /// ([`structural_fingerprint::fingerprint`](crate::structural_fingerprint::fingerprint)),
    /// and a [`ToolDescriptor`] is a `cli` / `mcp` / `rest` tool — it has no
    /// states and no transitions. Writing a graph hash here would be a
    /// fabrication the dedup pass would then act on. The populated slot lives on
    /// the workflow's catalog entry
    /// ([`DiscoveryItem::structural_fingerprint`](crate::discovery::DiscoveryItem::structural_fingerprint)).
    /// If tool-vs-tool dedup ever earns its place, this is where its own
    /// (different) fingerprint would go.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structural_fingerprint: Option<String>,
}

impl ToolDescriptor {
    /// Load a descriptor from JSON text: parse → canonical-schema validate →
    /// deserialize → [`validate`](Self::validate). Fail-fast: the first
    /// failing stage returns a typed [`ToolDescriptorError`]; there is no
    /// partially-loaded descriptor.
    pub fn load_str(text: &str) -> Result<Self, ToolDescriptorError> {
        let value: Value = serde_json::from_str(text)?;
        Self::load_value(value)
    }

    /// Load a descriptor from a file. See [`load_str`](Self::load_str).
    pub fn load_file(path: &Path) -> Result<Self, ToolDescriptorError> {
        let text = std::fs::read_to_string(path).map_err(|source| ToolDescriptorError::Io {
            path: path.display().to_string(),
            source,
        })?;
        Self::load_str(&text)
    }

    /// Load a descriptor from an already-parsed JSON value. See
    /// [`load_str`](Self::load_str).
    pub fn load_value(value: Value) -> Result<Self, ToolDescriptorError> {
        let validator = &*TOOL_DESCRIPTOR_VALIDATOR;
        if !validator.is_valid(&value) {
            let errs: Vec<String> = validator
                .iter_errors(&value)
                .map(|e| format!("{} (at {})", e, e.instance_path()))
                .collect();
            return Err(ToolDescriptorError::Schema(errs.join("; ")));
        }
        let descriptor: ToolDescriptor = serde_json::from_value(value)?;
        descriptor.validate()?;
        Ok(descriptor)
    }

    /// Cross-field validation the schema cannot express (FM2). Exhaustive
    /// `match` on [`ToolKind`] — a new kind fails to compile until every
    /// check here is decided.
    pub fn validate(&self) -> Result<(), ToolDescriptorError> {
        // FM2 — top-level kind must match reach.connection.kind. The schema
        // guarantees the connection is one of the three gateway shapes (all
        // of which require `kind`), so a missing kind reads as a mismatch.
        let connection_kind = self
            .reach
            .connection
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("<missing>");
        if ToolKind::from_token(connection_kind) != Some(self.kind) {
            return Err(ToolDescriptorError::KindMismatch {
                kind: self.kind.as_token(),
                connection_kind: connection_kind.to_string(),
            });
        }

        // Each operation must carry exactly the dispatch coordinate that
        // matches the descriptor's kind — no extras, no absences.
        let mut seen_ids: Vec<&str> = Vec::with_capacity(self.operations.len());
        for op in &self.operations {
            if seen_ids.contains(&op.id.as_str()) {
                return Err(ToolDescriptorError::DuplicateOperationId(op.id.clone()));
            }
            seen_ids.push(&op.id);

            let mismatch = |detail: String| ToolDescriptorError::OperationDispatchMismatch {
                operation: op.id.clone(),
                kind: self.kind.as_token(),
                detail,
            };
            let present = |name: &str, is_present: bool| {
                if is_present {
                    Some(name.to_string())
                } else {
                    None
                }
            };
            let extras: Vec<String> = match self.kind {
                ToolKind::Cli => [
                    present("mcp_tool", op.mcp_tool.is_some()),
                    present("rest", op.rest.is_some()),
                ],
                ToolKind::Mcp => [
                    present("rest", op.rest.is_some()),
                    present("cli", op.cli.is_some()),
                ],
                ToolKind::Rest => [
                    present("mcp_tool", op.mcp_tool.is_some()),
                    present("cli", op.cli.is_some()),
                ],
            }
            .into_iter()
            .flatten()
            .collect();
            if !extras.is_empty() {
                return Err(mismatch(format!(
                    "carries foreign dispatch coordinate(s): {}",
                    extras.join(", ")
                )));
            }
            let required_present = match self.kind {
                ToolKind::Cli => op.cli.is_some(),
                ToolKind::Mcp => op.mcp_tool.is_some(),
                ToolKind::Rest => op.rest.is_some(),
            };
            if !required_present {
                let required = match self.kind {
                    ToolKind::Cli => "cli",
                    ToolKind::Mcp => "mcp_tool",
                    ToolKind::Rest => "rest",
                };
                return Err(mismatch(format!(
                    "is missing its `{required}` dispatch coordinate"
                )));
            }
        }
        Ok(())
    }

    /// The [`embedding`](Self::embedding) slot as the index scores it: the
    /// stored `f64` wire values narrowed to the `f32`
    /// [`cosine_similarity`](crate::embeddings::cosine_similarity) compares.
    ///
    /// An empty stored vector reads back as `None`: a zero-length vector makes
    /// every cosine comparison 0.0, which would silently disable semantic
    /// matching for this tool while *looking* indexed.
    pub fn embedding_vec(&self) -> Option<Vec<f32>> {
        self.embedding
            .as_ref()
            .filter(|v| !v.is_empty())
            .map(|v| v.iter().map(|&x| x as f32).collect())
    }

    /// Record the vector this descriptor was indexed by. Called at index time
    /// by [`SemanticDiscoveryIndex::build_with_tools`] so a catalog that is
    /// exported, re-served, or inspected carries the vectors it was ranked by
    /// instead of them living only inside a private index array.
    ///
    /// An empty vector *clears* the slot rather than storing `[]` — see
    /// [`embedding_vec`](Self::embedding_vec) for why a zero-length vector is
    /// worse than no vector.
    ///
    /// [`SemanticDiscoveryIndex::build_with_tools`]: crate::discovery::SemanticDiscoveryIndex::build_with_tools
    pub fn set_embedding(&mut self, vector: &[f32]) {
        self.embedding = if vector.is_empty() {
            None
        } else {
            Some(vector.iter().map(|&x| f64::from(x)).collect())
        };
    }

    /// The bare grant token the operator must add to `grant_connections:` on
    /// the `repos:` entry to activate this tool's connection (SPEC §9.5).
    ///
    /// Extraction only — the descriptor *declares* the grant, it never
    /// performs it. Until the operator grants, the connection sits in
    /// `/praxec/_ungrantedConnections` and every operation fails typed
    /// `UNGRANTED_PACK_CONNECTION` (D3 gate, unchanged).
    pub fn grant_token(&self) -> &str {
        &self.reach.grant_as
    }
}

/// Validate a raw value against the gateway config's `$defs/connection`
/// shape — the exact shape the config merge validates. Public seam for D2
/// (install = copy `reach.connection` verbatim) and D4a (`px connections
/// add --from-descriptor`) to re-check a connection body at their own
/// boundaries. Mirrors [`crate::hop::validate_against_schema`]'s
/// plain-`String` error contract.
pub fn validate_gateway_connection(value: &Value) -> Result<(), String> {
    let schema = serde_json::json!({
        "$ref": format!("{GATEWAY_CONFIG_SCHEMA_URI}#/$defs/connection")
    });
    let validator = jsonschema::options()
        .with_registry(&TOOL_DESCRIPTOR_REGISTRY)
        .build(&schema)
        .map_err(|e| format!("invalid connection schema ref: {e}"))?;
    if validator.is_valid(value) {
        return Ok(());
    }
    let errs: Vec<String> = validator
        .iter_errors(value)
        .map(|e| e.to_string())
        .collect();
    Err(format!("connection: {}", errs.join("; ")))
}
