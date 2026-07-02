//! SPEC §4 — the closed 24-token verb cloud for `cap.*` workflows.
//!
//! Intentionally distinct from [`crate::discovery::Verb`] (the 10-token
//! cognitive verb cloud governing `skills:` subjects). Conflating the two
//! would break the `subject_roots.rs` / `subject_root_warnings.rs` /
//! `spec_enum_drift.rs` plumbing that treats `Verb` as the skill cloud.
//! Capability verbs live here.
//!
//! Adding a verb to the cloud requires a SPEC bump in
//! `docs/architecture/capability-orchestrator.md §4`.

/// Verb categories. Drive the per-verb primary-executor shape check
/// (V6, see [`category`]) and inform `gateway.describe` summaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapVerbCategory {
    /// LLM is the actor. Primary executor must be `mcp` or a `noop` that
    /// surfaces a skill via a `guidance:` reference.
    Cognitive,
    /// Script or MCP tool is the actor. Primary executor must be `script`
    /// or `mcp`.
    Deterministic,
    /// `gate` + `coordinate` — neither cognitive nor pure deterministic.
    /// Per-verb shape checks (HITL transition for `gate`, external MCP
    /// connection for `coordinate`) sit in [`crate::validate`].
    Coordination,
}

/// Every legal `cap.*` verb. The variants are alphabetical within each
/// category for readability; runtime lookup is via [`CapVerb::from_token`]
/// which does the str-to-enum mapping in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapVerb {
    // Cognitive (10)
    Triage,
    Diagnose,
    Plan,
    Implement,
    Review,
    Refactor,
    Explain,
    Compose,
    Research,
    Summarize,
    // Deterministic (12)
    Build,
    Test,
    Deploy,
    Format,
    Lint,
    Install,
    Verify,
    Run,
    Inspect,
    Search,
    Fetch,
    Audit,
    // Coordination (2)
    Gate,
    Coordinate,
}

impl CapVerb {
    /// Closed-enum token check. Returns `Some(verb)` iff `s` matches one
    /// of the 24 blessed tokens exactly (case-sensitive, no leading/
    /// trailing whitespace).
    pub fn from_token(s: &str) -> Option<CapVerb> {
        match s {
            "triage" => Some(CapVerb::Triage),
            "diagnose" => Some(CapVerb::Diagnose),
            "plan" => Some(CapVerb::Plan),
            "implement" => Some(CapVerb::Implement),
            "review" => Some(CapVerb::Review),
            "refactor" => Some(CapVerb::Refactor),
            "explain" => Some(CapVerb::Explain),
            "compose" => Some(CapVerb::Compose),
            "research" => Some(CapVerb::Research),
            "summarize" => Some(CapVerb::Summarize),
            "build" => Some(CapVerb::Build),
            "test" => Some(CapVerb::Test),
            "deploy" => Some(CapVerb::Deploy),
            "format" => Some(CapVerb::Format),
            "lint" => Some(CapVerb::Lint),
            "install" => Some(CapVerb::Install),
            "verify" => Some(CapVerb::Verify),
            "run" => Some(CapVerb::Run),
            "inspect" => Some(CapVerb::Inspect),
            "search" => Some(CapVerb::Search),
            "fetch" => Some(CapVerb::Fetch),
            "audit" => Some(CapVerb::Audit),
            "gate" => Some(CapVerb::Gate),
            "coordinate" => Some(CapVerb::Coordinate),
            _ => None,
        }
    }

    /// Return the verb's token string (round-trips through [`from_token`]).
    pub fn token(self) -> &'static str {
        match self {
            CapVerb::Triage => "triage",
            CapVerb::Diagnose => "diagnose",
            CapVerb::Plan => "plan",
            CapVerb::Implement => "implement",
            CapVerb::Review => "review",
            CapVerb::Refactor => "refactor",
            CapVerb::Explain => "explain",
            CapVerb::Compose => "compose",
            CapVerb::Research => "research",
            CapVerb::Summarize => "summarize",
            CapVerb::Build => "build",
            CapVerb::Test => "test",
            CapVerb::Deploy => "deploy",
            CapVerb::Format => "format",
            CapVerb::Lint => "lint",
            CapVerb::Install => "install",
            CapVerb::Verify => "verify",
            CapVerb::Run => "run",
            CapVerb::Inspect => "inspect",
            CapVerb::Search => "search",
            CapVerb::Fetch => "fetch",
            CapVerb::Audit => "audit",
            CapVerb::Gate => "gate",
            CapVerb::Coordinate => "coordinate",
        }
    }

    /// Verb category (drives V6 primary-executor shape check).
    pub fn category(self) -> CapVerbCategory {
        use CapVerb::*;
        match self {
            Triage | Diagnose | Plan | Implement | Review | Refactor | Explain | Compose
            | Research | Summarize => CapVerbCategory::Cognitive,
            Build | Test | Deploy | Format | Lint | Install | Verify | Run | Inspect | Search
            | Fetch | Audit => CapVerbCategory::Deterministic,
            Gate | Coordinate => CapVerbCategory::Coordination,
        }
    }
}

/// All blessed cap verb tokens, ordered for stable error-message lists
/// (cognitive → deterministic → coordination, mirroring spec §4).
pub const BLESSED_CAP_VERBS: &[&str] = &[
    // Cognitive (10)
    "triage",
    "diagnose",
    "plan",
    "implement",
    "review",
    "refactor",
    "explain",
    "compose",
    "research",
    "summarize",
    // Deterministic (12)
    "build",
    "test",
    "deploy",
    "format",
    "lint",
    "install",
    "verify",
    "run",
    "inspect",
    "search",
    "fetch",
    "audit",
    // Coordination (2)
    "gate",
    "coordinate",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_has_exactly_twenty_four_verbs() {
        assert_eq!(BLESSED_CAP_VERBS.len(), 24, "cloud size locked at 24");
    }

    #[test]
    fn every_blessed_token_round_trips_through_from_token_and_token() {
        for tok in BLESSED_CAP_VERBS {
            let verb = CapVerb::from_token(tok).unwrap_or_else(|| {
                panic!("blessed verb '{tok}' must round-trip through from_token")
            });
            assert_eq!(verb.token(), *tok, "round trip mismatch for '{tok}'");
        }
    }

    #[test]
    fn from_token_rejects_unblessed_strings() {
        for s in ["", "PLAN", " plan", "plan ", "destroy", "build_thing"] {
            assert!(CapVerb::from_token(s).is_none(), "should reject '{s}'");
        }
    }

    #[test]
    fn categories_partition_the_cloud() {
        let mut cognitive = 0;
        let mut deterministic = 0;
        let mut coordination = 0;
        for tok in BLESSED_CAP_VERBS {
            match CapVerb::from_token(tok).unwrap().category() {
                CapVerbCategory::Cognitive => cognitive += 1,
                CapVerbCategory::Deterministic => deterministic += 1,
                CapVerbCategory::Coordination => coordination += 1,
            }
        }
        assert_eq!(cognitive, 10, "10 cognitive verbs (SPEC §4)");
        assert_eq!(deterministic, 12, "12 deterministic verbs (SPEC §4)");
        assert_eq!(coordination, 2, "2 coordination verbs (SPEC §4)");
    }
}
