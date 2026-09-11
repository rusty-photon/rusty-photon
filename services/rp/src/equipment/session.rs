//! The live Alpaca session slot every equipment entry holds.

use std::sync::{Arc, RwLock};

/// The per-entry device-session slot.
///
/// Device handle, the `connected` flag the status API reports, and the
/// session's connect-time metadata, in one slot so the reconnect
/// supervisor's updates are atomic with respect to readers (rp.md
/// § Device Session Recovery). The lock is held only to clone the
/// handle, copy the metadata or flip the flag — never across an await.
///
/// `M` is whatever the device kind reads once per session and serves
/// from memory afterwards: a camera's sensor geometry, a focuser's
/// step size. It defaults to `()` for the kinds that cache nothing.
/// Holding it here rather than beside the session is what makes a torn
/// pair unrepresentable: a handle only enters through
/// [`Self::install`], which takes the metadata read from that same
/// session, and [`Self::snapshot`] hands both out under one guard. A
/// caller therefore cannot pair one session's handle with another
/// session's metadata.
///
/// A disconnected slot keeps its stale handle and metadata until a
/// successful re-establish replaces the pair: concurrent callers then
/// see honest `NOT_CONNECTED` errors from the device rather than a
/// handle vanishing mid-operation.
pub struct DeviceSession<T: ?Sized, M = ()> {
    state: RwLock<SessionState<T, M>>,
}

struct SessionState<T: ?Sized, M> {
    connected: bool,
    device: Option<Arc<T>>,
    metadata: M,
}

impl<T: ?Sized, M> DeviceSession<T, M> {
    /// A slot holding an established session and the metadata read
    /// from it.
    #[must_use]
    pub const fn connected_with(device: Arc<T>, metadata: M) -> Self {
        Self {
            state: RwLock::new(SessionState {
                connected: true,
                device: Some(device),
                metadata,
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

    /// The metadata of the session currently installed, for a caller
    /// that needs no handle to go with it.
    #[must_use]
    pub fn metadata(&self) -> M
    where
        M: Clone,
    {
        self.read().metadata.clone()
    }

    /// The handle and the metadata of one session, taken together.
    ///
    /// The pairing is the point: a caller that reads the two
    /// separately could take a handle, have a re-establish land, and
    /// then read metadata describing a session its handle never
    /// belonged to.
    #[must_use]
    pub fn snapshot(&self) -> Option<(Arc<T>, M)>
    where
        M: Clone,
    {
        let state = self.read();
        state
            .device
            .clone()
            .map(|device| (device, state.metadata.clone()))
    }

    /// Install a freshly established session — its handle and the
    /// metadata read from it — and mark it connected.
    pub fn install(&self, device: Arc<T>, metadata: M) {
        let mut state = self.write();
        state.device = Some(device);
        state.metadata = metadata;
        state.connected = true;
    }

    /// Mark the session dead. The handle and its metadata are
    /// deliberately kept — see the type-level docs.
    pub fn mark_disconnected(&self) {
        self.write().connected = false;
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, SessionState<T, M>> {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, SessionState<T, M>> {
        self.state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl<T: ?Sized, M: Default> DeviceSession<T, M> {
    /// A slot holding an established session, for a device kind that
    /// caches nothing about it.
    #[must_use]
    pub fn connected(device: Arc<T>) -> Self {
        Self::connected_with(device, M::default())
    }

    /// A slot for a device that has never been reached.
    #[must_use]
    pub fn disconnected() -> Self {
        Self {
            state: RwLock::new(SessionState {
                connected: false,
                device: None,
                metadata: M::default(),
            }),
        }
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
        session.install(Arc::from("new"), ());
        assert!(session.is_connected());
        assert_eq!(session.device().as_deref(), Some("new"));
    }

    #[test]
    fn a_slot_serves_the_metadata_it_was_built_with() {
        let session: DeviceSession<str, u32> =
            DeviceSession::connected_with(Arc::from("handle"), 7);
        assert_eq!(session.metadata(), 7);
    }

    /// The whole point of holding the two together: an install is one
    /// step, so a snapshot can only ever return a handle beside the
    /// metadata read from that same session.
    #[test]
    fn a_snapshot_pairs_a_handle_with_its_own_sessions_metadata() {
        let session: DeviceSession<str, u32> = DeviceSession::connected_with(Arc::from("old"), 1);
        let (old_handle, old_metadata) = session.snapshot().unwrap();
        assert_eq!(old_handle.as_ref(), "old");
        assert_eq!(old_metadata, 1);

        session.install(Arc::from("new"), 2);
        let (new_handle, new_metadata) = session.snapshot().unwrap();
        assert_eq!(new_handle.as_ref(), "new");
        assert_eq!(
            new_metadata, 2,
            "a handle and its metadata are installed and served as one pair"
        );
    }

    #[test]
    fn a_slot_with_no_device_has_no_snapshot() {
        let session: DeviceSession<str, u32> = DeviceSession::disconnected();
        assert!(session.snapshot().is_none());
    }

    #[test]
    fn mark_disconnected_keeps_the_stale_metadata_with_its_handle() {
        let session: DeviceSession<str, u32> = DeviceSession::connected_with(Arc::from("stale"), 5);
        session.mark_disconnected();
        let (handle, metadata) = session
            .snapshot()
            .expect("a dead session keeps its handle until one replaces it");
        assert_eq!(handle.as_ref(), "stale");
        assert_eq!(
            metadata, 5,
            "metadata belongs to its handle, so it outlives the session the same way"
        );
    }
}
