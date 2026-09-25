//! The spool: an unlinked temporary file holding every processed block's
//! text, so the merge keeps only per-block metadata in memory (plan D4,
//! N-36/N-37). Appends come from one thread; reads are positional, so rayon
//! workers can read different blocks at once.
//!
//! Every write is positional too. On Windows a positional read moves the
//! file cursor (`seek_read`), so cursor-based appends would land wherever
//! the last read stopped — the spool keeps its own end offset instead.

use std::fs::File;
use std::io;
use std::path::Path;

/// Appends are gathered up to this size before one positional write.
const BUFFER: usize = 1 << 20;

/// Where one block's UTF-8 text sits in the spool.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Span {
    pub(crate) offset: u64,
    pub(crate) len: u64,
}

pub(crate) struct Spool {
    file: File,
    /// Offset just past the last appended byte.
    end: u64,
    /// Appended bytes not yet written, starting at `buf_start`.
    buf: Vec<u8>,
    buf_start: u64,
}

impl Spool {
    /// A spool beside the output (same file system, and it is about as big as
    /// the output will be); the system temp dir when that is not writable.
    pub(crate) fn new_near(output: &Path) -> io::Result<Spool> {
        let dir = output
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let file = tempfile::tempfile_in(dir).or_else(|_| tempfile::tempfile())?;
        Ok(Spool {
            file,
            end: 0,
            buf: Vec::with_capacity(BUFFER),
            buf_start: 0,
        })
    }

    pub(crate) fn append(&mut self, text: &str) -> io::Result<Span> {
        if !self.buf.is_empty() && self.buf.len() + text.len() > BUFFER {
            self.flush()?;
        }
        if self.buf.is_empty() {
            self.buf_start = self.end;
        }
        let span = Span {
            offset: self.end,
            len: text.len() as u64,
        };
        self.buf.extend_from_slice(text.as_bytes());
        self.end += span.len;
        Ok(span)
    }

    /// Write out buffered appends. Call before a read phase.
    pub(crate) fn flush(&mut self) -> io::Result<()> {
        if !self.buf.is_empty() {
            write_all_at(&self.file, &self.buf, self.buf_start)?;
            self.buf.clear();
        }
        Ok(())
    }

    pub(crate) fn read(&self, span: Span) -> io::Result<String> {
        let mut out = vec![0u8; span.len as usize];
        // Bytes still in the buffer come from there; the rest from the file.
        let written_to = if self.buf.is_empty() {
            self.end
        } else {
            self.buf_start
        };
        let file_part_end = written_to.min(span.offset + span.len);
        if span.offset < file_part_end {
            let n = (file_part_end - span.offset) as usize;
            read_exact_at(&self.file, &mut out[..n], span.offset)?;
        }
        if !self.buf.is_empty() && span.offset + span.len > self.buf_start {
            let from = span.offset.max(self.buf_start);
            let dst = (from - span.offset) as usize;
            let src = (from - self.buf_start) as usize;
            let n = (span.offset + span.len - from) as usize;
            out[dst..dst + n].copy_from_slice(&self.buf[src..src + n]);
        }
        String::from_utf8(out).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
}

#[cfg(unix)]
fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(buf, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_read(buf, offset) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(windows)]
fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_write(buf, offset) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => {
                buf = &buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appended_text_reads_back_by_span() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Spool::new_near(&dir.path().join("out.md")).unwrap();
        let a = s.append("alpha\n").unwrap();
        let b = s.append("").unwrap();
        let c = s.append("γ — gamma\n").unwrap();
        // Readable before and after a flush.
        assert_eq!(s.read(c).unwrap(), "γ — gamma\n");
        s.flush().unwrap();
        assert_eq!(s.read(c).unwrap(), "γ — gamma\n");
        assert_eq!(s.read(a).unwrap(), "alpha\n");
        assert_eq!(s.read(b).unwrap(), "");
        // The spool is unlinked at once on Unix (Windows deletes it on close).
        #[cfg(unix)]
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn appends_after_reads_land_at_the_end() {
        // The Windows bug this layout prevents: reads in between must not
        // move where the next append goes.
        let dir = tempfile::tempdir().unwrap();
        let mut s = Spool::new_near(&dir.path().join("out.md")).unwrap();
        let spans: Vec<Span> = (0..50)
            .map(|i| s.append(&format!("block {i}\n")).unwrap())
            .collect();
        s.flush().unwrap();
        for sp in &spans[..10] {
            s.read(*sp).unwrap();
        }
        let masked = s.append("masked [REDACTED]\n").unwrap();
        let big = "x".repeat(BUFFER + 10);
        let big_span = s.append(&big).unwrap();
        s.flush().unwrap();
        s.read(spans[3]).unwrap();
        let tail = s.append("tail\n").unwrap();
        s.flush().unwrap();
        for (i, sp) in spans.iter().enumerate() {
            assert_eq!(s.read(*sp).unwrap(), format!("block {i}\n"));
        }
        assert_eq!(s.read(masked).unwrap(), "masked [REDACTED]\n");
        assert_eq!(s.read(big_span).unwrap(), big);
        assert_eq!(s.read(tail).unwrap(), "tail\n");
    }
}
