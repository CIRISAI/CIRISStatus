//! ciris-status — the small standalone service that serves ciris.ai's public
//! health/status surface, replacing the CIRISLens API's status subset so the
//! status page survives Lens's retirement.
//!
//! Routes (drop-in for the Lens nginx route `agents.ciris.ai/lens/api/…`):
//!   GET /health                  — liveness
//!   GET /v1/status               — local providers (live)
//!   GET /api/v1/status           — aggregated multi-region (live)
//!   GET /api/v1/status/history   — daily uptime rollup (SQLite)
//!
//! Pure outbound HTTP probes + a SQLite uptime history written by a 60s poller.
//! No Grafana/Timescale/OAuth/ingest.

mod aggregate;
mod config;
mod history;
mod model;
mod probe;

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};

use config::Config;
use model::HistoryResponse;

#[derive(Clone)]
struct AppState {
    cfg: Arc<Config>,
    client: reqwest::Client,
    db: history::Db,
}

fn now_z() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

async fn root(State(st): State<AppState>) -> impl IntoResponse {
    Json(json!({ "service": "ciris-status", "version": st.cfg.version }))
}

async fn health(State(st): State<AppState>) -> impl IntoResponse {
    Json(json!({ "status": "healthy", "timestamp": now_z(), "version": st.cfg.version }))
}

async fn v1_status(State(st): State<AppState>) -> impl IntoResponse {
    Json(aggregate::service_status(&st.cfg, &st.client).await)
}

async fn api_status(State(st): State<AppState>) -> impl IntoResponse {
    Json(aggregate::aggregated_status(&st.cfg, &st.client).await)
}

#[derive(Deserialize)]
struct HistoryParams {
    days: Option<i64>,
    region: Option<String>,
}

fn bad(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "detail": msg }))).into_response()
}

async fn history(State(st): State<AppState>, Query(q): Query<HistoryParams>) -> Response {
    let days = q.days.unwrap_or(30);
    if !(1..=365).contains(&days) {
        return bad("Days must be between 1 and 365");
    }
    let region = q.region.filter(|r| !r.is_empty());
    if let Some(r) = &region {
        if !matches!(r.as_str(), "us" | "eu" | "global") {
            return bad("Invalid region. Must be one of: us, eu, global");
        }
    }
    match history::query_history(&st.db, days, region.as_deref()) {
        Ok(hist) => Json(HistoryResponse {
            days,
            region,
            history: hist,
        })
        .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "history query failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "detail": "Failed to fetch history" })),
            )
                .into_response()
        }
    }
}

fn cors(cfg: &Config) -> CorsLayer {
    let origins: Vec<HeaderValue> = cfg
        .cors_origins
        .iter()
        .filter_map(|o| o.parse().ok())
        .collect();
    CorsLayer::new()
        .allow_origin(AllowOrigin::list(origins))
        .allow_methods([Method::GET])
        .allow_headers(Any)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg = Arc::new(Config::from_env());
    let db = history::init(&cfg.db_path)?;
    let client = reqwest::Client::builder()
        .user_agent(concat!("ciris-status/", env!("CARGO_PKG_VERSION")))
        .build()?;

    tracing::info!(
        listen = %cfg.listen_addr,
        db = %cfg.db_path,
        poll_s = cfg.poll_seconds,
        regions = cfg.regions.iter().filter(|r| r.billing_url.is_some() || r.proxy_url.is_some()).count(),
        external = cfg.external.len(),
        "ciris-status starting"
    );

    // Loudly flag any provider set to send its live key — that path can be
    // BILLABLE (Brave has no free tier/health endpoint as of 2026). The
    // recommended pattern for billable providers is PASSIVE monitoring: let the
    // proxy report their health from real traffic (folded in via the proxy's
    // /v1/status), and don't synthetic-probe them with the key at all.
    for ext in cfg.external.iter().filter(|e| e.authenticated) {
        tracing::warn!(
            provider = ext.key,
            "authenticated health probing ENABLED — the live API key will be sent and may be \
             BILLABLE per request; prefer passive monitoring (proxy /v1/status) for paid providers"
        );
    }

    // Background uptime poller.
    tokio::spawn(history::run_poller(
        Arc::clone(&cfg),
        client.clone(),
        Arc::clone(&db),
    ));

    let state = AppState {
        cfg: Arc::clone(&cfg),
        client,
        db,
    };
    let app = Router::new()
        .route("/", get(root))
        .route("/health", get(health))
        .route("/v1/status", get(v1_status))
        .route("/api/v1/status", get(api_status))
        .route("/api/v1/status/history", get(history))
        .layer(cors(&cfg))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&cfg.listen_addr).await?;
    tracing::info!(addr = %cfg.listen_addr, "listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
