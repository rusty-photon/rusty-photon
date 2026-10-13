//! One [`Sdk`](crate::Sdk) at a time in this process.
//!
//! `InitQHYCCDResource` and `ReleaseQHYCCDResource` act on the process, not on
//! the `Sdk` that calls them, and the SDK does not count them: an `Sdk` dropped
//! while another lives would release the resource under the other's cameras.
//! So [`Sdk::new`](crate::Sdk::new) and [`Sdk::try_new`](crate::Sdk::try_new)
//! take [`SDK_GATE`] before the init, and the `Sdk` keeps it until its drop has
//! run the release. The simulated build takes it too, so a host's simulation
//! tests meet the rule its hardware does.

use parking_lot::{Condvar, Mutex};

/// The gate every `Sdk` made by `new` or `try_new` is made under.
pub static SDK_GATE: SdkGate = SdkGate::new();

/// Whether an `Sdk` is alive, and the wait for it to go.
#[derive(Debug)]
pub struct SdkGate {
    held: Mutex<bool>,
    freed: Condvar,
}

/// The gate, held: released when this drops.
#[derive(Debug)]
#[must_use = "the gate is released as soon as the guard is dropped"]
pub struct SdkGuard(&'static SdkGate);

impl SdkGate {
    const fn new() -> Self {
        Self {
            held: Mutex::new(false),
            freed: Condvar::new(),
        }
    }

    /// Take the gate, waiting while it is held. Blocking.
    pub fn acquire(&'static self) -> SdkGuard {
        let mut held = self.held.lock();
        while *held {
            self.freed.wait(&mut held);
        }
        *held = true;
        drop(held);
        SdkGuard(self)
    }

    /// Take the gate if it is free.
    pub fn try_acquire(&'static self) -> Option<SdkGuard> {
        let mut held = self.held.lock();
        if *held {
            return None;
        }
        *held = true;
        drop(held);
        Some(SdkGuard(self))
    }
}

impl Drop for SdkGuard {
    fn drop(&mut self) {
        *self.0.held.lock() = false;
        self.0.freed.notify_one();
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn a_held_gate_cannot_be_tried_until_it_is_released() {
        static GATE: SdkGate = SdkGate::new();
        let guard = GATE.acquire();
        assert!(GATE.try_acquire().is_none());
        drop(guard);
        let again = GATE.try_acquire();
        assert!(again.is_some());
    }

    #[test]
    fn a_wait_for_the_gate_ends_when_it_is_released() {
        static GATE: SdkGate = SdkGate::new();
        let guard = GATE.try_acquire().unwrap();
        let (taken_tx, taken) = mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let guard = GATE.acquire();
            taken_tx.send(()).unwrap();
            drop(guard);
        });
        assert_eq!(
            taken.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout),
            "the gate was taken while held"
        );
        drop(guard);
        taken.recv_timeout(Duration::from_secs(30)).unwrap();
        waiter.join().unwrap();
        assert!(GATE.try_acquire().is_some(), "the waiter released it");
    }
}
