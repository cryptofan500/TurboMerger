//! Files in, results out in the same order, without chunk barriers (P9).
//!
//! The merge used to work in chunks of 64 files / 64 MiB and wait for each
//! chunk's slowest file, so one big PDF left nine of ten workers idle, and a
//! file over 64 MiB ran strictly alone (86–102 % CPU of 1,200 % on a folder
//! of scans). Here every worker takes the next file as soon as it is free;
//! results wait in a reorder buffer, and the merge writes them strictly in
//! order, so the output bytes are the same as before.
//!
//! Memory stays bounded by weight in flight: a file counts from when a
//! worker takes it until the merge has written it (its result may wait in
//! the buffer behind a slower one). A worker waits while taking its file
//! would pass [`Limits::budget`] or run more than [`Limits::window`] files
//! ahead — except for the very file the merge waits for, which always
//! starts, so no budget can stall the run.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// What the merge says while it runs (polled about four times a second).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flow {
    Go,
    /// Start nothing new; what has started finishes and is handed over.
    Finish,
    /// Stop: start nothing, hand nothing more over.
    Cancel,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Limits {
    /// Weight (bytes, documents counted several times over) in flight.
    pub(super) budget: u64,
    /// How far ahead of the merge's next file a worker may run.
    pub(super) window: usize,
}

/// What the consumer is handed, in index order.
pub(super) enum Next<T> {
    /// Item `i`'s result.
    Item(usize, T),
    /// Item `i` was never started: the run is finishing early.
    Skipped(usize),
    /// Still waiting for the next item: how many items are finished, and the
    /// one that has been running longest (what a stall report names).
    Waiting {
        finished: usize,
        oldest: Option<usize>,
    },
}

/// How the run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Ended {
    Completed,
    Cancelled,
}

struct State<T> {
    /// The next item the consumer takes.
    next: usize,
    /// Items started so far (they are started in order).
    claimed: usize,
    in_flight: u64,
    results: BTreeMap<usize, (T, u64)>,
    running: BTreeMap<usize, Instant>,
    /// Start nothing new.
    stop: bool,
    /// A worker panicked: the consumer stops waiting, the panic resurfaces.
    poisoned: bool,
}

/// Run `work` over items `0..n` on the worker pool and hand the results to
/// `consume` in index order. `weight(i)` is item `i`'s share of the budget
/// (0 for items `work` does not really process). `flow` is polled by the
/// consumer; its `Cancel` ends the run with [`Ended::Cancelled`]. An error
/// from `consume` stops the run and is returned.
pub(super) fn ordered<T: Send, E>(
    n: usize,
    limits: Limits,
    weight: &(dyn Fn(usize) -> u64 + Sync),
    work: &(dyn Fn(usize) -> T + Sync),
    flow: &dyn Fn() -> Flow,
    consume: &mut dyn FnMut(Next<T>) -> Result<(), E>,
) -> Result<Ended, E> {
    let state = Mutex::new(State {
        next: 0,
        claimed: 0,
        in_flight: 0,
        results: BTreeMap::new(),
        running: BTreeMap::new(),
        stop: false,
        poisoned: false,
    });
    let changed = Condvar::new();
    let finished = AtomicUsize::new(0);
    let lock = || state.lock().unwrap_or_else(|e| e.into_inner());

    let worker = || {
        // A panic in `work` must not leave the consumer waiting for a
        // result that never comes.
        struct Poison<'a, T>(&'a Mutex<State<T>>, &'a Condvar);
        impl<T> Drop for Poison<'_, T> {
            fn drop(&mut self) {
                if std::thread::panicking() {
                    let mut st = self.0.lock().unwrap_or_else(|e| e.into_inner());
                    st.poisoned = true;
                    st.stop = true;
                    self.1.notify_all();
                }
            }
        }
        let _guard = Poison(&state, &changed);
        loop {
            let (i, w) = {
                let mut st = lock();
                if st.stop || st.claimed >= n {
                    return;
                }
                let i = st.claimed;
                st.claimed += 1;
                let w = weight(i);
                // The merge's next item always starts; others wait for room.
                while !(st.stop
                    || i == st.next
                    || (i < st.next + limits.window && st.in_flight + w <= limits.budget))
                {
                    st = changed
                        .wait_timeout(st, Duration::from_millis(100))
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
                st.in_flight += w;
                st.running.insert(i, Instant::now());
                (i, w)
            };
            let out = work(i);
            finished.fetch_add(1, Ordering::Relaxed);
            let mut st = lock();
            st.running.remove(&i);
            st.results.insert(i, (out, w));
            drop(st);
            changed.notify_all();
        }
    };

    std::thread::scope(|s| {
        let producers = s.spawn(|| {
            crate::threads::on_pool(|| {
                rayon::scope(|rs| {
                    for _ in 0..rayon::current_num_threads().max(1) {
                        rs.spawn(|_| worker());
                    }
                })
            })
        });
        let ended = consume_in_order(n, &state, &changed, &finished, flow, consume);
        {
            let mut st = lock();
            st.stop = true;
        }
        changed.notify_all();
        if let Err(panic) = producers.join() {
            std::panic::resume_unwind(panic);
        }
        ended
    })
}

fn consume_in_order<T, E>(
    n: usize,
    state: &Mutex<State<T>>,
    changed: &Condvar,
    finished: &AtomicUsize,
    flow: &dyn Fn() -> Flow,
    consume: &mut dyn FnMut(Next<T>) -> Result<(), E>,
) -> Result<Ended, E> {
    let lock = || state.lock().unwrap_or_else(|e| e.into_inner());
    let mut last_report = Instant::now();
    for i in 0..n {
        let got = loop {
            match flow() {
                Flow::Cancel => return Ok(Ended::Cancelled),
                Flow::Finish => lock().stop = true,
                Flow::Go => {}
            }
            let mut st = lock();
            if let Some((out, w)) = st.results.remove(&i) {
                st.in_flight -= w;
                st.next = i + 1;
                drop(st);
                changed.notify_all();
                break Next::Item(i, out);
            }
            if st.poisoned {
                return Ok(Ended::Completed);
            }
            if st.stop && i >= st.claimed {
                st.next = i + 1;
                break Next::Skipped(i);
            }
            let st = changed
                .wait_timeout(st, Duration::from_millis(250))
                .unwrap_or_else(|e| e.into_inner())
                .0;
            if last_report.elapsed() >= Duration::from_millis(250) {
                let oldest = st
                    .running
                    .iter()
                    .min_by_key(|(_, at)| **at)
                    .map(|(&k, _)| k);
                drop(st);
                last_report = Instant::now();
                consume(Next::Waiting {
                    finished: finished.load(Ordering::Relaxed),
                    oldest,
                })?;
            }
        };
        consume(got)?;
    }
    Ok(Ended::Completed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    fn limits(budget: u64, window: usize) -> Limits {
        Limits { budget, window }
    }

    /// Results arrive in order whatever order the work finishes in, and the
    /// weight in flight never passes the budget except for the item the
    /// consumer waits for.
    #[test]
    fn results_come_in_order_within_the_budget() {
        let n = 200;
        let in_flight = AtomicU64::new(0);
        let peak = AtomicU64::new(0);
        let mut seen = Vec::new();
        let weight = |i: usize| (i % 7) as u64 * 10;
        let work = |i: usize| {
            let w = weight(i);
            let now = in_flight.fetch_add(w, Ordering::SeqCst) + w;
            peak.fetch_max(now, Ordering::SeqCst);
            // Later items finish first now and then.
            std::thread::sleep(Duration::from_micros(((n - i) % 13) as u64 * 50));
            in_flight.fetch_sub(w, Ordering::SeqCst);
            i * 2
        };
        let ended = ordered::<usize, ()>(
            n,
            limits(100, 16),
            &weight,
            &work,
            &|| Flow::Go,
            &mut |next| {
                if let Next::Item(i, out) = next {
                    assert_eq!(out, i * 2);
                    seen.push(i);
                }
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(ended, Ended::Completed);
        assert_eq!(seen, (0..n).collect::<Vec<_>>());
        // Work in progress never passes the budget plus the one oversized item.
        assert!(peak.load(Ordering::SeqCst) <= 100 + 60, "{peak:?}");
    }

    /// An item heavier than the whole budget still runs (alone, when it is
    /// the next one): no budget can stall the run.
    #[test]
    fn an_item_over_the_budget_cannot_stall_the_run() {
        let mut seen = Vec::new();
        let ended = ordered::<usize, ()>(
            6,
            limits(10, 4),
            &|i| if i == 2 { 1_000 } else { 5 },
            &|i| i,
            &|| Flow::Go,
            &mut |next| {
                if let Next::Item(i, _) = next {
                    seen.push(i);
                }
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(ended, Ended::Completed);
        assert_eq!(seen, vec![0, 1, 2, 3, 4, 5]);
    }

    /// Finishing early: what started is handed over, the rest is skipped,
    /// in order. A cancel hands nothing more over.
    #[test]
    fn finish_early_skips_what_never_started_and_cancel_stops() {
        let started = AtomicUsize::new(0);
        let finish = std::sync::atomic::AtomicBool::new(false);
        let mut items = Vec::new();
        let mut skipped = Vec::new();
        let ended = ordered::<usize, ()>(
            100,
            limits(1, 1),
            &|_| 1,
            &|i| {
                started.fetch_add(1, Ordering::SeqCst);
                if i == 3 {
                    finish.store(true, Ordering::SeqCst);
                }
                i
            },
            &|| {
                if finish.load(Ordering::SeqCst) {
                    Flow::Finish
                } else {
                    Flow::Go
                }
            },
            &mut |next| {
                match next {
                    Next::Item(i, _) => items.push(i),
                    Next::Skipped(i) => skipped.push(i),
                    Next::Waiting { .. } => {}
                }
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(ended, Ended::Completed);
        assert_eq!(items.len() + skipped.len(), 100);
        assert!(items.starts_with(&[0, 1, 2, 3]), "{items:?}");
        assert_eq!(items.len(), started.load(Ordering::SeqCst));
        assert!(skipped.windows(2).all(|w| w[0] + 1 == w[1]), "{skipped:?}");

        let ended = ordered::<usize, ()>(
            50,
            limits(1, 1),
            &|_| 1,
            &|i| i,
            &|| Flow::Cancel,
            &mut |_| panic!("nothing is handed over after a cancel"),
        )
        .unwrap();
        assert_eq!(ended, Ended::Cancelled);
    }

    /// While the next item runs long, the consumer hears which item it is.
    #[test]
    fn a_slow_item_is_named_while_the_consumer_waits() {
        let mut named = None;
        ordered::<(), ()>(
            3,
            limits(100, 8),
            &|_| 1,
            &|i| {
                if i == 1 {
                    std::thread::sleep(Duration::from_millis(700));
                }
            },
            &|| Flow::Go,
            &mut |next| {
                if let Next::Waiting {
                    oldest: Some(o), ..
                } = next
                {
                    named = Some(o);
                }
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(named, Some(1));
    }
}
