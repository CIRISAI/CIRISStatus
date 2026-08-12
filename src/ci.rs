//! `GET /api/v1/ci` — the substrate's build health, aggregated here so a
//! microcontroller can read it in one request.
//!
//! The Galactic Unicorn renders one "centipede" per repo: the last
//! [`RUNS_PER_REPO`] GitHub Actions runs, oldest → newest. Doing that from the
//! device directly is not viable — the unauthenticated Actions API allows 60
//! requests/hour per IP (five repos per refresh burns that in minutes) and each
//! `actions/runs?per_page=10` response is ~120 KB of JSON (measured: 124,809
//! bytes for CIRISServer) — five of those would flatten a Pico's heap. So this
//! node polls GitHub on its own cadence and serves a ~600-byte projection.
//!
//! Rate discipline: every request is conditional (`If-None-Match`). GitHub does
//! not count a `304 Not Modified` against the rate limit — verified against the
//! live API: a 200 took `x-ratelimit-remaining` 60→59, and the following 304 left
//! it at 59. A quiet stack costs effectively nothing no matter how often we poll.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use reqwest::Client;
use serde::Serialize;
use serde_json::Value;

/// How many runs make up one centipede.
pub const RUNS_PER_REPO: usize = 10;

/// The substrate in dependency order — verify → persist → edge → server →
/// agent. The display renders one row per repo in this order, so the top of the
/// board reads bottom-of-the-stack first.
pub const DEFAULT_REPOS: &[&str] = &[
    "CIRISVerify",
    "CIRISPersist",
    "CIRISEdge",
    "CIRISServer",
    "CIRISAgent",
];

pub const DEFAULT_OWNER: &str = "CIRISAI";

// Run states. Deliberately five, not four: a cancelled run is not a failed one,
// and colouring it red would cry wolf on every superseded PR push.
pub const SUCCESS: &str = "success";
pub const FAILURE: &str = "failure";
pub const IN_PROGRESS: &str = "in_progress";
pub const QUEUED: &str = "queued";
pub const CANCELLED: &str = "cancelled";

/// Collapse GitHub's `(status, conclusion)` pair into one render state.
///
/// `status` ∈ queued | in_progress | completed | waiting | requested | pending;
/// `conclusion` is only meaningful once completed.
pub fn run_state(status: &str, conclusion: Option<&str>) -> &'static str {
    match status {
        "completed" => match conclusion.unwrap_or("") {
            "success" => SUCCESS,
            // Not-run outcomes. Distinct from failure on purpose.
            "cancelled" | "skipped" | "neutral" | "stale" => CANCELLED,
            // failure, timed_out, startup_failure, action_required.
            _ => FAILURE,
        },
        "in_progress" => IN_PROGRESS,
        // queued, waiting, requested, pending — anything not started yet.
        _ => QUEUED,
    }
}

/// One repo's centipede: run states **oldest → newest**, so the display can
/// draw left-to-right and the freshest run sits at the leading edge.
#[derive(Serialize, Clone, PartialEq, Debug)]
pub struct RepoRuns {
    pub repo: String,
    pub runs: Vec<&'static str>,
}

#[derive(Serialize, Clone)]
pub struct CiSnapshot {
    pub timestamp: String,
    pub repos: Vec<RepoRuns>,
}

fn now_z() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Extract the run states from an `actions/runs` body, newest-first as GitHub
/// returns them, reversed to oldest-first and capped at [`RUNS_PER_REPO`].
///
/// Tolerant by design: a missing/short `workflow_runs` yields what is there
/// rather than an error — a young repo with three runs draws three segments.
pub fn parse_runs(body: &Value) -> Vec<&'static str> {
    let arr = match body.get("workflow_runs").and_then(Value::as_array) {
        Some(a) => a,
        None => return Vec::new(),
    };
    let mut out: Vec<&'static str> = arr
        .iter()
        .take(RUNS_PER_REPO)
        .map(|r| {
            run_state(
                r.get("status").and_then(Value::as_str).unwrap_or(""),
                r.get("conclusion").and_then(Value::as_str),
            )
        })
        .collect();
    out.reverse(); // GitHub returns newest-first; the centipede crawls forward.
    out
}

enum Fetch {
    Fresh(Vec<&'static str>, Option<String>),
    NotModified,
    Failed,
}

async fn fetch_repo(
    client: &Client,
    owner: &str,
    repo: &str,
    token: Option<&str>,
    etag: Option<&str>,
) -> Fetch {
    let url = format!(
        "https://api.github.com/repos/{owner}/{repo}/actions/runs?per_page={RUNS_PER_REPO}"
    );
    let mut req = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        // GitHub rejects requests without a User-Agent.
        .header("User-Agent", "ciris-status")
        .timeout(std::time::Duration::from_secs(10));
    if let Some(t) = token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    if let Some(e) = etag {
        req = req.header("If-None-Match", e);
    }

    match req.send().await {
        Ok(resp) => {
            let code = resp.status().as_u16();
            if code == 304 {
                return Fetch::NotModified;
            }
            if code == 403 || code == 429 {
                tracing::warn!(
                    repo,
                    code,
                    "ci: rate-limited by GitHub; keeping the last snapshot"
                );
                return Fetch::Failed;
            }
            if code >= 400 {
                tracing::warn!(repo, code, "ci: GitHub returned an error");
                return Fetch::Failed;
            }
            let new_etag = resp
                .headers()
                .get("etag")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            match resp.json::<Value>().await {
                Ok(body) => Fetch::Fresh(parse_runs(&body), new_etag),
                Err(e) => {
                    tracing::warn!(repo, error = %e, "ci: unparseable body");
                    Fetch::Failed
                }
            }
        }
        Err(e) => {
            tracing::warn!(repo, error = %e, "ci: fetch failed");
            Fetch::Failed
        }
    }
}

struct Inner {
    snapshot: CiSnapshot,
    /// Per-repo `ETag`, so the next poll can be conditional.
    etags: BTreeMap<String, String>,
}

/// Process-wide CI snapshot, swapped atomically by the refresher (mirrors
/// [`crate::roster::RosterCache`]).
#[derive(Clone)]
pub struct CiCache {
    inner: Arc<RwLock<Inner>>,
}

impl Default for CiCache {
    fn default() -> Self {
        CiCache {
            inner: Arc::new(RwLock::new(Inner {
                snapshot: CiSnapshot {
                    timestamp: now_z(),
                    repos: Vec::new(),
                },
                etags: BTreeMap::new(),
            })),
        }
    }
}

impl CiCache {
    pub fn snapshot(&self) -> CiSnapshot {
        self.inner.read().expect("ci cache lock").snapshot.clone()
    }

    /// Poll every configured repo and swap in a new snapshot.
    ///
    /// A repo whose fetch fails or 304s keeps its previous row — a GitHub
    /// hiccup must not blank the board, and it must never turn a green
    /// centipede red.
    pub async fn refresh(
        &self,
        client: &Client,
        owner: &str,
        repos: &[String],
        token: Option<&str>,
    ) {
        let (prev, etags) = {
            let g = self.inner.read().expect("ci cache lock");
            (g.snapshot.repos.clone(), g.etags.clone())
        };

        let mut out: Vec<RepoRuns> = Vec::with_capacity(repos.len());
        let mut new_etags = etags.clone();
        for repo in repos {
            let previous = prev.iter().find(|r| &r.repo == repo);
            match fetch_repo(
                client,
                owner,
                repo,
                token,
                etags.get(repo).map(String::as_str),
            )
            .await
            {
                Fetch::Fresh(runs, etag) => {
                    match etag {
                        Some(e) => {
                            new_etags.insert(repo.clone(), e);
                        }
                        None => {
                            new_etags.remove(repo);
                        }
                    }
                    out.push(RepoRuns {
                        repo: repo.clone(),
                        runs,
                    });
                }
                Fetch::NotModified | Fetch::Failed => {
                    if let Some(p) = previous {
                        out.push(p.clone());
                    } else {
                        // Nothing known yet: an empty row draws as unknown, not
                        // as a wall of green.
                        out.push(RepoRuns {
                            repo: repo.clone(),
                            runs: Vec::new(),
                        });
                    }
                }
            }
        }

        let mut g = self.inner.write().expect("ci cache lock");
        g.snapshot = CiSnapshot {
            timestamp: now_z(),
            repos: out,
        };
        g.etags = new_etags;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_every_github_state_pair() {
        assert_eq!(run_state("completed", Some("success")), SUCCESS);
        assert_eq!(run_state("completed", Some("failure")), FAILURE);
        assert_eq!(run_state("completed", Some("timed_out")), FAILURE);
        assert_eq!(run_state("completed", Some("startup_failure")), FAILURE);
        assert_eq!(run_state("completed", Some("action_required")), FAILURE);
        assert_eq!(run_state("in_progress", None), IN_PROGRESS);
        assert_eq!(run_state("queued", None), QUEUED);
        assert_eq!(run_state("waiting", None), QUEUED);
        assert_eq!(run_state("requested", None), QUEUED);
    }

    /// A cancelled or skipped run is NOT a failure. Superseded PR pushes cancel
    /// runs constantly; painting those red would make the board meaningless.
    #[test]
    fn not_run_outcomes_are_distinct_from_failure() {
        for c in ["cancelled", "skipped", "neutral", "stale"] {
            assert_eq!(run_state("completed", Some(c)), CANCELLED, "{c}");
        }
    }

    /// GitHub returns newest-first; the centipede crawls oldest → newest.
    #[test]
    fn runs_are_reversed_to_oldest_first() {
        let body = json!({"workflow_runs": [
            {"status": "in_progress", "conclusion": null},
            {"status": "completed", "conclusion": "failure"},
            {"status": "completed", "conclusion": "success"},
        ]});
        assert_eq!(parse_runs(&body), vec![SUCCESS, FAILURE, IN_PROGRESS]);
    }

    #[test]
    fn caps_at_ten_and_survives_odd_payloads() {
        let runs: Vec<Value> = (0..25)
            .map(|_| json!({"status": "completed", "conclusion": "success"}))
            .collect();
        assert_eq!(
            parse_runs(&json!({ "workflow_runs": runs })).len(),
            RUNS_PER_REPO
        );

        // A young repo draws a short centipede rather than erroring.
        let short = json!({"workflow_runs": [{"status": "queued", "conclusion": null}]});
        assert_eq!(parse_runs(&short), vec![QUEUED]);

        // Junk shapes yield nothing, never a panic.
        assert!(parse_runs(&json!({})).is_empty());
        assert!(parse_runs(&json!({"workflow_runs": "nope"})).is_empty());
        assert!(parse_runs(&json!({"workflow_runs": [{}]})) == vec![QUEUED]);
    }

    #[test]
    fn cache_starts_empty_and_is_cloneable() {
        let c = CiCache::default();
        assert!(c.snapshot().repos.is_empty());
        let c2 = c.clone();
        assert!(c2.snapshot().repos.is_empty());
    }
}
