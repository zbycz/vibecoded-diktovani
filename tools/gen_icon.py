#!/usr/bin/env python3
"""Render the Diktovani microphone icon with a vertical ABC on the mic body.

The drawing lives in a 1024-unit design space measured pixel by pixel off the
original icon_1024x1024.png, so every output size is the same artwork, only
resampled. Two body variants are available:

  small - the shipped mic, untouched, with ABC squeezed into its narrow body
  big   - a taller, wider body (and a shorter stand) so the letters get room

and two styles:

  outline  - hollow body with solid letters inside, like the current app icon
  knockout - solid body with the letters cut out, like SF Symbol mic.fill

Typical use:

    tools/gen_icon.py --variant big --style outline           # app icon set
    tools/gen_icon.py --out assets/menubar --sizes 36 \
        --content 0.88 --gamma 0.75 --name icon_{s}x{s}.png   # menu bar icon
"""

import argparse
import os

from PIL import Image, ImageDraw, ImageFont

DESIGN = 1024.0
SS = 8  # supersampling factor

FONT_CANDIDATES = [
    "/usr/share/fonts/truetype/liberation/LiberationSans-Bold.ttf",
    "/System/Library/Fonts/Supplemental/Arial Bold.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
]


class Geometry:
    """One microphone drawing, in 1024-unit design coordinates."""

    def __init__(
        self,
        stroke,
        body_x0,
        body_x1,
        body_y0,
        body_y1,
        cradle_cy,
        cradle_r,
        prong_y0,
        stem_y1,
        base_y,
        base_half,
        letter_gap_ratio,
        letter_pad,
        letter_aspect,
    ):
        self.stroke = stroke
        self.body_x0, self.body_x1 = body_x0, body_x1
        self.body_y0, self.body_y1 = body_y0, body_y1
        self.cradle_cy, self.cradle_r = cradle_cy, cradle_r
        self.prong_y0 = prong_y0
        self.stem_y1 = stem_y1
        self.base_y, self.base_half = base_y, base_half
        self.letter_gap_ratio = letter_gap_ratio
        self.letter_pad = letter_pad
        self.letter_aspect = letter_aspect

    @property
    def cx(self):
        return (self.body_x0 + self.body_x1) / 2.0

    @property
    def cavity(self):
        """Inner edge of the body - the area the letters may use."""
        h = self.stroke / 2.0
        return self.body_x0 + h, self.body_y0 + h, self.body_x1 - h, self.body_y1 - h

    @property
    def bounds(self):
        """Ink bounding box, so every output can be framed the same way."""
        h = self.stroke / 2.0
        x0 = min(self.body_x0, self.cx - self.cradle_r - h, self.cx - self.base_half - h)
        x1 = max(self.body_x1, self.cx + self.cradle_r + h, self.cx + self.base_half + h)
        return x0, self.body_y0, x1, self.base_y + h


# The shipped 1.3.1 icon, measured off icon_1024x1024.png.
SMALL = Geometry(
    stroke=26.0,
    body_x0=398.0,
    body_x1=624.0,
    body_y0=139.0,
    body_y1=624.0,
    cradle_cy=512.0,
    cradle_r=221.5,
    prong_y0=452.0,
    stem_y1=872.0,
    base_y=872.0,
    base_half=160.0,
    letter_gap_ratio=0.18,
    letter_pad=22.0,
    letter_aspect=0.86,
)

# Body stretched from 226x485 to 352x620 and the stand pulled in, which buys the
# letters roughly 45 % more cap height at the same overall icon size.
BIG = Geometry(
    stroke=30.0,
    body_x0=336.0,
    body_x1=688.0,
    body_y0=40.0,
    body_y1=660.0,
    cradle_cy=606.0,
    cradle_r=286.0,
    prong_y0=548.0,
    stem_y1=984.0,
    base_y=984.0,
    base_half=196.0,
    letter_gap_ratio=0.15,
    letter_pad=26.0,
    letter_aspect=0.86,
)

VARIANTS = {"small": SMALL, "big": BIG}

# Fraction of the canvas height the drawing covers. 0.73 reproduces the framing
# of the original app icon; the menu bar wants a tighter crop.
APP_CONTENT = 0.73
MENUBAR_CONTENT = 0.88


def load_font(px):
    for path in FONT_CANDIDATES:
        if os.path.exists(path):
            return ImageFont.truetype(path, max(px, 1))
    raise SystemExit("no usable sans-serif font found")


def letter_mask(ch, box_w, box_h):
    """`ch` rendered solid and scaled so its ink exactly fills box_w x box_h."""
    font = load_font(400)
    l, t, r, b = font.getbbox(ch)
    canvas = Image.new("L", (r - l + 8, b - t + 8), 0)
    ImageDraw.Draw(canvas).text((4 - l, 4 - t), ch, font=font, fill=255)
    ink = canvas.crop(canvas.getbbox())
    return ink.resize((max(round(box_w), 1), max(round(box_h), 1)), Image.LANCZOS)


class Frame:
    """Maps design coordinates onto the supersampled output canvas."""

    def __init__(self, g, px, content):
        x0, y0, x1, y1 = g.bounds
        self.k = content * px / (y1 - y0)
        self.ox = px / 2.0 - (x0 + x1) / 2.0 * self.k
        self.oy = px / 2.0 - (y0 + y1) / 2.0 * self.k

    def x(self, v):
        return v * self.k + self.ox

    def y(self, v):
        return v * self.k + self.oy

    def d(self, v):
        return v * self.k


def draw_stand(draw, g, f):
    """Cradle (two prongs closed by a half circle), stem and base plate."""
    s = f.d(g.stroke)
    w = max(round(s), 1)
    cx, cy, r = f.x(g.cx), f.y(g.cradle_cy), f.d(g.cradle_r)

    # PIL strokes an arc inward from its bounding box, so grow the box by half a
    # stroke to centre the band on r. Angles run clockwise from 3 o'clock.
    ro = r + s / 2
    draw.arc((cx - ro, cy - ro, cx + ro, cy + ro), start=0, end=180, fill=255, width=w)

    for sign in (-1, 1):
        px = cx + sign * r
        draw.line((px, f.y(g.prong_y0), px, cy), fill=255, width=w)
        cap(draw, px, f.y(g.prong_y0), s)

    draw.line((cx, cy + r, cx, f.y(g.stem_y1)), fill=255, width=w)

    by = f.y(g.base_y)
    half = f.d(g.base_half)
    draw.line((cx - half, by, cx + half, by), fill=255, width=w)
    for sign in (-1, 1):
        cap(draw, cx + sign * half, by, s)


def cap(draw, x, y, stroke):
    draw.ellipse((x - stroke / 2, y - stroke / 2, x + stroke / 2, y + stroke / 2), fill=255)


def place_letters(mask, g, f, knockout, text):
    """Stack `text` down the middle of the body cavity."""
    x0, y0, x1, y1 = (f.x(g.cavity[0]), f.y(g.cavity[1]), f.x(g.cavity[2]), f.y(g.cavity[3]))
    pad = f.d(g.letter_pad)
    inner_w = (x1 - x0) - 2 * pad
    inner_h = (y1 - y0) - 2 * pad
    n = len(text)
    gap = inner_h * g.letter_gap_ratio / (n - 1) if n > 1 else 0.0
    lh = (inner_h - gap * (n - 1)) / n
    lw = min(inner_w, lh * g.letter_aspect)
    cx = (x0 + x1) / 2.0

    for i, ch in enumerate(text):
        glyph = letter_mask(ch, lw, lh)
        pos = (round(cx - glyph.size[0] / 2.0), round(y0 + pad + i * (lh + gap)))
        mask.paste(Image.new("L", glyph.size, 0 if knockout else 255), pos, glyph)


def render(size, variant="big", style="outline", text="ABC", content=APP_CONTENT, gamma=1.0):
    g = VARIANTS[variant]
    px = size * SS
    f = Frame(g, px, content)
    mask = Image.new("L", (px, px), 0)
    draw = ImageDraw.Draw(mask)

    x0, y0 = f.x(g.body_x0), f.y(g.body_y0)
    x1, y1 = f.x(g.body_x1), f.y(g.body_y1)
    if style == "knockout":
        draw.rounded_rectangle((x0, y0, x1, y1), radius=(x1 - x0) / 2.0, fill=255)
    else:
        draw.rounded_rectangle(
            (x0, y0, x1, y1),
            radius=(x1 - x0) / 2.0,
            outline=255,
            width=max(round(f.d(g.stroke)), 1),
        )

    draw_stand(draw, g, f)
    if text:
        place_letters(mask, g, f, style == "knockout", text)

    mask = mask.resize((size, size), Image.LANCZOS)
    if gamma != 1.0:
        # Hairlines land on fractional pixels at menu-bar sizes and wash out to
        # grey; a sub-1 gamma pulls that coverage back towards solid ink.
        mask = mask.point(lambda v: min(255, round(255 * (v / 255.0) ** gamma)))
    out = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    out.putalpha(mask)
    return out


# Below 128 px the hairlines cover less than a whole pixel; darken them so the
# small Finder and menu-bar renditions keep their contrast.
def gamma_for(size):
    return 1.0 if size >= 128 else 0.8


# At 16 px three stacked letters are ~3 px tall and read as damage, so the
# smallest rendition drops them and keeps the plain microphone silhouette.
MIN_TEXT_SIZE = 32


PRESETS = {
    "app": dict(
        out="assets/AppIcon.appiconset",
        sizes=[1024, 512, 256, 128, 64, 32, 16],
        content=APP_CONTENT,
        gamma=None,
    ),
    "menubar": dict(
        out="assets/menubar",
        sizes=[36],
        content=MENUBAR_CONTENT,
        gamma=0.75,
    ),
}


def main():
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument("--preset", default="app", choices=sorted(PRESETS))
    ap.add_argument("--variant", default="big", choices=sorted(VARIANTS))
    ap.add_argument("--style", default="outline", choices=["outline", "knockout"])
    ap.add_argument("--text", default="ABC")
    ap.add_argument("--sizes", help="comma separated, overrides the preset")
    ap.add_argument("--content", type=float, help="drawing height as a fraction of the canvas")
    ap.add_argument("--gamma", type=float)
    ap.add_argument("--out", help="output directory, overrides the preset")
    ap.add_argument("--name", default="icon_{s}x{s}.png")
    args = ap.parse_args()

    preset = PRESETS[args.preset]
    out = args.out or preset["out"]
    sizes = [int(v) for v in args.sizes.split(",")] if args.sizes else preset["sizes"]
    content = args.content if args.content is not None else preset["content"]

    os.makedirs(out, exist_ok=True)
    for size in sizes:
        gamma = args.gamma if args.gamma is not None else preset["gamma"]
        if gamma is None:
            gamma = gamma_for(size)
        text = args.text if size >= MIN_TEXT_SIZE else ""
        img = render(size, args.variant, args.style, text, content, gamma)
        path = os.path.join(out, args.name.format(s=size))
        img.save(path)
        print(f"{path}  {size}x{size}  content={content}  gamma={gamma}")


if __name__ == "__main__":
    main()
