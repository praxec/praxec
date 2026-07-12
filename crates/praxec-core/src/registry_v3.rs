//! Registry-v3 — the `praxec.packs/v3` tool/pack registry loader (D4b).
//!
//! One JSON Schema, `schemas/registry.schema.json`, is the source of truth;
//! this module is the typed loader over it (same convention as
//! [`crate::tool_descriptor`]: hand-authored types + runtime jsonschema
//! validation, because the schema cross-`$ref`s
//! `tool-descriptor.schema.json`, which typify cannot resolve — the drift
//! guard lives in `tests/registry_v3_schema_snapshot.rs`).
//!
//! # Design invariants (docs/design-0.0.17-tool-source-ecosystem.md §D4b)
//!
//! - **v3 is a compatible superset of v2.** ADR-0013 `praxec.packs/v2`
//!   documents load unchanged (`descriptor` / `suggested_workflows` /
//!   `crossmatrix` simply absent). A v2 document that smuggles v3-only
//!   surface fails typed (`REGISTRY_V3_FIELD_UNDER_V2`) — the marker and the
//!   surface must agree, never drift.
//! - **The registry's tool descriptors ARE D1 [`ToolDescriptor`]s.** The
//!   `descriptor` field reuses the type verbatim (no parallel model); its
//!   own schema + cross-field validation ([`ToolDescriptor::validate`], FM2)
//!   run as part of [`Registry::validate`].
//! - **The crossmatrix is derived, never hand-maintained (FM7).** Every row
//!   must reference a tool id in `tools` and a workflow definitionId that
//!   some tool suggests (tool-level or descriptor-level
//!   `suggested_workflows`) — a divergent entry fails the load, so a
//!   drifted registry is not layerable. Deeper resolution of definitionIds
//!   against the merged config (FM4, `STALE_TOOL_SUGGESTION`) happens at
//!   config-load, not here: the registry only knows its own inventory.
//! - **The registry never grants.** It describes what a pack offers; the
//!   operator's `grant_connections:` decides what activates (D3, unchanged).
//! - **Closed enums throughout** ([`RegistrySchema`], [`PackTier`],
//!   [`CrossmatrixRole`]) with exhaustive `match` — no `Other(String)`
//!   escape, mirroring [`crate::tool_descriptor::ToolKind`].

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tool_descriptor::{
    ProvisionProvider, TOOL_DESCRIPTOR_SCHEMA, ToolDescriptor, ToolDescriptorError, ToolKind,
};

/// The canonical registry schema bytes, single-sourced from the shipped
/// `schemas/registry.schema.json` (same embedding convention as
/// [`TOOL_DESCRIPTOR_SCHEMA`]).
pub const REGISTRY_SCHEMA: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../schemas/registry.schema.json"
));

/// The gateway config schema bytes — the descriptor's `reach.connection`
/// `$ref` bottoms out here (registry → tool-descriptor → gateway-config).
const GATEWAY_CONFIG_SCHEMA: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../schemas/gateway-config.schema.json"
));

/// `$id`s the cross-`$ref`'d schemas declare; the registry schema's relative
/// refs resolve against its own `$id`, so both resources must be hosted at
/// these URIs in the validation registry.
const GATEWAY_CONFIG_SCHEMA_URI: &str = "https://praxec.dev/schemas/gateway-config.schema.json";
const TOOL_DESCRIPTOR_SCHEMA_URI: &str = "https://praxec.dev/schemas/tool-descriptor.schema.json";

/// Registry hosting the transitively-`$ref`'d schemas under their `$id`s.
/// Built once; the `.expect()`s encode the shipped-schema invariant (broken
/// shipped bytes are a build/test failure, not config dependent — see
/// [`crate::hop::HOP_REGISTRY`] for the pattern).
static REGISTRY_V3_REGISTRY: LazyLock<jsonschema::Registry> = LazyLock::new(|| {
    let gateway: Value = serde_json::from_str(GATEWAY_CONFIG_SCHEMA)
        .expect("invariant: shipped gateway-config.schema.json parses as JSON");
    let descriptor: Value = serde_json::from_str(TOOL_DESCRIPTOR_SCHEMA)
        .expect("invariant: shipped tool-descriptor.schema.json parses as JSON");
    jsonschema::Registry::new()
        .add(GATEWAY_CONFIG_SCHEMA_URI, gateway)
        .expect("invariant: gateway config schema URI is valid")
        .add(TOOL_DESCRIPTOR_SCHEMA_URI, descriptor)
        .expect("invariant: tool descriptor schema URI is valid")
        .prepare()
        .expect("invariant: shipped schemas are valid registry resources")
});

/// The compiled registry validator (schema + registry), built once.
static REGISTRY_V3_VALIDATOR: LazyLock<jsonschema::Validator> = LazyLock::new(|| {
    let schema: Value = serde_json::from_str(REGISTRY_SCHEMA)
        .expect("invariant: shipped registry.schema.json parses as JSON");
    jsonschema::options()
        .with_registry(&REGISTRY_V3_REGISTRY)
        .build(&schema)
        .expect("invariant: shipped registry schema compiles")
});

/// Typed failures from registry load + validation. Every variant carries a
/// stable `SCREAMING_SNAKE` code in its message so callers (and operators
/// reading audit trails) can match on it without string archaeology —
/// mirrors [`ToolDescriptorError`].
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// The configured registry file is unreadable (missing, wrong path,
    /// permissions). A registry the operator *named* and praxec cannot read is
    /// an operator error, never a shrug: the caller fails fast rather than
    /// booting registry-less and silently losing the topology it was
    /// configured with.
    #[error("REGISTRY_READ: cannot read registry file `{path}`: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    /// The bytes are not YAML/JSON.
    #[error("REGISTRY_PARSE: {0}")]
    Yaml(#[from] serde_yaml::Error),
    /// The JSON does not deserialize into the registry types (post-schema,
    /// this indicates schema/type drift — see
    /// `tests/registry_v3_schema_snapshot.rs`).
    #[error("REGISTRY_PARSE: {0}")]
    Parse(#[from] serde_json::Error),
    /// The document fails the canonical JSON Schema. Carries every
    /// violation, joined, so the author fixes them all in one pass.
    #[error("REGISTRY_SCHEMA: {0}")]
    Schema(String),
    /// The `schema` marker is missing or outside the closed set — the
    /// loader refuses to guess what shape follows.
    #[error(
        "REGISTRY_UNKNOWN_SCHEMA: `{found}` is not a supported registry schema \
         (expected one of: praxec.packs/v2, praxec.packs/v3)"
    )]
    UnknownSchema { found: String },
    /// A document marked `praxec.packs/v2` carries v3-only surface — the
    /// marker and the surface must agree; bump the marker, don't smuggle.
    #[error(
        "REGISTRY_V3_FIELD_UNDER_V2: `{site}` is v3-only surface but the document \
         declares schema `praxec.packs/v2`; declare `praxec.packs/v3` instead"
    )]
    V3FieldUnderV2 { site: String },
    /// Two packs share an `id` — layering would be ambiguous.
    #[error("REGISTRY_DUPLICATE_PACK_ID: pack id `{0}` appears more than once")]
    DuplicatePackId(String),
    /// Two tools share an `id` — crossmatrix / `requires:` refs would be
    /// ambiguous.
    #[error("REGISTRY_DUPLICATE_TOOL_ID: tool id `{0}` appears more than once")]
    DuplicateToolId(String),
    /// A `providers` key is outside the closed [`ProvisionProvider`] set
    /// (defense in depth behind the schema's `propertyNames` for
    /// directly-constructed registries).
    #[error(
        "REGISTRY_UNKNOWN_PROVIDER: tool `{tool}` declares provider `{provider}` \
         (expected one of: docker, release, cargo, npx, uvx)"
    )]
    UnknownProvider { tool: String, provider: String },
    /// A tool's D1 descriptor failed its own cross-field validation (FM2 —
    /// e.g. `TOOL_KIND_MISMATCH`); no partial registry.
    #[error("REGISTRY_TOOL_DESCRIPTOR: tool `{tool}`: {source}")]
    Descriptor {
        tool: String,
        source: ToolDescriptorError,
    },
    /// A tool's descriptor `kind` conflicts with its declared v2
    /// coordinates (e.g. a rest descriptor on a tool that declares an
    /// mcp `command`).
    #[error(
        "REGISTRY_TOOL_KIND_CONFLICT: tool `{tool}` declares `{coordinate}` but its \
         descriptor kind is `{kind}`; {coordinate} does not apply to {kind} tools"
    )]
    ToolKindConflict {
        tool: String,
        kind: &'static str,
        coordinate: &'static str,
    },
    /// A crossmatrix row references a tool id absent from `tools` (FM7 —
    /// the matrix is derived; a dangling edge means the registry drifted).
    #[error(
        "REGISTRY_CROSSMATRIX_UNKNOWN_TOOL: crossmatrix row (tool `{tool}`, workflow \
         `{workflow}`) references a tool id absent from this registry's `tools`"
    )]
    CrossmatrixUnknownTool { tool: String, workflow: String },
    /// A crossmatrix row references a workflow definitionId no tool in this
    /// registry suggests (FM7).
    #[error(
        "REGISTRY_CROSSMATRIX_UNKNOWN_WORKFLOW: crossmatrix row (tool `{tool}`, workflow \
         `{workflow}`) references a workflow definitionId absent from every \
         `suggested_workflows` in this registry"
    )]
    CrossmatrixUnknownWorkflow { tool: String, workflow: String },
}

/// The closed registry schema-marker set. Closed enum on purpose (mirrors
/// [`ToolKind`]): no `Other(String)` escape — a new registry version is a
/// deliberate schema amendment, not a config-time string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RegistrySchema {
    /// ADR-0013 `praxec.packs/v2` — the mcp-only tool-provider catalog.
    #[serde(rename = "praxec.packs/v2")]
    V2,
    /// `praxec.packs/v3` — v2 + per-tool D1 descriptors +
    /// `suggested_workflows` + the crossmatrix topology index.
    #[serde(rename = "praxec.packs/v3")]
    V3,
}

impl RegistrySchema {
    /// The closed set of allowed marker tokens, in version order.
    pub const ALL_TOKENS: &'static [&'static str] = &["praxec.packs/v2", "praxec.packs/v3"];

    pub fn as_token(self) -> &'static str {
        match self {
            RegistrySchema::V2 => "praxec.packs/v2",
            RegistrySchema::V3 => "praxec.packs/v3",
        }
    }

    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "praxec.packs/v2" => Some(RegistrySchema::V2),
            "praxec.packs/v3" => Some(RegistrySchema::V3),
            _ => None,
        }
    }
}

/// Pack distribution tier. Closed enum, same idiom as [`RegistrySchema`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PackTier {
    Open,
    Premium,
}

impl PackTier {
    pub const ALL_TOKENS: &'static [&'static str] = &["open", "premium"];

    pub fn as_token(self) -> &'static str {
        match self {
            PackTier::Open => "open",
            PackTier::Premium => "premium",
        }
    }
}

/// Provenance of a crossmatrix edge (FM7: the matrix is *derived* from
/// descriptors + workflow step refs, never hand-maintained). Closed enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CrossmatrixRole {
    /// Derived from the tool's `suggested_workflows` — the workflow
    /// composes (maximizes) the tool.
    Suggested,
    /// Derived from the workflow's step `executor.connection` refs — the
    /// workflow depends on the tool to run.
    Dependency,
}

impl CrossmatrixRole {
    pub const ALL_TOKENS: &'static [&'static str] = &["suggested", "dependency"];

    pub fn as_token(self) -> &'static str {
        match self {
            CrossmatrixRole::Suggested => "suggested",
            CrossmatrixRole::Dependency => "dependency",
        }
    }
}

/// One pack entry — a layerable `repos:` unit carrying workflows + the tool
/// descriptors they maximize. Unchanged from v2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pack {
    pub id: String,
    pub name: String,
    /// The `repos:` namespace the pack layers under (V20 uniqueness).
    pub namespace: String,
    #[serde(default)]
    pub description: String,
    /// Git URL of the pack repo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<PackTier>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Tool ids (from this registry's `tools`) the pack's workflows
    /// depend on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    /// Tool ids provisioned outside this registry (not resolvable here,
    /// by design).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub external: Vec<String>,
    /// Optional parent pack id this pack layers on top of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extends: Option<String>,
}

/// One tool entry — the v2 provider-catalog fields as-is, plus the v3
/// additions: an optional D1 [`ToolDescriptor`] (so the registry can
/// describe cli and rest tools, not just mcp) and `suggested_workflows`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryTool {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// The binary the tool's connection spawns (v2 mcp coordinate; also
    /// valid for cli tools). Conflicts with a rest descriptor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// ADR-0013 MCP registry id — mcp-only; conflicts with a cli/rest
    /// descriptor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_registry_id: Option<String>,
    /// ADR-0013 provider chain, provider → coordinate (image / URL /
    /// crate). Keys are validated against the closed [`ProvisionProvider`]
    /// set (schema `propertyNames` + [`Registry::validate`]); string-keyed
    /// here because the map shape predates the enum (v2 back-compat).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, String>,
    /// v3 — the D1 descriptor. Reused verbatim (no parallel model); its
    /// cross-field validation runs in [`Registry::validate`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub descriptor: Option<ToolDescriptor>,
    /// v3 — workflow definitionIds (namespace-qualified) that compose this
    /// tool.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggested_workflows: Vec<String>,
}

impl RegistryTool {
    /// Every workflow definitionId this tool suggests: the registry-level
    /// list plus the descriptor-level list, deduplicated, first-seen order.
    pub fn effective_suggested_workflows(&self) -> Vec<&str> {
        let descriptor_workflows = self
            .descriptor
            .iter()
            .flat_map(|d| d.suggested_workflows.iter());
        let mut seen = HashSet::new();
        self.suggested_workflows
            .iter()
            .chain(descriptor_workflows)
            .map(String::as_str)
            .filter(|w| seen.insert(*w))
            .collect()
    }
}

/// One crossmatrix row — a derived tool × workflow edge (FM7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrossmatrixRow {
    /// A tool id present in this registry's `tools`.
    pub tool: String,
    /// A workflow definitionId suggested somewhere in this registry.
    pub workflow: String,
    pub role: CrossmatrixRole,
}

/// The loaded registry — see module docs and
/// `docs/design-0.0.17-tool-source-ecosystem.md` §D4b.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    /// The closed schema marker ([`RegistrySchema`]) — v2 documents load
    /// unchanged (v3 is a compatible superset) and keep their marker.
    pub schema: RegistrySchema,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub packs: Vec<Pack>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<RegistryTool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub crossmatrix: Vec<CrossmatrixRow>,
}

impl Registry {
    /// Load a registry from a file on disk (the `packs.yaml` an operator points
    /// `discovery.registry` at): read → [`load_str`](Self::load_str). Same
    /// fail-fast contract — an unreadable, unparseable, or invalid file yields a
    /// typed [`RegistryError`], never a `None`-shaped shrug.
    pub fn load_path(path: &Path) -> Result<Self, RegistryError> {
        let text = std::fs::read_to_string(path).map_err(|source| RegistryError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::load_str(&text)
    }

    /// Load a registry from YAML (or JSON — YAML is a superset) text:
    /// parse → [`load_value`](Self::load_value). Fail-fast: the first
    /// failing stage returns a typed [`RegistryError`]; there is no
    /// partially-loaded registry.
    pub fn load_str(text: &str) -> Result<Self, RegistryError> {
        let value: Value = serde_yaml::from_str(text)?;
        Self::load_value(value)
    }

    /// Load a registry from an already-parsed JSON value: schema-marker
    /// gate → canonical-schema validate → deserialize →
    /// [`validate`](Self::validate).
    pub fn load_value(value: Value) -> Result<Self, RegistryError> {
        // 1. The marker gates everything — refuse to guess what shape an
        //    unknown version carries.
        let marker = value
            .get("schema")
            .and_then(Value::as_str)
            .unwrap_or("<missing>");
        let schema =
            RegistrySchema::from_token(marker).ok_or_else(|| RegistryError::UnknownSchema {
                found: marker.to_string(),
            })?;

        // 2. Structural validation against the shipped v3 schema. v3 is a
        //    compatible superset of v2, so a v2 document validates with its
        //    marker lifted — validation only; the loaded registry keeps
        //    schema == V2. The smuggle check first makes the lift honest:
        //    a v2 document must actually be v2-shaped.
        match schema {
            RegistrySchema::V3 => validate_document(&value)?,
            RegistrySchema::V2 => {
                reject_v3_surface_under_v2(&value)?;
                let mut lifted = value.clone();
                lifted["schema"] = Value::String(RegistrySchema::V3.as_token().to_string());
                validate_document(&lifted)?;
            }
        }

        let registry: Registry = serde_json::from_value(value)?;
        registry.validate()?;
        Ok(registry)
    }

    /// Cross-field validation the schema cannot express. Fail-fast on the
    /// first violation; a registry that fails here is not layerable (FM7).
    pub fn validate(&self) -> Result<(), RegistryError> {
        let mut pack_ids: HashSet<&str> = HashSet::with_capacity(self.packs.len());
        for pack in &self.packs {
            if !pack_ids.insert(&pack.id) {
                return Err(RegistryError::DuplicatePackId(pack.id.clone()));
            }
        }

        let mut tool_ids: HashSet<&str> = HashSet::with_capacity(self.tools.len());
        for tool in &self.tools {
            if !tool_ids.insert(&tool.id) {
                return Err(RegistryError::DuplicateToolId(tool.id.clone()));
            }

            // Defense in depth behind the schema's `propertyNames` — the
            // provider map is string-keyed for v2 back-compat, so the
            // closed-set check must also hold for directly-constructed
            // registries.
            for provider in tool.providers.keys() {
                if !ProvisionProvider::ALL_TOKENS.contains(&provider.as_str()) {
                    return Err(RegistryError::UnknownProvider {
                        tool: tool.id.clone(),
                        provider: provider.clone(),
                    });
                }
            }

            if let Some(descriptor) = &tool.descriptor {
                // The descriptor's own cross-field rules (FM2 — kind vs
                // reach.connection.kind, dispatch coordinates) run first.
                descriptor
                    .validate()
                    .map_err(|source| RegistryError::Descriptor {
                        tool: tool.id.clone(),
                        source,
                    })?;

                // The descriptor's kind must not conflict with the tool's
                // declared v2 coordinates. Exhaustive match — a new kind
                // fails to compile until its coordinate rules are decided.
                let conflict = |coordinate: &'static str| RegistryError::ToolKindConflict {
                    tool: tool.id.clone(),
                    kind: descriptor.kind.as_token(),
                    coordinate,
                };
                match descriptor.kind {
                    // mcp tools own every v2 coordinate (command,
                    // mcp_registry_id, providers) — that IS the v2 model.
                    ToolKind::Mcp => {}
                    // cli tools spawn a binary (`command` and `providers`
                    // apply) but have no MCP registry identity.
                    ToolKind::Cli => {
                        if tool.mcp_registry_id.is_some() {
                            return Err(conflict("mcp_registry_id"));
                        }
                    }
                    // rest tools reach over HTTP: nothing to spawn, nothing
                    // in the MCP registry.
                    ToolKind::Rest => {
                        if tool.mcp_registry_id.is_some() {
                            return Err(conflict("mcp_registry_id"));
                        }
                        if tool.command.is_some() {
                            return Err(conflict("command"));
                        }
                    }
                }
            }
        }

        // FM7 — every crossmatrix edge must resolve inside this registry.
        let known_workflows: HashSet<&str> = self
            .tools
            .iter()
            .flat_map(RegistryTool::effective_suggested_workflows)
            .collect();
        for row in &self.crossmatrix {
            if !tool_ids.contains(row.tool.as_str()) {
                return Err(RegistryError::CrossmatrixUnknownTool {
                    tool: row.tool.clone(),
                    workflow: row.workflow.clone(),
                });
            }
            if !known_workflows.contains(row.workflow.as_str()) {
                return Err(RegistryError::CrossmatrixUnknownWorkflow {
                    tool: row.tool.clone(),
                    workflow: row.workflow.clone(),
                });
            }
        }
        Ok(())
    }

    // ── the D6 selector read surface ──────────────────────────────────────

    /// Look up a tool by id.
    pub fn tool(&self, id: &str) -> Option<&RegistryTool> {
        self.tools.iter().find(|t| t.id == id)
    }

    /// The crossmatrix rows — the `topology_refs` the selector reads.
    pub fn crossmatrix(&self) -> &[CrossmatrixRow] {
        &self.crossmatrix
    }

    /// The D1 descriptors this registry carries — the tool catalog the discovery
    /// index indexes (`DiscoveryKind::Tool`), owned so the indexer can stamp
    /// each one's `embedding` slot.
    ///
    /// v2-only tools carry no descriptor and are therefore absent: a bare
    /// provider-catalog row has no operations, no reach, no typed I/O — nothing
    /// a caller could *do* with the search hit. Indexing it would surface a tool
    /// that leads nowhere, which is worse than not surfacing it.
    pub fn tool_descriptors(&self) -> Vec<ToolDescriptor> {
        self.tools
            .iter()
            .filter_map(|tool| tool.descriptor.clone())
            .collect()
    }

    /// Every workflow definitionId associated with a tool: its effective
    /// `suggested_workflows` plus its crossmatrix edges, deduplicated,
    /// first-seen order.
    pub fn workflows_for_tool(&self, tool_id: &str) -> Vec<&str> {
        let suggested = self
            .tool(tool_id)
            .map(RegistryTool::effective_suggested_workflows)
            .unwrap_or_default();
        let matrix = self
            .crossmatrix
            .iter()
            .filter(|row| row.tool == tool_id)
            .map(|row| row.workflow.as_str());
        let mut seen = HashSet::new();
        suggested
            .into_iter()
            .chain(matrix)
            .filter(|w| seen.insert(*w))
            .collect()
    }

    /// Every tool associated with a workflow definitionId (via
    /// `suggested_workflows` or a crossmatrix edge), in `tools` order.
    pub fn tools_for_workflow(&self, definition_id: &str) -> Vec<&RegistryTool> {
        self.tools
            .iter()
            .filter(|tool| {
                tool.effective_suggested_workflows()
                    .contains(&definition_id)
                    || self
                        .crossmatrix
                        .iter()
                        .any(|row| row.tool == tool.id && row.workflow == definition_id)
            })
            .collect()
    }
}

/// Validate a document against the compiled canonical schema, collecting
/// every violation (same contract as [`ToolDescriptor::load_value`]).
fn validate_document(value: &Value) -> Result<(), RegistryError> {
    let validator = &*REGISTRY_V3_VALIDATOR;
    if validator.is_valid(value) {
        return Ok(());
    }
    let errs: Vec<String> = validator
        .iter_errors(value)
        .map(|e| format!("{} (at {})", e, e.instance_path()))
        .collect();
    Err(RegistryError::Schema(errs.join("; ")))
}

/// A `praxec.packs/v2` document must not smuggle v3-only surface — the
/// marker and the shape must agree (poka-yoke: bump the marker instead).
fn reject_v3_surface_under_v2(value: &Value) -> Result<(), RegistryError> {
    if value.get("crossmatrix").is_some() {
        return Err(RegistryError::V3FieldUnderV2 {
            site: "crossmatrix".to_string(),
        });
    }
    if let Some(tools) = value.get("tools").and_then(Value::as_array) {
        for tool in tools {
            let id = tool.get("id").and_then(Value::as_str).unwrap_or("<no id>");
            for field in ["descriptor", "suggested_workflows"] {
                if tool.get(field).is_some() {
                    return Err(RegistryError::V3FieldUnderV2 {
                        site: format!("tools[{id}].{field}"),
                    });
                }
            }
        }
    }
    Ok(())
}
