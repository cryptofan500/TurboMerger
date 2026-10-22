//! The merge's OCR stage end to end, with a stand-in engine (P7): every
//! distinct picture is read once, however many copies the folder holds; a
//! scanned PDF page made from a photo shares that photo's reading; a second
//! run reads nothing (the cache); results land in each file's own place.
//!
//! The engine and the cache are chosen by environment variables, which are
//! process-wide: this file is its own test binary, with one test that runs
//! its cases in turn.

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use tm_core::merger::{merge_files_with_progress, MergeConfig};

/// A two-page PDF: page 1 has a text layer, page 2 is one JPEG picture
/// (stored as `/DCTDecode`, as scanners and img2pdf write them).
fn mixed_pdf(jpeg: &[u8]) -> Vec<u8> {
    fn stream(dict: &str, data: &[u8]) -> Vec<u8> {
        let mut v = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
        v.extend_from_slice(data);
        v.extend_from_slice(b"\nendstream");
        v
    }
    let objects: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] \
           /Resources << /Font << /F1 7 0 R >> >> /Contents 5 0 R >>"
            .to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] \
           /Resources << /XObject << /Im1 8 0 R >> >> /Contents 6 0 R >>"
            .to_vec(),
        stream(
            "",
            b"BT /F1 14 Tf 20 150 Td (TEXTLAYER WALRUS typed page one) Tj ET",
        ),
        stream("", b"q 300 0 0 300 0 0 cm /Im1 Do Q"),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
        stream(
            "/Type /XObject /Subtype /Image /Width 4 /Height 4 /ColorSpace /DeviceRGB \
             /BitsPerComponent 8 /Filter /DCTDecode",
            jpeg,
        ),
    ];
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for o in offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

fn write(root: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, bytes).unwrap();
    p
}

fn merge(root: &Path, files: &[PathBuf], out: &Path) -> (String, tm_core::merger::MergeOutcome) {
    let cfg = MergeConfig {
        include_tree: false,
        redact: false,
        extract_images_text: true,
        ..MergeConfig::default()
    };
    let cancel = AtomicBool::new(false);
    let outcome =
        merge_files_with_progress(root, files, out, &cfg, &cancel, |_, _, _| {}, &[]).unwrap();
    (fs::read_to_string(out).unwrap(), outcome)
}

fn calls(log: &Path) -> usize {
    fs::read_to_string(log).map_or(0, |s| s.lines().count())
}

#[test]
fn the_ocr_stage_reads_each_picture_once_and_remembers() {
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("engine.log");
    let cache = tmp.path().join("cache");
    // The stand-in engine prints the printable bytes of the file it is given
    // and logs each start.
    let engine = write(
        tmp.path(),
        "engine.sh",
        format!(
            "echo \"$1\" >> '{}'\nLC_ALL=C tr -cd '[:alnum:] \\n' < \"$1\"\n",
            log.display()
        )
        .as_bytes(),
    );
    std::env::set_var(
        "TURBOMERGER_OCR_CMD",
        format!("sh {} {{input}}", engine.display()),
    );
    std::env::set_var("TURBOMERGER_OCR_CACHE", &cache);

    let root = tmp.path().join("photos");
    let jpeg = b"\xFF\xD8\xFF JUNIPER CANYON \xFF\xD9";
    let files = vec![
        write(&root, "a.png", b"MARMALADE ZEPPELIN"),
        write(&root, "b.png", b"LANTERN OCTOPUS"),
        write(&root, "copy/a_again.png", b"MARMALADE ZEPPELIN"),
        write(&root, "notes.txt", b"plain notes\n"),
        write(&root, "photo.jpg", jpeg),
        write(&root, "scan.pdf", &mixed_pdf(jpeg)),
    ];

    let (md, outcome) = merge(&root, &files, &tmp.path().join("one.md"));
    // Three distinct pictures: the copy and the PDF's page share a reading.
    assert_eq!(
        calls(&log),
        3,
        "{}",
        fs::read_to_string(&log).unwrap_or_default()
    );
    assert_eq!(outcome.files_processed, 6, "{:?}", outcome.skipped);
    assert_eq!(md.matches("MARMALADE ZEPPELIN").count(), 2, "{md}");
    assert_eq!(md.matches("JUNIPER CANYON").count(), 2, "{md}");
    // Each text in its file's place: path order.
    let at = |needle: &str| {
        md.find(needle)
            .unwrap_or_else(|| panic!("{needle} missing:\n{md}"))
    };
    assert!(at("MARMALADE") < at("LANTERN"));
    assert!(at("LANTERN") < at("plain notes"));
    assert!(at("plain notes") < at("JUNIPER"));
    assert!(at("TEXTLAYER WALRUS") < md.rfind("JUNIPER CANYON").unwrap());
    assert!(md.contains("1 page read with OCR"), "{md}");

    // A second run reads nothing: every picture is in the cache.
    let (again, _) = merge(&root, &files, &tmp.path().join("two.md"));
    assert_eq!(calls(&log), 3, "the second run started the engine");
    assert_eq!(again, md);

    // An engine that cannot start: each picture says why, nothing is lost
    // silently, and the text still merges.
    std::env::set_var("TURBOMERGER_OCR_CMD", "/nonexistent/tm-ocr-engine {input}");
    std::env::set_var("TURBOMERGER_OCR_CACHE", "off");
    let (md, outcome) = merge(&root, &files, &tmp.path().join("three.md"));
    assert!(md.contains("plain notes"));
    assert!(
        md.contains("TEXTLAYER WALRUS"),
        "the PDF's text layer stays"
    );
    let reasons: Vec<&str> = outcome.skipped.iter().map(|s| s.reason.as_str()).collect();
    assert_eq!(reasons.len(), 4, "{reasons:?}");
    assert!(
        reasons.iter().all(|r| r.contains("not found")),
        "{reasons:?}"
    );
    std::env::remove_var("TURBOMERGER_OCR_CMD");
    std::env::remove_var("TURBOMERGER_OCR_CACHE");
}
