//! SPEC §33 PA1 — `Planner` data model.
//!
//! This module defines the *types* the [`Planner`] trait
//! (see [`crate::ports::Planner`]) carries across the IP boundary. The trait
//! itself lives in `ports.rs` next to the other runtime ports; everything an
//! implementer needs to construct, mutate, or report on a plan is here.
//!
//! Two implementations satisfy the contract:
//!
//! - Open source: the standalone `cpm-planner` crate (separate repo) is a
//!   textbook critical-path-method implementation, consumed over MCP.
//! - Closed source: a richer FrontRails implementation lives in a separate
//!   workspace and links the published `praxec-core` crate.
//!
//! The runtime always holds an `Arc<dyn Planner>`; operators register whichever
//! implementation suits their deployment.
//!
//! # Wire format
//!
//! Every data type derives `Serialize` + `Deserialize` because the MCP server
//! layer (PA4) serialises plans, cohorts, and status snapshots into JSON-RPC
//! responses. The unit test at the bottom of this file exercises a one-
//! deliverable round-trip as a smoke test for the wire shape.
//!
//! # Locking semantics summary
//!
//! - A `PlanGraph` is submitted once; the implementation hashes
//!   `(graph, caller)` and returns an existing [`PlanId`] on resubmit. Calls
//!   are idempotent.
//! - [`crate::ports::Planner::acquire_cohort`] returns a [`Cohort`]: a batch
//!   of deliverables whose prerequisites are all [`DeliverableStatus::Complete`]
//!   and whose owned-file sets are mutually disjoint *and* disjoint from every
//!   currently held lock. The batch is locked atomically (PA3 guarantees this).
//! - [`crate::ports::Planner::mark_status`] with `Complete` or `Failed`
//!   releases the lock. A caller-id mismatch on the held lock yields
//!   [`PlannerError::LockNotHeld`].
//! - [`crate::ports::Planner::heartbeat`] refreshes the TTL; the TTL itself is
//!   an implementation parameter (PA3 sets the open-source default at 5 min).
//! - [`crate::ports::Planner::force_release`] is the operator escape hatch.
//!   Implementations MUST emit an audit event carrying the supplied `reason`.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Opaque plan identifier returned by [`crate::ports::Planner::submit_plan`].
///
/// The string form is implementation-defined (UUID, deterministic content
/// hash, ULID, etc.). Callers treat the value as opaque and round-trip it
/// without inspecting the contents.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PlanId(pub String);

impl PlanId {
    /// Borrow the underlying string for logging or hashing.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PlanId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Opaque per-orchestrator identity. The Planner uses this to verify that
/// the caller releasing a lock is the same caller who acquired it.
///
/// The string form is implementation-defined; it must be stable for the
/// lifetime of a single orchestrator session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CallerId(pub String);

impl CallerId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CallerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Full plan submitted to [`crate::ports::Planner::submit_plan`].
///
/// A plan is a DAG of [`Deliverable`]s connected by the
/// `prerequisites` field. The Planner is responsible for detecting cycles,
/// missing prerequisite references, and duplicate ids; on any structural
/// problem it returns [`PlannerError::InvalidGraph`] with a precise
/// `reason`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanGraph {
    /// Every deliverable in the plan. Order is irrelevant; the Planner
    /// derives execution order from the `prerequisites` edges.
    pub deliverables: Vec<Deliverable>,

    /// Optional global guardrail: maximum number of deliverables that may
    /// be dispatched in one chained sequence before the orchestrator must
    /// pause for explicit re-prompt. `None` means no limit. Carried at the
    /// graph level rather than per-deliverable because it reflects an
    /// operator policy, not a property of any single task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_chained_dispatch: Option<u32>,
}

/// A single unit of work scheduled by the Planner.
///
/// `owned_files` is the load-bearing field for concurrent dispatch: the
/// Planner guarantees that two deliverables with overlapping `owned_files`
/// will never be returned in the same [`Cohort`] and will never both hold
/// active locks. This is the only mechanism the Planner uses to prevent
/// write-write conflicts; implementations of [`crate::ports::Planner`]
/// must therefore reject any plan that contains a deliverable whose
/// `owned_files` are not specified up front.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Deliverable {
    /// Unique identifier within the plan. The Planner rejects duplicate
    /// ids at submit time with [`PlannerError::InvalidGraph`].
    pub id: String,

    /// Exact file paths the implementer is going to write while completing
    /// this deliverable. Disjointness across this set is the lock-contention
    /// invariant; see [`crate::ports::Planner::acquire_cohort`] semantics.
    pub owned_files: Vec<PathBuf>,

    /// Ids of other deliverables in the same plan that must reach
    /// [`DeliverableStatus::Complete`] before this one becomes eligible
    /// for acquisition.
    pub prerequisites: Vec<String>,

    /// Estimated wall-clock effort, used by critical-path math in
    /// [`PlanStatus::critical_path`]. `None` means the implementation
    /// should treat the duration as one unit when computing the longest
    /// chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_effort_hours: Option<f32>,

    /// Free-form metadata. Conventionally carries model hints, human
    /// descriptions, links to specs, etc. The Planner does not interpret
    /// this field.
    #[serde(default)]
    pub metadata: serde_json::Value,
}

/// Lifecycle state of a single [`Deliverable`].
///
/// Transitions are driven by [`crate::ports::Planner::mark_status`] and by
/// the Planner's own scheduling logic (e.g. `Pending` -> `Ready` when the
/// last prerequisite completes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DeliverableStatus {
    /// At least one prerequisite is not yet complete.
    Pending,
    /// All prerequisites are complete; not yet acquired.
    Ready,
    /// A caller holds an active lock and is working on this deliverable.
    InProgress,
    /// The caller marked this deliverable complete. The lock has been
    /// released.
    Complete,
    /// The caller marked this deliverable failed. The lock has been
    /// released. The reason is preserved for audit and for human / planner
    /// retry decisions.
    Failed {
        /// Human-readable failure reason supplied by the caller.
        reason: String,
    },
}

/// Snapshot of a held lock. The Planner records one [`LockInfo`] per
/// acquired deliverable and surfaces them in [`Cohort::locks`] and
/// [`PlanStatus::locks_held`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockInfo {
    pub plan_id: PlanId,
    pub deliverable_id: String,
    pub caller_id: CallerId,
    pub acquired_at: DateTime<Utc>,
    /// TTL deadline. After this instant, the lock is treated as expired
    /// and the Planner is free to release it on any subsequent operation.
    pub expires_at: DateTime<Utc>,
}

/// One row in a [`Cohort`]: a deliverable held under a single lock.
///
/// Pairing the deliverable with its lock structurally makes the
/// invariant "the i-th deliverable is held under the i-th lock"
/// unrepresentable as broken at the type level. Pre-F5 the same
/// invariant lived in a doc comment + a runtime test assertion; a
/// future refactor that pushed to one parallel Vec but not the other
/// would silently violate it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CohortRow {
    pub deliverable: Deliverable,
    pub lock: LockInfo,
}

/// Result of a successful [`crate::ports::Planner::acquire_cohort`]
/// call.
///
/// SPEC §33 audit fixup (F5 INTERFACE_GAP-001) — the previous shape
/// was two parallel vectors (`deliverables: Vec<Deliverable>` +
/// `locks: Vec<LockInfo>`) with the index-pairing invariant carried
/// only in docs. F5 tightens to `rows: Vec<CohortRow>` so the
/// invariant is type-enforced.
///
/// The wire shape is preserved: `#[serde(into = "FlatCohort", try_from =
/// "FlatCohort")]` projects to/from the historical two-array JSON so
/// MCP clients see no breaking change. Deserialization is fallible
/// (CMP-032): mismatched `deliverables`/`locks` lengths are a corrupt
/// payload and are rejected rather than silently truncated.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(into = "FlatCohort", try_from = "FlatCohort")]
pub struct Cohort {
    pub plan_id: PlanId,
    pub rows: Vec<CohortRow>,
}

/// Error returned when a [`FlatCohort`] wire payload cannot be decoded into a
/// [`Cohort`] — currently only the deliverables/locks length mismatch
/// (CMP-032). Carries both lengths for triage and implements `Display` so it
/// satisfies serde's `try_from` error bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CohortDecodeError {
    pub deliverables: usize,
    pub locks: usize,
}

impl std::fmt::Display for CohortDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "COHORT_LENGTH_MISMATCH: deliverables ({}) and locks ({}) arrays \
             must be the same length; each deliverable is held under exactly one lock",
            self.deliverables, self.locks
        )
    }
}

impl std::error::Error for CohortDecodeError {}

/// Wire-shape adapter for [`Cohort`] — keeps the historical
/// `{plan_id, deliverables, locks}` JSON layout so the F5 in-Rust
/// API tightening does NOT break MCP clients. NEVER referenced
/// directly; only via the `From`/`Into` plumbing.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FlatCohort {
    plan_id: PlanId,
    deliverables: Vec<Deliverable>,
    locks: Vec<LockInfo>,
}

impl From<Cohort> for FlatCohort {
    fn from(cohort: Cohort) -> Self {
        let mut deliverables = Vec::with_capacity(cohort.rows.len());
        let mut locks = Vec::with_capacity(cohort.rows.len());
        for row in cohort.rows {
            deliverables.push(row.deliverable);
            locks.push(row.lock);
        }
        FlatCohort {
            plan_id: cohort.plan_id,
            deliverables,
            locks,
        }
    }
}

impl TryFrom<FlatCohort> for Cohort {
    type Error = CohortDecodeError;

    fn try_from(flat: FlatCohort) -> Result<Self, Self::Error> {
        // CMP-032 — mismatched lengths mean an unpaired deliverable or lock.
        // Previously this truncated to the shorter array, silently dropping
        // lock-grant entries (or deliverables). That hides a real corruption,
        // so we now reject the payload outright.
        if flat.deliverables.len() != flat.locks.len() {
            return Err(CohortDecodeError {
                deliverables: flat.deliverables.len(),
                locks: flat.locks.len(),
            });
        }
        let rows = flat
            .deliverables
            .into_iter()
            .zip(flat.locks)
            .map(|(deliverable, lock)| CohortRow { deliverable, lock })
            .collect();
        Ok(Cohort {
            plan_id: flat.plan_id,
            rows,
        })
    }
}

/// Read-only snapshot of a plan's current state, returned by
/// [`crate::ports::Planner::status`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStatus {
    pub plan_id: PlanId,
    /// Per-deliverable status. Order matches insertion order in the
    /// originally submitted [`PlanGraph::deliverables`] so callers can
    /// render a stable table.
    pub deliverables: Vec<(String, DeliverableStatus)>,
    /// Ids on the longest dependency chain, in execution order. Empty
    /// when the plan has no deliverables.
    pub critical_path: Vec<String>,
    /// Sum of `estimated_effort_hours` along `critical_path`. Deliverables
    /// without an estimate contribute zero.
    pub critical_path_hours: f32,
    /// Every lock currently active across the plan.
    pub locks_held: Vec<LockInfo>,
}

/// Errors returned by [`crate::ports::Planner`] methods.
///
/// Every variant carries enough context to log without further lookup. The
/// stable string token at the start of the `#[error(..)]` message doubles
/// as the wire-level error code surfaced by PA4's MCP server, so the
/// variant prefixes (`LOCK_HELD`, `LOCK_NOT_HELD`, etc.) MUST NOT change
/// without bumping the MCP server schema.
#[derive(Debug, Error)]
pub enum PlannerError {
    /// The requested deliverable is already locked by another caller. The
    /// `holder` field surfaces the conflicting caller for human triage.
    #[error("LOCK_HELD: deliverable {deliverable_id} in plan {plan_id} is locked by {holder}")]
    LockHeld {
        plan_id: String,
        deliverable_id: String,
        holder: String,
    },

    /// The caller invoked an operation that requires holding a lock
    /// (`mark_status`, `heartbeat`) but the lock is held by someone else,
    /// or no lock exists at all.
    #[error("LOCK_NOT_HELD: caller {caller_id} does not hold lock on {deliverable_id}")]
    LockNotHeld {
        caller_id: String,
        deliverable_id: String,
    },

    /// The lock the caller is referencing has passed its TTL. The Planner
    /// is free to reclaim the deliverable for another caller.
    #[error("LOCK_EXPIRED: lock on {deliverable_id} expired at {expired_at}")]
    LockExpired {
        deliverable_id: String,
        expired_at: DateTime<Utc>,
    },

    /// `acquire_cohort` discovered that the candidate deliverable's
    /// `owned_files` overlap with files held by an existing lock. Surfaced
    /// as a distinct variant (rather than `LOCK_HELD`) because the
    /// conflict is at the *file* level, not the deliverable level.
    #[error(
        "OVERLAP_DETECTED: deliverable {deliverable_id} owns files {files:?} that overlap with \
         currently locked files"
    )]
    OverlapDetected {
        deliverable_id: String,
        files: Vec<PathBuf>,
    },

    /// `acquire_cohort` cannot include the candidate because at least one
    /// of its prerequisites is not yet [`DeliverableStatus::Complete`].
    /// This is not a hard error for the whole call (other cohort members
    /// may still be returned); it surfaces when a caller explicitly
    /// requests an ineligible deliverable.
    #[error(
        "MISSING_PREREQUISITE: deliverable {deliverable_id} requires {prereq} which is not \
         Complete"
    )]
    MissingPrerequisite {
        deliverable_id: String,
        prereq: String,
    },

    /// The plan id supplied to a lookup or mutation does not correspond to
    /// any submitted plan.
    #[error("PLAN_NOT_FOUND: {plan_id}")]
    PlanNotFound { plan_id: String },

    /// The deliverable id supplied to a lookup or mutation does not
    /// correspond to any deliverable in the named plan.
    #[error("DELIVERABLE_NOT_FOUND: {deliverable_id} in plan {plan_id}")]
    DeliverableNotFound {
        plan_id: String,
        deliverable_id: String,
    },

    /// The submitted graph fails a structural invariant: duplicate ids,
    /// unknown prerequisite reference, cycle, empty `owned_files`, etc.
    /// The `reason` is the precise failure message.
    #[error("INVALID_GRAPH: {reason}")]
    InvalidGraph { reason: String },

    /// Catch-all for backend failures (DB unavailable, serialization
    /// errors against the persistence layer, etc.). Wraps the underlying
    /// `anyhow::Error` so the caller can introspect via `source()`.
    #[error("BACKEND_ERROR: {0}")]
    BackendError(#[source] anyhow::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wire-shape smoke test: constructing a `PlanGraph` with one
    /// deliverable and round-tripping it through `serde_json` must
    /// preserve every field. The MCP server in PA4 depends on this
    /// invariant.
    #[test]
    fn plan_graph_serde_roundtrip() -> Result<(), serde_json::Error> {
        let graph = PlanGraph {
            deliverables: vec![Deliverable {
                id: "d1".to_string(),
                owned_files: vec![PathBuf::from("src/foo.rs"), PathBuf::from("src/bar.rs")],
                prerequisites: vec!["d0".to_string()],
                estimated_effort_hours: Some(1.5),
                metadata: serde_json::json!({"description": "smoke test"}),
            }],
            max_chained_dispatch: Some(8),
        };

        let json = serde_json::to_string(&graph)?;
        let back: PlanGraph = serde_json::from_str(&json)?;

        assert_eq!(back.deliverables.len(), 1);
        let d = &back.deliverables[0];
        assert_eq!(d.id, "d1");
        assert_eq!(
            d.owned_files,
            vec![PathBuf::from("src/foo.rs"), PathBuf::from("src/bar.rs")]
        );
        assert_eq!(d.prerequisites, vec!["d0".to_string()]);
        assert_eq!(d.estimated_effort_hours, Some(1.5));
        assert_eq!(d.metadata, serde_json::json!({"description": "smoke test"}));
        assert_eq!(back.max_chained_dispatch, Some(8));
        Ok(())
    }

    /// `DeliverableStatus` uses an internally-tagged enum representation
    /// so the wire form matches what PA4's MCP server publishes. Pin the
    /// shape with an explicit JSON check so an accidental derive change
    /// fails loudly.
    #[test]
    fn deliverable_status_failed_carries_reason() -> Result<(), serde_json::Error> {
        let status = DeliverableStatus::Failed {
            reason: "tests failed".to_string(),
        };
        let json = serde_json::to_value(&status)?;
        assert_eq!(json["status"], "failed");
        assert_eq!(json["reason"], "tests failed");
        let back: DeliverableStatus = serde_json::from_value(json)?;
        assert_eq!(back, status);
        Ok(())
    }

    /// SPEC §33 audit fixup (F5 INTERFACE_GAP-001) — Cohort tightened
    /// from parallel vectors to `Vec<CohortRow>`, but the JSON wire
    /// shape MUST stay as `{plan_id, deliverables, locks}` so MCP
    /// clients see no breaking change. Pin both directions of the
    /// `serde(into/from)` adapter.
    #[test]
    fn cohort_wire_shape_preserves_two_array_layout() -> Result<(), serde_json::Error> {
        let plan_id = PlanId("plan_x".to_string());
        let now = chrono::Utc::now();
        let cohort = Cohort {
            plan_id: plan_id.clone(),
            rows: vec![
                CohortRow {
                    deliverable: Deliverable {
                        id: "d1".to_string(),
                        owned_files: vec![PathBuf::from("a.rs")],
                        prerequisites: vec![],
                        estimated_effort_hours: Some(1.0),
                        metadata: serde_json::Value::Null,
                    },
                    lock: LockInfo {
                        plan_id: plan_id.clone(),
                        deliverable_id: "d1".to_string(),
                        caller_id: CallerId("c1".to_string()),
                        acquired_at: now,
                        expires_at: now + chrono::Duration::seconds(60),
                    },
                },
                CohortRow {
                    deliverable: Deliverable {
                        id: "d2".to_string(),
                        owned_files: vec![PathBuf::from("b.rs")],
                        prerequisites: vec![],
                        estimated_effort_hours: Some(2.0),
                        metadata: serde_json::Value::Null,
                    },
                    lock: LockInfo {
                        plan_id: plan_id.clone(),
                        deliverable_id: "d2".to_string(),
                        caller_id: CallerId("c1".to_string()),
                        acquired_at: now,
                        expires_at: now + chrono::Duration::seconds(60),
                    },
                },
            ],
        };
        let json = serde_json::to_value(&cohort)?;
        // Wire shape: top-level keys are plan_id + deliverables + locks
        // (NOT `rows`). MCP clients depending on the historical shape
        // continue to see it.
        assert!(json.get("deliverables").is_some());
        assert!(json.get("locks").is_some());
        assert!(json.get("rows").is_none());
        let deliverables = json["deliverables"].as_array().unwrap();
        let locks = json["locks"].as_array().unwrap();
        assert_eq!(deliverables.len(), 2);
        assert_eq!(locks.len(), 2);
        // Position-aligned pairing on the wire.
        assert_eq!(deliverables[0]["id"], "d1");
        assert_eq!(locks[0]["deliverable_id"], "d1");

        // Round-trip back into the in-Rust row form.
        let back: Cohort = serde_json::from_value(json)?;
        assert_eq!(back.rows.len(), 2);
        assert_eq!(back.rows[0].deliverable.id, "d1");
        assert_eq!(back.rows[0].lock.deliverable_id, "d1");
        assert_eq!(back.rows[1].deliverable.id, "d2");
        assert_eq!(back.rows[1].lock.deliverable_id, "d2");
        Ok(())
    }

    /// CMP-032 — a wire payload whose `deliverables` and `locks` arrays have
    /// mismatched lengths is corrupt (an unpaired lock-grant or deliverable).
    /// Deserialization MUST error rather than silently truncate to the shorter
    /// array and drop the unpaired entry.
    #[test]
    fn cohort_rejects_mismatched_deliverables_and_locks_lengths() {
        let now = chrono::Utc::now();
        // Two deliverables but only one lock — the historical truncating impl
        // would have dropped d2 silently.
        let wire = serde_json::json!({
            "plan_id": "plan_x",
            "deliverables": [
                {
                    "id": "d1",
                    "owned_files": ["a.rs"],
                    "prerequisites": [],
                    "estimated_effort_hours": 1.0,
                    "metadata": null
                },
                {
                    "id": "d2",
                    "owned_files": ["b.rs"],
                    "prerequisites": [],
                    "estimated_effort_hours": 2.0,
                    "metadata": null
                }
            ],
            "locks": [
                {
                    "plan_id": "plan_x",
                    "deliverable_id": "d1",
                    "caller_id": "c1",
                    "acquired_at": now,
                    "expires_at": now + chrono::Duration::seconds(60)
                }
            ]
        });

        let err = serde_json::from_value::<Cohort>(wire).unwrap_err();
        assert!(
            err.to_string().contains("COHORT_LENGTH_MISMATCH"),
            "expected COHORT_LENGTH_MISMATCH, got: {err}"
        );
    }
}
