//! End-to-end fixture-tree tests: scan + merge a synthetic repo and assert on
//! the actual output. These are exactly the regression class of the v7.1 bugs:
//! gitignored browser profiles leaking, dotfiles vanishing, secrets passing
//! through verbatim, and embedded markdown breaking the fence structure.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use turbomerger::merger::merge_files_with_progress;
use turbomerger::scanner::{scan_text_files, ScanOptions};

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
        true,
        true,
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
    assert!(outcome.token_estimate > 0);
}

#[test]
fn bom_stripped_and_lossy_decode_noted() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("enc_repo");
    fs::create_dir_all(&root).unwrap();
    // UTF-8 BOM file
    let mut bom = vec![0xEF, 0xBB, 0xBF];
    bom.extend_from_slice(b"hello bom\n");
    fs::write(root.join("bom.txt"), &bom).unwrap();
    // windows-1252-ish byte (0xE9 = é) — invalid as UTF-8
    fs::write(root.join("latin.txt"), b"caf\xE9 latte\n").unwrap();

    let scan = scan_text_files(&root, &ScanOptions::default()).unwrap();
    let out = tmp.path().join("enc_out.md");
    let cancel = AtomicBool::new(false);
    merge_files_with_progress(
        &root,
        &scan.files,
        &out,
        false,
        true,
        &cancel,
        |_, _, _| {},
        &[],
    )
    .unwrap();

    let text = fs::read_to_string(&out).unwrap();
    assert!(!text.contains('\u{FEFF}'), "BOM must be stripped");
    assert!(text.contains("hello bom"));
    assert!(
        text.contains("Decoding notes"),
        "lossy decode must be reported"
    );
    assert!(text.contains("latin.txt"));
}
