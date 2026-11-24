//! What the devices are configured with, and the records they keep of
//! what they were asked.

use std::time::Duration;

use guide_simulator::{GuideDirection, SimConfig, SimTime};

/// The simulation and the two devices in front of it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DevicesConfig {
    /// The mount, the sky and the sensor the devices show.
    pub sim: SimConfig,
    /// The camera's Alpaca side.
    pub camera: CameraDeviceConfig,
    /// The mount's Alpaca side.
    pub mount: MountDeviceConfig,
}

/// What the camera reports beside the simulated sensor.
#[derive(Debug, Clone, PartialEq)]
pub struct CameraDeviceConfig {
    /// The `UniqueID` its server lists for it.
    pub unique_id: String,
    /// `PixelSizeX` and `PixelSizeY`, in micrometres.
    pub pixel_size_um: f64,
    /// `MaxADU`.
    pub max_adu: u32,
    /// `MaxBinX` and `MaxBinY`; at least 1.
    pub max_bin: u8,
    /// `CanAbortExposure`.
    pub can_abort_exposure: bool,
    /// How long after an exposure's end `ImageReady` turns true.
    pub readout: Duration,
}

impl Default for CameraDeviceConfig {
    /// 2 µm pixels, which at a focal length of 206.265 mm span the
    /// simulated sensor's default 2″; a 16-bit `MaxADU`; binning up to 2;
    /// an exposure that can be aborted, and an image ready at the
    /// exposure's end.
    fn default() -> Self {
        Self {
            unique_id: "guide-simulator-camera".to_owned(),
            pixel_size_um: 2.0,
            max_adu: u32::from(u16::MAX),
            max_bin: 2,
            can_abort_exposure: true,
            readout: Duration::ZERO,
        }
    }
}

/// What the mount reports beside the simulated mount.
#[derive(Debug, Clone, PartialEq)]
pub struct MountDeviceConfig {
    /// The `UniqueID` its server lists for it.
    pub unique_id: String,
    /// The right ascension it points at the start, in hours; the
    /// sidereal time at the start is the same, so it starts on the
    /// meridian.
    pub right_ascension_hours: f64,
    /// `Altitude`, in degrees.
    pub altitude_deg: f64,
    /// `Azimuth`, in degrees.
    pub azimuth_deg: f64,
    /// `CanPulseGuide` at the start.
    pub can_pulse_guide: bool,
    /// Whether `GuideRateRightAscension` and `GuideRateDeclination`
    /// answer; `NOT_IMPLEMENTED` when not.
    pub reports_guide_rates: bool,
    /// Whether `SideOfPier` reports the side; `pierUnknown` when not.
    pub reports_pier_side: bool,
}

impl Default for MountDeviceConfig {
    /// A mount at 6 h of right ascension, 60° up in the south, that
    /// pulses and reports its guide rates and its pier side.
    fn default() -> Self {
        Self {
            unique_id: "guide-simulator-mount".to_owned(),
            right_ascension_hours: 6.0,
            altitude_deg: 60.0,
            azimuth_deg: 180.0,
            can_pulse_guide: true,
            reports_guide_rates: true,
            reports_pier_side: true,
        }
    }
}

/// One of the two devices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Device {
    /// The camera, on the camera's server.
    Camera,
    /// The mount, on the mount's server.
    Mount,
}

/// How a scripted exposure fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameFault {
    /// `StartExposure` answers an error, and no exposure starts.
    StartFails,
    /// The exposure runs, and `ImageReady` never turns true.
    NeverReady,
    /// `ImageArray` answers a frame one pixel narrower than the region.
    WrongSize,
}

/// A region of the binned sensor, in binned pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Region {
    /// `StartX`.
    pub x: u32,
    /// `StartY`.
    pub y: u32,
    /// `NumX`.
    pub width: u32,
    /// `NumY`.
    pub height: u32,
}

/// A pulse the mount accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PulseRecord {
    /// Its direction.
    pub direction: GuideDirection,
    /// Its length, as asked.
    pub duration: Duration,
    /// When it started, in simulated time.
    pub at: SimTime,
}

/// An exposure the camera started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExposureRecord {
    /// When it started, in simulated time.
    pub at: SimTime,
    /// Its length.
    pub duration: Duration,
    /// The binning it was taken at.
    pub bin: u8,
    /// The region it was taken of.
    pub region: Region,
    /// `Gain` when it started.
    pub gain: i32,
    /// `Offset` when it started.
    pub offset: i32,
}
