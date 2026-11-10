#!/usr/bin/env python3
"""Write the Starcom app icon SVG, PNG, and ICO from one set of geometry."""
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


def glyph(px: float, py: float) -> float:
    distance = sd_round_box(px, py, *CURSOR, CURSOR_RADIUS)
    for polygon in chevron_polygons():
        distance = min(distance, sd_polygon(px, py, polygon))
    return distance


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


def main() -> None:
    macos = ROOT / "etc" / "macos"
    windows = ROOT / "etc" / "windows"
    macos.mkdir(parents=True, exist_ok=True)
    windows.mkdir(parents=True, exist_ok=True)
    (ROOT / "etc" / "starcom.svg").write_text(svg_text(), encoding="utf-8")
    # cargo-bundle's ICNS writer maps density-1 PNGs to 16/32/64/128/256/512.
    # 1024 px is 512@2x; the @2x filename is what sets density so the type
    # matches. icon.png stays 256 for the winit window icon.
    for size in (16, 32, 64, 128, 256, 512):
        data = png_bytes(size, size, render(size))
        (macos / f"icon_{size}.png").write_bytes(data)
        if size == 256:
            (macos / "icon.png").write_bytes(data)
    png1024 = png_bytes(1024, 1024, render(1024))
    (macos / "icon_512@2x.png").write_bytes(png1024)
    ico_images = [(size, png_bytes(size, size, render(size))) for size in (16, 24, 32, 48, 64, 128, 256)]
    (windows / "icon.ico").write_bytes(ico_bytes(ico_images))
    print(
        "wrote etc/starcom.svg, etc/macos/icon.png, etc/macos/icon_{16,32,64,128,256,512}.png, "
        "etc/macos/icon_512@2x.png, etc/windows/icon.ico"
    )


if __name__ == "__main__":
    main()
