# Multi-stage build for the webhook-receiver service.
#
# IMPORTANT: the Docker build context MUST be the workspace root (the repo
# root), NOT this crate directory. aish is a cargo workspace whose root member
# is the (heavy) `aish` package, so `cargo build -p webhook-receiver` needs the
# whole workspace present to resolve. The .dockerignore at the repo root keeps
# this context lean (excludes target/, .git/, etc.).
#
# rust:1-bookworm (latest 1.x) is required because the workspace root package
# is edition 2024 (needs rustc >= 1.85); the old rust:1.84 image cannot parse it.
FROM rust:1-bookworm AS builder

WORKDIR /app
RUN apt-get update \
    && apt-get install -y pkg-config libssl-dev sqlite3 \
    && rm -rf /var/lib/apt/lists/*

# Copy the entire workspace (context = repo root).
COPY . .

# Build only the webhook-receiver binary and its dependency subtree.
RUN cargo build -p webhook-receiver --release

# ---- Runtime stage -------------------------------------------------------
# debian:bookworm-slim matches the builder's glibc (2.36). libssl3 is required
# because sqlx is built with native-tls (openssl) — the binary dynamically
# links libssl.so.3 / libcrypto.so.3 even though it only talks to SQLite.
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y ca-certificates sqlite3 curl libssl3 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /app/target/release/webhook-receiver /usr/local/bin/

# Data directory (Fly mounts the persistent volume over /data at runtime).
# Pre-create the directory with writable permissions.
RUN mkdir -p /data && chmod 777 /data

# In-memory database — no pre-initialization needed.

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:8080/health || exit 1

ENV DATABASE_URL=sqlite://:memory:
ENV PORT=8080
ENV RUST_LOG=webhook_receiver=info

EXPOSE 8080

CMD ["webhook-receiver"]
