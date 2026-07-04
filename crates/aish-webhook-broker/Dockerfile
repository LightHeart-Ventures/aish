# syntax=docker/dockerfile:1
#
# aish-webhook-broker — multi-stage build (TASK-263 / SPR-059 Phase 4.7)
#
# Build context is this crate directory (crates/aish-webhook-broker/). The crate
# is a standalone cargo workspace (its own Cargo.lock), so the image builds
# without pulling in the parent aish package's heavy build graph.
#
#   docker build -t aish-webhook-broker:latest crates/aish-webhook-broker
#   docker run -p 8080:8080 -v aish_broker_data:/var/lib aish-webhook-broker:latest
#
# ---- builder ----------------------------------------------------------------
FROM rust:1.83-slim-bookworm AS builder

WORKDIR /build

# Cache dependency compilation: copy manifests first, build a stub, then the
# real sources. rusqlite uses the bundled SQLite (needs a C toolchain).
RUN apt-get update \
    && apt-get install -y --no-install-recommends gcc libc6-dev \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo 'fn main() { println!("stub"); }' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

# Now copy the real sources and build for real.
COPY src ./src
# Bust the stub's cached crate artifact so the real main.rs is recompiled.
RUN touch src/main.rs \
    && cargo build --release --locked \
    && strip target/release/aish-webhook-broker

# ---- runtime ----------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

# ca-certificates for outbound TLS; curl for the container HEALTHCHECK.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

# Run as an unprivileged user; /var/lib is writable for the SQLite db.
RUN useradd --system --create-home --home-dir /home/broker --shell /usr/sbin/nologin broker \
    && mkdir -p /var/lib \
    && chown broker:broker /var/lib

COPY --from=builder /build/target/release/aish-webhook-broker /usr/local/bin/aish-webhook-broker

USER broker

EXPOSE 8080

ENV BROKER_LISTEN=0.0.0.0:8080 \
    BROKER_DB=/var/lib/aish-broker.db \
    BROKER_LOG_LEVEL=info

# The broker exposes GET /health → {"status":"ok",...}.
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8080/health || exit 1

ENTRYPOINT ["/usr/local/bin/aish-webhook-broker"]
