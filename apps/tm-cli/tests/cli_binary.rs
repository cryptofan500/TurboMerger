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

#[test]
fn complete_plans_keep_every_named_file_and_name_what_cannot_come_back() {
    // HANDOFF_S8 §6.10–6.11 (S9): the archive twin holds every file a plan
    // names (a picture the scanner skips included), never embeds bytes under
    // --redact, and exits 3 naming each file it cannot restore.
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("a.txt"), "plain text\n").unwrap();
    let png: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR not a real picture";
    fs::write(src.join("pic.png"), png).unwrap();
    fs::write(src.join("notes.md"), "password: Vr7wKp2walKotwica91\n").unwrap();
    let plan = tmp.path().join("plan.toml");
    fs::write(
        &plan,
        "version = 1\nmode = \"explicit\"\ncomplete = true\n[[file]]\npath = \"a.txt\"\n[[file]]\npath = \"pic.png\"\n[[file]]\npath = \"notes.md\"\n",
    )
    .unwrap();
    let stderr = |o: &Output| String::from_utf8_lossy(&o.stderr).into_owned();
    let run = |name: &str, extra: &[&str]| {
        let out = tmp.path().join(name);
        let mut args = vec![
            "merge",
            src.to_str().unwrap(),
            out.to_str().unwrap(),
            "--selection",
            plan.to_str().unwrap(),
            "--yes",
            "-q",
        ];
        args.extend_from_slice(extra);
        let o = tm(&args, tmp.path());
        (o, fs::read_to_string(&out).unwrap_or_default())
    };

    let (o, text) = run("whole.md", &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(text.contains("## __binary__: pic.png"), "{text}");
    let pic = text
        .lines()
        .find(|l| l.starts_with('{') && l.contains("\"path\":\"pic.png\""))
        .unwrap_or_else(|| panic!("pic.png not in the manifest:\n{text}"));
    assert!(
        pic.contains("\"kind\":\"binary\"") && pic.contains(&format!("\"bytes\":{}", png.len())),
        "{pic}"
    );

    let (o, text) = run("redacted.md", &["--redact"]);
    assert_eq!(o.status.code(), Some(3), "{}", stderr(&o));
    for name in ["pic.png", "notes.md"] {
        assert!(
            stderr(&o).contains(&format!("not restorable: {name}")),
            "{}",
            stderr(&o)
        );
    }
    assert!(
        !text.contains("## __binary__:"),
        "bytes embedded under --redact:\n{text}"
    );
    assert!(!text.contains("Vr7wKp2walKotwica91"));

    let (o, _) = run("capped.md", &["--embed-binary", "0"]);
    assert_eq!(o.status.code(), Some(3), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("not restorable: pic.png"),
        "{}",
        stderr(&o)
    );

    let (o, _) = run("twin.json", &["--format", "json"]);
    assert_eq!(
        o.status.code(),
        Some(2),
        "the twin is Markdown only: {}",
        stderr(&o)
    );
}

#[test]
fn restore_rebuilds_a_twin_and_never_overwrites_silently() {
    // HANDOFF_S8 §6.10 item 3.5 (S9): the CLI's own restore.
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir_all(src.join("d")).unwrap();
    fs::write(src.join("a.txt"), "one\r\ntwo\r\n").unwrap();
    fs::write(src.join("d/b.bin.txt"), b"caf\xe9").unwrap();
    let twin = tmp.path().join("twin.md");
    let o = tm(
        &[
            "merge",
            src.to_str().unwrap(),
            twin.to_str().unwrap(),
            "--complete",
            "--yes",
            "-q",
        ],
        tmp.path(),
    );
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let dest = tmp.path().join("back");
    let run = |extra: &[&str]| {
        let mut args = vec!["restore", twin.to_str().unwrap()];
        args.extend_from_slice(extra);
        tm(&args, tmp.path())
    };
    let o = run(&["--dest", dest.to_str().unwrap()]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(
        stdout(&o).starts_with("restored=2 identical=0"),
        "{}",
        stdout(&o)
    );
    assert_eq!(fs::read(dest.join("a.txt")).unwrap(), b"one\r\ntwo\r\n");
    assert_eq!(fs::read(dest.join("d/b.bin.txt")).unwrap(), b"caf\xe9");
    let o = run(&["--dest", dest.to_str().unwrap()]);
    assert!(
        stdout(&o).starts_with("restored=0 identical=2"),
        "{}",
        stdout(&o)
    );
    fs::write(dest.join("a.txt"), "changed\n").unwrap();
    let o = run(&["--dest", dest.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&o.stderr).contains("nothing was written"));
    assert_eq!(fs::read(dest.join("a.txt")).unwrap(), b"changed\n");
    let o = run(&["--dest", dest.to_str().unwrap(), "--overwrite"]);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(fs::read(dest.join("a.txt")).unwrap(), b"one\r\ntwo\r\n");
    let o = run(&["--check"]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(stdout(&o).contains("(checked, nothing written)"));
    let o = tm(&["restore", twin.to_str().unwrap()], tmp.path());
    assert_eq!(o.status.code(), Some(2), "--dest or --check is required");
}

/// Lines in a checkpoint journal (header included), 0 if there is none yet.
fn journal_lines(dir: &Path) -> usize {
    fs::read(dir.join("journal.jsonl"))
        .map(|b| b.iter().filter(|&&c| c == b'\n').count())
        .unwrap_or(0)
}

#[test]
fn a_killed_run_resumes_to_the_same_bytes() {
    // S10 4.3 (HANDOFF_S8, DeepSeek B13): kill -9 part way through a
    // checkpointed merge, change one finished file, --resume: the output is
    // byte for byte what an uninterrupted run of the changed tree writes,
    // the finished files are not read again, and the checkpoint is removed.
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir_all(src.join("deep")).unwrap();
    for i in 0..120 {
        fs::write(
            src.join(format!("f{i:03}.rs")),
            format!("pub fn f{i}() -> u32 {{ {i} }}\n"),
        )
        .unwrap();
    }
    fs::write(src.join("deep/notes.md"), "# notes\n\ntext\n").unwrap();
    fs::write(src.join("blob.bin"), [0u8, 1, 2, 3, 0, 0, 0, 9]).unwrap();
    let base = ["--reproducible", "--threads", "2"];
    let args = |out: &str, extra: &[&str]| -> Vec<String> {
        ["merge", "src", out]
            .iter()
            .chain(base.iter())
            .chain(extra.iter())
            .map(|s| s.to_string())
            .collect()
    };
    let run = |a: Vec<String>| {
        let a: Vec<&str> = a.iter().map(String::as_str).collect();
        tm(&a, tmp.path())
    };

    // The run that is killed: slow files, a journal line after every file.
    let ck = tmp.path().join("part.md.tm-checkpoint");
    let mut child = Command::new(env!("CARGO_BIN_EXE_turbomerger"))
        .args(args("part.md", &["--checkpoint", "-q"]))
        .current_dir(tmp.path())
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env("TURBOMERGER_STATE_DIR", tmp.path().join("state"))
        .env("TURBOMERGER_CACHE_DIR", tmp.path().join("cache"))
        .env("TM_TEST_SLOW_FILE_MS", "40")
        .env("TM_TEST_CHECKPOINT_FLUSH_MS", "0")
        .spawn()
        .unwrap();
    let start = std::time::Instant::now();
    while journal_lines(&ck) < 41 {
        assert!(start.elapsed().as_secs() < 60, "the journal never grew");
        assert!(
            child.try_wait().unwrap().is_none(),
            "finished before the kill"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    child.kill().unwrap(); // SIGKILL: no clean-up of any kind
    child.wait().unwrap();
    let journaled = journal_lines(&ck) - 1;
    assert!(
        (40..122).contains(&journaled),
        "{journaled} files journaled"
    );
    assert!(
        !tmp.path().join("part.md").exists(),
        "a killed run writes no output"
    );
    assert!(ck.join("spool").is_file() && ck.join("CACHEDIR.TAG").is_file());

    // f000.rs was finished before the kill; it changes (size and time).
    fs::write(src.join("f000.rs"), "pub fn f0() -> u32 { 1000 }\n").unwrap();
    let o = run(args("full.md", &["-q"]));
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));

    let o = run(args("part.md", &["--resume"]));
    let said = format!("{}{}", stdout(&o), String::from_utf8_lossy(&o.stderr));
    assert_eq!(o.status.code(), Some(0), "{said}");
    let resumed: usize = said
        .split("resumed: ")
        .nth(1)
        .and_then(|t| t.split(' ').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no resumed note: {said}"));
    assert_eq!(
        resumed,
        journaled - 1,
        "every journaled file but the changed one"
    );
    assert_eq!(
        fs::read(tmp.path().join("part.md")).unwrap(),
        fs::read(tmp.path().join("full.md")).unwrap(),
        "the resumed output differs from the uninterrupted one"
    );
    assert!(
        !ck.exists(),
        "the checkpoint is removed once the merge is written"
    );
}

#[test]
fn resume_refuses_what_it_cannot_continue() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("a.rs"), "fn a() {}\n").unwrap();
    // Usage errors (exit 2), nothing written.
    for args in [
        vec!["merge", "src", "o.md", "--resume", "--redact"],
        vec!["merge", "src", "--resume"],
        vec!["merge", "src", "o.md", "--resume", "--checkpoint"],
    ] {
        let o = tm(&args, tmp.path());
        assert_eq!(o.status.code(), Some(2), "{args:?}");
    }
    assert!(!tmp.path().join("o.md.tm-checkpoint").exists());
    // A checkpoint made with other content options is not mixed in.
    let o = tm(&["merge", "src", "o.md", "--checkpoint", "-q"], tmp.path());
    assert_eq!(o.status.code(), Some(0));
    assert!(
        !tmp.path().join("o.md.tm-checkpoint").exists(),
        "removed after success"
    );
    fs::create_dir_all(tmp.path().join("o.md.tm-checkpoint")).unwrap();
    fs::write(
        tmp.path().join("o.md.tm-checkpoint/journal.jsonl"),
        "{\"tm_checkpoint\":1,\"fingerprint\":\"0000\"}\n",
    )
    .unwrap();
    let o = tm(&["merge", "src", "o.md", "--resume", "-q"], tmp.path());
    assert_eq!(o.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("other merge options"),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
}
