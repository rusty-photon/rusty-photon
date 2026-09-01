//! The live Alpaca session slot every equipment entry holds.

use std::sync::{Arc, RwLock};

/// The per-entry device-session slot.
///
/// Device handle plus the `connected` flag the status API reports, in
/// one slot so the reconnect supervisor's updates are atomic with
/// respect to readers (rp.md § Device Session Recovery). The lock is
/// held only to clone the handle or flip the flag — never across an
/// await.
///
/// A disconnected slot keeps its stale handle until a successful
/// re-establish replaces it: concurrent callers then see honest
/// `NOT_CONNECTED` errors from the device rather than a handle
/// vanishing mid-operation.
pub struct DeviceSession<T: ?Sized> {
    state: RwLock<SessionState<T>>,
}

struct SessionState<T: ?Sized> {
    connected: bool,
    device: Option<Arc<T>>,
}

impl<T: ?Sized> DeviceSession<T> {
    /// A slot holding an established session.
    #[must_use]
    pub const fn connected(device: Arc<T>) -> Self {
        Self {
            state: RwLock::new(SessionState {
                connected: true,
                device: Some(device),
            }),
        }
    }

    /// A slot for a device that has never been reached.
    #[must_use]
    pub const fn disconnected() -> Self {
        Self {
            state: RwLock::new(SessionState {
                connected: false,
                device: None,
            }),
        }
    }

    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.read().connected
    }

    /// The current device handle. May be a stale handle from a dead
    /// session when [`Self::is_connected`] is false — calls on it then
    /// fail with `NOT_CONNECTED` or a transport error, which is the
    /// honest outcome.
    #[must_use]
    pub fn device(&self) -> Option<Arc<T>> {
        self.read().device.clone()
    }

    /// Install a freshly established session and mark it connected.
    pub fn install(&self, device: Arc<T>) {
        let mut state = self.write();
        state.device = Some(device);
        state.connected = true;
    }

    /// Mark the session dead. The handle is deliberately kept — see the
    /// type-level docs.
    pub fn mark_disconnected(&self) {
        self.write().connected = false;
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, SessionState<T>> {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, SessionState<T>> {
        self.state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn disconnected_slot_has_no_device_and_reads_disconnected() {
        let session: DeviceSession<str> = DeviceSession::disconnected();
        assert!(!session.is_connected());
        assert!(session.device().is_none());
    }

    #[test]
    fn connected_slot_serves_the_device() {
        let session: DeviceSession<str> = DeviceSession::connected(Arc::from("handle"));
        assert!(session.is_connected());
        assert_eq!(session.device().as_deref(), Some("handle"));
    }

    #[test]
    fn mark_disconnected_keeps_the_stale_handle() {
        let session: DeviceSession<str> = DeviceSession::connected(Arc::from("stale"));
        session.mark_disconnected();
        assert!(!session.is_connected());
        assert_eq!(
            session.device().as_deref(),
            Some("stale"),
            "the dead session's handle must stay in place until replaced"
        );
    }

    #[test]
    fn install_replaces_the_handle_and_reconnects() {
        let session: DeviceSession<str> = DeviceSession::connected(Arc::from("old"));
        session.mark_disconnected();
        session.install(Arc::from("new"));
        assert!(session.is_connected());
        assert_eq!(session.device().as_deref(), Some("new"));
    }
}
