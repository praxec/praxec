//! Argument structs + schema helpers for the MCP tool surface.
//!
//! One `*Args` struct per tool. Both the published JSON Schema (via
//! `schemars::JsonSchema`) and the per-handler argument extraction (via
//! `serde::Deserialize`) come from these definitions.
//!
//! Required-field policy is encoded twice on purpose: the per-call required
//! list passed to `schema_for_args` controls what the published schema
//! advertises; the handler's `.ok_or_else(... "is required")` controls what
//! the runtime rejects. They're maintained as a pair because the published
//! surface and the runtime have diverged historically (some schema-required
//! fields are silently defaulted by the runtime), and the parity tests fix
//! that contract in place. Every field is `Option<T>` so the deserializer
//! never produces serde's default missing-field error — handlers raise the
//! canonical "<field> is required" message instead.
//!
//! Tool-specific schema shims (`integer_schema`, `object_schema`,
//! `discovery_kind_schema`) override the default schemars output so the
//! published schema matches what callers see today.

use std::sync::Arc;

use rmcp::model::JsonObject;
use schemars::JsonSchema;
use schemars::r#gen::{SchemaGenerator, SchemaSettings};
use schemars::schema::{InstanceType, Schema, SchemaObject};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SearchArgs {
    pub query: Option<String>,
    #[schemars(schema_with = "discovery_kind_schema")]
    pub kind: Option<String>,
    #[serde(default = "default_limit")]
    #[schemars(schema_with = "limit_schema")]
    pub limit: u64,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DescribeArgs {
    pub id: Option<String>,
    /// SPEC §8.2 — when present, resolve guidance bodies from this
    /// workflow's pinned snapshot so an in-flight instance sees the
    /// body that existed at `workflow.start`, not whatever the live
    /// config currently says. Workflow / capability lookups ignore it.
    pub workflow_id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StartArgs {
    pub definition_id: Option<String>,
    #[schemars(schema_with = "object_schema")]
    pub input: Option<Value>,
    /// SPEC §20.2 — optional trace id propagated to every audit event
    /// for the created workflow instance. Opaque to the gateway.
    pub trace_id: Option<String>,
    /// SPEC §20.2 — optional run id for grouping related workflow
    /// instances. Opaque to the gateway.
    pub run_id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GetArgs {
    pub workflow_id: Option<String>,
    /// SPEC §20.2 — optional per-call trace id override. The instance's
    /// persisted `trace_id` is used by default.
    pub trace_id: Option<String>,
    pub run_id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SubmitArgs {
    pub workflow_id: Option<String>,
    #[schemars(schema_with = "integer_schema")]
    pub expected_version: Option<u64>,
    pub transition: Option<String>,
    #[schemars(schema_with = "object_schema")]
    pub arguments: Option<Value>,
    /// SPEC §6.3 — optional model-authored summary. Stored to
    /// `context.summary` on commit; surfaced in every response.
    pub summary: Option<String>,
    /// SPEC §20.2 — optional per-submit trace id override.
    pub trace_id: Option<String>,
    pub run_id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExplainArgs {
    pub workflow_id: Option<String>,
    pub transition: Option<String>,
}

pub(crate) fn default_limit() -> u64 {
    10
}

// ---------- per-field schema overrides ----------------------------------
//
// Schemars's default schemas for `u64`/`Option<Value>` carry extra hints
// (`format: uint64`, `minimum: 0`, `additionalProperties: true`) that the
// previous hand-written schemas didn't. These shims keep the published
// schema byte-equivalent to the pre-refactor surface.

pub(crate) fn integer_schema(_: &mut SchemaGenerator) -> Schema {
    SchemaObject {
        instance_type: Some(InstanceType::Integer.into()),
        ..Default::default()
    }
    .into()
}

pub(crate) fn limit_schema(r#gen: &mut SchemaGenerator) -> Schema {
    let mut schema = match integer_schema(r#gen) {
        Schema::Object(o) => o,
        Schema::Bool(_) => unreachable!("integer_schema always returns Schema::Object"),
    };
    schema.metadata().default = Some(json!(default_limit()));
    schema.into()
}

pub(crate) fn object_schema(_: &mut SchemaGenerator) -> Schema {
    SchemaObject {
        instance_type: Some(InstanceType::Object.into()),
        ..Default::default()
    }
    .into()
}

pub(crate) fn discovery_kind_schema(_: &mut SchemaGenerator) -> Schema {
    SchemaObject {
        instance_type: Some(InstanceType::String.into()),
        enum_values: Some(vec![
            json!("workflow"),
            json!("capability"),
            json!("connection"),
        ]),
        ..Default::default()
    }
    .into()
}

/// Build the rmcp `Tool.input_schema` for a typed `*Args` struct. The
/// `required` list is supplied explicitly because some schema-required
/// fields are silently defaulted by the runtime — see the args-struct
/// comment block above.
pub(crate) fn schema_for_args<T: JsonSchema>(required: &[&'static str]) -> Arc<JsonObject> {
    let generator = SchemaSettings::draft07()
        .with(|s| {
            s.option_add_null_type = false;
            s.inline_subschemas = true;
            s.meta_schema = None;
        })
        .into_generator();
    let root = generator.into_root_schema_for::<T>();
    let mut value =
        serde_json::to_value(&root).expect("schemars produces JSON-serializable schema");
    let obj = value
        .as_object_mut()
        .expect("root schema is always an object");
    obj.remove("$schema");
    obj.remove("title");
    obj.remove("definitions");
    obj.remove("description");

    if let Some(Value::Object(props)) = obj.get_mut("properties") {
        for (_, v) in props.iter_mut() {
            if let Value::Object(field) = v {
                // Strip schemars hints the legacy hand-written schemas
                // didn't carry: numeric `format`/`minimum`, the recursive
                // `additionalProperties: true` schemars stamps on
                // `Map<String, Value>`, and field doc-comments.
                field.remove("format");
                field.remove("minimum");
                field.remove("additionalProperties");
                field.remove("description");
            }
        }
    }

    if required.is_empty() {
        obj.remove("required");
    } else {
        obj.insert("required".into(), json!(required));
    }
    obj.insert("additionalProperties".into(), Value::Bool(false));
    Arc::new(value.as_object().cloned().expect("still an object"))
}

/// Sparse args for `praxec.query` (§32). Every field optional; the
/// dispatch table (handlers.rs::dispatch_query) selects the operation
/// — home / search / describe / get / explain — by which required
/// fields are present.
#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct QueryArgs {
    /// Search query string. Present → dispatch to `search`.
    pub query: Option<String>,
    /// Search filter (e.g. `workflow`, `skill`, `script`, `capability`,
    /// `lexicon`). Modifier on `search`.
    pub kind: Option<String>,
    /// Describe subject. Present alone → browse-time describe. Present
    /// with `workflow_id` → describe against the workflow's pinned
    /// snapshot (audit fires; SPEC §5.8 + §8.2). Supports the
    /// `lexicon:<term>` namespace prefix per §32.
    pub subject: Option<String>,
    /// Workflow instance id. Alone → `get`. With `transition` →
    /// `explain`. With `subject` → describe-in-workflow.
    pub workflow_id: Option<String>,
    /// Transition name. Required for `explain`; present alongside
    /// `workflow_id`.
    pub transition: Option<String>,
    /// Definition id. Present alone → read that definition's current body +
    /// content hash (SPEC §8.4 — the basis for an edit). Distinct from
    /// `workflow_id` (a running instance) and `subject` (a guidance fragment).
    pub definition_id: Option<String>,
    /// Observability read: present-and-true (alone) → bounded replay of the
    /// structured audit event stream — the SAME events `praxec observe
    /// --follow` emits (heartbeat pulses excluded), each carrying
    /// `workflow_id` / `parent_workflow_id` / `depth` so the client can
    /// rebuild the execution tree. An MCP call returns a response, not a
    /// stream, so this is the PULL complement to the CLI tail: "give me
    /// events since X" — re-query with the returned `next_since` cursor to
    /// tail. Requires `audit.sink: file` (fails fast otherwise).
    pub observe: Option<bool>,
    /// HITL read: present-and-true (alone) → the store-derived queue of every
    /// live mission parked awaiting a human (an `actor: human` approval gate or
    /// an agent's elicitation). Each entry carries the `transition` +
    /// `expectedVersion` a human needs to resolve it, plus a ready-to-fire
    /// resolve link. The MCP-native complement to the CLI `praxec approvals`,
    /// so a human driving through an agent can SEE and clear a gate without a
    /// terminal into the gateway.
    pub approvals: Option<bool>,
    /// RFC3339 floor for `observe` — only events with `timestamp >= since`
    /// are returned. Modifier on `observe` only.
    pub since: Option<String>,
    /// Search result cap. Modifier on `search` and `observe`.
    #[schemars(schema_with = "integer_schema")]
    pub limit: Option<u64>,
}

/// Sparse args for `praxec.command` (§32). Every field optional; the
/// dispatch table (handlers.rs::dispatch_command) selects the operation
/// — start / submit / define — by which required fields are present.
#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CommandArgs {
    /// Workflow definition id to start. Present (with no `workflow_id`
    /// and no `subject`) → `start`.
    pub definition_id: Option<String>,
    /// Initial input for `start`.
    #[schemars(schema_with = "object_schema")]
    pub input: Option<Value>,
    /// Workflow instance id. Required for `submit` (alongside
    /// `expected_version` and `transition`).
    pub workflow_id: Option<String>,
    /// Optimistic-concurrency version for `submit`. Required for the
    /// submit shape.
    #[schemars(schema_with = "integer_schema")]
    pub expected_version: Option<u64>,
    /// Transition name for `submit`.
    pub transition: Option<String>,
    /// Transition arguments for `submit`.
    #[schemars(schema_with = "object_schema")]
    pub arguments: Option<Value>,
    /// Define subject — namespaced, e.g. `lexicon:<term>`. Present with
    /// `definition` → `define` (SPEC §32, §30).
    pub subject: Option<String>,
    /// Definition body for `define`. Inner shape per SPEC §30.5:
    /// `{ definition, boundedContext?, refs?, governance? }`.
    #[schemars(schema_with = "object_schema")]
    pub definition: Option<Value>,
    /// SPEC §6.3 — model-authored submit summary. Stored to
    /// `context.summary` on commit. Modifier on `submit`.
    pub summary: Option<String>,
    /// SPEC §20.2 — optional per-call trace id override.
    pub trace_id: Option<String>,
    /// SPEC §20.2 — optional per-call run id. On `start`, also doubles
    /// as a uniqueness assertion per §32 (collisions return
    /// `RUN_ID_ALREADY_RUNNING`).
    pub run_id: Option<String>,
    /// SPEC §30.10.7C — dispatch intent for out-of-band resolution commands.
    /// Present → `cancel_pending_subject` → drop a PENDING_DEFINITION
    /// placeholder without creating or modifying a lexicon entry.
    pub intent: Option<String>,
    /// SPEC §30.10.7C — subject to cancel. Required when
    /// `intent == "cancel_pending_subject"`. Wire key is `unknown_subject`
    /// (snake_case, not camelCase) matching the HATEOAS cancel-link shape
    /// emitted by SPEC §30.10.5.
    #[serde(rename = "unknown_subject")]
    pub unknown_subject: Option<String>,
    /// P6 — in-band config reload. `reload: true` fires the same gated
    /// rebuild+swap as SIGHUP (re-reads the config + `repos:` from disk so a
    /// post-startup repo becomes visible), without adding a third MCP tool.
    /// Present-and-true → reload; all other fields are ignored for that call.
    pub reload: Option<bool>,
}
