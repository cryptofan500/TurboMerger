//! Progress on stderr while a merge runs (plan §10): a status line every
//! 5 s on a terminal, or JSON lines for scripts; stall reports; the
//! deadline. Runs on its own thread and never touches stdout, which holds
//! only the final summary.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tm_core::job::Stage;
use tm_core::progress::{human_duration, Snapshot, Tracker};
use tm_core::CancelToken;

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgressMode {
    /// A status line every 5 s when stderr is a terminal; nothing otherwise
    Auto,
    /// One JSON object per line on stderr (progress every second, stalls, deadline)
    Json,
    /// No progress output
    None,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeadlineAction {
    /// Stop and write nothing (exit 4)
    Cancel,
    /// Stop taking new files, write what is done, report the rest (exit 4)
    Partial,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum StallAction {
    /// Report the stall and keep waiting
    Wait,
    /// Report the stall and cancel (exit 4)
    Fail,
}

pub struct ReporterConfig {
    pub mode: ProgressMode,
    pub quiet: bool,
    pub deadline: Option<Duration>,
    pub on_deadline: DeadlineAction,
    pub on_stall: StallAction,
}

pub struct Reporter {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    /// Why the run was stopped early, if the reporter stopped it.
    pub stopped_by: Arc<std::sync::Mutex<Option<&'static str>>>,
}

fn stage_word(s: Stage) -> &'static str {
    match s {
        Stage::Scan => "scan",
        Stage::Classify => "classify",
        Stage::Merge => "merge",
    }
}

fn json_line(event: &str, s: &Snapshot) -> String {
    let mut v = serde_json_lite(event, s);
    v.push('\n');
    v
}

/// A small hand-written JSON object (the CLI does not need serde_json).
fn serde_json_lite(event: &str, s: &Snapshot) -> String {
    let esc = |t: &str| -> String {
        let mut o = String::with_capacity(t.len() + 2);
        o.push('"');
        for c in t.chars() {
            match c {
                '"' => o.push_str("\\\""),
                '\\' => o.push_str("\\\\"),
                c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
                c => o.push(c),
            }
        }
        o.push('"');
        o
    };
    let ms = |d: Option<Duration>| d.map_or("null".to_string(), |d| d.as_millis().to_string());
    format!(
        "{{\"event\":{},\"stage\":{},\"done\":{},\"total\":{},\"current\":{},\"elapsed_ms\":{},\"eta_ms\":{},\"eta_band_ms\":{},\"stalled_ms\":{}}}",
        esc(event),
        esc(stage_word(s.stage)),
        s.done,
        s.total,
        esc(&s.current),
        s.elapsed.as_millis(),
        ms(s.eta),
        ms(s.eta_band),
        ms(s.stalled_for)
    )
}

fn status_line(s: &Snapshot) -> String {
    let mut line = format!("[{}] ", stage_word(s.stage));
    if s.total > 0 {
        line.push_str(&format!("{}/{} files", s.done, s.total));
    } else {
        line.push_str(&format!("{} entries", s.done));
    }
    line.push_str(&format!(" · {}", human_duration(s.elapsed)));
    if let Some(eta) = s.eta {
        line.push_str(&format!(" · ETA {}", human_duration(eta)));
        if let Some(b) = s.eta_band.filter(|b| b.as_secs() >= 1) {
            line.push_str(&format!(" ± {}", human_duration(b)));
        }
    }
    if !s.current.is_empty() {
        let tail: String = s
            .current
            .chars()
            .rev()
            .take(48)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        line.push_str(&format!(" · {}", tail));
    }
    line
}

impl Reporter {
    pub fn start(tracker: Arc<Tracker>, token: CancelToken, cfg: ReporterConfig) -> Reporter {
        use std::io::IsTerminal;
        let stop = Arc::new(AtomicBool::new(false));
        let stopped_by = Arc::new(std::sync::Mutex::new(None));
        let (stop2, stopped2) = (stop.clone(), stopped_by.clone());
        let tty = std::io::stderr().is_terminal();
        let handle = std::thread::spawn(move || {
            let started = Instant::now();
            let mut last_status = started;
            let mut last_json = started;
            let mut stall_reported = false;
            let mut deadline_fired = false;
            let mut drew_status = false;
            let mut err = std::io::stderr();
            while !stop2.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(200));
                let snap = tracker.snapshot();
                if let Some(d) = cfg
                    .deadline
                    .filter(|d| !deadline_fired && snap.elapsed >= *d)
                {
                    deadline_fired = true;
                    match cfg.on_deadline {
                        DeadlineAction::Cancel => token.cancel(),
                        DeadlineAction::Partial => token.finish_early(),
                    }
                    *stopped2.lock().unwrap() = Some("deadline");
                    let msg = format!(
                        "deadline of {} reached — {}",
                        human_duration(d),
                        match cfg.on_deadline {
                            DeadlineAction::Cancel => "cancelling",
                            DeadlineAction::Partial => "finishing with what is done",
                        }
                    );
                    if cfg.mode == ProgressMode::Json {
                        let _ = err.write_all(json_line("deadline", &snap).as_bytes());
                    } else if !cfg.quiet {
                        if drew_status {
                            let _ = write!(err, "\r\x1b[K");
                        }
                        let _ = writeln!(err, "{}", msg);
                    }
                }
                match snap.stalled_for {
                    Some(for_) if !stall_reported => {
                        stall_reported = true;
                        if cfg.mode == ProgressMode::Json {
                            let _ = err.write_all(json_line("stall", &snap).as_bytes());
                        } else if !cfg.quiet || cfg.on_stall == StallAction::Fail {
                            if drew_status {
                                let _ = write!(err, "\r\x1b[K");
                                drew_status = false;
                            }
                            let _ = writeln!(
                                err,
                                "stalled on {} for {}",
                                if snap.current.is_empty() {
                                    "(scan)"
                                } else {
                                    &snap.current
                                },
                                human_duration(for_)
                            );
                        }
                        if cfg.on_stall == StallAction::Fail {
                            token.cancel();
                            *stopped2.lock().unwrap() = Some("stall");
                        }
                    }
                    None => stall_reported = false,
                    _ => {}
                }
                match cfg.mode {
                    ProgressMode::Json if last_json.elapsed() >= Duration::from_secs(1) => {
                        last_json = Instant::now();
                        let _ = err.write_all(json_line("progress", &snap).as_bytes());
                    }
                    ProgressMode::Auto
                        if tty && !cfg.quiet && last_status.elapsed() >= Duration::from_secs(5) =>
                    {
                        last_status = Instant::now();
                        let _ = write!(err, "\r\x1b[K{}", status_line(&snap));
                        let _ = err.flush();
                        drew_status = true;
                    }
                    _ => {}
                }
            }
            if drew_status {
                let _ = write!(err, "\r\x1b[K");
                let _ = err.flush();
            }
        });
        Reporter {
            stop,
            handle: Some(handle),
            stopped_by,
        }
    }

    pub fn finish(mut self) -> Option<&'static str> {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        *self.stopped_by.lock().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_lines_are_valid_objects() {
        let s = Snapshot {
            stage: Stage::Merge,
            done: 3,
            total: 10,
            current: "a \"b\"\\c\n.rs".into(),
            elapsed: Duration::from_millis(1500),
            eta: Some(Duration::from_secs(7)),
            eta_band: None,
            stalled_for: None,
        };
        let line = json_line("progress", &s);
        assert!(line.ends_with("}\n"));
        assert!(
            line.contains(r#""current":"a \"b\"\\c\u000a.rs""#),
            "{line}"
        );
        assert!(
            line.contains(r#""eta_ms":7000,"eta_band_ms":null"#),
            "{line}"
        );
    }

    #[test]
    fn status_line_shows_eta_and_file() {
        let s = Snapshot {
            stage: Stage::Merge,
            done: 3,
            total: 10,
            current: "src/main.rs".into(),
            elapsed: Duration::from_secs(65),
            eta: Some(Duration::from_secs(30)),
            eta_band: Some(Duration::from_secs(4)),
            stalled_for: None,
        };
        assert_eq!(
            status_line(&s),
            "[merge] 3/10 files · 1m05s · ETA 30s ± 4s · src/main.rs"
        );
    }
}
