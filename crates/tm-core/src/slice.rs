//! Sub-file selection for text files (ADR 0020): line ranges, Markdown
//! sections and pattern lines.
//!
//! Kept text is byte-verbatim (never re-rendered). Every omitted span leaves
//! one `[tm:omitted lines A-B …]` line carrying its original line numbers, so
//! nothing disappears silently and a verifier can re-cut the source and
//! compare. A selector that matches nothing is an error unless the plan says
//! `on_no_match = "warn"`.
//!
//! Tables (CSV/TSV/workbooks) are not sliced here: `columns`, `rows` and
//! `sheets` are applied by the TM-GRID/1 encoder (`extract::grid`).

use crate::selection::FileSel;
use regex::Regex;

/// What a slice kept, for the Merge Report and the `[tm:slice …]` line.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SliceReport {
    pub total_lines: usize,
    pub kept_lines: usize,
    pub total_bytes: usize,
    pub kept_bytes: usize,
    /// Markdown headings recognised (ATX, outside fences).
    pub total_sections: usize,
    /// Headings whose own line was kept.
    pub kept_sections: usize,
    /// Warnings collected under `on_no_match = "warn"`.
    pub warnings: Vec<String>,
}

/// True when the entry asks for a text slice (lines, sections, patterns or a
/// token cap).
pub fn has_text_slice(sel: &FileSel) -> bool {
    !sel.lines.is_empty()
        || !sel.sections.is_empty()
        || !sel.exclude_sections.is_empty()
        || !sel.match_lines.is_empty()
        || sel.max_tokens.is_some()
        || sel.preamble == Some(false)
}

/// True when the entry asks for a table selection (sheets, columns, rows,
/// header row).
pub fn has_table_slice(sel: &FileSel) -> bool {
    !sel.columns.is_empty()
        || !sel.rows.is_empty()
        || !sel.sheets.is_empty()
        || !sel.exclude_sheets.is_empty()
        || sel.header_row.is_some()
        || !sel.sheet_overrides.is_empty()
}

#[derive(Debug, Clone)]
struct Heading {
    level: u8,
    text: String,
    /// 0-based line index of the heading line.
    line: usize,
    /// Line index one past the heading's own text (next heading of any level).
    own_end: usize,
    /// Line index one past the heading's subtree (next heading of level <= this).
    subtree_end: usize,
}

/// Slice `text` per `sel`. `strict` = the plan's `on_no_match == error`.
/// Returns the text to merge (marker lines included) and a report.
pub fn slice_text(
    text: &str,
    sel: &FileSel,
    strict: bool,
) -> Result<(String, SliceReport), String> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let total_lines = lines.len();
    let total_bytes = text.len();
    let label = sel.label.as_deref().unwrap_or(&sel.path);
    let mut report = SliceReport {
        total_lines,
        total_bytes,
        ..Default::default()
    };

    if !has_text_slice(sel) {
        report.kept_lines = total_lines;
        report.kept_bytes = total_bytes;
        return Ok((text.to_string(), report));
    }
    if !sel.lines.is_empty() && (!sel.sections.is_empty() || !sel.match_lines.is_empty()) {
        return Err("`lines` cannot be combined with `sections` or `match`".into());
    }

    let headings = scan_headings(&lines);
    report.total_sections = headings.len();
    let mut keep = vec![false; total_lines];
    let mut no_match: Vec<String> = Vec::new();

    if !sel.lines.is_empty() {
        for spec in &sel.lines {
            let (a, b) = parse_line_range(spec)?;
            if a > total_lines {
                no_match.push(format!(
                    "line range {spec} starts after the last line ({total_lines})"
                ));
                continue;
            }
            for k in keep.iter_mut().take(b.min(total_lines)).skip(a - 1) {
                *k = true;
            }
        }
    } else {
        let first_heading = headings.first().map(|h| h.line).unwrap_or(total_lines);
        let whole = sel.sections.is_empty() && sel.match_lines.is_empty();
        if whole {
            keep.iter_mut().for_each(|k| *k = true);
            if sel.preamble == Some(false) {
                keep.iter_mut().take(first_heading).for_each(|k| *k = false);
            }
        } else {
            // Preamble rides along with sections by default; with `match`
            // alone only the matched lines are kept unless asked for.
            let preamble = sel.preamble.unwrap_or(!sel.sections.is_empty());
            if preamble {
                keep.iter_mut().take(first_heading).for_each(|k| *k = true);
            }
        }
        for pattern in &sel.sections {
            let hits = mark_sections(pattern, &headings, &mut keep, true);
            if hits == 0 {
                no_match.push(no_heading_message("sections", pattern, &headings));
            }
        }
        if !sel.match_lines.is_empty() {
            for raw in &sel.match_lines {
                let re =
                    Regex::new(raw).map_err(|e| format!("invalid `match` regex {raw:?}: {e}"))?;
                let mut hits = 0usize;
                for (i, line) in lines.iter().enumerate() {
                    let body = line.strip_suffix('\n').unwrap_or(line);
                    let body = body.strip_suffix('\r').unwrap_or(body);
                    if re.is_match(body) {
                        hits += 1;
                        keep[i] = true;
                        if sel.heading_context {
                            if let Some(h) = headings.iter().rev().find(|h| h.line <= i) {
                                keep[h.line] = true;
                            }
                        }
                    }
                }
                if hits == 0 {
                    no_match.push(format!("`match` regex {raw:?} matches no line"));
                }
            }
        }
        for pattern in &sel.exclude_sections {
            let hits = mark_sections(pattern, &headings, &mut keep, false);
            if hits == 0 {
                no_match.push(no_heading_message("exclude_sections", pattern, &headings));
            }
        }
    }

    if !no_match.is_empty() {
        if strict {
            return Err(no_match.join("; "));
        }
        report.warnings = no_match;
    }

    // Token cap: keep the longest prefix of kept lines that fits.
    let mut truncated_from: Option<usize> = None;
    if let Some(cap) = sel.max_tokens {
        let kept_idx: Vec<usize> = (0..total_lines).filter(|&i| keep[i]).collect();
        let joined = |n: usize| kept_idx[..n].iter().map(|&i| lines[i]).collect::<String>();
        if crate::tokens::count(&joined(kept_idx.len())) as u64 > cap {
            let (mut lo, mut hi) = (0usize, kept_idx.len());
            while lo < hi {
                let mid = (lo + hi).div_ceil(2);
                if crate::tokens::count(&joined(mid)) as u64 <= cap {
                    lo = mid;
                } else {
                    hi = mid - 1;
                }
            }
            if lo < kept_idx.len() {
                truncated_from = Some(kept_idx[lo]);
                for &i in &kept_idx[lo..] {
                    keep[i] = false;
                }
            }
        }
    }

    report.kept_lines = keep.iter().filter(|&&k| k).count();
    report.kept_bytes = (0..total_lines)
        .filter(|&i| keep[i])
        .map(|i| lines[i].len())
        .sum();
    report.kept_sections = headings.iter().filter(|h| keep[h.line]).count();

    let mut out = String::with_capacity(report.kept_bytes + 256);
    out.push_str(&format!(
        "[tm:slice label={} kept=\"{} of {} lines\" sections=\"{} of {}\" bytes=\"{} of {}\"]\n",
        json_str(label),
        report.kept_lines,
        total_lines,
        report.kept_sections,
        report.total_sections,
        report.kept_bytes,
        total_bytes
    ));
    let mut i = 0usize;
    while i < total_lines {
        let start = i;
        let kept = keep[i];
        while i < total_lines && keep[i] == kept {
            i += 1;
        }
        if kept {
            for (k, line) in lines[start..i].iter().enumerate() {
                out.push_str(line);
                // A last line without a newline still needs one before a marker.
                if start + k + 1 < total_lines && !line.ends_with('\n') {
                    out.push('\n');
                }
            }
            if i < total_lines && !out.ends_with('\n') {
                out.push('\n');
            }
        } else {
            let bytes: usize = lines[start..i].iter().map(|l| l.len()).sum();
            let first = headings.iter().find(|h| h.line >= start && h.line < i);
            let what = if truncated_from.is_some_and(|t| t <= start) {
                " (max_tokens)".to_string()
            } else if let Some(h) = first {
                let more = headings
                    .iter()
                    .filter(|h| h.line >= start && h.line < i)
                    .count()
                    - 1;
                if more > 0 {
                    format!(" (section {} + {more} more)", json_str(&h.text))
                } else {
                    format!(" (section {})", json_str(&h.text))
                }
            } else {
                String::new()
            };
            out.push_str(&format!(
                "[tm:omitted lines {}-{}{} — {} lines, {} bytes]\n",
                start + 1,
                i,
                what,
                i - start,
                bytes
            ));
        }
    }
    Ok((out, report))
}

/// Mark (`on = true`) or clear the spans of every heading `pattern` selects.
/// Returns the number of headings matched.
fn mark_sections(pattern: &str, headings: &[Heading], keep: &mut [bool], on: bool) -> usize {
    let (own, level, glob) = parse_section_pattern(pattern);
    let mut hits = 0;
    for h in headings {
        if level.is_some_and(|l| l != h.level) || !glob_match(glob, &h.text) {
            continue;
        }
        hits += 1;
        let end = if own { h.own_end } else { h.subtree_end };
        for k in keep.iter_mut().take(end).skip(h.line) {
            *k = on;
        }
    }
    hits
}

/// `[own:][H<n>:]glob` → (own, level, glob).
fn parse_section_pattern(pattern: &str) -> (bool, Option<u8>, &str) {
    let mut p = pattern;
    let own = if let Some(rest) = p.strip_prefix("own:") {
        p = rest;
        true
    } else {
        false
    };
    let mut level = None;
    if let Some(rest) = p.strip_prefix('H').or_else(|| p.strip_prefix('h')) {
        if let Some((n, tail)) = rest.split_once(':') {
            if let Ok(l) = n.parse::<u8>() {
                if (1..=6).contains(&l) {
                    level = Some(l);
                    p = tail;
                }
            }
        }
    }
    (own, level, p)
}

fn no_heading_message(key: &str, pattern: &str, headings: &[Heading]) -> String {
    let list: Vec<String> = headings
        .iter()
        .take(40)
        .map(|h| format!("H{}:{}", h.level, h.text))
        .collect();
    format!(
        "`{key}` pattern {pattern:?} matches no heading (headings: {})",
        if list.is_empty() {
            "none".to_string()
        } else {
            list.join(" | ")
        }
    )
}

/// "12-40" or "7" → 1-based inclusive (start, end).
fn parse_line_range(spec: &str) -> Result<(usize, usize), String> {
    let s = spec.trim();
    let (a, b) = match s.split_once('-') {
        Some((a, b)) => (a.trim(), b.trim()),
        None => (s, s),
    };
    let a: usize = a
        .parse()
        .map_err(|_| format!("invalid line range {spec:?}"))?;
    let b: usize = b
        .parse()
        .map_err(|_| format!("invalid line range {spec:?}"))?;
    if a == 0 || b < a {
        return Err(format!("invalid line range {spec:?}"));
    }
    Ok((a, b))
}

/// ATX headings (`#`..`######`) outside fenced code blocks.
fn scan_headings(lines: &[&str]) -> Vec<Heading> {
    let mut out: Vec<Heading> = Vec::new();
    let mut fence: Option<(u8, usize)> = None;
    for (i, raw) in lines.iter().enumerate() {
        let line = raw.trim_end_matches(['\n', '\r']);
        let indent = line.len() - line.trim_start_matches(' ').len();
        let rest = &line[indent..];
        if indent <= 3 {
            if let Some(&c) = rest.as_bytes().first() {
                if c == b'`' || c == b'~' {
                    let n = rest.bytes().take_while(|&b| b == c).count();
                    if n >= 3 {
                        match fence {
                            None => {
                                fence = Some((c, n));
                                continue;
                            }
                            Some((fc, fl)) if fc == c && n >= fl && rest[n..].trim().is_empty() => {
                                fence = None;
                                continue;
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        if fence.is_some() || indent > 3 || !rest.starts_with('#') {
            continue;
        }
        let hashes = rest.bytes().take_while(|&b| b == b'#').count();
        if hashes > 6 {
            continue;
        }
        let after = &rest[hashes..];
        if !(after.is_empty() || after.starts_with(' ') || after.starts_with('\t')) {
            continue;
        }
        let mut text = after.trim();
        // Optional closing sequence: spaces then #s at the end.
        let stripped = text.trim_end_matches('#');
        if stripped.len() < text.len() && (stripped.is_empty() || stripped.ends_with([' ', '\t'])) {
            text = stripped.trim_end();
        }
        out.push(Heading {
            level: hashes as u8,
            text: text.to_string(),
            line: i,
            own_end: lines.len(),
            subtree_end: lines.len(),
        });
    }
    for i in 0..out.len() {
        if i + 1 < out.len() {
            out[i].own_end = out[i + 1].line;
        }
        let lvl = out[i].level;
        if let Some(next) = out[i + 1..].iter().find(|h| h.level <= lvl) {
            out[i].subtree_end = next.line;
        }
    }
    out
}

/// (1-based line, level, text) of every heading the slicer recognises.
pub fn outline_headings(text: &str) -> Vec<(usize, u8, String)> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    scan_headings(&lines)
        .into_iter()
        .map(|h| (h.line + 1, h.level, h.text))
        .collect()
}

/// Whole-text glob with `*` and `?` (case-sensitive), on chars.
pub(crate) fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut mark = 0usize;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            pi += 1;
            mark = ti;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "intro line\n# Title\nt1\n## 0. Files\nf1\n## 1. Method\nm1\n### 1.1 Detail\nd1 https://a.example\n```\n## not a heading\n```\n## 2. Rankings\nr1\n";

    fn sel() -> FileSel {
        FileSel {
            path: "doc.md".into(),
            ..Default::default()
        }
    }

    /// Lines kept by the slice, read back through the markers.
    fn kept(out: &str) -> String {
        out.lines()
            .filter(|l| !l.starts_with("[tm:"))
            .map(|l| format!("{l}\n"))
            .collect()
    }

    #[test]
    fn glob_matches_whole_text() {
        assert!(glob_match("Summary*", "Summary of Results"));
        assert!(glob_match(
            "*TOP 90*",
            "CLAUDE V2 TOP 90 FOR THE 2027 MATCH"
        ));
        assert!(!glob_match("Summary*", "Other"));
        assert!(glob_match("H?", "H2"));
        assert!(glob_match("Pr\u{f3}by *", "Pr\u{f3}by test"));
    }

    #[test]
    fn exclude_removes_a_child_of_a_kept_parent() {
        let s = FileSel {
            exclude_sections: vec!["H2:0. Files".into()],
            ..sel()
        };
        let (out, rep) = slice_text(DOC, &s, true).unwrap();
        assert!(!kept(&out).contains("f1"), "{out}");
        assert!(kept(&out).contains("m1"));
        assert!(out.contains("[tm:omitted lines 4-5 (section \"0. Files\") — 2 lines, 15 bytes]"));
        assert_eq!(rep.kept_lines, rep.total_lines - 2);
    }

    #[test]
    fn match_keeps_only_the_line_and_its_heading() {
        let s = FileSel {
            sections: vec!["own:H1:Title".into()],
            match_lines: vec!["https?://".into()],
            heading_context: true,
            ..sel()
        };
        let (out, _) = slice_text(DOC, &s, true).unwrap();
        let k = kept(&out);
        assert!(
            k.contains("### 1.1 Detail\nd1 https://a.example\n"),
            "{out}"
        );
        assert!(
            !k.contains("m1"),
            "the whole section must not ride along: {out}"
        );
        assert!(k.starts_with("intro line\n# Title\nt1\n"));
    }

    #[test]
    fn headings_inside_fences_are_not_sections() {
        let s = FileSel {
            sections: vec!["H2:not a heading".into()],
            ..sel()
        };
        let err = slice_text(DOC, &s, true).unwrap_err();
        assert!(err.contains("matches no heading"), "{err}");
    }

    #[test]
    fn no_match_warns_when_not_strict() {
        let s = FileSel {
            sections: vec!["H2:Nope".into()],
            ..sel()
        };
        let (_, rep) = slice_text(DOC, &s, false).unwrap();
        assert_eq!(rep.warnings.len(), 1);
    }

    #[test]
    fn bad_regex_is_an_error() {
        let s = FileSel {
            match_lines: vec!["(".into()],
            ..sel()
        };
        assert!(slice_text(DOC, &s, true).is_err());
    }

    #[test]
    fn line_ranges_are_verbatim_with_numbered_markers() {
        let text = "l1\nl2\nl3\nl4\nl5";
        let s = FileSel {
            lines: vec!["2-3".into(), "5".into()],
            ..sel()
        };
        let (out, rep) = slice_text(text, &s, true).unwrap();
        assert_eq!(rep.kept_lines, 3);
        assert!(out.contains("[tm:omitted lines 1-1 — 1 lines, 3 bytes]\nl2\nl3\n[tm:omitted lines 4-4 — 1 lines, 3 bytes]\nl5"), "{out}");
        assert!(!out.ends_with('\n'), "the source had no final newline");
    }

    #[test]
    fn lines_and_sections_conflict() {
        let s = FileSel {
            lines: vec!["1".into()],
            sections: vec!["H1:*".into()],
            ..sel()
        };
        assert!(slice_text(DOC, &s, true).is_err());
    }

    #[test]
    fn subtree_and_own_differ() {
        let sub = FileSel {
            sections: vec!["H2:1. Method".into()],
            preamble: Some(false),
            ..sel()
        };
        let (out, _) = slice_text(DOC, &sub, true).unwrap();
        assert!(kept(&out).contains("d1"));
        let own = FileSel {
            sections: vec!["own:H2:1. Method".into()],
            preamble: Some(false),
            ..sel()
        };
        let (out, _) = slice_text(DOC, &own, true).unwrap();
        assert!(
            kept(&out).contains("m1") && !kept(&out).contains("d1"),
            "{out}"
        );
    }

    #[test]
    fn max_tokens_truncates_with_a_marker() {
        let text: String = (0..400)
            .map(|i| format!("line number {i} with some words\n"))
            .collect();
        let s = FileSel {
            max_tokens: Some(100),
            ..sel()
        };
        let (out, rep) = slice_text(&text, &s, true).unwrap();
        assert!(rep.kept_lines < 400 && rep.kept_lines > 5);
        assert!(out.contains("(max_tokens)"));
    }
}
