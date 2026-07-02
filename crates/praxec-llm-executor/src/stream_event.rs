//! Provider-agnostic stream events — the executor's internal vocabulary for one
//! streaming turn, decoupled from any provider SDK. The rig-backed provider
//! factory maps rig's stream into these; [`crate::response::drain_stream`]
//! consumes them. (Was aether-llm's `LlmResponse`; rig is the engine now.)

/// A completed tool call the model requested. rig emits whole tool calls, so the
/// executor never assembles streaming arg deltas.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolCallRequest {
    pub id: String,
    pub name: String,
    /// JSON arguments, as a string (validated downstream).
    pub arguments: String,
}

/// Token usage for a turn — required by the budget caps (D6) + cost map (D8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Reasoning tokens, when the provider reports them separately (audit only).
    /// rig's `Usage` folds these into `output_tokens`, so this is `None` on the
    /// rig path.
    pub reasoning_tokens: Option<u64>,
}

/// Why the model stopped. Captured for audit; `validate` keys off `saw_done`, so
/// the exact value is informational.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    ToolCalls,
    FunctionCall,
    Length,
    ContentFilter,
    Unknown,
}

/// One event from a provider streaming turn. Closed by design — a new variant
/// breaks the drain's exhaustive match rather than silently dropping data.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// Turn started (provider message id), for audit.
    Start { message_id: String },
    /// A chunk of answer text.
    Text { chunk: String },
    /// A chunk of model-readable reasoning (the audit summary).
    Reasoning { chunk: String },
    /// An opaque/encrypted reasoning block (held for replay, never audited).
    EncryptedReasoning { id: String, content: String },
    /// A completed tool call.
    ToolCall(ToolCallRequest),
    /// Token usage for the turn.
    Usage(TokenUsage),
    /// The turn finished cleanly.
    Done { stop_reason: Option<StopReason> },
    /// A model-side error event (distinct from a transport `Err`).
    Error { message: String },
}
