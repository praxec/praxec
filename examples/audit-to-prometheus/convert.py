#!/usr/bin/env python3
"""
Convert praxec audit JSONL to Prometheus metrics.

Reads audit events from stdin (one JSON line per event) and emits
Prometheus exposition-format metrics to stdout.

Usage:
  tail -f /var/log/praxec-audit.jsonl | python3 convert.py

Or as a one-shot:
  cat /var/log/praxec-audit.jsonl | python3 convert.py
"""

import json
import sys
from collections import Counter, defaultdict
from datetime import datetime, timezone

# Metric state
event_counts = Counter()  # event_type -> count
workflow_state_counts = Counter()  # state -> count
executor_results = Counter()  # (event_type, kind) -> count
guard_results = Counter()  # (guard_kind, result) -> count

# Track unique workflows for gauge
active_workflows = set()
completed_workflows = set()

METRIC_PREFIX = "praxec_audit"


def emit_gauge(name, value, labels=None):
    """Emit a Prometheus gauge line."""
    label_str = ""
    if labels:
        parts = [f'{k}="{v}"' for k, v in sorted(labels.items())]
        label_str = "{" + ",".join(parts) + "}"
    print(f"# HELP {name} {name}")
    print(f"# TYPE {name} gauge")
    print(f"{name}{label_str} {value}")


def emit_counter(name, value, labels=None):
    """Emit a Prometheus counter line."""
    label_str = ""
    if labels:
        parts = [f'{k}="{v}"' for k, v in sorted(labels.items())]
        label_str = "{" + ",".join(parts) + "}"
    print(f"# HELP {name} {name}")
    print(f"# TYPE {name} counter")
    print(f"{name}{label_str} {value}")


def process_event(event):
    event_type = event.get("event_type", "unknown")
    event_counts[event_type] += 1

    # Track workflow lifecycle
    workflow_id = event.get("workflow_id")
    if workflow_id:
        if event_type == "workflow.started":
            active_workflows.add(workflow_id)
        elif event_type == "workflow.completed":
            active_workflows.discard(workflow_id)
            completed_workflows.add(workflow_id)

    # Track workflow state transitions
    payload = event.get("payload", {})
    if event_type == "workflow.transitioned":
        state = payload.get("state", "unknown")
        workflow_state_counts[state] += 1

    # Track executor results
    if event_type.startswith("executor."):
        kind = payload.get("kind", "unknown")
        executor_results[(event_type, kind)] += 1

    # Track guard results
    if event_type == "guard.evaluated":
        guard_kind = payload.get("guard_kind", "unknown")
        result = "pass" if payload.get("passed", False) else "fail"
        guard_results[(guard_kind, result)] += 1


def emit_metrics():
    timestamp = int(datetime.now(timezone.utc).timestamp())

    # Event count by type
    for event_type, count in sorted(event_counts.items()):
        emit_counter(
            f"{METRIC_PREFIX}_events_total",
            count,
            {"event_type": event_type},
        )

    # Active workflows gauge
    emit_gauge(
        f"{METRIC_PREFIX}_workflows_active",
        len(active_workflows),
    )

    # Workflow state distribution
    for state, count in sorted(workflow_state_counts.items()):
        emit_counter(
            f"{METRIC_PREFIX}_workflow_state_total",
            count,
            {"state": state},
        )

    # Executor results
    for (event_type, kind), count in sorted(executor_results.items()):
        emit_counter(
            f"{METRIC_PREFIX}_executor_total",
            count,
            {"event": event_type, "kind": kind},
        )

    # Guard results
    for (guard_kind, result), count in sorted(guard_results.items()):
        emit_counter(
            f"{METRIC_PREFIX}_guard_total",
            count,
            {"guard_kind": guard_kind, "result": result},
        )

    # Timestamp for scrape freshness
    print(f"{METRIC_PREFIX}_last_scrape_timestamp {timestamp}")


def main():
    print(
        f"# praxec audit metrics (generated {datetime.now(timezone.utc).isoformat()})"
    )

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            event = json.loads(line)
            process_event(event)
        except json.JSONDecodeError:
            print(f"# WARNING: skipping invalid JSON line", file=sys.stderr)
            continue

    emit_metrics()


if __name__ == "__main__":
    main()
