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

/// Worst (most-severe) of a set of component statuses; `None` → no components.
pub fn worst<'a>(statuses: impl IntoIterator<Item = &'a str>) -> Option<&'static str> {
    let mut rank = -1i8;
    for s in statuses {
        let r = match s {
            OPERATIONAL => 0,
            DEGRADED => 1,
            OUTAGE => 2,
            _ => 0,
        };
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

#[derive(Serialize)]
pub struct RegionStatus {
    pub name: String,
    pub status: String,
    pub services: BTreeMap<String, ServiceSummary>,
}

#[derive(Serialize)]
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

#[derive(Serialize)]
pub struct AggregatedStatus {
    pub status: String,
    pub timestamp: String,
    pub last_incident: Option<serde_json::Value>,
    pub regions: BTreeMap<String, RegionStatus>,
    pub infrastructure: BTreeMap<String, InfrastructureStatus>,
    pub llm_providers: BTreeMap<String, ProviderDetail>,
    pub auth_providers: BTreeMap<String, ProviderDetail>,
    pub database_providers: BTreeMap<String, ProviderDetail>,
    pub internal_providers: BTreeMap<String, ProviderDetail>,
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

#[derive(Serialize)]
pub struct HistoryDay {
    pub date: String,
    pub regions: BTreeMap<String, HistoryRegion>,
    pub services: BTreeMap<String, ServiceUptime>,
    pub overall_uptime_pct: f64,
}

#[derive(Serialize)]
pub struct HistoryResponse {
    pub days: i64,
    pub region: Option<String>,
    pub history: Vec<HistoryDay>,
}
