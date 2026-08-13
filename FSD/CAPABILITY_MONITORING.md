# Capability monitoring — what "down" means when the fabric is redundant

> **Status:** design / build spec. This is the spec the build follows.
> **Thesis:** a component being unhappy is not a service being impaired. The
> status surface currently conflates them, and every consequence of that
> conflation has been paid in public: four days of amber on ciris.ai for a
> provider that is not in the default call path, a headline that flips on one
> slow upstream, and a human writing an RCA to discover that none of it was
> service impact.
>
> **Scope correction that motivated the whole document:** the AI provider we
> actually serve from — **DeepInfra** — is not monitored at all. The three
> providers we do monitor (`openrouter`, `groq`, `together`) are fallbacks.
> A hard DeepInfra outage currently renders a green board.

## 0. The four defects, precisely

Each is independently reproducible against production data.

**D1 — Pooled providers degrade their own router.** CIRISProxy's `/v1/status`
folds every provider it checks into its own `status`, so one slow LLM provider
makes the proxy report `degraded`. We take that self-report verbatim
(`probe::fetch_service_status`), region status is `worst(billing, proxy)`, and
`aggregate::aggregated_status` derives the headline from regions. One slow
member of a redundant pool therefore walks unmodified to "Degraded performance"
on the public page. This is the same shape as the disabled Brave key that made
us permanently degraded for weeks.

**D2 — The monitored set is not the serving set.** Default routing is
DeepInfra → OpenRouter → Groq. We monitor OpenRouter, Groq and Together.
Together is not in the default chain at all; DeepInfra, which is, is invisible.
The pool's health is computed from members that are not the ones serving.

**D3 — Single vantage point.** The poller runs on the US node (Vultr Chicago).
When that node's network flickers, every probe fails and we record the whole
fabric as down — including EU, whose services were provably fine. A monitor that
cannot distinguish "the world is down" from "I lost my network" reports its own
outages as everyone else's.

**D4 — Path-blind latency thresholds.** `probe::check_http` compares latency to
an absolute threshold. A US→EU probe has a ~450–520 ms floor (cold TLS +
transatlantic RTT) against a 1000 ms threshold, so EU is structurally closer to
`degraded` than US for identical health. Today this is cosmetic, because service
status comes from upstream self-reports — but §3 makes us derive status from our
own transport probe, at which point the bias becomes real.

## 1. What we adopt rather than invent

Verified, including licences, because we ship AGPL-3.0.

| Concern | Adopt | Licence | Why |
|---|---|---|---|
| Status vocabulary | **Statuspage v2** component states + `indicator` | de facto (Atlassian) | Groq and GitHub already publish it; aligning makes our output and the vendor feeds we consume one language |
| SLI/SLO semantics | **OpenSLO** object model | Apache-2.0 | Our uptime arithmetic has been re-litigated three times; declaring the SLI makes the semantics inspectable config, not buried Rust |
| Redundancy rollup | **Vigil**'s `min_replicas_available` threshold | MPL-2.0 (file-level copyleft; combines with AGPL) | Strictly better than best-of: expresses "one member left, still serving, one failure from dark" |
| Service self-reports | `health+json` shape (IETF draft, **expired 2021**) | — | A convention, not a standard. Named because the CIRISProxy list-vs-CIRISBilling-map divergence is exactly what a shared shape prevents |

**No standard exists** for capability rollup across redundant providers or for
vantage attribution. Vigil is the closest prior art; the rest is SRE practice.
We borrow *semantics*, not code — nothing here vendors a dependency.

## 2. The model

### 2.1 Capability

A **capability** is a thing the fabric can do. It has members, and a threshold
for how many must be available.

```
capability := { id, label, members[], min_available, kind }
kind        := pool | singleton
```

- `min_available = 1` — classic redundancy. Serving while any member is up.
- `min_available = N` — quorum. Fewer than N available is `degraded` even
  though service continues, because the margin is gone.
- `singleton` is `min_available = 1` over one member. Regions are singletons.

**Regions are NOT a pool.** The fabric is active/active, but a regional outage
is a regional outage: EU users are not served by US being healthy. This is a
product decision, recorded here so it is not re-derived. Only providers behind a
common router pool.

### 2.2 Capability status

```
available   := members whose status is operational
capability  :=
    available >= min_available            -> operational
    0 < available <  min_available        -> degraded          (serving, no margin)
    available == 0 and members nonempty   -> major_outage
    members empty                          -> unknown           (never green by omission)
```

A pool member being `degraded` rather than `outage` counts as unavailable for
the threshold but is not itself an outage — the router will route around it.

### 2.3 Primary vs fallback

Members carry `role := primary | fallback`. Serving on a fallback is not an
outage, but it is a fact worth recording: it precedes cost, latency and quality
changes that nothing else on the board would explain. When the primary is
unavailable and the capability is otherwise operational, emit a
`capability.<id>.primary` transition event. It does **not** change the headline.

### 2.4 SLI

Per OpenSLO's separation of indicator from objective:

```
SLI(capability, window) = good_events / total_events
good_event  := at a sample instant, available >= min_available
total_event := a sample instant in which the capability had any member reported
```

Computed **exactly**, not bounded. Every row in a poll cycle shares one
timestamp (`history::poll_once`), so simultaneity is `GROUP BY ts` — we never
have to infer overlap from daily averages the way a consumer of daily rollups
must:

```sql
SELECT day, AVG(CASE WHEN available >= :min THEN 100.0 ELSE 0.0 END) AS sli
FROM (
  SELECT date(ts) AS day, ts,
         SUM(CASE WHEN status = 'operational' THEN 1 ELSE 0 END) AS available
  FROM status_checks
  WHERE service_name = :svc AND provider_name IN (:members)
  GROUP BY ts)
GROUP BY day
```

This is the single most important line in the document: **we hold the raw
samples, so we can measure overlap instead of bounding it.** Any consumer
computing redundancy from our daily rollup (as ciris.ai does today) is
approximating something we can state exactly.

## 3. Changes by defect

### 3.1 D1 — pooled members do not degrade their router

`aggregate::fold_proxy` already classifies providers by the upstream's `type`.
Extend that classification to a `pooled` predicate (`llm`, `search`), and:

- The reporting service's own status becomes `worst(transport_probe,
  non_pooled_providers)` — it no longer inherits pool member health.
- The upstream's self-report is preserved as `ServiceSummary.upstream_status`,
  so we never silently overwrite what a service told us about itself.
- Pool members populate their capability, which contributes to the headline
  through §2.2 — so an actual simultaneous failure still shows.

**Upstream fix (CIRISProxy, separate repo):** the proxy should not fold pooled
providers into its own `status` either. Ours is defence in depth; theirs is the
root. Tracked separately.

### 3.2 D2 — monitor what serves

- `status.ci`-style config gains `status.capability.<id>.members` and
  `status.capability.<id>.min_available`, so the pool is declared, not inferred
  from whatever the proxy happens to report.
- A member that is declared but **never reported** renders `unknown`, and the
  capability is `unknown` if that leaves it below threshold. Silence is not
  health.
- **CIRISProxy must add DeepInfra to its checks.** Until it does, the declared
  pool will show DeepInfra as `unknown`, which is the honest rendering of "we
  are not measuring the thing that serves" — visibly wrong on the board, rather
  than invisibly wrong.

### 3.3 D3 — vantage detection

If **every** probe in a cycle fails at the transport layer (connection refused,
timeout — not an HTTP status), the most likely explanation by a wide margin is
the monitor's own network, not the simultaneous failure of unrelated third
parties on three continents.

```
if transport_failures == probes_attempted and probes_attempted >= MIN_FOR_VERDICT:
    record component `monitor.network` = outage
    record NOTHING for the probed components this cycle
    serve the previous snapshot, marked stale
```

`MIN_FOR_VERDICT = 3` — with one or two probes configured, a genuine dual
outage is indistinguishable from a local failure, and we must not claim a
verdict we cannot support.

This is the change that would have removed root cause 2 from the 2026-08-12 RCA
entirely, and with it the "EU billing was degraded but has no errors in its
logs" paradox.

### 3.4 D4 — per-target latency baselines

`status.target.<id>.latency_baseline_ms` (default 0) is subtracted before
threshold comparison, so a transatlantic probe is judged on its excess over its
own floor rather than against a US-local constant. Where unset, behaviour is
unchanged.

A baseline is a **config value, not a learned one.** An automatically learned
baseline would drift upward during a slow degradation and quietly redefine
"normal" as whatever is happening — the failure mode where a monitor stops
noticing gradual decline.

## 4. Wire changes

Additive. Existing fields keep their meaning; nothing that ciris.ai reads today
changes shape without a new name alongside it.

### `GET /api/v1/status`

```json
{
  "status": "operational",
  "indicator": "none",
  "capabilities": {
    "ai_providers": {
      "label": "AI providers",
      "status": "operational",
      "min_available": 1,
      "available": 2,
      "members": [
        {"id": "deepinfra",  "role": "primary",  "status": "unknown"},
        {"id": "openrouter", "role": "fallback", "status": "operational"},
        {"id": "groq",       "role": "fallback", "status": "operational"},
        {"id": "together",   "role": "fallback", "status": "degraded"}
      ]
    }
  },
  "regions": { "...": "unchanged" }
}
```

`status` becomes capability-derived (§2.2), and `indicator` is added as the
Statuspage v2 severity word (`none|minor|major|critical`).

**Component status strings do NOT change in Phase 1.** Full alignment would
rename `degraded` → `degraded_performance` and `outage` → `major_outage` in
every component row, which the status board renders as *unknown* (its palette is
keyed on the current words) and which ciris.ai reads directly. Renaming a
vocabulary out from under two live consumers to gain interop we can already get
from an additive `indicator` is a bad trade. The alias mapping is accepted on
input, and the rename waits for a coordinated consumer update.

### `GET /api/v1/status/history`

Each day gains `capabilities: {id: {sli_pct, min_available, members[]}}` and
`service_uptime_pct` — the worst capability's SLI. `uptime_pct` keeps its
current meaning (the component mean) so the number does not move a fourth time
under anyone's feet.

### `GET /api/v1/status/events`

Events gain an optional `capability` field, and two new component classes:
`capability.<id>` for threshold crossings and `monitor.network` for §3.3.

## 5. Vendor corroboration (phase 2)

When a component transitions to a non-operational state and its vendor publishes
a machine-readable status, fetch it once and attach the vendor's own verdict to
the event. This turns "a human opens StatusGator" into a field in the log.

Probed 2026-08-13, and the coverage is worse than one would hope:

| Vendor | Feed |
|---|---|
| Groq | ✅ Statuspage v2 |
| GitHub | ✅ Statuspage v2 |
| Together AI | ❌ no feed at standard paths |
| OpenRouter | ❌ HTML only |
| Hetzner | ❌ HTML only |
| Vultr | ❌ **actively blocked** — Cloudflare 403 on `/history.rss` and `/api/v1/incidents` |

So vendor feeds are opportunistic corroboration, never a foundation, and we do
not scrape the ones that decline to be read.

**The stronger signal is one we already collect and currently discard.** Both
regional proxies independently probe the same external providers. Agreement
across regions implicates the provider; disagreement implicates the path or our
vantage. `aggregate::merge_worst` collapses those two observations into one row
(correctly — it fixed US hiding behind EU), destroying the disambiguation.
Retain per-region observations in the history and events while continuing to
serve the merged row for display.

## 6. Phases

**Phase 1 — this build, in CIRISStatus.** §2 model, §3.1 D1, §3.2 D2 (declared
pools + `unknown` for unmeasured members), §3.3 D3, §3.4 D4, and the additive
half of §4 (`capabilities`, `indicator`, `service_uptime_pct`, capability SLIs
in history). Ships as `0.3.44`. The component-vocabulary rename is explicitly
NOT in it.

**Phase 2 — CIRISProxy.** DeepInfra added to health checks; pooled providers
stop folding into the service's own `status`. Filed upstream.

**Phase 3 — corroboration.** §5 vendor adapter and per-region retention.

**Phase 4 — ciris.ai.** Adopt server-side capability numbers and delete the
client-side redundancy floor, which today approximates from daily aggregates
what §2.4 computes exactly. Filed on the website repo.

## 7. Test plan

Every defect gets a test that fails against the current code:

- **D1** — a pool member degrades; the router's own status stays operational;
  the headline stays operational; the member's capability records the change.
- **D1** — every pool member fails at once; capability is `major_outage` and the
  headline follows.
- **D2** — a declared member that is never reported renders `unknown`, and a
  pool below threshold on `unknown` members is not green.
- **D3** — all probes fail at transport: `monitor.network` is recorded, the
  probed components are NOT written down, and with fewer than
  `MIN_FOR_VERDICT` probes no verdict is claimed.
- **D4** — identical excess over different baselines yields identical status.
- **§2.4** — the exact-overlap SLI: two members whose downtime does not overlap
  yields 100% availability, and overlapping downtime yields exactly the
  overlap — the case a daily-rollup consumer can only bound.
- **§2.3** — primary unavailable with a healthy fallback: capability
  operational, headline unchanged, `primary` event emitted.

## 8. Non-goals

- Learned/adaptive baselines (§3.4).
- Scraping vendors that block automated reads (§5).
- Treating regions as redundant (§2.1) — decided, not open.
- Changing `uptime_pct`'s existing meaning (§4). It has moved three times this
  month; `service_uptime_pct` is the new name for the new number.
