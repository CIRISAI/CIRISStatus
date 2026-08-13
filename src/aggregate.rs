//! The status builders: `/v1/status` (local) and `/api/v1/status` (aggregated
//! multi-region). Outbound probes run concurrently, ONCE per poll cycle — the
//! adapter serves the resulting snapshot rather than re-probing per request, so
//! served, recorded, and attested are the same observation.
//! Faithful to CIRISLens's overall-status arithmetic.

use std::collections::BTreeMap;

use chrono::Utc;
use reqwest::Client;
use serde_json::Value;

use crate::capability;
use crate::config::Config;
use crate::model::*;
use crate::probe::*;

/// Statuses come off the wire owned; the summary field wants a `&'static str`.
///
/// The Statuspage spellings MUST normalise here, not fall through to
/// operational: `severity()` ranks `major_outage` as an outage, so the fold
/// picks it as the worst status and then this function turned it green — the
/// dependency failure was ranked correctly and reported as health.
fn leak_status(s: &str) -> &'static str {
    match s {
        DEGRADED | "degraded_performance" => DEGRADED,
        OUTAGE | "partial_outage" | "major_outage" => OUTAGE,
        UNKNOWN => UNKNOWN,
        _ => OPERATIONAL,
    }
}

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

/// A router's OWN health: its transport probe folded with the dependencies
/// nothing else can serve. Pooled members are excluded — the router is serving
/// fine on the others, and inheriting their unhappiness is what walked one slow
/// provider to the public headline (FSD §3.1 / D1).
pub fn service_status_excluding_pools(
    transport: &'static str,
    providers: &[Upstream],
    specs: &[capability::CapabilitySpec],
) -> &'static str {
    let mut status = transport;
    for p in providers {
        if capability::is_pooled(specs, p.kind.as_deref(), &p.id) {
            continue;
        }
        if severity(&p.status) > severity(status) {
            status = leak_status(&p.status);
        }
    }
    status
}

/// Fold one region's CIRISProxy `/v1/status` into the LLM + internal buckets.
pub(crate) fn fold_proxy(
    llm: &mut BTreeMap<String, ProviderDetail>,
    internal: &mut BTreeMap<String, ProviderDetail>,
    region_key: &str,
    body: &Value,
    specs: &[capability::CapabilitySpec],
    informational: &mut std::collections::BTreeSet<String>,
) {
    for p in upstream_providers(body) {
        // Recorded HERE because this is the only place the upstream's `kind` is
        // still in hand. A search provider excluded from its router by kind but
        // represented by no capability is invisible when it fails — the hole
        // this set exists to close.
        if matches!(
            capability::relation(specs, p.kind.as_deref(), &p.id),
            capability::Relation::Informational
        ) {
            informational.insert(p.id.clone());
        }
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
    // Capabilities are transition subjects in their own right: "the AI pool
    // went degraded" is the event a reader wants, not five provider rows they
    // have to correlate. And serving on a fallback gets its own component, so
    // it is recorded WITHOUT moving the headline (FSD §2.3) — it precedes cost,
    // latency and quality changes nothing else on the board would explain.
    // Always present, so a recovery is a transition rather than a component
    // silently reappearing.
    out.insert(
        "monitor.network".to_string(),
        if agg.vantage_failure {
            OUTAGE.to_string()
        } else {
            OPERATIONAL.to_string()
        },
    );
    for (id, cap) in &agg.capabilities {
        out.insert(format!("capability.{id}"), cap.status.clone());
        // Derived from the PRIMARY MEMBER, never from the capability: with a
        // threshold above one, losing a FALLBACK degrades the capability while
        // the primary is perfectly healthy, and inheriting that status emits a
        // primary failure that never happened — plus a false recovery later.
        // Only a POOL can serve on a fallback. Every region and infra entry is a
        // singleton whose sole member carries the primary role, so emitting a
        // `.primary` component for them recorded two transitions for one event.
        if let Some(primary) = cap
            .members
            .iter()
            .find(|m| m.role == crate::model::ROLE_PRIMARY)
            .filter(|_| cap.members.len() > 1)
        {
            out.insert(
                format!("capability.{id}.primary"),
                if capability::on_fallback(cap) {
                    "serving_on_fallback".to_string()
                } else {
                    primary.status.clone()
                },
            );
        }
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

/// `capability.ai_providers` / `capability.ai_providers.primary` -> the id.
fn capability_of(component: &str) -> Option<String> {
    component
        .strip_prefix("capability.")
        .map(|rest| rest.trim_end_matches(".primary").to_string())
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
                capability: capability_of(component),
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
                capability: capability_of(component),
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
    let specs = &cfg.capabilities;
    // Vantage accounting (FSD §3.3): transport failures are OUR network's
    // problem until proven otherwise; an HTTP status is the upstream's.
    let mut probes_attempted = 0usize;
    let mut transport_failures = 0usize;
    let mut count = |p: &Probe| {
        probes_attempted += 1;
        if p.transport_error {
            transport_failures += 1;
        }
    };
    // Providers the fold classified as routable-but-undeclared.
    let mut informational_ids: std::collections::BTreeSet<String> = Default::default();
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
            let (probe, body) = fetch_service_status(client, url, region.latency_baseline_ms).await;
            count(&probe);
            // Same fold as the proxy: billing's dependencies are things NOTHING
            // else serves, so an outage in one is an outage in billing. Taking
            // only the transport verdict here would leave the service, its
            // region and the headline green while authentication is unusable.
            let billing_status = match &body {
                Some(b) => {
                    service_status_excluding_pools(probe.status, &upstream_providers(b), specs)
                }
                None => probe.status,
            };
            services.insert(
                "billing".into(),
                ServiceSummary {
                    name: "Billing & Authentication".into(),
                    status: billing_status.to_string(),
                    latency_ms: probe.latency_ms,
                    upstream_status: probe.upstream_status.clone(),
                },
            );
            if let Some(b) = &body {
                fold_billing(&mut database, &mut auth, &mut internal, region.key, b);
            }
        }

        if let Some(url) = &region.proxy_url {
            let (probe, body) = fetch_service_status(client, url, region.latency_baseline_ms).await;
            count(&probe);
            // OUR verdict for the router: its own reachability folded with the
            // dependencies nothing else can serve. A slow member of a redundant
            // pool is excluded — the proxy is serving fine on the others, and
            // inheriting its self-report is what walked one slow provider all
            // the way to the public headline (FSD §3.1 / D1).
            let svc_status = match &body {
                Some(b) => {
                    service_status_excluding_pools(probe.status, &upstream_providers(b), specs)
                }
                None => probe.status,
            };
            services.insert(
                "proxy".into(),
                ServiceSummary {
                    name: "LLM Proxy".into(),
                    status: svc_status.to_string(),
                    latency_ms: probe.latency_ms,
                    upstream_status: probe.upstream_status.clone(),
                },
            );
            if let Some(b) = &body {
                fold_proxy(
                    &mut llm,
                    &mut internal,
                    region.key,
                    b,
                    specs,
                    &mut informational_ids,
                );
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
            let p =
                check_infrastructure(client, url, 1000, false, region.latency_baseline_ms).await;
            count(&p);
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
        let p = check_infrastructure(client, &cfg.ghcr_url, 3000, true, 0).await;
        count(&p);
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
        count(&p);
        database.insert(
            "lens.postgresql".into(),
            detail(p.status, p.latency_ms, "cirislens".into()),
        );
    }
    if let Some(g) = &cfg.grafana_url {
        let p = check_grafana(client, g).await;
        count(&p);
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
        count(&p);
        internal.insert(
            ext.display.to_string(),
            detail(p.status, p.latency_ms, format!("direct.{}", ext.key)),
        );
    }

    // ── Capabilities: the headline comes from what the fabric can DO ──
    let mut observed: BTreeMap<String, String> = BTreeMap::new();
    for (id, d) in &llm {
        observed.insert(id.clone(), d.status.clone());
    }
    let mut capabilities: BTreeMap<String, crate::model::CapabilityStatus> = BTreeMap::new();
    for spec in specs {
        capabilities.insert(spec.id.clone(), capability::roll_up(spec, &observed));
    }
    // Anything routable but undeclared is REPRESENTED, not merely excluded:
    // a visible capability that cannot move the headline. Without this, taking
    // a provider out of its router's verdict would make its failure invisible
    // everywhere — exclusion without representation.
    for (id, d) in llm.iter().chain(internal.iter()) {
        if !informational_ids.contains(id) {
            continue;
        }
        capabilities.insert(
            format!("provider.{id}"),
            capability::informational(id, &d.status),
        );
    }

    // Regions and infrastructure are singletons — no pooling across regions
    // (FSD §2.1: a regional outage is a regional outage).
    for (key, r) in &regions {
        capabilities.insert(
            format!("region.{key}"),
            capability::singleton(&format!("region.{key}"), &r.status),
        );
    }
    for (name, i) in &infrastructure {
        capabilities.insert(
            format!("infra.{name}"),
            capability::singleton(&format!("infra.{name}"), &i.status),
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
    let _ = &considered; // superseded by the capability rollup below
    let overall = capability::overall(&capabilities);

    // Every probe failed at the transport layer: that indicts our own network,
    // not the simultaneous failure of unrelated third parties on three
    // continents. Below MIN_FOR_VERDICT probes we cannot tell the difference
    // and do not claim to (FSD §3.3 / D3).
    let vantage_failure =
        probes_attempted >= crate::probe::MIN_FOR_VERDICT && transport_failures == probes_attempted;
    let overall = if vantage_failure { UNKNOWN } else { overall };

    AggregatedStatus {
        status: overall.to_string(),
        indicator: crate::model::indicator_for(overall),
        capabilities,
        vantage_failure,
        timestamp: now_z(),
        age_seconds: 0,
        stale: false,
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

    fn up(id: &str, kind: Option<&str>, status: &str) -> Upstream {
        Upstream {
            id: id.into(),
            kind: kind.map(str::to_string),
            status: status.into(),
            latency_ms: Some(10),
        }
    }

    /// D1, the defect that started this: a pooled provider degrades, and the
    /// router that merely REPORTS it inherits the unhappiness — which then
    /// walks through region status to the public headline. Four days of amber
    /// on ciris.ai for a provider that is not even in the default call path.
    #[test]
    fn a_degraded_pool_member_does_not_degrade_its_router() {
        let specs = crate::capability::default_specs();
        let providers = [
            up("groq", Some("llm"), DEGRADED),
            up("openrouter", Some("llm"), OPERATIONAL),
            up("billing", Some("internal"), OPERATIONAL),
        ];
        assert_eq!(
            service_status_excluding_pools(OPERATIONAL, &providers, &specs),
            OPERATIONAL,
            "the proxy is serving fine on the other members of the chain"
        );
    }

    /// A monitored-but-non-serving provider must NOT degrade its router: it is
    /// in nobody's call path, so its failure impairs nothing. Together is
    /// exactly this — four days of amber came from treating it otherwise.
    #[test]
    fn a_monitored_non_serving_provider_does_not_degrade_its_router() {
        let specs = crate::capability::default_specs();
        let providers = [up("together", Some("llm"), OUTAGE)];
        assert_eq!(
            service_status_excluding_pools(OPERATIONAL, &providers, &specs),
            OPERATIONAL,
            "routable, undeclared: informational, not indispensable"
        );
    }

    /// But an undeclared provider of a NON-routable kind has no alternative by
    /// definition, so its failure is its router's.
    #[test]
    fn an_undeclared_non_routable_dependency_still_degrades_its_router() {
        let specs = crate::capability::default_specs();
        let providers = [up("postgresql", None, OUTAGE)];
        assert_eq!(
            service_status_excluding_pools(OPERATIONAL, &providers, &specs),
            OUTAGE,
            "nothing else serves it"
        );
    }

    /// But a dependency nothing else can serve DOES degrade it.
    #[test]
    fn a_non_pooled_dependency_still_degrades_its_router() {
        let specs = crate::capability::default_specs();
        let providers = [
            up("together", Some("llm"), OUTAGE),
            up("billing", Some("internal"), OUTAGE),
        ];
        assert_eq!(
            service_status_excluding_pools(OPERATIONAL, &providers, &specs),
            OUTAGE,
            "nothing else serves billing"
        );
    }

    /// Transport trouble is the router's own, whatever its providers say.
    #[test]
    fn transport_health_is_never_masked_by_healthy_providers() {
        let specs = crate::capability::default_specs();
        let providers = [up("groq", Some("llm"), OPERATIONAL)];
        assert_eq!(
            service_status_excluding_pools(DEGRADED, &providers, &specs),
            DEGRADED
        );
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
        let specs = crate::capability::default_specs();
        let mut info = std::collections::BTreeSet::new();
        fold_proxy(
            &mut llm,
            &mut internal,
            "us",
            &proxy_body(177, 177),
            &specs,
            &mut info,
        );
        fold_proxy(
            &mut llm,
            &mut internal,
            "eu",
            &proxy_body(509, 624),
            &specs,
            &mut info,
        );
        // Together and Brave are routable but in no declared chain: recorded as
        // informational so their failures are represented somewhere, rather than
        // excluded from their router and then invisible.
        assert!(info.contains("together") && info.contains("brave"));
        assert!(!info.contains("groq"), "declared member, not informational");
        assert!(!info.contains("billing"), "nothing else serves it");

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
                upstream_status: None,
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
            indicator: "none",
            capabilities: BTreeMap::new(),
            vantage_failure: false,
            age_seconds: 0,
            stale: false,
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

    /// FSD §2.3: the primary goes down, fallbacks carry the traffic. The
    /// capability stays operational and the headline does not move — but the
    /// fact is recorded, because it precedes cost and latency changes that
    /// nothing else on the board would explain.
    #[test]
    fn serving_on_a_fallback_is_an_event_not_an_outage() {
        let spec = crate::capability::CapabilitySpec {
            id: "ai_providers".into(),
            label: "AI providers".into(),
            members: vec![("deepinfra".into(), true), ("groq".into(), false)],
            min_available: 1,
        };
        let observed = |primary: &str| -> BTreeMap<String, String> {
            [
                ("deepinfra".to_string(), primary.to_string()),
                ("groq".to_string(), OPERATIONAL.to_string()),
            ]
            .into_iter()
            .collect()
        };
        let healthy = crate::capability::roll_up(&spec, &observed(OPERATIONAL));
        let on_fallback = crate::capability::roll_up(&spec, &observed(OUTAGE));
        assert_eq!(on_fallback.status, OPERATIONAL, "fallbacks are serving");

        let mut a = snap(OPERATIONAL, OPERATIONAL, OPERATIONAL);
        a.capabilities.insert("ai_providers".into(), healthy);
        let mut b = snap(OPERATIONAL, OPERATIONAL, OPERATIONAL);
        b.capabilities.insert("ai_providers".into(), on_fallback);

        let events = transitions(&flatten(&a), &flatten(&b), "t");
        let subjects: Vec<_> = events.iter().map(|e| e.component.as_str()).collect();
        assert_eq!(
            subjects,
            ["capability.ai_providers.primary"],
            "the primary transition is recorded, and NOTHING else moves"
        );
        assert_eq!(events[0].to, "serving_on_fallback");
    }

    /// (4) With a threshold above one, losing a FALLBACK degrades the
    /// capability while the primary is perfectly healthy. Deriving the primary
    /// component from the capability emitted a primary failure that never
    /// happened — and a matching false recovery.
    #[test]
    fn a_fallback_failure_is_not_reported_as_a_primary_failure() {
        let spec = crate::capability::CapabilitySpec {
            id: "ai_providers".into(),
            label: "AI providers".into(),
            members: vec![
                ("deepinfra".into(), true),
                ("groq".into(), false),
                ("openrouter".into(), false),
            ],
            min_available: 2,
        };
        let obs = |groq: &str| -> BTreeMap<String, String> {
            [
                ("deepinfra".to_string(), OPERATIONAL.to_string()),
                ("groq".to_string(), groq.to_string()),
                ("openrouter".to_string(), OUTAGE.to_string()),
            ]
            .into_iter()
            .collect()
        };
        let before = crate::capability::roll_up(&spec, &obs(OPERATIONAL));
        let after = crate::capability::roll_up(&spec, &obs(OUTAGE));
        assert_eq!(before.status, OPERATIONAL);
        assert_eq!(after.status, DEGRADED, "one member left, threshold is two");

        let mut a = snap(OPERATIONAL, OPERATIONAL, OPERATIONAL);
        a.capabilities.insert("ai_providers".into(), before);
        let mut b = snap(OPERATIONAL, OPERATIONAL, OPERATIONAL);
        b.capabilities.insert("ai_providers".into(), after);

        let events = transitions(&flatten(&a), &flatten(&b), "t");
        let subjects: Vec<_> = events.iter().map(|e| e.component.as_str()).collect();
        assert_eq!(
            subjects,
            ["capability.ai_providers"],
            "the capability moved; the primary did not, and must not say it did"
        );
    }

    /// (10) A singleton cannot serve on a fallback — it has none. Emitting a
    /// `.primary` component for regions and infrastructure recorded two
    /// transitions for one event.
    #[test]
    fn singletons_do_not_get_a_primary_component() {
        let mut agg = snap(OPERATIONAL, OPERATIONAL, OPERATIONAL);
        agg.capabilities.insert(
            "region.us".into(),
            crate::capability::singleton("region.us", OPERATIONAL),
        );
        let flat = flatten(&agg);
        assert!(flat.contains_key("capability.region.us"));
        assert!(
            !flat.contains_key("capability.region.us.primary"),
            "no fallback exists, so there is no primary state to report"
        );
    }

    /// (3) Going blind is ONE fact. It must not be recorded as every component
    /// failing at once — that is the monitor reporting its own outage as the
    /// world's, which is the defect the detection exists to prevent.
    #[test]
    fn a_vantage_failure_reports_only_that_we_went_blind() {
        let healthy = flatten(&snap(OPERATIONAL, OPERATIONAL, OPERATIONAL));
        assert_eq!(
            healthy.get("monitor.network").map(String::as_str),
            Some(OPERATIONAL)
        );

        // What the lifecycle builds when it cannot see: last known values, plus
        // the one thing actually learned.
        let mut blind = healthy.clone();
        blind.insert("monitor.network".into(), OUTAGE.into());

        let events = transitions(&healthy, &blind, "t");
        let subjects: Vec<_> = events.iter().map(|e| e.component.as_str()).collect();
        assert_eq!(
            subjects,
            ["monitor.network"],
            "one event: we went blind. Not an outage per component."
        );

        // And recovery is a transition, not a component quietly reappearing.
        let back = transitions(&blind, &healthy, "t");
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].to, OPERATIONAL);
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
