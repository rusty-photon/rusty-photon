//! The service config (docs/services/guider.md § Configuration).
//!
//! The HTTP `server` block, the two device clients, the optics, and the
//! camera, mount, star, calibration, control, settling and guide-log
//! settings. Every bounded value is a newtype checked at deserialize, and
//! each block whose fields bound one another is checked whole as it is
//! read (parse-don't-validate), so a file the service cannot run with
//! fails the load naming the field (guider.md § Configuration validation
//! at startup).

use std::path::{Path, PathBuf};
use std::time::Duration;

use rp_auth::config::ClientAuthConfig;
use serde::{Deserialize, Serialize};

pub use rusty_photon_server_config::ServerConfig;

use crate::error::{GuiderError, Result};

/// The port the guider serves on unless its configuration says otherwise.
pub const DEFAULT_PORT: u16 = 11129;

/// A bounded config value: `$name($inner)` accepted when `$check(value)`
/// holds, refused at deserialize with a message naming the field. The
/// inner type is spelled twice because `serde`'s `try_from` takes the
/// type as a string literal.
macro_rules! bounded {
    (
        $(#[$meta:meta])*
        $name:ident($inner:ty as $inner_lit:literal),
        $field:literal,
        $check:expr,
        $message:literal
        $(, also: $extra:ident)?
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, $($extra,)? Serialize, Deserialize)]
        #[serde(try_from = $inner_lit)]
        pub struct $name($inner);

        impl TryFrom<$inner> for $name {
            type Error = String;

            fn try_from(value: $inner) -> std::result::Result<Self, Self::Error> {
                let check: fn($inner) -> bool = $check;
                if check(value) {
                    Ok(Self(value))
                } else {
                    Err(format!(
                        concat!($field, " must be ", $message, " (got {})"),
                        value
                    ))
                }
            }
        }

        impl $name {
            #[must_use]
            pub const fn get(self) -> $inner {
                self.0
            }
        }
    };
}

/// A duration of the config, written as humantime text (`"2s"`,
/// `"2500ms"`, `"90d"`), refused at deserialize with a message naming the
/// field when it does not parse, and when it is zero unless `zero` allows
/// it.
macro_rules! duration {
    (
        $(#[$meta:meta])*
        $name:ident,
        $field:literal,
        zero: $zero:literal
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(Duration);

        impl TryFrom<String> for $name {
            type Error = String;

            fn try_from(text: String) -> std::result::Result<Self, Self::Error> {
                parse_duration($field, &text, $zero).map(Self)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                duration_text(value.0)
            }
        }

        impl $name {
            #[must_use]
            pub const fn get(self) -> Duration {
                self.0
            }
        }
    };
}

/// `text` as the duration of `field`.
fn parse_duration(field: &str, text: &str, zero: bool) -> std::result::Result<Duration, String> {
    let duration = humantime::parse_duration(text)
        .map_err(|error| format!("{field} is not a duration: {error} (got {text:?})"))?;
    if duration.is_zero() && !zero {
        return Err(format!("{field} must be longer than zero (got {text:?})"));
    }
    Ok(duration)
}

/// A duration as the config writes it: a whole number of days as days, so
/// the default file says `"90d"` and not humantime's months, and anything
/// else as humantime writes it.
fn duration_text(duration: Duration) -> String {
    const DAY: u64 = 86_400;
    let seconds = duration.as_secs();
    if duration.subsec_nanos() == 0 && seconds != 0 && seconds.is_multiple_of(DAY) {
        format!("{}d", seconds / DAY)
    } else {
        humantime::format_duration(duration).to_string()
    }
}

bounded!(
    /// The guide scope's focal length, in millimetres.
    FocalLength(f64 as "f64"),
    "optics.focal_length_mm",
    |v| v.is_finite() && v > 0.0,
    "a finite number greater than 0"
);
bounded!(
    /// Hardware binning of a `Monochrome` camera.
    Binning(u8 as "u8"),
    "camera.binning",
    |v| v >= 1,
    "at least 1",
    also: Eq
);
bounded!(
    /// Consecutive failed frames that stop the loop.
    MaxFrameFailures(u32 as "u32"),
    "camera.max_frame_failures",
    |v| v >= 1,
    "at least 1",
    also: Eq
);
bounded!(
    /// Consecutive cycles with a failed mount call that stop the run.
    MaxMountFailures(u32 as "u32"),
    "mount.max_failures",
    |v| v >= 1,
    "at least 1",
    also: Eq
);
bounded!(
    /// The fewest usable guide stars a frame needs.
    MinStars(u32 as "u32"),
    "stars.min",
    |v| v >= 1,
    "at least 1",
    also: Eq
);
bounded!(
    /// How far from where it was last usable a star is looked for, in
    /// pixels.
    SearchRegion(f64 as "f64"),
    "stars.search_region_px",
    |v| v.is_finite() && v > 0.0,
    "a finite number greater than 0"
);
bounded!(
    /// The lowest signal-to-noise ratio of a usable star.
    MinSnr(f64 as "f64"),
    "stars.min_snr",
    |v| v.is_finite() && v > 0.0,
    "a finite number greater than 0"
);
bounded!(
    /// The largest change of a star's flux from its reference, as a
    /// fraction.
    MassChange(f64 as "f64"),
    "stars.mass_change",
    |v| v.is_finite() && v > 0.0 && v <= 1.0,
    "a number in (0, 1]"
);
bounded!(
    /// How far a calibration leg moves the stars, in pixels.
    CalibrationDistance(f64 as "f64"),
    "calibration.distance_px",
    |v| v.is_finite() && v > 0.0,
    "a finite number greater than 0"
);
bounded!(
    /// How far the stars move North before the Dec leg is measured, in
    /// pixels.
    BacklashClear(f64 as "f64"),
    "calibration.backlash_clear_px",
    |v| v.is_finite() && v > 0.0,
    "a finite number greater than 0"
);
bounded!(
    /// `control.ra.aggression`.
    RaAggression(f64 as "f64"),
    "control.ra.aggression",
    |v| v.is_finite() && v > 0.0 && v <= 2.0,
    "a number in (0, 2]"
);
bounded!(
    /// `control.ra.hysteresis`.
    Hysteresis(f64 as "f64"),
    "control.ra.hysteresis",
    |v| v.is_finite() && (0.0..1.0).contains(&v),
    "a number in [0, 1)"
);
bounded!(
    /// `control.dec.aggression`.
    DecAggression(f64 as "f64"),
    "control.dec.aggression",
    |v| v.is_finite() && v > 0.0 && v <= 2.0,
    "a number in (0, 2]"
);
bounded!(
    /// Frames the Dec error stays across the lock before Dec reverses.
    ReversalFrames(u32 as "u32"),
    "control.dec.reversal_frames",
    |v| v >= 1,
    "at least 1",
    also: Eq
);
bounded!(
    /// `control.min_move_px`, when set.
    MinMove(f64 as "f64"),
    "control.min_move_px",
    |v| v.is_finite() && v >= 0.0,
    "a finite number of at least 0"
);
bounded!(
    /// Consecutive one-way corrections the error does not follow before
    /// the run stops.
    RunawayFrames(u32 as "u32"),
    "control.runaway.frames",
    |v| v >= 1,
    "at least 1",
    also: Eq
);
bounded!(
    /// The error each of the runaway frames must reach, in pixels.
    RunawayMinError(f64 as "f64"),
    "control.runaway.min_error_px",
    |v| v.is_finite() && v >= 0.0,
    "a finite number of at least 0"
);
bounded!(
    /// The settle tolerance, in pixels.
    SettlePixels(f64 as "f64"),
    "settling.pixels",
    |v| v.is_finite() && v > 0.0,
    "a finite number greater than 0"
);

duration!(
    /// How often `Connected` and each server's device list are read.
    PollInterval,
    "devices.poll_interval",
    zero: false
);
duration!(
    /// The exposure of every frame.
    Exposure,
    "camera.exposure",
    zero: false
);
duration!(
    /// How long after its exposure a frame may take to arrive.
    FrameTimeout,
    "camera.frame_timeout",
    zero: false
);
duration!(
    /// The shortest correction that is sent.
    MinPulse,
    "mount.min_pulse",
    zero: false
);
duration!(
    /// The longest pulse.
    MaxPulse,
    "mount.max_pulse",
    zero: false
);
duration!(
    /// How long after its duration a pulse may still be in progress.
    PulseTimeout,
    "mount.pulse_timeout",
    zero: false
);
duration!(
    /// The step for a mount that reports no guide rate.
    FallbackStep,
    "calibration.fallback_step",
    zero: false
);
duration!(
    /// The longest a calibration may take.
    CalibrationTimeout,
    "calibration.timeout",
    zero: false
);
duration!(
    /// How long the stars must stay inside the tolerance. Zero settles on
    /// the first frame inside it.
    SettleTime,
    "settling.time",
    zero: true
);
duration!(
    /// How long after the start the stars may take to settle.
    SettleTimeout,
    "settling.timeout",
    zero: false
);
duration!(
    /// How long `guiding/stop` waits for the loop to end.
    StopTimeout,
    "stop_timeout",
    zero: false
);
duration!(
    /// How old a guide log may grow before it is removed.
    Retention,
    "guide_log.retention",
    zero: false
);

/// The guider's configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// The listener, its TLS and its Basic Auth.
    pub server: ServerConfig,
    /// The only root the device clients trust for an `https://` address,
    /// when set.
    pub ca_cert: Option<PathBuf>,
    /// Where the calibration store and the guide logs are kept, when set;
    /// the platform state directory otherwise.
    pub state_dir: Option<PathBuf>,
    pub devices: DevicesConfig,
    pub optics: OpticsConfig,
    pub camera: CameraConfig,
    pub mount: MountConfig,
    pub stars: StarsConfig,
    pub calibration: CalibrationConfig,
    pub control: ControlConfig,
    pub settling: SettlingConfig,
    pub stop_timeout: StopTimeout,
    pub guide_log: GuideLogConfig,
}

impl Default for Config {
    /// The default file: no camera, no mount and no focal length, so the
    /// service runs and reports what is missing (guider.md § Starting
    /// idle).
    fn default() -> Self {
        Self {
            server: ServerConfig::new(DEFAULT_PORT),
            ca_cert: None,
            state_dir: None,
            devices: DevicesConfig::default(),
            optics: OpticsConfig::default(),
            camera: CameraConfig::default(),
            mount: MountConfig::default(),
            stars: StarsConfig::default(),
            calibration: CalibrationConfig::default(),
            control: ControlConfig::default(),
            settling: SettlingConfig::default(),
            stop_timeout: StopTimeout(Duration::from_secs(10)),
            guide_log: GuideLogConfig::default(),
        }
    }
}

impl rusty_photon_config::ConfigFile for Config {}

impl Config {
    /// The CA certificate the device clients trust, when set.
    #[must_use]
    pub fn ca_cert_path(&self) -> Option<&Path> {
        self.ca_cert.as_deref()
    }

    /// Where the calibration store and the guide logs live: `state_dir`
    /// when set, otherwise the platform state directory (guider.md § State
    /// directory).
    ///
    /// # Errors
    ///
    /// Returns [`GuiderError::Config`] if no `state_dir` is set and the
    /// platform state directory cannot be resolved.
    pub fn state_dir(&self) -> Result<PathBuf> {
        self.state_dir
            .as_ref()
            .map_or_else(default_state_dir, |path| Ok(path.clone()))
    }
}

/// The Linux state directory, provisioned and owned by the packaged
/// unit's systemd `StateDirectory=`.
#[cfg(not(any(windows, target_os = "macos")))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the macOS / Windows variant can fail to resolve the platform directory; one signature for both"
)]
fn default_state_dir() -> Result<PathBuf> {
    Ok(PathBuf::from("/var/lib/rusty-photon/guider"))
}

/// macOS and Windows keep state beside the config, as `rp` does:
/// `~/Library/Application Support/rusty-photon/guider/` and
/// `%PROGRAMDATA%\rusty-photon\guider\`.
#[cfg(any(windows, target_os = "macos"))]
fn default_state_dir() -> Result<PathBuf> {
    rusty_photon_config::default_config_dir()
        .map(|dir| dir.join("guider"))
        .map_err(|e| {
            GuiderError::Config(format!("cannot resolve the platform state directory: {e}"))
        })
}

/// The camera and the mount the guider is a client of, and how often it
/// reads them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DevicesConfig {
    /// The guide camera; `null` until an operator names it.
    pub camera: Option<DeviceAddress>,
    /// The mount; `null` until an operator names it.
    pub mount: Option<DeviceAddress>,
    pub poll_interval: PollInterval,
}

impl Default for DevicesConfig {
    fn default() -> Self {
        Self {
            camera: None,
            mount: None,
            poll_interval: PollInterval(Duration::from_secs(10)),
        }
    }
}

/// Where a device is: the server, the device's position among that
/// server's devices of its kind, and the server's credential.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceAddress {
    pub alpaca_url: DeviceUrl,
    #[serde(default)]
    pub device_number: u32,
    #[serde(default)]
    pub auth: Option<ClientAuthConfig>,
}

/// An `http://` or `https://` URL of an Alpaca server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct DeviceUrl(String);

impl TryFrom<String> for DeviceUrl {
    type Error = String;

    fn try_from(text: String) -> std::result::Result<Self, Self::Error> {
        match reqwest::Url::parse(&text) {
            Ok(url) if matches!(url.scheme(), "http" | "https") => Ok(Self(text)),
            _ => Err(format!(
                "alpaca_url must be an http:// or https:// URL (got {text:?})"
            )),
        }
    }
}

impl From<DeviceUrl> for String {
    fn from(url: DeviceUrl) -> Self {
        url.0
    }
}

impl DeviceUrl {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The guide scope.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OpticsConfig {
    /// `null` until an operator names it.
    pub focal_length_mm: Option<FocalLength>,
}

/// The frames the guider takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CameraConfig {
    pub exposure: Exposure,
    pub binning: Binning,
    /// Written to the camera when set; `null` leaves its value alone.
    pub gain: Option<i32>,
    /// Written to the camera when set; `null` leaves its value alone.
    pub offset: Option<i32>,
    pub frame_timeout: FrameTimeout,
    pub max_frame_failures: MaxFrameFailures,
}

impl Default for CameraConfig {
    fn default() -> Self {
        Self {
            exposure: Exposure(Duration::from_secs(2)),
            binning: Binning(1),
            gain: None,
            offset: None,
            frame_timeout: FrameTimeout(Duration::from_secs(10)),
            max_frame_failures: MaxFrameFailures(3),
        }
    }
}

/// The pulses. `min_pulse` is shorter than `max_pulse`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "MountFields")]
pub struct MountConfig {
    pub min_pulse: MinPulse,
    pub max_pulse: MaxPulse,
    pub pulse_timeout: PulseTimeout,
    pub max_failures: MaxMountFailures,
}

impl Default for MountConfig {
    fn default() -> Self {
        Self {
            min_pulse: MinPulse(Duration::from_millis(20)),
            max_pulse: MaxPulse(Duration::from_millis(2_500)),
            pulse_timeout: PulseTimeout(Duration::from_secs(2)),
            max_failures: MaxMountFailures(3),
        }
    }
}

/// [`MountConfig`] as the file holds it, before the two pulse bounds are
/// held to each other.
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct MountFields {
    min_pulse: MinPulse,
    max_pulse: MaxPulse,
    pulse_timeout: PulseTimeout,
    max_failures: MaxMountFailures,
}

impl Default for MountFields {
    fn default() -> Self {
        let MountConfig {
            min_pulse,
            max_pulse,
            pulse_timeout,
            max_failures,
        } = MountConfig::default();
        Self {
            min_pulse,
            max_pulse,
            pulse_timeout,
            max_failures,
        }
    }
}

impl TryFrom<MountFields> for MountConfig {
    type Error = String;

    fn try_from(fields: MountFields) -> std::result::Result<Self, Self::Error> {
        if fields.min_pulse.get() >= fields.max_pulse.get() {
            return Err(format!(
                "mount.min_pulse must be shorter than mount.max_pulse (got {} and {})",
                duration_text(fields.min_pulse.get()),
                duration_text(fields.max_pulse.get())
            ));
        }
        Ok(Self {
            min_pulse: fields.min_pulse,
            max_pulse: fields.max_pulse,
            pulse_timeout: fields.pulse_timeout,
            max_failures: fields.max_failures,
        })
    }
}

/// The guide stars. `min` is at most `max`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "StarsFields")]
pub struct StarsConfig {
    /// The most guide stars kept.
    pub max: u32,
    pub min: MinStars,
    pub search_region_px: SearchRegion,
    pub min_snr: MinSnr,
    pub mass_change: MassChange,
}

impl Default for StarsConfig {
    fn default() -> Self {
        Self {
            max: 8,
            min: MinStars(1),
            search_region_px: SearchRegion(15.0),
            min_snr: MinSnr(3.0),
            mass_change: MassChange(0.5),
        }
    }
}

/// [`StarsConfig`] as the file holds it, before `min` is held to `max`.
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct StarsFields {
    max: u32,
    min: MinStars,
    search_region_px: SearchRegion,
    min_snr: MinSnr,
    mass_change: MassChange,
}

impl Default for StarsFields {
    fn default() -> Self {
        let StarsConfig {
            max,
            min,
            search_region_px,
            min_snr,
            mass_change,
        } = StarsConfig::default();
        Self {
            max,
            min,
            search_region_px,
            min_snr,
            mass_change,
        }
    }
}

impl TryFrom<StarsFields> for StarsConfig {
    type Error = String;

    fn try_from(fields: StarsFields) -> std::result::Result<Self, Self::Error> {
        if fields.min.get() > fields.max {
            return Err(format!(
                "stars.min must be at most stars.max (got {} and {})",
                fields.min.get(),
                fields.max
            ));
        }
        Ok(Self {
            max: fields.max,
            min: fields.min,
            search_region_px: fields.search_region_px,
            min_snr: fields.min_snr,
            mass_change: fields.mass_change,
        })
    }
}

/// The calibration. `steps` is at least 4 and at most `max_steps`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "CalibrationFields")]
pub struct CalibrationConfig {
    pub distance_px: CalibrationDistance,
    /// The steps a leg aims for.
    pub steps: u32,
    /// The steps after which a leg that has not covered its distance
    /// fails.
    pub max_steps: u32,
    pub backlash_clear_px: BacklashClear,
    pub fallback_step: FallbackStep,
    pub timeout: CalibrationTimeout,
}

impl Default for CalibrationConfig {
    fn default() -> Self {
        Self {
            distance_px: CalibrationDistance(25.0),
            steps: 12,
            max_steps: 60,
            backlash_clear_px: BacklashClear(3.0),
            fallback_step: FallbackStep(Duration::from_millis(750)),
            timeout: CalibrationTimeout(Duration::from_mins(10)),
        }
    }
}

/// [`CalibrationConfig`] as the file holds it, before `steps` is held to
/// its bounds.
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct CalibrationFields {
    distance_px: CalibrationDistance,
    steps: u32,
    max_steps: u32,
    backlash_clear_px: BacklashClear,
    fallback_step: FallbackStep,
    timeout: CalibrationTimeout,
}

impl Default for CalibrationFields {
    fn default() -> Self {
        let CalibrationConfig {
            distance_px,
            steps,
            max_steps,
            backlash_clear_px,
            fallback_step,
            timeout,
        } = CalibrationConfig::default();
        Self {
            distance_px,
            steps,
            max_steps,
            backlash_clear_px,
            fallback_step,
            timeout,
        }
    }
}

/// The fewest steps a calibration leg may aim for.
const MIN_CALIBRATION_STEPS: u32 = 4;

impl TryFrom<CalibrationFields> for CalibrationConfig {
    type Error = String;

    fn try_from(fields: CalibrationFields) -> std::result::Result<Self, Self::Error> {
        if fields.steps < MIN_CALIBRATION_STEPS {
            return Err(format!(
                "calibration.steps must be at least {MIN_CALIBRATION_STEPS} (got {})",
                fields.steps
            ));
        }
        if fields.steps > fields.max_steps {
            return Err(format!(
                "calibration.steps must be at most calibration.max_steps (got {} and {})",
                fields.steps, fields.max_steps
            ));
        }
        Ok(Self {
            distance_px: fields.distance_px,
            steps: fields.steps,
            max_steps: fields.max_steps,
            backlash_clear_px: fields.backlash_clear_px,
            fallback_step: fields.fallback_step,
            timeout: fields.timeout,
        })
    }
}

/// The controllers (guider.md § Control).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ControlConfig {
    pub ra: RaControl,
    pub dec: DecControl,
    /// `null` uses the formula of guider.md § Control.
    pub min_move_px: Option<MinMove>,
    pub runaway: RunawayConfig,
}

/// `control.ra`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RaControl {
    pub aggression: RaAggression,
    pub hysteresis: Hysteresis,
}

impl Default for RaControl {
    fn default() -> Self {
        Self {
            aggression: RaAggression(0.7),
            hysteresis: Hysteresis(0.1),
        }
    }
}

/// `control.dec`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DecControl {
    pub mode: DecMode,
    pub aggression: DecAggression,
    pub reversal_frames: ReversalFrames,
}

impl Default for DecControl {
    fn default() -> Self {
        Self {
            mode: DecMode::Auto,
            aggression: DecAggression(1.0),
            reversal_frames: ReversalFrames(3),
        }
    }
}

/// How Dec is corrected (`control.dec.mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "&'static str")]
pub enum DecMode {
    /// In both directions.
    Auto,
    /// North only.
    North,
    /// South only.
    South,
    /// Not at all.
    Off,
}

impl TryFrom<String> for DecMode {
    type Error = String;

    fn try_from(text: String) -> std::result::Result<Self, Self::Error> {
        match text.as_str() {
            "auto" => Ok(Self::Auto),
            "north" => Ok(Self::North),
            "south" => Ok(Self::South),
            "off" => Ok(Self::Off),
            _ => Err(format!(
                "control.dec.mode must be \"auto\", \"north\", \"south\" or \"off\" (got {text:?})"
            )),
        }
    }
}

impl From<DecMode> for &'static str {
    fn from(mode: DecMode) -> Self {
        match mode {
            DecMode::Auto => "auto",
            DecMode::North => "north",
            DecMode::South => "south",
            DecMode::Off => "off",
        }
    }
}

/// `control.runaway`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunawayConfig {
    pub frames: RunawayFrames,
    pub min_error_px: RunawayMinError,
}

impl Default for RunawayConfig {
    fn default() -> Self {
        Self {
            frames: RunawayFrames(15),
            min_error_px: RunawayMinError(3.0),
        }
    }
}

/// The settle defaults of `guiding/start` and `dither`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SettlingConfig {
    pub pixels: SettlePixels,
    pub time: SettleTime,
    pub timeout: SettleTimeout,
}

impl Default for SettlingConfig {
    fn default() -> Self {
        Self {
            pixels: SettlePixels(0.5),
            time: SettleTime(Duration::from_secs(10)),
            timeout: SettleTimeout(Duration::from_mins(1)),
        }
    }
}

/// The guide log (guider.md § Guide log).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GuideLogConfig {
    pub enabled: bool,
    pub retention: Retention,
}

impl Default for GuideLogConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            retention: Retention(Duration::from_hours(90 * 24)),
        }
    }
}

/// Read the config at `path`.
///
/// # Errors
///
/// Returns [`GuiderError::Config`] if the file is missing, is not valid
/// JSON, or is not a valid configuration; the message names the file and
/// the field.
pub fn load_config(path: &Path) -> Result<Config> {
    rusty_photon_config::load_file::<Config>(path)
        .map_err(|e| GuiderError::Config(e.to_string()))?
        .ok_or_else(|| {
            GuiderError::Config(format!("config file {} does not exist", path.display()))
        })
}

/// The default config as the JSON a first start writes.
///
/// # Errors
///
/// Returns [`GuiderError::Config`] if the default does not serialize.
pub fn default_scaffold() -> Result<serde_json::Value> {
    serde_json::to_value(Config::default())
        .map_err(|e| GuiderError::Config(format!("the default config does not serialize: {e}")))
}

/// CLI overrides layered over the file config after load: `--port` and
/// `--bind-address` pin `server.port` / `server.bind_address` over
/// whatever the file supplied.
#[derive(Debug, Clone, Copy, Default)]
pub struct CliOverrides {
    /// `--port` → `server.port`.
    pub port: Option<u16>,
    /// `--bind-address` → `server.bind_address`.
    pub bind_address: Option<std::net::IpAddr>,
}

impl CliOverrides {
    /// Apply the overrides onto `config` in place.
    pub const fn apply(&self, config: &mut Config) {
        if let Some(port) = self.port {
            config.server.port = port;
        }
        if let Some(bind_address) = self.bind_address {
            config.server.bind_address = bind_address;
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use serde_json::json;

    use super::*;

    /// The default file holds every default of guider.md § Configuration,
    /// with no camera, no mount and no focal length.
    #[test]
    fn the_default_file_holds_the_documented_defaults() {
        let scaffold = default_scaffold().unwrap();
        assert_eq!(
            scaffold,
            json!({
                "server": { "port": 11129, "bind_address": "0.0.0.0" },
                "ca_cert": null,
                "state_dir": null,
                "devices": { "camera": null, "mount": null, "poll_interval": "10s" },
                "optics": { "focal_length_mm": null },
                "camera": {
                    "exposure": "2s",
                    "binning": 1,
                    "gain": null,
                    "offset": null,
                    "frame_timeout": "10s",
                    "max_frame_failures": 3
                },
                "mount": {
                    "min_pulse": "20ms",
                    "max_pulse": "2s 500ms",
                    "pulse_timeout": "2s",
                    "max_failures": 3
                },
                "stars": {
                    "max": 8,
                    "min": 1,
                    "search_region_px": 15.0,
                    "min_snr": 3.0,
                    "mass_change": 0.5
                },
                "calibration": {
                    "distance_px": 25.0,
                    "steps": 12,
                    "max_steps": 60,
                    "backlash_clear_px": 3.0,
                    "fallback_step": "750ms",
                    "timeout": "10m"
                },
                "control": {
                    "ra": { "aggression": 0.7, "hysteresis": 0.1 },
                    "dec": { "mode": "auto", "aggression": 1.0, "reversal_frames": 3 },
                    "min_move_px": null,
                    "runaway": { "frames": 15, "min_error_px": 3.0 }
                },
                "settling": { "pixels": 0.5, "time": "10s", "timeout": "1m" },
                "stop_timeout": "10s",
                "guide_log": { "enabled": true, "retention": "90d" }
            })
        );
    }

    /// What a first start writes loads back as the defaults.
    #[test]
    fn the_default_file_loads_back() {
        let config: Config = serde_json::from_value(default_scaffold().unwrap()).unwrap();
        assert_eq!(config.camera, CameraConfig::default());
        assert_eq!(config.mount, MountConfig::default());
        assert_eq!(config.stars, StarsConfig::default());
        assert_eq!(config.calibration, CalibrationConfig::default());
        assert_eq!(config.control, ControlConfig::default());
        assert_eq!(config.settling, SettlingConfig::default());
        assert_eq!(config.guide_log, GuideLogConfig::default());
        assert_eq!(config.stop_timeout.get(), Duration::from_secs(10));
        assert_eq!(config.devices.poll_interval.get(), Duration::from_secs(10));
    }

    /// The example of guider.md § Configuration loads, its durations
    /// written as the document writes them.
    #[test]
    fn the_documented_example_loads() {
        let config: Config = serde_json::from_value(json!({
            "server": { "port": 11129, "bind_address": "0.0.0.0", "tls": null, "auth": null },
            "ca_cert": null,
            "state_dir": null,
            "devices": {
                "camera": { "alpaca_url": "http://localhost:11121", "device_number": 0, "auth": null },
                "mount": { "alpaca_url": "http://localhost:11117", "device_number": 0, "auth": null },
                "poll_interval": "10s"
            },
            "optics": { "focal_length_mm": 200.0 },
            "camera": { "exposure": "2s", "binning": 1, "gain": null, "offset": null, "frame_timeout": "10s", "max_frame_failures": 3 },
            "mount": { "min_pulse": "20ms", "max_pulse": "2500ms", "pulse_timeout": "2s", "max_failures": 3 },
            "stars": { "max": 8, "min": 1, "search_region_px": 15, "min_snr": 3.0, "mass_change": 0.5 },
            "calibration": { "distance_px": 25, "steps": 12, "max_steps": 60, "backlash_clear_px": 3, "fallback_step": "750ms", "timeout": "10m" },
            "control": {
                "ra": { "aggression": 0.7, "hysteresis": 0.1 },
                "dec": { "mode": "auto", "aggression": 1.0, "reversal_frames": 3 },
                "min_move_px": null,
                "runaway": { "frames": 15, "min_error_px": 3.0 }
            },
            "settling": { "pixels": 0.5, "time": "10s", "timeout": "60s" },
            "stop_timeout": "10s",
            "guide_log": { "enabled": true, "retention": "90d" }
        }))
        .unwrap();
        assert_eq!(config.mount, MountConfig::default());
        assert_eq!(config.settling, SettlingConfig::default());
        assert_eq!(
            config.optics.focal_length_mm.map(FocalLength::get),
            Some(200.0)
        );
        let camera = config.devices.camera.unwrap();
        assert_eq!(camera.alpaca_url.as_str(), "http://localhost:11121");
        assert_eq!(camera.device_number, 0);
    }

    #[test]
    fn a_device_address_takes_device_number_0_and_no_auth_by_default() {
        let address: DeviceAddress =
            serde_json::from_value(json!({ "alpaca_url": "https://rig.local:11121" })).unwrap();
        assert_eq!(address.device_number, 0);
        assert!(address.auth.is_none());
    }

    #[test]
    fn a_device_address_refuses_a_field_it_does_not_have() {
        let error = serde_json::from_value::<DeviceAddress>(
            json!({ "alpaca_url": "http://rig:11121", "number": 1 }),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("unknown field `number`"),
            "{error}"
        );
    }

    #[test]
    fn whole_days_are_written_as_days() {
        assert_eq!(duration_text(Duration::from_hours(90 * 24)), "90d");
        assert_eq!(duration_text(Duration::from_hours(24)), "1d");
    }

    #[test]
    fn other_durations_are_written_as_humantime_writes_them() {
        assert_eq!(duration_text(Duration::from_secs(2)), "2s");
        assert_eq!(duration_text(Duration::from_millis(2_500)), "2s 500ms");
        assert_eq!(duration_text(Duration::from_hours(25)), "1day 1h");
        assert_eq!(duration_text(Duration::ZERO), "0s");
        assert_eq!(
            duration_text(Duration::from_hours(24) + Duration::from_millis(1)),
            "1day 1ms"
        );
    }

    #[test]
    fn a_duration_that_does_not_parse_names_its_field() {
        let error = parse_duration("camera.exposure", "2 parsecs", false).unwrap_err();
        assert!(
            error.starts_with("camera.exposure is not a duration: "),
            "{error}"
        );
    }

    #[test]
    fn a_zero_duration_is_refused_unless_zero_is_allowed() {
        assert_eq!(
            parse_duration("stop_timeout", "0s", false).unwrap_err(),
            "stop_timeout must be longer than zero (got \"0s\")"
        );
        assert_eq!(
            parse_duration("settling.time", "0s", true).unwrap(),
            Duration::ZERO
        );
    }

    #[test]
    fn a_pulse_floor_just_under_the_cap_is_accepted() {
        let mount: MountConfig =
            serde_json::from_value(json!({ "min_pulse": "2499ms", "max_pulse": "2500ms" }))
                .unwrap();
        assert_eq!(mount.min_pulse.get(), Duration::from_millis(2_499));
    }

    #[test]
    fn as_many_minimum_stars_as_the_maximum_is_accepted() {
        let stars: StarsConfig = serde_json::from_value(json!({ "min": 8, "max": 8 })).unwrap();
        assert_eq!(stars.min.get(), 8);
    }

    #[test]
    fn calibration_steps_at_their_bounds_are_accepted() {
        let low: CalibrationConfig = serde_json::from_value(json!({ "steps": 4 })).unwrap();
        assert_eq!(low.steps, 4);
        let high: CalibrationConfig =
            serde_json::from_value(json!({ "steps": 60, "max_steps": 60 })).unwrap();
        assert_eq!(high.steps, 60);
    }

    #[test]
    fn the_bounds_of_each_range_are_held_exactly() {
        assert!(MassChange::try_from(1.0).is_ok());
        assert!(RaAggression::try_from(2.0).is_ok());
        assert!(DecAggression::try_from(2.0).is_ok());
        assert!(Hysteresis::try_from(0.0).is_ok());
        assert!(MinMove::try_from(0.0).is_ok());
        assert!(RunawayMinError::try_from(0.0).is_ok());
        assert_eq!(
            Hysteresis::try_from(1.0).unwrap_err(),
            "control.ra.hysteresis must be a number in [0, 1) (got 1)"
        );
    }

    #[test]
    fn every_dec_mode_reads_and_writes_as_its_name() {
        for name in ["auto", "north", "south", "off"] {
            let mode: DecMode = serde_json::from_value(json!(name)).unwrap();
            assert_eq!(serde_json::to_value(mode).unwrap(), json!(name));
        }
    }

    #[test]
    fn a_missing_file_is_an_error_naming_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("guider.json");
        let error = load_config(&path).unwrap_err().to_string();
        assert!(error.contains("does not exist"), "{error}");
        assert!(error.contains("guider.json"), "{error}");
    }

    #[test]
    fn the_state_directory_is_state_dir_when_set() {
        let config: Config =
            serde_json::from_value(json!({ "state_dir": "/srv/guider-state" })).unwrap();
        assert_eq!(
            config.state_dir().unwrap(),
            PathBuf::from("/srv/guider-state")
        );
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    #[test]
    fn the_linux_state_directory_is_the_units_state_directory() {
        assert_eq!(
            Config::default().state_dir().unwrap(),
            PathBuf::from("/var/lib/rusty-photon/guider")
        );
    }

    #[test]
    fn cli_overrides_pin_the_port_and_the_bind_address() {
        let mut config = Config::default();
        CliOverrides {
            port: Some(0),
            bind_address: Some("127.0.0.1".parse().unwrap()),
        }
        .apply(&mut config);
        assert_eq!(config.server.port, 0);
        assert_eq!(config.server.bind_address.to_string(), "127.0.0.1");
    }

    #[test]
    fn no_overrides_leave_the_server_block_alone() {
        let mut config = Config::default();
        CliOverrides::default().apply(&mut config);
        assert_eq!(config.server.port, DEFAULT_PORT);
        assert_eq!(config.server.bind_address.to_string(), "0.0.0.0");
    }
}
