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
//! **Zero env** (Server 0.5 zero-env model): the federation identity, listen
//! address, data dir, and `consent:replication` peering are all
//! `ciris_server::ServerConfig`'s job (resolved from `--home`/`--key-id` + the
//! node's `config:*`). This adapter's OWN config — probe targets, poll cadence,
//! CORS — is `config:*` CEG read at runtime via `graph_config` (see
//! [`crate::config::Config::resolve`]); the uptime-history DB path is DERIVED
//! from `ctx.cfg.data_dir` (`<data_dir>/status.db`). [`StatusAdapter::new`]
//! takes no env and reads no corpus — it just primes the HTTP client + live
//! channel; everything else resolves from the [`AdapterContext`] at runtime.

use std::convert::Infallible;
use std::sync::{Arc, RwLock};
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

use ciris_server::ciris_persist::scope::CallerScope;
use ciris_server::{Adapter, AdapterConfig, AdapterContext, AdapterStatus};

use crate::config::Config;
use crate::model::{HistoryResponse, LiveDelta};
use crate::roster::RosterCache;
use crate::{aggregate, history};

/// The status page's shared state — what the old `main.rs` `AppState` held,
/// minus the node concerns (engine/identity/data dir). The engine is reached
/// through the [`AdapterContext`] the lifecycle/router builders receive.
///
/// `cfg` lives behind an `RwLock` so the lifecycle loop can refresh it from
/// `config:*` each poll cycle (owner-authored config changes are picked up live,
/// no restart). `db` is opened lazily in [`Adapter::start`] once the node
/// `data_dir` is known (the path is `<data_dir>/status.db`).
#[derive(Clone)]
struct AppState {
    cfg: Arc<RwLock<Config>>,
    client: reqwest::Client,
    db: Arc<RwLock<Option<history::Db>>>,
    /// Flow A public roster snapshot (served by `/api/v1/scoring`).
    roster: RosterCache,
    /// Substrate CI snapshot (served by `/api/v1/ci`), refreshed on its own
    /// slower cadence so five GitHub calls don't ride the health poll.
    ci: crate::ci::CiCache,
    /// The poll loop's latest aggregated snapshot — what `/api/v1/status`
    /// SERVES. Probing per request meant every viewer sampled the upstreams
    /// independently, so what a caller saw was never what got recorded: a blip
    /// the status page rendered existed only in that HTTP response. One
    /// sampler, one truth, and no probe amplification from page traffic.
    status: Arc<RwLock<Option<crate::model::AggregatedStatus>>>,
    /// `component -> status` from the previous cycle, for transition detection.
    prev_flat: Arc<RwLock<std::collections::BTreeMap<String, String>>>,
    /// Live-push fan-out for roster + health deltas (the "extra website sockets").
    live_tx: broadcast::Sender<LiveDelta>,
}

impl AppState {
    /// A snapshot clone of the current resolved config.
    fn cfg(&self) -> Config {
        self.cfg.read().expect("cfg lock").clone()
    }
}

fn now_z() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

// ── HTTP handlers ─────────────────────────────────────────────────────────────

async fn root(State(st): State<AppState>) -> impl IntoResponse {
    Json(json!({ "service": "ciris-status", "version": st.cfg().version }))
}

// No longer routed (the embedded ciris-server owns `/health` since v0.5.32 —
// CIRISStatus#7). Kept for reference / a future relocated status-health surface.
#[allow(dead_code)]
async fn health(State(st): State<AppState>) -> impl IntoResponse {
    Json(json!({ "status": "healthy", "timestamp": now_z(), "version": st.cfg().version }))
}

async fn v1_status(State(st): State<AppState>) -> impl IntoResponse {
    Json(aggregate::service_status(&st.cfg(), &st.client).await)
}

/// `GET /api/v1/status` — served from the poll loop's snapshot, so a caller sees
/// exactly what was recorded and attested. Falls back to a live probe only
/// before the first poll has landed (a freshly booted node still answers).
async fn api_status(State(st): State<AppState>) -> impl IntoResponse {
    let cached = st.status.read().expect("status lock").clone();
    match cached {
        Some(agg) => Json(agg),
        None => Json(aggregate::aggregated_status(&st.cfg(), &st.client).await),
    }
}

#[derive(Deserialize)]
struct EventParams {
    days: Option<i64>,
    limit: Option<i64>,
}

/// `GET /api/v1/status/events` — observed transitions, newest first. The record
/// a daily uptime rollup cannot hold: a 60s `degraded` blip moves a day's mean
/// by 0.07% and reads as noise, but it is a real event with a start and an end.
async fn events(State(st): State<AppState>, Query(q): Query<EventParams>) -> Response {
    let days = q.days.unwrap_or(7);
    if !(1..=365).contains(&days) {
        return bad("Days must be between 1 and 365");
    }
    let limit = q.limit.unwrap_or(200).clamp(1, 1000);
    let db = match st.db.read().expect("db lock").clone() {
        Some(db) => db,
        None => {
            return Json(crate::model::EventsResponse {
                days,
                events: Vec::new(),
            })
            .into_response()
        }
    };
    match history::query_events(&db, days, limit) {
        Ok(events) => Json(crate::model::EventsResponse { days, events }).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "events query failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "detail": "Failed to fetch events" })),
            )
                .into_response()
        }
    }
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
    let db = match st.db.read().expect("db lock").clone() {
        Some(db) => db,
        None => {
            // The history store opens in `start()` once data_dir is known; until
            // then serve an empty history rather than 500.
            return Json(HistoryResponse {
                days,
                region,
                history: Vec::new(),
            })
            .into_response();
        }
    };
    match history::query_history(&db, days, region.as_deref()) {
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

/// `GET /api/v1/ci` — the substrate's last-N build states per repo, served from
/// the cache so a microcontroller gets one small, instant response.
async fn ci(State(st): State<AppState>) -> impl IntoResponse {
    Json(st.ci.snapshot())
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
    /// Build the adapter with NO env and NO corpus read: just the HTTP client and
    /// the live-push channel, plus a baked-default config (no probes, baked CORS,
    /// 60s cadence). The probe targets, poll cadence, and CORS resolve from
    /// `config:*` at runtime from the [`AdapterContext`]; the uptime-history DB
    /// path is derived from `ctx.cfg.data_dir` and the store opens in
    /// [`Adapter::start`]. The node identity/listen/peering come from
    /// `ServerConfig`, not here.
    pub fn new() -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("ciris-status/", env!("CARGO_PKG_VERSION")))
            .build()?;

        let (live_tx, _live_rx) = broadcast::channel::<LiveDelta>(64);
        let state = AppState {
            // db_path is filled in at `start()` from data_dir; defaults until then.
            cfg: Arc::new(RwLock::new(Config::defaults(String::new()))),
            client,
            db: Arc::new(RwLock::new(None)),
            roster: RosterCache::default(),
            ci: crate::ci::CiCache::default(),
            status: Arc::new(RwLock::new(None)),
            prev_flat: Arc::new(RwLock::new(std::collections::BTreeMap::new())),
            live_tx,
        };
        Ok(StatusAdapter { state })
    }

    /// Re-resolve the adapter `config:*` from this node's OWN corpus and swap it
    /// in. Called at `start()` and each poll cycle so an owner-authored config
    /// change is picked up live. `db_path` (derived from `data_dir`) is preserved
    /// across the swap. Loudly flags any provider opted into BILLABLE keyed probing.
    async fn refresh_config(&self, ctx: &AdapterContext) {
        let db_path = self.state.cfg().db_path;
        let cfg = Config::resolve(&ctx.engine, db_path).await;

        for ext in cfg.external.iter().filter(|e| e.authenticated) {
            tracing::warn!(
                provider = ext.key,
                "authenticated health probing ENABLED — the live API key will be sent and may be \
                 BILLABLE per request; prefer passive monitoring (proxy /v1/status) for paid providers"
            );
        }

        *self.state.cfg.write().expect("cfg lock") = cfg;
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
    /// a `config:*` service-key map; the node always self-attests its own liveness
    /// here, and that key is already registered by `serve_with_adapter`.)
    async fn emit_liveness(&self, ctx: &AdapterContext, agg: &crate::model::AggregatedStatus) {
        let now = chrono::Utc::now();
        let valid_until = now + chrono::Duration::seconds(self.state.cfg().poll_seconds as i64);

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
                attestation_id = %hash,
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

    fn routers(&self, ctx: &AdapterContext) -> Vec<axum::Router> {
        // Derive the uptime-history DB path from the node data dir (convention,
        // not env, not config) and record it in the shared config so `start()`
        // can open the store and the lifecycle can preserve it across refreshes.
        let db_path = crate::config::db_path_for(&ctx.cfg.data_dir);
        // Resolve CORS (and the rest) from config:* once, synchronously, for the
        // router's CORS layer. `routers` runs on a runtime worker thread inside
        // `serve_with_adapter`, so block on the async resolve via block_in_place.
        let cfg = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(Config::resolve(&ctx.engine, db_path))
        });
        let cors_layer = cors(&cfg);
        *self.state.cfg.write().expect("cfg lock") = cfg;

        let router = Router::new()
            .route("/", get(root))
            // NB: NO `/health` here. Since ciris-server v0.5.32 the embedded node
            // owns base liveness at `/health` (+ `/v1/health`, `/v1/system/health`),
            // and the adapter seam merges our routes ON TOP — axum panics on the
            // duplicate `GET /health` (CIRISStatus#7, the v0.3.7 crash-loop). Base
            // liveness is the server's; our rich status lives at `/v1/status` +
            // `/api/v1/status`. (`health` handler kept below for reference/reuse.)
            .route("/v1/status", get(v1_status))
            .route("/api/status", get(api_status))
            .route("/api/v1/status", get(api_status))
            .route("/api/v1/status/history", get(history))
            .route("/api/v1/status/events", get(events))
            .route("/api/v1/history", get(history))
            .route("/api/v1/scoring", get(scoring))
            .route("/api/v1/ci", get(ci))
            .route("/api/v1/scoring/live", get(live_sse))
            .route("/api/v1/status/live", get(live_sse))
            .route("/api/v1/status/ws", get(live_ws))
            .layer(cors_layer)
            .with_state(self.state.clone());
        vec![router]
    }

    async fn start(&self, ctx: &AdapterContext) -> anyhow::Result<()> {
        // Derive + open the uptime-history store from the node data dir.
        let db_path = crate::config::db_path_for(&ctx.cfg.data_dir);
        match history::init(&db_path) {
            Ok(db) => *self.state.db.write().expect("db lock") = Some(db),
            Err(e) => {
                tracing::error!(error = %e, db = %db_path, "uptime-history store open failed")
            }
        }
        // Record the derived path + resolve the initial adapter config:* set.
        self.state.cfg.write().expect("cfg lock").db_path = db_path.clone();
        self.refresh_config(ctx).await;
        tracing::info!(
            db = %db_path,
            poll_s = self.state.cfg().poll_seconds,
            "StatusAdapter starting — initial config:* resolved, roster build from own corpus"
        );
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
            tokio::time::interval(Duration::from_secs(self.state.cfg().poll_seconds.max(1)));
        let mut last_poll = self.state.cfg().poll_seconds;
        // Far enough in the past that the first tick refreshes CI immediately.
        let mut last_ci = std::time::Instant::now() - Duration::from_secs(86_400);
        tracing::info!(
            poll_s = last_poll,
            "StatusAdapter lifecycle running (probe → emit_liveness → roster refresh → history)"
        );
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    // ── Re-resolve config:* so an owner-authored change is live. ──
                    self.refresh_config(ctx).await;
                    let cfg = self.state.cfg();
                    // If the cadence changed, rebuild the interval timer.
                    if cfg.poll_seconds != last_poll {
                        last_poll = cfg.poll_seconds;
                        tick = tokio::time::interval(Duration::from_secs(cfg.poll_seconds.max(1)));
                        tracing::info!(poll_s = last_poll, "StatusAdapter poll cadence retuned from config:*");
                    }

                    // ── Probe everything once; record the uptime-history rows. ──
                    // Clone the handle out and drop the guard before awaiting (the
                    // guard is !Send and would poison the future otherwise).
                    let db = self.state.db.read().expect("db lock").clone();
                    if let Some(db) = db {
                        history::poll_once(&cfg, &self.state.client, &db).await;
                    }

                    // ── Flow B: probe-derived signed health:liveness emit. ──
                    let agg = aggregate::aggregated_status(&cfg, &self.state.client).await;
                    self.emit_liveness(ctx, &agg).await;

                    // ── Transitions: diff this snapshot against the last, and
                    // record what changed. This is the only place a transient
                    // becomes durable — the daily rollup cannot hold it. ──
                    let flat = aggregate::flatten(&agg);
                    let events = {
                        let prev = self.state.prev_flat.read().expect("flat lock").clone();
                        // The first cycle after boot has no previous snapshot;
                        // treating it as a diff would log every component as a
                        // transition from unknown on every restart.
                        if prev.is_empty() {
                            Vec::new()
                        } else {
                            aggregate::transitions(&prev, &flat, &now_z())
                        }
                    };
                    if !events.is_empty() {
                        for e in &events {
                            tracing::info!(component = %e.component, from = %e.from, to = %e.to, "status transition");
                        }
                        let db = self.state.db.read().expect("db lock").clone();
                        if let Some(db) = db {
                            history::record_events(&db, &events);
                        }
                    }
                    *self.state.prev_flat.write().expect("flat lock") = flat;

                    // Serve what we just recorded.
                    *self.state.status.write().expect("status lock") = Some(agg.clone());

                    // ── Flow A: rebuild the public roster from the OWN corpus. ──
                    self.refresh_roster(ctx).await;

                    // ── Substrate CI, on its OWN (slower) cadence. Five GitHub
                    // calls must not ride the health poll: unauthenticated, the
                    // Actions API allows 60/hour. Conditional requests make a
                    // quiet stack nearly free, but the cadence is the backstop.
                    if !cfg.ci_repos.is_empty()
                        && last_ci.elapsed() >= Duration::from_secs(cfg.ci_poll_seconds.max(1))
                    {
                        last_ci = std::time::Instant::now();
                        self.state
                            .ci
                            .refresh(
                                &self.state.client,
                                &cfg.ci_owner,
                                &cfg.ci_repos,
                                cfg.ci_token.as_deref(),
                            )
                            .await;
                    }

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
