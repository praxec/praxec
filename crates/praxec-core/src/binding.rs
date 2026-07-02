//! Shared executor binding validation: "exactly one of {direct, affinity}".
//! Used by both the `kind: llm` and `kind: agent` configs so the rule and its
//! error wording live in one place.

/// Enforce that exactly one of the direct binding or the affinity is set.
/// Returns a human-readable message on violation; callers wrap it in their
/// own typed executor error. `direct_field` names the executor's direct knob
/// (`"model"` for llm, `"agent"` for agent) so the message is precise.
pub fn validate_exclusive_binding(
    direct: Option<&str>,
    affinity: Option<&str>,
    direct_field: &str,
) -> Result<(), String> {
    match (direct, affinity) {
        (Some(_), Some(_)) => Err(format!(
            "set exactly one of `{direct_field}` or `affinity`, not both"
        )),
        (None, None) => Err(format!("set one of `{direct_field}` or `affinity`")),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_exactly_one() {
        assert!(validate_exclusive_binding(Some("openai/gpt"), None, "model").is_ok());
        assert!(validate_exclusive_binding(None, Some("coding"), "model").is_ok());
    }

    #[test]
    fn rejects_both() {
        let e = validate_exclusive_binding(Some("x"), Some("y"), "model").unwrap_err();
        assert!(e.contains("exactly one"));
        assert!(e.contains("model"));
    }

    #[test]
    fn rejects_neither() {
        let e = validate_exclusive_binding(None, None, "agent").unwrap_err();
        assert!(e.contains("one of"));
        assert!(e.contains("agent"));
    }
}
