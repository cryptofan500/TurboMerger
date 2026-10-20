//! Document text extraction (Phase 3, ADR 0005 — "documents become bundles").
//!
//! Turns PDFs, Office files and e-books into plain text so their content lands
//! in the merge instead of being reported as "not captured". Everything here
//! is pure Rust: the three OSes behave identically, the product stays offline
//! (ADR 0006, no Python) and the dependency surface stays small enough to
//! survive two dormant years (ADR 0002, Class 2).
//!
//! - **PDF** → `pdf_oxide` (reading-order aware; keeps two-column papers in
//!   reading order, strips running heads/page numbers). A scanned PDF has no
//!   text layer, so it yields nothing here; with `ocr` on, the file is handed
//!   to an external OCR command (`monaddoc` by default). PDFium rendering of
//!   figure/table crops into an image bundle is the next step (tm-worker).
//! - **xls/xlsx/xlsb/ods** → `calamine`, every sheet as tab-separated rows.
//! - **docx/pptx/odt/odp/epub** → a zip of XML parts; the text nodes are pulled
//!   with `quick-xml`.
//! - **doc/ppt** (Office 97-2003) → the OLE compound file (`cfb`): Word's
//!   piece table, PowerPoint's text atoms (`legacy`).
//!
//! Robustness: a document is one file in a merge of thousands, so a broken or
//! hostile file must never take the whole run down. Each extractor returns
//! `Err` on a broken archive (reported as "not captured", never fatal), and the
//! PDF parser additionally runs inside `catch_unwind`. NOTE: the release
//! profile is `panic = "abort"` (ADR 0016), where `catch_unwind` cannot catch;
//! `pdf_oxide` is validated at a 100% pass rate on 3,830 adversarial PDFs
//! (veraPDF / pdf.js / DARPA SafeDocs), and the committed hardening is to move
//! PDF parsing into the `tm-worker` subprocess the host can cap and kill.

use std::io::Read;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;

mod legacy;
mod ocr;

/// A document format TurboMerger can extract text from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocKind {
    Pdf,
    Docx,
    Pptx,
    Spreadsheet, // xls / xlsx / xlsb / xlsm / ods — calamine auto-detects
    Odt,
    Odp,
    Epub,
    /// Word 97-2003 (`.doc`, `.dot`).
    LegacyDoc,
    /// PowerPoint 97-2003 (`.ppt`, `.pps`, `.pot`).
    LegacyPpt,
    /// A raster image, read with OCR. Never returned by `from_ext`: images go
    /// to OCR only when text-from-images is on (`is_ocr_image`).
    Image,
}

/// Raster image formats OCR can read (what monaddoc accepts).
pub fn is_ocr_image(ext_lower: &str) -> bool {
    matches!(
        ext_lower,
        "png"
            | "jpg"
            | "jpeg"
            | "webp"
            | "gif"
            | "bmp"
            | "tif"
            | "tiff"
            | "heic"
            | "heif"
            | "avif"
            | "jxl"
            | "ppm"
            | "pgm"
            | "pbm"
            | "pnm"
            | "qoi"
    )
}

impl DocKind {
    /// The format for a lowercase extension, if TurboMerger has an extractor.
    /// `None` means "not a document we extract" (leave it to the normal rules).
    pub fn from_ext(ext_lower: &str) -> Option<DocKind> {
        Some(match ext_lower {
            "pdf" => DocKind::Pdf,
            "docx" => DocKind::Docx,
            "pptx" => DocKind::Pptx,
            "xls" | "xlsx" | "xlsm" | "xlsb" | "ods" => DocKind::Spreadsheet,
            "odt" => DocKind::Odt,
            "odp" => DocKind::Odp,
            "epub" => DocKind::Epub,
            "doc" | "dot" => DocKind::LegacyDoc,
            "ppt" | "pps" | "pot" => DocKind::LegacyPpt,
            _ => return None,
        })
    }

    /// Human name for the Merge Report and notes.
    pub fn label(self) -> &'static str {
        match self {
            DocKind::Pdf => "PDF",
            DocKind::Docx => "Word document",
            DocKind::Pptx => "PowerPoint deck",
            DocKind::Spreadsheet => "spreadsheet",
            DocKind::Odt => "OpenDocument text",
            DocKind::Odp => "OpenDocument presentation",
            DocKind::Epub => "EPUB",
            DocKind::LegacyDoc => "Word 97-2003 document",
            DocKind::LegacyPpt => "PowerPoint 97-2003 deck",
            DocKind::Image => "image",
        }
    }
}

/// How to extract. The merge config decides whether OCR is allowed.
#[derive(Debug, Clone, Default)]
pub struct ExtractOptions {
    /// Read text inside images with OCR: image files, and the PDF pages that
    /// have no text layer. Off by default (`--extract-images-text`).
    pub ocr: bool,
    /// The OCR command; `None` uses `$TURBOMERGER_OCR_CMD` or `monaddoc`.
    pub ocr_cmd: Option<String>,
}

/// Extracted text plus a one-line provenance note for the Merge Report.
pub struct Extracted {
    pub text: String,
    pub note: String,
}

/// Extract `path`'s text. `Err(reason)` on a broken or empty document — the
/// caller records that as "not captured" and keeps going.
pub fn extract(path: &Path, kind: DocKind, opts: &ExtractOptions) -> Result<Extracted, String> {
    match kind {
        DocKind::Pdf => extract_pdf(path, opts),
        DocKind::Spreadsheet => extract_spreadsheet(path),
        DocKind::Docx => extract_zip_xml(path, ZipXml::Docx),
        DocKind::Pptx => extract_zip_xml(path, ZipXml::Pptx),
        DocKind::Odt | DocKind::Odp => extract_zip_xml(path, ZipXml::Odf),
        DocKind::Epub => extract_zip_xml(path, ZipXml::Epub),
        DocKind::Image => extract_image(path, opts),
        DocKind::LegacyDoc | DocKind::LegacyPpt => {
            let text = if kind == DocKind::LegacyDoc {
                legacy::extract_doc(path)?
            } else {
                legacy::extract_ppt(path)?
            };
            let text = collapse_blank_lines(&text);
            if text.trim().is_empty() {
                return Err("no text in the document".into());
            }
            Ok(Extracted {
                text,
                note: format!("text extracted from a {}", kind.label()),
            })
        }
    }
}

// --------------------------------------------------------------------------- PDF

fn extract_pdf(path: &Path, opts: &ExtractOptions) -> Result<Extracted, String> {
    let owned = path.to_path_buf();
    // A malformed PDF must not panic the merge (debug/test: caught here;
    // release is panic=abort — see the module note).
    let parsed = catch_unwind(AssertUnwindSafe(|| -> Result<Vec<String>, String> {
        let doc = pdf_oxide::PdfDocument::open(&owned).map_err(|e| e.to_string())?;
        let pages = doc.page_count().map_err(|e| e.to_string())?;
        let mut out = Vec::with_capacity(pages);
        for i in 0..pages {
            // One unreadable page does not sink the rest of the document.
            let t = doc.extract_text(i).unwrap_or_default();
            out.push(t.trim_end_matches(['\n', '\r', ' ', '\t']).to_string());
        }
        Ok(out)
    }));
    let mut pages = match parsed {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Err(format!("PDF parse failed: {e}")),
        Err(_) => return Err("PDF parser panicked on a malformed file".into()),
    };
    let n = pages.len();
    let mut note = format!("extracted from PDF ({} page{})", n, s(n));

    // Pages with no text layer — a scan, a photographed signature page — are
    // read with OCR when text-from-images is on; pages with text keep it.
    let blank: Vec<usize> = (0..n).filter(|&i| pages[i].trim().is_empty()).collect();
    let mut ocr_error = None;
    if opts.ocr && !blank.is_empty() {
        match ocr::ocr_pages(path, opts.ocr_cmd.as_deref()) {
            Ok(ocr) if ocr.len() == n => {
                for &i in &blank {
                    pages[i].clone_from(&ocr[i]);
                }
                let filled = blank
                    .iter()
                    .filter(|&&i| !pages[i].trim().is_empty())
                    .count();
                note.push_str(&format!("; {} page{} read with OCR", filled, s(filled)));
                let unfilled = blank.len() - filled;
                if unfilled > 0 {
                    note.push_str(&format!(
                        "; {} page{} still without text after OCR",
                        unfilled,
                        s(unfilled)
                    ));
                }
            }
            Ok(ocr) => {
                // The engine did not mark pages: its text stands for the pages
                // that had none.
                let text = ocr.join("\n\n");
                if !text.trim().is_empty() {
                    note.push_str(&format!(
                        "; OCR text for {} page{} without a text layer appended",
                        blank.len(),
                        s(blank.len())
                    ));
                    pages.push(text);
                }
            }
            Err(e) => ocr_error = Some(e),
        }
    } else if !blank.is_empty() && blank.len() < n {
        // Some pages have no text layer (a scanned signature page, an
        // appendix, or a blank page): say so, or their text vanishes behind
        // "extracted from PDF". All pages blank is reported below.
        note.push_str(&format!(
            "; {} page{} without a text layer skipped — rerun with --extract-images-text for OCR",
            blank.len(),
            s(blank.len())
        ));
    }

    let text = pages
        .iter()
        .filter(|p| !p.trim().is_empty())
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n\n");
    if !text.trim().is_empty() {
        if let Some(e) = ocr_error {
            note.push_str(&format!(
                "; OCR of {} page{} failed: {e}",
                blank.len(),
                s(blank.len())
            ));
        }
        return Ok(Extracted { text, note });
    }
    match ocr_error {
        Some(e) => Err(format!(
            "PDF has no text layer ({} page{} of images); OCR failed: {e}",
            n,
            s(n)
        )),
        None if opts.ocr => Err(format!("no text found, even with OCR ({} page{})", n, s(n))),
        None => Err(format!(
            "PDF has no text layer ({} page{} of images); rerun with --extract-images-text for OCR",
            n,
            s(n)
        )),
    }
}

/// An image file's text, read with OCR.
fn extract_image(path: &Path, opts: &ExtractOptions) -> Result<Extracted, String> {
    let text = ocr::ocr_pages(path, opts.ocr_cmd.as_deref())?.join("\n\n");
    if text.trim().is_empty() {
        return Err("no text found in the image (OCR)".into());
    }
    Ok(Extracted {
        text,
        note: "text read from the image with OCR".into(),
    })
}

// ------------------------------------------------------------------- spreadsheets

fn extract_spreadsheet(path: &Path) -> Result<Extracted, String> {
    use calamine::{open_workbook_auto, Reader};
    let mut wb = open_workbook_auto(path).map_err(|e| format!("spreadsheet open failed: {e}"))?;
    let names = wb.sheet_names().to_vec();
    let mut out = String::new();
    let mut sheets = 0usize;
    for name in &names {
        let range = match wb.worksheet_range(name) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if range.is_empty() {
            continue;
        }
        sheets += 1;
        if names.len() > 1 {
            out.push_str("## ");
            out.push_str(name);
            out.push_str("\n\n");
        }
        for row in range.rows() {
            let line: Vec<String> = row.iter().map(|c| c.to_string()).collect();
            // Drop fully empty trailing rows' noise but keep structure.
            out.push_str(line.join("\t").trim_end());
            out.push('\n');
        }
        out.push('\n');
    }
    let text = out.trim();
    if text.is_empty() {
        return Err("spreadsheet has no cells".into());
    }
    Ok(Extracted {
        text: text.to_string(),
        note: format!("extracted from spreadsheet ({} sheet{})", sheets, s(sheets)),
    })
}

// ------------------------------------------------------------- zip-of-XML formats

#[derive(Clone, Copy)]
enum ZipXml {
    Docx,
    Pptx,
    Odf,
    Epub,
}

fn extract_zip_xml(path: &Path, fmt: ZipXml) -> Result<Extracted, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("open failed: {e}"))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("not a readable archive: {e}"))?;

    // Which inner parts carry the prose, in order.
    let parts: Vec<String> = match fmt {
        ZipXml::Docx => vec!["word/document.xml".to_string()],
        ZipXml::Odf => vec!["content.xml".to_string()],
        ZipXml::Pptx => {
            let mut v: Vec<String> = (0..zip.len())
                .filter_map(|i| zip.by_index(i).ok().map(|f| f.name().to_string()))
                .filter(|n| n.starts_with("ppt/slides/slide") && n.ends_with(".xml"))
                .collect();
            v.sort_by_key(|n| slide_index(n));
            v
        }
        ZipXml::Epub => {
            let mut v: Vec<String> = (0..zip.len())
                .filter_map(|i| zip.by_index(i).ok().map(|f| f.name().to_string()))
                .filter(|n| {
                    let l = n.to_ascii_lowercase();
                    l.ends_with(".xhtml") || l.ends_with(".html") || l.ends_with(".htm")
                })
                .collect();
            v.sort();
            v
        }
    };

    // Which element ends start a new line (quick-xml 0.42 names are &str).
    let breaks: &[&str] = match fmt {
        ZipXml::Docx => &["w:p"],
        ZipXml::Pptx => &["a:p"],
        ZipXml::Odf => &["text:p", "text:h"],
        ZipXml::Epub => &["p", "div", "br", "h1", "h2", "h3", "h4", "h5", "h6", "li"],
    };

    let mut out = String::new();
    let mut found = 0usize;
    for part in &parts {
        let mut buf = Vec::new();
        {
            let mut entry = match zip.by_name(part) {
                Ok(e) => e,
                Err(_) => continue,
            };
            if entry.read_to_end(&mut buf).is_err() {
                continue;
            }
        }
        found += 1;
        pull_xml_text(&buf, breaks, &mut out);
        out.push_str("\n\n");
    }

    if found == 0 {
        return Err("archive is missing its text parts".into());
    }
    let text = collapse_blank_lines(out.trim());
    if text.is_empty() {
        return Err("no text in the document".into());
    }
    let note = match fmt {
        ZipXml::Pptx => format!("extracted from PowerPoint ({} slide{})", found, s(found)),
        ZipXml::Epub => format!("extracted from EPUB ({} document{})", found, s(found)),
        ZipXml::Docx => "extracted from Word document".to_string(),
        ZipXml::Odf => "extracted from OpenDocument".to_string(),
    };
    Ok(Extracted { text, note })
}

fn slide_index(name: &str) -> u32 {
    name.trim_start_matches("ppt/slides/slide")
        .trim_end_matches(".xml")
        .parse()
        .unwrap_or(u32::MAX)
}

/// Pull text nodes out of one XML part, starting a new line when any element in
/// `breaks` closes. Best-effort: stops at the first malformation and keeps what
/// it has (a truncated document is better than none). quick-xml 0.42 is
/// `&str`-based, so the part must be UTF-8 (OOXML/ODF/XHTML always are).
fn pull_xml_text(xml: &[u8], breaks: &[&str], out: &mut String) {
    use quick_xml::events::Event;
    let text = match std::str::from_utf8(xml) {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut reader = quick_xml::Reader::from_str(text);
    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(Event::Text(e)) => match quick_xml::escape::unescape(e.as_ref()) {
                Ok(t) => out.push_str(&t),
                Err(_) => out.push_str(e.as_ref()),
            },
            Ok(Event::Empty(e)) => {
                let q = e.name();
                let n = q.as_ref();
                if n == "w:tab" || n == "text:tab" {
                    out.push('\t');
                } else if n == "w:br" || n == "a:br" || n == "br" || n == "text:line-break" {
                    out.push('\n');
                }
            }
            Ok(Event::End(e)) => {
                let q = e.name();
                let n = q.as_ref();
                if breaks.contains(&n) {
                    out.push('\n');
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
}

// ---------------------------------------------------------------------------- OCR

// ------------------------------------------------------------------------- helpers

/// Pluralizer: `s(1)` = "", `s(3)` = "s".
fn s(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Squeeze runs of 3+ blank lines down to one — extractors emit a blank line
/// per paragraph, which stacks up across parts.
fn collapse_blank_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blanks = 0u32;
    for line in text.lines() {
        if line.trim().is_empty() {
            blanks += 1;
            if blanks <= 1 {
                out.push('\n');
            }
        } else {
            blanks = 0;
            out.push_str(line);
            out.push('\n');
        }
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ext_mapping() {
        assert_eq!(DocKind::from_ext("pdf"), Some(DocKind::Pdf));
        assert_eq!(DocKind::from_ext("xlsx"), Some(DocKind::Spreadsheet));
        assert_eq!(DocKind::from_ext("ods"), Some(DocKind::Spreadsheet));
        assert_eq!(DocKind::from_ext("epub"), Some(DocKind::Epub));
        assert_eq!(DocKind::from_ext("rs"), None);
        assert_eq!(DocKind::from_ext("png"), None);
    }

    #[test]
    fn docx_like_xml_pulls_paragraphs() {
        let xml = b"<w:document><w:body>\
            <w:p><w:r><w:t>Hello</w:t></w:r><w:r><w:t> world</w:t></w:r></w:p>\
            <w:p><w:r><w:t>Second</w:t></w:r></w:p>\
            </w:body></w:document>";
        let mut out = String::new();
        pull_xml_text(xml, &["w:p"], &mut out);
        assert!(out.contains("Hello world"), "got: {out:?}");
        assert!(out.contains("Second"), "got: {out:?}");
        // The two paragraphs are on different lines.
        let joined = collapse_blank_lines(out.trim());
        assert_eq!(joined.lines().count(), 2, "got: {joined:?}");
    }

    #[test]
    fn collapse_blank_lines_squeezes() {
        assert_eq!(collapse_blank_lines("a\n\n\n\nb"), "a\n\nb");
        assert_eq!(collapse_blank_lines("a\nb"), "a\nb");
    }

    #[test]
    fn slide_order_is_numeric() {
        assert!(slide_index("ppt/slides/slide2.xml") < slide_index("ppt/slides/slide10.xml"));
    }
}
