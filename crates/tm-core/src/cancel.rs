//! Per-job cancellation (N-32: v7 had one global flag that every job shared
//! and any caller could reset mid-run).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Controls one job. Clones share the flags; checking one is an atomic load,
/// so the scan and the merge check them per file and inside long loops.
///
/// - `cancel`: stop now, write nothing.
/// - `finish_early`: stop taking new files, then write what is done; the
///   rest is reported as not captured (`--on-deadline partial`).
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    cancel: Arc<AtomicBool>,
    finish: Arc<AtomicBool>,
}

impl CancelToken {
    pub fn new() -> CancelToken {
        CancelToken::default()
    }
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
    pub fn finish_early(&self) {
        self.finish.store(true, Ordering::Relaxed);
    }
    pub fn is_finishing(&self) -> bool {
        self.finish.load(Ordering::Relaxed)
    }
    /// The raw cancel flag, for APIs that take `&AtomicBool`.
    pub fn flag(&self) -> &AtomicBool {
        &self.cancel
    }
    /// The raw finish-early flag.
    pub fn finish_flag(&self) -> &AtomicBool {
        &self.finish
    }
}

/// The work stopped because its job was cancelled; nothing was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}
