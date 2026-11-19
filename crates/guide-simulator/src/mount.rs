//! The mount model: where the telescope points, as simulated time runs.
//!
//! The pointing is the sum of drift, periodic error, the sidereal motion
//! of a mount that does not track, guide pulses as the mount delivers
//! them, Dec through its backlash, and slews. See
//! `docs/crates/guide-simulator.md` (The mount model).

use std::cmp::Reverse;
use std::f64::consts::TAU;
use std::time::Duration;

use num_traits::ToPrimitive;

use crate::config::{self, ConfigError};
use crate::geometry::{coordinate, SkyOffset};
use crate::random::Stream;
use crate::time::SimTime;

/// The sidereal rate: the arcseconds of RA axis a mount turns a second
/// to follow the sky.
pub const SIDEREAL_ARCSEC_PER_S: f64 = 15.041_067;

/// The mount's stream within a run's seed.
const MOUNT_STREAM: u64 = 0;

/// A guide pulse's direction, as ASCOM names it: North toward
/// increasing Dec, East toward increasing RA.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GuideDirection {
    /// Toward increasing Dec.
    North,
    /// Toward decreasing Dec.
    South,
    /// Toward increasing RA.
    East,
    /// Toward decreasing RA.
    West,
}

/// The side of the pier the mount is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PierSide {
    /// East of the pier.
    East,
    /// West of the pier.
    West,
}

/// A direction of Dec motion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DecSense {
    /// Toward increasing Dec.
    North,
    /// Toward decreasing Dec.
    South,
}

/// Why the mount refused a pulse, a slew or a start of tracking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MountError {
    /// The mount is parked.
    #[error("the mount is parked")]
    Parked,
    /// The mount is slewing.
    #[error("the mount is slewing")]
    Slewing,
    /// A pulse on the same axis is in progress.
    #[error("a pulse on this axis is in progress")]
    AxisBusy,
}

/// The mount and how it moves.
#[derive(Debug, Clone, PartialEq)]
pub struct MountConfig {
    /// The declination the mount points at, in degrees.
    pub declination_deg: f64,
    /// The pier side the mount starts on.
    pub pier_side: PierSide,
    /// Whether the mount tracks at the start.
    pub tracking: bool,
    /// The RA guide rate the mount reports, as a fraction of sidereal.
    pub guide_rate_ra: f64,
    /// The Dec guide rate the mount reports, as a fraction of sidereal.
    pub guide_rate_dec: f64,
    /// How fast a pulse moves RA, as a fraction of the reported rate.
    pub ra_rate_factor: f64,
    /// How fast a pulse moves Dec, as a fraction of the reported rate.
    pub dec_rate_factor: f64,
    /// The drift while tracking.
    pub drift: Drift,
    /// The periodic error's terms, summed.
    pub periodic_error: Vec<PeriodicTerm>,
    /// The play in the Dec gear.
    pub dec_backlash: DecBacklash,
    /// How the mount delivers a pulse.
    pub delivery: PulseDelivery,
}

impl Default for MountConfig {
    /// A mount at the celestial equator, tracking, on the East side,
    /// with guide rates of 0.5 × sidereal delivered exactly, and no
    /// drift, periodic error or backlash.
    fn default() -> Self {
        Self {
            declination_deg: 0.0,
            pier_side: PierSide::East,
            tracking: true,
            guide_rate_ra: 0.5,
            guide_rate_dec: 0.5,
            ra_rate_factor: 1.0,
            dec_rate_factor: 1.0,
            drift: Drift::default(),
            periodic_error: Vec::new(),
            dec_backlash: DecBacklash::default(),
            delivery: PulseDelivery::default(),
        }
    }
}

impl MountConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        config::within("mount.declination_deg", self.declination_deg, -90.0, 90.0)?;
        config::non_negative("mount.guide_rate_ra", self.guide_rate_ra)?;
        config::non_negative("mount.guide_rate_dec", self.guide_rate_dec)?;
        config::finite("mount.ra_rate_factor", self.ra_rate_factor)?;
        config::finite("mount.dec_rate_factor", self.dec_rate_factor)?;
        config::finite(
            "mount.drift.east_arcsec_per_min",
            self.drift.east_arcsec_per_min,
        )?;
        config::finite(
            "mount.drift.north_arcsec_per_min",
            self.drift.north_arcsec_per_min,
        )?;
        for term in &self.periodic_error {
            config::finite(
                "mount.periodic_error.amplitude_arcsec",
                term.amplitude_arcsec,
            )?;
            config::finite("mount.periodic_error.phase_deg", term.phase_deg)?;
            config::require(
                !term.period.is_zero(),
                "mount.periodic_error.period",
                "must be above zero",
            )?;
        }
        config::non_negative(
            "mount.dec_backlash.play_arcsec",
            self.dec_backlash.play_arcsec,
        )?;
        config::finite(
            "mount.delivery.east.step_arcsec",
            self.delivery.east.step_arcsec,
        )?;
        config::finite(
            "mount.delivery.west.step_arcsec",
            self.delivery.west.step_arcsec,
        )?;
        if let Some(step) = self.delivery.dec.step_arcsec {
            config::positive("mount.delivery.dec.step_arcsec", step)?;
        }
        if let Some(late) = self.delivery.dec.late_last_step {
            config::require(
                self.delivery.dec.step_arcsec.is_some(),
                "mount.delivery.dec.late_last_step",
                "needs a Dec step",
            )?;
            config::require(
                late.earliest <= late.latest,
                "mount.delivery.dec.late_last_step",
                "has its earliest delay after its latest",
            )?;
        }
        Ok(())
    }
}

/// A constant drift on each sky axis while the mount tracks.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Drift {
    /// Arcseconds a minute toward increasing RA.
    pub east_arcsec_per_min: f64,
    /// Arcseconds a minute toward increasing Dec.
    pub north_arcsec_per_min: f64,
}

/// One sine term of the periodic error, on the RA axis, in tracked time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PeriodicTerm {
    /// Half the peak-to-peak, in arcseconds of RA axis.
    pub amplitude_arcsec: f64,
    /// The period.
    pub period: Duration,
    /// The phase at the start, in degrees.
    pub phase_deg: f64,
}

impl PeriodicTerm {
    /// The term's error after `tracked` seconds of tracking.
    fn at(&self, tracked: f64) -> f64 {
        self.amplitude_arcsec
            * (TAU / self.period.as_secs_f64())
                .mul_add(tracked, self.phase_deg.to_radians())
                .sin()
    }
}

/// The play in the Dec gear, and the way it starts meshed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DecBacklash {
    /// How far the motor moves after a reversal before the axis
    /// follows, in arcseconds.
    pub play_arcsec: f64,
    /// The direction the gear starts meshed for: a first pulse this way
    /// moves the axis at once.
    pub meshed: DecSense,
}

impl Default for DecBacklash {
    fn default() -> Self {
        Self {
            play_arcsec: 0.0,
            meshed: DecSense::North,
        }
    }
}

/// How a mount delivers a pulse. The default delivers every pulse for
/// exactly the duration asked.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PulseDelivery {
    /// An East pulse.
    pub east: RaDelivery,
    /// A West pulse.
    pub west: RaDelivery,
    /// A North or South pulse.
    pub dec: DecDelivery,
}

impl PulseDelivery {
    /// The Star Adventurer `GTi`, as measured at its park pose with the
    /// driver's default trim and a guide rate of 0.5 × sidereal
    /// (`docs/services/star-adventurer-gti.md`, `PulseGuide` lifecycle).
    #[must_use]
    pub const fn star_adventurer_gti() -> Self {
        Self {
            east: RaDelivery {
                lengthened_by: Duration::from_millis(71),
                shortened_by: Duration::ZERO,
                shortest: Duration::ZERO,
                step_arcsec: -0.40,
            },
            west: RaDelivery {
                lengthened_by: Duration::ZERO,
                shortened_by: Duration::from_millis(71),
                shortest: Duration::from_millis(2),
                step_arcsec: 1.09,
            },
            dec: DecDelivery {
                step_arcsec: Some(0.446),
                late_last_step: Some(LateStep {
                    max_pulse: Duration::from_millis(100),
                    earliest: Duration::from_millis(100),
                    latest: Duration::from_millis(1900),
                }),
            },
        }
    }
}

/// How the mount delivers an East or a West pulse: the requested length
/// plus `lengthened_by`, less `shortened_by`, at least `shortest`, then
/// a fixed step.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RaDelivery {
    /// Added to the requested length.
    pub lengthened_by: Duration,
    /// Taken from the requested length.
    pub shortened_by: Duration,
    /// The shortest length delivered.
    pub shortest: Duration,
    /// The move when the pulse ends, in arcseconds of RA axis: positive
    /// along the pulse's direction, negative against it.
    pub step_arcsec: f64,
}

/// How the mount delivers a North or a South pulse.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DecDelivery {
    /// The whole step the axis moves in, in arcseconds; continuous when
    /// `None`.
    pub step_arcsec: Option<f64>,
    /// The last step of a short pulse arriving late. Needs a step.
    pub late_last_step: Option<LateStep>,
}

/// The last step of a Dec pulse no longer than `max_pulse` arrives a
/// random delay after the pulse ends, uniform between `earliest` and
/// `latest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LateStep {
    /// The longest pulse whose last step arrives late.
    pub max_pulse: Duration,
    /// The shortest delay.
    pub earliest: Duration,
    /// The longest delay.
    pub latest: Duration,
}

/// The mount model. It holds the simulation's clock.
#[derive(Debug, Clone)]
pub struct Mount {
    config: MountConfig,
    stream: Stream,
    now: SimTime,
    side: PierSide,
    tracking: bool,
    at_park: bool,
    /// The time the mount has tracked: periodic error's and drift's time.
    tracked: Duration,
    /// The RA axis's travel from pulses and from not tracking, in
    /// arcseconds of axis, positive East.
    ra_axis_arcsec: f64,
    dec: DecAxis,
    ra_pulse: Option<Motion>,
    dec_pulse: Option<DecPulse>,
    /// Moves still to come, the latest first.
    steps: Vec<Step>,
    slew: Option<Slew>,
    /// The slews' travel.
    slewed: SkyOffset,
}

impl Mount {
    /// A mount at the start of a simulation, drawing from the run seeded
    /// with `seed`.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] names the first field the model cannot take.
    pub fn new(config: MountConfig, seed: u64) -> Result<Self, ConfigError> {
        config.validate()?;
        let mut stream = Stream::new(seed, MOUNT_STREAM);
        let play = config.dec_backlash.play_arcsec;
        let dec = DecAxis {
            position_arcsec: 0.0,
            play_arcsec: play,
            north_slack_arcsec: match config.dec_backlash.meshed {
                DecSense::North => 0.0,
                DecSense::South => play,
            },
            motor_steps: stream.uniform(),
        };
        Ok(Self {
            side: config.pier_side,
            tracking: config.tracking,
            config,
            stream,
            now: SimTime::START,
            at_park: false,
            tracked: Duration::ZERO,
            ra_axis_arcsec: 0.0,
            dec,
            ra_pulse: None,
            dec_pulse: None,
            steps: Vec::new(),
            slew: None,
            slewed: SkyOffset::ZERO,
        })
    }

    /// The configuration, with the rate factors as they are now.
    #[must_use]
    pub const fn config(&self) -> &MountConfig {
        &self.config
    }

    /// The simulation's current instant.
    #[must_use]
    pub const fn now(&self) -> SimTime {
        self.now
    }

    /// Runs the model to `t`. An instant that has passed does nothing.
    pub fn advance_to(&mut self, t: SimTime) {
        while let Some(step) = self.steps.pop_if(|step| step.at <= t) {
            self.run_to(step.at);
            match step.axis {
                Axis::Ra => self.ra_axis_arcsec += step.arcsec,
                Axis::Dec => self.dec.turn(step.arcsec),
            }
        }
        self.run_to(t);
    }

    /// Where the telescope points now, relative to where it pointed at
    /// the start.
    #[must_use]
    pub fn pointing(&self) -> SkyOffset {
        let tracked = self.tracked.as_secs_f64();
        let periodic: f64 = self
            .config
            .periodic_error
            .iter()
            .map(|term| term.at(tracked))
            .sum();
        let drift = &self.config.drift;
        let east = self.cos_dec().mul_add(
            self.ra_axis_arcsec + periodic,
            (drift.east_arcsec_per_min / 60.0) * tracked,
        );
        let north = (drift.north_arcsec_per_min / 60.0).mul_add(tracked, self.dec.position_arcsec);
        SkyOffset::new(east, north).plus(self.slewed)
    }

    /// Starts a guide pulse now.
    ///
    /// # Errors
    ///
    /// [`MountError::Parked`] while parked, [`MountError::Slewing`]
    /// while slewing, and [`MountError::AxisBusy`] while a pulse on the
    /// same axis is in progress.
    pub fn pulse(
        &mut self,
        direction: GuideDirection,
        duration: Duration,
    ) -> Result<(), MountError> {
        if self.at_park {
            return Err(MountError::Parked);
        }
        if self.slewing() {
            return Err(MountError::Slewing);
        }
        let busy = match direction {
            GuideDirection::East | GuideDirection::West => self.ra_busy(),
            GuideDirection::North | GuideDirection::South => self.dec_busy(),
        };
        if busy {
            return Err(MountError::AxisBusy);
        }
        if duration.is_zero() {
            return Ok(());
        }
        match direction {
            GuideDirection::East => self.start_ra(1.0, self.config.delivery.east, duration),
            GuideDirection::West => self.start_ra(-1.0, self.config.delivery.west, duration),
            GuideDirection::North => self.start_dec(1.0, duration),
            GuideDirection::South => self.start_dec(-1.0, duration),
        }
        Ok(())
    }

    /// Whether a pulse on either axis is in progress.
    #[must_use]
    pub fn is_pulse_guiding(&self) -> bool {
        self.ra_busy() || self.dec_busy()
    }

    /// Changes how fast pulses started from now on move each axis, as a
    /// fraction of the reported rate: a mount that stops following its
    /// pulses part way through a run.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] when a factor is not finite.
    pub fn set_rate_factors(&mut self, ra: f64, dec: f64) -> Result<(), ConfigError> {
        config::finite("mount.ra_rate_factor", ra)?;
        config::finite("mount.dec_rate_factor", dec)?;
        self.config.ra_rate_factor = ra;
        self.config.dec_rate_factor = dec;
        Ok(())
    }

    /// Whether the mount tracks.
    #[must_use]
    pub const fn tracking(&self) -> bool {
        self.tracking
    }

    /// Turns tracking on or off. A change stops an RA pulse in progress
    /// where it stands.
    ///
    /// # Errors
    ///
    /// [`MountError::Parked`] when tracking is turned on while parked.
    pub fn set_tracking(&mut self, on: bool) -> Result<(), MountError> {
        if on && self.at_park {
            return Err(MountError::Parked);
        }
        if on != self.tracking {
            self.stop_ra_pulse();
            self.tracking = on;
        }
        Ok(())
    }

    /// Whether a slew is in progress.
    #[must_use]
    pub fn slewing(&self) -> bool {
        self.slew.is_some_and(|slew| slew.end > self.now)
    }

    /// Moves the pointing by `by` at an even pace over `duration`, ending
    /// on `pier_side`. Pulses in progress stop where they stand, and a
    /// slew in progress is replaced from where it stands.
    ///
    /// # Errors
    ///
    /// [`MountError::Parked`] while parked.
    pub fn slew(
        &mut self,
        by: SkyOffset,
        duration: Duration,
        pier_side: PierSide,
    ) -> Result<(), MountError> {
        if self.at_park {
            return Err(MountError::Parked);
        }
        self.stop_ra_pulse();
        self.stop_dec_pulse();
        if duration.is_zero() {
            self.slewed = self.slewed.plus(by);
            self.side = pier_side;
            self.slew = None;
        } else {
            self.slew = Some(Slew {
                start: self.now,
                end: self.now.after(duration),
                by,
                pier_side,
            });
        }
        Ok(())
    }

    /// Whether the mount is parked.
    #[must_use]
    pub const fn at_park(&self) -> bool {
        self.at_park
    }

    /// Parks at once: tracking off, and pulses and a slew stopped where
    /// they stand.
    pub fn park(&mut self) {
        self.stop_ra_pulse();
        self.stop_dec_pulse();
        self.slew = None;
        self.tracking = false;
        self.at_park = true;
    }

    /// Unparks, leaving tracking off.
    pub const fn unpark(&mut self) {
        self.at_park = false;
    }

    /// The side of the pier the mount is on.
    #[must_use]
    pub const fn side_of_pier(&self) -> PierSide {
        self.side
    }

    /// Whether the mount is on the other side of the pier from where it
    /// started, which turns the camera's view by 180°.
    pub(crate) fn flipped(&self) -> bool {
        self.side != self.config.pier_side
    }

    fn cos_dec(&self) -> f64 {
        self.config.declination_deg.to_radians().cos()
    }

    fn ra_busy(&self) -> bool {
        self.ra_pulse.is_some_and(|pulse| pulse.end > self.now)
    }

    fn dec_busy(&self) -> bool {
        self.dec_pulse
            .is_some_and(|pulse| pulse.motion.end > self.now)
    }

    /// Runs the continuous motion from now to `t`, with no step between.
    fn run_to(&mut self, t: SimTime) {
        if t <= self.now {
            return;
        }
        let (from, to) = (self.now.secs(), t.secs());
        if self.tracking {
            self.tracked = self
                .tracked
                .saturating_add(t.since_start().saturating_sub(self.now.since_start()));
        } else {
            self.ra_axis_arcsec = SIDEREAL_ARCSEC_PER_S.mul_add(to - from, self.ra_axis_arcsec);
        }
        if let Some(pulse) = self.ra_pulse {
            self.ra_axis_arcsec += pulse.travel(from, to);
        }
        if let Some(pulse) = self.dec_pulse {
            if pulse.steps_from.is_none() {
                self.dec.turn(pulse.motion.travel(from, to));
            }
        }
        if let Some(slew) = self.slew {
            let span = slew.end.secs() - slew.start.secs();
            let fraction = overlap(from, to, slew.start.secs(), slew.end.secs()) / span;
            self.slewed = self.slewed.plus(slew.by.scaled(fraction));
        }
        self.now = t;
        if self.ra_pulse.is_some_and(|pulse| pulse.end <= t) {
            self.ra_pulse = None;
        }
        if self.dec_pulse.is_some_and(|pulse| pulse.motion.end <= t) {
            self.dec_pulse = None;
        }
        if let Some(slew) = self.slew {
            if slew.end <= t {
                self.side = slew.pier_side;
                self.slew = None;
            }
        }
    }

    fn start_ra(&mut self, sign: f64, delivery: RaDelivery, duration: Duration) {
        let delivered = duration
            .saturating_add(delivery.lengthened_by)
            .saturating_sub(delivery.shortened_by)
            .max(delivery.shortest);
        let factor = self.config.ra_rate_factor;
        let end = self.now.after(delivered);
        self.ra_pulse = Some(Motion {
            start: self.now,
            end,
            rate: sign * self.config.guide_rate_ra * SIDEREAL_ARCSEC_PER_S * factor,
        });
        self.schedule(Step {
            at: end,
            axis: Axis::Ra,
            arcsec: sign * delivery.step_arcsec * factor,
        });
    }

    fn start_dec(&mut self, sign: f64, duration: Duration) {
        let motion = Motion {
            start: self.now,
            end: self.now.after(duration),
            rate: sign
                * self.config.guide_rate_dec
                * SIDEREAL_ARCSEC_PER_S
                * self.config.dec_rate_factor,
        };
        let steps_from = self.config.delivery.dec.step_arcsec.map(|step| {
            let from = self.dec.motor_steps;
            self.schedule_dec_steps(motion, step);
            from
        });
        self.dec_pulse = Some(DecPulse { motion, steps_from });
    }

    /// Schedules the whole steps a Dec pulse moves the axis: one each
    /// time the motor crosses a step boundary, the last one late when
    /// the pulse is short enough.
    fn schedule_dec_steps(&mut self, motion: Motion, step: f64) {
        let from = self.dec.motor_steps;
        let start = motion.start.secs();
        let steps_per_s = motion.rate / step;
        let to = steps_per_s.mul_add(motion.end.secs() - start, from);
        // The axis stands at the floor of the motor's position, so it
        // moves once for each whole number the motor passes: those in
        // (from, to] going North, in (to, from] going South.
        let rising = to > from;
        let count = (to.floor() - from.floor()).abs().to_usize().unwrap_or(0);
        let mut crossings: Vec<f64> = (0..count)
            .map(|index| {
                let boundary = if rising {
                    from.floor() + coordinate(index) + 1.0
                } else {
                    from.floor() - coordinate(index)
                };
                start + (boundary - from) / steps_per_s
            })
            .collect();
        self.dec.motor_steps = to;
        let late = self.config.delivery.dec.late_last_step;
        let duration = motion
            .end
            .since_start()
            .saturating_sub(motion.start.since_start());
        if let (Some(late), Some(last)) = (late, crossings.last_mut()) {
            if duration <= late.max_pulse {
                let spread = late.latest.saturating_sub(late.earliest);
                let delay = late
                    .earliest
                    .saturating_add(spread.mul_f64(self.stream.uniform()));
                *last = motion.end.after(delay).secs();
            }
        }
        let arcsec = step.copysign(to - from);
        for at in crossings {
            self.schedule(Step {
                at: SimTime::from_secs(at),
                axis: Axis::Dec,
                arcsec,
            });
        }
    }

    fn schedule(&mut self, step: Step) {
        self.steps.push(step);
        self.steps.sort_by_key(|step| Reverse(step.at));
    }

    /// Stops an RA pulse where it stands, dropping its end step.
    fn stop_ra_pulse(&mut self) {
        self.ra_pulse = None;
        self.drop_steps(Axis::Ra);
    }

    /// Stops a Dec pulse where it stands, dropping the steps still to
    /// come, a late one included.
    fn stop_dec_pulse(&mut self) {
        if let Some(pulse) = self.dec_pulse.take() {
            if let (Some(from), Some(step)) =
                (pulse.steps_from, self.config.delivery.dec.step_arcsec)
            {
                let ran = self.now.secs() - pulse.motion.start.secs();
                self.dec.motor_steps = (pulse.motion.rate / step).mul_add(ran, from);
            }
        }
        self.drop_steps(Axis::Dec);
    }

    fn drop_steps(&mut self, axis: Axis) {
        let now = self.now;
        self.steps
            .retain(|step| step.axis != axis || step.at <= now);
    }
}

/// A motion at a constant rate between two instants.
#[derive(Debug, Clone, Copy)]
struct Motion {
    start: SimTime,
    end: SimTime,
    /// Arcseconds a second, positive East or North.
    rate: f64,
}

impl Motion {
    /// The motion's travel between `from` and `to` seconds.
    fn travel(self, from: f64, to: f64) -> f64 {
        self.rate * overlap(from, to, self.start.secs(), self.end.secs())
    }
}

#[derive(Debug, Clone, Copy)]
struct DecPulse {
    motion: Motion,
    /// The motor's position in steps when the pulse started, for an axis
    /// that moves in whole steps; `None` for a continuous axis.
    steps_from: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Axis {
    Ra,
    Dec,
}

/// A move at an instant: arcseconds of RA axis, or of Dec motor.
#[derive(Debug, Clone, Copy)]
struct Step {
    at: SimTime,
    axis: Axis,
    arcsec: f64,
}

#[derive(Debug, Clone, Copy)]
struct Slew {
    start: SimTime,
    end: SimTime,
    by: SkyOffset,
    pier_side: PierSide,
}

/// The Dec axis behind its gear.
#[derive(Debug, Clone, Copy)]
struct DecAxis {
    /// Where the axis is, in arcseconds, positive North.
    position_arcsec: f64,
    play_arcsec: f64,
    /// How far the motor can move North before the axis follows.
    north_slack_arcsec: f64,
    /// The motor's position in whole steps, for an axis that moves in
    /// them.
    motor_steps: f64,
}

impl DecAxis {
    /// Turns the motor by `arcsec`, positive North; the axis follows
    /// once the play is taken up.
    fn turn(&mut self, arcsec: f64) {
        if arcsec >= 0.0 {
            let taken = arcsec.min(self.north_slack_arcsec);
            self.north_slack_arcsec -= taken;
            self.position_arcsec += arcsec - taken;
        } else {
            let taken = (-arcsec).min(self.play_arcsec - self.north_slack_arcsec);
            self.north_slack_arcsec += taken;
            self.position_arcsec += arcsec + taken;
        }
    }
}

/// The length of the overlap of `[from, to]` and `[start, end]`.
fn overlap(from: f64, to: f64, start: f64, end: f64) -> f64 {
    (to.min(end) - from.max(start)).max(0.0)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    /// The default guide rate, 0.5 × sidereal, in arcseconds a second.
    const GUIDE: f64 = 0.5 * SIDEREAL_ARCSEC_PER_S;

    fn mount(config: MountConfig) -> Mount {
        Mount::new(config, 1).unwrap()
    }

    fn at(secs: f64) -> SimTime {
        SimTime::from_secs(secs)
    }

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    /// Pulses, runs three seconds on, and answers how far the pointing moved.
    fn travel(mount: &mut Mount, direction: GuideDirection, duration: Duration) -> SkyOffset {
        let before = mount.pointing();
        mount.pulse(direction, duration).unwrap();
        mount.advance_to(mount.now().after(Duration::from_secs(3)));
        mount.pointing().minus(before)
    }

    fn close(actual: f64, expected: f64, tolerance: f64) {
        assert!(
            (actual - expected).abs() <= tolerance,
            "{actual} is not within {tolerance} of {expected}"
        );
    }

    #[test]
    fn drift_moves_the_pointing_at_its_rate_while_tracking() {
        let mut m = mount(MountConfig {
            drift: Drift {
                east_arcsec_per_min: 6.0,
                north_arcsec_per_min: -3.0,
            },
            ..MountConfig::default()
        });
        m.advance_to(at(60.0));
        close(m.pointing().east, 6.0, 1e-9);
        close(m.pointing().north, -3.0, 1e-9);
    }

    #[test]
    fn periodic_error_follows_its_sine_scaled_by_the_cosine_of_the_declination() {
        let mut m = mount(MountConfig {
            declination_deg: 60.0,
            periodic_error: vec![PeriodicTerm {
                amplitude_arcsec: 10.0,
                period: Duration::from_secs(400),
                phase_deg: 0.0,
            }],
            ..MountConfig::default()
        });
        m.advance_to(at(100.0));
        close(m.pointing().east, 5.0, 1e-9);
        m.advance_to(at(300.0));
        close(m.pointing().east, -5.0, 1e-9);
    }

    #[test]
    fn with_tracking_off_the_pointing_runs_east_at_sidereal_and_drift_and_periodic_error_stop() {
        let mut m = mount(MountConfig {
            tracking: false,
            drift: Drift {
                east_arcsec_per_min: 6.0,
                north_arcsec_per_min: 6.0,
            },
            periodic_error: vec![PeriodicTerm {
                amplitude_arcsec: 10.0,
                period: Duration::from_secs(400),
                phase_deg: 0.0,
            }],
            ..MountConfig::default()
        });
        m.advance_to(at(10.0));
        close(m.pointing().east, 10.0 * SIDEREAL_ARCSEC_PER_S, 1e-9);
        close(m.pointing().north, 0.0, 1e-12);
    }

    #[test]
    fn an_east_pulse_moves_the_pointing_east_and_guides_for_its_duration() {
        let mut m = mount(MountConfig::default());
        m.pulse(GuideDirection::East, Duration::from_secs(1))
            .unwrap();
        m.advance_to(at(0.5));
        assert!(m.is_pulse_guiding());
        m.advance_to(at(1.0));
        assert!(!m.is_pulse_guiding());
        m.advance_to(at(2.0));
        close(m.pointing().east, GUIDE, 1e-9);
        close(m.pointing().north, 0.0, 1e-12);
    }

    #[test]
    fn a_west_pulse_moves_the_pointing_west_by_the_cosine_of_the_declination() {
        let mut m = mount(MountConfig {
            declination_deg: 60.0,
            ..MountConfig::default()
        });
        let moved = travel(&mut m, GuideDirection::West, Duration::from_secs(1));
        close(moved.east, -GUIDE / 2.0, 1e-9);
    }

    #[test]
    fn north_and_south_pulses_move_dec_at_the_dec_guide_rate() {
        let mut m = mount(MountConfig {
            guide_rate_dec: 0.25,
            ..MountConfig::default()
        });
        let north = travel(&mut m, GuideDirection::North, Duration::from_secs(2));
        close(north.north, 0.5 * SIDEREAL_ARCSEC_PER_S, 1e-9);
        let south = travel(&mut m, GuideDirection::South, Duration::from_secs(1));
        close(south.north, -0.25 * SIDEREAL_ARCSEC_PER_S, 1e-9);
    }

    #[test]
    fn a_rate_factor_scales_a_pulse_and_a_factor_of_zero_stops_it() {
        let mut m = mount(MountConfig {
            ra_rate_factor: 0.5,
            ..MountConfig::default()
        });
        close(
            travel(&mut m, GuideDirection::East, Duration::from_secs(1)).east,
            GUIDE / 2.0,
            1e-9,
        );
        m.set_rate_factors(0.0, 0.0).unwrap();
        assert_eq!(
            travel(&mut m, GuideDirection::East, Duration::from_secs(1)),
            SkyOffset::ZERO
        );
        assert_eq!(
            travel(&mut m, GuideDirection::North, Duration::from_secs(1)),
            SkyOffset::ZERO
        );
    }

    #[test]
    fn a_rate_factor_that_is_not_finite_is_refused() {
        let mut m = mount(MountConfig::default());
        assert_eq!(
            m.set_rate_factors(1.0, f64::NAN).unwrap_err().field,
            "mount.dec_rate_factor"
        );
        assert_eq!(m.config().dec_rate_factor, 1.0);
    }

    #[test]
    fn a_parked_mount_refuses_pulses_and_slews() {
        let mut m = mount(MountConfig::default());
        m.park();
        assert_eq!(
            m.pulse(GuideDirection::East, ms(100)),
            Err(MountError::Parked)
        );
        assert_eq!(
            m.slew(
                SkyOffset::new(10.0, 0.0),
                Duration::from_secs(1),
                PierSide::East
            ),
            Err(MountError::Parked)
        );
    }

    #[test]
    fn a_slewing_mount_refuses_pulses() {
        let mut m = mount(MountConfig::default());
        m.slew(
            SkyOffset::new(10.0, 0.0),
            Duration::from_secs(5),
            PierSide::East,
        )
        .unwrap();
        assert_eq!(
            m.pulse(GuideDirection::North, ms(100)),
            Err(MountError::Slewing)
        );
    }

    #[test]
    fn a_busy_axis_refuses_a_second_pulse_and_the_other_axis_runs_alongside() {
        let mut m = mount(MountConfig::default());
        m.pulse(GuideDirection::East, Duration::from_secs(1))
            .unwrap();
        assert_eq!(
            m.pulse(GuideDirection::West, ms(100)),
            Err(MountError::AxisBusy)
        );
        m.pulse(GuideDirection::North, Duration::from_secs(1))
            .unwrap();
        m.advance_to(at(2.0));
        close(m.pointing().east, GUIDE, 1e-9);
        close(m.pointing().north, GUIDE, 1e-9);
    }

    #[test]
    fn a_pulse_of_zero_duration_is_accepted_and_does_nothing() {
        let mut m = mount(MountConfig::default());
        m.pulse(GuideDirection::East, Duration::ZERO).unwrap();
        assert!(!m.is_pulse_guiding());
        m.advance_to(at(1.0));
        assert_eq!(m.pointing(), SkyOffset::ZERO);
    }

    #[test]
    fn ra_delivery_lengthens_shortens_and_steps() {
        let mut m = mount(MountConfig {
            delivery: PulseDelivery {
                east: RaDelivery {
                    lengthened_by: ms(100),
                    step_arcsec: 0.5,
                    ..RaDelivery::default()
                },
                west: RaDelivery {
                    shortened_by: ms(100),
                    shortest: ms(20),
                    step_arcsec: -0.2,
                    ..RaDelivery::default()
                },
                ..PulseDelivery::default()
            },
            ..MountConfig::default()
        });
        m.pulse(GuideDirection::East, ms(200)).unwrap();
        m.advance_to(at(0.29));
        assert!(m.is_pulse_guiding(), "the delivered length is 300 ms");
        m.advance_to(at(1.0));
        close(m.pointing().east, GUIDE.mul_add(0.3, 0.5), 1e-9);
        let west = travel(&mut m, GuideDirection::West, ms(50));
        close(west.east, (-GUIDE).mul_add(0.02, 0.2), 1e-9);
    }

    /// The RA travel the rig timed at each requested length, in ticks of
    /// 0.357″ (`docs/services/star-adventurer-gti.md`, `PulseGuide`
    /// lifecycle): East's along the pulse, West's along the pulse.
    const GTI_RA: [(u64, f64, f64); 6] = [
        (20, 1.39, 3.45),
        (50, 1.40, 3.23),
        (100, 2.37, 3.19),
        (200, 4.88, 5.93),
        (500, 11.00, 12.37),
        (1000, 20.72, 22.25),
    ];

    #[test]
    fn the_gti_preset_delivers_each_ra_length_within_0_3_arcsec_of_the_rig() {
        for (millis, east_ticks, west_ticks) in GTI_RA {
            let mut m = mount(MountConfig {
                delivery: PulseDelivery::star_adventurer_gti(),
                ..MountConfig::default()
            });
            let east = travel(&mut m, GuideDirection::East, ms(millis)).east;
            let west = travel(&mut m, GuideDirection::West, ms(millis)).east;
            close(east, east_ticks * 0.357, 0.3);
            close(west, -west_ticks * 0.357, 0.3);
        }
    }

    #[test]
    fn dec_steps_move_the_axis_in_whole_steps_and_average_to_the_request() {
        let mut m = mount(MountConfig {
            delivery: PulseDelivery {
                dec: DecDelivery {
                    step_arcsec: Some(0.5),
                    late_last_step: None,
                },
                ..PulseDelivery::default()
            },
            ..MountConfig::default()
        });
        let mut total = 0.0;
        for _ in 0..100 {
            let moved = travel(&mut m, GuideDirection::North, ms(20)).north;
            assert!(
                moved.abs() < 1e-9 || (moved - 0.5).abs() < 1e-9,
                "moved {moved}"
            );
            total += moved;
        }
        close(total, 100.0 * GUIDE * 0.02, 0.5);
        let mut back = 0.0;
        for _ in 0..100 {
            back += travel(&mut m, GuideDirection::South, ms(20)).north;
        }
        close(back, -100.0 * GUIDE * 0.02, 0.5);
    }

    #[test]
    fn dec_steps_arrive_as_the_motor_reaches_them() {
        let stepped = |sense: GuideDirection, sign: f64| {
            let mut m = mount(MountConfig {
                delivery: PulseDelivery {
                    dec: DecDelivery {
                        step_arcsec: Some(0.5),
                        late_last_step: None,
                    },
                    ..PulseDelivery::default()
                },
                ..MountConfig::default()
            });
            let phase = m.dec.motor_steps;
            m.pulse(sense, Duration::from_secs(1)).unwrap();
            for hundredths in 1..100 {
                let t = f64::from(hundredths) * 0.01;
                m.advance_to(at(t));
                let motor = phase + sign * GUIDE * t / 0.5;
                close(
                    m.pointing().north,
                    (motor.floor() - phase.floor()) * 0.5,
                    1e-9,
                );
            }
        };
        stepped(GuideDirection::North, 1.0);
        stepped(GuideDirection::South, -1.0);
    }

    #[test]
    fn the_last_step_of_a_short_dec_pulse_arrives_after_the_pulse_has_ended() {
        let mut m = mount(MountConfig {
            delivery: PulseDelivery::star_adventurer_gti(),
            ..MountConfig::default()
        });
        m.pulse(GuideDirection::North, ms(100)).unwrap();
        m.advance_to(at(0.15));
        assert!(!m.is_pulse_guiding());
        let early = m.pointing().north;
        m.advance_to(at(2.1));
        close(m.pointing().north - early, 0.446, 1e-9);
    }

    #[test]
    fn a_dec_pulse_longer_than_the_late_limit_delivers_every_step_by_its_end() {
        let mut m = mount(MountConfig {
            delivery: PulseDelivery::star_adventurer_gti(),
            ..MountConfig::default()
        });
        m.pulse(GuideDirection::South, ms(500)).unwrap();
        m.advance_to(at(0.5));
        let at_end = m.pointing().north;
        m.advance_to(at(3.0));
        assert_eq!(m.pointing().north, at_end);
        let steps = -at_end / 0.446;
        close(steps, steps.round(), 1e-9);
        close(steps, GUIDE * 0.5 / 0.446, 1.0);
    }

    #[test]
    fn backlash_takes_up_the_play_before_a_reversal_moves_the_axis() {
        let mut m = mount(MountConfig {
            dec_backlash: DecBacklash {
                play_arcsec: 2.0,
                meshed: DecSense::North,
            },
            ..MountConfig::default()
        });
        close(
            travel(&mut m, GuideDirection::North, Duration::from_secs(1)).north,
            GUIDE,
            1e-9,
        );
        close(
            travel(&mut m, GuideDirection::South, Duration::from_secs(1)).north,
            2.0 - GUIDE,
            1e-9,
        );
        close(
            travel(&mut m, GuideDirection::North, ms(200)).north,
            0.0,
            1e-12,
        );
    }

    #[test]
    fn a_gear_meshed_south_takes_up_the_play_on_a_first_north_pulse() {
        let mut m = mount(MountConfig {
            dec_backlash: DecBacklash {
                play_arcsec: 2.0,
                meshed: DecSense::South,
            },
            ..MountConfig::default()
        });
        close(
            travel(&mut m, GuideDirection::North, Duration::from_secs(1)).north,
            GUIDE - 2.0,
            1e-9,
        );
    }

    #[test]
    fn a_slew_moves_the_pointing_evenly_and_changes_the_pier_side_at_its_end() {
        let mut m = mount(MountConfig::default());
        m.slew(
            SkyOffset::new(3600.0, -1800.0),
            Duration::from_secs(10),
            PierSide::West,
        )
        .unwrap();
        m.advance_to(at(5.0));
        assert!(m.slewing());
        assert_eq!(m.side_of_pier(), PierSide::East);
        close(m.pointing().east, 1800.0, 1e-6);
        close(m.pointing().north, -900.0, 1e-6);
        m.advance_to(at(10.0));
        assert!(!m.slewing());
        assert_eq!(m.side_of_pier(), PierSide::West);
        assert!(m.flipped());
        close(m.pointing().east, 3600.0, 1e-6);
    }

    #[test]
    fn a_slew_stops_a_pulse_in_progress_where_it_stands() {
        let mut m = mount(MountConfig::default());
        m.pulse(GuideDirection::East, Duration::from_secs(2))
            .unwrap();
        m.advance_to(at(1.0));
        m.slew(SkyOffset::ZERO, Duration::ZERO, PierSide::East)
            .unwrap();
        assert!(!m.is_pulse_guiding());
        m.advance_to(at(3.0));
        close(m.pointing().east, GUIDE, 1e-9);
    }

    #[test]
    fn a_slew_drops_a_late_dec_step() {
        let mut m = mount(MountConfig {
            delivery: PulseDelivery::star_adventurer_gti(),
            ..MountConfig::default()
        });
        m.pulse(GuideDirection::North, ms(100)).unwrap();
        m.advance_to(at(0.15));
        let before = m.pointing().north;
        m.slew(SkyOffset::ZERO, Duration::ZERO, PierSide::East)
            .unwrap();
        m.advance_to(at(2.1));
        assert_eq!(m.pointing().north, before);
    }

    #[test]
    fn park_stops_tracking_and_pulses_and_unpark_leaves_tracking_off() {
        let mut m = mount(MountConfig::default());
        m.pulse(GuideDirection::North, Duration::from_secs(2))
            .unwrap();
        m.advance_to(at(1.0));
        m.park();
        assert!(m.at_park());
        assert!(!m.tracking());
        assert!(!m.is_pulse_guiding());
        m.unpark();
        assert!(!m.at_park());
        assert!(!m.tracking());
        m.advance_to(at(3.0));
        close(m.pointing().north, GUIDE, 1e-9);
    }

    #[test]
    fn changing_tracking_stops_an_ra_pulse() {
        let mut m = mount(MountConfig::default());
        m.pulse(GuideDirection::West, Duration::from_secs(2))
            .unwrap();
        m.advance_to(at(1.0));
        m.set_tracking(false).unwrap();
        assert!(!m.is_pulse_guiding());
        m.set_tracking(true).unwrap();
        m.advance_to(at(3.0));
        close(m.pointing().east, -GUIDE, 1e-9);
    }

    #[test]
    fn a_parked_mount_refuses_to_track_and_may_stop_tracking() {
        let mut m = mount(MountConfig::default());
        m.park();
        assert_eq!(m.set_tracking(true), Err(MountError::Parked));
        assert!(!m.tracking());
        m.set_tracking(false).unwrap();
    }

    #[test]
    fn time_never_runs_backward() {
        let mut m = mount(MountConfig::default());
        m.advance_to(at(5.0));
        m.advance_to(at(2.0));
        assert_eq!(m.now(), at(5.0));
    }

    #[test]
    fn construction_refuses_what_the_model_cannot_take() {
        let refused = |config: MountConfig| Mount::new(config, 1).unwrap_err().field;
        assert_eq!(
            refused(MountConfig {
                declination_deg: 91.0,
                ..MountConfig::default()
            }),
            "mount.declination_deg"
        );
        assert_eq!(
            refused(MountConfig {
                periodic_error: vec![PeriodicTerm {
                    amplitude_arcsec: 1.0,
                    period: Duration::ZERO,
                    phase_deg: 0.0,
                }],
                ..MountConfig::default()
            }),
            "mount.periodic_error.period"
        );
        let mut late = PulseDelivery::star_adventurer_gti();
        late.dec.late_last_step = Some(LateStep {
            max_pulse: ms(100),
            earliest: ms(500),
            latest: ms(100),
        });
        assert_eq!(
            refused(MountConfig {
                delivery: late,
                ..MountConfig::default()
            }),
            "mount.delivery.dec.late_last_step"
        );
        let mut no_step = PulseDelivery::star_adventurer_gti();
        no_step.dec.step_arcsec = None;
        let refused_late = Mount::new(
            MountConfig {
                delivery: no_step,
                ..MountConfig::default()
            },
            1,
        )
        .unwrap_err();
        assert_eq!(refused_late.field, "mount.delivery.dec.late_last_step");
        assert_eq!(refused_late.reason, "needs a Dec step");
        let mut stepless = PulseDelivery::default();
        stepless.dec.step_arcsec = Some(0.0);
        assert_eq!(
            refused(MountConfig {
                delivery: stepless,
                ..MountConfig::default()
            }),
            "mount.delivery.dec.step_arcsec"
        );
        assert_eq!(
            refused(MountConfig {
                guide_rate_ra: -0.5,
                ..MountConfig::default()
            }),
            "mount.guide_rate_ra"
        );
    }
}
