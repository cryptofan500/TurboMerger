//! Big PDFs parse in a worker process (P4). `pdf_oxide` holds about 6.6 x
//! the file in memory, so a 450 MB scan peaked at 2.84 GiB, and inside the
//! merge an out-of-memory kill (or, in a `panic = "abort"` release, a parser
//! panic) ended the whole run with nothing written. Here the parse runs in
//! `<this program> __worker pdf`, a `tm-worker` protocol process with a
//! memory cap: when it dies, only that file is "not captured".
//!
//! Both sides live here: [`register`] (the merge's side) and [`serve`] (the
//! worker's). The desktop app's binary forwards `__worker` to this CLI, so
//! it can be its own worker too; nothing extra ships.

use std::io::BufRead;
use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use tm_core::extract::PdfParse;
use tm_core::CancelToken;
use tm_worker::{send, Limits, ToHost, ToWorker, Worker, WorkerError, PROTOCOL};

/// Send PDFs over `tm_core::extract::pdf_worker_above()` to a worker running
/// this same program. Without it (or when no worker starts) they are parsed
/// in-process, as before.
pub fn register() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    tm_core::extract::set_pdf_offload(Box::new(move |path, pictures| {
        parse_in_worker(&exe, path, pictures)
    }));
}

/// The worker's parse of `path`; with `pictures`, the worker also writes
/// the picture of each page without text into that folder (for OCR).
fn parse_in_worker(
    exe: &Path,
    path: &Path,
    pictures: Option<&Path>,
) -> Option<Result<PdfParse, String>> {
    // The protocol is JSON: a path that is not UTF-8 is parsed in-process.
    let path_text = path.to_str()?;
    let pictures_text = match pictures {
        Some(dir) => Some(dir.to_str()?),
        None => None,
    };
    let mb = std::fs::metadata(path).map_or(0, |m| m.len() >> 20);
    let budget = memory_budget();
    let limits = Limits {
        // One item and no progress lines: the budget covers the whole parse.
        item_budget: Duration::from_secs(60 + mb),
        // Linux: the host watches resident memory; elsewhere the OS caps
        // the address space (the only limit there is).
        resident_bytes: budget.filter(|_| cfg!(target_os = "linux")),
        memory_bytes: budget.filter(|_| !cfg!(target_os = "linux")),
        ..Limits::default()
    };
    let mut worker = match Worker::spawn(exe, &["__worker", "pdf"], limits) {
        Ok(w) => w,
        // Not a worker-capable program, or it cannot start at all.
        Err(WorkerError::Spawn(_) | WorkerError::Protocol(_)) => return None,
        Err(e) => return Some(Err(lost(mb, budget, &e))),
    };
    let job = serde_json::json!({ "path": path_text, "pictures": pictures_text });
    let pages = match worker.run("pdf_pages", job, &CancelToken::new(), &mut |_| {}) {
        Ok(out) => serde_json::from_value::<PdfParse>(out)
            .map_err(|e| format!("PDF worker sent unreadable pages: {e}")),
        // The parser's own reason ("PDF parse failed: …"), as in-process;
        // an allocation the address-space cap refused also says which.
        Err(WorkerError::Failed(reason)) if reason.contains("out of memory") => {
            Err(format!("{reason} ({})", context(mb, budget)))
        }
        Err(WorkerError::Failed(reason)) => Err(reason),
        Err(e) => Err(lost(mb, budget, &e)),
    };
    worker.shutdown();
    Some(pages)
}

/// The report's reason when the worker died or was stopped.
fn lost(mb: u64, budget: Option<u64>, e: &WorkerError) -> String {
    format!("PDF parser stopped ({}): {e}", context(mb, budget))
}

fn context(mb: u64, budget: Option<u64>) -> String {
    let budget = budget.map_or(String::new(), |b| format!(", memory budget {} MB", b >> 20));
    format!(
        "isolated worker, {mb} MB file{budget}; parsing needs about 7 x the file size in memory"
    )
}

/// How much memory the worker may use: `TURBOMERGER_PDF_WORKER_MB` (0 = no
/// limit), else 80 % of the memory available right now (Linux: also inside
/// this process's cgroup), else 60 % of physical memory, else none.
///
/// Below what is available, so the worker is stopped before the system runs
/// out: an OOM kill would also make systemd stop the whole scope it happened
/// in (`OOMPolicy=stop`), the merge included.
fn memory_budget() -> Option<u64> {
    let set = std::env::var("TURBOMERGER_PDF_WORKER_MB")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok());
    match set {
        Some(0) => None,
        Some(mb) => Some(mb << 20),
        None => available_memory()
            .map(|b| (b / 10 * 8).max(256 << 20))
            .or_else(|| physical_memory().map(|b| b / 10 * 6)),
    }
}

/// Bytes this process could still use: `MemAvailable`, lowered by every
/// cgroup v2 limit on the way up from this process's cgroup (its headroom
/// plus its inactive page cache, which the kernel reclaims first: a merge
/// that has read gigabytes of files is charged for their cache).
#[cfg(target_os = "linux")]
fn available_memory() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let mut avail = meminfo
        .lines()
        .find_map(|l| l.strip_prefix("MemAvailable:"))?
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse::<u64>()
        .ok()?
        << 10;
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
    if let Some(rel) = cgroup.lines().find_map(|l| l.strip_prefix("0::")) {
        let mut dir = Path::new("/sys/fs/cgroup").join(rel.trim().trim_start_matches('/'));
        loop {
            let read = |f: &str| std::fs::read_to_string(dir.join(f)).ok();
            if let (Some(max), Some(cur)) = (read("memory.max"), read("memory.current")) {
                if let (Ok(max), Ok(cur)) = (max.trim().parse::<u64>(), cur.trim().parse::<u64>()) {
                    let cache = read("memory.stat")
                        .and_then(|s| {
                            s.lines()
                                .find_map(|l| l.strip_prefix("inactive_file "))
                                .and_then(|v| v.trim().parse::<u64>().ok())
                        })
                        .unwrap_or(0);
                    avail = avail.min(max.saturating_sub(cur.saturating_sub(cache)));
                }
            }
            if dir == Path::new("/sys/fs/cgroup") || !dir.pop() {
                break;
            }
        }
    }
    Some(avail)
}

#[cfg(not(target_os = "linux"))]
fn available_memory() -> Option<u64> {
    None
}

#[cfg(unix)]
fn physical_memory() -> Option<u64> {
    // SAFETY: sysconf only reads two system constants.
    let (pages, size) = unsafe {
        (
            libc::sysconf(libc::_SC_PHYS_PAGES),
            libc::sysconf(libc::_SC_PAGESIZE),
        )
    };
    (pages > 0 && size > 0).then(|| pages as u64 * size as u64)
}

#[cfg(not(unix))]
fn physical_memory() -> Option<u64> {
    None
}

/// `__worker pdf`: parse the PDFs the host sends, one job at a time, until
/// it says shutdown or closes stdin. stdout carries only protocol lines.
pub fn serve(kind: &str) -> i32 {
    if kind != "pdf" {
        eprintln!("error: unknown worker kind {kind:?} (known: pdf)");
        return crate::EXIT_USAGE;
    }
    expendable();
    send(&ToHost::Hello {
        protocol: PROTOCOL,
        worker: "tm-pdf".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        capabilities: vec!["pdf_pages".into()],
    });
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        match serde_json::from_str::<ToWorker>(&line) {
            Ok(ToWorker::Job { id, kind, input }) => {
                let path = input.get("path").and_then(Value::as_str);
                let pictures = input.get("pictures").and_then(Value::as_str).map(Path::new);
                let reply = match (kind.as_str(), path) {
                    ("pdf_pages", Some(p)) => {
                        match tm_core::extract::pdf_parse(Path::new(p), pictures) {
                            Ok(parsed) => ToHost::Result {
                                id,
                                output: serde_json::to_value(parsed).unwrap_or_default(),
                            },
                            Err(message) => ToHost::Error { id, message },
                        }
                    }
                    _ => ToHost::Error {
                        id,
                        message: format!("unsupported job {kind:?}"),
                    },
                };
                send(&reply);
            }
            Ok(ToWorker::Shutdown) | Err(_) => break,
        }
    }
    crate::EXIT_OK
}

/// A worker is expendable: low priority like the merge's own workers (a
/// Windows child does not inherit it), the Linux OOM killer's first pick,
/// and gone when the merge that started it is.
fn expendable() {
    tm_core::threads::lower_current_thread_priority();
    #[cfg(target_os = "linux")]
    {
        let _ = std::fs::write("/proc/self/oom_score_adj", "1000");
        // SAFETY: prctl with integer arguments, on this process only.
        unsafe {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
        }
    }
}
