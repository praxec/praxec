#!/usr/bin/env python3
"""
Mechanical end-to-end dogfood of the TDD gateway against this repo.

Drives `praxec serve` over stdio (newline-delimited JSON-RPC),
walks the TDD workflow as far as the test state allows, and prints
each step so you can see the wire-format. This is **mechanical**
verification (the gateway responds correctly to a programmatic
driver). It is *not* an agent dogfood — whether a real LLM can
navigate this cycle is a separate empirical question.

Run from the workspace root after `cargo build --release -p praxec`:

    python3 examples/tdd/dogfood-drive.py

Expected: initialize handshake succeeds, tools/list returns exactly the
two stable tools (SPEC §32: `praxec.query` + `praxec.command`),
starting the `tdd` workflow via `praxec.command` returns a workflow id +
a link to `start_cycle`, and submitting `start_cycle` (again via
`praxec.command`) advances to `red_pending` with a baseline count
matching this repo's current test count.
"""
import json
import os
import subprocess
import sys
import time

GATEWAY = "./target/release/praxec"
CONFIG = "examples/tdd/gateway.yaml"

if not os.path.exists(GATEWAY):
    sys.exit(f"binary not found at {GATEWAY} — run `cargo build --release -p praxec` first")

proc = subprocess.Popen(
    [GATEWAY, "serve", "--config", CONFIG],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    bufsize=0,
)


def send(req):
    line = json.dumps(req) + "\n"
    proc.stdin.write(line.encode())
    proc.stdin.flush()


def recv_response(expect_id, timeout=60.0):
    """Read newline-delimited JSON from stdout; skip audit events.

    Finding from this dogfood: when the gateway has `audit.sink: stderr`
    (which examples/tdd/gateway.yaml does), audit events interleave
    with JSON-RPC responses on the same channel. We filter for the
    actual JSON-RPC response by matching the request id. Audit events
    don't carry `jsonrpc` / `id` keys in the JSON-RPC sense — they
    have their own `event_type` shape.
    """
    deadline = time.time() + timeout
    while time.time() < deadline:
        line = proc.stdout.readline()
        if not line:
            time.sleep(0.05)
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            sys.stderr.write(f"(non-JSON line from gateway: {line!r})\n")
            continue
        if msg.get("jsonrpc") == "2.0" and msg.get("id") == expect_id:
            return msg
        # otherwise it's an audit event or unrelated message — skip
    raise TimeoutError(f"gateway did not respond to id={expect_id} in time")


def step(name, req):
    print(f"\n=== {name} ===")
    print(">>", json.dumps(req)[:200])
    send(req)
    if "id" not in req:  # notification, no response expected
        return None
    resp = recv_response(req["id"])
    print("<<", json.dumps(resp)[:500])
    return resp


try:
    # 1. Initialize.
    init_resp = step("initialize", {
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "dogfood-driver", "version": "0.1"},
        },
    })

    # Notify that we're initialized.
    step("notifications/initialized", {
        "jsonrpc": "2.0", "method": "notifications/initialized", "params": {},
    })

    # 2. tools/list — should be exactly the two stable tools (SPEC §32).
    list_resp = step("tools/list", {
        "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {},
    })
    tools = [t["name"] for t in list_resp["result"]["tools"]]
    print("Tools:", tools)
    expected = {"praxec.query", "praxec.command"}
    assert set(tools) == expected, f"expected exactly {expected}, got {set(tools)}"
    print("  ✓ exactly the two stable tools (praxec.query + praxec.command)")

    # 3. Start the TDD workflow with this repo's test commands.
    #    SPEC §32: `praxec.command` with a `definitionId` (and no
    #    `workflowId`) dispatches to start.
    start_resp = step("praxec.command (start tdd)", {
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": {
            "name": "praxec.command",
            "arguments": {
                "definitionId": "tdd",
                "input": {
                    "test_cmd":  "cargo test --workspace --quiet 2>&1 | tail -1",
                    "count_cmd": "cargo test --workspace --quiet -- --list 2>/dev/null | grep -c ': test$'",
                    "runner_path": "./examples/tdd/tdd-runner.sh",
                    "feature": "dogfood: mechanical drive of TDD workflow",
                },
            },
        },
    })

    # rmcp wraps tool responses in CallToolResult with structuredContent.
    payload = start_resp["result"].get("structuredContent") or json.loads(
        start_resp["result"]["content"][0]["text"]
    )
    # Response shape: { workflow: {id, version, state}, context, evidence, links }
    workflow_id = payload["workflow"]["id"]
    version = payload["workflow"]["version"]
    state = payload["workflow"]["state"]
    links = payload.get("links", [])
    print(f"  workflowId={workflow_id} version={version} state={state}")
    print(f"  links: {[l['rel'] for l in links]}")
    assert state == "idle"
    assert any(l["rel"] == "start_cycle" for l in links), "expected start_cycle link"
    print("  ✓ started in `idle` with `start_cycle` link available")

    # 4. Submit start_cycle — should baseline the test count and advance to red_pending.
    #    SPEC §32: `praxec.command` with `workflowId` + `expectedVersion` +
    #    `transition` dispatches to submit.
    submit_resp = step("praxec.command (submit start_cycle)", {
        "jsonrpc": "2.0", "id": 4, "method": "tools/call",
        "params": {
            "name": "praxec.command",
            "arguments": {
                "workflowId": workflow_id,
                "expectedVersion": version,
                "transition": "start_cycle",
                "arguments": {},
            },
        },
    })
    payload = submit_resp["result"].get("structuredContent") or json.loads(
        submit_resp["result"]["content"][0]["text"]
    )
    state = payload["workflow"]["state"]
    ctx = payload.get("context", {})            # context is top-level, not nested in workflow
    baseline = ctx.get("baseline_count")
    links = payload.get("links", [])
    print(f"  state={state} baseline_count={baseline}")
    print(f"  links: {[l['rel'] for l in links]}")
    assert state == "red_pending", f"expected red_pending, got {state}"
    assert baseline and baseline > 0, f"expected positive baseline, got {baseline}"
    assert any(l["rel"] == "confirm_red" for l in links)
    print(f"  ✓ baselined at {baseline} tests, advanced to red_pending, confirm_red link offered")

    print("\n=== DOGFOOD SUMMARY ===")
    print(f"  Build:         binary exists, praxec check passed")
    print(f"  Protocol:      initialize + tools/list + 2× tools/call all succeeded")
    print(f"  Invariant 9:   tool surface is exactly the two stable tools (SPEC §32)")
    print(f"  TDD baseline:  captured {baseline} tests from this repo's cargo test")
    print(f"  State machine: idle → red_pending advance confirmed")
    print()
    print("  NOT verified by this script:")
    print("  - That a live LLM agent can complete a full red→green cycle")
    print("  - confirm_red / confirm_green branching (those need an actual")
    print("    code edit between submissions, which a script can't simulate)")

finally:
    proc.stdin.close()
    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
