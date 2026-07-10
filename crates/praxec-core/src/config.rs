//! Config preprocessor.
//!
//! Two stages, both pure on `serde_json::Value`:
//!
//! 1. `merge_includes` — walk top-level `include: [paths…]`, load and
//!    deep-merge every referenced YAML file into the config. Maps merge
//!    (later wins on collisions), arrays concatenate. Cycles raise an error.
//! 2. `resolve` — flatten everything compositional into the inline shapes
//!    the runtime understands:
//!    - Capability `wraps:` chains become single normalized capabilities.
//!    - `executor: { capability: foo }` references become inline
//!      `executor: { kind: ..., ... }` configs; the capability's guards and
//!      reliability stack into the calling context.
//!    - `proxy.expose: [{ capability: foo, as: bar, ... }]` references
//!      become inline `{ name: bar, executor: ..., ... }` exposures.
//!
//! After preprocessing, the rest of the system (DefinitionStore,
//! discovery indexer, proxy compiler) sees only the original inline shapes —
//! they don't need to know about capabilities, wraps, or includes.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::discovery::{BLESSED_SCRIPT_ROOTS, BLESSED_SUBJECT_ROOTS, Lifecycle, ScriptVerb, Verb};

/// Recursively load `path` as YAML and merge any `include:` files into it.
/// Includes resolve relative to the file that lists them.
pub fn load_yaml(path: impl AsRef<Path>) -> anyhow::Result<Value> {
    let mut visited = HashSet::new();
    load_yaml_inner(path.as_ref(), &mut visited)
}

fn load_yaml_inner(path: &Path, visited: &mut HashSet<PathBuf>) -> anyhow::Result<Value> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("resolving config path {}", path.display()))?;
    if !visited.insert(canonical.clone()) {
        bail!("config include cycle detected at {}", canonical.display());
    }

    let text = std::fs::read_to_string(&canonical)
        .with_context(|| format!("reading config {}", canonical.display()))?;
    let mut value: Value = serde_yaml::from_str(&text)
        .with_context(|| format!("parsing YAML {}", canonical.display()))?;

    // A canonicalized config-file path always has a parent directory. The
    // only way `parent()` is None is a degenerate root path (e.g. `/`), which
    // is not a readable config file. Fail explicitly rather than silently
    // switching the include base to CWD (`.`) — that would resolve relative
    // includes against the gateway's working directory instead of the config
    // file's own directory, a silent and surprising base swap.
    let parent = canonical.parent().ok_or_else(|| {
        anyhow!(
            "config path {} has no parent directory (degenerate root path)",
            canonical.display()
        )
    })?;

    // SPEC §22.2 — rewrite any `file://` URIs in `scripts:` entries to
    // absolute paths now, while we still know the config file's directory.
    // `resolve()` is path-agnostic by design; doing this here keeps the
    // pure-value abstraction downstream and gives sensible relative-path
    // semantics (file:// URIs resolve relative to the YAML they were
    // declared in, not the gateway's CWD).
    rewrite_script_uris_to_absolute(&mut value, parent);

    if let Some(includes) = value.get("include").and_then(Value::as_array).cloned() {
        // Each include is loaded in declaration order, then the current file's
        // body overrides on top. (Includes are "defaults" that the explicit
        // file can refine.) Final order of merging: includes[0], includes[1],
        // ..., main body (last wins).
        //
        // Each entry is either:
        //   - A plain string path (relative to this file) — original behaviour.
        //   - An object `{ uri: "file://..." | "https://...", hash?: "sha256:..." }` —
        //     new remote/verified form (SPEC §22.2).
        let mut merged = Value::Object(Map::new());
        for inc in &includes {
            let inc_value = match inc {
                Value::String(inc_path) => {
                    let inc_full = parent.join(inc_path);
                    load_yaml_inner(&inc_full, visited)?
                }
                Value::Object(_) => load_include_entry(inc, parent, visited)?,
                other => bail!(
                    "include entries must be a path string or {{uri, hash}} object, got {other:?}"
                ),
            };
            merged = deep_merge(merged, inc_value);
        }
        // Drop the `include` key from the local body before merging — it's
        // already been processed.
        if let Some(obj) = value.as_object_mut() {
            let _: Option<Value> = obj.remove("include");
        }
        merged = deep_merge(merged, value);
        return Ok(merged);
    }

    Ok(value)
}

/// Load one object-form `include:` entry: `{ uri: string, hash?: string }`.
/// Non-`file://` URIs REQUIRE `hash` (reproducibility + tamper-evidence).
/// Reuses the `scripts:` fetcher (`read_script_uri`) and raw-bytes sha256.
fn load_include_entry(
    inc: &Value,
    parent: &Path,
    visited: &mut HashSet<PathBuf>,
) -> anyhow::Result<Value> {
    let uri = inc
        .get("uri")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("object include entry requires a string `uri`"))?;

    // FIX 2: Explicit scheme whitelist — must be checked FIRST, before hash
    // validation, so the error is include-specific and fires regardless of hash.
    if !(uri.starts_with("file://")
        || uri.starts_with("https://")
        || uri.starts_with("git+https://"))
    {
        let scheme = uri.split("://").next().unwrap_or(uri);
        bail!(
            "UNSUPPORTED_INCLUDE_URI_SCHEME: include uri '{uri}' uses scheme '{scheme}://' — \
             supported schemes are file://, https://, git+https://."
        );
    }

    // Resolve a relative file:// URI against the including file's directory.
    let resolved_uri = if let Some(rel) = uri.strip_prefix("file://") {
        let p = Path::new(rel);
        if p.is_absolute() {
            uri.to_string()
        } else {
            format!("file://{}", parent.join(rel).display())
        }
    } else {
        uri.to_string()
    };

    let is_file = resolved_uri.starts_with("file://");
    let hash = inc.get("hash").and_then(Value::as_str);
    if !is_file && hash.is_none() {
        bail!(
            "include entry uri '{uri}' is remote (non-file://) and has no `hash`. \
             A sha256 is required so the merged config is reproducible and \
             tamper-evident (mirrors `scripts:` SPEC §22.2)."
        );
    }
    if let Some(h) = hash {
        validate_hash_format(h, &format!("include:{uri}"))?;
    }

    // FIX 3: Structural check for git+https:// URIs — shared logic with scripts validation.
    if resolved_uri.starts_with("git+https://") {
        validate_git_https_uri_shape(&resolved_uri, &format!("include:{uri}"))?;
    }

    // FIX 1: Cycle detection key — use the canonicalized filesystem path for
    // file:// URIs so that the same file included once as a plain string path
    // (keyed by canonical PathBuf in load_yaml_inner) and once as a file://
    // object entry is detected as a cycle rather than loaded twice.
    let visited_key = if let Some(path) = resolved_uri.strip_prefix("file://") {
        Path::new(path)
            .canonicalize()
            .with_context(|| format!("resolving include path for {resolved_uri}"))?
    } else {
        PathBuf::from(&resolved_uri)
    };
    if !visited.insert(visited_key) {
        bail!("config include cycle detected at {resolved_uri}");
    }

    let body = read_script_uri(&resolved_uri, &format!("include:{uri}"))?;
    if let Some(expected) = hash {
        let actual = raw_content_sha256(&body);
        if actual != expected {
            bail!(
                "INCLUDE_HASH_MISMATCH: include '{uri}' expected {expected} but \
                 fetched body hashes to {actual}. Refusing to merge unverified config."
            );
        }
    }
    serde_yaml::from_str(&body).with_context(|| format!("parsing included YAML from {uri}"))
}

/// P6b — best-effort enumeration of the LOCAL files that make up a config:
/// the top-level file plus every recursively-included local file (plain
/// string `include:` entries and `file://` object entries, resolved relative
/// to the file that lists them — the same base rule as `load_yaml_inner`).
///
/// Scope: local `include:` files AND every declared `repos:` working tree's
/// definition YAML are enumerated, so an edit to a pack file triggers the same
/// gated reload as a config edit (v0.0.17 dogfood Finding 6 — a repo edit was
/// previously invisible on a running gateway until an explicit reload / SIGHUP).
/// Remote includes (`https://` / `git+https://`) are NOT enumerated. Unreadable
/// or unparsable files are skipped rather than erroring: this feeds the lazy
/// staleness probe on a LIVE server, which must degrade to "track less" — never
/// fail a request.
pub fn local_config_file_set(path: impl AsRef<Path>) -> Vec<PathBuf> {
    let mut visited = HashSet::new();
    let mut out = Vec::new();
    collect_local_config_files(path.as_ref(), &mut visited, &mut out);
    out
}

fn collect_local_config_files(path: &Path, visited: &mut HashSet<PathBuf>, out: &mut Vec<PathBuf>) {
    let Ok(canonical) = path.canonicalize() else {
        return;
    };
    if !visited.insert(canonical.clone()) {
        return; // cycle / duplicate — already tracked
    }
    let Ok(text) = std::fs::read_to_string(&canonical) else {
        return;
    };
    out.push(canonical.clone());
    let Ok(value) = serde_yaml::from_str::<Value>(&text) else {
        return; // unparsable mid-edit: still track the file itself
    };
    let Some(parent) = canonical.parent() else {
        return;
    };
    if let Some(includes) = value.get("include").and_then(Value::as_array) {
        for inc in includes {
            match inc {
                Value::String(rel) => {
                    collect_local_config_files(&parent.join(rel), visited, out);
                }
                Value::Object(_) => {
                    // Mirror load_include_entry's file:// resolution; skip remote.
                    let Some(rel) = inc
                        .get("uri")
                        .and_then(Value::as_str)
                        .and_then(|uri| uri.strip_prefix("file://"))
                    else {
                        continue;
                    };
                    let p = Path::new(rel);
                    let full = if p.is_absolute() {
                        p.to_path_buf()
                    } else {
                        parent.join(rel)
                    };
                    collect_local_config_files(&full, visited, out);
                }
                _ => {}
            }
        }
    }

    // Track every declared repo's definition YAML so a pack edit triggers the
    // same gated reload as a config edit (Finding 6). Best-effort: an unreadable
    // repo just contributes nothing.
    if let Some(repos) = value.get("repos").and_then(Value::as_array) {
        for repo in repos {
            let Some(rel) = repo.get("path").and_then(Value::as_str) else {
                continue;
            };
            let p = Path::new(rel);
            let repo_path = if p.is_absolute() {
                p.to_path_buf()
            } else {
                parent.join(rel)
            };
            for f in crate::repo::definition_files(&repo_path) {
                if let Ok(canonical) = f.canonicalize() {
                    if visited.insert(canonical.clone()) {
                        out.push(canonical);
                    }
                }
            }
        }
    }
}

/// Deep-merge `b` into `a`. Maps merge recursively (b wins on key collisions).
/// Arrays concatenate (`a` first, then `b`). Scalars: `b` wins.
pub fn deep_merge(a: Value, b: Value) -> Value {
    match (a, b) {
        (Value::Object(mut am), Value::Object(bm)) => {
            for (k, v) in bm {
                let merged = match am.remove(&k) {
                    Some(existing) => deep_merge(existing, v),
                    None => v,
                };
                am.insert(k, merged);
            }
            Value::Object(am)
        }
        (Value::Array(mut aa), Value::Array(ab)) => {
            aa.extend(ab);
            Value::Array(aa)
        }
        (_, b) => b,
    }
}

/// SPEC §5.4.2 / audit-resolution C.2 — a single diagnostic produced by
/// `resolve_with_diagnostics`. Severity is `warn` for soft issues (e.g.
/// non-strict-mode unblessed subject roots) and `error` for hard issues
/// (which `resolve` itself returns via `Err`, so they don't appear here
/// except where surfacing a structured form is useful).
#[derive(Debug, Clone, serde::Serialize)]
pub struct Diagnostic {
    pub severity: DiagnosticSeverity,
    pub code: String,
    pub message: String,
    /// JSON-Pointer style path to the offending location, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// Free-form remediation hint (e.g. closest blessed root suggestion).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticSeverity {
    Warn,
    Error,
}

/// Resolve `capabilities:`, `wraps:`, and `capability:` references into the
/// inline shapes the runtime expects. Idempotent — calling it twice is safe.
///
/// Discards any soft diagnostics. Use `resolve_with_diagnostics` to capture
/// them (e.g. unblessed-subject-root warnings under
/// `strict_namespacing: false`).
pub fn resolve(value: Value) -> anyhow::Result<Value> {
    let (config, _diagnostics) = resolve_with_diagnostics(value)?;
    Ok(config)
}

/// SPEC §5.4.2 / audit-resolution C.2 — like `resolve` but also returns
/// any soft `Diagnostic`s collected during validation. Hard errors still
/// propagate via `Err`.
pub fn resolve_with_diagnostics(mut config: Value) -> anyhow::Result<(Value, Vec<Diagnostic>)> {
    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    // 1. Flatten the capabilities block into a registry of normalized defs.
    let registry = flatten_capabilities(&config)?;

    // 2. Rewrite `proxy.expose` entries that are capability refs into inline
    //    exposures. Inline entries pass through unchanged.
    if let Some(exposures) = config
        .pointer_mut("/proxy/expose")
        .and_then(Value::as_array_mut)
    {
        let rewritten: Vec<Value> = std::mem::take(exposures)
            .into_iter()
            .map(|ex| rewrite_exposure(ex, &registry))
            .collect::<anyhow::Result<Vec<_>>>()?;
        *exposures = rewritten;
    }

    // 3. Rewrite executors throughout `proxy.expose`, `workflows.*`, and any
    //    nested onEnter / transitions / fallback executors.
    rewrite_executors_in_value(&mut config, &registry)?;

    // 4. Strip the now-fully-resolved `capabilities` block — it's an authoring
    //    affordance, not runtime state.
    //    SPEC §30.10.3 — capture capability subjects BEFORE stripping so
    //    `inject_pending_definitions` can see them even though the block will
    //    no longer be present in the config when that function runs.
    //    Only keys that follow the `verb.subject` pattern (contain a `.`) are
    //    lexicon subjects; simple names like `do_thing` are capability names,
    //    not subject references, and are skipped.
    let capability_subjects: Vec<String> = config
        .pointer("/capabilities")
        .and_then(Value::as_object)
        .map(|caps| {
            caps.keys()
                .filter(|k| k.contains('.'))
                .map(|k| crate::lexicon::subject_portion_pub(k))
                .collect()
        })
        .unwrap_or_default();
    if let Some(obj) = config.as_object_mut() {
        let _: Option<Value> = obj.remove("capabilities");
    }

    // 5. Apply the per-workflow-definition version default ("0") to any
    //    workflow definition that does not carry an explicit `version`.
    //    This ensures downstream code (runtime, stores) always sees a version
    //    on every workflow definition.
    //    Also synthesize `/inputSchema` from the `inputs:` convenience block
    //    so start-time defaults and type validation apply (SPEC §8.1).
    if let Some(workflows) = config
        .pointer_mut("/workflows")
        .and_then(Value::as_object_mut)
    {
        for def in workflows.values_mut() {
            if let Some(obj) = def.as_object_mut() {
                obj.entry("version")
                    .or_insert_with(|| Value::String("0".to_string()));
                synthesize_input_schema(obj); // NEW: workflow inputs: -> /inputSchema
            }
        }
    }

    // 5b. Spec A / A.1 §3 — the `hop_slot:` primitive. For every transition that
    //     declares `hop_slot: <name>`, inject the canonical `In` contract as the
    //     transition `inputSchema` and the canonical `Out` contract as the
    //     `$.context.<name>` typed blackboard slot, both by `$ref` into the
    //     shipped HOP vocabulary (praxec://hop). The existing seams then enforce
    //     both with no new runtime code: input via `validate_schema`
    //     (runtime_submit.rs), output via `validate_blackboard_writes`
    //     (runtime_records.rs). An unknown slot name is a hard load error.
    inject_hop_slots(&mut config)?;

    // 6. Poka-yoke on `skills:` (SPEC §5.4). `verb` and the `skills:` keys
    //    must match `^[a-z][a-z0-9-]*$` — lowercase kebab, no whitespace.
    //    Enforced at config load so malformed descriptors are unrepresentable
    //    rather than only linted.
    validate_skills(&config, &mut diagnostics)?;

    // 6a-bis. SPEC §22 — `scripts:` block validates next to `skills:`. Same
    //         strict-vs-lenient blessed-root semantics. Distinct verb enum
    //         (action verbs vs cognitive verbs) and stricter hash
    //         normalization (whitespace is load-bearing in shell).
    validate_scripts(&config, &mut diagnostics)?;

    // 6b. SPEC §8.4 + §20.2 — reject runtime-only `praxec.*` flags when
    //     they appear inside any `workflows:` block. The flags are read at
    //     gateway startup only; allowing them at workflow scope would let
    //     an LLM-authored workflow attempt to (silently) flip the bypass
    //     flag on for itself.
    validate_workflow_flag_scope(&config)?;

    // 6c. SPEC §21 — `delegate` is a pass-through string. It MUST be
    //     a non-empty string when present. Validating shape here means the
    //     runtime never has to defend against `delegate: ""` or `delegate: 42`
    //     reaching the response surface.
    validate_state_delegate(&config)?;

    // 6d. SPEC §17.x (v0.3) — `praxec.authoring.*` preferences are
    //     advisory strings surfaced to LLM-driven authoring workflows via
    //     template substitution. Shape-validated here; nothing rejects a
    //     workflow for ignoring the preference.
    validate_authoring_preferences(&config)?;

    // 6e. ADR-0007 — a workflow may declare an `orchestrator` (the agent/model
    //     that drives it). Shape-validated here: a non-empty string ref (a model
    //     name or an agent name), so the launch path never has to defend against
    //     `orchestrator: ""` / `orchestrator: 42`.
    validate_orchestrator(&config)?;

    // 7. Stamp each workflow definition with `_skillsLibrary: { subject: verb }`
    //    drawn from the top-level `skills:` map (subjects only — verb, no body;
    //    body is fetched on demand via `gateway.describe`). Lets the runtime
    //    decorate `guidance.refs` from the per-instance snapshot alone without
    //    needing a side channel to the top-level config.
    stamp_skills_library(&mut config);

    // 7-bis. SPEC §22 — stamp `_scriptsLibrary` onto each workflow that
    //        references a curated script. Resolves file:// URIs and verifies
    //        hashes; `SCRIPT_HASH_MISMATCH` here means the external script
    //        body drifted since the workflow was authored.
    stamp_scripts_library(&mut config)?;

    // 7-ter. SPEC §17.x (v0.3) — stamp `_authoringPrefs` onto every workflow
    //        snapshot so authoring skills can reach the operator's
    //        preferences via template substitution `{{$.praxec.authoring.*}}`.
    stamp_authoring_preferences(&mut config);

    // 7-quater. SPEC §29 — when a workflow declares `enable_human_ask: true`,
    //           inject a self-loop `ask_human` transition into every
    //           non-terminal state. Lets the agent ask mid-reasoning
    //           clarifying questions without per-state authoring burden.
    inject_human_ask_transitions(&mut config);

    // 7-quinquies-pre. SPEC §30.5 durability — merge any lexicon terms that a
    //              prior run persisted to disk into the authored `lexicon:`
    //              block BEFORE validate/stamp/pending-detection, so a term
    //              defined in an earlier session is treated as defined (not
    //              re-flagged PENDING_DEFINITION) after a restart.
    merge_persisted_lexicon(&mut config);

    // 7-quinquies. SPEC §30 — validate + stamp the lexicon library.
    //              Every workflow gets a `_lexiconLibrary` snapshot
    //              so in-flight reads are deterministic (same
    //              invariant as `_skillsLibrary` / `_scriptsLibrary`).
    crate::lexicon::validate_lexicon(&config)?;
    crate::lexicon::stamp_lexicon_library(&mut config);

    // 7-sexties-bis. SPEC §30.10.3 — inject PENDING_DEFINITION placeholders
    //               for any subject referenced in scripts/skills/executors that
    //               lacks an authored lexicon entry. Placeholders accumulate in
    //               the stamped _lexiconLibrary so doctor and (Task 3.3) the
    //               runtime can surface unresolved subjects without hard-failing
    //               the load.
    //               `capability_subjects` carries subjects harvested from the
    //               `capabilities:` block at step 4 (before it was stripped).
    //               Passing them here closes the pipeline-ordering gap so
    //               capability-block subjects are detected as pending (SPEC §30.10.3).
    // TODO(SPEC §30.10.3): inherit bounded_context from the referencing
    // config. Currently defaults to global; sufficient for v0.5 since
    // Tier-1 lexicons are typically single-context.
    crate::lexicon::inject_pending_definitions(&mut config, &capability_subjects);

    // 7-sexies. SPEC §6 — for every transition whose executor is
    //           `kind: workflow` with a `use:` block, synthesize the
    //           transition-level `output:` mapping from `use.outputs`
    //           and embed the target capability's `snippet.outputs`
    //           schema as `_snippetOutputs` on the executor config.
    //           After this pass, the runtime's existing merge_output
    //           projection drives cap-output writes; the executor needs
    //           no schema lookup at run time.
    expand_use_bindings(&mut config)?;

    // 7-septies. Spec A.1 §7 (FM-7) — slot-named context keys are engine-owned.
    //            After `use:` expansion has normalized every projection into the
    //            transition `output:` mapping, reject any *non*-`hop_slot`
    //            transition that writes `$.context.<slot>` (an unvalidated write
    //            to a typed, engine-owned slot). `hop_slot:` transitions are
    //            exempt — the engine owns their slot write by construction.
    validate_slot_key_ownership(&config)?;

    // 7-octies. Spec A.1 §4.2/§4.4 — SchemaBound L2 registry (`finding.fix`).
    //           (1) every top-level `schemas:` entry must compile; (2) every
    //           statically-referenced `schema_ref` must resolve (closed-world,
    //           mirrors `validate_workflow_refs_resolve`); (3) stamp the merged
    //           registry onto every workflow snapshot as `_schemasRegistry` so
    //           the blackboard-write seam can validate a `finding.fix` inner
    //           `value` at runtime without a side channel. The closed-world walk
    //           runs BEFORE the stamp so it never sees the registry's own
    //           schema-property definitions.
    validate_schemas_registry(&config)?;
    validate_schema_refs_resolve(&config)?;
    stamp_schemas_registry(&mut config);

    Ok((config, diagnostics))
}

/// Spec A.1 §4.1 step 2 — every top-level `schemas:` entry must be a compilable
/// JSON Schema (registry-aware, so an inner `$ref praxec://hop#/…` resolves). A
/// bad pack schema fails at load, not mid-run.
fn validate_schemas_registry(config: &Value) -> anyhow::Result<()> {
    let Some(schemas) = config.pointer("/schemas").and_then(Value::as_object) else {
        return Ok(());
    };
    for (name, schema) in schemas {
        if let Err(e) = crate::hop::compile_validator(schema) {
            bail!(
                "SCHEMA_INVALID: `schemas:` entry '{name}' is not a valid JSON Schema: {e}. \
                 A registered SchemaBound inner schema must compile at load."
            );
        }
    }
    Ok(())
}

/// Spec A.1 §4.1 step 4 — closed-world `schema_ref` check (mirrors
/// [`validate_workflow_refs_resolve`]). Every `schema_ref` literal statically
/// present in a workflow body must name a registered top-level `schemas:` entry.
/// Unresolved → `SCHEMA_REF_UNRESOLVED` (the V22 fix-it voice).
fn validate_schema_refs_resolve(config: &Value) -> anyhow::Result<()> {
    let registered: HashSet<String> = config
        .pointer("/schemas")
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    let mut unresolved: Vec<(String, String)> = Vec::new();
    if let Some(workflows) = config.pointer("/workflows").and_then(Value::as_object) {
        for (wf_id, wf_def) in workflows {
            collect_unresolved_schema_refs(wf_def, &registered, wf_id, &mut unresolved);
        }
    }
    if let Some((wf_id, sref)) = unresolved.first() {
        bail!(
            "SCHEMA_REF_UNRESOLVED: workflow '{wf_id}' statically references schema_ref '{sref}', \
             but no `schemas:` entry registers it. Register the inner schema or fully qualify the \
             ref as `<namespace>/<name>` (Spec A.1 §4.2)."
        );
    }
    Ok(())
}

/// Walk a workflow body for `{ schema_ref: "<string>", … }` SchemaBound literals
/// and record any whose ref is not registered. Engine-internal stamps
/// (`_`-prefixed keys — e.g. `_schemasRegistry`, `_snippetOutputs`) are skipped:
/// a registered inner schema may itself declare a property *named* `schema_ref`
/// (whose value is a schema object, not a ref string), and must not be mistaken
/// for a reference.
fn collect_unresolved_schema_refs(
    value: &Value,
    registered: &HashSet<String>,
    wf_id: &str,
    out: &mut Vec<(String, String)>,
) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(sref)) = map.get("schema_ref") {
                if !registered.contains(sref) {
                    out.push((wf_id.to_string(), sref.clone()));
                }
            }
            for (k, child) in map {
                if k.starts_with('_') {
                    continue;
                }
                collect_unresolved_schema_refs(child, registered, wf_id, out);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                collect_unresolved_schema_refs(v, registered, wf_id, out);
            }
        }
        _ => {}
    }
}

/// Spec A.1 §4.1 step 6 — stamp the merged `schemas:` registry onto every
/// workflow snapshot as `_schemasRegistry` (internal metadata; mirrors
/// `_lexiconLibrary` / `_snippetOutputs`). The runtime blackboard-write seam
/// reads it to validate `finding.fix` inner values without a side channel.
/// No-op when no schemas are registered.
fn stamp_schemas_registry(config: &mut Value) {
    let Some(schemas) = config
        .pointer("/schemas")
        .and_then(Value::as_object)
        .filter(|m| !m.is_empty())
        .cloned()
    else {
        return;
    };
    let registry = Value::Object(schemas);
    if let Some(workflows) = config
        .pointer_mut("/workflows")
        .and_then(Value::as_object_mut)
    {
        for def in workflows.values_mut() {
            if let Some(obj) = def.as_object_mut() {
                obj.insert("_schemasRegistry".into(), registry.clone());
            }
        }
    }
}

/// Spec A.1 §7 (FM-7) — the poka-yoke that keeps slot-named context keys
/// engine-owned.
///
/// The five [`HOP_SLOT_NAMES`] name typed blackboard slots whose `Out` contract
/// only a `hop_slot:`-declared transition may produce (the engine injects the
/// contract + wires the resolved cap). A non-`hop_slot` transition that writes
/// `$.context.<slot>` — through an `output:` mapping key (which, post
/// [`expand_use_bindings`], is the context-key tail) or a `kind: workflow`
/// `use.outputs` LHS — is the FM-7/FM-13 hole: config surfaces an *unvalidated*
/// write to a slot-named key. This is a hard load error.
///
/// A transition carrying a `hop_slot:` marker is exempt: its `$.context.<slot>`
/// write is exactly the engine-owned production this lint protects.
fn validate_slot_key_ownership(config: &Value) -> anyhow::Result<()> {
    let Some(workflows) = config.pointer("/workflows").and_then(Value::as_object) else {
        return Ok(());
    };
    for (wf_id, def) in workflows {
        let Some(states) = def.pointer("/states").and_then(Value::as_object) else {
            continue;
        };
        for (state_name, state) in states {
            let Some(transitions) = state.pointer("/transitions").and_then(Value::as_object) else {
                continue;
            };
            for (t_name, t) in transitions {
                let Some(t_obj) = t.as_object() else {
                    continue;
                };
                // Exempt: a hop_slot transition legitimately owns its slot write.
                if t_obj.contains_key("hop_slot") {
                    continue;
                }
                // (a) `output:` mapping keys are context-key tails after expansion.
                if let Some(output) = t_obj.get("output").and_then(Value::as_object) {
                    for key in output.keys() {
                        if HOP_SLOT_NAMES.contains(&key.as_str()) {
                            // Exempt: the resolved slot cap is the sanctioned typed
                            // producer. A workflow that declares `snippet.outputs.<slot>`
                            // as the canonical `<slot>Out` contract IS the cap a
                            // `hop_slot: <slot>` flow resolves to; its `output.<slot>`
                            // write is runtime-validated against that same contract by
                            // `validate_outputs_against_snippet`, so it is not the
                            // unvalidated forge this lint guards against.
                            if declares_hop_typed_slot_output(def, key) {
                                continue;
                            }
                            bail!(
                                "SLOT_KEY_ENGINE_OWNED: workflow '{wf_id}' state '{state_name}' \
                                 transition '{t_name}': `output:` writes the engine-owned slot key \
                                 '$.context.{key}', but the transition is not `hop_slot:`-declared. \
                                 Slot keys [{}] carry an engine-injected typed contract — only a \
                                 `hop_slot: {key}` transition (in a flow), or the resolved slot cap \
                                 declaring `snippet.outputs.{key}: {{ $ref: praxec://hop#/$defs/…Out }}`, \
                                 may produce one. Declare `hop_slot: {key}`, add that typed \
                                 `snippet.outputs.{key}`, or write a non-slot context key.",
                                HOP_SLOT_NAMES.join(", ")
                            );
                        }
                    }
                }
                // (b) A `kind: workflow` `use.outputs` LHS (`$.context.<slot>`).
                if let Some(use_outputs) = t
                    .pointer("/executor/use/outputs")
                    .and_then(Value::as_object)
                {
                    for host_path in use_outputs.keys() {
                        if let Some(tail) = host_path_tail(host_path) {
                            if HOP_SLOT_NAMES.contains(&tail.as_str()) {
                                bail!(
                                    "SLOT_KEY_ENGINE_OWNED: workflow '{wf_id}' state \
                                     '{state_name}' transition '{t_name}': `use.outputs` projects \
                                     into the engine-owned slot key '$.context.{tail}', but the \
                                     transition is not `hop_slot:`-declared. Slot keys [{}] carry \
                                     an engine-injected typed contract — only a `hop_slot: {tail}` \
                                     transition may produce one.",
                                    HOP_SLOT_NAMES.join(", ")
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// SPEC: compile a workflow's `inputs:` block into a synthesized `/inputSchema`
/// so the start-time default + validate path (runtime_schema::apply_schema_defaults
/// and validate_schema, runtime.rs:502-503) applies. Each `inputs.<name>` becomes
/// `inputSchema.properties.<name>` (carrying its `type`/`default`/etc.), and a
/// per-input `required: true` lifts to the top-level JSON-Schema `required: []`.
/// No-op when the workflow already declares an explicit `inputSchema`, or has no
/// `inputs:` block.
fn synthesize_input_schema(def: &mut Map<String, Value>) {
    if def.contains_key("inputSchema") {
        return;
    }
    let Some(inputs) = def.get("inputs").and_then(Value::as_object) else {
        return;
    };
    let mut properties = Map::new();
    let mut required: Vec<Value> = Vec::new();
    for (name, spec) in inputs {
        let mut prop = spec.as_object().cloned().unwrap_or_default();
        // `required: true` is a per-input convenience; lift it to the schema-level
        // `required` array and drop it from the property (it's not a valid
        // per-property JSON Schema keyword for non-object types).
        if prop
            .remove("required")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            required.push(Value::String(name.clone()));
        }
        properties.insert(name.clone(), Value::Object(prop));
    }
    let mut schema = Map::new();
    schema.insert("type".into(), Value::String("object".into()));
    schema.insert("properties".into(), Value::Object(properties));
    if !required.is_empty() {
        schema.insert("required".into(), Value::Array(required));
    }
    def.insert("inputSchema".into(), Value::Object(schema));
}

/// The five canonical specialization-slot names (Spec A §3). A `hop_slot:`
/// marker MUST name one of these. Kept as the single closed set for the
/// load-time poka-yoke (the doctor rule) and the `$defs` base mapping.
pub const HOP_SLOT_NAMES: [&str; 5] = ["verify", "detect", "scaffold", "implement", "lint_format"];

/// Map a `hop_slot:` name to its camelCase `$defs` base in `hop.schema.json`
/// (`lint_format` → `lintFormat`); `<base>In` / `<base>Out` are the injected
/// contracts. `None` marks an unknown slot name — the doctor rule (§3).
///
/// Exhaustive match (poka-yoke): the closed set lives here, not as a
/// hand-maintained parallel string list.
fn hop_def_base(slot: &str) -> Option<&'static str> {
    match slot {
        "verify" => Some("verify"),
        "detect" => Some("detect"),
        "scaffold" => Some("scaffold"),
        "implement" => Some("implement"),
        "lint_format" => Some("lintFormat"),
        _ => None,
    }
}

/// A canonical `$ref` into the shipped HOP vocabulary for one slot def.
fn hop_ref(base: &str, dir: &str) -> Value {
    json!({ "$ref": format!("praxec://hop#/$defs/{base}{dir}") })
}

/// True when `def` declares `snippet.outputs.<slot>` as the canonical
/// `praxec://hop#/$defs/<base>Out` contract — i.e. this workflow IS the resolved
/// slot cap, the sanctioned typed producer of the slot value (Spec A §3.1). Such
/// a cap's `output.<slot>` write is runtime-validated against that same contract
/// by `validate_outputs_against_snippet`, so it is exempt from FM-7: it is a typed
/// production, not the unvalidated forge the lint guards against. The `$ref` must
/// match exactly — an untyped `snippet.outputs.<slot>` does NOT earn the exemption.
fn declares_hop_typed_slot_output(def: &Value, slot: &str) -> bool {
    let Some(base) = hop_def_base(slot) else {
        return false;
    };
    let expected = format!("praxec://hop#/$defs/{base}Out");
    def.pointer(&format!("/snippet/outputs/{slot}/$ref"))
        .and_then(Value::as_str)
        == Some(expected.as_str())
}

/// Spec A / A.1 §3 — the `hop_slot:` load-time injector.
///
/// For each transition declaring `hop_slot: <name>`:
///   (a) set the transition `inputSchema` to `{ "$ref": "…/<name>In" }` — only
///       if the transition does not already declare an explicit `inputSchema`
///       (the author may narrow, the engine supplies the default);
///   (b) declare `$.context.<name>` as a typed blackboard slot with schema
///       `{ "$ref": "…/<name>Out" }` on the workflow's `blackboard:` map — the
///       engine OWNS the output contract, so this overwrites any authored
///       schema for that key.
///
/// Enforcement is then entirely reuse: `runtime_submit`'s `validate_schema`
/// checks the input, `runtime_records`'s `validate_blackboard_writes` checks
/// the output (both now registry-aware so the `praxec://hop` refs resolve).
///
/// Precedents for this load-time move: [`synthesize_input_schema`] and
/// `expand_use_bindings` (both synthesize schema/mappings onto a transition at
/// load), and the V13 slot table (the typed blackboard the output check reads).
///
/// An unknown slot name, a non-string marker, or an array-form `blackboard:`
/// that collides with a slot is a hard load error (`bail!`).
fn inject_hop_slots(config: &mut Value) -> anyhow::Result<()> {
    // Snapshot the resolution inputs BEFORE taking the mutable `workflows`
    // borrow: the full set of loaded (namespace-prefixed) definition ids and
    // the repo `namespace → priority` map stamped by `merge_declared_repos`.
    let loaded_ids: Vec<String> = config
        .pointer("/workflows")
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    let repo_priority = read_repo_priority(config);

    let Some(workflows) = config
        .pointer_mut("/workflows")
        .and_then(Value::as_object_mut)
    else {
        return Ok(());
    };

    for (wf_id, def) in workflows.iter_mut() {
        let Some(def_obj) = def.as_object_mut() else {
            continue;
        };

        // The workflow's resolution chain (Spec A §5.1). `stack:` is either a
        // plain string (language-only, back-compat) or an object
        // `{ language, frameworks, primary_framework, project }`. Parsed into an
        // ordered, most-specific-first specificity chain
        // `[project, primary_framework, language]` (absent levels skipped);
        // `resolve_hop_cap` appends the `generic` floor.
        let stack_chain = parse_stack_chain(def_obj.get("stack"))?;

        // First pass: walk states→transitions, inject each transition's
        // `inputSchema` + resolved executor, and collect the slot→Out ref for
        // the blackboard. (Borrow split: mutate transitions here, mutate
        // `blackboard` after.)
        let mut out_slots: std::collections::BTreeMap<String, Value> =
            std::collections::BTreeMap::new();

        if let Some(states) = def_obj.get_mut("states").and_then(Value::as_object_mut) {
            for (state_name, state) in states.iter_mut() {
                let Some(transitions) = state.get_mut("transitions").and_then(Value::as_object_mut)
                else {
                    continue;
                };
                for (t_name, t) in transitions.iter_mut() {
                    let Some(t_obj) = t.as_object_mut() else {
                        continue;
                    };
                    let Some(marker) = t_obj.get("hop_slot") else {
                        continue;
                    };
                    // Own the slot name and end the immutable borrow of `t_obj`
                    // before mutating it below.
                    let slot = marker
                        .as_str()
                        .ok_or_else(|| {
                            anyhow!(
                                "HOP_SLOT_INVALID: workflow '{wf_id}' state '{state_name}' \
                                 transition '{t_name}': `hop_slot` must be a string naming a \
                                 slot; valid names: [{}]",
                                HOP_SLOT_NAMES.join(", ")
                            )
                        })?
                        .to_string();
                    // Doctor rule (§3): the marker must name a known slot.
                    let base = hop_def_base(&slot).ok_or_else(|| {
                        anyhow!(
                            "HOP_SLOT_UNKNOWN: workflow '{wf_id}' state '{state_name}' \
                             transition '{t_name}': `hop_slot: {slot}` is not a known \
                             specialization slot; valid names: [{}]",
                            HOP_SLOT_NAMES.join(", ")
                        )
                    })?;

                    // STRICT: a hop_slot transition never declares its own
                    // executor — the engine owns wiring the resolved cap.
                    if t_obj.contains_key("executor") {
                        bail!(
                            "HOP_SLOT_EXECUTOR_CONFLICT: workflow '{wf_id}' state \
                             '{state_name}' transition '{t_name}': hop_slot transitions must \
                             not declare an executor; the engine wires the resolved \
                             `cap.{slot}.<stack>`. Remove the `executor:` block."
                        );
                    }

                    // Resolve the marker to a concrete cap (stack-specific, then
                    // `generic` fallback; repo-priority breaks multi-namespace
                    // ties). A load error if nothing resolves.
                    let resolved = resolve_hop_cap(
                        &slot,
                        &stack_chain,
                        &loaded_ids,
                        &repo_priority,
                        wf_id,
                        state_name,
                        t_name,
                    )?;

                    // (a) Inject the input contract, honoring an explicit author schema.
                    if !t_obj.contains_key("inputSchema") {
                        t_obj.insert("inputSchema".into(), hop_ref(base, "In"));
                    }

                    // Wire the resolved cap as a `kind: workflow` executor. The
                    // `use:` block expands normally in `expand_use_bindings`
                    // (which runs after this pass): `outputs` synthesizes the
                    // `output:` mapping that lands the cap's `<slot>` output at
                    // `$.context.<slot>`; `inputs` forwards the required
                    // In-contract fields the actor supplied as arguments.
                    let mut use_inputs = Map::new();
                    for field in crate::hop::slot_in_required(base) {
                        use_inputs
                            .insert(field.clone(), Value::String(format!("$.arguments.{field}")));
                    }
                    let mut use_outputs = Map::new();
                    use_outputs.insert(
                        format!("$.context.{slot}"),
                        // Convention (Spec A.1 §4.2): a `cap.<slot>.<stack>`
                        // names its HOP output after the slot.
                        Value::String(slot.clone()),
                    );
                    t_obj.insert(
                        "executor".into(),
                        json!({
                            "kind": "workflow",
                            "definitionId": resolved,
                            "use": {
                                "inputs": Value::Object(use_inputs),
                                "outputs": Value::Object(use_outputs),
                            }
                        }),
                    );

                    // Record the Out ref; the engine owns this contract.
                    out_slots.insert(slot, hop_ref(base, "Out"));
                }
            }
        }

        // (b) Declare the typed blackboard slots for the collected Outs.
        if !out_slots.is_empty() {
            let bb = def_obj
                .entry("blackboard")
                .or_insert_with(|| Value::Object(Map::new()));
            let Some(bb_obj) = bb.as_object_mut() else {
                bail!(
                    "HOP_SLOT_BLACKBOARD_SHAPE: workflow '{wf_id}' declares `blackboard:` in \
                     array (bare-name) form, but a `hop_slot:` transition needs an object-form \
                     `blackboard:` to carry the typed slot contract. Convert `blackboard:` to \
                     the object form (`{{ <name>: <schema> }}`)."
                );
            };
            for (slot, out_ref) in out_slots {
                // Engine owns the slot Out contract — overwrite any authored schema.
                bb_obj.insert(slot, out_ref);
            }
        }
    }

    Ok(())
}

/// Read the stamped `namespace → priority` map (`/praxec/_repoPriority`,
/// [`stamp_repo_priority`]) into a lookup. Absent → empty (all namespaces
/// default to priority `0`).
fn read_repo_priority(config: &Value) -> HashMap<String, i64> {
    config
        .pointer("/praxec/_repoPriority")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(ns, v)| v.as_i64().map(|p| (ns.clone(), p)))
                .collect()
        })
        .unwrap_or_default()
}

/// Spec A §5.1 — parse a workflow's `stack:` field into an ordered,
/// most-specific-first specificity chain `[project, primary_framework, language]`
/// (absent levels skipped). `resolve_hop_cap` appends the `generic` floor.
///
/// Two accepted forms:
/// - a plain string (`stack: rust`) — language-only (back-compat);
/// - an object `{ language, frameworks: [set], primary_framework, project }` —
///   `project` and `primary_framework` layer above `language`. `frameworks:` is
///   accepted (additive-knowledge composition is a separate later concern,
///   Spec A §5.2) but does NOT participate in override-resolution; only
///   `primary_framework` does.
///
/// Absent `stack:` → empty chain → resolves against `generic` alone.
/// A non-string / non-object `stack:`, or non-string level values, are load
/// errors (poka-yoke — a malformed descriptor never silently degrades).
fn parse_stack_chain(stack: Option<&Value>) -> anyhow::Result<Vec<String>> {
    match stack {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(s)) => {
            // Language-only. `generic` is the floor `resolve_hop_cap` always
            // appends, so an explicit `stack: generic` collapses to the floor.
            if s == "generic" {
                Ok(Vec::new())
            } else {
                Ok(vec![s.clone()])
            }
        }
        Some(Value::Object(obj)) => {
            let level = |key: &str| -> anyhow::Result<Option<String>> {
                match obj.get(key) {
                    None | Some(Value::Null) => Ok(None),
                    Some(Value::String(s)) if s.is_empty() || s == "generic" => Ok(None),
                    Some(Value::String(s)) => Ok(Some(s.clone())),
                    Some(other) => {
                        bail!("HOP_STACK_INVALID: `stack.{key}` must be a string, got {other}")
                    }
                }
            };
            // Most-specific-first: project → primary_framework → language.
            let mut chain = Vec::new();
            if let Some(p) = level("project")? {
                chain.push(p);
            }
            if let Some(f) = level("primary_framework")? {
                chain.push(f);
            }
            if let Some(l) = level("language")? {
                chain.push(l);
            }
            Ok(chain)
        }
        Some(other) => bail!(
            "HOP_STACK_INVALID: `stack:` must be a string (language) or an object \
             {{ language, frameworks, primary_framework, project }}, got {other}"
        ),
    }
}

/// A human-readable rendering of the resolution chain for diagnostics — the
/// specificity levels joined most-specific-first with the `generic` floor,
/// e.g. `myapp→axum→rust→generic`. An empty chain renders as `generic`.
fn stack_display(chain: &[String]) -> String {
    if chain.is_empty() {
        "generic".to_string()
    } else {
        let mut parts: Vec<&str> = chain.iter().map(String::as_str).collect();
        parts.push("generic");
        parts.join("→")
    }
}

/// Spec A §5.1 — resolve a `hop_slot: <slot>` marker to a concrete loaded cap id
/// by walking the specificity chain most-specific-first.
///
/// - Walk `chain` (`[project, primary_framework, language]`, absent levels
///   already skipped) then the `generic` floor. For each level, look for
///   `cap.<slot>.<level>` among loaded ids (host-local, or namespaced as
///   `<ns>/cap.<slot>.<level>`). The FIRST level with any provider wins — a more
///   specific cap always beats a less specific one (override-resolution). No
///   level matches → `HOP_SLOT_UNRESOLVED`.
/// - When several namespaces provide the winning level's cap, the highest
///   `priority:` breaks the tie; a host-local (unprefixed) cap outranks any repo
///   (the operator's own config is top authority). An equal-priority tie →
///   `HOP_SLOT_AMBIGUOUS`.
fn resolve_hop_cap(
    slot: &str,
    chain: &[String],
    loaded_ids: &[String],
    repo_priority: &HashMap<String, i64>,
    wf_id: &str,
    state_name: &str,
    t_name: &str,
) -> anyhow::Result<String> {
    let stack_display = stack_display(chain);
    // Levels to try, most-specific-first, with the `generic` floor last.
    // (`generic` never duplicates: `parse_stack_chain` drops explicit `generic`
    // levels.)
    let mut levels: Vec<&str> = chain.iter().map(String::as_str).collect();
    levels.push("generic");

    let mut candidates: Vec<String> = Vec::new();
    for level in &levels {
        candidates = matching_caps(slot, level, loaded_ids);
        if !candidates.is_empty() {
            break; // most-specific level with a provider wins
        }
    }

    if candidates.is_empty() {
        bail!(
            "HOP_SLOT_UNRESOLVED: workflow '{wf_id}' state '{state_name}' transition '{t_name}': \
             `hop_slot: {slot}` (stack '{stack_display}') resolves to no loaded capability — no \
             `cap.{slot}.<level>` along the chain [{}] nor `cap.{slot}.generic` is loaded. \
             Register a slot cap or declare the repo that provides it.",
            levels.join(", ")
        );
    }

    if candidates.len() == 1 {
        return Ok(candidates.into_iter().next().expect("len checked == 1"));
    }

    // Multiple providers at the winning level — break the tie by repo priority.
    // Host-local (unprefixed) caps are the operator's own config: top authority.
    let ranked: Vec<(i64, String)> = candidates
        .into_iter()
        .map(|id| {
            let prio = match id.split_once('/') {
                Some((ns, _)) => repo_priority.get(ns).copied().unwrap_or(0),
                None => i64::MAX, // host-local wins outright
            };
            (prio, id)
        })
        .collect();
    let top = ranked.iter().map(|(p, _)| *p).max().expect("non-empty");
    let winners: Vec<&String> = ranked
        .iter()
        .filter(|(p, _)| *p == top)
        .map(|(_, id)| id)
        .collect();
    if winners.len() > 1 {
        let mut names: Vec<&str> = winners.iter().map(|s| s.as_str()).collect();
        names.sort_unstable();
        bail!(
            "HOP_SLOT_AMBIGUOUS: workflow '{wf_id}' state '{state_name}' transition '{t_name}': \
             `hop_slot: {slot}` (stack '{stack_display}') is provided by multiple repos at equal \
             priority {top}: [{}]. Raise one repo's `priority:` to disambiguate.",
            names.join(", ")
        );
    }
    Ok(winners[0].clone())
}

/// Collect every loaded id that is `cap.<slot>.<stack>` — either host-local
/// (exact match) or namespaced (`<ns>/cap.<slot>.<stack>`).
fn matching_caps(slot: &str, stack: &str, loaded_ids: &[String]) -> Vec<String> {
    let target = format!("cap.{slot}.{stack}");
    let suffix = format!("/{target}");
    loaded_ids
        .iter()
        .filter(|id| **id == target || id.ends_with(&suffix))
        .cloned()
        .collect()
}

/// SPEC §6 — Walk every workflow's transitions; for any `kind: workflow`
/// executor with a `use:` block:
///
/// 1. Resolve the target capability's `snippet.outputs` from
///    `config["workflows"][definitionId]["snippet"]["outputs"]` and embed
///    it on the executor as `_snippetOutputs` so the runtime executor
///    has the schema in hand without doing a DefinitionStore lookup.
///
/// 2. Synthesize the transition-level `output:` mapping from `use.outputs`.
///    Each `host_path → cap_output_name` entry becomes
///    `<host_path_tail>: "$.output.<cap_output_name>"` where
///    `host_path_tail` strips the `$.context.` prefix. The synthesized
///    mapping merges into any operator-declared `output:` block; operator
///    declarations win on tail-key collisions (so an author can override
///    a single field while letting the rest auto-project).
///
/// Errors when:
/// - `use:` is present but the target `definitionId` is not loaded.
/// - A `use.outputs` LHS does not match `^\$\.context\.[a-z][a-z0-9_-]*$`
///   (V12 — runtime can only write top-level context keys via merge_output).
///
/// Idempotent: re-running on already-expanded config detects the embedded
/// `_snippetOutputs` and skips.
fn expand_use_bindings(config: &mut Value) -> anyhow::Result<()> {
    // Borrow the workflows map immutably to harvest snippet schemas, then
    // walk it mutably to inject the synthesized outputs. We can't do both
    // at once, so snapshot the snippet schemas into a HashMap up front.
    let snippets: HashMap<String, Value> =
        match config.pointer("/workflows").and_then(Value::as_object) {
            Some(workflows) => workflows
                .iter()
                .filter_map(|(id, def)| {
                    // A capability declares its invokable outputs under
                    // `snippet.outputs`; a FLOW (now nestable — V11 relaxed)
                    // declares them under a top-level `outputs:` (flows have no
                    // `snippet:`). Either is the schema embedded as
                    // `_snippetOutputs` for a `kind: workflow` child.
                    def.pointer("/snippet/outputs")
                        .or_else(|| def.pointer("/outputs"))
                        .cloned()
                        .map(|outputs| (id.clone(), outputs))
                })
                .collect(),
            None => HashMap::new(),
        };

    let Some(workflows) = config
        .pointer_mut("/workflows")
        .and_then(Value::as_object_mut)
    else {
        return Ok(());
    };

    for (wf_id, def) in workflows.iter_mut() {
        let Some(states) = def.pointer_mut("/states").and_then(Value::as_object_mut) else {
            continue;
        };
        for (state_name, state_def) in states.iter_mut() {
            let Some(transitions) = state_def
                .pointer_mut("/transitions")
                .and_then(Value::as_object_mut)
            else {
                continue;
            };
            for (t_name, t_def) in transitions.iter_mut() {
                expand_one_transition(t_def, &snippets, wf_id, state_name, t_name)?;
            }
        }
    }
    Ok(())
}

/// Expand a single transition's `use:` block in place. See [`expand_use_bindings`]
/// for the full rule set. Trailing args (`wf_id`, `state_name`, `t_name`)
/// are diagnostic context for error messages — when V12 fires, the operator
/// gets the exact JSON-Pointer-equivalent path to the offender.
fn expand_one_transition(
    t_def: &mut Value,
    snippets: &HashMap<String, Value>,
    wf_id: &str,
    state_name: &str,
    t_name: &str,
) -> anyhow::Result<()> {
    let Some(t_obj) = t_def.as_object_mut() else {
        return Ok(());
    };
    let Some(executor) = t_obj.get_mut("executor") else {
        return Ok(());
    };
    let Some(exec_obj) = executor.as_object_mut() else {
        return Ok(());
    };
    let is_workflow = exec_obj.get("kind").and_then(Value::as_str) == Some("workflow");
    if !is_workflow {
        return Ok(());
    }
    let Some(use_val) = exec_obj.get("use").cloned() else {
        return Ok(());
    };

    // Without a definitionId we can't look up the snippet schema and we
    // can't validate references. Leave the transition untouched and let
    // `validate.rs::validate_use_bindings` surface the diagnostic — it
    // has the same context this function does and produces a proper
    // structured `Diagnostic::Error`.
    let Some(def_id) = exec_obj
        .get("definitionId")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Ok(());
    };

    // V22 (cross-PR with PR1): the ref must resolve to a loaded workflow.
    // We only emit `_snippetOutputs` when the target declares a snippet;
    // legacy non-cap callees stay untouched (some pre-v0.6 fixtures
    // use `kind: workflow` against plain workflows without `snippet:`).
    let snippet_outputs = snippets.get(&def_id);

    // Embed the snippet schema for the runtime executor.
    if let Some(s) = snippet_outputs {
        exec_obj.insert("_snippetOutputs".into(), s.clone());
    }

    // Synthesize the transition-level `output:` mapping from use.outputs.
    // Skips malformed entries silently — `validate.rs::validate_use_block_shape`
    // is the surface that reports them as `Diagnostic::Error`. Errors-as-data
    // beat errors-as-bail here so a single bad transition doesn't poison the
    // whole config load.
    let _ = (wf_id, state_name, t_name); // diagnostic context retained for future use
    let Some(use_outputs) = use_val.get("outputs").and_then(Value::as_object) else {
        return Ok(());
    };
    let mut synthesized = Map::new();
    for (host_path, cap_name_value) in use_outputs {
        let Some(cap_name) = cap_name_value.as_str() else {
            continue;
        };
        let Some(tail) = host_path_tail(host_path) else {
            continue;
        };
        synthesized.insert(tail, Value::String(format!("$.output.{cap_name}")));
    }

    // Merge with any operator-declared `output:` block. Operator wins on
    // collisions (lets authors override one slot while auto-projecting
    // the rest).
    let existing = t_obj
        .get("output")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for (k, v) in existing {
        synthesized.insert(k, v);
    }
    t_obj.insert("output".into(), Value::Object(synthesized));
    Ok(())
}

/// Extract the top-level context-slot name from `$.context.<name>`. Returns
/// `None` for any other path shape (nested paths, non-context roots, etc.).
/// `<name>` must match `^[a-z][a-z0-9_-]*$`.
fn host_path_tail(host_path: &str) -> Option<String> {
    let tail = host_path.strip_prefix("$.context.")?;
    if tail.is_empty() || tail.contains('.') || tail.contains('/') {
        return None;
    }
    let mut chars = tail.chars();
    let first = chars.next()?;
    if !first.is_ascii_lowercase() {
        return None;
    }
    if !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_') {
        return None;
    }
    Some(tail.to_string())
}

/// SPEC §29 — for every workflow with `enable_human_ask: true`, inject
/// a self-loop `ask_human` transition into every non-terminal state.
/// The injected transition:
/// - target = same state (self-loop, doesn't advance)
/// - actor: human (only humans can submit; gates the answer)
/// - purpose: ask (dashboard/client filtering tag)
/// - lightweight: true (audit emits `workflow.interaction` not `.transition`)
/// - max_fires_per_visit: <workflow's `human_ask_cap` field, default 5>
/// - inputSchema requires the agent to fill question + context_summary +
///   attempted_alternatives so questions arrive WITH context
/// - outputSchema requires a string answer
///
/// Idempotent: if the state already declares an `ask_human` transition,
/// the injection is skipped (operator override takes precedence).
fn inject_human_ask_transitions(config: &mut Value) {
    use serde_json::json;
    let Some(workflows) = config
        .pointer_mut("/workflows")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    for (_id, def) in workflows {
        let enabled = def
            .get("enable_human_ask")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !enabled {
            continue;
        }
        let cap = def
            .get("human_ask_cap")
            .and_then(Value::as_u64)
            .unwrap_or(5);
        let Some(states) = def.pointer_mut("/states").and_then(Value::as_object_mut) else {
            continue;
        };
        for (state_name, state_def) in states {
            // Skip terminal states — no point asking questions on a state
            // the workflow can never leave.
            if state_def
                .get("terminal")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                continue;
            }
            let Some(state_obj) = state_def.as_object_mut() else {
                continue;
            };
            // Ensure transitions: {} exists.
            let transitions = state_obj
                .entry("transitions")
                .or_insert(Value::Object(Default::default()))
                .as_object_mut()
                .expect("transitions must be an object");
            // Operator override — don't clobber an existing ask_human.
            if transitions.contains_key("ask_human") {
                continue;
            }
            transitions.insert(
                "ask_human".to_string(),
                json!({
                    "target":              state_name,
                    "actor":               "human",
                    "purpose":             "ask",
                    "lightweight":         true,
                    "max_fires_per_visit": cap,
                    "inputSchema": {
                        "type": "object",
                        "required": ["question", "context_summary", "attempted_alternatives"],
                        "properties": {
                            "question": {
                                "type": "string",
                                "minLength": 1,
                                "maxLength": 2000,
                                "description": "The question for the human. Be specific; the human can't see your reasoning chain."
                            },
                            "context_summary": {
                                "type": "string",
                                "minLength": 1,
                                "maxLength": 1000,
                                "description": "Brief context — what you're trying to do, what state you're in."
                            },
                            "attempted_alternatives": {
                                "type": "string",
                                "minLength": 1,
                                "maxLength": 1000,
                                "description": "What you already tried (docs, scripts, other tools) before asking. SPEC §29.6 — agents must demonstrate effort before interrupting humans."
                            }
                        }
                    },
                    "outputSchema": {
                        "type": "object",
                        "required": ["answer"],
                        "properties": {
                            "answer": {
                                "type": "string",
                                "minLength": 1,
                                "description": "The human's answer."
                            }
                        }
                    }
                }),
            );
        }
    }
}

/// Audit-resolution C.2 — return the blessed root closest to `candidate`
/// by simple shared-prefix length. Cheap heuristic: enough to catch
/// `revoew` → `review` typos without dragging in a Levenshtein dependency.
/// Returns `None` when candidate is empty or no prefix overlap exists.
fn closest_blessed_root(candidate: &str) -> Option<&'static str> {
    if candidate.is_empty() {
        return None;
    }
    let mut best: Option<(usize, &'static str)> = None;
    for root in BLESSED_SUBJECT_ROOTS {
        let shared = candidate
            .chars()
            .zip(root.chars())
            .take_while(|(a, b)| a == b)
            .count();
        if shared == 0 {
            continue;
        }
        if best.map(|(b, _)| shared > b).unwrap_or(true) {
            best = Some((shared, root));
        }
    }
    best.map(|(_, r)| r)
}

/// SPEC §30.5 durability — merge lexicon terms persisted to disk by a prior
/// run into the authored `lexicon:` block. Files live under
/// `praxec.authoring.lexicon_dir` (default `.praxec/lexicon`) and each is
/// `{ "term": "...", "entry": { ... } }`. An authored entry of the same name
/// always wins. Malformed/unreadable files are skipped silently — a corrupt
/// persisted term must never break config load.
fn merge_persisted_lexicon(config: &mut Value) {
    let dir = config
        .pointer("/praxec/authoring/lexicon_dir")
        .and_then(Value::as_str)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(".praxec/lexicon"));
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return;
    };
    let Some(root) = config.as_object_mut() else {
        return;
    };
    let lex = root
        .entry("lexicon".to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let Some(lex_map) = lex.as_object_mut() else {
        return;
    };
    for ent in rd.flatten() {
        let path = ent.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(v) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        else {
            continue;
        };
        if let (Some(term), Some(entry)) = (v.get("term").and_then(Value::as_str), v.get("entry")) {
            lex_map
                .entry(term.to_string())
                .or_insert_with(|| entry.clone());
        }
    }
}

/// SPEC §17.x (v0.3) — Validate `praxec.authoring.*` preferences. v1
/// surface is one optional field, `preferred_script_language`. Adding
/// more authoring preferences later means adding more checks here.
/// All authoring preferences are advisory — surfaced to LLMs via
/// template substitution, never enforced.
fn validate_authoring_preferences(config: &Value) -> anyhow::Result<()> {
    let Some(authoring) = config.pointer("/praxec/authoring") else {
        return Ok(());
    };
    let Some(obj) = authoring.as_object() else {
        bail!(
            "INVALID_AUTHORING_PREFERENCE: `praxec.authoring` must be an object \
             ({})",
            short_value_kind(authoring)
        );
    };
    if let Some(lang) = obj.get("preferred_script_language") {
        match lang {
            Value::String(s) if !s.is_empty() => {}
            Value::String(_) => bail!(
                "INVALID_AUTHORING_PREFERENCE: `praxec.authoring.preferred_script_language` \
                 is empty. Either set a non-empty string (e.g. `bash`, `python3`, `powershell`) \
                 or omit the key entirely."
            ),
            other => bail!(
                "INVALID_AUTHORING_PREFERENCE: `praxec.authoring.preferred_script_language` \
                 must be a string ({})",
                short_value_kind(other)
            ),
        }
    }
    Ok(())
}

/// SPEC §17.x (v0.3) — Stamp `praxec.authoring` onto every workflow
/// snapshot as `_authoringPrefs` so template substitution can reach the
/// preferences at render time via `{{$.praxec.authoring.*}}`. The
/// snapshot is self-contained (SPEC §8.2): an in-flight instance sees
/// the preferences that existed at `workflow.start`, not whatever the
/// live config currently says.
///
/// Cheap by design: the authoring block is typically a small map
/// (one or a few key/value pairs). The duplication cost across workflows
/// is negligible; the alternative — plumbing the live config Arc
/// through the template resolver — would add far more surface than this
/// saves.
fn stamp_authoring_preferences(config: &mut Value) {
    let prefs = match config.pointer("/praxec/authoring") {
        Some(p) if !p.as_object().map(|m| m.is_empty()).unwrap_or(true) => p.clone(),
        _ => return,
    };
    let Some(workflows) = config
        .pointer_mut("/workflows")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    for def in workflows.values_mut() {
        let Some(obj) = def.as_object_mut() else {
            continue;
        };
        obj.insert("_authoringPrefs".into(), prefs.clone());
    }
}

/// SPEC §21 — Validate that every `states.<name>.delegate` value (when
/// present) is a non-empty string. The runtime treats the field as a
/// pass-through pointer; shape-validation here means `runtime_response.rs`
/// never has to defend against `null`/`""`/numeric values reaching the
/// response surface. Returns `INVALID_DELEGATE` naming the workflow + state.
fn validate_state_delegate(config: &Value) -> anyhow::Result<()> {
    let Some(workflows) = config.pointer("/workflows").and_then(Value::as_object) else {
        return Ok(());
    };
    for (wf_id, wf_def) in workflows {
        let Some(states) = wf_def.pointer("/states").and_then(Value::as_object) else {
            continue;
        };
        for (state_name, state_def) in states {
            let Some(value) = state_def.get("delegate") else {
                continue;
            };
            match value {
                Value::String(s) if !s.is_empty() => {}
                Value::String(_) => bail!(
                    "INVALID_DELEGATE: workflow '{wf_id}' state '{state_name}' \
                     has empty `delegate`. Must be a non-empty agent-config name (SPEC §21)."
                ),
                _ => bail!(
                    "INVALID_DELEGATE: workflow '{wf_id}' state '{state_name}' \
                     has non-string `delegate` ({}). Must be a non-empty string naming \
                     an agent config (SPEC §21).",
                    short_value_kind(value)
                ),
            }
        }
    }
    Ok(())
}

/// ADR-0007 — a workflow's optional `orchestrator` (the agent/model that drives
/// it) must be a non-empty string ref (a model name or an agent name). Absent →
/// the gateway default orchestrator applies. Shape-validated so the launch path
/// never defends against `orchestrator: ""` / non-string.
fn validate_orchestrator(config: &Value) -> anyhow::Result<()> {
    let Some(workflows) = config.pointer("/workflows").and_then(Value::as_object) else {
        return Ok(());
    };
    for (wf_id, wf_def) in workflows {
        let Some(value) = wf_def.get("orchestrator") else {
            continue;
        };
        match value {
            Value::String(s) if !s.is_empty() => {}
            Value::String(_) => bail!(
                "INVALID_ORCHESTRATOR: workflow '{wf_id}' has empty `orchestrator`. \
                 Must be a non-empty model or agent name, or omit it for the gateway \
                 default (ADR-0007)."
            ),
            _ => bail!(
                "INVALID_ORCHESTRATOR: workflow '{wf_id}' has non-string `orchestrator` \
                 ({}). Must be a non-empty string naming a model or agent (ADR-0007).",
                short_value_kind(value)
            ),
        }
    }
    Ok(())
}

/// Short human-readable name for a JSON value's kind. Used by error messages
/// that quote a config-shape mismatch.
fn short_value_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// SPEC §8.4 + §20.2 — `praxec.*` flags (e.g. `praxec.authoring.write_enabled`,
/// `praxec.strict_namespacing`) are read only at gateway startup. They MUST
/// NOT appear nested inside any `workflows.<id>` definition — otherwise an
/// LLM-authored workflow could embed a key intending to flip a runtime
/// invariant.
///
/// This validator walks every workflow definition recursively and rejects
/// any object key literally named `praxec` OR starting with `praxec.`,
/// returning `CONFIG_FLAG_NOT_RUNTIME_MUTABLE` with the exact JSON Pointer
/// path to the offending key.
fn validate_workflow_flag_scope(config: &Value) -> anyhow::Result<()> {
    let Some(workflows) = config.pointer("/workflows").and_then(Value::as_object) else {
        return Ok(());
    };
    for (wf_id, wf_def) in workflows {
        let base = format!("/workflows/{wf_id}");
        check_no_praxec_keys(wf_def, &base)?;
    }
    Ok(())
}

/// Recursively walk `value` looking for any object key literally `praxec`
/// or starting with `praxec.`. Returns a CONFIG_FLAG_NOT_RUNTIME_MUTABLE
/// error naming the JSON-Pointer path of the first offender.
fn check_no_praxec_keys(value: &Value, path: &str) -> anyhow::Result<()> {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                if k == "praxec" || k.starts_with("praxec.") {
                    bail!(
                        "CONFIG_FLAG_NOT_RUNTIME_MUTABLE: key '{k}' at '{path}' \
                         — `praxec.*` flags are read at gateway startup only and \
                         MUST NOT appear inside `workflows:` (SPEC §8.4)."
                    );
                }
                let child_path = format!("{path}/{k}");
                check_no_praxec_keys(v, &child_path)?;
            }
            Ok(())
        }
        Value::Array(arr) => {
            for (i, v) in arr.iter().enumerate() {
                let child_path = format!("{path}/{i}");
                check_no_praxec_keys(v, &child_path)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn validate_skills(config: &Value, diagnostics: &mut Vec<Diagnostic>) -> anyhow::Result<()> {
    let Some(skills) = config.pointer("/skills").and_then(Value::as_object) else {
        return Ok(());
    };
    let strict_ns = strict_namespacing(config);
    for (subject, entry) in skills {
        // SPEC §5.4.2 — subject must be dotted-namespaced. The dotted pattern
        // is enforced regardless of strict_namespacing; only the *blessed
        // root* check is governed by the flag.
        if subject.trim().is_empty() {
            bail!("EMPTY_SUBJECT: skills key is empty after trim");
        }
        if !is_subject_pattern(subject) {
            bail!(
                "skills key '{subject}' must match ^[a-z][a-z0-9-]+(\\.[a-z][a-z0-9-]+)+$ \
                 — lowercase, kebab, dotted, at least two segments, no whitespace (SPEC §5.4.2)"
            );
        }
        // First-segment blessed-root check. Under strict_namespacing
        // (default true), an unblessed root is a hard error; otherwise
        // (SPEC §5.4.2 / audit-resolution C.2) it's a soft warning
        // pushed into the diagnostics collector.
        let root = strip_namespace_prefix(subject)
            .split('.')
            .next()
            .unwrap_or("");
        if !BLESSED_SUBJECT_ROOTS.contains(&root) {
            if strict_ns {
                bail!(
                    "INVALID_SUBJECT_ROOT: skills key '{subject}' has unblessed root '{root}'; \
                     blessed roots are {:?} (SPEC §5.4.2). Disable with `praxec.strict_namespacing: false`.",
                    BLESSED_SUBJECT_ROOTS
                );
            } else {
                let suggestion =
                    closest_blessed_root(root).map(|sugg| format!("did you mean '{sugg}'?"));
                diagnostics.push(Diagnostic {
                    severity: DiagnosticSeverity::Warn,
                    code: "INVALID_SUBJECT_ROOT".to_string(),
                    message: format!(
                        "skills key '{subject}' has unblessed root '{root}'; \
                         blessed roots are {:?}",
                        BLESSED_SUBJECT_ROOTS
                    ),
                    location: Some(format!("/skills/{subject}")),
                    suggestion,
                });
            }
        }

        // SPEC §5.4.1 — `verb` is a closed enum.
        let verb_str = entry
            .get("verb")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("MISSING_VERB: skills entry '{subject}' is missing a `verb`"))?;
        if Verb::from_token(verb_str).is_none() {
            bail!(
                "INVALID_VERB: skills entry '{subject}' has verb '{verb_str}'; \
                 allowed verbs are {:?} (SPEC §5.4.1)",
                Verb::ALL_TOKENS
            );
        }

        // SPEC §5.3 — `lifecycle` is required, closed enum, no silent default.
        let lifecycle_str = entry
            .get("lifecycle")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                anyhow!(
                    "MISSING_LIFECYCLE: skills entry '{subject}' is missing a `lifecycle` field; \
                     allowed values are {:?} (SPEC §5.3)",
                    Lifecycle::ALL_TOKENS
                )
            })?;
        if Lifecycle::from_token(lifecycle_str).is_none() {
            bail!(
                "INVALID_LIFECYCLE: skills entry '{subject}' has lifecycle '{lifecycle_str}'; \
                 allowed values are {:?} (SPEC §5.3)",
                Lifecycle::ALL_TOKENS
            );
        }

        // Body required (no silent default).
        let body = entry.get("body").and_then(Value::as_str).ok_or_else(|| {
            anyhow!("MISSING_BODY: skills entry '{subject}' is missing a `body` string")
        })?;

        // SPEC §5.7 — if author provided a pre-computed hash, it must match
        // the normalized body hash. Authors aren't required to provide one;
        // we compute it at stamp time. But if present, mismatch is fail-fast.
        if let Some(stored_hash) = entry.get("hash").and_then(Value::as_str) {
            let computed = compute_skill_hash(body);
            if stored_hash != computed {
                bail!(
                    "HASH_MISMATCH: skills entry '{subject}' has stored hash '{stored_hash}' \
                     but normalize_for_hash(body) produced '{computed}' (SPEC §5.7)"
                );
            }
        }
    }
    Ok(())
}

/// SPEC §22 — validate the top-level `scripts:` block. Mirrors
/// [`validate_skills`] in shape with three key differences:
///
/// 1. **Verb vocabulary** is the [`ScriptVerb`] closed enum (build/test/
///    deploy/format/lint/install/verify/run), not [`Verb`].
/// 2. **Blessed roots** come from [`BLESSED_SCRIPT_ROOTS`], not
///    [`BLESSED_SUBJECT_ROOTS`].
/// 3. **Body source is XOR**: either inline `body: string` OR external
///    `{ uri: string, hash: string }`. v1 supports `file://` URIs only.
fn validate_scripts(config: &Value, diagnostics: &mut Vec<Diagnostic>) -> anyhow::Result<()> {
    let Some(scripts) = config.pointer("/scripts").and_then(Value::as_object) else {
        return Ok(());
    };
    let strict_ns = strict_namespacing(config);
    for (subject, entry) in scripts {
        // Subject shape — same pattern as skills.
        if subject.trim().is_empty() {
            bail!("EMPTY_SCRIPT_SUBJECT: scripts key is empty after trim");
        }
        if !is_subject_pattern(subject) {
            bail!(
                "scripts key '{subject}' must match ^[a-z][a-z0-9-]+(\\.[a-z][a-z0-9-]+)+$ \
                 — lowercase, kebab, dotted, at least two segments, no whitespace (SPEC §22.4)"
            );
        }
        let root = strip_namespace_prefix(subject)
            .split('.')
            .next()
            .unwrap_or("");
        if !BLESSED_SCRIPT_ROOTS.contains(&root) {
            if strict_ns {
                bail!(
                    "INVALID_SCRIPT_SUBJECT_ROOT: scripts key '{subject}' has unblessed root '{root}'; \
                     blessed roots are {:?} (SPEC §22.4). Disable with `praxec.strict_namespacing: false`.",
                    BLESSED_SCRIPT_ROOTS
                );
            } else {
                let suggestion =
                    closest_blessed_script_root(root).map(|sugg| format!("did you mean '{sugg}'?"));
                diagnostics.push(Diagnostic {
                    severity: DiagnosticSeverity::Warn,
                    code: "INVALID_SCRIPT_SUBJECT_ROOT".to_string(),
                    message: format!(
                        "scripts key '{subject}' has unblessed root '{root}'; \
                         blessed roots are {:?}",
                        BLESSED_SCRIPT_ROOTS
                    ),
                    location: Some(format!("/scripts/{subject}")),
                    suggestion,
                });
            }
        }

        // SPEC §22.3 — `verb` is a closed enum, distinct from cognitive Verb.
        let verb_str = entry.get("verb").and_then(Value::as_str).ok_or_else(|| {
            anyhow!("MISSING_SCRIPT_VERB: scripts entry '{subject}' is missing a `verb`")
        })?;
        if ScriptVerb::from_token(verb_str).is_none() {
            bail!(
                "INVALID_SCRIPT_VERB: scripts entry '{subject}' has verb '{verb_str}'; \
                 allowed verbs are {:?} (SPEC §22.3)",
                ScriptVerb::ALL_TOKENS
            );
        }

        // Lifecycle — same shape as skills; the Lifecycle enum is shared.
        let lifecycle_str = entry
            .get("lifecycle")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                anyhow!(
                    "MISSING_SCRIPT_LIFECYCLE: scripts entry '{subject}' is missing a `lifecycle` field; \
                     allowed values are {:?} (SPEC §22)",
                    Lifecycle::ALL_TOKENS
                )
            })?;
        if Lifecycle::from_token(lifecycle_str).is_none() {
            bail!(
                "INVALID_SCRIPT_LIFECYCLE: scripts entry '{subject}' has lifecycle '{lifecycle_str}'; \
                 allowed values are {:?} (SPEC §22)",
                Lifecycle::ALL_TOKENS
            );
        }

        // SPEC §22.2 — body source XOR: inline body OR external uri+hash.
        let body_inline = entry.get("body").and_then(Value::as_str);
        let uri = entry.get("uri").and_then(Value::as_str);
        match (body_inline, uri) {
            (Some(_), Some(_)) => bail!(
                "SCRIPT_BODY_SOURCE_AMBIGUOUS: scripts entry '{subject}' declares both \
                 `body` and `uri` — exactly one is required (SPEC §22.2)"
            ),
            (None, None) => bail!(
                "SCRIPT_BODY_SOURCE_AMBIGUOUS: scripts entry '{subject}' declares neither \
                 `body` nor `uri` — exactly one is required (SPEC §22.2)"
            ),
            (Some(body), None) => {
                // Inline body: hash is OPTIONAL (computed at stamp time).
                // If author provided one, it must match.
                if let Some(stored_hash) = entry.get("hash").and_then(Value::as_str) {
                    validate_hash_format(stored_hash, subject)?;
                    let computed = compute_script_hash(body);
                    if stored_hash != computed {
                        bail!(
                            "SCRIPT_HASH_MISMATCH: scripts entry '{subject}' has stored hash \
                             '{stored_hash}' but normalize_for_script_hash(body) produced \
                             '{computed}' (SPEC §22.2). Script hashing collapses trailing \
                             newlines only; internal whitespace is preserved."
                        );
                    }
                }
            }
            (None, Some(uri_str)) => {
                // External body: hash is REQUIRED (we verify at stamp time).
                let stored_hash = entry.get("hash").and_then(Value::as_str).ok_or_else(|| {
                    anyhow!(
                        "MISSING_SCRIPT_HASH: scripts entry '{subject}' uses an external \
                             `uri` but has no `hash`. Hash is required for uri-sourced bodies \
                             so the runtime can verify content-identity at load time (SPEC §22.2)."
                    )
                })?;
                validate_hash_format(stored_hash, subject)?;
                if !(uri_str.starts_with("file://")
                    || uri_str.starts_with("https://")
                    || uri_str.starts_with("git+https://"))
                {
                    let scheme = uri_str.split("://").next().unwrap_or(uri_str);
                    bail!(
                        "UNSUPPORTED_SCRIPT_URI_SCHEME: scripts entry '{subject}' uri \
                         '{uri_str}' uses scheme '{scheme}://' — supported schemes are \
                         `file://` (relative to config), `https://` (load-time fetch), \
                         and `git+https://...@<ref>#<path>` (load-time `git archive` \
                         extraction). All non-file URIs require sha256 verification per \
                         SPEC §22.2."
                    );
                }
                if uri_str.starts_with("git+https://") {
                    // Cheap structural check at validate time — delegate to
                    // shared helper so include: and scripts: use identical logic.
                    validate_git_https_uri_shape(uri_str, &format!("scripts entry '{subject}'"))?;
                }
                // file:// resolution + hash verification happens at
                // stamp_scripts_library time (Tranche N) — needs the config
                // file path for relative-path resolution, which validate
                // doesn't have. https:// is also resolved there (no
                // base-dir rewrite needed; URLs are already absolute).
                // Shape is locked here; integrity is enforced there.
            }
        }
    }
    Ok(())
}

/// Structural check for `git+https://<host>/<repo>(.git)?@<ref>#<path>`.
/// Shared by `scripts:` validation and `include:` resolution. Emits
/// `INVALID_GIT_HTTPS_URI` on missing `#<path>` or `@<ref>`.
fn validate_git_https_uri_shape(uri: &str, subject: &str) -> anyhow::Result<()> {
    let body = uri.trim_start_matches("git+https://");
    let (repo_at_ref, _path) = body.split_once('#').ok_or_else(|| {
        anyhow!(
            "INVALID_GIT_HTTPS_URI: {subject} uri '{uri}' is missing the `#<path>` fragment. \
             Required form: git+https://<host>/<repo>(.git)?@<ref>#<path>"
        )
    })?;
    if !repo_at_ref.contains('@') {
        bail!(
            "INVALID_GIT_HTTPS_URI: {subject} uri '{uri}' is missing the `@<ref>` revision. \
             Required form: git+https://<host>/<repo>(.git)?@<ref>#<path>."
        );
    }
    Ok(())
}

/// Validate that `s` matches `^sha256:[0-9a-f]{64}$` — the only hash format
/// the script library accepts. Future-proofed by making this a check, not a
/// hard parser: if we add `sha512:` later we update this in one place.
fn validate_hash_format(s: &str, subject: &str) -> anyhow::Result<()> {
    if !s.starts_with("sha256:") {
        bail!(
            "INVALID_SCRIPT_HASH_FORMAT: scripts entry '{subject}' hash '{s}' is missing \
             the `sha256:` prefix. Expected `sha256:<64-hex-chars>` (SPEC §22.2)."
        );
    }
    let hex = &s["sha256:".len()..];
    if hex.len() != 64
        || !hex
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    {
        bail!(
            "INVALID_SCRIPT_HASH_FORMAT: scripts entry '{subject}' hash '{s}' has malformed \
             digest. Expected `sha256:<64 lowercase hex chars>` (SPEC §22.2)."
        );
    }
    Ok(())
}

/// SPEC §22.2 — closest-blessed-script-root suggestion for the lenient
/// namespacing diagnostic. Mirror of [`closest_blessed_root`].
fn closest_blessed_script_root(candidate: &str) -> Option<&'static str> {
    if candidate.is_empty() {
        return None;
    }
    let mut best: Option<(usize, &'static str)> = None;
    for root in BLESSED_SCRIPT_ROOTS {
        let shared = candidate
            .chars()
            .zip(root.chars())
            .take_while(|(a, b)| a == b)
            .count();
        if shared == 0 {
            continue;
        }
        if best.map(|(b, _)| shared > b).unwrap_or(true) {
            best = Some((shared, root));
        }
    }
    best.map(|(_, r)| r)
}

/// SPEC §22.2 — script body normalization. **Stricter than
/// [`normalize_for_hash`]** because shell scripts treat whitespace as
/// load-bearing: `if [[ $x == "y" ]]` and `if [[ $x  ==  "y" ]]` are
/// different programs, and `\t` vs spaces matters for heredocs.
///
/// Rules:
/// 1. Preserve all internal whitespace exactly (no collapse).
/// 2. Collapse trailing newlines to exactly one terminal newline (so
///    `script\n` and `script\n\n\n` hash identically — editor-dependent
///    trailing-newline drift shouldn't break content-identity).
/// 3. No leading-whitespace trim (scripts may legitimately start with `#!`
///    on column 0 or with whitespace for indentation).
///
/// This stricter rule means inline `body: |` YAML scripts are hashed
/// verbatim modulo trailing newlines. Authors who edit a script body in
/// place (changing a tab to spaces, say) WILL get a SCRIPT_HASH_MISMATCH
/// when a uri-source script references that body — by design.
///
/// ```
/// use praxec_core::config::normalize_for_script_hash;
///
/// // Internal whitespace preserved.
/// assert_eq!(normalize_for_script_hash("if [[  x ]]"), "if [[  x ]]\n");
/// // Trailing newlines collapsed to one.
/// assert_eq!(normalize_for_script_hash("echo hi\n\n\n"), "echo hi\n");
/// // Single trailing newline preserved.
/// assert_eq!(normalize_for_script_hash("echo hi\n"), "echo hi\n");
/// // No trailing newline -> one added.
/// assert_eq!(normalize_for_script_hash("echo hi"), "echo hi\n");
/// ```
pub fn normalize_for_script_hash(body: &str) -> String {
    // Strip all trailing newlines first.
    let mut s = body.to_string();
    while s.ends_with('\n') {
        s.pop();
    }
    // Re-append exactly one terminal newline.
    s.push('\n');
    s
}

/// SPEC §22.2 — content-identity hash for a script body. Pair with
/// [`normalize_for_script_hash`] always; never hash raw bytes.
///
/// ```
/// use praxec_core::config::compute_script_hash;
///
/// // Trailing-newline drift produces identical hashes.
/// assert_eq!(
///     compute_script_hash("echo hi\n"),
///     compute_script_hash("echo hi\n\n\n"),
/// );
/// // But internal whitespace changes produce different hashes (unlike skill hash).
/// assert_ne!(
///     compute_script_hash("if [[ x ]]"),
///     compute_script_hash("if [[  x  ]]"),
/// );
/// // Hash carries algorithm prefix + lowercase-hex digest.
/// let h = compute_script_hash("echo hi");
/// assert!(h.starts_with("sha256:"));
/// assert_eq!(h.len(), "sha256:".len() + 64);
/// ```
pub fn compute_script_hash(body: &str) -> String {
    let normalized = normalize_for_script_hash(body);
    let digest = Sha256::digest(normalized.as_bytes());
    format!("sha256:{:x}", digest)
}

/// Raw sha256 of `content` exactly as given (no newline normalization).
/// Used to verify `include:` bodies byte-for-byte. Distinct from
/// `compute_script_hash`, which collapses trailing newlines for scripts.
pub fn raw_content_sha256(content: &str) -> String {
    let digest = Sha256::digest(content.as_bytes());
    format!("sha256:{:x}", digest)
}

/// SPEC §5.4.2 — subject pattern: dotted, lowercase-kebab segments, at least
/// two segments (`a.b`), no whitespace. Does NOT enforce blessed-root; that's
/// a separate check governed by `strict_namespacing`.
///
/// SPEC §9 — accepts an optional single-segment namespace prefix
/// (`<ns>/<a>.<b>`) for skills loaded via a `repos:` manifest.
/// Bare-subject form remains the canonical shape for skills declared
/// directly in the gateway config.
fn is_subject_pattern(s: &str) -> bool {
    let body = strip_namespace_prefix(s);
    let parts: Vec<&str> = body.split('.').collect();
    if parts.len() < 2 {
        return false;
    }
    parts.iter().all(|p| is_kebab_token(p))
}

/// SPEC §9 — return the post-prefix portion of a subject string. If `s`
/// has a single leading `<ns>/` segment (kebab-token namespace), strip
/// it; otherwise return the input unchanged. Used by the blessed-root
/// check so namespace-prefixed subjects like `cognitive/plan.draft`
/// are evaluated against the root `plan`, not `cognitive`.
fn strip_namespace_prefix(s: &str) -> &str {
    match s.split_once('/') {
        Some((ns, rest)) if is_kebab_token(ns) => rest,
        _ => s,
    }
}

/// SPEC §5.4.2 — under `strict_namespacing: true` (default), an unblessed
/// subject root is a hard error. With `false`, it's a warning surfaced via
/// the `check` diagnostics layer. Top-level `praxec.strict_namespacing`
/// only — schema must reject this flag at workflow scope
/// (`CONFIG_FLAG_NOT_RUNTIME_MUTABLE`); enforcement happens in `resolve`.
fn strict_namespacing(config: &Value) -> bool {
    config
        .pointer("/praxec/strict_namespacing")
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

/// SPEC §5.7 — body normalization rule applied before hashing.
///
/// 1. Trim leading and trailing whitespace.
/// 2. Replace each run of internal whitespace (spaces, tabs, newlines) of
///    length ≥1 with a single space.
/// 3. Strip a trailing newline if any remains after step 2.
///
/// This is the **single source-of-truth** function for hash normalization.
/// Every component that hashes a body MUST call this; read-side and
/// write-side parity is enforced by cross-impl test.
///
/// ```
/// use praxec_core::config::normalize_for_hash;
///
/// assert_eq!(normalize_for_hash("  hello   world  "), "hello world");
/// assert_eq!(normalize_for_hash("a\n\nb"), "a b");
/// assert_eq!(normalize_for_hash("trailing\n\n"), "trailing");
/// // Idempotent: re-normalizing produces the same output.
/// let once = normalize_for_hash("  a b  c\n");
/// assert_eq!(normalize_for_hash(&once), once);
/// ```
pub fn normalize_for_hash(body: &str) -> String {
    let trimmed = body.trim();
    let mut out = String::with_capacity(trimmed.len());
    let mut in_ws_run = false;
    for c in trimmed.chars() {
        if c.is_whitespace() {
            if !in_ws_run {
                out.push(' ');
                in_ws_run = true;
            }
        } else {
            out.push(c);
            in_ws_run = false;
        }
    }
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// SPEC §5.7 — `sha256:` prefix + lowercase-hex digest of the normalized
/// body's UTF-8 bytes. Pair this with [`normalize_for_hash`] always — never
/// hash raw bytes.
///
/// ```
/// use praxec_core::config::compute_skill_hash;
///
/// // Whitespace-only differences MUST produce the same hash (normalization
/// // is the whole point — read-side and write-side must agree).
/// assert_eq!(
///     compute_skill_hash("hello world"),
///     compute_skill_hash("  hello   world  "),
/// );
///
/// // Hash always carries the algorithm prefix and a lowercase-hex digest.
/// let h = compute_skill_hash("anything");
/// assert!(h.starts_with("sha256:"));
/// assert_eq!(h.len(), "sha256:".len() + 64);
/// ```
pub fn compute_skill_hash(body: &str) -> String {
    let normalized = normalize_for_hash(body);
    let digest = Sha256::digest(normalized.as_bytes());
    format!("sha256:{:x}", digest)
}

/// SPEC §8.4 — canonical content hash of a definition value: `sha256:` + hex
/// digest of its canonical (recursively key-sorted) JSON. Two definitions hash
/// equal iff structurally equal, independent of key order. This is the basis
/// for optimistic concurrency on **edit-publish**: the snapshot an author
/// edited must still be the current one, or the write is rejected.
///
/// ```
/// use praxec_core::config::compute_definition_hash;
/// use serde_json::json;
///
/// // Key order does not matter — same structure, same hash.
/// let a = json!({ "initialState": "s", "states": { "s": { "terminal": true } } });
/// let b = json!({ "states": { "s": { "terminal": true } }, "initialState": "s" });
/// assert_eq!(compute_definition_hash(&a), compute_definition_hash(&b));
///
/// // A real change flips the hash; the prefix + width are stable.
/// let c = json!({ "initialState": "t", "states": {} });
/// assert_ne!(compute_definition_hash(&a), compute_definition_hash(&c));
/// let h = compute_definition_hash(&a);
/// assert!(h.starts_with("sha256:"));
/// assert_eq!(h.len(), "sha256:".len() + 64);
/// ```
pub fn compute_definition_hash(definition: &Value) -> String {
    let mut canonical = String::new();
    write_canonical_json(definition, &mut canonical);
    let digest = Sha256::digest(canonical.as_bytes());
    format!("sha256:{:x}", digest)
}

/// Serialize `value` to deterministic JSON with object keys sorted recursively,
/// independent of serde_json's map ordering (so the hash is stable whether or
/// not `preserve_order` is enabled anywhere in the dep graph).
fn write_canonical_json(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                // Infallible for a Value string key; `expect` documents the
                // invariant and fails loud rather than silently emitting `""` —
                // a wrong hash would weaken the optimistic-concurrency guard.
                out.push_str(&serde_json::to_string(k).expect("serialize JSON string key"));
                out.push(':');
                write_canonical_json(&map[*k], out);
            }
            out.push('}');
        }
        Value::Array(arr) => {
            out.push('[');
            for (i, e) in arr.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical_json(e, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&serde_json::to_string(scalar).expect("serialize JSON scalar")),
    }
}

/// SPEC §8.4 — a minimal unified line diff between two definitions (pretty
/// YAML), for human/LLM review before an edit is published. Lines only in
/// `prior` are prefixed `-`, only in `candidate` `+`, unchanged lines ` `.
/// LCS-based so unchanged regions stay aligned. Returns `"(no changes)"` when
/// the two are identical.
///
/// ```
/// use praxec_core::config::definition_diff;
/// use serde_json::json;
///
/// let prior = json!({ "initialState": "draft" });
/// let candidate = json!({ "initialState": "ready" });
/// let d = definition_diff(&prior, &candidate);
/// assert!(d.contains("- initialState: draft"));
/// assert!(d.contains("+ initialState: ready"));
///
/// // Identical definitions report no changes.
/// assert_eq!(definition_diff(&prior, &prior), "(no changes)");
/// ```
pub fn definition_diff(prior: &Value, candidate: &Value) -> String {
    // Infallible for a serde_json::Value; `expect` over `unwrap_or_default` so a
    // serialization failure can't silently render an empty (misleading) diff.
    let a_text = serde_yaml::to_string(prior).expect("serialize prior definition to YAML");
    let b_text = serde_yaml::to_string(candidate).expect("serialize candidate definition to YAML");
    let a: Vec<&str> = a_text.lines().collect();
    let b: Vec<&str> = b_text.lines().collect();

    // LCS table over lines.
    let (n, m) = (a.len(), b.len());
    let mut lcs = vec![vec![0u16; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }

    let mut out = String::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push_str(&format!("  {}\n", a[i]));
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            out.push_str(&format!("- {}\n", a[i]));
            i += 1;
        } else {
            out.push_str(&format!("+ {}\n", b[j]));
            j += 1;
        }
    }
    while i < n {
        out.push_str(&format!("- {}\n", a[i]));
        i += 1;
    }
    while j < m {
        out.push_str(&format!("+ {}\n", b[j]));
        j += 1;
    }

    if out.lines().all(|l| l.starts_with("  ")) {
        return "(no changes)".to_string();
    }
    out
}

fn stamp_skills_library(config: &mut Value) {
    let full_library: Map<String, Value> =
        match config.pointer("/skills").and_then(Value::as_object) {
            Some(skills) if !skills.is_empty() => {
                // SPEC §8.2: the snapshot is self-contained — it carries the
                // resolved fragment bodies the workflow references, not just
                // the verb. Editing the top-level `skills:` block cannot
                // mutate what an in-flight instance sees.
                let mut lib = Map::new();
                for (subject, entry) in skills {
                    // validate_skills already enforced these are present
                    // and well-typed; unwrap_or_default would mask drift, so
                    // we re-read defensively but with explicit shape.
                    let Some(verb) = entry.get("verb").and_then(Value::as_str) else {
                        continue;
                    };
                    let Some(lifecycle) = entry.get("lifecycle").and_then(Value::as_str) else {
                        continue;
                    };
                    let Some(body) = entry.get("body").and_then(Value::as_str) else {
                        continue;
                    };
                    let hash = compute_skill_hash(body);
                    let source = entry
                        .get("source")
                        .and_then(Value::as_str)
                        .unwrap_or("config");
                    lib.insert(
                        subject.clone(),
                        json!({
                            "verb":      verb,
                            "lifecycle": lifecycle,
                            "body":      body,
                            "hash":      hash,
                            "source":    source,
                        }),
                    );
                }
                lib
            }
            _ => return,
        };

    if let Some(workflows) = config
        .pointer_mut("/workflows")
        .and_then(Value::as_object_mut)
    {
        for def in workflows.values_mut() {
            let Some(obj) = def.as_object_mut() else {
                continue;
            };
            let referenced = collect_referenced_subjects(obj);
            if referenced.is_empty() {
                continue;
            }
            let mut scoped = Map::new();
            for subject in &referenced {
                if let Some(entry) = full_library.get(subject) {
                    scoped.insert(subject.clone(), entry.clone());
                }
            }
            // Skip stamping if none of the referenced subjects resolve — the
            // check pass reports those dangling refs as errors; no need to
            // bloat the snapshot with an empty library.
            if !scoped.is_empty() {
                obj.insert("_skillsLibrary".into(), Value::Object(scoped));
            }
        }
    }
}

/// SPEC §22 — stamp `_scriptsLibrary` onto each workflow that references a
/// curated script, mirroring [`stamp_skills_library`]. Resolution policy:
///
/// - Inline `body:` scripts → body stamped verbatim; hash computed.
/// - `uri:` scripts → file:// URIs resolved at load time (path is already
///   absolute by the time we get here, courtesy of [`rewrite_script_uris_to_absolute`]).
///   Body materialized into the snapshot; the declared `hash` is verified
///   against `compute_script_hash(resolved_body)`. Mismatch → `SCRIPT_HASH_MISMATCH`.
///
/// The instance-snapshot invariant (SPEC §8.2) holds for scripts the same way
/// it does for skills: editing the top-level `scripts:` block — or the
/// external file — after `workflow.start` cannot mutate what an in-flight
/// instance sees.
fn stamp_scripts_library(config: &mut Value) -> anyhow::Result<()> {
    let scripts = match config.pointer("/scripts").and_then(Value::as_object) {
        Some(s) if !s.is_empty() => s.clone(),
        _ => return Ok(()),
    };
    let mut full_library: Map<String, Value> = Map::new();
    for (subject, entry) in &scripts {
        // validate_scripts has already enforced shape; re-read defensively.
        let Some(verb) = entry.get("verb").and_then(Value::as_str) else {
            continue;
        };
        let Some(lifecycle) = entry.get("lifecycle").and_then(Value::as_str) else {
            continue;
        };
        let source = entry
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or("config")
            .to_string();

        // Materialize body (inline or uri). Hash verification on uri path.
        let (body, hash) = match (
            entry.get("body").and_then(Value::as_str),
            entry.get("uri").and_then(Value::as_str),
        ) {
            (Some(b), None) => (b.to_string(), compute_script_hash(b)),
            (None, Some(uri)) => {
                let declared_hash = entry
                    .get("hash")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        anyhow!(
                            "scripts entry '{subject}' uri-source without hash reached \
                             stamp_scripts_library — validate_scripts should have caught this"
                        )
                    })?
                    .to_string();
                let body = read_script_uri(uri, subject)?;
                let computed = compute_script_hash(&body);
                if computed != declared_hash {
                    bail!(
                        "SCRIPT_HASH_MISMATCH: scripts entry '{subject}' uri '{uri}' \
                         resolved to a body whose content-hash is '{computed}' but the \
                         declared hash is '{declared_hash}'. Either the external source \
                         has drifted since the workflow was authored, or the declared \
                         hash is wrong (SPEC §22.2)."
                    );
                }
                (body, declared_hash)
            }
            // Both / neither — validate_scripts has already errored.
            _ => continue,
        };

        full_library.insert(
            subject.clone(),
            json!({
                "verb":      verb,
                "lifecycle": lifecycle,
                "body":      body,
                "hash":      hash,
                "source":    source,
            }),
        );
    }

    if let Some(workflows) = config
        .pointer_mut("/workflows")
        .and_then(Value::as_object_mut)
    {
        for def in workflows.values_mut() {
            let Some(obj) = def.as_object_mut() else {
                continue;
            };
            let referenced = collect_referenced_script_subjects(obj);
            if referenced.is_empty() {
                continue;
            }
            let mut scoped = Map::new();
            for subject in &referenced {
                if let Some(entry) = full_library.get(subject) {
                    scoped.insert(subject.clone(), entry.clone());
                }
            }
            if !scoped.is_empty() {
                obj.insert("_scriptsLibrary".into(), Value::Object(scoped));
            }
        }
    }
    Ok(())
}

/// Walk a workflow definition and collect every subject named by a
/// `script` executor — workflow-level `onEnter`, state-level `onEnter`,
/// and every transition's `executor`. Parallel to
/// [`collect_referenced_subjects`] but the harvest point is
/// `executor: { kind: script, subject: <name> }` rather than `skills: [...]`.
fn collect_referenced_script_subjects(workflow: &Map<String, Value>) -> HashSet<String> {
    let mut out = HashSet::new();
    collect_script_subject_from(workflow.get("onEnter"), &mut out);
    let Some(states) = workflow.get("states").and_then(Value::as_object) else {
        return out;
    };
    for state in states.values() {
        collect_script_subject_from(state.get("onEnter"), &mut out);
        let Some(transitions) = state.get("transitions").and_then(Value::as_object) else {
            continue;
        };
        for t in transitions.values() {
            collect_script_subject_from(t.get("executor"), &mut out);
        }
    }
    out
}

/// If `scope` is an executor block (or an action wrapping one) with
/// `kind: script`, push its `subject` into `out`. Tolerant of either
/// `{ kind, subject, ... }` directly or `{ executor: { kind, subject, ... } }`
/// nesting (the onEnter shape).
fn collect_script_subject_from(scope: Option<&Value>, out: &mut HashSet<String>) {
    let Some(v) = scope else { return };
    // Unwrap onEnter action wrapper.
    let executor = v
        .get("executor")
        .filter(|inner| inner.is_object())
        .unwrap_or(v);
    if executor.get("kind").and_then(Value::as_str) == Some("script") {
        if let Some(subj) = executor.get("subject").and_then(Value::as_str) {
            out.insert(subj.to_string());
        }
    }
}

/// SPEC §22.2 — read a script URI's contents. Dispatches by scheme:
/// - `file://` → local filesystem (absolute path post-rewrite).
/// - `https://` → blocking HTTP GET via reqwest::blocking. The
///   declared `hash:` is what makes this safe — we verify the
///   fetched bytes match the operator's declaration, so a hijacked
///   endpoint can't silently swap the script.
///
/// Other schemes are validator-rejected upstream; this function
/// errors on them as a defense-in-depth assertion.
fn read_script_uri(uri: &str, subject: &str) -> anyhow::Result<String> {
    if let Some(path) = uri.strip_prefix("file://") {
        return std::fs::read_to_string(path).with_context(|| {
            format!("reading scripts entry '{subject}' from {uri} (resolved path: {path})")
        });
    }
    if uri.starts_with("https://") {
        return read_https_uri(uri, subject);
    }
    if uri.starts_with("git+https://") {
        return read_git_https_uri(uri, subject);
    }
    bail!(
        "UNSUPPORTED_SCRIPT_URI_SCHEME: scripts entry '{subject}' uri '{uri}' \
         reached read_script_uri with an unsupported scheme — validate_scripts \
         should have caught this. Supported: file://, https://, git+https://."
    )
}

/// Resolve a `git+https://<host>/<repo>(.git)?@<ref>#<path>` URI by
/// invoking `git archive --remote=<https-url> <ref> <path> | tar`.
/// This avoids a full clone — only the requested ref+path is fetched.
///
/// Many forges (GitHub, GitLab.com) do NOT support `git archive` over
/// https for security reasons (it's `git upload-archive` permission,
/// often disabled). When that's the case, this function emits a
/// `GIT_ARCHIVE_NOT_SUPPORTED` error suggesting the operator either
/// host the script via plain `https://` (raw.githubusercontent.com,
/// gist raw URL) or run a local mirror that allows `upload-archive`.
///
/// Hash-verified by the caller; we don't trust the network or git's
/// integrity guarantees — operator-declared sha256 is the gate.
fn read_git_https_uri(uri: &str, subject: &str) -> anyhow::Result<String> {
    let body = uri
        .strip_prefix("git+https://")
        .expect("validate_scripts ensures git+https:// prefix");
    let (repo_at_ref, path) = body
        .split_once('#')
        .ok_or_else(|| anyhow!("missing #<path> in {uri}"))?;
    let (repo, gitref) = repo_at_ref
        .rsplit_once('@')
        .ok_or_else(|| anyhow!("missing @<ref> in {uri}"))?;
    let repo_url = format!("https://{repo}");

    // `git archive --remote=<url> <ref> <path>` writes a tar to stdout.
    // We pipe to `tar -x -O -f -` to extract <path> only and dump
    // contents to stdout in one shot.
    //
    // Two child processes connected via a pipe. We capture tar's
    // stdout as the script body.
    use std::io::Read;
    use std::process::{Command, Stdio};

    let mut git = Command::new("git")
        .arg("archive")
        .arg("--format=tar")
        .arg(format!("--remote={repo_url}"))
        .arg(gitref)
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "spawning `git archive` for scripts entry '{subject}' uri '{uri}'. \
                 The `git` binary must be on PATH for git+https:// script URIs."
            )
        })?;

    let git_stdout = git
        .stdout
        .take()
        .ok_or_else(|| anyhow!("scripts entry '{subject}' git archive missing stdout pipe"))?;

    let mut tar = Command::new("tar")
        .arg("-x")
        .arg("-O")
        .arg("-f")
        .arg("-")
        .arg(path)
        .stdin(Stdio::from(git_stdout))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            format!("spawning `tar` to extract scripts entry '{subject}' from git archive")
        })?;

    let mut body = String::new();
    let mut tar_stdout = tar
        .stdout
        .take()
        .ok_or_else(|| anyhow!("tar missing stdout pipe"))?;
    tar_stdout
        .read_to_string(&mut body)
        .with_context(|| format!("reading scripts entry '{subject}' body from tar stdout"))?;

    let git_status = git
        .wait()
        .with_context(|| format!("waiting on `git archive` for scripts entry '{subject}'"))?;
    let tar_status = tar
        .wait()
        .with_context(|| format!("waiting on `tar` for scripts entry '{subject}'"))?;

    if !git_status.success() {
        bail!(
            "GIT_ARCHIVE_NOT_SUPPORTED: scripts entry '{subject}' uri '{uri}' — \
             `git archive --remote={repo_url}` exited with code {:?}. Many forges \
             (GitHub, GitLab.com) disable `upload-archive` over https for security. \
             Workarounds: host the script via plain https:// (e.g. \
             raw.githubusercontent.com/<owner>/<repo>/<ref>/<path>), or use a \
             self-hosted mirror that permits upload-archive.",
            git_status.code()
        );
    }
    if !tar_status.success() {
        bail!(
            "scripts entry '{subject}' uri '{uri}' — `tar -x -O` exited with code \
             {:?}. The git archive may not contain '{path}', or the path uses an \
             unsupported format.",
            tar_status.code()
        );
    }
    if body.is_empty() {
        bail!(
            "scripts entry '{subject}' uri '{uri}' resolved to an empty body. \
             Check that '{path}' exists in the repo at ref '{gitref}'."
        );
    }
    Ok(body)
}

/// Blocking HTTP GET for an `https://` script URI. 30-second hard
/// timeout (script bodies are small; long blocking calls at config
/// load are an operator-visible problem). Non-200 responses fail
/// with a clear error naming the URL + status code.
///
/// The fetched body is hash-verified by the caller; no need to
/// trust the network — operator-declared sha256 is the integrity gate.
fn read_https_uri(uri: &str, subject: &str) -> anyhow::Result<String> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(concat!("praxec/", env!("CARGO_PKG_VERSION")))
        .build()
        .with_context(|| {
            format!("building blocking HTTP client for scripts entry '{subject}' {uri}")
        })?;
    let resp = client
        .get(uri)
        .send()
        .with_context(|| format!("fetching scripts entry '{subject}' from {uri}"))?;
    let status = resp.status();
    if !status.is_success() {
        bail!(
            "SCRIPT_URI_FETCH_FAILED: scripts entry '{subject}' uri '{uri}' returned \
             HTTP {} — expected 2xx. Caller may have moved/deleted the resource, or \
             the host requires authentication (not currently supported; the v1 https \
             fetcher is anonymous).",
            status.as_u16()
        );
    }
    resp.text()
        .with_context(|| format!("decoding body for scripts entry '{subject}' from {uri}"))
}

/// SPEC §22.2 — rewrite relative `file://` URIs in `scripts:` entries to
/// absolute paths, relative to `base_dir`. Called by `load_yaml_inner`
/// after parsing each YAML file so `resolve()` can stay path-agnostic.
///
/// Idempotent: an already-absolute `file:///etc/foo.sh` is left alone.
/// Non-`file://` URIs are left alone (the validator will reject them).
fn rewrite_script_uris_to_absolute(value: &mut Value, base_dir: &Path) {
    let Some(scripts) = value.pointer_mut("/scripts").and_then(Value::as_object_mut) else {
        return;
    };
    for entry in scripts.values_mut() {
        let Some(obj) = entry.as_object_mut() else {
            continue;
        };
        let Some(uri_val) = obj.get_mut("uri") else {
            continue;
        };
        let Some(uri_str) = uri_val.as_str() else {
            continue;
        };
        let Some(rest) = uri_str.strip_prefix("file://") else {
            continue;
        };
        if rest.starts_with('/') {
            // Already absolute.
            continue;
        }
        let abs = base_dir.join(rest);
        // Canonicalize when possible (cleans up `./` etc.); fall back to
        // join result for not-yet-existing paths so the load-time
        // file-read produces a clear NotFound rather than a canonicalize
        // error here.
        let final_path = abs.canonicalize().unwrap_or(abs);
        *uri_val = Value::String(format!("file://{}", final_path.display()));
    }
}

/// Walk a workflow definition and collect every subject named in any
/// `skills:` array — workflow, state, and transition scope (SPEC §5.5).
fn collect_referenced_subjects(workflow: &Map<String, Value>) -> HashSet<String> {
    let mut out = HashSet::new();
    collect_skills_strings(workflow.get("skills"), &mut out);
    let Some(states) = workflow.get("states").and_then(Value::as_object) else {
        return out;
    };
    for state in states.values() {
        collect_skills_strings(state.get("skills"), &mut out);
        let Some(transitions) = state.get("transitions").and_then(Value::as_object) else {
            continue;
        };
        for t in transitions.values() {
            collect_skills_strings(t.get("skills"), &mut out);
        }
    }
    out
}

fn collect_skills_strings(scope: Option<&Value>, out: &mut HashSet<String>) {
    if let Some(arr) = scope.and_then(Value::as_array) {
        for entry in arr {
            if let Some(s) = entry.as_str() {
                out.insert(s.to_string());
            }
        }
    }
}

fn is_kebab_token(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Convenience: load + resolve in one call.
pub fn load_resolved(path: impl AsRef<Path>) -> anyhow::Result<Value> {
    resolve(load_yaml(path)?)
}

/// Convenience: load + resolve in one call, also returning any soft
/// diagnostics (SPEC §5.4.2 / audit-resolution C.2).
pub fn load_resolved_with_diagnostics(
    path: impl AsRef<Path>,
) -> anyhow::Result<(Value, Vec<Diagnostic>)> {
    resolve_with_diagnostics(load_yaml(path)?)
}

/// SPEC §9 — load + merge declared repos + resolve in one call.
///
/// Compared to [`load_resolved_with_diagnostics`], this variant additionally
/// honors top-level `repos: [{ path: <repo-root> }]` and `overrides: [<id>]`
/// blocks in the host config. Each repo is loaded via [`crate::repo::load_repo`],
/// its definitionIds prefixed `<namespace>/`, and merged into the gateway
/// registry BEFORE the host config's own entries — so the host can shadow a
/// repo-provided id only when it lists the id in `overrides:` (V23). Repos
/// declaring the same `namespace` fail at load (V20). After merging, every
/// `kind: workflow` `definitionId:` reference must resolve to a loaded entry
/// (V22).
///
/// Hosts with no `repos:` block behave exactly like
/// [`load_resolved_with_diagnostics`] — the wrapper is the new entrypoint
/// the binary should call regardless.
pub fn load_resolved_with_repos(
    path: impl AsRef<Path>,
) -> anyhow::Result<(Value, Vec<Diagnostic>)> {
    let path = path.as_ref();
    let host = load_yaml(path)?;
    let parent_dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let merged = merge_declared_repos(host, &parent_dir)?;
    resolve_with_diagnostics(merged)
}

/// Extract `repos:` + `overrides:` from `host`, load each repo, validate
/// V20 / V21 / V22 / V23, deep-merge repo contents (then host on top so
/// declared overrides win), and return the cleaned value (with `repos:`
/// and `overrides:` stripped). Hosts without a `repos:` block round-trip
/// through unchanged.
fn merge_declared_repos(mut host: Value, host_dir: &Path) -> anyhow::Result<Value> {
    let (repos, overrides) = take_repos_and_overrides(&mut host)?;
    if repos.is_empty() {
        // No repos declared — strip an empty `overrides:` (it's meaningless
        // without any repo-provided ids to shadow). Host-level staged
        // connections (`px connections add` / `grant`) still get the grant gate.
        let staged_ungranted = apply_staged_connection_grants(&mut host)?;
        stamp_ungranted_connections(&mut host, staged_ungranted);
        return Ok(host);
    }

    let mut repo_aggregate = Value::Object(Map::new());
    let mut repo_provided_ids: HashSet<String> = HashSet::new();
    let mut seen_namespaces: HashMap<String, String> = HashMap::new();
    // SPEC §8.4 — (absolute root, push) of repos opted in as authoring write
    // targets, carried forward to the gateway via the resolved config (see
    // `stamp_writable_repos`).
    let mut writable_repo_roots: Vec<(String, bool)> = Vec::new();
    // Spec A §5 — namespace → priority, carried into the merged config for
    // `hop_slot:` cap-resolution tie-breaking (see `stamp_repo_priority`).
    let mut repo_priorities: Vec<(String, i64)> = Vec::new();
    // SPEC §9.5 — pack-declared connections the operator has NOT granted,
    // diverted out of the live registry and stamped as self-documenting
    // diagnostic state (see `stamp_ungranted_connections`).
    let mut ungranted_connections: Vec<UngrantedConnection> = Vec::new();

    for RepoDecl {
        source,
        writable,
        push,
        priority,
        grant_connections,
    } in repos
    {
        // Resolve to a local path: a local dir relative to the host config (same
        // base-dir convention as `include:`), or a remote repo imported (cloned/
        // updated) into a cache under `<host>/.praxec/repos/<slug>`. The import
        // shells out to git, inheriting the operator's git auth.
        let repo_path = match source {
            RepoSource::Local(p) => {
                if p.is_absolute() {
                    p
                } else {
                    host_dir.join(p)
                }
            }
            RepoSource::Remote { uri, gitref } => {
                let dest = host_dir
                    .join(".praxec")
                    .join("repos")
                    .join(crate::repo_git::cache_dir_name(&uri));
                crate::repo_git::clone_or_update(&uri, &gitref, &dest)
                    .with_context(|| format!("importing repo {uri}"))?
            }
        };
        let (manifest, mut repo_value) = crate::repo::load_repo(&repo_path)
            .with_context(|| format!("loading repo at {}", repo_path.display()))?;
        if writable {
            writable_repo_roots.push((repo_path.display().to_string(), push));
        }
        // V20 — namespace uniqueness across declared repos.
        if let Some(prev_name) =
            seen_namespaces.insert(manifest.namespace.clone(), manifest.name.clone())
        {
            bail!(
                "DUPLICATE_REPO_NAMESPACE: namespace '{}' is declared by repos '{}' and '{}'. \
                 Each repo MUST declare a unique namespace (SPEC §9.4).",
                manifest.namespace,
                prev_name,
                manifest.name
            );
        }
        repo_priorities.push((manifest.namespace.clone(), priority));
        // Collect ids BEFORE the grant gate strips ungranted connections, so
        // a host definition colliding with an ungranted pack connection still
        // trips V23 (the operator must acknowledge the shadowing either way).
        for id in crate::repo::aggregate_ids(&repo_value) {
            repo_provided_ids.insert(id);
        }
        // SPEC §9.5 — connection GRANT GATE. A pack DECLARES connections; only
        // the OPERATOR activates them (the `human:intent` trust factor). Any
        // connection this repo contributed that is not listed in the host's
        // `grant_connections:` is diverted out of the live `/connections`
        // registry here, before the merge — so it can never be spawned and
        // never seeds the authoring provenance gate.
        ungranted_connections.extend(gate_repo_connections(
            &mut repo_value,
            &manifest,
            &grant_connections,
        )?);
        repo_aggregate = deep_merge(repo_aggregate, repo_value);
    }

    // V23 — any host-defined id that collides with a repo-provided id MUST
    // appear in the explicit `overrides:` block. This closes the supply-chain
    // backdoor: an operator cannot silently shadow a vendored definition.
    let host_ids = host_definition_ids(&host);
    let collisions: Vec<String> = host_ids.intersection(&repo_provided_ids).cloned().collect();
    for id in &collisions {
        if !overrides.contains(id) {
            bail!(
                "ANONYMOUS_OVERRIDE: '{id}' is provided by a declared repo and shadowed \
                 by the host config without an explicit `overrides:` entry. Add `{id}` to \
                 the top-level `overrides:` array to make the shadowing intentional \
                 (SPEC §9.4)."
            );
        }
    }
    // Any id listed in `overrides:` that doesn't actually collide is a
    // stale declaration — surface it as a hard error so authors aren't
    // misled into thinking they're shadowing something they aren't.
    for id in &overrides {
        if !repo_provided_ids.contains(id) {
            bail!(
                "STALE_OVERRIDE: `overrides:` lists '{id}', but no declared repo provides \
                 that id. Remove it or correct the namespace prefix (SPEC §9.4)."
            );
        }
    }

    // Repo contents first, host body last → host wins on the explicitly
    // declared overrides.
    let mut merged = deep_merge(repo_aggregate, host);

    // V22 — every `kind: workflow` definitionId reference in the merged
    // registry must resolve. References were namespace-prefixed inside
    // each repo's workflow bodies by `load_repo`; here we walk the final
    // registry and assert every `kind: workflow` ref binds.
    validate_workflow_refs_resolve(&merged)?;

    // SPEC §8.4 — carry the writable repo roots forward so the gateway can
    // construct the repo-backed `DefinitionStoreWritable` when
    // `praxec.authoring.write_enabled` is set. Reads still flow through the
    // merged registry above; this records only the authoring write target(s).
    stamp_writable_repos(&mut merged, writable_repo_roots);

    // Spec A §5 — carry namespace priorities forward for `hop_slot:` resolution.
    stamp_repo_priority(&mut merged, repo_priorities);

    // SPEC §9.5 — surface ungranted pack connections as live, self-documenting
    // DEGRADED state (the #23 pattern): each entry carries the exact YAML
    // remedy. Keys the host itself (re)declares are skipped — the operator's
    // own definition is live under that key (implicit grant by authorship),
    // gated by the explicit `overrides:` acknowledgment above.
    ungranted_connections.retain(|u| !host_ids.contains(&u.key));
    // SPEC §9.5 — host-level staged connections (`px connections add` / `grant`):
    // promote granted staged bodies into the live `/connections` registry and
    // divert ungranted ones to `_ungrantedConnections` — the same mechanism as
    // repo-declared connections. Applied after the repo merge so a staged name
    // colliding with a repo-provided live connection is caught.
    let staged_ungranted = apply_staged_connection_grants(&mut merged)?;
    ungranted_connections.extend(staged_ungranted);
    stamp_ungranted_connections(&mut merged, ungranted_connections);
    Ok(merged)
}

/// SPEC §9.5 — apply the host-level staged-connection grant gate (`px connections
/// add` / `grant`). This is the operator-authored analog of
/// [`gate_repo_connections`]: `add` writes a body under top-level
/// `stagedConnections:` (never live), and `grant` lists its name under top-level
/// `grant_connections:`. Here, for each staged entry:
///   - GRANTED (name in `grant_connections:`) → the body is promoted into the
///     live `/connections` registry (fail-fast on a collision with an existing
///     live connection — never a silent overwrite);
///   - UNGRANTED → returned as an [`UngrantedConnection`] for stamping into
///     `/praxec/_ungrantedConnections`, so a spawn attempt fails typed with the
///     grant remedy exactly like a pack-declared ungranted connection.
///
/// Both `stagedConnections:` and top-level `grant_connections:` are stripped from
/// the resolved config (internal authoring state, not live registry keys). A
/// grant naming no staged connection is a hard error (`STALE_CONNECTION_GRANT`,
/// mirroring the repo path). A config with neither key round-trips unchanged.
fn apply_staged_connection_grants(config: &mut Value) -> anyhow::Result<Vec<UngrantedConnection>> {
    let Some(obj) = config.as_object_mut() else {
        return Ok(Vec::new());
    };
    let staged = match obj.remove("stagedConnections") {
        Some(Value::Object(m)) => m,
        None | Some(Value::Null) => {
            // No staged connections — a lone `grant_connections:` grants nothing.
            if let Some(g) = obj.remove("grant_connections") {
                if g.as_array().is_some_and(|a| !a.is_empty()) {
                    bail!(
                        "STALE_CONNECTION_GRANT: top-level `grant_connections:` lists names, but \
                         there is no `stagedConnections:` block. Add a connection with \
                         `px connections add <name> --kind <kind> ...` first (SPEC §9.5)."
                    );
                }
            }
            return Ok(Vec::new());
        }
        Some(_) => bail!(
            "INVALID_STAGED_CONNECTIONS: top-level `stagedConnections:` must be a mapping of \
             name → connection body (SPEC §9.5)."
        ),
    };
    let grants: HashSet<String> = match obj.remove("grant_connections") {
        Some(Value::Array(a)) => a
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        None | Some(Value::Null) => HashSet::new(),
        Some(_) => bail!(
            "INVALID_GRANT_CONNECTIONS: top-level `grant_connections:` must be an array of \
             staged connection names (SPEC §9.5)."
        ),
    };
    // A grant naming a connection that is not staged is stale (mirrors the repo
    // `STALE_CONNECTION_GRANT`): the operator's mental model has drifted.
    for g in &grants {
        if !staged.contains_key(g) {
            bail!(
                "STALE_CONNECTION_GRANT: top-level `grant_connections:` lists '{g}', but no \
                 `stagedConnections:` entry declares it. Add it with \
                 `px connections add {g} --kind <kind> ...` or remove the grant (SPEC §9.5)."
            );
        }
    }
    let mut ungranted = Vec::new();
    for (name, body) in staged {
        if grants.contains(&name) {
            let connections = obj
                .entry("connections")
                .or_insert_with(|| Value::Object(Map::new()));
            let cm = connections.as_object_mut().ok_or_else(|| {
                anyhow::anyhow!("INVALID_CONNECTIONS: `connections:` must be a mapping")
            })?;
            if cm.contains_key(&name) {
                bail!(
                    "DUPLICATE_CONNECTION: granting staged connection '{name}' collides with an \
                     existing live `connections:` entry of the same name. Rename or remove one \
                     (SPEC §9.5)."
                );
            }
            cm.insert(name, body);
        } else {
            ungranted.push(UngrantedConnection {
                key: name.clone(),
                bare: name,
                repo: "host config (px connections add)".to_string(),
                namespace: String::new(),
            });
        }
    }
    Ok(ungranted)
}

/// SPEC §9.5 — one pack-declared connection the operator has not granted.
/// Carried from the grant gate to [`stamp_ungranted_connections`].
struct UngrantedConnection {
    /// Fully-qualified live-registry key the connection would occupy
    /// (`<namespace>/<name>`).
    key: String,
    /// Bare pack-local name — what the operator writes in `grant_connections:`.
    bare: String,
    /// Manifest `name` of the declaring repo.
    repo: String,
    /// Manifest `namespace` of the declaring repo.
    namespace: String,
}

/// SPEC §9.5 — apply the connection grant gate to one loaded repo aggregate.
///
/// Every connection key this repo contributed is admitted into the live
/// `/connections` ONLY when the host's `grant_connections:` for this repo
/// lists it (bare pack-local name or fully-qualified `<namespace>/<name>`).
/// Ungranted keys are REMOVED from the aggregate (never merged live) and
/// returned for diagnostic stamping — not silently dropped.
///
/// A grant naming a connection this repo does not declare is a hard error
/// (`STALE_CONNECTION_GRANT`, mirroring `STALE_OVERRIDE`): a grant that grants
/// nothing means the operator's mental model and the pack have drifted.
fn gate_repo_connections(
    repo_value: &mut Value,
    manifest: &crate::repo::RepoManifest,
    grants: &[String],
) -> anyhow::Result<Vec<UngrantedConnection>> {
    let conn_keys: Vec<String> = repo_value
        .pointer("/connections")
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    let ns_prefix = format!("{}/", manifest.namespace);
    for grant in grants {
        let matches = conn_keys
            .iter()
            .any(|k| k == grant || k.strip_prefix(&ns_prefix) == Some(grant.as_str()));
        if !matches {
            bail!(
                "STALE_CONNECTION_GRANT: the `repos:` entry for '{}' lists '{}' in \
                 `grant_connections`, but that repo declares no such connection \
                 (declared: {:?}). Remove the grant or correct the name (SPEC §9.5).",
                manifest.name,
                grant,
                conn_keys
            );
        }
    }
    if conn_keys.is_empty() {
        return Ok(Vec::new());
    }
    let mut ungranted = Vec::new();
    let block = repo_value
        .pointer_mut("/connections")
        .and_then(Value::as_object_mut)
        .expect("conn_keys non-empty implies /connections object");
    for key in conn_keys {
        let bare = key
            .strip_prefix(&ns_prefix)
            .unwrap_or(key.as_str())
            .to_string();
        let granted = grants.iter().any(|g| *g == key || *g == bare);
        if !granted {
            block.remove(&key);
            ungranted.push(UngrantedConnection {
                key,
                bare,
                repo: manifest.name.clone(),
                namespace: manifest.namespace.clone(),
            });
        }
    }
    if block.is_empty() {
        if let Some(obj) = repo_value.as_object_mut() {
            obj.remove("connections");
        }
    }
    Ok(ungranted)
}

/// SPEC §9.5 — record pack-declared-but-ungranted connections under
/// `/praxec/_ungrantedConnections` (internal resolved-config metadata, not an
/// operator-authored key — mirrors [`stamp_writable_repos`]/`_writableRepos`).
/// Keyed by the fully-qualified connection key the pack's workflows reference;
/// each entry names the declaring repo and carries the exact YAML remedy. The
/// executors read this to turn a spawn attempt into a typed
/// `UNGRANTED_PACK_CONNECTION` failure instead of a bare not-found. No-op when
/// empty.
fn stamp_ungranted_connections(config: &mut Value, entries: Vec<UngrantedConnection>) {
    if entries.is_empty() {
        return;
    }
    let Some(obj) = config.as_object_mut() else {
        return;
    };
    let praxec = obj
        .entry("praxec")
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(fg) = praxec.as_object_mut() {
        let mut map = Map::new();
        for u in entries {
            // Host-staged connections (empty namespace) carry the `px connections
            // grant` remedy; pack-declared ones carry the per-repo grant remedy.
            let remedy = if u.namespace.is_empty() {
                format!(
                    "run `px connections grant {0}` (or add `{0}` to the top-level \
                     `grant_connections:`) to activate this operator-staged connection",
                    u.bare
                )
            } else {
                format!(
                    "add `grant_connections: [{}]` to the `repos:` entry for {} to activate \
                     this connection",
                    u.bare, u.repo
                )
            };
            map.insert(
                u.key,
                json!({ "repo": u.repo, "namespace": u.namespace, "remedy": remedy }),
            );
        }
        fg.insert("_ungrantedConnections".into(), Value::Object(map));
    }
}

/// Spec A §5 — record declared repos' `namespace → priority` under
/// `/praxec/_repoPriority` (internal resolved-config metadata, not an
/// operator-authored key — mirrors [`stamp_writable_repos`]/`_writableRepos`).
/// [`inject_hop_slots`] reads it to break `hop_slot:` cap-resolution ties.
/// No-op when empty. Priority `0` entries are stamped too, so an explicit
/// `priority: 0` reads back identically to the default.
fn stamp_repo_priority(config: &mut Value, priorities: Vec<(String, i64)>) {
    if priorities.is_empty() {
        return;
    }
    let Some(obj) = config.as_object_mut() else {
        return;
    };
    let praxec = obj
        .entry("praxec")
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(fg) = praxec.as_object_mut() {
        let mut map = Map::new();
        for (ns, prio) in priorities {
            map.insert(ns, Value::from(prio));
        }
        fg.insert("_repoPriority".into(), Value::Object(map));
    }
}

/// SPEC §8.4 — record repos declared `writable: true` under
/// `/praxec/_writableRepos` as `{ root, push }` objects (internal
/// resolved-config metadata, not an operator-authored key). The gateway reads
/// this to build the repo-backed writable definition store for the authoring
/// write path. No-op when empty.
fn stamp_writable_repos(config: &mut Value, roots: Vec<(String, bool)>) {
    if roots.is_empty() {
        return;
    }
    let Some(obj) = config.as_object_mut() else {
        return;
    };
    let praxec = obj
        .entry("praxec")
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(fg) = praxec.as_object_mut() {
        let entries: Vec<Value> = roots
            .into_iter()
            .map(|(root, push)| json!({ "root": root, "push": push }))
            .collect();
        fg.insert("_writableRepos".into(), Value::Array(entries));
    }
}

/// Remove the `repos:` and `overrides:` top-level keys from `host` and
/// return their parsed payloads. Errors on shape mismatches.
/// SPEC §9 + §8.4 — one parsed `repos:` entry: where the repo lives (local path
/// or a remote URI to import) and whether it's an authoring write target.
struct RepoDecl {
    source: RepoSource,
    writable: bool,
    push: bool,
    /// Spec A §5 — repo-priority for `hop_slot:` cap resolution. When multiple
    /// declared repos provide the same `cap.<slot>.<stack>`, the highest
    /// `priority` wins; an equal-priority tie is a hard load error
    /// (`HOP_SLOT_AMBIGUOUS`). Default `0`. Carried into the merged config as a
    /// `namespace → priority` map (see [`stamp_repo_priority`]).
    priority: i64,
    /// SPEC §9.5 — operator grant for pack-contributed connections. A layered
    /// repo may DECLARE `connections:`, but a declared connection only goes
    /// live when the OPERATOR lists its name here (bare pack-local name or the
    /// fully-qualified `<namespace>/<name>` form). The grant lives ONLY in the
    /// host config — a pack can never grant itself. Ungranted declarations are
    /// diverted to `/praxec/_ungrantedConnections` (see
    /// [`gate_repo_connections`] / [`stamp_ungranted_connections`]). Default
    /// empty: no pack connection is ever live without an explicit grant.
    grant_connections: Vec<String>,
}

/// Where a declared repo comes from.
enum RepoSource {
    /// A local directory (resolved relative to the host config).
    Local(PathBuf),
    /// A remote git repo imported (cloned/updated) into a local cache.
    Remote { uri: String, gitref: String },
}

fn take_repos_and_overrides(host: &mut Value) -> anyhow::Result<(Vec<RepoDecl>, HashSet<String>)> {
    let Some(obj) = host.as_object_mut() else {
        return Ok((Vec::new(), HashSet::new()));
    };
    let repos: Vec<RepoDecl> = match obj.remove("repos") {
        None => Vec::new(),
        Some(Value::Array(arr)) => arr
            .into_iter()
            .enumerate()
            .map(|(i, entry)| parse_repo_entry(i, entry))
            .collect::<anyhow::Result<Vec<_>>>()?,
        Some(other) => bail!(
            "INVALID_REPOS_SHAPE: top-level `repos:` must be an array of `{{ path: <dir> }}` \
             objects ({})",
            short_value_kind(&other)
        ),
    };
    let overrides: HashSet<String> = match obj.remove("overrides") {
        None => HashSet::new(),
        Some(Value::Array(arr)) => arr
            .into_iter()
            .map(|entry| match entry {
                Value::String(s) if !s.is_empty() => Ok(s),
                Value::String(_) => {
                    bail!("INVALID_OVERRIDE_ENTRY: `overrides:` entries MUST be non-empty strings")
                }
                other => bail!(
                    "INVALID_OVERRIDE_ENTRY: `overrides:` entries MUST be strings ({})",
                    short_value_kind(&other)
                ),
            })
            .collect::<anyhow::Result<HashSet<_>>>()?,
        Some(other) => bail!(
            "INVALID_OVERRIDES_SHAPE: top-level `overrides:` must be an array of \
             fully-qualified id strings ({})",
            short_value_kind(&other)
        ),
    };
    Ok((repos, overrides))
}

/// Parse one `repos:` array entry. Accepts `{ path: <string> }`; expands
/// `~/` to `$HOME` and `~` alone is treated literally (no expansion).
fn parse_repo_entry(index: usize, entry: Value) -> anyhow::Result<RepoDecl> {
    // SPEC §8.4 — a repo opts in as an authoring write target with
    // `writable: true`. Default false: declared repos are read-only (consumed,
    // never authored into) unless the operator explicitly marks them.
    let writable = match entry.get("writable") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(other) => bail!(
            "INVALID_REPO_ENTRY: `repos[{index}].writable` must be a boolean ({})",
            short_value_kind(other)
        ),
    };
    // SPEC §9 — publish authored commits to the repo's remote (`git push`) after
    // each register. Only meaningful for a writable repo.
    let push = match entry.get("push") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(other) => bail!(
            "INVALID_REPO_ENTRY: `repos[{index}].push` must be a boolean ({})",
            short_value_kind(other)
        ),
    };
    if push && !writable {
        bail!(
            "INVALID_REPO_ENTRY: `repos[{index}]` sets `push: true` without `writable: true` — \
             only a writable repo has authored commits to push."
        );
    }
    // Spec A §5 — optional numeric `priority:` (default 0) breaks `hop_slot:`
    // cap-resolution ties between repos providing the same `cap.<slot>.<stack>`.
    let priority = match entry.get("priority") {
        None | Some(Value::Null) => 0,
        Some(v) => v.as_i64().ok_or_else(|| {
            anyhow!(
                "INVALID_REPO_ENTRY: `repos[{index}].priority` must be an integer ({})",
                short_value_kind(v)
            )
        })?,
    };
    // SPEC §9.5 — optional `grant_connections:` (default empty), the operator's
    // explicit activation of this repo's declared connections.
    let grant_connections: Vec<String> = match entry.get("grant_connections") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(arr)) => arr
            .iter()
            .map(|v| match v {
                Value::String(s) if !s.is_empty() => Ok(s.clone()),
                Value::String(_) => bail!(
                    "INVALID_REPO_ENTRY: `repos[{index}].grant_connections` entries MUST be \
                     non-empty connection names"
                ),
                other => bail!(
                    "INVALID_REPO_ENTRY: `repos[{index}].grant_connections` entries MUST be \
                     strings ({})",
                    short_value_kind(other)
                ),
            })
            .collect::<anyhow::Result<Vec<_>>>()?,
        Some(other) => bail!(
            "INVALID_REPO_ENTRY: `repos[{index}].grant_connections` must be an array of \
             connection names ({})",
            short_value_kind(other)
        ),
    };
    // SPEC §9 — a repo is either local (`path`) or remote (`uri`, imported via
    // git). Exactly one; a remote repo may pin a `ref` (default `main`).
    let path = entry.get("path").and_then(Value::as_str);
    let uri = entry.get("uri").and_then(Value::as_str);
    let source = match (path, uri) {
        (Some(p), None) => RepoSource::Local(expand_repo_path(p)),
        (None, Some(u)) => {
            let gitref = entry
                .get("ref")
                .and_then(Value::as_str)
                .unwrap_or("main")
                .to_string();
            RepoSource::Remote {
                uri: u.to_string(),
                gitref,
            }
        }
        (Some(_), Some(_)) => bail!(
            "INVALID_REPO_ENTRY: `repos[{index}]` declares both `path` and `uri` — \
             a repo is either local or remote, not both."
        ),
        (None, None) => bail!(
            "INVALID_REPO_ENTRY: `repos[{index}]` needs a `path` (local dir) or a `uri` \
             (remote git repo to import), e.g. `- path: ~/repos/swe-core` or \
             `- uri: git+https://github.com/acme/workflows@main`."
        ),
    };
    Ok(RepoDecl {
        source,
        writable,
        push,
        priority,
        grant_connections,
    })
}

/// Expand a `~/`-prefixed path against `$HOME`. Returns the input unchanged
/// when no `~/` prefix is present or `$HOME` is unset (load-time error will
/// surface in `load_repo` instead, with the unresolved literal in the
/// message).
fn expand_repo_path(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(p)
}

/// Collect every host-declared definitionId across the four prefixable
/// blocks. Mirror of [`crate::repo::aggregate_ids`] for the host side.
fn host_definition_ids(host: &Value) -> HashSet<String> {
    let mut out = HashSet::new();
    let Some(obj) = host.as_object() else {
        return out;
    };
    for block in ["workflows", "skills", "scripts", "connections"] {
        if let Some(entries) = obj.get(block).and_then(Value::as_object) {
            for k in entries.keys() {
                out.insert(k.clone());
            }
        }
    }
    out
}

/// SPEC §9.3 — after repo loading, every `kind: workflow` executor's
/// `definitionId:` reference must resolve to a loaded workflow. Unresolved
/// refs are V22 (likely an unprefixed cross-repo ref or a typo).
fn validate_workflow_refs_resolve(config: &Value) -> anyhow::Result<()> {
    let known: HashSet<String> = config
        .pointer("/workflows")
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    if known.is_empty() {
        return Ok(());
    }
    let mut unresolved: Vec<(String, String)> = Vec::new();
    if let Some(workflows) = config.pointer("/workflows").and_then(Value::as_object) {
        for (wf_id, wf_def) in workflows {
            collect_unresolved_workflow_refs(wf_def, &known, wf_id, &mut unresolved);
        }
    }
    if let Some((wf_id, target)) = unresolved.first() {
        bail!(
            "UNRESOLVED_WORKFLOW_REF: workflow '{wf_id}' references definitionId '{target}' \
             via a `kind: workflow` executor, but no workflow with that id is loaded. \
             Unprefixed names resolve in the workflow's OWN namespace; to call into \
             another repo, fully qualify the id as `<namespace>/{target}` (SPEC §9.3)."
        );
    }
    Ok(())
}

fn collect_unresolved_workflow_refs(
    value: &Value,
    known: &HashSet<String>,
    wf_id: &str,
    out: &mut Vec<(String, String)>,
) {
    match value {
        Value::Object(map) => {
            let is_workflow_exec = map.get("kind").and_then(Value::as_str) == Some("workflow");
            if is_workflow_exec {
                if let Some(def_id) = map.get("definitionId").and_then(Value::as_str) {
                    if !known.contains(def_id) {
                        out.push((wf_id.to_string(), def_id.to_string()));
                    }
                }
            }
            for child in map.values() {
                collect_unresolved_workflow_refs(child, known, wf_id, out);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                collect_unresolved_workflow_refs(v, known, wf_id, out);
            }
        }
        _ => {}
    }
}

/// Parse + resolve a YAML string in-process. Use this when the config is
/// embedded with `include_str!` so rules ship with the binary and end users
/// can't edit them. Multi-file `include:` directives won't work in this path
/// since there's no filesystem to walk — pre-merge with `deep_merge` if you
/// need that.
pub fn resolve_str(yaml: &str) -> anyhow::Result<Value> {
    let value: Value = serde_yaml::from_str(yaml).context("parsing embedded YAML")?;
    resolve(value)
}

// ---------- capability flattening -----------------------------------------

/// A normalized capability: executor + the union of all guards/reliability
/// down the wraps chain, plus carried metadata.
#[derive(Debug, Clone)]
struct NormalizedCapability {
    executor: Value,
    input_schema: Option<Value>,
    title: Option<String>,
    description: Option<String>,
    tags: Vec<Value>,
    examples: Vec<Value>,
    guards: Vec<Value>,
    reliability: Option<Value>,
}

fn flatten_capabilities(config: &Value) -> anyhow::Result<HashMap<String, NormalizedCapability>> {
    let Some(map) = config.pointer("/capabilities").and_then(Value::as_object) else {
        return Ok(HashMap::new());
    };

    let mut resolving = HashSet::new();
    let mut resolved: HashMap<String, NormalizedCapability> = HashMap::new();

    // Preserve declaration order so error messages reference the first one
    // a user hits.
    let names: Vec<String> = map.keys().cloned().collect();
    for name in names {
        flatten_one(&name, map, &mut resolving, &mut resolved)?;
    }
    Ok(resolved)
}

fn flatten_one(
    name: &str,
    raw: &Map<String, Value>,
    resolving: &mut HashSet<String>,
    resolved: &mut HashMap<String, NormalizedCapability>,
) -> anyhow::Result<()> {
    if resolved.contains_key(name) {
        return Ok(());
    }
    if !resolving.insert(name.to_string()) {
        bail!("capability `wraps` cycle detected at '{}'", name);
    }

    let def = raw
        .get(name)
        .ok_or_else(|| anyhow!("capability '{}' is referenced but not defined", name))?;

    let mut current = NormalizedCapability {
        executor: Value::Null,
        input_schema: def.get("inputSchema").cloned(),
        title: def.get("title").and_then(Value::as_str).map(str::to_string),
        description: def
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string),
        tags: def
            .get("tags")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        examples: def
            .get("examples")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        guards: def
            .get("guards")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        reliability: def.get("reliability").cloned(),
    };

    // If this capability wraps another, flatten the parent first and then
    // layer this one's guards / reliability on top. The parent provides the
    // executor unless this def overrides it.
    if let Some(parent_name) = def.get("wraps").and_then(Value::as_str) {
        flatten_one(parent_name, raw, resolving, resolved)?;
        let parent = resolved.get(parent_name).expect("just resolved").clone();
        current.executor = def
            .get("executor")
            .cloned()
            .unwrap_or(parent.executor.clone());
        current.input_schema = current.input_schema.or(parent.input_schema);
        current.title = current.title.or(parent.title);
        current.description = current.description.or(parent.description);
        // Tags/examples union (preserving order, parent first).
        let mut tags = parent.tags;
        tags.extend(current.tags);
        current.tags = tags;
        let mut examples = parent.examples;
        examples.extend(current.examples);
        current.examples = examples;
        // Guards stack: parent first, then wrapper's. Both must pass.
        let mut guards = parent.guards;
        guards.extend(current.guards);
        current.guards = guards;
        // Reliability: more specific (this def) wins; else inherit.
        current.reliability = current.reliability.or(parent.reliability);
    } else {
        current.executor = def
            .get("executor")
            .cloned()
            .ok_or_else(|| anyhow!("capability '{}' needs `executor` or `wraps`", name))?;
    }

    resolving.remove(name);
    resolved.insert(name.to_string(), current);
    Ok(())
}

// ---------- exposure rewriting --------------------------------------------

fn rewrite_exposure(
    exposure: Value,
    registry: &HashMap<String, NormalizedCapability>,
) -> anyhow::Result<Value> {
    let Some(obj) = exposure.as_object() else {
        return Ok(exposure);
    };

    if let Some(cap_name) = obj.get("capability").and_then(Value::as_str) {
        let cap = registry
            .get(cap_name)
            .ok_or_else(|| anyhow!("exposure references unknown capability '{}'", cap_name))?;

        let alias = obj
            .get("as")
            .and_then(Value::as_str)
            .unwrap_or(cap_name)
            .to_string();

        let mut out = Map::new();
        out.insert("name".into(), Value::String(alias));
        if let Some(t) = &cap.title {
            out.insert("title".into(), Value::String(t.clone()));
        }
        let description = obj
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| cap.description.clone());
        if let Some(d) = description {
            out.insert("description".into(), Value::String(d));
        }
        if let Some(s) = &cap.input_schema {
            out.insert("inputSchema".into(), s.clone());
        }

        // Tags = capability tags ++ exposure's own.
        let mut tags = cap.tags.clone();
        if let Some(local) = obj.get("tags").and_then(Value::as_array) {
            tags.extend(local.iter().cloned());
        }
        if !tags.is_empty() {
            out.insert("tags".into(), Value::Array(tags));
        }

        // Guards = capability guards ++ exposure's own.
        let mut guards = cap.guards.clone();
        if let Some(local) = obj.get("guards").and_then(Value::as_array) {
            guards.extend(local.iter().cloned());
        }
        if !guards.is_empty() {
            out.insert("guards".into(), Value::Array(guards));
        }

        // Reliability: exposure overrides if specified.
        let reliability = obj.get("reliability").cloned().or(cap.reliability.clone());
        if let Some(r) = reliability {
            out.insert("reliability".into(), r);
        }

        out.insert("executor".into(), cap.executor.clone());

        return Ok(Value::Object(out));
    }

    Ok(exposure)
}

// ---------- executor reference rewriting ----------------------------------

fn rewrite_executors_in_value(
    value: &mut Value,
    registry: &HashMap<String, NormalizedCapability>,
) -> anyhow::Result<()> {
    match value {
        Value::Object(map) => {
            // If `executor` is itself a capability ref, rewrite this object
            // to merge the capability's guards/reliability into the parent.
            if let Some(executor) = map.get("executor").cloned() {
                if let Some(cap_name) = executor
                    .as_object()
                    .and_then(|o| o.get("capability"))
                    .and_then(Value::as_str)
                {
                    let cap = registry.get(cap_name).ok_or_else(|| {
                        anyhow!("executor references unknown capability '{}'", cap_name)
                    })?;
                    map.insert("executor".into(), cap.executor.clone());

                    // Stack guards: capability first, then existing.
                    let existing_guards = map
                        .get("guards")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    let mut all_guards = cap.guards.clone();
                    all_guards.extend(existing_guards);
                    if !all_guards.is_empty() {
                        map.insert("guards".into(), Value::Array(all_guards));
                    }

                    // Reliability: parent (transition/etc.) wins if set, else
                    // capability's.
                    if !map.contains_key("reliability") {
                        if let Some(r) = &cap.reliability {
                            map.insert("reliability".into(), r.clone());
                        }
                    }

                    if !map.contains_key("inputSchema") {
                        if let Some(s) = &cap.input_schema {
                            map.insert("inputSchema".into(), s.clone());
                        }
                    }
                }
            }

            // Recurse into all children — covers transitions, onEnter,
            // fallback executors, etc.
            for child in map.values_mut() {
                rewrite_executors_in_value(child, registry)?;
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                rewrite_executors_in_value(v, registry)?;
            }
        }
        _ => {}
    }
    Ok(())
}
