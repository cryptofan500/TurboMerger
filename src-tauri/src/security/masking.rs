//! Known-value masking (N-13, N-14, N-35).
//!
//! v7.7.0 kept harvested secrets in a `HashSet` and ran
//! `content.replace(secret, "[REDACTED]")` once per secret per block:
//! - iteration order was random per process, so when one secret was a prefix
//!   of another the output differed between runs and could leak the longer
//!   secret's tail (`[REDACTED]W8nB3c`) — N-14;
//! - raw substring replacement cut inside numbers and words
//!   (`4,096-token` → `4,0[REDACTED]`) — N-13;
//! - cost was O(secrets × corpus) on one thread after the progress bar read
//!   100 % (21.65 s for one 8,000-token notes file) — N-35.
//!
//! Here all values go into one Aho-Corasick automaton built from a sorted,
//! de-duplicated set. Matches count only as whole tokens (the bytes around a
//! match must not continue a word), overlapping candidates resolve
//! leftmost-longest, and blocks are processed in parallel — so the result is
//! byte-identical on every run and the cost is linear in the corpus.

use std::collections::BTreeSet;

use aho_corasick::{AhoCorasick, MatchKind};

/// A deterministic set of literal values to mask as whole tokens.
pub struct KnownValues {
    ac: Option<AhoCorasick>,
    len: usize,
}

/// Bytes that continue a word: a match must not be glued to one of these on
/// a side where the value itself starts/ends with a word byte. Non-ASCII
/// bytes count as word bytes (they are letters far more often than not).
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

fn is_whole_token(bytes: &[u8], start: usize, end: usize) -> bool {
    let before_ok = start == 0 || !is_word_byte(bytes[start - 1]) || !is_word_byte(bytes[start]);
    let after_ok = end == bytes.len() || !is_word_byte(bytes[end]) || !is_word_byte(bytes[end - 1]);
    before_ok && after_ok
}

impl KnownValues {
    pub fn new<I: IntoIterator<Item = String>>(values: I) -> KnownValues {
        let set: BTreeSet<String> = values.into_iter().filter(|v| !v.is_empty()).collect();
        let len = set.len();
        let ac = if set.is_empty() {
            None
        } else {
            Some(
                AhoCorasick::builder()
                    .match_kind(MatchKind::Standard)
                    .build(set.iter())
                    .expect("literal patterns always build"),
            )
        };
        KnownValues { ac, len }
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whole-token matches, leftmost-longest, non-overlapping:
    /// `(start, end, pattern_index)`.
    fn matches(&self, text: &str) -> Vec<(usize, usize, usize)> {
        let Some(ac) = &self.ac else {
            return Vec::new();
        };
        let bytes = text.as_bytes();
        let mut cands: Vec<(usize, usize, usize)> = ac
            .find_overlapping_iter(text)
            .filter(|m| is_whole_token(bytes, m.start(), m.end()))
            .map(|m| (m.start(), m.end(), m.pattern().as_usize()))
            .collect();
        cands.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
        let mut out = Vec::with_capacity(cands.len());
        let mut cursor = 0usize;
        for c in cands {
            if c.0 >= cursor {
                cursor = c.1;
                out.push(c);
            }
        }
        out
    }

    /// Replace every whole-token occurrence with `placeholder`. `None` when
    /// nothing matched (the caller keeps its buffer and token count).
    pub fn mask(&self, text: &str, placeholder: &str) -> Option<(String, usize)> {
        let found = self.matches(text);
        if found.is_empty() {
            return None;
        }
        let mut out = String::with_capacity(text.len());
        let mut last = 0usize;
        for (start, end, _) in &found {
            out.push_str(&text[last..*start]);
            out.push_str(placeholder);
            last = *end;
        }
        out.push_str(&text[last..]);
        Some((out, found.len()))
    }

    /// Indices of the values that occur (as whole tokens) in `text`.
    pub fn present(&self, text: &str) -> BTreeSet<usize> {
        self.matches(text).into_iter().map(|(_, _, p)| p).collect()
    }

    /// Number of distinct values (pattern indices are in sorted order).
    pub fn len(&self) -> usize {
        self.len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv(v: &[&str]) -> KnownValues {
        KnownValues::new(v.iter().map(|s| s.to_string()))
    }

    #[test]
    fn prefix_secrets_never_leak_a_tail() {
        // v1 A.5: the rotated secret extends the old one.
        let k = kv(&["Qx7Zk2Mv9Rt4Lp", "Qx7Zk2Mv9Rt4LpW8nB3c"]);
        let text =
            "The old value was Qx7Zk2Mv9Rt4Lp and the rotated value is Qx7Zk2Mv9Rt4LpW8nB3c today.";
        let (out, n) = k.mask(text, "[REDACTED]").unwrap();
        assert_eq!(
            out,
            "The old value was [REDACTED] and the rotated value is [REDACTED] today."
        );
        assert_eq!(n, 2);
        // Insertion order does not matter.
        let k2 = kv(&["Qx7Zk2Mv9Rt4LpW8nB3c", "Qx7Zk2Mv9Rt4Lp"]);
        assert_eq!(k2.mask(text, "[REDACTED]").unwrap().0, out);
    }

    #[test]
    fn values_only_match_as_whole_tokens() {
        let k = kv(&["96-token", "Opus-4.8", "arXiv:2210.03629"]);
        // Glued to digits/letters: not the value.
        assert!(k.mask("a 4,096-token context", "[R]").is_none());
        assert!(k.mask("xOpus-4.8", "[R]").is_none());
        // Standalone: masked; punctuation around it is fine.
        assert_eq!(
            k.mask("use (Opus-4.8), see arXiv:2210.03629.", "[R]")
                .unwrap()
                .0,
            "use ([R]), see [R]."
        );
        // Values that start/end with punctuation match next to anything.
        let p = kv(&["!Xk9v!m22"]);
        assert_eq!(p.mask("pw:!Xk9v!m22;", "[R]").unwrap().0, "pw:[R];");
    }

    #[test]
    fn present_reports_whole_token_hits() {
        let k = kv(&["alpha123beta", "gamma456delta"]);
        assert_eq!(
            k.present("x alpha123beta y alpha123beta"),
            BTreeSet::from([0])
        );
        assert!(k.present("xalpha123beta").is_empty());
        assert!(kv(&[]).is_empty());
        assert!(kv(&[]).mask("anything", "[R]").is_none());
    }
}
