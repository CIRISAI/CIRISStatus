//! The status builders: `/v1/status` (local) and `/api/v1/status` (aggregated
//! multi-region). Outbound probes run concurrently, ONCE per poll cycle — the
//! adapter serves the resulting snapshot rather than re-probing per request, so
//! served, recorded, and attested are the same observation.
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

/// One provider entry parsed out of an upstream service's `/v1/status`.
pub struct Upstream {
    /// The **stable id**, never the display name: the map key (CIRISBilling's
    /// `{"postgresql": {...}}`) or the `provider` field (CIRISProxy's
    /// `[{"provider": "openrouter", "name": "OpenRouter", ...}]`).
    pub id: String,
    /// The upstream's own bucket hint — `llm` | `search` | `internal`. CIRISProxy
    /// emits it as `type`; CIRISBilling's map shape has none.
    pub kind: Option<String>,
    pub status: String,
    pub latency_ms: Option<i64>,
}

/// Last-resort id when an upstream gives us only a display name: `"Together AI"`
/// → `"together_ai"`, so a bucket key can never contain spaces or capitals.
fn slug(name: &str) -> String {
    name.trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Extract the providers an upstream service reports, tolerant of both wire
/// shapes we consume: CIRISBilling's `{id: {...}}` map and CIRISProxy's
/// `[{provider, name, type, ...}]` list.
///
/// The id MUST come from `provider` for the list shape — `name` is the human
/// label (`"Together AI"`, `"Brave Search"`), and keying on it silently dropped
/// every LLM/search provider at the categorization step downstream.
pub fn upstream_providers(body: &Value) -> Vec<Upstream> {
    let mut out = Vec::new();
    let pv = match body.get("providers") {
        Some(v) => v,
        None => return out,
    };
    let mut push = |id: String, v: &Value| {
        let status = v
            .get("status")
            .and_then(Value::as_str)
            .or_else(|| v.as_str())
            .unwrap_or(OPERATIONAL)
            .to_string();
        out.push(Upstream {
            id,
            kind: v
                .get("type")
                .and_then(Value::as_str)
                .map(|s| s.to_lowercase()),
            status,
            latency_ms: v.get("latency_ms").and_then(Value::as_i64),
        });
    };
    match pv {
        Value::Object(map) => {
            for (k, v) in map {
                push(k.clone(), v);
            }
        }
        Value::Array(arr) => {
            for item in arr {
                let id = item
                    .get("provider")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| item.get("name").and_then(Value::as_str).map(slug))
                    .or_else(|| item.as_str().map(slug))
                    .unwrap_or_else(|| "unknown".into());
                push(id, item);
            }
        }
        _ => {}
    }
    out
}

/// Insert `d` under `key`, keeping the **worst** status when more than one
/// region reports the same shared provider. A plain `insert` here made the
/// last-iterated region silently overwrite the others, so a US-side outage
/// disappeared behind an operational EU report.
fn merge_worst(map: &mut BTreeMap<String, ProviderDetail>, key: String, d: ProviderDetail) {
    match map.get(&key) {
        Some(existing) if severity(&existing.status) >= severity(&d.status) => {}
        _ => {
            map.insert(key, d);
        }
    }
}

fn detail(status: &str, latency: Option<i64>, source: String) -> ProviderDetail {
    ProviderDetail {
        status: status.to_string(),
        latency_ms: latency,
        source: Some(source),
    }
}

/// Is this provider **shared across regions** (one external service every region
/// probes) rather than a per-region dependency? Shared ones collapse to a single
/// bare-keyed row; regional ones keep a `<region>.` prefix, as `us.postgresql`
/// always has.
///
/// The upstream's own `type` decides when it declares one — the id list is only
/// the fallback for an upstream that doesn't. Neither path may DROP an entry:
/// silently discarding unrecognized providers is how every LLM and search
/// provider disappeared from this page after the Lens cutover.
fn is_shared_llm(p: &Upstream) -> bool {
    p.kind.as_deref() == Some("llm")
        || (p.kind.is_none()
            && matches!(p.id.as_str(), "openrouter" | "groq" | "together" | "openai"))
}

fn is_shared_search(p: &Upstream) -> bool {
    p.kind.as_deref() == Some("search")
        || (p.kind.is_none() && matches!(p.id.as_str(), "exa" | "brave"))
}

/// Fold one region's CIRISProxy `/v1/status` into the LLM + internal buckets.
pub(crate) fn fold_proxy(
    llm: &mut BTreeMap<String, ProviderDetail>,
    internal: &mut BTreeMap<String, ProviderDetail>,
    region_key: &str,
    body: &Value,
) {
    for p in upstream_providers(body) {
        let d = detail(&p.status, p.latency_ms, format!("cirisproxy.{region_key}"));
        if is_shared_llm(&p) {
            merge_worst(llm, p.id, d);
        } else if is_shared_search(&p) {
            merge_worst(internal, p.id, d);
        } else {
            // The proxy's own regional dependencies (e.g. `billing`).
            internal.insert(format!("{}.{}", region_key, p.id), d);
        }
    }
}

/// Fold one region's CIRISBilling `/v1/status` into the database + auth +
/// internal buckets.
pub(crate) fn fold_billing(
    database: &mut BTreeMap<String, ProviderDetail>,
    auth: &mut BTreeMap<String, ProviderDetail>,
    internal: &mut BTreeMap<String, ProviderDetail>,
    region_key: &str,
    body: &Value,
) {
    for p in upstream_providers(body) {
        let d = detail(
            &p.status,
            p.latency_ms,
            format!("cirisbilling.{region_key}"),
        );
        match p.id.as_str() {
            // A regional dependency → region-prefixed key.
            "postgresql" => {
                database.insert(format!("{region_key}.postgresql"), d);
            }
            // Shared external identity providers, checked from every region →
            // one row, worst vantage point wins.
            "google_oauth" | "google_play" => merge_worst(auth, p.id, d),
            // Anything billing grows later: surfaced as a regional internal dep
            // instead of dropped on the floor.
            _ => {
                internal.insert(format!("{}.{}", region_key, p.id), d);
            }
        }
    }
}

/// Flatten a snapshot into `component -> status`, the form transition detection
/// works on. Component ids are stable and self-describing (`eu.proxy`,
/// `llm.together`, `us.postgresql`) because they end up in an event log a human
/// reads months later.
pub fn flatten(agg: &AggregatedStatus) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    out.insert("overall".to_string(), agg.status.clone());
    for (region, r) in &agg.regions {
        for (svc, s) in &r.services {
            out.insert(format!("{region}.{svc}"), s.status.clone());
        }
    }
    for (name, i) in &agg.infrastructure {
        out.insert(format!("infra.{name}"), i.status.clone());
    }
    for (bucket, map) in [
        ("llm", &agg.llm_providers),
        ("auth", &agg.auth_providers),
        ("db", &agg.database_providers),
        ("internal", &agg.internal_providers),
    ] {
        for (name, d) in map {
            out.insert(format!("{bucket}.{name}"), d.status.clone());
        }
    }
    out
}

/// The transitions between two snapshots, as events stamped `ts`.
///
/// A component that appears or disappears is a transition too — from/to
/// `unknown` — because "we stopped being able to see it" is exactly the kind of
/// thing that otherwise vanishes silently.
pub fn transitions(
    prev: &BTreeMap<String, String>,
    now: &BTreeMap<String, String>,
    ts: &str,
) -> Vec<StatusEvent> {
    let mut events = Vec::new();
    for (component, to) in now {
        let from = prev.get(component).map(String::as_str).unwrap_or(UNKNOWN);
        if from != to {
            events.push(StatusEvent {
                ts: ts.to_string(),
                component: component.clone(),
                from: from.to_string(),
                to: to.clone(),
            });
        }
    }
    for (component, from) in prev {
        if !now.contains_key(component) {
            events.push(StatusEvent {
                ts: ts.to_string(),
                component: component.clone(),
                from: from.clone(),
                to: UNKNOWN.to_string(),
            });
        }
    }
    events
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
                fold_billing(&mut database, &mut auth, &mut internal, region.key, b);
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
                fold_proxy(&mut llm, &mut internal, region.key, b);
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// CIRISProxy's real wire shape (`hooks/status_handler.py`): a LIST whose
    /// stable id is `provider` and whose `name` is the human label. Keying on
    /// `name` yielded "OpenRouter"/"Together AI", which matched no categorization
    /// arm — every LLM + search provider silently vanished from the status page,
    /// the uptime history, and the signed liveness evidence.
    #[test]
    fn parses_proxy_list_shape_by_provider_id() {
        let body = json!({
            "service": "cirisproxy",
            "status": "degraded",
            "providers": [
                {"provider": "openrouter", "name": "OpenRouter", "type": "llm",
                 "status": "operational", "latency_ms": 210},
                {"provider": "together", "name": "Together AI", "type": "llm",
                 "status": "outage", "latency_ms": null, "error": "HTTP 503"},
                {"provider": "brave", "name": "Brave Search", "type": "search",
                 "status": "operational", "latency_ms": 340},
                {"provider": "billing", "name": "CIRISBilling", "type": "internal",
                 "status": "operational", "latency_ms": 12},
            ]
        });
        let got = upstream_providers(&body);
        let ids: Vec<_> = got.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["openrouter", "together", "brave", "billing"]);
        assert_eq!(got[0].kind.as_deref(), Some("llm"));
        assert_eq!(got[1].status, "outage");
        assert_eq!(got[2].kind.as_deref(), Some("search"));
        assert_eq!(got[0].latency_ms, Some(210));
        assert_eq!(got[1].latency_ms, None);
    }

    /// CIRISBilling's real wire shape (`app/api/status_routes.py`): a MAP already
    /// keyed by the stable id, with no `type` field.
    #[test]
    fn parses_billing_map_shape() {
        let body = json!({
            "service": "cirisbilling",
            "status": "operational",
            "providers": {
                "postgresql": {"status": "operational", "latency_ms": 29},
                "google_oauth": {"status": "degraded", "latency_ms": 1500},
            }
        });
        let got = upstream_providers(&body);
        assert_eq!(got.len(), 2);
        let pg = got.iter().find(|p| p.id == "postgresql").unwrap();
        assert_eq!(pg.status, "operational");
        assert!(pg.kind.is_none(), "billing declares no type");
    }

    /// An upstream that gives only a display name still gets a usable key.
    #[test]
    fn falls_back_to_slugged_display_name() {
        let body = json!({"providers": [{"name": "Together AI", "status": "operational"}]});
        assert_eq!(upstream_providers(&body)[0].id, "together_ai");
    }

    #[test]
    fn missing_or_scalar_providers_is_empty_not_a_panic() {
        assert!(upstream_providers(&json!({"status": "operational"})).is_empty());
        assert!(upstream_providers(&json!({"providers": "none"})).is_empty());
    }

    /// The REAL payload both deployed proxies returned on 2026-08-12, verbatim
    /// (`llm01.ciris-services-1.ai` / `llm01.ciris-services-eu-1.com`). Before the
    /// id fix this produced `llm_providers: {}` and `internal_providers: {}` —
    /// the page reported "degraded" in both regions and showed nothing that
    /// explained it, while Brave had been failing HTTP 422 the whole time.
    fn proxy_body(brave_latency: i64, together_latency: i64) -> Value {
        json!({
            "service": "cirisproxy",
            "status": "degraded",
            "providers": [
                {"provider": "openrouter", "name": "OpenRouter", "type": "llm",
                 "status": "operational", "latency_ms": 71, "error": null},
                {"provider": "groq", "name": "Groq", "type": "llm",
                 "status": "operational", "latency_ms": 124, "error": null},
                {"provider": "together", "name": "Together AI", "type": "llm",
                 "status": "operational", "latency_ms": together_latency, "error": null},
                {"provider": "billing", "name": "CIRISBilling", "type": "internal",
                 "status": "operational", "latency_ms": 30, "error": null},
                {"provider": "brave", "name": "Brave Search", "type": "search",
                 "status": "outage", "latency_ms": brave_latency, "error": "HTTP 422"},
            ]
        })
    }

    #[test]
    fn live_proxy_payload_surfaces_the_cause_of_degraded() {
        let mut llm = BTreeMap::new();
        let mut internal = BTreeMap::new();
        fold_proxy(&mut llm, &mut internal, "us", &proxy_body(177, 177));
        fold_proxy(&mut llm, &mut internal, "eu", &proxy_body(509, 624));

        // Every LLM provider is visible, bare-keyed, one row per provider.
        assert_eq!(
            llm.keys().collect::<Vec<_>>(),
            ["groq", "openrouter", "together"]
        );
        assert!(llm.values().all(|d| d.status == OPERATIONAL));

        // The reason the regions read "degraded" is now on the page.
        assert_eq!(internal["brave"].status, OUTAGE);

        // The proxy's regional dependency stays region-scoped.
        assert_eq!(internal["us.billing"].status, OPERATIONAL);
        assert_eq!(internal["eu.billing"].status, OPERATIONAL);

        // No display names leaked in as keys.
        assert!(!llm.contains_key("OpenRouter") && !internal.contains_key("Brave Search"));
    }

    #[test]
    fn live_billing_payload_keeps_both_regions_databases() {
        let body = |pg: &str| {
            json!({"service": "cirisbilling", "status": "operational", "providers": {
                "postgresql": {"status": pg, "latency_ms": 29},
                "google_oauth": {"status": "operational", "latency_ms": 150},
                "google_play": {"status": "operational", "latency_ms": 484},
            }})
        };
        let (mut db, mut auth, mut internal) = (BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
        fold_billing(&mut db, &mut auth, &mut internal, "us", &body(OUTAGE));
        fold_billing(&mut db, &mut auth, &mut internal, "eu", &body(OPERATIONAL));

        // Regional databases stay distinct — a US outage cannot hide behind EU.
        assert_eq!(db["us.postgresql"].status, OUTAGE);
        assert_eq!(db["eu.postgresql"].status, OPERATIONAL);
        // Shared identity providers collapse to one row each.
        assert_eq!(auth.len(), 2);
        assert!(auth.contains_key("google_oauth") && auth.contains_key("google_play"));
    }

    /// A shared provider probed from every region collapses to ONE row that keeps
    /// the worst vantage point. A plain insert let the last region iterated
    /// overwrite an outage seen by the first.
    #[test]
    fn merge_worst_keeps_the_unhealthy_region() {
        let mut m: BTreeMap<String, ProviderDetail> = BTreeMap::new();
        merge_worst(
            &mut m,
            "google_oauth".into(),
            detail(OUTAGE, None, "cirisbilling.us".into()),
        );
        merge_worst(
            &mut m,
            "google_oauth".into(),
            detail(OPERATIONAL, Some(150), "cirisbilling.eu".into()),
        );
        let e = &m["google_oauth"];
        assert_eq!(e.status, OUTAGE);
        assert_eq!(e.source.as_deref(), Some("cirisbilling.us"));
        assert_eq!(m.len(), 1, "one row per shared provider");
    }

    #[test]
    fn merge_worst_upgrades_severity_regardless_of_order() {
        let mut m: BTreeMap<String, ProviderDetail> = BTreeMap::new();
        merge_worst(
            &mut m,
            "groq".into(),
            detail(OPERATIONAL, Some(90), "cirisproxy.eu".into()),
        );
        merge_worst(
            &mut m,
            "groq".into(),
            detail(DEGRADED, Some(2100), "cirisproxy.us".into()),
        );
        assert_eq!(m["groq"].status, DEGRADED);
        assert_eq!(m["groq"].source.as_deref(), Some("cirisproxy.us"));
    }
}

#[cfg(test)]
mod transition_tests {
    use super::*;

    fn snap(status: &str, proxy_eu: &str, together: &str) -> AggregatedStatus {
        let mut regions = BTreeMap::new();
        let mut services = BTreeMap::new();
        services.insert(
            "proxy".to_string(),
            ServiceSummary {
                name: "LLM Proxy".into(),
                status: proxy_eu.to_string(),
                latency_ms: Some(10),
            },
        );
        regions.insert(
            "eu".to_string(),
            RegionStatus {
                name: "EU (Germany)".into(),
                status: proxy_eu.to_string(),
                services,
            },
        );
        let mut llm = BTreeMap::new();
        llm.insert(
            "together".to_string(),
            detail(together, Some(10), "cirisproxy.eu".into()),
        );
        AggregatedStatus {
            status: status.to_string(),
            timestamp: "2026-08-13T14:03:00Z".into(),
            last_incident: None,
            regions,
            infrastructure: BTreeMap::new(),
            llm_providers: llm,
            auth_providers: BTreeMap::new(),
            database_providers: BTreeMap::new(),
            internal_providers: BTreeMap::new(),
        }
    }

    /// The exact shape of the blip that prompted this: an LLM provider goes
    /// slow, so the provider row AND the proxy that reports it both degrade,
    /// then both recover. Two transitions out, two back — none of which a daily
    /// uptime rollup can express.
    #[test]
    fn records_a_transient_degradation_and_its_recovery() {
        let healthy = flatten(&snap(OPERATIONAL, OPERATIONAL, OPERATIONAL));
        let blip = flatten(&snap(DEGRADED, DEGRADED, DEGRADED));

        let out = transitions(&healthy, &blip, "2026-08-13T14:03:00Z");
        let mut got: Vec<_> = out
            .iter()
            .map(|e| (e.component.as_str(), e.to.as_str()))
            .collect();
        got.sort();
        assert_eq!(
            got,
            [
                ("eu.proxy", DEGRADED),
                ("llm.together", DEGRADED),
                ("overall", DEGRADED),
            ]
        );
        assert!(out.iter().all(|e| e.from == OPERATIONAL));

        let back = transitions(&blip, &healthy, "2026-08-13T14:04:00Z");
        assert_eq!(back.len(), 3);
        assert!(back
            .iter()
            .all(|e| e.from == DEGRADED && e.to == OPERATIONAL));
    }

    #[test]
    fn a_steady_state_produces_nothing() {
        let a = flatten(&snap(OPERATIONAL, OPERATIONAL, OPERATIONAL));
        assert!(transitions(&a, &a.clone(), "t").is_empty());
    }

    /// A component that stops being reported is itself a transition. Losing
    /// sight of something must not look identical to it being fine.
    #[test]
    fn appearing_and_disappearing_components_are_transitions() {
        let with = flatten(&snap(OPERATIONAL, OPERATIONAL, OPERATIONAL));
        let mut without = with.clone();
        without.remove("llm.together");

        let lost = transitions(&with, &without, "t");
        assert_eq!(lost.len(), 1);
        assert_eq!(lost[0].component, "llm.together");
        assert_eq!(lost[0].to, UNKNOWN);

        let found = transitions(&without, &with, "t");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].from, UNKNOWN);
        assert_eq!(found[0].to, OPERATIONAL);
    }

    /// Component ids end up in a log a human reads months later.
    #[test]
    fn component_ids_are_stable_and_self_describing() {
        let f = flatten(&snap(OPERATIONAL, DEGRADED, OPERATIONAL));
        assert_eq!(f.get("eu.proxy").map(String::as_str), Some(DEGRADED));
        assert_eq!(f.get("llm.together").map(String::as_str), Some(OPERATIONAL));
        assert!(f.contains_key("overall"));
    }
}
