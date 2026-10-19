//! A device's slot: one of rp's calls through the device holds it at a
//! time.
//!
//! A capture holds its camera's (rp.md § Capture Tool Details,
//! Concurrency), a move its focuser's, a filter change its wheel's, a
//! rotation its rotator's (§ Focuser, Filter Wheel and Rotator Tool
//! Details), and a train lease holds the slots of what it covers for
//! the whole lease (§ Train Lease).
//!
//! It lives on the device's registry entry rather than in its session,
//! so it outlives a reconnect: a call still running on the old session
//! keeps out one started on the new. The guard is owned (`Arc`-backed)
//! so a lease can keep it past the call that took it, and a cancelled
//! capture can hand it to a task that waits out the camera's exposure.

use std::sync::Arc;

use tokio::sync::{Mutex, OwnedMutexGuard};
use tracing::debug;

/// What holding a [`DeviceSlot`] looks like; dropping it hands the slot
/// to the next waiter.
pub type SlotGuard = OwnedMutexGuard<()>;

#[derive(Debug, Default)]
pub struct DeviceSlot(Arc<Mutex<()>>);

impl DeviceSlot {
    /// Wait for the slot. Tokio's mutex queues waiters in arrival order.
    /// The caller races this against its own cancellation.
    pub async fn take(&self, kind: &str, id: &str) -> SlotGuard {
        if let Some(slot) = self.try_take() {
            return slot;
        }
        debug!(kind, id, "device busy: queued behind the call in flight");
        self.0.clone().lock_owned().await
    }

    /// The slot if it is free now; `None` while something holds it.
    #[must_use]
    pub fn try_take(&self) -> Option<SlotGuard> {
        self.0.clone().try_lock_owned().ok()
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_second_taker_waits_until_the_first_guard_drops() {
        let slot = DeviceSlot::default();
        let first = slot.take("focuser", "f").await;

        let waited = tokio::time::timeout(Duration::from_secs(1), slot.take("focuser", "f")).await;
        assert!(waited.is_err(), "the slot was handed out twice");

        drop(first);
        let _second = tokio::time::timeout(Duration::from_secs(1), slot.take("focuser", "f"))
            .await
            .expect("the slot was not handed on once the first guard dropped");
    }

    #[tokio::test]
    async fn try_take_answers_none_while_the_slot_is_held() {
        let slot = DeviceSlot::default();
        let held = slot.try_take().expect("a free slot is taken at once");
        assert!(slot.try_take().is_none(), "the slot was handed out twice");
        drop(held);
        assert!(slot.try_take().is_some(), "the slot was not handed back");
    }
}
