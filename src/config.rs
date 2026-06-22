//! StatusAdapter configuration — the probe targets, poll cadence, and CORS
//! origins for the status page. **Zero env** (Server 0.5 zero-env model): every
//! one of these is a signed `config:*` CEG object, owner-authored via
//! `POST /v1/config`, read at runtime through `ciris_server::graph_config`.
//!
//! The node's identity, listen address, peering, and data dir are NOT here —
//! they belong to `ciris_server::ServerConfig` (resolved from `--home`/`--key-id`
//! plus the node's own `config:*`). This module resolves ONLY the adapter's concerns,
//! and only from the corpus (`graph_config::get_*(&engine, &node_key_id, KEY)`),
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
//!
//! `<r>` ∈ {`us`,`eu`}; `<p>` ∈ {`exa`,`brave`,`serper`,`tavily`}. A region/
//! external provider is probed only when its `*_url` config key is set; an unset
//! key simply omits that component (exactly as the old `*_URL` env behaved).

use std::path::Path;
use std::sync::Arc;

use ciris_persist::prelude::Engine;
use ciris_server::graph_config;

/// One regional deployment (US / EU): its public billing + proxy `/v1/status`
/// surfaces, plus the host's infrastructure health URL.
#[derive(Clone)]
pub struct Region {
    pub key: &'static str, // "us" / "eu"
    pub name: String,      // "US (Chicago)"
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
    pub version: &'static str,
    pub grafana_url: Option<String>,
    pub database_url: Option<String>, // local "postgresql" provider (TCP liveness)
    pub ghcr_url: String,
    pub regions: Vec<Region>,
    pub external: Vec<ExternalProvider>,
    pub cors_origins: Vec<String>,
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
    pub async fn resolve(engine: &Arc<Engine>, node_key_id: &str, db_path: String) -> Self {
        let poll_seconds = graph_config::get_i64(engine, node_key_id, "status.poll_secs")
            .await
            .ok()
            .flatten()
            .filter(|v| *v > 0)
            .unwrap_or(60) as u64;

        let cors_origins = graph_config::get_str_list(engine, node_key_id, "status.cors_origins")
            .await
            .ok()
            .flatten()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(default_cors_origins);

        let mut regions = Vec::new();
        for (key, label, provider) in REGION_SPECS {
            let name = get_str(engine, node_key_id, &format!("status.region.{key}.name"))
                .await
                .unwrap_or_else(|| (*label).to_string());
            regions.push(Region {
                key,
                name,
                billing_url: get_str(
                    engine,
                    node_key_id,
                    &format!("status.region.{key}.billing_url"),
                )
                .await,
                proxy_url: get_str(
                    engine,
                    node_key_id,
                    &format!("status.region.{key}.proxy_url"),
                )
                .await,
                infra_url: get_str(
                    engine,
                    node_key_id,
                    &format!("status.region.{key}.infra_url"),
                )
                .await,
                infra_provider: provider,
            });
        }

        let mut external = Vec::new();
        for (key, display, header, expected) in EXTERNAL_SPECS {
            // A provider is probed only when its url is configured.
            if let Some(url) =
                get_str(engine, node_key_id, &format!("status.external.{key}.url")).await
            {
                let authenticated = graph_config::get_bool(
                    engine,
                    node_key_id,
                    &format!("status.external.{key}.auth"),
                )
                .await
                .ok()
                .flatten()
                .unwrap_or(false);
                external.push(ExternalProvider {
                    key,
                    display,
                    url,
                    api_key: get_str(
                        engine,
                        node_key_id,
                        &format!("status.external.{key}.api_key"),
                    )
                    .await,
                    header,
                    expected_text: *expected,
                    authenticated,
                });
            }
        }

        let ghcr_url = get_str(engine, node_key_id, "status.ghcr_url")
            .await
            .unwrap_or_else(|| "https://ghcr.io/v2/".into());

        Config {
            db_path,
            poll_seconds,
            version: env!("CARGO_PKG_VERSION"),
            grafana_url: get_str(engine, node_key_id, "status.grafana_url").await,
            database_url: get_str(engine, node_key_id, "status.database_url").await,
            ghcr_url,
            regions,
            external,
            cors_origins,
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
                billing_url: None,
                proxy_url: None,
                infra_url: None,
                infra_provider: provider,
            })
            .collect();
        Config {
            db_path,
            poll_seconds: 60,
            version: env!("CARGO_PKG_VERSION"),
            grafana_url: None,
            database_url: None,
            ghcr_url: "https://ghcr.io/v2/".into(),
            regions,
            external: Vec::new(),
            cors_origins: default_cors_origins(),
        }
    }
}

/// Read a `config:*` string key, treating an empty string as unset (so an owner
/// can clear a probe target by setting it to `""` as well as by omitting it).
async fn get_str(engine: &Arc<Engine>, node_key_id: &str, key: &str) -> Option<String> {
    graph_config::get_str(engine, node_key_id, key)
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
    use ciris_persist::federation::types::{algorithm, KeyRecord, SignedKeyRecord};
    use ciris_persist::federation::Error as FederationError;
    use ciris_persist::prelude::{Engine, LocalSigner, LocalSignerConfig};
    use ciris_persist::verify::canonical::ceg_produce_canonicalize;
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
        let envelope = serde_json::json!({ "key_id": key_id });
        let canonical = ceg_produce_canonicalize(&envelope).unwrap();
        let och = hex::encode(Sha256::digest(&canonical));
        let sig = engine.sign_hybrid(&canonical).await.unwrap();
        let now = chrono::Utc::now();
        let rec = KeyRecord {
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
            roles: Vec::new(),
            attestation_evidence: None,
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
            node,
            "status.poll_secs",
            ConfigValue::I64(15),
            node,
            ConfigScope::Local,
        )
        .await
        .expect("set poll_secs");
        set_config(
            &engine,
            node,
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
            node,
            "status.region.us.billing_url",
            ConfigValue::Str("https://billing.us.test/".into()),
            node,
            ConfigScope::Local,
        )
        .await
        .expect("set us billing_url");
        set_config(
            &engine,
            node,
            "status.external.exa.url",
            ConfigValue::Str("https://exa.test/health".into()),
            node,
            ConfigScope::Local,
        )
        .await
        .expect("set exa url");
        set_config(
            &engine,
            node,
            "status.external.exa.auth",
            ConfigValue::Bool(true),
            node,
            ConfigScope::Local,
        )
        .await
        .expect("set exa auth");

        let cfg = Config::resolve(&engine, node, "/data/status.db".into()).await;

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
        let cfg = Config::resolve(&engine, NODE, "/data/status.db".into()).await;
        assert_eq!(cfg.poll_seconds, 60);
        assert!(cfg.external.is_empty());
        assert!(cfg.regions.iter().all(|r| r.billing_url.is_none()));
        assert!(cfg.cors_origins.contains(&"https://ciris.ai".to_string()));
    }
}
