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

pub(super) struct Activity(Mutex<&'static str>);

impl Default for Activity {
    fn default() -> Self {
        Self(Mutex::new("starting destination copy"))
    }
}

impl Activity {
    pub fn set(&self, operation: &'static str) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = operation;
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
        let mut advanced = Instant::now();
        let mut stalled = None;
        let mut cancelled = false;
        let mut speed = freemkv_engine::SpeedEstimator::new();
        speed.observe(advanced, 0);
        loop {
            let now = Instant::now();
            let done = self.written.load(Ordering::Relaxed);
            if done != last_bytes {
                last_bytes = done;
                advanced = now;
            }
            if stalled.is_none() && now.duration_since(advanced) >= self.stall_window {
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
        let key = self.job.to_string_lossy();
        {
            let mut errors = super::MOVE_ERRORS.lock().unwrap_or_else(|e| e.into_inner());
            let attempts = errors.get(key.as_ref()).map_or(0, |e| e.attempts);
            errors.insert(key.to_string(), super::MoverError {
                path: key.to_string(), reason: reason.into(),
                hint: "Cancellation requested; an already-entered publication may still finish. Cannot retry while the filesystem call is active; restore/remount the filesystem and wait for the worker to return. Sources and ownership are retained.".into(),
                attempts, worker_active: true, copy_stalled: true,
                retry_held: true, retry_after: None, waiting_for_folder: false,
            });
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
    }

    fn worker_returned(&self) {
        if let Some(error) = super::MOVE_ERRORS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(self.job.to_string_lossy().as_ref())
            && error.copy_stalled
        {
            error.worker_active = false;
            error.hint = "The copy worker has returned. Source files and delivery journal are retained; restore the filesystem, then choose Retry to reconcile any completed publication.".into();
        }
    }
}

#[cfg(test)]
#[path = "mover_copy_monitor_tests.rs"]
mod tests;
