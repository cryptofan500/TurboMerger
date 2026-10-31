//! S9 (HANDOFF_S8 §6.10 item 3.5, §6.11 D): `restore` rebuilds an archive
//! twin byte for byte, all or nothing, and never writes outside `dest`,
//! through a symlink, or over a different existing file.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use tm_core::merger::{merge_files_with_progress, MergeConfig};
use tm_core::restore::{restore, RestoreOptions};

/// (relative path, bytes) of every file under `root`, sorted.
fn tree(root: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(root, &p, out);
            } else {
                let rel = p
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((rel, fs::read(&p).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

/// Files that are hard to give back: line ends, no final newline, empty,
/// non-UTF-8, BOM, fences inside, a nested Unicode name, a zip, a script.
fn fixture(root: &Path) -> Vec<PathBuf> {
    let files: Vec<(&str, Vec<u8>)> = vec![
        ("a.txt", b"line\n".to_vec()),
        ("crlf.txt", b"p\r\nq\r\n".to_vec()),
        ("nofinal.txt", b"z".to_vec()),
        ("empty.txt", Vec::new()),
        ("only_newline.txt", b"\n".to_vec()),
        (
            "dir/sub/\u{fc}n\u{ef} c\u{f6}d\u{e9}.md",
            "# T\n\ntext\n".as_bytes().to_vec(),
        ),
        ("latin1.txt", b"caf\xe9\n".to_vec()),
        ("mixed.txt", b"a\nb\r\nc\n".to_vec()),
        ("bom.txt", "\u{feff}bom\n".as_bytes().to_vec()),
        ("fence.md", b"```\ncode\n```\n````\n".to_vec()),
        ("script.sh", b"#!/bin/sh\necho hi\n".to_vec()),
    ];
    let mut paths = Vec::new();
    for (rel, bytes) in files {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, bytes).unwrap();
        paths.push(p);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root.join("script.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    }
    let zip_path = root.join("pack.zip");
    let mut z = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
    z.start_file("inner.txt", zip::write::FileOptions::default())
        .unwrap();
    std::io::Write::write_all(&mut z, b"hello from inside\n").unwrap();
    z.finish().unwrap();
    paths.push(zip_path);
    paths
}

fn twin(root: &Path, files: &[PathBuf], out: &Path, cfg: MergeConfig) -> Vec<PathBuf> {
    let cfg = MergeConfig {
        complete: true,
        ..cfg
    };
    merge_files_with_progress(
        root,
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

fn plain() -> MergeConfig {
    MergeConfig {
        redact: false,
        ..MergeConfig::default()
    }
}

#[test]
fn a_twin_restores_every_file_byte_for_byte() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let files = fixture(&src);
    let parts = twin(&src, &files, &tmp.path().join("twin.md"), plain());
    let dest = tmp.path().join("restored");
    let r = restore(&parts, &dest, RestoreOptions::default()).expect("restore");
    assert_eq!(tree(&dest), tree(&src), "restored tree differs");
    assert_eq!(r.restored.len(), files.len(), "{:?}", r.restored);
    assert!(r.not_restorable.is_empty() && r.conflicts.is_empty());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(dest.join("script.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
    }
    // A second run finds everything in place and writes nothing.
    let again = restore(&parts, &dest, RestoreOptions::default()).expect("again");
    assert_eq!(again.identical.len(), files.len());
    assert!(again.restored.is_empty());
    // --check decodes everything and writes nothing.
    let checked = tmp.path().join("checked");
    let c = restore(
        &parts,
        &checked,
        RestoreOptions {
            check_only: true,
            ..Default::default()
        },
    )
    .expect("check");
    assert_eq!(c.restored.len(), files.len());
    assert!(!checked.exists());
}

#[test]
fn a_damaged_archive_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let files = fixture(&src);
    let out = tmp.path().join("twin.md");
    twin(&src, &files, &out, plain());
    let text = fs::read_to_string(&out).unwrap();
    // One character of embedded bytes, then one character of a text block.
    let b64_line = text
        .lines()
        .skip_while(|l| !l.starts_with("## __binary__: latin1.txt"))
        .nth(3)
        .unwrap()
        .to_string();
    let flipped: String = b64_line
        .chars()
        .enumerate()
        .map(|(i, c)| {
            if i == 0 {
                if c == 'A' {
                    'B'
                } else {
                    'A'
                }
            } else {
                c
            }
        })
        .collect();
    for (name, bad) in [
        ("bytes", text.replacen(&b64_line, &flipped, 1)),
        ("text", text.replacen("echo hi\n", "echo ho\n", 1)),
    ] {
        let damaged = tmp.path().join(format!("damaged_{name}.md"));
        fs::write(&damaged, bad).unwrap();
        let dest = tmp.path().join(format!("dest_{name}"));
        let err = restore(&[damaged], &dest, RestoreOptions::default()).unwrap_err();
        assert!(err.contains("nothing was written"), "{name}: {err}");
        assert!(
            tree(&dest).is_empty(),
            "{name}: files written: {:?}",
            tree(&dest)
        );
    }
}

#[test]
fn manifest_paths_cannot_leave_dest() {
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("dest");
    // "hi" = aGk=, sha256 of "hi".
    let sha = "8f434346648f6b96df89dda901c5176b10a6d83961dd3c1ac88b59b2dc327aa4";
    for path in [
        "../evil.txt",
        "/tmp/tm_restore_evil.txt",
        "a/../../evil.txt",
    ] {
        let archive = tmp.path().join("evil.md");
        fs::write(
            &archive,
            format!(
                "## __binary__: {path}\n\n```base64\naGk=\n```\n\n## __manifest__\n\n```jsonlines\n\
                 {{\"path\":\"{path}\",\"kind\":\"binary\",\"bytes\":2,\"sha256\":\"{sha}\",\"mode\":420}}\n```\n"
            ),
        )
        .unwrap();
        let err = restore(&[archive], &dest, RestoreOptions::default()).unwrap_err();
        assert!(err.contains("nothing was written"), "{path}: {err}");
        assert!(!tmp.path().join("evil.txt").exists());
    }
    assert!(!Path::new("/tmp/tm_restore_evil.txt").exists());
}

#[cfg(unix)]
#[test]
fn restore_never_writes_through_a_symlink() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir_all(src.join("dir")).unwrap();
    fs::write(src.join("dir/x.txt"), "x\n").unwrap();
    let parts = twin(
        &src,
        &[src.join("dir/x.txt")],
        &tmp.path().join("twin.md"),
        plain(),
    );
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let dest = tmp.path().join("dest");
    fs::create_dir_all(&dest).unwrap();
    std::os::unix::fs::symlink(&outside, dest.join("dir")).unwrap();
    let err = restore(&parts, &dest, RestoreOptions::default()).unwrap_err();
    assert!(err.contains("symlink"), "{err}");
    assert!(!outside.join("x.txt").exists());
}

#[test]
fn a_different_existing_file_stops_the_restore_unless_overwrite() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let files = fixture(&src);
    let parts = twin(&src, &files, &tmp.path().join("twin.md"), plain());
    let dest = tmp.path().join("dest");
    fs::create_dir_all(&dest).unwrap();
    fs::write(dest.join("a.txt"), "mine\n").unwrap();
    let r = restore(&parts, &dest, RestoreOptions::default()).expect("report");
    assert_eq!(r.conflicts, vec!["a.txt".to_string()]);
    assert!(r.restored.is_empty());
    assert_eq!(tree(&dest), vec![("a.txt".to_string(), b"mine\n".to_vec())]);
    let r = restore(
        &parts,
        &dest,
        RestoreOptions {
            overwrite: true,
            ..Default::default()
        },
    )
    .expect("overwrite");
    assert!(r.conflicts.is_empty());
    assert_eq!(tree(&dest), tree(&src));
}

#[test]
fn every_part_is_needed_and_withheld_files_are_named() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir_all(&src).unwrap();
    let mut files = Vec::new();
    for i in 0..6 {
        let p = src.join(format!("f{i}.txt"));
        fs::write(&p, format!("{}\n", "word ".repeat(150 + i))).unwrap();
        files.push(p);
    }
    fs::write(src.join("latin1.txt"), b"caf\xe9\n").unwrap();
    files.push(src.join("latin1.txt"));
    let parts = twin(
        &src,
        &files,
        &tmp.path().join("twin.md"),
        MergeConfig {
            max_tokens: Some(400),
            ..plain()
        },
    );
    assert!(parts.len() >= 3, "{} parts", parts.len());
    let dest = tmp.path().join("dest");
    restore(&parts, &dest, RestoreOptions::default()).expect("all parts");
    assert_eq!(tree(&dest), tree(&src));
    let missing_one: Vec<PathBuf> = parts[..parts.len() - 1].to_vec();
    let err = restore(
        &missing_one,
        &tmp.path().join("dest2"),
        RestoreOptions::default(),
    )
    .unwrap_err();
    assert!(
        err.contains(&format!(
            "part {} of {} is missing",
            parts.len(),
            parts.len()
        )),
        "{err}"
    );

    // Under --redact nothing is embedded; restore names what it cannot give back.
    let redacted = twin(
        &src,
        &files,
        &tmp.path().join("red.md"),
        MergeConfig {
            redact: true,
            ..MergeConfig::default()
        },
    );
    // A part given twice, or one from another merge, is refused too.
    let mut doubled = parts.clone();
    doubled[1] = parts[0].clone();
    let err = restore(
        &doubled,
        &tmp.path().join("dest4"),
        RestoreOptions::default(),
    )
    .unwrap_err();
    assert!(err.contains("given twice"), "{err}");
    let r = restore(
        &redacted,
        &tmp.path().join("dest3"),
        RestoreOptions::default(),
    )
    .expect("redacted");
    assert_eq!(r.not_restorable.len(), 1, "{:?}", r.not_restorable);
    assert_eq!(r.not_restorable[0].0, "latin1.txt");
    assert_eq!(r.restored.len(), 6);
}
