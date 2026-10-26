"""Draws the WebM Studio logo and writes icon.png (256px) and icon.ico (16-256px).

Run: python app/assets/make_icon.py
Colors come from app/src/theme.rs.
"""

from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter

S = 1024  # drawn large, then downscaled
HERE = Path(__file__).parent

BG_TOP = (0x38, 0x3A, 0x40)
BG_BOT = (0x1B, 0x1C, 0x1E)
STRIP = (0x14, 0x14, 0x16)
ACCENT = (0x4F, 0xB3, 0xFF)
ACCENT_DARK = (0x2F, 0x6D, 0xB5)
PIN = (0xF2, 0xD2, 0x4B)
WAVE = (0x9F, 0xE0, 0xD2)


def rounded_mask(size, radius):
    m = Image.new("L", (size, size), 0)
    ImageDraw.Draw(m).rounded_rectangle([0, 0, size - 1, size - 1], radius, fill=255)
    return m


def draw():
    img = Image.new("RGBA", (S, S), (0, 0, 0, 0))

    # Background: vertical gradient in a rounded square.
    grad = Image.new("RGBA", (S, S))
    gd = ImageDraw.Draw(grad)
    for y in range(S):
        t = y / (S - 1)
        c = tuple(round(a + (b - a) * t) for a, b in zip(BG_TOP, BG_BOT))
        gd.line([(0, y), (S, y)], fill=c + (255,))
    img.paste(grad, (0, 0), rounded_mask(S, 200))
    d = ImageDraw.Draw(img)

    # Film strip bands top and bottom, with sprocket holes.
    band = 150
    for y0 in (0, S - band):
        band_img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
        bd = ImageDraw.Draw(band_img)
        bd.rectangle([0, y0, S, y0 + band], fill=STRIP + (255,))
        hole_w, hole_h = 70, 56
        hy = y0 + (band - hole_h) // 2
        for i in range(7):
            hx = 92 + i * 130 - hole_w // 2
            bd.rounded_rectangle([hx, hy, hx + hole_w, hy + hole_h], 14, fill=(0x44, 0x46, 0x4C, 255))
        img.alpha_composite(Image.composite(band_img, Image.new("RGBA", (S, S), (0, 0, 0, 0)), rounded_mask(S, 200)))
    d = ImageDraw.Draw(img)

    # Play triangle with a soft glow.
    cx, cy = S // 2 + 30, S // 2
    r = 250
    tri = [(cx - r * 0.8, cy - r), (cx - r * 0.8, cy + r), (cx + r * 1.05, cy)]
    glow = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    ImageDraw.Draw(glow).polygon(tri, fill=ACCENT + (150,))
    img.alpha_composite(glow.filter(ImageFilter.GaussianBlur(40)))

    tri_img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    td = ImageDraw.Draw(tri_img)
    td.polygon(tri, fill=ACCENT + (255,))
    # Lower half a little darker for depth.
    shade = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    ImageDraw.Draw(shade).polygon([(cx - r * 0.8, cy), (cx - r * 0.8, cy + r), (cx + r * 1.05, cy)], fill=ACCENT_DARK + (255,))
    tri_img.alpha_composite(shade)
    img.alpha_composite(tri_img)

    # Small waveform under the triangle, on the bottom strip edge.
    d = ImageDraw.Draw(img)
    heights = [10, 22, 38, 26, 48, 30, 18, 40, 24, 12]
    base = S - band - 40
    x = 300
    for h in heights:
        d.rounded_rectangle([x, base - h, x + 22, base + h], 11, fill=WAVE + (255,))
        x += 44

    # Region pin (yellow marker) at the top-left of the frame area, like the timeline's region bar.
    px, py = 150, band + 50
    d.rounded_rectangle([px, py, px + 26, py + 140], 8, fill=PIN + (255,))
    d.polygon([(px + 26, py), (px + 110, py + 40), (px + 26, py + 80)], fill=PIN + (255,))

    return img


def main():
    img = draw()
    img.resize((256, 256), Image.LANCZOS).save(HERE / "icon.png")
    sizes = [16, 24, 32, 48, 64, 128, 256]
    img.resize((256, 256), Image.LANCZOS).save(HERE / "icon.ico", sizes=[(s, s) for s in sizes])
    img.resize((512, 512), Image.LANCZOS).save(HERE / "logo_512.png")
    print("wrote", HERE / "icon.png", HERE / "icon.ico", HERE / "logo_512.png")


if __name__ == "__main__":
    main()
