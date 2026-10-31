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
