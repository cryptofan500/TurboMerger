//! Document extraction (Phase 3): the text of PDFs, Office files and e-books
//! reaches the merge instead of being dropped as "not captured".

use std::io::Write;
use std::path::{Path, PathBuf};

use tm_core::extract::{extract, DocKind, ExtractOptions};

/// Write a minimal zip holding the given `(name, bytes)` entries, stored
/// (no compression) so the test needs no deflate feature.
fn make_zip(path: &Path, entries: &[(&str, &[u8])]) {
    let file = std::fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let opts =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, bytes) in entries {
        zip.start_file(*name, opts).unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap();
}

#[test]
fn docx_paragraphs_are_extracted_in_order() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("letter.docx");
    let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>
  <w:p><w:r><w:t>Dear reviewer,</w:t></w:r></w:p>
  <w:p><w:r><w:t>The </w:t></w:r><w:r><w:t>budget</w:t></w:r><w:r><w:t> is attached.</w:t></w:r></w:p>
</w:body></w:document>"#;
    make_zip(&p, &[("word/document.xml", xml)]);

    let out = extract(&p, DocKind::Docx, &ExtractOptions::default()).expect("docx extracts");
    assert!(out.text.contains("Dear reviewer,"), "got: {:?}", out.text);
    // Runs inside one paragraph join without a break between them.
    assert!(
        out.text.contains("The budget is attached."),
        "got: {:?}",
        out.text
    );
    // The two paragraphs are on separate lines.
    let dear = out.text.find("Dear").unwrap();
    let budget = out.text.find("budget").unwrap();
    assert!(
        out.text[dear..budget].contains('\n'),
        "paragraphs should break"
    );
}

#[test]
fn pptx_slides_join_in_numeric_order() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("deck.pptx");
    let slide = |t: &str| {
        format!(
            r#"<?xml version="1.0"?><p:sld xmlns:a="a"><p:cSld><p:spTree>
            <a:p><a:r><a:t>{t}</a:t></a:r></a:p></p:spTree></p:cSld></p:sld>"#
        )
        .into_bytes()
    };
    // Deliberately out of lexical order to prove numeric sorting (2 before 10).
    let s2 = slide("Second slide");
    let s10 = slide("Tenth slide");
    make_zip(
        &p,
        &[
            ("ppt/slides/slide10.xml", &s10),
            ("ppt/slides/slide2.xml", &s2),
        ],
    );

    let out = extract(&p, DocKind::Pptx, &ExtractOptions::default()).expect("pptx extracts");
    let second = out.text.find("Second slide").expect("slide 2 text");
    let tenth = out.text.find("Tenth slide").expect("slide 10 text");
    assert!(
        second < tenth,
        "slide 2 must come before slide 10: {:?}",
        out.text
    );
}

#[test]
fn broken_archive_is_reported_not_panicked() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("truncated.docx");
    std::fs::write(&p, b"PK\x03\x04 not really a zip").unwrap();
    let r = extract(&p, DocKind::Docx, &ExtractOptions::default());
    assert!(r.is_err(), "a broken archive is an Err, never a panic");
}

/// Real arXiv PDFs (two-column papers) extract substantial reading-order text.
/// Skips cleanly when the private fixtures are not present (ADR 0012).
#[test]
fn real_arxiv_pdf_extracts_reading_order_text() {
    let dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/arxiv-mini/local/2026-08");
    let pdf = std::fs::read_dir(&dir).ok().and_then(|rd| {
        rd.filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.extension().and_then(|x| x.to_str()) == Some("pdf"))
    });
    let Some(pdf) = pdf else {
        eprintln!("skipping: {} has no PDFs (private fixture)", dir.display());
        return;
    };
    let out = extract(&pdf, DocKind::Pdf, &ExtractOptions::default())
        .unwrap_or_else(|e| panic!("extract {}: {e}", pdf.display()));
    assert!(
        out.text.len() > 500,
        "expected substantial text from {}, got {} bytes",
        pdf.display(),
        out.text.len()
    );
    assert!(
        out.note.contains("page"),
        "note should mention page count: {:?}",
        out.note
    );
}

/// One BIFF12 record: type and size as 7-bit groups (high bit = more), then
/// the body ([MS-XLSB] 2.1.4).
fn biff12(out: &mut Vec<u8>, typ: u16, body: &[u8]) {
    if typ < 0x80 {
        out.push(typ as u8);
    } else {
        out.push((typ & 0x7F) as u8 | 0x80);
        out.push((typ >> 7) as u8);
    }
    let mut n = body.len();
    loop {
        let more = n >> 7;
        out.push((n & 0x7F) as u8 | if more > 0 { 0x80 } else { 0 });
        n = more;
        if n == 0 {
            break;
        }
    }
    out.extend_from_slice(body);
}

/// XLWideString: character count, then UTF-16LE.
fn wide(s: &str) -> Vec<u8> {
    let units: Vec<u16> = s.encode_utf16().collect();
    let mut v = (units.len() as u32).to_le_bytes().to_vec();
    for u in units {
        v.extend_from_slice(&u.to_le_bytes());
    }
    v
}

/// A Cell structure (column, 24-bit style index) followed by the value.
fn xlsb_cell(col: u32, style: u32, value: &[u8]) -> Vec<u8> {
    let mut v = col.to_le_bytes().to_vec();
    v.extend_from_slice(&style.to_le_bytes());
    v.extend_from_slice(value);
    v
}

/// (record type, body) of one cell record.
type XlsbCell = (u16, Vec<u8>);

fn xlsb_sheet(rows: &[(u32, Vec<XlsbCell>)], last_row: u32, last_col: u32) -> Vec<u8> {
    let mut s = Vec::new();
    biff12(&mut s, 0x81, &[]); // BrtBeginSheet
    let dim: Vec<u8> = [0u32, last_row, 0, last_col]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    biff12(&mut s, 0x94, &dim); // BrtWsDim
    biff12(&mut s, 0x91, &[]); // BrtBeginSheetData
    for (row, cells) in rows {
        let mut hdr = row.to_le_bytes().to_vec();
        hdr.extend_from_slice(&[0; 13]); // ixfe, miyRw, flags, ccolspan = 0
        biff12(&mut s, 0x00, &hdr); // BrtRowHdr
        for (typ, body) in cells {
            biff12(&mut s, *typ, body);
        }
    }
    biff12(&mut s, 0x92, &[]); // BrtEndSheetData
    biff12(&mut s, 0x82, &[]); // BrtEndSheet
    s
}

/// A minimal real `.xlsb` (BIFF12 records in an OPC zip), written here
/// because no `.xlsb` writer exists on the build machines (HANDOFF_S8 §8.4).
/// Sheet "Programs": a header of shared strings; inline, shared and Polish
/// strings; RK integers; doubles; a boolean; a date (style 1 = numFmt 14).
/// Sheet "Notes" is hidden.
fn tiny_xlsb(path: &Path) {
    let strings = [
        "Program",
        "City",
        "Positions",
        "Score",
        "Visa",
        "Deadline",
        "Łódź",
        "St. Vincent",
        "hidden note",
    ];
    let isst = |col: u32, i: u32| (0x07u16, xlsb_cell(col, 0, &i.to_le_bytes()));
    let rk_int = |col: u32, v: i32| {
        (
            0x02u16,
            xlsb_cell(col, 0, &(((v << 2) | 2) as u32).to_le_bytes()),
        )
    };
    let real = |col: u32, style: u32, v: f64| (0x05u16, xlsb_cell(col, style, &v.to_le_bytes()));
    let sheet1 = xlsb_sheet(
        &[
            (0, (0..6).map(|c| isst(c, c)).collect()),
            (
                1,
                vec![
                    (0x06, xlsb_cell(0, 0, &wide("Mercy | Lorain"))), // BrtCellSt
                    isst(1, 6),
                    rk_int(2, 12),
                    real(3, 0, 45.5),
                    (0x04, xlsb_cell(4, 0, &[1])), // BrtCellBool
                    real(5, 1, 45961.0),           // 2025-10-31
                ],
            ),
            (2, vec![isst(0, 7), rk_int(2, -3), real(3, 0, 0.001)]),
        ],
        2,
        5,
    );
    let sheet2 = xlsb_sheet(&[(0, vec![isst(0, 8)])], 0, 0);
    let mut book = Vec::new();
    biff12(&mut book, 0x83, &[]); // BrtBeginBook
    biff12(&mut book, 0x8F, &[]); // BrtBeginBundleShs
    for (state, tab, rel, name) in [(0u32, 1u32, "rId1", "Programs"), (1, 2, "rId2", "Notes")] {
        let mut b = state.to_le_bytes().to_vec();
        b.extend_from_slice(&tab.to_le_bytes());
        b.extend(wide(rel));
        b.extend(wide(name));
        biff12(&mut book, 0x9C, &b); // BrtBundleSh
    }
    biff12(&mut book, 0x90, &[]); // BrtEndBundleShs
    biff12(&mut book, 0x84, &[]); // BrtEndBook
    let mut sst = Vec::new();
    let n = (strings.len() as u32).to_le_bytes();
    biff12(&mut sst, 0x9F, &[n, n].concat()); // BrtBeginSst
    for s in strings {
        biff12(&mut sst, 0x13, &[vec![0u8], wide(s)].concat()); // BrtSSTItem
    }
    biff12(&mut sst, 0xA0, &[]); // BrtEndSst
    let mut styles = Vec::new();
    biff12(&mut styles, 0x116, &[]); // BrtBeginStyleSheet
    biff12(&mut styles, 0x269, &2u32.to_le_bytes()); // BrtBeginCellXFs
    for fmt in [0u16, 14] {
        let mut xf = vec![0u8; 16];
        xf[2..4].copy_from_slice(&fmt.to_le_bytes());
        biff12(&mut styles, 0x2F, &xf); // BrtXF
    }
    biff12(&mut styles, 0x26A, &[]); // BrtEndCellXFs
    biff12(&mut styles, 0x117, &[]); // BrtEndStyleSheet
    let rel = |id: &str, kind: &str, target: &str| {
        format!(
            r#"<Relationship Id="{id}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/{kind}" Target="{target}"/>"#
        )
    };
    let rels = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{}{}{}{}</Relationships>"#,
        rel("rId1", "worksheet", "worksheets/sheet1.bin"),
        rel("rId2", "worksheet", "worksheets/sheet2.bin"),
        rel("rId3", "styles", "styles.bin"),
        rel("rId4", "sharedStrings", "sharedStrings.bin"),
    );
    let types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="bin" ContentType="application/vnd.ms-excel.sheet.binary.macroEnabled.main"/><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/></Types>"#;
    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.bin"/></Relationships>"#;
    make_zip(
        path,
        &[
            ("[Content_Types].xml", types.as_bytes()),
            ("_rels/.rels", root_rels.as_bytes()),
            ("xl/workbook.bin", &book),
            ("xl/_rels/workbook.bin.rels", rels.as_bytes()),
            ("xl/worksheets/sheet1.bin", &sheet1),
            ("xl/worksheets/sheet2.bin", &sheet2),
            ("xl/sharedStrings.bin", &sst),
            ("xl/styles.bin", &styles),
        ],
    );
}

#[test]
fn xlsb_workbooks_are_found_read_as_grids_and_restored() {
    // HANDOFF_S8 §8.4 / §10.4 (S13): `.xlsb` detection, never tested before.
    use std::sync::atomic::AtomicBool;
    use tm_core::merger::{merge_files_with_progress, MergeConfig};
    use tm_core::restore::{restore, RestoreOptions};
    use tm_core::scanner::{scan_text_files, ScanOptions};

    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let book = src.join("programs.xlsb");
    tiny_xlsb(&book);
    assert!(matches!(
        DocKind::from_ext("xlsb"),
        Some(DocKind::Spreadsheet)
    ));

    // The scanner takes it as a document to read, not a binary to skip.
    let scan = scan_text_files(&src, &ScanOptions::default()).expect("scan");
    assert_eq!(
        scan.files,
        vec![book.clone()],
        "skipped: {:?}",
        scan.skipped
    );

    // calamine reads it: every sheet, every typed cell, as TM-GRID/1.
    let out =
        extract(&book, DocKind::Spreadsheet, &ExtractOptions::default()).expect("xlsb extracts");
    let t = &out.text;
    for want in [
        "@sheet 1/2 name=\"Programs\" state=visible",
        "@sheet 2/2 name=\"Notes\" state=hidden",
        "Mercy \\| Lorain",
        "Łódź",
        "St. Vincent",
        "| 45.5 |",
        "| 12 |",
        "| -3 |",
        "| 0.001 |",
        "| TRUE |",
        "2025-10-31",
        "hidden note",
    ] {
        assert!(t.contains(want), "{want:?} missing from:\n{t}");
    }

    // In an archive twin it is embedded and comes back byte for byte.
    let twin = tmp.path().join("twin.md");
    let cfg = MergeConfig {
        redact: false,
        complete: true,
        ..MergeConfig::default()
    };
    let parts = merge_files_with_progress(
        &src,
        std::slice::from_ref(&book),
        &twin,
        &cfg,
        &AtomicBool::new(false),
        |_, _, _| {},
        &[],
    )
    .expect("merge")
    .outputs;
    let merged = std::fs::read_to_string(&parts[0]).unwrap();
    assert!(
        merged.contains("## programs.xlsb\n\n```tm-grid"),
        "no grid block:\n{}",
        &merged[..merged.len().min(2000)]
    );
    assert!(merged.contains("## __binary__: programs.xlsb"));
    let dest = tmp.path().join("back");
    let r = restore(&parts, &dest, RestoreOptions::default()).expect("restore");
    assert_eq!(r.restored, vec!["programs.xlsb".to_string()]);
    assert_eq!(
        std::fs::read(dest.join("programs.xlsb")).unwrap(),
        std::fs::read(&book).unwrap()
    );
}
