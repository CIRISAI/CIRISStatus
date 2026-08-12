# Galactic Unicorn status board

A physical status board for the CIRIS stack: a [Pimoroni Galactic
Unicorn](https://shop.pimoroni.com/products/galactic-unicorn) (53×11 RGB matrix
on a Pico W). Two fixed sections — nothing floats, nothing drifts.

```
    x=0  2   4                                            52
 y=0  ▐▐  ████ ████ ████ ████ ████ ████ ████ ████ ████ ████   CIRISVerify
   1  ▐▐  ████ ████ ████ ████ ████ ████ ████ ████ ████ ████   CIRISPersist
   2  ▐▐  ████ ████ ████ ████ ████ ████ ████ ████ ████ ████   CIRISEdge
   3  ▐▐  ████ ████ ████ ████ ████ ████ ████ ████ ████ ████   CIRISServer
   4  ▐▐  ████ ████ ████ ████ ████ ████ ████ ████ ████ ████   CIRISAgent
   5  · · · · · · · · · · · · · · · · · · · · · · · · · ·     overall status
   6  ·     [──── US ────] [──── EU ────] [── GLOBAL ──]       billing
   7  ··    [────────────] [────────────] [───────────]        proxy
   8  ···   [────────────] [────────────] [───────────]        databases
   9  ····  [────────────] [────────────] [──] [──] [──]       providers
  10  ····· [────────────] [────────────] [──] [──] [──]       infrastructure
```

**Centipedes (rows 0–4)** — the last 10 GitHub Actions runs per repo, oldest at
the left, newest at the leading edge. Repos are the substrate in dependency
order: verify → persist → edge → server → agent. The 2px tag at the far left is
a fixed per-repo hue (teal, magenta, white, amber, violet) so you can tell rows
apart without counting.

| Run | Colour |
|---|---|
| success | green |
| failure | red |
| in progress | amber, **pulsing** — the only moving thing on the board |
| queued | dim blue |
| cancelled / skipped | grey (deliberately *not* red — superseded PR pushes cancel runs constantly) |
| no data yet | near-black |

**Divider (row 5)** — a dotted line carrying the aggregate `status`. One glance
gives you the whole system: green, amber, or red (`partial_outage` and
`major_outage` both read red).

**Health grid (rows 6–10)** — completely static. Regions are column blocks
sorted **west → east**, so US sits left of EU like a map; a `GLOBAL` block on
the right holds everything belonging to no region. Adding a region inserts a
block and re-widths the row — no code change, no reflash. The tick gutter on the
far left says which row you're looking at:

| Ticks | Row |
|---|---|
| `·` | billing (per region) |
| `··` | proxy (per region) |
| `···` | databases — `us.postgresql` → US block, unprefixed → GLOBAL |
| `····` | providers — LLM providers global, `internal_providers` by prefix |
| `·····` | infrastructure (matched to its region by name) + auth |

Green operational, amber degraded, red outage, dim blue unknown. Where a row has
several components in one block, they share it as sub-cells.

**Blue means "we don't know", never "it's fine."** The two feeds go stale
independently: no successful `/api/v1/status` for 90 s turns the health grid
blue, and no successful `/api/v1/ci` for 3 minutes turns the centipedes blue,
each without touching the other.

## Flashing

1. Flash the [Pimoroni MicroPython
   build](https://github.com/pimoroni/pimoroni-pico/releases) for Galactic
   Unicorn (BOOTSEL + drag the `.uf2`).
2. Copy `secrets.py` to the device:
   ```python
   WIFI_SSID = "…"
   WIFI_PASSWORD = "…"
   ```
3. Copy `main.py` to the device root. It runs at power-on.

Buttons: **A** refreshes both feeds now, **LUX +/−** adjust brightness.

## What it reads

```
GET https://lens.ciris-services-1.ai/status/api/v1/status   (30s)
GET https://lens.ciris-services-1.ai/status/api/v1/ci       (60s)
```

`/api/v1/ci` exists because the Pico cannot poll GitHub itself: the
unauthenticated Actions API allows 60 requests/hour per IP (five repos per
refresh burns that quickly) and each `actions/runs` response is tens of KB —
enough to exhaust the device's heap. The service polls GitHub on its own
cadence with conditional requests and serves a ~600-byte projection. See
`src/ci.rs`; the repos, owner, token and cadence are `status.ci.*` config keys.

## History

This lived in `CIRISBridge/extras/galactic-unicorn/` and polled CIRISLens at
`lens.ciris-services-1.ai/lens-api/api/v1/status`. That route retired with Lens
and returns `404 — lens retired`, so the board sat on 18 blue bubbles. Taken
over here, alongside the service that now serves its data, and rebuilt as a
fixed-layout board.
