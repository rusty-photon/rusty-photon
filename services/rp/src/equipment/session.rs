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
/// step size. Holding it here rather than beside the session is what
/// keeps the two in step: a handle only enters through
/// [`Self::install`], which takes the metadata read from that same
/// session, so no writer can publish one half of a pair. It defaults
/// to `()` for the kinds that cache nothing.
///
/// [`Self::snapshot`] is the paired read, and the only one that cannot
/// straddle an install — a caller that needs a handle *and* its
/// metadata must take it, because [`Self::device`] and
/// [`Self::metadata`] take their own guards and a re-establish landing
/// between two such calls would hand back halves of two sessions.
/// They remain for the callers that genuinely want one half: the
/// health check and the cooler loop need no metadata, and the optics
/// and binning readers need no handle.
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

    /// The current device handle, for a caller that needs no metadata
    /// to go with it — pair the two with [`Self::snapshot`] instead of
    /// following this with [`Self::metadata`].
    ///
    /// May be a stale handle from a dead session when
    /// [`Self::is_connected`] is false — calls on it then fail with
    /// `NOT_CONNECTED` or a transport error, which is the honest
    /// outcome.
    #[must_use]
    pub fn device(&self) -> Option<Arc<T>> {
        self.read().device.clone()
    }

    /// The metadata of the session currently installed, for a caller
    /// that needs no handle to go with it — pair the two with
    /// [`Self::snapshot`] rather than with [`Self::device`].
    #[must_use]
    pub fn metadata(&self) -> M
    where
        M: Clone,
    {
        self.read().metadata.clone()
    }

    /// The handle and the metadata of one session, taken together
    /// under one guard.
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
    /// A slot for a device that has never been reached: no handle, and
    /// metadata nothing has read yet.
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

impl<T: ?Sized> DeviceSession<T, ()> {
    /// A slot holding an established session, for a device kind that
    /// caches nothing about it.
    ///
    /// Deliberately not offered for a metadata-bearing slot, even
    /// though `M: Default` would make it compile: it would install a
    /// live handle beside default metadata — a camera with no sensor
    /// geometry, reported as connected — which is the pairing contract
    /// broken at the constructor. Those kinds call
    /// [`Self::connected_with`] and say what they read.
    #[must_use]
    pub const fn connected(device: Arc<T>) -> Self {
        Self::connected_with(device, ())
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

    /// The assertion the sequential test above cannot make: under a
    /// concurrent installer, a snapshot must never return one
    /// session's handle beside the other's metadata. Two independent
    /// locks — the shape this slot replaced — tear here within a few
    /// iterations; one lock cannot tear at all.
    #[test]
    fn a_snapshot_never_straddles_a_concurrent_install() {
        const ROUNDS: usize = 20_000;
        let session: Arc<DeviceSession<str, u32>> =
            Arc::new(DeviceSession::connected_with(Arc::from("old"), 1));

        let installer = Arc::clone(&session);
        let writer = std::thread::spawn(move || {
            let mut fresh = true;
            for _ in 0..ROUNDS {
                if fresh {
                    installer.install(Arc::from("new"), 2);
                } else {
                    installer.install(Arc::from("old"), 1);
                }
                fresh = !fresh;
            }
        });

        for _ in 0..ROUNDS {
            let (handle, metadata) = session.snapshot().unwrap();
            let expected = if handle.as_ref() == "new" { 2 } else { 1 };
            assert_eq!(
                metadata, expected,
                "handle {handle:?} was served with another session's metadata"
            );
        }
        writer.join().unwrap();
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
