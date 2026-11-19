//! Guide stars: their selection in one frame, and their measurement in
//! each frame after it. See `docs/services/guider.md` (Star selection and
//! measurement).

use std::collections::VecDeque;

use num_traits::ToPrimitive;
use rp_image_analysis::aperture::{
    aperture_snr, measure_in_apertures, measure_listed, Listed, MeasuredStar,
};
use rp_image_analysis::stars::{detect, DetectionParams, MIN_STAR_HFR};
use tracing::debug;

use crate::frame::Frame;

/// Detection's threshold above the background, in standard deviations,
/// its smoothing and its smallest component: the defaults of `rp`'s
/// measuring tools.
const THRESHOLD_SIGMA: f64 = 5.0;
const SMOOTHING_SIGMA: f64 = 1.0;
const MIN_AREA: usize = 5;

/// The largest half-flux disc of a star, in pixels².
const MAX_HALF_FLUX_AREA: usize = 1_000;

/// The usable frames a star's flux reference is the median over, and the
/// frames in a row that fail only the flux test before the reference
/// starts again from them.
const REFERENCE_FRAMES: usize = 10;

/// The star settings of the guider's configuration (`stars.*`) and the
/// calibration distance the edge margin adds. The service validates
/// them when it loads its configuration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StarParams {
    /// The most guide stars kept (`stars.max`).
    pub max: usize,
    /// The fewest usable stars a frame needs (`stars.min`).
    pub min: usize,
    /// How far from where it was last usable a star is looked for
    /// (`stars.search_region_px`).
    pub search_region_px: f64,
    /// The lowest signal-to-noise ratio of a usable star
    /// (`stars.min_snr`).
    pub min_snr: f64,
    /// The largest change of a star's flux from its reference, as a
    /// fraction (`stars.mass_change`).
    pub mass_change: f64,
    /// How far a calibration leg moves the stars
    /// (`calibration.distance_px`).
    pub calibration_distance_px: f64,
}

impl Default for StarParams {
    /// The guider configuration's defaults.
    fn default() -> Self {
        Self {
            max: 8,
            min: 1,
            search_region_px: 15.0,
            min_snr: 3.0,
            mass_change: 0.5,
            calibration_distance_px: 25.0,
        }
    }
}

/// Why no guide star was selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SelectError {
    /// No detection in the frame passed the tests.
    #[error("the frame has no star to guide on")]
    NoStars,
}

/// An offset on the sensor, in pixels of the measured frame.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Offset {
    /// Pixels along a row.
    pub x: f64,
    /// Pixels along a column.
    pub y: f64,
}

/// A usable star in a frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Usable {
    /// Its position along a row.
    pub x: f64,
    /// Its position along a column.
    pub y: f64,
    /// Its position less its position in the frame it was selected in.
    pub offset: Offset,
    /// Its half-flux diameter, in pixels.
    pub hfd: f64,
    /// Its signal-to-noise ratio.
    pub snr: f64,
    /// Its aperture's flux, in the measured frame's units.
    pub flux: f64,
}

/// What became of one guide star in a frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StarReading {
    /// The star is usable.
    Usable(Usable),
    /// No star settled within the search region.
    NotFound,
    /// The star was found below the lowest signal-to-noise ratio.
    Faint {
        /// Its signal-to-noise ratio.
        snr: f64,
    },
    /// The star was found with its flux too far from its reference.
    FluxChanged {
        /// Its flux.
        flux: f64,
        /// Its reference.
        reference: f64,
    },
}

/// What one frame's measurement found.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameMeasurement {
    /// The median of the usable stars' offsets on each axis; `None` for a
    /// star-lost frame.
    pub offset: Option<Offset>,
    /// Each guide star's reading, in the order they were selected in,
    /// highest signal-to-noise ratio first.
    pub stars: Vec<StarReading>,
    /// How many stars are usable.
    pub usable: usize,
    /// Whether the frame has fewer usable stars than the fewest it needs.
    pub star_lost: bool,
    /// The median half-flux diameter of the usable stars.
    pub median_hfd: Option<f64>,
    /// The median signal-to-noise ratio of the usable stars.
    pub median_snr: Option<f64>,
    /// The median flux of the usable stars.
    pub median_flux: Option<f64>,
}

/// The guide stars of a run, and what each frame has told about them.
#[derive(Debug, Clone)]
pub struct GuideStars {
    params: StarParams,
    stars: Vec<GuideStar>,
    /// The hot pixels selection found, left out of every measurement.
    hot: Vec<(usize, usize)>,
}

impl GuideStars {
    /// Selects up to `params.max` guide stars from `frame`, highest
    /// signal-to-noise ratio first, and measures the frame with them: its
    /// first usable frame.
    ///
    /// # Errors
    ///
    /// [`SelectError::NoStars`] when no detection passes the tests.
    pub fn select(
        frame: &Frame,
        params: StarParams,
    ) -> Result<(Self, FrameMeasurement), SelectError> {
        let view = frame.pixels();
        let background = frame.background();
        let detection = detect(
            &view,
            background,
            &DetectionParams {
                threshold_sigma: THRESHOLD_SIGMA,
                smoothing_sigma: SMOOTHING_SIGMA,
                min_area: MIN_AREA,
                max_area: usize::MAX,
                saturation_level: None,
            },
        );
        let measured =
            measure_in_apertures(&view, &detection, background.mean, MAX_HALF_FLUX_AREA, None);
        let (rows, cols) = view.dim();
        let margin = params.calibration_distance_px + params.search_region_px;
        let mut passed_over = PassedOver::default();
        let mut candidates: Vec<(MeasuredStar, f64)> = measured
            .iter()
            .filter(|star| {
                let verdict = if holds_full_scale(frame, star) {
                    Some(&mut passed_over.saturated)
                } else if near_edge(star, margin, rows, cols) {
                    Some(&mut passed_over.edge)
                } else if star.hfr < MIN_STAR_HFR {
                    Some(&mut passed_over.sharp)
                } else if measured.iter().any(|other| {
                    other.detection != star.detection
                        && distance(other.x, other.y, star.x, star.y) <= params.search_region_px
                }) {
                    Some(&mut passed_over.confused)
                } else {
                    None
                };
                verdict.is_none_or(|count| {
                    *count = count.saturating_add(1);
                    false
                })
            })
            .map(|star| {
                (
                    *star,
                    aperture_snr(star.flux, star.radius, background.stddev),
                )
            })
            .collect();
        candidates.sort_by(|a, b| b.1.total_cmp(&a.1));
        candidates.truncate(params.max);
        debug!(
            detections = detection.stars.len(),
            measured = measured.len(),
            selected = candidates.len(),
            ?passed_over,
            "guide stars selected"
        );
        if candidates.is_empty() {
            return Err(SelectError::NoStars);
        }
        let stars: Vec<GuideStar> = candidates
            .iter()
            .map(|(star, _)| GuideStar::new(star))
            .collect();
        let readings = candidates
            .iter()
            .map(|(star, snr)| {
                StarReading::Usable(Usable {
                    x: star.x,
                    y: star.y,
                    offset: Offset::default(),
                    hfd: 2.0 * star.hfr,
                    snr: *snr,
                    flux: star.flux,
                })
            })
            .collect();
        let guide_stars = Self {
            params,
            stars,
            hot: detection.hot,
        };
        let measurement = summarize(readings, params.min);
        Ok((guide_stars, measurement))
    }

    /// Measures the guide stars in `frame`, each where it was last
    /// usable.
    pub fn measure(&mut self, frame: &Frame) -> FrameMeasurement {
        let params = self.params;
        let sigma = frame.background().stddev;
        let listed: Vec<Listed> = self.stars.iter().map(|star| star.last).collect();
        // The kernel's own threshold is left at zero: the guider judges a
        // star's signal-to-noise ratio itself.
        let found = measure_listed(
            &frame.pixels(),
            &listed,
            &self.hot,
            sigma,
            MAX_HALF_FLUX_AREA,
            0.0,
        );
        let readings: Vec<StarReading> = self
            .stars
            .iter_mut()
            .zip(found)
            .map(|(star, found)| star.read(found, sigma, &params))
            .collect();
        let measurement = summarize(readings, params.min);
        debug!(
            usable = measurement.usable,
            star_lost = measurement.star_lost,
            "guide stars measured"
        );
        measurement
    }

    /// Where each guide star was selected, highest signal-to-noise ratio
    /// first.
    #[must_use]
    pub fn references(&self) -> Vec<Offset> {
        self.stars.iter().map(|star| star.reference).collect()
    }
}

/// How many detections selection passed over, by reason.
#[derive(Debug, Default)]
struct PassedOver {
    saturated: usize,
    edge: usize,
    sharp: usize,
    confused: usize,
}

/// One guide star and its history.
#[derive(Debug, Clone)]
struct GuideStar {
    /// Where it was selected.
    reference: Offset,
    /// Where it was last usable, and the aperture it was measured in.
    last: Listed,
    /// Its flux in its last usable frames, up to [`REFERENCE_FRAMES`].
    fluxes: VecDeque<f64>,
    /// Its flux in the frames in a row that failed only the flux test.
    flux_failures: Vec<f64>,
}

impl GuideStar {
    fn new(star: &MeasuredStar) -> Self {
        Self {
            reference: Offset {
                x: star.x,
                y: star.y,
            },
            last: Listed {
                x: star.x,
                y: star.y,
                radius: star.radius,
            },
            fluxes: VecDeque::from([star.flux]),
            flux_failures: Vec::new(),
        }
    }

    /// Judges the star as `found` in a frame whose background's standard
    /// deviation is `sigma`.
    fn read(
        &mut self,
        found: Option<MeasuredStar>,
        sigma: f64,
        params: &StarParams,
    ) -> StarReading {
        let found = found.filter(|star| {
            distance(star.x, star.y, self.last.x, self.last.y) <= params.search_region_px
        });
        let Some(star) = found else {
            self.flux_failures.clear();
            return StarReading::NotFound;
        };
        let snr = aperture_snr(star.flux, star.radius, sigma);
        if snr < params.min_snr {
            self.flux_failures.clear();
            return StarReading::Faint { snr };
        }
        let reference = self.reference_flux();
        if within(star.flux, reference, params.mass_change) {
            self.flux_failures.clear();
            self.fluxes.push_back(star.flux);
            if self.fluxes.len() > REFERENCE_FRAMES {
                self.fluxes.pop_front();
            }
            return self.usable(&star, snr);
        }
        self.flux_failures.push(star.flux);
        if self.flux_failures.len() < REFERENCE_FRAMES {
            return StarReading::FluxChanged {
                flux: star.flux,
                reference,
            };
        }
        // A change that lasted: the reference starts again from the
        // frames that showed it, this one among them.
        self.fluxes = self.flux_failures.drain(..).collect();
        let restarted = self.reference_flux();
        if within(star.flux, restarted, params.mass_change) {
            self.usable(&star, snr)
        } else {
            StarReading::FluxChanged {
                flux: star.flux,
                reference: restarted,
            }
        }
    }

    /// Records `star` as where the star was last usable, and reads it.
    fn usable(&mut self, star: &MeasuredStar, snr: f64) -> StarReading {
        self.last = Listed {
            x: star.x,
            y: star.y,
            radius: star.radius,
        };
        StarReading::Usable(Usable {
            x: star.x,
            y: star.y,
            offset: Offset {
                x: star.x - self.reference.x,
                y: star.y - self.reference.y,
            },
            hfd: 2.0 * star.hfr,
            snr,
            flux: star.flux,
        })
    }

    /// The median of the flux of the star's last usable frames.
    fn reference_flux(&self) -> f64 {
        median(self.fluxes.iter().copied().collect()).unwrap_or(0.0)
    }
}

/// A frame's measurement from its stars' readings.
fn summarize(stars: Vec<StarReading>, min: usize) -> FrameMeasurement {
    let usable: Vec<Usable> = stars
        .iter()
        .filter_map(|reading| match reading {
            StarReading::Usable(usable) => Some(*usable),
            _ => None,
        })
        .collect();
    let star_lost = usable.len() < min;
    let of = |value: fn(&Usable) -> f64| median(usable.iter().map(value).collect());
    let offset = if star_lost {
        None
    } else {
        of(|star| star.offset.x)
            .zip(of(|star| star.offset.y))
            .map(|(x, y)| Offset { x, y })
    };
    FrameMeasurement {
        offset,
        usable: usable.len(),
        star_lost,
        median_hfd: of(|star| star.hfd),
        median_snr: of(|star| star.snr),
        median_flux: of(|star| star.flux),
        stars,
    }
}

/// The median of `values`, the mean of the middle two for an even count;
/// `None` for none.
fn median(mut values: Vec<f64>) -> Option<f64> {
    values.sort_by(f64::total_cmp);
    let half = values.len().div_euclid(2);
    let upper = *values.get(half)?;
    if values.len().is_multiple_of(2) {
        let lower = *values.get(half.checked_sub(1)?)?;
        Some(f64::midpoint(lower, upper))
    } else {
        Some(upper)
    }
}

/// Whether `flux` is within `fraction` of `reference`.
fn within(flux: f64, reference: f64, fraction: f64) -> bool {
    (flux - reference).abs() <= fraction * reference
}

/// The distance between two points, in pixels.
fn distance(x1: f64, y1: f64, x2: f64, y2: f64) -> f64 {
    (x1 - x2).hypot(y1 - y2)
}

/// Whether `star`'s aperture centre is nearer an edge of a frame of
/// `rows` × `cols` than `margin`.
fn near_edge(star: &MeasuredStar, margin: f64, rows: usize, cols: usize) -> bool {
    let last = |len: usize| f64::from(u32::try_from(len).unwrap_or(u32::MAX)) - 1.0;
    star.x < margin
        || star.y < margin
        || star.x > last(cols) - margin
        || star.y > last(rows) - margin
}

/// Whether `star`'s aperture holds a pixel at full scale.
fn holds_full_scale(frame: &Frame, star: &MeasuredStar) -> bool {
    let (rows, cols) = frame.pixels().dim();
    let span = |centre: f64, len: usize| {
        let first = (centre - star.radius)
            .floor()
            .max(0.0)
            .to_usize()
            .unwrap_or(len);
        let end = ((centre + star.radius).ceil() + 1.0)
            .max(0.0)
            .to_usize()
            .unwrap_or(0)
            .min(len);
        first..end.max(first)
    };
    span(star.y, rows).any(|row| {
        span(star.x, cols).any(|col| {
            let at = |index: usize| f64::from(u32::try_from(index).unwrap_or(u32::MAX));
            frame.at_full_scale(row, col)
                && distance(at(col), at(row), star.x, star.y) <= star.radius
        })
    })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::frame::SensorType;
    use guide_simulator::{
        BayerResponse, CameraConfig, Cloud, GuideDirection, PierSide, Seeing, SimConfig, Simulator,
        SkyConfig, SkyOffset, Star, StarField,
    };
    use std::time::Duration;

    const EXPOSURE: Duration = Duration::from_secs(2);

    fn star(east: f64, north: f64, e_per_s: f64) -> Star {
        Star {
            position: SkyOffset::new(east, north),
            e_per_s,
        }
    }

    /// A 320 × 240 frame at 2″ a pixel, in 3″ seeing with no image motion.
    fn simulator(stars: Vec<Star>, adjust: impl FnOnce(&mut SimConfig)) -> Simulator {
        let mut config = SimConfig {
            seed: 5,
            camera: CameraConfig {
                width: 320,
                height: 240,
                ..CameraConfig::default()
            },
            sky: SkyConfig {
                stars: StarField::Listed(stars),
                seeing: Seeing {
                    fwhm_arcsec: 3.0,
                    common_motion_arcsec: 0.0,
                    star_motion_arcsec: 0.0,
                },
                clouds: Vec::new(),
            },
            ..SimConfig::default()
        };
        adjust(&mut config);
        Simulator::new(config).unwrap()
    }

    fn expose(sim: &mut Simulator) -> (Frame, guide_simulator::Frame) {
        let shot = sim.expose(EXPOSURE);
        let frame = Frame::new(&shot.pixels.view(), SensorType::Monochrome, Some(65_535)).unwrap();
        (frame, shot)
    }

    /// Four bright stars well apart, inside the edge margin.
    fn field() -> Vec<Star> {
        vec![
            star(-100.0, -60.0, 20_000.0),
            star(100.0, 60.0, 80_000.0),
            star(0.0, 0.0, 40_000.0),
            star(-100.0, 60.0, 10_000.0),
        ]
    }

    fn truth_of(shot: &guide_simulator::Frame, index: usize) -> (f64, f64) {
        let star = shot
            .truth
            .stars
            .iter()
            .find(|star| star.index == index)
            .unwrap();
        (star.position.x, star.position.y)
    }

    fn close(actual: f64, expected: f64, tolerance: f64) {
        assert!(
            (actual - expected).abs() <= tolerance,
            "{actual} is not within {tolerance} of {expected}"
        );
    }

    #[test]
    fn selection_keeps_the_most_stars_allowed_highest_snr_first() {
        let mut sim = simulator(field(), |_| {});
        let (frame, shot) = expose(&mut sim);
        let params = StarParams {
            max: 3,
            ..StarParams::default()
        };
        let (stars, first) = GuideStars::select(&frame, params).unwrap();
        let references = stars.references();
        assert_eq!(references.len(), 3);
        for (reference, index) in references.iter().zip([1, 2, 0]) {
            let (x, y) = truth_of(&shot, index);
            close(reference.x, x, 0.05);
            close(reference.y, y, 0.05);
        }
        assert_eq!(first.usable, 3);
        assert_eq!(first.offset, Some(Offset::default()));
        assert!(!first.star_lost);
    }

    #[test]
    fn selection_passes_over_a_saturated_star() {
        let mut stars = field();
        stars.push(star(100.0, -60.0, 2_000_000.0));
        let mut sim = simulator(stars, |_| {});
        let (frame, shot) = expose(&mut sim);
        let (selected, _) = GuideStars::select(&frame, StarParams::default()).unwrap();
        let (x, y) = truth_of(&shot, 4);
        assert_eq!(selected.references().len(), 4);
        assert!(selected
            .references()
            .iter()
            .all(|r| distance(r.x, r.y, x, y) > 5.0));
    }

    #[test]
    fn selection_passes_over_a_star_near_the_edge() {
        // 25 px from the left edge, inside the 40 px margin.
        let mut sim = simulator(
            vec![star(-269.0, 0.0, 80_000.0), star(0.0, 0.0, 20_000.0)],
            |_| {},
        );
        let (frame, shot) = expose(&mut sim);
        let (selected, _) = GuideStars::select(&frame, StarParams::default()).unwrap();
        let (x, y) = truth_of(&shot, 1);
        assert_eq!(selected.references().len(), 1);
        close(selected.references()[0].x, x, 0.05);
        close(selected.references()[0].y, y, 0.05);
    }

    #[test]
    fn selection_passes_over_both_stars_of_a_close_pair() {
        let mut sim = simulator(
            vec![
                star(60.0, 0.0, 80_000.0),
                star(80.0, 0.0, 60_000.0),
                star(-80.0, 0.0, 20_000.0),
            ],
            |_| {},
        );
        let (frame, shot) = expose(&mut sim);
        let (selected, _) = GuideStars::select(&frame, StarParams::default()).unwrap();
        let (x, _) = truth_of(&shot, 2);
        assert_eq!(selected.references().len(), 1);
        close(selected.references()[0].x, x, 0.05);
    }

    #[test]
    fn a_frame_without_stars_selects_none() {
        let mut sim = simulator(Vec::new(), |_| {});
        let (frame, _) = expose(&mut sim);
        assert_eq!(
            GuideStars::select(&frame, StarParams::default()).unwrap_err(),
            SelectError::NoStars
        );
    }

    #[test]
    fn hot_pixels_are_not_guide_stars() {
        let mut sim = simulator(Vec::new(), |config| {
            config.camera.hot_pixels = 30;
            config.camera.hot_pixel_e_per_s = 20_000.0;
        });
        let (frame, _) = expose(&mut sim);
        assert_eq!(
            GuideStars::select(&frame, StarParams::default()).unwrap_err(),
            SelectError::NoStars
        );
    }

    #[test]
    fn the_offset_follows_the_field() {
        let mut sim = simulator(field(), |_| {});
        let (frame, before) = expose(&mut sim);
        let (mut stars, _) = GuideStars::select(&frame, StarParams::default()).unwrap();
        sim.mount_mut()
            .pulse(GuideDirection::West, Duration::from_secs(1))
            .unwrap();
        sim.advance(Duration::from_secs(1));
        let (frame, after) = expose(&mut sim);
        let measured = stars.measure(&frame);
        let shift_x = after.truth.field_shift.x - before.truth.field_shift.x;
        let shift_y = after.truth.field_shift.y - before.truth.field_shift.y;
        assert!(shift_x > 3.0, "the pulse moved the field {shift_x} px");
        let offset = measured.offset.unwrap();
        close(offset.x, shift_x, 0.03);
        close(offset.y, shift_y, 0.03);
        assert_eq!(measured.usable, 4);
        assert!(measured.median_hfd.unwrap() > 1.0);
    }

    #[test]
    fn the_median_of_several_stars_holds_through_seeing() {
        let stars: Vec<Star> = (0..8)
            .map(|i| {
                let i = f64::from(i);
                star(
                    -180.0 + 50.0 * i,
                    if i % 2.0 == 0.0 { -60.0 } else { 60.0 },
                    30_000.0,
                )
            })
            .collect();
        let mut sim = simulator(stars, |config| config.sky.seeing.star_motion_arcsec = 0.6);
        let (frame, first) = expose(&mut sim);
        let (mut guide, _) = GuideStars::select(&frame, StarParams::default()).unwrap();
        assert_eq!(guide.references().len(), 8);
        // Each star's seeing moves it by 0.3 px of its own in every frame,
        // so what the guider measures is the median of the stars' true
        // offsets; the mount's shift of the field is near it.
        let positions = |shot: &guide_simulator::Frame| -> Vec<(usize, f64, f64)> {
            shot.truth
                .stars
                .iter()
                .map(|s| (s.index, s.position.x, s.position.y))
                .collect()
        };
        let base = positions(&first);
        for _ in 0..5 {
            let (frame, shot) = expose(&mut sim);
            let measured = guide.measure(&frame);
            assert_eq!(measured.usable, 8);
            let now = positions(&shot);
            let true_x: Vec<f64> = now
                .iter()
                .zip(&base)
                .map(|(n, b)| {
                    assert_eq!(n.0, b.0);
                    n.1 - b.1
                })
                .collect();
            let offset = measured.offset.unwrap();
            close(offset.x, median(true_x).unwrap(), 0.05);
            close(
                offset.x,
                shot.truth.field_shift.x - first.truth.field_shift.x,
                0.6,
            );
        }
    }

    #[test]
    fn stars_moved_beyond_the_search_region_are_lost() {
        let mut sim = simulator(field(), |_| {});
        let (frame, _) = expose(&mut sim);
        let (mut stars, _) = GuideStars::select(&frame, StarParams::default()).unwrap();
        sim.mount_mut()
            .slew(SkyOffset::new(50.0, 0.0), Duration::ZERO, PierSide::East)
            .unwrap();
        let (frame, _) = expose(&mut sim);
        let measured = stars.measure(&frame);
        assert!(measured.star_lost);
        assert_eq!(measured.offset, None);
        // Where a star was, an aperture finds nothing, or settles on the
        // sky's noise below the lowest signal-to-noise ratio.
        assert!(measured
            .stars
            .iter()
            .all(|reading| matches!(reading, StarReading::NotFound | StarReading::Faint { .. })));
    }

    #[test]
    fn a_thick_cloud_loses_the_stars_and_they_return_after_it() {
        let mut sim = simulator(field(), |config| {
            config.sky.clouds = vec![Cloud {
                start: Duration::from_secs(4),
                duration: Duration::from_secs(4),
                ramp: Duration::ZERO,
                transmission: 0.0,
            }];
        });
        let (frame, _) = expose(&mut sim);
        let (mut stars, _) = GuideStars::select(&frame, StarParams::default()).unwrap();
        let lost: Vec<bool> = (0..4)
            .map(|_| stars.measure(&expose(&mut sim).0).star_lost)
            .collect();
        assert_eq!(lost, [false, true, true, false]);
    }

    #[test]
    fn a_dimming_that_lasts_fails_the_flux_test_until_the_reference_starts_again() {
        let mut sim = simulator(field(), |config| {
            config.sky.clouds = vec![Cloud {
                start: Duration::from_secs(2),
                duration: Duration::from_hours(1),
                ramp: Duration::ZERO,
                transmission: 0.3,
            }];
        });
        let (frame, _) = expose(&mut sim);
        let (mut stars, _) = GuideStars::select(&frame, StarParams::default()).unwrap();
        let usable: Vec<bool> = (0..12)
            .map(|_| {
                matches!(
                    stars.measure(&expose(&mut sim).0).stars[0],
                    StarReading::Usable(_)
                )
            })
            .collect();
        assert_eq!(usable[..9], [false; 9]);
        assert_eq!(usable[9..], [true; 3]);
    }

    #[test]
    fn a_brief_dimming_keeps_the_reference() {
        let mut sim = simulator(field(), |config| {
            config.sky.clouds = vec![Cloud {
                start: Duration::from_secs(2),
                duration: Duration::from_secs(6),
                ramp: Duration::ZERO,
                transmission: 0.3,
            }];
        });
        let (frame, _) = expose(&mut sim);
        let (mut stars, _) = GuideStars::select(&frame, StarParams::default()).unwrap();
        let readings: Vec<StarReading> = (0..5)
            .map(|_| stars.measure(&expose(&mut sim).0).stars[0])
            .collect();
        assert!(readings[..3]
            .iter()
            .all(|r| matches!(r, StarReading::FluxChanged { .. })));
        assert!(readings[3..]
            .iter()
            .all(|r| matches!(r, StarReading::Usable(_))));
    }

    #[test]
    fn a_star_below_the_lowest_snr_is_faint() {
        let mut sim = simulator(field(), |_| {});
        let (frame, _) = expose(&mut sim);
        let params = StarParams {
            min_snr: 1.0e6,
            ..StarParams::default()
        };
        let (mut stars, _) = GuideStars::select(&frame, params).unwrap();
        let measured = stars.measure(&expose(&mut sim).0);
        assert!(measured.star_lost);
        assert!(measured
            .stars
            .iter()
            .all(|r| matches!(r, StarReading::Faint { .. })));
    }

    #[test]
    fn an_rggb_frame_is_measured_on_its_summed_cells() {
        let mut sim = simulator(field(), |config| {
            config.camera.width = 640;
            config.camera.height = 480;
            config.camera.pixel_scale_arcsec = 1.0;
            config.camera.sensor = guide_simulator::SensorType::Rggb(BayerResponse {
                red: 0.5,
                green: 1.0,
                blue: 0.4,
            });
        });
        let rggb = |sim: &mut Simulator| {
            let shot = sim.expose(EXPOSURE);
            let frame = Frame::new(&shot.pixels.view(), SensorType::Rggb, Some(65_535)).unwrap();
            (frame, shot)
        };
        let (frame, before) = rggb(&mut sim);
        let (mut stars, _) = GuideStars::select(&frame, StarParams::default()).unwrap();
        assert_eq!(stars.references().len(), 4);
        sim.mount_mut()
            .pulse(GuideDirection::West, Duration::from_secs(1))
            .unwrap();
        sim.advance(Duration::from_secs(1));
        let (frame, after) = rggb(&mut sim);
        let offset = stars.measure(&frame).offset.unwrap();
        // A summed pixel spans two of the camera's.
        close(
            offset.x,
            (after.truth.field_shift.x - before.truth.field_shift.x) / 2.0,
            0.05,
        );
    }

    fn measured(x: f64, flux: f64) -> MeasuredStar {
        MeasuredStar {
            x,
            y: 50.0,
            hfr: 1.0,
            flux,
            radius: 3.0,
            saturated: false,
            detection: 0,
        }
    }

    #[test]
    fn a_frame_that_is_not_a_flux_failure_ends_the_row() {
        let params = StarParams::default();
        let mut star = GuideStar::new(&measured(50.0, 1000.0));
        for _ in 0..9 {
            assert!(matches!(
                star.read(Some(measured(50.0, 300.0)), 1.0, &params),
                StarReading::FluxChanged { .. }
            ));
        }
        assert_eq!(star.read(None, 1.0, &params), StarReading::NotFound);
        for _ in 0..9 {
            assert!(matches!(
                star.read(Some(measured(50.0, 300.0)), 1.0, &params),
                StarReading::FluxChanged { .. }
            ));
        }
        assert!(matches!(
            star.read(Some(measured(50.0, 300.0)), 1.0, &params),
            StarReading::Usable(_)
        ));
    }

    #[test]
    fn a_star_found_outside_its_search_region_is_not_found() {
        let params = StarParams::default();
        let mut star = GuideStar::new(&measured(50.0, 1000.0));
        assert_eq!(
            star.read(Some(measured(66.0, 1000.0)), 1.0, &params),
            StarReading::NotFound
        );
        assert!(matches!(
            star.read(Some(measured(64.0, 1000.0)), 1.0, &params),
            StarReading::Usable(Usable { offset: Offset { x, .. }, .. }) if x == 14.0
        ));
    }

    #[test]
    fn the_median_takes_the_middle_two_of_an_even_count() {
        assert_eq!(median(vec![3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(vec![4.0, 1.0, 3.0, 2.0]), Some(2.5));
        assert_eq!(median(Vec::new()), None);
    }

    #[test]
    fn a_frame_with_fewer_usable_stars_than_the_fewest_is_star_lost() {
        let usable = StarReading::Usable(Usable {
            x: 1.0,
            y: 1.0,
            offset: Offset { x: 0.5, y: -0.5 },
            hfd: 2.0,
            snr: 10.0,
            flux: 100.0,
        });
        let lost = summarize(vec![usable, StarReading::NotFound], 2);
        assert!(lost.star_lost);
        assert_eq!(lost.offset, None);
        assert_eq!(lost.median_hfd, Some(2.0));
        let kept = summarize(vec![usable, StarReading::NotFound], 1);
        assert_eq!(kept.offset, Some(Offset { x: 0.5, y: -0.5 }));
    }
}
