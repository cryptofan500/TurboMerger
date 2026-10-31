//! S9 (HANDOFF_S8 §6.10 item 3.3, §6.11 C): `--max-tokens` is a contract.
//! A file larger than one part is split into pieces; no part is over the
//! budget; joined in order, the pieces are exactly the text the file gets in
//! an unsplit merge — a text file and a TM-GRID table alike — and a split
//! file still comes back byte for byte from the archive twin.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use tm_core::merger::{merge_files_with_progress, MergeConfig};
use tm_core::restore::{restore, RestoreOptions};
use tm_core::selection::SelectionPlan;

/// `## path` + fenced blocks of a merged text (content without the line
/// the closing fence follows), like `tm_verify.py md_blocks`.
fn md_blocks(text: &str) -> Vec<(String, String)> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if let Some(h) = lines[i].strip_prefix("## ") {
            if i + 2 < lines.len() && lines[i + 1].is_empty() && lines[i + 2].starts_with("```") {
                let fence: String = lines[i + 2].chars().take_while(|&c| c == '`').collect();
                let mut j = i + 3;
                while j < lines.len() && lines[j] != fence {
                    j += 1;
                }
                out.push((h.to_string(), lines[i + 3..j].join("\n")));
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Blocks by path, the pieces of a split file joined in order.
fn joined(blocks: Vec<(String, String)>) -> (BTreeMap<String, String>, usize) {
    let re = regex::Regex::new(r"^(.*) \(piece (\d+)/(\d+)(, line continues)?\)$").unwrap();
    let mut out = BTreeMap::new();
    let mut pieces = 0;
    let mut cur: Option<(String, u32, u32, String)> = None;
    for (h, c) in blocks {
        let Some(m) = re.captures(&h) else {
            assert!(cur.is_none(), "a piece is missing before {h}");
            out.insert(h, c);
            continue;
        };
        pieces += 1;
        let (base, k, n) = (
            m[1].to_string(),
            m[2].parse::<u32>().unwrap(),
            m[3].parse::<u32>().unwrap(),
        );
        let piece = if m.get(4).is_some() || k == n {
            c
        } else {
            c + "\n"
        };
        match &mut cur {
            None => {
                assert_eq!(k, 1, "{h}: does not start at piece 1");
                cur = Some((base, 2, n, piece));
            }
            Some((b, next, nn, acc)) => {
                assert!(*b == base && *next == k && *nn == n, "{h}: out of order");
                acc.push_str(&piece);
                *next += 1;
            }
        }
        if let Some((b, next, n, acc)) = cur.take() {
            if next > n {
                out.insert(b, acc);
            } else {
                cur = Some((b, next, n, acc));
            }
        }
    }
    assert!(cur.is_none(), "the last piece is missing");
    (out, pieces)
}

fn fixture(src: &Path) -> Vec<PathBuf> {
    fs::create_dir_all(src).unwrap();
    let mut md = String::from("# A long document\n\n");
    for i in 0..1500 {
        md.push_str(&format!(
            "Paragraph line {i}: words to count, ünïcode 😀, and a number {}.\n",
            i * 7
        ));
        if i % 25 == 24 {
            md.push('\n');
        }
    }
    // One line longer than any piece: it must be cut inside and say so.
    md.push_str(&"0123456789abcdef".repeat(4000));
    md.push_str("\nlast line\n");
    fs::write(src.join("big.md"), md).unwrap();
    let mut csv = String::from("id,name,note\n");
    for i in 0..2500 {
        csv.push_str(&format!("{i},Program {i},\"note with, comma {i}\"\n"));
    }
    fs::write(src.join("table.csv"), csv).unwrap();
    fs::write(src.join("small.txt"), "small and whole\n").unwrap();
    ["big.md", "table.csv", "small.txt"]
        .iter()
        .map(|n| src.join(n))
        .collect()
}

fn merge(
    src: &Path,
    files: &[PathBuf],
    out: &Path,
    max_tokens: Option<usize>,
    complete: bool,
) -> Vec<PathBuf> {
    let plan = SelectionPlan::from_toml(
        "version = 1\nmode = \"explicit\"\n[[file]]\npath = \"big.md\"\n[[file]]\npath = \"table.csv\"\ncolumns = [\"id\", \"name\", \"note\"]\n[[file]]\npath = \"small.txt\"\n",
    )
    .unwrap();
    let cfg = MergeConfig {
        redact: false,
        max_tokens,
        complete,
        selection: Some(plan),
        ..MergeConfig::default()
    };
    merge_files_with_progress(
        src,
        files,
        out,
        &cfg,
        &AtomicBool::new(false),
        |_, _, _| {},
        &[],
    )
    .expect("merge")
    .outputs
}

#[test]
fn no_part_is_over_the_budget_and_the_pieces_join_back_exactly() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let files = fixture(&src);
    let whole = merge(&src, &files, &tmp.path().join("whole.md"), None, false);
    assert_eq!(whole.len(), 1);
    let (want, none) = joined(md_blocks(&fs::read_to_string(&whole[0]).unwrap()));
    assert_eq!(none, 0);
    assert!(
        want["table.csv"].starts_with("TM-GRID/1"),
        "the CSV is a grid"
    );

    let budget = 6000;
    let parts = merge(
        &src,
        &files,
        &tmp.path().join("parts.md"),
        Some(budget),
        false,
    );
    assert!(parts.len() >= 5, "{} parts", parts.len());
    let mut all = String::new();
    for p in &parts {
        let text = fs::read_to_string(p).unwrap();
        let t = tm_core::tokens::count(&text);
        assert!(t <= budget, "{}: {t} tokens > {budget}", p.display());
        all.push_str(&text);
        all.push('\n');
    }
    let blocks = md_blocks(&all);
    assert!(
        blocks.iter().any(|(h, _)| h.ends_with(", line continues)")),
        "the long line is cut inside and says so"
    );
    let (got, pieces) = joined(blocks);
    assert!(pieces > 6, "{pieces} pieces");
    assert_eq!(
        got.keys().collect::<Vec<_>>(),
        want.keys().collect::<Vec<_>>()
    );
    for (path, text) in &want {
        assert!(
            got[path] == *text,
            "{path}: joined pieces differ from the unsplit block"
        );
    }
}

#[test]
fn a_split_file_comes_back_from_the_archive_twin() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let files = fixture(&src);
    let parts = merge(&src, &files, &tmp.path().join("twin.md"), Some(6000), true);
    assert!(parts.len() >= 5);
    let all: String = parts
        .iter()
        .map(|p| fs::read_to_string(p).unwrap())
        .collect();
    let big = all
        .lines()
        .find(|l| l.starts_with('{') && l.contains("\"path\":\"big.md\""))
        .expect("big.md in a manifest");
    assert!(
        big.contains("\"kind\":\"text\""),
        "rebuilt from its pieces, not embedded: {big}"
    );
    let dest = tmp.path().join("back");
    let r = restore(&parts, &dest, RestoreOptions::default()).expect("restore");
    assert_eq!(r.restored.len(), 3);
    for name in ["big.md", "table.csv", "small.txt"] {
        assert_eq!(
            fs::read(dest.join(name)).unwrap(),
            fs::read(src.join(name)).unwrap(),
            "{name}"
        );
    }
}
