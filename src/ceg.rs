//! CEG `scores` attestation shapes + Flow B emit (probe → signed
//! `health:liveness`).
//!
//! This module is the novel piece of the monitoring node: it turns the existing
//! cost-safe probe results into **first-class, signed, replicable federation
//! data**. Per `FSD/MONITORING_NODE_DESIGN.md` §2 (Flow B) / §1, ciris-status
//! speaks *about* services as an **external witness** on the open-vocab
//! `health:liveness` dimension — it never speaks *as* the substrate (`system:*`
//! is reserved and would be rejected at admission).
//!
//! Two layers:
//!   * [`LivenessEnvelope`] — the pure CEG `scores` envelope shape. Always
//!     compiled and unit-tested; this is the JCS canonical-signing payload.
//!   * [`emit`] (under the `fabric` feature) — canonicalize → hybrid-sign →
//!     `FederationDirectory::put_attestation`, using the persist v8.4.0 /
//!     verify v5.10.0 substrate. No faked signing: if the substrate isn't
//!     linked (default build) there is no emit path at all.
//!
//! The shapes/helpers below are the emit-side API consumed under `--features
//! fabric`; in the default build they are exercised by the unit tests but
//! otherwise unused.
#![cfg_attr(not(feature = "fabric"), allow(dead_code))]

use serde::Serialize;
use serde_json::{json, Value};

use crate::probe::Probe;

/// The CEG dimension we emit on. Open-vocab leaf (§11.2.1) — NOT a reserved
/// prefix, so any `device_class: service` node may emit it (no substrate role
/// required). Versioned (`:v1`) to satisfy persist's default
/// `DimensionAdmissionPolicy { require_version_segment: true }` (admission.rs
/// §T3) so the emit survives a deployment that turns the admission gate on.
pub const DIMENSION: &str = "health:liveness:v1";

/// `witness_relation` — ciris-status observes from the outside.
pub const WITNESS_RELATION_EXTERNAL: &str = "external";

/// `stake` — the monitor is reputationally accountable for its claims.
pub const STAKE_REPUTATIONAL: &str = "reputational";

/// CEG `attestation_type` for state claims (matches
/// `ciris_persist::federation::types::attestation_type::SCORES`).
pub const ATTESTATION_TYPE_SCORES: &str = "scores";

/// Map a component health string → the CEG `scores` value on `health:liveness`:
/// operational `+1.0` / degraded `0.0` / outage `-1.0`.
pub fn liveness_score(status: &str) -> f64 {
    match status {
        crate::model::OPERATIONAL => 1.0,
        crate::model::DEGRADED => 0.0,
        crate::model::OUTAGE => -1.0,
        // Unknown is treated as "no positive signal" without asserting an outage.
        _ => 0.0,
    }
}

/// The CEG dimension Node B emits to record DIRECTED CONSENT to replicate its
/// `health:*` data to a federation peer (Node A). A `scores`-type attestation,
/// `subject_key_ids = [peer]`, `cohort_scope = "federation"`, FEDERATION tier —
/// the shared Round-2 wire contract (identical on both nodes). Modeled on the
/// agent's `consent:community_trust:v1` directed grant
/// (CIRISAgent .../consent/attestation.py:351 `build_community_consent_grant`):
/// a directed, bilateral consent object — NEVER broadcast.
pub const CONSENT_DIMENSION: &str = "consent:replication:v1";

/// The replication grant value carried in the consent payload's `grants` field
/// (constant — §5.6.8.15).
pub const CONSENT_GRANTS_REPLICATION: &str = "replication";

/// `witness_relation` for the consent attestation — REQUIRED to be `"self"`
/// (§5.6.8.15 RC29 LOCKED): the granting node speaks about ITS OWN consent.
pub const CONSENT_WITNESS_RELATION_SELF: &str = "self";

/// `subject_kind` for the consent payload (§4.2.2.3 payload member) — declares
/// the payload is a replication-consent object.
pub const CONSENT_SUBJECT_KIND: &str = "consent_replication";

/// `topical_relation` SHOULD-carry value: A<->B is a directed bilateral pair.
pub const CONSENT_TOPICAL_RELATION_BILATERAL: &str = "bilateral_pair";

/// `epistemic_mode` (§2 Flow B): a direct probe vs a proxy-folded observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
#[allow(dead_code)] // `Direct` is the direct-/health-probe variant; the loop
                    // currently folds region/proxy self-reports → `Derivative`.
pub enum EpistemicMode {
    /// We probed the target ourselves (the keyed service's `/health`).
    Direct,
    /// Folded in from a proxy/upstream self-report (provider/region evidence).
    Derivative,
}

impl EpistemicMode {
    pub fn as_str(self) -> &'static str {
        match self {
            EpistemicMode::Direct => "direct",
            EpistemicMode::Derivative => "derivative",
        }
    }
}

/// One piece of evidence behind a keyed service's `health:liveness` score.
///
/// Non-keyed infra (LLM/search providers, regions, billing/proxy) folds in here
/// — it has no federation key, so per design §1/§2.2 it is *evidence behind* a
/// keyed service's health, **not** a subject of its own CEG attestation.
#[derive(Clone, Debug, Serialize)]
pub struct EvidenceRef {
    /// e.g. `"provider:openrouter"`, `"region:us"`, `"probe:billing.us"`.
    pub ref_id: String,
    /// The observed component status (operational/degraded/outage).
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl EvidenceRef {
    #[allow(dead_code)] // convenience ctor for direct-probe evidence folding
    pub fn from_probe(ref_id: impl Into<String>, p: &Probe) -> Self {
        EvidenceRef {
            ref_id: ref_id.into(),
            status: p.status.to_string(),
            latency_ms: p.latency_ms,
            detail: p.message.clone(),
        }
    }
}

/// The full CEG `health:liveness` `scores` envelope ciris-status emits per keyed
/// CIRIS service. This is the canonical-signing payload (the JCS bytes signed),
/// matching `FSD/MONITORING_NODE_DESIGN.md` §2 Flow B step 3.
#[derive(Clone, Debug)]
pub struct LivenessEnvelope {
    /// The CIRIS service node's `key_id` (the subject — goes in the row's
    /// `attested_key_id`, and is also echoed in the envelope for self-containment).
    pub attested_key_id: String,
    /// `+1.0 | 0.0 | -1.0` (operational/degraded/outage).
    pub score: f64,
    /// Probe certainty `[0,1]`.
    pub confidence: f64,
    /// Region / target detail (e.g. `"US (Chicago) — billing+proxy"`).
    pub context: String,
    /// Provider/region/probe evidence — the non-keyed infra folded in here.
    pub evidence: Vec<EvidenceRef>,
    /// `now + poll cadence` (freshness; becomes the row's `expires_at`).
    pub valid_until: chrono::DateTime<chrono::Utc>,
    /// When the observation was made (becomes the row's `asserted_at`).
    pub asserted_at: chrono::DateTime<chrono::Utc>,
    pub epistemic_mode: EpistemicMode,
}

impl LivenessEnvelope {
    /// Build the `scores` envelope JSON — the exact object that gets
    /// JCS-canonicalized and hybrid-signed. Stable key set; numbers are plain
    /// JSON numbers (JCS-safe: small integers/one-dp confidences).
    pub fn to_envelope(&self) -> Value {
        json!({
            "dimension": DIMENSION,
            "score": self.score,
            "confidence": self.confidence,
            "context": self.context,
            "evidence_refs": self.evidence,
            "valid_until": rfc3339(self.valid_until),
            "epistemic_mode": self.epistemic_mode.as_str(),
            "witness_relation": WITNESS_RELATION_EXTERNAL,
            "stake": STAKE_REPUTATIONAL,
            "attested_key_id": self.attested_key_id,
        })
    }
}

fn rfc3339(t: chrono::DateTime<chrono::Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// The DIRECTED consent grant Node B emits at Node A: "B consents to replicate
/// its `health:` attestations to A." A `scores`-type, federation-scope object
/// directed at one counterparty (`subject = [A key_id]`) — the bilateral
/// authorization for A<->B replication (federation-scope, NOT in-group trust).
///
/// This is the Round-2 SHARED WIRE CONTRACT — its envelope shape is identical on
/// both nodes. The payload records `grants: "replication"`, the
/// `attestation_prefixes` B authorizes A to pull (`["health:"]`), and the
/// directed `peer` (A's key_id). Revocable later via CEG structural primitives
/// (withdraws/recants) — not built here (see attestation.py:380).
#[derive(Clone, Debug)]
pub struct ConsentEnvelope {
    /// The directed counterparty — Node A's federation `key_id` (the subject).
    pub peer_key_id: String,
    /// The attestation prefixes B authorizes A to replicate (e.g. `["health:"]`).
    pub attestation_prefixes: Vec<String>,
    /// When the grant was made (becomes the row's `asserted_at`).
    pub asserted_at: chrono::DateTime<chrono::Utc>,
}

impl ConsentEnvelope {
    /// `attestation_prefixes` normalized to the §5.6.8.15 LOCKED JCS form: a
    /// JSON array, **sorted ascending + deduplicated**, trailing `":"`
    /// significant (e.g. `["health:"]`). Narrowing the grant later MUST go via a
    /// `supersedes` attestation, NEVER a silent drop of a prefix here.
    fn sorted_deduped_prefixes(&self) -> Vec<String> {
        let mut v = self.attestation_prefixes.clone();
        v.sort();
        v.dedup();
        v
    }

    /// Build the `consent:replication:v1` `scores` consent envelope JSON — the
    /// exact object that gets JCS-canonicalized and hybrid-signed — in the RC29
    /// LOCKED shape (CEG §5.6.8.15, resolves CIRISRegistry#98).
    ///
    /// ENVELOPE-level members:
    ///   * `dimension` = `consent:replication:v1`
    ///   * `score` > 0 (positive grant; here +1.0 / full confidence)
    ///   * `subject_key_ids` = `[A]` (SINGLE peer — directed, bilateral)
    ///   * `cohort_scope` = `"federation"`
    ///   * `witness_relation` = `"self"` (REQUIRED — B speaks about its own consent)
    ///   * `topical_relation` = `"bilateral_pair"` (SHOULD)
    ///   * (`valid_until` deliberately OMITTED unless time-boxing the grant)
    ///
    /// PAYLOAD-level members (nested `payload`, §4.2.2.3 — subject_kind =
    /// `consent_replication`; these are payload members, NOT envelope fields):
    ///   * `grants` = `"replication"` (constant)
    ///   * `attestation_prefixes` = sorted-ascending + deduplicated JCS array
    ///     (trailing `":"` significant).
    pub fn to_envelope(&self) -> Value {
        json!({
            "dimension": CONSENT_DIMENSION,
            // A positive `scores` grant (consent granted). +1.0 / full confidence.
            "score": 1.0,
            "confidence": 1.0,
            // DIRECTED at a SINGLE peer (A) — bilateral, never broadcast.
            "subject_key_ids": [self.peer_key_id],
            "cohort_scope": "federation",
            // REQUIRED (§5.6.8.15): B attests about its OWN consent.
            "witness_relation": CONSENT_WITNESS_RELATION_SELF,
            // SHOULD: A<->B is a directed bilateral pair.
            "topical_relation": CONSENT_TOPICAL_RELATION_BILATERAL,
            "asserted_at": rfc3339(self.asserted_at),
            // Payload member (§4.2.2.3): subject_kind = consent_replication.
            "payload": {
                "subject_kind": CONSENT_SUBJECT_KIND,
                "grants": CONSENT_GRANTS_REPLICATION,
                // Sorted-ascending + deduped; trailing ":" significant. Partial
                // narrowing MUST go via `supersedes`, never a silent drop.
                "attestation_prefixes": self.sorted_deduped_prefixes(),
            },
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Flow B emit — REAL signing + emission via the persist/verify substrate.
// Compiled only under `--features fabric`. No substrate ⇒ no emit path (we never
// fake a signature).
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(feature = "fabric")]
pub mod emit {
    use super::*;
    use anyhow::{Context, Result};
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    use sha2::{Digest, Sha256};

    use ciris_persist::federation::types::{attestation_tier, Attestation, SignedAttestation};
    use ciris_persist::federation::FederationDirectory;
    use ciris_persist::prelude::{canonicalize_envelope_for_signing, Engine};

    /// Sign + emit one `health:liveness` `scores` attestation for a keyed
    /// service, returning the `original_content_hash` (hex) of the signed
    /// envelope.
    ///
    /// Recipe mirrors persist's own native produce path (engine.rs
    /// `attestation_promote`, v8.4.0):
    ///   1. build the envelope JSON ([`LivenessEnvelope::to_envelope`]),
    ///   2. JCS-canonicalize it (`canonicalize_envelope_for_signing`),
    ///   3. `original_content_hash = hex(SHA-256(canonical))`,
    ///   4. `Engine::sign_hybrid(canonical)` → Ed25519 + ML-DSA-65 (base64),
    ///   5. assemble a federation-tier [`Attestation`] and `put_attestation`.
    pub async fn emit_liveness<D>(
        engine: &Engine,
        directory: &D,
        env: &LivenessEnvelope,
    ) -> Result<String>
    where
        D: FederationDirectory,
    {
        let envelope = env.to_envelope();

        // 2. JCS canonical bytes (the signing basis — CEG §0.9).
        let canonical = canonicalize_envelope_for_signing(&envelope)
            .map_err(|e| anyhow::anyhow!("canonicalize health:liveness envelope: {e}"))?;

        // 3. original_content_hash = hex(SHA-256(canonical)).
        let original_content_hash = hex::encode(Sha256::digest(&canonical));

        // 4. Hybrid sign (Ed25519 classical + ML-DSA-65 PQC) over the canonical
        //    bytes — the same payload persist verifies against on admission/read.
        let sig = engine
            .sign_hybrid(&canonical)
            .await
            .context("hybrid-sign health:liveness envelope")?;
        let classical_b64 = B64.encode(&sig.classical.signature);
        let pqc_b64 = B64.encode(&sig.pqc.signature);
        let scrub_key_id = engine.signer().current_alias().to_owned();
        let now = chrono::Utc::now();

        // 5. Assemble the federation-tier row.
        let attestation = Attestation {
            attestation_id: uuid_v4(),
            attesting_key_id: scrub_key_id.clone(),
            attested_key_id: env.attested_key_id.clone(),
            attestation_type: ATTESTATION_TYPE_SCORES.to_owned(),
            weight: Some(env.confidence),
            asserted_at: env.asserted_at,
            expires_at: Some(env.valid_until),
            attestation_envelope: envelope,
            original_content_hash: original_content_hash.clone(),
            scrub_signature_classical: classical_b64,
            scrub_signature_pqc: Some(pqc_b64),
            scrub_key_id,
            scrub_timestamp: now,
            pqc_completed_at: Some(now),
            persist_row_hash: String::new(), // server-computed on insert
            subject_key_ids: vec![env.attested_key_id.clone()],
            withdraws_admission_rule: None,
            cohort_scope: "federation".to_owned(),
            tier: attestation_tier::FEDERATION.to_owned(),
            promoted_at: None,
        };

        directory
            .put_attestation(SignedAttestation { attestation })
            .await
            .map_err(|e| anyhow::anyhow!("put_attestation(health:liveness): {e}"))?;

        Ok(original_content_hash)
    }

    /// Sign + emit the DIRECTED `consent:replication:v1` `scores` attestation at
    /// the peer (Node A), returning the `original_content_hash` (hex). Same recipe
    /// as [`emit_liveness`]: build envelope → JCS-canonicalize → hybrid-sign →
    /// assemble a federation-tier [`Attestation`] with `subject_key_ids = [peer]`
    /// → `put_attestation`.
    ///
    /// The attestation is DIRECTED (`subject_key_ids = [peer]`) and federation-
    /// scope — the bilateral consent wire artifact, never broadcast (the
    /// attestation.py:351 directed-grant invariant ported to Rust).
    ///
    /// Idempotent: if a `consent:replication:v1` row directed at this peer is
    /// already present, the emit is a no-op (returns that row's
    /// `original_content_hash`). Partial NARROWING of an existing grant MUST go
    /// via a `supersedes` attestation — never a silent re-emit / drop here.
    pub async fn emit_consent<D>(
        engine: &Engine,
        directory: &D,
        env: &ConsentEnvelope,
    ) -> Result<String>
    where
        D: FederationDirectory,
    {
        // ── Idempotence guard: don't re-emit if a consent:replication:v1 grant
        //    directed at this peer already exists. (Re-narrowing the grant is a
        //    `supersedes`, handled separately — never a silent drop.)
        if let Ok(existing) = directory.list_attestations_for(&env.peer_key_id).await {
            if let Some(row) = existing.iter().find(|a| {
                a.attestation_envelope
                    .get("dimension")
                    .and_then(|v| v.as_str())
                    == Some(CONSENT_DIMENSION)
                    && a.subject_key_ids == vec![env.peer_key_id.clone()]
            }) {
                return Ok(row.original_content_hash.clone());
            }
        }

        let envelope = env.to_envelope();

        let canonical = canonicalize_envelope_for_signing(&envelope)
            .map_err(|e| anyhow::anyhow!("canonicalize consent:replication envelope: {e}"))?;
        let original_content_hash = hex::encode(Sha256::digest(&canonical));

        let sig = engine
            .sign_hybrid(&canonical)
            .await
            .context("hybrid-sign consent:replication envelope")?;
        let classical_b64 = B64.encode(&sig.classical.signature);
        let pqc_b64 = B64.encode(&sig.pqc.signature);
        let scrub_key_id = engine.signer().current_alias().to_owned();
        let now = chrono::Utc::now();

        let attestation = Attestation {
            attestation_id: uuid_v4(),
            attesting_key_id: scrub_key_id.clone(),
            // The directed counterparty is BOTH the attested key and the subject.
            attested_key_id: env.peer_key_id.clone(),
            attestation_type: ATTESTATION_TYPE_SCORES.to_owned(),
            weight: Some(1.0),
            asserted_at: env.asserted_at,
            // Consent grants don't expire on a poll cadence — leave open-ended
            // (revocable via CEG structural primitives, not TTL).
            expires_at: None,
            attestation_envelope: envelope,
            original_content_hash: original_content_hash.clone(),
            scrub_signature_classical: classical_b64,
            scrub_signature_pqc: Some(pqc_b64),
            scrub_key_id,
            scrub_timestamp: now,
            pqc_completed_at: Some(now),
            persist_row_hash: String::new(),
            // DIRECTED: subject = [peer]. This is what makes the grant bilateral
            // and not a broadcast attestation.
            subject_key_ids: vec![env.peer_key_id.clone()],
            withdraws_admission_rule: None,
            cohort_scope: "federation".to_owned(),
            tier: attestation_tier::FEDERATION.to_owned(),
            promoted_at: None,
        };

        directory
            .put_attestation(SignedAttestation { attestation })
            .await
            .map_err(|e| anyhow::anyhow!("put_attestation(consent:replication): {e}"))?;

        Ok(original_content_hash)
    }

    fn uuid_v4() -> String {
        // Minimal RFC-4122 v4 without pulling a uuid dep; randomness from getrandom
        // via the rng the substrate already links is overkill here — use time +
        // a process-local counter mixed value. Good enough for a row id (the
        // content hash is the integrity anchor, not this).
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let t = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default() as u64;
        let a = t ^ (n.rotate_left(17));
        let b = t.rotate_left(31) ^ n;
        format!(
            "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
            (a >> 32) as u32,
            (a >> 16) as u16,
            (a as u16) & 0x0fff,
            ((b >> 48) as u16 & 0x3fff) | 0x8000,
            b & 0xffff_ffff_ffff,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scores_map_to_pm1() {
        assert_eq!(liveness_score(crate::model::OPERATIONAL), 1.0);
        assert_eq!(liveness_score(crate::model::DEGRADED), 0.0);
        assert_eq!(liveness_score(crate::model::OUTAGE), -1.0);
        assert_eq!(liveness_score("unknown"), 0.0);
    }

    #[test]
    fn envelope_shape_is_stable_and_external() {
        let env = LivenessEnvelope {
            attested_key_id: "k_service_us".into(),
            score: 1.0,
            confidence: 0.9,
            context: "US (Chicago)".into(),
            evidence: vec![EvidenceRef {
                ref_id: "provider:openrouter".into(),
                status: "operational".into(),
                latency_ms: Some(120),
                detail: None,
            }],
            valid_until: chrono::DateTime::parse_from_rfc3339("2026-06-16T00:01:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            asserted_at: chrono::DateTime::parse_from_rfc3339("2026-06-16T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            epistemic_mode: EpistemicMode::Direct,
        };
        let v = env.to_envelope();
        assert_eq!(v["dimension"], DIMENSION);
        assert_eq!(v["witness_relation"], WITNESS_RELATION_EXTERNAL);
        assert_eq!(v["epistemic_mode"], "direct");
        assert_eq!(v["score"], 1.0);
        assert_eq!(v["stake"], STAKE_REPUTATIONAL);
        // Non-keyed infra is evidence, not a subject.
        assert_eq!(v["evidence_refs"][0]["ref_id"], "provider:openrouter");
        // valid_until present for freshness.
        assert!(v["valid_until"].is_string());
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Flow B — REAL probe→emit sign path proof.
//
// Builds a `LivenessEnvelope`, JCS-canonicalizes it with persist's OWN
// canonicalizer (`canonicalize_envelope_for_signing`, the exact signing basis),
// computes the SHA-256 content hash, and HYBRID-SIGNS the canonical bytes via a
// real `LocalSigner` (Ed25519 + ML-DSA-65 software, loaded from seed files the
// way production does). This proves the probe→signed-`health:liveness` pipeline
// up to a verifiable hybrid signature — the prompt's Flow B bar ("construct +
// sign the attestation"). The dimension carries the mandatory `:v1` segment so
// it would survive persist's admission gate.
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(all(test, feature = "fabric"))]
mod flow_b_sign {
    use super::*;
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    use ciris_persist::prelude::{
        canonicalize_envelope_for_signing, Engine, LocalSigner, LocalSignerConfig,
    };
    use sha2::{Digest, Sha256};

    /// Temp seed-file dir (32-byte raw seeds for Ed25519 + ML-DSA-65).
    struct SeedDir {
        dir: std::path::PathBuf,
    }
    impl SeedDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static CTR: AtomicU64 = AtomicU64::new(0);
            let n = CTR.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("ciris-status-ceg-seed-{}-{n}", std::process::id()));
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

    fn sample_env() -> LivenessEnvelope {
        LivenessEnvelope {
            attested_key_id: "k_service_us".into(),
            score: liveness_score(crate::model::OPERATIONAL),
            confidence: 0.9,
            context: "US (Chicago) — region us".into(),
            evidence: vec![EvidenceRef {
                ref_id: "provider:openrouter".into(),
                status: "operational".into(),
                latency_ms: Some(120),
                detail: None,
            }],
            valid_until: chrono::Utc::now() + chrono::Duration::seconds(60),
            asserted_at: chrono::Utc::now(),
            epistemic_mode: EpistemicMode::Derivative,
        }
    }

    #[tokio::test]
    async fn liveness_envelope_canonicalizes_and_hybrid_signs() {
        let seeds = SeedDir::new();
        let ed = seeds.seed("ed.seed", [0x42; 32]);
        let pqc = seeds.seed("pqc.seed", [0x77; 32]);

        let signer = std::sync::Arc::new(
            LocalSigner::from_config(&LocalSignerConfig {
                key_id: "ciris-status-monitor".into(),
                key_path: ed,
                pqc_key_id: Some("ciris-status-monitor-pqc".into()),
                pqc_key_path: Some(pqc),
            })
            .expect("LocalSigner::from_config with PQC"),
        );
        let engine = Engine::with_signer(signer, "sqlite::memory:")
            .await
            .expect("Engine::with_signer");

        let env = sample_env();
        let envelope = env.to_envelope();
        assert_eq!(envelope["dimension"], DIMENSION); // health:liveness:v1

        // The EXACT signing basis: persist's JCS canonicalizer (CEG §0.9).
        let canonical =
            canonicalize_envelope_for_signing(&envelope).expect("canonicalize envelope");
        assert!(!canonical.is_empty());

        // Content hash anchor (what emit_liveness records as original_content_hash).
        let content_hash = hex::encode(Sha256::digest(&canonical));
        assert_eq!(content_hash.len(), 64, "SHA-256 hex");

        // Hybrid sign the canonical bytes — Ed25519 + ML-DSA-65.
        let sig = engine
            .sign_hybrid(&canonical)
            .await
            .expect("hybrid-sign canonical envelope");
        assert!(!sig.classical.signature.is_empty(), "Ed25519 half present");
        assert!(!sig.pqc.signature.is_empty(), "ML-DSA-65 half present");
        // Both halves base64-encode (the shape ceg::emit stores).
        assert!(!B64.encode(&sig.classical.signature).is_empty());
        assert!(!B64.encode(&sig.pqc.signature).is_empty());

        // The signer alias is the attesting key_id ceg::emit stamps.
        assert_eq!(engine.signer().current_alias(), "ciris-status-monitor");
    }

    #[tokio::test]
    async fn degraded_and_outage_map_to_zero_and_negative() {
        // Cost-safe: pure construction, no probe, no network.
        let mut env = sample_env();
        env.score = liveness_score(crate::model::DEGRADED);
        assert_eq!(env.to_envelope()["score"], 0.0);
        env.score = liveness_score(crate::model::OUTAGE);
        assert_eq!(env.to_envelope()["score"], -1.0);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Round-2 federation proof: self-registration → admissible health:liveness emit;
// peer-A key registration → A-signed capacity:* admitted + surfaced in the
// roster; directed consent:replication:v1 emit.
//
// The live transport round-trip (A pushing CRPL frames over HTTP into B's
// `route_inbound_bytes`) is integration-only — `Edge::run` / the listen loop need
// two real sockets — so this proves the DIRECTORY-LEVEL admission spine both
// replication directions deliver into (`put_attestation` is exactly what the
// inbound bridge calls), plus the consent emit. Same approach as the upstream
// edge note in CIRISServer/tests/replication.rs.
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(all(test, feature = "fabric"))]
mod round2_federation {
    use super::emit::{emit_consent, emit_liveness};
    use super::{ConsentEnvelope, EpistemicMode, EvidenceRef, LivenessEnvelope};

    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    use chrono::Utc;
    use ciris_persist::ceg::list::federation::AttestationFilter;
    use ciris_persist::federation::types::{
        algorithm, attestation_tier, cohort_scope, Attestation, KeyRecord, SignedAttestation,
        SignedKeyRecord,
    };
    use ciris_persist::federation::{Error as FederationError, FederationDirectory};
    use ciris_persist::prelude::{Engine, LocalSigner, LocalSignerConfig};
    use ciris_persist::scope::CallerScope;
    use ciris_persist::verify::canonical::ceg_produce_canonicalize;
    use sha2::{Digest, Sha256};

    struct SeedDir {
        dir: std::path::PathBuf,
    }
    impl SeedDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static CTR: AtomicU64 = AtomicU64::new(0);
            let n = CTR.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("ciris-status-r2-seed-{}-{n}", std::process::id()));
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

    /// Node B: a hybrid-signing in-memory Engine keyed by its witness identity.
    async fn node_b() -> (std::sync::Arc<Engine>, SeedDir) {
        let seeds = SeedDir::new();
        let ed = seeds.seed("b.ed", [0x42; 32]);
        let pqc = seeds.seed("b.pqc", [0x77; 32]);
        let signer = std::sync::Arc::new(
            LocalSigner::from_config(&LocalSignerConfig {
                key_id: "ciris-status-monitor".into(),
                key_path: ed,
                pqc_key_id: Some("ciris-status-monitor-pqc".into()),
                pqc_key_path: Some(pqc),
            })
            .expect("LocalSigner::from_config with PQC"),
        );
        let engine = std::sync::Arc::new(
            Engine::with_signer(signer, "sqlite::memory:")
                .await
                .expect("Engine::with_signer"),
        );
        (engine, seeds)
    }

    /// Build a self-signed (proof-of-possession) `SignedKeyRecord` for `key_id`,
    /// signed by `signer`'s hybrid keypair, in the v8.8.0 §5.6.8.15 admission-gate
    /// shape: `scrub_key_id == key_id`, hybrid-signed over
    /// `ceg_produce_canonicalize(envelope)`, `original_content_hash` cross-checked.
    /// This is exactly what a node exports for a peer to register.
    async fn signed_self_record(
        signer: &Engine,
        key_id: &str,
        identity_type: &str,
    ) -> SignedKeyRecord {
        let envelope = serde_json::json!({ "key_id": key_id });
        // The gate canonicalizes via ceg_produce_canonicalize — sign over those.
        let canonical = ceg_produce_canonicalize(&envelope).unwrap();
        let och = hex::encode(Sha256::digest(&canonical));
        let sig = signer.sign_hybrid(&canonical).await.unwrap();
        let now = Utc::now();
        let rec = KeyRecord {
            key_id: key_id.to_string(),
            pubkey_ed25519_base64: B64.encode(&sig.classical.public_key),
            pubkey_ml_dsa_65_base64: Some(B64.encode(&sig.pqc.public_key)),
            algorithm: algorithm::HYBRID.into(),
            identity_type: identity_type.into(),
            identity_ref: key_id.to_string(),
            valid_from: now,
            valid_until: None,
            registration_envelope: envelope,
            original_content_hash: och,
            scrub_signature_classical: B64.encode(&sig.classical.signature),
            scrub_signature_pqc: Some(B64.encode(&sig.pqc.signature)),
            scrub_key_id: key_id.to_string(),
            scrub_timestamp: now,
            pqc_completed_at: Some(now),
            persist_row_hash: String::new(),
            roles: Vec::new(),
            attestation_evidence: None,
        };
        SignedKeyRecord { record: rec }
    }

    /// Self-register B's OWN witness key via the v8.8.0 single canonical gate
    /// `register_federation_key` (fail-secure PoP verify). Conflict = benign.
    async fn register_self_key(engine: &Engine, key_id: &str) {
        let record = signed_self_record(engine, key_id, "witness").await;
        match engine.register_federation_key(record).await {
            Ok(()) | Err(FederationError::Conflict(_)) => {}
            Err(e) => panic!("self-register witness key: {e}"),
        }
    }

    /// Build A's hybrid signer (a distinct keypair from B) so A can self-sign its
    /// own SignedKeyRecord — the v8.8.0 gate requires A's proof-of-possession, B
    /// can no longer fabricate A's row.
    async fn node_a(seeds: &SeedDir, key_id: &str) -> std::sync::Arc<Engine> {
        let ed = seeds.seed("a.ed", [0xA1; 32]);
        let pqc = seeds.seed("a.pqc", [0xA7; 32]);
        let signer = std::sync::Arc::new(
            LocalSigner::from_config(&LocalSignerConfig {
                key_id: key_id.into(),
                key_path: ed,
                pqc_key_id: Some(format!("{key_id}-pqc")),
                pqc_key_path: Some(pqc),
            })
            .expect("LocalSigner::from_config A with PQC"),
        );
        std::sync::Arc::new(
            Engine::with_signer(signer, "sqlite::memory:")
                .await
                .expect("Engine::with_signer A"),
        )
    }

    /// Register peer A via the v8.8.0 gate from A's *self-signed* SignedKeyRecord
    /// (A's proof-of-possession). B hands A's exported record to the gate, which
    /// verifies A's signature fail-secure. `a_signer` is A's own keypair.
    async fn register_peer_key(engine: &Engine, a_signer: &Engine, key_id: &str) {
        let record = signed_self_record(a_signer, key_id, "steward").await;
        match engine.register_federation_key(record).await {
            Ok(()) | Err(FederationError::Conflict(_)) => {}
            Err(e) => panic!("register peer A key: {e}"),
        }
    }

    /// Legacy raw-pubkey directory insert used only to seed *subject* keys (the
    /// `attested_key_id` an attestation targets) where no PoP is needed — these
    /// are just FK targets, not peering authorities. Goes straight to
    /// `put_public_key` (the gate is for granting peers).
    async fn register_subject_key(engine: &Engine, key_id: &str, ed_pub_b64: &str) {
        let now = Utc::now();
        let rec = KeyRecord {
            key_id: key_id.to_string(),
            pubkey_ed25519_base64: ed_pub_b64.to_string(),
            pubkey_ml_dsa_65_base64: None,
            algorithm: algorithm::HYBRID.into(),
            identity_type: "steward".into(),
            identity_ref: key_id.to_string(),
            valid_from: now,
            valid_until: None,
            registration_envelope: serde_json::json!({ "key_id": key_id }),
            original_content_hash: String::new(),
            scrub_signature_classical: ed_pub_b64.to_string(),
            scrub_signature_pqc: None,
            scrub_key_id: key_id.to_string(),
            scrub_timestamp: now,
            pqc_completed_at: None,
            persist_row_hash: String::new(),
            roles: Vec::new(),
            attestation_evidence: None,
        };
        engine
            .federation_directory()
            .put_public_key(SignedKeyRecord { record: rec })
            .await
            .expect("register subject key");
    }

    fn capacity_filter() -> AttestationFilter {
        AttestationFilter {
            attesting_key_id: None,
            attested_key_id: None,
            attestation_type: Some(super::ATTESTATION_TYPE_SCORES.to_owned()),
            pqc_completed: None,
            dimension_prefixes: vec![super::super::roster::CAPACITY_PREFIX.to_owned()],
            valid_at: None,
            confidence_floor: None,
            subject_key_id: None,
        }
    }

    #[tokio::test]
    async fn self_register_admits_liveness_and_consent_directed_at_peer() {
        use ciris_persist::ceg::ReadEngine;
        let (engine, seeds) = node_b().await;
        let backend = engine.sqlite_backend().expect("sqlite backend").clone();

        // The subject service B attests health about must exist as a key too
        // (a plain FK target, not a peering authority → no PoP needed).
        register_subject_key(
            &engine,
            "k_service_us",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
        )
        .await;

        // ── (1) BEFORE self-registration, B's own emit must FAIL the key gate ──
        let env = LivenessEnvelope {
            attested_key_id: "k_service_us".into(),
            score: 1.0,
            confidence: 0.9,
            context: "US — region us".into(),
            evidence: vec![EvidenceRef {
                ref_id: "provider:openrouter".into(),
                status: "operational".into(),
                latency_ms: Some(120),
                detail: None,
            }],
            valid_until: Utc::now() + chrono::Duration::hours(1),
            asserted_at: Utc::now(),
            epistemic_mode: EpistemicMode::Derivative,
        };
        let before = emit_liveness(&engine, backend.as_ref(), &env).await;
        assert!(
            before.is_err(),
            "without self-registration the attesting key is absent → emit must be rejected"
        );

        // ── self-register B (witness) → now the emit is admissible ───────────
        register_self_key(&engine, "ciris-status-monitor").await;
        let hash = emit_liveness(&engine, backend.as_ref(), &env)
            .await
            .expect("after self-registration, B's health:liveness must be admitted");
        assert_eq!(hash.len(), 64, "content hash is SHA-256 hex");

        // It actually landed: read it back on the health:liveness dimension.
        let mut f = capacity_filter();
        f.dimension_prefixes = vec![super::DIMENSION.to_owned()];
        let page = backend
            .list_attestations(f, None, 100, CallerScope::Unauthenticated)
            .await
            .expect("list health:liveness");
        assert!(
            page.items
                .iter()
                .any(|a| a.attested_key_id == "k_service_us"),
            "B's emitted health:liveness must be in B's own corpus"
        );

        // ── (3) directed consent:replication:v1 at peer A ────────────────────
        // A self-signs its own SignedKeyRecord (v8.8.0 gate requires A's PoP).
        let a = node_a(&seeds, "ciris-server-steward").await;
        register_peer_key(&engine, &a, "ciris-server-steward").await;

        // Unsorted + duplicated prefixes on input → the envelope must emit them
        // sorted-ascending + deduped (RC29 LOCKED JCS form).
        let consent = ConsentEnvelope {
            peer_key_id: "ciris-server-steward".into(),
            attestation_prefixes: vec!["health:".into(), "capacity:".into(), "health:".into()],
            asserted_at: Utc::now(),
        };
        let chash = emit_consent(&engine, backend.as_ref(), &consent)
            .await
            .expect("directed consent emit must be admitted");
        assert_eq!(chash.len(), 64);

        // Idempotent re-emit returns the SAME content hash (no duplicate row).
        let chash2 = emit_consent(&engine, backend.as_ref(), &consent)
            .await
            .expect("idempotent consent re-emit");
        assert_eq!(chash, chash2, "consent re-emit must be idempotent");

        // Confirm the consent row is DIRECTED (subject = [A]) + scores-type.
        let cf = AttestationFilter {
            attesting_key_id: None,
            attested_key_id: Some("ciris-server-steward".into()),
            attestation_type: Some(super::ATTESTATION_TYPE_SCORES.to_owned()),
            pqc_completed: None,
            dimension_prefixes: vec!["consent:".into()],
            valid_at: None,
            confidence_floor: None,
            subject_key_id: None,
        };
        let cpage = backend
            .list_attestations(cf, None, 10, CallerScope::Unauthenticated)
            .await
            .expect("list consent");
        let consent_rows: Vec<_> = cpage
            .items
            .iter()
            .filter(|a| {
                a.attestation_envelope
                    .get("dimension")
                    .and_then(|v| v.as_str())
                    == Some(super::CONSENT_DIMENSION)
            })
            .collect();
        assert_eq!(
            consent_rows.len(),
            1,
            "idempotence: exactly one consent:replication:v1 row"
        );
        let row = consent_rows[0];

        // ── RC29 LOCKED shape (§5.6.8.15) ────────────────────────────────────
        // ENVELOPE-level:
        assert_eq!(
            row.subject_key_ids,
            vec!["ciris-server-steward".to_string()],
            "SINGLE subject = [A], directed/bilateral"
        );
        let envv = &row.attestation_envelope;
        assert_eq!(envv["cohort_scope"], "federation");
        assert_eq!(
            envv["witness_relation"], "self",
            "witness_relation = self is REQUIRED"
        );
        assert_eq!(envv["topical_relation"], "bilateral_pair");
        assert!(
            envv["score"].as_f64().unwrap() > 0.0,
            "positive grant score"
        );
        assert_eq!(
            envv["subject_key_ids"],
            serde_json::json!(["ciris-server-steward"]),
            "single subject_key_ids in the signed envelope"
        );
        // PAYLOAD-level (subject_kind = consent_replication):
        let payload = &envv["payload"];
        assert_eq!(payload["subject_kind"], "consent_replication");
        assert_eq!(payload["grants"], super::CONSENT_GRANTS_REPLICATION);
        assert_eq!(
            payload["attestation_prefixes"],
            serde_json::json!(["capacity:", "health:"]),
            "attestation_prefixes sorted-ascending + deduped"
        );
    }

    #[tokio::test]
    async fn peer_a_registration_admits_a_signed_capacity_into_roster() {
        let (engine, seeds) = node_b().await;
        let backend = engine.sqlite_backend().expect("sqlite backend").clone();

        const A_KEY: &str = "ciris-server-steward";
        const A_PUB: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
        const AGENT: &str = "agent-alpha";

        // Register A's key via the v8.8.0 gate from A's own self-signed record
        // (admission), plus the subject agent's key (a plain FK target).
        let a = node_a(&seeds, A_KEY).await;
        register_peer_key(&engine, &a, A_KEY).await;
        register_subject_key(&engine, AGENT, A_PUB).await;

        // Synthetic A-signed capacity:composite:v1 row about agent-alpha — exactly
        // the shape A's replicated `capacity:*` lands as (what the inbound bridge
        // would put_attestation). Admitted only because A's key is now registered.
        let valid_until = Utc::now() + chrono::Duration::hours(24);
        let envelope = serde_json::json!({
            "dimension": "capacity:composite:v1",
            "score": 0.87,
            "valid_until": valid_until.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            "witness_relation": "external",
        });
        let now = Utc::now();
        let att = Attestation {
            attestation_id: format!("a-cap-{}", now.timestamp_nanos_opt().unwrap_or(0)),
            attesting_key_id: A_KEY.into(),
            attested_key_id: AGENT.into(),
            attestation_type: super::ATTESTATION_TYPE_SCORES.to_owned(),
            weight: Some(0.95),
            asserted_at: now,
            expires_at: Some(valid_until),
            attestation_envelope: envelope,
            original_content_hash: "00".repeat(32),
            scrub_signature_classical: "AAAA".into(),
            scrub_signature_pqc: None,
            scrub_key_id: A_KEY.into(),
            scrub_timestamp: now,
            pqc_completed_at: None,
            persist_row_hash: String::new(),
            subject_key_ids: vec![AGENT.into()],
            withdraws_admission_rule: None,
            cohort_scope: cohort_scope::FEDERATION.to_owned(),
            tier: attestation_tier::FEDERATION.to_owned(),
            promoted_at: None,
        };
        backend
            .put_attestation(SignedAttestation { attestation: att })
            .await
            .expect("A-signed capacity:* must be admitted once A's key is registered");

        // It surfaces in B's public roster (Flow A read at the Unauthenticated scope).
        let roster =
            crate::roster::read::build_roster(backend.as_ref(), CallerScope::Unauthenticated)
                .await
                .expect("build_roster");
        let alpha = roster
            .agents
            .iter()
            .find(|a| a.key_id == AGENT)
            .expect("agent-alpha surfaced from replicated capacity:*");
        assert_eq!(alpha.capacity_composite, Some(0.87));
    }
}
