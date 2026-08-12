# Galactic Unicorn status display

A physical status board for ciris.ai: a [Pimoroni Galactic
Unicorn](https://shop.pimoroni.com/products/galactic-unicorn) (53×11 RGB matrix
on a Pico W) floating one bubble per monitored component, colored by that
component's status.

| Color | Meaning |
|---|---|
| 🟢 green | `operational` |
| 🟡 yellow | `degraded` |
| 🔴 red | `outage`, `partial_outage`, `major_outage` |
| 🔵 blue | unknown, absent, **or stale** — we could not reach the API |

Blue means *"we don't know"*, never *"it's fine"*. After `STALE_AFTER_MS`
(90 s — three missed polls) with no successful fetch, every bubble goes blue
rather than leaving colors on the wall from a snapshot that may be minutes old.

## Flashing

1. Flash the [Pimoroni MicroPython
   build](https://github.com/pimoroni/pimoroni-pico/releases) for Galactic
   Unicorn onto the Pico W (BOOTSEL + drag the `.uf2`).
2. Copy `secrets.py` to the device:
   ```python
   WIFI_SSID = "…"
   WIFI_PASSWORD = "…"
   ```
3. Copy `main.py` to the device root. It runs at power-on.

Buttons: **A** refreshes immediately, **LUX +/−** adjust brightness.

## What it reads

```
GET https://lens.ciris-services-1.ai/status/api/v1/status
```

That is this service's aggregated endpoint (`src/aggregate.rs`) — the same URL
the ciris.ai status page reads. Change `STATUS_API_URL` in `main.py` if the
route moves.

Bubbles are **derived from the response**, not hardcoded: every region service,
plus every entry in `infrastructure`, `llm_providers`, `database_providers`,
`auth_providers`, and `internal_providers`, plus one for the aggregate
`status`. A new region or provider appears on the next poll with no reflash;
one that disappears loses its bubble instead of floating blue forever.

`MAX_BUBBLES` (24) caps the count so the per-pixel glow keeps ~20 FPS. If the
API returns more, the firmware logs how many it dropped — it never silently
truncates.

## History

This lived in `CIRISBridge/extras/galactic-unicorn/` and polled CIRISLens at
`lens.ciris-services-1.ai/lens-api/api/v1/status`. That route was retired with
Lens and now returns `404 — lens retired`, so the board had been showing 18 blue
bubbles. Taken over here, alongside the service that now serves its data.
