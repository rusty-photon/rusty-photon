//! Control: each frame's offset turned into at most one pulse on each
//! axis, the runaway stop, and the lock a dither moves. See
//! `docs/services/guider.md` (Control; Dither, settle and abort).

use std::collections::VecDeque;
use std::time::Duration;

use rand::{Rng, RngExt};
use tracing::debug;

use crate::calibration::{Calibration, Direction, Pulse};
use crate::stars::Offset;

/// The minimum move's formula, `max(A + B ÷ scale, FLOOR)` pixels, with
/// `scale` the pixel scale in arcseconds a pixel.
const MIN_MOVE_A_PX: f64 = 0.1515;
const MIN_MOVE_B_ARCSEC: f64 = 0.1548;
const MIN_MOVE_FLOOR_PX: f64 = 0.15;

/// The draws a dither makes before it takes the corner of the square
/// furthest from the current lock offset. Each draw lands outside the
/// left-out disc with a probability above 0.8.
const MAX_DITHER_DRAWS: usize = 100;

/// How Dec is corrected (`control.dec.mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// A point or a distance along the calibrated axes, in pixels: `ra` along
/// the direction a West pulse moves the stars, `dec` along the direction a
/// North pulse moves them.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AxisOffset {
    /// Along the RA axis.
    pub ra: f64,
    /// Along the Dec axis.
    pub dec: f64,
}

/// The control settings of the guider's configuration.
///
/// They are `control.*` and the pulse bounds (`mount.min_pulse`,
/// `mount.max_pulse`). The service validates them when it loads its
/// configuration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ControlParams {
    /// `control.ra.aggression`.
    pub ra_aggression: f64,
    /// `control.ra.hysteresis`.
    pub ra_hysteresis: f64,
    /// `control.dec.mode`.
    pub dec_mode: DecMode,
    /// `control.dec.aggression`.
    pub dec_aggression: f64,
    /// `control.dec.reversal_frames`.
    pub dec_reversal_frames: u32,
    /// `control.min_move_px`; `None` for the formula.
    pub min_move_px: Option<f64>,
    /// `control.runaway.frames`.
    pub runaway_frames: u32,
    /// `control.runaway.min_error_px`.
    pub runaway_min_error_px: f64,
    /// The shortest pulse (`mount.min_pulse`).
    pub min_pulse: Duration,
    /// The longest pulse (`mount.max_pulse`).
    pub max_pulse: Duration,
}

impl Default for ControlParams {
    /// The guider configuration's defaults.
    fn default() -> Self {
        Self {
            ra_aggression: 0.7,
            ra_hysteresis: 0.1,
            dec_mode: DecMode::Auto,
            dec_aggression: 1.0,
            dec_reversal_frames: 3,
            min_move_px: None,
            runaway_frames: 15,
            runaway_min_error_px: 3.0,
            min_pulse: Duration::from_millis(20),
            max_pulse: Duration::from_millis(2_500),
        }
    }
}

/// What one guide step decided.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GuideStep {
    /// The error on each axis before the correction.
    pub error: AxisOffset,
    /// The RA pulse to send, first.
    pub ra: Option<Pulse>,
    /// The Dec pulse to send, after the RA pulse has ended.
    pub dec: Option<Pulse>,
}

/// A calibrated axis, as a runaway names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, derive_more::Display)]
pub enum GuideAxis {
    /// Right ascension.
    #[display("RA")]
    Ra,
    /// Declination.
    #[display("Dec")]
    Dec,
}

/// Why the controller stopped the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ControlError {
    /// Corrections in one direction that the error did not follow.
    #[error("the {axis} corrections ran away: the error did not follow them")]
    Runaway {
        /// The axis.
        axis: GuideAxis,
    },
}

/// The guide loop's controller for one run: the error on each axis, the
/// pulses that correct it, and the lock offset a dither moves.
#[derive(Debug, Clone)]
pub struct Controller {
    params: ControlParams,
    /// The unit vector of the RA axis on the sensor.
    ra_axis: (f64, f64),
    /// The unit vector of the Dec axis; `None` when Dec is not corrected.
    dec_axis: Option<(f64, f64)>,
    ra_rate_px_per_s: f64,
    dec_rate_px_per_s: f64,
    min_move_px: f64,
    home: Offset,
    lock: AxisOffset,
    /// The RA correction the last step worked out, in pixels.
    previous_ra: f64,
    /// The direction of the last Dec pulse.
    dec_last: Option<Direction>,
    /// The frames in a row with the Dec error on the other side of the
    /// lock from the last Dec pulse.
    dec_other_side: u32,
    ra_watch: RunawayWatch,
    dec_watch: RunawayWatch,
}

impl Controller {
    /// The controller of a run that guides with `calibration` at
    /// `declination_deg`, the declination read when the run started, with
    /// `home` the frame offset where the guide stars are when guiding
    /// begins and the lock offset at zero.
    ///
    /// Dec is not corrected when `params.dec_mode` is [`DecMode::Off`] or
    /// the calibration has no Dec axis.
    #[must_use]
    pub fn new(
        calibration: &Calibration,
        declination_deg: f64,
        params: ControlParams,
        home: Offset,
    ) -> Self {
        let unit = |angle_deg: f64| {
            let (sin, cos) = angle_deg.to_radians().sin_cos();
            (cos, sin)
        };
        let dec = calibration.dec.filter(|_| params.dec_mode != DecMode::Off);
        let min_move_px = params.min_move_px.unwrap_or_else(|| {
            (MIN_MOVE_A_PX + MIN_MOVE_B_ARCSEC / calibration.setup.pixel_scale_arcsec())
                .max(MIN_MOVE_FLOOR_PX)
        });
        let controller = Self {
            params,
            ra_axis: unit(calibration.ra.angle_deg),
            dec_axis: dec.map(|dec| unit(dec.axis.angle_deg)),
            ra_rate_px_per_s: calibration.ra_rate_at(declination_deg),
            dec_rate_px_per_s: dec.map_or(0.0, |dec| dec.axis.rate_px_per_s),
            min_move_px,
            home,
            lock: AxisOffset::default(),
            previous_ra: 0.0,
            dec_last: None,
            dec_other_side: 0,
            ra_watch: RunawayWatch::default(),
            dec_watch: RunawayWatch::default(),
        };
        debug!(
            ra_rate_px_per_s = controller.ra_rate_px_per_s,
            dec_rate_px_per_s = controller.dec_rate_px_per_s,
            min_move_px,
            "controller started"
        );
        controller
    }

    /// The smallest error an axis corrects, in pixels.
    #[must_use]
    pub const fn min_move_px(&self) -> f64 {
        self.min_move_px
    }

    /// The lock offset: the point, relative to home and along the axes,
    /// the controller steers to.
    #[must_use]
    pub const fn lock(&self) -> AxisOffset {
        self.lock
    }

    /// Whether Dec is corrected.
    #[must_use]
    pub const fn corrects_dec(&self) -> bool {
        self.dec_axis.is_some()
    }

    /// The error on each axis of a frame with `offset`: the offset less
    /// home, resolved into the distances along the two axes that sum to
    /// it, less the lock offset. Without a Dec axis it is the projection
    /// onto the RA axis, and the Dec error is 0.
    #[must_use]
    pub fn error(&self, offset: Offset) -> AxisOffset {
        let along = self.along_axes(offset);
        AxisOffset {
            ra: along.ra - self.lock.ra,
            dec: along.dec - self.lock.dec,
        }
    }

    /// The distance of a frame with `offset` from the lock, in pixels on
    /// the sensor; the RA error alone when Dec is not corrected.
    #[must_use]
    pub fn distance_from_lock(&self, offset: Offset) -> f64 {
        let error = self.error(offset);
        self.dec_axis.map_or_else(
            || error.ra.abs(),
            |(dec_x, dec_y)| {
                let x = error.ra.mul_add(self.ra_axis.0, error.dec * dec_x);
                let y = error.ra.mul_add(self.ra_axis.1, error.dec * dec_y);
                x.hypot(y)
            },
        )
    }

    /// One guide step: the pulses that correct the error of a frame with
    /// `offset`.
    ///
    /// # Errors
    ///
    /// [`ControlError::Runaway`] when this step's correction on an axis
    /// would be the last of `control.runaway.frames` in one direction that
    /// the error did not follow. The step then sends nothing.
    pub fn step(&mut self, offset: Offset) -> Result<GuideStep, ControlError> {
        let error = self.error(offset);
        let ra = self.ra_pulse(error.ra);
        let dec = self.dec_pulse(error.dec);
        let (frames, min_error) = (self.params.runaway_frames, self.params.runaway_min_error_px);
        for (axis, ran_away) in [
            (
                GuideAxis::Ra,
                self.ra_watch.note(ra, error.ra, frames, min_error),
            ),
            (
                GuideAxis::Dec,
                self.dec_watch.note(dec, error.dec, frames, min_error),
            ),
        ] {
            if ran_away {
                debug!(%axis, ?error, "corrections ran away");
                return Err(ControlError::Runaway { axis });
            }
        }
        debug!(?error, ?ra, ?dec, "guide step");
        Ok(GuideStep { error, ra, dec })
    }

    /// Moves the lock offset by a dither of `amount_px`, and returns it.
    ///
    /// The new offset is drawn uniformly from the square of half-width
    /// `amount_px` around home, leaving out every point closer than half
    /// of `amount_px` to the current one. With `ra_only`, or whenever Dec
    /// is not corrected in both directions, only the RA part is drawn. An
    /// amount that is not a positive number moves nothing.
    pub fn dither<R: Rng + ?Sized>(
        &mut self,
        amount_px: f64,
        ra_only: bool,
        rng: &mut R,
    ) -> AxisOffset {
        if !(amount_px.is_finite() && amount_px > 0.0) {
            return self.lock;
        }
        let current = self.lock;
        let half = amount_px / 2.0;
        let ra_only = ra_only || self.params.dec_mode != DecMode::Auto || self.dec_axis.is_none();
        let lock = if ra_only {
            AxisOffset {
                ra: draw_outside(rng.random::<f64>(), amount_px, current.ra, half),
                dec: current.dec,
            }
        } else {
            let mut draw = || amount_px.mul_add(2.0 * rng.random::<f64>(), -amount_px);
            (0..MAX_DITHER_DRAWS)
                .map(|_| AxisOffset {
                    ra: draw(),
                    dec: draw(),
                })
                .find(|point| (point.ra - current.ra).hypot(point.dec - current.dec) >= half)
                .unwrap_or_else(|| AxisOffset {
                    ra: -amount_px.copysign(current.ra),
                    dec: -amount_px.copysign(current.dec),
                })
        };
        debug!(?current, ?lock, amount_px, "dither");
        self.lock = lock;
        self.forget();
        lock
    }

    /// Sets the lock offset to where the stars are in a frame with
    /// `offset`, as an aborted dither does.
    pub fn lock_at(&mut self, offset: Offset) {
        self.lock = self.along_axes(offset);
        debug!(lock = ?self.lock, "lock set to the stars");
        self.forget();
    }

    /// Takes up the corrections after a pause.
    pub fn resume(&mut self) {
        self.forget();
    }

    /// Takes `home` as the new home after a new selection, with the lock
    /// offset at zero.
    pub fn rehome(&mut self, home: Offset) {
        self.home = home;
        self.lock = AxisOffset::default();
        self.forget();
    }

    /// Clears what earlier steps carry into the next: the previous RA
    /// correction, the Dec reversal count and the runaway watch. The
    /// direction of the last Dec pulse stays: the gear is still meshed
    /// that way.
    fn forget(&mut self) {
        self.previous_ra = 0.0;
        self.dec_other_side = 0;
        self.ra_watch.clear();
        self.dec_watch.clear();
    }

    /// `offset` less home, along the axes.
    fn along_axes(&self, offset: Offset) -> AxisOffset {
        let x = offset.x - self.home.x;
        let y = offset.y - self.home.y;
        let (ra_x, ra_y) = self.ra_axis;
        match self.dec_axis {
            Some((dec_x, dec_y)) => {
                // The axes are within 30° of square, so the determinant is
                // at least sin 60°.
                let det = ra_x.mul_add(dec_y, -(ra_y * dec_x));
                AxisOffset {
                    ra: x.mul_add(dec_y, -(y * dec_x)) / det,
                    dec: ra_x.mul_add(y, -(ra_y * x)) / det,
                }
            }
            None => AxisOffset {
                ra: x.mul_add(ra_x, y * ra_y),
                dec: 0.0,
            },
        }
    }

    /// The RA pulse for `error`: the error blended with the previous
    /// correction and scaled.
    fn ra_pulse(&mut self, error: f64) -> Option<Pulse> {
        if error.abs() < self.min_move_px {
            self.previous_ra = 0.0;
            return None;
        }
        let correction = self.params.ra_aggression
            * (1.0 - self.params.ra_hysteresis)
                .mul_add(error, self.params.ra_hysteresis * self.previous_ra);
        self.previous_ra = correction;
        let direction = if correction > 0.0 {
            Direction::East
        } else {
            Direction::West
        };
        self.pulse(direction, correction.abs() / self.ra_rate_px_per_s)
    }

    /// The Dec pulse for `error`, which holds its direction until the
    /// error has stayed on the other side of the lock.
    fn dec_pulse(&mut self, error: f64) -> Option<Pulse> {
        self.dec_axis?;
        let wanted = if error > 0.0 {
            Some(Direction::South)
        } else if error < 0.0 {
            Some(Direction::North)
        } else {
            None
        };
        let reverses =
            matches!((self.dec_last, wanted), (Some(last), Some(wanted)) if last != wanted);
        self.dec_other_side = if reverses {
            self.dec_other_side.saturating_add(1)
        } else {
            0
        };
        let direction = wanted?;
        let allowed = match self.params.dec_mode {
            DecMode::Auto => !reverses || self.dec_other_side >= self.params.dec_reversal_frames,
            DecMode::North => direction == Direction::North,
            DecMode::South => direction == Direction::South,
            DecMode::Off => false,
        };
        if error.abs() < self.min_move_px || !allowed {
            return None;
        }
        let pulse = self.pulse(
            direction,
            self.params.dec_aggression * error.abs() / self.dec_rate_px_per_s,
        )?;
        self.dec_last = Some(direction);
        self.dec_other_side = 0;
        Some(pulse)
    }

    /// A pulse of `secs` seconds, held to the pulse bounds: none below the
    /// shortest pulse, and no longer than the longest.
    fn pulse(&self, direction: Direction, secs: f64) -> Option<Pulse> {
        let duration = Duration::try_from_secs_f64(secs)
            .ok()
            .or_else(|| (secs > 0.0).then_some(self.params.max_pulse))?;
        (duration >= self.params.min_pulse).then(|| Pulse {
            direction,
            duration: duration.min(self.params.max_pulse),
        })
    }
}

/// The corrections an axis has sent in a row in one direction, and the
/// error of each.
#[derive(Debug, Clone, Default)]
struct RunawayWatch {
    direction: Option<Direction>,
    errors: VecDeque<f64>,
}

impl RunawayWatch {
    /// Notes this step's pulse on the axis and its error. True when it is
    /// the last of `frames` in one direction, with the error at least
    /// `min_error` and no smaller than at the first of them.
    fn note(&mut self, pulse: Option<Pulse>, error: f64, frames: u32, min_error: f64) -> bool {
        let Some(pulse) = pulse else {
            self.clear();
            return false;
        };
        if self.direction != Some(pulse.direction) {
            self.clear();
            self.direction = Some(pulse.direction);
        }
        self.errors.push_back(error.abs());
        let frames = usize::try_from(frames).unwrap_or(usize::MAX);
        while self.errors.len() > frames {
            self.errors.pop_front();
        }
        match (self.errors.front(), self.errors.back()) {
            (Some(first), Some(last)) if self.errors.len() == frames => {
                *last >= min_error && last >= first
            }
            _ => false,
        }
    }

    fn clear(&mut self) {
        self.direction = None;
        self.errors.clear();
    }
}

/// A draw from `[−amount, amount]` leaving out the points closer than
/// `half` to `current`, made from `u`, a draw from `[0, 1)`.
fn draw_outside(u: f64, amount: f64, current: f64, half: f64) -> f64 {
    let below_end = (current - half).min(amount);
    let above_start = (current + half).max(-amount);
    let below = (below_end + amount).max(0.0);
    let above = (amount - above_start).max(0.0);
    let t = u * (below + above);
    if t < below {
        t - amount
    } else {
        above_start + (t - below)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::calibration::{Axis, CalibrationParams, DecAxis};
    use crate::settle::{Settle, SettleParams, Settling};
    use crate::stars::GuideStars;
    use crate::test_support::{calibrate_on, close, expose, select, send, setup, simulator};
    use guide_simulator::{
        Cloud, DecBacklash, DecSense, Drift, PeriodicTerm, PulseDelivery, SensorPoint, SimConfig,
        Simulator,
    };
    use rand::rngs::Xoshiro256PlusPlus;
    use rand::{SeedableRng, TryRng};
    use std::convert::Infallible;

    /// The calibrated rate of both axes, in pixels a second.
    const RATE: f64 = 4.0;

    /// A record whose West pulse moves the stars at `ra_deg` and North
    /// pulse at `dec_deg`, both at `RATE`.
    fn record(ra_deg: f64, dec_deg: Option<f64>) -> Calibration {
        let axis = Axis {
            angle_deg: ra_deg,
            rate_px_per_s: RATE,
            steps: 12,
            step: Duration::from_millis(550),
        };
        Calibration {
            setup: setup(),
            declination_deg: 0.0,
            ra: axis,
            dec: dec_deg.map(|angle_deg| DecAxis {
                axis: Axis { angle_deg, ..axis },
                backlash: Duration::ZERO,
            }),
            warnings: Vec::new(),
        }
    }

    /// A controller with West moving the stars toward +x and North toward
    /// −y, as on the simulator's unrotated camera, home at the origin.
    fn controller(params: ControlParams) -> Controller {
        Controller::new(&record(0.0, Some(270.0)), 0.0, params, Offset::default())
    }

    /// The offset `ra` pixels along the RA axis and `dec` along the Dec
    /// axis of [`controller`].
    const fn along(ra: f64, dec: f64) -> Offset {
        Offset { x: ra, y: -dec }
    }

    fn unit(angle_deg: f64) -> (f64, f64) {
        let (sin, cos) = angle_deg.to_radians().sin_cos();
        (cos, sin)
    }

    /// The seconds of `pulse`, which must be toward `direction`.
    fn secs(pulse: Option<Pulse>, direction: Direction) -> f64 {
        let pulse = pulse.unwrap();
        assert_eq!(pulse.direction, direction);
        pulse.duration.as_secs_f64()
    }

    fn with_min_move(min_move_px: f64) -> ControlParams {
        ControlParams {
            min_move_px: Some(min_move_px),
            ..ControlParams::default()
        }
    }

    #[test]
    fn the_error_is_the_offset_from_home_resolved_along_the_axes_less_the_lock() {
        // Axes 15° off square.
        let home = Offset { x: 5.0, y: -3.0 };
        let mut controller = Controller::new(
            &record(30.0, Some(135.0)),
            0.0,
            ControlParams::default(),
            home,
        );
        let ((ra_x, ra_y), (dec_x, dec_y)) = (unit(30.0), unit(135.0));
        let offset = Offset {
            x: home.x + 2.0 * ra_x + 3.0 * dec_x,
            y: home.y + 2.0 * ra_y + 3.0 * dec_y,
        };
        let error = controller.error(offset);
        close(error.ra, 2.0, 1e-9);
        close(error.dec, 3.0, 1e-9);
        controller.lock_at(Offset {
            x: home.x + ra_x,
            y: home.y + ra_y,
        });
        let error = controller.error(offset);
        close(error.ra, 1.0, 1e-9);
        close(error.dec, 3.0, 1e-9);
        close(
            controller.distance_from_lock(offset),
            (ra_x + 3.0 * dec_x).hypot(ra_y + 3.0 * dec_y),
            1e-9,
        );
    }

    #[test]
    fn without_dec_the_error_is_the_projection_onto_the_ra_axis() {
        let off = ControlParams {
            dec_mode: DecMode::Off,
            ..ControlParams::default()
        };
        for controller in [
            Controller::new(
                &record(30.0, None),
                0.0,
                ControlParams::default(),
                Offset::default(),
            ),
            Controller::new(&record(30.0, Some(120.0)), 0.0, off, Offset::default()),
        ] {
            let ((ra_x, ra_y), (across_x, across_y)) = (unit(30.0), unit(120.0));
            let offset = Offset {
                x: 2.0 * ra_x + 3.0 * across_x,
                y: 2.0 * ra_y + 3.0 * across_y,
            };
            assert!(!controller.corrects_dec());
            let error = controller.error(offset);
            close(error.ra, 2.0, 1e-9);
            assert_eq!(error.dec, 0.0);
            close(controller.distance_from_lock(offset), 2.0, 1e-9);
        }
    }

    #[test]
    fn the_ra_correction_blends_the_previous_one_and_is_scaled() {
        let mut controller = controller(ControlParams::default());
        let first = controller.step(along(2.0, 0.0)).unwrap();
        close(
            secs(first.ra, Direction::East),
            0.7 * 0.9 * 2.0 / RATE,
            1e-9,
        );
        assert_eq!(first.dec, None);
        let second = controller.step(along(2.0, 0.0)).unwrap();
        close(
            secs(second.ra, Direction::East),
            0.7 * (0.9 * 2.0 + 0.1 * 0.7 * 0.9 * 2.0) / RATE,
            1e-9,
        );
    }

    #[test]
    fn an_error_below_the_minimum_move_sends_nothing_and_clears_the_previous_correction() {
        let mut controller = controller(with_min_move(0.5));
        controller.step(along(2.0, 0.0)).unwrap();
        let small = controller.step(along(0.4, 0.4)).unwrap();
        assert_eq!((small.ra, small.dec), (None, None));
        let after = controller.step(along(2.0, 0.0)).unwrap();
        close(
            secs(after.ra, Direction::East),
            0.7 * 0.9 * 2.0 / RATE,
            1e-9,
        );
    }

    #[test]
    fn the_ra_pulse_follows_the_rate_at_the_run_s_declination() {
        let mut controller = Controller::new(
            &record(0.0, Some(270.0)),
            60.0,
            ControlParams::default(),
            Offset::default(),
        );
        let step = controller.step(along(2.0, 0.0)).unwrap();
        close(
            secs(step.ra, Direction::East),
            0.7 * 0.9 * 2.0 / (RATE * 0.5),
            1e-9,
        );
    }

    #[test]
    fn each_axis_pulses_toward_the_lock() {
        let pulse = |offset: Offset| controller(ControlParams::default()).step(offset).unwrap();
        close(
            secs(pulse(along(-2.0, 0.0)).ra, Direction::West),
            0.7 * 0.9 * 2.0 / RATE,
            1e-9,
        );
        close(
            secs(pulse(along(0.0, 2.0)).dec, Direction::South),
            2.0 / RATE,
            1e-9,
        );
        close(
            secs(pulse(along(0.0, -2.0)).dec, Direction::North),
            2.0 / RATE,
            1e-9,
        );
    }

    #[test]
    fn a_correction_shorter_than_the_shortest_pulse_is_not_sent_but_carries_on() {
        let mut controller = controller(ControlParams {
            min_pulse: Duration::from_millis(500),
            ..ControlParams::default()
        });
        assert_eq!(controller.step(along(1.0, 0.0)).unwrap().ra, None);
        let next = controller.step(along(10.0, 0.0)).unwrap();
        close(
            secs(next.ra, Direction::East),
            0.7 * (0.9 * 10.0 + 0.1 * 0.7 * 0.9) / RATE,
            1e-9,
        );
    }

    #[test]
    fn no_pulse_is_longer_than_the_longest() {
        let step = controller(ControlParams::default())
            .step(along(100.0, 100.0))
            .unwrap();
        assert_eq!(step.ra.unwrap().duration, Duration::from_millis(2_500));
        assert_eq!(step.dec.unwrap().duration, Duration::from_millis(2_500));
    }

    #[test]
    fn the_minimum_move_follows_the_pixel_scale_unless_it_is_set() {
        close(
            controller(ControlParams::default()).min_move_px(),
            0.1515 + 0.1548 / 2.0,
            1e-12,
        );
        assert_eq!(controller(with_min_move(0.4)).min_move_px(), 0.4);
    }

    #[test]
    fn the_dec_correction_is_scaled_by_its_aggression() {
        let mut controller = controller(ControlParams {
            dec_aggression: 0.5,
            ..ControlParams::default()
        });
        let step = controller.step(along(0.0, 2.0)).unwrap();
        close(secs(step.dec, Direction::South), 0.5 * 2.0 / RATE, 1e-9);
    }

    #[test]
    fn dec_reverses_only_once_the_error_has_stayed_on_the_other_side() {
        let mut controller = controller(ControlParams::default());
        let decs: Vec<Option<Direction>> = [2.0, -2.0, -2.0, -2.0, -2.0]
            .into_iter()
            .map(|dec| {
                controller
                    .step(along(0.0, dec))
                    .unwrap()
                    .dec
                    .map(|pulse| pulse.direction)
            })
            .collect();
        assert_eq!(
            decs,
            [
                Some(Direction::South),
                None,
                None,
                Some(Direction::North),
                Some(Direction::North)
            ]
        );
    }

    #[test]
    fn a_frame_back_on_the_first_side_starts_the_reversal_count_again() {
        let mut controller = controller(ControlParams::default());
        let decs: Vec<Option<Direction>> = [2.0, -2.0, -2.0, 0.1, -2.0, -2.0, -2.0]
            .into_iter()
            .map(|dec| {
                controller
                    .step(along(0.0, dec))
                    .unwrap()
                    .dec
                    .map(|pulse| pulse.direction)
            })
            .collect();
        assert_eq!(
            decs,
            [
                Some(Direction::South),
                None,
                None,
                None,
                None,
                None,
                Some(Direction::North)
            ]
        );
    }

    #[test]
    fn a_reversal_waits_out_its_own_count_before_turning_back() {
        let mut controller = controller(ControlParams::default());
        let decs: Vec<Option<Direction>> = [2.0, -2.0, -2.0, -2.0, 2.0, 2.0, 2.0]
            .into_iter()
            .map(|dec| {
                controller
                    .step(along(0.0, dec))
                    .unwrap()
                    .dec
                    .map(|pulse| pulse.direction)
            })
            .collect();
        assert_eq!(
            decs,
            [
                Some(Direction::South),
                None,
                None,
                Some(Direction::North),
                None,
                None,
                Some(Direction::South)
            ]
        );
    }

    #[test]
    fn a_new_home_puts_the_lock_back_at_zero() {
        let mut controller = controller(ControlParams::default());
        controller.lock_at(along(1.0, 2.0));
        let home = along(5.0, -3.0);
        controller.rehome(home);
        assert_eq!(controller.lock(), AxisOffset::default());
        assert_eq!(controller.error(home), AxisOffset::default());
    }

    #[test]
    fn the_north_and_south_modes_correct_one_way_only() {
        for (mode, sent, held) in [(DecMode::North, -2.0, 2.0), (DecMode::South, 2.0, -2.0)] {
            let params = ControlParams {
                dec_mode: mode,
                ..ControlParams::default()
            };
            assert_eq!(controller(params).step(along(0.0, held)).unwrap().dec, None);
            assert!(controller(params)
                .step(along(0.0, sent))
                .unwrap()
                .dec
                .is_some());
        }
    }

    fn runaway_params() -> ControlParams {
        ControlParams {
            runaway_frames: 3,
            runaway_min_error_px: 3.0,
            ..ControlParams::default()
        }
    }

    /// Steps through `offsets`, and returns the index of the step that ran
    /// away and its axis.
    fn run_away(controller: &mut Controller, offsets: &[Offset]) -> Option<(usize, GuideAxis)> {
        offsets
            .iter()
            .enumerate()
            .find_map(|(index, offset)| match controller.step(*offset) {
                Ok(_) => None,
                Err(ControlError::Runaway { axis }) => Some((index, axis)),
            })
    }

    #[test]
    fn corrections_the_error_does_not_follow_stop_the_run() {
        let ra = [along(4.0, 0.0), along(4.5, 0.0), along(5.0, 0.0)];
        assert_eq!(
            run_away(&mut controller(runaway_params()), &ra),
            Some((2, GuideAxis::Ra))
        );
        let dec = [along(0.0, 4.0), along(0.0, 4.5), along(0.0, 5.0)];
        assert_eq!(
            run_away(&mut controller(runaway_params()), &dec),
            Some((2, GuideAxis::Dec))
        );
        let level = [along(4.0, 0.0), along(3.9, 0.0), along(4.0, 0.0)];
        assert_eq!(
            run_away(&mut controller(runaway_params()), &level),
            Some((2, GuideAxis::Ra))
        );
    }

    #[test]
    fn an_error_that_shrinks_or_stays_small_is_no_runaway() {
        let shrinking = [along(5.0, 0.0), along(4.5, 0.0), along(4.0, 0.0)];
        assert_eq!(
            run_away(&mut controller(runaway_params()), &shrinking),
            None
        );
        let small = [along(1.0, 0.0), along(2.0, 0.0), along(2.9, 0.0)];
        assert_eq!(run_away(&mut controller(runaway_params()), &small), None);
    }

    #[test]
    fn a_step_without_a_correction_or_in_the_other_direction_starts_the_count_again() {
        let paused = [
            along(4.0, 0.0),
            along(0.1, 0.0),
            along(4.5, 0.0),
            along(5.0, 0.0),
            along(5.5, 0.0),
        ];
        assert_eq!(
            run_away(&mut controller(runaway_params()), &paused),
            Some((4, GuideAxis::Ra))
        );
        let turned = [
            along(4.0, 0.0),
            along(-4.5, 0.0),
            along(5.0, 0.0),
            along(5.5, 0.0),
            along(6.0, 0.0),
        ];
        assert_eq!(
            run_away(&mut controller(runaway_params()), &turned),
            Some((4, GuideAxis::Ra))
        );
    }

    #[test]
    fn resuming_rehoming_and_moving_the_lock_start_the_count_again() {
        let actions: [fn(&mut Controller); 3] = [
            Controller::resume,
            |c| c.rehome(Offset::default()),
            |c| c.lock_at(Offset::default()),
        ];
        for action in actions {
            let mut controller = controller(runaway_params());
            assert_eq!(
                run_away(&mut controller, &[along(4.0, 0.0), along(4.5, 0.0)]),
                None
            );
            action(&mut controller);
            let step = controller.step(along(5.0, 0.0)).unwrap();
            close(secs(step.ra, Direction::East), 0.7 * 0.9 * 5.0 / RATE, 1e-9);
            assert_eq!(run_away(&mut controller, &[along(5.5, 0.0)]), None);
        }
    }

    #[test]
    fn a_dither_starts_the_previous_correction_and_the_runaway_count_again() {
        let mut controller = controller(runaway_params());
        assert_eq!(
            run_away(&mut controller, &[along(4.0, 0.0), along(4.5, 0.0)]),
            None
        );
        let lock = controller.dither(1.0, true, &mut Xoshiro256PlusPlus::seed_from_u64(3));
        let step = controller.step(along(lock.ra + 5.0, 0.0)).unwrap();
        close(secs(step.ra, Direction::East), 0.7 * 0.9 * 5.0 / RATE, 1e-9);
    }

    #[test]
    fn dithers_stay_within_the_square_and_move_at_least_half_the_amount() {
        let mut rng = Xoshiro256PlusPlus::seed_from_u64(7);
        let mut controller = controller(ControlParams::default());
        let mut previous = AxisOffset::default();
        let mut quadrants = [false; 4];
        for _ in 0..2_000 {
            let lock = controller.dither(4.0, false, &mut rng);
            assert_eq!(lock, controller.lock());
            assert!(lock.ra.abs() <= 4.0 && lock.dec.abs() <= 4.0, "{lock:?}");
            assert!(
                (lock.ra - previous.ra).hypot(lock.dec - previous.dec) >= 2.0,
                "{previous:?} to {lock:?}"
            );
            quadrants[usize::from(lock.ra > 0.0) + 2 * usize::from(lock.dec > 0.0)] = true;
            previous = lock;
        }
        assert_eq!(quadrants, [true; 4]);
    }

    #[test]
    fn an_ra_only_dither_keeps_the_dec_part() {
        let mut rng = Xoshiro256PlusPlus::seed_from_u64(9);
        for params in [
            ControlParams::default(),
            ControlParams {
                dec_mode: DecMode::North,
                ..ControlParams::default()
            },
        ] {
            let ra_only = params.dec_mode == DecMode::Auto;
            let mut controller = controller(params);
            controller.lock_at(along(0.0, 1.5));
            let mut previous = controller.lock();
            for _ in 0..1_000 {
                let lock = controller.dither(4.0, ra_only, &mut rng);
                assert_eq!(lock.dec, 1.5);
                assert!(lock.ra.abs() <= 4.0, "{lock:?}");
                assert!(
                    (lock.ra - previous.ra).abs() >= 2.0,
                    "{previous:?} to {lock:?}"
                );
                previous = lock;
            }
        }
    }

    #[test]
    fn an_amount_that_is_not_a_positive_number_moves_nothing() {
        let mut rng = Xoshiro256PlusPlus::seed_from_u64(1);
        let mut controller = controller(ControlParams::default());
        for amount in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(
                controller.dither(amount, false, &mut rng),
                AxisOffset::default()
            );
        }
    }

    /// A generator whose every draw is the middle of the range.
    struct Middle;

    impl TryRng for Middle {
        type Error = Infallible;

        fn try_next_u32(&mut self) -> Result<u32, Infallible> {
            Ok(1 << 31)
        }

        fn try_next_u64(&mut self) -> Result<u64, Infallible> {
            Ok(1 << 63)
        }

        fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Infallible> {
            dst.fill(0);
            Ok(())
        }
    }

    #[test]
    fn a_dither_that_draws_only_near_the_lock_takes_the_furthest_corner() {
        let mut controller = controller(ControlParams::default());
        assert_eq!(
            controller.dither(4.0, false, &mut Middle),
            AxisOffset {
                ra: -4.0,
                dec: -4.0
            }
        );
    }

    #[test]
    fn a_draw_outside_leaves_out_the_points_near_the_current_one() {
        for (u, current, drawn) in [
            (0.0, 0.0, -4.0),
            (0.25, 0.0, -3.0),
            (0.5, 0.0, 2.0),
            (0.75, 0.0, 3.0),
            (0.5, 4.0, -1.0),
            (0.5, -4.0, 1.0),
            (0.5, 10.0, 0.0),
        ] {
            close(draw_outside(u, 4.0, current, 2.0), drawn, 1e-12);
        }
    }

    // Closed-loop runs through the simulator: select, calibrate, lock, and
    // guide, each frame's pulses sent before the next frame.

    /// A run guiding in the simulator.
    struct Run {
        sim: Simulator,
        stars: GuideStars,
        controller: Controller,
        /// The field's true shift when guiding began.
        home: SensorPoint,
    }

    /// One guided frame.
    #[derive(Debug)]
    struct Guided {
        /// How far the field truly lies from the lock, in pixels.
        off_px: f64,
        /// The measured distance from the lock; `None` for a star-lost
        /// frame.
        distance_px: Option<f64>,
        step: Option<GuideStep>,
    }

    impl Run {
        /// Selects, calibrates and locks in a simulator `adjust` sets up.
        fn start(adjust: impl FnOnce(&mut SimConfig), params: ControlParams) -> Self {
            let mut sim = simulator(adjust);
            let (mut stars, offset) = select(&mut sim);
            let calibration =
                calibrate_on(&mut sim, &mut stars, offset, CalibrationParams::default())
                    .0
                    .unwrap();
            let (frame, shot) = expose(&mut sim);
            let home = stars.measure(&frame).offset.unwrap();
            let declination_deg = sim.mount().config().declination_deg;
            Self {
                controller: Controller::new(&calibration, declination_deg, params, home),
                sim,
                stars,
                home: shot.truth.field_shift,
            }
        }

        /// Takes one frame and sends its pulses.
        fn frame(&mut self) -> Result<Guided, ControlError> {
            let (frame, shot) = expose(&mut self.sim);
            let offset = self.stars.measure(&frame).offset;
            let lock = self.controller.lock();
            // The lock on the sensor of the simulator's unrotated camera.
            let x = shot.truth.field_shift.x - self.home.x - lock.ra;
            let y = shot.truth.field_shift.y - self.home.y + lock.dec;
            let step = offset
                .map(|offset| self.controller.step(offset))
                .transpose()?;
            for pulse in step.iter().flat_map(|step| [step.ra, step.dec]).flatten() {
                send(&mut self.sim, pulse);
            }
            Ok(Guided {
                off_px: x.hypot(y),
                distance_px: offset.map(|offset| self.controller.distance_from_lock(offset)),
                step,
            })
        }

        fn frames(&mut self, count: usize) -> Result<Vec<Guided>, ControlError> {
            (0..count).map(|_| self.frame()).collect()
        }

        fn now(&self) -> Duration {
            self.sim.now().since_start()
        }
    }

    fn rms(frames: &[Guided]) -> f64 {
        let count = f64::from(u32::try_from(frames.len()).unwrap());
        (frames.iter().map(|frame| frame.off_px.powi(2)).sum::<f64>() / count).sqrt()
    }

    /// A mount that drifts 2″ a minute in RA and 1″ in Dec, with 5″ of
    /// periodic error over 8 minutes.
    fn drifting(config: &mut SimConfig) {
        config.mount.drift = Drift {
            east_arcsec_per_min: 2.0,
            north_arcsec_per_min: 1.0,
        };
        config.mount.periodic_error = vec![PeriodicTerm {
            amplitude_arcsec: 5.0,
            period: Duration::from_secs(480),
            phase_deg: 0.0,
        }];
    }

    #[test]
    fn guiding_holds_the_field_against_drift_and_periodic_error() {
        let mut run = Run::start(drifting, ControlParams::default());
        let guided = run.frames(400).unwrap();
        let rms = rms(&guided[20..]);
        // Unguided, the field drifts some 16 px in these frames. Guided, it
        // stays within about the minimum move, 0.23 px.
        assert!(rms < 0.35, "{rms} px");
        assert!(guided.iter().all(|frame| frame.step.is_some()));
    }

    #[test]
    fn the_gti_s_coarse_pulses_and_backlash_still_guide() {
        let mut run = Run::start(
            |config| {
                drifting(config);
                config.mount.delivery = PulseDelivery::star_adventurer_gti();
                config.mount.dec_backlash = DecBacklash {
                    play_arcsec: 10.0,
                    meshed: DecSense::South,
                };
            },
            ControlParams::default(),
        );
        let guided = run.frames(400).unwrap();
        let rms = rms(&guided[20..]);
        assert!(rms < 0.35, "{rms} px");
    }

    #[test]
    fn guiding_takes_up_again_after_a_cloud() {
        // The same run, without the cloud, begins guiding at `start`.
        let start = Run::start(drifting, ControlParams::default()).now();
        let cloud = Cloud {
            start: start + Duration::from_secs(60),
            duration: Duration::from_secs(40),
            ramp: Duration::from_secs(2),
            transmission: 0.0,
        };
        let mut run = Run::start(
            |config| {
                drifting(config);
                config.sky.clouds = vec![cloud];
            },
            ControlParams::default(),
        );
        let guided = run.frames(160).unwrap();
        let lost = guided.iter().filter(|frame| frame.step.is_none()).count();
        assert!(lost >= 10, "{lost} star-lost frames");
        let last_lost = guided
            .iter()
            .rposition(|frame| frame.step.is_none())
            .unwrap();
        let after = &guided[last_lost + 10..];
        let rms = rms(after);
        assert!(
            rms < 0.35,
            "{rms} px over {} frames after the cloud",
            after.len()
        );
    }

    #[test]
    fn a_dither_settles_at_its_new_lock() {
        let mut run = Run::start(drifting, ControlParams::default());
        run.frames(20).unwrap();
        let lock = run
            .controller
            .dither(5.0, false, &mut Xoshiro256PlusPlus::seed_from_u64(2));
        assert!(lock.ra.hypot(lock.dec) >= 2.5);
        let mut settle = Settle::new(
            SettleParams {
                tolerance_px: 1.0,
                time: Duration::from_secs(10),
                timeout: Duration::from_secs(60),
            },
            run.now(),
        );
        let mut settling = Settling::Settling;
        while settling == Settling::Settling {
            let guided = run.frame().unwrap();
            settling = settle.frame(guided.distance_px, run.now());
        }
        assert_eq!(settling, Settling::Settled);
        // Forty frames see more of the periodic error's swing than they
        // average away.
        let held = run.frames(40).unwrap();
        let rms = rms(&held);
        assert!(rms < 0.45, "{rms} px from the new lock");
    }

    #[test]
    fn a_mount_that_stops_following_the_pulses_runs_away() {
        let mut run = Run::start(
            |config| {
                config.mount.drift = Drift {
                    east_arcsec_per_min: 10.0,
                    north_arcsec_per_min: 0.0,
                };
            },
            ControlParams::default(),
        );
        run.frames(20).unwrap();
        run.sim.mount_mut().set_rate_factors(0.0, 0.0).unwrap();
        let error = (0..200).find_map(|_| run.frame().err()).unwrap();
        assert_eq!(
            error,
            ControlError::Runaway {
                axis: GuideAxis::Ra
            }
        );
    }

    #[test]
    fn a_mount_that_moves_the_wrong_way_runs_away() {
        let mut run = Run::start(drifting, ControlParams::default());
        run.frames(20).unwrap();
        run.sim.mount_mut().set_rate_factors(-1.0, -1.0).unwrap();
        let error = (0..100).find_map(|_| run.frame().err()).unwrap();
        assert!(matches!(error, ControlError::Runaway { .. }), "{error:?}");
    }

    #[test]
    fn with_dec_off_only_ra_is_guided() {
        let mut run = Run::start(
            drifting,
            ControlParams {
                dec_mode: DecMode::Off,
                ..ControlParams::default()
            },
        );
        let guided = run.frames(100).unwrap();
        assert!(guided
            .iter()
            .filter_map(|frame| frame.step)
            .all(|step| step.dec.is_none()));
        let worst_ra = guided
            .iter()
            .filter_map(|frame| frame.step.map(|step| step.error.ra.abs()))
            .skip(20)
            .fold(0.0, f64::max);
        assert!(worst_ra < 1.0, "{worst_ra} px");
    }
}
