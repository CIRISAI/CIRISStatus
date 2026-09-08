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
   1 |# #GGGGG   |   runs 1-5  (oldest first)
   2 |# #        |
   3 |# #GGGGG   |   runs 6-10 (newest last)
   4 | #         |
   5 |        ## |   P persist    letter on the far edge …
   6 |   b##G## #|   … dots never move
   7 |        ## |
   8 |   G#b#G#  |
   9 |        #  |
  10 |###        |   E edge
  11 |#  GGRGG   |   a failure in the older five
  12 |##         |
  13 |#  GGGGG   |
  14 |###        |
  15 |         ##|   S server
  16 |   GGGGG#  |
  17 |         # |
  18 |   GGGGY  #|   newest run in progress
  19 |        ## |
  20 | #         |   A agent
  21 |# #GGG..   |   a young repo: only three runs
  22 |###        |
  23 |# #.....   |
  24 |# #        |
  25 |       Y   |   region header: column 3 = three dots
  26 |     Y Y   |   column 2 = two dots
  27 |   Y Y Y   |   column 1 = one dot
  28 |        ## |   B billing
  29 |        # #|
  30 |   G G  ## |   US EU, on the grid the runs use
  31 |        # #|
  32 |        ## |
  33 |##         |   P proxy
  34 |# #        |
  35 |## Y Y     |
  36 |#          |
  37 |#          |
  38 |        ## |   D database
  39 |        # #|
  40 |   G G  # #|
  41 |        # #|
  42 |        ## |
  43 |#          |   L providers
  44 |#          |
  45 |#  G G Y   |   GLOBAL degraded behind two healthy regions
  46 |#          |
  47 |###        |
  48 |        ###|   I infra
  49 |         # |
  50 |   G G G # |
  51 |         # |
  52 |        ###|
     +-----------+

     x=   3 4 5 6 7   = the one grid both halves use
```

Every row is a 3×5 letter with its status as single-pixel dots beside it. Ten
rows fill the board exactly.

**Letters** alternate edges — left, right, left. Ten 5-row letters stacked flush
leave no blank row between them, so two neighbours on the same edge touch and
blur; opposite edges separate them horizontally instead.

**Dots never move.** Letters take either the first three columns or the last
three, so 3–7 are the only five never covered by a glyph, and both halves use
exactly those. A repo's run columns and the service blocks sit on one grid
running down the middle of the board. Dots always read left to right, so run
order and the west→east region order never flip with the letter.

Five columns also means a fourth region needs no layout change. A block beyond
the fifth is **logged, not dropped** — a board that omits a region silently
looks healthy by omission.

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
infrastructure. One dot per block, centred on the letter's middle row, on the
same centre grid the runs use — spaced every other column while they fit, which
reads better and keeps neighbours from merging. Sorted **west → east** (US left
of EU, like a map), with GLOBAL last — whatever belongs to no region. Adding
a region adds a dot, no code change. Each dot is the **worst** status among that
block's components, so a single sick provider cannot hide behind healthy
siblings. Green operational, amber degraded, red outage, dim blue unknown.

`P` appears twice — persist above the divider, proxy below it. The divider and
the differing dot layouts keep them apart.

**Blue means "we don't know", never "it's fine."** The two feeds go stale
independently: no successful `/api/v1/status` for 90 s turns the health grid
blue, and no successful `/api/v1/ci` for 3 minutes turns the centipedes blue,
each without touching the other.

The service also reports staleness of its own: when its poll loop has not
produced a snapshot within the poll window it answers `stale: true` with
`age_seconds`, and the board goes blue on that alone. Otherwise an HTTP 200
carrying a stalled node's last healthy snapshot would render as current
indefinitely — the board would be reporting freshness it does not have.

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
