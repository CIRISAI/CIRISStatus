"""
CIRIS status board - Pimoroni Galactic Unicorn (53x11)

Two sections, fixed positions, nothing floats:

  rows 0-4   CENTIPEDE - ONE repo's last 10 CI runs as chunky 4x5 blocks,
             oldest at the left, newest at the leading edge. A 3x5 letter names
             the repo (V/P/E/S/A = verify, persist, edge, server, agent) with a
             1px separator. Cycles through the repos every few seconds.

             Five rows of 1px each would fit all five repos at once, but a
             letter needs 5 rows of height — so the board shows one repo large
             and legible instead of five rows you cannot tell apart.

  row  5     divider: five pips on the left, one per repo, coloured by that
             repo's worst run in the window (the current repo's pip is bright,
             so you can see all five at a glance and which one is on screen);
             then a dotted line in the colour of the OVERALL status.
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
WIFI_PASSWORD. Buttons: A = refresh now, B = next repo, LUX +/- = brightness.
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
CI_ROWS = 5           # band height — also the height of a letter
DIVIDER_ROW = 5
HEALTH_ROW0 = 6
HEALTH_ROWS = 5

LETTER_W = 3          # 3x5 glyph, the smallest that stays legible
SEP_X = LETTER_W      # 1px separator column right after the letter
CI_X0 = LETTER_W + 1  # exactly "letter + separator", nothing wasted
RUNS = 10
CELL_W = 4            # 10 cells of 4px + 9 gaps = 49px: x=4..52, flush right
CELL_GAP = 1

MAX_REPOS = 5         # as many pips as fit in the letter+separator width
CYCLE_MS = 4000       # dwell per repo

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
    # Run cells build their own pulsing amber per frame; this static one is for
    # anything that needs an in-progress colour without animating — the repo
    # pips. Without it `PENS.get('in_progress')` fell through to unknown, and a
    # building repo's pip rendered blue as if we had no data on it.
    'in_progress': graphics.create_pen(200, 140, 0),
}
PEN_UNKNOWN = PENS['unknown']
PEN_TICK = graphics.create_pen(40, 40, 40)
PEN_EMPTY = graphics.create_pen(6, 6, 6)   # a cell that exists but has no data

PEN_LETTER = graphics.create_pen(120, 120, 120)
PEN_SEP = graphics.create_pen(25, 25, 25)

# Dim twins of the run colours, for the pips of repos that are not on screen.
PIPS_DIM = {
    'success': graphics.create_pen(0, 45, 0),
    'failure': graphics.create_pen(60, 0, 0),
    'in_progress': graphics.create_pen(55, 38, 0),
    'queued': graphics.create_pen(0, 12, 35),
    'cancelled': graphics.create_pen(18, 18, 18),
}

# A 3x5 uppercase font: one entry per letter, five rows of three bits, MSB left.
# Hand-rolled because PicoGraphics' smallest bitmap font is 6px tall and the CI
# band is 5 — and because the whole alphabet is here, so a repo renamed in
# `status.ci.repos` still gets a correct initial instead of a placeholder.
FONT_3X5 = {
    'A': (0b010, 0b101, 0b111, 0b101, 0b101),
    'B': (0b110, 0b101, 0b110, 0b101, 0b110),
    'C': (0b011, 0b100, 0b100, 0b100, 0b011),
    'D': (0b110, 0b101, 0b101, 0b101, 0b110),
    'E': (0b111, 0b100, 0b110, 0b100, 0b111),
    'F': (0b111, 0b100, 0b110, 0b100, 0b100),
    'G': (0b011, 0b100, 0b101, 0b101, 0b011),
    'H': (0b101, 0b101, 0b111, 0b101, 0b101),
    'I': (0b111, 0b010, 0b010, 0b010, 0b111),
    'J': (0b001, 0b001, 0b001, 0b101, 0b010),
    'K': (0b101, 0b101, 0b110, 0b101, 0b101),
    'L': (0b100, 0b100, 0b100, 0b100, 0b111),
    'M': (0b101, 0b111, 0b111, 0b101, 0b101),
    'N': (0b101, 0b111, 0b111, 0b111, 0b101),
    'O': (0b010, 0b101, 0b101, 0b101, 0b010),
    'P': (0b110, 0b101, 0b110, 0b100, 0b100),
    'Q': (0b010, 0b101, 0b101, 0b111, 0b011),
    'R': (0b110, 0b101, 0b110, 0b101, 0b101),
    'S': (0b011, 0b100, 0b010, 0b001, 0b110),
    'T': (0b111, 0b010, 0b010, 0b010, 0b010),
    'U': (0b101, 0b101, 0b101, 0b101, 0b011),
    'V': (0b101, 0b101, 0b101, 0b101, 0b010),
    'W': (0b101, 0b101, 0b111, 0b111, 0b101),
    'X': (0b101, 0b101, 0b010, 0b101, 0b101),
    'Y': (0b101, 0b101, 0b010, 0b010, 0b010),
    'Z': (0b111, 0b001, 0b010, 0b100, 0b111),
    '?': (0b110, 0b001, 0b010, 0b000, 0b010),
}

# Worst-first ranking for a repo's pip: one failure in the window outranks
# anything in flight, and `cancelled` never counts as bad.
RUN_RANK = {'failure': 3, 'in_progress': 2, 'queued': 1, 'success': 0, 'cancelled': 0}

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
ci_index = 0      # which repo the band is showing right now

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
    for entry in (data.get('repos') or [])[:MAX_REPOS]:
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


def repo_letter(name):
    """`CIRISVerify` -> `V`. The CIRIS prefix is on every repo, so it carries no
    information; the letter after it is what tells them apart."""
    n = name.upper()
    if n.startswith('CIRIS'):
        n = n[5:]
    return n[0] if n else '?'


def draw_letter(x, y, ch, pen):
    graphics.set_pen(pen)
    rows = FONT_3X5.get(ch, FONT_3X5['?'])
    for dy, bits in enumerate(rows):
        for dx in range(LETTER_W):
            if bits & (1 << (LETTER_W - 1 - dx)):
                graphics.pixel(x + dx, y + dy)


def repo_state(runs):
    """The one state that describes a repo's window — worst wins."""
    worst, rank = None, -1
    for s in runs:
        r = RUN_RANK.get(s, 0)
        if r > rank:
            worst, rank = s, r
    return worst


def draw_ci_band(phase):
    """One repo, drawn large: letter, separator, ten 4x5 run blocks."""
    blue = ci_stale()
    pulse = pulse_pen(phase)

    runs = []
    letter = '?'
    if centipedes:
        name, runs = centipedes[ci_index % len(centipedes)]
        letter = repo_letter(name)

    draw_letter(0, CI_ROW0, letter, PEN_UNKNOWN if blue else PEN_LETTER)
    graphics.set_pen(PEN_SEP)
    for y in range(CI_ROW0, CI_ROW0 + CI_ROWS):
        graphics.pixel(SEP_X, y)

    for i in range(RUNS):
        x = CI_X0 + i * (CELL_W + CELL_GAP)
        if i < len(runs):
            pen = PEN_UNKNOWN if blue else (
                pulse if runs[i] == 'in_progress' else PENS.get(runs[i], PEN_UNKNOWN)
            )
        else:
            pen = PEN_EMPTY          # a young repo draws a short centipede
        graphics.set_pen(pen)
        graphics.rectangle(x, CI_ROW0, CELL_W, CI_ROWS)


def draw_divider():
    """Pips for every repo on the left, then the overall status as a dotted
    line — so the cycling band never costs you sight of the other repos."""
    blue = ci_stale()
    for i in range(min(len(centipedes), MAX_REPOS)):
        state = repo_state(centipedes[i][1])
        if blue or state is None:
            pen = PEN_UNKNOWN if blue else PEN_EMPTY
        elif i == ci_index % max(1, len(centipedes)):
            pen = PENS.get(state, PEN_UNKNOWN)          # current repo: bright
        else:
            pen = PIPS_DIM.get(state, PEN_EMPTY)
        graphics.set_pen(pen)
        graphics.pixel(i, DIVIDER_ROW)

    pen = PEN_UNKNOWN if stale() else PENS.get(overall, PEN_UNKNOWN)
    graphics.set_pen(pen)
    for x in range(GRID_X0, WIDTH, 2):
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
    draw_ci_band(phase)
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

    global ci_index

    refresh_status()
    refresh_ci()
    last_status = last_ci = last_cycle = time.ticks_ms()

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
        # B steps the band on demand, so you never have to wait out the cycle.
        if gu.is_pressed(GalacticUnicorn.SWITCH_B) and centipedes:
            ci_index = (ci_index + 1) % len(centipedes)
            last_cycle = time.ticks_ms()
            time.sleep_ms(150)      # crude debounce; the loop is 20 FPS

        now = time.ticks_ms()
        if time.ticks_diff(now, last_status) > STATUS_REFRESH_MS:
            refresh_status()
            last_status = now
        if time.ticks_diff(now, last_ci) > CI_REFRESH_MS:
            refresh_ci()
            last_ci = now
        if centipedes and time.ticks_diff(now, last_cycle) > CYCLE_MS:
            ci_index = (ci_index + 1) % len(centipedes)
            last_cycle = now

        phase += 0.16          # ~1.2s breath at 20 FPS
        draw(phase)
        time.sleep_ms(50)


if __name__ == "__main__":
    main()
