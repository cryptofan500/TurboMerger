//! The spool: an unlinked temporary file holding every processed block's
//! text, so the merge keeps only per-block metadata in memory (plan D4,
//! N-36/N-37). Appends come from one thread; reads are positional, so rayon
//! workers can read different blocks at once.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

/// Where one block's UTF-8 text sits in the spool.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Span {
    pub(crate) offset: u64,
    pub(crate) len: u64,
}

pub(crate) struct Spool {
    out: BufWriter<File>,
    end: u64,
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
            out: BufWriter::with_capacity(1 << 20, file),
            end: 0,
        })
    }

    pub(crate) fn append(&mut self, text: &str) -> io::Result<Span> {
        let span = Span {
            offset: self.end,
            len: text.len() as u64,
        };
        self.out.write_all(text.as_bytes())?;
        self.end += span.len;
        Ok(span)
    }

    /// Make every appended byte readable. Call before a read phase.
    pub(crate) fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }

    pub(crate) fn read(&self, span: Span) -> io::Result<String> {
        let mut buf = vec![0u8; span.len as usize];
        read_exact_at(self.out.get_ref(), &mut buf, span.offset)?;
        String::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
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
        s.flush().unwrap();
        assert_eq!(s.read(c).unwrap(), "γ — gamma\n");
        assert_eq!(s.read(a).unwrap(), "alpha\n");
        assert_eq!(s.read(b).unwrap(), "");
        // The spool is unlinked at once on Unix (Windows deletes it on close).
        #[cfg(unix)]
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
