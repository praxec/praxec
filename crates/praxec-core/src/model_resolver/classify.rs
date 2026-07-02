//! Failure classification for the Chain-of-Responsibility resolver.
//!
//! **The one rule that prevents a whole class of silent-fallback bugs**
//! (FMECA R1): unknown response status defaults to `ContentOther`, which
//! is **not** infrastructure — so unmapped failures surface to the caller
//! rather than silently triggering CoR fall-through.
//!
//! Closed enum; exhaustive `match` in `from_response`. Tested for 400,
//! 422, an unmapped 4xx (418), and 500-range.

use crate::error::{ExecutorError, LlmErrorCode};

/// What happened when an attempt against a binding failed.
///
/// `is_infrastructure() == true` for failures the resolver treats as
/// "try the next binding" — infrastructure trouble OR a model-capability
/// gap (i.e. "route-around-able"); everything else surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// HTTP 401 — bad/expired API key.
    Auth401,
    /// HTTP 403 — key is valid but lacks permission.
    Auth403,
    /// HTTP 429 — rate-limited.
    RateLimit429,
    /// HTTP 404 — model name unknown / deprecated.
    NotFound404,
    /// Network unreachable, connection timed out, DNS failure, etc.
    NetworkTimeout,
    /// Response body did not match the expected schema.
    ContentSchema,
    /// Provider refused the request on safety grounds.
    ContentSafety,
    /// Any other content-level error, including unmapped HTTP statuses.
    /// **Surfaces** — never triggers CoR fall-through.
    ContentOther,
    /// The model produced no conforming/usable result (no final_answer,
    /// no tool call, or a result that failed conformance) — a WEAKER
    /// MODEL'S CAPABILITY GAP, which the resolver routes around by
    /// escalating to a stronger model in the chain.
    Capability,
}

impl FailureClass {
    /// True iff the failure represents infrastructure trouble OR a
    /// model-capability gap that the resolver should try to route around
    /// (i.e. "route-around-able" — try the next binding in the list).
    /// False for content-level / author errors, which the resolver surfaces.
    pub fn is_infrastructure(self) -> bool {
        matches!(
            self,
            FailureClass::Auth401
                | FailureClass::Auth403
                | FailureClass::RateLimit429
                | FailureClass::NotFound404
                | FailureClass::NetworkTimeout
                | FailureClass::Capability
        )
    }

    /// Classify an HTTP response by status code. The body is logged at
    /// the call site for diagnostics; classification itself only reads
    /// the status code so it stays deterministic and testable.
    ///
    /// Unmapped statuses (incl. unusual 4xx like 418 and any 5xx that
    /// isn't a connection failure) → `ContentOther`. The caller surfaces.
    pub fn from_status(status: u16) -> Self {
        match status {
            401 => FailureClass::Auth401,
            403 => FailureClass::Auth403,
            429 => FailureClass::RateLimit429,
            404 => FailureClass::NotFound404,
            502..=504 => FailureClass::NetworkTimeout,
            _ => FailureClass::ContentOther,
        }
    }

    /// Classify a transport-layer error (no HTTP status — connection
    /// never completed). Anything resembling a network timeout/connection
    /// failure maps to `NetworkTimeout`; everything else → `ContentOther`
    /// (surface). The conservative default holds even here.
    pub fn from_io_error(kind: std::io::ErrorKind) -> Self {
        use std::io::ErrorKind::*;
        match kind {
            TimedOut | ConnectionRefused | ConnectionReset | ConnectionAborted | NotConnected
            | HostUnreachable | NetworkUnreachable | NetworkDown | Interrupted => {
                FailureClass::NetworkTimeout
            }
            _ => FailureClass::ContentOther,
        }
    }

    /// Classify an [`ExecutorError`] for the model resolver's CoR walk.
    ///
    /// Mapping rationale:
    /// - `Permanent` whose message starts with `"AGENT_NO_RESULT"` or
    ///   `"AGENT_RESULT_FAILED"` → `Capability` (weak model; escalate).
    /// - `Llm(NoToolCall, _)` / `LlmWithUpdates { code: NoToolCall, .. }`
    ///   → `Capability` (model emitted a final answer instead of a tool
    ///   call; capability gap, route around).
    /// - `Timeout` / `Connection` / `Transient` → `NetworkTimeout`.
    /// - `RateLimited` → `RateLimit429`.
    /// - `Auth` → `Auth401`.
    /// - Everything else (other `Permanent`, `SchemaViolation`, other Llm
    ///   codes, `Other`) → `ContentOther` (surfaces — author/content error).
    pub fn from_executor_error(err: &ExecutorError) -> Self {
        match err {
            ExecutorError::Permanent(msg)
                if msg.starts_with("AGENT_NO_RESULT") || msg.starts_with("AGENT_RESULT_FAILED") =>
            {
                FailureClass::Capability
            }
            ExecutorError::Llm(LlmErrorCode::NoToolCall, _) => FailureClass::Capability,
            ExecutorError::LlmWithUpdates {
                code: LlmErrorCode::NoToolCall,
                ..
            } => FailureClass::Capability,
            ExecutorError::Timeout(_)
            | ExecutorError::Connection(_)
            | ExecutorError::Transient(_) => FailureClass::NetworkTimeout,
            ExecutorError::RateLimited(_) => FailureClass::RateLimit429,
            ExecutorError::Auth(_) => FailureClass::Auth401,
            // Everything else surfaces (author/content errors, schema
            // violations, other Llm codes, Other).
            _ => FailureClass::ContentOther,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── existing HTTP / IO tests ─────────────────────────────────────────

    #[test]
    fn from_status_maps_known_codes() {
        assert_eq!(FailureClass::from_status(401), FailureClass::Auth401);
        assert_eq!(FailureClass::from_status(403), FailureClass::Auth403);
        assert_eq!(FailureClass::from_status(429), FailureClass::RateLimit429);
        assert_eq!(FailureClass::from_status(404), FailureClass::NotFound404);
        assert_eq!(FailureClass::from_status(502), FailureClass::NetworkTimeout);
        assert_eq!(FailureClass::from_status(503), FailureClass::NetworkTimeout);
        assert_eq!(FailureClass::from_status(504), FailureClass::NetworkTimeout);
    }

    #[test]
    fn from_status_defaults_to_content_other() {
        assert_eq!(FailureClass::from_status(400), FailureClass::ContentOther);
        assert_eq!(FailureClass::from_status(418), FailureClass::ContentOther);
        assert_eq!(FailureClass::from_status(422), FailureClass::ContentOther);
        assert_eq!(FailureClass::from_status(500), FailureClass::ContentOther);
    }

    #[test]
    fn infrastructure_variants_return_true() {
        assert!(FailureClass::Auth401.is_infrastructure());
        assert!(FailureClass::Auth403.is_infrastructure());
        assert!(FailureClass::RateLimit429.is_infrastructure());
        assert!(FailureClass::NotFound404.is_infrastructure());
        assert!(FailureClass::NetworkTimeout.is_infrastructure());
    }

    #[test]
    fn content_variants_return_false() {
        assert!(!FailureClass::ContentSchema.is_infrastructure());
        assert!(!FailureClass::ContentSafety.is_infrastructure());
        assert!(!FailureClass::ContentOther.is_infrastructure());
    }

    // ── Capability variant tests (plan deliverable) ──────────────────────

    /// (a) Capability.is_infrastructure() == true — escalatable.
    #[test]
    fn capability_is_infrastructure() {
        assert!(FailureClass::Capability.is_infrastructure());
    }

    /// (b) AGENT_NO_RESULT permanent error maps to Capability.
    #[test]
    fn from_executor_error_agent_no_result_is_capability() {
        let err = ExecutorError::Permanent("AGENT_NO_RESULT: model returned a final answer".into());
        assert_eq!(
            FailureClass::from_executor_error(&err),
            FailureClass::Capability
        );
    }

    /// (c) AGENT_RESULT_FAILED permanent error maps to Capability.
    #[test]
    fn from_executor_error_agent_result_failed_is_capability() {
        let err = ExecutorError::Permanent("AGENT_RESULT_FAILED: conformance check failed".into());
        assert_eq!(
            FailureClass::from_executor_error(&err),
            FailureClass::Capability
        );
    }

    /// (d) LlmErrorCode::NoToolCall maps to Capability.
    #[test]
    fn from_executor_error_no_tool_call_is_capability() {
        let err = ExecutorError::Llm(LlmErrorCode::NoToolCall, "final answer instead".into());
        assert_eq!(
            FailureClass::from_executor_error(&err),
            FailureClass::Capability
        );
    }

    /// (d-bis) LlmWithUpdates NoToolCall also maps to Capability.
    #[test]
    fn from_executor_error_no_tool_call_with_updates_is_capability() {
        let err = ExecutorError::LlmWithUpdates {
            code: LlmErrorCode::NoToolCall,
            detail: "no tool call".into(),
            output: serde_json::json!({}),
        };
        assert_eq!(
            FailureClass::from_executor_error(&err),
            FailureClass::Capability
        );
    }

    /// (e) A Permanent error that is NOT an AGENT_* prefix → ContentOther (surfaces).
    #[test]
    fn from_executor_error_other_permanent_is_content_other() {
        let err = ExecutorError::Permanent("some author bug".into());
        assert_eq!(
            FailureClass::from_executor_error(&err),
            FailureClass::ContentOther
        );
    }

    /// (f) Auth error maps to Auth401 which is_infrastructure().
    #[test]
    fn from_executor_error_auth_is_infrastructure() {
        let err = ExecutorError::Auth("x".into());
        let class = FailureClass::from_executor_error(&err);
        assert_eq!(class, FailureClass::Auth401);
        assert!(class.is_infrastructure());
    }

    /// Timeout maps to NetworkTimeout (infrastructure).
    #[test]
    fn from_executor_error_timeout_is_network_timeout() {
        let err = ExecutorError::Timeout(5000);
        assert_eq!(
            FailureClass::from_executor_error(&err),
            FailureClass::NetworkTimeout
        );
    }

    /// RateLimited maps to RateLimit429 (infrastructure).
    #[test]
    fn from_executor_error_rate_limited_is_rate_limit() {
        let err = ExecutorError::RateLimited("too fast".into());
        assert_eq!(
            FailureClass::from_executor_error(&err),
            FailureClass::RateLimit429
        );
    }

    /// SchemaViolation is a content/author error — surfaces as ContentOther.
    #[test]
    fn from_executor_error_schema_violation_is_content_other() {
        let err = ExecutorError::SchemaViolation("bad output".into());
        assert_eq!(
            FailureClass::from_executor_error(&err),
            FailureClass::ContentOther
        );
    }

    /// Other LLM codes (not NoToolCall) → ContentOther (surfaces).
    #[test]
    fn from_executor_error_other_llm_codes_are_content_other() {
        for code in [
            LlmErrorCode::MultiToolCall,
            LlmErrorCode::UnknownTool,
            LlmErrorCode::MalformedArguments,
            LlmErrorCode::ExecutionExhausted,
            LlmErrorCode::BudgetExceeded,
        ] {
            let err = ExecutorError::Llm(code, "detail".into());
            assert_eq!(
                FailureClass::from_executor_error(&err),
                FailureClass::ContentOther,
                "LlmErrorCode::{code:?} should map to ContentOther"
            );
        }
    }
}
