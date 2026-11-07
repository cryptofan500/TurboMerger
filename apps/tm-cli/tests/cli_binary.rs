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
fn version_names_the_commit_it_was_built_from() {
    // E2 (2026-11-07): the Dev build printed a bare 7.7.0, like the retired
    // 2026-08 package. Built from git, the stamp names HEAD and its date.
    let tmp = tempfile::tempdir().unwrap();
    let head = Command::new("git")
        .args([
            "-C",
            env!("CARGO_MANIFEST_DIR"),
            "rev-parse",
            "--short=7",
            "HEAD",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let o = tm(&["--version"], tmp.path());
    let v = stdout(&o);
    let want = format!("turbomerger {}", env!("CARGO_PKG_VERSION"));
    assert!(v.starts_with(&want), "{v}");
    if let Some(sha) = head {
        assert!(v.contains(&format!("+{sha}")), "{v} lacks {sha}");
        assert!(v.trim_end().ends_with(')'), "{v}");
    }
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

/// The named-file fixture of the 2026-11-07 lockfile handoff (synthetic
/// content only): five files a folder walk leaves out or keeps.
const NAMED: [(&str, &str); 5] = [
    ("app.js", "console.log('app');\n"),
    (
        "package-lock.json",
        "{\n  \"name\": \"root-lock\",\n  \"lockfileVersion\": 3\n}\n",
    ),
    ("web/package-lock.json", "{\n  \"name\": \"web-lock\"\n}\n"),
    (
        "Cargo.lock",
        "version = 4\n\n[[package]]\nname = \"cargo-lock-entry\"\n",
    ),
    ("vendor.min.js", "!function(){window.minifiedBundle=1}();\n"),
];
const SYNTHETIC_SECRET: &str = "sk-synthetic-0123456789abcdef0123";

fn named_fixture(tmp: &Path) -> std::path::PathBuf {
    let src = tmp.join("src");
    fs::create_dir_all(src.join("web")).unwrap();
    for (rel, body) in NAMED {
        fs::write(src.join(rel), body).unwrap();
    }
    fs::write(src.join(".env"), format!("API_TOKEN={SYNTHETIC_SECRET}\n")).unwrap();
    fs::write(src.join("logo.png"), b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR").unwrap();
    fs::write(tmp.join("outside.txt"), "OUTSIDE-CONTENT\n").unwrap();
    fs::create_dir_all(src.join(".ssh")).unwrap();
    fs::write(src.join(".ssh/config"), "Host synthetic-ssh-entry\n").unwrap();
    fs::write(src.join("prev_merged.md"), "# PREVIOUS-OUTPUT\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(tmp.join("outside.txt"), src.join("escape.txt")).unwrap();
    src
}

fn tm_owned(args: &[String], cwd: &Path) -> Output {
    let a: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    tm(&a, cwd)
}

#[test]
fn named_lockfiles_are_merged_and_lists_agree_with_the_merge() {
    // ADR 0020 addendum 2026-11-07, D8 "named = included": the SSM selection
    // named 253 files and got 250 with exit 0 — three lockfiles were left out
    // as "context bloat". A named file is merged; the walk is unchanged.
    let tmp = tempfile::tempdir().unwrap();
    let src = named_fixture(tmp.path());
    let src_s = src.to_string_lossy().into_owned();
    let names: Vec<&str> = NAMED.iter().map(|f| f.0).collect();
    let list = tmp.path().join("list.txt");
    fs::write(&list, names.join("\n") + "\n").unwrap();
    let plan = tmp.path().join("plan.toml");
    // `ordering = "plan"`: what a --files-from list gets (`from_paths`).
    let mut toml = String::from("version = 1\nmode = \"explicit\"\nordering = \"plan\"\n");
    for n in &names {
        toml.push_str(&format!("[[file]]\npath = \"{n}\"\n"));
    }
    fs::write(&plan, toml).unwrap();
    let pick = |flag: &str| -> Vec<String> {
        let file = if flag == "--files-from" { &list } else { &plan };
        vec![flag.into(), file.to_string_lossy().into_owned()]
    };
    let merge = |out: &str, extra: &[String]| -> (Output, String) {
        let out = tmp.path().join(out);
        let mut args = vec![
            "merge".to_string(),
            src_s.clone(),
            out.to_string_lossy().into_owned(),
        ];
        args.push("--yes".into());
        args.extend_from_slice(extra);
        let o = tm_owned(&args, tmp.path());
        (o, fs::read_to_string(&out).unwrap_or_default())
    };

    for flag in ["--files-from", "--selection"] {
        let (o, text) = merge(&format!("named{flag}.md"), &pick(flag));
        assert_eq!(o.status.code(), Some(0), "{flag}: {}", stdout(&o));
        assert!(
            stdout(&o).contains("merged=5 scan_skipped=0 "),
            "{flag}: {}",
            stdout(&o)
        );
        for (rel, body) in NAMED {
            assert!(text.contains(body), "{flag}: {rel} not merged verbatim");
        }
        assert!(!text.contains("context bloat"), "{flag}");
        // --fail-on-skip: nothing named was skipped.
        let mut strict = pick(flag);
        strict.push("--fail-on-skip".into());
        let (o, _) = merge(&format!("strict{flag}.md"), &strict);
        assert_eq!(o.status.code(), Some(0), "{flag}: {}", stdout(&o));
    }

    // --list-only lists what the merge merges, the same for both flags.
    let listed: Vec<String> = ["--files-from", "--selection"]
        .iter()
        .map(|flag| {
            let mut extra = pick(flag);
            extra.push("--list-only".into());
            let (o, _) = merge("unused.md", &extra);
            assert_eq!(o.status.code(), Some(0), "{flag}: {}", stdout(&o));
            stdout(&o)
        })
        .collect();
    assert_eq!(listed[0], listed[1]);
    for n in &names {
        assert!(listed[0].contains(n), "{n}: {}", listed[0]);
    }
    assert!(listed[0].contains("\n5 files, "), "{}", listed[0]);
    assert!(!tmp.path().join("unused.md").exists());

    // The folder walk is unchanged: lockfiles and bundles stay out on
    // purpose, exit 0; --fail-on-skip fails it, and the list predicts that.
    let (o, text) = merge("walk.md", &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    assert!(text.contains("console.log('app');"));
    for gone in [
        "root-lock",
        "web-lock",
        "cargo-lock-entry",
        "minifiedBundle",
    ] {
        assert!(!text.contains(gone), "walk merged {gone}");
    }
    assert!(text.contains("lock/system file (context bloat)"));
    let (o, _) = merge("walk2.md", &["--fail-on-skip".into()]);
    assert_eq!(o.status.code(), Some(3));
    let (o, _) = merge("walk3.md", &["--fail-on-skip".into(), "--list-only".into()]);
    assert_eq!(o.status.code(), Some(3), "{}", stdout(&o));
}

#[test]
fn named_files_a_safety_rule_holds_back_exit_3_and_stay_out() {
    let tmp = tempfile::tempdir().unwrap();
    let src = named_fixture(tmp.path());
    let src_s = src.to_string_lossy().into_owned();
    let run = |out: &str, names: &[&str], extra: &[&str]| -> (Output, String) {
        let list = tmp.path().join(format!("{out}.list"));
        fs::write(&list, names.join("\n") + "\n").unwrap();
        let out = tmp.path().join(out);
        let mut args: Vec<String> = vec![
            "merge".into(),
            src_s.clone(),
            out.to_string_lossy().into_owned(),
            "--yes".into(),
            "--files-from".into(),
            list.to_string_lossy().into_owned(),
        ];
        args.extend(extra.iter().map(|s| s.to_string()));
        let o = tm_owned(&args, tmp.path());
        (o, fs::read_to_string(&out).unwrap_or_default())
    };

    // A named credential file while redacting: withheld, exit 3, its value
    // nowhere in the output; the named lockfile is still merged.
    let (o, text) = run("redacted.md", &["package-lock.json", ".env"], &["--redact"]);
    assert_eq!(o.status.code(), Some(3), "{}", stdout(&o));
    assert!(stdout(&o).contains("not_captured=1"), "{}", stdout(&o));
    assert!(text.contains("root-lock"));
    assert!(text.contains("named but withheld: env file"), "{text}");
    assert!(!text.contains(SYNTHETIC_SECRET));
    assert!(!stdout(&o).contains(SYNTHETIC_SECRET));
    let (o, _) = run(
        "redacted-list.md",
        &["package-lock.json", ".env"],
        &["--redact", "--list-only"],
    );
    assert_eq!(o.status.code(), Some(3), "{}", stdout(&o));
    assert!(stdout(&o).contains("NOT CAPTURED  .env"), "{}", stdout(&o));
    assert!(!stdout(&o).contains(SYNTHETIC_SECRET));
    // Redaction off takes every text file, named ones included.
    let (o, text) = run("open.md", &[".env"], &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    assert!(text.contains(SYNTHETIC_SECRET));

    // A named picture: not text, so not merged without the archive twin
    // (exit 3); --complete keeps its bytes (exit 0), and the list says so.
    let (o, _) = run("pic.md", &["app.js", "logo.png"], &[]);
    assert_eq!(o.status.code(), Some(3), "{}", stdout(&o));
    assert!(stdout(&o).contains("not_captured=1"), "{}", stdout(&o));
    let (o, text) = run("pic-twin.md", &["app.js", "logo.png"], &["--complete"]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    assert!(text.contains("## __binary__: logo.png"));
    let (o, _) = run(
        "pic-list.md",
        &["app.js", "logo.png"],
        &["--complete", "--list-only"],
    );
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    assert!(
        stdout(&o).contains("TWIN BYTES    logo.png"),
        "{}",
        stdout(&o)
    );

    // A credential directory and a previous output stay out even with the
    // archive twin and redaction off: no text, no embedded bytes.
    let (o, text) = run(
        "withheld-twin.md",
        &["app.js", ".ssh/config", "prev_merged.md"],
        &["--complete"],
    );
    assert_eq!(o.status.code(), Some(3), "{}", stdout(&o));
    assert!(stdout(&o).contains("not_captured=2"), "{}", stdout(&o));
    assert!(!text.contains("synthetic-ssh-entry") && !text.contains("PREVIOUS-OUTPUT"));
    assert!(
        !text.contains("## __binary__: .ssh/config")
            && !text.contains("## __binary__: prev_merged.md")
    );

    // Missing, `..` and a symlink out of the folder: not captured, exit 3,
    // nothing outside the source folder read.
    let mut names = vec!["app.js", "missing.txt", "../outside.txt"];
    if cfg!(unix) {
        names.push("escape.txt");
    }
    let (o, text) = run("escape.md", &names, &[]);
    assert_eq!(o.status.code(), Some(3), "{}", stdout(&o));
    assert!(
        stdout(&o).contains(&format!("not_captured={}", names.len() - 1)),
        "{}",
        stdout(&o)
    );
    assert!(!text.contains("OUTSIDE-CONTENT"));
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
    // A zip is streamed (members are synthetic blocks): it must be journaled
    // and come back too (S10: streamed files were never journaled).
    fs::write(src.join("a_pack.zip"), b"\x50\x4b\x03\x04\x14\x00\x00\x00\x00\x00\x00\x20\x5f\x5d\xa7\xf4\x85\x0a\x0d\x00\x00\x00\x0d\x00\x00\x00\x0d\x00\x00\x00\x69\x6e\x6e\x65\x72\x2f\x6f\x6e\x65\x2e\x74\x78\x74\x66\x69\x72\x73\x74\x20\x6d\x65\x6d\x62\x65\x72\x0a\x50\x4b\x03\x04\x14\x00\x00\x00\x00\x00\x00\x20\x5f\x5d\xbd\xbc\x1c\x7a\x11\x00\x00\x00\x11\x00\x00\x00\x0c\x00\x00\x00\x69\x6e\x6e\x65\x72\x2f\x74\x77\x6f\x2e\x6d\x64\x23\x20\x73\x65\x63\x6f\x6e\x64\x0a\x0a\x6d\x65\x6d\x62\x65\x72\x0a\x50\x4b\x01\x02\x14\x03\x14\x00\x00\x00\x00\x00\x00\x20\x5f\x5d\xa7\xf4\x85\x0a\x0d\x00\x00\x00\x0d\x00\x00\x00\x0d\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x80\x01\x00\x00\x00\x00\x69\x6e\x6e\x65\x72\x2f\x6f\x6e\x65\x2e\x74\x78\x74\x50\x4b\x01\x02\x14\x03\x14\x00\x00\x00\x00\x00\x00\x20\x5f\x5d\xbd\xbc\x1c\x7a\x11\x00\x00\x00\x11\x00\x00\x00\x0c\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x80\x01\x38\x00\x00\x00\x69\x6e\x6e\x65\x72\x2f\x74\x77\x6f\x2e\x6d\x64\x50\x4b\x05\x06\x00\x00\x00\x00\x02\x00\x02\x00\x75\x00\x00\x00\x73\x00\x00\x00\x00\x00").unwrap();
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
    let journal = fs::read_to_string(ck.join("journal.jsonl")).unwrap();
    assert!(
        journal.contains("\"path\":\"a_pack.zip\""),
        "the streamed zip (first in order) is journaled"
    );

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
        vec!["merge", "gh:owner/repo", "o.md", "--checkpoint"],
    ] {
        let o = tm(&args, tmp.path());
        assert_eq!(o.status.code(), Some(2), "{args:?}");
        // Refused before anything is cloned (S11 B5).
        assert!(
            !String::from_utf8_lossy(&o.stderr).contains("cloning"),
            "{args:?}"
        );
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

/// S13 (HANDOFF_S8 §6.11 F, §10.4): a twin of four text files and a binary
/// of `kib` KiB, for the disk-full tests below. Over 1024 KiB (restore's write
/// buffer) the failing write comes while decoding; under it, at the flush.
#[cfg(unix)]
fn twin_for_full_disk(tmp: &Path, kib: u32) -> std::path::PathBuf {
    let src = tmp.join("src");
    fs::create_dir_all(src.join("d")).unwrap();
    for i in 0..4 {
        fs::write(
            src.join(format!("d/t{i}.txt")),
            format!("line {i}\n").repeat(500),
        )
        .unwrap();
    }
    let blob: Vec<u8> = (0..kib * 1024)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    fs::write(src.join("blob.bin"), blob).unwrap();
    // A plan names the binary: a folder walk skips it, so no twin would hold it.
    let plan = tmp.join("all.tmsel.toml");
    let mut toml = String::from("version = 1\nmode = \"explicit\"\ncomplete = true\n");
    for rel in ["blob.bin", "d/t0.txt", "d/t1.txt", "d/t2.txt", "d/t3.txt"] {
        toml.push_str(&format!("[[file]]\npath = \"{rel}\"\n"));
    }
    fs::write(&plan, toml).unwrap();
    let twin = tmp.join("twin.md");
    let o = tm(
        &[
            "merge",
            src.to_str().unwrap(),
            twin.to_str().unwrap(),
            "--selection",
            plan.to_str().unwrap(),
            "--yes",
            "-q",
        ],
        tmp,
    );
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    twin
}

/// Run `turbomerger restore TWIN --dest MNT/out EXTRA…` with a tmpfs of
/// `size` mounted at MNT inside an unprivileged mount namespace; returns
/// (exit code, stderr, files left under MNT), or None when this machine
/// cannot mount one (no `unshare`, user namespaces off).
#[cfg(target_os = "linux")]
fn restore_on_tmpfs(
    tmp: &Path,
    twin: &Path,
    size: &str,
    extra: &[&str],
) -> Option<(i32, String, Vec<String>)> {
    let mnt = tmp.join(format!("mnt_{size}_{}", extra.len()));
    fs::create_dir_all(&mnt).unwrap();
    let script = r#"m=$1; size=$2; shift 2
mount -t tmpfs -o "size=$size" none "$m" || exit 99
"$@"; rc=$?
find "$m" -type f -printf '@@file %P\n' >&2
exit $rc"#;
    let dest = mnt.join("out");
    let o = Command::new("unshare")
        .args(["-rm", "sh", "-c", script, "sh"])
        .arg(&mnt)
        .arg(size)
        .arg(env!("CARGO_BIN_EXE_turbomerger"))
        .args([
            "restore",
            twin.to_str().unwrap(),
            "--dest",
            dest.to_str().unwrap(),
        ])
        .args(extra)
        .env("TURBOMERGER_STATE_DIR", tmp.join("state"))
        .output()
        .ok()?;
    let code = o.status.code()?;
    if code == 99 {
        return None;
    }
    let err = String::from_utf8_lossy(&o.stderr).into_owned();
    let files = err
        .lines()
        .filter_map(|l| l.strip_prefix("@@file "))
        .map(str::to_string)
        .collect();
    Some((code, err, files))
}

#[cfg(target_os = "linux")]
#[test]
fn restore_onto_a_full_disk_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let twin = twin_for_full_disk(tmp.path(), 1536);
    // 64 KiB cannot hold a 1.5 MiB file: refused before anything is written.
    let Some((code, err, files)) = restore_on_tmpfs(tmp.path(), &twin, "64k", &[]) else {
        eprintln!("skipped: no unprivileged mount namespace (unshare -rm) on this machine");
        return;
    };
    assert_eq!(code, 1, "{err}");
    assert!(
        err.contains("not enough free space") && err.contains("nothing was written"),
        "{err}"
    );
    assert!(files.is_empty(), "{files:?}");
    // Without the check the disk fills up while staging: stopped, all or nothing.
    let (code, err, files) =
        restore_on_tmpfs(tmp.path(), &twin, "64k", &["--no-space-check"]).unwrap();
    assert_eq!(code, 1, "{err}");
    assert!(
        err.contains("the disk is full under") && err.contains("nothing was written"),
        "{err}"
    );
    assert!(files.is_empty(), "staged files left behind: {files:?}");
    // Enough room: every file back, byte for byte (the harness itself works).
    let (code, err, files) = restore_on_tmpfs(tmp.path(), &twin, "4m", &[]).unwrap();
    assert_eq!(code, 0, "{err}");
    assert_eq!(files.len(), 5, "{files:?}");
}

#[cfg(unix)]
#[test]
fn a_file_too_large_for_the_destination_writes_nothing() {
    // RLIMIT_FSIZE stands in for FAT32's 4 GiB cap: the write fails with EFBIG
    // (SIGXFSZ ignored, as a shell `trap '' XFSZ` leaves it across exec).
    let tmp = tempfile::tempdir().unwrap();
    let twin = twin_for_full_disk(tmp.path(), 200);
    let dest = tmp.path().join("back");
    let o = Command::new("sh")
        .args(["-c", "trap '' XFSZ; ulimit -f 128; exec \"$@\"", "sh"])
        .arg(env!("CARGO_BIN_EXE_turbomerger"))
        .args([
            "restore",
            twin.to_str().unwrap(),
            "--dest",
            dest.to_str().unwrap(),
        ])
        .env("TURBOMERGER_STATE_DIR", tmp.path().join("state"))
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&o.stderr);
    assert_eq!(o.status.code(), Some(1), "{err}");
    assert!(
        err.contains("blob.bin: too large for the file system")
            && err.contains("nothing was written"),
        "{err}"
    );
    assert!(!dest.exists(), "the folder restore made is removed again");
}
