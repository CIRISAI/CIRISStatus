# Build the static-ish ciris-status binary (rustls, no openssl) and ship it on a
# slim runtime. Replaces the CIRISLens API container; listens on :8200.
FROM rust:1.86-slim AS build
WORKDIR /app
COPY Cargo.toml ./
COPY src ./src
RUN cargo build --release && strip target/release/ciris-status

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /app/target/release/ciris-status /usr/local/bin/ciris-status
# History DB lives on a mounted volume in prod.
ENV STATUS_LISTEN_ADDR=0.0.0.0:8200 \
    STATUS_DB_PATH=/data/status.db
VOLUME ["/data"]
EXPOSE 8200
HEALTHCHECK --interval=30s --timeout=5s --retries=3 \
    CMD curl -fsS http://localhost:8200/health || exit 1
ENTRYPOINT ["/usr/local/bin/ciris-status"]
