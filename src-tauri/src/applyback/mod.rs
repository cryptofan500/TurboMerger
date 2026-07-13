//! Apply-back (T3-3): parse an LLM reply into per-file change proposals,
//! preview them as diffs against the working tree, and apply accepted ones
//! with timestamped backups + a restore path.
//!
//! Supported reply formats (TurboMerger's own outputs round-trip):
//! - A file header (`## path`, `**path**`, `File: path`, or a backticked
//!   path — backticks also allow paths with spaces) followed by a fenced
//!   code block = whole-file replacement. New paths create files.
//! - cxml documents (`<source>path</source>` + `<document_contents>`),
//!   parsed outside fenced regions; synthetic sources (MERGE_INFO, GIT *)
//!   are ignored.
//! - Unified diffs (`--- a/x` / `+++ b/x` with `@@` hunks), fenced or bare.
//!
//! Safety rails: proposals can never escape the target root (lexical check —
//! new files can't canonicalize), binary targets are refused, deletions are
//! surfaced but never executed, nothing is written at parse/preview time,
//! and every apply first copies originals to
//! `<root>/.turbomerger/backups/<UTC>/files/<rel>` plus a manifest that
//! `restore_last` reverses. `.turbomerger/` is in the scanner's always-skip
//! set so backups never re-merge.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

// ============================================================================
// PARSED CHANGES
// ============================================================================

#[derive(Debug, Clone)]
pub enum ChangeBody {
    /// Whole-file replacement content.
    Full(String),
    /// Unified-diff hunks to apply against the current content.
    Diff(Vec<Hunk>),
    /// A `+++ /dev/null` deletion — surfaced, never executed.
    Delete,
}

#[derive(Debug, Clone)]
pub struct ParsedChange {
    pub path: String,
    pub body: ChangeBody,
    /// Document order, so later changes to the same file chain on earlier ones.
    order: usize,
}

#[derive(Debug, Clone)]
pub struct Hunk {
    pub old_start: usize,
    pub new_start: usize,
    pub lines: Vec<HunkLine>,
    /// `\ No newline at end of file` seen after the last NEW-side line.
    pub new_no_eol: bool,
}

#[derive(Debug, Clone)]
pub enum HunkLine {
    Ctx(String),
    Del(String),
    Ins(String),
}

// ============================================================================
// PREVIEW / APPLY DATA (serialized to the UI)
// ============================================================================

#[derive(Debug, Clone, Serialize)]
pub struct DiffLine {
    /// "eq" | "del" | "ins"
    pub tag: &'static str,
    pub old: Option<usize>,
    pub new: Option<usize>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PreviewFile {
    pub rel_path: String,
    /// "modify" | "create" | "delete"
    pub action: String,
    pub ok: bool,
    /// Reason when !ok, or a non-fatal remark (e.g. "no changes").
    pub note: String,
    /// True when the proposal equals what is already on disk.
    pub identical: bool,
    pub adds: usize,
    pub dels: usize,
    pub diff: Vec<DiffLine>,
    pub diff_truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Preview {
    pub root: String,
    pub files: Vec<PreviewFile>,
}

/// A validated, appliable file: everything `apply_files` needs.
#[derive(Debug, Clone)]
pub struct ReadyFile {
    pub rel_path: String,
    pub new_content: String,
    /// Hash of the on-disk bytes at preview time; None = file must not exist.
    pub base_hash: Option<u64>,
}

#[derive(Debug)]
pub struct BuiltPreview {
    pub preview: Preview,
    pub ready: Vec<ReadyFile>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApplyFailure {
    pub rel_path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApplyOutcome {
    pub backup_dir: Option<String>,
    pub applied: Vec<String>,
    pub failed: Vec<ApplyFailure>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RestoreOutcome {
    pub backup_dir: String,
    pub restored: Vec<String>,
    pub deleted: Vec<String>,
}

/// Cap on preview diff lines sent to the UI per file.
const DIFF_LINE_CAP: usize = 4000;

// ============================================================================
// REPLY PARSER
// ============================================================================

/// Parse an LLM reply into per-file change proposals, in document order.
/// Pure text → proposals; touches no filesystem.
///
/// Line-ending policy: structural matching (headers, fences, diff grammar) is
/// always `\r`-insensitive, but captured file BODIES keep their raw `\r`s —
/// TurboMerger's own merged output embeds source bytes verbatim, so a CRLF or
/// mixed-endings file must round-trip byte-exact. Only when the whole reply is
/// uniformly CRLF (a reply saved by a CRLF editor) are body `\r`s treated as
/// encoding artifacts and stripped; ending-agnostic (`\r`-free) bodies are
/// later adapted to the target file's endings in `build_preview`.
pub fn parse_reply(text: &str) -> Vec<ParsedChange> {
    let newline_count = text.matches('\n').count();
    let uniform_crlf = newline_count > 0 && text.matches("\r\n").count() == newline_count;
    let raw_lines: Vec<&str> = text.split('\n').collect();
    let lines: Vec<&str> = raw_lines
        .iter()
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let mut changes: Vec<ParsedChange> = Vec::new();
    let mut fenced: Vec<(usize, usize)> = Vec::new(); // [start, end] line ranges of fences

    let mut pending_path: Option<String> = None;
    let mut pending_prose_used = false;
    let mut i = 0usize;

    while i < lines.len() {
        let line = lines[i];

        // Fenced block?
        if let Some((fence_char, fence_len, _info)) = fence_open(line) {
            let body_start = i + 1;
            let mut j = body_start;
            while j < lines.len() && !fence_close(lines[j], fence_char, fence_len) {
                j += 1;
            }
            let end = j.min(lines.len());
            fenced.push((i, j));
            if looks_like_unified_diff(&lines[body_start..end]) {
                let (mut parsed, _) = parse_unified_block(&lines[body_start..end], i);
                changes.append(&mut parsed);
            } else if let Some(path) = pending_path.take() {
                let body: &[&str] = if uniform_crlf {
                    &lines[body_start..end]
                } else {
                    &raw_lines[body_start..end]
                };
                let mut content = body.join("\n");
                if !content.is_empty() {
                    content.push('\n');
                }
                changes.push(ParsedChange {
                    path,
                    body: ChangeBody::Full(content),
                    order: i,
                });
            }
            pending_path = None;
            pending_prose_used = false;
            i = (j + 1).min(lines.len());
            continue;
        }

        // Bare unified diff?
        if line.starts_with("--- ") || line.starts_with("diff --git ") {
            let (mut parsed, consumed) = parse_unified_block(&lines[i..], i);
            if !parsed.is_empty() {
                changes.append(&mut parsed);
                pending_path = None;
                pending_prose_used = false;
                i += consumed.max(1);
                continue;
            }
        }

        // Header candidate?
        if let Some(path) = header_path(line) {
            pending_path = Some(path);
            pending_prose_used = false;
            i += 1;
            continue;
        }

        // Blank lines keep a pending header alive; one short "Here's the
        // file:" style prose line (ends with ':') is tolerated once.
        if line.trim().is_empty() {
            i += 1;
            continue;
        }
        if pending_path.is_some()
            && !pending_prose_used
            && line.trim_end().ends_with(':')
            && line.len() < 120
        {
            pending_prose_used = true;
            i += 1;
            continue;
        }
        pending_path = None;
        pending_prose_used = false;
        i += 1;
    }

    // cxml documents, outside fenced regions (a fence quoting cxml is an example).
    let mut cxml = parse_cxml(&raw_lines, &lines, &fenced, uniform_crlf);
    changes.append(&mut cxml);

    changes.sort_by_key(|c| c.order);
    changes
}

/// `(fence_char, len, info-string)` when the line opens a code fence.
fn fence_open(line: &str) -> Option<(char, usize, String)> {
    let t = line.trim_start();
    for ch in ['`', '~'] {
        let run = t.chars().take_while(|&c| c == ch).count();
        if run >= 3 {
            let info = t[run..].trim().to_string();
            // An info string containing the fence char is not an opener (CommonMark).
            if !info.contains(ch) {
                return Some((ch, run, info));
            }
        }
    }
    None
}

fn fence_close(line: &str, fence_char: char, fence_len: usize) -> bool {
    let t = line.trim();
    !t.is_empty() && t.chars().all(|c| c == fence_char) && t.len() >= fence_len
}

/// Extract a file path from a header line (`## path`, `**path**`, `File: path`,
/// or a standalone backticked path). Backticks allow paths with spaces.
fn header_path(line: &str) -> Option<String> {
    let t = line.trim();
    let candidate = if t.starts_with('#') {
        // `## path` — any heading level; markdown requires a space after the
        // hashes, which also keeps shebangs and `#pragma` lines out.
        let hashes = t.chars().take_while(|&c| c == '#').count();
        if hashes > 6 || !t[hashes..].starts_with(' ') {
            return None;
        }
        t[hashes..].trim()
    } else if t.starts_with("**") && t.trim_end_matches(':').ends_with("**") {
        t.trim_end_matches(':').trim_matches('*').trim()
    } else if t.starts_with('`') && t.trim_end_matches(':').ends_with('`') && t.len() > 2 {
        t // clean_header_path strips the backticks
    } else {
        let lower = t.to_ascii_lowercase();
        if lower.starts_with("file:")
            || lower.starts_with("path:")
            || lower.starts_with("filename:")
        {
            t.split_once(':').map(|(_, rest)| rest.trim()).unwrap_or("")
        } else {
            return None;
        }
    };
    clean_header_path(candidate)
}

/// Normalize + vet a header path candidate. Returns forward-slash form.
fn clean_header_path(raw: &str) -> Option<String> {
    let mut s = raw.trim();
    let mut backticked = false;
    s = s.trim_end_matches(':').trim();
    if s.starts_with('`') && s.ends_with('`') && s.len() >= 2 {
        backticked = true;
        s = s[1..s.len() - 1].trim();
    }
    s = s.trim_start_matches("**").trim_end_matches("**").trim();
    if s.is_empty() || s.len() > 300 {
        return None;
    }
    if s.chars()
        .any(|c| matches!(c, '<' | '>' | '"' | '|' | '?' | '*'))
    {
        return None;
    }
    // Unbackticked paths must look path-like: no spaces, and a dot or slash.
    if !backticked && s.contains(char::is_whitespace) {
        return None;
    }
    if !(s.contains('/') || s.contains('.') || s.contains('\\')) {
        return None;
    }
    Some(s.replace('\\', "/"))
}

/// True when a fenced body is a unified diff rather than file content.
fn looks_like_unified_diff(body: &[&str]) -> bool {
    let mut saw_minus = false;
    for l in body.iter().take(10) {
        if l.starts_with("diff --git ") {
            return true;
        }
        if l.starts_with("--- ") {
            saw_minus = true;
        } else if saw_minus && l.starts_with("+++ ") {
            return true;
        }
    }
    false
}

/// Strip `a/` / `b/` prefixes and trailing tab-metadata from a diff path.
fn diff_path(raw: &str) -> String {
    let p = raw.split('\t').next().unwrap_or(raw).trim();
    let p = p
        .strip_prefix("a/")
        .or_else(|| p.strip_prefix("b/"))
        .unwrap_or(p);
    p.trim_matches('"').replace('\\', "/")
}

/// Parse one or more file diffs from `lines`. Returns proposals + lines consumed.
fn parse_unified_block(lines: &[&str], order_base: usize) -> (Vec<ParsedChange>, usize) {
    let mut changes = Vec::new();
    let mut i = 0usize;

    while i < lines.len() {
        let line = lines[i];
        if line.starts_with("diff --git ")
            || line.starts_with("index ")
            || line.starts_with("new file mode")
            || line.starts_with("deleted file mode")
            || line.starts_with("old mode")
            || line.starts_with("new mode")
            || line.starts_with("similarity index")
            || line.starts_with("rename from")
            || line.starts_with("rename to")
            || line.starts_with("Binary files ")
        {
            i += 1;
            continue;
        }
        if !line.starts_with("--- ") {
            break;
        }
        if i + 1 >= lines.len() || !lines[i + 1].starts_with("+++ ") {
            break;
        }
        let old_path = diff_path(&lines[i]["--- ".len()..]);
        let new_path = diff_path(&lines[i + 1]["+++ ".len()..]);
        i += 2;

        let mut hunks: Vec<Hunk> = Vec::new();
        while i < lines.len() {
            let Some((old_start, old_count, new_start, new_count)) = hunk_header(lines[i]) else {
                break;
            };
            i += 1;
            let mut old_left = old_count;
            let mut new_left = new_count;
            let mut hl: Vec<HunkLine> = Vec::new();
            let mut new_no_eol = false;
            while (old_left > 0 || new_left > 0) && i < lines.len() {
                let l = lines[i];
                if let Some(rest) = l.strip_prefix('+') {
                    hl.push(HunkLine::Ins(rest.to_string()));
                    new_left = new_left.saturating_sub(1);
                } else if let Some(rest) = l.strip_prefix('-') {
                    hl.push(HunkLine::Del(rest.to_string()));
                    old_left = old_left.saturating_sub(1);
                } else if l.starts_with('\\') {
                    // "\ No newline at end of file" — applies to the side of
                    // the line just above; only the new side changes output.
                    if matches!(hl.last(), Some(HunkLine::Ins(_)) | Some(HunkLine::Ctx(_))) {
                        new_no_eol = true;
                    }
                } else {
                    // Context. The leading space is stripped; a fully blank
                    // line is a context line whose trailing space a chat UI
                    // ate (hunk counts keep this unambiguous).
                    let rest = l.strip_prefix(' ').unwrap_or(l);
                    hl.push(HunkLine::Ctx(rest.to_string()));
                    old_left = old_left.saturating_sub(1);
                    new_left = new_left.saturating_sub(1);
                }
                i += 1;
            }
            // Trailing no-newline marker directly after the hunk body.
            if i < lines.len() && lines[i].starts_with('\\') {
                new_no_eol = true;
                i += 1;
            }
            hunks.push(Hunk {
                old_start,
                new_start,
                lines: hl,
                new_no_eol,
            });
        }

        if new_path == "/dev/null" {
            changes.push(ParsedChange {
                path: old_path,
                body: ChangeBody::Delete,
                order: order_base + i,
            });
        } else if !hunks.is_empty() {
            changes.push(ParsedChange {
                path: new_path,
                body: ChangeBody::Diff(hunks),
                order: order_base + i,
            });
        }
    }
    (changes, i)
}

/// Parse `@@ -l[,c] +l[,c] @@` → (old_start, old_count, new_start, new_count).
fn hunk_header(line: &str) -> Option<(usize, usize, usize, usize)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let new = rest.split_once(" @@").map(|(n, _)| n)?;
    let parse_pair = |s: &str| -> Option<(usize, usize)> {
        match s.split_once(',') {
            Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
            None => Some((s.parse().ok()?, 1)),
        }
    };
    let (os, oc) = parse_pair(old)?;
    let (ns, nc) = parse_pair(new)?;
    Some((os, oc, ns, nc))
}

/// cxml `<source>path</source>…<document_contents>` pairs outside fences.
/// `raw_lines` keep original `\r`s for body capture (see `parse_reply`).
fn parse_cxml(
    raw_lines: &[&str],
    lines: &[&str],
    fenced: &[(usize, usize)],
    uniform_crlf: bool,
) -> Vec<ParsedChange> {
    let in_fence = |idx: usize| fenced.iter().any(|&(s, e)| idx >= s && idx <= e);
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        if in_fence(i) {
            i += 1;
            continue;
        }
        let t = lines[i].trim();
        let Some(rest) = t.strip_prefix("<source>") else {
            i += 1;
            continue;
        };
        let Some(path) = rest.strip_suffix("</source>") else {
            i += 1;
            continue;
        };
        let path = path.trim().to_string();
        // Find the contents opener within the next couple of lines.
        let mut j = i + 1;
        while j < lines.len() && j <= i + 3 && lines[j].trim() != "<document_contents>" {
            j += 1;
        }
        if j >= lines.len() || lines[j].trim() != "<document_contents>" {
            i += 1;
            continue;
        }
        let body_start = j + 1;
        let mut k = body_start;
        while k < lines.len() && lines[k].trim() != "</document_contents>" {
            k += 1;
        }
        if k >= lines.len() {
            break;
        }
        let synthetic =
            path == "MERGE_INFO" || path.starts_with("GIT DIFF") || path.starts_with("GIT LOG");
        if !synthetic && clean_header_path(&format!("`{}`", path)).is_some() {
            let body: &[&str] = if uniform_crlf {
                &lines[body_start..k]
            } else {
                &raw_lines[body_start..k]
            };
            let mut content = body.join("\n");
            if !content.is_empty() {
                content.push('\n');
            }
            out.push(ParsedChange {
                path: path.replace('\\', "/"),
                body: ChangeBody::Full(content),
                order: i,
            });
        }
        i = k + 1;
    }
    out
}

// ============================================================================
// DIFF APPLICATION
// ============================================================================

/// Apply parsed hunks to `original`. Exact placement at the stated line is
/// tried first, then the old block is searched forward (LLM line numbers
/// drift). Line endings of the original (CRLF vs LF) are preserved.
pub fn apply_hunks(original: &str, hunks: &[Hunk]) -> Result<String, String> {
    let had_crlf = original.contains("\r\n");
    let orig: Vec<&str> = if original.is_empty() {
        Vec::new()
    } else {
        original.lines().collect()
    };
    let orig_ends_nl = original.is_empty() || original.ends_with('\n');

    let mut out: Vec<String> = Vec::new();
    let mut cursor = 0usize;
    let mut no_eol = false;

    for (hi, h) in hunks.iter().enumerate() {
        let old_block: Vec<&str> = h
            .lines
            .iter()
            .filter_map(|l| match l {
                HunkLine::Ctx(s) | HunkLine::Del(s) => Some(s.as_str()),
                HunkLine::Ins(_) => None,
            })
            .collect();
        let ideal = h.old_start.saturating_sub(1);
        let pos = if old_block.is_empty() {
            // Pure insertion: trust the stated position, clamped.
            ideal.clamp(cursor, orig.len())
        } else {
            find_block(&orig, &old_block, cursor, ideal).ok_or_else(|| {
                format!(
                    "hunk {} (@@ -{} +{}) does not match the current file content",
                    hi + 1,
                    h.old_start,
                    h.new_start
                )
            })?
        };
        out.extend(orig[cursor..pos].iter().map(|s| s.to_string()));
        for l in &h.lines {
            match l {
                HunkLine::Ctx(s) | HunkLine::Ins(s) => out.push(s.clone()),
                HunkLine::Del(_) => {}
            }
        }
        cursor = pos + old_block.len();
        no_eol = h.new_no_eol;
    }
    let trailing = cursor < orig.len();
    out.extend(orig[cursor..].iter().map(|s| s.to_string()));

    let mut res = out.join("\n");
    // The original's trailing newline wins unless the last hunk rewrote the
    // end of the file and flagged no-newline.
    let ends_nl = if trailing {
        orig_ends_nl
    } else {
        !no_eol && (orig_ends_nl || orig.is_empty())
    };
    if ends_nl && !res.is_empty() {
        res.push('\n');
    }
    if had_crlf {
        res = res.replace('\n', "\r\n");
    }
    Ok(res)
}

/// Find `block` in `orig` at/after `cursor`: exact position first, then scan.
/// Comparison is trailing-whitespace tolerant (chat UIs strip it).
fn find_block(orig: &[&str], block: &[&str], cursor: usize, ideal: usize) -> Option<usize> {
    let matches_at = |pos: usize| -> bool {
        pos + block.len() <= orig.len()
            && block
                .iter()
                .zip(&orig[pos..pos + block.len()])
                .all(|(b, o)| b.trim_end() == o.trim_end())
    };
    if ideal >= cursor && matches_at(ideal) {
        return Some(ideal);
    }
    (cursor..=orig.len().saturating_sub(block.len())).find(|&pos| matches_at(pos))
}

/// Re-terminate `new` content with the original's line endings so a CRLF file
/// doesn't come back as a whole-file LF rewrite.
fn match_line_endings(new: &str, original: &str) -> String {
    let normalized = new.replace("\r\n", "\n");
    if original.contains("\r\n") {
        normalized.replace('\n', "\r\n")
    } else {
        normalized
    }
}

// ============================================================================
// PREVIEW (dry-run — no writes)
// ============================================================================

/// Join a proposal path under `root`, refusing anything that could escape it.
/// Lexical (component-based) because created files can't canonicalize yet.
fn safe_join(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel = rel.trim();
    if rel.is_empty() {
        return Err("empty path".into());
    }
    // ':' rejects drive letters and NTFS alternate data streams in one go.
    if rel.contains(':') {
        return Err("absolute or drive-qualified paths are not allowed".into());
    }
    let p = Path::new(rel);
    if p.is_absolute() || rel.starts_with('/') || rel.starts_with('\\') {
        return Err("absolute paths are not allowed".into());
    }
    let mut clean = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::Normal(c) => clean.push(c),
            Component::CurDir => {}
            _ => return Err("path escapes the target root".into()),
        }
    }
    if clean.as_os_str().is_empty() {
        return Err("empty path".into());
    }
    Ok(root.join(clean))
}

/// FNV-1a 64 over raw bytes — change detection between preview and apply.
fn content_hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

struct DiskState {
    exists: bool,
    binary: bool,
    content: String,
    hash: u64,
}

fn read_disk(target: &Path) -> Result<DiskState, String> {
    if !target.exists() {
        return Ok(DiskState {
            exists: false,
            binary: false,
            content: String::new(),
            hash: 0,
        });
    }
    if target.is_dir() {
        return Err("target is a directory".into());
    }
    let bytes = std::fs::read(target).map_err(|e| format!("unreadable: {}", e))?;
    let check = 8192.min(bytes.len());
    let binary = crate::security::is_binary_content(&bytes[..check]);
    Ok(DiskState {
        exists: true,
        binary,
        content: String::from_utf8_lossy(&bytes).into_owned(),
        hash: content_hash(&bytes),
    })
}

/// Resolve proposals against the working tree: validate paths, apply diffs,
/// compute per-file previews. Dry-run — reads only.
pub fn build_preview(root: &Path, changes: &[ParsedChange]) -> Result<BuiltPreview, String> {
    // Fold changes per path in document order (later ones chain on earlier).
    struct Slot {
        first_order: usize,
        result: Result<(String, DiskState), String>, // (proposed content, disk)
        is_delete: bool,
    }
    let mut slots: HashMap<String, Slot> = HashMap::new();
    let mut order: Vec<String> = Vec::new();

    for ch in changes {
        let rel = ch.path.trim_matches('/').to_string();
        if !slots.contains_key(&rel) {
            order.push(rel.clone());
        }
        let slot = slots.entry(rel.clone()).or_insert_with(|| Slot {
            first_order: ch.order,
            result: Err("unresolved".into()),
            is_delete: false,
        });
        slot.first_order = slot.first_order.min(ch.order);

        if matches!(ch.body, ChangeBody::Delete) {
            slot.is_delete = true;
            continue;
        }
        // (Re)resolve the base: prior proposed content, else disk.
        let base = match &slot.result {
            Ok((content, disk)) => Ok((
                content.clone(),
                DiskState {
                    exists: disk.exists,
                    binary: disk.binary,
                    content: disk.content.clone(),
                    hash: disk.hash,
                },
            )),
            Err(_) => match safe_join(root, &rel) {
                Ok(target) => match read_disk(&target) {
                    Ok(disk) => {
                        if disk.binary {
                            Err("refusing to modify a binary file".to_string())
                        } else {
                            Ok((disk.content.clone(), disk))
                        }
                    }
                    Err(e) => Err(e),
                },
                Err(e) => Err(e),
            },
        };
        slot.result = match base {
            Err(e) => Err(e),
            Ok((base_content, disk)) => match &ch.body {
                // A body carrying `\r`s is byte-authoritative (round-tripped
                // source content); an ending-agnostic body adapts to the
                // target file's endings.
                ChangeBody::Full(c) if c.contains('\r') => Ok((c.clone(), disk)),
                ChangeBody::Full(c) => Ok((match_line_endings(c, &disk.content), disk)),
                ChangeBody::Diff(hunks) => match apply_hunks(&base_content, hunks) {
                    Ok(next) => Ok((next, disk)),
                    Err(e) => Err(e),
                },
                ChangeBody::Delete => unreachable!(),
            },
        };
    }

    let mut files: Vec<PreviewFile> = Vec::new();
    let mut ready: Vec<ReadyFile> = Vec::new();

    for rel in order {
        let slot = &slots[&rel];
        if slot.is_delete {
            files.push(PreviewFile {
                rel_path: rel,
                action: "delete".into(),
                ok: false,
                note: "deletion proposals are shown but never executed — delete manually".into(),
                identical: false,
                adds: 0,
                dels: 0,
                diff: Vec::new(),
                diff_truncated: false,
            });
            continue;
        }
        match &slot.result {
            Err(e) => files.push(PreviewFile {
                rel_path: rel,
                action: "modify".into(),
                ok: false,
                note: e.clone(),
                identical: false,
                adds: 0,
                dels: 0,
                diff: Vec::new(),
                diff_truncated: false,
            }),
            Ok((proposed, disk)) => {
                let action = if disk.exists { "modify" } else { "create" };
                let identical = disk.exists && *proposed == disk.content;
                let (diff, adds, dels, truncated) = if identical {
                    (Vec::new(), 0, 0, false)
                } else {
                    diff_lines(&disk.content, proposed)
                };
                if !identical {
                    ready.push(ReadyFile {
                        rel_path: rel.clone(),
                        new_content: proposed.clone(),
                        base_hash: disk.exists.then_some(disk.hash),
                    });
                }
                files.push(PreviewFile {
                    rel_path: rel,
                    action: action.into(),
                    ok: true,
                    note: if identical {
                        "no changes — already matches disk".into()
                    } else {
                        String::new()
                    },
                    identical,
                    adds,
                    dels,
                    diff,
                    diff_truncated: truncated,
                });
            }
        }
    }

    Ok(BuiltPreview {
        preview: Preview {
            root: root.to_string_lossy().to_string(),
            files,
        },
        ready,
    })
}

/// Line diff via `similar` for the review UI; capped for very large files.
fn diff_lines(old: &str, new: &str) -> (Vec<DiffLine>, usize, usize, bool) {
    let diff = similar::TextDiff::from_lines(old, new);
    let mut out = Vec::new();
    let mut adds = 0usize;
    let mut dels = 0usize;
    let mut truncated = false;
    for change in diff.iter_all_changes() {
        let (tag, is_add, is_del) = match change.tag() {
            similar::ChangeTag::Equal => ("eq", false, false),
            similar::ChangeTag::Delete => ("del", false, true),
            similar::ChangeTag::Insert => ("ins", true, false),
        };
        if is_add {
            adds += 1;
        }
        if is_del {
            dels += 1;
        }
        if out.len() < DIFF_LINE_CAP {
            out.push(DiffLine {
                tag,
                old: change.old_index().map(|i| i + 1),
                new: change.new_index().map(|i| i + 1),
                text: change.value().trim_end_matches(['\n', '\r']).to_string(),
            });
        } else {
            truncated = true;
        }
    }
    (out, adds, dels, truncated)
}

// ============================================================================
// APPLY + RESTORE
// ============================================================================

#[derive(Debug, Serialize, Deserialize)]
struct ManifestEntry {
    path: String,
    existed: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    created_utc: String,
    root: String,
    entries: Vec<ManifestEntry>,
}

fn backups_root(root: &Path) -> PathBuf {
    root.join(".turbomerger").join("backups")
}

/// Apply validated files: back originals up under
/// `<root>/.turbomerger/backups/<UTC>/files/<rel>`, write a manifest, then
/// write the new contents. Per-file hash mismatches fail that file only.
pub fn apply_files(root: &Path, files: &[ReadyFile]) -> Result<ApplyOutcome, String> {
    let mut outcome = ApplyOutcome {
        backup_dir: None,
        applied: Vec::new(),
        failed: Vec::new(),
    };

    // Revalidate everything before touching the disk.
    let mut valid: Vec<(&ReadyFile, PathBuf, bool)> = Vec::new(); // (file, target, existed)
    for f in files {
        let target = match safe_join(root, &f.rel_path) {
            Ok(t) => t,
            Err(e) => {
                outcome.failed.push(ApplyFailure {
                    rel_path: f.rel_path.clone(),
                    reason: e,
                });
                continue;
            }
        };
        match (&f.base_hash, target.exists()) {
            (Some(expected), true) => {
                let bytes = match std::fs::read(&target) {
                    Ok(b) => b,
                    Err(e) => {
                        outcome.failed.push(ApplyFailure {
                            rel_path: f.rel_path.clone(),
                            reason: format!("unreadable: {}", e),
                        });
                        continue;
                    }
                };
                if content_hash(&bytes) != *expected {
                    outcome.failed.push(ApplyFailure {
                        rel_path: f.rel_path.clone(),
                        reason: "changed on disk since the preview — re-parse the reply".into(),
                    });
                    continue;
                }
                valid.push((f, target, true));
            }
            (Some(_), false) => {
                outcome.failed.push(ApplyFailure {
                    rel_path: f.rel_path.clone(),
                    reason: "file disappeared since the preview — re-parse the reply".into(),
                });
            }
            (None, true) => {
                outcome.failed.push(ApplyFailure {
                    rel_path: f.rel_path.clone(),
                    reason: "file appeared on disk since the preview — re-parse the reply".into(),
                });
            }
            (None, false) => valid.push((f, target, false)),
        }
    }
    if valid.is_empty() {
        return Ok(outcome);
    }

    // Backup dir: UTC stamp, uniquified if two applies land in one second.
    let stamp = chrono::Utc::now().format("%Y-%m-%dT%H-%M-%SZ").to_string();
    let mut backup_dir = backups_root(root).join(&stamp);
    let mut n = 1;
    while backup_dir.exists() {
        n += 1;
        backup_dir = backups_root(root).join(format!("{}-{}", stamp, n));
    }
    let files_dir = backup_dir.join("files");
    std::fs::create_dir_all(&files_dir).map_err(|e| format!("cannot create backup dir: {}", e))?;

    // Backups + manifest FIRST, so a crash mid-write is always restorable.
    let mut entries = Vec::new();
    for (f, target, existed) in &valid {
        if *existed {
            let dst = files_dir.join(f.rel_path.replace('/', std::path::MAIN_SEPARATOR_STR));
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("backup dir failed for {}: {}", f.rel_path, e))?;
            }
            std::fs::copy(target, &dst)
                .map_err(|e| format!("backup failed for {}: {}", f.rel_path, e))?;
        }
        entries.push(ManifestEntry {
            path: f.rel_path.clone(),
            existed: *existed,
        });
    }
    let manifest = Manifest {
        created_utc: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        root: root.to_string_lossy().to_string(),
        entries,
    };
    std::fs::write(
        backup_dir.join("manifest.json"),
        serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("manifest write failed: {}", e))?;
    outcome.backup_dir = Some(backup_dir.to_string_lossy().to_string());

    // Now write.
    for (f, target, _) in &valid {
        if let Some(parent) = target.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                outcome.failed.push(ApplyFailure {
                    rel_path: f.rel_path.clone(),
                    reason: format!("cannot create parent dir: {}", e),
                });
                continue;
            }
        }
        match std::fs::write(target, f.new_content.as_bytes()) {
            Ok(()) => outcome.applied.push(f.rel_path.clone()),
            Err(e) => outcome.failed.push(ApplyFailure {
                rel_path: f.rel_path.clone(),
                reason: format!("write failed: {}", e),
            }),
        }
    }
    Ok(outcome)
}

/// Reverse the most recent apply from its manifest: restore backed-up
/// originals, delete files the apply created. Idempotent; keeps the backup.
pub fn restore_last(root: &Path) -> Result<RestoreOutcome, String> {
    let base = backups_root(root);
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&base)
        .map_err(|_| "no backups found for this folder".to_string())?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir() && p.join("manifest.json").exists())
        .collect();
    dirs.sort();
    let latest = dirs
        .pop()
        .ok_or_else(|| "no backups found for this folder".to_string())?;

    let manifest: Manifest = serde_json::from_str(
        &std::fs::read_to_string(latest.join("manifest.json"))
            .map_err(|e| format!("manifest unreadable: {}", e))?,
    )
    .map_err(|e| format!("manifest corrupt: {}", e))?;

    let mut restored = Vec::new();
    let mut deleted = Vec::new();
    for entry in &manifest.entries {
        let target = safe_join(root, &entry.path)?;
        if entry.existed {
            let src = latest
                .join("files")
                .join(entry.path.replace('/', std::path::MAIN_SEPARATOR_STR));
            if let Some(parent) = target.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::copy(&src, &target)
                .map_err(|e| format!("restore failed for {}: {}", entry.path, e))?;
            restored.push(entry.path.clone());
        } else if target.exists() {
            std::fs::remove_file(&target)
                .map_err(|e| format!("delete failed for {}: {}", entry.path, e))?;
            deleted.push(entry.path.clone());
        }
    }
    Ok(RestoreOutcome {
        backup_dir: latest.to_string_lossy().to_string(),
        restored,
        deleted,
    })
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_paths_parse_and_prose_is_rejected() {
        assert_eq!(header_path("## src/main.rs"), Some("src/main.rs".into()));
        assert_eq!(header_path("### `src/app.tsx`"), Some("src/app.tsx".into()));
        assert_eq!(header_path("**lib/util.py**"), Some("lib/util.py".into()));
        assert_eq!(header_path("**lib/util.py**:"), Some("lib/util.py".into()));
        assert_eq!(
            header_path("File: docs/notes.md"),
            Some("docs/notes.md".into())
        );
        assert_eq!(
            header_path("`My Dir/file name.rs`"),
            Some("My Dir/file name.rs".into())
        );
        assert_eq!(
            header_path("`src\\win\\path.rs`"),
            Some("src/win/path.rs".into())
        );
        assert_eq!(header_path("## Summary"), None);
        assert_eq!(header_path("## What changed in src/main.rs"), None);
        assert_eq!(header_path("plain prose line"), None);
        assert_eq!(header_path("## a?.rs"), None);
    }

    #[test]
    fn parse_markdown_reply_with_fences() {
        let reply = "Intro prose.\n\n## src/a.rs\n\n```rust\nfn a() {}\n```\n\nSome words.\n\n**b.md**\nHere's the file:\n\n````\n# B\n\n```rust\ninner fence\n```\n````\n";
        let changes = parse_reply(reply);
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].path, "src/a.rs");
        match &changes[0].body {
            ChangeBody::Full(c) => assert_eq!(c, "fn a() {}\n"),
            _ => panic!("expected full"),
        }
        assert_eq!(changes[1].path, "b.md");
        match &changes[1].body {
            ChangeBody::Full(c) => assert!(c.contains("inner fence"), "nested fence kept: {}", c),
            _ => panic!("expected full"),
        }
    }

    #[test]
    fn orphan_fence_without_header_is_ignored() {
        let reply = "Look at this example:\n\n```rust\nfn example() {}\n```\n";
        assert!(parse_reply(reply).is_empty());
    }

    #[test]
    fn parse_unified_diff_bare_and_fenced() {
        let reply = "Apply this:\n\n--- a/src/x.rs\n+++ b/src/x.rs\n@@ -1,3 +1,3 @@\n fn x() {\n-    1\n+    2\n }\n\nand also:\n\n```diff\n--- a/y.txt\n+++ b/y.txt\n@@ -1 +1 @@\n-old\n+new\n```\n";
        let changes = parse_reply(reply);
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].path, "src/x.rs");
        assert!(matches!(changes[0].body, ChangeBody::Diff(_)));
        assert_eq!(changes[1].path, "y.txt");
    }

    #[test]
    fn deletion_diffs_surface_as_delete() {
        let reply = "--- a/gone.rs\n+++ /dev/null\n@@ -1 +0,0 @@\n-fn gone() {}\n";
        let changes = parse_reply(reply);
        assert_eq!(changes.len(), 1);
        assert!(matches!(changes[0].body, ChangeBody::Delete));
        assert_eq!(changes[0].path, "gone.rs");
    }

    #[test]
    fn cxml_documents_parse_and_synthetics_skip() {
        let reply = "<documents>\n<document index=\"1\">\n<source>MERGE_INFO</source>\n<document_contents>\nheader stuff\n</document_contents>\n</document>\n<document index=\"2\">\n<source>src/c.py</source>\n<document_contents>\ndef c():\n    pass\n</document_contents>\n</document>\n</documents>\n";
        let changes = parse_reply(reply);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "src/c.py");
        match &changes[0].body {
            ChangeBody::Full(c) => assert_eq!(c, "def c():\n    pass\n"),
            _ => panic!("expected full"),
        }
    }

    #[test]
    fn hunks_apply_with_drifted_line_numbers() {
        let original = "line1\nline2\nline3\nline4\nline5\n";
        let hunks = vec![Hunk {
            old_start: 1, // wrong on purpose — real match is at line 3
            new_start: 1,
            lines: vec![
                HunkLine::Ctx("line3".into()),
                HunkLine::Del("line4".into()),
                HunkLine::Ins("LINE4".into()),
            ],
            new_no_eol: false,
        }];
        let out = apply_hunks(original, &hunks).unwrap();
        assert_eq!(out, "line1\nline2\nline3\nLINE4\nline5\n");
    }

    #[test]
    fn hunk_mismatch_reports_error() {
        let hunks = vec![Hunk {
            old_start: 1,
            new_start: 1,
            lines: vec![HunkLine::Del("not present".into())],
            new_no_eol: false,
        }];
        let err = apply_hunks("actual content\n", &hunks).unwrap_err();
        assert!(err.contains("does not match"), "{}", err);
    }

    #[test]
    fn uniform_crlf_reply_bodies_are_normalized() {
        // A reply saved by a CRLF editor: body \r's are encoding artifacts,
        // not content — the captured body must come out ending-agnostic.
        let reply = "## a.txt\r\n\r\n```\r\nhello\r\nworld\r\n```\r\n";
        let changes = parse_reply(reply);
        assert_eq!(changes.len(), 1);
        match &changes[0].body {
            ChangeBody::Full(c) => assert_eq!(c, "hello\nworld\n"),
            _ => panic!("expected full"),
        }
    }

    #[test]
    fn mixed_ending_reply_bodies_stay_byte_exact() {
        // An LF reply embedding a CRLF line (TurboMerger's own output shape):
        // the \r IS content and must survive capture.
        let reply = "## a.txt\n\n```\nplain\ncarriage\r\n```\n";
        let changes = parse_reply(reply);
        assert_eq!(changes.len(), 1);
        match &changes[0].body {
            ChangeBody::Full(c) => assert_eq!(c, "plain\ncarriage\r\n"),
            _ => panic!("expected full"),
        }
    }

    #[test]
    fn crlf_originals_stay_crlf() {
        let original = "a\r\nb\r\n";
        let hunks = vec![Hunk {
            old_start: 1,
            new_start: 1,
            lines: vec![HunkLine::Del("a".into()), HunkLine::Ins("A".into())],
            new_no_eol: false,
        }];
        assert_eq!(apply_hunks(original, &hunks).unwrap(), "A\r\nb\r\n");
        assert_eq!(match_line_endings("x\ny\n", "p\r\nq\r\n"), "x\r\ny\r\n");
    }

    #[test]
    fn safe_join_refuses_escapes() {
        let root = Path::new("C:\\proj");
        assert!(safe_join(root, "src/ok.rs").is_ok());
        assert!(safe_join(root, "./src/ok.rs").is_ok());
        assert!(safe_join(root, "../evil.rs").is_err());
        assert!(safe_join(root, "src/../../evil.rs").is_err());
        assert!(safe_join(root, "C:/evil.rs").is_err());
        assert!(safe_join(root, "C:\\evil.rs").is_err());
        assert!(safe_join(root, "/etc/passwd").is_err());
        assert!(safe_join(root, "\\\\server\\share").is_err());
        assert!(safe_join(root, "file.txt:stream").is_err());
        assert!(safe_join(root, "").is_err());
    }

    #[test]
    fn hunk_header_parses_counts() {
        assert_eq!(hunk_header("@@ -1,3 +1,4 @@"), Some((1, 3, 1, 4)));
        assert_eq!(hunk_header("@@ -5 +6 @@ fn ctx()"), Some((5, 1, 6, 1)));
        assert_eq!(hunk_header("not a hunk"), None);
    }

    #[test]
    fn content_hash_is_stable_and_sensitive() {
        assert_eq!(content_hash(b"abc"), content_hash(b"abc"));
        assert_ne!(content_hash(b"abc"), content_hash(b"abd"));
    }
}
