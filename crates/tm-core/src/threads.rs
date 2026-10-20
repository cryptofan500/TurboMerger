//! The worker pool every front end runs on (P4: use the hardware fully
//! without input lag). Rayon's global pool gets the logical cores minus
//! one (minus two from 8 cores up), so the desktop always keeps a core,
//! and every worker lowers its own CPU and I/O priority: a merge takes
//! every idle cycle but yields to the mouse, the keyboard and the window
//! manager. Threads that are not workers (the GUI, the CLI's writer) keep
//! their normal priority.
//!
//! `--threads N` (CLI) or `TURBOMERGER_THREADS=N` (every front end)
//! overrides the count; 0 or unset means automatic.

use std::sync::OnceLock;

/// Environment override for the worker count (0 or unset = automatic).
pub const THREADS_ENV: &str = "TURBOMERGER_THREADS";

/// A worker's nice value on Linux (0 is the default, 19 the lowest).
#[cfg(target_os = "linux")]
pub const WORKER_NICE: i32 = 10;

static CONFIGURED: OnceLock<usize> = OnceLock::new();

/// Workers for `logical` cores: one fewer, two fewer from 8 cores up,
/// never fewer than one.
pub fn default_workers(logical: usize) -> usize {
    let reserve = if logical >= 8 { 2 } else { 1 };
    logical.saturating_sub(reserve).max(1)
}

/// Logical cores this process may use (affinity and cgroup quotas count).
pub fn logical_cores() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

/// A worker count from the environment variable's text; 0, empty or
/// unreadable = automatic (`None`).
pub fn parse_workers(text: Option<&str>) -> Option<usize> {
    text?.trim().parse::<usize>().ok().filter(|&n| n > 0)
}

/// The worker count: `explicit` (`--threads`), else `TURBOMERGER_THREADS`,
/// else [`default_workers`] of the logical cores. 0 means automatic.
pub fn resolve_workers(explicit: Option<usize>) -> usize {
    explicit
        .filter(|&n| n > 0)
        .or_else(|| parse_workers(std::env::var(THREADS_ENV).ok().as_deref()))
        .unwrap_or_else(|| default_workers(logical_cores()))
}

/// A pool of `workers` threads that lower their own priority on start.
pub fn builder(workers: usize) -> rayon::ThreadPoolBuilder {
    rayon::ThreadPoolBuilder::new()
        .num_threads(workers)
        .thread_name(|i| format!("tm-pool-{i}"))
        .start_handler(|_| lower_current_thread_priority())
}

/// Build rayon's global pool, once, before the first parallel call; later
/// calls change nothing and return the same count. If rayon was used
/// before this ran, its default pool stays (every core, normal priority)
/// and that pool's size is returned.
pub fn init(explicit: Option<usize>) -> usize {
    *CONFIGURED.get_or_init(|| {
        let workers = resolve_workers(explicit);
        match builder(workers).build_global() {
            Ok(()) => workers,
            Err(_) => rayon::current_num_threads(),
        }
    })
}

/// Lower the calling thread's CPU and I/O priority, best effort: a refusal
/// leaves the thread as it was. On Linux it never raises a priority the
/// thread already had lower (e.g. a run started under `nice 15`).
pub fn lower_current_thread_priority() {
    #[cfg(target_os = "linux")]
    linux::lower();
    #[cfg(target_os = "macos")]
    // SAFETY: sets the calling thread's own QoS class; plain values only.
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0);
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
        };
        // SAFETY: GetCurrentThread returns a pseudo-handle for the calling
        // thread, valid for this call and never closed.
        unsafe {
            SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use libc::c_int;

    // ioprio_set(2) / ioprio_get(2). With IOPRIO_WHO_PROCESS, "who" 0 is
    // the calling thread, and Linux keeps the nice value per thread too, so
    // both calls leave the caller's other threads alone.
    const IOPRIO_WHO_PROCESS: c_int = 1;
    const IOPRIO_CLASS_SHIFT: c_int = 13;
    const IOPRIO_CLASS_BE: c_int = 2;
    const IOPRIO_CLASS_IDLE: c_int = 3;
    /// Best effort class, lowest of its 8 levels.
    pub(super) const WORKER_IOPRIO: c_int = (IOPRIO_CLASS_BE << IOPRIO_CLASS_SHIFT) | 7;

    pub(super) fn lower() {
        // SAFETY: getpriority/setpriority and the ioprio syscalls on the
        // calling thread (who = 0); integers only, results checked.
        unsafe {
            // getpriority cannot fail for the calling thread, so -1 is a
            // real nice value (and below ours either way).
            if libc::getpriority(libc::PRIO_PROCESS, 0) < super::WORKER_NICE {
                libc::setpriority(libc::PRIO_PROCESS, 0, super::WORKER_NICE);
            }
            let io = libc::syscall(libc::SYS_ioprio_get, IOPRIO_WHO_PROCESS, 0);
            if io >= 0 && (io as c_int) >> IOPRIO_CLASS_SHIFT != IOPRIO_CLASS_IDLE {
                libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, WORKER_IOPRIO);
            }
        }
    }

    /// The calling thread's (nice value, I/O priority), for tests.
    #[cfg(test)]
    pub(super) fn current() -> (c_int, libc::c_long) {
        // SAFETY: reads of the calling thread's own priorities.
        unsafe {
            (
                libc::getpriority(libc::PRIO_PROCESS, 0),
                libc::syscall(libc::SYS_ioprio_get, IOPRIO_WHO_PROCESS, 0),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_keeps_one_or_two_cores_free() {
        assert_eq!(default_workers(0), 1);
        assert_eq!(default_workers(1), 1);
        assert_eq!(default_workers(2), 1);
        assert_eq!(default_workers(4), 3);
        assert_eq!(default_workers(7), 6);
        assert_eq!(default_workers(8), 6);
        assert_eq!(default_workers(12), 10);
        assert_eq!(default_workers(64), 62);
    }

    #[test]
    fn env_text_zero_or_junk_means_automatic() {
        assert_eq!(parse_workers(Some("4")), Some(4));
        assert_eq!(parse_workers(Some(" 16\n")), Some(16));
        assert_eq!(parse_workers(Some("0")), None);
        assert_eq!(parse_workers(Some("")), None);
        assert_eq!(parse_workers(Some("-3")), None);
        assert_eq!(parse_workers(Some("many")), None);
        assert_eq!(parse_workers(None), None);
    }

    #[test]
    fn explicit_count_wins() {
        assert_eq!(resolve_workers(Some(3)), 3);
        assert!(resolve_workers(Some(0)) >= 1);
    }

    #[test]
    fn pool_has_the_requested_size() {
        let pool = builder(3).build().unwrap();
        assert_eq!(pool.current_num_threads(), 3);
        let names = pool.broadcast(|_| std::thread::current().name().map(str::to_owned));
        assert!(names
            .iter()
            .all(|n| n.as_deref().is_some_and(|n| n.starts_with("tm-pool-"))));
    }

    /// Every worker runs at nice 10 (or the caller's, if already lower)
    /// and I/O best effort 7; the caller's own thread is unchanged.
    #[cfg(target_os = "linux")]
    #[test]
    fn workers_lower_their_priority_and_leave_the_caller_alone() {
        let (caller_nice, caller_io) = linux::current();
        let pool = builder(2).build().unwrap();
        let seen = pool.broadcast(|_| linux::current());
        let want_nice = caller_nice.max(WORKER_NICE);
        for (nice, io) in seen {
            assert_eq!(nice, want_nice);
            if caller_io >> 13 != 3 {
                assert_eq!(io, libc::c_long::from(linux::WORKER_IOPRIO));
            }
        }
        assert_eq!(linux::current(), (caller_nice, caller_io));
    }
}
