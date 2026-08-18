# Multi-vantage monitoring — a second observer, over CEG

> **Status:** design / build spec, informed by a source audit of the pinned
> `ciris-server v0.5.177` / `ciris-persist v32.3.0` / `ciris-edge v17.4.1`
> revs and CC 1.0-rc3. (Audited at server v0.5.169 / persist v30.11.0. §2's
> defects are re-checked at every repin: D6 closed in v31.2.0; D5 and D7 still
> live at v32.3.0.)
>
> **Thesis:** one observer cannot distinguish "the service is down" from "my
> path to it is down". The information is not there. A second node makes the
> difference observable, and CEG already carries everything needed to move its
> claims — except a name for *whose vantage a claim is from*.

## 0. What prompted it

A momentary EU-billing blip that took a human, an Ansible run, two vendor status
pages and an RCA to attribute — correctly — to the monitor's own datacentre. The
monitor runs on Vultr Chicago; when its network flickers, every probe fails and
the fabric is recorded as down, including a region that was provably fine.

`FSD/CAPABILITY_MONITORING.md` §3.3 closed the *total* case: if every probe
fails at transport, record `monitor.network` and nothing else. It cannot close
the single-target case, and no amount of cleverness on one node will: "EU
billing is unreachable" and "EU billing is down" produce identical evidence.

There is a second reason, independent of attribution. **The status page shares a
failure domain with what it monitors.** It runs on the node it reports on. A
status page that cannot outlive its own infrastructure tells you about the world
only while the world is fine.

## 1. What the substrate already does (audited, not assumed)

### 1.1 Emitting is solved

Flow B already does exactly the write a second node needs, every poll cycle:
canonicalise through the CEG produce gate → `Engine::sign_hybrid` (Ed25519 +
ML-DSA-65) → assemble a federation-tier `Attestation` → `put_attestation`
(`src/ceg.rs`). A second node emitting `health:liveness:v1` runs *the same code
path*. Nothing new is required to produce.

### 1.2 Querying is solved, and better than we are using

`ReadEngine::list_attestations(filter, cursor, limit, scope)` — which Flow A
already calls for `capacity:*` (`src/roster.rs`) — filters on:

```
attesting_key_id     WHO attested        ← the vantage axis
attested_key_id      who was attested
subject_key_id       attestations naming this subject
attestation_type     "scores"
dimension_prefixes   ["health:liveness"]  (open-vocabulary, OR-combined)
dimension            exact match
valid_at             point-in-time validity; expired rows drop out
confidence_floor     minimum weight
```

So "every `health:liveness` attestation valid now, from any attester, about
these subjects" is one filter and a cursor loop. **Grouping by
`attesting_key_id` is the vantage split.**

Better still, persist ships the fold we would otherwise hand-roll:
`resolve_scores` applies precedence (withdraws/recants/supersedes per attester),
takes **latest-wins per attester** so each observer contributes exactly one
head, aggregates, and — the part that matters here — counts
**`open_contradictions`**: heads whose sign opposes the believed sign. That is
disagreement-between-observers as a first-class output. We do not call it.

### 1.3 Trust is key registration; consent is governance

The audit is unambiguous, and CC 3.3.7 states it normatively: the gate that lets
A's corpus *admit* a row B signed is **B's key existing in A's
`federation_keys`**. `consent:replication` adds no substrate admission check —
it is the auditable, revocable, bilateral record of intent, and it selects who a
node *initiates* replication with.

So "same owner, mod privs granted by trusted root" maps onto three separate
things, and it is worth keeping them separate:

| Question | Mechanism |
|---|---|
| May this node's owner peer at all? | owner-binding — `delegates_to(user → node, infra:*)`, CC 3.2, enforced by the serve-only floor |
| Will A store a row B signed? | B's key registered in A's `federation_keys` |
| Will A run replication rounds with B? | A's own `consent:replication:v1` grant naming B |

Peering registers the key and authors the grant in one owner-gated call. Each
node authors its own grant — a consent object is self-attested by the granting
party, which forecloses third-party forgery.

**Anti-requirement:** trust decides which attestations are *admissible*. It must
not decide which one is *true*. If A trusts B and then merges B's rows
worst-wins or newest-wins, the second vantage has bought nothing: disagreement
is the finding, and resolving it by trust converts the most informative signal
available into a coin flip.

## 2. Three defects in what we emit and consume today

Found by audit, all live.

### D5 — our liveness attestations are self-attested, which CC forbids

`src/adapter.rs` sets `attested_key_id` to **this node's own key**, and
`src/ceg.rs` simultaneously declares `witness_relation: "external"`. Persist's
family rule for `health:liveness:` says: *witness_relation MUST be external — a
service never attests its own liveness (attester != attested)* (CC 3.1.9.4 /
CC 3.4.3).

It is admitted only because the invariant gate is keyed on the row's
`attestation_type` (`"scores"`) rather than the envelope's `dimension`
(`health:liveness:v1`), so `"scores".starts_with("health:liveness:")` is false
and the check never fires. Persist has already fixed this exact
wrong-axis bug for `capacity:` one line above in the same function. When the
`health:liveness:` twin lands — one line, and the pattern is right there — **our
emits stop being admitted.**

Re-verified against persist v31.2.0, and the distance is now shorter, not
longer: `invariant::NEWLY_ENFORCED_SELF_EMISSION_PREFIXES = ["health:liveness:"]`
exists, `enforce_admission_invariants` implements the attester≠attested check,
and it is already wired into `check_reserved_prefix_admission` — the same
chokepoint every backend's `put_attestation` runs. Only the axis still saves
us: that function is handed the row's `attestation_type`. The fix is a
`starts_with` against the envelope dimension, and the day it lands our liveness
plane goes dark. Deciding D5 (1/2/3 above) is not indefinitely deferrable.

Still true at persist v32.3.0, and the asymmetry is now visible in a single
screen of `check_reserved_prefix_admission`: `check_capacity_not_self_attested`
is handed `envelope_dimension(&row.attestation_envelope)`, and
`enforce_admission_invariants` — the `health:liveness:` arm — is handed `at`,
the `attestation_type`, four lines below it. The correct axis is already in
scope at the call site.

Naming the *service* as `attested_key_id` is not a drop-in fix: the attested
subject must resolve to a registered key, and billing/proxy have none.
Options, to decide before building:

1. **Register service keys** and attest about them properly. Most correct,
   most work, and makes every service a fabric identity.
2. **Move to an `observation:*` dimension.** A monitor attesting "I observed X
   at T" is a genuinely different claim from "X is alive", and the honest one
   for an outside observer. Needs a `:v1` segment and lands in the
   registry-conformance grey zone.
3. **Keep `health:liveness:v1`, name the subject only in `subject_key_ids`**,
   and set `attested_key_id` to something that resolves. This is closest to
   today and still fails the rule's intent once the gate is keyed correctly.

(1) is the shape the grammar wants. (2) is the shape the *epistemics* want.

#### The decision (2026-08-14): (2) now, (1) as the destination

**We cannot sign "billing is alive." We do not know it.** We know that at T,
from this node, an HTTPS GET to billing's health endpoint returned 200 in 84ms.
First-person experience is the only thing our key is entitled to bind.
`health:liveness` is a third-person claim about a service's state; the grammar
wants that signed either by the service itself or by a witness *about a
registered subject*, which is exactly what `attester != attested` encodes.

So the two options are not alternatives, they are a sequence.

**Now — `observation:reachability:v1`, one row per observed target.** The row's
subject is *the observation*, and the observation is genuinely ours, so
`attester == attested` is CORRECT here rather than a violation. What changes
substantively is not the dimension string but the addressability: today's single
row names no target in any machine-readable field (the services live in a
`context` string and in `evidence_refs`), so the fabric can be asked *"what did
ciris-status say about itself?"* and nothing else. Per-target rows are also the
precondition for `resolve_scores`'s per-attester fold and `open_contradictions`
to mean anything once a second vantage exists — §4's whole mechanism is
per-subject.

Verified admissible against persist v31.2.0 rather than assumed:
`observation:` is absent from `default_reserved_prefix_rules` (the reserved set
was `system:`, `audit_chain:`, `corpus_health:`, `identity_continuity:`,
`federation_directory:`, `transparency_log:cosigned:`), the `scores` vocabulary
is open, the `:v1` segment satisfies `require_version_segment`, and no morally-
charged stem matches. No registration, no new gate, admitted today.

**Re-verified at v32.3.0, where the reserved set has since grown**
(`age_assurance:`, the capacity-assurance prefix, `detection:` and two
`detection:` sub-prefixes). `observation:` is still not in it. Because that set
is a moving target rather than a settled boundary, the check is not left to
this document: `ceg::flow_b_emit::the_observation_prefix_needs_no_substrate_role`
emits through a real engine under a key holding no substrate role, so a repin
that reserves the prefix fails in CI rather than in production.

`witness_relation` becomes `"self"`. It has no closed vocabulary, no validator
and no gate (§3), so nothing enforces this — which is the reason to get it
right rather than a reason not to bother. `"external"` alongside
`attester == attested` was simply false.

**No `vantage` field**, per §3: `attesting_key_id` IS the vantage, and one node
does not observe from several places. The temptation to add one arrives with
this change and should be refused on the same grounds.

**Later — services as fabric identities, and then `health:liveness` is
legitimate.** attester = the monitor, attested = the service,
`witness_relation: "external"` finally true. The cost is specific and it is not
ours to pay alone: `check_attested_subject_admission` requires the attested
subject to be a registered key (or a stored constitutional family), and
CIRISPersist#659 requires the registration envelope to bind that subject's own
`key_id`, `identity_type` and *both pubkeys* — which the subject must sign. We
therefore cannot mint identities for billing and proxy on their behalf without
holding their keys and signing as them, which is impersonation wearing a
convenience costume. Each service repo grows a keypair and a self-signed
registration, or the claim stays first-person.

**(3) is rejected.** Beyond failing the rule's intent, `subject_key_ids` is not
a label slot: under CIRISPersist#643 a canonical binding hash there confers
REVOCATION AUTHORITY (`resolve_withdraws_admission_rule` rule 2).

##### The cardinality constraint this decision runs into

Per-target rows multiply what we author, and authoring is metered. Persist's
`PeerWriteQuota` is charged inside `put_attestation` on every backend, keyed on
**`attesting_key_id` — the row's author** — with no exemption for a node's own
local writes: `PER_PEER_SUSTAINED_WRITES_PER_WINDOW = 14_400` per 86_400s. At
the 60s probe cadence that is 1,440 cycles/day and therefore **10 rows per cycle
before we saturate our own key's budget** — and the same ceiling applies again
at any peer that replicates us, since the bucket there is keyed on our key too.

Our direct targets alone (2 regions × {billing, proxy, infra}, plus the identity
providers and the database) sit at roughly that number, so emitting per-target
at probe cadence would spend the entire daily budget on observations and leave a
second node nothing.

**Therefore the observation cadence is decoupled from the probe cadence**
(`status.observation.poll_secs`, default 300s) and `valid_until` follows the
observation cadence, not the probe cadence. The page and the SSE stream keep
serving the 60s snapshot — human-facing freshness is a local concern. The signed
plane is for the fabric, where a 5-minute-old observation with an honest expiry
is worth more than a 1-minute-old one that exhausts the budget it needs to
replicate.

### D6 — our envelope carries no signed instant — **closed by the substrate in persist v31.2.0**

Equivocation detection (CC 6.1.1 N4) compares the instant **inside the signed
envelope**; an envelope without one is `NoSignedInstant` — counted, not guessed
at. Our liveness envelope omitted `asserted_at`, so every row we emitted was
invisible to it.

persist v31.0.0 (CIRISPersist#598) made this refusable rather than merely
lossy: `check_instant_binding` now runs on **every** dimension, so a row whose
envelope carries no signed `asserted_at` is one the substrate's own put door
rejects — "folds pick a winner by the `asserted_at` COLUMN, which no signature
covers". The note below about never reconciling on the column is now enforced
rather than advised.

We satisfy it for free because `src/ceg.rs` emits through
`emit_attestation_self`: `stamp_and_canonicalize` stamps a truncated
`asserted_at` into the envelope before signing when the producer did not set
one, and `assemble` then reads the column back *from* the signed envelope — so
column and signed twin are equal by construction. The one thing that must not
regress is the door: a hand-rolled row assembled around `put_attestation`
carries no stamp and is refused. That is exactly what our two roster fixtures
did, and v31 caught both; they now mint through
`envelope::RowMirror::stamp_local_row(&mut row, false)` before signing, which
places the instants *and* the seven-column mirror (#643/#656).

### D7 — our consumer collapses attesters

`src/roster.rs` keys its projection solely on `attested_key_id`: no
`attesting_key_id`, no precedence, no timestamp comparison. Two observers
attesting about one subject silently overwrite each other, and which one wins
depends on DB iteration order. **This must be fixed before a second node exists**
or the second node will make the aggregate *worse* — nondeterministically.

Related: nothing reads `health:liveness` at all. Flow A filters
`dimension_prefixes = ["capacity:"]`, so a peer's liveness rows would replicate
into our corpus and be read by nothing.

## 3. The vantage gap — the part we are specifying

`vantage` does not exist in the CEG grammar. Neither does `observer`. Zero
occurrences in persist, edge, or the Constitution. `witness_relation` is a real
envelope field but has no closed vocabulary, no validator and no gate — it is
producer convention. `identity_type = "witness"` exists but is reserved for
transparency-log co-signing, not monitoring.

So per-observer vantage is ours to define. Two candidate carriers:

- **`context` / a payload member** on `health:liveness:v1` — e.g.
  `observer_vantage: "us-chicago"`. No new dimension, no registry question,
  admitted today. Weakly typed.
- **A new `observation:*` dimension** with vantage as a first-class field.
  Cleaner semantics; needs `:v1` and a conformance conversation.

**Recommendation: neither, at first.** `attesting_key_id` *is* the vantage — it
is the key of the node that observed. It is already on every row, already a
filter axis, already what `resolve_scores` groups by. A separate vantage field
is only needed when one node observes from several places, which is not a thing
we have. Adding a field we do not yet need is how vocabularies rot.

## 4. Reconciliation semantics

Given N observers attesting about one subject:

```
agreement (all heads same sign)      -> the subject. Report it.
disagreement (open_contradictions>0) -> the PATH between a dissenting observer
                                        and the subject, or that observer.
                                        Report BOTH heads, attributed.
single observer                      -> report, flagged single-vantage:
                                        unattributable by construction.
```

Concretely: `/api/v1/status/vantage` already returns this shape for providers
seen by two regional proxies. Extending it to services means sourcing the same
response from attestations grouped by `attesting_key_id` instead of from local
rows grouped by `region`.

The headline takes the **agreed** view. A component only one observer can see is
reported at that observer's word, marked as such. A component observers disagree
about is `unknown` for the headline and loud in the detail — we do not know, and
the disagreement is more useful than either answer.

## 5. Phases

**P1 — fix the consumer before adding the observer.** D7: group by
`(attested subject, attesting_key_id)`, use `resolve_scores` rather than
hand-rolling, and make something actually read `health:liveness`. Landing a
second node before this makes the aggregate nondeterministic.

**P2 — fix what we emit.** D5 (decide 1/2/3 above; it is a correctness and
conformance question, not a style one) and D6 (`asserted_at`, one line).

**P3 — the second node.** Boot with its own `--home` and `--key-id`, claim
ownership, author its own `status.*` config (config is `cohort_scope: self` and
never replicates — it must be authored locally), then peering in both
directions. Grant prefixes must include `health:` — the default is
`["capacity:", "trace:"]`, which does not cover it.

**P4 — reconciliation.** §4 semantics; `/api/v1/status/vantage` sourced from
attestations; headline takes the agreed view.

## 6. Operational notes worth not rediscovering

- **There is no peering CLI in `ciris-status`** — only `config set|get`. Peering
  is `POST /v1/federation/peering` with an owner session, or the desktop client.
- **Per-peer write quotas**: 600 rows/60s burst, 14,400/day sustained. A 60s
  cadence is nowhere near it; a debug loop is.
- **Use expiry, not withdrawal, for liveness churn.** A node emitting many
  `withdraws` against its own stale rows gets de-peered by withdraws-arbitrage,
  which is re-judged every reconcile tick.
- **Freshness is the consumer's job**: `expires_at = valid_until`, and readers
  pass `valid_at`. There is no liveness-of-the-monitor signal in the grammar —
  a dead monitor's last row simply expires.
- **No erasure primitive reaches an attestation envelope today.** Withdrawal
  obliges forward-only cessation; it cannot un-send.

## 7. Non-goals

- A `vantage` field in CEG (§3) until one node observes from several places.
- Resolving observer disagreement by trust, weighting or recency (§1.3).
- Treating a second node as a failover for *serving* the page — that is a
  separate concern (DNS/anycast) and does not need the fabric.
