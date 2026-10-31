//! Sub-file byte-verbatim slicing for Markdown and text files (ADR 0020).

use std::ops::Range;
use regex::Regex;
use crate::selection::FileSel;

#[derive(Debug, Clone, Default)]
pub struct SliceReport {
    pub total_lines: usize,
    pub kept_lines: usize,
    pub total_bytes: usize,
    pub kept_bytes: usize,
    pub total_sections: usize,
    pub kept_sections: usize,
    pub omitted_sections: Vec<String>,
}

#[derive(Debug, Clone)]
struct Heading {
    level: u8,
    text: String,
    #[allow(dead_code)]
    line_idx: usize,
    start_byte: usize,
    end_byte: usize, // byte before next heading or EOF
    own_end_byte: usize, // byte before next heading of ANY level or EOF
}

/// Slices text based on FileSel parameters.
/// Returns the sliced text and a report.
pub fn slice_text(text: &str, sel: &FileSel) -> Result<(String, SliceReport), String> {
    if !sel.columns.is_empty() {
        return slice_csv_columns(text, sel);
    }

    if sel.lines.is_empty()
        && sel.sections.is_empty()
        && sel.exclude_sections.is_empty()
        && sel.match_lines.is_empty()
        && sel.max_tokens.is_none()
    {
        // No slice rules applied; return text as-is.
        let mut report = SliceReport::default();
        report.total_lines = text.lines().count();
        report.kept_lines = report.total_lines;
        report.total_bytes = text.len();
        report.kept_bytes = text.len();
        return Ok((text.to_string(), report));
    }

    // 1. Line slicing mode (lines = ["1-47", "91-104", ...])
    if !sel.lines.is_empty() {
        return slice_by_lines(text, sel);
    }

    // 2. Section and pattern matching mode
    slice_by_sections(text, sel)
}

fn slice_by_lines(text: &str, sel: &FileSel) -> Result<(String, SliceReport), String> {
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for l_spec in &sel.lines {
        let trimmed = l_spec.trim();
        if let Some((start_s, end_s)) = trimmed.split_once('-') {
            let start: usize = start_s.trim().parse().map_err(|_| format!("invalid line start: {start_s}"))?;
            let end: usize = end_s.trim().parse().map_err(|_| format!("invalid line end: {end_s}"))?;
            if start == 0 || end < start {
                return Err(format!("invalid line range: {l_spec}"));
            }
            ranges.push((start - 1)..(end));
        } else {
            let single: usize = trimmed.parse().map_err(|_| format!("invalid line number: {trimmed}"))?;
            if single == 0 {
                return Err(format!("invalid line number: {trimmed}"));
            }
            ranges.push((single - 1)..single);
        }
    }
    // Sort and merge ranges
    ranges.sort_by_key(|r| r.start);
    let mut merged: Vec<Range<usize>> = Vec::new();
    for r in ranges {
        if let Some(last) = merged.last_mut() {
            if r.start <= last.end {
                last.end = last.end.max(r.end);
                continue;
            }
        }
        merged.push(r);
    }

    let all_lines: Vec<&str> = text.split_inclusive('\n').collect();
    let total_lines = all_lines.len();
    let total_bytes = text.len();

    let mut out = String::new();
    let label = sel.label.as_deref().unwrap_or("slice");
    let mut kept_bytes = 0;
    let mut kept_lines = 0;

    let mut cursor = 0;
    let mut blocks: Vec<(bool, usize, usize, String)> = Vec::new(); // (kept, start_line_1based, end_line_1based, text)

    for r in merged {
        let start = r.start.min(total_lines);
        let end = r.end.min(total_lines);
        if start > cursor {
            let omitted_text: String = all_lines[cursor..start].concat();
            blocks.push((false, cursor + 1, start, omitted_text));
        }
        if end > start {
            let kept_text: String = all_lines[start..end].concat();
            blocks.push((true, start + 1, end, kept_text));
            cursor = end;
        }
    }
    if cursor < total_lines {
        let omitted_text: String = all_lines[cursor..total_lines].concat();
        blocks.push((false, cursor + 1, total_lines, omitted_text));
    }

    for (kept, start, end, chunk) in &blocks {
        if *kept {
            kept_bytes += chunk.len();
            kept_lines += end - start + 1;
        }
    }

    out.push_str(&format!(
        "[tm:slice label=\"{}\" kept=\"{} of {} lines\" bytes=\"{} of {}\"]\n",
        label, kept_lines, total_lines, kept_bytes, total_bytes
    ));

    for (kept, start, end, chunk) in blocks {
        if kept {
            out.push_str(&chunk);
            if !chunk.ends_with('\n') {
                out.push('\n');
            }
        } else {
            let lines_count = end - start + 1;
            let bytes_count = chunk.len();
            out.push_str(&format!(
                "[tm:omitted lines {}-{} — {} lines, {} bytes]\n",
                start, end, lines_count, bytes_count
            ));
        }
    }

    Ok((
        out,
        SliceReport {
            total_lines,
            kept_lines,
            total_bytes,
            kept_bytes,
            total_sections: 0,
            kept_sections: 0,
            omitted_sections: Vec::new(),
        },
    ))
}

fn slice_by_sections(text: &str, sel: &FileSel) -> Result<(String, SliceReport), String> {
    let raw_lines: Vec<&str> = text.split_inclusive('\n').collect();
    let total_lines = raw_lines.len();
    let total_bytes = text.len();

    // 1. Scan ATX headings outside fenced code blocks
    let mut headings: Vec<Heading> = Vec::new();
    let mut in_fence: Option<(char, usize)> = None;
    let mut byte_offset = 0;

    let mut line_byte_offsets: Vec<usize> = Vec::with_capacity(raw_lines.len() + 1);
    for line in &raw_lines {
        line_byte_offsets.push(byte_offset);
        let trimmed = line.trim_start();
        let leading_spaces = line.len() - trimmed.len();

        if leading_spaces <= 3 {
            let fence_char = trimmed.chars().next();
            if fence_char == Some('`') || fence_char == Some('~') {
                let fc = fence_char.unwrap();
                let fence_len = trimmed.chars().take_while(|&c| c == fc).count();
                if fence_len >= 3 {
                    match in_fence {
                        None => in_fence = Some((fc, fence_len)),
                        Some((cur_fc, cur_len)) => {
                            if fc == cur_fc && fence_len >= cur_len {
                                in_fence = None;
                            }
                        }
                    }
                }
            }
        }

        if in_fence.is_none() && leading_spaces <= 3 && trimmed.starts_with('#') {
            let hash_count = trimmed.chars().take_while(|&c| c == '#').count();
            if hash_count >= 1 && hash_count <= 6 {
                let rest = &trimmed[hash_count..];
                if rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t') {
                    let heading_text = rest.trim().trim_end_matches('#').trim();
                    headings.push(Heading {
                        level: hash_count as u8,
                        text: heading_text.to_string(),
                        line_idx: line_byte_offsets.len() - 1,
                        start_byte: byte_offset,
                        end_byte: total_bytes,
                        own_end_byte: total_bytes,
                    });
                }
            }
        }
        byte_offset += line.len();
    }
    line_byte_offsets.push(byte_offset);

    // Compute end_byte and own_end_byte for each heading
    for i in 0..headings.len() {
        // own_end_byte is the start of the immediately next heading (of any level)
        if i + 1 < headings.len() {
            headings[i].own_end_byte = headings[i + 1].start_byte;
        } else {
            headings[i].own_end_byte = total_bytes;
        }

        // end_byte is the start of the next heading of level <= current heading level
        let mut end = total_bytes;
        for j in (i + 1)..headings.len() {
            if headings[j].level <= headings[i].level {
                end = headings[j].start_byte;
                break;
            }
        }
        headings[i].end_byte = end;
    }

    // 2. Determine which spans to keep
    let mut keep_spans: Vec<(usize, usize)> = Vec::new(); // [start_byte, end_byte]
    let preamble = sel.preamble.unwrap_or(true);
    let first_heading_start = headings.first().map(|h| h.start_byte).unwrap_or(total_bytes);

    if preamble && first_heading_start > 0 {
        keep_spans.push((0, first_heading_start));
    }

    let mut kept_sections_count = 0;
    let mut omitted_sections_list: Vec<String> = Vec::new();

    for h in &headings {
        let is_included = if sel.sections.is_empty() {
            true
        } else {
            sel.sections.iter().any(|pattern| match_heading_pattern(pattern, h))
        };

        let is_excluded = sel.exclude_sections.iter().any(|pattern| match_heading_pattern(pattern, h));

        if is_included && !is_excluded {
            kept_sections_count += 1;
            let span_end = if sel.sections.iter().any(|p| p.starts_with("own:") && match_heading_pattern(p, h)) {
                h.own_end_byte
            } else {
                h.end_byte
            };
            keep_spans.push((h.start_byte, span_end));
        } else {
            omitted_sections_list.push(format!("{}. {}", h.level, h.text));
        }
    }

    // 3. Pattern lines (match = ["regex", ...], heading_context = true)
    if !sel.match_lines.is_empty() {
        let mut regexes = Vec::new();
        for m in &sel.match_lines {
            if let Ok(re) = Regex::new(m) {
                regexes.push(re);
            }
        }

        for (idx, line) in raw_lines.iter().enumerate() {
            let line_start = line_byte_offsets[idx];
            let line_end = line_byte_offsets[idx + 1];

            if regexes.iter().any(|re| re.is_match(line)) {
                keep_spans.push((line_start, line_end));
                if sel.heading_context {
                    // Find nearest preceding heading
                    if let Some(h) = headings.iter().rev().find(|h| h.start_byte <= line_start) {
                        keep_spans.push((h.start_byte, h.own_end_byte.min(line_start)));
                    }
                }
            }
        }
    }

    // 4. Sort and merge keep_spans
    keep_spans.sort_by_key(|s| s.0);
    let mut merged_spans: Vec<(usize, usize)> = Vec::new();
    for span in keep_spans {
        if let Some(last) = merged_spans.last_mut() {
            if span.0 <= last.1 {
                last.1 = last.1.max(span.1);
                continue;
            }
        }
        merged_spans.push(span);
    }

    // 5. Construct output with omission markers
    let mut out = String::new();
    let label = sel.label.as_deref().unwrap_or("slice");
    let mut kept_bytes = 0;
    let mut cursor = 0;

    for (start, end) in &merged_spans {
        let s = (*start).min(total_bytes);
        let e = (*end).min(total_bytes);
        if s > cursor {
            let omitted_str = &text[cursor..s];
            let omitted_lines = omitted_str.lines().count();
            let omitted_bytes = omitted_str.len();

            // Find if this omission corresponds to a specific heading
            let omitted_heading = headings.iter().find(|h| h.start_byte >= cursor && h.start_byte < s);
            if let Some(oh) = omitted_heading {
                out.push_str(&format!(
                    "[tm:omitted section \"{}\" — {} lines, {} bytes]\n",
                    oh.text, omitted_lines, omitted_bytes
                ));
            } else {
                let start_line = byte_to_line(&line_byte_offsets, cursor) + 1;
                let end_line = byte_to_line(&line_byte_offsets, s);
                out.push_str(&format!(
                    "[tm:omitted lines {}-{} — {} lines, {} bytes]\n",
                    start_line, end_line, omitted_lines, omitted_bytes
                ));
            }
        }
        if e > s {
            let chunk = &text[s..e];
            out.push_str(chunk);
            if !chunk.ends_with('\n') {
                out.push('\n');
            }
            kept_bytes += chunk.len();
            cursor = e;
        }
    }

    if cursor < total_bytes {
        let omitted_str = &text[cursor..total_bytes];
        let omitted_lines = omitted_str.lines().count();
        let omitted_bytes = omitted_str.len();
        let start_line = byte_to_line(&line_byte_offsets, cursor) + 1;
        let end_line = total_lines;
        out.push_str(&format!(
            "[tm:omitted lines {}-{} — {} lines, {} bytes]\n",
            start_line, end_line, omitted_lines, omitted_bytes
        ));
    }

    let kept_lines_count = out.lines().count();
    let final_slice_header = format!(
        "[tm:slice label=\"{}\" kept=\"{} of {} sections\" bytes=\"{} of {}\"]\n",
        label, kept_sections_count, headings.len(), kept_bytes, total_bytes
    );

    let result_text = format!("{}{}", final_slice_header, out);

    Ok((
        result_text,
        SliceReport {
            total_lines,
            kept_lines: kept_lines_count,
            total_bytes,
            kept_bytes,
            total_sections: headings.len(),
            kept_sections: kept_sections_count,
            omitted_sections: omitted_sections_list,
        },
    ))
}

fn byte_to_line(offsets: &[usize], byte_pos: usize) -> usize {
    match offsets.binary_search(&byte_pos) {
        Ok(idx) => idx,
        Err(idx) => idx.saturating_sub(1),
    }
}

fn match_heading_pattern(pattern: &str, h: &Heading) -> bool {
    let mut p = pattern;
    if p.starts_with("own:") {
        p = &p[4..];
    }
    if p.starts_with("H") || p.starts_with("h") {
        if let Some(colon) = p.find(':') {
            let level_str = &p[1..colon];
            if let Ok(lvl) = level_str.parse::<u8>() {
                if h.level != lvl {
                    return false;
                }
                p = &p[colon + 1..];
            }
        }
    }
    glob_match(p, &h.text)
}

fn glob_match(pattern: &str, text: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    // Simple glob with * and ?
    let p_bytes = pattern.as_bytes();
    let t_bytes = text.as_bytes();
    let mut p_idx = 0;
    let mut t_idx = 0;
    let mut star_p = None;
    let mut match_t = 0;

    while t_idx < t_bytes.len() {
        if p_idx < p_bytes.len() && (p_bytes[p_idx] == b'?' || p_bytes[p_idx] == t_bytes[t_idx]) {
            p_idx += 1;
            t_idx += 1;
        } else if p_idx < p_bytes.len() && p_bytes[p_idx] == b'*' {
            star_p = Some(p_idx);
            p_idx += 1;
            match_t = t_idx;
        } else if let Some(sp) = star_p {
            p_idx = sp + 1;
            match_t += 1;
            t_idx = match_t;
        } else {
            return false;
        }
    }
    while p_idx < p_bytes.len() && p_bytes[p_idx] == b'*' {
        p_idx += 1;
    }
    p_idx == p_bytes.len()
}

fn parse_csv_line(line: &str, delimiter: char) -> Vec<String> {
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' {
            if in_quotes && chars.peek() == Some(&'"') {
                cur.push('"');
                chars.next();
            } else {
                in_quotes = !in_quotes;
            }
        } else if c == delimiter && !in_quotes {
            fields.push(cur);
            cur = String::new();
        } else {
            cur.push(c);
        }
    }
    fields.push(cur);
    fields
}

fn format_csv_line(fields: &[&str], delimiter: char) -> String {
    let mut out = String::new();
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            out.push(delimiter);
        }
        if f.contains(delimiter) || f.contains('"') || f.contains('\n') || f.contains('\r') {
            out.push('"');
            out.push_str(&f.replace('"', "\"\""));
            out.push('"');
        } else {
            out.push_str(f);
        }
    }
    out
}

fn slice_csv_columns(text: &str, sel: &FileSel) -> Result<(String, SliceReport), String> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return Ok((String::new(), SliceReport::default()));
    }
    let delimiter = if lines[0].contains('\t') { '\t' } else { ',' };
    let headers = parse_csv_line(lines[0], delimiter);
    let mut col_indices = Vec::new();
    for col_name in &sel.columns {
        let norm = col_name.trim().to_ascii_lowercase();
        if let Some(idx) = headers.iter().position(|h| h.trim().to_ascii_lowercase() == norm) {
            col_indices.push(idx);
        }
    }
    if col_indices.is_empty() {
        return Ok((text.to_string(), SliceReport::default()));
    }

    let mut out = String::with_capacity(text.len());
    for line in lines {
        let fields = parse_csv_line(line, delimiter);
        let projected: Vec<&str> = col_indices
            .iter()
            .map(|&i| fields.get(i).map(|s| s.as_str()).unwrap_or(""))
            .collect();
        out.push_str(&format_csv_line(&projected, delimiter));
        out.push('\n');
    }

    let total_lines = text.lines().count();
    let total_bytes = text.len();
    let kept_lines = out.lines().count();
    let kept_bytes = out.len();

    Ok((
        out,
        SliceReport {
            total_lines,
            kept_lines,
            total_bytes,
            kept_bytes,
            total_sections: 0,
            kept_sections: 0,
            omitted_sections: Vec::new(),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_glob_match() {
        assert!(glob_match("Summary*", "Summary of Results"));
        assert!(glob_match("*TOP 90*", "CLAUDE V2 TOP 90 FOR THE 2027 MATCH"));
        assert!(!glob_match("Summary*", "Other"));
        assert!(glob_match("H?", "H2"));
    }

    #[test]
    fn test_line_slicing() {
        let text = "line 1\nline 2\nline 3\nline 4\nline 5\n";
        let sel = FileSel {
            lines: vec!["2-4".into()],
            ..Default::default()
        };
        let (sliced, report) = slice_text(text, &sel).unwrap();
        assert!(sliced.contains("line 2\nline 3\nline 4\n"));
        assert!(sliced.contains("[tm:omitted lines 1-1"));
        assert!(sliced.contains("[tm:omitted lines 5-5"));
        assert_eq!(report.total_lines, 5);
        assert_eq!(report.kept_lines, 3);
    }

    #[test]
    fn test_csv_column_projection() {
        let csv = "Rank,Program,City,State,Notes\n1,General FM,Springfield,IL,Top program\n2,University IM,Shelbyville,IL,Good\n";
        let sel = FileSel {
            columns: vec!["Rank".into(), "Program".into(), "State".into()],
            ..Default::default()
        };
        let (projected, report) = slice_text(csv, &sel).unwrap();
        assert_eq!(projected, "Rank,Program,State\n1,General FM,IL\n2,University IM,IL\n");
        assert_eq!(report.total_lines, 3);
        assert_eq!(report.kept_lines, 3);
    }
}
