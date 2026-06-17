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

    use crate::ceg::emit::{emit_consent, emit_liveness};
    use crate::ceg::{ConsentEnvelope, EpistemicMode, EvidenceRef, LivenessEnvelope};

    /// The attestation prefixes Node B consents to replicate to Node A. B is an
    /// external witness: it authorizes A to pull only its `health:` family
    /// (Flow B's `health:liveness:v1`), never anything else.
    const CONSENT_PREFIXES: &[&str] = &["health:"];

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
        /// Round-2 federation peer (Node A, the lens node). When present, B
        /// registers A's key (admission), emits a directed consent grant at A,
        /// and starts A<->B `Attestation`-kind replication. Absent ⇒ B runs
        /// solo (self-registers + emits its own liveness, no replication).
        peer_a: Option<PeerA>,
    }

    /// Node A's federation identity + replication URL, sourced from
    /// `STATUS_PEER_A_*`. `key_id` is required for replication wiring.
    ///
    /// v8.8.0 (CIRISPersist#234 §5.6.8.15): the admission gate now REQUIRES A's
    /// own *self-signed* `SignedKeyRecord` (proof-of-possession over A's
    /// registration envelope), NOT raw pubkeys — B can no longer fabricate a row
    /// for A. So the config carries `key_record`: A's exported `SignedKeyRecord`
    /// as serde_json (the cross-repo peer contract; both nodes are on persist
    /// v8.8.0 so the serde shape matches). `inbound_url` enables the outbound
    /// replication leg.
    struct PeerA {
        key_id: String,
        /// A's exported self-signed `SignedKeyRecord` (from
        /// `STATUS_PEER_A_KEY_RECORD`, serde_json). Handed to
        /// `register_federation_key`, which verifies A's signature fail-secure.
        key_record: ciris_persist::federation::types::SignedKeyRecord,
        /// A's `/edge/inbound` URL — outbound replication target. If unset, B
        /// still admits + receives A's data but won't initiate rounds toward A.
        inbound_url: Option<String>,
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

        // Round-2 peer (Node A). v8.8.0 gate: the peer leg needs A's key_id
        // (replication wiring) AND A's self-signed SignedKeyRecord (admission via
        // register_federation_key — B can no longer fabricate a row for A). Both
        // required; without A's record we can't admit its replicated `capacity:*`,
        // so skip the peer entirely (B runs solo).
        let peer_a = match (
            env_opt("STATUS_PEER_A_KEY_ID"),
            env_opt("STATUS_PEER_A_KEY_RECORD"),
        ) {
            (Some(key_id), Some(record_json)) => {
                match serde_json::from_str::<ciris_persist::federation::types::SignedKeyRecord>(
                    &record_json,
                ) {
                    Ok(key_record) => Some(PeerA {
                        key_id,
                        key_record,
                        inbound_url: env_opt("STATUS_PEER_A_INBOUND_URL"),
                    }),
                    Err(e) => {
                        tracing::error!(
                            error = %e,
                            "STATUS_PEER_A_KEY_RECORD is not a valid persist v8.8.0 SignedKeyRecord \
                             (serde_json) — skipping peer registration, consent emit, and replication"
                        );
                        None
                    }
                }
            }
            _ => {
                tracing::info!(
                    "STATUS_PEER_A_KEY_ID / STATUS_PEER_A_KEY_RECORD unset — \
                     skipping peer registration, consent emit, and A<->B replication"
                );
                None
            }
        };

        Some(FabricConfig {
            dsn,
            signer: LocalSignerConfig {
                key_id,
                key_path: PathBuf::from(key_path),
                pqc_key_id: Some(pqc_key_id),
                pqc_key_path: Some(PathBuf::from(pqc_key_path)),
            },
            service_keys,
            peer_a,
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

        let mut tick =
            tokio::time::interval(std::time::Duration::from_secs(state.cfg.poll_seconds));
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

        // ── Bootstrap admission (MUST precede any emit) ───────────────────────
        // (1) Self-register B's OWN witness key — `put_attestation` rejects rows
        //     whose attesting key isn't in `federation_keys`, so without this
        //     B's Flow-B `health:liveness` emit fails the key-existence gate.
        if let Err(e) = register_self_key(&engine, &cfg).await {
            tracing::error!(error = %e, "fabric: self-registration failed; flows disabled");
            return;
        }

        // (2) Peer A: register A's steward key (admits A's replicated capacity:*),
        //     (3) emit the directed consent grant, (4) start A<->B replication.
        let _replication = match &cfg.peer_a {
            Some(peer) => {
                if let Err(e) = register_peer_key(&engine, peer).await {
                    // Non-fatal: B still self-attests; it just can't admit A's data.
                    tracing::warn!(error = %e, peer = %peer.key_id, "fabric: peer-A key registration failed");
                }
                emit_replication_consent(&engine, backend.as_ref(), peer).await;
                start_replication(&engine, peer).await
            }
            None => None,
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
            for (name, d) in agg
                .llm_providers
                .iter()
                .chain(agg.internal_providers.iter())
            {
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
                Err(e) => {
                    tracing::warn!(error = %e, service = %service_key_id, "flow B emit failed")
                }
            }
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Round-2 federation: key registration, directed consent, replication.
    // ─────────────────────────────────────────────────────────────────────────

    /// Self-register Node B's OWN federation signing key via the v8.8.0 single
    /// canonical admission gate `Engine::register_federation_key` (CIRISPersist#234
    /// §5.6.8.15), which is fail-secure: it hybrid-verifies B's self-signed
    /// proof-of-possession BEFORE any store. REQUIRED before any B-authored
    /// attestation can be admitted: `put_attestation` enforces that the attesting
    /// key exists as a `federation_keys` row.
    ///
    /// B builds a self-signed `SignedKeyRecord` (identity_type **"witness"** — an
    /// out-of-group external monitor, least-privilege; does NOT unlock reserved
    /// prefixes B must not emit) with `scrub_key_id == key_id` (self-attested PoP),
    /// hybrid-signed (Ed25519 + ML-DSA-65) over `ceg_produce_canonicalize(envelope)`
    /// — the EXACT canonical form the gate re-derives — so the row passes the
    /// `original_content_hash` cross-check + the Strict hybrid verify. Idempotent:
    /// a `Conflict` (an identical row already holds this key_id) is benign. Also
    /// logs B's own record as JSON so an operator can hand it to peer A (the
    /// cross-repo peer-config artifact — A's `STATUS_PEER_*_KEY_RECORD` analogue).
    async fn register_self_key(engine: &Engine, cfg: &FabricConfig) -> anyhow::Result<()> {
        use anyhow::Context as _;
        use base64::engine::general_purpose::STANDARD as B64;
        use base64::Engine as _;
        use ciris_persist::federation::types::{algorithm, KeyRecord, SignedKeyRecord};
        use ciris_persist::federation::Error as FederationError;
        use ciris_persist::verify::canonical::ceg_produce_canonicalize;
        use sha2::{Digest, Sha256};

        let key_id = cfg.signer.key_id.clone();
        let envelope = serde_json::json!({ "key_id": key_id });
        // The gate canonicalizes via `ceg_produce_canonicalize` (produce epoch,
        // V2/JCS) and cross-checks `original_content_hash` against it — sign over
        // exactly those bytes so PoP verify + hash match.
        let canonical = ceg_produce_canonicalize(&envelope).map_err(|e| {
            anyhow::anyhow!("ceg_produce_canonicalize self-registration envelope: {e}")
        })?;
        let original_content_hash = hex::encode(Sha256::digest(&canonical));

        let sig = engine
            .sign_hybrid(&canonical)
            .await
            .context("hybrid-sign self-registration envelope")?;
        let now = chrono::Utc::now();
        let record = KeyRecord {
            key_id: key_id.clone(),
            pubkey_ed25519_base64: B64.encode(&sig.classical.public_key),
            pubkey_ml_dsa_65_base64: Some(B64.encode(&sig.pqc.public_key)),
            algorithm: algorithm::HYBRID.to_owned(),
            // Node B is an out-of-group external monitor → "witness" (least
            // privilege; does NOT unlock reserved prefixes B shouldn't emit).
            identity_type: "witness".to_owned(),
            identity_ref: key_id.clone(),
            valid_from: now,
            valid_until: None,
            registration_envelope: envelope,
            original_content_hash,
            scrub_signature_classical: B64.encode(&sig.classical.signature),
            scrub_signature_pqc: Some(B64.encode(&sig.pqc.signature)),
            // Self-attested proof-of-possession: B signs for itself.
            scrub_key_id: key_id.clone(),
            scrub_timestamp: now,
            pqc_completed_at: Some(now),
            persist_row_hash: String::new(),
            roles: Vec::new(),
            attestation_evidence: None,
        };
        let signed = SignedKeyRecord { record };

        // Export B's own record so an operator can hand it to peer A as A's
        // `STATUS_PEER_*` config (cross-repo contract: peer config = the peer's
        // SignedKeyRecord serde_json; both nodes are on persist v8.8.0).
        match serde_json::to_string(&signed) {
            Ok(json) => tracing::info!(
                key_id = %key_id,
                node_b_key_record = %json,
                "fabric: Node B's exported SignedKeyRecord (hand this to peer A as STATUS_PEER_*_KEY_RECORD)"
            ),
            Err(e) => {
                tracing::warn!(error = %e, "fabric: could not serialize Node B's key record for export")
            }
        }

        match engine.register_federation_key(signed).await {
            Ok(()) => {
                tracing::info!(
                    key_id = %key_id,
                    "fabric: registered Node B's own witness key via register_federation_key (hybrid PoP verified, fail-secure)"
                );
                Ok(())
            }
            Err(FederationError::Conflict(msg)) => {
                tracing::debug!(
                    key_id = %key_id,
                    conflict = %msg,
                    "fabric: self-registration is a benign conflict (key already present)"
                );
                Ok(())
            }
            Err(e) => Err(anyhow::anyhow!("self-register Node B federation key: {e}")),
        }
    }

    /// Register peer Node A's federation key via the v8.8.0 single canonical
    /// admission gate `Engine::register_federation_key` (CIRISPersist#234
    /// §5.6.8.15). This is the admission mechanism: A's replicated `capacity:*`
    /// rows are only admitted into B's corpus once A's key is in B's
    /// `federation_keys`.
    ///
    /// v8.8.0: the gate REQUIRES A's *self-signed* `SignedKeyRecord` (A's
    /// proof-of-possession over its own registration envelope) — B can no longer
    /// fabricate a plain directory row for A from raw pubkeys. B simply hands A's
    /// exported record (from `STATUS_PEER_A_KEY_RECORD`) to the gate, which
    /// hybrid-verifies A's signature fail-secure before storing (an unverifiable
    /// record is rejected and never stored). `Conflict` (A's identical record
    /// already present) is benign.
    async fn register_peer_key(engine: &Engine, peer: &PeerA) -> anyhow::Result<()> {
        use ciris_persist::federation::Error as FederationError;

        // A's record is its own self-signed proof-of-possession; B does NOT
        // re-sign or mutate it — the gate verifies A's signature.
        match engine
            .register_federation_key(peer.key_record.clone())
            .await
        {
            Ok(()) => {
                tracing::info!(peer = %peer.key_id, "fabric: registered peer A key via register_federation_key (A's PoP verified, fail-secure)");
                Ok(())
            }
            Err(FederationError::Conflict(msg)) => {
                tracing::debug!(peer = %peer.key_id, conflict = %msg, "fabric: peer-A key already present");
                Ok(())
            }
            Err(e) => Err(anyhow::anyhow!("register peer A federation key: {e}")),
        }
    }

    /// Emit the DIRECTED `consent:replication:v1` grant at peer A: "Node B
    /// consents to replicate its `health:` attestations to A." Federation-scope,
    /// subject = [A key_id] — the bilateral consent wire artifact. Best-effort:
    /// a failure is logged, not fatal (consent can be re-emitted next boot).
    async fn emit_replication_consent<D>(engine: &Engine, dir: &D, peer: &PeerA)
    where
        D: ciris_persist::federation::FederationDirectory,
    {
        let env = ConsentEnvelope {
            peer_key_id: peer.key_id.clone(),
            attestation_prefixes: CONSENT_PREFIXES.iter().map(|s| s.to_string()).collect(),
            asserted_at: chrono::Utc::now(),
        };
        match emit_consent(engine, dir, &env).await {
            Ok(hash) => tracing::info!(
                peer = %peer.key_id,
                content_hash = %hash,
                "fabric: emitted directed consent:replication:v1 at peer A"
            ),
            Err(e) => {
                tracing::warn!(error = %e, peer = %peer.key_id, "fabric: consent emit failed")
            }
        }
    }

    /// Start the A<->B anti-entropy replication runtime over the HTTP transport,
    /// peer = A, kind = `Attestation` (carries both directions — A's `capacity:*`
    /// inbound, B's `health:*` outbound). Spawns an inbound listen loop that
    /// routes received frames into the registry so A's `capacity:*` lands in B's
    /// own corpus. Returns the held runtime handle (None on transport failure).
    async fn start_replication(
        engine: &Engine,
        peer: &PeerA,
    ) -> Option<ciris_edge::replication::ReplicationRuntime> {
        use ciris_edge::replication::protocol::EnvelopeKind;
        use ciris_edge::replication::{
            ReplicationPeer, ReplicationRuntime, ReplicationRuntimeConfig,
        };
        use ciris_edge::transport::http::{HttpTransport, HttpTransportConfig};
        use ciris_edge::Transport;

        // B's own /edge/inbound listen address (where A pushes replication frames).
        let listen_addr = std::env::var("STATUS_REPLICATION_LISTEN")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "0.0.0.0:8201".to_string());
        let listen_addr = match listen_addr.parse() {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(error = %e, addr = %listen_addr, "fabric: bad STATUS_REPLICATION_LISTEN; replication disabled");
                return None;
            }
        };

        // Outbound: resolve A's key_id → A's /edge/inbound URL.
        let mut peer_urls = std::collections::HashMap::new();
        if let Some(url) = &peer.inbound_url {
            peer_urls.insert(peer.key_id.clone(), url.clone());
        } else {
            tracing::info!(
                "STATUS_PEER_A_INBOUND_URL unset — replication will RECEIVE A's frames \
                 but not initiate outbound rounds toward A"
            );
        }

        let transport: Arc<dyn Transport> = match HttpTransport::new(HttpTransportConfig {
            listen_addr,
            peer_urls,
            request_timeout: std::time::Duration::from_secs(30),
        }) {
            Ok(t) => Arc::new(t),
            Err(e) => {
                tracing::warn!(error = %e, "fabric: HTTP transport build failed; replication disabled");
                return None;
            }
        };

        // `EnvelopeKind::Attestation` carries BOTH directions (A's capacity:* in,
        // B's health:* out) — see edge replication/protocol.rs.
        let peers = vec![ReplicationPeer {
            peer_key_id: peer.key_id.clone(),
            kind: EnvelopeKind::Attestation,
        }];

        // `ReplicationRuntime::start` is async; bridge it onto the current
        // runtime via a blocking handle (we're inside a tokio task).
        let directory = engine.federation_directory();
        let transport_listen = Arc::clone(&transport);
        let peer_key_for_route = peer.key_id.clone();

        let runtime = ReplicationRuntime::start(
            directory,
            transport,
            peers,
            ReplicationRuntimeConfig::default(),
        )
        .await;

        // Inbound loop: A's frames → registry.route_inbound_bytes → B's corpus.
        let registry = runtime.registry();
        tokio::spawn(async move {
            let (tx, mut rx) = tokio::sync::mpsc::channel(64);
            let listen_task = tokio::spawn(async move {
                if let Err(e) = transport_listen.listen(tx).await {
                    tracing::warn!(error = %e, "fabric: replication transport listen exited");
                }
            });
            while let Some(frame) = rx.recv().await {
                // Plain HTTP transport doesn't attribute the source peer, so use
                // the single configured peer A as the source identity. (mTLS /
                // bearer HTTPS would populate `frame.source_key_id` directly.)
                let peer_key = frame
                    .source_key_id
                    .clone()
                    .unwrap_or_else(|| peer_key_for_route.clone());
                match registry
                    .route_inbound_bytes(&peer_key, &frame.envelope_bytes)
                    .await
                {
                    Ok(outcome) => {
                        tracing::debug!(?outcome, peer = %peer_key, "fabric: routed inbound replication frame")
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, peer = %peer_key, "fabric: route_inbound_bytes failed")
                    }
                }
            }
            listen_task.abort();
        });

        tracing::info!(peer = %peer.key_id, "fabric: A<->B replication runtime started (Attestation kind, HTTP transport)");
        Some(runtime)
    }

    fn redact(dsn: &str) -> String {
        match dsn.split_once('@') {
            Some((_, host)) => format!("***@{host}"),
            None => dsn.to_owned(),
        }
    }
}
