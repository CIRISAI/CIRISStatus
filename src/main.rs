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
mod ceg;
mod config;
mod history;
mod model;
mod probe;
mod roster;

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures::stream::Stream;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt as _;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};

use config::Config;
use model::{HistoryResponse, LiveDelta};
use roster::RosterCache;

#[derive(Clone)]
struct AppState {
    cfg: Arc<Config>,
    client: reqwest::Client,
    db: history::Db,
    /// Flow A public roster snapshot (served by `/api/v1/scoring`).
    roster: RosterCache,
    /// Live-push fan-out for roster + health deltas (the "extra website sockets").
    live_tx: broadcast::Sender<LiveDelta>,
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

/// `GET /api/v1/scoring` — the public scoring roster (Flow A projection).
/// Drop-in replacement for lens-python's scoring feed. Served from the cache so
/// the request never blocks on the corpus.
async fn scoring(State(st): State<AppState>) -> impl IntoResponse {
    Json(st.roster.snapshot())
}

/// `GET /api/v1/status/live` (and `/api/v1/scoring/live`) — SSE live-push of
/// roster + health deltas. One of the "extra website sockets" from design §3.
async fn live_sse(
    State(st): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // Prime with the current snapshot, then stream subsequent deltas.
    let initial = LiveDelta {
        timestamp: now_z(),
        roster: Some(st.roster.snapshot()),
        overall: None,
    };
    let rx = st.live_tx.subscribe();
    let live = BroadcastStream::new(rx).filter_map(|r| r.ok());
    let stream = tokio_stream::once(initial)
        .chain(live)
        .map(|delta| Ok(Event::default().json_data(delta).unwrap_or_default()));
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

/// `GET /api/v1/status/ws` — websocket variant of the same live-push.
async fn live_ws(State(st): State<AppState>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| live_ws_loop(socket, st))
}

async fn live_ws_loop(mut socket: WebSocket, st: AppState) {
    // Send the current snapshot immediately.
    let initial = LiveDelta {
        timestamp: now_z(),
        roster: Some(st.roster.snapshot()),
        overall: None,
    };
    if let Ok(txt) = serde_json::to_string(&initial) {
        if socket.send(Message::Text(txt.into())).await.is_err() {
            return;
        }
    }
    let mut rx = st.live_tx.subscribe();
    loop {
        match rx.recv().await {
            Ok(delta) => {
                let txt = match serde_json::to_string(&delta) {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                if socket.send(Message::Text(txt.into())).await.is_err() {
                    return;
                }
            }
            // Lagged: skip ahead (the next snapshot tick recovers the client).
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Background loop that publishes roster + overall-health deltas onto the
/// live-push channel on the poll cadence. Cheap: it reuses the cached roster and
/// a single aggregated probe so subscribers get fresh `overall` + roster without
/// each socket re-probing.
async fn run_live_pusher(st: AppState) {
    let mut tick = tokio::time::interval(Duration::from_secs(st.cfg.poll_seconds));
    loop {
        tick.tick().await;
        if st.live_tx.receiver_count() == 0 {
            continue; // nobody listening — don't probe
        }
        let agg = aggregate::aggregated_status(&st.cfg, &st.client).await;
        let delta = LiveDelta {
            timestamp: now_z(),
            roster: Some(st.roster.snapshot()),
            overall: Some(agg.status),
        };
        let _ = st.live_tx.send(delta);
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

    // Live-push fan-out channel (roster + health deltas) for the website sockets.
    let (live_tx, _live_rx) = broadcast::channel::<LiveDelta>(64);

    let state = AppState {
        cfg: Arc::clone(&cfg),
        client,
        db,
        roster: RosterCache::default(),
        live_tx,
    };

    // Live-push driver (SSE/WS). Cost-safe: only probes when sockets are open.
    tokio::spawn(run_live_pusher(state.clone()));

    // Flow A roster refresher — only when built as a fabric node AND configured
    // with a corpus DSN + caller scope. Default build skips this entirely (the
    // roster stays an empty, well-formed public_sample projection).
    #[cfg(feature = "fabric")]
    fabric::spawn_flows(state.clone());

    let app = Router::new()
        .route("/", get(root))
        .route("/health", get(health))
        .route("/v1/status", get(v1_status))
        .route("/api/v1/status", get(api_status))
        .route("/api/v1/status/history", get(history))
        // ── Phase 2 monitoring-node sockets (design §3) ──────────────────────
        .route("/api/v1/scoring", get(scoring))
        .route("/api/v1/scoring/live", get(live_sse))
        .route("/api/v1/status/live", get(live_sse))
        .route("/api/v1/status/ws", get(live_ws))
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

// ─────────────────────────────────────────────────────────────────────────────
// Fabric node (Node B) — Flows A + B wired to the persist/verify substrate.
// Compiled only under `--features fabric`. Construction is env-gated: if the
// node isn't configured with a corpus DSN + signing seed, the flows simply don't
// start and ciris-status runs as the plain prober (every default endpoint works).
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(feature = "fabric")]
mod fabric {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;

    use ciris_persist::prelude::{Engine, LocalSigner, LocalSignerConfig};
    use ciris_persist::scope::CallerScope;

    use crate::ceg::emit::emit_liveness;
    use crate::ceg::{EpistemicMode, EvidenceRef, LivenessEnvelope};

    /// Env config for the fabric node. All-or-nothing: every var must be present
    /// or the flows are disabled.
    struct FabricConfig {
        dsn: String,
        signer: LocalSignerConfig,
        /// Map region-key → the keyed CIRIS service node's `key_id` we attest
        /// `health:liveness` about (design §2: keyed services are subjects;
        /// providers/regions fold in as evidence_refs). Format env:
        ///   STATUS_SERVICE_KEYS="us=k_service_us,eu=k_service_eu"
        service_keys: std::collections::BTreeMap<String, String>,
    }

    fn env_opt(k: &str) -> Option<String> {
        std::env::var(k).ok().filter(|v| !v.trim().is_empty())
    }

    fn load_config() -> Option<FabricConfig> {
        let dsn = env_opt("STATUS_CORPUS_DSN")?;
        let key_id = env_opt("STATUS_NODE_KEY_ID")?;
        let key_path = env_opt("STATUS_NODE_KEY_PATH")?;
        // PQC seed required for hybrid signing (sign_hybrid needs ML-DSA-65).
        let pqc_key_id = env_opt("STATUS_NODE_PQC_KEY_ID")?;
        let pqc_key_path = env_opt("STATUS_NODE_PQC_KEY_PATH")?;

        let mut service_keys = std::collections::BTreeMap::new();
        if let Some(raw) = env_opt("STATUS_SERVICE_KEYS") {
            for pair in raw.split(',') {
                if let Some((region, key)) = pair.split_once('=') {
                    service_keys.insert(region.trim().to_owned(), key.trim().to_owned());
                }
            }
        }

        Some(FabricConfig {
            dsn,
            signer: LocalSignerConfig {
                key_id,
                key_path: PathBuf::from(key_path),
                pqc_key_id: Some(pqc_key_id),
                pqc_key_path: Some(PathBuf::from(pqc_key_path)),
            },
            service_keys,
        })
    }

    /// Spawn the Flow A refresher + Flow B emitter if (and only if) the node is
    /// configured for the fabric.
    pub fn spawn_flows(state: AppState) {
        let cfg = match load_config() {
            Some(c) => c,
            None => {
                tracing::info!(
                    "fabric feature built but not configured (set STATUS_CORPUS_DSN + \
                     STATUS_NODE_KEY_* ) — running as plain prober; roster is empty"
                );
                return;
            }
        };
        tokio::spawn(run(state, cfg));
    }

    async fn run(state: AppState, cfg: FabricConfig) {
        let signer = match LocalSigner::from_config(&cfg.signer) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                tracing::error!(error = %e, "fabric: LocalSigner load failed; flows disabled");
                return;
            }
        };
        let engine = match Engine::with_signer(signer, &cfg.dsn).await {
            Ok(e) => Arc::new(e),
            Err(e) => {
                tracing::error!(error = %e, "fabric: Engine open failed; flows disabled");
                return;
            }
        };
        tracing::info!(dsn = %redact(&cfg.dsn), "fabric: Node B flows online");

        let mut tick = tokio::time::interval(std::time::Duration::from_secs(
            state.cfg.poll_seconds,
        ));
        // Both flows go through the SQLite backend handle: it implements
        // ReadEngine (Flow A) AND FederationDirectory (Flow B); `Engine` itself
        // implements neither directly.
        let backend = match engine.sqlite_backend() {
            Some(b) => Arc::clone(b),
            None => {
                tracing::error!("fabric: non-sqlite backend; flows need the directory handle");
                return;
            }
        };

        loop {
            tick.tick().await;
            // ── Flow A: refresh the public roster from signed capacity:* rows ──
            flow_a_refresh(backend.as_ref(), &state).await;
            // ── Flow B: probe + emit signed health:liveness per keyed service ──
            flow_b_emit(&engine, backend.as_ref(), &state, &cfg).await;
        }
    }

    /// Flow A: read capacity:* (public-tier) → roster cache → live push.
    async fn flow_a_refresh<R>(reader: &R, state: &AppState)
    where
        R: ciris_persist::ceg::ReadEngine,
    {
        match crate::roster::read::build_roster(reader, CallerScope::Unauthenticated).await {
            Ok(roster) => {
                state.roster.replace(roster.clone());
                let _ = state.live_tx.send(LiveDelta {
                    timestamp: now_z(),
                    roster: Some(roster),
                    overall: None,
                });
            }
            Err(e) => tracing::warn!(error = %e, "flow A roster refresh failed"),
        }
    }

    /// Flow B: per keyed CIRIS service, fold this poll's probe results into a
    /// `health:liveness` envelope and sign+emit it. Reuses the existing cost-safe
    /// aggregated probe (never authed-probes paid providers in the loop).
    async fn flow_b_emit<D>(engine: &Engine, dir: &D, state: &AppState, cfg: &FabricConfig)
    where
        D: ciris_persist::federation::FederationDirectory,
    {
        if cfg.service_keys.is_empty() {
            return; // nothing keyed to attest about
        }
        let agg = aggregate::aggregated_status(&state.cfg, &state.client).await;
        let now = chrono::Utc::now();
        let valid_until = now + chrono::Duration::seconds(state.cfg.poll_seconds as i64);

        for (region_key, service_key_id) in &cfg.service_keys {
            let region = match agg.regions.get(region_key) {
                Some(r) => r,
                None => continue,
            };
            // Region status → score; provider/proxy/infra detail → evidence_refs.
            let score = crate::ceg::liveness_score(&region.status);
            let mut evidence: Vec<EvidenceRef> = Vec::new();
            for (svc, summ) in &region.services {
                evidence.push(EvidenceRef {
                    ref_id: format!("service:{region_key}.{svc}"),
                    status: summ.status.clone(),
                    latency_ms: summ.latency_ms,
                    detail: None,
                });
            }
            // Non-keyed cross-region infra folded as evidence (not its own subject).
            for (name, d) in agg.llm_providers.iter().chain(agg.internal_providers.iter()) {
                evidence.push(EvidenceRef {
                    ref_id: format!("provider:{name}"),
                    status: d.status.clone(),
                    latency_ms: d.latency_ms,
                    detail: d.source.clone(),
                });
            }

            let env = LivenessEnvelope {
                attested_key_id: service_key_id.clone(),
                score,
                confidence: 0.9,
                context: format!("{} — region {region_key}", region.name),
                evidence,
                valid_until,
                asserted_at: now,
                // Region/proxy self-reports are folded → derivative; a direct
                // /health probe of the node would be `Direct`.
                epistemic_mode: EpistemicMode::Derivative,
            };

            match emit_liveness(engine, dir, &env).await {
                Ok(hash) => tracing::info!(
                    service = %service_key_id,
                    region = %region_key,
                    score,
                    content_hash = %hash,
                    "flow B: emitted signed health:liveness"
                ),
                Err(e) => tracing::warn!(error = %e, service = %service_key_id, "flow B emit failed"),
            }
        }
    }

    fn redact(dsn: &str) -> String {
        match dsn.split_once('@') {
            Some((_, host)) => format!("***@{host}"),
            None => dsn.to_owned(),
        }
    }
}
