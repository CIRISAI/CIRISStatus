//! Capabilities — a thing the fabric can *do*, and how many members must be up
//! for it to work. See `FSD/CAPABILITY_MONITORING.md`.
//!
//! The model is Vigil's (`min_replicas_available`) rather than best-of: with a
//! threshold you can say "three providers, at least two available", which
//! expresses the state best-of cannot — still serving, but one failure from
//! dark. Best-of calls that healthy right up until the moment it is not.
//!
//! **Regions are not a capability pool.** The fabric is active/active, but a
//! regional outage is a regional outage: EU users are not served by US being
//! healthy. Only providers behind a common router pool.

use std::collections::BTreeMap;

use crate::model::{
    severity, CapabilityMember, CapabilityStatus, DEGRADED, OPERATIONAL, ROLE_FALLBACK,
    ROLE_PRIMARY, UNKNOWN,
};

/// A declared pool: what SHOULD be measured, so that something unmeasured shows
/// as `unknown` instead of being absent and therefore invisibly fine.
#[derive(Clone, Debug)]
pub struct CapabilitySpec {
    pub id: String,
    pub label: String,
    /// `(member id, is_primary)` in call-path order.
    pub members: Vec<(String, bool)>,
    pub min_available: usize,
}

/// The AI pool as the proxy actually routes it: DeepInfra serves by default,
/// the rest are fallbacks. DeepInfra is NOT currently health-checked by
/// CIRISProxy — it will render `unknown` here, which is the honest picture of
/// "we are not measuring the thing that serves" and is meant to look wrong.
pub fn default_specs() -> Vec<CapabilitySpec> {
    vec![CapabilitySpec {
        id: "ai_providers".into(),
        label: "AI providers".into(),
        // The DEFAULT ROUTING CHAIN, not the monitored set. Together is
        // monitored but is not in it, and counting it as an available fallback
        // would let `ai_providers` read operational with every serving provider
        // down — the exact confusion this whole model exists to remove.
        members: vec![
            ("deepinfra".into(), true),
            ("openrouter".into(), false),
            ("groq".into(), false),
        ],
        min_available: 1,
    }]
}

/// Is this provider a member of some declared capability, and therefore NOT
/// part of its router's own health? A pooled member must not degrade the
/// service that reports it — that service is serving fine on the others.
///
/// Membership is the ONLY thing that excludes. Excluding by `kind` (every
/// `llm`/`search` provider) removed providers from their router's verdict
/// without giving them a capability to live in, so if every search provider
/// failed, the proxy, the region, the capabilities and the headline all stayed
/// green while search was unavailable. Exclusion without representation is a
/// blind spot: declare a capability, or the provider counts against its router.
pub fn is_pooled(specs: &[CapabilitySpec], _kind: Option<&str>, id: &str) -> bool {
    specs.iter().any(|s| s.members.iter().any(|(m, _)| m == id))
}

/// Roll a spec up against what was actually observed.
///
/// `observed` maps member id → status. A declared member that is absent is
/// `unknown` and counts as unavailable: we do not get to call a capability
/// healthy on the strength of members we never looked at.
pub fn roll_up(spec: &CapabilitySpec, observed: &BTreeMap<String, String>) -> CapabilityStatus {
    let members: Vec<CapabilityMember> = spec
        .members
        .iter()
        .map(|(id, primary)| CapabilityMember {
            id: id.clone(),
            role: if *primary {
                ROLE_PRIMARY
            } else {
                ROLE_FALLBACK
            },
            status: observed
                .get(id)
                .cloned()
                .unwrap_or_else(|| UNKNOWN.to_string()),
        })
        .collect();

    let available = members.iter().filter(|m| m.status == OPERATIONAL).count();

    let status = if members.is_empty() {
        UNKNOWN
    } else if available >= spec.min_available.max(1) {
        OPERATIONAL
    } else if available > 0 {
        // Serving, but the margin is gone. Not an outage; not "fine" either.
        DEGRADED
    } else {
        "major_outage"
    };

    CapabilityStatus {
        label: spec.label.clone(),
        status: status.to_string(),
        min_available: spec.min_available.max(1),
        available,
        members,
    }
}

/// A capability with exactly one member and no redundancy — regions,
/// infrastructure, anything nothing else can serve for.
pub fn singleton(label: &str, status: &str) -> CapabilityStatus {
    CapabilityStatus {
        label: label.to_string(),
        status: status.to_string(),
        min_available: 1,
        available: usize::from(status == OPERATIONAL),
        members: vec![CapabilityMember {
            id: label.to_string(),
            role: ROLE_PRIMARY,
            status: status.to_string(),
        }],
    }
}

/// Is the capability serving, but not on its primary? Worth an event, never a
/// headline change.
pub fn on_fallback(cap: &CapabilityStatus) -> bool {
    cap.status == OPERATIONAL
        && cap
            .members
            .iter()
            .any(|m| m.role == ROLE_PRIMARY && m.status != OPERATIONAL)
}

/// The headline, derived from capabilities rather than from whichever component
/// happens to be unhappiest.
pub fn overall(caps: &BTreeMap<String, CapabilityStatus>) -> &'static str {
    let considered: Vec<&str> = caps
        .values()
        .map(|c| c.status.as_str())
        .filter(|s| *s != UNKNOWN)
        .collect();
    let outages = considered.iter().filter(|s| severity(s) >= 2).count();
    let degraded = considered.iter().any(|s| severity(s) == 1);
    if outages >= 3 {
        "major_outage"
    } else if outages > 0 {
        "partial_outage"
    } else if degraded {
        DEGRADED
    } else {
        OPERATIONAL
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::OUTAGE;

    fn spec(min: usize) -> CapabilitySpec {
        CapabilitySpec {
            id: "ai_providers".into(),
            label: "AI providers".into(),
            members: vec![
                ("deepinfra".into(), true),
                ("openrouter".into(), false),
                ("groq".into(), false),
            ],
            min_available: min,
        }
    }

    fn observed(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// D1: the 2026-08-10..13 shape. One pool member degraded, others serving.
    /// Nothing was impaired, so nothing may report impairment.
    #[test]
    fn one_degraded_member_does_not_impair_the_capability() {
        let cap = roll_up(
            &spec(1),
            &observed(&[
                ("deepinfra", OPERATIONAL),
                ("openrouter", OPERATIONAL),
                ("groq", DEGRADED),
            ]),
        );
        assert_eq!(cap.status, OPERATIONAL);
        assert_eq!(cap.available, 2);
        assert_eq!(
            overall(&[("ai".to_string(), cap)].into_iter().collect()),
            OPERATIONAL
        );
    }

    /// D1: but a simultaneous failure of every member is a real outage.
    #[test]
    fn losing_every_member_is_an_outage() {
        let cap = roll_up(
            &spec(1),
            &observed(&[
                ("deepinfra", OUTAGE),
                ("openrouter", OUTAGE),
                ("groq", DEGRADED),
            ]),
        );
        assert_eq!(cap.status, "major_outage");
        assert_eq!(cap.available, 0);
    }

    /// The state best-of cannot express: still serving, but the margin is gone.
    #[test]
    fn below_the_threshold_is_degraded_not_healthy() {
        let cap = roll_up(
            &spec(2),
            &observed(&[
                ("deepinfra", OPERATIONAL),
                ("openrouter", OUTAGE),
                ("groq", OUTAGE),
            ]),
        );
        assert_eq!(cap.status, DEGRADED, "one of three left, threshold is two");
        assert_eq!(cap.available, 1);
    }

    /// D2: a declared member nobody measured is `unknown` and counts as
    /// unavailable. Silence is not health.
    #[test]
    fn an_unmeasured_member_is_unknown_and_unavailable() {
        // Exactly today's production shape: DeepInfra serves and is not checked.
        let cap = roll_up(
            &spec(2),
            &observed(&[("openrouter", OPERATIONAL), ("groq", OPERATIONAL)]),
        );
        let primary = cap.members.iter().find(|m| m.role == ROLE_PRIMARY).unwrap();
        assert_eq!(primary.id, "deepinfra");
        assert_eq!(primary.status, UNKNOWN);
        assert_eq!(
            cap.available, 2,
            "unknown members are not counted available"
        );
        assert_eq!(cap.status, OPERATIONAL);
    }

    /// Serving on a fallback: no headline change, but it is reported.
    #[test]
    fn serving_on_a_fallback_is_visible_without_being_an_outage() {
        let cap = roll_up(
            &spec(1),
            &observed(&[
                ("deepinfra", OUTAGE),
                ("openrouter", OPERATIONAL),
                ("groq", OPERATIONAL),
            ]),
        );
        assert_eq!(cap.status, OPERATIONAL);
        assert!(on_fallback(&cap), "the primary is down; say so");
    }

    #[test]
    fn only_declared_members_are_excluded_from_their_router() {
        let specs = default_specs();
        assert!(is_pooled(&specs, Some("llm"), "groq"));
        assert!(is_pooled(&specs, None, "deepinfra"), "declared, so pooled");
        // NOT declared in any capability: it counts against its router, because
        // otherwise nothing anywhere would report its failure.
        assert!(!is_pooled(&specs, Some("search"), "brave"));
        assert!(
            !is_pooled(&specs, Some("llm"), "together"),
            "monitored, not in the chain"
        );
        // The router's OWN dependencies are not pooled — nothing else serves them.
        assert!(!is_pooled(&specs, Some("internal"), "billing"));
        assert!(!is_pooled(&specs, None, "postgresql"));
    }

    /// (1) The pool describes what SERVES. A healthy provider outside the
    /// routing chain must not satisfy the threshold.
    #[test]
    fn a_provider_outside_the_chain_cannot_satisfy_the_threshold() {
        let spec = &default_specs()[0];
        assert!(
            !spec.members.iter().any(|(m, _)| m == "together"),
            "together is monitored but not in the default chain"
        );
        let cap = roll_up(
            spec,
            &observed(&[
                ("deepinfra", OUTAGE),
                ("openrouter", OUTAGE),
                ("groq", OUTAGE),
                ("together", OPERATIONAL),
            ]),
        );
        assert_eq!(cap.status, "major_outage", "no usable default route");
    }

    #[test]
    fn an_empty_capability_is_unknown_never_green() {
        let cap = roll_up(
            &CapabilitySpec {
                id: "x".into(),
                label: "x".into(),
                members: vec![],
                min_available: 1,
            },
            &observed(&[]),
        );
        assert_eq!(cap.status, UNKNOWN);
    }
}
