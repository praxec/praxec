//! SPEC §19 — ingest executor: adapt external guidance sources to the
//! Praxec fragment shape. v1 handles mattpocock-style `.claude/skills/*.md`
//! (YAML frontmatter + markdown body).
//!
//! Output never publishes; it returns a fragment that the calling workflow
//! routes through the rest of the authoring workflow (structural analysis,
//! dry-run, registry). This keeps the gates uniform regardless of source.

use std::collections::HashMap;
use std::path::Path;

use async_trait::async_trait;
use praxec_core::config::compute_skill_hash;
use praxec_core::discovery::Verb;
use praxec_core::error::ExecutorError;
use praxec_core::model::{ExecuteRequest, ExecuteResult};
use praxec_core::ports::Executor;
use serde_json::{Value, json};

pub struct IngestExecutor;

#[async_trait]
impl Executor for IngestExecutor {
    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        let args = &request.arguments;
        let source_path = args
            .get("source_path")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ExecutorError::Permanent("ingest: missing required argument `source_path`".into())
            })?;

        // Optional caller-supplied subject; if absent we infer from path.
        let subject_arg = args.get("subject").and_then(Value::as_str);

        // Optional caller-supplied verb synonym overrides.
        let mut synonyms = default_verb_synonyms();
        if let Some(map) = args.get("verb_synonyms").and_then(Value::as_object) {
            for (k, v) in map {
                if let Some(val) = v.as_str() {
                    synonyms.insert(k.clone(), val.to_string());
                }
            }
        }

        // Source body — for v1 we accept either an inline body via the
        // `source_body` arg (so the executor stays testable without disk)
        // or read from disk if only a path is given.
        let raw = match args.get("source_body").and_then(Value::as_str) {
            Some(b) => b.to_string(),
            None => std::fs::read_to_string(source_path).map_err(|e| {
                ExecutorError::Permanent(format!(
                    "ingest: failed to read source_path '{source_path}': {e}"
                ))
            })?,
        };

        let (frontmatter, body) = split_frontmatter(&raw, source_path)?;
        if body.trim().is_empty() {
            return Err(ExecutorError::Permanent(format!(
                "INGEST_EMPTY_BODY: source '{source_path}' has no body content"
            )));
        }

        // Subject resolution: explicit arg > frontmatter `subject:` > path inference.
        let subject: String = match subject_arg.map(str::to_string) {
            Some(s) => s,
            None => match frontmatter
                .get("subject")
                .and_then(Value::as_str)
                .map(str::to_string)
            {
                Some(s) => s,
                None => infer_subject_from_path(source_path).ok_or_else(|| {
                    ExecutorError::Permanent(format!(
                        "INGEST_CANNOT_INFER_SUBJECT: no `subject` argument, no frontmatter \
                         subject, and path '{source_path}' did not yield a usable subject"
                    ))
                })?,
            },
        };

        // Verb resolution. Frontmatter `verb:` is consulted; if absent, we
        // try the frontmatter `name:` (mattpocock convention treats name as
        // an action verb, e.g. "diagnose"). Run through synonym mapping.
        let raw_verb = frontmatter
            .get("verb")
            .and_then(Value::as_str)
            .or_else(|| frontmatter.get("name").and_then(Value::as_str))
            .unwrap_or("");
        let mut diagnostics: Vec<Value> = Vec::new();
        let resolved_verb = if Verb::from_token(raw_verb).is_some() {
            raw_verb.to_string()
        } else if let Some(mapped) = synonyms.get(raw_verb) {
            diagnostics.push(json!({
                "level": "info",
                "code":  "VERB_MAPPED",
                "from":  raw_verb,
                "to":    mapped,
            }));
            mapped.clone()
        } else {
            return Err(ExecutorError::Permanent(format!(
                "INGEST_INVALID_VERB: source verb '{raw_verb}' is neither in the closed eight \
                 nor in the synonym table"
            )));
        };

        let hash = compute_skill_hash(body.trim());
        let fragment = json!({
            "subject":   subject,
            "verb":      resolved_verb,
            "lifecycle": "experimental",
            "body":      body.trim(),
            "hash":      hash,
            "source":    source_path,
        });

        Ok(ExecuteResult {
            output: json!({
                "fragment":    fragment,
                "diagnostics": diagnostics,
            }),
            evidence: vec![],
            child_workflow_id: None,
            next_transition: None,
            suspend: None,
            telemetry: None,
        })
    }
}

/// Built-in verb synonym table (SPEC §19.3).
fn default_verb_synonyms() -> HashMap<String, String> {
    let pairs = [
        ("fix", "implement"),
        ("verify", "review"),
        ("validate", "review"),
        ("test", "review"),
        ("audit", "review"),
        ("cleanup", "refactor"),
        ("tidy", "refactor"),
        ("improve", "refactor"),
        ("document", "explain"),
        ("teach", "explain"),
        ("walkthrough", "explain"),
        ("assemble", "compose"),
        ("bundle", "compose"),
        ("integrate", "compose"),
        ("investigate", "diagnose"),
        ("inspect", "diagnose"),
        ("analyze", "diagnose"),
        ("prioritize", "triage"),
        ("classify", "triage"),
        ("route", "triage"),
        ("design", "plan"),
        ("spec", "plan"),
        ("specify", "plan"),
    ];
    pairs
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Split YAML frontmatter (between `---` fences) from the body. If the body
/// has no frontmatter, returns an empty map and the entire input as body.
///
/// When a frontmatter fence IS present but the enclosed YAML fails to parse,
/// fail-fast with `INGEST_INVALID_FRONTMATTER` rather than swallowing the
/// error to an empty map (CMP-048) — silent loss of declared metadata
/// (subject, verb, …) would surface later as a confusing downstream failure.
fn split_frontmatter(
    raw: &str,
    source_path: &str,
) -> Result<(serde_json::Map<String, Value>, String), ExecutorError> {
    let stripped = raw.trim_start();
    if !stripped.starts_with("---") {
        return Ok((serde_json::Map::new(), raw.to_string()));
    }
    let after_open = &stripped[3..];
    let after_open = after_open.trim_start_matches('\n');
    let Some(close_idx) = after_open.find("\n---") else {
        // Malformed frontmatter — treat as no frontmatter.
        return Ok((serde_json::Map::new(), raw.to_string()));
    };
    let fm_text = &after_open[..close_idx];
    let body_start = close_idx + "\n---".len();
    let body_rest = &after_open[body_start..];
    let body = body_rest.trim_start_matches('\n').to_string();

    let parsed: Value = serde_yaml::from_str(fm_text).map_err(|e| {
        ExecutorError::Permanent(format!(
            "INGEST_INVALID_FRONTMATTER: source '{source_path}' has a `---` frontmatter fence \
             but its YAML failed to parse: {e}"
        ))
    })?;
    let map = parsed
        .as_object()
        .cloned()
        .unwrap_or_else(serde_json::Map::new);
    Ok((map, body))
}

/// Infer a subject like `import.mattpocock.diagnose` from a source path
/// like `.claude/skills/engineering/diagnose/SKILL.md`. The last
/// non-trivial directory wins as the subject leaf; the second-to-last
/// becomes the middle segment; root is always `import.<sourcetag>` with
/// sourcetag inferred from the second-from-top directory.
fn infer_subject_from_path(path: &str) -> Option<String> {
    let p = Path::new(path);
    let segments: Vec<&str> = p
        .iter()
        .filter_map(|os| os.to_str())
        .filter(|s| !s.is_empty() && *s != "/" && *s != ".")
        .collect();
    if segments.is_empty() {
        return None;
    }
    // Heuristic: take the last directory name (skipping SKILL.md /
    // index.md / README.md trailing files) as the leaf.
    let mut leaf_idx = segments.len().saturating_sub(1);
    let trailing_filenames = ["SKILL.md", "index.md", "README.md", "skill.md"];
    if trailing_filenames.contains(&segments[leaf_idx]) && leaf_idx > 0 {
        leaf_idx -= 1;
    }
    // If the chosen segment ends in `.md`, strip extension.
    let leaf = segments[leaf_idx]
        .strip_suffix(".md")
        .unwrap_or(segments[leaf_idx])
        .to_string();
    if leaf.is_empty() {
        return None;
    }
    // Mid segment: previous non-trivial directory if available, else "general".
    let mid = if leaf_idx > 0 {
        segments[leaf_idx - 1].to_string()
    } else {
        "general".to_string()
    };
    Some(format!("import.{mid}.{leaf}"))
}
