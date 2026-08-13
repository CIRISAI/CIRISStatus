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
    pub status: String,
    pub latency_ms: Option<i64>,
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
