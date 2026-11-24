//! The Alpaca Telescope: the simulated mount. See
//! `docs/crates/guide-simulator-alpaca.md` (The mount).

use std::ops::RangeInclusive;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use ascom_alpaca::api::telescope::{
    AlignmentMode, DriveRate, EquatorialCoordinateType, GuideDirection, PierSide, TelescopeAxis,
};
use ascom_alpaca::api::{Device, Telescope};
use ascom_alpaca::{ASCOMError, ASCOMErrorCode, ASCOMResult};
use guide_simulator::{
    GuideDirection as SimDirection, Mount, MountError, PierSide as SimSide, SIDEREAL_ARCSEC_PER_S,
};
use tracing::debug;

use crate::config::PulseRecord;
use crate::rig::{Rig, State, StuckPulses};

/// Sidereal seconds in a second of time.
const SIDEREAL_PER_SOLAR: f64 = 1.002_737_909_35;

/// The mount in front of the simulated mount.
#[derive(Debug, Clone)]
pub struct SimTelescope {
    rig: Arc<Rig>,
}

impl SimTelescope {
    pub const fn new(rig: Arc<Rig>) -> Self {
        Self { rig }
    }

    /// Runs `f` on the state, refused when the mount is not connected.
    fn session<T>(&self, f: impl FnOnce(&mut State) -> ASCOMResult<T>) -> ASCOMResult<T> {
        self.rig.with(|state| {
            if state.mount.connected {
                f(state)
            } else {
                Err(ASCOMError::NOT_CONNECTED)
            }
        })
    }

    /// `value`, refused when the mount is not connected.
    fn when_connected<T>(&self, value: T) -> ASCOMResult<T> {
        self.session(|_| Ok(value))
    }

    /// Runs `f` on the simulated mount, refused when the mount is not
    /// connected.
    fn mount<T>(&self, f: impl FnOnce(&mut Mount) -> T) -> ASCOMResult<T> {
        self.session(|state| Ok(f(state.sim.mount_mut())))
    }
}

/// An hour angle or right ascension brought into 0 to 24 hours, 24
/// excluded: a value a hair below 0 wraps to 24.0 in floating point, and
/// that is 0.
fn hours(value: f64) -> f64 {
    let wrapped = value.rem_euclid(24.0);
    if wrapped < 24.0 {
        wrapped
    } else {
        0.0
    }
}

/// The simulated mount's refusal as an Alpaca error.
fn refusal(error: MountError) -> ASCOMError {
    match error {
        MountError::Parked => ASCOMError::new(ASCOMErrorCode::INVALID_WHILE_PARKED, error),
        MountError::Slewing | MountError::AxisBusy => ASCOMError::invalid_operation(error),
    }
}

/// A guide rate as a fraction of sidereal, in degrees a second.
fn degrees_per_second(fraction: f64) -> f64 {
    fraction * SIDEREAL_ARCSEC_PER_S / 3_600.0
}

#[async_trait::async_trait]
impl Device for SimTelescope {
    fn static_name(&self) -> &'static str {
        "guide-simulator mount"
    }

    fn unique_id(&self) -> &str {
        &self.rig.mount.unique_id
    }

    async fn connected(&self) -> ASCOMResult<bool> {
        Ok(self.rig.with(|state| state.mount.connected))
    }

    async fn set_connected(&self, connected: bool) -> ASCOMResult<()> {
        self.rig.with(|state| state.mount.connected = connected);
        debug!(connected, "simulated mount Connected written");
        Ok(())
    }

    async fn description(&self) -> ASCOMResult<String> {
        Ok("guide-simulator's mount behind an Alpaca Telescope".to_owned())
    }

    async fn driver_info(&self) -> ASCOMResult<String> {
        Ok("guide-simulator-alpaca".to_owned())
    }

    async fn driver_version(&self) -> ASCOMResult<String> {
        Ok(env!("CARGO_PKG_VERSION").to_owned())
    }
}

#[async_trait::async_trait]
impl Telescope for SimTelescope {
    async fn alignment_mode(&self) -> ASCOMResult<AlignmentMode> {
        self.when_connected(AlignmentMode::GermanPolar)
    }

    async fn equatorial_system(&self) -> ASCOMResult<EquatorialCoordinateType> {
        self.when_connected(EquatorialCoordinateType::Topocentric)
    }

    async fn at_home(&self) -> ASCOMResult<bool> {
        self.when_connected(false)
    }

    async fn at_park(&self) -> ASCOMResult<bool> {
        self.mount(|mount| mount.at_park())
    }

    async fn can_park(&self) -> ASCOMResult<bool> {
        self.when_connected(true)
    }

    async fn can_unpark(&self) -> ASCOMResult<bool> {
        self.when_connected(true)
    }

    async fn can_set_tracking(&self) -> ASCOMResult<bool> {
        self.when_connected(true)
    }

    async fn can_pulse_guide(&self) -> ASCOMResult<bool> {
        self.session(|state| Ok(state.mount.can_pulse_guide))
    }

    async fn park(&self) -> ASCOMResult<()> {
        self.mount(Mount::park)?;
        debug!("simulated mount parked");
        Ok(())
    }

    async fn unpark(&self) -> ASCOMResult<()> {
        self.mount(Mount::unpark)?;
        debug!("simulated mount unparked");
        Ok(())
    }

    async fn tracking(&self) -> ASCOMResult<bool> {
        self.mount(|mount| mount.tracking())
    }

    async fn set_tracking(&self, tracking: bool) -> ASCOMResult<()> {
        self.mount(|mount| mount.set_tracking(tracking))?
            .map_err(refusal)
    }

    async fn tracking_rate(&self) -> ASCOMResult<DriveRate> {
        self.when_connected(DriveRate::Sidereal)
    }

    async fn set_tracking_rate(&self, tracking_rate: DriveRate) -> ASCOMResult<()> {
        self.session(|_| {
            if tracking_rate == DriveRate::Sidereal {
                Ok(())
            } else {
                Err(ASCOMError::invalid_value(
                    "the mount tracks at the sidereal rate only",
                ))
            }
        })
    }

    async fn tracking_rates(&self) -> ASCOMResult<Vec<DriveRate>> {
        self.when_connected(vec![DriveRate::Sidereal])
    }

    async fn slewing(&self) -> ASCOMResult<bool> {
        self.mount(|mount| mount.slewing())
    }

    async fn side_of_pier(&self) -> ASCOMResult<PierSide> {
        let reports = self.rig.mount.reports_pier_side;
        self.mount(|mount| match (reports, mount.side_of_pier()) {
            (false, _) => PierSide::Unknown,
            (true, SimSide::East) => PierSide::East,
            (true, SimSide::West) => PierSide::West,
        })
    }

    async fn declination(&self) -> ASCOMResult<f64> {
        self.mount(|mount| mount.config().declination_deg + mount.pointing().north / 3_600.0)
    }

    async fn right_ascension(&self) -> ASCOMResult<f64> {
        let start = self.rig.mount.right_ascension_hours;
        self.mount(|mount| {
            let cos_dec = mount.config().declination_deg.to_radians().cos();
            hours(start + mount.pointing().east / cos_dec / (15.0 * 3_600.0))
        })
    }

    async fn declination_rate(&self) -> ASCOMResult<f64> {
        self.when_connected(0.0)
    }

    async fn right_ascension_rate(&self) -> ASCOMResult<f64> {
        self.when_connected(0.0)
    }

    async fn altitude(&self) -> ASCOMResult<f64> {
        self.when_connected(self.rig.mount.altitude_deg)
    }

    async fn azimuth(&self) -> ASCOMResult<f64> {
        self.when_connected(self.rig.mount.azimuth_deg)
    }

    async fn sidereal_time(&self) -> ASCOMResult<f64> {
        let start = self.rig.mount.right_ascension_hours;
        self.mount(|mount| {
            let elapsed = mount.now().since_start().as_secs_f64();
            hours(start + elapsed * SIDEREAL_PER_SOLAR / 3_600.0)
        })
    }

    async fn guide_rate_right_ascension(&self) -> ASCOMResult<f64> {
        if !self.rig.mount.reports_guide_rates {
            return self.session(|_| Err(ASCOMError::NOT_IMPLEMENTED));
        }
        self.mount(|mount| degrees_per_second(mount.config().guide_rate_ra))
    }

    async fn guide_rate_declination(&self) -> ASCOMResult<f64> {
        if !self.rig.mount.reports_guide_rates {
            return self.session(|_| Err(ASCOMError::NOT_IMPLEMENTED));
        }
        self.mount(|mount| degrees_per_second(mount.config().guide_rate_dec))
    }

    async fn pulse_guide(&self, direction: GuideDirection, duration: Duration) -> ASCOMResult<()> {
        let direction = match direction {
            GuideDirection::North => SimDirection::North,
            GuideDirection::South => SimDirection::South,
            GuideDirection::East => SimDirection::East,
            GuideDirection::West => SimDirection::West,
        };
        self.session(|state| {
            if !state.mount.can_pulse_guide {
                return Err(ASCOMError::NOT_IMPLEMENTED);
            }
            let at = state.sim.now();
            state
                .sim
                .mount_mut()
                .pulse(direction, duration)
                .map_err(refusal)?;
            if state.mount.stuck_pulses == StuckPulses::Armed {
                state.mount.stuck_pulses = StuckPulses::Stuck;
            }
            state.pulses.push(PulseRecord {
                direction,
                duration,
                at,
            });
            Ok(())
        })?;
        debug!(?direction, ?duration, "simulated mount pulse started");
        Ok(())
    }

    async fn is_pulse_guiding(&self) -> ASCOMResult<bool> {
        self.session(|state| {
            Ok(state.sim.mount().is_pulse_guiding()
                || state.mount.stuck_pulses == StuckPulses::Stuck)
        })
    }

    async fn utc_date(&self) -> ASCOMResult<SystemTime> {
        self.when_connected(SystemTime::now())
    }

    /// No axis moves at a rate a client sets: the mount slews only when a
    /// scenario slews it.
    async fn axis_rates(&self, _axis: TelescopeAxis) -> ASCOMResult<Vec<RangeInclusive<f64>>> {
        self.when_connected(Vec::new())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn hours_wrap_into_a_day() {
        assert_eq!(hours(25.5), 1.5);
        assert_eq!(hours(-1.0), 23.0);
        assert_eq!(hours(6.0), 6.0);
        assert_eq!(hours(-1e-18), 0.0);
    }

    #[test]
    fn half_sidereal_is_about_two_millidegrees_a_second() {
        assert!((degrees_per_second(0.5) - 0.002_089).abs() < 1e-6);
    }
}
