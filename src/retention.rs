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

/// Default rows deleted per pass when no `config:*` value is set — the value
/// that ships, not the one production is stuck with (`status.corpus_retention_budget`).
///
/// A pass still takes a BOUNDED bite: the point is that the bite is sized
/// against how fast the backlog needs to disappear, not against a guess. Every
/// row left in the corpus is paid for again by every scan until it goes.
pub const PRUNE_BUDGET_PER_PASS: usize = 2_000;

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
    /// Whether the caller rebuilt the signed wire index after this pass. The
    /// pass itself never does — see [`rebuild_wire_index`] for why the policy
    /// lives with the caller.
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

    // Scan for ONE MORE than the budget, then keep the budget and let the
    // extra decide `more`.
    //
    // The obvious version — stop at the budget and set `more` because you
    // stopped — cannot tell "there are more rows" from "there were exactly this
    // many". With a backlog that happens to end on a budget boundary it reports
    // `more = true` with nothing left, which suppresses the end-of-drain
    // rebuild; the next pass then purges nothing, so `purged > 0` is false and
    // the rebuild never happens at all. A flag that is right except at the
    // boundary is wrong, and this one guards the expensive operation.
    let scan_target = budget.saturating_add(1);
    let mut cursor = None;
    let mut doomed: Vec<String> = Vec::new();
    'pages: loop {
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

        let next = page.next_cursor;
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
                if doomed.len() >= scan_target {
                    break 'pages;
                }
            }
        }
        match next {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    // The extra candidate, if we found one, is evidence rather than an
    // inference — and it is not deleted this pass.
    out.more = doomed.len() > budget;
    doomed.truncate(budget);

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

    Ok(out)
}

/// Repair the V111 signed wire index after purges.
///
/// **Separated from the pass that deletes, because the two have different
/// economics.** Deleting is cheap and wants to run often; this walks every
/// remaining row into memory and then upserts each one from an
/// immediately-invoked closure — no `spawn_blocking` — while holding the shared
/// connection mutex. At ~24k rows that parks a tokio worker and blocks every
/// other database user behind the mutex, the read API included.
///
/// Tying it to "this pass finished the backlog" looked right while a backlog
/// existed and became wrong the moment one did not: in steady state every pass
/// purges a few expired rows AND finishes, so the condition fired every single
/// time — a 24k-row rebuild every two minutes, which is how a fixed problem
/// came back wearing different clothes.
///
/// The index is a lookup accelerator, not a correctness invariant: an entry for
/// a purged row resolves to a miss. So the repair is owed eventually, not
/// immediately, and the caller decides when enough has changed to be worth the
/// stall.
pub async fn rebuild_wire_index(engine: &Engine) -> Result<u64> {
    engine
        .federation_directory()
        .rebuild_signed_wire_index()
        .await
        .map_err(|e| anyhow::anyhow!("rebuild signed wire index: {e}"))
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

    /// **The repair still has to WORK when the caller asks for it.**
    ///
    /// Splitting the rebuild out of the pass removed the only thing that
    /// exercised it — every remaining assertion in this file is about the pass
    /// NOT rebuilding, and a seam nothing calls is a seam nothing checks. This
    /// covers the half that moved: purge rows the pass will not repair after,
    /// then repair explicitly, and see the index reflect what actually
    /// survived.
    #[tokio::test]
    async fn the_extracted_rebuild_repairs_the_index_when_the_caller_asks() {
        let (engine, _s) = node().await;
        for i in 0..4 {
            crate::ceg::emit_observation(&engine, "unused", &env(&format!("service:x{i}")))
                .await
                .expect("emit");
        }

        // Measured as a DELTA, not against `count_own`: the index walks EVERY
        // signed row in the store — genesis and seed rows included — while
        // `count_own` is the filtered subset this retention pass can touch.
        // The first cut of this test asserted the two were equal and read 8
        // against 0, which is a two-different-populations mistake in the test
        // rather than a defect in the rebuild.
        let before = rebuild_wire_index(&engine).await.expect("rebuild");

        let out = prune_own_observations(&engine, 0, 100)
            .await
            .expect("prune");
        assert_eq!(out.purged, 4, "precondition: the rows went");
        assert!(
            !out.rebuilt,
            "precondition: the pass leaves the index owed, which is the point"
        );

        // The caller's job, now explicit. It must succeed, and the index it
        // rebuilds must have SHRUNK by exactly what the purge removed — a
        // rebuild that silently no-opped would reset the adapter's debt counter
        // against work that never happened.
        let after = rebuild_wire_index(&engine)
            .await
            .expect("the extracted rebuild must still work when called");
        assert_eq!(
            before - after,
            out.purged as u64,
            "the repaired index must have lost exactly the purged rows \
             (before={before}, after={after}, purged={})",
            out.purged
        );
    }

    /// The regression this arm exists for: on the sqlite backend the rebuild is
    /// tens of thousands of synchronous upserts on the async runtime, holding
    /// the shared connection mutex. Doing it per pass parks a worker every few
    /// minutes and the read API stops answering — the process stays alive, so
    /// it reads as a hang rather than as load. Deleting must never drag the
    /// repair along with it.
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

        // AND THE PASS THAT FINISHES THE BACKLOG DOES NOT PAY EITHER.
        //
        // This assertion used to be its inverse, and the inversion is the whole
        // lesson. "Rebuild once the backlog is drained" is correct exactly
        // while a backlog exists — and in STEADY STATE every pass purges a
        // handful of expired rows and finishes, so the condition fired every
        // single time: a full-table walk holding the connection mutex, every
        // two minutes, for seventeen deleted rows. The fixed problem came back
        // wearing different clothes.
        //
        // Deleting never drags the repair along with it now. The caller owns
        // that decision — see `rebuild_wire_index` and the adapter's purge
        // debt.
        let last = prune_own_observations(&engine, 0, 100)
            .await
            .expect("prune");
        assert!(!last.more);
        assert!(
            !last.rebuilt,
            "the pass that DRAINS the backlog must not rebuild either — a \
             finished-the-backlog condition is true on every steady-state pass"
        );
    }

    /// The boundary Codex caught on #61: a backlog that ends exactly on a
    /// budget boundary. The naive "I stopped, so there must be more" reports
    /// `more = true` with nothing left, which suppresses the end-of-drain
    /// rebuild — and the next pass purges nothing, so the `purged > 0` guard
    /// means the rebuild never happens at all.
    #[tokio::test]
    async fn a_backlog_ending_exactly_on_the_budget_still_finishes() {
        let (engine, _s) = node().await;
        for i in 0..3 {
            crate::ceg::emit_observation(&engine, "unused", &env(&format!("service:e{i}")))
                .await
                .expect("emit");
        }
        // Exactly as many candidates as the budget allows.
        let out = prune_own_observations(&engine, 0, 3).await.expect("prune");
        assert_eq!(out.purged, 3);
        assert!(
            !out.more,
            "nothing is left, so the pass must not claim there is"
        );
        assert!(
            !out.rebuilt,
            "finishing the backlog is not a reason to rebuild — in steady state \
             every pass finishes, so this is the condition that fired forever"
        );
        assert_eq!(count_own(&engine).await, 0);
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
