# Deploying ciris-status — Node B (the public scoring-feed monitor node)

This is the operator runbook for standing up **Node B** of
`FSD/MONITORING_NODE_DESIGN.md`: ciris-status built `--features fabric`, the
lens-python scoring-feed replacement. It reads `capacity:*` signed `scores` from
ciris-server's corpus (Flow A) and emits signed `health:liveness` (Flow B), and
serves the public website surface ciris.ai consumes.

> **Cold-prod-miles warning.** The `fabric` build serves real mesh data but has
> not yet run in production. Stand it up **alongside** the lens (read-only,
> non-authoritative) and verify the surface before cutting public traffic. The
> cutover ordering below is designed so every step is independently reversible.

---

## 1. Artifacts (the contract the CIRISBridge ansible role builds against)

| Artifact | What |
|---|---|
| `ghcr.io/cirisai/cirisstatus:<vTAG>` | **Node B** image — `--features fabric`. Serves real data. |
| `ghcr.io/cirisai/cirisstatus:<vTAG>-status` | status-page-only image (default build; prober + cache). |
| `ghcr.io/cirisai/cirisstatus:latest` / `:status` | rolling tags for the two above. |
| `docker-compose.fabric.yml` | Node B compose: image + corpus wiring + node identity + listen addr. |
| GitHub Release `ciris-status-<vTAG>-<target>.tar.gz` | stripped fabric binary (x86_64 + aarch64), Sigstore-signed. |

Built by `.github/workflows/release.yml` on a `v*` tag. The image is the blessed
release; the ansible role pulls it and templates `.env` from the reference below.

---

## 2. Environment reference

Copy `.env.example` → `.env`. The non-fabric probe/uptime vars are documented in
`README.md`. The **fabric (Node B)** vars — ALL required or the flows stay
disabled (binary degrades to the plain prober; roster stays empty):

| Var | Example | Meaning |
|---|---|---|
| `STATUS_LISTEN_ADDR` | `0.0.0.0:8200` | bind address (host maps `127.0.0.1:8200`) |
| `STATUS_DB_PATH` | `/data/status.db` | Node B's own uptime SQLite (NOT the corpus) |
| `STATUS_CORPUS_DSN` | `sqlite:///corpus/corpus.db` | the **shared ciris-server corpus** (Flow A reads, Flow B writes) |
| `STATUS_NODE_KEY_ID` | `ciris-status-monitor` | Node B's Ed25519 identity key_id |
| `STATUS_NODE_KEY_PATH` | `/secrets/ed25519.seed` | 32-byte raw Ed25519 seed, `chmod 600` |
| `STATUS_NODE_PQC_KEY_ID` | `ciris-status-monitor-pqc` | Node B's ML-DSA-65 key_id (hybrid signing) |
| `STATUS_NODE_PQC_KEY_PATH` | `/secrets/mldsa65.seed` | 32-byte raw ML-DSA-65 seed, `chmod 600` |
| `STATUS_SERVICE_KEYS` | `us=k_service_us,eu=k_service_eu` | region-key → the keyed CIRIS service node key_id Flow B attests `health:liveness` about (providers/regions fold in as `evidence_refs`, never as separate subjects) |

**Node identity.** Node B is a distinct fabric node (`device_class: service`),
NOT the lens identity (that is Node A — ciris-server). Generate fresh seeds:

```sh
mkdir -p secrets && chmod 700 secrets
head -c 32 /dev/urandom > secrets/ed25519.seed   && chmod 600 secrets/ed25519.seed
head -c 32 /dev/urandom > secrets/mldsa65.seed    && chmod 600 secrets/mldsa65.seed
```

Node B's public key must be **registered in the federation directory** (the
founder-quorum admission door, same as any node) so its `health:liveness` rows
admit. Node B reads `capacity:*` / `system:*` with NO special role
(`CallerScope::Unauthenticated` → the §8.1.13.3 broad public tiers).

---

## 3. Cutover ordering (each step reversible)

The lens must stay reachable for DSAR until Phase 3 (substrate-bump-gated); this
cutover covers ONLY the public scoring/status surface (Phase 2 of the design §6).

1. **Deploy Node B (off public traffic).**
   ```sh
   docker compose -f docker-compose.fabric.yml up -d
   curl -fsS http://127.0.0.1:8200/health
   ```
   Confirm logs show `fabric: Node B flows online` (not the "built but not
   configured" line — that means an env var is missing).

2. **Wire it to ciris-server's corpus.** Point `STATUS_CORPUS_DSN` at the shared
   corpus (bind-mount Node A's corpus dir, or a networked/replicated corpus if
   Node A is on another host). Verify Flow A serves **real** rows:
   ```sh
   curl -fsS http://127.0.0.1:8200/api/v1/scoring | jq '.agents[0]'
   # expect {key_id, capacity_composite, factors?, valid_until} — the lens shape.
   ```
   If `agents` is empty: the corpus has no opted-in `capacity:*` rows yet, or the
   DSN is wrong, or Node B's key isn't admitted. (Empty is well-formed, not an
   error — the default cache is also empty.)
   Verify Flow B emits: look for `flow B: emitted signed health:liveness` in the
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
`docker compose -f docker-compose.fabric.yml down`. Node B is the public
*window*, never load-bearing — the federation runs without it.

---

## 4. DNS / Caddy / nginx

Node B needs **its own hostname** (it is a distinct node from Node A; do not
share the lens host). Suggested: `status.ciris.ai` (or reuse the lens public
route family). Node B listens on `127.0.0.1:8200`; the reverse proxy terminates
TLS and forwards.

**Caddy** (TLS + SSE/WS pass-through):

```caddyfile
status.ciris.ai {
    reverse_proxy 127.0.0.1:8200 {
        # SSE (/api/v1/*/live) + WS (/api/v1/status/ws) need streaming, no buffer.
        flush_interval -1
    }
}
```

**nginx** (preserving the existing lens `/lens/api/` route shape, README §Deploy):

```nginx
location /lens/api/  { proxy_pass http://127.0.0.1:8200/; }     # strips /lens/api
location /lens/health { proxy_pass http://127.0.0.1:8200/health; }
# SSE/WS:
location /api/v1/ {
    proxy_pass http://127.0.0.1:8200;
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
