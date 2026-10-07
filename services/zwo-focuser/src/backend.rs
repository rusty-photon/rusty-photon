//! The SDK seam: a thin trait over the blocking `zwo-rs` `Focuser` surface the
//! ASCOM device drives, plus a production wrapper and a test mock.
//!
//! Why a seam: it (1) collapses [`zwo_rs::Error`] into a typed [`BackendError`]
//! at one boundary, (2) lets the ASCOM device hold an `Arc<dyn FocuserHandle>`
//! so unit tests can substitute a mock that forces paths the `zwo-rs`
//! simulation cannot, and (3) keeps the open/close lifecycle in one place.
//! `zwo-rs`'s `Focuser` is RAII (open = [`zwo_rs::FocuserList::open_focuser`],
//! close = drop) and `Send + !Sync`, so the production handle keeps it behind a
//! `parking_lot::Mutex` — mirroring `zwo-camera`'s `CameraHandle`/
//! `ZwoCameraHandle` seam. The handle also owns the departure check (C5): a
//! failed call asks whether the EAF is still there, and an open finds the EAF
//! by its serial.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use tracing::{debug, warn};
use zwo_rs::FocuserInfo;

/// A `zwo-rs` SDK call failed.
///
/// Carries the underlying message, and whether the failure means the EAF has
/// left the bus, which is a disconnect wherever it is met (C5). The ASCOM
/// device decides the `ASCOMError` per call site.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct BackendError {
    message: String,
    kind: BackendErrorKind,
}

/// What a [`BackendError`] says about the EAF, beyond its message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendErrorKind {
    /// The EAF has left the bus (C5): the handle judged the failure, met on
    /// its open session, to mean so.
    Departed,
    /// Any other failure.
    Other,
}

/// Collapse a [`zwo_rs::Error`] into the typed seam error. The SDK's code
/// alone does not say whether the EAF has left: `EAF_ERROR_REMOVED` from an
/// open means an inaccessible hidraw node. Only the handle, which knows the
/// session was open, can relabel a failure [`BackendErrorKind::Departed`].
impl From<zwo_rs::Error> for BackendError {
    fn from(err: zwo_rs::Error) -> Self {
        Self::new(err.to_string())
    }
}

impl BackendError {
    /// A failure that says nothing about the EAF's presence.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: BackendErrorKind::Other,
        }
    }

    fn closed() -> Self {
        Self::new("focuser not open")
    }

    /// What this failure says about the EAF.
    #[must_use]
    pub const fn kind(&self) -> BackendErrorKind {
        self.kind
    }

    /// Whether the EAF has left the bus (C5).
    #[must_use]
    pub fn is_departed(&self) -> bool {
        self.kind == BackendErrorKind::Departed
    }

    /// The same failure, now known to mean the EAF has left the bus (C5).
    #[must_use]
    pub fn departed(self) -> Self {
        Self {
            kind: BackendErrorKind::Departed,
            ..self
        }
    }

    /// The failure's message, for the ASCOM error a client sees.
    #[must_use]
    pub fn into_message(self) -> String {
        self.message
    }
}

pub type BackendResult<T> = std::result::Result<T, BackendError>;

/// What a [`FocuserHandle`] holds, read in one step (see
/// [`FocuserHandle::session`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// No EAF is open.
    Closed,
    /// An EAF is open, and no failure on it has meant it left the bus.
    Live,
    /// An EAF is open, but a failure on it has meant it left the bus since the
    /// open (C5). It stays held until a close.
    Lost,
}

/// The blocking focuser operations the ASCOM `Focuser` device drives. Every
/// method is synchronous (the SDK is blocking C FFI); the device offloads each
/// call onto `spawn_blocking`.
pub trait FocuserHandle: std::fmt::Debug + Send + Sync {
    /// The stable ASCOM `UniqueID` (serial-derived; read once at enumeration).
    fn unique_id(&self) -> String;

    /// The focuser's enumeration [`FocuserInfo`] (cached; no open required).
    fn info(&self) -> FocuserInfo;

    /// The working travel limit (`EAFGetMaxStep`; cached, read during
    /// enumeration's brief open). The firmware stops at this limit even when
    /// a move targets beyond it, so all range validation uses this — NOT
    /// [`FocuserInfo::max_step`] (`EAF_INFO::MaxStep`), which is only the
    /// fixed ceiling the limit can be raised to.
    fn max_step(&self) -> u32;

    /// What the handle holds: no EAF, a live session, or a session whose EAF
    /// has left the bus (C5). Read in one step under the lock an open and a
    /// close take, so the answer never mixes two sessions. Answered from the
    /// handle's own state, never from the SDK.
    fn session(&self) -> SessionState;
    /// Find this handle's EAF on the bus by its serial and open it (a no-op
    /// when already open, lost or not; C1, C5).
    ///
    /// # Errors
    ///
    /// Returns a [`BackendError`] if the EAF is not on the bus or the SDK
    /// cannot open it; the handle stays closed.
    fn open(&self) -> BackendResult<()>;
    /// Close the EAF (a no-op when already closed), whatever the SDK says
    /// about one that has left the bus (C5).
    ///
    /// # Errors
    ///
    /// Never fails in either shipped handle: the production close is a drop
    /// (`EAFClose` has no error path here), and the mock only clears a flag.
    fn close(&self) -> BackendResult<()>;

    /// Current step position (`EAFGetPosition`; no moving sentinel).
    ///
    /// # Errors
    ///
    /// Returns `focuser not open` if the handle is closed, or the SDK's error,
    /// relabelled [`BackendErrorKind::Departed`] when it means the EAF left.
    fn position(&self) -> BackendResult<i32>;
    /// Whether a move is in progress (`EAFIsMoving`).
    ///
    /// # Errors
    ///
    /// As [`Self::position`].
    fn is_moving(&self) -> BackendResult<bool>;
    /// Start an absolute move to `position` (`EAFMove`).
    ///
    /// # Errors
    ///
    /// As [`Self::position`] — the SDK's refusal of a move while one is in
    /// progress (M8) included.
    fn move_to(&self, position: i32) -> BackendResult<()>;
    /// Stop an in-progress move (`EAFStop`); a no-op when idle.
    ///
    /// # Errors
    ///
    /// As [`Self::position`].
    fn stop(&self) -> BackendResult<()>;
    /// The live temperature-sensor reading in degrees Celsius (`EAFGetTemp`).
    ///
    /// # Errors
    ///
    /// As [`Self::position`].
    fn temperature(&self) -> BackendResult<f32>;
    /// Whether the focuser moves along the reverse direction.
    ///
    /// # Errors
    ///
    /// As [`Self::position`].
    fn reverse(&self) -> BackendResult<bool>;
    /// Set whether the focuser moves along the reverse direction.
    ///
    /// # Errors
    ///
    /// As [`Self::position`].
    fn set_reverse(&self, reverse: bool) -> BackendResult<()>;
}

// --- production wrapper over zwo-rs ---------------------------------------------

/// The SDK focuser IDs a service's [`ZwoFocuserHandle`]s hold open.
///
/// One set is shared by every handle the service builds. An open looking for
/// its EAF skips them: reading a candidate's serial opens it, and the close
/// after the read would close another device's session (C5).
pub type HeldFocusers = Arc<Mutex<BTreeSet<i32>>>;

/// Production [`FocuserHandle`] over a real (or `zwo-rs`-simulated) EAF.
///
/// Holds the [`zwo_rs::Sdk`] (a ZST) and the EAF's identity (its serial and
/// startup `index`), so it can find and re-open the RAII [`zwo_rs::Focuser`] on
/// connect; the open handle lives behind a `Mutex<Option<…>>` because
/// `Focuser` is `Send + !Sync`.
#[derive(Debug)]
pub struct ZwoFocuserHandle {
    sdk: zwo_rs::Sdk,
    index: usize,
    info: FocuserInfo,
    max_step: u32,
    unique_id: String,
    /// The hardware serial read at enumeration; `None` for an EAF that
    /// exposes none (`noserial-{index}`), which an open takes by position.
    serial: Option<String>,
    held: HeldFocusers,
    focuser: Mutex<Option<zwo_rs::Focuser>>,
    /// Set when a failure on the open EAF turns out to mean it has left the
    /// bus (C5); cleared by the close that lets that EAF go.
    ///
    /// Written only while holding [`Self::focuser`] — the mark under the lock
    /// the failing call ran under (see [`Self::judge`]), the clear under the
    /// lock a close empties the slot under — so it always describes
    /// the EAF in the slot, never one a reconnect has replaced.
    /// [`FocuserHandle::session`] reads it under that lock too, beside the
    /// slot, so the two are never read from different sessions.
    lost: AtomicBool,
}

impl ZwoFocuserHandle {
    /// Build a handle for the EAF found at enumeration `index`, with its
    /// cached [`FocuserInfo`], working travel limit (`EAFGetMaxStep`), the
    /// serial-derived `unique_id`, and the hardware `serial` (`None` when it
    /// has none) — all read at enumeration. `held` is the set every handle of
    /// the service shares.
    #[must_use]
    pub const fn new(
        sdk: zwo_rs::Sdk,
        index: usize,
        info: FocuserInfo,
        max_step: u32,
        unique_id: String,
        serial: Option<String>,
        held: HeldFocusers,
    ) -> Self {
        Self {
            sdk,
            index,
            info,
            max_step,
            unique_id,
            serial,
            held,
            focuser: Mutex::new(None),
            lost: AtomicBool::new(false),
        }
    }

    /// Run one SDK call on the open focuser and judge its failure (C5), all
    /// under one lock acquisition. Returns the closed error when the handle
    /// slot is empty.
    #[expect(
        clippy::significant_drop_tightening,
        reason = "the focuser reference borrows the handle guard to the judgement's end; the guard scope is already minimal"
    )]
    fn with_focuser<T>(
        &self,
        f: impl FnOnce(&zwo_rs::Focuser) -> zwo_rs::Result<T>,
    ) -> BackendResult<T> {
        let guard = self.focuser.lock();
        let focuser = guard.as_ref().ok_or_else(BackendError::closed)?;
        // Still under the guard the call ran under (C5).
        self.judge(focuser, f(focuser))
    }

    /// Ask, on a failure, whether `focuser` has left the bus (C5). A failure
    /// that does mean so marks the session lost, logged at `warn` the first
    /// time, and comes back relabelled [`BackendErrorKind::Departed`]; any
    /// other result comes back as it was.
    ///
    /// Call it while still holding [`Self::focuser`] from the SDK call that
    /// produced `result`: an open and a close change the EAF only under that
    /// lock, so the answer and the mark describe the EAF that failed. Made
    /// after the guard was dropped, they could land on a session a reconnect
    /// opened in between, ending one nothing is wrong with.
    fn judge<T>(&self, focuser: &zwo_rs::Focuser, result: zwo_rs::Result<T>) -> BackendResult<T> {
        let failure = match result {
            Ok(value) => return Ok(value),
            Err(failure) => failure,
        };
        let left = self.left_the_bus(focuser, &failure);
        let failure = BackendError::from(failure);
        if !left {
            return Err(failure);
        }
        if !self.lost.swap(true, Ordering::SeqCst) {
            warn!(
                focuser = %self.unique_id,
                failure = %failure,
                "the focuser has left the bus; it reads disconnected until a client releases it"
            );
        }
        Err(failure.departed())
    }

    /// Whether `failure`, met on the open `focuser`, means the EAF has left
    /// the bus. The EAF SDK says so itself (`REMOVED`, and `INVALID_ID` or
    /// `CLOSED` once a rescan has run), so those answer at once. Any other
    /// failure asks with one more session read on the same ID
    /// ([`zwo_rs::Focuser::still_connected`]), which catches a call already in
    /// flight when the EAF left. An answer that says neither leaves the
    /// failure standing.
    fn left_the_bus(&self, focuser: &zwo_rs::Focuser, failure: &zwo_rs::Error) -> bool {
        if matches!(failure, zwo_rs::Error::Eaf(code) if code.left_the_bus()) {
            return true;
        }
        match focuser.still_connected() {
            Ok(present) => !present,
            Err(e) => {
                debug!(
                    focuser = %self.unique_id,
                    failure = %failure,
                    error = %e,
                    "presence check answered neither way; the failure stands"
                );
                false
            }
        }
    }

    /// Find this handle's EAF on the bus as it is now, open it, and reserve
    /// it in [`HeldFocusers`] (C5).
    ///
    /// A rescan can list a returned EAF under a new ID and renumber the SDK's
    /// list, so the index read at startup may name another EAF, or none, once
    /// one has run. This rescans, opens in turn the listed EAFs no sibling
    /// device holds, the startup index first, and keeps the first whose serial
    /// matches. An EAF without a serial is taken by position alone.
    ///
    /// All of that is one step. The held set is locked from the first look at
    /// it to the reservation, and the SDK's focuser list from the rescan to
    /// the open, so another device's rescan cannot renumber the list between
    /// the choice and the open, and a sibling's open cannot pick the same EAF.
    ///
    /// Lock order: the handle's own [`Self::focuser`] (held by the caller),
    /// then the held set, then the focuser list. A close takes the first two,
    /// so no cycle forms.
    fn open_by_identity(&self) -> BackendResult<zwo_rs::Focuser> {
        let mut held = self.held.lock();
        let focuser = self.open_listed(&held)?;
        held.insert(focuser.id());
        drop(held);
        Ok(focuser)
    }

    /// The focuser-list half of [`Self::open_by_identity`]: rescan, then open
    /// this handle's EAF among those `held` leaves free, under one hold of the
    /// list.
    fn open_listed(&self, held: &BTreeSet<i32>) -> BackendResult<zwo_rs::Focuser> {
        let list = self.sdk.focuser_list();
        let mut candidates: Vec<usize> = list
            .rescan()?
            .iter()
            .enumerate()
            .filter(|(_, info)| !held.contains(&info.id))
            .map(|(index, _)| index)
            .collect();
        candidates.sort_by_key(|&index| index != self.index);
        let mut last_failure = None;
        for index in candidates {
            match list.open_focuser(index) {
                Ok(focuser) if self.is_this_focuser(&focuser, index) => return Ok(focuser),
                // Not this one: dropping it closes it again.
                Ok(_) => {}
                Err(e) => {
                    debug!(focuser = %self.unique_id, index, error = %e, "candidate focuser did not open");
                    last_failure = Some(e);
                }
            }
        }
        drop(list);
        debug!(focuser = %self.unique_id, "focuser not on the bus");
        Err(last_failure.map_or_else(
            || BackendError::new("focuser not on the bus"),
            BackendError::from,
        ))
    }

    /// Whether the open `focuser` at `index` is this handle's: its serial
    /// matches, or this handle has no serial to match. A focuser whose serial
    /// cannot be read is not this one.
    fn is_this_focuser(&self, focuser: &zwo_rs::Focuser, index: usize) -> bool {
        let Some(want) = &self.serial else {
            return true;
        };
        match focuser.serial() {
            Ok(serial) => serial == *want,
            Err(e) => {
                debug!(focuser = %self.unique_id, index, error = %e, "candidate focuser's serial unreadable; skipped");
                false
            }
        }
    }
}

impl FocuserHandle for ZwoFocuserHandle {
    fn unique_id(&self) -> String {
        self.unique_id.clone()
    }

    fn info(&self) -> FocuserInfo {
        self.info.clone()
    }

    fn max_step(&self) -> u32 {
        self.max_step
    }

    fn session(&self) -> SessionState {
        let guard = self.focuser.lock();
        let session = if guard.is_none() {
            SessionState::Closed
        } else if self.lost.load(Ordering::SeqCst) {
            SessionState::Lost
        } else {
            SessionState::Live
        };
        drop(guard);
        session
    }

    fn open(&self) -> BackendResult<()> {
        let mut guard = self.focuser.lock();
        if guard.is_none() {
            // The lost mark is already clear: only a call on an open session
            // sets it, and the close that emptied the slot cleared it.
            *guard = Some(self.open_by_identity()?);
        }
        drop(guard);
        Ok(())
    }

    fn close(&self) -> BackendResult<()> {
        let mut guard = self.focuser.lock();
        if let Some(focuser) = guard.take() {
            // The reservation outlives the close: a sibling's open locks the
            // held set first, so it cannot take this ID until the EAF is
            // closed. Otherwise it could open the ID afresh in between, and
            // this drop, which is `EAFClose`, would close its session (C5).
            // The close goes ahead whatever the SDK makes of an EAF that has
            // left the bus.
            let mut held = self.held.lock();
            let id = focuser.id();
            drop(focuser);
            held.remove(&id);
            drop(held);
        }
        self.lost.store(false, Ordering::SeqCst);
        drop(guard);
        Ok(())
    }

    fn position(&self) -> BackendResult<i32> {
        self.with_focuser(zwo_rs::Focuser::position)
    }

    fn is_moving(&self) -> BackendResult<bool> {
        self.with_focuser(zwo_rs::Focuser::is_moving)
    }

    fn move_to(&self, position: i32) -> BackendResult<()> {
        self.with_focuser(|focuser| focuser.move_to(position))
    }

    fn stop(&self) -> BackendResult<()> {
        self.with_focuser(zwo_rs::Focuser::stop)
    }

    fn temperature(&self) -> BackendResult<f32> {
        self.with_focuser(zwo_rs::Focuser::temperature)
    }

    fn reverse(&self) -> BackendResult<bool> {
        self.with_focuser(zwo_rs::Focuser::reverse)
    }

    fn set_reverse(&self, reverse: bool) -> BackendResult<()> {
        self.with_focuser(|focuser| focuser.set_reverse(reverse))
    }
}

// --- test mock -----------------------------------------------------------------

/// Exercise the *production* [`ZwoFocuserHandle`] against the `zwo-rs`
/// simulation backend (the mock seam below covers the device logic; this
/// covers the real SDK wrapper that the BDD suite otherwise reaches only via
/// the spawned binary).
#[cfg(all(test, feature = "simulation"))]
#[cfg_attr(coverage_nightly, coverage(off))]
#[allow(clippy::expect_used)]
mod handle_tests {
    use super::*;

    const SIM_SERIAL: &str = "2a3b4c5d6e7f8091";

    /// A departure file in a directory of its own: `zwo-rs` keeps what the
    /// rescans made of each file process-wide, so no two tests may share one.
    fn departure_path() -> (tempfile::TempDir, std::path::PathBuf) {
        let root = std::env::var_os("TEST_TMPDIR")
            .map_or_else(std::env::temp_dir, std::path::PathBuf::from);
        let dir = tempfile::Builder::new()
            .prefix("zwo-focuser-departure-")
            .tempdir_in(root)
            .expect("temp dir");
        let path = dir.path().join("departed");
        (dir, path)
    }

    fn sim_handle_on(
        sdk: zwo_rs::Sdk,
        serial: Option<&str>,
        held: HeldFocusers,
    ) -> ZwoFocuserHandle {
        let info = sdk.focusers().expect("enumerate")[0].clone();
        let max_step = sdk
            .open_focuser(0)
            .expect("open")
            .max_step()
            .expect("working travel limit");
        ZwoFocuserHandle::new(
            sdk,
            0,
            info,
            max_step,
            format!("ZWO:Sim:{}", serial.unwrap_or("noserial-0")),
            serial.map(str::to_owned),
            held,
        )
    }

    fn sim_handle() -> ZwoFocuserHandle {
        sim_handle_on(
            zwo_rs::Sdk::new().expect("simulation SDK"),
            Some(SIM_SERIAL),
            HeldFocusers::default(),
        )
    }

    fn departing_handle(departure: &std::path::Path) -> ZwoFocuserHandle {
        sim_handle_on(
            zwo_rs::Sdk::new()
                .expect("simulation SDK")
                .with_departure_file(departure),
            Some(SIM_SERIAL),
            HeldFocusers::default(),
        )
    }

    #[test]
    fn production_handle_round_trips_against_the_sim_sdk() {
        let handle = sim_handle();
        assert_eq!(handle.unique_id(), "ZWO:Sim:2a3b4c5d6e7f8091");
        // The EAF_INFO ceiling and the working travel limit stay distinct.
        assert_eq!(handle.info().max_step, 600_000);
        assert_eq!(handle.max_step(), 60_000);
        // Open/close lifecycle.
        assert_eq!(handle.session(), SessionState::Closed);
        handle.open().unwrap();
        assert_eq!(handle.session(), SessionState::Live);
        assert_eq!(handle.position().unwrap(), 0);
        handle.move_to(500).unwrap();
        assert!(handle.is_moving().unwrap());
        assert!(!handle.is_moving().unwrap());
        assert_eq!(handle.position().unwrap(), 500);
        let _ = handle.temperature().unwrap();
        assert!(!handle.reverse().unwrap());
        handle.set_reverse(true).unwrap();
        assert!(handle.reverse().unwrap());
        handle.close().unwrap();
        assert_eq!(handle.session(), SessionState::Closed);
    }

    #[test]
    fn operations_on_a_closed_handle_are_rejected() {
        let handle = sim_handle();
        assert_eq!(
            handle.position().unwrap_err().to_string(),
            "focuser not open"
        );
        assert_eq!(
            handle.move_to(0).unwrap_err().to_string(),
            "focuser not open"
        );
    }

    #[test]
    fn a_failure_that_says_the_focuser_left_marks_the_session_lost() {
        let (_dir, departure) = departure_path();
        let handle = departing_handle(&departure);
        handle.open().unwrap();
        std::fs::write(&departure, b"").unwrap();
        let failure = handle.position().unwrap_err();
        assert!(failure.is_departed(), "{failure}");
        assert_eq!(handle.session(), SessionState::Lost);
        // Every later call says the same, and the session stays held.
        assert!(handle.is_moving().unwrap_err().is_departed());
        assert_eq!(handle.session(), SessionState::Lost);
    }

    #[test]
    fn a_failure_on_a_focuser_still_there_stands() {
        let (_dir, departure) = departure_path();
        let handle = departing_handle(&departure);
        handle.open().unwrap();
        handle.move_to(1000).unwrap();
        // The SDK refuses a move while one runs (M8); the EAF is still there.
        let failure = handle.move_to(2000).unwrap_err();
        assert_eq!(failure.kind(), BackendErrorKind::Other, "{failure}");
        assert_eq!(handle.session(), SessionState::Live);
    }

    /// A failure that names no departure asks once more on the same session:
    /// the call already in flight when the EAF left. The simulation answers
    /// `REMOVED` to every call once the EAF is gone, so the failure is handed
    /// to the judgement directly.
    #[test]
    fn any_other_failure_asks_and_a_focuser_found_gone_is_lost() {
        let (_dir, departure) = departure_path();
        let handle = departing_handle(&departure);
        handle.open().unwrap();
        let in_flight = || Err::<(), _>(zwo_rs::Error::Eaf(zwo_rs::EafError::GeneralError));

        let guard = handle.focuser.lock();
        let focuser = guard.as_ref().expect("open");
        let present = handle.judge(focuser, in_flight()).unwrap_err();
        assert_eq!(
            present.kind(),
            BackendErrorKind::Other,
            "still there: it stands"
        );
        assert!(!handle.lost.load(Ordering::SeqCst));

        std::fs::write(&departure, b"").unwrap();
        let gone = handle.judge(focuser, in_flight()).unwrap_err();
        assert!(gone.is_departed(), "{gone}");
        drop(guard);
        assert_eq!(handle.session(), SessionState::Lost);
    }

    #[test]
    fn a_lost_session_closes_cleanly() {
        let (_dir, departure) = departure_path();
        let handle = departing_handle(&departure);
        handle.open().unwrap();
        std::fs::write(&departure, b"").unwrap();
        handle.position().unwrap_err();
        handle.close().unwrap();
        assert_eq!(handle.session(), SessionState::Closed);
    }

    #[test]
    fn an_open_finds_the_focuser_only_once_it_is_back() {
        let (_dir, departure) = departure_path();
        let handle = departing_handle(&departure);
        handle.open().unwrap();
        std::fs::write(&departure, b"").unwrap();
        handle.position().unwrap_err();
        handle.close().unwrap();

        handle.open().unwrap_err();
        assert_eq!(handle.session(), SessionState::Closed);

        std::fs::remove_file(&departure).unwrap();
        handle.open().unwrap();
        assert_eq!(handle.session(), SessionState::Live);
        handle.position().unwrap();
    }

    #[test]
    fn an_open_skips_a_focuser_whose_serial_is_not_this_ones() {
        let handle = sim_handle_on(
            zwo_rs::Sdk::new().expect("simulation SDK"),
            Some("ffffffffffffffff"),
            HeldFocusers::default(),
        );
        handle.open().unwrap_err();
        assert_eq!(handle.session(), SessionState::Closed);
    }

    #[test]
    fn two_handles_never_hold_one_focuser() {
        // Two devices of one service share the held set; the simulated bus has
        // one EAF. Without a serial, each would take any free one.
        let held = HeldFocusers::default();
        let first = sim_handle_on(zwo_rs::Sdk::new().expect("sdk"), None, Arc::clone(&held));
        let second = sim_handle_on(zwo_rs::Sdk::new().expect("sdk"), None, Arc::clone(&held));
        first.open().unwrap();
        second.open().unwrap_err();
        assert_eq!(second.session(), SessionState::Closed);
        first.close().unwrap();
        second.open().unwrap();
        assert_eq!(second.session(), SessionState::Live);
    }
}

/// A configurable in-memory [`FocuserHandle`] for the crate's unit tests, so
/// the device logic — including paths the `zwo-rs` simulation cannot force —
/// is exercised without hardware.
#[cfg(test)]
pub(crate) mod mock {
    use super::*;
    use std::sync::atomic::{AtomicI32, AtomicUsize};

    fn default_info() -> FocuserInfo {
        FocuserInfo {
            id: 0,
            name: "EAF-Mock".to_string(),
            max_step: 600_000,
        }
    }

    fn removed() -> BackendError {
        BackendError::new("focuser removed").departed()
    }

    #[derive(Debug)]
    pub struct MockFocuserHandle {
        info: FocuserInfo,
        max_step: u32,
        open: AtomicBool,
        lost: AtomicBool,
        /// The EAF is off the bus: an open fails, and every call on an open
        /// session fails as a departure and marks it lost, as the production
        /// handle's check would.
        gone: AtomicBool,
        /// One shot: the next call fails as a departure without marking the
        /// session lost, as when a reconnect clears the mark between the
        /// failing call and the device's look at the session (C5).
        departure_unmarked_once: AtomicBool,
        opens: AtomicUsize,
        /// Closes begun, held ones included.
        closes: AtomicUsize,
        /// While set, a close waits inside, before it lets the EAF go, until
        /// [`Self::release_closes`].
        closes_held: Mutex<bool>,
        closes_released: parking_lot::Condvar,
        position: AtomicI32,
        moving: AtomicBool,
        reverse: AtomicBool,
        temperature: Mutex<f32>,
        /// E-style injection: make the next `temperature()` call fail at the SDK.
        pub fail_temperature: AtomicBool,
    }

    impl Default for MockFocuserHandle {
        fn default() -> Self {
            Self {
                info: default_info(),
                max_step: 60_000,
                open: AtomicBool::new(false),
                lost: AtomicBool::new(false),
                gone: AtomicBool::new(false),
                departure_unmarked_once: AtomicBool::new(false),
                opens: AtomicUsize::new(0),
                closes: AtomicUsize::new(0),
                closes_held: Mutex::new(false),
                closes_released: parking_lot::Condvar::new(),
                position: AtomicI32::new(0),
                moving: AtomicBool::new(false),
                reverse: AtomicBool::new(false),
                temperature: Mutex::new(20.0),
                fail_temperature: AtomicBool::new(false),
            }
        }
    }

    impl MockFocuserHandle {
        /// Present a focuser with a specific working travel limit
        /// (bounds-validation tests).
        pub fn with_max_step(mut self, max_step: u32) -> Self {
            self.max_step = max_step;
            self
        }

        /// Take the EAF off the bus (C5).
        pub fn leave_bus(&self) {
            self.gone.store(true, Ordering::SeqCst);
        }

        /// Put the EAF back on the bus. A lost session stays lost.
        pub fn return_to_bus(&self) {
            self.gone.store(false, Ordering::SeqCst);
        }

        /// Make the next call fail as a departure without marking the session.
        pub fn answer_one_departure_unmarked(&self) {
            self.departure_unmarked_once.store(true, Ordering::SeqCst);
        }

        /// How many opens actually opened an EAF.
        pub fn opens(&self) -> usize {
            self.opens.load(Ordering::SeqCst)
        }

        /// How many closes have begun, held ones included.
        pub fn closes(&self) -> usize {
            self.closes.load(Ordering::SeqCst)
        }

        /// Hold every close inside, before it lets the EAF go.
        pub fn hold_closes(&self) {
            *self.closes_held.lock() = true;
        }

        /// Let held closes, and later ones, finish.
        pub fn release_closes(&self) {
            *self.closes_held.lock() = false;
            self.closes_released.notify_all();
        }

        /// What a call on the session meets before it reaches the "SDK".
        fn answer(&self) -> BackendResult<()> {
            if !self.open.load(Ordering::SeqCst) {
                return Err(BackendError::closed());
            }
            if self.departure_unmarked_once.swap(false, Ordering::SeqCst) {
                return Err(removed());
            }
            if self.gone.load(Ordering::SeqCst) || self.lost.load(Ordering::SeqCst) {
                self.lost.store(true, Ordering::SeqCst);
                return Err(removed());
            }
            Ok(())
        }
    }

    impl FocuserHandle for MockFocuserHandle {
        fn unique_id(&self) -> String {
            "ZWO:EAF-Mock:2a3b4c5d6e7f8091".to_string()
        }

        fn info(&self) -> FocuserInfo {
            self.info.clone()
        }

        fn max_step(&self) -> u32 {
            self.max_step
        }

        fn session(&self) -> SessionState {
            if !self.open.load(Ordering::SeqCst) {
                SessionState::Closed
            } else if self.lost.load(Ordering::SeqCst) {
                SessionState::Lost
            } else {
                SessionState::Live
            }
        }

        fn open(&self) -> BackendResult<()> {
            if self.open.load(Ordering::SeqCst) {
                return Ok(());
            }
            if self.gone.load(Ordering::SeqCst) {
                return Err(BackendError::new("focuser not on the bus"));
            }
            self.lost.store(false, Ordering::SeqCst);
            self.open.store(true, Ordering::SeqCst);
            self.opens.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn close(&self) -> BackendResult<()> {
            self.closes.fetch_add(1, Ordering::SeqCst);
            let mut held = self.closes_held.lock();
            while *held {
                self.closes_released.wait(&mut held);
            }
            drop(held);
            self.open.store(false, Ordering::SeqCst);
            self.lost.store(false, Ordering::SeqCst);
            Ok(())
        }

        fn position(&self) -> BackendResult<i32> {
            self.answer()?;
            Ok(self.position.load(Ordering::SeqCst))
        }

        fn is_moving(&self) -> BackendResult<bool> {
            self.answer()?;
            // Settle one poll after the move, mirroring `zwo-rs`'s simulation.
            Ok(self.moving.swap(false, Ordering::SeqCst))
        }

        fn move_to(&self, position: i32) -> BackendResult<()> {
            self.answer()?;
            self.position.store(position, Ordering::SeqCst);
            self.moving.store(true, Ordering::SeqCst);
            Ok(())
        }

        fn stop(&self) -> BackendResult<()> {
            self.answer()?;
            self.moving.store(false, Ordering::SeqCst);
            Ok(())
        }

        fn temperature(&self) -> BackendResult<f32> {
            self.answer()?;
            if self.fail_temperature.load(Ordering::SeqCst) {
                return Err(BackendError::new("simulated temperature failure"));
            }
            Ok(*self.temperature.lock())
        }

        fn reverse(&self) -> BackendResult<bool> {
            self.answer()?;
            Ok(self.reverse.load(Ordering::SeqCst))
        }

        fn set_reverse(&self, reverse: bool) -> BackendResult<()> {
            self.answer()?;
            self.reverse.store(reverse, Ordering::SeqCst);
            Ok(())
        }
    }
}
