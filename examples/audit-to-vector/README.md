# Audit-to-SIEM via Vector

This example shows how to pipe praxec's structured audit events
into a SIEM using [Vector](https://vector.dev/).

## How it works

1. praxec writes one JSON line per audit event to a file
   (`audit.sink: file`).
2. Vector tails the file, parses each line, enriches it with severity
   and metadata, and forwards to a SIEM sink.

## Prerequisites

- [Vector](https://vector.dev/download/) installed
- praxec configured with `audit.sink: file`

## Quick start

```bash
# 1. Start praxec with file audit
cargo run -p praxec -- serve --config examples/simple-proxy.yaml &
PRAXEC_PID=$!

# 2. Start Vector with the provided config
vector --config examples/audit-to-vector/vector.toml

# 3. Make some calls to generate audit events
# (Vector will tail the audit file and forward events)

# 4. Stop
kill $PRAXEC_PID
```

## Sink alternatives

The provided config ships to Elasticsearch. To use a different sink,
replace the `[sinks.elasticsearch]` section:

| Sink | Vector config key |
|------|-------------------|
| Splunk HEC | `sinks.splunk_hec` |
| Loki | `sinks.loki` |
| Datadog Logs | `sinks.datadog_logs` |
| AWS CloudWatch | `sinks.cloudwatch_logs` |
| GCP Cloud Logging | `sinks.gcp_cloud_logging` |

## Audit event taxonomy

See [docs/reference/governance.md](../../docs/reference/governance.md#audit) for the
complete event type list and payload shapes.