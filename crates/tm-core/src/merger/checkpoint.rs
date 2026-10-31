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
//! The journal is written only after the spool bytes it names are on disk
//! (write + `fdatasync`), at most every [`FLUSH_EVERY`]; a torn last line is
//! the worst a crash leaves, and it is ignored. Line 1 is a header with a
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
const FORMAT: u32 = 1;
/// Journal lines go to disk at least this often (and at the end of the text
/// stage, and on Ctrl-C).
const FLUSH_EVERY: Duration = Duration::from_secs(5);
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    compressed: bool,
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
    /// streamed one) and/or the reason it was skipped. `None` when a block
    /// cannot come back from the journal alone (redactions, pieces, git
    /// sections — none occur in the text stage of a checkpointed merge).
    pub(super) fn new(
        path: String,
        stamp: Stamp,
        blocks: &[Block],
        skip: Option<&SkipEntry>,
    ) -> Option<Entry> {
        let mut out = Vec::with_capacity(blocks.len());
        for b in blocks {
            if !b.redactions.is_empty() || b.synthetic || b.piece.is_some() {
                return None;
            }
            out.push(JBlock {
                rel: b.relative.clone(),
                off: b.span.offset,
                len: b.span.len,
                lang: b.lang.clone(),
                tokens: b.tokens,
                note: b.utf8_note.clone(),
                compressed: b.compressed,
            });
        }
        Some(Entry {
            path,
            size: stamp.size,
            mtime_ns: stamp.mtime_ns,
            blocks: out,
            skip: skip.map(|s| JSkip {
                path: s.path.clone(),
                reason: s.reason.clone(),
                kind: kind_name(s.kind).into(),
            }),
        })
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
                synthetic: false,
                piece: None,
            })
            .collect();
        let skip = self
            .skip
            .map(|s| SkipEntry::new(s.path, s.reason, kind_from(&s.kind)));
        (blocks, skip)
    }

    fn end(&self) -> u64 {
        self.blocks.iter().map(|b| b.off + b.len).max().unwrap_or(0)
    }
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
    kept: HashMap<String, Entry>,
    /// Files this run took from the checkpoint instead of reading them.
    pub(super) resumed: usize,
    /// Files the checkpoint held when it was opened.
    pub(super) found: usize,
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
        let mut kept: HashMap<String, Entry> = HashMap::new();
        if cfg.resume && jpath.is_file() {
            let spool_len = std::fs::metadata(&spath).map(|m| m.len()).unwrap_or(0);
            let (header, entries) = read_journal(&jpath)?;
            match header {
                Some(h) if h.tm_checkpoint == FORMAT && h.fingerprint == fingerprint => {}
                Some(_) => bail!(
                    "the checkpoint {} was made with other merge options or another TurboMerger build; \
                     remove that folder, or use --checkpoint instead of --resume to start over",
                    dir.display()
                ),
                // Torn while it was being created: nothing in it.
                None => {}
            }
            for e in entries {
                // A later line for the same file wins; a span past the
                // spool's end (it cannot happen, but a copied folder…) never
                // comes back.
                if e.end() <= spool_len {
                    kept.insert(e.path.clone(), e);
                }
            }
        }
        let end = kept.values().map(Entry::end).max().unwrap_or(0);
        // The journal is rewritten with what it keeps (header first), then
        // the spool is cut to the last byte those entries name.
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
            let mut order: Vec<&Entry> = kept.values().collect();
            order.sort_by(|a, b| a.path.cmp(&b.path));
            for e in order {
                serde_json::to_writer(&mut w, e)?;
                w.write_all(b"\n")?;
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
                resumed: 0,
                found,
            },
            spool,
        ))
    }

    /// The journaled entry for `path`, if the file is unchanged since.
    pub(super) fn take(&mut self, path: &str, stamp: Option<Stamp>) -> Option<Entry> {
        let e = self.kept.remove(path)?;
        let s = stamp?;
        (e.size == s.size && e.mtime_ns == s.mtime_ns).then_some(e)
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

    /// Put the spool, then the waiting journal lines, on disk.
    pub(super) fn flush(&mut self, spool: &mut Spool) -> io::Result<()> {
        self.last_flush = Instant::now();
        if self.pending.is_empty() {
            return Ok(());
        }
        spool.sync()?;
        self.journal.write_all(&self.pending)?;
        self.journal.sync_data()?;
        self.pending.clear();
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

/// The header and every whole entry; reading stops at the first line that
/// is not one (a torn last write).
fn read_journal(path: &Path) -> Result<(Option<Header>, Vec<Entry>)> {
    let mut lines = BufReader::new(File::open(path)?).split(b'\n');
    let header = match lines.next() {
        Some(Ok(l)) => serde_json::from_slice::<Header>(&l).ok(),
        _ => None,
    };
    let mut entries = Vec::new();
    if header.is_some() {
        for line in lines {
            match line
                .ok()
                .and_then(|l| serde_json::from_slice::<Entry>(&l).ok())
            {
                Some(e) => entries.push(e),
                None => break,
            }
        }
    }
    Ok((header, entries))
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
        let stamp = Stamp {
            size: 3,
            mtime_ns: 9,
        };
        let skip = SkipEntry::new("a.bin", "binary content", SkipKind::Binary);
        let e = Entry::new("a.rs".into(), stamp, &[block("a.rs", 5, 3)], Some(&skip)).unwrap();
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
        assert!(Entry::new("b.rs".into(), stamp, &[redacted], None).is_none());
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
        for (i, name) in ["a", "b"].iter().enumerate() {
            let span = spool.append("xy").unwrap();
            assert_eq!(span.offset, 2 * i as u64);
            let mut b = block(name, span.offset, span.len);
            b.utf8_note = None;
            ck.record(
                &Entry::new((*name).into(), stamp, &[b], None).unwrap(),
                &mut spool,
            )
            .unwrap();
        }
        // A second run cannot open the checkpoint while this one holds it.
        let again = MergeConfig {
            resume: true,
            ..MergeConfig::default()
        };
        assert!(Checkpoint::open(&out, &root, &again).is_err());
        ck.flush(&mut spool).unwrap();
        drop((ck, spool));
        // A torn write after the two entries: half a line.
        let j = tmp.path().join("out.md.tm-checkpoint").join(JOURNAL);
        let mut f = OpenOptions::new().append(true).open(&j).unwrap();
        f.write_all(br#"{"path":"c","size":1,"mt"#).unwrap();
        drop(f);
        let (mut ck, spool) = Checkpoint::open(&out, &root, &again).unwrap();
        assert_eq!(ck.found, 2);
        assert!(ck.take("a", Some(stamp)).is_some());
        let changed = Stamp {
            size: 1,
            mtime_ns: 2,
        };
        assert!(
            ck.take("b", Some(changed)).is_none(),
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
}
