//! External programs the extractors start (the OCR engines), run so that
//! Cancel stops them at once and none outlives TurboMerger (P7). A plain
//! `Command::output()` blocked until the program ended: Cancel waited for
//! the picture in flight, and an engine started by a merge that was killed
//! kept running.
//!
//! - Unix: the program leads a new process group, so stopping it reaches the
//!   workers it started (monaddoc runs a process pool): SIGTERM to the group,
//!   SIGKILL after a grace period.
//! - Linux: `PR_SET_PDEATHSIG` sends the program SIGTERM when the thread that
//!   started it ends, TurboMerger dying included. Callers always wait for the
//!   program, so otherwise that thread outlives it.
//! - Windows: a Job Object kills the program and its children when the job's
//!   handle closes, this process exiting included.
//!
//! Output goes to files, not pipes: a worker left holding a pipe can keep a
//! reader waiting forever, and a file can be read as it grows (monaddoc's
//! `--events` lines).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// How often the cancel flag and the output are looked at.
const POLL: Duration = Duration::from_millis(100);
/// How long a stopped program gets to exit after SIGTERM.
#[cfg(unix)]
const GRACE: Duration = Duration::from_secs(2);
/// How much of stderr an error message quotes.
const STDERR_TAIL: u64 = 4096;

/// What a program that ran to its end left.
#[derive(Debug)]
pub(crate) struct Finished {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: Vec<u8>,
    /// The end of its stderr, for error messages.
    pub(crate) stderr_tail: String,
}

#[derive(Debug)]
pub(crate) enum RunError {
    /// The job was cancelled and the program stopped.
    Cancelled,
    /// It did not start (`NotFound`: no such program).
    Spawn(std::io::Error),
    /// Its output files could not be made or read.
    Io(std::io::Error),
}

/// What the caller hears while the program runs.
pub(crate) enum Event<'a> {
    /// One complete line of stdout, in order.
    Line(&'a str),
    /// About every [`POLL`]: a chance to report progress.
    Tick,
}

/// Run `cmd` to its end, or until `cancel` is set. Its stdout and stderr go
/// to files in `scratch` (a folder the caller owns and removes).
pub(crate) fn run(
    cmd: &mut Command,
    scratch: &Path,
    cancel: Option<&AtomicBool>,
    mut on: impl FnMut(Event<'_>),
) -> Result<Finished, RunError> {
    let out_path = scratch.join("stdout.txt");
    let err_path = scratch.join("stderr.txt");
    let stdout = File::create(&out_path).map_err(RunError::Io)?;
    let stderr = File::create(&err_path).map_err(RunError::Io)?;
    cmd.stdin(Stdio::null()).stdout(stdout).stderr(stderr);
    isolate(cmd);
    let mut running = Running::start(cmd)?;
    let mut lines = LineTail {
        file: File::open(&out_path).map_err(RunError::Io)?,
        pending: Vec::new(),
    };
    let status = loop {
        lines
            .read(&mut |l: &str| on(Event::Line(l)))
            .map_err(RunError::Io)?;
        if let Some(status) = running.child.try_wait().map_err(RunError::Io)? {
            break status;
        }
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            running.stop();
            return Err(RunError::Cancelled);
        }
        on(Event::Tick);
        std::thread::sleep(POLL);
    };
    running.finished();
    lines
        .read(&mut |l: &str| on(Event::Line(l)))
        .map_err(RunError::Io)?;
    lines.rest(&mut |l: &str| on(Event::Line(l)));
    Ok(Finished {
        status,
        stdout: std::fs::read(&out_path).map_err(RunError::Io)?,
        stderr_tail: tail_of(&err_path),
    })
}

/// Give the program its own process group and, on Linux, a parent-death
/// signal. Called on every command [`run`] starts.
#[cfg(unix)]
fn isolate(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
    #[cfg(target_os = "linux")]
    {
        let parent = std::process::id() as libc::pid_t;
        // SAFETY: prctl and getppid are async-signal-safe system calls, which
        // is all that may run between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM as libc::c_ulong) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                // The parent died before prctl ran: nothing would stop us.
                if libc::getppid() != parent {
                    return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
                }
                Ok(())
            });
        }
    }
}

#[cfg(not(unix))]
fn isolate(_cmd: &mut Command) {}

/// A started program; dropping it stops the program if it still runs.
struct Running {
    child: Child,
    /// It exited and was waited for.
    reaped: bool,
    /// Closing it ends every process in the job (Windows).
    #[cfg(windows)]
    _job: Option<job::JobObject>,
}

impl Running {
    fn start(cmd: &mut Command) -> Result<Running, RunError> {
        let child = cmd.spawn().map_err(RunError::Spawn)?;
        Ok(Running {
            // Best effort: without a job, stopping still ends the program
            // itself (`Child::kill`), just not processes it started.
            #[cfg(windows)]
            _job: job::JobObject::holding(&child).ok(),
            child,
            reaped: false,
        })
    }

    /// The program exited on its own: end what it left behind in its group.
    fn finished(&mut self) {
        self.reaped = true;
        #[cfg(unix)]
        signal_group(&self.child, libc::SIGKILL);
    }

    /// Stop the program and everything it started, and wait for it.
    fn stop(&mut self) {
        if self.reaped {
            return;
        }
        #[cfg(unix)]
        {
            signal_group(&self.child, libc::SIGTERM);
            let deadline = std::time::Instant::now() + GRACE;
            while std::time::Instant::now() < deadline {
                if matches!(self.child.try_wait(), Ok(Some(_))) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            // Whatever ignored SIGTERM, the group's leader included.
            signal_group(&self.child, libc::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.reaped = true;
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Signal the process group `child` leads. Its id cannot have been reused:
/// while any member lives, the group id stays taken.
#[cfg(unix)]
fn signal_group(child: &Child, signal: libc::c_int) {
    let group = child.id() as libc::pid_t;
    // SAFETY: kill(2) on our own child's group; ESRCH (nobody left) is fine.
    unsafe {
        libc::kill(-group, signal);
    }
}

/// Complete lines from a file another process is still writing.
struct LineTail {
    file: File,
    pending: Vec<u8>,
}

impl LineTail {
    /// Hand over every complete line written since the last call.
    fn read(&mut self, each: &mut dyn FnMut(&str)) -> std::io::Result<()> {
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = self.file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            self.pending.extend_from_slice(&buf[..n]);
        }
        let mut start = 0;
        while let Some(end) = self.pending[start..].iter().position(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(&self.pending[start..start + end]);
            each(line.trim_end_matches('\r'));
            start += end + 1;
        }
        self.pending.drain(..start);
        Ok(())
    }

    /// The last line, when the program ended without a newline.
    fn rest(&mut self, each: &mut dyn FnMut(&str)) {
        if !self.pending.is_empty() {
            each(String::from_utf8_lossy(&self.pending).trim_end_matches('\r'));
            self.pending.clear();
        }
    }
}

/// The last [`STDERR_TAIL`] bytes of a file, as text.
fn tail_of(path: &Path) -> String {
    let Ok(mut f) = File::open(path) else {
        return String::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len > STDERR_TAIL && f.seek(SeekFrom::Start(len - STDERR_TAIL)).is_err() {
        return String::new();
    }
    let mut bytes = Vec::new();
    let _ = f.read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).trim().to_string()
}

#[cfg(windows)]
mod job {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// A Job Object holding one program: closing it (this process exiting
    /// included) ends the program and every process it started.
    pub(super) struct JobObject(HANDLE);

    // SAFETY: a job handle may be used and closed from any thread.
    unsafe impl Send for JobObject {}

    impl JobObject {
        pub(super) fn holding(child: &std::process::Child) -> std::io::Result<Self> {
            // SAFETY: plain Win32 calls on a handle we own; the struct is
            // zero-initialised as the API allows.
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job == 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let job = JobObject(job);
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ) == 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                if AssignProcessToJobObject(job.0, child.as_raw_handle() as HANDLE) == 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(job)
            }
        }
    }

    impl Drop for JobObject {
        fn drop(&mut self) {
            // SAFETY: the handle came from CreateJobObjectW and is closed once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Instant;

    fn sh(script: &str) -> Command {
        let mut c = Command::new("sh");
        c.args(["-c", script]);
        c
    }

    /// Gone, or a zombie nobody has waited for yet (it no longer runs).
    #[cfg(target_os = "linux")]
    fn dead(pid: u32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => true,
            // The state follows the parenthesised command name.
            Ok(stat) => stat
                .rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with(['Z', 'X'])),
        }
    }

    #[cfg(target_os = "linux")]
    fn dies_soon(pid: u32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if dead(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn a_finished_program_hands_back_its_output_line_by_line() {
        let dir = tempfile::tempdir().unwrap();
        let mut lines = Vec::new();
        let done = run(
            &mut sh("echo one; echo two >&2; printf 'three\\nlast'"),
            dir.path(),
            None,
            |e| {
                if let Event::Line(l) = e {
                    lines.push(l.to_string());
                }
            },
        )
        .unwrap();
        assert!(done.status.success());
        assert_eq!(lines, ["one", "three", "last"]);
        assert_eq!(String::from_utf8_lossy(&done.stdout), "one\nthree\nlast");
        assert_eq!(done.stderr_tail, "two");
    }

    #[test]
    fn a_missing_program_is_reported_as_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(
            &mut Command::new("/nonexistent/tm-no-such-ocr"),
            dir.path(),
            None,
            |_| {},
        )
        .unwrap_err();
        assert!(
            matches!(&err, RunError::Spawn(e) if e.kind() == std::io::ErrorKind::NotFound),
            "{err:?}"
        );
    }

    /// Cancel ends the program AND the process it started, well before the
    /// 30 s they would run, and `run` says it was cancelled.
    #[test]
    fn cancel_stops_the_program_and_its_children_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);
        let mut pids: Vec<u32> = Vec::new();
        let started = Instant::now();
        let err = run(
            &mut sh("sleep 30 & echo $!; echo $$; sleep 30"),
            dir.path(),
            Some(&cancel),
            |e| {
                if let Event::Line(l) = e {
                    pids.push(l.trim().parse().unwrap());
                    if pids.len() == 2 {
                        cancel.store(true, Ordering::Relaxed);
                    }
                }
            },
        )
        .unwrap_err();
        assert!(matches!(err, RunError::Cancelled), "{err:?}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(pids.len(), 2, "{pids:?}");
        #[cfg(target_os = "linux")]
        for pid in pids {
            assert!(dies_soon(pid), "process {pid} outlived the cancel");
        }
    }

    /// The program dies with the thread that started it (TurboMerger
    /// crashing, or killed, is the case that matters).
    #[cfg(target_os = "linux")]
    #[test]
    fn the_program_dies_with_the_thread_that_started_it() {
        let pid = std::thread::spawn(|| {
            let mut cmd = Command::new("sleep");
            cmd.arg("30");
            isolate(&mut cmd);
            // Dropping a `Child` neither kills nor waits for it.
            cmd.spawn().unwrap().id()
        })
        .join()
        .unwrap();
        assert!(dies_soon(pid), "sleep {pid} outlived its parent thread");
    }

    /// Without a cancel, a program that writes nothing for a while is simply
    /// waited for, with ticks in between.
    #[test]
    fn a_quiet_program_is_waited_for_with_ticks() {
        let dir = tempfile::tempdir().unwrap();
        let mut ticks = 0;
        let done = run(&mut sh("sleep 0.5; echo ok"), dir.path(), None, |e| {
            if let Event::Tick = e {
                ticks += 1;
            }
        })
        .unwrap();
        assert!(done.status.success());
        assert!(ticks >= 2, "{ticks}");
        assert_eq!(String::from_utf8_lossy(&done.stdout), "ok\n");
    }
}
