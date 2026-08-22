//! Outbound health probes — the live signals behind every status field. Mirrors
//! CIRISLens's `check_infrastructure` / `check_external_provider` /
//! `fetch_service_status` / `check_grafana` semantics (timeouts, latency
//! thresholds, the operational/degraded/outage decision, error scrubbing).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use reqwest::Client;
use serde_json::Value;

use crate::model::{DEGRADED, OPERATIONAL, OUTAGE};

/// One probe outcome (component-level).
#[derive(Clone)]
pub struct Probe {
    pub status: &'static str,
    pub latency_ms: Option<i64>,
    pub message: Option<String>,
    /// The request never got an answer — connection refused, DNS, timeout. An
    /// HTTP status, however bad, means the network worked. Distinguishing these
    /// is what lets the poller tell "the world is down" from "I lost my network"
    /// (FSD §3.3).
    pub transport_error: bool,
    /// What a service said about ITSELF, when it says anything.
    pub upstream_status: Option<String>,
}

/// Fewer probes than this and a total failure is indistinguishable from a
/// genuine multi-target outage, so no vantage verdict is claimed.
pub const MIN_FOR_VERDICT: usize = 3;

/// Latency verdict, judged on EXCESS over the path's own floor.
pub fn latency_status(latency_ms: i64, threshold_ms: i64, baseline_ms: i64) -> &'static str {
    if (latency_ms - baseline_ms).max(0) < threshold_ms {
        OPERATIONAL
    } else {
        DEGRADED
    }
}

/// Did every probe this cycle fail at the transport layer? Then the fault is
/// almost certainly ours: unrelated third parties on three continents do not
/// fail in the same second. Below [`MIN_FOR_VERDICT`] probes the two cases are
/// indistinguishable and no verdict is claimed.
pub fn is_vantage_failure(attempted: usize, transport_failures: usize) -> bool {
    attempted >= MIN_FOR_VERDICT && transport_failures == attempted
}

impl Probe {
    /// `latency` is judged on its EXCESS over the path's own floor: a
    /// transatlantic probe carries ~450-520ms of physics before anything is
    /// wrong, and judging it against a US-local constant makes EU structurally
    /// closer to `degraded` for identical health (FSD §3.4 / D4).
    fn ok(latency: i64, threshold: i64, baseline: i64) -> Self {
        Probe {
            status: latency_status(latency, threshold, baseline),
            latency_ms: Some(latency),
            message: None,
            transport_error: false,
            upstream_status: None,
        }
    }
    fn down(msg: impl Into<String>) -> Self {
        Probe {
            status: OUTAGE,
            latency_ms: None,
            message: Some(msg.into()),
            transport_error: true,
            upstream_status: None,
        }
    }
    fn degraded(latency: i64, msg: impl Into<String>) -> Self {
        Probe {
            status: DEGRADED,
            latency_ms: Some(latency),
            message: Some(msg.into()),
            transport_error: false,
            upstream_status: None,
        }
    }
}

fn scrub(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        "Timeout"
    } else {
        "Connection failed"
    }
}

/// Generic HTTP probe: GET `url`, optional headers, optional body-substring
/// assertion. `< threshold_ms` → operational, else degraded; non-OK code →
/// degraded `HTTP <code>`; transport error → outage.
#[allow(clippy::too_many_arguments)]
pub async fn check_http(
    client: &Client,
    url: &str,
    timeout: Duration,
    threshold_ms: i64,
    accept_401: bool,
    headers: &[(&str, String)],
    expected_text: Option<&str>,
    baseline_ms: i64,
) -> Probe {
    let start = Instant::now();
    let mut req = client.get(url).timeout(timeout);
    for (k, v) in headers {
        req = req.header(*k, v);
    }
    match req.send().await {
        Ok(resp) => {
            let code = resp.status().as_u16();
            let ok_code = code < 400 || (accept_401 && code == 401);
            let body_ok = if ok_code {
                match expected_text {
                    Some(t) => resp.text().await.map(|b| b.contains(t)).unwrap_or(false),
                    None => true,
                }
            } else {
                false
            };
            let latency = start.elapsed().as_millis() as i64;
            if ok_code && body_ok {
                Probe::ok(latency, threshold_ms, baseline_ms)
            } else if ok_code {
                Probe::degraded(latency, "unexpected body")
            } else {
                Probe::degraded(latency, format!("HTTP {code}"))
            }
        }
        Err(e) => Probe::down(scrub(&e)),
    }
}

/// Grafana `/api/health` (threshold 1s).
pub async fn check_grafana(client: &Client, base: &str) -> Probe {
    let url = format!("{}/api/health", base.trim_end_matches('/'));
    check_http(
        client,
        &url,
        Duration::from_secs(5),
        1000,
        false,
        &[],
        None,
        0,
    )
    .await
}

/// Infrastructure host health (Vultr/Hetzner/GHCR). GHCR uses threshold 3s +
/// `accept_401` (its `/v2/` returns 401 unauthenticated but is "up").
pub async fn check_infrastructure(
    client: &Client,
    url: &str,
    threshold_ms: i64,
    accept_401: bool,
    baseline_ms: i64,
) -> Probe {
    check_http(
        client,
        url,
        Duration::from_secs(5),
        threshold_ms,
        accept_401,
        &[],
        None,
        baseline_ms,
    )
    .await
}

/// Directly-probed external provider (search APIs): 10s timeout, threshold 2s.
///
/// COST SAFETY: when `authenticated` is false (the default) we probe **keyless**
/// — no API key is sent, so no billable call is made (billable APIs reject the
/// unauthenticated request before doing any work). That is reachability-only: any
/// HTTP response (incl. 401/403/429) means the provider is *up*. The live key is
/// sent ONLY when `authenticated` is true (operator opt-in for a free health
/// endpoint), in which case the body-text assertion also applies.
pub async fn check_external_provider(
    client: &Client,
    url: &str,
    header: &str,
    api_key: Option<&str>,
    expected_text: Option<&str>,
    authenticated: bool,
) -> Probe {
    if authenticated {
        if let Some(k) = api_key {
            let headers = [(header, k.to_string())];
            return check_http(
                client,
                url,
                Duration::from_secs(10),
                2000,
                false,
                &headers,
                expected_text,
                0,
            )
            .await;
        }
    }
    check_reachable(client, url, Duration::from_secs(10), 2000).await
}

/// Keyless reachability: ANY HTTP response < 500 → up (operational by latency);
/// 5xx → degraded; transport error → outage. No headers, no body read, no charge.
pub async fn check_reachable(
    client: &Client,
    url: &str,
    timeout: Duration,
    threshold_ms: i64,
) -> Probe {
    let start = Instant::now();
    match client.get(url).timeout(timeout).send().await {
        Ok(resp) => {
            let code = resp.status().as_u16();
            let latency = start.elapsed().as_millis() as i64;
            if code < 500 {
                Probe::ok(latency, threshold_ms, 0)
            } else {
                Probe::degraded(latency, format!("HTTP {code}"))
            }
        }
        Err(e) => Probe::down(scrub(&e)),
    }
}

/// Fetch a regional service's own `/v1/status`. Returns the derived component
/// status plus the parsed body (for upstream provider categorization). The
/// component status prefers the upstream's self-reported `status` on 200.
pub async fn fetch_service_status(
    client: &Client,
    base: &str,
    baseline_ms: i64,
) -> (Probe, Option<Value>) {
    let url = format!("{}/v1/status", base.trim_end_matches('/'));
    let start = Instant::now();
    match client
        .get(&url)
        .timeout(Duration::from_secs(5))
        .send()
        .await
    {
        Ok(resp) => {
            let code = resp.status().as_u16();
            let latency = start.elapsed().as_millis() as i64;
            if code == 200 {
                let body = resp.json::<Value>().await.ok();
                let upstream = body
                    .as_ref()
                    .and_then(|b| b.get("status"))
                    .and_then(Value::as_str);
                // The upstream's own verdict is KEPT, not adopted: it folds
                // pooled providers into it, we do not (FSD §3.1). Our verdict
                // starts from transport health.
                let upstream_status = upstream.map(str::to_string);
                let mut probe = Probe::ok(latency, 1000, baseline_ms);
                if matches!(upstream, Some(DEGRADED) | Some(OUTAGE)) {
                    probe.message = Some(format!("upstream: {}", upstream.unwrap_or("")));
                }
                probe.upstream_status = upstream_status;
                (probe, body)
            } else {
                (Probe::degraded(latency, format!("HTTP {code}")), None)
            }
        }
        Err(e) => (Probe::down(scrub(&e)), None),
    }
}

/// Local "postgresql" provider — a TCP-connect liveness probe parsed from a
/// `postgres://…` DSN (avoids a full SQL client; sufficient for a status page).
pub async fn check_postgres_tcp(database_url: &str) -> Probe {
    let (host, port) = parse_pg_host_port(database_url);
    let start = Instant::now();
    match tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::TcpStream::connect((host.as_str(), port)),
    )
    .await
    {
        Ok(Ok(_)) => Probe::ok(start.elapsed().as_millis() as i64, 1000, 0),
        Ok(Err(_)) => Probe::down("Connection failed"),
        Err(_) => Probe {
            status: OUTAGE,
            latency_ms: Some(5000),
            message: Some("Timeout".into()),
            transport_error: true,
            upstream_status: None,
        },
    }
}

/// Extract (host, port) from a `postgres[ql]://user:pass@host:port/db` DSN.
fn parse_pg_host_port(dsn: &str) -> (String, u16) {
    let after_scheme = dsn.split("://").nth(1).unwrap_or(dsn);
    let authority = after_scheme.split('/').next().unwrap_or(after_scheme);
    let hostport = authority.rsplit('@').next().unwrap_or(authority);
    // Strip any IPv6 brackets minimally; take host:port on the last colon.
    if let Some((h, p)) = hostport.rsplit_once(':') {
        if let Ok(port) = p.parse::<u16>() {
            return (h.to_string(), port);
        }
    }
    (hostport.to_string(), 5432)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D4: a transatlantic path carries ~500ms of physics. Judged against a
    /// US-local constant, EU is structurally closer to `degraded` for identical
    /// health; judged on excess over its own floor, identical health reads
    /// identically.
    #[test]
    fn latency_is_judged_on_excess_over_the_paths_own_floor() {
        // 100ms of excess, on a local path and a transatlantic one.
        assert_eq!(latency_status(100, 1000, 0), OPERATIONAL);
        assert_eq!(latency_status(600, 1000, 500), OPERATIONAL);
        // The same 600ms WITHOUT a baseline is still fine here...
        assert_eq!(latency_status(600, 1000, 0), OPERATIONAL);
        // ...but 1200ms is not, unless 500 of it is the path's floor.
        assert_eq!(latency_status(1200, 1000, 0), DEGRADED);
        assert_eq!(latency_status(1200, 1000, 500), OPERATIONAL);
        // A baseline can never make a probe look better than instant.
        assert_eq!(latency_status(200, 1000, 5000), OPERATIONAL);
    }

    /// D3: a monitor must not report its own network failure as the world's.
    #[test]
    fn a_total_transport_failure_indicts_our_own_vantage() {
        assert!(is_vantage_failure(6, 6), "everything failed to connect");
        assert!(
            !is_vantage_failure(6, 5),
            "one target answered — the net works"
        );
        // Too few probes to tell a local failure from a genuine dual outage.
        assert!(!is_vantage_failure(2, 2));
        assert!(!is_vantage_failure(0, 0));
    }

    #[test]
    fn parses_pg_dsn() {
        assert_eq!(
            parse_pg_host_port("postgres://u:p@db.example:5433/x"),
            ("db.example".to_string(), 5433)
        );
        assert_eq!(
            parse_pg_host_port("postgresql://host/db"),
            ("host".to_string(), 5432)
        );
        assert_eq!(
            parse_pg_host_port("postgres://u:p@h:5432"),
            ("h".to_string(), 5432)
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// One cycle's fetches, memoised.
// ─────────────────────────────────────────────────────────────────────────────

/// Every probe made during ONE poll cycle, cached by target.
///
/// # Why
///
/// The loop ran two independent sweeps per cycle — `history::poll_once` to
/// record, then `aggregate::aggregated_status` to serve — each opening its own
/// TLS connection to the same billing, proxy, infra, identity and search
/// endpoints. Every cycle asked the world the same question twice
/// (CIRISStatus#47).
///
/// That is wasteful anywhere and load-bearing here: the two sweeps run in
/// series, so the cycle takes twice as long as it needs to, and on the US node
/// that pushed snapshot age past the staleness ceiling — `age_s=190` against
/// `max_age_s=180` — which makes `/api/v1/status` serve `unknown` while the
/// probes themselves were succeeding.
///
/// A cache rather than a restructure: the recording sweep and the serving sweep
/// keep their own shapes and their own fidelity (history records per-region
/// provider views the served snapshot deliberately merges), they just stop
/// asking twice. It also makes the two sweeps agree BY CONSTRUCTION — they now
/// read one measurement, so "what we serve is what we recorded" stops depending
/// on two probes landing on the same side of a threshold.
///
/// Scope is one cycle. Never share it across requests: a cached probe served to
/// a later caller is a stale claim with a fresh timestamp.
#[derive(Clone)]
pub struct Cycle {
    client: Client,
    seen: std::sync::Arc<tokio::sync::Mutex<HashMap<String, Cached>>>,
    /// Requests that actually went out. The saving this type exists for is
    /// invisible from the outside — a cached probe and a fresh one are the same
    /// value — so the test asserts on this rather than on wall-clock timing.
    fetches: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[derive(Clone)]
enum Cached {
    Simple(Probe),
    Service(Probe, Option<Value>),
}

impl Cycle {
    pub fn new(client: &Client) -> Self {
        Cycle {
            client: client.clone(),
            seen: std::sync::Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            fetches: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// How many requests this cycle actually made.
    pub fn fetches(&self) -> usize {
        self.fetches.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn count_fetch(&self) {
        self.fetches
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    async fn simple<F, Fut>(&self, key: String, f: F) -> Probe
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Probe>,
    {
        if let Some(Cached::Simple(p)) = self.seen.lock().await.get(&key) {
            return p.clone();
        }
        self.count_fetch();
        let p = f().await;
        self.seen
            .lock()
            .await
            .insert(key, Cached::Simple(p.clone()));
        p
    }

    pub async fn service_status(&self, base: &str, baseline_ms: i64) -> (Probe, Option<Value>) {
        let key = format!("service:{base}");
        if let Some(Cached::Service(p, v)) = self.seen.lock().await.get(&key) {
            return (p.clone(), v.clone());
        }
        self.count_fetch();
        let (p, v) = fetch_service_status(&self.client, base, baseline_ms).await;
        self.seen
            .lock()
            .await
            .insert(key, Cached::Service(p.clone(), v.clone()));
        (p, v)
    }

    pub async fn reachable(&self, url: &str, timeout: Duration, threshold_ms: i64) -> Probe {
        self.simple(format!("reach:{url}"), || {
            check_reachable(&self.client, url, timeout, threshold_ms)
        })
        .await
    }

    pub async fn infrastructure(
        &self,
        url: &str,
        threshold_ms: i64,
        allow_401: bool,
        baseline_ms: i64,
    ) -> Probe {
        self.simple(format!("infra:{url}"), || {
            check_infrastructure(&self.client, url, threshold_ms, allow_401, baseline_ms)
        })
        .await
    }

    pub async fn grafana(&self, base: &str) -> Probe {
        self.simple(format!("grafana:{base}"), || {
            check_grafana(&self.client, base)
        })
        .await
    }

    pub async fn external_provider(
        &self,
        url: &str,
        header: &str,
        api_key: Option<&str>,
        expected_text: Option<&str>,
        authenticated: bool,
    ) -> Probe {
        self.simple(format!("ext:{url}"), || {
            check_external_provider(
                &self.client,
                url,
                header,
                api_key,
                expected_text,
                authenticated,
            )
        })
        .await
    }

    pub async fn postgres_tcp(&self, dsn: &str) -> Probe {
        self.simple(format!("pg:{dsn}"), || check_postgres_tcp(dsn))
            .await
    }
}

#[cfg(test)]
mod cycle_tests {
    use super::*;

    /// The saving is invisible in the return value — a cached probe and a fresh
    /// one are the same `Probe` — so assert on the request count.
    #[tokio::test]
    async fn one_target_is_fetched_once_per_cycle() {
        let client = Client::builder()
            .timeout(Duration::from_millis(300))
            .build()
            .expect("client");
        let cycle = Cycle::new(&client);
        // Unroutable on purpose: this test is about how many requests leave,
        // not about what comes back. TEST-NET-1 (RFC 5737) is guaranteed
        // non-routable, so the result is a transport error either way.
        let url = "http://192.0.2.1:9/health";

        let first = cycle.reachable(url, Duration::from_millis(200), 1000).await;
        let second = cycle.reachable(url, Duration::from_millis(200), 1000).await;

        assert_eq!(
            cycle.fetches(),
            1,
            "the recorder and the server share one probe"
        );
        assert_eq!(first.status, second.status);
        assert_eq!(first.transport_error, second.transport_error);
    }

    #[tokio::test]
    async fn different_targets_are_not_conflated() {
        let client = Client::builder()
            .timeout(Duration::from_millis(300))
            .build()
            .expect("client");
        let cycle = Cycle::new(&client);
        cycle
            .reachable("http://192.0.2.1:9/a", Duration::from_millis(200), 1000)
            .await;
        cycle
            .reachable("http://192.0.2.2:9/b", Duration::from_millis(200), 1000)
            .await;
        assert_eq!(cycle.fetches(), 2);
    }

    /// Kinds share a URL space; a reachability probe must not satisfy an
    /// infrastructure probe of the same endpoint (different thresholds, and
    /// `allow_401` changes what counts as up).
    #[tokio::test]
    async fn probe_kinds_do_not_share_a_cache_slot() {
        let client = Client::builder()
            .timeout(Duration::from_millis(300))
            .build()
            .expect("client");
        let cycle = Cycle::new(&client);
        let url = "http://192.0.2.1:9/same";
        cycle.reachable(url, Duration::from_millis(200), 1000).await;
        cycle.infrastructure(url, 1000, true, 0).await;
        assert_eq!(cycle.fetches(), 2);
    }
}
