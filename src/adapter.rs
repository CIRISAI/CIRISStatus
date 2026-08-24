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
//!     external services → (a) emit signed `observation:reachability:v1` into the node's
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
    /// `observed -> what we last SIGNED about it`. Flow B re-signed every
    /// target every cycle, which is how a week produced 30,781 rows that were
    /// expired within minutes and kept forever. A repeat of an unchanged
    /// verdict is not new evidence, so this ledger lets the loop tell news from
    /// an echo (`ceg::emit_due`).
    emitted: Arc<RwLock<std::collections::BTreeMap<String, crate::ceg::EmitRecord>>>,
    /// Rows purged since the signed wire index was last repaired. The repair is
    /// a full-table walk on the runtime, so it is paid off in one lump rather
    /// than owed a little at a time (see `retention::rebuild_wire_index`).
    purge_debt: Arc<RwLock<usize>>,
    /// Live-push fan-out for roster + health deltas (the "extra website sockets").
    live_tx: broadcast::Sender<LiveDelta>,
}

impl AppState {
    /// A snapshot clone of the current resolved config.
    fn cfg(&self) -> Config {
        self.cfg.read().expect("cfg lock").clone()
    }
}

/// Rows that must be purged before the signed wire index is rebuilt.
///
/// The rebuild is a full-table walk holding the connection mutex, so it is
/// amortised: 5,000 rows is roughly a day of steady-state churn at the current
/// emit rate, and the index being briefly out of date costs a lookup miss, not
/// a wrong answer.
const REBUILD_AFTER_PURGES: usize = 5_000;

/// How many missed poll cycles before a cached snapshot stops being served as
/// current. Three, to match what the status board uses before it blues out.
const STALE_AFTER_POLLS: i64 = 3;

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
    Json(aggregate::service_status(&st.cfg(), &crate::probe::Cycle::new(&st.client)).await)
}

/// `GET /api/v1/status` — served from the poll loop's snapshot, so a caller sees
/// exactly what was recorded and attested. Falls back to a live probe only
/// before the first poll has landed (a freshly booted node still answers).
///
/// A cached snapshot is only worth serving while it is fresh. If the lifecycle
/// stalls — or simply runs long, since the probes are sequential and their
/// timeouts can exceed the cadence — an old snapshot would otherwise be served
/// as though it were current, indefinitely and with every appearance of health.
/// Past `STALE_AFTER_POLLS` cycles the response is marked `stale` and its
/// overall status becomes `unknown`: we do not know, and saying so is the whole
/// point of the endpoint.
async fn api_status(State(st): State<AppState>) -> impl IntoResponse {
    let cached = st.status.read().expect("status lock").clone();
    let cfg = st.cfg();
    match cached {
        Some(mut agg) => {
            let max_age = (cfg.poll_seconds as i64) * STALE_AFTER_POLLS;
            agg.age_seconds = crate::model::age_seconds(&agg.timestamp, chrono::Utc::now());
            if agg.age_seconds > max_age {
                tracing::warn!(
                    age_s = agg.age_seconds,
                    max_age_s = max_age,
                    "serving a STALE snapshot — the poll loop has not produced one in time"
                );
                agg.stale = true;
                agg.status = crate::model::UNKNOWN.to_string();
                // The indicator describes `status`; leaving the old one made the
                // response say "unknown" and "critical" at once, so a client
                // reading the Statuspage field kept showing a verdict we had
                // just withdrawn.
                agg.indicator = crate::model::indicator_for(&agg.status);
            }
            Json(agg)
        }
        // NOTHING is probed on the request path — not even once, not even as a
        // fallback.
        //
        // This branch used to run a full live sweep when no snapshot existed
        // yet, which is how a starved node turned every caller into a probe
        // amplifier: ~17 sequential requests, 8-12s per response, and the
        // board's 15s fetch timing out while the process worked. Worse, it
        // fired exactly when the node could least afford it — the snapshot is
        // missing precisely when the poll loop is failing.
        //
        // An honest `unknown` in a millisecond beats a truthful answer that
        // arrives after the caller has given up. `stale: true` says the age is
        // not meaningful; a consumer that wants known-good data knows to keep
        // its last reading.
        None => Json(crate::model::AggregatedStatus::unknown(cfg.version)),
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
    match history::query_history(&db, days, region.as_deref(), &st.cfg().capabilities) {
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

/// `GET /api/v1/status/vantage` — where independent vantages disagreed about
/// the same component. Agreement implicates the component; disagreement
/// implicates the path between a vantage and it.
async fn vantage(State(st): State<AppState>, Query(q): Query<EventParams>) -> Response {
    let days = q.days.unwrap_or(7);
    if !(1..=365).contains(&days) {
        return bad("Days must be between 1 and 365");
    }
    let db = match st.db.read().expect("db lock").clone() {
        Some(db) => db,
        None => {
            return Json(crate::model::VantageResponse {
                days,
                rows: Vec::new(),
            })
            .into_response()
        }
    };
    match history::query_vantage(&db, days) {
        Ok(rows) => Json(crate::model::VantageResponse { days, rows }).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "vantage query failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "detail": "Failed to fetch vantage data" })),
            )
                .into_response()
        }
    }
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
            emitted: Arc::new(RwLock::new(std::collections::BTreeMap::new())),
            purge_debt: Arc::new(RwLock::new(0)),
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

    /// Flow B: sign + emit one `observation:reachability:v1` row **per observed
    /// target** into this node's own corpus.
    ///
    /// One row per target, not one folded row for the fabric: the folded shape
    /// named its targets only in a prose `context`, so the corpus could be asked
    /// what ciris-status said about *itself* and nothing else. Per-subject rows
    /// are what `resolve_scores` folds per attester, which is what a second
    /// vantage needs (`FSD/MULTI_VANTAGE.md` §4).
    ///
    /// The claim is first-person by construction — `attester == attested` is
    /// this node attesting its OWN observation — because "billing is alive" is
    /// not something a monitor knows. See §2 D5 for the decision and for what
    /// registering service keys would cost.
    async fn emit_observations(&self, ctx: &AdapterContext, agg: &crate::model::AggregatedStatus) {
        let cfg = self.state.cfg();
        let now = chrono::Utc::now();
        let heartbeat = chrono::Duration::seconds(cfg.observation_seconds as i64);
        // TWICE the heartbeat, not once. `expires_at` is what a consumer's
        // `valid_at` filter reads, so an expiry equal to the refresh interval
        // leaves a gap every time a cycle runs a second late — and on a loaded
        // box every cycle runs late. One missed beat should degrade freshness,
        // not blank the subject out of the fabric.
        let valid_until = now + heartbeat * 2;

        let envs = crate::ceg::observation_envelopes(&cfg, agg, now, valid_until);
        let total = envs.len();
        let (mut emitted, mut failed, mut skipped) = (0usize, 0usize, 0usize);

        for env in &envs {
            let prev = self
                .state
                .emitted
                .read()
                .expect("emit ledger lock")
                .get(&env.observed)
                .copied();
            if !crate::ceg::emit_due(prev, env.score, now, heartbeat) {
                skipped += 1;
                continue;
            }
            match crate::ceg::emit_observation(&ctx.engine, &ctx.key_id, env).await {
                Ok(_) => {
                    emitted += 1;
                    self.state
                        .emitted
                        .write()
                        .expect("emit ledger lock")
                        .insert(
                            env.observed.clone(),
                            crate::ceg::EmitRecord {
                                score: env.score,
                                at: now,
                            },
                        );
                }
                Err(e) => {
                    failed += 1;
                    // Per-row, with the target named: a batch that reports only
                    // a count cannot tell you WHICH subject stopped being
                    // attestable, which is the thing worth knowing. The ledger
                    // is NOT updated on failure, so the next cycle retries
                    // rather than waiting out a heartbeat on a row that was
                    // never written.
                    tracing::warn!(
                        observed = %env.observed,
                        error = %e,
                        "Flow B: observation emit failed"
                    );
                }
            }
        }
        if emitted > 0 || failed > 0 {
            tracing::info!(
                emitted,
                failed,
                skipped,
                total,
                overall = %agg.status,
                "Flow B: emitted signed observation:reachability:v1"
            );
        } else {
            // A cycle where nothing changed is the common case now, and logging
            // it at INFO every 60s is its own small tax on a box under pressure.
            tracing::debug!(skipped, total, "Flow B: nothing new to attest");
        }
    }

    /// Delete our own long-expired observation rows.
    ///
    /// Expiry is a read-side predicate, not a storage policy: `valid_at` hides
    /// these rows from every consumer, which is exactly why nobody noticed the
    /// corpus reaching 388MB with 95% of its rows dead. Bounded per pass, on its
    /// own slow cadence, because this shares two cores with the rest of the mesh.
    async fn prune_corpus(&self, ctx: &AdapterContext) {
        let hours = self.state.cfg().corpus_retention_hours as i64;
        let budget = self.state.cfg().corpus_retention_budget;
        let mut o = match crate::retention::prune_own_observations(&ctx.engine, hours, budget).await
        {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!(error = %e, "retention pass failed");
                return;
            }
        };

        // The index repair is owed EVENTUALLY, not per pass. Steady state
        // purges a handful of rows every couple of minutes and finishes the
        // backlog each time, so any "did we finish?" condition fires forever —
        // a 24k-row walk holding the connection mutex, for seventeen deleted
        // rows. Debt instead: pay once enough has changed to be worth one
        // stall. An index entry for a purged row is a miss, not a wrong answer,
        // so paying late costs a lookup and nothing else.
        // THE DEBT IS DURABLE (codex review, PR #63). Held in memory alone it
        // evaporates on restart, and `purge_attestation_v31` does not remove
        // the index entries for the rows it deletes — so a process that
        // restarts before paying forgets rows it already purged and their
        // stale entries are never repaired. On a node with an OOM restart loop
        // that is precisely the wrong direction: the more it restarts, the more
        // it forgets it owes.
        //
        // The in-memory cell stays as the fast path and is seeded from the
        // store on first use; the store is the record.
        let db = self.state.db.read().ok().and_then(|g| g.clone());
        let owed = {
            let mut d = self.state.purge_debt.write().expect("purge debt lock");
            if *d == 0 {
                if let Some(db) = db.as_ref() {
                    *d = crate::history::counter_get(db, crate::history::COUNTER_PURGE_DEBT)
                        as usize;
                }
            }
            *d += o.purged;
            *d
        };
        if o.purged > 0 {
            if let Some(db) = db.as_ref() {
                crate::history::counter_set(db, crate::history::COUNTER_PURGE_DEBT, owed as u64);
            }
        }
        // **BOTH BOUNDS, NOT EITHER** (codex review, PR #63). The debt alone
        // reintroduces the stall it was written to prevent: with a 30,000-row
        // backlog draining 2,000 a pass, the counter crosses 5,000 on pass
        // three and again every two and a half passes after it — roughly six
        // full-table rebuilds DURING the drain, each holding the connection
        // mutex and blocking the read API. That is the production failure,
        // reached by the fix for it.
        //
        // `!o.more` alone is the OTHER failure, the one this commit exists to
        // remove: in steady state every pass finishes, so it fires forever.
        //
        // Together they are exactly right, because they bound different things.
        // The debt says ENOUGH HAS CHANGED to be worth one stall; `!o.more`
        // says THIS IS A QUIET MOMENT to take it. A drain is never a quiet
        // moment, and a steady-state pass is never enough change on its own.
        if owed >= REBUILD_AFTER_PURGES && !o.more {
            match crate::retention::rebuild_wire_index(&ctx.engine).await {
                Ok(rows) => {
                    *self.state.purge_debt.write().expect("purge debt lock") = 0;
                    // Zero the DURABLE record in the same breath. Clearing only
                    // the in-memory cell would make a restart re-owe work that
                    // was actually done — the mirror of the bug above, and just
                    // as invisible.
                    if let Some(db) = db.as_ref() {
                        crate::history::counter_set(db, crate::history::COUNTER_PURGE_DEBT, 0);
                    }
                    o.rebuilt = true;
                    tracing::info!(
                        rows,
                        purged_since_last = owed,
                        "retention: signed wire index rebuilt"
                    );
                }
                Err(e) => tracing::warn!(error = %e, "retention: wire-index rebuild failed"),
            }
        }

        if o.did_anything() {
            tracing::info!(
                purged = o.purged,
                refused = o.refused,
                scanned = o.scanned,
                more = o.more,
                rebuilt = o.rebuilt,
                debt = owed,
                retention_hours = hours,
                "retention: pruned our own expired observation rows"
            );
        } else {
            tracing::debug!("retention: nothing past the window");
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
            .route("/api/v1/status/vantage", get(vantage))
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
        // The first retention pass runs on the first cycle (there is a backlog
        // to work through), then on `status.corpus_retention_secs`.
        let mut last_prune = std::time::Instant::now() - Duration::from_secs(86_400);
        // Roster likewise: built on the first cycle, then on its own cadence.
        let mut last_roster = std::time::Instant::now() - Duration::from_secs(86_400);
        tracing::info!(
            poll_s = last_poll,
            observation_s = self.state.cfg().observation_seconds,
            "StatusAdapter lifecycle running (probe → emit observations → roster refresh → history)"
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
                    // ONE sweep of the world per cycle, shared by the recorder
                    // and the server. Both used to probe every endpoint
                    // independently (CIRISStatus#47), which doubled the outbound
                    // work and — because they run in series — the wall clock,
                    // pushing snapshot age past the staleness ceiling.
                    let cycle = crate::probe::Cycle::new(&self.state.client);
                    // Fire every probe CONCURRENTLY first; both sweeps then read
                    // a warm memo. Serial probing is what took a lap from
                    // seconds to 5-7 minutes, which is why the served snapshot
                    // was almost always past its staleness ceiling and the page
                    // answered `unknown` while every probe was in fact fine.
                    cycle.prefetch(&cfg).await;
                    let db = self.state.db.read().expect("db lock").clone();
                    if let Some(db) = db {
                        history::poll_once(&cfg, &cycle, &db).await;
                    }

                    // ── Flow B: probe-derived signed observation emit. ──
                    let agg = aggregate::aggregated_status(&cfg, &cycle).await;
                    // Both sweeps have now run against one cache, so this is
                    // the real outbound cost of a cycle — and the number to
                    // watch if a future target starts probing twice again.
                    tracing::debug!(
                        requests = cycle.fetches(),
                        "probe sweep complete (recorder + server shared one pass)"
                    );
                    // A vantage failure means we could not SEE, so the snapshot
                    // is full of synthetic outages we have no evidence for.
                    // Marking its headline `unknown` was not enough: it was
                    // still flattened into outage transitions, signed as
                    // liveness evidence, installed as the baseline and served.
                    // The only honest handling is to record that WE went blind
                    // and otherwise change nothing.
                    if !agg.vantage_failure {
                        // Every cycle now — but `emit_due` signs only what
                        // CHANGED, plus a per-target heartbeat. A cadence gate
                        // here would have delayed news by up to the heartbeat,
                        // which is the opposite of what a status plane is for.
                        self.emit_observations(ctx, &agg).await;

                        // Retention on its own slow cadence: bounded work, and
                        // nothing about it is urgent.
                        if last_prune.elapsed()
                            >= Duration::from_secs(cfg.corpus_retention_secs.max(1))
                        {
                            last_prune = std::time::Instant::now();
                            self.prune_corpus(ctx).await;
                        }
                    } else {
                        tracing::warn!(
                            "vantage failure — not attesting, not installing this snapshot"
                        );
                    }

                    // ── Transitions: diff this snapshot against the last, and
                    // record what changed. This is the only place a transient
                    // becomes durable — the daily rollup cannot hold it. ──
                    // On a vantage failure the only thing we learned is that we
                    // went blind: keep every other component at its last known
                    // value rather than asserting outages we cannot support.
                    let prev_empty = self.state.prev_flat.read().expect("flat lock").is_empty();
                    // A blind FIRST poll establishes nothing. Seeding the
                    // baseline from it would leave a map containing only
                    // `monitor.network=outage`, so the first successful poll
                    // would emit a network recovery with no matching start plus
                    // an `unknown` transition for every ordinary component. The
                    // first poll that can actually SEE is the baseline.
                    let skip_cycle = agg.vantage_failure && prev_empty;
                    let flat = if agg.vantage_failure {
                        let mut f = self.state.prev_flat.read().expect("flat lock").clone();
                        f.insert("monitor.network".to_string(), "outage".to_string());
                        f
                    } else {
                        aggregate::flatten(&agg)
                    };
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
                    // The baseline advances ONLY once the transitions are
                    // durable. If the write fails and we move on, the component
                    // is already in its new state, so the next cycle sees no
                    // diff — the transition is lost for good. Holding the old
                    // baseline means the diff is simply retried next cycle.
                    let persisted = if events.is_empty() {
                        true
                    } else {
                        for e in &events {
                            tracing::info!(component = %e.component, from = %e.from, to = %e.to, "status transition");
                        }
                        match self.state.db.read().expect("db lock").clone() {
                            Some(db) => match history::record_events(&db, &events) {
                                Ok(n) => {
                                    tracing::debug!(rows = n, "transitions recorded");
                                    true
                                }
                                Err(e) => {
                                    tracing::error!(error = %e, count = events.len(),
                                        "transitions NOT recorded — holding the baseline so the diff retries");
                                    false
                                }
                            },
                            None => false,
                        }
                    };
                    if persisted && !skip_cycle {
                        *self.state.prev_flat.write().expect("flat lock") = flat;
                    }

                    // Serve what we just recorded — unless we could not see, in
                    // which case the previous snapshot stands and ages into
                    // `stale` on its own.
                    // Only when we could SEE. A vantage-failure snapshot
                    // carries an `unknown` headline over component maps still
                    // full of outages synthesised from our own failed probes,
                    // so publishing it would report the fabric down because we
                    // went blind — see `AggregatedStatus::safe_to_publish`.
                    //
                    // Nothing is lost by withholding it now that the request
                    // path never probes: the previous snapshot stands and ages
                    // into `stale`, and an empty cache answers instantly with
                    // `unknown`.
                    if agg.safe_to_publish() {
                        *self.state.status.write().expect("status lock") = Some(agg.clone());
                    }

                    // ── Flow A: rebuild the public roster from the OWN corpus. ──
                    // Flow A is a FULL SCAN of the corpus (dimension-prefix
                    // filtering walks every row, signatures included). On this
                    // node it returns nothing — no agents — so running it every
                    // cycle spent ~26s of disk to re-derive an empty roster.
                    if last_roster.elapsed() >= Duration::from_secs(cfg.roster_seconds.max(1)) {
                        last_roster = std::time::Instant::now();
                        self.refresh_roster(ctx).await;
                    }

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
