"""Draws the app and tray icons with no dependencies: a cassette on a graphite tile.

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

def box(x, y, half_w, half_h, radius):
    dx, dy = max(abs(x) - (half_w - radius), 0), max(abs(y) - (half_h - radius), 0)
    return math.hypot(dx, dy) <= radius

def cassette(ink, cut, scale):
    """Body, one window across it and the two reels showing through. `cut` is drawn over the body,
    so None leaves a real hole, which is what a tray icon needs."""
    w, h, corner = .94 * scale, .64 * scale, .13 * scale
    win_w, win_h, win_y, win_corner = .70 * scale, .25 * scale, -.03 * scale, .25 * scale
    reel_x, reel = .425 * scale, .145 * scale
    return [
        (lambda x, y: box(x, y, w, h, corner), ink),
        (lambda x, y: box(x, y - win_y, win_w, win_h, win_corner), cut),
        (lambda x, y: math.hypot(x + reel_x, y - win_y) <= reel, ink),
        (lambda x, y: math.hypot(x - reel_x, y - win_y) <= reel, ink),
    ]

os.makedirs(OUT, exist_ok=True)
tile = (lambda x, y: rounded(x, y, 1, .44) <= 0, TILE)
png(os.path.join(OUT, "app-icon.png"), 1024, render(1024, [tile] + cassette(INK, TILE, .78)))
for name, color in (("idle", (143, 140, 133)), ("replay", (76, 201, 111)), ("recording", (239, 75, 75)), ("trouble", (232, 185, 49))):
    png(os.path.join(OUT, f"tray-{name}.png"), 64, render(64, cassette(color, None, 1.0)))
