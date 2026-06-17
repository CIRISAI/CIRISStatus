# ciris-status as a fabric monitoring node — design + ship plan

> **Status:** design / build spec. This is the spec the build follows.
> **Thesis:** `monitoring agent = fabric node + monitoring cards` — the same
> shape as `agent = fabric node + brain` (CIRISServer#15). ciris-status stops
> being an out-of-band prober and becomes **a fabric node specialized as the
> public mesh-monitoring agent**: it reads the mesh's signed `scores`, aggregates
> them into the live surface ciris.ai displays, and attests CIRIS service health
> back into the fabric as signed CEG.

## 0. Why this shape

- **The generic fabric node stays minimal.** It exposes only *required* public
  hooks (`/v1/identity`, `/health` probe target, replication, and **signed /
  key-scoped** reads). It is **not** a public website oracle.
- **The opinionated public surface is concentrated in one purpose-built node**
  that earns the right to it by being a fabric participant — it verifies signed
  mesh data natively rather than scraping.
- **Singleton-by-role, not by-dependency.** The federation runs without
  ciris-status; you may run several (US/EU); aggregates converge because they
  derive from signed data. It is the public *window*, never load-bearing.

## 1. The primitive: it's all `scores` (CEG 1+4)

There is **no new statement type.** CEG has exactly one workhorse attestation
primitive — `scores` — and §3.1 says it covers claims about an entity's *state*.
Everything ciris-status touches is a `scores` attestation on a dimension:

| What | Dimension | Notes |
|---|---|---|
| Per-agent CIRIS score (the website's "key_id + score") | **`capacity:*`** — `capacity:composite` (𝒞_CIRIS) + factors `capacity:core_identity` (C), `:integrity` (I_int), `:resilience` (R), `:incompleteness_awareness` (I_inc), `:sustained_coherence` (S) | Attested *about* an agent by a `lenscore_detector`. §7.5: an entity may **not** self-emit `capacity:*` (anti-Goodhart). The public roster is a public-tier projection of these rows. |
| A node's own substrate health | **`system:*`** (§5.3 / §7.2) — `corpus_health:n_eff_measurable`, `federation_directory:replication_lag`, `audit_chain:hash_continuity`, `identity_continuity:relational_anchor` | **Reserved** to the substrate self-reporting: emitter `identity_type ∈ {substrate_persist, substrate_edge}`, steward-cross-attested. `witness_relation: self`. ciris-status **reads** these; it must **not** emit them. |
| ciris-status's external health observations | **`health:liveness`** (open vocab, §11.2.1) — *proposed leaf* | External witness: `witness_relation: external`, `epistemic_mode: direct` (probe) or `derivative` (proxy-folded). |

## 2. The two `scores` flows

ciris-status's entire job is **read/aggregate `scores`** and **emit external
health `scores`**.

### Flow A — read & aggregate → the website sockets
1. **Read** `scores` rows on `capacity:*` (per opted-in agent) and `system:*`
   (per node self-report) from the shared corpus / federation directory — the
   same reads any fabric node can do, signature-verified.
2. **Gate by consent / access tier.** ciris-status is a **public-tier reader**:
   it may surface only what the subject opted in to (the `public_sample` /
   consent projection — see §4). The lens-python "key_id + CIRIS score of each
   opted-in agent" is exactly this projection.
3. **Project** to the website surface (§3): the opted-in agent roster + current
   `capacity:composite` (+ factors on request), fleet rollups, and the
   service-health view assembled from `system:*` self-reports + Flow B.

### Flow B — probe & attest → CEG service health
1. **Probe** the infra CIRIS operates that *cannot self-report* `system:*`
   (LLM/search providers, regions, billing/proxy, a node that's *down*) — the
   existing cost-safe probe discipline (passive for paid providers; see README).
2. **Fold** provider/region probe results as **`evidence_refs` + context**, not
   as separate CEG entities (non-keyed infra has no federation key, so it is
   *evidence behind* a keyed service's health, not a subject of its own).
3. **Emit** a `scores` attestation per keyed CIRIS service:
   ```
   attestation_type: scores
   attesting_key_id:  <ciris-status node key>
   attested_key_id:   <the CIRIS service node's key_id>
   envelope: {
     dimension:        "health:liveness",
     score:            +1.0 operational | 0.0 degraded | -1.0 outage,
     confidence:       <probe certainty>,
     context:          "<region / target detail>",
     evidence_refs:    ["<probe result hashes / provider statuses>"],
     valid_until:      "<now + poll cadence>",        // freshness
     epistemic_mode:   "direct" | "derivative",
     witness_relation: "external",
     stake:            "reputational"
   }
   ```
   Health becomes **first-class, signed, replicable federation data** — the
   monitor is accountable for its claims like any participant, and any node can
   verify them. The 60s poller's probe results are the *evidence* behind these
   signed statements, not the product.

**The honest line CEG enforces:** ciris-status speaks **about** services
(`witness_relation: external`, open-vocab `health:liveness`); it never speaks
**as** the substrate (`system:*` is reserved and would be a category error — and
is rejected at admission). Self-report vs. external-observation stays distinct.

## 3. The website surface (the "extra sockets")

Drop-in for the routes ciris.ai already consumes (the Lens nginx route family),
served by ciris-status, fed by Flows A+B:

- **Public scoring** — opted-in agent roster: `{key_id, capacity:composite,
  factors?, valid_until}`. (Replaces lens-python's scoring feed.)
- **Service health** — the aggregated `operational|degraded|outage` view
  (regions / providers / nodes) — superset of today's `/api/v1/status`, now
  backed by signed `health:liveness` + `system:*` rather than a JSON cache.
- **Uptime history** — unchanged (`/api/v1/status/history`), SQLite rollup.
- **Live push** — a websocket/SSE socket pushing roster + health deltas for the
  live ciris.ai display (the "extra website sockets").

ciris-status renders **no UI**; ciris.ai (or Portal) renders over these.

## 4. Decisions made here (namespace / role — not primitive changes)

1. **Monitor identity.** ciris-status runs as a normal fabric node with
   `device_class: service` (§5). It is **NOT** `substrate_persist/edge` — so it
   reads `system:*` but emits its health observations on the open-vocab
   `health:liveness` dimension (no reserved-prefix gate; default `external`
   witness). No special role required to read `capacity:*` / `system:*`.
2. **External-health dimension** = `health:liveness` (open vocab per §11.2.1),
   operational/degraded/outage → `+1/0/-1`, target in `attested_key_id`
   (keyed services) with provider/region detail in `context` + `evidence_refs`.
   *To ratify with CEG as the canonical leaf + operational definition* (open-vocab
   axis discipline) — but it needs **no** primitive/wire change.
3. **Non-keyed infra is evidence, not a subject.** Providers/regions fold into a
   keyed service's `health:liveness` score via `evidence_refs`; they are not
   separate CEG attestations.
4. **Cost-safety preserved.** The probe tiers (passive → keyless → authed) carry
   over unchanged; the 60s loop never authed-probes paid providers. The probe→
   `scores` pipeline is the only new step.

## 5. Deployment — two fabric nodes

| Node | Role | Does |
|---|---|---|
| **Node A — lens-replacement** ("lens node") | CIRISServer, `mode=server` | Carries the **CIRISLens identity byte-identically** (ed25519 seed + RNS `.rid`, no re-key — see CIRISServer `FSD/LENS_TO_SERVER_MIGRATION.md`). Ingests traces (relay), stores the corpus, runs scoring → emits `capacity:*`, self-reports `system:*`. Serves the **gated/key-scoped** reads + **DSAR** + **public-keys registry** + **access tiers**. The substrate. |
| **Node B — ciris-status** ("monitor node") | CIRISServer cores + monitoring cards | Flows A+B above. Reads `capacity:*`/`system:*`, probes off-fabric infra, emits `health:liveness`, serves the website sockets ciris.ai consumes. The public window. |

Topology: both are `ciris-canonical` fabric nodes; Node B reads the mesh Node A
(and others) populate and probes Node A's `/health` like any target. nginx
routes `ciris.ai/...` public surface → Node B; the keyed lens reads → Node A.

### 5.1 GDPR erasure routing — two entry points (AND), one mechanism

Right-to-be-forgotten reaches the §19.7 hard-delete by **both** paths (not
either/or). The fabric *performs* erasure but exposes no public hook for it:

1. **DSAR endpoint on the monitor node (Node B).** The public, key-scoped,
   hybrid-signed `dsar/delete` request (CIRISLens `accord_api.py` shape — *only
   content signed by the requesting key is deleted*) is surfaced **here**, on the
   public window, and drives Node A's erasure. It is **not** a fabric public hook.
2. **In-app, CEG-native** (the fabric app, CIRISServer#15/#9 `auth::erasure`).
   The data subject (or their agent delegate) emits a signed `withdraws`/
   revocation against their own content; the substrate honours it via the same
   §19.7 descent (`Engine::evict_actor`).

Both collapse to one substrate mechanism (revocation → hard delete → below the
recoverability floor), proven by the noise-floor demonstration (CIRISServer#14,
`tests/noise_floor.rs`).

## 6. Ship sequence + gates (honest about blockers)

**Phase 1 — read-replacement swap (ready now).** Stand up Node A; carry the lens
identity; `import-traces` the prod dump; verify the 7 read endpoints + six-key
`/v1/identity`; repoint ingest. Lens runs read-only alongside during the window
(rollback = repoint back). *Gate: none new — this is the migration FSD path.*

**Phase 2 — ciris-status node (this build).** Build Node B's monitoring cards
(Flows A+B + sockets); deploy; repoint `ciris.ai` public surface to it; retire
the lens-python status/scoring serving. *Gate: ratify the `health:liveness`
leaf with CEG (namespace, not primitive).*

**Phase 3 — GDPR / DSAR (gated on substrate bump).** Port DSAR (Art. 17,
key-scoped erasure), the public-keys registry, and the access tiers onto Node A.
*Hard gate:* the erasure primitives (`evict_fountain_content_hard_delete`,
`content_aggregation`, `EjectionVerdict::EjectHardDelete`) ship in **persist
v8.4.0 / verify v5.10.0**; we pin **v8.2.0 / v5.9.0**. Bump first, then wire DSAR,
then the noise-floor compliance demonstration (CIRISServer#14). **The lens
cannot be fully decommissioned until DSAR works as designed** — keep the
Python DSAR path reachable until Phase 3 lands.

**Phase 4 — decommission CIRISLens.** After Phases 1–3 verify, tear down the
Python deployment per the migration FSD §5.

### What's ready vs. blocked
- **Ready:** Node A read-replacement (Phase 1); Node B monitoring cards (Phase 2,
  modulo the `health:liveness` leaf ratification — no code blocker).
- **Blocked on substrate bump:** full GDPR/DSAR (Phase 3) → persist v8.4.0 /
  verify v5.10.0. Until then, DSAR stays on the Python lens (don't decommission).

## 7. Migration-FSD correction (CIRISServer)

`FSD/LENS_TO_SERVER_MIGRATION.md` implies the fabric absorbs "the lens"
wholesale. The corrected three-way boundary:
- **fabric (Node A):** epistemic substrate + signed/keyed reads + DSAR + keys +
  tiers (produces `capacity:*`, self-reports `system:*`).
- **ciris-status (Node B):** the public aggregator + health attestor (`scores`
  reader + `health:liveness` emitter + website sockets).
- **thin presentation (ciris.ai / Portal):** renders over Node B's surface.

That correction should land in the CIRISServer migration FSD alongside this build.
