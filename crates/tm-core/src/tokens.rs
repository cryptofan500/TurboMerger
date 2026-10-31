//! Exact o200k_base token counts (GPT-4o/4.1/5, o-series) via tiktoken's
//! ordinary encoding: special-token strings such as `<|endoftext|>` count as
//! the text they are, which is how a chat window tokenizes pasted content.
//! (`bpe-openai` counts the same and is ~20 % faster, but adds 28 MB to every
//! binary — ADR 0016.)
//!
//! Anthropic ships no offline Claude tokenizer; o200k undercounts Claude by
//! ~15-20% on code, so the UI labels the Claude figure as an estimate
//! (o200k × ~1.18). The count runs per file on rayon threads during the merge.

use std::io::{self, Read};
use std::path::Path;

static BPE: std::sync::LazyLock<tiktoken_rs::CoreBPE> =
    std::sync::LazyLock::new(|| tiktoken_rs::o200k_base().expect("o200k_base tokenizer"));

/// Exact o200k_base token count for `text`.
pub fn count(text: &str) -> usize {
    BPE.encode_ordinary(text).len()
}

/// Rough Claude-token estimate (o200k tends to undercount Claude on code).
pub fn claude_estimate(o200k: usize) -> usize {
    (o200k as f64 * 1.18).round() as usize
}

/// Where `text` may be cut so that counting the pieces separately gives the
/// same total as counting the whole: just after a `\n` that is followed by
/// neither whitespace nor `/`. No o200k pre-token spans such a point — none
/// of the pattern's alternatives continues past a newline except
/// `[\r\n/]*` (hence the `/`) and the whitespace runs.
fn safe_cut(text: &str, near: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = near.min(bytes.len());
    while i > 0 {
        if bytes[i - 1] == b'\n' {
            match bytes.get(i) {
                Some(&c) if !c.is_ascii_whitespace() && c != b'/' && text.is_char_boundary(i) => {
                    return Some(i)
                }
                _ => {}
            }
        }
        i -= 1;
    }
    None
}

/// Where a newline-free stretch may be cut with the same guarantee: just
/// after an ASCII letter or digit that is followed by an ASCII character that
/// is neither (nor `'`, which can start a contraction such as `'s`). Every
/// o200k pre-token holding that letter or digit ends there: letter runs,
/// digit runs and their contractions cannot take a space or punctuation.
fn word_cut(text: &str, near: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    (1..near.min(bytes.len())).rev().find(|&i| {
        let (a, b) = (bytes[i - 1], bytes[i]);
        a.is_ascii_alphanumeric() && b.is_ascii() && !b.is_ascii_alphanumeric() && b != b'\''
    })
}

/// Text held before counting when it has no cut point at all (one 200 MB
/// word): past this it is cut anyway, which can split one pre-token — the
/// count may then be off by a token per cut.
const FORCED_CUT: usize = 16 << 20;

/// Exact o200k count of a file's UTF-8 text without holding all of it:
/// read in pieces of about 4 MiB, cut at `safe_cut` points (`word_cut` in a
/// long line), so memory stays near [`FORCED_CUT`] for any file.
pub fn count_file(path: &Path) -> io::Result<usize> {
    count_reader(std::fs::File::open(path)?, 4 << 20)
}

fn count_reader(reader: impl Read, chunk: usize) -> io::Result<usize> {
    count_bounded(reader, chunk, FORCED_CUT.max(chunk)).map(|(n, _)| n)
}

/// `count_reader` with the cut limit as a parameter; also returns the most
/// text it held at once (tests check the bound).
fn count_bounded(mut reader: impl Read, chunk: usize, cap: usize) -> io::Result<(usize, usize)> {
    let mut pending = String::new();
    let mut raw = vec![0u8; chunk];
    let mut carry: Vec<u8> = Vec::new();
    let mut total = 0usize;
    let mut peak = 0usize;
    loop {
        let n = reader.read(&mut raw)?;
        if n == 0 {
            break;
        }
        carry.extend_from_slice(&raw[..n]);
        // Keep an incomplete UTF-8 sequence for the next read.
        let valid = match std::str::from_utf8(&carry) {
            Ok(_) => carry.len(),
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            Err(e) => return Err(io::Error::new(io::ErrorKind::InvalidData, e)),
        };
        pending.push_str(std::str::from_utf8(&carry[..valid]).expect("validated"));
        carry.drain(..valid);
        peak = peak.max(pending.len());
        if pending.len() >= chunk {
            let end = pending.len() - 1;
            let mut cut = safe_cut(&pending, end);
            if cut.is_none() && pending.len() >= cap {
                cut = word_cut(&pending, end).or_else(|| {
                    // No boundary at all: cut at a character boundary.
                    (1..=end).rev().find(|&i| pending.is_char_boundary(i))
                });
            }
            if let Some(cut) = cut {
                total += count(&pending[..cut]);
                pending.drain(..cut);
            }
        }
    }
    if !carry.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "text ends inside a UTF-8 sequence",
        ));
    }
    Ok((total + count(&pending), peak))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_are_reasonable() {
        assert_eq!(count(""), 0);
        // "hello world" is a couple of tokens, definitely < chars
        let n = count("hello world");
        assert!((1..11).contains(&n), "unexpected token count {}", n);
        // a chunk of code should be far fewer tokens than bytes
        let code = "fn main() { println!(\"hello\"); }\n".repeat(20);
        assert!(count(&code) < code.len());
    }

    #[test]
    fn safe_cuts_keep_counts_additive() {
        let samples = [
            "fn a() {\n    x\n}\n//comment\nfn b() {}\n",
            "line\r\nnext\r\n\r\n  indented\n\n\nlast",
            "}\n/path\n#heading\n9 digits\n<|endoftext|>\nend\n",
            "日本語\nテキスト\n  \n\tTab\nZ",
        ];
        for s in samples {
            let whole = count(s);
            for near in 0..=s.len() {
                if let Some(cut) = safe_cut(s, near) {
                    assert_eq!(
                        count(&s[..cut]) + count(&s[cut..]),
                        whole,
                        "cut at {cut} in {s:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn special_token_strings_count_as_text() {
        // A chat UI does not parse `<|endoftext|>` in pasted text as one
        // special token; neither do we.
        assert!(count("<|endoftext|>") > 1);
    }

    #[test]
    fn long_lines_are_cut_with_bounded_memory() {
        // S8 1.5 (DeepSeek B3.7): a file without newlines was held whole.
        let prose: String = (0..3000).map(|i| format!("word{i} ünï, {{x}} ")).collect();
        let b64: String = (0..3000).map(|i| format!("QUJD{i}+/x=")).collect();
        for text in [&prose, &b64] {
            let whole = count(text);
            let (n, peak) = count_bounded(text.as_bytes(), 256, 2048).unwrap();
            assert_eq!(n, whole, "word cuts keep the count exact");
            assert!(peak <= 2048 + 256 + 4, "held {peak} bytes");
        }
        // No boundary at all: still bounded, the count off by a few tokens.
        let blob = "a".repeat(6000);
        let (n, peak) = count_bounded(blob.as_bytes(), 256, 1024).unwrap();
        assert!(peak <= 1024 + 256 + 4, "held {peak} bytes");
        assert!(n.abs_diff(count(&blob)) <= 12, "{n} vs {}", count(&blob));
    }

    #[test]
    fn piecewise_counts_match_whole_counts() {
        let body: String = (0..400)
            .map(|i| format!("line {i}: ünïcödé {{ x }} //c\n  indented {i}\n}}\n/slash {i}\n"))
            .collect();
        let whole = count(&body);
        for chunk in [7, 64, 1000, 1 << 20] {
            assert_eq!(
                count_reader(body.as_bytes(), chunk).unwrap(),
                whole,
                "chunk {chunk}"
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.md");
        std::fs::write(&p, &body).unwrap();
        assert_eq!(count_file(&p).unwrap(), whole);
    }
}
