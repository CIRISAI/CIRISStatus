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
   0 |# #  GGGGG |   V verify      runs 1-5  (oldest first)
   2 |# #  GGGGG |                 runs 6-10 (newest last)
   4 | #         |
   5 | b##G#  ## |   P persist     letter on the far edge …
   7 | G#b#G  ## |                 … dots on the near one
  10 |###  GGRGG |   E edge        a failure in the older five
  15 | GGGGG   ##|   S server
  17 | GGGGY   # |                 newest run in progress
  20 | #   GGG.. |   A agent       a young repo: only three runs
  25 |Y Y Y Y Y Y|   overall status
  26 | GG     ## |   B billing     US EU
  31 |##   YY    |   P proxy
  36 | GG     ## |   D database
  41 |#    GG Y  |   L providers   US EU | GLOBAL
  46 | GG G   ###|   I infra
     +-----------+
```

Every row is a 3×5 letter with its status as single-pixel dots beside it. Ten
rows fill the board exactly.

Rows **alternate edges** — letter left, letter right, letter left. Ten 5-row
letters stacked flush leave no blank row between them, so two neighbours on the
same edge touch and blur into each other; putting them on opposite edges
separates them horizontally instead. The whole band mirrors, letter and dots
together, but the dots always read left to right, so run order and the
west-to-east region order never flip.

**Repos (rows 0–24)** — V/P/E/S/A, the substrate in dependency order: verify →
persist → edge → server → agent. Each row carries that repo's ten most recent
GitHub Actions runs as **two rows of five dots** — the older five on the band's
first row, the newer five two rows below, with a blank row between so they
cannot fuse.

| Run | Colour |
|---|---|
| success | green |
| failure | red |
| in progress | amber, **pulsing** — the only moving thing on the board |
| queued | dim blue |
| cancelled / skipped | grey (deliberately *not* red — superseded PR pushes cancel runs constantly) |
| no data yet | near-black |

**Divider (row 25)** — a dotted line carrying the aggregate `status`:
green, amber, or red (`partial_outage` and `major_outage` both read red).

**Services (rows 26–50)** — B/P/D/L/I: billing, proxy, database, LLM providers,
infrastructure. One dot per region sorted **west → east** (US left of EU, like a
map), then a gap and one dot for GLOBAL — whatever belongs to no region. Adding
a region adds a dot, no code change. Each dot is the **worst** status among that
block's components, so a single sick provider cannot hide behind healthy
siblings. Green operational, amber degraded, red outage, dim blue unknown.

`P` appears twice — persist above the divider, proxy below it. The divider and
the differing dot layouts keep them apart.

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
