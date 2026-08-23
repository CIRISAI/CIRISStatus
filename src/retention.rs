//! Corpus retention for the rows THIS node authored.
//!
//! # Why this exists
//!
//! Flow B signs one `observation:reachability:v1` row per observed target and
//! sets `expires_at` to the observation cadence, so a row is dead within
//! minutes of being written. Nothing then removed it. Measured on the US node
//! on 2026-08-22, the corpus held 52,107 attestations of which **49,292 (95%)
//! were expired** — 30,781 of them ours from a single week — inside a 388MB
//! `ciris_engine.db` with a 280MB WAL, on a box with 3.9GB of RAM and no swap
//! left. Each row carries an ML-DSA-65 signature of ~3.3KB, so the growth is
//! not in the envelopes (35MB total) but in the signatures and indexes around
//! them.
//!
//! Expiry is a READ-side predicate: `valid_at` hides these rows from every
//! consumer, which is why nothing noticed. It is not a storage policy. On a
//! four-node mesh sharing one small box, storage policy is not optional.
//!
//! # What it will and will not delete
//!
//! Only rows that are ALL of: authored by this node's own derived key, on the
//! `observation:` dimension we mint ourselves, and asserted longer ago than the
//! retention window. A peer's row, a `config:*` row, an ownership or accord row
//! — none are reachable from here, because the filter that finds candidates
//! cannot express them.
//!
//! Deletion goes through persist's own `purge_attestation_v31`, which re-asks
//! the one question whose wrong answer is unrecoverable and REFUSES an
//! exclusion-bearing row whatever the caller believes (CIRISPersist#650/#652).
//! We do not hand-roll a DELETE.

use anyhow::Result;

use ciris_server::ciris_persist::ceg::list::federation::{AttestationFilter, LifecycleView};
use ciris_server::ciris_persist::ceg::ReadEngine;
use ciris_server::ciris_persist::federation::migration::MigrationSigner;
use ciris_server::ciris_persist::prelude::Engine;
use ciris_server::ciris_persist::scope::CallerScope;

/// The dimension prefix this node mints and is therefore allowed to reap.
const OWN_PREFIX: &str = "observation:";

/// Rows examined per pass. The backlog is tens of thousands and the box this
/// runs on is memory-starved, so a pass takes a bounded bite and comes back
/// next cycle rather than holding the corpus for a minute to catch up at once.
pub const PRUNE_BUDGET_PER_PASS: usize = 400;

/// Page size for the candidate scan. Small on purpose: each row carries its
/// signatures, so a 500-row page is several MB of allocation on a node that is
/// already paging.
const PAGE: i64 = 100;

/// What one pass did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PruneOutcome {
    pub scanned: usize,
    pub purged: usize,
    /// Rows persist refused to delete (exclusion-bearing). Reported, never
    /// retried into a loop.
    pub refused: usize,
    /// More candidates remain past this pass's budget.
    pub more: bool,
    /// Whether this pass rebuilt the signed wire index. Reported because it is
    /// the expensive half and it must happen exactly once, at the end of a
    /// drain — see the call site for what it costs on the runtime.
    pub rebuilt: bool,
}

impl PruneOutcome {
    pub fn did_anything(&self) -> bool {
        self.purged > 0 || self.refused > 0
    }
}

/// Delete this node's own `observation:*` rows asserted more than
/// `retention_hours` ago, up to `budget` rows.
///
/// Returns `Ok(PruneOutcome::default())` when the node has no resolvable
/// identity — with no derived key there is no way to prove a row is ours, and
/// "delete what you cannot attribute" is not a retention policy.
pub async fn prune_own_observations(
    engine: &Engine,
    retention_hours: i64,
    budget: usize,
) -> Result<PruneOutcome> {
    let mut out = PruneOutcome::default();

    // The DERIVED key (CIRISPersist#247), never the `--key-id` alias: rows are
    // authored as `ciris-status-1-<fp>`, so filtering on the alias silently
    // matches nothing and a retention pass that deletes nothing looks exactly
    // like one with nothing to do.
    let self_key = match MigrationSigner::signing_key_id(engine).await {
        Some(k) => k,
        None => return Ok(out),
    };

    let reader = match engine.sqlite_backend() {
        Some(b) => b,
        None => return Ok(out),
    };
    let directory = engine.federation_directory();

    let cutoff = chrono::Utc::now() - chrono::Duration::hours(retention_hours.max(0));
    // `window` is a half-open range on `asserted_at` compared as RFC-3339 TEXT
    // (CIRISPersist#605), so the lower bound must be a real, ordinary-year
    // instant rather than `DateTime::MIN_UTC` — a sentinel that sorts before
    // every row would exclude all of them.
    let epoch = chrono::DateTime::from_timestamp(0, 0).expect("1970 is representable");

    let mut filter = AttestationFilter::default();
    filter.attesting_key_id = Some(self_key.clone());
    filter.dimension_prefixes = vec![OWN_PREFIX.to_owned()];
    // Not `Live`: the default view keeps only the precedence-live head per
    // chain, and the rows worth reaping are precisely the ones that are no
    // longer anybody's head.
    filter.lifecycle = LifecycleView::All;
    filter.window = Some((epoch, cutoff));

    let mut cursor = None;
    let mut doomed: Vec<String> = Vec::new();
    while doomed.len() < budget {
        let page = reader
            .list_attestations(
                filter.clone(),
                cursor,
                PAGE,
                // Our rows are `cohort_scope: federation`, which the
                // unauthenticated projection admits — the same scope Flow A
                // reads the roster under.
                CallerScope::Unauthenticated,
            )
            .await
            .map_err(|e| anyhow::anyhow!("list own observation rows: {e}"))?;

        for row in page.items {
            out.scanned += 1;
            // Belt and braces on the two facts that license deletion. The
            // filter already asserts both; re-checking here means a future
            // filter change cannot quietly widen what this function reaps.
            let ours = row.attesting_key_id == self_key;
            let mine = row
                .attestation_envelope
                .get("dimension")
                .and_then(|v| v.as_str())
                .is_some_and(|d| d.starts_with(OWN_PREFIX));
            if ours && mine && row.asserted_at < cutoff {
                doomed.push(row.attestation_id);
                if doomed.len() >= budget {
                    out.more = true;
                    break;
                }
            }
        }
        match page.next_cursor {
            Some(c) if doomed.len() < budget => cursor = Some(c),
            Some(_) => {
                out.more = true;
                break;
            }
            None => break,
        }
    }

    for id in doomed {
        match directory.purge_attestation_v31(&id).await {
            // `false` = already gone, which a re-run after an interrupted pass
            // is expected to hit. Not an error, not a purge.
            Ok(true) => out.purged += 1,
            Ok(false) => {}
            Err(e) => {
                out.refused += 1;
                tracing::warn!(attestation_id = %id, error = %e, "retention: purge refused");
            }
        }
    }

    // The V111 signed wire index is not maintained per row by the purge door, so
    // it needs one rebuild — ONCE THE BACKLOG IS DRAINED, which is what
    // persist's own migration does and what the first cut of this function got
    // wrong by calling it per pass.
    //
    // The cost is not incidental. On the sqlite backend this walks every row
    // into memory and then runs an upsert per row from an IMMEDIATELY-INVOKED
    // closure — no `spawn_blocking` — while holding the shared connection
    // mutex. At 52k rows that parks a tokio worker (one of two on this box) and
    // blocks every other database user behind the mutex, including the read
    // API's accept loop. Threads that are not on that runtime keep running, so
    // the process looks alive while HTTP stops answering.
    //
    // Per pass, with a 30k backlog draining 400 at a time, that was ~75 of
    // these. Once, at the end, is one.
    if out.purged > 0 && !out.more {
        match directory.rebuild_signed_wire_index().await {
            Ok(n) => {
                out.rebuilt = true;
                tracing::info!(
                    rows = n,
                    "retention: signed wire index rebuilt (backlog drained)"
                );
            }
            Err(e) => tracing::warn!(error = %e, "retention: wire-index rebuild failed"),
        }
    }

    Ok(out)
}

// ─────────────────────────────────────────────────────────────────────────────
// Deletion is the one operation with no undo, so it gets a REAL corpus: a live
// engine, real hybrid-signed rows through the production emit door, and the
// production purge door. A mocked directory here would be testing the mock.
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;
    use ciris_server::ciris_persist::federation::Error as FederationError;
    use ciris_server::ciris_persist::prelude::{LocalSigner, LocalSignerConfig};

    struct SeedDir {
        dir: std::path::PathBuf,
    }
    impl SeedDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static CTR: AtomicU64 = AtomicU64::new(0);
            let n = CTR.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("ciris-status-retention-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            SeedDir { dir }
        }
        fn seed(&self, name: &str, b: [u8; 32]) -> std::path::PathBuf {
            let p = self.dir.join(name);
            std::fs::write(&p, b).unwrap();
            p
        }
    }
    impl Drop for SeedDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    async fn node() -> (std::sync::Arc<Engine>, SeedDir) {
        let seeds = SeedDir::new();
        let signer = std::sync::Arc::new(
            LocalSigner::from_config(&LocalSignerConfig {
                key_id: "ciris-status-retention".into(),
                key_path: seeds.seed("ed.seed", [0x31; 32]),
                pqc_key_id: Some("ciris-status-retention-pqc".into()),
                pqc_key_path: Some(seeds.seed("pqc.seed", [0x32; 32])),
            })
            .expect("LocalSigner"),
        );
        let engine = std::sync::Arc::new(
            Engine::with_signer(signer, "sqlite::memory:")
                .await
                .expect("Engine::with_signer"),
        );
        match engine
            .register_self_federation_key(
                "witness",
                "ciris-status-retention",
                None,
                serde_json::json!({}),
                Vec::new(),
            )
            .await
        {
            Ok(_) | Err(FederationError::Conflict(_)) => {}
            Err(e) => panic!("self-register: {e}"),
        }
        (engine, seeds)
    }

    fn env(observed: &str) -> crate::ceg::ObservationEnvelope {
        crate::ceg::ObservationEnvelope {
            observed: observed.into(),
            endpoint: Some("https://example.test/health".into()),
            via: None,
            score: 1.0,
            latency_ms: Some(12),
            confidence: 0.9,
            context: "fixture".into(),
            evidence: Vec::new(),
            valid_until: chrono::Utc::now() + chrono::Duration::seconds(300),
            asserted_at: chrono::Utc::now(),
            epistemic_mode: crate::ceg::EpistemicMode::Direct,
        }
    }

    async fn count_own(engine: &Engine) -> usize {
        let reader = engine.sqlite_backend().expect("sqlite backend");
        let mut f = AttestationFilter::default();
        f.dimension_prefixes = vec![OWN_PREFIX.to_owned()];
        f.lifecycle = LifecycleView::All;
        reader
            .list_attestations(f, None, 500, CallerScope::Unauthenticated)
            .await
            .expect("list")
            .items
            .len()
    }

    #[tokio::test]
    async fn our_own_expired_rows_are_reaped() {
        let (engine, _s) = node().await;
        for i in 0..3 {
            crate::ceg::emit_observation(&engine, "unused", &env(&format!("service:t{i}")))
                .await
                .expect("emit");
        }
        assert_eq!(count_own(&engine).await, 3);

        // retention_hours = 0 → the cutoff is "now", so rows asserted a moment
        // ago are past it. Config refuses anything below 1; this is the test
        // seam, and it is the same code path production runs.
        let out = prune_own_observations(&engine, 0, 100)
            .await
            .expect("prune");
        assert_eq!(out.purged, 3, "{out:?}");
        assert_eq!(out.refused, 0);
        assert_eq!(
            count_own(&engine).await,
            0,
            "the corpus is actually smaller"
        );
    }

    #[tokio::test]
    async fn rows_inside_the_window_are_left_alone() {
        let (engine, _s) = node().await;
        crate::ceg::emit_observation(&engine, "unused", &env("service:keep"))
            .await
            .expect("emit");

        // A 24h window: nothing written seconds ago is eligible. This is the
        // arm that stops a misconfigured retention pass from eating the plane
        // it is meant to keep tidy.
        let out = prune_own_observations(&engine, 24, 100)
            .await
            .expect("prune");
        assert_eq!(out.purged, 0, "{out:?}");
        assert_eq!(count_own(&engine).await, 1);
    }

    /// The regression this arm exists for: on the sqlite backend the rebuild is
    /// 52k synchronous upserts on the async runtime, holding the shared
    /// connection mutex. Doing it per pass while draining a backlog parks a
    /// worker every few minutes and the read API stops accepting — the process
    /// stays alive, so it reads as a hang rather than as load.
    #[tokio::test]
    async fn a_budgeted_pass_does_not_rebuild_the_wire_index() {
        let (engine, _s) = node().await;
        for i in 0..5 {
            crate::ceg::emit_observation(&engine, "unused", &env(&format!("service:r{i}")))
                .await
                .expect("emit");
        }
        let mid = prune_own_observations(&engine, 0, 2).await.expect("prune");
        assert!(mid.more, "precondition: this pass left work behind");
        assert!(
            !mid.rebuilt,
            "no rebuild while the backlog is still draining"
        );

        // Drain the rest; the pass that finishes the job is the one that pays.
        let last = prune_own_observations(&engine, 0, 100)
            .await
            .expect("prune");
        assert!(!last.more);
        assert!(last.rebuilt, "the final pass rebuilds exactly once");
    }

    #[tokio::test]
    async fn a_pass_stops_at_its_budget_and_says_there_is_more() {
        let (engine, _s) = node().await;
        for i in 0..5 {
            crate::ceg::emit_observation(&engine, "unused", &env(&format!("service:b{i}")))
                .await
                .expect("emit");
        }
        let out = prune_own_observations(&engine, 0, 2).await.expect("prune");
        assert_eq!(out.purged, 2);
        assert!(out.more, "a budgeted pass must admit it did not finish");
        assert_eq!(count_own(&engine).await, 3);
    }
}
