//! Tracks helpers whose caller may time out and drop their join handle.
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Default)]
struct State {
    active: usize,
    sealed: bool,
}

#[derive(Default)]
pub(super) struct Background {
    state: Mutex<State>,
    changed: Condvar,
}

struct Ticket(Arc<Background>);

impl Drop for Ticket {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.active -= 1;
        self.0.changed.notify_all();
    }
}

impl Background {
    pub(super) fn spawn<T: Send + 'static>(
        self: &Arc<Self>,
        name: &str,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> std::io::Result<std::thread::JoinHandle<T>> {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.sealed {
                return Err(std::io::Error::other("services are drained for restart"));
            }
            state.active += 1;
        }
        // Reserve before spawn; spawn failure and panic release the same ticket.
        let ticket = Ticket(self.clone());
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let _ticket = ticket;
                f()
            })
    }

    /// Seal admission atomically with observing zero helpers. A timeout retains
    /// every ticket; it never authorizes resetting the generation's cancellation.
    pub(super) fn drain(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while state.active != 0 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            state = self
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        state.sealed = true;
        true
    }

    pub(super) fn reopen(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            state.sealed && state.active == 0,
            "restart requires a complete drain"
        );
        state.sealed = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Barrier,
        atomic::{AtomicBool, Ordering},
    };

    #[test]
    fn detached_worker_blocks_restart_until_barrier_released() {
        for name in ["index", "audit", "health", "keydb", "http", "delivery"] {
            let background = Arc::new(Background::default());
            let barrier = Arc::new(Barrier::new(2));
            let shutdown = Arc::new(AtomicBool::new(true));
            let wrote = Arc::new(AtomicBool::new(false));
            let handle = background
                .spawn(name, {
                    let (barrier, shutdown, wrote) =
                        (barrier.clone(), shutdown.clone(), wrote.clone());
                    move || {
                        barrier.wait();
                        if !shutdown.load(Ordering::Acquire) {
                            wrote.store(true, Ordering::Release);
                        }
                    }
                })
                .unwrap();
            drop(handle); // Exactly the timeout/detachment case.
            assert!(!background.drain(Duration::ZERO));
            assert!(shutdown.load(Ordering::Acquire));
            barrier.wait();
            assert!(background.drain(Duration::from_secs(5)));
            assert!(!wrote.load(Ordering::Acquire));
            assert!(background.spawn("late old admission", || ()).is_err());
            background.reopen();
            shutdown.store(false, Ordering::Release);
            background
                .spawn("new generation", || ())
                .unwrap()
                .join()
                .unwrap();
        }
    }

    #[test]
    fn panicking_helper_releases_its_ticket() {
        let background = Arc::new(Background::default());
        assert!(
            background
                .spawn("panic", || panic!("test panic"))
                .unwrap()
                .join()
                .is_err()
        );
        assert!(background.drain(Duration::ZERO));
    }
}
