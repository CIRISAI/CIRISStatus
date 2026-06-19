# Deploying ciris-status — a ciris-server node + the StatusAdapter

This is the operator runbook for standing up **Node B** of
`FSD/MONITORING_NODE_DESIGN.md`. ciris-status is now **a `ciris-server` fabric
node + a `StatusAdapter`** (not a parallel federation impl): the whole node —
identity, the OWN local corpus, `consent:replication` peering, A↔B replication,
the read API — is `ciris-server`'s `serve_with_adapter`; the status page is the
adapter. Node B owns its **OWN local corpus**: Flow A reads `capacity:*` signed
`scores` from **B's own corpus** and Flow B emits signed `health:liveness` into
it. Node A's `capacity:*` lands in B's corpus by **consented anti-entropy
replication** that `ciris-server` performs — Node B **never reads Node A's
database directly**.

> **Cold-prod-miles warning.** This serves real mesh data but has not yet run in
> production. Stand it up **alongside** the lens (read-only, non-authoritative)
> and verify the surface before cutting public traffic. The cutover ordering
> below is designed so every step is independently reversible.

---

## 1. Artifacts (the contract the CIRISBridge ansible role builds against)

| Artifact | What |
|---|---|
| `ghcr.io/cirisai/cirisstatus:<vTAG>` | the node image (always a node — the `ciris-server` node + StatusAdapter). |
| `ghcr.io/cirisai/cirisstatus:latest` | rolling tag for the above. |
| GitHub Release `ciris-status-<vTAG>-<target>.tar.gz` | stripped binary (x86_64 + aarch64), Sigstore-signed. |

Built by `.github/workflows/release.yml` on a `v*` tag. The image is the blessed
release; the ansible role pulls it and runs it with the two CLI flags below.

> The optional `fabric` feature is **gone** — there is one build now, and it is
> always a node. There is no "prober-only" image any more.

---

## 2. Configuration — ZERO ENV (ciris-server 0.5 zero-env model)

ciris-status takes **no environment variables**. It boots from two CLI flags and
resolves everything else from **signed CEG objects in its own corpus**, authored
by the OWNER at runtime. `.env.example` documents this (there is nothing to put
in a `.env`).

### Boot inputs (CLI flags — the ONLY two)

| Flag | Default | Meaning |
|---|---|---|
| `--home <path>` | `/var/lib/ciris` | the data root. `data_dir = <home>/data`; the corpus is `<data_dir>/ciris_engine.db`, the minted Ed25519 + ML-DSA-65 identity lives under `<home>`, and the uptime-history DB is **derived** as `<data_dir>/status.db`. The docker-compose deploy passes `--home /data` (the mounted volume). |
| `--key-id <name>` | `ciris-status` | this node's federation `key_id` — the `health:liveness` attester. `serve_with_adapter` self-registers it at boot, so Flow B rows admit with no extra step. |

```sh
ciris-status --home /data --key-id ciris-status   # docker-compose passes this as command:
```

The listen address, transport/NAT-traversal toggles, replication cadence, and
mode are themselves the **node's** `config:*` CEG (resolved at boot, hot-applied)
— see `ciris-server`'s `src/config.rs`. There is no `CIRIS_*` env any more.

> **The corpus is its OWN** — `<home>/data/ciris_engine.db`. Never share `--home`
> with the lens node or bind-mount the lens node's `data/`. Node A's `capacity:*`
> arrives **only** by the consent:replication leg below.

### Adapter config:* (probe targets, poll cadence, CORS) — owner-authored

The StatusAdapter's own config is `config:*` CEG under the `status.` namespace,
read live each poll cycle via `graph_config` (an owner change is picked up with
**no restart**). Author it via the desktop client or `POST /v1/config` after
claiming ownership. Keys (full table in `src/config.rs` / `README.md`):

| key | type | default |
|---|---|---|
| `status.poll_secs` | i64 | `60` |
| `status.cors_origins` | list | baked `ciris.ai` set |
| `status.ghcr_url` | str | `https://ghcr.io/v2/` |
| `status.grafana_url` / `status.database_url` | str | — (skipped) |
| `status.region.<us\|eu>.{name,billing_url,proxy_url,infra_url}` | str | baked label / skipped |
| `status.external.<exa\|brave\|serper\|tavily>.{url,api_key,auth}` | str/bool | skipped / keyless |

A region or external provider is probed **only** when its `*_url` is set. On a
fresh node (no `config:*` yet) the adapter runs with defaults: **no probes**, the
baked CORS allow-list, 60s cadence — correct, not an error.

```sh
curl -X POST https://status.ciris.ai/v1/config \
  -d '{"key":"status.region.us.billing_url","value":"https://billing.us.example/"}'
curl -X POST https://status.ciris.ai/v1/config \
  -d '{"key":"status.poll_secs","value":60}'
```

### `consent:replication` peering — CONSENT-DRIVEN, runtime, no env

Replication is driven by the **fabric**: the corpus's `consent:replication`
objects ARE the desired peer set, and `ciris-server`'s reconcile loop converges
the live runtime to them — fully no-restart on edge v5.1.0 (CIRISEdge#173
resolved). Claim ownership of this node, then author a `consent:replication`
grant naming Node A (desktop client or `POST /v1/federation/peering`). The grant
lands in the corpus → the reconciler picks it up → A's `capacity:*` flows INTO
B's own corpus, live. Unset ⇒ B runs solo (self-registers + emits its own
`health:liveness`; roster stays empty until a grant is authored).

```sh
curl -X POST https://status.ciris.ai/v1/federation/peering \
  -d '{"peer_key_id":"ciris-server-steward","peer_key_record":{...}}'
```

The peer admission gate verifies proof-of-possession — neither side can fabricate
the other's `SignedKeyRecord` (both nodes are on persist v9.0.3). This node logs
its OWN `SignedKeyRecord` (JSON) at boot — hand that to the peer as its
corresponding peer-config artifact; the contract is symmetric. The reachability
mesh path to Node A (the Reticulum bootstrap peer) is itself node `config:*`.

---

## 3. Cutover ordering (each step reversible)

The lens must stay reachable for DSAR until Phase 3 (substrate-bump-gated); this
cutover covers ONLY the public scoring/status surface (Phase 2 of the design §6).

1. **Deploy Node B (off public traffic).**
   ```sh
   docker compose up -d
   curl -fsS http://127.0.0.1:4243/health
   ```
   Confirm the logs show `ciris-status starting as a ciris-server node +
   StatusAdapter (zero-env)`, `StatusAdapter lifecycle running`, and the node's
   self-registration line.

2. **Enable A↔B consented replication.** The corpus is **B's OWN local corpus**
   (the node's `ciris_engine.db` under the data dir, never Node A's DB file). To
   pull A's `capacity:*` INTO it, claim ownership and author a `consent:replication`
   grant naming Node A (§2): `ciris-server` registers the peer's key, emits the
   directed `consent:replication:v1` grant, and runs A↔B replication — live, no
   restart. Hand the peer the `SignedKeyRecord` this node logs at boot so it
   registers + replicates symmetrically. The roster is **empty until replication
   delivers** — that is correct, not an error. Verify Flow A serves **real** rows
   once replication has run:
   ```sh
   curl -fsS http://127.0.0.1:4243/api/v1/scoring | jq '.agents[0]'
   # expect {key_id, capacity_composite, factors?, valid_until} — the lens shape.
   ```
   If `agents` is empty: replication hasn't delivered opted-in `capacity:*` rows
   yet, no consent grant is authored, or this node's key isn't admitted at the peer.
   (Empty is well-formed, not an error.)
   Verify Flow B emits: look for `Flow B: emitted signed health:liveness:v1` in the
   logs after one poll cadence.

3. **Cut the `ciris.ai/ciris-scoring/` public page lens → status.** Repoint the
   front-end / nginx / Caddy route for the public scoring + status surface from
   the lens-python feed to Node B (see §4). The lens still runs read-only;
   **rollback = repoint the route back.** Watch the page for a poll cycle.

4. **THEN hard-cutover ciris-server ↔ lens** (the Node A migration, separate
   from this repo — `CIRISServer/FSD/LENS_TO_SERVER_MIGRATION.md`). Only after
   the public surface is proven on Node B. The lens DSAR path stays up until the
   Phase 3 substrate bump lands (design §6).

To roll Node B back at any point: repoint the public route to the lens feed and
`docker compose down`. Node B is the public
*window*, never load-bearing — the federation runs without it.

---

## 4. DNS / Caddy / nginx

Node B needs **its own hostname** (it is a distinct node from Node A; do not
share the lens host). Suggested: `status.ciris.ai` (or reuse the lens public
route family). Node B listens on `127.0.0.1:4243`; the reverse proxy terminates
TLS and forwards.

**Caddy** (TLS + SSE/WS pass-through):

```caddyfile
status.ciris.ai {
    reverse_proxy 127.0.0.1:4243 {
        # SSE (/api/v1/*/live) + WS (/api/v1/status/ws) need streaming, no buffer.
        flush_interval -1
    }
}
```

**nginx** (preserving the existing lens `/lens/api/` route shape, README §Deploy):

```nginx
location /lens/api/  { proxy_pass http://127.0.0.1:4243/; }     # strips /lens/api
location /lens/health { proxy_pass http://127.0.0.1:4243/health; }
# SSE/WS:
location /api/v1/ {
    proxy_pass http://127.0.0.1:4243;
    proxy_http_version 1.1;
    proxy_set_header Connection "";          # SSE
    proxy_set_header Upgrade $http_upgrade;  # WS (/status/ws)
    proxy_buffering off;
    proxy_read_timeout 1h;
}
```

CORS defaults to the public origins (`ciris.ai`, `www.ciris.ai`,
`agents.ciris.ai`) baked into the binary (`src/config.rs`); override the
allow-list at runtime with the `status.cors_origins` `config:*` key (§2).

**DNS:** point `status.ciris.ai` A/AAAA at the Node B host. If Node B runs on the
same host as the lens, a path route on the existing host works too — but a
dedicated hostname keeps the node boundary clean and the rollback a one-line
route change.

---

## 5. Cost safety (unchanged)

Flow B reuses the same cost-safe aggregated probe — it never authed-probes paid
providers in the loop, and the uptime poller never probes external providers at
all (their health comes from the proxy's `/v1/status`). External providers probe
**keyless by default**; keyed (possibly BILLABLE) probing is opt-in per provider
via the `status.external.<p>.auth = true` config key. See `README.md` "Monitoring
billable providers". Leave `status.external.brave.auth` unset (Brave bills health
checks).
