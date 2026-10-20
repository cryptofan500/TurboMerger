//! Text inside images: photos, screenshots, scans, and the pages of a PDF
//! that have no text layer. Off by default (`--extract-images-text`): OCR is
//! slow (seconds per page) and runs as an external program.
//!
//! The engine is `monaddoc` (pdf_to_markdown_claude: PP-OCRv5 with page
//! dewarping, orientation and table recovery — it reads photographed and
//! rotated pages that Tesseract turns into noise). `$TURBOMERGER_OCR_CMD`
//! swaps in any command that prints one file's text on stdout; `{input}` in
//! it stands for the file (`tesseract {input} stdout`).
//!
//! One OCR process at a time: an engine loads large models (monaddoc peaks
//! near 1.2 GB), and the merger works on many files at once.

use std::io::ErrorKind;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;

static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

const NOT_FOUND: &str = "OCR engine not found: install monaddoc (pdf_to_markdown_claude), \
or set TURBOMERGER_OCR_CMD to a command that prints a file's text (e.g. \"tesseract {input} stdout\")";

/// The text of `path` page by page (images: one page; PDFs: one entry per
/// page when the engine marks pages, else one entry for the whole file).
pub(super) fn ocr_pages(path: &Path, cmd: Option<&str>) -> Result<Vec<String>, String> {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let custom = cmd
        .map(str::to_string)
        .or_else(|| std::env::var("TURBOMERGER_OCR_CMD").ok())
        .filter(|c| !c.trim().is_empty());
    match custom {
        Some(template) => custom_command(&template, path).map(|t| vec![t]),
        None => monaddoc(path),
    }
}

/// `program arg …` with `{input}` replaced by the file (appended when absent).
fn custom_command(template: &str, path: &Path) -> Result<String, String> {
    let mut words = template.split_whitespace();
    let program = words.next().ok_or("empty OCR command")?;
    let input = path.to_string_lossy();
    let mut args: Vec<String> = words.map(|w| w.replace("{input}", &input)).collect();
    if !template.contains("{input}") {
        args.push(input.into_owned());
    }
    let out = Command::new(program).args(&args).output().map_err(|e| {
        if e.kind() == ErrorKind::NotFound {
            format!("OCR command '{program}' not found")
        } else {
            format!("OCR command '{program}' did not start: {e}")
        }
    })?;
    if !out.status.success() {
        return Err(format!("OCR command '{program}' failed ({})", out.status));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `monaddoc run --only md-structured` into a throwaway folder, then its
/// structured Markdown split into pages.
fn monaddoc(path: &Path) -> Result<Vec<String>, String> {
    let tmp = tempfile::tempdir().map_err(|e| format!("OCR: no temporary folder: {e}"))?;
    let out_dir = tmp.path().join("out");
    let output = Command::new("monaddoc")
        .arg("run")
        .arg("--input")
        .arg(path)
        .arg("--work")
        .arg(tmp.path().join("work"))
        .arg("--out-dir")
        .arg(&out_dir)
        .args(["--stem", "ocr", "--only", "md-structured", "--json"])
        .output()
        .map_err(|e| {
            if e.kind() == ErrorKind::NotFound {
                NOT_FOUND.to_string()
            } else {
                format!("monaddoc did not start: {e}")
            }
        })?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stdout);
        let detail = said
            .lines()
            .find(|l| l.contains("\"detail\"") || l.contains("\"error\""))
            .map(|l| l.trim().trim_end_matches(','))
            .unwrap_or("no detail");
        return Err(format!("monaddoc failed ({}): {detail}", output.status));
    }
    let md = std::fs::read_to_string(out_dir.join("ocr_3_structured.md"))
        .map_err(|e| format!("monaddoc wrote no Markdown: {e}"))?;
    Ok(split_pages(&md))
}

/// monaddoc's structured Markdown → plain text per page. Page anchors
/// (`<a id="page-000001">`) split pages; its comments (provenance, REVIEW
/// notes), figure links and the title go; headings, lists and tables stay.
pub(super) fn split_pages(md: &str) -> Vec<String> {
    let mut pages = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    let mut in_comment = false;
    for line in md.lines() {
        let t = line.trim();
        if in_comment {
            in_comment = !t.contains("-->");
            continue;
        }
        if t.starts_with("<a id=\"page-") {
            if started {
                pages.push(std::mem::take(&mut cur));
            }
            started = true;
            continue;
        }
        if t.starts_with("<!--") {
            in_comment = !t.contains("-->");
            continue;
        }
        if t.starts_with("![") || (!started && t.starts_with("# ")) {
            continue;
        }
        cur.push_str(line);
        cur.push('\n');
    }
    if started || !cur.trim().is_empty() {
        pages.push(cur);
    }
    pages.into_iter().map(|p| collapse(&p)).collect()
}

/// Squeeze blank-line runs and trim.
fn collapse(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank = 0;
    for line in text.lines() {
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monaddoc_markdown_splits_into_clean_pages() {
        let md = "# ocr\n\n<!-- MonadDoc 0.4.0 structured Markdown -->\n\n\
                  <a id=\"page-000001\"></a>\n<!-- page ? | PDF page 1 | photo a.jpg -->\n\n\
                  ![Figure 1 on page 1](ocr_assets/p001-1.webp)\n\nFirst page text\n\n\
                  | A | B |\n|---|---|\n| 1 | 2 |\n<!-- REVIEW a.jpg L3 conf 0.5: x -->\n\n\
                  <a id=\"page-000002\"></a>\n<!-- multi\nline comment -->\nSecond page\n";
        let pages = split_pages(md);
        assert_eq!(pages.len(), 2, "{pages:?}");
        assert_eq!(
            pages[0],
            "First page text\n\n| A | B |\n|---|---|\n| 1 | 2 |"
        );
        assert_eq!(pages[1], "Second page");
    }

    #[test]
    fn custom_command_substitutes_input() {
        // `echo` stands in for an OCR engine on Unix.
        #[cfg(unix)]
        {
            let t = custom_command("echo got {input}", Path::new("/x/scan.png")).unwrap();
            assert_eq!(t.trim(), "got /x/scan.png");
            let t = custom_command("echo appended", Path::new("/x/a.png")).unwrap();
            assert_eq!(t.trim(), "appended /x/a.png");
        }
    }
}
