//! Byte-exact text round-trip for apply-back (N-15).
//!
//! v7.7.0 decoded targets with `from_utf8_lossy` and wrote UTF-8 back, so a
//! one-line diff to a Windows-1252 file silently rewrote every `é` elsewhere
//! as U+FFFD. Here a target is decoded with the same detection chain the
//! merger uses (BOM → strict UTF-8 → chardetng legacy guess), and it is only
//! editable when that decode re-encodes to the identical bytes. New content is
//! encoded back to the file's own encoding (and BOM); characters that encoding
//! cannot represent are refused instead of becoming `&#NNNN;` or `?`.

use encoding_rs::Encoding;

/// The on-disk encoding of an apply-back target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextEncoding {
    Utf8 {
        bom: bool,
    },
    Utf16Le,
    Utf16Be,
    /// A legacy encoding detected by chardetng (no BOM).
    Legacy(&'static Encoding),
}

impl TextEncoding {
    pub fn label(self) -> String {
        match self {
            TextEncoding::Utf8 { bom: false } => "UTF-8".into(),
            TextEncoding::Utf8 { bom: true } => "UTF-8 (BOM)".into(),
            TextEncoding::Utf16Le => "UTF-16LE (BOM)".into(),
            TextEncoding::Utf16Be => "UTF-16BE (BOM)".into(),
            TextEncoding::Legacy(e) => e.name().to_string(),
        }
    }
}

/// Decode `bytes` for editing. Fails when the file is binary, malformed in
/// its detected encoding, or would not re-encode to the same bytes.
pub fn decode_for_edit(bytes: &[u8]) -> Result<(String, TextEncoding), String> {
    if let Some((enc, bom_len)) = Encoding::for_bom(bytes) {
        let body = &bytes[bom_len..];
        let (te, text) = if enc == encoding_rs::UTF_8 {
            let s = std::str::from_utf8(body)
                .map_err(|_| "invalid UTF-8 after a UTF-8 BOM — refusing to edit".to_string())?;
            (TextEncoding::Utf8 { bom: true }, s.to_string())
        } else {
            let te = if enc == encoding_rs::UTF_16LE {
                TextEncoding::Utf16Le
            } else {
                TextEncoding::Utf16Be
            };
            let s = enc
                .decode_without_bom_handling_and_without_replacement(body)
                .ok_or_else(|| format!("malformed {} — refusing to edit", te.label()))?;
            (te, s.into_owned())
        };
        return verify_round_trip(bytes, text, te);
    }

    // Validity first: well-formed UTF-8 without NULs is text even when it
    // happens to start with a magic number (`MZ-80 notes`, `BZ-1234`).
    if let Ok(s) = std::str::from_utf8(bytes) {
        if !bytes.contains(&0) {
            return Ok((s.to_string(), TextEncoding::Utf8 { bom: false }));
        }
    }
    let check = 8192.min(bytes.len());
    if bytes.contains(&0) || crate::security::is_binary_content(&bytes[..check]) {
        return Err("refusing to modify a binary file".into());
    }
    // Same guess the merger makes (so the LLM saw this decoding).
    let mut det = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Deny);
    det.feed(bytes, true);
    let enc = det.guess(None, chardetng::Utf8Detection::Deny);
    let text = enc
        .decode_without_bom_handling_and_without_replacement(bytes)
        .ok_or_else(|| {
            format!(
                "not valid UTF-8 and not decodable as {} — refusing to edit",
                enc.name()
            )
        })?;
    verify_round_trip(bytes, text.into_owned(), TextEncoding::Legacy(enc))
}

fn verify_round_trip(
    original: &[u8],
    text: String,
    te: TextEncoding,
) -> Result<(String, TextEncoding), String> {
    match encode(&text, te) {
        Ok(back) if back == original => Ok((text, te)),
        _ => Err(format!(
            "{} does not round-trip byte-for-byte — refusing to edit (it would be corrupted)",
            te.label()
        )),
    }
}

/// Encode `text` in `te` (BOM included). Refuses characters `te` cannot hold.
pub fn encode(text: &str, te: TextEncoding) -> Result<Vec<u8>, String> {
    match te {
        TextEncoding::Utf8 { bom } => {
            let mut out = Vec::with_capacity(text.len() + 3);
            if bom {
                out.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
            }
            out.extend_from_slice(text.as_bytes());
            Ok(out)
        }
        TextEncoding::Utf16Le | TextEncoding::Utf16Be => {
            let le = te == TextEncoding::Utf16Le;
            let mut out = Vec::with_capacity(2 + text.len() * 2);
            out.extend_from_slice(if le { &[0xFF, 0xFE] } else { &[0xFE, 0xFF] });
            for unit in text.encode_utf16() {
                out.extend_from_slice(&if le {
                    unit.to_le_bytes()
                } else {
                    unit.to_be_bytes()
                });
            }
            Ok(out)
        }
        TextEncoding::Legacy(enc) => {
            if enc.output_encoding() != enc {
                return Err(format!("{} cannot be written back", enc.name()));
            }
            let (bytes, _, had_errors) = enc.encode(text);
            if had_errors {
                let bad: String = text
                    .chars()
                    .filter(|&c| {
                        let mut buf = [0u8; 4];
                        enc.encode(c.encode_utf8(&mut buf)).2
                    })
                    .take(5)
                    .collect();
                return Err(format!(
                    "the new content has characters {} cannot represent ({:?}) — refusing to write",
                    enc.name(),
                    bad
                ));
            }
            Ok(bytes.into_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_round_trips_with_and_without_bom() {
        let (t, te) = decode_for_edit(b"caf\xC3\xA9\n").unwrap();
        assert_eq!(t, "café\n");
        assert_eq!(te, TextEncoding::Utf8 { bom: false });
        let (t2, te2) = decode_for_edit(b"\xEF\xBB\xBFhi\n").unwrap();
        assert_eq!(t2, "hi\n");
        assert_eq!(
            encode("hi\nthere\n", te2).unwrap(),
            b"\xEF\xBB\xBFhi\nthere\n"
        );
    }

    #[test]
    fn windows_1252_keeps_its_bytes() {
        // The v1 A.8 file: line one ends in é (0xE9).
        let bytes = b"line one caf\xE9\nline two\nline three\n";
        let (text, te) = decode_for_edit(bytes).unwrap();
        assert_eq!(te, TextEncoding::Legacy(encoding_rs::WINDOWS_1252));
        assert!(text.starts_with("line one café\n"));
        let edited = text.replace("line three", "line THREE");
        let out = encode(&edited, te).unwrap();
        assert_eq!(
            &out[..14],
            b"line one caf\xE9\n",
            "untouched line keeps its bytes"
        );
        assert_eq!(out, b"line one caf\xE9\nline two\nline THREE\n");
    }

    #[test]
    fn unrepresentable_characters_are_refused() {
        let (_, te) = decode_for_edit(b"caf\xE9\n").unwrap();
        let err = encode("café — 日本\n", te).unwrap_err();
        assert!(err.contains("cannot represent"), "{err}");
    }

    #[test]
    fn utf16_round_trips() {
        let mut bytes = vec![0xFF, 0xFE];
        for u in "wide text\r\n".encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        let (text, te) = decode_for_edit(&bytes).unwrap();
        assert_eq!(text, "wide text\r\n");
        assert_eq!(te, TextEncoding::Utf16Le);
        assert_eq!(encode(&text, te).unwrap(), bytes);
    }

    #[test]
    fn magic_lookalike_text_is_editable() {
        let (t, _) = decode_for_edit(b"MZ-80 emulator notes\n").unwrap();
        assert!(t.starts_with("MZ-80"));
    }

    #[test]
    fn binary_and_malformed_are_refused() {
        assert!(decode_for_edit(&[0x89, b'P', b'N', b'G', 0, 0, 0, 0])
            .unwrap_err()
            .contains("binary"));
        assert!(decode_for_edit(b"\xEF\xBB\xBFok \xFF\n").is_err());
        // Odd byte count after a UTF-16 BOM is malformed.
        assert!(decode_for_edit(&[0xFF, 0xFE, b'a', 0, b'b']).is_err());
    }
}
