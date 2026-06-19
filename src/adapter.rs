//! `StatusAdapter` — the status page as a `ciris_server::Adapter`.
//!
//! ciris-status IS a ciris-server fabric node now: `serve_with_adapter` builds the
//! whole node (the shared persist `Engine`, the edge, `consent:replication`
//! peering, the read API, NodeCode, ownership, safety, NAT-traversal), and this
//! adapter folds the public status surface onto the SAME shared core, mirroring
//! CIRISAgent's adapter model.
//!
//!   * [`StatusAdapter::routers`] contributes the status HTTP routers (root,
//!     `/health`, `/v1/status`, `/api/status`, `/api/v1/history`,
//!     `/api/v1/scoring`, the live SSE/WS sockets), merged onto ciris-server's
//!     read-API listener (`:4243`). One node, one read surface.
//!   * [`StatusAdapter::run_lifecycle`] is the background poller: probe the
//!     external services → (a) emit signed `health:liveness:v1` into the node's
//!     own corpus (Flow B), (b) rebuild the Flow-A public roster from THIS node's
//!     OWN corpus (the rows replicated in under `consent:replication`), (c) update
//!     the roster cache + uptime history + broadcast the live delta. Loops on a
//!     tokio interval; exits cleanly when `shutdown` flips true.
//!   * [`StatusAdapter::start`] / [`StatusAdapter::stop`] log + prime the roster.
//!
//! The federation identity, DSN, listen address, and `consent:replication`
//! peering are all `ciris_server::ServerConfig`'s job (the `CIRIS_SERVER_*` /
//! `CIRIS_PEER_B_*` env). This adapter's own env is just probe targets, cadence,
//! and CORS — see [`StatusAdapter::from_env`].

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

use ciris_persist::scope::CallerScope;
use ciris_server::{Adapter, AdapterConfig, AdapterContext, AdapterStatus};

use crate::config::Config;
use crate::model::{HistoryResponse, LiveDelta};
use crate::roster::RosterCache;
use crate::{aggregate, history};

/// The status page's shared state — what the old `main.rs` `AppState` held,
/// minus the node concerns (engine/identity/DSN). The engine is reached through
/// the [`AdapterContext`] the lifecycle/router builders receive.
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

// ── HTTP handlers ─────────────────────────────────────────────────────────────

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
/// Served from the cache so the request never blocks on the corpus.
async fn scoring(State(st): State<AppState>) -> impl IntoResponse {
    Json(st.roster.snapshot())
}

/// `GET /api/v1/status/live` (and `/api/v1/scoring/live`) — SSE live-push of
/// roster + health deltas.
async fn live_sse(
    State(st): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
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
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return,
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

// ── The adapter ───────────────────────────────────────────────────────────────

/// The status page, as a `ciris_server::Adapter`.
pub struct StatusAdapter {
    state: AppState,
}

impl StatusAdapter {
    /// Build the adapter from the environment: the probe targets, poll cadence,
    /// CORS origins, uptime-history DB path, the HTTP client, and the live-push
    /// channel. The node identity/DSN/listen come from `ServerConfig`, not here.
    pub fn from_env() -> anyhow::Result<Self> {
        let cfg = Arc::new(Config::from_env());
        let db = history::init(&cfg.db_path)?;
        let client = reqwest::Client::builder()
            .user_agent(concat!("ciris-status/", env!("CARGO_PKG_VERSION")))
            .build()?;

        tracing::info!(
            db = %cfg.db_path,
            poll_s = cfg.poll_seconds,
            regions = cfg
                .regions
                .iter()
                .filter(|r| r.billing_url.is_some() || r.proxy_url.is_some())
                .count(),
            external = cfg.external.len(),
            "StatusAdapter configured"
        );

        // Loudly flag any provider set to send its live key — that path can be
        // BILLABLE (Brave has no free tier/health endpoint as of 2026).
        for ext in cfg.external.iter().filter(|e| e.authenticated) {
            tracing::warn!(
                provider = ext.key,
                "authenticated health probing ENABLED — the live API key will be sent and may be \
                 BILLABLE per request; prefer passive monitoring (proxy /v1/status) for paid providers"
            );
        }

        let (live_tx, _live_rx) = broadcast::channel::<LiveDelta>(64);
        let state = AppState {
            cfg,
            client,
            db,
            roster: RosterCache::default(),
            live_tx,
        };
        Ok(StatusAdapter { state })
    }

    /// Rebuild the Flow-A roster from THIS node's OWN corpus and publish it to the
    /// cache + the live channel. The reader is `engine.sqlite_backend()` (the
    /// `ReadEngine` handle); the scope is `Unauthenticated` (the public projection).
    async fn refresh_roster(&self, ctx: &AdapterContext) {
        let reader = match ctx.engine.sqlite_backend() {
            Some(b) => b,
            None => {
                tracing::warn!("roster refresh: non-sqlite backend; cannot read own corpus");
                return;
            }
        };
        match crate::roster::read::build_roster(reader.as_ref(), CallerScope::Unauthenticated).await
        {
            Ok(roster) => {
                self.state.roster.replace(roster.clone());
                let _ = self.state.live_tx.send(LiveDelta {
                    timestamp: now_z(),
                    roster: Some(roster),
                    overall: None,
                });
            }
            Err(e) => tracing::warn!(error = %e, "Flow A roster refresh failed"),
        }
    }

    /// Flow B: probe the configured external services, fold the result into a
    /// `health:liveness:v1` envelope ABOUT this node, and sign + emit it into the
    /// node's own corpus. (Per-keyed-service attestation can layer on later via
    /// `STATUS_SERVICE_KEYS`; the node always self-attests its own liveness here,
    /// and that key is already registered by `serve_with_adapter`.)
    async fn emit_liveness(&self, ctx: &AdapterContext, agg: &crate::model::AggregatedStatus) {
        let now = chrono::Utc::now();
        let valid_until = now + chrono::Duration::seconds(self.state.cfg.poll_seconds as i64);

        // Fold every probed region/provider as evidence behind the node's own
        // liveness score (non-keyed infra is evidence, not a subject — §1/§2.2).
        let mut evidence: Vec<crate::ceg::EvidenceRef> = Vec::new();
        for (region_key, region) in &agg.regions {
            for (svc, summ) in &region.services {
                evidence.push(crate::ceg::EvidenceRef {
                    ref_id: format!("service:{region_key}.{svc}"),
                    status: summ.status.clone(),
                    latency_ms: summ.latency_ms,
                    detail: None,
                });
            }
        }
        for (name, d) in agg
            .llm_providers
            .iter()
            .chain(agg.internal_providers.iter())
        {
            evidence.push(crate::ceg::EvidenceRef {
                ref_id: format!("provider:{name}"),
                status: d.status.clone(),
                latency_ms: d.latency_ms,
                detail: d.source.clone(),
            });
        }

        let env = crate::ceg::LivenessEnvelope {
            attested_key_id: ctx.key_id.clone(),
            score: crate::ceg::liveness_score(&agg.status),
            confidence: 0.9,
            context: format!("ciris-status monitor — overall {}", agg.status),
            evidence,
            valid_until,
            asserted_at: now,
            epistemic_mode: crate::ceg::EpistemicMode::Derivative,
        };

        match crate::ceg::emit_liveness(&ctx.engine, &ctx.key_id, &env).await {
            Ok(hash) => tracing::info!(
                content_hash = %hash,
                overall = %agg.status,
                "Flow B: emitted signed health:liveness:v1"
            ),
            Err(e) => tracing::warn!(error = %e, "Flow B health:liveness emit failed"),
        }
    }
}

#[async_trait::async_trait]
impl Adapter for StatusAdapter {
    fn adapter_config(&self) -> AdapterConfig {
        AdapterConfig {
            adapter_type: "status".to_string(),
            enabled: true,
        }
    }

    fn status(&self) -> AdapterStatus {
        AdapterStatus {
            adapter_id: "status".to_string(),
            running: true,
        }
    }

    fn routers(&self, _ctx: &AdapterContext) -> Vec<axum::Router> {
        let router = Router::new()
            .route("/", get(root))
            .route("/health", get(health))
            .route("/v1/status", get(v1_status))
            .route("/api/status", get(api_status))
            .route("/api/v1/status", get(api_status))
            .route("/api/v1/status/history", get(history))
            .route("/api/v1/history", get(history))
            .route("/api/v1/scoring", get(scoring))
            .route("/api/v1/scoring/live", get(live_sse))
            .route("/api/v1/status/live", get(live_sse))
            .route("/api/v1/status/ws", get(live_ws))
            .layer(cors(&self.state.cfg))
            .with_state(self.state.clone());
        vec![router]
    }

    async fn start(&self, ctx: &AdapterContext) -> anyhow::Result<()> {
        tracing::info!("StatusAdapter starting — initial roster build from own corpus");
        self.refresh_roster(ctx).await;
        Ok(())
    }

    async fn run_lifecycle(
        &self,
        ctx: &AdapterContext,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        // The uptime-history poller and the live/Flow-A/Flow-B refresh are folded
        // into one interval loop here so they share the node's runtime + shutdown.
        let mut tick =
            tokio::time::interval(Duration::from_secs(self.state.cfg.poll_seconds.max(1)));
        tracing::info!(
            poll_s = self.state.cfg.poll_seconds,
            "StatusAdapter lifecycle running (probe → emit_liveness → roster refresh → history)"
        );
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    // ── Probe everything once; record the uptime-history rows. ──
                    history::poll_once(&self.state.cfg, &self.state.client, &self.state.db).await;

                    // ── Flow B: probe-derived signed health:liveness emit. ──
                    let agg = aggregate::aggregated_status(&self.state.cfg, &self.state.client).await;
                    self.emit_liveness(ctx, &agg).await;

                    // ── Flow A: rebuild the public roster from the OWN corpus. ──
                    self.refresh_roster(ctx).await;

                    // ── Live push: roster + overall-health delta to open sockets. ──
                    if self.state.live_tx.receiver_count() > 0 {
                        let _ = self.state.live_tx.send(LiveDelta {
                            timestamp: now_z(),
                            roster: Some(self.state.roster.snapshot()),
                            overall: Some(agg.status),
                        });
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        tracing::info!("StatusAdapter lifecycle shutting down");
                        return Ok(());
                    }
                }
            }
        }
    }

    async fn stop(&self) -> anyhow::Result<()> {
        tracing::info!("StatusAdapter stopped");
        Ok(())
    }
}
