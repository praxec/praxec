# NFR: Provider Resilience

**Status:** Active  
**Branch:** preveti-resilience-nfr  
**Governing artifact:** This document.

> Formal intent-harness grounding is deferred until/unless praxec adopts a
> spec harness. This document is the governing artifact.

---

## Scope

This NFR governs how praxec classifies, reacts to, and surfaces provider
failures — including throttling, transient unavailability, authentication
errors, and timeouts — across all executor kinds (`rest`, `mcp`, `llm`,
`script`, `cli`). It covers the full call stack: `ExecutorError` taxonomy,
`ReliabilityPolicy` backoff/retry, runtime journal behaviour, and
observability.

---

## Requirements

### R1 — Typed Error Taxonomy

**Statement:** Every provider failure MUST be classified into one of the
following typed variants. Variants MUST NOT be collapsed — the distinction
must be preserved through the entire call stack (executor → reliability loop
→ audit event → response).

| Variant | HTTP/condition | Token (`errorClass`) |
|---------|---------------|----------------------|
| `Timeout` | 408, 504, network timeout | `timeout` |
| `RateLimited` | 429 | `rate_limited` |
| `Auth` | 401, 403 | `auth_error` |
| `Connection` | TCP/TLS/DNS failure | `connection_error` |
| `Transient` | 5xx (not 504) | `transient_error` |
| `Permanent` | other 4xx, config bugs | `permanent_error` |

The `Auth` variant is NEW and distinct from `Permanent` — it signals
credential failure (not a workflow-author bug), which requires a different
operational response (rotate credentials, not fix YAML).

**Acceptance criteria:**
- `ExecutorError::Auth` exists and classifies as `ErrorClass::Auth`
- `ErrorClass::Auth.token()` returns `"auth_error"`
- HTTP 401/403 from the REST executor maps to `ExecutorError::Auth`
- `Auth` is never included in the default `retryOn` list

**Conformance (post-fix):** MET — `error.rs`, `rest.rs`

---

### R2 — Per-Type Reaction

**Statement:** The reliability layer MUST apply a distinct reaction per error
class:

| Class | Required reaction |
|-------|------------------|
| `RateLimited` | Retry with exponential backoff + full-jitter; honor `Retry-After` header |
| `Transient` / `Connection` | Retry per policy with backoff |
| `Timeout` | Retry per policy |
| `Auth` | Fail-fast — NEVER retry |
| `Permanent` | Fail-fast — NEVER retry |

**Retry-After:** When the REST executor receives a 429 with a `Retry-After`
header, the raw value is embedded in the `RateLimited` error message so it
appears in the audit trail. The reliability layer does not yet parse
`Retry-After` to override the configured delay — see **NFR-R2-RETRY-AFTER**
recommendation below.

**Jitter:** Exponential backoff applies full-jitter (random ∈ [0, capped))
to prevent thundering-herd when multiple concurrent workflows retry the same
throttled provider.

**Acceptance criteria:**
- `auth_error` token is absent from `default_retry_on()`
- `retryable()` returns false for `ErrorClass::Auth`
- `backoff_delay()` with `Backoff::Exponential` returns a value in `[0, cap)`
  (not a fixed value) across repeated calls
- 429 responses with `Retry-After: 60` produce error messages containing
  `Retry-After: 60`

**Conformance (post-fix):** MET (jitter + Auth fail-fast + Retry-After in
message). Circuit-breaker and auto-parsed Retry-After delay are documented
as recommendations.

**Recommendation NFR-R2-CIRCUIT:** Add a per-executor circuit-breaker that
opens after N consecutive `Unavailable`/5xx failures and short-circuits calls
during a cool-off window. This requires external mutable state (a per-endpoint
failure counter + timestamp) that is currently absent. Implement as a
`CircuitBreakerPolicy` struct in `reliability.rs`, gated by a `circuitBreaker:`
block in the workflow definition.

**Recommendation NFR-R2-RETRY-AFTER:** Parse `Retry-After` as seconds or
HTTP-date and clamp the retry delay to `max(configured_delay, retry_after)`.
Requires passing the parsed value from the REST executor back to the
reliability loop — currently the reliability loop only receives an
`ExecutorError`, not a structured header value.

---

### R3 — Mid-Workflow Throttle Durability

**Statement:** A throttle (429) that occurs mid-workflow SHOULD pause
execution via the journal (durable suspend) rather than hard-failing the
workflow. After the backoff window, the workflow SHOULD resume transparently.

**Current state:** The runtime already supports durable journal-based suspend
for file locks (`_lock_wait`) and sub-workflow waits (`_subworkflow_wait`).
A throttle-specific suspend mechanism does not exist — the reliability layer
retries in-process (within a single `execute_with_reliability` call) rather
than durably parking the workflow.

**Conformance:** MISSING

**Recommendation NFR-R3-JOURNAL:** Add `ThrottleSuspend` as a third durable-
suspend kind in the runtime. When a REST/MCP executor receives a 429 and the
`Retry-After` window exceeds the configured in-process retry budget, the
executor should return `ExecuteResult { suspend: Some(ThrottleSuspend { ... }) }`
instead of retrying in-process. The runtime would then write `_throttle_wait`
into the persisted context, and a scheduler would re-drive after the window.
This is a significant runtime change (4–6 days); defer until throttle
suspension is observed in production.

---

### R4 — Idempotency

**Statement:** The reliability layer MUST only retry calls that are declared
idempotent. A non-idempotent call retried after a partial success can cause
duplicate side effects (double-charges, double-writes, etc.).

**Current state:** An idempotency key (`idempotencyKey: true` or a template)
is computed and shared across retries and fallbacks (`idempotency_key` in
`ExecuteRequest`). The REST executor forwards it as `Idempotency-Key` HTTP
header. However, the `retryable()` function does NOT consult the idempotency
key — it only checks whether the error class is in `retryOn`. If a workflow
author configures `retryOn: [transient_error]` on a non-idempotent REST call
(no `idempotencyKey`), the reliability layer will retry it anyway.

**Conformance:** PARTIAL — key exists and is forwarded; enforcement is absent.

**Recommendation NFR-R4-ENFORCE:** Add `idempotent: bool` to `RetryPolicy`
(default `false`). The `retryable()` function should return `false` when
`retry.idempotent == false && attempt > 1`, regardless of error class.
The `idempotencyKey` field can serve as an implicit `idempotent: true` signal.
This is a schema + runtime change; coordinate with workflow authors.

---

### R5 — Observability

**Statement:** Metrics and logs MUST separately surface throttled vs errored
vs timed-out vs auth failures. Each audit event carrying an executor failure
MUST include the HTTP status code (when available) and the `Retry-After` value
(when present).

**Current state:**
- Audit events carry `errorClass: err.class().token()` — distinguishes
  `rate_limited` / `timeout` / `auth_error` / `transient_error` / etc.
- `Retry-After` header value is now embedded in the `RateLimited` error
  message (post-fix), so it appears in the audit `error` field.
- HTTP status code is embedded in the error message string but not as a
  structured field — dashboards must text-match to extract it.
- No per-class counter metrics (Prometheus/StatsD) exist.

**Conformance:** PARTIAL — audit trail is good; structured status code and
metrics counters are absent.

**Recommendation NFR-R5-STATUS:** Add `httpStatus: u16` as a structured
field in the `executor.failed` / `executor.retrying` audit event payloads
when the error originated from an HTTP response. This requires threading the
status code through `ExecutorError` (a new `RateLimitedHttp { status, body,
retry_after }` variant, or a parallel metadata struct).

**Recommendation NFR-R5-METRICS:** Add a `MetricsCollector` port in
`reliability.rs` (default no-op) that increments per-class counters at each
`executor.retrying` and `executor.failed` event. Wire a Prometheus adapter
in the binary crate.

---

## Conformance Summary

| Requirement | Pre-fix | Post-fix | Scope |
|-------------|---------|----------|-------|
| R1 — Typed error taxonomy | Partial (no Auth) | **MET** | `error.rs`, `rest.rs` |
| R2 — Per-type reaction | Partial (no jitter, no auth fail-fast, Retry-After ignored) | **MET** | `reliability.rs`, `rest.rs`, `error.rs` |
| R3 — Journal durability | Missing | Missing | Recommendation only |
| R4 — Idempotency | Partial (key exists, not enforced) | Partial | Recommendation only |
| R5 — Observability | Partial (class token present, no structured status/metrics) | Partial | Recommendation only |

---

## File References

| File | Role |
|------|------|
| `crates/praxec-core/src/error.rs` | `ExecutorError` + `ErrorClass` taxonomy |
| `crates/praxec-core/src/reliability.rs` | `ReliabilityPolicy`, retry loop, jitter |
| `crates/praxec-executors/src/rest.rs` | HTTP status → `ExecutorError`, Retry-After |
| `crates/praxec-executors/src/mcp.rs` | MCP error → `ExecutorError` |
| `crates/praxec-core/src/runtime/runtime_submit.rs` | `dispatch_once` drives the reliability call |
