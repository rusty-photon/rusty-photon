//! Simulation support for QHYCCD cameras
//!
//! This module provides simulated camera and filter wheel support, allowing
//! library users to develop and test applications without physical QHYCCD hardware.
//!
//! Folded into a single file (was a `simulation/` subtree) as part of the
//! zwo/svbony convention alignment: the simulated device state is held directly
//! by a [`Camera`](crate::Camera) under `#[cfg(feature = "simulation")]`, not via
//! a runtime backend enum.
//!
//! # Example
//!
//! ```no_run
//! use qhyccd_rs::simulation::{SimulatedCameraConfig, ImagePattern, ImageGenerator};
//!
//! // Create a custom camera configuration
//! let config = SimulatedCameraConfig::default()
//!     .with_id("TEST-001")
//!     .with_filter_wheel(5)
//!     .with_cooler();
//!
//! // Create an image generator for testing
//! let generator = ImageGenerator::new(ImagePattern::StarField)
//!     .with_noise_level(0.02);
//! ```

use crate::quantize;
use crate::{BayerPattern, CCDChipArea, CCDChipInfo, ControlType, QHYError, Result, StreamMode};
use rand::{Rng, RngExt};
use rayon::prelude::*;
use std::collections::HashMap;
use std::time::Instant;

// ===== Simulated camera configuration =====

/// Configuration for a simulated camera
///
/// # Example
/// ```no_run
/// use qhyccd_rs::simulation::SimulatedCameraConfig;
///
/// let config = SimulatedCameraConfig::default()
///     .with_filter_wheel(5)
///     .with_cooler();
/// ```
#[derive(Debug, Clone)]
pub struct SimulatedCameraConfig {
    /// Camera identifier (e.g., "SIM-001")
    pub id: String,
    /// Model name (e.g., "QHY178M-SIM")
    pub model: String,
    /// CCD/CMOS chip information
    pub chip_info: CCDChipInfo,
    /// Effective imaging area
    pub effective_area: CCDChipArea,
    /// Overscan area (if any)
    pub overscan_area: CCDChipArea,
    /// Supported controls with their (min, max, step) values
    pub supported_controls: HashMap<ControlType, (f64, f64, f64)>,
    /// Number of filter wheel slots (0 = no filter wheel)
    pub filter_wheel_slots: u32,
    /// Whether the camera has a cooler
    pub has_cooler: bool,
    /// Bayer pattern for color cameras (None = mono)
    pub bayer_pattern: Option<BayerPattern>,
    /// Available readout modes (name, (width, height))
    pub readout_modes: Vec<(String, (u32, u32))>,
    /// Camera type code
    pub camera_type: u32,
    /// Firmware version string
    pub firmware_version: String,
    /// Probability (0.0..=1.0) that a live-mode frame read reports "frame not
    /// ready" (a retryable error) — opt-in via `with_live_not_ready_probability`
    /// (default 0.0). Models the real `GetQHYCCDLiveFrame` returning `QHYCCD_ERROR`
    /// between frames.
    pub live_not_ready_probability: f64,
}

impl Default for SimulatedCameraConfig {
    /// Creates a default configuration similar to a QHY178M
    fn default() -> Self {
        let mut supported_controls = HashMap::new();

        // Basic controls
        supported_controls.insert(ControlType::Gain, (0.0, 100.0, 1.0));
        supported_controls.insert(ControlType::Offset, (0.0, 255.0, 1.0));
        supported_controls.insert(ControlType::Exposure, (1.0, 3_600_000_000.0, 1.0)); // 1us to 1hr
        supported_controls.insert(ControlType::Speed, (0.0, 2.0, 1.0));
        supported_controls.insert(ControlType::UsbTraffic, (0.0, 255.0, 1.0));
        supported_controls.insert(ControlType::TransferBit, (8.0, 16.0, 8.0));

        // Binning modes
        supported_controls.insert(ControlType::CamBin1x1mode, (1.0, 1.0, 1.0));
        supported_controls.insert(ControlType::CamBin2x2mode, (1.0, 1.0, 1.0));

        // Frame modes
        supported_controls.insert(ControlType::CamSingleFrameMode, (1.0, 1.0, 1.0));
        supported_controls.insert(ControlType::CamLiveVideoMode, (1.0, 1.0, 1.0));

        // Bit modes
        supported_controls.insert(ControlType::Cam8bits, (1.0, 1.0, 1.0));
        supported_controls.insert(ControlType::Cam16bits, (1.0, 1.0, 1.0));

        Self {
            id: "SIM-001".to_string(),
            model: "QHY-SIMULATED".to_string(),
            chip_info: CCDChipInfo {
                // um, like the real SDK (verified on a QHY178M: GetQHYCCDChipInfo
                // returns chip dims in micrometers ≈ image_dim × pixel_size, not mm).
                chip_width: 7372.8,  // um (3072 × 2.4)
                chip_height: 4915.2, // um (2048 × 2.4)
                image_width: 3072,
                image_height: 2048,
                pixel_width: 2.4,  // um
                pixel_height: 2.4, // um
                bits_per_pixel: 16,
            },
            effective_area: CCDChipArea {
                start_x: 0,
                start_y: 0,
                width: 3072,
                height: 2048,
            },
            // A small overscan strip distinct from the effective imaging area —
            // real `GetQHYCCDOverScanArea` reports a separate calibration region,
            // not the full frame (audit fidelity fix).
            overscan_area: CCDChipArea {
                start_x: 0,
                start_y: 0,
                width: 24,
                height: 2048,
            },
            supported_controls,
            filter_wheel_slots: 0,
            has_cooler: false,
            bayer_pattern: None,
            readout_modes: vec![("Standard".to_string(), (3072, 2048))],
            camera_type: 4010,
            firmware_version: "Firmware version: 2024_1_1".to_string(),
            live_not_ready_probability: 0.0,
        }
    }
}

impl SimulatedCameraConfig {
    /// Creates a new configuration with a custom ID
    #[must_use]
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    /// Sets the camera model name
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Adds filter wheel support with the specified number of slots, capped at
    /// the sixteen a `CONTROL_CFWPORT` value can address
    /// ([`CFW_MAX_SLOTS`](crate::camera::CFW_MAX_SLOTS)).
    ///
    /// The cap keeps the simulated wheel internally consistent. Without it a
    /// larger count would be advertised through `CfwSlotsNum` and `CfwPort`'s
    /// range while every slot from the sixteenth up has no code to command it
    /// by — a wheel reporting a size it then refuses to move within.
    #[must_use]
    pub fn with_filter_wheel(mut self, slots: u32) -> Self {
        let slots = slots.min(crate::camera::CFW_MAX_SLOTS);
        self.filter_wheel_slots = slots;
        if slots > 0 {
            // The guard above is what keeps the decrement total.
            self.supported_controls.insert(
                ControlType::CfwPort,
                (0.0, f64::from(slots.saturating_sub(1)), 1.0),
            );
            self.supported_controls.insert(
                ControlType::CfwSlotsNum,
                (f64::from(slots), f64::from(slots), 0.0),
            );
        }
        self
    }

    /// Makes this a color camera with the specified Bayer pattern
    #[must_use]
    pub fn with_color(mut self, bayer_pattern: BayerPattern) -> Self {
        self.bayer_pattern = Some(bayer_pattern);
        // A fixed control: the pattern is both ends of its range.
        let code = f64::from(u32::from(bayer_pattern));
        self.supported_controls
            .insert(ControlType::CamColor, (code, code, 0.0));
        self.supported_controls
            .insert(ControlType::Wbr, (0.0, 255.0, 1.0));
        self.supported_controls
            .insert(ControlType::Wbb, (0.0, 255.0, 1.0));
        self.supported_controls
            .insert(ControlType::Wbg, (0.0, 255.0, 1.0));
        self
    }

    /// Adds cooler support
    #[must_use]
    pub fn with_cooler(mut self) -> Self {
        self.has_cooler = true;
        self.supported_controls
            .insert(ControlType::Cooler, (-40.0, 30.0, 0.1));
        self.supported_controls
            .insert(ControlType::CurTemp, (-40.0, 50.0, 0.1));
        self.supported_controls
            .insert(ControlType::CurPWM, (0.0, 255.0, 1.0));
        self.supported_controls
            .insert(ControlType::ManualPWM, (0.0, 255.0, 1.0));
        self
    }

    /// Sets the probability (clamped to 0.0..=1.0) that each live-mode frame read
    /// reports "not ready" (a retryable error) instead of delivering a frame, so a
    /// consumer's poll/retry loop is exercised against the simulation (real
    /// `GetQHYCCDLiveFrame` returns `QHYCCD_ERROR` between frames — e.g. ~0.05).
    /// Default is 0.0 (a frame is returned on every read).
    #[must_use]
    pub const fn with_live_not_ready_probability(mut self, p: f64) -> Self {
        self.live_not_ready_probability = p.clamp(0.0, 1.0);
        self
    }

    /// Sets custom chip information
    #[must_use]
    pub fn with_chip_info(mut self, chip_info: CCDChipInfo) -> Self {
        self.effective_area = CCDChipArea {
            start_x: 0,
            start_y: 0,
            width: chip_info.image_width,
            height: chip_info.image_height,
        };
        // Keep the overscan a distinct strip, not a copy of the effective area
        // (see the default config): the two SDK areas are separate regions.
        self.overscan_area = CCDChipArea {
            start_x: 0,
            start_y: 0,
            width: chip_info.image_width.min(24),
            height: chip_info.image_height,
        };
        self.chip_info = chip_info;
        self
    }

    /// Adds a readout mode
    #[must_use]
    pub fn with_readout_mode(mut self, name: impl Into<String>, width: u32, height: u32) -> Self {
        self.readout_modes.push((name.into(), (width, height)));
        self
    }

    /// Sets the firmware version string
    #[must_use]
    pub fn with_firmware_version(mut self, version: impl Into<String>) -> Self {
        self.firmware_version = version.into();
        self
    }

    /// Adds support for a control with the specified min, max, step values
    #[must_use]
    pub fn with_control(mut self, control: ControlType, min: f64, max: f64, step: f64) -> Self {
        self.supported_controls.insert(control, (min, max, step));
        self
    }
}

// ===== Runtime state for a simulated camera =====

/// Metadata for a captured image
#[derive(Debug, Clone)]
pub(crate) struct ImageMetadata {
    pub width: u32,
    pub height: u32,
    pub bits_per_pixel: u32,
    pub channels: u32,
}

/// Runtime state for a simulated camera
// Open / initialized / live-mode / debayer are four independent device states
// the real SDK also tracks separately; the bool count mirrors the hardware
// lifecycle, not a design choice (the zwo-rs `ASI_CAMERA_INFO` precedent).
#[expect(clippy::struct_excessive_bools)]
#[derive(Debug)]
pub(crate) struct SimulatedCameraState {
    /// Camera configuration (immutable reference data)
    pub config: SimulatedCameraConfig,
    /// Whether the camera is currently open
    pub is_open: bool,
    /// Whether the camera has been initialized
    pub is_initialized: bool,
    /// Current stream mode
    pub stream_mode: Option<StreamMode>,
    /// Current parameter values
    pub parameters: HashMap<ControlType, f64>,
    /// Current ROI settings
    pub roi: CCDChipArea,
    /// Current binning (x, y)
    pub binning: (u32, u32),
    /// Current bit depth for transfers
    pub bit_depth: u32,
    /// Current readout mode index
    pub readout_mode: u32,
    /// Whether live mode is active
    pub live_mode_active: bool,
    /// Exposure start time (for single frame mode)
    pub exposure_start: Option<Instant>,
    /// Exposure duration in microseconds (for single frame mode)
    pub exposure_duration_us: u64,
    /// Pre-generated image data (available after exposure completes)
    pub captured_image: Option<Vec<u8>>,
    /// Dimensions and metadata for the captured image
    pub captured_image_metadata: Option<ImageMetadata>,
    /// Current filter wheel position (0-indexed)
    pub filter_wheel_position: u32,
    /// Current target temperature for cooler
    pub target_temperature: f64,
    /// Current actual temperature (simulated)
    pub current_temperature: f64,
    /// Current cooler PWM
    pub cooler_pwm: f64,
    /// Debayer enabled
    pub debayer_enabled: bool,
    /// Commanded filter-wheel target slot (for the simulated settle delay).
    pub filter_wheel_target: u32,
    /// Reads remaining before the filter wheel reaches `filter_wheel_target`
    /// (advance-on-poll settle, so a poll-to-arrival loop is exercised).
    pub filter_wheel_settle_polls: u32,
}

/// Ambient (cooler-off) sensor temperature the simulated camera settles at, °C.
const SIM_AMBIENT_C: f64 = 20.0;
/// Per-poll cooling-ramp step, °C. Advance-on-poll (not wall-clock) mirrors the
/// sibling `svbony-rs` / `zwo-rs` sims so tests converge deterministically
/// without sleeping.
const SIM_COOLING_STEP_C: f64 = 1.0;
/// Simulated cooler PWM per °C below ambient (result saturated to 0..255).
const SIM_COOLER_PWM_PER_DEGREE: f64 = 12.0;
/// Reads the simulated filter wheel takes to reach a newly commanded slot
/// (advance-on-poll, so a consumer's poll-to-arrival loop runs a few iterations).
const SIM_CFW_SETTLE_POLLS: u32 = 3;

impl SimulatedCameraState {
    /// Creates a new state from a configuration
    pub fn new(config: SimulatedCameraConfig) -> Self {
        let roi = config.effective_area;
        let bit_depth = config.chip_info.bits_per_pixel;

        // Initialize parameters with default values (middle of range)
        let mut parameters = HashMap::new();
        for (control, (min, max, _step)) in &config.supported_controls {
            let default = match control {
                ControlType::Gain
                | ControlType::Speed
                | ControlType::CfwPort
                | ControlType::CurPWM
                | ControlType::ManualPWM => 0.0,
                ControlType::Offset => 10.0,
                ControlType::Exposure => 1000.0, // 1ms default
                ControlType::UsbTraffic => 50.0,
                ControlType::TransferBit => 16.0,
                ControlType::CfwSlotsNum => f64::from(config.filter_wheel_slots),
                // Room temperature: both the reading and the auto setpoint start there.
                ControlType::CurTemp | ControlType::Cooler => 20.0,
                _ => f64::midpoint(*min, *max),
            };
            parameters.insert(*control, default);
        }

        Self {
            config,
            is_open: false,
            is_initialized: false,
            stream_mode: None,
            parameters,
            roi,
            binning: (1, 1),
            bit_depth,
            readout_mode: 0,
            live_mode_active: false,
            exposure_start: None,
            exposure_duration_us: 1000,
            captured_image: None,
            captured_image_metadata: None,
            filter_wheel_position: 0,
            target_temperature: 20.0,
            current_temperature: 20.0,
            cooler_pwm: 0.0,
            debayer_enabled: false,
            filter_wheel_target: 0,
            filter_wheel_settle_polls: 0,
        }
    }

    /// Gets the current image dimensions accounting for ROI
    /// Note: ROI dimensions are already in binned coordinates when set via ASCOM Alpaca,
    /// so we don't apply binning division here
    pub const fn get_current_image_dimensions(&self) -> (u32, u32) {
        (self.roi.width, self.roi.height)
    }

    /// Gets the number of bytes per pixel based on current bit depth
    pub const fn get_bytes_per_pixel(&self) -> u32 {
        if self.bit_depth <= 8 {
            1
        } else {
            2
        }
    }

    /// Gets the number of channels (1 for mono, 3 for color with debayer)
    pub const fn get_channels(&self) -> u32 {
        if self.config.bayer_pattern.is_some() && self.debayer_enabled {
            3
        } else {
            1
        }
    }

    /// Calculates the required buffer size for the current settings. Zero for
    /// geometry that names no frame, which is the length the generators produce
    /// for it — a caller sizing a buffer from this always gets one the frame
    /// fits in.
    pub fn calculate_buffer_size(&self) -> usize {
        let (width, height) = self.get_current_image_dimensions();
        Frame::new(
            width,
            height,
            self.get_channels(),
            self.get_bytes_per_pixel(),
        )
        .map_or(0, |frame| frame.len)
    }

    /// Returns the remaining exposure time in microseconds
    pub fn get_remaining_exposure_us(&self) -> u32 {
        self.exposure_start.map_or(0, |start| {
            let elapsed_us = elapsed_micros(start.elapsed());
            if elapsed_us >= self.exposure_duration_us {
                0
            } else {
                // The SDK reports the remainder in `u32` microseconds,
                // which runs out at ~71 minutes — a ceiling of the vendor
                // API's shape, not this narrowing. Saturating pins there and
                // says "still plenty left"; narrowing wrapped a long
                // exposure round to a small number, so a caller polling this
                // as its authority would have believed the frame was nearly
                // done. `qhy-camera` is not such a caller (it times the
                // exposure host-side and only asks for confirmation), which
                // bounds the blast radius to this crate's API contract.
                u32::try_from(self.exposure_duration_us.saturating_sub(elapsed_us))
                    .unwrap_or(u32::MAX)
            }
        })
    }

    /// Checks if the current exposure is complete
    pub fn is_exposure_complete(&self) -> bool {
        self.exposure_start
            .is_none_or(|start| elapsed_micros(start.elapsed()) >= self.exposure_duration_us)
    }

    /// Starts an exposure
    pub fn start_exposure(&mut self) {
        // Get exposure time from parameters
        if let Some(&exposure_us) = self.parameters.get(&ControlType::Exposure) {
            self.exposure_duration_us = quantize::to_u64(exposure_us);
        }

        // Pre-generate the image when the exposure starts
        let (width, height) = self.get_current_image_dimensions();
        let bits_per_pixel = self.bit_depth;
        let channels = self.get_channels();

        let generator = ImageGenerator::default();
        let data = if bits_per_pixel <= 8 {
            generator.generate_8bit(width, height, channels)
        } else {
            generator.generate_16bit(width, height, channels)
        };

        // Store the generated image and metadata
        self.captured_image = Some(data);
        self.captured_image_metadata = Some(ImageMetadata {
            width,
            height,
            bits_per_pixel,
            channels,
        });

        // Capture the timestamp last: the exposure clock must not include
        // the frame-generation work above, which can take visible wall time
        // in unoptimized builds on loaded machines and would otherwise eat
        // into (or instantly complete) the simulated exposure.
        self.exposure_start = Some(Instant::now());
    }

    /// Stops the current exposure but keeps the image data
    /// (for `CancelQHYCCDExposing` - image stays in camera)
    pub const fn stop_exposure(&mut self) {
        // Mark exposure as complete while preserving the captured image for later retrieval.
        self.exposure_start = None;
    }

    /// Aborts the current exposure and discards the image data
    /// (for `CancelQHYCCDExposingAndReadout` - image is discarded)
    pub fn abort_exposure(&mut self) {
        self.exposure_start = None;
        self.captured_image = None;
        self.captured_image_metadata = None;
    }

    /// Advance the reported sensor temperature one poll-step toward the current
    /// target (the `CONTROL_COOLER` auto-cooling setpoint) and reflect a plausible
    /// cooler PWM. Advance-on-poll — a read of `CONTROL_CURTEMP` moves the value
    /// one step — mirrors the sibling `svbony-rs` / `zwo-rs` sims: it converges
    /// deterministically without sleeping. Real `GetQHYCCDParam(CURTEMP)` tracks
    /// the setpoint the same way. Called from the `get_parameter(CurTemp)` read
    /// path; with no setpoint the target stays at ambient so the value holds.
    pub fn update_temperature(&mut self) {
        let delta = self.target_temperature - self.current_temperature;
        let step = delta.clamp(-SIM_COOLING_STEP_C, SIM_COOLING_STEP_C);
        self.current_temperature += step;
        // A real cooler burns PWM proportional to how far below ambient it must
        // hold the sensor; zero at/above ambient (0..255 per the SDK's CurPWM).
        self.cooler_pwm = ((SIM_AMBIENT_C - self.current_temperature) * SIM_COOLER_PWM_PER_DEGREE)
            .clamp(0.0, 255.0);
        self.parameters
            .insert(ControlType::CurTemp, self.current_temperature);
    }

    /// Command the simulated filter wheel to `target`. Like real hardware the move
    /// is not instantaneous: subsequent position reads report the OLD slot until
    /// `SIM_CFW_SETTLE_POLLS` polls have elapsed (see `poll_filter_wheel`).
    ///
    /// A target with no `CONTROL_CFWPORT` code is rejected rather than commanded.
    /// The position read reports the wheel by *encoding* it, so accepting such a
    /// target would park the wheel where it could no longer answer — the bound
    /// the encoder puts on `Camera::set_cfw_position`'s input has to hold here
    /// too, since this path arrives through a decode that has none.
    pub fn command_filter_wheel(&mut self, target: u32) -> Result<()> {
        if crate::camera::cfw_slot_to_ascii(target).is_none() {
            let error = QHYError::InvalidFilterSlot { slot: target };
            tracing::error!(error = ?error);
            return Err(error);
        }
        self.filter_wheel_target = target;
        self.filter_wheel_settle_polls = if target == self.filter_wheel_position {
            0
        } else {
            SIM_CFW_SETTLE_POLLS
        };
        Ok(())
    }

    /// Read the simulated filter-wheel position, advancing the settle by one poll.
    /// Returns the current (possibly still-moving) slot; it equals the target only
    /// after `SIM_CFW_SETTLE_POLLS` reads, so a consumer's poll-to-arrival loop is
    /// exercised instead of an unrealistic instantaneous move.
    pub const fn poll_filter_wheel(&mut self) -> u32 {
        if self.filter_wheel_settle_polls > 0 {
            self.filter_wheel_settle_polls = self.filter_wheel_settle_polls.saturating_sub(1);
            if self.filter_wheel_settle_polls == 0 {
                self.filter_wheel_position = self.filter_wheel_target;
            }
        }
        self.filter_wheel_position
    }

    /// Roll whether this live-mode read reports "frame not ready" (a retryable
    /// error), per the configured probability (0.0 = never). Models real
    /// `GetQHYCCDLiveFrame` returning `QHYCCD_ERROR` between frames.
    pub fn roll_live_not_ready(&self) -> bool {
        let p = self.config.live_not_ready_probability;
        p > 0.0 && rand::rng().random::<f64>() < p
    }
}

// ===== Image generation for simulated frames =====

/// Pattern type for generated images
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImagePattern {
    /// Gradient from dark to light with noise
    #[default]
    Gradient,
    /// Simulated star field
    StarField,
    /// Flat field with noise
    Flat,
    /// Test pattern with geometric shapes
    TestPattern,
}

/// Per-pixel noise source (xorshift32). Simulated frames need noise that
/// looks plausible, not statistical rigor, and a full `rand` uniform-range
/// sample per pixel dominates frame-generation cost in unoptimized builds
/// — a multi-megapixel frame is millions of samples.
struct PixelNoise {
    state: u32,
    /// `2 * range + 1`, the width of the interval samples land in. Held rather
    /// than recomputed because `range` is fixed for a whole frame and
    /// `next_signed` runs once per pixel.
    span: u64,
    /// The same `range`, in the width the subtraction needs.
    range: i64,
}

impl PixelNoise {
    /// `range` is the half-width of the interval `next_signed` draws from; a
    /// negative one is no noise at all, which `span == 1` yields for free.
    fn new(seed: u32, range: i32) -> Self {
        let range = range.max(0);
        // Once per frame, so the saturating spellings cost nothing: `range`
        // fits `i32`, so `2 * range + 1` cannot leave `u64` and neither arm
        // can be taken.
        let span = u64::from(range.unsigned_abs())
            .saturating_mul(2)
            .saturating_add(1);
        Self {
            // xorshift is stuck at zero; force a nonzero start.
            state: seed | 1,
            span,
            range: i64::from(range),
        }
    }

    const fn next_u32(&mut self) -> u32 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.state = x;
        x
    }

    /// Roughly uniform value in `[-range, +range]`. Scaling by the span and
    /// keeping the high word is the same distribution a modulo gives, to within
    /// the same negligible bias at the spans used here (a few thousand out of
    /// 2^32) — for one multiply instead of a divide. That is worth naming: at a
    /// megapixel per frame the divide was most of the cost of generating one,
    /// and it dominated the loop badly enough to make an indexed write look
    /// faster than walking the buffer.
    #[expect(
        clippy::arithmetic_side_effects,
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        reason = "every step is bounded, and this body runs once per pixel: `range <= i32::MAX` gives `span <= 2^32 - 1`, so the product is at most `(2^32 - 1)^2 = 2^64 - 2^33 + 1`, inside `u64` with 2^33 - 1 to spare; `scaled < span` then puts the difference in `-range..=range`, which fits `i32` because `range` did. Neither cast has a total spelling that is not a dead arm, and writing the arithmetic saturating instead measured +45% on the per-pixel loop"
    )]
    fn next_signed(&mut self) -> i32 {
        let scaled = (u64::from(self.next_u32()) * self.span) >> 32;
        (scaled as i64 - self.range) as i32
    }
}

/// Row-distinct seed so rayon rows (and serial frames reusing a seed)
/// don't repeat the same noise sequence.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "`row` indexes the rows of a frame whose height is a `u32`, so the truncation is unreachable; `usize` has no total conversion to `u32`, and every fallible spelling would add an arm that cannot be taken and whose fallback would alias row 0's seed. A seed needs only to differ between rows, which folding preserves either way"
)]
const fn row_seed(frame_seed: u32, row: usize) -> u32 {
    frame_seed ^ (row as u32).wrapping_mul(0x9E37_79B9)
}

/// Whole microseconds as `u64`. `Duration` has no `u64` accessor for them —
/// `as_micros` is `u128` — but `as_secs` is `u64` and `subsec_micros` is `u32`,
/// so composing the two needs no conversion at all and saturates rather than
/// wrapping at the (~584,000 year) ceiling.
fn elapsed_micros(elapsed: std::time::Duration) -> u64 {
    elapsed
        .as_secs()
        .saturating_mul(1_000_000)
        .saturating_add(u64::from(elapsed.subsec_micros()))
}

/// Lands a computed sample on the wire type, saturating rather than wrapping.
/// Noise, a gradient, or a star's core routinely pushes a value past black or
/// white; clipping there is what a sensor does, and both arms are reached often
/// enough to test.
fn sample_u8(v: i32) -> u8 {
    u8::try_from(v).unwrap_or(if v < 0 { u8::MIN } else { u8::MAX })
}

fn sample_u16(v: i32) -> u16 {
    u16::try_from(v).unwrap_or(if v < 0 { u16::MIN } else { u16::MAX })
}

/// Writes a 16-bit sample into every channel of a pixel. The chunk length is
/// the sample's own byte count, so the copy cannot mismatch it.
fn write_sample(pixel: &mut [u8], value: u16) {
    let bytes = value.to_le_bytes();
    for sample in pixel.chunks_exact_mut(bytes.len()) {
        sample.copy_from_slice(&bytes);
    }
}

/// Adds `value` to every 16-bit channel of a pixel, saturating so an
/// overlapping star's core stays white rather than wrapping to black.
fn add_sample(pixel: &mut [u8], value: u16) {
    for sample in pixel.chunks_exact_mut(2) {
        if let Some(bytes) = sample.first_chunk::<2>() {
            let sum = u16::from_le_bytes(*bytes).saturating_add(value);
            sample.copy_from_slice(&sum.to_le_bytes());
        }
    }
}

/// Frame geometry, in both of the roles the generators need it. `width` and
/// `height` are the coordinates a pattern computes from; `pixel_bytes`,
/// `row_bytes` and `len` are the lengths that walk the buffer. The SDK reports
/// the former as `u32`, so the conversion into the latter happens once, here,
/// instead of at every index.
#[derive(Debug, Clone, Copy)]
struct Frame {
    width: u32,
    height: u32,
    pixel_bytes: usize,
    row_bytes: usize,
    len: usize,
}

impl Frame {
    /// `None` for a frame with no bytes in it, and for one too large to address
    /// on this target — both mean the same thing to every caller here: nothing
    /// to fill. Excluding the empty frame also keeps every stride below
    /// non-zero, which is what `chunks_exact_mut` and `par_chunks_mut` require.
    fn new(width: u32, height: u32, channels: u32, sample_bytes: u32) -> Option<Self> {
        let sample_bytes = usize::try_from(sample_bytes).ok()?;
        let pixel_bytes = usize::try_from(channels).ok()?.checked_mul(sample_bytes)?;
        let row_bytes = usize::try_from(width).ok()?.checked_mul(pixel_bytes)?;
        let len = row_bytes.checked_mul(usize::try_from(height).ok()?)?;
        (len > 0).then_some(Self {
            width,
            height,
            pixel_bytes,
            row_bytes,
            len,
        })
    }

    /// The bytes of one pixel, or `None` if it lies outside the frame — so a
    /// pattern that computes a coordinate past an edge clips instead of
    /// panicking. This is the only place a coordinate becomes an offset.
    fn pixel_mut<'a>(&self, data: &'a mut [u8], x: u32, y: u32) -> Option<&'a mut [u8]> {
        let row = data
            .chunks_exact_mut(self.row_bytes)
            .nth(usize::try_from(y).ok()?)?;
        row.chunks_exact_mut(self.pixel_bytes)
            .nth(usize::try_from(x).ok()?)
    }
}

/// Generates test images for simulated camera capture
#[derive(Debug, Clone)]
pub struct ImageGenerator {
    pattern: ImagePattern,
    noise_level: f64,
    base_level: u16,
}

impl Default for ImageGenerator {
    fn default() -> Self {
        Self {
            pattern: ImagePattern::Gradient,
            noise_level: 0.05, // 5% noise
            base_level: 1000,  // Base ADU level
        }
    }
}

impl ImageGenerator {
    /// Creates a new generator with the specified pattern
    #[must_use]
    pub fn new(pattern: ImagePattern) -> Self {
        Self {
            pattern,
            ..Default::default()
        }
    }

    /// Sets the noise level (0.0 to 1.0)
    #[must_use]
    pub const fn with_noise_level(mut self, level: f64) -> Self {
        self.noise_level = level.clamp(0.0, 1.0);
        self
    }

    /// Sets the base signal level
    #[must_use]
    pub const fn with_base_level(mut self, level: u16) -> Self {
        self.base_level = level;
        self
    }

    /// Generates an 8-bit image. An empty vector means the geometry names no
    /// frame — zero in any dimension, or more bytes than this target can
    /// address.
    #[must_use]
    pub fn generate_8bit(&self, width: u32, height: u32, channels: u32) -> Vec<u8> {
        let Some(frame) = Frame::new(width, height, channels, 1) else {
            return Vec::new();
        };
        let mut data = vec![0u8; frame.len];
        // One `rand` sample per frame keeps frames distinct; the per-pixel
        // noise itself comes from the cheap `PixelNoise` stream.
        let mut rng = rand::rng();
        let frame_seed: u32 = rng.random();

        match self.pattern {
            ImagePattern::Gradient => self.generate_gradient_8bit(&mut data, frame, frame_seed),
            ImagePattern::StarField => {
                self.generate_starfield_8bit(&mut data, frame, frame_seed, &mut rng);
            }
            ImagePattern::Flat => self.generate_flat_8bit(&mut data, frame, frame_seed),
            ImagePattern::TestPattern => {
                self.generate_test_pattern_8bit(&mut data, frame, frame_seed);
            }
        }

        data
    }

    /// Generates a 16-bit image. Empty means the same as it does for
    /// [`Self::generate_8bit`].
    #[must_use]
    pub fn generate_16bit(&self, width: u32, height: u32, channels: u32) -> Vec<u8> {
        let Some(frame) = Frame::new(width, height, channels, 2) else {
            return Vec::new();
        };
        let mut data = vec![0u8; frame.len];
        // One `rand` sample per frame keeps frames distinct; the per-pixel
        // noise itself comes from the cheap `PixelNoise` stream.
        let mut rng = rand::rng();
        let frame_seed: u32 = rng.random();

        match self.pattern {
            ImagePattern::Gradient => self.generate_gradient_16bit(&mut data, frame, frame_seed),
            ImagePattern::StarField => {
                self.generate_starfield_16bit(&mut data, frame, frame_seed, &mut rng);
            }
            ImagePattern::Flat => self.generate_flat_16bit(&mut data, frame, frame_seed),
            ImagePattern::TestPattern => {
                self.generate_test_pattern_16bit(&mut data, frame, frame_seed);
            }
        }

        data
    }

    fn generate_gradient_8bit(&self, data: &mut [u8], frame: Frame, frame_seed: u32) {
        let [base, _] = self.base_level.to_be_bytes();
        let noise_range = quantize::to_i32(255.0 * self.noise_level);
        let mut noise_source = PixelNoise::new(frame_seed, noise_range);

        // Rows and pixels are chunks of the buffer, so the column index stays a
        // coordinate the ramp reads and never becomes an offset.
        for row in data.chunks_exact_mut(frame.row_bytes) {
            for (x, pixel) in (0u32..).zip(row.chunks_exact_mut(frame.pixel_bytes)) {
                let gradient = quantize::to_u8((f64::from(x) / f64::from(frame.width)) * 200.0);
                let noise = noise_source.next_signed();
                let value = sample_u8(
                    i32::from(base)
                        .saturating_add(i32::from(gradient))
                        .saturating_add(noise),
                );

                pixel.fill(value);
            }
        }
    }

    fn generate_gradient_16bit(&self, data: &mut [u8], frame: Frame, frame_seed: u32) {
        let noise_range = quantize::to_i32(65535.0 * self.noise_level);
        let base_level = self.base_level;

        // Process rows in parallel; each row gets its own noise stream.
        data.par_chunks_mut(frame.row_bytes)
            .enumerate()
            .for_each(|(y, row)| {
                let mut noise_source = PixelNoise::new(row_seed(frame_seed, y), noise_range);

                for (x, pixel) in (0u32..).zip(row.chunks_exact_mut(frame.pixel_bytes)) {
                    let gradient =
                        quantize::to_u16((f64::from(x) / f64::from(frame.width)) * 50000.0);
                    let noise = noise_source.next_signed();
                    let value = sample_u16(
                        i32::from(base_level)
                            .saturating_add(i32::from(gradient))
                            .saturating_add(noise),
                    );

                    write_sample(pixel, value);
                }
            });
    }

    fn generate_starfield_8bit<R: Rng>(
        &self,
        data: &mut [u8],
        frame: Frame,
        frame_seed: u32,
        rng: &mut R,
    ) {
        // Fill with background noise
        let [base, _] = self.base_level.to_be_bytes();
        let noise_range = quantize::to_i32(255.0 * self.noise_level * 0.5); // Less noise for starfield
        let mut noise_source = PixelNoise::new(frame_seed, noise_range);

        // One draw per pixel, replicated across its channels — the model the
        // 16-bit starfield and every other generator here uses. A mono frame's
        // pixel is a single byte, where chunking costs more than this body does.
        if frame.pixel_bytes == 1 {
            for sample in data.iter_mut() {
                let noise = noise_source.next_signed();
                *sample = sample_u8(i32::from(base).saturating_add(noise));
            }
        } else {
            for pixel in data.chunks_exact_mut(frame.pixel_bytes) {
                let noise = noise_source.next_signed();
                pixel.fill(sample_u8(i32::from(base).saturating_add(noise)));
            }
        }

        // Add stars. A centre needs a pixel either side of it, so a frame
        // narrower than three has no interior to place one in.
        if frame.width < 3 || frame.height < 3 {
            return;
        }
        let num_stars =
            quantize::to_usize(f64::from(frame.width) * f64::from(frame.height) * 0.001); // ~0.1% coverage
        for _ in 0..num_stars {
            // The guard above is what keeps these ranges non-empty; the
            // saturating spelling is only what says so to the compiler.
            let x = rng.random_range(1..frame.width.saturating_sub(1));
            let y = rng.random_range(1..frame.height.saturating_sub(1));
            let brightness: u8 = rng.random_range(150..255);
            let size: u8 = rng.random_range(1..=3);

            Self::draw_star_8bit(data, frame, x, y, brightness, size);
        }
    }

    fn generate_starfield_16bit<R: Rng>(
        &self,
        data: &mut [u8],
        frame: Frame,
        frame_seed: u32,
        rng: &mut R,
    ) {
        // Fill with background noise
        let noise_range = quantize::to_i32(65535.0 * self.noise_level * 0.3);
        let mut noise_source = PixelNoise::new(frame_seed, noise_range);

        for pixel in data.chunks_exact_mut(frame.pixel_bytes) {
            let noise = noise_source.next_signed();
            let value = sample_u16(i32::from(self.base_level).saturating_add(noise));

            write_sample(pixel, value);
        }

        // Add stars. The 16-bit centres sit two pixels in, so this needs a
        // wider interior than the 8-bit pattern above.
        if frame.width < 5 || frame.height < 5 {
            return;
        }
        let num_stars =
            quantize::to_usize(f64::from(frame.width) * f64::from(frame.height) * 0.001);
        for _ in 0..num_stars {
            let x = rng.random_range(2..frame.width.saturating_sub(2));
            let y = rng.random_range(2..frame.height.saturating_sub(2));
            let brightness: u16 = rng.random_range(40000..65535);
            let size: u8 = rng.random_range(1..=3);

            Self::draw_star_16bit(data, frame, x, y, brightness, size);
        }
    }

    fn draw_star_8bit(data: &mut [u8], frame: Frame, cx: u32, cy: u32, brightness: u8, size: u8) {
        let radius = u32::from(size);
        // `size` is a `u8`, so the widest box a caller can ask for is 511
        // across and the doubling cannot leave `u32`.
        let diameter = radius.saturating_mul(2);

        for dy in 0..=diameter {
            for dx in 0..=diameter {
                // The offset from the centre is a magnitude and a direction,
                // and the two halves are wanted separately: the distance needs
                // the magnitude, the coordinate needs the direction. The `None`
                // arm is a star overhanging the *near* edge, where the
                // coordinate would go negative — half the bounds check. The far
                // edge is `frame.pixel_mut` below, which is why both are needed.
                let (ox, oy) = (dx.abs_diff(radius), dy.abs_diff(radius));
                let (Some(x), Some(y)) = (
                    if dx >= radius {
                        cx.checked_add(ox)
                    } else {
                        cx.checked_sub(ox)
                    },
                    if dy >= radius {
                        cy.checked_add(oy)
                    } else {
                        cy.checked_sub(oy)
                    },
                ) else {
                    continue;
                };

                let (fx, fy) = (f64::from(ox), f64::from(oy));
                let dist = fx.hypot(fy);
                if dist <= f64::from(size) {
                    let falloff = 1.0 - (dist / (f64::from(size) + 1.0));
                    let value = quantize::to_u8(f64::from(brightness) * falloff);

                    let Some(pixel) = frame.pixel_mut(data, x, y) else {
                        continue;
                    };
                    for sample in pixel.iter_mut() {
                        *sample = sample.saturating_add(value);
                    }
                }
            }
        }
    }

    fn draw_star_16bit(data: &mut [u8], frame: Frame, cx: u32, cy: u32, brightness: u16, size: u8) {
        let radius = u32::from(size);
        let diameter = radius.saturating_mul(2);

        for dy in 0..=diameter {
            for dx in 0..=diameter {
                let (ox, oy) = (dx.abs_diff(radius), dy.abs_diff(radius));
                let (Some(x), Some(y)) = (
                    if dx >= radius {
                        cx.checked_add(ox)
                    } else {
                        cx.checked_sub(ox)
                    },
                    if dy >= radius {
                        cy.checked_add(oy)
                    } else {
                        cy.checked_sub(oy)
                    },
                ) else {
                    continue;
                };

                let (fx, fy) = (f64::from(ox), f64::from(oy));
                let dist = fx.hypot(fy);
                if dist <= f64::from(size) {
                    let falloff = 1.0 - (dist / (f64::from(size) + 1.0));
                    let value = quantize::to_u16(f64::from(brightness) * falloff);

                    let Some(pixel) = frame.pixel_mut(data, x, y) else {
                        continue;
                    };
                    add_sample(pixel, value);
                }
            }
        }
    }

    fn generate_flat_8bit(&self, data: &mut [u8], frame: Frame, frame_seed: u32) {
        let [base, _] = self.base_level.to_be_bytes();
        let noise_range = quantize::to_i32(255.0 * self.noise_level);
        let mut noise_source = PixelNoise::new(frame_seed, noise_range);

        // A flat is uniform, so the pixel's position never enters the value —
        // walking pixels is the whole loop. A mono frame's pixel *is* its
        // sample, and chunking a buffer by one costs more per pixel than this
        // body does, so that case walks samples directly.
        if frame.pixel_bytes == 1 {
            for sample in data.iter_mut() {
                let noise = noise_source.next_signed();
                *sample = sample_u8(i32::from(base).saturating_add(noise));
            }
        } else {
            for pixel in data.chunks_exact_mut(frame.pixel_bytes) {
                let noise = noise_source.next_signed();
                pixel.fill(sample_u8(i32::from(base).saturating_add(noise)));
            }
        }
    }

    fn generate_flat_16bit(&self, data: &mut [u8], frame: Frame, frame_seed: u32) {
        let noise_range = quantize::to_i32(65535.0 * self.noise_level);
        let mut noise_source = PixelNoise::new(frame_seed, noise_range);

        for pixel in data.chunks_exact_mut(frame.pixel_bytes) {
            let noise = noise_source.next_signed();
            let value = sample_u16(i32::from(self.base_level).saturating_add(noise));

            write_sample(pixel, value);
        }
    }

    fn generate_test_pattern_8bit(&self, data: &mut [u8], frame: Frame, frame_seed: u32) {
        let noise_range = quantize::to_i32(255.0 * self.noise_level * 0.5);
        let mut noise_source = PixelNoise::new(frame_seed, noise_range);

        // The frame's centre, and the vertical leg of the distance to it: both
        // are invariant across the row, so only the horizontal leg is per-pixel.
        let (cx, cy) = (frame.width / 2, frame.height / 2);

        for (y, row) in (0u32..).zip(data.chunks_exact_mut(frame.row_bytes)) {
            let dy = f64::from(y.abs_diff(cy));
            let dy2 = dy * dy;

            for (x, pixel) in (0u32..).zip(row.chunks_exact_mut(frame.pixel_bytes)) {
                // Create a checkerboard with varying intensities
                let block_size = 64;
                let block_x = x / block_size;
                let block_y = y / block_size;
                // Only the parity of the sum matters, and wrapping preserves it.
                let is_light = block_x.wrapping_add(block_y) % 2 == 0;

                let base = if is_light { 200u8 } else { 50u8 };

                // Add concentric circles in center
                let dx = f64::from(x.abs_diff(cx));
                let dist = dx.mul_add(dx, dy2).sqrt();
                let ring = quantize::to_u32(dist / 50.0) % 2;
                let ring_mod = if ring == 0 { 20 } else { -20 };

                let noise = noise_source.next_signed();
                let value = sample_u8(
                    i32::from(base)
                        .saturating_add(ring_mod)
                        .saturating_add(noise),
                );

                pixel.fill(value);
            }
        }
    }

    fn generate_test_pattern_16bit(&self, data: &mut [u8], frame: Frame, frame_seed: u32) {
        let noise_range = quantize::to_i32(65535.0 * self.noise_level * 0.5);
        let mut noise_source = PixelNoise::new(frame_seed, noise_range);

        let (cx, cy) = (frame.width / 2, frame.height / 2);

        for (y, row) in (0u32..).zip(data.chunks_exact_mut(frame.row_bytes)) {
            let dy = f64::from(y.abs_diff(cy));
            let dy2 = dy * dy;

            for (x, pixel) in (0u32..).zip(row.chunks_exact_mut(frame.pixel_bytes)) {
                // Create a checkerboard with varying intensities
                let block_size = 64;
                let block_x = x / block_size;
                let block_y = y / block_size;
                let is_light = block_x.wrapping_add(block_y) % 2 == 0;

                let base: u16 = if is_light { 50000 } else { 10000 };

                // Add concentric circles in center
                let dx = f64::from(x.abs_diff(cx));
                let dist = dx.mul_add(dx, dy2).sqrt();
                let ring = quantize::to_u32(dist / 50.0) % 2;
                let ring_mod: i32 = if ring == 0 { 5000 } else { -5000 };

                let noise = noise_source.next_signed();
                let value = sample_u16(
                    i32::from(base)
                        .saturating_add(ring_mod)
                        .saturating_add(noise),
                );

                write_sample(pixel, value);
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod filter_wheel_config_tests {
    use super::{ControlType, SimulatedCameraConfig};

    #[test]
    fn a_wheel_is_capped_at_the_slots_the_encoding_can_address() {
        // Asking for more slots than a CONTROL_CFWPORT digit can name would
        // advertise a count the wheel then refuses to move within, so the
        // count and the encoding have to agree.
        let config = SimulatedCameraConfig::default().with_filter_wheel(20);
        assert_eq!(config.filter_wheel_slots, 16);
        assert_eq!(
            config.supported_controls.get(&ControlType::CfwSlotsNum),
            Some(&(16.0, 16.0, 0.0))
        );
        // The port's range is 0-indexed, so its top is the last nameable slot.
        assert_eq!(
            config.supported_controls.get(&ControlType::CfwPort),
            Some(&(0.0, 15.0, 1.0))
        );
    }

    #[test]
    fn a_wheel_within_the_bound_is_untouched() {
        let config = SimulatedCameraConfig::default().with_filter_wheel(5);
        assert_eq!(config.filter_wheel_slots, 5);
        assert_eq!(
            config.supported_controls.get(&ControlType::CfwPort),
            Some(&(0.0, 4.0, 1.0))
        );
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod image_generator_tests {
    use super::*;

    // Frame-envelope literals below derive from `ImageGenerator::default()`:
    // base_level 1000 (8-bit base 1000 >> 8 = 3), noise_level 0.05
    // (8-bit ±12, 16-bit ±3276; starfield/test-pattern scale these down).

    const W: u32 = 64;
    const H: u32 = 64;

    fn px16(data: &[u8], width: u32, x: u32, y: u32) -> u16 {
        let idx = ((y * width + x) * 2) as usize;
        u16::from_le_bytes([data[idx], data[idx + 1]])
    }

    #[test]
    fn pixel_noise_zero_or_negative_range_is_zero() {
        // A zero range gives a span of one, so every draw scales to zero; a
        // negative one is clamped to the same thing rather than rejected.
        let mut zero = PixelNoise::new(42, 0);
        let mut negative = PixelNoise::new(42, -5);
        assert!((0..100).all(|_| zero.next_signed() == 0));
        assert!((0..100).all(|_| negative.next_signed() == 0));
    }

    #[test]
    fn pixel_noise_stays_within_range_and_varies() {
        let mut noise = PixelNoise::new(7, 100);
        let samples: Vec<i32> = (0..1000).map(|_| noise.next_signed()).collect();
        assert!(samples.iter().all(|v| (-100..=100).contains(v)));
        assert!(
            samples.iter().any(|v| *v != samples[0]),
            "noise stream must not be constant"
        );
    }

    #[test]
    fn pixel_noise_reaches_both_ends_of_its_range() {
        // Scaling by the span has to reach both extremes: a high-word multiply
        // that lost the top value would bias every frame dark, and one that
        // overshot would push samples outside the envelope the generators
        // clamp against. 20k draws over a 21-wide span settle it.
        let mut noise = PixelNoise::new(3, 10);
        let samples: Vec<i32> = (0..20_000).map(|_| noise.next_signed()).collect();
        assert_eq!(*samples.iter().min().unwrap(), -10);
        assert_eq!(*samples.iter().max().unwrap(), 10);
    }

    #[test]
    fn a_sample_saturates_at_both_ends_instead_of_wrapping() {
        // Both arms are ordinary traffic, not defensive padding: noise drives a
        // dark pixel below black, and a star's core drives one past white.
        assert_eq!(sample_u8(-40), 0, "below black clips to black");
        assert_eq!(sample_u8(400), 255, "above white clips to white");
        assert_eq!(sample_u8(120), 120);
        assert_eq!(sample_u16(-1), 0);
        assert_eq!(sample_u16(70_000), 65_535);
        assert_eq!(sample_u16(1_000), 1_000);
    }

    #[test]
    fn whole_micros_match_the_u128_accessor_and_saturate_past_it() {
        use std::time::Duration;

        for elapsed in [
            Duration::ZERO,
            Duration::from_micros(1),
            Duration::from_millis(1234),
            Duration::from_secs(3600),
            Duration::new(u32::MAX.into(), 999_999_000),
        ] {
            assert_eq!(
                u128::from(elapsed_micros(elapsed)),
                elapsed.as_micros(),
                "{elapsed:?}"
            );
        }
        let past_the_ceiling = Duration::new(u64::MAX, 999_999_999);
        assert_eq!(elapsed_micros(past_the_ceiling), u64::MAX);
    }

    #[test]
    fn row_seed_differs_between_rows() {
        assert_ne!(row_seed(1234, 0), row_seed(1234, 1));
    }

    #[test]
    fn gradient_8bit_brightens_left_to_right() {
        let data = ImageGenerator::new(ImagePattern::Gradient).generate_8bit(W, H, 1);
        assert_eq!(data.len(), (W * H) as usize);
        let col_mean = |x: u32| {
            (0..H)
                .map(|y| f64::from(data[(y * W + x) as usize]))
                .sum::<f64>()
                / f64::from(H)
        };
        // Left column ≈ base 3, right ≈ 3 + 196; ±12 noise averages out
        // over a column, so a 100-count margin cannot flake.
        assert!(
            col_mean(W - 1) > col_mean(0) + 100.0,
            "gradient must rise left to right: left {} right {}",
            col_mean(0),
            col_mean(W - 1)
        );
    }

    #[test]
    fn gradient_16bit_brightens_left_to_right() {
        let data = ImageGenerator::new(ImagePattern::Gradient).generate_16bit(W, H, 1);
        assert_eq!(data.len(), (W * H * 2) as usize);
        let col_mean =
            |x: u32| (0..H).map(|y| f64::from(px16(&data, W, x, y))).sum::<f64>() / f64::from(H);
        // Left column ≈ base 1000, right ≈ 1000 + 49218; ±3276 noise
        // averages out over a column.
        assert!(
            col_mean(W - 1) > col_mean(0) + 20_000.0,
            "gradient must rise left to right: left {} right {}",
            col_mean(0),
            col_mean(W - 1)
        );
    }

    #[test]
    fn flat_frames_stay_inside_the_noise_envelope() {
        let generator = ImageGenerator::new(ImagePattern::Flat);
        let data8 = generator.generate_8bit(W, H, 1);
        // 8-bit flat: base 3 ± 12, clamped at 0 → every sample ≤ 15.
        assert!(data8.iter().all(|&v| v <= 15));
        let data16 = generator.generate_16bit(W, H, 1);
        // 16-bit flat: base 1000 ± 3276 → every sample ≤ 4276.
        let max = (0..H)
            .flat_map(|y| (0..W).map(move |x| (x, y)))
            .map(|(x, y)| px16(&data16, W, x, y))
            .max()
            .unwrap();
        assert!(max <= 4276, "flat sample above the noise envelope: {max}");
        // Noise must actually be present.
        let first = px16(&data16, W, 0, 0);
        assert!((0..W).any(|x| px16(&data16, W, x, 0) != first));
    }

    #[test]
    fn starfield_adds_stars_above_the_background() {
        // 64×64 places 4 stars (0.1% coverage); each star's centre pixel
        // carries its full brightness (≥150 in 8-bit, ≥40000 in 16-bit),
        // far above the ≤15 / ≤1983 background envelope.
        let data8 = ImageGenerator::new(ImagePattern::StarField).generate_8bit(W, H, 1);
        assert!(data8.iter().copied().max().unwrap() >= 100);
        let data16 = ImageGenerator::new(ImagePattern::StarField).generate_16bit(W, H, 1);
        let max = data16
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .max()
            .unwrap();
        assert!(max >= 30_000, "no star found: max sample {max}");
    }

    #[test]
    fn test_pattern_has_light_and_dark_blocks() {
        // 128×128 spans four 64-px checkerboard blocks; (0,0) sits in a
        // light block, (64,0) in a dark one. Both sample points share the
        // same ring modifier, so the block contrast dominates the noise.
        let (w, h) = (128, 128);
        let data8 = ImageGenerator::new(ImagePattern::TestPattern).generate_8bit(w, h, 1);
        let light = data8[0];
        let dark = data8[64];
        assert!(light > dark, "8-bit light {light} dark {dark}");
        let data16 = ImageGenerator::new(ImagePattern::TestPattern).generate_16bit(w, h, 1);
        assert!(px16(&data16, w, 0, 0) > px16(&data16, w, 64, 0));
    }

    #[test]
    fn channels_replicate_each_sample() {
        let data8 = ImageGenerator::new(ImagePattern::Gradient).generate_8bit(8, 8, 3);
        assert_eq!(data8.len(), 8 * 8 * 3);
        for px in data8.chunks_exact(3) {
            assert_eq!(px[0], px[1]);
            assert_eq!(px[1], px[2]);
        }
        let data16 = ImageGenerator::new(ImagePattern::Gradient).generate_16bit(8, 8, 3);
        assert_eq!(data16.len(), 8 * 8 * 3 * 2);
        for px in data16.chunks_exact(6) {
            assert_eq!(px[0..2], px[2..4]);
            assert_eq!(px[2..4], px[4..6]);
        }
    }

    #[test]
    fn every_pattern_replicates_a_pixel_across_its_channels() {
        // One noise draw per pixel, not per byte: a colour frame whose channels
        // diverge is speckle, and the 16-bit generators have always drawn per
        // pixel. The 8-bit starfield background was the one that did not.
        for pattern in [
            ImagePattern::Gradient,
            ImagePattern::StarField,
            ImagePattern::Flat,
            ImagePattern::TestPattern,
        ] {
            let data = ImageGenerator::new(pattern).generate_8bit(64, 64, 3);
            assert!(
                data.chunks_exact(3)
                    .all(|px| px[0] == px[1] && px[1] == px[2]),
                "{pattern:?} 8-bit channels diverge within a pixel"
            );
            let data = ImageGenerator::new(pattern).generate_16bit(64, 64, 3);
            assert!(
                data.chunks_exact(6)
                    .all(|px| px[0..2] == px[2..4] && px[2..4] == px[4..6]),
                "{pattern:?} 16-bit channels diverge within a pixel"
            );
        }
    }

    #[test]
    fn geometry_naming_no_frame_produces_no_data() {
        let generator = ImageGenerator::new(ImagePattern::Gradient);
        assert!(generator.generate_8bit(0, H, 1).is_empty(), "zero width");
        assert!(generator.generate_8bit(W, 0, 1).is_empty(), "zero height");
        assert!(generator.generate_8bit(W, H, 0).is_empty(), "no channels");
        assert!(generator.generate_16bit(0, H, 1).is_empty(), "zero width");
    }

    #[test]
    fn a_frame_too_large_to_address_produces_no_data() {
        // 2^32 square at three channels of 16-bit colour is ~1.1e20 bytes, past
        // `usize::MAX` on a 64-bit target. The geometry is refused whole rather
        // than multiplied out, so nothing is allocated and no loop walks off a
        // buffer sized from a wrapped length.
        let generator = ImageGenerator::new(ImagePattern::Flat);
        assert_eq!(
            generator.generate_16bit(u32::MAX, u32::MAX, 3),
            Vec::<u8>::new()
        );
        assert!(Frame::new(u32::MAX, u32::MAX, 3, 2).is_none());
    }

    #[test]
    fn a_frame_too_narrow_for_a_star_still_renders_its_background() {
        // Star centres come from a range that excludes the outermost pixels, so
        // a frame under 3 across (5 for the 16-bit pattern) leaves it empty —
        // which `random_range` rejects. It takes a frame that is *both* narrow
        // and large to reach: the star count is 0.1% of the pixels, so only at
        // 1000 pixels does the generator ask for a centre at all.
        let generator = ImageGenerator::new(ImagePattern::StarField);
        assert_eq!(generator.generate_8bit(2, 600, 1).len(), 1200);
        assert_eq!(generator.generate_16bit(4, 300, 1).len(), 2400);
    }

    #[test]
    fn a_star_overhanging_the_frame_draws_only_the_part_inside_it() {
        // Centred on the last column, so half the star's box lies past the
        // right edge and a quarter above the top. Both must clip.
        let frame = Frame::new(8, 8, 1, 1).unwrap();
        let mut data = vec![0u8; frame.len];
        ImageGenerator::draw_star_8bit(&mut data, frame, 7, 0, 200, 2);

        assert!(data[7] > 0, "the star's centre must be lit");
        assert_eq!(
            data[8], 0,
            "the next row's first pixel is not the star's overhang"
        );
    }

    #[test]
    fn a_16bit_star_overhanging_the_frame_draws_only_the_part_inside_it() {
        // The 16-bit drawer clips the same way, and its own placement only
        // reaches an edge at random, so pin it here rather than leave it to
        // whichever centres the generator happens to draw.
        let frame = Frame::new(8, 8, 1, 2).unwrap();
        let mut data = vec![0u8; frame.len];
        ImageGenerator::draw_star_16bit(&mut data, frame, 7, 0, 40000, 2);

        assert!(px16(&data, 8, 7, 0) > 0, "the star's centre must be lit");
        assert_eq!(
            px16(&data, 8, 0, 1),
            0,
            "the next row's first pixel is not the star's overhang"
        );
    }

    #[test]
    fn zero_noise_level_yields_uniform_flat_frames() {
        let generator = ImageGenerator::new(ImagePattern::Flat).with_noise_level(0.0);
        let data8 = generator.generate_8bit(8, 8, 1);
        assert!(data8.iter().all(|&v| v == data8[0]));
        let data16 = generator.generate_16bit(8, 8, 1);
        assert!(data16.chunks_exact(2).all(|c| c == &data16[0..2]));
    }
}

// ===== Tests for the simulated camera state =====

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod state_tests {
    use super::{SimulatedCameraConfig, SimulatedCameraState};
    use crate::{BayerPattern, ControlType};

    #[test]
    fn test_new_state() {
        let config = SimulatedCameraConfig::default();
        let state = SimulatedCameraState::new(config);

        assert!(!state.is_open);
        assert!(!state.is_initialized);
        assert_eq!(state.binning, (1, 1));
    }

    #[test]
    fn test_image_dimensions() {
        let config = SimulatedCameraConfig::default();
        let mut state = SimulatedCameraState::new(config);

        let (w, h) = state.get_current_image_dimensions();
        assert_eq!(w, 3072);
        assert_eq!(h, 2048);

        // When binning changes, the ROI should be updated to binned coordinates
        // (this is done by the ASCOM Alpaca server in practice)
        state.binning = (2, 2);
        state.roi.width = 1536; // 3072 / 2
        state.roi.height = 1024; // 2048 / 2
        let (w, h) = state.get_current_image_dimensions();
        assert_eq!(w, 1536);
        assert_eq!(h, 1024);
    }

    #[test]
    fn test_buffer_size() {
        let config = SimulatedCameraConfig::default();
        let state = SimulatedCameraState::new(config);

        // 3072 * 2048 * 2 bytes (16-bit) * 1 channel = 12,582,912
        assert_eq!(state.calculate_buffer_size(), 12_582_912);
    }

    #[test]
    fn test_stop_exposure() {
        let config = SimulatedCameraConfig::default();
        let mut state = SimulatedCameraState::new(config);

        // Set exposure time via parameter (start_exposure reads from this)
        state.parameters.insert(ControlType::Exposure, 10_000_000.0); // 10 s — the exposure clock starts after frame generation, so the assertions below land well inside the window
        state.start_exposure();

        // Exposure should be in progress
        assert!(!state.is_exposure_complete());
        assert!(state.exposure_start.is_some());
        assert!(state.captured_image.is_some());

        // Stop the exposure (but keep image data)
        state.stop_exposure();

        // After stopping, exposure_start should be None
        assert!(state.exposure_start.is_none());
        // is_exposure_complete returns true when exposure_start is None
        assert!(state.is_exposure_complete());
        // Remaining time should be 0
        assert_eq!(state.get_remaining_exposure_us(), 0);
        // Image data should still be available
        assert!(state.captured_image.is_some());
        assert!(state.captured_image_metadata.is_some());
    }

    #[test]
    fn test_abort_exposure() {
        let config = SimulatedCameraConfig::default();
        let mut state = SimulatedCameraState::new(config);

        // Set exposure time via parameter (start_exposure reads from this)
        state.parameters.insert(ControlType::Exposure, 10_000_000.0); // 10 s — the exposure clock starts after frame generation, so the assertions below land well inside the window
        state.start_exposure();

        // Exposure should be in progress
        assert!(!state.is_exposure_complete());
        assert!(state.exposure_start.is_some());
        assert!(state.captured_image.is_some());

        // Abort the exposure (and discard image data)
        state.abort_exposure();

        // After aborting, exposure_start should be None
        assert!(state.exposure_start.is_none());
        // is_exposure_complete returns true when exposure_start is None
        assert!(state.is_exposure_complete());
        // Remaining time should be 0
        assert_eq!(state.get_remaining_exposure_us(), 0);
        // Image data should be cleared
        assert!(state.captured_image.is_none());
        assert!(state.captured_image_metadata.is_none());
    }

    #[test]
    fn test_update_temperature_cooling() {
        let config = SimulatedCameraConfig::default().with_cooler();
        let mut state = SimulatedCameraState::new(config);

        // Set up cooling: current temp is 20C, target is 0C, PWM is max
        state.current_temperature = 20.0;
        state.target_temperature = 0.0;
        state.cooler_pwm = 255.0;

        let initial_temp = state.current_temperature;

        // Update temperature several times
        for _ in 0..10 {
            state.update_temperature();
        }

        // Temperature should have decreased
        assert!(state.current_temperature < initial_temp);
        // CurTemp parameter should be updated
        assert!(
            (state.parameters.get(&ControlType::CurTemp).unwrap() - state.current_temperature)
                .abs()
                < f64::EPSILON
        );
    }

    #[test]
    fn test_update_temperature_warming() {
        let config = SimulatedCameraConfig::default().with_cooler();
        let mut state = SimulatedCameraState::new(config);

        // Camera is cold and cooler is off
        state.current_temperature = 0.0;
        state.cooler_pwm = 0.0;

        let initial_temp = state.current_temperature;

        // Update temperature several times
        for _ in 0..10 {
            state.update_temperature();
        }

        // Temperature should have increased toward ambient (20C)
        assert!(state.current_temperature > initial_temp);
        assert!(state.current_temperature <= 20.0);
    }

    #[test]
    fn test_get_channels_mono() {
        let config = SimulatedCameraConfig::default(); // Mono camera
        let state = SimulatedCameraState::new(config);

        // Mono camera should have 1 channel
        assert_eq!(state.get_channels(), 1);
    }

    #[test]
    fn test_get_channels_color_no_debayer() {
        let config = SimulatedCameraConfig::default().with_color(BayerPattern::RGGB);
        let state = SimulatedCameraState::new(config);

        // Color camera with debayer disabled should return 1 channel
        assert_eq!(state.get_channels(), 1);
    }

    #[test]
    fn test_get_channels_color_debayer() {
        let config = SimulatedCameraConfig::default().with_color(BayerPattern::RGGB);
        let mut state = SimulatedCameraState::new(config);

        // Enable debayer
        state.debayer_enabled = true;

        // Color camera with debayer enabled should return 3 channels
        assert_eq!(state.get_channels(), 3);
    }

    #[test]
    fn test_bytes_per_pixel_8bit() {
        let config = SimulatedCameraConfig::default();
        let mut state = SimulatedCameraState::new(config);

        state.bit_depth = 8;
        assert_eq!(state.get_bytes_per_pixel(), 1);
    }

    #[test]
    fn test_bytes_per_pixel_16bit() {
        let config = SimulatedCameraConfig::default();
        let mut state = SimulatedCameraState::new(config);

        state.bit_depth = 16;
        assert_eq!(state.get_bytes_per_pixel(), 2);

        // Also test intermediate values (12-bit, etc.)
        state.bit_depth = 12;
        assert_eq!(state.get_bytes_per_pixel(), 2);
    }

    #[test]
    fn test_buffer_size_with_binning_and_channels() {
        let config = SimulatedCameraConfig::default().with_color(BayerPattern::RGGB);
        let mut state = SimulatedCameraState::new(config);

        // Set 2x2 binning, 8-bit mode, and enable debayer (3 channels)
        // When binning changes, ROI should be updated to binned coordinates
        state.binning = (2, 2);
        state.roi.width = 1536; // 3072 / 2
        state.roi.height = 1024; // 2048 / 2
        state.bit_depth = 8;
        state.debayer_enabled = true;

        // 1536 * 1024 * 1 byte * 3 channels = 4,718,592
        assert_eq!(state.calculate_buffer_size(), 4_718_592);
    }

    #[test]
    fn test_remaining_exposure_no_exposure_started() {
        let config = SimulatedCameraConfig::default();
        let state = SimulatedCameraState::new(config);

        // No exposure started, should return 0
        assert_eq!(state.get_remaining_exposure_us(), 0);
        // Should be considered complete
        assert!(state.is_exposure_complete());
    }

    #[test]
    fn test_start_exposure_uses_parameter() {
        let config = SimulatedCameraConfig::default();
        let mut state = SimulatedCameraState::new(config);

        // Set exposure parameter
        state.parameters.insert(ControlType::Exposure, 5_000_000.0); // 5 seconds

        state.start_exposure();

        // exposure_duration_us should be set from parameter
        assert_eq!(state.exposure_duration_us, 5_000_000);
    }

    #[test]
    fn an_exposure_past_the_sdk_word_reports_a_saturated_remainder() {
        // The SDK reports the remainder in `u32` microseconds, which runs out at
        // ~71 minutes; nothing clamps the Exposure parameter to its own control
        // range on the way in. Narrowing wrapped a two-hour exposure round to
        // 2.9e9 µs, so it read as *falling* and then jumping — a caller that
        // trusted it would have committed to the readout early.
        let mut state = SimulatedCameraState::new(SimulatedCameraConfig::default());
        state
            .parameters
            .insert(ControlType::Exposure, 7_200_000_000.0); // 2 hours
        state.start_exposure();

        assert_eq!(state.get_remaining_exposure_us(), u32::MAX);
        assert!(!state.is_exposure_complete());
    }
}
