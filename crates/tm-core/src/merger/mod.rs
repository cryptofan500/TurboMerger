//! Merger: reads scanned files (parallel), redacts secrets, optionally slims
//! content, counts tokens, orders files, and writes one or more output files in
//! the chosen format (Markdown / XML / Claude-XML / JSON / Plain), splitting by
//! a token budget when asked.
//!
//! Streaming (v8 plan Phase 2.7, D4/N-36/N-37): files are processed in
//! bounded parallel chunks and each block's text goes to an unlinked spool
//! file as soon as its chunk finishes; memory holds only per-block metadata.
//! The known-value masking passes and the writers read the spool back one
//! chunk or one block at a time. The bytes written are the same as the
//! in-memory pipeline's (tests/golden.rs).

mod checkpoint;
mod git;
mod pipeline;
mod process;
mod render;
mod split;
mod spool;
mod stream;

use std::collections::{BTreeSet, HashSet};
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrd};
use std::sync::OnceLock;

use anyhow::Result;
use rayon::prelude::*;

use crate::scanner::{SkipEntry, SkipKind};
use crate::tokens;
use pipeline::{Ended, Flow, Next};
use render::RenderCtx;
use spool::{Span, Spool};

/// The masking passes read the spool back at most this many blocks, or
/// this many bytes of text, at a time.
const CHUNK_FILES: usize = 64;
const CHUNK_BYTES: u64 = 64 << 20;

/// Weight in flight while merging (`TURBOMERGER_INFLIGHT_MB` overrides it):
/// bytes of files being read or waiting to be written, a document counted
/// [`DOC_WEIGHT`] times (a PDF parse holds several times the file).
const INFLIGHT_BYTES: u64 = 256 << 20;
const DOC_WEIGHT: u64 = 4;
/// How far ahead of the file being written the workers may run.
const WINDOW_FILES: usize = 1024;

fn pipeline_limits() -> pipeline::Limits {
    static BUDGET: OnceLock<u64> = OnceLock::new();
    let budget = *BUDGET.get_or_init(|| {
        std::env::var("TURBOMERGER_INFLIGHT_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|&mb| mb > 0)
            .map_or(INFLIGHT_BYTES, |mb| mb << 20)
    });
    pipeline::Limits {
        budget,
        window: WINDOW_FILES,
    }
}

/// A file's share of the in-flight budget: its size, several times over
/// for a document, nothing for one the merge streams itself.
fn work_weight(path: &Path, len: u64) -> u64 {
    let doc = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| crate::extract::DocKind::from_ext(&e).is_some());
    if doc {
        len.saturating_mul(DOC_WEIGHT)
    } else if process::needs_streaming(path) {
        0
    } else {
        len
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Markdown,
    Xml,
    Cxml,
    Json,
    Plain,
}

impl OutputFormat {
    pub fn from_str_lenient(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "xml" => OutputFormat::Xml,
            "cxml" | "claude" => OutputFormat::Cxml,
            "json" => OutputFormat::Json,
            "plain" | "text" | "txt" => OutputFormat::Plain,
            _ => OutputFormat::Markdown,
        }
    }
    pub fn extension(&self) -> &'static str {
        match self {
            OutputFormat::Json => "json",
            OutputFormat::Xml | OutputFormat::Cxml => "xml",
            _ => "md",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ordering {
    Path,
    EntryFirst,
    ImportantLast,
    Plan,
}

impl Ordering {
    pub fn from_str_lenient(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "entry-first" | "entryfirst" | "entry" => Ordering::EntryFirst,
            "important-last" | "importantlast" => Ordering::ImportantLast,
            "plan" => Ordering::Plan,
            _ => Ordering::Path,
        }
    }
}

pub struct MergeConfig {
    pub include_tree: bool,
    pub redact: bool,
    pub format: OutputFormat,
    pub ordering: Ordering,
    /// Split output into parts if the total exceeds this many o200k tokens.
    pub max_tokens: Option<usize>,
    pub remove_empty_lines: bool,
    pub truncate_base64: bool,
    /// Elide function/method bodies via tree-sitter (signatures-only mode).
    pub compress: bool,
    /// Remove comments via tree-sitter.
    pub strip_comments: bool,
    /// Append `git diff HEAD` as a final section (T2-5).
    pub git_diff: bool,
    /// Append `git log -n N` as a final section; 0 = off (T2-5).
    pub git_log: usize,
    /// Write `.claude/skills/<repo>/SKILL.md` into the scanned repo (T3-4).
    pub emit_skill: bool,
    /// What the header calls the source. `None` = `local folder "<name>"`;
    /// remote packs pass the repository URL. Absolute local paths are never
    /// written unless `show_source_path` is set (N-21).
    pub source_label: Option<String>,
    pub show_source_path: bool,
    /// The root is a temporary clone (skills written into it would vanish).
    pub remote: bool,
    /// Same bytes on every OS (N-16): CRLF → LF in contents, NFC paths
    /// (display and order), no timestamps.
    pub reproducible: bool,
    /// Send image-only document pages (a scanned PDF with no text layer) to the
    /// external OCR command. Off by default (`--extract-images-text`).
    pub extract_images_text: bool,
    /// With OCR, keep the figure crops monaddoc cuts out of pages beside the
    /// output (`<output stem>_assets/`) and link them from each page's text.
    /// Off by default (`--keep-figures`).
    pub keep_figures: bool,
    /// Per-file cap in bytes for text that only shows its size once read —
    /// inflated gzip, archive members, streamed files. 0 = no cap.
    pub max_file_size: u64,
    /// Granular selection plan for files, slices and sheets.
    pub selection: Option<crate::selection::SelectionPlan>,
    /// Spreadsheet chunk interval (default 150; 0 = one table per sheet).
    pub grid_chunk_rows: usize,
    /// Archive twin mode: appends manifest block for offline restoration.
    pub complete: bool,
    /// Archive mode: embed only files up to N MB; a larger file is withheld,
    /// named in the Merge Report and counted in `archive_gaps`. `None` = no
    /// cap (every file its text block cannot rebuild is embedded).
    pub embed_binary: Option<u64>,
    /// Archive mode: files a complete plan names that the scanner did not
    /// hand to the merge (binary, a photo without OCR, an archive format it
    /// cannot open). Not merged as text; the archive twin keeps their bytes.
    pub archive_also: Vec<PathBuf>,
    /// Token budget ceiling: fail gate if exceeded.
    pub budget_tokens: Option<u64>,
    /// Keep a checkpoint beside the output (`<output>.tm-checkpoint/`) so a
    /// stopped run can continue with `resume` (S10 4.3; not with `redact`).
    pub checkpoint: bool,
    /// Continue from that checkpoint when there is one (else start one):
    /// unchanged files are taken from it instead of being read again.
    pub resume: bool,
}

impl Default for MergeConfig {
    fn default() -> Self {
        Self {
            include_tree: true,
            redact: true,
            format: OutputFormat::Markdown,
            ordering: Ordering::Path,
            max_tokens: None,
            remove_empty_lines: false,
            truncate_base64: false,
            compress: false,
            strip_comments: false,
            git_diff: false,
            git_log: 0,
            emit_skill: false,
            source_label: None,
            show_source_path: false,
            remote: false,
            reproducible: false,
            extract_images_text: false,
            keep_figures: false,
            max_file_size: 0,
            selection: None,
            grid_chunk_rows: 150,
            complete: false,
            embed_binary: None,
            archive_also: Vec::new(),
            budget_tokens: None,
            checkpoint: false,
            resume: false,
        }
    }
}

#[derive(Debug, Default)]
pub struct MergeOutcome {
    pub total_bytes: usize,
    pub files_processed: usize,
    pub files_skipped: usize,
    pub secrets_redacted: usize,
    pub files_compressed: usize,
    pub tokens_o200k: usize,
    pub outputs: Vec<PathBuf>,
    /// Path of the generated SKILL.md, when `emit_skill` was on and it wrote.
    pub skill: Option<PathBuf>,
    /// Files dropped at merge time (binary content, credential-dense, …) and
    /// git-context problems — the scan's own skips are the caller's.
    pub skipped: Vec<SkipEntry>,
    /// Remarks about the output itself (e.g. a part over its budget).
    pub notes: Vec<String>,
    /// The merge finished early (`MergeCtl::finish`): files it did not get to
    /// are in `skipped` as not captured.
    pub partial: bool,
    /// Figure crops kept beside the output (`keep_figures`), and where.
    pub figures_kept: usize,
    pub figures_dir: Option<PathBuf>,
    /// `--complete`: source files the archive twin cannot restore byte for
    /// byte, with the reason (withheld under redaction or over the embed cap,
    /// unreadable, not processed).
    pub archive_gaps: Vec<(String, String)>,
}

/// Where kept figures go: `<output stem>_assets/` beside the output file.
fn figures_dir_for(output: &Path) -> PathBuf {
    let stem = output
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".into());
    output.with_file_name(format!("{stem}_assets"))
}

/// Runtime control of one merge.
#[derive(Clone, Copy)]
pub struct MergeCtl<'a> {
    /// Stop and write nothing (`Err(Cancelled)`).
    pub cancel: &'a AtomicBool,
    /// Stop taking new files, then write what is done.
    pub finish: Option<&'a AtomicBool>,
    /// Live figures a front end can read while the merge runs.
    pub watch: Option<&'a MergeWatch>,
    /// The OCR stage's progress: pages read, pages in all, and what is
    /// being read (or why nothing is, e.g. "waiting for memory").
    pub on_ocr: Option<&'a OcrCallback<'a>>,
}

/// `(pages read, pages in all, what is being read)`.
pub type OcrCallback<'a> = dyn Fn(usize, usize, &str) + Sync + 'a;

/// What a merge reports besides its per-file callback: bytes for an ETA
/// that weighs files by size, and the OCR time still to come.
#[derive(Debug, Default)]
pub struct MergeWatch {
    /// Input bytes of the files finished so far.
    pub bytes_done: AtomicU64,
    /// Input bytes of every file to merge.
    pub bytes_total: AtomicU64,
    /// Seconds the OCR stage will take for the pictures queued so far, at
    /// this machine's last measured speed.
    pub ocr_secs: AtomicU64,
}

/// The merge stopped because its cancel flag was set; nothing was written.
pub use crate::cancel::Cancelled;

/// One merged file's metadata; its text lives in the spool at `span`.
pub(crate) struct Block {
    relative: String,
    span: Span,
    lang: String,
    tokens: usize,
    utf8_note: Option<String>,
    redactions: Vec<(&'static str, usize)>,
    compressed: bool,
    /// Git-context sections and similar: real output, but not part of the
    /// scanned file set, so the project tree must not list them.
    synthetic: bool,
    /// Set when the file's text was split to fit `--max-tokens`.
    piece: Option<Piece>,
}

/// One piece of a block split to fit `--max-tokens` (HANDOFF_S8 §6.10 item
/// 3.3): pieces are exact slices of the block's text, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Piece {
    /// This piece's number (from 1) and how many there are.
    k: u32,
    n: u32,
    /// The piece ends inside a line that the next piece continues.
    cont: bool,
}

impl Block {
    /// The block's name in the output: its path, plus `(piece k/n)` when
    /// the file was split (`, line continues` when the cut is inside a line).
    fn label(&self) -> std::borrow::Cow<'_, str> {
        match self.piece {
            None => std::borrow::Cow::Borrowed(&self.relative),
            Some(p) => std::borrow::Cow::Owned(format!(
                "{} (piece {}/{}{})",
                self.relative,
                p.k,
                p.n,
                if p.cont { ", line continues" } else { "" }
            )),
        }
    }
}

/// (relative_path, reason, kind) for a file dropped at merge time
type MergeSkip = (String, String, SkipKind);

/// A dense-file token seen in more than this many blocks is a common term
/// (project name, hostname), not a secret — the frequency guard drops it.
const DENSE_TOKEN_MAX_BLOCKS: usize = 8;

fn cancelled(flag: &AtomicBool) -> bool {
    flag.load(AtomicOrd::Relaxed)
}

/// Top-level entry point. Returns the outcome (including all output paths),
/// or `Cancelled` (nothing written) when `cancel_flag` is set mid-way.
#[allow(clippy::too_many_arguments)]
pub fn merge_files_with_progress<F>(
    root: &Path,
    files: &[PathBuf],
    output: &Path,
    cfg: &MergeConfig,
    cancel_flag: &AtomicBool,
    progress_callback: F,
    scan_skips: &[SkipEntry],
) -> Result<MergeOutcome>
where
    F: FnMut(usize, usize, &str),
{
    let ctl = MergeCtl {
        cancel: cancel_flag,
        finish: None,
        watch: None,
        on_ocr: None,
    };
    merge_files_ctl(root, files, output, cfg, ctl, progress_callback, scan_skips)
}

/// `merge_files_with_progress` with finish-early support (`MergeCtl`).
#[allow(clippy::too_many_arguments)]
pub fn merge_files_ctl<F>(
    root: &Path,
    files: &[PathBuf],
    output: &Path,
    cfg: &MergeConfig,
    ctl: MergeCtl,
    mut progress_callback: F,
    scan_skips: &[SkipEntry],
) -> Result<MergeOutcome>
where
    F: FnMut(usize, usize, &str),
{
    let cancel_flag = ctl.cancel;
    let finishing = || ctl.finish.is_some_and(|f| f.load(AtomicOrd::Relaxed));
    let total_files = files.len();
    if cfg.complete && cfg.format != OutputFormat::Markdown {
        anyhow::bail!("the archive twin (--complete) is written in the markdown format only");
    }
    let checkpointing = cfg.checkpoint || cfg.resume;
    if checkpointing && cfg.redact {
        anyhow::bail!(
            "--checkpoint / --resume cannot be used with --redact: the checkpoint would keep unredacted text and the harvested secret values on disk"
        );
    }

    // Order files (a stable copy so the caller's slice is untouched).
    let mut ordered: Vec<&PathBuf> = files.iter().collect();
    order_files(root, &mut ordered, cfg.ordering, cfg.reproducible);
    // --reproducible: the report's paths are NFC too, in NFC order.
    let nfc_skips: Vec<SkipEntry>;
    let scan_skips: &[SkipEntry] = if cfg.reproducible {
        let mut v: Vec<SkipEntry> = scan_skips
            .iter()
            .map(|s| SkipEntry::new(nfc(&s.path), s.reason.clone(), s.kind))
            .collect();
        v.sort_by(|a, b| a.path.cmp(&b.path));
        nfc_skips = v;
        &nfc_skips
    } else {
        scan_skips
    };

    // With a checkpoint the spool is its named file (it outlives a killed
    // run); otherwise an unlinked temp file beside the output.
    let (mut ck, mut spool) = if checkpointing {
        let (c, s) = checkpoint::Checkpoint::open(output, root, cfg)?;
        (Some(c), s)
    } else {
        (None, Spool::new_near(output)?)
    };
    let mut blocks: Vec<Block> = Vec::with_capacity(total_files);
    let mut merge_skips: Vec<SkipEntry> = Vec::new();

    // Labeled secret values harvested from every read file — INCLUDING
    // credential-dense files that are then excluded — for repo-wide
    // propagation after processing. Dense-file opaque tokens ride separately
    // (they face a frequency guard first).
    // Sorted sets: every later step iterates them, and the output must be
    // byte-identical across runs (N-14).
    let mut known_secrets: BTreeSet<String> = BTreeSet::new();
    let mut dense_candidates: BTreeSet<String> = BTreeSet::new();

    // Pictures, and the pages of PDFs that have no text layer, wait for one
    // OCR stage after the text (P7): each distinct picture is read once, in
    // batches, and remembered across runs.
    let ocr = cfg.extract_images_text.then(|| {
        let figures = cfg.keep_figures.then(|| figures_dir_for(output));
        crate::extract::OcrQueue::with_figures(None, figures).staging_near(output)
    });
    let mut waiting: Vec<(usize, crate::extract::OcrWait)> = Vec::new();

    // Sizes up front: the scheduler's weights and the byte-weighted ETA.
    let lens: Vec<u64> = crate::threads::on_pool(|| {
        ordered
            .par_iter()
            .map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
            .collect()
    });
    let mut weights: Vec<u64> = crate::threads::on_pool(|| {
        ordered
            .par_iter()
            .zip(lens.par_iter())
            .map(|(p, &len)| work_weight(p, len))
            .collect()
    });
    // --resume: files the checkpoint holds, unchanged since (size and
    // modification time), come from it; the workers skip them (weight 0).
    let key = |p: &Path| relative_display(root, p);
    let stamps: Vec<Option<checkpoint::Stamp>> = if checkpointing {
        crate::threads::on_pool(|| {
            ordered
                .par_iter()
                .map(|p| checkpoint::Stamp::of(p))
                .collect()
        })
    } else {
        Vec::new()
    };
    let mut restored: Vec<Option<checkpoint::Entry>> = match ck.as_mut() {
        Some(c) if c.found > 0 => ordered
            .iter()
            .zip(&stamps)
            .map(|(p, s)| c.take(&key(p), *s))
            .collect(),
        _ => Vec::new(),
    };
    let is_restored: Vec<bool> = restored.iter().map(Option::is_some).collect();
    for (w, _) in weights.iter_mut().zip(&is_restored).filter(|(_, r)| **r) {
        *w = 0;
    }
    let watch = ctl.watch;
    if let Some(w) = watch {
        w.bytes_total.store(lens.iter().sum(), AtomicOrd::Relaxed);
        w.bytes_done.store(0, AtomicOrd::Relaxed);
    }
    let ocr_still = |w: &MergeWatch| {
        if let Some(q) = &ocr {
            w.ocr_secs.store(q.pending().1 as u64, AtomicOrd::Relaxed);
        }
    };

    // Every worker takes the next file as soon as it is free; results are
    // written in order (pipeline.rs), so no chunk waits for its slowest file.
    let work = |i: usize| -> process::Processed {
        if cancelled(cancel_flag) || is_restored.get(i).copied().unwrap_or(false) {
            return process::Processed::empty(false);
        }
        let p = process::process_file(ordered[i], root, cfg, cancel_flag, ocr.as_ref());
        if let (Some(w), false) = (watch, p.stream) {
            w.bytes_done.fetch_add(lens[i], AtomicOrd::Relaxed);
        }
        p
    };
    let flow = || {
        if cancelled(cancel_flag) {
            Flow::Cancel
        } else if finishing() {
            Flow::Finish
        } else {
            Flow::Go
        }
    };
    let mut done = 0usize;
    let mut partial = false;
    let mut unprocessed = vec![false; total_files];
    progress_callback(0, total_files, "");
    let ended = pipeline::ordered(
        total_files,
        pipeline_limits(),
        &|i| weights[i],
        &work,
        &flow,
        &mut |next| -> Result<()> {
            let (i, p) = match next {
                Next::Waiting { finished, oldest } => {
                    // Waiting on a slow file: what is finished goes to the
                    // journal meanwhile (S10: a journal waits for no file).
                    if let Some(c) = ck.as_mut() {
                        c.tick(&mut spool)?;
                    }
                    // The stall line names the file the merge waits on
                    // longest, not the first of a batch.
                    let current = oldest
                        .map(|o| relative_display(root, ordered[o]))
                        .unwrap_or_default();
                    progress_callback(finished.max(done), total_files, &current);
                    if let Some(w) = watch {
                        ocr_still(w);
                    }
                    return Ok(());
                }
                Next::Skipped(i) => {
                    // Finishing early: what is left is reported, never dropped.
                    partial = true;
                    unprocessed[i] = true;
                    done += 1;
                    let rel = relative_display(root, ordered[i]);
                    progress_callback(done, total_files, &rel);
                    merge_skips.push(SkipEntry::new(
                        rel,
                        "not processed — the run was finished early (deadline)",
                        SkipKind::NotCaptured,
                    ));
                    return Ok(());
                }
                Next::Item(i, p) => (i, p),
            };
            let path = ordered[i];
            // Taken from the checkpoint: its blocks and its skip, in its place.
            if let Some(entry) = restored.get_mut(i).and_then(Option::take) {
                done += 1;
                let (from_ck, skip) = entry.restore();
                let rel = from_ck
                    .first()
                    .map(|b| b.relative.clone())
                    .or_else(|| skip.as_ref().map(|s| s.path.clone()))
                    .unwrap_or_default();
                blocks.extend(from_ck);
                merge_skips.extend(skip);
                if let Some(w) = watch {
                    w.bytes_done.fetch_add(lens[i], AtomicOrd::Relaxed);
                }
                if let Some(c) = ck.as_mut() {
                    c.resumed += 1;
                }
                progress_callback(done, total_files, &rel);
                return Ok(());
            }
            let stamp = stamps.get(i).copied().flatten();
            known_secrets.extend(p.harvested);
            dense_candidates.extend(p.dense);
            done += 1;
            if p.stream {
                // A streamed file can take minutes: journal what is finished
                // before it (S10: a 1 GiB log held 22 s of entries back).
                if let Some(c) = ck.as_mut() {
                    c.flush(&mut spool)?;
                }
                // Archives, big gzip and files past `STREAM_ABOVE`: segment by
                // segment straight into the spool, in this file's place in
                // the order, so no size is too big and memory stays bounded.
                // The workers keep going on the files after it meanwhile.
                let rel = relative_display(root, path);
                let rel = if cfg.reproducible { nfc(&rel) } else { rel };
                let first = blocks.len();
                let streamed = {
                    let mut emit = |mut b: Block, text: &str| -> std::io::Result<()> {
                        b.span = spool.append(text)?;
                        blocks.push(b);
                        Ok(())
                    };
                    let mut read_so_far = |bytes: u64| {
                        progress_callback(
                            done - 1,
                            total_files,
                            &format!("{rel} ({} MB read)", bytes >> 20),
                        )
                    };
                    let mut sink = stream::Sink {
                        emit: &mut emit,
                        progress: &mut read_so_far,
                        cancel: cancel_flag,
                    };
                    stream::stream_source(path, &rel, stream::container_of(&rel), cfg, &mut sink)
                };
                if let Some(w) = watch {
                    w.bytes_done.fetch_add(lens[i], AtomicOrd::Relaxed);
                }
                progress_callback(done, total_files, &rel);
                let failed = streamed.is_err();
                match streamed {
                    Ok(out) => {
                        known_secrets.extend(out.harvested);
                        if let Some(b) = blocks.get_mut(first).filter(|_| !out.notes.is_empty()) {
                            b.utf8_note = Some(out.notes.join("; "));
                        }
                    }
                    Err(reason) => {
                        let kind = if reason.starts_with("binary") {
                            SkipKind::Binary
                        } else if reason.starts_with("empty") {
                            SkipKind::Excluded
                        } else {
                            SkipKind::NotCaptured
                        };
                        merge_skips.push(SkipEntry::new(rel, reason, kind));
                    }
                }
                let skip = if failed { merge_skips.last() } else { None };
                journal(
                    &mut ck,
                    &mut spool,
                    cancel_flag,
                    key(path),
                    stamp,
                    &blocks[first..],
                    skip,
                )?;
                return Ok(());
            }
            match p.result {
                Ok((mut b, content)) => {
                    if cfg.reproducible {
                        b.relative = nfc(&b.relative);
                    }
                    progress_callback(done, total_files, &b.relative);
                    match p.wait {
                        // Its place in the order is kept; the text comes after
                        // the OCR stage.
                        Some(wait) => {
                            blocks.push(b);
                            waiting.push((blocks.len() - 1, wait));
                            if let Some(w) = watch {
                                ocr_still(w);
                            }
                        }
                        None => {
                            b.span = spool.append(&content)?;
                            blocks.push(b);
                            let last = blocks.len() - 1;
                            journal(
                                &mut ck,
                                &mut spool,
                                cancel_flag,
                                key(path),
                                stamp,
                                &blocks[last..],
                                None,
                            )?;
                        }
                    }
                }
                Err((rel, reason, kind)) => {
                    let rel = if cfg.reproducible { nfc(&rel) } else { rel };
                    progress_callback(done, total_files, &rel);
                    merge_skips.push(SkipEntry::new(rel, reason, kind));
                    journal(
                        &mut ck,
                        &mut spool,
                        cancel_flag,
                        key(path),
                        stamp,
                        &[],
                        merge_skips.last(),
                    )?;
                }
            }
            Ok(())
        },
    )?;
    if ended == Ended::Cancelled || cancelled(cancel_flag) {
        // What is done stays done: the next --resume starts after it.
        if let Some(c) = ck.as_mut() {
            let _ = c.flush(&mut spool);
        }
        return Err(Cancelled.into());
    }
    if let Some(c) = ck.as_mut() {
        c.flush(&mut spool)?;
    }

    // The OCR stage: every picture the text merge queued, a batch per engine
    // start; then the documents that waited get their text, in their place.
    if let (Some(queue), false) = (&ocr, waiting.is_empty()) {
        let mut report = |p: crate::extract::OcrProgress| {
            if let Some(on_ocr) = ctl.on_ocr {
                on_ocr(p.done, p.total, &p.current);
            }
        };
        if queue.run(cancel_flag, ctl.finish, &mut report).is_err() {
            return Err(Cancelled.into());
        }
        if let Some(w) = watch {
            w.ocr_secs.store(0, AtomicOrd::Relaxed);
        }
        let mut gone = vec![false; blocks.len()];
        for (bi, wait) in waiting.drain(..) {
            let relative = blocks[bi].relative.clone();
            let (result, harvested) = process::finish_waiting(relative, wait, queue, cfg);
            known_secrets.extend(harvested);
            match result {
                Ok((mut b, content)) => {
                    b.span = spool.append(&content)?;
                    blocks[bi] = b;
                }
                Err((rel, reason, kind)) => {
                    if reason.contains(crate::extract::OCR_FINISHED_EARLY) {
                        partial = true;
                    }
                    gone[bi] = true;
                    merge_skips.push(SkipEntry::new(rel, reason, kind));
                }
            }
        }
        if gone.contains(&true) {
            let mut k = 0;
            blocks.retain(|_| {
                k += 1;
                !gone[k - 1]
            });
        }
    }

    // Harvest secrets from credential files the merge never includes —
    // `.env`, `MASTER_CREDENTIALS_*.md`, credential stores — INCLUDING
    // gitignored ones (they almost always are, so the scan never yields
    // them). Their values echo in prose elsewhere (session notes,
    // changelogs); harvesting lets the propagation pass scrub those echoes.
    // Harvest-only: content is read, secrets extracted, content dropped —
    // the credential file itself is never merged.
    if cfg.redact {
        harvest_from_credential_files(root, &mut known_secrets, &mut dense_candidates);
    }
    if cancelled(cancel_flag) {
        return Err(Cancelled.into());
    }

    // Git context rides at the very end (LLMs weight the end of context;
    // "review my change" wants the diff last).
    if cfg.git_diff || cfg.git_log > 0 {
        let included: HashSet<String> = blocks.iter().map(|b| b.relative.clone()).collect();
        for (mut b, content) in git::git_context_blocks(root, cfg, &included, &mut merge_skips) {
            b.span = spool.append(&content)?;
            blocks.push(b);
        }
    }
    spool.flush()?;

    if cfg.redact {
        mask_known_values(
            &mut blocks,
            &mut spool,
            known_secrets,
            dense_candidates,
            cancel_flag,
        )?;
    }
    if cancelled(cancel_flag) {
        return Err(Cancelled.into());
    }

    // Aggregate stats.
    let (figures_kept, figures_dir) = ocr.as_ref().map_or((0, None), |q| {
        (q.figures_kept(), q.figures_dir().map(Path::to_path_buf))
    });
    let mut outcome = MergeOutcome {
        files_skipped: merge_skips.len(),
        partial,
        figures_kept,
        figures_dir,
        ..Default::default()
    };
    for b in &blocks {
        outcome.total_bytes += b.span.len as usize;
        outcome.tokens_o200k += b.tokens;
        outcome.files_processed += 1;
        if b.compressed {
            outcome.files_compressed += 1;
        }
        for (_, n) in &b.redactions {
            outcome.secrets_redacted += n;
        }
    }
    if cfg.emit_skill && cfg.remote {
        merge_skips.push(SkipEntry::new(
            "SKILL.md",
            "--emit-skill ignored for a remote source (the clone is temporary)",
            SkipKind::Excluded,
        ));
    }
    outcome.skipped = merge_skips.clone();

    // Archive twin (`--complete`): every source file is read once before
    // anything is written, so the report and the exit code can name each one
    // that cannot come back byte for byte.
    let mut archive = if cfg.complete {
        let rel = |p: &Path| {
            let r = relative_display(root, p);
            if cfg.reproducible {
                nfc(&r)
            } else {
                r
            }
        };
        let sources: Vec<(PathBuf, String, bool)> = ordered
            .iter()
            .enumerate()
            .map(|(i, p)| ((*p).clone(), rel(p), !unprocessed[i]))
            .chain(cfg.archive_also.iter().map(|p| (p.clone(), rel(p), true)))
            .collect();
        render::archive_plan(&sources, &blocks, &spool, cfg, cancel_flag)?
    } else {
        Vec::new()
    };
    outcome.archive_gaps = archive
        .iter()
        .filter_map(|e| e.gap().map(|why| (e.relative.clone(), why)))
        .collect();

    let all_skips: Vec<&SkipEntry> = scan_skips.iter().chain(merge_skips.iter()).collect();
    let folder = render::display_path(
        root.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("Project"),
    )
    .into_owned();
    let source_label = if cfg.show_source_path {
        root.display().to_string()
    } else {
        cfg.source_label
            .clone()
            .unwrap_or_else(|| format!("local folder \"{}\"", folder))
    };

    // --max-tokens is a contract (HANDOFF_S8 §6.11 C): a file larger than one
    // part is split into pieces that each fit a later part — exact slices of
    // its text, in order, named `(piece k/n)`.
    let mut split_files = 0;
    if let Some(budget) = cfg.max_tokens.filter(|&b| b > 0) {
        let room = {
            let refs: Vec<&Block> = blocks.iter().collect();
            let probe = RenderCtx {
                cfg,
                root,
                folder: folder.clone(),
                source_label: source_label.clone(),
                nonce: String::new(),
                all: &refs,
                part_of: vec![0; refs.len()],
                idx: 1,
                n_parts: 10,
                total_scanned: total_files,
                outcome: &outcome,
                skips: &all_skips,
                spool: None,
                archive: &archive,
                archive_set: String::new(),
            };
            render::part_room(&probe, budget)
        };
        let (first_of, n) = split::split_blocks(&mut blocks, &spool, |b| {
            room.saturating_sub(render::block_overhead_tokens(b, cfg.format) + split::LABEL_TOKENS)
        })?;
        for e in &mut archive {
            e.remap(&first_of);
        }
        split_files = n;
    }
    let all_blocks: Vec<&Block> = blocks.iter().collect();
    // Only the plain format prints the nonce; it costs a pass over the spool.
    let nonce = if cfg.format == OutputFormat::Plain {
        delimiter_nonce(&blocks, &spool)?
    } else {
        String::new()
    };
    let mut ctx = RenderCtx {
        cfg,
        root,
        folder,
        source_label,
        nonce,
        all: &all_blocks,
        part_of: Vec::new(),
        idx: 0,
        n_parts: 1,
        total_scanned: total_files,
        outcome: &outcome,
        skips: &all_skips,
        spool: Some(&spool),
        archive: &archive,
        archive_set: render::archive_set_id(&archive),
    };

    // Partition into parts by token budget, counting every token each part
    // really carries: headers, the part-1 tree/contents/report, and the
    // per-file wrappers (N-12).
    let parts = render::partition(&ctx, cfg.max_tokens);
    let n_parts = parts.len().max(1);
    let mut part_of = vec![0usize; blocks.len()];
    for (pi, part) in parts.iter().enumerate() {
        for &bi in part {
            part_of[bi] = pi;
        }
    }
    ctx.part_of = part_of;
    ctx.n_parts = n_parts;

    // Every part goes to a temp file first; they are renamed into place only
    // once all of them are written, so a cancel or an error leaves no part.
    let mut staged: Vec<(PathBuf, PathBuf)> = Vec::new();
    let result = (|| -> Result<Vec<String>> {
        let mut notes = Vec::new();
        for (idx, part) in parts.iter().enumerate() {
            if cancelled(cancel_flag) {
                return Err(Cancelled.into());
            }
            ctx.idx = idx;
            let path = render::part_path(output, idx, n_parts, cfg.format);
            let tmp = temp_path_for(&path);
            staged.push((tmp.clone(), path));
            let part_blocks: Vec<&Block> = part.iter().map(|&i| &blocks[i]).collect();
            let file = File::create(&tmp)?;
            let mut w = BufWriter::with_capacity(256 * 1024, file);
            render::render_part(&mut w, &ctx, &part_blocks)?;
            w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
            if let Some(budget) = cfg.max_tokens.filter(|_| n_parts > 1) {
                let t = tokens::count_file(&tmp)?;
                if t > budget {
                    notes.push(format!(
                        "part {} is {} tokens, over the {}-token budget: a single file is larger than one part",
                        idx + 1,
                        t,
                        budget
                    ));
                }
            }
        }
        Ok(notes)
    })();
    let notes = match result {
        Ok(notes) => notes,
        Err(e) => {
            for (tmp, _) in &staged {
                let _ = std::fs::remove_file(tmp);
            }
            return Err(e);
        }
    };
    let mut outputs = Vec::new();
    for (tmp, path) in staged {
        if let Err(e) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        outputs.push(path);
    }
    drop(ctx);
    outcome.outputs = outputs;
    outcome.notes = notes;
    if !outcome.archive_gaps.is_empty() {
        outcome.notes.push(format!(
            "archive twin: {} of {} source files cannot be restored byte for byte (named under \"Archive twin\" in the Merge Report)",
            outcome.archive_gaps.len(),
            archive.len()
        ));
    }
    if split_files > 0 {
        outcome.notes.push(format!(
            "{split_files} file(s) larger than one --max-tokens part were split into pieces named \"(piece k/n)\"; joined in order they are the file's text"
        ));
    }
    if cfg.complete && n_parts > 1 {
        outcome.notes.push(
            "archive twin: each part carries the manifest and embedded bytes of its own files; --max-tokens does not count them".into(),
        );
    }
    if let (true, Some(dir)) = (outcome.figures_kept > 0, &outcome.figures_dir) {
        outcome.notes.push(format!(
            "{} figures kept in {}",
            outcome.figures_kept,
            dir.display()
        ));
    }

    // The outputs are in place: the checkpoint has done its job — unless the
    // run finished early, when it is what the next --resume continues from.
    if let Some(c) = ck {
        if c.failed_check > 0 {
            outcome.notes.push(format!(
                "checkpoint: {} journaled file(s) no longer matched the spool (power loss?) and were read again",
                c.failed_check
            ));
        }
        if c.resumed > 0 {
            outcome.notes.push(format!(
                "resumed: {} of {} files came from the checkpoint, the rest were read",
                c.resumed, total_files
            ));
        }
        if outcome.partial {
            outcome.notes.push(format!(
                "checkpoint kept in {}: run the same command with --resume to finish",
                c.dir().display()
            ));
        } else {
            c.remove(spool);
        }
    }

    // Claude-skill emission (T3-4): best-effort, into the scanned repo.
    if cfg.emit_skill && !cfg.remote {
        match write_skill(root, &blocks, &outcome, cfg.reproducible) {
            Ok(p) => outcome.skill = Some(p),
            Err(e) => eprintln!("skill generation failed: {}", e),
        }
    }

    Ok(outcome)
}

/// Journal one finished file in the checkpoint (`--checkpoint`/`--resume`).
/// Nothing is journaled once the run is cancelled: a worker that saw the
/// cancel may hand over an empty result.
fn journal(
    ck: &mut Option<checkpoint::Checkpoint>,
    spool: &mut Spool,
    cancel: &AtomicBool,
    key: String,
    stamp: Option<checkpoint::Stamp>,
    blocks: &[Block],
    skip: Option<&SkipEntry>,
) -> std::io::Result<()> {
    if let (Some(c), Some(stamp), false) = (ck.as_mut(), stamp, cancelled(cancel)) {
        if let Some(e) = checkpoint::Entry::new(key, stamp, blocks, skip, spool)? {
            c.record(&e, spool)?;
        }
    }
    Ok(())
}

/// Blocks in consecutive runs whose texts total at most `CHUNK_BYTES`.
fn block_chunks(blocks: &[Block]) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut bytes = 0u64;
    for (i, b) in blocks.iter().enumerate() {
        if i > start && (i - start >= CHUNK_FILES || bytes + b.span.len > CHUNK_BYTES) {
            ranges.push(start..i);
            start = i;
            bytes = 0;
        }
        bytes += b.span.len;
    }
    if start < blocks.len() {
        ranges.push(start..blocks.len());
    }
    ranges
}

/// `<dir>/.<name>.<pid>.tmp` beside `path`, for write-then-rename (N-45).
fn temp_path_for(path: &Path) -> PathBuf {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "merged".into());
    dir.join(format!(".{}.{}.tmp", name, std::process::id()))
}

/// Repo-wide known-value masking (N-13/N-14/N-35).
///
/// A value LABELED a secret anywhere (even in an excluded credential file) is
/// masked in every block — prose that echoes a password without a label
/// leaks it otherwise. Unlabeled tokens from credential dumps first pass a
/// frequency guard: a real secret echoes in a couple of places; a token in
/// many blocks is a name or host and must not be masked (it would shred the
/// merge). Both passes are one Aho-Corasick automaton over a sorted set,
/// whole-token matches only, parallel over blocks — deterministic and linear
/// in the corpus. Token counts are recomputed for touched blocks, whose new
/// text is appended to the spool.
fn mask_known_values(
    blocks: &mut [Block],
    spool: &mut Spool,
    known: BTreeSet<String>,
    dense: BTreeSet<String>,
    cancel_flag: &AtomicBool,
) -> Result<()> {
    use crate::security::masking::KnownValues;

    let chunks = block_chunks(blocks);
    let dense: Vec<String> = dense.difference(&known).cloned().collect();
    let mut values = known;
    if !dense.is_empty() {
        let guard = KnownValues::new(dense.iter().cloned());
        let mut hits = vec![0usize; guard.len()];
        for range in &chunks {
            if cancelled(cancel_flag) {
                return Err(Cancelled.into());
            }
            let per_block: Vec<BTreeSet<usize>> = crate::threads::on_pool(|| {
                blocks[range.clone()]
                    .par_iter()
                    .map(|b| spool.read(b.span).map(|text| guard.present(&text)))
                    .collect::<std::io::Result<_>>()
            })?;
            for present in &per_block {
                for &i in present {
                    hits[i] += 1;
                }
            }
        }
        // `dense` is sorted, and pattern indices follow the sorted order.
        values.extend(
            dense
                .into_iter()
                .zip(hits)
                .filter(|(_, n)| (1..=DENSE_TOKEN_MAX_BLOCKS).contains(n))
                .map(|(t, _)| t),
        );
    }
    if values.is_empty() || cancelled(cancel_flag) {
        return Ok(());
    }
    let masker = KnownValues::new(values);
    for range in chunks {
        if cancelled(cancel_flag) {
            return Err(Cancelled.into());
        }
        let masked: Vec<Option<(String, usize, usize)>> = crate::threads::on_pool(|| {
            blocks[range.clone()]
                .par_iter()
                .map(|b| {
                    spool.read(b.span).map(|text| {
                        masker.mask(&text, "[REDACTED]").map(|(m, n)| {
                            let t = crate::cache::count(&m);
                            (m, n, t)
                        })
                    })
                })
                .collect::<std::io::Result<_>>()
        })?;
        for (b, m) in blocks[range].iter_mut().zip(masked) {
            if let Some((text, n, t)) = m {
                b.span = spool.append(&text)?;
                b.redactions.push(("Propagated known secret", n));
                b.tokens = t;
            }
        }
    }
    spool.flush()?;
    Ok(())
}

/// Read credential DOCUMENT files (credential stores, `.env`,
/// `*_CREDENTIALS_*.md`) — found regardless of gitignore — purely to harvest
/// their secret values for repo-wide propagation. The files themselves are
/// never merged. Key/cert/SSH material is skipped (a private-key body doesn't
/// echo as prose). Bounded per file inside `find_credential_files`.
fn harvest_from_credential_files(
    root: &Path,
    known_secrets: &mut BTreeSet<String>,
    dense_candidates: &mut BTreeSet<String>,
) {
    for path in crate::scanner::find_credential_files(root) {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let content = String::from_utf8_lossy(&bytes);
        known_secrets.extend(crate::security::harvest_labeled_values(&content));
        dense_candidates.extend(crate::security::harvest_dense_file_tokens(&content));
    }
}

/// Write `.claude/skills/<repo>/SKILL.md` describing the merged snapshot and
/// how to regenerate it. Overwrites on re-merge (watch mode included).
fn write_skill(
    root: &Path,
    blocks: &[Block],
    outcome: &MergeOutcome,
    reproducible: bool,
) -> Result<PathBuf> {
    let repo = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");
    let slug: String = repo
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    let slug = if slug.is_empty() {
        "project".into()
    } else {
        slug
    };

    let dir = root
        .join(".claude")
        .join("skills")
        .join(crate::security::sanitize_filename(repo));
    std::fs::create_dir_all(&dir)?;

    let refs: Vec<&Block> = blocks.iter().collect();
    // The repo name lands in YAML frontmatter and the tree in a fence:
    // neither may carry a newline or a fence-closing run (N-20).
    let repo = render::display_path(repo).replace(':', "-");
    let repo = repo.as_str();
    let tree = render::generate_tree(repo, &refs);
    let fence = render::fence_for(&tree);
    let outputs = outcome
        .outputs
        .iter()
        .map(|p| format!("- `{}`", p.display()))
        .collect::<Vec<_>>()
        .join("\n");

    let content = format!(
        "---\nname: {slug}\ndescription: Repo context for {repo}. Use when working on {repo} code — points at the TurboMerger merged snapshot and how to regenerate or map it.\n---\n\n\
# {repo} — TurboMerger context\n\n\
Merged snapshot ({files} files, ~{tokens} o200k tokens, generated {when}by TurboMerger v{version}):\n\n{outputs}\n\n\
Regenerate: `turbomerger merge \"{root}\"` (flags: `--compress` for signatures-only, `--git-diff` for the working-tree diff).\n\
Structural overview instead of full content: `turbomerger map \"{root}\" --tokens 1024`.\n\n\
## Project structure\n\n{fence}\n{tree}{fence}\n",
        fence = fence,
        slug = slug,
        repo = repo,
        files = outcome.files_processed,
        tokens = outcome.tokens_o200k,
        when = if reproducible {
            String::new()
        } else {
            format!("{} ", chrono::Utc::now().format("%Y-%m-%dT%H:%MZ"))
        },
        version = env!("CARGO_PKG_VERSION"),
        outputs = outputs,
        root = root.display(),
        tree = tree,
    );
    let path = dir.join("SKILL.md");
    std::fs::write(&path, content)?;
    Ok(path)
}

// ============================================================================
// HELPERS
// ============================================================================

/// Unicode NFC — for `--reproducible` display paths only (never for I/O).
fn nfc(s: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    s.nfc().collect()
}

fn relative_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.to_string_lossy().to_string())
}

/// Deterministic 8-hex tag derived from every block (FNV-1a 64): content
/// cannot contain the delimiter that its own bytes determine, and reruns on
/// the same input stay byte-identical.
fn delimiter_nonce(blocks: &[Block], spool: &Spool) -> Result<String> {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for &byte in bytes {
            h ^= byte as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for b in blocks {
        feed(b.relative.as_bytes());
        feed(&[0]);
        feed(spool.read(b.span)?.as_bytes());
        feed(&[0]);
    }
    Ok(format!("{:08x}", (h >> 32) as u32 ^ h as u32))
}

/// Rank for entry-first ordering: lower = more important (shown earlier).
fn entry_rank(rel: &str) -> i32 {
    let lower = rel.to_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    let depth = lower.matches('/').count() as i32;
    let mut score = depth * 2;
    if name.starts_with("readme") {
        score -= 100;
    }
    if matches!(
        name,
        "main.rs"
            | "lib.rs"
            | "mod.rs"
            | "main.py"
            | "__init__.py"
            | "index.ts"
            | "index.js"
            | "index.tsx"
            | "app.tsx"
            | "cargo.toml"
            | "package.json"
            | "pyproject.toml"
            | "go.mod"
            | "makefile"
            | "dockerfile"
    ) {
        score -= 40;
    }
    if lower.starts_with("src/") {
        score -= 10;
    }
    if lower.starts_with("tests/") || lower.starts_with("test/") || lower.contains("/tests/") {
        score += 20;
    }
    if lower.starts_with("docs/") || lower.starts_with("doc/") {
        score += 15;
    }
    score
}

fn order_files(root: &Path, files: &mut [&PathBuf], ordering: Ordering, reproducible: bool) {
    if ordering == Ordering::Plan {
        return;
    }
    if reproducible {
        // The same order on every OS: NFC path strings, not OS path bytes.
        let key = |p: &PathBuf| nfc(&relative_display(root, p));
        match ordering {
            Ordering::Path => files.sort_by_cached_key(|p| key(p)),
            Ordering::EntryFirst => files.sort_by_cached_key(|p| {
                let k = key(p);
                (entry_rank(&k), k)
            }),
            Ordering::ImportantLast => {
                files.sort_by_cached_key(|p| {
                    let k = key(p);
                    (std::cmp::Reverse(entry_rank(&k)), std::cmp::Reverse(k))
                });
            }
            Ordering::Plan => {}
        }
        return;
    }
    match ordering {
        Ordering::Path => files.sort(),
        Ordering::EntryFirst => {
            files.sort_by(|a, b| {
                let ra = entry_rank(&relative_display(root, a));
                let rb = entry_rank(&relative_display(root, b));
                ra.cmp(&rb).then_with(|| a.cmp(b))
            });
        }
        Ordering::ImportantLast => {
            files.sort_by(|a, b| {
                let ra = entry_rank(&relative_display(root, a));
                let rb = entry_rank(&relative_display(root, b));
                rb.cmp(&ra).then_with(|| b.cmp(a))
            });
        }
        Ordering::Plan => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_first_puts_readme_and_main_early() {
        assert!(entry_rank("README.md") < entry_rank("src/util.rs"));
        assert!(entry_rank("src/main.rs") < entry_rank("tests/foo.rs"));
        assert!(entry_rank("src/a.rs") < entry_rank("docs/guide.md"));
    }

    #[test]
    fn format_and_ordering_parse() {
        assert_eq!(OutputFormat::from_str_lenient("CXML"), OutputFormat::Cxml);
        assert_eq!(
            OutputFormat::from_str_lenient("weird"),
            OutputFormat::Markdown
        );
        assert_eq!(OutputFormat::Json.extension(), "json");
        assert_eq!(
            Ordering::from_str_lenient("important-last"),
            Ordering::ImportantLast
        );
    }

    #[test]
    fn cancel_before_writing_leaves_no_output() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("r");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        let out = dir.path().join("out.md");
        let cancel = AtomicBool::new(true);
        let err = merge_files_with_progress(
            &root,
            &[root.join("a.rs")],
            &out,
            &MergeConfig::default(),
            &cancel,
            |_, _, _| {},
            &[],
        )
        .unwrap_err();
        assert!(err.is::<Cancelled>());
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(left, vec![std::ffi::OsString::from("r")], "{left:?}");
    }
}
