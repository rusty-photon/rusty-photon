//! EAF focuser enumeration and device handle.
//!
//! [`Sdk::focusers`] lists every connected EAF's [`FocuserInfo`].
//! [`Sdk::open_focuser`] opens one and returns a [`Focuser`] RAII handle that
//! closes the device on drop. Unlike the EFW filter wheel, `EAFGetPosition` has
//! no moving sentinel — it always returns the live step count — so moving state
//! is read via the dedicated `EAFIsMoving` call instead. With the `simulation`
//! feature a single fabricated `EAF-Simulated` focuser is presented and the SDK
//! is never called.
//!
//! Note: like EFW, EAF status codes (`EAF_ERROR_CODE`) are a signed `c_int` (the
//! header's `EAF_ERROR_END = -1` makes the enum signed), so the return value is
//! fed to [`crate::eaf_check`] directly, with no `as i32` cast.

#[cfg(not(feature = "simulation"))]
use crate::ffi_util::{c_string_field, hex8};
#[cfg(not(feature = "simulation"))]
use crate::{eaf_check, sys};
#[cfg(not(feature = "simulation"))]
use std::os::raw::c_int;

use crate::{EafError, Error, Result, Sdk};

/// Safe view of `EAF_INFO`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocuserInfo {
    /// `ID` — the handle used by all per-focuser SDK calls.
    pub id: i32,
    /// Model name, e.g. `"EAF"`.
    pub name: String,
    /// Fixed maximum position (`EAF_INFO::MaxStep`).
    pub max_step: u32,
}

/// An open EAF focuser. Closes the device on drop.
///
/// As with [`crate::Camera`] and [`crate::FilterWheel`], the SDK is not safe for
/// concurrent calls on a single handle, so `Focuser` is `Send` but **not**
/// `Sync` — share it across threads behind a `Mutex`.
#[derive(Debug)]
pub struct Focuser {
    info: FocuserInfo,
    #[cfg(feature = "simulation")]
    state: std::sync::Mutex<SimFocuserState>,
    /// Makes `Focuser` `!Sync` (see the type docs) while leaving it `Send`.
    _not_sync: std::marker::PhantomData<std::cell::Cell<()>>,
}

impl Sdk {
    /// Enumerate every connected EAF's [`FocuserInfo`].
    ///
    /// # Errors
    /// Returns [`Error::Eaf`] if the SDK fails to read a focuser's id or
    /// property.
    pub fn focusers(&self) -> Result<Vec<FocuserInfo>> {
        #[cfg(feature = "simulation")]
        let infos = (0..crate::SIM_FOCUSER_COUNT)
            .map(|_| sim_focuser_info())
            .collect();
        #[cfg(not(feature = "simulation"))]
        let infos = {
            let n = self.focuser_count()?;
            (0..n)
                .map(|index| {
                    let idx =
                        i32::try_from(index).map_err(|_| Error::Eaf(EafError::InvalidIndex))?;
                    let id = read_focuser_id(idx)?;
                    read_focuser_property(id)
                })
                .collect::<Result<Vec<_>>>()?
        };
        Ok(infos)
    }

    /// Open the EAF at enumeration `index`.
    ///
    /// On the real path this calls `EAFGetID` + `EAFOpen` + `EAFGetProperty` (so
    /// the returned info carries the real `MaxStep`); the [`Focuser`] closes the
    /// device on drop.
    ///
    /// # Errors
    /// Returns [`Error::Eaf`] if the index is out of range or the SDK fails to
    /// open the focuser.
    pub fn open_focuser(&self, index: usize) -> Result<Focuser> {
        #[cfg(feature = "simulation")]
        let focuser = {
            if index >= crate::SIM_FOCUSER_COUNT {
                return Err(Error::Eaf(EafError::InvalidIndex));
            }
            Focuser {
                info: sim_focuser_info(),
                state: std::sync::Mutex::new(SimFocuserState::default()),
                _not_sync: std::marker::PhantomData,
            }
        };
        #[cfg(not(feature = "simulation"))]
        let focuser = {
            let idx = i32::try_from(index).map_err(|_| Error::Eaf(EafError::InvalidIndex))?;
            let id = read_focuser_id(idx)?;
            // SAFETY: `id` is a valid focuser id from enumeration; open it.
            eaf_check(unsafe { sys::EAFOpen(id) })?;
            // Read the property after opening so MaxStep is populated. On
            // failure, close the focuser so the open handle is not leaked.
            let info = match read_focuser_property(id) {
                Ok(info) => info,
                Err(e) => {
                    // SAFETY: the focuser was just opened; close it again.
                    unsafe {
                        let _ = sys::EAFClose(id);
                    }
                    return Err(e);
                }
            };
            Focuser {
                info,
                _not_sync: std::marker::PhantomData,
            }
        };
        Ok(focuser)
    }
}

impl Focuser {
    /// The focuser's cached [`FocuserInfo`].
    #[must_use]
    pub const fn info(&self) -> &FocuserInfo {
        &self.info
    }

    /// The focuser's `ID`.
    #[must_use]
    pub const fn id(&self) -> i32 {
        self.info.id
    }

    /// The working travel limit (`EAFGetMaxStep`) — the position the firmware
    /// actually stops at, user-settable via ZWO's tooling (factory default
    /// 60000). Distinct from [`FocuserInfo::max_step`] (`EAF_INFO::MaxStep`,
    /// the fixed ceiling the limit can be raised to): a move commanded past
    /// this limit but within the ceiling is *accepted* by `EAFMove` and then
    /// silently stops here, so range validation must use this value.
    ///
    /// # Errors
    /// Returns [`Error::Eaf`] if the SDK call fails (the focuser must be
    /// open — `EAF_ERROR_CLOSED` otherwise).
    // Const only under the simulation cfg; the real body calls into the SDK.
    #[allow(clippy::missing_const_for_fn)]
    pub fn max_step(&self) -> Result<u32> {
        #[cfg(feature = "simulation")]
        let max_step = SIM_MAX_STEP;
        #[cfg(not(feature = "simulation"))]
        let max_step = {
            let mut v: c_int = 0;
            // SAFETY: open focuser id; the SDK writes the current limit.
            eaf_check(unsafe { sys::EAFGetMaxStep(self.info.id, &raw mut v) })?;
            u32::try_from(v).unwrap_or(0)
        };
        Ok(max_step)
    }

    /// Current step position. Unlike [`crate::FilterWheel::position`], the EAF
    /// has no moving sentinel — this always returns the live/target step count,
    /// whether or not the focuser is currently moving (see [`Self::is_moving`]).
    ///
    /// # Errors
    /// Returns [`Error::Eaf`] if the SDK call fails.
    pub fn position(&self) -> Result<i32> {
        #[cfg(feature = "simulation")]
        let pos = self.sim_position();
        #[cfg(not(feature = "simulation"))]
        let pos = {
            let mut p: c_int = 0;
            // SAFETY: open focuser id; the SDK writes the current step count.
            eaf_check(unsafe { sys::EAFGetPosition(self.info.id, &raw mut p) })?;
            p
        };
        Ok(pos)
    }

    /// Whether the focuser is currently moving (`EAFIsMoving`).
    ///
    /// The SDK's `pbHandControl` out-parameter (whether the physical hand-paddle
    /// is driving the move) is read but discarded: no ASCOM `Focuser` property
    /// maps to it (see `docs/services/zwo-focuser.md` "MVP scope").
    ///
    /// # Errors
    /// Returns [`Error::Eaf`] if the SDK call fails.
    pub fn is_moving(&self) -> Result<bool> {
        #[cfg(feature = "simulation")]
        let moving = self.sim_is_moving();
        #[cfg(not(feature = "simulation"))]
        let moving = {
            let mut moving = false;
            let mut hand_control = false;
            // SAFETY: open focuser id; the SDK writes both out-parameters.
            eaf_check(unsafe {
                sys::EAFIsMoving(self.info.id, &raw mut moving, &raw mut hand_control)
            })?;
            moving
        };
        Ok(moving)
    }

    /// Move to absolute `position` (0-based step count, `0..=max_step`).
    ///
    /// The SDK itself does not document a bounds check for `EAFMove`; range
    /// validation against `max_step` is the ASCOM device's responsibility (see
    /// `docs/services/zwo-focuser.md` "ASCOM Focuser Mapping"). The `simulation`
    /// backend enforces the same bound for test fidelity.
    ///
    /// # Errors
    /// Returns [`Error::Eaf`] if the focuser is already moving or the SDK call
    /// fails.
    pub fn move_to(&self, position: i32) -> Result<()> {
        #[cfg(feature = "simulation")]
        self.sim_move_to(position)?;
        #[cfg(not(feature = "simulation"))]
        {
            // SAFETY: open focuser id; the SDK validates/starts the move.
            eaf_check(unsafe { sys::EAFMove(self.info.id, position) })?;
        }
        Ok(())
    }

    /// Stop an in-progress move (`EAFStop`). A no-op on an idle focuser.
    ///
    /// # Errors
    /// Returns [`Error::Eaf`] if the SDK call fails.
    pub fn stop(&self) -> Result<()> {
        #[cfg(feature = "simulation")]
        self.sim_stop();
        #[cfg(not(feature = "simulation"))]
        // SAFETY: open focuser id; stops any in-progress move.
        eaf_check(unsafe { sys::EAFStop(self.info.id) })?;
        Ok(())
    }

    /// The temperature sensor reading in degrees Celsius (`EAFGetTemp`).
    ///
    /// # Errors
    /// Returns [`Error::Eaf`] if the SDK call fails (e.g. the value is
    /// currently unusable — see the header's `EAFGetTemp` docs).
    pub fn temperature(&self) -> Result<f32> {
        #[cfg(feature = "simulation")]
        let temp = self.sim_temperature();
        #[cfg(not(feature = "simulation"))]
        let temp = {
            let mut t: f32 = 0.0;
            // SAFETY: open focuser id; the SDK writes the temperature reading.
            eaf_check(unsafe { sys::EAFGetTemp(self.info.id, &raw mut t) })?;
            t
        };
        Ok(temp)
    }

    /// Whether the focuser moves along the reverse direction.
    ///
    /// # Errors
    /// Returns [`Error::Eaf`] if the SDK call fails.
    pub fn reverse(&self) -> Result<bool> {
        #[cfg(feature = "simulation")]
        let reverse = self.sim_reverse();
        #[cfg(not(feature = "simulation"))]
        let reverse = {
            let mut r = false;
            // SAFETY: open focuser id; the SDK writes the direction flag.
            eaf_check(unsafe { sys::EAFGetReverse(self.info.id, &raw mut r) })?;
            r
        };
        Ok(reverse)
    }

    /// Set whether the focuser moves along the reverse direction.
    ///
    /// # Errors
    /// Returns [`Error::Eaf`] if the SDK call fails.
    pub fn set_reverse(&self, reverse: bool) -> Result<()> {
        #[cfg(feature = "simulation")]
        self.sim_set_reverse(reverse);
        #[cfg(not(feature = "simulation"))]
        // SAFETY: open focuser id; sets the moving direction.
        eaf_check(unsafe { sys::EAFSetReverse(self.info.id, reverse) })?;
        Ok(())
    }

    /// Set the current position without moving (`EAFResetPostion`, the header's
    /// own spelling). Kept for parity with `EFW`'s `calibrate`; not called by
    /// the ASCOM device in v0.
    ///
    /// # Errors
    /// Returns [`Error::Eaf`] if the SDK call fails.
    pub fn reset_position(&self, position: i32) -> Result<()> {
        #[cfg(feature = "simulation")]
        self.sim_reset_position(position);
        #[cfg(not(feature = "simulation"))]
        // SAFETY: open focuser id; sets the position counter without moving.
        eaf_check(unsafe { sys::EAFResetPostion(self.info.id, position) })?;
        Ok(())
    }

    /// The focuser's serial number as a 16-character hex string.
    ///
    /// # Errors
    /// Returns [`Error::Eaf`] if the firmware does not report a serial number
    /// (`EAF_ERROR_NOT_SUPPORTED` on older firmware).
    pub fn serial(&self) -> Result<String> {
        #[cfg(feature = "simulation")]
        let serial = SIM_EAF_SERIAL.to_owned();
        #[cfg(not(feature = "simulation"))]
        let serial = {
            // SAFETY: `EAF_SN` is a POD `[u8; 8]`; the SDK fills it on success.
            let mut sn: sys::EAF_SN = unsafe { std::mem::zeroed() };
            eaf_check(unsafe { sys::EAFGetSerialNumber(self.info.id, &raw mut sn) })?;
            hex8(sn.id)
        };
        Ok(serial)
    }

    /// The focuser firmware version as `(major, minor, build)`.
    ///
    /// # Errors
    /// Returns [`Error::Eaf`] if the SDK call fails.
    // Const only under the simulation cfg; the real body calls into the SDK.
    #[allow(clippy::missing_const_for_fn)]
    pub fn firmware_version(&self) -> Result<(u8, u8, u8)> {
        #[cfg(feature = "simulation")]
        let version = (1, 0, 0);
        #[cfg(not(feature = "simulation"))]
        let version = {
            let mut major: u8 = 0;
            let mut minor: u8 = 0;
            let mut build: u8 = 0;
            // SAFETY: open focuser id; the SDK writes the three version bytes.
            eaf_check(unsafe {
                sys::EAFGetFirmwareVersion(
                    self.info.id,
                    &raw mut major,
                    &raw mut minor,
                    &raw mut build,
                )
            })?;
            (major, minor, build)
        };
        Ok(version)
    }
}

#[cfg(not(feature = "simulation"))]
impl Drop for Focuser {
    fn drop(&mut self) {
        // SAFETY: closing an open focuser by id; `EAFClose` is safe to call once
        // on an open handle.
        unsafe {
            let _ = sys::EAFClose(self.info.id);
        }
    }
}

// ---- real FFI helpers --------------------------------------------------------

#[cfg(not(feature = "simulation"))]
fn read_focuser_id(index: i32) -> Result<i32> {
    let mut id: c_int = 0;
    // SAFETY: the SDK writes the focuser id for a valid index.
    eaf_check(unsafe { sys::EAFGetID(index, &raw mut id) })?;
    Ok(id)
}

#[cfg(not(feature = "simulation"))]
fn read_focuser_property(id: i32) -> Result<FocuserInfo> {
    // SAFETY: `EAF_INFO` is POD; the SDK fills it for a valid id.
    let mut raw: sys::EAF_INFO = unsafe { std::mem::zeroed() };
    eaf_check(unsafe { sys::EAFGetProperty(id, &raw mut raw) })?;
    Ok(FocuserInfo {
        id: raw.ID,
        name: c_string_field(&raw.Name),
        max_step: u32::try_from(raw.MaxStep).unwrap_or(0),
    })
}

// ---- simulation backend ------------------------------------------------------

#[cfg(feature = "simulation")]
const SIM_EAF_SERIAL: &str = "2a3b4c5d6e7f8091";

/// `EAF_INFO::MaxStep` — the fixed ceiling the working limit can be raised to.
/// A real EAF reports 600000.
#[cfg(feature = "simulation")]
const SIM_INFO_MAX_STEP: u32 = 600_000;

/// The working travel limit (`EAFGetMaxStep`) — the position the firmware
/// actually stops at. A real EAF's factory default is 60000.
#[cfg(feature = "simulation")]
pub const SIM_MAX_STEP: u32 = 60_000;

/// Steps a simulated move advances per `is_moving` poll. A real EAF travels
/// ~640 steps per second (a 1000-step move settles in ≈1.7 s), so this models
/// one poll ≈ one second of travel while keeping the simulation deterministic
/// (travel advances on observation, not wall time).
#[cfg(feature = "simulation")]
const SIM_STEPS_PER_POLL: i32 = 640;

/// The fabricated simulated focuser. The limits mirror a real EAF unit:
/// `EAF_INFO::MaxStep` (the ceiling, here) is 600000 while the firmware's
/// working travel limit ([`SIM_MAX_STEP`], served via [`Focuser::max_step`])
/// is 60000 — keeping the two-limit distinction visible in the simulation.
#[cfg(feature = "simulation")]
fn sim_focuser_info() -> FocuserInfo {
    FocuserInfo {
        id: 0,
        name: "EAF-Simulated".to_owned(),
        max_step: SIM_INFO_MAX_STEP,
    }
}

/// Mutable state for the simulated focuser, behind a `Mutex` so the `&self`
/// device methods can update it.
#[cfg(feature = "simulation")]
#[derive(Debug)]
struct SimFocuserState {
    position: i32,
    target: i32,
    moving: bool,
    reverse: bool,
    temperature: f32,
}

#[cfg(feature = "simulation")]
impl Default for SimFocuserState {
    fn default() -> Self {
        Self {
            position: 0,
            target: 0,
            moving: false,
            reverse: false,
            temperature: 20.0,
        }
    }
}

#[cfg(feature = "simulation")]
impl Focuser {
    fn sim_position(&self) -> i32 {
        // The in-flight position: a real EAF's `EAFGetPosition` reports the
        // live step count that ramps toward the target while moving (it never
        // jumps to the target). Travel advances on `is_moving` polls (see
        // `sim_is_moving`), so this is a pure read.
        self.state.lock().unwrap().position
    }

    fn sim_is_moving(&self) -> bool {
        let mut st = self.state.lock().unwrap();
        if !st.moving {
            return false;
        }
        // Advance up to one poll's worth of travel toward the target,
        // mirroring the real EAF where `IsMoving` stays true across several
        // polls and the position ramps in between. The poll that reaches the
        // target still reports `true` (the hardware reports moving until
        // after it lands); the next poll reports `false`.
        let delta = st.target.saturating_sub(st.position);
        let step = delta.clamp(-SIM_STEPS_PER_POLL, SIM_STEPS_PER_POLL);
        st.position = st.position.saturating_add(step);
        if st.position == st.target {
            st.moving = false;
        }
        true
    }

    fn sim_move_to(&self, position: i32) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        // A focuser already in motion rejects a new move, as the hardware does.
        if st.moving {
            return Err(Error::Eaf(EafError::Moving));
        }
        // `EAFMove` validates against the `EAF_INFO::MaxStep` ceiling only; a
        // target past the working limit but within the ceiling is accepted
        // and the firmware silently stops at the limit (observed on real
        // hardware). Range validation against the working limit is the ASCOM
        // device's responsibility.
        if position < 0 || u32::try_from(position).unwrap_or(u32::MAX) > self.info.max_step {
            return Err(Error::Eaf(EafError::InvalidValue));
        }
        st.target = position.min(i32::try_from(SIM_MAX_STEP).expect("SIM_MAX_STEP fits in i32"));
        st.moving = true;
        Ok(())
    }

    fn sim_stop(&self) {
        // Freeze wherever the move currently is — a real halt leaves the
        // position mid-travel, not at the original target.
        let mut st = self.state.lock().unwrap();
        st.target = st.position;
        st.moving = false;
    }

    fn sim_temperature(&self) -> f32 {
        self.state.lock().unwrap().temperature
    }

    fn sim_reverse(&self) -> bool {
        self.state.lock().unwrap().reverse
    }

    fn sim_set_reverse(&self, reverse: bool) {
        self.state.lock().unwrap().reverse = reverse;
    }

    fn sim_reset_position(&self, position: i32) {
        let mut st = self.state.lock().unwrap();
        st.position = position;
        st.target = position;
        st.moving = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focuser_is_send() {
        // Same threading contract as `Camera`/`FilterWheel`: `Send` but not `Sync`.
        fn assert_send<T: Send>() {}
        assert_send::<Focuser>();
    }

    #[test]
    fn focusers_enumerates() {
        let sdk = Sdk::new().unwrap();
        let focusers = sdk.focusers().unwrap();
        #[cfg(feature = "simulation")]
        {
            assert_eq!(focusers.len(), crate::SIM_FOCUSER_COUNT);
            assert_eq!(focusers[0].name, "EAF-Simulated");
            assert_eq!(focusers[0].max_step, 600_000);
        }
        // Without the feature this calls the real SDK; with no hardware the
        // list is empty, but the call must still succeed.
        #[cfg(not(feature = "simulation"))]
        {
            let _ = focusers;
        }
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn open_exposes_info_serial_and_firmware() {
        let sdk = Sdk::new().unwrap();
        let focuser = sdk.open_focuser(0).unwrap();
        assert_eq!(focuser.id(), 0);
        // The two limits stay distinct: the EAF_INFO ceiling vs the working
        // travel limit the firmware enforces (EAFGetMaxStep).
        assert_eq!(focuser.info().max_step, 600_000);
        assert_eq!(focuser.max_step().unwrap(), 60_000);
        assert_eq!(focuser.info().name, "EAF-Simulated");
        let serial = focuser.serial().unwrap();
        assert_eq!(serial.len(), 16);
        assert!(serial.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(focuser.firmware_version().unwrap(), (1, 0, 0));
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn open_out_of_range_is_rejected() {
        let sdk = Sdk::new().unwrap();
        assert_eq!(
            sdk.open_focuser(9).unwrap_err(),
            Error::Eaf(EafError::InvalidIndex)
        );
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn move_ramps_position_and_settles_at_the_target() {
        let sdk = Sdk::new().unwrap();
        let focuser = sdk.open_focuser(0).unwrap();
        assert_eq!(focuser.position().unwrap(), 0);
        focuser.move_to(3000).unwrap();
        // Position ramps toward the target across is_moving polls — it never
        // jumps straight there (mirrors real EAF behavior). 3000 steps at 640
        // per poll = 5 polls to land; the landing poll still reports true.
        assert_eq!(focuser.position().unwrap(), 0);
        assert!(focuser.is_moving().unwrap());
        assert_eq!(focuser.position().unwrap(), 640);
        for _ in 0..4 {
            assert!(focuser.is_moving().unwrap());
        }
        assert!(!focuser.is_moving().unwrap());
        assert_eq!(focuser.position().unwrap(), 3000);
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn move_out_of_range_is_rejected() {
        let sdk = Sdk::new().unwrap();
        let focuser = sdk.open_focuser(0).unwrap();
        assert_eq!(
            focuser.move_to(-1).unwrap_err(),
            Error::Eaf(EafError::InvalidValue)
        );
        // The SDK bound is the EAF_INFO ceiling, not the working limit.
        assert_eq!(
            focuser.move_to(600_001).unwrap_err(),
            Error::Eaf(EafError::InvalidValue)
        );
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn move_past_the_working_limit_stops_at_the_limit() {
        // Observed on real hardware: EAFMove accepts a target within the
        // EAF_INFO ceiling but past the EAFGetMaxStep working limit, and the
        // firmware silently stops at the limit.
        let sdk = Sdk::new().unwrap();
        let focuser = sdk.open_focuser(0).unwrap();
        focuser.move_to(90_000).unwrap();
        while focuser.is_moving().unwrap() {}
        assert_eq!(focuser.position().unwrap(), 60_000);
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn move_while_moving_is_rejected() {
        let sdk = Sdk::new().unwrap();
        let focuser = sdk.open_focuser(0).unwrap();
        focuser.move_to(100).unwrap();
        assert_eq!(
            focuser.move_to(200).unwrap_err(),
            Error::Eaf(EafError::Moving)
        );
        // A short (≤ one poll's travel) move settles after one is_moving
        // poll; a new move is then accepted.
        assert!(focuser.is_moving().unwrap());
        focuser.move_to(200).unwrap();
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn stop_freezes_mid_travel() {
        let sdk = Sdk::new().unwrap();
        let focuser = sdk.open_focuser(0).unwrap();
        focuser.move_to(2000).unwrap();
        // One poll of travel, then halt: the position freezes wherever the
        // move was (mirrors real hardware), not at the original target.
        assert!(focuser.is_moving().unwrap());
        focuser.stop().unwrap();
        assert!(!focuser.is_moving().unwrap());
        assert_eq!(focuser.position().unwrap(), 640);
        // A halted focuser accepts a new move immediately.
        focuser.move_to(600).unwrap();
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn reverse_round_trips() {
        let sdk = Sdk::new().unwrap();
        let focuser = sdk.open_focuser(0).unwrap();
        assert!(!focuser.reverse().unwrap());
        focuser.set_reverse(true).unwrap();
        assert!(focuser.reverse().unwrap());
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn temperature_returns_a_value() {
        let sdk = Sdk::new().unwrap();
        let focuser = sdk.open_focuser(0).unwrap();
        assert_eq!(focuser.temperature().unwrap(), 20.0);
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn reset_position_sets_without_moving() {
        let sdk = Sdk::new().unwrap();
        let focuser = sdk.open_focuser(0).unwrap();
        focuser.reset_position(42).unwrap();
        assert_eq!(focuser.position().unwrap(), 42);
        assert!(!focuser.is_moving().unwrap());
    }
}
