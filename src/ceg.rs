//! CEG `observation:reachability:v1` `scores` attestation shape + the Flow-B emit
//! (probe → signed observation).
//!
//! This is the novel piece the StatusAdapter contributes to the node: it turns
//! the cost-safe probe results into **first-class, signed, replicable federation
//! data**.
//!
//! # Why this is an observation and not a liveness claim
//!
//! We cannot sign "billing is alive" — we do not know it. We know that at T,
//! from this node, a request to billing's health endpoint answered (or did not).
//! First-person experience is what this key is entitled to bind, so the row's
//! subject is **the observation**, which is genuinely ours: `attester ==
//! attested` is correct here, and `witness_relation` is `self`.
//!
//! `health:liveness` is the third-person claim, and persist's family rule for it
//! is *witness_relation MUST be external — a service never attests its own
//! liveness (attester != attested)*. Emitting it about ourselves was admitted
//! only by a wrong-axis gate (`attestation_type` rather than the envelope
//! dimension), and v31 ships that ban fully built. Moving there properly needs
//! each service to be a registered federation key it signs for itself — see
//! `FSD/MULTI_VANTAGE.md` §2 D5 for the decision and what it costs.
//!
//! The node never speaks *as* the substrate either (`system:*` is reserved and
//! would be rejected at admission).
//!
//! The node itself (engine, signing key, self-registration, consent:replication
//! peering, and A<->B replication) is ciris-server's job — `serve_with_adapter`
//! already self-registers this node's signing key in the federation directory, so
//! the rows emitted here are authored under a key that's already admitted. This
//! module is just the envelope shape + the sign-and-put recipe, driven from the
//! adapter's `run_lifecycle` loop.

use serde::Serialize;
use serde_json::{json, Value};

use crate::probe::Probe;

/// The CEG dimension we emit on. Open-vocab leaf — verified against persist
/// v31.2.0 to be absent from `default_reserved_prefix_rules` (the reserved set
/// is `system:`, `audit_chain:`, `corpus_health:`, `identity_continuity:`,
/// `federation_directory:`, `transparency_log:cosigned:`), so no substrate role
/// is required to emit it. Versioned (`:v1`) to satisfy persist's default
/// `DimensionAdmissionPolicy { require_version_segment: true }` (admission.rs
/// §T3) so the emit survives a deployment that turns the admission gate on.
pub const DIMENSION: &str = "observation:reachability:v1";

/// `witness_relation` — we witnessed our OWN probe. Nothing validates this
/// field (no closed vocabulary, no gate), which is the reason to state it
/// accurately rather than a reason not to: `external` next to
/// `attester == attested` was false.
pub const WITNESS_RELATION_SELF: &str = "self";

/// `stake` — the monitor is reputationally accountable for its claims.
pub const STAKE_REPUTATIONAL: &str = "reputational";

/// CEG `attestation_type` for state claims (matches
/// `ciris_server::ciris_persist::federation::types::attestation_type::SCORES`).
pub const ATTESTATION_TYPE_SCORES: &str = "scores";

/// Map a component health string → the CEG `scores` value:
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

/// One piece of evidence behind an observation.
///
/// Since the cut-over to per-target rows, an observed target is a SUBJECT in its
/// own right (`observed`), so this is no longer the place non-keyed infra hides.
/// It carries what sat behind one target's verdict — the upstream's own opinion
/// of itself, the probe detail — rather than the whole fabric folded flat.
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

/// The full CEG `observation:reachability` `scores` envelope, **one per observed
/// target**. This is the canonical-signing payload (the JCS bytes signed).
///
/// One row per target rather than one folded row for the whole fabric: the old
/// shape named its targets only in a prose `context` and in `evidence_refs`, so
/// no consumer could ask the corpus what we had seen of *billing*. Per-subject
/// rows are also what `resolve_scores` folds per attester, which is the
/// mechanism a second vantage needs (`FSD/MULTI_VANTAGE.md` §4).
#[derive(Clone, Debug)]
pub struct ObservationEnvelope {
    /// **What we observed**, as a stable id in the same vocabulary the history
    /// tables and `EvidenceRef`s use: `service:us.billing`, `provider:groq`,
    /// `auth:google_oauth`. Not a federation key — these have none, which is
    /// precisely why the claim is first-person (see the module doc).
    pub observed: String,
    /// The endpoint we actually hit, when we hit one directly. Absent for a
    /// derivative observation, where we hit something else and it told us.
    pub endpoint: Option<String>,
    /// For a derivative observation: WHO told us, in the same id vocabulary.
    /// `epistemic_mode: derivative` without this is an unattributable rumour.
    pub via: Option<String>,
    /// `+1.0 | 0.0 | -1.0` (operational/degraded/outage).
    pub score: f64,
    /// How long it took, when we timed it ourselves. The measurement IS the
    /// observation — a score with no latency is a verdict with its evidence
    /// removed, and a second vantage disagreeing about a subject wants to
    /// compare these, not just the signs.
    pub latency_ms: Option<i64>,
    /// Probe certainty `[0,1]`.
    pub confidence: f64,
    /// Human detail for the target (e.g. `"US (Chicago) — billing"`).
    pub context: String,
    /// What sat behind this one target's verdict — the upstream's own opinion,
    /// the region, the probe. Evidence for THIS observation, not a dumping
    /// ground for the whole fabric as in the folded shape.
    pub evidence: Vec<EvidenceRef>,
    /// `now + observation cadence` (freshness; becomes the row's `expires_at`).
    ///
    /// This tracks `status.observation.poll_secs`, NOT the probe cadence: the
    /// signed plane is metered (`PeerWriteQuota`, 14,400 rows/day keyed on the
    /// author) and per-target rows at probe cadence would spend all of it. An
    /// expiry that outruns the emit cadence would be the worse error — a
    /// consumer would read a row as current after we stopped refreshing it.
    pub valid_until: chrono::DateTime<chrono::Utc>,
    /// When the observation was made. (`emit_attestation_self` stamps the row's
    /// `asserted_at` itself, CIRISStatus#31, so this is retained for the adapter's
    /// own bookkeeping / future envelope enrichment.)
    #[allow(dead_code)]
    pub asserted_at: chrono::DateTime<chrono::Utc>,
    pub epistemic_mode: EpistemicMode,
}

impl ObservationEnvelope {
    /// Build the `scores` envelope JSON — the exact object that gets
    /// JCS-canonicalized and hybrid-signed. Stable key set; numbers are plain
    /// JSON numbers (JCS-safe: small integers/one-dp confidences).
    ///
    /// No `vantage` member, deliberately (`FSD/MULTI_VANTAGE.md` §3):
    /// `attesting_key_id` IS the vantage, it is already a filter axis, and one
    /// node does not observe from several places. The temptation arrives with
    /// per-target rows and is refused on the same grounds.
    pub fn to_envelope(&self) -> Value {
        let mut v = json!({
            "dimension": DIMENSION,
            "observed": self.observed,
            "score": self.score,
            "confidence": self.confidence,
            "context": self.context,
            "evidence_refs": self.evidence,
            "valid_until": rfc3339(self.valid_until),
            "epistemic_mode": self.epistemic_mode.as_str(),
            "witness_relation": WITNESS_RELATION_SELF,
            "stake": STAKE_REPUTATIONAL,
        });
        // Absent rather than null: a JCS-canonicalized envelope is the signed
        // byte string, so an optional member that is sometimes `null` and
        // sometimes missing is two shapes for one claim.
        if let Some(e) = &self.endpoint {
            v["endpoint"] = json!(e);
        }
        if let Some(via) = &self.via {
            v["via"] = json!(via);
        }
        if let Some(ms) = self.latency_ms {
            v["latency_ms"] = json!(ms);
        }
        v
    }
}

fn rfc3339(t: chrono::DateTime<chrono::Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Map a `ProviderDetail.source` to the id of whoever told us.
///
/// `cirisproxy.us` → `service:us.proxy`. A `direct.*` source is not a teller at
/// all — we hit it ourselves — so it returns `None` and the caller emits a
/// `direct` observation.
fn via_id(source: &str) -> Option<String> {
    if source.starts_with("direct.") {
        return None;
    }
    let (who, region) = match source.split_once('.') {
        Some((w, r)) => (w, Some(r)),
        None => (source, None),
    };
    let svc = match who {
        "cirisproxy" => "proxy",
        "cirisbilling" => "billing",
        // Anything else keeps its own name rather than being forced into the
        // region.service shape — a wrong attribution is worse than a coarse one.
        other => return Some(format!("service:{other}")),
    };
    Some(match region {
        Some(r) => format!("service:{r}.{svc}"),
        None => format!("service:{svc}"),
    })
}

/// Turn one aggregated snapshot into the per-target observation envelopes to
/// sign — **the whole emit set for a cycle**, so its size is the thing to look
/// at when reasoning about the write quota (`Config::observation_seconds`).
///
/// Direct targets (we made the request) carry `endpoint` and
/// `epistemic_mode: direct`. Everything a service told us about its own
/// upstreams is `derivative` and carries `via`: we did not touch Groq, we
/// touched the proxy and it told us about Groq. Collapsing those two into one
/// claim is how a proxy outage reads as eight provider outages.
pub fn observation_envelopes(
    cfg: &crate::config::Config,
    agg: &crate::model::AggregatedStatus,
    now: chrono::DateTime<chrono::Utc>,
    valid_until: chrono::DateTime<chrono::Utc>,
) -> Vec<ObservationEnvelope> {
    let mut out = Vec::new();
    let mut push = |observed: String,
                    endpoint: Option<String>,
                    via: Option<String>,
                    status: &str,
                    latency_ms: Option<i64>,
                    context: String,
                    evidence: Vec<EvidenceRef>| {
        let epistemic_mode = if via.is_some() {
            EpistemicMode::Derivative
        } else {
            EpistemicMode::Direct
        };
        out.push(ObservationEnvelope {
            observed,
            endpoint,
            via,
            score: liveness_score(status),
            latency_ms,
            // A derivative claim is worth less than one we made ourselves, and
            // saying so is the honest use of the field.
            confidence: if epistemic_mode == EpistemicMode::Direct {
                0.9
            } else {
                0.7
            },
            context,
            evidence,
            valid_until,
            asserted_at: now,
            epistemic_mode,
        });
    };

    // ── Direct: the region services we probe ourselves. ──
    for (region_key, region) in &agg.regions {
        let spec = cfg.regions.iter().find(|r| r.key == region_key);
        for (svc, summ) in &region.services {
            let endpoint = spec.and_then(|r| match svc.as_str() {
                "billing" => r.billing_url.clone(),
                "proxy" => r.proxy_url.clone(),
                _ => None,
            });
            // The service's own opinion of itself, kept as evidence rather than
            // folded into our verdict — it counts pooled providers we do not.
            let evidence = summ
                .upstream_status
                .as_ref()
                .map(|u| {
                    vec![EvidenceRef {
                        ref_id: format!("upstream:{region_key}.{svc}"),
                        status: u.clone(),
                        latency_ms: summ.latency_ms,
                        detail: Some("the service's own verdict".into()),
                    }]
                })
                .unwrap_or_default();
            push(
                format!("service:{region_key}.{svc}"),
                endpoint,
                None,
                &summ.status,
                summ.latency_ms,
                format!("{} — {}", region.name, summ.name),
                evidence,
            );
        }
    }

    // ── Direct: infrastructure (region hosts + the container registry). ──
    for (key, infra) in &agg.infrastructure {
        let endpoint = if key == "github" {
            Some(cfg.ghcr_url.clone())
        } else {
            cfg.regions
                .iter()
                .find(|r| r.infra_provider == key)
                .and_then(|r| r.infra_url.clone())
        };
        push(
            format!("infra:{key}"),
            endpoint,
            None,
            &infra.status,
            infra.latency_ms,
            infra.name.clone(),
            Vec::new(),
        );
    }

    // ── Providers, direct or reported, each keeping its own provenance. ──
    let provider_sets: [(
        &str,
        &std::collections::BTreeMap<String, crate::model::ProviderDetail>,
    ); 4] = [
        ("auth", &agg.auth_providers),
        ("provider", &agg.llm_providers),
        ("provider", &agg.internal_providers),
        ("database", &agg.database_providers),
    ];
    for (prefix, set) in provider_sets {
        for (name, d) in set {
            let source = d.source.clone().unwrap_or_default();
            let via = via_id(&source);
            let endpoint = if via.is_none() {
                cfg.auth_targets
                    .iter()
                    .find(|(k, _)| k == name)
                    .map(|(_, u)| u.clone())
                    .or_else(|| {
                        cfg.external
                            .iter()
                            .find(|e| e.key == name)
                            .map(|e| e.url.clone())
                    })
            } else {
                None
            };
            push(
                format!("{prefix}:{name}"),
                endpoint,
                via,
                &d.status,
                d.latency_ms,
                source,
                Vec::new(),
            );
        }
    }

    out
}

// ─────────────────────────────────────────────────────────────────────────────
// Flow B emit — REAL signing + emission via the node's shared persist Engine.
// The node's signing key is already self-registered by ciris-server's
// `serve_with_adapter`, so a row authored here passes the attesting-key gate.
// ─────────────────────────────────────────────────────────────────────────────

use anyhow::Result;
use ciris_server::ciris_persist::prelude::Engine;
// v9.0.0 federation-tier ingest gate (CC 5.3.2.4.3.1) re-derives the signed
// canonical bytes via `ceg_produce_canonicalize` (the PRODUCE-side JCS gate)
// and cross-checks `SHA-256(canonical) == original_content_hash` before a
// Strict hybrid-verify. Emit MUST sign over THESE bytes.

/// Sign + emit ONE `observation:reachability` `scores` attestation — one
/// observed target — via persist's `emit_attestation_self` (CIRISStatus#31),
/// returning the row's `attestation_id`.
///
/// `engine` is the node's shared persist `Engine` (from
/// [`ciris_server::AdapterContext::engine`]); the federation directory the row is
/// written to is `engine.sqlite_backend()`. `key_id` is the node's federation
/// `key_id` (the attesting steward identity ciris-server already self-registered)
/// — passed for logging/clarity; the actual attesting key comes from the engine's
/// current signer alias.
///
/// Recipe mirrors persist's native produce path AND the v9.0.0 federation-tier
/// ingest gate, which re-derives + verifies against EXACTLY these bytes:
///   1. build the envelope JSON ([`ObservationEnvelope::to_envelope`]),
///   2. JCS-canonicalize it via the PRODUCE gate (`ceg_produce_canonicalize`),
///   3. `original_content_hash = hex(SHA-256(canonical))`,
///   4. `Engine::sign_hybrid(canonical)` → Ed25519 + ML-DSA-65 (base64),
///   5. assemble a federation-tier [`Attestation`] and `put_attestation`.
pub async fn emit_observation(
    engine: &Engine,
    key_id: &str,
    env: &ObservationEnvelope,
) -> Result<String> {
    let _ = key_id;
    // CIRISStatus#31 — emit via persist's `Engine::emit_attestation_self`
    // (CIRISPersist#248) rather than hand-rolling the row. The old path set
    // `attesting_key_id`/`scrub_key_id = engine.signer().current_alias()` — the
    // RAW keystore alias `ciris-status-1` — but `serve_with_adapter` enrolls this
    // node into `federation_keys` under the DERIVED key_id `ciris-status-1-<fp>`
    // (CIRISServer#27). So every liveness emit was refused with
    // "ciris-status-1 does not exist in federation_keys". `emit_attestation_self`
    // derives `attesting_key_id`/`scrub_key_id` internally from the engine's own
    // composed signer (the #247 floor — never a caller alias), canonicalizes +
    // hybrid-signs, and assembles the federation-tier row — so it CANNOT pick the
    // raw form.
    //
    // `attested_key_id = None` defaults it to the same derived self key, and here
    // that is the CORRECT subject rather than a tolerated one: the row asserts an
    // observation, and the observation is ours. The thing observed has no
    // federation key — which is exactly why the claim is first-person — and it
    // rides the envelope's `observed` member.
    use ciris_server::ciris_persist::federation::EmitAttestationInput;

    // persist #519/#527 added an explicit write-side cohort_scope: write and read
    // must not share one default. `federation` is correct here for the same reason
    // the server's capacity scorer uses it — a liveness score is a REPUTATIONAL
    // claim published so peers can read it. Defaulting this to `self` would leave
    // every status score born (self, local) and unpromotable, which is precisely
    // how the trace plane shipped zero rows for eight releases while staying green.
    //
    // `subject_key_ids` stays EMPTY. It is not a label slot for the observed
    // target: under CIRISPersist#643 a canonical binding hash there confers
    // revocation authority (`resolve_withdraws_admission_rule` rule 2), and the
    // previous shape put our own key in it, which named a subject that added
    // nothing and claimed authority over ourselves.
    let mut input = EmitAttestationInput::with_envelope(
        ATTESTATION_TYPE_SCORES,
        ciris_server::ciris_persist::federation::envelope::EnvelopeCore::from_value(
            env.to_envelope(),
        )?,
        ciris_server::ciris_persist::federation::types::cohort_scope::FEDERATION,
    )
    .with_weight(Some(env.confidence));
    input.expires_at = Some(env.valid_until);

    engine
        .emit_attestation_self(input)
        .await
        .map_err(|e| anyhow::anyhow!("emit_attestation_self({DIMENSION}): {e}"))
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

    fn at(s: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(s)
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    fn direct(observed: &str) -> ObservationEnvelope {
        ObservationEnvelope {
            observed: observed.into(),
            endpoint: Some("https://billing.example/health".into()),
            via: None,
            score: 1.0,
            latency_ms: Some(84),
            confidence: 0.9,
            context: "US (Chicago) — billing".into(),
            evidence: vec![EvidenceRef {
                ref_id: "probe:us.billing".into(),
                status: "operational".into(),
                latency_ms: Some(120),
                detail: None,
            }],
            valid_until: at("2026-06-16T00:05:00Z"),
            asserted_at: at("2026-06-16T00:00:00Z"),
            epistemic_mode: EpistemicMode::Direct,
        }
    }

    #[test]
    fn envelope_names_what_was_observed() {
        let v = direct("service:us.billing").to_envelope();
        assert_eq!(v["dimension"], DIMENSION);
        assert_eq!(v["score"], 1.0);
        assert_eq!(v["stake"], STAKE_REPUTATIONAL);
        assert_eq!(v["epistemic_mode"], "direct");
        // The point of the cut-over: the target is machine-readable, so a
        // consumer can ask the corpus what we saw of billing. The folded
        // `health:liveness` shape named it only in prose.
        assert_eq!(v["observed"], "service:us.billing");
        assert_eq!(v["endpoint"], "https://billing.example/health");
        assert!(v["valid_until"].is_string());
    }

    #[test]
    fn witness_relation_is_self_because_the_observation_is_ours() {
        // `external` next to attester == attested was false. Nothing validates
        // this field, which is why the test does.
        let v = direct("service:us.billing").to_envelope();
        assert_eq!(v["witness_relation"], WITNESS_RELATION_SELF);
        assert_eq!(WITNESS_RELATION_SELF, "self");
    }

    #[test]
    fn no_vantage_member_until_one_node_observes_from_two_places() {
        // FSD/MULTI_VANTAGE.md §3: `attesting_key_id` IS the vantage. Pinned
        // because per-target rows are exactly when someone reaches for it.
        let v = direct("service:us.billing").to_envelope();
        assert!(v.get("vantage").is_none(), "no vantage member: {v}");
        assert!(v.get("observer").is_none(), "no observer member: {v}");
    }

    #[test]
    fn optional_members_are_absent_not_null() {
        // The envelope IS the signed byte string. A member that is sometimes
        // `null` and sometimes missing is two shapes for one claim.
        let mut env = direct("provider:groq");
        env.endpoint = None;
        env.via = Some("service:us.proxy".into());
        env.epistemic_mode = EpistemicMode::Derivative;
        let v = env.to_envelope();
        assert!(v.get("endpoint").is_none(), "endpoint must be absent: {v}");
        assert_eq!(v["via"], "service:us.proxy");
        assert_eq!(v["epistemic_mode"], "derivative");
    }

    // ── The snapshot → envelope-set builder ──────────────────────────────────
    fn snapshot() -> crate::model::AggregatedStatus {
        use crate::model::*;
        use std::collections::BTreeMap;
        let mut services = BTreeMap::new();
        services.insert(
            "billing".to_string(),
            ServiceSummary {
                name: "Billing & Authentication".into(),
                status: OPERATIONAL.into(),
                latency_ms: Some(84),
                upstream_status: Some(DEGRADED.into()),
            },
        );
        services.insert(
            "proxy".to_string(),
            ServiceSummary {
                name: "LLM Proxy".into(),
                status: OPERATIONAL.into(),
                latency_ms: Some(90),
                upstream_status: None,
            },
        );
        let mut regions = BTreeMap::new();
        regions.insert(
            "us".to_string(),
            RegionStatus {
                name: "US (Chicago)".into(),
                status: OPERATIONAL.into(),
                services,
            },
        );
        let mut infrastructure = BTreeMap::new();
        infrastructure.insert(
            "github".to_string(),
            InfrastructureStatus {
                name: "Container Registry".into(),
                status: OPERATIONAL.into(),
                provider: "github".into(),
                latency_ms: Some(210),
            },
        );
        let mut llm = BTreeMap::new();
        llm.insert(
            "groq".to_string(),
            ProviderDetail {
                status: OUTAGE.into(),
                latency_ms: None,
                source: Some("cirisproxy.us".into()),
            },
        );
        let mut auth = BTreeMap::new();
        auth.insert(
            "google_oauth".to_string(),
            ProviderDetail {
                status: OPERATIONAL.into(),
                latency_ms: Some(120),
                source: Some("direct.google_oauth".into()),
            },
        );
        AggregatedStatus {
            status: OPERATIONAL.into(),
            indicator: indicator_for(OPERATIONAL),
            capabilities: BTreeMap::new(),
            vantage_failure: false,
            timestamp: "2026-06-16T00:00:00Z".into(),
            age_seconds: 0,
            stale: false,
            last_incident: None,
            regions,
            infrastructure,
            llm_providers: llm,
            auth_providers: auth,
            database_providers: BTreeMap::new(),
            internal_providers: BTreeMap::new(),
        }
    }

    fn built() -> Vec<ObservationEnvelope> {
        let mut cfg = crate::config::Config::defaults(String::new());
        cfg.auth_targets = vec![(
            "google_oauth".to_string(),
            "https://oauth2.googleapis.com/tokeninfo".to_string(),
        )];
        if let Some(us) = cfg.regions.iter_mut().find(|r| r.key == "us") {
            us.billing_url = Some("https://billing.example/health".into());
            us.proxy_url = Some("https://proxy.example/v1/status".into());
        }
        observation_envelopes(
            &cfg,
            &snapshot(),
            at("2026-06-16T00:00:00Z"),
            at("2026-06-16T00:05:00Z"),
        )
    }

    fn find<'a>(v: &'a [ObservationEnvelope], observed: &str) -> &'a ObservationEnvelope {
        v.iter()
            .find(|e| e.observed == observed)
            .unwrap_or_else(|| panic!("no envelope for {observed}"))
    }

    #[test]
    fn every_observed_target_gets_its_own_row() {
        let envs = built();
        for want in [
            "service:us.billing",
            "service:us.proxy",
            "infra:github",
            "provider:groq",
            "auth:google_oauth",
        ] {
            let _ = find(&envs, want);
        }
        assert_eq!(envs.len(), 5, "one row per target, no folding");
    }

    #[test]
    fn what_we_probed_is_direct_and_carries_its_endpoint() {
        let envs = built();
        let billing = find(&envs, "service:us.billing");
        assert_eq!(billing.epistemic_mode, EpistemicMode::Direct);
        assert_eq!(
            billing.endpoint.as_deref(),
            Some("https://billing.example/health")
        );
        assert_eq!(billing.via, None);
        assert_eq!(billing.latency_ms, Some(84));
        // The service's own verdict is kept as evidence, not folded into ours:
        // it counts pooled providers we deliberately do not.
        assert_eq!(billing.evidence.len(), 1);
        assert_eq!(billing.evidence[0].status, crate::model::DEGRADED);
    }

    #[test]
    fn what_a_service_told_us_is_derivative_and_names_the_teller() {
        // We never touched Groq. We touched the proxy, and it told us. Merging
        // those into one first-person claim is how one proxy outage reads as
        // eight independent provider outages.
        let envs = built();
        let groq = find(&envs, "provider:groq");
        assert_eq!(groq.epistemic_mode, EpistemicMode::Derivative);
        assert_eq!(groq.via.as_deref(), Some("service:us.proxy"));
        assert_eq!(groq.endpoint, None);
        assert!(
            groq.confidence < find(&envs, "service:us.billing").confidence,
            "hearsay must not be worth as much as a measurement we made"
        );
    }

    #[test]
    fn a_directly_probed_provider_is_not_hearsay() {
        // 0.3.45 started probing the identity providers ourselves precisely so
        // their health stopped being a value lifted out of billing's report.
        let envs = built();
        let google = find(&envs, "auth:google_oauth");
        assert_eq!(google.epistemic_mode, EpistemicMode::Direct);
        assert_eq!(google.via, None);
        assert_eq!(
            google.endpoint.as_deref(),
            Some("https://oauth2.googleapis.com/tokeninfo")
        );
    }

    #[test]
    fn via_maps_a_source_to_the_teller_and_refuses_to_guess() {
        assert_eq!(via_id("cirisproxy.eu").as_deref(), Some("service:eu.proxy"));
        assert_eq!(
            via_id("cirisbilling.us").as_deref(),
            Some("service:us.billing")
        );
        // Not ours to reshape: keep the name we were given rather than forcing
        // it into region.service and attributing the claim to the wrong node.
        assert_eq!(via_id("cirislens").as_deref(), Some("service:cirislens"));
        // We hit it ourselves — there is no teller.
        assert_eq!(via_id("direct.google_oauth"), None);
    }

    #[test]
    fn a_cycle_fits_inside_the_write_quota() {
        // persist charges PeerWriteQuota per put, keyed on the AUTHOR:
        // 14_400 rows/day. At the 300s default that is 288 cycles/day, so a
        // cycle has ~50 rows of room. This test exists so that adding targets
        // is a decision rather than an accident.
        const SUSTAINED_ROWS_PER_DAY: usize = 14_400;
        const DEFAULT_OBSERVATION_SECS: usize = 300;
        let cycles_per_day = 86_400 / DEFAULT_OBSERVATION_SECS;
        let per_cycle = built().len();
        assert!(
            per_cycle * cycles_per_day < SUSTAINED_ROWS_PER_DAY / 2,
            "{per_cycle} rows/cycle × {cycles_per_day} cycles leaves no headroom for a peer"
        );
    }

    #[test]
    fn a_derivative_observation_names_who_told_us() {
        // Second-hand knowledge that cannot say whose it is cannot be checked
        // against the teller later.
        let mut env = direct("provider:groq");
        env.epistemic_mode = EpistemicMode::Derivative;
        env.via = Some("service:us.proxy".into());
        let v = env.to_envelope();
        assert_eq!(v["epistemic_mode"], "derivative");
        assert!(v["via"].is_string());
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Flow B — REAL probe→emit sign path proof. Builds an `ObservationEnvelope`,
// JCS-canonicalizes via the PRODUCE gate, hybrid-signs, and `put_attestation`s a
// federation-tier `observation:reachability:v1` row via `emit_observation`. Mirrors the node
// runtime: the attesting (node) key must be self-registered first (what
// `serve_with_adapter` does at boot) before the row is admissible.
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod flow_b_emit {
    use super::*;

    use ciris_server::ciris_persist::federation::Error as FederationError;
    use ciris_server::ciris_persist::prelude::{Engine, LocalSigner, LocalSignerConfig};

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

    async fn node(key_id: &str) -> (std::sync::Arc<Engine>, SeedDir) {
        let seeds = SeedDir::new();
        let ed = seeds.seed("ed.seed", [0x42; 32]);
        let pqc = seeds.seed("pqc.seed", [0x77; 32]);
        let signer = std::sync::Arc::new(
            LocalSigner::from_config(&LocalSignerConfig {
                key_id: key_id.into(),
                key_path: ed,
                pqc_key_id: Some(format!("{key_id}-pqc")),
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

    /// Self-register the node's witness key (what ciris-server does at boot) so
    /// `emit_observation` rows pass the attesting-key gate.
    async fn register_self_key(engine: &Engine, key_id: &str) {
        // CIRISStatus#31 — register the engine's OWN DERIVED key_id (what
        // `serve_with_adapter` does in production and what `emit_attestation_self`
        // attests under), NOT the raw label. `register_self_federation_key`
        // derives the key_id internally from the composed signer, so this test
        // now mirrors the real enrollment the emit relies on.
        let _ = key_id;
        match engine
            .register_self_federation_key(
                "witness",
                key_id,
                None,
                serde_json::json!({}),
                Vec::new(),
            )
            .await
        {
            Ok(_) | Err(FederationError::Conflict(_)) => {}
            Err(e) => panic!("self-register witness key: {e}"),
        }
    }

    fn sample_env(observed: &str) -> ObservationEnvelope {
        ObservationEnvelope {
            observed: observed.into(),
            endpoint: Some("https://billing.example/health".into()),
            via: None,
            score: liveness_score(crate::model::OPERATIONAL),
            latency_ms: Some(84),
            confidence: 0.9,
            context: "US (Chicago) — billing".into(),
            evidence: vec![EvidenceRef {
                ref_id: "probe:us.billing".into(),
                status: "operational".into(),
                latency_ms: Some(120),
                detail: None,
            }],
            valid_until: chrono::Utc::now() + chrono::Duration::seconds(300),
            asserted_at: chrono::Utc::now(),
            epistemic_mode: EpistemicMode::Direct,
        }
    }

    #[tokio::test]
    async fn self_registration_admits_a_signed_observation() {
        const NODE: &str = "ciris-status-monitor";
        let (engine, _seeds) = node(NODE).await;

        // Before self-registration the attesting key is absent → emit rejected.
        let env = sample_env("service:us.billing");
        let before = emit_observation(&engine, NODE, &env).await;
        assert!(
            before.is_err(),
            "without self-registration the attesting key is absent → emit must be rejected"
        );

        // Self-register, then the emit is admissible. attester == attested is
        // the node attesting its OWN observation, which is what it witnessed.
        register_self_key(&engine, NODE).await;
        let hash = emit_observation(&engine, NODE, &env)
            .await
            .expect("after self-registration, the observation must be admitted");
        assert!(
            !hash.is_empty(),
            "emit_attestation_self returns the attestation_id"
        );
    }

    /// The reason for the cut-over, as a test: `observation:` is unreserved, so
    /// the substrate admits it from a key holding no substrate role. If a future
    /// persist reserves the prefix, this fails here rather than in production.
    #[tokio::test]
    async fn the_observation_prefix_needs_no_substrate_role() {
        const NODE: &str = "ciris-status-unprivileged";
        let (engine, _seeds) = node(NODE).await;
        register_self_key(&engine, NODE).await;

        // `witness` identity_type, no infra:* capability roles — the same
        // standing a monitor actually has.
        emit_observation(&engine, NODE, &sample_env("provider:groq"))
            .await
            .expect("observation:* is open vocabulary — no reserved-prefix gate");
    }

    #[tokio::test]
    async fn degraded_and_outage_map_to_zero_and_negative() {
        let mut env = sample_env("service:us.billing");
        env.score = liveness_score(crate::model::DEGRADED);
        assert_eq!(env.to_envelope()["score"], 0.0);
        env.score = liveness_score(crate::model::OUTAGE);
        assert_eq!(env.to_envelope()["score"], -1.0);
    }
}
