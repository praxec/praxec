//! SPEC §3.2 — workflow tier discrimination.
//!
//! Every workflow is either a capability (`cap.*`) or an flow
//! (`flow.*`); the rest of the codebase (validators V1–V16, V22, the
//! synthesis pass in [`crate::config`], the `use:` executor branch) keys
//! many of its rules off the tier. Centralizing the prefix parse here
//! gives us one canonical place to extend when the spec adds a third
//! tier — and lets us silently classify legacy pre-v0.6 workflows (e.g.
//! `with_artifact_lock`) as [`Tier::Other`] so existing validators
//! don't fire spurious cap/flow-only rules against them.

/// Workflow tier. Determined purely by the unprefixed id stem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// `cap.*` — invokable, typed-contract capability. Subject to V1–V6,
    /// V10, V12, V13, V17, V18.
    Cap,
    /// `flow.*` — lifecycle flow. Subject to V7–V9, V11, V13,
    /// V14, V15, V16.
    Flow,
    /// Pre-v0.6 workflows or anything else. Spec §12.3: no migration
    /// shim — but neither do we error on legacy ids; only cap/flow
    /// shaped ids participate in the new validation cloud.
    Other,
}

impl Tier {
    /// Parse the tier from a definitionId. Handles the namespace prefix
    /// (`swe/cap.plan.vet` → [`Tier::Cap`]) by inspecting only the
    /// portion after the last `/`.
    pub fn from_id(id: &str) -> Tier {
        let stem = id.rsplit('/').next().unwrap_or(id);
        if stem.starts_with("cap.") {
            Tier::Cap
        } else if stem.starts_with("flow.") {
            Tier::Flow
        } else {
            Tier::Other
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_get_cap_tier() {
        assert_eq!(Tier::from_id("cap.plan.vet"), Tier::Cap);
        assert_eq!(Tier::from_id("swe/cap.plan.vet"), Tier::Cap);
        assert_eq!(Tier::from_id("deeply/nested/cap.x"), Tier::Cap); // last segment
    }

    #[test]
    fn flows_get_flow_tier() {
        assert_eq!(Tier::from_id("flow.add-feature"), Tier::Flow);
        assert_eq!(Tier::from_id("swe/flow.add-feature"), Tier::Flow);
    }

    #[test]
    fn legacy_ids_get_other_tier() {
        assert_eq!(Tier::from_id("with_artifact_lock"), Tier::Other);
        assert_eq!(Tier::from_id("namespaced/with_artifact_lock"), Tier::Other);
        assert_eq!(Tier::from_id(""), Tier::Other);
    }

    #[test]
    fn prefix_must_be_exact_with_dot() {
        // `capable.thing` does NOT start with `cap.`. The trailing dot
        // matters; without it we'd misclassify things like `capacity_check`.
        assert_eq!(Tier::from_id("capable.thing"), Tier::Other);
        assert_eq!(Tier::from_id("flow_no_dot"), Tier::Other);
    }
}
