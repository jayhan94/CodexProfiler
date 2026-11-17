"""Export CodexProfiler's vector mark and matching application icons (requires Pillow)."""

from pathlib import Path

from PIL import Image, ImageDraw

INK = "#363131"
YELLOW = "#F2CF46"
# The same geometry produces the editable SVG and all raster sizes.
SHAPES = [
    (8, 8, 240, 240, 32, YELLOW, INK, 8),
    (48, 48, 160, 160, 36, INK, None, 0),
    (76, 76, 132, 104, 12, YELLOW, None, 0),
    (172, 76, 44, 104, 0, YELLOW, None, 0),
    (100, 128, 20, 36, 4, "#F2D2DD", INK, 6),
    (132, 108, 20, 56, 4, "#97D4FF", INK, 6),
    (164, 88, 20, 76, 4, "#FFFFFF", INK, 6),
]


def vector():
    rows = [
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 256" role="img" aria-labelledby="title">',
        '<title id="title">CodexProfiler — C and performance bars</title>',
    ]
    for x, y, width, height, radius, fill, stroke, weight in SHAPES:
        # SVG strokes are centered; the raster export follows these same outer bounds.
        edge = f' stroke="{stroke}" stroke-width="{weight}"' if stroke else ""
        rows.append(
            f'<rect x="{x}" y="{y}" width="{width}" height="{height}" rx="{radius}" fill="{fill}"{edge}/>'
        )
    return "\n".join(rows + ["</svg>\n"])


def raster(size):
    scale = size * 4 / 256
    canvas = Image.new("RGBA", (size * 4, size * 4))
    draw = ImageDraw.Draw(canvas)
    for x, y, width, height, radius, fill, stroke, weight in SHAPES:
        margin = weight / 2
        bounds = [
            (x - margin) * scale,
            (y - margin) * scale,
            (x + width + margin) * scale,
            (y + height + margin) * scale,
        ]
        draw.rounded_rectangle(
            bounds,
            radius=(radius + margin) * scale,
            fill=fill,
            outline=stroke,
            width=round(weight * scale),
        )
    return canvas.resize((size, size), Image.Resampling.LANCZOS)


def main():
    destination = Path(__file__).resolve().parents[1] / "assets" / "branding"
    destination.mkdir(parents=True, exist_ok=True)
    (destination / "logo.svg").write_text(vector(), encoding="utf-8")
    raster(256).save(destination / "app.png")
    raster(1024).save(destination / "preview.png")
    raster(256).save(
        destination / "app.ico",
        sizes=[(n, n) for n in [16, 20, 24, 32, 48, 64, 128, 256]],
    )
    print(destination)


if __name__ == "__main__":
    main()
