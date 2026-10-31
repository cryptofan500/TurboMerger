//! Known-value masking regressions (plan v1 N-13, N-14, N-35; v2 §13 gates).

use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use tm_core::merger::{merge_files_with_progress, MergeConfig};
use tm_core::scanner::{scan_text_files, ScanOptions};

fn merge_to_string(root: &Path, out: &Path) -> String {
    let scan = scan_text_files(root, &ScanOptions::default()).expect("scan");
    let cancel = AtomicBool::new(false);
    merge_files_with_progress(
        root,
        &scan.files,
        out,
        &MergeConfig::default(),
        &cancel,
        |_, _, _| {},
        &scan.skipped,
    )
    .expect("merge");
    fs::read_to_string(out).unwrap()
}

#[test]
fn a5_output_is_byte_identical_across_runs_and_never_leaks_a_tail() {
    // v1 A.5: one secret is a prefix of another. v7.7.0 gave 2 distinct
    // outputs in 12 runs, half of them ending in "[REDACTED]W8nB3c".
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("detrepo");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join(".env"),
        "API_TOKEN=Qx7Zk2Mv9Rt4Lp\nAPI_TOKEN_V2=Qx7Zk2Mv9Rt4LpW8nB3c\n",
    )
    .unwrap();
    fs::write(
        root.join("NOTES.md"),
        "# Notes\nThe old value was Qx7Zk2Mv9Rt4Lp and the rotated value is Qx7Zk2Mv9Rt4LpW8nB3c today.\n",
    )
    .unwrap();
    fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();

    let first = merge_to_string(&root, &tmp.path().join("run0.md"));
    assert!(
        first.contains("The old value was [REDACTED] and the rotated value is [REDACTED] today."),
        "{first}"
    );
    assert!(!first.contains("W8nB3c"), "secret tail leaked");
    for i in 1..20 {
        let again = merge_to_string(&root, &tmp.path().join(format!("run{i}.md")));
        assert_eq!(again, first, "run {i} differs from run 0");
    }
}

#[test]
fn n13_a_security_paper_is_not_a_dump_and_rewrites_nothing() {
    // v1 N-13: papers flagged "credential-dense" (author e-mail lines, login
    // vocabulary) donated every letter+digit token to repo-wide masking:
    // `4,096-token` → `4,0[REDACTED]`, `Opus-4.8 medium` → `[REDACTED] medium`.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("papers");
    fs::create_dir_all(&root).unwrap();
    let mut flagged = String::from(
        "Attacks on login flows\nalice@uni.edu | Department of Computer Science\nbob@lab.org : Institute42 for Security\nWe study password managers.\n",
    );
    for i in 0..120 {
        flagged.push_str(&format!(
            "Paragraph {i}: we evaluate Opus-4.8 medium and claude-fable-5 with a 4,096-token window, following arXiv:2210.03629 and gpt-4o-mini-2024-07-18 baselines.\n"
        ));
    }
    fs::write(root.join("2608.00001.txt"), &flagged).unwrap();
    let other = "We compare Opus-4.8 medium with claude-fable-5, using a 4,096-token context (see arXiv:2210.03629). The gpt-4o-mini-2024-07-18 baseline is weaker.\n";
    fs::write(root.join("2608.00002.txt"), other).unwrap();
    fs::write(root.join("2608.00003.txt"), other).unwrap();

    let text = merge_to_string(&root, &tmp.path().join("papers.md"));
    // A credential dump needs one indicator line in ten; three author/login
    // lines in a 124-line paper are not a dump, so the paper is kept …
    assert!(
        !text.contains("`2608.00001.txt` — credential-dense"),
        "a security paper is not a credential dump"
    );
    // … with its own model names and numbers intact (the residency-folder
    // complaint: research text masked away) …
    assert!(
        text.contains(
            "Paragraph 60: we evaluate Opus-4.8 medium and claude-fable-5 with a 4,096-token window, following arXiv:2210.03629 and gpt-4o-mini-2024-07-18 baselines."
        ),
        "the paper's own text must survive: {text}"
    );
    // … and it donates nothing to repo-wide masking.
    assert!(
        !text.contains("Propagated known secret"),
        "no value may be masked repo-wide from a paper"
    );
    assert_eq!(
        text.matches(other.trim_end()).count(),
        2,
        "the other papers must come through byte-for-byte"
    );
}

#[test]
fn credential_dump_values_are_masked_only_as_whole_tokens() {
    // A real dump still teaches the masker its values (v7.5 behaviour) — but
    // a value never matches inside a longer word or number.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("dump");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("accounts.md"),
        "# account notes\npassword: Vr7wKp2walKotwica91\nzbig77@wp-post.org:Tr4mwaj9Nocny88\nRENDER key svcKey-Zx9Kq2Mv7Rt4Lp8Wb\n",
    )
    .unwrap();
    fs::write(
        root.join("NOTES.md"),
        "rotated svcKey-Zx9Kq2Mv7Rt4Lp8Wb today; build id xsvcKey-Zx9Kq2Mv7Rt4Lp8Wb99 is unrelated.\n",
    )
    .unwrap();
    let text = merge_to_string(&root, &tmp.path().join("dump.md"));
    // Every line is a credential, so this IS a dump: excluded and harvested.
    // (n13 above is the counterpart — the density path must still fire.)
    assert!(
        text.contains("`accounts.md` — credential-dense"),
        "a real dump must trip the density rule: {text}"
    );
    assert!(
        text.contains(
            "rotated [REDACTED] today; build id xsvcKey-Zx9Kq2Mv7Rt4Lp8Wb99 is unrelated."
        ),
        "{text}"
    );
}

#[test]
fn documents_about_secrets_merge_but_dumps_in_docs_do_not() {
    // S8 1.4 (DeepSeek B3.4): the file-name rule dropped an ADR about secret
    // masking. Under docs/ the name no longer decides; the content does.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("proj");
    fs::create_dir_all(root.join("docs/adr")).unwrap();
    fs::write(
        root.join("docs/adr/0013-phase1-secret-masking.md"),
        "# ADR 0013: secret masking\n\nWe mask secrets before a merge leaves the machine.\n",
    )
    .unwrap();
    fs::write(
        root.join("docs/SSM_CREDENTIALS_2026.md"),
        "# account notes\npassword: Vr7wKp2walKotwica91\nzbig77@wp-post.org:Tr4mwaj9Nocny88\nRENDER key svcKey-Zx9Kq2Mv7Rt4Lp8Wb\n",
    )
    .unwrap();
    let text = merge_to_string(&root, &tmp.path().join("docs.md"));
    assert!(
        text.contains("## docs/adr/0013-phase1-secret-masking.md"),
        "the ADR must be merged: {text}"
    );
    assert!(
        text.contains("`docs/SSM_CREDENTIALS_2026.md` — credential-dense"),
        "a dump inside docs/ is still excluded by its content: {text}"
    );
}
