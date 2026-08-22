//! Mount motion gate (rp.md § Mount Motion Gate): an rp-internal
//! readers-writer gate on the singular mount.
//!
//! Slew and dither take
//! the gate exclusively; captures through a camera terminating an
//! imaging train hold it shared. Tokio's `RwLock` provides exactly
//! the queueing the contract requires — a fair FIFO queue where a
//! pending exclusive blocks new shared acquires (no starvation) and
//! queued exclusives run in arrival order.
//!
//! Waits are transitively bounded: every holder is itself
//! deadline-bounded (captures by `duration` plus the readout
//! backstop, slews and dithers by their own deadlines and settle
//! timeouts), so the gate adds no timeout of its own.

use std::future::Future;
use std::sync::Arc;

use tokio::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use tracing::debug;

use crate::events::EventBus;

pub struct MotionGate {
    lock: RwLock<()>,
    event_bus: Arc<EventBus>,
}

impl MotionGate {
    pub fn new(event_bus: Arc<EventBus>) -> Self {
        Self {
            lock: RwLock::new(()),
            event_bus,
        }
    }

    /// Acquire the gate exclusively for the named mount motion,
    /// emitting `mount_motion_pending {operation}` when the motion
    /// cannot start immediately. The emission happens *after* this
    /// waiter has entered the fair queue (the acquire future is
    /// polled once first), so a shared acquire issued in reaction to
    /// the event is guaranteed to be ordered behind this motion —
    /// the property the motion-gate BDD scenarios lean on.
    pub async fn exclusive(&self, operation: &str) -> RwLockWriteGuard<'_, ()> {
        let mut acquire = Box::pin(self.lock.write());
        let first_poll =
            std::future::poll_fn(|cx| std::task::Poll::Ready(acquire.as_mut().poll(cx))).await;
        match first_poll {
            std::task::Poll::Ready(guard) => guard,
            std::task::Poll::Pending => {
                debug!(operation, "mount motion pending: gate held");
                self.event_bus.emit(
                    "mount_motion_pending",
                    serde_json::json!({ "operation": operation }),
                );
                acquire.await
            }
        }
    }

    /// Hold the gate shared for an imaging-train exposure. Blocks
    /// while an exclusive motion holds the gate or waits in the
    /// queue; concurrent shared holders coexist freely.
    pub async fn shared(&self) -> RwLockReadGuard<'_, ()> {
        self.lock.read().await
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::time::Duration;

    use super::*;

    fn bus_and_gate() -> (Arc<EventBus>, Arc<MotionGate>) {
        let bus = Arc::new(EventBus::from_config(&[], None).unwrap());
        let gate = Arc::new(MotionGate::new(bus.clone()));
        (bus, gate)
    }

    #[tokio::test]
    async fn exclusive_on_a_free_gate_emits_no_pending_event() {
        let (bus, gate) = bus_and_gate();
        let mut rx = bus.subscribe();
        let _guard = gate.exclusive("slew").await;
        assert!(
            rx.try_recv().is_err(),
            "an uncontended exclusive acquire must not announce itself"
        );
    }

    #[tokio::test]
    async fn shared_holders_coexist() {
        let (_bus, gate) = bus_and_gate();
        let _s1 = gate.shared().await;
        let _s2 = tokio::time::timeout(Duration::from_secs(1), gate.shared())
            .await
            .expect("a second shared acquire must not block");
    }

    /// The load-bearing ordering property: once `mount_motion_pending`
    /// is observed, the exclusive is already queued, so a shared
    /// acquire issued afterwards blocks until the motion has run —
    /// even though another shared holder is still in flight.
    #[tokio::test(start_paused = true)]
    async fn pending_exclusive_blocks_new_shared_acquires() {
        let (bus, gate) = bus_and_gate();
        let mut rx = bus.subscribe();

        let s1 = gate.shared().await;
        let exclusive = {
            let gate = gate.clone();
            tokio::spawn(async move {
                let _guard = gate.exclusive("dither").await;
            })
        };

        let envelope = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("expected the pending event while a shared holder is live")
            .unwrap();
        assert_eq!(envelope.event, "mount_motion_pending");
        assert_eq!(envelope.payload["operation"], "dither");

        let blocked = tokio::time::timeout(Duration::from_millis(100), gate.shared()).await;
        assert!(
            blocked.is_err(),
            "a shared acquire issued after the pending event must wait behind the motion"
        );

        drop(s1);
        exclusive.await.unwrap();
        let _reacquired = tokio::time::timeout(Duration::from_secs(5), gate.shared())
            .await
            .expect("shared must acquire once the motion released the gate");
    }

    #[tokio::test(start_paused = true)]
    async fn exclusive_waits_for_the_shared_holder_before_acquiring() {
        let (_bus, gate) = bus_and_gate();
        let s1 = gate.shared().await;

        let blocked =
            tokio::time::timeout(Duration::from_millis(100), gate.exclusive("slew")).await;
        assert!(
            blocked.is_err(),
            "exclusive must wait while a shared holder is live"
        );

        drop(s1);
        let _acquired = tokio::time::timeout(Duration::from_secs(5), gate.exclusive("slew"))
            .await
            .expect("exclusive must acquire once the shared holder released");
    }
}
