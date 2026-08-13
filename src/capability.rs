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

/// Routable classes: a kind whose members a router chooses between. A provider
/// of one of these is never an indispensable dependency of the thing reporting
/// it — the router has, by construction, somewhere else to go.
pub const ROUTABLE_KINDS: &[&str] = &["llm", "search"];

/// How a provider relates to the service that reports it.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Relation {
    /// Declared member of a capability. Excluded from its router's health; its
    /// failure shows through the capability rollup.
    Pooled,
    /// A routable provider belonging to no declared capability: monitored for
    /// visibility, in nobody's serving path. Excluded from its router's health
    /// AND from the headline, surfaced as an informational capability so it is
    /// never silently absent.
    Informational,
    /// Nothing else serves this. Its failure IS its router's failure.
    Indispensable,
}

/// Classify a provider. The three-way split matters because the two-way one is
/// wrong in both directions: treating every routable provider as pooled hides
/// failures nothing represents, and treating undeclared ones as indispensable
/// propagates a monitored-but-non-serving provider (Together, today) through
/// its router to the public headline — the exact false impairment this model
/// exists to remove.
pub fn relation(specs: &[CapabilitySpec], kind: Option<&str>, id: &str) -> Relation {
    if specs.iter().any(|s| s.members.iter().any(|(m, _)| m == id)) {
        Relation::Pooled
    } else if kind.is_some_and(|k| ROUTABLE_KINDS.contains(&k)) {
        Relation::Informational
    } else {
        Relation::Indispensable
    }
}

/// Does this provider stay OUT of its router's own health verdict?
pub fn is_pooled(specs: &[CapabilitySpec], kind: Option<&str>, id: &str) -> bool {
    !matches!(relation(specs, kind, id), Relation::Indispensable)
}

/// An informational capability: visible, never headline-moving.
pub fn informational(id: &str, status: &str) -> CapabilityStatus {
    let mut cap = singleton(id, status);
    cap.informational = true;
    cap
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
    } else if members.iter().any(|m| m.status == UNKNOWN) {
        // Nothing observed is up, but a declared member was never measured —
        // and it may be the one serving. DeepInfra is exactly this today. An
        // absence of evidence about the primary is not evidence of an outage.
        UNKNOWN
    } else {
        "major_outage"
    };

    CapabilityStatus {
        label: spec.label.clone(),
        status: status.to_string(),
        informational: false,
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
        informational: false,
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
        && cap.members.iter().any(|m| {
            // KNOWN unhealthy, not merely not-green. An unmeasured primary is
            // today's expected state for DeepInfra, and calling that "serving on
            // a fallback" asserts a routing change — with its cost, latency and
            // quality implications — on no evidence whatsoever.
            m.role == ROLE_PRIMARY && m.status != OPERATIONAL && m.status != UNKNOWN
        })
}

/// The headline, derived from capabilities rather than from whichever component
/// happens to be unhappiest.
pub fn overall(caps: &BTreeMap<String, CapabilityStatus>) -> &'static str {
    // Informational capabilities are reported and never counted: a provider in
    // nobody's serving path cannot impair service by definition.
    let considered: Vec<&str> = caps
        .values()
        .filter(|c| !c.informational)
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

    /// (6) Every OBSERVED member is down, but the primary was never measured.
    /// It may be serving; we do not know, and must not assert an outage.
    #[test]
    fn an_unmeasured_member_prevents_an_outage_verdict() {
        let cap = roll_up(
            &spec(1),
            // deepinfra absent entirely — exactly today's production shape.
            &observed(&[("openrouter", OUTAGE), ("groq", OUTAGE)]),
        );
        assert_eq!(cap.status, UNKNOWN, "no evidence about the primary");

        // Measure it and find it down too — NOW it is an outage.
        let cap = roll_up(
            &spec(1),
            &observed(&[
                ("deepinfra", OUTAGE),
                ("openrouter", OUTAGE),
                ("groq", OUTAGE),
            ]),
        );
        assert_eq!(cap.status, "major_outage");
    }

    /// (5) An UNMEASURED primary is not a failed one. Reporting fallback use
    /// here would assert a routing change on no evidence — and it is the baked
    /// configuration's expected state today.
    #[test]
    fn an_unmeasured_primary_is_not_a_fallback_event() {
        let cap = roll_up(
            &spec(1),
            &observed(&[("openrouter", OPERATIONAL), ("groq", OPERATIONAL)]),
        );
        assert_eq!(cap.status, OPERATIONAL);
        assert!(!on_fallback(&cap), "we do not know the primary is down");

        let cap = roll_up(
            &spec(1),
            &observed(&[
                ("deepinfra", OUTAGE),
                ("openrouter", OPERATIONAL),
                ("groq", OPERATIONAL),
            ]),
        );
        assert!(on_fallback(&cap), "now we know");
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

    /// The three-way split. Two-way is wrong in BOTH directions.
    #[test]
    fn providers_are_pooled_informational_or_indispensable() {
        let specs = default_specs();
        // In the serving chain → its capability represents it.
        assert_eq!(relation(&specs, Some("llm"), "groq"), Relation::Pooled);
        // Monitored, routable, in no declared chain → informational. Treating
        // it as indispensable propagated Together's dip to the headline.
        assert_eq!(
            relation(&specs, Some("llm"), "together"),
            Relation::Informational
        );
        assert_eq!(
            relation(&specs, Some("search"), "brave"),
            Relation::Informational
        );
        // Nothing else serves billing → its failure is the router's.
        assert_eq!(
            relation(&specs, Some("internal"), "billing"),
            Relation::Indispensable
        );
        assert_eq!(
            relation(&specs, None, "postgresql"),
            Relation::Indispensable
        );
    }

    /// Informational capabilities are visible but cannot move the headline.
    #[test]
    fn an_informational_capability_never_moves_the_headline() {
        let mut caps = BTreeMap::new();
        caps.insert(
            "provider.together".to_string(),
            informational("together", OUTAGE),
        );
        assert_eq!(overall(&caps), OPERATIONAL, "nothing was impaired");
        assert!(caps["provider.together"].informational);

        // A real capability in the same map still counts.
        caps.insert("region.us".to_string(), singleton("region.us", OUTAGE));
        assert_eq!(overall(&caps), "partial_outage");
    }

    #[test]
    fn only_indispensable_providers_count_against_their_router() {
        let specs = default_specs();
        assert!(is_pooled(&specs, Some("llm"), "groq"), "declared member");
        assert!(is_pooled(&specs, None, "deepinfra"), "declared member");
        // Routable but undeclared: out of the router's verdict, and surfaced as
        // an informational capability so it is never silently absent.
        assert!(is_pooled(&specs, Some("search"), "brave"));
        assert!(
            is_pooled(&specs, Some("llm"), "together"),
            "monitored, in nobody's serving path"
        );
        // Nothing else serves these, so their failure is the router's.
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
