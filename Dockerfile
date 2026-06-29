# ciris-status — a ciris-server fabric node + a StatusAdapter
# (FSD/MONITORING_NODE_DESIGN.md). There is ONE build now: ciris-status is always
# a node (the optional `fabric` feature is gone). It links the persist/verify
# substrate via ciris-server: Flow A reads `capacity:*` from this node's own
# corpus, Flow B emits signed `health:liveness`.
#
# Build:
#   docker build -t ciris-status:latest .
#
# ZERO ENV (ciris-server 0.5): boot takes only `--home <path>` / `--key-id <name>`
# on the CLI (passed by docker-compose `command:`). No env vars. The node binds
# the Reticulum port (default :4242); the read API + the status routers bind
# port + 1 (default :4243) — point the status reverse-proxy there.

# The substrate graph (ciris-persist/verify/edge) requires a recent stable rustc
# (redb 4.1 → 1.89, time → 1.88).
ARG RUST_VERSION=1.90

# ── build stage ──────────────────────────────────────────────────────────────
# PIN the build base to bookworm (glibc 2.36) so it MATCHES the bookworm runtime
# stage below. The bare `-slim` tag tracks the latest Debian (trixie, glibc 2.39),
# which produced a binary needing GLIBC_2.39 that the bookworm runtime (2.36)
# could not load — `cirisstatus:v0.3.2: GLIBC_2.39 not found` (CIRISServer#33).
# Build glibc must be ≤ runtime glibc; pinning both to bookworm guarantees it.
FROM rust:${RUST_VERSION}-slim-bookworm AS build
WORKDIR /app

# Substrate build deps. libtss2-dev (ciris-keyring's TPM backend links tss2) +
# libsqlite3-dev (ciris-persist links sqlite3) + libudev-dev (ciris-server's
# serialport / serial-LoRa transport pulls libudev-sys, whose build script needs
# the libudev pkg-config); pkg-config + a C toolchain back the `-sys` crates;
# git fetches the substrate git deps.
RUN apt-get update && apt-get install -y --no-install-recommends \
        libtss2-dev libsqlite3-dev libudev-dev pkg-config build-essential git ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# ciris-server is a git pin (tag v0.5.0) — fetched by cargo from the committed
# Cargo.lock, so the build context is just this repo (no sibling path dep).
COPY Cargo.toml Cargo.lock ./
COPY src ./src

# `--locked` so the image build uses the committed Cargo.lock (reproducible).
RUN cargo build --release --locked \
    && strip target/release/ciris-status \
    && cp target/release/ciris-status /ciris-status

# ── runtime stage (slim) ─────────────────────────────────────────────────────
FROM debian:bookworm-slim
# Runtime libs:
#   - ca-certificates: outbound TLS probes; curl: HEALTHCHECK.
#   - libtss2-esys + tctildr: ciris-keyring's TPM backend (tss-esapi) links
#     `libtss2-esys.so.0` DYNAMICALLY — it is NOT static (the old comment claiming
#     so was wrong; the slim image crash-looped on `libtss2-esys.so.0: cannot open
#     shared object file`, CIRISServer#28). The lib must be present even with no TPM
#     (the dynamic link resolves at load; the backend then degrades to the software
#     keystore). sqlite IS static (`rusqlite/bundled`), so no libsqlite3 needed.
#   - libudev1: ciris-server's serialport / serial-LoRa transport links
#     `libudev.so.1` DYNAMICALLY (via libudev-sys); the runtime lib must be present
#     for the binary to load even with no serial hardware.
#   (Structural alternative — build this image FROM a published ciris-server base so
#    the substrate runtime libs inherit cleanly — is tracked as CIRISServer#28.)
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl libtss2-esys-3.0.2-0 libtss2-tctildr0 libudev1 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /ciris-status /usr/local/bin/ciris-status

# The node data dir (corpus DB + minted identity) and the uptime-history DB
# (<data_dir>/status.db) live on a volume. The data root is set with `--home` at
# boot (docker-compose passes `--home /data`); there are NO env vars.
VOLUME ["/data"]
# 4242 = Reticulum node port; 4243 = read API + status routers.
EXPOSE 4242 4243
HEALTHCHECK --interval=30s --timeout=5s --retries=3 \
    CMD curl -fsS http://localhost:4243/health || exit 1
# Zero-env: pass `--home <path>` / `--key-id <name>` as CMD args (compose does).
# Defaults if none given: --home /var/lib/ciris --key-id ciris-status.
ENTRYPOINT ["/usr/local/bin/ciris-status"]
