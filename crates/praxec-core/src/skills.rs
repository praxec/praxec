//! Skill → system-message assembly (SPEC §33.12).
//!
//! Single source of truth for building an agent's system prompt from the
//! in-scope skills' bodies — shared by the `kind: llm` and `kind: agent`
//! executors so the two cannot drift. Bodies are injected **verbatim**, never
//! through a templater, so the hash `guidance_acknowledged` pins stays stable.
//! This is pure data assembly; the executors map [`SkillAssemblyError`] to
//! their own typed error codes.

use serde_json::Value;

use crate::runtime::runtime_links::collect_in_scope_skill_subjects;

/// A skill declared in scope could not be assembled into the system message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillAssemblyError {
    /// Declared in scope but absent from the snapshot `_skillsLibrary`.
    SubjectUnknown(String),
    /// Present but its `body` is missing or empty.
    BodyMissing(String),
}

/// Build the system message from the skills in scope for this step
/// (workflow + state + transition, via [`collect_in_scope_skill_subjects`]).
///
/// `Ok(None)` when no skills are in scope. Fails loud on a declared-but-
/// unstamped subject or an empty body — silently dropping an agent's
/// instructions is worse than a hard stop.
pub fn assemble_system_message(
    definition: &Value,
    state: &str,
    transition: Option<&str>,
) -> Result<Option<String>, SkillAssemblyError> {
    let subjects = collect_in_scope_skill_subjects(definition, state, transition);
    if subjects.is_empty() {
        return Ok(None);
    }

    let library = definition.get("_skillsLibrary").and_then(Value::as_object);
    let mut blocks = Vec::with_capacity(subjects.len());
    for subject in &subjects {
        let entry = library
            .and_then(|lib| lib.get(subject))
            .ok_or_else(|| SkillAssemblyError::SubjectUnknown(subject.clone()))?;

        let body = entry
            .get("body")
            .and_then(Value::as_str)
            .filter(|b| !b.trim().is_empty())
            .ok_or_else(|| SkillAssemblyError::BodyMissing(subject.clone()))?;

        if entry.get("lifecycle").and_then(Value::as_str) == Some("deprecated") {
            tracing::warn!(
                subject = %subject,
                "injecting a deprecated skill into the system message"
            );
        }

        let verb = entry.get("verb").and_then(Value::as_str).unwrap_or("skill");
        blocks.push(format!("## {verb}.{subject}\n\n{body}"));
    }

    Ok(Some(blocks.join("\n\n")))
}

/// A stable prompt-cache key for a skill-derived system message: the sha256 of
/// the verbatim system content, prefixed `sha256:`.
///
/// The system message is the hash-pinned skill bodies joined verbatim (see
/// [`assemble_system_message`]), so identical skill sets yield an identical
/// key. Wired into the LLM call as the provider cache key (OpenAI's
/// `prompt_cache_key`), it routes same-persona requests to the same cache —
/// the skill (persona) is paid for once and reused across turns/agents, while
/// the per-run prompt (user message) stays uncached. This is the payoff of the
/// model⊥skill split (SPEC §33.12): the skill is the stable, cacheable prefix.
pub fn system_message_cache_key(system: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("sha256:{:x}", Sha256::digest(system.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn def(skills: Value, library: Value) -> Value {
        json!({
            "initialState": "s",
            "skills": skills,
            "states": { "s": {} },
            "_skillsLibrary": library
        })
    }

    #[test]
    fn none_when_no_skills_in_scope() {
        let d = json!({ "initialState": "s", "states": { "s": {} } });
        assert_eq!(assemble_system_message(&d, "s", None), Ok(None));
    }

    #[test]
    fn assembles_body_with_header() {
        let d = def(
            json!(["review.tone"]),
            json!({ "review.tone": { "verb": "review", "lifecycle": "stable", "body": "Be terse." } }),
        );
        let out = assemble_system_message(&d, "s", None).unwrap().unwrap();
        assert!(out.contains("## review.review.tone"));
        assert!(out.contains("Be terse."));
    }

    #[test]
    fn unknown_subject_fails_loud() {
        let d = def(json!(["ghost"]), json!({}));
        assert_eq!(
            assemble_system_message(&d, "s", None),
            Err(SkillAssemblyError::SubjectUnknown("ghost".into()))
        );
    }

    #[test]
    fn empty_body_fails_loud() {
        let d = def(
            json!(["x"]),
            json!({ "x": { "verb": "review", "lifecycle": "stable", "body": "  " } }),
        );
        assert_eq!(
            assemble_system_message(&d, "s", None),
            Err(SkillAssemblyError::BodyMissing("x".into()))
        );
    }

    #[test]
    fn cache_key_is_sha256_prefixed() {
        assert!(system_message_cache_key("persona").starts_with("sha256:"));
    }

    #[test]
    fn cache_key_is_64_hex_chars() {
        let key = system_message_cache_key("persona");
        assert_eq!(key.trim_start_matches("sha256:").len(), 64);
    }

    #[test]
    fn cache_key_is_stable_for_identical_content() {
        assert_eq!(
            system_message_cache_key("same skill body"),
            system_message_cache_key("same skill body")
        );
    }

    #[test]
    fn cache_key_differs_for_different_content() {
        assert_ne!(
            system_message_cache_key("skill A"),
            system_message_cache_key("skill B")
        );
    }
}
