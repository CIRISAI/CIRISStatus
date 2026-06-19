//! Uptime history — a single append-only SQLite table written by a 60s poller,
//! read by `/api/v1/status/history` via a plain daily `GROUP BY` rollup (no
//! TimescaleDB needed). `uptime_pct = mean(status == operational) * 100`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use rusqlite::Connection;

use crate::config::Config;
use crate::model::{HistoryDay, HistoryRegion, ServiceUptime};
use crate::probe::{check_grafana, check_postgres_tcp, fetch_service_status, Probe};

pub type Db = Arc<Mutex<Connection>>;

pub fn init(path: &str) -> Result<Db> {
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS status_checks (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            ts           TEXT    NOT NULL,
            service_name TEXT    NOT NULL,
            provider_name TEXT   NOT NULL,
            region       TEXT    NOT NULL DEFAULT 'global',
            status       TEXT    NOT NULL,
            latency_ms   INTEGER
         );
         CREATE INDEX IF NOT EXISTS idx_status_checks_ts ON status_checks(ts);
         CREATE INDEX IF NOT EXISTS idx_status_checks_region ON status_checks(region);",
    )?;
    Ok(Arc::new(Mutex::new(conn)))
}

fn record(conn: &Connection, ts: &str, service: &str, provider: &str, region: &str, p: &Probe) {
    let _ = conn.execute(
        "INSERT INTO status_checks (ts, service_name, provider_name, region, status, latency_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![ts, service, provider, region, p.status, p.latency_ms],
    );
}

/// One poll cycle: probe everything we track and append rows. Region "global"
/// for cross-region providers (LLMs, local deps), the region key otherwise.
/// Driven by the StatusAdapter's `run_lifecycle` interval loop.
pub async fn poll_once(cfg: &Config, client: &reqwest::Client, db: &Db) {
    let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let mut rows: Vec<(String, String, String, Probe)> = Vec::new();

    if let Some(dsn) = &cfg.database_url {
        rows.push((
            "cirislens".into(),
            "postgresql".into(),
            "global".into(),
            check_postgres_tcp(dsn).await,
        ));
    }
    if let Some(g) = &cfg.grafana_url {
        rows.push((
            "cirislens".into(),
            "grafana".into(),
            "global".into(),
            check_grafana(client, g).await,
        ));
    }

    for region in &cfg.regions {
        if let Some(url) = &region.billing_url {
            let (probe, body) = fetch_service_status(client, url).await;
            let providers = body
                .as_ref()
                .map(crate::aggregate::upstream_providers)
                .unwrap_or_default();
            if providers.is_empty() {
                rows.push((
                    "cirisbilling".into(),
                    "service".into(),
                    region.key.into(),
                    probe,
                ));
            } else {
                for (name, status, latency) in providers {
                    rows.push((
                        "cirisbilling".into(),
                        name,
                        region.key.into(),
                        Probe {
                            status: leak(status),
                            latency_ms: latency,
                            message: None,
                        },
                    ));
                }
            }
        }
        if let Some(url) = &region.proxy_url {
            let (probe, body) = fetch_service_status(client, url).await;
            let providers = body
                .as_ref()
                .map(crate::aggregate::upstream_providers)
                .unwrap_or_default();
            if providers.is_empty() {
                rows.push((
                    "cirisproxy".into(),
                    "service".into(),
                    region.key.into(),
                    probe,
                ));
            } else {
                for (name, status, latency) in providers {
                    // LLM providers are cross-region → record under "global".
                    let is_llm =
                        matches!(name.as_str(), "openrouter" | "groq" | "together" | "openai");
                    let reg = if is_llm {
                        "global".to_string()
                    } else {
                        region.key.to_string()
                    };
                    rows.push((
                        "cirisproxy".into(),
                        name,
                        reg,
                        Probe {
                            status: leak(status),
                            latency_ms: latency,
                            message: None,
                        },
                    ));
                }
            }
        }
    }

    if let Ok(conn) = db.lock() {
        for (service, provider, region, p) in &rows {
            record(&conn, &ts, service, provider, region, p);
        }
    }
}

// The probe status field is `&'static str`; upstream statuses are owned strings.
// Map the three known values to statics (anything else → "operational").
fn leak(s: String) -> &'static str {
    match s.as_str() {
        "degraded" => "degraded",
        "outage" => "outage",
        _ => "operational",
    }
}

/// `/api/v1/status/history` rollup: daily uptime per region/service/provider.
pub fn query_history(db: &Db, days: i64, region: Option<&str>) -> Result<Vec<HistoryDay>> {
    let conn = db.lock().map_err(|_| anyhow::anyhow!("db poisoned"))?;
    let since = format!("-{days} days");
    let mut sql = String::from(
        "SELECT date(ts) AS day, region, service_name, provider_name,
                AVG(CASE WHEN status='operational' THEN 100.0 ELSE 0.0 END) AS uptime,
                AVG(COALESCE(latency_ms,0)) AS lat,
                SUM(CASE WHEN status='outage' THEN 1 ELSE 0 END) AS outages
         FROM status_checks
         WHERE ts >= datetime('now', ?1)",
    );
    if region.is_some() {
        sql.push_str(" AND region = ?2");
    }
    sql.push_str(" GROUP BY day, region, service_name, provider_name ORDER BY day");

    let mut stmt = conn.prepare(&sql)?;
    let map_row = |r: &rusqlite::Row| {
        Ok((
            r.get::<_, String>(0)?, // day
            r.get::<_, String>(1)?, // region
            r.get::<_, String>(2)?, // service
            r.get::<_, String>(3)?, // provider
            r.get::<_, f64>(4)?,    // uptime
            r.get::<_, f64>(5)?,    // latency
            r.get::<_, i64>(6)?,    // outages
        ))
    };
    let rows: Vec<_> = if let Some(reg) = region {
        stmt.query_map(rusqlite::params![since, reg], map_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        stmt.query_map(rusqlite::params![since], map_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };

    // Group by day → per-region nested services + a flat "region.service.provider".
    // (nested per-region services, flat region.service.provider) for one day.
    type DayRollup = (
        BTreeMap<String, BTreeMap<String, ServiceUptime>>,
        BTreeMap<String, ServiceUptime>,
    );
    let mut by_day: BTreeMap<String, DayRollup> = BTreeMap::new();
    for (day, region, service, provider, uptime, lat, outages) in rows {
        let su = ServiceUptime {
            uptime_pct: round1(uptime),
            avg_latency_ms: lat.round() as i64,
            outage_count: outages,
        };
        let entry = by_day.entry(day).or_default();
        entry
            .0
            .entry(region.clone())
            .or_default()
            .insert(format!("{service}.{provider}"), su.clone());
        entry.1.insert(format!("{region}.{service}.{provider}"), su);
    }

    let mut out = Vec::new();
    for (date, (regions_raw, flat)) in by_day {
        let mut regions = BTreeMap::new();
        for (reg, services) in regions_raw {
            let mean = mean_uptime(services.values());
            regions.insert(
                reg,
                HistoryRegion {
                    services,
                    uptime_pct: mean,
                },
            );
        }
        let overall = if flat.is_empty() {
            100.0
        } else {
            mean_uptime(flat.values())
        };
        out.push(HistoryDay {
            date,
            regions,
            services: flat,
            overall_uptime_pct: overall,
        });
    }
    Ok(out)
}

fn mean_uptime<'a>(it: impl Iterator<Item = &'a ServiceUptime>) -> f64 {
    let (sum, n) = it.fold((0.0, 0i64), |(s, n), u| (s + u.uptime_pct, n + 1));
    if n == 0 {
        100.0
    } else {
        round1(sum / n as f64)
    }
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}
