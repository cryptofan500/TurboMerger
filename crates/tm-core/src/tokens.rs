//! Real token counting via tiktoken (o200k_base — GPT-4o/4.1/5, o-series).
//!
//! Anthropic ships no offline Claude tokenizer; o200k undercounts Claude by
//! ~15-20% on code, so the UI labels the Claude figure as an estimate
//! (o200k × ~1.18). The count runs per file on rayon threads during the merge.

use std::io::{self, Read};
use std::path::Path;
use std::sync::LazyLock;
use tiktoken_rs::CoreBPE;

static BPE: LazyLock<CoreBPE> =
    LazyLock::new(|| tiktoken_rs::o200k_base().expect("o200k_base tokenizer"));

/// Exact o200k_base token count for `text`.
pub fn count(text: &str) -> usize {
    BPE.encode_with_special_tokens(text).len()
}

/// Rough Claude-token estimate (o200k tends to undercount Claude on code).
pub fn claude_estimate(o200k: usize) -> usize {
    (o200k as f64 * 1.18).round() as usize
}

/// Where `text` may be cut so that counting the pieces separately gives the
/// same total as counting the whole: just after a `\n` that is followed by
/// neither whitespace nor `/`. No o200k pre-token spans such a point — none
/// of the pattern's alternatives continues past a newline except
/// `[\r\n/]*` (hence the `/`) and the whitespace runs — and special tokens
/// contain no newline.
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

/// Exact o200k count of a file's UTF-8 text without holding all of it:
/// read in pieces of about 4 MiB, cut at `safe_cut` points.
pub fn count_file(path: &Path) -> io::Result<usize> {
    count_reader(std::fs::File::open(path)?, 4 << 20)
}

fn count_reader(mut reader: impl Read, chunk: usize) -> io::Result<usize> {
    let mut pending = String::new();
    let mut raw = vec![0u8; chunk];
    let mut carry: Vec<u8> = Vec::new();
    let mut total = 0usize;
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
        if pending.len() >= chunk {
            if let Some(cut) = safe_cut(&pending, pending.len() - 1) {
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
    Ok(total + count(&pending))
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
