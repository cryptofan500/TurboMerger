//! A block larger than one `--max-tokens` part is split into pieces that
//! each fit (HANDOFF_S8 §6.10 item 3.3, §6.11 C). Pieces are exact slices of
//! the block's text, in order — joined, they are the text again — and point
//! into the same spool bytes. A cut goes before a TM-GRID `@sheet`/`@cont`
//! line or after a blank line when one is near, else at a line end; only a
//! line longer than a piece is cut inside, at a character boundary, and that
//! piece says `line continues`.

use std::ops::Range;

use super::spool::{Span, Spool};
use super::{Block, Piece};
use crate::tokens;

/// The smallest piece worth making: below it a part has no room for text.
const MIN_PIECE_TOKENS: usize = 256;

/// What a piece's name (`(piece 12/34, line continues)`) adds to its wrapper.
pub(super) const LABEL_TOKENS: usize = 24;

/// Split every block whose text is over `room(block)` tokens. Returns, for
/// every old block index, the new index of its first piece, and how many
/// blocks were split.
pub(super) fn split_blocks(
    blocks: &mut Vec<Block>,
    spool: &Spool,
    room: impl Fn(&Block) -> usize,
) -> std::io::Result<(Vec<usize>, usize)> {
    let old = std::mem::take(blocks);
    let mut first_of = Vec::with_capacity(old.len());
    let mut split = 0;
    for b in old {
        first_of.push(blocks.len());
        let cap = room(&b);
        if b.tokens <= cap || cap < MIN_PIECE_TOKENS {
            blocks.push(b);
            continue;
        }
        let text = spool.read(b.span)?;
        let pieces = split_points(&text, cap, b.lang == "tm-grid");
        if pieces.len() < 2 {
            blocks.push(b);
            continue;
        }
        split += 1;
        let n = pieces.len() as u32;
        for (i, (r, cont, tokens)) in pieces.into_iter().enumerate() {
            let first = i == 0;
            blocks.push(Block {
                relative: b.relative.clone(),
                span: Span {
                    offset: b.span.offset + r.start as u64,
                    len: (r.end - r.start) as u64,
                },
                lang: b.lang.clone(),
                tokens,
                // Notes and redaction counts are per file: once, on piece 1.
                utf8_note: if first { b.utf8_note.clone() } else { None },
                redactions: if first {
                    b.redactions.clone()
                } else {
                    Vec::new()
                },
                compressed: b.compressed,
                synthetic: b.synthetic || !first,
                piece: Some(Piece {
                    k: i as u32 + 1,
                    n,
                    cont,
                }),
            });
        }
    }
    Ok((first_of, split))
}

/// Pieces of `text` of at most `cap` tokens each: byte range, whether the
/// piece ends inside a line, and its tokens.
pub(super) fn split_points(text: &str, cap: usize, grid: bool) -> Vec<(Range<usize>, bool, usize)> {
    let mut rough: Vec<(Range<usize>, bool)> = Vec::new();
    let (mut start, mut used) = (0usize, 0usize);
    // The best cut seen in the current piece: (offset, tokens before it).
    let mut good: Option<(usize, usize)> = None;
    let mut ls = 0usize;
    while ls < text.len() {
        let le = text[ls..].find('\n').map_or(text.len(), |i| ls + i + 1);
        let line = &text[ls..le];
        let t = tokens::count(line);
        if grid && ls > start && (line.starts_with("@sheet ") || line.starts_with("@cont ")) {
            good = Some((ls, used));
        }
        if t > cap {
            if ls > start {
                rough.push((start..ls, false));
            }
            rough.extend(cut_line(text, ls..le, t, cap));
            start = le;
            used = 0;
            good = None;
        } else {
            // Close the piece at the best cut when it keeps the piece at
            // least half full, else right before this line.
            while used + t > cap && ls > start {
                let (cut, before) = match good {
                    Some((g, u)) if u >= cap / 2 && g > start => (g, u),
                    _ => (ls, used),
                };
                rough.push((start..cut, false));
                used -= before;
                start = cut;
                good = None;
            }
            used += t;
            if !grid && line.trim().is_empty() && le < text.len() {
                good = Some((le, used));
            }
        }
        ls = le;
    }
    if start < text.len() || rough.is_empty() {
        rough.push((start..text.len(), false));
    }
    // Token counts do not add up exactly across a cut: count every piece and
    // halve the rare one that is still over.
    let mut out = Vec::with_capacity(rough.len());
    for (r, cont) in rough {
        refine(text, r, cont, cap, &mut out);
    }
    out
}

/// A line longer than a piece, cut at character boundaries into pieces of
/// about `cap` tokens; every piece but the last ends inside the line.
fn cut_line(text: &str, r: Range<usize>, t: usize, cap: usize) -> Vec<(Range<usize>, bool)> {
    let per = ((r.len() as f64 / t.max(1) as f64) * cap as f64 * 0.9).max(4.0) as usize;
    let mut out = Vec::new();
    let mut s = r.start;
    while r.end - s > per {
        let e = floor_boundary(text, s + per).max(ceil_boundary(text, s + 1));
        out.push((s..e, true));
        s = e;
    }
    out.push((s..r.end, false));
    out
}

fn refine(
    text: &str,
    r: Range<usize>,
    cont: bool,
    cap: usize,
    out: &mut Vec<(Range<usize>, bool, usize)>,
) {
    let t = tokens::count(&text[r.clone()]);
    if t <= cap || r.len() < 8 {
        out.push((r, cont, t));
        return;
    }
    let mid = floor_boundary(text, r.start + r.len() / 2);
    let (cut, inside) = match text[r.start..mid].rfind('\n') {
        Some(i) => (r.start + i + 1, false),
        None => (mid, true),
    };
    if cut <= r.start || cut >= r.end {
        out.push((r, cont, t));
        return;
    }
    refine(text, r.start..cut, inside, cap, out);
    refine(text, cut..r.end, cont, cap, out);
}

fn floor_boundary(text: &str, mut i: usize) -> usize {
    i = i.min(text.len());
    while !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_boundary(text: &str, mut i: usize) -> usize {
    i = i.min(text.len());
    while !text.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joined(text: &str, pieces: &[(Range<usize>, bool, usize)]) -> String {
        pieces.iter().map(|(r, _, _)| &text[r.clone()]).collect()
    }

    #[test]
    fn pieces_fit_and_join_back_exactly() {
        let mut text = String::new();
        for i in 0..400 {
            text.push_str(&format!("line {i} with some words to count, ünïcode 😀\n"));
            if i % 37 == 0 {
                text.push('\n');
            }
        }
        let pieces = split_points(&text, 300, false);
        assert!(pieces.len() > 5);
        assert_eq!(joined(&text, &pieces), text);
        for (r, cont, t) in &pieces {
            assert!(*t <= 300, "{t} tokens");
            assert_eq!(*t, tokens::count(&text[r.clone()]));
            assert!(!cont, "no line here is longer than a piece");
            assert!(text[r.clone()].ends_with('\n'));
        }
    }

    #[test]
    fn grid_pieces_start_at_a_sheet_or_chunk_line() {
        let mut text = String::from("TM-GRID/1\n@file name=\"x.xlsx\"\n");
        for s in 0..3 {
            text.push_str(&format!(
                "@sheet {}/3 name=\"S{s}\"\n| row | a |\n| ---: | :--- |\n",
                s + 1
            ));
            for c in 0..4 {
                for r in 0..40 {
                    text.push_str(&format!(
                        "| {} | value {s}-{c}-{r} text |\n",
                        c * 40 + r + 2
                    ));
                }
                text.push_str(&format!(
                    "\n@cont body_rows={}-{} of 160\n| row | a |\n| ---: | :--- |\n",
                    c * 40 + 1,
                    c * 40 + 40
                ));
            }
            text.push_str("@end sheet\n\n");
        }
        let pieces = split_points(&text, 600, true);
        assert!(pieces.len() > 3);
        assert_eq!(joined(&text, &pieces), text);
        for (r, _, t) in pieces.iter().skip(1) {
            let head = &text[r.clone()];
            assert!(
                head.starts_with("@sheet ") || head.starts_with("@cont "),
                "{:?}",
                &head[..40.min(head.len())]
            );
            assert!(*t <= 600);
        }
    }

    #[test]
    fn a_line_longer_than_a_piece_is_cut_inside_and_says_so() {
        let long = "x1y2z3 ".repeat(3000);
        let text = format!("head\n{long}\ntail\n");
        let pieces = split_points(&text, 400, false);
        assert_eq!(joined(&text, &pieces), text);
        assert!(
            pieces.iter().any(|p| p.1),
            "some piece ends inside the line"
        );
        for (r, cont, t) in &pieces {
            assert!(*t <= 400, "{t}");
            assert_eq!(*cont, !text[r.clone()].ends_with('\n'), "{r:?}");
        }
    }
}
