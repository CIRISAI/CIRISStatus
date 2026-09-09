# Deploying ciris-status — a ciris-server node + the StatusAdapter

This is the operator runbook for standing up **Node B** of
`FSD/MONITORING_NODE_DESIGN.md`. ciris-status is now **a `ciris-server` fabric
node + a `StatusAdapter`** (not a parallel federation impl): the whole node —
identity, the OWN local corpus, `consent:replication` peering, A↔B replication,
the read API — is `ciris-server`'s `serve_with_adapter`; the status page is the
adapter. Node B owns its **OWN local corpus**: Flow A reads `capacity:*` signed
`scores` from **B's own corpus** and Flow B emits signed
`observation:reachability` rows into it. Node A's `capacity:*` lands in B's corpus by **consented anti-entropy
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

### Boot inputs (CLI flags — two that shape the node, one that opens a door)

| Flag | Default | Meaning |
|---|---|---|
| `--home <path>` | `/var/lib/ciris` | the data root. `data_dir = <home>/data`; the corpus is `<data_dir>/ciris_engine.db`, the minted Ed25519 + ML-DSA-65 identity lives under `<home>`, and the uptime-history DB is **derived** as `<data_dir>/status.db`. The docker-compose deploy passes `--home /data` (the mounted volume). |
| `--key-id <name>` | `ciris-status` | this node's federation `key_id` — the observation attester. `serve_with_adapter` self-registers it at boot, so Flow B rows admit with no extra step. |
| `--diagnostics` | off | mount `GET /api/v1/debug/memory` (the `mallinfo2` live/free split). `CIRIS_DIAGNOSTICS=1` does the same thing — ciris-server's own switch, read through its own parser so the truthy set cannot drift. See below. |

```sh
ciris-status --home /data --key-id ciris-status   # docker-compose passes this as command:
```

The listen address, transport/NAT-traversal toggles, replication cadence, and
mode are themselves the **node's** `config:*` CEG (resolved at boot, hot-applied)
— see `ciris-server`'s `src/config.rs`. No `CIRIS_*` env configures the NODE;
`CIRIS_DIAGNOSTICS` is the substrate's own debug switch, not adapter config.

#### Diagnostics — off by default, and there ARE two ways to open it

`GET /api/v1/debug/memory` is not mounted unless asked for, because it answered
unauthenticated on the published port (CIRISStatus#73):

```yaml
command: ["--home", "/data", "--key-id", "ciris-status", "--diagnostics"]
# a shorter window:  "--diagnostics=30"
# or
environment:
  CIRIS_DIAGNOSTICS: "1"
```

**The window closes itself.** `--diagnostics` opens for 120 minutes by default;
`--diagnostics=<5..240>` sets it. After that the route answers 404 — the same
thing a caller sees when it was never mounted.

**A restart does not renew it.** The deadline is absolute and lives on the data
volume (`<data_dir>/diagnostics-window`), so a crash or `restart: unless-stopped`
bounce inside the window RESUMES the original deadline, and a restart after it
stays closed. Opening a new window is a deliberate act — remove the marker file. A marker that
exists but is empty, malformed or unreadable keeps diagnostics CLOSED rather
than being treated as absent: it is written atomically (temp + rename), so an
unreadable one means something went wrong, and something-went-wrong is not
permission to reopen an unauthenticated route.
Restarting is not consent, and a container that bounces on its own must not be
able to hold an unauthenticated route open.

Boot says which happened: `diagnostics OPEN`, `diagnostics RESUMED` with the
seconds left, or `diagnostics requested but the window is SPENT`.

> `--diagnostics=1` is REFUSED with a migration error. 0.3.71 accepted that
> spelling and ignored the value, so it meant "on"; here a number is minutes,
> and silently turning it into a one-minute window would close the endpoint
> during boot. Use bare `--diagnostics`. Nothing has to be remembered, and a
session that ends early takes no exposure with it. Boot logs which opener fired
and how long the window is, so both are checkable rather than assumed.

> Why an expiry and not just a switch: the route is not loopback-bound here, so
> while it is open the edge proxy is the only thing between allocator internals
> and the internet. That makes "remember to turn it off" a security control, and
> it failed the first time it was used — a reading that finished at 16:37 left
> the endpoint answering until 20:31, five and a half hours instead of the
> planned hundred minutes. The measurement was made resilient to the operator's
> machine dying; the closing was not.

**It is NOT loopback-bound here.** ciris-server pairs its gate with
`require_loopback`; an adapter router cannot — that guard is not exported and
the read-API listener does not hand us `ConnectInfo`. With diagnostics on the
route answers from wherever the port reaches, and the edge is what keeps it off
the internet. Turn it on for a reading, turn it back off.

> **0.3.69 and 0.3.70 shipped the gate with NO opener.** `diag::enable()` is
> called from ciris-server's own binary entry point, which this binary does not
> run, so the switch was permanently false: the route was gone, not gated, and
> no flag or env could bring it back. Fixed in 0.3.71.

> **The corpus is its OWN** — `<home>/data/ciris_engine.db`. Never share `--home`
> with the lens node or bind-mount the lens node's `data/`. Node A's `capacity:*`
> arrives **only** by the consent:replication leg below.

### Memory: what the ansible role should and should NOT set

**Do not set `MALLOC_ARENA_MAX` in compose.** The binary caps glibc's arenas
itself, before it builds its runtime (`diag::cap_malloc_arenas`). That keeps the
zero-env contract above intact: the one knob this process needs is not a CIRIS
env var, and a deployment file is a place a fix can quietly stop being applied —
a stack redeploy, a new host, a second node stood up from the same image.

An explicit value in the environment still wins, because glibc reads it before
we run and re-deciding underneath an operator who stated a number would make
compose and the binary disagree about the same setting. Which path ran is in the
log at boot, so it is checkable rather than assumed:

```text
capped glibc malloc arenas (CIRISStatus#69: -847MB committed, live unchanged)
MALLOC_ARENA_MAX set in the environment — leaving the operator's value alone
```

**Size `mem_limit` from a measurement, not from history — and be ready to raise
it.** The US node's original 1536m was sized against an UNCAPPED process holding
~1.46GB committed, of which ~1.4GB was allocator free-list and ~44MB was live.
Capped, the same work settles near ~611MB, so the limit was cut to 896m — and
that turned out to be too tight: `memory.events max` climbed to 6,821 in 43
minutes of steady state, with swap at only 40MB. **That was CACHE reclaim, not
anon thrashing** — never an OOM risk, but this node reads its corpus constantly
and evicting those pages repeatedly is the same class of cost as the first-load
latency the status page is judged on. It now runs at 1216m with `max` back to 0.

The rule that catches this is below, and it fired against the person who wrote
it: **if `max` climbs, the limit goes UP.** After deploying, read the figure
rather than guessing:

```sh
curl -s localhost:4253/api/v1/debug/memory | jq '{
  live: .mallinfo2.uordblks, held: .mallinfo2.fordblks,
  live_fraction, rss: .proc.RssAnon, swap: .proc.VmSwap }'
```

Then set `mem_limit` to roughly 1.5x the settled `RssAnon`, and confirm the
cgroup is not fighting it:

```sh
cat /sys/fs/cgroup/system.slice/docker-$(docker inspect -f '{{.Id}}' ciris-status).scope/memory.events
# `max` climbing = the limit is being enforced continuously; raise it.
```

Headroom returned here is not free money — it is headroom `ciris-server` needs
on the same host (CIRISServer#551), so it is worth reclaiming deliberately.

**`malloc_trim` is a separate lever, and `keepcost` does NOT predict it.**
`status.malloc_trim_secs` (default 0, off) calls `malloc_trim(0)` on a cadence.
Measured on the canonical: `keepcost` was 3.9KB and the trim returned **172MB**
of residency — `RssAnon` 271MB → 113MB — while `fordblks` and `arena` did not
move at all. Since glibc 2.8 trim also `MADV_DONTNEED`s free pages INSIDE every
arena, so it reaches what `keepcost` (top of the main arena only) says nothing
about. It is off by default because the pages fault back in on reuse, which is a
real cost for a churn workload; turn it on as an A/B, with the before/after in
the log line it emits.

Related, and worth not misreading: `fordblks` is address space, not resident
memory. A node showing 769MB of free list against 313MB committed is not holding
769MB.

**Do not copy the arena cap to `ciris-server` expecting this result.** Same
pathology, different composition: its largest single region is a ~706MB brk
`[heap]`, which an arena cap does not consolidate.

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
| `status.ci.{owner,repos,token,poll_secs}` | str/list/str/i64 | `CIRISAI` / the substrate five / — / `300` |

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
observations; roster stays empty until a grant is authored).

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
   Verify Flow B emits: look for `Flow B: emitted signed observation:reachability:v1`
   with an `emitted=`/`skipped=` count. Emission is **change-driven**: a target
   whose verdict moved is signed on the next probe cycle, and one that has not
   moved is re-signed only every `status.observation_secs` (900s default). A
   quiet fabric therefore logs `Flow B: nothing new to attest` at DEBUG and
   nothing at INFO — that is the healthy steady state, not a stall. A non-zero
   `failed=` names the target on its own warning line.

   Retention runs every 10 minutes and logs `retention: pruned our own expired
   observation rows` with `purged=`/`more=`. `more=true` means it took its
   bounded bite and will continue next pass — expected while draining a
   backlog.

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
