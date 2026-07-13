//! End-to-end apply-back (T3-3) tests: parse real reply shapes against a
//! fixture tree, apply with backups, restore, and prove the safety rails
//! (traversal, binary, delete, changed-on-disk) hold.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use turbomerger::applyback::{apply_files, build_preview, parse_reply, restore_last};
use turbomerger::merger::{merge_files_with_progress, MergeConfig, OutputFormat};
use turbomerger::scanner::{scan_text_files, ScanOptions};

struct Fixture {
    root: PathBuf,
    _tmp: tempfile::TempDir,
}

fn build_fixture() -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("apply_repo");
    let write = |rel: &str, content: &[u8]| {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    };
    write("src/main.rs", b"fn main() {\n    println!(\"one\");\n}\n");
    write(
        "src/lib.rs",
        b"pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
    );
    write("README.md", b"# Fixture\n\ndocs\n");
    write(
        "data/img.png",
        &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0],
    );
    write("win.txt", b"alpha\r\nbeta\r\n");
    // Mixed endings (LF body, one CRLF line): must round-trip byte-exact.
    write("mixed.md", b"## heading-ish content\n\n```\nnope\n```\r\n");
    Fixture { root, _tmp: tmp }
}

#[test]
fn markdown_reply_applies_creates_and_restores() {
    let fx = build_fixture();
    let reply = "Here are the changes.\n\n\
## src/main.rs\n\n\
```rust\nfn main() {\n    println!(\"two\");\n}\n```\n\n\
## docs/new_guide.md\n\n\
```markdown\n# Guide\n\nfresh file\n```\n";

    let changes = parse_reply(reply);
    assert_eq!(changes.len(), 2);
    let built = build_preview(&fx.root, &changes).expect("preview");

    // Preview is honest: one modify, one create, diffs counted.
    let main = &built.preview.files[0];
    assert_eq!(main.rel_path, "src/main.rs");
    assert_eq!(main.action, "modify");
    assert!(main.ok && !main.identical);
    assert!(main.adds >= 1 && main.dels >= 1);
    assert!(!main.diff.is_empty());
    let guide = &built.preview.files[1];
    assert_eq!(guide.action, "create");

    // Preview wrote NOTHING (dry-run default).
    assert!(fs::read_to_string(fx.root.join("src/main.rs"))
        .unwrap()
        .contains("one"));
    assert!(!fx.root.join("docs/new_guide.md").exists());
    assert!(!fx.root.join(".turbomerger").exists());

    // Apply: contents land, backup + manifest exist.
    let outcome = apply_files(&fx.root, &built.ready).expect("apply");
    assert_eq!(outcome.applied.len(), 2);
    assert!(outcome.failed.is_empty());
    assert!(fs::read_to_string(fx.root.join("src/main.rs"))
        .unwrap()
        .contains("two"));
    assert_eq!(
        fs::read_to_string(fx.root.join("docs/new_guide.md")).unwrap(),
        "# Guide\n\nfresh file\n"
    );
    let backup_dir = PathBuf::from(outcome.backup_dir.expect("backup dir"));
    assert!(backup_dir.starts_with(fx.root.join(".turbomerger").join("backups")));
    assert!(backup_dir.join("manifest.json").exists());
    let backed = fs::read_to_string(backup_dir.join("files").join("src").join("main.rs")).unwrap();
    assert!(backed.contains("one"), "backup holds the original");

    // Restore: original back, created file gone.
    let restore = restore_last(&fx.root).expect("restore");
    assert_eq!(restore.restored, vec!["src/main.rs".to_string()]);
    assert_eq!(restore.deleted, vec!["docs/new_guide.md".to_string()]);
    assert!(fs::read_to_string(fx.root.join("src/main.rs"))
        .unwrap()
        .contains("one"));
    assert!(!fx.root.join("docs/new_guide.md").exists());
}

#[test]
fn own_markdown_output_round_trips_as_identical() {
    // Merge the fixture, paste the merged output straight back in:
    // every section must parse and be byte-identical to disk.
    let fx = build_fixture();
    let scan = scan_text_files(&fx.root, &ScanOptions::default()).expect("scan");
    let out = fx._tmp.path().join("roundtrip.md");
    let cancel = AtomicBool::new(false);
    merge_files_with_progress(
        &fx.root,
        &scan.files,
        &out,
        &MergeConfig::default(),
        &cancel,
        |_, _, _| {},
        &scan.skipped,
    )
    .expect("merge");

    let merged = fs::read_to_string(&out).unwrap();
    let changes = parse_reply(&merged);
    assert!(
        changes.len() >= 4,
        "all merged sections should parse: got {}",
        changes.len()
    );
    let built = build_preview(&fx.root, &changes).expect("preview");
    for f in &built.preview.files {
        assert!(f.ok, "{}: {}", f.rel_path, f.note);
        assert!(
            f.identical,
            "{} must round-trip identical (CRLF preserved), diff: +{} -{}",
            f.rel_path, f.adds, f.dels
        );
    }
    assert!(
        built.ready.is_empty(),
        "identical files are never re-written"
    );
}

#[test]
fn own_cxml_output_round_trips_as_identical() {
    let fx = build_fixture();
    let scan = scan_text_files(&fx.root, &ScanOptions::default()).expect("scan");
    let out = fx._tmp.path().join("roundtrip.xml");
    let cancel = AtomicBool::new(false);
    let cfg = MergeConfig {
        format: OutputFormat::Cxml,
        ..MergeConfig::default()
    };
    merge_files_with_progress(
        &fx.root,
        &scan.files,
        &out,
        &cfg,
        &cancel,
        |_, _, _| {},
        &scan.skipped,
    )
    .expect("merge");

    let merged = fs::read_to_string(&out).unwrap();
    let changes = parse_reply(&merged);
    assert!(
        changes.len() >= 4,
        "cxml sections should parse: {}",
        changes.len()
    );
    assert!(
        !changes.iter().any(|c| c.path.contains("MERGE_INFO")),
        "synthetic MERGE_INFO document must be skipped"
    );
    let built = build_preview(&fx.root, &changes).expect("preview");
    for f in &built.preview.files {
        assert!(
            f.ok && f.identical,
            "{} not identical: {}",
            f.rel_path,
            f.note
        );
    }
}

#[test]
fn unified_diff_modifies_and_dev_null_creates() {
    let fx = build_fixture();
    let reply = "\
--- a/src/lib.rs\n\
+++ b/src/lib.rs\n\
@@ -1,3 +1,3 @@\n\
 pub fn add(a: i32, b: i32) -> i32 {\n\
-    a + b\n\
+    a.wrapping_add(b)\n\
 }\n\
--- /dev/null\n\
+++ b/src/newmod.rs\n\
@@ -0,0 +1,2 @@\n\
+pub fn fresh() -> u8 {\n\
+    7\n\
";
    let changes = parse_reply(reply);
    assert_eq!(changes.len(), 2);
    let built = build_preview(&fx.root, &changes).expect("preview");
    assert!(
        built.preview.files.iter().all(|f| f.ok),
        "{:?}",
        built.preview.files
    );

    let outcome = apply_files(&fx.root, &built.ready).expect("apply");
    assert_eq!(outcome.applied.len(), 2);
    let lib = fs::read_to_string(fx.root.join("src/lib.rs")).unwrap();
    assert!(lib.contains("a.wrapping_add(b)"), "{}", lib);
    assert!(!lib.contains("    a + b"), "{}", lib);
    assert!(fx.root.join("src/newmod.rs").exists());
}

#[test]
fn crlf_file_keeps_crlf_through_full_replacement() {
    let fx = build_fixture();
    // LLM replies come back LF-only; the CRLF original must stay CRLF.
    let reply = "## win.txt\n\n```\nalpha\ngamma\n```\n";
    let built = build_preview(&fx.root, &parse_reply(reply)).expect("preview");
    assert!(built.preview.files[0].ok);
    apply_files(&fx.root, &built.ready).expect("apply");
    let bytes = fs::read(fx.root.join("win.txt")).unwrap();
    let text = String::from_utf8(bytes).unwrap();
    assert_eq!(text, "alpha\r\ngamma\r\n");
}

#[test]
fn safety_rails_traversal_binary_delete() {
    let fx = build_fixture();
    let reply = "\
## ../escape.md\n\n```\nnope\n```\n\n\
## data/img.png\n\n```\nnot an image\n```\n\n\
--- a/src/lib.rs\n\
+++ /dev/null\n\
@@ -1,3 +0,0 @@\n\
-pub fn add(a: i32, b: i32) -> i32 {\n\
-    a + b\n\
-}\n\
";
    let changes = parse_reply(reply);
    assert_eq!(changes.len(), 3);
    let built = build_preview(&fx.root, &changes).expect("preview");
    let by_path = |p: &str| {
        built
            .preview
            .files
            .iter()
            .find(|f| f.rel_path.contains(p))
            .unwrap_or_else(|| panic!("{} missing from preview", p))
    };
    let esc = by_path("escape.md");
    assert!(!esc.ok && esc.note.contains("escapes"), "{:?}", esc.note);
    let img = by_path("img.png");
    assert!(!img.ok && img.note.contains("binary"), "{:?}", img.note);
    let del = by_path("src/lib.rs");
    assert!(!del.ok && del.action == "delete", "{:?}", del.note);
    assert!(
        built.ready.is_empty(),
        "nothing unsafe may become appliable"
    );

    // Nothing was created outside or inside the root.
    assert!(!fx.root.parent().unwrap().join("escape.md").exists());
    assert!(fx.root.join("src/lib.rs").exists());
}

#[test]
fn changed_on_disk_fails_that_file_only() {
    let fx = build_fixture();
    let reply = "## src/main.rs\n\n```rust\nfn main() { println!(\"three\"); }\n```\n\n\
## README.md\n\n```\n# Fixture\n\nnew docs\n```\n";
    let built = build_preview(&fx.root, &parse_reply(reply)).expect("preview");
    assert_eq!(built.ready.len(), 2);

    // Someone edits main.rs between preview and apply.
    fs::write(fx.root.join("src/main.rs"), "fn main() { /* raced */ }\n").unwrap();

    let outcome = apply_files(&fx.root, &built.ready).expect("apply");
    assert_eq!(outcome.applied, vec!["README.md".to_string()]);
    assert_eq!(outcome.failed.len(), 1);
    assert_eq!(outcome.failed[0].rel_path, "src/main.rs");
    assert!(outcome.failed[0].reason.contains("changed on disk"));
    // The raced file was left alone.
    let main = fs::read_to_string(fx.root.join("src/main.rs")).unwrap();
    assert!(main.contains("raced"));
}

#[test]
fn chained_changes_to_one_file_fold_in_order() {
    let fx = build_fixture();
    // Full replacement first, then a diff on top of that replacement.
    let reply = "## README.md\n\n```\nstep one\n```\n\n\
--- a/README.md\n\
+++ b/README.md\n\
@@ -1 +1 @@\n\
-step one\n\
+step two\n\
";
    let built = build_preview(&fx.root, &parse_reply(reply)).expect("preview");
    assert_eq!(built.preview.files.len(), 1, "one file, one preview card");
    assert_eq!(built.ready.len(), 1);
    apply_files(&fx.root, &built.ready).expect("apply");
    assert_eq!(
        fs::read_to_string(fx.root.join("README.md")).unwrap(),
        "step two\n"
    );
}
