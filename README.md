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
history written by the adapter's poll loop. No Grafana, no TimescaleDB, no OAuth,
no ingest pipeline — those retire with Lens.

> **Release note:** `ciris-status` currently depends on `ciris-server` as a path
> dep (`../CIRISServer`). The release pin (`{ git = "…/CIRISServer", tag }`) lands
> once the adapter-seam `ciris-server` is tagged.

## Endpoints

Drop-in for the Lens nginx route (`agents.ciris.ai/lens/api/…` → this service):

| Route | What it does |
|---|---|
| `GET /health` | Liveness: `{status:"healthy", timestamp, version}` |
| `GET /v1/status` | Local providers (postgresql + grafana), live, only if configured |
| `GET /api/v1/status` | Aggregated multi-region: regions (billing/proxy), infrastructure (Vultr/Hetzner/GHCR), LLM/auth/database/internal provider buckets — all live |
| `GET /api/v1/status/history?days=&region=` | Daily uptime rollup from SQLite. `days` 1–365 (default 30), `region` ∈ `us\|eu\|global` |
| `GET /api/v1/scoring` | **Public scoring roster** (Flow A): opted-in agents `{key_id, capacity_composite, factors?, valid_until}`, consent-gated. Replaces lens-python's scoring feed. Served from cache, populated from this node's OWN corpus by the adapter loop. |
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

## Configuration (env)

Env splits in two: the **node** env (read by `ciris_server::ServerConfig`) and the
**StatusAdapter** env (probe targets, cadence, CORS — read by `src/config.rs`).

### Node env (`ciris-server`'s — identity, listen, peering)

| Var | Default | Meaning |
|---|---|---|
| `CIRIS_HOME` / `CIRIS_SERVER_DATA_DIR` / `CIRIS_SERVER_IDENTITY_DIR` | `~/ciris/…` | data + identity dirs. The corpus is **always** SQLite at `<data>/ciris_engine.db` — there is **no `CIRIS_DB_URL`/DSN env**, and this node's corpus is its **OWN** (never share dirs with / mount the lens node's DB). |
| `CIRIS_SERVER_LISTEN_ADDR` | `0.0.0.0:4242` | the Reticulum node port; the read API (and the status routers) bind `port + 1` (`:4243`) |
| `CIRIS_SERVER_KEY_ID` | `ciris-server` | the node's federation `key_id` (the `health:liveness` attester) |
| `CIRIS_SERVER_BOOTSTRAP_PEERS` | — | comma-sep `host:port` Reticulum peers to join the mesh — **set to Node A for cross-host replication** (how this node reaches the `CIRIS_PEER_B_*` peer) |
| `CIRIS_PEER_B_KEY_ID` / `CIRIS_PEER_B_KEY_RECORD` | — | the `consent:replication` peer (Node A / the lens node) — its `key_id` + exported self-signed `SignedKeyRecord` JSON. A's `capacity:*` arrives **only** via this leg, into this node's own corpus. |
| `CIRIS_SERVER_TRANSPORT_NODE` / `CIRIS_SERVER_STORE_AND_FORWARD` | `on` | NAT-traversal infra (relay + mail-for-asleep-edges) |

The full node env reference is `ciris-server`'s `src/config.rs`.

### StatusAdapter env (probe targets, cadence, CORS)

Every probe target is optional — an unset `*_URL` simply omits that component.

| Var | Default | Meaning |
|---|---|---|
| `STATUS_DB_PATH` | `status.db` | SQLite **uptime-history** file (the status page's own store; distinct from the node corpus) |
| `STATUS_POLL_SECONDS` | `60` | probe + roster-refresh + history poll cadence |
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
# or the built binary (node listens 0.0.0.0:4242, read API + status routers :4243):
CIRIS_SERVER_LISTEN_ADDR=0.0.0.0:4242 STATUS_DB_PATH=/var/lib/ciris-status/status.db ./ciris-status
```

There is one binary now — it is always a node. Point the status reverse-proxy at
the read-API listener (`CIRIS_SERVER_LISTEN_ADDR` port + 1, default `:4243`).

## Deploy (replacing the Lens API container)

**See [`DEPLOY.md`](DEPLOY.md)** for the full runbook: the GHCR image, the env
reference, the lens→status cutover ordering, and the DNS/Caddy/nginx routing.

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
