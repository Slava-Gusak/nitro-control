"""Generates assets/icon.ico: red geometric "N" on a dark rounded square."""
from pathlib import Path
from PIL import Image, ImageDraw

S = 1024  # draw large, downsample for smooth edges
BG, RED = (20, 21, 24, 255), (229, 50, 45, 255)

img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
d = ImageDraw.Draw(img)
d.rounded_rectangle((32, 32, S - 32, S - 32), radius=220, fill=BG)
x0, x1, y0, y1, w = 270, 754, 250, 774, 150
d.rectangle((x0, y0, x0 + w, y1), fill=RED)            # left stem
d.rectangle((x1 - w, y0, x1, y1), fill=RED)            # right stem
d.polygon([(x0, y0), (x0 + w, y0), (x1, y1), (x1 - w, y1)], fill=RED)  # diagonal

out = Path(__file__).with_name("icon.ico")
img.save(out, sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (256, 256)])
print(out, out.stat().st_size, "bytes")
