//! Worker host (plan §7, Phase 2.10): heavy or fragile work — PDF parsing
//! and rendering, OCR — runs in a subprocess the host can watch, cap and
//! kill. Why: 108 of 120 real PDFs made the parser warn (crash isolation);
//! a cancel must stop the work at once (kill); a runaway page must not take
//! the machine's memory (cap); and a static-musl CLI can run glibc workers
//! it could never `dlopen`.
//!
//! Protocol: JSON, one object per line.
//!
//! ```text
//! worker → host  {"type":"hello","protocol":1,"worker":"tm-pdf","version":"…","capabilities":[…]}
//! host → worker  {"type":"job","id":"1","kind":"…","input":{…}}
//! worker → host  {"type":"progress","id":"1","item":"p. 3","done":3,"total":12,"message":"…"}   (any number)
//! worker → host  {"type":"result","id":"1","output":{…}}   or   {"type":"error","id":"1","message":"…"}
//! host → worker  {"type":"shutdown"}
//! ```
//!
//! The host's watchdog reports a stall when the worker says nothing for
//! `Limits::stall_after` (plan §10.4) and kills it when one item runs past
//! `Limits::item_budget` or the job's `CancelToken` is set. Memory is capped
//! with `prlimit(RLIMIT_AS)` on Linux, `setrlimit` on other Unixes, and a Job
//! Object on Windows (which also ends the worker if the host dies).

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tm_core::CancelToken;

/// The protocol version this host speaks.
pub const PROTOCOL: u32 = 1;

/// Messages a worker sends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToHost {
    Hello {
        protocol: u32,
        worker: String,
        version: String,
        #[serde(default)]
        capabilities: Vec<String>,
    },
    Progress {
        id: String,
        item: String,
        done: u64,
        total: u64,
        #[serde(default)]
        message: String,
    },
    Result {
        id: String,
        output: Value,
    },
    Error {
        id: String,
        message: String,
    },
}

/// Messages the host sends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToWorker {
    Job {
        id: String,
        kind: String,
        input: Value,
    },
    Shutdown,
}

/// What a worker may do before the host steps in.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Report a stall after this long without a word (plan §10.4: 10 s).
    pub stall_after: Duration,
    /// Kill the worker when one item takes longer (plan: max(30 s, 5 ×
    /// predicted); callers pass their prediction).
    pub item_budget: Duration,
    /// Address-space cap in bytes (`None` = unlimited).
    pub memory_bytes: Option<u64>,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            stall_after: Duration::from_secs(10),
            item_budget: Duration::from_secs(30),
            memory_bytes: None,
        }
    }
}

/// What the host reports while a job runs.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Progress {
        item: String,
        done: u64,
        total: u64,
        message: String,
    },
    /// Nothing heard from the worker for `quiet` while on `item`.
    Stalled { item: String, quiet: Duration },
}

#[derive(Debug, Clone, PartialEq)]
pub enum WorkerError {
    /// The worker could not be started.
    Spawn(String),
    /// It spoke something other than this protocol.
    Protocol(String),
    /// It reported that the job failed.
    Failed(String),
    /// It exited or closed its output mid-job (with its last stderr lines).
    Crashed(String),
    /// Killed: one item ran past the budget.
    Timeout { item: String, after: Duration },
    /// Killed: the job was cancelled.
    Cancelled,
}

impl std::fmt::Display for WorkerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkerError::Spawn(m) => write!(f, "worker could not start: {}", m),
            WorkerError::Protocol(m) => write!(f, "worker protocol error: {}", m),
            WorkerError::Failed(m) => write!(f, "worker failed: {}", m),
            WorkerError::Crashed(m) => write!(f, "worker crashed: {}", m),
            WorkerError::Timeout { item, after } => write!(
                f,
                "worker killed: {} took longer than {} s",
                if item.is_empty() { "the job" } else { item },
                after.as_secs()
            ),
            WorkerError::Cancelled => f.write_str("cancelled"),
        }
    }
}

impl std::error::Error for WorkerError {}

/// Who a worker says it is.
#[derive(Debug, Clone, PartialEq)]
pub struct Hello {
    pub worker: String,
    pub version: String,
    pub capabilities: Vec<String>,
}

/// A running worker process.
pub struct Worker {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<std::io::Result<String>>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    limits: Limits,
    next_id: u64,
    pub hello: Hello,
    #[cfg(windows)]
    _job: windows::JobObject,
}

const STDERR_TAIL: usize = 20;
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

impl Worker {
    /// Start `program`, apply the memory cap and wait for its hello.
    pub fn spawn(program: &Path, args: &[&str], limits: Limits) -> Result<Worker, WorkerError> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(all(unix, not(target_os = "linux")))]
        if let Some(bytes) = limits.memory_bytes {
            use std::os::unix::process::CommandExt;
            // SAFETY: setrlimit is a plain syscall, safe between fork and exec.
            unsafe {
                cmd.pre_exec(move || {
                    rustix::process::setrlimit(
                        rustix::process::Resource::As,
                        rustix::process::Rlimit {
                            current: Some(bytes),
                            maximum: Some(bytes),
                        },
                    )
                    .map_err(std::io::Error::from)
                });
            }
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| WorkerError::Spawn(format!("{}: {}", program.display(), e)))?;

        #[cfg(target_os = "linux")]
        if let Some(bytes) = limits.memory_bytes {
            let pid = rustix::process::Pid::from_child(&child);
            let cap = rustix::process::Rlimit {
                current: Some(bytes),
                maximum: Some(bytes),
            };
            if let Err(e) = rustix::process::prlimit(Some(pid), rustix::process::Resource::As, cap)
            {
                let _ = child.kill();
                let _ = child.wait();
                return Err(WorkerError::Spawn(format!("memory cap: {}", e)));
            }
        }
        #[cfg(windows)]
        let job = match windows::JobObject::cap(&child, limits.memory_bytes) {
            Ok(j) => j,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(WorkerError::Spawn(format!("job object: {}", e)));
            }
        };

        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));
        {
            let tail = stderr_tail.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let mut t = tail.lock().expect("stderr tail");
                    if t.len() == STDERR_TAIL {
                        t.pop_front();
                    }
                    t.push_back(line);
                }
            });
        }

        let mut worker = Worker {
            child,
            stdin,
            lines: rx,
            stderr_tail,
            limits,
            next_id: 0,
            hello: Hello {
                worker: String::new(),
                version: String::new(),
                capabilities: Vec::new(),
            },
            #[cfg(windows)]
            _job: job,
        };
        let first = match worker.lines.recv_timeout(HELLO_TIMEOUT) {
            Ok(Ok(line)) => line,
            Ok(Err(e)) => return Err(worker.crashed(&e.to_string())),
            Err(RecvTimeoutError::Timeout) => {
                worker.kill();
                return Err(WorkerError::Protocol("no hello within 10 s".into()));
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(worker.crashed("exited before hello"))
            }
        };
        match serde_json::from_str::<ToHost>(&first) {
            Ok(ToHost::Hello {
                protocol,
                worker: name,
                version,
                capabilities,
            }) => {
                if protocol != PROTOCOL {
                    worker.kill();
                    return Err(WorkerError::Protocol(format!(
                        "{} speaks protocol {}, this host speaks {}",
                        name, protocol, PROTOCOL
                    )));
                }
                worker.hello = Hello {
                    worker: name,
                    version,
                    capabilities,
                };
                Ok(worker)
            }
            _ => {
                worker.kill();
                Err(WorkerError::Protocol(format!(
                    "expected hello, got {}",
                    shorten(&first)
                )))
            }
        }
    }

    /// Run one job to its result. Reports progress and stalls through
    /// `on_event`; kills the worker on cancel, on a blown item budget, or on
    /// a protocol violation (the worker cannot be trusted after those).
    pub fn run(
        &mut self,
        kind: &str,
        input: Value,
        cancel: &CancelToken,
        on_event: &mut dyn FnMut(Event),
    ) -> Result<Value, WorkerError> {
        self.next_id += 1;
        let id = self.next_id.to_string();
        let job = ToWorker::Job {
            id: id.clone(),
            kind: kind.to_string(),
            input,
        };
        let sent = self.stdin.as_mut().map(|w| {
            serde_json::to_writer(&mut *w, &job)
                .map_err(std::io::Error::from)
                .and_then(|_| w.write_all(b"\n"))
                .and_then(|_| w.flush())
        });
        if !matches!(sent, Some(Ok(()))) {
            return Err(self.crashed("its input is closed"));
        }

        let mut item = String::new();
        let mut item_started = Instant::now();
        let mut last_word = Instant::now();
        let mut stall_reported = false;
        loop {
            if cancel.is_cancelled() {
                self.kill();
                return Err(WorkerError::Cancelled);
            }
            match self.lines.recv_timeout(Duration::from_millis(50)) {
                Ok(Ok(line)) => {
                    last_word = Instant::now();
                    stall_reported = false;
                    match serde_json::from_str::<ToHost>(&line) {
                        Ok(ToHost::Progress {
                            id: pid,
                            item: it,
                            done,
                            total,
                            message,
                        }) if pid == id => {
                            if it != item {
                                item = it.clone();
                                item_started = Instant::now();
                            }
                            on_event(Event::Progress {
                                item: it,
                                done,
                                total,
                                message,
                            });
                        }
                        Ok(ToHost::Result { id: rid, output }) if rid == id => return Ok(output),
                        Ok(ToHost::Error { id: eid, message }) if eid == id => {
                            return Err(WorkerError::Failed(message))
                        }
                        _ => {
                            self.kill();
                            return Err(WorkerError::Protocol(format!(
                                "unexpected line: {}",
                                shorten(&line)
                            )));
                        }
                    }
                }
                Ok(Err(e)) => return Err(self.crashed(&e.to_string())),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err(self.crashed("exited")),
            }
            let quiet = last_word.elapsed();
            if !stall_reported && quiet >= self.limits.stall_after {
                stall_reported = true;
                on_event(Event::Stalled {
                    item: item.clone(),
                    quiet,
                });
            }
            if item_started.elapsed() >= self.limits.item_budget {
                self.kill();
                return Err(WorkerError::Timeout {
                    item,
                    after: self.limits.item_budget,
                });
            }
        }
    }

    /// Ask the worker to exit; kill it if it does not within a second.
    pub fn shutdown(mut self) {
        if let Some(mut w) = self.stdin.take() {
            let _ = serde_json::to_writer(&mut w, &ToWorker::Shutdown);
            let _ = w.write_all(b"\n");
        }
        let until = Instant::now() + Duration::from_secs(1);
        while Instant::now() < until {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        self.kill();
    }

    /// The OS process id (for tests and diagnostics).
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// The worker went away: its exit status and last stderr lines.
    fn crashed(&mut self, what: &str) -> WorkerError {
        // Give the stderr reader a moment to catch the last lines.
        let status = {
            let until = Instant::now() + Duration::from_millis(500);
            loop {
                match self.child.try_wait() {
                    Ok(Some(s)) => break s.to_string(),
                    _ if Instant::now() >= until => {
                        self.kill();
                        break "killed".to_string();
                    }
                    _ => std::thread::sleep(Duration::from_millis(20)),
                }
            }
        };
        std::thread::sleep(Duration::from_millis(50));
        let tail: Vec<String> = self
            .stderr_tail
            .lock()
            .map(|t| t.iter().cloned().collect())
            .unwrap_or_default();
        let mut msg = format!("{} ({})", what, status);
        if !tail.is_empty() {
            msg.push_str(": ");
            msg.push_str(&tail.join(" | "));
        }
        WorkerError::Crashed(msg)
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            self.kill();
        }
    }
}

fn shorten(s: &str) -> String {
    let mut t: String = s.chars().take(120).collect();
    if t.len() < s.len() {
        t.push('…');
    }
    t
}

/// For worker binaries: write one protocol message as a line on stdout.
pub fn send(msg: &ToHost) {
    let mut out = std::io::stdout().lock();
    let _ = serde_json::to_writer(&mut out, msg);
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

#[cfg(windows)]
mod windows {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    };

    /// A Job Object holding one worker: closing it (host exit included)
    /// ends the worker; an optional per-process memory limit.
    pub(crate) struct JobObject(HANDLE);

    // SAFETY: a job handle may be used and closed from any thread.
    unsafe impl Send for JobObject {}

    impl JobObject {
        pub(crate) fn cap(
            child: &std::process::Child,
            memory: Option<u64>,
        ) -> std::io::Result<Self> {
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
                if let Some(bytes) = memory {
                    info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
                    info.ProcessMemoryLimit = bytes as usize;
                }
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
