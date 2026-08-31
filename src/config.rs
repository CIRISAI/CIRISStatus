//! StatusAdapter configuration — the probe targets, poll cadence, and CORS
//! origins for the status page. **Zero env** (Server 0.5 zero-env model): every
//! one of these is a signed `config:*` CEG object, owner-authored via
//! `POST /v1/config`, read at runtime through `ciris_server::graph_config`.
//!
//! The node's identity, listen address, peering, and data dir are NOT here —
//! they belong to `ciris_server::ServerConfig` (resolved from `--home`/`--key-id`
//! plus the node's own `config:*`). This module resolves ONLY the adapter's concerns,
//! and only from the corpus (`graph_config::get_*(&engine, KEY)`),
//! with baked defaults so a fresh, unconfigured node runs cleanly (no probes,
//! empty roster until replication).
//!
//! Config key reference (all under the `status.` namespace):
//!
//! | key                                | type | default              |
//! |------------------------------------|------|----------------------|
//! | `status.poll_secs`                 | i64  | `60`                 |
//! | `status.cors_origins`              | list | baked ciris.ai set   |
//! | `status.grafana_url`               | str  | — (skipped)          |
//! | `status.database_url`              | str  | — (skipped)          |
//! | `status.ghcr_url`                  | str  | `https://ghcr.io/v2/`|
//! | `status.region.<r>.name`           | str  | baked region label   |
//! | `status.region.<r>.billing_url`    | str  | — (skipped)          |
//! | `status.region.<r>.proxy_url`      | str  | — (skipped)          |
//! | `status.region.<r>.infra_url`      | str  | — (skipped)          |
//! | `status.external.<p>.url`          | str  | — (skipped)          |
//! | `status.external.<p>.api_key`      | str  | —                    |
//! | `status.external.<p>.auth`         | bool | `false` (keyless)    |
//! | `status.ci.owner`                  | str  | `CIRISAI`            |
//! | `status.ci.repos`                  | list | the substrate five   |
//! | `status.ci.token`                  | str  | — (unauthenticated)  |
//! | `status.ci.poll_secs`              | i64  | `300`                |
//!
//! `<r>` ∈ {`us`,`eu`}; `<p>` ∈ {`exa`,`brave`,`serper`,`tavily`}. A region/
//! external provider is probed only when its `*_url` config key is set; an unset
//! key simply omits that component (exactly as the old `*_URL` env behaved).

use std::path::Path;
use std::sync::Arc;

use ciris_server::ciris_persist::prelude::Engine;
use ciris_server::graph_config;

/// One regional deployment (US / EU): its public billing + proxy `/v1/status`
/// surfaces, plus the host's infrastructure health URL.
#[derive(Clone)]
pub struct Region {
    pub key: &'static str, // "us" / "eu"
    pub name: String,      // "US (Chicago)"
    /// Physics floor for probes to this region, subtracted before the latency
    /// threshold. A US→EU probe carries ~450-520ms of transatlantic RTT before
    /// anything is wrong; judging it against a US-local constant makes EU
    /// structurally closer to `degraded` for identical health (FSD §3.4).
    /// CONFIGURED, never learned — a learned baseline drifts upward during a
    /// slow degradation and quietly redefines normal.
    pub latency_baseline_ms: i64,
    pub billing_url: Option<String>,
    pub proxy_url: Option<String>,
    pub infra_url: Option<String>,
    pub infra_provider: &'static str, // "vultr" / "hetzner"
}

/// A directly-probed external provider (search APIs).
///
/// COST SAFETY: a health check that sends the live API key is a *billable* call
/// for some providers — Brave bills health checks (the old CIRISLens code had to
/// disable Brave for exactly this). So by default we probe **keyless**
/// (reachability only — billable APIs reject an unauthenticated request before
/// doing any billable work). The key is sent ONLY when `authenticated` is set
/// (`status.external.<p>.auth = true`), which an owner should enable *only* for a
/// provider whose health endpoint is free.
#[derive(Clone)]
pub struct ExternalProvider {
    pub key: &'static str,     // "exa" / "brave" / "serper" / "tavily"
    pub display: &'static str, // "web_search" / "brave_search" / ...
    pub url: String,
    pub api_key: Option<String>,
    pub header: &'static str, // "x-api-key" / "X-Subscription-Token" / ...
    pub expected_text: Option<&'static str>, // e.g. exa expects "healthy"
    pub authenticated: bool,  // send the key (billable!) — opt-in per provider
}

#[derive(Clone)]
pub struct Config {
    /// SQLite path for the uptime-history table the poller writes (the status
    /// page's own append-only history store; distinct from the node corpus).
    /// DERIVED from the node `data_dir` — convention, not config, not env.
    pub db_path: String,
    pub poll_seconds: u64,
    /// **Heartbeat** for the signed `observation:reachability:v1` plane — the
    /// longest a target goes without a fresh row when nothing about it changes.
    /// A CHANGED verdict is signed at probe speed regardless (`ceg::emit_due`);
    /// this only bounds the quiet case.
    ///
    /// Authoring is metered: persist charges `PeerWriteQuota` inside
    /// `put_attestation` on every backend, keyed on the row's author with no
    /// exemption for a node's own writes — 14,400 rows/day sustained. One row
    /// per observed target at a 60s probe cadence would spend the entire budget
    /// on ourselves and leave a peer nothing (the same bucket is charged again
    /// at any node that replicates us, since it is keyed on OUR key there too).
    ///
    /// The page and the SSE stream keep the 60s snapshot: human-facing freshness
    /// is a local concern. See `FSD/MULTI_VANTAGE.md` §2 D5.
    /// Raised 300 → 900 after the US-node measurement of 2026-08-22: at 300s
    /// with unconditional re-signing, one week produced 30,781 rows, 95% of them
    /// expired-on-arrival, in a corpus that reached 388MB on a box with no swap
    /// left.
    pub observation_seconds: u64,
    /// How long our OWN expired observation rows are kept before
    /// [`crate::retention`] deletes them. Expiry hides a row from readers; it
    /// does not reclaim anything, and on a four-node mesh sharing one small box
    /// that difference is the whole ballgame.
    pub corpus_retention_hours: u64,
    /// Rows the retention pass may delete per pass, and how often it runs.
    ///
    /// Sized against MEASURED I/O rather than a guess: the corpus scans this
    /// node performs read ~15MB/s off a disk already carrying 55MB/s between
    /// the two nodes, so a backlog left to drain at 400 rows every ten minutes
    /// takes days — during which every scan keeps paying for the rows the pass
    /// has not reached yet. Draining is the thing that makes the scans cheap,
    /// so it should not be the slowest part of the loop.
    pub corpus_retention_budget: usize,
    pub corpus_retention_secs: u64,
    /// How often Flow A rebuilds the public roster from the corpus.
    ///
    /// DECOUPLED from `poll_seconds` because it is a full scan: filtering by
    /// dimension prefix walks every attestation row, signatures included. On a
    /// monitor node with no agents that scan returns nothing, every cycle, at a
    /// cost proportional to how much OTHER data the corpus holds. Paying ~26s
    /// of disk to re-derive an empty roster once a minute is what turned a 60s
    /// lap into minutes.
    pub roster_seconds: u64,
    pub version: &'static str,
    pub grafana_url: Option<String>,
    pub database_url: Option<String>, // local "postgresql" provider (TCP liveness)
    pub ghcr_url: String,
    pub regions: Vec<Region>,
    pub external: Vec<ExternalProvider>,
    pub cors_origins: Vec<String>,
    /// GitHub org owning the substrate repos `/api/v1/ci` reports on.
    pub ci_owner: String,
    /// Repos rendered as centipedes, in dependency order. Empty ⇒ CI polling off.
    pub ci_repos: Vec<String>,
    /// Optional GitHub token. Unset is workable — every poll is conditional and
    /// a `304` costs no rate limit — but a token raises the ceiling from 60 to
    /// 5000 requests/hour, which matters the first time each ETag goes stale.
    pub ci_token: Option<String>,
    /// CI poll cadence. Slower than the health cadence by default: five repos
    /// per cycle against a 60/hour unauthenticated ceiling.
    pub ci_poll_seconds: u64,
    /// Declared capability pools (FSD §2). Declared rather than inferred, so a
    /// member nobody measures shows as `unknown` instead of being absent and
    /// therefore invisibly fine.
    pub capabilities: Vec<crate::capability::CapabilitySpec>,
    /// `(id, url)` identity providers probed directly. Empty ⇒ auth health comes
    /// only from CIRISBilling's report, as before.
    pub auth_targets: Vec<(String, String)>,
}

/// The baked CORS allow-list used when `status.cors_origins` is unset.
fn default_cors_origins() -> Vec<String> {
    vec![
        "https://ciris.ai".into(),
        "https://www.ciris.ai".into(),
        "https://agents.ciris.ai".into(),
        "http://localhost:3000".into(),
        "http://localhost:8080".into(),
    ]
}

/// The static region scaffold (key + infra_provider + baked label). The probe
/// URLs are filled in from `config:*`; an empty-URL region is simply not probed.
const REGION_SPECS: &[(&str, &str, &str)] = &[
    ("us", "US (Chicago)", "vultr"),
    ("eu", "EU (Germany)", "hetzner"),
];

/// Identity providers we probe DIRECTLY, keyless, so their health is our own
/// observation rather than a value lifted out of CIRISBilling's self-report.
///
/// Without this, `auth_providers` and the billing service status came from the
/// SAME measurement: billing probes Google, folds the result into its own
/// status, and we surfaced both. Two dots moving together looked like
/// corroboration and was one observation rendered twice — with no way to tell
/// "Google is down" from "billing cannot reach Google".
///
/// Both endpoints are unauthenticated and free; the URLs mirror what
/// CIRISBilling probes, so the two observations are comparable.
pub const AUTH_SPECS: &[(&str, &str)] = &[
    ("google_oauth", "https://oauth2.googleapis.com/tokeninfo"),
    (
        "google_play",
        "https://androidpublisher.googleapis.com/$discovery/rest?version=v3",
    ),
];

/// The external-provider scaffold: (key, display, header, expected_text). The
/// url/api_key/auth are filled in from `config:*`; no url ⇒ not probed.
const EXTERNAL_SPECS: &[(&str, &str, &str, Option<&str>)] = &[
    ("exa", "web_search", "x-api-key", Some("healthy")),
    ("brave", "brave_search", "X-Subscription-Token", None),
    ("serper", "serper_search", "X-API-KEY", None),
    ("tavily", "tavily_search", "x-api-key", None),
];

/// Derive the uptime-history DB path from the node data dir (`<data_dir>/status.db`).
/// Convention only — never env, never config.
pub fn db_path_for(data_dir: &Path) -> String {
    data_dir.join("status.db").to_string_lossy().into_owned()
}

impl Config {
    /// Resolve the adapter config from this node's OWN corpus (`config:*` CEG),
    /// with baked defaults for every unset key. `db_path` is derived by the
    /// caller from `ctx.cfg.data_dir` (convention) and threaded in here.
    ///
    /// Re-callable each poll cycle so an owner-authored config change is picked
    /// up live without a restart.
    pub async fn resolve(engine: &Arc<Engine>, db_path: String) -> Self {
        let poll_seconds = graph_config::get_i64(engine, "status.poll_secs")
            .await
            .ok()
            .flatten()
            .filter(|v| *v > 0)
            .unwrap_or(60) as u64;

        // Never faster than the probe cadence: emitting an observation more
        // often than we observe would re-sign the same measurement under a new
        // instant, which is what an equivocation check is built to catch.
        let observation_seconds = graph_config::get_i64(engine, "status.observation_secs")
            .await
            .ok()
            .flatten()
            .filter(|v| *v > 0)
            .map(|v| v as u64)
            .unwrap_or(900)
            .max(poll_seconds);

        let corpus_retention_hours = graph_config::get_i64(engine, "status.corpus_retention_hours")
            .await
            .ok()
            .flatten()
            .filter(|v| *v > 0)
            .unwrap_or(24) as u64;

        let corpus_retention_budget =
            graph_config::get_i64(engine, "status.corpus_retention_budget")
                .await
                .ok()
                .flatten()
                .filter(|v| *v > 0)
                .unwrap_or(crate::retention::PRUNE_BUDGET_PER_PASS as i64) as usize;

        let corpus_retention_secs = graph_config::get_i64(engine, "status.corpus_retention_secs")
            .await
            .ok()
            .flatten()
            .filter(|v| *v > 0)
            .unwrap_or(120) as u64;

        let roster_seconds = graph_config::get_i64(engine, "status.roster_secs")
            .await
            .ok()
            .flatten()
            .filter(|v| *v > 0)
            .unwrap_or(300)
            .max(poll_seconds as i64) as u64;

        let cors_origins = graph_config::get_str_list(engine, "status.cors_origins")
            .await
            .ok()
            .flatten()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(default_cors_origins);

        let mut regions = Vec::new();
        for (key, label, provider) in REGION_SPECS {
            let name = get_str(engine, &format!("status.region.{key}.name"))
                .await
                .unwrap_or_else(|| (*label).to_string());
            regions.push(Region {
                key,
                name,
                latency_baseline_ms: graph_config::get_i64(
                    engine,
                    &format!("status.region.{key}.latency_baseline_ms"),
                )
                .await
                .ok()
                .flatten()
                .filter(|v| *v >= 0)
                .unwrap_or(0),
                billing_url: get_str(engine, &format!("status.region.{key}.billing_url")).await,
                proxy_url: get_str(engine, &format!("status.region.{key}.proxy_url")).await,
                infra_url: get_str(engine, &format!("status.region.{key}.infra_url")).await,
                infra_provider: provider,
            });
        }

        let mut external = Vec::new();
        for (key, display, header, expected) in EXTERNAL_SPECS {
            // A provider is probed only when its url is configured.
            if let Some(url) = get_str(engine, &format!("status.external.{key}.url")).await {
                let authenticated =
                    graph_config::get_bool(engine, &format!("status.external.{key}.auth"))
                        .await
                        .ok()
                        .flatten()
                        .unwrap_or(false);
                external.push(ExternalProvider {
                    key,
                    display,
                    url,
                    api_key: get_str(engine, &format!("status.external.{key}.api_key")).await,
                    header,
                    expected_text: *expected,
                    authenticated,
                });
            }
        }

        let ghcr_url = get_str(engine, "status.ghcr_url")
            .await
            .unwrap_or_else(|| "https://ghcr.io/v2/".into());

        let ci_repos = graph_config::get_str_list(engine, "status.ci.repos")
            .await
            .ok()
            .flatten()
            .filter(|v: &Vec<String>| !v.is_empty())
            .unwrap_or_else(|| {
                crate::ci::DEFAULT_REPOS
                    .iter()
                    .map(|s| s.to_string())
                    .collect()
            });

        Config {
            db_path,
            poll_seconds,
            observation_seconds,
            corpus_retention_hours,
            corpus_retention_budget,
            corpus_retention_secs,
            roster_seconds,
            version: env!("CARGO_PKG_VERSION"),
            grafana_url: get_str(engine, "status.grafana_url").await,
            database_url: get_str(engine, "status.database_url").await,
            ghcr_url,
            regions,
            external,
            cors_origins,
            ci_owner: get_str(engine, "status.ci.owner")
                .await
                .unwrap_or_else(|| crate::ci::DEFAULT_OWNER.into()),
            ci_repos,
            ci_token: get_str(engine, "status.ci.token").await,
            capabilities: resolve_capabilities(engine).await,
            auth_targets: resolve_auth_targets(engine).await,
            ci_poll_seconds: graph_config::get_i64(engine, "status.ci.poll_secs")
                .await
                .ok()
                .flatten()
                .filter(|v| *v > 0)
                .unwrap_or(300) as u64,
        }
    }

    /// Baked defaults only — no corpus read. The shape a fresh, unconfigured node
    /// runs with (no probes, baked CORS, 60s cadence). Used at router-build time
    /// (before the engine is reachable) and as a test seam.
    pub fn defaults(db_path: String) -> Self {
        let regions = REGION_SPECS
            .iter()
            .map(|(key, label, provider)| Region {
                key,
                name: (*label).to_string(),
                latency_baseline_ms: 0,
                billing_url: None,
                proxy_url: None,
                infra_url: None,
                infra_provider: provider,
            })
            .collect();
        Config {
            db_path,
            poll_seconds: 60,
            observation_seconds: 900,
            corpus_retention_hours: 24,
            corpus_retention_budget: crate::retention::PRUNE_BUDGET_PER_PASS,
            corpus_retention_secs: 120,
            roster_seconds: 300,
            version: env!("CARGO_PKG_VERSION"),
            grafana_url: None,
            database_url: None,
            ghcr_url: "https://ghcr.io/v2/".into(),
            regions,
            external: Vec::new(),
            cors_origins: default_cors_origins(),
            ci_owner: crate::ci::DEFAULT_OWNER.into(),
            ci_repos: crate::ci::DEFAULT_REPOS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            ci_token: None,
            ci_poll_seconds: 300,
            capabilities: crate::capability::default_specs(),
            auth_targets: AUTH_SPECS
                .iter()
                .map(|(k, u)| (k.to_string(), u.to_string()))
                .collect(),
        }
    }
}

/// Resolve declared capability pools. `status.capability.<id>.members` is a
/// list in call-path order, `*` marking the primary (`deepinfra*,openrouter`);
/// `status.capability.<id>.min_available` is the threshold. Unset → the baked
/// default, so a fresh node still declares what it expects to measure.
async fn resolve_capabilities(engine: &Arc<Engine>) -> Vec<crate::capability::CapabilitySpec> {
    let mut out = Vec::new();
    for spec in crate::capability::default_specs() {
        let members =
            graph_config::get_str_list(engine, &format!("status.capability.{}.members", spec.id))
                .await
                .ok()
                .flatten()
                .filter(|v: &Vec<String>| !v.is_empty())
                .map(|v| {
                    v.iter()
                        .map(|m| {
                            let primary = m.ends_with('*');
                            (m.trim_end_matches('*').trim().to_string(), primary)
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or(spec.members);
        let min_available = graph_config::get_i64(
            engine,
            &format!("status.capability.{}.min_available", spec.id),
        )
        .await
        .ok()
        .flatten()
        .filter(|v| *v > 0)
        .map(|v| v as usize)
        .unwrap_or(spec.min_available);
        out.push(crate::capability::CapabilitySpec {
            id: spec.id,
            label: spec.label,
            members,
            min_available,
        });
    }
    out
}

/// Direct identity-provider probes. `status.auth.<id>.url` overrides the baked
/// endpoint; setting it to `""` disables that probe and falls back to billing's
/// report alone.
async fn resolve_auth_targets(engine: &Arc<Engine>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (id, default_url) in AUTH_SPECS {
        let url = graph_config::get_str(engine, &format!("status.auth.{id}.url"))
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| (*default_url).to_string());
        if !url.trim().is_empty() {
            out.push(((*id).to_string(), url));
        }
    }
    out
}

/// Read a `config:*` string key, treating an empty string as unset (so an owner
/// can clear a probe target by setting it to `""` as well as by omitting it).
async fn get_str(engine: &Arc<Engine>, key: &str) -> Option<String> {
    graph_config::get_str(engine, key)
        .await
        .ok()
        .flatten()
        .filter(|v| !v.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn defaults_are_zero_probe_and_baked_cors() {
        let cfg = Config::defaults("/tmp/x/status.db".into());
        assert_eq!(cfg.poll_seconds, 60);
        assert_eq!(cfg.ghcr_url, "https://ghcr.io/v2/");
        assert!(cfg.grafana_url.is_none());
        assert!(cfg.database_url.is_none());
        assert!(cfg.external.is_empty(), "no probes on a fresh node");
        // The region scaffold exists (keys/labels) but is not probed (no URLs).
        assert_eq!(cfg.regions.len(), 2);
        assert!(cfg
            .regions
            .iter()
            .all(|r| r.billing_url.is_none() && r.proxy_url.is_none() && r.infra_url.is_none()));
        assert!(cfg.cors_origins.contains(&"https://ciris.ai".to_string()));
    }

    #[test]
    fn db_path_is_derived_from_data_dir() {
        let p = db_path_for(&PathBuf::from("/var/lib/ciris/data"));
        assert_eq!(p, "/var/lib/ciris/data/status.db");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// config:* resolve proof — seeds signed config:v1 CEG objects via set_config and
// asserts `Config::resolve` reads them back (zero env; the corpus IS the config).
// Mirrors the node runtime: the node key must be self-registered (what
// serve_with_adapter does at boot) before set_config's put_attestation admits.
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod config_ceg {
    use super::*;
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    use ciris_server::ciris_persist::federation::admission::subject_binding;
    use ciris_server::ciris_persist::federation::types::{algorithm, KeyRecord, SignedKeyRecord};
    use ciris_server::ciris_persist::federation::Error as FederationError;
    use ciris_server::ciris_persist::prelude::{Engine, LocalSigner, LocalSignerConfig};
    use ciris_server::ciris_persist::verify::canonical::ceg_produce_canonicalize;
    use ciris_server::graph_config::{set_config, ConfigScope, ConfigValue};
    use sha2::{Digest, Sha256};

    struct SeedDir {
        dir: std::path::PathBuf,
    }
    impl SeedDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static CTR: AtomicU64 = AtomicU64::new(0);
            let n = CTR.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("ciris-status-cfg-seed-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            SeedDir { dir }
        }
        fn seed(&self, name: &str, b: [u8; 32]) -> std::path::PathBuf {
            let p = self.dir.join(name);
            std::fs::write(&p, b).unwrap();
            p
        }
    }
    impl Drop for SeedDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    async fn node(key_id: &str) -> (Arc<Engine>, SeedDir) {
        let seeds = SeedDir::new();
        let ed = seeds.seed("ed.seed", [0x11; 32]);
        let pqc = seeds.seed("pqc.seed", [0x22; 32]);
        let signer = Arc::new(
            LocalSigner::from_config(&LocalSignerConfig {
                key_id: key_id.into(),
                key_path: ed,
                pqc_key_id: Some(format!("{key_id}-pqc")),
                pqc_key_path: Some(pqc),
            })
            .expect("LocalSigner::from_config"),
        );
        let engine = Arc::new(
            Engine::with_signer(signer, "sqlite::memory:")
                .await
                .expect("Engine::with_signer"),
        );
        (engine, seeds)
    }

    async fn register_self_key(engine: &Engine, key_id: &str) {
        // The envelope must BIND the subject it registers — key_id,
        // identity_type and both pubkeys (CIRISPersist#659). A bare
        // `{"key_id": …}` stands for any record it is pasted onto, so persist
        // v31 refuses it outright rather than tolerating the legacy shape.
        //
        // Built from persist's own `subject_binding` rather than hand-rolled a
        // second time: this fixture existed because the binding was hand-rolled
        // once, and a fifth member would silently pass an envelope we wrote by
        // hand while failing the one the substrate expects.
        let sig = engine
            .sign_hybrid(b"probe")
            .await
            .expect("sign to obtain the pubkeys");
        let ed = B64.encode(&sig.classical.public_key);
        let pqc = B64.encode(&sig.pqc.public_key);
        let envelope = serde_json::Value::Object(subject_binding(
            key_id,
            "node",
            &ed,
            Some(pqc.as_str()),
            None,
        ));
        let canonical = ceg_produce_canonicalize(&envelope).unwrap();
        let och = hex::encode(Sha256::digest(&canonical));
        let sig = engine.sign_hybrid(&canonical).await.unwrap();
        let now = chrono::Utc::now();
        let rec = KeyRecord {
            // renamed from `roles` — empty is correct for a test subject/attester
            // key: capability_roles is the co-scrub plane's serve-node grant
            // ([infra:serve, infra:attest, …]), which a scoring fixture does not hold.
            capability_roles: Vec::new(),
            key_id: key_id.into(),
            pubkey_ed25519_base64: B64.encode(&sig.classical.public_key),
            pubkey_ml_dsa_65_base64: Some(B64.encode(&sig.pqc.public_key)),
            algorithm: algorithm::HYBRID.into(),
            identity_type: "node".into(),
            identity_ref: key_id.into(),
            valid_from: now,
            valid_until: None,
            registration_envelope: envelope,
            original_content_hash: och,
            scrub_signature_classical: B64.encode(&sig.classical.signature),
            scrub_signature_pqc: Some(B64.encode(&sig.pqc.signature)),
            scrub_key_id: key_id.into(),
            scrub_timestamp: now,
            pqc_completed_at: Some(now),
            persist_row_hash: String::new(),
            attestation_evidence: None,
            consent_role: None,
            additional_scrubs: Vec::new(),
        };
        match engine
            .register_federation_key(SignedKeyRecord { record: rec })
            .await
        {
            Ok(()) | Err(FederationError::Conflict(_)) => {}
            Err(e) => panic!("self-register node key: {e}"),
        }
    }

    #[tokio::test]
    async fn resolve_reads_seeded_config_objects() {
        const ALIAS: &str = "ciris-status";
        let (engine, _seeds) = node(ALIAS).await;
        // #247: set_config attests (via emit_attestation_self) under the node's
        // DERIVED key_id AND scopes the config object by it — register, author, and
        // resolve all key off that derived id (== prod cfg.key_id), not the bare alias.
        let node_kid = engine
            .local_derived_key_id()
            .await
            .expect("derive node key_id");
        let node = node_kid.as_str();
        register_self_key(&engine, node).await;

        // Seed an owner-authored config:* set.
        set_config(
            &engine,
            "status.poll_secs",
            ConfigValue::I64(15),
            node,
            ConfigScope::Local,
        )
        .await
        .expect("set poll_secs");
        set_config(
            &engine,
            "status.cors_origins",
            ConfigValue::List(vec![serde_json::Value::String(
                "https://example.test".into(),
            )]),
            node,
            ConfigScope::Local,
        )
        .await
        .expect("set cors_origins");
        set_config(
            &engine,
            "status.region.us.billing_url",
            ConfigValue::Str("https://billing.us.test/".into()),
            node,
            ConfigScope::Local,
        )
        .await
        .expect("set us billing_url");
        set_config(
            &engine,
            "status.external.exa.url",
            ConfigValue::Str("https://exa.test/health".into()),
            node,
            ConfigScope::Local,
        )
        .await
        .expect("set exa url");
        set_config(
            &engine,
            "status.external.exa.auth",
            ConfigValue::Bool(true),
            node,
            ConfigScope::Local,
        )
        .await
        .expect("set exa auth");

        let cfg = Config::resolve(&engine, "/data/status.db".into()).await;

        assert_eq!(cfg.poll_seconds, 15, "poll cadence from config:*");
        assert_eq!(cfg.cors_origins, vec!["https://example.test".to_string()]);
        let us = cfg.regions.iter().find(|r| r.key == "us").unwrap();
        assert_eq!(us.billing_url.as_deref(), Some("https://billing.us.test/"));
        // Only exa is configured with a url ⇒ exactly one external probe, authed.
        assert_eq!(cfg.external.len(), 1);
        assert_eq!(cfg.external[0].key, "exa");
        assert!(cfg.external[0].authenticated);
        // db_path is the caller-derived value, never from config:*.
        assert_eq!(cfg.db_path, "/data/status.db");
    }

    #[tokio::test]
    async fn resolve_falls_back_to_defaults_on_empty_corpus() {
        const NODE: &str = "ciris-status";
        let (engine, _seeds) = node(NODE).await;
        register_self_key(&engine, NODE).await;

        // No config:* authored → baked defaults (the fresh-node path).
        let cfg = Config::resolve(&engine, "/data/status.db".into()).await;
        assert_eq!(cfg.poll_seconds, 60);
        assert!(cfg.external.is_empty());
        assert!(cfg.regions.iter().all(|r| r.billing_url.is_none()));
        assert!(cfg.cors_origins.contains(&"https://ciris.ai".to_string()));
    }
}
