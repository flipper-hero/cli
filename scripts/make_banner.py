#!/usr/bin/env python3
"""Generates docs/banner.png: a terminal window with real flipper CLI output."""

from PIL import Image, ImageDraw, ImageFont

W, H = 1600, 700
BG = (13, 17, 23)          # github dark
TERM_BG = (22, 27, 34)
BORDER = (48, 54, 61)
FG = (201, 209, 217)
DIM = (110, 118, 129)
GREEN = (63, 185, 80)
YELLOW = (210, 153, 34)
BLUE = (88, 166, 255)
DOLPHIN = (63, 185, 80)

FONT_PATH = "/System/Library/Fonts/Menlo.ttc"
font = ImageFont.truetype(FONT_PATH, 22)
font_small = ImageFont.truetype(FONT_PATH, 17)
font_title = ImageFont.truetype(FONT_PATH, 26)

img = Image.new("RGB", (W, H), BG)
d = ImageDraw.Draw(img)

# terminal window
tx, ty, tw, th = 60, 50, 1480, 600
d.rounded_rectangle([tx, ty, tx + tw, ty + th], radius=14, fill=TERM_BG, outline=BORDER, width=2)

# traffic lights
for i, color in enumerate([(255, 95, 86), (255, 189, 46), (39, 201, 63)]):
    cx = tx + 30 + i * 34
    cy = ty + 30
    d.ellipse([cx - 9, cy - 9, cx + 9, cy + 9], fill=color)

d.text((tx + 140, ty + 16), "flipper — zsh — 80×24", font=font_title, fill=DIM)
d.line([tx, ty + 58, tx + tw, ty + 58], fill=BORDER, width=2)

x0, y0 = tx + 36, ty + 92
lh = 30

lines = [
    ([(FG, "$ "), (BLUE, "flipper"), (FG, " --transport usb info")], None),
    ([(FG, "hardware_name                Flipper Zero")], None),
    ([(FG, "firmware_version             mntm-dev")], None),
    ([(FG, "charge_level                 100")], None),
    ([(FG, "charge_state                 charged")], None),
    ([(FG, "storage free/total           15209398272/15519940608")], None),
    ([], None),
    ([(FG, "$ "), (BLUE, "flipper"), (FG, " tx /ext/infrared/Samsung.ir --button POWER")], None),
    ([(GREEN, "sent /ext/infrared/Samsung.ir")], None),
    ([], None),
    ([(FG, "$ "), (BLUE, "flipper"), (FG, " --json battery")], None),
    ([(DIM, '{ "ok": true, "data": { "level": "100", "state": "charged", "health": "100" } }')], None),
    ([], None),
    ([(FG, "$ "), (BLUE, "flipper"), (FG, " press ok"), (DIM, "   # the screen answers: capture with"), (FG, " screen --ascii")], None),
    ([(GREEN, "pressed Ok")], None),
]

y = y0
for parts, _ in lines:
    x = x0
    for color, text in parts:
        d.text((x, y), text, font=font, fill=color)
        x += int(d.textlength(text, font=font))
    y += lh

d.text((x0, y + 4), "█", font=font, fill=FG)

img.save("docs/banner.png")
print("wrote docs/banner.png", img.size)
