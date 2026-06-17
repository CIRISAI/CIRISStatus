# ciris-status — the ciris.ai public health/status surface + (with the `fabric`
# feature) Node B of FSD/MONITORING_NODE_DESIGN.md.
#
# Two build modes via the FEATURES build-arg:
#   * default ("")          — the cost-safe outbound prober + SQLite uptime +
#                             website sockets. Self-contained (rustls, no
#                             openssl); roster served from cache (empty).
#   * "fabric"              — the real fabric node: links the persist/verify
#                             substrate (Flow A reads `capacity:*`, Flow B emits
#                             signed `health:liveness`). Needs the substrate build
#                             deps (libtss2-dev for the TPM/keyring backend,
#                             libsqlite3-dev), matching CIRISServer's release CI.
#
# Build the fabric image (Node B):
#   docker build --build-arg FEATURES=fabric -t ciris-status:fabric .
# Build the default status-page image:
#   docker build -t ciris-status:latest .
#
# Listens on :8200 (drop-in for the CIRISLens API container).

# The fabric feature pulls the substrate graph (ciris-persist/verify) whose
# transitive deps require a recent stable rustc (redb 4.1 → 1.89, time → 1.88).
# Bumped from 1.86 so `--features fabric` compiles in-container.
ARG RUST_VERSION=1.90

# ── build stage ──────────────────────────────────────────────────────────────
FROM rust:${RUST_VERSION}-slim AS build
ARG FEATURES=""
WORKDIR /app

# Substrate build deps. libtss2-dev + libsqlite3-dev are required by the
# `fabric` feature (ciris-keyring's TPM backend links tss2; ciris-persist links
# sqlite3). Harmless for the default build. pkg-config + a C toolchain back the
# `-sys` crates; git fetches the substrate git deps under `fabric`.
RUN apt-get update && apt-get install -y --no-install-recommends \
        libtss2-dev libsqlite3-dev pkg-config build-essential git ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Cache deps: copy the manifests + lockfile first.
COPY Cargo.toml Cargo.lock ./
COPY src ./src

# `--locked` so the image build uses the committed Cargo.lock (reproducible).
RUN if [ -n "$FEATURES" ]; then \
        cargo build --release --locked --features "$FEATURES"; \
    else \
        cargo build --release --locked; \
    fi \
    && strip target/release/ciris-status \
    && cp target/release/ciris-status /ciris-status

# ── runtime stage (slim) ─────────────────────────────────────────────────────
FROM debian:bookworm-slim
# Runtime libs: ca-certificates for outbound TLS probes; curl for HEALTHCHECK.
# NOTE: the binary statically links sqlite (`rusqlite/bundled`) and, in the
# fabric build, the keyring/TPM backend — `ldd` shows NO libtss2/libsqlite3
# dynamic dependency — so the runtime stage stays minimal for BOTH build modes
# (verified: `ldd target/release/ciris-status` → only libc/libm/libgcc).
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /ciris-status /usr/local/bin/ciris-status

# History DB (and, for the fabric node, the corpus mount) live on volumes.
ENV STATUS_LISTEN_ADDR=0.0.0.0:8200 \
    STATUS_DB_PATH=/data/status.db
VOLUME ["/data"]
EXPOSE 8200
HEALTHCHECK --interval=30s --timeout=5s --retries=3 \
    CMD curl -fsS http://localhost:8200/health || exit 1
ENTRYPOINT ["/usr/local/bin/ciris-status"]
