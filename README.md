# ciris-status

The small standalone service that serves **ciris.ai's public health/status
surface** — the subset CIRISLens's API serves today, lifted out so the status page
survives Lens's retirement.

Pure outbound HTTP probes + a SQLite uptime history written by a 60-second poller.
No Grafana, no TimescaleDB, no OAuth, no ingest pipeline — those retire with Lens.

## Endpoints

Drop-in for the Lens nginx route (`agents.ciris.ai/lens/api/…` → this service):

| Route | What it does |
|---|---|
| `GET /health` | Liveness: `{status:"healthy", timestamp, version}` |
| `GET /v1/status` | Local providers (postgresql + grafana), live, only if configured |
| `GET /api/v1/status` | Aggregated multi-region: regions (billing/proxy), infrastructure (Vultr/Hetzner/GHCR), LLM/auth/database/internal provider buckets — all live |
| `GET /api/v1/status/history?days=&region=` | Daily uptime rollup from SQLite. `days` 1–365 (default 30), `region` ∈ `us\|eu\|global` |
| `GET /api/v1/scoring` | **Public scoring roster** (Flow A): opted-in agents `{key_id, capacity_composite, factors?, valid_until}`, consent-gated. Replaces lens-python's scoring feed. Served from cache (empty in the default build; populated when run as a fabric node). |
| `GET /api/v1/scoring/live`, `GET /api/v1/status/live` | **SSE** live-push of roster + overall-health deltas (the "extra website sockets"). |
| `GET /api/v1/status/ws` | **WebSocket** variant of the same live-push. |

### Fabric node (monitoring cards — `--features fabric`)

The default build is the cost-safe outbound prober + SQLite uptime + the website
sockets (roster served from cache). Built with `cargo build --features fabric`
and configured (see `.env.example`), ciris-status becomes **Node B** of
`FSD/MONITORING_NODE_DESIGN.md`:

- **Flow B** — each poll, probe results become a signed CEG `scores` attestation
  on dimension `health:liveness:v1` (`witness_relation: external`,
  operational/degraded/outage → `+1/0/-1`), per keyed CIRIS service. Non-keyed
  infra (LLM/search providers, regions) folds in as `evidence_refs`, not as
  separate subjects. Hybrid-signed (Ed25519 + ML-DSA-65) via persist v8.4.0 /
  verify v5.10.0 and written with `FederationDirectory::put_attestation`.
- **Flow A** — reads `capacity:*` `scores` from the corpus (public-tier
  `CallerScope::Unauthenticated`, i.e. the consent / `public_sample` projection)
  and projects the roster `/api/v1/scoring` serves.

Cost discipline is unchanged: Flow B reuses the same aggregated probe and never
authed-probes paid providers in the loop.

Response shapes match the Lens API field-for-field (status strings
`operational\|degraded\|outage`; aggregate overall
`operational\|degraded\|partial_outage\|major_outage`).

## Configuration (env)

Every probe target is optional — an unset `*_URL` simply omits that component.

| Var | Default | Meaning |
|---|---|---|
| `STATUS_LISTEN_ADDR` | `127.0.0.1:8200` | bind address (Lens prod published 8200) |
| `STATUS_DB_PATH` | `status.db` | SQLite history file |
| `STATUS_POLL_SECONDS` | `60` | uptime poll cadence |
| `DATABASE_URL` | — | local `postgresql` provider (TCP liveness probe) |
| `GRAFANA_URL` | — | local `grafana` provider (`/api/health`) |
| `US_BILLING_URL` / `US_PROXY_URL` / `VULTR_HEALTH_URL` | — | US region |
| `EU_BILLING_URL` / `EU_PROXY_URL` / `HETZNER_HEALTH_URL` | — | EU region |
| `GHCR_HEALTH_URL` | `https://ghcr.io/v2/` | container registry (401 = up, 3s threshold) |
| `{EXA,BRAVE,SERPER,TAVILY}_HEALTH_URL` / `_API_KEY` | — | external search providers (see cost note) |
| `{EXA,BRAVE,SERPER,TAVILY}_HEALTH_AUTH` | `false` | send the live key when probing — **billable for some providers** |

### Monitoring billable providers — the right way

For a **paid** provider with no free health endpoint (Brave dropped its free tier
in Feb 2026 → metered, every request billed), the correct pattern — and the
industry consensus (real-user / passive monitoring) — is **don't synthetic-probe
it at all.** Derive its health from the **real traffic your stack already pays
for**: the LLM proxy reports each provider's health in its own `/v1/status`
(success/latency of actual searches), and this service folds that into
`internal_providers`. Zero extra cost, and a *truer* signal (it reflects whether
your key + quota actually work, which a synthetic probe can't tell you).

So for Brave: **leave `BRAVE_HEALTH_URL` unset** — its status comes from the proxy.

Three tiers, safest first:
1. **Passive (recommended for paid APIs):** unset `*_HEALTH_URL`; health comes
   from the proxy's `/v1/status`. No probe, no charge.
2. **Direct keyless probe (default if `*_HEALTH_URL` set):** reachability only, no
   key sent → no billable call (paid APIs reject the unauthenticated request
   before billing). An independent liveness signal.
3. **Direct authenticated probe (`*_HEALTH_AUTH=true`):** sends the live key —
   **billable for metered providers.** Opt-in per provider, and only for one with
   a genuinely free health endpoint. Logged with a warning at startup.

The 60s history poller never probes external providers at all (its provider rows
come from the proxy reports), so the recurring loop can't incur charges.

## Run

```sh
cargo run --release
# or the built binary:
STATUS_LISTEN_ADDR=127.0.0.1:8200 STATUS_DB_PATH=/var/lib/ciris-status/status.db ./ciris-status
```

## Deploy (replacing the Lens API container)

Build the image and point the existing nginx `location /lens/api/` upstream at it
(it listens on the same `:8200`). See `Dockerfile`. The nginx mapping is unchanged:

```
location /lens/api/ { proxy_pass http://127.0.0.1:8200/; }   # strips /lens/api
location /lens/health { proxy_pass http://127.0.0.1:8200/health; }
```

So `agents.ciris.ai/lens/api/v1/status` → `/v1/status`,
`…/lens/api/api/v1/status` → `/api/v1/status` (the double `api` is nginx stripping
only `/lens/api/`, preserved from the Lens layout).

## What it is NOT

Out of scope by design (retires with Lens): Grafana dashboards, Mimir/Loki/Tempo,
the OTLP/manager collectors, OAuth/admin routes, the data-ingest pipeline,
`persist_engine`. This service is only the public status surface.
