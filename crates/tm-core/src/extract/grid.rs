//! TM-GRID/1 spreadsheet extraction engine (ADR 0021).
//! Converts spreadsheets into verified, unambiguous, column-aligned GFM tables
//! with absolute row IDs, reversible `<br>` escapes, and per-sheet SHA-256 provenance.

use std::fs::File;
use std::io::Read;
use std::path::Path;
use calamine::{open_workbook_auto, Data, DataType, Reader, Sheets};
use sha2::{Digest, Sha256};
use crate::extract::Extracted;
use crate::selection::GridSelect;

/// Converts 0-based column index to Excel column letters (0 -> "A", 25 -> "Z", 26 -> "AA").
pub fn col_letter(mut col_idx: u32) -> String {
    let mut s = String::new();
    loop {
        let rem = (col_idx % 26) as u8;
        s.push((b'A' + rem) as char);
        if col_idx < 26 {
            break;
        }
        col_idx = (col_idx / 26) - 1;
    }
    s.chars().rev().collect()
}

/// Converts Excel column letters back to 0-based index ("A" -> 0, "Z" -> 25, "AA" -> 26).
pub fn col_from_letter(s: &str) -> Option<u32> {
    let s = s.trim().to_ascii_uppercase();
    if s.is_empty() || !s.chars().all(|c| c.is_ascii_uppercase()) {
        return None;
    }
    let mut idx = 0u32;
    for ch in s.bytes() {
        idx = idx.checked_mul(26)?.checked_add((ch - b'A' + 1) as u32)?;
    }
    Some(idx - 1)
}

/// Convert canonical cell value to cell type marker: s, n, b, e, d
pub fn kind_of(data: &Data) -> &'static str {
    match data {
        Data::Empty => "s",
        Data::String(_) => "s",
        Data::Int(_) | Data::Float(_) => "n",
        Data::Bool(_) => "b",
        Data::DateTime(_) | Data::DateTimeIso(_) | Data::DurationIso(_) => "d",
        Data::Error(_) => "e",
    }
}

/// Canonical string representation of a cell value according to TM-GRID/1.
pub fn canon_text(data: &Data) -> String {
    match data {
        Data::Empty => String::new(),
        Data::String(s) => s.clone(),
        Data::Int(i) => i.to_string(),
        Data::Float(f) => {
            if f.is_nan() || f.is_infinite() {
                "#NUM!".to_string()
            } else if f.fract() == 0.0 && *f >= i64::MIN as f64 && *f <= i64::MAX as f64 {
                // Integer value formatted as integer
                format!("{:.0}", f)
            } else {
                f.to_string()
            }
        }
        Data::Bool(b) => if *b { "TRUE".into() } else { "FALSE".into() },
        Data::DateTime(dt) => {
            let (y, m, d, hh, mm, ss, ms) = dt.to_ymd_hms_milli();
            if hh == 0 && mm == 0 && ss == 0 && ms == 0 {
                format!("{:04}-{:02}-{:02}", y, m, d)
            } else if ms == 0 {
                format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", y, m, d, hh, mm, ss)
            } else {
                format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}", y, m, d, hh, mm, ss, ms)
            }
        }
        Data::DateTimeIso(s) => s.clone(),
        Data::DurationIso(s) => s.clone(),
        Data::Error(err) => format!("{:?}", err),
    }
}

/// Tests whether text matches NUM pattern: ^-?(0|[1-9][0-9]*)(\.[0-9]+)?$
fn matches_num(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let mut i = 0;
    if bytes[0] == b'-' {
        i += 1;
        if i >= bytes.len() {
            return false;
        }
    }
    if bytes[i] == b'0' {
        i += 1;
    } else if bytes[i] >= b'1' && bytes[i] <= b'9' {
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    } else {
        return false;
    }

    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        let frac_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == frac_start {
            return false;
        }
    }
    i == bytes.len()
}

/// Tests whether text matches standard BOOL, ERR, or DATE patterns.
fn matches_bool(s: &str) -> bool {
    s == "TRUE" || s == "FALSE"
}

fn matches_err(s: &str) -> bool {
    matches!(
        s,
        "#NULL!" | "#DIV/0!" | "#VALUE!" | "#REF!" | "#NAME?" | "#NUM!" | "#N/A" | "#GETTING_DATA"
    )
}

fn matches_date(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() == 10 && b[4] == b'-' && b[7] == b'-' {
        return true;
    }
    if b.len() >= 19 && b[4] == b'-' && b[7] == b'-' && b[10] == b' ' && b[13] == b':' && b[16] == b':' {
        return true;
    }
    false
}

/// Encodes cell text for TM-GRID/1 markdown table.
pub fn escape_cell(canon: &str, is_string: bool) -> String {
    let mut s = String::with_capacity(canon.len() + 8);

    // 1. Typed-text marker
    if is_string {
        if canon.starts_with('\'') || matches_num(canon) || matches_bool(canon) || matches_err(canon) || matches_date(canon) {
            s.push('\'');
        }
    }

    // 2. Escape backslashes, pipes, <br>, newlines, controls
    let mut chars = canon.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => s.push_str("\\\\"),
            '|' => s.push_str("\\|"),
            '<' => {
                // Check if starts with <br>
                let rest: String = chars.clone().take(3).collect();
                if rest == "br>" {
                    s.push_str("\\<br>");
                    chars.next();
                    chars.next();
                    chars.next();
                } else {
                    s.push('<');
                }
            }
            '\n' => s.push_str("<br>"),
            '\r' => s.push_str("\\r"),
            '\t' => s.push_str("\\t"),
            c if (c as u32) < 0x20 || (c as u32) == 0x7F => {
                s.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => s.push(c),
        }
    }

    s
}

/// Decodes cell text (used for verification and tests).
pub fn decode_cell(raw: &str) -> (String, &'static str) {
    let mut text = raw.trim();
    if text.starts_with(' ') {
        text = &text[1..];
    }
    if text.ends_with(' ') {
        text = &text[..text.len() - 1];
    }

    let is_quoted = text.starts_with('\'');
    let mut unescaped = String::with_capacity(text.len());
    let chars_to_read = if is_quoted { &text[1..] } else { text };

    let mut it = chars_to_read.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\\' {
            if let Some(&next) = it.peek() {
                match next {
                    '\\' => { unescaped.push('\\'); it.next(); }
                    '|' => { unescaped.push('|'); it.next(); }
                    'r' => { unescaped.push('\r'); it.next(); }
                    't' => { unescaped.push('\t'); it.next(); }
                    '<' => {
                        unescaped.push('<');
                        it.next();
                    }
                    'x' => {
                        it.next();
                        let h1 = it.next().unwrap_or('0');
                        let h2 = it.next().unwrap_or('0');
                        let hex = format!("{}{}", h1, h2);
                        if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                            unescaped.push(byte as char);
                        }
                    }
                    _ => unescaped.push('\\'),
                }
            } else {
                unescaped.push('\\');
            }
        } else if c == '<' {
            let peek_rest: String = it.clone().take(3).collect();
            if peek_rest == "br>" {
                unescaped.push('\n');
                it.next();
                it.next();
                it.next();
            } else {
                unescaped.push('<');
            }
        } else {
            unescaped.push(c);
        }
    }

    if is_quoted {
        (unescaped, "s")
    } else if matches_num(&unescaped) {
        (unescaped, "n")
    } else if matches_bool(&unescaped) {
        (unescaped, "b")
    } else if matches_err(&unescaped) {
        (unescaped, "e")
    } else if matches_date(&unescaped) {
        (unescaped, "d")
    } else {
        (unescaped, "s")
    }
}

/// Escape string to valid JSON string literal without surrounding quotes.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            ch if (ch as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out
}

/// Encode workbook into TM-GRID/1 markdown format.
pub fn encode_workbook(
    path: &Path,
    select: &GridSelect,
    chunk_rows: usize,
) -> Result<Extracted, String> {
    let mut file = File::open(path).map_err(|e| format!("cannot open spreadsheet: {e}"))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|e| format!("read failed: {e}"))?;
    let file_bytes = bytes.len();
    let source_sha256 = format!("{:x}", Sha256::digest(&bytes));

    let mut wb = open_workbook_auto(path).map_err(|e| format!("calamine open failed: {e}"))?;
    let sheet_names = wb.sheet_names().to_vec();
    let total_sheets_in_wb = sheet_names.len();

    let mut out = String::new();
    out.push_str("TM-GRID/1\n");
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("workbook.xlsx");
    out.push_str(&format!(
        "@file name=\"{}\" path=\"{}\" sheets={} bytes={} source_sha256={}\n\n",
        file_name, file_name, total_sheets_in_wb, file_bytes, source_sha256
    ));

    let mut emitted_sheets = 0usize;
    for (sheet_idx, sheet_name) in sheet_names.iter().enumerate() {
        // Filter sheets
        if !select.sheets.is_empty() && !select.sheets.iter().any(|s| s == sheet_name) {
            continue;
        }
        if select.exclude_sheets.iter().any(|s| s == sheet_name) {
            continue;
        }

        let sheet_override = select.sheet_overrides.iter().find(|o| o.name == *sheet_name);

        let range = match wb.worksheet_range(sheet_name) {
            Ok(r) => r,
            Err(_) => {
                out.push_str(&format!(
                    "@sheet {}/{} name=\"{}\" state=visible skipped=\"unreadable\"\n@end sheet={} skipped\n\n",
                    sheet_idx + 1, total_sheets_in_wb, sheet_name, sheet_idx + 1
                ));
                continue;
            }
        };

        let (rows_count, cols_count) = range.get_size();
        if (rows_count as u64) * (cols_count as u64) > 10_000_000 {
            return Err("sheet range too large".into());
        }

        let start_pos = range.start().unwrap_or((0, 0));
        let base_row = start_pos.0; // 0-based
        let base_col = start_pos.1; // 0-based

        // Scan non-empty cells and bounding box
        let mut min_r = u32::MAX;
        let mut max_r = 0u32;
        let mut min_c = u32::MAX;
        let mut max_c = 0u32;
        let mut has_cells = false;

        for r in 0..rows_count as u32 {
            for c in 0..cols_count as u32 {
                if let Some(cell) = range.get((r as usize, c as usize)) {
                    if !cell.is_empty() && cell.to_string() != "" {
                        has_cells = true;
                        let abs_r = base_row + r;
                        let abs_c = base_col + c;
                        min_r = min_r.min(abs_r);
                        max_r = max_r.max(abs_r);
                        min_c = min_c.min(abs_c);
                        max_c = max_c.max(abs_c);
                    }
                }
            }
        }

        if !has_cells {
            out.push_str(&format!(
                "@sheet {}/{} name=\"{}\" state=visible used=- header_row=none cols=- body_rows=0 cells=0 merged=- formulas=0 types=typed sha256={}\n",
                sheet_idx + 1, total_sheets_in_wb, sheet_name, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
            ));
            out.push_str(&format!(
                "@end sheet={} body_rows=0 cells=0 sha256={}\n\n",
                sheet_idx + 1, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
            ));
            emitted_sheets += 1;
            continue;
        }

        let used_str = format!("{}{}:{}{}", col_letter(min_c), min_r + 1, col_letter(max_c), max_r + 1);

        // Header row heuristic
        let header_row_1based: Option<u32> = if let Some(hr) = sheet_override.and_then(|o| o.header_row).or(select.header_row) {
            Some(hr)
        } else {
            let mut detected = None;
            for r in min_r..=max_r {
                let mut non_empty_str_count = 0;
                let mut non_empty_count = 0;
                for c in min_c..=max_c {
                    let rel_r = r - base_row;
                    let rel_c = c - base_col;
                    if let Some(cell) = range.get((rel_r as usize, rel_c as usize)) {
                        if !cell.is_empty() && cell.to_string() != "" {
                            non_empty_count += 1;
                            if matches!(cell, Data::String(_)) {
                                non_empty_str_count += 1;
                            }
                        }
                    }
                }
                let used_width = max_c - min_c + 1;
                let half_width = (used_width + 1) / 2;
                let threshold = 2.max(half_width);
                if non_empty_count >= threshold && non_empty_str_count == non_empty_count {
                    detected = Some(r + 1);
                    break;
                }
            }
            detected
        };

        // Columns projection
        let active_cols: Vec<u32> = if let Some(cols_spec) = sheet_override.map(|o| &o.columns).or_else(|| {
            if select.columns.is_empty() { None } else { Some(&select.columns) }
        }) {
            let mut picked = Vec::new();
            for col_req in cols_spec {
                if let Some(idx) = col_from_letter(col_req) {
                    if idx >= min_c && idx <= max_c {
                        picked.push(idx);
                        continue;
                    }
                }
                // Try matching against header text
                if let Some(hr) = header_row_1based {
                    let rel_r = (hr - 1) - base_row;
                    for c in min_c..=max_c {
                        let rel_c = c - base_col;
                        if let Some(cell) = range.get((rel_r as usize, rel_c as usize)) {
                            if cell.to_string().trim() == col_req.trim() {
                                picked.push(c);
                                break;
                            }
                        }
                    }
                }
            }
            if picked.is_empty() {
                (min_c..=max_c).collect()
            } else {
                picked
            }
        } else {
            (min_c..=max_c).collect()
        };

        let cols_repr = if active_cols.len() == 1 {
            col_letter(active_cols[0])
        } else if active_cols.first().copied().unwrap_or(0) + (active_cols.len() as u32 - 1) == active_cols.last().copied().unwrap_or(0) {
            format!("{}-{}", col_letter(active_cols[0]), col_letter(*active_cols.last().unwrap()))
        } else {
            active_cols.iter().map(|&c| col_letter(c)).collect::<Vec<_>>().join(",")
        };

        // Merged regions
        let merged_str = match &mut wb {
            Sheets::Xlsx(x) => {
                let regions = x.merge_cells_by_sheet_name(sheet_name).unwrap_or_default();
                if regions.is_empty() {
                    "-".to_string()
                } else {
                    regions
                        .iter()
                        .map(|d| format!("{}{}:{}{}", col_letter(d.start.1), d.start.0 + 1, col_letter(d.end.1), d.end.0 + 1))
                        .collect::<Vec<_>>()
                        .join(";")
                }
            }
            Sheets::Xls(x) => {
                let regions = x.merge_cells_by_sheet_name(sheet_name).unwrap_or_default();
                if regions.is_empty() {
                    "-".to_string()
                } else {
                    regions
                        .iter()
                        .map(|d| format!("{}{}:{}{}", col_letter(d.start.1), d.start.0 + 1, col_letter(d.end.1), d.end.0 + 1))
                        .collect::<Vec<_>>()
                        .join(";")
                }
            }
            _ => "-".to_string(),
        };

        // Header cells
        let header_row_idx = header_row_1based.map(|hr| hr - 1);
        let mut header_names: Vec<String> = Vec::with_capacity(active_cols.len());
        for &c in &active_cols {
            if let Some(hr) = header_row_idx {
                let rel_r = hr - base_row;
                let rel_c = c - base_col;
                let cell_txt = range.get((rel_r as usize, rel_c as usize))
                    .map(|d| d.to_string().trim().to_string())
                    .unwrap_or_default();
                if cell_txt.is_empty() {
                    header_names.push(col_letter(c));
                } else {
                    header_names.push(cell_txt);
                }
            } else {
                header_names.push(col_letter(c));
            }
        }

        // Collect body rows & calculate canonical cell dump
        let mut body_lines = Vec::new();
        let mut preamble_lines = Vec::new();
        let mut canon_dump = String::new();
        let mut emitted_cell_count = 0usize;

        let start_body_r = header_row_idx.map(|hr| hr + 1).unwrap_or(min_r);

        // Emit preamble (rows before header_row that have non-empty cells)
        if let Some(hr) = header_row_idx {
            for r in min_r..hr {
                for &c in &active_cols {
                    let rel_r = r - base_row;
                    let rel_c = c - base_col;
                    if let Some(cell) = range.get((rel_r as usize, rel_c as usize)) {
                        let c_text = canon_text(cell);
                        if !c_text.is_empty() {
                            emitted_cell_count += 1;
                            let k = kind_of(cell);
                            canon_dump.push_str(&format!(
                                "{}\t{}\t{}\t\"{}\"\n",
                                r + 1, c + 1, k, json_escape(&c_text)
                            ));
                            preamble_lines.push(format!("> {}{}: {}\n", col_letter(c), r + 1, escape_cell(&c_text, matches!(cell, Data::String(_)))));
                        }
                    }
                }
            }
        }

        // Body rows
        let mut omitted_empty_rows = 0usize;
        for r in start_body_r..=max_r {
            let mut row_has_cell = false;
            let mut row_cells: Vec<String> = Vec::with_capacity(active_cols.len());

            for &c in &active_cols {
                let rel_r = r - base_row;
                let rel_c = c - base_col;
                let cell = range.get((rel_r as usize, rel_c as usize)).unwrap_or(&Data::Empty);
                let c_text = canon_text(cell);
                if !c_text.is_empty() {
                    row_has_cell = true;
                    emitted_cell_count += 1;
                    let k = kind_of(cell);
                    canon_dump.push_str(&format!(
                        "{}\t{}\t{}\t\"{}\"\n",
                        r + 1, c + 1, k, json_escape(&c_text)
                    ));
                }
                let esc = escape_cell(&c_text, matches!(cell, Data::String(_)));
                row_cells.push(esc);
            }

            if row_has_cell {
                body_lines.push((r + 1, row_cells));
            } else {
                omitted_empty_rows += 1;
            }
        }

        let sheet_sha256 = format!("{:x}", Sha256::digest(canon_dump.as_bytes()));
        let body_rows_count = body_lines.len();

        let header_row_str = header_row_1based.map(|hr| hr.to_string()).unwrap_or_else(|| "none".into());
        out.push_str(&format!(
            "@sheet {}/{} name=\"{}\" state=visible used={} header_row={} cols={} body_rows={} cells={} merged={} formulas=0 types=typed",
            sheet_idx + 1, total_sheets_in_wb, sheet_name, used_str, header_row_str, cols_repr, body_rows_count, emitted_cell_count, merged_str
        ));
        if omitted_empty_rows > 0 {
            out.push_str(&format!(" omitted_empty_rows={}", omitted_empty_rows));
        }
        out.push_str(&format!(" sha256={}\n", sheet_sha256));

        // Preamble lines
        for pl in preamble_lines {
            out.push_str(&pl);
        }

        // Table header function
        let make_table_header = || {
            let mut th = String::new();
            th.push_str("| row |");
            for name in &header_names {
                th.push_str(&format!(" {} |", escape_cell(name, true)));
            }
            th.push('\n');
            th.push_str("| ---: |");
            for _ in &header_names {
                th.push_str(" :--- |");
            }
            th.push('\n');
            th
        };

        if body_rows_count > 0 {
            out.push_str(&make_table_header());

            for (idx, (row_num, cells)) in body_lines.iter().enumerate() {
                if chunk_rows > 0 && idx > 0 && idx % chunk_rows == 0 {
                    let end_chunk = (idx + chunk_rows).min(body_rows_count);
                    out.push_str(&format!("\n@cont body_rows={}-{} of {}\n", idx + 1, end_chunk, body_rows_count));
                    out.push_str(&make_table_header());
                }

                out.push_str(&format!("| {} |", row_num));
                for cell in cells {
                    if cell.is_empty() {
                        out.push_str("  |");
                    } else {
                        out.push_str(&format!(" {} |", cell));
                    }
                }
                out.push('\n');
            }
        }

        out.push_str(&format!(
            "@end sheet={} body_rows={} cells={} sha256={}\n\n",
            sheet_idx + 1, body_rows_count, emitted_cell_count, sheet_sha256
        ));

        emitted_sheets += 1;
    }

    if emitted_sheets == 0 {
        return Err("spreadsheet has no matching cells".into());
    }

    Ok(Extracted {
        text: out.trim_end().to_string(),
        note: format!("TM-GRID/1 spreadsheet ({} sheet{})", emitted_sheets, if emitted_sheets == 1 { "" } else { "s" }),
        lang: "tm-grid".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_col_letter_roundtrip() {
        assert_eq!(col_letter(0), "A");
        assert_eq!(col_letter(25), "Z");
        assert_eq!(col_letter(26), "AA");
        assert_eq!(col_letter(27), "AB");
        assert_eq!(col_letter(701), "ZZ");
        assert_eq!(col_letter(702), "AAA");

        assert_eq!(col_from_letter("A"), Some(0));
        assert_eq!(col_from_letter("Z"), Some(25));
        assert_eq!(col_from_letter("AA"), Some(26));
        assert_eq!(col_from_letter("AB"), Some(27));
        assert_eq!(col_from_letter("ZZ"), Some(701));
        assert_eq!(col_from_letter("AAA"), Some(702));
    }

    #[test]
    fn test_escape_decode_roundtrip() {
        let test_cases = vec![
            ("plain text", true),
            ("line 1\nline 2", true),
            ("has | pipe and \\ backslash", true),
            ("<br> literal", true),
            ("1403811331", true), // string that looks like number
            ("TRUE", true),       // string that looks like bool
            ("123.45", false),    // number
            ("TRUE", false),      // bool
        ];

        for (val, is_str) in test_cases {
            let esc = escape_cell(val, is_str);
            let (dec, _kind) = decode_cell(&esc);
            assert_eq!(dec, val, "failed on {:?}", val);
        }
    }
}
