"""
CIRIS status board - Pimoroni Galactic Unicorn, mounted PORTRAIT

The panel is 53x11 in hardware. Mounted on its end (USB/power at the bottom)
you get an 11-wide, 53-tall board, and everything here is written in those
viewer coordinates — `vpixel`/`vrect` rotate into panel space on the way out.
Getting this backwards is what made the first version unreadable: five 1px-tall
rows and a letter drawn sideways.

  vy  0-34   CENTIPEDES - one band per repo, five of them, no cycling:
             a 3x5 letter naming the repo (V/P/E/S/A = verify, persist, edge,
             server, agent) with all 10 CI runs as a full-width bar directly
             beneath it — oldest at the left, newest at the right.
  vy  35     divider, lit in the colour of the OVERALL status
  vy  37-50  HEALTH GRID - static, one blank row between categories so they
             cannot fuse. Three column blocks ordered west to east: US, EU,
             then GLOBAL for what belongs to no region. Rows are, top to
             bottom: billing, proxy, databases, providers, infrastructure.

Run colours    green success | red failure | pulsing amber in-progress |
               dim blue queued | grey cancelled
Health colours green operational | amber degraded | red outage | dim blue unknown

Only in-progress CI cells animate. The health grid never moves.

Flash: copy to the Pico W as main.py alongside a secrets.py with WIFI_SSID /
WIFI_PASSWORD. Buttons: A = refresh now, C = flip orientation 180 (persisted),
LUX +/- = brightness.
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
# DISPLAY + ORIENTATION
# =============================================================================

gu = GalacticUnicorn()
graphics = PicoGraphics(DISPLAY_GALACTIC_UNICORN)

PANEL_W = GalacticUnicorn.WIDTH    # 53 — the long axis, horizontal in hardware
PANEL_H = GalacticUnicorn.HEIGHT   # 11
VW, VH = PANEL_H, PANEL_W          # 11 x 53 as the board is actually mounted

ORIENT_FILE = 'orientation.txt'


def load_orientation():
    """`True` rotates clockwise, `False` counter-clockwise.

    The default is CCW: the Pico's USB sits at the panel's left end in native
    landscape, so standing the board up with the connector at the BOTTOM is a
    counter-clockwise rotation. (Assuming otherwise is what put the first
    portrait build upside down.) Persisted, so a flip survives a power cycle and
    a board mounted the other way up is fixed with button C, not a reflash.
    """
    try:
        with open(ORIENT_FILE) as f:
            return f.read().strip() == 'cw'
    except OSError:
        return False


def save_orientation(cw):
    try:
        with open(ORIENT_FILE, 'w') as f:
            f.write('cw' if cw else 'ccw')
    except OSError:
        pass


rotate_cw = load_orientation()


def vrect(x, y, w, h, pen):
    """Draw a rect in VIEWER coordinates. Rotation maps rectangles to
    rectangles, so this stays one native call rather than a pixel loop."""
    if w <= 0 or h <= 0:
        return
    graphics.set_pen(pen)
    if rotate_cw:
        # viewer down = panel +x, viewer right = panel -y
        graphics.rectangle(y, PANEL_H - x - w, h, w)
    else:
        graphics.rectangle(PANEL_W - y - h, x, h, w)


def vpixel(x, y, pen):
    vrect(x, y, 1, 1, pen)


# ── Layout, in viewer coordinates ────────────────────────────────────────────
LETTER_W = 3
LETTER_H = 5
RUNS_H = 2                      # run bar height
BAND_H = LETTER_H + RUNS_H      # 7 rows per repo: the bar sits under its letter
CI_BANDS = 5
RUNS = 10

DIVIDER_Y = CI_BANDS * BAND_H   # 35
HEALTH_Y0 = DIVIDER_Y + 2       # 37
HEALTH_ROW_H = 2
# One blank row between health rows. Without it, adjacent rows of the same
# colour fuse into a single tall block and the five categories read as a random
# stack of boxes — you cannot see where billing ends and proxy begins.
HEALTH_PITCH = HEALTH_ROW_H + 1
HEALTH_ROWS = 5                 # billing, proxy, db, providers, infra

PEN_BLACK = graphics.create_pen(0, 0, 0)

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
    # Run cells build their own pulsing amber per frame; this static one is the
    # fallback for anything needing the colour without animating.
    'in_progress': graphics.create_pen(200, 140, 0),
}
PEN_UNKNOWN = PENS['unknown']
PEN_EMPTY = graphics.create_pen(6, 6, 6)     # a cell that exists but has no data
PEN_LETTER = graphics.create_pen(150, 150, 150)

# A 3x5 uppercase font: five rows of three bits, MSB leftmost. Hand-rolled
# because PicoGraphics' smallest bitmap font is 6px tall. Covers A-Z, not just
# the five initials we ship, so a repo added to `status.ci.repos` still gets a
# correct letter instead of a placeholder.
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
    """Split the width into n column blocks 1px apart, remainder to the
    leftmost, so the row always reaches the right edge."""
    if n <= 0:
        return []
    inner = VW - (n - 1)
    if inner < n:
        return [(i, 1) for i in range(min(n, VW))]
    w, extra = inner // n, inner % n
    out, x = [], 0
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
    for entry in (data.get('repos') or [])[:CI_BANDS]:
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

def repo_letter(name):
    """`CIRISVerify` -> `V`. The CIRIS prefix is on every repo, so it carries no
    information; the letter after it is what tells them apart."""
    n = name.upper()
    if n.startswith('CIRIS'):
        n = n[5:]
    return n[0] if n else '?'


def draw_letter(x, y, ch, pen):
    rows = FONT_3X5.get(ch, FONT_3X5['?'])
    for dy, bits in enumerate(rows):
        for dx in range(LETTER_W):
            if bits & (1 << (LETTER_W - 1 - dx)):
                vpixel(x + dx, y + dy, pen)


def pulse_pen(phase):
    """Amber, breathing — what makes in-progress unmistakable next to queued."""
    v = 0.45 + 0.55 * (0.5 + 0.5 * math.sin(phase))
    return graphics.create_pen(int(210 * v), int(140 * v), 0)


def draw_centipedes(phase):
    """One band per repo: letter + state pip, then the 10-run bar beneath."""
    blue = ci_stale()
    pulse = pulse_pen(phase)
    cell_w = VW // RUNS          # 1px per run at 11 wide

    for band in range(CI_BANDS):
        top = band * BAND_H
        if band < len(centipedes):
            name, runs = centipedes[band]
        else:
            name, runs = '?', []

        draw_letter(0, top, repo_letter(name),
                    PEN_UNKNOWN if blue else PEN_LETTER)

        runs_y = top + LETTER_H
        for i in range(RUNS):
            if i < len(runs):
                pen = PEN_UNKNOWN if blue else (
                    pulse if runs[i] == 'in_progress'
                    else PENS.get(runs[i], PEN_UNKNOWN)
                )
            else:
                pen = PEN_EMPTY          # a young repo draws a short centipede
            vrect(i * cell_w, runs_y, cell_w, RUNS_H, pen)


def draw_divider():
    """The separator carries the overall status — one glance, whole system."""
    pen = PEN_UNKNOWN if stale() else PENS.get(overall, PEN_UNKNOWN)
    for x in range(0, VW, 2):
        vpixel(x, DIVIDER_Y, pen)


def draw_health():
    blue = stale()
    spans = block_spans(len(blocks))
    for row in range(HEALTH_ROWS):
        y = HEALTH_Y0 + row * HEALTH_PITCH
        for b, (bx, bw) in enumerate(spans):
            cells = grid.get((row, b), [])
            if not cells:
                continue
            n = len(cells)
            w = max(1, bw // n)
            extra = bw - w * n
            x = bx
            for i, status in enumerate(cells):
                cw = w + (1 if i < extra else 0)
                if x + cw > bx + bw:
                    cw = bx + bw - x
                if cw <= 0:
                    break
                vrect(x, y, cw, HEALTH_ROW_H,
                      PEN_UNKNOWN if blue else PENS.get(status, PEN_UNKNOWN))
                x += cw


def draw(phase):
    graphics.set_pen(PEN_BLACK)
    graphics.clear()
    draw_centipedes(phase)
    draw_divider()
    draw_health()
    gu.update(graphics)


# An arrow pointing UP in viewer coordinates: if it points any other way, the
# orientation constant is wrong — press C to flip it.
ARROW = (0b00100, 0b01110, 0b11111, 0b00100, 0b00100, 0b00100)


def splash(pen):
    graphics.set_pen(PEN_BLACK)
    graphics.clear()
    for dy, bits in enumerate(ARROW):
        for dx in range(5):
            if bits & (1 << (4 - dx)):
                vpixel(3 + dx, 4 + dy, pen)
    gu.update(graphics)


# =============================================================================
# MAIN
# =============================================================================

def main():
    global rotate_cw

    log("=" * 46)
    log("CIRIS status board  %dx%d viewer (%dx%d panel, rot=%s)"
        % (VW, VH, PANEL_W, PANEL_H, 'cw' if rotate_cw else 'ccw'))
    log("  bands: V=verify P=persist E=edge S=server A=agent")
    log("  health rows: billing, proxy, database, providers, infra")
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
        # C flips the board 180 and remembers it — so a board mounted the other
        # way up is fixed by pressing a button, not by reflashing.
        if gu.is_pressed(GalacticUnicorn.SWITCH_C):
            rotate_cw = not rotate_cw
            save_orientation(rotate_cw)
            log("orientation: %s" % ('cw' if rotate_cw else 'ccw'))
            time.sleep_ms(300)

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
