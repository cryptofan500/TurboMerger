# /// script
# requires-python = ">=3.10"
# dependencies = ["pdf_oxide>=0.3.78", "pypdfium2>=5.13", "pillow>=11", "tiktoken>=0.9"]
# ///
"""TurboMerger spot test #1 — arXiv PDFs → organized, LLM-ready bundle.

Reference prototype for the Rust port (NOT TurboMerger code). Every stage maps
to a planned tm-core / tm-extract component:
  discover → metadata (inventory.tsv > PDF info > first-page heuristic)
  → per-paper worker (process pool, longest-first scheduling)
      text per page (pdf_oxide, artifacts removed, reading order kept)
      headings from the PDF outline
      caption detection → figure/table region (vector paths + images + text lines)
      render (PDFium) → crop → PNG; full-page fallback, never silent
  → per-paper Markdown, INDEX tree with titles, DIGEST, merged file, parts,
    contact sheets, manifest.json, QUALITY_REPORT.md
Usage: python arxiv_spottest.py <pdf_root> <out_dir> [--limit N] [--workers N]
"""
from __future__ import annotations

import argparse
import hashlib
import io
import json
import math
import multiprocessing as mp
import os
import re
import sys
import tempfile
import time
import unicodedata
from collections import Counter, defaultdict
from dataclasses import dataclass, field, asdict
from pathlib import Path

RENDER_DPI = 150
PART_TOKENS = 140_000          # o200k budget per web-LLM part (≈ 180-190k Claude-4.7 tokens)
THUMB_W = 380

KIND_ALT = r"(?i:figure|fig\.?|table|tab\.)"
CAPTION_RE = re.compile(
    r"^\s*(?P<kind>" + KIND_ALT + r")\s*(?P<num>\d+|[IVXLC]{1,6}\b|[A-Z]\.?\d+|S\d+)"
    r"(?P<sep>\s*[:.|—–]\s*|\s+(?=[A-Z(\[“\"']))"
)
CAPTION_PREFILTER_RE = re.compile(KIND_ALT + r"\s*(\d+|[IVXLC]{1,6}\b|[A-Z]\.?\d+|S\d+)")   # anywhere: a mid-line caption must not hide a page
REFERENCE_VERB_RE = re.compile(r"^\s*" + KIND_ALT + r"\s*\S+\s+(shows|show|illustrates|presents|depicts|compares|"
                               r"summarizes|summarises|reports|plots|lists|gives|contains|displays|highlights|provides)\b", re.I)
REF_RE = re.compile(r"\b(?i:(fig(?:ure)?s?\.?|tables?|tab\.))\s*~?(\d+)\b")
ARXIV_STAMP_RE = re.compile(r"^arXiv:\d{4}\.\d{4,5}(v\d+)?\s*\[[^\]]+\]\s*\d{1,2}\s+\w{3}\s+\d{4}\s*$")
PAGE_NUM_RE = re.compile(r"^\s*(\d{1,3}|[ivxlc]{1,6})\s*$", re.I)
LIGATURES = {"\ufb00": "ff", "\ufb01": "fi", "\ufb02": "fl", "\ufb03": "ffi", "\ufb04": "ffl", "\ufb05": "st", "\ufb06": "st"}


# ----------------------------------------------------------------------------- helpers
def slugify(text: str, maxlen: int = 60) -> str:
    t = unicodedata.normalize("NFKD", text).encode("ascii", "ignore").decode()
    t = re.sub(r"[^A-Za-z0-9]+", "-", t).strip("-").lower()
    return t[:maxlen].rstrip("-") or "untitled"


def clean_text(s: str) -> str:
    for k, v in LIGATURES.items():
        s = s.replace(k, v)
    s = s.replace("\ufffe", "").replace("\u00ad", "")
    return s


def norm_key(s: str) -> str:
    return re.sub(r"[^0-9a-z]+", "", s.lower())


def md_escape_cell(s: str) -> str:
    return s.replace("|", "\\|").replace("\n", " ").strip()


def first_sentence(s: str, limit: int = 220) -> str:
    s = " ".join(s.split())
    s = re.sub(r"^(?i:figure|fig\.?|table|tab\.)\s*\S+?\s*[:.|—–]?\s+", "", s, count=1)
    m = re.search(r"(.+?[.!?])(\s|$)", s[: limit + 40])
    out = m.group(1) if m else s[:limit]
    return out if len(out) <= limit else out[: limit - 1] + "…"


def reflow(lines: list[str]) -> list[str]:
    """Join hard-wrapped lines into paragraphs; keep list items, short lines, equations."""
    paras, cur = [], []
    for raw in lines:
        line = raw.rstrip()
        if not line.strip():
            if cur:
                paras.append(" ".join(cur))
                cur = []
            continue
        starts_block = bool(re.match(r"^\s*([-•▪◦*]|\d+[.)]|\(\w\)|[A-Z]\.\s)", line))
        if cur and (starts_block or len(cur[-1]) < 45 and cur[-1].endswith((".", ":", "?", "!"))):
            paras.append(" ".join(cur))
            cur = []
        if cur and cur[-1].endswith("-") and line[:1].islower():
            cur[-1] = cur[-1][:-1] + line.strip()
        else:
            cur.append(line.strip())
    if cur:
        paras.append(" ".join(cur))
    return paras


# ----------------------------------------------------------------------------- metadata
def load_inventory(root: Path) -> dict[str, dict]:
    inv = {}
    p = root / "inventory.tsv"
    if p.exists():
        rows = p.read_text(encoding="utf-8").splitlines()
        hdr = rows[0].split("\t")
        for r in rows[1:]:
            cols = r.split("\t")
            if len(cols) == len(hdr):
                d = dict(zip(hdr, cols))
                inv[d["pdf"]] = d
    return inv


# ----------------------------------------------------------------------------- geometry
@dataclass
class Box:
    x0: float
    top: float
    x1: float
    bottom: float
    fs: float = 0.0          # dominant font size for text segments (0 for graphics)

    @property
    def w(self):
        return self.x1 - self.x0

    @property
    def h(self):
        return self.bottom - self.top

    def xoverlap(self, o: "Box") -> float:
        return max(0.0, min(self.x1, o.x1) - max(self.x0, o.x0))

    def union(self, o: "Box") -> "Box":
        return Box(min(self.x0, o.x0), min(self.top, o.top), max(self.x1, o.x1), max(self.bottom, o.bottom))

    def contains_center(self, o: "Box") -> bool:
        cx, cy = (o.x0 + o.x1) / 2, (o.top + o.bottom) / 2
        return self.x0 - 1 <= cx <= self.x1 + 1 and self.top - 1 <= cy <= self.bottom + 1


def to_box(b, H) -> Box:
    x, y, w, h = b
    return Box(float(x), float(H - (y + h)), float(x + w), float(H - y))


def build_segments(words, H):
    """Column-aware text segments from word boxes: group words by baseline, then
    split a row wherever the horizontal gap exceeds ~1.1 em (column gutters, table cells)."""
    items = []
    for w in words:
        t = clean_text(w.text).strip()
        if not t:
            continue
        b = to_box(w.bbox, H)
        if b.h <= 0 or b.w <= 0:
            continue
        items.append((b, t, float(getattr(w, "font_size", 0) or b.h)))
    items.sort(key=lambda it: ((it[0].top + it[0].bottom) / 2, it[0].x0))
    rows = []
    for it in items:
        cy = (it[0].top + it[0].bottom) / 2
        if rows and abs(cy - rows[-1]["cy"]) <= 0.45 * max(it[0].h, rows[-1]["h"]):
            rows[-1]["items"].append(it)
            rows[-1]["h"] = max(rows[-1]["h"], it[0].h)
        else:
            rows.append({"cy": cy, "h": it[0].h, "items": [it]})
    segs = []
    for r in rows:
        its = sorted(r["items"], key=lambda it: it[0].x0)
        cur = [its[0]]
        for it in its[1:]:
            gap = it[0].x0 - cur[-1][0].x1
            if gap > max(1.1 * max(cur[-1][2], it[2], 6.0), 8.0):
                segs.append(cur)
                cur = [it]
            else:
                cur.append(it)
        segs.append(cur)
    out = []
    for s in segs:
        b = s[0][0]
        for it in s[1:]:
            b = b.union(it[0])
        sizes = sorted(it[2] for it in s)
        b.fs = sizes[len(sizes) // 2]
        out.append((b, " ".join(it[1] for it in s)))
    return out


def find_regions(lines, graphics, W, H):
    """Caption-anchored figure/table regions (engine-agnostic: boxes + text in, boxes out).

    v2: regions GROW outward from the caption through contiguous content only
    (graphics + non-body text such as axis labels), so stray logos, headers and
    unrelated art further away are not swallowed. The nearest body-text line is a
    hard barrier; the crop never pads into the caption or the barrier line.
    """
    body_h = sorted(b.h for b, t in lines if 5 < b.h < 20)
    med_h = body_h[len(body_h) // 2] if body_h else 10.0
    wide = [b.w for b, t in lines if b.w > 0.25 * W]
    med_w = sorted(wide)[len(wide) // 2] if wide else 0.45 * W
    rules = [g for g in graphics if g.h <= 2.0 and g.w >= 20]
    vrules = [g for g in graphics if g.w <= 2.0 and g.h >= 15]
    solids = [g for g in graphics if not (g.h <= 2.0 or g.w <= 2.0) and not (g.w > 0.95 * W and g.h > 0.9 * H)]
    shapes = rules + vrules + solids
    maxgap = max(30.0, 3.0 * med_h)

    def overlaps_graphic(b: Box) -> bool:
        return any(g.xoverlap(b) > 0 and not (g.bottom < b.top or g.top > b.bottom) for g in solids)

    def framed(b: Box) -> bool:
        cy = (b.top + b.bottom) / 2
        left = any(v.x1 <= b.x0 + 2 and v.top - 2 <= cy <= v.bottom + 2 for v in vrules)
        right = any(v.x0 >= b.x1 - 2 and v.top - 2 <= cy <= v.bottom + 2 for v in vrules)
        return left and right

    # table zones: spans between horizontal rules that share the same x-extent (booktabs top/mid/bottom)
    groups = []
    for r in sorted(rules, key=lambda r: r.top):
        for g in groups:
            if abs(g[0].x0 - r.x0) <= 3 and abs(g[0].x1 - r.x1) <= 3:
                g.append(r)
                break
        else:
            groups.append([r])
    cap_boxes = [b for b, t in lines if CAPTION_RE.match(t)]
    zones = []
    for g in groups:                                  # split stacked tables: a caption between two rules ends a zone
        sub = [g[0]]
        for r in g[1:]:
            prev = sub[-1]
            if any(prev.bottom - 1 <= c.top and c.bottom <= r.top + 1 and c.xoverlap(r) > 0 for c in cap_boxes):
                if len(sub) >= 2:
                    zones.append(Box(sub[0].x0, sub[0].top, sub[0].x1, sub[-1].bottom))
                sub = [r]
            else:
                sub.append(r)
        if len(sub) >= 2 and sub[-1].bottom - sub[0].top < 0.9 * H:
            zones.append(Box(sub[0].x0, sub[0].top, sub[0].x1, sub[-1].bottom))

    def between_rules(b: Box) -> bool:
        return any(z.top - 1 <= b.top and b.bottom <= z.bottom + 1 and z.xoverlap(b) >= 0.5 * b.w for z in zones)

    wide_fs = sorted(b.fs for b, t in lines if b.w > 0.25 * W and b.fs > 0)
    med_fs = wide_fs[len(wide_fs) // 2] if wide_fs else 0.0

    def plain_body(b: Box, t: str) -> bool:
        size_ok = abs(b.fs - med_fs) <= 0.8 if (b.fs and med_fs) else abs(b.h - med_h) <= 0.35 * med_h
        return (b.w >= 0.6 * med_w and size_ok and not overlaps_graphic(b)
                and not framed(b) and not between_rules(b) and not CAPTION_RE.match(t))

    body_idx = {i for i, (b, t) in enumerate(lines) if plain_body(b, t)}
    # a short line that ends a paragraph (aligned under a body line) is body too
    for i, (b, t) in enumerate(lines):
        if i in body_idx or overlaps_graphic(b) or CAPTION_RE.match(t):
            continue
        for j in list(body_idx):
            a = lines[j][0]
            if 0 <= b.top - a.bottom < 0.6 * med_h and abs(b.x0 - a.x0) <= 3 and abs(b.h - a.h) <= 0.35 * med_h:
                body_idx.add(i)
                break

    body_ids = {id(lines[i][0]) for i in body_idx}

    def is_body(b: Box, t: str) -> bool:
        return id(b) in body_ids

    def union_all(els):
        u = Box(els[0].x0, els[0].top, els[0].x1, els[0].bottom)
        for g in els[1:]:
            u = u.union(g)
        return u

    caps = []
    for b, t in lines:
        m = CAPTION_RE.match(t)
        if m and not REFERENCE_VERB_RE.match(t):
            kind = "table" if m.group("kind").lower().startswith("tab") else "figure"
            caps.append((b, t, kind, m.group("num")))
    out = []
    text_top = min((b.top for b, t in lines), default=0.0)

    def band_for(cb: Box) -> Box:
        if cb.w > 0.55 * W or (cb.x0 < 0.45 * W and cb.x1 > 0.55 * W):
            return Box(0, 0, W, H)
        return Box(0, 0, 0.52 * W, H) if cb.x1 <= 0.55 * W else Box(0.48 * W, 0, W, H)

    def block_for(cb: Box, band: Box) -> Box:
        # caption block: following lines with the caption's font size and alignment
        block = Box(cb.x0, cb.top, cb.x1, cb.bottom, cb.fs)
        for b, t in sorted(lines, key=lambda x: x[0].top):
            if (b.top >= block.bottom - 1 and b.top - block.bottom < 0.8 * med_h and band.xoverlap(b) > 0.5 * b.w
                    and not CAPTION_RE.match(t) and not between_rules(b)
                    and (cb.fs == 0 or b.fs == 0 or abs(b.fs - cb.fs) <= 1.0)
                    and (abs(b.x0 - cb.x0) <= 14 or abs((b.x0 + b.x1) / 2 - (cb.x0 + cb.x1) / 2) <= 14)):
                block = block.union(b)
        return block

    all_blocks = [block_for(cb, band_for(cb)) for cb, _, _, _ in caps]

    def in_any_block(b: Box) -> bool:                 # every caption (incl. continuation lines) is a barrier
        return any(k.top - 1 <= b.top and b.bottom <= k.bottom + 1 and k.xoverlap(b) > 0.5 * b.w for k in all_blocks)

    for ci, (cb, ct, kind, num) in enumerate(caps):
        band = band_for(cb)
        block = all_blocks[ci]

        def candidates():
            c = [g for g in shapes if band.xoverlap(g) >= 0.5 * max(g.w, 1)]
            c += [b for b, t in lines if band.xoverlap(b) > 0.5 * b.w and not is_body(b, t)
                  and not CAPTION_RE.match(t) and not in_any_block(b)]
            return c

        def grow_up():
            barrier = 0.0
            for b, t in lines:
                if b.bottom <= cb.top - 1 and band.xoverlap(b) > 0.5 * b.w and (is_body(b, t) or CAPTION_RE.match(t) or in_any_block(b)):
                    barrier = max(barrier, b.bottom)
            cand = sorted((c for c in candidates() if c.bottom <= cb.top + 2 and c.top >= barrier - 1), key=lambda c: -c.bottom)
            chosen, frontier = [], cb.top
            for c in cand:
                if c.bottom >= frontier - maxgap:
                    chosen.append(c)
                    frontier = min(frontier, c.top)
                else:
                    break
            return barrier, chosen

        def grow_down(start):
            limit = H
            for b, t in lines:
                if b.top >= start + 1 and band.xoverlap(b) > 0.5 * b.w and (CAPTION_RE.match(t) or is_body(b, t) or in_any_block(b)):
                    limit = min(limit, b.top)
            cand = sorted((c for c in candidates() if c.top >= start - 2 and c.bottom <= limit + 2), key=lambda c: c.top)
            chosen, frontier = [], start
            for c in cand:
                if c.top <= frontier + maxgap:
                    chosen.append(c)
                    frontier = max(frontier, c.bottom)
                else:
                    break
            return limit, chosen

        region, conf, how, side, floor = None, "low", "", "above", 0.0
        cands_all = candidates()
        gap_up = min((cb.top - c.bottom for c in cands_all if c.bottom <= cb.top + 2), default=1e9)
        gap_down = min((c.top - block.bottom for c in cands_all if c.top >= block.bottom - 2), default=1e9)
        if kind == "figure":
            order = ("down", "up") if gap_down < 0.5 * gap_up else ("up", "down")
        else:
            order = ("up", "down") if gap_up <= gap_down else ("down", "up")
        for direction in order:
            if direction == "up":
                barrier, chosen = grow_up()
                n_shapes = sum(1 for c in chosen if c.fs == 0)
                if n_shapes:
                    u = union_all(chosen)
                    region, side, floor = Box(u.x0, max(u.top, barrier), u.x1, cb.top), "above", barrier
                    conf = "high" if n_shapes >= 3 else "medium"
                    how = "grow-up"
                    break
            else:
                limit, chosen = grow_down(block.bottom)
                n_shapes = sum(1 for c in chosen if c.fs == 0)
                if n_shapes:
                    u = union_all(chosen)
                    region, side = Box(min(u.x0, cb.x0), block.bottom, max(u.x1, cb.x1), u.bottom), "below"
                    conf = "high" if n_shapes >= 2 else "medium"
                    how = "grow-down"
                    break
        if region is None and kind == "figure":
            barrier, chosen = grow_up()
            if chosen and cb.top - barrier >= 40:        # text-only figure (listing, prompt box)
                u = union_all(chosen)
                region, side, floor, conf, how = Box(u.x0, max(u.top, barrier), u.x1, cb.top), "above", barrier, "low", "text-above"
        if region is not None:
            if side == "below":                          # table notes (†, ‡, small font) directly under the table
                for b, t in sorted(lines, key=lambda s: s[0].top):
                    if (b.top >= region.bottom - 1 and b.top - region.bottom < 1.6 * med_h and b.h < 0.95 * med_h
                            and band.xoverlap(b) > 0.5 * b.w and not CAPTION_RE.match(t) and not is_body(b, t)):
                        region = region.union(b)
            pad = 4
            x0, x1 = max(band.x0, region.x0 - pad), min(band.x1, region.x1 + pad)
            if side == "above":                          # never pad into the caption or the barrier line
                top, bottom = max(floor + 0.5, region.top - pad), min(region.bottom, cb.top - 0.5)
            else:
                top, bottom = max(region.top, block.bottom + 0.5), min(H, region.bottom + pad)
            region = Box(x0, top, x1, bottom)
            if region.h < 18 or region.w < 50 or region.h > 0.98 * H:
                region, conf, how = None, "low", how + "+rejected"
        continues = bool(region and side == "above" and region.top <= text_top + 1.5 * med_h)
        out.append({"kind": kind, "num": num, "caption_line": ct, "caption_box": asdict(cb), "caption_block": asdict(block),
                    "region": asdict(region) if region else None, "confidence": conf, "method": how or "none",
                    "may_continue_from_previous_page": continues})
    return out


def xy_order(items, W):
    """Reading order for (Box, payload) items: full-width items split the page into
    horizontal bands; inside a band, a two-column layout is read left column then right."""
    items = sorted(items, key=lambda it: (it[0].top, it[0].x0))
    out, band = [], []

    def full(b: Box) -> bool:
        return b.w > 0.55 * W or (b.x0 < 0.45 * W and b.x1 > 0.55 * W)

    def flush():
        if not band:
            return
        left = [it for it in band if it[0].x1 <= 0.55 * W]
        right = [it for it in band if it[0].x0 >= 0.45 * W and it not in left]
        rest = [it for it in band if it not in left and it not in right]
        if len(left) >= 2 and len(right) >= 2:
            out.extend(sorted(left + rest, key=lambda it: (it[0].top, it[0].x0)))
            out.extend(sorted(right, key=lambda it: (it[0].top, it[0].x0)))
        else:
            out.extend(sorted(band, key=lambda it: (it[0].top, it[0].x0)))
        band.clear()

    for it in items:
        if full(it[0]):
            flush()
            out.append(it)
        else:
            band.append(it)
    flush()
    return out


def join_segments(segs) -> str:
    """Join caption/figure segments into one clean string (dehyphenated)."""
    txt = ""
    for b, t in sorted(segs, key=lambda s: (s[0].top, s[0].x0)):
        if txt.endswith("-") and t[:1].islower():
            txt = txt[:-1] + t
        else:
            txt = (txt + " " + t).strip()
    return txt


# ----------------------------------------------------------------------------- worker
def process_pdf(task: dict) -> dict:
    """Runs in a worker process. Never raises: failures are returned as records."""
    t0 = time.perf_counter()
    rel, root, out = task["rel"], Path(task["root"]), Path(task["out"])
    pid = task["id"]
    res = {"rel": rel, "id": pid, "status": "ok", "timings": {}, "warnings": [], "figures": [], "pages": 0}
    err_fd = tempfile.TemporaryFile(mode="w+b")
    saved = os.dup(2)
    os.dup2(err_fd.fileno(), 2)                         # capture native-library warnings per file
    try:
        import pdf_oxide
        import pypdfium2 as pdfium
        from PIL import Image

        path = str(root / rel)
        doc = pdf_oxide.PdfDocument(path)
        n = doc.page_count()
        res["pages"] = n
        pdoc = pdfium.PdfDocument(path)
        meta = {}
        try:
            meta = {k: v for k, v in pdoc.get_metadata_dict().items() if v}
        except Exception:
            pass
        res["pdf_meta_title"] = meta.get("Title", "")
        t = time.perf_counter()
        outline = []
        try:
            def walk(items, lvl):
                for it in items or []:
                    outline.append({"title": clean_text(it.get("title", "")).strip(), "page": it.get("page"), "level": lvl})
                    walk(it.get("children"), lvl + 1)
            walk(doc.get_outline(), 0)
        except Exception as e:
            res["warnings"].append(f"outline: {e}")
        res["outline"] = outline
        pages_text = []
        for p in range(n):
            try:
                pages_text.append(clean_text(doc.extract_text(p, include_artifacts=False)))
            except Exception as e:
                pages_text.append("")
                res["warnings"].append(f"page {p+1} text: {e}")
        res["timings"]["text_ms"] = round((time.perf_counter() - t) * 1000)

        # first-page title heuristic (used when no inventory / metadata title)
        try:
            lines0 = doc.extract_text_lines(0, include_artifacts=False)
            H0 = doc.page_media_box(0)[3]
            top = [l for l in lines0 if l.bbox[1] > H0 * 0.45 and len(l.text.strip()) > 8]
            if top:
                mh = max(l.bbox[3] for l in top)
                res["heuristic_title"] = " ".join(clean_text(l.text.strip()) for l in top if l.bbox[3] >= 0.85 * mh)[:250]
        except Exception:
            res["heuristic_title"] = ""

        # figure / table regions
        t = time.perf_counter()
        fig_dir = out / "figures" / pid
        fig_dir.mkdir(parents=True, exist_ok=True)
        seen = set()
        render_ms = 0
        page_blocks = {}
        for p in range(n):
            if not CAPTION_PREFILTER_RE.search(pages_text[p]):
                continue
            mb = doc.page_media_box(p)
            W, H = float(mb[2] - mb[0]), float(mb[3] - mb[1])
            try:
                lines = build_segments(doc.extract_words(p, include_artifacts=False), H)
                graphics = [to_box(pth["bbox"], H) for pth in doc.extract_paths(p)]
                graphics += [to_box(im["bbox"], H) for im in doc.extract_images(p)]
            except Exception as e:
                res["warnings"].append(f"page {p+1} geometry: {e}")
                continue
            regs = [r for r in find_regions(lines, graphics, W, H) if (r["kind"], r["num"]) not in seen]
            if not regs:
                continue
            # single geometric pass: every segment gets exactly one owner
            owner = {}
            for r in regs:
                rb = Box(**r["region"]) if r["region"] else None
                cbk = Box(**r["caption_block"])
                key = (r["kind"], r["num"])
                for i, (b, tx) in enumerate(lines):
                    if i in owner:
                        continue
                    if cbk.contains_center(b):
                        owner[i] = ("cap", key)
                    elif rb is not None and rb.contains_center(b):
                        owner[i] = ("fig", key)
                for r2 in [r]:
                    r2["caption_text"] = join_segments([lines[i] for i, o in owner.items() if o == ("cap", key)])
                    r2["inner_text"] = "\n".join(tx for b, tx in sorted((lines[i] for i, o in owner.items() if o == ("fig", key)),
                                                                        key=lambda s: (s[0].top, s[0].x0)))
            items = [(b, ("text", tx)) for i, (b, tx) in enumerate(lines) if i not in owner]
            for r in regs:
                u = Box(**r["caption_block"])
                if r["region"]:
                    u = u.union(Box(**r["region"]))
                items.append((u, ("fig", (r["kind"], r["num"]))))
            ordered = xy_order(items, W)
            blocks, prev, prev_t = [], None, ""
            for b, (kind_, payload) in ordered:
                if kind_ == "fig":
                    blocks.append(["fig", payload[0], payload[1]])
                    prev = None
                    continue
                if prev is not None:
                    new_col = b.top < prev.top - 1
                    ends = prev_t.rstrip().endswith((".", "!", "?", ":"))
                    if (not new_col and b.top - prev.bottom > 0.9 * max(b.h, prev.h)) or (new_col and ends):
                        blocks.append(["text", ""])        # paragraph break (a column change continues the sentence)
                blocks.append(["text", payload])
                prev, prev_t = b, payload
            page_blocks[p + 1] = blocks
            tr = time.perf_counter()
            scale = RENDER_DPI / 72.0
            page_img = pdoc[p].render(scale=scale).to_pil().convert("RGB")
            render_ms += (time.perf_counter() - tr) * 1000
            page_file = None
            for r in regs:
                seen.add((r["kind"], r["num"]))
                label = f"{'fig' if r['kind']=='figure' else 'tab'}-{r['num'].replace('.', '_')}"
                if r["region"]:
                    b = r["region"]
                    crop = page_img.crop((int(b["x0"] * scale), int(b["top"] * scale), int(math.ceil(b["x1"] * scale)), int(math.ceil(b["bottom"] * scale))))
                    fn = f"{label}.png"
                    crop.save(fig_dir / fn, optimize=True)
                    r.update(file=fn, crop_px=list(crop.size))
                    if r.get("may_continue_from_previous_page") and p > 0:
                        Hp = H
                        try:
                            Hp = float(doc.page_media_box(p - 1)[3])
                            low = max((to_box(x["bbox"], Hp).bottom for x in doc.extract_paths(p - 1) + doc.extract_images(p - 1)), default=0.0)
                        except Exception:
                            low = Hp
                        if low < 0.82 * Hp:                 # previous page does not end in graphics: not a continuation
                            r["may_continue_from_previous_page"] = False
                    if r.get("may_continue_from_previous_page") and p > 0:
                        prev_fn = f"page-{p:03d}.png"
                        if not (fig_dir / prev_fn).exists():
                            prev = pdoc[p - 1].render(scale=scale).to_pil().convert("RGB")
                            prev.thumbnail((1100, 1400))
                            prev.save(fig_dir / prev_fn, optimize=True)
                        r["also_page_file"] = prev_fn
                else:
                    if page_file is None:
                        page_file = f"page-{p+1:03d}.png"
                        thumb = page_img.copy()
                        thumb.thumbnail((1100, 1400))
                        thumb.save(fig_dir / page_file, optimize=True)
                    r.update(file=page_file, crop_px=None, fallback="full page (region not isolated)")
                r["page"] = p + 1
                res["figures"].append(r)
        res["timings"]["figures_ms"] = round((time.perf_counter() - t) * 1000)
        res["timings"]["render_ms"] = round(render_ms)
        res["pages_text"] = pages_text
        res["page_blocks"] = page_blocks
    except Exception as e:  # noqa: BLE001 - worker must report, not die
        res["status"] = f"error: {type(e).__name__}: {e}"
    finally:
        os.dup2(saved, 2)
        os.close(saved)
        err_fd.seek(0)
        native = [l for l in err_fd.read().decode("utf-8", "replace").splitlines() if l.strip()]
        err_fd.close()
        if native:
            c = Counter(native)
            res["warnings"] += [f"native×{k}: {v}" for v, k in c.most_common(5)]
    res["timings"]["total_ms"] = round((time.perf_counter() - t0) * 1000)
    return res


# ----------------------------------------------------------------------------- markdown assembly
def build_paper_md(r: dict, info: dict, fig_prefix: str) -> tuple[str, dict]:
    """Structured Markdown for one paper. fig_prefix is the relative path to figures/."""
    title = info["title"]
    pages_text: list[str] = r.get("pages_text", [])
    figs_by_page = defaultdict(list)
    for f in r["figures"]:
        figs_by_page[f["page"]].append(f)
    n_fig = sum(1 for f in r["figures"] if f["kind"] == "figure")
    n_tab = sum(1 for f in r["figures"] if f["kind"] == "table")
    # running header/footer lines repeated on many pages (after digit normalisation)
    edge = Counter()
    for pt in pages_text:
        ls = [l.strip() for l in pt.splitlines() if l.strip()]
        for l in ls[:2] + ls[-2:]:
            edge[re.sub(r"\d+", "#", l)] += 1
    repeated = {k for k, v in edge.items() if v >= max(3, 0.4 * len(pages_text)) and len(k) < 120}
    outline = r.get("outline", [])
    headings_by_page = defaultdict(list)
    for o in outline:
        if o["page"] is not None and o["title"]:
            headings_by_page[o["page"] + 1].append(o)

    md = []
    authors = info.get("authors", "")
    md.append(f"# {title}\n")
    md.append("| arXiv | Authors | Version date | Pages | Figures | Tables | Source PDF |")
    md.append("|---|---|---|---|---|---|---|")
    arx = info.get("arxiv_id", "")
    arx_cell = f"[{arx}](https://arxiv.org/abs/{arx})" if arx else "—"
    md.append(f"| {arx_cell} | {md_escape_cell(authors) or '—'} | {info.get('date') or '—'} | {r['pages']} | {n_fig} | {n_tab} | `{info['src']}` |\n")
    if info.get("models"):
        md.append(f"> **Models named:** {info['models']}  ·  **Collection note:** {info.get('note','')}\n")
    if outline:
        md.append("## Contents\n")
        for o in outline:
            pg = f" · p. {o['page']+1}" if o["page"] is not None else ""
            md.append(f"{'  ' * o['level']}- {o['title']}{pg}")
        md.append("")
    if r["figures"]:
        md.append("## Figures & tables at a glance\n")
        md.append("| Preview | Label | Page | Caption |")
        md.append("|---|---|---|---|")
        for f in sorted(r["figures"], key=lambda f: (f["page"], f["kind"], f["num"])):
            lab = f"{'Figure' if f['kind']=='figure' else 'Table'} {f['num']}"
            img = f'<img src="{fig_prefix}{r["id"]}/{f["file"]}" width="220">'
            note = " *(full page)*" if f.get("fallback") else ""
            md.append(f"| {img} | **{lab}**{note} | {f['page']} | {md_escape_cell(first_sentence(f.get('caption_text') or f['caption_line']))} |")
        md.append("")
    md.append("## Full text\n")
    page_blocks = r.get("page_blocks", {})
    for i, pt in enumerate(pages_text, start=1):
        md.append(f"*— page {i} —*\n")
        blocks = page_blocks.get(i)
        if blocks is None:                            # no figures here: engine reading order is good
            raw_lines = pt.splitlines()
        else:                                         # figure page: single geometric pass, figures as markers
            raw_lines = [f"\x00FIG\x00{b[1]}\x00{b[2]}" if b[0] == "fig" else b[1] for b in blocks]
        lines = []
        for l in raw_lines:
            s = l.strip()
            if s.startswith("\x00FIG"):
                lines.append(s)
                continue
            if not s:
                lines.append("")
                continue
            if ARXIV_STAMP_RE.match(s) or re.sub(r"\d+", "#", s) in repeated or (PAGE_NUM_RE.match(s) and len(s) <= 3):
                continue
            lines.append(l)
        # headings from outline
        hs = headings_by_page.get(i, [])
        out_lines, pending = [], list(hs)
        for l in lines:
            s = l.strip()
            hit = None
            if s.startswith("\x00FIG"):
                out_lines.append(s)
                continue
            for o in pending:
                k = norm_key(o["title"])
                sk = norm_key(re.sub(r"^\s*([A-Z]|\d+)(\.\d+)*\.?\s+", "", s))
                if k and (sk == k or norm_key(s) == k or (norm_key(s).endswith(k) and len(s) <= len(o["title"]) + 8)):
                    hit = o
                    break
            if hit:
                pending.remove(hit)
                out_lines.append("")
                out_lines.append(f"{'#' * min(6, 3 + hit['level'])} {s}")
                out_lines.append("")
            else:
                out_lines.append(l)
        for o in pending:                               # heading text not found on the page: keep it anyway
            out_lines.insert(0, f"{'#' * min(6, 3 + o['level'])} {o['title']}")
        # paragraphs + figure blocks at caption positions
        paras = []
        buf = []
        for l in out_lines:
            if l.startswith("#") or l.startswith("\x00FIG"):
                paras += reflow(buf)
                buf = []
                paras.append(l)
            else:
                buf.append(l)
        paras += reflow(buf)
        placed = set()
        for para in paras:
            if para.startswith("\x00FIG"):
                _, _, kind, num = para.split("\x00")
                fig = next((f for f in figs_by_page.get(i, []) if (f["kind"], f["num"]) == (kind, num)), None)
                if fig:
                    placed.add((kind, num))
                    md.append(render_fig_block(fig, r["id"], fig_prefix, fig.get("caption_text") or fig["caption_line"]))
                continue
            md.append(para + ("\n" if not para.startswith("#") else ""))
        for f in figs_by_page.get(i, []):               # safety net: never drop a detected figure
            if (f["kind"], f["num"]) not in placed:
                md.append(render_fig_block(f, r["id"], fig_prefix, f.get("caption_text") or f.get("caption_line", "")))
    stats = {"figures": n_fig, "tables": n_tab, "outline_entries": len(outline)}
    return "\n".join(md) + "\n", stats


def render_fig_block(f: dict, pid: str, prefix: str, caption: str) -> str:
    lab = f"{'Figure' if f['kind']=='figure' else 'Table'} {f['num']}"
    cap = re.sub(r"^\s*" + KIND_ALT + r"\s*\S+?\s*[:.|—–]?\s+", "", caption, count=1).strip()
    lines = [f"![{lab} (p. {f['page']})]({prefix}{pid}/{f['file']})", "",
             f"> **{lab}.** {cap}  ", f"> *p. {f['page']} · crop: {f['confidence']} ({f['method']})"
             + (" · shown as full page — region not isolated" if f.get("fallback") else "")
             + (f" · may start on the previous page → [page {f['page'] - 1}]({prefix}{pid}/{f['also_page_file']})" if f.get("also_page_file") else "")
             + "*", ""]
    inner = " · ".join(x.strip() for x in (f.get("inner_text") or "").splitlines() if x.strip())
    if inner:
        lines += [f"<details><summary>Text inside {lab} (labels, axes, cells)</summary>", "", inner[:4000], "", "</details>", ""]
    return "\n".join(lines)


# ----------------------------------------------------------------------------- contact sheets
def contact_sheet(out: Path, r: dict, title: str):
    from PIL import Image, ImageDraw, ImageFont
    figs = [f for f in sorted(r["figures"], key=lambda f: (f["page"], f["kind"], f["num"])) if f.get("file")]
    if not figs:
        return None
    try:
        font = ImageFont.truetype("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", 15)
        bold = ImageFont.truetype("/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf", 17)
    except OSError:
        font = bold = ImageFont.load_default()
    cols = 3 if len(figs) > 4 else max(1, min(len(figs), 2))
    tiles = []
    for f in figs:
        im = Image.open(out / "figures" / r["id"] / f["file"]).convert("RGB")
        im.thumbnail((THUMB_W, 300))
        tiles.append((f, im))
    cell_w, cell_h = THUMB_W + 20, 300 + 48
    rows = math.ceil(len(tiles) / cols)
    sheet = Image.new("RGB", (cols * cell_w + 20, rows * cell_h + 70), "white")
    d = ImageDraw.Draw(sheet)
    d.text((12, 10), f"{r['id']} — {title[:110]}", fill="black", font=bold)
    d.text((12, 36), f"{len(figs)} figures/tables · TurboMerger spot test", fill=(90, 90, 90), font=font)
    for i, (f, im) in enumerate(tiles):
        cx, cy = 10 + (i % cols) * cell_w, 70 + (i // cols) * cell_h
        d.rectangle([cx, cy, cx + cell_w - 10, cy + cell_h - 10], outline=(200, 200, 200))
        sheet.paste(im, (cx + 5 + (THUMB_W - im.width) // 2, cy + 5))
        lab = f"{'Figure' if f['kind']=='figure' else 'Table'} {f['num']} · p.{f['page']}" + (" · full page" if f.get("fallback") else "")
        d.text((cx + 8, cy + cell_h - 38), lab, fill="black", font=font)
    fn = out / "figures" / r["id"] / "contact-sheet.jpg"
    sheet.save(fn, quality=85, optimize=True)
    return fn


# ----------------------------------------------------------------------------- main
def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("root")
    ap.add_argument("out")
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--workers", type=int, default=max(1, (os.cpu_count() or 2) - 2))
    ap.add_argument("--eta-after", type=float, default=60.0)
    a = ap.parse_args()
    root, out = Path(a.root).expanduser().resolve(), Path(a.out).expanduser().resolve()
    root_label = root.name  # relative label only: outputs never carry absolute/home paths (N-21)
    out.mkdir(parents=True, exist_ok=True)
    t_start = time.perf_counter()
    inv = load_inventory(root)
    pdfs = sorted(p for p in root.rglob("*") if p.suffix.lower() == ".pdf" and p.is_file())
    if a.limit:
        pdfs = pdfs[: a.limit]
    import pypdfium2 as pdfium
    tasks, page_total = [], 0
    for p in pdfs:
        rel = p.relative_to(root).as_posix()
        try:
            npg = len(pdfium.PdfDocument(str(p)))
        except Exception:
            npg = 1
        page_total += npg
        tasks.append({"rel": rel, "root": str(root), "out": str(out), "id": p.stem, "pages_hint": npg, "bytes": p.stat().st_size})
    tasks.sort(key=lambda t: -t["pages_hint"])            # longest-processing-time first
    print(f"[plan] {len(tasks)} PDFs · {page_total} pages · {a.workers} workers · inventory rows {len(inv)}", flush=True)

    results, done_pages, last_print, hist = [], 0, 0.0, []
    ctx = mp.get_context("spawn")
    with ctx.Pool(a.workers, maxtasksperchild=8) as pool:
        for r in pool.imap_unordered(process_pdf, tasks):
            results.append(r)
            done_pages += r.get("pages", 0) or 1
            now = time.perf_counter() - t_start
            hist.append((now, done_pages))
            win = [h for h in hist if h[0] >= now - 15] or hist
            rate = (win[-1][1] - win[0][1]) / max(1e-6, win[-1][0] - win[0][0]) if len(win) > 1 else done_pages / max(now, 1e-6)
            if now - last_print > 3 or len(results) == len(tasks):
                eta = (page_total - done_pages) / rate if rate > 0 else float("inf")
                tag = "ETA" if now >= a.eta_after else "eta"
                print(f"[{now:6.1f}s] {len(results)}/{len(tasks)} PDFs · {done_pages}/{page_total} pages · "
                      f"{rate:5.1f} pages/s · {tag} {eta:5.1f}s", flush=True)
                last_print = now
    t_workers = time.perf_counter() - t_start

    # ---------------- assemble outputs
    import tiktoken
    enc = tiktoken.get_encoding("o200k_base")
    results.sort(key=lambda r: r["rel"])
    papers = []
    for r in results:
        rel = r["rel"]
        iv = inv.get(rel, {})
        note = "in inventory" if iv else "not in inventory (excluded search candidate)" if "_excluded" in rel else "not in inventory"
        title = iv.get("title") or (r.get("pdf_meta_title") or "").strip() or r.get("heuristic_title") or r["id"]
        tsrc = "inventory.tsv" if iv.get("title") else ("PDF metadata" if (r.get("pdf_meta_title") or "").strip() else "first-page heuristic")
        month = rel.split("/")[0] if "/" in rel else "."
        slug = f"{r['id']}__{slugify(title)}"
        info = {"title": title, "title_source": tsrc, "authors": iv.get("authors", ""), "date": iv.get("latest_version_submission", ""),
                "models": iv.get("verified_model_names", ""), "arxiv_id": iv.get("arxiv_id", "") or (r["id"] if re.match(r"^\d{4}\.\d{4,5}$", r["id"]) else ""),
                "src": f"{root_label}/{rel}", "note": note, "month": month, "slug": slug}
        papers.append((r, info))

    (out / "papers").mkdir(exist_ok=True)
    merged_chunks, manifest, digest = [], [], []
    for r, info in papers:
        if r["status"] != "ok":
            manifest.append({"pdf": r["rel"], "status": r["status"], "title": info["title"]})
            continue
        pdir = out / "papers" / info["month"]
        pdir.mkdir(parents=True, exist_ok=True)
        md_local, stats = build_paper_md(r, info, "../../figures/")
        (pdir / f"{info['slug']}.md").write_text(md_local, encoding="utf-8")
        md_root, _ = build_paper_md(r, info, "figures/")
        merged_chunks.append((info, md_root))
        cs = contact_sheet(out, r, info["title"])
        toks = len(enc.encode(md_local, disallowed_special=()))
        info.update(stats, tokens=toks, md=f"papers/{info['month']}/{info['slug']}.md", sheet=(cs.relative_to(out).as_posix() if cs else None))
        crops = sum(1 for f in r["figures"] if not f.get("fallback"))
        referenced = set()
        for pt in r["pages_text"]:
            for m in REF_RE.finditer(pt):
                referenced.add(("table" if m.group(1).lower().startswith("tab") else "figure", m.group(2)))
        detected = {(f["kind"], f["num"]) for f in r["figures"]}
        top_num = {k: max([int(n) for kk, n in detected if kk == k and n.isdigit()] or [0]) for k in ("figure", "table")}
        missing = sorted((k for k in referenced - detected if 1 <= int(k[1]) <= (top_num[k[0]] + 3 if top_num[k[0]] else 30)
                          and not (k[0] == "table" and any(kk == "table" and not n.isdigit() for kk, n in detected))),
                         key=lambda k: (k[0], int(k[1])))
        manifest.append({"pdf": r["rel"], "status": r["status"], "title": info["title"], "title_source": info["title_source"],
                         "pages": r["pages"], "tokens_o200k": toks, "figures": stats["figures"], "tables": stats["tables"],
                         "referenced_not_detected": [f"{k} {n}" for k, n in missing],
                         "crops": crops, "fallback_pages": len(r["figures"]) - crops, "outline_entries": stats["outline_entries"],
                         "timings_ms": r["timings"], "warnings": r["warnings"], "markdown": info["md"], "contact_sheet": info["sheet"],
                         "figures_detail": [{k: v for k, v in f.items() if k in ("kind", "num", "page", "file", "confidence", "method", "fallback", "crop_px")} for f in r["figures"]]})
        # digest entry
        text_all = "\n".join(r["pages_text"])
        abs_m = re.search(r"(?is)\babstract\b[.:—\s]*(.+?)(?:\n\s*(?:1|I)\.?\s+Introduction|\n\s*Introduction\s*\n|\n\s*Keywords|\Z)", text_all)
        abstract = " ".join((abs_m.group(1) if abs_m else "").split())[:2500]
        concl = ""
        ol = r.get("outline", [])
        for idx, o in enumerate(ol):
            if re.search(r"conclu|discussion|limitation|summary", o["title"], re.I) and o["page"] is not None:
                start = text_all.find(o["title"].split()[-1])
                seg = "\n".join(r["pages_text"][o["page"]:o["page"] + 2])
                j = seg.lower().find(o["title"].lower()[:30])
                concl = " ".join(seg[j if j >= 0 else 0:].split())[:1800]
        dg = [f"## {info['title']}", "",
              f"`{info['arxiv_id'] or r['id']}` · {info['authors'][:160] or '—'} · {info['date'] or '—'} · {r['pages']} pp · "
              f"{stats['figures']} figures · {stats['tables']} tables · [full text]({info['md']})", ""]
        if abstract:
            dg += ["**Abstract.** " + abstract, ""]
        if ol:
            dg += ["**Sections:** " + " · ".join(o["title"] for o in ol if o["level"] == 0)[:900], ""]
        if r["figures"]:
            dg += ["**Figures & tables:**", ""]
            for f in sorted(r["figures"], key=lambda f: (f["kind"], f["page"])):
                lab = f"{'Fig.' if f['kind']=='figure' else 'Tab.'} {f['num']}"
                dg.append(f"- {lab} (p. {f['page']}) — {first_sentence(f.get('caption_text') or f['caption_line'], 200)} [img](figures/{r['id']}/{f['file']})")
            dg.append("")
        if concl:
            dg += ["**Conclusion (excerpt).** " + concl, ""]
        digest.append("\n".join(dg))

    # ---------------- INDEX (tree with titles + paths) and tables
    by_month = defaultdict(list)
    for r, info in papers:
        by_month[info["month"]].append((r, info))
    tot_pages = sum(r["pages"] for r, _ in papers)
    tot_figs = sum(info.get("figures", 0) for _, info in papers)
    tot_tabs = sum(info.get("tables", 0) for _, info in papers)
    tree = [f"arxiv/  ({len(papers)} PDFs · {tot_pages:,} pages · {tot_figs} figures · {tot_tabs} tables)"]
    months = sorted(by_month)
    for mi, m in enumerate(months):
        last_m = mi == len(months) - 1
        tree.append(f"{'└──' if last_m else '├──'} {m}/  ({len(by_month[m])} papers)")
        items = by_month[m]
        for ii, (r, info) in enumerate(items):
            pre = "    " if last_m else "│   "
            last_i = ii == len(items) - 1
            tree.append(f"{pre}{'└──' if last_i else '├──'} {Path(r['rel']).name}  —  {info['title']}")
            tree.append(f"{pre}{'    ' if last_i else '│   '}    {r['pages']} pp · {info.get('figures',0)} fig · {info.get('tables',0)} tab · → {info.get('md','(failed)')}")
    index = ["# arXiv spot test — index", "",
             f"Source: `{root_label}/` · generated {time.strftime('%Y-%m-%d %H:%M')} · titles from inventory.tsv (fallback: PDF metadata, then first-page heuristic)", "",
             "## Tree (title and path for every paper)", "", "```text", *tree, "```", "",
             "## Table", "", "| # | arXiv | Title | Pages | Fig | Tab | ≈tokens | Markdown | Contact sheet |", "|---|---|---|---|---|---|---|---|---|"]
    for k, (r, info) in enumerate(papers, 1):
        sheet = f"[sheet]({info['sheet']})" if info.get("sheet") else "—"
        index.append(f"| {k} | {info['arxiv_id'] or r['id']} | {md_escape_cell(info['title'])} | {r['pages']} | {info.get('figures',0)} | {info.get('tables',0)} | "
                     f"{info.get('tokens',0):,} | [md]({info.get('md','')}) | {sheet} |")
    (out / "INDEX.md").write_text("\n".join(index) + "\n", encoding="utf-8")

    # ---------------- DIGEST (+ parts)
    dig_head = ["# arXiv digest — abstracts, sections, figure/table captions, conclusions", "",
                "One entry per paper. Use this file for cross-paper critique; open the linked full text for detail.", "", "---", ""]
    digest_md = "\n".join(dig_head) + "\n\n---\n\n".join(digest) + "\n"
    (out / "DIGEST.md").write_text(digest_md, encoding="utf-8")
    dig_tokens = len(enc.encode(digest_md, disallowed_special=()))

    def split_parts(chunks, name, header_fn):
        parts, cur, cur_t = [], [], 0
        for c in chunks:
            ct = len(enc.encode(c, disallowed_special=()))
            if cur and cur_t + ct > PART_TOKENS:
                parts.append(cur)
                cur, cur_t = [], 0
            cur.append(c)
            cur_t += ct
        if cur:
            parts.append(cur)
        pd = out / "parts"
        pd.mkdir(exist_ok=True)
        files = []
        for i, p in enumerate(parts, 1):
            fn = pd / f"{name}.part{i:02d}-of-{len(parts):02d}.md"
            fn.write_text(header_fn(i, len(parts)) + "\n\n".join(p), encoding="utf-8")
            files.append(fn)
        return files

    dig_parts = split_parts(digest, "DIGEST", lambda i, n: f"# arXiv digest — part {i} of {n}\n\nWait for all {n} parts before answering.\n\n---\n\n") if dig_tokens > PART_TOKENS else []

    # ---------------- MERGED single file (+ parts)
    merged_head = ["# arXiv corpus — merged full text (TurboMerger spot test)", "",
                   f"{len(papers)} PDFs · {tot_pages:,} pages · {tot_figs} figures · {tot_tabs} tables. Figures are referenced as images under `figures/`; "
                   "text found inside figures is kept in collapsible blocks under each figure.", "",
                   "## Tree", "", "```text", *tree, "```", "", "---", ""]
    merged_md = "\n".join(merged_head) + "\n\n---\n\n".join(md for _, md in merged_chunks)
    (out / "MERGED_FULLTEXT.md").write_text(merged_md, encoding="utf-8")
    merged_tokens = len(enc.encode(merged_md, disallowed_special=()))
    full_parts = split_parts([md for _, md in merged_chunks], "FULLTEXT",
                             lambda i, n: (f"# arXiv corpus — full text, part {i} of {n}\n\nWait for all {n} parts before answering. "
                                           "Part 1 carries the index tree.\n\n" + ("```text\n" + "\n".join(tree) + "\n```\n\n" if i == 1 else "") + "---\n\n"))

    # ---------------- figure index
    fi = ["# Figure & table index", "", "| Paper | Label | Page | Caption | Image |", "|---|---|---|---|---|"]
    for r, info in papers:
        for f in sorted(r.get("figures", []), key=lambda f: (f["page"], f["kind"])):
            lab = f"{'Figure' if f['kind']=='figure' else 'Table'} {f['num']}"
            fi.append(f"| {info['arxiv_id'] or r['id']} | {lab} | {f['page']} | {md_escape_cell(first_sentence(f.get('caption_text') or f['caption_line'], 160))} | "
                      f"[{'crop' if not f.get('fallback') else 'page'}]({r['id']}/{f['file']}) |")
    (out / "figures" / "INDEX.md").write_text("\n".join(fi) + "\n", encoding="utf-8")

    # ---------------- manifest + quality report
    t_total = time.perf_counter() - t_start
    ok = [m for m in manifest if m["status"] == "ok"]
    n_cap = sum(m["figures"] + m["tables"] for m in ok)
    n_crop = sum(m["crops"] for m in ok)
    conf = Counter(f["confidence"] for m in ok for f in m["figures_detail"] if not f.get("fallback"))
    methods = Counter(f["method"] for m in ok for f in m["figures_detail"])
    n_missing = sum(len(m.get("referenced_not_detected", [])) for m in ok)
    summary = {"generated": time.strftime("%Y-%m-%dT%H:%M:%S%z"), "source": str(root), "pdfs": len(papers), "ok": len(ok),
               "failed": len(papers) - len(ok), "pages": tot_pages, "captions_detected": n_cap, "crops": n_crop,
               "referenced_not_detected": n_missing,
               "papers_with_missing": sum(1 for m in ok if m.get("referenced_not_detected")),
               "fallback_pages": n_cap - n_crop, "crop_confidence": dict(conf), "methods": dict(methods),
               "tokens_o200k": {"merged": merged_tokens, "digest": dig_tokens},
               "parts": {"fulltext": len(full_parts), "digest": len(dig_parts), "part_budget_o200k": PART_TOKENS},
               "timing_s": {"workers": round(t_workers, 1), "total": round(t_total, 1), "workers_n": a.workers},
               "worker_ms_sum": {k: sum(m["timings_ms"].get(k, 0) for m in ok) for k in ("text_ms", "figures_ms", "render_ms", "total_ms")}}
    (out / "manifest.json").write_text(json.dumps({"summary": summary, "papers": manifest}, indent=1, ensure_ascii=False), encoding="utf-8")
    print(json.dumps(summary, indent=1), flush=True)


if __name__ == "__main__":
    main()
