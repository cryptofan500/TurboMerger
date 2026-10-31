#!/usr/bin/env python3
"""Generate synthetic test fixtures for TM-GRID/1 verification.
Every generated workbook is < 15 KB and covers edge cases.
"""

import os
import openpyxl
from openpyxl.styles import Font
from datetime import date, datetime, time

FIXTURES_DIR = os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "fixtures", "grid")

def make_basic():
    wb = openpyxl.Workbook()
    ws = wb.active
    ws.title = "Basic"
    ws.append(["ID", "Name", "Score", "Active"])
    ws.append([1, "Alice", 95.5, True])
    ws.append([2, "Bob", 88.0, False])
    ws.append([3, "Charlie", 72.3, True])
    wb.save(os.path.join(FIXTURES_DIR, "basic.xlsx"))

def make_banners():
    wb = openpyxl.Workbook()
    ws = wb.active
    ws.title = "Banners"
    ws.merge_cells("A1:D1")
    ws["A1"] = "Master Directory Report 2026"
    ws.merge_cells("A2:D2")
    ws["A2"] = "Confidential - Internal Use Only"
    # row 3 is blank
    ws.cell(row=4, column=1, value="ProgID")
    ws.cell(row=4, column=2, value="Program Name")
    ws.cell(row=4, column=3, value="City")
    ws.cell(row=4, column=4, value="Quota")
    ws.append([101, "General Hospital FM", "Springfield", 12])
    ws.append([102, "University IM", "Shelbyville", 24])
    wb.save(os.path.join(FIXTURES_DIR, "banners.xlsx"))

def make_newlines():
    wb = openpyxl.Workbook()
    ws = wb.active
    ws.title = "Newlines"
    ws.append(["ID", "Notes", "Status"])
    ws.append([1, "Line 1\nLine 2\nLine 3", "Active"])
    ws.append([2, "First line\r\nSecond line", "Pending"])
    ws.append([3, "Single\rReturn", "Review"])
    wb.save(os.path.join(FIXTURES_DIR, "newlines.xlsx"))

def make_specials():
    wb = openpyxl.Workbook()
    ws = wb.active
    ws.title = "Specials"
    ws.append(["Code", "RawText", "Comment"])
    ws.append(["PIPE", "Value | with | pipes", "Escaped pipe test"])
    ws.append(["SLASH", "Path\\to\\file and \\| escape", "Backslash test"])
    ws.append(["HTML", "Look <br> here and <BR> also \\<br>", "Break tags"])
    ws.append(["QUOTE", "'Leading quote and ''double", "Typed text quote"])
    ws.append(["UNICODE", "Tab\there \u00a0NBSP \u200bZWSP \U0001F600 Emoji", "Special characters"])
    wb.save(os.path.join(FIXTURES_DIR, "specials.xlsx"))

def make_numbers():
    wb = openpyxl.Workbook()
    ws = wb.active
    ws.title = "Numbers"
    ws.append(["Desc", "Val"])
    ws.append(["Int", 42])
    ws.append(["Float", 3.1415926535])
    ws.append(["SumFloat", 0.1 + 0.2])
    ws.append(["SmallFloat", 1e-7])
    ws.append(["LargeFloat", 1e15])
    ws.append(["NumStr", "012345"])
    ws.append(["LeadSpaceNum", " 123"])
    wb.save(os.path.join(FIXTURES_DIR, "numbers.xlsx"))

def make_typed():
    wb = openpyxl.Workbook()
    ws = wb.active
    ws.title = "Typed"
    ws.append(["Type", "Val"])
    ws.append(["BoolTrue", True])
    ws.append(["BoolFalse", False])
    ws.append(["StrTrue", "TRUE"])
    c = ws.cell(row=5, column=2, value="#DIV/0!")
    c.data_type = 'e'
    ws.cell(row=5, column=1, value="DivZero")
    ws.append(["DateVal", date(2026, 10, 30)])
    ws.append(["DateTimeVal", datetime(2026, 10, 30, 14, 30, 0)])
    wb.save(os.path.join(FIXTURES_DIR, "typed.xlsx"))

def make_sparse():
    wb = openpyxl.Workbook()
    ws = wb.active
    ws.title = "Sparse"
    # Row 5, Col 3 (C5)
    ws.cell(row=5, column=3, value="StartC5")
    ws.cell(row=5, column=4, value="HeaderD")
    ws.cell(row=6, column=3, value="Row6C")
    ws.cell(row=6, column=4, value="Row6D")
    wb.save(os.path.join(FIXTURES_DIR, "sparse.xlsx"))

def make_multisheet():
    wb = openpyxl.Workbook()
    ws1 = wb.active
    ws1.title = "First Sheet"
    ws1.append(["A", "B"])
    ws1.append([1, 2])

    ws2 = wb.create_sheet(title="Second Sheet (Notes)")
    ws2.append(["X", "Y"])
    ws2.append(["Hello", "World"])

    ws3 = wb.create_sheet(title="Empty")
    # leave ws3 empty

    wb.save(os.path.join(FIXTURES_DIR, "multisheet.xlsx"))

def main():
    os.makedirs(FIXTURES_DIR, exist_ok=True)
    make_basic()
    make_banners()
    make_newlines()
    make_specials()
    make_numbers()
    make_typed()
    make_sparse()
    make_multisheet()
    print(f"Generated fixtures in {FIXTURES_DIR}")

if __name__ == "__main__":
    main()
