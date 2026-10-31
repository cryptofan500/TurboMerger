#!/usr/bin/env python3
"""tm_verify.py — independent verifier for TurboMerger output.

Checks what TurboMerger wrote against the source files with code that shares
nothing with the Rust encoder: openpyxl / the csv module read the sources, a
strict stdlib decoder reads the merged Markdown, and the slicer is re-implemented
from the written spec (ADR 0020 / 0021, `crates/tm-core/src/slice.rs` docs).

Subcommands (exit 0 = pass, 5 = a gate failed, 2 = usage):
  grid       --merged M.md --src-root DIR [--selection SEL.toml]
             every workbook/CSV grid: sheet list, used range, header row, columns,
             every cell (row, column, type, text) incl. header and preamble cells,
             counts, per-sheet SHA-256, merged ranges, formula counts, table syntax.
  slices     --merged M.md --src-root DIR --selection SEL.toml
             every sliced text file re-cut from the source and compared byte for byte
             (kept text and every [tm:…] marker).
  accounting --json M.json --selection SEL.toml
             every planned file present, nothing not captured.
  cold       --merged M.md --src-root DIR --work DIR
             only M.md and this script go to an empty folder; workbooks are rebuilt
             there from the Markdown alone; the rebuilt cells must equal the originals.
  restore    ARCHIVE.md --out DIR [--src-root DIR]
             archive twin → files at their relative paths; size + SHA-256 per manifest.
  decode     M.md --out DIR [--xlsx]      (used by `cold`; no source access)
  audit      --merged M.md --src-root DIR --out AUDIT.md
             side-by-side sample (source value vs decoded value) for human reading.
  selftest   [--bin TURBOMERGER]
             builds adversarial fixtures, runs the real binary, requires a clean pass,
             then requires each of 14 deliberate corruptions to FAIL.
  all        --merged M.md --json M.json --src-root DIR --selection SEL.toml --report R.md

Run with `python3 -P` (openpyxl may be a user-site package; -P keeps the script
folder off sys.path).
"""
import argparse
import base64
import csv
import datetime as dt
import hashlib
import io
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import unicodedata
import zipfile
from decimal import Decimal

try:  # Python 3.11+
    import tomllib
except ImportError:  # pragma: no cover
    tomllib = None

EXIT_OK, EXIT_USAGE, EXIT_GATE = 0, 2, 5
EMPTY_SHA = hashlib.sha256(b"").hexdigest()

RE_NUM = re.compile(r"^-?(0|[1-9][0-9]*)(\.[0-9]+)?$")
RE_BOOL = re.compile(r"^(TRUE|FALSE)$")
RE_ERR = re.compile(r"^(#NULL!|#DIV/0!|#VALUE!|#REF!|#NAME\?|#NUM!|#N/A|#GETTING_DATA|#DATA!)$")
RE_DATE = re.compile(r"^(\d{4}-\d{2}-\d{2}( \d{2}:\d{2}:\d{2}(\.\d{3})?)?|-?\d+:\d{2}:\d{2}(\.\d{3})?)$")


class Gate:
    """Collects failures for one check; prints them and keeps counters."""

    def __init__(self, name):
        self.name, self.fails, self.notes, self.counts = name, [], [], {}

    def fail(self, msg):
        self.fails.append(msg)

    def note(self, msg):
        self.notes.append(msg)

    def add(self, key, n=1):
        self.counts[key] = self.counts.get(key, 0) + n

    @property
    def ok(self):
        return not self.fails

    def report(self, limit=25):
        status = "PASS" if self.ok else "FAIL"
        counts = ", ".join(f"{k}={v}" for k, v in sorted(self.counts.items()))
        lines = [f"[{status}] {self.name}: {counts}"]
        lines += [f"    FAIL {m}" for m in self.fails[:limit]]
        if len(self.fails) > limit:
            lines.append(f"    … {len(self.fails) - limit} more failures")
        lines += [f"    note {m}" for m in self.notes[:limit]]
        return "\n".join(lines)


# ----------------------------------------------------------------- shared rules

def json_lit(s):
    out = ['"']
    for c in s:
        o = ord(c)
        if c == '"':
            out.append('\\"')
        elif c == "\\":
            out.append("\\\\")
        elif c == "\n":
            out.append("\\n")
        elif c == "\r":
            out.append("\\r")
        elif c == "\t":
            out.append("\\t")
        elif c == "\b":
            out.append("\\b")
        elif c == "\f":
            out.append("\\f")
        elif o < 0x20:
            out.append("\\u%04x" % o)
        else:
            out.append(c)
    out.append('"')
    return "".join(out)


def canon_sha(cells):
    """cells: {(row, col1): (kind, text)} → (count, sha256 of the canonical dump)."""
    buf = "".join(f"{r}\t{c}\t{k}\t{json_lit(t)}\n" for (r, c), (k, t) in sorted(cells.items()))
    return len(cells), hashlib.sha256(buf.encode("utf-8")).hexdigest()


def looks_typed(t):
    return bool(RE_NUM.match(t) or RE_BOOL.match(t) or RE_ERR.match(t) or RE_DATE.match(t))


def escape_cell(text, marker):
    out = ["'"] if marker and (text.startswith("'") or looks_typed(text)) else []
    i = 0
    while i < len(text):
        if text.startswith("<br>", i):
            out.append("\\<br>")
            i += 4
            continue
        c = text[i]
        o = ord(c)
        if c == "\\":
            out.append("\\\\")
        elif c == "|":
            out.append("\\|")
        elif c == "\n":
            out.append("<br>")
        elif c == "\r":
            out.append("\\r")
        elif c == "\t":
            out.append("\\t")
        elif o < 0x20 or o == 0x7F:
            out.append("\\x%02x" % o)
        else:
            out.append(c)
        i += 1
    return "".join(out)


def decode_field(field, typed):
    """Escaped cell text (no pad) → (kind, text) or None for an empty cell."""
    if field == "":
        return None
    quoted = typed and field.startswith("'")
    body = field[1:] if quoted else field
    out, i = [], 0
    while i < len(body):
        if body.startswith("<br>", i):
            out.append("\n")
            i += 4
            continue
        c = body[i]
        if c == "\\":
            nxt = body[i + 1:i + 2]
            if nxt in ("\\", "|", "<"):
                out.append(nxt)
                i += 2
            elif nxt == "r":
                out.append("\r")
                i += 2
            elif nxt == "t":
                out.append("\t")
                i += 2
            elif nxt == "x" and re.match(r"[0-9a-f]{2}", body[i + 2:i + 4]):
                out.append(chr(int(body[i + 2:i + 4], 16)))
                i += 4
            else:
                raise ValueError(f"bad escape in {field!r}")
            continue
        if c == "|":
            raise ValueError(f"unescaped pipe in {field!r}")
        out.append(c)
        i += 1
    text = "".join(out)
    if not typed or quoted:
        return ("s", text)
    if RE_NUM.match(text):
        return ("n", text)
    if RE_BOOL.match(text):
        return ("b", text)
    if RE_ERR.match(text):
        return ("e", text)
    if RE_DATE.match(text):
        return ("d", text)
    return ("s", text)


def split_row(line):
    """'| a | b\\|c |' → [' a ', ' b\\|c '] (split on unescaped pipes only)."""
    if not (line.startswith("|") and line.endswith("|")) or len(line) < 2:
        raise ValueError(f"table line must start and end with '|': {line[:80]!r}")
    fields, cur, i, body = [], [], 0, line[1:]
    while i < len(body):
        c = body[i]
        if c == "\\" and i + 1 < len(body):
            cur.append(body[i:i + 2])
            i += 2
            continue
        if c == "|":
            fields.append("".join(cur))
            cur = []
        else:
            cur.append(c)
        i += 1
    if cur:
        raise ValueError(f"text after the last pipe: {line[-40:]!r}")
    return fields


def unpad(field):
    if len(field) < 2 or field[0] != " " or field[-1] != " ":
        raise ValueError(f"cell must be padded with exactly one space each side: {field!r}")
    return field[1:-1]


def col_letter(i):  # 0-based
    s = ""
    i += 1
    while i:
        i, r = divmod(i - 1, 26)
        s = chr(65 + r) + s
    return s


def col_index(s):  # 0-based, None if not letters
    if not s or len(s) > 3 or not all("A" <= ch <= "Z" for ch in s):
        return None
    n = 0
    for ch in s:
        n = n * 26 + ord(ch) - 64
    return n - 1


def expand_cols(spec):
    if spec in ("-", ""):
        return []
    out = []
    for part in spec.split(","):
        a, _, b = part.partition("-")
        lo, hi = col_index(a), col_index(b or a)
        out += list(range(lo, hi + 1))
    return out


def parse_meta(line, prefix):
    """'@sheet 1/2 name="x" a=b' → ({'name': 'x', 'a': 'b'}, '1/2')."""
    rest = line[len(prefix):].lstrip(" ")
    head, _, rest = rest.partition(" ") if not rest.startswith(("name=", "body_rows=", "sheet=")) else ("", "", rest)
    meta, i = {}, 0
    dec = json.JSONDecoder()
    while i < len(rest):
        j = rest.index("=", i)
        key = rest[i:j]
        if rest[j + 1:j + 2] == '"':
            val, end = dec.raw_decode(rest, j + 1)
        else:
            end = rest.find(" ", j + 1)
            end = len(rest) if end < 0 else end
            val = rest[j + 1:end]
        meta[key] = val
        i = end + 1 if end < len(rest) and rest[end:end + 1] == " " else end
    return meta, head


# ------------------------------------------------------------- merged Markdown

def md_blocks(text):
    """Every '## path' + fenced block → list of dict(path, lang, content, line)."""
    lines = text.split("\n")
    out, i = [], 0
    while i < len(lines):
        ln = lines[i]
        if ln.startswith("## ") and i + 2 < len(lines) and lines[i + 1] == "":
            m = re.match(r"^(`{3,})(.*)$", lines[i + 2])
            if m:
                fence, lang = m.group(1), m.group(2).strip()
                j = i + 3
                while j < len(lines) and lines[j] != fence:
                    j += 1
                out.append({"path": ln[3:], "lang": lang, "content": "\n".join(lines[i + 3:j]), "line": i + 1})
                i = j + 1
                continue
        i += 1
    return out


def decode_grid(content, gate, where):
    """Strict TM-GRID/1 decoder → {'file': meta, 'sheets': [sheet...]}."""
    lines = content.split("\n")
    if not lines or lines[0] != "TM-GRID/1":
        gate.fail(f"{where}: first line is not TM-GRID/1")
        return None
    fmeta, _ = parse_meta(lines[1], "@file")
    sheets, i = [], 2
    while i < len(lines):
        ln = lines[i]
        if ln == "":
            i += 1
            continue
        if not ln.startswith("@sheet "):
            gate.fail(f"{where}: unexpected line {i + 1}: {ln[:80]!r}")
            return None
        meta, pos = parse_meta(ln, "@sheet")
        sh = {"meta": meta, "pos": pos, "cells": {}, "header_fields": None, "body_rows": [], "errors": []}
        sheets.append(sh)
        i += 1
        if "skipped" in meta:
            if i >= len(lines) or not lines[i].startswith("@end ") or not lines[i].endswith(" skipped"):
                gate.fail(f"{where}: skipped sheet {meta.get('name')!r} lacks its '@end … skipped'")
            i += 1
            continue
        typed = meta.get("types") == "typed"
        cols = expand_cols(meta.get("cols", "-"))
        hr = meta.get("header_row", "none")
        header_line = None
        ended = False
        while i < len(lines):
            ln = lines[i]
            try:
                if ln.startswith("@end "):
                    emeta, _ = parse_meta(ln, "@end")
                    sh["end"] = emeta
                    ended = True
                    i += 1
                    break
                if ln.startswith("> "):
                    m = re.match(r"^> ([A-Z]{1,3})(\d+): (.*)$", ln)
                    if not m:
                        raise ValueError(f"bad preamble line {ln[:80]!r}")
                    v = decode_field(m.group(3), typed)
                    if v is None:
                        raise ValueError("empty preamble cell")
                    sh["cells"][(int(m.group(2)), col_index(m.group(1)) + 1)] = v
                elif ln.startswith("@cont "):
                    pass
                elif ln == "":
                    pass
                elif ln.startswith("| row |"):
                    if header_line is None:
                        header_line = ln
                        fields = split_row(ln)
                        if len(fields) != len(cols) + 1:
                            raise ValueError(f"header has {len(fields) - 1} cells, cols= says {len(cols)}")
                        sh["header_fields"] = [unpad(f) for f in fields[1:]]
                        if hr != "none":
                            for k, f in enumerate(sh["header_fields"]):
                                v = decode_field(f, typed)
                                if v is not None:
                                    sh["cells"][(int(hr), cols[k] + 1)] = v
                    elif ln != header_line:
                        raise ValueError("a repeated header differs from the first one")
                elif ln.startswith("| ---: |"):
                    if len(split_row(ln)) != len(cols) + 1:
                        raise ValueError("delimiter row has the wrong cell count")
                elif ln.startswith("| "):
                    fields = split_row(ln)
                    if len(fields) != len(cols) + 1:
                        raise ValueError(f"row has {len(fields) - 1} cells, cols= says {len(cols)}: {ln[:80]!r}")
                    rown = int(unpad(fields[0]))
                    if sh["body_rows"] and rown <= sh["body_rows"][-1]:
                        raise ValueError(f"row {rown} out of order")
                    sh["body_rows"].append(rown)
                    for k, f in enumerate(fields[1:]):
                        v = decode_field(unpad(f), typed)
                        if v is not None:
                            sh["cells"][(rown, cols[k] + 1)] = v
                else:
                    raise ValueError(f"unexpected line {ln[:80]!r}")
            except (ValueError, IndexError) as e:
                sh["errors"].append(f"line {i + 1}: {e}")
            i += 1
        if not ended:
            gate.fail(f"{where}: sheet {meta.get('name')!r} has no @end (cut off)")
    return {"file": fmeta, "sheets": sheets}


def check_grid_self(g, gate, where):
    """Counts and hashes inside the Markdown agree with themselves."""
    for sh in g["sheets"]:
        m, name = sh["meta"], sh["meta"].get("name")
        if "skipped" in m:
            continue
        for e in sh["errors"]:
            gate.fail(f"{where} [{name}] {e}")
        n, sha = canon_sha(sh["cells"])
        end = sh.get("end", {})
        if str(n) != m.get("cells") or str(n) != end.get("cells"):
            gate.fail(f"{where} [{name}] decoded {n} cells, meta says {m.get('cells')} / end {end.get('cells')}")
        if sha != m.get("sha256") or sha != end.get("sha256"):
            gate.fail(f"{where} [{name}] decoded sha256 {sha[:12]} ≠ meta {m.get('sha256', '')[:12]} / end {end.get('sha256', '')[:12]}")
        if str(len(sh["body_rows"])) != m.get("body_rows") or m.get("body_rows") != end.get("body_rows"):
            gate.fail(f"{where} [{name}] {len(sh['body_rows'])} body rows, meta says {m.get('body_rows')}")


# ------------------------------------------------------------------- oracles

def canon_py(v, data_type=None):
    """openpyxl value → (kind, text) or None (the written TM-GRID/1 rules)."""
    if v is None:
        return None
    if isinstance(v, bool):
        return ("b", "TRUE" if v else "FALSE")
    if isinstance(v, int):
        return ("n", str(v))
    if isinstance(v, float):
        if v != v or v in (float("inf"), float("-inf")):
            return ("e", "#NUM!")
        if v == 0:
            return ("n", "0")  # -0.0 reads as 0 (Excel shows 0)
        t = format(Decimal(repr(v)), "f")
        return ("n", t[:-2] if t.endswith(".0") else t)
    if isinstance(v, dt.datetime):
        ms = v.microsecond // 1000
        frac = f".{ms:03d}" if ms else ""
        if (v.hour, v.minute, v.second, ms) == (0, 0, 0, 0):
            return ("d", v.strftime("%Y-%m-%d"))
        return ("d", v.strftime("%Y-%m-%d %H:%M:%S") + frac)
    if isinstance(v, dt.date):
        return ("d", v.strftime("%Y-%m-%d"))
    if isinstance(v, dt.time):
        ms = v.microsecond // 1000
        return ("d", v.strftime("%H:%M:%S") + (f".{ms:03d}" if ms else ""))
    if isinstance(v, dt.timedelta):
        total_ms = round(abs(v.total_seconds()) * 1000)
        h, rem = divmod(total_ms, 3_600_000)
        m, rem = divmod(rem, 60_000)
        s, ms = divmod(rem, 1000)
        return ("d", f"{'-' if v.total_seconds() < 0 else ''}{h}:{m:02d}:{s:02d}" + (f".{ms:03d}" if ms else ""))
    s = str(v)
    if s == "":
        return None
    if data_type == "e":
        return ("e", s)
    return ("s", s)


def oracle_workbook(path):
    """{name: {'state', 'kind', 'cells', 'merged', 'formulas'}} in sheet order."""
    import openpyxl  # noqa: PLC0415
    wb = openpyxl.load_workbook(path, data_only=True)
    try:
        wbf = openpyxl.load_workbook(path, data_only=False)
    except Exception:  # noqa: BLE001
        wbf = None
    out = {}
    for name in wb.sheetnames:
        ws = wb[name]
        state = {"veryHidden": "veryhidden"}.get(ws.sheet_state, ws.sheet_state)
        if ws.__class__.__name__ != "Worksheet":
            out[name] = {"state": state, "kind": "chartsheet", "cells": {}, "merged": set(), "formulas": 0}
            continue
        cells = {}
        for row in ws.iter_rows():
            for c in row:
                v = canon_py(c.value, c.data_type)
                if v is not None:
                    cells[(c.row, c.column)] = v
        formulas = 0
        if wbf is not None:
            for row in wbf[name].iter_rows():
                for c in row:
                    if c.data_type == "f":
                        formulas += 1
        merged = {str(r) for r in ws.merged_cells.ranges}
        out[name] = {"state": state, "kind": "worksheet", "cells": cells, "merged": merged, "formulas": formulas}
    return out


def oracle_delimited(path, delim):
    with open(path, encoding="utf-8-sig", newline="") as f:
        recs = list(csv.reader(f, delimiter=delim))
    cells = {}
    for r, rec in enumerate(recs, 1):
        for c, v in enumerate(rec, 1):
            if v != "":
                cells[(r, c)] = ("s", v)
    name = os.path.basename(path)
    return {name: {"state": "visible", "kind": "worksheet", "cells": cells, "merged": set(), "formulas": 0, "text": True}}


def glob_match(p, t):
    return re.fullmatch("".join(".*" if ch == "*" else "." if ch == "?" else re.escape(ch) for ch in p), t, re.S) is not None


def guess_header(cells):
    """Independent copy of the header heuristic (typed sheets)."""
    if not cells:
        return None
    cols = [c for _, c in cells]
    width = max(cols) - min(cols) + 1
    need = max(2, -(-width // 2))
    rows = {}
    for (r, _), (k, _) in cells.items():
        n, all_s = rows.get(r, (0, True))
        rows[r] = (n + 1, all_s and k == "s")
    for r in sorted(rows):
        if rows[r][0] >= need and rows[r][1]:
            return r
    return None


def resolve_cols(specs, header, lo, hi):
    """Same documented rules as the encoder: exact header name, letters/ranges, then case-insensitive."""
    out, missing = [], []
    for spec in specs:
        exact = [c for c, t in header.items() if t == spec]
        if len(exact) == 1:
            c = exact[0]
        elif len(exact) > 1:
            missing.append(spec)
            continue
        else:
            a, _, b = spec.partition("-")
            ia, ib = col_index(a), col_index(b or a)
            if ia is not None and ib is not None and ia <= ib and ia >= lo and ib <= hi:
                for c in range(ia, ib + 1):
                    if c not in out:
                        out.append(c)
                continue
            loose = [c for c, t in header.items() if t.strip().lower() == spec.strip().lower()]
            if len(loose) != 1:
                missing.append(spec)
                continue
            c = loose[0]
        if c not in out:
            out.append(c)
    return out, missing


def sheet_pick(entry, name):
    over = next((o for o in entry.get("sheet", []) if o.get("name") == name or glob_match(o.get("name", ""), name)), None)
    pick = {"header_row": entry.get("header_row"), "columns": entry.get("columns", []), "rows": entry.get("rows", [])}
    if over:
        if over.get("header_row") is not None:
            pick["header_row"] = over["header_row"]
        if over.get("columns"):
            pick["columns"] = over["columns"]
        if over.get("rows"):
            pick["rows"] = over["rows"]
    return pick


def rows_ok(specs):
    rng = []
    for s in specs:
        a, _, b = str(s).partition("-")
        rng.append((int(a), int(b or a)))
    return lambda r: not rng or any(a <= r <= b for a, b in rng)


def verify_grid_against_oracle(block, g, entry, src_root, gate):
    rel = block["path"]
    where = rel
    src = os.path.join(src_root, rel)
    if not os.path.isfile(src):
        gate.fail(f"{where}: source not found at {src}")
        return
    ext = os.path.splitext(rel)[1].lower()
    if ext in (".csv", ".tsv", ".tab"):
        oracle = oracle_delimited(src, "\t" if ext != ".csv" else ",")
    else:
        oracle = oracle_workbook(src)
    with open(src, "rb") as f:
        data = f.read()
    fm = g["file"]
    if fm.get("path") != rel:
        gate.fail(f"{where}: @file path={fm.get('path')!r} ≠ block path")
    if fm.get("source_sha256") != hashlib.sha256(data).hexdigest() or fm.get("bytes") != str(len(data)):
        gate.fail(f"{where}: @file bytes/source_sha256 do not match the source on disk")
    names = list(oracle)
    got = [sh["meta"].get("name") for sh in g["sheets"]]
    if got != names:
        gate.fail(f"{where}: sheets {got} ≠ source sheets {names}")
        return
    if fm.get("sheets") != str(len(names)):
        gate.fail(f"{where}: @file sheets={fm.get('sheets')} ≠ {len(names)}")
    entry = entry or {}
    sel_sheets, excl = entry.get("sheets", []), entry.get("exclude_sheets", [])
    for sh in g["sheets"]:
        m, name = sh["meta"], sh["meta"].get("name")
        o = oracle[name]
        gate.add("sheets")
        if m.get("state") != o["state"]:
            gate.fail(f"{where} [{name}] state={m.get('state')} ≠ source {o['state']}")
        selected = (not sel_sheets or any(p == name or glob_match(p, name) for p in sel_sheets)) and not any(
            p == name or glob_match(p, name) for p in excl)
        if "skipped" in m:
            gate.add("sheets_skipped")
            why = m["skipped"]
            if selected and o["kind"] == "worksheet" and not why.startswith("unreadable"):
                gate.fail(f"{where} [{name}] selected sheet was skipped ({why})")
            if not selected and why != "not selected":
                gate.fail(f"{where} [{name}] skip reason {why!r}")
            continue
        if not selected:
            gate.fail(f"{where} [{name}] was emitted but the selection leaves it out")
        cells_all = o["cells"]
        if not cells_all:
            if m.get("cells") != "0":
                gate.fail(f"{where} [{name}] source is empty, meta cells={m.get('cells')}")
            continue
        rows = [r for r, _ in cells_all]
        cols = [c - 1 for _, c in cells_all]
        used = f"{col_letter(min(cols))}{min(rows)}:{col_letter(max(cols))}{max(rows)}"
        if m.get("used") != used:
            gate.fail(f"{where} [{name}] used={m.get('used')} ≠ source {used}")
        pick = sheet_pick(entry, name)
        text_sheet = o.get("text", False)
        if pick["header_row"] is not None:
            hr = int(pick["header_row"])
        elif text_sheet:
            hr = 1 if any(r == 1 for r, _ in cells_all) else None
        else:
            hr = guess_header(cells_all)
        if m.get("header_row") != (str(hr) if hr is not None else "none"):
            gate.fail(f"{where} [{name}] header_row={m.get('header_row')} ≠ expected {hr}")
            continue
        header = {c - 1: t for (r, c), (_, t) in cells_all.items() if r == hr} if hr is not None else {}
        if pick["columns"]:
            want, missing = resolve_cols(pick["columns"], header, min(cols), max(cols))
            if missing:
                gate.fail(f"{where} [{name}] columns not resolvable: {missing}")
        else:
            want = list(range(min(cols), max(cols) + 1))
        if expand_cols(m.get("cols", "-")) != want:
            gate.fail(f"{where} [{name}] cols={m.get('cols')} ≠ expected {','.join(col_letter(c) for c in want)}")
            continue
        keep_row = rows_ok(pick["rows"])
        wantset = {c + 1 for c in want}
        expected = {}
        for (r, c), v in cells_all.items():
            if c not in wantset:
                continue
            if hr is not None and r <= hr:
                expected[(r, c)] = v
            elif keep_row(r):
                expected[(r, c)] = v
        if text_sheet:
            expected = {k: ("s", t) for k, (_, t) in expected.items()}
        decoded = sh["cells"]
        gate.add("cells_source", len(expected))
        gate.add("cells_decoded", len(decoded))
        diff = 0
        for key in sorted(set(expected) | set(decoded)):
            a, b = expected.get(key), decoded.get(key)
            if a == b:
                continue
            diff += 1
            r, c = key
            ref = f"{col_letter(c - 1)}{r}"
            if a is None:
                gate.fail(f"{where} [{name}] {ref}: extra cell {b!r}")
            elif b is None:
                gate.fail(f"{where} [{name}] {ref}: MISSING cell {a!r}")
            elif a[1] == b[1]:
                gate.fail(f"{where} [{name}] {ref}: type {b[0]} ≠ source {a[0]} for {a[1][:60]!r}")
            elif a[1].strip() == b[1] and a[1] != b[1]:
                gate.add("edge_whitespace_lost")
                gate.fail(f"{where} [{name}] {ref}: edge whitespace lost: source {a[1][:60]!r}")
            else:
                gate.fail(f"{where} [{name}] {ref}: text {b[1][:60]!r} ≠ source {a[1][:60]!r}")
        if diff == 0:
            gate.add("sheets_exact")
        n, sha = canon_sha(expected)
        if sha != m.get("sha256"):
            gate.fail(f"{where} [{name}] source-side sha256 {sha[:12]} ≠ meta {m.get('sha256', '')[:12]}")
        if not text_sheet:
            mg = set(filter(None, m.get("merged", "-").replace("-", "").split(";"))) if m.get("merged") != "-" else set()
            if mg != o["merged"]:
                gate.fail(f"{where} [{name}] merged={sorted(mg)} ≠ source {sorted(o['merged'])}")
            if m.get("formulas") != str(o["formulas"]):
                gate.fail(f"{where} [{name}] formulas={m.get('formulas')} ≠ source {o['formulas']}")


def load_plan(path):
    if not path:
        return {}
    with open(path, "rb") as f:
        plan = tomllib.load(f)
    return {norm_rel(e["path"]): e for e in plan.get("file", [])}, plan


def norm_rel(p):
    return "/".join(x for x in p.replace("\\", "/").split("/") if x not in ("", "."))


def cmd_grid(merged, src_root, selection, gate=None):
    gate = gate or Gate("grid")
    entries = load_plan(selection)[0] if selection else {}
    with open(merged, encoding="utf-8", newline="") as f:
        text = f.read()
    n = 0
    for b in md_blocks(text):
        if b["lang"] != "tm-grid":
            continue
        n += 1
        g = decode_grid(b["content"], gate, b["path"])
        if g is None:
            continue
        check_grid_self(g, gate, b["path"])
        verify_grid_against_oracle(b, g, entries.get(norm_rel(b["path"])), src_root, gate)
    gate.add("grid_blocks", n)
    if selection:
        for rel, e in entries.items():
            ext = os.path.splitext(rel)[1].lower()
            is_table = ext in (".xlsx", ".xlsm", ".xls", ".xlsb", ".ods") or (
                ext in (".csv", ".tsv") and any(e.get(k) for k in ("columns", "rows", "header_row")))
            if is_table and not e.get("exclude") and not any(norm_rel(b["path"]) == rel and b["lang"] == "tm-grid" for b in md_blocks(text)):
                gate.fail(f"{rel}: planned table has no tm-grid block")
    return gate


# ------------------------------------------------------------------- slices

def scan_headings(lines):
    heads, fence = [], None
    for i, raw in enumerate(lines):
        line = raw.rstrip("\n").rstrip("\r")
        indent = len(line) - len(line.lstrip(" "))
        rest = line[indent:]
        if indent <= 3 and rest[:1] in ("`", "~"):
            ch = rest[0]
            n = len(rest) - len(rest.lstrip(ch))
            if n >= 3:
                if fence is None:
                    fence = (ch, n)
                    continue
                if fence[0] == ch and n >= fence[1] and rest[n:].strip() == "":
                    fence = None
                    continue
        if fence is not None or indent > 3 or not rest.startswith("#"):
            continue
        h = len(rest) - len(rest.lstrip("#"))
        if h > 6:
            continue
        after = rest[h:]
        if not (after == "" or after[0] in " \t"):
            continue
        t = after.strip()
        stripped = t.rstrip("#")
        if len(stripped) < len(t) and (stripped == "" or stripped[-1] in " \t"):
            t = stripped.rstrip()
        heads.append({"level": h, "text": t, "line": i})
    for k, hd in enumerate(heads):
        hd["own_end"] = heads[k + 1]["line"] if k + 1 < len(heads) else len(lines)
        nxt = next((x for x in heads[k + 1:] if x["level"] <= hd["level"]), None)
        hd["sub_end"] = nxt["line"] if nxt else len(lines)
    return heads


def parse_pattern(p):
    own = p.startswith("own:")
    if own:
        p = p[4:]
    m = re.match(r"^[Hh]([1-6]):(.*)$", p, re.S)
    if m:
        return own, int(m.group(1)), m.group(2)
    return own, None, p


def expected_slice(text, e):
    """Re-implementation of slice_text from the written rules → expected output string."""
    lines = text.splitlines(keepends=True)
    total = len(lines)
    heads = scan_headings(lines)
    keep = [False] * total
    if e.get("lines"):
        for spec in e["lines"]:
            a, _, b = str(spec).partition("-")
            a, b = int(a), int(b or a)
            for i in range(a - 1, min(b, total)):
                keep[i] = True
    else:
        first = heads[0]["line"] if heads else total
        secs, matches = e.get("sections", []), e.get("match", [])
        if not secs and not matches:
            keep = [True] * total
            if e.get("preamble") is False:
                for i in range(first):
                    keep[i] = False
        elif e.get("preamble", bool(secs)):
            for i in range(first):
                keep[i] = True

        def mark(p, on):
            own, lvl, glob = parse_pattern(p)
            for hd in heads:
                if (lvl is None or lvl == hd["level"]) and glob_match(glob, hd["text"]):
                    for i in range(hd["line"], hd["own_end"] if own else hd["sub_end"]):
                        keep[i] = on

        for p in secs:
            mark(p, True)
        for rx in matches:
            r = re.compile(rx)
            for i, ln in enumerate(lines):
                if r.search(ln.rstrip("\n").rstrip("\r")):
                    keep[i] = True
                    if e.get("heading_context"):
                        hd = next((x for x in reversed(heads) if x["line"] <= i), None)
                        if hd:
                            keep[hd["line"]] = True
        for p in e.get("exclude_sections", []):
            mark(p, False)
    kept_lines = sum(keep)
    kept_bytes = sum(len(lines[i].encode("utf-8")) for i in range(total) if keep[i])
    kept_secs = sum(1 for hd in heads if keep[hd["line"]])
    label = e.get("label") or e["path"]
    out = [f"[tm:slice label={json.dumps(label, ensure_ascii=False)} kept=\"{kept_lines} of {total} lines\" "
           f"sections=\"{kept_secs} of {len(heads)}\" bytes=\"{kept_bytes} of {len(text.encode('utf-8'))}\"]\n"]
    i = 0
    while i < total:
        start, k = i, keep[i]
        while i < total and keep[i] == k:
            i += 1
        if k:
            for j in range(start, i):
                ln = lines[j]
                out.append(ln if (ln.endswith("\n") or j + 1 == total) else ln + "\n")
        else:
            b = sum(len(lines[j].encode("utf-8")) for j in range(start, i))
            inside = [hd for hd in heads if start <= hd["line"] < i]
            what = ""
            if inside:
                what = f" (section {json.dumps(inside[0]['text'], ensure_ascii=False)}" + (
                    f" + {len(inside) - 1} more)" if len(inside) > 1 else ")")
            out.append(f"[tm:omitted lines {start + 1}-{i}{what} — {i - start} lines, {b} bytes]\n")
    return "".join(out)


def cmd_slices(merged, src_root, selection, gate=None):
    gate = gate or Gate("slices")
    entries, _ = load_plan(selection)
    with open(merged, encoding="utf-8", newline="") as f:
        blocks = {norm_rel(b["path"]): b for b in md_blocks(f.read())}
    for rel, e in entries.items():
        if not any(e.get(k) for k in ("lines", "sections", "exclude_sections", "match")) and e.get("preamble") is not False:
            continue
        if e.get("max_tokens"):
            gate.note(f"{rel}: max_tokens slice not re-derived (needs the o200k tokenizer)")
            continue
        b = blocks.get(rel)
        if b is None:
            gate.fail(f"{rel}: sliced file missing from the merge")
            continue
        with open(os.path.join(src_root, rel), "rb") as f:
            raw = f.read()
        src = raw.decode("utf-8")
        if src.startswith("﻿"):
            src = src[1:]
        if "\r\n" in src and "\r" not in b["content"]:
            src = src.replace("\r\n", "\n")  # --reproducible
        want = expected_slice(src, e)
        got = b["content"] + "\n"
        gate.add("sliced_files")
        if got == want or (not want.endswith("\n") and got == want + "\n"):
            gate.add("byte_identical")
            continue
        wl, gl = want.splitlines(), got.splitlines()
        k = next((n for n, (x, y) in enumerate(zip(wl, gl)) if x != y), min(len(wl), len(gl)))
        gate.fail(f"{rel}: slice differs at output line {k + 1}: expected {wl[k][:90] if k < len(wl) else '<end>'!r} got {gl[k][:90] if k < len(gl) else '<end>'!r}")
    return gate


# --------------------------------------------------------------- accounting

def cmd_accounting(json_path, selection, gate=None):
    gate = gate or Gate("accounting")
    entries, plan = load_plan(selection)
    with open(json_path, encoding="utf-8") as f:
        doc = json.load(f)
    files = doc.get("files", [])
    got = {norm_rel(x.get("path", "")): x for x in files}
    for rel, e in entries.items():
        if e.get("exclude"):
            continue
        gate.add("planned")
        if rel not in got:
            gate.fail(f"{rel}: planned but not in the merge")
            continue
        gate.add("merged")
        ext = os.path.splitext(rel)[1].lower()
        want_grid = ext in (".xlsx", ".xlsm", ".xls", ".xlsb", ".ods") or (ext in (".csv", ".tsv") and any(e.get(k) for k in ("columns", "rows", "header_row")))
        lang = got[rel].get("language", got[rel].get("lang", ""))
        if want_grid and lang != "tm-grid":
            gate.fail(f"{rel}: language {lang!r}, expected tm-grid")
    extra = [p for p in got if p not in entries]
    if extra and plan.get("mode") == "explicit":
        gate.fail(f"files not in the plan: {extra[:5]}")
    skipped = doc.get("skipped") or doc.get("not_captured_files") or []
    nc = [s for s in skipped if isinstance(s, dict) and s.get("kind", "").lower().replace("_", "") in ("notcaptured",)]
    summary = doc.get("summary", {})
    ncount = summary.get("not_captured", len(nc)) if isinstance(summary, dict) else len(nc)
    if ncount:
        gate.fail(f"not captured: {ncount} {nc[:3]}")
    gate.add("not_captured", ncount)
    return gate


# ------------------------------------------------------------ decode & cold

def py_value(kind, text):
    if kind == "n":
        return int(text) if RE_NUM.match(text) and "." not in text else float(text)
    if kind == "b":
        return text == "TRUE"
    if kind == "d":
        if re.match(r"^\d{4}-\d{2}-\d{2}$", text):
            return dt.datetime.strptime(text, "%Y-%m-%d")
        if re.match(r"^\d{4}-", text):
            fmt = "%Y-%m-%d %H:%M:%S.%f" if "." in text else "%Y-%m-%d %H:%M:%S"
            return dt.datetime.strptime(text, fmt)
        neg = text.startswith("-")
        h, m, s = text.lstrip("-").split(":")
        sec = float(s)
        td = dt.timedelta(hours=int(h), minutes=int(m), seconds=sec)
        if not neg and int(h) < 24 and len(h) == 2:
            return (dt.datetime.min + td).time()
        return -td if neg else td
    return text


def cmd_decode(merged, out_dir, xlsx=False):
    gate = Gate("decode")
    with open(merged, encoding="utf-8", newline="") as f:
        text = f.read()
    os.makedirs(out_dir, exist_ok=True)
    built = []
    for b in md_blocks(text):
        if b["lang"] != "tm-grid":
            continue
        g = decode_grid(b["content"], gate, b["path"])
        if g is None:
            continue
        check_grid_self(g, gate, b["path"])
        base = os.path.join(out_dir, norm_rel(b["path"]))
        os.makedirs(base, exist_ok=True)
        for sh in g["sheets"]:
            if "skipped" in sh["meta"]:
                continue
            nm = re.sub(r"[^\w.-]+", "_", sh["meta"]["name"])[:80]
            with open(os.path.join(base, nm + ".jsonl"), "w", encoding="utf-8") as f:
                for (r, c), (k, t) in sorted(sh["cells"].items()):
                    f.write(json.dumps({"row": r, "col": c, "type": k, "text": t}, ensure_ascii=False) + "\n")
        if xlsx and not b["path"].lower().endswith((".csv", ".tsv")):
            import openpyxl  # noqa: PLC0415
            wb = openpyxl.Workbook()
            wb.remove(wb.active)
            for sh in g["sheets"]:
                ws = wb.create_sheet(sh["meta"]["name"][:31])
                state = sh["meta"].get("state", "visible")
                ws.sheet_state = {"veryhidden": "veryHidden"}.get(state, state)
                for (r, c), (k, t) in sh["cells"].items():
                    cell = ws.cell(row=r, column=c)
                    if k == "s":
                        cell.value = t
                        cell.data_type = "s"  # never let '=…' become a formula
                    elif k == "e":
                        cell.value = t
                        cell.data_type = "e"
                    else:
                        cell.value = py_value(k, t)
                        if k == "d" and isinstance(cell.value, dt.timedelta):
                            cell.number_format = "[h]:mm:ss"
                for rng in filter(None, sh["meta"].get("merged", "-").split(";")):
                    if rng != "-":
                        ws.merged_cells.add(rng)
            path = base + ".rebuilt.xlsx"
            wb.save(path)
            built.append(path)
    gate.add("workbooks_rebuilt", len(built))
    return gate


def cmd_cold(merged, src_root, work, selection=None):
    gate = Gate("cold")
    if os.path.exists(work):
        shutil.rmtree(work)
    os.makedirs(work)
    shutil.copy(merged, os.path.join(work, "merged.md"))
    shutil.copy(os.path.abspath(__file__), os.path.join(work, "tm_verify.py"))
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": work}
    for k in ("PYTHONUSERBASE",):
        if k in os.environ:
            env[k] = os.environ[k]
    user_site = subprocess.run([sys.executable, "-c", "import site;print(site.getusersitepackages())"],
                               capture_output=True, text=True).stdout.strip()
    if user_site:
        env["PYTHONPATH"] = user_site  # openpyxl may live there; the source tree is never on the path
    r = subprocess.run([sys.executable, "-P", "tm_verify.py", "decode", "merged.md", "--out", "rebuilt", "--xlsx"],
                       cwd=work, env=env, capture_output=True, text=True)
    if r.returncode != 0:
        gate.fail(f"decode in the empty folder failed: {r.stderr.strip()[-400:]}")
        return gate
    with open(merged, encoding="utf-8", newline="") as f:
        text = f.read()
    for b in md_blocks(text):
        if b["lang"] != "tm-grid" or b["path"].lower().endswith((".csv", ".tsv")):
            continue
        rebuilt = os.path.join(work, "rebuilt", norm_rel(b["path"]) + ".rebuilt.xlsx")
        if not os.path.isfile(rebuilt):
            gate.fail(f"{b['path']}: no rebuilt workbook")
            continue
        g = decode_grid(b["content"], Gate("tmp"), b["path"])
        rb = oracle_workbook(rebuilt)
        orig = oracle_workbook(os.path.join(src_root, b["path"]))
        for sh in g["sheets"]:
            if "skipped" in sh["meta"]:
                continue
            name = sh["meta"]["name"]
            got = rb.get(name[:31], {}).get("cells", {})
            if got != sh["cells"]:
                k = next(k for k in sorted(set(got) | set(sh["cells"])) if got.get(k) != sh["cells"].get(k))
                gate.fail(f"{b['path']} [{name}] rebuilt workbook ≠ Markdown cells at {k}: rebuilt {got.get(k)!r} vs md {sh['cells'].get(k)!r}")
                continue
            o = orig[name]["cells"]
            bad = [k for k, v in got.items() if o.get(k) != v]
            if bad:
                gate.fail(f"{b['path']} [{name}] {len(bad)} rebuilt cells ≠ original, first {bad[0]}")
            else:
                gate.add("sheets_cell_identical")
                gate.add("cells", len(got))
    return gate


# ------------------------------------------------------------------ restore

def display_path(rel):
    """TurboMerger's block-header form of a path: control characters as \\u{hex}."""
    return "".join(f"\\u{{{ord(c):x}}}" if unicodedata.category(c) == "Cc" else c for c in rel)


def cmd_restore(archive, out_dir, src_root=None):
    gate = Gate("restore")
    with open(archive, encoding="utf-8", newline="") as f:
        text = f.read()
    blocks = md_blocks(text)
    man = next((b for b in blocks if b["path"] == "__manifest__"), None)
    if man is None:
        gate.fail("no ## __manifest__ block")
        return gate
    lines = [json.loads(x) for x in man["content"].splitlines() if x.strip()]
    entries = [x for x in lines if "path" in x]
    for h in (x for x in lines if "archive" in x):
        if h.get("parts", 1) != 1:
            gate.fail(f"this archive is part {h.get('part')} of {h.get('parts')}: use `turbomerger restore` with every part")
            return gate
    bins = {b["path"][len("__binary__: "):]: b["content"] for b in blocks if b["path"].startswith("__binary__: ")}
    content = {b["path"]: b for b in blocks}
    os.makedirs(out_dir, exist_ok=True)
    for e in entries:
        rel = e["path"]
        parts = rel.replace("\\", "/").split("/")
        if os.path.isabs(rel) or re.match(r"^[A-Za-z]:", rel) or ".." in parts or "" in parts:
            gate.fail(f"{rel}: unsafe path (absolute, drive, empty or '..' part) — not written")
            continue
        if e.get("kind") in ("withheld", "missing"):
            gate.fail(f"{rel}: {e['kind']} — {e.get('reason', 'no reason given')}")
            continue
        target = os.path.join(out_dir, rel)
        os.makedirs(os.path.dirname(target) or ".", exist_ok=True)
        head = display_path(rel)
        if head in bins:
            data = base64.b64decode("".join(bins[head].split()))
        elif e.get("kind") == "text" and head in content:
            body = content[head]["content"]
            if e.get("eol") == "crlf":
                body = body.replace("\r\n", "\n")
                if body.endswith("\r"):  # the CR of the last CRLF sits before the closing fence
                    body = body[:-1]
            if e.get("final_newline") and e["bytes"] > 0:
                body += "\n"
            if e.get("eol") == "crlf":
                body = body.replace("\n", "\r\n")
            data = body.encode("utf-8") if e["bytes"] > 0 else b""
        else:
            gate.fail(f"{rel}: {e.get('kind')} file has no embedded bytes (use --embed-binary)")
            continue
        with open(target, "wb") as f:
            f.write(data)
        if e.get("mode"):
            os.chmod(target, e["mode"] & 0o777)
        sha = hashlib.sha256(data).hexdigest()
        if len(data) != e["bytes"] or sha != e["sha256"]:
            gate.fail(f"{rel}: restored {len(data)} B sha {sha[:12]} ≠ manifest {e['bytes']} B {e['sha256'][:12]}")
            continue
        if src_root:
            with open(os.path.join(src_root, rel), "rb") as f:
                if hashlib.sha256(f.read()).hexdigest() != sha:
                    gate.fail(f"{rel}: restored file ≠ the original on disk")
                    continue
        gate.add("byte_identical")
        gate.add("bytes", len(data))
    gate.add("manifest_files", len(entries))
    return gate


# -------------------------------------------------------------------- audit

def cmd_audit(merged, src_root, out, selection=None):
    entries = load_plan(selection)[0] if selection else {}
    with open(merged, encoding="utf-8", newline="") as f:
        text = f.read()
    rows = ["# TurboMerger audit sample (source value vs decoded value)", "",
            f"Merged file: `{merged}`  ", f"Written: {dt.datetime.now().astimezone().isoformat(timespec='seconds')}", "",
            "Exhaustive checking is done by `grid`; this sample is for eyes: first/last 3 body rows of every sheet,",
            "every 25th row, and every cell with a line break, pipe, backslash, leading quote, edge space, or > 1,000 chars.", ""]
    for b in md_blocks(text):
        if b["lang"] != "tm-grid":
            continue
        g = decode_grid(b["content"], Gate("tmp"), b["path"])
        src = os.path.join(src_root, b["path"])
        ext = os.path.splitext(src)[1].lower()
        oracle = oracle_delimited(src, "\t" if ext == ".tsv" else ",") if ext in (".csv", ".tsv") else oracle_workbook(src)
        rows.append(f"## {b['path']}")
        for sh in g["sheets"]:
            if "skipped" in sh["meta"]:
                rows.append(f"- sheet `{sh['meta']['name']}`: skipped ({sh['meta']['skipped']})")
                continue
            o = oracle[sh["meta"]["name"]]["cells"]
            br = sh["body_rows"]
            pick = set(br[:3] + br[-3:] + br[::25])
            rows.append(f"### `{sh['meta']['name']}` — {sh['meta'].get('body_rows')} rows, {sh['meta'].get('cells')} cells")
            rows.append("| cell | source (oracle) | decoded from the bundle | equal |")
            rows.append("| --- | --- | --- | --- |")
            for (r, c), v in sorted(sh["cells"].items()):
                t = v[1]
                special = any(x in t for x in ("\n", "|", "\\")) or t.startswith("'") or t != t.strip() or len(t) > 1000
                if r in pick or special:
                    ov = o.get((r, c))
                    show = lambda x: escape_cell(x[1][:120], False) + ("…" if len(x[1]) > 120 else "") if x else "∅"  # noqa: E731
                    rows.append(f"| {col_letter(c - 1)}{r} | {show(ov)} | {show(v)} | {'yes' if ov == v else '**NO**'} |")
            rows.append("")
    with open(out, "w", encoding="utf-8") as f:
        f.write("\n".join(rows) + "\n")
    g = Gate("audit")
    g.add("lines", len(rows))
    return g


# ----------------------------------------------------------------- selftest

def find_bin(arg):
    for c in (arg, os.environ.get("TURBOMERGER_BIN"), os.path.expanduser("~/.local/opt/TurboMerger-dev/turbomerger"),
              os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "target", "debug", "turbomerger")):
        if c and os.path.isfile(c) and os.access(c, os.X_OK):
            return os.path.abspath(c)
    return None


def build_fixtures(root):
    import openpyxl  # noqa: PLC0415
    wb = openpyxl.Workbook()
    ws = wb.active
    ws.title = "Cases"
    ws["A1"] = "Second-chance cases (synthetic)"
    ws["A2"] = "built for tm_verify selftest"
    ws.merge_cells("A1:F1")
    for i, h in enumerate(["id", "name", "note", "score", "flag", "when"], 1):
        ws.cell(row=4, column=i, value=h)
    data = [
        ["1403811331", "Mercy | Lorain", "line one\nline two", 54, True, dt.datetime(2024, 1, 31)],
        ["0123", "<br> literal", "back\\slash \\| mix", 45.5, False, dt.datetime(2024, 2, 1, 13, 45, 7)],
        ["'quoted", " lead space", "tab\there", 1e-7, None, dt.time(9, 30)],
        ["TRUE", "trail space ", "cr\rinside", 0.1 + 0.2, None, None],
        ["x7", "Ünïcödé 😀 ‏RTL", "=SUM(1,2) as text", -0.0, None, None],
    ]
    for r, row in enumerate(data, 5):
        for c, v in enumerate(row, 1):
            if v is not None:
                cell = ws.cell(row=r, column=c, value=v)
                if isinstance(v, str):
                    cell.data_type = "s"
    ws.cell(row=10, column=4, value="#DIV/0!").data_type = "e"
    ws["G12"] = "=D5*2"
    hid = wb.create_sheet("Hidden")
    hid["C5"] = "sparse start"
    hid["D7"] = 3
    hid.sheet_state = "hidden"
    wb.create_sheet("Empty")
    wb.save(os.path.join(root, "cases.xlsx"))
    # ISO dates (OOXML t="d"), as other writers store them; S8 found them
    # kept raw (`2026-10-13T12:00:00.000Z`) in a real workbook.
    iso = openpyxl.Workbook(iso_dates=True)
    ws = iso.active
    ws.title = "ISO"
    for i, h in enumerate(["id", "checked", "day", "clock"], 1):
        ws.cell(row=1, column=i, value=h)
    ws.append(["a", dt.datetime(2026, 10, 13, 12, 0, 0), dt.date(2024, 2, 29), dt.time(9, 30)])
    ws.append(["b", dt.datetime(2024, 1, 31, 13, 45, 7, 500000), dt.datetime(2026, 10, 13), None])
    iso_path = os.path.join(root, "isodates.xlsx")
    iso.save(iso_path)
    with zipfile.ZipFile(iso_path) as z:
        members = [(i, z.read(i.filename)) for i in z.infolist()]
    with zipfile.ZipFile(iso_path, "w", zipfile.ZIP_DEFLATED) as z:
        for info, data in members:
            if info.filename == "xl/worksheets/sheet1.xml":
                data = data.replace(b">2026-10-13T12:00:00<", b">2026-10-13T12:00:00.000Z<")
            z.writestr(info, data)
    with open(os.path.join(root, "roster.csv"), "w", encoding="utf-8", newline="") as f:
        f.write('Rank,Program,City,Why,Website\r\n1,"Mercy, Lorain",Lorain,"two\r\nlines","https://a.example"\r\n'
                '2,"Say ""hi""",X,,https://b.example\r\n\r\n3,Third,Y,"pipe | and \\ back",\r\n')
    with open(os.path.join(root, "dossier.md"), "w", encoding="utf-8", newline="") as f:
        f.write("pre\n# DOSSIER TITLE\nintro\n## 0. Files\nf\n## 1. Summary\ns1\n### 1.1 Detail\n"
                "d https://c.example/x\n```\n## fenced, not a heading\n```\n## 2. Rankings\nr\n## 3. Programs\n"
                "### FM #1 Mercy\nlong text\nproof https://d.example\n### FM #2 Other\nno url here\n")
    with open(os.path.join(root, "code.py"), "w", encoding="utf-8", newline="") as f:
        f.write("".join(f"line {i}\n" for i in range(1, 21)))
    sel = '''version = 1
mode = "explicit"
ordering = "plan"
[[file]]
path = "cases.xlsx"
sheets = ["Cases", "Hidden"]
[[file.sheet]]
name = "Cases"
header_row = 4
columns = ["id", "name", "note", "score", "flag", "when"]
[[file]]
path = "roster.csv"
columns = ["Rank", "Program", "Why", "Website"]
[[file]]
path = "dossier.md"
sections = ["own:H1:DOSSIER*", "H2:1. Summary", "H2:2. *"]
exclude_sections = ["H3:1.1 *"]
match = ["https?://"]
heading_context = true
[[file]]
path = "code.py"
lines = ["2-4", "10"]
[[file]]
path = "isodates.xlsx"
'''
    with open(os.path.join(root, "sel.toml"), "w", encoding="utf-8") as f:
        f.write(sel)
    return os.path.join(root, "sel.toml")


def run_checks(md, js, root, sel):
    gates = [cmd_grid(md, root, sel), cmd_slices(md, root, sel)]
    if js:
        gates.append(cmd_accounting(js, sel))
    return gates


def cmd_selftest(bin_arg):
    tm = find_bin(bin_arg)
    if not tm:
        print("selftest: no turbomerger binary (--bin, $TURBOMERGER_BIN, ~/.local/opt/TurboMerger-dev)", file=sys.stderr)
        return EXIT_USAGE
    ok = True
    with tempfile.TemporaryDirectory(prefix="tm_verify_selftest_") as tmp:
        src = os.path.join(tmp, "src")
        os.makedirs(src)
        sel = build_fixtures(src)
        md, js = os.path.join(tmp, "out.md"), os.path.join(tmp, "out.json")
        env = dict(os.environ, TURBOMERGER_NO_CACHE="1")
        for out, fmt in ((md, "markdown"), (js, "json")):
            r = subprocess.run([tm, "merge", src, out, "--selection", sel, "--format", fmt, "--reproducible", "--yes", "-q"],
                               capture_output=True, text=True, env=env)
            if r.returncode != 0:
                print(f"selftest: turbomerger exit {r.returncode}: {r.stderr[-600:]}", file=sys.stderr)
                return EXIT_GATE
        clean = run_checks(md, js, src, sel)
        for g in clean:
            print(g.report())
            ok &= g.ok
        cg = cmd_cold(md, src, os.path.join(tmp, "cold"))
        print(cg.report())
        ok &= cg.ok
        with open(md, encoding="utf-8", newline="") as f:
            base = f.read()

        def mut(n, a, b, count=1):
            if a not in base:
                return None, f"mutation {n}: anchor {a!r} not found"
            return base.replace(a, b, count), None

        def first_row(prefix):
            return next(ln for ln in base.split("\n") if ln.startswith(prefix))

        row5 = first_row("| 5 |")
        row6 = first_row("| 6 |")
        f5 = split_row(row5)
        swapped = "|" + "|".join([f5[0], f5[2], f5[1]] + f5[3:]) + "|"
        sheet_line = next(ln for ln in base.split("\n") if ln.startswith('@sheet 1/3 name="Cases"'))
        sha = re.search(r"sha256=([0-9a-f]{64})", sheet_line).group(1)
        mutations = [
            ("change one character in a cell", *mut(1, "Mercy \\| Lorain", "Mercy \\| Lorsin")),
            ("delete a body row", *mut(2, row6 + "\n", "")),
            ("swap two adjacent cells", *mut(3, row5, swapped)),
            ("change a number", *mut(4, " 45.5 |", " 45.6 |")),
            ("insert an unescaped pipe", *mut(5, "Mercy \\| Lorain", "Mercy | Lorain")),
            ("remove an @end", re.sub(r"\n@end sheet=1 [^\n]*", "", base, count=1), None),
            ("edit the sha256", *mut(7, sha, sha[:-1] + ("0" if sha[-1] != "0" else "1"))),
            ("swap two sheets", *mut(8, 'name="Hidden"', 'name="Hiddem"')),
            ("duplicate a row", *mut(9, row6 + "\n", row6 + "\n" + row6 + "\n")),
            ("strip a pad space", *mut(10, "| 54 |", "|54 |")),
            ("delete a ' marker", *mut(11, "| '1403811331 |", "| 1403811331 |")),
            ("replace <br> by a space", *mut(12, "line one<br>line two", "line one line two")),
            ("alter a kept slice line", *mut(13, "\ns1\n", "\ns2\n")),
            ("change an omitted-lines marker", re.sub(r"\[tm:omitted lines (\d+)-", lambda m: f"[tm:omitted lines {int(m.group(1)) + 1}-", base, count=1), None),
        ]
        detected = 0
        for k, (name, text, err) in enumerate(mutations, 1):
            if err or text is None or text == base:
                print(f"  [!!] mutation {k:02d} ({name}) could not be applied: {err}")
                ok = False
                continue
            mp = os.path.join(tmp, f"mut{k}.md")
            with open(mp, "w", encoding="utf-8", newline="") as f:
                f.write(text)
            gates = [cmd_grid(mp, src, sel), cmd_slices(mp, src, sel)]
            if all(g.ok for g in gates):
                print(f"  [!!] mutation {k:02d} ({name}) was NOT detected")
                ok = False
            else:
                detected += 1
                first = next(m for g in gates for m in g.fails)
                print(f"  [ok] mutation {k:02d} ({name}) detected: {first[:110]}")
        print(f"selftest: clean run {'PASS' if all(g.ok for g in clean) and cg.ok else 'FAIL'}, "
              f"{detected}/{len(mutations)} mutations detected")
        ar = os.path.join(tmp, "archive.md")
        plan = os.path.join(tmp, "complete.toml")
        with open(plan, "w", encoding="utf-8") as f:
            f.write('version = 1\nmode = "explicit"\nordering = "plan"\ncomplete = true\n' +
                    "".join(f'[[file]]\npath = "{p}"\n' for p in ("cases.xlsx", "roster.csv", "dossier.md", "code.py", "isodates.xlsx")))
        r = subprocess.run([tm, "merge", src, ar, "--selection", plan, "--complete", "--embed-binary", "5", "--format",
                            "markdown", "--yes", "-q"], capture_output=True, text=True, env=env)
        if r.returncode != 0:
            print(f"selftest: archive merge exit {r.returncode}: {r.stderr[-400:]}")
            return EXIT_GATE
        rg = cmd_restore(ar, os.path.join(tmp, "restored"), src)
        print(rg.report())
        ok &= rg.ok
        # Two independent readers of the twin must agree: this script's restore
        # and the binary's own `restore` (S9; a binary without it says exit 2).
        native = os.path.join(tmp, "restored_native")
        r = subprocess.run([tm, "restore", ar, "--dest", native], capture_output=True, text=True, env=env)
        if r.returncode == 2 and "restore" in r.stderr and not os.path.exists(native):
            print("  [--] this binary has no `restore` command (made before S9)")
        else:
            def tree(d):
                out = {}
                for base, _, names in os.walk(d):
                    for n in names:
                        fp = os.path.join(base, n)
                        with open(fp, "rb") as f:
                            out[os.path.relpath(fp, d)] = hashlib.sha256(f.read()).hexdigest()
                return out
            same = r.returncode == 0 and tree(native) == tree(os.path.join(tmp, "restored"))
            print(f"  [{'ok' if same else '!!'}] native `turbomerger restore` "
                  f"{'matches' if same else 'DIFFERS from'} this script's restore (exit {r.returncode})")
            ok &= same
        with open(ar, encoding="utf-8", newline="") as f:
            atext = f.read()
        bad = os.path.join(tmp, "archive_bad.md")
        with open(bad, "w", encoding="utf-8", newline="") as f:
            f.write(atext.replace("line 7\n", "line 7 \n", 1))
        bg = cmd_restore(bad, os.path.join(tmp, "restored_bad"), src)
        if bg.ok:
            print("  [!!] a one-byte change in the archive was NOT detected by restore")
            ok = False
        else:
            print("  [ok] a one-byte change in the archive is detected by restore")
        # Redaction must never leak through the twin (HANDOFF_S8 §6.10 item 1).
        secret = "Vr7wKp2walKotwica91"
        with open(os.path.join(src, "secret.md"), "w", encoding="utf-8") as f:
            f.write(f"# Notes\npassword: {secret}\n")
        leak_plan = os.path.join(tmp, "leak.toml")
        with open(leak_plan, "w", encoding="utf-8") as f:
            f.write('version = 1\nmode = "explicit"\ncomplete = true\n[[file]]\npath = "secret.md"\n'
                    '[[file]]\npath = "cases.xlsx"\n')
        lk = os.path.join(tmp, "leak.md")
        r = subprocess.run([tm, "merge", src, lk, "--selection", leak_plan, "--redact", "--format", "markdown",
                            "--yes", "-q"], capture_output=True, text=True, env=env)
        with open(lk, encoding="utf-8", newline="") as f:
            ltext = f.read()
        decoded = b"".join(base64.b64decode("".join(b["content"].split()))
                           for b in md_blocks(ltext) if b["path"].startswith("__binary__: "))
        if secret in ltext or secret.encode() in decoded or r.returncode != 3:
            print(f"  [!!] --complete --redact leaked the secret or did not exit 3 (exit {r.returncode})")
            ok = False
        else:
            print("  [ok] --complete --redact embeds no source bytes; withheld files are named (exit 3)")
        ok &= detected == len(mutations)
    print("SELFTEST", "PASS" if ok else "FAIL")
    return EXIT_OK if ok else EXIT_GATE


# --------------------------------------------------------------------- main

def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("selftest")
    s.add_argument("--bin")
    for name in ("grid", "slices", "audit", "all"):
        p = sub.add_parser(name)
        p.add_argument("--merged", required=True)
        p.add_argument("--src-root", required=True)
        p.add_argument("--selection", required=name == "slices")
        if name == "audit":
            p.add_argument("--out", required=True)
        if name == "all":
            p.add_argument("--json")
            p.add_argument("--report")
            p.add_argument("--work")
    p = sub.add_parser("accounting")
    p.add_argument("--json", required=True)
    p.add_argument("--selection", required=True)
    p = sub.add_parser("cold")
    p.add_argument("--merged", required=True)
    p.add_argument("--src-root", required=True)
    p.add_argument("--work", required=True)
    p = sub.add_parser("restore")
    p.add_argument("archive")
    p.add_argument("--out", required=True)
    p.add_argument("--src-root")
    p = sub.add_parser("decode")
    p.add_argument("merged")
    p.add_argument("--out", required=True)
    p.add_argument("--xlsx", action="store_true")
    a = ap.parse_args()
    if a.cmd == "selftest":
        return cmd_selftest(a.bin)
    if a.cmd == "grid":
        gates = [cmd_grid(a.merged, a.src_root, a.selection)]
    elif a.cmd == "slices":
        gates = [cmd_slices(a.merged, a.src_root, a.selection)]
    elif a.cmd == "accounting":
        gates = [cmd_accounting(a.json, a.selection)]
    elif a.cmd == "cold":
        gates = [cmd_cold(a.merged, a.src_root, a.work)]
    elif a.cmd == "restore":
        gates = [cmd_restore(a.archive, a.out, a.src_root)]
    elif a.cmd == "decode":
        gates = [cmd_decode(a.merged, a.out, a.xlsx)]
    elif a.cmd == "audit":
        gates = [cmd_audit(a.merged, a.src_root, a.out, a.selection)]
    else:  # all
        gates = [cmd_grid(a.merged, a.src_root, a.selection)]
        if a.selection:
            gates.append(cmd_slices(a.merged, a.src_root, a.selection))
        if a.json and a.selection:
            gates.append(cmd_accounting(a.json, a.selection))
        if a.work:
            gates.append(cmd_cold(a.merged, a.src_root, a.work))
    text = "\n".join(g.report(limit=200 if a.cmd == "all" else 25) for g in gates)
    print(text)
    if getattr(a, "report", None):
        with open(a.report, "w", encoding="utf-8") as f:
            f.write(f"# tm_verify report\n\nMerged: `{a.merged}`  \nSource root: `{a.src_root}`  \n"
                    f"Selection: `{a.selection}`  \nWritten: {dt.datetime.now().astimezone().isoformat(timespec='seconds')}\n\n"
                    f"```\n{text}\n```\n")
    return EXIT_OK if all(g.ok for g in gates) else EXIT_GATE


if __name__ == "__main__":
    sys.exit(main())
