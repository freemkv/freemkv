//! Copy stall feedback never transfers ownership away from a blocked worker.
use std::io;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug)]
struct Stalled(String);
impl std::fmt::Display for Stalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Stalled {}

pub(super) fn was_stalled(error: &io::Error) -> bool {
    error.get_ref().is_some_and(|e| e.is::<Stalled>())
}

// The worker's current operation, and a heartbeat for progress that adds no bytes
// (a durable flush's completed pieces or server write counters).
pub(super) struct Activity(Mutex<&'static str>, AtomicU64);

impl Default for Activity {
    fn default() -> Self {
        Self(Mutex::new("starting destination copy"), AtomicU64::new(0))
    }
}

impl Activity {
    pub fn set(&self, operation: &'static str) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = operation;
    }
    pub fn beat(&self) {
        self.1.fetch_add(1, Ordering::Relaxed);
    }
    fn beats(&self) -> u64 {
        self.1.load(Ordering::Relaxed)
    }
    fn operation(&self) -> &'static str {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub(super) struct Monitor<'a> {
    pub job: &'a Path,
    pub destination: &'a Path,
    pub written: &'a AtomicU64,
    pub activity: &'a Activity,
    pub halt: &'a libfreemkv::Halt,
    pub progress: &'a dyn Fn(u8, f64, f64, f64),
    pub source_size: u64,
    pub stall_window: Duration,
    pub poll_interval: Duration,
}

impl Monitor<'_> {
    pub fn wait(
        self,
        rx: std::sync::mpsc::Receiver<io::Result<u64>>,
        worker: std::thread::JoinHandle<()>,
    ) -> io::Result<u64> {
        let mut last_bytes = 0;
        let mut last_beats = self.activity.beats();
        let progress = libfreemkv::halt::Liveness::new();
        let mut timer = libfreemkv::halt::StallTimer::new(self.stall_window, &progress);
        let mut stalled = None;
        let mut cancelled = false;
        let mut speed = freemkv_engine::SpeedEstimator::new();
        speed.observe(Instant::now(), 0);
        loop {
            let now = Instant::now();
            let done = self.written.load(Ordering::Relaxed);
            let beats = self.activity.beats();
            if done != last_bytes || beats != last_beats {
                (last_bytes, last_beats) = (done, beats);
                progress.bump();
            }
            let expired = timer.poll(&progress) == libfreemkv::halt::Stall::Expired;
            if stalled.is_none() && expired {
                let reason = format!(
                    "{} stalled for {} seconds without byte progress: {}",
                    self.activity.operation(),
                    self.stall_window.as_secs(),
                    self.destination.display()
                );
                self.halt.cancel();
                self.blocked(&reason);
                stalled = Some(reason);
            }
            if crate::server::SHUTDOWN.load(Ordering::Relaxed) {
                self.halt.cancel();
                cancelled = true;
            }
            match rx.recv_timeout(self.poll_interval) {
                Ok(result) => {
                    // Never detach on timeout/cancel: caller still holds its
                    // job lease and target locks until this join completes.
                    let joined = worker.join();
                    self.worker_returned();
                    if let Some(reason) = stalled {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, Stalled(reason)));
                    }
                    if cancelled {
                        return Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            "copy cancelled; worker drained",
                        ));
                    }
                    joined.map_err(|_| io::Error::other("copy worker panicked"))?;
                    return result;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = worker.join();
                    self.worker_returned();
                    if let Some(reason) = stalled {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, Stalled(reason)));
                    }
                    return Err(io::Error::other("copy worker stopped without a result"));
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if stalled.is_none() && !cancelled {
                        let pct = done
                            .saturating_mul(100)
                            .checked_div(self.source_size)
                            .unwrap_or(0)
                            .min(100) as u8;
                        let unit = crate::server::util::BYTES_PER_GIB;
                        (self.progress)(
                            pct,
                            done as f64 / unit,
                            self.source_size as f64 / unit,
                            speed.observe(now, done),
                        );
                    }
                }
            }
        }
    }

    fn blocked(&self, reason: &str) {
        // No filesystem logging from this monitor: the log may share the
        // stalled mount. Publish the actionable reason directly to SSE state.
        // The silent sink keeps filesystem logging out of this monitor.
        let key = self.job.to_string_lossy();
        super::record_error_with(
            &key,
            reason,
            "Cancellation requested; an already-entered publication may still finish. Cannot retry while the filesystem call is active; restore/remount the filesystem and wait for the worker to return. Sources and ownership are retained.",
            |_| {},
        );
        if let Some(error) = super::MOVE_ERRORS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(key.as_ref())
        {
            error.worker_active = true;
            error.copy_stalled = true;
            error.retry_held = true;
            error.retry_after = None;
        }
        for state in super::MOVE_STATE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter_mut()
        {
            if matches!(
                state.phase,
                super::MovePhase::Copying | super::MovePhase::Finalizing
            ) {
                state.phase = super::MovePhase::Blocked;
                state.speed_mbs = 0.0;
                state.eta.clear();
            }
        }
        // Shown first: the journal write below may sit on the same slow share.
        super::commit_stall(&key);
    }

    fn worker_returned(&self) {
        if let Some(error) = super::MOVE_ERRORS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(self.job.to_string_lossy().as_ref())
            && error.copy_stalled
        {
            error.worker_active = false;
            error.hint = "The copy worker has returned. Source files and delivery journal are retained; restore the filesystem; the delivery retries on its backoff, or choose Retry now to reconcile any completed publication.".into();
        }
    }
}

#[cfg(test)]
#[path = "mover_copy_monitor_tests.rs"]
mod tests;
