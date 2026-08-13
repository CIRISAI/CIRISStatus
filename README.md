# ciris-status

**ciris-status IS a `ciris-server` fabric node + a `StatusAdapter`** — it is not a
parallel federation implementation. It serves **ciris.ai's public health/status
surface** (the subset CIRISLens's API serves today, lifted out so the status page
survives Lens's retirement), as an adapter folded onto a real fabric node.

The whole node — the shared persist `Engine`, the Reticulum edge,
`consent:replication` peering, the read API, NodeCode, ownership, the safety
foundation, and NAT-traversal — is `ciris_server::serve_with_adapter`. The status
page is a `StatusAdapter: ciris_server::Adapter`, mirroring CIRISAgent's adapter
model: it contributes the status HTTP routers (merged onto the node's read-API
listener) and a background lifecycle (probe → emit signed `health:liveness:v1` →
rebuild the public roster from this node's OWN corpus → cache/history/live push).

The status surface itself is still pure outbound HTTP probes + a SQLite uptime
history and event log written by the adapter's poll loop. No Grafana, no TimescaleDB, no OAuth,
no ingest pipeline — those retire with Lens.

> **Zero env (ciris-server 0.5):** ciris-status takes **no environment
> variables**. It boots from two CLI flags — `--home <path>` and `--key-id <name>`
> — and resolves everything else from signed `config:*` / consent CEG objects in
> its own corpus, authored by the owner at runtime. Pinned to `ciris-server`
> `v0.5.0`.

## Endpoints

Drop-in for the Lens nginx route (`agents.ciris.ai/lens/api/…` → this service):

| Route | What it does |
|---|---|
| `GET /health` | Liveness: `{status:"healthy", timestamp, version}` |
| `GET /v1/status` | Local providers (postgresql + grafana), live, only if configured |
| `GET /api/v1/status` | Aggregated multi-region: regions (billing/proxy), infrastructure (Vultr/Hetzner/GHCR), LLM/auth/database/internal provider buckets. Served from the poll loop's snapshot (≤ `status.poll_secs` old) so what a caller sees is what was recorded and attested |
| `GET /api/v1/status/events?days=&limit=` | **Observed transitions**, newest first: `{ts, component, from, to}`. `days` 1–365 (default 7), `limit` ≤ 1000 |
| `GET /api/v1/status/history?days=&region=` | Daily uptime rollup from SQLite. `days` 1–365 (default 30), `region` ∈ `us\|eu\|global`. Each day carries `date`, `uptime_pct` (and its `overall_uptime_pct` alias), a one-word `status`, and the per-region/service breakdown. `outage_count` counts **incidents**, not samples. |
| `GET /api/v1/scoring` | **Public scoring roster** (Flow A): opted-in agents `{key_id, capacity_composite, factors?, valid_until}`, consent-gated. Replaces lens-python's scoring feed. Served from cache, populated from this node's OWN corpus by the adapter loop. |
| `GET /api/v1/ci` | **Substrate build health**: the last 10 GitHub Actions runs per repo (verify → persist → edge → server → agent) as `{repo, runs[]}`, each run one of `success\|failure\|in_progress\|queued\|cancelled`. A ~600-byte projection so a microcontroller can read it in one request; polled server-side with conditional requests (see below). |
| `GET /api/v1/scoring/live`, `GET /api/v1/status/live` | **SSE** live-push of roster + overall-health deltas (the "extra website sockets"). |
| `GET /api/v1/status/ws` | **WebSocket** variant of the same live-push. |

These routers merge onto `ciris-server`'s read-API listener (the RET port + 1,
default `:4243`). One node, one read surface.

### The fabric node — what comes from `ciris-server`

ciris-status is **always** a node (there is no optional `fabric` feature any more —
that duplicate federation code was deleted). The node's identity, corpus,
self-key registration, `consent:replication` peering, and A↔B replication are all
`ciris-server`'s `serve_with_adapter`. The adapter only contributes the two flows
of `FSD/MONITORING_NODE_DESIGN.md`:

- **Flow B** — each poll, probe results become a signed CEG `scores` attestation
  on dimension `health:liveness:v1` (`witness_relation: external`,
  operational/degraded/outage → `+1/0/-1`). Non-keyed infra (LLM/search
  providers, regions) folds in as `evidence_refs`, not as separate subjects.
  Hybrid-signed (Ed25519 + ML-DSA-65) via persist v9.0.3 / verify v6.2.0 over
  `ceg_produce_canonicalize` and written with
  `FederationDirectory::put_attestation` into **this node's own corpus**
  (federation-tier rows are PQC-mandatory at the v9.0.0 ingest gate,
  CC 5.3.2.4.3.1). The node's signing key is already self-registered by
  `serve_with_adapter` at boot, so the row passes the attesting-key gate.
- **Flow A** — reads `capacity:*` `scores` from **this node's own corpus**
  (public-tier `CallerScope::Unauthenticated`, i.e. the consent / `public_sample`
  projection) and projects the roster `/api/v1/scoring` serves. Node A's
  `capacity:*` arrives in this corpus by **consented A↔B replication** (which
  `ciris-server` owns), never by reading A's database directly.

Cost discipline is unchanged: Flow B reuses the same aggregated probe and never
authed-probes paid providers in the loop.

Response shapes match the Lens API field-for-field (status strings
`operational\|degraded\|outage`; aggregate overall
`operational\|degraded\|partial_outage\|major_outage`).

## Configuration — ZERO ENV

ciris-status takes **no environment variables**. Boot takes two CLI flags; all
other config is signed CEG, owner-authored at runtime.

### Boot (CLI flags — the only inputs)

| Flag | Default | Meaning |
|---|---|---|
| `--home <path>` | `/var/lib/ciris` | the data root. `data_dir = <home>/data`; corpus `<data_dir>/ciris_engine.db`; minted Ed25519 + ML-DSA-65 identity under `<home>`; the uptime-history DB is **derived** as `<data_dir>/status.db`. The corpus is this node's **OWN** — never share `--home` with / mount the lens node's DB. |
| `--key-id <name>` | `ciris-status` | the node's federation `key_id` (the `health:liveness` attester; self-registered at boot by `serve_with_adapter`). |

The node's listen address, transport/NAT-traversal, replication cadence, and mode
are the **node's** `config:*` CEG (resolved at boot) — see `ciris-server`'s
`src/config.rs`. The read API + status routers bind the Reticulum port + 1
(default `:4243`).

### Adapter config:* (probe targets, cadence, CORS) — `config:*` CEG

The StatusAdapter resolves its own config from signed `config:*` objects in this
node's corpus (read live each poll cycle via `graph_config` — owner changes apply
with **no restart**), all under the `status.` namespace. Author via the desktop
client or `POST /v1/config` after claiming ownership. A region/external provider
is probed **only** when its `*_url` is set; a fresh node runs with no probes, the
baked CORS allow-list, and 60s cadence.

| key | type | default | meaning |
|---|---|---|---|
| `status.poll_secs` | i64 | `60` | probe + roster-refresh + history poll cadence |
| `status.cors_origins` | list | baked `ciris.ai` set | CORS allow-list |
| `status.ghcr_url` | str | `https://ghcr.io/v2/` | container registry (401 = up) |
| `status.database_url` | str | — | local `postgresql` provider (TCP liveness) |
| `status.grafana_url` | str | — | local `grafana` provider (`/api/health`) |
| `status.region.<us\|eu>.name` | str | baked label | region display name |
| `status.region.<us\|eu>.billing_url` | str | — | regional billing `/v1/status` |
| `status.region.<us\|eu>.proxy_url` | str | — | regional LLM-proxy `/v1/status` |
| `status.region.<us\|eu>.infra_url` | str | — | infra host health (Vultr/Hetzner) |
| `status.external.<exa\|brave\|serper\|tavily>.url` | str | — | external search provider health URL |
| `status.external.<…>.api_key` | str | — | key sent only when `.auth = true` |
| `status.external.<…>.auth` | bool | `false` | send the live key when probing — **billable for some providers** |
| `status.ci.owner` | str | `CIRISAI` | GitHub org the CI repos live in |
| `status.ci.repos` | list | the substrate five | repos `/api/v1/ci` reports, in render order. Empty ⇒ CI polling off |
| `status.ci.token` | str | — | GitHub token. Optional: unauthenticated works, but a token raises the ceiling from 60 to 5000 req/hour |
| `status.ci.poll_secs` | i64 | `300` | CI poll cadence — deliberately slower than `status.poll_secs` |

### Why `/api/v1/ci` exists

The physical status board (`extras/galactic-unicorn/`) renders these as one
"centipede" per repo, and it cannot poll GitHub itself: the unauthenticated
Actions API allows **60 requests/hour per IP** (five repos per refresh burns
that in minutes) and each `actions/runs?per_page=10` response is **~120 KB** of
JSON (measured: 124,809 bytes for CIRISServer) — five of those would flatten a
Pico's heap. So the node polls on its own cadence and serves a cached snapshot.

Every poll is conditional (`If-None-Match`, one ETag per repo). GitHub does not
count a `304 Not Modified` against the rate limit, so a quiet stack costs
almost nothing; `status.ci.poll_secs` is the backstop for when ETags do go
stale. A repo whose fetch fails or is rate-limited **keeps its previous row** —
a GitHub hiccup must never turn a green centipede red, or blank the board.

The uptime-history DB path is **not** config — it is derived by convention from
the node data dir (`<data_dir>/status.db`). It keeps 400 days (the 365-day
maximum query window plus slack) and prunes older samples at boot, so an
append-only table cannot grow without limit on a small node.

### One sampler, one truth

`/api/v1/status` used to probe the upstreams **on every request**. That had two
consequences worth naming, because both bit us:

- **What was served was never what was recorded.** A caller's request ran its
  own probe round; the history poller ran a different one 30 seconds later. A
  transient — say an LLM provider going slow, which degrades both the provider
  row and the proxy that reports it — could render on the status page and be
  absent from the history, because the poller's samples straddled it. The
  physical status board caught exactly this repeatedly, and nothing in the
  service could corroborate it.
- **Probe amplification.** Every viewer of the status page triggered real
  outbound requests to billing, proxy and GHCR, proportional to page traffic.

Now the poll loop is the only sampler. It probes once per cycle, and that single
snapshot is what gets served, recorded, diffed for transitions, and signed into
the `health:liveness:v1` attestation. The endpoint is at most `status.poll_secs`
stale (60s by default); the SSE/WS sockets still push deltas as they happen.

### Transitions, not just averages

A daily uptime rollup cannot express a 90-second blip: it moves the mean by
0.07% and reads as noise. So each cycle's snapshot is diffed against the
previous one and every change is appended to `status_events` and served by
`/api/v1/status/events`:

```json
{"ts":"2026-08-13T14:03:00Z","component":"eu.proxy","from":"operational","to":"degraded"}
```

A component that stops being reported transitions to `unknown` rather than
silently vanishing — losing sight of something must not look like it being fine.

### Reading the history honestly

Two properties of this rollup are easy to get wrong, and both have burned us:

- **`uptime_pct = mean(status == 'operational')` over a day's samples**, and a
  region's figure is an unweighted mean over its component series. A component
  that is permanently wrong therefore costs the whole region a fixed slice —
  which is how a disabled Brave key published 73.2% uptime on a day when nothing
  was down. Never record a series that can only take one value.
- **`outage_count` counts incidents** — transitions into `outage`, not samples in
  `outage`. Summing samples reported a single stuck component as "1438 outages
  in a day", a number with no meaning to a reader.

Repairs for both live in `history::init` and run once, idempotently, at boot.

### Monitoring billable providers — the right way

For a **paid** provider with no free health endpoint (Brave dropped its free tier
in Feb 2026 → metered, every request billed), the correct pattern — and the
industry consensus (real-user / passive monitoring) — is **don't synthetic-probe
it at all.** Derive its health from the **real traffic your stack already pays
for**: the LLM proxy reports each provider's health in its own `/v1/status`
(success/latency of actual searches), and this service folds that into
`internal_providers`. Zero extra cost, and a *truer* signal (it reflects whether
your key + quota actually work, which a synthetic probe can't tell you).

So for Brave: **leave `status.external.brave.url` unset** — its status comes from
the proxy.

Three tiers, safest first:
1. **Passive (recommended for paid APIs):** unset `status.external.<p>.url`;
   health comes from the proxy's `/v1/status`. No probe, no charge.
2. **Direct keyless probe (default once `status.external.<p>.url` is set):**
   reachability only, no key sent → no billable call (paid APIs reject the
   unauthenticated request before billing). An independent liveness signal.
3. **Direct authenticated probe (`status.external.<p>.auth = true`):** sends the
   live key — **billable for metered providers.** Opt-in per provider, and only
   for one with a genuinely free health endpoint. Logged with a warning at runtime.

The uptime-history poller never probes external providers at all (its provider
rows come from the proxy reports), so the recurring loop can't incur charges.

## Run

```sh
# zero-env: the only inputs are --home and --key-id (both optional, defaults shown).
cargo run --release -- --home /var/lib/ciris --key-id ciris-status
# or the built binary (read API + status routers on the RET port + 1, default :4243):
./ciris-status --home /data --key-id ciris-status
```

There is one binary now — it is always a node, and it takes no env. Point the
status reverse-proxy at the read-API listener (the RET port + 1, default `:4243`).
After it is up, claim ownership and author the adapter `config:*` (above) + a
`consent:replication` grant — see [`DEPLOY.md`](DEPLOY.md).

## Deploy (replacing the Lens API container)

**See [`DEPLOY.md`](DEPLOY.md)** for the full runbook: the GHCR image, the
zero-env CLI boot, the owner-authored `config:*` / peering, the lens→status
cutover ordering, and the DNS/Caddy/nginx routing.

Build:

```sh
docker build -t ciris-status .
```

Point the existing nginx `location /lens/api/` upstream at the node's read-API
listener (`:4243`). The nginx mapping is unchanged except the port:

```
location /lens/api/ { proxy_pass http://127.0.0.1:4243/; }   # strips /lens/api
location /lens/health { proxy_pass http://127.0.0.1:4243/health; }
```

So `agents.ciris.ai/lens/api/v1/status` → `/v1/status`,
`…/lens/api/api/v1/status` → `/api/v1/status` (the double `api` is nginx stripping
only `/lens/api/`, preserved from the Lens layout).

## What it is NOT

Out of scope by design (retires with Lens): Grafana dashboards, Mimir/Loki/Tempo,
the OTLP/manager collectors, OAuth/admin routes, the data-ingest pipeline,
`persist_engine`. This service is only the public status surface.
