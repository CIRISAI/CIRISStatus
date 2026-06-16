//! Environment configuration — mirrors the env vars CIRISLens's status API read,
//! so this is a drop-in replacement behind the same nginx route. Every probe
//! target is optional: an unset `*_URL` simply skips that probe (the component is
//! omitted), exactly as the Python service behaved.

use std::env;

/// One regional deployment (US / EU): its public billing + proxy `/v1/status`
/// surfaces, plus the host's infrastructure health URL.
#[derive(Clone)]
pub struct Region {
    pub key: &'static str, // "us" / "eu"
    pub name: String,      // "US (Chicago)"
    pub billing_url: Option<String>,
    pub proxy_url: Option<String>,
    pub infra_url: Option<String>,    // VULTR_/HETZNER_HEALTH_URL
    pub infra_provider: &'static str, // "vultr" / "hetzner"
}

/// A directly-probed external provider (search APIs).
///
/// COST SAFETY: a health check that sends the live API key is a *billable* call
/// for some providers — Brave bills health checks (the old CIRISLens code had to
/// disable Brave for exactly this). So by default we probe **keyless**
/// (reachability only — billable APIs reject an unauthenticated request before
/// doing any billable work). The key is sent ONLY when `authenticated` is set
/// (`<PREFIX>_HEALTH_AUTH=true`), which an operator should enable *only* for a
/// provider whose health endpoint is free.
#[derive(Clone)]
pub struct ExternalProvider {
    pub key: &'static str,     // "exa" / "brave" / "serper" / "tavily"
    pub display: &'static str, // "web_search" / "brave_search" / ...
    pub url: String,
    pub api_key: Option<String>,
    pub header: &'static str, // "x-api-key" / "X-Subscription-Token" / ...
    pub expected_text: Option<&'static str>, // e.g. exa expects "healthy"
    pub authenticated: bool,  // send the key (billable!) — opt-in per provider
}

#[derive(Clone)]
pub struct Config {
    pub listen_addr: String,
    pub db_path: String,
    pub poll_seconds: u64,
    pub version: &'static str,
    pub grafana_url: Option<String>,
    pub database_url: Option<String>, // local "postgresql" provider (TCP liveness)
    pub ghcr_url: String,
    pub regions: Vec<Region>,
    pub external: Vec<ExternalProvider>,
    pub cors_origins: Vec<String>,
}

fn opt(name: &str) -> Option<String> {
    env::var(name).ok().filter(|v| !v.trim().is_empty())
}

impl Config {
    pub fn from_env() -> Self {
        let regions = vec![
            Region {
                key: "us",
                name: env::var("US_REGION_NAME").unwrap_or_else(|_| "US (Chicago)".into()),
                billing_url: opt("US_BILLING_URL"),
                proxy_url: opt("US_PROXY_URL"),
                infra_url: opt("VULTR_HEALTH_URL"),
                infra_provider: "vultr",
            },
            Region {
                key: "eu",
                name: env::var("EU_REGION_NAME").unwrap_or_else(|_| "EU (Germany)".into()),
                billing_url: opt("EU_BILLING_URL"),
                proxy_url: opt("EU_PROXY_URL"),
                infra_url: opt("HETZNER_HEALTH_URL"),
                infra_provider: "hetzner",
            },
        ];

        // External search providers — only probed when their *_HEALTH_URL is set.
        let mut external = Vec::new();
        let ext_specs: &[(&str, &str, &str, &str, Option<&str>)] = &[
            ("exa", "web_search", "EXA", "x-api-key", Some("healthy")),
            (
                "brave",
                "brave_search",
                "BRAVE",
                "X-Subscription-Token",
                None,
            ),
            ("serper", "serper_search", "SERPER", "X-API-KEY", None),
            ("tavily", "tavily_search", "TAVILY", "x-api-key", None),
        ];
        for (key, display, env_prefix, header, expected) in ext_specs {
            if let Some(url) = opt(&format!("{env_prefix}_HEALTH_URL")) {
                // Authenticated (key-sending, possibly BILLABLE) probing is strictly
                // opt-in per provider. Default keyless so we never repeat the Brave
                // health-check-charges incident.
                let authenticated = opt(&format!("{env_prefix}_HEALTH_AUTH"))
                    .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
                    .unwrap_or(false);
                external.push(ExternalProvider {
                    key,
                    display,
                    url,
                    api_key: opt(&format!("{env_prefix}_API_KEY")),
                    header,
                    expected_text: *expected,
                    authenticated,
                });
            }
        }

        Config {
            listen_addr: env::var("STATUS_LISTEN_ADDR").unwrap_or_else(|_| "127.0.0.1:8200".into()),
            db_path: env::var("STATUS_DB_PATH").unwrap_or_else(|_| "status.db".into()),
            poll_seconds: opt("STATUS_POLL_SECONDS")
                .and_then(|v| v.parse().ok())
                .unwrap_or(60),
            version: env!("CARGO_PKG_VERSION"),
            grafana_url: opt("GRAFANA_URL"),
            database_url: opt("DATABASE_URL"),
            ghcr_url: env::var("GHCR_HEALTH_URL").unwrap_or_else(|_| "https://ghcr.io/v2/".into()),
            regions,
            external,
            cors_origins: vec![
                "https://ciris.ai".into(),
                "https://www.ciris.ai".into(),
                "https://agents.ciris.ai".into(),
                "http://localhost:3000".into(),
                "http://localhost:8080".into(),
            ],
        }
    }
}
