//! Merger: reads scanned files (parallel), redacts secrets, optionally slims
//! content, counts tokens, orders files, and writes one or more output files in
//! the chosen format (Markdown / XML / Claude-XML / JSON / Plain), splitting by
//! a token budget when asked.
//!
//! Design note: to support ordering, token-budget splitting, and multi-format
//! rendering the merger holds the processed file blocks in memory (bounded by
//! the merged-output size, which is the paste-to-chat use case). Reads are still
//! chunked + parallel so peak memory tracks output size, not 2x it.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrd};
use std::sync::LazyLock;

use anyhow::Result;
use rayon::prelude::*;
use regex::Regex;

use crate::scanner::SkipEntry;
use crate::tokens;

const CHUNK: usize = 64;
const MANIFEST_MAX: usize = 1000;

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
}

struct Block {
    relative: String,
    content: String,
    lang: String,
    tokens: usize,
    utf8_note: Option<String>,
    redactions: Vec<(&'static str, usize)>,
    compressed: bool,
    /// Git-context sections and similar: real output, but not part of the
    /// scanned file set, so the project tree must not list them.
    synthetic: bool,
}

/// (relative_path, reason) for a file dropped at merge time
type MergeSkip = (String, String);

static BASE64_RUN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z0-9+/]{200,}={0,2}").expect("base64 run regex"));

/// Top-level entry point. Returns the outcome (including all output paths).
#[allow(clippy::too_many_arguments)]
pub fn merge_files_with_progress<F>(
    root: &Path,
    files: &[PathBuf],
    output: &Path,
    cfg: &MergeConfig,
    cancel_flag: &AtomicBool,
    mut progress_callback: F,
    scan_skips: &[SkipEntry],
) -> Result<MergeOutcome>
where
    F: FnMut(usize, usize, &str),
{
    let total_files = files.len();

    // Order files (a stable copy so the caller's slice is untouched).
    let mut ordered: Vec<&PathBuf> = files.iter().collect();
    order_files(root, &mut ordered, cfg.ordering);

    // Process (read/decode/slim/redact/count) in parallel, chunked.
    let mut blocks: Vec<Block> = Vec::with_capacity(total_files);
    let mut merge_skips: Vec<SkipEntry> = Vec::new();
    let mut done = 0usize;

    'outer: for chunk in ordered.chunks(CHUNK) {
        if cancel_flag.load(AtomicOrd::Relaxed) {
            break;
        }
        let mut processed: Vec<(usize, std::result::Result<Block, MergeSkip>)> = chunk
            .par_iter()
            .enumerate()
            .map(|(i, path)| (i, process_file(path, root, cfg)))
            .collect();
        processed.sort_by_key(|(i, _)| *i);

        for (_, res) in processed {
            if cancel_flag.load(AtomicOrd::Relaxed) {
                break 'outer;
            }
            done += 1;
            match res {
                Ok(b) => {
                    progress_callback(done, total_files, &b.relative);
                    blocks.push(b);
                }
                Err((rel, reason)) => {
                    progress_callback(done, total_files, &rel);
                    merge_skips.push(SkipEntry { path: rel, reason });
                }
            }
        }
    }

    // Git context rides at the very end (LLMs weight the end of context;
    // "review my change" wants the diff last).
    if !cancel_flag.load(AtomicOrd::Relaxed) && (cfg.git_diff || cfg.git_log > 0) {
        blocks.extend(git_context_blocks(root, cfg, &mut merge_skips));
    }

    // Aggregate stats.
    let mut outcome = MergeOutcome {
        files_skipped: merge_skips.len(),
        ..Default::default()
    };
    for b in &blocks {
        outcome.total_bytes += b.content.len();
        outcome.tokens_o200k += b.tokens;
        outcome.files_processed += 1;
        if b.compressed {
            outcome.files_compressed += 1;
        }
        for (_, n) in &b.redactions {
            outcome.secrets_redacted += n;
        }
    }

    // Partition into parts by token budget (single part if no budget / fits).
    let parts = partition(&blocks, cfg.max_tokens);
    let n_parts = parts.len().max(1);

    let all_skips: Vec<&SkipEntry> = scan_skips.iter().chain(merge_skips.iter()).collect();

    for (idx, part) in parts.iter().enumerate() {
        let path = part_path(output, idx, n_parts, cfg.format);
        let part_blocks: Vec<&Block> = part.iter().map(|&i| &blocks[i]).collect();
        write_part(
            &path,
            root,
            &part_blocks,
            cfg,
            idx,
            n_parts,
            total_files,
            &outcome,
            &all_skips,
        )?;
        outcome.outputs.push(path);
    }

    // Claude-skill emission (T3-4): best-effort, into the scanned repo.
    if cfg.emit_skill && !cancel_flag.load(AtomicOrd::Relaxed) {
        match write_skill(root, &blocks, &outcome) {
            Ok(p) => outcome.skill = Some(p),
            Err(e) => eprintln!("skill generation failed: {}", e),
        }
    }

    Ok(outcome)
}

/// Write `.claude/skills/<repo>/SKILL.md` describing the merged snapshot and
/// how to regenerate it. Overwrites on re-merge (watch mode included).
fn write_skill(root: &Path, blocks: &[Block], outcome: &MergeOutcome) -> Result<PathBuf> {
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
    let slug = if slug.is_empty() { "project".into() } else { slug };

    let dir = root
        .join(".claude")
        .join("skills")
        .join(crate::security::sanitize_filename(repo));
    std::fs::create_dir_all(&dir)?;

    let refs: Vec<&Block> = blocks.iter().collect();
    let tree = generate_tree(root, &refs);
    let outputs = outcome
        .outputs
        .iter()
        .map(|p| format!("- `{}`", p.display()))
        .collect::<Vec<_>>()
        .join("\n");

    let content = format!(
        "---\nname: {slug}\ndescription: Repo context for {repo}. Use when working on {repo} code — points at the TurboMerger merged snapshot and how to regenerate or map it.\n---\n\n\
# {repo} — TurboMerger context\n\n\
Merged snapshot ({files} files, ~{tokens} o200k tokens, generated {when} by TurboMerger v{version}):\n\n{outputs}\n\n\
Regenerate: `turbomerger merge \"{root}\"` (flags: `--compress` for signatures-only, `--git-diff` for the working-tree diff).\n\
Structural overview instead of full content: `turbomerger map \"{root}\" --tokens 1024`.\n\n\
## Project structure\n\n```\n{tree}```\n",
        slug = slug,
        repo = repo,
        files = outcome.files_processed,
        tokens = outcome.tokens_o200k,
        when = chrono::Utc::now().format("%Y-%m-%dT%H:%MZ"),
        version = env!("CARGO_PKG_VERSION"),
        outputs = outputs,
        root = root.display(),
        tree = tree,
    );
    let path = dir.join("SKILL.md");
    std::fs::write(&path, content)?;
    Ok(path)
}

/// Greedy bin-packing of block indices into parts under `max_tokens`.
fn partition(blocks: &[Block], max_tokens: Option<usize>) -> Vec<Vec<usize>> {
    let total: usize = blocks.iter().map(|b| b.tokens).sum();
    match max_tokens {
        Some(budget) if budget > 0 && total > budget => {
            let mut parts: Vec<Vec<usize>> = Vec::new();
            let mut cur: Vec<usize> = Vec::new();
            let mut cur_tokens = 0usize;
            for (i, b) in blocks.iter().enumerate() {
                if !cur.is_empty() && cur_tokens + b.tokens > budget {
                    parts.push(std::mem::take(&mut cur));
                    cur_tokens = 0;
                }
                cur.push(i);
                cur_tokens += b.tokens;
            }
            if !cur.is_empty() {
                parts.push(cur);
            }
            parts
        }
        _ => vec![(0..blocks.len()).collect()],
    }
}

fn part_path(output: &Path, idx: usize, n_parts: usize, fmt: OutputFormat) -> PathBuf {
    if n_parts <= 1 {
        return output.to_path_buf();
    }
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    let stem = output
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("merged");
    parent.join(format!(
        "{}.part{}-of-{}.{}",
        stem,
        idx + 1,
        n_parts,
        fmt.extension()
    ))
}

/// Read + decode + slim + redact + token-count a single already-vetted file.
fn process_file(
    path: &Path,
    root: &Path,
    cfg: &MergeConfig,
) -> std::result::Result<Block, MergeSkip> {
    let relative = relative_display(root, path);

    let file = File::open(path).map_err(|e| (relative.clone(), format!("unreadable: {}", e)))?;
    let mut buffer = Vec::new();
    if BufReader::new(file).read_to_end(&mut buffer).is_err() {
        return Err((relative, "unreadable".into()));
    }

    // A UTF-16 BOM legitimizes the null bytes that the binary sniff would
    // otherwise reject, so BOM detection must run first.
    let bom = encoding_rs::Encoding::for_bom(&buffer);
    if bom.is_none() && !buffer.is_empty() {
        let check = 8192.min(buffer.len());
        if crate::security::is_binary_content(&buffer[..check]) {
            return Err((relative, "binary content".into()));
        }
    }

    let (mut content, utf8_note) = decode_text(buffer, bom);

    // Whole-file exclusion for credential-dense content (inline login tables,
    // Google app-passwords, key blocks) that per-line redaction can't fully
    // scrub. Checked on the raw decoded text, before any slimming/redaction.
    let cred_count = crate::security::credential_indicator_count(&content);
    if cred_count >= crate::security::CREDENTIAL_DENSITY_THRESHOLD {
        return Err((
            relative,
            format!(
                "credential-dense content ({} inline credentials) — excluded",
                cred_count
            ),
        ));
    }

    let lang = Path::new(&relative)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

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

    if cfg.truncate_base64 {
        content = BASE64_RUN
            .replace_all(&content, "[base64 omitted]")
            .into_owned();
    }
    if cfg.remove_empty_lines {
        let mut out = String::with_capacity(content.len());
        for line in content.lines() {
            if !line.trim().is_empty() {
                out.push_str(line);
                out.push('\n');
            }
        }
        content = out;
    }

    let redactions = if cfg.redact {
        let (clean, events) = crate::security::redact_secrets(&content);
        content = clean;
        events.into_iter().map(|ev| (ev.rule, ev.count)).collect()
    } else {
        Vec::new()
    };

    let tokens = tokens::count(&content);

    Ok(Block {
        relative,
        content,
        lang,
        tokens,
        utf8_note,
        redactions,
        compressed,
        synthetic: false,
    })
}

// ============================================================================
// GIT CONTEXT (T2-5)
// ============================================================================

/// Cap for the diff section so a giant rebase can't dwarf the codebase.
const GIT_DIFF_MAX_BYTES: usize = 512 * 1024;

fn run_git(root: &Path, args: &[&str]) -> std::result::Result<String, String> {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C").arg(root).args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = cmd.output().map_err(|e| format!("git not runnable: {}", e))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(err.lines().next().unwrap_or("git failed").to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Build the synthetic git-context blocks. Failures (not a repo, no git) are
/// reported as skip entries, never errors — git context is best-effort.
fn git_context_blocks(root: &Path, cfg: &MergeConfig, skips: &mut Vec<SkipEntry>) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut push = |relative: &str, lang: &str, mut content: String, cfg: &MergeConfig| {
        let redactions = if cfg.redact {
            let (clean, events) = crate::security::redact_secrets(&content);
            content = clean;
            events.into_iter().map(|ev| (ev.rule, ev.count)).collect()
        } else {
            Vec::new()
        };
        let tokens = tokens::count(&content);
        blocks.push(Block {
            relative: relative.to_string(),
            content,
            lang: lang.to_string(),
            tokens,
            utf8_note: None,
            redactions,
            compressed: false,
            synthetic: true,
        });
    };

    if cfg.git_diff {
        match run_git(root, &["diff", "HEAD"]) {
            Ok(diff) if diff.trim().is_empty() => skips.push(SkipEntry {
                path: "GIT DIFF".into(),
                reason: "working tree clean — no diff section".into(),
            }),
            Ok(mut diff) => {
                if diff.len() > GIT_DIFF_MAX_BYTES {
                    let mut cut = GIT_DIFF_MAX_BYTES;
                    while !diff.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    diff.truncate(cut);
                    diff.push_str("\n[... diff truncated at 512 KB ...]\n");
                }
                push("GIT DIFF (working tree vs HEAD)", "diff", diff, cfg);
            }
            Err(e) => skips.push(SkipEntry {
                path: "GIT DIFF".into(),
                reason: format!("git diff unavailable: {}", e),
            }),
        }
    }
    if cfg.git_log > 0 {
        let n = cfg.git_log.to_string();
        match run_git(
            root,
            &[
                "log",
                "-n",
                &n,
                "--pretty=format:%h %ad %an — %s",
                "--date=short",
            ],
        ) {
            Ok(log) if log.trim().is_empty() => skips.push(SkipEntry {
                path: "GIT LOG".into(),
                reason: "no commits".into(),
            }),
            Ok(log) => {
                let title = format!("GIT LOG (last {} commits)", cfg.git_log);
                push(&title, "", log, cfg);
            }
            Err(e) => skips.push(SkipEntry {
                path: "GIT LOG".into(),
                reason: format!("git log unavailable: {}", e),
            }),
        }
    }
    blocks
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
            let note = if had_errors {
                format!("decoded from {} (BOM) with replacement characters", enc.name())
            } else {
                format!("decoded from {} (BOM)", enc.name())
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

// ============================================================================
// WRITERS (one per part)
// ============================================================================

#[allow(clippy::too_many_arguments)]
fn write_part(
    path: &Path,
    root: &Path,
    blocks: &[&Block],
    cfg: &MergeConfig,
    part_idx: usize,
    n_parts: usize,
    total_scanned: usize,
    outcome: &MergeOutcome,
    skips: &[&SkipEntry],
) -> Result<()> {
    let mut w = BufWriter::with_capacity(256 * 1024, File::create(path)?);
    let folder = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("Project");
    let part_note = if n_parts > 1 {
        format!(
            " — Part {}/{} (wait for all {} parts before answering)",
            part_idx + 1,
            n_parts,
            n_parts
        )
    } else {
        String::new()
    };
    let first = part_idx == 0;

    match cfg.format {
        OutputFormat::Json => write_json(&mut w, folder, root, blocks, outcome, skips, &part_note)?,
        OutputFormat::Cxml => write_cxml(
            &mut w,
            folder,
            root,
            blocks,
            cfg,
            first,
            total_scanned,
            outcome,
            skips,
            &part_note,
        )?,
        OutputFormat::Xml => write_xml(
            &mut w,
            folder,
            root,
            blocks,
            cfg,
            first,
            total_scanned,
            outcome,
            skips,
            &part_note,
        )?,
        OutputFormat::Plain => write_plain(
            &mut w,
            folder,
            root,
            blocks,
            cfg,
            first,
            total_scanned,
            outcome,
            skips,
            &part_note,
        )?,
        OutputFormat::Markdown => write_markdown(
            &mut w,
            folder,
            root,
            blocks,
            cfg,
            first,
            total_scanned,
            outcome,
            skips,
            &part_note,
        )?,
    }
    w.flush()?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_markdown<W: Write>(
    w: &mut W,
    folder: &str,
    root: &Path,
    blocks: &[&Block],
    cfg: &MergeConfig,
    first: bool,
    total_scanned: usize,
    outcome: &MergeOutcome,
    skips: &[&SkipEntry],
    part_note: &str,
) -> Result<()> {
    writeln!(w, "# {} — Merged Codebase{}\n", folder, part_note)?;
    writeln!(
        w,
        "> Generated by TurboMerger v{}",
        env!("CARGO_PKG_VERSION")
    )?;
    writeln!(w, "> Files scanned: {}", total_scanned)?;
    writeln!(
        w,
        "> Estimated tokens: ~{} (o200k) · ~{} (Claude est.)",
        outcome.tokens_o200k,
        tokens::claude_estimate(outcome.tokens_o200k)
    )?;
    writeln!(w, "> Source: {}\n", root.display())?;
    writeln!(w, "---\n")?;

    if cfg.include_tree && first {
        writeln!(w, "## Project Structure\n")?;
        writeln!(w, "```")?;
        write!(w, "{}", generate_tree(root, blocks))?;
        writeln!(w, "```\n")?;
        writeln!(w, "## Contents\n")?;
        for b in blocks {
            writeln!(
                w,
                "- [{}](#{}) — ~{} tok",
                b.relative,
                anchor_for(&b.relative),
                b.tokens
            )?;
        }
        writeln!(w, "\n---\n")?;
    }

    for b in blocks {
        let fence = "`".repeat((longest_backtick_run(&b.content) + 1).max(3));
        writeln!(w, "## {}\n", b.relative)?;
        writeln!(w, "{}{}", fence, b.lang)?;
        w.write_all(b.content.as_bytes())?;
        if !b.content.ends_with('\n') {
            writeln!(w)?;
        }
        writeln!(w, "{}\n", fence)?;
    }

    write_markdown_report(w, blocks, outcome, skips)?;
    Ok(())
}

fn write_markdown_report<W: Write>(
    w: &mut W,
    blocks: &[&Block],
    outcome: &MergeOutcome,
    skips: &[&SkipEntry],
) -> Result<()> {
    writeln!(w, "---\n")?;
    writeln!(w, "## Merge Report\n")?;
    writeln!(w, "- Files merged: {}", outcome.files_processed)?;
    writeln!(
        w,
        "- Estimated tokens: ~{} (o200k) · ~{} (Claude est.)",
        outcome.tokens_o200k,
        tokens::claude_estimate(outcome.tokens_o200k)
    )?;
    if outcome.secrets_redacted > 0 {
        writeln!(w, "- Secrets redacted: {}", outcome.secrets_redacted)?;
    }
    if outcome.files_compressed > 0 {
        writeln!(
            w,
            "- Compressed to signatures (function bodies elided): {} files",
            outcome.files_compressed
        )?;
    }
    let redactions: Vec<(&str, &str, usize)> = blocks
        .iter()
        .flat_map(|b| {
            b.redactions
                .iter()
                .map(move |(r, n)| (b.relative.as_str(), *r, *n))
        })
        .collect();
    if !skips.is_empty() {
        writeln!(w, "\n### Skipped files ({})\n", skips.len())?;
        for e in skips.iter().take(MANIFEST_MAX) {
            writeln!(w, "- `{}` — {}", e.path, e.reason)?;
        }
        if skips.len() > MANIFEST_MAX {
            writeln!(w, "- …and {} more", skips.len() - MANIFEST_MAX)?;
        }
    }
    if !redactions.is_empty() {
        writeln!(w, "\n### Redactions\n")?;
        for (rel, rule, n) in &redactions {
            writeln!(w, "- `{}` — {} × {}", rel, n, rule)?;
        }
    }
    let notes: Vec<(&str, &str)> = blocks
        .iter()
        .filter_map(|b| b.utf8_note.as_deref().map(|n| (b.relative.as_str(), n)))
        .collect();
    if !notes.is_empty() {
        writeln!(w, "\n### Decoding notes\n")?;
        for (rel, note) in &notes {
            writeln!(w, "- `{}` — {}", rel, note)?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_plain<W: Write>(
    w: &mut W,
    folder: &str,
    root: &Path,
    blocks: &[&Block],
    cfg: &MergeConfig,
    first: bool,
    total_scanned: usize,
    outcome: &MergeOutcome,
    skips: &[&SkipEntry],
    part_note: &str,
) -> Result<()> {
    writeln!(w, "{} — Merged Codebase{}", folder, part_note)?;
    writeln!(w, "Generated by TurboMerger v{}", env!("CARGO_PKG_VERSION"))?;
    writeln!(
        w,
        "Files scanned: {}  Estimated tokens: ~{} (o200k)\n",
        total_scanned, outcome.tokens_o200k
    )?;
    if cfg.include_tree && first {
        writeln!(w, "PROJECT STRUCTURE\n{}", generate_tree(root, blocks))?;
    }
    for b in blocks {
        writeln!(w, "\n===== {} =====", b.relative)?;
        w.write_all(b.content.as_bytes())?;
        if !b.content.ends_with('\n') {
            writeln!(w)?;
        }
    }
    writeln!(w, "\n===== MERGE REPORT =====")?;
    writeln!(
        w,
        "Files merged: {}  Secrets redacted: {}",
        outcome.files_processed, outcome.secrets_redacted
    )?;
    if !skips.is_empty() {
        writeln!(w, "Skipped ({}):", skips.len())?;
        for e in skips.iter().take(MANIFEST_MAX) {
            writeln!(w, "  {} — {}", e.path, e.reason)?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_xml<W: Write>(
    w: &mut W,
    folder: &str,
    root: &Path,
    blocks: &[&Block],
    cfg: &MergeConfig,
    first: bool,
    total_scanned: usize,
    outcome: &MergeOutcome,
    skips: &[&SkipEntry],
    part_note: &str,
) -> Result<()> {
    writeln!(
        w,
        "<codebase name=\"{}\" files_scanned=\"{}\" tokens_o200k=\"{}\" note=\"{}\">",
        xml_escape(folder),
        total_scanned,
        outcome.tokens_o200k,
        xml_escape(part_note.trim_start_matches(" — "))
    )?;
    if cfg.include_tree && first {
        writeln!(
            w,
            "  <structure>\n{}  </structure>",
            xml_escape(&generate_tree(root, blocks))
        )?;
    }
    for b in blocks {
        writeln!(
            w,
            "  <file path=\"{}\" tokens=\"{}\">",
            xml_escape(&b.relative),
            b.tokens
        )?;
        w.write_all(xml_escape(&b.content).as_bytes())?;
        if !b.content.ends_with('\n') {
            writeln!(w)?;
        }
        writeln!(w, "  </file>")?;
    }
    writeln!(
        w,
        "  <merge_report files_merged=\"{}\" secrets_redacted=\"{}\">",
        outcome.files_processed, outcome.secrets_redacted
    )?;
    for e in skips.iter().take(MANIFEST_MAX) {
        writeln!(
            w,
            "    <skipped path=\"{}\" reason=\"{}\"/>",
            xml_escape(&e.path),
            xml_escape(&e.reason)
        )?;
    }
    writeln!(w, "  </merge_report>")?;
    writeln!(w, "</codebase>")?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_cxml<W: Write>(
    w: &mut W,
    folder: &str,
    root: &Path,
    blocks: &[&Block],
    cfg: &MergeConfig,
    first: bool,
    total_scanned: usize,
    outcome: &MergeOutcome,
    skips: &[&SkipEntry],
    part_note: &str,
) -> Result<()> {
    // Anthropic long-context "documents" convention. Content is NOT XML-escaped
    // (the tags are delimiters), matching files-to-prompt's cxml output.
    writeln!(w, "<documents>")?;
    let mut index = 1;
    if first {
        writeln!(w, "<document index=\"{}\">", index)?;
        writeln!(w, "<source>MERGE_INFO</source>")?;
        writeln!(w, "<document_contents>")?;
        writeln!(w, "{} — Merged Codebase{}", folder, part_note)?;
        writeln!(
            w,
            "Files scanned: {}  Tokens (o200k): ~{}",
            total_scanned, outcome.tokens_o200k
        )?;
        if cfg.include_tree {
            writeln!(w, "\n{}", generate_tree(root, blocks))?;
        }
        if !skips.is_empty() {
            writeln!(
                w,
                "Skipped {} files (redaction/gitignore/binary/etc.).",
                skips.len()
            )?;
        }
        writeln!(w, "</document_contents>")?;
        writeln!(w, "</document>")?;
        index += 1;
    }
    for b in blocks {
        writeln!(w, "<document index=\"{}\">", index)?;
        writeln!(w, "<source>{}</source>", b.relative)?;
        writeln!(w, "<document_contents>")?;
        w.write_all(b.content.as_bytes())?;
        if !b.content.ends_with('\n') {
            writeln!(w)?;
        }
        writeln!(w, "</document_contents>")?;
        writeln!(w, "</document>")?;
        index += 1;
    }
    writeln!(w, "</documents>")?;
    Ok(())
}

fn write_json<W: Write>(
    w: &mut W,
    folder: &str,
    root: &Path,
    blocks: &[&Block],
    outcome: &MergeOutcome,
    skips: &[&SkipEntry],
    part_note: &str,
) -> Result<()> {
    let files: Vec<serde_json::Value> = blocks
        .iter()
        .map(|b| {
            serde_json::json!({
                "path": b.relative,
                "language": b.lang,
                "tokens": b.tokens,
                "content": b.content,
            })
        })
        .collect();
    let skipped: Vec<serde_json::Value> = skips
        .iter()
        .map(|e| serde_json::json!({ "path": e.path, "reason": e.reason }))
        .collect();
    let doc = serde_json::json!({
        "generator": format!("TurboMerger v{}", env!("CARGO_PKG_VERSION")),
        "project": folder,
        "source": root.to_string_lossy(),
        "note": part_note.trim(),
        "tokens_o200k": outcome.tokens_o200k,
        "files_merged": outcome.files_processed,
        "secrets_redacted": outcome.secrets_redacted,
        "files": files,
        "skipped": skipped,
    });
    w.write_all(serde_json::to_string_pretty(&doc)?.as_bytes())?;
    writeln!(w)?;
    Ok(())
}

// ============================================================================
// HELPERS
// ============================================================================

fn relative_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.to_string_lossy().to_string())
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

fn longest_backtick_run(s: &str) -> usize {
    let mut max = 0;
    let mut cur = 0;
    for b in s.bytes() {
        if b == b'`' {
            cur += 1;
            max = max.max(cur);
        } else {
            cur = 0;
        }
    }
    max
}

fn anchor_for(rel: &str) -> String {
    rel.chars()
        .filter_map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_alphanumeric() {
                Some(c)
            } else if c == ' ' || c == '-' {
                Some('-')
            } else {
                None
            }
        })
        .collect()
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

fn order_files(root: &Path, files: &mut [&PathBuf], ordering: Ordering) {
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

/// Recursive ASCII tree of the merged blocks.
fn generate_tree(root: &Path, blocks: &[&Block]) -> String {
    #[derive(Default)]
    struct Node {
        dirs: std::collections::BTreeMap<String, Node>,
        files: Vec<String>,
    }
    let mut top = Node::default();
    for b in blocks {
        if b.synthetic {
            continue;
        }
        let comps: Vec<&str> = b.relative.split('/').collect();
        if comps.is_empty() {
            continue;
        }
        let mut cur = &mut top;
        for c in &comps[..comps.len() - 1] {
            cur = cur.dirs.entry((*c).to_string()).or_default();
        }
        cur.files.push(comps[comps.len() - 1].to_string());
    }
    fn render(node: &Node, prefix: &str, out: &mut String) {
        let n = node.dirs.len() + node.files.len();
        let mut i = 0;
        for (name, child) in &node.dirs {
            i += 1;
            let last = i == n;
            out.push_str(prefix);
            out.push_str(if last { "└── " } else { "├── " });
            out.push_str(name);
            out.push_str("/\n");
            let cp = format!("{}{}", prefix, if last { "    " } else { "│   " });
            render(child, &cp, out);
        }
        for name in &node.files {
            i += 1;
            let last = i == n;
            out.push_str(prefix);
            out.push_str(if last { "└── " } else { "├── " });
            out.push_str(name);
            out.push('\n');
        }
    }
    let mut out = format!(
        "{}/\n",
        root.file_name().and_then(|n| n.to_str()).unwrap_or(".")
    );
    render(&top, "", &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fence_grows_past_embedded_backticks() {
        assert_eq!(longest_backtick_run("no ticks"), 0);
        assert_eq!(longest_backtick_run("a ``` b"), 3);
        assert_eq!(longest_backtick_run("x ````` y ``` z"), 5);
    }

    #[test]
    fn anchors_match_github_slugs() {
        assert_eq!(anchor_for("src/main.rs"), "srcmainrs");
    }

    #[test]
    fn xml_escapes_content() {
        assert_eq!(
            xml_escape("a < b & c > d \"q\""),
            "a &lt; b &amp; c &gt; d &quot;q&quot;"
        );
    }

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
}
