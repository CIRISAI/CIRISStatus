//! Flow A — read & aggregate signed `scores` → the public roster surface.
//!
//! Per `FSD/MONITORING_NODE_DESIGN.md` §2 (Flow A) / §3: read `capacity:*`
//! (per opted-in agent) and `system:*` (node self-reports) from the federation
//! directory, **gate by consent / access tier** (public-tier reader: surface
//! only the `public_sample` / consent projection), and project to the website
//! roster `{key_id, capacity:composite, factors?, valid_until}`.
//!
//! The public endpoints serve from an in-memory [`RosterCache`] so the
//! request path never blocks on the corpus; a background refresher (under the
//! `fabric` feature) repopulates it from the substrate read. Without `fabric`
//! the cache is simply empty (the roster endpoint returns an empty, well-formed
//! `public_sample` projection) — the default build keeps compiling and serving.
//!
//! The capacity-projection constants/helpers are consumed under `--features
//! fabric`; in the default build they are tested but otherwise unused.
#![cfg_attr(not(feature = "fabric"), allow(dead_code))]

use std::sync::{Arc, RwLock};

use crate::model::Roster;
#[cfg(feature = "fabric")]
use crate::model::RosterEntry;
#[cfg(feature = "fabric")]
use std::collections::BTreeMap;

/// Process-wide roster snapshot, swapped atomically by the refresher.
#[derive(Clone)]
pub struct RosterCache {
    inner: Arc<RwLock<Roster>>,
}

fn now_z() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

impl Default for RosterCache {
    fn default() -> Self {
        RosterCache {
            inner: Arc::new(RwLock::new(Roster {
                timestamp: now_z(),
                projection: "public_sample".into(),
                agents: Vec::new(),
            })),
        }
    }
}

impl RosterCache {
    pub fn snapshot(&self) -> Roster {
        self.inner.read().expect("roster lock").clone()
    }

    pub fn replace(&self, roster: Roster) {
        *self.inner.write().expect("roster lock") = roster;
    }
}

/// The CEG capacity dimensions Flow A reads (design §1 table).
pub const CAPACITY_PREFIX: &str = "capacity:";
pub const CAPACITY_COMPOSITE: &str = "capacity:composite";
/// The substrate self-report dimensions ciris-status READS (never emits).
/// Reserved for the §3 service-health view (system:* node self-reports folded
/// alongside Flow B) — see the integration plan.
#[allow(dead_code)]
pub const SYSTEM_PREFIX: &str = "system:";

/// Map a `capacity:*` dimension to its short factor key for the roster
/// `factors` map (`capacity:core_identity` → `core_identity`).
pub fn factor_key(dimension: &str) -> Option<&str> {
    dimension.strip_prefix(CAPACITY_PREFIX).filter(|s| *s != "composite")
}

// ─────────────────────────────────────────────────────────────────────────────
// Flow A read — REAL signed-`scores` read via the persist substrate.
// Compiled only under `--features fabric`.
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(feature = "fabric")]
pub mod read {
    use super::*;
    use anyhow::Result;

    use ciris_persist::ceg::list::federation::AttestationFilter;
    use ciris_persist::ceg::ReadEngine;
    use ciris_persist::federation::types::Attestation;
    use ciris_persist::scope::CallerScope;

    /// Read all currently-valid `capacity:*` `scores` rows, gated to the
    /// public/opted-in projection, and fold them into the roster.
    ///
    /// `reader` is the persist backend handle (`SqliteBackend`), which impls
    /// [`ReadEngine`] — `Engine` itself does not, so callers pass
    /// `engine.sqlite_backend()`.
    ///
    /// `scope` MUST be the public/consent caller scope so the substrate's §4.3
    /// cohort_scope predicate filters to what subjects opted in to surfacing.
    pub async fn build_roster<R>(reader: &R, scope: CallerScope) -> Result<Roster>
    where
        R: ReadEngine,
    {
        let engine = reader;
        let now = chrono::Utc::now();
        let filter = AttestationFilter {
            attesting_key_id: None,
            attested_key_id: None,
            attestation_type: Some(super::super::ceg::ATTESTATION_TYPE_SCORES.to_owned()),
            pqc_completed: None,
            dimension_prefixes: vec![CAPACITY_PREFIX.to_owned()],
            valid_at: Some(now), // freshness: drop expired rows
            confidence_floor: None,
            subject_key_id: None,
        };

        // Page through; the public roster is small (opted-in agents only).
        let mut cursor = None;
        let mut rows: Vec<Attestation> = Vec::new();
        loop {
            let page = engine
                .list_attestations(filter.clone(), cursor, 500, scope.clone())
                .await
                .map_err(|e| anyhow::anyhow!("list capacity:* attestations: {e}"))?;
            rows.extend(page.items);
            match page.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }

        Ok(project_rows(rows))
    }

    /// Pure projection: `capacity:*` rows → roster. Subject = `attested_key_id`
    /// (CEG §7.5: an agent may not self-emit `capacity:*`, so this is always an
    /// about-attestation). Keeps the newest composite per agent.
    pub(super) fn project_rows(rows: Vec<Attestation>) -> Roster {
        // attested_key_id → entry
        let mut by_agent: BTreeMap<String, RosterEntry> = BTreeMap::new();
        for row in rows {
            let dim = row
                .attestation_envelope
                .get("dimension")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let score = row
                .attestation_envelope
                .get("score")
                .and_then(|v| v.as_f64());
            let valid_until = row
                .attestation_envelope
                .get("valid_until")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
                .or_else(|| row.expires_at.map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string()));

            let entry = by_agent.entry(row.attested_key_id.clone()).or_insert_with(|| {
                RosterEntry {
                    key_id: row.attested_key_id.clone(),
                    capacity_composite: None,
                    factors: BTreeMap::new(),
                    valid_until: None,
                }
            });
            if dim == CAPACITY_COMPOSITE {
                entry.capacity_composite = score;
            } else if let (Some(fk), Some(s)) = (super::factor_key(&dim), score) {
                entry.factors.insert(fk.to_owned(), s);
            }
            // Track the earliest valid_until as the entry's freshness bound.
            match (&entry.valid_until, &valid_until) {
                (None, Some(_)) => entry.valid_until = valid_until,
                (Some(cur), Some(new)) if new < cur => entry.valid_until = valid_until,
                _ => {}
            }
        }

        Roster {
            timestamp: now_z(),
            projection: "public_sample".into(),
            agents: by_agent.into_values().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factor_key_strips_prefix_and_excludes_composite() {
        assert_eq!(factor_key("capacity:core_identity"), Some("core_identity"));
        assert_eq!(factor_key("capacity:integrity"), Some("integrity"));
        assert_eq!(factor_key("capacity:composite"), None);
        assert_eq!(factor_key("system:corpus_health"), None);
    }

    #[test]
    fn cache_default_is_empty_public_sample() {
        let c = RosterCache::default();
        let snap = c.snapshot();
        assert_eq!(snap.projection, "public_sample");
        assert!(snap.agents.is_empty());
    }
}
