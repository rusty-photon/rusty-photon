//! The Alpaca Camera: the simulated sensor's frames. See
//! `docs/crates/guide-simulator-alpaca.md` (The camera).

use std::sync::Arc;
use std::time::Duration;

use ascom_alpaca::api::camera::{CameraState, ImageArray, SensorType};
use ascom_alpaca::api::{Camera, Device};
use ascom_alpaca::{ASCOMError, ASCOMResult};
use guide_simulator::{SensorType as SimSensor, SimTime};
use tracing::debug;

use crate::config::{ExposureRecord, FrameFault};
use crate::rig::{self, Exposure, Rig, State};

/// The shortest exposure the camera takes.
const EXPOSURE_MIN: Duration = Duration::from_millis(1);

/// The longest exposure the camera takes.
const EXPOSURE_MAX: Duration = Duration::from_secs(3_600);

/// The highest `Gain` and `Offset`; the lowest is 0.
const SETTING_MAX: i32 = 1_000;

/// The camera in front of the simulated sensor.
#[derive(Debug, Clone)]
pub struct SimCamera {
    rig: Arc<Rig>,
}

impl SimCamera {
    pub const fn new(rig: Arc<Rig>) -> Self {
        Self { rig }
    }

    /// Runs `f` on the state, refused when the camera is not connected.
    fn session<T>(&self, f: impl FnOnce(&mut State, SimTime) -> ASCOMResult<T>) -> ASCOMResult<T> {
        let now = self.rig.now();
        self.rig.with(|state| {
            if state.camera.connected {
                f(state, now)
            } else {
                Err(ASCOMError::NOT_CONNECTED)
            }
        })
    }

    /// `value`, refused when the camera is not connected.
    fn when_connected<T>(&self, value: T) -> ASCOMResult<T> {
        self.session(|_, _| Ok(value))
    }

    /// The simulated sensor's colour layout.
    fn sensor(&self) -> ASCOMResult<SimSensor> {
        self.session(|state, _| Ok(state.sim.camera().sensor))
    }
}

/// The exposure whose image is ready at `now`.
fn ready(state: &State, now: SimTime) -> Option<&Exposure> {
    state
        .camera
        .exposure
        .as_ref()
        .filter(|exposure| exposure.ready.is_some_and(|ready| ready <= now))
}

/// Whether an exposure runs at `now`.
fn exposing(state: &State, now: SimTime) -> bool {
    state
        .camera
        .exposure
        .as_ref()
        .is_some_and(|exposure| exposure.end > now)
}

/// Whether the last exposure reads out at `now`: it has ended, and its
/// image is still to come. An image that never comes holds nothing up.
fn reading(state: &State, now: SimTime) -> bool {
    state.camera.exposure.as_ref().is_some_and(|exposure| {
        exposure.end <= now && exposure.ready.is_some_and(|ready| ready > now)
    })
}

/// A setting outside 0 to `SETTING_MAX`.
fn check_setting(name: &str, value: i32) -> ASCOMResult<()> {
    if (0..=SETTING_MAX).contains(&value) {
        Ok(())
    } else {
        Err(ASCOMError::invalid_value(format!(
            "{name} {value} is outside 0 to {SETTING_MAX}"
        )))
    }
}

/// Connects or disconnects the camera. A connect from disconnected
/// starts at the full frame at bin 1 with no frame, and a disconnect
/// drops the frame.
pub fn connect(state: &mut State, connected: bool) {
    if connected && !state.camera.connected {
        state.camera.bin = 1;
        state.camera.region = state.full_region(1);
    }
    if connected != state.camera.connected {
        state.camera.exposure = None;
    }
    state.camera.connected = connected;
}

/// Starts an exposure at `now`: renders its frame and keeps it.
fn start(
    state: &mut State,
    now: SimTime,
    duration: Duration,
    readout: Duration,
) -> ASCOMResult<()> {
    if !(EXPOSURE_MIN..=EXPOSURE_MAX).contains(&duration) {
        return Err(ASCOMError::invalid_value(format!(
            "an exposure of {duration:?} is outside {EXPOSURE_MIN:?} to {EXPOSURE_MAX:?}"
        )));
    }
    if exposing(state, now) {
        return Err(ASCOMError::invalid_operation("an exposure is in progress"));
    }
    if reading(state, now) {
        return Err(ASCOMError::invalid_operation(
            "the last exposure is still reading out",
        ));
    }
    let bin = state.camera.bin;
    let region = state.camera.region;
    if !rig::fits(region, state.full_region(bin)) {
        return Err(ASCOMError::invalid_value(format!(
            "the region {region:?} leaves the sensor at bin {bin}"
        )));
    }
    let fault = state.camera.faults.take();
    if fault == Some(FrameFault::StartFails) {
        state.camera.exposure = None;
        debug!("simulated camera fails StartExposure, as scripted");
        return Err(ASCOMError::invalid_operation(
            "the exposure failed to start, as scripted",
        ));
    }
    // The frame is rendered from a copy of the mount run over the whole
    // exposure; the mount itself stays on the wall clock, so nothing runs
    // ahead of it, and an exposure that is dropped leaves nothing behind.
    let at = state.sim.now();
    let mount = state.sim.mount().clone();
    let frame = state.sim.expose(duration);
    *state.sim.mount_mut() = mount;
    let binned = rig::binned(&frame.pixels, bin, state.sim.camera().full_scale_adu);
    let narrower = u32::from(fault == Some(FrameFault::WrongSize));
    let end = at.after(duration);
    state.camera.exposure = Some(Exposure {
        end,
        ready: (fault != Some(FrameFault::NeverReady)).then(|| end.after(readout)),
        image: rig::cut(&binned, region, narrower),
    });
    state.exposures.push(ExposureRecord {
        at,
        duration,
        bin,
        region,
        gain: state.camera.gain,
        offset: state.camera.offset,
        truth: frame.truth,
    });
    debug!(
        ?duration,
        bin,
        ?region,
        ?fault,
        "simulated camera exposure started"
    );
    Ok(())
}

#[async_trait::async_trait]
impl Device for SimCamera {
    fn static_name(&self) -> &'static str {
        "guide-simulator camera"
    }

    fn unique_id(&self) -> &str {
        &self.rig.camera.unique_id
    }

    async fn connected(&self) -> ASCOMResult<bool> {
        Ok(self.rig.with(|state| state.camera.connected))
    }

    async fn set_connected(&self, connected: bool) -> ASCOMResult<()> {
        self.rig.with(|state| connect(state, connected));
        debug!(connected, "simulated camera Connected written");
        Ok(())
    }

    async fn description(&self) -> ASCOMResult<String> {
        Ok("guide-simulator's sensor behind an Alpaca Camera".to_owned())
    }

    async fn driver_info(&self) -> ASCOMResult<String> {
        Ok("guide-simulator-alpaca".to_owned())
    }

    async fn driver_version(&self) -> ASCOMResult<String> {
        Ok(env!("CARGO_PKG_VERSION").to_owned())
    }
}

#[async_trait::async_trait]
impl Camera for SimCamera {
    async fn camera_x_size(&self) -> ASCOMResult<u32> {
        self.session(|state, _| Ok(rig::dimension(state.sim.camera().width)))
    }

    async fn camera_y_size(&self) -> ASCOMResult<u32> {
        self.session(|state, _| Ok(rig::dimension(state.sim.camera().height)))
    }

    async fn pixel_size_x(&self) -> ASCOMResult<f64> {
        self.when_connected(self.rig.camera.pixel_size_um)
    }

    async fn pixel_size_y(&self) -> ASCOMResult<f64> {
        self.when_connected(self.rig.camera.pixel_size_um)
    }

    async fn sensor_type(&self) -> ASCOMResult<SensorType> {
        Ok(match self.sensor()? {
            SimSensor::Monochrome => SensorType::Monochrome,
            SimSensor::Rggb(_) => SensorType::RGGB,
        })
    }

    async fn bayer_offset_x(&self) -> ASCOMResult<u8> {
        match self.sensor()? {
            SimSensor::Monochrome => Err(ASCOMError::NOT_IMPLEMENTED),
            SimSensor::Rggb(_) => Ok(0),
        }
    }

    async fn bayer_offset_y(&self) -> ASCOMResult<u8> {
        self.bayer_offset_x().await
    }

    async fn max_adu(&self) -> ASCOMResult<u32> {
        self.when_connected(self.rig.camera.max_adu)
    }

    async fn max_bin_x(&self) -> ASCOMResult<u8> {
        self.when_connected(self.rig.camera.max_bin)
    }

    async fn max_bin_y(&self) -> ASCOMResult<u8> {
        self.max_bin_x().await
    }

    async fn can_asymmetric_bin(&self) -> ASCOMResult<bool> {
        self.when_connected(false)
    }

    async fn bin_x(&self) -> ASCOMResult<u8> {
        self.session(|state, _| Ok(state.camera.bin))
    }

    async fn set_bin_x(&self, bin_x: u8) -> ASCOMResult<()> {
        let max_bin = self.rig.camera.max_bin;
        self.session(|state, _| {
            if bin_x == 0 || bin_x > max_bin {
                return Err(ASCOMError::invalid_value(format!(
                    "BinX {bin_x} is outside 1 to {max_bin}"
                )));
            }
            state.camera.bin = bin_x;
            Ok(())
        })
    }

    async fn bin_y(&self) -> ASCOMResult<u8> {
        self.bin_x().await
    }

    async fn set_bin_y(&self, bin_y: u8) -> ASCOMResult<()> {
        self.set_bin_x(bin_y).await
    }

    async fn start_x(&self) -> ASCOMResult<u32> {
        self.session(|state, _| Ok(state.camera.region.x))
    }

    async fn set_start_x(&self, start_x: u32) -> ASCOMResult<()> {
        self.session(|state, _| {
            state.camera.region.x = start_x;
            Ok(())
        })
    }

    async fn start_y(&self) -> ASCOMResult<u32> {
        self.session(|state, _| Ok(state.camera.region.y))
    }

    async fn set_start_y(&self, start_y: u32) -> ASCOMResult<()> {
        self.session(|state, _| {
            state.camera.region.y = start_y;
            Ok(())
        })
    }

    async fn num_x(&self) -> ASCOMResult<u32> {
        self.session(|state, _| Ok(state.camera.region.width))
    }

    async fn set_num_x(&self, num_x: u32) -> ASCOMResult<()> {
        self.session(|state, _| {
            state.camera.region.width = num_x;
            Ok(())
        })
    }

    async fn num_y(&self) -> ASCOMResult<u32> {
        self.session(|state, _| Ok(state.camera.region.height))
    }

    async fn set_num_y(&self, num_y: u32) -> ASCOMResult<()> {
        self.session(|state, _| {
            state.camera.region.height = num_y;
            Ok(())
        })
    }

    async fn gain(&self) -> ASCOMResult<i32> {
        self.session(|state, _| Ok(state.camera.gain))
    }

    async fn set_gain(&self, gain: i32) -> ASCOMResult<()> {
        self.session(|state, _| {
            check_setting("Gain", gain)?;
            state.camera.gain = gain;
            Ok(())
        })
    }

    async fn gain_min(&self) -> ASCOMResult<i32> {
        self.when_connected(0)
    }

    async fn gain_max(&self) -> ASCOMResult<i32> {
        self.when_connected(SETTING_MAX)
    }

    async fn offset(&self) -> ASCOMResult<i32> {
        self.session(|state, _| Ok(state.camera.offset))
    }

    async fn set_offset(&self, offset: i32) -> ASCOMResult<()> {
        self.session(|state, _| {
            check_setting("Offset", offset)?;
            state.camera.offset = offset;
            Ok(())
        })
    }

    async fn offset_min(&self) -> ASCOMResult<i32> {
        self.when_connected(0)
    }

    async fn offset_max(&self) -> ASCOMResult<i32> {
        self.when_connected(SETTING_MAX)
    }

    async fn exposure_min(&self) -> ASCOMResult<Duration> {
        self.when_connected(EXPOSURE_MIN)
    }

    async fn exposure_max(&self) -> ASCOMResult<Duration> {
        self.when_connected(EXPOSURE_MAX)
    }

    async fn exposure_resolution(&self) -> ASCOMResult<Duration> {
        self.when_connected(EXPOSURE_MIN)
    }

    async fn has_shutter(&self) -> ASCOMResult<bool> {
        self.when_connected(false)
    }

    async fn can_abort_exposure(&self) -> ASCOMResult<bool> {
        self.when_connected(self.rig.camera.can_abort_exposure)
    }

    async fn can_stop_exposure(&self) -> ASCOMResult<bool> {
        self.when_connected(false)
    }

    async fn start_exposure(&self, duration: Duration, light: bool) -> ASCOMResult<()> {
        let readout = self.rig.camera.readout;
        self.session(|state, now| {
            if !light {
                return Err(ASCOMError::invalid_value("dark frames are not simulated"));
            }
            start(state, now, duration, readout)
        })
    }

    async fn abort_exposure(&self) -> ASCOMResult<()> {
        let can_abort = self.rig.camera.can_abort_exposure;
        self.session(|state, now| {
            state.aborts = state.aborts.saturating_add(1);
            if !can_abort {
                return Err(ASCOMError::NOT_IMPLEMENTED);
            }
            // Exposing, reading out, or an image that never comes: all are
            // dropped. A ready image is the idle camera's, and stays.
            if state.camera.exposure.is_some() && ready(state, now).is_none() {
                state.camera.exposure = None;
                debug!("simulated camera exposure aborted");
            }
            Ok(())
        })
    }

    async fn image_ready(&self) -> ASCOMResult<bool> {
        self.session(|state, now| Ok(ready(state, now).is_some()))
    }

    async fn image_array(&self) -> ASCOMResult<ImageArray> {
        self.session(|state, now| {
            let exposure = ready(state, now)
                .ok_or_else(|| ASCOMError::invalid_operation("no image is ready"))?;
            // Alpaca's image is indexed [x][y], the frame [[y, x]].
            Ok(ImageArray::from(exposure.image.clone().reversed_axes()))
        })
    }

    async fn camera_state(&self) -> ASCOMResult<CameraState> {
        self.session(|state, now| {
            Ok(match &state.camera.exposure {
                Some(exposure) if exposure.end > now => CameraState::Exposing,
                Some(exposure) if exposure.ready.is_none_or(|ready| ready > now) => {
                    CameraState::Reading
                }
                _ => CameraState::Idle,
            })
        })
    }

    async fn percent_completed(&self) -> ASCOMResult<u8> {
        self.session(|state, now| Ok(if exposing(state, now) { 0 } else { 100 }))
    }

    async fn sensor_name(&self) -> ASCOMResult<String> {
        self.when_connected("guide-simulator".to_owned())
    }
}
