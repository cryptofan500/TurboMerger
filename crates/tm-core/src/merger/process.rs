//! One file in, one block (or a skip) out: read, decode, slim, redact, count.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use super::{relative_display, Block, MergeConfig, MergeSkip};
use crate::scanner::SkipKind;
use crate::tokens;

static BASE64_RUN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z0-9+/]{200,}={0,2}").expect("base64 run regex"));

/// A processed file: its block and text, or the reason it was dropped.
/// `harvested` = labeled secret values (high confidence, propagated
/// unconditionally); `dense` = whole-file opaque tokens from a credential
/// dump (propagated behind a frequency guard).
pub(super) struct Processed {
    pub(super) result: std::result::Result<(Block, String), MergeSkip>,
    pub(super) harvested: Vec<String>,
    pub(super) dense: Vec<String>,
}

/// Read + decode + slim + redact + token-count a single already-vetted file.
/// Also returns the secrets harvested from the decoded text — harvested
/// BEFORE the credential-density exclusion, so an excluded credential file
/// still teaches the propagation pass its values.
pub(super) fn process_file(path: &Path, root: &Path, cfg: &MergeConfig) -> Processed {
    // Test hook, debug builds only: make every file slow so the deadline,
    // partial-output and stall paths can be exercised end to end.
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var("TM_TEST_SLOW_FILE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
    let mut harvested: Vec<String> = Vec::new();
    let mut dense: Vec<String> = Vec::new();
    let result = process_file_inner(path, root, cfg, &mut harvested, &mut dense);
    Processed {
        result,
        harvested,
        dense,
    }
}

fn process_file_inner(
    path: &Path,
    root: &Path,
    cfg: &MergeConfig,
    harvested: &mut Vec<String>,
    dense: &mut Vec<String>,
) -> std::result::Result<(Block, String), MergeSkip> {
    let relative = relative_display(root, path);

    let file = File::open(path).map_err(|e| {
        (
            relative.clone(),
            format!("unreadable: {}", e),
            SkipKind::NotCaptured,
        )
    })?;
    let mut buffer = Vec::new();
    if BufReader::new(file).read_to_end(&mut buffer).is_err() {
        return Err((relative, "unreadable".into(), SkipKind::NotCaptured));
    }

    // A UTF-16 BOM legitimizes the null bytes that the binary sniff would
    // otherwise reject, so BOM detection must run first.
    let bom = encoding_rs::Encoding::for_bom(&buffer);
    if bom.is_none() && !buffer.is_empty() {
        let check = 8192.min(buffer.len());
        if crate::security::is_binary_content(&buffer[..check]) {
            return Err((relative, "binary content".into(), SkipKind::Binary));
        }
    }

    let (mut content, utf8_note) = decode_text(buffer, bom);

    if cfg.redact {
        *harvested = crate::security::harvest_labeled_values(&content);
    }

    // Whole-file exclusion for credential-dense content (inline login tables,
    // Google app-passwords, key blocks) that per-line redaction can't fully
    // scrub. Checked on the raw decoded text, before any slimming/redaction.
    // Every opaque token of such a file is harvested for propagation — the
    // file IS a credential store, and its values echo in prose elsewhere
    // (grammar no labeled rule can parse).
    let cred_count = crate::security::credential_indicator_count(&content);
    if cred_count >= crate::security::CREDENTIAL_DENSITY_THRESHOLD {
        // Whole-token harvest only for credential DUMPS in document form: a
        // dense CODE file (a seed script with test logins) would contribute
        // its identifiers, and a long document that merely trips the density
        // rule (a paper's author e-mails) would contribute every citation and
        // model name — 1,011 corrupting edits on 120 arXiv papers (N-13).
        let ext = Path::new(&relative)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if cfg.redact
            && matches!(
                ext.as_str(),
                "md" | "markdown" | "txt" | "text" | "csv" | "log" | ""
            )
            && crate::security::is_credential_dump(&content, cred_count)
        {
            *dense = crate::security::harvest_dense_file_tokens(&content);
        }
        return Err((
            relative,
            format!(
                "credential-dense content ({} inline credentials) — excluded",
                cred_count
            ),
            SkipKind::NotCaptured,
        ));
    }

    let lang = Path::new(&relative)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    // Tree-sitter reductions (a failed parse keeps the original; redaction
    // still runs on whatever survives). Comment strip must run BEFORE
    // compression: elided bodies (`{ ... }`) are placeholders, not parseable
    // source, so the reverse order can't re-parse.
    let mut compressed = false;
    if cfg.strip_comments {
        if let Some(slim) = crate::compress::strip_comments(&content, &lang) {
            content = slim;
        }
    }
    if cfg.compress {
        if let Some(slim) = crate::compress::compress_signatures(&content, &lang) {
            content = slim;
            compressed = true;
        }
    }

    if cfg.truncate_base64 {
        content = BASE64_RUN
            .replace_all(&content, "[base64 omitted]")
            .into_owned();
    }
    if cfg.remove_empty_lines {
        let mut out = String::with_capacity(content.len());
        for line in content.lines() {
            if !line.trim().is_empty() {
                out.push_str(line);
                out.push('\n');
            }
        }
        content = out;
    }

    let redactions = if cfg.redact {
        let (clean, events) = crate::security::redact_secrets(&content);
        content = clean;
        events.into_iter().map(|ev| (ev.rule, ev.count)).collect()
    } else {
        Vec::new()
    };

    let tokens = tokens::count(&content);

    Ok((
        Block {
            relative,
            span: Default::default(),
            lang,
            tokens,
            utf8_note,
            redactions,
            compressed,
            synthetic: false,
        },
        content,
    ))
}

/// Decode raw file bytes to a String: BOM → strict UTF-8 → chardetng-guessed
/// legacy encoding → lossy UTF-8. Returns the text plus an optional note for
/// the Merge Report's "Decoding notes".
fn decode_text(
    buffer: Vec<u8>,
    bom: Option<(&'static encoding_rs::Encoding, usize)>,
) -> (String, Option<String>) {
    if let Some((enc, bom_len)) = bom {
        if enc == encoding_rs::UTF_8 {
            // Strip the BOM, then take the normal UTF-8 path below.
        } else {
            let (text, had_errors) = enc.decode_without_bom_handling(&buffer[bom_len..]);
            let note = if had_errors {
                format!(
                    "decoded from {} (BOM) with replacement characters",
                    enc.name()
                )
            } else {
                format!("decoded from {} (BOM)", enc.name())
            };
            return (text.into_owned(), Some(note));
        }
    }
    let start = bom.map(|(_, len)| len).unwrap_or(0);
    let body = &buffer[start..];

    match std::str::from_utf8(body) {
        Ok(s) => (s.to_string(), None),
        Err(_) => {
            // Strict UTF-8 already failed, so deny UTF-8 as a guess; a legacy
            // single-byte decode (windows-1252 default) never errors, matching
            // what editors do on auto-detect.
            let mut det = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Deny);
            det.feed(body, true);
            let enc = det.guess(None, chardetng::Utf8Detection::Deny);
            if enc != encoding_rs::UTF_8 {
                let (text, had_errors) = enc.decode_without_bom_handling(body);
                let note = if had_errors {
                    format!(
                        "not UTF-8 — decoded as {} (detected) with replacement characters",
                        enc.name()
                    )
                } else {
                    format!("not UTF-8 — decoded as {} (detected)", enc.name())
                };
                (text.into_owned(), Some(note))
            } else {
                let s = String::from_utf8_lossy(body).into_owned();
                let n = s.matches('\u{FFFD}').count();
                (
                    s,
                    Some(format!(
                        "not valid UTF-8 — {} byte(s) replaced with U+FFFD (consider re-saving as UTF-8)",
                        n
                    )),
                )
            }
        }
    }
}
