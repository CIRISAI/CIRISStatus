//! Outbound health probes — the live signals behind every status field. Mirrors
//! CIRISLens's `check_infrastructure` / `check_external_provider` /
//! `fetch_service_status` / `check_grafana` semantics (timeouts, latency
//! thresholds, the operational/degraded/outage decision, error scrubbing).

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
}

impl Probe {
    fn ok(latency: i64, threshold: i64) -> Self {
        Probe {
            status: if latency < threshold {
                OPERATIONAL
            } else {
                DEGRADED
            },
            latency_ms: Some(latency),
            message: None,
        }
    }
    fn down(msg: impl Into<String>) -> Self {
        Probe {
            status: OUTAGE,
            latency_ms: None,
            message: Some(msg.into()),
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
pub async fn check_http(
    client: &Client,
    url: &str,
    timeout: Duration,
    threshold_ms: i64,
    accept_401: bool,
    headers: &[(&str, String)],
    expected_text: Option<&str>,
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
                Probe::ok(latency, threshold_ms)
            } else if ok_code {
                Probe {
                    status: DEGRADED,
                    latency_ms: Some(latency),
                    message: Some("unexpected body".into()),
                }
            } else {
                Probe {
                    status: DEGRADED,
                    latency_ms: Some(latency),
                    message: Some(format!("HTTP {code}")),
                }
            }
        }
        Err(e) => Probe::down(scrub(&e)),
    }
}

/// Grafana `/api/health` (threshold 1s).
pub async fn check_grafana(client: &Client, base: &str) -> Probe {
    let url = format!("{}/api/health", base.trim_end_matches('/'));
    check_http(client, &url, Duration::from_secs(5), 1000, false, &[], None).await
}

/// Infrastructure host health (Vultr/Hetzner/GHCR). GHCR uses threshold 3s +
/// `accept_401` (its `/v2/` returns 401 unauthenticated but is "up").
pub async fn check_infrastructure(
    client: &Client,
    url: &str,
    threshold_ms: i64,
    accept_401: bool,
) -> Probe {
    check_http(
        client,
        url,
        Duration::from_secs(5),
        threshold_ms,
        accept_401,
        &[],
        None,
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
                Probe::ok(latency, threshold_ms)
            } else {
                Probe {
                    status: DEGRADED,
                    latency_ms: Some(latency),
                    message: Some(format!("HTTP {code}")),
                }
            }
        }
        Err(e) => Probe::down(scrub(&e)),
    }
}

/// Fetch a regional service's own `/v1/status`. Returns the derived component
/// status plus the parsed body (for upstream provider categorization). The
/// component status prefers the upstream's self-reported `status` on 200.
pub async fn fetch_service_status(client: &Client, base: &str) -> (Probe, Option<Value>) {
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
                let status = match upstream {
                    Some(OPERATIONAL) => OPERATIONAL,
                    Some(DEGRADED) => DEGRADED,
                    Some(OUTAGE) => OUTAGE,
                    _ => OPERATIONAL,
                };
                (
                    Probe {
                        status,
                        latency_ms: Some(latency),
                        message: None,
                    },
                    body,
                )
            } else {
                (
                    Probe {
                        status: DEGRADED,
                        latency_ms: Some(latency),
                        message: Some(format!("HTTP {code}")),
                    },
                    None,
                )
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
        Ok(Ok(_)) => Probe::ok(start.elapsed().as_millis() as i64, 1000),
        Ok(Err(_)) => Probe::down("Connection failed"),
        Err(_) => Probe {
            status: OUTAGE,
            latency_ms: Some(5000),
            message: Some("Timeout".into()),
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
