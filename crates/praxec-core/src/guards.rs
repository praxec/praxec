use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use thiserror::Error;

use crate::model::*;
use crate::ports::{EvidenceStore, GuardEvaluator};

/// SPEC §9 — the closed set of guard kinds recognised by
/// `DefaultGuardEvaluator`. The runtime evaluator matches exhaustively
/// on this enum so adding a variant forces every dispatch site to
/// handle it (compiler error otherwise). The validator parses
/// guard `kind:` strings through `from_token` so an invalid kind is
/// rejected at `praxec check` time, before any workflow runs.
///
/// **Wire format stays JSON strings.** Workflow YAML/JSON sources
/// continue to write `kind: permission` etc.; this enum only governs
/// dispatch inside the gateway.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GuardKind {
    Permission,
    Role,
    /// Inline expression with operands and operators (`==`, `!=`, `<`,
    /// `starts_with`, …) — see [`eval_expr`].
    Expr,
    /// Deprecated alias for [`GuardKind::Expr`] kept for backward
    /// compatibility; logs a deprecation warning at evaluation time.
    Jsonpath,
    AllOf,
    AnyOf,
    Not,
    GuidanceAcknowledged,
    ScriptAcknowledged,
    Evidence,
}

impl GuardKind {
    /// Every defined variant. Iterate this to enumerate the valid kinds
    /// (used by validator diagnostics and the in-sync test).
    pub const ALL: &'static [GuardKind] = &[
        GuardKind::Permission,
        GuardKind::Role,
        GuardKind::Expr,
        GuardKind::Jsonpath,
        GuardKind::AllOf,
        GuardKind::AnyOf,
        GuardKind::Not,
        GuardKind::GuidanceAcknowledged,
        GuardKind::ScriptAcknowledged,
        GuardKind::Evidence,
    ];

    /// Parse the wire-format string. Returns `None` for any value not in
    /// the closed set; callers turn that into an `INVALID_GUARD_KIND`
    /// diagnostic (at load) or error (at runtime). Named `from_token` (not
    /// `from_str`) to match the sibling closed-set parsers (`CapVerb::from_token`)
    /// and because it returns `Option`, not the `FromStr` trait's `Result`.
    pub fn from_token(s: &str) -> Option<Self> {
        match s {
            "permission" => Some(GuardKind::Permission),
            "role" => Some(GuardKind::Role),
            "expr" => Some(GuardKind::Expr),
            "jsonpath" => Some(GuardKind::Jsonpath),
            "all_of" => Some(GuardKind::AllOf),
            "any_of" => Some(GuardKind::AnyOf),
            "not" => Some(GuardKind::Not),
            "guidance_acknowledged" => Some(GuardKind::GuidanceAcknowledged),
            "script_acknowledged" => Some(GuardKind::ScriptAcknowledged),
            "evidence" => Some(GuardKind::Evidence),
            _ => None,
        }
    }

    /// The canonical wire-format string. Round-trips with `from_token`.
    pub fn as_str(self) -> &'static str {
        match self {
            GuardKind::Permission => "permission",
            GuardKind::Role => "role",
            GuardKind::Expr => "expr",
            GuardKind::Jsonpath => "jsonpath",
            GuardKind::AllOf => "all_of",
            GuardKind::AnyOf => "any_of",
            GuardKind::Not => "not",
            GuardKind::GuidanceAcknowledged => "guidance_acknowledged",
            GuardKind::ScriptAcknowledged => "script_acknowledged",
            GuardKind::Evidence => "evidence",
        }
    }
}

/// Raised when an `expr` guard references `$.context.X` (or another rooted
/// scope) for a path that does not resolve to *any* value — distinct from a
/// path that resolves to an explicit `null`. SPEC §9 mandates the runtime
/// fail fast on this case rather than silently coercing the missing read
/// to `null` (which would let the guard evaluate to a meaningless `false`).
#[derive(Debug, Error)]
#[error("GUARD_UNSET_SLOT: guard reads `{path}` which is not set on the workflow context")]
pub struct UnsetSlotError {
    pub path: String,
}

/// The built-in `GuardEvaluator`. Handles `permission`, `role`, `expr`,
/// and `evidence` kinds out of the box. The `evidence` kind needs an
/// `EvidenceStore` to check against — without one the guard *cannot be
/// satisfied* and fails closed (returns `false`), mirroring the
/// `guidance_acknowledged` / `script_acknowledged` ack guards. A
/// governance gate must never fail open just because its backing store
/// was left unwired.
pub struct DefaultGuardEvaluator {
    evidence: Option<Arc<dyn EvidenceStore>>,
    ack_store: Option<Arc<dyn crate::ports::GuidanceAcknowledgmentStore>>,
    script_ack_store: Option<Arc<dyn crate::ports::ScriptAcknowledgmentStore>>,
}

impl Default for DefaultGuardEvaluator {
    fn default() -> Self {
        Self::new()
    }
}

impl DefaultGuardEvaluator {
    pub fn new() -> Self {
        Self {
            evidence: None,
            ack_store: None,
            script_ack_store: None,
        }
    }

    pub fn with_evidence(evidence: Arc<dyn EvidenceStore>) -> Self {
        Self {
            evidence: Some(evidence),
            ack_store: None,
            script_ack_store: None,
        }
    }

    pub fn with_ack_store(
        mut self,
        ack_store: Arc<dyn crate::ports::GuidanceAcknowledgmentStore>,
    ) -> Self {
        self.ack_store = Some(ack_store);
        self
    }

    /// SPEC §22 — wire a script-acknowledgment store. Required for
    /// workflows that use the `script_acknowledged` guard (e.g.
    /// review-before-execute gates on destructive scripts).
    pub fn with_script_ack_store(
        mut self,
        store: Arc<dyn crate::ports::ScriptAcknowledgmentStore>,
    ) -> Self {
        self.script_ack_store = Some(store);
        self
    }
}

#[async_trait]
impl GuardEvaluator for DefaultGuardEvaluator {
    async fn evaluate(
        &self,
        guard: &Value,
        instance: &WorkflowInstance,
        arguments: &Value,
        principal: &Principal,
    ) -> anyhow::Result<bool> {
        let raw_kind = guard.get("kind").and_then(Value::as_str).unwrap_or("");
        let Some(kind) = GuardKind::from_token(raw_kind) else {
            // SPEC §9 — guard kinds are a closed set; anything else is
            // an invalid config. `praxec check` rejects these at load
            // time (`validate_guard_kinds`); this runtime arm is the
            // defense-in-depth backstop for callers that submit
            // pre-validated definitions or build guards in code. The
            // empty-string case covers a guard object that omitted
            // `kind:` entirely.
            let shown = if raw_kind.is_empty() {
                "<missing>"
            } else {
                raw_kind
            };
            let valid = GuardKind::ALL
                .iter()
                .map(|k| k.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(anyhow::anyhow!(
                "INVALID_GUARD_KIND: '{shown}' is not a valid guard kind (valid: {valid})"
            ));
        };
        // EXHAUSTIVE — adding a `GuardKind` variant without a match arm
        // is a compiler error. That's the poka-yoke: the validator and
        // evaluator can't drift out of sync without `cargo build`
        // failing first.
        match kind {
            GuardKind::Permission => {
                let required = guard
                    .get("permission")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                Ok(principal.permissions.iter().any(|p| p == required))
            }

            GuardKind::Role => {
                let required = guard
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                Ok(principal.roles.iter().any(|r| r == required))
            }

            GuardKind::Expr | GuardKind::Jsonpath => {
                if kind == GuardKind::Jsonpath {
                    tracing::warn!("guard kind \"jsonpath\" is deprecated; use \"expr\" instead");
                }
                let expr = guard.get("expr").and_then(Value::as_str).unwrap_or("");
                eval_expr(expr, instance, arguments)
            }

            GuardKind::AllOf => {
                let inner = guard
                    .get("guards")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                for g in inner {
                    let pass = self.evaluate(&g, instance, arguments, principal).await?;
                    if !pass {
                        return Ok(false);
                    }
                }
                Ok(true)
            }

            GuardKind::AnyOf => {
                let inner = guard
                    .get("guards")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if inner.is_empty() {
                    // Vacuously: nothing to satisfy → false (consistent with `requires` semantics).
                    return Ok(false);
                }
                // SPEC §9 interplay with `any_of`: a sub-guard reading an
                // unset slot is *not* a workflow-level failure if a sibling
                // clause passes — the author explicitly opted into "any of
                // these works". Errors are remembered and only surfaced if
                // no sibling satisfies. This preserves fail-fast for the
                // "no clause covered me" case without breaking the
                // declarative use of `any_of` over partially-written slots.
                let mut first_err: Option<anyhow::Error> = None;
                for g in inner {
                    match self.evaluate(&g, instance, arguments, principal).await {
                        Ok(true) => return Ok(true),
                        Ok(false) => {}
                        Err(e) => {
                            if first_err.is_none() {
                                first_err = Some(e);
                            }
                        }
                    }
                }
                match first_err {
                    Some(e) => Err(e),
                    None => Ok(false),
                }
            }

            GuardKind::Not => {
                let inner = guard
                    .get("guard")
                    .ok_or_else(|| anyhow::anyhow!("`not` guard needs a `guard:` body"))?;
                let pass = self.evaluate(inner, instance, arguments, principal).await?;
                Ok(!pass)
            }

            GuardKind::GuidanceAcknowledged => {
                // SPEC §5.9 + §17.4 — pass iff `gateway.describe` was called
                // for `subject` against this workflow AND the recorded
                // body-hash matches the current snapshot's hash for the
                // subject. Hash flip invalidates the ack (TRIZ-bounded
                // semantic teeth, FMECA FM-4).
                let subject = guard
                    .get("subject")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        anyhow::anyhow!("guidance_acknowledged guard needs `subject`")
                    })?;

                // Look up the expected hash from the per-instance skill
                // library snapshot. If the subject isn't in the snapshot
                // we fail explicitly with GUIDANCE_SUBJECT_UNKNOWN rather
                // than a generic false — surfaces author errors loudly.
                let expected_hash = instance
                    .definition
                    .pointer("/_skillsLibrary")
                    .and_then(Value::as_object)
                    .and_then(|lib| lib.get(subject))
                    .and_then(|entry| entry.get("hash"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "GUIDANCE_SUBJECT_UNKNOWN: subject '{subject}' not present in workflow snapshot"
                        )
                    })?;

                let Some(store) = self.ack_store.as_ref() else {
                    // No ack store wired? The guard cannot be satisfied —
                    // fail rather than silently pass. Authoring workflows
                    // that use this guard MUST wire an ack store.
                    return Ok(false);
                };
                let recorded = store.last_acknowledged_hash(&instance.id, subject).await?;
                Ok(recorded.as_deref() == Some(expected_hash))
            }

            GuardKind::ScriptAcknowledged => {
                // SPEC §22 — same shape as `guidance_acknowledged` but
                // operates on the SCRIPT library snapshot. Use case:
                // require operator/critic to have called gateway.describe
                // on a destructive script (e.g. `deploy.production.rollout`)
                // before the workflow may invoke it. Hash flip invalidates
                // the ack — editing the script body forces re-review.
                let subject = guard
                    .get("subject")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("script_acknowledged guard needs `subject`"))?;

                let expected_hash = instance
                    .definition
                    .pointer("/_scriptsLibrary")
                    .and_then(Value::as_object)
                    .and_then(|lib| lib.get(subject))
                    .and_then(|entry| entry.get("hash"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "SCRIPT_SUBJECT_UNKNOWN: subject '{subject}' not present \
                             in this workflow's _scriptsLibrary snapshot"
                        )
                    })?;

                let Some(store) = self.script_ack_store.as_ref() else {
                    // No script ack store wired? Guard cannot pass.
                    // Workflows that use this guard MUST wire one.
                    return Ok(false);
                };
                let recorded = store.last_acknowledged_hash(&instance.id, subject).await?;
                Ok(recorded.as_deref() == Some(expected_hash))
            }

            GuardKind::Evidence => {
                let (pass, _diag) = self.evaluate_evidence(guard, instance).await?;
                Ok(pass)
            }
        }
    }

    /// SPEC §20.1 + §20.4 — alongside the pass/fail bool, surface the
    /// specific filter that caused a rejection so callers can render
    /// `EVIDENCE_DIGEST_REQUIRED` / `EVIDENCE_CONFIDENCE_BELOW_THRESHOLD`
    /// in `error.code` instead of generic `GUARD_REJECTED`.
    async fn evaluate_with_diagnostic(
        &self,
        guard: &Value,
        instance: &WorkflowInstance,
        arguments: &Value,
        principal: &Principal,
    ) -> anyhow::Result<(bool, Option<String>)> {
        let raw_kind = guard.get("kind").and_then(Value::as_str).unwrap_or("");
        // Only `Evidence` carries the §20.4 diagnostic; everything else
        // dispatches through `evaluate` which surfaces INVALID_GUARD_KIND
        // for any non-recognised input.
        match GuardKind::from_token(raw_kind) {
            Some(GuardKind::Evidence) => self.evaluate_evidence(guard, instance).await,
            _ => {
                let pass = self.evaluate(guard, instance, arguments, principal).await?;
                Ok((pass, None))
            }
        }
    }
}

impl DefaultGuardEvaluator {
    /// SPEC §20.1 — evidence-guard evaluation with §20.4 diagnostic.
    /// Returns `(pass, Some(code))` when a §20.1 filter blocked a record
    /// that would otherwise have satisfied the quorum. Only emits the
    /// diagnostic when the filter-rejection is the *cause* of the failure;
    /// a missing record entirely (wrong kind) returns `(false, None)` so
    /// the caller stays on the generic `GUARD_REJECTED` path.
    async fn evaluate_evidence(
        &self,
        guard: &Value,
        instance: &WorkflowInstance,
    ) -> anyhow::Result<(bool, Option<String>)> {
        let requirements: Vec<EvidenceRequirement> = guard
            .get("requires")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(parse_evidence_requirement)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        // No store wired? The evidence guard CANNOT be satisfied — fail
        // closed rather than silently passing. An `evidence` guard is a
        // governance gate; a missing store is a wiring bug, not an
        // implicit allow. This mirrors the `guidance_acknowledged` /
        // `script_acknowledged` arms which also return `false` with no
        // store. Workflows that use `evidence` MUST wire an `EvidenceStore`
        // (`DefaultGuardEvaluator::with_evidence`).
        let Some(store) = &self.evidence else {
            return Ok((false, None));
        };

        let recorded = store.list(&instance.id).await?;
        for req in &requirements {
            // Count three buckets so we can attribute a quorum failure:
            //   - matching_full: passes both filters → counts
            //   - dropped_digest: right kind, missing digest under require_digest
            //   - dropped_confidence: right kind, fails min_confidence
            let mut matching_full = 0usize;
            let mut dropped_digest = 0usize;
            let mut dropped_confidence = 0usize;
            for e in &recorded {
                if e.kind != req.kind {
                    continue;
                }
                if req.require_digest && e.digest.is_none() {
                    dropped_digest += 1;
                    continue;
                }
                if let Some(threshold) = req.min_confidence {
                    match e.confidence {
                        Some(c) if c >= threshold => matching_full += 1,
                        _ => {
                            dropped_confidence += 1;
                            continue;
                        }
                    }
                } else {
                    matching_full += 1;
                }
            }
            if matching_full >= req.count {
                continue;
            }
            // Quorum failed. Attribute to a §20.4 code only if a §20.1
            // filter is *why* it failed — i.e. without the filter we'd
            // have had enough records. Otherwise (no relevant records at
            // all) stay on the generic path.
            let would_pass_without_digest_filter =
                req.require_digest && matching_full + dropped_digest >= req.count;
            let would_pass_without_confidence_filter =
                req.min_confidence.is_some() && matching_full + dropped_confidence >= req.count;
            if would_pass_without_digest_filter {
                return Ok((false, Some("EVIDENCE_DIGEST_REQUIRED".to_string())));
            }
            if would_pass_without_confidence_filter {
                return Ok((
                    false,
                    Some("EVIDENCE_CONFIDENCE_BELOW_THRESHOLD".to_string()),
                ));
            }
            // No filter-attributable cause. Generic quorum-miss.
            return Ok((false, None));
        }
        Ok((true, None))
    }
}

/// SPEC §9 + §20.1 — one requirement entry on an `evidence` guard's
/// `requires:` list. Carries the optional §20.1 filters.
struct EvidenceRequirement {
    kind: String,
    count: usize,
    /// SPEC §20.1 — minimum `Evidence.confidence` for a record to count.
    /// Records with no confidence are excluded when this is set.
    min_confidence: Option<f32>,
    /// SPEC §20.1 — when true, records without a `digest` are excluded.
    require_digest: bool,
}

fn parse_evidence_requirement(v: &Value) -> Option<EvidenceRequirement> {
    if let Some(s) = v.as_str() {
        return Some(EvidenceRequirement {
            kind: s.to_string(),
            count: 1,
            min_confidence: None,
            require_digest: false,
        });
    }
    if let Some(obj) = v.as_object() {
        let kind = obj.get("kind").and_then(Value::as_str)?.to_string();
        let count = obj
            .get("count")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(1);
        let min_confidence = obj
            .get("min_confidence")
            .and_then(Value::as_f64)
            .map(|n| n as f32);
        let require_digest = obj
            .get("require_digest")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        return Some(EvidenceRequirement {
            kind,
            count,
            min_confidence,
            require_digest,
        });
    }
    None
}

/// Evaluate a small expression of the form `<operand> <op> <operand>`.
///
/// Operands can be:
///   - A path: `$.context.x`, `$.arguments.y`, `$.workflow.input.z`
///   - A number: `42`, `3.14`
///   - A string: `"value"` or `'value'`
///   - A bool: `true`, `false`
///   - `null`
///
/// Operators:
///   - `==`, `!=`: any two same-typed operands (or null)
///   - `<`, `<=`, `>`, `>=`: numbers only
///
/// Path resolution semantics:
///
/// - `$.context.X` for an unset slot → **fail-fast** with [`UnsetSlotError`]
///   (SPEC §9: "a guard hitting an unset slot fails fast with rich context,
///   never a silent `false`"). A slot explicitly set to JSON `null` resolves
///   to `Value::Null` and is *not* an unset-slot error.
/// - `$.arguments.X` / `$.workflow.input.X` for a missing path resolve to
///   `Value::Null` (caller-controlled scopes; absent fields are legitimate).
/// - `$.workflow.id` / `$.workflow.state` / `$.workflow.version` resolve to
///   the instance's identity / current state / pinned definition version
///   (SPEC §5.2: "same `$.`-rooted paths as guards: `$.context.*`,
///   `$.workflow.input.*`, `$.workflow.*`").
///
/// `null == null` is true; `null` compared to anything else is false
/// (except `!=` which inverts).
fn eval_expr(expr: &str, instance: &WorkflowInstance, arguments: &Value) -> anyhow::Result<bool> {
    // SPEC §9 — an `expr` guard whose `expr:` is missing, empty, or not a
    // parseable binary comparison is a misconfiguration, not a deliberate
    // deny. `praxec check` rejects these at load time (`expr_parses`);
    // this runtime arm is the defense-in-depth backstop for callers that
    // submit pre-validated definitions or build guards in code — it mirrors
    // the INVALID_GUARD_KIND backstop in `evaluate`. Returning `Ok(false)`
    // here would be a silent deny that masks the broken expression.
    let Some((left, op, right)) = parse_binary_expr(expr) else {
        let shown = if expr.trim().is_empty() {
            "<missing>"
        } else {
            expr
        };
        return Err(anyhow::anyhow!(
            "INVALID_GUARD_EXPR: '{shown}' is not a parseable binary comparison \
             (expected `<operand> <op> <operand>`)"
        ));
    };

    let l = resolve_operand(left, instance, arguments)?;
    let r = resolve_operand(right, instance, arguments)?;

    Ok(compare_values(&l, op, &r))
}

/// Is `s` a `$.`-rooted operand reference (as opposed to a literal like `true`,
/// `42`, or `'go'`)?
pub fn is_rooted_operand(s: &str) -> bool {
    s.trim().starts_with("$.")
}

/// For a `$.`-rooted guard operand, does it name a scope the evaluator actually
/// resolves? A `false` means [`resolve_operand`] would have coalesced it to
/// `Null` — the silent-null footgun (`$.input.mode` resolving to null and making
/// a guard permanently false). V25 (`validate.rs`) rejects these at load; the
/// evaluator fails fast on them at eval.
///
/// This is the single source of truth for the resolvable-scope set: [`resolve_operand`]
/// dispatches on exactly these arms, and a poka-yoke test asserts the two cannot drift.
pub fn is_resolvable_guard_scope(s: &str) -> bool {
    let s = s.trim();
    matches!(
        s,
        "$.workflow.id" | "$.workflow.state" | "$.workflow.version"
    ) || s.starts_with("$.context.")
        || s.starts_with("$.arguments.")
        || s.starts_with("$.workflow.input.")
}

fn resolve_operand(
    s: &str,
    instance: &WorkflowInstance,
    arguments: &Value,
) -> anyhow::Result<Value> {
    let s = s.trim();

    // Quoted string literal.
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        return Ok(Value::String(s[1..s.len() - 1].to_string()));
    }
    // Bool / null.
    match s {
        "true" => return Ok(Value::Bool(true)),
        "false" => return Ok(Value::Bool(false)),
        "null" => return Ok(Value::Null),
        _ => {}
    }
    // Number.
    if let Ok(n) = s.parse::<f64>() {
        return Ok(serde_json::Number::from_f64(n)
            .map(Value::Number)
            .unwrap_or(Value::Null));
    }
    // Workflow identity / state / version — three closed paths. Order
    // matters: check before `$.workflow.input.` so the input prefix doesn't
    // accidentally swallow these.
    if s == "$.workflow.id" {
        return Ok(Value::String(instance.id.clone()));
    }
    if s == "$.workflow.state" {
        return Ok(Value::String(instance.state.clone()));
    }
    if s == "$.workflow.version" {
        return Ok(Value::String(instance.definition_version.clone()));
    }
    // Path — `$.context.*` fails fast on missing; other scopes coalesce.
    if let Some(path) = s.strip_prefix("$.context.") {
        return match instance.context.pointer(&path_to_pointer(path)) {
            Some(v) => Ok(v.clone()),
            None => Err(UnsetSlotError {
                path: format!("$.context.{path}"),
            }
            .into()),
        };
    }
    if let Some(path) = s.strip_prefix("$.arguments.") {
        return Ok(arguments
            .pointer(&path_to_pointer(path))
            .cloned()
            .unwrap_or(Value::Null));
    }
    if let Some(path) = s.strip_prefix("$.workflow.input.") {
        return Ok(instance
            .input
            .pointer(&path_to_pointer(path))
            .cloned()
            .unwrap_or(Value::Null));
    }
    // H4 — an unknown `$.`-rooted scope used to coalesce to `Null` (fail-open),
    // silently making a guard that reads it always-false (the `$.input.mode`
    // bug). Fail fast instead: V25 rejects these at load, and here we surface the
    // authoring error rather than pretend the operand is null. A NON-rooted bare
    // token (not a recognized literal) keeps the legacy `Null` for compatibility.
    if is_rooted_operand(s) {
        anyhow::bail!(
            "UNRESOLVABLE_GUARD_SCOPE: guard operand `{s}` names no resolvable scope — \
             use `$.context.*`, `$.arguments.*`, `$.workflow.input.*`, or \
             `$.workflow.{{id,state,version}}`"
        );
    }
    Ok(Value::Null)
}

/// Convert a dot-notation path (e.g. `items[0].name` or `items.0.name`)
/// into a JSON Pointer string (e.g. `/items/0/name`).
pub(crate) fn path_to_pointer(path: &str) -> String {
    let mut result = String::with_capacity(path.len() + 1);
    result.push('/');
    let mut i = 0;
    let bytes = path.as_bytes();
    while i < bytes.len() {
        match bytes[i] {
            b'.' => result.push('/'),
            b'[' => {
                result.push('/');
                i += 1;
                while i < bytes.len() && bytes[i] != b']' {
                    result.push(bytes[i] as char);
                    i += 1;
                }
                // skip the closing ']'
            }
            c => result.push(c as char),
        }
        i += 1;
    }
    result
}

/// SPEC §24.2 — evaluate a `parallel` `join: { expression: <expr> }`
/// condition against the just-aggregated executor output value. The
/// expression surface mirrors `expr` guards: binary comparisons
/// (`==`, `!=`, `<`, `<=`, `>`, `>=`, `starts_with`, `contains`) over
/// operands that are literals (numeric / quoted string / bool / null)
/// OR `$.<path>` references resolved against `output`.
///
/// **Evaluation timing.** The parallel executor calls this **after**
/// every branch has completed (or been cancelled / failed). Expression
/// joins do NOT support early-exit cancellation of siblings — by
/// construction, the expression cannot observe a still-running branch.
/// This is the structural answer to the SPEC §24.8 amendment criterion
/// concern about expression-based joins reading mid-flight state.
///
/// Returns `Ok(true)` if the expression evaluates truthy, `Ok(false)`
/// for falsy or unparseable input. `Err` is reserved for cases that
/// should hard-fail (currently none — the implementation is
/// intentionally tolerant of malformed expressions, mirroring `expr`
/// guards which return `false` on parse failure).
pub fn evaluate_join_expression(expr: &str, output: &Value) -> anyhow::Result<bool> {
    // Trim a leading `$.` if the operator passed a bare path expecting
    // truthiness check. Bare paths are convenience syntax for
    // "is this truthy?"; binary expressions get full operator handling.
    let trimmed = expr.trim();
    if parse_binary_expr(trimmed).is_none() {
        // Pure path → resolve and check truthiness.
        if let Some(path) = trimmed.strip_prefix("$.") {
            let resolved = output
                .pointer(&path_to_pointer(path))
                .cloned()
                .unwrap_or(Value::Null);
            return Ok(is_truthy(&resolved));
        }
        return Ok(false);
    }
    let Some((left, op, right)) = parse_binary_expr(trimmed) else {
        return Ok(false);
    };
    let l = resolve_output_operand(left, output);
    let r = resolve_output_operand(right, output);
    Ok(compare_values(&l, op, &r))
}

fn resolve_output_operand(s: &str, output: &Value) -> Value {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        return Value::String(s[1..s.len() - 1].to_string());
    }
    match s {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        "null" => return Value::Null,
        _ => {}
    }
    if let Ok(n) = s.parse::<f64>() {
        return serde_json::Number::from_f64(n)
            .map(Value::Number)
            .unwrap_or(Value::Null);
    }
    if let Some(path) = s.strip_prefix("$.") {
        return output
            .pointer(&path_to_pointer(path))
            .cloned()
            .unwrap_or(Value::Null);
    }
    Value::Null
}

fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn compare_values(a: &Value, op: &str, b: &Value) -> bool {
    // Numeric comparisons work whenever both sides are numbers.
    if let (Some(an), Some(bn)) = (a.as_f64(), b.as_f64()) {
        return match op {
            "==" => (an - bn).abs() < f64::EPSILON,
            "!=" => (an - bn).abs() >= f64::EPSILON,
            "<" => an < bn,
            "<=" => an <= bn,
            ">" => an > bn,
            ">=" => an >= bn,
            _ => false,
        };
    }
    // String operations
    match op {
        "starts_with" => {
            let sa = a.as_str().unwrap_or("");
            let sb = b.as_str().unwrap_or("");
            sa.starts_with(sb)
        }
        "contains" => {
            let sa = a.as_str().unwrap_or("");
            let sb = b.as_str().unwrap_or("");
            sa.contains(sb)
        }
        // Strings, bools, nulls — equality only.
        "==" => a == b,
        "!=" => a != b,
        _ => false,
    }
}

/// SPEC §9 — load-time check that an `expr`/`jsonpath` guard's expression
/// is a parseable binary comparison. Used by `validate_guard_kinds` so an
/// empty/garbage `expr:` is rejected at `praxec check` time rather than
/// silently evaluating to `false` (silent deny) at runtime. Mirrors what
/// [`eval_expr`] requires before it can resolve operands.
pub fn expr_parses(expr: &str) -> bool {
    parse_binary_expr(expr).is_some()
}

/// The two operands of a parseable comparison, trimmed — for V25's scope check.
/// `None` when the expr doesn't parse (a separate diagnostic already covers that).
pub fn expr_operands(expr: &str) -> Option<(&str, &str)> {
    parse_binary_expr(expr).map(|(l, _op, r)| (l, r))
}

fn parse_binary_expr(expr: &str) -> Option<(&str, &str, &str)> {
    // Order matters: "<=" / ">=" / "==" / "!=" must be tried before "<" / ">".
    // We also need to skip operators that fall inside quoted strings — a
    // string literal "is not" mustn't cause the splitter to break on `<`.
    for op in ["starts_with", "contains", "<=", ">=", "==", "!=", "<", ">"] {
        if let Some(idx) = find_op_outside_quotes(expr, op) {
            let (left, rest) = expr.split_at(idx);
            let right = &rest[op.len()..];
            return Some((left.trim(), op, right.trim()));
        }
    }
    None
}

/// Find the byte index of `needle` in `haystack` skipping any occurrence
/// inside single- or double-quoted regions.
fn find_op_outside_quotes(haystack: &str, needle: &str) -> Option<usize> {
    let bytes = haystack.as_bytes();
    let needle_bytes = needle.as_bytes();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i + needle_bytes.len() <= bytes.len() {
        let c = bytes[i];
        if !in_single && c == b'"' {
            in_double = !in_double;
            i += 1;
            continue;
        }
        if !in_double && c == b'\'' {
            in_single = !in_single;
            i += 1;
            continue;
        }
        if !in_single && !in_double && bytes[i..i + needle_bytes.len()] == *needle_bytes {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::*;
    use crate::model::WorkflowInstance;

    fn instance(context: serde_json::Value) -> WorkflowInstance {
        WorkflowInstance {
            id: "wf".into(),
            definition_id: "d".into(),
            definition_version: "0".into(),
            definition: json!({}),
            state: "s".into(),
            version: 0,
            input: json!({}),
            context,
            started_at: chrono::Utc::now(),
            run_env: crate::RunEnv::for_test(),
            cancelled_at: None,
            cancelled_reason: None,
            depth: 0,
            parent: None,
        }
    }

    // ── V25 / H4 — resolvable guard scope ───────────────────────────────────

    /// POKA-YOKE: `is_resolvable_guard_scope` must agree with what
    /// `resolve_operand` actually does — a resolvable scope never bails with
    /// UNRESOLVABLE_GUARD_SCOPE, and an unresolvable one always does. If someone
    /// adds a scope arm to `resolve_operand` without updating the predicate (or
    /// vice-versa), this fails.
    #[rstest]
    #[case("$.context.x", true)]
    #[case("$.arguments.y", true)]
    #[case("$.workflow.input.z", true)]
    #[case("$.workflow.id", true)]
    #[case("$.workflow.state", true)]
    #[case("$.workflow.version", true)]
    #[case("$.input.mode", false)] // the bug: not a resolvable guard scope
    #[case("$.workflow.foo", false)]
    #[case("$.output.x", false)]
    fn predicate_agrees_with_resolve_operand(#[case] operand: &str, #[case] resolvable: bool) {
        assert_eq!(is_resolvable_guard_scope(operand), resolvable);

        // Now the ground truth: does resolve_operand treat it as resolvable?
        let inst = instance(json!({ "x": 1 }));
        let bailed = resolve_operand(operand, &inst, &json!({ "y": 1 }))
            .err()
            .map(|e| e.to_string())
            .is_some_and(|m| m.contains("UNRESOLVABLE_GUARD_SCOPE"));
        // A resolvable scope must NOT bail as unresolvable; an unresolvable one MUST.
        assert_eq!(
            !bailed, resolvable,
            "resolve_operand disagrees for `{operand}`"
        );
    }

    /// H4 — an unresolvable `$.`-rooted operand fails fast at EVAL rather than
    /// coalescing to null and silently making the guard false.
    #[test]
    fn eval_fails_fast_on_an_unresolvable_scope() {
        let inst = instance(json!({}));
        let err = eval_expr("$.input.mode == 'auto'", &inst, &json!({}))
            .expect_err("must error, not silently return false");
        assert!(
            err.to_string().contains("UNRESOLVABLE_GUARD_SCOPE"),
            "{err}"
        );
    }

    // G2 — the `expr` evaluation matrix (testing-strategy): operators × operand
    // types × context paths, one row per case. The fixed context carries one of
    // each type so the path cases exercise real resolution.
    #[rstest]
    // numeric comparisons
    #[case("1 == 1", true)]
    #[case("1 != 2", true)]
    #[case("1 < 2", true)]
    #[case("2 <= 2", true)]
    #[case("3 > 2", true)]
    #[case("2 >= 3", false)]
    // string ops
    #[case("'a' == 'a'", true)]
    #[case("'a' != 'b'", true)]
    #[case("'abc' starts_with 'ab'", true)]
    #[case("'abc' contains 'b'", true)]
    #[case("'abc' starts_with 'xy'", false)]
    // bool / null
    #[case("true == true", true)]
    #[case("true != false", true)]
    #[case("null == null", true)]
    #[case("null != 1", true)]
    // cross-type: a number vs a string is never equal
    #[case("1 == 'a'", false)]
    // context-path resolution (against the fixed context below)
    #[case("$.context.x == 1", true)]
    #[case("$.context.x == 2", false)]
    #[case("$.context.s == 'abc'", true)]
    #[case("$.context.b == true", true)]
    #[case("$.context.n == null", true)]
    // a slot explicitly set to null resolves
    // a `<` inside quotes must not be parsed as the operator
    #[case("'a < b' == 'a < b'", true)]
    fn expr_eval(#[case] expr: &str, #[case] expected: bool) {
        let inst = instance(json!({ "x": 1, "s": "abc", "b": true, "n": null }));
        assert_eq!(eval_expr(expr, &inst, &json!({})).ok(), Some(expected));
    }

    #[test]
    fn an_expr_reading_an_unset_context_slot_fails_fast() {
        let inst = instance(json!({}));
        assert!(eval_expr("$.context.missing == 1", &inst, &json!({})).is_err());
    }

    #[test]
    fn an_unparseable_expr_does_not_parse() {
        assert!(!expr_parses("this is not an expression"));
    }

    #[test]
    fn a_binary_comparison_parses() {
        assert!(expr_parses("$.context.x == 1"));
    }

    // FIX H2 — an `expr` guard whose expression does not parse must FAIL
    // FAST at runtime (INVALID_GUARD_EXPR), not silently deny with
    // `Ok(false)`. This is the runtime backstop mirroring INVALID_GUARD_KIND.
    #[test]
    fn an_unparseable_expr_errors_at_runtime_not_silent_false() {
        let inst = instance(json!({}));
        let err = eval_expr("this is not an expression", &inst, &json!({}))
            .expect_err("an unparseable expr must error, not return Ok(false)");
        assert!(
            err.to_string().contains("INVALID_GUARD_EXPR"),
            "error should carry the named code, got: {err}"
        );
    }

    #[test]
    fn an_empty_expr_errors_at_runtime() {
        let inst = instance(json!({}));
        let err = eval_expr("", &inst, &json!({}))
            .expect_err("an empty expr must error, not return Ok(false)");
        let msg = err.to_string();
        assert!(msg.contains("INVALID_GUARD_EXPR"), "got: {msg}");
        assert!(msg.contains("<missing>"), "got: {msg}");
    }

    // FIX H1 — an `evidence` guard evaluated with no `EvidenceStore` wired
    // must FAIL CLOSED (Ok(false)), not default-allow (Ok(true)). A
    // governance gate cannot pass when its backing store is missing.
    #[tokio::test]
    async fn evidence_guard_with_no_store_fails_closed() {
        let evaluator = DefaultGuardEvaluator::new(); // no evidence store
        let inst = instance(json!({}));
        let guard = json!({ "kind": "evidence", "requires": ["tests_passed"] });
        let pass = evaluator
            .evaluate(&guard, &inst, &json!({}), &Principal::anonymous())
            .await
            .expect("evidence guard eval should not error");
        assert!(
            !pass,
            "an evidence guard with no store wired must fail closed, not default-allow"
        );
    }

    #[tokio::test]
    async fn evidence_guard_with_no_store_diagnostic_is_generic_deny() {
        // The §20.4 diagnostic path must also fail closed and stay on the
        // generic GUARD_REJECTED path (None) rather than passing.
        let evaluator = DefaultGuardEvaluator::new();
        let inst = instance(json!({}));
        let guard = json!({ "kind": "evidence", "requires": ["tests_passed"] });
        let (pass, diag) = evaluator
            .evaluate_evidence(&guard, &inst)
            .await
            .expect("evidence eval should not error");
        assert!(!pass, "no-store evidence guard must not pass");
        assert!(diag.is_none(), "no-store deny is a generic quorum-miss");
    }
}
