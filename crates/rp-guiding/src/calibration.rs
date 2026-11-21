//! Calibration: the direction and the speed with which a West and a North
//! pulse move the stars on the sensor, measured in legs of pulses, and when
//! a record serves a run. See `docs/services/guider.md` (Calibration).

use std::time::Duration;

use tracing::debug;

use crate::stars::Offset;

/// Arcseconds in a degree.
const ARCSEC_PER_DEG: f64 = 3_600.0;

/// The arcseconds a pixel of 1 µm spans behind 1 mm of focal length: the
/// arcseconds in a radian, over the µm in a mm.
const ARCSEC_PER_UM_PER_MM: f64 = 206.265;

/// The furthest from the celestial equator, in degrees, a declination is
/// taken to be when the RA rate is scaled by its cosine.
const MAX_SCALED_DEC_DEG: f64 = 89.0;

/// The furthest from the celestial equator, in degrees, a record is
/// stored from.
const MAX_STORED_DEC_DEG: f64 = 60.0;

/// The orthogonality check's thresholds, in degrees from 90°.
const ORTHOGONALITY_WARN_DEG: f64 = 12.5;
const ORTHOGONALITY_FAIL_DEG: f64 = 30.0;

/// The rate checks' thresholds, as a fraction of the expected value.
const RATE_WARN: f64 = 0.20;
const RATE_FAIL: f64 = 0.50;

/// How closely a record's value must match a run's to be the same one: a
/// part in a billion, so that a value read back from the record's file
/// compares equal to the value it was written from.
const SAME_WITHIN: f64 = 1e-9;

/// A guide pulse's direction, as ASCOM names it: North toward increasing
/// Dec, East toward increasing RA.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Toward increasing Dec.
    North,
    /// Toward decreasing Dec.
    South,
    /// Toward increasing RA.
    East,
    /// Toward decreasing RA.
    West,
}

/// A guide pulse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pulse {
    /// Its direction.
    pub direction: Direction,
    /// Its duration.
    pub duration: Duration,
}

/// The side of the pier the mount is on, as a calibration's slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PierSide {
    /// East of the pier.
    East,
    /// West of the pier.
    West,
    /// A mount that reports no pier side, or reports it unknown.
    Unknown,
}

/// The camera a calibration is made with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CameraGeometry {
    /// The size of the camera's pixel, in µm (`PixelSizeX`).
    pub pixel_size_um: f64,
    /// The sensor's width, in the camera's pixels (`CameraXSize`).
    pub width: u32,
    /// The sensor's height, in the camera's pixels (`CameraYSize`).
    pub height: u32,
    /// The effective binning: `camera.binning` for a monochrome camera, 2
    /// for an RGGB one.
    pub binning: u32,
}

/// The guide rates a mount reports, in degrees a second.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GuideRates {
    /// `GuideRateRightAscension`.
    pub ra_deg_per_s: f64,
    /// `GuideRateDeclination`.
    pub dec_deg_per_s: f64,
}

/// What a calibration is made on, and what a record is compared with
/// before it serves a run.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Setup {
    /// The pier side the mount is on.
    pub pier_side: PierSide,
    /// The camera.
    pub camera: CameraGeometry,
    /// The guide scope's focal length (`optics.focal_length_mm`).
    pub focal_length_mm: f64,
    /// The mount's guide rates; `None` for a mount that reports none.
    pub guide_rates: Option<GuideRates>,
}

/// The rates the mount's guide rates should move the stars at, in pixels
/// a second.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExpectedRates {
    /// The RA rate, at the equator.
    pub ra_px_per_s: f64,
    /// The Dec rate.
    pub dec_px_per_s: f64,
}

impl Setup {
    /// The pixel scale, in arcseconds a pixel of the measured frame.
    #[must_use]
    pub fn pixel_scale_arcsec(&self) -> f64 {
        ARCSEC_PER_UM_PER_MM * self.camera.pixel_size_um * f64::from(self.camera.binning)
            / self.focal_length_mm
    }

    /// The rates the mount's guide rates should move the stars at; `None`
    /// for a mount that reports no guide rate.
    #[must_use]
    pub fn expected_rates(&self) -> Option<ExpectedRates> {
        let scale = self.pixel_scale_arcsec();
        self.guide_rates.map(|rates| ExpectedRates {
            ra_px_per_s: rates.ra_deg_per_s * ARCSEC_PER_DEG / scale,
            dec_px_per_s: rates.dec_deg_per_s * ARCSEC_PER_DEG / scale,
        })
    }

    /// Whether `other` has the same pier side, camera, focal length and
    /// guide rates. A setup without guide rates is the same only as
    /// another without them.
    fn same_as(&self, other: &Self) -> bool {
        let rates = match (self.guide_rates, other.guide_rates) {
            (None, None) => true,
            (Some(mine), Some(theirs)) => {
                same(mine.ra_deg_per_s, theirs.ra_deg_per_s)
                    && same(mine.dec_deg_per_s, theirs.dec_deg_per_s)
            }
            _ => false,
        };
        self.pier_side == other.pier_side
            && self.camera.width == other.camera.width
            && self.camera.height == other.camera.height
            && self.camera.binning == other.camera.binning
            && same(self.camera.pixel_size_um, other.camera.pixel_size_um)
            && same(self.focal_length_mm, other.focal_length_mm)
            && rates
    }
}

/// The calibration settings of the guider's configuration.
///
/// They are `calibration.*`, the pulse bounds (`mount.min_pulse`,
/// `mount.max_pulse`) and whether Dec is calibrated. The service validates
/// them when it loads its configuration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CalibrationParams {
    /// How far the West and the North leg move the stars
    /// (`calibration.distance_px`).
    pub distance_px: f64,
    /// The steps a leg aims for (`calibration.steps`).
    pub steps: u32,
    /// The steps after which a leg that has not covered its distance fails
    /// (`calibration.max_steps`).
    pub max_steps: u32,
    /// How far the stars move North before the Dec axis is measured
    /// (`calibration.backlash_clear_px`).
    pub backlash_clear_px: f64,
    /// The step for a mount that reports no guide rate
    /// (`calibration.fallback_step`).
    pub fallback_step: Duration,
    /// The longest a calibration may take (`calibration.timeout`).
    pub timeout: Duration,
    /// The shortest pulse (`mount.min_pulse`).
    pub min_pulse: Duration,
    /// The longest pulse (`mount.max_pulse`).
    pub max_pulse: Duration,
    /// Whether the Dec axis is calibrated: false when `control.dec.mode`
    /// is `"off"`.
    pub dec: bool,
}

impl Default for CalibrationParams {
    /// The guider configuration's defaults, with Dec calibrated.
    fn default() -> Self {
        Self {
            distance_px: 25.0,
            steps: 12,
            max_steps: 60,
            backlash_clear_px: 3.0,
            fallback_step: Duration::from_millis(750),
            timeout: Duration::from_secs(600),
            min_pulse: Duration::from_millis(20),
            max_pulse: Duration::from_millis(2_500),
            dec: true,
        }
    }
}

/// One calibrated axis.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Axis {
    /// The direction in which the axis's pulse, West for RA and North for
    /// Dec, moves the stars on the sensor: degrees counter-clockwise from
    /// +x toward +y, from 0 up to 360.
    pub angle_deg: f64,
    /// The speed with which the pulse moves the stars, in pixels a second;
    /// for RA, as it would be at the equator.
    pub rate_px_per_s: f64,
    /// The steps of the leg that measured it.
    pub steps: u32,
    /// The pulse of each of its steps.
    pub step: Duration,
}

/// The calibrated Dec axis.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DecAxis {
    /// The axis.
    pub axis: Axis,
    /// The pulse time of the South leg that passed before the stars moved
    /// back, for the operator.
    pub backlash: Duration,
}

/// A cross-check of a calibration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, derive_more::Display)]
pub enum Check {
    /// The angle between the two axes, as its distance from 90°.
    #[display("orthogonality")]
    Orthogonality,
    /// The RA rate against the rate the RA guide rate should give.
    #[display("RA rate")]
    RaRate,
    /// The Dec rate against the rate the Dec guide rate should give.
    #[display("Dec rate")]
    DecRate,
    /// For a mount that reports no guide rate: the ratio of the RA rate at
    /// the declination to the Dec rate, against the cosine of the
    /// declination.
    #[display("rate ratio")]
    RateRatio,
}

impl Check {
    /// The value above which the check warns, and the one above which it
    /// fails.
    const fn thresholds(self) -> (f64, f64) {
        match self {
            Self::Orthogonality => (ORTHOGONALITY_WARN_DEG, ORTHOGONALITY_FAIL_DEG),
            Self::RaRate | Self::DecRate | Self::RateRatio => (RATE_WARN, RATE_FAIL),
        }
    }
}

/// A cross-check that passed its warning threshold.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Warning {
    /// The check.
    pub check: Check,
    /// Its value.
    pub value: f64,
    /// The warning threshold it passed.
    pub threshold: f64,
}

/// A calibration that passed: the record a run guides with.
#[derive(Debug, Clone, PartialEq)]
pub struct Calibration {
    /// What it was made on.
    pub setup: Setup,
    /// The declination the mount reported, in degrees.
    pub declination_deg: f64,
    /// The RA axis.
    pub ra: Axis,
    /// The Dec axis; `None` when Dec was not calibrated.
    pub dec: Option<DecAxis>,
    /// The cross-checks that passed their warning threshold.
    pub warnings: Vec<Warning>,
}

impl Calibration {
    /// The angle between the two axes as its distance from 90°, in
    /// degrees, whichever way the image is mirrored; `None` without a Dec
    /// axis.
    #[must_use]
    pub fn orthogonality_error_deg(&self) -> Option<f64> {
        self.dec.map(|dec| {
            let between = (dec.axis.angle_deg - self.ra.angle_deg).rem_euclid(180.0);
            (between - 90.0).abs()
        })
    }

    /// Whether the record is stored: it was made within 60° of the
    /// celestial equator.
    #[must_use]
    pub fn storable(&self) -> bool {
        self.declination_deg.abs() <= MAX_STORED_DEC_DEG
    }

    /// Whether the record serves a run on `setup`: the same pier side,
    /// camera, focal length and guide rates, and a Dec axis when the run
    /// corrects Dec (`dec`).
    #[must_use]
    pub fn serves(&self, setup: &Setup, dec: bool) -> bool {
        self.setup.same_as(setup) && (self.dec.is_some() || !dec)
    }

    /// The RA rate at `declination_deg`, in pixels a second: the record's
    /// rate times the cosine of the declination, held to 89° from the
    /// equator.
    #[must_use]
    pub fn ra_rate_at(&self, declination_deg: f64) -> f64 {
        self.ra.rate_px_per_s * scaling_cosine(declination_deg)
    }

    /// Each cross-check that applies, with its value.
    fn check_values(&self) -> Vec<(Check, f64)> {
        let mut values = Vec::new();
        if let Some(error) = self.orthogonality_error_deg() {
            values.push((Check::Orthogonality, error));
        }
        match (self.setup.expected_rates(), self.dec) {
            (Some(expected), dec) => {
                values.push((
                    Check::RaRate,
                    relative(self.ra.rate_px_per_s, expected.ra_px_per_s),
                ));
                if let Some(dec) = dec {
                    values.push((
                        Check::DecRate,
                        relative(dec.axis.rate_px_per_s, expected.dec_px_per_s),
                    ));
                }
            }
            (None, Some(dec)) => values.push((
                Check::RateRatio,
                relative(
                    self.ra_rate_at(self.declination_deg) / dec.axis.rate_px_per_s,
                    scaling_cosine(self.declination_deg),
                ),
            )),
            (None, None) => {}
        }
        values
    }
}

/// A leg of a calibration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, derive_more::Display)]
pub enum Leg {
    /// West, until the stars have moved `calibration.distance_px`.
    #[display("West")]
    West,
    /// East, as many steps as the West leg took.
    #[display("East")]
    East,
    /// North, until the stars have moved `calibration.backlash_clear_px`.
    #[display("backlash-clearing North")]
    ClearBacklash,
    /// North, until the stars have moved `calibration.distance_px`.
    #[display("North")]
    North,
    /// South, as many steps as the North leg took.
    #[display("South")]
    South,
}

/// Why a calibration failed.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub enum CalibrationError {
    /// A leg had not covered its distance after `calibration.max_steps`
    /// steps.
    #[error("the {leg} leg had not moved the stars far enough after calibration.max_steps steps")]
    OutOfSteps {
        /// The leg.
        leg: Leg,
    },
    /// The calibration took longer than `calibration.timeout`.
    #[error("the calibration took longer than calibration.timeout, in its {leg} leg")]
    TimedOut {
        /// The leg it was in.
        leg: Leg,
    },
    /// A cross-check passed its failure threshold.
    #[error("the {check} check failed: {value} is above {threshold}")]
    CheckFailed {
        /// The check.
        check: Check,
        /// Its value.
        value: f64,
        /// The failure threshold.
        threshold: f64,
    },
}

/// What a calibration does after a frame.
#[derive(Debug, Clone)]
pub enum Progress {
    /// Send the pulse, then take a frame.
    Pulse(Calibrator, Pulse),
    /// Send no pulse, and take another frame.
    Wait(Calibrator),
    /// The calibration passed.
    Done(Calibration),
}

/// A calibration in progress. Each frame's offset goes in, and the next
/// pulse comes out.
#[derive(Debug, Clone)]
pub struct Calibrator {
    setup: Setup,
    declination_deg: f64,
    params: CalibrationParams,
    started: Duration,
    ra_step: Duration,
    dec_step: Duration,
    stage: Stage,
    /// Where the stars were when the leg began; `None` until the first
    /// frame with an offset.
    start: Option<Offset>,
    /// The pulses the leg has sent.
    steps: u32,
}

/// A leg, with what the legs before it measured.
#[derive(Debug, Clone, Copy)]
enum Stage {
    West,
    East { ra: Axis },
    ClearBacklash { ra: Axis },
    North { ra: Axis },
    South { ra: Axis, dec: Axis },
}

impl Stage {
    const fn leg(self) -> Leg {
        match self {
            Self::West => Leg::West,
            Self::East { .. } => Leg::East,
            Self::ClearBacklash { .. } => Leg::ClearBacklash,
            Self::North { .. } => Leg::North,
            Self::South { .. } => Leg::South,
        }
    }
}

impl Calibrator {
    /// A calibration on `setup` at `declination_deg`, starting at `now`.
    ///
    /// `now` is a time since an epoch of the caller's, on a clock that
    /// never runs backward. Each axis's step is the pulse that moves the stars
    /// `params.distance_px / params.steps` at the rate the axis's guide
    /// rate should give, the RA rate at the declination held to 89° from
    /// the equator; `params.fallback_step` for a mount that reports no
    /// guide rate. Both are held within `params.min_pulse` and
    /// `params.max_pulse`.
    #[must_use]
    pub fn new(
        setup: Setup,
        declination_deg: f64,
        params: CalibrationParams,
        now: Duration,
    ) -> Self {
        let per_step = params.distance_px / f64::from(params.steps);
        let fallback = held(params.fallback_step, &params);
        let (ra_step, dec_step) = setup
            .expected_rates()
            .map_or((fallback, fallback), |rates| {
                (
                    held_step(
                        per_step / (rates.ra_px_per_s * scaling_cosine(declination_deg)),
                        &params,
                    ),
                    held_step(per_step / rates.dec_px_per_s, &params),
                )
            });
        debug!(?ra_step, ?dec_step, declination_deg, "calibration started");
        Self {
            setup,
            declination_deg,
            params,
            started: now,
            ra_step,
            dec_step,
            stage: Stage::West,
            start: None,
            steps: 0,
        }
    }

    /// Takes a frame's offset at `now`, and says what comes next.
    ///
    /// The frame is the one taken after the last pulse, or the selection's
    /// frame for the first call. Its offset is `None` for a star-lost or a
    /// failed frame, which is no step.
    ///
    /// # Errors
    ///
    /// [`CalibrationError::TimedOut`] once more than `params.timeout` has
    /// passed since the start, [`CalibrationError::OutOfSteps`] for a leg
    /// that has not covered its distance after `params.max_steps` steps,
    /// and [`CalibrationError::CheckFailed`] for a result that fails a
    /// cross-check.
    pub fn frame(
        mut self,
        offset: Option<Offset>,
        now: Duration,
    ) -> Result<Progress, CalibrationError> {
        if now.saturating_sub(self.started) > self.params.timeout {
            debug!(leg = %self.stage.leg(), "calibration timed out");
            return Err(CalibrationError::TimedOut {
                leg: self.stage.leg(),
            });
        }
        let Some(offset) = offset else {
            debug!(leg = %self.stage.leg(), "calibration frame without an offset");
            return Ok(Progress::Wait(self));
        };
        loop {
            let start = *self.start.get_or_insert(offset);
            let moved = distance(start, offset);
            let next = match self.stage {
                Stage::West if moved >= self.params.distance_px => Stage::East {
                    ra: self.axis(
                        start,
                        offset,
                        self.ra_step,
                        scaling_cosine(self.declination_deg),
                    ),
                },
                Stage::East { ra } if self.steps >= ra.steps => {
                    if !self.params.dec {
                        return self.finish(ra, None);
                    }
                    Stage::ClearBacklash { ra }
                }
                Stage::ClearBacklash { ra } if moved >= self.params.backlash_clear_px => {
                    Stage::North { ra }
                }
                Stage::North { ra } if moved >= self.params.distance_px => Stage::South {
                    ra,
                    dec: self.axis(start, offset, self.dec_step, 1.0),
                },
                Stage::South { ra, dec } if self.steps >= dec.steps => {
                    let backlash = self.backlash(start, offset, &dec);
                    return self.finish(
                        ra,
                        Some(DecAxis {
                            axis: dec,
                            backlash,
                        }),
                    );
                }
                _ => {
                    if self.steps >= self.params.max_steps {
                        debug!(leg = %self.stage.leg(), moved, "calibration leg out of steps");
                        return Err(CalibrationError::OutOfSteps {
                            leg: self.stage.leg(),
                        });
                    }
                    return Ok(self.pulse());
                }
            };
            debug!(
                ended = %self.stage.leg(),
                steps = self.steps,
                moved,
                next = %next.leg(),
                "calibration leg ended"
            );
            self.stage = next;
            self.start = Some(offset);
            self.steps = 0;
        }
    }

    /// The leg's next step.
    const fn pulse(mut self) -> Progress {
        self.steps = self.steps.saturating_add(1);
        let (direction, duration) = match self.stage {
            Stage::West => (Direction::West, self.ra_step),
            Stage::East { .. } => (Direction::East, self.ra_step),
            Stage::ClearBacklash { .. } | Stage::North { .. } => (Direction::North, self.dec_step),
            Stage::South { .. } => (Direction::South, self.dec_step),
        };
        Progress::Pulse(
            self,
            Pulse {
                direction,
                duration,
            },
        )
    }

    /// The axis the leg that ends at `end` measured: the direction from
    /// its start, and the distance over its pulse time, divided by
    /// `cosine` to take the RA rate to the equator.
    fn axis(&self, start: Offset, end: Offset, step: Duration, cosine: f64) -> Axis {
        let pulse_time = step.saturating_mul(self.steps).as_secs_f64();
        Axis {
            angle_deg: (end.y - start.y)
                .atan2(end.x - start.x)
                .to_degrees()
                .rem_euclid(360.0),
            rate_px_per_s: distance(start, end) / pulse_time / cosine,
            steps: self.steps,
            step,
        }
    }

    /// The South leg's pulse time less the time its travel back along the
    /// Dec axis takes at the Dec rate, within the leg's pulse time.
    fn backlash(&self, start: Offset, end: Offset, dec: &Axis) -> Duration {
        let pulse_time = self.dec_step.saturating_mul(self.steps);
        let (sin, cos) = dec.angle_deg.to_radians().sin_cos();
        let back_px = -(end.x - start.x).mul_add(cos, (end.y - start.y) * sin);
        Duration::try_from_secs_f64(pulse_time.as_secs_f64() - back_px / dec.rate_px_per_s)
            .map_or(Duration::ZERO, |backlash| backlash.min(pulse_time))
    }

    /// The record of the legs' axes, held to the cross-checks.
    fn finish(self, ra: Axis, dec: Option<DecAxis>) -> Result<Progress, CalibrationError> {
        let mut calibration = Calibration {
            setup: self.setup,
            declination_deg: self.declination_deg,
            ra,
            dec,
            warnings: Vec::new(),
        };
        for (check, value) in calibration.check_values() {
            let (warn, fail) = check.thresholds();
            if value.is_nan() || value > fail {
                debug!(%check, value, "calibration cross-check failed");
                return Err(CalibrationError::CheckFailed {
                    check,
                    value,
                    threshold: fail,
                });
            }
            if value > warn {
                calibration.warnings.push(Warning {
                    check,
                    value,
                    threshold: warn,
                });
            }
        }
        debug!(
            ra_angle_deg = calibration.ra.angle_deg,
            ra_rate_px_per_s = calibration.ra.rate_px_per_s,
            dec = ?calibration.dec,
            warnings = calibration.warnings.len(),
            "calibration passed"
        );
        Ok(Progress::Done(calibration))
    }
}

/// The cosine a run scales the RA rate by: of the declination, held to
/// 89° from the equator.
fn scaling_cosine(declination_deg: f64) -> f64 {
    declination_deg
        .abs()
        .min(MAX_SCALED_DEC_DEG)
        .to_radians()
        .cos()
}

/// A step of `secs` seconds, held within the pulse bounds; the longest
/// pulse when `secs` is no duration.
fn held_step(secs: f64, params: &CalibrationParams) -> Duration {
    Duration::try_from_secs_f64(secs).map_or(params.max_pulse, |step| held(step, params))
}

/// `step`, held within the pulse bounds.
fn held(step: Duration, params: &CalibrationParams) -> Duration {
    step.max(params.min_pulse).min(params.max_pulse)
}

/// How far `actual` lies from `expected`, as a fraction of `expected`.
fn relative(actual: f64, expected: f64) -> f64 {
    (actual - expected).abs() / expected
}

/// Whether two values of a record are the same one.
fn same(a: f64, b: f64) -> bool {
    (a - b).abs() <= SAME_WITHIN * a.abs().max(b.abs())
}

fn distance(a: Offset, b: Offset) -> f64 {
    (b.x - a.x).hypot(b.y - a.y)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::frame::{Frame, SensorType};
    use crate::stars::{GuideStars, StarParams};
    use guide_simulator::{
        CameraConfig, Cloud, DecBacklash, DecSense, GuideDirection, PulseDelivery, Seeing,
        SimConfig, Simulator, SkyConfig, SkyOffset, Star, StarField, SIDEREAL_ARCSEC_PER_S,
    };

    /// What each step costs besides its pulse: the frame after it.
    const EXPOSURE: Duration = Duration::from_secs(2);

    /// The rate a guide rate of 0.5 × sidereal moves the stars at, at 2″ a
    /// pixel, in pixels a second.
    const HALF_SIDEREAL_PX_PER_S: f64 = 0.5 * SIDEREAL_ARCSEC_PER_S / 2.0;

    fn close(actual: f64, expected: f64, tolerance: f64) {
        assert!(
            (actual - expected).abs() <= tolerance,
            "{actual} is not within {tolerance} of {expected}"
        );
    }

    fn half_sidereal() -> GuideRates {
        let deg_per_s = 0.5 * SIDEREAL_ARCSEC_PER_S / ARCSEC_PER_DEG;
        GuideRates {
            ra_deg_per_s: deg_per_s,
            dec_deg_per_s: deg_per_s,
        }
    }

    /// A camera at 2″ a pixel on the East side, with guide rates of 0.5 ×
    /// sidereal.
    fn setup() -> Setup {
        Setup {
            pier_side: PierSide::East,
            camera: CameraGeometry {
                pixel_size_um: 2.0,
                width: 320,
                height: 240,
                binning: 1,
            },
            focal_length_mm: 206.265,
            guide_rates: Some(half_sidereal()),
        }
    }

    /// A mount as the sensor sees it: each pulse moves the stars along its
    /// axis at its rate, once a Dec reversal has taken up the play.
    struct Scripted {
        position: Offset,
        /// The direction and the rate at the declination in which a West
        /// pulse moves the stars.
        ra: (f64, f64),
        /// The same for a North pulse.
        dec: (f64, f64),
        /// The Dec pulse time a reversal takes up before the stars move.
        play: Duration,
        meshed: Direction,
        play_left: Duration,
        /// How far the stars drift with each pulse.
        drift: Offset,
    }

    impl Scripted {
        /// A mount whose pulses run 5 % faster than its guide rates say,
        /// with West moving the stars at 30° and North at 120°.
        fn new() -> Self {
            Self {
                position: Offset::default(),
                ra: (30.0, 1.05 * HALF_SIDEREAL_PX_PER_S),
                dec: (120.0, 1.05 * HALF_SIDEREAL_PX_PER_S),
                play: Duration::ZERO,
                meshed: Direction::North,
                play_left: Duration::ZERO,
                drift: Offset::default(),
            }
        }

        fn apply(&mut self, pulse: Pulse) {
            let mut secs = pulse.duration.as_secs_f64();
            let ((angle, rate), sign) = match pulse.direction {
                Direction::West => (self.ra, 1.0),
                Direction::East => (self.ra, -1.0),
                Direction::North => (self.dec, 1.0),
                Direction::South => (self.dec, -1.0),
            };
            if matches!(pulse.direction, Direction::North | Direction::South)
                && pulse.direction != self.meshed
            {
                let taken = pulse.duration.min(self.play_left);
                secs -= taken.as_secs_f64();
                self.play_left -= taken;
                if self.play_left.is_zero() {
                    self.meshed = pulse.direction;
                    self.play_left = self.play;
                }
            }
            let (sin, cos) = f64::to_radians(angle).sin_cos();
            self.position.x += sign * rate * secs * cos + self.drift.x;
            self.position.y += sign * rate * secs * sin + self.drift.y;
        }
    }

    /// Runs a calibration on `mount`, one frame after each pulse.
    fn run(
        mount: &mut Scripted,
        setup: Setup,
        declination_deg: f64,
        params: CalibrationParams,
    ) -> (Result<Calibration, CalibrationError>, Vec<Pulse>) {
        let mut calibrator = Calibrator::new(setup, declination_deg, params, Duration::ZERO);
        let mut now = Duration::ZERO;
        let mut pulses = Vec::new();
        loop {
            match calibrator.frame(Some(mount.position), now) {
                Ok(Progress::Pulse(next, pulse)) => {
                    mount.apply(pulse);
                    pulses.push(pulse);
                    now += pulse.duration + EXPOSURE;
                    calibrator = next;
                }
                Ok(Progress::Wait(next)) => calibrator = next,
                Ok(Progress::Done(calibration)) => return (Ok(calibration), pulses),
                Err(error) => return (Err(error), pulses),
            }
        }
    }

    fn calibrate(mount: &mut Scripted) -> (Calibration, Vec<Pulse>) {
        let (result, pulses) = run(mount, setup(), 0.0, CalibrationParams::default());
        (result.unwrap(), pulses)
    }

    /// The pulses' directions, with how many came in a row.
    fn runs(pulses: &[Pulse]) -> Vec<(Direction, usize)> {
        let mut runs: Vec<(Direction, usize)> = Vec::new();
        for pulse in pulses {
            match runs.last_mut() {
                Some((direction, count)) if *direction == pulse.direction => *count += 1,
                _ => runs.push((pulse.direction, 1)),
            }
        }
        runs
    }

    #[test]
    fn the_legs_go_west_then_east_then_north_then_south() {
        let (_, pulses) = calibrate(&mut Scripted::new());
        // 25 px in steps of 25/12 px at 1.05 × the expected rate is 12 steps;
        // the 3 px that clear the backlash take 2 more North.
        assert_eq!(
            runs(&pulses),
            [
                (Direction::West, 12),
                (Direction::East, 12),
                (Direction::North, 14),
                (Direction::South, 12)
            ]
        );
    }

    #[test]
    fn each_axis_is_measured_from_its_leg() {
        let (calibration, pulses) = calibrate(&mut Scripted::new());
        let step = Duration::from_secs_f64(25.0 / 12.0 / HALF_SIDEREAL_PX_PER_S);
        assert!(pulses.iter().all(|pulse| pulse.duration == step));
        close(calibration.ra.angle_deg, 30.0, 1e-9);
        close(
            calibration.ra.rate_px_per_s,
            1.05 * HALF_SIDEREAL_PX_PER_S,
            1e-9,
        );
        assert_eq!(calibration.ra.steps, 12);
        assert_eq!(calibration.ra.step, step);
        let dec = calibration.dec.unwrap();
        close(dec.axis.angle_deg, 120.0, 1e-9);
        close(dec.axis.rate_px_per_s, 1.05 * HALF_SIDEREAL_PX_PER_S, 1e-9);
        assert_eq!(dec.axis.steps, 12);
        assert_eq!(dec.backlash, Duration::ZERO);
        close(calibration.orthogonality_error_deg().unwrap(), 0.0, 1e-9);
        assert_eq!(calibration.warnings, []);
    }

    #[test]
    fn the_ra_rate_is_recorded_at_the_equator_and_its_step_sized_for_the_declination() {
        let mut mount = Scripted::new();
        mount.ra.1 *= 0.5;
        let (result, pulses) = run(&mut mount, setup(), 60.0, CalibrationParams::default());
        let calibration = result.unwrap();
        close(
            calibration.ra.rate_px_per_s,
            1.05 * HALF_SIDEREAL_PX_PER_S,
            1e-9,
        );
        close(
            calibration.ra_rate_at(60.0),
            0.525 * HALF_SIDEREAL_PX_PER_S,
            1e-9,
        );
        assert_eq!(calibration.ra.steps, 12);
        let dec = calibration.dec.unwrap();
        close(dec.axis.rate_px_per_s, 1.05 * HALF_SIDEREAL_PX_PER_S, 1e-9);
        close(
            calibration.ra.step.as_secs_f64(),
            2.0 * dec.axis.step.as_secs_f64(),
            1e-6,
        );
        for pulse in pulses {
            let step = match pulse.direction {
                Direction::West | Direction::East => calibration.ra.step,
                Direction::North | Direction::South => dec.axis.step,
            };
            assert_eq!(pulse.duration, step, "{pulse:?}");
        }
        assert_eq!(calibration.warnings, []);
    }

    #[test]
    fn a_frame_without_an_offset_sends_no_pulse_and_is_no_step() {
        let mut mount = Scripted::new();
        let mut calibrator =
            Calibrator::new(setup(), 0.0, CalibrationParams::default(), Duration::ZERO);
        let mut wests = 0;
        loop {
            let offset = (wests != 3).then_some(mount.position);
            match calibrator.frame(offset, Duration::ZERO).unwrap() {
                Progress::Pulse(next, pulse) if pulse.direction == Direction::West => {
                    mount.apply(pulse);
                    wests += 1;
                    calibrator = next;
                }
                Progress::Pulse(..) => break,
                Progress::Wait(next) => {
                    assert_eq!(wests, 3);
                    calibrator = next;
                    let Progress::Pulse(next, pulse) = calibrator
                        .frame(Some(mount.position), Duration::ZERO)
                        .unwrap()
                    else {
                        panic!("no pulse after the frame with an offset");
                    };
                    mount.apply(pulse);
                    wests += 1;
                    calibrator = next;
                }
                Progress::Done(_) => panic!("the calibration ended in its West leg"),
            }
        }
        assert_eq!(wests, 12);
    }

    #[test]
    fn a_leg_that_does_not_move_the_stars_fails_at_max_steps() {
        let mut mount = Scripted::new();
        mount.ra.1 = 0.0;
        let (result, pulses) = run(&mut mount, setup(), 0.0, CalibrationParams::default());
        assert_eq!(
            result.unwrap_err(),
            CalibrationError::OutOfSteps { leg: Leg::West }
        );
        assert_eq!(pulses.len(), 60);
    }

    #[test]
    fn the_backlash_clearing_leg_fails_at_max_steps_too() {
        let mut mount = Scripted::new();
        mount.dec.1 = 0.0;
        let (result, pulses) = run(&mut mount, setup(), 0.0, CalibrationParams::default());
        assert_eq!(
            result.unwrap_err(),
            CalibrationError::OutOfSteps {
                leg: Leg::ClearBacklash
            }
        );
        assert_eq!(runs(&pulses).last(), Some(&(Direction::North, 60)));
    }

    #[test]
    fn a_calibration_longer_than_its_timeout_fails() {
        let params = CalibrationParams {
            timeout: Duration::from_secs(5),
            ..CalibrationParams::default()
        };
        let calibrator = Calibrator::new(setup(), 0.0, params, Duration::from_secs(100));
        let Progress::Pulse(calibrator, _) = calibrator
            .frame(Some(Offset::default()), Duration::from_secs(105))
            .unwrap()
        else {
            panic!("no pulse at the timeout");
        };
        assert_eq!(
            calibrator
                .frame(
                    Some(Offset::default()),
                    Duration::from_secs(105) + Duration::from_nanos(1)
                )
                .unwrap_err(),
            CalibrationError::TimedOut { leg: Leg::West }
        );
    }

    #[test]
    fn without_dec_only_ra_is_calibrated() {
        let params = CalibrationParams {
            dec: false,
            ..CalibrationParams::default()
        };
        let (result, pulses) = run(&mut Scripted::new(), setup(), 0.0, params);
        let calibration = result.unwrap();
        assert_eq!(
            runs(&pulses),
            [(Direction::West, 12), (Direction::East, 12)]
        );
        assert_eq!(calibration.dec, None);
        assert_eq!(calibration.orthogonality_error_deg(), None);
        assert_eq!(calibration.warnings, []);
    }

    #[test]
    fn axes_off_square_warn_and_further_off_fail() {
        let mut mount = Scripted::new();
        mount.dec.0 = 140.0;
        let (calibration, _) = calibrate(&mut mount);
        let [warning] = calibration.warnings[..] else {
            panic!("{:?}", calibration.warnings);
        };
        assert_eq!(warning.check, Check::Orthogonality);
        close(warning.value, 20.0, 1e-9);
        assert_eq!(warning.threshold, 12.5);

        let mut mount = Scripted::new();
        mount.dec.0 = 155.0;
        let (result, _) = run(&mut mount, setup(), 0.0, CalibrationParams::default());
        let CalibrationError::CheckFailed {
            check,
            value,
            threshold,
        } = result.unwrap_err()
        else {
            panic!("not a failed check");
        };
        assert_eq!(check, Check::Orthogonality);
        close(value, 35.0, 1e-9);
        assert_eq!(threshold, 30.0);
    }

    #[test]
    fn a_rate_off_its_guide_rate_warns_and_further_off_fails() {
        let mut mount = Scripted::new();
        mount.ra.1 = 1.3 * HALF_SIDEREAL_PX_PER_S;
        mount.dec.1 = 0.75 * HALF_SIDEREAL_PX_PER_S;
        let (calibration, _) = calibrate(&mut mount);
        let checks: Vec<(Check, f64)> = calibration
            .warnings
            .iter()
            .map(|warning| (warning.check, warning.value))
            .collect();
        assert_eq!(checks.len(), 2, "{checks:?}");
        assert_eq!(checks[0].0, Check::RaRate);
        close(checks[0].1, 0.3, 1e-9);
        assert_eq!(checks[1].0, Check::DecRate);
        close(checks[1].1, 0.25, 1e-9);

        let mut mount = Scripted::new();
        mount.dec.1 = 0.45 * HALF_SIDEREAL_PX_PER_S;
        let (result, _) = run(&mut mount, setup(), 0.0, CalibrationParams::default());
        let CalibrationError::CheckFailed { check, value, .. } = result.unwrap_err() else {
            panic!("not a failed check");
        };
        assert_eq!(check, Check::DecRate);
        close(value, 0.55, 1e-9);
    }

    #[test]
    fn without_guide_rates_the_fallback_step_is_used_and_the_rates_compared_with_each_other() {
        let setup = Setup {
            guide_rates: None,
            ..setup()
        };
        let (result, pulses) = run(
            &mut Scripted::new(),
            setup,
            0.0,
            CalibrationParams::default(),
        );
        assert_eq!(result.unwrap().warnings, []);
        assert!(pulses
            .iter()
            .all(|pulse| pulse.duration == Duration::from_millis(750)));

        let mut mount = Scripted::new();
        mount.ra.1 *= 1.3;
        let (result, _) = run(&mut mount, setup, 0.0, CalibrationParams::default());
        let calibration = result.unwrap();
        let [warning] = calibration.warnings[..] else {
            panic!("{:?}", calibration.warnings);
        };
        assert_eq!(warning.check, Check::RateRatio);
        close(warning.value, 0.3, 1e-9);
    }

    #[test]
    fn the_rate_ratio_expects_the_cosine_of_the_declination() {
        let setup = Setup {
            guide_rates: None,
            ..setup()
        };
        let mut mount = Scripted::new();
        mount.ra.1 *= 0.5;
        let (result, _) = run(&mut mount, setup, 60.0, CalibrationParams::default());
        assert_eq!(result.unwrap().warnings, []);
    }

    #[test]
    fn the_south_leg_measures_the_backlash() {
        let mut mount = Scripted::new();
        mount.play = Duration::from_millis(1_200);
        mount.play_left = mount.play;
        let (calibration, _) = calibrate(&mut mount);
        let dec = calibration.dec.unwrap();
        close(dec.backlash.as_secs_f64(), 1.2, 1e-6);
        close(dec.axis.rate_px_per_s, 1.05 * HALF_SIDEREAL_PX_PER_S, 1e-9);
    }

    #[test]
    fn each_rate_is_checked_against_its_own_guide_rate() {
        let mut slow_dec = setup();
        slow_dec.guide_rates.as_mut().unwrap().dec_deg_per_s *= 0.5;
        let mut mount = Scripted::new();
        mount.dec.1 *= 0.5;
        let (result, _) = run(&mut mount, slow_dec, 0.0, CalibrationParams::default());
        let calibration = result.unwrap();
        assert!(
            calibration.warnings.is_empty(),
            "{:?}",
            calibration.warnings
        );
        assert_eq!(calibration.dec.unwrap().axis.steps, 12);
    }

    #[test]
    fn the_pixel_scale_takes_the_binning() {
        // The rig of the `calibrate` example in `docs/services/guider.md`.
        let rig = Setup {
            camera: CameraGeometry {
                pixel_size_um: 1.45,
                width: 3840,
                height: 2192,
                binning: 2,
            },
            focal_length_mm: 200.0,
            guide_rates: Some(GuideRates {
                ra_deg_per_s: 0.002_089,
                dec_deg_per_s: 0.002_089,
            }),
            ..setup()
        };
        close(rig.pixel_scale_arcsec(), 2.99, 0.005);
        let expected = rig.expected_rates().unwrap();
        close(expected.ra_px_per_s, 2.51, 0.005);
        close(expected.dec_px_per_s, 2.51, 0.005);
    }

    #[test]
    fn a_south_leg_that_never_brings_the_stars_back_is_all_backlash() {
        let mut mount = Scripted::new();
        mount.play = Duration::from_secs(20);
        mount.play_left = mount.play;
        // The stars drift on along the North axis, at 120°.
        mount.drift = Offset { x: 0.0, y: 0.05 };
        let (calibration, _) = calibrate(&mut mount);
        let dec = calibration.dec.unwrap();
        assert_eq!(dec.backlash, dec.axis.step * dec.axis.steps);
    }

    #[test]
    fn a_camera_that_reports_no_pixel_size_fails_the_rate_check() {
        let setup = Setup {
            camera: CameraGeometry {
                pixel_size_um: 0.0,
                ..setup().camera
            },
            ..setup()
        };
        // No expected rate sizes the step, so it is the shortest pulse.
        let params = CalibrationParams {
            max_steps: 1_000,
            timeout: Duration::from_secs(36_000),
            ..CalibrationParams::default()
        };
        let (result, _) = run(&mut Scripted::new(), setup, 0.0, params);
        let error = result.unwrap_err();
        let CalibrationError::CheckFailed { check, value, .. } = error else {
            panic!("{error:?}");
        };
        assert_eq!(check, Check::RaRate);
        assert!(value.is_nan());
    }

    #[test]
    fn the_step_is_held_within_the_pulse_bounds() {
        let step_of = |setup: Setup, params: CalibrationParams| {
            let calibrator = Calibrator::new(setup, 0.0, params, Duration::ZERO);
            let Progress::Pulse(_, pulse) = calibrator
                .frame(Some(Offset::default()), Duration::ZERO)
                .unwrap()
            else {
                panic!("no first pulse");
            };
            pulse.duration
        };
        let floor = CalibrationParams {
            min_pulse: Duration::from_millis(600),
            ..CalibrationParams::default()
        };
        assert_eq!(step_of(setup(), floor), Duration::from_millis(600));
        let cap = CalibrationParams {
            max_pulse: Duration::from_millis(300),
            ..CalibrationParams::default()
        };
        assert_eq!(step_of(setup(), cap), Duration::from_millis(300));
        let no_rates = Setup {
            guide_rates: None,
            ..setup()
        };
        let long_fallback = CalibrationParams {
            fallback_step: Duration::from_secs(5),
            ..CalibrationParams::default()
        };
        assert_eq!(
            step_of(no_rates, long_fallback),
            Duration::from_millis(2_500)
        );
        let still = Setup {
            guide_rates: Some(GuideRates {
                ra_deg_per_s: 0.0,
                dec_deg_per_s: 0.0,
            }),
            ..setup()
        };
        assert_eq!(
            step_of(still, CalibrationParams::default()),
            Duration::from_millis(2_500)
        );
    }

    fn record(declination_deg: f64) -> Calibration {
        let axis = Axis {
            angle_deg: 0.0,
            rate_px_per_s: 3.0,
            steps: 12,
            step: Duration::from_millis(500),
        };
        Calibration {
            setup: setup(),
            declination_deg,
            ra: axis,
            dec: Some(DecAxis {
                axis: Axis {
                    angle_deg: 270.0,
                    ..axis
                },
                backlash: Duration::ZERO,
            }),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn a_record_is_stored_within_60_degrees_of_the_equator() {
        assert!(record(60.0).storable());
        assert!(record(-60.0).storable());
        assert!(!record(60.01).storable());
        assert!(!record(-75.0).storable());
    }

    #[test]
    fn a_record_serves_only_the_setup_it_was_made_on() {
        let made = record(10.0);
        assert!(made.serves(&setup(), true));
        let changes: [fn(&mut Setup); 8] = [
            |s| s.pier_side = PierSide::West,
            |s| s.camera.pixel_size_um = 2.4,
            |s| s.camera.width = 640,
            |s| s.camera.height = 480,
            |s| s.camera.binning = 2,
            |s| s.focal_length_mm = 200.0,
            |s| s.guide_rates.as_mut().unwrap().dec_deg_per_s *= 0.5,
            |s| s.guide_rates = None,
        ];
        for change in changes {
            let mut other = setup();
            change(&mut other);
            assert!(!made.serves(&other, true), "{other:?}");
        }
    }

    #[test]
    fn a_value_read_back_from_its_file_is_the_same_one() {
        let made = record(10.0);
        let mut read_back = setup();
        read_back.focal_length_mm *= 1.0 + 4.0 * f64::EPSILON;
        read_back.guide_rates.as_mut().unwrap().ra_deg_per_s *= 1.0 - 4.0 * f64::EPSILON;
        assert!(made.serves(&read_back, true));
        read_back.focal_length_mm *= 1.0 + 1e-6;
        assert!(!made.serves(&read_back, true));
    }

    #[test]
    fn a_record_without_guide_rates_serves_a_mount_that_reports_none() {
        let made = Calibration {
            setup: Setup {
                guide_rates: None,
                ..setup()
            },
            ..record(10.0)
        };
        assert!(made.serves(&made.setup, true));
        assert!(!made.serves(&setup(), true));
    }

    #[test]
    fn a_record_without_dec_serves_only_a_run_that_does_not_correct_dec() {
        let made = Calibration {
            dec: None,
            ..record(10.0)
        };
        assert!(made.serves(&setup(), false));
        assert!(!made.serves(&setup(), true));
        assert!(record(10.0).serves(&setup(), false));
    }

    #[test]
    fn the_ra_rate_scales_by_the_cosine_held_to_89_degrees() {
        let made = record(10.0);
        close(made.ra_rate_at(60.0), 1.5, 1e-9);
        close(made.ra_rate_at(-60.0), 1.5, 1e-9);
        let at_89 = 3.0 * 89_f64.to_radians().cos();
        close(made.ra_rate_at(89.0), at_89, 1e-12);
        close(made.ra_rate_at(89.9), at_89, 1e-12);
        close(made.ra_rate_at(-90.0), at_89, 1e-12);
    }

    #[test]
    fn orthogonality_is_the_same_whichever_way_the_image_is_mirrored() {
        for (ra, dec, error) in [
            (0.0, 270.0, 0.0),
            (0.0, 90.0, 0.0),
            (350.0, 80.0, 0.0),
            (10.0, 280.0, 0.0),
            (30.0, 140.0, 20.0),
            (30.0, 280.0, 20.0),
        ] {
            let mut made = record(0.0);
            made.ra.angle_deg = ra;
            made.dec.as_mut().unwrap().axis.angle_deg = dec;
            close(made.orthogonality_error_deg().unwrap(), error, 1e-9);
        }
    }

    // Closed-loop runs through the simulator: its frames measured by the
    // guide stars, its mount pulsed as the calibration says.

    fn star(east: f64, north: f64, e_per_s: f64) -> Star {
        Star {
            position: SkyOffset::new(east, north),
            e_per_s,
        }
    }

    /// A 320 × 240 frame at 2″ a pixel in 3″ seeing with a little image
    /// motion, four bright stars well inside the edge margin.
    fn simulator(adjust: impl FnOnce(&mut SimConfig)) -> Simulator {
        let mut config = SimConfig {
            seed: 11,
            camera: CameraConfig {
                width: 320,
                height: 240,
                ..CameraConfig::default()
            },
            sky: SkyConfig {
                stars: StarField::Listed(vec![
                    star(-100.0, -60.0, 20_000.0),
                    star(100.0, 60.0, 80_000.0),
                    star(0.0, 0.0, 40_000.0),
                    star(-100.0, 60.0, 10_000.0),
                ]),
                seeing: Seeing {
                    fwhm_arcsec: 3.0,
                    common_motion_arcsec: 0.1,
                    star_motion_arcsec: 0.1,
                },
                clouds: Vec::new(),
            },
            ..SimConfig::default()
        };
        adjust(&mut config);
        Simulator::new(config).unwrap()
    }

    fn measure(sim: &mut Simulator) -> Frame {
        let shot = sim.expose(EXPOSURE);
        Frame::new(&shot.pixels.view(), SensorType::Monochrome, Some(65_535)).unwrap()
    }

    const fn guide_direction(direction: Direction) -> GuideDirection {
        match direction {
            Direction::North => GuideDirection::North,
            Direction::South => GuideDirection::South,
            Direction::East => GuideDirection::East,
            Direction::West => GuideDirection::West,
        }
    }

    /// Selects guide stars on a first frame and calibrates on the frames
    /// after it, with the declination the simulated mount points at. Also
    /// counts the frames that sent no pulse.
    fn calibrate_in(
        sim: &mut Simulator,
        params: CalibrationParams,
    ) -> (Result<Calibration, CalibrationError>, usize) {
        let declination_deg = sim.mount().config().declination_deg;
        let (mut stars, first) = GuideStars::select(&measure(sim), StarParams::default()).unwrap();
        let mut calibrator =
            Calibrator::new(setup(), declination_deg, params, sim.now().since_start());
        let mut offset = first.offset;
        let mut waits = 0;
        loop {
            match calibrator.frame(offset, sim.now().since_start()) {
                Ok(Progress::Pulse(next, pulse)) => {
                    sim.mount_mut()
                        .pulse(guide_direction(pulse.direction), pulse.duration)
                        .unwrap();
                    sim.advance(pulse.duration);
                    while sim.mount().is_pulse_guiding() {
                        sim.advance(Duration::from_millis(10));
                    }
                    calibrator = next;
                }
                Ok(Progress::Wait(next)) => {
                    waits += 1;
                    calibrator = next;
                }
                Ok(Progress::Done(calibration)) => return (Ok(calibration), waits),
                Err(error) => return (Err(error), waits),
            }
            offset = stars.measure(&measure(sim)).offset;
        }
    }

    #[test]
    fn a_calibration_through_the_simulator_finds_its_camera_s_angle_and_rates() {
        let mut sim = simulator(|config| config.camera.angle_deg = 37.0);
        let (result, waits) = calibrate_in(&mut sim, CalibrationParams::default());
        let calibration = result.unwrap();
        assert_eq!(waits, 0);
        close(calibration.ra.angle_deg, 37.0, 0.5);
        let dec = calibration.dec.unwrap();
        close(dec.axis.angle_deg, 307.0, 0.5);
        close(
            calibration.ra.rate_px_per_s,
            HALF_SIDEREAL_PX_PER_S,
            0.03 * HALF_SIDEREAL_PX_PER_S,
        );
        close(
            dec.axis.rate_px_per_s,
            HALF_SIDEREAL_PX_PER_S,
            0.03 * HALF_SIDEREAL_PX_PER_S,
        );
        assert!(dec.backlash < Duration::from_millis(100), "{dec:?}");
        assert_eq!(calibration.warnings, []);
    }

    #[test]
    fn a_mirrored_camera_away_from_the_equator() {
        let mut sim = simulator(|config| {
            config.camera.angle_deg = 37.0;
            config.camera.mirrored = true;
            config.mount.declination_deg = 40.0;
        });
        let (result, _) = calibrate_in(&mut sim, CalibrationParams::default());
        let calibration = result.unwrap();
        close(calibration.ra.angle_deg, 37.0, 0.5);
        close(calibration.dec.unwrap().axis.angle_deg, 127.0, 0.5);
        close(
            calibration.ra.rate_px_per_s,
            HALF_SIDEREAL_PX_PER_S,
            0.03 * HALF_SIDEREAL_PX_PER_S,
        );
        assert_eq!(calibration.warnings, []);
    }

    #[test]
    fn the_gti_s_coarse_pulses_and_backlash_calibrate_without_a_warning() {
        let mut sim = simulator(|config| {
            config.mount.delivery = PulseDelivery::star_adventurer_gti();
            config.mount.dec_backlash = DecBacklash {
                play_arcsec: 10.0,
                meshed: DecSense::South,
            };
        });
        let (result, _) = calibrate_in(&mut sim, CalibrationParams::default());
        let calibration = result.unwrap();
        assert_eq!(calibration.warnings, []);
    }

    #[test]
    fn the_simulated_backlash_is_measured_on_the_south_leg() {
        let mut sim = simulator(|config| {
            config.mount.dec_backlash = DecBacklash {
                play_arcsec: 10.0,
                meshed: DecSense::South,
            };
        });
        let (result, _) = calibrate_in(&mut sim, CalibrationParams::default());
        let dec = result.unwrap().dec.unwrap();
        // 10″ of play at 0.5 × sidereal.
        close(
            dec.backlash.as_secs_f64(),
            10.0 / (0.5 * SIDEREAL_ARCSEC_PER_S),
            0.1,
        );
        close(
            dec.axis.rate_px_per_s,
            HALF_SIDEREAL_PX_PER_S,
            0.03 * HALF_SIDEREAL_PX_PER_S,
        );
    }

    #[test]
    fn a_cloud_holds_a_leg_up_and_the_calibration_goes_on_after_it() {
        let mut sim = simulator(|config| {
            config.sky.clouds = vec![Cloud {
                start: Duration::from_secs(10),
                duration: Duration::from_secs(12),
                ramp: Duration::from_millis(500),
                transmission: 0.0,
            }];
        });
        let (result, waits) = calibrate_in(&mut sim, CalibrationParams::default());
        let calibration = result.unwrap();
        assert!(waits >= 3, "{waits} frames without an offset");
        close(
            calibration.ra.rate_px_per_s,
            HALF_SIDEREAL_PX_PER_S,
            0.03 * HALF_SIDEREAL_PX_PER_S,
        );
        assert_eq!(calibration.warnings, []);
    }

    #[test]
    fn near_the_pole_the_west_leg_runs_out_of_steps() {
        let mut sim = simulator(|config| config.mount.declination_deg = 88.0);
        let (result, _) = calibrate_in(&mut sim, CalibrationParams::default());
        assert_eq!(
            result.unwrap_err(),
            CalibrationError::OutOfSteps { leg: Leg::West }
        );
    }

    #[test]
    fn a_mount_slower_than_its_guide_rate_warns_and_one_far_slower_fails() {
        let mut sim = simulator(|config| config.mount.ra_rate_factor = 0.7);
        let (result, _) = calibrate_in(&mut sim, CalibrationParams::default());
        let calibration = result.unwrap();
        let [warning] = calibration.warnings[..] else {
            panic!("{:?}", calibration.warnings);
        };
        assert_eq!(warning.check, Check::RaRate);
        close(warning.value, 0.3, 0.03);

        let mut sim = simulator(|config| config.mount.ra_rate_factor = 0.4);
        let (result, _) = calibrate_in(&mut sim, CalibrationParams::default());
        let CalibrationError::CheckFailed { check, value, .. } = result.unwrap_err() else {
            panic!("not a failed check");
        };
        assert_eq!(check, Check::RaRate);
        close(value, 0.6, 0.03);
    }
}
