//! Skill → system-message assembly for the `kind: llm` executor.
//!
//! The agent/skill/prompt contract (SPEC §33): an **agent** is a model
//! binding (no instructions); a **skill** is static, hash-pinned
//! instruction content; the **prompt_template** is the per-run task. A
//! `kind: llm` step's model call is therefore `Agent` (model) + `Skill`
//! (system message) + `Prompt` (user message).
//!
//! This module resolves the skills in scope for the firing step
//! (workflow + state + transition, via the shared core walker) and joins
//! their bodies into the system message. Bodies are injected **verbatim** —
//! never run through the prompt templater — so the hash that
//! `guidance_acknowledged` depends on stays stable.

use praxec_core::error::{ExecutorError, LlmErrorCode};
use praxec_core::skills::{assemble_system_message, SkillAssemblyError};
use serde_json::Value;

/// Build the system message from the skills in scope for this step.
///
/// Thin wrapper over the shared `core::skills::assemble_system_message` (the
/// single source of truth shared with `kind: agent`), mapping the neutral
/// assembly error to the llm executor's typed wire codes. `Ok(None)` = no
/// skills in scope; fails loud on a declared-but-unstamped subject or empty
/// body.
pub(crate) fn collect_system_message(
    definition: &Value,
    state: &str,
    transition: Option<&str>,
) -> Result<Option<String>, ExecutorError> {
    assemble_system_message(definition, state, transition).map_err(|e| match e {
        SkillAssemblyError::SubjectUnknown(subject) => ExecutorError::Llm(
            LlmErrorCode::SkillSubjectUnknown,
            format!(
                "LLM_SKILL_SUBJECT_UNKNOWN: skill '{subject}' is declared in scope but \
                 absent from the workflow snapshot's `_skillsLibrary`"
            ),
        ),
        SkillAssemblyError::BodyMissing(subject) => ExecutorError::Llm(
            LlmErrorCode::SkillBodyMissing,
            format!(
                "LLM_SKILL_BODY_MISSING: skill '{subject}' has no body in the snapshot \
                 `_skillsLibrary`"
            ),
        ),
    })
}
