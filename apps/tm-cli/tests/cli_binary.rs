//! The real binary, end to end (plan v1 N-30/N-31, repros A.1, A.3, A.14):
//! strict parsing, headless --version, and the exit-code contract.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn tm(args: &[&str], cwd: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_turbomerger"))
        .args(args)
        .current_dir(cwd)
        // Headless: --help/--version must never reach the GUI toolkit.
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env("TURBOMERGER_STATE_DIR", cwd.join("state"))
        .env("TURBOMERGER_CACHE_DIR", cwd.join("cache"))
        .output()
        .expect("binary runs")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn version_and_help_work_without_a_display() {
    let tmp = tempfile::tempdir().unwrap();
    let o = tm(&["--version"], tmp.path());
    assert_eq!(o.status.code(), Some(0));
    assert!(stdout(&o).starts_with("turbomerger "), "{}", stdout(&o));
    let o = tm(&["merge", "--help"], tmp.path());
    assert_eq!(o.status.code(), Some(0));
    assert!(stdout(&o).contains("--fail-on-skip"));
}

#[test]
fn a3_usage_errors_exit_2_and_write_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("fx");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("main.rs"), "fn main() {}\n").unwrap();
    let cwd = tmp.path().join("cwd");
    fs::create_dir_all(&cwd).unwrap();
    for args in [
        vec!["merge", "../fx", "--includ-hidden", "-q"],
        vec!["merge", "../fx", "out.md", "--max-tokens", "abc"],
        vec!["merge", "../fx", "out.md", "--format", "yaml"],
    ] {
        let o = tm(&args, &cwd);
        assert_eq!(o.status.code(), Some(2), "{args:?}");
    }
    let leftovers: Vec<_> = fs::read_dir(&cwd)
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert!(
        leftovers.is_empty(),
        "usage errors must not write: {leftovers:?}"
    );
}

#[test]
fn exit_codes_follow_the_contract() {
    let tmp = tempfile::tempdir().unwrap();
    let code = tmp.path().join("code");
    fs::create_dir_all(&code).unwrap();
    fs::write(code.join("main.rs"), "fn main() {}\n").unwrap();
    fs::write(code.join("logo.png"), [0x89u8, b'P', b'N', b'G', 0, 0]).unwrap();
    let o = tm(&["merge", "code", "code.md", "-q"], tmp.path());
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    // Same folder, but any skipped file counts under --fail-on-skip.
    let o = tm(
        &["merge", "code", "code2.md", "-q", "--fail-on-skip"],
        tmp.path(),
    );
    assert_eq!(o.status.code(), Some(3));

    // A.1: documents without an extractor are NOT captured → 3, and the
    // report says so.
    let docs = tmp.path().join("docs");
    fs::create_dir_all(&docs).unwrap();
    fs::write(docs.join("paper.pdf"), "%PDF-1.7\n").unwrap();
    fs::write(docs.join("notes.md"), "# notes\n").unwrap();
    let o = tm(&["merge", "docs", "docs.md"], tmp.path());
    assert_eq!(o.status.code(), Some(3));
    assert!(stdout(&o).contains("not_captured=1"), "{}", stdout(&o));
    let report = fs::read_to_string(tmp.path().join("docs.md")).unwrap();
    assert!(report.contains("### Not captured (1)"));

    // A.14: nothing mergeable → still a report, exit 3 (v7.7.0: exit 1, nothing).
    let photos = tmp.path().join("photos");
    fs::create_dir_all(&photos).unwrap();
    fs::write(photos.join("IMG_0001.HEIC"), "not really heic").unwrap();
    let o = tm(&["merge", "photos", "photos.md"], tmp.path());
    assert_eq!(o.status.code(), Some(3));
    assert!(tmp.path().join("photos.md").is_file());

    // An empty folder is an error.
    fs::create_dir_all(tmp.path().join("empty")).unwrap();
    let o = tm(&["merge", "empty", "empty.md"], tmp.path());
    assert_eq!(o.status.code(), Some(1));

    // N-23: a bare owner/repo that is not a local folder never clones.
    let o = tm(&["merge", "docs-typo/api", "x.md"], tmp.path());
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&o.stderr).contains("gh:docs-typo/api"));
}

#[test]
fn apply_exits_3_when_proposals_are_held() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    let reply = tmp.path().join("reply.md");
    fs::write(
        &reply,
        "## src/main.rs\n\n```rust\nfn main() { two(); }\n```\n\n## .github/workflows/ci.yml\n\n```yaml\non: [push]\n```\n",
    )
    .unwrap();
    let o = tm(
        &["apply", "repo", "--from", "reply.md", "--yes"],
        tmp.path(),
    );
    assert_eq!(o.status.code(), Some(3), "{}", stdout(&o));
    assert!(stdout(&o).contains("HOLD   .github/workflows/ci.yml"));
    assert!(!root.join(".github").exists());
    // Allowed explicitly: applies, exit 0.
    let o = tm(
        &[
            "apply",
            "repo",
            "--from",
            "reply.md",
            "--yes",
            "--allow-control",
            ".github/workflows/*",
        ],
        tmp.path(),
    );
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    assert!(root.join(".github/workflows/ci.yml").is_file());
    // Restore uses the state dir the apply recorded into.
    let o = tm(&["apply", "repo", "--restore"], tmp.path());
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    assert!(!root.join(".github/workflows/ci.yml").exists());
}

#[cfg(unix)]
#[test]
fn explain_names_the_rule() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("r");
    fs::create_dir_all(root.join("node_modules/x")).unwrap();
    fs::write(root.join("node_modules/x/i.js"), "1\n").unwrap();
    fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
    std::os::unix::fs::symlink("main.rs", root.join("link.rs")).unwrap();
    let o = tm(&["explain", "r", "node_modules/x/i.js"], tmp.path());
    assert!(
        stdout(&o).contains("inside node_modules/"),
        "{}",
        stdout(&o)
    );
    let o = tm(&["explain", "r", "link.rs"], tmp.path());
    assert!(stdout(&o).contains("symlink to main.rs"), "{}", stdout(&o));
    let o = tm(&["explain", "r", "main.rs"], tmp.path());
    assert!(stdout(&o).contains("merged"));
}

#[test]
fn reproducible_outputs_do_not_depend_on_line_ends_or_name_normalization() {
    // N-16: the same project checked out on Windows (CRLF) and macOS (NFD
    // names) must merge to the same bytes with --reproducible.
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a").join("proj");
    let b = tmp.path().join("b").join("proj");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(a.join("main.rs"), "fn main() {\r\n    x();\r\n}\r\n").unwrap();
    fs::write(b.join("main.rs"), "fn main() {\n    x();\n}\n").unwrap();
    fs::write(a.join("cafe\u{301}.md"), "# notes\r\n").unwrap(); // NFD
    fs::write(b.join("caf\u{e9}.md"), "# notes\n").unwrap(); // NFC
    fs::write(a.join("zeta.txt"), "z\n").unwrap();
    fs::write(b.join("zeta.txt"), "z\n").unwrap();
    for (src, out) in [(&a, "a.md"), (&b, "b.md")] {
        let o = tm(
            &["merge", &src.to_string_lossy(), out, "--reproducible", "-q"],
            tmp.path(),
        );
        assert_eq!(
            o.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&o.stderr)
        );
    }
    let (ra, rb) = (
        fs::read(tmp.path().join("a.md")).unwrap(),
        fs::read(tmp.path().join("b.md")).unwrap(),
    );
    assert!(!ra.contains(&b'\r'), "LF only");
    assert_eq!(
        String::from_utf8_lossy(&ra),
        String::from_utf8_lossy(&rb),
        "same bytes"
    );
    // Without the flag the CRLF copy keeps its line ends.
    let o = tm(&["merge", &a.to_string_lossy(), "raw.md", "-q"], tmp.path());
    assert_eq!(o.status.code(), Some(0));
    assert!(fs::read(tmp.path().join("raw.md"))
        .unwrap()
        .contains(&b'\r'));
}

#[test]
fn the_token_cache_changes_nothing_but_speed() {
    // Plan 2.8: a second run reuses cached counts; the bytes are identical.
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir_all(&src).unwrap();
    for i in 0..30 {
        fs::write(
            src.join(format!("f{i}.rs")),
            format!(
                "// file {i}\n{}",
                "fn body() { let x = 1 + 2; }\n".repeat(20)
            ),
        )
        .unwrap();
    }
    let o = tm(&["merge", "src", "one.md", "-q"], tmp.path());
    assert_eq!(o.status.code(), Some(0));
    let cache = tmp.path().join("cache").join("token-counts-v1.bin");
    assert!(cache.is_file(), "the first run saves the cache");
    let o = tm(&["merge", "src", "two.md", "-q"], tmp.path());
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(
        fs::read(tmp.path().join("one.md")).unwrap(),
        fs::read(tmp.path().join("two.md")).unwrap()
    );
    // A damaged cache is ignored, never trusted.
    fs::write(&cache, b"garbage").unwrap();
    let o = tm(&["merge", "src", "three.md", "-q"], tmp.path());
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(
        fs::read(tmp.path().join("one.md")).unwrap(),
        fs::read(tmp.path().join("three.md")).unwrap()
    );
}
