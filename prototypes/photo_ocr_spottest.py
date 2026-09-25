# /// script
# requires-python = ">=3.10"
# dependencies = ["rapidocr==3.9.2", "onnxruntime>=1.20", "pillow>=11", "pillow-heif>=0.20",
#                 "zxing-cpp>=2.2", "numpy>=1.26", "opencv-python-headless>=4.10"]
# ///
"""TurboMerger spot test #2 — photos of printed labels → spatial, LLM-ready Markdown.

Reference prototype for the Rust port (NOT TurboMerger code). Stage → planned component:
  decode HEIC/JPEG/PNG, apply EXIF orientation, drop metadata      → tm-extract::image
  OCR lines: PP-OCRv6 det + rec on ONNX Runtime (CPU)              → tm-ocr worker (ort crate)
  barcodes: ZXing-C++                                              → tm-extract::barcode (rxing)
  paper mask (text outside the sheet) · ink colour (annotations)   → tm-layout
  deskew → repeated-form segmentation (labels that repeat per sticker, relative-phase boundaries)
  label→value pairing · header tables · cross-checks (barcode vs printed digits, sticker vs summary row)
  render: sheet view (box-drawn cards in the photo's arrangement) + structured tables + flags + JSON
Nothing here knows about a particular label vendor: segmentation uses whatever text repeats.
Usage: uv run photo_ocr_spottest.py <photo_dir> <out_dir> [--workers N] [--threads N] [--max-side PX]
       uv run photo_ocr_spottest.py <photo_dir> <out_dir> --bench      (engine / thread / resolution means test)
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import multiprocessing as mp
import os
import re
import statistics as st
import subprocess
import sys
import tempfile
import time
from collections import Counter, defaultdict
from dataclasses import asdict, dataclass, field
from pathlib import Path

IMG_EXT = {".heic", ".heif", ".jpg", ".jpeg", ".png", ".webp", ".tif", ".tiff", ".bmp"}
LOW_CONF = 0.85
PREVIEW_EDGE = 1400

# Strings read by eye from each photo (ground truth for the accuracy check only; never used by the pipeline).
# Kept OUT of the repository: the strings are real customer names and order
# numbers read from private photos. Point TM_PHOTO_TRUTH at a JSON file, or place it at
# fixtures/photos-labels/local/truth.json (gitignored). Without it the accuracy check is skipped.
def _load_truth():
    p = os.environ.get("TM_PHOTO_TRUTH") or str(
        Path(__file__).resolve().parent.parent / "fixtures" / "photos-labels" / "local" / "truth.json")
    try:
        with open(p, encoding="utf-8") as f:
            return json.load(f)
    except FileNotFoundError:
        return {}


TRUTH = _load_truth()


# ----------------------------------------------------------------------------- data
@dataclass
class Item:
    text: str
    conf: float
    quad: list                 # 4 × [x, y] in image pixels (TL, TR, BR, BL)
    kind: str = "text"         # text | barcode
    fmt: str = ""              # barcode format
    x0: float = 0.0            # deskewed axis-aligned box
    y0: float = 0.0
    x1: float = 0.0
    y1: float = 0.0
    ink: str = "print"         # print | colour:<name>
    where: str = "sheet"       # sheet | background
    region: str = ""           # header | sticker | footer
    inst: int = -1
    edge: bool = False         # touches the photo border
    skin: float = 0.0          # share of the box covered by skin tones
    role: str = ""             # label | value | text
    label: str = ""            # canonical field label (for role == label)
    words: list = field(default_factory=list)   # [[word, quad], ...] from the recogniser

    @property
    def h(self):
        return self.y1 - self.y0

    @property
    def w(self):
        return self.x1 - self.x0

    @property
    def cx(self):
        return (self.x0 + self.x1) / 2

    @property
    def cy(self):
        return (self.y0 + self.y1) / 2


def norm(t: str) -> str:
    t = t.upper().strip()
    t = re.sub(r"\s*([/#:])\s*", r"\1", t)
    t = re.sub(r"\s+", " ", t)
    return t.rstrip(":").strip()


def shape(t: str) -> str:
    """Shape signature: 123-456-7 → 999-999-9, A12345 → A99999 (recurring shapes mark the same field)."""
    t = re.sub(r"\d", "9", t.strip())
    t = re.sub(r"[A-Z]", "A", t)
    return re.sub(r"[a-z]", "a", re.sub(r"\s+", " ", t))


def tok_shapes(t: str) -> set:
    return {shape(w) for w in t.split() if len(w) >= 4 and any(c.isdigit() for c in w)}


def label_like(t: str) -> bool:
    letters = sum(c.isalpha() for c in t)
    digits = sum(c.isdigit() for c in t)
    return letters >= 3 and digits <= 0.3 * max(1, len(t))


def med(xs, default=0.0):
    xs = list(xs)
    return st.median(xs) if xs else default


def rotate(items, ang, cx, cy):
    """Set the deskewed axis-aligned box of each item (rotation by -ang around cx, cy)."""
    ca, sa = math.cos(ang), math.sin(ang)
    for it in items:
        xs, ys = [], []
        for x, y in it.quad:
            dx, dy = x - cx, y - cy
            xs.append(cx + dx * ca + dy * sa)
            ys.append(cy - dx * sa + dy * ca)
        it.x0, it.x1, it.y0, it.y1 = min(xs), max(xs), min(ys), max(ys)


def text_angle(items) -> float:
    angs = []
    for it in items:
        if it.kind != "text" or len(it.text) < 4:
            continue
        (x0, y0), (x1, y1) = it.quad[0], it.quad[1]
        w = math.hypot(x1 - x0, y1 - y0)
        hh = math.hypot(it.quad[3][0] - x0, it.quad[3][1] - y0)
        if w > 2.5 * hh:
            angs.append(math.atan2(y1 - y0, x1 - x0))
    return med(angs, 0.0)


# ----------------------------------------------------------------------------- image analysis
def paper_hull(bgr):
    import cv2
    import numpy as np
    h, w = bgr.shape[:2]
    s = max(1, int(max(h, w) / 800))
    small = cv2.resize(bgr, (w // s, h // s), interpolation=cv2.INTER_AREA)
    hsv = cv2.cvtColor(small, cv2.COLOR_BGR2HSV)
    _, S, V = cv2.split(hsv)
    m = ((S < 60) & (V > 115)).astype(np.uint8) * 255
    m = cv2.morphologyEx(m, cv2.MORPH_CLOSE, np.ones((9, 9), np.uint8))
    m = cv2.morphologyEx(m, cv2.MORPH_OPEN, np.ones((7, 7), np.uint8))
    n, lab, stats, _ = cv2.connectedComponentsWithStats(m)
    if n <= 1:
        return None, s
    big = 1 + int(np.argmax(stats[1:, cv2.CC_STAT_AREA]))
    comp = (lab == big).astype(np.uint8)
    cnts, _ = cv2.findContours(comp, cv2.RETR_EXTERNAL, cv2.CHAIN_APPROX_SIMPLE)
    hull = cv2.convexHull(max(cnts, key=cv2.contourArea))
    return hull, s


def skin_fraction(hsv, quad) -> float:
    """Share of the box covered by skin tones (a thumb or hand over the text)."""
    import numpy as np
    xs = [p[0] for p in quad]
    ys = [p[1] for p in quad]
    c = hsv[max(0, int(min(ys))):int(max(ys)) + 1, max(0, int(min(xs))):int(max(xs)) + 1].reshape(-1, 3).astype(int)
    if c.size == 0:
        return 0.0
    m = (c[:, 0] >= 3) & (c[:, 0] <= 25) & (c[:, 1] >= 40) & (c[:, 1] <= 180) & (c[:, 2] >= 90)
    return float(m.mean())


def ink_colour(hsv, quad, paper) -> str:
    """'colour:<name>' when the strokes are saturated ink unlike the paper; skin-tone hues are ignored (thumbs)."""
    import numpy as np
    xs = [p[0] for p in quad]
    ys = [p[1] for p in quad]
    x0, x1 = max(0, int(min(xs))), int(max(xs)) + 1
    y0, y1 = max(0, int(min(ys))), int(max(ys)) + 1
    crop = hsv[y0:y1, x0:x1].reshape(-1, 3)
    if crop.size == 0:
        return "print"
    Hh, S, V = crop[:, 0].astype(int), crop[:, 1].astype(int), crop[:, 2].astype(int)
    ps, pv = paper
    ink = (S > ps + 45) | (V < pv - 60)
    if ink.sum() < 25:
        return "print"
    col = ink & (S > 55) & (V > 150) & ~((Hh >= 4) & (Hh <= 25))
    if col.sum() < 0.5 * ink.sum():
        return "print"
    s_ink, v_ink, h_ink = np.median(S[col]), np.median(V[col]), np.median(Hh[col])
    if s_ink > 60 and v_ink > 150:
        name = ("pink" if 140 <= h_ink <= 172 else "red" if h_ink > 172 or h_ink < 10 else "orange/yellow" if h_ink < 35
                else "green" if h_ink < 85 else "blue" if h_ink < 135 else "purple")
        return f"colour:{name}"
    return "print"


# ----------------------------------------------------------------------------- segmentation
def split_by_gap(vals_items, gap):
    vals_items = sorted(vals_items, key=lambda v: v[0])
    groups, cur = [], []
    for v, it in vals_items:
        if cur and v - cur[-1][0] > gap:
            groups.append(cur)
            cur = []
        cur.append((v, it))
    if cur:
        groups.append(cur)
    return [[it for _, it in g] for g in groups]


def segment(items, W, H):
    """Repeated-form segmentation. Returns (instances, bands, info). Assigns it.region / it.inst."""
    texts = [it for it in items if it.kind == "text" and it.where == "sheet"]
    allv = [it for it in items if it.where == "sheet"]
    h_med = med((it.h for it in texts), 20.0)
    cnt = Counter(norm(it.text) for it in texts)
    best = None
    for tok, c in cnt.items():
        if c < 2 or not label_like(tok):
            continue
        occ = [it for it in texts if norm(it.text) == tok]
        hm = med(o.h for o in occ)
        occ = [o for o in occ if 0.72 * hm <= o.h <= 1.38 * hm]      # same field ⇒ same font size
        cols = split_by_gap([(o.cx, o) for o in occ], 0.18 * W)
        valid, pitches, virt = [], [], 0
        for col in cols:
            if len(col) < 2:
                continue
            ys = sorted(o.cy for o in col)
            d = [b - a for a, b in zip(ys, ys[1:])]
            p = min(d)
            if p < 4 * h_med:
                continue
            ratios = [x / p for x in d]
            if all(abs(r - round(r)) <= 0.22 and round(r) <= 3 for r in ratios):
                valid += col
                virt += sum(round(r) - 1 for r in ratios)
                pitches += [x / round(x / p) for x in d]
        if len(valid) < 2:
            continue
        score = len(valid) - 1.5 * virt                                # a gap needing a virtual anchor costs more than it adds
        key = (-score, med(o.cy for o in valid), tok)
        if best is None or key < best[0]:
            best = (key, tok, valid, med(pitches))
    if best is None:
        for it in allv:
            it.region = "free"
        return [], [(0.0, float(W))], {"mode": "free layout (no repeated labels)"}
    _, anchor_tok, valid, P = best
    cols = split_by_gap([(o.cx, o) for o in valid], 0.18 * W)
    cols = [c for c in cols if len(c) >= 2] or cols
    centers = sorted(med(o.cx for o in c) for c in cols)
    cols = sorted(cols, key=lambda c: med(o.cx for o in c))
    # column boundary = the emptiest vertical gutter between two columns, measured over the sticker zone only
    ya = min(o.cy for c in cols for o in c)
    yb = max(o.cy for c in cols for o in c)
    zone = [it for it in allv if ya - 0.3 * P <= it.cy <= yb + 0.7 * P]
    bounds = [0.0]
    for a, b in zip(centers, centers[1:]):
        xs = [a + (b - a) * k / 240 for k in range(241)]
        cov = [sum(1 for it in zone if it.x0 <= x <= it.x1) for x in xs]
        m, runs, start = min(cov), [], None
        for k, c in enumerate(cov):
            if c == m and start is None:
                start = k
            if c != m and start is not None:
                runs.append((start, k - 1))
                start = None
        if start is not None:
            runs.append((start, len(cov) - 1))
        s0, e0 = max(runs, key=lambda r: r[1] - r[0])
        bounds.append((xs[s0] + xs[e0]) / 2)
    bounds.append(float(W))
    bands = list(zip(bounds[:-1], bounds[1:]))
    # anchors per column (fill gaps with virtual anchors)
    anchors = []
    for c in cols:
        ys = sorted(o.cy for o in c)
        filled = [ys[0]]
        for y in ys[1:]:
            gap = y - filled[-1]
            k = round(gap / P) if P else 1
            for j in range(1, k):
                filled.append(filled[-1] + gap / k)
            filled.append(y)
        anchors.append(filled)
    # boundary phase from all lines between the first and last anchor of each column
    rel = []
    for bi, (xl, xr) in enumerate(bands):
        A = anchors[bi]
        for it in allv:
            if xl <= it.cx < xr and A[0] <= it.cy < A[-1]:
                i = max(j for j in range(len(A) - 1) if A[j] <= it.cy)
                rel.append((it.cy - A[i]) / (A[i + 1] - A[i]))
    rel = sorted(r % 1.0 for r in rel)
    if len(rel) >= 2:
        gaps = [(b - a, (a + b) / 2) for a, b in zip(rel, rel[1:])] + [(rel[0] + 1 - rel[-1], ((rel[-1] + rel[0] + 1) / 2) % 1.0)]
        r_b = max(gaps)[1]
    else:
        r_b = 0.75
    instances = []
    for bi, (xl, xr) in enumerate(bands):
        A = anchors[bi]
        loc = [b - a for a, b in zip(A, A[1:])] or [P]
        tops, bots = [], []
        for j, a in enumerate(A):
            p_next = loc[j] if j < len(loc) else loc[-1]
            tops.append(a - (1 - r_b) * loc[0] if j == 0 else A[j - 1] + r_b * loc[j - 1])
            bots.append(a + r_b * p_next)
        band_items = [it for it in allv if xl <= it.cx < xr]
        # a partially visible sticker below the last one: text below whose shape recurs in ≥ 2 stickers
        # (e.g. 999-999-9 item numbers), so a page footer such as "Page 1 of 2" is not mistaken for one
        sig_inst = defaultdict(set)
        for it in band_items:
            if it.kind == "text" and tops[0] <= it.cy < bots[-1]:
                j = next((j for j in range(len(A)) if tops[j] <= it.cy < bots[j]), -1)
                for sg in tok_shapes(it.text):
                    sig_inst[sg].add(j)
        recurring = {sg for sg, js in sig_inst.items() if len(js) >= 2}
        below = [it for it in band_items if it.cy >= bots[-1]]
        if any(it.kind == "text" and tok_shapes(it.text) & recurring for it in below):
            a_new = A[-1] + loc[-1]
            A.append(a_new)
            tops.append(bots[-1])
            bots.append(a_new + r_b * loc[-1])
        real = {round(o.cy, 3) for o in cols[bi]}
        for j in range(len(A)):
            instances.append({"band": bi, "col": bi, "top": tops[j], "bottom": bots[j], "anchor": A[j],
                              "virtual_anchor": round(A[j], 3) not in real})
        for it in band_items:
            if it.cy < tops[0]:
                it.region = "header"
            elif it.cy >= bots[-1]:
                it.region = "footer"
            else:
                it.region = "sticker"
    # reading order: rows across columns, then left → right
    cy_all = sorted(((ins["top"] + ins["bottom"]) / 2, k) for k, ins in enumerate(instances))
    row_of, row, last = {}, -1, None
    for cyv, k in cy_all:
        if last is None or cyv - last > 0.5 * P:
            row += 1
            last = cyv
        row_of[k] = row
    order = sorted(range(len(instances)), key=lambda k: (row_of[k], instances[k]["col"]))
    instances = [dict(instances[k], row=row_of[k]) for k in order]
    for n, ins in enumerate(instances):
        ins["n"] = n + 1
    for it in allv:
        if it.region != "sticker":
            continue
        xl_xr = [(bi, b) for bi, b in enumerate(bands) if b[0] <= it.cx < b[1]]
        bi = xl_xr[0][0]
        for ins in instances:
            if ins["band"] == bi and ins["top"] <= it.cy < ins["bottom"]:
                it.inst = ins["n"]
                break
    info = {"mode": "repeated form", "anchor_label": anchor_tok, "pitch_px": round(P, 1), "boundary_phase": round(r_b, 3),
            "columns": len(bands)}
    return instances, bands, info


# ----------------------------------------------------------------------------- label/value, tables
def field_labels(items):
    ins_txt = [it for it in items if it.kind == "text" and it.region == "sticker"]
    hs = sorted(it.h for it in ins_txt) or [20.0]
    h_all = hs[int(0.7 * (len(hs) - 1))]
    by = defaultdict(list)
    for it in ins_txt:
        by[norm(it.text)].append(it)
    labels = set()
    for tok, its in by.items():
        if len({it.inst for it in its}) >= 2 and label_like(tok) and med(it.h for it in its) <= 0.72 * h_all:
            labels.add(tok)
    return labels


def lev1(a: str, b: str) -> bool:
    """Edit distance ≤ 1."""
    if a == b:
        return True
    if abs(len(a) - len(b)) > 1:
        return False
    if len(a) > len(b):
        a, b = b, a
    i = j = diff = 0
    while i < len(a) and j < len(b):
        if a[i] != b[j]:
            diff += 1
            if diff > 1:
                return False
            if len(a) == len(b):
                i += 1
            j += 1
        else:
            i += 1
            j += 1
    return diff + (len(b) - j) + (len(a) - i) <= 1


def match_label(t: str, labels) -> str:
    n = norm(t)
    if n in labels:
        return n
    for L in sorted(labels):
        if len(L) >= 4 and lev1(n, L):
            return L
    return ""


def split_label_lines(items, labels, rot):
    """OCR sometimes merges a field label with its value ('ORD# OR1234567'): split at the word boxes."""
    out = []
    for it in items:
        if it.kind != "text" or it.region != "sticker":
            out.append(it)
            continue
        lab = match_label(it.text, labels)
        if lab:
            it.label = lab
            out.append(it)
            continue
        done = False
        if len(it.words) >= 2:
            for k in (3, 2, 1):
                if k >= len(it.words):
                    continue
                head = " ".join(w[0] for w in it.words[:k])
                lab = match_label(head, labels)
                if lab:
                    def mk(ws, text, label=""):
                        q = [p for w in ws for p in w[1]]
                        xs, ys = [p[0] for p in q], [p[1] for p in q]
                        quad = [[min(xs), min(ys)], [max(xs), min(ys)], [max(xs), max(ys)], [min(xs), max(ys)]]
                        n_ = Item(text=text, conf=it.conf, quad=quad, ink=it.ink, where=it.where, region=it.region,
                                  inst=it.inst, edge=it.edge, label=label, words=ws)
                        rotate([n_], *rot)
                        return n_
                    out.append(mk(it.words[:k], head, lab))
                    out.append(mk(it.words[k:], " ".join(w[0] for w in it.words[k:])))
                    done = True
                    break
        if not done:
            out.append(it)
    return out


def pair_values(inst_items, labels):
    """label → value(s): the text to the right on the same row, and/or the text directly below."""
    txt = [it for it in inst_items if it.kind == "text"]
    lab_h = med((it.h for it in txt if norm(it.text) in labels), 0.0)
    labs = sorted([it for it in txt if it.label or norm(it.text) in labels], key=lambda it: (it.y0, it.x0))
    used, pairs = set(), []
    for L in labs:
        L.role = "label"
        L.label = L.label or norm(L.text)
    for L in labs:
        Lh = min(L.h, 1.3 * lab_h) if lab_h else L.h
        ly1 = L.y0 + Lh if L.h > 1.5 * Lh else L.y1       # a split label's box spans the whole merged line
        right, below = None, None
        for V in txt:                                   # nearest text to the right on the same row (labels included)
            if V is L:
                continue
            ov = min(L.y1, V.y1) - max(L.y0, V.y0)
            if ov >= 0.3 * min(Lh, V.h) and V.x0 >= L.cx and V.cx > L.x1 and V.x0 - L.x1 <= 8 * Lh:
                if right is None or V.x0 < right.x0:
                    right = V
        for V in txt:                                   # nearest text directly below (labels included)
            if V is L:
                continue
            dy = V.y0 - ly1
            if -0.4 * Lh <= dy <= 1.8 * max(Lh, 0.5 * V.h) and V.cy > L.cy and (min(L.x1, V.x1) - max(L.x0, V.x0) > 0 or abs(V.x0 - L.x0) < 3 * Lh):
                if below is None or (dy, abs(V.x0 - L.x0)) < (below.y0 - ly1, abs(below.x0 - L.x0)):
                    below = V
        # a label's value is the first thing next to it, unless that is another label or already taken
        vals = [v for v in (right, below) if v is not None and v.role != "label" and id(v) not in used]
        for v in vals:
            used.add(id(v))
            v.role = "value"
        pairs.append((L, vals))
    for it in txt:
        if not it.role:
            it.role = "text"
    return pairs


def rows_of(items):
    """Group items (deskewed boxes) into rows: centre within ~½ line height of the row's median centre."""
    rows = []
    for it in sorted(items, key=lambda it: (it.cy, it.x0)):
        for r in reversed(rows[-4:]):
            rcy, rh = med(x.cy for x in r), med(x.h for x in r)
            if abs(it.cy - rcy) <= 0.45 * min(it.h, rh) + 0.15 * max(it.h, rh):
                r.append(it)
                break
        else:
            rows.append([it])
    rows = [sorted(r, key=lambda it: it.x0) for r in rows]
    return sorted(rows, key=lambda r: med(x.cy for x in r))


COLNAME_RULES = [
    (re.compile(r"^\d{3}-\d{3}-\d$"), "item no."),
    (re.compile(r"^[A-Z]\d{5}$"), "code"),
    (re.compile(r"^\d+(CASE|BOX|CS|BX|PC|PCS|EA|PK|BAG|CTN)S?$", re.I), "qty"),
    (re.compile(r"^\d+(\.\d+)?(LBS?|KG|G|OZ)$", re.I), "weight"),
]


def header_tables(hdr_items, bands, W, rot=None):
    """Tables = runs of ≥ 2 rows with ≥ 3 cells, one per band; cells split into tokens placed by x."""
    tables = []
    for bi, (xl, xr) in enumerate(bands):
        its = [it for it in hdr_items if it.kind == "text" and xl <= it.cx < xr]
        rws = rows_of(its)
        cand = [r for r in rws if len(r) >= 2 and sum(len(x.text.split()) for x in r) >= 3
                and sum(c.isdigit() for x in r for c in x.text) >= 4]
        runs, cur = [], []
        for r in rws:
            if r in cand:
                cur.append(r)
            else:
                if len(cur) >= 2:
                    runs.append(cur)
                cur = []
        if len(cur) >= 2:
            runs.append(cur)
        for run in runs:
            toks = []                                     # (x_start, x_end, row_index, token)
            for ri, r in enumerate(run):
                for it in r:
                    if it.words and rot is not None:      # true word boxes from the recogniser
                        wits = [Item(text=w[0], conf=1.0, quad=w[1]) for w in it.words]
                        rotate(wits, *rot)
                        toks += [(w.x0, w.x1, ri, w.text) for w in wits]
                        continue
                    n = max(1, len(it.text))
                    pos = 0
                    for wd in it.text.split():
                        k = it.text.index(wd, pos)
                        pos = k + len(wd)
                        toks.append((it.x0 + it.w * k / n, it.x0 + it.w * (k + len(wd)) / n, ri, wd))
            cw = med((t[1] - t[0]) / max(1, len(t[3])) for t in toks)
            spans = []                                    # merge token extents; a gutter ≥ 1.6 characters splits columns
            for x0_, x1_, _, _ in sorted(toks):
                if spans and x0_ - spans[-1][1] < 1.6 * cw:
                    spans[-1][1] = max(spans[-1][1], x1_)
                else:
                    spans.append([x0_, x1_])
            grid = [["" for _ in spans] for _ in run]
            for x0_, x1_, ri, wd in sorted(toks):
                c = (x0_ + x1_) / 2
                ci = min(range(len(spans)), key=lambda k: 0 if spans[k][0] <= c <= spans[k][1] else min(abs(c - spans[k][0]), abs(c - spans[k][1])))
                grid[ri][ci] = (grid[ri][ci] + " " + wd).strip()
            cols = spans
            keep = [c for c in range(len(cols)) if sum(1 for g in grid if g[c]) >= 1]
            grid = [[g[c] for c in keep] for g in grid]
            names = []

            def name_of(vals):
                return next((name for rx, name in COLNAME_RULES if vals and sum(bool(rx.match(v)) for v in vals) >= 0.6 * len(vals)), "")
            for c in range(len(keep)):
                vals = [g[c].replace(" ", "") for g in grid if g[c]]
                nm = name_of(vals)
                parts = [g[c].split() for g in grid if g[c]]
                if not nm and parts and all(len(x) == 2 for x in parts):
                    a_, b_ = name_of([x[0] for x in parts]), name_of([x[1] for x in parts])
                    nm = f"{a_} + {b_}" if a_ and b_ else ""
                nm = nm or f"col {c + 1}"
                if nm in names:
                    nm = f"{nm} ({names.count(nm) + 1 + sum(1 for x in names if x.startswith(nm + ' ('))})"
                names.append(nm)
            tables.append({"band": bi, "columns": names, "rows": grid,
                           "top": min(x.y0 for r in run for x in r), "bottom": max(x.y1 for r in run for x in r)})
    return tables


# ----------------------------------------------------------------------------- rendering
BAR_GLYPHS = "▌│║▐▍▎"


def barcode_glyph(value: str, n: int) -> str:
    hsh = hashlib.sha1(value.encode()).digest()
    return "".join(BAR_GLYPHS[b % len(BAR_GLYPHS)] for b in (hsh * 4)[:n])


def item_label(it: Item) -> str:
    if it.kind == "barcode":
        return ""
    t = it.text
    if it.ink.startswith("colour"):
        t = "✎" + t
    if it.conf < LOW_CONF:
        t = f"‹{t}›"
    return t


def layout_rows(items, X0, X1, inner_w):
    """Place items on character rows: x scaled into inner_w, collisions pushed right, overflow → continuation row."""
    if not items:
        return []
    h_med = med((it.h for it in items if it.kind == "text"), 20.0)
    out, prev_bottom = [], None
    span = max(1.0, X1 - X0)
    for r in rows_of(items):
        if prev_bottom is not None and med(x.y0 for x in r) - prev_bottom > 1.3 * h_med:
            out.append("")
        prev_bottom = max(x.y1 for x in r)
        pending = []
        for it in r:
            if it.kind == "barcode":
                n = max(6, min(20, round(it.w / span * inner_w)))
                pending.append((round((it.x0 - X0) / span * inner_w), barcode_glyph(it.text, n)))
            else:
                pending.append((round((it.x0 - X0) / span * inner_w), item_label(it)))
        while pending:
            line, cursor, rest = [], 0, []
            for col, s in pending:
                start = max(col, cursor + (1 if cursor else 0))
                if start + len(s) > inner_w and line:
                    rest.append((col, s))
                    continue
                if start + len(s) > inner_w:              # first item too long for the row: shift left / wrap
                    start = max(0, inner_w - len(s))
                    if len(s) > inner_w:
                        cut = s.rfind(" ", 0, inner_w)
                        cut = cut if cut > 0 else inner_w
                        rest.insert(0, (col, s[cut:].strip()))
                        s = s[:cut]
                        start = 0
                line.append((start, s))
                cursor = start + len(s)
            buf = [" "] * inner_w
            for start, s in line:
                for k, ch in enumerate(s):
                    if start + k < inner_w:
                        buf[start + k] = ch
            out.append("".join(buf).rstrip())
            pending = rest
    return out


def card(lines, inner_w, title=""):
    top = "┌" + ("─ " + title + " " if title else "") + "─" * max(0, inner_w - (len(title) + 3 if title else 0)) + "┐"
    top = top[: inner_w + 1] + "┐" if len(top) > inner_w + 2 else top
    body = ["│" + l.ljust(inner_w)[:inner_w] + "│" for l in lines] or ["│" + " " * inner_w + "│"]
    return [top] + body + ["└" + "─" * inner_w + "┘"]


def local_deskew(items):
    if not items:
        return None
    ang = text_angle(items)
    xs = [p[0] for it in items for p in it.quad]
    ys = [p[1] for it in items for p in it.quad]
    rot = (ang, (min(xs) + max(xs)) / 2, (min(ys) + max(ys)) / 2)
    rotate(items, *rot)
    return rot


# ----------------------------------------------------------------------------- worker
_ENGINE = {}


def get_engine(threads, max_side):
    from rapidocr import RapidOCR
    key = (threads, max_side)
    if key not in _ENGINE:
        _ENGINE[key] = RapidOCR(params={"Global.log_level": "error", "Global.max_side_len": max_side, "Global.return_word_box": True,
                                        "EngineConfig.onnxruntime.intra_op_num_threads": threads,
                                        "EngineConfig.onnxruntime.inter_op_num_threads": 1})
    return _ENGINE[key]


def process_photo(task):
    import cv2
    import numpy as np
    import pillow_heif
    import zxingcpp
    from PIL import Image, ImageOps
    pillow_heif.register_heif_opener()
    t0 = time.perf_counter()
    src, out = Path(task["src"]), Path(task["out"])
    res = {"file": src.name, "stem": src.stem, "status": "ok", "timings_ms": {}, "warnings": []}
    try:
        t = time.perf_counter()
        im0 = Image.open(src)
        exif = im0.getexif()
        res["meta"] = {"camera": str(exif.get(0x0110) or ""), "taken_local": str(exif.get(0x0132) or ""),
                       "gps_in_source": bool(exif.get_ifd(0x8825)), "orientation_tag": int(exif.get(0x0112) or 1)}
        im = ImageOps.exif_transpose(im0).convert("RGB")
        W, H = im.size
        res["size"] = [W, H]
        arr = np.asarray(im)
        prev = im.copy()
        prev.thumbnail((PREVIEW_EDGE, PREVIEW_EDGE))
        (out / "previews").mkdir(parents=True, exist_ok=True)
        prev.save(out / "previews" / f"{src.stem}.jpg", quality=85, optimize=True)   # no EXIF written
        res["timings_ms"]["decode"] = round((time.perf_counter() - t) * 1000)

        t = time.perf_counter()
        eng = get_engine(task["threads"], task["max_side"])
        r = eng(arr)
        res["timings_ms"]["ocr"] = round((time.perf_counter() - t) * 1000)
        el = getattr(r, "elapse_list", None) or []
        res["timings_ms"]["ocr_det_cls_rec"] = [round(x * 1000) for x in el]
        items = []
        if r.boxes is not None:
            wres = getattr(r, "word_results", None) or [()] * len(r.txts)
            for q, tx, sc, wr in zip(r.boxes, r.txts, r.scores, wres):
                words = []
                for w in wr or ():
                    try:
                        wq = np.asarray(w[2], dtype=float).reshape(4, 2).tolist()
                        words.append([str(w[0]), wq])
                    except Exception:  # noqa: BLE001 - word boxes are optional
                        pass
                items.append(Item(text=str(tx).strip(), conf=float(sc), quad=[[float(a), float(b)] for a, b in q], words=words))
        items = [it for it in items if it.text]

        t = time.perf_counter()
        for c in zxingcpp.read_barcodes(im):
            p = c.position
            quad = [[p.top_left.x, p.top_left.y], [p.top_right.x, p.top_right.y],
                    [p.bottom_right.x, p.bottom_right.y], [p.bottom_left.x, p.bottom_left.y]]
            items.append(Item(text=c.text, conf=1.0, quad=[[float(a), float(b)] for a, b in quad], kind="barcode",
                              fmt=str(c.format)))
        res["timings_ms"]["barcodes"] = round((time.perf_counter() - t) * 1000)

        t = time.perf_counter()
        bgr = cv2.cvtColor(arr, cv2.COLOR_RGB2BGR)
        hull, s = paper_hull(bgr)
        hsv = cv2.cvtColor(bgr, cv2.COLOR_BGR2HSV)
        small = hsv[::s, ::s].reshape(-1, 3)
        pap = small[(small[:, 1] < 60) & (small[:, 2] > 115)]
        paper = (float(np.median(pap[:, 1])), float(np.median(pap[:, 2]))) if len(pap) else (20.0, 200.0)
        res["paper_sv"] = [round(paper[0]), round(paper[1])]
        for it in items:
            cxq = sum(p[0] for p in it.quad) / 4
            cyq = sum(p[1] for p in it.quad) / 4
            if hull is not None and cv2.pointPolygonTest(hull, (cxq / s, cyq / s), False) < 0:
                it.where = "background"
            if it.kind == "text":
                it.ink = ink_colour(hsv, it.quad, paper)
                it.skin = round(skin_fraction(hsv, it.quad), 3)
            xs = [p[0] for p in it.quad]
            ys = [p[1] for p in it.quad]
            it.edge = min(xs) < 0.006 * W or max(xs) > 0.994 * W or min(ys) < 0.006 * H or max(ys) > 0.994 * H
        ang = text_angle(items)
        res["deskew_deg"] = round(math.degrees(ang), 2)
        rotate(items, ang, W / 2, H / 2)
        instances, bands, seg = segment(items, W, H)
        res["segmentation"] = seg
        labels = field_labels(items)
        res["field_labels"] = sorted(labels)
        items = split_label_lines(items, labels, (ang, W / 2, H / 2))
        # render-time geometry: local deskew per region / per column of stickers
        sheet = [it for it in items if it.where == "sheet"]
        hdr = [it for it in sheet if it.region in ("header", "free")]
        ftr = [it for it in sheet if it.region == "footer"]
        hdr_rot = local_deskew(hdr)
        local_deskew(ftr)
        per_inst = defaultdict(list)
        for it in sheet:
            if it.region == "sticker" and it.inst > 0:
                per_inst[it.inst].append(it)
        for k, its in per_inst.items():
            local_deskew(its)
        pairs = {k: pair_values(its, labels) for k, its in per_inst.items()}
        tables = header_tables(hdr, bands if len(bands) > 1 else [(0.0, float(W))], W, hdr_rot)
        res["timings_ms"]["layout"] = round((time.perf_counter() - t) * 1000)
        res["items"] = [dict(asdict(it)) for it in items]
        res["instances"] = instances
        res["pairs"] = {k: [(L.label, L.text, [v.text for v in vs]) for L, vs in pr] for k, pr in pairs.items()}
        res["tables"] = tables
        res["W"], res["H"] = W, H
    except Exception as e:  # noqa: BLE001
        import traceback
        res["status"] = f"error: {type(e).__name__}: {e}"
        res["trace"] = traceback.format_exc()
    res["timings_ms"]["total"] = round((time.perf_counter() - t0) * 1000)
    return res


# ----------------------------------------------------------------------------- assembly (main process)
def md_cell(s: str) -> str:
    return str(s).replace("|", "\\|").replace("\n", " ").strip()


def rebuild(res):
    items = [Item(**{k: v for k, v in d.items()}) for d in res["items"]]
    return items


def digits(s):
    return re.sub(r"\D", "", s)


def cross_checks(items, instances, tables):
    out = {"barcodes": [], "stickers": {}}
    texts = [it for it in items if it.kind == "text"]
    for b in (it for it in items if it.kind == "barcode"):
        dv = digits(b.text)
        hit = next((it for it in texts if digits(it.text) == dv and len(dv) >= 6), None)
        near = None
        if not hit:
            cands = [it for it in texts if len(digits(it.text)) >= 6 and it.y0 >= b.y1 - 0.5 * b.h and it.y0 - b.y1 < 1.5 * b.h
                     and min(it.x1, b.x1) - max(it.x0, b.x0) > 0]
            near = min(cands, key=lambda it: it.y0 - b.y1) if cands else None
        out["barcodes"].append({"value": b.text, "format": b.fmt, "inst": b.inst, "printed_digits_match": bool(hit),
                                "ocr_near": (near.text if near else None)})
    rows = [(ti, ri, r) for ti, tb in enumerate(tables) for ri, r in enumerate(tb["rows"])]
    for ins in instances:
        its = [it for it in texts if it.inst == ins["n"]]
        if not its:
            continue
        first = rows_of(its)[0]
        toks = [tk for it in first for tk in it.text.split() if sum(c.isdigit() for c in tk) >= 3 and len(tk) >= 5]
        qty = [it.text for it in first if re.fullmatch(r"\d{1,3}", it.text.strip())]
        unit = [it.text for it in first if re.fullmatch(r"[A-Z]{2,5}", it.text.strip())]
        match, qty_ok = None, None
        if toks:
            cands = [(ti, ri, cells) for ti, ri, cells in rows if all(tk in "|".join(cells).replace(" ", "") for tk in toks)]
            want = (qty[0] + unit[0]).replace(" ", "") if qty and unit else None
            if want:
                good = [c for c in cands if any(cell.replace(" ", "") == want for cell in c[2])]
                match = (good or cands or [None])[0]
                qty_ok = bool(good) if cands else None
            else:
                match = cands[0] if cands else None
        out["stickers"][ins["n"]] = {"keys": toks, "summary_row": " | ".join(match[2]) if match else None, "qty_consistent": qty_ok}
    return out


def build_photo_md(res, prev_rel, boiler=frozenset()):
    items = rebuild(res)
    W, H = res["W"], res["H"]
    instances = res["instances"]
    tables = res["tables"]
    sheet = [it for it in items if it.where == "sheet"]
    two = res["segmentation"].get("columns", 1) > 1
    inner = 54 if two else 64
    gap = 2
    full_inner = (inner + 2) * 2 + gap - 2 if two else inner
    hdr = [it for it in sheet if it.region in ("header", "free")]
    ftr = [it for it in sheet if it.region == "footer"]
    view = []
    if hdr:
        X0, X1 = min(it.x0 for it in hdr), max(it.x1 for it in hdr)
        title = "sheet header" if instances else "photo text"
        view += card(layout_rows(hdr, X0, X1, full_inner), full_inner, title)
    # sticker cards per column, aligned by row
    cols = defaultdict(list)
    for ins in instances:
        cols[ins["col"]].append(ins)
    col_x = {}
    for c, lst in cols.items():
        its = [it for it in sheet if it.inst in {i["n"] for i in lst}]
        if its:
            col_x[c] = (min(it.x0 for it in its), max(it.x1 for it in its))
    cards = {}
    for ins in instances:
        its = [it for it in sheet if it.inst == ins["n"]]
        if not its:
            continue
        X0, X1 = col_x[ins["col"]]
        X0, X1 = min(X0, min(it.x0 for it in its)), max(X1, max(it.x1 for it in its))
        lines = layout_rows(its, X0, X1, inner)
        cards[ins["n"]] = card(lines, inner, f"#{ins['n']}")
    rows = defaultdict(dict)
    for ins in instances:
        if ins["n"] in cards:
            rows[ins["row"]][ins["col"]] = cards[ins["n"]]
    ncol = res["segmentation"].get("columns", 1)
    for rk in sorted(rows):
        cs = [rows[rk].get(c) for c in range(ncol)]
        hmax = max(len(x) for x in cs if x)
        for li in range(hmax):
            parts = []
            for c in cs:
                if c and li < len(c):
                    parts.append(c[li])
                else:
                    parts.append(" " * (inner + 2))
            view.append((" " * gap).join(parts).rstrip())
    if ftr:
        X0, X1 = min(it.x0 for it in ftr), max(it.x1 for it in ftr)
        view += card(layout_rows(ftr, X0, X1, full_inner), full_inner, "below the stickers")
    checks = cross_checks(items, instances, tables)
    res["checks"] = checks
    # ---------- markdown
    def name_like(t):
        letters = sum(c.isalpha() for c in t)
        return letters >= 4 and letters >= 0.3 * len(t.replace(" ", "")) and norm(t) not in boiler
    title = next((it.text for it in sorted(hdr, key=lambda it: it.y0) if it.kind == "text" and it.ink == "print" and name_like(it.text)),
                 res["file"])
    texts = [it for it in items if it.kind == "text"]
    confs = [it.conf for it in texts]
    bcs = [it for it in items if it.kind == "barcode"]
    n_ok_bc = sum(1 for b in checks["barcodes"] if b["printed_digits_match"])
    md = [f"# {title}", "",
          f"<img src=\"{prev_rel}\" width=\"300\" align=\"right\" alt=\"photo preview\">", "",
          "| Source photo | Taken (local) | Camera | Pixels | Text lines | Mean OCR conf. | Barcodes read | Stickers |",
          "|---|---|---|---|---|---|---|---|",
          f"| `{res['file']}` | {res['meta'].get('taken_local') or '—'} | {res['meta'].get('camera') or '—'} | {W}×{H} | {len(texts)} | "
          f"{(sum(confs) / len(confs)) if confs else 0:.3f} | {len(bcs)} ({n_ok_bc} match printed digits) | {len(instances)} |", "",
          f"*OCR: PP-OCRv6 (det+rec) on ONNX Runtime CPU · barcodes: ZXing · deskew {res.get('deskew_deg', 0)}° · "
          f"layout: {res['segmentation'].get('mode')}"
          + (f" (anchor label “{res['segmentation'].get('anchor_label')}”, {ncol} column{'s' if ncol > 1 else ''})" if instances else "")
          + " · metadata stripped (GPS in source: " + ("yes" if res["meta"].get("gps_in_source") else "no") + ")*", "",
          "## Sheet view", "",
          "The photo's text in its original arrangement: each sticker is a box, side by side as on the sheet.", "",
          "```text", *view, "```", "",
          "Legend: `‹…›` low OCR confidence (< 0.85) · `✎` coloured ink (hand annotation) · "
          "`▌│║▐` barcode (decoded value in the sticker table) · rows follow the photo top → bottom.", ""]
    # ---------- tables from the header
    for ti, tb in enumerate(tables):
        md += [f"## Table in the sheet header" + (f" ({'left' if tb['band'] == 0 else 'right'} block)" if two else ""), "",
               "| # | " + " | ".join(tb["columns"]) + " |", "|---|" + "---|" * len(tb["columns"])]
        for ri, r in enumerate(tb["rows"], 1):
            md.append(f"| {ri} | " + " | ".join(md_cell(c) or "—" for c in r) + " |")
        md.append("")
    # ---------- stickers table
    if instances:
        lab_order, lab_disp = [], {}
        for k, prs in res["pairs"].items():
            for canon, disp, _ in prs:
                if canon not in lab_order:
                    lab_order.append(canon)
                if canon == norm(disp):
                    lab_disp.setdefault(canon, disp)
        head = ["#", "Pos", "First line"] + [md_cell(lab_disp.get(l, l)) for l in lab_order] + ["Other text", "Barcode", "Summary row"]
        md += ["## Stickers", "", "| " + " | ".join(head) + " |", "|" + "---|" * len(head)]
        for ins in instances:
            its = [it for it in texts if it.inst == ins["n"]]
            if not its:
                md.append(f"| {ins['n']} | r{ins['row'] + 1} c{ins['col'] + 1} | *(no text found — outside the photo)* |" + " |" * (len(head) - 3))
                continue
            rws = rows_of(its)
            first = " · ".join(it.text for it in rws[0])
            prs = {canon: vs for canon, _, vs in res["pairs"].get(ins["n"], res["pairs"].get(str(ins["n"]), []))}
            cells = []
            for l in lab_order:
                vs = prs.get(l)
                cells.append(" · ".join(vs) if vs else ("·" if vs == [] else "—"))
            used = {id(x) for x in rws[0]}
            other = [it.text for r in rws[1:] for it in r if it.role == "text" and id(it) not in used]
            bc = [b for b in checks["barcodes"] if b["inst"] == ins["n"]]
            bc_s = " · ".join(f"`{b['value']}` " + ("✓" if b["printed_digits_match"] else
                              ("(printed digits not visible)" if b["ocr_near"] is None else f"✗ printed “{b['ocr_near']}”"))
                              for b in bc) or "—"
            sc = checks["stickers"].get(ins["n"], {})
            srow = ("✓" + ("" if sc.get("qty_consistent") in (True, None) else " (qty differs)")) if sc.get("summary_row") else ("✗ not in table" if tables else "—")
            md.append(f"| {ins['n']} | r{ins['row'] + 1} c{ins['col'] + 1} | {md_cell(first)} | " + " | ".join(md_cell(c) for c in cells)
                      + f" | {md_cell(' · '.join(other)) or '—'} | {bc_s} | {srow} |")
        md.append("")
        md.append("`·` = label printed but empty · `—` = label not visible on this sticker.")
        md.append("")
    # ---------- flags
    flags = []
    if instances:
        tmpl = Counter(canon for k, prs in res["pairs"].items() for canon, _, _ in prs)
        n_inst_with_text = len({it.inst for it in texts if it.inst > 0})
        common = {l for l, c in tmpl.items() if c >= 0.5 * n_inst_with_text}
        for ins in instances:
            present = {canon for canon, _, _ in res["pairs"].get(ins["n"], res["pairs"].get(str(ins["n"]), []))}
            its = [it for it in items if it.inst == ins["n"]]
            miss = sorted(common - present)
            edge = any(it.edge for it in its)
            hand = [it.text for it in its if it.kind == "text" and it.skin >= 0.2]
            if not its:
                flags.append(f"Sticker #{ins['n']}: expected from the sheet pattern but no text visible (outside the photo).")
            elif miss or edge or hand:
                why = []
                if edge:
                    why.append("cut at the photo edge")
                if hand:
                    why.append("partly covered by a hand/thumb near " + " · ".join(f"“{t}”" for t in hand))
                if miss and not why:
                    why.append("covered or outside the photo")
                flags.append(f"Sticker #{ins['n']} (row {ins['row'] + 1}, col {ins['col'] + 1}): "
                             + (f"missing {', '.join(miss)} — " if miss else "") + "; ".join(why) + ".")
    cut = [it.text for it in sheet if it.edge and it.kind == "text" and it.inst <= 0]
    if cut:
        flags.append("Header/footer text touching the photo edge (may be cut off): " + " · ".join(f"“{c}”" for c in cut) + ".")
    low = [it for it in texts if it.conf < LOW_CONF]
    if low:
        flags.append("Low-confidence OCR: " + " · ".join(f"“{it.text}” ({it.conf:.2f})" for it in low) + ".")
    col = [it for it in texts if it.ink.startswith("colour")]
    if col:
        flags.append("Coloured ink (likely hand annotation): " + " · ".join(f"“{it.text}” ({it.ink.split(':')[1]})" for it in col) + ".")
    bgd = [it for it in texts if it.where == "background"]
    if bgd:
        flags.append("Text outside the sheet (background objects; kept, not part of the layout): " + " · ".join(f"“{it.text}”" for it in bgd) + ".")
    bad_bc = [b for b in checks["barcodes"] if not b["printed_digits_match"] and b["ocr_near"] is not None]
    hid_bc = [b for b in checks["barcodes"] if not b["printed_digits_match"] and b["ocr_near"] is None]
    if hid_bc:
        flags.append("Barcode decoded but its printed digits are not visible (cut/covered): " + " · ".join(b["value"] for b in hid_bc) + ".")
    if bad_bc:
        flags.append("Barcode value differs from the OCR of its printed digits: " + " · ".join(f"{b['value']} vs {b['ocr_near']}" for b in bad_bc) + ".")
    not_in = [n for n, sc in checks["stickers"].items() if tables and not sc.get("summary_row")]
    if not_in:
        flags.append("Stickers not matched to a header-table row: " + ", ".join(f"#{n}" for n in not_in) + ".")
    md += ["## Flags", ""] + ([f"- {f}" for f in flags] or ["- none"]) + [""]
    res["flags"] = flags
    return "\n".join(md) + "\n", title


def truth_score(res):
    stem = res["stem"]
    truth = TRUTH.get(stem)
    if not truth:
        return None
    joined = "".join(it["text"] for it in res["items"] if it["kind"] == "text").replace(" ", "")
    hits = [s for s in truth if s.replace(" ", "") in joined]
    return {"n": len(truth), "hits": len(hits), "missed": [s for s in truth if s not in hits]}


# ----------------------------------------------------------------------------- bench (means test)
def _ocr_only(task):
    import numpy as np
    import pillow_heif
    from PIL import Image, ImageOps
    pillow_heif.register_heif_opener()
    im = ImageOps.exif_transpose(Image.open(task["src"])).convert("RGB")
    eng = get_engine(task["threads"], task["max_side"])
    t = time.perf_counter()
    r = eng(np.asarray(im))
    dt = time.perf_counter() - t
    txt = "".join(str(x) for x in (r.txts or ())).replace(" ", "")
    truth = TRUTH.get(Path(task["src"]).stem, [])
    return {"stem": Path(task["src"]).stem, "ocr_s": round(dt, 3), "lines": len(r.txts or ()),
            "hits": sum(1 for s in truth if s.replace(" ", "") in txt), "n": len(truth)}


def bench(photos, out):
    ctx = mp.get_context("spawn")
    res = {"splits": [], "resolution": [], "tesseract": []}
    for procs, threads in ((1, 6), (2, 3), (3, 2), (6, 1), (1, 12)):
        t = time.perf_counter()
        with ctx.Pool(procs) as pool:
            pool.map(_ocr_only, [{"src": str(photos[0]), "threads": threads, "max_side": 2000}] * procs)   # warm-up / model load
            t = time.perf_counter()
            rr = pool.map(_ocr_only, [{"src": str(p), "threads": threads, "max_side": 2000} for p in photos])
        res["splits"].append({"procs": procs, "threads": threads, "wall_s": round(time.perf_counter() - t, 2),
                              "per_photo_s": [x["ocr_s"] for x in rr]})
        print("split", res["splits"][-1], flush=True)
    for ms in (1280, 2000, 3000, 4032):
        with ctx.Pool(3) as pool:
            rr = pool.map(_ocr_only, [{"src": str(p), "threads": 2, "max_side": ms} for p in photos])
        res["resolution"].append({"max_side": ms, "hits": sum(x["hits"] for x in rr), "n": sum(x["n"] for x in rr),
                                  "per_photo": [(x["stem"][-9:], x["hits"], x["n"], x["ocr_s"]) for x in rr],
                                  "ocr_s_sum": round(sum(x["ocr_s"] for x in rr), 2)})
        print("res", res["resolution"][-1], flush=True)
    import pillow_heif
    from PIL import Image, ImageOps
    pillow_heif.register_heif_opener()
    with tempfile.TemporaryDirectory() as td:
        for p in photos:
            png = Path(td) / "x.png"
            ImageOps.exif_transpose(Image.open(p)).convert("RGB").save(png)
            truth = TRUTH.get(p.stem, [])
            for psm in (3, 11):
                t = time.perf_counter()
                tx = subprocess.run(["tesseract", str(png), "-", "--psm", str(psm)], capture_output=True, text=True).stdout
                dt = time.perf_counter() - t
                j = tx.replace(" ", "").replace("\n", "")
                res["tesseract"].append({"stem": p.stem, "psm": psm, "s": round(dt, 2),
                                         "hits": sum(1 for s in truth if s.replace(" ", "") in j), "n": len(truth)})
    (out / "bench.json").write_text(json.dumps(res, indent=1), encoding="utf-8")
    print(json.dumps(res, indent=1))


# ----------------------------------------------------------------------------- main
def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("src")
    ap.add_argument("out")
    ap.add_argument("--workers", type=int, default=3)
    ap.add_argument("--threads", type=int, default=2)
    ap.add_argument("--max-side", type=int, default=2000)
    ap.add_argument("--bench", action="store_true")
    a = ap.parse_args()
    src, out = Path(a.src).expanduser().resolve(), Path(a.out).expanduser().resolve()
    out.mkdir(parents=True, exist_ok=True)
    photos = sorted(p for p in src.iterdir() if p.suffix.lower() in IMG_EXT and p.is_file())
    if a.bench:
        bench(photos, out)
        return
    t_start = time.perf_counter()
    print(f"[plan] {len(photos)} photos · {a.workers} workers × {a.threads} ORT threads · max side {a.max_side}px", flush=True)
    tasks = [{"src": str(p), "out": str(out), "threads": a.threads, "max_side": a.max_side} for p in photos]
    results = []
    with mp.get_context("spawn").Pool(a.workers) as pool:
        for r in pool.imap_unordered(process_photo, tasks):
            results.append(r)
            print(f"[{time.perf_counter() - t_start:5.1f}s] {len(results)}/{len(tasks)} {r['file']} {r['status']} "
                  f"{r['timings_ms']}", flush=True)
    t_workers = time.perf_counter() - t_start
    results.sort(key=lambda r: r["file"])
    (out / "photos").mkdir(exist_ok=True)
    # header lines that recur on most photos (carrier, form captions) are boilerplate, not titles
    hdr_count = Counter(t for r in results if r["status"] == "ok"
                        for t in {norm(it["text"]) for it in r["items"] if it["region"] in ("header", "free") and it["kind"] == "text"})
    boiler = frozenset(t for t, c in hdr_count.items() if c >= max(2, 0.5 * len(results)))
    index_rows, merged, manifest = [], [], []
    for r in results:
        if r["status"] != "ok":
            manifest.append({"file": r["file"], "status": r["status"], "trace": r.get("trace")})
            continue
        md, title = build_photo_md(r, f"../previews/{r['stem']}.jpg", boiler)
        (out / "photos" / f"{r['stem']}.md").write_text(md, encoding="utf-8")
        md_root, _ = build_photo_md(r, f"previews/{r['stem']}.jpg", boiler)
        merged.append(md_root)
        ts = truth_score(r)
        side = {k: r[k] for k in ("file", "size", "meta", "deskew_deg", "segmentation", "field_labels", "instances", "tables",
                                  "checks", "flags", "timings_ms")}
        side["items"] = [{k: v for k, v in it.items() if k in ("text", "conf", "quad", "kind", "fmt", "ink", "where", "region",
                                                               "inst", "edge", "role")} for it in r["items"]]
        side["truth"] = ts
        (out / "photos" / f"{r['stem']}.json").write_text(json.dumps(side, indent=1, ensure_ascii=False), encoding="utf-8")
        texts = [it for it in r["items"] if it["kind"] == "text"]
        n_bc = sum(1 for it in r["items"] if it["kind"] == "barcode")
        manifest.append({"file": r["file"], "status": "ok", "title": title, "lines": len(texts),
                         "mean_conf": round(sum(it["conf"] for it in texts) / max(1, len(texts)), 4),
                         "low_conf": sum(1 for it in texts if it["conf"] < LOW_CONF),
                         "stickers": len(r["instances"]), "barcodes": n_bc,
                         "barcodes_match_printed": sum(1 for b in r["checks"]["barcodes"] if b["printed_digits_match"]),
                         "stickers_matched_to_table": sum(1 for s in r["checks"]["stickers"].values() if s.get("summary_row")),
                         "qty_consistent": sum(1 for s in r["checks"]["stickers"].values() if s.get("qty_consistent")),
                         "flags": len(r["flags"]), "truth": ts, "timings_ms": r["timings_ms"], "segmentation": r["segmentation"],
                         "markdown": f"photos/{r['stem']}.md", "json": f"photos/{r['stem']}.json", "preview": f"previews/{r['stem']}.jpg"})
        index_rows.append((r, title))
    tree = [f"turbomergerspottest/  ({len(photos)} photos)"]
    for i, (r, title) in enumerate(index_rows):
        last = i == len(index_rows) - 1
        m = next(x for x in manifest if x["file"] == r["file"])
        tree.append(f"{'└──' if last else '├──'} {r['file']}  —  {title}")
        tree.append(f"{'    ' if last else '│   '}    {m['stickers']} stickers · {m['lines']} lines · conf {m['mean_conf']:.3f} · "
                    f"{m['barcodes']} barcodes · → {m['markdown']}")
    idx = ["# Photo OCR spot test — index", "", f"Source: `{src.name}/` · generated {time.strftime('%Y-%m-%d %H:%M')}", "",
           "```text", *tree, "```", "",
           "| # | Photo | Title (top header line) | Stickers | Lines | Mean conf. | Low-conf. | Barcodes ✓/read | Stickers ✓ table | Flags | Check strings | Markdown |",
           "|---|---|---|---|---|---|---|---|---|---|---|---|"]
    for k, m in enumerate([m for m in manifest if m["status"] == "ok"], 1):
        tr = m["truth"]
        idx.append(f"| {k} | `{m['file']}` | {md_cell(m['title'])} | {m['stickers']} | {m['lines']} | {m['mean_conf']:.3f} | {m['low_conf']} | "
                   f"{m['barcodes_match_printed']}/{m['barcodes']} | {m['stickers_matched_to_table']}/{m['stickers']} | {m['flags']} | "
                   f"{tr['hits']}/{tr['n'] if tr else '—'} | [md]({m['markdown']}) |")
    (out / "INDEX.md").write_text("\n".join(idx) + "\n", encoding="utf-8")
    head = ["# Label photos — merged (TurboMerger spot test #2)", "", "```text", *tree, "```", "", "---", ""]
    (out / "MERGED_PHOTOS.md").write_text("\n".join(head) + "\n\n---\n\n".join(merged), encoding="utf-8")
    ok = [m for m in manifest if m["status"] == "ok"]
    summary = {"generated": time.strftime("%Y-%m-%dT%H:%M:%S%z"), "photos": len(photos), "ok": len(ok),
               "lines": sum(m["lines"] for m in ok), "stickers": sum(m["stickers"] for m in ok),
               "barcodes": sum(m["barcodes"] for m in ok), "barcodes_match_printed": sum(m["barcodes_match_printed"] for m in ok),
               "stickers_matched_to_table": sum(m["stickers_matched_to_table"] for m in ok),
               "truth_hits": sum(m["truth"]["hits"] for m in ok if m["truth"]), "truth_n": sum(m["truth"]["n"] for m in ok if m["truth"]),
               "wall_s": round(time.perf_counter() - t_start, 2), "workers_s": round(t_workers, 2),
               "config": {"workers": a.workers, "threads": a.threads, "max_side": a.max_side}}
    (out / "manifest.json").write_text(json.dumps({"summary": summary, "photos": manifest}, indent=1, ensure_ascii=False), encoding="utf-8")
    print(json.dumps(summary, indent=1))


if __name__ == "__main__":
    main()
