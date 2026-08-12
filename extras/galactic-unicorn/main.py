"""
CIRIS status board - Pimoroni Galactic Unicorn (53x11)

Two sections, fixed positions, nothing floats:

  rows 0-4   CENTIPEDES - the substrate's last 10 CI runs, one row per repo,
             oldest run at the left, newest at the leading edge. A 2px tag on
             the far left identifies the repo by hue.
  row  5     divider, lit in the colour of the OVERALL status
  rows 6-10  HEALTH GRID - static. Regions are column blocks ordered west to
             east (US left of EU, like a map), then a GLOBAL block for things
             that belong to no region. A tick gutter on the far left says which
             row is which: 1 tick = billing, 2 = proxy, 3 = database,
             4 = providers, 5 = infrastructure.

Run colours   green success | red failure | pulsing amber in-progress |
              dim blue queued | grey cancelled
Health colours green operational | amber degraded | red outage | dim blue unknown

Only in-progress CI cells animate. The health grid never moves.

Flash: copy to the Pico W as main.py alongside a secrets.py with WIFI_SSID /
WIFI_PASSWORD. Buttons: A = refresh now, LUX +/- = brightness.
"""

import network
import urequests
import time
import math
from galactic import GalacticUnicorn
from picographics import PicoGraphics, DISPLAY_GALACTIC_UNICORN

# =============================================================================
# CONFIGURATION
# =============================================================================

WIFI_SSID = "YOUR_WIFI_SSID"
WIFI_PASSWORD = "YOUR_WIFI_PASSWORD"

API_ROOT = "https://lens.ciris-services-1.ai/status"
STATUS_URL = API_ROOT + "/api/v1/status"
CI_URL = API_ROOT + "/api/v1/ci"

try:
    from secrets import WIFI_SSID, WIFI_PASSWORD
except ImportError:
    pass

STATUS_REFRESH_MS = 30000
# The server polls GitHub on its own (slower) cadence and serves a cached
# snapshot, so asking more often than this buys nothing.
CI_REFRESH_MS = 60000
# Past this long with no successful fetch, stop trusting what is on the wall.
STALE_AFTER_MS = 3 * STATUS_REFRESH_MS
BRIGHTNESS = 0.5

# =============================================================================
# DISPLAY
# =============================================================================

gu = GalacticUnicorn()
graphics = PicoGraphics(DISPLAY_GALACTIC_UNICORN)

WIDTH = GalacticUnicorn.WIDTH    # 53
HEIGHT = GalacticUnicorn.HEIGHT  # 11

# ── Layout ───────────────────────────────────────────────────────────────────
CI_ROW0 = 0
CI_ROWS = 5           # one per repo
DIVIDER_ROW = 5
HEALTH_ROW0 = 6
HEALTH_ROWS = 5

TAG_W = 2             # repo hue tag on each centipede
CI_X0 = TAG_W + 2     # 2px gap after the tag
RUNS = 10
CELL_W = 4            # 10 cells of 4px + 9 gaps = 49px: x=4..52, flush right
CELL_GAP = 1

GUTTER_W = 5          # up to 5 ticks
GRID_X0 = GUTTER_W + 1

PEN_BLACK = graphics.create_pen(0, 0, 0)

# Health / run status colours.
PENS = {
    'operational': graphics.create_pen(0, 170, 0),
    'degraded': graphics.create_pen(190, 130, 0),
    'outage': graphics.create_pen(200, 0, 0),
    'partial_outage': graphics.create_pen(200, 0, 0),
    'major_outage': graphics.create_pen(220, 0, 0),
    'unknown': graphics.create_pen(0, 22, 55),

    'success': graphics.create_pen(0, 150, 0),
    'failure': graphics.create_pen(200, 0, 0),
    'queued': graphics.create_pen(0, 40, 120),
    'cancelled': graphics.create_pen(45, 45, 45),
    # in_progress is drawn with a pulse, built per frame
}
PEN_UNKNOWN = PENS['unknown']
PEN_TICK = graphics.create_pen(40, 40, 40)
PEN_EMPTY = graphics.create_pen(6, 6, 6)   # a cell that exists but has no data

# Repo tag hues — deliberately not status colours, so the left edge never reads
# as health. Same order as the centipede rows.
REPO_TAGS = [
    graphics.create_pen(0, 70, 70),     # teal
    graphics.create_pen(70, 0, 70),     # magenta
    graphics.create_pen(60, 60, 60),    # white
    graphics.create_pen(80, 40, 0),     # amber-brown
    graphics.create_pen(40, 0, 80),     # violet
]

# Region ordering, west to east, so US sits left of EU like a map. Unknown
# regions sort after the known ones, alphabetically — a new region just appears.
REGION_ORDER = {
    'us': 0, 'usw': 1, 'use': 2, 'ca': 3,
    'sa': 5,
    'uk': 10, 'eu': 11, 'euw': 12, 'eue': 13,
    'af': 20, 'me': 21,
    'in': 30, 'apac': 31, 'jp': 32, 'au': 35,
}

# =============================================================================
# STATE
# =============================================================================

overall = 'unknown'
regions = []      # ordered [{'key','name','services':{...}}]
grid = {}         # (row_index, block_index) -> [status, ...]
blocks = []       # ordered block keys: region keys, then 'global'
centipedes = []   # [(repo, [run_state, ...])]

last_status_ok_ms = None
last_ci_ok_ms = None


def log(msg):
    print("[%d] %s" % (time.ticks_ms(), msg))


def _stale(mark, window):
    if mark is None:
        return True
    return time.ticks_diff(time.ticks_ms(), mark) > window


def stale():
    """Health data no longer trustworthy."""
    return _stale(last_status_ok_ms, STALE_AFTER_MS)


def ci_stale():
    """CI data no longer trustworthy — tracked separately, so a broken
    /api/v1/ci greys out the centipedes without touching the health grid (and
    vice versa)."""
    return _stale(last_ci_ok_ms, 3 * CI_REFRESH_MS)


# =============================================================================
# LAYOUT MATH
# =============================================================================

def block_spans(n):
    """Split the grid area into n column blocks, 1px apart, remainder spread
    across the leftmost blocks so the row always reaches the right edge."""
    if n <= 0:
        return []
    span = WIDTH - GRID_X0
    inner = span - (n - 1)
    if inner < n:            # more blocks than pixels; degrade to 1px each
        return [(GRID_X0 + i, 1) for i in range(min(n, span))]
    w, extra = inner // n, inner % n
    out, x = [], GRID_X0
    for i in range(n):
        bw = w + (1 if i < extra else 0)
        out.append((x, bw))
        x += bw + 1
    return out


def region_sort_key(key):
    return (REGION_ORDER.get(key, 50), key)


# =============================================================================
# PARSING
# =============================================================================

def _status_of(d):
    s = d.get('status') if isinstance(d, dict) else None
    return s if s else 'unknown'


def parse_status(data):
    """Fold /api/v1/status into the fixed grid.

    Row 0 billing | 1 proxy | 2 databases | 3 providers (llm + internal) |
    4 infrastructure + auth. A key like `us.postgresql` lands in the US block;
    an unprefixed one is global. Infrastructure is matched back to its region by
    the `name` the aggregator copies from the region label.
    """
    global overall, regions, blocks, grid

    overall = data.get('status') or 'unknown'

    raw = data.get('regions', {}) or {}
    regions = []
    for key in sorted(raw.keys(), key=region_sort_key):
        rd = raw[key] or {}
        regions.append({
            'key': key,
            'name': rd.get('name', key),
            'services': rd.get('services', {}) or {},
        })

    blocks = [r['key'] for r in regions] + ['global']
    idx = {}
    for i, b in enumerate(blocks):
        idx[b] = i
    g = {}

    def put(row, block, status):
        b = idx.get(block, idx['global'])
        g.setdefault((row, b), []).append(status)

    # Rows 0/1 — the regional services.
    for r in regions:
        svcs = r['services']
        for row, name in ((0, 'billing'), (1, 'proxy')):
            if name in svcs:
                put(row, r['key'], _status_of(svcs[name]))

    def spread(row, bucket):
        """Region-prefixed keys go to their block; the rest are global."""
        for name in sorted((data.get(bucket) or {}).keys()):
            info = data[bucket][name]
            block = name.split('.')[0] if '.' in name else 'global'
            put(row, block, _status_of(info))

    spread(2, 'database_providers')
    spread(3, 'internal_providers')
    for name in sorted((data.get('llm_providers') or {}).keys()):
        put(3, 'global', _status_of(data['llm_providers'][name]))

    # Row 4 — infrastructure under the region it hosts, plus global auth.
    by_name = {}
    for r in regions:
        by_name[r['name']] = r['key']
    for name in sorted((data.get('infrastructure') or {}).keys()):
        info = data['infrastructure'][name] or {}
        put(4, by_name.get(info.get('name'), 'global'), _status_of(info))
    for name in sorted((data.get('auth_providers') or {}).keys()):
        put(4, 'global', _status_of(data['auth_providers'][name]))

    grid = g


def parse_ci(data):
    global centipedes
    out = []
    for entry in (data.get('repos') or [])[:CI_ROWS]:
        out.append((entry.get('repo', '?'), entry.get('runs') or []))
    centipedes = out


# =============================================================================
# NETWORK
# =============================================================================

wlan = None


def connect_wifi(timeout_s=30):
    """Connect or reconnect. Safe to call repeatedly."""
    global wlan
    if wlan is None:
        wlan = network.WLAN(network.STA_IF)
    wlan.active(True)
    if wlan.isconnected():
        return True

    log("connecting to %s..." % WIFI_SSID)
    try:
        wlan.connect(WIFI_SSID, WIFI_PASSWORD)
    except OSError as e:
        log("  connect() failed: %s" % e)
        return False
    for _ in range(timeout_s):
        if wlan.isconnected():
            log("  connected, ip=%s" % wlan.ifconfig()[0])
            return True
        time.sleep(1)
    log("  wifi failed")
    return False


def fetch_json(url):
    if wlan is None or not wlan.isconnected():
        if not connect_wifi(timeout_s=10):
            return None
    r = None
    try:
        r = urequests.get(url, timeout=15)
        if r.status_code != 200:
            log("  HTTP %d from %s" % (r.status_code, url))
            return None
        return r.json()
    except Exception as e:
        log("  %s: %s" % (type(e).__name__, e))
        return None
    finally:
        if r is not None:
            try:
                r.close()
            except Exception:
                pass


def refresh_status():
    global last_status_ok_ms
    data = fetch_json(STATUS_URL)
    if data is None:
        return False
    parse_status(data)
    last_status_ok_ms = time.ticks_ms()
    bad = []
    for (row, b), cells in grid.items():
        for s in cells:
            if s != 'operational':
                bad.append("r%d/%s=%s" % (row, blocks[b], s))
    log("status: overall=%s regions=%d cells=%d notok=%s"
        % (overall, len(regions), sum(len(v) for v in grid.values()),
           ",".join(sorted(bad)) if bad else "none"))
    return True


def refresh_ci():
    global last_ci_ok_ms
    data = fetch_json(CI_URL)
    if data is None:
        return False
    parse_ci(data)
    last_ci_ok_ms = time.ticks_ms()
    log("ci: " + " ".join("%s=%s" % (r, "".join(s[0] for s in runs))
                          for r, runs in centipedes))
    return True


# =============================================================================
# RENDER
# =============================================================================

def bar(x, y, w, pen):
    if w <= 0:
        return
    graphics.set_pen(pen)
    graphics.rectangle(x, y, w, 1)


def pulse_pen(phase):
    """Amber, breathing — what makes in-progress unmistakable next to queued."""
    v = 0.45 + 0.55 * (0.5 + 0.5 * math.sin(phase))
    return graphics.create_pen(int(210 * v), int(140 * v), 0)


def draw_centipedes(phase):
    blue = ci_stale()
    pulse = pulse_pen(phase)
    for row in range(CI_ROWS):
        y = CI_ROW0 + row
        if row < len(centipedes):
            bar(0, y, TAG_W, REPO_TAGS[row % len(REPO_TAGS)])
            runs = centipedes[row][1]
        else:
            runs = []
        for i in range(RUNS):
            x = CI_X0 + i * (CELL_W + CELL_GAP)
            if i < len(runs) and not blue:
                state = runs[i]
                pen = pulse if state == 'in_progress' else PENS.get(state, PEN_UNKNOWN)
            else:
                pen = PEN_UNKNOWN if (blue and i < len(runs)) else PEN_EMPTY
            bar(x, y, CELL_W, pen)


def draw_divider():
    """The separator carries the overall status — one glance, whole system."""
    pen = PEN_UNKNOWN if stale() else PENS.get(overall, PEN_UNKNOWN)
    graphics.set_pen(pen)
    for x in range(0, WIDTH, 2):
        graphics.pixel(x, DIVIDER_ROW)


def draw_health():
    blue = stale()
    spans = block_spans(len(blocks))
    for row in range(HEALTH_ROWS):
        y = HEALTH_ROW0 + row

        # Tick gutter: this row's index, countable at a glance.
        graphics.set_pen(PEN_TICK)
        for t in range(row + 1):
            graphics.pixel(t, y)

        for b, (bx, bw) in enumerate(spans):
            cells = grid.get((row, b), [])
            if not cells:
                continue
            n = len(cells)
            # Sub-cells share the block, 1px apart when there is room.
            gap = 1 if n > 1 and bw >= 2 * n else 0
            inner = bw - gap * (n - 1)
            w = inner // n
            if w < 1:
                w, gap = 1, 0
            extra = inner - w * n if w >= 1 else 0
            x = bx
            for i, status in enumerate(cells):
                cw = w + (1 if i < extra else 0)
                pen = PEN_UNKNOWN if blue else PENS.get(status, PEN_UNKNOWN)
                bar(x, y, cw, pen)
                x += cw + gap


def draw(phase):
    graphics.set_pen(PEN_BLACK)
    graphics.clear()
    draw_centipedes(phase)
    draw_divider()
    draw_health()
    gu.update(graphics)


def splash(pen):
    graphics.set_pen(PEN_BLACK)
    graphics.clear()
    graphics.set_pen(pen)
    for x in range(10, WIDTH - 10):
        graphics.pixel(x, HEIGHT // 2)
    gu.update(graphics)


# =============================================================================
# MAIN
# =============================================================================

def main():
    log("=" * 46)
    log("CIRIS status board  %dx%d" % (WIDTH, HEIGHT))
    log("  rows 0-4 CI centipedes   row 5 overall   rows 6-10 health")
    log("  ticks: 1=billing 2=proxy 3=database 4=providers 5=infra")
    log("  %s" % STATUS_URL)
    log("  %s" % CI_URL)
    log("=" * 46)

    gu.set_brightness(BRIGHTNESS)
    splash(PENS['degraded'])

    if not connect_wifi():
        splash(PENS['outage'])
        return

    refresh_status()
    refresh_ci()
    last_status = last_ci = time.ticks_ms()

    phase = 0.0
    while True:
        if gu.is_pressed(GalacticUnicorn.SWITCH_BRIGHTNESS_UP):
            gu.adjust_brightness(+0.05)
        if gu.is_pressed(GalacticUnicorn.SWITCH_BRIGHTNESS_DOWN):
            gu.adjust_brightness(-0.05)
        if gu.is_pressed(GalacticUnicorn.SWITCH_A):
            refresh_status()
            refresh_ci()
            last_status = last_ci = time.ticks_ms()

        now = time.ticks_ms()
        if time.ticks_diff(now, last_status) > STATUS_REFRESH_MS:
            refresh_status()
            last_status = now
        if time.ticks_diff(now, last_ci) > CI_REFRESH_MS:
            refresh_ci()
            last_ci = now

        phase += 0.16          # ~1.2s breath at 20 FPS
        draw(phase)
        time.sleep_ms(50)


if __name__ == "__main__":
    main()
