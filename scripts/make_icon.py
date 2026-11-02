"""Generate the small blue magnifier icon at Windows icon sizes."""
from pathlib import Path
from PIL import Image, ImageDraw

base = Path(__file__).resolve().parents[1] / "assets"
scale = 16
canvas = Image.new("RGBA", (64 * scale, 64 * scale))
draw = ImageDraw.Draw(canvas)

def box(values):
    return tuple(round(v * scale) for v in values)

blue = "#2188ff"
draw.line(box((40, 40, 56, 56)), fill=blue, width=11 * scale)
for cx, cy in ((40, 40), (56, 56)):
    draw.ellipse(box((cx - 5.5, cy - 5.5, cx + 5.5, cy + 5.5)), fill=blue)
draw.ellipse(box((6, 6, 46, 46)), fill=blue)
draw.ellipse(box((12.5, 12.5, 39.5, 39.5)), fill="#bfe4ff")
draw.arc(box((17, 17, 35, 35)), start=185, end=268, fill="white", width=round(3.5 * scale))
preview = canvas.resize((256, 256), Image.Resampling.LANCZOS)
preview.save(base / "quickfind.png")
preview.save(base / "quickfind.ico", sizes=[(n, n) for n in (16, 20, 24, 32, 48, 64, 128, 256)])
