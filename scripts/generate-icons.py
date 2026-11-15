#!/usr/bin/env python3
"""Write the Starcom app icon SVG, PNG, and ICO from one set of geometry.

The mark is the mirrored prompt ``> ■ <``. Linux keeps the Breeze tile:
square corners, lighter at the top, and a long shadow at 45°. Windows and
macOS rebuild that mark the way FileMan rebuilds its panes. The chevrons and
the cursor share one blue and one light from above. Windows is a Fluent mica
tile. macOS is Liquid Glass inside Apple's squircle (a quintic superellipse).
"""
from __future__ import annotations

import math
import struct
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
VIEW = 256

# Breeze lighting: lighter at the top, darker at the bottom.
BG_TOP = (0x3A, 0x4C, 0x64)
BG_BOT = (0x16, 0x1C, 0x2A)
CYAN_HI = (0xB4, 0xE8, 0xF0)
CYAN_LO = (0x2C, 0x86, 0x94)
SHADOW = (0x10, 0x14, 0x1C)
# Long shadow at 45° toward the bottom-right, in the same units as the view.
SHADOW_X = 34.0
SHADOW_Y = 34.0
SHADOW_ALPHA = 0.55
# Thin lip, not a wide bevel. A 48px icon keeps this under one pixel.
RIM = 2.0
RIM_LIGHT = 0.32
RIM_DARK = 0.35
PLATE = (8.0, 8.0, 248.0, 248.0)
PLATE_RADIUS = 0.0

# Mirrored prompt `> ■ <`: one commander between two directions.
CHEVRON_SPAN = 32.0
CHEVRON_RADIUS = 13.0
CHEVRON_LEFT = 64.0
CHEVRON_RIGHT = 192.0
CURSOR = (112.0, 112.0, 144.0, 144.0)
CURSOR_RADIUS = 0.0
MID_Y = 128.0


def fmt(value: float) -> str:
    return f"{value:g}"


def hex_rgb(color: tuple[int, int, int]) -> str:
    return f"#{color[0]:02x}{color[1]:02x}{color[2]:02x}"


def glyph_span() -> tuple[float, float]:
    pad = CHEVRON_SPAN + CHEVRON_RADIUS
    return MID_Y - pad, MID_Y + pad


def chevron_segments() -> list[tuple[float, float, float, float]]:
    span = CHEVRON_SPAN
    return [
        (CHEVRON_LEFT - span * 0.55, MID_Y - span, CHEVRON_LEFT + span * 0.7, MID_Y),
        (CHEVRON_LEFT - span * 0.55, MID_Y + span, CHEVRON_LEFT + span * 0.7, MID_Y),
        (CHEVRON_RIGHT + span * 0.55, MID_Y - span, CHEVRON_RIGHT - span * 0.7, MID_Y),
        (CHEVRON_RIGHT + span * 0.55, MID_Y + span, CHEVRON_RIGHT - span * 0.7, MID_Y),
    ]


def _extend(ax: float, ay: float, bx: float, by: float, extra: float) -> tuple[float, float]:
    dx, dy = ax - bx, ay - by
    length = math.hypot(dx, dy) or 1.0
    return ax + dx / length * extra, ay + dy / length * extra


def chevron_centerlines() -> list[tuple[float, float, float, float, float, float]]:
    # Ends are pushed out by the old round-cap radius so a flat cut keeps the same reach.
    lines = []
    segments = chevron_segments()
    for index in (0, 2):
        ax, ay, bx, by = segments[index]
        cx, cy, tip_x, tip_y = segments[index + 1]
        ax, ay = _extend(ax, ay, bx, by, CHEVRON_RADIUS)
        cx, cy = _extend(cx, cy, tip_x, tip_y, CHEVRON_RADIUS)
        lines.append((ax, ay, bx, by, cx, cy))
    return lines


def glyph_paint(stroke: str, square: str, dx: float, dy: float) -> str:
    shift = "" if dx == 0.0 and dy == 0.0 else f' transform="translate({fmt(dx)} {fmt(dy)})"'
    arms = "\n".join(
        f'      <path d="M{fmt(ax)} {fmt(ay)} L{fmt(bx)} {fmt(by)} L{fmt(cx)} {fmt(cy)}"/>'
        for ax, ay, bx, by, cx, cy in chevron_centerlines()
    )
    x0, y0, x1, y1 = CURSOR
    return f"""\
    <g{shift}>
      <g fill="none" stroke="{stroke}" stroke-width="{fmt(CHEVRON_RADIUS * 2)}" stroke-linecap="butt" stroke-linejoin="miter">
{arms}
      </g>
      <rect x="{fmt(x0)}" y="{fmt(y0)}" width="{fmt(x1 - x0)}" height="{fmt(y1 - y0)}" rx="{fmt(CURSOR_RADIUS)}" fill="{square}"/>
    </g>"""


def svg_text() -> str:
    shade_top, shade_bot = glyph_span()
    x0, y0, x1, y1 = PLATE
    plate = (
        f'<rect x="{fmt(x0)}" y="{fmt(y0)}" width="{fmt(x1 - x0)}" height="{fmt(y1 - y0)}" '
        f'rx="{fmt(PLATE_RADIUS)}" fill="{{fill}}"/>'
    )
    # Eight slices. Each mask is that slice of the extrusion minus the nearer one,
    # so overlapping arms do not darken twice.
    masks = []
    layers = []
    steps = 8
    for index in range(1, steps + 1):
        far = index / steps
        near = (index - 1) / steps
        fade = SHADOW_ALPHA * (1.0 - (far + near) * 0.5)
        masks.append(
            f"""\
    <mask id="cast{index}" maskContentUnits="userSpaceOnUse">
      <rect width="256" height="256" fill="#000"/>
{glyph_paint("#fff", "#fff", SHADOW_X * far, SHADOW_Y * far)}
{glyph_paint("#000", "#000", SHADOW_X * near, SHADOW_Y * near)}
    </mask>"""
        )
        layers.append(
            f'    <rect width="256" height="256" fill="{hex_rgb(SHADOW)}" opacity="{fmt(fade)}" mask="url(#cast{index})"/>'
        )
    cast = "\n".join(layers)
    mask_defs = "\n".join(masks)
    return f"""\
<svg xmlns="http://www.w3.org/2000/svg" width="256" height="256" viewBox="0 0 256 256">
  <defs>
    <linearGradient id="bg" x1="0" y1="8" x2="0" y2="248" gradientUnits="userSpaceOnUse">
      <stop offset="0" stop-color="{hex_rgb(BG_TOP)}"/>
      <stop offset="1" stop-color="{hex_rgb(BG_BOT)}"/>
    </linearGradient>
    <linearGradient id="mark" gradientUnits="userSpaceOnUse" x1="0" y1="{fmt(shade_top)}" x2="0" y2="{fmt(shade_bot)}">
      <stop offset="0" stop-color="{hex_rgb(CYAN_HI)}"/>
      <stop offset="1" stop-color="{hex_rgb(CYAN_LO)}"/>
    </linearGradient>
    <clipPath id="plate">
      {plate.format(fill="#fff")}
    </clipPath>
{mask_defs}
  </defs>
  {plate.format(fill="url(#bg)")}
  <g clip-path="url(#plate)">
{cast}
    <rect x="8" y="8" width="240" height="{fmt(RIM)}" fill="#fff" opacity="{fmt(RIM_LIGHT)}"/>
    <rect x="8" y="{fmt(248 - RIM)}" width="240" height="{fmt(RIM)}" fill="#000" opacity="{fmt(RIM_DARK)}"/>
{glyph_paint("url(#mark)", "url(#mark)", 0.0, 0.0)}
  </g>
</svg>
"""


def sd_round_box(px: float, py: float, x0: float, y0: float, x1: float, y1: float, radius: float) -> float:
    half_w = (x1 - x0) * 0.5
    half_h = (y1 - y0) * 0.5
    radius = min(radius, half_w, half_h)
    dx = abs(px - (x0 + half_w)) - (half_w - radius)
    dy = abs(py - (y0 + half_h)) - (half_h - radius)
    return math.hypot(max(dx, 0.0), max(dy, 0.0)) + min(max(dx, dy), 0.0) - radius


def _segment_distance(px: float, py: float, ax: float, ay: float, bx: float, by: float) -> float:
    abx, aby = bx - ax, by - ay
    denom = abx * abx + aby * aby
    t = 0.0 if denom == 0.0 else max(0.0, min(1.0, ((px - ax) * abx + (py - ay) * aby) / denom))
    return math.hypot(px - (ax + t * abx), py - (ay + t * aby))


def stroked_chevron(
    ax: float, ay: float, bx: float, by: float, cx: float, cy: float, radius: float
) -> list[tuple[float, float]]:
    def unit(x: float, y: float) -> tuple[float, float]:
        length = math.hypot(x, y) or 1.0
        return x / length, y / length

    def shift(x: float, y: float, nx: float, ny: float, sign: float) -> tuple[float, float]:
        return x + nx * radius * sign, y + ny * radius * sign

    d1 = unit(bx - ax, by - ay)
    d2 = unit(cx - bx, cy - by)
    n1 = (-d1[1], d1[0])
    n2 = (-d2[1], d2[0])

    def miter(sign: float) -> tuple[float, float]:
        p1 = shift(ax, ay, n1[0], n1[1], sign)
        p2 = shift(bx, by, n2[0], n2[1], sign)
        det = d1[0] * d2[1] - d1[1] * d2[0]
        t = ((p2[0] - p1[0]) * d2[1] - (p2[1] - p1[1]) * d2[0]) / det
        return p1[0] + d1[0] * t, p1[1] + d1[1] * t

    return [
        shift(ax, ay, n1[0], n1[1], 1.0),
        miter(1.0),
        shift(cx, cy, n2[0], n2[1], 1.0),
        shift(cx, cy, n2[0], n2[1], -1.0),
        miter(-1.0),
        shift(ax, ay, n1[0], n1[1], -1.0),
    ]


def sd_polygon(px: float, py: float, points: list[tuple[float, float]]) -> float:
    distance = 1e9
    inside = False
    count = len(points)
    for index in range(count):
        ax, ay = points[index]
        bx, by = points[(index + 1) % count]
        distance = min(distance, _segment_distance(px, py, ax, ay, bx, by))
        if (ay > py) != (by > py):
            cross = (bx - ax) * (py - ay) / (by - ay) + ax
            if px < cross:
                inside = not inside
    return -distance if inside else distance


_CHEVRON_POLYS: list[list[tuple[float, float]]] | None = None


def chevron_polygons() -> list[list[tuple[float, float]]]:
    global _CHEVRON_POLYS
    if _CHEVRON_POLYS is None:
        _CHEVRON_POLYS = [
            stroked_chevron(ax, ay, bx, by, cx, cy, CHEVRON_RADIUS)
            for ax, ay, bx, by, cx, cy in chevron_centerlines()
        ]
    return _CHEVRON_POLYS


def chevron_distance(px: float, py: float) -> float:
    distance = 1e9
    for polygon in chevron_polygons():
        distance = min(distance, sd_polygon(px, py, polygon))
    return distance


def glyph(px: float, py: float) -> float:
    return min(sd_round_box(px, py, *CURSOR, CURSOR_RADIUS), chevron_distance(px, py))


def coverage(signed_distance: float) -> float:
    if signed_distance >= 0.5:
        return 0.0
    if signed_distance <= -0.5:
        return 1.0
    return 0.5 - signed_distance


def mark_color(py: float) -> tuple[float, float, float]:
    top, bot = glyph_span()
    return lerp(CYAN_HI, CYAN_LO, (py - top) / (bot - top))


def band_coverage(py: float, y0: float, y1: float, scale: float) -> float:
    return coverage((y0 - py) * scale) * coverage((py - y1) * scale)


_SHADOW_LEN = math.hypot(SHADOW_X, SHADOW_Y)
_SHADOW_DX = -SHADOW_X / _SHADOW_LEN
_SHADOW_DY = -SHADOW_Y / _SHADOW_LEN


def ray_disk_roots(
    px: float, py: float, dx: float, dy: float, cx: float, cy: float, radius: float, max_t: float
) -> list[float]:
    ox, oy = px - cx, py - cy
    closest = ox * dx + oy * dy
    disc = closest * closest - (ox * ox + oy * oy - radius * radius)
    if disc < 0.0:
        return []
    root = math.sqrt(disc)
    return [t for t in (-closest - root, -closest + root) if 0.0 <= t <= max_t]


def ray_polygon(
    px: float,
    py: float,
    dx: float,
    dy: float,
    points: list[tuple[float, float]],
    max_t: float,
) -> float | None:
    if sd_polygon(px, py, points) <= 0.0:
        return 0.0
    hits: list[float] = []
    count = len(points)
    for index in range(count):
        ax, ay = points[index]
        bx, by = points[(index + 1) % count]
        ex, ey = bx - ax, by - ay
        det = dx * ey - dy * ex
        if abs(det) < 1e-8:
            continue
        t = ((ax - px) * ey - (ay - py) * ex) / det
        u = ((ax - px) * dy - (ay - py) * dx) / det
        if 0.0 <= t <= max_t and 0.0 <= u <= 1.0:
            hits.append(t)
    return min(hits) if hits else None


def ray_round_box(
    px: float,
    py: float,
    dx: float,
    dy: float,
    x0: float,
    y0: float,
    x1: float,
    y1: float,
    radius: float,
    max_t: float,
) -> float | None:
    if sd_round_box(px, py, x0, y0, x1, y1, radius) <= 0.0:
        return 0.0
    radius = min(radius, (x1 - x0) * 0.5, (y1 - y0) * 0.5)
    hits: list[float] = []

    def edge_h(edge_y: float, xa: float, xb: float) -> None:
        if abs(dy) < 1e-9:
            return
        t = (edge_y - py) / dy
        if 0.0 <= t <= max_t and xa <= px + t * dx <= xb:
            hits.append(t)

    def edge_v(edge_x: float, ya: float, yb: float) -> None:
        if abs(dx) < 1e-9:
            return
        t = (edge_x - px) / dx
        if 0.0 <= t <= max_t and ya <= py + t * dy <= yb:
            hits.append(t)

    edge_h(y0, x0 + radius, x1 - radius)
    edge_h(y1, x0 + radius, x1 - radius)
    edge_v(x0, y0 + radius, y1 - radius)
    edge_v(x1, y0 + radius, y1 - radius)
    for cx, cy, qx, qy in (
        (x0 + radius, y0 + radius, -1.0, -1.0),
        (x1 - radius, y0 + radius, 1.0, -1.0),
        (x0 + radius, y1 - radius, -1.0, 1.0),
        (x1 - radius, y1 - radius, 1.0, 1.0),
    ):
        for t in ray_disk_roots(px, py, dx, dy, cx, cy, radius, max_t):
            x = px + t * dx
            y = py + t * dy
            if (x - cx) * qx >= -1e-3 and (y - cy) * qy >= -1e-3:
                hits.append(t)
    return min(hits) if hits else None


def shadow_distance(px: float, py: float) -> float | None:
    best: float | None = None
    for polygon in chevron_polygons():
        hit = ray_polygon(px, py, _SHADOW_DX, _SHADOW_DY, polygon, _SHADOW_LEN)
        if hit is not None and (best is None or hit < best):
            best = hit
    x0, y0, x1, y1 = CURSOR
    hit = ray_round_box(px, py, _SHADOW_DX, _SHADOW_DY, x0, y0, x1, y1, CURSOR_RADIUS, _SHADOW_LEN)
    if hit is not None and (best is None or hit < best):
        best = hit
    return best


def long_shadow(px: float, py: float, scale: float) -> float:
    # The cast stays inside the mark's diagonal footprint.
    if px < 16.0 or py < 60.0 or px > 252.0 or py > 236.0:
        return 0.0
    half = 0.25 / scale
    total = 0.0
    for oy in (-half, half):
        for ox in (-half, half):
            distance = shadow_distance(px + ox, py + oy)
            if distance is not None:
                total += SHADOW_ALPHA * (1.0 - distance / _SHADOW_LEN)
    return total * 0.25


def lerp(a: tuple[int, int, int], b: tuple[int, int, int], t: float) -> tuple[float, float, float]:
    t = 0.0 if t < 0.0 else 1.0 if t > 1.0 else t
    return (
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    )


def over(pixel: list[float], red: float, green: float, blue: float, alpha: float) -> None:
    if alpha <= 0.0:
        return
    inverse = 1.0 - alpha
    pixel[0] = red * alpha + pixel[0] * inverse
    pixel[1] = green * alpha + pixel[1] * inverse
    pixel[2] = blue * alpha + pixel[2] * inverse
    pixel[3] = alpha + pixel[3] * inverse


def render(size: int) -> bytes:
    scale = size / VIEW
    pixels = [[[0.0, 0.0, 0.0, 0.0] for _ in range(size)] for _ in range(size)]
    for y in range(size):
        py = (y + 0.5) / scale
        t = (py - 8) / 240
        fill = lerp(BG_TOP, BG_BOT, t)
        row = pixels[y]
        for x in range(size):
            px = (x + 0.5) / scale
            signed = sd_round_box(px, py, *PLATE, PLATE_RADIUS)
            alpha = coverage(signed * scale)
            if alpha:
                over(row[x], fill[0], fill[1], fill[2], alpha)
                shade = long_shadow(px, py, scale) * alpha
                if shade:
                    over(row[x], float(SHADOW[0]), float(SHADOW[1]), float(SHADOW[2]), shade)
                lip = band_coverage(py, 8.0, 8.0 + RIM, scale) * alpha
                if lip:
                    over(row[x], 255.0, 255.0, 255.0, RIM_LIGHT * lip)
                foot = band_coverage(py, 248.0 - RIM, 248.0, scale) * alpha
                if foot:
                    over(row[x], 0.0, 0.0, 0.0, RIM_DARK * foot)
            mark = coverage(glyph(px, py) * scale)
            if mark:
                red, green, blue = mark_color(py)
                over(row[x], red, green, blue, mark)
    out = bytearray(size * size * 4)
    i = 0
    for row in pixels:
        for red, green, blue, alpha in row:
            out[i] = int(red + 0.5)
            out[i + 1] = int(green + 0.5)
            out[i + 2] = int(blue + 0.5)
            out[i + 3] = int(alpha * 255.0 + 0.5)
            i += 4
    return bytes(out)


def png_bytes(width: int, height: int, rgba: bytes) -> bytes:
    def chunk(tag: bytes, data: bytes) -> bytes:
        crc = zlib.crc32(tag + data) & 0xFFFFFFFF
        return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", crc)

    raw = b"".join(b"\x00" + rgba[y * width * 4 : (y + 1) * width * 4] for y in range(height))
    ihdr = struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b"")


def ico_bytes(images: list[tuple[int, bytes]]) -> bytes:
    count = len(images)
    offset = 6 + 16 * count
    header = struct.pack("<HHH", 0, 1, count)
    entries = bytearray()
    payload = bytearray()
    for size, data in images:
        stored = 0 if size >= 256 else size
        entries += struct.pack("<BBBBHHII", stored, stored, 0, 0, 1, 32, len(data), offset)
        payload += data
        offset += len(data)
    return bytes(header + entries + payload)


def pixel(rgba: bytes, size: int, x: int, y: int) -> tuple[int, int, int, int]:
    i = (y * size + x) * 4
    return rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]


def check_square(rgba: bytes, size: int) -> None:
    scale = size / VIEW

    def at(vx: float, vy: float) -> tuple[int, int, int, int]:
        return pixel(rgba, size, int(vx * scale), int(vy * scale))

    if at(0.0, 0.0)[3] != 0:
        raise SystemExit(f"linux: margin outside the plate is not clear {at(0.0, 0.0)}")
    corner = at(PLATE[0] + 1.0, PLATE[1] + 1.0)
    if corner[3] != 255:
        raise SystemExit(f"linux: plate corner is not opaque {corner}")
    cursor = at(128.0, 128.0)
    if cursor[1] < 140 or cursor[2] < cursor[0]:
        raise SystemExit(f"linux: cursor is not the lit cyan {cursor}")
    # The long shadow falls down-right of the cursor, on the plate.
    lit = at(160.0, 100.0)
    cast = at(160.0, 160.0)
    if sum(cast[:3]) > sum(lit[:3]) - 25:
        raise SystemExit(f"linux: cursor does not cast a long shadow {cast} vs {lit}")
    top = at(128.0, 20.0)
    below = at(128.0, 220.0)
    if top[1] <= below[1]:
        raise SystemExit(f"linux: plate is not lighter at the top {top} vs {below}")


# Round slabs for the platform icons. Linux keeps the mitered mark above.
# (outer upper, tip, outer lower, stroke radius)
PLATFORM_CHEVRONS = (
    ((36.0, 70.0), (74.0, 128.0), (36.0, 186.0), 20.0),
    ((220.0, 70.0), (182.0, 128.0), (220.0, 186.0), 20.0),
)
PLATFORM_CURSOR = (110.0, 110.0, 146.0, 146.0)

# Windows 11 Fluent. Rounded mica tile, shadow falls straight down.
WIN_PLATE = (10.0, 8.0, 246.0, 234.0)
WIN_PLATE_R = 44.0
WIN_BG_TOP = (0x3E, 0x44, 0x4C)
WIN_BG_BOT = (0x1A, 0x1C, 0x20)
# One blue for the chevrons and the cursor. The top stays blue, not white.
WIN_SLAB_HI = (0x5E, 0xC4, 0xFF)
WIN_SLAB_LO = (0x00, 0x62, 0xC0)
WIN_CURSOR_R = 8.0

# Liquid Glass. Light comes from above, across the whole mark.
GLASS_BG_TOP = (0x9E, 0xC4, 0xE6)
GLASS_BG_BOT = (0x16, 0x28, 0x44)
# Chevrons and cursor share this blue. Highlights stay a pale blue, not white.
GLASS_CHEV_HI = (0x6A, 0xD0, 0xFF)
GLASS_CHEV_LO = (0x00, 0x6E, 0xE0)
GLASS_CURSOR_R = 12.0


def pack_rgba(pixels: list[list[list[float]]]) -> bytes:
    out = bytearray(len(pixels) * len(pixels[0]) * 4)
    i = 0
    for row in pixels:
        for red, green, blue, alpha in row:
            out[i] = int(red + 0.5)
            out[i + 1] = int(green + 0.5)
            out[i + 2] = int(blue + 0.5)
            out[i + 3] = int(alpha * 255.0 + 0.5)
            i += 4
    return bytes(out)


def platform_chevron_sd(px: float, py: float, spec: tuple) -> float:
    (ax, ay), (bx, by), (cx, cy), radius = spec
    return min(
        _segment_distance(px, py, ax, ay, bx, by),
        _segment_distance(px, py, bx, by, cx, cy),
    ) - radius


def platform_cursor_sd(px: float, py: float, radius: float) -> float:
    return sd_round_box(px, py, *PLATFORM_CURSOR, radius)


def facing(sd_at, px: float, py: float, eps: float = 6.0) -> tuple[float, float]:
    # Wide sample so the join of the two arms does not crease.
    # Screen-up is negative y, so a top rim has gy < 0.
    gx = sd_at(px + eps, py) - sd_at(px - eps, py)
    gy = sd_at(px, py + eps) - sd_at(px, py - eps)
    scale = 1.0 / (math.hypot(gx, gy) or 1.0)
    return max(0.0, -gy * scale), max(0.0, gy * scale)


def mark_t(py: float) -> float:
    ys = [y - radius for (_a, y), _tip, (_c, _cy), radius in PLATFORM_CHEVRONS]
    ys += [y + radius for (_a, _ay), _tip, (_c, y), radius in PLATFORM_CHEVRONS]
    y0, y1 = min(ys), max(ys)
    t = (py - y0) / (y1 - y0)
    return 0.0 if t < 0.0 else 1.0 if t > 1.0 else t


def mark_mix(t: float) -> float:
    # A tall gradient made the upper arm light, the lower arm dark, and the
    # square a third shade. Keep one blue. The rim carries the light.
    return 0.46 + 0.08 * t


def cast_shadow(point: list[float], px: float, py: float, sd_at, scale: float, plate: float = 1.0) -> None:
    for dy, alpha in ((16.0, 0.11), (9.0, 0.16), (4.0, 0.20)):
        shade = coverage(sd_at(px, py - dy) * scale * 0.42)
        if shade:
            over(point, 6.0, 14.0, 28.0, shade * alpha * plate)


def render_windows(size: int) -> bytes:
    scale = size / VIEW
    plate = WIN_PLATE
    pixels = [[[0.0, 0.0, 0.0, 0.0] for _ in range(size)] for _ in range(size)]
    px0, py0, px1, py1 = plate
    slabs = [
        (spec, lambda qx, qy, spec=spec: platform_chevron_sd(qx, qy, spec)) for spec in PLATFORM_CHEVRONS
    ]
    cursor_at = lambda qx, qy: platform_cursor_sd(qx, qy, WIN_CURSOR_R)
    for y in range(size):
        py = (y + 0.5) / scale
        row = pixels[y]
        plate_rgb = lerp(WIN_BG_TOP, WIN_BG_BOT, (py - py0) / (py1 - py0))
        for x in range(size):
            px = (x + 0.5) / scale
            point = row[x]
            for dy, alpha in ((4.0, 0.22), (9.0, 0.16), (15.0, 0.10)):
                shade = coverage(sd_round_box(px, py - dy, *plate, WIN_PLATE_R) * scale)
                if shade:
                    over(point, 0.0, 0.0, 0.0, shade * alpha)
            plate_sd = sd_round_box(px, py, *plate, WIN_PLATE_R)
            plate_alpha = coverage(plate_sd * scale)
            if plate_alpha:
                over(point, *plate_rgb, plate_alpha)
                if -1.6 < plate_sd < 0.4:
                    edge = 1.0 - abs(plate_sd + 0.6) / 1.0
                    if edge > 0.0:
                        over(point, 255.0, 255.0, 255.0, 0.28 * edge * plate_alpha)
                sheen_t = (py - py0) / (py1 - py0)
                if sheen_t < 0.22:
                    over(point, 255.0, 255.0, 255.0, 0.16 * (1.0 - sheen_t / 0.22) * plate_alpha)
                for _spec, sd_at in slabs:
                    cast_shadow(point, px, py, sd_at, scale, plate_alpha)
                cast_shadow(point, px, py, cursor_at, scale, plate_alpha)
            for _spec, sd_at in slabs:
                paint_mark(point, px, py, sd_at, scale, plate_alpha, WIN_SLAB_HI, WIN_SLAB_LO)
            paint_mark(point, px, py, cursor_at, scale, plate_alpha, WIN_SLAB_HI, WIN_SLAB_LO)
    return pack_rgba(pixels)


def paint_mark(point, px, py, sd_at, scale, cover, hi, lo) -> None:
    sd = sd_at(px, py)
    cov = coverage(sd * scale) * cover
    if sd > 1.2 or cov <= 0.0:
        return
    red, green, blue = lerp(hi, lo, mark_mix(mark_t(py)))
    over(point, red, green, blue, cov)
    if -2.6 < sd < 0.7:
        up, down = facing(sd_at, px, py)
        closeness = 1.0 - abs(sd + 0.95) / 1.65
        if closeness <= 0.0:
            return
        # Same catch-light on every shape. Strength does not depend on height.
        if up > 0.25:
            over(point, 186.0, 230.0, 255.0, 0.55 * closeness * up * cov)
        if down > 0.25:
            over(point, 0.0, 40.0, 110.0, 0.42 * closeness * down * cov)


def glass_background(px: float, py: float) -> tuple[float, float, float]:
    base = lerp(GLASS_BG_TOP, GLASS_BG_BOT, (py / VIEW) ** 0.9)
    dx = (px - 128.0) / 108.0
    dy = (py - 24.0) / 88.0
    pool = max(0.0, 1.0 - (dx * dx * 0.7 + dy * dy))
    return (
        base[0] + (255.0 - base[0]) * pool * 0.62,
        base[1] + (255.0 - base[1]) * pool * 0.55,
        base[2] + (255.0 - base[2]) * pool * 0.38,
    )


def render_glass(size: int) -> bytes:
    scale = size / VIEW
    slabs = [(spec, lambda qx, qy, spec=spec: platform_chevron_sd(qx, qy, spec)) for spec in PLATFORM_CHEVRONS]
    cursor_at = lambda qx, qy: platform_cursor_sd(qx, qy, GLASS_CURSOR_R)
    pixels = [[[0.0, 0.0, 0.0, 0.0] for _ in range(size)] for _ in range(size)]
    for y in range(size):
        py = (y + 0.5) / scale
        row = pixels[y]
        for x in range(size):
            px = (x + 0.5) / scale
            point = row[x]
            over(point, *glass_background(px, py), 1.0)
            if py < 42.0:
                over(point, 255.0, 255.0, 255.0, 0.48 * (1.0 - py / 42.0) ** 1.35)
            for _spec, sd_at in slabs:
                cast_shadow(point, px, py, sd_at, scale)
            cast_shadow(point, px, py, cursor_at, scale)
            for _spec, sd_at in slabs:
                paint_mark(point, px, py, sd_at, scale, 1.0, GLASS_CHEV_HI, GLASS_CHEV_LO)
            paint_mark(point, px, py, cursor_at, scale, 1.0, GLASS_CHEV_HI, GLASS_CHEV_LO)
    mask_squircle(pixels, size)
    return pack_rgba(pixels)


def squircle_sd(px: float, py: float) -> float:
    # Apple's icon is a quintic superellipse, not a rounded rectangle.
    n = 5.0
    half = 128.0
    x = abs(px - 128.0) / half
    y = abs(py - 128.0) / half
    field = (x**n if x else 0.0) + (y**n if y else 0.0)
    return (field ** (1.0 / n) - 1.0) * half


def mask_squircle(pixels: list[list[list[float]]], size: int) -> None:
    scale = size / VIEW
    for y in range(size):
        py = (y + 0.5) / scale
        row = pixels[y]
        for x in range(size):
            cover = coverage(squircle_sd((x + 0.5) / scale, py) * scale)
            if cover < 1.0:
                row[x][3] *= cover


def squircle_path() -> str:
    half = 128.0
    steps = 96
    coords = []
    for i in range(steps):
        theta = 2.0 * math.pi * i / steps
        c = math.cos(theta)
        s = math.sin(theta)
        x = 128.0 + half * math.copysign(abs(c) ** 0.4, c)
        y = 128.0 + half * math.copysign(abs(s) ** 0.4, s)
        coords.append(f"{fmt(x)} {fmt(y)}")
    return "M" + " L".join(coords) + " Z"


def round_chevrons(stroke: str, width: float, extra: str = "") -> str:
    paths = []
    for (ax, ay), (bx, by), (cx, cy), _radius in PLATFORM_CHEVRONS:
        paths.append(
            f'<path d="M{fmt(ax)} {fmt(ay)} L{fmt(bx)} {fmt(by)} L{fmt(cx)} {fmt(cy)}" '
            f'fill="none" stroke="{stroke}" stroke-width="{fmt(width)}" '
            f'stroke-linecap="round" stroke-linejoin="round"{extra}/>'
        )
    return "\n  ".join(paths)


def body_stops(hi: tuple[int, int, int], lo: tuple[int, int, int]) -> str:
    top = tuple(int(channel + 0.5) for channel in lerp(hi, lo, mark_mix(0.0)))
    bot = tuple(int(channel + 0.5) for channel in lerp(hi, lo, mark_mix(1.0)))
    return (
        f'<stop offset="0" stop-color="{hex_rgb(top)}"/>\n'
        f'      <stop offset="1" stop-color="{hex_rgb(bot)}"/>'
    )


def glass_svg() -> str:
    x0, y0, x1, y1 = PLATFORM_CURSOR
    body = round_chevrons("url(#frost)", 40.0)
    shadow = round_chevrons("#061428", 40.0)
    return f"""\
<svg xmlns="http://www.w3.org/2000/svg" width="256" height="256" viewBox="0 0 256 256">
  <!-- Liquid Glass inside Apple's squircle. Chevrons and cursor share one blue. -->
  <defs>
    <linearGradient id="bg" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="{hex_rgb(GLASS_BG_TOP)}"/>
      <stop offset="1" stop-color="{hex_rgb(GLASS_BG_BOT)}"/>
    </linearGradient>
    <radialGradient id="pool" cx="50%" cy="8%" r="58%">
      <stop offset="0" stop-color="#ffffff" stop-opacity="0.9"/>
      <stop offset="1" stop-color="#ffffff" stop-opacity="0"/>
    </radialGradient>
    <linearGradient id="lip" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#ffffff" stop-opacity="0.55"/>
      <stop offset="1" stop-color="#ffffff" stop-opacity="0"/>
    </linearGradient>
    <linearGradient id="frost" gradientUnits="userSpaceOnUse" x1="0" y1="50" x2="0" y2="206">
      {body_stops(GLASS_CHEV_HI, GLASS_CHEV_LO)}
    </linearGradient>
    <filter id="soft" x="-40%" y="-40%" width="180%" height="180%">
      <feGaussianBlur stdDeviation="6"/>
    </filter>
    <clipPath id="squircle">
      <path d="{squircle_path()}"/>
    </clipPath>
  </defs>
  <g clip-path="url(#squircle)">
  <rect width="256" height="256" fill="url(#bg)"/>
  <rect width="256" height="256" fill="url(#pool)"/>
  <rect width="256" height="46" fill="url(#lip)"/>
  <g opacity="0.40" transform="translate(0 14)" filter="url(#soft)">
  {shadow}
  </g>
  {body}
  <rect x="{fmt(x0)}" y="{fmt(y0)}" width="{fmt(x1 - x0)}" height="{fmt(y1 - y0)}" rx="{fmt(GLASS_CURSOR_R)}" fill="url(#frost)"/>
  </g>
</svg>
"""


def windows_svg() -> str:
    px0, py0, px1, py1 = WIN_PLATE
    x0, y0, x1, y1 = PLATFORM_CURSOR
    slabs = round_chevrons("url(#slab)", 40.0)
    return f"""\
<svg xmlns="http://www.w3.org/2000/svg" width="256" height="256" viewBox="0 0 256 256">
  <!-- Fluent mica tile. The chevrons and the cursor share the accent blue. -->
  <defs>
    <linearGradient id="bg" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="{hex_rgb(WIN_BG_TOP)}"/>
      <stop offset="1" stop-color="{hex_rgb(WIN_BG_BOT)}"/>
    </linearGradient>
    <linearGradient id="slab" gradientUnits="userSpaceOnUse" x1="0" y1="50" x2="0" y2="206">
      {body_stops(WIN_SLAB_HI, WIN_SLAB_LO)}
    </linearGradient>
    <filter id="shadow" x="-20%" y="-20%" width="140%" height="150%">
      <feDropShadow dx="0" dy="8" stdDeviation="6" flood-color="#000" flood-opacity="0.45"/>
    </filter>
    <filter id="cast" x="-30%" y="-30%" width="160%" height="170%">
      <feDropShadow dx="0" dy="5" stdDeviation="3" flood-color="#000" flood-opacity="0.35"/>
    </filter>
    <clipPath id="plate">
      <rect x="{fmt(px0)}" y="{fmt(py0)}" width="{fmt(px1 - px0)}" height="{fmt(py1 - py0)}" rx="{fmt(WIN_PLATE_R)}"/>
    </clipPath>
  </defs>
  <rect x="{fmt(px0)}" y="{fmt(py0)}" width="{fmt(px1 - px0)}" height="{fmt(py1 - py0)}" rx="{fmt(WIN_PLATE_R)}" fill="url(#bg)" filter="url(#shadow)"/>
  <g clip-path="url(#plate)">
    <rect x="{fmt(px0)}" y="{fmt(py0)}" width="{fmt(px1 - px0)}" height="48" fill="#fff" opacity="0.08"/>
    <g filter="url(#cast)">
  {slabs}
    </g>
    <rect x="{fmt(x0)}" y="{fmt(y0)}" width="{fmt(x1 - x0)}" height="{fmt(y1 - y0)}" rx="{fmt(WIN_CURSOR_R)}" fill="url(#slab)"/>
  </g>
</svg>
"""


def check_windows(rgba: bytes, size: int) -> None:
    scale = size / VIEW

    def at(vx: float, vy: float) -> tuple[int, int, int, int]:
        return pixel(rgba, size, min(size - 1, int(vx * scale)), min(size - 1, int(vy * scale)))

    corner = at(WIN_PLATE[0], WIN_PLATE[1])
    if corner[3] != 0:
        raise SystemExit(f"windows: plate corner is not rounded {corner}")
    face = at((WIN_PLATE[0] + WIN_PLATE[2]) * 0.5, WIN_PLATE[1] + 8.0)
    if face[3] != 255:
        raise SystemExit(f"windows: tile face is not opaque {face}")
    cursor = at(128.0, 128.0)
    if cursor[2] < cursor[0] + 30 or cursor[2] < 160:
        raise SystemExit(f"windows: cursor is not the accent blue {cursor}")
    same_mark("windows", at)
    outside = at(2.0, 2.0)
    if outside[3] != 0:
        raise SystemExit(f"windows: outside the tile is not clear {outside}")


def check_glass(name: str, rgba: bytes, size: int) -> None:
    scale = size / VIEW

    def at(vx: float, vy: float) -> tuple[int, int, int, int]:
        return pixel(rgba, size, min(size - 1, int(vx * scale)), min(size - 1, int(vy * scale)))

    # Quintic superellipse: corners clear, side centers filled, and the
    # corner bulge (inside the squircle, outside a circle on the same box).
    corner = at(1.0, 1.0)
    if corner[3] != 0:
        raise SystemExit(f"{name}: squircle corner is not clear {corner}")
    edge = at(128.0, 2.0)
    if edge[3] != 255:
        raise SystemExit(f"{name}: squircle does not meet the side {edge}")
    bulge = at(22.0, 22.0)
    if bulge[3] < 250:
        raise SystemExit(f"{name}: squircle bulge is missing {bulge}")
    top = at(128.0, 12.0)
    bot = at(128.0, 244.0)
    if sum(top[:3]) < sum(bot[:3]) + 40:
        raise SystemExit(f"{name}: glass is not brighter at the top {top} vs {bot}")
    cursor = at(128.0, 128.0)
    margin = at(16.0, 220.0)
    if cursor[2] < cursor[0] + 15 or sum(cursor[:3]) < sum(margin[:3]) + 30:
        raise SystemExit(f"{name}: cursor does not glow {cursor} vs {margin}")
    same_mark(name, at)


def same_mark(name: str, at) -> None:
    # Upper arm, lower arm, and the square are one blue.
    upper = at(50.0, 96.0)
    lower = at(50.0, 160.0)
    cursor = at(128.0, 128.0)
    for label, sample in (("upper", upper), ("lower", lower)):
        if any(abs(sample[i] - cursor[i]) > 24 for i in range(3)):
            raise SystemExit(f"{name}: {label} arm does not match the cursor {sample} vs {cursor}")
        if sample[2] < sample[0] + 30:
            raise SystemExit(f"{name}: {label} arm is not the accent blue {sample}")
    # The top edge of each shape catches the same light.
    cursor_rim = at(128.0, 110.0)
    if sum(cursor_rim[:3]) < sum(cursor[:3]) + 12:
        raise SystemExit(f"{name}: cursor top is not lit {cursor_rim} vs {cursor}")
    cap = at(36.0, 50.0)
    if sum(cap[:3]) < sum(upper[:3]) + 8:
        raise SystemExit(f"{name}: chevron top is not lit {cap} vs {upper}")


def main() -> None:
    macos = ROOT / "etc" / "macos"
    windows = ROOT / "etc" / "windows"
    linux = ROOT / "etc" / "linux"
    macos.mkdir(parents=True, exist_ok=True)
    windows.mkdir(parents=True, exist_ok=True)
    linux.mkdir(parents=True, exist_ok=True)
    linux_svg = svg_text()
    if 'rx="0"' not in linux_svg:
        raise SystemExit("linux icon lost its square corners")
    linux_256 = render(256)
    check_square(linux_256, 256)
    windows_256 = render_windows(256)
    check_windows(windows_256, 256)
    glass_256 = render_glass(256)
    check_glass("glass", glass_256, 256)
    glass_1024 = render_glass(1024)
    check_glass("glass-1024", glass_1024, 1024)
    (ROOT / "etc" / "starcom.svg").write_text(linux_svg, encoding="utf-8")
    (linux / "icon.png").write_bytes(png_bytes(256, 256, linux_256))
    (macos / "icon.svg").write_text(glass_svg(), encoding="utf-8")
    (windows / "icon.svg").write_text(windows_svg(), encoding="utf-8")
    # cargo-bundle's ICNS writer maps density-1 PNGs to 16/32/64/128/256/512.
    # 1024 px is 512@2x; the @2x filename is what sets density so the type
    # matches. icon.png is the in-app preview of that same 256 artwork.
    for size in (16, 32, 64, 128, 256, 512):
        data = png_bytes(size, size, glass_256 if size == 256 else render_glass(size))
        (macos / f"icon_{size}.png").write_bytes(data)
        if size == 256:
            (macos / "icon.png").write_bytes(data)
    (macos / "icon_512@2x.png").write_bytes(png_bytes(1024, 1024, glass_1024))
    (windows / "icon.png").write_bytes(png_bytes(256, 256, windows_256))
    ico_sizes = (16, 24, 32, 48, 64, 128, 256)
    ico_images = []
    for size in ico_sizes:
        rgba = windows_256 if size == 256 else render_windows(size)
        ico_images.append((size, png_bytes(size, size, rgba)))
    (windows / "icon.ico").write_bytes(ico_bytes(ico_images))
    print(
        "wrote etc/starcom.svg, etc/linux/icon.png, etc/macos/icon.svg, "
        "etc/macos/icon.png, etc/macos/icon_{16,32,64,128,256,512}.png, "
        "etc/macos/icon_512@2x.png, etc/windows/icon.svg, etc/windows/icon.png, "
        "etc/windows/icon.ico"
    )


if __name__ == "__main__":
    main()
