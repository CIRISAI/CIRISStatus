//! Uptime history — a single append-only SQLite table written by a 60s poller,
//! read by `/api/v1/status/history` via a plain daily `GROUP BY` rollup (no
//! TimescaleDB needed). `uptime_pct = mean(status == operational) * 100`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use rusqlite::Connection;

use crate::config::Config;
use crate::model::{CapabilitySli, HistoryDay, HistoryRegion, ServiceUptime, StatusEvent, OUTAGE};
use crate::probe::{check_grafana, check_postgres_tcp, fetch_service_status, Probe};

pub type Db = Arc<Mutex<Connection>>;

/// Rows recorded for ATTRIBUTION, never for arithmetic: one per reporting
/// region for a provider that several regions can see. Excluded from every
/// uptime rollup — they are extra views of one component, not extra components.
pub const OBSERVATION_SERVICE: &str = "observation";

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
         CREATE INDEX IF NOT EXISTS idx_status_checks_region ON status_checks(region);
         -- Transitions, not samples. A daily uptime rollup cannot tell you that
         -- eu.proxy was degraded for ninety seconds at 14:03 — a 60s blip moves
         -- a day's mean by 0.07% and is indistinguishable from noise. This is
         -- where a transient becomes a thing you can point at.
         CREATE TABLE IF NOT EXISTS status_events (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            ts        TEXT NOT NULL,
            component TEXT NOT NULL,
            from_status TEXT NOT NULL,
            to_status TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_status_events_ts ON status_events(ts);",
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
    for table in ["status_checks", "status_events"] {
        prune_table(conn, table);
    }
}

fn prune_table(conn: &Connection, table: &str) {
    match conn.execute(
        // Compare against a cutoff in OUR stored format, not `datetime()`'s.
        &format!("DELETE FROM {table} WHERE ts < strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ?1)"),
        rusqlite::params![format!("-{RETENTION_DAYS} days")],
    ) {
        Ok(n) if n > 0 => {
            tracing::info!(
                rows = n,
                table,
                days = RETENTION_DAYS,
                "history: pruned old rows"
            )
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, table, "history: retention prune failed"),
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

/// Append observed transitions, all or nothing.
///
/// Transactional and fallible on purpose: the caller advances its baseline only
/// when this returns `Ok`. Swallowing the error and advancing anyway meant a
/// failed write lost the transition *permanently* — the component stays in its
/// new state, so the next cycle sees no diff and there is nothing left to retry.
pub fn record_events(db: &Db, events: &[StatusEvent]) -> Result<usize> {
    if events.is_empty() {
        return Ok(0);
    }
    let mut guard = db.lock().map_err(|_| anyhow::anyhow!("db poisoned"))?;
    let tx = guard.transaction()?;
    for e in events {
        tx.execute(
            "INSERT INTO status_events (ts, component, from_status, to_status)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![e.ts, e.component, e.from, e.to],
        )?;
    }
    tx.commit()?;
    Ok(events.len())
}

/// A capability's availability per day, computed by EXACT overlap.
///
/// Every row in a poll cycle shares one timestamp, so "were enough members up
/// AT THE SAME INSTANT" is a `GROUP BY ts` — we hold the raw samples, so we
/// measure the overlap instead of bounding it. A consumer working from daily
/// rollups can only say "capability uptime is at least the best member's";
/// this says what it was (FSD §2.4).
pub fn query_capability_sli(
    db: &Db,
    days: i64,
    spec: &crate::capability::CapabilitySpec,
) -> Result<BTreeMap<String, f64>> {
    if spec.members.is_empty() {
        return Ok(BTreeMap::new());
    }
    let conn = db.lock().map_err(|_| anyhow::anyhow!("db poisoned"))?;
    let placeholders = spec
        .members
        .iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(",");
    let obs = OBSERVATION_SERVICE;
    let sql = format!(
        "SELECT day, AVG(CASE WHEN available >= ?1 THEN 100.0 ELSE 0.0 END) AS sli FROM (
             SELECT date(ts) AS day, ts,
                    -- DISTINCT: a provider several regions report is still ONE
                    -- member. Counting rows let `available` exceed the member
                    -- count and satisfy a threshold that was never met.
                    COUNT(DISTINCT CASE WHEN status = 'operational'
                                        THEN provider_name END) AS available
             FROM status_checks
             WHERE ts >= strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ?2)
               AND service_name <> '{obs}'
               AND provider_name IN ({placeholders})
             GROUP BY ts)
         GROUP BY day"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![
        Box::new(spec.min_available.max(1) as i64),
        Box::new(format!("-{days} days")),
    ];
    for (id, _) in &spec.members {
        params.push(Box::new(id.clone()));
    }
    let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let rows = stmt
        .query_map(refs.as_slice(), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows.into_iter().collect())
}

/// `/api/v1/status/vantage` — where the vantages disagreed.
///
/// Every region's proxy reports the same external providers, so we hold several
/// independent views of one component. Agreement implicates the component;
/// disagreement implicates the path. This is the query that answers "is it them
/// or is it us" from our own data, without asking a vendor status page that
/// half of them do not publish.
pub fn query_vantage(db: &Db, days: i64) -> Result<Vec<crate::model::VantageRow>> {
    let conn = db.lock().map_err(|_| anyhow::anyhow!("db poisoned"))?;
    let since = format!("-{days} days");

    let mut stmt = conn.prepare(
        "SELECT day, provider_name, COUNT(*) AS samples,
                SUM(CASE WHEN distinct_statuses > 1 THEN 1 ELSE 0 END) AS disagreements
         FROM (
            SELECT date(ts) AS day, ts, provider_name,
                   COUNT(DISTINCT status) AS distinct_statuses
            FROM status_checks
            WHERE service_name = ?1
              AND ts >= strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ?2)
            GROUP BY ts, provider_name)
         GROUP BY day, provider_name
         ORDER BY day DESC, provider_name",
    )?;
    let base = stmt
        .query_map(rusqlite::params![OBSERVATION_SERVICE, since], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut dissent = conn.prepare(
        "SELECT date(ts), provider_name, region,
                SUM(CASE WHEN status <> 'operational' THEN 1 ELSE 0 END)
         FROM status_checks
         WHERE service_name = ?1
           AND ts >= strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ?2)
         GROUP BY date(ts), provider_name, region",
    )?;
    let mut by_key: BTreeMap<(String, String), BTreeMap<String, i64>> = BTreeMap::new();
    for row in dissent.query_map(rusqlite::params![OBSERVATION_SERVICE, since], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })? {
        let (day, component, region, bad) = row?;
        by_key
            .entry((day, component))
            .or_default()
            .insert(region, bad);
    }

    Ok(base
        .into_iter()
        .map(
            |(date, component, samples, disagreements)| crate::model::VantageRow {
                dissent_by_vantage: by_key
                    .get(&(date.clone(), component.clone()))
                    .cloned()
                    .unwrap_or_default(),
                date,
                component,
                samples,
                disagreements,
            },
        )
        .collect())
}

/// `/api/v1/status/events` — transitions newest-first within the window.
pub fn query_events(db: &Db, days: i64, limit: i64) -> Result<Vec<StatusEvent>> {
    let conn = db.lock().map_err(|_| anyhow::anyhow!("db poisoned"))?;
    let mut stmt = conn.prepare(
        "SELECT ts, component, from_status, to_status FROM status_events
         WHERE ts >= strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ?1)
         ORDER BY ts DESC, id DESC LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![format!("-{days} days"), limit], |r| {
            Ok(StatusEvent {
                ts: r.get(0)?,
                component: r.get(1)?,
                from: r.get(2)?,
                to: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
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
    // EVERY probe counts toward the vantage verdict, local ones included:
    // counting only the regional requests meant three failed regional probes
    // could declare a monitor-network failure and clear a local row that had
    // just succeeded — proving the network worked.
    let (mut attempted, mut transport_failures) = (0usize, 0usize);
    // Cross-region providers, merged worst-wins into ONE row per cycle.
    let mut cross_region: BTreeMap<String, (String, Option<i64>)> = BTreeMap::new();
    // The same providers as each region actually saw them — kept for
    // attribution, excluded from every rollup.
    let mut observations: Vec<(String, String, String, Probe)> = Vec::new();

    if let Some(dsn) = &cfg.database_url {
        rows.push(("cirislens".into(), "postgresql".into(), "global".into(), {
            let p = check_postgres_tcp(dsn).await;
            attempted += 1;
            transport_failures += usize::from(p.transport_error);
            p
        }));
    }
    if let Some(g) = &cfg.grafana_url {
        rows.push(("cirislens".into(), "grafana".into(), "global".into(), {
            let p = check_grafana(client, g).await;
            attempted += 1;
            transport_failures += usize::from(p.transport_error);
            p
        }));
    }

    for region in &cfg.regions {
        if let Some(url) = &region.billing_url {
            let (probe, body) = fetch_service_status(client, url, region.latency_baseline_ms).await;
            attempted += 1;
            transport_failures += usize::from(probe.transport_error);
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
                        transport_error: false,
                        upstream_status: None,
                    },
                ));
            }
        }
        if let Some(url) = &region.proxy_url {
            let (probe, body) = fetch_service_status(client, url, region.latency_baseline_ms).await;
            attempted += 1;
            transport_failures += usize::from(probe.transport_error);
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
                // Cross-region providers are reported by EVERY region's proxy,
                // so they need care in two directions.
                let is_llm = p.kind.as_deref() == Some("llm")
                    || (p.kind.is_none()
                        && matches!(p.id.as_str(), "openrouter" | "groq" | "together" | "openai"));
                if is_llm {
                    // One canonical row per provider per cycle, worst-wins.
                    // Writing one per reporting region — all labelled `global` —
                    // duplicated the provider at the same timestamp, which
                    // double-counts it in the daily mean and, worse, lets a
                    // capability's `available` count exceed its member count and
                    // satisfy a threshold that was never met.
                    cross_region
                        .entry(p.id.clone())
                        .and_modify(|(st, lat)| {
                            if crate::model::severity(&p.status) > crate::model::severity(st) {
                                *st = p.status.clone();
                                *lat = p.latency_ms;
                            }
                        })
                        .or_insert((p.status.clone(), p.latency_ms));
                    // AND keep this region's own view, under a service name the
                    // rollups ignore. Agreement across regions implicates the
                    // provider; disagreement implicates the path or our vantage.
                    // Merging destroyed that signal — this is where it lives now.
                    observations.push((
                        OBSERVATION_SERVICE.to_string(),
                        p.id,
                        region.key.to_string(),
                        Probe {
                            status: leak(p.status),
                            latency_ms: p.latency_ms,
                            message: None,
                            transport_error: false,
                            upstream_status: None,
                        },
                    ));
                } else {
                    rows.push((
                        "cirisproxy".into(),
                        p.id,
                        region.key.to_string(),
                        Probe {
                            status: leak(p.status),
                            latency_ms: p.latency_ms,
                            message: None,
                            transport_error: false,
                            upstream_status: None,
                        },
                    ));
                }
            }
        }
    }

    for (id, (status, latency_ms)) in cross_region {
        rows.push((
            "cirisproxy".into(),
            id,
            "global".into(),
            Probe {
                status: leak(status),
                latency_ms,
                message: None,
                transport_error: false,
                upstream_status: None,
            },
        ));
    }

    // Every probe failed at the transport layer. Unrelated third parties on
    // three continents do not fail in the same second; our network does. Record
    // that, and record NOTHING about the components we could not see — writing
    // them all down as outages is how a monitor's own flicker became four days
    // of everyone else's downtime (FSD §3.3 / D3).
    let blind = crate::probe::is_vantage_failure(attempted, transport_failures);
    if !blind {
        rows.extend(observations);
    } else {
        tracing::warn!(
            attempted,
            "all probes failed at transport — recording monitor.network, not a global outage"
        );
        rows.clear();
    }
    // Recorded on EVERY poll, healthy or not. Writing it only on failure would
    // make it 0% uptime by construction — present exactly on the polls that
    // failed, absent on every one that worked — and drag the daily mean, which
    // is the identical defect this poller already fixed once for `service` rows.
    rows.push((
        "monitor".into(),
        "network".into(),
        "global".into(),
        Probe {
            status: if blind {
                OUTAGE
            } else {
                crate::model::OPERATIONAL
            },
            latency_ms: None,
            message: blind.then(|| "all probes failed at transport".to_string()),
            transport_error: blind,
            upstream_status: None,
        },
    ));

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
pub fn query_history(
    db: &Db,
    days: i64,
    region: Option<&str>,
    specs: &[crate::capability::CapabilitySpec],
) -> Result<Vec<HistoryDay>> {
    // Capability SLIs are computed per spec across the whole window, then
    // attached per day.
    let mut sli_by_spec: Vec<(&crate::capability::CapabilitySpec, BTreeMap<String, f64>)> =
        Vec::new();
    for spec in specs {
        match query_capability_sli(db, days, spec) {
            Ok(m) => sli_by_spec.push((spec, m)),
            Err(e) => tracing::warn!(error = %e, id = %spec.id, "capability SLI query failed"),
        }
    }
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
            WHERE ts >= strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ?1)
              AND service_name <> 'observation'",
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
        let mut capabilities: BTreeMap<String, CapabilitySli> = BTreeMap::new();
        for (spec, by_day) in &sli_by_spec {
            if let Some(pct) = by_day.get(&date) {
                capabilities.insert(
                    spec.id.clone(),
                    CapabilitySli {
                        sli_pct: round1(*pct),
                        min_available: spec.min_available.max(1),
                        members: spec.members.iter().map(|(m, _)| m.clone()).collect(),
                    },
                );
            }
        }
        // Service availability is the WORST capability, not the mean of
        // components: a fabric is as available as the thing it can least do.
        // The worst CAPABILITY, and nothing else. Folding in the component mean
        // let a degraded non-capability component drag service uptime below
        // 100% on a day when every capability was fully available — which is
        // precisely the component-versus-service conflation this field exists
        // to end. `overall` is the fallback only when no capability was
        // measured at all.
        let service_uptime_pct = capabilities
            .values()
            .map(|c| c.sli_pct)
            .fold(f64::INFINITY, f64::min);
        let service_uptime_pct = if service_uptime_pct.is_finite() {
            service_uptime_pct
        } else {
            overall
        };

        out.push(HistoryDay {
            date,
            regions,
            services: flat,
            overall_uptime_pct: overall,
            uptime_pct: overall,
            capabilities,
            service_uptime_pct: round1(service_uptime_pct),
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

    pub(super) fn tmp_db() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("ciris-status-hist-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("status.db").to_string_lossy().into_owned()
    }

    pub(super) fn insert_region(
        conn: &Connection,
        ts: &str,
        service: &str,
        provider: &str,
        region: &str,
        status: &str,
    ) {
        conn.execute(
            "INSERT INTO status_checks (ts, service_name, provider_name, region, status, latency_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, 10)",
            rusqlite::params![ts, service, provider, region, status],
        )
        .unwrap();
    }

    pub(super) fn insert(conn: &Connection, ts: &str, service: &str, provider: &str, status: &str) {
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
        let days = query_history(&db, 2, None, &[]).unwrap();
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
        let days = query_history(&db, 2, None, &[]).unwrap();
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
        let days = query_history(&db, 365, None, &[]).unwrap();
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

#[cfg(test)]
mod event_tests {
    use super::*;

    /// Test-only: a write that is expected to succeed. Dropping the `Result`
    /// would silently ignore exactly the failure this module is about.
    fn record_events_expect(db: &Db, events: &[StatusEvent]) {
        let n = record_events(db, events).expect("write must succeed");
        assert_eq!(n, events.len());
    }

    #[test]
    fn events_round_trip_newest_first_and_prune_with_retention() {
        let path = repair_tests::tmp_db();
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        {
            let db = init(&path).unwrap();
            record_events_expect(
                &db,
                &[
                    StatusEvent {
                        ts: now.clone(),
                        component: "eu.proxy".into(),
                        from: "operational".into(),
                        to: "degraded".into(),
                    },
                    StatusEvent {
                        ts: now.clone(),
                        component: "llm.together".into(),
                        from: "operational".into(),
                        to: "degraded".into(),
                    },
                ],
            );
            // Older than retention: must not survive the next boot.
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO status_events (ts, component, from_status, to_status)
                 VALUES ('2020-01-01T00:00:00Z', 'ancient.thing', 'operational', 'outage')",
                [],
            )
            .unwrap();
        }
        let db = init(&path).unwrap();
        let got = query_events(&db, 7, 100).unwrap();
        let names: Vec<_> = got.iter().map(|e| e.component.as_str()).collect();
        assert!(names.contains(&"eu.proxy") && names.contains(&"llm.together"));
        assert!(
            !names.contains(&"ancient.thing"),
            "retention prunes the events table too"
        );
        assert!(got.iter().all(|e| e.to == "degraded"));
    }

    /// The window must be compared in the format we STORE. `datetime('now',…)`
    /// yields `2026-08-12 21:07:28` while rows read `2026-08-12T00:00:00Z`, and
    /// `T` sorts after a space — so every event on the cutoff DATE compared
    /// greater and a 1-day window returned events up to ~40 hours old.
    #[test]
    fn the_day_window_does_not_leak_older_events() {
        let path = repair_tests::tmp_db();
        let db = init(&path).unwrap();
        let old = (chrono::Utc::now() - chrono::Duration::hours(30))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let recent = (chrono::Utc::now() - chrono::Duration::hours(2))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        record_events_expect(
            &db,
            &[
                StatusEvent {
                    ts: old,
                    component: "too.old".into(),
                    from: "operational".into(),
                    to: "degraded".into(),
                },
                StatusEvent {
                    ts: recent,
                    component: "in.window".into(),
                    from: "operational".into(),
                    to: "degraded".into(),
                },
            ],
        );
        let got = query_events(&db, 1, 100).unwrap();
        let names: Vec<_> = got.iter().map(|e| e.component.as_str()).collect();
        assert_eq!(names, ["in.window"], "a 30h-old event is not within 1 day");
    }

    /// Retention must DELETE from the events table, not merely be filtered out
    /// of a query window — otherwise the table grows without bound and the
    /// documented 400-day store is a fiction.
    #[test]
    fn retention_deletes_events_from_the_table_itself() {
        let path = repair_tests::tmp_db();
        {
            let db = init(&path).unwrap();
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO status_events (ts, component, from_status, to_status)
                 VALUES ('2020-01-01T00:00:00Z', 'ancient.thing', 'operational', 'outage')",
                [],
            )
            .unwrap();
        }
        let db = init(&path).unwrap();
        let conn = db.lock().unwrap();
        let left: i64 = conn
            .query_row("SELECT COUNT(*) FROM status_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            left, 0,
            "the row must be gone from the table, not just the window"
        );
    }

    /// FSD §2.4 — the whole point of holding raw samples. Two members whose
    /// downtime does NOT overlap means the capability was never actually down,
    /// and we can say so exactly. A consumer working from daily rollups can
    /// only bound this ("at least the best member's uptime"); we measure it.
    #[test]
    fn capability_sli_measures_overlap_not_a_bound() {
        let path = repair_tests::tmp_db();
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let spec = crate::capability::CapabilitySpec {
            id: "ai_providers".into(),
            label: "AI".into(),
            members: vec![("groq".into(), false), ("together".into(), false)],
            min_available: 1,
        };
        {
            let db = init(&path).unwrap();
            let conn = db.lock().unwrap();
            // Four sample instants. Each member is down for one of them, but
            // never the same one: the capability was continuously available.
            let plan = [
                ("00:00:00", "operational", "operational"),
                ("00:01:00", "outage", "operational"),
                ("00:02:00", "operational", "outage"),
                ("00:03:00", "operational", "operational"),
            ];
            for (t, groq, together) in plan {
                let ts = format!("{today}T{t}Z");
                repair_tests::insert(&conn, &ts, "cirisproxy", "groq", groq);
                repair_tests::insert(&conn, &ts, "cirisproxy", "together", together);
            }
        }
        let db = init(&path).unwrap();
        let sli = query_capability_sli(&db, 2, &spec).unwrap();
        assert_eq!(
            sli.get(&today).copied(),
            Some(100.0),
            "each member was down 25% of the day, but never at the same time"
        );

        // Now make them fail together for one instant: exactly 75%.
        {
            let db2 = init(&path).unwrap();
            let conn = db2.lock().unwrap();
            let ts = format!("{today}T00:01:00Z");
            conn.execute(
                "UPDATE status_checks SET status='outage'
                 WHERE ts=?1 AND provider_name='together'",
                rusqlite::params![ts],
            )
            .unwrap();
        }
        let sli = query_capability_sli(&init(&path).unwrap(), 2, &spec).unwrap();
        assert_eq!(
            sli.get(&today).copied(),
            Some(75.0),
            "one instant of overlap out of four is exactly 25% unavailable"
        );
    }

    /// A threshold above 1 means "serving, but the margin is gone" is visible
    /// in the history too, not just live.
    #[test]
    fn capability_sli_honours_the_availability_threshold() {
        let path = repair_tests::tmp_db();
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let spec = crate::capability::CapabilitySpec {
            id: "ai_providers".into(),
            label: "AI".into(),
            members: vec![("groq".into(), false), ("together".into(), false)],
            min_available: 2,
        };
        {
            let db = init(&path).unwrap();
            let conn = db.lock().unwrap();
            for (t, together) in [("00:00:00", "operational"), ("00:01:00", "outage")] {
                let ts = format!("{today}T{t}Z");
                repair_tests::insert(&conn, &ts, "cirisproxy", "groq", "operational");
                repair_tests::insert(&conn, &ts, "cirisproxy", "together", together);
            }
        }
        let sli = query_capability_sli(&init(&path).unwrap(), 2, &spec).unwrap();
        assert_eq!(
            sli.get(&today).copied(),
            Some(50.0),
            "half the samples had both"
        );
    }

    /// A provider several regions report is still ONE member. Counting rows
    /// instead of distinct providers let `available` exceed the member count
    /// and satisfy a threshold that was never met.
    #[test]
    fn duplicate_reports_of_one_provider_cannot_satisfy_a_threshold() {
        let path = repair_tests::tmp_db();
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let spec = crate::capability::CapabilitySpec {
            id: "ai_providers".into(),
            label: "AI".into(),
            members: vec![("groq".into(), false), ("openrouter".into(), false)],
            min_available: 2,
        };
        {
            let db = init(&path).unwrap();
            let conn = db.lock().unwrap();
            let ts = format!("{today}T00:00:00Z");
            // groq reported healthy TWICE (once per region), openrouter down.
            repair_tests::insert(&conn, &ts, "cirisproxy", "groq", "operational");
            repair_tests::insert(&conn, &ts, "cirisproxy", "groq", "operational");
            repair_tests::insert(&conn, &ts, "cirisproxy", "openrouter", "outage");
        }
        let sli = query_capability_sli(&init(&path).unwrap(), 2, &spec).unwrap();
        assert_eq!(
            sli.get(&today).copied(),
            Some(0.0),
            "one distinct provider available, threshold is two"
        );
    }

    /// Attribution rows are extra VIEWS of one component, not extra components,
    /// so they must not enter any rollup.
    #[test]
    fn observation_rows_are_excluded_from_the_rollup() {
        let path = repair_tests::tmp_db();
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        {
            let db = init(&path).unwrap();
            let conn = db.lock().unwrap();
            let ts = format!("{today}T00:00:00Z");
            repair_tests::insert(&conn, &ts, "cirisproxy", "groq", "operational");
            // EU saw it fail; US did not. Kept for attribution, ignored by math.
            repair_tests::insert(&conn, &ts, OBSERVATION_SERVICE, "groq", "outage");
        }
        let db = init(&path).unwrap();
        let days = query_history(&db, 2, None, &[]).unwrap();
        let day = days.iter().find(|d| d.date == today).expect("today");
        assert_eq!(
            day.overall_uptime_pct, 100.0,
            "an attribution row must not count as an outage"
        );
        let spec = crate::capability::CapabilitySpec {
            id: "ai".into(),
            label: "AI".into(),
            members: vec![("groq".into(), false)],
            min_available: 1,
        };
        assert_eq!(
            query_capability_sli(&db, 2, &spec)
                .unwrap()
                .get(&today)
                .copied(),
            Some(100.0)
        );
    }

    /// The question a single-vantage monitor cannot answer: is the provider
    /// down, or is my route to it? Two vantages, one disagreeing, names which.
    #[test]
    fn vantage_disagreement_names_the_dissenting_observer() {
        let path = repair_tests::tmp_db();
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        {
            let db = init(&path).unwrap();
            let conn = db.lock().unwrap();
            // Instant 1: both vantages agree it is fine.
            let ts = format!("{today}T00:00:00Z");
            repair_tests::insert_region(
                &conn,
                &ts,
                OBSERVATION_SERVICE,
                "groq",
                "us",
                "operational",
            );
            repair_tests::insert_region(
                &conn,
                &ts,
                OBSERVATION_SERVICE,
                "groq",
                "eu",
                "operational",
            );
            // Instant 2: only EU cannot reach it — a path problem, not groq's.
            let ts = format!("{today}T00:01:00Z");
            repair_tests::insert_region(
                &conn,
                &ts,
                OBSERVATION_SERVICE,
                "groq",
                "us",
                "operational",
            );
            repair_tests::insert_region(&conn, &ts, OBSERVATION_SERVICE, "groq", "eu", "outage");
            // Instant 3: BOTH see it down — that one is groq.
            let ts = format!("{today}T00:02:00Z");
            repair_tests::insert_region(&conn, &ts, OBSERVATION_SERVICE, "groq", "us", "outage");
            repair_tests::insert_region(&conn, &ts, OBSERVATION_SERVICE, "groq", "eu", "outage");
        }
        let rows = query_vantage(&init(&path).unwrap(), 2).unwrap();
        let row = rows.iter().find(|r| r.component == "groq").expect("groq");
        assert_eq!(row.samples, 3);
        assert_eq!(
            row.disagreements, 1,
            "exactly one instant where they differed"
        );
        assert_eq!(row.dissent_by_vantage.get("eu").copied(), Some(2));
        assert_eq!(row.dissent_by_vantage.get("us").copied(), Some(1));
    }

    #[test]
    fn empty_event_list_is_a_noop() {
        let path = repair_tests::tmp_db();
        let db = init(&path).unwrap();
        assert_eq!(record_events(&db, &[]).unwrap(), 0);
        assert!(query_events(&db, 7, 100).unwrap().is_empty());
    }

    /// A failed write must REPORT failure, so the caller can hold its baseline
    /// and retry. Silently returning `()` meant the transition was lost for
    /// good: the component stays in its new state, so the next diff is empty.
    #[test]
    fn a_failed_write_is_reported_and_writes_nothing() {
        let path = repair_tests::tmp_db();
        let db = init(&path).unwrap();
        let ev = |c: &str| StatusEvent {
            ts: "2026-08-13T14:03:00Z".into(),
            component: c.into(),
            from: "operational".into(),
            to: "degraded".into(),
        };
        assert_eq!(record_events(&db, &[ev("a"), ev("b")]).unwrap(), 2);

        db.lock()
            .unwrap()
            .execute("DROP TABLE status_events", [])
            .unwrap();
        assert!(
            record_events(&db, &[ev("c"), ev("d")]).is_err(),
            "the caller must be able to tell that nothing was persisted"
        );
    }
}
