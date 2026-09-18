#!/usr/bin/env python3
"""Render a monospace TTF into a raw 8x16 1-bpp bitmap font (256 glyphs, CP437-ish ASCII range)."""
import sys
from PIL import Image, ImageDraw, ImageFont

TTF = sys.argv[1] if len(sys.argv) > 1 else "/usr/share/fonts/hack/Hack-Regular.ttf"
OUT = sys.argv[2] if len(sys.argv) > 2 else "kernel/src/console/font8x16.bin"
W, H = 8, 16

font = ImageFont.truetype(TTF, 14)
data = bytearray()
for code in range(256):
    ch = chr(code) if 32 <= code < 127 else " "
    img = Image.new("L", (W, H), 0)
    draw = ImageDraw.Draw(img)
    bbox = draw.textbbox((0, 0), ch, font=font)
    x = (W - (bbox[2] - bbox[0])) // 2 - bbox[0]
    y = 1 - bbox[1] + (H - 2 - (bbox[3] - bbox[1])) // 2
    draw.text((x, y), ch, font=font, fill=255)
    px = img.load()
    for row in range(H):
        byte = 0
        for col in range(W):
            if px[col, row] > 110:
                byte |= 0x80 >> col
        data.append(byte)

# block glyph for cursor at 0xDB
for row in range(H):
    data[0xDB * H + row] = 0xFF

with open(OUT, "wb") as f:
    f.write(data)
print(f"wrote {OUT}: {len(data)} bytes")
