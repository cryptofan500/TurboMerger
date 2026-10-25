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
//! near 1.2 GB), and the merger works on many files at once. Each runs in
//! [`super::child`]: Cancel stops it mid-picture, and it never outlives the
//! merge.
//!
//! A merge does not read pictures one by one (P7): [`OcrQueue`] collects
//! them while the text is merged, and its [`OcrQueue::run`] is the merge's
//! OCR stage. Each distinct picture (by the SHA-256 of its bytes) is read
//! once, however many copies the folder holds; pictures go to the engine in
//! batches of up to [`BATCH_PAGES`] pages (one model load per batch); and
//! every result is kept in a cache (`<user cache>/turbomerger/ocr-v1/
//! <sha256>-<engine>.txt`), so the next run reads nothing it read before and
//! a cancel loses at most the batch in flight.

use std::collections::HashMap;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use sha2::{Digest, Sha256};

use super::child::{self, Event, RunError};

static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

const NOT_FOUND: &str = "OCR engine not found: install monaddoc (pdf_to_markdown_claude), \
or set TURBOMERGER_OCR_CMD to a command that prints a file's text (e.g. \"tesseract {input} stdout\")";

const CANCELLED: &str = "OCR cancelled";

/// What a picture the deadline left unread is recorded as.
pub(crate) const FINISHED_EARLY: &str = "not read with OCR — the run was finished early (deadline)";

/// Pages per engine start (`TURBOMERGER_OCR_BATCH` overrides it). A start
/// costs a model load (seconds); a batch is also what a cancel can lose,
/// because each finished batch is in the cache at once.
const BATCH_PAGES: usize = 64;

/// Seconds per page before this machine's own speed is known.
const DEFAULT_SECS_PER_PAGE: f64 = 10.0;

/// Stands for the figures folder in a page text until the output is known:
/// the folder's name depends on the output file, a cache entry must not.
const ASSETS: &str = "{TURBOMERGER_ASSETS}";

/// Where a merge keeps the figure crops monaddoc cuts out of pages
/// (`--keep-figures`): `<output stem>_assets/` beside the output.
struct Figures {
    dir: PathBuf,
    /// How a link in the output names that folder: its file name.
    href: String,
    kept: AtomicUsize,
}

/// The text of `path` page by page (images: one page; PDFs: one entry per
/// page when the engine marks pages, else one entry for the whole file).
/// Reads at once, without the queue — for callers outside a merge.
pub(super) fn ocr_pages(
    path: &Path,
    cmd: Option<&str>,
    cancel: Option<&AtomicBool>,
) -> Result<Vec<String>, String> {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    // Cancelled while this call waited for its turn: start nothing.
    if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
        return Err(CANCELLED.into());
    }
    match Engine::pick(cmd) {
        Engine::Command(template) => {
            custom_command(&template, path, cancel, &mut || {}).map(|t| vec![t])
        }
        Engine::Monaddoc => monaddoc(path, cancel),
    }
}

/// `program arg …` with `{input}` replaced by the file (appended when absent).
/// `tick` runs about ten times a second while the program works.
fn custom_command(
    template: &str,
    path: &Path,
    cancel: Option<&AtomicBool>,
    tick: &mut dyn FnMut(),
) -> Result<String, String> {
    let mut words = template.split_whitespace();
    let program = words.next().ok_or("empty OCR command")?;
    let input = path.to_string_lossy();
    let mut args: Vec<String> = words.map(|w| w.replace("{input}", &input)).collect();
    if !template.contains("{input}") {
        args.push(input.into_owned());
    }
    let tmp = tempfile::tempdir().map_err(|e| format!("OCR: no temporary folder: {e}"))?;
    let mut command = Command::new(program);
    command.args(&args);
    let out = child::run(&mut command, tmp.path(), cancel, |e| {
        if let Event::Tick = e {
            tick();
        }
    })
    .map_err(|e| match e {
        RunError::Cancelled => CANCELLED.to_string(),
        RunError::Spawn(e) if e.kind() == ErrorKind::NotFound => {
            format!("OCR command '{program}' not found")
        }
        RunError::Spawn(e) => format!("OCR command '{program}' did not start: {e}"),
        RunError::Io(e) => format!("OCR command '{program}': {e}"),
    })?;
    if !out.status.success() {
        return Err(match out.stderr_tail.lines().last() {
            Some(said) => format!("OCR command '{program}' failed ({}): {said}", out.status),
            None => format!("OCR command '{program}' failed ({})", out.status),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `monaddoc run --only md-structured` into a throwaway folder, then its
/// structured Markdown split into pages.
fn monaddoc(path: &Path, cancel: Option<&AtomicBool>) -> Result<Vec<String>, String> {
    let tmp = tempfile::tempdir().map_err(|e| format!("OCR: no temporary folder: {e}"))?;
    let out_dir = tmp.path().join("out");
    let mut command = monaddoc_run(path, &tmp.path().join("work"), &out_dir);
    let output = child::run(&mut command, tmp.path(), cancel, |_| {}).map_err(|e| match e {
        RunError::Cancelled => CANCELLED.to_string(),
        RunError::Spawn(e) if e.kind() == ErrorKind::NotFound => NOT_FOUND.to_string(),
        RunError::Spawn(e) => format!("monaddoc did not start: {e}"),
        RunError::Io(e) => format!("monaddoc: {e}"),
    })?;
    if !output.status.success() {
        return Err(monaddoc_failure(&output));
    }
    let md = std::fs::read_to_string(out_dir.join("ocr_3_structured.md"))
        .map_err(|e| format!("monaddoc wrote no Markdown: {e}"))?;
    Ok(split_pages(&md))
}

/// The `monaddoc run` command for one input (a picture, a PDF or a folder).
fn monaddoc_run(input: &Path, work: &Path, out_dir: &Path) -> Command {
    let mut command = Command::new("monaddoc");
    command
        .arg("run")
        .arg("--input")
        .arg(input)
        .arg("--work")
        .arg(work)
        .arg("--out-dir")
        .arg(out_dir)
        .args(["--stem", "ocr", "--only", "md-structured", "--json"]);
    command
}

/// Why a monaddoc run that exited with an error failed, in one line.
fn monaddoc_failure(output: &child::Finished) -> String {
    let said = String::from_utf8_lossy(&output.stdout);
    let detail = said
        .lines()
        .find(|l| l.contains("\"detail\"") || l.contains("\"error\""))
        .map(|l| l.trim().trim_end_matches(',').to_string())
        .or_else(|| output.stderr_tail.lines().last().map(str::to_string))
        .unwrap_or_else(|| "no detail".into());
    format!("monaddoc failed ({}): {detail}", output.status)
}

/// monaddoc's structured Markdown → plain text per page. Page anchors
/// (`<a id="page-000001">`) split pages; its comments (provenance, REVIEW
/// notes), figure links and the title go; headings, lists and tables stay.
pub(super) fn split_pages(md: &str) -> Vec<String> {
    split_photos(md).into_iter().map(|(_, text)| text).collect()
}

/// [`split_pages`], with each page's photo file (`<!-- page ? | PDF page 3 |
/// photo IMG_1.jpg -->`; empty when the page names none).
pub(super) fn split_photos(md: &str) -> Vec<(String, String)> {
    split_photos_opts(md, false)
}

/// [`split_photos`]; with `keep_links`, each page keeps monaddoc's figure
/// links (`![Figure 1 on page 2](ocr_assets/p002-1.webp)`) for
/// [`keep_figures`] to copy and rename.
pub(super) fn split_photos_opts(md: &str, keep_links: bool) -> Vec<(String, String)> {
    let mut pages = Vec::new();
    let mut cur = String::new();
    let mut photo = String::new();
    let mut started = false;
    let mut want_photo = false;
    let mut in_comment = false;
    for line in md.lines() {
        let t = line.trim();
        if in_comment {
            in_comment = !t.contains("-->");
            continue;
        }
        if t.starts_with("<a id=\"page-") {
            if started {
                pages.push((std::mem::take(&mut photo), std::mem::take(&mut cur)));
            }
            started = true;
            want_photo = true;
            continue;
        }
        if t.starts_with("<!--") {
            if want_photo {
                if let Some(p) = photo_of(t) {
                    photo = p;
                    want_photo = false;
                }
            }
            in_comment = !t.contains("-->");
            continue;
        }
        if (t.starts_with("![") && !keep_links) || (!started && t.starts_with("# ")) {
            continue;
        }
        cur.push_str(line);
        cur.push('\n');
    }
    if started || !cur.trim().is_empty() {
        pages.push((photo, cur));
    }
    pages
        .into_iter()
        .map(|(photo, text)| (photo, collapse(&text)))
        .collect()
}

/// A page's figure links made permanent: each crop monaddoc wrote into
/// `crops` (`ocr_assets/pPPP-K.webp`, numbered per batch) is copied to the
/// figures folder as `<picture sha256, 12 hex>-<page of the picture>-K.webp`
/// (the same name every run, so the cache can hold it), and the link names
/// it through [`ASSETS`]. A link whose crop is missing is dropped: no
/// dangling links.
fn keep_figures(figures: &Figures, crops: &Path, hex: &str, page: usize, text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let t = line.trim();
        let link = t
            .starts_with("![")
            .then(|| t.rsplit_once("](ocr_assets/"))
            .flatten()
            .and_then(|(alt, rest)| rest.strip_suffix(')').map(|name| (alt, name)));
        let Some((alt, name)) = link else {
            out.push_str(line);
            out.push('\n');
            continue;
        };
        let k = name.rsplit_once('-').map_or(name, |(_, k)| k);
        let dest = format!("{hex}-{page}-{k}");
        let copied = std::fs::create_dir_all(&figures.dir).is_ok()
            && std::fs::copy(crops.join(name), figures.dir.join(&dest)).is_ok();
        if copied {
            figures.kept.fetch_add(1, Ordering::Relaxed);
            out.push_str(&format!("{alt}]({ASSETS}/{dest})\n"));
        }
    }
    out.trim_end().to_string()
}

/// The first 12 hex digits of a content hash: a picture's name in the
/// figures folder.
fn hex12(key: &Key) -> String {
    key.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

/// `<!-- page ? | PDF page 3 | photo IMG_1.jpg -->` → `IMG_1.jpg`.
fn photo_of(comment: &str) -> Option<String> {
    comment
        .trim_start_matches("<!--")
        .trim_end_matches("-->")
        .split('|')
        .map(str::trim)
        .find_map(|field| field.strip_prefix("photo "))
        .map(|name| name.trim().to_string())
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

// ------------------------------------------------------------------- engine

#[derive(Debug, Clone)]
enum Engine {
    Monaddoc,
    /// A `$TURBOMERGER_OCR_CMD`-style template.
    Command(String),
}

impl Engine {
    fn pick(cmd: Option<&str>) -> Engine {
        match cmd
            .map(str::to_string)
            .or_else(|| std::env::var("TURBOMERGER_OCR_CMD").ok())
            .filter(|c| !c.trim().is_empty())
        {
            Some(template) => Engine::Command(template),
            None => Engine::Monaddoc,
        }
    }

    /// The engine's name in cache files (results of another engine, or of
    /// another monaddoc version, are not reused), or why it cannot run.
    fn tag(&self) -> Result<String, String> {
        match self {
            Engine::Command(template) => Ok(format!(
                "cmd-{:016x}",
                twox_hash::XxHash3_64::oneshot(template.as_bytes())
            )),
            Engine::Monaddoc => {
                let tmp =
                    tempfile::tempdir().map_err(|e| format!("OCR: no temporary folder: {e}"))?;
                let mut version = Command::new("monaddoc");
                version.arg("--version");
                let out =
                    child::run(&mut version, tmp.path(), None, |_| {}).map_err(|e| match e {
                        RunError::Spawn(e) if e.kind() == ErrorKind::NotFound => {
                            NOT_FOUND.to_string()
                        }
                        RunError::Spawn(e) => format!("monaddoc did not start: {e}"),
                        RunError::Io(e) => format!("monaddoc: {e}"),
                        RunError::Cancelled => CANCELLED.to_string(),
                    })?;
                // "monaddoc 0.4.0"
                let said = String::from_utf8_lossy(&out.stdout);
                let v: String = said
                    .split_whitespace()
                    .last()
                    .unwrap_or("")
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '.')
                    .collect();
                if !out.status.success() || v.is_empty() {
                    return Err(format!("monaddoc --version failed ({})", out.status));
                }
                Ok(format!("monaddoc-{v}"))
            }
        }
    }
}

// -------------------------------------------------------------------- queue

/// A content hash: the SHA-256 of a picture's (or a PDF's) bytes.
type Key = [u8; 32];

/// Where a queued picture's text will come from once [`OcrQueue::run`] ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ticket(usize);

/// A request's answer: known now (from the cache, or an error), or once the
/// OCR stage ran.
#[derive(Debug)]
pub enum Got {
    /// The text page by page (a picture: one entry; monaddoc splits a photo
    /// of a two-page spread into two).
    Ready(Result<Vec<String>, String>),
    Wait(Ticket),
}

/// One OCR progress report: pages read of all queued, and what is being read
/// (or why nothing is: "waiting for memory …").
#[derive(Debug, Clone, PartialEq)]
pub struct OcrProgress {
    pub done: usize,
    pub total: usize,
    pub current: String,
}

/// The run was cancelled: nothing more is read, and nothing is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OcrCancelled;

struct Item {
    key: Key,
    /// The copy in the staging folder the engine reads.
    staged: PathBuf,
    /// The file it stands for, in progress lines.
    name: String,
    /// 1 for a picture; a whole PDF's page count.
    pages: usize,
    pdf: bool,
    result: Option<Result<Vec<String>, String>>,
}

#[derive(Default)]
struct Inner {
    items: Vec<Item>,
    by_key: HashMap<Key, usize>,
}

/// The pictures a merge must read with OCR. Requests come from the merge's
/// workers (any thread); [`OcrQueue::run`] reads them all after the text.
pub struct OcrQueue {
    engine: Engine,
    tag: OnceLock<Result<String, String>>,
    cache: Option<PathBuf>,
    /// Figure crops are kept beside the output (`--keep-figures`).
    figures: Option<Figures>,
    staging: OnceLock<Result<tempfile::TempDir, String>>,
    folders: AtomicUsize,
    inner: Mutex<Inner>,
    batch_pages: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Copy the file (an original: only read).
    Copy,
    /// Move it (a page picture already in the staging folder).
    Move,
}

/// Each item of a batch: its text page by page, or why there is none.
type BatchResults = Vec<(usize, Result<Vec<String>, String>)>;

/// How one batch ended when it did not finish.
enum Stop {
    Cancelled,
    Finished,
    Failed(String),
}

impl OcrQueue {
    /// `cmd`: the OCR command template; `None` uses `$TURBOMERGER_OCR_CMD`,
    /// else monaddoc.
    pub fn new(cmd: Option<&str>) -> OcrQueue {
        Self::with_figures(cmd, None)
    }

    /// [`OcrQueue::new`]; with `figures_dir`, the figure crops monaddoc cuts
    /// out of pages are kept there and linked from each page's text (only
    /// monaddoc makes crops: a `$TURBOMERGER_OCR_CMD` engine keeps none).
    pub fn with_figures(cmd: Option<&str>, figures_dir: Option<PathBuf>) -> OcrQueue {
        let figures = figures_dir.map(|dir| Figures {
            href: dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "figures".into()),
            dir,
            kept: AtomicUsize::new(0),
        });
        OcrQueue {
            engine: Engine::pick(cmd),
            tag: OnceLock::new(),
            cache: cache_dir(),
            figures,
            staging: OnceLock::new(),
            folders: AtomicUsize::new(0),
            inner: Mutex::new(Inner::default()),
            batch_pages: std::env::var("TURBOMERGER_OCR_BATCH")
                .ok()
                .and_then(|v| v.trim().parse::<usize>().ok())
                .filter(|&n| n > 0)
                .unwrap_or(BATCH_PAGES),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Figure crops kept beside the output so far (fresh and from the cache).
    pub fn figures_kept(&self) -> usize {
        self.figures
            .as_ref()
            .map_or(0, |f| f.kept.load(Ordering::Relaxed))
    }

    /// The figures folder, when figures are kept.
    pub fn figures_dir(&self) -> Option<&Path> {
        self.figures.as_ref().map(|f| f.dir.as_path())
    }

    /// Page texts as the output wants them: figure links name the real folder.
    fn finish(&self, pages: Vec<String>) -> Vec<String> {
        match &self.figures {
            Some(f) => pages
                .into_iter()
                .map(|p| p.replace(ASSETS, &f.href))
                .collect(),
            None => pages,
        }
    }

    /// The engine's cache name, checked once per queue: a missing engine
    /// fails every request at once, before anything is copied.
    fn tag(&self) -> Result<&str, String> {
        self.tag
            .get_or_init(|| self.engine.tag())
            .as_deref()
            .map_err(Clone::clone)
    }

    fn staging(&self) -> Result<&Path, String> {
        self.staging
            .get_or_init(|| {
                tempfile::Builder::new()
                    .prefix("turbomerger-ocr-")
                    .tempdir()
                    .map_err(|e| format!("OCR: no temporary folder: {e}"))
            })
            .as_ref()
            .map(|d| d.path())
            .map_err(Clone::clone)
    }

    /// A new empty folder in the staging area, for one PDF's page pictures
    /// ([`OcrQueue::page_picture`] takes them over).
    pub fn pictures_folder(&self) -> Result<PathBuf, String> {
        self.tag()?;
        let n = self.folders.fetch_add(1, Ordering::Relaxed);
        let dir = self.staging()?.join(format!("pages-{n:06}"));
        std::fs::create_dir_all(&dir).map_err(|e| format!("OCR: no temporary folder: {e}"))?;
        Ok(dir)
    }

    /// Queue a picture file. The original is only read (a copy is staged).
    pub fn picture(&self, path: &Path, name: &str) -> Got {
        self.add(path, name, 1, false, Stage::Copy)
    }

    /// Queue a page picture written into a [`OcrQueue::pictures_folder`]:
    /// moved into the queue, or deleted when the same picture is queued (or
    /// cached) already.
    pub fn page_picture(&self, file: &Path, name: &str) -> Got {
        self.add(file, name, 1, false, Stage::Move)
    }

    /// Queue a whole PDF of `pages` pages (pages whose picture could not be
    /// taken out on its own). The engine reads every page; the caller uses
    /// the ones it needs.
    pub fn whole_pdf(&self, path: &Path, pages: usize, name: &str) -> Got {
        self.add(path, name, pages.max(1), true, Stage::Copy)
    }

    fn add(&self, file: &Path, name: &str, pages: usize, pdf: bool, how: Stage) -> Got {
        let drop_file = || {
            if how == Stage::Move {
                let _ = std::fs::remove_file(file);
            }
        };
        if let Err(e) = self.tag() {
            drop_file();
            return Got::Ready(Err(e));
        }
        let key = match sha256_file(file) {
            Ok(k) => k,
            Err(e) => {
                drop_file();
                return Got::Ready(Err(format!("unreadable: {e}")));
            }
        };
        if let Some(text) = self.cached(&key, pdf) {
            drop_file();
            return Got::Ready(Ok(self.finish(text)));
        }
        let staging = match self.staging() {
            Ok(s) => s.to_path_buf(),
            Err(e) => {
                drop_file();
                return Got::Ready(Err(e));
            }
        };
        let (idx, staged) = {
            let mut inner = self.lock();
            if let Some(&i) = inner.by_key.get(&key) {
                drop(inner);
                drop_file();
                return Got::Wait(Ticket(i));
            }
            let i = inner.items.len();
            let ext = if pdf {
                "pdf".to_string()
            } else {
                file.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.to_ascii_lowercase())
                    .filter(|e| !e.is_empty() && e.chars().all(|c| c.is_ascii_alphanumeric()))
                    .unwrap_or_else(|| "png".into())
            };
            let staged = staging.join(format!("{i:06}.{ext}"));
            inner.items.push(Item {
                key,
                staged: staged.clone(),
                name: name.to_string(),
                pages,
                pdf,
                result: None,
            });
            inner.by_key.insert(key, i);
            (i, staged)
        };
        // Outside the lock: other workers keep queueing while this copies.
        let put = match how {
            Stage::Copy => std::fs::copy(file, &staged).map(|_| ()),
            Stage::Move => {
                std::fs::rename(file, &staged).or_else(|_| std::fs::copy(file, &staged).map(|_| ()))
            }
        };
        if let Err(e) = put {
            self.lock().items[idx].result =
                Some(Err(format!("could not stage the picture for OCR: {e}")));
        }
        Got::Wait(Ticket(idx))
    }

    /// Pages still to read, and what they will take at this machine's last
    /// measured speed (seconds).
    pub fn pending(&self) -> (usize, f64) {
        let pages: usize = self
            .lock()
            .items
            .iter()
            .filter(|i| i.result.is_none())
            .map(|i| i.pages)
            .sum();
        (pages, pages as f64 * self.secs_per_page())
    }

    /// A queued picture's text, page by page, once [`OcrQueue::run`] ran.
    pub fn result(&self, t: Ticket) -> Result<Vec<String>, String> {
        self.lock()
            .items
            .get(t.0)
            .and_then(|i| i.result.clone())
            .map(|r| r.map(|pages| self.finish(pages)))
            .unwrap_or_else(|| Err("OCR did not run".into()))
    }

    /// The OCR stage: read every queued picture, a batch at a time, keeping
    /// each result in the cache as its batch ends. `cancel` stops at once
    /// (`Err`); `finish` (a deadline) stops too, and what was not read is
    /// recorded as [`FINISHED_EARLY`].
    pub fn run(
        &self,
        cancel: &AtomicBool,
        finish: Option<&AtomicBool>,
        progress: &mut dyn FnMut(OcrProgress),
    ) -> Result<(), OcrCancelled> {
        let todo: Vec<(usize, usize)> = self
            .lock()
            .items
            .iter()
            .enumerate()
            .filter(|(_, it)| it.result.is_none())
            .map(|(i, it)| (i, it.pages))
            .collect();
        let total: usize = todo.iter().map(|&(_, p)| p).sum();
        if todo.is_empty() {
            return Ok(());
        }
        let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let mut batches: Vec<Vec<usize>> = Vec::new();
        let mut pages_in = 0;
        for &(i, pages) in &todo {
            if batches.is_empty() || pages_in + pages > self.batch_pages {
                batches.push(Vec::new());
                pages_in = 0;
            }
            batches.last_mut().expect("a batch").push(i);
            pages_in += pages;
        }
        let mut done = 0usize;
        let mut stopped = false;
        for (n, batch) in batches.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                return Err(OcrCancelled);
            }
            if stopped || finish.is_some_and(|f| f.load(Ordering::Relaxed)) {
                stopped = true;
                self.settle(batch, &Err(FINISHED_EARLY.into()));
                continue;
            }
            let pages: usize = batch.iter().map(|&i| self.lock().items[i].pages).sum();
            let started = std::time::Instant::now();
            let outcome = match &self.engine {
                Engine::Monaddoc => {
                    self.run_monaddoc(n, batch, done, total, cancel, finish, progress)
                }
                Engine::Command(t) => {
                    self.run_command(t, batch, done, total, cancel, finish, progress)
                }
            };
            match outcome {
                Ok(results) => {
                    self.store(results);
                    // Batches of a few pages measure mostly the model load.
                    if pages >= 4 {
                        self.measured(started.elapsed().as_secs_f64() / pages as f64);
                    }
                    self.settle(
                        batch,
                        &Err("the OCR engine did not read this picture".into()),
                    );
                }
                Err(Stop::Cancelled) => return Err(OcrCancelled),
                Err(Stop::Finished) => {
                    stopped = true;
                    self.settle(batch, &Err(FINISHED_EARLY.into()));
                }
                Err(Stop::Failed(why)) => self.settle(batch, &Err(why)),
            }
            done += pages;
            progress(OcrProgress {
                done,
                total,
                current: String::new(),
            });
        }
        Ok(())
    }

    fn settle(&self, batch: &[usize], result: &Result<Vec<String>, String>) {
        let mut inner = self.lock();
        for &i in batch {
            if inner.items[i].result.is_none() {
                inner.items[i].result = Some(result.clone());
            }
        }
    }

    /// One monaddoc start for a batch: its pictures move into a folder of
    /// their own, monaddoc reads the folder (its own planner picks workers
    /// and waits for memory), and its Markdown names each page's picture.
    #[allow(clippy::too_many_arguments)]
    fn run_monaddoc(
        &self,
        n: usize,
        batch: &[usize],
        done_before: usize,
        total: usize,
        cancel: &AtomicBool,
        finish: Option<&AtomicBool>,
        progress: &mut dyn FnMut(OcrProgress),
    ) -> Result<BatchResults, Stop> {
        let staging = self.staging().map_err(Stop::Failed)?;
        let base = staging.join(format!("batch-{n:04}"));
        let input = base.join("in");
        std::fs::create_dir_all(&input)
            .map_err(|e| Stop::Failed(format!("OCR: no temporary folder: {e}")))?;
        let mut names: HashMap<String, usize> = HashMap::new();
        let mut ready = Vec::new();
        for &i in batch {
            let (staged, name, failed) = {
                let it = &self.lock().items[i];
                (it.staged.clone(), it.name.clone(), it.result.is_some())
            };
            if failed {
                continue;
            }
            let file = staged.file_name().map(PathBuf::from).unwrap_or_default();
            if std::fs::rename(&staged, input.join(&file)).is_err() {
                continue;
            }
            names.insert(file.to_string_lossy().into_owned(), i);
            ready.push((i, name));
        }
        if ready.is_empty() {
            return Ok(Vec::new());
        }
        let mut command = monaddoc_run(&input, &base.join("work"), &base.join("out"));
        command.arg("--events");
        let stop = AtomicBool::new(false);
        // By item: what the engine said went wrong, and what it did read.
        let mut errors: HashMap<usize, String> = HashMap::new();
        let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
        // Until the first page is read: the engine starts and loads models.
        let mut current = String::from("starting the OCR engine");
        progress(OcrProgress {
            done: done_before,
            total,
            current: current.clone(),
        });
        let label = |file: &str| -> String {
            let Some((i, page)) = staged_index(file) else {
                return file.to_string();
            };
            let name = ready
                .iter()
                .find(|(r, _)| *r == i)
                .map_or(file, |(_, n)| n.as_str());
            match page {
                Some(p) => format!("{name} (page {})", p + 1),
                None => name.to_string(),
            }
        };
        let ran = child::run(&mut command, &base, Some(&stop), |e| match e {
            Event::Line(line) => {
                let Ok(ev) = serde_json::from_str::<serde_json::Value>(line) else {
                    return;
                };
                let text = |k: &str| ev.get(k).and_then(|v| v.as_str()).unwrap_or("");
                match text("kind") {
                    "progress" => {
                        let batch_done =
                            ev.get("done").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                        let status = text("status");
                        if status.starts_with("waiting") || status.starts_with("other programs") {
                            current = format!("OCR {status} — close other programs, or Cancel");
                        } else if current.starts_with("OCR ") {
                            // Memory is free again.
                            current = "reading pictures".into();
                        }
                        progress(OcrProgress {
                            done: done_before + batch_done,
                            total,
                            current: current.clone(),
                        });
                    }
                    "page" => {
                        if let Some((i, _)) = staged_index(text("file")) {
                            seen.insert(i);
                        }
                        current = label(text("file"));
                    }
                    "page_error" => {
                        if let Some((i, _)) = staged_index(text("file")) {
                            errors.insert(i, text("error").to_string());
                        }
                    }
                    _ => {}
                }
            }
            Event::Tick => {
                if cancel.load(Ordering::Relaxed)
                    || finish.is_some_and(|f| f.load(Ordering::Relaxed))
                {
                    stop.store(true, Ordering::Relaxed);
                }
            }
        });
        let output = match ran {
            Ok(o) => o,
            Err(RunError::Cancelled) => {
                let _ = std::fs::remove_dir_all(&base);
                return Err(if cancel.load(Ordering::Relaxed) {
                    Stop::Cancelled
                } else {
                    Stop::Finished
                });
            }
            Err(RunError::Spawn(e)) if e.kind() == ErrorKind::NotFound => {
                return Err(Stop::Failed(NOT_FOUND.into()))
            }
            Err(RunError::Spawn(e)) => {
                return Err(Stop::Failed(format!("monaddoc did not start: {e}")))
            }
            Err(RunError::Io(e)) => return Err(Stop::Failed(format!("monaddoc: {e}"))),
        };
        if !output.status.success() {
            let _ = std::fs::remove_dir_all(&base);
            return Err(Stop::Failed(monaddoc_failure(&output)));
        }
        let md = match std::fs::read_to_string(base.join("out").join("ocr_3_structured.md")) {
            Ok(md) => md,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&base);
                return Err(Stop::Failed(format!("monaddoc wrote no Markdown: {e}")));
            }
        };
        // Each page names its picture: `000012.jpg`, or `000013__p0004.png`
        // for page 4 of a whole PDF. A photo of a two-page spread is two pages.
        let crops = base.join("out").join("ocr_assets");
        let mut texts: HashMap<usize, Vec<String>> = HashMap::new();
        for (photo, text) in split_photos_opts(&md, self.figures.is_some()) {
            let Some((i, page)) = staged_index(&photo) else {
                continue;
            };
            let Some(&item) = names.get(&staged_name(&photo, i)) else {
                continue;
            };
            let pages = texts.entry(item).or_default();
            let text = match &self.figures {
                Some(f) => {
                    let key = self.lock().items[item].key;
                    keep_figures(f, &crops, &hex12(&key), page.unwrap_or(pages.len()), &text)
                }
                None => text,
            };
            match page {
                Some(p) => {
                    if pages.len() <= p {
                        pages.resize(p + 1, String::new());
                    }
                    if pages[p].is_empty() {
                        pages[p] = text;
                    } else {
                        pages[p] = format!("{}\n\n{text}", pages[p]);
                    }
                }
                None => pages.push(text),
            }
        }
        let _ = std::fs::remove_dir_all(&base);
        let mut out = Vec::with_capacity(ready.len());
        for (i, _) in ready {
            let pdf_pages = {
                let it = &self.lock().items[i];
                it.pdf.then_some(it.pages)
            };
            let result = match (texts.remove(&i), errors.remove(&i)) {
                (Some(mut pages), _) => {
                    if let Some(n) = pdf_pages {
                        pages.resize(n.max(pages.len()), String::new());
                    }
                    Ok(pages)
                }
                (None, Some(e)) => Err(format!("OCR failed: {e}")),
                // Read, and nothing found: an empty page is an answer too
                // (and is kept, so the next run does not read it again).
                (None, None) if seen.contains(&i) => {
                    Ok(vec![String::new(); pdf_pages.unwrap_or(1)])
                }
                // Never read (a format the engine skips): not an answer.
                (None, None) => Err("the OCR engine did not read this picture".into()),
            };
            out.push((i, result));
        }
        Ok(out)
    }

    /// A `$TURBOMERGER_OCR_CMD` engine reads one file per start.
    #[allow(clippy::too_many_arguments)]
    fn run_command(
        &self,
        template: &str,
        batch: &[usize],
        done_before: usize,
        total: usize,
        cancel: &AtomicBool,
        finish: Option<&AtomicBool>,
        progress: &mut dyn FnMut(OcrProgress),
    ) -> Result<BatchResults, Stop> {
        let mut out = Vec::with_capacity(batch.len());
        let mut done = done_before;
        let stop = AtomicBool::new(false);
        for &i in batch {
            let (staged, name, pages, failed) = {
                let it = &self.lock().items[i];
                (
                    it.staged.clone(),
                    it.name.clone(),
                    it.pages,
                    it.result.is_some(),
                )
            };
            if failed {
                continue;
            }
            progress(OcrProgress {
                done,
                total,
                current: name,
            });
            let mut tick = || {
                if cancel.load(Ordering::Relaxed)
                    || finish.is_some_and(|f| f.load(Ordering::Relaxed))
                {
                    stop.store(true, Ordering::Relaxed);
                }
            };
            tick();
            let result = custom_command(template, &staged, Some(&stop), &mut tick);
            let _ = std::fs::remove_file(&staged);
            if stop.load(Ordering::Relaxed) {
                // What finished before the stop is kept.
                self.store(out);
                return Err(if cancel.load(Ordering::Relaxed) {
                    Stop::Cancelled
                } else {
                    Stop::Finished
                });
            }
            out.push((i, result.map(|t| vec![t])));
            done += pages;
        }
        Ok(out)
    }

    fn store(&self, results: BatchResults) {
        for (i, result) in results {
            if let Ok(text) = &result {
                let (key, pdf) = {
                    let it = &self.lock().items[i];
                    (it.key, it.pdf)
                };
                self.remember(&key, pdf, text);
            }
            self.lock().items[i].result = Some(result);
        }
    }

    /// With figures kept, an entry of its own (`-fig`): its texts carry the
    /// links, and its crops sit in a folder beside it.
    fn cache_file(&self, key: &Key, pdf: bool) -> Option<PathBuf> {
        let tag = self.tag().ok()?;
        let dir = self.cache.as_ref()?;
        let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
        let fig = if self.figures.is_some() { "-fig" } else { "" };
        let kind = if pdf { "pdf.json" } else { "json" };
        Some(dir.join(format!("{hex}-{tag}{fig}.{kind}")))
    }

    /// The crops of a cached picture: `<entry>.assets/`.
    fn cache_assets(&self, key: &Key, pdf: bool) -> Option<PathBuf> {
        self.figures.as_ref()?;
        Some(self.cache_file(key, pdf)?.with_extension("assets"))
    }

    /// The names of the crops a page text links.
    fn linked_crops(pages: &[String]) -> Vec<String> {
        let mut names = Vec::new();
        for p in pages {
            for part in p.split(ASSETS).skip(1) {
                if let Some(name) = part.strip_prefix('/').and_then(|r| r.split(')').next()) {
                    names.push(name.to_string());
                }
            }
        }
        names
    }

    /// A cached reading; with figures kept, its crops are copied back into
    /// the figures folder first (a crop the cache lost makes the entry a miss).
    fn cached(&self, key: &Key, pdf: bool) -> Option<Vec<String>> {
        let text = std::fs::read_to_string(self.cache_file(key, pdf)?).ok()?;
        let pages: Vec<String> = serde_json::from_str(&text).ok()?;
        if let Some(f) = &self.figures {
            let names = Self::linked_crops(&pages);
            if !names.is_empty() {
                let assets = self.cache_assets(key, pdf)?;
                std::fs::create_dir_all(&f.dir).ok()?;
                for name in &names {
                    std::fs::copy(assets.join(name), f.dir.join(name)).ok()?;
                }
                f.kept.fetch_add(names.len(), Ordering::Relaxed);
            }
        }
        Some(pages)
    }

    /// Best effort: a result that cannot be kept only costs time next run.
    /// Written beside its name, then renamed, so a reader never sees half.
    fn remember(&self, key: &Key, pdf: bool, pages: &[String]) {
        let Some(file) = self.cache_file(key, pdf) else {
            return;
        };
        let Some(dir) = file.parent() else { return };
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        let Ok(body) = serde_json::to_string(pages) else {
            return;
        };
        // Crops first: a reader of the entry must find every crop it links.
        if let (Some(f), Some(assets)) = (&self.figures, self.cache_assets(key, pdf)) {
            let names = Self::linked_crops(pages);
            if !names.is_empty() && std::fs::create_dir_all(&assets).is_err() {
                return;
            }
            for name in &names {
                if std::fs::copy(f.dir.join(name), assets.join(name)).is_err() {
                    return;
                }
            }
        }
        let tmp = file.with_extension(format!("tmp{}", std::process::id()));
        if std::fs::write(&tmp, body).is_err() || std::fs::rename(&tmp, &file).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    fn speed_file(&self) -> Option<PathBuf> {
        let tag = self.tag().ok()?;
        Some(self.cache.as_ref()?.join(format!("{tag}.seconds-per-page")))
    }

    /// Seconds per page on this machine, last measured (or a default).
    pub fn secs_per_page(&self) -> f64 {
        self.speed_file()
            .and_then(|f| std::fs::read_to_string(f).ok())
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|s| s.is_finite() && *s > 0.0)
            .unwrap_or(DEFAULT_SECS_PER_PAGE)
    }

    fn measured(&self, secs: f64) {
        if let (Some(f), true) = (self.speed_file(), secs.is_finite() && secs > 0.0) {
            let _ = std::fs::write(f, format!("{secs:.2}"));
        }
    }
}

/// `000012.jpg` → (12, None); `000013__p0004.png` → (13, Some(3)).
fn staged_index(photo: &str) -> Option<(usize, Option<usize>)> {
    let stem = photo.split('.').next()?;
    match stem.split_once("__p") {
        Some((i, p)) => Some((
            i.parse().ok()?,
            Some(p.parse::<usize>().ok()?.checked_sub(1)?),
        )),
        None => Some((stem.parse().ok()?, None)),
    }
}

/// The staged file a page came from: the photo itself, or the PDF for a
/// rendered PDF page.
fn staged_name(photo: &str, i: usize) -> String {
    if photo.contains("__p") {
        format!("{i:06}.pdf")
    } else {
        photo.to_string()
    }
}

/// `TURBOMERGER_OCR_CACHE`: a folder, or `off`; else
/// `<user cache>/turbomerger/ocr-v1`.
fn cache_dir() -> Option<PathBuf> {
    match std::env::var("TURBOMERGER_OCR_CACHE") {
        Ok(v) if v.trim().eq_ignore_ascii_case("off") => None,
        Ok(v) if !v.trim().is_empty() => Some(PathBuf::from(v.trim())),
        _ => dirs::cache_dir().map(|d| d.join("turbomerger").join("ocr-v1")),
    }
}

fn sha256_file(path: &Path) -> std::io::Result<Key> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
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
    fn figure_links_are_kept_renamed_and_copied_only_when_asked() {
        let md = "# ocr\n\n<a id=\"page-000001\"></a>\n<!-- page ? | PDF page 1 | photo 000000.png -->\n\n\
                  ![Figure 1 on page 1](ocr_assets/p001-1.webp)\n\nText\n\n\
                  ![Figure 2 on page 1](ocr_assets/p001-2.webp)\n";
        assert_eq!(split_photos(md)[0].1, "Text");
        let kept = split_photos_opts(md, true);
        assert!(
            kept[0]
                .1
                .starts_with("![Figure 1 on page 1](ocr_assets/p001-1.webp)"),
            "{kept:?}"
        );
        let tmp = tempfile::tempdir().unwrap();
        let crops = tmp.path().join("ocr_assets");
        std::fs::create_dir_all(&crops).unwrap();
        std::fs::write(crops.join("p001-1.webp"), b"RIFF").unwrap();
        let figures = Figures {
            dir: tmp.path().join("one_assets"),
            href: "one_assets".into(),
            kept: AtomicUsize::new(0),
        };
        let text = keep_figures(&figures, &crops, "0123456789ab", 0, &kept[0].1);
        // The first crop is copied under the picture's name; the second has no
        // crop file, so its link goes rather than dangle.
        assert_eq!(
            text,
            format!("![Figure 1 on page 1]({ASSETS}/0123456789ab-0-1.webp)\n\nText")
        );
        assert_eq!(figures.kept.load(Ordering::Relaxed), 1);
        assert_eq!(
            std::fs::read(tmp.path().join("one_assets/0123456789ab-0-1.webp")).unwrap(),
            b"RIFF"
        );
        assert_eq!(
            OcrQueue::linked_crops(&[text]),
            vec!["0123456789ab-0-1.webp".to_string()]
        );
    }

    #[test]
    fn each_page_names_its_photo() {
        // The format of a real batch run (monaddoc 0.4.0, three inputs, one a PDF).
        let md = "# ocr\n\n<!-- MonadDoc 0.4.0 structured Markdown: … -->\n\n\
                  <a id=\"page-000001\"></a>\n<!-- page ? | PDF page 1 | photo 000000.png -->\n\n\
                  ### Marmalade Zeppelin\n\nphotographed residency letter\n\n\
                  <a id=\"page-000002\"></a>\n<!-- page ? | PDF page 2 | photo 000001__p0001.png -->\n\n\
                  JUARIZ BUFFALC\n";
        let pages = split_photos(md);
        assert_eq!(
            pages,
            vec![
                (
                    "000000.png".to_string(),
                    "### Marmalade Zeppelin\n\nphotographed residency letter".to_string()
                ),
                (
                    "000001__p0001.png".to_string(),
                    "JUARIZ BUFFALC".to_string()
                ),
            ]
        );
        assert_eq!(staged_index("000000.png"), Some((0, None)));
        assert_eq!(staged_index("000001__p0001.png"), Some((1, Some(0))));
        assert_eq!(staged_index("000001__p0000.png"), None);
        assert_eq!(staged_index("cover.jpg"), None);
        assert_eq!(staged_name("000001__p0001.png", 1), "000001.pdf");
    }

    #[test]
    fn custom_command_substitutes_input() {
        // `echo` stands in for an OCR engine on Unix.
        #[cfg(unix)]
        {
            let t = custom_command(
                "echo got {input}",
                Path::new("/x/scan.png"),
                None,
                &mut || {},
            )
            .unwrap();
            assert_eq!(t.trim(), "got /x/scan.png");
            let t =
                custom_command("echo appended", Path::new("/x/a.png"), None, &mut || {}).unwrap();
            assert_eq!(t.trim(), "appended /x/a.png");
        }
    }

    /// Cancel stops an engine in the middle of a picture (it would take 30 s),
    /// and a call still waiting for its turn starts nothing. `{input}` is
    /// one argument, so the "file" here is the shell script.
    #[cfg(unix)]
    #[test]
    fn cancel_stops_an_engine_mid_picture() {
        use std::time::{Duration, Instant};
        let cancel = AtomicBool::new(false);
        let started = Instant::now();
        let err = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(300));
                cancel.store(true, Ordering::Relaxed);
            });
            ocr_pages(Path::new("sleep 30"), Some("sh -c {input}"), Some(&cancel)).unwrap_err()
        });
        assert_eq!(err, CANCELLED);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        let err =
            ocr_pages(Path::new("sleep 30"), Some("sh -c {input}"), Some(&cancel)).unwrap_err();
        assert_eq!(err, CANCELLED);
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_command_names_its_last_error_line() {
        let script = Path::new("echo noise >&2; echo bad page >&2; exit 3");
        let err = custom_command("sh -c {input}", script, None, &mut || {}).unwrap_err();
        assert!(err.starts_with("OCR command 'sh' failed ("), "{err}");
        assert!(err.ends_with("): bad page"), "{err}");
    }
}
