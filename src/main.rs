//! ciris-status — the ciris.ai public health/status surface, now a **ciris-server
//! fabric node + a `StatusAdapter`** (mirrors CIRISAgent's adapter model).
//!
//! The whole node — the shared persist `Engine`, the Reticulum edge,
//! `consent:replication` peering, the read API, NodeCode, ownership, the safety
//! foundation, and NAT-traversal — is `ciris_server::serve_with_adapter`. The
//! status page is a `StatusAdapter` folded onto that SAME shared core: its routers
//! (`/health`, `/v1/status`, `/api/v1/status`, `/api/v1/status/history`,
//! `/api/v1/scoring`, the live SSE/WS sockets) merge onto the node's read-API
//! listener, and its background lifecycle probes the external services → emits
//! signed `health:liveness:v1` (Flow B) + rebuilds the public roster from this
//! node's OWN corpus (Flow A) → updates the cache + uptime history + live push.
//!
//! Env split: the node reads `CIRIS_SERVER_*` / `CIRIS_PEER_B_*` (identity, DSN,
//! listen, peering); the StatusAdapter reads only its probe targets, poll cadence,
//! and CORS origins (see `src/config.rs`).

mod adapter;
mod aggregate;
mod ceg;
mod config;
mod history;
mod model;
mod probe;
mod roster;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // The fabric node config (CIRIS_SERVER_* / CIRIS_PEER_B_*).
    let cfg = ciris_server::ServerConfig::from_env()?;
    // The status page, as an adapter folded onto the node's shared core.
    let adapter = std::sync::Arc::new(adapter::StatusAdapter::from_env()?);

    tracing::info!("ciris-status starting as a ciris-server node + StatusAdapter");
    ciris_server::serve_with_adapter(cfg, adapter).await
}
