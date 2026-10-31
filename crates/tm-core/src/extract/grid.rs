//! TM-GRID/1 (ADR 0021): spreadsheet and CSV/TSV text that keeps every cell
//! at its place, readable as a Markdown table and decodable back to cells.
//!
//! One workbook block: a `TM-GRID/1` line, an `@file` line, then per sheet an
//! `@sheet` line, preamble lines (`> A1: text`) for cells above the header
//! row, a GFM table whose first column is the original row number, and an
//! `@end` line that repeats the counts and the SHA-256 of the sheet's
//! canonical cell dump. Nothing is dropped silently: sheets left out by the
//! selection are listed as `skipped`, empty rows are counted, and a selector
//! that matches nothing is an error (or a warning under `on_no_match =
//! "warn"`). The written spec and the independent verifier live in
//! `scripts/e2e/tm_verify.py`.

use crate::extract::Extracted;
use crate::selection::{GridSelect, SheetSel};
use calamine::{open_workbook_auto, Data, Reader, SheetType, SheetVisible, Sheets};
use regex::Regex;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::OnceLock;

/// A sheet larger than this (rows × columns of its range) is refused.
pub const MAX_SHEET_CELLS: u64 = 10_000_000;

/// SHA-256 of the empty canonical dump.
const EMPTY_SHA: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// What the caller knows about the file being encoded.
pub struct GridSource<'a> {
    /// Relative display path (the block's `## ` header).
    pub rel_path: &'a str,
    pub bytes: u64,
    pub sha256: String,
}

/// Options shared by workbooks and delimited text.
pub struct GridOptions<'a> {
    pub select: &'a GridSelect,
    /// Body rows per table chunk; 0 = one table per sheet.
    pub chunk_rows: usize,
    /// `on_no_match = "error"`: unknown sheets/columns fail the file.
    pub strict: bool,
}

/// 0-based column index → Excel letters (0 → "A", 26 → "AA").
pub fn col_letter(mut col_idx: u32) -> String {
    let mut s = Vec::new();
    loop {
        s.push(b'A' + (col_idx % 26) as u8);
        if col_idx < 26 {
            break;
        }
        col_idx = col_idx / 26 - 1;
    }
    s.reverse();
    String::from_utf8(s).unwrap_or_default()
}

/// Excel letters → 0-based column index ("A" → 0, "AA" → 26). Upper case only.
pub fn col_from_letter(s: &str) -> Option<u32> {
    if s.is_empty() || s.len() > 3 || !s.bytes().all(|b| b.is_ascii_uppercase()) {
        return None;
    }
    let mut idx = 0u32;
    for ch in s.bytes() {
        idx = idx * 26 + u32::from(ch - b'A' + 1);
    }
    Some(idx - 1)
}

/// A cell's canonical text and type (`s` string, `n` number, `b` boolean,
/// `e` error, `d` date/time/duration).
#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    pub text: String,
    pub kind: char,
}

/// Canonical text of a calamine value; `None` for an empty cell.
pub fn canon(data: &Data) -> Option<Cell> {
    let cell = |text: String, kind: char| Some(Cell { text, kind });
    match data {
        Data::Empty => None,
        Data::String(s) if s.is_empty() => None,
        Data::String(s) => cell(s.clone(), 's'),
        Data::Int(i) => cell(i.to_string(), 'n'),
        Data::Float(f) if !f.is_finite() => cell("#NUM!".into(), 'e'),
        // Rust's `{}` for f64 is the shortest round-trip text and never uses
        // an exponent: 54.0 → "54", 1e-7 → "0.0000001".
        // -0.0 is 0 to every reader (Excel shows 0, openpyxl reads "-0" as 0).
        Data::Float(f) if *f == 0.0 => cell("0".into(), 'n'),
        Data::Float(f) => cell(f.to_string(), 'n'),
        Data::Bool(b) => cell(if *b { "TRUE" } else { "FALSE" }.into(), 'b'),
        Data::DateTime(dt) => cell(canon_datetime(dt), 'd'),
        Data::DateTimeIso(s) | Data::DurationIso(s) if s.is_empty() => None,
        Data::DateTimeIso(s) | Data::DurationIso(s) => cell(s.clone(), 'd'),
        Data::Error(e) => cell(e.to_string(), 'e'),
    }
}

fn canon_datetime(dt: &calamine::ExcelDateTime) -> String {
    let v = dt.as_f64();
    if dt.is_duration() {
        let neg = v < 0.0;
        let ms = (v.abs() * 86_400_000.0).round() as u64;
        let (h, rem) = (ms / 3_600_000, ms % 3_600_000);
        let (m, rem) = (rem / 60_000, rem % 60_000);
        let (s, milli) = (rem / 1000, rem % 1000);
        let frac = if milli > 0 {
            format!(".{milli:03}")
        } else {
            String::new()
        };
        return format!("{}{h}:{m:02}:{s:02}{frac}", if neg { "-" } else { "" });
    }
    let (y, mo, d, h, mi, s, milli) = dt.to_ymd_hms_milli();
    let frac = if milli > 0 {
        format!(".{milli:03}")
    } else {
        String::new()
    };
    if (0.0..1.0).contains(&v) {
        format!("{h:02}:{mi:02}:{s:02}{frac}")
    } else if h == 0 && mi == 0 && s == 0 && milli == 0 {
        format!("{y:04}-{mo:02}-{d:02}")
    } else {
        format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}{frac}")
    }
}

fn typed_patterns() -> &'static [Regex; 4] {
    static RE: OnceLock<[Regex; 4]> = OnceLock::new();
    RE.get_or_init(|| {
        let r = |p: &str| Regex::new(p).expect("static regex");
        [
            r(r"^-?(0|[1-9][0-9]*)(\.[0-9]+)?$"),
            r(r"^(TRUE|FALSE)$"),
            r(r"^(#NULL!|#DIV/0!|#VALUE!|#REF!|#NAME\?|#NUM!|#N/A|#GETTING_DATA|#DATA!)$"),
            r(r"^(\d{4}-\d{2}-\d{2}( \d{2}:\d{2}:\d{2}(\.\d{3})?)?|-?\d+:\d{2}:\d{2}(\.\d{3})?)$"),
        ]
    })
}

/// Would a reader take this text for a number, boolean, error or date?
pub fn looks_typed(text: &str) -> bool {
    typed_patterns().iter().any(|re| re.is_match(text))
}

/// Encode a cell's canonical text. `marker` = a string in a typed sheet:
/// it gets a leading `'` when it starts with `'` or looks typed.
pub fn escape_cell(text: &str, marker: bool) -> String {
    let mut s = String::with_capacity(text.len() + 8);
    if marker && (text.starts_with('\'') || looks_typed(text)) {
        s.push('\'');
    }
    let mut rest = text;
    while let Some(ch) = rest.chars().next() {
        if rest.starts_with("<br>") {
            s.push_str("\\<br>");
            rest = &rest[4..];
            continue;
        }
        match ch {
            '\\' => s.push_str("\\\\"),
            '|' => s.push_str("\\|"),
            '\n' => s.push_str("<br>"),
            '\r' => s.push_str("\\r"),
            '\t' => s.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                s.push_str(&format!("\\x{:02x}", c as u32))
            }
            c => s.push(c),
        }
        rest = &rest[ch.len_utf8()..];
    }
    s
}

/// Inverse of [`escape_cell`] for one table field *without* its pad spaces.
/// `typed` = the sheet is `types=typed` (a leading `'` is the marker).
/// Returns the text and whether it carried the typed-text marker.
pub fn decode_cell(field: &str, typed: bool) -> Result<(String, bool), String> {
    let (quoted, body) = match field.strip_prefix('\'') {
        Some(b) if typed => (true, b),
        _ => (false, field),
    };
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(ch) = rest.chars().next() {
        if let Some(r) = rest.strip_prefix("<br>") {
            out.push('\n');
            rest = r;
            continue;
        }
        if ch == '\\' {
            let tail = &rest[1..];
            let (push, used): (String, usize) = match tail.chars().next() {
                Some('\\') => ("\\".into(), 1),
                Some('|') => ("|".into(), 1),
                Some('<') => ("<".into(), 1),
                Some('r') => ("\r".into(), 1),
                Some('t') => ("\t".into(), 1),
                Some('x') if tail.len() >= 3 => {
                    let v = u8::from_str_radix(&tail[1..3], 16)
                        .map_err(|_| format!("bad \\x escape in {field:?}"))?;
                    ((v as char).to_string(), 3)
                }
                _ => return Err(format!("bad escape in {field:?}")),
            };
            out.push_str(&push);
            rest = &tail[used..];
            continue;
        }
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    Ok((out, quoted))
}

/// JSON string literal with the canonical-dump escapes.
fn json_lit(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// One sheet ready to emit.
struct SheetModel {
    name: String,
    state: &'static str,
    /// (1-based row, 0-based column) → cell.
    cells: BTreeMap<(u32, u32), Cell>,
    merged: String,
    formulas: usize,
    formula_nocache: usize,
    /// false for CSV/TSV: every cell is text, no `'` marker.
    typed: bool,
    /// CSV/TSV: the header is record 1 unless overridden.
    default_header: Option<u32>,
}

/// The selection that applies to one sheet (file level, overridden per sheet).
struct SheetPick<'a> {
    header_row: Option<u32>,
    columns: &'a [String],
    rows: &'a [String],
}

fn pick_for<'a>(select: &'a GridSelect, name: &str) -> SheetPick<'a> {
    let over: Option<&SheetSel> = select
        .sheet_overrides
        .iter()
        .find(|o| o.name == name || crate::slice::glob_match(&o.name, name));
    SheetPick {
        header_row: over.and_then(|o| o.header_row).or(select.header_row),
        columns: match over {
            Some(o) if !o.columns.is_empty() => &o.columns,
            _ => &select.columns,
        },
        rows: match over {
            Some(o) if !o.rows.is_empty() => &o.rows,
            _ => &select.rows,
        },
    }
}

fn name_matches(pattern: &str, name: &str) -> bool {
    pattern == name || crate::slice::glob_match(pattern, name)
}

/// "A-C,E,H-K" for a column list (runs of consecutive ascending columns).
fn cols_repr(cols: &[u32]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < cols.len() {
        let mut j = i;
        while j + 1 < cols.len() && cols[j + 1] == cols[j] + 1 {
            j += 1;
        }
        if j > i {
            parts.push(format!("{}-{}", col_letter(cols[i]), col_letter(cols[j])));
        } else {
            parts.push(col_letter(cols[i]));
        }
        i = j + 1;
    }
    parts.join(",")
}

/// Resolve `columns` entries (header names first, then letters or letter
/// ranges) to absolute column indexes, in the order asked.
fn resolve_columns(
    specs: &[String],
    header: &BTreeMap<u32, String>,
    min_c: u32,
    max_c: u32,
    problems: &mut Vec<String>,
) -> Vec<u32> {
    let mut out: Vec<u32> = Vec::new();
    let push = |c: u32, out: &mut Vec<u32>| {
        if !out.contains(&c) {
            out.push(c);
        }
    };
    for spec in specs {
        let exact: Vec<u32> = header
            .iter()
            .filter(|(_, t)| *t == spec)
            .map(|(c, _)| *c)
            .collect();
        if exact.len() == 1 {
            push(exact[0], &mut out);
            continue;
        }
        if exact.len() > 1 {
            problems.push(format!(
                "column {spec:?} appears {} times in the header; name it by letter",
                exact.len()
            ));
            continue;
        }
        let letters = spec
            .split_once('-')
            .map_or((spec.as_str(), spec.as_str()), |(a, b)| (a, b));
        if let (Some(a), Some(b)) = (col_from_letter(letters.0), col_from_letter(letters.1)) {
            if a <= b && a >= min_c && b <= max_c {
                for c in a..=b {
                    push(c, &mut out);
                }
                continue;
            }
        }
        let loose: Vec<u32> = header
            .iter()
            .filter(|(_, t)| t.trim().eq_ignore_ascii_case(spec.trim()))
            .map(|(c, _)| *c)
            .collect();
        if loose.len() == 1 {
            push(loose[0], &mut out);
            continue;
        }
        let names: Vec<&str> = header.values().map(|s| s.as_str()).collect();
        problems.push(format!(
            "column {spec:?} not found (header: {})",
            names.join(" | ")
        ));
    }
    out
}

/// "5-20", "31" → inclusive row ranges.
fn parse_rows(specs: &[String]) -> Result<Vec<(u32, u32)>, String> {
    let mut out = Vec::new();
    for spec in specs {
        let s = spec.trim();
        let (a, b) = s.split_once('-').unwrap_or((s, s));
        let a: u32 = a
            .trim()
            .parse()
            .map_err(|_| format!("invalid rows entry {spec:?}"))?;
        let b: u32 = b
            .trim()
            .parse()
            .map_err(|_| format!("invalid rows entry {spec:?}"))?;
        if a == 0 || b < a {
            return Err(format!("invalid rows entry {spec:?}"));
        }
        out.push((a, b));
    }
    Ok(out)
}

/// Emit one sheet. Problems that a strict plan must refuse are returned in
/// `problems` (the caller decides).
fn emit_sheet(
    out: &mut String,
    idx: usize,
    total: usize,
    m: &SheetModel,
    pick: &SheetPick<'_>,
    chunk_rows: usize,
    problems: &mut Vec<String>,
) -> Result<(), String> {
    let types = if m.typed { "typed" } else { "text" };
    let mut formula_part = format!("formulas={}", m.formulas);
    if m.formula_nocache > 0 {
        formula_part.push_str(&format!(" formula_nocache={}", m.formula_nocache));
    }
    let head = format!(
        "@sheet {idx}/{total} name={} state={}",
        json_lit(&m.name),
        m.state
    );
    if m.cells.is_empty() {
        out.push_str(&format!(
            "{head} used=- header_row=none cols=- body_rows=0 cells=0 merged={} {formula_part} types={types} sha256={EMPTY_SHA}\n"
        , m.merged));
        out.push_str(&format!(
            "@end sheet={idx} body_rows=0 cells=0 sha256={EMPTY_SHA}\n\n"
        ));
        return Ok(());
    }
    let min_r = m.cells.keys().map(|k| k.0).min().unwrap_or(1);
    let max_r = m.cells.keys().map(|k| k.0).max().unwrap_or(1);
    let min_c = m.cells.keys().map(|k| k.1).min().unwrap_or(0);
    let max_c = m.cells.keys().map(|k| k.1).max().unwrap_or(0);
    let used = format!(
        "{}{}:{}{}",
        col_letter(min_c),
        min_r,
        col_letter(max_c),
        max_r
    );

    // Header row: explicit, then the sheet default (CSV record 1), then the
    // heuristic (typed sheets only). The result is always printed.
    let in_range = |h: u32| h >= min_r && h <= max_r;
    let header_row = match (pick.header_row, m.default_header) {
        (Some(h), _) if !in_range(h) => {
            problems.push(format!(
                "sheet {:?}: header_row {h} is outside the used rows {min_r}-{max_r}",
                m.name
            ));
            None
        }
        (Some(h), _) => Some(h),
        (None, Some(h)) => Some(h).filter(|&h| in_range(h)),
        (None, None) if m.typed => {
            let width = max_c - min_c + 1;
            let need = 2.max(width.div_ceil(2)) as usize;
            let mut by_row: BTreeMap<u32, (usize, bool)> = BTreeMap::new();
            for ((r, _), c) in &m.cells {
                let e = by_row.entry(*r).or_insert((0, true));
                e.0 += 1;
                e.1 &= c.kind == 's';
            }
            by_row
                .iter()
                .find(|(_, (n, all_s))| *n >= need && *all_s)
                .map(|(r, _)| *r)
        }
        (None, None) => None,
    };
    let header_cells: BTreeMap<u32, String> = match header_row {
        Some(h) => m
            .cells
            .range((h, 0)..=(h, u32::MAX))
            .map(|((_, c), cell)| (*c, cell.text.clone()))
            .collect(),
        None => BTreeMap::new(),
    };
    let mut proj: Vec<&str> = Vec::new();
    let cols: Vec<u32> = if pick.columns.is_empty() {
        (min_c..=max_c).collect()
    } else {
        proj.push("columns");
        let before = problems.len();
        let c = resolve_columns(pick.columns, &header_cells, min_c, max_c, problems);
        for p in problems.iter_mut().skip(before) {
            *p = format!("sheet {:?}: {p}", m.name);
        }
        if c.is_empty() {
            (min_c..=max_c).collect()
        } else {
            c
        }
    };
    let row_ranges = parse_rows(pick.rows)?;
    if !row_ranges.is_empty() {
        proj.push("rows");
    }
    let row_kept =
        |r: u32| row_ranges.is_empty() || row_ranges.iter().any(|&(a, b)| r >= a && r <= b);
    let col_set: BTreeSet<u32> = cols.iter().copied().collect();

    let enc = |c: &Cell| escape_cell(&c.text, m.typed && c.kind == 's');
    let mut dump: Vec<(u32, u32, char, &str)> = Vec::new();
    let kind_of = |c: &Cell| if m.typed { c.kind } else { 's' };

    let mut preamble = String::new();
    if let Some(h) = header_row {
        for ((r, c), cell) in m.cells.range((min_r, 0)..(h, 0)) {
            if col_set.contains(c) {
                preamble.push_str(&format!("> {}{}: {}\n", col_letter(*c), r, enc(cell)));
                dump.push((*r, *c, kind_of(cell), &cell.text));
            }
        }
    }
    let mut header_line = String::from("| row |");
    for &c in &cols {
        match header_row {
            Some(h) => match m.cells.get(&(h, c)) {
                Some(cell) => {
                    header_line.push_str(&format!(" {} |", enc(cell)));
                    dump.push((h, c, kind_of(cell), &cell.text));
                }
                None => header_line.push_str("  |"),
            },
            None => header_line.push_str(&format!(" {} |", col_letter(c))),
        }
    }
    header_line.push('\n');
    header_line.push_str("| ---: |");
    for _ in &cols {
        header_line.push_str(" :--- |");
    }
    header_line.push('\n');

    let first_body = header_row.map(|h| h + 1).unwrap_or(min_r);
    let mut body: Vec<String> = Vec::new();
    let mut empty_rows = 0usize;
    for r in first_body..=max_r {
        if !row_kept(r) {
            continue;
        }
        let mut line = format!("| {r} |");
        let mut any = false;
        for &c in &cols {
            match m.cells.get(&(r, c)) {
                Some(cell) => {
                    any = true;
                    line.push_str(&format!(" {} |", enc(cell)));
                    dump.push((r, c, kind_of(cell), &cell.text));
                }
                None => line.push_str("  |"),
            }
        }
        if any {
            line.push('\n');
            body.push(line);
        } else {
            empty_rows += 1;
        }
    }
    dump.sort_by_key(|d| (d.0, d.1));
    let mut canon = String::new();
    for (r, c, k, t) in &dump {
        canon.push_str(&format!("{r}\t{}\t{k}\t{}\n", c + 1, json_lit(t)));
    }
    let sha = format!("{:x}", Sha256::digest(canon.as_bytes()));
    let n_cells = dump.len();
    let n_body = body.len();

    out.push_str(&format!(
        "{head} used={used} header_row={} cols={} body_rows={n_body} cells={n_cells} merged={} {formula_part} types={types}",
        header_row.map_or("none".to_string(), |h| h.to_string()),
        cols_repr(&cols),
        m.merged
    ));
    if empty_rows > 0 {
        out.push_str(&format!(" omitted_empty_rows={empty_rows}"));
    }
    if !proj.is_empty() {
        out.push_str(&format!(" projection={}", proj.join(",")));
    }
    out.push_str(&format!(" sha256={sha}\n"));
    out.push_str(&preamble);
    out.push_str(&header_line);
    for (i, line) in body.iter().enumerate() {
        if chunk_rows > 0 && i > 0 && i % chunk_rows == 0 {
            out.push_str(&format!(
                "\n@cont body_rows={}-{} of {n_body}\n",
                i + 1,
                (i + chunk_rows).min(n_body)
            ));
            out.push_str(&header_line);
        }
        out.push_str(line);
    }
    out.push_str(&format!(
        "@end sheet={idx} body_rows={n_body} cells={n_cells} sha256={sha}\n\n"
    ));
    Ok(())
}

fn file_line(name: &str, src: &GridSource<'_>, sheets: usize) -> String {
    format!(
        "TM-GRID/1\n@file name={} path={} sheets={sheets} bytes={} source_sha256={}\n\n",
        json_lit(name),
        json_lit(src.rel_path),
        src.bytes,
        src.sha256
    )
}

fn finish(
    problems: Vec<String>,
    strict: bool,
    note: String,
    warnings_out: &mut String,
) -> Result<String, String> {
    if problems.is_empty() {
        return Ok(note);
    }
    if strict {
        return Err(problems.join("; "));
    }
    warnings_out.push_str(&format!("; selection warnings: {}", problems.join("; ")));
    Ok(note)
}

/// Encode a workbook (xlsx/xlsm/xlsb/xls/ods).
pub fn encode_workbook(
    path: &Path,
    src: &GridSource<'_>,
    opts: &GridOptions<'_>,
) -> Result<Extracted, String> {
    let mut wb = open_workbook_auto(path).map_err(|e| format!("spreadsheet open failed: {e}"))?;
    let meta: Vec<(String, SheetType, SheetVisible)> = wb
        .sheets_metadata()
        .iter()
        .map(|s| (s.name.clone(), s.typ, s.visible))
        .collect();
    let total = meta.len();
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workbook");
    let mut out = file_line(name, src, total);
    let mut problems: Vec<String> = Vec::new();
    let sel = opts.select;

    for pattern in sel.sheets.iter().chain(&sel.exclude_sheets) {
        if !meta.iter().any(|(n, _, _)| name_matches(pattern, n)) {
            let names: Vec<&str> = meta.iter().map(|(n, _, _)| n.as_str()).collect();
            problems.push(format!(
                "sheet {pattern:?} not found (sheets: {})",
                names.join(" | ")
            ));
        }
    }
    for o in &sel.sheet_overrides {
        if !meta.iter().any(|(n, _, _)| name_matches(&o.name, n)) {
            problems.push(format!("[[file.sheet]] {:?} matches no sheet", o.name));
        }
    }

    let mut emitted = 0usize;
    for (i, (sheet, typ, vis)) in meta.iter().enumerate() {
        let idx = i + 1;
        let state = match vis {
            SheetVisible::Visible => "visible",
            SheetVisible::Hidden => "hidden",
            SheetVisible::VeryHidden => "veryhidden",
        };
        let head = format!(
            "@sheet {idx}/{total} name={} state={state}",
            json_lit(sheet)
        );
        let selected = (sel.sheets.is_empty() || sel.sheets.iter().any(|p| name_matches(p, sheet)))
            && !sel.exclude_sheets.iter().any(|p| name_matches(p, sheet));
        let skip = |out: &mut String, why: &str| {
            out.push_str(&format!(
                "{head} skipped={}\n@end sheet={idx} skipped\n\n",
                json_lit(why)
            ));
        };
        if !selected {
            skip(&mut out, "not selected");
            continue;
        }
        let kind = match typ {
            SheetType::WorkSheet => None,
            SheetType::ChartSheet => Some("chartsheet"),
            SheetType::DialogSheet => Some("dialogsheet"),
            SheetType::MacroSheet => Some("macrosheet"),
            _ => Some("other sheet type"),
        };
        if let Some(why) = kind {
            skip(&mut out, why);
            continue;
        }
        let range = match wb.worksheet_range(sheet) {
            Ok(r) => r,
            Err(e) => {
                skip(&mut out, &format!("unreadable: {e}"));
                problems.push(format!("sheet {sheet:?} unreadable: {e}"));
                continue;
            }
        };
        let (h, w) = range.get_size();
        if (h as u64) * (w as u64) > MAX_SHEET_CELLS {
            return Err(format!("sheet {sheet:?} range too large ({h} x {w} cells)"));
        }
        let (r0, c0) = range.start().unwrap_or((0, 0));
        let mut cells = BTreeMap::new();
        for (r, c, v) in range.used_cells() {
            if let Some(cell) = canon(v) {
                cells.insert((r0 + r as u32 + 1, c0 + c as u32), cell);
            }
        }
        let (mut formulas, mut nocache) = (0usize, 0usize);
        if let Ok(f) = wb.worksheet_formula(sheet) {
            let (fr0, fc0) = f.start().unwrap_or((0, 0));
            for (r, c, text) in f.used_cells() {
                if !text.is_empty() {
                    formulas += 1;
                    if !cells.contains_key(&(fr0 + r as u32 + 1, fc0 + c as u32)) {
                        nocache += 1;
                    }
                }
            }
        }
        let merged = merged_ranges(&mut wb, sheet);
        let model = SheetModel {
            name: sheet.clone(),
            state,
            cells,
            merged,
            formulas,
            formula_nocache: nocache,
            typed: true,
            default_header: None,
        };
        emit_sheet(
            &mut out,
            idx,
            total,
            &model,
            &pick_for(sel, sheet),
            opts.chunk_rows,
            &mut problems,
        )?;
        emitted += 1;
    }
    if emitted == 0 && problems.is_empty() {
        problems.push("no sheet selected".into());
    }
    let mut note = format!(
        "TM-GRID/1 spreadsheet ({emitted} of {total} sheet{})",
        if total == 1 { "" } else { "s" }
    );
    let mut extra = String::new();
    note = finish(problems, opts.strict, note, &mut extra)?;
    note.push_str(&extra);
    Ok(Extracted {
        text: out.trim_end().to_string(),
        note,
        lang: "tm-grid".to_string(),
    })
}

fn merged_ranges<RS: std::io::Read + std::io::Seek>(wb: &mut Sheets<RS>, sheet: &str) -> String {
    let dims = match wb {
        Sheets::Xlsx(x) => x.merge_cells_by_sheet_name(sheet).unwrap_or_default(),
        Sheets::Xls(x) => x.merge_cells_by_sheet_name(sheet).unwrap_or_default(),
        _ => Vec::new(),
    };
    if dims.is_empty() {
        return "-".into();
    }
    dims.iter()
        .map(|d| {
            format!(
                "{}{}:{}{}",
                col_letter(d.start.1),
                d.start.0 + 1,
                col_letter(d.end.1),
                d.end.0 + 1
            )
        })
        .collect::<Vec<_>>()
        .join(";")
}

/// RFC 4180 records (quoted fields may hold the delimiter, quotes and line
/// breaks; `\r\n`, `\n` and a lone `\r` end a record; a blank line is an empty
/// record). Mirrors Python's `csv.reader` with the default dialect.
pub fn parse_delimited(text: &str, delim: char) -> Vec<Vec<String>> {
    let mut records: Vec<Vec<String>> = Vec::new();
    let mut rec: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut field_started = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' if !field_started => {
                in_quotes = true;
                field_started = true;
            }
            c if c == delim => {
                rec.push(std::mem::take(&mut field));
                field_started = false;
            }
            '\r' | '\n' => {
                if c == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                if field_started || !rec.is_empty() {
                    rec.push(std::mem::take(&mut field));
                }
                records.push(std::mem::take(&mut rec));
                field_started = false;
            }
            c => {
                field.push(c);
                field_started = true;
            }
        }
    }
    if field_started || !rec.is_empty() || in_quotes {
        rec.push(field);
        records.push(rec);
    }
    records
}

/// Encode CSV/TSV text as one TM-GRID/1 sheet (`types=text`, header = record 1
/// unless the selection says otherwise). Row numbers are record numbers.
pub fn encode_delimited(
    text: &str,
    delim: char,
    src: &GridSource<'_>,
    opts: &GridOptions<'_>,
) -> Result<(String, String), String> {
    let name = src.rel_path.rsplit('/').next().unwrap_or(src.rel_path);
    let records = parse_delimited(text, delim);
    let mut cells = BTreeMap::new();
    for (r, rec) in records.iter().enumerate() {
        for (c, f) in rec.iter().enumerate() {
            if !f.is_empty() {
                cells.insert(
                    ((r + 1) as u32, c as u32),
                    Cell {
                        text: f.clone(),
                        kind: 's',
                    },
                );
            }
        }
    }
    let model = SheetModel {
        name: name.to_string(),
        state: "visible",
        cells,
        merged: "-".into(),
        formulas: 0,
        formula_nocache: 0,
        typed: false,
        default_header: Some(1),
    };
    let sel = opts.select;
    let mut problems = Vec::new();
    if !sel.sheets.is_empty() || !sel.exclude_sheets.is_empty() || !sel.sheet_overrides.is_empty() {
        problems
            .push("`sheets`/`exclude_sheets`/`[[file.sheet]]` do not apply to CSV/TSV".to_string());
    }
    let mut out = file_line(name, src, 1);
    emit_sheet(
        &mut out,
        1,
        1,
        &model,
        &pick_for(sel, name),
        opts.chunk_rows,
        &mut problems,
    )?;
    let mut extra = String::new();
    let note = finish(
        problems,
        opts.strict,
        format!(
            "TM-GRID/1 from {} ({} records)",
            if delim == '\t' { "TSV" } else { "CSV" },
            records.len()
        ),
        &mut extra,
    )?;
    Ok((out.trim_end().to_string(), format!("{note}{extra}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(rel: &str) -> GridSource<'_> {
        GridSource {
            rel_path: rel,
            bytes: 0,
            sha256: "0".repeat(64),
        }
    }

    #[test]
    fn col_letter_roundtrip() {
        for (i, s) in [
            (0, "A"),
            (25, "Z"),
            (26, "AA"),
            (27, "AB"),
            (701, "ZZ"),
            (702, "AAA"),
        ] {
            assert_eq!(col_letter(i), s);
            assert_eq!(col_from_letter(s), Some(i));
        }
        assert_eq!(
            col_from_letter("Rank"),
            None,
            "mixed case is a name, not a column"
        );
    }

    #[test]
    fn escape_decode_roundtrip() {
        let cases = [
            "plain",
            "line 1\nline 2",
            "a | b \\ c",
            "<br> literal",
            "\\<br>",
            "tab\there",
            "cr\rhere",
            "ctl\u{1}x",
            " lead",
            "trail ",
            "'quoted",
            "''x",
            "1403811331",
            "TRUE",
            "#N/A",
            "2024-01-31",
            "émoji 😀",
        ];
        for text in cases {
            for typed in [true, false] {
                let e = escape_cell(text, typed);
                assert!(!e.contains('\n') && !e.contains('\r'), "{e:?}");
                let (back, quoted) = decode_cell(&e, typed).unwrap();
                assert_eq!(back, text, "typed={typed} encoded={e:?}");
                assert_eq!(
                    quoted,
                    typed && (text.starts_with('\'') || looks_typed(text))
                );
            }
        }
        assert!(decode_cell("bad \\q escape", true).is_err());
        assert_eq!(escape_cell("54", true), "'54");
        assert_eq!(escape_cell("54", false), "54");
        assert_eq!(
            escape_cell("0123", true),
            "0123",
            "not a NUM shape, no marker needed"
        );
    }

    #[test]
    fn csv_records_keep_quoted_newlines() {
        let text = "id,name,note\r\n1,\"Mercy, Lorain\",\"line one\r\nline two\"\n2,x,\"say \"\"hi\"\"\"\n\n3,,\n";
        let recs = parse_delimited(text, ',');
        assert_eq!(recs.len(), 5);
        assert_eq!(recs[1], vec!["1", "Mercy, Lorain", "line one\r\nline two"]);
        assert_eq!(recs[2][2], "say \"hi\"");
        assert!(recs[3].is_empty(), "a blank line is an empty record");
        assert_eq!(recs[4], vec!["3", "", ""]);
    }

    #[test]
    fn csv_projection_is_a_grid_with_record_numbers() {
        let text = "Rank,Program,City,Notes\n1,\"General | FM\",Springfield,\"two\nlines\"\n2,University IM,Shelbyville,\n";
        let sel = GridSelect {
            columns: vec!["Rank".into(), "Program".into(), "Notes".into()],
            ..Default::default()
        };
        let opts = GridOptions {
            select: &sel,
            chunk_rows: 150,
            strict: true,
        };
        let (out, _) = encode_delimited(text, ',', &src("a/b.csv"), &opts).unwrap();
        assert!(
            out.contains("header_row=1 cols=A-B,D body_rows=2 cells=8"),
            "{out}"
        );
        assert!(out.contains("| row | Rank | Program | Notes |\n"), "{out}");
        assert!(
            out.contains("| 2 | 1 | General \\| FM | two<br>lines |\n"),
            "{out}"
        );
        assert!(out.contains("| 3 | 2 | University IM |  |\n"), "{out}");
        assert!(out.contains("projection=columns"));
    }

    #[test]
    fn unknown_column_is_an_error_when_strict() {
        let text = "Rank,Program\n1,A\n";
        let sel = GridSelect {
            columns: vec!["Rank".into(), "Nope".into()],
            ..Default::default()
        };
        let strict = GridOptions {
            select: &sel,
            chunk_rows: 0,
            strict: true,
        };
        let err = encode_delimited(text, ',', &src("x.csv"), &strict).unwrap_err();
        assert!(err.contains("\"Nope\" not found"), "{err}");
        let lax = GridOptions {
            select: &sel,
            chunk_rows: 0,
            strict: false,
        };
        let (_, note) = encode_delimited(text, ',', &src("x.csv"), &lax).unwrap();
        assert!(note.contains("selection warnings"), "{note}");
    }

    #[test]
    fn chunks_restart_the_table_after_a_blank_line() {
        let mut text = String::from("n\n");
        for i in 0..5 {
            text.push_str(&format!("v{i}\n"));
        }
        let sel = GridSelect::default();
        let opts = GridOptions {
            select: &sel,
            chunk_rows: 2,
            strict: true,
        };
        let (out, _) = encode_delimited(&text, ',', &src("c.csv"), &opts).unwrap();
        assert!(
            out.contains(
                "| 3 | v1 |\n\n@cont body_rows=3-4 of 5\n| row | n |\n| ---: | :--- |\n| 4 | v2 |"
            ),
            "{out}"
        );
    }
}
