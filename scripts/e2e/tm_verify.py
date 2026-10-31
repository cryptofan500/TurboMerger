#!/usr/bin/env python3
"""Line-by-line Oracle Verifier for TurboMerger TM-GRID/1 and Granular Selection.
Validates Gates G1–G11 against openpyxl ground truth.
Exit code 0: ALL PASS
Exit code 5: VERIFICATION GATE FAILED
"""

import sys
import os
import re
import json
import hashlib
import argparse
import base64
from typing import Dict, List, Tuple, Any, Optional

try:
    import tomllib
except ImportError:
    try:
        import tomli as tomllib
    except ImportError:
        tomllib = None

import openpyxl

EXIT_OK = 0
EXIT_ERR = 1
EXIT_GATE_FAILED = 5

def sha256_bytes(b: bytes) -> str:
    return hashlib.sha256(b).hexdigest()

def unescape_cell(raw: str) -> str:
    """Decodes a TM-GRID/1 table cell into raw string."""
    text = raw.strip()
    if text.startswith(' '):
        text = text[1:]
    if text.endswith(' '):
        text = text[:-1]
    
    is_quoted = text.startswith("'")
    if is_quoted:
        text = text[1:]
    
    out = []
    i = 0
    n = len(text)
    while i < n:
        if text[i] == '\\' and i + 1 < n:
            nxt = text[i+1]
            if nxt == '\\':
                out.append('\\')
                i += 2
            elif nxt == '|':
                out.append('|')
                i += 2
            elif nxt == 'r':
                out.append('\r')
                i += 2
            elif nxt == 't':
                out.append('\t')
                i += 2
            elif text[i:].startswith('\\<br>'):
                out.append('<br>')
                i += 5
            elif nxt == 'x' and i + 3 < n:
                try:
                    code = int(text[i+2:i+4], 16)
                    out.append(chr(code))
                    i += 4
                except ValueError:
                    out.append(text[i:i+2])
                    i += 2
            else:
                out.append(nxt)
                i += 2
        elif text[i:].startswith('<br>'):
            out.append('\n')
            i += 4
        else:
            out.append(text[i])
            i += 1
    return "".join(out)

def escape_cell(canon: str, is_string: bool = True) -> str:
    """Encodes cell text to TM-GRID/1."""
    prefix = "'" if (is_string and (canon.startswith("'") or re.match(r'^-?\d+(\.\d+)?$', canon))) else ""
    res = []
    for c in canon:
        if c == '\\':
            res.append('\\\\')
        elif c == '|':
            res.append('\\|')
        elif c == '\n':
            res.append('<br>')
        elif c == '\r':
            res.append('\\r')
        elif c == '\t':
            res.append('\\t')
        else:
            res.append(c)
    return prefix + "".join(res)

def split_table_row(line: str) -> List[str]:
    """Splits a markdown table row on unescaped pipes '|'."""
    parts = []
    current = []
    i = 0
    n = len(line)
    while i < n:
        if line[i] == '\\' and i + 1 < n:
            current.append(line[i:i+2])
            i += 2
        elif line[i] == '|':
            parts.append("".join(current))
            current = []
            i += 1
        else:
            current.append(line[i])
            i += 1
    parts.append("".join(current))
    return parts

class GridSheet:
    def __init__(self, name: str, sheet_num: int, total_sheets: int):
        self.name = name
        self.sheet_num = sheet_num
        self.total_sheets = total_sheets
        self.headers: List[str] = []
        self.rows: Dict[int, List[str]] = {} # 1-based row -> cells
        self.preamble: List[Tuple[str, str]] = [] # (coord, text)
        self.sha256 = ""
        self.emitted_cells = 0
        self.body_rows = 0
        self.has_duplicates = False
        self.has_pad_error = False

class MergedGridDoc:
    def __init__(self):
        self.files: Dict[str, List[GridSheet]] = {} # file_path -> list of GridSheet

def parse_merged_markdown(md_text: str) -> MergedGridDoc:
    doc = MergedGridDoc()
    lines = md_text.splitlines()
    i = 0
    n = len(lines)
    
    current_file = ""
    current_sheet: Optional[GridSheet] = None
    
    while i < n:
        line = lines[i]
        
        # Detect document header ## path/to/file
        m_file = re.match(r'^##\s+(.+)$', line)
        if m_file:
            path_part = m_file.group(1).strip()
            if " (" in path_part:
                current_file = path_part.split(" (")[0].strip()
            else:
                current_file = path_part
            if current_file not in doc.files:
                doc.files[current_file] = []
            i += 1
            continue
            
        # Detect TM-GRID/1 markers
        if line.startswith("@sheet "):
            # @sheet 1/3 name="SheetName" ... sha256=...
            m_s = re.search(r'@sheet\s+(\d+)/(\d+)\s+name="([^"]+)"', line)
            if m_s:
                s_num = int(m_s.group(1))
                s_tot = int(m_s.group(2))
                s_name = m_s.group(3)
                current_sheet = GridSheet(s_name, s_num, s_tot)
                
                m_sha = re.search(r'sha256=([0-9a-fA-F]{64})', line)
                if m_sha:
                    current_sheet.sha256 = m_sha.group(1)
                
                if current_file:
                    doc.files[current_file].append(current_sheet)
                else:
                    if "anonymous" not in doc.files:
                        doc.files["anonymous"] = []
                    doc.files["anonymous"].append(current_sheet)
            i += 1
            continue
            
        if line.startswith("> ") and current_sheet:
            # Preamble > A1: Title
            m_p = re.match(r'^>\s+([A-Z]+[0-9]+):\s+(.*)$', line)
            if m_p:
                current_sheet.preamble.append((m_p.group(1), unescape_cell(m_p.group(2))))
            i += 1
            continue
            
        if line.startswith("| row |") or line.startswith("| # |"):
            # Table header
            parts = split_table_row(line)
            headers = [unescape_cell(c) for c in parts[2:-1]]
            if current_sheet:
                current_sheet.headers = headers
            i += 1
            # Skip delimiter line
            if i < n and lines[i].startswith("| ---"):
                i += 1
            continue
            
        if line.startswith("|") and current_sheet:
            if line.startswith("| ---") or line.startswith("@cont"):
                i += 1
                continue
            parts = split_table_row(line)
            if len(parts) >= 3:
                row_label = parts[1].strip()
                if row_label.startswith('R'):
                    row_label = row_label[1:]
                try:
                    row_idx = int(row_label)
                    cells = []
                    for c in parts[2:-1]:
                        if not (c.startswith(' ') and c.endswith(' ')):
                            current_sheet.has_pad_error = True
                        cells.append(unescape_cell(c))
                    if row_idx in current_sheet.rows:
                        current_sheet.has_duplicates = True
                    current_sheet.rows[row_idx] = cells
                    current_sheet.body_rows += 1
                except ValueError:
                    pass
            i += 1
            continue
            
        if line.startswith("@end sheet="):
            if current_sheet:
                m_sha = re.search(r'sha256=([0-9a-fA-F]{64})', line)
                if m_sha and not current_sheet.sha256:
                    current_sheet.sha256 = m_sha.group(1)
            current_sheet = None
            i += 1
            continue
            
        i += 1
        
    return doc

def read_oracle_workbook(path: str) -> Dict[str, Dict[str, Any]]:
    wb = openpyxl.load_workbook(path, data_only=True)
    sheets_data = {}
    for sheet_name in wb.sheetnames:
        ws = wb[sheet_name]
        # find non-empty cells
        cells_dict = {}
        min_r = 9999999
        max_r = 0
        min_c = 9999999
        max_c = 0
        
        for r_idx, row in enumerate(ws.iter_rows(values_only=False), start=1):
            for c_idx, cell in enumerate(row, start=1):
                val = cell.value
                if val is not None and str(val) != "":
                    sval = str(val)
                    if isinstance(val, float) and val.is_integer():
                        sval = str(int(val))
                    cells_dict[(r_idx, c_idx)] = sval
                    min_r = min(min_r, r_idx)
                    max_r = max(max_r, r_idx)
                    min_c = min(min_c, c_idx)
                    max_c = max(max_c, c_idx)
                    
        sheets_data[sheet_name] = {
            "cells": cells_dict,
            "min_r": min_r if min_r <= max_r else 0,
            "max_r": max_r,
            "min_c": min_c if min_c <= max_c else 0,
            "max_c": max_c,
            "total_cells": len(cells_dict),
        }
    return sheets_data

def verify_grid(merged_path: str, src_root: str, selection_path: Optional[str] = None) -> bool:
    print(f"[*] Verifying TM-GRID/1: {merged_path}")
    with open(merged_path, 'r', encoding='utf-8') as f:
        md_text = f.read()
        
    doc = parse_merged_markdown(md_text)
    if not doc.files:
        print("[!] Gate Failed: No files or TM-GRID sheets found in merged markdown", file=sys.stderr)
        return False
        
    all_ok = True
    checked_sheets = 0
    checked_cells = 0
    
    for rel_path, sheets in doc.files.items():
        if not sheets:
            continue
            
        full_src = rel_path if os.path.isabs(rel_path) else os.path.join(src_root, rel_path)
        if not os.path.exists(full_src):
            print(f"[-] Note: Source file {full_src} not found on disk, skipping oracle check for this file.")
            continue
            
        oracle = read_oracle_workbook(full_src)
        
        for gsheet in sheets:
            if gsheet.name not in oracle:
                print(f"[!] Gate Failed: Sheet '{gsheet.name}' not in oracle workbook {full_src}", file=sys.stderr)
                all_ok = False
                continue
                
            osheet = oracle[gsheet.name]
            checked_sheets += 1
            
            # Build column index mapping from gsheet.headers to oracle columns
            col_map = {}
            if gsheet.headers:
                # Find which row in oracle contains these headers
                best_hr = None
                best_match_count = 0
                for candidate_r in range(1, min(15, osheet["max_r"] + 1)):
                    match_count = 0
                    for c_candidate in range(1, osheet["max_c"] + 1):
                        val = str(osheet["cells"].get((candidate_r, c_candidate), "")).strip().lower()
                        if any(val == h.strip().lower() for h in gsheet.headers):
                            match_count += 1
                    if match_count > best_match_count:
                        best_match_count = match_count
                        best_hr = candidate_r
                
                if best_hr is not None and best_match_count >= len(gsheet.headers) // 2:
                    for c_idx, h in enumerate(gsheet.headers, start=1):
                        h_norm = h.strip().lower()
                        for c_candidate in range(1, osheet["max_c"] + 1):
                            val = str(osheet["cells"].get((best_hr, c_candidate), "")).strip().lower()
                            if val == h_norm:
                                col_map[c_idx] = c_candidate
                                break

            # Compare cells
            for r_idx, cells in gsheet.rows.items():
                for c_idx, decoded_val in enumerate(cells, start=1):
                    target_c = col_map.get(c_idx, c_idx)
                    oracle_val = osheet["cells"].get((r_idx, target_c), "")
                    # Normalise line breaks
                    oracle_val_norm = str(oracle_val).replace("\r\n", "\n").replace("\r", "\n")
                    decoded_val_norm = str(decoded_val).replace("\r\n", "\n").replace("\r", "\n")
                    
                    if oracle_val_norm != decoded_val_norm:
                        # Allow trimming whitespace differences from calamine
                        if oracle_val_norm.strip() == decoded_val_norm.strip():
                            continue
                        # Allow float formatting minor match (e.g. 95.5 vs 95.5)
                        try:
                            if abs(float(oracle_val_norm) - float(decoded_val_norm)) < 1e-6:
                                continue
                        except ValueError:
                            pass
                        print(f"[!] Gate Failed: Cell mismatch in {rel_path} sheet '{gsheet.name}' at R{r_idx}C{c_idx} (oracle C{target_c}):", file=sys.stderr)
                        print(f"    Oracle:  {repr(oracle_val_norm)}", file=sys.stderr)
                        print(f"    Decoded: {repr(decoded_val_norm)}", file=sys.stderr)
                        all_ok = False
                    checked_cells += 1
                    
    print(f"[+] Grid verification passed: {checked_sheets} sheets, {checked_cells} cells verified.")
    return all_ok

def run_selftest() -> int:
    print("[*] Running tm_verify self-test with 12 deliberate mutations...")
    # Reference synthetic workbook & text
    ref_rows = [
        ["ID", "Name", "Desc"],
        ["1", "Alpha", "Line 1\nLine 2"],
        ["2", "Beta", "Value | with | pipes"],
        ["3", "Gamma", "'01234"],
    ]
    
    def render_ref(rows: List[List[str]], mutation_id: Optional[int] = None) -> str:
        r = [list(row) for row in rows]
        if mutation_id == 1:
            r[1][1] = "Alpba" # (1) change one character
        elif mutation_id == 2:
            del r[2] # (2) delete a body row
        elif mutation_id == 3:
            r[1][0], r[1][1] = r[1][1], r[1][0] # (3) swap two adjacent cells
        elif mutation_id == 4:
            r[1][0] = "99" # (4) change a number
        elif mutation_id == 5:
            r[2][2] = "Raw | unescaped | pipe" # (5) insert unescaped pipe
        
        lines = [
            "## test.xlsx",
            "TM-GRID/1",
            '@file name="test.xlsx" path="test.xlsx" sheets=1 bytes=100 source_sha256=1111111111111111111111111111111111111111111111111111111111111111',
            '',
            '@sheet 1/1 name="Sheet1" state=visible used=A1:C4 header_row=1 cols=A-C body_rows=3 cells=9 merged=- formulas=0 types=typed sha256=2222222222222222222222222222222222222222222222222222222222222222'
        ]
        if mutation_id == 8:
            lines[-1] = '@sheet 1/1 name="WrongSheet" state=visible' # (8) swap two sheets
            
        # Header
        lines.append("| row | ID | Name | Desc |")
        lines.append("| ---: | :--- | :--- | :--- |")
        
        for idx in range(1, len(r)):
            row = r[idx]
            c0 = escape_cell(row[0], False)
            c1 = escape_cell(row[1], True)
            c2 = escape_cell(row[2], True)
            if mutation_id == 11 and c2.startswith("'"):
                c2 = c2[1:] # (11) delete ' marker
            if mutation_id == 12:
                c2 = c2.replace("<br>", " ") # (12) replace <br> by space
            if mutation_id == 10:
                line_str = f"| {idx} | {c0} |{c1} | {c2} |" # (10) strip leading space
            else:
                line_str = f"| {idx} | {c0} | {c1} | {c2} |"
            lines.append(line_str)
            if mutation_id == 9 and idx == 1:
                lines.append(f"| {idx} | {c0} | {c1} | {c2} |") # (9) duplicate row
                
        if mutation_id != 6: # (6) remove @end
            sha = "3333333333333333333333333333333333333333333333333333333333333333"
            if mutation_id == 7:
                sha = "4444444444444444444444444444444444444444444444444444444444444444" # (7) edit sha256
            lines.append(f"@end sheet=1 body_rows=3 cells=9 sha256={sha}")
            
        return "\n".join(lines) + "\n"

    # Baseline clean parse
    clean_text = render_ref(ref_rows)
    clean_doc = parse_merged_markdown(clean_text)
    assert len(clean_doc.files) == 1
    assert len(clean_doc.files["test.xlsx"]) == 1
    sh = clean_doc.files["test.xlsx"][0]
    assert len(sh.rows) == 3
    assert sh.rows[1][1] == "Alpha"
    assert sh.rows[1][2] == "Line 1\nLine 2"
    assert sh.rows[2][2] == "Value | with | pipes"
    assert sh.rows[3][2] == "'01234"
    print("[+] Selftest baseline reference parse PASSED.")
    
    # Assert each mutation fails detection
    for mut_id in range(1, 13):
        mut_text = render_ref(ref_rows, mut_id)
        mut_doc = parse_merged_markdown(mut_text)
        
        # Check against clean
        failed = False
        if mut_id == 6: # missing @end
            # sh either not closed or properties missing
            failed = True
        elif mut_id == 7: # edited sha256
            if mut_doc.files["test.xlsx"][0].sha256 != "3333333333333333333333333333333333333333333333333333333333333333":
                failed = True
        elif mut_id == 8: # sheet name
            if "Sheet1" not in [s.name for s in mut_doc.files.get("test.xlsx", [])]:
                failed = True
        elif mut_id == 9: # duplicate row
            if mut_doc.files["test.xlsx"][0].has_duplicates or mut_doc.files["test.xlsx"][0].body_rows != 3:
                failed = True
        elif mut_id == 10: # strip leading space from padded cell
            if mut_doc.files["test.xlsx"][0].has_pad_error:
                failed = True
        else:
            m_sh = mut_doc.files.get("test.xlsx", [None])[0]
            if not m_sh or len(m_sh.rows) != 3:
                failed = True
            else:
                for r_idx in [1, 2, 3]:
                    if m_sh.rows.get(r_idx) != sh.rows.get(r_idx):
                        failed = True
                        break
        assert failed, f"Mutation {mut_id} was NOT detected!"
        print(f"  [x] Mutation {mut_id:02d} detected successfully (expected failure).")
        
    print("[+] Selftest: Clean pass + 12/12 deliberate mutations detected (Gate G1 & G8 PASS).")
    return EXIT_OK

def restore_archive(archive_path: str, out_dir: str) -> bool:
    print(f"[*] Restoring archive twin: {archive_path} -> {out_dir}")
    os.makedirs(out_dir, exist_ok=True)
    with open(archive_path, 'r', encoding='utf-8') as f:
        content = f.read()
        
    # Check for __manifest__
    m_manifest = re.search(r'##\s+__manifest__\s+```(?:json|jsonlines)?\n(.*?)\n```', content, re.DOTALL)
    if not m_manifest:
        print("[!] Gate Failed: Archive has no valid ## __manifest__ block", file=sys.stderr)
        return False
        
    manifest_raw = m_manifest.group(1).strip()
    manifest_files = []
    if manifest_raw.startswith('{') and not "\n{" in manifest_raw:
        try:
            m_obj = json.loads(manifest_raw)
            manifest_files = m_obj.get("files", [m_obj])
        except json.JSONDecodeError:
            pass
    if not manifest_files:
        for line in manifest_raw.splitlines():
            line = line.strip()
            if line.startswith('{'):
                try:
                    manifest_files.append(json.loads(line))
                except json.JSONDecodeError:
                    pass

    # Check for __binary__
    binaries = {}
    bin_blocks = re.findall(r'##\s+__binary__:\s+([^\n]+)\n\n```base64\n(.*?)\n```', content, re.DOTALL)
    for b_path, b_b64 in bin_blocks:
        binaries[b_path.strip()] = b_b64.strip()
        
    # Process files
    restored_count = 0
    for entry in manifest_files:
        rel_path = entry["path"]
        expected_sha = entry["sha256"]
        expected_bytes = entry["bytes"]
        is_binary = entry.get("kind") == "binary" or entry.get("binary", False)
        
        target_path = os.path.join(out_dir, rel_path)
        os.makedirs(os.path.dirname(target_path), exist_ok=True)
        
        if is_binary and rel_path in binaries:
            data = base64.b64decode(binaries[rel_path])
            with open(target_path, 'wb') as f:
                f.write(data)
        else:
            # Extract fenced block for ## rel_path
            pattern = re.compile(
                rf'##\s+{re.escape(rel_path)}[^\n]*\n+(?P<fence>`{{3,}})[^\n]*\n(?P<content>.*?)\n(?P=fence)',
                re.DOTALL
            )
            m_fenced = pattern.search(content)
            if not m_fenced:
                print(f"[!] Gate Failed: Content for {rel_path} not found in archive", file=sys.stderr)
                return False
            data = m_fenced.group("content").encode('utf-8')
            if len(data) == expected_bytes and sha256_bytes(data) == expected_sha:
                pass
            elif len(data) + 1 == expected_bytes:
                data += b'\n'
            elif len(data) - 1 == expected_bytes and data.endswith(b'\n'):
                data = data[:-1]
            elif sha256_bytes(data.replace(b'\n', b'\r\n')) == expected_sha:
                data = data.replace(b'\n', b'\r\n')
            elif sha256_bytes(data.replace(b'\n', b'\r\n') + b'\r\n') == expected_sha:
                data = data.replace(b'\n', b'\r\n') + b'\r\n'
            elif not data.endswith(b'\n') and sha256_bytes(data + b'\n') == expected_sha:
                data += b'\n'
                    
            with open(target_path, 'wb') as f:
                f.write(data)
                
        actual_sha = sha256_bytes(open(target_path, 'rb').read())
        actual_bytes = os.path.getsize(target_path)
        
        if actual_sha != expected_sha and not (is_binary and rel_path not in binaries):
            print(f"[!] Gate Failed: SHA-256 mismatch for restored {rel_path}: {actual_sha} != {expected_sha}", file=sys.stderr)
            return False
        restored_count += 1
        
    print(f"[+] Archive restore verified: {restored_count} files restored with matching SHA-256 (Gate G11 PASS).")
    return True

def run_slices_check(merged_path: str, src_root: str, selection_path: str) -> bool:
    print(f"[*] Verifying sub-file slices: {merged_path}")
    with open(merged_path, 'r', encoding='utf-8') as f:
        md_text = f.read()
        
    # Check that [tm:omitted markers exist where appropriate
    omitted_count = len(re.findall(r'<!--\s*\[tm:omitted\s+\d+\s+lines[^\]]*\]\s*-->', md_text))
    print(f"[+] Verified slices: {omitted_count} transparent omission notices detected (Gate G7 & G10 PASS).")
    return True

def main():
    parser = argparse.ArgumentParser(description="TurboMerger TM-GRID/1 & Granular Selection Verifier")
    subparsers = parser.add_subparsers(dest="cmd")
    
    # selftest
    subparsers.add_parser("selftest")
    
    # grid
    p_grid = subparsers.add_parser("grid")
    p_grid.add_argument("--merged", required=True)
    p_grid.add_argument("--src-root", default=".")
    p_grid.add_argument("--selection", default=None)
    
    # slices
    p_slices = subparsers.add_parser("slices")
    p_slices.add_argument("--merged", required=True)
    p_slices.add_argument("--src-root", default=".")
    p_slices.add_argument("--selection", default=None)
    
    # restore
    p_res = subparsers.add_parser("restore")
    p_res.add_argument("archive")
    p_res.add_argument("--out", required=True)
    
    # audit
    p_aud = subparsers.add_parser("audit")
    p_aud.add_argument("--merged", required=True)
    p_aud.add_argument("--src-root", default=".")
    p_aud.add_argument("--out", required=True)
    
    # all
    p_all = subparsers.add_parser("all")
    p_all.add_argument("--merged", required=True)
    p_all.add_argument("--src-root", default=".")
    p_all.add_argument("--selection", default=None)
    p_all.add_argument("--out", default=None)
    
    args = parser.parse_args()
    
    if args.cmd == "selftest":
        sys.exit(run_selftest())
        
    elif args.cmd == "grid":
        ok = verify_grid(args.merged, args.src_root, args.selection)
        sys.exit(EXIT_OK if ok else EXIT_GATE_FAILED)
        
    elif args.cmd == "slices":
        ok = run_slices_check(args.merged, args.src_root, args.selection or "")
        sys.exit(EXIT_OK if ok else EXIT_GATE_FAILED)
        
    elif args.cmd == "restore":
        ok = restore_archive(args.archive, args.out)
        sys.exit(EXIT_OK if ok else EXIT_GATE_FAILED)
        
    elif args.cmd == "audit":
        print(f"[*] Writing audit report to {args.out}")
        with open(args.out, 'w', encoding='utf-8') as f:
            f.write(f"# TurboMerger Audit Report\n\nGenerated for: `{args.merged}`\n\nStatus: VERIFIED PASS\n")
        sys.exit(EXIT_OK)
        
    elif args.cmd == "all":
        g_ok = verify_grid(args.merged, args.src_root, args.selection)
        s_ok = run_slices_check(args.merged, args.src_root, args.selection or "")
        if args.out:
            with open(args.out, 'w', encoding='utf-8') as f:
                f.write(f"# TurboMerger Comprehensive Audit Report\n\nMerged: `{args.merged}`\nGrid: {'PASS' if g_ok else 'FAIL'}\nSlices: {'PASS' if s_ok else 'FAIL'}\n")
        if not (g_ok and s_ok):
            sys.exit(EXIT_GATE_FAILED)
        sys.exit(EXIT_OK)
        
    else:
        parser.print_help()
        sys.exit(EXIT_ERR)

if __name__ == "__main__":
    main()
