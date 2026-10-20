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
        vec!["merge", "../fx", "out.md", "--threads", "abc"],
        vec!["merge", "../fx", "out.md", "--threads", "5000"],
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

    // A.1: a document whose text cannot be extracted (this stub PDF has no
    // valid structure) is NOT captured → 3, and the report says so.
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

#[test]
fn the_worker_count_changes_nothing_but_speed() {
    // P4: --threads and TURBOMERGER_THREADS size the pool; the bytes are the same.
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir_all(&src).unwrap();
    for i in 0..40 {
        fs::write(
            src.join(format!("f{i:02}.rs")),
            format!("// file {i}\nfn f{i}() {{}}\n"),
        )
        .unwrap();
    }
    for (out, threads) in [("one.md", "1"), ("three.md", "3"), ("auto.md", "0")] {
        let o = tm(
            &[
                "merge",
                "src",
                out,
                "--reproducible",
                "-q",
                "--threads",
                threads,
            ],
            tmp.path(),
        );
        assert_eq!(o.status.code(), Some(0), "--threads {threads}");
    }
    let with_env = |value: &str, out: &str| {
        Command::new(env!("CARGO_BIN_EXE_turbomerger"))
            .args(["merge", "src", out, "--reproducible", "-q"])
            .current_dir(tmp.path())
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env("TURBOMERGER_STATE_DIR", tmp.path().join("state"))
            .env("TURBOMERGER_CACHE_DIR", tmp.path().join("cache"))
            .env("TURBOMERGER_THREADS", value)
            .output()
            .expect("binary runs")
    };
    assert_eq!(with_env("2", "env.md").status.code(), Some(0));
    let one = fs::read(tmp.path().join("one.md")).unwrap();
    for other in ["three.md", "auto.md", "env.md"] {
        assert_eq!(one, fs::read(tmp.path().join(other)).unwrap(), "{other}");
    }
    // An unreadable value is a usage error, like any other flag's.
    assert_eq!(with_env("many", "bad.md").status.code(), Some(2));
    assert!(!tmp.path().join("bad.md").exists());
}

fn fixture_pdf(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/arxiv-mini/local/2026-08")
        .join(name)
}

#[test]
fn the_pdf_worker_speaks_the_protocol_and_matches_in_process_text() {
    // P4: big PDFs parse in `turbomerger __worker pdf`.
    let exe = Path::new(env!("CARGO_BIN_EXE_turbomerger"));
    let mut w =
        tm_worker::Worker::spawn(exe, &["__worker", "pdf"], tm_worker::Limits::default()).unwrap();
    assert_eq!(w.hello.worker, "tm-pdf");
    let pdf = fixture_pdf("2608.14352.pdf");
    let cancel = tm_core::CancelToken::new();
    let mut out = w
        .run(
            "pdf_pages",
            serde_json::json!({ "path": pdf.to_str().unwrap() }),
            &cancel,
            &mut |_| {},
        )
        .unwrap();
    let pages: Vec<String> = serde_json::from_value(out["pages"].take()).unwrap();
    assert_eq!(pages, tm_core::extract::pdf_pages(&pdf).unwrap());
    assert!(pages.iter().any(|p| !p.trim().is_empty()));
    // A parse error is the job's error; the worker lives on.
    let err = w
        .run(
            "pdf_pages",
            serde_json::json!({ "path": "/nonexistent/x.pdf" }),
            &cancel,
            &mut |_| {},
        )
        .unwrap_err();
    assert!(
        matches!(&err, tm_worker::WorkerError::Failed(m) if m.starts_with("PDF parse failed")),
        "{err:?}"
    );
    w.shutdown();
}

#[test]
fn pdfs_parsed_in_workers_merge_to_the_same_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("docs");
    fs::create_dir_all(&src).unwrap();
    for name in ["2608.14352.pdf", "2608.15188.pdf"] {
        fs::copy(fixture_pdf(name), src.join(name)).unwrap();
    }
    fs::write(src.join("notes.md"), "# notes\n").unwrap();
    let merge = |out: &str, env: &[(&str, &str)]| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_turbomerger"));
        c.args([
            "merge",
            "docs",
            out,
            "--format",
            "json",
            "--reproducible",
            "-q",
        ])
        .current_dir(tmp.path())
        .env("TURBOMERGER_STATE_DIR", tmp.path().join("state"))
        .env("TURBOMERGER_CACHE_DIR", tmp.path().join("cache"));
        for (k, v) in env {
            c.env(k, v);
        }
        c.output().expect("binary runs")
    };
    assert_eq!(merge("here.json", &[]).status.code(), Some(0));
    // Every PDF through a worker (the threshold is 256 MB by default).
    let o = merge("worker.json", &[("TURBOMERGER_PDF_WORKER_ABOVE_MB", "0")]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert_eq!(
        fs::read(tmp.path().join("here.json")).unwrap(),
        fs::read(tmp.path().join("worker.json")).unwrap()
    );
    // A worker stopped by its memory budget (here 1 MB) loses only its file.
    let o = merge(
        "capped.json",
        &[
            ("TURBOMERGER_PDF_WORKER_ABOVE_MB", "0"),
            ("TURBOMERGER_PDF_WORKER_MB", "1"),
        ],
    );
    assert_eq!(
        o.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let d: serde_json::Value =
        serde_json::from_slice(&fs::read(tmp.path().join("capped.json")).unwrap()).unwrap();
    let merged: Vec<&str> = d["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(merged, ["notes.md"]);
    let reasons: Vec<&str> = d["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["reason"].as_str().unwrap())
        .collect();
    assert_eq!(reasons.len(), 2, "{reasons:?}");
    assert!(
        reasons
            .iter()
            .all(|r| r.contains("isolated worker") && r.contains("memory budget 1 MB")),
        "{reasons:?}"
    );
}

#[test]
fn metatrader_and_editor_formats_merge_by_extension_audio_video_never() {
    // P8: "anything Notepad++ opens, not video/audio". An 800 KB EA was dropped
    // as "large file with unknown extension" whenever a size cap was set.
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("mt5");
    fs::create_dir_all(&src).unwrap();
    let body: String = (0..20_000)
        .map(|i| format!("input double Lot{i}=0.1;\n"))
        .collect();
    fs::write(
        src.join("EA.mq5"),
        format!("// EA_START\n{body}// EA_END\n"),
    )
    .unwrap();
    let utf16 = |s: &str, bom: bool| -> Vec<u8> {
        let mut v = if bom { vec![0xFF, 0xFE] } else { Vec::new() };
        v.extend(s.encode_utf16().flat_map(u16::to_le_bytes));
        v
    };
    fs::write(
        src.join("params.set"),
        utf16("Lots=0.10\r\nSET_BOM=1\r\n", true),
    )
    .unwrap();
    fs::write(
        src.join("chart.chr"),
        utf16("<chart>\r\nCHR_NO_BOM\r\n</chart>\r\n", false),
    )
    .unwrap();
    fs::write(src.join("app.ts"), "export const TS_KEPT = 1;\n").unwrap();
    for (name, head) in [
        ("song.mid", &b"MThd\0\0\0\x06"[..]),
        ("clip.3gp", &b"\0\0\0\x18ftyp3gp4"[..]),
        ("video.m2ts", &b"\x47\x40\0\x10"[..]),
    ] {
        fs::write(src.join(name), head).unwrap();
    }
    let o = tm(
        &[
            "merge",
            "mt5",
            "mt5.json",
            "--format",
            "json",
            "-q",
            "--max-file-size",
            "2",
        ],
        tmp.path(),
    );
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let d: serde_json::Value =
        serde_json::from_slice(&fs::read(tmp.path().join("mt5.json")).unwrap()).unwrap();
    let text: String = d["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["content"].as_str().unwrap())
        .collect();
    for token in ["EA_START", "EA_END", "SET_BOM", "CHR_NO_BOM", "TS_KEPT"] {
        assert!(text.contains(token), "{token} missing");
    }
    for name in ["song.mid", "clip.3gp", "video.m2ts"] {
        let s = d["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["path"] == name)
            .unwrap_or_else(|| panic!("{name} not recorded"));
        assert_eq!(s["reason"], "binary extension", "{name}");
    }
}
