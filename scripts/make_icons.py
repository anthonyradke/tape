"""Draws the app and tray icons with no dependencies: a graphite tile with two tape reels.

Usage: python3 scripts/make_icons.py
Then:  cd app/src-tauri && cargo tauri icon icons/app-icon.png
"""
import math, struct, zlib, os

OUT = os.path.join(os.path.dirname(__file__), "..", "app", "src-tauri", "icons")

def png(path, size, pixels):
    raw = bytearray()
    for y in range(size):
        raw.append(0)
        raw.extend(pixels[y * size * 4:(y + 1) * size * 4])
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
    with open(path, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n")
        f.write(chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)))
        f.write(chunk(b"IDAT", zlib.compress(bytes(raw), 9)))
        f.write(chunk(b"IEND", b""))

def rounded(x, y, half, radius):
    dx, dy = max(abs(x) - (half - radius), 0), max(abs(y) - (half - radius), 0)
    return math.hypot(dx, dy) - radius

def render(size, shapes):
    """shapes: ordered (distance function in unit coords, rgb). 3x3 supersampled."""
    out = bytearray(size * size * 4)
    for py in range(size):
        for px in range(size):
            r = g = b = a = 0.0
            for sy in range(3):
                for sx in range(3):
                    x = (px + (sx + .5) / 3) / size * 2 - 1
                    y = (py + (sy + .5) / 3) / size * 2 - 1
                    color = None
                    for inside, rgb in shapes:
                        if inside(x, y):
                            color = rgb
                    if color:
                        r += color[0]; g += color[1]; b += color[2]; a += 1
            i = (py * size + px) * 4
            if a:
                out[i:i + 4] = bytes((int(r / a), int(g / a), int(b / a), int(a / 9 * 255)))
    return out

TILE, INK = (20, 21, 23), (236, 232, 223)

def reels(ring, hub=None, big=False):
    """Two reels joined by the tape running under them."""
    x0, y0, r = (.36, -.08, .27) if big else (.46, -.10, .36)
    t = .075 if big else .13
    hub = hub or ring
    def ring_at(cx):
        return lambda x, y: abs(math.hypot(x - cx, y - y0) - r) <= t
    def hub_at(cx):
        return lambda x, y: math.hypot(x - cx, y - y0) <= t * 1.15
    base = lambda x, y: abs(y - (y0 + r + (.0 if big else .0))) <= t and abs(x) <= x0
    return [(ring_at(-x0), ring), (ring_at(x0), ring), (hub_at(-x0), hub), (hub_at(x0), hub), (base, ring)]

os.makedirs(OUT, exist_ok=True)
tile = (lambda x, y: rounded(x, y, 1, .44) <= 0, TILE)
png(os.path.join(OUT, "app-icon.png"), 1024, render(1024, [tile] + reels(INK, big=True)))
for name, color in (("idle", (143, 140, 133)), ("replay", (76, 201, 111)), ("recording", (239, 75, 75)), ("trouble", (232, 185, 49))):
    png(os.path.join(OUT, f"tray-{name}.png"), 64, render(64, reels(color)))
