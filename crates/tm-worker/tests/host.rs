//! The worker host against its test double (Phase 2.10 and the Phase 2
//! gate "stall message within 15 s of an injected hang").

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::json;
use tm_core::CancelToken;
use tm_worker::{Event, Limits, Worker, WorkerError};

fn worker(limits: Limits) -> Worker {
    Worker::spawn(Path::new(env!("CARGO_BIN_EXE_tm-test-worker")), &[], limits).expect("spawns")
}

fn alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        // A reaped child has no /proc entry; a zombie would show state Z.
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .map(|s| !s.contains(") Z "))
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        false
    }
}

#[test]
fn hello_echo_progress_and_several_jobs_on_one_worker() {
    let mut w = worker(Limits::default());
    assert_eq!(w.hello.worker, "tm-test-worker");
    assert!(w.hello.capabilities.contains(&"echo".to_string()));
    let out = w
        .run("echo", json!({"x": 1}), &CancelToken::new(), &mut |_| {})
        .unwrap();
    assert_eq!(out, json!({"x": 1}));
    let mut events = Vec::new();
    let out = w
        .run(
            "progress",
            json!({"items": 3, "ms": 5}),
            &CancelToken::new(),
            &mut |e| events.push(e),
        )
        .unwrap();
    assert_eq!(out, json!({"items": 3}));
    assert_eq!(
        events.last(),
        Some(&Event::Progress {
            item: "item 3".into(),
            done: 3,
            total: 3,
            message: String::new()
        })
    );
    w.shutdown();
}

#[test]
fn an_injected_hang_is_reported_within_15_s_with_default_limits() {
    let mut w = worker(Limits::default());
    let cancel = CancelToken::new();
    let started = Instant::now();
    let mut stalled_at = None;
    let err = w
        .run("hang", json!({"item": "p. 17"}), &cancel, &mut |e| {
            if let Event::Stalled { item, quiet } = e {
                assert_eq!(item, "p. 17");
                assert!(quiet >= Duration::from_secs(10));
                stalled_at = Some(started.elapsed());
                cancel.cancel();
            }
        })
        .unwrap_err();
    let at = stalled_at.expect("a stall report");
    assert!(at < Duration::from_secs(15), "stall reported after {at:?}");
    assert_eq!(err, WorkerError::Cancelled);
}

#[test]
fn cancel_kills_the_worker_within_a_second() {
    let mut w = worker(Limits::default());
    let pid = w.pid();
    let cancel = CancelToken::new();
    let c2 = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        c2.cancel();
    });
    let t0 = Instant::now();
    let err = w.run("hang", json!({}), &cancel, &mut |_| {}).unwrap_err();
    assert_eq!(err, WorkerError::Cancelled);
    let latency = t0.elapsed() - Duration::from_millis(300);
    assert!(latency < Duration::from_secs(1), "{latency:?}");
    assert!(!alive(pid), "the worker process is gone");
}

#[test]
fn an_item_over_budget_is_killed() {
    let mut w = worker(Limits {
        item_budget: Duration::from_secs(1),
        ..Limits::default()
    });
    let t0 = Instant::now();
    let err = w
        .run(
            "hang",
            json!({"item": "fig 3"}),
            &CancelToken::new(),
            &mut |_| {},
        )
        .unwrap_err();
    assert!(
        matches!(&err, WorkerError::Timeout { item, .. } if item == "fig 3"),
        "{err:?}"
    );
    assert!(t0.elapsed() < Duration::from_secs(3));
}

#[test]
fn crashes_failures_and_garbage_are_errors_not_hangs() {
    let mut w = worker(Limits::default());
    let err = w
        .run("fail", json!({}), &CancelToken::new(), &mut |_| {})
        .unwrap_err();
    assert_eq!(err, WorkerError::Failed("requested failure".into()));
    let err = w
        .run("crash", json!({}), &CancelToken::new(), &mut |_| {})
        .unwrap_err();
    assert!(
        matches!(&err, WorkerError::Crashed(m) if m.contains("crashing on purpose")),
        "{err:?}"
    );
    let mut w = worker(Limits::default());
    let err = w
        .run("garbage", json!({}), &CancelToken::new(), &mut |_| {})
        .unwrap_err();
    assert!(matches!(err, WorkerError::Protocol(_)), "{err:?}");
}

#[test]
fn a_protocol_mismatch_is_refused_at_hello() {
    let r = Worker::spawn(
        Path::new(env!("CARGO_BIN_EXE_tm-test-worker")),
        &["--protocol", "99"],
        Limits::default(),
    );
    match r {
        Err(WorkerError::Protocol(m)) => assert!(m.contains("protocol 99"), "{m}"),
        Err(e) => panic!("unexpected error {e:?}"),
        Ok(_) => panic!("a protocol-99 worker was accepted"),
    }
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn the_memory_cap_stops_a_runaway_worker() {
    let mut w = worker(Limits {
        memory_bytes: Some(512 << 20),
        ..Limits::default()
    });
    let mut peak = 0u64;
    let err = w
        .run("oom", json!({}), &CancelToken::new(), &mut |e| {
            if let Event::Progress { done, .. } = e {
                peak = done;
            }
        })
        .unwrap_err();
    assert!(matches!(err, WorkerError::Crashed(_)), "{err:?}");
    assert!(peak * 64 <= 512, "stopped at {} MiB", peak * 64);
}
