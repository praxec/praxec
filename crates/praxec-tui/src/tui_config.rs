//! TUI runtime configuration with **poka-yoke** on the timeout and step
//! fields: missing values are rejected at startup. The rationale is the
//! whole point of the interpreter — sub-agent isolation only delivers
//! value if every sub-agent has a hard stop. A missing timeout is a
//! foot-gun (orphan sub-agents accumulating cost, or a buggy critic
//! looping forever). Same for step count.

#[derive(Debug, Clone)]
pub struct TuiConfig {
    /// Maximum wall-clock seconds a sub-agent may run before the
    /// interpreter kills it and treats the attempt as a timeout. NO
    /// DEFAULT — must be set via `--max-sub-agent-seconds`.
    pub max_sub_agent_seconds: u64,

    /// **Advisory** step-count hint surfaced to the operator (and
    /// logged at every spawn). NOT currently enforced by the
    /// interpreter: `aether_cli::headless::run_headless` runs the
    /// sub-agent to its own `AgentMessage::Done` OR the wall-clock
    /// timeout (`max_sub_agent_seconds`) — there is no in-band hook to
    /// abort on the Nth tool call. The wall-clock timeout is the
    /// enforced cap. NO DEFAULT — must be set via
    /// `--max-sub-agent-steps` so operators are forced to declare what
    /// they consider reasonable, even though the runtime can only
    /// honour the time bound today.
    pub max_sub_agent_steps: usize,

    /// Warn-level threshold for sub-agent blackboard size (serialized
    /// JSON bytes). Defaults to 16 KiB. Exceeding this is allowed but
    /// emits a `tracing::warn` at spawn time so operators see
    /// pathological context growth early.
    pub max_blackboard_bytes: usize,
}

impl TuiConfig {
    /// Default for `max_blackboard_bytes`. 16 KiB is comfortably above
    /// the typical scoped-blackboard size for a single workflow phase
    /// (~few KB) but well below a frontier model's context window —
    /// crosses the threshold only when an architecture is leaking
    /// previous-phase data into a downstream sub-agent.
    pub const DEFAULT_MAX_BLACKBOARD_BYTES: usize = 16 * 1024;
}

#[derive(Debug, thiserror::Error)]
pub enum TuiConfigError {
    #[error(
        "TUI config requires both --max-sub-agent-seconds and --max-sub-agent-steps. \
         These have no defaults by design: an unbounded sub-agent is a foot-gun \
         (orphan tasks, runaway cost, looping critic). Set them explicitly per \
         your tolerance, then run again."
    )]
    MissingTimeoutOrSteps,

    #[error(
        "TUI config rejects --max-sub-agent-seconds=0 — a zero timeout would \
         kill every sub-agent before it issues a single tool call."
    )]
    ZeroSeconds,

    #[error(
        "TUI config rejects --max-sub-agent-steps=0 — a zero step limit would \
         prevent any sub-agent from doing useful work."
    )]
    ZeroSteps,
}

impl TuiConfig {
    /// Build from CLI inputs. Both `seconds` and `steps` arrive as
    /// `Option<…>` because clap captures missing flags as `None` rather
    /// than failing parsing — the missing-flag poka-yoke lives here so
    /// the error message is meaningful rather than `error: required
    /// arguments not provided`.
    pub fn from_cli(
        max_sub_agent_seconds: Option<u64>,
        max_sub_agent_steps: Option<usize>,
        max_blackboard_bytes: Option<usize>,
    ) -> Result<Self, TuiConfigError> {
        let (Some(seconds), Some(steps)) = (max_sub_agent_seconds, max_sub_agent_steps) else {
            return Err(TuiConfigError::MissingTimeoutOrSteps);
        };
        if seconds == 0 {
            return Err(TuiConfigError::ZeroSeconds);
        }
        if steps == 0 {
            return Err(TuiConfigError::ZeroSteps);
        }
        Ok(Self {
            max_sub_agent_seconds: seconds,
            max_sub_agent_steps: steps,
            max_blackboard_bytes: max_blackboard_bytes
                .unwrap_or(Self::DEFAULT_MAX_BLACKBOARD_BYTES),
        })
    }
}
