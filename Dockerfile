# ciris-status — a ciris-server fabric node + a StatusAdapter
# (FSD/MONITORING_NODE_DESIGN.md). There is ONE build now: ciris-status is always
# a node (the optional `fabric` feature is gone). It links the persist/verify
# substrate via ciris-server: Flow A reads `capacity:*` from this node's own
# corpus, Flow B emits signed `health:liveness`.
#
# Build:
#   docker build -t ciris-status:latest .
#
# The node binds the Reticulum port (default :4242); the read API + the status
# routers bind port + 1 (default :4243) — point the status reverse-proxy there.

# The substrate graph (ciris-persist/verify/edge) requires a recent stable rustc
# (redb 4.1 → 1.89, time → 1.88).
ARG RUST_VERSION=1.90

# ── build stage ──────────────────────────────────────────────────────────────
FROM rust:${RUST_VERSION}-slim AS build
WORKDIR /app

# Substrate build deps. libtss2-dev (ciris-keyring's TPM backend links tss2) +
# libsqlite3-dev (ciris-persist links sqlite3); pkg-config + a C toolchain back
# the `-sys` crates; git fetches the substrate git deps.
RUN apt-get update && apt-get install -y --no-install-recommends \
        libtss2-dev libsqlite3-dev pkg-config build-essential git ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# NOTE: ciris-status depends on ../CIRISServer as a path dep, so the build context
# must include both repos (build from a parent dir, or swap to the git pin once
# the adapter-seam ciris-server is tagged). The COPY below assumes ciris-status is
# the context root with CIRISServer available as a sibling path dep.
COPY Cargo.toml Cargo.lock ./
COPY src ./src

# `--locked` so the image build uses the committed Cargo.lock (reproducible).
RUN cargo build --release --locked \
    && strip target/release/ciris-status \
    && cp target/release/ciris-status /ciris-status

# ── runtime stage (slim) ─────────────────────────────────────────────────────
FROM debian:bookworm-slim
# Runtime libs: ca-certificates for outbound TLS probes; curl for HEALTHCHECK.
# The binary statically links sqlite (`rusqlite/bundled`) and the keyring/TPM
# backend — `ldd` shows NO libtss2/libsqlite3 dynamic dependency — so the runtime
# stage stays minimal.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /ciris-status /usr/local/bin/ciris-status

# The node data dir (corpus DB + minted identity) and the uptime-history DB live
# on volumes.
ENV CIRIS_HOME=/data/ciris \
    STATUS_DB_PATH=/data/status.db
VOLUME ["/data"]
# 4242 = Reticulum node port; 4243 = read API + status routers.
EXPOSE 4242 4243
HEALTHCHECK --interval=30s --timeout=5s --retries=3 \
    CMD curl -fsS http://localhost:4243/health || exit 1
ENTRYPOINT ["/usr/local/bin/ciris-status"]
