"""Makes the wallet app's icons from the circular logo (assets/logo-circle.webp, as supplied by the owner).

    python tools/make_icons.py

Writes:
  assets/tenero.ico             the Windows icon (16 to 256 pixels) for a desktop shortcut or an installer
  assets/tenero-icon-128.rgba   128 x 128 raw RGBA, compiled into the wallet app as its window and taskbar icon

The logo sits on a black square; the icons are cut to its circle (transparent outside it). Needs Pillow (a build-time tool only:
nothing in the programs depends on it). The logo itself is not altered; only cropped, masked and scaled.
"""
import os

from PIL import Image, ImageChops, ImageDraw

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRC = os.path.join(ROOT, "assets", "logo-circle.webp")


def circle_of(img):
    """The square that holds the logo's circle: the box of everything that is not (nearly) black."""
    rgb = img.convert("RGB")
    lum = rgb.convert("L").point(lambda v: 255 if v > 24 else 0)
    box = lum.getbbox()
    left, top, right, bottom = box
    size = max(right - left, bottom - top)
    cx, cy = (left + right) / 2, (top + bottom) / 2
    half = size / 2
    return rgb.crop((round(cx - half), round(cy - half), round(cx + half), round(cy + half)))


def round_icon(square, px):
    big = square.resize((px * 4, px * 4), Image.LANCZOS)
    mask = Image.new("L", big.size, 0)
    ImageDraw.Draw(mask).ellipse((0, 0, big.size[0] - 1, big.size[1] - 1), fill=255)
    out = big.convert("RGBA")
    out.putalpha(ImageChops.multiply(mask, Image.new("L", big.size, 255)))
    return out.resize((px, px), Image.LANCZOS)


def main():
    square = circle_of(Image.open(SRC))
    sizes = [16, 24, 32, 48, 64, 128, 256]
    imgs = [round_icon(square, s) for s in sizes]
    ico = os.path.join(ROOT, "assets", "tenero.ico")
    imgs[-1].save(ico, format="ICO", sizes=[(s, s) for s in sizes], append_images=imgs[:-1])
    raw = os.path.join(ROOT, "assets", "tenero-icon-128.rgba")
    with open(raw, "wb") as f:
        f.write(round_icon(square, 128).tobytes())
    print("cropped square", square.size, "->", os.path.relpath(ico, ROOT), os.path.getsize(ico), "bytes;",
          os.path.relpath(raw, ROOT), os.path.getsize(raw), "bytes")


if __name__ == "__main__":
    main()
