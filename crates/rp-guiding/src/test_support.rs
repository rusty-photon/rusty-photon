//! What the tests share: a rig, a simulated sky and mount to close the
//! loop through, and the calibration run against them.

use std::time::Duration;

use guide_simulator::{
    CameraConfig, GuideDirection, Seeing, SimConfig, Simulator, SkyConfig, SkyOffset, Star,
    StarField, SIDEREAL_ARCSEC_PER_S,
};

use crate::calibration::{
    CalibrationError, CalibrationParams, Calibrator, CameraGeometry, Direction, GuideRates,
    PierSide, Progress, Pulse, Setup,
};
use crate::frame::{Frame, SensorType};
use crate::stars::{GuideStars, Offset, StarParams};
use crate::Calibration;

/// What each step costs besides its pulse: the frame after it.
pub const EXPOSURE: Duration = Duration::from_secs(2);

/// The rate a guide rate of 0.5 × sidereal moves the stars at, at 2″ a
/// pixel, in pixels a second.
pub const HALF_SIDEREAL_PX_PER_S: f64 = 0.5 * SIDEREAL_ARCSEC_PER_S / 2.0;

pub fn close(actual: f64, expected: f64, tolerance: f64) {
    assert!(
        (actual - expected).abs() <= tolerance,
        "{actual} is not within {tolerance} of {expected}"
    );
}

/// A camera at 2″ a pixel on the East side, with guide rates of 0.5 ×
/// sidereal.
pub fn setup() -> Setup {
    let deg_per_s = 0.5 * SIDEREAL_ARCSEC_PER_S / 3_600.0;
    Setup {
        pier_side: PierSide::East,
        camera: CameraGeometry {
            pixel_size_um: 2.0,
            width: 320,
            height: 240,
            binning: 1,
        },
        focal_length_mm: 206.265,
        guide_rates: Some(GuideRates {
            ra_deg_per_s: deg_per_s,
            dec_deg_per_s: deg_per_s,
        }),
    }
}

fn star(east: f64, north: f64, e_per_s: f64) -> Star {
    Star {
        position: SkyOffset::new(east, north),
        e_per_s,
    }
}

/// A 320 × 240 frame at 2″ a pixel in 3″ seeing with a little image
/// motion, four bright stars well inside the edge margin.
pub fn simulator(adjust: impl FnOnce(&mut SimConfig)) -> Simulator {
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

/// Exposes a frame and reads it as the guider does.
pub fn measure(sim: &mut Simulator) -> Frame {
    expose(sim).0
}

/// Exposes a frame and reads it as the guider does, with its truth.
pub fn expose(sim: &mut Simulator) -> (Frame, guide_simulator::Frame) {
    let shot = sim.expose(EXPOSURE);
    let frame = Frame::new(&shot.pixels.view(), SensorType::Monochrome, Some(65_535)).unwrap();
    (frame, shot)
}

/// Sends `pulse` and waits until the mount has delivered it.
pub fn send(sim: &mut Simulator, pulse: Pulse) {
    let direction = match pulse.direction {
        Direction::North => GuideDirection::North,
        Direction::South => GuideDirection::South,
        Direction::East => GuideDirection::East,
        Direction::West => GuideDirection::West,
    };
    sim.mount_mut().pulse(direction, pulse.duration).unwrap();
    sim.advance(pulse.duration);
    while sim.mount().is_pulse_guiding() {
        sim.advance(Duration::from_millis(10));
    }
}

/// Selects guide stars on a fresh frame.
pub fn select(sim: &mut Simulator) -> (GuideStars, Option<Offset>) {
    let (stars, first) = GuideStars::select(&measure(sim), StarParams::default()).unwrap();
    (stars, first.offset)
}

/// Calibrates on the frames after one with `offset`, measured with
/// `stars`, at the declination the simulated mount points at. Also counts
/// the frames that sent no pulse.
pub fn calibrate_on(
    sim: &mut Simulator,
    stars: &mut GuideStars,
    mut offset: Option<Offset>,
    params: CalibrationParams,
) -> (Result<Calibration, CalibrationError>, usize) {
    let declination_deg = sim.mount().config().declination_deg;
    let mut calibrator = Calibrator::new(setup(), declination_deg, params, sim.now().since_start());
    let mut waits = 0;
    loop {
        match calibrator.frame(offset, sim.now().since_start()) {
            Ok(Progress::Pulse(next, pulse)) => {
                send(sim, pulse);
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

/// Selects guide stars and calibrates with them.
pub fn calibrate_in(
    sim: &mut Simulator,
    params: CalibrationParams,
) -> (Result<Calibration, CalibrationError>, usize) {
    let (mut stars, offset) = select(sim);
    calibrate_on(sim, &mut stars, offset, params)
}
