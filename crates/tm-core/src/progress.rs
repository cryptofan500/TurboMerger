//! Progress, ETA and stall detection, shared by every shell (plan §10).
//!
//! - ETA = remaining work ÷ EWMA(throughput), sampled on ≥ 1 s ticks
//!   (α = 0.2), with a ± band from the EWMA variance. It is withheld until
//!   `eta_after` has elapsed: early rates mislead (plan §10.3, `--eta-after`).
//! - A stall is no progress (no new item, no new current file) for
//!   `stall_after`; the snapshot says what the job is stuck on and for how
//!   long (plan §10.4).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::job::{Progress, Stage};

const ALPHA: f64 = 0.2;
const TICK: Duration = Duration::from_secs(1);

/// A point-in-time view of a running job.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub stage: Stage,
    pub done: usize,
    /// 0 when not known yet.
    pub total: usize,
    pub current: String,
    pub elapsed: Duration,
    /// Time left, once `eta_after` has passed and the rate is known.
    pub eta: Option<Duration>,
    /// Uncertainty of `eta` (one standard deviation of the rate).
    pub eta_band: Option<Duration>,
    /// Set when nothing moved for `stall_after`.
    pub stalled_for: Option<Duration>,
}

struct State {
    started: Instant,
    last_change: Instant,
    stage: Stage,
    done: usize,
    total: usize,
    current: String,
    /// Items per second (EWMA) and its variance.
    rate: Option<f64>,
    var: f64,
    sample_at: Instant,
    sample_done: usize,
}

pub struct Tracker {
    state: Mutex<State>,
    eta_after: Duration,
    stall_after: Duration,
}

impl Tracker {
    pub fn new(eta_after: Duration, stall_after: Duration) -> Tracker {
        Tracker::starting_at(Instant::now(), eta_after, stall_after)
    }

    pub fn starting_at(now: Instant, eta_after: Duration, stall_after: Duration) -> Tracker {
        Tracker {
            state: Mutex::new(State {
                started: now,
                last_change: now,
                stage: Stage::Scan,
                done: 0,
                total: 0,
                current: String::new(),
                rate: None,
                var: 0.0,
                sample_at: now,
                sample_done: 0,
            }),
            eta_after,
            stall_after,
        }
    }

    pub fn update(&self, p: &Progress) {
        self.update_at(p, Instant::now());
    }

    pub fn update_at(&self, p: &Progress, now: Instant) {
        let mut s = self.state.lock().expect("tracker");
        if p.stage != s.stage {
            // New units of work: the old rate says nothing about them.
            s.stage = p.stage;
            s.rate = None;
            s.var = 0.0;
            s.sample_at = now;
            s.sample_done = p.done;
            s.last_change = now;
        } else if p.done != s.done || p.current != s.current {
            s.last_change = now;
        }
        s.done = p.done;
        s.total = p.total;
        s.current.clone_from(&p.current);
        let dt = now.saturating_duration_since(s.sample_at);
        if dt >= TICK {
            let inst = p.done.saturating_sub(s.sample_done) as f64 / dt.as_secs_f64();
            match s.rate {
                None => s.rate = Some(inst),
                Some(r) => {
                    let next = ALPHA * inst + (1.0 - ALPHA) * r;
                    s.var = ALPHA * (inst - next).powi(2) + (1.0 - ALPHA) * s.var;
                    s.rate = Some(next);
                }
            }
            s.sample_at = now;
            s.sample_done = p.done;
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot_at(Instant::now())
    }

    pub fn snapshot_at(&self, now: Instant) -> Snapshot {
        let s = self.state.lock().expect("tracker");
        let elapsed = now.saturating_duration_since(s.started);
        let (eta, eta_band) = match s.rate {
            Some(rate) if rate > 0.0 && s.total > 0 && elapsed >= self.eta_after => {
                let left = s.total.saturating_sub(s.done) as f64;
                let secs = left / rate;
                let band = secs * (s.var.sqrt() / rate).min(1.0);
                (
                    Some(Duration::from_secs_f64(secs)),
                    Some(Duration::from_secs_f64(band)),
                )
            }
            _ => (None, None),
        };
        let quiet = now.saturating_duration_since(s.last_change);
        Snapshot {
            stage: s.stage,
            done: s.done,
            total: s.total,
            current: s.current.clone(),
            elapsed,
            eta,
            eta_band,
            stalled_for: (quiet >= self.stall_after).then_some(quiet),
        }
    }
}

/// `1m05s`, `42s`, `1h02m`.
pub fn human_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}s", s)
    }
}

/// Parse `90`, `90s`, `10m`, `1h`, `1h30m` into a duration.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let t = text.trim();
    if t.is_empty() {
        return Err("empty duration".into());
    }
    let mut total = 0u64;
    let mut num = String::new();
    let mut saw_unit = false;
    for c in t.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let n: u64 = num
            .parse()
            .map_err(|_| format!("bad duration {:?} (examples: 90s, 10m, 1h30m)", text))?;
        num.clear();
        total += match c {
            's' => n,
            'm' => n * 60,
            'h' => n * 3600,
            _ => {
                return Err(format!(
                    "bad duration {:?} (examples: 90s, 10m, 1h30m)",
                    text
                ))
            }
        };
        saw_unit = true;
    }
    if !num.is_empty() {
        let n: u64 = num
            .parse()
            .map_err(|_| format!("bad duration {:?}", text))?;
        total += n; // bare number = seconds
    } else if !saw_unit {
        return Err(format!("bad duration {:?}", text));
    }
    Ok(Duration::from_secs(total))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(stage: Stage, done: usize, total: usize, current: &str) -> Progress {
        Progress {
            stage,
            done,
            total,
            current: current.to_string(),
        }
    }

    #[test]
    fn eta_appears_only_after_the_threshold_and_tracks_the_rate() {
        let t0 = Instant::now();
        let tr = Tracker::starting_at(t0, Duration::from_secs(60), Duration::from_secs(15));
        // 10 files per second, steadily, for 70 s; 1,000 files in total.
        for s in 0..=70u64 {
            tr.update_at(
                &p(Stage::Merge, (s * 10) as usize, 1000, &format!("f{s}")),
                t0 + Duration::from_secs(s),
            );
            let snap = tr.snapshot_at(t0 + Duration::from_secs(s));
            if s < 60 {
                assert_eq!(snap.eta, None, "no ETA before 60 s (at {s} s)");
            }
        }
        let snap = tr.snapshot_at(t0 + Duration::from_secs(70));
        let eta = snap.eta.expect("ETA after 60 s").as_secs_f64();
        assert!((eta - 30.0).abs() < 1.0, "300 files left at 10/s: {eta}");
        assert!(
            snap.eta_band.unwrap() < Duration::from_secs(2),
            "steady rate, narrow band"
        );
        assert_eq!(snap.stalled_for, None);
    }

    #[test]
    fn a_hang_is_reported_as_a_stall_on_the_current_item() {
        let t0 = Instant::now();
        let tr = Tracker::starting_at(t0, Duration::from_secs(60), Duration::from_secs(15));
        tr.update_at(
            &p(Stage::Merge, 5, 10, "big.rs"),
            t0 + Duration::from_secs(1),
        );
        assert_eq!(
            tr.snapshot_at(t0 + Duration::from_secs(10)).stalled_for,
            None
        );
        let snap = tr.snapshot_at(t0 + Duration::from_secs(17));
        assert_eq!(snap.stalled_for, Some(Duration::from_secs(16)));
        assert_eq!(snap.current, "big.rs");
        // Progress clears it.
        tr.update_at(
            &p(Stage::Merge, 6, 10, "next.rs"),
            t0 + Duration::from_secs(18),
        );
        assert_eq!(
            tr.snapshot_at(t0 + Duration::from_secs(19)).stalled_for,
            None
        );
    }

    #[test]
    fn durations_parse_and_print() {
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("1h30m").unwrap(), Duration::from_secs(5400));
        assert!(parse_duration("ten").is_err());
        assert!(parse_duration("5x").is_err());
        assert_eq!(human_duration(Duration::from_secs(65)), "1m05s");
        assert_eq!(human_duration(Duration::from_secs(3720)), "1h02m");
    }
}
