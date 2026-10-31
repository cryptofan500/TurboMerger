//! S8 stage 1: the archive twin (`--complete`) marks a file `text` only when
//! its block rebuilds it byte for byte (DeepSeek B3.3: `--remove-empty-lines`
//! left `kind: text` on files the block could no longer reproduce).

use std::fs;
use std::sync::atomic::AtomicBool;

use tm_core::merger::{merge_files_with_progress, MergeConfig};
use tm_core::scanner::{scan_text_files, ScanOptions};

fn manifest_kind(out: &str, path: &str) -> String {
    let line = out
        .lines()
        .find(|l| l.starts_with('{') && l.contains(&format!("\"path\":\"{path}\"")))
        .unwrap_or_else(|| panic!("{path} not in the manifest:\n{out}"));
    let v: serde_json::Value = serde_json::from_str(line).unwrap();
    v["kind"].as_str().unwrap().to_string()
}

#[test]
fn slimmed_files_are_binary_in_the_archive_twin() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("src");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("blank.txt"), "a\n\nb\n").unwrap();
    fs::write(root.join("plain.txt"), "x\ny\n").unwrap();
    fs::write(root.join("crlf.txt"), "p\r\nq\r\n").unwrap();
    fs::write(root.join("nofinal.txt"), "z").unwrap();
    let scan = scan_text_files(&root, &ScanOptions::default()).expect("scan");
    let out = tmp.path().join("twin.md");
    let cfg = MergeConfig {
        complete: true,
        embed_binary: Some(5),
        remove_empty_lines: true,
        redact: false,
        ..MergeConfig::default()
    };
    merge_files_with_progress(
        &root,
        &scan.files,
        &out,
        &cfg,
        &AtomicBool::new(false),
        |_, _, _| {},
        &scan.skipped,
    )
    .expect("merge");
    let text = fs::read_to_string(&out).unwrap();
    // The blank line is gone from the block, so the block cannot rebuild it:
    // binary, and embedded because --embed-binary allows it.
    assert_eq!(manifest_kind(&text, "blank.txt"), "binary");
    assert!(text.contains("## __binary__: blank.txt"), "{text}");
    // Untouched files still rebuild from their blocks.
    for p in ["plain.txt", "crlf.txt", "nofinal.txt"] {
        assert_eq!(manifest_kind(&text, p), "text", "{p}");
        assert!(!text.contains(&format!("## __binary__: {p}")), "{p}");
    }
}

// ---------------------------------------------------------------------------
// S9 (HANDOFF_S8 §6.10 items 1–2, §6.11 B): what the twin may and may not hold.

const EMPTY_SHA: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

fn manifest(out: &str, path: &str) -> serde_json::Value {
    let line = out
        .lines()
        .find(|l| l.starts_with('{') && l.contains(&format!("\"path\":\"{path}\"")))
        .unwrap_or_else(|| panic!("{path} not in the manifest:\n{out}"));
    serde_json::from_str(line).unwrap()
}

fn manifest_paths(out: &str) -> Vec<String> {
    out.lines()
        .filter(|l| l.starts_with('{') && l.contains("\"sha256\""))
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            v["path"].as_str().unwrap().to_string()
        })
        .collect()
}

fn b64_decode(s: &str) -> Vec<u8> {
    let val = |c: u8| -> u32 {
        u32::from(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => panic!("not base64: {c}"),
        })
    };
    let digits: Vec<u8> = s
        .bytes()
        .filter(|c| c.is_ascii_alphanumeric() || *c == b'+' || *c == b'/')
        .collect();
    let mut out = Vec::new();
    for chunk in digits.chunks(4) {
        let acc = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, &c)| acc | val(c) << (18 - 6 * i));
        out.push((acc >> 16) as u8);
        if chunk.len() > 2 {
            out.push((acc >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(acc as u8);
        }
    }
    out
}

/// Every `## __binary__: <path>` block of a twin, decoded.
fn embedded(out: &str) -> Vec<(String, Vec<u8>)> {
    let mut found = Vec::new();
    let mut lines = out.lines();
    while let Some(l) = lines.next() {
        if let Some(path) = l.strip_prefix("## __binary__: ") {
            let b64: String = lines
                .by_ref()
                .skip_while(|l| !l.starts_with("```base64"))
                .skip(1)
                .take_while(|l| !l.starts_with("```"))
                .collect();
            found.push((path.to_string(), b64_decode(&b64)));
        }
    }
    found
}

fn sha(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

/// Merge `names` (relative to `root`) into `<root>/../twin.md`; `on_done`
/// sees each file's relative path once it is merged.
fn merge_twin(
    root: &std::path::Path,
    names: &[&str],
    cfg: &MergeConfig,
    mut on_done: impl FnMut(&str),
) -> String {
    let files: Vec<_> = names.iter().map(|n| root.join(n)).collect();
    let out = root.parent().unwrap().join("twin.md");
    merge_files_with_progress(
        root,
        &files,
        &out,
        cfg,
        &AtomicBool::new(false),
        |_, _, f| on_done(f),
        &[],
    )
    .expect("merge");
    fs::read_to_string(&out).unwrap()
}

fn src_dir(tmp: &tempfile::TempDir) -> std::path::PathBuf {
    let root = tmp.path().join("src");
    fs::create_dir_all(&root).unwrap();
    root
}

#[test]
fn a_redacted_file_never_leaks_through_the_archive_twin() {
    // S8 §6.10 item 1, reproduced on 7e81fa4: the text block showed
    // [REDACTED], the base64 block decoded to the password.
    let tmp = tempfile::tempdir().unwrap();
    let root = src_dir(&tmp);
    let secret = "Vr7wKp2walKotwica91";
    fs::write(
        root.join("notes.md"),
        format!("# Notes\npassword: {secret}\n"),
    )
    .unwrap();
    fs::write(root.join("plain.txt"), "nothing to hide\n").unwrap();
    let cfg = MergeConfig {
        complete: true,
        embed_binary: Some(5),
        redact: true,
        ..MergeConfig::default()
    };
    let text = merge_twin(&root, &["notes.md", "plain.txt"], &cfg, |_| {});
    assert!(text.contains("REDACTED"), "the setup must redact:\n{text}");
    assert!(!text.contains(secret), "secret in the text:\n{text}");
    for (path, bytes) in embedded(&text) {
        assert!(
            !String::from_utf8_lossy(&bytes).contains(secret),
            "{path}: the secret is in the embedded bytes"
        );
    }
    let m = manifest(&text, "notes.md");
    assert_eq!(m["kind"], "withheld", "{m}");
    assert!(m["reason"].as_str().unwrap_or("").contains("redact"), "{m}");
    assert_eq!(manifest(&text, "plain.txt")["kind"], "text");
}

#[test]
fn a_file_gone_before_the_archive_pass_is_named_not_restored_empty() {
    // S8 §6.10 item 2: `fs::read(..).unwrap_or_default()` recorded a vanished
    // file as an empty one, and restore "passed" with 0 bytes.
    let tmp = tempfile::tempdir().unwrap();
    let root = src_dir(&tmp);
    fs::write(root.join("a.txt"), "stays\n").unwrap();
    fs::write(root.join("gone.txt"), "merged, then deleted\n").unwrap();
    let cfg = MergeConfig {
        complete: true,
        redact: false,
        ..MergeConfig::default()
    };
    let gone = root.join("gone.txt");
    let text = merge_twin(&root, &["a.txt", "gone.txt"], &cfg, |f| {
        if f == "gone.txt" {
            let _ = fs::remove_file(&gone);
        }
    });
    let m = manifest(&text, "gone.txt");
    assert_ne!(m["sha256"], EMPTY_SHA, "recorded as an empty file: {m}");
    assert_eq!(m["kind"], "missing", "{m}");
    assert!(!embedded(&text).iter().any(|(p, _)| p == "gone.txt"));
    assert_eq!(manifest(&text, "a.txt")["kind"], "text");
}

#[test]
fn archive_members_are_not_archived_as_files_but_the_archive_is() {
    // Member and continuation blocks are output, not files on disk; the old
    // pass read `pack.zip!/inner.txt` from disk and archived 0 bytes.
    let tmp = tempfile::tempdir().unwrap();
    let root = src_dir(&tmp);
    {
        let f = fs::File::create(root.join("pack.zip")).unwrap();
        let mut z = zip::ZipWriter::new(f);
        z.start_file("inner.txt", zip::write::FileOptions::default())
            .unwrap();
        std::io::Write::write_all(&mut z, b"hello from inside\n").unwrap();
        z.finish().unwrap();
    }
    let zip_bytes = fs::read(root.join("pack.zip")).unwrap();
    let cfg = MergeConfig {
        complete: true,
        redact: false,
        ..MergeConfig::default()
    };
    let text = merge_twin(&root, &["pack.zip"], &cfg, |_| {});
    assert!(
        text.contains("hello from inside"),
        "member text merged:\n{text}"
    );
    assert_eq!(
        manifest_paths(&text),
        vec!["pack.zip".to_string()],
        "{text}"
    );
    let m = manifest(&text, "pack.zip");
    assert_eq!(m["sha256"], sha(&zip_bytes));
    assert_eq!(m["kind"], "binary");
    let emb = embedded(&text);
    assert_eq!(emb.len(), 1);
    assert_eq!(emb[0], ("pack.zip".to_string(), zip_bytes));
}

#[test]
fn without_a_cap_every_file_its_text_cannot_rebuild_is_embedded() {
    // §6.11 B.1: `--complete` alone listed such files with no bytes.
    let tmp = tempfile::tempdir().unwrap();
    let root = src_dir(&tmp);
    let latin1 = b"caf\xe9 au lait\n".to_vec();
    fs::write(root.join("latin1.txt"), &latin1).unwrap();
    let cfg = MergeConfig {
        complete: true,
        redact: false,
        ..MergeConfig::default()
    };
    let text = merge_twin(&root, &["latin1.txt"], &cfg, |_| {});
    let m = manifest(&text, "latin1.txt");
    assert_eq!(m["kind"], "binary", "{m}");
    assert_eq!(embedded(&text), vec![("latin1.txt".to_string(), latin1)]);
}

#[test]
fn a_file_over_the_embed_cap_is_withheld_and_says_why() {
    let tmp = tempfile::tempdir().unwrap();
    let root = src_dir(&tmp);
    fs::write(root.join("latin1.txt"), b"caf\xe9\n").unwrap();
    let cfg = MergeConfig {
        complete: true,
        embed_binary: Some(0),
        redact: false,
        ..MergeConfig::default()
    };
    let text = merge_twin(&root, &["latin1.txt"], &cfg, |_| {});
    let m = manifest(&text, "latin1.txt");
    assert_eq!(m["kind"], "withheld", "{m}");
    assert!(
        m["reason"]
            .as_str()
            .unwrap_or("")
            .contains("--embed-binary"),
        "{m}"
    );
    assert!(embedded(&text).is_empty());
}
