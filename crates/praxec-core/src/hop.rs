//! HOP typed-core — the canonical hand-off-point vocabulary.
//!
//! The specialization-slot contracts (`verifyIn`/`verifyOut`, `detectIn`/…, etc.)
//! ship as a single JSON Schema document — [`HOP_SCHEMA`] — that is registered for
//! **runtime jsonschema validation**, not compiled to Rust types (Spec A.1 §1.2:
//! the runtime has no consumer for the types, and typify cannot resolve the
//! cross-`$ref` shape anyway). It is therefore deliberately kept out of
//! `praxec-schema/build.rs`.
//!
//! Config-authored `$ref`s into the vocabulary are spelled
//! `praxec://hop#/$defs/<def>`; [`HOP_REGISTRY`] is the process-wide
//! [`jsonschema::Registry`] that resolves that alias. A slot cap's
//! `snippet.outputs` fragment, an injected `hop_slot:` contract, and the
//! `SchemaBound` inner-value check all compile against this one registry so a
//! single canonical definition is referenced everywhere and cannot be forked.
//!
//! # Fail-at-boot invariant (Spec A.1 §4.2, FM-1)
//!
//! The `LazyLock` below `.expect()`s that the shipped bytes parse and prepare.
//! Left to first use, a malformed shipped schema would panic *mid-run*. Serve
//! startup forces one deref via [`force_init`], turning a broken shipped schema
//! into a boot failure instead of a latent crash.

use std::sync::LazyLock;

/// The canonical HOP vocabulary bytes, single-sourced from the shipped
/// `schemas/hop.schema.json` (mirrors the `../../schemas` path convention
/// `praxec-schema/build.rs` uses). No `praxec-core -> praxec-schema` dep edge:
/// the bytes are embedded directly.
pub const HOP_SCHEMA: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../schemas/hop.schema.json"
));

/// The alias URI the vocabulary is registered under. Config-side `$ref`s spell
/// it `praxec://hop#/$defs/<def>` — short and stable, distinct from the
/// document's own `$id` (Spec A.1 §1.1, §4.2).
pub const HOP_REGISTRY_URI: &str = "praxec://hop";

/// One process-wide registry, built once from the bundled [`HOP_SCHEMA`] bytes.
///
/// Prepared via the verified jsonschema 0.46 API
/// (`Registry::new().add(uri, json)?.prepare()?`); hand it to a validator build
/// with `jsonschema::options().with_registry(&HOP_REGISTRY).build(schema)`.
///
/// The `.expect()`s encode the shipped-schema invariant (see module docs):
/// broken bytes are a boot failure via [`force_init`], not a mid-run panic.
pub static HOP_REGISTRY: LazyLock<jsonschema::Registry> = LazyLock::new(|| {
    let schema: serde_json::Value = serde_json::from_str(HOP_SCHEMA)
        .expect("invariant: shipped hop.schema.json parses as JSON");
    jsonschema::Registry::new()
        .add(HOP_REGISTRY_URI, schema)
        .expect("invariant: hop registry alias URI is valid")
        .prepare()
        .expect("invariant: shipped hop schema is a valid registry resource")
});

/// Force the [`HOP_REGISTRY`] `LazyLock` to initialize (a single deref).
///
/// Called once at serve startup so a broken shipped schema fails at **boot**,
/// not on the first slot validation mid-run (Spec A.1 §4.2, FM-1).
pub fn force_init() {
    LazyLock::force(&HOP_REGISTRY);
}

/// Compile a `jsonschema::Validator` with [`HOP_REGISTRY`] attached, so a
/// `$ref` into the shipped vocabulary (`praxec://hop#/$defs/<def>`) resolves.
///
/// This is the **strictly-widening** replacement for bare
/// `jsonschema::validator_for(schema)` at the runtime validation seams
/// (`validate_schema`, `validate_outputs_against_snippet`,
/// `validate_blackboard_writes`): a schema with no `praxec://hop` `$ref`
/// compiles and behaves exactly as before — the registry only adds the ability
/// to resolve the alias, it changes nothing for refs that were already
/// self-contained. Draft autodetection (`$schema`) is unchanged from
/// `validator_for` (both defer to the same option defaults).
pub(crate) fn compile_validator(
    schema: &serde_json::Value,
) -> Result<jsonschema::Validator, jsonschema::ValidationError<'static>> {
    jsonschema::options()
        .with_registry(&HOP_REGISTRY)
        .build(schema)
}

/// Public, registry-aware "validate this value against this schema" entry.
///
/// Executors (a different crate than the runtime seams) need the *same*
/// registry-aware validation the runtime uses so a `praxec://hop` `$ref`
/// resolves — e.g. the `parallel` executor's map-boundary per-item input check
/// (Spec A §7.1). Mirrors `runtime_schema::validate_schema` but returns a plain
/// `String` error (joining every violation) so callers can wrap it in whatever
/// `ExecutorError` variant fits. Strictly widening: a self-contained schema
/// behaves exactly as bare `jsonschema::validator_for`.
pub fn validate_against_schema(
    schema: &serde_json::Value,
    value: &serde_json::Value,
    label: &str,
) -> Result<(), String> {
    let validator =
        compile_validator(schema).map_err(|e| format!("invalid {label} schema: {e}"))?;
    if validator.is_valid(value) {
        return Ok(());
    }
    let errs: Vec<String> = validator
        .iter_errors(value)
        .map(|e| e.to_string())
        .collect();
    Err(format!("{label}: {}", errs.join("; ")))
}

/// The parsed shipped HOP vocabulary, for structural lookups (e.g. a slot
/// `In` contract's `required` field list). Parsed once; shares the same
/// fail-at-boot invariant as [`HOP_REGISTRY`].
static HOP_SCHEMA_JSON: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::from_str(HOP_SCHEMA).expect("invariant: shipped hop.schema.json parses as JSON")
});

/// The `required` field names of a slot's `In` contract (`<base>In`), read from
/// the shipped vocabulary — e.g. `slot_in_required("verify")` → `["cwd"]`.
///
/// Used by the `hop_slot:` resolver to forward the parent transition's
/// required arguments into the resolved cap's `use.inputs` (the actor validated
/// them against `<base>In`, so a required field is always present at runtime).
/// `base` is the camelCase `$defs` base from `config::hop_def_base`.
pub(crate) fn slot_in_required(base: &str) -> Vec<String> {
    HOP_SCHEMA_JSON
        .pointer(&format!("/$defs/{base}In/required"))
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Parse a config-authored HOP `$ref` (`praxec://hop#/$defs/<def>`) into its
/// `<def>` name (e.g. `verifyOut`). Returns `None` for any other string — a
/// self-contained inline schema, a foreign `$ref`, etc.
pub fn hop_ref_def(ref_uri: &str) -> Option<&str> {
    ref_uri.strip_prefix("praxec://hop#/$defs/")
}

/// The declared `properties` field names of a HOP `$defs` entry (e.g.
/// `hop_def_properties("verifyOut")` → the keys of `verifyOut.properties`).
/// `None` when the def is unknown or declares no `properties` object.
///
/// Backs the Spec A §7.1 fan-in composition check: to prove "the reduce
/// consumes what the map produces" the load-time checker needs the worker
/// `<slot>Out` field set, which for a `$ref`-declared contract lives in the
/// shipped vocabulary rather than inline on the transition.
pub fn hop_def_properties(def: &str) -> Option<Vec<String>> {
    HOP_SCHEMA_JSON
        .pointer(&format!("/$defs/{def}/properties"))
        .and_then(serde_json::Value::as_object)
        .map(|o| o.keys().cloned().collect())
}

/// The declared `required` field names of a HOP `$defs` entry. `None` when the
/// def is unknown or declares no `required` array.
pub fn hop_def_required(def: &str) -> Option<Vec<String>> {
    HOP_SCHEMA_JSON
        .pointer(&format!("/$defs/{def}/required"))
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn slot_in_required_reads_the_vocabulary() {
        assert_eq!(slot_in_required("verify"), vec!["cwd".to_string()]);
        // detectIn requires cwd + ruleset (order per the schema).
        assert_eq!(
            slot_in_required("detect"),
            vec!["cwd".to_string(), "ruleset".to_string()]
        );
        // Unknown base → empty (never panics).
        assert!(slot_in_required("nope").is_empty());
    }

    #[test]
    fn hop_ref_def_parses_only_the_hop_alias() {
        assert_eq!(
            hop_ref_def("praxec://hop#/$defs/verifyOut"),
            Some("verifyOut")
        );
        assert_eq!(
            hop_ref_def("praxec://hop#/$defs/detectIn"),
            Some("detectIn")
        );
        // A foreign / inline ref is not a HOP alias.
        assert_eq!(hop_ref_def("#/definitions/Foo"), None);
        assert_eq!(hop_ref_def("verifyOut"), None);
    }

    #[test]
    fn hop_def_properties_and_required_read_the_vocabulary() {
        // verifyOut declares `status` among its properties and requires it.
        let props = hop_def_properties("verifyOut").expect("verifyOut has properties");
        assert!(props.contains(&"status".to_string()), "got: {props:?}");
        let req = hop_def_required("verifyOut").expect("verifyOut has required");
        assert!(req.contains(&"status".to_string()), "got: {req:?}");
        // Unknown def → None (never panics).
        assert!(hop_def_properties("nope").is_none());
        assert!(hop_def_required("nope").is_none());
    }

    #[test]
    fn validate_against_schema_resolves_hop_ref() {
        // A verifyIn contract requires `cwd`; a value missing it must fail via
        // the registry, and a conforming value must pass.
        let schema = json!({ "$ref": "praxec://hop#/$defs/verifyIn" });
        assert!(validate_against_schema(&schema, &json!({ "cwd": "/tmp" }), "x").is_ok());
        let err = validate_against_schema(&schema, &json!({ "nope": 1 }), "item")
            .expect_err("missing cwd must fail the verifyIn contract");
        assert!(
            err.starts_with("item:"),
            "label prefix expected, got: {err}"
        );
    }

    /// A minimal schema that `$ref`s a slot-out through the alias URI, resolved
    /// against the shipped vocabulary via the registry.
    fn verify_out_validator() -> jsonschema::Validator {
        let schema = json!({ "$ref": "praxec://hop#/$defs/verifyOut" });
        jsonschema::options()
            .with_registry(&HOP_REGISTRY)
            .build(&schema)
            .expect("verifyOut $ref resolves through the HOP registry")
    }

    #[test]
    fn hop_registry_prepares() {
        // Forcing must not panic — the shipped bytes parse and prepare.
        force_init();
        // And a canonical $ref into it compiles.
        let _ = verify_out_validator();
    }

    #[test]
    fn valid_verify_out_instance_validates() {
        let validator = verify_out_validator();
        let instance = json!({
            "status": "pass",
            "summary": "all acceptance criteria met",
            "criteria": [
                { "id": "c1", "met": true, "evidence": "cargo test green" }
            ],
            "findings": [],
            "provenance": {
                "stack": "language:rust",
                "source": "pack",
                "chain": ["language:rust", "generic"]
            }
        });
        assert!(
            validator.is_valid(&instance),
            "a well-formed verifyOut must validate; errors: {:?}",
            validator.iter_errors(&instance).collect::<Vec<_>>()
        );
    }

    #[test]
    fn bad_status_enum_is_rejected() {
        let validator = verify_out_validator();
        // `status: "green"` is not a member of gateStatus (pass|fail|not_evaluated).
        let instance = json!({
            "status": "green",
            "summary": "s",
            "criteria": [],
            "provenance": { "stack": "generic", "source": "generic" }
        });
        assert!(
            !validator.is_valid(&instance),
            "an out-of-enum status must be rejected via the resolved gateStatus $ref"
        );
    }

    #[test]
    fn compile_validator_is_strictly_widening_for_plain_schemas() {
        // A self-contained schema (no praxec://hop $ref) must behave IDENTICALLY
        // under the registry-aware helper as under bare `validator_for`, for both
        // matching and non-matching instances. This is the guarantee that swapping
        // the three runtime seams changed nothing for existing configs.
        let plain = json!({
            "type": "object",
            "properties": { "n": { "type": "integer", "minimum": 0 } },
            "required": ["n"],
            "additionalProperties": false
        });
        let bare = jsonschema::validator_for(&plain).expect("bare compiles");
        let widened = compile_validator(&plain).expect("registry-aware compiles");

        for instance in [
            json!({ "n": 5 }),
            json!({ "n": -1 }),
            json!({ "n": "x" }),
            json!({}),
            json!({ "n": 5, "extra": true }),
            json!("not-an-object"),
        ] {
            assert_eq!(
                bare.is_valid(&instance),
                widened.is_valid(&instance),
                "registry-aware validation diverged from bare for {instance}"
            );
        }
    }

    #[test]
    fn finding_fix_missing_schema_ref_is_rejected() {
        let validator = verify_out_validator();
        // finding.fix is a schemaBound requiring `schema_ref` + `value`; omit
        // schema_ref to prove the nested $ref chain (finding -> fix -> schemaBound)
        // resolves and enforces.
        let instance = json!({
            "status": "fail",
            "summary": "one finding",
            "criteria": [],
            "findings": [
                {
                    "file": "src/lib.rs",
                    "line": 10,
                    "rule_id": "r1",
                    "severity": "error",
                    "message": "bad",
                    "fix": { "value": { "kind": "manual" } }
                }
            ],
            "provenance": { "stack": "generic", "source": "generic" }
        });
        assert!(
            !validator.is_valid(&instance),
            "a finding.fix missing schema_ref must be rejected via the schemaBound $ref"
        );
    }
}
