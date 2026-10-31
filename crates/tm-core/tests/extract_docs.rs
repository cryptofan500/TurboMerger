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

#[test]
fn every_pdf_is_parsed_on_a_thread_of_its_own() {
    // S11 (HANDOFF_S8 §9): pdf_oxide keeps resolver state per thread, and a
    // panicking PDF leaves it dirty for the next PDF on that thread (fonts
    // lost, raw glyph codes). Debug builds refuse a parse on a reused thread,
    // so two parses from this one test thread must both run elsewhere.
    let tmp = tempfile::tempdir().unwrap();
    for name in ["a.pdf", "b.pdf", "c.pdf"] {
        let p = tmp.path().join(name);
        std::fs::write(&p, "%PDF-1.7\nnot really a pdf\n").unwrap();
        let err = tm_core::extract::pdf_parse(&p, None)
            .err()
            .unwrap_or_default();
        assert!(!err.contains("already parsed"), "{name}: {err}");
    }
}
