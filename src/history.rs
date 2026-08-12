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
    normalize_legacy_provider_names(&conn);
    purge_unmonitored_brave_rows(&conn);
    refile_legacy_llm_rows(&conn);
    purge_failure_only_service_rows(&conn);
    prune_beyond_retention(&conn);
    Ok(Arc::new(Mutex::new(conn)))
}

/// Rows written before the always-record fix are indistinguishable from valid
/// ones by content alone, so the repairs that cannot use a precise predicate are
/// bounded by this date instead. It is the day the fix deployed — anything
/// recorded from here on is already correct.
const LEGACY_REPAIR_BEFORE: &str = "2026-08-12";

/// Keep a bounded window. `/api/v1/status/history` accepts at most 365 days, so
/// 400 leaves the whole queryable range intact with room to spare, and stops an
/// append-only table growing without limit on a small node (~21k rows/day).
const RETENTION_DAYS: i64 = 400;

/// One-time repair: LLM providers are cross-region, and `poll_once` records them
/// under `global`. Before the `provider`-id fix, `is_llm` never matched (it was
/// comparing against display names), so every LLM sample was filed under the
/// region whose proxy reported it — inflating that region's row count and
/// double-counting the same external provider in `us` and `eu`.
///
/// The predicate is self-limiting: correct rows are already `global`, so a
/// non-global LLM row is by definition pre-fix. No date bound needed.
fn refile_legacy_llm_rows(conn: &Connection) {
    match conn.execute(
        "UPDATE status_checks SET region = 'global'
         WHERE service_name = 'cirisproxy'
           AND region <> 'global'
           AND provider_name IN ('openrouter', 'groq', 'together', 'openai')",
        [],
    ) {
        Ok(n) if n > 0 => {
            tracing::info!(rows = n, "history: re-filed legacy LLM rows under global")
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "history: LLM re-file failed"),
    }
}

/// One-time repair: drop the `service` rows written while that row was only
/// recorded on FAILURE.
///
/// Such a series is 0% uptime by construction — it exists exactly on the polls
/// that failed and is absent on every poll that succeeded — so averaging it with
/// real provider series does not measure availability, it just subtracts a fixed
/// penalty. Two failed polls out of 1440 published `cirisproxy.service: 0.0%`
/// and cost that region ~11 points. The successful polls it never wrote cannot
/// be reconstructed, so the biased series is removed rather than repaired.
/// Rows from [`LEGACY_REPAIR_BEFORE`] onward are written every poll and are kept.
fn purge_failure_only_service_rows(conn: &Connection) {
    match conn.execute(
        "DELETE FROM status_checks WHERE provider_name = 'service' AND ts < ?1",
        rusqlite::params![LEGACY_REPAIR_BEFORE],
    ) {
        Ok(n) if n > 0 => tracing::info!(
            rows = n,
            "history: purged failure-only service rows (0% by construction)"
        ),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "history: service-row purge failed"),
    }
}

/// Trim samples older than the queryable window.
fn prune_beyond_retention(conn: &Connection) {
    match conn.execute(
        "DELETE FROM status_checks WHERE ts < datetime('now', ?1)",
        rusqlite::params![format!("-{RETENTION_DAYS} days")],
    ) {
        Ok(n) if n > 0 => {
            tracing::info!(
                rows = n,
                days = RETENTION_DAYS,
                "history: pruned old samples"
            )
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "history: retention prune failed"),
    }
}

/// Rows recorded *before* this date are subject to the Brave purge below. A
/// hard cutoff, not an open-ended `WHERE provider_name = 'brave'`: if search
/// health is ever legitimately reported again, this repair must not eat it on
/// the next restart.
const BRAVE_PURGE_BEFORE: &str = "2026-08-13";

/// One-time repair: drop the CIRISProxy `brave` rows that recorded a
/// deliberately-disabled key as an outage.
///
/// CIRISProxy health-checked Brave with a live, billable search request. The key
/// was disabled to stop that spend — but an unconfigured provider reported
/// `outage`, so the poller wrote ~1440 outage rows a day for a service that was
/// never down. Because a region's uptime is an unweighted mean over its provider
/// rows, that one component published 73.2% overall uptime on a day when
/// everything was healthy. The probe is gone from CIRISProxy now (search health
/// is monitored passively); these rows are the residue.
fn purge_unmonitored_brave_rows(conn: &Connection) {
    match conn.execute(
        "DELETE FROM status_checks
         WHERE service_name = 'cirisproxy'
           AND provider_name IN ('brave', 'Brave Search')
           AND ts < ?1",
        rusqlite::params![BRAVE_PURGE_BEFORE],
    ) {
        Ok(n) if n > 0 => tracing::info!(
            rows = n,
            "history: purged brave rows from the disabled-key window"
        ),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "history: brave purge failed"),
    }
}

/// One-time repair of rows written while the upstream parser keyed CIRISProxy's
/// providers by their DISPLAY name. Those rows read `cirisproxy.Brave Search`
/// in `/api/v1/status/history`; new rows use the stable id, so without this the
/// series splits in two at the deploy and every chart shows a cliff.
///
/// Idempotent (matches only the legacy spellings) and scoped to `cirisproxy`,
/// whose display names are a known, closed set.
fn normalize_legacy_provider_names(conn: &Connection) {
    const RENAMES: &[(&str, &str)] = &[
        ("OpenRouter", "openrouter"),
        ("Groq", "groq"),
        ("Together AI", "together"),
        ("Brave Search", "brave"),
        ("CIRISBilling", "billing"),
    ];
    for (old, new) in RENAMES {
        match conn.execute(
            "UPDATE status_checks SET provider_name = ?1
             WHERE service_name = 'cirisproxy' AND provider_name = ?2",
            rusqlite::params![new, old],
        ) {
            Ok(n) if n > 0 => {
                tracing::info!(
                    rows = n,
                    from = old,
                    to = new,
                    "history: normalized legacy provider name"
                )
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, from = old, "history: provider-name migration failed")
            }
        }
    }
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
///
/// Each upstream's own `service` row is written on EVERY poll, not only when the
/// fetch fails. Recording it only on failure made it 0% by construction: the row
/// existed exactly on the polls that were down, so two bad polls out of 1440
/// published `cirisproxy.service: 0.0% uptime` and — since a region's uptime is
/// an unweighted mean over its provider rows — cost that region ~11 points.
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
            // The service's own reachability, recorded EVERY poll — see
            // `poll_once`'s note on why a failure-only row is unreadable.
            rows.push((
                "cirisbilling".into(),
                "service".into(),
                region.key.into(),
                probe,
            ));
            for p in providers {
                rows.push((
                    "cirisbilling".into(),
                    p.id,
                    region.key.into(),
                    Probe {
                        status: leak(p.status),
                        latency_ms: p.latency_ms,
                        message: None,
                    },
                ));
            }
        }
        if let Some(url) = &region.proxy_url {
            let (probe, body) = fetch_service_status(client, url).await;
            let providers = body
                .as_ref()
                .map(crate::aggregate::upstream_providers)
                .unwrap_or_default();
            rows.push((
                "cirisproxy".into(),
                "service".into(),
                region.key.into(),
                probe,
            ));
            for p in providers {
                // LLM providers are cross-region → record under "global".
                // Trust the upstream's own `type` first; the id list only
                // covers an upstream that doesn't declare one.
                let is_llm = p.kind.as_deref() == Some("llm")
                    || (p.kind.is_none()
                        && matches!(p.id.as_str(), "openrouter" | "groq" | "together" | "openai"));
                let reg = if is_llm {
                    "global".to_string()
                } else {
                    region.key.to_string()
                };
                rows.push((
                    "cirisproxy".into(),
                    p.id,
                    reg,
                    Probe {
                        status: leak(p.status),
                        latency_ms: p.latency_ms,
                        message: None,
                    },
                ));
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
    // `outages` counts INCIDENTS, not polls: a row is an incident only when the
    // previous sample of the same series was not already an outage. Summing
    // `status='outage'` counted one "outage" per poll, so a single sustained
    // failure published as 1438 outages in a day — a number that says nothing
    // about how often the thing actually broke. LAG() gives us the transition.
    let mut sql = String::from(
        "SELECT day, region, service_name, provider_name,
                AVG(CASE WHEN status='operational' THEN 100.0 ELSE 0.0 END) AS uptime,
                AVG(COALESCE(latency_ms,0)) AS lat,
                SUM(CASE WHEN status='outage' AND (prev IS NULL OR prev<>'outage')
                         THEN 1 ELSE 0 END) AS outages
         FROM (
            SELECT date(ts) AS day, region, service_name, provider_name, status, latency_ms,
                   LAG(status) OVER (
                       PARTITION BY region, service_name, provider_name ORDER BY ts
                   ) AS prev
            FROM status_checks
            WHERE ts >= datetime('now', ?1)",
    );
    if region.is_some() {
        sql.push_str(" AND region = ?2");
    }
    sql.push_str(" ) GROUP BY day, region, service_name, provider_name ORDER BY day");

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
            uptime_pct: overall,
            status: crate::model::day_status(overall),
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

#[cfg(test)]
mod repair_tests {
    use super::*;

    fn tmp_db() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("ciris-status-hist-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("status.db").to_string_lossy().into_owned()
    }

    fn insert(conn: &Connection, ts: &str, service: &str, provider: &str, status: &str) {
        conn.execute(
            "INSERT INTO status_checks (ts, service_name, provider_name, region, status, latency_ms)
             VALUES (?1, ?2, ?3, 'us', ?4, 10)",
            rusqlite::params![ts, service, provider, status],
        )
        .unwrap();
    }

    fn names(conn: &Connection) -> Vec<(String, String)> {
        let mut stmt = conn
            .prepare("SELECT provider_name, status FROM status_checks ORDER BY provider_name, ts")
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        rows
    }

    /// `outage_count` must count INCIDENTS, not polls. A sustained failure is
    /// one outage, however many samples it spans — the old `SUM(status='outage')`
    /// published "1438 outages" for a single stuck component.
    #[test]
    fn outage_count_counts_incidents_not_polls() {
        let path = tmp_db();
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        {
            let db = init(&path).unwrap();
            let conn = db.lock().unwrap();
            // one incident spanning 5 polls, recovery, then a second incident
            let seq = [
                ("00:00:00", "operational"),
                ("00:01:00", "outage"),
                ("00:02:00", "outage"),
                ("00:03:00", "outage"),
                ("00:04:00", "outage"),
                ("00:05:00", "outage"),
                ("00:06:00", "operational"),
                ("00:07:00", "outage"),
                ("00:08:00", "operational"),
            ];
            for (t, st) in seq {
                insert(&conn, &format!("{today}T{t}Z"), "cirisproxy", "groq", st);
            }
        }
        let db = init(&path).unwrap();
        let days = query_history(&db, 2, None).unwrap();
        let day = days.iter().find(|d| d.date == today).expect("today");
        let row = day.services.values().next().expect("one series");
        assert_eq!(row.outage_count, 2, "two incidents, not six outage samples");
        // 3 operational of 9 samples.
        assert!(
            (row.uptime_pct - 33.3).abs() < 0.2,
            "got {}",
            row.uptime_pct
        );
    }

    /// Legacy LLM rows were filed under the reporting region; they belong to
    /// `global`. The predicate is self-limiting, so it is safe to run forever.
    #[test]
    fn llm_rows_are_refiled_to_global_and_repeat_is_a_noop() {
        let path = tmp_db();
        {
            let db = init(&path).unwrap();
            let conn = db.lock().unwrap();
            insert(
                &conn,
                "2026-08-11T00:00:00Z",
                "cirisproxy",
                "groq",
                "operational",
            );
            insert(
                &conn,
                "2026-08-11T00:00:00Z",
                "cirisproxy",
                "together",
                "operational",
            );
            // Not an LLM provider: stays in its region.
            insert(
                &conn,
                "2026-08-11T00:00:00Z",
                "cirisproxy",
                "billing",
                "operational",
            );
            // A billing-service row of the same name must not be touched.
            insert(
                &conn,
                "2026-08-11T00:00:00Z",
                "cirisbilling",
                "groq",
                "operational",
            );
        }
        let db = init(&path).unwrap();
        let conn = db.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT service_name, provider_name, region FROM status_checks ORDER BY service_name, provider_name")
            .unwrap();
        let rows: Vec<(String, String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(rows.contains(&("cirisproxy".into(), "groq".into(), "global".into())));
        assert!(rows.contains(&("cirisproxy".into(), "together".into(), "global".into())));
        assert!(rows.contains(&("cirisproxy".into(), "billing".into(), "us".into())));
        assert!(
            rows.contains(&("cirisbilling".into(), "groq".into(), "us".into())),
            "another service's provider of the same name is untouched"
        );
    }

    /// The failure-only `service` series is removed for the legacy window and
    /// kept for the always-recorded window.
    #[test]
    fn failure_only_service_rows_are_purged_only_before_the_cutoff() {
        let path = tmp_db();
        {
            let db = init(&path).unwrap();
            let conn = db.lock().unwrap();
            insert(
                &conn,
                "2026-08-10T00:00:00Z",
                "cirisproxy",
                "service",
                "outage",
            );
            insert(
                &conn,
                "2026-08-11T00:00:00Z",
                "cirisbilling",
                "service",
                "outage",
            );
            insert(
                &conn,
                "2026-08-20T00:00:00Z",
                "cirisproxy",
                "service",
                "operational",
            );
            insert(
                &conn,
                "2026-08-10T00:00:00Z",
                "cirisproxy",
                "groq",
                "operational",
            );
        }
        let db = init(&path).unwrap();
        let conn = db.lock().unwrap();
        let got = names(&conn);
        assert_eq!(
            got.iter().filter(|(n, _)| n == "service").count(),
            1,
            "only the post-cutoff service row survives"
        );
        assert!(got
            .iter()
            .any(|(n, s)| n == "service" && s == "operational"));
        assert!(got.iter().any(|(n, _)| n == "groq"), "providers untouched");
    }

    /// A day now carries the fields a status page renders from: the alias plus a
    /// one-word verdict.
    #[test]
    fn day_carries_uptime_alias_and_status() {
        let path = tmp_db();
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        {
            let db = init(&path).unwrap();
            let conn = db.lock().unwrap();
            insert(
                &conn,
                &format!("{today}T00:00:00Z"),
                "cirisproxy",
                "groq",
                "operational",
            );
        }
        let db = init(&path).unwrap();
        let days = query_history(&db, 2, None).unwrap();
        let day = days.iter().find(|d| d.date == today).expect("today");
        assert_eq!(day.uptime_pct, day.overall_uptime_pct);
        assert_eq!(day.status, "operational");
        assert_eq!(crate::model::day_status(99.95), "operational");
        assert_eq!(crate::model::day_status(97.0), "degraded");
        assert_eq!(crate::model::day_status(80.0), "outage");
    }

    /// Legacy display-name rows are renamed to the stable id, so the history
    /// series doesn't split in two at the deploy.
    #[test]
    fn migration_renames_display_names_and_is_idempotent() {
        let path = tmp_db();
        {
            let db = init(&path).unwrap();
            let conn = db.lock().unwrap();
            insert(
                &conn,
                "2026-08-11T00:00:00Z",
                "cirisproxy",
                "Together AI",
                "operational",
            );
            insert(
                &conn,
                "2026-08-11T00:01:00Z",
                "cirisproxy",
                "OpenRouter",
                "operational",
            );
            // A same-named provider on a DIFFERENT service must not be touched.
            insert(
                &conn,
                "2026-08-11T00:02:00Z",
                "cirisbilling",
                "OpenRouter",
                "operational",
            );
        }
        // Re-open twice: the repair runs on every boot and must be idempotent.
        let db = init(&path).unwrap();
        drop(db);
        let db = init(&path).unwrap();
        let conn = db.lock().unwrap();
        let got = names(&conn);
        assert!(got.contains(&("together".into(), "operational".into())));
        assert!(got.contains(&("openrouter".into(), "operational".into())));
        assert_eq!(
            got.iter().filter(|(n, _)| n == "OpenRouter").count(),
            1,
            "the cirisbilling row keeps its own name"
        );
    }

    /// The disabled-key Brave rows are purged — under either spelling, since the
    /// rename runs first — and only inside the historical window.
    #[test]
    fn brave_purge_is_bounded_by_the_cutoff() {
        let path = tmp_db();
        {
            let db = init(&path).unwrap();
            let conn = db.lock().unwrap();
            insert(
                &conn,
                "2026-08-11T00:00:00Z",
                "cirisproxy",
                "Brave Search",
                "outage",
            );
            insert(
                &conn,
                "2026-08-12T00:00:00Z",
                "cirisproxy",
                "brave",
                "outage",
            );
            // After the cutoff: a legitimate future report survives.
            insert(
                &conn,
                "2026-09-01T00:00:00Z",
                "cirisproxy",
                "brave",
                "operational",
            );
            insert(
                &conn,
                "2026-08-11T00:00:00Z",
                "cirisproxy",
                "groq",
                "operational",
            );
        }
        let db = init(&path).unwrap();
        let conn = db.lock().unwrap();
        let got = names(&conn);
        assert_eq!(
            got.iter().filter(|(n, _)| n == "brave").count(),
            1,
            "only the post-cutoff brave row remains"
        );
        assert!(got.iter().any(|(n, s)| n == "brave" && s == "operational"));
        assert!(
            got.iter().any(|(n, _)| n == "groq"),
            "other providers untouched"
        );
    }

    /// The published uptime the purge restores: with the bogus 0% component
    /// gone, the day reads ~100% instead of being dragged down by one row.
    #[test]
    fn purge_restores_published_uptime() {
        let path = tmp_db();
        let today = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        {
            let db = init(&path).unwrap();
            let conn = db.lock().unwrap();
            for p in ["groq", "openrouter", "together", "service"] {
                insert(&conn, &today, "cirisproxy", p, "operational");
            }
            // Pre-cutoff brave outages, exactly as the poller wrote them.
            for _ in 0..10 {
                insert(
                    &conn,
                    "2026-08-11T00:00:00Z",
                    "cirisproxy",
                    "brave",
                    "outage",
                );
            }
        }
        let db = init(&path).unwrap();
        let days = query_history(&db, 365, None).unwrap();
        for day in &days {
            assert_eq!(
                day.overall_uptime_pct,
                100.0,
                "no day may be dragged by the purged component: {day:?}",
                day = day.date
            );
        }
    }
}
