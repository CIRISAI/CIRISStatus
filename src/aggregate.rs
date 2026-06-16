//! The status builders: `/v1/status` (local) and `/api/v1/status` (aggregated
//! multi-region). All live outbound probes at request time, run concurrently.
//! Faithful to CIRISLens's overall-status arithmetic.

use std::collections::BTreeMap;

use chrono::Utc;
use reqwest::Client;
use serde_json::Value;

use crate::config::Config;
use crate::model::*;
use crate::probe::*;

fn now_z() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn provider_status(p: &Probe) -> ProviderStatus {
    ProviderStatus {
        status: p.status.to_string(),
        latency_ms: p.latency_ms,
        last_check: now_z(),
        message: p.message.clone(),
    }
}

/// `GET /v1/status` — the service's local view: the configured local providers
/// (postgresql + grafana, each probed only if configured).
pub async fn service_status(cfg: &Config, client: &Client) -> ServiceStatus {
    let mut providers: BTreeMap<String, ProviderStatus> = BTreeMap::new();

    if let Some(dsn) = &cfg.database_url {
        providers.insert(
            "postgresql".into(),
            provider_status(&check_postgres_tcp(dsn).await),
        );
    }
    if let Some(g) = &cfg.grafana_url {
        providers.insert(
            "grafana".into(),
            provider_status(&check_grafana(client, g).await),
        );
    }

    let overall = worst(providers.values().map(|p| p.status.as_str())).unwrap_or(OPERATIONAL);
    ServiceStatus {
        service: "cirislens".into(),
        status: overall.to_string(),
        timestamp: now_z(),
        version: cfg.version.to_string(),
        providers,
    }
}

/// Extract `[(name, status, latency_ms)]` from an upstream service's reported
/// `providers` (tolerant of both the `{name: {...}}` map and `[{...}]` list shapes).
pub fn upstream_providers(body: &Value) -> Vec<(String, String, Option<i64>)> {
    let mut out = Vec::new();
    let pv = match body.get("providers") {
        Some(v) => v,
        None => return out,
    };
    let mut push = |name: String, v: &Value| {
        let status = v
            .get("status")
            .and_then(Value::as_str)
            .or_else(|| v.as_str())
            .unwrap_or(OPERATIONAL)
            .to_string();
        let latency = v.get("latency_ms").and_then(Value::as_i64);
        out.push((name, status, latency));
    };
    match pv {
        Value::Object(map) => {
            for (k, v) in map {
                push(k.clone(), v);
            }
        }
        Value::Array(arr) => {
            for item in arr {
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .or_else(|| item.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                push(name, item);
            }
        }
        _ => {}
    }
    out
}

fn detail(status: &str, latency: Option<i64>, source: String) -> ProviderDetail {
    ProviderDetail {
        status: status.to_string(),
        latency_ms: latency,
        source: Some(source),
    }
}

/// `GET /api/v1/status` — the aggregated multi-region status page contract.
pub async fn aggregated_status(cfg: &Config, client: &Client) -> AggregatedStatus {
    let mut regions: BTreeMap<String, RegionStatus> = BTreeMap::new();
    let mut infrastructure: BTreeMap<String, InfrastructureStatus> = BTreeMap::new();
    let mut llm: BTreeMap<String, ProviderDetail> = BTreeMap::new();
    let mut auth: BTreeMap<String, ProviderDetail> = BTreeMap::new();
    let mut database: BTreeMap<String, ProviderDetail> = BTreeMap::new();
    let mut internal: BTreeMap<String, ProviderDetail> = BTreeMap::new();

    // ── Regions: billing + proxy live probes, plus upstream provider folding ──
    for region in &cfg.regions {
        let mut services: BTreeMap<String, ServiceSummary> = BTreeMap::new();

        if let Some(url) = &region.billing_url {
            let (probe, body) = fetch_service_status(client, url).await;
            services.insert(
                "billing".into(),
                ServiceSummary {
                    name: "Billing & Authentication".into(),
                    status: probe.status.to_string(),
                    latency_ms: probe.latency_ms,
                },
            );
            if let Some(b) = &body {
                for (name, status, latency) in upstream_providers(b) {
                    let src = format!("cirisbilling.{}", region.key);
                    match name.as_str() {
                        "postgresql" => {
                            database.insert(
                                format!("{}.postgresql", region.key),
                                detail(&status, latency, src),
                            );
                        }
                        "google_oauth" | "google_play" => {
                            auth.insert(name, detail(&status, latency, src));
                        }
                        _ => {}
                    }
                }
            }
        }

        if let Some(url) = &region.proxy_url {
            let (probe, body) = fetch_service_status(client, url).await;
            services.insert(
                "proxy".into(),
                ServiceSummary {
                    name: "LLM Proxy".into(),
                    status: probe.status.to_string(),
                    latency_ms: probe.latency_ms,
                },
            );
            if let Some(b) = &body {
                for (name, status, latency) in upstream_providers(b) {
                    let src = format!("cirisproxy.{}", region.key);
                    match name.as_str() {
                        "openrouter" | "groq" | "together" | "openai" => {
                            llm.insert(name, detail(&status, latency, src));
                        }
                        "exa" | "brave" => {
                            // Only if a direct external check didn't already cover it.
                            internal
                                .entry("web_search".into())
                                .or_insert_with(|| detail(&status, latency, src.clone()));
                        }
                        _ => {}
                    }
                }
            }
        }

        let region_status = worst(services.values().map(|s| s.status.as_str()))
            .unwrap_or(UNKNOWN)
            .to_string();
        regions.insert(
            region.key.to_string(),
            RegionStatus {
                name: region.name.clone(),
                status: region_status,
                services,
            },
        );

        // Infrastructure host health (Vultr/Hetzner).
        if let Some(url) = &region.infra_url {
            let p = check_infrastructure(client, url, 1000, false).await;
            infrastructure.insert(
                region.infra_provider.to_string(),
                InfrastructureStatus {
                    name: region.name.clone(),
                    status: p.status.to_string(),
                    provider: region.infra_provider.to_string(),
                    latency_ms: p.latency_ms,
                },
            );
        }
    }

    // ── Container registry (GHCR): higher threshold, 401 == up ──
    {
        let p = check_infrastructure(client, &cfg.ghcr_url, 3000, true).await;
        infrastructure.insert(
            "github".into(),
            InfrastructureStatus {
                name: "Container Registry".into(),
                status: p.status.to_string(),
                provider: "github".into(),
                latency_ms: p.latency_ms,
            },
        );
    }

    // ── Local providers (this service's own deps), if configured ──
    if let Some(dsn) = &cfg.database_url {
        let p = check_postgres_tcp(dsn).await;
        database.insert(
            "lens.postgresql".into(),
            detail(p.status, p.latency_ms, "cirislens".into()),
        );
    }
    if let Some(g) = &cfg.grafana_url {
        let p = check_grafana(client, g).await;
        internal.insert(
            "lens.grafana".into(),
            detail(p.status, p.latency_ms, "cirislens".into()),
        );
    }

    // ── Direct external providers (search APIs) — override upstream guesses ──
    for ext in &cfg.external {
        let p = check_external_provider(
            client,
            &ext.url,
            ext.header,
            ext.api_key.as_deref(),
            ext.expected_text,
            ext.authenticated,
        )
        .await;
        internal.insert(
            ext.display.to_string(),
            detail(p.status, p.latency_ms, format!("direct.{}", ext.key)),
        );
    }

    // ── Overall status arithmetic (regions + infrastructure) ──
    let mut considered: Vec<&str> = Vec::new();
    for r in regions.values() {
        if r.status != UNKNOWN {
            considered.push(&r.status);
        }
    }
    for i in infrastructure.values() {
        considered.push(&i.status);
    }
    let outages = considered.iter().filter(|s| **s == OUTAGE).count();
    let degraded = considered.contains(&DEGRADED);
    let overall = if outages >= 3 {
        "major_outage"
    } else if outages > 0 {
        "partial_outage"
    } else if degraded {
        DEGRADED
    } else {
        OPERATIONAL
    };

    AggregatedStatus {
        status: overall.to_string(),
        timestamp: now_z(),
        last_incident: None,
        regions,
        infrastructure,
        llm_providers: llm,
        auth_providers: auth,
        database_providers: database,
        internal_providers: internal,
    }
}
