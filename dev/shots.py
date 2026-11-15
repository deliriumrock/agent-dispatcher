#!/usr/bin/env python3
"""Разбирает журнал поддельного терминала: какие картинки zellij передал и где разместил.

Печатает события графического протокола и собирает итоговый «экран» (только картинки)
в PNG. Использование: shots.py <журнал> <png> [смещение-в-байтах] [колонок строк]
"""
import base64
import re
import struct
import sys
import zlib

CELL_W, CELL_H = 10, 20
TOKEN = re.compile(rb"\x1b\[(\d+);(\d+)H|\x1b\[H|\x1b_G([^;\x1b]*)(?:;([^\x1b]*))?\x1b\\|\x1b\[([23])J")


def png(path: str, width: int, height: int, rgba: bytearray) -> None:
    """Пишет RGBA-буфер в PNG без сторонних библиотек."""
    def chunk(kind: bytes, body: bytes) -> bytes:
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body))

    rows = b"".join(b"\0" + bytes(rgba[y * width * 4:(y + 1) * width * 4]) for y in range(height))
    with open(path, "wb") as out:
        out.write(b"\x89PNG\r\n\x1a\n")
        out.write(chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)))
        out.write(chunk(b"IDAT", zlib.compress(rows, 6)))
        out.write(chunk(b"IEND", b""))


def main() -> int:
    log_path, png_path = sys.argv[1], sys.argv[2]
    start = int(sys.argv[3]) if len(sys.argv) > 3 else 0
    cols = int(sys.argv[4]) if len(sys.argv) > 4 else 100
    rows = int(sys.argv[5]) if len(sys.argv) > 5 else 30
    data = open(log_path, "rb").read()[start:]

    images: dict[int, tuple[int, int, bytes]] = {}
    placements: dict[tuple[int, int], dict] = {}
    cursor = (1, 1)
    partial: tuple[dict, list[bytes]] | None = None

    for match in TOKEN.finditer(data):
        if match.group(1):
            cursor = (int(match.group(1)), int(match.group(2)))
            continue
        if match.group(0) == b"\x1b[H":
            cursor = (1, 1)
            continue
        if match.group(5):
            print(f"@{start + match.start():>8} очистка экрана ED {match.group(5).decode()}")
            continue
        keys = dict(item.split("=", 1) for item in match.group(3).decode().split(",") if "=" in item)
        payload = match.group(4) or b""
        if partial is not None and "a" not in keys:
            partial[1].append(payload)
            if keys.get("m", "0") == "0":
                keys, parts = partial[0], partial[1]
                payload, partial = b"".join(parts), None
            else:
                continue
        elif keys.get("m") == "1":
            partial = (keys, [payload])
            continue

        action = keys.get("a", "t")
        ident = int(keys.get("i", 0))
        if action in ("t", "T"):
            raw = base64.b64decode(payload)
            if keys.get("o") == "z":
                raw = zlib.decompress(raw)
            width, height = int(keys["s"]), int(keys["v"])
            depth = 4 if keys.get("f", "32") == "32" else 3
            if depth == 3:
                raw = b"".join(raw[i:i + 3] + b"\xff" for i in range(0, len(raw), 3))
            images[ident] = (width, height, raw)
            alphas = sorted(set(raw[3::4]))
            print(f"@{start + match.start():>8} картинка i={ident} {width}x{height} f={keys.get('f')}"
                  f" сжато={len(payload)} альфа={alphas[:6]}{'…' if len(alphas) > 6 else ''}")
        if action in ("p", "T"):
            place = int(keys.get("p", 0))
            placements[(ident, place)] = {"cursor": cursor, **keys}
            print(f"@{start + match.start():>8} размещение i={ident} p={place} строка={cursor[0]} колонка={cursor[1]}"
                  f" x={keys.get('x')} y={keys.get('y')} w={keys.get('w')} h={keys.get('h')} z={keys.get('z')}"
                  f" c={keys.get('c')} r={keys.get('r')}")
        if action == "d":
            what = keys.get("d", "a")
            if what in ("a", "A"):
                placements.clear()
            elif what in ("i", "I"):
                for key in [key for key in placements if key[0] == ident and ("p" not in keys or key[1] == int(keys["p"]))]:
                    del placements[key]
                if what == "I":
                    images.pop(ident, None)
            print(f"@{start + match.start():>8} удаление d={what} i={keys.get('i')} p={keys.get('p')}")

    width, height = cols * CELL_W, rows * CELL_H
    canvas = bytearray(b"\x11\x11\x11\xff" * (width * height))
    for (ident, _place), keys in placements.items():
        if ident not in images:
            continue
        img_w, img_h, raw = images[ident]
        src_x, src_y = int(keys.get("x", 0)), int(keys.get("y", 0))
        crop_w = int(keys.get("w", 0)) or img_w - src_x
        crop_h = int(keys.get("h", 0)) or img_h - src_y
        left = (keys["cursor"][1] - 1) * CELL_W + int(keys.get("X", 0))
        top = (keys["cursor"][0] - 1) * CELL_H + int(keys.get("Y", 0))
        for dy in range(crop_h):
            y = top + dy
            if not 0 <= y < height or src_y + dy >= img_h:
                continue
            for dx in range(crop_w):
                x = left + dx
                if not 0 <= x < width or src_x + dx >= img_w:
                    continue
                at = ((src_y + dy) * img_w + src_x + dx) * 4
                r, g, b, a = raw[at:at + 4]
                to = (y * width + x) * 4
                for channel, value in enumerate((r, g, b)):
                    canvas[to + channel] = (value * a + canvas[to + channel] * (255 - a)) // 255
    if len(sys.argv) > 7:
        # Вырезать прямоугольник x,y,w,h и увеличить в целое число раз: так видны детали.
        cx, cy, cw, ch = (int(part) for part in sys.argv[6].split(","))
        scale = int(sys.argv[7])
        zoomed = bytearray()
        for y in range(ch):
            line = bytearray()
            for x in range(cw):
                at = ((cy + y) * width + cx + x) * 4
                line += bytes(canvas[at:at + 4]) * scale
            zoomed += line * scale
        width, height, canvas = cw * scale, ch * scale, zoomed
    png(png_path, width, height, canvas)
    print(f"итог: картинок {len(images)}, размещений {len(placements)} → {png_path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
