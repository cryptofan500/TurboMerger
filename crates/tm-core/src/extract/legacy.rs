//! Legacy binary Office: Word 97-2003 (`.doc`) and PowerPoint 97-2003
//! (`.ppt`), both OLE compound files (`cfb`).
//!
//! - Word keeps its text in the `WordDocument` stream, scattered in pieces
//!   that the piece table (the CLX in the `0Table`/`1Table` stream) puts in
//!   order; each piece is cp1252 ("compressed") or UTF-16LE ([MS-DOC] 2.9.38,
//!   2.9.73, 2.4.1). Every story is kept — body, footnotes, headers,
//!   comments, text boxes — in the order Word stores them.
//! - PowerPoint keeps slide and outline text in `TextCharsAtom` (UTF-16LE)
//!   and `TextBytesAtom` (cp1252) records of the `PowerPoint Document` stream
//!   ([MS-PPT] 2.2.6); a linear walk of the record tree finds them in order.
//!
//! A file whose structure does not parse still gives up its readable text: a
//! scan for UTF-16LE runs of the main stream (what `strings -el` finds).

use std::io::Read;
use std::path::Path;

/// Text of a Word 97-2003 document.
pub(super) fn extract_doc(path: &Path) -> Result<String, String> {
    let mut comp = cfb::open(path).map_err(|e| format!("not a readable Word 97-2003 file: {e}"))?;
    let word = read_stream(&mut comp, "/WordDocument")
        .ok_or("not a Word 97-2003 file (no WordDocument stream)")?;
    if word.len() < 0x1AA || u16_at(&word, 0) != Some(0xA5EC) {
        return utf16_runs(&word).ok_or_else(|| "Word file without a readable header".into());
    }
    let flags = u16_at(&word, 0x0A).unwrap_or(0);
    if flags & 0x0100 != 0 {
        return Err("password-protected Word document".into());
    }
    let table_name = if flags & 0x0200 != 0 {
        "/1Table"
    } else {
        "/0Table"
    };
    let pieces = read_stream(&mut comp, table_name).and_then(|table| {
        let fc = u32_at(&word, 0x01A2)? as usize;
        let lcb = u32_at(&word, 0x01A6)? as usize;
        piece_text(&word, table.get(fc..fc.checked_add(lcb)?)?)
    });
    match pieces {
        Some(text) if !text.trim().is_empty() => Ok(clean_word_text(&text)),
        _ => utf16_runs(&word).ok_or_else(|| "no text found in the Word document".into()),
    }
}

/// Text of a PowerPoint 97-2003 deck.
pub(super) fn extract_ppt(path: &Path) -> Result<String, String> {
    let mut comp =
        cfb::open(path).map_err(|e| format!("not a readable PowerPoint 97-2003 file: {e}"))?;
    let doc = read_stream(&mut comp, "/PowerPoint Document")
        .ok_or("not a PowerPoint 97-2003 file (no PowerPoint Document stream)")?;
    ppt_text(&doc)
}

/// Text atoms of a PowerPoint Document stream, in record order. The master
/// and handout containers are skipped whole: their text is the template
/// ("Click to edit the title text format", seven outline levels), which
/// would otherwise open every deck.
fn ppt_text(doc: &[u8]) -> Result<String, String> {
    let mut out = String::new();
    let mut parsed = false;
    let mut i = 0usize;
    while i + 8 <= doc.len() {
        let ver_inst = u16_at(doc, i).unwrap_or(0);
        let rec_type = u16_at(doc, i + 2).unwrap_or(0);
        let len = u32_at(doc, i + 4).unwrap_or(0) as usize;
        if ver_inst & 0x000F == 0x000F {
            match rec_type {
                0x03F8 | 0x0FC9 => i = i.saturating_add(8).saturating_add(len), // MainMaster, Handout
                0x03E8 => {
                    parsed = true; // DocumentContainer: the structure reads
                    i += 8;
                }
                _ => i += 8, // container: walk into it
            }
            continue;
        }
        let body = match doc.get(i + 8..(i + 8).saturating_add(len)) {
            Some(b) => b,
            None => break,
        };
        let text = match rec_type {
            0x0FA0 => Some(decode_utf16le(body)),
            0x0FA8 => Some(
                encoding_rs::WINDOWS_1252
                    .decode_without_bom_handling(body)
                    .0
                    .into_owned(),
            ),
            _ => None,
        };
        if let Some(t) = text {
            let t = t.replace(['\r', '\u{b}'], "\n");
            // A lone "*" is a field's stand-in (slide number, date, header,
            // footer) that PowerPoint fills at render time — not text.
            if !matches!(t.trim(), "" | "*") {
                out.push_str(t.trim_end());
                out.push('\n');
            }
        }
        i += 8 + len;
    }
    if out.trim().is_empty() {
        // A deck that parsed but has no slide text (pictures only) has none to
        // give; salvage is for a damaged file and would return the master.
        if parsed {
            return Err("no text found in the PowerPoint deck".into());
        }
        return utf16_runs(doc).ok_or_else(|| "no text found in the PowerPoint deck".into());
    }
    Ok(out)
}

fn read_stream<F: Read + std::io::Seek>(
    comp: &mut cfb::CompoundFile<F>,
    name: &str,
) -> Option<Vec<u8>> {
    let mut s = comp.open_stream(name).ok()?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// Concatenate the pieces the CLX lists, in character-position order.
fn piece_text(word: &[u8], clx: &[u8]) -> Option<String> {
    // Skip any Prc (property modifier) blocks, then the Pcdt.
    let mut i = 0usize;
    while *clx.get(i)? == 0x01 {
        let cb = u16_at(clx, i + 1)? as usize;
        i += 3 + cb;
    }
    if *clx.get(i)? != 0x02 {
        return None;
    }
    let lcb = u32_at(clx, i + 1)? as usize;
    let plc = clx.get(i + 5..i + 5 + lcb)?;
    if lcb < 4 || !(lcb - 4).is_multiple_of(12) {
        return None;
    }
    let n = (lcb - 4) / 12;
    let mut out = String::new();
    for k in 0..n {
        let cp_start = u32_at(plc, k * 4)? as usize;
        let cp_end = u32_at(plc, (k + 1) * 4)? as usize;
        let chars = cp_end.checked_sub(cp_start)?;
        let pcd = plc.get((n + 1) * 4 + k * 8..(n + 1) * 4 + k * 8 + 8)?;
        let fc_raw = u32_at(pcd, 2)?;
        if fc_raw & 0x4000_0000 != 0 {
            let off = ((fc_raw & 0x3FFF_FFFF) / 2) as usize;
            let bytes = word.get(off..off.checked_add(chars)?)?;
            out.push_str(
                &encoding_rs::WINDOWS_1252
                    .decode_without_bom_handling(bytes)
                    .0,
            );
        } else {
            let off = fc_raw as usize;
            let bytes = word.get(off..off.checked_add(chars.checked_mul(2)?)?)?;
            out.push_str(&decode_utf16le(bytes));
        }
    }
    Some(out)
}

/// Word's control characters to plain text: paragraph/cell/line/page marks
/// become line breaks or tabs, field instructions (between 0x13 and 0x14) go
/// and their results stay, object anchors and soft hyphens vanish.
fn clean_word_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut in_field_code = 0usize;
    for c in raw.chars() {
        match c {
            '\u{13}' => in_field_code += 1,
            '\u{14}' => in_field_code = in_field_code.saturating_sub(1),
            '\u{15}' => in_field_code = in_field_code.saturating_sub(1),
            _ if in_field_code > 0 => {}
            '\r' | '\u{b}' | '\u{c}' | '\u{e}' => out.push('\n'),
            '\u{7}' => out.push('\t'),
            '\u{1e}' => out.push('-'),
            '\u{1}' | '\u{8}' | '\u{1f}' | '\u{5}' | '\u{2}' | '\u{3}' | '\u{4}' => {}
            c => out.push(c),
        }
    }
    out
}

fn decode_utf16le(bytes: &[u8]) -> String {
    encoding_rs::UTF_16LE
        .decode_without_bom_handling(&bytes[..bytes.len() & !1])
        .0
        .into_owned()
}

/// Readable UTF-16LE runs (≥ 6 characters) — the salvage path for a file
/// whose structure does not parse.
fn utf16_runs(data: &[u8]) -> Option<String> {
    let mut out = String::new();
    let mut run = String::new();
    for pair in data.chunks_exact(2) {
        let u = u16::from_le_bytes([pair[0], pair[1]]);
        // Noncharacters (U+FDD0..U+FDEF, U+FFFE, U+FFFF) are never text; 0xFFFF
        // is common binary fill, so it must break a run rather than prefix one.
        let c = char::from_u32(u as u32)
            .filter(|c| !c.is_control() || matches!(c, '\r' | '\n' | '\t'))
            .filter(|_| !matches!(u, 0xFDD0..=0xFDEF | 0xFFFE..=0xFFFF));
        match c {
            Some(c) if u >= 0x20 || matches!(c, '\r' | '\n' | '\t') => run.push(c),
            _ => {
                if run.chars().filter(|c| c.is_alphanumeric()).count() >= 6 {
                    out.push_str(run.trim());
                    out.push('\n');
                }
                run.clear();
            }
        }
    }
    if run.chars().filter(|c| c.is_alphanumeric()).count() >= 6 {
        out.push_str(run.trim());
        out.push('\n');
    }
    (!out.trim().is_empty()).then_some(out)
}

fn u16_at(b: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(i..i + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_codes_drop_but_results_stay() {
        let raw = "Total \u{13} PAGE \\* MERGEFORMAT \u{14}7\u{15} pages\rNext\u{7}cell";
        assert_eq!(clean_word_text(raw), "Total 7 pages\nNext\tcell");
    }

    #[test]
    fn pieces_in_cp_order_mixed_encodings() {
        // WordDocument: "Hi " compressed at byte 100, "żółw" UTF-16 at byte 200.
        let mut word = vec![0u8; 300];
        word[100..103].copy_from_slice(b"Hi ");
        for (k, u) in "żółw".encode_utf16().enumerate() {
            word[200 + 2 * k..202 + 2 * k].copy_from_slice(&u.to_le_bytes());
        }
        // PlcPcd: CPs 0,3,7 then two PCDs (fc compressed flag for the first).
        let mut plc = Vec::new();
        for cp in [0u32, 3, 7] {
            plc.extend(cp.to_le_bytes());
        }
        let mut pcd = |fc: u32| {
            let mut p = vec![0u8; 8];
            p[2..6].copy_from_slice(&fc.to_le_bytes());
            plc.extend(p);
        };
        pcd(0x4000_0000 | (100 * 2));
        pcd(200);
        let mut clx = vec![0x02];
        clx.extend((plc.len() as u32).to_le_bytes());
        clx.extend(&plc);
        assert_eq!(piece_text(&word, &clx).as_deref(), Some("Hi żółw"));
    }

    fn utf16le(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    #[test]
    fn utf16_salvage_finds_runs() {
        // U+FFFF, U+FFFE (binary fill) sit directly against the text, so only
        // the noncharacter rule keeps them from prefixing the salvaged run.
        let mut data = vec![0xFFu8, 0xFF, 0xFE, 0xFF];
        data.extend(utf16le("Quarterly report"));
        data.extend([0, 0, 0xFF, 0xFF]);
        assert_eq!(utf16_runs(&data).as_deref(), Some("Quarterly report\n"));
    }

    fn rec(ver_inst: u16, rec_type: u16, body: &[u8]) -> Vec<u8> {
        let mut r = ver_inst.to_le_bytes().to_vec();
        r.extend(rec_type.to_le_bytes());
        r.extend((body.len() as u32).to_le_bytes());
        r.extend(body);
        r
    }

    #[test]
    fn ppt_master_template_text_is_skipped() {
        // MainMaster (0x03F8) holds "Click to edit…"; the Document container
        // (0x03E8) holds the slide text. Only the latter is the user's.
        let atom = |s: &str| rec(0, 0x0FA0, &utf16le(s));
        let master = rec(0x000F, 0x03F8, &atom("Click to edit the title text format"));
        let document = rec(0x000F, 0x03E8, &atom("Quarterly results"));
        let doc = [master, document].concat();
        assert_eq!(ppt_text(&doc).unwrap(), "Quarterly results\n");
    }

    #[test]
    fn ppt_field_placeholders_drop_but_asterisk_text_stays() {
        // "*" alone is a slide-number/date field; "* first point" is a bullet.
        let atom = |s: &str| rec(0, 0x0FA0, &utf16le(s));
        let body = [atom("*"), atom("* first point"), atom("*")].concat();
        let doc = rec(0x000F, 0x03E8, &body);
        assert_eq!(ppt_text(&doc).unwrap(), "* first point\n");
    }

    #[test]
    fn ppt_without_slide_text_is_not_salvaged_into_the_master() {
        // A pictures-only deck parses but has no text: report that, rather
        // than letting salvage hand back the master's template.
        let template = rec(0, 0x0FA0, &utf16le("Click to edit the title text format"));
        let doc = [rec(0x000F, 0x03F8, &template), rec(0x000F, 0x03E8, &[])].concat();
        assert!(ppt_text(&doc).is_err());
    }

    #[test]
    fn utf16_salvage_keeps_polish_letters() {
        // Latin Extended-A (U+0100..) is real text — a damaged Polish .doc must
        // keep its diacritics, so salvage may not narrow itself to ASCII.
        let mut data = vec![0u8, 0];
        data.extend(utf16le("Zażółć gęślą jaźń"));
        data.extend([0, 0]);
        assert_eq!(utf16_runs(&data).as_deref(), Some("Zażółć gęślą jaźń\n"));
    }
}
