//! "0 silent drops" (plan v1 N-01/N-02/N-03, repro A.2): every file under the
//! root is either merged or recorded with a reason — pruned directories with
//! counts, hidden files, symlinks, and entries hidden by ignore rules.

use std::fs;
use std::path::{Path, PathBuf};

use tm_core::scanner::{scan_text_files, ScanOptions, ScanResult, SkipKind};

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/")
}

fn all_files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            let ft = e.file_type().unwrap();
            if ft.is_dir() {
                stack.push(e.path());
            } else {
                out.push(rel(root, &e.path()));
            }
        }
    }
    out.sort();
    out
}

fn merged(root: &Path, scan: &ScanResult) -> Vec<String> {
    scan.files.iter().map(|f| rel(root, f)).collect()
}

fn skip<'a>(scan: &'a ScanResult, path: &str) -> &'a tm_core::scanner::SkipEntry {
    scan.skipped
        .iter()
        .find(|s| s.path == path)
        .unwrap_or_else(|| panic!("no record for {path}: {:#?}", scan.skipped))
}

/// v1 A.2 plus the artifacts that genuinely should be pruned.
fn build_a2(root: &Path) {
    // The seven files v7.7.0 lost silently (directory names at any depth).
    write(
        root,
        "packages/core/src/index.ts",
        "export const core = () => 42;\n",
    );
    write(root, "src/debug/mod.rs", "pub fn dbg() {}\n");
    write(
        root,
        "src/build/gen.py",
        "def build_step():\n    return 1\n",
    );
    write(root, "src/release/mod.rs", "pub fn rel() {}\n");
    write(
        root,
        "app/env/settings.py",
        "SETTINGS = {\"debug\": False}\n",
    );
    write(root, "coverage/report.py", "def measure():\n    pass\n");
    write(root, "vendor/mylib/lib.go", "package mylib\n");
    write(root, "main.rs", "fn main() { println!(\"hi\"); }\n");
    write(root, "Cargo.toml", "[package]\nname = \"fx\"\n");
    // Real artifacts, detected by marker or unambiguous name.
    write(root, "target/debug/fx.d", "deps\n");
    write(
        root,
        "node_modules/left-pad/index.js",
        "module.exports = 1;\n",
    );
    write(root, "node_modules/left-pad/package.json", "{}\n");
    write(root, "venv/pyvenv.cfg", "home = /usr/bin\n");
    write(root, "venv/lib/site.py", "x = 1\n");
    write(
        root,
        "tmpcache/CACHEDIR.TAG",
        "Signature: 8a477f597d28d172789f06886806bc55\n",
    );
    write(root, "tmpcache/blob.txt", "cached\n");
    // Hidden things.
    write(root, ".secrets/notes.md", "private\n");
    write(root, ".hidden_notes.txt", "hidden\n");
    // N-04/N-05: text that v7.7.0 sniffed as binary or dropped as "large
    // file with unknown extension".
    write(
        root,
        "MZ80_notes.md",
        "MZ-80 emulator notes\nThis is plain text.\n",
    );
    write(
        root,
        "ticket.md",
        "BZ-1234 ticket triage\nplain text again\n",
    );
    let subs: String = (1..60)
        .map(|i| format!("{i}\n00:00:{:02},000 --> 00:00:{:02},500\n这是一个关于机器学习的讲座字幕示例。\n\n", i % 60, i % 60))
        .collect();
    write(root, "subs/lecture.srt", &subs);
    let nb = format!(
        "{{\"cells\": [{{\"cell_type\": \"code\", \"source\": [\"x = 1\\n\"], \"outputs\": [{{\"data\": {{\"image/png\": \"{}\"}}}}]}}]}}\n",
        "iVBORw0KGgo".repeat(400)
    );
    write(root, "notebooks/analysis.ipynb", &nb);
    write(
        root,
        "assets/icon.svg",
        "<svg xmlns=\"http://www.w3.org/2000/svg\"><circle r=\"4\"/></svg>\n",
    );
    let big: String = (0..18000)
        .map(|i| format!("int f{i}(int x) {{ return x + {i}; }}\n"))
        .collect();
    write(root, "cpp/big_table.cc", &big);
    // An ignore rule.
    write(root, ".gitignore", "scratch/\n*.log\n");
    write(root, "scratch/a.txt", "a\n");
    write(root, "scratch/b.txt", "b\n");
    write(root, "debug.log", "log line\n");
}

#[test]
fn a2_every_file_is_merged_or_recorded() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("fx");
    build_a2(&root);
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("main.rs", root.join("main_link.rs")).unwrap();
        fs::write(tmp.path().join("outside.txt"), "outside\n").unwrap();
        std::os::unix::fs::symlink("../outside.txt", root.join("escape_link.txt")).unwrap();
        std::os::unix::fs::symlink("missing.txt", root.join("broken_link.txt")).unwrap();
        std::os::unix::fs::symlink("src", root.join("src_link")).unwrap();
        std::os::unix::fs::symlink(".secrets/notes.md", root.join("notes_link.md")).unwrap();
    }

    let scan = scan_text_files(&root, &ScanOptions::default()).expect("scan");
    let got = merged(&root, &scan);

    // N-01: the seven source files are merged now.
    for f in [
        "packages/core/src/index.ts",
        "src/debug/mod.rs",
        "src/build/gen.py",
        "src/release/mod.rs",
        "app/env/settings.py",
        "coverage/report.py",
        "vendor/mylib/lib.go",
        // N-04/N-05
        "MZ80_notes.md",
        "ticket.md",
        "subs/lecture.srt",
        "notebooks/analysis.ipynb",
        "assets/icon.svg",
        "cpp/big_table.cc",
    ] {
        assert!(got.contains(&f.to_string()), "{f} missing from {got:?}");
    }

    // Pruned directories are recorded with a reason and counts.
    let nm = skip(&scan, "node_modules/");
    assert_eq!(nm.kind, SkipKind::PrunedDir);
    assert!(nm.reason.contains("2 files"), "{}", nm.reason);
    assert!(skip(&scan, "target/").reason.contains("Cargo.toml"));
    assert!(skip(&scan, "venv/").reason.contains("pyvenv.cfg"));
    assert!(skip(&scan, "tmpcache/").reason.contains("CACHEDIR.TAG"));
    assert!(skip(&scan, ".secrets/").reason.contains("hidden directory"));
    assert!(skip(&scan, ".hidden_notes.txt")
        .reason
        .contains("hidden file"));

    // N-03: ignored entries are counted per rule, never listed by name.
    let dir_rule = skip(&scan, ".gitignore: scratch/");
    assert_eq!(dir_rule.kind, SkipKind::IgnoredByRule);
    assert!(
        dir_rule.reason.contains("2 files in 1 directory"),
        "{}",
        dir_rule.reason
    );
    assert!(skip(&scan, ".gitignore: *.log").reason.contains("1 file"));
    assert!(!scan
        .skipped
        .iter()
        .any(|s| s.path.contains("scratch/a.txt")));

    // N-02: symlinks are resolved and recorded.
    #[cfg(unix)]
    {
        assert!(skip(&scan, "main_link.rs")
            .reason
            .contains("content included there"));
        assert!(skip(&scan, "escape_link.txt")
            .reason
            .contains("outside the root"));
        assert_eq!(skip(&scan, "broken_link.txt").reason, "broken symlink");
        assert!(skip(&scan, "src_link/")
            .reason
            .contains("symlinked directory"));
        // A link to an in-root file that is not merged under its own name is
        // merged once under the link's name.
        assert!(got.contains(&"notes_link.md".to_string()), "{got:?}");
    }

    // The gate: nothing vanishes. Every file is merged, or covered by a
    // record for itself, a pruned parent directory, or an ignore rule.
    let ignore_rules = ["scratch/", "debug.log"];
    for f in all_files(&root) {
        let covered = got.contains(&f)
            || scan.skipped.iter().any(|s| {
                s.path == f
                    || s.path == format!("{f}/")
                    || (s.path.ends_with('/') && f.starts_with(&s.path))
            })
            || ignore_rules.iter().any(|r| f.starts_with(r));
        assert!(covered, "{f} vanished without a record");
    }
    assert_eq!(scan.stats.pruned_dirs, 5, "{:#?}", scan.skipped);
}

#[test]
fn documents_and_photos_are_reported_as_not_captured() {
    // N-06 / N-53: PDFs and photos used to vanish as "binary" (exit 0) or
    // leave "no text files found" (exit 1, nothing written).
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("docs");
    write(&root, "paper.pdf", "%PDF-1.7\n");
    write(&root, "photo.HEIC", "not really heic");
    write(&root, "logo.png", "\u{89}PNG");
    write(&root, "notes.md", "# notes\n");
    let scan = scan_text_files(&root, &ScanOptions::default()).unwrap();
    assert_eq!(skip(&scan, "paper.pdf").kind, SkipKind::NotCaptured);
    assert!(skip(&scan, "paper.pdf").reason.contains("document"));
    assert_eq!(skip(&scan, "photo.HEIC").kind, SkipKind::NotCaptured);
    assert!(skip(&scan, "photo.HEIC").reason.contains("photo"));
    assert_eq!(skip(&scan, "logo.png").kind, SkipKind::Binary);
}

#[test]
fn ancestor_ignore_files_apply_only_inside_a_worktree() {
    // v1 N-03: a dotfiles repo's .gitignore in $HOME hid files in unrelated
    // non-repo folders.
    let tmp = tempfile::tempdir().unwrap();
    let outer = tmp.path().join("home");
    write(&outer, ".gitignore", "*.md\n");
    let root = outer.join("papers");
    write(&root, "paper.md", "# a paper\n");
    let scan = scan_text_files(&root, &ScanOptions::default()).unwrap();
    assert_eq!(merged(&root, &scan), vec!["paper.md".to_string()]);

    // Inside a worktree the ancestor rule applies, and is named.
    fs::create_dir_all(outer.join(".git")).unwrap();
    let scan = scan_text_files(&root, &ScanOptions::default()).unwrap();
    assert!(merged(&root, &scan).is_empty());
    let rule = scan
        .skipped
        .iter()
        .find(|s| s.kind == SkipKind::IgnoredByRule)
        .expect("rule summary");
    assert!(
        rule.path.contains("outside the scanned folder") && rule.path.ends_with("*.md"),
        "{rule:?}"
    );
}

#[test]
fn user_globs_are_reported_as_such() {
    let tmp = tempfile::tempdir().unwrap();
    let root: PathBuf = tmp.path().join("g");
    write(&root, "src/a.rs", "fn a() {}\n");
    write(&root, "docs/b.md", "# b\n");
    let opts = ScanOptions {
        exclude_globs: vec!["docs/**".into()],
        ..ScanOptions::default()
    };
    let scan = scan_text_files(&root, &opts).unwrap();
    assert_eq!(merged(&root, &scan), vec!["src/a.rs".to_string()]);
    let g = skip(&scan, "--include/--exclude: user glob");
    assert_eq!(g.kind, SkipKind::IgnoredByRule);
}

/// Windows junctions are links: recorded, never followed, and a junction
/// cannot be picked as the root (D2; N-10 keeps other reparse points, such
/// as OneDrive placeholders, as ordinary entries).
#[cfg(windows)]
#[test]
fn junctions_are_recorded_and_not_followed() {
    let tmp = tempfile::tempdir().unwrap();
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret.rs"), "fn secret() {}\n").unwrap();
    let root = tmp.path().join("root");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(root.join("link"))
        .arg(&outside)
        .status()
        .expect("mklink runs");
    assert!(status.success(), "mklink /J needs no admin rights");

    let scan = scan_text_files(&root, &ScanOptions::default()).unwrap();
    let names: Vec<String> = scan
        .files
        .iter()
        .map(|f| {
            f.strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    assert_eq!(names, vec!["main.rs"], "the junction is not followed");
    let entry = scan
        .skipped
        .iter()
        .find(|s| s.path.starts_with("link"))
        .expect("the junction is recorded");
    assert!(
        entry.reason.contains("outside the root"),
        "{}",
        entry.reason
    );
    assert!(
        tm_core::security::validate_and_canonicalize(&root.join("link").to_string_lossy()).is_err()
    );
}
