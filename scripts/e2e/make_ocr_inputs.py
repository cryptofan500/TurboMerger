"""OCR end-to-end inputs with known words (Pillow + reportlab).

Usage: python3 -I make_ocr_inputs.py OUT_DIR
- photo.png      image only                -> needs OCR
- scan.pdf       one image-only page       -> needs OCR
- mixed.pdf      p1 real text layer, p2 image-only -> p1 kept, only p2 OCR'd
"""
import sys
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont
from reportlab.lib.pagesizes import A4
from reportlab.lib.utils import ImageReader
from reportlab.pdfgen import canvas

out = Path(sys.argv[1])
out.mkdir(parents=True, exist_ok=True)
FONT = ImageFont.truetype("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", 64)


def page(lines):
    img = Image.new("RGB", (1654, 2339), "white")  # A4 at 200 dpi
    d = ImageDraw.Draw(img)
    for k, line in enumerate(lines):
        d.text((140, 220 + 130 * k), line, fill="black", font=FONT)
    return img


page(["MARMALADE ZEPPELIN", "photographed residency letter"]).save(out / "photo.png")
page(["QUARTZ BUFFALO", "scanned page without text"]).save(out / "scan.pdf")

c = canvas.Canvas(str(out / "mixed.pdf"), pagesize=A4)
c.setFont("Helvetica", 20)
c.drawString(72, 760, "TEXTLAYER WALRUS typed page one")
c.showPage()
w, h = A4
c.drawImage(ImageReader(page(["JUNIPER CANYON", "scanned page two"])), 0, 0, w, h)
c.showPage()
c.save()
