#!/usr/bin/env python3
"""Generate the branded icon set of a custom client from its logo sources.

Usage: python3 res/custom/make_assets.py res/custom/<variant>

Reads <variant dir>/logo/mark.png (the square-ish mark, transparent background) and
<variant dir>/logo/lockup.png (mark + wordmark side by side), and writes every image the
desktop builds use under <variant dir>/overlay/, mirroring the repository layout so a build
can copy the overlay over the checkout. Needs Pillow.
"""
import base64
import io
import sys
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter

BLUE_TEXT_MIN_GAP = 40  # a wordmark pixel is "blue" when b exceeds r by this much


def fill_enclosed(mark: Image.Image) -> Image.Image:
    """Paint the transparent areas enclosed by the mark white, so it reads on dark backgrounds."""
    w, h = mark.size
    pad = 4
    solid = Image.new("L", (w + 2 * pad, h + 2 * pad), 0)
    solid.paste(mark.getchannel("A").point(lambda v: 255 if v > 8 else 0), (pad, pad))
    ImageDraw.floodfill(solid, (0, 0), 128)
    inside = solid.point(lambda v: 0 if v == 128 else 255).crop((pad, pad, pad + w, pad + h))
    # Shrink the fill so it stays under the mark's outline instead of fringing past it.
    inside = inside.filter(ImageFilter.MinFilter(5))
    white = Image.new("RGBA", (w, h), (255, 255, 255, 0))
    white.putalpha(inside)
    return Image.alpha_composite(white, mark)


def fit(im: Image.Image, box: int) -> Image.Image:
    s = min(box / im.width, box / im.height)
    return im.resize((round(im.width * s), round(im.height * s)), Image.LANCZOS)


def square(im: Image.Image, size: int, fill: float) -> Image.Image:
    canvas = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    art = fit(im, round(size * fill))
    canvas.alpha_composite(art, ((size - art.width) // 2, (size - art.height) // 2))
    return canvas


def macos_tile(mark: Image.Image, size: int = 1024) -> Image.Image:
    """White rounded tile on Apple's 1024 grid (824 px body), mark at 74% of the body."""
    canvas = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    body = round(size * 824 / 1024)
    off = (size - body) // 2
    ImageDraw.Draw(canvas).rounded_rectangle(
        (off, off, off + body - 1, off + body - 1), radius=round(body * 0.2245), fill="white"
    )
    art = fit(mark, round(body * 0.74))
    canvas.alpha_composite(art, ((size - art.width) // 2, (size - art.height) // 2))
    return canvas


def template(mark: Image.Image, size: int) -> Image.Image:
    """Black silhouette keeping the mark's own transparency, for macOS template images."""
    black = Image.new("RGBA", mark.size, (0, 0, 0, 0))
    black.putalpha(mark.getchannel("A"))
    return square(black, size, 0.96)


def dark_wordmark(lockup: Image.Image, mark_width: int) -> Image.Image:
    out = lockup.copy()
    px = out.load()
    for y in range(out.height):
        for x in range(mark_width, out.width):
            r, g, b, a = px[x, y]
            if a and b > r + BLUE_TEXT_MIN_GAP:
                px[x, y] = (255, 255, 255, a)
    return out


def mark_width_of(lockup: Image.Image) -> int:
    alpha = lockup.getchannel("A")
    for x in range(1, lockup.width):
        prev = any(alpha.getpixel((x - 1, y)) > 16 for y in range(lockup.height))
        cur = any(alpha.getpixel((x, y)) > 16 for y in range(lockup.height))
        if prev and not cur:
            return x
    raise SystemExit("lockup.png: no gap between the mark and the wordmark")


def svg_with_png(png: Image.Image) -> str:
    buf = io.BytesIO()
    png.save(buf, "PNG", optimize=True)
    data = base64.b64encode(buf.getvalue()).decode()
    n = png.width
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{n}" height="{n}" viewBox="0 0 {n} {n}">'
        f'<image width="{n}" height="{n}" href="data:image/png;base64,{data}"/></svg>\n'
    )


def main() -> None:
    variant = Path(sys.argv[1]).resolve()
    mark = Image.open(variant / "logo/mark.png").convert("RGBA")
    mark = mark.crop(mark.getbbox())
    lockup = Image.open(variant / "logo/lockup.png").convert("RGBA")
    lockup = lockup.crop(lockup.getbbox())
    out = variant / "overlay"

    def save(rel: str, im: Image.Image, **kw) -> None:
        path = out / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        im.save(path, **kw)
        print("wrote", rel)

    filled = fill_enclosed(mark)
    bare = square(filled, 1024, 0.88)

    # macOS: app icon and menu-bar template.
    save("flutter/macos/Runner/AppIcon.icns", macos_tile(filled),
         sizes=[(16, 16), (32, 32), (64, 64), (128, 128), (256, 256), (512, 512), (1024, 1024)])
    save("res/mac-tray-dark-x2.png", template(mark, 60))

    # Windows: exe resource icon, installer / packer icon, tray fallback.
    save("flutter/windows/runner/resources/app_icon.ico", bare,
         sizes=[(s, s) for s in (16, 20, 24, 32, 40, 48, 64, 128, 256)], bitmap_format="bmp")
    save("res/icon.ico", bare,
         sizes=[(s, s) for s in (16, 24, 32, 48, 64, 128, 256)], bitmap_format="bmp")
    save("res/tray-icon.ico", square(filled, 256, 0.96),
         sizes=[(s, s) for s in (16, 20, 24, 32, 40, 48, 64)], bitmap_format="bmp")
    # The Sciter client's window icon (title bars of every window), embedded by src/ui.rs.
    save("res/sciter-window-icon.png", bare.resize((128, 128), Image.LANCZOS))

    # Linux (deb): hicolor 256 px and scalable icons; the AppImage takes 32, 64 and 128 px.
    save("res/128x128@2x.png", bare.resize((256, 256), Image.LANCZOS))
    for size in (32, 64, 128):
        save(f"res/{size}x{size}.png", bare.resize((size, size), Image.LANCZOS))
    scalable = out / "res/scalable.svg"
    scalable.write_text(svg_with_png(bare.resize((512, 512), Image.LANCZOS)))
    print("wrote", scalable.relative_to(out))

    # Flutter assets. The tab / connection-manager icon comes from icon.svg (flutter_svg renders
    # data: URI images). No icon.png on purpose: when present the tray loads it instead of the
    # icons above, and macOS would then draw the filled mark as a solid template silhouette.
    # logo_light.png / logo_dark.png are the home-page logo per theme, shown in a 300x60 box.
    icon_svg = out / "flutter/assets/icon.svg"
    icon_svg.parent.mkdir(parents=True, exist_ok=True)
    icon_svg.write_text(svg_with_png(square(filled, 256, 0.96)))
    print("wrote", icon_svg.relative_to(out))
    width = mark_width_of(lockup)
    lock_light = lockup.copy()
    lock_light.paste((0, 0, 0, 0), (0, 0, width, lockup.height))
    lock_light.alpha_composite(fill_enclosed(lockup.crop((0, 0, width, lockup.height))), (0, 0))
    height = 180
    size = (round(lock_light.width * height / lock_light.height), height)
    save("flutter/assets/logo_light.png", lock_light.resize(size, Image.LANCZOS))
    save("flutter/assets/logo_dark.png", dark_wordmark(lock_light, width).resize(size, Image.LANCZOS))


if __name__ == "__main__":
    main()
