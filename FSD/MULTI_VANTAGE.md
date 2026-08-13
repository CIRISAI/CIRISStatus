# Multi-vantage monitoring — a second observer, over CEG

> **Status:** design / build spec, informed by a source audit of the pinned
> `ciris-server v0.5.169` / `ciris-persist v30.11.0` / `ciris-edge v15.22.0`
> revs and CC 1.0-rc3.
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
This FSD does not choose; it refuses to let the choice be made by accident.

### D6 — our envelope carries no signed instant

Equivocation detection (CC 6.1.1 N4) compares the instant **inside the signed
envelope**; an envelope without one is `NoSignedInstant` — counted, not guessed
at. Our liveness envelope omits `asserted_at`, so every row we emit is invisible
to it. Adding the field is one line and buys real integrity checking. Note the
row column `asserted_at` is stamped at write time and is *not* covered by the
content hash — never reconcile on it.

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
