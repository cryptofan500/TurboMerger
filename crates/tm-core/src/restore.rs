//! Native restore of an archive twin (`merge --complete`, ADR 0020): every
//! file back at its relative path, byte for byte, from the Markdown output
//! alone — the CLI's `restore` (HANDOFF_S8 §6.10 item 3.5, §6.11 D).
//!
//! All or nothing. The archive parts are indexed first (streamed: offsets,
//! never whole files), every manifest entry is checked (a safe relative path,
//! its block present, no duplicate), and existing files are compared; then
//! every file is decoded into a temporary file beside its target and checked
//! against the manifest's size and SHA-256. Only when all of them pass are
//! they moved into place. Nothing is ever written outside `dest`, through a
//! symlink, or over a different existing file without `overwrite`.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

/// What `restore` may do besides writing new files.
#[derive(Debug, Default, Clone, Copy)]
pub struct RestoreOptions {
    /// Replace an existing file whose bytes differ (default: refuse).
    pub overwrite: bool,
    /// Decode and check every file, write nothing.
    pub check_only: bool,
}

/// What a restore did (or, with `check_only`, would do).
#[derive(Debug, Default)]
pub struct RestoreReport {
    /// Files written (or verified, with `check_only`).
    pub restored: Vec<String>,
    /// Files already present with the manifest's exact bytes (left alone).
    pub identical: Vec<String>,
    /// Entries the archive itself marks as not restorable (withheld, missing).
    pub not_restorable: Vec<(String, String)>,
    /// Existing files with other bytes; nothing was written because of them.
    pub conflicts: Vec<String>,
    /// Names another system may refuse (case-only twins, Windows-reserved).
    pub warnings: Vec<String>,
    /// Bytes decoded.
    pub bytes: u64,
}

/// One manifest line (`## __manifest__`).
struct Entry {
    path: String,
    kind: String,
    bytes: u64,
    sha256: String,
    eol: String,
    final_newline: bool,
    mode: u32,
    reason: String,
}

/// A part's header line in its manifest (`{"archive":"tm-twin/1",…}`).
struct PartHeader {
    set: String,
    part: u64,
    parts: u64,
    files: u64,
}

/// Where a fenced block's content sits: (input index, start, end) in bytes.
type Span = (usize, u64, u64);

/// Every block of the archive parts, by kind.
#[derive(Default)]
struct Index {
    entries: Vec<Entry>,
    /// Part headers, by input.
    headers: Vec<(usize, PartHeader)>,
    /// `## <header>` blocks; a later block with the same header wins (a
    /// file named like the tree heading comes after the tree).
    text: HashMap<String, Span>,
    /// `## <header> (piece k/n)` blocks of a file split to fit --max-tokens:
    /// (k, n, ends inside a line, span).
    pieces: HashMap<String, Vec<(u32, u32, bool, Span)>>,
    /// `## __binary__: <header>` blocks.
    binary: HashMap<String, Span>,
    /// A second embedded block for the same path (checked after the parts).
    doubled: Vec<String>,
}

/// Restore every file of the archive `parts` (one Markdown output, or all
/// parts of a split one) under `dest`.
pub fn restore(
    parts: &[PathBuf],
    dest: &Path,
    opts: RestoreOptions,
) -> Result<RestoreReport, String> {
    let index = index_parts(parts)?;
    if index.entries.is_empty() && index.headers.is_empty() {
        return Err(
            "no `## __manifest__` block: not an archive twin (merge with --complete)".into(),
        );
    }
    let mut report = RestoreReport::default();
    check_part_set(parts, &index, &mut report)?;
    if let Some(name) = index.doubled.first() {
        return Err(format!(
            "two embedded blocks for {name}; nothing was written"
        ));
    }

    // Check every entry before anything is written.
    let mut seen: HashMap<&str, ()> = HashMap::new();
    let mut lower: BTreeMap<String, &str> = BTreeMap::new();
    let mut work: Vec<(&Entry, Vec<Span>, bool)> = Vec::new(); // (entry, blocks, is_text)
    for e in &index.entries {
        safe_relative(&e.path).map_err(|why| format!("{}: {why} — nothing was written", e.path))?;
        if seen.insert(&e.path, ()).is_some() {
            return Err(format!(
                "{}: listed twice in the manifest — nothing was written",
                e.path
            ));
        }
        if let Some(other) = lower.insert(e.path.to_lowercase(), &e.path) {
            report.warnings.push(format!(
                "{} and {other} differ only in case: one overwrites the other on a case-insensitive file system",
                e.path
            ));
        }
        if let Some(why) = windows_reserved(&e.path) {
            report.warnings.push(format!("{}: {why}", e.path));
        }
        let head = display_path(&e.path);
        match e.kind.as_str() {
            "withheld" | "missing" => {
                report
                    .not_restorable
                    .push((e.path.clone(), format!("{}: {}", e.kind, e.reason)));
            }
            "text" => match (index.text.get(&head), index.pieces.get(&head)) {
                (Some(&span), _) => work.push((e, vec![span], true)),
                (None, Some(pieces)) => {
                    let spans = joined_pieces(pieces)
                        .map_err(|why| format!("{}: {why} — nothing was written", e.path))?;
                    work.push((e, spans, true));
                }
                (None, None) => {
                    return Err(format!(
                        "{}: its text block is not in these parts — nothing was written",
                        e.path
                    ))
                }
            },
            "binary" => match index.binary.get(&head) {
                Some(&span) => work.push((e, vec![span], false)),
                None => {
                    return Err(format!(
                        "{}: its embedded bytes are not in these parts — nothing was written",
                        e.path
                    ))
                }
            },
            other => {
                return Err(format!(
                    "{}: unknown kind {other:?} — nothing was written",
                    e.path
                ))
            }
        }
    }

    // What is already there: identical files stay, different ones conflict.
    let mut todo: Vec<(&Entry, Vec<Span>, bool, PathBuf)> = Vec::new();
    for (e, span, is_text) in work {
        let target = dest.join(native(&e.path));
        if let Some(bad) = symlink_on_the_way(dest, &e.path) {
            return Err(format!(
                "{}: {} is a symlink; restore never writes through one — nothing was written",
                e.path,
                bad.display()
            ));
        }
        match std::fs::symlink_metadata(&target) {
            Ok(meta) if meta.is_file() => {
                if file_matches(&target, e.bytes, &e.sha256) {
                    report.identical.push(e.path.clone());
                } else if opts.overwrite {
                    todo.push((e, span, is_text, target));
                } else {
                    report.conflicts.push(e.path.clone());
                }
            }
            Ok(_) => report.conflicts.push(e.path.clone()),
            Err(_) => todo.push((e, span, is_text, target)),
        }
    }
    if !report.conflicts.is_empty() {
        return Ok(report);
    }

    // Decode everything into temporary files, check each, then move them in.
    if !opts.check_only {
        std::fs::create_dir_all(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    }
    let mut readers: Vec<File> = Vec::with_capacity(parts.len());
    for p in parts {
        readers.push(File::open(p).map_err(|e| format!("{}: {e}", p.display()))?);
    }
    let mut staged: Vec<(PathBuf, PathBuf, u32)> = Vec::new();
    let mut made_dirs: Vec<PathBuf> = Vec::new();
    let result = (|| -> Result<(), String> {
        for (e, spans, is_text, target) in &todo {
            let mut out: Box<dyn Write> = if opts.check_only {
                Box::new(std::io::sink())
            } else {
                let parent = target.parent().unwrap_or(dest);
                create_dirs(dest, parent, &mut made_dirs)
                    .map_err(|err| format!("{}: {err}", e.path))?;
                let tmp = parent.join(format!(
                    ".{}.tm-restore-{}.tmp",
                    target
                        .file_name()
                        .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
                    std::process::id()
                ));
                let file = File::create(&tmp).map_err(|err| format!("{}: {err}", e.path))?;
                staged.push((tmp, target.clone(), e.mode));
                Box::new(std::io::BufWriter::new(file))
            };
            let mut hashing = Hashing::new(&mut out);
            let mut src = BufReader::with_capacity(1 << 20, Spans::new(&mut readers, spans));
            let decoded = if *is_text {
                rebuild_text(&mut src, &e.eol, e.final_newline, &mut hashing)
            } else {
                decode_base64(&mut src, &mut hashing)
            };
            decoded.map_err(|err| format!("{}: {err}", e.path))?;
            let (n, sha) = hashing.finish();
            out.flush().map_err(|err| format!("{}: {err}", e.path))?;
            drop(out);
            if n != e.bytes || sha != e.sha256 {
                return Err(format!(
                    "{}: decoded {n} B sha256 {} ≠ manifest {} B {} (archive changed or damaged) — nothing was written",
                    e.path,
                    &sha[..12],
                    e.bytes,
                    e.sha256.get(..12).unwrap_or(&e.sha256)
                ));
            }
            report.bytes += n;
            report.restored.push(e.path.clone());
        }
        for (tmp, target, mode) in &staged {
            set_mode(tmp, *mode);
            replace(tmp, target).map_err(|err| format!("{}: {err}", target.display()))?;
        }
        Ok(())
    })();
    if let Err(e) = result {
        for (tmp, _, _) in &staged {
            let _ = std::fs::remove_file(tmp);
        }
        for d in made_dirs.iter().rev() {
            let _ = std::fs::remove_dir(d);
        }
        return Err(e);
    }
    Ok(report)
}

/// Every part of one merge, each once, and every file listed: a missing,
/// doubled or foreign part stops the restore before anything is written.
fn check_part_set(
    parts: &[PathBuf],
    index: &Index,
    report: &mut RestoreReport,
) -> Result<(), String> {
    if index.headers.is_empty() {
        report.warnings.push(
            "the archive has no part header (made before 2026-10-31 S9): a missing part cannot be detected".into(),
        );
        return Ok(());
    }
    for (pi, p) in parts.iter().enumerate() {
        match index.headers.iter().filter(|(i, _)| *i == pi).count() {
            1 => {}
            0 => {
                return Err(format!(
                    "{}: no part header — not a part of this archive; nothing was written",
                    p.display()
                ))
            }
            _ => {
                return Err(format!(
                    "{}: two part headers; nothing was written",
                    p.display()
                ))
            }
        }
    }
    let first = &index.headers[0].1;
    if index
        .headers
        .iter()
        .any(|(_, h)| h.set != first.set || h.parts != first.parts)
    {
        return Err("these parts come from different merges; nothing was written".into());
    }
    let numbers: Vec<u64> = index.headers.iter().map(|(_, h)| h.part).collect();
    for k in 1..=first.parts {
        match numbers.iter().filter(|&&n| n == k).count() {
            1 => {}
            0 => {
                return Err(format!(
                    "part {k} of {} is missing; nothing was written",
                    first.parts
                ))
            }
            _ => {
                return Err(format!(
                    "part {k} of {} is given twice; nothing was written",
                    first.parts
                ))
            }
        }
    }
    if numbers.len() as u64 != first.parts {
        return Err(format!(
            "{} parts given for a set of {}; nothing was written",
            numbers.len(),
            first.parts
        ));
    }
    if index.entries.len() as u64 != first.files {
        return Err(format!(
            "the manifests list {} files, the part headers {}; nothing was written",
            index.entries.len(),
            first.files
        ));
    }
    Ok(())
}

/// Index every part: manifests parsed, every other block's content located.
fn index_parts(parts: &[PathBuf]) -> Result<Index, String> {
    let mut index = Index::default();
    for (pi, p) in parts.iter().enumerate() {
        let file = File::open(p).map_err(|e| format!("{}: {e}", p.display()))?;
        let mut r = BufReader::with_capacity(1 << 20, file);
        let mut line: Vec<u8> = Vec::new();
        let mut pos = 0u64;
        // The last `## ` header and the state after it.
        let mut header: Option<String> = None;
        let mut blank_seen = false;
        loop {
            line.clear();
            let n = r
                .read_until(b'\n', &mut line)
                .map_err(|e| format!("{}: {e}", p.display()))?;
            if n == 0 {
                break;
            }
            pos += n as u64;
            let text = trim_eol(&line);
            if let Some(h) = text.strip_prefix(b"## ") {
                header = Some(String::from_utf8_lossy(h).into_owned());
                blank_seen = false;
                continue;
            }
            match (&header, blank_seen) {
                (Some(_), false) if text.is_empty() => blank_seen = true,
                (Some(h), true) if text.starts_with(b"```") => {
                    let fence: Vec<u8> = text.iter().take_while(|&&c| c == b'`').copied().collect();
                    let start = pos;
                    let mut end = None;
                    loop {
                        line.clear();
                        let n = r
                            .read_until(b'\n', &mut line)
                            .map_err(|e| format!("{}: {e}", p.display()))?;
                        if n == 0 {
                            break;
                        }
                        if trim_eol(&line) == fence.as_slice() {
                            end = Some(pos);
                            pos += n as u64;
                            break;
                        }
                        pos += n as u64;
                    }
                    let Some(end) = end else {
                        return Err(format!(
                            "{}: block `{h}` has no closing fence (cut off?)",
                            p.display()
                        ));
                    };
                    let span = (pi, start, end);
                    if h == "__manifest__" {
                        read_manifest(p, span, &mut index)?;
                    } else if let Some(name) = h.strip_prefix("__binary__: ") {
                        if index.binary.insert(name.to_string(), span).is_some() {
                            index.doubled.push(name.to_string());
                        }
                    } else {
                        match piece_of(h) {
                            Some((base, k, n, cont)) => index
                                .pieces
                                .entry(base.to_string())
                                .or_default()
                                .push((k, n, cont, span)),
                            None => {
                                index.text.insert(h.clone(), span);
                            }
                        }
                    }
                    header = None;
                }
                _ => header = None,
            }
        }
    }
    Ok(index)
}

fn trim_eol(line: &[u8]) -> &[u8] {
    let l = line.strip_suffix(b"\n").unwrap_or(line);
    l.strip_suffix(b"\r").unwrap_or(l)
}

fn read_manifest(p: &Path, (pi, start, end): Span, index: &mut Index) -> Result<(), String> {
    let mut f = File::open(p).map_err(|e| format!("{}: {e}", p.display()))?;
    f.seek(SeekFrom::Start(start)).map_err(|e| e.to_string())?;
    let r = BufReader::new(f.take(end - start));
    for line in r.lines() {
        let line = line.map_err(|e| format!("{}: manifest: {e}", p.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(&line)
            .map_err(|e| format!("{}: manifest line {line:?}: {e}", p.display()))?;
        let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
        if v.get("archive").is_some() {
            index.headers.push((
                pi,
                PartHeader {
                    set: s("set"),
                    part: v["part"].as_u64().unwrap_or(0),
                    parts: v["parts"].as_u64().unwrap_or(0),
                    files: v["files"].as_u64().unwrap_or(0),
                },
            ));
            continue;
        }
        let path = s("path");
        if path.is_empty() {
            return Err(format!(
                "{}: manifest line without a path: {line}",
                p.display()
            ));
        }
        index.entries.push(Entry {
            path,
            kind: s("kind"),
            bytes: v["bytes"].as_u64().unwrap_or(0),
            sha256: s("sha256"),
            eol: s("eol"),
            final_newline: v["final_newline"].as_bool().unwrap_or(false),
            mode: v["mode"].as_u64().unwrap_or(0) as u32,
            reason: s("reason"),
        });
    }
    Ok(())
}

/// A manifest path is relative, plain and inside `dest`: no root, drive,
/// `..`, `.`, empty part, backslash or NUL.
fn safe_relative(rel: &str) -> Result<(), &'static str> {
    if rel.contains('\0') {
        return Err("NUL in the path");
    }
    if rel.contains('\\') {
        return Err("backslash in the path");
    }
    if rel.len() >= 2 && rel.as_bytes()[1] == b':' {
        return Err("drive-letter path");
    }
    if rel.starts_with('/') {
        return Err("absolute path");
    }
    for part in rel.split('/') {
        match part {
            "" => return Err("empty path part"),
            "." | ".." => return Err("`.` or `..` in the path"),
            _ => {}
        }
    }
    if Path::new(rel)
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err("not a plain relative path");
    }
    Ok(())
}

fn native(rel: &str) -> PathBuf {
    rel.split('/').collect()
}

/// The first existing component under `dest` on the way to `rel` that is a
/// symlink (restore never writes through one).
fn symlink_on_the_way(dest: &Path, rel: &str) -> Option<PathBuf> {
    let mut p = dest.to_path_buf();
    for part in rel.split('/') {
        p.push(part);
        match std::fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => return Some(p),
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    None
}

/// Create `dir` (under `dest`) and its missing parents, recording each one
/// made so a failed restore can remove them again.
fn create_dirs(dest: &Path, dir: &Path, made: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    let mut missing: Vec<&Path> = Vec::new();
    let mut d = dir;
    while !d.exists() && d.starts_with(dest) && d != dest {
        missing.push(d);
        match d.parent() {
            Some(p) => d = p,
            None => break,
        }
    }
    for m in missing.into_iter().rev() {
        std::fs::create_dir(m)?;
        made.push(m.to_path_buf());
    }
    if !dir.is_dir() {
        std::fs::create_dir_all(dir)?;
    }
    Ok(())
}

fn file_matches(path: &Path, bytes: u64, sha: &str) -> bool {
    let Ok(mut f) = File::open(path) else {
        return false;
    };
    if f.metadata().map(|m| m.len()).ok() != Some(bytes) {
        return false;
    }
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => h.update(&buf[..n]),
            Err(_) => return false,
        }
    }
    format!("{:x}", h.finalize()) == sha
}

/// Move `tmp` onto `target` (replacing it).
fn replace(tmp: &Path, target: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    if target.exists() {
        std::fs::remove_file(target)?;
    }
    std::fs::rename(tmp, target)
}

fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    if mode != 0 {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o777));
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
}

/// Windows refuses these names; a warning elsewhere.
fn windows_reserved(rel: &str) -> Option<String> {
    for part in rel.split('/') {
        let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.as_bytes()[3].is_ascii_digit()
                && stem.as_bytes()[3] != b'0');
        if reserved {
            return Some(format!("`{part}` is a reserved name on Windows"));
        }
        if part.ends_with('.') || part.ends_with(' ') {
            return Some(format!(
                "`{part}` ends in a dot or space (Windows drops it)"
            ));
        }
        if part
            .chars()
            .any(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
        {
            return Some(format!("`{part}` has a character Windows refuses"));
        }
    }
    None
}

/// The block-header form of a path (render.rs: control characters escaped).
fn display_path(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() {
                format!("\\u{{{:x}}}", c as u32)
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// A writer that counts and hashes what passes through.
struct Hashing<'a> {
    inner: &'a mut dyn Write,
    sha: Sha256,
    n: u64,
}

impl<'a> Hashing<'a> {
    fn new(inner: &'a mut dyn Write) -> Self {
        Hashing {
            inner,
            sha: Sha256::new(),
            n: 0,
        }
    }
    fn finish(self) -> (u64, String) {
        (self.n, format!("{:x}", self.sha.finalize()))
    }
}

impl Write for Hashing<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let k = self.inner.write(buf)?;
        self.sha.update(&buf[..k]);
        self.n += k as u64;
        Ok(k)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// `header` = `<path> (piece k/n)` or `<path> (piece k/n, line continues)`.
fn piece_of(header: &str) -> Option<(&str, u32, u32, bool)> {
    let rest = header.strip_suffix(')')?;
    let at = rest.rfind(" (piece ")?;
    let (base, tail) = (&rest[..at], &rest[at + " (piece ".len()..]);
    let (nums, cont) = match tail.strip_suffix(", line continues") {
        Some(t) => (t, true),
        None => (tail, false),
    };
    let (k, n) = nums.split_once('/')?;
    Some((base, k.parse().ok()?, n.parse().ok()?, cont))
}

/// The spans of a split file's pieces in order — every piece once, all of
/// one split. A piece that ends inside a line loses the newline its fence
/// added, so the pieces read on as one text.
fn joined_pieces(pieces: &[(u32, u32, bool, Span)]) -> Result<Vec<Span>, String> {
    let n = pieces[0].1;
    let mut spans = Vec::with_capacity(n as usize);
    for k in 1..=n {
        let mut found = pieces.iter().filter(|p| p.0 == k);
        let (Some(&(_, pn, cont, (part, start, end))), None) = (found.next(), found.next()) else {
            return Err(format!("piece {k}/{n} is missing or given twice"));
        };
        if pn != n {
            return Err(format!("pieces of two different splits ({pn} and {n})"));
        }
        spans.push((part, start, if cont { end.saturating_sub(1) } else { end }));
    }
    if pieces.len() as u32 != n {
        return Err(format!("{} pieces for a split into {n}", pieces.len()));
    }
    Ok(spans)
}

/// The content of one or more block spans, in order, as one stream.
struct Spans<'a> {
    files: &'a mut [File],
    spans: &'a [Span],
    next: usize,
    /// The span being read: its file and the bytes left in it.
    cur: Option<(usize, u64)>,
}

impl<'a> Spans<'a> {
    fn new(files: &'a mut [File], spans: &'a [Span]) -> Self {
        Spans {
            files,
            spans,
            next: 0,
            cur: None,
        }
    }
}

impl Read for Spans<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if let Some((part, left)) = self.cur.filter(|&(_, left)| left > 0) {
                let want = buf.len().min(left as usize);
                let n = self.files[part].read(&mut buf[..want])?;
                if n == 0 {
                    return Err(std::io::ErrorKind::UnexpectedEof.into());
                }
                self.cur = Some((part, left - n as u64));
                return Ok(n);
            }
            let Some(&(part, start, end)) = self.spans.get(self.next) else {
                return Ok(0);
            };
            self.next += 1;
            self.files[part].seek(SeekFrom::Start(start))?;
            self.cur = Some((part, end - start));
        }
    }
}

/// A text block back to the file (the rules of `render::rebuilt_from_block`
/// and `tm_verify.py restore`): the fence's own last newline is not part of
/// the file, `eol` and `final_newline` come from the manifest.
fn rebuild_text(
    r: &mut impl BufRead,
    eol: &str,
    final_newline: bool,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    let crlf = eol == "crlf";
    let sep: &[u8] = if crlf { b"\r\n" } else { b"\n" };
    let mut line: Vec<u8> = Vec::new();
    let mut first = true;
    loop {
        line.clear();
        if r.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let mut l: &[u8] = line.strip_suffix(b"\n").unwrap_or(&line);
        if crlf {
            l = l.strip_suffix(b"\r").unwrap_or(l);
        }
        if !first {
            out.write_all(sep)?;
        }
        out.write_all(l)?;
        first = false;
    }
    if final_newline && !first {
        out.write_all(sep)?;
    }
    Ok(())
}

/// A `base64` block back to bytes, line by line.
fn decode_base64(r: &mut impl BufRead, out: &mut dyn Write) -> std::io::Result<()> {
    let bad = |what: &str| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("bad base64: {what}"),
        )
    };
    let mut line: Vec<u8> = Vec::new();
    // Digits carried to the next line when a line is not a multiple of 4.
    let mut carry: Vec<u8> = Vec::new();
    let mut ended = false;
    loop {
        line.clear();
        if r.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        for &c in trim_eol(&line) {
            if c == b'=' {
                ended = true;
                carry.push(c);
                continue;
            }
            if ended {
                return Err(bad("data after padding"));
            }
            carry.push(c);
        }
        let whole = carry.len() / 4 * 4;
        let decoded = b64_quads(&carry[..whole]).ok_or_else(|| bad("not a base64 character"))?;
        out.write_all(&decoded)?;
        carry.drain(..whole);
    }
    if !carry.is_empty() {
        return Err(bad("length is not a multiple of 4"));
    }
    Ok(())
}

fn b64_quads(digits: &[u8]) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(u32::from(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        }))
    };
    let mut out = Vec::with_capacity(digits.len() / 4 * 3);
    for q in digits.chunks(4) {
        let pad = q.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || q[..4 - pad].contains(&b'=') {
            return None;
        }
        let mut acc = 0u32;
        for (i, &c) in q[..4 - pad].iter().enumerate() {
            acc |= val(c)? << (18 - 6 * i);
        }
        out.push((acc >> 16) as u8);
        if pad < 2 {
            out.push((acc >> 8) as u8);
        }
        if pad < 1 {
            out.push(acc as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_paths_must_stay_inside_dest() {
        for bad in [
            "/etc/passwd",
            "../x",
            "a/../../x",
            "a//b",
            "./a",
            "C:/x",
            "a\\b",
            "a/\0",
            "",
        ] {
            assert!(safe_relative(bad).is_err(), "{bad:?} accepted");
        }
        for good in ["a.txt", "dir/sub/ü file.md", "..hidden", "a..b/c"] {
            assert!(safe_relative(good).is_ok(), "{good:?} refused");
        }
    }

    #[test]
    fn base64_decodes_like_the_encoder_writes() {
        for len in 0..200usize {
            let data: Vec<u8> = (0..len).map(|i| (i * 37 % 256) as u8).collect();
            let enc = crate::remote::base64(&data);
            let mut digits: Vec<u8> = enc.into_bytes();
            assert_eq!(digits.len() % 4, 0);
            let out = b64_quads(&std::mem::take(&mut digits)).unwrap();
            assert_eq!(out, data, "len {len}");
        }
        assert!(b64_quads(b"ab$d").is_none());
        assert!(b64_quads(b"a=bc").is_none());
    }

    #[test]
    fn windows_names_are_flagged() {
        assert!(windows_reserved("a/CON.txt").is_some());
        assert!(windows_reserved("lpt1").is_some());
        assert!(windows_reserved("dir./x").is_some());
        assert!(windows_reserved("a?.md").is_some());
        assert!(windows_reserved("console.txt").is_none());
        assert!(windows_reserved("com0.txt").is_none());
    }
}
