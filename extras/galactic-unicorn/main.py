"""
CIRIS Infrastructure Status Display - Pimoroni Galactic Unicorn (53x11)

One floating bubble per monitored component, colored by that component's status.
Bubbles are derived from whatever /api/v1/status actually returns, so a config
change on the server (a new region, a new LLM provider) shows up on the next
poll with no reflash.

Colors:
  GREEN  = operational
  YELLOW = degraded
  RED    = outage / partial_outage / major_outage
  BLUE   = unknown, no data, or STALE (we could not reach the API)

Blue means "we don't know", never "it's fine" — if the fetch fails for
STALE_AFTER_MS the whole display goes blue rather than showing colors from a
snapshot that may be minutes old.

Flash: copy this file to the Pico W as main.py, alongside a secrets.py holding
WIFI_SSID / WIFI_PASSWORD. Buttons: A = refresh now, LUX +/- = brightness.
"""

import network
import urequests
import time
import random
import math
from galactic import GalacticUnicorn
from picographics import PicoGraphics, DISPLAY_GALACTIC_UNICORN

# =============================================================================
# CONFIGURATION
# =============================================================================

WIFI_SSID = "YOUR_WIFI_SSID"
WIFI_PASSWORD = "YOUR_WIFI_PASSWORD"

# ciris-status serves the public status surface that CIRISLens used to. This is
# the same URL the ciris.ai status page reads (see the website's status page);
# CIRISLens's old /lens-api/... route is retired and now 404s.
STATUS_API_URL = "https://lens.ciris-services-1.ai/status/api/v1/status"

try:
    from secrets import WIFI_SSID, WIFI_PASSWORD
except ImportError:
    pass

REFRESH_INTERVAL_MS = 30000
# Past this long without a successful fetch, stop trusting what's on screen.
STALE_AFTER_MS = 3 * REFRESH_INTERVAL_MS
BRIGHTNESS = 0.5
# Frame-rate guard: the glow is drawn per-pixel, so cap how many bubbles float.
MAX_BUBBLES = 24

# =============================================================================
# DISPLAY SETUP
# =============================================================================

gu = GalacticUnicorn()
graphics = PicoGraphics(DISPLAY_GALACTIC_UNICORN)

WIDTH = GalacticUnicorn.WIDTH   # 53
HEIGHT = GalacticUnicorn.HEIGHT  # 11

# Pre-create pens for performance
PEN_BLACK = graphics.create_pen(0, 0, 0)
PEN_GREEN = graphics.create_pen(0, 200, 0)
PEN_GREEN_DIM = graphics.create_pen(0, 80, 0)
PEN_GREEN_BRIGHT = graphics.create_pen(0, 255, 0)
PEN_YELLOW = graphics.create_pen(200, 160, 0)
PEN_YELLOW_DIM = graphics.create_pen(80, 60, 0)
PEN_YELLOW_BRIGHT = graphics.create_pen(255, 200, 0)
PEN_RED = graphics.create_pen(200, 0, 0)
PEN_RED_DIM = graphics.create_pen(80, 0, 0)
PEN_RED_BRIGHT = graphics.create_pen(255, 0, 0)
PEN_BLUE = graphics.create_pen(0, 60, 150)
PEN_BLUE_DIM = graphics.create_pen(0, 25, 60)
PEN_BLUE_BRIGHT = graphics.create_pen(0, 100, 200)

PENS_GREEN = (PEN_GREEN_DIM, PEN_GREEN, PEN_GREEN_BRIGHT)
PENS_YELLOW = (PEN_YELLOW_DIM, PEN_YELLOW, PEN_YELLOW_BRIGHT)
PENS_RED = (PEN_RED_DIM, PEN_RED, PEN_RED_BRIGHT)
PENS_BLUE = (PEN_BLUE_DIM, PEN_BLUE, PEN_BLUE_BRIGHT)

# The aggregate `status` field uses a wider vocabulary than the per-component
# one: partial_outage / major_outage are outages, and must not fall through to
# the blue "unknown" branch — that turned the overall bubble blue at exactly the
# moment it should have been red.
PENS_BY_STATUS = {
    'operational': PENS_GREEN,
    'degraded': PENS_YELLOW,
    'outage': PENS_RED,
    'partial_outage': PENS_RED,
    'major_outage': PENS_RED,
}

# =============================================================================
# METRICS STATE
# =============================================================================

metrics = {}          # name -> status string
last_success_ms = None  # ticks_ms of the last good fetch, None = never


def log(msg):
    print(f"[{time.ticks_ms()}] {msg}")


def data_is_stale():
    if last_success_ms is None:
        return True
    return time.ticks_diff(time.ticks_ms(), last_success_ms) > STALE_AFTER_MS


# =============================================================================
# BUBBLE CLASS
# =============================================================================

class Bubble:
    def __init__(self, metric_name, x=None):
        self.metric_name = metric_name
        self.x = float(x if x is not None else random.randint(0, WIDTH - 1))
        self.y = float(random.randint(0, HEIGHT - 1))
        self.r = random.uniform(2.0, 4.0)  # radius
        self.speed = random.uniform(0.03, 0.1)  # upward speed
        self.wobble = random.uniform(0, 6.28)  # phase offset
        self.wobble_speed = random.uniform(0.03, 0.1)

    def update(self):
        # Float upward
        self.y -= self.speed

        # Gentle horizontal wobble
        self.wobble += self.wobble_speed
        self.x += math.sin(self.wobble) * 0.15

        # Wrap around
        if self.y < -self.r:
            self.y = HEIGHT + self.r
            self.x = float(random.randint(3, WIDTH - 4))

        # Keep in bounds horizontally
        if self.x < 1:
            self.x = 1
        if self.x >= WIDTH - 1:
            self.x = WIDTH - 2

    def get_pens(self):
        """(dim, mid, bright) pens for this bubble's current status."""
        if data_is_stale():
            return PENS_BLUE
        return PENS_BY_STATUS.get(metrics.get(self.metric_name), PENS_BLUE)


# =============================================================================
# BUBBLES — derived from the payload, not a hardcoded list
# =============================================================================

bubbles = []


def sync_bubbles():
    """Reconcile the bubble list with whatever metrics the API returned.

    New components get a bubble; components that disappear from the payload lose
    theirs. A hardcoded list silently kept floating bubbles for things that no
    longer exist (Lens's own database, Lens's Grafana) — permanently blue, and
    indistinguishable from a real loss of signal.
    """
    global bubbles

    wanted = list(metrics.keys())
    if len(wanted) > MAX_BUBBLES:
        dropped = len(wanted) - MAX_BUBBLES
        wanted = wanted[:MAX_BUBBLES]
        log(f"  NOTE: {dropped} metric(s) beyond MAX_BUBBLES not shown")

    have = {b.metric_name: b for b in bubbles}
    kept = [have[n] for n in wanted if n in have]
    added = [n for n in wanted if n not in have]

    for i, name in enumerate(added):
        x = ((len(kept) + i) * max(1, WIDTH // max(1, len(wanted)))) % WIDTH
        kept.append(Bubble(name, x))

    if added or len(kept) != len(bubbles):
        log(f"  Bubbles: {len(kept)} ({len(added)} new)")
    bubbles = kept


# =============================================================================
# NETWORKING
# =============================================================================

wlan = None


def connect_wifi(timeout_s=30):
    """Connect (or reconnect) to WiFi. Safe to call repeatedly."""
    global wlan

    if wlan is None:
        wlan = network.WLAN(network.STA_IF)
    wlan.active(True)

    if wlan.isconnected():
        return True

    log(f"Connecting to {WIFI_SSID}...")
    try:
        wlan.connect(WIFI_SSID, WIFI_PASSWORD)
    except OSError as e:
        log(f"  connect() failed: {e}")
        return False

    for _ in range(timeout_s):
        if wlan.isconnected():
            log(f"Connected! IP: {wlan.ifconfig()[0]}")
            return True
        time.sleep(1)

    log("WiFi failed!")
    return False


def _add(into, name, status):
    into[name] = status if status else 'unknown'


def fetch_metrics():
    """Fetch status from the ciris-status public API.

    Returns True on success. On failure the old values stay put until
    STALE_AFTER_MS passes, after which the display goes blue.
    """
    global last_success_ms

    # The radio drops silently; reconnect before blaming the API.
    if wlan is None or not wlan.isconnected():
        if not connect_wifi(timeout_s=10):
            return False

    log("Fetching metrics...")
    response = None
    try:
        response = urequests.get(STATUS_API_URL, timeout=15)

        if response.status_code != 200:
            log(f"  ERROR: HTTP {response.status_code}")
            return False

        data = response.json()

        # Parse into a fresh dict and swap at the end, so a malformed payload
        # can't leave the display half-updated.
        fresh = {}

        # Overall — note this field carries partial_outage / major_outage too.
        _add(fresh, 'overall', data.get('status'))

        # Regions: one bubble per service per region (billing_us, proxy_eu, ...)
        for region_key, region_data in data.get('regions', {}).items():
            region = region_key.split('.')[0].lower()
            for svc_name, svc in region_data.get('services', {}).items():
                _add(fresh, f"{svc_name}_{region}", svc.get('status'))

        # Flat provider buckets. Keys can be region-qualified (us.postgresql),
        # so keep the whole key — truncating at the dot merged distinct
        # components into one bubble.
        for bucket, prefix in (
            ('infrastructure', 'infra'),
            ('llm_providers', 'llm'),
            ('database_providers', 'db'),
            ('auth_providers', 'auth'),
            ('internal_providers', 'internal'),
        ):
            for name, info in data.get(bucket, {}).items():
                _add(fresh, f"{prefix}_{name.replace('.', '_')}", info.get('status'))

        metrics.clear()
        metrics.update(fresh)
        last_success_ms = time.ticks_ms()
        sync_bubbles()

        unhealthy = [k for k, v in metrics.items() if v != 'operational']
        log(f"  Loaded {len(metrics)} metrics, {len(unhealthy)} not operational")
        for k in unhealthy:
            log(f"    {k}: {metrics[k]}")
        return True

    except Exception as e:
        log(f"  ERROR: {type(e).__name__}: {e}")
        return False
    finally:
        if response is not None:
            try:
                response.close()
            except Exception:
                pass


# =============================================================================
# RENDERING
# =============================================================================

@micropython.native
def draw_bubbles():
    """Draw all bubbles with glow effect"""
    # Clear to black
    graphics.set_pen(PEN_BLACK)
    graphics.clear()

    # Draw each bubble
    for bubble in bubbles:
        bubble.update()
        pen_dim, pen_mid, pen_bright = bubble.get_pens()

        cx, cy = int(bubble.x), int(bubble.y)
        radius = int(bubble.r)

        # Draw concentric rings for glow effect (fast version)
        for dy in range(-radius-1, radius+2):
            for dx in range(-radius-1, radius+2):
                px, py = cx + dx, cy + dy
                if 0 <= px < WIDTH and 0 <= py < HEIGHT:
                    dist_sq = dx*dx + dy*dy
                    r_sq = (radius+1) * (radius+1)
                    if dist_sq <= r_sq:
                        if dist_sq <= radius * radius * 0.3:
                            graphics.set_pen(pen_bright)
                        elif dist_sq <= radius * radius * 0.7:
                            graphics.set_pen(pen_mid)
                        else:
                            graphics.set_pen(pen_dim)
                        graphics.pixel(px, py)

    gu.update(graphics)


def show_connecting():
    """Yellow dots while connecting"""
    graphics.set_pen(PEN_BLACK)
    graphics.clear()
    graphics.set_pen(PEN_YELLOW_BRIGHT)

    for i in range(5):
        graphics.pixel(10 + i * 8, 5)

    gu.update(graphics)


def show_error():
    """Red X on unrecoverable error"""
    graphics.set_pen(PEN_BLACK)
    graphics.clear()
    graphics.set_pen(PEN_RED_BRIGHT)

    for i in range(min(WIDTH, HEIGHT)):
        graphics.pixel(i, i)
        graphics.pixel(WIDTH - 1 - i, i)

    gu.update(graphics)


# =============================================================================
# MAIN
# =============================================================================

def main():
    log("=" * 40)
    log("CIRIS Status Bubbles")
    log(f"Display: {WIDTH}x{HEIGHT}")
    log(f"API: {STATUS_API_URL}")
    log("=" * 40)

    gu.set_brightness(BRIGHTNESS)
    show_connecting()

    if not connect_wifi():
        show_error()
        return

    if not fetch_metrics():
        log("Initial fetch failed — showing unknown until one succeeds")

    last_fetch = time.ticks_ms()

    log("Starting bubble animation...")

    while True:
        # Button controls
        if gu.is_pressed(GalacticUnicorn.SWITCH_BRIGHTNESS_UP):
            gu.adjust_brightness(+0.05)
        if gu.is_pressed(GalacticUnicorn.SWITCH_BRIGHTNESS_DOWN):
            gu.adjust_brightness(-0.05)
        if gu.is_pressed(GalacticUnicorn.SWITCH_A):
            fetch_metrics()
            last_fetch = time.ticks_ms()

        # Periodic refresh (retries on the same cadence; the WiFi reconnect and
        # the stale-out both live in fetch_metrics/get_pens)
        if time.ticks_diff(time.ticks_ms(), last_fetch) > REFRESH_INTERVAL_MS:
            fetch_metrics()
            last_fetch = time.ticks_ms()

        # Animate
        draw_bubbles()

        time.sleep_ms(50)  # ~20 FPS


if __name__ == "__main__":
    main()
