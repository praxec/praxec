# Builder — compiles the release gateway binary.
FROM rust:1.83-slim AS builder
WORKDIR /src
COPY . .
RUN cargo build --release -p praxec

# --- Gateway image (ghcr.io/praxec/praxec) — default target ---
FROM debian:bookworm-slim AS gateway
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/praxec /usr/local/bin/praxec

# Ownership annotation for the official MCP Registry. The value MUST
# match the `name` field in server.json — the registry reads this label
# off the published image to confirm the publisher owns the namespace.
LABEL io.modelcontextprotocol.server.name="io.github.praxec/praxec"

ENTRYPOINT ["praxec"]
CMD ["serve", "--config", "/config/gateway.yaml"]
