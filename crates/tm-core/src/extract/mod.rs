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
//! - **xls/xlsx/xlsb/ods** → `calamine`, every sheet as TM-GRID/1 text (`grid`).
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
//! Done for big PDFs (P4): `pdf_oxide` holds about 6.6 x the file in memory
//! (a 450 MB scan peaked at 2.84 GiB), so a PDF over [`pdf_worker_above`]
//! goes to the parser a front end registered with [`set_pdf_offload`] (the
//! CLI's `__worker pdf` process). A worker that runs out of memory or
//! crashes takes only itself down; the file is reported, the merge goes on.

use std::io::Read;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::OnceLock;

mod child;
pub mod grid;
pub mod imgsize;
mod legacy;
mod ocr;

pub(crate) use ocr::FINISHED_EARLY as OCR_FINISHED_EARLY;
pub use ocr::{Got, OcrCancelled, OcrProgress, OcrQueue, Ticket};

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

/// The longest side of a thumbnail or an icon, not a page: monaddoc's own
/// thumbnails are 260 x 195; a photographed page is thousands of pixels.
const THUMBNAIL_MAX_SIDE: u32 = 320;

/// Why a picture is not sent to OCR when text-from-images is on, or `None`
/// to read it. Each rule leaves out pictures whose text is not there or is
/// merged already (P7); `--ocr-all-pictures` reads them anyway.
pub fn non_content_picture(path: &Path) -> Option<String> {
    const ANYWAY: &str = " — --ocr-all-pictures reads it";
    let parent = path.parent()?;
    let grandparent = parent.parent();
    // A MonadDoc work folder holds copies, renders and thumbnails of photos
    // that are read where they came from.
    if [Some(parent), grandparent]
        .into_iter()
        .flatten()
        .any(|d| d.join("input_manifest.json").is_file() && d.join("evidence").is_dir())
    {
        return Some(format!(
            "picture in a MonadDoc work folder (copies and renders of photos){ANYWAY}"
        ));
    }
    // `NAME_assets/` beside `NAME….md`: figure crops of a Markdown export,
    // whose text is in the Markdown.
    if let (Some(stem), Some(home)) = (
        parent
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix("_assets"))
            .filter(|s| !s.is_empty()),
        grandparent,
    ) {
        let markdown = std::fs::read_dir(home).ok().and_then(|entries| {
            entries.flatten().find_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                (name.starts_with(stem) && name.to_ascii_lowercase().ends_with(".md"))
                    .then_some(name)
            })
        });
        if let Some(md) = markdown {
            return Some(format!(
                "figure crop of {md} (its text is in the Markdown){ANYWAY}"
            ));
        }
    }
    let (w, h) = imgsize::dimensions(path)?;
    (w.max(h) <= THUMBNAIL_MAX_SIDE).then(|| format!("thumbnail or icon ({w} x {h} px){ANYWAY}"))
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
pub struct ExtractOptions<'a> {
    /// Read text inside images with OCR: image files, and the PDF pages that
    /// have no text layer. Off by default (`--extract-images-text`).
    pub ocr: bool,
    /// The OCR command; `None` uses `$TURBOMERGER_OCR_CMD` or `monaddoc`.
    pub ocr_cmd: Option<String>,
    /// The job's cancel flag: an OCR engine still running is stopped (with
    /// everything it started) as soon as it is set.
    pub cancel: Option<&'a AtomicBool>,
    /// TM-GRID/1 sheet, column and row selection for this file.
    pub grid_select: Option<&'a crate::selection::GridSelect>,
    /// TM-GRID/1 body rows per table chunk (0 = one table per sheet).
    pub grid_chunk_rows: usize,
    /// The file's relative display path, written into the `@file` line.
    pub grid_path: Option<&'a str>,
    /// `on_no_match = "error"`: a sheet or column that matches nothing fails the file.
    pub grid_strict: bool,
}

/// Extracted text plus a one-line provenance note for the Merge Report.
pub struct Extracted {
    pub text: String,
    pub note: String,
    pub lang: String,
}

impl Extracted {
    pub fn plain(text: String, note: String) -> Self {
        Self {
            text,
            note,
            lang: String::new(),
        }
    }
}

/// Extract `path`'s text. `Err(reason)` on a broken or empty document — the
/// caller records that as "not captured" and keeps going.
pub fn extract(path: &Path, kind: DocKind, opts: &ExtractOptions<'_>) -> Result<Extracted, String> {
    match kind {
        DocKind::Pdf => extract_pdf(path, opts),
        DocKind::Spreadsheet => extract_spreadsheet(path, opts),
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
            Ok(Extracted::plain(
                text,
                format!("text extracted from a {}", kind.label()),
            ))
        }
    }
}

// --------------------------------------------------------------------------- PDF

/// A parsed PDF: every page's text and, when a folder was given, the
/// picture of each page that has no text layer, ready for OCR.
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PdfParse {
    pub pages: Vec<String>,
    /// `(page, file)`: a page without text whose one picture was written
    /// into the folder (`p00007.jpg` holds a JPEG's own bytes, so a scan
    /// made from photos matches the photos' OCR cache entries).
    #[serde(default)]
    pub page_pictures: Vec<(usize, String)>,
    /// Pages with neither text nor a picture: nothing for OCR to read.
    #[serde(default)]
    pub empty_pages: Vec<usize>,
}

/// Parses a big PDF outside this process (a worker), writing page pictures
/// into the folder when one is given; `None` when no worker could run it,
/// and the PDF is parsed here as before.
pub type PdfOffload =
    dyn Fn(&Path, Option<&Path>) -> Option<Result<PdfParse, String>> + Send + Sync;

static PDF_OFFLOAD: OnceLock<Box<PdfOffload>> = OnceLock::new();

/// Register the out-of-process PDF parser (once per process; `false` if one
/// was already set). Without one, every PDF is parsed in-process.
pub fn set_pdf_offload(offload: Box<PdfOffload>) -> bool {
    PDF_OFFLOAD.set(offload).is_ok()
}

/// PDFs larger than this many bytes go to the registered worker: 256 MiB,
/// or `TURBOMERGER_PDF_WORKER_ABOVE_MB` (0 = every PDF; for tests).
pub fn pdf_worker_above() -> u64 {
    static ABOVE: OnceLock<u64> = OnceLock::new();
    *ABOVE.get_or_init(|| {
        std::env::var("TURBOMERGER_PDF_WORKER_ABOVE_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map_or(256 << 20, |mb| mb << 20)
    })
}

/// The text of every page of a PDF, parsed in this process (a worker calls
/// this too). Errors are the report's reasons.
pub fn pdf_pages(path: &Path) -> Result<Vec<String>, String> {
    pdf_parse(path, None).map(|p| p.pages)
}

/// [`pdf_pages`], plus — with `pictures` — each text-less page's picture
/// written into that folder for OCR: a page holding one picture gets it as
/// `p<page>.jpg` (a JPEG's stored bytes, unchanged) or `.png` (decoded); a
/// page with several pictures, or one that does not decode, gets none (the
/// whole PDF is read instead).
pub fn pdf_parse(path: &Path, pictures: Option<&Path>) -> Result<PdfParse, String> {
    // pdf_oxide keeps resolver state per THREAD, not per document
    // (`RESOLVING_STACK`, `RECURSION_DEPTH`, pdf_oxide 0.3.78 document.rs:329).
    // A malformed PDF that panics leaves entries behind; the next document on
    // that pool thread then gets false "circular reference" errors and loses
    // fonts — raw glyph codes on a page, depending on which PDF ran there next
    // (S11, HANDOFF_S8 §9: 3 of 120 arxiv PDFs, every time, with one thread).
    // Each PDF is parsed on its own short-lived thread, so that state starts
    // empty and dies with the document. The thread inherits the worker's
    // lowered CPU and I/O priority.
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("tm-pdf".into())
            .stack_size(PDF_THREAD_STACK)
            .spawn_scoped(scope, || pdf_parse_on_this_thread(path, pictures))
            .map_err(|e| format!("PDF parse failed: no parser thread: {e}"))?
            .join()
            .unwrap_or_else(|_| Err("PDF parser panicked on a malformed file".into()))
    })
}

/// Stack of the per-PDF parser thread: generous, since only the pages it
/// touches are ever resident.
const PDF_THREAD_STACK: usize = 16 << 20;

#[cfg(debug_assertions)]
thread_local! {
    /// Debug builds: PDFs parsed on this thread — never more than one.
    static PDFS_ON_THIS_THREAD: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn pdf_parse_on_this_thread(path: &Path, pictures: Option<&Path>) -> Result<PdfParse, String> {
    #[cfg(debug_assertions)]
    {
        let n = PDFS_ON_THIS_THREAD.with(|c| {
            c.set(c.get() + 1);
            c.get()
        });
        // An assertion with a message instead of a panic: the merge names it.
        if n != 1 {
            return Err(format!(
                "PDF parsed on a thread that already parsed {} PDF(s)",
                n - 1
            ));
        }
    }
    let owned = path.to_path_buf();
    // A malformed PDF must not panic the merge (debug/test: caught here;
    // release is panic=abort — see the module note).
    let parsed = catch_unwind(AssertUnwindSafe(|| -> Result<PdfParse, String> {
        let doc = pdf_oxide::PdfDocument::open(&owned).map_err(|e| e.to_string())?;
        let pages = doc.page_count().map_err(|e| e.to_string())?;
        let mut out = PdfParse {
            pages: Vec::with_capacity(pages),
            ..PdfParse::default()
        };
        for i in 0..pages {
            // One unreadable page does not sink the rest of the document.
            let t = doc.extract_text(i).unwrap_or_default();
            out.pages
                .push(t.trim_end_matches(['\n', '\r', ' ', '\t']).to_string());
        }
        if let Some(dir) = pictures {
            for i in 0..pages {
                if !out.pages[i].trim().is_empty() {
                    continue;
                }
                match page_picture(&doc, i, dir) {
                    PagePicture::Written(file) => out.page_pictures.push((i, file)),
                    PagePicture::None => out.empty_pages.push(i),
                    PagePicture::Unusable => {}
                }
            }
        }
        Ok(out)
    }));
    match parsed {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(format!("PDF parse failed: {e}")),
        Err(_) => Err("PDF parser panicked on a malformed file".into()),
    }
}

enum PagePicture {
    Written(String),
    /// No picture on the page at all.
    None,
    /// Several pictures, or one that cannot be taken out.
    Unusable,
}

fn page_picture(doc: &pdf_oxide::PdfDocument, page: usize, dir: &Path) -> PagePicture {
    let Ok(handles) = doc.page_image_handles(page) else {
        return PagePicture::Unusable;
    };
    match handles.as_slice() {
        [] => PagePicture::None,
        [one] => {
            let jpeg = matches!(
                one.filter_chain.as_slice(),
                [pdf_oxide::PdfFilter::DCTDecode]
            );
            let (bytes, ext) = if jpeg {
                (one.raw_compressed_bytes().ok(), "jpg")
            } else {
                (
                    one.decode().ok().and_then(|img| img.to_png_bytes().ok()),
                    "png",
                )
            };
            let Some(bytes) = bytes.filter(|b| !b.is_empty()) else {
                return PagePicture::Unusable;
            };
            let file = format!("p{:05}.{ext}", page + 1);
            match std::fs::write(dir.join(&file), bytes) {
                Ok(()) => PagePicture::Written(file),
                Err(_) => PagePicture::Unusable,
            }
        }
        _ => PagePicture::Unusable,
    }
}

/// The PDF's parse, by the worker when it is big and one is registered.
fn parse_pdf(path: &Path, pictures: Option<&Path>) -> Result<PdfParse, String> {
    let big = std::fs::metadata(path).is_ok_and(|m| m.len() > pdf_worker_above());
    let offloaded = big
        .then(|| PDF_OFFLOAD.get().and_then(|parse| parse(path, pictures)))
        .flatten();
    match offloaded {
        Some(parsed) => parsed,
        None => pdf_parse(path, pictures),
    }
}

/// How OCR went for a PDF's pages without a text layer.
#[derive(Default)]
struct PdfOcr {
    /// Text-from-images is on.
    on: bool,
    /// OCR text that could not be put on its pages (an engine that does not
    /// mark pages): appended after them.
    appended: Option<String>,
    /// Pages with neither text nor a picture (nothing to read).
    empty: usize,
    /// The first OCR error, and how many pages it left without text.
    error: Option<(String, usize)>,
}

fn extract_pdf(path: &Path, opts: &ExtractOptions<'_>) -> Result<Extracted, String> {
    let mut pages = parse_pdf(path, None)?.pages;
    let n = pages.len();
    // Pages with no text layer — a scan, a photographed signature page — are
    // read with OCR when text-from-images is on; pages with text keep it.
    let blank: Vec<usize> = (0..n).filter(|&i| pages[i].trim().is_empty()).collect();
    let mut ocr = PdfOcr {
        on: opts.ocr,
        ..PdfOcr::default()
    };
    if opts.ocr && !blank.is_empty() {
        match ocr::ocr_pages(path, opts.ocr_cmd.as_deref(), opts.cancel) {
            Ok(text) if text.len() == n => {
                for &i in &blank {
                    pages[i].clone_from(&text[i]);
                }
            }
            // The engine did not mark pages: its text stands for the pages
            // that had none.
            Ok(text) => ocr.appended = Some(text.join("\n\n")),
            Err(e) => ocr.error = Some((e, blank.len())),
        }
    }
    compose_pdf(pages, &blank, ocr)
}

/// A PDF's text and note from its pages (blank pages filled by OCR where it
/// ran). The note says what OCR did, and what it could not do.
fn compose_pdf(mut pages: Vec<String>, blank: &[usize], ocr: PdfOcr) -> Result<Extracted, String> {
    let n = pages.len();
    let mut note = format!("extracted from PDF ({} page{})", n, s(n));
    if ocr.on && !blank.is_empty() {
        if let Some(text) = ocr.appended.filter(|t| !t.trim().is_empty()) {
            note.push_str(&format!(
                "; OCR text for {} page{} without a text layer appended",
                blank.len(),
                s(blank.len())
            ));
            pages.push(text);
        } else if ocr.error.as_ref().is_none_or(|(_, k)| *k < blank.len()) {
            let filled = blank
                .iter()
                .filter(|&&i| !pages[i].trim().is_empty())
                .count();
            note.push_str(&format!("; {} page{} read with OCR", filled, s(filled)));
            let failed = ocr.error.as_ref().map_or(0, |(_, k)| *k);
            let unfilled = blank.len().saturating_sub(filled + ocr.empty + failed);
            if unfilled > 0 {
                note.push_str(&format!(
                    "; {} page{} still without text after OCR",
                    unfilled,
                    s(unfilled)
                ));
            }
            if ocr.empty > 0 {
                note.push_str(&format!(
                    "; {} blank page{} (no text, no picture)",
                    ocr.empty,
                    s(ocr.empty)
                ));
            }
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
        if let Some((e, k)) = &ocr.error {
            note.push_str(&format!("; OCR of {} page{} failed: {e}", k, s(*k)));
        }
        return Ok(Extracted::plain(text, note));
    }
    match ocr.error {
        Some((e, _)) => Err(format!(
            "PDF has no text layer ({} page{} of images); OCR failed: {e}",
            n,
            s(n)
        )),
        None if ocr.on => Err(format!("no text found, even with OCR ({} page{})", n, s(n))),
        None => Err(format!(
            "PDF has no text layer ({} page{} of images); rerun with --extract-images-text for OCR",
            n,
            s(n)
        )),
    }
}

/// An image file's text, read with OCR.
fn extract_image(path: &Path, opts: &ExtractOptions<'_>) -> Result<Extracted, String> {
    compose_image(ocr::ocr_pages(path, opts.ocr_cmd.as_deref(), opts.cancel))
}

fn compose_image(read: Result<Vec<String>, String>) -> Result<Extracted, String> {
    let text = read?.join("\n\n");
    if text.trim().is_empty() {
        return Err("no text found in the image (OCR)".into());
    }
    Ok(Extracted::plain(
        text,
        "text read from the image with OCR".into(),
    ))
}

// --------------------------------------------------------------- OCR as a stage

/// What a document became in a merge whose OCR runs as a stage
/// ([`extract_queued`]): its text now, or pages waiting for that stage.
pub enum Queued {
    Done(Extracted),
    Waiting(OcrWait),
}

/// A document waiting for the OCR stage: the text it has, and where each
/// missing page's text will come from. [`finish_queued`] completes it.
pub struct OcrWait {
    kind: DocKind,
    pages: Vec<String>,
    blank: Vec<usize>,
    slots: Vec<(usize, Slot)>,
    empty: usize,
}

enum Slot {
    /// Known already (the cache, or an error).
    Ready(Result<String, String>),
    /// A queued picture's text (all its pages).
    Picture(Ticket),
    /// The same page of a whole PDF the engine reads.
    PdfPage(Ticket),
}

/// [`extract`] for a merge that reads pictures in its own OCR stage: image
/// files and the text-less pages of PDFs are queued on `queue` (each
/// scanned page by its own picture when it has one), not read now. `name`
/// labels progress lines. Other documents are extracted as usual.
pub fn extract_queued(
    path: &Path,
    kind: DocKind,
    queue: &OcrQueue,
    name: &str,
) -> Result<Queued, String> {
    match kind {
        DocKind::Image => match queue.picture(path, name) {
            Got::Ready(read) => compose_image(read).map(Queued::Done),
            Got::Wait(t) => Ok(Queued::Waiting(OcrWait {
                kind,
                pages: vec![String::new()],
                blank: vec![0],
                slots: vec![(0, Slot::Picture(t))],
                empty: 0,
            })),
        },
        DocKind::Pdf => queue_pdf(path, queue, name),
        _ => extract(path, kind, &ExtractOptions::default()).map(Queued::Done),
    }
}

fn queue_pdf(path: &Path, queue: &OcrQueue, name: &str) -> Result<Queued, String> {
    // No folder (no engine, or no temporary space): pages are not taken
    // out, and the whole-PDF request below reports why.
    let folder = queue.pictures_folder().ok();
    let parsed = parse_pdf(path, folder.as_deref());
    let parsed = match parsed {
        Ok(p) => p,
        Err(e) => {
            if let Some(dir) = &folder {
                let _ = std::fs::remove_dir_all(dir);
            }
            return Err(e);
        }
    };
    let n = parsed.pages.len();
    let blank: Vec<usize> = (0..n)
        .filter(|&i| parsed.pages[i].trim().is_empty())
        .collect();
    let pictures: std::collections::HashMap<usize, String> =
        parsed.page_pictures.into_iter().collect();
    let empty: std::collections::HashSet<usize> = parsed.empty_pages.into_iter().collect();
    let mut slots = Vec::new();
    let mut whole: Option<Got> = None;
    for &i in &blank {
        if empty.contains(&i) {
            continue;
        }
        let page_name = format!("{name} (page {})", i + 1);
        let slot = match (pictures.get(&i), &folder) {
            (Some(file), Some(dir)) => match queue.page_picture(&dir.join(file), &page_name) {
                Got::Ready(read) => Slot::Ready(read.map(|v| v.join("\n\n"))),
                Got::Wait(t) => Slot::Picture(t),
            },
            _ => match whole.get_or_insert_with(|| queue.whole_pdf(path, n, name)) {
                Got::Ready(read) => {
                    Slot::Ready(read.clone().map(|v| v.get(i).cloned().unwrap_or_default()))
                }
                Got::Wait(t) => Slot::PdfPage(*t),
            },
        };
        slots.push((i, slot));
    }
    if let Some(dir) = &folder {
        let _ = std::fs::remove_dir_all(dir);
    }
    let wait = OcrWait {
        kind: DocKind::Pdf,
        pages: parsed.pages,
        blank,
        slots,
        empty: empty.len(),
    };
    if wait.slots.iter().all(|(_, s)| matches!(s, Slot::Ready(_))) {
        return finish_queued(wait, queue).map(Queued::Done);
    }
    Ok(Queued::Waiting(wait))
}

/// The text of a document that waited for the OCR stage ([`OcrQueue::run`]).
pub fn finish_queued(wait: OcrWait, queue: &OcrQueue) -> Result<Extracted, String> {
    let resolve = |page: usize, slot: &Slot| -> Result<String, String> {
        match slot {
            Slot::Ready(read) => read.clone(),
            Slot::Picture(t) => queue.result(*t).map(|v| v.join("\n\n")),
            Slot::PdfPage(t) => queue
                .result(*t)
                .map(|v| v.get(page).cloned().unwrap_or_default()),
        }
    };
    if wait.kind == DocKind::Image {
        let read = wait
            .slots
            .first()
            .map(|(p, slot)| resolve(*p, slot))
            .unwrap_or_else(|| Err("OCR did not run".into()));
        return compose_image(read.map(|t| vec![t]));
    }
    let mut pages = wait.pages;
    let mut error: Option<(String, usize)> = None;
    for (page, slot) in &wait.slots {
        match resolve(*page, slot) {
            Ok(text) => pages[*page] = text,
            Err(e) => {
                let e = error.get_or_insert((e, 0));
                e.1 += 1;
            }
        }
    }
    compose_pdf(
        pages,
        &wait.blank,
        PdfOcr {
            on: true,
            appended: None,
            empty: wait.empty,
            error,
        },
    )
}

// ------------------------------------------------------------------- spreadsheets

/// Workbooks become TM-GRID/1 text (ADR 0021): every non-empty cell at its
/// row and column, typed, escaped, with per-sheet counts and SHA-256.
fn extract_spreadsheet(path: &Path, opts: &ExtractOptions<'_>) -> Result<Extracted, String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path).map_err(|e| format!("spreadsheet read failed: {e}"))?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workbook");
    let src = grid::GridSource {
        rel_path: opts.grid_path.unwrap_or(name),
        bytes: bytes.len() as u64,
        sha256: format!("{:x}", Sha256::digest(&bytes)),
    };
    drop(bytes);
    let empty = crate::selection::GridSelect::default();
    let gopts = grid::GridOptions {
        select: opts.grid_select.unwrap_or(&empty),
        chunk_rows: opts.grid_chunk_rows,
        strict: opts.grid_strict,
    };
    grid::encode_workbook(path, &src, &gopts)
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
    Ok(Extracted::plain(text, note))
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

/// Outline a file's structure for writing a selection plan: Markdown
/// headings (ATX, outside fences — the same scanner the slicer uses), sheets
/// of a workbook, or the header of a CSV/TSV file.
pub fn outline_file(path: &Path) -> Result<serde_json::Value, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let file = path.display().to_string();
    match ext.as_str() {
        "md" | "markdown" | "txt" => {
            let content = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
            let headings: Vec<serde_json::Value> = crate::slice::outline_headings(&content)
                .into_iter()
                .map(|(line, level, text)| serde_json::json!({"line": line, "level": level, "text": text}))
                .collect();
            Ok(
                serde_json::json!({"file": file, "kind": "markdown", "lines": content.lines().count(), "headings": headings}),
            )
        }
        "csv" | "tsv" => {
            let content = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
            let delim = if ext == "tsv" { '\t' } else { ',' };
            let records = grid::parse_delimited(content.trim_start_matches('\u{feff}'), delim);
            let header = records.first().cloned().unwrap_or_default();
            Ok(
                serde_json::json!({"file": file, "kind": "table", "records": records.len(), "header": header}),
            )
        }
        "xlsx" | "xlsm" | "xlsb" | "xls" | "ods" => {
            use calamine::{open_workbook_auto, Reader};
            let mut wb =
                open_workbook_auto(path).map_err(|e| format!("cannot open spreadsheet: {e}"))?;
            let names: Vec<String> = wb.sheet_names().to_vec();
            let mut sheets = Vec::new();
            for name in &names {
                let (rows, cols, cells) = match wb.worksheet_range(name) {
                    Ok(range) => {
                        let (r, c) = range.get_size();
                        (
                            r,
                            c,
                            range
                                .used_cells()
                                .filter(|(_, _, v)| grid::canon(v).is_some())
                                .count(),
                        )
                    }
                    Err(_) => (0, 0, 0),
                };
                sheets.push(
                    serde_json::json!({"name": name, "rows": rows, "cols": cols, "cells": cells}),
                );
            }
            Ok(serde_json::json!({"file": file, "kind": "spreadsheet", "sheets": sheets}))
        }
        _ => Ok(serde_json::json!({"file": file, "kind": "unknown"})),
    }
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

    /// Thumbnails, figure crops beside their Markdown and MonadDoc work
    /// folders are not pages; a page-sized picture, or one of unknown size,
    /// is read.
    #[test]
    fn pictures_that_are_not_pages_are_named() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let png = |w: u32, h: u32| {
            let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
            b.extend_from_slice(&w.to_be_bytes());
            b.extend_from_slice(&h.to_be_bytes());
            b.extend_from_slice(&[8, 2, 0, 0, 0]);
            b
        };
        let put = |rel: &str, bytes: &[u8]| {
            let p = d.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, bytes).unwrap();
            p
        };
        let thumb = put("thumbs/a.png", &png(260, 195));
        assert!(non_content_picture(&thumb)
            .unwrap()
            .starts_with("thumbnail or icon (260 x 195 px)"));
        assert_eq!(
            non_content_picture(&put("page.png", &png(2550, 3300))),
            None
        );
        assert_eq!(
            non_content_picture(&put("unknown.heic", b"\0\0\0\x18ftypheic")),
            None
        );

        put("NSK/NSK_3_structured.md", b"# NSK\n");
        let crop = put("NSK/NSK_assets/p001-1.webp", b"RIFF");
        let why = non_content_picture(&crop).unwrap();
        assert!(
            why.starts_with("figure crop of NSK_3_structured.md"),
            "{why}"
        );
        // Without its Markdown, an `_assets` folder is just a folder.
        assert_eq!(
            non_content_picture(&put("other_assets/scan.png", &png(2000, 3000))),
            None
        );

        put("work/input_manifest.json", b"{}");
        std::fs::create_dir_all(d.join("work/evidence")).unwrap();
        let render = put("work/scan_pages/p1.png", &png(2550, 3300));
        assert!(non_content_picture(&render)
            .unwrap()
            .contains("MonadDoc work folder"));
    }

    #[test]
    fn slide_order_is_numeric() {
        assert!(slide_index("ppt/slides/slide2.xml") < slide_index("ppt/slides/slide10.xml"));
    }
}
