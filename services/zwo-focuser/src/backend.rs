//! The SDK seam: a thin trait over the blocking `zwo-rs` `Focuser` surface the
//! ASCOM device drives, plus a production wrapper and a test mock.
//!
//! Why a seam: it (1) collapses [`zwo_rs::Error`] into a typed [`BackendError`]
//! at one boundary, (2) lets the ASCOM device hold an `Arc<dyn FocuserHandle>`
//! so unit tests can substitute a mock that forces paths the `zwo-rs`
//! simulation cannot, and (3) keeps the open/close lifecycle in one place.
//! `zwo-rs`'s `Focuser` is RAII (open = [`zwo_rs::Sdk::open_focuser`], close =
//! drop) and `Send + !Sync`, so the production handle keeps it behind a
//! `parking_lot::Mutex` and re-opens on connect from the cached enumeration
//! `index` — mirroring `zwo-camera`'s `CameraHandle`/`ZwoCameraHandle` seam.

use parking_lot::Mutex;
use zwo_rs::FocuserInfo;

/// A `zwo-rs` SDK call failed. Carries the underlying message; the ASCOM
/// device decides the `ASCOMError` per call site.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct BackendError(pub String);

/// Collapse a [`zwo_rs::Error`] into the typed seam error.
impl From<zwo_rs::Error> for BackendError {
    fn from(err: zwo_rs::Error) -> Self {
        Self(err.to_string())
    }
}

impl BackendError {
    fn closed() -> Self {
        Self("focuser not open".to_string())
    }
}

pub type BackendResult<T> = std::result::Result<T, BackendError>;

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

    fn is_open(&self) -> bool;
    fn open(&self) -> BackendResult<()>;
    fn close(&self) -> BackendResult<()>;

    /// Current step position (`EAFGetPosition`; no moving sentinel).
    fn position(&self) -> BackendResult<i32>;
    /// Whether a move is in progress (`EAFIsMoving`).
    fn is_moving(&self) -> BackendResult<bool>;
    /// Start an absolute move to `position` (`EAFMove`).
    fn move_to(&self, position: i32) -> BackendResult<()>;
    /// Stop an in-progress move (`EAFStop`); a no-op when idle.
    fn stop(&self) -> BackendResult<()>;
    /// The live temperature-sensor reading in degrees Celsius (`EAFGetTemp`).
    fn temperature(&self) -> BackendResult<f32>;
    /// Whether the focuser moves along the reverse direction.
    fn reverse(&self) -> BackendResult<bool>;
    /// Set whether the focuser moves along the reverse direction.
    fn set_reverse(&self, reverse: bool) -> BackendResult<()>;
}

// --- production wrapper over zwo-rs ---------------------------------------------

/// Production [`FocuserHandle`] over a real (or `zwo-rs`-simulated) EAF.
///
/// Holds the [`zwo_rs::Sdk`] (a ZST) and the enumeration `index` so it can
/// re-open the RAII [`zwo_rs::Focuser`] on connect; the open handle lives
/// behind a `Mutex<Option<…>>` because `Focuser` is `Send + !Sync`.
#[derive(Debug)]
pub struct ZwoFocuserHandle {
    sdk: zwo_rs::Sdk,
    index: usize,
    info: FocuserInfo,
    max_step: u32,
    unique_id: String,
    focuser: Mutex<Option<zwo_rs::Focuser>>,
}

impl ZwoFocuserHandle {
    /// Build a handle for the focuser at enumeration `index`, with its cached
    /// [`FocuserInfo`], working travel limit (`EAFGetMaxStep`), and the
    /// serial-derived `unique_id` — all read at enumeration.
    #[must_use]
    pub const fn new(
        sdk: zwo_rs::Sdk,
        index: usize,
        info: FocuserInfo,
        max_step: u32,
        unique_id: String,
    ) -> Self {
        Self {
            sdk,
            index,
            info,
            max_step,
            unique_id,
            focuser: Mutex::new(None),
        }
    }

    /// Borrow the open focuser for the closure's SDK work — a single call
    /// or a sequence that must share one lock acquisition. Returns the
    /// closed error when the handle slot is empty.
    #[expect(
        clippy::significant_drop_tightening,
        reason = "the focuser reference borrows the handle guard to the closure's end; the guard scope is already minimal"
    )]
    fn with_focuser<T>(
        &self,
        f: impl FnOnce(&zwo_rs::Focuser) -> BackendResult<T>,
    ) -> BackendResult<T> {
        let guard = self.focuser.lock();
        let focuser = guard.as_ref().ok_or_else(BackendError::closed)?;
        f(focuser)
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

    fn is_open(&self) -> bool {
        self.focuser.lock().is_some()
    }

    fn open(&self) -> BackendResult<()> {
        let mut guard = self.focuser.lock();
        if guard.is_none() {
            *guard = Some(self.sdk.open_focuser(self.index)?);
        }
        drop(guard);
        Ok(())
    }

    fn close(&self) -> BackendResult<()> {
        // Dropping the `Focuser` calls `EAFClose`.
        *self.focuser.lock() = None;
        Ok(())
    }

    fn position(&self) -> BackendResult<i32> {
        self.with_focuser(|focuser| Ok(focuser.position()?))
    }

    fn is_moving(&self) -> BackendResult<bool> {
        self.with_focuser(|focuser| Ok(focuser.is_moving()?))
    }

    fn move_to(&self, position: i32) -> BackendResult<()> {
        self.with_focuser(|focuser| Ok(focuser.move_to(position)?))
    }

    fn stop(&self) -> BackendResult<()> {
        self.with_focuser(|focuser| Ok(focuser.stop()?))
    }

    fn temperature(&self) -> BackendResult<f32> {
        self.with_focuser(|focuser| Ok(focuser.temperature()?))
    }

    fn reverse(&self) -> BackendResult<bool> {
        self.with_focuser(|focuser| Ok(focuser.reverse()?))
    }

    fn set_reverse(&self, reverse: bool) -> BackendResult<()> {
        self.with_focuser(|focuser| Ok(focuser.set_reverse(reverse)?))
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

    fn sim_handle() -> ZwoFocuserHandle {
        let sdk = zwo_rs::Sdk::new().expect("simulation SDK");
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
            "ZWO:Sim:2a3b4c5d6e7f8091".to_string(),
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
        assert!(!handle.is_open());
        handle.open().unwrap();
        assert!(handle.is_open());
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
        assert!(!handle.is_open());
    }

    #[test]
    fn operations_on_a_closed_handle_are_rejected() {
        let handle = sim_handle();
        assert_eq!(handle.position().unwrap_err().0, "focuser not open");
        assert_eq!(handle.move_to(0).unwrap_err().0, "focuser not open");
    }
}

/// A configurable in-memory [`FocuserHandle`] for the crate's unit tests, so
/// the device logic — including paths the `zwo-rs` simulation cannot force —
/// is exercised without hardware.
#[cfg(test)]
pub(crate) mod mock {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicI32};

    fn default_info() -> FocuserInfo {
        FocuserInfo {
            id: 0,
            name: "EAF-Mock".to_string(),
            max_step: 600_000,
        }
    }

    #[derive(Debug)]
    pub struct MockFocuserHandle {
        info: FocuserInfo,
        max_step: u32,
        open: AtomicBool,
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

        fn is_open(&self) -> bool {
            self.open.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn open(&self) -> BackendResult<()> {
            self.open.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }

        fn close(&self) -> BackendResult<()> {
            self.open.store(false, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }

        fn position(&self) -> BackendResult<i32> {
            Ok(self.position.load(std::sync::atomic::Ordering::SeqCst))
        }

        fn is_moving(&self) -> BackendResult<bool> {
            // Settle one poll after the move, mirroring `zwo-rs`'s simulation.
            let was_moving = self.moving.swap(false, std::sync::atomic::Ordering::SeqCst);
            Ok(was_moving)
        }

        fn move_to(&self, position: i32) -> BackendResult<()> {
            self.position
                .store(position, std::sync::atomic::Ordering::SeqCst);
            self.moving.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }

        fn stop(&self) -> BackendResult<()> {
            self.moving
                .store(false, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }

        fn temperature(&self) -> BackendResult<f32> {
            if self
                .fail_temperature
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err(BackendError("simulated temperature failure".to_string()));
            }
            Ok(*self.temperature.lock())
        }

        fn reverse(&self) -> BackendResult<bool> {
            Ok(self.reverse.load(std::sync::atomic::Ordering::SeqCst))
        }

        fn set_reverse(&self, reverse: bool) -> BackendResult<()> {
            self.reverse
                .store(reverse, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }
}
