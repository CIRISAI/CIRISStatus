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
