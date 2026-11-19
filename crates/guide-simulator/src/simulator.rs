//! The simulator: the mount, the sky and the camera on one clock.

use std::time::Duration;

use crate::camera::{Camera, CameraConfig, Frame, Sample};
use crate::config::ConfigError;
use num_traits::ToPrimitive;

use crate::geometry::{coordinate, SensorPoint};
use crate::mount::{Mount, MountConfig};
use crate::random::Stream;
use crate::sky::{self, Cloud, RandomStars, Seeing, SkyConfig, Star, StarField};
use crate::time::SimTime;

/// The star placement's stream within a run's seed.
const STARS_STREAM: u64 = 2;

/// The longest slice of an exposure that is drawn from one sample of the
/// pointing.
const SLICE: Duration = Duration::from_millis(50);

/// Everything a simulation is made of.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SimConfig {
    /// The seed every random draw comes from.
    pub seed: u64,
    /// The mount.
    pub mount: MountConfig,
    /// The camera.
    pub camera: CameraConfig,
    /// The stars, the seeing and the clouds.
    pub sky: SkyConfig,
}

/// A guide camera's view through a mount, in simulated time.
#[derive(Debug, Clone)]
pub struct Simulator {
    mount: Mount,
    camera: Camera,
    stars: Vec<Star>,
    seeing: Seeing,
    clouds: Vec<Cloud>,
}

impl Simulator {
    /// A simulation at its start.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] names the first field the simulator cannot model.
    pub fn new(config: SimConfig) -> Result<Self, ConfigError> {
        config.sky.validate()?;
        let mount = Mount::new(config.mount, config.seed)?;
        let camera = Camera::new(config.camera, config.seed)?;
        let stars = match config.sky.stars {
            StarField::Listed(stars) => stars,
            StarField::Random(random) => random_stars(&camera, random, config.seed),
        };
        Ok(Self {
            mount,
            camera,
            stars,
            seeing: config.sky.seeing,
            clouds: config.sky.clouds,
        })
    }

    /// The simulation's current instant.
    #[must_use]
    pub const fn now(&self) -> SimTime {
        self.mount.now()
    }

    /// Moves the clock on by `by`.
    pub fn advance(&mut self, by: Duration) {
        let to = self.now().after(by);
        self.mount.advance_to(to);
    }

    /// Exposes from now for `duration`, and moves the clock to the
    /// exposure's end.
    pub fn expose(&mut self, duration: Duration) -> Frame {
        let start = self.now();
        let span = duration.as_secs_f64();
        let slices = (span / SLICE.as_secs_f64())
            .ceil()
            .to_usize()
            .unwrap_or(1)
            .max(1);
        let secs = span / coordinate(slices);
        let mut samples = Vec::with_capacity(slices);
        for slice in 0..slices {
            let at = SimTime::from_secs((coordinate(slice) + 0.5).mul_add(secs, start.secs()));
            self.mount.advance_to(at);
            samples.push(Sample {
                pointing: self.mount.pointing(),
                flipped: self.mount.flipped(),
                transmission: sky::transmission(&self.clouds, at),
                secs,
            });
        }
        self.mount.advance_to(start.after(duration));
        self.camera
            .render(&samples, &self.stars, &self.seeing, start, duration)
    }

    /// The mount.
    #[must_use]
    pub const fn mount(&self) -> &Mount {
        &self.mount
    }

    /// The mount, to pulse, slew, park or change its tracking.
    pub const fn mount_mut(&mut self) -> &mut Mount {
        &mut self.mount
    }

    /// The camera's configuration.
    #[must_use]
    pub const fn camera(&self) -> &CameraConfig {
        self.camera.config()
    }

    /// The star field, random stars placed.
    #[must_use]
    pub fn stars(&self) -> &[Star] {
        &self.stars
    }
}

/// Stars placed uniform over the frame as it lies at the start, with
/// brightnesses log-uniform between the faintest and the brightest.
fn random_stars(camera: &Camera, random: RandomStars, seed: u64) -> Vec<Star> {
    let mut stream = Stream::new(seed, STARS_STREAM);
    let (width, height) = (camera.config().width, camera.config().height);
    let ratio = random.brightest_e_per_s / random.faintest_e_per_s;
    (0..random.count)
        .map(|_| {
            let x = stream.uniform().mul_add(coordinate(width), -0.5);
            let y = stream.uniform().mul_add(coordinate(height), -0.5);
            Star {
                position: camera.to_sky(SensorPoint::new(x, y)),
                e_per_s: random.faintest_e_per_s * ratio.powf(stream.uniform()),
            }
        })
        .collect()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::geometry::SkyOffset;
    use crate::mount::{GuideDirection, PierSide, PulseDelivery, SIDEREAL_ARCSEC_PER_S};
    use crate::sky::RandomStars;

    /// The default guide rate, 0.5 × sidereal, in arcseconds a second.
    const GUIDE: f64 = 0.5 * SIDEREAL_ARCSEC_PER_S;

    /// A small camera, so that frames render quickly.
    fn small() -> CameraConfig {
        CameraConfig {
            width: 160,
            height: 120,
            ..CameraConfig::default()
        }
    }

    fn simulator(config: SimConfig) -> Simulator {
        Simulator::new(config).unwrap()
    }

    fn base() -> SimConfig {
        SimConfig {
            seed: 11,
            camera: small(),
            ..SimConfig::default()
        }
    }

    fn close(actual: f64, expected: f64, tolerance: f64) {
        assert!(
            (actual - expected).abs() <= tolerance,
            "{actual} is not within {tolerance} of {expected}"
        );
    }

    #[test]
    fn the_same_seed_gives_the_same_frames() {
        let mut a = simulator(base());
        let mut b = simulator(base());
        assert_eq!(
            a.expose(Duration::from_secs(2)),
            b.expose(Duration::from_secs(2))
        );
    }

    #[test]
    fn another_seed_gives_other_frames() {
        let mut a = simulator(base());
        let mut b = simulator(SimConfig { seed: 12, ..base() });
        assert_ne!(
            a.expose(Duration::from_secs(2)).pixels,
            b.expose(Duration::from_secs(2)).pixels
        );
    }

    #[test]
    fn a_bigger_frame_leaves_the_mount_s_draws_as_they_were() {
        let gti = MountConfig {
            delivery: PulseDelivery::star_adventurer_gti(),
            ..MountConfig::default()
        };
        // When the late last step of a pulse rendered after a frame lands:
        // the mount draws its delay after the camera has drawn the frame's
        // noise, so a stream the two shared would move it.
        let late_step = |width: usize| {
            let mut sim = simulator(SimConfig {
                mount: gti.clone(),
                camera: CameraConfig { width, ..small() },
                ..base()
            });
            sim.expose(Duration::from_secs(1));
            let ended = sim.now().after(Duration::from_millis(100));
            sim.mount_mut()
                .pulse(GuideDirection::North, Duration::from_millis(100))
                .unwrap();
            let mut landed = sim.now();
            let mut last = sim.mount().pointing();
            for _ in 0..250 {
                sim.advance(Duration::from_millis(10));
                if sim.mount().pointing() != last {
                    last = sim.mount().pointing();
                    landed = sim.now();
                }
            }
            assert!(
                landed > ended.after(Duration::from_millis(90)),
                "{landed:?}"
            );
            landed
        };
        assert_eq!(late_step(160), late_step(320));
    }

    #[test]
    fn an_exposure_moves_the_clock_by_its_length() {
        let mut sim = simulator(base());
        sim.advance(Duration::from_millis(300));
        let frame = sim.expose(Duration::from_secs(2));
        assert_eq!(
            frame.truth.start,
            SimTime::from_start(Duration::from_millis(300))
        );
        assert_eq!(sim.now(), SimTime::from_start(Duration::from_millis(2300)));
    }

    #[test]
    fn random_stars_lie_on_the_frame_within_their_brightnesses() {
        let mut sim = simulator(SimConfig {
            sky: SkyConfig {
                stars: StarField::Random(RandomStars {
                    count: 50,
                    faintest_e_per_s: 500.0,
                    brightest_e_per_s: 50_000.0,
                }),
                ..SkyConfig::default()
            },
            ..base()
        });
        assert_eq!(sim.stars().len(), 50);
        assert!(sim
            .stars()
            .iter()
            .all(|star| (500.0..=50_000.0).contains(&star.e_per_s)));
        assert_eq!(sim.expose(Duration::from_secs(1)).truth.stars.len(), 50);
    }

    #[test]
    fn a_west_pulse_moves_the_stars_toward_plus_x() {
        let mut sim = simulator(base());
        sim.mount_mut()
            .pulse(GuideDirection::West, Duration::from_secs(1))
            .unwrap();
        sim.advance(Duration::from_secs(1));
        let frame = sim.expose(Duration::from_secs(1));
        let scale = sim.camera().pixel_scale_arcsec;
        close(frame.truth.field_shift.x, GUIDE / scale, 1e-6);
        close(frame.truth.field_shift.y, 0.0, 1e-9);
    }

    #[test]
    fn a_pulse_during_an_exposure_shifts_the_field_by_its_average() {
        let mut sim = simulator(base());
        sim.mount_mut()
            .pulse(GuideDirection::East, Duration::from_secs(1))
            .unwrap();
        let frame = sim.expose(Duration::from_secs(2));
        // East for the first second, then still: an average of three
        // quarters of the pulse's travel over the two seconds.
        close(frame.truth.pointing.east, 0.75 * GUIDE, 1e-6);
        close(frame.truth.field_shift.x, -0.75 * GUIDE / 2.0, 1e-6);
    }

    #[test]
    fn a_flip_turns_the_way_a_pulse_moves_the_stars() {
        let mut sim = simulator(base());
        sim.mount_mut()
            .slew(SkyOffset::ZERO, Duration::ZERO, PierSide::West)
            .unwrap();
        sim.mount_mut()
            .pulse(GuideDirection::West, Duration::from_secs(1))
            .unwrap();
        sim.advance(Duration::from_secs(1));
        let frame = sim.expose(Duration::from_secs(1));
        close(frame.truth.field_shift.x, -GUIDE / 2.0, 1e-6);
    }

    #[test]
    fn a_thick_cloud_hides_the_stars() {
        let mut sim = simulator(SimConfig {
            sky: SkyConfig {
                clouds: vec![Cloud {
                    start: Duration::ZERO,
                    duration: Duration::from_mins(1),
                    ramp: Duration::ZERO,
                    transmission: 0.0,
                }],
                ..SkyConfig::default()
            },
            ..base()
        });
        let frame = sim.expose(Duration::from_secs(2));
        assert_eq!(frame.truth.transmission, 0.0);
        assert!(frame.truth.stars.iter().all(|star| star.electrons == 0.0));
    }

    #[test]
    fn seeing_moves_each_star_by_an_offset_of_its_own() {
        let mut config = base();
        config.sky.seeing.star_motion_arcsec = 1.0;
        let mut sim = simulator(config);
        let still = simulator(base()).expose(Duration::from_secs(1));
        let seen = sim.expose(Duration::from_secs(1));
        let moves: Vec<f64> = seen
            .truth
            .stars
            .iter()
            .filter_map(|b| {
                let a = still.truth.stars.iter().find(|a| a.index == b.index)?;
                Some(b.position.x - a.position.x)
            })
            .collect();
        let spread = moves.iter().map(|m| m * m).sum::<f64>() / moves.len() as f64;
        assert!(moves.windows(2).any(|pair| pair[0] != pair[1]));
        assert!(
            spread.sqrt() > 0.2 && spread.sqrt() < 1.0,
            "rms {}",
            spread.sqrt()
        );
    }

    #[test]
    fn construction_names_the_field_it_refuses() {
        let refused = |config: SimConfig| Simulator::new(config).unwrap_err().field;
        let mut sky = base();
        sky.sky.seeing.fwhm_arcsec = f64::NAN;
        assert_eq!(refused(sky), "sky.seeing.fwhm_arcsec");
        assert_eq!(
            refused(SimConfig {
                camera: CameraConfig {
                    height: 0,
                    ..small()
                },
                ..base()
            }),
            "camera.height"
        );
        assert_eq!(
            refused(SimConfig {
                mount: MountConfig {
                    ra_rate_factor: f64::INFINITY,
                    ..MountConfig::default()
                },
                ..base()
            }),
            "mount.ra_rate_factor"
        );
    }
}
