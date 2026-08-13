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
   0 |# #        |   V verify
   1 |# #  GGGGG |                 runs 1-5  (oldest first)
   2 |# #        |
   3 |# #  GGGGG |                 runs 6-10 (newest last)
   4 | #         |
   5 |        ## |   P persist     letter on the far edge …
   6 | b##G#  # #|                 … runs on the near one
   8 | G#b#G  #  |
  10 |###        |   E edge
  11 |#    GGRGG |                 a failure in the older five
  15 |         ##|   S server
  18 | GGGGY    #|                 newest run in progress
  20 | #         |   A agent
  21 |# #  GGG.. |                 a young repo: only three runs
  25 |       Y   |   region header: column 3 = three dots
  26 |     Y Y   |                 column 2 = two dots
  27 |   Y Y Y   |                 column 1 = one dot
  28 |        ## |   B billing
  30 |   G G  ## |                 dots in the fixed centre columns …
  33 |##         |   P proxy
  35 |## Y Y     |                 … so they line up under the header
  38 |        ## |   D database
  40 |   G G  # #|
  43 |#    G G Y |   L providers
  48 |   G G G   |   I infra
     +-----------+

     x=      3 5 7   = US, EU, GLOBAL
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
GitHub Actions runs as **two rows of five dots** — the older five, then the
newer five — on rows 2 and 4 of the letter's five, with a blank row between so
they cannot fuse. Centring them against the glyph keeps the pair optically tied
to its letter instead of floating above dead space.

| Run | Colour |
|---|---|
| success | green |
| failure | red |
| in progress | amber, **pulsing** — the only moving thing on the board |
| queued | dim blue |
| cancelled / skipped | grey (deliberately *not* red — superseded PR pushes cancel runs constantly) |
| no data yet | near-black |

**Region header (rows 25–27)** — one column per block, its **height counting the
column**: one dot for the first, two for the second, three for the third, so
there is no legend to memorise. Each column is lit in that block's worst-of
rollup colour, so the header is also a per-region summary — strictly more than
the dotted divider it replaced, in the same space.

Known limit: three header rows can count to three. A fourth or fifth block would
pack into adjacent columns but their heights would both cap at three and the
count would start lying. Three blocks (US, EU, GLOBAL) is what exists today.

**Services (rows 28–52)** — B/P/D/L/I: billing, proxy, database, LLM providers,
infrastructure. One dot per block, centred on the letter's middle row, in the
**fixed centre columns 3/5/7** — the only columns never covered by a glyph,
which is what lets them line up under the header. Sorted **west → east** (US
left of EU, like a map), with GLOBAL one column further along — whatever belongs to no region. Adding
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
