use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Default, Clone, Copy)]
pub struct Probe {
    pub delay_ms: u64,
    /// Incremented on scan ENTRY (before the delay), so a waiting test
    /// can tell that the slow scan is genuinely in flight.
    pub scans: usize,
    /// Per-dir override of `QUEUE_VIEW_REFRESH_DEADLINE` (0 = use the
    /// production value). Keyed by dir like everything else here so a
    /// timing test cannot perturb another test's staging dir.
    pub refresh_deadline_ms: u64,
    /// Per-dir override of `QUEUE_VIEW_COLD_WAIT` (0 = production value).
    pub cold_wait_ms: u64,
}

static PROBES: Mutex<Option<HashMap<String, Probe>>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut HashMap<String, Probe>) -> R) -> R {
    let mut g = PROBES.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(HashMap::new))
}

/// Arm the probe for `dir`; every subsequent scan of it sleeps `delay_ms`.
pub fn arm(dir: &str, delay_ms: u64) {
    with(|m| {
        m.entry(dir.to_string()).or_default().delay_ms = delay_ms;
    });
}

/// Number of scans this dir has received since it was first armed.
pub fn scans(dir: &str) -> usize {
    with(|m| m.get(dir).map(|p| p.scans).unwrap_or(0))
}

/// Shorten (or lengthen) how long this dir's in-flight refresh marker is
/// trusted before it is presumed dead. Lets a test observe the
/// wedged-refresher takeover in milliseconds instead of the production
/// deadline.
pub fn set_refresh_deadline(dir: &str, ms: u64) {
    with(|m| {
        m.entry(dir.to_string()).or_default().refresh_deadline_ms = ms;
    });
}

/// Shorten how long a COLD caller (nothing at all to serve) parks before
/// giving up on the in-flight scan.
pub fn set_cold_wait(dir: &str, ms: u64) {
    with(|m| {
        m.entry(dir.to_string()).or_default().cold_wait_ms = ms;
    });
}

/// `(refresh_deadline_ms, cold_wait_ms)`; 0 means "use production".
pub fn overrides(dir: &str) -> (u64, u64) {
    with(|m| {
        m.get(dir)
            .map(|p| (p.refresh_deadline_ms, p.cold_wait_ms))
            .unwrap_or((0, 0))
    })
}

/// Called from the production scan path. Never holds the probe lock
/// across the sleep.
pub fn enter(dir: &str) {
    let delay = with(|m| match m.get_mut(dir) {
        Some(p) => {
            p.scans += 1;
            p.delay_ms
        }
        None => 0,
    });
    if delay > 0 {
        std::thread::sleep(std::time::Duration::from_millis(delay));
    }
}
