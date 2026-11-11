"""Build the synthetic benchmark corpus for Sleuth's reader.

    python bench/make_corpus.py bench/corpus

Every sheet is invented (no client data) and its tags are known, so
`truth.json` lists exactly what a reader must find. The corpus covers the
page classes the reader distinguishes:

- scanned isometrics and a P&ID (images only, slightly rotated and noisy)
- a faded, blurred site scan (low contrast)
- plotted text documents (line list, valve schedule, datasheet)
- hybrid sheets: a plotted drawing with a scanned site mark-up pasted on it
- a plotted drawing with a small logo (should need no OCR)
- two revisions of one plotted drawing, for revision comparison

Real drawings belong in a separate, private corpus with hand-checked tags; see
bench/README.md.
"""
import io
import json
import math
import os
import random
import sys

from PIL import Image, ImageDraw, ImageFilter, ImageFont
from reportlab.lib.pagesizes import A3, A4, landscape
from reportlab.lib.utils import ImageReader
from reportlab.pdfgen import canvas

OUT = sys.argv[1] if len(sys.argv) > 1 else os.path.join(os.path.dirname(__file__), "corpus")
os.makedirs(OUT, exist_ok=True)
FONTS = "C:/Windows/Fonts/"
TRUTH = {}


def font(name, size):
    return ImageFont.truetype(FONTS + name, size)


DPI = 200
W, H = int(16.54 * DPI), int(11.69 * DPI)  # A3 landscape


def iso(p):
    x, y, z = p
    return (x - y) * math.cos(math.radians(30)), (x + y) * math.sin(math.radians(30)) - z


def sheet(title, dwg, rev, notes):
    im = Image.new("L", (W, H), 255)
    d = ImageDraw.Draw(im)
    d.rectangle([40, 40, W - 40, H - 40], outline=0, width=5)
    tb = [W - 1180, H - 420, W - 40, H - 40]
    d.rectangle(tb, outline=0, width=4)
    for y in (H - 330, H - 240, H - 150):
        d.line([tb[0], y, tb[2], y], fill=0, width=2)
    d.line([W - 600, H - 240, W - 600, H - 40], fill=0, width=2)
    t, s = font("arialbd.ttf", 34), font("arial.ttf", 26)
    d.text((tb[0] + 20, H - 405), "SAMPLE ENGINEERING PROJECT", font=t, fill=0)
    d.text((tb[0] + 20, H - 315), "DRG. TITLE: " + title, font=s, fill=0)
    d.text((tb[0] + 20, H - 225), "DRAWING No.: " + dwg, font=s, fill=0)
    d.text((W - 580, H - 225), "REV. " + rev, font=s, fill=0)
    d.text((tb[0] + 20, H - 135), "SCALE: NTS", font=s, fill=0)
    d.text((W - 580, H - 135), "SHEET 1 OF 1", font=s, fill=0)
    d.text((80, H - 420), "NOTES:", font=font("arialbd.ttf", 24), fill=0)
    for i, line in enumerate(notes):
        d.text((80, H - 385 + i * 34), f"{i + 1}. {line}", font=font("arial.ttf", 22), fill=0)
    v = Image.new("L", (900, 40), 255)
    ImageDraw.Draw(v).text((0, 4), "FORM SAMPLE_ISO.DGN (REV.1)", font=font("arial.ttf", 24), fill=0)
    im.paste(v.rotate(90, expand=True), (W - 90, H - 1360))
    return im, d


def pipe_run(d, pts, ox, oy, k, width=7):
    xy = [(ox + iso(p)[0] * k, oy + iso(p)[1] * k) for p in pts]
    d.line(xy, fill=0, width=width, joint="curve")
    for x, y in xy[1:-1]:
        d.ellipse([x - 9, y - 9, x + 9, y + 9], outline=0, width=4)
    return xy


def label(d, xy, text, size=30, box=False):
    f = font("arial.ttf", size)
    d.text(xy, text, font=f, fill=0)
    if box:
        b = d.textbbox(xy, text, font=f)
        d.rectangle([b[0] - 8, b[1] - 6, b[2] + 8, b[3] + 6], outline=0, width=3)


def scanify(im, angle=0.35, blur=0.6, noise=8):
    im = im.rotate(angle, fillcolor=255, resample=Image.BICUBIC)
    im = im.filter(ImageFilter.GaussianBlur(blur))
    px = im.load()
    rnd = random.Random(7)
    for _ in range(W * H // 900):
        x, y = rnd.randrange(W), rnd.randrange(H)
        px[x, y] = max(0, px[x, y] - rnd.randrange(noise, 90))
    return im


def isometric(name, line, line2, extra, notes, angle):
    im, d = sheet("PIPING ISOMETRIC & BOM", name, "A", notes)
    ox, oy, k = 1050, 620, 1.0
    xy = pipe_run(d, [(0, 0, 0), (900, 0, 0), (900, 0, -380), (900, 700, -380), (1500, 700, -380)], ox, oy, k)
    pipe_run(d, [(900, 700, -380), (900, 700, -760)], ox, oy, k, 6)
    label(d, (xy[0][0] - 40, xy[0][1] - 120), "CONT. ON", 26)
    label(d, (xy[0][0] - 40, xy[0][1] - 80), "LINE " + line, 30)
    label(d, (xy[2][0] + 30, xy[2][1] - 20), "EL +104250", 26)
    label(d, (xy[1][0] + 40, xy[1][1] - 60), extra, 30, box=True)
    label(d, (xy[3][0] + 30, xy[3][1] - 40), line2, 28)
    label(d, (xy[4][0] - 60, xy[4][1] + 40), "E 51200  N 33900", 24)
    bx, by = W - 1180, 120
    d.rectangle([bx, by, W - 60, by + 520], outline=0, width=3)
    rows = [("PT", "DESCRIPTION", "QTY"), ("1", 'PIPE SMLS, ASTM A106 GR.B, 6" SCH-40', "12.4 M"),
            ("2", "ELBOW 90 DEG LR BW, ASTM A234 WPB", "4"), ("3", "GATE VALVE CL150 RF, " + extra, "1"),
            ("4", "GASKET SPIRAL WOUND CL150 RF", "2"), ("5", "STUD BOLT A193 B7 / A194 2H", "16")]
    for i, (a, b, c) in enumerate(rows):
        y = by + 20 + i * 80
        f = font("arialbd.ttf" if i == 0 else "arial.ttf", 24)
        d.text((bx + 20, y), a, font=f, fill=0)
        d.text((bx + 100, y), b, font=f, fill=0)
        d.text((W - 220, y), c, font=f, fill=0)
    return scanify(im, angle)


def pid():
    im, d = sheet("PIPING & INSTRUMENTATION DIAGRAM", "PID-0101", "C", ["ALL INSTRUMENT TAGS PER PROJECT STANDARD.", "RELIEF VALVES DISCHARGE TO FLARE HEADER."])
    d.rounded_rectangle([700, 500, 1000, 1300], radius=150, outline=0, width=6)
    label(d, (720, 1330), "V-101 SEPARATOR", 28)
    d.line([850, 500, 850, 330, 1490, 330], fill=0, width=6)

    def bubble(x, y, a, b):
        d.ellipse([x - 70, y - 70, x + 70, y + 70], outline=0, width=4)
        d.line([x - 70, y, x + 70, y], fill=0, width=3)
        f = font("arialbd.ttf", 30)
        d.text((x - d.textlength(a, font=f) / 2, y - 48), a, font=f, fill=0)
        d.text((x - d.textlength(b, font=f) / 2, y + 8), b, font=f, fill=0)

    bubble(1560, 330, "PSV", "1203"); label(d, (1480, 430), "PSV-1203", 28)
    d.line([1000, 900, 1700, 900], fill=0, width=6); bubble(1350, 760, "FV", "2201"); label(d, (1270, 950), "FV-2201", 28)
    bubble(600, 700, "TT", "3105"); d.line([670, 700, 700, 700], fill=0, width=3)
    label(d, (1040, 860), '6"-P-1001-A1B', 28); label(d, (1720, 880), "TO E-102", 26)
    return scanify(im, -0.25)


def faint_scan():
    im = isometric("ISO-1004-01", '3"-P-1004-A1B', '2"-D-1005-B2A', "HV-4410", ["FIELD ROUTED SMALL BORE.", "REFER PSV-1203 DISCHARGE."], 0.8)
    return Image.eval(im.filter(ImageFilter.GaussianBlur(1.3)), lambda v: 120 + v * 135 // 255)


SCANS = {}


def save_img_pdf(im, name, tags):
    im.convert("RGB").save(os.path.join(OUT, name), "PDF", resolution=DPI)
    TRUTH[name] = tags
    SCANS[name] = (im, tags)


save_img_pdf(isometric("ISO-1001-01", '6"-P-1001-A1B', '4"-P-1002-A1B', "PSV-1203", ["ALL DIMENSIONS IN MM UNLESS STATED OTHERWISE.", "FOR LINE DATA REFER LINE LIST DOC-1001-17.", "HYDROTEST AS PER SPEC TS-400."], 0.35),
             "ISO-1001-01.pdf", ['6"-P-1001-A1B', '4"-P-1002-A1B', "PSV-1203", "ISO-1001-01", "DOC-1001-17"])
save_img_pdf(isometric("ISO-1001-02", '6"-P-1001-A1B', '6"-CW-2014-B2A', "FV-2201", ["CONTINUATION OF ISO-1001-01.", "SUPPORTS PER STD DWG SD-09."], -0.4),
             "ISO-1001-02.pdf", ['6"-P-1001-A1B', '6"-CW-2014-B2A', "FV-2201", "ISO-1001-02", "ISO-1001-01"])
save_img_pdf(pid(), "PID-0101 rev C.pdf", ["PSV-1203", "FV-2201", '6"-P-1001-A1B', "V-101", "PID-0101"])
save_img_pdf(faint_scan(), "Scan_0042.pdf", ['3"-P-1004-A1B', '2"-D-1005-B2A', "HV-4410", "PSV-1203", "ISO-1004-01"])


def text_pdf(name, title, header, rows, tags):
    c = canvas.Canvas(os.path.join(OUT, name), pagesize=landscape(A4))
    w, h = landscape(A4)
    c.setFont("Helvetica-Bold", 16); c.drawString(40, h - 50, title)
    c.setFont("Helvetica", 9); c.drawString(40, h - 66, "SAMPLE ENGINEERING PROJECT   DOC-1001-17   REV 3")
    y = h - 100
    c.setFont("Helvetica-Bold", 9)
    for i, t in enumerate(header):
        c.drawString(40 + i * 120, y, t)
    c.setFont("Helvetica", 9)
    for r in rows:
        y -= 18
        for i, t in enumerate(r):
            c.drawString(40 + i * 120, y, t)
    c.save()
    TRUTH[name] = tags


text_pdf("Line list rev 3.pdf", "LINE LIST", ["LINE No.", "FROM", "TO", "FLUID", "DESIGN P (barg)", "DESIGN T (C)"],
         [['6"-P-1001-A1B', "V-101", "E-102", "CRUDE", "19.6", "120"], ['4"-P-1002-A1B', "E-102", "P-103", "CRUDE", "19.6", "120"],
          ['6"-CW-2014-B2A', "CW HDR", "E-102", "COOLING WATER", "8.0", "65"], ['3"-P-1004-A1B', "P-103", "T-104", "CRUDE", "24.0", "120"],
          ['2"-D-1005-B2A', "T-104", "DRAIN", "OILY WATER", "3.5", "80"]],
         ['6"-P-1001-A1B', '4"-P-1002-A1B', '6"-CW-2014-B2A', '3"-P-1004-A1B', '2"-D-1005-B2A', "DOC-1001-17"])
text_pdf("Valve schedule.pdf", "VALVE SCHEDULE", ["TAG", "TYPE", "SIZE", "LINE", "RATING", "ACTUATOR"],
         [["FV-2201", "GLOBE", '6"', '6"-CW-2014-B2A', "CL150", "PNEUMATIC"], ["HV-4410", "BALL", '2"', '2"-D-1005-B2A', "CL150", "MANUAL"],
          ["XV-1101", "BALL", '6"', '6"-P-1001-A1B', "CL150", "PNEUMATIC"]],
         ["FV-2201", "HV-4410", "XV-1101", '6"-CW-2014-B2A'])


def markup(lines, size=(900, 420)):
    """A scanned site mark-up: red-pen style text on off-white paper."""
    im = Image.new("RGB", size, (246, 243, 232))
    d = ImageDraw.Draw(im)
    d.rectangle([4, 4, size[0] - 5, size[1] - 5], outline=(170, 30, 30), width=4)
    for i, t in enumerate(lines):
        d.text((24, 24 + i * 64), t, font=font("arialbd.ttf", 40), fill=(170, 30, 30))
    im = im.rotate(-0.6, fillcolor=(246, 243, 232), resample=Image.BICUBIC).filter(ImageFilter.GaussianBlur(0.5))
    buf = io.BytesIO()
    im.save(buf, "PNG")
    buf.seek(0)
    return ImageReader(buf)


def plotted(name, dwg, rev, tags, extra_image=None, logo=False):
    """A plotted (vector) isometric: every printed word is in the text layer."""
    c = canvas.Canvas(os.path.join(OUT, name), pagesize=landscape(A3))
    w, h = landscape(A3)
    c.setLineWidth(2); c.rect(20, 20, w - 40, h - 40)
    c.rect(w - 440, 20, 420, 150)
    c.setFont("Helvetica-Bold", 12); c.drawString(w - 430, 150, "SAMPLE ENGINEERING PROJECT")
    c.setFont("Helvetica", 10)
    c.drawString(w - 430, 120, "DRG. TITLE: PIPING ISOMETRIC")
    c.drawString(w - 430, 90, "DRAWING No.: " + dwg)
    c.drawString(w - 200, 90, "REV. " + rev)
    c.drawString(w - 200, 60, "SHEET 1 OF 1")
    c.setLineWidth(3)
    pts = [(200, 600), (520, 600), (520, 420), (820, 300), (1000, 300)]
    for a, b in zip(pts, pts[1:]):
        c.line(a[0], a[1], b[0], b[1])
    c.setFont("Helvetica", 11)
    for (x, y), t in zip(pts, tags):
        c.drawString(x + 10, y + 12, t)
    if logo:
        c.drawImage(markup(["DDS"], (160, 90)), 40, h - 80, 80, 45)
    if extra_image:
        img, (x, y, iw, ih) = extra_image
        c.drawImage(img, x, y, iw, ih)
    c.save()


# Hybrid: a plotted sheet with a scanned site mark-up carrying two more tags.
plotted("ISO-3001-01 with markup.pdf", "ISO-3001-01", "B", ['8"-P-3001-A1B', "PSV-3101", "FV-3201", "HV-3301", "PT-3401"],
        extra_image=(markup(["FIELD CHANGE:", "ADD TT-3105", "RELOCATE XV-3109"]), (120, 80, 300, 140)))
TRUTH["ISO-3001-01 with markup.pdf"] = ['8"-P-3001-A1B', "PSV-3101", "FV-3201", "HV-3301", "PT-3401", "ISO-3001-01", "TT-3105", "XV-3109"]
# Plotted with only a small logo image: should need no OCR at all.
plotted("ISO-3002-01.pdf", "ISO-3002-01", "A", ['6"-P-3002-A1B', "PSV-3102", "FV-3202", "HV-3302", "PT-3402"], logo=True)
TRUTH["ISO-3002-01.pdf"] = ['6"-P-3002-A1B', "PSV-3102", "FV-3202", "HV-3302", "PT-3402", "ISO-3002-01"]
# Two revisions of one drawing: FV-2301 became FV-2302, PT-5501 removed, LT-6601 added.
plotted("ISO-2001-01 rev A.pdf", "ISO-2001-01", "A", ['6"-P-2001-A1B', "PSV-2203", "FV-2301", "PT-5501", "HV-2401"])
TRUTH["ISO-2001-01 rev A.pdf"] = ['6"-P-2001-A1B', "PSV-2203", "FV-2301", "PT-5501", "HV-2401", "ISO-2001-01"]
plotted("ISO-2001-01 rev B.pdf", "ISO-2001-01", "B", ['6"-P-2001-A1B', "PSV-2203", "FV-2302", "LT-6601", "HV-2401"])
TRUTH["ISO-2001-01 rev B.pdf"] = ['6"-P-2001-A1B', "PSV-2203", "FV-2302", "LT-6601", "HV-2401", "ISO-2001-01"]


def small_print():
    """A scanned A2 general arrangement in small print: tags 1.8, 2.0 and
    2.5 mm high spread over the whole sheet (so some sit on every tile
    border), some vertical, some boxed, some with a leader line touching
    them, and a schedule in 2 mm text. This is the sheet that shows whether a
    faster setting still finds small text."""
    dpi = 300
    w, h = int(23.39 * dpi), int(16.54 * dpi)  # A2 landscape
    im = Image.new("L", (w, h), 255)
    d = ImageDraw.Draw(im)
    d.rectangle([60, 60, w - 60, h - 60], outline=0, width=6)
    rnd = random.Random(11)
    cap = {1.8: 30, 2.0: 33, 2.5: 41}  # Arial size in px for a capital this many mm high
    kinds = ["PSV", "FV", "HV", "TT", "PT", "LT", "XV", "FT", "PI", "TI", "LG", "FE"]
    tags = []
    n = 0
    cols, rows = 10, 7
    cw, rh = (w - 400) // cols, (h - 1100) // rows
    for r in range(rows):
        for c in range(cols):
            n += 1
            mm = [1.8, 2.0, 2.5][n % 3]
            f = font("arial.ttf", cap[mm])
            if n % 5 == 0:
                tag = f'{rnd.choice([2, 3, 4, 6, 8])}"-{rnd.choice(["P", "CW", "D", "FG"])}-{5000 + n}-{rnd.choice(["A1B", "B2A", "C3D"])}'
            else:
                tag = f"{kinds[n % len(kinds)]}-{5000 + n}"
            tags.append(tag)
            x = 200 + c * cw + rnd.randrange(0, cw - 420)
            y = 200 + r * rh + rnd.randrange(0, rh - 120)
            if n % 7 == 0:
                # vertical, reading from the right-hand side of the sheet
                tw = int(d.textlength(tag, font=f)) + 8
                v = Image.new("L", (tw, cap[mm] + 12), 255)
                ImageDraw.Draw(v).text((4, 2), tag, font=f, fill=0)
                im.paste(v.rotate(90, expand=True), (x, y - 60))
                continue
            d.text((x, y), tag, font=f, fill=0)
            b = d.textbbox((x, y), tag, font=f)
            if n % 4 == 0:
                d.rectangle([b[0] - 9, b[1] - 8, b[2] + 9, b[3] + 8], outline=0, width=3)
            elif n % 4 == 1:
                # leader line ending just under the tag, and an equipment outline near it
                d.line([b[0] - 6, b[3] + 7, b[2] + 30, b[3] + 7, b[2] + 150, b[3] + 90], fill=0, width=3)
                d.ellipse([b[2] + 130, b[3] + 70, b[2] + 190, b[3] + 130], outline=0, width=3)
            elif n % 4 == 2:
                d.line([b[0] - 60, (b[1] + b[3]) // 2, b[0] - 10, (b[1] + b[3]) // 2], fill=0, width=5)
    # Schedule in 2 mm text along the bottom.
    f = font("arial.ttf", cap[2.0])
    sx, sy = 200, h - 820
    d.rectangle([sx, sy, sx + 3300, sy + 700], outline=0, width=4)
    for i in range(12):
        n += 1
        tag = f"NZ-{6100 + i}"
        tags.append(tag)
        y = sy + 14 + i * 57
        d.line([sx, y + 46, sx + 3300, y + 46], fill=0, width=2)
        d.text((sx + 20, y), tag, font=f, fill=0)
        d.text((sx + 420, y), rnd.choice(['2" CL150 RF', '4" CL300 RF', '6" CL150 RF', '1" NPT']), font=f, fill=0)
        d.text((sx + 1100, y), rnd.choice(["INLET", "OUTLET", "VENT", "DRAIN", "LEVEL GAUGE", "MANHOLE"]), font=f, fill=0)
        d.text((sx + 1900, y), f"EL +{rnd.randrange(100000, 110000)}", font=f, fill=0)
    d.text((w - 2300, h - 400), "DRAWING No.: GA-5001-01   REV. B   GENERAL ARRANGEMENT", font=font("arial.ttf", 44), fill=0)
    tags.append("GA-5001-01")
    im = im.rotate(0.3, fillcolor=255, resample=Image.BICUBIC).filter(ImageFilter.GaussianBlur(0.8))
    px = im.load()
    for _ in range(w * h // 900):
        x, y = rnd.randrange(w), rnd.randrange(h)
        px[x, y] = max(0, px[x, y] - rnd.randrange(8, 90))
    im.convert("RGB").save(os.path.join(OUT, "GA-5001-01 small print.pdf"), "PDF", resolution=dpi)
    TRUTH["GA-5001-01 small print.pdf"] = tags


small_print()


def image_files():
    """The same scans as loose image files, for the reader's image support:
    a PNG, a JPEG, a BMP and a three-page Group 4 (fax) TIFF, in `images/`
    with their own `truth.json`."""
    d = os.path.join(OUT, "images")
    os.makedirs(d, exist_ok=True)
    truth = {}
    names = ["ISO-1001-01.pdf", "ISO-1001-02.pdf", "PID-0101 rev C.pdf"]
    im, tags = SCANS[names[0]]
    im.convert("RGB").save(os.path.join(d, "ISO-1001-01.png"))
    truth["ISO-1001-01.png"] = tags
    im, tags = SCANS[names[1]]
    im.save(os.path.join(d, "ISO-1001-02.jpg"), quality=85, dpi=(DPI, DPI))
    truth["ISO-1001-02.jpg"] = tags
    im, tags = SCANS[names[2]]
    im.convert("RGB").save(os.path.join(d, "PID-0101 rev C.bmp"))
    truth["PID-0101 rev C.bmp"] = tags
    frames = [SCANS[n][0].point(lambda v: 255 if v > 150 else 0).convert("1") for n in names]
    frames[0].save(os.path.join(d, "Scans 3 pages.tif"), save_all=True, append_images=frames[1:], compression="group4", dpi=(DPI, DPI))
    truth["Scans 3 pages.tif"] = sorted({t for n in names for t in SCANS[n][1]})
    with open(os.path.join(d, "truth.json"), "w", encoding="utf-8") as f:
        json.dump(truth, f, indent=1, ensure_ascii=False)
    print("wrote", len(truth), "image files to", d)


image_files()

with open(os.path.join(OUT, "truth.json"), "w", encoding="utf-8") as f:
    json.dump(TRUTH, f, indent=1, ensure_ascii=False)
print("wrote", len(TRUTH), "files to", OUT)
