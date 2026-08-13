# Galactic Unicorn status board

A physical status board for the CIRIS stack: a [Pimoroni Galactic
Unicorn](https://shop.pimoroni.com/products/galactic-unicorn) (53×11 RGB matrix
on a Pico W), **mounted portrait** — stood on its end with the USB/power
connector at the bottom, giving an 11-wide, 53-tall board.

Everything in `main.py` is written in those viewer coordinates; `vpixel` /
`vrect` rotate into panel space on the way out. Two fixed sections, nothing
floats, nothing cycles.

```
     +-----------+
   0 |# #        |   V — CIRISVerify, a 3x5 letter …
   1 |# #        |
   2 |# #        |
   3 |# #        |
   4 | #         |
   5 |GGGGGGGGGG |   … with all 10 runs beneath it, oldest left, newest right
   6 |GGGGGGGGGG |
   7 |##         |   P — CIRISPersist
  12 |b##G#G#b#G |      queued / cancelled churn
  14 |###        |   E — CIRISEdge
  19 |GGRGGGGGGG |      a failure three runs back
  21 | ##        |   S — CIRISServer
  26 |GGGGGGGGGY |      newest run in progress (pulsing amber)
  28 | #         |   A — CIRISAgent
  33 |GGG....... |      a young repo draws a short centipede
  35 |Y Y Y Y Y Y|   overall status
  37 |GGG GGG    |   health: billing          US | EU | GLOBAL
  40 |YYY YYY    |   health: proxy
  43 |GGG GGG    |   health: databases
  46 |GGG GGG GGY|   health: providers
  49 |GGG GGG GGG|   health: infrastructure
     +-----------+
```

**Centipedes (rows 0–34)** — one band per repo, all five visible at once. The
repos are the substrate in dependency order: verify → persist → edge → server →
agent. Each band is a 3×5 letter with that repo's ten most recent GitHub Actions
runs as a full-width bar directly beneath it.

| Run | Colour |
|---|---|
| success | green |
| failure | red |
| in progress | amber, **pulsing** — the only moving thing on the board |
| queued | dim blue |
| cancelled / skipped | grey (deliberately *not* red — superseded PR pushes cancel runs constantly) |
| no data yet | near-black |

**Divider (row 35)** — a dotted line carrying the aggregate `status`:
green, amber, or red (`partial_outage` and `major_outage` both read red).

**Health grid (rows 37–50)** — completely static, with a blank row between
categories: without it, adjacent rows of the same colour fuse into one tall
block and the five categories read as an arbitrary stack of boxes. Three column
blocks sorted **west → east** — US, EU, then GLOBAL for what belongs to no
region — and five rows, top to bottom: billing, proxy, databases, providers,
infrastructure.
Adding a region re-widths the blocks with no code change. Green operational,
amber degraded, red outage, dim blue unknown; where a row has several components
in one block they share it as sub-cells.

**Blue means "we don't know", never "it's fine."** The two feeds go stale
independently: no successful `/api/v1/status` for 90 s turns the health grid
blue, and no successful `/api/v1/ci` for 3 minutes turns the centipedes blue,
each without touching the other.

## Orientation

The default is a counter-clockwise rotation, which is correct when the panel
stands with its connector at the **bottom**. If yours is mounted the other way
up, **press button C** — it flips 180° and writes `orientation.txt` to the
device, so the setting survives a power cycle and nobody has to reflash.

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

Buttons: **A** refreshes both feeds, **C** flips orientation, **LUX +/−** adjust
brightness.

## What it reads

```
GET https://lens.ciris-services-1.ai/status/api/v1/status   (30s)
GET https://lens.ciris-services-1.ai/status/api/v1/ci       (60s)
```

`/api/v1/ci` exists because the Pico cannot poll GitHub itself: the
unauthenticated Actions API allows 60 requests/hour per IP (five repos per
refresh burns that quickly) and each `actions/runs` response is ~120 KB
(measured: 124,809 bytes for CIRISServer) — five of those would flatten the
device's heap. The service polls GitHub on its own cadence with conditional
requests and serves a ~600-byte projection. See `src/ci.rs`; the repos, owner,
token and cadence are `status.ci.*` config keys.

## History

This lived in `CIRISBridge/extras/galactic-unicorn/` and polled CIRISLens at
`lens.ciris-services-1.ai/lens-api/api/v1/status`. That route retired with Lens
and returns `404 — lens retired`, so the board sat on 18 blue bubbles. Taken
over here, alongside the service that now serves its data, and rebuilt as a
fixed-layout board.
