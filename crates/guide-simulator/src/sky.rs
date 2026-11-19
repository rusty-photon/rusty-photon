//! The sky: the star field, seeing and clouds. See
//! `docs/crates/guide-simulator.md` (The sky).

use std::time::Duration;

use crate::config::{self, ConfigError};
use crate::geometry::SkyOffset;
use crate::time::SimTime;

/// The stars, the seeing and the clouds.
#[derive(Debug, Clone, PartialEq)]
pub struct SkyConfig {
    /// The star field.
    pub stars: StarField,
    /// The seeing.
    pub seeing: Seeing,
    /// The clouds that pass.
    pub clouds: Vec<Cloud>,
}

impl Default for SkyConfig {
    /// Twenty random stars of 1,000 to 100,000 electrons a second, in 3″
    /// seeing with no image motion, and no clouds.
    fn default() -> Self {
        Self {
            stars: StarField::Random(RandomStars {
                count: 20,
                faintest_e_per_s: 1_000.0,
                brightest_e_per_s: 100_000.0,
            }),
            seeing: Seeing {
                fwhm_arcsec: 3.0,
                common_motion_arcsec: 0.0,
                star_motion_arcsec: 0.0,
            },
            clouds: Vec::new(),
        }
    }
}

impl SkyConfig {
    pub(crate) fn validate(&self) -> Result<(), ConfigError> {
        match &self.stars {
            StarField::Listed(stars) => {
                for star in stars {
                    config::finite("sky.stars.position.east", star.position.east)?;
                    config::finite("sky.stars.position.north", star.position.north)?;
                    config::non_negative("sky.stars.e_per_s", star.e_per_s)?;
                }
            }
            StarField::Random(random) => {
                config::positive("sky.stars.faintest_e_per_s", random.faintest_e_per_s)?;
                config::finite("sky.stars.brightest_e_per_s", random.brightest_e_per_s)?;
                config::require(
                    random.brightest_e_per_s >= random.faintest_e_per_s,
                    "sky.stars.brightest_e_per_s",
                    "must not be below the faintest",
                )?;
            }
        }
        config::positive("sky.seeing.fwhm_arcsec", self.seeing.fwhm_arcsec)?;
        config::non_negative(
            "sky.seeing.common_motion_arcsec",
            self.seeing.common_motion_arcsec,
        )?;
        config::non_negative(
            "sky.seeing.star_motion_arcsec",
            self.seeing.star_motion_arcsec,
        )?;
        for cloud in &self.clouds {
            config::within("sky.clouds.transmission", cloud.transmission, 0.0, 1.0)?;
            config::require(
                cloud.ramp.saturating_mul(2) <= cloud.duration,
                "sky.clouds.ramp",
                "must not be longer than half the cloud",
            )?;
        }
        Ok(())
    }
}

/// The star field: listed, or drawn at random over the frame.
#[derive(Debug, Clone, PartialEq)]
pub enum StarField {
    /// These stars.
    Listed(Vec<Star>),
    /// Stars drawn at random.
    Random(RandomStars),
}

/// A star.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Star {
    /// Its sky position, relative to the pointing at the start.
    pub position: SkyOffset,
    /// Its brightness at full transmission: electrons a second, summed
    /// over its image, before any Bayer response.
    pub e_per_s: f64,
}

/// Stars drawn at random: positions uniform over the frame as it lies
/// at the start, brightnesses log-uniform between the faintest and the
/// brightest.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RandomStars {
    /// How many.
    pub count: usize,
    /// The faintest brightness, in electrons a second.
    pub faintest_e_per_s: f64,
    /// The brightest brightness, in electrons a second.
    pub brightest_e_per_s: f64,
}

/// The seeing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Seeing {
    /// The full width at half maximum of the Gaussian star image, in
    /// arcseconds.
    pub fwhm_arcsec: f64,
    /// The standard deviation, on each sky axis, of the whole field's
    /// image motion in a frame.
    pub common_motion_arcsec: f64,
    /// The standard deviation, on each sky axis, of each star's own
    /// image motion in a frame, on top of the field's.
    pub star_motion_arcsec: f64,
}

/// A cloud: the stars' light falls to `transmission` over `ramp` from
/// `start`, stays there, and climbs back over `ramp` before the cloud's
/// end.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cloud {
    /// When it starts, from the start of the simulation.
    pub start: Duration,
    /// How long it lasts, ramps included.
    pub duration: Duration,
    /// How long the light takes to fall and to climb back.
    pub ramp: Duration,
    /// The fraction of the stars' light that passes it at its thickest.
    pub transmission: f64,
}

impl Cloud {
    /// The fraction of the light that passes the cloud at `t`.
    fn factor(&self, t: SimTime) -> f64 {
        let at = t.secs();
        let start = self.start.as_secs_f64();
        let end = start + self.duration.as_secs_f64();
        if at < start || at > end {
            return 1.0;
        }
        let ramp = self.ramp.as_secs_f64();
        let depth = if ramp > 0.0 {
            ((at - start).min(end - at) / ramp).min(1.0)
        } else {
            1.0
        };
        depth.mul_add(self.transmission - 1.0, 1.0)
    }
}

/// The fraction of the stars' light that passes every cloud at `t`.
pub fn transmission(clouds: &[Cloud], t: SimTime) -> f64 {
    clouds.iter().map(|cloud| cloud.factor(t)).product()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn at(secs: f64) -> SimTime {
        SimTime::from_secs(secs)
    }

    fn cloud() -> Cloud {
        Cloud {
            start: Duration::from_secs(10),
            duration: Duration::from_secs(20),
            ramp: Duration::from_secs(5),
            transmission: 0.2,
        }
    }

    #[test]
    fn a_cloud_ramps_down_holds_and_ramps_back() {
        let clouds = [cloud()];
        assert_eq!(transmission(&clouds, at(9.0)), 1.0);
        assert!((transmission(&clouds, at(12.5)) - 0.6).abs() < 1e-12);
        assert!((transmission(&clouds, at(20.0)) - 0.2).abs() < 1e-12);
        assert!((transmission(&clouds, at(27.5)) - 0.6).abs() < 1e-12);
        assert_eq!(transmission(&clouds, at(31.0)), 1.0);
    }

    #[test]
    fn a_cloud_with_no_ramp_is_a_step() {
        let clouds = [Cloud {
            ramp: Duration::ZERO,
            ..cloud()
        }];
        assert!((transmission(&clouds, at(10.0)) - 0.2).abs() < 1e-12);
    }

    #[test]
    fn clouds_that_overlap_multiply() {
        let clouds = [
            cloud(),
            Cloud {
                transmission: 0.5,
                ..cloud()
            },
        ];
        assert!((transmission(&clouds, at(20.0)) - 0.1).abs() < 1e-12);
    }

    #[test]
    fn a_ramp_longer_than_half_the_cloud_is_refused() {
        let sky = SkyConfig {
            clouds: vec![Cloud {
                ramp: Duration::from_secs(11),
                ..cloud()
            }],
            ..SkyConfig::default()
        };
        assert_eq!(sky.validate().unwrap_err().field, "sky.clouds.ramp");
    }

    #[test]
    fn a_transmission_above_one_is_refused() {
        let sky = SkyConfig {
            clouds: vec![Cloud {
                transmission: 1.5,
                ..cloud()
            }],
            ..SkyConfig::default()
        };
        assert_eq!(sky.validate().unwrap_err().field, "sky.clouds.transmission");
    }

    #[test]
    fn a_random_field_brighter_at_its_faint_end_is_refused() {
        let sky = SkyConfig {
            stars: StarField::Random(RandomStars {
                count: 3,
                faintest_e_per_s: 10.0,
                brightest_e_per_s: 5.0,
            }),
            ..SkyConfig::default()
        };
        assert_eq!(
            sky.validate().unwrap_err().field,
            "sky.stars.brightest_e_per_s"
        );
    }

    #[test]
    fn a_listed_star_must_be_finite_and_not_negative() {
        let star = |east: f64, e_per_s: f64| SkyConfig {
            stars: StarField::Listed(vec![Star {
                position: SkyOffset::new(east, 0.0),
                e_per_s,
            }]),
            ..SkyConfig::default()
        };
        assert_eq!(
            star(f64::NAN, 1.0).validate().unwrap_err().field,
            "sky.stars.position.east"
        );
        assert_eq!(
            star(0.0, -1.0).validate().unwrap_err().field,
            "sky.stars.e_per_s"
        );
    }

    #[test]
    fn seeing_needs_a_positive_fwhm() {
        let mut sky = SkyConfig::default();
        sky.seeing.fwhm_arcsec = 0.0;
        assert_eq!(sky.validate().unwrap_err().field, "sky.seeing.fwhm_arcsec");
    }
}
