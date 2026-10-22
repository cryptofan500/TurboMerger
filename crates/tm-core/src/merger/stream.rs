//! Bounded-memory text streaming, so no input is too big to merge: plain
//! files above the parallel path's budget (any size — there is no cap by
//! default), gzip-compressed text, and the text members of zip and tar
//! archives. A source is decoded with a streaming decoder (a multi-byte
//! character split across two reads stays whole), cut into segments of at
//! most `SEGMENT_BYTES` at line ends, and segments are redacted and counted
//! a few at a time side by side (P9: counting is the slow part), then spooled
//! in order before more is read, so memory stays at a few segments whatever
//! the size. The first segment of a file
//! carries its path; later ones are labelled continuations that the
//! token-budget splitter can place in later parts.

use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use encoding_rs::{CoderResult, Decoder, Encoding};

use super::{Block, MergeConfig};

/// Plain files above this are streamed (in order, by the merge's own thread,
/// while the workers go on with the files after it) instead of being read
/// whole.
pub(super) const STREAM_ABOVE: u64 = 64 << 20;
/// Gzip files that inflate (per their trailer) to more than this are
/// streamed; smaller ones are inflated inline in the parallel path.
pub(super) const GZIP_INLINE_MAX: u64 = 8 << 20;
/// Decoded text per segment handed to redaction, counting and the spool.
const SEGMENT_BYTES: usize = 16 << 20;
/// Raw bytes read per step (also the unit of the mid-file binary check).
const READ_STEP: usize = 1 << 20;
/// Bytes inspected before encoding and text-vs-binary are decided.
const PROBE_BYTES: usize = 64 << 10;

/// A compressed or archived source, recognised by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Container {
    Gzip,
    Zip,
    Tar,
    TarGz,
}

/// `notes.txt.gz` → Gzip, `x.tar.gz`/`x.tgz` → TarGz, `x.tar` → Tar,
/// `x.zip` → Zip; anything else is not a container.
pub(super) fn container_of(name: &str) -> Option<Container> {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        Some(Container::TarGz)
    } else if lower.ends_with(".gz") {
        Some(Container::Gzip)
    } else if lower.ends_with(".tar") {
        Some(Container::Tar)
    } else if lower.ends_with(".zip") {
        Some(Container::Zip)
    } else {
        None
    }
}

/// The inflated size a gzip file declares in its trailer (modulo 4 GiB, so
/// only a hint), or `None` when it cannot be read.
pub(super) fn gzip_inflated_size(path: &Path) -> Option<u64> {
    use std::io::{Seek, SeekFrom};
    let mut f = File::open(path).ok()?;
    if f.metadata().ok()?.len() < 18 {
        return None;
    }
    f.seek(SeekFrom::End(-4)).ok()?;
    let mut b = [0u8; 4];
    f.read_exact(&mut b).ok()?;
    Some(u32::from_le_bytes(b) as u64)
}

/// What streaming one source produced, beyond the blocks it emitted.
#[derive(Default)]
pub(super) struct Streamed {
    /// Labeled secret values seen (for repo-wide propagation).
    pub(super) harvested: Vec<String>,
    /// Blocks emitted.
    pub(super) blocks: usize,
    /// Notes for the Merge Report (encoding, parts, a binary tail, skipped
    /// archive members).
    pub(super) notes: Vec<String>,
}

/// Where the streaming path sends its output and reports progress.
pub(super) struct Sink<'a> {
    /// Takes one finished block and its text (the spool append).
    pub(super) emit: &'a mut dyn FnMut(Block, &str) -> io::Result<()>,
    /// Raw bytes consumed so far for the current source (progress and ETA).
    pub(super) progress: &'a mut dyn FnMut(u64),
    pub(super) cancel: &'a AtomicBool,
}

/// Stream one file: plain text, gzip, or an archive's text members.
/// `Err` carries the reason the whole source yielded no text.
pub(super) fn stream_source(
    path: &Path,
    relative: &str,
    container: Option<Container>,
    cfg: &MergeConfig,
    sink: &mut Sink,
) -> Result<Streamed, String> {
    let file = File::open(path).map_err(|e| format!("unreadable: {e}"))?;
    let reader = BufReader::with_capacity(READ_STEP, file);
    let lang = Path::new(relative)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mut out = Streamed::default();
    match container {
        None => {
            stream_text(reader, relative, false, &lang, cfg, sink, &mut out)?;
        }
        Some(Container::Gzip) => {
            let inner = Path::new(relative.trim_end_matches(".gz").trim_end_matches(".GZ"))
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let gz = flate2::read::MultiGzDecoder::new(reader);
            stream_text(gz, relative, false, &inner, cfg, sink, &mut out)
                .map_err(|e| format!("{e} (inside gzip)"))?;
            out.notes.push("inflated from gzip".into());
        }
        Some(Container::Zip) => stream_zip(reader, relative, cfg, sink, &mut out)?,
        Some(Container::Tar) => stream_tar(reader, relative, cfg, sink, &mut out)?,
        Some(Container::TarGz) => {
            let gz = flate2::read::MultiGzDecoder::new(reader);
            stream_tar(gz, relative, cfg, sink, &mut out)?;
        }
    }
    Ok(out)
}

/// Decode, segment, redact, count and emit one text stream. `member` marks an
/// archive member (every block synthetic: its path is not a file on disk).
#[allow(clippy::too_many_arguments)]
fn stream_text<R: Read>(
    mut reader: R,
    label: &str,
    member: bool,
    lang: &str,
    cfg: &MergeConfig,
    sink: &mut Sink,
    out: &mut Streamed,
) -> Result<(), String> {
    let mut probe = Vec::with_capacity(PROBE_BYTES);
    (&mut reader)
        .take(PROBE_BYTES as u64)
        .read_to_end(&mut probe)
        .map_err(|e| format!("unreadable: {e}"))?;
    if probe.is_empty() {
        return Err("empty".into());
    }
    let (enc, bom_len, enc_note) = detect(&probe)?;
    if let Some(n) = enc_note {
        out.notes.push(n);
    }
    let utf16 = enc == encoding_rs::UTF_16LE || enc == encoding_rs::UTF_16BE;
    let mut decoder = enc.new_decoder_without_bom_handling();
    let mut pending = String::new();
    let mut part = 0usize;
    let mut consumed = probe.len() as u64;
    let mut replaced = decode_into(&mut decoder, &probe[bom_len..], &mut pending, false);
    let mut seg = Segmenter {
        label,
        member,
        lang,
        batch: Vec::new(),
    };
    let mut buf = vec![0u8; READ_STEP];
    loop {
        if sink.cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let n = read_some(&mut reader, &mut buf).map_err(|e| format!("read failed: {e}"))?;
        if n == 0 {
            break;
        }
        consumed += n as u64;
        // A text stream that turns into binary (a log with a core dump
        // appended, a mislabelled blob) stops here; what was read stays.
        if !utf16 && buf[..n].iter().filter(|&&b| b == 0).count() > n / 20 {
            out.notes.push(format!(
                "binary data from about byte {} — the rest was not read",
                consumed - n as u64
            ));
            break;
        }
        replaced |= decode_into(&mut decoder, &buf[..n], &mut pending, false);
        (sink.progress)(consumed);
        if cfg.max_file_size != 0 && consumed > cfg.max_file_size {
            out.notes.push(format!(
                "stopped at the size cap ({} MB)",
                cfg.max_file_size >> 20
            ));
            break;
        }
        seg.emit_ready(&mut pending, false, &mut part, cfg, sink, out)?;
    }
    replaced |= decode_into(&mut decoder, &[], &mut pending, true);
    seg.emit_ready(&mut pending, true, &mut part, cfg, sink, out)?;
    if replaced {
        out.notes
            .push("some bytes were not valid in the detected encoding (shown as U+FFFD)".into());
    }
    if part > 1 {
        out.notes.push(format!("streamed in {part} parts"));
    }
    Ok(())
}

/// Labels and language shared by every segment of one stream, and the cut
/// segments not yet emitted.
struct Segmenter<'a> {
    label: &'a str,
    member: bool,
    lang: &'a str,
    /// `(text, part)`: cut, waiting to be slimmed, redacted and counted
    /// together with the next ones.
    batch: Vec<(String, usize)>,
}

impl Segmenter<'_> {
    /// Emit every full segment in `pending` (and the remainder when `last`),
    /// cut at the last line end inside the segment budget. Segments are
    /// processed [`parallel_segments`] at a time and emitted in order.
    fn emit_ready(
        &mut self,
        pending: &mut String,
        last: bool,
        part: &mut usize,
        cfg: &MergeConfig,
        sink: &mut Sink,
        out: &mut Streamed,
    ) -> Result<(), String> {
        while pending.len() >= SEGMENT_BYTES || (last && !pending.is_empty()) {
            let cut = if pending.len() >= SEGMENT_BYTES {
                let mut limit = SEGMENT_BYTES;
                while !pending.is_char_boundary(limit) {
                    limit -= 1;
                }
                match pending[..limit].rfind('\n') {
                    Some(i) if i > 0 => i + 1,
                    _ => limit,
                }
            } else {
                pending.len()
            };
            let rest = pending.split_off(cut);
            let segment = std::mem::replace(pending, rest);
            *part += 1;
            self.batch.push((segment, *part));
            if self.batch.len() >= parallel_segments() {
                self.flush(cfg, sink, out)?;
            }
        }
        if last {
            self.flush(cfg, sink, out)?;
        }
        Ok(())
    }

    /// Slim, redact and count the cut segments side by side, then emit them
    /// in order. Counting tokens is the slow part of a big text file (about
    /// 13 MB/s on one core), and segments are independent. Plain threads,
    /// not the worker pool: the merge's workers may all be waiting for this
    /// very file to be written.
    fn flush(
        &mut self,
        cfg: &MergeConfig,
        sink: &mut Sink,
        out: &mut Streamed,
    ) -> Result<(), String> {
        let batch = std::mem::take(&mut self.batch);
        let ready: Vec<(Prepared, usize)> = if batch.len() == 1 {
            batch
                .into_iter()
                .map(|(text, part)| (prepare_segment(text, cfg), part))
                .collect()
        } else {
            std::thread::scope(|s| {
                let handles: Vec<_> = batch
                    .into_iter()
                    .map(|(text, part)| {
                        s.spawn(move || {
                            crate::threads::lower_current_thread_priority();
                            (prepare_segment(text, cfg), part)
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().unwrap_or_else(|p| std::panic::resume_unwind(p)))
                    .collect()
            })
        };
        for (prepared, part) in ready {
            emit_segment(
                prepared,
                self.label,
                self.member,
                part,
                self.lang,
                sink,
                out,
            )?;
        }
        Ok(())
    }
}

/// Segments processed at once: the worker pool's size (`--threads`), at
/// most 8 (8 x 16 MiB of text in memory).
fn parallel_segments() -> usize {
    rayon::current_num_threads().clamp(1, 8)
}

/// A segment ready to emit: its text after slimming and redaction, what was
/// redacted, the labeled secrets it held, and its token count.
struct Prepared {
    text: String,
    redactions: Vec<(&'static str, usize)>,
    harvested: Vec<String>,
    tokens: usize,
}

fn prepare_segment(mut text: String, cfg: &MergeConfig) -> Prepared {
    if cfg.reproducible && text.contains('\r') {
        text = text.replace("\r\n", "\n");
    }
    if cfg.truncate_base64 {
        text = super::process::truncate_base64(&text);
    }
    if cfg.remove_empty_lines {
        text = super::process::drop_empty_lines(&text);
    }
    let mut harvested = Vec::new();
    let redactions = if cfg.redact {
        harvested = crate::security::harvest_labeled_values(&text);
        let (clean, events) = crate::security::redact_secrets(&text);
        text = clean;
        events.into_iter().map(|ev| (ev.rule, ev.count)).collect()
    } else {
        Vec::new()
    };
    let tokens = crate::cache::count(&text);
    Prepared {
        text,
        redactions,
        harvested,
        tokens,
    }
}

/// Encoding and BOM length for a probe, or why it is not text.
fn detect(probe: &[u8]) -> Result<(&'static Encoding, usize, Option<String>), String> {
    if let Some((enc, bom)) = Encoding::for_bom(probe) {
        let note =
            (enc != encoding_rs::UTF_8).then(|| format!("decoded from {} (BOM)", enc.name()));
        return Ok((enc, bom, note));
    }
    if let Some((enc, _)) = crate::security::utf16_text_encoding(probe) {
        return Ok((
            enc,
            0,
            Some(format!("decoded from {} (no byte-order mark)", enc.name())),
        ));
    }
    if crate::security::is_binary_content(&probe[..probe.len().min(8192)]) {
        return Err("binary content".into());
    }
    if crate::security::is_valid_text_window(probe) {
        return Ok((encoding_rs::UTF_8, 0, None));
    }
    let mut det = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Deny);
    det.feed(probe, false);
    let enc = det.guess(None, chardetng::Utf8Detection::Deny);
    Ok((
        enc,
        0,
        Some(format!("not UTF-8 — decoded as {} (detected)", enc.name())),
    ))
}

/// Decode `src` onto `out`; true when replacement characters were produced.
fn decode_into(decoder: &mut Decoder, mut src: &[u8], out: &mut String, last: bool) -> bool {
    let mut replaced = false;
    loop {
        let need = decoder
            .max_utf8_buffer_length(src.len())
            .unwrap_or(src.len() * 3 + 16);
        out.reserve(need);
        let (result, read, errors) = decoder.decode_to_string(src, out, last);
        replaced |= errors;
        src = &src[read..];
        if let CoderResult::InputEmpty = result {
            return replaced;
        }
    }
}

/// One `read` that retries on `Interrupted`.
fn read_some<R: Read>(reader: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        match reader.read(buf) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            other => return other,
        }
    }
}

/// Emit one prepared segment.
fn emit_segment(
    prepared: Prepared,
    label: &str,
    member: bool,
    part: usize,
    lang: &str,
    sink: &mut Sink,
    out: &mut Streamed,
) -> Result<(), String> {
    let Prepared {
        text,
        redactions,
        harvested,
        tokens,
    } = prepared;
    out.harvested.extend(harvested);
    let relative = if part == 1 {
        label.to_string()
    } else {
        format!("{label} (part {part})")
    };
    let block = Block {
        relative,
        span: Default::default(),
        lang: lang.to_string(),
        tokens,
        utf8_note: None,
        redactions,
        compressed: false,
        // Continuations and archive members are output, not files on disk:
        // the project tree lists the file once, from its first segment.
        synthetic: member || part > 1,
    };
    (sink.emit)(block, &text).map_err(|e| format!("spool write failed: {e}"))?;
    out.blocks += 1;
    Ok(())
}

/// Text members of a zip archive, in archive order. Documents, nested
/// archives and binary members are counted in a note, not merged.
fn stream_zip<R: Read + io::Seek>(
    reader: R,
    relative: &str,
    cfg: &MergeConfig,
    sink: &mut Sink,
    out: &mut Streamed,
) -> Result<(), String> {
    let mut zip = zip::ZipArchive::new(reader).map_err(|e| format!("not a readable zip: {e}"))?;
    let mut skipped = MemberTally::default();
    for i in 0..zip.len() {
        if sink.cancel.load(Ordering::Relaxed) {
            break;
        }
        let mut member = match zip.by_index(i) {
            Ok(m) => m,
            Err(_) => {
                skipped.unreadable += 1;
                continue;
            }
        };
        if member.is_dir() {
            continue;
        }
        let name = member.name().replace('\\', "/");
        stream_member(&mut member, relative, &name, cfg, sink, out, &mut skipped);
    }
    skipped.finish(relative, out)
}

/// Text members of a tar stream (plain or gzip): ustar/GNU headers, GNU long
/// names and pax `path=` records; links, devices and directories skipped.
fn stream_tar<R: Read>(
    mut reader: R,
    relative: &str,
    cfg: &MergeConfig,
    sink: &mut Sink,
    out: &mut Streamed,
) -> Result<(), String> {
    let mut skipped = MemberTally::default();
    let mut header = [0u8; 512];
    let mut long_name: Option<String> = None;
    loop {
        if sink.cancel.load(Ordering::Relaxed) {
            break;
        }
        match read_block(&mut reader, &mut header) {
            Ok(true) => {}
            Ok(false) => break,
            Err(e) => return Err(format!("not a readable tar: {e}")),
        }
        if header.iter().all(|&b| b == 0) {
            break;
        }
        let size = tar_size(&header[124..136]).ok_or("not a readable tar: bad size field")?;
        let padded = size.div_ceil(512) * 512;
        let name = long_name.take().unwrap_or_else(|| tar_name(&header));
        let io_err = |e: io::Error| format!("not a readable tar: {e}");
        match header[156] {
            b'0' | 0 | b'7' => {
                let mut body = (&mut reader).take(size);
                stream_member(&mut body, relative, &name, cfg, sink, out, &mut skipped);
                io::copy(&mut body, &mut io::sink()).map_err(io_err)?;
                skip_bytes(&mut reader, padded - size).map_err(io_err)?;
            }
            b'L' | b'x' => {
                let mut meta = Vec::new();
                (&mut reader)
                    .take(size)
                    .read_to_end(&mut meta)
                    .map_err(io_err)?;
                long_name = if header[156] == b'L' {
                    Some(cstr(&meta))
                } else {
                    pax_path(&meta)
                };
                skip_bytes(&mut reader, padded - size).map_err(io_err)?;
            }
            _ => skip_bytes(&mut reader, padded).map_err(io_err)?,
        }
    }
    skipped.finish(relative, out)
}

/// Members that were not merged, by reason.
#[derive(Default)]
struct MemberTally {
    merged: usize,
    binary: usize,
    documents: usize,
    unreadable: usize,
}

impl MemberTally {
    fn finish(self, relative: &str, out: &mut Streamed) -> Result<(), String> {
        let mut parts = Vec::new();
        if self.binary > 0 {
            parts.push(format!("{} binary", self.binary));
        }
        if self.documents > 0 {
            parts.push(format!("{} documents/archives", self.documents));
        }
        if self.unreadable > 0 {
            parts.push(format!("{} unreadable", self.unreadable));
        }
        if self.merged == 0 {
            return Err(if parts.is_empty() {
                "archive holds no text files".into()
            } else {
                format!(
                    "archive holds no text files ({} members not merged)",
                    parts.join(", ")
                )
            });
        }
        if !parts.is_empty() {
            out.notes.push(format!(
                "{relative}: {} text members merged; not merged: {}",
                self.merged,
                parts.join(", ")
            ));
        }
        Ok(())
    }
}

/// Merge one archive member if it is text.
fn stream_member(
    body: &mut dyn Read,
    relative: &str,
    name: &str,
    cfg: &MergeConfig,
    sink: &mut Sink,
    out: &mut Streamed,
    tally: &mut MemberTally,
) {
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if crate::extract::DocKind::from_ext(&ext).is_some() || container_of(name).is_some() {
        tally.documents += 1;
        return;
    }
    let label = format!("{relative}!/{name}");
    match stream_text(body, &label, true, &ext, cfg, sink, out) {
        Ok(()) => tally.merged += 1,
        Err(e) if e == "empty" => {}
        Err(e) if e.starts_with("binary") => tally.binary += 1,
        Err(_) => tally.unreadable += 1,
    }
}

/// Read exactly one 512-byte tar block; `false` at a clean end of stream.
fn read_block<R: Read>(reader: &mut R, block: &mut [u8; 512]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < 512 {
        match reader.read(&mut block[filled..]) {
            Ok(0) if filled == 0 => return Ok(false),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

fn skip_bytes<R: Read>(reader: &mut R, n: u64) -> io::Result<()> {
    io::copy(&mut reader.take(n), &mut io::sink()).map(|_| ())
}

/// The size field: octal text, or GNU base-256 for sizes past 8 GiB.
fn tar_size(field: &[u8]) -> Option<u64> {
    if field[0] & 0x80 != 0 {
        return Some(field[1..].iter().fold(0u64, |a, &b| (a << 8) | b as u64));
    }
    let text = std::str::from_utf8(field).ok()?;
    let digits = text.trim_matches(|c: char| c == '\0' || c == ' ');
    if digits.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(digits, 8).ok()
}

/// Name (with the ustar prefix when present).
fn tar_name(h: &[u8; 512]) -> String {
    let name = cstr(&h[0..100]);
    if &h[257..262] == b"ustar" {
        let prefix = cstr(&h[345..500]);
        if !prefix.is_empty() {
            return format!("{prefix}/{name}");
        }
    }
    name
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).trim_end().to_string()
}

/// `path=` from pax extended-header records (`<len> path=<value>\n`).
fn pax_path(meta: &[u8]) -> Option<String> {
    String::from_utf8_lossy(meta)
        .lines()
        .find_map(|l| l.split_once(" path=").map(|(_, v)| v.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_names() {
        assert_eq!(container_of("a/notes.txt.gz"), Some(Container::Gzip));
        assert_eq!(container_of("changelog.Debian.gz"), Some(Container::Gzip));
        assert_eq!(container_of("x.tar.gz"), Some(Container::TarGz));
        assert_eq!(container_of("x.TGZ"), Some(Container::TarGz));
        assert_eq!(container_of("x.tar"), Some(Container::Tar));
        assert_eq!(container_of("x.zip"), Some(Container::Zip));
        assert_eq!(container_of("x.gzip.txt"), None);
    }

    #[test]
    fn tar_size_fields() {
        assert_eq!(tar_size(b"00000001750\0"), Some(1000));
        assert_eq!(tar_size(b"           \0"), Some(0));
        let mut big = [0u8; 12];
        big[0] = 0x80;
        big[11] = 7;
        assert_eq!(tar_size(&big), Some(7));
    }

    #[test]
    fn utf8_split_across_reads_stays_whole() {
        // "żółw" with the 2-byte 'ż' split between two decode calls.
        let bytes = "żółw".as_bytes();
        let mut d = encoding_rs::UTF_8.new_decoder_without_bom_handling();
        let mut s = String::new();
        assert!(!decode_into(&mut d, &bytes[..1], &mut s, false));
        assert!(!decode_into(&mut d, &bytes[1..], &mut s, true));
        assert_eq!(s, "żółw");
    }
}
