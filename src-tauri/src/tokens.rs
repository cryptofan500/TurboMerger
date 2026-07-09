//! Real token counting via tiktoken (o200k_base — GPT-4o/4.1/5, o-series).
//!
//! Anthropic ships no offline Claude tokenizer; o200k undercounts Claude by
//! ~15-20% on code, so the UI labels the Claude figure as an estimate
//! (o200k × ~1.18). The count runs per file on rayon threads during the merge.

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
}
