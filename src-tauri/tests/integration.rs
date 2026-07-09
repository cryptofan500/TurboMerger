//! End-to-end fixture-tree tests: scan + merge a synthetic repo and assert on
//! the actual output. These are exactly the regression class of the v7.1 bugs:
//! gitignored browser profiles leaking, dotfiles vanishing, secrets passing
//! through verbatim, and embedded markdown breaking the fence structure.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use turbomerger::merger::{merge_files_with_progress, MergeConfig, Ordering, OutputFormat};
use turbomerger::scanner::{scan_text_files, ScanOptions};

fn md_cfg() -> MergeConfig {
    MergeConfig::default()
}

struct Fixture {
    root: PathBuf,
    _tmp: tempfile::TempDir,
}

fn build_fixture() -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("fixture_repo");

    let write = |rel: &str, content: &[u8]| {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    };

    // normal source
    write("src/main.rs", b"fn main() { println!(\"hi\"); }\n");
    write(
        "README.md",
        b"# Fixture\n\nSome docs with a code block:\n\n```rust\nlet x = 1;\n```\n",
    );
    // file the old substring filter would have silently dropped
    write(
        "src/password_reset.py",
        b"def reset(user):\n    return send_email(user)\n",
    );
    // dot-config files that v7.1 dropped entirely
    write(".gitignore", b"profiles/\n");
    write(".mcp.json", b"{ \"mcpServers\": {} }\n");
    // gitignored browser-profile dir (the cookie leak)
    write(
        "profiles/camoufox/cookies.txt",
        b"cf_clearance=SECRETCOOKIEVALUE\n",
    );
    write(
        "profiles/camoufox/cookies.sqlite-wal",
        b"mostly ascii wal content cf_clearance more ascii\n",
    );
    // secrets that must be redacted
    write(
        "src/config.py",
        b"GITHUB_TOKEN = \"ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"\nAWS = \"AKIAIOSFODNN7QQQQQQQ\"\n",
    );
    // sensitive file that must be skipped (with a reason)
    write(".env", b"OPENAI_API_KEY=sk-proj-abcdef\n");
    // safe template that must be INCLUDED
    write(".env.example", b"OPENAI_API_KEY=your_key_here\n");
    // binary by magic bytes
    write(
        "assets/img.dat",
        &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0],
    );
    // minified
    write("web/vendor.min.js", b"var a=1;\n");
    // lockfile noise
    write("package-lock.json", b"{}\n");
    // a previous TurboMerger dump (self-exclusion)
    write("old_2026-01-01_merged.md", b"# old dump\n");

    Fixture { root, _tmp: tmp }
}

fn rel_names(root: &Path, files: &[PathBuf]) -> Vec<String> {
    files
        .iter()
        .map(|f| {
            f.strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect()
}

#[test]
fn scan_respects_gitignore_dotconfig_and_sensitivity() {
    let fx = build_fixture();
    let result = scan_text_files(&fx.root, &ScanOptions::default()).expect("scan");
    let names = rel_names(&fx.root, &result.files);

    // included
    for expected in [
        "src/main.rs",
        "README.md",
        "src/password_reset.py",
        ".gitignore",
        ".mcp.json",
        ".env.example",
        "src/config.py",
    ] {
        assert!(
            names.contains(&expected.to_string()),
            "{} missing from {:?}",
            expected,
            names
        );
    }

    // excluded
    assert!(
        !names.iter().any(|n| n.starts_with("profiles/")),
        "gitignored profiles/ leaked: {:?}",
        names
    );
    assert!(
        !names.contains(&".env".to_string()),
        ".env must be excluded"
    );
    assert!(
        !names.contains(&"web/vendor.min.js".to_string()),
        "minified must be excluded"
    );
    assert!(
        !names.contains(&"package-lock.json".to_string()),
        "lockfile must be excluded"
    );
    assert!(
        !names.contains(&"old_2026-01-01_merged.md".to_string()),
        "previous dump must be excluded"
    );
    assert!(
        !names.contains(&"assets/img.dat".to_string()),
        "binary must be excluded"
    );

    // every exclusion has a recorded reason
    let skipped_paths: Vec<&str> = result.skipped.iter().map(|s| s.path.as_str()).collect();
    assert!(
        skipped_paths.contains(&".env"),
        "skip manifest missing .env: {:?}",
        skipped_paths
    );
    assert!(
        skipped_paths.contains(&"old_2026-01-01_merged.md"),
        "skip manifest missing self-output"
    );
}

#[test]
fn merge_redacts_secrets_and_keeps_fences_safe() {
    let fx = build_fixture();
    let scan = scan_text_files(&fx.root, &ScanOptions::default()).expect("scan");
    let out = fx.root.parent().unwrap().join("out.md");
    let cancel = AtomicBool::new(false);

    let outcome = merge_files_with_progress(
        &fx.root,
        &scan.files,
        &out,
        &md_cfg(),
        &cancel,
        |_, _, _| {},
        &scan.skipped,
    )
    .expect("merge");

    let text = fs::read_to_string(&out).unwrap();

    // secrets are redacted
    assert!(
        !text.contains("ghp_AAAA"),
        "GitHub token leaked into output"
    );
    assert!(
        !text.contains("AKIAIOSFODNN7QQQQQQQ"),
        "AWS key leaked into output"
    );
    assert!(text.contains("[REDACTED]"));
    assert!(
        outcome.secrets_redacted >= 2,
        "redaction count: {}",
        outcome.secrets_redacted
    );

    // no cookie material anywhere
    assert!(!text.contains("cf_clearance"), "cookie value leaked");

    // fence safety: README.md contains a ``` block, so its wrapper fence must be longer
    let readme_section = text
        .split("## README.md")
        .nth(1)
        .expect("README section present");
    assert!(
        readme_section.trim_start().starts_with("````"),
        "README fence must be 4+ backticks, got: {}",
        &readme_section.trim_start()[..12.min(readme_section.trim_start().len())]
    );

    // merge report exists and lists the .env skip with a reason
    assert!(text.contains("## Merge Report"));
    assert!(text.contains("`.env`"), "merge report should list .env");
    assert!(text.contains("env file"), "reason text missing");

    // TOC + tree present
    assert!(text.contains("## Project Structure"));
    assert!(text.contains("## Contents"));
    assert!(text.contains("- [src/main.rs](#srcmainrs)"));

    // header is honest
    assert!(text.contains(&format!("Files scanned: {}", scan.files.len())));
    assert!(outcome.files_processed > 0);
    assert!(outcome.tokens_o200k > 0);
}

#[test]
fn bom_stripped_and_legacy_encodings_decoded() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("enc_repo");
    fs::create_dir_all(&root).unwrap();
    // UTF-8 BOM file
    let mut bom = vec![0xEF, 0xBB, 0xBF];
    bom.extend_from_slice(b"hello bom\n");
    fs::write(root.join("bom.txt"), &bom).unwrap();
    // windows-1252 byte (0xE9 = é) — invalid as UTF-8, must be DETECTED not mangled
    fs::write(root.join("latin.txt"), b"caf\xE9 latte\n").unwrap();
    // UTF-16LE with BOM — null bytes must not trip the binary check
    let mut utf16 = vec![0xFF, 0xFE];
    for u in "hi utf sixteen\n".encode_utf16() {
        utf16.extend_from_slice(&u.to_le_bytes());
    }
    fs::write(root.join("wide.txt"), &utf16).unwrap();

    let scan = scan_text_files(&root, &ScanOptions::default()).unwrap();
    let out = tmp.path().join("enc_out.md");
    let cancel = AtomicBool::new(false);
    let cfg = MergeConfig {
        include_tree: false,
        ..MergeConfig::default()
    };
    merge_files_with_progress(&root, &scan.files, &out, &cfg, &cancel, |_, _, _| {}, &[]).unwrap();

    let text = fs::read_to_string(&out).unwrap();
    assert!(!text.contains('\u{FEFF}'), "BOM must be stripped");
    assert!(text.contains("hello bom"));
    assert!(
        text.contains("café latte"),
        "windows-1252 é must survive via chardetng detection"
    );
    assert!(
        text.contains("hi utf sixteen"),
        "UTF-16LE BOM file must decode"
    );
    assert!(
        text.contains("Decoding notes"),
        "non-UTF-8 decodes must be reported"
    );
    assert!(text.contains("latin.txt"));
    assert!(text.contains("UTF-16LE"), "UTF-16 note must name the encoding");
}

#[test]
fn formats_and_split_produce_valid_output() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("fmt_repo");
    fs::create_dir_all(root.join("src")).unwrap();
    for i in 0..6 {
        fs::write(
            root.join("src").join(format!("f{}.rs", i)),
            format!("fn f{}() {{ /* {} */ }}\n", i, "x".repeat(400)),
        )
        .unwrap();
    }
    let scan = scan_text_files(&root, &ScanOptions::default()).unwrap();
    let cancel = AtomicBool::new(false);

    // XML output is well-formed-ish and escapes content
    let xml_out = tmp.path().join("o.xml");
    let xcfg = MergeConfig {
        format: OutputFormat::Xml,
        ..MergeConfig::default()
    };
    merge_files_with_progress(
        &root,
        &scan.files,
        &xml_out,
        &xcfg,
        &cancel,
        |_, _, _| {},
        &[],
    )
    .unwrap();
    let xml = fs::read_to_string(&xml_out).unwrap();
    assert!(xml.starts_with("<codebase"));
    assert!(xml.contains("<file path=\"src/f0.rs\""));
    assert!(xml.trim_end().ends_with("</codebase>"));

    // JSON output parses
    let json_out = tmp.path().join("o.json");
    let jcfg = MergeConfig {
        format: OutputFormat::Json,
        ..MergeConfig::default()
    };
    merge_files_with_progress(
        &root,
        &scan.files,
        &json_out,
        &jcfg,
        &cancel,
        |_, _, _| {},
        &[],
    )
    .unwrap();
    let val: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&json_out).unwrap()).unwrap();
    assert_eq!(val["files"].as_array().unwrap().len(), 6);
    assert!(val["tokens_o200k"].as_u64().unwrap() > 0);

    // Split by a tiny token budget yields multiple parts
    let split_out = tmp.path().join("split.md");
    let scfg = MergeConfig {
        max_tokens: Some(50),
        ..MergeConfig::default()
    };
    let outcome = merge_files_with_progress(
        &root,
        &scan.files,
        &split_out,
        &scfg,
        &cancel,
        |_, _, _| {},
        &[],
    )
    .unwrap();
    assert!(outcome.outputs.len() > 1, "expected split into parts");
    assert!(outcome.outputs.iter().all(|p| p.exists()));
    let p1 = fs::read_to_string(&outcome.outputs[0]).unwrap();
    assert!(p1.contains("Part 1/"));

    // Ordering: important-last puts a README at the very end
    fs::write(root.join("README.md"), "# readme\n").unwrap();
    let scan2 = scan_text_files(&root, &ScanOptions::default()).unwrap();
    let ord_out = tmp.path().join("ord.md");
    let ocfg = MergeConfig {
        ordering: Ordering::ImportantLast,
        include_tree: false,
        ..MergeConfig::default()
    };
    merge_files_with_progress(
        &root,
        &scan2.files,
        &ord_out,
        &ocfg,
        &cancel,
        |_, _, _| {},
        &[],
    )
    .unwrap();
    let ord = fs::read_to_string(&ord_out).unwrap();
    let readme_pos = ord.find("## README.md").unwrap();
    let first_src = ord.find("## src/f0.rs").unwrap();
    assert!(
        readme_pos > first_src,
        "important-last should put README after src files"
    );
}

#[test]
fn compress_elides_bodies_and_strip_removes_comments() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cmp_repo");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "// top comment\npub fn add(a: i32, b: i32) -> i32 {\n    a + b // inline math\n}\n\npub struct Keep {\n    pub field: u32,\n}\n",
    )
    .unwrap();
    fs::write(
        root.join("src/util.py"),
        "# helper\ndef double(x):\n    return x * 2\n",
    )
    .unwrap();
    // Unsupported language must pass through untouched.
    fs::write(root.join("notes.md"), "# Notes\n\nplain prose\n").unwrap();

    let scan = scan_text_files(&root, &ScanOptions::default()).unwrap();
    let out = tmp.path().join("cmp.md");
    let cancel = AtomicBool::new(false);
    let cfg = MergeConfig {
        compress: true,
        strip_comments: true,
        include_tree: false,
        ..MergeConfig::default()
    };
    let outcome =
        merge_files_with_progress(&root, &scan.files, &out, &cfg, &cancel, |_, _, _| {}, &[])
            .unwrap();

    let text = fs::read_to_string(&out).unwrap();
    assert!(
        text.contains("pub fn add(a: i32, b: i32) -> i32 { ... }"),
        "rust body must be elided: {}",
        text
    );
    assert!(!text.contains("a + b"), "rust body leaked");
    assert!(text.contains("pub struct Keep"), "struct must survive");
    assert!(text.contains("def double(x):\n    ..."), "python body must be elided");
    assert!(!text.contains("x * 2"), "python body leaked");
    assert!(!text.contains("top comment"), "rust comment must be stripped");
    assert!(!text.contains("# helper"), "python comment must be stripped");
    assert!(text.contains("plain prose"), "unsupported md must pass through");
    assert_eq!(outcome.files_compressed, 2, "both code files compressed");
    assert!(
        text.contains("Compressed to signatures"),
        "merge report must note compression"
    );
}
