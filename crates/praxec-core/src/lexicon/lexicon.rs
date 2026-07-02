//! SPEC §30 — Lexicon / Ubiquitous Language primitive (Tier 1).
//!
//! A persistent vocabulary store that workflows reach for via the
//! `gateway.lexicon.*` MCP tools. Each term carries a definition, an
//! optional bounded context (DDD), refs to related terms, and a
//! governance marker that defaults to `human-only`.
//!
//! ## Why a runtime primitive?
//!
//! A skill can extract terms via Socratic questioning. But to be
//! reusable across runs, the result needs a stable STORE that:
//!
//! 1. Snapshot-stamps onto in-flight workflows — same invariant as
//!    `_skillsLibrary` per SPEC §8.2. A workflow started before a term
//!    was redefined keeps its old understanding.
//! 2. Is searchable from any workflow via `gateway.lexicon.search`.
//! 3. Is human-governed by default — agents cannot silently drift
//!    vocabulary; they propose, humans accept.
//! 4. Is version-controllable — Tier 1 lives in `praxec.yaml`,
//!    operators commit + review via PR.
//!
//! ## Tier 1 — Per-config
//!
//! The top-level `lexicon:` block in `praxec.yaml` is the entire
//! store. At config-load, every workflow gets a stamped
//! `_lexiconLibrary` on its definition snapshot. The MCP tools read
//! from the snapshot (not the live config) so in-flight reads are
//! deterministic.
//!
//! Tier 2 (per-operator file store) and Tier 3 (multi-tenant DB)
//! follow the same shape; a `LexiconStore` trait is reserved but the
//! Tier 1 in-config form is the only one shipped today.

use std::collections::HashMap;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Map, Value};

/// Default governance level when a lexicon entry omits the field.
/// SPEC §30.6 — `human-only` is the load-bearing default. Agents
/// proposing definitions get rejected so vocabulary doesn't drift
/// silently. Operators opting into `agent-may-propose` are making
/// an explicit choice to accept faster iteration over discipline.
pub const DEFAULT_GOVERNANCE: &str = "human-only";

/// Alias to keep the collision-detection map type readable.
type ContextEntries<'a> = Vec<(&'a str, &'a Map<String, Value>)>;

/// Validate the top-level `lexicon:` block at config load. Catches:
/// - non-object entries
/// - missing `definition_short` field
/// - invalid `governance` value
/// - non-string `refs` entries
/// - same-bounded-context alias collisions (SPEC §30.10.1)
///
/// Surfaces `INVALID_LEXICON_ENTRY` or `LEXICON_ALIAS_COLLISION` with the
/// offending term(s) named.
pub fn validate_lexicon(config: &Value) -> Result<()> {
    let Some(lexicon) = config.get("lexicon").and_then(Value::as_object) else {
        return Ok(()); // no lexicon block is fine
    };
    for (term, entry) in lexicon {
        let entry_obj = entry.as_object().ok_or_else(|| {
            anyhow!(
                "INVALID_LEXICON_ENTRY: lexicon entry '{term}' must be an object \
                 with at least `definition_short:` set"
            )
        })?;
        let definition = entry_obj
            .get("definition_short")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                anyhow!(
                    "INVALID_LEXICON_ENTRY: lexicon entry '{term}' is missing the \
                     required `definition:` field (string)"
                )
            })?;
        if definition.trim().is_empty() {
            bail!(
                "INVALID_LEXICON_ENTRY: lexicon entry '{term}' has empty \
                 `definition:` — definitions must be substantive"
            );
        }
        if let Some(gov) = entry_obj.get("governance").and_then(Value::as_str) {
            if gov != "human-only" && gov != "agent-may-propose" {
                bail!(
                    "INVALID_LEXICON_ENTRY: lexicon entry '{term}' has unknown \
                     `governance: {gov}` — supported: `human-only` (default) | \
                     `agent-may-propose`"
                );
            }
        }
        if let Some(refs) = entry_obj.get("refs").and_then(Value::as_array) {
            for (i, r) in refs.iter().enumerate() {
                if !r.is_string() {
                    bail!(
                        "INVALID_LEXICON_ENTRY: lexicon entry '{term}' refs[{i}] is not \
                         a string — refs must be term names"
                    );
                }
            }
        }
    }

    // ── SPEC §30.10.1 — same-bounded-context alias collision detection ────
    //
    // Group entries by bounded_context (empty string = no context).
    // Within each group build the combined-form index; if any alias or
    // canonical term appears more than once → LEXICON_ALIAS_COLLISION.
    let mut by_context: HashMap<&str, ContextEntries<'_>> = HashMap::new();
    for (term, entry) in lexicon {
        if let Some(obj) = entry.as_object() {
            let ctx = obj
                .get("bounded_context")
                .and_then(Value::as_str)
                .unwrap_or("");
            by_context
                .entry(ctx)
                .or_default()
                .push((term.as_str(), obj));
        }
    }
    for (ctx, entries) in &by_context {
        build_combined_index_inner(entries, ctx)?;
    }
    Ok(())
}

/// Internal helper: build the combined-form index for a slice of entries
/// that share a bounded context. Returns an error on the first collision.
fn build_combined_index_inner<'a>(
    entries: &[(&'a str, &'a Map<String, Value>)],
    bounded_context: &str,
) -> Result<HashMap<&'a str, &'a Map<String, Value>>> {
    let mut index: HashMap<&str, (&str, &Map<String, Value>)> = HashMap::new();

    let register = |key: &'a str,
                    owner_term: &'a str,
                    owner_obj: &'a Map<String, Value>,
                    index: &mut HashMap<&'a str, (&'a str, &'a Map<String, Value>)>|
     -> Result<()> {
        if let Some((existing_term, _)) = index.get(key) {
            bail!(
                "LEXICON_ALIAS_COLLISION: within bounded_context '{bounded_context}', \
                 key '{key}' is claimed by both '{existing_term}' and '{owner_term}'. \
                 Aliases must be unique within a bounded context. (SPEC §30.10.1)"
            );
        }
        index.insert(key, (owner_term, owner_obj));
        Ok(())
    };

    for &(term, obj) in entries {
        register(term, term, obj, &mut index)?;
        if let Some(aliases) = obj.get("aliases").and_then(Value::as_array) {
            for alias_val in aliases {
                if let Some(alias) = alias_val.as_str() {
                    register(alias, term, obj, &mut index)?;
                }
            }
        }
    }
    Ok(index.into_iter().map(|(k, (_, v))| (k, v)).collect())
}

/// SPEC §30.10.1 — build the snapshot-time combined-form index for a
/// single bounded context. Returns a `HashMap<&str, &Map<String, Value>>`
/// keyed by canonical term + every alias, all pointing at the same entry
/// object. O(1) lookup against any surface form.
///
/// Call once per bounded context at snapshot-stamp time (or validation).
/// Returns `Err` on collision (same check as `validate_lexicon`).
pub fn build_combined_index<'a>(
    lexicon_obj: &'a Map<String, Value>,
    bounded_context: &str,
) -> Result<HashMap<&'a str, &'a Map<String, Value>>> {
    let entries: Vec<(&str, &Map<String, Value>)> = lexicon_obj
        .iter()
        .filter_map(|(k, v)| {
            let obj = v.as_object()?;
            let entry_ctx = obj
                .get("bounded_context")
                .and_then(Value::as_str)
                .unwrap_or("");
            if entry_ctx == bounded_context {
                Some((k.as_str(), obj))
            } else {
                None
            }
        })
        .collect();
    build_combined_index_inner(&entries, bounded_context)
}

/// SPEC §30.4 — stamp the full lexicon onto every workflow that exists
/// in the config. Mirrors `stamp_skills_library` (SPEC §8.2): every
/// in-flight workflow sees the lexicon as it existed at
/// `workflow.start` time, immune to mid-flight edits of the top-level
/// `lexicon:` block.
pub fn stamp_lexicon_library(config: &mut Value) {
    let Some(lexicon) = config.get("lexicon").cloned() else {
        return;
    };
    let Some(workflows) = config
        .pointer_mut("/workflows")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    for (_id, def) in workflows {
        if let Some(obj) = def.as_object_mut() {
            obj.insert("_lexiconLibrary".to_string(), lexicon.clone());
        }
    }
}

/// SPEC §30.5 — exact-term lookup against a workflow's stamped lexicon
/// library. Returns the entry value (`{definition, examples?, refs?,
/// bounded_context?, governance}`) or `None` when the term is absent.
pub fn lookup_term<'a>(
    workflow_definition: &'a Value,
    term: &str,
    bounded_context: Option<&str>,
) -> Option<&'a Value> {
    let lib = workflow_definition
        .get("_lexiconLibrary")
        .and_then(Value::as_object)?;
    let entry = lib.get(term)?;
    if let Some(filter_ctx) = bounded_context {
        let entry_ctx = entry
            .get("bounded_context")
            .and_then(Value::as_str)
            .unwrap_or("");
        if entry_ctx != filter_ctx {
            return None;
        }
    }
    Some(entry)
}

/// SPEC §30.5 — keyword search across the stamped lexicon library.
/// Substring match against term name + definition; optional
/// bounded_context filter; results limited to `limit` (default 10).
/// Returns a list of `{term, ...entry-fields}` objects in match order.
pub fn search_terms(
    workflow_definition: &Value,
    query: &str,
    bounded_context: Option<&str>,
    limit: Option<usize>,
) -> Vec<Value> {
    let limit = limit.unwrap_or(10);
    let Some(lib) = workflow_definition
        .get("_lexiconLibrary")
        .and_then(Value::as_object)
    else {
        return vec![];
    };
    let q_lower = query.to_lowercase();
    let mut hits: Vec<Value> = Vec::new();
    for (term, entry) in lib {
        if let Some(filter_ctx) = bounded_context {
            let entry_ctx = entry
                .get("bounded_context")
                .and_then(Value::as_str)
                .unwrap_or("");
            if entry_ctx != filter_ctx {
                continue;
            }
        }
        let term_match = term.to_lowercase().contains(&q_lower);
        let def_match = entry
            .get("definition_short")
            .and_then(Value::as_str)
            .map(|d| d.to_lowercase().contains(&q_lower))
            .unwrap_or(false);
        if term_match || def_match {
            let mut hit = entry.clone();
            if let Some(obj) = hit.as_object_mut() {
                obj.insert("term".to_string(), json!(term));
            }
            hits.push(hit);
            if hits.len() >= limit {
                break;
            }
        }
    }
    hits
}

/// SPEC §30.6 — governance check. Returns the governance level for a
/// term (defaults to `human-only` when absent). Used by the MCP
/// `gateway.lexicon.define` handler to gate agent writes.
pub fn governance_for(workflow_definition: &Value, term: &str) -> String {
    workflow_definition
        .get("_lexiconLibrary")
        .and_then(Value::as_object)
        .and_then(|lib| lib.get(term))
        .and_then(|entry| entry.get("governance"))
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_GOVERNANCE)
        .to_string()
}

/// SPEC §30.6 — whether a proposed write to `term` is allowed for the
/// given actor role. Agents calling `gateway.lexicon.define` against
/// a `human-only` term must be rejected with
/// `LEXICON_DEFINE_REQUIRES_HUMAN`. Humans always pass; agents only
/// pass against `agent-may-propose` terms.
///
/// `actor_is_human` reflects whether the calling principal has the
/// `human` role (same gate the existing `actor: human` transition
/// machinery uses).
pub fn define_allowed(
    workflow_definition: &Value,
    term: &str,
    actor_is_human: bool,
) -> Result<(), String> {
    if actor_is_human {
        return Ok(());
    }
    let governance = governance_for(workflow_definition, term);
    if governance == "agent-may-propose" {
        Ok(())
    } else {
        Err(format!(
            "LEXICON_DEFINE_REQUIRES_HUMAN: term '{term}' has governance \
             '{governance}'; an agent attempted to define it. Route through an \
             actor: human transition to commit. (SPEC §30.6)"
        ))
    }
}

/// SPEC §30.10.3 — walk all subject reference sites in `config` and return
/// the subject portion (everything after the first dot) for each site.
///
/// Reference sites covered:
/// - `scripts:` block keys   (e.g. `build.cargo.release` → subject `cargo.release`)
/// - `skills:`  block keys   (e.g. `plan.evidence-foo` → subject `evidence-foo`)
/// - `executor: { kind: script, subject: <name> }` inside workflows (subject is already
///   a full verb-subject key; extract the post-first-dot portion)
/// - `executor: { kind: skill, subject: <name> }` likewise (skill executors)
///
/// The verb-subject split: take everything after the first `.` separator.
/// For `build.cargo.release` that is `cargo.release`; for `plan.foo` that is `foo`.
/// If there is no `.` (should not happen after `validate_skills` / `validate_scripts`,
/// which require at least two dotted segments), the whole key is returned as-is.
///
/// Returns a `Vec<String>` of subject portions (may contain duplicates across sites).
pub fn walk_all_subject_references(config: &Value) -> Vec<String> {
    let mut subjects = Vec::new();

    // 1. scripts: block keys.
    if let Some(scripts) = config.pointer("/scripts").and_then(Value::as_object) {
        for key in scripts.keys() {
            subjects.push(subject_portion(key));
        }
    }

    // 2. skills: block keys.
    if let Some(skills) = config.pointer("/skills").and_then(Value::as_object) {
        for key in skills.keys() {
            subjects.push(subject_portion(key));
        }
    }

    // 3. capabilities: block keys (SPEC §30.10.2).
    //    Only keys that follow `verb.subject` form (contain `.`) are lexicon
    //    subjects. Simple capability names like `do_thing` are not subject
    //    references — they're capability handles that don't require a lexicon
    //    entry.
    if let Some(caps) = config.pointer("/capabilities").and_then(Value::as_object) {
        for key in caps.keys() {
            if key.contains('.') {
                subjects.push(subject_portion(key));
            }
        }
    }

    // 4. Workflow executors: kind=script or kind=skill with a `subject:` field,
    //    and `system:` keys inside executor `map:` objects.
    if let Some(workflows) = config.pointer("/workflows").and_then(Value::as_object) {
        for wf_def in workflows.values() {
            walk_executor_subjects_in_value(wf_def, &mut subjects);
        }
    }

    subjects
}

/// Extract the subject portion from a verb-subject key by dropping everything
/// up to and including the first `.`. If there is no `.`, returns the whole key.
fn subject_portion(key: &str) -> String {
    match key.split_once('.') {
        Some((_, rest)) => rest.to_string(),
        None => key.to_string(),
    }
}

/// Public wrapper around [`subject_portion`] for callers outside this module
/// (e.g. `config.rs`) that need to extract the subject portion from a
/// verb-subject key. Named distinctly to keep the internal `subject_portion`
/// a private detail.
pub fn subject_portion_pub(key: &str) -> String {
    subject_portion(key)
}

/// Recursively walk a JSON value looking for executor objects with
/// `kind: "script"` or `kind: "skill"` and a `subject:` field, plus
/// any `system:` key in executor `map:` objects whose value is a
/// `<verb>.<subject>` string (SPEC §30.10.3).
/// Pushes the subject-portion (post-first-dot) into `out`.
fn walk_executor_subjects_in_value(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            let kind = map.get("kind").and_then(Value::as_str);
            if matches!(kind, Some("script") | Some("skill")) {
                // Direct executor object: extract subject and stop recursing
                // into *this* object to avoid double-counting via the child walk.
                if let Some(subj) = map.get("subject").and_then(Value::as_str) {
                    out.push(subject_portion(subj));
                }
                // Still recurse into children (e.g. nested structures within the
                // executor body), but skip the fields we already consumed.
                for (key, child) in map {
                    if key == "subject" {
                        continue; // already handled above
                    }
                    walk_executor_subjects_in_value(child, out);
                }
                return;
            }

            // Check for an `executor:` wrapper (onEnter shape). Extract from
            // the wrapper directly so we don't double-count via the child walk.
            if let Some(exec) = map.get("executor").filter(|v| v.is_object()) {
                let exec_kind = exec.pointer("/kind").and_then(Value::as_str);
                if matches!(exec_kind, Some("script") | Some("skill")) {
                    if let Some(subj) = exec.pointer("/subject").and_then(Value::as_str) {
                        out.push(subject_portion(subj));
                    }
                    // Recurse into the rest of the current map (skipping `executor`
                    // which we've handled) to find nested subjects.
                    for (key, child) in map {
                        if key == "executor" {
                            continue;
                        }
                        walk_executor_subjects_in_value(child, out);
                    }
                    return;
                }
            }

            // Extract subjects from `system:` keys in executor map: objects.
            // A `system: "<verb>.<subject>"` string is a skill subject reference.
            if let Some(system_val) = map.get("system").and_then(Value::as_str) {
                if system_val.contains('.') {
                    out.push(subject_portion(system_val));
                }
            }

            // General recursion for all other object shapes.
            for child in map.values() {
                walk_executor_subjects_in_value(child, out);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                walk_executor_subjects_in_value(v, out);
            }
        }
        _ => {}
    }
}

/// SPEC §30.10.3 — inject `PENDING_DEFINITION` placeholder entries into the
/// lexicon snapshot on each workflow definition.
///
/// For every subject referenced in `config` (via `walk_all_subject_references`)
/// that is NOT already present in the authored `lexicon:` block, we add a
/// placeholder entry to each workflow's `_lexiconLibrary` snapshot:
///
/// ```json
/// {
///   "state": "PENDING_DEFINITION",
///   "governance": "human-only"
/// }
/// ```
///
/// Placeholders skip `validate_lexicon`'s `definition_short` requirement
/// because they are runtime-created, not author-created.
///
/// The placeholder marks that the subject is referenced but undefined; Task 3.3
/// will use this to block workflow execution at runtime. For now, doctor
/// surfaces them as informational warnings.
///
/// `extra_subjects` carries subjects collected from parts of the config that
/// were stripped before this function runs (e.g. the `capabilities:` block
/// which is removed at resolve step 4). Those subjects are merged into the
/// pending detection walk so capability-block subjects are not invisible to
/// the injector.
///
/// Returns the set of pending subject names (for use by callers like doctor).
pub fn inject_pending_definitions(config: &mut Value, extra_subjects: &[String]) -> Vec<String> {
    // Collect all referenced subjects (from current config + pre-strip extras).
    let mut all_subjects = walk_all_subject_references(config);
    all_subjects.extend_from_slice(extra_subjects);

    // Collect all authored lexicon keys (the "registered" ones).
    let authored_keys: std::collections::HashSet<String> = config
        .pointer("/lexicon")
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();

    // SPEC §30.10.3 (relaxation): a subject that is itself DEFINED as a
    // script/skill/capability is resolved by that registration — it does not
    // also require a separate lexicon glossary entry. Without this, every
    // referenced script/skill/capability subject (e.g. `cargo.release`,
    // `tdd.discipline`) is flagged PENDING_DEFINITION even though the thing it
    // names is loaded, which makes any repo unusable until ~100 glossary
    // entries are hand-authored. The lexicon gate keeps its real value
    // (catching genuinely-unknown vocabulary) while no longer false-flagging
    // loaded definitions.
    let mut defined_subjects: std::collections::HashSet<String> = std::collections::HashSet::new();
    for block in ["/scripts", "/skills"] {
        if let Some(obj) = config.pointer(block).and_then(Value::as_object) {
            for key in obj.keys() {
                defined_subjects.insert(subject_portion(key));
            }
        }
    }
    // Capability subjects were captured before the `capabilities:` block was
    // stripped (config.rs step 4) and arrive via `extra_subjects`; they are
    // definitions, not references.
    for s in extra_subjects {
        defined_subjects.insert(s.clone());
    }

    // Find subjects that are neither lexicon-authored nor a loaded definition.
    let mut pending: Vec<String> = all_subjects
        .into_iter()
        .filter(|s| !authored_keys.contains(s.as_str()) && !defined_subjects.contains(s.as_str()))
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    pending.sort(); // deterministic order for tests

    if pending.is_empty() {
        return Vec::new();
    }

    // Inject placeholders into each workflow's _lexiconLibrary.
    let placeholder_entry = json!({
        "state": "PENDING_DEFINITION",
        "governance": DEFAULT_GOVERNANCE
    });

    if let Some(workflows) = config
        .pointer_mut("/workflows")
        .and_then(Value::as_object_mut)
    {
        for def in workflows.values_mut() {
            let Some(obj) = def.as_object_mut() else {
                continue;
            };
            // Only inject into workflows that have a _lexiconLibrary stamped.
            // If no _lexiconLibrary exists yet, create one (the workflow may have
            // no authored entries to merge with).
            let lib = obj
                .entry("_lexiconLibrary".to_string())
                .or_insert_with(|| Value::Object(serde_json::Map::new()))
                .as_object_mut();
            if let Some(lib_map) = lib {
                for subject in &pending {
                    // Don't overwrite an authored (resolved) entry.
                    if !lib_map.contains_key(subject.as_str()) {
                        lib_map.insert(subject.clone(), placeholder_entry.clone());
                    }
                }
            }
        }
    }

    pending
}

/// SPEC §30.10.3 — scan the `_lexiconLibrary` of any workflow in an already-
/// resolved config and return the list of subjects whose entries carry
/// `state: "PENDING_DEFINITION"`. This is the post-resolve view used by
/// doctor; it reads the data that `inject_pending_definitions` already wrote.
///
/// Returns a sorted, deduplicated list of pending subject names.
pub fn pending_subjects_from_resolved(config: &Value) -> Vec<String> {
    let mut pending = std::collections::BTreeSet::new();
    if let Some(workflows) = config.pointer("/workflows").and_then(Value::as_object) {
        for def in workflows.values() {
            if let Some(lib) = def.get("_lexiconLibrary").and_then(Value::as_object) {
                for (term, entry) in lib {
                    if entry.get("state").and_then(Value::as_str) == Some("PENDING_DEFINITION") {
                        pending.insert(term.clone());
                    }
                }
            }
        }
    }
    pending.into_iter().collect()
}

/// SPEC §30.5 / §30.10.1 — build a proposed entry value from
/// `gateway.lexicon.define` arguments. Uses `definition_short` as the
/// primary one-sentence definition field. Used by the MCP handler before
/// persisting; centralized so the shape is consistent and validation runs
/// in one place.
///
/// `embedding` — when `Some`, stores the vector as `_embedding` on the entry
/// so Tier 3 semantic candidate ranking can compare it against unknown
/// subjects. Callers are responsible for computing the vector before calling
/// this function (e.g. via `embeddings::EmbeddingProvider::embed`).
pub fn build_entry(
    definition_short: &str,
    bounded_context: Option<&str>,
    refs: Option<&Vec<String>>,
    governance: Option<&str>,
    embedding: Option<Vec<f32>>,
) -> Result<Value> {
    if definition_short.trim().is_empty() {
        bail!("INVALID_LEXICON_ENTRY: definition must be non-empty");
    }
    let mut entry = Map::new();
    entry.insert("definition_short".into(), json!(definition_short));
    if let Some(ctx) = bounded_context {
        entry.insert("bounded_context".into(), json!(ctx));
    }
    if let Some(rs) = refs {
        entry.insert("refs".into(), json!(rs));
    }
    let gov = governance.unwrap_or(DEFAULT_GOVERNANCE);
    if gov != "human-only" && gov != "agent-may-propose" {
        bail!(
            "INVALID_LEXICON_ENTRY: governance must be `human-only` or \
             `agent-may-propose`; got '{gov}'"
        );
    }
    entry.insert("governance".into(), json!(gov));
    if let Some(vec) = embedding {
        // Store as a JSON array; the `_` prefix marks it as a runtime-internal
        // field (not authored by operators, not validated by the lexicon schema).
        entry.insert("_embedding".into(), json!(vec));
    }
    Ok(Value::Object(entry))
}
