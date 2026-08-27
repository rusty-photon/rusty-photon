//! EFW filter-wheel enumeration and device handle.
//!
//! [`Sdk::filter_wheels`] lists every connected wheel's [`FilterWheelInfo`].
//! [`Sdk::open_filter_wheel`] opens one and returns a [`FilterWheel`] RAII handle
//! that closes the device on drop. The handle covers slot position (with the
//! SDK's `-1`-while-moving sentinel surfaced as `None`), serial, firmware
//! version, calibration, and rotation direction. With the `simulation` feature a
//! single fabricated 7-slot `EFW-Simulated` wheel is presented and the SDK is
//! never called.
//!
//! Note: EFW status codes (`EFW_ERROR_CODE`) are a signed `c_int` (the header's
//! `EFW_ERROR_END = -1` makes the enum signed), so unlike the ASI side the
//! return value is fed to [`crate::efw_check`] directly, with no `as i32` cast.

#[cfg(not(feature = "simulation"))]
use crate::ffi_util::{c_string_field, hex8};
#[cfg(not(feature = "simulation"))]
use crate::{efw_check, sys};
#[cfg(not(feature = "simulation"))]
use std::os::raw::c_int;

use crate::{EfwError, Error, Result, Sdk};

/// Safe view of `EFW_INFO`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterWheelInfo {
    /// `ID` — the handle used by all per-wheel SDK calls.
    pub id: i32,
    /// Model name, e.g. `"EFW"`.
    pub name: String,
    /// Number of filter slots. The SDK reports `0` until the wheel is opened.
    pub slot_count: u32,
}

/// An open EFW filter wheel. Closes the device on drop.
///
/// As with [`crate::Camera`], the SDK is not safe for concurrent calls on a
/// single handle, so `FilterWheel` is `Send` but **not** `Sync` — share it
/// across threads behind a `Mutex`.
#[derive(Debug)]
pub struct FilterWheel {
    info: FilterWheelInfo,
    #[cfg(feature = "simulation")]
    state: std::sync::Mutex<SimEfwState>,
    /// Makes `FilterWheel` `!Sync` (see the type docs) while leaving it `Send`.
    _not_sync: std::marker::PhantomData<std::cell::Cell<()>>,
}

impl Sdk {
    /// Enumerate every connected filter wheel's [`FilterWheelInfo`].
    ///
    /// `slot_count` is `0` for wheels that are not yet open (an SDK quirk); open
    /// the wheel to read the real slot count.
    ///
    /// # Errors
    /// Returns [`Error::Efw`] if the SDK fails to read a wheel's id or property.
    pub fn filter_wheels(&self) -> Result<Vec<FilterWheelInfo>> {
        #[cfg(feature = "simulation")]
        let infos = (0..crate::SIM_FILTER_WHEEL_COUNT)
            .map(|_| sim_filter_wheel_info())
            .collect();
        #[cfg(not(feature = "simulation"))]
        let infos = {
            let n = self.filter_wheel_count()?;
            (0..n)
                .map(|index| {
                    let idx =
                        i32::try_from(index).map_err(|_| Error::Efw(EfwError::InvalidIndex))?;
                    let id = read_filter_wheel_id(idx)?;
                    read_filter_wheel_property(id)
                })
                .collect::<Result<Vec<_>>>()?
        };
        Ok(infos)
    }

    /// Open the filter wheel at enumeration `index`.
    ///
    /// On the real path this calls `EFWGetID` + `EFWOpen` + `EFWGetProperty` (so
    /// the returned info carries the real slot count); the [`FilterWheel`] closes
    /// the device on drop.
    ///
    /// # Errors
    /// Returns [`Error::Efw`] if the index is out of range or the SDK fails to
    /// open the wheel.
    pub fn open_filter_wheel(&self, index: usize) -> Result<FilterWheel> {
        #[cfg(feature = "simulation")]
        let wheel = {
            if index >= crate::SIM_FILTER_WHEEL_COUNT {
                return Err(Error::Efw(EfwError::InvalidIndex));
            }
            FilterWheel {
                info: sim_filter_wheel_info(),
                state: std::sync::Mutex::new(SimEfwState::default()),
                _not_sync: std::marker::PhantomData,
            }
        };
        #[cfg(not(feature = "simulation"))]
        let wheel = {
            let idx = i32::try_from(index).map_err(|_| Error::Efw(EfwError::InvalidIndex))?;
            let id = read_filter_wheel_id(idx)?;
            // SAFETY: `id` is a valid wheel id from enumeration; open it.
            efw_check(unsafe { sys::EFWOpen(id) })?;
            // Read the property after opening so the slot count is populated. On
            // failure, close the wheel so the open handle is not leaked.
            let info = match read_filter_wheel_property(id) {
                Ok(info) => info,
                Err(e) => {
                    // SAFETY: the wheel was just opened; close it again.
                    unsafe {
                        let _ = sys::EFWClose(id);
                    }
                    return Err(e);
                }
            };
            FilterWheel {
                info,
                _not_sync: std::marker::PhantomData,
            }
        };
        Ok(wheel)
    }
}

impl FilterWheel {
    /// The wheel's cached [`FilterWheelInfo`].
    #[must_use]
    pub const fn info(&self) -> &FilterWheelInfo {
        &self.info
    }

    /// The wheel's `ID`.
    #[must_use]
    pub const fn id(&self) -> i32 {
        self.info.id
    }

    /// Number of filter slots.
    #[must_use]
    pub const fn slot_count(&self) -> u32 {
        self.info.slot_count
    }

    /// Current slot position, or `None` while the wheel is moving (the SDK's
    /// `-1` sentinel).
    ///
    /// # Errors
    /// Returns [`Error::Efw`] if the SDK call fails.
    pub fn position(&self) -> Result<Option<u32>> {
        #[cfg(feature = "simulation")]
        let pos = self.sim_position();
        #[cfg(not(feature = "simulation"))]
        let pos = {
            let mut p: c_int = 0;
            // SAFETY: open wheel id; the SDK writes the position (or -1 moving).
            efw_check(unsafe { sys::EFWGetPosition(self.info.id, &raw mut p) })?;
            if p < 0 {
                None
            } else {
                Some(u32::try_from(p).unwrap_or(0))
            }
        };
        Ok(pos)
    }

    /// Whether the wheel is currently moving (i.e. [`position`](Self::position)
    /// is `None`).
    ///
    /// # Errors
    /// Returns [`Error::Efw`] if the SDK call fails.
    pub fn is_moving(&self) -> Result<bool> {
        Ok(self.position()?.is_none())
    }

    /// Move to slot `position` (0-based).
    ///
    /// # Errors
    /// Returns [`Error::Efw`] if the slot is out of range, the wheel is already
    /// moving, or the SDK call fails.
    pub fn set_position(&self, position: u32) -> Result<()> {
        #[cfg(feature = "simulation")]
        self.sim_set_position(position)?;
        #[cfg(not(feature = "simulation"))]
        {
            let p = c_int::try_from(position).map_err(|_| Error::Efw(EfwError::InvalidValue))?;
            // SAFETY: open wheel id; the SDK validates the slot.
            efw_check(unsafe { sys::EFWSetPosition(self.info.id, p) })?;
        }
        Ok(())
    }

    /// The wheel's serial number as a 16-character hex string.
    ///
    /// # Errors
    /// Returns [`Error::Efw`] if the firmware does not report a serial number.
    pub fn serial(&self) -> Result<String> {
        #[cfg(feature = "simulation")]
        let serial = SIM_EFW_SERIAL.to_owned();
        #[cfg(not(feature = "simulation"))]
        let serial = {
            // SAFETY: `EFW_SN` is a POD `[u8; 8]`; the SDK fills it on success.
            let mut sn: sys::EFW_SN = unsafe { std::mem::zeroed() };
            efw_check(unsafe { sys::EFWGetSerialNumber(self.info.id, &raw mut sn) })?;
            hex8(sn.id)
        };
        Ok(serial)
    }

    /// The wheel firmware version as `(major, minor, build)`.
    ///
    /// # Errors
    /// Returns [`Error::Efw`] if the SDK call fails.
    // Const only under the simulation cfg; the real body calls into the SDK.
    #[allow(clippy::missing_const_for_fn)]
    pub fn firmware_version(&self) -> Result<(u8, u8, u8)> {
        #[cfg(feature = "simulation")]
        let version = (1, 7, 0);
        #[cfg(not(feature = "simulation"))]
        let version = {
            let mut major: u8 = 0;
            let mut minor: u8 = 0;
            let mut build: u8 = 0;
            // SAFETY: open wheel id; the SDK writes the three version bytes.
            efw_check(unsafe {
                sys::EFWGetFirmwareVersion(
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

    /// Calibrate the wheel (re-home the slot detection).
    ///
    /// # Errors
    /// Returns [`Error::Efw`] if the wheel is moving or the SDK call fails.
    pub fn calibrate(&self) -> Result<()> {
        #[cfg(feature = "simulation")]
        self.sim_calibrate()?;
        #[cfg(not(feature = "simulation"))]
        // SAFETY: open wheel id; starts a calibration cycle.
        efw_check(unsafe { sys::EFWCalibrate(self.info.id) })?;
        Ok(())
    }

    /// Whether the wheel rotates unidirectionally.
    ///
    /// # Errors
    /// Returns [`Error::Efw`] if the SDK call fails.
    pub fn is_unidirectional(&self) -> Result<bool> {
        #[cfg(feature = "simulation")]
        let uni = self.sim_unidirectional();
        #[cfg(not(feature = "simulation"))]
        let uni = {
            let mut u = false;
            // SAFETY: open wheel id; the SDK writes the direction flag.
            efw_check(unsafe { sys::EFWGetDirection(self.info.id, &raw mut u) })?;
            u
        };
        Ok(uni)
    }

    /// Set whether the wheel rotates unidirectionally.
    ///
    /// # Errors
    /// Returns [`Error::Efw`] if the SDK call fails.
    pub fn set_unidirectional(&self, unidirectional: bool) -> Result<()> {
        #[cfg(feature = "simulation")]
        self.sim_set_unidirectional(unidirectional);
        #[cfg(not(feature = "simulation"))]
        // SAFETY: open wheel id; sets the rotation direction.
        efw_check(unsafe { sys::EFWSetDirection(self.info.id, unidirectional) })?;
        Ok(())
    }
}

#[cfg(not(feature = "simulation"))]
impl Drop for FilterWheel {
    fn drop(&mut self) {
        // SAFETY: closing an open wheel by id; `EFWClose` is safe to call once
        // on an open handle.
        unsafe {
            let _ = sys::EFWClose(self.info.id);
        }
    }
}

// ---- real FFI helpers --------------------------------------------------------

#[cfg(not(feature = "simulation"))]
fn read_filter_wheel_id(index: i32) -> Result<i32> {
    let mut id: c_int = 0;
    // SAFETY: the SDK writes the wheel id for a valid index.
    efw_check(unsafe { sys::EFWGetID(index, &raw mut id) })?;
    Ok(id)
}

#[cfg(not(feature = "simulation"))]
fn read_filter_wheel_property(id: i32) -> Result<FilterWheelInfo> {
    // SAFETY: `EFW_INFO` is POD; the SDK fills it for a valid id.
    let mut raw: sys::EFW_INFO = unsafe { std::mem::zeroed() };
    efw_check(unsafe { sys::EFWGetProperty(id, &raw mut raw) })?;
    Ok(FilterWheelInfo {
        id: raw.ID,
        name: c_string_field(&raw.Name),
        slot_count: u32::try_from(raw.slotNum).unwrap_or(0),
    })
}

// ---- simulation backend ------------------------------------------------------

#[cfg(feature = "simulation")]
const SIM_EFW_SERIAL: &str = "1a2b3c4d5e6f7081";

/// The fabricated simulated filter wheel: a 7-slot `EFW-Simulated`.
#[cfg(feature = "simulation")]
fn sim_filter_wheel_info() -> FilterWheelInfo {
    FilterWheelInfo {
        id: 0,
        name: "EFW-Simulated".to_owned(),
        slot_count: 7,
    }
}

/// Mutable state for the simulated filter wheel, behind a `Mutex` so the `&self`
/// device methods can update it.
#[cfg(feature = "simulation")]
#[derive(Debug, Default)]
struct SimEfwState {
    position: u32,
    moving: bool,
    unidirectional: bool,
}

#[cfg(feature = "simulation")]
impl FilterWheel {
    fn sim_position(&self) -> Option<u32> {
        let mut st = self.state.lock().unwrap();
        if st.moving {
            // A simulated move settles one poll after it is requested, mirroring
            // the real `-1`-while-moving sentinel.
            st.moving = false;
            None
        } else {
            Some(st.position)
        }
    }

    fn sim_set_position(&self, position: u32) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        // A wheel already in motion rejects a new move, as the hardware does
        // (`EFWSetPosition` -> EFW_ERROR_MOVING).
        if st.moving {
            return Err(Error::Efw(EfwError::Moving));
        }
        if position >= self.info.slot_count {
            return Err(Error::Efw(EfwError::InvalidValue));
        }
        st.position = position;
        st.moving = true;
        Ok(())
    }

    fn sim_calibrate(&self) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        // Calibration is also rejected mid-move (`EFWCalibrate` -> EFW_ERROR_MOVING).
        if st.moving {
            return Err(Error::Efw(EfwError::Moving));
        }
        st.position = 0;
        st.moving = true;
        Ok(())
    }

    fn sim_unidirectional(&self) -> bool {
        self.state.lock().unwrap().unidirectional
    }

    fn sim_set_unidirectional(&self, unidirectional: bool) {
        self.state.lock().unwrap().unidirectional = unidirectional;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_wheel_is_send() {
        // Same threading contract as `Camera`: `Send` but not `Sync`.
        fn assert_send<T: Send>() {}
        assert_send::<FilterWheel>();
    }

    #[test]
    fn filter_wheels_enumerates() {
        let sdk = Sdk::new().unwrap();
        let wheels = sdk.filter_wheels().unwrap();
        #[cfg(feature = "simulation")]
        {
            assert_eq!(wheels.len(), crate::SIM_FILTER_WHEEL_COUNT);
            assert_eq!(wheels[0].name, "EFW-Simulated");
            assert_eq!(wheels[0].slot_count, 7);
        }
        // Without the feature this calls the real SDK; with no hardware the list
        // is empty, but the call must still succeed.
        #[cfg(not(feature = "simulation"))]
        {
            let _ = wheels;
        }
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn open_exposes_info_serial_and_firmware() {
        let sdk = Sdk::new().unwrap();
        let wheel = sdk.open_filter_wheel(0).unwrap();
        assert_eq!(wheel.id(), 0);
        assert_eq!(wheel.slot_count(), 7);
        assert_eq!(wheel.info().name, "EFW-Simulated");
        let serial = wheel.serial().unwrap();
        assert_eq!(serial.len(), 16);
        assert!(serial.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(wheel.firmware_version().unwrap(), (1, 7, 0));
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn open_out_of_range_is_rejected() {
        let sdk = Sdk::new().unwrap();
        assert_eq!(
            sdk.open_filter_wheel(9).unwrap_err(),
            Error::Efw(EfwError::InvalidIndex)
        );
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn move_reports_moving_then_settles() {
        let sdk = Sdk::new().unwrap();
        let wheel = sdk.open_filter_wheel(0).unwrap();
        assert_eq!(wheel.position().unwrap(), Some(0));
        wheel.set_position(3).unwrap();
        // The simulated wheel reports moving (None) once, then the new slot.
        assert!(wheel.is_moving().unwrap());
        assert_eq!(wheel.position().unwrap(), Some(3));
        assert!(!wheel.is_moving().unwrap());
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn set_position_out_of_range_is_rejected() {
        let sdk = Sdk::new().unwrap();
        let wheel = sdk.open_filter_wheel(0).unwrap();
        // Slots are 0..=6; slot 7 is out of range.
        assert_eq!(
            wheel.set_position(7).unwrap_err(),
            Error::Efw(EfwError::InvalidValue)
        );
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn unidirectional_round_trips() {
        let sdk = Sdk::new().unwrap();
        let wheel = sdk.open_filter_wheel(0).unwrap();
        assert!(!wheel.is_unidirectional().unwrap());
        wheel.set_unidirectional(true).unwrap();
        assert!(wheel.is_unidirectional().unwrap());
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn calibrate_runs_and_homes() {
        let sdk = Sdk::new().unwrap();
        let wheel = sdk.open_filter_wheel(0).unwrap();
        wheel.set_position(5).unwrap();
        let _ = wheel.position().unwrap(); // drain the move
        wheel.calibrate().unwrap();
        // Calibration re-homes to slot 0 after the move settles.
        assert!(wheel.is_moving().unwrap());
        assert_eq!(wheel.position().unwrap(), Some(0));
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn move_or_calibrate_while_moving_is_rejected() {
        let sdk = Sdk::new().unwrap();
        let wheel = sdk.open_filter_wheel(0).unwrap();
        wheel.set_position(2).unwrap();
        // While still moving, a second move and a calibrate are both rejected,
        // matching the hardware's EFW_ERROR_MOVING (not silently accepted).
        assert_eq!(
            wheel.set_position(4).unwrap_err(),
            Error::Efw(EfwError::Moving)
        );
        assert_eq!(wheel.calibrate().unwrap_err(), Error::Efw(EfwError::Moving));
        // After the move settles (one position read), a new move is accepted.
        let _ = wheel.position().unwrap();
        wheel.set_position(4).unwrap();
    }
}
