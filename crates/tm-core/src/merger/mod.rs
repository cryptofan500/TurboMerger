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

mod git;
mod process;
mod render;
mod spool;

use std::collections::{BTreeSet, HashSet};
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrd};

use anyhow::Result;
use rayon::prelude::*;

use crate::scanner::{SkipEntry, SkipKind};
use crate::tokens;
use render::RenderCtx;
use spool::{Span, Spool};

/// At most this many files, or this many bytes on disk, are processed (and
/// held) at once.
const CHUNK_FILES: usize = 64;
const CHUNK_BYTES: u64 = 64 << 20;

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
}

impl Ordering {
    pub fn from_str_lenient(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "entry-first" | "entryfirst" | "entry" => Ordering::EntryFirst,
            "important-last" | "importantlast" => Ordering::ImportantLast,
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
}

/// Runtime control of one merge.
#[derive(Clone, Copy)]
pub struct MergeCtl<'a> {
    /// Stop and write nothing (`Err(Cancelled)`).
    pub cancel: &'a AtomicBool,
    /// Stop taking new files, then write what is done.
    pub finish: Option<&'a AtomicBool>,
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

    let mut spool = Spool::new_near(output)?;
    let mut blocks: Vec<Block> = Vec::with_capacity(total_files);
    let mut merge_skips: Vec<SkipEntry> = Vec::new();
    let mut done = 0usize;

    // Labeled secret values harvested from every read file — INCLUDING
    // credential-dense files that are then excluded — for repo-wide
    // propagation after processing. Dense-file opaque tokens ride separately
    // (they face a frequency guard first).
    // Sorted sets: every later step iterates them, and the output must be
    // byte-identical across runs (N-14).
    let mut known_secrets: BTreeSet<String> = BTreeSet::new();
    let mut dense_candidates: BTreeSet<String> = BTreeSet::new();

    // Process in parallel, a bounded chunk at a time; each chunk's texts go
    // to the spool in order before the next chunk is read.
    let mut partial = false;
    for chunk in chunk_by_size(&ordered) {
        if cancelled(cancel_flag) {
            return Err(Cancelled.into());
        }
        if partial || finishing() {
            // Finishing early: what is left is reported, never dropped.
            partial = true;
            for path in chunk {
                done += 1;
                let rel = relative_display(root, path);
                progress_callback(done, total_files, &rel);
                merge_skips.push(SkipEntry::new(
                    rel,
                    "not processed — the run was finished early (deadline)",
                    SkipKind::NotCaptured,
                ));
            }
            continue;
        }
        // Say which chunk is in flight: a stall report can then name a file.
        if let Some(first) = chunk.first() {
            progress_callback(done, total_files, &relative_display(root, first));
        }
        let processed: Vec<process::Processed> = chunk
            .par_iter()
            .map(|path| {
                if cancelled(cancel_flag) {
                    process::Processed {
                        result: Err((String::new(), String::new(), SkipKind::Excluded)),
                        harvested: Vec::new(),
                        dense: Vec::new(),
                    }
                } else {
                    process::process_file(path, root, cfg)
                }
            })
            .collect();
        if cancelled(cancel_flag) {
            return Err(Cancelled.into());
        }
        for p in processed {
            known_secrets.extend(p.harvested);
            dense_candidates.extend(p.dense);
            done += 1;
            match p.result {
                Ok((mut b, content)) => {
                    if cfg.reproducible {
                        b.relative = nfc(&b.relative);
                    }
                    progress_callback(done, total_files, &b.relative);
                    b.span = spool.append(&content)?;
                    blocks.push(b);
                }
                Err((rel, reason, kind)) => {
                    let rel = if cfg.reproducible { nfc(&rel) } else { rel };
                    progress_callback(done, total_files, &rel);
                    merge_skips.push(SkipEntry::new(rel, reason, kind));
                }
            }
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
    let mut outcome = MergeOutcome {
        files_skipped: merge_skips.len(),
        partial,
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

    let all_skips: Vec<&SkipEntry> = scan_skips.iter().chain(merge_skips.iter()).collect();
    let all_blocks: Vec<&Block> = blocks.iter().collect();
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
    // Only the plain format prints the nonce; it costs a pass over the spool.
    let nonce = if cfg.format == OutputFormat::Plain {
        delimiter_nonce(&blocks, &spool)?
    } else {
        String::new()
    };
    let mut ctx = RenderCtx {
        cfg,
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

    // Claude-skill emission (T3-4): best-effort, into the scanned repo.
    if cfg.emit_skill && !cfg.remote {
        match write_skill(root, &blocks, &outcome, cfg.reproducible) {
            Ok(p) => outcome.skill = Some(p),
            Err(e) => eprintln!("skill generation failed: {}", e),
        }
    }

    Ok(outcome)
}

/// Consecutive runs of at most `CHUNK_FILES` files and `CHUNK_BYTES` bytes
/// (a single larger file is a chunk of its own).
fn chunk_by_size<'a>(files: &[&'a PathBuf]) -> Vec<Vec<&'a PathBuf>> {
    let mut chunks = Vec::new();
    let mut cur: Vec<&PathBuf> = Vec::new();
    let mut bytes = 0u64;
    for &f in files {
        let len = std::fs::metadata(f).map(|m| m.len()).unwrap_or(0);
        if !cur.is_empty() && (cur.len() >= CHUNK_FILES || bytes + len > CHUNK_BYTES) {
            chunks.push(std::mem::take(&mut cur));
            bytes = 0;
        }
        cur.push(f);
        bytes += len;
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    chunks
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
            let per_block: Vec<BTreeSet<usize>> = blocks[range.clone()]
                .par_iter()
                .map(|b| spool.read(b.span).map(|text| guard.present(&text)))
                .collect::<std::io::Result<_>>()?;
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
        let masked: Vec<Option<(String, usize, usize)>> = blocks[range.clone()]
            .par_iter()
            .map(|b| {
                spool.read(b.span).map(|text| {
                    masker.mask(&text, "[REDACTED]").map(|(m, n)| {
                        let t = tokens::count(&m);
                        (m, n, t)
                    })
                })
            })
            .collect::<std::io::Result<_>>()?;
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
    fn chunks_respect_file_and_byte_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let mut paths = Vec::new();
        for i in 0..150 {
            let p = dir.path().join(format!("f{i:03}.txt"));
            std::fs::write(&p, "x").unwrap();
            paths.push(p);
        }
        let refs: Vec<&PathBuf> = paths.iter().collect();
        let sizes: Vec<usize> = chunk_by_size(&refs).iter().map(Vec::len).collect();
        assert_eq!(sizes, vec![64, 64, 22]);
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
