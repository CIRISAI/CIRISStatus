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
release; the ansible role pulls it and templates `.env` from the reference below.

> The optional `fabric` feature is **gone** — there is one build now, and it is
> always a node. There is no "prober-only" image any more.

---

## 2. Environment reference

Copy `.env.example` → `.env`. Env splits in two: the **node** vars (read by
`ciris_server::ServerConfig`) and the **StatusAdapter** vars (probe/uptime/CORS,
documented in `README.md`).

### Node vars (`ciris-server`'s — identity, corpus, listen, peering)

| Var | Example | Meaning |
|---|---|---|
| `CIRIS_HOME` | `/data/ciris` | base for the data + identity dirs (corpus DB at `<data>/ciris_engine.db`, node seed at `<identity>/ed25519.seed`) |
| `CIRIS_SERVER_DATA_DIR` / `CIRIS_SERVER_IDENTITY_DIR` | — | override the data / identity dirs individually |
| `CIRIS_SERVER_LISTEN_ADDR` | `0.0.0.0:4242` | the Reticulum node port; the **read API + the status routers** bind `port + 1` (`:4243`) |
| `CIRIS_SERVER_KEY_ID` | `ciris-server` | the node's federation `key_id` — the `health:liveness` attester. `serve_with_adapter` self-registers it at boot, so Flow B rows admit with no extra step. |
| `CIRIS_SERVER_TRANSPORT_NODE` / `CIRIS_SERVER_STORE_AND_FORWARD` | `on` | NAT-traversal infra (relay + mail-for-asleep-edges); default on for a public node |
| `CIRIS_SERVER_BOOTSTRAP_PEERS` | `lens.ciris.ai:4242` | comma-separated `host:port` Reticulum peers to join the mesh. **Required for cross-host replication** — set it to the lens node (Node A) so this node can actually *reach* the peer whose key you configure below. Without a mesh path, the `CIRIS_PEER_B_*` key is registered but no `capacity:*` ever arrives. |

The node mints its own Ed25519 + ML-DSA-65 identity on first boot under the
identity dir — there are **no `STATUS_NODE_*` seed vars to manage** any more.

> **The corpus is its OWN** — `<CIRIS_HOME>/data/ciris_engine.db`. There is **no
> `CIRIS_DB_URL`/DSN env**; never share `CIRIS_HOME`/`CIRIS_SERVER_DATA_DIR` with
> the lens node or bind-mount the lens node's `data/`. Node A's `capacity:*`
> arrives **only** by the consent:replication leg below — the old "point Node B at
> Node A's DSN" model is gone.

### `consent:replication` peer (Node A / the lens node) — enables A→B replication

`ciris-server` owns peering. Set these to register Node A's key, emit the directed
`consent:replication:v1` grant, and run A↔B anti-entropy replication so A's
`capacity:*` flows INTO B's own corpus. Unset ⇒ B runs solo (self-registers +
emits its own `health:liveness`, no replication; roster stays empty until rows
are replicated/seeded).

| Var | Example | Meaning |
|---|---|---|
| `CIRIS_PEER_B_KEY_ID` | `ciris-server-steward` | the peer's federation key_id (replication wiring + consent subject) |
| `CIRIS_PEER_B_KEY_RECORD` | `{"record":{…}}` | the peer's exported **self-signed** `SignedKeyRecord` (persist v9.0.3 serde_json, single line). The v8.8.0+ gate verifies the peer's proof-of-possession — neither side can fabricate the other's row from raw pubkeys. |
| `CIRIS_SERVER_BOOTSTRAP_PEERS` | `lens.ciris.ai:4242` | (node var, above) must point at Node A so the consent grant + replication actually have a mesh path to it. |

> Note the env name is `ciris-server`'s `CIRIS_PEER_B_*` (its "peer B" slot is the
> directed-consent peer). The full peering/transport env reference is
> `ciris-server`'s `src/config.rs`.

This node logs its OWN `SignedKeyRecord` (JSON) at boot — hand that to the peer as
its corresponding peer-config artifact; the contract is symmetric.

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
   StatusAdapter`, `StatusAdapter lifecycle running`, and the node's
   self-registration line.

2. **Enable A↔B consented replication.** The corpus is **B's OWN local corpus**
   (the node's `ciris_engine.db` under the data dir, never Node A's DB file). To
   pull A's `capacity:*` INTO it, set the `CIRIS_PEER_B_*` vars (§2): `ciris-server`
   registers the peer's key, emits the directed `consent:replication:v1` grant,
   and runs A↔B replication. Hand the peer the `SignedKeyRecord` this node logs at
   boot so it registers + replicates symmetrically. The roster is **empty until
   replication delivers** — that is correct, not an error. Verify Flow A serves
   **real** rows once replication has run:
   ```sh
   curl -fsS http://127.0.0.1:4243/api/v1/scoring | jq '.agents[0]'
   # expect {key_id, capacity_composite, factors?, valid_until} — the lens shape.
   ```
   If `agents` is empty: replication hasn't delivered opted-in `capacity:*` rows
   yet, the peer env is unset/wrong, or this node's key isn't admitted at the peer.
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

CORS for the public origins (`ciris.ai`, `www.ciris.ai`, `agents.ciris.ai`) is
already allow-listed in the binary (`src/config.rs`).

**DNS:** point `status.ciris.ai` A/AAAA at the Node B host. If Node B runs on the
same host as the lens, a path route on the existing host works too — but a
dedicated hostname keeps the node boundary clean and the rollback a one-line
route change.

---

## 5. Cost safety (unchanged)

Flow B reuses the same cost-safe aggregated probe — it never authed-probes paid
providers in the loop, and the 60s uptime poller never probes external providers
at all (their health comes from the proxy's `/v1/status`). See `README.md`
"Monitoring billable providers". Leave `BRAVE_HEALTH_AUTH` unset.
