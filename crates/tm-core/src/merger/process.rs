//! One file in, one block (or a skip) out: read, decode, slim, redact, count.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::LazyLock;

use regex::Regex;

use super::{relative_display, Block, MergeConfig, MergeSkip};
use crate::extract::{OcrQueue, OcrWait};
use crate::scanner::SkipKind;
use sha2::Digest as _;

static BASE64_RUN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z0-9+/]{200,}={0,2}").expect("base64 run regex"));

/// A processed file: its block and text, or the reason it was dropped.
/// `harvested` = labeled secret values (high confidence, propagated
/// unconditionally); `dense` = whole-file opaque tokens from a credential
/// dump (propagated behind a frequency guard).
pub(super) struct Processed {
    pub(super) result: std::result::Result<(Block, String), MergeSkip>,
    pub(super) harvested: Vec<String>,
    pub(super) dense: Vec<String>,
    /// The file must take the sequential streaming path instead (archives,
    /// big gzip, plain files past `STREAM_ABOVE`); `result` is unused then.
    pub(super) stream: bool,
    /// Pages waiting for the merge's OCR stage: `result` holds the block to
    /// keep its place in the order; [`finish_waiting`] completes it.
    pub(super) wait: Option<OcrWait>,
}

impl Processed {
    /// Nothing to merge here (a cancelled run, or a file for the streaming path).
    pub(super) fn empty(stream: bool) -> Processed {
        Processed {
            result: Err((String::new(), String::new(), SkipKind::Excluded)),
            harvested: Vec::new(),
            dense: Vec::new(),
            stream,
            wait: None,
        }
    }
}

/// Read + decode + slim + redact + token-count a single already-vetted file.
/// Also returns the secrets harvested from the decoded text — harvested
/// BEFORE the credential-density exclusion, so an excluded credential file
/// still teaches the propagation pass its values. `cancel` stops an OCR
/// engine still reading this file; with `ocr`, pictures (and the scanned
/// pages of PDFs) are queued for the merge's OCR stage instead.
pub(super) fn process_file(
    path: &Path,
    root: &Path,
    cfg: &MergeConfig,
    cancel: &AtomicBool,
    ocr: Option<&OcrQueue>,
) -> Processed {
    // Test hook, debug builds only: make every file slow so the deadline,
    // partial-output and stall paths can be exercised end to end.
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var("TM_TEST_SLOW_FILE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
    let mut harvested: Vec<String> = Vec::new();
    let mut dense: Vec<String> = Vec::new();
    let mut wait = None;
    let streamed = || Processed::empty(true);
    if needs_streaming(path) {
        return streamed();
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let result = if super::stream::container_of(name) == Some(super::stream::Container::Gzip) {
        // Small gzip (`changelog.gz`, `notes.txt.gz`): inflate inline, with a
        // hard limit — the trailer's size is only a hint, so a file that
        // inflates past it after all goes to the streaming path.
        let relative = relative_display(root, path);
        match inflate_limited(path, super::stream::GZIP_INLINE_MAX) {
            Ok(Some(bytes)) => {
                let lang_from = relative
                    .strip_suffix(".gz")
                    .or_else(|| relative.strip_suffix(".GZ"))
                    .unwrap_or(&relative)
                    .to_string();
                process_bytes(
                    relative,
                    &lang_from,
                    bytes,
                    Some("inflated from gzip"),
                    cfg,
                    &mut harvested,
                    &mut dense,
                )
            }
            Ok(None) => return streamed(),
            Err(e) => Err((relative, e, SkipKind::NotCaptured)),
        }
    } else {
        let mut ctx = Inner {
            cfg,
            harvested: &mut harvested,
            dense: &mut dense,
            cancel,
            ocr,
            wait: &mut wait,
        };
        process_file_inner(path, root, &mut ctx)
    };
    Processed {
        result,
        harvested,
        dense,
        stream: false,
        wait,
    }
}

/// What one file's processing reads and fills in.
struct Inner<'a> {
    cfg: &'a MergeConfig,
    harvested: &'a mut Vec<String>,
    dense: &'a mut Vec<String>,
    cancel: &'a AtomicBool,
    ocr: Option<&'a OcrQueue>,
    wait: &'a mut Option<OcrWait>,
}

/// Archives, gzip that inflates past `GZIP_INLINE_MAX`, and plain files past
/// `STREAM_ABOVE` take the sequential streaming path (`merger::stream`), so
/// no file is too big to merge and memory stays bounded.
pub(super) fn needs_streaming(path: &Path) -> bool {
    use super::stream::{
        container_of, gzip_inflated_size, Container, GZIP_INLINE_MAX, STREAM_ABOVE,
    };
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if crate::extract::DocKind::from_ext(&ext).is_some() {
        return false;
    }
    let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    match container_of(name) {
        Some(Container::Gzip) => {
            len > GZIP_INLINE_MAX || gzip_inflated_size(path).is_none_or(|n| n > GZIP_INLINE_MAX)
        }
        Some(_) => true,
        None => len > STREAM_ABOVE,
    }
}

/// Inflate a gzip file, or `None` when it holds more than `limit` bytes.
fn inflate_limited(path: &Path, limit: u64) -> Result<Option<Vec<u8>>, String> {
    let file = File::open(path).map_err(|e| format!("unreadable: {e}"))?;
    let mut out = Vec::new();
    flate2::read::MultiGzDecoder::new(BufReader::new(file))
        .take(limit + 1)
        .read_to_end(&mut out)
        .map_err(|e| format!("not a readable gzip file: {e}"))?;
    Ok((out.len() as u64 <= limit).then_some(out))
}

fn process_file_inner(
    path: &Path,
    root: &Path,
    ctx: &mut Inner<'_>,
) -> std::result::Result<(Block, String), MergeSkip> {
    let relative = relative_display(root, path);
    let cfg = ctx.cfg;

    // Documents (PDF, Office, e-book) have their text extracted (Phase 3,
    // ADR 0005) rather than being read as raw bytes — the binary sniff below
    // would otherwise reject a PDF outright. The extracted text then runs
    // through the same redaction + token-count path as any file.
    if let Some(kind) = Path::new(&relative)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
        .and_then(crate::extract::DocKind::from_ext)
    {
        return process_document(path, relative, kind, ctx);
    }
    // Text inside images, when text-from-images is on (the scanner routes
    // images here only then).
    if cfg.extract_images_text
        && Path::new(&relative)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| crate::extract::is_ocr_image(&e.to_ascii_lowercase()))
    {
        return process_document(path, relative, crate::extract::DocKind::Image, ctx);
    }
    let (harvested, dense) = (&mut *ctx.harvested, &mut *ctx.dense);

    let file = File::open(path).map_err(|e| {
        (
            relative.clone(),
            format!("unreadable: {}", e),
            SkipKind::NotCaptured,
        )
    })?;
    let mut buffer = Vec::new();
    if BufReader::new(file).read_to_end(&mut buffer).is_err() {
        return Err((relative, "unreadable".into(), SkipKind::NotCaptured));
    }
    let lang_from = relative.clone();
    process_bytes(relative, &lang_from, buffer, None, cfg, harvested, dense)
}

/// Decode + exclude-if-credential-dump + slim + redact + count one file's
/// bytes. `lang_from` names the file whose extension sets the fence language
/// (`notes.md` for `notes.md.gz`); `note` heads the decoding note.
fn process_bytes(
    relative: String,
    lang_from: &str,
    buffer: Vec<u8>,
    note: Option<&str>,
    cfg: &MergeConfig,
    harvested: &mut Vec<String>,
    dense: &mut Vec<String>,
) -> std::result::Result<(Block, String), MergeSkip> {
    // A byte-order mark — or BOM-less UTF-16, which puts a zero byte in every
    // second position (MetaTrader configs, Windows exports) — legitimizes the
    // null bytes the binary sniff would otherwise reject, so encoding
    // detection must run first.
    let check = 8192.min(buffer.len());
    let bom = encoding_rs::Encoding::for_bom(&buffer)
        .or_else(|| crate::security::utf16_text_encoding(&buffer[..check]));
    if bom.is_none() && !buffer.is_empty() && crate::security::is_binary_content(&buffer[..check]) {
        return Err((relative, "binary content".into(), SkipKind::Binary));
    }

    let source = if cfg.selection.is_some() {
        Some((
            buffer.len() as u64,
            format!("{:x}", sha2::Sha256::digest(&buffer)),
        ))
    } else {
        None
    };
    let (mut content, utf8_note) = decode_text(buffer, bom);
    let mut utf8_note = match (note, utf8_note) {
        (Some(n), Some(u)) => Some(format!("{n}; {u}")),
        (Some(n), None) => Some(n.to_string()),
        (None, u) => u,
    };

    // Selection plan (ADR 0020/0021). A selector that cannot be applied makes
    // the file "not captured" with the reason — never a silent whole-file merge.
    let ext = Path::new(lang_from)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mut grid = false;
    if let Some((plan, sel)) = cfg
        .selection
        .as_ref()
        .and_then(|p| p.find_file(&relative).map(|f| (p, f)))
    {
        let strict = plan.on_no_match == crate::selection::Strictness::Error;
        let fail = |e: String| {
            (
                relative.clone(),
                format!("selection: {e}"),
                SkipKind::NotCaptured,
            )
        };
        let delim = match ext.as_str() {
            "csv" => Some(','),
            "tsv" | "tab" => Some('\t'),
            _ => None,
        };
        if crate::slice::has_table_slice(sel) {
            let Some(d) = delim else {
                return Err(fail(
                    "columns/rows/sheets apply only to spreadsheets and CSV/TSV".into(),
                ));
            };
            if crate::slice::has_text_slice(sel) {
                return Err(fail("a table cannot also take lines/sections/match".into()));
            }
            let (bytes, sha256) = source.clone().unwrap_or_default();
            let src = crate::extract::grid::GridSource {
                rel_path: &relative,
                bytes,
                sha256,
            };
            let select = sel.to_grid_select();
            let gopts = crate::extract::grid::GridOptions {
                select: &select,
                chunk_rows: cfg.grid_chunk_rows,
                strict,
            };
            let text = content.strip_prefix('\u{feff}').unwrap_or(&content);
            let (grid_text, grid_note) =
                crate::extract::grid::encode_delimited(text, d, &src, &gopts).map_err(fail)?;
            content = grid_text;
            utf8_note = Some(match utf8_note {
                Some(n) => format!("{n}; {grid_note}"),
                None => grid_note,
            });
            grid = true;
        } else {
            if cfg.reproducible && content.contains('\r') {
                content = content.replace("\r\n", "\n");
            }
            if crate::slice::has_text_slice(sel) {
                let (sliced, report) =
                    crate::slice::slice_text(&content, sel, strict).map_err(fail)?;
                content = sliced;
                if !report.warnings.is_empty() {
                    let w = format!("selection warnings: {}", report.warnings.join("; "));
                    utf8_note = Some(match utf8_note {
                        Some(n) => format!("{n}; {w}"),
                        None => w,
                    });
                }
            }
        }
    }
    if !grid && cfg.reproducible && content.contains('\r') {
        content = content.replace("\r\n", "\n");
    }

    if cfg.redact {
        *harvested = crate::security::harvest_labeled_values(&content);
    }

    // Whole-file exclusion for credential DUMPS (inline login tables, Google
    // app-passwords, key blocks) that per-line redaction can't fully scrub.
    // Only when redacting — with redaction off every text file is taken as it
    // is — and only for a real dump, at least one line in ten a credential: a
    // long document that mentions a few logins keeps its text and gets
    // per-value redaction (the count-only rule dropped 57 dossiers, ledgers
    // and transcripts from one research folder). Checked on the raw decoded
    // text, before any slimming/redaction. Every opaque token of a dump is
    // harvested for propagation — the file IS a credential store, and its
    // values echo in prose elsewhere (grammar no labeled rule can parse).
    let cred_count = if cfg.redact {
        crate::security::credential_indicator_count(&content)
    } else {
        0
    };
    if cred_count >= crate::security::CREDENTIAL_DENSITY_THRESHOLD
        && crate::security::is_credential_dump(&content, cred_count)
    {
        // Whole-token harvest only for credential DUMPS in document form: a
        // dense CODE file (a seed script with test logins) would contribute
        // its identifiers, and a long document that merely trips the density
        // rule (a paper's author e-mails) would contribute every citation and
        // model name — 1,011 corrupting edits on 120 arXiv papers (N-13).
        let ext = Path::new(&relative)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(
            ext.as_str(),
            "md" | "markdown" | "txt" | "text" | "csv" | "log" | ""
        ) {
            *dense = crate::security::harvest_dense_file_tokens(&content);
        }
        return Err((
            relative,
            format!(
                "credential-dense content ({} inline credentials) — excluded",
                cred_count
            ),
            SkipKind::NotCaptured,
        ));
    }

    let lang = if grid { "tm-grid".to_string() } else { ext };

    // Tree-sitter reductions (a failed parse keeps the original; redaction
    // still runs on whatever survives). Comment strip must run BEFORE
    // compression: elided bodies (`{ ... }`) are placeholders, not parseable
    // source, so the reverse order can't re-parse.
    let mut compressed = false;
    if cfg.strip_comments {
        if let Some(slim) = crate::compress::strip_comments(&content, &lang) {
            content = slim;
        }
    }
    if cfg.compress {
        if let Some(slim) = crate::compress::compress_signatures(&content, &lang) {
            content = slim;
            compressed = true;
        }
    }

    // A grid's blank lines separate its chunked tables and its cells are data:
    // neither text reduction applies to it.
    if cfg.truncate_base64 && !grid {
        content = truncate_base64(&content);
    }
    if cfg.remove_empty_lines && !grid {
        content = drop_empty_lines(&content);
    }

    let redactions = if cfg.redact {
        let (clean, events) = crate::security::redact_secrets(&content);
        content = clean;
        events.into_iter().map(|ev| (ev.rule, ev.count)).collect()
    } else {
        Vec::new()
    };

    let tokens = crate::cache::count(&content);

    Ok((
        Block {
            relative,
            span: Default::default(),
            lang,
            tokens,
            utf8_note,
            redactions,
            compressed,
            synthetic: false,
        },
        content,
    ))
}

/// `--truncate-base64`: long base64 runs become a short marker.
pub(super) fn truncate_base64(text: &str) -> String {
    BASE64_RUN
        .replace_all(text, "[base64 omitted]")
        .into_owned()
}

/// `--remove-empty-lines`.
pub(super) fn drop_empty_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        if !line.trim().is_empty() {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Extract a document's text (PDF, Office, e-book) and fold it into the same
/// block shape as a text file. Unlike the text path this skips the
/// credential-density *exclusion*: a document (e.g. a research paper full of
/// author e-mails) should still be captured, with per-value redaction applied.
fn process_document(
    path: &Path,
    relative: String,
    kind: crate::extract::DocKind,
    ctx: &mut Inner<'_>,
) -> std::result::Result<(Block, String), MergeSkip> {
    use crate::extract::{DocKind, Queued};
    let queued = match ctx.ocr {
        Some(queue) if matches!(kind, DocKind::Pdf | DocKind::Image) => {
            crate::extract::extract_queued(path, kind, queue, &relative)
        }
        _ => {
            let plan = ctx.cfg.selection.as_ref();
            let grid_sel = plan
                .and_then(|p| p.find_file(&relative))
                .map(|f| f.to_grid_select());
            let opts = crate::extract::ExtractOptions {
                ocr: ctx.cfg.extract_images_text,
                ocr_cmd: None,
                cancel: Some(ctx.cancel),
                grid_select: grid_sel.as_ref(),
                grid_chunk_rows: ctx.cfg.grid_chunk_rows,
                grid_path: Some(&relative),
                grid_strict: plan
                    .is_none_or(|p| p.on_no_match == crate::selection::Strictness::Error),
            };
            crate::extract::extract(path, kind, &opts).map(Queued::Done)
        }
    }
    .map_err(|e| (relative.clone(), e, SkipKind::NotCaptured))?;
    match queued {
        Queued::Done(extracted) => document_block(relative, extracted, ctx.cfg, ctx.harvested),
        Queued::Waiting(wait) => {
            *ctx.wait = Some(wait);
            // Keeps the file's place in the order until the OCR stage.
            Ok((
                Block {
                    relative,
                    span: Default::default(),
                    lang: String::new(),
                    tokens: 0,
                    utf8_note: None,
                    redactions: Vec::new(),
                    compressed: false,
                    synthetic: false,
                },
                String::new(),
            ))
        }
    }
}

/// A document whose pages waited for the OCR stage, completed: its block and
/// text, or why it is not captured. Returns the secrets harvested from it.
pub(super) fn finish_waiting(
    relative: String,
    wait: OcrWait,
    queue: &OcrQueue,
    cfg: &MergeConfig,
) -> (std::result::Result<(Block, String), MergeSkip>, Vec<String>) {
    let mut harvested = Vec::new();
    let result = match crate::extract::finish_queued(wait, queue) {
        Ok(extracted) => document_block(relative, extracted, cfg, &mut harvested),
        Err(e) => Err((relative, e, SkipKind::NotCaptured)),
    };
    (result, harvested)
}

/// An extracted document's block: redacted (when on) and counted.
fn document_block(
    relative: String,
    extracted: crate::extract::Extracted,
    cfg: &MergeConfig,
    harvested: &mut Vec<String>,
) -> std::result::Result<(Block, String), MergeSkip> {
    let mut content = extracted.text;
    let mut note = extracted.note;
    let grid = extracted.lang == "tm-grid";
    if cfg.reproducible && !grid && content.contains('\r') {
        content = content.replace("\r\n", "\n");
    }

    // Workbooks took their table selection in the encoder; extracted prose
    // (PDF, Word, slides) can take a text slice.
    if let Some((plan, sel)) = cfg
        .selection
        .as_ref()
        .and_then(|p| p.find_file(&relative).map(|f| (p, f)))
    {
        let strict = plan.on_no_match == crate::selection::Strictness::Error;
        let fail = |e: String| {
            (
                relative.clone(),
                format!("selection: {e}"),
                SkipKind::NotCaptured,
            )
        };
        if !grid && crate::slice::has_table_slice(sel) {
            return Err(fail(
                "columns/rows/sheets apply only to spreadsheets and CSV/TSV".into(),
            ));
        }
        if crate::slice::has_text_slice(sel) {
            if grid {
                return Err(fail(
                    "a workbook cannot take lines/sections/match; use sheets/columns/rows".into(),
                ));
            }
            let (sliced, report) = crate::slice::slice_text(&content, sel, strict).map_err(fail)?;
            content = sliced;
            if !report.warnings.is_empty() {
                note.push_str(&format!(
                    "; selection warnings: {}",
                    report.warnings.join("; ")
                ));
            }
        }
    }

    let redactions = if cfg.redact {
        *harvested = crate::security::harvest_labeled_values(&content);
        let (clean, events) = crate::security::redact_secrets(&content);
        content = clean;
        events.into_iter().map(|ev| (ev.rule, ev.count)).collect()
    } else {
        Vec::new()
    };

    let tokens = crate::cache::count(&content);

    Ok((
        Block {
            relative,
            span: Default::default(),
            lang: extracted.lang,
            tokens,
            utf8_note: Some(note),
            redactions,
            compressed: false,
            synthetic: false,
        },
        content,
    ))
}

/// Decode raw file bytes to a String: BOM → strict UTF-8 → chardetng-guessed
/// legacy encoding → lossy UTF-8. Returns the text plus an optional note for
/// the Merge Report's "Decoding notes".
fn decode_text(
    buffer: Vec<u8>,
    bom: Option<(&'static encoding_rs::Encoding, usize)>,
) -> (String, Option<String>) {
    if let Some((enc, bom_len)) = bom {
        if enc == encoding_rs::UTF_8 {
            // Strip the BOM, then take the normal UTF-8 path below.
        } else {
            let (text, had_errors) = enc.decode_without_bom_handling(&buffer[bom_len..]);
            let how = if bom_len == 0 {
                "no byte-order mark"
            } else {
                "BOM"
            };
            let note = if had_errors {
                format!(
                    "decoded from {} ({how}) with replacement characters",
                    enc.name()
                )
            } else {
                format!("decoded from {} ({how})", enc.name())
            };
            return (text.into_owned(), Some(note));
        }
    }
    let start = bom.map(|(_, len)| len).unwrap_or(0);
    let body = &buffer[start..];

    match std::str::from_utf8(body) {
        Ok(s) => (s.to_string(), None),
        Err(_) => {
            // Strict UTF-8 already failed, so deny UTF-8 as a guess; a legacy
            // single-byte decode (windows-1252 default) never errors, matching
            // what editors do on auto-detect.
            let mut det = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Deny);
            det.feed(body, true);
            let enc = det.guess(None, chardetng::Utf8Detection::Deny);
            if enc != encoding_rs::UTF_8 {
                let (text, had_errors) = enc.decode_without_bom_handling(body);
                let note = if had_errors {
                    format!(
                        "not UTF-8 — decoded as {} (detected) with replacement characters",
                        enc.name()
                    )
                } else {
                    format!("not UTF-8 — decoded as {} (detected)", enc.name())
                };
                (text.into_owned(), Some(note))
            } else {
                let s = String::from_utf8_lossy(body).into_owned();
                let n = s.matches('\u{FFFD}').count();
                (
                    s,
                    Some(format!(
                        "not valid UTF-8 — {} byte(s) replaced with U+FFFD (consider re-saving as UTF-8)",
                        n
                    )),
                )
            }
        }
    }
}
