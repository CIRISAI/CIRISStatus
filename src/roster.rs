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

/// Strip a trailing `:vN` version segment from a CEG dimension.
///
/// Persist's `DimensionAdmissionPolicy { require_version_segment: true }`
/// (admission.rs §T3) admits *only* `scores` dimensions carrying a `:v[0-9]+`
/// segment — so the rows actually written to the corpus are
/// `capacity:composite:v1`, `capacity:core_identity:v1`, … The roster
/// projection must compare against the *unversioned* leaf, so we canonicalize
/// by dropping that trailing segment. (Mirrors persist's own
/// `contains_version_segment` shape: the version is the last `:`-segment.)
pub fn strip_version(dimension: &str) -> &str {
    match dimension.rsplit_once(':') {
        Some((head, tail))
            if tail.len() >= 2
                && tail.as_bytes()[0] == b'v'
                && tail[1..].bytes().all(|b| b.is_ascii_digit()) =>
        {
            head
        }
        _ => dimension,
    }
}

/// Is this dimension the composite (`capacity:composite`, version-insensitive)?
pub fn is_composite(dimension: &str) -> bool {
    strip_version(dimension) == CAPACITY_COMPOSITE
}

/// Map a `capacity:*` dimension to its short factor key for the roster
/// `factors` map (`capacity:core_identity` / `capacity:core_identity:v1` →
/// `core_identity`). Returns `None` for the composite (it has its own field).
pub fn factor_key(dimension: &str) -> Option<&str> {
    strip_version(dimension)
        .strip_prefix(CAPACITY_PREFIX)
        .filter(|s| *s != "composite")
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
                .or_else(|| {
                    row.expires_at
                        .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
                });

            let entry = by_agent
                .entry(row.attested_key_id.clone())
                .or_insert_with(|| RosterEntry {
                    key_id: row.attested_key_id.clone(),
                    capacity_composite: None,
                    factors: BTreeMap::new(),
                    valid_until: None,
                });
            if super::is_composite(&dim) {
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
    fn version_segment_is_stripped_for_projection() {
        // Persist admits only versioned `scores` dims (require_version_segment),
        // so the corpus rows are `capacity:composite:v1` etc. The projection
        // must canonicalize to the unversioned leaf.
        assert_eq!(strip_version("capacity:composite:v1"), "capacity:composite");
        assert_eq!(
            strip_version("capacity:core_identity:v2"),
            "capacity:core_identity"
        );
        assert_eq!(strip_version("capacity:composite"), "capacity:composite");
        assert!(is_composite("capacity:composite:v1"));
        assert!(is_composite("capacity:composite"));
        assert!(!is_composite("capacity:integrity:v1"));
        assert_eq!(
            factor_key("capacity:core_identity:v1"),
            Some("core_identity")
        );
        assert_eq!(factor_key("capacity:composite:v1"), None);
    }

    #[test]
    fn cache_default_is_empty_public_sample() {
        let c = RosterCache::default();
        let snap = c.snapshot();
        assert_eq!(snap.projection, "public_sample");
        assert!(snap.agents.is_empty());
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Flow A — REAL signed-`scores` read proof (the load-bearing test).
//
// Seeds a ciris-server SQLite corpus with `capacity:*:v1` `scores` rows for a
// couple of opted-in agents (using the persist v8.4.0 federation-directory API —
// `put_public_key` to register the keys, `put_attestation` to write the rows,
// mirroring CIRISServer's `tests/replication.rs` seed pattern), then asserts
// `read::build_roster(reader, CallerScope::Unauthenticated)` returns the expected
// roster. This proves Flow A serves REAL substrate data — not the empty default
// cache — and that the public/opted-in projection (`cohort_scope: federation`,
// the §8.1.13.3 broad tier the Unauthenticated reader admits) is what surfaces.
//
// Versioned dimensions (`capacity:composite:v1`) are used because persist's
// `DimensionAdmissionPolicy { require_version_segment: true }` admits ONLY
// versioned `scores` dims — i.e. these are the rows that actually land in a real
// corpus, so the projection's version-stripping is exercised end-to-end.
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(all(test, feature = "fabric"))]
mod flow_a_real_data {
    use super::read::build_roster;

    use chrono::Utc;
    use ciris_persist::federation::types::{
        algorithm, attestation_tier, cohort_scope, identity_type, Attestation, KeyRecord,
        SignedAttestation, SignedKeyRecord,
    };
    use ciris_persist::federation::FederationDirectory;
    use ciris_persist::prelude::{Engine, LocalSigner, LocalSignerConfig};
    use ciris_persist::scope::CallerScope;

    /// Minimal temp-dir for 32-byte raw seed files (avoids a `tempfile` dep).
    /// Cleaned up on drop.
    pub(super) mod tempdir {
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicU64, Ordering};

        pub struct SeedDir {
            dir: PathBuf,
        }
        impl SeedDir {
            pub fn new() -> Self {
                static CTR: AtomicU64 = AtomicU64::new(0);
                let n = CTR.fetch_add(1, Ordering::Relaxed);
                let pid = std::process::id();
                let dir = std::env::temp_dir().join(format!("ciris-status-seed-{pid}-{n}"));
                std::fs::create_dir_all(&dir).expect("create seed dir");
                SeedDir { dir }
            }
            pub fn write_seed(&self, name: &str, bytes: [u8; 32]) -> PathBuf {
                let p = self.dir.join(name);
                std::fs::write(&p, bytes).expect("write seed file");
                p
            }
        }
        impl Drop for SeedDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }
    }

    /// Stand up an in-memory fabric node keyed by its own node identity. The
    /// Ed25519 seed is written to a temp file and loaded via the same
    /// `LocalSigner::from_config` path production uses (no PQC needed for the
    /// Flow A READ path — `put_attestation` takes the deferred-scrub federation
    /// row we hand-build, exactly as `ceg::emit` assembles it).
    async fn node() -> (std::sync::Arc<Engine>, tempdir::SeedDir) {
        let seeds = tempdir::SeedDir::new();
        let ed = seeds.write_seed("node.ed25519", [0x5A; 32]);
        let signer = std::sync::Arc::new(
            LocalSigner::from_config(&LocalSignerConfig {
                key_id: "ciris-status-detector".to_string(),
                key_path: ed,
                pqc_key_id: None,
                pqc_key_path: None,
            })
            .expect("LocalSigner::from_config"),
        );
        let engine = std::sync::Arc::new(
            Engine::with_signer(signer, "sqlite::memory:")
                .await
                .expect("Engine::with_signer (sqlite::memory:) must succeed"),
        );
        (engine, seeds)
    }

    /// Register a key in `federation_keys` so `put_attestation`'s FK + identity
    /// lookup resolves it (mirrors `replication.rs::cross_register`).
    async fn register_key<B: FederationDirectory>(dir: &B, key_id: &str, id_type: &str) {
        let now = Utc::now();
        let rec = KeyRecord {
            key_id: key_id.to_string(),
            pubkey_ed25519_base64: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            pubkey_ml_dsa_65_base64: None,
            algorithm: algorithm::HYBRID.into(),
            identity_type: id_type.to_string(),
            identity_ref: key_id.to_string(),
            valid_from: now,
            valid_until: None,
            registration_envelope: serde_json::json!({ "key_id": key_id }),
            original_content_hash: "deadbeef".into(),
            scrub_signature_classical: "AAAA".into(),
            scrub_signature_pqc: None,
            scrub_key_id: key_id.to_string(),
            scrub_timestamp: now,
            pqc_completed_at: None,
            persist_row_hash: String::new(),
            roles: Vec::new(),
            attestation_evidence: None,
        };
        dir.put_public_key(SignedKeyRecord { record: rec })
            .await
            .expect("register key");
    }

    /// Write one `capacity:<leaf>:v1` `scores` row ABOUT `subject` BY `detector`,
    /// at the public broad tier (`cohort_scope: federation`, visible to the
    /// Unauthenticated reader). Federation-tier so the read-gate admits it.
    async fn seed_capacity<B: FederationDirectory>(
        dir: &B,
        detector: &str,
        subject: &str,
        leaf: &str,
        score: f64,
        valid_until: chrono::DateTime<Utc>,
    ) {
        let dim = format!("capacity:{leaf}:v1");
        let now = Utc::now();
        let envelope = serde_json::json!({
            "dimension": dim,
            "score": score,
            "valid_until": valid_until.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            "witness_relation": "external",
        });
        let att = Attestation {
            attestation_id: format!(
                "{subject}-{leaf}-{}",
                now.timestamp_nanos_opt().unwrap_or(0)
            ),
            attesting_key_id: detector.to_string(),
            attested_key_id: subject.to_string(),
            attestation_type: super::super::ceg::ATTESTATION_TYPE_SCORES.to_owned(),
            weight: Some(0.95),
            asserted_at: now,
            expires_at: Some(valid_until),
            attestation_envelope: envelope,
            original_content_hash: "00".repeat(32),
            scrub_signature_classical: "AAAA".into(),
            scrub_signature_pqc: None,
            scrub_key_id: detector.to_string(),
            scrub_timestamp: now,
            pqc_completed_at: None,
            persist_row_hash: String::new(),
            subject_key_ids: vec![subject.to_string()],
            withdraws_admission_rule: None,
            cohort_scope: cohort_scope::FEDERATION.to_owned(),
            tier: attestation_tier::FEDERATION.to_owned(),
            promoted_at: None,
        };
        dir.put_attestation(SignedAttestation { attestation: att })
            .await
            .expect("seed capacity:* row");
    }

    #[tokio::test]
    async fn build_roster_serves_real_capacity_scores() {
        let (engine, _seeds) = node().await;
        let dir = engine.sqlite_backend().expect("sqlite backend");

        // The about-attester (a `lenscore_detector` in prod; an `agent`-typed
        // key here — `capacity:` is NOT a reserved prefix, so any registered
        // key may emit it. §7.5 anti-self-emit is a subject≠attester rule,
        // satisfied because we attest ABOUT other keys).
        register_key(dir.as_ref(), "lenscore-detector", identity_type::AGENT).await;
        register_key(dir.as_ref(), "agent-alpha", identity_type::AGENT).await;
        register_key(dir.as_ref(), "agent-bravo", identity_type::AGENT).await;

        let valid_until = Utc::now() + chrono::Duration::hours(24);

        // Agent alpha: composite + two factors.
        seed_capacity(
            dir.as_ref(),
            "lenscore-detector",
            "agent-alpha",
            "composite",
            0.87,
            valid_until,
        )
        .await;
        seed_capacity(
            dir.as_ref(),
            "lenscore-detector",
            "agent-alpha",
            "core_identity",
            0.9,
            valid_until,
        )
        .await;
        seed_capacity(
            dir.as_ref(),
            "lenscore-detector",
            "agent-alpha",
            "integrity",
            0.8,
            valid_until,
        )
        .await;
        // Agent bravo: composite only.
        seed_capacity(
            dir.as_ref(),
            "lenscore-detector",
            "agent-bravo",
            "composite",
            0.42,
            valid_until,
        )
        .await;

        // ── Flow A read at the PUBLIC scope (the opted-in projection). ──
        let roster = build_roster(dir.as_ref(), CallerScope::Unauthenticated)
            .await
            .expect("build_roster must succeed");

        assert_eq!(roster.projection, "public_sample");
        assert_eq!(roster.agents.len(), 2, "two opted-in agents expected");

        // Agents are BTreeMap-ordered by attested_key_id.
        let alpha = roster
            .agents
            .iter()
            .find(|a| a.key_id == "agent-alpha")
            .expect("alpha present");
        assert_eq!(
            alpha.capacity_composite,
            Some(0.87),
            "composite projected from capacity:composite:v1"
        );
        assert_eq!(alpha.factors.get("core_identity"), Some(&0.9));
        assert_eq!(alpha.factors.get("integrity"), Some(&0.8));
        assert!(alpha.valid_until.is_some(), "freshness bound surfaced");

        let bravo = roster
            .agents
            .iter()
            .find(|a| a.key_id == "agent-bravo")
            .expect("bravo present");
        assert_eq!(bravo.capacity_composite, Some(0.42));
        assert!(bravo.factors.is_empty(), "bravo has no factors");

        // ── Field-compatibility with the lens scoring feed: serialize the
        // roster as `/api/v1/scoring` would and assert the lens shape
        // {key_id, capacity_composite, valid_until}. ──
        let json = serde_json::to_value(&roster).expect("serialize roster");
        let first = &json["agents"][0];
        assert!(first.get("key_id").is_some(), "key_id field present");
        assert!(
            first.get("capacity_composite").is_some(),
            "capacity_composite present"
        );
        assert!(first.get("valid_until").is_some(), "valid_until present");
    }

    #[tokio::test]
    async fn expired_capacity_rows_are_dropped_by_freshness() {
        let (engine, _seeds) = node().await;
        let dir = engine.sqlite_backend().expect("sqlite backend");
        register_key(dir.as_ref(), "lenscore-detector", identity_type::AGENT).await;
        register_key(dir.as_ref(), "agent-stale", identity_type::AGENT).await;

        // valid_until in the PAST → the filter's `valid_at: now` drops it.
        let past = Utc::now() - chrono::Duration::hours(1);
        seed_capacity(
            dir.as_ref(),
            "lenscore-detector",
            "agent-stale",
            "composite",
            0.5,
            past,
        )
        .await;

        let roster = build_roster(dir.as_ref(), CallerScope::Unauthenticated)
            .await
            .expect("build_roster");
        assert!(roster.agents.is_empty(), "expired rows must not surface");
    }
}
