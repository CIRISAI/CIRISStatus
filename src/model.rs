//! Response types — the exact JSON contracts the ciris.ai status page consumes.
//! Field names, nesting, and the status-string enums match CIRISLens's API so the
//! frontend is unaffected by the swap.

use std::collections::BTreeMap;

use serde::Serialize;

// Component-level status strings.
pub const OPERATIONAL: &str = "operational";
pub const DEGRADED: &str = "degraded";
pub const OUTAGE: &str = "outage";
pub const UNKNOWN: &str = "unknown";

/// Severity rank of a component status — the ordering behind [`worst`] and the
/// worst-wins merge of a provider reported by more than one region.
/// Unknown strings rank as `operational` (Lens-faithful: an unrecognized status
/// never invents an outage).
pub fn severity(status: &str) -> i8 {
    match status {
        DEGRADED => 1,
        OUTAGE => 2,
        _ => 0,
    }
}

/// Worst (most-severe) of a set of component statuses; `None` → no components.
pub fn worst<'a>(statuses: impl IntoIterator<Item = &'a str>) -> Option<&'static str> {
    let mut rank = -1i8;
    for s in statuses {
        let r = severity(s);
        if r > rank {
            rank = r;
        }
    }
    match rank {
        0 => Some(OPERATIONAL),
        1 => Some(DEGRADED),
        2 => Some(OUTAGE),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_age_is_measured_and_never_negative() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-08-13T14:05:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(age_seconds("2026-08-13T14:03:00Z", now), 120);
        assert_eq!(age_seconds("2026-08-13T14:05:00Z", now), 0);
        // Clock skew must not read as impossibly old and flap the endpoint.
        assert_eq!(age_seconds("2026-08-13T14:06:00Z", now), 0);
        assert_eq!(age_seconds("not a timestamp", now), 0);
    }

    #[test]
    fn worst_picks_most_severe() {
        assert_eq!(
            worst(["operational", "degraded", "operational"]),
            Some(DEGRADED)
        );
        assert_eq!(worst(["operational", "outage"]), Some(OUTAGE));
        assert_eq!(worst(["operational"]), Some(OPERATIONAL));
        assert_eq!(worst([] as [&str; 0]), None);
    }
}

// ── /v1/status ───────────────────────────────────────────────────────────────
#[derive(Serialize, Clone)]
pub struct ProviderStatus {
    pub status: String,
    pub latency_ms: Option<i64>,
    pub last_check: String,
    pub message: Option<String>,
}

#[derive(Serialize)]
pub struct ServiceStatus {
    pub service: String,
    pub status: String,
    pub timestamp: String,
    pub version: String,
    pub providers: BTreeMap<String, ProviderStatus>,
}

// ── /api/v1/status ───────────────────────────────────────────────────────────
#[derive(Serialize, Clone)]
pub struct ServiceSummary {
    pub name: String,
    /// OUR verdict: the transport probe folded with the upstream's non-pooled
    /// dependencies. A slow member of a redundant pool does not appear here —
    /// the router is serving fine on the others (FSD §3.1).
    pub status: String,
    pub latency_ms: Option<i64>,
    /// What the service said about ITSELF, preserved rather than overwritten.
    /// It folds pooled providers into its own verdict; we do not, but we also
    /// do not get to silently discard its opinion.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_status: Option<String>,
}

#[derive(Serialize, Clone)]
pub struct RegionStatus {
    pub name: String,
    pub status: String,
    pub services: BTreeMap<String, ServiceSummary>,
}

#[derive(Serialize, Clone)]
pub struct InfrastructureStatus {
    pub name: String,
    pub status: String,
    pub provider: String,
    pub latency_ms: Option<i64>,
}

#[derive(Serialize, Clone)]
pub struct ProviderDetail {
    pub status: String,
    pub latency_ms: Option<i64>,
    pub source: Option<String>,
}

#[derive(Serialize, Clone)]
pub struct AggregatedStatus {
    pub status: String,
    /// Statuspage v2 severity for `status`.
    pub indicator: &'static str,
    /// Capability rollups (FSD §2.2) — what the headline is derived from.
    pub capabilities: BTreeMap<String, CapabilityStatus>,
    /// True when every probe this cycle failed at the transport layer, which
    /// indicts our own network rather than the world (FSD §3.3). `status` is
    /// then `unknown`: we cannot see, and saying so beats reporting a global
    /// outage we have no evidence for.
    pub vantage_failure: bool,
    pub timestamp: String,
    /// Age of this snapshot when served. A cached snapshot that says nothing
    /// about its own age can be served as current forever by a stalled loop.
    pub age_seconds: i64,
    /// True when the snapshot has outlived its poll window; `status` is then
    /// `unknown` rather than a stale-but-healthy-looking verdict.
    pub stale: bool,
    pub last_incident: Option<serde_json::Value>,
    pub regions: BTreeMap<String, RegionStatus>,
    pub infrastructure: BTreeMap<String, InfrastructureStatus>,
    pub llm_providers: BTreeMap<String, ProviderDetail>,
    pub auth_providers: BTreeMap<String, ProviderDetail>,
    pub database_providers: BTreeMap<String, ProviderDetail>,
    pub internal_providers: BTreeMap<String, ProviderDetail>,
}

// ── Capabilities (FSD/CAPABILITY_MONITORING.md §2) ───────────────────────────
/// One member of a capability, and the role it plays in the call path.
#[derive(Serialize, Clone, PartialEq, Debug)]
pub struct CapabilityMember {
    pub id: String,
    /// `primary` | `fallback`. Serving on a fallback is not an outage, but it is
    /// a fact worth surfacing: it precedes cost, latency and quality changes
    /// that nothing else on the board would explain.
    pub role: &'static str,
    /// `unknown` when the member is declared but never reported — silence is
    /// not health.
    pub status: String,
}

/// A thing the fabric can do, and how many members must be up for it to work.
#[derive(Serialize, Clone, Debug)]
pub struct CapabilityStatus {
    pub label: String,
    pub status: String,
    pub min_available: usize,
    pub available: usize,
    pub members: Vec<CapabilityMember>,
}

pub const ROLE_PRIMARY: &str = "primary";
pub const ROLE_FALLBACK: &str = "fallback";

/// Statuspage v2 severity words, so our top line and the vendor feeds we consume
/// speak one language. Component strings keep their current vocabulary — see
/// FSD §4 for why renaming them out from under two live consumers is a bad
/// trade for interop we can get additively.
pub fn indicator_for(status: &str) -> &'static str {
    match status {
        OPERATIONAL => "none",
        DEGRADED => "minor",
        "partial_outage" => "major",
        "major_outage" | OUTAGE => "critical",
        _ => "none",
    }
}

// ── /api/v1/status/events ────────────────────────────────────────────────────
/// One observed transition of one component. The thing a daily uptime rollup
/// cannot tell you: that `eu.proxy` was `degraded` for ninety seconds at 14:03.
#[derive(Serialize, Clone, PartialEq, Debug)]
pub struct StatusEvent {
    pub ts: String,
    /// Stable component id — `eu.proxy`, `llm.together`, `us.postgresql`.
    pub component: String,
    pub from: String,
    pub to: String,
}

#[derive(Serialize)]
pub struct EventsResponse {
    pub days: i64,
    pub events: Vec<StatusEvent>,
}

// ── /api/v1/scoring (public roster — Flow A projection) ──────────────────────
/// One opted-in agent in the public scoring roster. The consent-gated
/// projection of that agent's `capacity:*` `scores` rows (design §3).
#[derive(Serialize, Clone)]
pub struct RosterEntry {
    pub key_id: String,
    /// `capacity:composite` (𝒞_CIRIS).
    pub capacity_composite: Option<f64>,
    /// The five factors, if requested / available.
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub factors: BTreeMap<String, f64>,
    /// Freshness — the earliest `valid_until` across the rows behind this entry.
    pub valid_until: Option<String>,
}

#[derive(Serialize, Clone)]
pub struct Roster {
    pub timestamp: String,
    /// `consent` / `public_sample` — which projection tier this is.
    pub projection: String,
    pub agents: Vec<RosterEntry>,
}

// ── /api/v1/scoring/live + /api/v1/status/live (SSE/WS push payloads) ─────────
/// A live delta pushed over the websocket/SSE socket: the current roster +
/// aggregated service-health snapshot (design §3 "extra website sockets").
#[derive(Serialize, Clone)]
pub struct LiveDelta {
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub roster: Option<Roster>,
    /// Aggregated `operational|degraded|outage` overall string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overall: Option<String>,
}

// ── /api/v1/status/vantage ───────────────────────────────────────────────────
/// One component's day, as seen from every vantage that reported it.
///
/// Agreement across vantages implicates the component; disagreement implicates
/// the path between a vantage and it. Without this, a monitor cannot tell
/// "the provider is down" from "my route to it is", and attributes its own
/// network to the world.
#[derive(Serialize, Clone, Debug)]
pub struct VantageRow {
    pub date: String,
    pub component: String,
    /// Sample instants where at least one vantage reported.
    pub samples: i64,
    /// Instants where the vantages did NOT agree.
    pub disagreements: i64,
    /// Non-operational samples per vantage — who kept dissenting.
    pub dissent_by_vantage: BTreeMap<String, i64>,
}

#[derive(Serialize)]
pub struct VantageResponse {
    pub days: i64,
    pub rows: Vec<VantageRow>,
}

// ── /api/v1/status/history ───────────────────────────────────────────────────
#[derive(Serialize, Clone)]
pub struct ServiceUptime {
    pub uptime_pct: f64,
    pub avg_latency_ms: i64,
    pub outage_count: i64,
}

#[derive(Serialize)]
pub struct HistoryRegion {
    pub services: BTreeMap<String, ServiceUptime>,
    pub uptime_pct: f64,
}

/// Per-day rollup.
///
/// `uptime_pct` and `status` are the fields a status *page* actually renders one
/// bar from; `overall_uptime_pct` is kept as-is so existing consumers don't
/// break. Serving only the long name is what painted ciris.ai's 90-day bar
/// yellow at "0.00% uptime" for months: the front-end read `uptime_pct`,
/// defaulted the miss to `0`, and 0 < 99.9 renders degraded.
#[derive(Serialize)]
pub struct HistoryDay {
    pub date: String,
    pub regions: BTreeMap<String, HistoryRegion>,
    pub services: BTreeMap<String, ServiceUptime>,
    pub overall_uptime_pct: f64,
    /// Alias of `overall_uptime_pct`.
    pub uptime_pct: f64,
    /// Per-capability SLI for the day, computed by EXACT overlap (FSD §2.4).
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub capabilities: BTreeMap<String, CapabilitySli>,
    /// The worst capability's SLI — service availability, as opposed to the
    /// component mean `uptime_pct` keeps reporting.
    pub service_uptime_pct: f64,
    /// The day as one word: `operational` ≥ 99.9%, `degraded` ≥ 95%, else
    /// `outage`. Derived here so every consumer draws the same conclusion from
    /// the same number.
    pub status: &'static str,
}

/// Seconds between a `%Y-%m-%dT%H:%M:%SZ` stamp and `now`. An unparseable or
/// future stamp reads as `0` — never as "impossibly old", which would flap the
/// endpoint into `unknown` on a clock skew.
pub fn age_seconds(timestamp: &str, now: chrono::DateTime<chrono::Utc>) -> i64 {
    match chrono::NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%dT%H:%M:%SZ") {
        Ok(t) => (now.naive_utc() - t).num_seconds().max(0),
        Err(_) => 0,
    }
}

/// A capability's measured availability over a day.
#[derive(Serialize, Clone, Debug)]
pub struct CapabilitySli {
    pub sli_pct: f64,
    pub min_available: usize,
    pub members: Vec<String>,
}

/// Bucket a day's uptime percentage into the status vocabulary.
pub fn day_status(uptime_pct: f64) -> &'static str {
    if uptime_pct >= 99.9 {
        OPERATIONAL
    } else if uptime_pct >= 95.0 {
        DEGRADED
    } else {
        OUTAGE
    }
}

#[derive(Serialize)]
pub struct HistoryResponse {
    pub days: i64,
    pub region: Option<String>,
    pub history: Vec<HistoryDay>,
}
