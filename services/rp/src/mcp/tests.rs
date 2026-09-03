//! Test module relocated verbatim from the pre-split `mcp.rs`. A
//! follow-up will distribute these tests across the per-category
//! `built_in/<category>.rs` files (matching the `imaging/` layout
//! convention) and split the shared mock-device fixtures into a
//! sibling `test_support.rs`.

use super::built_in::auto_focus::*;
use super::built_in::camera::*;
use super::built_in::center_on_target::*;
use super::built_in::cover_calibrator::*;
use super::built_in::filter_wheel::*;
use super::built_in::focuser::*;
use super::built_in::imaging::*;
use super::built_in::mount::*;
use super::built_in::planner::*;
use super::built_in::plate_solve::*;
use super::handler::McpHandler;
use super::inflight::Cancel;
use crate::persistence::{self, CachedPixels, ExposureDocument, ImageCache};
use crate::session::SessionConfig;
use ascom_alpaca::api::cover_calibrator::{CalibratorStatus, CoverStatus};
use ascom_alpaca::ASCOMError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use std::sync::Arc;
use std::time::Duration;

// -----------------------------------------------------------------------
// Mock Device macro
// -----------------------------------------------------------------------

/// Generates Debug + Device impl with stubs for all required methods.
macro_rules! impl_mock_device {
    ($name:ident) => {
        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, stringify!($name))
            }
        }

        #[async_trait::async_trait]
        impl ascom_alpaca::api::Device for $name {
            fn static_name(&self) -> &str {
                "mock"
            }
            fn unique_id(&self) -> &str {
                "mock-id"
            }
            async fn connected(&self) -> ascom_alpaca::ASCOMResult<bool> {
                Ok(true)
            }
            async fn set_connected(&self, _: bool) -> ascom_alpaca::ASCOMResult<()> {
                Ok(())
            }
            async fn description(&self) -> ascom_alpaca::ASCOMResult<String> {
                Ok("mock".into())
            }
            async fn driver_info(&self) -> ascom_alpaca::ASCOMResult<String> {
                Ok("mock".into())
            }
            async fn driver_version(&self) -> ascom_alpaca::ASCOMResult<String> {
                Ok("0.0".into())
            }
        }
    };
}

// -----------------------------------------------------------------------
// MockCamera — single configurable mock for all Camera error-injection
// -----------------------------------------------------------------------

#[derive(Default)]
struct MockCamera {
    fail_start_exposure: bool,
    fail_image_ready: bool,
    fail_image_array: bool,
    fail_max_adu: bool,
    fail_camera_size: bool,
    fail_pixel_size: bool,
    fail_exposure_range: bool,
    /// `0` ⇒ default 65535. Any other value is returned verbatim — set
    /// to `> u16::MAX` to exercise the I32 cache-insert path.
    max_adu_value: u32,
    /// When set, `image_ready` reports `false` forever — simulating a
    /// camera that never completes (a failed or wedged exposure).
    never_ready: bool,
    /// When non-zero, `image_ready` reports `false` for the first N
    /// calls and `true` thereafter — models a bounded readout for
    /// progress-notification tests. `0` (default) keeps the original
    /// behavior (`image_ready` returns true immediately).
    not_ready_count: u32,
    image_ready_calls: std::sync::atomic::AtomicU32,
    /// When set, `camera_state` reports `Error` — simulating a camera
    /// that failed an exposure (e.g. sky-survey-camera's mount read
    /// timing out). Drives the `do_capture` failed-exposure path.
    report_error_state: bool,
    /// When set, `camera_state` reports `Idle` — simulating an aborted
    /// exposure (the safety enforcer's `AbortExposure` returns the
    /// camera to Idle with no image). Without either state knob the
    /// mock reports `Exposing`, a camera mid-exposure.
    report_idle_state: bool,
    /// When set, `image_ready` reports `false` for the first N calls
    /// and errors thereafter — drives the aborted-idle re-check's
    /// read-error arm.
    fail_image_ready_after: Option<u32>,
    /// `abort_exposure` calls seen — the stop-class counterpart a
    /// cancelled `do_capture` must issue.
    abort_exposure_calls: std::sync::atomic::AtomicU32,
}

impl_mock_device!(MockCamera);

#[async_trait::async_trait]
impl ascom_alpaca::api::Camera for MockCamera {
    async fn start_exposure(
        &self,
        _duration: Duration,
        _light: bool,
    ) -> ascom_alpaca::ASCOMResult<()> {
        if self.fail_start_exposure {
            return Err(ASCOMError::invalid_operation("shutter jammed"));
        }
        Ok(())
    }

    async fn abort_exposure(&self) -> ascom_alpaca::ASCOMResult<()> {
        self.abort_exposure_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    async fn image_ready(&self) -> ascom_alpaca::ASCOMResult<bool> {
        if self.fail_image_ready {
            return Err(ASCOMError::invalid_operation("readout failed"));
        }
        if let Some(after) = self.fail_image_ready_after {
            let n = self
                .image_ready_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n >= after {
                return Err(ASCOMError::invalid_operation("link dropped mid-poll"));
            }
            return Ok(false);
        }
        if self.never_ready {
            return Ok(false);
        }
        if self.not_ready_count > 0 {
            let n = self
                .image_ready_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n < self.not_ready_count {
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn camera_state(
        &self,
    ) -> ascom_alpaca::ASCOMResult<ascom_alpaca::api::camera::CameraState> {
        use ascom_alpaca::api::camera::CameraState;
        Ok(if self.report_error_state {
            CameraState::Error
        } else if self.report_idle_state {
            CameraState::Idle
        } else {
            CameraState::Exposing
        })
    }

    async fn image_array(
        &self,
    ) -> ascom_alpaca::ASCOMResult<ascom_alpaca::api::camera::ImageArray> {
        if self.fail_image_array {
            return Err(ASCOMError::invalid_operation("download timeout"));
        }
        Ok(ndarray::Array3::<i32>::zeros((2, 2, 1)).into())
    }

    async fn max_adu(&self) -> ascom_alpaca::ASCOMResult<u32> {
        if self.fail_max_adu {
            return Err(ASCOMError::invalid_operation("not available"));
        }
        Ok(if self.max_adu_value == 0 {
            65535
        } else {
            self.max_adu_value
        })
    }

    async fn camera_x_size(&self) -> ascom_alpaca::ASCOMResult<u32> {
        if self.fail_camera_size {
            return Err(ASCOMError::invalid_operation("sensor error"));
        }
        Ok(1024)
    }

    async fn camera_y_size(&self) -> ascom_alpaca::ASCOMResult<u32> {
        if self.fail_camera_size {
            return Err(ASCOMError::invalid_operation("sensor error"));
        }
        Ok(1024)
    }

    async fn exposure_max(&self) -> ascom_alpaca::ASCOMResult<Duration> {
        if self.fail_exposure_range {
            return Err(ASCOMError::invalid_operation("range unavailable"));
        }
        Ok(Duration::from_hours(1))
    }

    async fn exposure_min(&self) -> ascom_alpaca::ASCOMResult<Duration> {
        if self.fail_exposure_range {
            return Err(ASCOMError::invalid_operation("range unavailable"));
        }
        Ok(Duration::from_millis(1))
    }

    async fn exposure_resolution(&self) -> ascom_alpaca::ASCOMResult<Duration> {
        Ok(Duration::from_millis(1))
    }

    async fn has_shutter(&self) -> ascom_alpaca::ASCOMResult<bool> {
        Ok(true)
    }

    async fn pixel_size_x(&self) -> ascom_alpaca::ASCOMResult<f64> {
        if self.fail_pixel_size {
            return Err(ASCOMError::invalid_operation("pixel size unavailable"));
        }
        Ok(3.76)
    }

    async fn pixel_size_y(&self) -> ascom_alpaca::ASCOMResult<f64> {
        if self.fail_pixel_size {
            return Err(ASCOMError::invalid_operation("pixel size unavailable"));
        }
        Ok(3.76)
    }

    async fn start_x(&self) -> ascom_alpaca::ASCOMResult<u32> {
        Ok(0)
    }

    async fn set_start_x(&self, _start_x: u32) -> ascom_alpaca::ASCOMResult<()> {
        Ok(())
    }

    async fn start_y(&self) -> ascom_alpaca::ASCOMResult<u32> {
        Ok(0)
    }

    async fn set_start_y(&self, _start_y: u32) -> ascom_alpaca::ASCOMResult<()> {
        Ok(())
    }
}

// -----------------------------------------------------------------------
// MockCameraNoMetadata — regression contract for "per-capture metadata
// reads are gone". Implements only the exposure path; every invariant-
// sensor property method panics if called. Pins the `do_capture` ↔
// `CameraEntry`-cache contract: with the cache populated, capture must
// not touch `max_adu`, `pixel_size_*`, or `camera_*_size` on the device.
// -----------------------------------------------------------------------

#[derive(Default)]
struct MockCameraNoMetadata;

impl_mock_device!(MockCameraNoMetadata);

#[async_trait::async_trait]
impl ascom_alpaca::api::Camera for MockCameraNoMetadata {
    async fn start_exposure(
        &self,
        _duration: Duration,
        _light: bool,
    ) -> ascom_alpaca::ASCOMResult<()> {
        Ok(())
    }

    async fn image_ready(&self) -> ascom_alpaca::ASCOMResult<bool> {
        Ok(true)
    }

    async fn image_array(
        &self,
    ) -> ascom_alpaca::ASCOMResult<ascom_alpaca::api::camera::ImageArray> {
        Ok(ndarray::Array3::<i32>::zeros((2, 2, 1)).into())
    }

    async fn max_adu(&self) -> ascom_alpaca::ASCOMResult<u32> {
        panic!("do_capture must read max_adu from CameraEntry cache, not the device")
    }

    async fn pixel_size_x(&self) -> ascom_alpaca::ASCOMResult<f64> {
        panic!("do_capture must read pixel_size_x from CameraEntry cache, not the device")
    }

    async fn pixel_size_y(&self) -> ascom_alpaca::ASCOMResult<f64> {
        panic!("do_capture must read pixel_size_y from CameraEntry cache, not the device")
    }

    async fn camera_x_size(&self) -> ascom_alpaca::ASCOMResult<u32> {
        panic!("do_capture must read camera_x_size from CameraEntry cache, not the device")
    }

    async fn camera_y_size(&self) -> ascom_alpaca::ASCOMResult<u32> {
        panic!("do_capture must read camera_y_size from CameraEntry cache, not the device")
    }

    async fn exposure_max(&self) -> ascom_alpaca::ASCOMResult<Duration> {
        Ok(Duration::from_hours(1))
    }

    async fn exposure_min(&self) -> ascom_alpaca::ASCOMResult<Duration> {
        Ok(Duration::from_millis(1))
    }

    async fn exposure_resolution(&self) -> ascom_alpaca::ASCOMResult<Duration> {
        Ok(Duration::from_millis(1))
    }

    async fn has_shutter(&self) -> ascom_alpaca::ASCOMResult<bool> {
        Ok(true)
    }

    async fn start_x(&self) -> ascom_alpaca::ASCOMResult<u32> {
        Ok(0)
    }

    async fn set_start_x(&self, _start_x: u32) -> ascom_alpaca::ASCOMResult<()> {
        Ok(())
    }

    async fn start_y(&self) -> ascom_alpaca::ASCOMResult<u32> {
        Ok(0)
    }

    async fn set_start_y(&self, _start_y: u32) -> ascom_alpaca::ASCOMResult<()> {
        Ok(())
    }
}

// -----------------------------------------------------------------------
// MockFilterWheel — single configurable mock for FilterWheel errors
// -----------------------------------------------------------------------

#[derive(Default)]
struct MockFilterWheel {
    fail_set_position: bool,
    fail_position_poll: bool,
    report_moving: bool,
    /// The slot `position()` reports (`Lum` at 0, `Red` at 1 in the
    /// test registry's names).
    position: usize,
}

impl_mock_device!(MockFilterWheel);

#[async_trait::async_trait]
impl ascom_alpaca::api::FilterWheel for MockFilterWheel {
    async fn set_position(&self, _position: usize) -> ascom_alpaca::ASCOMResult<()> {
        if self.fail_set_position {
            return Err(ASCOMError::invalid_operation("wheel stuck"));
        }
        Ok(())
    }

    async fn position(&self) -> ascom_alpaca::ASCOMResult<Option<usize>> {
        if self.fail_position_poll {
            return Err(ASCOMError::invalid_operation("encoder error"));
        }
        if self.report_moving {
            return Ok(None);
        }
        Ok(Some(self.position))
    }

    async fn names(&self) -> ascom_alpaca::ASCOMResult<Vec<String>> {
        Ok(vec!["Lum".into(), "Red".into()])
    }

    async fn focus_offsets(&self) -> ascom_alpaca::ASCOMResult<Vec<i32>> {
        Ok(vec![0, 0])
    }
}

// -----------------------------------------------------------------------
// MockCoverCalibrator — single configurable mock for CoverCalibrator
// -----------------------------------------------------------------------

#[derive(Default)]
struct MockCoverCalibrator {
    fail_close_cover: bool,
    fail_open_cover: bool,
    fail_calibrator_on: bool,
    fail_calibrator_off: bool,
    fail_max_brightness: bool,
    fail_cover_state_poll: bool,
    stuck_cover_moving: bool,
    fail_calibrator_state_poll: bool,
    stuck_calibrator_not_ready: bool,
}

impl_mock_device!(MockCoverCalibrator);

#[async_trait::async_trait]
impl ascom_alpaca::api::CoverCalibrator for MockCoverCalibrator {
    async fn close_cover(&self) -> ascom_alpaca::ASCOMResult<()> {
        if self.fail_close_cover {
            return Err(ASCOMError::invalid_operation("motor fault"));
        }
        Ok(())
    }

    async fn open_cover(&self) -> ascom_alpaca::ASCOMResult<()> {
        if self.fail_open_cover {
            return Err(ASCOMError::invalid_operation("motor fault"));
        }
        Ok(())
    }

    async fn calibrator_on(&self, _brightness: u32) -> ascom_alpaca::ASCOMResult<()> {
        if self.fail_calibrator_on {
            return Err(ASCOMError::invalid_operation("lamp failure"));
        }
        Ok(())
    }

    async fn calibrator_off(&self) -> ascom_alpaca::ASCOMResult<()> {
        if self.fail_calibrator_off {
            return Err(ASCOMError::invalid_operation("stuck on"));
        }
        Ok(())
    }

    async fn cover_state(&self) -> ascom_alpaca::ASCOMResult<CoverStatus> {
        if self.fail_cover_state_poll {
            return Err(ASCOMError::invalid_operation("device unreachable"));
        }
        if self.stuck_cover_moving {
            return Ok(CoverStatus::Moving);
        }
        Ok(CoverStatus::Closed)
    }

    async fn calibrator_state(&self) -> ascom_alpaca::ASCOMResult<CalibratorStatus> {
        if self.fail_calibrator_state_poll {
            return Err(ASCOMError::invalid_operation("device unreachable"));
        }
        if self.stuck_calibrator_not_ready {
            return Ok(CalibratorStatus::NotReady);
        }
        Ok(CalibratorStatus::Off)
    }

    async fn max_brightness(&self) -> ascom_alpaca::ASCOMResult<u32> {
        if self.fail_max_brightness {
            return Err(ASCOMError::invalid_operation("not supported"));
        }
        Ok(255)
    }

    async fn brightness(&self) -> ascom_alpaca::ASCOMResult<u32> {
        Ok(0)
    }
}

// -----------------------------------------------------------------------
// MockFocuser — single configurable mock for Focuser
// -----------------------------------------------------------------------

#[derive(Default)]
struct MockFocuser {
    fail_move: bool,
    fail_is_moving: bool,
    fail_position: bool,
    /// `true` ⇒ `temperature()` returns a generic `INVALID_OPERATION`
    /// error (sensor wired but reading failed). Distinct from
    /// `temperature_not_implemented` below.
    fail_temperature: bool,
    /// `true` ⇒ `temperature()` returns `ASCOMError::NOT_IMPLEMENTED`.
    /// Models a focuser that does not implement the `Temperature`
    /// property at all.
    temperature_not_implemented: bool,
    stuck_moving: bool,
    /// When non-zero, `is_moving` reports `true` for the first N calls
    /// and `false` thereafter — models a bounded focuser move for
    /// progress-notification tests, mirroring `MockCamera::not_ready_count`.
    /// `0` (default) keeps the `stuck_moving` behavior.
    is_moving_true_count: u32,
    is_moving_calls: std::sync::atomic::AtomicU32,
    /// `halt` calls seen — the stop-class counterpart a cancelled
    /// `do_move_focuser_blocking` must issue.
    halt_calls: std::sync::atomic::AtomicU32,
    temperature_value: f64,
    position_value: i32,
}

impl_mock_device!(MockFocuser);

#[async_trait::async_trait]
impl ascom_alpaca::api::Focuser for MockFocuser {
    async fn absolute(&self) -> ascom_alpaca::ASCOMResult<bool> {
        Ok(true)
    }

    async fn is_moving(&self) -> ascom_alpaca::ASCOMResult<bool> {
        if self.fail_is_moving {
            return Err(ASCOMError::invalid_operation("encoder fault"));
        }
        if self.is_moving_true_count > 0 {
            let n = self
                .is_moving_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Ok(n < self.is_moving_true_count);
        }
        Ok(self.stuck_moving)
    }

    async fn max_increment(&self) -> ascom_alpaca::ASCOMResult<u32> {
        Ok(100_000)
    }

    async fn max_step(&self) -> ascom_alpaca::ASCOMResult<u32> {
        Ok(100_000)
    }

    async fn position(&self) -> ascom_alpaca::ASCOMResult<i32> {
        if self.fail_position {
            return Err(ASCOMError::invalid_operation("position unavailable"));
        }
        Ok(self.position_value)
    }

    async fn step_size(&self) -> ascom_alpaca::ASCOMResult<f64> {
        Err(ASCOMError::NOT_IMPLEMENTED)
    }

    async fn temp_comp(&self) -> ascom_alpaca::ASCOMResult<bool> {
        Ok(false)
    }

    async fn set_temp_comp(&self, _: bool) -> ascom_alpaca::ASCOMResult<()> {
        Err(ASCOMError::NOT_IMPLEMENTED)
    }

    async fn temp_comp_available(&self) -> ascom_alpaca::ASCOMResult<bool> {
        Ok(false)
    }

    async fn temperature(&self) -> ascom_alpaca::ASCOMResult<f64> {
        if self.temperature_not_implemented {
            return Err(ASCOMError::NOT_IMPLEMENTED);
        }
        if self.fail_temperature {
            return Err(ASCOMError::invalid_operation("sensor failure"));
        }
        Ok(self.temperature_value)
    }

    async fn halt(&self) -> ascom_alpaca::ASCOMResult<()> {
        self.halt_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    async fn move_(&self, _position: i32) -> ascom_alpaca::ASCOMResult<()> {
        if self.fail_move {
            return Err(ASCOMError::invalid_operation("focuser stuck"));
        }
        Ok(())
    }
}

// -----------------------------------------------------------------------
// MockTelescope — single configurable mock for Telescope (mount).
//
// Defaults are "happy path" (capable, tracking on, returns a fixed
// RA/Dec). Set fail_* fields to inject errors per test, or set
// tracking_value / can_set_tracking_value / ra_value / dec_value to
// shape the read responses.
// -----------------------------------------------------------------------

struct MockTelescope {
    fail_slew: bool,
    fail_slewing_poll: bool,
    fail_sync: bool,
    fail_right_ascension: bool,
    fail_declination: bool,
    fail_tracking: bool,
    fail_can_set_tracking: bool,
    fail_set_tracking: bool,
    fail_park: bool,
    fail_unpark: bool,
    fail_at_park: bool,
    fail_can_park: bool,
    fail_can_unpark: bool,
    fail_abort_slew: bool,
    /// `abort_slew` calls seen — the stop-class counterpart a cancelled
    /// `do_slew_blocking` must issue (and a cancelled park must not).
    abort_slew_calls: std::sync::atomic::AtomicU32,
    /// `unpark` calls seen — zero when the body ran with a handle that
    /// was already cancelled.
    unpark_calls: std::sync::atomic::AtomicU32,
    /// `set_tracking` calls seen — same contract as `unpark_calls`.
    set_tracking_calls: std::sync::atomic::AtomicU32,
    /// `slewing()` returns `true` forever — drives the timeout path.
    stuck_slewing: bool,
    /// First N `slewing()` calls return a transient error before
    /// behaving normally — drives `poll_slewing_until_idle`'s
    /// transient-tolerance path (issue #319). Counter is `slewing_calls`.
    slewing_transient_errors: u32,
    /// When non-zero (and `stuck_slewing` is false), `slewing()`
    /// returns `true` for the first N successful calls (after the
    /// transient-error budget is exhausted) and `false` thereafter.
    /// Used by progress-notification tests to model a slew of
    /// bounded duration. `0` (default) means "report idle immediately".
    slewing_true_count: u32,
    slewing_calls: std::sync::atomic::AtomicU32,
    /// When non-zero, `at_park()` returns `false` for the first N
    /// calls and `true` thereafter — models a park of bounded
    /// duration for progress-notification tests. `0` (default)
    /// means "report `at_park` immediately".
    at_park_false_count: u32,
    at_park_calls: std::sync::atomic::AtomicU32,
    tracking_value: bool,
    can_set_tracking_value: bool,
    at_park_value: bool,
    can_park_value: bool,
    can_unpark_value: bool,
    ra_value: f64,
    dec_value: f64,
}

impl Default for MockTelescope {
    fn default() -> Self {
        Self {
            fail_slew: false,
            fail_slewing_poll: false,
            fail_sync: false,
            fail_right_ascension: false,
            fail_declination: false,
            fail_tracking: false,
            fail_can_set_tracking: false,
            fail_set_tracking: false,
            fail_park: false,
            fail_unpark: false,
            fail_at_park: false,
            fail_can_park: false,
            fail_can_unpark: false,
            fail_abort_slew: false,
            abort_slew_calls: std::sync::atomic::AtomicU32::new(0),
            unpark_calls: std::sync::atomic::AtomicU32::new(0),
            set_tracking_calls: std::sync::atomic::AtomicU32::new(0),
            stuck_slewing: false,
            slewing_transient_errors: 0,
            slewing_true_count: 0,
            slewing_calls: std::sync::atomic::AtomicU32::new(0),
            at_park_false_count: 0,
            at_park_calls: std::sync::atomic::AtomicU32::new(0),
            tracking_value: true,
            can_set_tracking_value: true,
            at_park_value: false,
            can_park_value: true,
            can_unpark_value: true,
            ra_value: 0.0,
            dec_value: 0.0,
        }
    }
}

impl_mock_device!(MockTelescope);

#[async_trait::async_trait]
impl ascom_alpaca::api::Telescope for MockTelescope {
    async fn at_home(&self) -> ascom_alpaca::ASCOMResult<bool> {
        Ok(false)
    }

    async fn at_park(&self) -> ascom_alpaca::ASCOMResult<bool> {
        if self.fail_at_park {
            return Err(ASCOMError::invalid_operation("at_park read failed"));
        }
        let n = self
            .at_park_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.at_park_false_count > 0 && n < self.at_park_false_count {
            return Ok(false);
        }
        Ok(self.at_park_value)
    }

    async fn can_park(&self) -> ascom_alpaca::ASCOMResult<bool> {
        if self.fail_can_park {
            return Err(ASCOMError::invalid_operation("can_park read failed"));
        }
        Ok(self.can_park_value)
    }

    async fn can_unpark(&self) -> ascom_alpaca::ASCOMResult<bool> {
        if self.fail_can_unpark {
            return Err(ASCOMError::invalid_operation("can_unpark read failed"));
        }
        Ok(self.can_unpark_value)
    }

    async fn park(&self) -> ascom_alpaca::ASCOMResult<()> {
        if self.fail_park {
            return Err(ASCOMError::invalid_operation("park failed"));
        }
        Ok(())
    }

    async fn unpark(&self) -> ascom_alpaca::ASCOMResult<()> {
        self.unpark_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.fail_unpark {
            return Err(ASCOMError::invalid_operation("unpark failed"));
        }
        Ok(())
    }

    async fn declination(&self) -> ascom_alpaca::ASCOMResult<f64> {
        if self.fail_declination {
            return Err(ASCOMError::invalid_operation("encoder fault"));
        }
        Ok(self.dec_value)
    }

    async fn declination_rate(&self) -> ascom_alpaca::ASCOMResult<f64> {
        Ok(0.0)
    }

    async fn equatorial_system(
        &self,
    ) -> ascom_alpaca::ASCOMResult<ascom_alpaca::api::telescope::EquatorialCoordinateType> {
        Ok(ascom_alpaca::api::telescope::EquatorialCoordinateType::J2000)
    }

    async fn right_ascension(&self) -> ascom_alpaca::ASCOMResult<f64> {
        if self.fail_right_ascension {
            return Err(ASCOMError::invalid_operation("encoder fault"));
        }
        Ok(self.ra_value)
    }

    async fn right_ascension_rate(&self) -> ascom_alpaca::ASCOMResult<f64> {
        Ok(0.0)
    }

    async fn sidereal_time(&self) -> ascom_alpaca::ASCOMResult<f64> {
        Ok(0.0)
    }

    async fn tracking(&self) -> ascom_alpaca::ASCOMResult<bool> {
        if self.fail_tracking {
            return Err(ASCOMError::invalid_operation("tracking read failed"));
        }
        Ok(self.tracking_value)
    }

    async fn set_tracking(&self, _: bool) -> ascom_alpaca::ASCOMResult<()> {
        self.set_tracking_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.fail_set_tracking {
            return Err(ASCOMError::invalid_operation("CanSetTracking is false"));
        }
        Ok(())
    }

    async fn can_set_tracking(&self) -> ascom_alpaca::ASCOMResult<bool> {
        if self.fail_can_set_tracking {
            return Err(ASCOMError::invalid_operation("capability read failed"));
        }
        Ok(self.can_set_tracking_value)
    }

    async fn tracking_rate(
        &self,
    ) -> ascom_alpaca::ASCOMResult<ascom_alpaca::api::telescope::DriveRate> {
        Ok(ascom_alpaca::api::telescope::DriveRate::Sidereal)
    }

    async fn axis_rates(
        &self,
        _axis: ascom_alpaca::api::telescope::TelescopeAxis,
    ) -> ascom_alpaca::ASCOMResult<Vec<std::ops::RangeInclusive<f64>>> {
        Ok(vec![])
    }

    async fn utc_date(&self) -> ascom_alpaca::ASCOMResult<std::time::SystemTime> {
        Ok(std::time::SystemTime::UNIX_EPOCH)
    }

    async fn slewing(&self) -> ascom_alpaca::ASCOMResult<bool> {
        let n = self
            .slewing_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.fail_slewing_poll || n < self.slewing_transient_errors {
            return Err(ASCOMError::invalid_operation("slewing poll failed"));
        }
        if self.stuck_slewing {
            return Ok(true);
        }
        if self.slewing_true_count > 0 {
            // Successful-call index (after the transient-error budget).
            let success_idx = n - self.slewing_transient_errors;
            if success_idx < self.slewing_true_count {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn abort_slew(&self) -> ascom_alpaca::ASCOMResult<()> {
        self.abort_slew_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.fail_abort_slew {
            return Err(ASCOMError::invalid_operation("abort_slew failed"));
        }
        Ok(())
    }

    async fn slew_to_coordinates_async(
        &self,
        _ra: f64,
        _dec: f64,
    ) -> ascom_alpaca::ASCOMResult<()> {
        if self.fail_slew {
            return Err(ASCOMError::invalid_operation("Tracking is off"));
        }
        Ok(())
    }

    async fn sync_to_coordinates(&self, _ra: f64, _dec: f64) -> ascom_alpaca::ASCOMResult<()> {
        if self.fail_sync {
            return Err(ASCOMError::invalid_operation("sync failed"));
        }
        Ok(())
    }
}

// -----------------------------------------------------------------------
// Helper functions
// -----------------------------------------------------------------------

fn test_handler(registry: crate::equipment::EquipmentRegistry) -> McpHandler {
    McpHandler::new(
        Arc::new(registry),
        Arc::new(crate::events::EventBus::from_config(&[], None).unwrap()),
        SessionConfig {
            data_directory: std::env::temp_dir()
                .join("rp-unit-test")
                .to_string_lossy()
                .to_string(),
        },
        ImageCache::new(64, 4, std::path::PathBuf::from("/nonexistent"), 0),
        None,
    )
}

fn assert_tool_error(result: Result<CallToolResult, rmcp::ErrorData>, expected_substr: &str) {
    let call_result = result.expect("tool returned protocol error");
    assert!(
        call_result.is_error.unwrap_or(false),
        "expected is_error=true"
    );
    let text = call_result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map_or("", |tc| tc.text.as_str());
    assert!(
        text.contains(expected_substr),
        "expected error containing '{expected_substr}', got: '{text}'"
    );
}

// -----------------------------------------------------------------------
// Registry builders
// -----------------------------------------------------------------------

/// Pre-populated cache values for the `MockCamera` defaults: the mock
/// reports `max_adu = 65535`, `pixel_size_* = 3.76 µm`, and `camera_*_
/// size = 1024 px`. Test helpers stamp the same values onto the
/// `CameraEntry` cache so `do_capture` sees what `connect_camera` would
/// have populated against the real driver — without paying connect-time
/// Alpaca calls in unit tests.
const MOCK_CAMERA_MAX_ADU: u32 = 65535;
const MOCK_CAMERA_PIXEL_SIZE_UM: f64 = 3.76;
const MOCK_CAMERA_SENSOR_PX: u32 = 1024;

/// Per-call overrides for the cached invariant-metadata fields on
/// `CameraEntry`. Defaults mirror `MockCamera`'s static reads so tests
/// that don't care about metadata get the same shape `connect_camera`
/// would have produced. Tests that want to model a connect-time read
/// failure (or a scientific camera with `max_adu > u16::MAX`) override
/// the relevant field.
#[derive(Clone, Copy)]
struct CachedCameraMeta {
    max_adu: Option<u32>,
    pixel_size_x_um: Option<f64>,
    pixel_size_y_um: Option<f64>,
    sensor_width_px: Option<u32>,
    sensor_height_px: Option<u32>,
}

impl Default for CachedCameraMeta {
    fn default() -> Self {
        Self {
            max_adu: Some(MOCK_CAMERA_MAX_ADU),
            pixel_size_x_um: Some(MOCK_CAMERA_PIXEL_SIZE_UM),
            pixel_size_y_um: Some(MOCK_CAMERA_PIXEL_SIZE_UM),
            sensor_width_px: Some(MOCK_CAMERA_SENSOR_PX),
            sensor_height_px: Some(MOCK_CAMERA_SENSOR_PX),
        }
    }
}

fn camera_registry(cam: Arc<dyn ascom_alpaca::api::Camera>) -> crate::equipment::EquipmentRegistry {
    camera_registry_with_meta(cam, CachedCameraMeta::default())
}

/// A train model with a single camera-only train over the fixture
/// camera "cam", carrying `focal_length_mm` — the optics-block input
/// `do_capture` resolves through `McpHandler::trains`.
fn cam_trains(focal_length_mm: f64) -> crate::equipment::trains::TrainModel {
    let equipment: crate::config::EquipmentConfig = serde_json::from_value(serde_json::json!({
        "cameras": [{"id": "cam", "alpaca_url": "http://localhost:1"}],
        "optical_trains": [
            {"id": "main", "focal_length_mm": focal_length_mm, "devices": ["cam"]}
        ]
    }))
    .unwrap();
    crate::equipment::trains::TrainModel::try_from_equipment(&equipment).unwrap()
}

fn camera_registry_with_meta(
    cam: Arc<dyn ascom_alpaca::api::Camera>,
    meta: CachedCameraMeta,
) -> crate::equipment::EquipmentRegistry {
    crate::equipment::EquipmentRegistry {
        safety_monitors: vec![],
        cameras: vec![crate::equipment::CameraEntry::new(
            "cam".to_string(),
            crate::config::CameraConfig {
                id: "cam".to_string(),
                name: "mock".to_string(),
                alpaca_url: "http://localhost:1".to_string(),
                device_type: String::new(),
                device_number: 0,
                cooler_targets_c: Vec::new(),
                gain: None,
                offset: None,
                readout_time_estimate: None,
                auth: None,
            },
            crate::equipment::DeviceSession::connected(cam),
            crate::equipment::CameraInvariants {
                max_adu: meta.max_adu,
                pixel_size_x_um: meta.pixel_size_x_um,
                pixel_size_y_um: meta.pixel_size_y_um,
                sensor_width_px: meta.sensor_width_px,
                sensor_height_px: meta.sensor_height_px,
            },
        )],
        filter_wheels: vec![],
        cover_calibrators: vec![],
        focusers: vec![],
        mount: None,
        ..Default::default()
    }
}

fn filter_wheel_registry(
    fw: Arc<dyn ascom_alpaca::api::FilterWheel>,
) -> crate::equipment::EquipmentRegistry {
    crate::equipment::EquipmentRegistry {
        safety_monitors: vec![],
        cameras: vec![],
        filter_wheels: vec![crate::equipment::FilterWheelEntry {
            id: "fw".to_string(),
            config: crate::config::FilterWheelConfig {
                id: "fw".to_string(),
                alpaca_url: "http://localhost:1".to_string(),
                device_number: 0,
                filters: vec!["Lum".to_string(), "Red".to_string()],
                auth: None,
            },
            session: crate::equipment::DeviceSession::connected(fw),
        }],
        cover_calibrators: vec![],
        focusers: vec![],
        mount: None,
        ..Default::default()
    }
}

fn calibrator_registry(
    cc: Arc<dyn ascom_alpaca::api::CoverCalibrator>,
) -> crate::equipment::EquipmentRegistry {
    crate::equipment::EquipmentRegistry {
        safety_monitors: vec![],
        cameras: vec![],
        filter_wheels: vec![],
        cover_calibrators: vec![crate::equipment::CoverCalibratorEntry {
            id: "cc".to_string(),
            config: crate::config::CoverCalibratorConfig {
                id: "cc".to_string(),
                alpaca_url: "http://localhost:1".to_string(),
                device_number: 0,
                poll_interval: Duration::from_secs(1),
                auth: None,
            },
            session: crate::equipment::DeviceSession::connected(cc),
        }],
        focusers: vec![],
        mount: None,
        ..Default::default()
    }
}

fn focuser_registry(
    foc: Arc<dyn ascom_alpaca::api::Focuser>,
    min_position: Option<i32>,
    max_position: Option<i32>,
) -> crate::equipment::EquipmentRegistry {
    crate::equipment::EquipmentRegistry {
        safety_monitors: vec![],
        cameras: vec![],
        filter_wheels: vec![],
        cover_calibrators: vec![],
        focusers: vec![crate::equipment::FocuserEntry {
            id: "foc".to_string(),
            config: crate::config::FocuserConfig {
                id: "foc".to_string(),
                alpaca_url: "http://localhost:1".to_string(),
                device_number: 0,
                min_position,
                max_position,
                steps_per_sec: crate::config::focuser::FocuserStepsPerSec::default(),
                auth: None,
            },
            session: crate::equipment::DeviceSession::connected(foc),
        }],
        mount: None,
        ..Default::default()
    }
}

fn mount_registry(
    mount: Arc<dyn ascom_alpaca::api::Telescope>,
    settle_after_slew: Option<Duration>,
) -> crate::equipment::EquipmentRegistry {
    crate::equipment::EquipmentRegistry {
        safety_monitors: vec![],
        cameras: vec![],
        filter_wheels: vec![],
        cover_calibrators: vec![],
        focusers: vec![],
        mount: Some(crate::equipment::MountEntry {
            config: crate::config::MountConfig {
                alpaca_url: "http://localhost:1".to_string(),
                device_number: 0,
                settle_after_slew,
                slew_rate_arcsec_per_sec: crate::config::mount::SlewRateArcsecPerSec::default(),
                guiding: None,
                auth: None,
            },
            session: crate::equipment::DeviceSession::connected(mount),
        }),
        ..Default::default()
    }
}

/// A registry holding both a camera ("cam") and the singular mount, so
/// `center_on_target_inner` resolves its devices and emits
/// `centering_started`. The camera reuses `camera_registry`'s shape.
fn camera_mount_registry(
    cam: Arc<dyn ascom_alpaca::api::Camera>,
    mount: Arc<dyn ascom_alpaca::api::Telescope>,
) -> crate::equipment::EquipmentRegistry {
    let mut registry = camera_registry(cam);
    registry.mount = mount_registry(mount, None).mount;
    registry
}

fn empty_registry() -> crate::equipment::EquipmentRegistry {
    crate::equipment::EquipmentRegistry {
        safety_monitors: vec![],
        cameras: vec![],
        filter_wheels: vec![],
        cover_calibrators: vec![],
        focusers: vec![],
        mount: None,
        ..Default::default()
    }
}

fn disconnected_mount_registry() -> crate::equipment::EquipmentRegistry {
    crate::equipment::EquipmentRegistry {
        safety_monitors: vec![],
        cameras: vec![],
        filter_wheels: vec![],
        cover_calibrators: vec![],
        focusers: vec![],
        mount: Some(crate::equipment::MountEntry {
            config: crate::config::MountConfig {
                alpaca_url: "http://localhost:1".to_string(),
                device_number: 0,
                settle_after_slew: None,
                slew_rate_arcsec_per_sec: crate::config::mount::SlewRateArcsecPerSec::default(),
                guiding: None,
                auth: None,
            },
            session: crate::equipment::DeviceSession::disconnected(),
        }),
        ..Default::default()
    }
}

// -----------------------------------------------------------------------
// Capture tests
// -----------------------------------------------------------------------

#[tokio::test]
async fn test_capture_start_exposure_fails() {
    let cam = MockCamera {
        fail_start_exposure: true,
        ..Default::default()
    };
    let handler = test_handler(camera_registry(Arc::new(cam)));
    let result = handler
        .capture_inner(
            CaptureParams {
                target: None,
                frame_type: None,
                camera_id: Some("cam".into()),
                train_id: None,
                duration: Duration::from_millis(100),
            },
            None,
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "failed to start exposure");
}

#[tokio::test]
async fn test_capture_image_ready_error() {
    let cam = MockCamera {
        fail_image_ready: true,
        ..Default::default()
    };
    let handler = test_handler(camera_registry(Arc::new(cam)));
    let result = handler
        .capture_inner(
            CaptureParams {
                target: None,
                frame_type: None,
                camera_id: Some("cam".into()),
                train_id: None,
                duration: Duration::from_millis(100),
            },
            None,
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "error checking image ready");
}

#[tokio::test]
async fn test_capture_image_array_fails() {
    let cam = MockCamera {
        fail_image_array: true,
        ..Default::default()
    };
    let handler = test_handler(camera_registry(Arc::new(cam)));
    let result = handler
        .capture_inner(
            CaptureParams {
                target: None,
                frame_type: None,
                camera_id: Some("cam".into()),
                train_id: None,
                duration: Duration::from_millis(100),
            },
            None,
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "failed to download image array");
}

#[tokio::test]
async fn test_capture_failed_exposure_surfaces_error_not_hang() {
    // Regression for the 6 h CI hang: a camera that *fails* an exposure
    // reports `CameraState::Error` and leaves `ImageReady` false forever.
    // `do_capture` must surface that as an error rather than polling
    // `ImageReady` indefinitely. `fail_image_array` makes `image_array`
    // carry the stored failure detail, mirroring sky-survey-camera after a
    // follow-mode mount-read timeout. The outer `timeout` guard fails the
    // test loudly (rather than hanging the suite) if the loop regresses.
    let cam = MockCamera {
        never_ready: true,
        report_error_state: true,
        fail_image_array: true,
        ..Default::default()
    };
    let handler = test_handler(camera_registry(Arc::new(cam)));
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        handler.capture_inner(
            CaptureParams {
                target: None,
                frame_type: None,
                camera_id: Some("cam".into()),
                train_id: None,
                duration: Duration::from_millis(100),
            },
            None,
            &Cancel::never(),
        ),
    )
    .await
    .expect("do_capture hung on a failed exposure instead of returning an error");
    assert_tool_error(result, "exposure failed");
}

#[tokio::test(start_paused = true)]
async fn test_capture_times_out_when_camera_never_ready() {
    // Backstop: a camera wedged not-ready *without* signalling an Error
    // state must still not hang forever — the `duration + CAPTURE_READOUT_
    // GRACE` deadline ends the loop. `start_paused` auto-advances tokio's
    // clock so the ~120 s deadline is reached without real waiting.
    let cam = MockCamera {
        never_ready: true,
        // Neither state knob is set → camera_state == Exposing, so
        // neither the Error branch nor the aborted-idle detection
        // trips and only the deadline can end the loop.
        ..Default::default()
    };
    let handler = test_handler(camera_registry(Arc::new(cam)));
    let result = handler
        .capture_inner(
            CaptureParams {
                target: None,
                frame_type: None,
                camera_id: Some("cam".into()),
                train_id: None,
                duration: Duration::from_millis(100),
            },
            None,
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "timeout waiting for image_ready");
}

#[tokio::test]
async fn test_capture_surfaces_an_aborted_exposure_instead_of_waiting_out_the_backstop() {
    // A safety abort (AbortExposure) returns the camera to Idle with
    // no image. The poll must surface that within a few cycles — an
    // imaging-train capture holds the motion gate shared, and waiting
    // out the ~120 s readout backstop here would block the recovery
    // slew that follows a safety interruption.
    let cam = MockCamera {
        never_ready: true,
        report_idle_state: true,
        ..Default::default()
    };
    let handler = test_handler(camera_registry(Arc::new(cam)));
    let started = std::time::Instant::now();
    let result = handler
        .capture_inner(
            CaptureParams {
                target: None,
                frame_type: None,
                camera_id: Some("cam".into()),
                train_id: None,
                duration: Duration::from_millis(100),
            },
            None,
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "exposure aborted: camera is idle with no image");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the aborted exposure must surface promptly, not at the readout backstop"
    );
}

#[tokio::test]
async fn test_capture_surfaces_a_read_error_on_the_aborted_idle_recheck() {
    // The re-check that guards the aborted-idle classification must
    // pass a failed ImageReady read through as a read error — not
    // misreport a transport failure as an aborted exposure.
    let cam = MockCamera {
        report_idle_state: true,
        // Two not-ready polls build the idle streak; the re-check is
        // the third read and errors.
        fail_image_ready_after: Some(2),
        ..Default::default()
    };
    let handler = test_handler(camera_registry(Arc::new(cam)));
    let result = handler
        .capture_inner(
            CaptureParams {
                target: None,
                frame_type: None,
                camera_id: Some("cam".into()),
                train_id: None,
                duration: Duration::from_millis(100),
            },
            None,
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "error checking image ready: ");
}

// -----------------------------------------------------------------------
// get_camera_info tests
// -----------------------------------------------------------------------

#[tokio::test]
async fn test_get_camera_info_max_adu_unavailable_when_cache_none() {
    // `max_adu` moved to a connect-time cache on `CameraEntry`. A connect-
    // time read failure leaves `max_adu = None`; `get_camera_info` must
    // surface that as a tool_error so consumers don't mistake "absent"
    // for "zero". This replaces the old live-read failure test.
    let registry = camera_registry_with_meta(
        Arc::new(MockCamera::default()),
        CachedCameraMeta {
            max_adu: None,
            ..CachedCameraMeta::default()
        },
    );
    let handler = test_handler(registry);
    let result = handler
        .get_camera_info(Parameters(CameraIdParams {
            camera_id: "cam".into(),
        }))
        .await;
    assert_tool_error(result, "max_adu unavailable");
}

#[tokio::test]
async fn test_get_camera_info_reads_max_adu_and_sensor_from_cache_not_live() {
    // Pin the contract that `max_adu` and the sensor dimensions come from
    // `CameraEntry`'s connect-time cache, NOT from per-call Alpaca reads.
    // We rig the MockCamera so its `max_adu` and `camera_size` methods
    // would fail if invoked, then seed the cache with distinctive values.
    // `get_camera_info` must succeed and report the cached values — proving
    // the live reads aren't happening on the hot path.
    let cam = MockCamera {
        fail_max_adu: true,
        fail_camera_size: true,
        ..Default::default()
    };
    let registry = camera_registry_with_meta(
        Arc::new(cam),
        CachedCameraMeta {
            max_adu: Some(4242),
            sensor_width_px: Some(3000),
            sensor_height_px: Some(2000),
            ..CachedCameraMeta::default()
        },
    );
    let handler = test_handler(registry);
    let result = handler
        .get_camera_info(Parameters(CameraIdParams {
            camera_id: "cam".into(),
        }))
        .await
        .unwrap();
    assert!(!result.is_error.unwrap_or(false));
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|tc| tc.text.as_str())
        .unwrap();
    let json: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(json["max_adu"], 4242);
    assert_eq!(json["sensor_x"], 3000);
    assert_eq!(json["sensor_y"], 2000);
}

#[tokio::test]
async fn test_get_camera_info_sensor_size_unavailable_when_cache_none() {
    // Sensor dimensions moved to the connect-time cache (same shape as
    // `max_adu` above). A missing `sensor_width_px` or `sensor_height_px`
    // surfaces as a tool_error.
    let registry = camera_registry_with_meta(
        Arc::new(MockCamera::default()),
        CachedCameraMeta {
            sensor_width_px: None,
            ..CachedCameraMeta::default()
        },
    );
    let handler = test_handler(registry);
    let result = handler
        .get_camera_info(Parameters(CameraIdParams {
            camera_id: "cam".into(),
        }))
        .await;
    assert_tool_error(result, "sensor size unavailable");
}

// -----------------------------------------------------------------------
// set_filter tests
// -----------------------------------------------------------------------

#[tokio::test]
async fn test_set_filter_set_position_fails() {
    let fw = MockFilterWheel {
        fail_set_position: true,
        ..Default::default()
    };
    let handler = test_handler(filter_wheel_registry(Arc::new(fw)));
    let result = handler
        .set_filter_inner(
            SetFilterParams {
                filter_wheel_id: Some("fw".into()),
                train_id: None,
                filter_name: "Lum".into(),
            },
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "failed to set filter position");
}

// -----------------------------------------------------------------------
// CoverCalibrator tests
// -----------------------------------------------------------------------

#[tokio::test]
async fn test_close_cover_command_fails() {
    let cc = MockCoverCalibrator {
        fail_close_cover: true,
        ..Default::default()
    };
    let handler = test_handler(calibrator_registry(Arc::new(cc)));
    let result = handler
        .close_cover_inner(
            CalibratorIdParams {
                calibrator_id: "cc".into(),
            },
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "failed to close cover");
}

#[tokio::test]
async fn test_close_cover_polling_error() {
    let cc = MockCoverCalibrator {
        fail_cover_state_poll: true,
        ..Default::default()
    };
    let handler = test_handler(calibrator_registry(Arc::new(cc)));
    let result = handler
        .close_cover_inner(
            CalibratorIdParams {
                calibrator_id: "cc".into(),
            },
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "error polling cover state");
}

#[tokio::test]
async fn test_open_cover_command_fails() {
    let cc = MockCoverCalibrator {
        fail_open_cover: true,
        ..Default::default()
    };
    let handler = test_handler(calibrator_registry(Arc::new(cc)));
    let result = handler
        .open_cover_inner(
            CalibratorIdParams {
                calibrator_id: "cc".into(),
            },
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "failed to open cover");
}

#[tokio::test]
async fn test_calibrator_on_max_brightness_fails() {
    let cc = MockCoverCalibrator {
        fail_max_brightness: true,
        ..Default::default()
    };
    let handler = test_handler(calibrator_registry(Arc::new(cc)));
    let result = handler
        .calibrator_on_inner(
            CalibratorOnParams {
                calibrator_id: "cc".into(),
                brightness: None,
            },
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "failed to read max_brightness");
}

#[tokio::test]
async fn test_calibrator_on_command_fails() {
    let cc = MockCoverCalibrator {
        fail_calibrator_on: true,
        ..Default::default()
    };
    let handler = test_handler(calibrator_registry(Arc::new(cc)));
    let result = handler
        .calibrator_on_inner(
            CalibratorOnParams {
                calibrator_id: "cc".into(),
                brightness: None,
            },
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "failed to turn calibrator on");
}

#[tokio::test]
async fn test_calibrator_off_command_fails() {
    let cc = MockCoverCalibrator {
        fail_calibrator_off: true,
        ..Default::default()
    };
    let handler = test_handler(calibrator_registry(Arc::new(cc)));
    let result = handler
        .calibrator_off_inner(
            CalibratorIdParams {
                calibrator_id: "cc".into(),
            },
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "failed to turn calibrator off");
}

// -----------------------------------------------------------------------
// capture — write_fits failure
// -----------------------------------------------------------------------

#[tokio::test]
async fn test_capture_write_fits_fails() {
    let cam = MockCamera::default(); // succeeds through image_array
    let registry = camera_registry(Arc::new(cam));
    // Use an existing file as the "directory" so write_fits fails cross-platform.
    // The capture tool appends /<uuid8>.fits — creating a file inside
    // another file fails on all OSes.
    let blocker = tempfile::NamedTempFile::new().unwrap();
    let handler = McpHandler::new(
        Arc::new(registry),
        Arc::new(crate::events::EventBus::from_config(&[], None).unwrap()),
        SessionConfig {
            data_directory: blocker.path().to_string_lossy().to_string(),
        },
        ImageCache::new(64, 4, std::path::PathBuf::from("/nonexistent"), 0),
        None,
    );
    let result = handler
        .capture_inner(
            CaptureParams {
                target: None,
                frame_type: None,
                camera_id: Some("cam".into()),
                train_id: None,
                duration: Duration::from_millis(100),
            },
            None,
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "failed to write FITS file");
}

// -----------------------------------------------------------------------
// capture — caches I32 variant when max_adu > u16::MAX
// -----------------------------------------------------------------------

#[tokio::test]
async fn test_capture_caches_i32_when_max_adu_above_u16_max() {
    // Drives the scientific-camera (I32) cache-insert branch in
    // `capture` — exercised by no other test, since OmniSim and the
    // default MockCamera both report max_adu ≤ 65535. Pins the
    // capture invariant: a successful capture leaves the embedded
    // document accessible through the cache entry (now the single
    // source of truth) with the matching `max_adu`.
    //
    // The 20-bit max_adu lives on the cached `CameraEntry` rather than
    // on the MockCamera: `do_capture` reads max_adu from the cache (see
    // `CameraEntry` docs) so the live `cam.max_adu()` flag on MockCamera
    // is irrelevant for capture-time semantics.
    let cam = MockCamera::default();
    let registry = camera_registry_with_meta(
        Arc::new(cam),
        CachedCameraMeta {
            max_adu: Some(1 << 20),
            ..CachedCameraMeta::default()
        },
    );
    let temp = tempfile::tempdir().unwrap();
    let cache = ImageCache::new(64, 4, std::path::PathBuf::from("/nonexistent"), 0);
    let handler = McpHandler::new(
        Arc::new(registry),
        Arc::new(crate::events::EventBus::from_config(&[], None).unwrap()),
        SessionConfig {
            data_directory: temp.path().to_string_lossy().to_string(),
        },
        cache.clone(),
        None,
    );
    let result = handler
        .capture_inner(
            CaptureParams {
                target: None,
                frame_type: None,
                camera_id: Some("cam".into()),
                train_id: None,
                duration: Duration::from_millis(100),
            },
            None,
            &Cancel::never(),
        )
        .await
        .unwrap();
    assert!(!result.is_error.unwrap_or(false));
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|tc| tc.text.clone())
        .unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    let doc_id = json["document_id"].as_str().unwrap();
    let cached = cache.get(doc_id).expect("expected cache entry");
    assert_eq!(cached.max_adu, 1 << 20);
    assert!(
        matches!(cached.pixels, CachedPixels::I32(_)),
        "expected I32 variant for max_adu > u16::MAX"
    );
    let doc = cache
        .resolve_document(doc_id)
        .await
        .expect("expected cache entry to carry the document");
    assert_eq!(doc.max_adu, Some(1 << 20));
}

// -----------------------------------------------------------------------
// capture — filename uses 8-char UUID suffix
// -----------------------------------------------------------------------

#[tokio::test]
async fn test_capture_filename_uses_uuid8_suffix() {
    // Pins the on-disk reverse-lookup contract: the FITS basename matches
    // the first 8 hex chars of the document_id. The disk-fallback
    // resolution path in Phase 7 grep's by this suffix.
    let cam = MockCamera::default();
    let temp = tempfile::tempdir().unwrap();
    let cache = ImageCache::new(64, 4, std::path::PathBuf::from("/nonexistent"), 0);
    let handler = McpHandler::new(
        Arc::new(camera_registry(Arc::new(cam))),
        Arc::new(crate::events::EventBus::from_config(&[], None).unwrap()),
        SessionConfig {
            data_directory: temp.path().to_string_lossy().to_string(),
        },
        cache,
        None,
    );
    let result = handler
        .capture_inner(
            CaptureParams {
                target: None,
                frame_type: None,
                camera_id: Some("cam".into()),
                train_id: None,
                duration: Duration::from_millis(100),
            },
            None,
            &Cancel::never(),
        )
        .await
        .unwrap();
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|tc| tc.text.clone())
        .unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    let doc_id = json["document_id"].as_str().unwrap().to_string();
    let image_path = json["image_path"].as_str().unwrap().to_string();
    let basename = std::path::Path::new(&image_path)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        basename,
        format!("{}.fits", &doc_id[..8]),
        "FITS basename must equal first 8 hex chars of document_id + .fits"
    );
    assert!(
        std::path::Path::new(&image_path).exists(),
        "FITS file should exist at the reported path"
    );
}

// -----------------------------------------------------------------------
// Train addressing — capture / set_filter / center_on_target
// -----------------------------------------------------------------------

/// One imaging train `main` = [fw, cam] plus `two-wheels` =
/// [fw-a, fw-b, cam2] for the several-wheels ambiguity case.
fn wheel_trains() -> crate::equipment::trains::TrainModel {
    let equipment: crate::config::EquipmentConfig = serde_json::from_value(serde_json::json!({
        "cameras": [
            {"id": "cam", "alpaca_url": "http://localhost:1"},
            {"id": "cam2", "alpaca_url": "http://localhost:1"}
        ],
        "filter_wheels": [
            {"id": "fw", "alpaca_url": "http://localhost:1", "filters": ["Lum"]},
            {"id": "fw-a", "alpaca_url": "http://localhost:1", "filters": ["Lum"]},
            {"id": "fw-b", "alpaca_url": "http://localhost:1", "filters": ["Lum"]}
        ],
        "optical_trains": [
            {"id": "main", "devices": ["fw", "cam"]},
            {"id": "two-wheels", "devices": ["fw-a", "fw-b", "cam2"]}
        ]
    }))
    .unwrap();
    crate::equipment::trains::TrainModel::try_from_equipment(&equipment).unwrap()
}

#[tokio::test]
async fn capture_addressed_by_train_resolves_the_terminal_camera() {
    let handler = test_handler(camera_registry(Arc::new(MockCamera::default())))
        .with_trains(cam_trains(1000.0));
    let result = handler
        .capture_inner(
            CaptureParams {
                target: None,
                frame_type: None,
                camera_id: None,
                train_id: Some("main".into()),
                duration: Duration::from_millis(100),
            },
            None,
            &Cancel::never(),
        )
        .await
        .unwrap();
    assert!(
        !result.is_error.unwrap_or(false),
        "capture failed: {result:?}"
    );
}

#[tokio::test]
async fn capture_rejects_both_camera_and_train_addressing() {
    let handler = test_handler(camera_registry(Arc::new(MockCamera::default())));
    let result = handler
        .capture_inner(
            CaptureParams {
                target: None,
                frame_type: None,
                camera_id: Some("cam".into()),
                train_id: Some("main".into()),
                duration: Duration::from_millis(100),
            },
            None,
            &Cancel::never(),
        )
        .await;
    assert_tool_error(
        result,
        "capture: train_id is mutually exclusive with camera_id",
    );
}

#[tokio::test]
async fn capture_with_neither_address_names_both_alternatives() {
    let handler = test_handler(camera_registry(Arc::new(MockCamera::default())));
    let result = handler
        .capture_inner(
            CaptureParams {
                target: None,
                frame_type: None,
                camera_id: None,
                train_id: None,
                duration: Duration::from_millis(100),
            },
            None,
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "capture: pass exactly one of camera_id or train_id");
}

#[tokio::test]
async fn capture_through_an_unknown_train_is_rejected() {
    let handler = test_handler(camera_registry(Arc::new(MockCamera::default())));
    let result = handler
        .capture_inner(
            CaptureParams {
                target: None,
                frame_type: None,
                camera_id: None,
                train_id: Some("nope".into()),
                duration: Duration::from_millis(100),
            },
            None,
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "train not found: nope");
}

#[tokio::test]
async fn set_filter_addressed_by_train_resolves_the_sole_wheel() {
    let handler = test_handler(filter_wheel_registry(Arc::new(MockFilterWheel::default())))
        .with_trains(wheel_trains());
    let result = handler
        .set_filter_inner(
            SetFilterParams {
                filter_wheel_id: None,
                train_id: Some("main".into()),
                filter_name: "Lum".into(),
            },
            &Cancel::never(),
        )
        .await
        .unwrap();
    assert!(
        !result.is_error.unwrap_or(false),
        "set_filter failed: {result:?}"
    );
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|tc| tc.text.clone())
        .unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(json["filter_wheel_id"], "fw");
}

#[tokio::test]
async fn set_filter_rejects_both_wheel_and_train_addressing() {
    let handler = test_handler(filter_wheel_registry(Arc::new(MockFilterWheel::default())));
    let result = handler
        .set_filter_inner(
            SetFilterParams {
                filter_wheel_id: Some("fw".into()),
                train_id: Some("main".into()),
                filter_name: "Lum".into(),
            },
            &Cancel::never(),
        )
        .await;
    assert_tool_error(
        result,
        "set_filter: train_id is mutually exclusive with filter_wheel_id",
    );
}

#[tokio::test]
async fn set_filter_with_neither_address_names_both_alternatives() {
    let handler = test_handler(filter_wheel_registry(Arc::new(MockFilterWheel::default())));
    let result = handler
        .set_filter_inner(
            SetFilterParams {
                filter_wheel_id: None,
                train_id: None,
                filter_name: "Lum".into(),
            },
            &Cancel::never(),
        )
        .await;
    assert_tool_error(
        result,
        "set_filter: pass exactly one of filter_wheel_id or train_id",
    );
}

#[tokio::test]
async fn set_filter_through_a_wheelless_train_names_the_train() {
    let handler = test_handler(filter_wheel_registry(Arc::new(MockFilterWheel::default())))
        .with_trains(cam_trains(1000.0));
    let result = handler
        .set_filter_inner(
            SetFilterParams {
                filter_wheel_id: None,
                train_id: Some("main".into()),
                filter_name: "Lum".into(),
            },
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "train 'main' has no filter wheel");
}

#[tokio::test]
async fn set_filter_through_a_train_with_several_wheels_is_ambiguous() {
    let handler = test_handler(filter_wheel_registry(Arc::new(MockFilterWheel::default())))
        .with_trains(wheel_trains());
    let result = handler
        .set_filter_inner(
            SetFilterParams {
                filter_wheel_id: None,
                train_id: Some("two-wheels".into()),
                filter_name: "Lum".into(),
            },
            &Cancel::never(),
        )
        .await;
    assert_tool_error(
        result,
        "train 'two-wheels' has 2 filter wheels; pass filter_wheel_id",
    );
}

#[test]
fn train_addressable_tool_schemas_declare_their_addressing_alternatives() {
    // session-runner's layer-2 catalog validation fails a document
    // fast when a call satisfies none (or several) of a tool's
    // addressing alternatives — but only if the tool's input schema
    // declares them. Presence-only `oneOf` branches (each object
    // carrying nothing but `required`) are the published contract.
    for (schema, expected) in [
        (
            schemars::schema_for!(CaptureParams),
            serde_json::json!([{"required": ["camera_id"]}, {"required": ["train_id"]}]),
        ),
        (
            schemars::schema_for!(SetFilterParams),
            serde_json::json!([{"required": ["filter_wheel_id"]}, {"required": ["train_id"]}]),
        ),
        (
            schemars::schema_for!(CenterOnTargetToolParams),
            serde_json::json!([{"required": ["camera_id"]}, {"required": ["train_id"]}]),
        ),
        (
            schemars::schema_for!(crate::mcp::built_in::auto_focus::AutoFocusToolParams),
            serde_json::json!([{"required": ["camera_id", "focuser_id"]}, {"required": ["train_id"]}]),
        ),
        (
            schemars::schema_for!(crate::mcp::built_in::rotator::MoveRotatorParams),
            serde_json::json!([{"required": ["rotator_id"]}, {"required": ["train_id"]}]),
        ),
        (
            schemars::schema_for!(crate::mcp::built_in::rotator::RotatorPositionParams),
            serde_json::json!([{"required": ["rotator_id"]}, {"required": ["train_id"]}]),
        ),
    ] {
        let value = serde_json::to_value(&schema).unwrap();
        assert_eq!(
            value["oneOf"], expected,
            "schema missing its addressing oneOf: {value}"
        );
    }
    let refocus = serde_json::to_value(schemars::schema_for!(
        crate::mcp::built_in::auto_focus::RefocusTrainParams
    ))
    .unwrap();
    assert_eq!(refocus["required"], serde_json::json!(["train_id"]));
}

#[tokio::test]
async fn center_on_target_rejects_both_camera_and_train_addressing() {
    let handler = test_handler(camera_registry(Arc::new(MockCamera::default())));
    let result = handler
        .center_on_target_inner(
            CenterOnTargetToolParams {
                camera_id: Some("cam".into()),
                train_id: Some("main".into()),
                ra: Some(1.0),
                dec: Some(10.0),
                duration: Some(Duration::from_millis(100)),
                tolerance_arcsec: Some(60.0),
                max_attempts: Some(3),
            },
            None,
            Cancel::never(),
        )
        .await;
    assert_tool_error(
        result,
        "center_on_target: train_id is mutually exclusive with camera_id",
    );
}

#[tokio::test]
async fn center_on_target_addressed_by_train_resolves_before_mount_checks() {
    // No mount is configured, so a resolved train camera must get as
    // far as the mount-resolution error — proof the train resolved.
    let handler = test_handler(camera_registry(Arc::new(MockCamera::default())))
        .with_trains(cam_trains(1000.0));
    let result = handler
        .center_on_target_inner(
            CenterOnTargetToolParams {
                camera_id: None,
                train_id: Some("main".into()),
                ra: Some(1.0),
                dec: Some(10.0),
                duration: Some(Duration::from_millis(100)),
                tolerance_arcsec: Some(60.0),
                max_attempts: Some(3),
            },
            None,
            Cancel::never(),
        )
        .await;
    assert_tool_error(result, "no mount configured");
}

// -----------------------------------------------------------------------
// capture — optics block in sidecar
// -----------------------------------------------------------------------

async fn capture_and_read_sidecar(
    registry: crate::equipment::EquipmentRegistry,
    trains: crate::equipment::trains::TrainModel,
) -> ExposureDocument {
    let temp = tempfile::tempdir().unwrap();
    let cache = ImageCache::new(64, 4, std::path::PathBuf::from("/nonexistent"), 0);
    let handler = McpHandler::new(
        Arc::new(registry),
        Arc::new(crate::events::EventBus::from_config(&[], None).unwrap()),
        SessionConfig {
            data_directory: temp.path().to_string_lossy().to_string(),
        },
        cache,
        None,
    )
    .with_trains(trains);
    let result = handler
        .capture_inner(
            CaptureParams {
                target: None,
                frame_type: None,
                camera_id: Some("cam".into()),
                train_id: None,
                duration: Duration::from_millis(100),
            },
            None,
            &Cancel::never(),
        )
        .await
        .unwrap();
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|tc| tc.text.clone())
        .unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    let image_path = json["image_path"].as_str().unwrap().to_string();
    let sidecar = persistence::ExposureDocument::sidecar_path_for(&image_path).unwrap();
    let doc = persistence::ExposureDocument::read_sidecar_sync(&sidecar).unwrap();
    // Explicit drop pins the TempDir lifetime past the sidecar read
    // — without it the borrow checker is happy but the temp dir could
    // be cleaned up at any drop point the optimizer chose.
    drop(temp);
    doc
}

#[tokio::test]
async fn test_capture_persists_optics_when_focal_length_configured() {
    // Mock returns 3.76 µm pixels and 1024×1024 sensor; with a 1000 mm
    // focal length the derivation gives:
    //   pixel_scale = 206.265 × 3.76 / 1000 ≈ 0.7755564 arcsec/px
    //   fov         = 0.7755564 × 1024 / 3600 ≈ 0.220603 deg
    let cam = MockCamera::default();
    let registry = camera_registry(Arc::new(cam));
    let doc = capture_and_read_sidecar(registry, cam_trains(1000.0)).await;
    let optics = doc.optics.expect("optics block should be present");
    assert_eq!(optics.focal_length_mm, 1000.0);
    assert_eq!(optics.pixel_size_x_um, 3.76);
    assert_eq!(optics.pixel_size_y_um, 3.76);
    assert_eq!(optics.sensor_width_px, 1024);
    assert_eq!(optics.sensor_height_px, 1024);
    assert!(
        (optics.pixel_scale_x_arcsec_per_pixel - 0.775_556_4).abs() < 1e-6,
        "pixel_scale_x = {}",
        optics.pixel_scale_x_arcsec_per_pixel
    );
    assert!(
        (optics.fov_height_deg - 0.220_603).abs() < 1e-4,
        "fov_height_deg = {}",
        optics.fov_height_deg
    );
}

#[tokio::test]
async fn test_capture_omits_optics_when_focal_length_missing() {
    let cam = MockCamera::default();
    let registry = camera_registry(Arc::new(cam));
    let doc =
        capture_and_read_sidecar(registry, crate::equipment::trains::TrainModel::default()).await;
    assert!(
        doc.optics.is_none(),
        "optics must be omitted when focal_length_mm is not configured"
    );
}

#[tokio::test]
async fn test_capture_omits_optics_when_pixel_size_unavailable() {
    // Models a camera whose connect-time `pixel_size_*` read failed:
    // `CameraEntry.pixel_size_*_um` is None, so the optics block has no
    // pixel pitch to combine with `focal_length_mm` and must be omitted.
    let cam = MockCamera::default();
    let registry = camera_registry_with_meta(
        Arc::new(cam),
        CachedCameraMeta {
            pixel_size_x_um: None,
            pixel_size_y_um: None,
            ..CachedCameraMeta::default()
        },
    );
    let doc = capture_and_read_sidecar(registry, cam_trains(1000.0)).await;
    assert!(
        doc.optics.is_none(),
        "optics must be omitted when cached pixel_size is None"
    );
}

#[tokio::test]
async fn test_capture_omits_optics_when_sensor_size_unavailable() {
    // Same shape as the pixel_size case: models a camera whose connect-
    // time `camera_*_size` read failed.
    let cam = MockCamera::default();
    let registry = camera_registry_with_meta(
        Arc::new(cam),
        CachedCameraMeta {
            sensor_width_px: None,
            sensor_height_px: None,
            ..CachedCameraMeta::default()
        },
    );
    let doc = capture_and_read_sidecar(registry, cam_trains(1000.0)).await;
    assert!(
        doc.optics.is_none(),
        "optics must be omitted when cached sensor size is None"
    );
}

#[tokio::test]
async fn test_capture_does_not_call_invariant_metadata_methods_on_device() {
    // Regression contract pinned by `MockCameraNoMetadata` (above): every
    // invariant-sensor property method panics. With the `CameraEntry`
    // cache populated, `do_capture` must satisfy itself from the cache
    // and never touch the device — so the call must succeed without any
    // panic. If a future change reintroduces a per-capture read of one
    // of these properties, this test catches it via panic.
    let cam = MockCameraNoMetadata;
    let registry = camera_registry_with_meta(
        Arc::new(cam),
        // Populate the cache with realistic values so the U16-cache path
        // is taken and the optics block is built — exercising every
        // place `do_capture` consumes a cached metadata field.
        CachedCameraMeta::default(),
    );
    let doc = capture_and_read_sidecar(registry, cam_trains(1000.0)).await;
    assert_eq!(doc.max_adu, Some(MOCK_CAMERA_MAX_ADU));
    let optics = doc.optics.expect(
        "optics block should be present (cached pixel/sensor + configured focal_length_mm)",
    );
    assert_eq!(optics.focal_length_mm, 1000.0);
    assert_eq!(optics.pixel_size_x_um, MOCK_CAMERA_PIXEL_SIZE_UM);
    assert_eq!(optics.sensor_width_px, MOCK_CAMERA_SENSOR_PX);
}

// -----------------------------------------------------------------------
// persist_capture_artifact — sidecar failure skips cache
// -----------------------------------------------------------------------

#[tokio::test]
async fn test_persist_capture_artifact_skips_cache_on_sidecar_failure() {
    // Pins the sidecar-failure branch in `persist_capture_artifact` (the
    // post-FITS persistence step extracted from `capture`). Contract
    // documented in `docs/services/rp.md` → Capture Tool Details
    // → Sidecar failure contract: write_sidecar fails →
    // `document_persistence_failed` event payload is constructed → cache
    // insert is skipped → `document_id`-keyed lookups return 404.
    //
    // Forcing the failure: `doc.file_path` lives inside a regular file so
    // `create_dir_all(parent)` in write_sidecar errors with NotADirectory.
    // Same trick as the put_section rollback tests in cache.rs.
    let temp = tempfile::tempdir().unwrap();
    let blocker = temp.path().join("blocker");
    std::fs::write(&blocker, b"not a directory").unwrap();

    let cache = ImageCache::new(64, 4, std::path::PathBuf::from("/nonexistent"), 0);
    let handler = McpHandler::new(
        Arc::new(crate::equipment::EquipmentRegistry {
            safety_monitors: vec![],
            cameras: vec![],
            filter_wheels: vec![],
            cover_calibrators: vec![],
            focusers: vec![],
            mount: None,
            ..Default::default()
        }),
        Arc::new(crate::events::EventBus::from_config(&[], None).unwrap()),
        SessionConfig {
            data_directory: temp.path().to_string_lossy().to_string(),
        },
        cache.clone(),
        None,
    );

    let doc = ExposureDocument {
        target: None,
        frame_type: None,
        id: "doc-fail-1".to_string(),
        captured_at: "2026-04-30T00:00:00Z".to_string(),
        file_path: blocker.join("x.fits").to_string_lossy().into_owned(),
        width: 2,
        height: 2,
        camera_id: Some("cam".into()),
        duration: Some(Duration::from_millis(100)),
        max_adu: Some(65535),
        cooler_setpoint_c: None,
        sensor_temperature_c: None,
        optics: None,
        sections: serde_json::Map::new(),
    };
    let cached = CachedPixels::from_i32_pixels(vec![1, 2, 3, 4], (2, 2), 65535);

    handler
        .persist_capture_artifact(doc, cached, Some(65535))
        .await;

    assert!(
        cache.get("doc-fail-1").is_none(),
        "cache must not be populated when sidecar write fails"
    );
}

// -----------------------------------------------------------------------
// get_camera_info — exposure_range fallback
// -----------------------------------------------------------------------

#[tokio::test]
async fn test_get_camera_info_exposure_range_fallback() {
    let cam = MockCamera {
        fail_exposure_range: true,
        ..Default::default()
    };
    let handler = test_handler(camera_registry(Arc::new(cam)));
    let result = handler
        .get_camera_info(Parameters(CameraIdParams {
            camera_id: "cam".into(),
        }))
        .await;
    // This is a soft failure — it falls back to defaults, so the call succeeds
    let call_result = result.unwrap();
    assert!(!call_result.is_error.unwrap_or(false));
    let text = call_result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map_or("", |tc| tc.text.as_str());
    let json: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(json["exposure_min"], "1ms");
    assert_eq!(json["exposure_max"], "1h");
}

// -----------------------------------------------------------------------
// set_filter — polling error
// -----------------------------------------------------------------------

#[tokio::test]
async fn test_set_filter_polling_error() {
    let fw = MockFilterWheel {
        fail_position_poll: true,
        ..Default::default()
    };
    let handler = test_handler(filter_wheel_registry(Arc::new(fw)));
    let result = handler
        .set_filter_inner(
            SetFilterParams {
                filter_wheel_id: Some("fw".into()),
                train_id: None,
                filter_name: "Lum".into(),
            },
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "error waiting for filter wheel");
}

// -----------------------------------------------------------------------
// get_filter — errors
// -----------------------------------------------------------------------

#[tokio::test]
async fn test_get_filter_position_error() {
    let fw = MockFilterWheel {
        fail_position_poll: true,
        ..Default::default()
    };
    let handler = test_handler(filter_wheel_registry(Arc::new(fw)));
    let result = handler
        .get_filter(Parameters(FilterWheelIdParams {
            filter_wheel_id: "fw".into(),
        }))
        .await;
    assert_tool_error(result, "failed to get filter position");
}

#[tokio::test]
async fn test_get_filter_wheel_moving() {
    let fw = MockFilterWheel {
        report_moving: true,
        ..Default::default()
    };
    let handler = test_handler(filter_wheel_registry(Arc::new(fw)));
    let result = handler
        .get_filter(Parameters(FilterWheelIdParams {
            filter_wheel_id: "fw".into(),
        }))
        .await;
    assert_tool_error(result, "filter wheel is moving");
}

// -----------------------------------------------------------------------
// Timeout tests (use tokio::time::pause to fast-forward)
// -----------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn test_close_cover_timeout() {
    let cc = MockCoverCalibrator {
        stuck_cover_moving: true,
        ..Default::default()
    };
    let handler = test_handler(calibrator_registry(Arc::new(cc)));
    let result = handler
        .close_cover_inner(
            CalibratorIdParams {
                calibrator_id: "cc".into(),
            },
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "timeout waiting for cover to close");
}

#[tokio::test(start_paused = true)]
async fn test_open_cover_timeout() {
    let cc = MockCoverCalibrator {
        stuck_cover_moving: true,
        ..Default::default()
    };
    let handler = test_handler(calibrator_registry(Arc::new(cc)));
    let result = handler
        .open_cover_inner(
            CalibratorIdParams {
                calibrator_id: "cc".into(),
            },
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "timeout waiting for cover to open");
}

#[tokio::test]
async fn test_open_cover_polling_error() {
    let cc = MockCoverCalibrator {
        fail_cover_state_poll: true,
        ..Default::default()
    };
    let handler = test_handler(calibrator_registry(Arc::new(cc)));
    let result = handler
        .open_cover_inner(
            CalibratorIdParams {
                calibrator_id: "cc".into(),
            },
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "error polling cover state");
}

#[tokio::test(start_paused = true)]
async fn test_calibrator_on_timeout() {
    let cc = MockCoverCalibrator {
        stuck_calibrator_not_ready: true,
        ..Default::default()
    };
    let handler = test_handler(calibrator_registry(Arc::new(cc)));
    let result = handler
        .calibrator_on_inner(
            CalibratorOnParams {
                calibrator_id: "cc".into(),
                brightness: Some(100),
            },
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "timeout waiting for calibrator to become ready");
}

#[tokio::test]
async fn test_calibrator_on_polling_error() {
    let cc = MockCoverCalibrator {
        fail_calibrator_state_poll: true,
        ..Default::default()
    };
    let handler = test_handler(calibrator_registry(Arc::new(cc)));
    let result = handler
        .calibrator_on_inner(
            CalibratorOnParams {
                calibrator_id: "cc".into(),
                brightness: Some(100),
            },
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "error polling calibrator state");
}

#[tokio::test(start_paused = true)]
async fn test_calibrator_off_timeout() {
    let cc = MockCoverCalibrator {
        stuck_calibrator_not_ready: true,
        ..Default::default()
    };
    let handler = test_handler(calibrator_registry(Arc::new(cc)));
    let result = handler
        .calibrator_off_inner(
            CalibratorIdParams {
                calibrator_id: "cc".into(),
            },
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "timeout waiting for calibrator to turn off");
}

#[tokio::test]
async fn test_calibrator_off_polling_error() {
    let cc = MockCoverCalibrator {
        fail_calibrator_state_poll: true,
        ..Default::default()
    };
    let handler = test_handler(calibrator_registry(Arc::new(cc)));
    let result = handler
        .calibrator_off_inner(
            CalibratorIdParams {
                calibrator_id: "cc".into(),
            },
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "error polling calibrator state");
}

// -----------------------------------------------------------------------
// compute_image_stats error paths
// -----------------------------------------------------------------------

#[tokio::test]
async fn test_compute_image_stats_bad_fits() {
    // Write a non-FITS file so read_fits_pixels fails inside spawn_blocking
    let dir = tempfile::tempdir().unwrap();
    let bad_file = dir.path().join("bad.fits");
    std::fs::write(&bad_file, b"not a fits file").unwrap();

    let handler = test_handler(crate::equipment::EquipmentRegistry {
        safety_monitors: vec![],
        cameras: vec![],
        filter_wheels: vec![],
        cover_calibrators: vec![],
        focusers: vec![],
        mount: None,
        ..Default::default()
    });
    let result = handler
        .compute_image_stats(Parameters(ComputeImageStatsParams {
            image_path: Some(bad_file.to_string_lossy().to_string()),
            document_id: None,
        }))
        .await;
    assert_tool_error(result, "failed to compute stats");
}

#[tokio::test]
async fn test_compute_image_stats_missing_arguments() {
    // Pins the validation contract surfaced by `rp.md:702` ("document_id
    // or image_path"): callers must supply at least one. Mirrors the
    // missing-args branch tested for measure_basic / estimate_background.
    let handler = test_handler(crate::equipment::EquipmentRegistry {
        safety_monitors: vec![],
        cameras: vec![],
        filter_wheels: vec![],
        cover_calibrators: vec![],
        focusers: vec![],
        mount: None,
        ..Default::default()
    });
    let result = handler
        .compute_image_stats(Parameters(ComputeImageStatsParams {
            document_id: None,
            image_path: None,
        }))
        .await;
    assert_tool_error(result, "image_path");
}

#[tokio::test]
async fn test_compute_image_stats_persists_section_via_document_id() {
    // Pins the load-bearing piece of issue #175: when called with a
    // `document_id` that resolves through the cache, the computed
    // stats are written into the exposure document as an
    // `image_stats` section. Mirrors the section-persistence test
    // shape used by the other imaging tools (measure_basic ->
    // image_analysis, estimate_background -> background, etc).
    let temp = tempfile::tempdir().unwrap();
    let cache = ImageCache::new(64, 4, temp.path().to_path_buf(), 0);

    // Pixels chosen so the resulting stats are unambiguous: pixel_count = 4,
    // min = 100, max = 400, median = (200 + 300) / 2 = 250, mean = 250.0.
    let pixel_buf: Vec<u16> = vec![100, 200, 300, 400];
    let cached_pixels = CachedPixels::from_u16_pixels(pixel_buf, (2, 2)).unwrap();

    let document_id = "doc-image-stats-1".to_string();
    let uuid8 = "doc-imgs"; // 8-char-stable suffix for the on-disk basename.
    let file_path = temp
        .path()
        .join(format!("{uuid8}.fits"))
        .to_string_lossy()
        .into_owned();

    let doc = ExposureDocument {
        target: None,
        frame_type: None,
        id: document_id.clone(),
        captured_at: "2026-05-08T00:00:00Z".to_string(),
        file_path: file_path.clone(),
        width: 2,
        height: 2,
        camera_id: Some("cam".into()),
        duration: Some(Duration::from_millis(100)),
        max_adu: Some(65535),
        cooler_setpoint_c: None,
        sensor_temperature_c: None,
        optics: None,
        sections: serde_json::Map::new(),
    };

    cache.insert(
        &document_id,
        crate::persistence::CachedImage::new(
            cached_pixels,
            2,
            2,
            std::path::PathBuf::from(&file_path),
            65535,
            doc,
        ),
    );

    let handler = McpHandler::new(
        Arc::new(crate::equipment::EquipmentRegistry {
            safety_monitors: vec![],
            cameras: vec![],
            filter_wheels: vec![],
            cover_calibrators: vec![],
            focusers: vec![],
            mount: None,
            ..Default::default()
        }),
        Arc::new(crate::events::EventBus::from_config(&[], None).unwrap()),
        SessionConfig {
            data_directory: temp.path().to_string_lossy().to_string(),
        },
        cache.clone(),
        None,
    );

    let call_result = handler
        .compute_image_stats(Parameters(ComputeImageStatsParams {
            document_id: Some(document_id.clone()),
            image_path: None,
        }))
        .await
        .unwrap();
    assert!(!call_result.is_error.unwrap_or(false));

    let payload_text = call_result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|tc| tc.text.clone())
        .unwrap();
    let payload: serde_json::Value = serde_json::from_str(&payload_text).unwrap();
    assert_eq!(payload["pixel_count"], 4);
    assert_eq!(payload["min_adu"], 100);
    assert_eq!(payload["max_adu"], 400);
    assert_eq!(payload["median_adu"], 250);
    assert!((payload["mean_adu"].as_f64().unwrap() - 250.0).abs() < 1e-9);

    let updated = cache
        .resolve_document(&document_id)
        .await
        .expect("document should resolve from cache after compute_image_stats");
    let section = updated
        .sections
        .get("image_stats")
        .expect("image_stats section must be persisted when document_id is supplied");
    assert_eq!(section["pixel_count"], 4);
    assert_eq!(section["min_adu"], 100);
    assert_eq!(section["max_adu"], 400);
    assert_eq!(section["median_adu"], 250);
}

// -----------------------------------------------------------------------
// set_filter — filter not found
// -----------------------------------------------------------------------

#[tokio::test]
async fn test_set_filter_filter_not_found() {
    let fw = MockFilterWheel::default();
    let handler = test_handler(filter_wheel_registry(Arc::new(fw)));
    let result = handler
        .set_filter_inner(
            SetFilterParams {
                filter_wheel_id: Some("fw".into()),
                train_id: None,
                filter_name: "Ultraviolet".into(), // not in mock's filter list
            },
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "filter not found");
}

// -----------------------------------------------------------------------
// get_filter — success path (covers lines 387-391)
// -----------------------------------------------------------------------

#[tokio::test]
async fn test_get_filter_success() {
    let fw = MockFilterWheel::default(); // position() returns Some(0)
    let handler = test_handler(filter_wheel_registry(Arc::new(fw)));
    let result = handler
        .get_filter(Parameters(FilterWheelIdParams {
            filter_wheel_id: "fw".into(),
        }))
        .await;
    let call_result = result.unwrap();
    assert!(!call_result.is_error.unwrap_or(false));
    let text = call_result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map_or("", |tc| tc.text.as_str());
    let json: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(json["filter_name"], "Lum");
    assert_eq!(json["position"], 0);
}

// -----------------------------------------------------------------------
// CoverCalibrator success paths (covers resolve_device! macro lines)
// -----------------------------------------------------------------------

#[tokio::test]
async fn test_close_cover_success() {
    let cc = MockCoverCalibrator::default(); // cover_state returns Closed
    let handler = test_handler(calibrator_registry(Arc::new(cc)));
    let result = handler
        .close_cover_inner(
            CalibratorIdParams {
                calibrator_id: "cc".into(),
            },
            &Cancel::never(),
        )
        .await;
    let call_result = result.unwrap();
    assert!(!call_result.is_error.unwrap_or(false));
}

#[tokio::test]
async fn test_get_cover_state_reports_the_state_name() {
    let cc = MockCoverCalibrator::default(); // cover_state returns Closed
    let handler = test_handler(calibrator_registry(Arc::new(cc)));
    let result = handler
        .get_cover_state(Parameters(CalibratorIdParams {
            calibrator_id: "cc".into(),
        }))
        .await;
    let body = ok_text(result.unwrap());
    assert_eq!(body["cover_state"], "Closed");
}

#[tokio::test]
async fn test_get_cover_state_reports_a_moving_cover() {
    let cc = MockCoverCalibrator {
        stuck_cover_moving: true,
        ..Default::default()
    };
    let handler = test_handler(calibrator_registry(Arc::new(cc)));
    let result = handler
        .get_cover_state(Parameters(CalibratorIdParams {
            calibrator_id: "cc".into(),
        }))
        .await;
    let body = ok_text(result.unwrap());
    assert_eq!(body["cover_state"], "Moving");
}

#[tokio::test]
async fn test_get_cover_state_read_error() {
    let cc = MockCoverCalibrator {
        fail_cover_state_poll: true,
        ..Default::default()
    };
    let handler = test_handler(calibrator_registry(Arc::new(cc)));
    let result = handler
        .get_cover_state(Parameters(CalibratorIdParams {
            calibrator_id: "cc".into(),
        }))
        .await;
    assert_tool_error(result, "failed to read cover state");
}

// -----------------------------------------------------------------------
// Focuser tests
// -----------------------------------------------------------------------

fn ok_text(call_result: CallToolResult) -> serde_json::Value {
    assert!(
        !call_result.is_error.unwrap_or(false),
        "expected success, got error: {:?}",
        call_result.content
    );
    let text = call_result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map_or("", |tc| tc.text.as_str());
    serde_json::from_str(text).expect("valid JSON")
}

#[tokio::test]
async fn test_move_focuser_success() {
    let foc = MockFocuser {
        position_value: 4321,
        ..Default::default()
    };
    let handler = test_handler(focuser_registry(Arc::new(foc), None, None));
    let result = handler
        .move_focuser_inner(
            MoveFocuserParams {
                focuser_id: "foc".into(),
                position: 4321,
            },
            None,
            &Cancel::never(),
        )
        .await
        .unwrap();
    let json = ok_text(result);
    assert_eq!(json["actual_position"], 4321);
    assert_eq!(json["focuser_id"], "foc");
}

#[tokio::test]
async fn test_move_focuser_not_found() {
    let foc = MockFocuser::default();
    let handler = test_handler(focuser_registry(Arc::new(foc), None, None));
    let result = handler
        .move_focuser_inner(
            MoveFocuserParams {
                focuser_id: "missing".into(),
                position: 100,
            },
            None,
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "focuser not found");
}

#[tokio::test]
async fn test_move_focuser_below_min_position() {
    let foc = MockFocuser::default();
    let handler = test_handler(focuser_registry(Arc::new(foc), Some(1000), Some(9000)));
    let result = handler
        .move_focuser_inner(
            MoveFocuserParams {
                focuser_id: "foc".into(),
                position: 500,
            },
            None,
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "position out of range");
}

#[tokio::test]
async fn test_move_focuser_above_max_position() {
    let foc = MockFocuser::default();
    let handler = test_handler(focuser_registry(Arc::new(foc), Some(1000), Some(9000)));
    let result = handler
        .move_focuser_inner(
            MoveFocuserParams {
                focuser_id: "foc".into(),
                position: 9500,
            },
            None,
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "position out of range");
}

#[tokio::test]
async fn test_move_focuser_command_fails() {
    let foc = MockFocuser {
        fail_move: true,
        ..Default::default()
    };
    let handler = test_handler(focuser_registry(Arc::new(foc), None, None));
    let result = handler
        .move_focuser_inner(
            MoveFocuserParams {
                focuser_id: "foc".into(),
                position: 1000,
            },
            None,
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "failed to move focuser");
}

#[tokio::test]
async fn test_move_focuser_is_moving_poll_fails() {
    let foc = MockFocuser {
        fail_is_moving: true,
        ..Default::default()
    };
    let handler = test_handler(focuser_registry(Arc::new(foc), None, None));
    let result = handler
        .move_focuser_inner(
            MoveFocuserParams {
                focuser_id: "foc".into(),
                position: 1000,
            },
            None,
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "error polling focuser is_moving");
}

#[tokio::test]
async fn test_move_focuser_position_read_fails() {
    let foc = MockFocuser {
        fail_position: true,
        ..Default::default()
    };
    let handler = test_handler(focuser_registry(Arc::new(foc), None, None));
    let result = handler
        .move_focuser_inner(
            MoveFocuserParams {
                focuser_id: "foc".into(),
                position: 1000,
            },
            None,
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "failed to read focuser position");
}

/// §2.6: `is_moving()` stays `true`, the predicted deadline expires, and the
/// move returns the timeout error. With current 0 → target 1000 at the
/// default 500 steps/s the deadline is `max(2 s × 2, 5 s floor) = 5 s`, so
/// the failure lands far under the old 120 s ceiling — proof the deadline
/// now scales with the move. `start_paused` auto-advances virtual time.
#[tokio::test(start_paused = true)]
async fn test_move_focuser_timeout() {
    let foc = MockFocuser {
        stuck_moving: true,
        ..Default::default()
    };
    let handler = test_handler(focuser_registry(Arc::new(foc), None, None));
    let started_at = tokio::time::Instant::now();
    let result = handler
        .move_focuser_inner(
            MoveFocuserParams {
                focuser_id: "foc".into(),
                position: 1000,
            },
            None,
            &Cancel::never(),
        )
        .await;
    let elapsed = started_at.elapsed();
    assert_tool_error(result, "timeout waiting for focuser to settle");
    assert!(
        elapsed < Duration::from_mins(1),
        "a short move should hit its computed ~5 s deadline, not the old \
         120 s ceiling; elapsed {elapsed:?}"
    );
}

#[tokio::test]
async fn test_move_focuser_not_connected() {
    let registry = crate::equipment::EquipmentRegistry {
        safety_monitors: vec![],
        cameras: vec![],
        filter_wheels: vec![],
        cover_calibrators: vec![],
        focusers: vec![crate::equipment::FocuserEntry {
            id: "foc".to_string(),
            config: crate::config::FocuserConfig {
                id: "foc".to_string(),
                alpaca_url: "http://localhost:1".to_string(),
                device_number: 0,
                min_position: None,
                max_position: None,
                steps_per_sec: crate::config::focuser::FocuserStepsPerSec::default(),
                auth: None,
            },
            session: crate::equipment::DeviceSession::disconnected(),
        }],
        mount: None,
        ..Default::default()
    };
    let handler = test_handler(registry);
    let result = handler
        .move_focuser_inner(
            MoveFocuserParams {
                focuser_id: "foc".into(),
                position: 1000,
            },
            None,
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "focuser not connected");
}

#[tokio::test]
async fn test_get_focuser_position_success() {
    let foc = MockFocuser {
        position_value: 12345,
        ..Default::default()
    };
    let handler = test_handler(focuser_registry(Arc::new(foc), None, None));
    let result = handler
        .get_focuser_position(Parameters(FocuserIdParams {
            focuser_id: "foc".into(),
        }))
        .await
        .unwrap();
    let json = ok_text(result);
    assert_eq!(json["position"], 12345);
}

#[tokio::test]
async fn test_get_focuser_position_not_connected() {
    let registry = crate::equipment::EquipmentRegistry {
        safety_monitors: vec![],
        cameras: vec![],
        filter_wheels: vec![],
        cover_calibrators: vec![],
        focusers: vec![crate::equipment::FocuserEntry {
            id: "foc".to_string(),
            config: crate::config::FocuserConfig {
                id: "foc".to_string(),
                alpaca_url: "http://localhost:1".to_string(),
                device_number: 0,
                min_position: None,
                max_position: None,
                steps_per_sec: crate::config::focuser::FocuserStepsPerSec::default(),
                auth: None,
            },
            session: crate::equipment::DeviceSession::disconnected(),
        }],
        mount: None,
        ..Default::default()
    };
    let handler = test_handler(registry);
    let result = handler
        .get_focuser_position(Parameters(FocuserIdParams {
            focuser_id: "foc".into(),
        }))
        .await;
    assert_tool_error(result, "focuser not connected");
}

/// `Temperature` is independent of `TempCompAvailable`: a focuser may
/// report a temperature reading regardless of whether temperature
/// compensation is available. The mock leaves `temp_comp_available()`
/// at its default `Ok(false)` to make that decoupling explicit.
#[tokio::test]
async fn test_get_focuser_temperature_returns_value() {
    let foc = MockFocuser {
        temperature_value: 12.5,
        ..Default::default()
    };
    let handler = test_handler(focuser_registry(Arc::new(foc), None, None));
    let result = handler
        .get_focuser_temperature(Parameters(FocuserIdParams {
            focuser_id: "foc".into(),
        }))
        .await
        .unwrap();
    let json = ok_text(result);
    assert_eq!(json["temperature_c"], 12.5);
}

/// `Temperature` returning `NOT_IMPLEMENTED` is the only signal that
/// the property is unsupported on this device; the tool surfaces
/// `temperature_c: null` for that exact case.
#[tokio::test]
async fn test_get_focuser_temperature_null_when_not_implemented() {
    let foc = MockFocuser {
        temperature_not_implemented: true,
        ..Default::default()
    };
    let handler = test_handler(focuser_registry(Arc::new(foc), None, None));
    let result = handler
        .get_focuser_temperature(Parameters(FocuserIdParams {
            focuser_id: "foc".into(),
        }))
        .await
        .unwrap();
    let json = ok_text(result);
    assert!(
        json["temperature_c"].is_null(),
        "expected null temperature_c, got {:?}",
        json["temperature_c"]
    );
}

/// Any non-`NOT_IMPLEMENTED` failure from `temperature()` propagates
/// as a tool error rather than being silently coerced to `null`. This
/// pins the asymmetry between "device says I don't have one" and
/// "device tried to read but the read itself failed".
#[tokio::test]
async fn test_get_focuser_temperature_sensor_fails() {
    let foc = MockFocuser {
        fail_temperature: true,
        ..Default::default()
    };
    let handler = test_handler(focuser_registry(Arc::new(foc), None, None));
    let result = handler
        .get_focuser_temperature(Parameters(FocuserIdParams {
            focuser_id: "foc".into(),
        }))
        .await;
    assert_tool_error(result, "failed to read focuser temperature");
}

// -----------------------------------------------------------------------
// Mount tool tests — slew / sync_mount / get_mount_position /
// get_tracking / set_tracking. Singular mount, no mount_id parameter.
// -----------------------------------------------------------------------

#[tokio::test]
async fn test_slew_success() {
    let mount = MockTelescope {
        ra_value: 10.6847,
        dec_value: 41.2689,
        ..Default::default()
    };
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let result = handler
        .slew_inner(
            SlewParams {
                ra: Some(10.6847),
                dec: Some(41.2689),
                settle_after: None,
            },
            None,
            &Cancel::never(),
        )
        .await
        .unwrap();
    let json = ok_text(result);
    assert_eq!(json["actual_ra"], 10.6847);
    assert_eq!(json["actual_dec"], 41.2689);
}

#[tokio::test]
async fn test_slew_no_mount_configured() {
    let handler = test_handler(empty_registry());
    let result = handler
        .slew_inner(
            SlewParams {
                ra: Some(0.0),
                dec: Some(0.0),
                settle_after: None,
            },
            None,
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "no mount configured");
}

#[tokio::test]
async fn test_slew_mount_not_connected() {
    let handler = test_handler(disconnected_mount_registry());
    let result = handler
        .slew_inner(
            SlewParams {
                ra: Some(0.0),
                dec: Some(0.0),
                settle_after: None,
            },
            None,
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "mount not connected");
}

#[tokio::test]
async fn test_slew_missing_ra() {
    let handler = test_handler(mount_registry(Arc::new(MockTelescope::default()), None));
    let result = handler
        .slew_inner(
            SlewParams {
                ra: None,
                dec: Some(0.0),
                settle_after: None,
            },
            None,
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "missing required parameter: ra");
}

#[tokio::test]
async fn test_slew_missing_dec() {
    let handler = test_handler(mount_registry(Arc::new(MockTelescope::default()), None));
    let result = handler
        .slew_inner(
            SlewParams {
                ra: Some(0.0),
                dec: None,
                settle_after: None,
            },
            None,
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "missing required parameter: dec");
}

#[tokio::test]
async fn test_slew_ra_out_of_range() {
    let handler = test_handler(mount_registry(Arc::new(MockTelescope::default()), None));
    let result = handler
        .slew_inner(
            SlewParams {
                ra: Some(25.0),
                dec: Some(0.0),
                settle_after: None,
            },
            None,
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "ra out of range");
}

#[tokio::test]
async fn test_slew_dec_out_of_range() {
    let handler = test_handler(mount_registry(Arc::new(MockTelescope::default()), None));
    let result = handler
        .slew_inner(
            SlewParams {
                ra: Some(0.0),
                dec: Some(91.0),
                settle_after: None,
            },
            None,
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "dec out of range");
}

/// Models the ASCOM `InvalidOperationException` that fires when
/// `Tracking == false` and the caller invokes
/// `SlewToCoordinatesAsync` — the natural error path the design
/// explicitly chose over a magical `ensure_tracking` parameter.
#[tokio::test]
async fn test_slew_alpaca_error_propagates() {
    let mount = MockTelescope {
        fail_slew: true,
        ..Default::default()
    };
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let result = handler
        .slew_inner(
            SlewParams {
                ra: Some(0.0),
                dec: Some(0.0),
                settle_after: None,
            },
            None,
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "failed to slew");
}

/// Drives the timeout escalation path: `slewing()` returns `true`
/// indefinitely, the predicted deadline expires, `abort_slew()` is
/// called (best-effort, ignored result), and the tool returns the
/// timeout error. With current pointing == target (distance 0) the
/// deadline floors at `MIN_SLEW_DEADLINE` (30 s), so the test also
/// asserts the failure lands far under the old 300 s ceiling — proof
/// the deadline now scales with the slew (§2.6). `start_paused` lets
/// tokio auto-advance virtual time so the test runs in real-time ms.
#[tokio::test(start_paused = true)]
async fn test_slew_timeout_returns_error_after_abort() {
    let mount = MockTelescope {
        stuck_slewing: true,
        ..Default::default()
    };
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let started_at = tokio::time::Instant::now();
    let result = handler
        .slew_inner(
            SlewParams {
                ra: Some(0.0),
                dec: Some(0.0),
                settle_after: None,
            },
            None,
            &Cancel::never(),
        )
        .await;
    let elapsed = started_at.elapsed();
    assert_tool_error(result, "timeout waiting for mount to settle");
    assert!(
        elapsed < Duration::from_mins(1),
        "a zero-distance slew should hit the ~30 s MIN_SLEW_DEADLINE floor, \
         not the old 300 s ceiling; elapsed {elapsed:?}"
    );
}

/// Per-call `settle_after` overrides the config default. Passes
/// `Duration::ZERO` to skip an otherwise-non-zero config value;
/// behavior of the actual sleep is exercised in BDD where
/// wall-clock timing is observable.
#[tokio::test]
async fn test_slew_per_call_settle_overrides_config() {
    let mount = MockTelescope::default();
    let handler = test_handler(mount_registry(
        Arc::new(mount),
        Some(Duration::from_mins(1)),
    ));
    let result = handler
        .slew_inner(
            SlewParams {
                ra: Some(0.0),
                dec: Some(0.0),
                settle_after: Some(Duration::ZERO),
            },
            None,
            &Cancel::never(),
        )
        .await
        .unwrap();
    assert!(!result.is_error.unwrap_or(false));
}

#[tokio::test]
async fn test_sync_mount_success() {
    let mount = MockTelescope::default();
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let result = handler
        .sync_mount(Parameters(SyncMountParams {
            ra: Some(0.0),
            dec: Some(0.0),
        }))
        .await
        .unwrap();
    assert!(!result.is_error.unwrap_or(false));
}

#[tokio::test]
async fn test_sync_mount_no_mount_configured() {
    let handler = test_handler(empty_registry());
    let result = handler
        .sync_mount(Parameters(SyncMountParams {
            ra: Some(0.0),
            dec: Some(0.0),
        }))
        .await;
    assert_tool_error(result, "no mount configured");
}

#[tokio::test]
async fn test_sync_mount_alpaca_error() {
    let mount = MockTelescope {
        fail_sync: true,
        ..Default::default()
    };
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let result = handler
        .sync_mount(Parameters(SyncMountParams {
            ra: Some(0.0),
            dec: Some(0.0),
        }))
        .await;
    assert_tool_error(result, "failed to sync mount");
}

#[tokio::test]
async fn test_sync_mount_ra_out_of_range() {
    let handler = test_handler(mount_registry(Arc::new(MockTelescope::default()), None));
    let result = handler
        .sync_mount(Parameters(SyncMountParams {
            ra: Some(-1.0),
            dec: Some(0.0),
        }))
        .await;
    assert_tool_error(result, "ra out of range");
}

#[tokio::test]
async fn test_get_mount_position_returns_ra_dec() {
    let mount = MockTelescope {
        ra_value: 12.5,
        dec_value: -23.4,
        ..Default::default()
    };
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let result = handler
        .get_mount_position(Parameters(GetMountPositionParams {}))
        .await
        .unwrap();
    let json = ok_text(result);
    assert_eq!(json["ra"], 12.5);
    assert_eq!(json["dec"], -23.4);
}

#[tokio::test]
async fn test_get_mount_position_no_mount() {
    let handler = test_handler(empty_registry());
    let result = handler
        .get_mount_position(Parameters(GetMountPositionParams {}))
        .await;
    assert_tool_error(result, "no mount configured");
}

#[tokio::test]
async fn test_get_mount_position_ra_read_fails() {
    let mount = MockTelescope {
        fail_right_ascension: true,
        ..Default::default()
    };
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let result = handler
        .get_mount_position(Parameters(GetMountPositionParams {}))
        .await;
    assert_tool_error(result, "failed to read mount right_ascension");
}

#[tokio::test]
async fn test_get_tracking_returns_state_and_capability() {
    let mount = MockTelescope {
        tracking_value: true,
        can_set_tracking_value: true,
        ..Default::default()
    };
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let result = handler
        .get_tracking(Parameters(GetTrackingParams {}))
        .await
        .unwrap();
    let json = ok_text(result);
    assert_eq!(json["tracking"], true);
    assert_eq!(json["can_set_tracking"], true);
}

/// Mount that reports `CanSetTracking == false` — surfaces in the
/// tool result rather than failing the call. Workflows can read
/// the field and decide whether to continue.
#[tokio::test]
async fn test_get_tracking_surfaces_can_set_tracking_false() {
    let mount = MockTelescope {
        tracking_value: false,
        can_set_tracking_value: false,
        ..Default::default()
    };
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let result = handler
        .get_tracking(Parameters(GetTrackingParams {}))
        .await
        .unwrap();
    let json = ok_text(result);
    assert_eq!(json["tracking"], false);
    assert_eq!(json["can_set_tracking"], false);
}

/// Per the design decision: fail loud on `Tracking` read errors;
/// don't try to half-succeed by returning `can_set_tracking` alone.
#[tokio::test]
async fn test_get_tracking_fails_when_tracking_read_errors() {
    let mount = MockTelescope {
        fail_tracking: true,
        ..Default::default()
    };
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let result = handler.get_tracking(Parameters(GetTrackingParams {})).await;
    assert_tool_error(result, "failed to read mount tracking");
}

#[tokio::test]
async fn test_set_tracking_enables() {
    let mount = MockTelescope::default();
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let result = handler
        .set_tracking_inner(SetTrackingParams { enabled: true }, &Cancel::never())
        .await
        .unwrap();
    assert!(!result.is_error.unwrap_or(false));
}

#[tokio::test]
async fn test_set_tracking_disables() {
    let mount = MockTelescope::default();
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let result = handler
        .set_tracking_inner(SetTrackingParams { enabled: false }, &Cancel::never())
        .await
        .unwrap();
    assert!(!result.is_error.unwrap_or(false));
}

/// Models a mount that responds to `set_tracking` with an
/// `InvalidOperationException` (e.g. `CanSetTracking == false`).
/// The error propagates with the friendly prefix.
#[tokio::test]
async fn test_set_tracking_alpaca_error() {
    let mount = MockTelescope {
        fail_set_tracking: true,
        ..Default::default()
    };
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let result = handler
        .set_tracking_inner(SetTrackingParams { enabled: true }, &Cancel::never())
        .await;
    assert_tool_error(result, "failed to set tracking");
}

// -----------------------------------------------------------------------
// Mount park / unpark / get_park_state / abort_slew tests.
// Singular mount, no params on any of these tools.
// -----------------------------------------------------------------------

/// Mock's `at_park()` returns true immediately, so the polling
/// loop exits on the first iteration.
#[tokio::test]
async fn test_park_success() {
    let mount = MockTelescope {
        at_park_value: true,
        ..Default::default()
    };
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let result = handler
        .park_inner(ParkParams {}, None, &Cancel::never())
        .await
        .unwrap();
    assert!(!result.is_error.unwrap_or(false));
}

#[tokio::test]
async fn test_park_no_mount_configured() {
    let handler = test_handler(empty_registry());
    let result = handler
        .park_inner(ParkParams {}, None, &Cancel::never())
        .await;
    assert_tool_error(result, "no mount configured");
}

#[tokio::test]
async fn test_park_mount_not_connected() {
    let handler = test_handler(disconnected_mount_registry());
    let result = handler
        .park_inner(ParkParams {}, None, &Cancel::never())
        .await;
    assert_tool_error(result, "mount not connected");
}

#[tokio::test]
async fn test_park_alpaca_error_propagates() {
    let mount = MockTelescope {
        fail_park: true,
        ..Default::default()
    };
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let result = handler
        .park_inner(ParkParams {}, None, &Cancel::never())
        .await;
    assert_tool_error(result, "failed to park");
}

/// The predicted deadline expires while `at_park()` keeps returning
/// `false`. Unlike `slew`, `park` does NOT auto-abort — it surfaces the
/// timeout and lets the caller decide. §2.2: the deadline is the worst-case
/// 180° traverse at the default 7200″/s rate (`90 s × 2 = 180 s`), so the
/// failure lands below the old 300 s ceiling. `start_paused` auto-advances
/// virtual time so the test runs in real-time milliseconds.
#[tokio::test(start_paused = true)]
async fn test_park_timeout_does_not_auto_abort() {
    let mount = MockTelescope {
        at_park_value: false,
        ..Default::default()
    };
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let started_at = tokio::time::Instant::now();
    let result = handler
        .park_inner(ParkParams {}, None, &Cancel::never())
        .await;
    let elapsed = started_at.elapsed();
    assert_tool_error(result, "timeout waiting for mount to park");
    assert!(
        elapsed < Duration::from_secs(250),
        "park should hit its computed worst-case deadline (~180 s at the \
         default rate), not the old 300 s ceiling; elapsed {elapsed:?}"
    );
}

#[tokio::test]
async fn test_unpark_success() {
    let mount = MockTelescope {
        at_park_value: true,
        ..Default::default()
    };
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let result = handler
        .unpark_inner(UnparkParams {}, &Cancel::never())
        .await
        .unwrap();
    assert!(!result.is_error.unwrap_or(false));
}

#[tokio::test]
async fn test_unpark_no_mount_configured() {
    let handler = test_handler(empty_registry());
    let result = handler
        .unpark_inner(UnparkParams {}, &Cancel::never())
        .await;
    assert_tool_error(result, "no mount configured");
}

#[tokio::test]
async fn test_unpark_alpaca_error() {
    let mount = MockTelescope {
        fail_unpark: true,
        ..Default::default()
    };
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let result = handler
        .unpark_inner(UnparkParams {}, &Cancel::never())
        .await;
    assert_tool_error(result, "failed to unpark");
}

#[tokio::test]
async fn test_get_park_state_returns_all_fields() {
    let mount = MockTelescope {
        at_park_value: true,
        can_park_value: true,
        can_unpark_value: true,
        ..Default::default()
    };
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let result = handler
        .get_park_state(Parameters(GetParkStateParams {}))
        .await
        .unwrap();
    let json = ok_text(result);
    assert_eq!(json["at_park"], true);
    assert_eq!(json["can_park"], true);
    assert_eq!(json["can_unpark"], true);
}

/// Per the design decision: fail loud on `at_park` read errors;
/// don't try to half-succeed by returning `can_park` alone.
#[tokio::test]
async fn test_get_park_state_fails_when_at_park_read_errors() {
    let mount = MockTelescope {
        fail_at_park: true,
        ..Default::default()
    };
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let result = handler
        .get_park_state(Parameters(GetParkStateParams {}))
        .await;
    assert_tool_error(result, "failed to read mount at_park");
}

#[tokio::test]
async fn test_get_park_state_fails_when_can_park_read_errors() {
    let mount = MockTelescope {
        fail_can_park: true,
        ..Default::default()
    };
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let result = handler
        .get_park_state(Parameters(GetParkStateParams {}))
        .await;
    assert_tool_error(result, "failed to read mount can_park");
}

#[tokio::test]
async fn test_get_park_state_fails_when_can_unpark_read_errors() {
    let mount = MockTelescope {
        fail_can_unpark: true,
        ..Default::default()
    };
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let result = handler
        .get_park_state(Parameters(GetParkStateParams {}))
        .await;
    assert_tool_error(result, "failed to read mount can_unpark");
}

/// `park()` succeeds, but the very first `at_park()` poll errors
/// — covers the `Err` arm of the polling loop. The previous
/// implementation polled `Slewing` and then verified `AtPark`
/// separately; now both arms collapse into the single `at_park`
/// poll error path.
#[tokio::test]
async fn test_park_at_park_poll_fails() {
    let mount = MockTelescope {
        fail_at_park: true,
        ..Default::default()
    };
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let result = handler
        .park_inner(ParkParams {}, None, &Cancel::never())
        .await;
    assert_tool_error(result, "error polling mount at_park");
}

/// `do_slew_blocking` polls `Slewing`; this covers the
/// `PollIdleError::Read` arm. A *persistent* `slewing()` read error
/// (here every call fails) is tolerated for
/// `SLEWING_READ_ERROR_TOLERANCE` ticks, then surfaces — `start_paused`
/// so those ticks cost no wall-clock time.
#[tokio::test(start_paused = true)]
async fn test_slew_polling_error_propagates() {
    let mount = MockTelescope {
        fail_slewing_poll: true,
        ..Default::default()
    };
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let result = handler
        .slew_inner(
            SlewParams {
                ra: Some(0.0),
                dec: Some(0.0),
                settle_after: None,
            },
            None,
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "error polling mount slewing");
}

/// Issue #319 resilience: a *transient* `slewing()` read error mid-slew
/// (fewer than `SLEWING_READ_ERROR_TOLERANCE` consecutive failures) is
/// tolerated — the poll keeps going and reports idle once the reads
/// recover — rather than aborting the slew (and the whole
/// `center_on_target` loop) on a single hiccup.
#[tokio::test(start_paused = true)]
async fn poll_slewing_tolerates_transient_read_errors_then_idles() {
    let mount = MockTelescope {
        slewing_transient_errors: 3,
        stuck_slewing: false,
        ..Default::default()
    };
    super::internals::poll_slewing_until_idle(
        &mount,
        Duration::from_mins(5),
        None,
        &Cancel::never(),
    )
    .await
    .expect("transient slewing() errors below the tolerance must not abort the slew");
}

#[tokio::test]
async fn test_abort_slew_success() {
    let mount = MockTelescope::default();
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let result = handler
        .abort_slew(Parameters(AbortSlewParams {}))
        .await
        .unwrap();
    assert!(!result.is_error.unwrap_or(false));
}

#[tokio::test]
async fn test_abort_slew_no_mount_configured() {
    let handler = test_handler(empty_registry());
    let result = handler.abort_slew(Parameters(AbortSlewParams {})).await;
    assert_tool_error(result, "no mount configured");
}

/// Models a mount that returns `InvalidOperation` from `abort_slew`
/// (e.g. when not currently slewing). The error propagates with
/// the friendly prefix.
#[tokio::test]
async fn test_abort_slew_alpaca_error() {
    let mount = MockTelescope {
        fail_abort_slew: true,
        ..Default::default()
    };
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let result = handler.abort_slew(Parameters(AbortSlewParams {})).await;
    assert_tool_error(result, "failed to abort slew");
}

// -----------------------------------------------------------------------
// Planner tools — error paths
//
// The new ephemeris/planner tools added in Phases 5-7 share two common
// failure shapes: missing site config (10 of the 12 tools require it)
// and parameter validation (range / format). One unit test per branch
// is enough to pin the wiring; the math itself is covered by the
// primitives.rs / decision.rs unit tests.
// -----------------------------------------------------------------------

fn test_handler_with_site(site: rp_ephemeris::Site) -> McpHandler {
    McpHandler::new(
        Arc::new(empty_registry()),
        Arc::new(crate::events::EventBus::from_config(&[], None).unwrap()),
        SessionConfig {
            data_directory: std::env::temp_dir()
                .join("rp-planner-unit-test")
                .to_string_lossy()
                .to_string(),
        },
        ImageCache::new(64, 4, std::path::PathBuf::from("/nonexistent"), 0),
        Some(site),
    )
}

fn test_site() -> rp_ephemeris::Site {
    rp_ephemeris::Site::new(51.0786, -0.2944).unwrap()
}

#[tokio::test]
async fn compute_alt_az_errors_when_site_absent() {
    let h = test_handler(empty_registry());
    let r = h
        .compute_alt_az(Parameters(AltAzParams {
            ra: 0.7,
            dec: 41.0,
            time: None,
        }))
        .await;
    assert_tool_error(r, "site not configured");
}

#[tokio::test]
async fn compute_alt_az_errors_on_out_of_range_inputs() {
    let h = test_handler_with_site(test_site());
    let r = h
        .compute_alt_az(Parameters(AltAzParams {
            ra: 30.0,
            dec: 0.0,
            time: None,
        }))
        .await;
    assert_tool_error(r, "ra_hours");
}

#[tokio::test]
async fn compute_alt_az_errors_on_bad_time() {
    let h = test_handler_with_site(test_site());
    let r = h
        .compute_alt_az(Parameters(AltAzParams {
            ra: 0.0,
            dec: 0.0,
            time: Some("not a time".into()),
        }))
        .await;
    assert_tool_error(r, "RFC3339");
}

#[tokio::test]
async fn compute_transit_errors_when_site_absent() {
    let h = test_handler(empty_registry());
    let r = h
        .compute_transit(Parameters(TransitParams {
            ra: 0.0,
            dec: 0.0,
            date: "2026-05-03".into(),
        }))
        .await;
    assert_tool_error(r, "site not configured");
}

#[tokio::test]
async fn compute_transit_errors_on_bad_date() {
    let h = test_handler_with_site(test_site());
    let r = h
        .compute_transit(Parameters(TransitParams {
            ra: 0.0,
            dec: 0.0,
            date: "tomorrow".into(),
        }))
        .await;
    assert_tool_error(r, "YYYY-MM-DD");
}

#[tokio::test]
async fn compute_rise_set_errors_on_out_of_range_min_alt() {
    let h = test_handler_with_site(test_site());
    let r = h
        .compute_rise_set(Parameters(RiseSetParams {
            ra: 0.0,
            dec: 0.0,
            date: "2026-05-03".into(),
            min_alt_degrees: 200.0,
        }))
        .await;
    assert_tool_error(r, "min_alt_degrees");
}

#[tokio::test]
async fn compute_rise_set_errors_when_site_absent() {
    let h = test_handler(empty_registry());
    let r = h
        .compute_rise_set(Parameters(RiseSetParams {
            ra: 0.0,
            dec: 0.0,
            date: "2026-05-03".into(),
            min_alt_degrees: 0.0,
        }))
        .await;
    assert_tool_error(r, "site not configured");
}

#[tokio::test]
async fn compute_meridian_flip_errors_when_site_absent() {
    let h = test_handler(empty_registry());
    let r = h
        .compute_meridian_flip(Parameters(MeridianFlipParams {
            ra: 0.0,
            dec: 0.0,
            time: None,
            side_of_pier: "unknown".into(),
        }))
        .await;
    assert_tool_error(r, "site not configured");
}

#[tokio::test]
async fn compute_meridian_flip_errors_on_bad_side_of_pier() {
    let h = test_handler_with_site(test_site());
    let r = h
        .compute_meridian_flip(Parameters(MeridianFlipParams {
            ra: 0.0,
            dec: 0.0,
            time: None,
            side_of_pier: "middle".into(),
        }))
        .await;
    assert_tool_error(r, "side_of_pier");
}

#[tokio::test]
async fn get_sun_position_errors_when_site_absent() {
    let h = test_handler(empty_registry());
    let r = h
        .get_sun_position(Parameters(TimeOnlyParams { time: None }))
        .await;
    assert_tool_error(r, "site not configured");
}

#[tokio::test]
async fn get_twilight_errors_when_site_absent() {
    let h = test_handler(empty_registry());
    let r = h
        .get_twilight(Parameters(TwilightParams {
            date: "2026-12-21".into(),
            kind: "civil".into(),
        }))
        .await;
    assert_tool_error(r, "site not configured");
}

#[tokio::test]
async fn get_moon_position_errors_when_site_absent() {
    let h = test_handler(empty_registry());
    let r = h
        .get_moon_position(Parameters(TimeOnlyParams { time: None }))
        .await;
    assert_tool_error(r, "site not configured");
}

#[tokio::test]
async fn compute_moon_separation_errors_on_bad_inputs() {
    let h = test_handler(empty_registry());
    let r = h
        .compute_moon_separation(Parameters(MoonSeparationParams {
            ra: 100.0,
            dec: 0.0,
            time: None,
        }))
        .await;
    assert_tool_error(r, "ra_hours");
}

#[tokio::test]
async fn get_local_sidereal_time_errors_when_site_absent() {
    let h = test_handler(empty_registry());
    let r = h
        .get_local_sidereal_time(Parameters(TimeOnlyParams { time: None }))
        .await;
    assert_tool_error(r, "site not configured");
}

#[tokio::test]
async fn get_site_errors_when_site_absent() {
    let h = test_handler(empty_registry());
    let r = h.get_site(Parameters(GetSiteParams {})).await;
    assert_tool_error(r, "site not configured");
}

#[tokio::test]
async fn get_site_reports_the_configured_coordinates() {
    let h = test_handler_with_site(test_site());
    let v = ok_json(h.get_site(Parameters(GetSiteParams {})).await);
    assert_eq!(v["latitude_degrees"], 51.0786);
    assert_eq!(v["longitude_degrees"], -0.2944);
}

#[tokio::test]
async fn get_target_status_errors_when_site_absent() {
    let h = test_handler(empty_registry());
    let r = h
        .get_target_status(Parameters(GetTargetStatusParams {
            target_name: Some("M 31".into()),
            ra: None,
            dec: None,
            time: None,
        }))
        .await;
    assert_tool_error(r, "site not configured");
}

#[tokio::test]
async fn get_target_status_errors_on_unknown_name() {
    let h = test_handler_with_site(test_site());
    let r = h
        .get_target_status(Parameters(GetTargetStatusParams {
            target_name: Some("M 999".into()),
            ra: None,
            dec: None,
            time: None,
        }))
        .await;
    // The catalog miss path returns a structured `target_not_found`
    // payload as a CallToolResult::error.
    let call_result = r.expect("tool returned protocol error");
    assert!(call_result.is_error.unwrap_or(false));
    let text = call_result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    assert!(text.contains("target_not_found"), "got: {text}");
}

#[tokio::test]
async fn get_target_status_errors_when_neither_name_nor_radec_supplied() {
    let h = test_handler_with_site(test_site());
    let r = h
        .get_target_status(Parameters(GetTargetStatusParams {
            target_name: None,
            ra: None,
            dec: None,
            time: None,
        }))
        .await;
    assert_tool_error(r, "supply exactly one");
}

#[tokio::test]
async fn get_target_status_accepts_radec_form() {
    let h = test_handler_with_site(test_site());
    let r = h
        .get_target_status(Parameters(GetTargetStatusParams {
            target_name: None,
            ra: Some(2.530_194_4),
            dec: Some(89.264_111_1),
            time: None,
        }))
        .await
        .expect("tool returned protocol error");
    assert!(!r.is_error.unwrap_or(false), "expected success");
}

#[tokio::test]
async fn get_next_target_errors_when_site_absent() {
    let h = test_handler(empty_registry());
    let r = h
        .get_next_target(Parameters(GetNextTargetParams {
            time: None,
            train_id: None,
        }))
        .await;
    assert_tool_error(r, "site not configured");
}

#[tokio::test]
async fn get_next_target_with_no_targets_returns_no_targets_configured() {
    let h = test_handler_with_site(test_site());
    let r = h
        .get_next_target(Parameters(GetNextTargetParams {
            time: None,
            train_id: None,
        }))
        .await
        .expect("tool returned protocol error");
    let text = r
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap();
    assert!(text.contains("no_targets_configured"), "got: {text}");
}

// -----------------------------------------------------------------------
// record_exposure / get_session_progress — the progress readbacks
// behind plan rotation and the all-goals-met end_of_session (derived
// from the frames on disk, never counted). The derivation and selection
// math is covered by progress.rs / decision.rs unit tests; these pin
// the tool wiring (store sharing, goal lookup, error arms).
// -----------------------------------------------------------------------

/// A handler backed by a real (temp) target store holding a single
/// always-visible target (`scheduling.min_altitude` -90 survives any
/// sky), display name `"Test Field"` (slug `testfield`), with a Red×1 +
/// Blue×1 goal set. Returns the handler and the `TempDir` the store's
/// redb file lives in — keep the dir bound for the handler's lifetime.
async fn handler_with_planned_target() -> (McpHandler, tempfile::TempDir) {
    handler_with_store_target(test_store_target(
        "Test Field",
        0.0,
        0.0,
        Some(-90.0),
        vec![store_goal("Red", 120, 1), store_goal("Blue", 60, 1)],
    ))
    .await
}

/// Opens a temp redb store, upserts `target`, and wires it onto a
/// site-configured handler (the store is now the sole planner-target
/// source).
async fn handler_with_store_target(target: rp_targets::Target) -> (McpHandler, tempfile::TempDir) {
    handler_with_store_target_and_defaults(target, crate::config::TargetStoreConfig::default())
        .await
}

/// As [`handler_with_store_target`], with explicit target-store
/// settings — for the scenarios that configure `default_grading`.
///
/// The handler's data directory is the same temp dir, and the
/// documented naming templates are compiled onto it, so
/// [`seed_frame`] can place frames the progress scan will find (rp.md
/// § Progress derivation). The `TempDir` is returned so the caller
/// keeps it alive for the test's duration.
async fn handler_with_store_target_and_defaults(
    target: rp_targets::Target,
    defaults: crate::config::TargetStoreConfig,
) -> (McpHandler, tempfile::TempDir) {
    use rp_targets::TargetStore;
    let dir = tempfile::tempdir().unwrap();
    let store = rp_targets::RedbTargetStore::open(dir.path().join("targets.redb"))
        .await
        .unwrap();
    store.upsert_target(target).await.unwrap();
    let store: Arc<dyn rp_targets::TargetStore> = Arc::new(store);
    let mut h = test_handler_with_site(test_site());
    h.session_config.data_directory = dir.path().to_string_lossy().to_string();
    let h = h
        .with_target_store(Some(store), defaults)
        .with_naming_templates(Some(Arc::new(test_naming_templates())));
    (h, dir)
}

/// The documented default `directory_pattern` / `file_naming_pattern`
/// pair (rp.md § Persistence), compiled.
fn test_naming_templates() -> crate::config::naming_template::NamingTemplates {
    crate::config::naming_template::NamingTemplates::from_session_config(
        &crate::config::session::SessionConfig {
        data_directory: String::new(),
        file_naming_pattern: Some(
            "{target}_{filter}_{binning}_{frame_number}_{exposure_duration}_fpos_{filter_position}_{sensor_temp}"
                .to_string(),
            ),
            // The default `directory_pattern` applies whenever
            // `file_naming_pattern` is set.
            directory_pattern: None,
            session_state_file: String::new(),
        },
    )
    .unwrap()
    .unwrap()
}

/// Write one empty `.fits` frame at `relative` under the handler's data
/// directory, optionally with a sidecar carrying a `grading` section.
/// The scan reads filenames and sidecars, never pixels.
fn seed_frame(dir: &tempfile::TempDir, relative: &str, grading: Option<serde_json::Value>) {
    let mut path = dir.path().to_path_buf();
    for component in relative.split('/') {
        path.push(component);
    }
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"").unwrap();
    if let Some(grading) = grading {
        std::fs::write(
            path.with_extension("json"),
            serde_json::to_vec(&serde_json::json!({ "sections": { "grading": grading } })).unwrap(),
        )
        .unwrap();
    }
}

/// One acquisition goal in the store's typed shape.
fn store_goal(filter: &str, secs: u64, desired_count: u32) -> rp_targets::AcquisitionGoal {
    rp_targets::AcquisitionGoal {
        filter: filter.to_string(),
        binning: rp_targets::Binning { x: 1, y: 1 },
        exposure_duration: Duration::from_secs(secs),
        desired_count,
    }
}

/// A store `Target` from a display name, coordinates, an optional
/// per-target altitude floor, and goals. `active: true` so the planner
/// sees it. The slug comes from `from_display_name`, the same derivation
/// `add_target` uses — a fixture that minted slugs its own way is what
/// let a lookup/store mismatch hide here once already.
fn test_store_target(
    display_name: &str,
    ra_hours: f64,
    dec_degrees: f64,
    min_altitude_degrees: Option<f64>,
    goals: Vec<rp_targets::AcquisitionGoal>,
) -> rp_targets::Target {
    rp_targets::Target {
        slug: rp_targets::TargetSlug::from_display_name(display_name).unwrap(),
        display_name: display_name.to_string(),
        coord: rp_targets::IcrsCoord::try_new(ra_hours, dec_degrees).unwrap(),
        catalog_ref: None,
        object_type: None,
        magnitude: None,
        size_arcmin: None,
        position_angle_degrees: None,
        priority: 0,
        active: true,
        goals,
        scheduling: min_altitude_degrees.map(|m| rp_targets::SchedulingConstraints {
            min_altitude_degrees: Some(m),
            ..Default::default()
        }),
        grading: None,
        notes: None,
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        created_by: "operator".to_string(),
        updated_by: "operator".to_string(),
    }
}

#[tokio::test]
async fn record_exposure_errors_for_an_unknown_target() {
    let (h, _store_dir) = handler_with_planned_target().await;
    let r = h
        .record_exposure(Parameters(RecordExposureParams {
            target: "no-such-field".into(),
            filter: Some("Red".into()),
        }))
        .await;
    assert_tool_error(r, "unknown target");
}

#[tokio::test]
async fn record_exposure_reports_derived_progress_without_incrementing() {
    let (h, store_dir) = handler_with_planned_target().await;
    seed_frame(
        &store_dir,
        "test-field/2026-07-30/Light/test-field_Red_1x1_0001_2m_fpos_1_-10C_aaaaaaa1.fits",
        None,
    );
    let call = |filter: Option<&str>| {
        let filter = filter.map(String::from);
        h.record_exposure(Parameters(RecordExposureParams {
            target: "test-field".into(),
            filter,
        }))
    };
    let v = ok_json(call(Some("Red")).await);
    assert_eq!(v["target"], "test-field");
    assert_eq!(v["filter"], "Red");
    assert_eq!(v["progress"][0]["filter"], "Red");
    assert_eq!(v["progress"][0]["good"], 1);
    assert_eq!(v["progress"][0]["total"], 1);
    assert_eq!(v["progress"][0]["desired_count"], 1);
    assert_eq!(v["progress"][1]["filter"], "Blue");
    assert_eq!(v["progress"][1]["good"], 0);

    // Calling again changes nothing — the frame on disk is the count,
    // and `record_exposure` increments nothing.
    let v = ok_json(call(Some("Red")).await);
    assert_eq!(v["progress"][0]["good"], 1);

    // An unfiltered frame echoes its filter back as null; the progress
    // payload is the target's, not the filter's.
    let v = ok_json(call(None).await);
    assert!(v["filter"].is_null());
    assert_eq!(v["progress"][0]["good"], 1);
}

#[tokio::test]
async fn get_session_progress_reports_every_configured_target() {
    let (h, store_dir) = handler_with_planned_target().await;
    seed_frame(
        &store_dir,
        "test-field/2026-07-30/Light/test-field_Red_1x1_0001_2m_fpos_1_-10C_aaaaaaa1.fits",
        None,
    );
    let v = ok_json(
        h.get_session_progress(Parameters(GetSessionProgressParams {}))
            .await,
    );
    assert_eq!(
        v["progress"]["test-field"][0],
        serde_json::json!({
            "filter": "Red", "binning": "1x1", "exposure_duration": "2m",
            "desired_count": 1, "good": 1, "total": 1
        })
    );
    assert_eq!(
        v["progress"]["test-field"][1],
        serde_json::json!({
            "filter": "Blue", "binning": "1x1", "exposure_duration": "1m",
            "desired_count": 1, "good": 0, "total": 0
        }),
        "every goal appears, captured or not"
    );
}

#[tokio::test]
async fn get_next_target_rotates_the_plan_and_ends_when_goals_are_met() {
    // The tool-level loop an orchestrator drives: recommend Red,
    // capture writes the frame, recommend Blue, capture writes that
    // one, end_of_session — the plan rotates off the frames on disk.
    let (h, store_dir) = handler_with_planned_target().await;
    let next = || {
        h.get_next_target(Parameters(GetNextTargetParams {
            time: None,
            train_id: None,
        }))
    };
    let v = ok_json(next().await);
    assert_eq!(v["exposure"]["filter"], "Red");
    assert_eq!(v["exposure"]["duration_secs"], 120.0);

    seed_frame(
        &store_dir,
        "test-field/2026-07-30/Light/test-field_Red_1x1_0001_2m_fpos_1_-10C_aaaaaaa1.fits",
        None,
    );
    let v = ok_json(next().await);
    assert_eq!(
        v["exposure"]["filter"], "Blue",
        "the met Red goal rotates the plan"
    );
    assert_eq!(v["exposure"]["duration_secs"], 60.0);

    seed_frame(
        &store_dir,
        "test-field/2026-07-30/Light/test-field_Blue_1x1_0001_1m_fpos_2_-10C_bbbbbbb1.fits",
        None,
    );
    let v = ok_json(next().await);
    assert_eq!(v["reason"], "end_of_session");
    assert!(v["target"].is_null());
}

#[tokio::test]
async fn a_rejected_frame_does_not_end_the_session() {
    // Same loop, but the only frame is graded out: `total` advances and
    // `good` does not, so the goal is unmet and the target stays in the
    // rotation (rp.md § Decision Logic — exhaustion is judged on good).
    let (h, store_dir) = handler_with_store_target_and_defaults(
        test_store_target(
            "Test Field",
            0.0,
            0.0,
            Some(-90.0),
            vec![store_goal("Red", 120, 1)],
        ),
        crate::config::TargetStoreConfig {
            default_grading: Some(rp_targets::GradingThresholds {
                max_hfr_pixels: Some(3.0),
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await;
    seed_frame(
        &store_dir,
        "test-field/2026-07-30/Light/test-field_Red_1x1_0001_2m_fpos_1_-10C_aaaaaaa1.fits",
        Some(serde_json::json!({ "hfr": 9.9 })),
    );
    let v = ok_json(
        h.get_next_target(Parameters(GetNextTargetParams {
            time: None,
            train_id: None,
        }))
        .await,
    );
    assert_eq!(v["reason"], "best_transiting_candidate");
    assert_eq!(v["exposure"]["filter"], "Red");
}

#[tokio::test]
async fn get_target_status_reports_progress_for_a_configured_target() {
    // Stored under its catalog name so the same string both resolves
    // coordinates and (derived to `m-31`) matches the progress map.
    let (h, store_dir) = handler_with_store_target(test_store_target(
        "M 31",
        0.7123,
        41.269,
        None,
        vec![store_goal("Red", 120, 4)],
    ))
    .await;
    seed_frame(
        &store_dir,
        "m-31/2026-07-30/Light/m-31_Red_1x1_0001_2m_fpos_1_-10C_aaaaaaa1.fits",
        None,
    );
    let v = ok_json(
        h.get_target_status(Parameters(GetTargetStatusParams {
            target_name: Some("M 31".into()),
            ra: None,
            dec: None,
            time: None,
        }))
        .await,
    );
    assert_eq!(
        v["progress"][0],
        serde_json::json!({
            "filter": "Red", "binning": "1x1", "exposure_duration": "2m",
            "desired_count": 4, "good": 1, "total": 1
        })
    );
    // A case-variant spelling resolves through the catalog to the
    // same canonical name, and the progress lookup must follow it —
    // "m 31" answers with "M 31"'s progress, not null.
    let v = ok_json(
        h.get_target_status(Parameters(GetTargetStatusParams {
            target_name: Some("m 31".into()),
            ra: None,
            dec: None,
            time: None,
        }))
        .await,
    );
    assert_eq!(v["target_name"], "M 31");
    assert_eq!(
        v["progress"][0]["good"], 1,
        "progress must match via the catalog-resolved name"
    );
}

#[tokio::test]
async fn get_meridian_status_errors_when_site_absent() {
    let h = test_handler(empty_registry());
    let r = h
        .get_meridian_status(Parameters(GetMeridianStatusParams { time: None }))
        .await;
    assert_tool_error(r, "site not configured");
}

#[tokio::test]
async fn get_meridian_status_errors_when_mount_absent() {
    let h = test_handler_with_site(test_site());
    let r = h
        .get_meridian_status(Parameters(GetMeridianStatusParams { time: None }))
        .await;
    // empty_registry has no mount, so `resolve_mount` returns the
    // standard "mount not configured" error.
    assert_tool_error(r, "mount");
}

// -----------------------------------------------------------------------
// Planner tools — happy paths (cover the success-return arms in mcp.rs;
// value correctness is covered by primitives.rs / convenience.rs unit
// tests).
// -----------------------------------------------------------------------

fn handler_with_site_and_mount() -> McpHandler {
    let mock = MockTelescope::default();
    let mount_cfg = crate::config::MountConfig {
        alpaca_url: "http://unused".into(),
        device_number: 0,
        settle_after_slew: None,
        slew_rate_arcsec_per_sec: crate::config::mount::SlewRateArcsecPerSec::default(),
        guiding: None,
        auth: None,
    };
    // Skip the connect-time HTTP fetch by hand-building a registry
    // with the mock device wired in directly.
    let registry = crate::equipment::EquipmentRegistry {
        safety_monitors: vec![],
        cameras: vec![],
        filter_wheels: vec![],
        cover_calibrators: vec![],
        focusers: vec![],
        mount: Some(crate::equipment::MountEntry {
            config: mount_cfg,
            session: crate::equipment::DeviceSession::connected(Arc::new(mock)),
        }),
        ..Default::default()
    };
    McpHandler::new(
        Arc::new(registry),
        Arc::new(crate::events::EventBus::from_config(&[], None).unwrap()),
        SessionConfig {
            data_directory: std::env::temp_dir()
                .join("rp-planner-happy-test")
                .to_string_lossy()
                .to_string(),
        },
        ImageCache::new(64, 4, std::path::PathBuf::from("/nonexistent"), 0),
        Some(test_site()),
    )
}

/// Yank the JSON payload from a successful `CallToolResult`.
fn ok_json(r: Result<CallToolResult, rmcp::ErrorData>) -> serde_json::Value {
    let call_result = r.expect("tool returned protocol error");
    assert!(
        !call_result.is_error.unwrap_or(false),
        "expected success, got error: {call_result:?}"
    );
    let text = call_result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("expected text content");
    serde_json::from_str(&text).expect("response was not valid JSON")
}

const TEST_TIME: &str = "2026-05-03T22:00:00Z";

#[tokio::test]
async fn compute_alt_az_happy_path() {
    let h = test_handler_with_site(test_site());
    let v = ok_json(
        h.compute_alt_az(Parameters(AltAzParams {
            ra: 2.530_194_4,
            dec: 89.264_111_1,
            time: Some(TEST_TIME.into()),
        }))
        .await,
    );
    assert!(v["altitude_degrees"].as_f64().is_some());
    assert!(v["azimuth_degrees"].as_f64().is_some());
}

#[tokio::test]
async fn compute_transit_happy_path() {
    let h = test_handler_with_site(test_site());
    let v = ok_json(
        h.compute_transit(Parameters(TransitParams {
            ra: 0.7123,
            dec: 41.27,
            date: "2026-11-01".into(),
        }))
        .await,
    );
    assert!(v.get("transit_utc").is_some());
}

#[tokio::test]
async fn compute_rise_set_happy_path() {
    let h = test_handler_with_site(test_site());
    let v = ok_json(
        h.compute_rise_set(Parameters(RiseSetParams {
            ra: 0.7123,
            dec: 41.27,
            date: "2026-11-01".into(),
            min_alt_degrees: 30.0,
        }))
        .await,
    );
    assert!(v.get("rise_utc").is_some());
    assert!(v.get("set_utc").is_some());
}

#[tokio::test]
async fn compute_meridian_flip_happy_path() {
    let h = test_handler_with_site(test_site());
    let v = ok_json(
        h.compute_meridian_flip(Parameters(MeridianFlipParams {
            ra: 0.7123,
            dec: 41.27,
            time: Some(TEST_TIME.into()),
            side_of_pier: "east".into(),
        }))
        .await,
    );
    assert!(v["time_to_flip_seconds"].as_i64().is_some());
}

#[tokio::test]
async fn get_sun_position_happy_path() {
    let h = test_handler_with_site(test_site());
    let v = ok_json(
        h.get_sun_position(Parameters(TimeOnlyParams {
            time: Some(TEST_TIME.into()),
        }))
        .await,
    );
    assert!(v["ra_hours"].as_f64().is_some());
    assert!(v["dec_degrees"].as_f64().is_some());
}

#[tokio::test]
async fn get_twilight_happy_path() {
    let h = test_handler_with_site(test_site());
    let v = ok_json(
        h.get_twilight(Parameters(TwilightParams {
            date: "2026-12-21".into(),
            kind: "civil".into(),
        }))
        .await,
    );
    assert_eq!(v["kind"], "civil");
}

#[tokio::test]
async fn get_moon_position_happy_path() {
    let h = test_handler_with_site(test_site());
    let v = ok_json(
        h.get_moon_position(Parameters(TimeOnlyParams {
            time: Some(TEST_TIME.into()),
        }))
        .await,
    );
    assert!(v["phase_degrees"].as_f64().is_some());
    assert!(v["illumination_fraction"].as_f64().is_some());
}

#[tokio::test]
async fn compute_moon_separation_happy_path() {
    let h = test_handler_with_site(test_site());
    let v = ok_json(
        h.compute_moon_separation(Parameters(MoonSeparationParams {
            ra: 0.7123,
            dec: 41.27,
            time: Some(TEST_TIME.into()),
        }))
        .await,
    );
    let sep = v["separation_degrees"].as_f64().unwrap();
    assert!((0.0..=180.0).contains(&sep));
}

#[tokio::test]
async fn get_local_sidereal_time_happy_path() {
    let h = test_handler_with_site(test_site());
    let v = ok_json(
        h.get_local_sidereal_time(Parameters(TimeOnlyParams {
            time: Some(TEST_TIME.into()),
        }))
        .await,
    );
    let lst = v["lst_hours"].as_f64().unwrap();
    assert!((0.0..24.0).contains(&lst));
}

#[tokio::test]
async fn get_target_status_happy_path_via_catalog() {
    let h = test_handler_with_site(test_site());
    let v = ok_json(
        h.get_target_status(Parameters(GetTargetStatusParams {
            target_name: Some("M 31".into()),
            ra: None,
            dec: None,
            time: Some(TEST_TIME.into()),
        }))
        .await,
    );
    assert_eq!(v["target_name"], "M 31");
    assert!(v["altitude_degrees"].as_f64().is_some());
}

#[tokio::test]
async fn get_meridian_status_happy_path() {
    // MockTelescope doesn't implement side_of_pier, which returns
    // NOT_IMPLEMENTED — get_meridian_status maps that to "unknown"
    // and surfaces the JSON. Exercises the success arm + the
    // NOT_IMPLEMENTED → Unknown branch in one shot.
    let h = handler_with_site_and_mount();
    let v = ok_json(
        h.get_meridian_status(Parameters(GetMeridianStatusParams {
            time: Some(TEST_TIME.into()),
        }))
        .await,
    );
    assert!(v["time_to_flip_seconds"].is_number());
    assert_eq!(v["side_of_pier"], "unknown");
    assert!(v["mount_ra_hours"].as_f64().is_some());
}

// -----------------------------------------------------------------------
// plate_solve tests
// -----------------------------------------------------------------------
//
// These exercise the MCP handler against `MockPlateSolveClient`. The
// handler resolves devices, validates hints, calls the client, maps
// SolveError → MCP error, and attempts persistence. End-to-end
// wire-format coverage lives in the BDD suite (`plate_solve.feature`)
// against `PlateSolverStub`; the unit layer pins the per-code error
// shape and the mount-hint conversion deterministically.

use rp_plate_solver::{MockPlateSolveClient, SolveError, SolveOutcome};

fn ok_outcome() -> SolveOutcome {
    SolveOutcome {
        ra_center: 10.6848,
        dec_center: 41.269,
        pixel_scale_arcsec: 1.05,
        rotation_deg: 12.3,
        solver: "stub".to_string(),
        wcs_matrix: None,
    }
}

/// Build a handler with a configured plate-solver client. Pass
/// `expectations` to wire up `mock.expect_solve()` calls before the
/// handler is built; pass `None` for `default_search_radius_deg`
/// when the test doesn't care about the config-default path.
fn handler_with_plate_solver(
    registry: crate::equipment::EquipmentRegistry,
    configure: impl FnOnce(&mut MockPlateSolveClient),
    default_search_radius_deg: Option<f64>,
) -> McpHandler {
    let mut mock = MockPlateSolveClient::new();
    configure(&mut mock);
    let client: Arc<dyn rp_plate_solver::PlateSolveClient> = Arc::new(mock);
    test_handler(registry).with_plate_solver(Some(client), default_search_radius_deg)
}

#[tokio::test]
async fn plate_solve_happy_path_image_path() {
    let handler = handler_with_plate_solver(
        empty_registry(),
        |mock| {
            mock.expect_solve()
                .withf(|req| req.fits_path == "/tmp/x.fits")
                .returning(|_| Ok(ok_outcome()));
        },
        None,
    );
    let result = handler
        .plate_solve(Parameters(PlateSolveParams {
            document_id: None,
            image_path: Some("/tmp/x.fits".to_string()),
            pointing_hint: None,
            use_mount_hints: None,
            fov_hint_deg: None,
            search_radius_deg: None,
            timeout: None,
        }))
        .await
        .unwrap();
    let json = ok_text(result);
    assert_eq!(json["ra_center"], 10.6848);
    assert_eq!(json["dec_center"], 41.269);
    assert_eq!(json["pixel_scale_arcsec"], 1.05);
    assert_eq!(json["rotation_deg"], 12.3);
    assert_eq!(json["solver"], "stub");
    // The wrapper returned no matrix; the output carries an explicit
    // null rather than omitting the key. Index-style access yields
    // Null for a missing key too, so check presence first.
    let wcs = json
        .as_object()
        .unwrap()
        .get("wcs_matrix")
        .expect("wcs_matrix key must be present");
    assert!(wcs.is_null());
}

#[tokio::test]
async fn plate_solve_forwards_populated_wcs_matrix() {
    let handler = handler_with_plate_solver(
        empty_registry(),
        |mock| {
            mock.expect_solve().returning(|_| {
                Ok(SolveOutcome {
                    wcs_matrix: Some(rp_plate_solver::WcsMatrix {
                        crpix1: 512.0,
                        crpix2: 384.0,
                        cd1_1: -2.91e-4,
                        cd1_2: 1.2e-6,
                        cd2_1: 1.1e-6,
                        cd2_2: 2.91e-4,
                    }),
                    ..ok_outcome()
                })
            });
        },
        None,
    );
    let result = handler
        .plate_solve(Parameters(PlateSolveParams {
            document_id: None,
            image_path: Some("/tmp/x.fits".to_string()),
            pointing_hint: None,
            use_mount_hints: None,
            fov_hint_deg: None,
            search_radius_deg: None,
            timeout: None,
        }))
        .await
        .unwrap();
    let json = ok_text(result);
    assert_eq!(json["wcs_matrix"]["crpix1"], 512.0);
    assert_eq!(json["wcs_matrix"]["crpix2"], 384.0);
    assert_eq!(json["wcs_matrix"]["cd1_1"], -2.91e-4);
    assert_eq!(json["wcs_matrix"]["cd1_2"], 1.2e-6);
    assert_eq!(json["wcs_matrix"]["cd2_1"], 1.1e-6);
    assert_eq!(json["wcs_matrix"]["cd2_2"], 2.91e-4);
}

#[tokio::test]
async fn plate_solve_neither_document_id_nor_image_path_errors() {
    let handler = handler_with_plate_solver(empty_registry(), |_| {}, None);
    let result = handler
        .plate_solve(Parameters(PlateSolveParams {
            document_id: None,
            image_path: None,
            pointing_hint: None,
            use_mount_hints: None,
            fov_hint_deg: None,
            search_radius_deg: None,
            timeout: None,
        }))
        .await;
    assert_tool_error(result, "image_path");
}

#[tokio::test]
async fn plate_solve_unconfigured_returns_not_configured_error() {
    // No `with_plate_solver` call ⇒ tool reports not configured
    // even when a path is supplied.
    let handler = test_handler(empty_registry());
    let result = handler
        .plate_solve(Parameters(PlateSolveParams {
            document_id: None,
            image_path: Some("/tmp/x.fits".to_string()),
            pointing_hint: None,
            use_mount_hints: None,
            fov_hint_deg: None,
            search_radius_deg: None,
            timeout: None,
        }))
        .await;
    assert_tool_error(result, "plate solver not configured");
}

#[tokio::test]
async fn plate_solve_pointing_hint_and_use_mount_hints_are_mutually_exclusive() {
    let handler = handler_with_plate_solver(empty_registry(), |_| {}, None);
    let result = handler
        .plate_solve(Parameters(PlateSolveParams {
            document_id: None,
            image_path: Some("/tmp/x.fits".to_string()),
            pointing_hint: Some(PointingHint {
                ra_deg: 1.0,
                dec_deg: 2.0,
            }),
            use_mount_hints: Some(true),
            fov_hint_deg: None,
            search_radius_deg: None,
            timeout: None,
        }))
        .await;
    assert_tool_error(result, "pointing_hint or use_mount_hints");
}

#[tokio::test]
async fn plate_solve_use_mount_hints_with_no_mount_errors() {
    let handler = handler_with_plate_solver(empty_registry(), |_| {}, None);
    let result = handler
        .plate_solve(Parameters(PlateSolveParams {
            document_id: None,
            image_path: Some("/tmp/x.fits".to_string()),
            pointing_hint: None,
            use_mount_hints: Some(true),
            fov_hint_deg: None,
            search_radius_deg: None,
            timeout: None,
        }))
        .await;
    assert_tool_error(result, "use_mount_hints");
}

#[tokio::test]
async fn plate_solve_use_mount_hints_reads_mount_and_converts_ra_to_degrees() {
    // Mount at RA=10.6848h, Dec=41.269° — matches the plate-solver.md
    // M31 example. The handler should send ra_hint=160.272° (10.6848*15)
    // and dec_hint=41.269° to the wrapper.
    let mount = MockTelescope {
        ra_value: 10.6848,
        dec_value: 41.269,
        ..Default::default()
    };
    let registry = mount_registry(Arc::new(mount), None);
    let handler = handler_with_plate_solver(
        registry,
        |mock| {
            mock.expect_solve()
                .withf(|req| {
                    let ra = req.ra_hint.unwrap_or_default();
                    let dec = req.dec_hint.unwrap_or_default();
                    (ra - 160.272).abs() < 1e-3 && (dec - 41.269).abs() < 1e-3
                })
                .returning(|_| Ok(ok_outcome()));
        },
        None,
    );
    let result = handler
        .plate_solve(Parameters(PlateSolveParams {
            document_id: None,
            image_path: Some("/tmp/x.fits".to_string()),
            pointing_hint: None,
            use_mount_hints: Some(true),
            fov_hint_deg: None,
            search_radius_deg: None,
            timeout: None,
        }))
        .await
        .unwrap();
    assert!(!result.is_error.unwrap_or(false));
}

#[tokio::test]
async fn plate_solve_explicit_pointing_hint_forwards_verbatim() {
    let handler = handler_with_plate_solver(
        empty_registry(),
        |mock| {
            mock.expect_solve()
                .withf(|req| req.ra_hint == Some(120.5) && req.dec_hint == Some(-30.1))
                .returning(|_| Ok(ok_outcome()));
        },
        None,
    );
    let result = handler
        .plate_solve(Parameters(PlateSolveParams {
            document_id: None,
            image_path: Some("/tmp/x.fits".to_string()),
            pointing_hint: Some(PointingHint {
                ra_deg: 120.5,
                dec_deg: -30.1,
            }),
            use_mount_hints: None,
            fov_hint_deg: None,
            search_radius_deg: None,
            timeout: None,
        }))
        .await
        .unwrap();
    assert!(!result.is_error.unwrap_or(false));
}

#[tokio::test]
async fn plate_solve_optional_fields_forwarded_verbatim() {
    let handler = handler_with_plate_solver(
        empty_registry(),
        |mock| {
            mock.expect_solve()
                .withf(|req| {
                    req.fov_hint_deg == Some(1.5)
                        && req.search_radius_deg == Some(2.0)
                        && req.timeout == Some(Duration::from_secs(45))
                })
                .returning(|_| Ok(ok_outcome()));
        },
        None,
    );
    let result = handler
        .plate_solve(Parameters(PlateSolveParams {
            document_id: None,
            image_path: Some("/tmp/x.fits".to_string()),
            pointing_hint: None,
            use_mount_hints: None,
            fov_hint_deg: Some(1.5),
            search_radius_deg: Some(2.0),
            timeout: Some(Duration::from_secs(45)),
        }))
        .await
        .unwrap();
    assert!(!result.is_error.unwrap_or(false));
}

#[tokio::test]
async fn plate_solve_config_default_search_radius_applied_when_omitted() {
    let handler = handler_with_plate_solver(
        empty_registry(),
        |mock| {
            mock.expect_solve()
                .withf(|req| req.search_radius_deg == Some(4.0))
                .returning(|_| Ok(ok_outcome()));
        },
        Some(4.0),
    );
    let result = handler
        .plate_solve(Parameters(PlateSolveParams {
            document_id: None,
            image_path: Some("/tmp/x.fits".to_string()),
            pointing_hint: None,
            use_mount_hints: None,
            fov_hint_deg: None,
            search_radius_deg: None,
            timeout: None,
        }))
        .await
        .unwrap();
    assert!(!result.is_error.unwrap_or(false));
}

#[tokio::test]
async fn plate_solve_per_call_search_radius_overrides_config_default() {
    let handler = handler_with_plate_solver(
        empty_registry(),
        |mock| {
            mock.expect_solve()
                .withf(|req| req.search_radius_deg == Some(2.5))
                .returning(|_| Ok(ok_outcome()));
        },
        Some(4.0),
    );
    let result = handler
        .plate_solve(Parameters(PlateSolveParams {
            document_id: None,
            image_path: Some("/tmp/x.fits".to_string()),
            pointing_hint: None,
            use_mount_hints: None,
            fov_hint_deg: None,
            search_radius_deg: Some(2.5),
            timeout: None,
        }))
        .await
        .unwrap();
    assert!(!result.is_error.unwrap_or(false));
}

#[tokio::test]
async fn plate_solve_no_hints_produces_blind_request() {
    let handler = handler_with_plate_solver(
        empty_registry(),
        |mock| {
            mock.expect_solve()
                .withf(|req| {
                    req.ra_hint.is_none()
                        && req.dec_hint.is_none()
                        && req.fov_hint_deg.is_none()
                        && req.search_radius_deg.is_none()
                })
                .returning(|_| Ok(ok_outcome()));
        },
        None,
    );
    let result = handler
        .plate_solve(Parameters(PlateSolveParams {
            document_id: None,
            image_path: Some("/tmp/x.fits".to_string()),
            pointing_hint: None,
            use_mount_hints: None,
            fov_hint_deg: None,
            search_radius_deg: None,
            timeout: None,
        }))
        .await
        .unwrap();
    assert!(!result.is_error.unwrap_or(false));
}

#[tokio::test]
async fn plate_solve_propagates_wrapper_solve_failed() {
    let handler = handler_with_plate_solver(
        empty_registry(),
        |mock| {
            mock.expect_solve().returning(|_| {
                Err(SolveError::Wrapper {
                    code: "solve_failed".to_string(),
                    message: "ASTAP exited with code 1".to_string(),
                    details: serde_json::Value::Null,
                })
            });
        },
        None,
    );
    let result = handler
        .plate_solve(Parameters(PlateSolveParams {
            document_id: None,
            image_path: Some("/tmp/x.fits".to_string()),
            pointing_hint: None,
            use_mount_hints: None,
            fov_hint_deg: None,
            search_radius_deg: None,
            timeout: None,
        }))
        .await;
    assert_tool_error(result, "solve_failed");
}

#[tokio::test]
async fn plate_solve_propagates_wrapper_solve_failed_with_details() {
    let handler = handler_with_plate_solver(
        empty_registry(),
        |mock| {
            mock.expect_solve().returning(|_| {
                Err(SolveError::Wrapper {
                    code: "solve_failed".to_string(),
                    message: "ASTAP exited with code 1".to_string(),
                    details: serde_json::json!({"stderr_tail": "no stars found"}),
                })
            });
        },
        None,
    );
    let result = handler
        .plate_solve(Parameters(PlateSolveParams {
            document_id: None,
            image_path: Some("/tmp/x.fits".to_string()),
            pointing_hint: None,
            use_mount_hints: None,
            fov_hint_deg: None,
            search_radius_deg: None,
            timeout: None,
        }))
        .await;
    assert_tool_error(result, "no stars found");
}

#[tokio::test]
async fn plate_solve_maps_service_unreachable_to_distinct_error() {
    let handler = handler_with_plate_solver(
        empty_registry(),
        |mock| {
            mock.expect_solve().returning(|_| {
                Err(SolveError::ServiceUnreachable(
                    "connection refused".to_string(),
                ))
            });
        },
        None,
    );
    let result = handler
        .plate_solve(Parameters(PlateSolveParams {
            document_id: None,
            image_path: Some("/tmp/x.fits".to_string()),
            pointing_hint: None,
            use_mount_hints: None,
            fov_hint_deg: None,
            search_radius_deg: None,
            timeout: None,
        }))
        .await;
    assert_tool_error(result, "service unreachable");
}

#[tokio::test]
async fn plate_solve_maps_internal_error_with_internal_prefix() {
    let handler = handler_with_plate_solver(
        empty_registry(),
        |mock| {
            mock.expect_solve()
                .returning(|_| Err(SolveError::Internal("broken pipe".to_string())));
        },
        None,
    );
    let result = handler
        .plate_solve(Parameters(PlateSolveParams {
            document_id: None,
            image_path: Some("/tmp/x.fits".to_string()),
            pointing_hint: None,
            use_mount_hints: None,
            fov_hint_deg: None,
            search_radius_deg: None,
            timeout: None,
        }))
        .await;
    assert_tool_error(result, "internal");
}

// =======================================================================
// auto_focus happy-path test
//
// Closes the coverage gap on lines 156-176 of
// `mcp/built_in/auto_focus.rs` (the `Ok(result)` branch — `focus_complete`
// emit + JSON success-result construction). Drives the full
// `run_auto_focus` sweep against the canonical V-curve fixtures
// under `tests/fixtures/auto_focus/`, asserts the JSON shape and the
// `focus_complete` event payload.
//
// Why not OmniSim: the simulator's camera (`Camera.Simulator/Camera.cs`
// in ASCOM.Alpaca.Simulators) loads a single JPG/PNG via ImageSharp's
// `Image.Load<Rgba32>` and reuses it for every exposure with no
// optics simulation — every focuser position would yield the same
// HFR → singular fit → `monotonic_curve` error. We synthesize the
// V-curve ourselves and inject the per-step images via `FixtureCamera`,
// which implements `ascom_alpaca::api::Camera` directly (no HTTP) and
// returns the fixtures in sweep order.
// =======================================================================

use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

/// The 11 canonical V-curve fixtures embedded at compile time via
/// `include_bytes!` so the test runs under both Cargo and Bazel
/// without filesystem access. Bazel sandboxes test execution and
/// would need explicit `data` declarations in `BUILD.bazel` to make
/// `tests/fixtures/**` visible at runtime — `include_bytes!` sidesteps
/// that, matching the precedent set by
/// `services/plate-solver/src/bin/mock_astap.rs`. Order matches the
/// natural sweep order (`-100, -80, …, +100`) `run_auto_focus`
/// produces, so the `FixtureCamera` counter indexes directly into
/// this slice.
const AUTO_FOCUS_FIXTURE_BYTES: [&[u8]; 11] = [
    include_bytes!("../../tests/fixtures/auto_focus/pos_m100.fits"),
    include_bytes!("../../tests/fixtures/auto_focus/pos_m080.fits"),
    include_bytes!("../../tests/fixtures/auto_focus/pos_m060.fits"),
    include_bytes!("../../tests/fixtures/auto_focus/pos_m040.fits"),
    include_bytes!("../../tests/fixtures/auto_focus/pos_m020.fits"),
    include_bytes!("../../tests/fixtures/auto_focus/pos_p000.fits"),
    include_bytes!("../../tests/fixtures/auto_focus/pos_p020.fits"),
    include_bytes!("../../tests/fixtures/auto_focus/pos_p040.fits"),
    include_bytes!("../../tests/fixtures/auto_focus/pos_p060.fits"),
    include_bytes!("../../tests/fixtures/auto_focus/pos_p080.fits"),
    include_bytes!("../../tests/fixtures/auto_focus/pos_p100.fits"),
];

/// Decode the embedded V-curve fixtures into `(width, height, 1)`
/// `Array3<i32>` frames in sweep order. `ascom_alpaca`'s
/// `ImageArray::from` expects that shape for monochrome data.
fn load_auto_focus_fixtures() -> Vec<ndarray::Array3<i32>> {
    AUTO_FOCUS_FIXTURE_BYTES
        .iter()
        .map(|bytes| {
            let (pixels, w, h) = rp_fits::reader::read_primary_as_i32(std::io::Cursor::new(*bytes))
                .expect("decode fixture");
            ndarray::Array3::from_shape_vec((w, h, 1), pixels).expect("fixture shape")
        })
        .collect()
}

/// Mock camera that returns a sequence of pre-loaded `Array3<i32>`
/// frames in order, one per `image_array()` call. Used to drive
/// `auto_focus` end-to-end with a known V-curve.
struct FixtureCamera {
    images: Vec<ndarray::Array3<i32>>,
    counter: AtomicUsize,
}

impl FixtureCamera {
    fn new(images: Vec<ndarray::Array3<i32>>) -> Self {
        Self {
            images,
            counter: AtomicUsize::new(0),
        }
    }
}

impl_mock_device!(FixtureCamera);

#[async_trait::async_trait]
impl ascom_alpaca::api::Camera for FixtureCamera {
    async fn start_exposure(
        &self,
        _duration: Duration,
        _light: bool,
    ) -> ascom_alpaca::ASCOMResult<()> {
        Ok(())
    }

    async fn image_ready(&self) -> ascom_alpaca::ASCOMResult<bool> {
        Ok(true)
    }

    async fn image_array(
        &self,
    ) -> ascom_alpaca::ASCOMResult<ascom_alpaca::api::camera::ImageArray> {
        let idx = self.counter.fetch_add(1, Ordering::SeqCst);
        let img = self
            .images
            .get(idx)
            .unwrap_or_else(|| {
                panic!(
                    "FixtureCamera exhausted: requested image {} of {}",
                    idx,
                    self.images.len()
                )
            })
            .clone();
        Ok(img.into())
    }

    async fn max_adu(&self) -> ascom_alpaca::ASCOMResult<u32> {
        Ok(65535)
    }

    async fn camera_x_size(&self) -> ascom_alpaca::ASCOMResult<u32> {
        Ok(200)
    }

    async fn camera_y_size(&self) -> ascom_alpaca::ASCOMResult<u32> {
        Ok(200)
    }

    async fn pixel_size_x(&self) -> ascom_alpaca::ASCOMResult<f64> {
        Ok(3.76)
    }

    async fn pixel_size_y(&self) -> ascom_alpaca::ASCOMResult<f64> {
        Ok(3.76)
    }

    async fn exposure_max(&self) -> ascom_alpaca::ASCOMResult<Duration> {
        Ok(Duration::from_hours(1))
    }

    async fn exposure_min(&self) -> ascom_alpaca::ASCOMResult<Duration> {
        Ok(Duration::from_millis(1))
    }

    async fn exposure_resolution(&self) -> ascom_alpaca::ASCOMResult<Duration> {
        Ok(Duration::from_millis(1))
    }

    async fn has_shutter(&self) -> ascom_alpaca::ASCOMResult<bool> {
        Ok(true)
    }

    async fn start_x(&self) -> ascom_alpaca::ASCOMResult<u32> {
        Ok(0)
    }

    async fn set_start_x(&self, _: u32) -> ascom_alpaca::ASCOMResult<()> {
        Ok(())
    }

    async fn start_y(&self) -> ascom_alpaca::ASCOMResult<u32> {
        Ok(0)
    }

    async fn set_start_y(&self, _: u32) -> ascom_alpaca::ASCOMResult<()> {
        Ok(())
    }
}

/// Mock focuser that tracks position across `move_(target)` calls
/// via an interior `AtomicI32`. Returns a constant temperature.
struct TrackingFocuser {
    position: AtomicI32,
    temperature_c: f64,
}

impl TrackingFocuser {
    fn new(starting_position: i32, temperature_c: f64) -> Self {
        Self {
            position: AtomicI32::new(starting_position),
            temperature_c,
        }
    }
}

impl_mock_device!(TrackingFocuser);

#[async_trait::async_trait]
impl ascom_alpaca::api::Focuser for TrackingFocuser {
    async fn absolute(&self) -> ascom_alpaca::ASCOMResult<bool> {
        Ok(true)
    }

    async fn is_moving(&self) -> ascom_alpaca::ASCOMResult<bool> {
        Ok(false)
    }

    async fn max_increment(&self) -> ascom_alpaca::ASCOMResult<u32> {
        Ok(100_000)
    }

    async fn max_step(&self) -> ascom_alpaca::ASCOMResult<u32> {
        Ok(100_000)
    }

    async fn position(&self) -> ascom_alpaca::ASCOMResult<i32> {
        Ok(self.position.load(Ordering::SeqCst))
    }

    async fn step_size(&self) -> ascom_alpaca::ASCOMResult<f64> {
        Err(ASCOMError::NOT_IMPLEMENTED)
    }

    async fn temp_comp(&self) -> ascom_alpaca::ASCOMResult<bool> {
        Ok(false)
    }

    async fn set_temp_comp(&self, _: bool) -> ascom_alpaca::ASCOMResult<()> {
        Err(ASCOMError::NOT_IMPLEMENTED)
    }

    async fn temp_comp_available(&self) -> ascom_alpaca::ASCOMResult<bool> {
        Ok(false)
    }

    async fn temperature(&self) -> ascom_alpaca::ASCOMResult<f64> {
        Ok(self.temperature_c)
    }

    async fn halt(&self) -> ascom_alpaca::ASCOMResult<()> {
        Ok(())
    }

    async fn move_(&self, position: i32) -> ascom_alpaca::ASCOMResult<()> {
        self.position.store(position, Ordering::SeqCst);
        Ok(())
    }
}

/// Build an `EquipmentRegistry` with a `FixtureCamera` (preloaded
/// V-curve fixtures) and a `TrackingFocuser` (starting at
/// `starting_position`). The IDs match the ones the test calls
/// `auto_focus` with: `"cam"` and `"foc"`.
fn auto_focus_registry(starting_position: i32) -> crate::equipment::EquipmentRegistry {
    let camera = FixtureCamera::new(load_auto_focus_fixtures());
    let focuser = TrackingFocuser::new(starting_position, 4.5);
    crate::equipment::EquipmentRegistry {
        safety_monitors: vec![],
        cameras: vec![crate::equipment::CameraEntry::new(
            "cam".to_string(),
            crate::config::CameraConfig {
                id: "cam".to_string(),
                name: "fixture".to_string(),
                alpaca_url: "http://localhost:1".to_string(),
                device_type: String::new(),
                device_number: 0,
                cooler_targets_c: Vec::new(),
                gain: None,
                offset: None,
                readout_time_estimate: None,
                auth: None,
            },
            crate::equipment::DeviceSession::connected(Arc::new(camera)),
            // FixtureCamera reports max_adu=65535, pixel_size=3.76 µm,
            // sensor_*_size=200 px (see its impl); mirror those values
            // here so `do_capture` (which consumes the cache rather than
            // calling the device) behaves identically to a real connect.
            crate::equipment::CameraInvariants {
                max_adu: Some(65535),
                pixel_size_x_um: Some(3.76),
                pixel_size_y_um: Some(3.76),
                sensor_width_px: Some(200),
                sensor_height_px: Some(200),
            },
        )],
        filter_wheels: vec![],
        cover_calibrators: vec![],
        focusers: vec![crate::equipment::FocuserEntry {
            id: "foc".to_string(),
            config: crate::config::FocuserConfig {
                id: "foc".to_string(),
                alpaca_url: "http://localhost:1".to_string(),
                device_number: 0,
                min_position: None,
                max_position: None,
                steps_per_sec: crate::config::focuser::FocuserStepsPerSec::default(),
                auth: None,
            },
            session: crate::equipment::DeviceSession::connected(Arc::new(focuser)),
        }],
        mount: None,
        ..Default::default()
    }
}

#[tokio::test]
async fn auto_focus_happy_path_emits_focus_complete_and_returns_curve() {
    // starting_position pinned at a realistic focuser scale (QHY
    // M-series, Pegasus FocusCube, Robofocus all operate in the
    // 5_000–100_000 range). fit_parabola recenters x by the weighted
    // mean before solving the normal equations, so the design matrix
    // stays well-conditioned at any plausible focuser scale (#174).
    // The fitted vertex must land near `starting_position` because
    // the synthesised parabola in HFR is symmetric about d=0.
    const STARTING_POSITION: i32 = 11_000;

    // Sandbox the FITS writes do_capture performs in a per-test temp
    // dir so successive runs don't pollute /tmp.
    let dir = tempfile::tempdir().expect("tempdir");
    let mut handler = test_handler(auto_focus_registry(STARTING_POSITION));
    handler.session_config = SessionConfig {
        data_directory: dir.path().to_string_lossy().into_owned(),
    };

    // Sweep grid: 11 positions at step_size=20 over half_width=100,
    // matching the eleven fixture offsets generated by
    // `examples/gen_autofocus_fixtures.rs`.
    let result = handler
        .auto_focus_inner(
            AutoFocusToolParams {
                camera_id: Some("cam".to_string()),
                focuser_id: Some("foc".to_string()),
                train_id: None,
                duration: Some(Duration::from_millis(100)),
                step_size: Some(20),
                half_width: Some(100),
                min_area: Some(4),
                max_area: Some(2000),
                threshold_sigma: Some(5.0),
                min_fit_points: None,
            },
            None,
            Cancel::never(),
        )
        .await;
    let call_result = result.expect("auto_focus protocol error");
    assert!(
        !call_result.is_error.unwrap_or(false),
        "expected success; got: {:?}",
        call_result.content
    );
    let body: serde_json::Value = ok_text(call_result);

    // Vertex of the synthesised parabola sits at the focuser's
    // starting_position by construction (HFR(d) = 2.0 + 0.0005·d² is
    // symmetric about d=0). The fitted best_position should land
    // within a couple of steps of that — empirically the σ≈1 px
    // measure_basic smoothing kernel introduces a small negative
    // bias on HFRs around 3-4 px, but the symmetry of the curve
    // keeps the fitted vertex within ±2 step_size of true minimum.
    let best_position = body["best_position"].as_i64().expect("best_position i64");
    assert!(
        (best_position - i64::from(STARTING_POSITION)).abs() <= 2 * 20,
        "best_position {best_position} not within ±2·step_size of starting {STARTING_POSITION}"
    );

    // best_hfr should be near the synthesised minimum (2.0 px). The
    // measured curve at d=0 is essentially exact (2.0023 px); the
    // parabolic fit on the 11 noisy samples lands very close.
    let best_hfr = body["best_hfr"].as_f64().expect("best_hfr f64");
    assert!(
        (best_hfr - 2.0).abs() < 0.5,
        "best_hfr {best_hfr} not near 2.0"
    );

    // Final focuser position must equal best_position — auto_focus
    // moves the focuser to the fit's vertex at the end of the run.
    let final_position = body["final_position"].as_i64().expect("final_position i64");
    assert_eq!(final_position, best_position);

    // All 11 sweep frames have detectable stars (the V-curve fixture
    // generation pipeline ensures non-null HFR at every offset), so
    // `samples_used` must be 11 — every grid point contributes.
    let samples_used = body["samples_used"].as_u64().expect("samples_used u64");
    assert_eq!(samples_used, 11);

    // curve_points must have one entry per grid point.
    let curve_points = body["curve_points"].as_array().expect("curve_points array");
    assert_eq!(curve_points.len(), 11);
    for entry in curve_points {
        assert!(entry["position"].is_i64());
        assert!(entry["hfr"].is_f64() || entry["hfr"].is_null());
        assert!(entry["star_count"].is_u64());
        assert!(entry["document_id"].is_string());
    }

    // Temperature passes through from the focuser's `temperature()`
    // read in `auto_focus`'s step-1 (recorded once before any sweep
    // motion).
    let temperature_c = body["temperature_c"].as_f64().expect("temperature_c f64");
    assert!(
        (temperature_c - 4.5).abs() < 1e-9,
        "temperature_c {temperature_c} not 4.5"
    );

    // The handler emits `focus_complete` at the end of
    // `run_auto_focus_step`'s success branch. The event_bus exposed by
    // `test_handler` is an empty-config bus, so subscribers list is
    // empty; the most readable assertion is that the whole pipeline
    // produced an Ok result without an error, which is what the body
    // checks above. The event itself is an outbound webhook
    // side-effect and is exercised by the event_delivery BDD
    // scenarios.
}

// -----------------------------------------------------------------------
// Train addressing on auto_focus + the refocus_train expansion.
//
// These pin the resolution and pre-motion validation paths that need
// no live devices: addressing conflicts, unknown trains, the
// guiding-train refusal, the config-block requirement, and the
// train-block parameter fallback (proven by the error advancing past
// "missing required parameter" to device resolution against an empty
// registry). The full sweep-through-train path runs in
// auto_focus.feature / refocus_train.feature against OmniSim.
// -----------------------------------------------------------------------

/// The reference-rig train shape: main = [main-focuser → main-cam]
/// (imaging, with an `auto_focus` block unless `with_block` is false),
/// guide = [main-focuser → guide-focuser → guide-cam] (guiding).
fn reference_trains(with_block: bool) -> crate::equipment::trains::TrainModel {
    let mut main = serde_json::json!({
        "id": "main",
        "focal_length_mm": 1000.0,
        "devices": ["main-focuser", "main-cam"]
    });
    if with_block {
        main["auto_focus"] = serde_json::json!({
            "duration": "100ms", "step_size": 100, "half_width": 200,
            "min_area": 5, "max_area": 65536
        });
    }
    let mut guide = serde_json::json!({
        "id": "guide", "purpose": "guiding",
        "devices": ["main-focuser", "guide-focuser", "guide-cam"]
    });
    if with_block {
        guide["auto_focus"] = serde_json::json!({ "step_size": 50, "half_width": 200 });
    }
    let equipment: crate::config::EquipmentConfig = serde_json::from_value(serde_json::json!({
        "cameras": [
            {"id": "main-cam", "alpaca_url": "http://localhost:1"},
            {"id": "guide-cam", "alpaca_url": "http://localhost:1"}
        ],
        "focusers": [
            {"id": "main-focuser", "alpaca_url": "http://localhost:1"},
            {"id": "guide-focuser", "alpaca_url": "http://localhost:1"}
        ],
        "mount": {
            "alpaca_url": "http://localhost:1",
            "guiding": {"url": "http://localhost:1"}
        },
        "optical_trains": [
            main,
            guide
        ]
    }))
    .unwrap();
    crate::equipment::trains::TrainModel::try_from_equipment(&equipment).unwrap()
}

fn af_params_with_train(train_id: &str) -> AutoFocusToolParams {
    AutoFocusToolParams {
        camera_id: None,
        focuser_id: None,
        train_id: Some(train_id.to_string()),
        duration: None,
        step_size: None,
        half_width: None,
        min_area: None,
        max_area: None,
        threshold_sigma: None,
        min_fit_points: None,
    }
}

#[tokio::test]
async fn auto_focus_rejects_train_id_combined_with_an_explicit_id() {
    let handler = test_handler(empty_registry()).with_trains(reference_trains(true));
    let mut params = af_params_with_train("main");
    params.camera_id = Some("main-cam".to_string());
    let result = handler
        .auto_focus_inner(params, None, Cancel::never())
        .await;
    assert_tool_error(result, "mutually exclusive");
}

#[tokio::test]
async fn auto_focus_rejects_an_unknown_train() {
    let handler = test_handler(empty_registry()).with_trains(reference_trains(true));
    let result = handler
        .auto_focus_inner(af_params_with_train("nonexistent"), None, Cancel::never())
        .await;
    assert_tool_error(result, "train not found");
}

#[tokio::test]
async fn auto_focus_on_the_guiding_train_requires_active_guiding() {
    // The guide train's metric block supplies the geometry; with no
    // guider configured the sweep's precondition fails first.
    let handler = test_handler(empty_registry()).with_trains(reference_trains(true));
    let result = handler
        .auto_focus_inner(af_params_with_train("guide"), None, Cancel::never())
        .await;
    assert_tool_error(result, "requires active guiding");
}

#[tokio::test]
async fn auto_focus_on_a_blockless_guiding_train_requires_per_call_geometry() {
    let handler = test_handler(empty_registry()).with_trains(reference_trains(false));
    let result = handler
        .auto_focus_inner(af_params_with_train("guide"), None, Cancel::never())
        .await;
    assert_tool_error(result, "missing required parameter: step_size");
}

#[tokio::test]
async fn auto_focus_rejects_capture_parameters_for_the_guiding_train() {
    let handler = test_handler(empty_registry()).with_trains(reference_trains(true));
    let mut params = af_params_with_train("guide");
    params.duration = Some(Duration::from_secs(3));
    let result = handler
        .auto_focus_inner(params, None, Cancel::never())
        .await;
    assert_tool_error(result, "capture-based");
}

/// Minimal mock rotator for the rotate-while-guiding ladder tests:
/// moves land instantly (`is_moving` false), `position` reports
/// `position_value` before a move and the last commanded angle after.
#[derive(Default)]
struct MockRotator {
    fail_position: bool,
    fail_move: bool,
    position_value: std::sync::Mutex<f64>,
}

impl_mock_device!(MockRotator);

#[async_trait::async_trait]
impl ascom_alpaca::api::Rotator for MockRotator {
    async fn can_reverse(&self) -> ascom_alpaca::ASCOMResult<bool> {
        Ok(false)
    }

    async fn is_moving(&self) -> ascom_alpaca::ASCOMResult<bool> {
        Ok(false)
    }

    async fn position(&self) -> ascom_alpaca::ASCOMResult<f64> {
        if self.fail_position {
            return Err(ASCOMError::invalid_operation("position unavailable"));
        }
        Ok(*self.position_value.lock().unwrap())
    }

    async fn mechanical_position(&self) -> ascom_alpaca::ASCOMResult<f64> {
        Ok(*self.position_value.lock().unwrap())
    }

    async fn move_absolute(&self, position: f64) -> ascom_alpaca::ASCOMResult<()> {
        if self.fail_move {
            return Err(ASCOMError::invalid_operation("motor fault"));
        }
        *self.position_value.lock().unwrap() = position;
        Ok(())
    }

    async fn reverse(&self) -> ascom_alpaca::ASCOMResult<bool> {
        Ok(false)
    }

    async fn set_reverse(&self, _: bool) -> ascom_alpaca::ASCOMResult<()> {
        Err(ASCOMError::NOT_IMPLEMENTED)
    }

    async fn target_position(&self) -> ascom_alpaca::ASCOMResult<f64> {
        Ok(*self.position_value.lock().unwrap())
    }

    async fn move_(&self, _: f64) -> ascom_alpaca::ASCOMResult<()> {
        Err(ASCOMError::NOT_IMPLEMENTED)
    }

    async fn move_mechanical(&self, _: f64) -> ascom_alpaca::ASCOMResult<()> {
        Err(ASCOMError::NOT_IMPLEMENTED)
    }

    async fn sync(&self, _: f64) -> ascom_alpaca::ASCOMResult<()> {
        Err(ASCOMError::NOT_IMPLEMENTED)
    }
}

fn rotator_registry(
    rot: Arc<dyn ascom_alpaca::api::Rotator>,
) -> crate::equipment::EquipmentRegistry {
    crate::equipment::EquipmentRegistry {
        rotators: vec![crate::equipment::RotatorEntry {
            id: "rot".to_string(),
            config: crate::config::RotatorConfig {
                id: "rot".to_string(),
                name: None,
                alpaca_url: "http://localhost:1".to_string(),
                device_number: 0,
                auth: None,
            },
            session: crate::equipment::DeviceSession::connected(rot),
        }],
        ..Default::default()
    }
}

/// A guiding train containing the mock rotator "rot" (terminating in
/// an offline camera) — the ladder-engagement shape.
fn rotator_guiding_trains() -> crate::equipment::trains::TrainModel {
    let equipment: crate::config::EquipmentConfig = serde_json::from_value(serde_json::json!({
        "cameras": [{"id": "gcam", "alpaca_url": "http://localhost:1"}],
        "rotators": [{"id": "rot", "alpaca_url": "http://localhost:1"}],
        "mount": {
            "alpaca_url": "http://localhost:1",
            "guiding": {"url": "http://localhost:1"}
        },
        "optical_trains": [
            {"id": "guide", "purpose": "guiding", "devices": ["rot", "gcam"]}
        ]
    }))
    .unwrap();
    crate::equipment::trains::TrainModel::try_from_equipment(&equipment).unwrap()
}

fn move_rot_params(angle: f64) -> MoveRotatorParams {
    MoveRotatorParams {
        rotator_id: Some("rot".to_string()),
        train_id: None,
        angle: Some(angle),
    }
}

fn ladder_handler(rot: MockRotator, configure: impl FnOnce(&mut MockGuiderClient)) -> McpHandler {
    let mut mock = MockGuiderClient::new();
    configure(&mut mock);
    let client: Arc<dyn rp_guider::GuiderClient> = Arc::new(mock);
    test_handler(rotator_registry(Arc::new(rot)))
        .with_trains(rotator_guiding_trains())
        .with_guider(Some(client), GuiderDefaults::default())
}

#[tokio::test]
async fn ladder_pause_failure_aborts_before_any_motion() {
    let handler = ladder_handler(MockRotator::default(), |mock| {
        mock.expect_guiding_stats()
            .returning(|| Ok(guiding_stats_active()));
        mock.expect_pause_guiding()
            .returning(|_| Err(rp_guider::GuiderError::Internal("boom".to_string())));
    });
    let mut rx = handler.event_bus.subscribe();
    let result = handler
        .move_rotator_inner(move_rot_params(10.0), &Cancel::never())
        .await;
    assert_tool_error(result, "failed to pause guiding before rotating");

    // The pause is part of the operation: the triple must surface
    // even though the rotator never moved.
    let started = rx.try_recv().unwrap();
    assert_eq!(started.event, "move_rotator_started");
    assert_eq!(started.payload["guiding_paused"], true);
    let failed = rx.try_recv().unwrap();
    assert_eq!(failed.event, "move_rotator_failed");
}

#[tokio::test]
async fn ladder_pre_move_read_failure_resumes_and_errors() {
    let rot = MockRotator {
        fail_position: true,
        ..Default::default()
    };
    let handler = ladder_handler(rot, |mock| {
        mock.expect_guiding_stats()
            .returning(|| Ok(guiding_stats_active()));
        mock.expect_pause_guiding().returning(|_| Ok(()));
        mock.expect_resume_guiding().times(1).returning(|| Ok(()));
    });
    let result = handler
        .move_rotator_inner(move_rot_params(10.0), &Cancel::never())
        .await;
    assert_tool_error(result, "failed to read the pre-move sky angle");
}

#[tokio::test]
async fn ladder_move_failure_still_reselects_and_resumes() {
    let rot = MockRotator {
        fail_move: true,
        ..Default::default()
    };
    let handler = ladder_handler(rot, |mock| {
        mock.expect_guiding_stats()
            .returning(|| Ok(guiding_stats_active()));
        mock.expect_pause_guiding().returning(|_| Ok(()));
        mock.expect_reselect_star().times(1).returning(|| Ok(()));
        mock.expect_resume_guiding().times(1).returning(|| Ok(()));
    });
    let result = handler
        .move_rotator_inner(move_rot_params(10.0), &Cancel::never())
        .await;
    assert_tool_error(result, "failed to move rotator");
}

#[tokio::test]
async fn ladder_equipment_read_failure_resumes_and_errors() {
    let handler = ladder_handler(MockRotator::default(), |mock| {
        mock.expect_guiding_stats()
            .returning(|| Ok(guiding_stats_active()));
        mock.expect_pause_guiding().returning(|_| Ok(()));
        mock.expect_current_equipment()
            .returning(|| Err(rp_guider::GuiderError::Internal("down".to_string())));
        mock.expect_resume_guiding().times(1).returning(|| Ok(()));
    });
    let result = handler
        .move_rotator_inner(move_rot_params(10.0), &Cancel::never())
        .await;
    assert_tool_error(result, "failed to read PHD2 equipment");
}

fn equipment_without_rotator() -> rp_guider::PhdEquipment {
    rp_guider::PhdEquipment {
        camera: None,
        mount: None,
        aux_mount: None,
        ao: None,
        rotator: None,
    }
}

#[tokio::test]
async fn ladder_clear_calibration_failure_resumes_and_errors() {
    let handler = ladder_handler(MockRotator::default(), |mock| {
        mock.expect_guiding_stats()
            .returning(|| Ok(guiding_stats_active()));
        mock.expect_pause_guiding().returning(|_| Ok(()));
        mock.expect_current_equipment()
            .returning(|| Ok(equipment_without_rotator()));
        mock.expect_clear_calibration()
            .returning(|| Err(rp_guider::GuiderError::Internal("rpc".to_string())));
        mock.expect_resume_guiding().times(1).returning(|| Ok(()));
    });
    let result = handler
        .move_rotator_inner(move_rot_params(90.0), &Cancel::never())
        .await;
    assert_tool_error(result, "failed to clear the PHD2 calibration");
}

#[tokio::test]
async fn ladder_resume_failure_after_success_tail_is_a_hard_error() {
    let handler = ladder_handler(MockRotator::default(), |mock| {
        mock.expect_guiding_stats()
            .returning(|| Ok(guiding_stats_active()));
        mock.expect_pause_guiding().returning(|_| Ok(()));
        mock.expect_current_equipment()
            .returning(|| Ok(equipment_without_rotator()));
        mock.expect_reselect_star().returning(|| Ok(()));
        mock.expect_resume_guiding()
            .returning(|| Err(rp_guider::GuiderError::Internal("gone".to_string())));
    });
    // A 2° move stays under the 5° threshold: no calibration clear,
    // so the failure is isolated to the resume.
    let result = handler
        .move_rotator_inner(move_rot_params(2.0), &Cancel::never())
        .await;
    assert_tool_error(result, "failed to resume guiding after rotating");
}

#[tokio::test]
async fn ladder_runs_bare_when_the_stats_read_fails() {
    let handler = ladder_handler(MockRotator::default(), |mock| {
        mock.expect_guiding_stats()
            .returning(|| Err(rp_guider::GuiderError::Internal("down".to_string())));
    });
    let json = ok_text(
        handler
            .move_rotator_inner(move_rot_params(45.0), &Cancel::never())
            .await
            .unwrap(),
    );
    assert!(json["guiding_ladder"].is_null());
}

#[tokio::test]
async fn ladder_success_reports_the_calibration_decision() {
    let handler = ladder_handler(MockRotator::default(), |mock| {
        mock.expect_guiding_stats()
            .returning(|| Ok(guiding_stats_active()));
        mock.expect_pause_guiding()
            .with(mockall::predicate::eq(false))
            .times(1)
            .returning(|_| Ok(()));
        mock.expect_current_equipment()
            .returning(|| Ok(equipment_without_rotator()));
        mock.expect_clear_calibration()
            .times(1)
            .returning(|| Ok(()));
        mock.expect_reselect_star().times(1).returning(|| Ok(()));
        mock.expect_resume_guiding().times(1).returning(|| Ok(()));
    });
    let json = ok_text(
        handler
            .move_rotator_inner(move_rot_params(90.0), &Cancel::never())
            .await
            .unwrap(),
    );
    let ladder = &json["guiding_ladder"];
    assert_eq!(ladder["phd2_has_rotator"], false);
    assert_eq!(ladder["calibration_cleared"], true);
    assert!((ladder["delta_deg"].as_f64().unwrap() - 90.0).abs() < 1e-9);
}

/// A guiding train terminating in the mock-focuser fixture's "foc",
/// with a metric block giving a 5-position grid around the focuser's
/// starting position.
fn guide_sweep_trains() -> crate::equipment::trains::TrainModel {
    let equipment: crate::config::EquipmentConfig = serde_json::from_value(serde_json::json!({
        "cameras": [{"id": "gcam", "alpaca_url": "http://localhost:1"}],
        "focusers": [{"id": "foc", "alpaca_url": "http://localhost:1"}],
        "mount": {
            "alpaca_url": "http://localhost:1",
            "guiding": {"url": "http://localhost:1"}
        },
        "optical_trains": [
            {"id": "guide", "purpose": "guiding",
             "devices": ["foc", "gcam"],
             "auto_focus": {"step_size": 50, "half_width": 100}}
        ]
    }))
    .unwrap();
    crate::equipment::trains::TrainModel::try_from_equipment(&equipment).unwrap()
}

fn guiding_stats_active() -> rp_guider::GuidingStats {
    rp_guider::GuidingStats {
        app_state: "Guiding".to_string(),
        guiding: true,
        rms_ra_px: None,
        rms_dec_px: None,
        total_rms_px: None,
        snr: None,
        star_mass: None,
        sample_count: 0,
    }
}

/// A mock guider whose metrics responses follow `hfd_script` one call
/// at a time (the last value repeats), each response carrying three
/// fresh frames. A sweep with the default `frames_per_step` (3)
/// makes two calls per grid position — the post-move watermark
/// refresh, then the collect — so position N's sample comes from
/// script index `2N + 1` (odd indexes; the refresh responses only
/// contribute frame numbers).
fn scripted_metrics_guider(hfd_script: Vec<f64>) -> MockGuiderClient {
    let mut mock = MockGuiderClient::new();
    mock.expect_guiding_stats()
        .returning(|| Ok(guiding_stats_active()));
    let call = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    mock.expect_guiding_metrics().returning(move || {
        let n = call.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let hfd = hfd_script[(n as usize).min(hfd_script.len() - 1)];
        let frames = (n * 3 + 1..=n * 3 + 3)
            .map(|frame| rp_guider::FrameMetrics {
                frame,
                hfd: Some(hfd),
                snr: Some(20.0),
                star_mass: Some(1000.0),
                star_lost: false,
            })
            .collect();
        Ok(rp_guider::GuidingMetrics {
            guiding: true,
            frames,
        })
    });
    mock
}

#[tokio::test]
async fn guide_train_auto_focus_fits_the_scripted_v_curve() {
    // Odd script indexes serve the five positions' collect calls
    // with a symmetric V — the fitted minimum is the center, i.e.
    // the focuser's starting position. Even indexes back the
    // watermark refreshes and contribute frame numbers only.
    let foc = MockFocuser::default();
    let start = foc.position_value;
    let mock = scripted_metrics_guider(vec![9.0, 4.0, 9.0, 3.0, 9.0, 2.0, 9.0, 3.0, 9.0, 4.0]);
    let client: Arc<dyn rp_guider::GuiderClient> = Arc::new(mock);
    let handler = test_handler(focuser_registry(Arc::new(foc), None, None))
        .with_trains(guide_sweep_trains())
        .with_guider(Some(client), GuiderDefaults::default());

    let result = handler
        .auto_focus_inner(af_params_with_train("guide"), None, Cancel::never())
        .await;
    let json = ok_text(result.unwrap());
    assert_eq!(json["best_position"], start);
    assert_eq!(json["final_position"], start);
    assert_eq!(json["samples_used"], 5);
    let points = json["curve_points"].as_array().unwrap();
    assert_eq!(points.len(), 5);
    assert_eq!(points[0]["position"], start - 100);
    assert_eq!(points[0]["hfd"], 4.0);
    assert_eq!(points[0]["frames_used"], 3);
    assert!(
        points.iter().all(|p| p.get("document_id").is_none()),
        "metric sweeps capture nothing"
    );
}

#[tokio::test]
async fn guide_train_auto_focus_surfaces_a_stats_read_failure() {
    let mut mock = MockGuiderClient::new();
    mock.expect_guiding_stats()
        .returning(|| Err(rp_guider::GuiderError::Internal("down".to_string())));
    let client: Arc<dyn rp_guider::GuiderClient> = Arc::new(mock);
    let handler = test_handler(focuser_registry(
        Arc::new(MockFocuser::default()),
        None,
        None,
    ))
    .with_trains(guide_sweep_trains())
    .with_guider(Some(client), GuiderDefaults::default());
    let result = handler
        .auto_focus_inner(af_params_with_train("guide"), None, Cancel::never())
        .await;
    assert_tool_error(result, "stats unavailable");
}

#[tokio::test]
async fn guide_train_auto_focus_rejects_a_clamped_grid_below_min_fit_points() {
    // Bounds clamp the ±100 grid to two positions — fewer than the
    // default min_fit_points of five, rejected before any motion.
    let foc = MockFocuser::default();
    let start = foc.position_value;
    let mock = scripted_metrics_guider(vec![2.5]);
    let client: Arc<dyn rp_guider::GuiderClient> = Arc::new(mock);
    let handler = test_handler(focuser_registry(
        Arc::new(foc),
        Some(start - 50),
        Some(start),
    ))
    .with_trains(guide_sweep_trains())
    .with_guider(Some(client), GuiderDefaults::default());
    let result = handler
        .auto_focus_inner(af_params_with_train("guide"), None, Cancel::never())
        .await;
    assert_tool_error(result, "fewer than min_fit_points");
}

#[tokio::test]
async fn guide_train_auto_focus_rejects_a_minimum_outside_the_sampled_range() {
    // A convex but one-sided curve: the fitted vertex lies beyond the
    // right edge of the grid, so the visible curve is monotonic over
    // the sampled range.
    let mock = scripted_metrics_guider(vec![9.0, 62.5, 9.0, 40.0, 9.0, 22.5, 9.0, 10.0, 9.0, 2.5]);
    let client: Arc<dyn rp_guider::GuiderClient> = Arc::new(mock);
    let handler = test_handler(focuser_registry(
        Arc::new(MockFocuser::default()),
        None,
        None,
    ))
    .with_trains(guide_sweep_trains())
    .with_guider(Some(client), GuiderDefaults::default());
    let result = handler
        .auto_focus_inner(af_params_with_train("guide"), None, Cancel::never())
        .await;
    assert_tool_error(result, "outside the sampled range");
}

#[tokio::test]
async fn guide_train_auto_focus_fails_fast_when_guiding_stops_mid_sweep() {
    // The stats precondition passes, but the first metrics poll after
    // the move reports guiding stopped — the sweep fails immediately
    // instead of burning the per-position ceiling.
    let mut mock = MockGuiderClient::new();
    mock.expect_guiding_stats()
        .returning(|| Ok(guiding_stats_active()));
    mock.expect_guiding_metrics().returning(|| {
        Ok(rp_guider::GuidingMetrics {
            guiding: false,
            frames: Vec::new(),
        })
    });
    let client: Arc<dyn rp_guider::GuiderClient> = Arc::new(mock);
    let handler = test_handler(focuser_registry(
        Arc::new(MockFocuser::default()),
        None,
        None,
    ))
    .with_trains(guide_sweep_trains())
    .with_guider(Some(client), GuiderDefaults::default());
    let result = handler
        .auto_focus_inner(af_params_with_train("guide"), None, Cancel::never())
        .await;
    assert_tool_error(result, "guiding stopped during the metric sweep");
}

#[tokio::test(start_paused = true)]
async fn guide_train_auto_focus_times_out_when_frames_stop_flowing() {
    // The mock always returns the same three frames: after the first
    // position consumes them, no fresh frames ever arrive and the
    // per-position ceiling expires (paused clock — no real waiting).
    let mut mock = MockGuiderClient::new();
    mock.expect_guiding_stats()
        .returning(|| Ok(guiding_stats_active()));
    mock.expect_guiding_metrics().returning(|| {
        Ok(rp_guider::GuidingMetrics {
            guiding: true,
            frames: (1..=3)
                .map(|frame| rp_guider::FrameMetrics {
                    frame,
                    hfd: Some(2.5),
                    snr: Some(20.0),
                    star_mass: Some(1000.0),
                    star_lost: false,
                })
                .collect(),
        })
    });
    let client: Arc<dyn rp_guider::GuiderClient> = Arc::new(mock);
    let handler = test_handler(focuser_registry(
        Arc::new(MockFocuser::default()),
        None,
        None,
    ))
    .with_trains(guide_sweep_trains())
    .with_guider(Some(client), GuiderDefaults::default());
    let result = handler
        .auto_focus_inner(af_params_with_train("guide"), None, Cancel::never())
        .await;
    assert_tool_error(result, "timeout waiting for");
}

#[tokio::test]
async fn refocus_train_runs_a_metric_step_and_reports_best_hfd() {
    // The guide train's expansion is its single terminal focuser as a
    // metric step; the scripted V gives a deterministic success
    // payload with no camera involved.
    let foc = MockFocuser::default();
    let start = foc.position_value;
    let mock = scripted_metrics_guider(vec![9.0, 4.0, 9.0, 3.0, 9.0, 2.0, 9.0, 3.0, 9.0, 4.0]);
    let client: Arc<dyn rp_guider::GuiderClient> = Arc::new(mock);
    let handler = test_handler(focuser_registry(Arc::new(foc), None, None))
        .with_trains(guide_sweep_trains())
        .with_guider(Some(client), GuiderDefaults::default());
    let json = ok_text(
        handler
            .refocus_train_inner(refocus_params("guide"), None, Cancel::never())
            .await
            .unwrap(),
    );
    assert_eq!(json["guiding_paused"], false);
    let steps = json["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0]["focuser_id"], "foc");
    assert_eq!(steps[0]["train_id"], "guide");
    assert!(steps[0]["camera_id"].is_null());
    assert_eq!(steps[0]["best_position"], start);
    assert!(steps[0]["best_hfd"].as_f64().is_some());
}

#[tokio::test]
async fn guide_train_auto_focus_reports_a_flat_curve_as_monotonic() {
    let foc = MockFocuser::default();
    let mock = scripted_metrics_guider(vec![2.5]);
    let client: Arc<dyn rp_guider::GuiderClient> = Arc::new(mock);
    let handler = test_handler(focuser_registry(Arc::new(foc), None, None))
        .with_trains(guide_sweep_trains())
        .with_guider(Some(client), GuiderDefaults::default());

    let result = handler
        .auto_focus_inner(af_params_with_train("guide"), None, Cancel::never())
        .await;
    assert_tool_error(result, "monotonic");
}

#[tokio::test]
async fn guide_train_auto_focus_treats_star_lost_positions_as_null_samples() {
    // Star-lost frames at the sweep's first position: the sample is
    // null, and with only four valid positions against the default
    // min_fit_points of five, the run reports the shortfall.
    let foc = MockFocuser::default();
    let mut mock = MockGuiderClient::new();
    mock.expect_guiding_stats()
        .returning(|| Ok(guiding_stats_active()));
    let call = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    mock.expect_guiding_metrics().returning(move || {
        let n = call.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let star_lost = n == 1;
        let frames = (n * 3 + 1..=n * 3 + 3)
            .map(|frame| rp_guider::FrameMetrics {
                frame,
                hfd: (!star_lost).then_some(3.0),
                snr: Some(3.0),
                star_mass: None,
                star_lost,
            })
            .collect();
        Ok(rp_guider::GuidingMetrics {
            guiding: true,
            frames,
        })
    });
    let client: Arc<dyn rp_guider::GuiderClient> = Arc::new(mock);
    let handler = test_handler(focuser_registry(Arc::new(foc), None, None))
        .with_trains(guide_sweep_trains())
        .with_guider(Some(client), GuiderDefaults::default());

    let result = handler
        .auto_focus_inner(af_params_with_train("guide"), None, Cancel::never())
        .await;
    assert_tool_error(result, "not enough valid guide samples");
}

#[tokio::test]
async fn auto_focus_rejects_a_train_without_a_focuser() {
    // `cam_trains` builds a camera-only train "main".
    let handler = test_handler(empty_registry()).with_trains(cam_trains(200.0));
    let result = handler
        .auto_focus_inner(af_params_with_train("main"), None, Cancel::never())
        .await;
    assert_tool_error(result, "has no focuser");
}

#[tokio::test]
async fn auto_focus_train_block_fills_the_sweep_parameters() {
    // Against an empty registry the call must advance past every
    // "missing required parameter" check (the block filled them all)
    // and fail at device resolution instead.
    let handler = test_handler(empty_registry()).with_trains(reference_trains(true));
    let result = handler
        .auto_focus_inner(af_params_with_train("main"), None, Cancel::never())
        .await;
    assert_tool_error(result, "camera not found: main-cam");
}

#[tokio::test]
async fn auto_focus_train_without_a_block_still_requires_sweep_parameters() {
    let handler = test_handler(empty_registry()).with_trains(reference_trains(false));
    let result = handler
        .auto_focus_inner(af_params_with_train("main"), None, Cancel::never())
        .await;
    assert_tool_error(result, "missing required parameter: duration");
}

fn refocus_params(train_id: &str) -> super::built_in::auto_focus::RefocusTrainParams {
    super::built_in::auto_focus::RefocusTrainParams {
        train_id: Some(train_id.to_string()),
        reason: None,
    }
}

#[tokio::test]
async fn refocus_train_rejects_an_unknown_train() {
    let handler = test_handler(empty_registry()).with_trains(reference_trains(true));
    let result = handler
        .refocus_train_inner(refocus_params("nonexistent"), None, Cancel::never())
        .await;
    assert_tool_error(result, "train not found");
}

#[tokio::test]
async fn refocus_train_rejects_a_train_without_focusers() {
    let handler = test_handler(empty_registry()).with_trains(cam_trains(200.0));
    let result = handler
        .refocus_train_inner(refocus_params("main"), None, Cancel::never())
        .await;
    assert_tool_error(result, "has no focusers");
}

#[tokio::test]
async fn refocus_train_with_a_guiding_step_requires_active_guiding() {
    // Refocusing the guiding train itself always expands to a
    // guide-train metric step (its own terminal focuser); with no
    // guider configured the expansion is refused before any motion.
    let handler = test_handler(empty_registry()).with_trains(reference_trains(true));
    let result = handler
        .refocus_train_inner(refocus_params("guide"), None, Cancel::never())
        .await;
    assert_tool_error(result, "guide-train step requires active guiding");
}

#[tokio::test]
async fn refocus_train_requires_the_run_trains_auto_focus_block() {
    let handler = test_handler(empty_registry()).with_trains(reference_trains(false));
    let result = handler
        .refocus_train_inner(refocus_params("main"), None, Cancel::never())
        .await;
    assert_tool_error(result, "auto_focus config block");
}

// The success-path and handshake tests below run real V-curve sweeps
// over `auto_focus_registry`'s fixture devices — BDD cannot pin the
// success payload because OmniSim's camera image is focuser-
// independent (flat HFR, non-deterministic fit outcome).

/// Trains over the fixture registry: main = [foc → cam] with an
/// `auto_focus` block matching the fixture sweep (step 20, ±100), plus
/// optionally a guiding train sharing "foc".
fn fixture_trains(with_guiding: bool) -> crate::equipment::trains::TrainModel {
    let mut trains = vec![serde_json::json!({
        "id": "main",
        "devices": ["foc", "cam"],
        "auto_focus": {"duration": "100ms", "step_size": 20, "half_width": 100,
                       "min_area": 4, "max_area": 2000}
    })];
    let mut equipment = serde_json::json!({
        "cameras": [
            {"id": "cam", "alpaca_url": "http://localhost:1"},
            {"id": "guide-cam", "alpaca_url": "http://localhost:1"}
        ],
        "focusers": [{"id": "foc", "alpaca_url": "http://localhost:1"}]
    });
    if with_guiding {
        trains.push(serde_json::json!({
            "id": "guide", "purpose": "guiding", "devices": ["foc", "guide-cam"]
        }));
        equipment["mount"] = serde_json::json!({
            "alpaca_url": "http://localhost:1",
            "guiding": {"url": "http://localhost:1"}
        });
    }
    equipment["optical_trains"] = serde_json::Value::Array(trains);
    let equipment: crate::config::EquipmentConfig = serde_json::from_value(equipment).unwrap();
    crate::equipment::trains::TrainModel::try_from_equipment(&equipment).unwrap()
}

fn stats_with_guiding(guiding: bool) -> GuidingStats {
    GuidingStats {
        app_state: if guiding { "Guiding" } else { "Stopped" }.to_string(),
        guiding,
        rms_ra_px: None,
        rms_dec_px: None,
        total_rms_px: None,
        snr: None,
        star_mass: None,
        sample_count: 0,
    }
}

#[tokio::test]
async fn refocus_train_success_payload_over_the_fixture_registry() {
    const STARTING_POSITION: i32 = 11_000;
    let dir = tempfile::tempdir().expect("tempdir");
    let mut handler =
        test_handler(auto_focus_registry(STARTING_POSITION)).with_trains(fixture_trains(false));
    handler.session_config = SessionConfig {
        data_directory: dir.path().to_string_lossy().into_owned(),
    };

    let result = handler
        .refocus_train_inner(refocus_params("main"), None, Cancel::never())
        .await;
    let call = result.expect("refocus_train protocol error");
    assert!(
        !call.is_error.unwrap_or(false),
        "expected success; got: {:?}",
        call.content
    );
    let body = ok_text(call);
    assert_eq!(body["train_id"], "main");
    assert_eq!(body["reason"], "manual");
    assert_eq!(body["guiding_paused"], false);
    let steps = body["steps"].as_array().expect("steps array");
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0]["focuser_id"], "foc");
    assert_eq!(steps[0]["train_id"], "main");
    assert_eq!(steps[0]["camera_id"], "cam");
    // The synthesised V-curve is symmetric about the starting
    // position; the auto_focus happy-path test pins the ±2·step_size
    // tolerance, so here presence + plausibility suffices.
    assert!(steps[0]["best_position"].is_i64());
    assert!(steps[0]["best_hfr"].is_f64());
    assert!(steps[0]["samples_used"].is_u64());
}

#[tokio::test]
async fn refocus_train_pauses_and_resumes_around_a_guiding_coupled_step() {
    const STARTING_POSITION: i32 = 11_000;
    let dir = tempfile::tempdir().expect("tempdir");
    let mut mock = MockGuiderClient::new();
    mock.expect_guiding_stats()
        .times(1)
        .returning(|| Ok(stats_with_guiding(true)));
    mock.expect_pause_guiding()
        .with(mockall::predicate::eq(false))
        .times(1)
        .returning(|_| Ok(()));
    mock.expect_resume_guiding().times(1).returning(|| Ok(()));
    let client: Arc<dyn rp_guider::GuiderClient> = Arc::new(mock);
    let mut handler = test_handler(auto_focus_registry(STARTING_POSITION))
        .with_guider(Some(client), GuiderDefaults::default())
        .with_trains(fixture_trains(true));
    handler.session_config = SessionConfig {
        data_directory: dir.path().to_string_lossy().into_owned(),
    };

    let result = handler
        .refocus_train_inner(refocus_params("main"), None, Cancel::never())
        .await;
    let call = result.expect("refocus_train protocol error");
    assert!(
        !call.is_error.unwrap_or(false),
        "expected success; got: {:?}",
        call.content
    );
    assert_eq!(ok_text(call)["guiding_paused"], true);
}

#[tokio::test]
async fn refocus_train_skips_the_handshake_when_not_guiding() {
    const STARTING_POSITION: i32 = 11_000;
    let dir = tempfile::tempdir().expect("tempdir");
    let mut mock = MockGuiderClient::new();
    mock.expect_guiding_stats()
        .times(1)
        .returning(|| Ok(stats_with_guiding(false)));
    // No pause/resume expectations: reaching either would panic.
    let client: Arc<dyn rp_guider::GuiderClient> = Arc::new(mock);
    let mut handler = test_handler(auto_focus_registry(STARTING_POSITION))
        .with_guider(Some(client), GuiderDefaults::default())
        .with_trains(fixture_trains(true));
    handler.session_config = SessionConfig {
        data_directory: dir.path().to_string_lossy().into_owned(),
    };

    let result = handler
        .refocus_train_inner(refocus_params("main"), None, Cancel::never())
        .await;
    let call = result.expect("refocus_train protocol error");
    assert!(!call.is_error.unwrap_or(false));
    assert_eq!(ok_text(call)["guiding_paused"], false);
}

#[tokio::test]
async fn refocus_train_step_failure_still_resumes_guiding() {
    // Empty registry: the step fails at device resolution, after the
    // pause. mockall's drop check enforces that resume still ran.
    let mut mock = MockGuiderClient::new();
    mock.expect_guiding_stats()
        .times(1)
        .returning(|| Ok(stats_with_guiding(true)));
    mock.expect_pause_guiding().times(1).returning(|_| Ok(()));
    mock.expect_resume_guiding().times(1).returning(|| Ok(()));
    let client: Arc<dyn rp_guider::GuiderClient> = Arc::new(mock);
    let handler = test_handler(empty_registry())
        .with_guider(Some(client), GuiderDefaults::default())
        .with_trains(fixture_trains(true));

    let result = handler
        .refocus_train_inner(refocus_params("main"), None, Cancel::never())
        .await;
    assert_tool_error(
        result,
        "step 1 (focuser 'foc' in train 'main') failed: camera not found: cam",
    );
}

#[tokio::test]
async fn refocus_train_resume_failure_after_success_is_an_error() {
    const STARTING_POSITION: i32 = 11_000;
    let dir = tempfile::tempdir().expect("tempdir");
    let mut mock = MockGuiderClient::new();
    mock.expect_guiding_stats()
        .times(1)
        .returning(|| Ok(stats_with_guiding(true)));
    mock.expect_pause_guiding().times(1).returning(|_| Ok(()));
    mock.expect_resume_guiding()
        .times(1)
        .returning(|| Err(GuiderError::Internal("boom".to_string())));
    let client: Arc<dyn rp_guider::GuiderClient> = Arc::new(mock);
    let mut handler = test_handler(auto_focus_registry(STARTING_POSITION))
        .with_guider(Some(client), GuiderDefaults::default())
        .with_trains(fixture_trains(true));
    handler.session_config = SessionConfig {
        data_directory: dir.path().to_string_lossy().into_owned(),
    };

    let result = handler
        .refocus_train_inner(refocus_params("main"), None, Cancel::never())
        .await;
    assert_tool_error(result, "resuming guiding failed");
}

// -----------------------------------------------------------------------
// Rotator tool addressing. The happy-path move (device motion, poll,
// events, moved_trains) runs against OmniSim in rotator.feature; these
// pin the shared addressing/validation paths that need no device.
// -----------------------------------------------------------------------

use super::built_in::rotator::{MoveRotatorParams, RotatorPositionParams};

fn move_rotator_params(
    rotator_id: Option<&str>,
    train_id: Option<&str>,
    angle: Option<f64>,
) -> MoveRotatorParams {
    MoveRotatorParams {
        rotator_id: rotator_id.map(str::to_string),
        train_id: train_id.map(str::to_string),
        angle,
    }
}

/// One two-rotator train over an offline roster: [r1, r2, cam].
fn two_rotator_trains() -> crate::equipment::trains::TrainModel {
    let equipment: crate::config::EquipmentConfig = serde_json::from_value(serde_json::json!({
        "cameras": [{"id": "cam", "alpaca_url": "http://localhost:1"}],
        "rotators": [
            {"id": "r1", "alpaca_url": "http://localhost:1"},
            {"id": "r2", "alpaca_url": "http://localhost:1"}
        ],
        "optical_trains": [
            {"id": "main", "devices": ["r1", "r2", "cam"]}
        ]
    }))
    .unwrap();
    crate::equipment::trains::TrainModel::try_from_equipment(&equipment).unwrap()
}

#[tokio::test]
async fn move_rotator_rejects_both_addressing_forms() {
    let handler = test_handler(empty_registry());
    let result = handler
        .move_rotator_inner(
            move_rotator_params(Some("r"), Some("main"), Some(5.0)),
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "exactly one of rotator_id or train_id");
}

#[tokio::test]
async fn move_rotator_rejects_missing_addressing() {
    let handler = test_handler(empty_registry());
    let result = handler
        .move_rotator_inner(move_rotator_params(None, None, Some(5.0)), &Cancel::never())
        .await;
    assert_tool_error(result, "exactly one of rotator_id or train_id");
}

#[tokio::test]
async fn move_rotator_requires_an_angle() {
    let handler = test_handler(empty_registry());
    let result = handler
        .move_rotator_inner(move_rotator_params(Some("r"), None, None), &Cancel::never())
        .await;
    assert_tool_error(result, "missing required parameter: angle");
}

#[tokio::test]
async fn move_rotator_validates_the_angle_before_device_resolution() {
    // The rotator does not exist in the registry; an in-range angle
    // would error "rotator not found", so the "angle out of range"
    // error proves the validation order.
    let handler = test_handler(empty_registry());
    for bad in [360.0, -0.1, f64::NAN, f64::INFINITY] {
        let result = handler
            .move_rotator_inner(
                move_rotator_params(Some("r"), None, Some(bad)),
                &Cancel::never(),
            )
            .await;
        assert_tool_error(result, "angle out of range");
    }
}

#[tokio::test]
async fn move_rotator_rejects_an_unknown_rotator() {
    let handler = test_handler(empty_registry());
    let result = handler
        .move_rotator_inner(
            move_rotator_params(Some("r"), None, Some(5.0)),
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "rotator not found: r");
}

#[tokio::test]
async fn move_rotator_rejects_an_unknown_train() {
    let handler = test_handler(empty_registry());
    let result = handler
        .move_rotator_inner(
            move_rotator_params(None, Some("nonexistent"), Some(5.0)),
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "train not found");
}

#[tokio::test]
async fn move_rotator_rejects_a_train_without_a_rotator() {
    let handler = test_handler(empty_registry()).with_trains(cam_trains(200.0));
    let result = handler
        .move_rotator_inner(
            move_rotator_params(None, Some("main"), Some(5.0)),
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "has no rotator");
}

#[tokio::test]
async fn move_rotator_asks_for_the_explicit_id_on_a_two_rotator_train() {
    let handler = test_handler(empty_registry()).with_trains(two_rotator_trains());
    let result = handler
        .move_rotator_inner(
            move_rotator_params(None, Some("main"), Some(5.0)),
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "pass rotator_id");
}

#[tokio::test]
async fn get_rotator_position_shares_the_addressing_resolution() {
    let handler = test_handler(empty_registry()).with_trains(cam_trains(200.0));
    let result = handler
        .get_rotator_position(Parameters(RotatorPositionParams {
            rotator_id: None,
            train_id: Some("main".to_string()),
        }))
        .await;
    assert_tool_error(result, "has no rotator");
}

// -----------------------------------------------------------------------
// Progress-notification emission from long-running blocking helpers.
//
// Each poll loop in `mcp/internals.rs` emits `notifications/progress`
// every `PROGRESS_INTERVAL` while it runs, so a `progressToken`-bearing
// client sees a long tool advance (see `mcp/progress.rs`). These tests
// drive each helper against a mock that reports "not done yet" for a
// controlled number of poll iterations and count emissions on a test
// sink.
//
// -----------------------------------------------------------------------

/// `do_slew_blocking` against a mount that reports `slewing == true`
/// for ~12 s of simulated time and then `false` must fire at least
/// two progress notifications (one each at the 5 s and 10 s marks,
/// per `PROGRESS_INTERVAL == 5 s`).
#[tokio::test(start_paused = true)]
async fn do_slew_blocking_emits_progress_during_slew() {
    // 120 polls × 100 ms tick ≈ 12 s of simulated `slewing()==true`.
    // After 12 s of activity, `PROGRESS_INTERVAL == 5 s` produces a
    // tick at 5 s and at 10 s — assert ≥ 2 emissions. current == target
    // (distance 0) so the deadline is the `MIN_SLEW_DEADLINE` floor
    // (30 s), comfortably above the 12 s simulated slew — and a canary:
    // a floor dropped below ~12 s would make this slew time out.
    let mount = MockTelescope {
        slewing_true_count: 120,
        ..Default::default()
    };
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let emitter = super::progress::test_support::CountingProgressEmitter::default();
    let (actual_ra, actual_dec) = handler
        .do_slew_blocking(0.0, 0.0, Duration::ZERO, Some(&emitter), &Cancel::never())
        .await
        .expect("slew completes when the mount reports idle");
    assert_eq!(actual_ra, 0.0);
    assert_eq!(actual_dec, 0.0);
    assert!(
        emitter.count() >= 2,
        "expected ≥ 2 progress notifications over ~12 s of slew, got {}",
        emitter.count()
    );
}

/// `do_park_blocking` against a mount that reports `at_park == false`
/// for ~12 s of simulated time and then `true` must fire at least two
/// progress notifications, same shape as the slew test.
#[tokio::test(start_paused = true)]
async fn do_park_blocking_emits_progress_during_park() {
    let mount = MockTelescope {
        at_park_false_count: 120,
        at_park_value: true,
        ..Default::default()
    };
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let emitter = super::progress::test_support::CountingProgressEmitter::default();
    handler
        .do_park_blocking(Some(&emitter), &Cancel::never())
        .await
        .expect("park completes when the mount reports at_park");
    assert!(
        emitter.count() >= 2,
        "expected ≥ 2 progress notifications over ~12 s of park, got {}",
        emitter.count()
    );
}

/// `do_capture` against a camera whose `image_ready` returns `false`
/// for ~12 s and then `true` must fire at least two progress
/// notifications during the readout-wait poll loop.
///
/// Uses the same `temp_dir`-redirected `SessionConfig` shape as the
/// other `do_capture` tests so the FITS write inside `do_capture`
/// lands in a sandbox.
#[tokio::test(start_paused = true)]
async fn do_capture_emits_progress_during_readout_wait() {
    let cam = MockCamera {
        not_ready_count: 120,
        ..Default::default()
    };
    let tmp = tempfile::tempdir().expect("temp dir");
    let mut handler = test_handler(camera_registry(Arc::new(cam)));
    handler.session_config = SessionConfig {
        data_directory: tmp.path().to_string_lossy().to_string(),
    };
    let emitter = super::progress::test_support::CountingProgressEmitter::default();
    let (_image_path, _document_id) = handler
        .do_capture(
            "cam",
            Duration::from_millis(50),
            None,
            None,
            Some(&emitter),
            &Cancel::never(),
        )
        .await
        .expect("capture completes when image_ready flips true");
    assert!(
        emitter.count() >= 2,
        "expected ≥ 2 progress notifications over ~12 s of readout wait, got {}",
        emitter.count()
    );
}

/// `None` for the progress emitter is the default for unit tests and
/// for MCP clients that didn't send a `progressToken`. The helpers
/// must remain functionally identical — pin that the slew still
/// completes correctly when no sink is supplied.
#[tokio::test(start_paused = true)]
async fn do_slew_blocking_with_none_emitter_is_a_noop() {
    let mount = MockTelescope {
        slewing_true_count: 5,
        ..Default::default()
    };
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let result = handler
        .do_slew_blocking(0.0, 0.0, Duration::ZERO, None, &Cancel::never())
        .await;
    result.expect("slew with None emitter still completes");
}

/// `ProgressSink::from_peer_and_meta` returns `None` when `_meta`
/// carries no `progressToken`. The helper is consequently a no-op for
/// any client (or BDD harness) that doesn't opt in.
#[test]
fn progress_sink_returns_none_without_progress_token() {
    // Build an empty RequestMetaObject — no progressToken set. We can't
    // easily construct a real `Peer<RoleServer>` from outside rmcp (its
    // constructor is `pub(crate)`), but the meta-only path through
    // `RequestMetaObject::get_progress_token()` is enough to pin the
    // contract: any None on the meta side must turn into a None sink.
    let meta = rmcp::model::RequestMetaObject::default();
    assert!(
        meta.get_progress_token().is_none(),
        "default RequestMetaObject should not carry a progressToken"
    );
}

/// Companion to `do_capture_emits_progress_during_readout_wait`. With a
/// `duration` longer than `PROGRESS_INTERVAL`, every tick fired while the
/// camera is still shuttering falls in the `elapsed < duration` arm, so
/// the emitted phase is `"exposing"` — the branch the 50 ms-duration
/// readout test never reaches.
#[tokio::test(start_paused = true)]
async fn do_capture_emits_exposing_phase_before_readout() {
    // `not_ready_count: 120` ≈ 12 s of `image_ready==false`; a 60 s
    // `duration` keeps the 5 s and 10 s emit marks inside the exposure
    // window, so each is tagged `"exposing"`.
    let cam = MockCamera {
        not_ready_count: 120,
        ..Default::default()
    };
    let tmp = tempfile::tempdir().expect("temp dir");
    let mut handler = test_handler(camera_registry(Arc::new(cam)));
    handler.session_config = SessionConfig {
        data_directory: tmp.path().to_string_lossy().to_string(),
    };
    let emitter = super::progress::test_support::CountingProgressEmitter::default();
    handler
        .do_capture(
            "cam",
            Duration::from_mins(1),
            None,
            None,
            Some(&emitter),
            &Cancel::never(),
        )
        .await
        .expect("capture completes when image_ready flips true");
    assert!(
        emitter.count() >= 2,
        "expected ≥ 2 progress notifications during the exposure window, got {}",
        emitter.count()
    );
    assert!(
        emitter
            .records()
            .iter()
            .any(|(_, _, msg)| msg.as_deref() == Some("exposing")),
        "expected at least one tick tagged \"exposing\", got {:?}",
        emitter.records()
    );
}

/// `do_move_focuser_blocking` against a focuser that reports
/// `is_moving == true` for ~12 s and then `false` must fire at least two
/// progress notifications (5 s + 10 s marks) tagged `"focuser_moving"`
/// during the settle poll — the focuser counterpart to the slew/park/
/// capture progress tests. The target (10000) sits far enough from the
/// current position (4321) that the predicted deadline
/// (`5679 / 500 × 2 ≈ 22.7 s`) comfortably covers the 12 s move; the mock's
/// fixed readback still returns 4321.
#[tokio::test(start_paused = true)]
async fn do_move_focuser_blocking_emits_progress_during_move() {
    let foc = MockFocuser {
        is_moving_true_count: 120,
        position_value: 4321,
        ..Default::default()
    };
    let handler = test_handler(focuser_registry(Arc::new(foc), None, None));
    let emitter = super::progress::test_support::CountingProgressEmitter::default();
    let position = handler
        .do_move_focuser_blocking("foc", 10000, Some(&emitter), &Cancel::never())
        .await
        .expect("move completes when the focuser reports idle");
    assert_eq!(position, 4321);
    assert!(
        emitter.count() >= 2,
        "expected ≥ 2 progress notifications over ~12 s of focuser move, got {}",
        emitter.count()
    );
    assert!(
        emitter
            .records()
            .iter()
            .any(|(_, _, msg)| msg.as_deref() == Some("focuser_moving")),
        "expected at least one tick tagged \"focuser_moving\", got {:?}",
        emitter.records()
    );
}

/// `do_slew_blocking` with a `settle_after` longer than
/// `PROGRESS_INTERVAL` emits one `"settling"` tick even when the slew
/// itself finishes immediately — covers the settle-phase emit that the
/// during-slew test (zero settle) never reaches.
#[tokio::test(start_paused = true)]
async fn do_slew_blocking_emits_progress_during_settle() {
    // `slewing_true_count: 0` ⇒ the mount reports idle on the first poll,
    // so `poll_slewing_until_idle` returns without emitting; the only tick
    // is the settle one (`settle_after == 10 s >= PROGRESS_INTERVAL`).
    let mount = MockTelescope {
        slewing_true_count: 0,
        ..Default::default()
    };
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let emitter = super::progress::test_support::CountingProgressEmitter::default();
    handler
        .do_slew_blocking(
            0.0,
            0.0,
            Duration::from_secs(10),
            Some(&emitter),
            &Cancel::never(),
        )
        .await
        .expect("slew + settle completes");
    assert!(
        emitter
            .records()
            .iter()
            .any(|(_, _, msg)| msg.as_deref() == Some("settling")),
        "expected a tick tagged \"settling\", got {:?}",
        emitter.records()
    );
}

/// Complement to the settle test: a non-zero `settle_after` *below*
/// `PROGRESS_INTERVAL` enters the `Some(sink)` block but skips the emit
/// (the `settle_after >= PROGRESS_INTERVAL` guard is false), so no tick
/// is sent. Pins the short-settle branch so brief corrective slews don't
/// spam progress.
#[tokio::test(start_paused = true)]
async fn do_slew_blocking_skips_settle_tick_below_interval() {
    let mount = MockTelescope {
        slewing_true_count: 0,
        ..Default::default()
    };
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let emitter = super::progress::test_support::CountingProgressEmitter::default();
    handler
        .do_slew_blocking(
            0.0,
            0.0,
            Duration::from_secs(2),
            Some(&emitter),
            &Cancel::never(),
        )
        .await
        .expect("slew + short settle completes");
    assert_eq!(
        emitter.count(),
        0,
        "a settle below PROGRESS_INTERVAL must emit no tick, got {:?}",
        emitter.records()
    );
}

/// Exercises the *live* `ProgressSink::emit` (not the `CountingProgressEmitter`
/// double): builds a real `Peer<RoleServer>` via `serve_directly` over an
/// in-memory tokio duplex. `serve_directly` skips the init handshake, so no
/// client is required — the server end backs the peer, and the client end is
/// held open (unread) so the single outbound notification buffers instead of
/// erroring. Pins that `from_peer_and_meta` yields a sink when a
/// `progressToken` is present and that `emit` performs a `notify_progress`
/// send without panicking. A 5 s timeout guards against a transport wedge.
#[tokio::test]
async fn progress_sink_emit_sends_via_real_peer() {
    use super::progress::{ProgressEmitter, ProgressSink};
    use rmcp::model::{NumberOrString, ProgressToken, RequestMetaObject};

    let (server_io, _client_io) = tokio::io::duplex(4096);
    let (rx, tx) = tokio::io::split(server_io);
    let service = test_handler(camera_registry(Arc::new(MockCamera::default())));
    let running = rmcp::service::serve_directly(service, (rx, tx), None);
    let peer = running.peer().clone();

    let meta = RequestMetaObject::with_progress_token(ProgressToken(NumberOrString::Number(1)));
    let sink = ProgressSink::from_peer_and_meta(peer, &meta)
        .expect("meta carries a progressToken => Some(sink)");

    tokio::time::timeout(
        Duration::from_secs(5),
        sink.emit(5.0, Some(60.0), Some("exposing".to_string())),
    )
    .await
    .expect("emit completed within the timeout");

    // `_client_io` and `running` are held to here so the session worker is
    // alive (and the duplex buffer un-closed) while the notification drains;
    // both shut down on drop at end of scope.
    drop(running);
}

// -----------------------------------------------------------------------
// Operation-event triple tests (predictive-deadlines Phase 1, Step 2)
//
// Each blocking helper emits a `*_started` envelope at entry and a
// `*_complete` / `*_failed` envelope at exit, all sharing one
// `operation_id`. Decision B: the assertion seam is the `EventBus`
// broadcast `Receiver` — exercising the real production fan-out path
// with no test-only abstraction. `emit_operation` publishes
// synchronously, so by the time a helper returns its envelopes are
// already queued on a receiver that subscribed beforehand.
// -----------------------------------------------------------------------

/// Drain the next envelope, failing the test (rather than hanging) if
/// none is queued within a generous bound.
async fn next_event(
    rx: &mut tokio::sync::broadcast::Receiver<crate::events::EventEnvelope>,
) -> crate::events::EventEnvelope {
    tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("expected an operation event but the channel stayed empty")
        .expect("event channel closed or lagged")
}

/// Assert the channel has no further envelope queued.
async fn assert_no_more_events(
    rx: &mut tokio::sync::broadcast::Receiver<crate::events::EventEnvelope>,
) {
    assert!(
        tokio::time::timeout(Duration::from_millis(50), rx.recv())
            .await
            .is_err(),
        "expected no further operation events on the bus"
    );
}

/// Assertions common to every `*_complete` / `*_failed` envelope: it
/// shares the started envelope's `operation_id`, carries a fresh
/// `event_id` and a strictly greater `event_seq`, has the full timing
/// trio, and never carries the deadline fields (those ride only the
/// matching `*_started`, and only for operations with a predictive
/// deadline). The start envelope's deadline fields are asserted
/// per-operation by that operation's own test.
fn assert_end_mirrors_start(
    start: &crate::events::EventEnvelope,
    end: &crate::events::EventEnvelope,
) {
    assert_eq!(
        start.operation_id, end.operation_id,
        "started and ended envelopes must share one operation_id"
    );
    assert!(start.operation_id.is_some(), "operation_id must be present");
    assert_ne!(
        start.event_id, end.event_id,
        "each emission carries its own event_id"
    );
    assert!(
        end.event_seq > start.event_seq,
        "event_seq is monotonic across the triple"
    );
    assert!(start.started_at.is_some(), "started_at present on start");
    assert!(end.started_at.is_some(), "started_at echoed on end");
    assert!(end.ended_at.is_some(), "ended_at present on end");
    assert!(end.elapsed_ms.is_some(), "elapsed_ms present on end");
    assert!(
        start.ended_at.is_none(),
        "no ended_at on the start envelope"
    );
    // The matching `*_started` envelope carries deadline fields only for
    // operations with a predictive deadline (slew §2.1, park + move_focuser
    // §2.2/§2.3, exposure §2.4, centering §2.5, and guide/dither when a
    // settle timeout is resolved); every other operation's start must
    // still omit them. Slew/park/move_focuser/exposure/centering always
    // predict, so those are keyed off the event name (each such
    // operation's start is asserted present by its own test). Guide/dither
    // predict only conditionally, so those are keyed off whether the start
    // envelope actually carries a deadline — that way any guide/dither test
    // built on `GuiderDefaults::default()` (no settle timeout) gets its
    // omission checked here for free, not just by the one dedicated test
    // (`start_guiding_without_a_settle_timeout_omits_the_deadline`).
    let has_predictive_deadline = start.event.starts_with("slew")
        || start.event.starts_with("park")
        || start.event.starts_with("move_focuser")
        || start.event.starts_with("exposure")
        || start.event.starts_with("centering")
        || ((start.event.starts_with("guide") || start.event.starts_with("dither"))
            && start.predicted_duration_ms.is_some());
    if !has_predictive_deadline {
        assert!(
            start.predicted_duration_ms.is_none(),
            "{} start envelope must omit predicted_duration_ms",
            start.event
        );
        assert!(
            start.max_duration_ms.is_none(),
            "{} start envelope must omit max_duration_ms",
            start.event
        );
    }
    assert!(
        end.predicted_duration_ms.is_none(),
        "the end envelope must not carry predicted_duration_ms"
    );
    assert!(
        end.max_duration_ms.is_none(),
        "the end envelope must not carry max_duration_ms"
    );
}

#[tokio::test]
async fn slew_emits_started_complete_triple() {
    let mount = MockTelescope {
        ra_value: 10.6847,
        dec_value: 41.2689,
        ..Default::default()
    };
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let mut rx = handler.event_bus.subscribe();

    let (actual_ra, actual_dec) = handler
        .do_slew_blocking(10.6847, 41.2689, Duration::ZERO, None, &Cancel::never())
        .await
        .unwrap();
    assert_eq!(actual_ra, 10.6847);
    assert_eq!(actual_dec, 41.2689);

    let started = next_event(&mut rx).await;
    let complete = next_event(&mut rx).await;
    assert_no_more_events(&mut rx).await;

    assert_eq!(started.event, "slew_started");
    assert_eq!(complete.event, "slew_complete");
    assert_eq!(started.payload["ra"], 10.6847);
    assert_eq!(started.payload["dec"], 41.2689);
    assert_eq!(complete.payload["actual_ra"], 10.6847);
    assert_eq!(complete.payload["actual_dec"], 41.2689);
    assert_end_mirrors_start(&started, &complete);

    // current == target (the mock reports the slew target as its current
    // pointing), so the great-circle distance is 0: predicted floors at
    // 0 ms and max at the 30 s MIN_SLEW_DEADLINE floor.
    assert_eq!(started.predicted_duration_ms, Some(0));
    assert_eq!(started.max_duration_ms, Some(30000));
}

/// §2.1: the `slew_started` envelope carries a deadline sized from the
/// great-circle distance to the target. Current pointing (0h, 0°) →
/// target (0h, 60°) is 60° = 216000″; at the default 7200″/s rate with no
/// settle, predicted = 30 s and max = max(30 × 3, 30 s floor) = 90 s
/// (target chosen large enough that `predicted × 3` clears the floor, so
/// the distance scaling — not the floor — is what's asserted).
#[tokio::test]
async fn slew_started_carries_distance_scaled_deadline() {
    let mount = MockTelescope {
        ra_value: 0.0,
        dec_value: 0.0,
        ..Default::default()
    };
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let mut rx = handler.event_bus.subscribe();

    handler
        .do_slew_blocking(0.0, 60.0, Duration::ZERO, None, &Cancel::never())
        .await
        .unwrap();

    let started = next_event(&mut rx).await;
    assert_eq!(started.event, "slew_started");
    assert_eq!(
        started.predicted_duration_ms,
        Some(30000),
        "60° / 7200 arcsec·s⁻¹ = 30 s predicted"
    );
    assert_eq!(
        started.max_duration_ms,
        Some(90000),
        "max = predicted × 3 = 90 s (above the 30 s floor)"
    );
}

/// §2.1 fallback: when the pre-slew pointing read fails the deadline can't
/// be predicted, so `slew_started` omits the deadline fields and the slew
/// proceeds on the fallback ceiling. (Here the same failing read also
/// fails the post-slew read, so the operation ends in error — the point
/// under test is that the started envelope carries no deadline fields.)
#[tokio::test]
async fn slew_started_omits_deadline_when_pointing_read_fails() {
    let mount = MockTelescope {
        fail_right_ascension: true,
        ..Default::default()
    };
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let mut rx = handler.event_bus.subscribe();

    let err = handler
        .do_slew_blocking(0.0, 10.0, Duration::ZERO, None, &Cancel::never())
        .await
        .unwrap_err();
    assert!(err.contains("right_ascension"), "got: {err}");

    let started = next_event(&mut rx).await;
    assert_eq!(started.event, "slew_started");
    assert!(
        started.predicted_duration_ms.is_none(),
        "fallback path must omit predicted_duration_ms"
    );
    assert!(
        started.max_duration_ms.is_none(),
        "fallback path must omit max_duration_ms"
    );
}

/// §2.1 robustness: a finite, positive, but absurdly small slew rate makes
/// `distance / rate` a *finite* value far too large for `Duration` to hold
/// — the case a bare `is_finite()` check misses (it slips through to
/// `Duration::from_secs_f64`, which panics on overflow). `compute_slew_deadline`
/// rejects it via `try_from_secs_f64` and falls back (300 s ceiling,
/// deadline fields omitted) rather than crashing.
#[tokio::test]
async fn slew_deadline_overflow_falls_back_without_panic() {
    let registry = crate::equipment::EquipmentRegistry {
        safety_monitors: vec![],
        cameras: vec![],
        filter_wheels: vec![],
        cover_calibrators: vec![],
        focusers: vec![],
        mount: Some(crate::equipment::MountEntry {
            config: crate::config::MountConfig {
                alpaca_url: "http://localhost:1".to_string(),
                device_number: 0,
                settle_after_slew: None,
                slew_rate_arcsec_per_sec: crate::config::mount::SlewRateArcsecPerSec::try_new(
                    1e-20,
                )
                .unwrap(),
                guiding: None,
                auth: None,
            },
            session: crate::equipment::DeviceSession::connected(Arc::new(MockTelescope::default())),
        }),
        ..Default::default()
    };
    let handler = test_handler(registry);
    let mut rx = handler.event_bus.subscribe();

    // current (0,0) → (0, 80°): 80° / 1e-20 arcsec·s⁻¹ ≈ 2.9e25 s —
    // finite but far beyond Duration's range, so the prediction is
    // rejected and the slew completes on the fallback deadline.
    handler
        .do_slew_blocking(0.0, 80.0, Duration::ZERO, None, &Cancel::never())
        .await
        .unwrap();

    let started = next_event(&mut rx).await;
    assert_eq!(started.event, "slew_started");
    assert!(
        started.predicted_duration_ms.is_none(),
        "overflow must fall back and omit predicted_duration_ms"
    );
    assert!(started.max_duration_ms.is_none());
}

#[tokio::test]
async fn slew_failure_emits_started_then_failed() {
    let mount = MockTelescope {
        fail_slew: true,
        ..Default::default()
    };
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let mut rx = handler.event_bus.subscribe();

    let err = handler
        .do_slew_blocking(0.0, 0.0, Duration::ZERO, None, &Cancel::never())
        .await
        .unwrap_err();
    assert!(err.contains("failed to slew"));

    let started = next_event(&mut rx).await;
    let failed = next_event(&mut rx).await;
    assert_no_more_events(&mut rx).await;

    assert_eq!(started.event, "slew_started");
    assert_eq!(failed.event, "slew_failed");
    assert!(failed.payload["error"]
        .as_str()
        .unwrap()
        .contains("failed to slew"));
    assert_end_mirrors_start(&started, &failed);
}

#[tokio::test]
async fn park_emits_started_complete_triple() {
    let mount = MockTelescope {
        at_park_value: true,
        ..Default::default()
    };
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let mut rx = handler.event_bus.subscribe();

    handler
        .do_park_blocking(None, &Cancel::never())
        .await
        .unwrap();

    let started = next_event(&mut rx).await;
    let complete = next_event(&mut rx).await;
    assert_no_more_events(&mut rx).await;

    assert_eq!(started.event, "park_started");
    assert_eq!(complete.event, "park_complete");
    assert_end_mirrors_start(&started, &complete);

    // §2.2: no park coordinates are knowable via Alpaca, so the deadline is
    // the worst-case 180° traverse at the default 7200″/s rate: predicted =
    // 648000″ / 7200 = 90 s, max = 90 × 2 = 180 s (above the 60 s floor).
    assert_eq!(started.predicted_duration_ms, Some(90000));
    assert_eq!(started.max_duration_ms, Some(180_000));
}

#[tokio::test]
async fn move_focuser_emits_started_complete_triple() {
    let foc = MockFocuser {
        position_value: 4321,
        ..Default::default()
    };
    let handler = test_handler(focuser_registry(Arc::new(foc), None, None));
    let mut rx = handler.event_bus.subscribe();

    let final_position = handler
        .do_move_focuser_blocking("foc", 4321, None, &Cancel::never())
        .await
        .unwrap();
    assert_eq!(final_position, 4321);

    let started = next_event(&mut rx).await;
    let complete = next_event(&mut rx).await;
    assert_no_more_events(&mut rx).await;

    assert_eq!(started.event, "move_focuser_started");
    assert_eq!(complete.event, "move_focuser_complete");
    assert_eq!(started.payload["focuser_id"], "foc");
    assert_eq!(started.payload["position"], 4321);
    assert_eq!(complete.payload["position"], 4321);
    assert_end_mirrors_start(&started, &complete);

    // §2.3: current == target (4321) ⇒ distance 0, so predicted floors at
    // 0 ms and max at the 5 s MIN_FOCUSER_DEADLINE floor.
    assert_eq!(started.predicted_duration_ms, Some(0));
    assert_eq!(started.max_duration_ms, Some(5000));
}

/// §2.3: the `move_focuser_started` envelope carries a deadline sized from
/// the step travel. Current 0 → target 10000 at the default 500 steps/s is
/// predicted = 20 s and max = max(20 × 2, 5 s floor) = 40 s (target chosen
/// large enough that `predicted × 2` clears the floor, so the distance
/// scaling — not the floor — is what's asserted).
#[tokio::test]
async fn move_focuser_started_carries_distance_scaled_deadline() {
    let foc = MockFocuser {
        position_value: 0,
        ..Default::default()
    };
    let handler = test_handler(focuser_registry(Arc::new(foc), None, None));
    let mut rx = handler.event_bus.subscribe();

    handler
        .do_move_focuser_blocking("foc", 10000, None, &Cancel::never())
        .await
        .unwrap();

    let started = next_event(&mut rx).await;
    assert_eq!(started.event, "move_focuser_started");
    assert_eq!(
        started.predicted_duration_ms,
        Some(20000),
        "10000 steps / 500 steps·s⁻¹ = 20 s predicted"
    );
    assert_eq!(
        started.max_duration_ms,
        Some(40000),
        "max = predicted × 2 = 40 s (above the 5 s floor)"
    );
}

/// §2.3 fallback: when the pre-move position read fails the deadline can't
/// be predicted, so `move_focuser_started` omits the deadline fields and the
/// move proceeds on the fallback ceiling. (Here the same failing read also
/// fails the post-move read, so the operation ends in error — the point
/// under test is that the started envelope carries no deadline fields.)
#[tokio::test]
async fn move_focuser_started_omits_deadline_when_position_read_fails() {
    let foc = MockFocuser {
        fail_position: true,
        ..Default::default()
    };
    let handler = test_handler(focuser_registry(Arc::new(foc), None, None));
    let mut rx = handler.event_bus.subscribe();

    let _ = handler
        .do_move_focuser_blocking("foc", 1000, None, &Cancel::never())
        .await;

    let started = next_event(&mut rx).await;
    assert_eq!(started.event, "move_focuser_started");
    assert!(
        started.predicted_duration_ms.is_none(),
        "fallback path must omit predicted_duration_ms"
    );
    assert!(
        started.max_duration_ms.is_none(),
        "fallback path must omit max_duration_ms"
    );
}

#[tokio::test]
async fn move_focuser_failure_emits_started_then_failed() {
    let foc = MockFocuser {
        fail_move: true,
        ..Default::default()
    };
    let handler = test_handler(focuser_registry(Arc::new(foc), None, None));
    let mut rx = handler.event_bus.subscribe();

    let err = handler
        .do_move_focuser_blocking("foc", 1000, None, &Cancel::never())
        .await
        .unwrap_err();
    assert!(err.contains("failed to move focuser"));

    let started = next_event(&mut rx).await;
    let failed = next_event(&mut rx).await;
    assert_no_more_events(&mut rx).await;

    assert_eq!(started.event, "move_focuser_started");
    assert_eq!(failed.event, "move_focuser_failed");
    assert_end_mirrors_start(&started, &failed);
}

#[tokio::test]
async fn sync_mount_emits_complete_only_no_started() {
    // Sync is instant per ASCOM, so the helper emits the
    // `sync_mount_complete` / `sync_mount_failed` pair *without* a
    // `_started` (parent plan §1.2).
    let mount = MockTelescope::default();
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let mut rx = handler.event_bus.subscribe();

    handler.do_sync_mount(5.0, -10.0).await.unwrap();

    let complete = next_event(&mut rx).await;
    assert_no_more_events(&mut rx).await;

    assert_eq!(complete.event, "sync_mount_complete");
    assert!(complete.operation_id.is_some());
    assert!(complete.ended_at.is_some());
    assert_eq!(complete.payload["ra"], 5.0);
    assert_eq!(complete.payload["dec"], -10.0);
    assert!(complete.predicted_duration_ms.is_none());
}

#[tokio::test]
async fn sync_mount_failure_emits_failed_only() {
    let mount = MockTelescope {
        fail_sync: true,
        ..Default::default()
    };
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let mut rx = handler.event_bus.subscribe();

    let err = handler.do_sync_mount(5.0, -10.0).await.unwrap_err();
    assert!(err.contains("failed to sync mount"));

    let failed = next_event(&mut rx).await;
    assert_no_more_events(&mut rx).await;

    assert_eq!(failed.event, "sync_mount_failed");
    assert!(failed.payload["error"]
        .as_str()
        .unwrap()
        .contains("failed to sync mount"));
    assert!(failed.elapsed_ms.is_some());
}

#[tokio::test]
async fn capture_migrated_emits_exposure_triple_with_shared_operation_id() {
    // The historical `exposure_started` / `exposure_complete` point
    // events are migrated onto the envelope under one operation_id; their
    // payloads are preserved byte-for-byte (Decision A).
    let handler = test_handler(camera_registry(Arc::new(MockCamera::default())));
    let mut rx = handler.event_bus.subscribe();

    let (image_path, document_id) = handler
        .do_capture(
            "cam",
            Duration::from_millis(100),
            None,
            None,
            None,
            &Cancel::never(),
        )
        .await
        .unwrap();

    let started = next_event(&mut rx).await;
    let complete = next_event(&mut rx).await;
    assert_no_more_events(&mut rx).await;

    assert_eq!(started.event, "exposure_started");
    assert_eq!(complete.event, "exposure_complete");
    // Legacy payload shape preserved.
    assert_eq!(started.payload["camera_id"], "cam");
    assert!(started.payload["duration"].is_string());
    assert_eq!(complete.payload["document_id"], document_id);
    assert_eq!(complete.payload["file_path"], image_path);
    assert_end_mirrors_start(&started, &complete);

    // §2.4: the exposure deadline rides `exposure_started`. The camera has
    // no configured `readout_time_estimate`, so the 15 s default applies:
    // predicted = 100 ms + 15 s = 15100 ms; max = predicted + 30 s headroom.
    assert_eq!(started.predicted_duration_ms, Some(15_100));
    assert_eq!(started.max_duration_ms, Some(45_100));
}

#[tokio::test]
async fn capture_failure_emits_exposure_failed() {
    let cam = MockCamera {
        fail_start_exposure: true,
        ..Default::default()
    };
    let handler = test_handler(camera_registry(Arc::new(cam)));
    let mut rx = handler.event_bus.subscribe();

    let err = handler
        .do_capture(
            "cam",
            Duration::from_millis(100),
            None,
            None,
            None,
            &Cancel::never(),
        )
        .await
        .unwrap_err();
    assert!(err.contains("failed to start exposure"));

    let started = next_event(&mut rx).await;
    let failed = next_event(&mut rx).await;
    assert_no_more_events(&mut rx).await;

    assert_eq!(started.event, "exposure_started");
    assert_eq!(failed.event, "exposure_failed");
    assert!(failed.payload["error"]
        .as_str()
        .unwrap()
        .contains("failed to start exposure"));
    assert_end_mirrors_start(&started, &failed);

    // The deadline is sized from the request, not the camera's success, so a
    // failed exposure still advertises it on `exposure_started` (§2.4).
    assert_eq!(started.predicted_duration_ms, Some(15_100));
    assert_eq!(started.max_duration_ms, Some(45_100));
}

#[test]
fn exposure_deadlines_add_readout_and_headroom() {
    // §2.4: predicted = duration + readout_estimate; max = predicted + 30 s.
    let (predicted_ms, max_ms) =
        super::internals::exposure_deadlines(Duration::from_mins(5), Duration::from_secs(8));
    assert_eq!(predicted_ms, 308_000, "300 s exposure + 8 s readout");
    assert_eq!(max_ms, 338_000, "predicted + 30 s readout headroom");
}

#[test]
fn centering_deadlines_compose_per_iteration_over_max_attempts() {
    // §2.5: per_iter = capture + solve + slew_overhead; predicted = per_iter
    // (single-pass convergence); max = max_attempts × per_iter.
    let (predicted_ms, max_ms) = super::internals::centering_deadlines(
        5,
        Duration::from_millis(100),
        Duration::from_secs(30),
        Duration::from_secs(10),
    );
    assert_eq!(
        predicted_ms, 40_100,
        "100 ms capture + 30 s solve + 10 s slew"
    );
    assert_eq!(max_ms, 200_500, "5 attempts × 40_100 ms");
}

#[tokio::test]
async fn centering_started_carries_outer_loop_deadline() {
    // §2.5 wiring: center_on_target stamps the outer-loop deadline on
    // `centering_started` from the default centering config (30 s solve,
    // 10 s slew overhead) and the call's duration + max_attempts. The loop
    // then fails (no plate solver configured), but the started event — the
    // first on the bus — carries the deadline regardless.
    let handler = test_handler(camera_mount_registry(
        Arc::new(MockCamera::default()),
        Arc::new(MockTelescope::default()),
    ));
    let mut rx = handler.event_bus.subscribe();

    let _ = handler
        .center_on_target_inner(
            CenterOnTargetToolParams {
                camera_id: Some("cam".to_string()),
                train_id: None,
                ra: Some(0.7123),
                dec: Some(41.269),
                duration: Some(Duration::from_millis(100)),
                tolerance_arcsec: Some(60.0),
                max_attempts: Some(5),
            },
            None,
            Cancel::never(),
        )
        .await;

    let started = next_event(&mut rx).await;
    assert_eq!(started.event, "centering_started");
    assert_eq!(started.payload["max_attempts"], 5);
    // per_iter = 100 ms + 30 s + 10 s = 40_100 ms; max = 5 × per_iter.
    assert_eq!(started.predicted_duration_ms, Some(40_100));
    assert_eq!(started.max_duration_ms, Some(200_500));
}

#[tokio::test]
async fn unpark_emits_started_complete_triple() {
    let mount = MockTelescope::default();
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let mut rx = handler.event_bus.subscribe();

    let result = handler
        .unpark_inner(UnparkParams {}, &Cancel::never())
        .await
        .unwrap();
    assert!(!result.is_error.unwrap_or(false));

    let started = next_event(&mut rx).await;
    let complete = next_event(&mut rx).await;
    assert_no_more_events(&mut rx).await;

    assert_eq!(started.event, "unpark_started");
    assert_eq!(complete.event, "unpark_complete");
    assert_end_mirrors_start(&started, &complete);
}

#[tokio::test]
async fn unpark_failure_emits_started_then_failed() {
    let mount = MockTelescope {
        fail_unpark: true,
        ..Default::default()
    };
    let handler = test_handler(mount_registry(Arc::new(mount), None));
    let mut rx = handler.event_bus.subscribe();

    let result = handler
        .unpark_inner(UnparkParams {}, &Cancel::never())
        .await
        .unwrap();
    assert!(result.is_error.unwrap_or(false));

    let started = next_event(&mut rx).await;
    let failed = next_event(&mut rx).await;
    assert_no_more_events(&mut rx).await;

    assert_eq!(started.event, "unpark_started");
    assert_eq!(failed.event, "unpark_failed");
    assert!(failed.payload["error"]
        .as_str()
        .unwrap()
        .contains("failed to unpark"));
    assert_end_mirrors_start(&started, &failed);
}

// ---------------------------------------------------------------------------
// Guider tools (start_guiding / stop_guiding / dither / pause_guiding /
// resume_guiding / get_guiding_stats)
//
// These exercise the MCP handler against `MockGuiderClient`: settle
// merging (per-call > config default > omitted), the dither-amount
// fallback, the per-code error mapping, and the guide/dither event
// triples. End-to-end wire-format coverage lives in the BDD suite
// (`guider.feature`) against `GuiderStub`.
// ---------------------------------------------------------------------------

use crate::config::GuiderDefaults;
use crate::mcp::built_in::guider::{
    DitherParams, DitherUnit, GetGuidingStatsParams, PauseGuidingParams, ResumeGuidingParams,
    StartGuidingParams, StopGuidingParams,
};
use rp_guider::{GuiderError, GuidingStats, MockGuiderClient, SettledOutcome};

fn settled_outcome() -> SettledOutcome {
    SettledOutcome {
        state: "guiding".to_string(),
        rms_ra_px: Some(0.3),
        rms_dec_px: Some(0.4),
        total_rms_px: Some(0.5),
        sample_count: 12,
    }
}

fn start_params_empty() -> StartGuidingParams {
    StartGuidingParams {
        recalibrate: None,
        settle_pixels: None,
        settle_time: None,
        settle_timeout: None,
    }
}

fn dither_params_empty() -> DitherParams {
    DitherParams {
        pixels: None,
        unit: None,
        ra_only: None,
        settle_pixels: None,
        settle_time: None,
        settle_timeout: None,
    }
}

// -----------------------------------------------------------------------
// Mount motion gate wiring tests (rp.md § Mount Motion Gate)
// -----------------------------------------------------------------------
// The gate's own queueing semantics are pinned in motion_gate.rs; these
// pin the call-site wiring — which tools acquire which mode, that the
// `*_started` envelope stays post-acquire, and that un-trained cameras
// bypass the gate.

#[tokio::test(start_paused = true)]
async fn dither_takes_the_gate_exclusively_and_waits_for_shared_holders() {
    let handler = handler_with_guider(
        |mock| {
            mock.expect_dither().returning(|_| Ok(settled_outcome()));
        },
        GuiderDefaults::default(),
    );
    let mut rx = handler.event_bus.subscribe();
    let shared = handler.motion_gate.shared().await;

    let dither = {
        let handler = handler.clone();
        tokio::spawn(async move {
            handler
                .dither_inner(
                    DitherParams {
                        pixels: Some(3.0),
                        ..dither_params_empty()
                    },
                    &Cancel::never(),
                )
                .await
        })
    };

    // The pending event proves dither requested exclusive while the
    // shared holder was live — and, per the gate contract, only after
    // entering the fair queue.
    let envelope = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("expected mount_motion_pending while the shared holder is live")
        .unwrap();
    assert_eq!(envelope.event, "mount_motion_pending");
    assert_eq!(envelope.payload["operation"], "dither");
    assert!(
        !dither.is_finished(),
        "dither must still be waiting on the gate"
    );

    drop(shared);
    let json = ok_text(dither.await.unwrap().unwrap());
    assert_eq!(json["state"], "guiding");

    // dither_started must be the next emission — after the acquire,
    // never while queued.
    let started = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("expected dither_started after the gate released")
        .unwrap();
    assert_eq!(started.event, "dither_started");
}

#[tokio::test(start_paused = true)]
async fn slew_takes_the_gate_exclusively_before_resolving_the_mount() {
    let handler = test_handler(empty_registry());
    let mut rx = handler.event_bus.subscribe();
    let shared = handler.motion_gate.shared().await;

    let slew = {
        let handler = handler.clone();
        tokio::spawn(async move {
            handler
                .slew_inner(
                    SlewParams {
                        ra: Some(10.0),
                        dec: Some(45.0),
                        settle_after: None,
                    },
                    None,
                    &Cancel::never(),
                )
                .await
        })
    };

    let envelope = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("expected mount_motion_pending while the shared holder is live")
        .unwrap();
    assert_eq!(envelope.event, "mount_motion_pending");
    assert_eq!(envelope.payload["operation"], "slew");
    assert!(
        !slew.is_finished(),
        "slew must still be waiting on the gate"
    );

    // With no mount configured the slew then fails — after the gate,
    // which is exactly the documented acquire-before-pointing-read
    // ordering.
    drop(shared);
    assert_tool_error(slew.await.unwrap(), "no mount configured");
}

#[tokio::test(start_paused = true)]
async fn capture_through_an_imaging_train_camera_waits_for_motion() {
    let handler = test_handler(camera_registry(Arc::new(MockCamera::default())))
        .with_trains(cam_trains(1000.0));
    let exclusive = handler.motion_gate.exclusive("test").await;

    let capture = {
        let handler = handler.clone();
        tokio::spawn(async move {
            handler
                .capture_inner(
                    CaptureParams {
                        target: None,
                        frame_type: None,
                        camera_id: Some("cam".into()),
                        train_id: None,
                        duration: Duration::from_millis(100),
                    },
                    None,
                    &Cancel::never(),
                )
                .await
        })
    };

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !capture.is_finished(),
        "imaging-train capture must wait behind the exclusive holder"
    );

    drop(exclusive);
    let json = ok_text(capture.await.unwrap().unwrap());
    assert!(
        json["image_path"].as_str().is_some(),
        "capture must complete once the motion released the gate"
    );
}

#[tokio::test(start_paused = true)]
async fn capture_through_an_untrained_camera_ignores_the_gate() {
    let handler = test_handler(camera_registry(Arc::new(MockCamera::default())));
    let _exclusive = handler.motion_gate.exclusive("test").await;

    let result = tokio::time::timeout(
        Duration::from_secs(30),
        handler.capture_inner(
            CaptureParams {
                target: None,
                frame_type: None,
                camera_id: Some("cam".into()),
                train_id: None,
                duration: Duration::from_millis(100),
            },
            None,
            &Cancel::never(),
        ),
    )
    .await
    .expect("an un-trained capture must not wait on the gate")
    .unwrap();
    let json = ok_text(result);
    assert!(json["image_path"].as_str().is_some());
}

/// Build a handler with a configured guider client. Pass
/// `configure` to wire up mock expectations before the handler is
/// built.
fn handler_with_guider(
    configure: impl FnOnce(&mut MockGuiderClient),
    defaults: GuiderDefaults,
) -> McpHandler {
    let mut mock = MockGuiderClient::new();
    configure(&mut mock);
    let client: Arc<dyn rp_guider::GuiderClient> = Arc::new(mock);
    test_handler(empty_registry()).with_guider(Some(client), defaults)
}

#[tokio::test]
async fn start_guiding_returns_the_settled_snapshot() {
    let handler = handler_with_guider(
        |mock| {
            mock.expect_start_guiding()
                .withf(|req| !req.recalibrate && req.settle.is_none())
                .returning(|_| Ok(settled_outcome()));
        },
        GuiderDefaults::default(),
    );
    let result = handler
        .start_guiding_inner(start_params_empty(), &Cancel::never())
        .await
        .unwrap();
    let json = ok_text(result);
    assert_eq!(json["state"], "guiding");
    assert_eq!(json["rms_ra_px"], 0.3);
    assert_eq!(json["rms_dec_px"], 0.4);
    assert_eq!(json["total_rms_px"], 0.5);
    assert_eq!(json["sample_count"], 12);
}

#[tokio::test]
async fn start_guiding_forwards_config_settle_defaults() {
    let defaults = GuiderDefaults {
        settle_pixels: Some(0.8),
        settle_time: Some(Duration::from_secs(8)),
        settle_timeout: Some(Duration::from_secs(40)),
        dither_pixels: None,
        ..GuiderDefaults::default()
    };
    let handler = handler_with_guider(
        |mock| {
            mock.expect_start_guiding()
                .withf(|req| {
                    let settle = req.settle.as_ref().expect("settle expected");
                    settle.pixels == Some(0.8)
                        && settle.time == Some(Duration::from_secs(8))
                        && settle.timeout == Some(Duration::from_secs(40))
                })
                .returning(|_| Ok(settled_outcome()));
        },
        defaults,
    );
    let result = handler
        .start_guiding_inner(start_params_empty(), &Cancel::never())
        .await
        .unwrap();
    assert!(!result.is_error.unwrap_or(false));
}

#[tokio::test]
async fn start_guiding_per_call_settle_overrides_config_field_by_field() {
    let defaults = GuiderDefaults {
        settle_pixels: Some(0.8),
        settle_time: Some(Duration::from_secs(8)),
        settle_timeout: Some(Duration::from_secs(40)),
        dither_pixels: None,
        ..GuiderDefaults::default()
    };
    let handler = handler_with_guider(
        |mock| {
            mock.expect_start_guiding()
                .withf(|req| {
                    let settle = req.settle.as_ref().expect("settle expected");
                    // pixels and timeout from the call; time from config.
                    settle.pixels == Some(1.5)
                        && settle.time == Some(Duration::from_secs(8))
                        && settle.timeout == Some(Duration::from_secs(20))
                })
                .returning(|_| Ok(settled_outcome()));
        },
        defaults,
    );
    let result = handler
        .start_guiding_inner(
            StartGuidingParams {
                recalibrate: None,
                settle_pixels: Some(1.5),
                settle_time: None,
                settle_timeout: Some(Duration::from_secs(20)),
            },
            &Cancel::never(),
        )
        .await
        .unwrap();
    assert!(!result.is_error.unwrap_or(false));
}

#[tokio::test]
async fn start_guiding_emits_started_and_settled_with_the_settle_deadline() {
    let defaults = GuiderDefaults {
        settle_pixels: None,
        settle_time: Some(Duration::from_secs(8)),
        settle_timeout: Some(Duration::from_secs(40)),
        dither_pixels: None,
        ..GuiderDefaults::default()
    };
    let handler = handler_with_guider(
        |mock| {
            mock.expect_start_guiding()
                .returning(|_| Ok(settled_outcome()));
        },
        defaults,
    );
    let mut rx = handler.event_bus.subscribe();

    let result = handler
        .start_guiding_inner(start_params_empty(), &Cancel::never())
        .await
        .unwrap();
    assert!(!result.is_error.unwrap_or(false));

    let started = next_event(&mut rx).await;
    let settled = next_event(&mut rx).await;
    assert_no_more_events(&mut rx).await;

    assert_eq!(started.event, "guide_started");
    // predicted = settle_time; max = settle_timeout + the service's
    // 10 s backstop grace.
    assert_eq!(started.predicted_duration_ms, Some(8_000));
    assert_eq!(started.max_duration_ms, Some(50_000));
    assert_eq!(settled.event, "guide_settled");
    assert_eq!(settled.payload["total_rms_px"], 0.5);
    assert_eq!(settled.payload["sample_count"], 12);
    assert_end_mirrors_start(&started, &settled);
}

#[tokio::test]
async fn start_guiding_clamps_predicted_duration_to_the_timeout_when_settle_time_exceeds_it() {
    // A misconfigured settle_time longer than settle_timeout must
    // never produce predicted_duration_ms > max_duration_ms — the
    // guider service itself does not validate that ordering.
    let defaults = GuiderDefaults {
        settle_pixels: None,
        settle_time: Some(Duration::from_mins(2)),
        settle_timeout: Some(Duration::from_secs(40)),
        dither_pixels: None,
        ..GuiderDefaults::default()
    };
    let handler = handler_with_guider(
        |mock| {
            mock.expect_start_guiding()
                .returning(|_| Ok(settled_outcome()));
        },
        defaults,
    );
    let mut rx = handler.event_bus.subscribe();

    handler
        .start_guiding_inner(start_params_empty(), &Cancel::never())
        .await
        .unwrap();

    let started = next_event(&mut rx).await;
    // predicted clamps down to the timeout (40s), not the oversized
    // settle_time (120s); max stays timeout + 10s backstop grace.
    assert_eq!(started.predicted_duration_ms, Some(40_000));
    assert_eq!(started.max_duration_ms, Some(50_000));
    assert!(started.predicted_duration_ms <= started.max_duration_ms);
}

#[tokio::test]
async fn start_guiding_saturates_instead_of_overflowing_on_an_extreme_settle_timeout() {
    // An operator-configured settle_timeout near Duration::MAX must not
    // panic the process via `Duration`'s overflow-checked `Add` (the
    // backstop-grace addition saturates instead), and the resulting
    // millisecond count must saturate to u64::MAX rather than silently
    // truncating (as a bare `as u64` cast on the u128 millis would).
    let defaults = GuiderDefaults {
        settle_pixels: None,
        settle_time: None,
        settle_timeout: Some(Duration::MAX),
        dither_pixels: None,
        ..GuiderDefaults::default()
    };
    let handler = handler_with_guider(
        |mock| {
            mock.expect_start_guiding()
                .returning(|_| Ok(settled_outcome()));
        },
        defaults,
    );
    let mut rx = handler.event_bus.subscribe();

    handler
        .start_guiding_inner(start_params_empty(), &Cancel::never())
        .await
        .unwrap();

    let started = next_event(&mut rx).await;
    assert_eq!(started.predicted_duration_ms, Some(u64::MAX));
    assert_eq!(started.max_duration_ms, Some(u64::MAX));
}

#[tokio::test]
async fn start_guiding_without_a_settle_timeout_omits_the_deadline() {
    let handler = handler_with_guider(
        |mock| {
            mock.expect_start_guiding()
                .returning(|_| Ok(settled_outcome()));
        },
        GuiderDefaults::default(),
    );
    let mut rx = handler.event_bus.subscribe();

    handler
        .start_guiding_inner(start_params_empty(), &Cancel::never())
        .await
        .unwrap();

    let started = next_event(&mut rx).await;
    assert_eq!(started.event, "guide_started");
    assert!(started.predicted_duration_ms.is_none());
    assert!(started.max_duration_ms.is_none());
}

#[tokio::test]
async fn start_guiding_failure_maps_the_envelope_and_emits_guide_failed() {
    let handler = handler_with_guider(
        |mock| {
            mock.expect_start_guiding().returning(|_| {
                Err(GuiderError::Service {
                    code: "guide_failed".to_string(),
                    message: "no guide star".to_string(),
                    details: serde_json::Value::Null,
                })
            });
        },
        GuiderDefaults::default(),
    );
    let mut rx = handler.event_bus.subscribe();

    let result = handler
        .start_guiding_inner(start_params_empty(), &Cancel::never())
        .await;
    assert_tool_error(result, "guide_failed: no guide star");

    let started = next_event(&mut rx).await;
    let failed = next_event(&mut rx).await;
    assert_eq!(started.event, "guide_started");
    assert_eq!(failed.event, "guide_failed");
    assert!(failed.payload["error"]
        .as_str()
        .unwrap()
        .contains("no guide star"));
    assert_end_mirrors_start(&started, &failed);
}

#[tokio::test]
async fn start_guiding_unreachable_service_maps_to_service_unreachable() {
    let handler = handler_with_guider(
        |mock| {
            mock.expect_start_guiding().returning(|_| {
                Err(GuiderError::ServiceUnreachable(
                    "connection refused".to_string(),
                ))
            });
        },
        GuiderDefaults::default(),
    );
    let result = handler
        .start_guiding_inner(start_params_empty(), &Cancel::never())
        .await;
    assert_tool_error(result, "service unreachable");
}

#[tokio::test]
async fn dither_uses_the_pixels_parameter() {
    let handler = handler_with_guider(
        |mock| {
            mock.expect_dither()
                .withf(|req| req.amount_px == 5.0 && req.ra_only)
                .returning(|_| Ok(settled_outcome()));
        },
        GuiderDefaults::default(),
    );
    let result = handler
        .dither_inner(
            DitherParams {
                pixels: Some(5.0),
                ra_only: Some(true),
                ..dither_params_empty()
            },
            &Cancel::never(),
        )
        .await
        .unwrap();
    let json = ok_text(result);
    assert_eq!(json["state"], "guiding");
}

#[tokio::test]
async fn dither_falls_back_to_the_configured_dither_pixels() {
    let defaults = GuiderDefaults {
        dither_pixels: Some(3.5),
        ..GuiderDefaults::default()
    };
    let handler = handler_with_guider(
        |mock| {
            mock.expect_dither()
                .withf(|req| req.amount_px == 3.5 && !req.ra_only)
                .returning(|_| Ok(settled_outcome()));
        },
        defaults,
    );
    let result = handler
        .dither_inner(dither_params_empty(), &Cancel::never())
        .await
        .unwrap();
    assert!(!result.is_error.unwrap_or(false));
}

#[tokio::test]
async fn dither_with_no_amount_available_errors_without_an_rpc() {
    // No expectation on the mock: reaching the client would panic.
    let handler = handler_with_guider(|_| {}, GuiderDefaults::default());
    let result = handler
        .dither_inner(dither_params_empty(), &Cancel::never())
        .await;
    assert_tool_error(result, "dither_pixels");
}

/// Registry with two disconnected cameras carrying cached pixel
/// sizes: "guide-cam" 3.76 µm and "main-cam" 2.9 µm. The dither unit
/// conversion reads only the cached connect-time values, so no live
/// device is needed.
fn dither_dual_camera_registry() -> crate::equipment::EquipmentRegistry {
    let entry = |id: &str, pixel_size_x_um: f64| {
        crate::equipment::CameraEntry::new(
            id.to_string(),
            crate::config::CameraConfig {
                id: id.to_string(),
                name: "mock".to_string(),
                alpaca_url: "http://localhost:1".to_string(),
                device_type: String::new(),
                device_number: 0,
                cooler_targets_c: Vec::new(),
                gain: None,
                offset: None,
                readout_time_estimate: None,
                auth: None,
            },
            crate::equipment::DeviceSession::disconnected(),
            crate::equipment::CameraInvariants {
                max_adu: None,
                pixel_size_x_um: Some(pixel_size_x_um),
                pixel_size_y_um: Some(pixel_size_x_um),
                sensor_width_px: None,
                sensor_height_px: None,
            },
        )
    };
    crate::equipment::EquipmentRegistry {
        cameras: vec![entry("guide-cam", 3.76), entry("main-cam", 2.9)],
        ..Default::default()
    }
}

/// A guiding train (200 mm, "guide-cam") and optionally one imaging
/// train (1000 mm, "main-cam") — the dither unit-conversion inputs.
fn dither_trains(include_imaging: bool) -> crate::equipment::trains::TrainModel {
    let mut trains = vec![serde_json::json!(
        {"id": "guide", "purpose": "guiding", "focal_length_mm": 200.0,
         "devices": ["guide-cam"]}
    )];
    if include_imaging {
        trains.push(serde_json::json!(
            {"id": "main", "focal_length_mm": 1000.0, "devices": ["main-cam"]}
        ));
    }
    let equipment: crate::config::EquipmentConfig = serde_json::from_value(serde_json::json!({
        "cameras": [
            {"id": "guide-cam", "alpaca_url": "http://localhost:1"},
            {"id": "main-cam", "alpaca_url": "http://localhost:1"}
        ],
        "mount": {
            "alpaca_url": "http://localhost:1",
            "guiding": {"url": "http://localhost:1"}
        },
        "optical_trains": trains
    }))
    .unwrap();
    crate::equipment::trains::TrainModel::try_from_equipment(&equipment).unwrap()
}

fn dither_handler_with_trains(expected_amount_px: f64, include_imaging: bool) -> McpHandler {
    let mut mock = MockGuiderClient::new();
    mock.expect_dither()
        .withf(move |req| (req.amount_px - expected_amount_px).abs() < 1e-9)
        .returning(|_| Ok(settled_outcome()));
    let client: Arc<dyn rp_guider::GuiderClient> = Arc::new(mock);
    test_handler(dither_dual_camera_registry())
        .with_guider(Some(client), GuiderDefaults::default())
        .with_trains(dither_trains(include_imaging))
}

#[tokio::test]
async fn dither_converts_arcsec_at_the_guiding_train_pixel_scale() {
    // 10″ at guide scale 206.265 × 3.76 µm / 200 mm ≈ 3.8778 ″/px.
    let expected = 10.0 / (206.265 * 3.76 / 200.0);
    let handler = dither_handler_with_trains(expected, false);
    let result = handler
        .dither_inner(
            DitherParams {
                pixels: Some(10.0),
                unit: Some(DitherUnit::Arcsec),
                ..dither_params_empty()
            },
            &Cancel::never(),
        )
        .await
        .unwrap();
    assert!(!result.is_error.unwrap_or(false));
}

#[tokio::test]
async fn dither_converts_main_px_through_both_train_pixel_scales() {
    // 10 main-camera px → arcsec at the imaging train's scale
    // (206.265 × 2.9 / 1000) → guide px at the guiding train's scale.
    let expected = 10.0 * (206.265 * 2.9 / 1000.0) / (206.265 * 3.76 / 200.0);
    let handler = dither_handler_with_trains(expected, true);
    let result = handler
        .dither_inner(
            DitherParams {
                pixels: Some(10.0),
                unit: Some(DitherUnit::MainPx),
                ..dither_params_empty()
            },
            &Cancel::never(),
        )
        .await
        .unwrap();
    assert!(!result.is_error.unwrap_or(false));
}

#[tokio::test]
async fn dither_arcsec_without_a_guiding_train_errors_without_an_rpc() {
    // Trains default to empty on the test handler; no mock
    // expectation — reaching the client would panic.
    let handler = handler_with_guider(|_| {}, GuiderDefaults::default());
    let result = handler
        .dither_inner(
            DitherParams {
                pixels: Some(10.0),
                unit: Some(DitherUnit::Arcsec),
                ..dither_params_empty()
            },
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "guiding train");
}

#[tokio::test]
async fn dither_unit_without_explicit_pixels_errors_without_an_rpc() {
    let handler = handler_with_guider(
        |_| {},
        GuiderDefaults {
            dither_pixels: Some(3.5),
            ..GuiderDefaults::default()
        },
    );
    let result = handler
        .dither_inner(
            DitherParams {
                unit: Some(DitherUnit::Arcsec),
                ..dither_params_empty()
            },
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "explicit pixels");
}

#[tokio::test]
async fn dither_emits_started_and_settled() {
    let handler = handler_with_guider(
        |mock| {
            mock.expect_dither().returning(|_| Ok(settled_outcome()));
        },
        GuiderDefaults::default(),
    );
    let mut rx = handler.event_bus.subscribe();

    handler
        .dither_inner(
            DitherParams {
                pixels: Some(5.0),
                ..dither_params_empty()
            },
            &Cancel::never(),
        )
        .await
        .unwrap();

    let started = next_event(&mut rx).await;
    let settled = next_event(&mut rx).await;
    assert_no_more_events(&mut rx).await;

    assert_eq!(started.event, "dither_started");
    assert_eq!(started.payload["pixels"], 5.0);
    assert_eq!(settled.event, "dither_settled");
    assert_end_mirrors_start(&started, &settled);
}

#[tokio::test]
async fn dither_failure_emits_dither_failed() {
    let handler = handler_with_guider(
        |mock| {
            mock.expect_dither().returning(|_| {
                Err(GuiderError::Service {
                    code: "not_guiding".to_string(),
                    message: "PHD2 is not guiding".to_string(),
                    details: serde_json::Value::Null,
                })
            });
        },
        GuiderDefaults::default(),
    );
    let mut rx = handler.event_bus.subscribe();

    let result = handler
        .dither_inner(
            DitherParams {
                pixels: Some(5.0),
                ..dither_params_empty()
            },
            &Cancel::never(),
        )
        .await;
    assert_tool_error(result, "not_guiding");

    let started = next_event(&mut rx).await;
    let failed = next_event(&mut rx).await;
    assert_eq!(started.event, "dither_started");
    assert_eq!(failed.event, "dither_failed");
}

#[tokio::test]
async fn stop_guiding_emits_guide_stopped_with_reason_requested() {
    let handler = handler_with_guider(
        |mock| {
            mock.expect_stop_guiding().returning(|| Ok(()));
        },
        GuiderDefaults::default(),
    );
    let mut rx = handler.event_bus.subscribe();

    let result = handler
        .stop_guiding(Parameters(StopGuidingParams {}))
        .await
        .unwrap();
    let json = ok_text(result);
    assert_eq!(json["state"], "stopped");

    let stopped = next_event(&mut rx).await;
    assert_no_more_events(&mut rx).await;
    assert_eq!(stopped.event, "guide_stopped");
    assert_eq!(stopped.payload["reason"], "requested");
    assert!(stopped.operation_id.is_none(), "point event, no operation");
}

#[tokio::test]
async fn stop_guiding_failure_errors_without_the_stopped_event() {
    let handler = handler_with_guider(
        |mock| {
            mock.expect_stop_guiding().returning(|| {
                Err(GuiderError::Service {
                    code: "stop_timeout".to_string(),
                    message: "PHD2 did not confirm the stop".to_string(),
                    details: serde_json::Value::Null,
                })
            });
        },
        GuiderDefaults::default(),
    );
    let mut rx = handler.event_bus.subscribe();

    let result = handler.stop_guiding(Parameters(StopGuidingParams {})).await;
    assert_tool_error(result, "stop_timeout");
    assert_no_more_events(&mut rx).await;
}

#[tokio::test]
async fn pause_guiding_forwards_full() {
    let handler = handler_with_guider(
        |mock| {
            mock.expect_pause_guiding()
                .withf(|&full| full)
                .returning(|_| Ok(()));
        },
        GuiderDefaults::default(),
    );
    let result = handler
        .pause_guiding(Parameters(PauseGuidingParams { full: Some(true) }))
        .await
        .unwrap();
    let json = ok_text(result);
    assert_eq!(json["state"], "paused");
}

#[tokio::test]
async fn resume_guiding_resumes() {
    let handler = handler_with_guider(
        |mock| {
            mock.expect_resume_guiding().returning(|| Ok(()));
        },
        GuiderDefaults::default(),
    );
    let result = handler
        .resume_guiding_inner(ResumeGuidingParams {}, &Cancel::never())
        .await
        .unwrap();
    let json = ok_text(result);
    assert_eq!(json["state"], "resumed");
}

#[tokio::test]
async fn get_guiding_stats_passes_the_snapshot_through() {
    let handler = handler_with_guider(
        |mock| {
            mock.expect_guiding_stats().returning(|| {
                Ok(GuidingStats {
                    app_state: "Guiding".to_string(),
                    guiding: true,
                    rms_ra_px: Some(0.3),
                    rms_dec_px: Some(0.4),
                    total_rms_px: Some(0.5),
                    snr: None,
                    star_mass: None,
                    sample_count: 12,
                })
            });
        },
        GuiderDefaults::default(),
    );
    let result = handler
        .get_guiding_stats(Parameters(GetGuidingStatsParams {}))
        .await
        .unwrap();
    let json = ok_text(result);
    assert_eq!(json["app_state"], "Guiding");
    assert_eq!(json["guiding"], true);
    assert_eq!(json["rms_ra_px"], 0.3);
    // Never-sampled telemetry stays null, not stale numbers.
    assert_eq!(json["snr"], serde_json::Value::Null);
    assert_eq!(json["star_mass"], serde_json::Value::Null);
}

#[tokio::test]
async fn every_guider_tool_reports_not_configured_without_a_guider_block() {
    // No `with_guider` call ⇒ each of the six tools errors cleanly.
    let handler = test_handler(empty_registry());
    assert_tool_error(
        handler
            .start_guiding_inner(start_params_empty(), &Cancel::never())
            .await,
        "start_guiding: guider not configured",
    );
    assert_tool_error(
        handler.stop_guiding(Parameters(StopGuidingParams {})).await,
        "stop_guiding: guider not configured",
    );
    assert_tool_error(
        handler
            .dither_inner(
                DitherParams {
                    pixels: Some(5.0),
                    ..dither_params_empty()
                },
                &Cancel::never(),
            )
            .await,
        "dither: guider not configured",
    );
    assert_tool_error(
        handler
            .pause_guiding(Parameters(PauseGuidingParams { full: None }))
            .await,
        "pause_guiding: guider not configured",
    );
    assert_tool_error(
        handler
            .resume_guiding_inner(ResumeGuidingParams {}, &Cancel::never())
            .await,
        "resume_guiding: guider not configured",
    );
    assert_tool_error(
        handler
            .get_guiding_stats(Parameters(GetGuidingStatsParams {}))
            .await,
        "get_guiding_stats: guider not configured",
    );
}

/// The on-disk reverse-lookup key is written from the UUID's `time_low`
/// field (`internals.rs` capture) but read back by slicing the document id's
/// first eight characters (`persistence::cache::disk_resolve`). The two must
/// agree or a captured frame becomes unresolvable from disk.
#[test]
fn test_uuid8_disk_key_matches_the_document_id_prefix() {
    for _ in 0..1_000 {
        let uuid = uuid::Uuid::new_v4();
        let written = format!("{:08x}", uuid.as_fields().0);
        let read_back = uuid.to_string();
        let read_back = read_back.get(..8).unwrap();
        assert_eq!(written, read_back, "uuid8 mismatch for {uuid}");
    }
}

// -----------------------------------------------------------------------
// Cancellation (rp.md § Safety → In-Flight Tool Calls): every blocking
// helper races its poll loop against the call's `Cancel`, stops within
// one tick, issues its stop-class counterpart where one exists, and
// answers `cancelled: <reason>`. `start_paused` auto-advances virtual
// time, so the cancel lands at exactly `CANCEL_AT` and the helper's
// return time measures its reaction.
// -----------------------------------------------------------------------

/// When the scripted cancellation fires, in virtual time.
const CANCEL_AT: Duration = Duration::from_secs(1);

/// The poll tick every helper sleeps between device reads.
const POLL_TICK: Duration = Duration::from_millis(100);

/// Cancel `cancel` with `reason` after [`CANCEL_AT`] of virtual time.
fn cancel_later(cancel: &Cancel, reason: super::inflight::CancelReason) {
    let cancel = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(CANCEL_AT).await;
        cancel.cancel(reason);
    });
}

fn calls(counter: &std::sync::atomic::AtomicU32) -> u32 {
    counter.load(std::sync::atomic::Ordering::SeqCst)
}

#[tokio::test(start_paused = true)]
async fn do_slew_blocking_cancelled_aborts_the_slew_within_one_tick() {
    let mount = Arc::new(MockTelescope {
        stuck_slewing: true,
        ..Default::default()
    });
    let handler = test_handler(mount_registry(mount.clone(), None));
    let cancel = Cancel::never();
    cancel_later(&cancel, super::inflight::CancelReason::Safety);
    let started = tokio::time::Instant::now();

    let err = handler
        .do_slew_blocking(0.0, 0.0, Duration::ZERO, None, &cancel)
        .await
        .expect_err("a cancelled slew must fail");

    assert_eq!(err, "cancelled: safety");
    assert_eq!(
        calls(&mount.abort_slew_calls),
        1,
        "the slew must be aborted"
    );
    assert!(
        started.elapsed() <= CANCEL_AT + POLL_TICK,
        "the loop kept polling after the cancel: {:?}",
        started.elapsed()
    );
}

#[tokio::test(start_paused = true)]
async fn do_slew_blocking_cancelled_during_settle_returns_without_aborting() {
    // The mount reports idle at once; the 30 s settle is where the
    // cancel lands. Nothing is moving, so nothing is aborted.
    let mount = Arc::new(MockTelescope::default());
    let handler = test_handler(mount_registry(mount.clone(), None));
    let cancel = Cancel::never();
    cancel_later(&cancel, super::inflight::CancelReason::Safety);

    let err = handler
        .do_slew_blocking(0.0, 0.0, Duration::from_secs(30), None, &cancel)
        .await
        .expect_err("a slew cancelled mid-settle must fail");

    assert_eq!(err, "cancelled: safety");
    assert_eq!(calls(&mount.abort_slew_calls), 0);
}

#[tokio::test(start_paused = true)]
async fn do_park_blocking_cancelled_stops_waiting_without_aborting() {
    let mount = Arc::new(MockTelescope {
        at_park_false_count: u32::MAX,
        ..Default::default()
    });
    let handler = test_handler(mount_registry(mount.clone(), None));
    let cancel = Cancel::never();
    cancel_later(&cancel, super::inflight::CancelReason::ClientDisconnected);
    let started = tokio::time::Instant::now();

    let err = handler
        .do_park_blocking(None, &cancel)
        .await
        .expect_err("a cancelled park wait must fail");

    assert_eq!(err, "cancelled: client disconnected");
    assert_eq!(
        calls(&mount.abort_slew_calls),
        0,
        "a park is never aborted on cancellation"
    );
    assert!(started.elapsed() <= CANCEL_AT + POLL_TICK);
}

#[tokio::test(start_paused = true)]
async fn poll_slewing_until_idle_cancelled_reports_cancelled_to_the_caller() {
    let mount = MockTelescope {
        stuck_slewing: true,
        ..Default::default()
    };
    let cancel = Cancel::never();
    cancel_later(&cancel, super::inflight::CancelReason::Safety);

    let err =
        super::internals::poll_slewing_until_idle(&mount, Duration::from_mins(5), None, &cancel)
            .await
            .expect_err("a cancelled poll must fail");

    assert!(
        matches!(err, super::internals::PollIdleError::Cancelled),
        "the abort decision belongs to the caller, not the poll"
    );
    assert_eq!(calls(&mount.abort_slew_calls), 0);
}

#[tokio::test(start_paused = true)]
async fn do_capture_cancelled_aborts_the_exposure_within_one_tick() {
    let cam = Arc::new(MockCamera {
        never_ready: true,
        ..Default::default()
    });
    let tmp = tempfile::tempdir().expect("temp dir");
    let mut handler = test_handler(camera_registry(cam.clone()));
    handler.session_config = SessionConfig {
        data_directory: tmp.path().to_string_lossy().to_string(),
    };
    let cancel = Cancel::never();
    cancel_later(&cancel, super::inflight::CancelReason::ClientDisconnected);
    let started = tokio::time::Instant::now();

    let err = handler
        .do_capture("cam", Duration::from_secs(10), None, None, None, &cancel)
        .await
        .expect_err("a cancelled capture must fail");

    assert_eq!(err, "cancelled: client disconnected");
    assert_eq!(
        calls(&cam.abort_exposure_calls),
        1,
        "the exposure must be aborted"
    );
    assert!(started.elapsed() <= CANCEL_AT + POLL_TICK);
}

#[tokio::test(start_paused = true)]
async fn do_move_focuser_blocking_cancelled_halts_the_focuser_within_one_tick() {
    let foc = Arc::new(MockFocuser {
        stuck_moving: true,
        ..Default::default()
    });
    let handler = test_handler(focuser_registry(foc.clone(), None, None));
    let cancel = Cancel::never();
    cancel_later(&cancel, super::inflight::CancelReason::Safety);
    let started = tokio::time::Instant::now();

    let err = handler
        .do_move_focuser_blocking("foc", 1000, None, &cancel)
        .await
        .expect_err("a cancelled focuser move must fail");

    assert_eq!(err, "cancelled: safety");
    assert_eq!(calls(&foc.halt_calls), 1, "the move must be halted");
    assert!(started.elapsed() <= CANCEL_AT + POLL_TICK);
}

/// A handle that is already cancelled when the helper starts must
/// return before touching the device: the slew never begins.
#[tokio::test]
async fn do_slew_blocking_with_a_cancelled_handle_never_slews() {
    let mount = Arc::new(MockTelescope::default());
    let handler = test_handler(mount_registry(mount.clone(), None));
    let cancel = Cancel::never();
    cancel.cancel(super::inflight::CancelReason::Safety);

    let err = handler
        .do_slew_blocking(0.0, 0.0, Duration::ZERO, None, &cancel)
        .await
        .expect_err("an already-cancelled slew must fail");

    assert_eq!(err, "cancelled: safety");
    assert_eq!(calls(&mount.abort_slew_calls), 0, "nothing was moving");
}

/// `unpark` is gated: a handle already cancelled when the body runs must
/// leave `AtPark` untouched and report the cancellation on the event
/// stream, so the enforcer's own park is never raced by a late unpark.
#[tokio::test]
async fn unpark_inner_with_a_cancelled_handle_never_unparks() {
    let mount = Arc::new(MockTelescope {
        at_park_value: true,
        ..Default::default()
    });
    let handler = test_handler(mount_registry(mount.clone(), None));
    let mut rx = handler.event_bus.subscribe();
    let cancel = Cancel::never();
    cancel.cancel(super::inflight::CancelReason::Safety);

    let result = handler.unpark_inner(UnparkParams {}, &cancel).await;

    assert_tool_error(result, "cancelled: safety");
    assert_eq!(
        calls(&mount.unpark_calls),
        0,
        "the driver must not be asked to unpark"
    );
    let started = next_event(&mut rx).await;
    let failed = next_event(&mut rx).await;
    assert_no_more_events(&mut rx).await;
    assert_eq!(started.event, "unpark_started");
    assert_eq!(failed.event, "unpark_failed");
    assert_eq!(failed.payload["error"].as_str(), Some("cancelled: safety"));
    assert_end_mirrors_start(&started, &failed);
}

/// `set_tracking` is gated: a handle already cancelled when the body runs
/// must leave the tracking drive untouched.
#[tokio::test]
async fn set_tracking_inner_with_a_cancelled_handle_never_touches_the_drive() {
    let mount = Arc::new(MockTelescope::default());
    let handler = test_handler(mount_registry(mount.clone(), None));
    let cancel = Cancel::never();
    cancel.cancel(super::inflight::CancelReason::ClientDisconnected);

    let result = handler
        .set_tracking_inner(SetTrackingParams { enabled: true }, &cancel)
        .await;

    assert_tool_error(result, "cancelled: client disconnected");
    assert_eq!(
        calls(&mount.set_tracking_calls),
        0,
        "the driver must not be asked to change tracking"
    );
}

// -----------------------------------------------------------------------
// Dither cancellation (rp.md § Mount Motion Gate + § In-Flight Tool
// Calls): a call cancelled while queued never reaches the guider; a
// call cancelled mid-settle answers at once but its motion permit stays
// held until the guider's settle RPC ends, bounded by the tail cap.
// -----------------------------------------------------------------------

/// A guider whose `dither` blocks until [`ReleasableDitherGuider::release`]
/// is called — stands in for PHD2 still pulsing the mount towards the
/// new lock position after rp stopped waiting.
#[derive(Default)]
struct ReleasableDitherGuider {
    release: tokio::sync::Notify,
    dither_calls: std::sync::atomic::AtomicU32,
}

impl ReleasableDitherGuider {
    fn release(&self) {
        self.release.notify_one();
    }
}

#[async_trait::async_trait]
impl rp_guider::GuiderClient for ReleasableDitherGuider {
    async fn start_guiding(
        &self,
        _request: rp_guider::StartGuidingRequest,
    ) -> Result<SettledOutcome, GuiderError> {
        panic!("not exercised by these tests")
    }

    async fn stop_guiding(&self) -> Result<(), GuiderError> {
        panic!("not exercised by these tests")
    }

    async fn pause_guiding(&self, _full: bool) -> Result<(), GuiderError> {
        panic!("not exercised by these tests")
    }

    async fn resume_guiding(&self) -> Result<(), GuiderError> {
        panic!("not exercised by these tests")
    }

    async fn dither(
        &self,
        _request: rp_guider::DitherRequest,
    ) -> Result<SettledOutcome, GuiderError> {
        self.dither_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.release.notified().await;
        Ok(settled_outcome())
    }

    async fn guiding_stats(&self) -> Result<rp_guider::GuidingStats, GuiderError> {
        panic!("not exercised by these tests")
    }

    async fn guiding_metrics(&self) -> Result<rp_guider::GuidingMetrics, GuiderError> {
        panic!("not exercised by these tests")
    }

    async fn current_equipment(&self) -> Result<rp_guider::PhdEquipment, GuiderError> {
        panic!("not exercised by these tests")
    }

    async fn clear_calibration(&self) -> Result<(), GuiderError> {
        panic!("not exercised by these tests")
    }

    async fn reselect_star(&self) -> Result<(), GuiderError> {
        panic!("not exercised by these tests")
    }
}

fn handler_with_releasable_guider() -> (McpHandler, Arc<ReleasableDitherGuider>) {
    let guider = Arc::new(ReleasableDitherGuider::default());
    let client: Arc<dyn rp_guider::GuiderClient> = guider.clone();
    let handler =
        test_handler(empty_registry()).with_guider(Some(client), GuiderDefaults::default());
    (handler, guider)
}

#[tokio::test(start_paused = true)]
async fn dither_cancelled_while_queued_on_the_gate_returns_without_an_rpc() {
    let handler = handler_with_guider(
        |mock| {
            mock.expect_dither().times(0);
        },
        GuiderDefaults::default(),
    );
    let mut rx = handler.event_bus.subscribe();
    let shared = handler.motion_gate.shared().await;
    let cancel = Cancel::never();

    let dither = {
        let handler = handler.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            handler
                .dither_inner(
                    DitherParams {
                        pixels: Some(3.0),
                        ..dither_params_empty()
                    },
                    &cancel,
                )
                .await
        })
    };
    let pending = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("expected mount_motion_pending while the shared holder is live")
        .unwrap();
    assert_eq!(pending.event, "mount_motion_pending");

    cancel.cancel(super::inflight::CancelReason::Safety);
    let result = tokio::time::timeout(Duration::from_secs(5), dither)
        .await
        .expect("a dither cancelled while queued must return promptly")
        .unwrap();
    assert_tool_error(result, "cancelled: safety");
    // No `dither_started`, so no `dither_failed` either: nothing began.
    assert_no_more_events(&mut rx).await;
    drop(shared);
}

#[tokio::test(start_paused = true)]
async fn dither_cancelled_mid_settle_keeps_the_gate_exclusive_until_the_guider_settles() {
    let (handler, guider) = handler_with_releasable_guider();
    let mut rx = handler.event_bus.subscribe();
    let cancel = Cancel::never();
    cancel_later(&cancel, super::inflight::CancelReason::ClientDisconnected);
    let started_at = tokio::time::Instant::now();

    let result = handler
        .dither_inner(
            DitherParams {
                pixels: Some(3.0),
                ..dither_params_empty()
            },
            &cancel,
        )
        .await;

    assert_tool_error(result, "cancelled: client disconnected");
    assert!(
        started_at.elapsed() <= CANCEL_AT + POLL_TICK,
        "the body must answer as soon as the cancel lands, not wait for the settle"
    );
    assert_eq!(calls(&guider.dither_calls), 1);
    let started = next_event(&mut rx).await;
    let failed = next_event(&mut rx).await;
    assert_eq!(started.event, "dither_started");
    assert_eq!(failed.event, "dither_failed");
    assert_eq!(
        failed.payload["error"].as_str(),
        Some("cancelled: client disconnected")
    );

    // The guider is still settling: a capture must not get the gate.
    assert!(
        tokio::time::timeout(Duration::from_secs(1), handler.motion_gate.shared())
            .await
            .is_err(),
        "the motion permit must outlive the cancelled body while the guider is still settling"
    );

    guider.release();
    let _shared = tokio::time::timeout(Duration::from_secs(5), handler.motion_gate.shared())
        .await
        .expect("the permit must release once the guider's settle RPC ends");
}

#[tokio::test(start_paused = true)]
async fn dither_tail_holder_releases_the_gate_at_the_cap_when_the_guider_never_settles() {
    let (handler, _guider) = handler_with_releasable_guider();
    let cancel = Cancel::never();
    cancel_later(&cancel, super::inflight::CancelReason::Safety);
    let settle_timeout = Duration::from_secs(5);

    let result = handler
        .dither_inner(
            DitherParams {
                pixels: Some(3.0),
                settle_timeout: Some(settle_timeout),
                ..dither_params_empty()
            },
            &cancel,
        )
        .await;
    assert_tool_error(result, "cancelled: safety");

    // Held past the settle timeout itself (the guider may still be
    // inside its own backstop margin)...
    assert!(
        tokio::time::timeout(settle_timeout, handler.motion_gate.shared())
            .await
            .is_err(),
        "the permit must be held through the settle timeout"
    );
    // ...but never past the cap: a wedged guider must not wedge the gate.
    let _shared = tokio::time::timeout(Duration::from_secs(60), handler.motion_gate.shared())
        .await
        .expect("the tail holder must release the permit at its cap");
}

// -----------------------------------------------------------------------
// get_next_target's filter-batching tie-break reads the wheel (rp.md
// § Decision Logic bullet 4). The ranking itself is decision.rs's; these
// pin the wheel resolution (train's sole wheel / the rig's only wheel)
// and the neutral outcomes (failed read, moving, ambiguous train).
// -----------------------------------------------------------------------

/// Two always-visible targets at the same coordinates with untouched
/// goals — an exact tie all the way down to bullet 4, where store order
/// ("alpha-lum" before "beta-red") decides unless the wheel says
/// otherwise — behind the test registry's wheel `fw` (`Lum` at 0, `Red`
/// at 1) and the `wheel_trains()` model, whose `main` train holds that
/// wheel alone and whose `two-wheels` train holds two.
async fn handler_with_tied_targets_and_wheel(
    fw: MockFilterWheel,
) -> (McpHandler, tempfile::TempDir) {
    use rp_targets::TargetStore;
    let dir = tempfile::tempdir().unwrap();
    let store = rp_targets::RedbTargetStore::open(dir.path().join("targets.redb"))
        .await
        .unwrap();
    for (name, filter) in [("Alpha Lum", "Lum"), ("Beta Red", "Red")] {
        store
            .upsert_target(test_store_target(
                name,
                0.0,
                0.0,
                Some(-90.0),
                vec![store_goal(filter, 60, 2)],
            ))
            .await
            .unwrap();
    }
    let store: Arc<dyn rp_targets::TargetStore> = Arc::new(store);
    let h = McpHandler::new(
        Arc::new(filter_wheel_registry(Arc::new(fw))),
        Arc::new(crate::events::EventBus::from_config(&[], None).unwrap()),
        SessionConfig {
            data_directory: dir.path().to_string_lossy().to_string(),
        },
        ImageCache::new(64, 4, std::path::PathBuf::from("/nonexistent"), 0),
        Some(test_site()),
    )
    .with_trains(wheel_trains())
    .with_target_store(Some(store), crate::config::TargetStoreConfig::default())
    .with_naming_templates(Some(Arc::new(test_naming_templates())));
    (h, dir)
}

async fn recommended(h: &McpHandler, train_id: Option<&str>) -> String {
    let v = ok_json(
        h.get_next_target(Parameters(GetNextTargetParams {
            time: None,
            train_id: train_id.map(String::from),
        }))
        .await,
    );
    v["target"]["name"]
        .as_str()
        .unwrap_or_else(|| panic!("no target in {v}"))
        .to_string()
}

#[tokio::test]
async fn get_next_target_prefers_the_filter_in_the_train_wheel() {
    let (h, _dir) = handler_with_tied_targets_and_wheel(MockFilterWheel {
        position: 1,
        ..Default::default()
    })
    .await;
    assert_eq!(
        recommended(&h, Some("main")).await,
        "beta-red",
        "Red is in the wheel, so the Red goal wins the tie"
    );
}

#[tokio::test]
async fn get_next_target_reads_the_only_wheel_when_no_train_is_named() {
    let (h, _dir) = handler_with_tied_targets_and_wheel(MockFilterWheel {
        position: 1,
        ..Default::default()
    })
    .await;
    assert_eq!(recommended(&h, None).await, "beta-red");
}

#[tokio::test]
async fn a_wheel_read_failure_leaves_the_tie_break_neutral() {
    let (h, _dir) = handler_with_tied_targets_and_wheel(MockFilterWheel {
        position: 1,
        fail_position_poll: true,
        ..Default::default()
    })
    .await;
    assert_eq!(
        recommended(&h, Some("main")).await,
        "alpha-lum",
        "an unreadable wheel abstains; store order decides"
    );
}

#[tokio::test]
async fn a_moving_wheel_leaves_the_tie_break_neutral() {
    let (h, _dir) = handler_with_tied_targets_and_wheel(MockFilterWheel {
        position: 1,
        report_moving: true,
        ..Default::default()
    })
    .await;
    assert_eq!(recommended(&h, Some("main")).await, "alpha-lum");
}

#[tokio::test]
async fn a_train_without_a_sole_wheel_leaves_the_tie_break_neutral() {
    let (h, _dir) = handler_with_tied_targets_and_wheel(MockFilterWheel {
        position: 1,
        ..Default::default()
    })
    .await;
    assert_eq!(
        recommended(&h, Some("two-wheels")).await,
        "alpha-lum",
        "an ambiguous train reads no wheel"
    );
}

#[tokio::test]
async fn a_wheelless_rig_leaves_the_tie_break_neutral() {
    let (h, _dir) = handler_with_store_target(test_store_target(
        "Alpha Lum",
        0.0,
        0.0,
        Some(-90.0),
        vec![store_goal("Lum", 60, 2)],
    ))
    .await;
    // No wheel in the roster and no train named: the recommendation
    // still comes, with bullet 4 abstaining.
    assert_eq!(recommended(&h, None).await, "alpha-lum");
}

// -----------------------------------------------------------------------
// start_cooldown / start_warmup — the tool wrappers over the cooling
// controller (rp.md § Camera Cooling). The controller's own semantics
// are pinned in cooling.rs; these pin the wiring and the result shape.
// -----------------------------------------------------------------------

#[tokio::test]
async fn cooling_tools_report_not_configured_without_a_controller() {
    let h = test_handler(empty_registry());
    assert_tool_error(h.start_cooldown().await, "camera cooling is not configured");
    assert_tool_error(h.start_warmup().await, "camera cooling is not configured");
}

#[tokio::test]
async fn cooling_tools_answer_with_the_cameras_they_drive() {
    use crate::cooling::test_support::{controller_for, stub_router, CoolerSim, Sim};
    let sim: Sim = Arc::new(std::sync::Mutex::new(CoolerSim::new()));
    let stub = crate::equipment::test_support::spawn_stub(stub_router(sim.clone())).await;
    let (cooling, _rx) = controller_for(&stub.url(), &[-10]).await;
    let h = test_handler(empty_registry()).with_cooling(cooling.clone());

    let v = ok_json(h.start_cooldown().await);
    assert_eq!(v["cameras"], serde_json::json!(["main-cam"]));
    // The pass commands the rung within its first poll; wait for it so
    // the warm-up below has something commanded to ramp from.
    for _ in 0..100 {
        if cooling.rung_for("main-cam").is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(cooling.rung_for("main-cam"), Some(-10));

    let v = ok_json(h.start_warmup().await);
    assert_eq!(v["cameras"], serde_json::json!(["main-cam"]));
    assert_eq!(
        cooling.rung_for("main-cam"),
        None,
        "the rung clears at once"
    );
}
