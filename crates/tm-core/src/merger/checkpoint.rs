//! Checkpoint and resume for long merges (HANDOFF_S8 4.3; DeepSeek A1, B13).
//!
//! With `--checkpoint` (or `--resume`) the spool is not an unlinked temp
//! file but `spool` in `<output>.tm-checkpoint/`, and every finished source
//! file is written to `journal.jsonl` there: its size and modification time,
//! and where its blocks sit in the spool (or why it was skipped). A stopped
//! run — Ctrl-C, `kill -9`, a crash, a reboot, `--on-deadline partial` —
//! loses at most the files of the last few seconds. `--resume` keeps every
//! journaled file whose size and modification time are unchanged, processes
//! the rest, and writes the same bytes an uninterrupted run writes (tests
//! `a_killed_run_resumes_to_the_same_bytes`, `a_changed_file_is_read_again`).
//! The folder is removed once the outputs are in place.
//!
//! A journal line is written only after the spool bytes it names were
//! written, every [`FLUSH_EVERY`] and before each streamed (long) file; a
//! killed process loses nothing the kernel already holds, and a torn last
//! line is ignored. There is no `fdatasync` (it cost 28 % of the run on the
//! S10 corpus): after a power loss the spool may lack bytes a journal line
//! names, so every block carries the XXH3-64 of its text and `--resume`
//! checks it — a file whose check fails is read again. Line 1 is a header with a
//! fingerprint of everything that shapes a file's text (version, the
//! executable, the root, the content options); `--resume` refuses a
//! checkpoint made with anything else rather than mix two runs.
//!
//! Not with `--redact`: the propagation pass needs every secret harvested
//! from every file, and a journal of secret values beside the output would
//! be a leak of its own.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use super::spool::{Span, Spool};
use super::{Block, MergeConfig};
use crate::scanner::{SkipEntry, SkipKind};

/// `<output>.tm-checkpoint/`; the scanner never walks into such a folder.
pub(crate) const DIR_SUFFIX: &str = ".tm-checkpoint";
const JOURNAL: &str = "journal.jsonl";
const COMPACTED: &str = "journal.jsonl.new";
const SPOOL: &str = "spool";
const LOCK: &str = "lock";
const TAG: &str = "CACHEDIR.TAG";
const TAG_TEXT: &str = "Signature: 8a477f597d28d172789f06886806bc55\n\
# TurboMerger checkpoint (merge --checkpoint / --resume); removed when the merge is written.\n\
# Cache directory tag: https://bford.info/cachedir/\n";
/// Journal format: another one is refused, never guessed at.
const FORMAT: u32 = 3;
/// Journal lines are written at least this often (and before each streamed
/// file, at the end of the text stage, and on Ctrl-C).
const FLUSH_EVERY: Duration = Duration::from_secs(2);
/// …or when this much is waiting, so memory stays bounded.
const FLUSH_BYTES: usize = 32 << 20;

/// The journal interval; debug builds take `TM_TEST_CHECKPOINT_FLUSH_MS`
/// so a test that kills a two-second run sees the journal grow.
fn flush_every() -> Duration {
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var("TM_TEST_CHECKPOINT_FLUSH_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        return Duration::from_millis(ms);
    }
    FLUSH_EVERY
}

/// The checkpoint folder of an output path.
pub(crate) fn dir_for(output: &Path) -> PathBuf {
    let mut name = output
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| "merge".into());
    name.push(DIR_SUFFIX);
    output.with_file_name(name)
}

/// A source file's size and modification time (ns since the epoch): what
/// decides whether its journaled text is still its text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Stamp {
    size: u64,
    mtime_ns: u64,
}

impl Stamp {
    pub(super) fn of(path: &Path) -> Option<Stamp> {
        let m = std::fs::metadata(path).ok()?;
        Some(Stamp {
            size: m.len(),
            mtime_ns: mtime_ns(&m)?,
        })
    }
}

fn mtime_ns(m: &std::fs::Metadata) -> Option<u64> {
    let d = m.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    u64::try_from(d.as_nanos()).ok()
}

#[derive(Serialize, Deserialize)]
struct Header {
    tm_checkpoint: u32,
    fingerprint: String,
}

/// One finished source file.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub(super) struct Entry {
    /// The source file as the merge names it (the lookup key).
    path: String,
    size: u64,
    mtime_ns: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    blocks: Vec<JBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    skip: Option<JSkip>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct JBlock {
    rel: String,
    off: u64,
    len: u64,
    lang: String,
    tokens: usize,
    /// XXH3-64 of the block's text in the spool.
    xxh: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    compressed: bool,
    /// Archive members and continuation segments (stream.rs): output that
    /// is not a file of the tree.
    #[serde(default, skip_serializing_if = "is_false")]
    synthetic: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct JSkip {
    path: String,
    reason: String,
    kind: String,
}

fn kind_name(k: SkipKind) -> &'static str {
    match k {
        SkipKind::Excluded => "excluded",
        SkipKind::Binary => "binary",
        SkipKind::NotCaptured => "not_captured",
        SkipKind::PrunedDir => "pruned_dir",
        SkipKind::IgnoredByRule => "ignored_by_rule",
    }
}

fn kind_from(s: &str) -> SkipKind {
    match s {
        "excluded" => SkipKind::Excluded,
        "binary" => SkipKind::Binary,
        "pruned_dir" => SkipKind::PrunedDir,
        "ignored_by_rule" => SkipKind::IgnoredByRule,
        _ => SkipKind::NotCaptured,
    }
}

impl Entry {
    /// A file's journal entry: its blocks (a file, or the segments of a
    /// streamed one: its segments and archive members, flagged synthetic)
    /// and/or the reason it was skipped. `None` when a block cannot come
    /// back from the journal alone (redactions, pieces — neither occurs in
    /// the text stage of a checkpointed merge).
    pub(super) fn new(
        path: String,
        stamp: Stamp,
        blocks: &[Block],
        skip: Option<&SkipEntry>,
        spool: &Spool,
    ) -> io::Result<Option<Entry>> {
        let mut out = Vec::with_capacity(blocks.len());
        for b in blocks {
            if !b.redactions.is_empty() || b.piece.is_some() {
                return Ok(None);
            }
            out.push(JBlock {
                xxh: twox_hash::XxHash3_64::oneshot(spool.read(b.span)?.as_bytes()),
                rel: b.relative.clone(),
                off: b.span.offset,
                len: b.span.len,
                lang: b.lang.clone(),
                tokens: b.tokens,
                note: b.utf8_note.clone(),
                compressed: b.compressed,
                synthetic: b.synthetic,
            });
        }
        Ok(Some(Entry {
            path,
            size: stamp.size,
            mtime_ns: stamp.mtime_ns,
            blocks: out,
            skip: skip.map(|s| JSkip {
                path: s.path.clone(),
                reason: s.reason.clone(),
                kind: kind_name(s.kind).into(),
            }),
        }))
    }

    /// The blocks and the skip, as the merge holds them.
    pub(super) fn restore(self) -> (Vec<Block>, Option<SkipEntry>) {
        let blocks = self
            .blocks
            .into_iter()
            .map(|b| Block {
                relative: b.rel,
                span: Span {
                    offset: b.off,
                    len: b.len,
                },
                lang: b.lang,
                tokens: b.tokens,
                utf8_note: b.note,
                redactions: Vec::new(),
                compressed: b.compressed,
                synthetic: b.synthetic,
                piece: None,
            })
            .collect();
        let skip = self
            .skip
            .map(|s| SkipEntry::new(s.path, s.reason, kind_from(&s.kind)));
        (blocks, skip)
    }
}

/// What `--resume` holds of a journaled file until the merge takes it (S12,
/// HANDOFF_S8 §9.3): no path — the index key is the path's XXH3-128 and the
/// caller brings the path back —, a block's name only when it is not the
/// path, the language as an index into the few that occur. ≈ 150 B a file
/// instead of ≈ 600 B with whole entries keyed by path strings.
struct Kept {
    size: u64,
    mtime_ns: u64,
    /// Its journal line (after the header): pass 2 copies exactly these.
    line: u64,
    blocks: Box<[KBlock]>,
    skip: Option<Box<JSkip>>,
}

struct KBlock {
    rel: Option<Box<str>>,
    note: Option<Box<str>>,
    off: u64,
    len: u64,
    tokens: u64,
    xxh: u64,
    lang: u16,
    compressed: bool,
    synthetic: bool,
}

impl Kept {
    /// `None` only past 65,535 languages (the file is then read again).
    fn of(
        e: Entry,
        line: u64,
        langs: &mut Vec<String>,
        ix: &mut HashMap<String, u16>,
    ) -> Option<(u128, Kept)> {
        let mut blocks = Vec::with_capacity(e.blocks.len());
        for b in e.blocks {
            let lang = match ix.get(&b.lang) {
                Some(&i) => i,
                None => {
                    let i = u16::try_from(langs.len()).ok()?;
                    langs.push(b.lang.clone());
                    ix.insert(b.lang, i);
                    i
                }
            };
            blocks.push(KBlock {
                rel: (b.rel != e.path).then(|| b.rel.into_boxed_str()),
                note: b.note.map(String::into_boxed_str),
                off: b.off,
                len: b.len,
                tokens: b.tokens as u64,
                xxh: b.xxh,
                lang,
                compressed: b.compressed,
                synthetic: b.synthetic,
            });
        }
        let kept = Kept {
            size: e.size,
            mtime_ns: e.mtime_ns,
            line,
            blocks: blocks.into_boxed_slice(),
            skip: e.skip.map(Box::new),
        };
        Some((path_key(&e.path), kept))
    }

    fn end(&self) -> u64 {
        self.blocks.iter().map(|b| b.off + b.len).max().unwrap_or(0)
    }
}

fn path_key(path: &str) -> u128 {
    twox_hash::XxHash3_128::oneshot(path.as_bytes())
}

/// Pass 2 needs only a line's path.
#[derive(Deserialize)]
struct PathOnly {
    path: String,
}

/// An open checkpoint: the journal being written and what the last run left.
pub(super) struct Checkpoint {
    dir: PathBuf,
    journal: File,
    /// Held (locked) for the whole run: two merges never share a checkpoint.
    lock: File,
    pending: Vec<u8>,
    last_flush: Instant,
    every: Duration,
    /// The files the last run journaled, by the XXH3-128 of their path.
    kept: HashMap<u128, Kept>,
    /// Languages of the kept blocks (`KBlock::lang` indexes this).
    langs: Vec<String>,
    /// Files this run took from the checkpoint instead of reading them.
    pub(super) resumed: usize,
    /// Files the checkpoint held when it was opened.
    pub(super) found: usize,
    /// Journaled files whose spool bytes failed their check (read again).
    pub(super) failed_check: usize,
}

impl Checkpoint {
    /// Open the checkpoint beside `output` — continued with `cfg.resume`
    /// when one exists, else a new one — and the spool that goes with it.
    pub(super) fn open(
        output: &Path,
        root: &Path,
        cfg: &MergeConfig,
    ) -> Result<(Checkpoint, Spool)> {
        let dir = dir_for(output);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create the checkpoint folder {}", dir.display()))?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(LOCK))?;
        if lock.try_lock().is_err() {
            bail!(
                "the checkpoint {} is in use by another TurboMerger run",
                dir.display()
            );
        }
        let fingerprint = fingerprint(root, cfg);
        let jpath = dir.join(JOURNAL);
        let spath = dir.join(SPOOL);
        let mut kept: HashMap<u128, Kept> = HashMap::new();
        let mut langs: Vec<String> = Vec::new();
        let mut failed_check = 0;
        // Entry lines that parsed; pass 2 reads no further.
        let mut whole = 0u64;
        if cfg.resume && jpath.is_file() {
            let spool_len = std::fs::metadata(&spath).map(|m| m.len()).unwrap_or(0);
            // Pass 1: index every whole entry by its path's XXH3-128 (a later
            // line for the same file wins); reading stops at the first line
            // that is not one (a torn last write).
            let mut lines = BufReader::new(File::open(&jpath)?).split(b'\n');
            let header = match lines.next() {
                Some(Ok(l)) => serde_json::from_slice::<Header>(&l).ok(),
                _ => None,
            };
            match &header {
                Some(h) if h.tm_checkpoint == FORMAT && h.fingerprint == fingerprint => {}
                Some(_) => bail!(
                    "the checkpoint {} was made with other merge options or another TurboMerger build; \
                     remove that folder, or use --checkpoint instead of --resume to start over",
                    dir.display()
                ),
                // Torn while it was being created: nothing in it.
                None => {}
            }
            if header.is_some() {
                let mut ix: HashMap<String, u16> = HashMap::new();
                for (i, line) in lines.enumerate() {
                    let Some(e) = line
                        .ok()
                        .and_then(|l| serde_json::from_slice::<Entry>(&l).ok())
                    else {
                        break;
                    };
                    if let Some((k, v)) = Kept::of(e, i as u64, &mut langs, &mut ix) {
                        kept.insert(k, v);
                    }
                    whole = i as u64 + 1;
                }
            }
            // Every kept block must still be in the spool, byte for byte
            // (a power loss can drop bytes a journal line names).
            let check = Spool::at(&spath, spool_len)?;
            kept.retain(|_, k| {
                let ok = k.end() <= spool_len
                    && k.blocks.iter().all(|b| {
                        let span = Span {
                            offset: b.off,
                            len: b.len,
                        };
                        check
                            .read(span)
                            .is_ok_and(|t| twox_hash::XxHash3_64::oneshot(t.as_bytes()) == b.xxh)
                    });
                failed_check += usize::from(!ok);
                ok
            });
        }
        let end = kept.values().map(Kept::end).max().unwrap_or(0);
        // Pass 2: the journal is rewritten with exactly the lines it keeps,
        // byte for byte and in their order (header first); then the spool is
        // cut to the last byte they name.
        let tmp = dir.join(COMPACTED);
        {
            let mut w = io::BufWriter::new(File::create(&tmp)?);
            serde_json::to_writer(
                &mut w,
                &Header {
                    tm_checkpoint: FORMAT,
                    fingerprint,
                },
            )?;
            w.write_all(b"\n")?;
            if !kept.is_empty() {
                let mut lines = BufReader::new(File::open(&jpath)?).split(b'\n');
                lines.next();
                for (i, line) in lines.enumerate().take(whole as usize) {
                    let line = line?;
                    let Ok(p) = serde_json::from_slice::<PathOnly>(&line) else {
                        break;
                    };
                    if kept
                        .get(&path_key(&p.path))
                        .is_some_and(|k| k.line == i as u64)
                    {
                        w.write_all(&line)?;
                        w.write_all(b"\n")?;
                    }
                }
            }
            w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        }
        std::fs::rename(&tmp, &jpath)?;
        if !dir.join(TAG).exists() {
            std::fs::write(dir.join(TAG), TAG_TEXT)?;
        }
        let spool = Spool::at(&spath, end)?;
        let journal = OpenOptions::new().append(true).open(&jpath)?;
        let found = kept.len();
        Ok((
            Checkpoint {
                dir,
                journal,
                lock,
                pending: Vec::new(),
                last_flush: Instant::now(),
                every: flush_every(),
                kept,
                langs,
                resumed: 0,
                found,
                failed_check,
            },
            spool,
        ))
    }

    /// Whether `path` can come from the checkpoint (unchanged since), without
    /// taking it: entries are expanded one at a time, when the merge reaches
    /// them, so the compact index is all a resumed run holds (S12).
    pub(super) fn holds(&self, path: &str, stamp: Option<Stamp>) -> bool {
        let (Some(k), Some(s)) = (self.kept.get(&path_key(path)), stamp) else {
            return false;
        };
        k.size == s.size && k.mtime_ns == s.mtime_ns
    }

    /// The journaled entry for `path`, if the file is unchanged since.
    pub(super) fn take(&mut self, path: &str, stamp: Option<Stamp>) -> Option<Entry> {
        let k = self.kept.remove(&path_key(path))?;
        let s = stamp?;
        if k.size != s.size || k.mtime_ns != s.mtime_ns {
            return None;
        }
        let langs = &self.langs;
        let blocks = k
            .blocks
            .into_vec()
            .into_iter()
            .map(|b| JBlock {
                rel: b.rel.map_or_else(|| path.to_string(), String::from),
                off: b.off,
                len: b.len,
                lang: langs[usize::from(b.lang)].clone(),
                tokens: b.tokens as usize,
                xxh: b.xxh,
                note: b.note.map(String::from),
                compressed: b.compressed,
                synthetic: b.synthetic,
            })
            .collect();
        Some(Entry {
            path: path.to_string(),
            size: k.size,
            mtime_ns: k.mtime_ns,
            blocks,
            skip: k.skip.map(|s| *s),
        })
    }

    /// Journal a finished file (written out with the next flush).
    pub(super) fn record(&mut self, e: &Entry, spool: &mut Spool) -> io::Result<()> {
        serde_json::to_writer(&mut self.pending, e)?;
        self.pending.push(b'\n');
        if self.last_flush.elapsed() >= self.every || self.pending.len() >= FLUSH_BYTES {
            self.flush(spool)?;
        }
        Ok(())
    }

    /// Write the spool's buffer, then the waiting journal lines.
    pub(super) fn flush(&mut self, spool: &mut Spool) -> io::Result<()> {
        self.last_flush = Instant::now();
        if self.pending.is_empty() {
            return Ok(());
        }
        spool.flush()?;
        self.journal.write_all(&self.pending)?;
        self.pending.clear();
        Ok(())
    }

    /// Flush when the interval is over (the merge calls this while it waits).
    pub(super) fn tick(&mut self, spool: &mut Spool) -> io::Result<()> {
        if self.last_flush.elapsed() >= self.every {
            self.flush(spool)?;
        }
        Ok(())
    }

    pub(super) fn dir(&self) -> &Path {
        &self.dir
    }

    /// The merge is written: remove the folder (only the files it made).
    pub(super) fn remove(self, spool: Spool) {
        drop(spool);
        let Checkpoint {
            dir, journal, lock, ..
        } = self;
        drop(journal);
        for f in [JOURNAL, COMPACTED, SPOOL, TAG] {
            let _ = std::fs::remove_file(dir.join(f));
        }
        let _ = lock.unlock();
        drop(lock);
        let _ = std::fs::remove_file(dir.join(LOCK));
        let _ = std::fs::remove_dir(&dir);
    }
}

/// Everything that shapes a file's text in the merge: another value means
/// another merge, whose blocks must not be mixed with this one's.
fn fingerprint(root: &Path, cfg: &MergeConfig) -> String {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| format!("{}:{}", m.len(), mtime_ns(&m).unwrap_or(0)))
        .unwrap_or_default();
    let selection = cfg
        .selection
        .as_ref()
        .map(|s| format!("{s:?}"))
        .unwrap_or_default();
    let text = format!(
        "turbomerger {} journal {FORMAT} | root {} | exe {exe} | format {:?} | empty {} | base64 {} | \
         compress {} | strip {} | reproducible {} | ocr {} | figures {} | max {} | grid {} | selection {selection}",
        env!("CARGO_PKG_VERSION"),
        root.display(),
        cfg.format,
        cfg.remove_empty_lines,
        cfg.truncate_base64,
        cfg.compress,
        cfg.strip_comments,
        cfg.reproducible,
        cfg.extract_images_text,
        cfg.keep_figures,
        cfg.max_file_size,
        cfg.grid_chunk_rows,
    );
    format!("{:032x}", twox_hash::XxHash3_128::oneshot(text.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(rel: &str, off: u64, len: u64) -> Block {
        Block {
            relative: rel.into(),
            span: Span { offset: off, len },
            lang: "rust".into(),
            tokens: 7,
            utf8_note: Some("decoded from windows-1252".into()),
            redactions: Vec::new(),
            compressed: true,
            synthetic: false,
            piece: None,
        }
    }

    #[test]
    fn entries_round_trip_and_only_plain_blocks_are_journaled() {
        let tmp = tempfile::tempdir().unwrap();
        let mut spool = Spool::at(&tmp.path().join("s"), 0).unwrap();
        spool.append("12345abc").unwrap();
        let stamp = Stamp {
            size: 3,
            mtime_ns: 9,
        };
        let skip = SkipEntry::new("a.bin", "binary content", SkipKind::Binary);
        let e = Entry::new(
            "a.rs".into(),
            stamp,
            &[block("a.rs", 5, 3)],
            Some(&skip),
            &spool,
        )
        .unwrap()
        .unwrap();
        assert_eq!(e.blocks[0].xxh, twox_hash::XxHash3_64::oneshot(b"abc"));
        let line = serde_json::to_string(&e).unwrap();
        let back: Entry = serde_json::from_str(&line).unwrap();
        let (blocks, skip) = back.restore();
        assert_eq!(blocks.len(), 1);
        let b = &blocks[0];
        assert_eq!(
            (b.relative.as_str(), b.span.offset, b.span.len),
            ("a.rs", 5, 3)
        );
        assert_eq!((b.lang.as_str(), b.tokens, b.compressed), ("rust", 7, true));
        assert!(!b.synthetic);
        let mut member = block("p.zip!/m.txt", 0, 5);
        member.synthetic = true;
        let e = Entry::new("p.zip".into(), stamp, &[member], None, &spool)
            .unwrap()
            .expect("archive members are journaled");
        assert!(e.restore().0[0].synthetic, "and come back as members");
        assert_eq!(b.utf8_note.as_deref(), Some("decoded from windows-1252"));
        let s = skip.unwrap();
        assert_eq!((s.path.as_str(), s.kind), ("a.bin", SkipKind::Binary));
        for k in [
            SkipKind::Excluded,
            SkipKind::Binary,
            SkipKind::NotCaptured,
            SkipKind::PrunedDir,
            SkipKind::IgnoredByRule,
        ] {
            assert_eq!(kind_from(kind_name(k)), k);
        }
        let mut redacted = block("b.rs", 0, 1);
        redacted.redactions.push(("token", 1));
        assert!(Entry::new("b.rs".into(), stamp, &[redacted], None, &spool)
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_torn_last_line_ends_the_journal_and_options_must_match() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("out.md");
        let root = tmp.path().join("src");
        let cfg = MergeConfig {
            checkpoint: true,
            ..MergeConfig::default()
        };
        let (mut ck, mut spool) = Checkpoint::open(&out, &root, &cfg).unwrap();
        assert_eq!(ck.dir(), tmp.path().join("out.md.tm-checkpoint"));
        let stamp = Stamp {
            size: 1,
            mtime_ns: 1,
        };
        for (i, name) in ["a", "b", "d"].iter().enumerate() {
            let span = spool.append("xy").unwrap();
            assert_eq!(span.offset, 2 * i as u64);
            let mut b = block(name, span.offset, span.len);
            b.utf8_note = None;
            let e = Entry::new((*name).into(), stamp, &[b], None, &spool)
                .unwrap()
                .unwrap();
            ck.record(&e, &mut spool).unwrap();
        }
        // A second run cannot open the checkpoint while this one holds it.
        let again = MergeConfig {
            resume: true,
            ..MergeConfig::default()
        };
        assert!(Checkpoint::open(&out, &root, &again).is_err());
        ck.flush(&mut spool).unwrap();
        drop((ck, spool));
        // After a power loss the spool may lack bytes a line names: one
        // byte of b's text changes on disk.
        let s = tmp.path().join("out.md.tm-checkpoint").join(SPOOL);
        let mut bytes = std::fs::read(&s).unwrap();
        bytes[2] = b'Q';
        std::fs::write(&s, bytes).unwrap();
        // A torn write after the three entries: half a line.
        let j = tmp.path().join("out.md.tm-checkpoint").join(JOURNAL);
        let mut f = OpenOptions::new().append(true).open(&j).unwrap();
        f.write_all(br#"{"path":"c","size":1,"mt"#).unwrap();
        drop(f);
        let (mut ck, spool) = Checkpoint::open(&out, &root, &again).unwrap();
        assert_eq!((ck.found, ck.failed_check), (2, 1), "b fails its check");
        assert!(ck.take("b", Some(stamp)).is_none());
        assert!(ck.take("a", Some(stamp)).is_some());
        let changed = Stamp {
            size: 1,
            mtime_ns: 2,
        };
        assert!(
            ck.take("d", Some(changed)).is_none(),
            "a changed file is read again"
        );
        assert!(ck.take("c", Some(stamp)).is_none());
        drop((ck, spool));
        // Other content options: refused under --resume, not mixed.
        let other = MergeConfig {
            resume: true,
            remove_empty_lines: true,
            ..MergeConfig::default()
        };
        let err = Checkpoint::open(&out, &root, &other).err().unwrap();
        assert!(err.to_string().contains("other merge options"), "{err}");
        // --checkpoint (not --resume) starts over; remove leaves nothing.
        let (ck, spool) = Checkpoint::open(&out, &root, &cfg).unwrap();
        assert_eq!(ck.found, 0);
        ck.remove(spool);
        assert!(!tmp.path().join("out.md.tm-checkpoint").exists());
    }

    fn record_n(ck: &mut Checkpoint, spool: &mut Spool, names: &[&str], stamp: Stamp) {
        for name in names {
            let span = spool.append(&format!("text of {name}\n")).unwrap();
            let e = Entry::new(
                (*name).into(),
                stamp,
                &[block(name, span.offset, span.len)],
                None,
                spool,
            )
            .unwrap()
            .unwrap();
            ck.record(&e, spool).unwrap();
        }
    }

    #[test]
    fn the_resume_index_keeps_no_paths_and_stays_small() {
        // S12 (§9.3): ≈ 150 B a file instead of ≈ 600 B.
        assert!(
            std::mem::size_of::<Kept>() <= 48,
            "{}",
            std::mem::size_of::<Kept>()
        );
        assert!(
            std::mem::size_of::<KBlock>() <= 72,
            "{}",
            std::mem::size_of::<KBlock>()
        );
        let tmp = tempfile::tempdir().unwrap();
        let (out, root) = (tmp.path().join("o.md"), tmp.path().join("src"));
        let cfg = MergeConfig {
            checkpoint: true,
            ..MergeConfig::default()
        };
        let (mut ck, mut spool) = Checkpoint::open(&out, &root, &cfg).unwrap();
        let names: Vec<String> = (0..1000).map(|i| format!("dir/f{i}.rs")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let stamp = Stamp {
            size: 4,
            mtime_ns: 5,
        };
        record_n(&mut ck, &mut spool, &refs, stamp);
        ck.flush(&mut spool).unwrap();
        drop((ck, spool));
        let again = MergeConfig {
            resume: true,
            ..MergeConfig::default()
        };
        let (mut ck, spool) = Checkpoint::open(&out, &root, &again).unwrap();
        assert_eq!(ck.found, 1000);
        assert!(ck
            .kept
            .values()
            .all(|k| k.blocks.iter().all(|b| b.rel.is_none())));
        assert_eq!(ck.langs, vec!["rust".to_string()]);
        let e = ck.take("dir/f7.rs", Some(stamp)).unwrap();
        let (blocks, _) = e.restore();
        assert_eq!(blocks[0].relative, "dir/f7.rs");
        assert_eq!(blocks[0].lang, "rust");
        assert_eq!(
            blocks[0].utf8_note.as_deref(),
            Some("decoded from windows-1252")
        );
        assert_eq!(spool.read(blocks[0].span).unwrap(), "text of dir/f7.rs\n");
    }

    #[test]
    fn the_rewritten_journal_keeps_the_latest_lines_byte_for_byte() {
        let tmp = tempfile::tempdir().unwrap();
        let (out, root) = (tmp.path().join("o.md"), tmp.path().join("src"));
        let cfg = MergeConfig {
            checkpoint: true,
            ..MergeConfig::default()
        };
        let (mut ck, mut spool) = Checkpoint::open(&out, &root, &cfg).unwrap();
        let (s1, s2) = (
            Stamp {
                size: 1,
                mtime_ns: 1,
            },
            Stamp {
                size: 2,
                mtime_ns: 2,
            },
        );
        record_n(&mut ck, &mut spool, &["a", "b"], s1);
        record_n(&mut ck, &mut spool, &["a"], s2); // a again, later: this line wins
        record_n(&mut ck, &mut spool, &["c"], s1);
        ck.flush(&mut spool).unwrap();
        drop((ck, spool));
        let j = tmp.path().join("o.md.tm-checkpoint").join(JOURNAL);
        let before = std::fs::read_to_string(&j).unwrap();
        let lines: Vec<&str> = before.lines().collect(); // header, a1, b, a2, c
                                                         // c's bytes in the spool change: it fails its check and is dropped.
        let sp = tmp.path().join("o.md.tm-checkpoint").join(SPOOL);
        let mut bytes = std::fs::read(&sp).unwrap();
        let n = bytes.len();
        bytes[n - 3] ^= 1;
        std::fs::write(&sp, bytes).unwrap();
        let again = MergeConfig {
            resume: true,
            ..MergeConfig::default()
        };
        let (mut ck, _spool) = Checkpoint::open(&out, &root, &again).unwrap();
        assert_eq!((ck.found, ck.failed_check), (2, 1));
        let after = std::fs::read_to_string(&j).unwrap();
        let kept: Vec<&str> = after.lines().collect();
        assert_eq!(kept[0], lines[0], "header");
        assert_eq!(
            &kept[1..],
            &[lines[2], lines[3]],
            "b then the later a, byte for byte"
        );
        assert!(ck.take("a", Some(s1)).is_none(), "the earlier a is gone");
        assert!(ck.take("b", Some(s1)).is_some());
    }
}
