use thiserror::Error;

/// Errors raised by the workflow runtime when applying a transition.
///
/// Distinct from [`ExecutorError`]: those classify *executor* failures so
/// reliability policies can retry. A `RuntimeError` is a hard stop in the
/// commit path that must abort the transition and propagate to the caller.
#[derive(Debug, Error)]
pub enum RuntimeError {
    /// The transition record (a `workflow.transition` audit event) could not be
    /// written. Because records are emitted *record-first* — before the
    /// authoritative state snapshot is committed — a record-write failure means
    /// the transition must fail fast and the snapshot must NOT be committed.
    /// The message names the workflow id and the `seq` (resulting version) so
    /// operators can pinpoint exactly which transition was aborted.
    #[error(
        "RECORD_WRITE_FAILED: failed to write transition record for workflow '{workflow_id}' at seq {seq}: {source}"
    )]
    RecordWriteFailed {
        workflow_id: String,
        seq: u64,
        #[source]
        source: anyhow::Error,
    },

    /// SPEC §32 — `run_id` uniqueness assertion on `workflow.start`. When the
    /// caller supplies a `runId` and the store already has a live instance
    /// indexed under that id, `start` is rejected here rather than creating a
    /// duplicate. The MCP layer surfaces this as a structured
    /// `RUN_ID_ALREADY_RUNNING` response with a HATEOAS `get` link to the
    /// existing instance.
    #[error(
        "run_id '{run_id}' is already in flight (existing workflow id: {existing_workflow_id})"
    )]
    RunIdAlreadyRunning {
        run_id: String,
        existing_workflow_id: String,
    },

    /// SPEC §30.10.4-5 — pre-start subject walk found a placeholder subject.
    ///
    /// Raised in `WorkflowRuntime::start` when the workflow definition's
    /// `_lexiconLibrary` contains an entry with `state: "PENDING_DEFINITION"`.
    /// The runtime must NOT create the workflow instance. The MCP layer
    /// translates this into a structured `SUBJECT_NEEDS_DEFINITION` interaction
    /// response per §30.10.5.
    #[error("subject '{unknown_subject}' is unresolved in workflow '{workflow_id_context}'")]
    SubjectNeedsDefinition {
        /// The placeholder term that has no lexicon definition.
        unknown_subject: String,
        /// Optional bounded context from the placeholder entry (if any).
        bounded_context: Option<String>,
        /// The `encountered_in` context, formatted as `"workflow:<id>"`.
        workflow_id_context: String,
    },

    /// SPEC §30.10.10 — the configured embedding backend failed during a
    /// lexicon write or a SUBJECT_NEEDS_DEFINITION candidate ranking call.
    ///
    /// When the operator has configured a non-`none` embedding backend,
    /// failures at write time must be surfaced as a structured error so
    /// callers can distinguish "backend down" from other write errors.
    #[error("EMBEDDING_BACKEND_FAILED: {message}")]
    EmbeddingBackendFailed { message: String },
}

impl RuntimeError {
    /// Stable error code token, mirroring the `code` strings used elsewhere in
    /// the runtime (e.g. `ACTOR_MISMATCH`, `STALE_WORKFLOW_VERSION`).
    pub fn code(&self) -> &'static str {
        match self {
            RuntimeError::RecordWriteFailed { .. } => "RECORD_WRITE_FAILED",
            RuntimeError::RunIdAlreadyRunning { .. } => "RUN_ID_ALREADY_RUNNING",
            RuntimeError::SubjectNeedsDefinition { .. } => "SUBJECT_NEEDS_DEFINITION",
            RuntimeError::EmbeddingBackendFailed { .. } => "EMBEDDING_BACKEND_FAILED",
        }
    }
}

/// Classified executor errors. Reliability policies retry / fall back based on
/// the variant, so executors should classify failures here rather than wrapping
/// everything as `Other`.
#[derive(Debug, Error)]
pub enum ExecutorError {
    #[error("timeout after {0} ms")]
    Timeout(u64),

    #[error("rate limited: {0}")]
    RateLimited(String),

    /// NFR R1 — provider-resilience: authentication / authorization failure
    /// (HTTP 401/403, API-key invalid, token expired). These are fail-fast:
    /// the reliability layer MUST NOT retry them — a retry would re-use the
    /// same invalid credential and produce another identical failure. The
    /// caller must fix the credential out-of-band before re-trying.
    ///
    /// Distinct from [`ExecutorError::Permanent`] (which covers config bugs
    /// and schema violations) so dashboards and retry policies can treat auth
    /// failures as an operational signal (rotate credentials) rather than a
    /// workflow-author bug.
    #[error("authentication / authorization error: {0}")]
    Auth(String),

    #[error("connection error: {0}")]
    Connection(String),

    #[error("transient error: {0}")]
    Transient(String),

    #[error("permanent error: {0}")]
    Permanent(String),

    /// SPEC §5.3 — a capability produced an output that failed validation
    /// against its declared `snippet.outputs` schema. The message carries
    /// the structured violation diff (slot name + jsonschema reason). The
    /// variant is distinct from [`ExecutorError::Permanent`] so reliability
    /// policy can refuse to retry contract-typing failures explicitly and
    /// so audit emitters can recognize this class of failure as a
    /// `cap.output.schema_violation` event without text-matching the
    /// `Permanent(..)` payload. Classifies as `ErrorClass::Permanent`
    /// (never retryable).
    #[error("schema violation: {0}")]
    SchemaViolation(String),

    /// SPEC §33 — typed in-runtime LLM executor failures. The
    /// [`LlmErrorCode`] carries a stable, machine-parseable code that
    /// audit emitters and operator error-mapping configs key off; the
    /// trailing string is the human-readable detail. Each code maps to
    /// a specific [`ErrorClass`] (most are `Permanent` per FMECA F1/F2/F6;
    /// `ProviderError` and `StreamTruncated` are `Transient` so the
    /// reliability layer can retry network blips). See SPEC §33 plan D2
    /// for the full FMECA-driven rationale.
    #[error("{0}: {1}")]
    Llm(LlmErrorCode, String),

    /// SPEC §33 audit fixup (F3 STUB-004) — typed LLM failure that
    /// ALSO carries a side-effect blackboard payload the runtime must
    /// merge into `next.context` BEFORE recording the rejection.
    ///
    /// The motivating case is FMECA F1's consecutive-failure cap: when
    /// the model returns `LLM_NO_TOOL_CALL`, the executor needs to
    /// increment `_llm.consecutive_no_tool_call` so the next turn's
    /// pre-turn `apply_caps` sees the higher count. If the increment
    /// happened only on the success path (the pre-fixup behavior), the
    /// counter would never tick up and the F1 cap could never fire —
    /// silently dead protection.
    ///
    /// Classifies identically to `Llm(code, _)` — the side-effect
    /// payload is transport-level metadata, not part of the failure
    /// semantics. Runtime checks via [`ExecutorError::slot_updates`].
    #[error("{code}: {detail}")]
    LlmWithUpdates {
        code: LlmErrorCode,
        detail: String,
        /// JSON object whose keys are the reserved `_llm.*` slot names
        /// the runtime should merge into the post-turn `next.context`.
        /// The runtime treats this identically to a successful
        /// `ExecuteResult.output` for the purpose of `merge_output`,
        /// then proceeds to `failed_response` so the workflow sees a
        /// rejection (not an advance).
        output: serde_json::Value,
    },

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl ExecutorError {
    /// Side-effect blackboard payload to merge into `next.context`
    /// BEFORE rejecting a transition. Returns `Some` only for the
    /// `LlmWithUpdates` variant (SPEC §33 F1 counter increment).
    pub fn slot_updates(&self) -> Option<&serde_json::Value> {
        match self {
            ExecutorError::LlmWithUpdates { output, .. } => Some(output),
            _ => None,
        }
    }
}

/// SPEC §33 FMECA F2 — stable error codes surfaced into the
/// `transition.rejected` audit event so operators can map each LLM
/// failure path to a workflow-author-meaningful response without
/// text-matching the message string. Display impl emits the exact wire
/// code (`LLM_NO_TOOL_CALL`, etc.) used in audit payloads and doctor
/// `errorMapping:` configs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LlmErrorCode {
    /// Model returned a final answer with no tool call. Permanent
    /// (the reliability layer does NOT retry); the executor's own
    /// `max_iterations` is the only retry budget. SPEC §33 FMECA F1.
    NoToolCall,
    /// Model emitted more than one tool call in a single turn. Per
    /// SPEC §33 (multi-tool-call decision), reject and require the
    /// model to pick one. Permanent.
    MultiToolCall,
    /// Model selected a tool name that does not correspond to any
    /// available transition at the current state. Permanent — surfaces
    /// to the model as a structured retry hint at the executor's
    /// outer loop, not via reliability layer.
    UnknownTool,
    /// Tool call arguments failed JSON Schema validation against the
    /// transition's `inputSchema`. Permanent.
    MalformedArguments,
    /// Executor exhausted its per-call budget (`max_iterations`,
    /// `max_seconds`, or the consecutive-failure cap from F1).
    /// Permanent.
    ExecutionExhausted,
    /// Workflow's cumulative `max_cost_usd` or `max_tokens` budget hit;
    /// the executor refuses to issue further provider calls until the
    /// workflow advances out of the budget-binding state. Permanent.
    BudgetExceeded,
    /// The provider returned a model-side error response (the
    /// `LlmResponse::Error` event in the stream — e.g. content filter,
    /// invalid request, provider-rejected payload). Classified
    /// `Connection` so the reliability layer can retry within policy,
    /// but distinct from the transport-level variant so operators can
    /// graph the two failure modes separately and apply different
    /// back-off strategies. SPEC §33 audit fixup (F6 STUB-006).
    ProviderError,
    /// Transport-level failure reaching the provider (HTTP, TLS,
    /// connection-reset, DNS, etc.). The provider never got a chance
    /// to respond. Distinct from [`Self::ProviderError`] (a model-side
    /// rejection) so dashboards can attribute network-flap noise
    /// separately from genuine provider failures.
    ProviderTransport,
    /// The provider's response stream terminated before a `Done` or
    /// `Error` event. Transient.
    StreamTruncated,
    /// The provider's response stream completed without a `Usage`
    /// event AND budget tracking is enabled. Per SPEC §33 FMECA F6
    /// the executor refuses to silently accept a null cost; this
    /// surfaces the missing data instead. Permanent.
    UsageMissing,
    /// Workflow declares `kind: llm` and attempts to inject MCP tools
    /// (`praxec.*` or arbitrary author-supplied tools). Closed by
    /// design per SPEC §33 FMECA F3; raised at config load time, not
    /// runtime. Permanent.
    ExecutorForbiddenTools,
    /// Runtime's submit chain hit `max_chained_llm_turns` (D3).
    /// Permanent.
    ChainDepthExceeded,
    /// SPEC §33 FMECA F7 — two or more available transitions at the
    /// current state share the same `rel` (tool name). The LLM
    /// executor refuses to call the provider because the model's tool
    /// selection would be ambiguous. Permanent (workflow author bug).
    DuplicateTransitionRel,
    /// SPEC §33 audit fixup (F1 STUB-005) — the workflow's
    /// `prompt_template` rendered to empty content (an empty literal,
    /// or a template whose only references resolved to missing scope
    /// variables). Sending an empty user message to the LLM yields a
    /// nonsense turn that an operator would mistake for a real
    /// advance, so the executor fails fast before the provider call.
    /// Permanent (workflow author bug).
    EmptyPrompt,
    /// CMP-012 — the current state offers NO available transitions to
    /// the model after guard-filtering, so the per-turn tool list is
    /// empty. Calling the provider with zero tools can only ever yield
    /// a (billable) final answer with no tool call, which then fails F1
    /// — a guaranteed-wasteful turn. The executor fails fast BEFORE
    /// building context or issuing the provider call. Permanent
    /// (workflow author bug or an over-restrictive guard set).
    NoAvailableTools,
    /// A skill subject is declared in scope (`skills:` at workflow / state /
    /// transition level) but absent from the instance snapshot's
    /// `_skillsLibrary`. The executor injects skill bodies as the model's
    /// system message; a declared-but-unstamped subject is a config/stamp
    /// bug, so the executor fails fast rather than silently dropping the
    /// instructions. Permanent (mirrors the guard's GUIDANCE_SUBJECT_UNKNOWN).
    SkillSubjectUnknown,
    /// A scoped skill resolves in `_skillsLibrary` but its `body` is missing
    /// or empty. Injecting nothing would silently strip the agent's
    /// instructions, so the executor fails fast. Permanent (stamp bug).
    SkillBodyMissing,
}

impl LlmErrorCode {
    /// Stable string code surfaced into audit events and operator
    /// error-mapping configs. Must NOT change across releases.
    pub fn as_wire_code(self) -> &'static str {
        match self {
            LlmErrorCode::NoToolCall => "LLM_NO_TOOL_CALL",
            LlmErrorCode::MultiToolCall => "LLM_MULTI_TOOL_CALL",
            LlmErrorCode::UnknownTool => "LLM_UNKNOWN_TOOL",
            LlmErrorCode::MalformedArguments => "LLM_MALFORMED_ARGUMENTS",
            LlmErrorCode::ExecutionExhausted => "LLM_EXECUTION_EXHAUSTED",
            LlmErrorCode::BudgetExceeded => "LLM_BUDGET_EXCEEDED",
            LlmErrorCode::ProviderError => "LLM_PROVIDER_ERROR",
            LlmErrorCode::ProviderTransport => "LLM_PROVIDER_TRANSPORT",
            LlmErrorCode::StreamTruncated => "LLM_STREAM_TRUNCATED",
            LlmErrorCode::UsageMissing => "LLM_USAGE_MISSING",
            LlmErrorCode::ExecutorForbiddenTools => "LLM_EXECUTOR_FORBIDDEN_TOOLS",
            LlmErrorCode::ChainDepthExceeded => "LLM_CHAIN_DEPTH_EXCEEDED",
            LlmErrorCode::DuplicateTransitionRel => "LLM_DUPLICATE_TRANSITION_REL",
            LlmErrorCode::EmptyPrompt => "LLM_EMPTY_PROMPT",
            LlmErrorCode::NoAvailableTools => "LLM_NO_AVAILABLE_TOOLS",
            LlmErrorCode::SkillSubjectUnknown => "LLM_SKILL_SUBJECT_UNKNOWN",
            LlmErrorCode::SkillBodyMissing => "LLM_SKILL_BODY_MISSING",
        }
    }

    /// Map each code to its [`ErrorClass`] so the reliability layer
    /// knows when to retry.
    pub fn class(self) -> ErrorClass {
        match self {
            // Network-shaped: retryable within policy.
            LlmErrorCode::ProviderError => ErrorClass::Connection,
            LlmErrorCode::ProviderTransport => ErrorClass::Connection,
            LlmErrorCode::StreamTruncated => ErrorClass::Transient,
            // Everything else: permanent — caller bug, budget done, or
            // closed-by-design rejection.
            _ => ErrorClass::Permanent,
        }
    }
}

impl std::fmt::Display for LlmErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_wire_code())
    }
}

impl ExecutorError {
    pub fn class(&self) -> ErrorClass {
        match self {
            ExecutorError::Timeout(_) => ErrorClass::Timeout,
            ExecutorError::RateLimited(_) => ErrorClass::RateLimited,
            ExecutorError::Auth(_) => ErrorClass::Auth,
            ExecutorError::Connection(_) => ErrorClass::Connection,
            ExecutorError::Transient(_) => ErrorClass::Transient,
            ExecutorError::Permanent(_) => ErrorClass::Permanent,
            ExecutorError::SchemaViolation(_) => ErrorClass::Permanent,
            ExecutorError::Llm(code, _) => code.class(),
            ExecutorError::LlmWithUpdates { code, .. } => code.class(),
            ExecutorError::Other(_) => ErrorClass::Permanent,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    Timeout,
    RateLimited,
    /// NFR R1/R2 — auth failures (401/403). Never retried by the reliability
    /// layer; the token is `"auth_error"` so operator dashboards and
    /// `retryOn:` configs can filter it explicitly (and to distinguish it
    /// from generic `"permanent_error"` which covers workflow-author bugs).
    Auth,
    Connection,
    Transient,
    Permanent,
}

impl ErrorClass {
    pub fn token(self) -> &'static str {
        match self {
            ErrorClass::Timeout => "timeout",
            ErrorClass::RateLimited => "rate_limited",
            ErrorClass::Auth => "auth_error",
            ErrorClass::Connection => "connection_error",
            ErrorClass::Transient => "transient_error",
            ErrorClass::Permanent => "permanent_error",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC §33 FMECA F2 — wire codes are operator-facing and MUST NOT
    /// change across releases. This test fails loudly if anyone edits a
    /// code string by mistake.
    #[test]
    fn llm_error_code_wire_codes_are_stable() {
        assert_eq!(LlmErrorCode::NoToolCall.as_wire_code(), "LLM_NO_TOOL_CALL");
        assert_eq!(
            LlmErrorCode::MultiToolCall.as_wire_code(),
            "LLM_MULTI_TOOL_CALL"
        );
        assert_eq!(LlmErrorCode::UnknownTool.as_wire_code(), "LLM_UNKNOWN_TOOL");
        assert_eq!(
            LlmErrorCode::MalformedArguments.as_wire_code(),
            "LLM_MALFORMED_ARGUMENTS"
        );
        assert_eq!(
            LlmErrorCode::ExecutionExhausted.as_wire_code(),
            "LLM_EXECUTION_EXHAUSTED"
        );
        assert_eq!(
            LlmErrorCode::BudgetExceeded.as_wire_code(),
            "LLM_BUDGET_EXCEEDED"
        );
        assert_eq!(
            LlmErrorCode::ProviderError.as_wire_code(),
            "LLM_PROVIDER_ERROR"
        );
        assert_eq!(
            LlmErrorCode::ProviderTransport.as_wire_code(),
            "LLM_PROVIDER_TRANSPORT"
        );
        assert_eq!(
            LlmErrorCode::StreamTruncated.as_wire_code(),
            "LLM_STREAM_TRUNCATED"
        );
        assert_eq!(
            LlmErrorCode::UsageMissing.as_wire_code(),
            "LLM_USAGE_MISSING"
        );
        assert_eq!(
            LlmErrorCode::ExecutorForbiddenTools.as_wire_code(),
            "LLM_EXECUTOR_FORBIDDEN_TOOLS"
        );
        assert_eq!(
            LlmErrorCode::ChainDepthExceeded.as_wire_code(),
            "LLM_CHAIN_DEPTH_EXCEEDED"
        );
        assert_eq!(
            LlmErrorCode::DuplicateTransitionRel.as_wire_code(),
            "LLM_DUPLICATE_TRANSITION_REL"
        );
        assert_eq!(LlmErrorCode::EmptyPrompt.as_wire_code(), "LLM_EMPTY_PROMPT");
        assert_eq!(
            LlmErrorCode::NoAvailableTools.as_wire_code(),
            "LLM_NO_AVAILABLE_TOOLS"
        );
        assert_eq!(
            LlmErrorCode::SkillSubjectUnknown.as_wire_code(),
            "LLM_SKILL_SUBJECT_UNKNOWN"
        );
        assert_eq!(
            LlmErrorCode::SkillBodyMissing.as_wire_code(),
            "LLM_SKILL_BODY_MISSING"
        );
    }

    #[test]
    fn llm_error_code_classes_match_fmeca() {
        // ProviderError, ProviderTransport, and StreamTruncated are
        // retryable network shapes.
        assert_eq!(LlmErrorCode::ProviderError.class(), ErrorClass::Connection);
        assert_eq!(
            LlmErrorCode::ProviderTransport.class(),
            ErrorClass::Connection
        );
        assert_eq!(LlmErrorCode::StreamTruncated.class(), ErrorClass::Transient);
        // Everything else is permanent (FMECA F1, F2, F3, F6).
        for code in [
            LlmErrorCode::NoToolCall,
            LlmErrorCode::MultiToolCall,
            LlmErrorCode::UnknownTool,
            LlmErrorCode::MalformedArguments,
            LlmErrorCode::ExecutionExhausted,
            LlmErrorCode::BudgetExceeded,
            LlmErrorCode::UsageMissing,
            LlmErrorCode::ExecutorForbiddenTools,
            LlmErrorCode::ChainDepthExceeded,
            LlmErrorCode::DuplicateTransitionRel,
            LlmErrorCode::EmptyPrompt,
            LlmErrorCode::NoAvailableTools,
        ] {
            assert_eq!(
                code.class(),
                ErrorClass::Permanent,
                "code {code:?} should be Permanent"
            );
        }
    }

    #[test]
    fn llm_error_class_flows_through_executor_error() {
        let err = ExecutorError::Llm(LlmErrorCode::ProviderError, "503".into());
        assert_eq!(err.class(), ErrorClass::Connection);
        let err = ExecutorError::Llm(LlmErrorCode::NoToolCall, "final answer instead".into());
        assert_eq!(err.class(), ErrorClass::Permanent);
    }

    /// NFR R1/R2 — Auth variant classifies as ErrorClass::Auth (never
    /// retryable by the reliability layer; token "auth_error" is distinct
    /// from "permanent_error" so dashboards can filter it).
    #[test]
    fn auth_error_class_is_distinct_and_not_permanent() {
        let err = ExecutorError::Auth("401 Unauthorized".into());
        assert_eq!(err.class(), ErrorClass::Auth);
        assert_ne!(err.class(), ErrorClass::Permanent);
        assert_eq!(ErrorClass::Auth.token(), "auth_error");
    }
}
