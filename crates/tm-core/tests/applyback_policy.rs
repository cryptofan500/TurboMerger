//! Apply-back safety regressions (plan v2 §15): control-path policy (N-52),
//! handle-relative writes that never follow links (N-17), encoding
//! round-trip (N-15), exec bits, case collisions, redaction placeholders and
//! trusted restore. Each test is one of the audit repros turned into an
//! assertion about the fixed behaviour.

use std::fs;
use std::path::PathBuf;

use tm_core::applyback::{
    apply_files, build_preview, parse_reply, restore_last, ApplyPolicy, BuiltPreview, PreviewFile,
};

struct Repo {
    root: PathBuf,
    tmp: tempfile::TempDir,
}

impl Repo {
    fn new() -> Repo {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        Repo { root, tmp }
    }
    fn write(&self, rel: &str, bytes: &[u8]) {
        let p = self.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, bytes).unwrap();
    }
    fn read(&self, rel: &str) -> Vec<u8> {
        fs::read(self.root.join(rel)).unwrap()
    }
    fn policy(&self, control: &[&str], manifest: &[&str], exec: bool) -> ApplyPolicy {
        let own = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        ApplyPolicy::new(&own(control), &own(manifest), exec)
            .unwrap()
            .with_state_dir(self.tmp.path().join("state"))
    }
    fn preview(&self, reply: &str, policy: &ApplyPolicy) -> BuiltPreview {
        build_preview(&self.root, &parse_reply(reply), policy).expect("preview")
    }
}

fn card<'a>(built: &'a BuiltPreview, rel: &str) -> &'a PreviewFile {
    built
        .preview
        .files
        .iter()
        .find(|f| f.rel_path == rel)
        .unwrap_or_else(|| panic!("{rel} missing from preview: {:?}", built.preview.files))
}

/// Apply only what the policy lets through without confirmation (CLI --yes).
fn apply_permitted(repo: &Repo, built: &BuiltPreview, policy: &ApplyPolicy) -> Vec<String> {
    let ok: Vec<_> = built
        .ready
        .iter()
        .filter(|f| !f.needs_confirm)
        .cloned()
        .collect();
    apply_files(&repo.root, &ok, policy).expect("apply").applied
}

const POISON: &str = include_str!("../../../fixtures/poison-applyback/reply.md");

#[cfg(unix)]
fn make_executable(p: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(p, fs::Permissions::from_mode(0o775)).unwrap();
}

#[cfg(unix)]
fn mode(p: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn poisoned_reply_writes_only_the_legitimate_change() {
    let repo = Repo::new();
    repo.write("src/main.rs", b"fn main() {}\n");
    repo.write(".git/hooks/pre-commit", b"#!/bin/sh\necho original hook\n");
    #[cfg(unix)]
    make_executable(&repo.root.join(".git/hooks/pre-commit"));
    let outside = repo.tmp.path().join("outside/target.txt");
    fs::create_dir_all(outside.parent().unwrap()).unwrap();
    fs::write(&outside, "ORIGINAL OUTSIDE CONTENT\n").unwrap();
    fs::create_dir_all(repo.root.join("docs")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("../../outside/target.txt", repo.root.join("docs/note.md")).unwrap();

    let policy = repo.policy(&[], &[], false);
    let built = repo.preview(POISON, &policy);

    // The legitimate change is appliable without ceremony.
    let main = card(&built, "src/main.rs");
    assert!(main.ok && !main.needs_confirm, "{main:?}");
    assert_eq!(main.class, "code");

    // .git internals are refused outright, including the case variant.
    for rel in [".git/hooks/pre-commit", ".GIT/hooks/post-checkout"] {
        let c = card(&built, rel);
        assert!(!c.ok, "{rel} must be refused: {c:?}");
        assert_eq!(c.class, "forbidden");
        assert!(c.note.contains(".git"), "{}", c.note);
    }
    // Control files and manifests are held for explicit confirmation.
    for (rel, class) in [
        (".github/workflows/ci.yml", "control"),
        (".vscode/tasks.json", "control"),
        (".claude/settings.json", "control"),
        ("build.rs", "manifest"),
    ] {
        let c = card(&built, rel);
        assert!(c.ok && c.needs_confirm, "{rel}: {c:?}");
        assert_eq!(c.class, class, "{rel}");
    }
    #[cfg(unix)]
    {
        let note = card(&built, "docs/note.md");
        assert!(!note.ok && note.note.contains("symlink"), "{note:?}");
    }

    let applied = apply_permitted(&repo, &built, &policy);
    // Without the planted symlink (non-Unix) docs/note.md is an ordinary new file.
    let expected: &[&str] = if cfg!(unix) {
        &["src/main.rs"]
    } else {
        &["src/main.rs", "docs/note.md"]
    };
    assert_eq!(applied, expected);

    // Nothing else changed: hook content and mode, outside file, no new files.
    assert_eq!(
        repo.read(".git/hooks/pre-commit"),
        b"#!/bin/sh\necho original hook\n"
    );
    #[cfg(unix)]
    assert_eq!(mode(&repo.root.join(".git/hooks/pre-commit")), 0o775);
    assert_eq!(
        fs::read_to_string(&outside).unwrap(),
        "ORIGINAL OUTSIDE CONTENT\n"
    );
    for rel in [
        ".github/workflows/ci.yml",
        ".vscode/tasks.json",
        ".claude/settings.json",
        "build.rs",
        ".GIT/hooks/post-checkout",
    ] {
        if rel == ".GIT/hooks/post-checkout" && !cfg!(target_os = "linux") {
            continue; // same directory as .git on case-insensitive volumes
        }
        assert!(!repo.root.join(rel).exists(), "{rel} must not be created");
    }
}

#[test]
fn allow_flags_unlock_exactly_what_they_name() {
    let repo = Repo::new();
    repo.write("src/main.rs", b"fn main() {}\n");
    let policy = repo.policy(&[".github/workflows/*"], &["build.rs"], false);
    let built = repo.preview(POISON, &policy);
    assert!(!card(&built, ".github/workflows/ci.yml").needs_confirm);
    assert!(!card(&built, "build.rs").needs_confirm);
    assert!(card(&built, ".vscode/tasks.json").needs_confirm);

    let applied = apply_permitted(&repo, &built, &policy);
    assert!(applied.contains(&".github/workflows/ci.yml".to_string()));
    assert!(applied.contains(&"build.rs".to_string()));
    assert!(!repo.root.join(".vscode/tasks.json").exists());

    // Even a match-everything allow list never opens .git.
    let all = repo.policy(&["**"], &["**"], true);
    let built = repo.preview(POISON, &all);
    assert!(!card(&built, ".git/hooks/pre-commit").ok);
    let forged: Vec<_> = built
        .ready
        .iter()
        .filter(|f| f.rel_path.starts_with(".git/"))
        .collect();
    assert!(forged.is_empty(), "no .git file may become appliable");
}

#[test]
fn gui_confirmation_unlocks_a_single_control_file() {
    let repo = Repo::new();
    let reply = "## .vscode/settings.json\n\n```json\n{ \"editor.tabSize\": 2 }\n```\n\n## .vscode/tasks.json\n\n```json\n{}\n```\n";
    let mut policy = repo.policy(&[], &[], false);
    let built = repo.preview(reply, &policy);
    assert!(built.ready.iter().all(|f| f.needs_confirm));

    // Unconfirmed: refused at apply time too (defence in depth).
    let out = apply_files(&repo.root, &built.ready, &policy).unwrap();
    assert!(out.applied.is_empty());
    assert_eq!(out.failed.len(), 2);

    policy.confirmed = vec![".vscode/settings.json".to_string()];
    let built = repo.preview(reply, &policy);
    let out = apply_files(&repo.root, &built.ready, &policy).unwrap();
    assert_eq!(out.applied, vec![".vscode/settings.json".to_string()]);
    assert_eq!(out.failed.len(), 1);
    assert!(out.failed[0].reason.contains("confirmation"));
}

#[test]
fn windows_1252_file_keeps_its_bytes_through_a_diff() {
    // v1 A.8: line one ends in é (0xE9); the diff touches line 3 only.
    let repo = Repo::new();
    repo.write("menu.txt", b"line one caf\xe9\nline two\nline three\n");
    let reply =
        "--- a/menu.txt\n+++ b/menu.txt\n@@ -2,2 +2,2 @@\n line two\n-line three\n+line THREE\n";
    let policy = repo.policy(&[], &[], false);
    let built = repo.preview(reply, &policy);
    let c = card(&built, "menu.txt");
    assert!(c.ok, "{c:?}");
    assert_eq!(c.encoding, "windows-1252");
    assert_eq!((c.adds, c.dels), (1, 1), "only line 3 changes");
    apply_permitted(&repo, &built, &policy);
    assert_eq!(
        repo.read("menu.txt"),
        b"line one caf\xe9\nline two\nline THREE\n"
    );
}

#[test]
fn unrepresentable_characters_are_refused_not_mangled() {
    let repo = Repo::new();
    repo.write("menu.txt", b"caf\xe9\n");
    let reply = "## menu.txt\n\n```\ncafé — 東京\n```\n";
    let built = repo.preview(reply, &repo.policy(&[], &[], false));
    let c = card(&built, "menu.txt");
    assert!(!c.ok && c.note.contains("cannot represent"), "{c:?}");
    assert!(built.ready.is_empty());
}

#[test]
fn utf16_file_keeps_bom_and_encoding() {
    let repo = Repo::new();
    let enc = |s: &str| {
        let mut b = vec![0xFF, 0xFE];
        for u in s.encode_utf16() {
            b.extend_from_slice(&u.to_le_bytes());
        }
        b
    };
    repo.write("wide.txt", &enc("alpha\r\nbeta\r\n"));
    let reply = "## wide.txt\n\n```\nalpha\ngamma\n```\n";
    let policy = repo.policy(&[], &[], false);
    let built = repo.preview(reply, &policy);
    assert_eq!(card(&built, "wide.txt").encoding, "UTF-16LE (BOM)");
    apply_permitted(&repo, &built, &policy);
    assert_eq!(repo.read("wide.txt"), enc("alpha\r\ngamma\r\n"));
}

#[cfg(unix)]
#[test]
fn executable_files_need_allow_exec_and_new_files_never_get_the_bit() {
    let repo = Repo::new();
    repo.write("run.sh", b"#!/bin/sh\necho one\n");
    make_executable(&repo.root.join("run.sh"));
    let reply = "## run.sh\n\n```sh\n#!/bin/sh\necho two\n```\n\n## tools/new.sh\n\n```sh\n#!/bin/sh\necho new\n```\n";

    let strict = repo.policy(&[], &[], false);
    let built = repo.preview(reply, &strict);
    let run = card(&built, "run.sh");
    assert!(!run.ok && run.note.contains("--allow-exec"), "{run:?}");
    let new = card(&built, "tools/new.sh");
    assert!(new.ok && new.mode_note.contains("without the executable bit"));
    apply_permitted(&repo, &built, &strict);
    assert_eq!(repo.read("run.sh"), b"#!/bin/sh\necho one\n");
    assert_eq!(mode(&repo.root.join("tools/new.sh")) & 0o111, 0);

    let lenient = repo.policy(&[], &[], true);
    let built = repo.preview("## run.sh\n\n```sh\n#!/bin/sh\necho two\n```\n", &lenient);
    assert_eq!(card(&built, "run.sh").mode_note, "keeps its executable bit");
    apply_permitted(&repo, &built, &lenient);
    assert_eq!(repo.read("run.sh"), b"#!/bin/sh\necho two\n");
    assert_eq!(mode(&repo.root.join("run.sh")), 0o775);
}

#[test]
fn case_only_collisions_are_refused() {
    let repo = Repo::new();
    repo.write("README.md", b"# readme\n");
    let policy = repo.policy(&[], &[], false);

    // Two proposals in one reply that differ only by case.
    let reply = "## docs/Guide.md\n\n```\none\n```\n\n## docs/guide.md\n\n```\ntwo\n```\n";
    let built = repo.preview(reply, &policy);
    assert!(!card(&built, "docs/Guide.md").ok);
    assert!(!card(&built, "docs/guide.md").ok);
    assert!(built.ready.is_empty());

    // A proposal that differs from an existing file only by case.
    let built = repo.preview("## Readme.md\n\n```\n# other\n```\n", &policy);
    let c = card(&built, "Readme.md");
    assert!(!c.ok, "{c:?}");
    assert!(
        c.note.contains("README.md"),
        "must name the real file: {}",
        c.note
    );
    assert_eq!(repo.read("README.md"), b"# readme\n");
}

#[test]
fn trailing_separator_and_escapes_are_refused_before_io() {
    let repo = Repo::new();
    let reply = "## docs/\n\n```\nx\n```\n\n## ../escape.md\n\n```\nx\n```\n";
    let built = repo.preview(reply, &repo.policy(&[], &[], false));
    assert!(built.ready.is_empty(), "{:?}", built.preview.files);
    assert!(!repo.tmp.path().join("escape.md").exists());
    assert!(!repo.root.join("docs").exists());
}

#[test]
fn redaction_placeholders_never_overwrite_real_values() {
    let repo = Repo::new();
    repo.write(
        "config.py",
        b"TOKEN = \"ghp_realvaluerealvaluerealvaluerealva1\"\nDEBUG = False\n",
    );
    // The LLM saw a redacted merge and sent the file back with a fix.
    let reply = "## config.py\n\n```python\nTOKEN = \"[REDACTED]\"\nDEBUG = True\n```\n";
    let built = repo.preview(reply, &repo.policy(&[], &[], false));
    let c = card(&built, "config.py");
    assert!(!c.ok && c.note.contains("[REDACTED]"), "{c:?}");
    assert!(built.ready.is_empty());
}

#[test]
fn restore_reverses_an_apply_and_skips_later_edits() {
    let repo = Repo::new();
    repo.write("a.txt", b"one\n");
    repo.write("b.txt", b"bee\n");
    let policy = repo.policy(&[], &[], false);
    let reply =
        "## a.txt\n\n```\ntwo\n```\n\n## b.txt\n\n```\nBEE\n```\n\n## c.txt\n\n```\nnew\n```\n";
    let built = repo.preview(reply, &policy);
    apply_permitted(&repo, &built, &policy);
    // The user keeps working on b.txt after the apply.
    repo.write("b.txt", b"BEE plus my own edit\n");

    let r = restore_last(&repo.root, &policy).expect("restore");
    assert_eq!(r.restored, vec!["a.txt".to_string()]);
    assert_eq!(r.deleted, vec!["c.txt".to_string()]);
    assert_eq!(r.skipped.len(), 1);
    assert_eq!(r.skipped[0].rel_path, "b.txt");
    assert_eq!(repo.read("a.txt"), b"one\n");
    assert_eq!(repo.read("b.txt"), b"BEE plus my own edit\n");
    assert!(!repo.root.join("c.txt").exists());

    // Idempotent.
    let again = restore_last(&repo.root, &policy).expect("restore again");
    assert_eq!(again.restored, vec!["a.txt".to_string()]);
    assert!(again.deleted.is_empty());
}

#[test]
fn restore_refuses_backups_this_machine_did_not_make() {
    // A cloned repo ships its own ".turbomerger/backups" tree whose manifest
    // "restores" an IDE auto-task and a source file.
    let repo = Repo::new();
    repo.write("src/main.rs", b"fn main() {}\n");
    repo.write(
        ".turbomerger/backups/9999-01-01T00-00-00Z/manifest.json",
        br#"{"created_utc":"9999","root":"x","entries":[{"path":"src/main.rs","existed":true},{"path":".vscode/tasks.json","existed":true}]}"#,
    );
    repo.write(
        ".turbomerger/backups/9999-01-01T00-00-00Z/files/src/main.rs",
        b"fn main() { evil() }\n",
    );
    repo.write(
        ".turbomerger/backups/9999-01-01T00-00-00Z/files/.vscode/tasks.json",
        b"{\"runOn\":\"folderOpen\"}\n",
    );
    let policy = repo.policy(&[], &[], false);
    let err = restore_last(&repo.root, &policy).unwrap_err();
    assert!(
        err.contains("not created by TurboMerger on this machine"),
        "{err}"
    );
    assert_eq!(repo.read("src/main.rs"), b"fn main() {}\n");
    assert!(!repo.root.join(".vscode/tasks.json").exists());

    // A genuine backup whose manifest is edited afterwards is refused too.
    let repo = Repo::new();
    repo.write("a.txt", b"one\n");
    let built = repo.preview("## a.txt\n\n```\ntwo\n```\n", &policy);
    let out = apply_files(&repo.root, &built.ready, &repo.policy(&[], &[], false)).unwrap();
    let manifest = PathBuf::from(out.backup_dir.unwrap()).join("manifest.json");
    let tampered = fs::read_to_string(&manifest)
        .unwrap()
        .replace("\"a.txt\"", "\"b.txt\"");
    fs::write(&manifest, tampered).unwrap();
    assert!(restore_last(&repo.root, &repo.policy(&[], &[], false)).is_err());
}

#[cfg(unix)]
#[test]
fn symlinked_backup_directory_is_never_written_through() {
    let repo = Repo::new();
    repo.write("a.txt", b"one\n");
    let elsewhere = repo.tmp.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, repo.root.join(".turbomerger")).unwrap();
    let policy = repo.policy(&[], &[], false);
    let built = repo.preview("## a.txt\n\n```\ntwo\n```\n", &policy);
    let err = apply_files(&repo.root, &built.ready, &policy).unwrap_err();
    assert!(err.contains("symlink"), "{err}");
    assert_eq!(
        repo.read("a.txt"),
        b"one\n",
        "nothing written without a backup"
    );
    assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
}
