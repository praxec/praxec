//! Per-repo manifest loading (SPEC §9, capability/flow composition design §9).
//!
//! Each resource repo ships a `praxec.repo.yaml` at its root declaring
//! a `namespace`, a `version`, and a `layout` of directories where
//! capabilities, flows, skills, scripts, and connections live.
//!
//! Gateway configs reference repos via a top-level `repos:` array. At
//! config-load, every YAML under each repo's layout directories is loaded
//! and its top-level `workflows:` / `skills:` / `scripts:` / `connections:`
//! entries are merged into the gateway registry, with every key prefixed
//! `<namespace>/<id>`. See `config::load_repos` for the integration site.
//!
//! This module owns the manifest schema + loader. Namespace-prefixing
//! lives in `config.rs` where it can reuse `deep_merge`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::Deserialize;
use serde_json::{Map, Value};

/// The expected value of the manifest's `schema` field. Loaders refuse any
/// manifest whose `schema` is not exactly this string — forward-incompatible
/// schema bumps will introduce new constants (e.g. `praxec.repo/v2`) so
/// older gateways can refuse rather than silently mis-parse.
pub const REPO_MANIFEST_SCHEMA_V1: &str = "praxec.repo/v1";

/// Parsed `praxec.repo.yaml` manifest. See `schemas/praxec-repo.schema.json`
/// for the canonical schema.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoManifest {
    /// MUST equal [`REPO_MANIFEST_SCHEMA_V1`].
    pub schema: String,
    /// Human-readable repo identifier; lowercase-kebab.
    pub name: String,
    /// Single-segment prefix applied to every definitionId loaded from this
    /// repo. Two repos declaring the same `namespace` fail at config-load.
    pub namespace: String,
    /// Repo version, semver-shaped by convention. Surfaced via
    /// `gateway.describe`.
    pub version: String,
    /// Free-form description; surfaced via `gateway.describe`.
    #[serde(default)]
    pub description: Option<String>,
    /// Per-tier directory locations. Each field defaults to the directory
    /// name matching the field name.
    #[serde(default)]
    pub layout: RepoLayout,
}

/// Layout of resource directories within a repo. All fields are optional;
/// defaults match the directory names exactly (e.g. `capabilities/`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoLayout {
    #[serde(default = "default_capabilities_dir")]
    pub capabilities: String,
    #[serde(default = "default_flows_dir")]
    pub flows: String,
    #[serde(default = "default_skills_dir")]
    pub skills: String,
    #[serde(default = "default_scripts_dir")]
    pub scripts: String,
    #[serde(default = "default_connections_dir")]
    pub connections: String,
}

impl Default for RepoLayout {
    fn default() -> Self {
        Self {
            capabilities: default_capabilities_dir(),
            flows: default_flows_dir(),
            skills: default_skills_dir(),
            scripts: default_scripts_dir(),
            connections: default_connections_dir(),
        }
    }
}

fn default_capabilities_dir() -> String {
    "capabilities".to_string()
}
fn default_flows_dir() -> String {
    "flows".to_string()
}
fn default_skills_dir() -> String {
    "skills".to_string()
}
fn default_scripts_dir() -> String {
    "scripts".to_string()
}
fn default_connections_dir() -> String {
    "connections".to_string()
}

/// Load and validate a `praxec.repo.yaml` from the given repo root.
/// The path argument is the repo directory; the manifest is read from
/// `<root>/praxec.repo.yaml`.
///
/// Errors at:
/// - missing manifest file
/// - YAML parse failure
/// - `schema` not equal to [`REPO_MANIFEST_SCHEMA_V1`]
/// - unknown fields (manifest uses `deny_unknown_fields`)
pub fn load_manifest(repo_root: &Path) -> anyhow::Result<RepoManifest> {
    let manifest_path: PathBuf = repo_root.join("praxec.repo.yaml");
    let text = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("reading repo manifest {}", manifest_path.display()))?;
    let manifest: RepoManifest = serde_yaml::from_str(&text)
        .with_context(|| format!("parsing repo manifest {}", manifest_path.display()))?;
    if manifest.schema != REPO_MANIFEST_SCHEMA_V1 {
        bail!(
            "repo manifest {} declares schema `{}`; expected `{}`",
            manifest_path.display(),
            manifest.schema,
            REPO_MANIFEST_SCHEMA_V1
        );
    }
    Ok(manifest)
}

/// Top-level config blocks whose entries are subject to namespace prefixing
/// when loaded from a repo. Order matters only for stable error messages;
/// the merged value is order-independent.
const PREFIXABLE_BLOCKS: &[&str] = &["workflows", "skills", "scripts", "connections", "schemas"];

/// Load a repo: parse its `praxec.repo.yaml`, walk every layout directory,
/// merge every `*.yaml` file's top-level `workflows:` / `skills:` / `scripts:` /
/// `connections:` block into a single aggregate Value with every entry key
/// prefixed `<namespace>/<id>`. Inside repo-loaded workflows, also rewrite
/// `kind: workflow` `definitionId:` references so unprefixed names bind to
/// the current namespace (the "current namespace" resolution rule from
/// spec §9.3).
///
/// Returns the manifest plus an aggregate Value shaped like a gateway
/// config fragment — caller deep-merges it into the host config.
///
/// Errors at:
/// - manifest load (see [`load_manifest`])
/// - YAML parse failure on any layout-dir file
/// - duplicate prefixed id within this repo (V21)
/// - unsupported top-level block in a layout-dir file
pub fn load_repo(repo_path: &Path) -> anyhow::Result<(RepoManifest, Value)> {
    let manifest = load_manifest(repo_path)?;
    let ns = &manifest.namespace;
    let mut aggregate = Value::Object(Map::new());

    for layout_dir in [
        &manifest.layout.capabilities,
        &manifest.layout.flows,
        &manifest.layout.skills,
        &manifest.layout.scripts,
        &manifest.layout.connections,
    ] {
        let dir_path = repo_path.join(layout_dir);
        if !dir_path.is_dir() {
            // A repo may legitimately ship only some of the layout tiers
            // (e.g. a `connections-only` repo). Missing dirs are silent.
            continue;
        }
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir_path)
            .with_context(|| format!("reading repo layout dir {}", dir_path.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("yaml"))
            .collect();
        // Deterministic order so duplicate-id errors are reproducible.
        entries.sort();
        for file_path in entries {
            let text = std::fs::read_to_string(&file_path)
                .with_context(|| format!("reading repo file {}", file_path.display()))?;
            let value: Value = serde_yaml::from_str(&text)
                .with_context(|| format!("parsing YAML {}", file_path.display()))?;
            merge_repo_file(&mut aggregate, value, ns, &file_path)?;
        }
    }
    // Surface the silent-drop footgun: definition files placed in a DEFAULT tier
    // directory that this manifest has REMAPPED elsewhere are never scanned. Warn
    // (loudly, at every load — `serve` and `check` both route through here) rather
    // than let authored YAML vanish with no feedback.
    for w in unscanned_definition_warnings(repo_path, &manifest.layout) {
        tracing::warn!(repo = %manifest.name, "{w}");
    }
    Ok((manifest, aggregate))
}

/// Detect definition files that sit in a DEFAULT tier directory (e.g. `flows/`)
/// which the manifest has remapped to a different directory (e.g. `orchestrators/`).
/// Those files are silently unscanned by [`load_repo`] — a footgun that lets
/// authored (or hand-written) definitions vanish with no error. Returns one
/// human-readable warning per remapped tier whose default dir still holds YAML.
///
/// Only the *remapped* case is flagged: if a tier uses its default directory,
/// nothing is remapped and there is nothing to warn about; a malformed file
/// *inside* the configured directory is a hard error in [`merge_repo_file`].
pub fn unscanned_definition_warnings(repo_path: &Path, layout: &RepoLayout) -> Vec<String> {
    let tiers: [(&str, String, &String); 5] = [
        (
            "capabilities",
            default_capabilities_dir(),
            &layout.capabilities,
        ),
        ("flows", default_flows_dir(), &layout.flows),
        ("skills", default_skills_dir(), &layout.skills),
        ("scripts", default_scripts_dir(), &layout.scripts),
        (
            "connections",
            default_connections_dir(),
            &layout.connections,
        ),
    ];
    let mut warnings = Vec::new();
    for (tier, default_dir, configured) in tiers {
        if configured == &default_dir {
            continue; // tier uses its default dir — nothing remapped.
        }
        let default_path = repo_path.join(&default_dir);
        if !default_path.is_dir() {
            continue;
        }
        let yaml_count = std::fs::read_dir(&default_path)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("yaml"))
            .count();
        if yaml_count > 0 {
            warnings.push(format!(
                "UNSCANNED_DEFINITION_DIR: `{default_dir}/` holds {yaml_count} YAML file(s) but \
                 this repo's manifest maps the `{tier}` tier to `{configured}/` — files under \
                 `{default_dir}/` are NOT loaded. Move them to `{configured}/` (or fix the \
                 manifest `layout`)."
            ));
        }
    }
    warnings
}

/// Merge one repo YAML file's contents into the aggregate Value. Prefix
/// every entry key under any [`PREFIXABLE_BLOCKS`] block with `<ns>/`, and
/// rewrite `kind: workflow` `definitionId:` refs inside workflow bodies.
fn merge_repo_file(
    aggregate: &mut Value,
    file_value: Value,
    namespace: &str,
    file_path: &Path,
) -> anyhow::Result<()> {
    let Value::Object(top) = file_value else {
        bail!(
            "repo file {} must be a YAML mapping at the top level",
            file_path.display()
        );
    };
    let agg_obj = aggregate
        .as_object_mut()
        .expect("aggregate is constructed as an object");
    for (block_key, block_value) in top {
        // SPEC §9.2 — silently skip harmless top-level metadata keys
        // (`version:`, `include:`, `description:`) that legacy files
        // commonly carry. Hard-error only on keys that look like they
        // intended to declare a block we don't know how to namespace-
        // prefix.
        const HARMLESS_TOP_LEVEL_KEYS: &[&str] = &["version", "include", "description", "metadata"];
        if HARMLESS_TOP_LEVEL_KEYS.contains(&block_key.as_str()) {
            continue;
        }
        if !PREFIXABLE_BLOCKS.contains(&block_key.as_str()) {
            bail!(
                "repo file {} has unsupported top-level key `{}`. Repo files may declare \
                 only {:?} blocks (SPEC §9.2).",
                file_path.display(),
                block_key,
                PREFIXABLE_BLOCKS
            );
        }
        let Value::Object(entries) = block_value else {
            bail!(
                "repo file {} block `{}:` must be a mapping",
                file_path.display(),
                block_key
            );
        };
        let agg_block = agg_obj
            .entry(block_key.clone())
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .expect("just inserted as object");
        for (id, mut def) in entries {
            let prefixed = prefix_id(namespace, &id);
            if block_key == "workflows" {
                rewrite_workflow_refs(&mut def, namespace);
            }
            if agg_block.contains_key(&prefixed) {
                // V21 — duplicate definitionId within one repo.
                bail!(
                    "DUPLICATE_REPO_DEF: repo namespace '{}' defines '{}' more than once \
                     (last seen in {}). Each definitionId may appear at most once per repo \
                     (SPEC §9.4).",
                    namespace,
                    prefixed,
                    file_path.display()
                );
            }
            agg_block.insert(prefixed, def);
        }
    }
    Ok(())
}

/// Prefix `id` with `<namespace>/` unless it is already namespace-qualified
/// (contains `/`). A repo may legitimately re-export a foreign id by writing
/// it fully qualified.
fn prefix_id(namespace: &str, id: &str) -> String {
    if id.contains('/') {
        id.to_string()
    } else {
        format!("{}/{}", namespace, id)
    }
}

/// SPEC §9.3 — inside a repo-loaded workflow, rewrite every intra-repo
/// reference from an unprefixed name (`cap.plan.vet`) to the current-namespace
/// form (`swe/cap.plan.vet`). Three reference kinds bind to sibling repo
/// entries and so get prefixed:
///   - `kind: workflow` executor `definitionId:` (sub-workflow composition)
///   - `skills: [...]` arrays (workflow / state / transition level)
///   - `kind: script` executor `subject:` (curated-script reference)
///
/// Fully qualified refs (`other-ns/cap.plan.vet`) pass through unchanged.
/// Connection refs (`kind: mcp` `connection:`) are deliberately NOT rewritten:
/// they bind to gateway-level `connections:`, not repo entries.
///
/// Rewriting all three at load keeps the snapshot key space consistent: the
/// reference the cap carries matches the namespaced key the entry lands under
/// in `/skills` / `/scripts`, so `_skillsLibrary` / `_scriptsLibrary` stamping
/// (exact-match) and the agent/script executors (which look up by the cap's
/// own reference string) all agree. Without this, a bare `skills:` ref never
/// resolves to its namespaced library entry → `AGENT_SKILL_SUBJECT_UNKNOWN` /
/// `SCRIPT_NOT_IN_SNAPSHOT` at run time even though `check` (which strips the
/// namespace prefix to validate) passes.
///
/// Recursive walk: refs may appear in workflow-level `onEnter`, state-level
/// `onEnter`, transitions, fallback executors, and arbitrarily nested
/// executors. Cheap by design — workflow bodies are small.
pub(crate) fn rewrite_workflow_refs(value: &mut Value, namespace: &str) {
    match value {
        Value::Object(map) => {
            // Direct executor block: { kind: workflow, definitionId: ... }
            let is_workflow_executor = map.get("kind").and_then(Value::as_str) == Some("workflow");
            if is_workflow_executor {
                if let Some(Value::String(id)) = map.get_mut("definitionId") {
                    if !id.contains('/') {
                        *id = format!("{}/{}", namespace, id);
                    }
                }
            }
            // Script executor block: { kind: script, subject: ... }
            let is_script_executor = map.get("kind").and_then(Value::as_str) == Some("script");
            if is_script_executor {
                if let Some(Value::String(id)) = map.get_mut("subject") {
                    if !id.contains('/') {
                        *id = format!("{}/{}", namespace, id);
                    }
                }
            }
            // Skill references: `skills: [bare, ns/qualified, ...]`. A skills
            // array is always a list of subject strings wherever it appears.
            if let Some(Value::Array(skills)) = map.get_mut("skills") {
                for s in skills.iter_mut() {
                    if let Value::String(id) = s {
                        if !id.contains('/') {
                            *id = format!("{}/{}", namespace, id);
                        }
                    }
                }
            }
            for child in map.values_mut() {
                rewrite_workflow_refs(child, namespace);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                rewrite_workflow_refs(v, namespace);
            }
        }
        _ => {}
    }
}

/// Collect every fully-qualified definitionId provided by a repo (the keys
/// of every prefixable block in the aggregate Value). Used by the host-side
/// loader to seed V20 (namespace uniqueness via separate map) and V23
/// (anonymous-shadowing) checks.
pub fn aggregate_ids(aggregate: &Value) -> HashSet<String> {
    let mut out = HashSet::new();
    let Some(obj) = aggregate.as_object() else {
        return out;
    };
    for block in PREFIXABLE_BLOCKS {
        if let Some(entries) = obj.get(*block).and_then(Value::as_object) {
            for k in entries.keys() {
                out.insert(k.clone());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn write_manifest(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("praxec.repo.yaml");
        std::fs::write(&p, body).unwrap();
        p
    }

    fn write_file(dir: &Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, body).unwrap();
    }

    #[test]
    fn load_manifest_accepts_minimal_well_formed_manifest() {
        let td = TempDir::new().unwrap();
        write_manifest(
            td.path(),
            "schema: praxec.repo/v1\nname: swe-core\nnamespace: swe\nversion: 0.6.0\n",
        );
        let m = load_manifest(td.path()).expect("manifest should load");
        assert_eq!(m.schema, REPO_MANIFEST_SCHEMA_V1);
        assert_eq!(m.name, "swe-core");
        assert_eq!(m.namespace, "swe");
        assert_eq!(m.version, "0.6.0");
        assert_eq!(m.layout.capabilities, "capabilities");
        assert_eq!(m.layout.flows, "flows");
    }

    #[test]
    fn load_manifest_rejects_wrong_schema_constant() {
        let td = TempDir::new().unwrap();
        write_manifest(
            td.path(),
            "schema: praxec.repo/v2\nname: swe-core\nnamespace: swe\nversion: 0.6.0\n",
        );
        let err = load_manifest(td.path()).expect_err("schema mismatch should error");
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("praxec.repo/v1"),
            "error should mention v1: {msg}"
        );
        assert!(
            msg.contains("praxec.repo/v2"),
            "error should mention actual: {msg}"
        );
    }

    #[test]
    fn load_manifest_rejects_unknown_top_level_field() {
        let td = TempDir::new().unwrap();
        write_manifest(
            td.path(),
            "schema: praxec.repo/v1\nname: swe-core\nnamespace: swe\nversion: 0.6.0\nbogus: hi\n",
        );
        load_manifest(td.path()).expect_err("unknown field should error");
    }

    #[test]
    fn load_manifest_accepts_partial_layout_with_defaults_for_rest() {
        let td = TempDir::new().unwrap();
        write_manifest(
            td.path(),
            "schema: praxec.repo/v1\nname: swe-core\nnamespace: swe\nversion: 0.6.0\nlayout:\n  capabilities: caps\n",
        );
        let m = load_manifest(td.path()).expect("partial layout should load");
        assert_eq!(m.layout.capabilities, "caps");
        assert_eq!(m.layout.flows, "flows");
        assert_eq!(m.layout.skills, "skills");
    }

    #[test]
    fn load_manifest_errors_when_file_missing() {
        let td = TempDir::new().unwrap();
        let err = load_manifest(td.path()).expect_err("missing file should error");
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("praxec.repo.yaml"),
            "error should mention file: {msg}"
        );
    }

    fn minimal_manifest(namespace: &str) -> String {
        format!(
            "schema: praxec.repo/v1\nname: {namespace}-core\nnamespace: {namespace}\nversion: 0.6.0\n"
        )
    }

    #[test]
    fn load_repo_prefixes_workflow_ids_with_namespace() {
        let td = TempDir::new().unwrap();
        write_manifest(td.path(), &minimal_manifest("swe"));
        write_file(
            td.path(),
            "capabilities/cap.plan.vet.yaml",
            "workflows:\n  cap.plan.vet:\n    title: Plan vet\n",
        );

        let (manifest, agg) = load_repo(td.path()).expect("repo loads");
        assert_eq!(manifest.namespace, "swe");
        let workflows = agg
            .pointer("/workflows")
            .and_then(Value::as_object)
            .expect("workflows present");
        assert!(
            workflows.contains_key("swe/cap.plan.vet"),
            "got keys: {:?}",
            workflows.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn load_repo_merges_files_across_layout_dirs() {
        let td = TempDir::new().unwrap();
        write_manifest(td.path(), &minimal_manifest("swe"));
        write_file(
            td.path(),
            "capabilities/cap.plan.vet.yaml",
            "workflows:\n  cap.plan.vet:\n    title: Vet\n",
        );
        write_file(
            td.path(),
            "flows/flow.add-feature.yaml",
            "workflows:\n  flow.add-feature:\n    title: Add feature\n",
        );
        write_file(
            td.path(),
            "skills/sk.plan.specify.change-request.yaml",
            "skills:\n  sk.plan.specify.change-request:\n    verb: specify\n    lifecycle: stable\n    body: hi\n",
        );

        let (_m, agg) = load_repo(td.path()).expect("repo loads");
        let workflows = agg
            .pointer("/workflows")
            .and_then(Value::as_object)
            .unwrap();
        assert!(workflows.contains_key("swe/cap.plan.vet"));
        assert!(workflows.contains_key("swe/flow.add-feature"));
        let skills = agg.pointer("/skills").and_then(Value::as_object).unwrap();
        assert!(skills.contains_key("swe/sk.plan.specify.change-request"));
    }

    #[test]
    fn load_repo_rewrites_unprefixed_workflow_refs_to_current_namespace() {
        let td = TempDir::new().unwrap();
        write_manifest(td.path(), &minimal_manifest("swe"));
        write_file(
            td.path(),
            "flows/flow.add-feature.yaml",
            r#"
workflows:
  flow.add-feature:
    title: Add feature
    states:
      planning:
        transitions:
          plan_drafted:
            target: vetting
            executor:
              kind: workflow
              definitionId: cap.plan.vet
          plan_external:
            target: vetting
            executor:
              kind: workflow
              definitionId: quality/cap.plan.vet
"#,
        );

        let (_m, agg) = load_repo(td.path()).expect("repo loads");
        let vet_def_id = agg.pointer(
            "/workflows/swe~1flow.add-feature/states/planning/transitions/plan_drafted/executor/definitionId",
        ).and_then(Value::as_str).expect("ref should be present");
        assert_eq!(
            vet_def_id, "swe/cap.plan.vet",
            "unprefixed ref should rewrite to current namespace"
        );
        let external = agg.pointer(
            "/workflows/swe~1flow.add-feature/states/planning/transitions/plan_external/executor/definitionId",
        ).and_then(Value::as_str).expect("external ref should be present");
        assert_eq!(
            external, "quality/cap.plan.vet",
            "fully-qualified ref should pass through unchanged"
        );
    }

    #[test]
    fn load_repo_rewrites_skill_and_script_refs_but_not_connections() {
        let td = TempDir::new().unwrap();
        write_manifest(td.path(), &minimal_manifest("swe"));
        write_file(
            td.path(),
            "capabilities/cap.plan.elicit-spec.yaml",
            r#"
workflows:
  cap.plan.elicit-spec:
    verb: plan
    skills: [plan.elicit.structured-interview, other-ns/foo.skill]
    states:
      interviewing:
        transitions:
          submit_spec:
            target: done
            actor: agent
            executor: { kind: noop }
          scaffold:
            target: done
            actor: deterministic
            executor:
              kind: script
              subject: run.scaffold.cargo-mcp
          schedule:
            target: done
            actor: deterministic
            executor:
              kind: mcp
              connection: cpm-planner
              tool: plan.submit
      done: { terminal: true }
"#,
        );

        let (_m, agg) = load_repo(td.path()).expect("repo loads");
        let base = "/workflows/swe~1cap.plan.elicit-spec";

        // skills: bare → namespaced; fully-qualified passes through.
        let skills = agg
            .pointer(&format!("{base}/skills"))
            .and_then(Value::as_array)
            .expect("skills array present");
        assert_eq!(
            skills[0].as_str(),
            Some("swe/plan.elicit.structured-interview")
        );
        assert_eq!(skills[1].as_str(), Some("other-ns/foo.skill"));

        // script executor subject: bare → namespaced.
        let subject = agg
            .pointer(&format!(
                "{base}/states/interviewing/transitions/scaffold/executor/subject"
            ))
            .and_then(Value::as_str)
            .expect("script subject present");
        assert_eq!(subject, "swe/run.scaffold.cargo-mcp");

        // mcp connection: NOT rewritten — binds to gateway-level connections.
        let connection = agg
            .pointer(&format!(
                "{base}/states/interviewing/transitions/schedule/executor/connection"
            ))
            .and_then(Value::as_str)
            .expect("connection present");
        assert_eq!(connection, "cpm-planner", "connection ref must stay bare");
    }

    #[test]
    fn load_repo_errors_on_duplicate_id_within_same_repo() {
        let td = TempDir::new().unwrap();
        write_manifest(td.path(), &minimal_manifest("swe"));
        write_file(
            td.path(),
            "capabilities/a.yaml",
            "workflows:\n  cap.plan.vet:\n    title: A\n",
        );
        write_file(
            td.path(),
            "capabilities/b.yaml",
            "workflows:\n  cap.plan.vet:\n    title: B (collides)\n",
        );

        let err = load_repo(td.path()).expect_err("duplicate id should error");
        let msg = format!("{:#}", err);
        assert!(msg.contains("DUPLICATE_REPO_DEF"), "msg: {msg}");
        assert!(
            msg.contains("swe/cap.plan.vet"),
            "msg should name the id: {msg}"
        );
    }

    #[test]
    fn load_repo_errors_on_unsupported_top_level_block() {
        let td = TempDir::new().unwrap();
        write_manifest(td.path(), &minimal_manifest("swe"));
        write_file(
            td.path(),
            "capabilities/bad.yaml",
            "bogus_top_level:\n  foo: bar\n",
        );
        let err = load_repo(td.path()).expect_err("unsupported block should error");
        let msg = format!("{:#}", err);
        assert!(msg.contains("bogus_top_level"), "msg: {msg}");
    }

    #[test]
    fn load_repo_silently_skips_missing_layout_dirs() {
        // A repo that only ships skills, no capabilities/flows/etc.
        let td = TempDir::new().unwrap();
        write_manifest(td.path(), &minimal_manifest("snip"));
        write_file(
            td.path(),
            "skills/sk.do.thing.yaml",
            "skills:\n  sk.do.thing:\n    verb: explain\n    lifecycle: stable\n    body: hi\n",
        );
        let (_m, agg) = load_repo(td.path()).expect("skills-only repo loads");
        assert!(agg.pointer("/skills/snip~1sk.do.thing").is_some());
        assert!(
            agg.pointer("/workflows").is_none(),
            "no workflows block expected"
        );
    }

    #[test]
    fn unscanned_warning_flags_yaml_in_a_remapped_default_dir() {
        // Manifest maps flows -> orchestrators/, but a file lands in the DEFAULT
        // flows/ dir (the exact onboard-tool footgun). It must be flagged, not
        // silently dropped.
        let td = TempDir::new().unwrap();
        write_manifest(
            td.path(),
            "schema: praxec.repo/v1\nname: cog\nnamespace: cog\nversion: 0.1.0\nlayout:\n  flows: orchestrators\n",
        );
        write_file(
            td.path(),
            "flows/flow.onboard-tool.yaml",
            "workflows:\n  flow.onboard-tool:\n    title: x\n",
        );
        let m = load_manifest(td.path()).unwrap();
        let warns = unscanned_definition_warnings(td.path(), &m.layout);
        assert_eq!(
            warns.len(),
            1,
            "one remapped-dir warning expected: {warns:?}"
        );
        assert!(
            warns[0].contains("UNSCANNED_DEFINITION_DIR"),
            "{}",
            warns[0]
        );
        assert!(
            warns[0].contains("flows/"),
            "names default dir: {}",
            warns[0]
        );
        assert!(
            warns[0].contains("orchestrators/"),
            "names configured dir: {}",
            warns[0]
        );
    }

    #[test]
    fn unscanned_warning_silent_when_tier_uses_its_default_dir() {
        // flows/ IS the configured dir (no remap) → no warning even with files.
        let td = TempDir::new().unwrap();
        write_manifest(td.path(), &minimal_manifest("swe"));
        write_file(
            td.path(),
            "flows/flow.add-feature.yaml",
            "workflows:\n  flow.add-feature:\n    title: x\n",
        );
        let m = load_manifest(td.path()).unwrap();
        assert!(
            unscanned_definition_warnings(td.path(), &m.layout).is_empty(),
            "default-dir tier must not warn"
        );
    }

    #[test]
    fn rewrite_workflow_refs_recurses_into_nested_executors() {
        // Cover deeply nested executor configs — e.g. inside a fallback or
        // pipeline step.
        let mut v = json!({
            "states": {
                "s1": {
                    "transitions": {
                        "t1": {
                            "executor": {
                                "kind": "pipeline",
                                "steps": [
                                    { "executor": { "kind": "workflow", "definitionId": "cap.x" } },
                                    { "executor": { "kind": "workflow", "definitionId": "other/cap.y" } }
                                ]
                            }
                        }
                    }
                }
            }
        });
        rewrite_workflow_refs(&mut v, "ns");
        let step0 = v
            .pointer("/states/s1/transitions/t1/executor/steps/0/executor/definitionId")
            .and_then(Value::as_str)
            .unwrap();
        assert_eq!(step0, "ns/cap.x");
        let step1 = v
            .pointer("/states/s1/transitions/t1/executor/steps/1/executor/definitionId")
            .and_then(Value::as_str)
            .unwrap();
        assert_eq!(step1, "other/cap.y", "qualified ref untouched");
    }

    #[test]
    fn aggregate_ids_collects_keys_across_all_prefixable_blocks() {
        let agg = json!({
            "workflows":   { "swe/cap.a": {}, "swe/flow.b": {} },
            "skills":      { "swe/sk.x.y": {} },
            "connections": { "swe/conn.z": {} }
        });
        let ids = aggregate_ids(&agg);
        assert!(ids.contains("swe/cap.a"));
        assert!(ids.contains("swe/flow.b"));
        assert!(ids.contains("swe/sk.x.y"));
        assert!(ids.contains("swe/conn.z"));
        assert_eq!(ids.len(), 4);
    }
}
