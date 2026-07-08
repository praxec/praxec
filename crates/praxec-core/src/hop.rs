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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
