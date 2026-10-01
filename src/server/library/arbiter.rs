//! The one mux slot the ripper and the Library remux queue share.
//!
//! Rip has priority and never waits: taking a [`RipSlot`] is a counter bump.
//! A remux only starts while no rip holds a slot, and a running remux polls
//! [`Arbiter::rip_active`] through its `Sink::should_cancel`, so a rip that
//! starts mid-remux cancels it. That is safe: the engine muxes to `.partial`
//! and only renames over the old MKV once verified, and the worker re-queues
//! a preempted job at the head of the queue.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Shared state between rip workers and the remux worker.
#[derive(Debug, Default)]
pub struct Arbiter {
    rips: AtomicUsize,
    // Bumped on every rip start, so a remux can tell it was preempted even if
    // the rip already finished by the time it looks.
    rip_starts: AtomicU64,
}

/// Held by a rip or mux worker for the length of its disk-writing work.
#[must_use = "the slot is released when this guard drops"]
pub struct RipSlot<'a>(&'a Arbiter);

impl Drop for RipSlot<'_> {
    fn drop(&mut self) {
        self.0.rips.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Arbiter {
    pub const fn new() -> Self {
        Self {
            rips: AtomicUsize::new(0),
            rip_starts: AtomicU64::new(0),
        }
    }

    /// Claim the slot for a rip. Never blocks; a running remux sees it and stops.
    pub fn rip(&self) -> RipSlot<'_> {
        self.rips.fetch_add(1, Ordering::SeqCst);
        self.rip_starts.fetch_add(1, Ordering::SeqCst);
        RipSlot(self)
    }

    /// True while any rip or ripper-side mux holds the slot.
    pub fn rip_active(&self) -> bool {
        self.rips.load(Ordering::SeqCst) > 0
    }

    /// A token for "rips seen so far"; compare with [`Arbiter::rip_started_since`].
    pub fn epoch(&self) -> u64 {
        self.rip_starts.load(Ordering::SeqCst)
    }

    /// Whether a rip started after `epoch` was taken (it may have ended since).
    pub fn rip_started_since(&self, epoch: u64) -> bool {
        self.rip_starts.load(Ordering::SeqCst) != epoch
    }
}

/// The daemon's arbiter.
pub static ARBITER: Arbiter = Arbiter::new();

/// Hook for the ripper: hold the returned guard while ripping or muxing.
pub fn claim_for_rip() -> RipSlot<'static> {
    ARBITER.rip()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rip_slot_is_counted_and_released() {
        let a = Arbiter::new();
        assert!(!a.rip_active());
        let e = a.epoch();
        {
            let _one = a.rip();
            let _two = a.rip();
            assert!(a.rip_active());
        }
        assert!(!a.rip_active());
        assert!(
            a.rip_started_since(e),
            "the starts stay visible after release"
        );
    }

    #[test]
    fn no_rip_since_the_epoch_means_not_preempted() {
        let a = Arbiter::new();
        let e = a.epoch();
        assert!(!a.rip_started_since(e));
        drop(a.rip());
        assert!(a.rip_started_since(e));
        assert!(!a.rip_started_since(a.epoch()), "a later epoch is clean");
    }
}
