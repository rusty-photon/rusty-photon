//! The renderer: a frame of the sensor over an exposure, and its truth.
//! See `docs/crates/guide-simulator.md` (Coordinates and units, The
//! renderer).

use std::f64::consts::{FRAC_1_SQRT_2, PI};
use std::ops::Range;
use std::time::Duration;

use ndarray::Array2;
use num_traits::ToPrimitive;

use crate::config::{self, ConfigError};
use crate::geometry::{coordinate, SensorPoint, SkyOffset};
use crate::random::{erf, Stream};
use crate::sky::{Seeing, Star};
use crate::time::SimTime;

/// The camera's stream within a run's seed.
const CAMERA_STREAM: u64 = 1;

/// The expected electrons from which shot and read noise are drawn
/// together, as one Gaussian.
const GAUSSIAN_FROM_E: f64 = 20.0;

/// How far a star's image is drawn, in standard deviations on each axis.
const REACH_SIGMAS: f64 = 4.0;

/// A Gaussian's full width at half maximum over its standard deviation,
/// 2√(2 ln 2).
const FWHM_PER_SIGMA: f64 = 2.354_820_045_030_949;

/// The sensor's colour layout.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SensorType {
    /// Every pixel sees all colours alike.
    Monochrome,
    /// A Bayer mosaic: red at an even row and an even column, blue at an
    /// odd row and an odd column, green at the other two.
    Rggb(BayerResponse),
}

/// How strongly each colour's pixels respond to the light, as a
/// multiplier.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BayerResponse {
    /// The red pixels.
    pub red: f64,
    /// The two green pixels of each cell.
    pub green: f64,
    /// The blue pixels.
    pub blue: f64,
}

/// The camera: its sensor, its optics' scale and its orientation.
#[derive(Debug, Clone, PartialEq)]
pub struct CameraConfig {
    /// Pixels in a row.
    pub width: usize,
    /// Rows.
    pub height: usize,
    /// Arcseconds a pixel spans.
    pub pixel_scale_arcsec: f64,
    /// The angle, counter-clockwise from +x toward +y, at which the
    /// sky's East lies on the sensor on the pier side the mount starts
    /// on: where a West pulse moves the stars.
    pub angle_deg: f64,
    /// Whether the image is mirrored, which puts the sky's North 90°
    /// clockwise of East rather than counter-clockwise.
    pub mirrored: bool,
    /// The colour layout.
    pub sensor: SensorType,
    /// Electrons an ADU.
    pub gain_e_per_adu: f64,
    /// The ADU every pixel reads with no light.
    pub bias_adu: f64,
    /// The read noise's standard deviation, in electrons.
    pub read_noise_e: f64,
    /// The highest ADU a pixel reads: its clip.
    pub full_scale_adu: u16,
    /// The sky's light, in electrons a pixel a second.
    pub sky_e_per_pixel_per_s: f64,
    /// How many hot pixels.
    pub hot_pixels: usize,
    /// The electrons a second each hot pixel gains.
    pub hot_pixel_e_per_s: f64,
}

impl Default for CameraConfig {
    /// A 640 × 480 monochrome sensor at 2″ a pixel, unrotated, with a
    /// gain of 1 electron an ADU, a bias of 500 ADU, 3 electrons of read
    /// noise, a 16-bit clip, 10 electrons a pixel a second of sky and no
    /// hot pixels.
    fn default() -> Self {
        Self {
            width: 640,
            height: 480,
            pixel_scale_arcsec: 2.0,
            angle_deg: 0.0,
            mirrored: false,
            sensor: SensorType::Monochrome,
            gain_e_per_adu: 1.0,
            bias_adu: 500.0,
            read_noise_e: 3.0,
            full_scale_adu: u16::MAX,
            sky_e_per_pixel_per_s: 10.0,
            hot_pixels: 0,
            hot_pixel_e_per_s: 5_000.0,
        }
    }
}

impl CameraConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        config::require(self.width > 0, "camera.width", "must be above zero")?;
        config::require(self.height > 0, "camera.height", "must be above zero")?;
        config::positive("camera.pixel_scale_arcsec", self.pixel_scale_arcsec)?;
        config::finite("camera.angle_deg", self.angle_deg)?;
        config::positive("camera.gain_e_per_adu", self.gain_e_per_adu)?;
        config::finite("camera.bias_adu", self.bias_adu)?;
        config::non_negative("camera.read_noise_e", self.read_noise_e)?;
        config::non_negative("camera.sky_e_per_pixel_per_s", self.sky_e_per_pixel_per_s)?;
        config::non_negative("camera.hot_pixel_e_per_s", self.hot_pixel_e_per_s)?;
        if let SensorType::Rggb(response) = self.sensor {
            config::non_negative("camera.sensor.red", response.red)?;
            config::non_negative("camera.sensor.green", response.green)?;
            config::non_negative("camera.sensor.blue", response.blue)?;
        }
        Ok(())
    }
}

/// A rendered frame and its truth.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    /// The ADU of each pixel, indexed `[[y, x]]`.
    pub pixels: Array2<u16>,
    /// What the frame shows, as the simulation knows it.
    pub truth: FrameTruth,
}

/// What a frame shows, averaged over its exposure.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameTruth {
    /// When the exposure started.
    pub start: SimTime,
    /// How long it lasted.
    pub duration: Duration,
    /// The pointing.
    pub pointing: SkyOffset,
    /// Where a star that sat at the frame's centre at the start lies,
    /// less the frame's centre: the pointing's shift of the field,
    /// without seeing.
    pub field_shift: SensorPoint,
    /// The fraction of the stars' light the clouds let through.
    pub transmission: f64,
    /// The stars whose averaged position lies on the frame.
    pub stars: Vec<StarTruth>,
}

/// A star on a frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StarTruth {
    /// Its index in the star field.
    pub index: usize,
    /// Its position, averaged over the exposure, seeing included.
    pub position: SensorPoint,
    /// The electrons it delivered, before any Bayer response.
    pub electrons: f64,
}

/// The state of the sky and the mount at the middle of a slice of an
/// exposure.
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub(crate) pointing: SkyOffset,
    pub(crate) flipped: bool,
    pub(crate) transmission: f64,
    /// The slice's length, in seconds.
    pub(crate) secs: f64,
}

/// The renderer, with the hot pixels it keeps for the run.
#[derive(Debug, Clone)]
pub struct Camera {
    config: CameraConfig,
    /// `(row, column)` of each hot pixel.
    hot_pixels: Vec<(usize, usize)>,
    stream: Stream,
}

impl Camera {
    pub(crate) fn new(config: CameraConfig, seed: u64) -> Result<Self, ConfigError> {
        config.validate()?;
        let mut stream = Stream::new(seed, CAMERA_STREAM);
        let hot_pixels = (0..config.hot_pixels)
            .map(|_| (stream.index(config.height), stream.index(config.width)))
            .collect();
        Ok(Self {
            config,
            hot_pixels,
            stream,
        })
    }

    pub(crate) const fn config(&self) -> &CameraConfig {
        &self.config
    }

    /// The frame's centre.
    fn centre(&self) -> SensorPoint {
        SensorPoint::new(
            (coordinate(self.config.width) - 1.0) / 2.0,
            (coordinate(self.config.height) - 1.0) / 2.0,
        )
    }

    /// Where an offset from the frame's centre on the sky lands on the
    /// sensor, turned by 180° when the mount has `flipped` pier side.
    pub(crate) fn to_sensor(&self, offset: SkyOffset, flipped: bool) -> SensorPoint {
        let scale = self.config.pixel_scale_arcsec;
        let parity = if self.config.mirrored { -1.0 } else { 1.0 };
        let (u, v) = (offset.east / scale, parity * offset.north / scale);
        let turn = if flipped { PI } else { 0.0 };
        let (sin, cos) = (self.config.angle_deg.to_radians() + turn).sin_cos();
        let centre = self.centre();
        SensorPoint::new(
            u.mul_add(cos, (-v).mul_add(sin, centre.x)),
            u.mul_add(sin, v.mul_add(cos, centre.y)),
        )
    }

    /// The sky offset from the frame's centre that lands on `point` on
    /// the pier side the mount starts on.
    pub(crate) fn to_sky(&self, point: SensorPoint) -> SkyOffset {
        let scale = self.config.pixel_scale_arcsec;
        let parity = if self.config.mirrored { -1.0 } else { 1.0 };
        let (sin, cos) = self.config.angle_deg.to_radians().sin_cos();
        let offset = point.minus(self.centre());
        let u = offset.x.mul_add(cos, offset.y * sin);
        let v = offset.y.mul_add(cos, -(offset.x * sin));
        SkyOffset::new(u * scale, parity * v * scale)
    }

    /// Renders the exposure the samples cover.
    pub(crate) fn render(
        &mut self,
        samples: &[Sample],
        stars: &[Star],
        seeing: &Seeing,
        opened: SimTime,
        duration: Duration,
    ) -> Frame {
        let (width, height) = (self.config.width, self.config.height);
        let sigma = seeing.fwhm_arcsec / FWHM_PER_SIGMA / self.config.pixel_scale_arcsec;
        let common = self.motion(seeing.common_motion_arcsec);
        let own: Vec<SkyOffset> = stars
            .iter()
            .map(|_| self.motion(seeing.star_motion_arcsec))
            .collect();
        let centre = self.centre();
        let mut electrons = Array2::<f64>::zeros((height, width));
        let mut tallies = vec![Tally::default(); stars.len()];
        let mut shift = Tally::default();
        let mut pointing = SkyOffset::ZERO;
        let mut transmission = 0.0;
        for sample in samples {
            for ((star, own), tally) in stars.iter().zip(&own).zip(&mut tallies) {
                let at = self.to_sensor(
                    star.position.plus(common).plus(*own).minus(sample.pointing),
                    sample.flipped,
                );
                let light = star.e_per_s * sample.transmission * sample.secs;
                draw(&mut electrons, at, sigma, light);
                tally.add(at, light);
            }
            shift.add(
                self.to_sensor(SkyOffset::ZERO.minus(sample.pointing), sample.flipped)
                    .minus(centre),
                0.0,
            );
            pointing = pointing.plus(sample.pointing);
            transmission += sample.transmission;
        }
        let count = shift.count.max(1.0);
        let truth = FrameTruth {
            start: opened,
            duration,
            pointing: pointing.scaled(1.0 / count),
            field_shift: shift.mean(),
            transmission: transmission / count,
            stars: tallies
                .iter()
                .enumerate()
                .map(|(index, tally)| StarTruth {
                    index,
                    position: tally.mean(),
                    electrons: tally.light,
                })
                .filter(|star| self.on_frame(star.position))
                .collect(),
        };
        let exposure = duration.as_secs_f64();
        let sky = self.config.sky_e_per_pixel_per_s * exposure;
        for ((row, col), value) in electrons.indexed_iter_mut() {
            *value = (*value + sky) * self.response(row, col);
        }
        for &(row, col) in &self.hot_pixels {
            if let Some(value) = electrons.get_mut((row, col)) {
                *value = self.config.hot_pixel_e_per_s.mul_add(exposure, *value);
            }
        }
        let config = &self.config;
        let stream = &mut self.stream;
        let pixels = electrons.mapv(|expected| {
            let read = config.read_noise_e;
            let drawn = if expected >= GAUSSIAN_FROM_E {
                read.mul_add(read, expected)
                    .sqrt()
                    .mul_add(stream.normal(), expected)
            } else {
                read.mul_add(stream.normal(), stream.poisson(expected.max(0.0)))
            };
            (drawn / config.gain_e_per_adu + config.bias_adu)
                .round()
                .clamp(0.0, f64::from(config.full_scale_adu))
                .to_u16()
                .unwrap_or(0)
        });
        Frame { pixels, truth }
    }

    /// One frame's image motion: a random offset of standard deviation
    /// `sigma` on each sky axis.
    fn motion(&mut self, sigma: f64) -> SkyOffset {
        let east = sigma * self.stream.normal();
        SkyOffset::new(east, sigma * self.stream.normal())
    }

    /// The response of the pixel at `row`, `col`.
    const fn response(&self, row: usize, col: usize) -> f64 {
        match self.config.sensor {
            SensorType::Monochrome => 1.0,
            SensorType::Rggb(response) => match (row.is_multiple_of(2), col.is_multiple_of(2)) {
                (true, true) => response.red,
                (false, false) => response.blue,
                _ => response.green,
            },
        }
    }

    /// Whether `point` lies on one of the frame's pixels.
    fn on_frame(&self, point: SensorPoint) -> bool {
        (-0.5..coordinate(self.config.width) - 0.5).contains(&point.x)
            && (-0.5..coordinate(self.config.height) - 0.5).contains(&point.y)
    }
}

/// Sums of a star's or the field's positions over an exposure's samples.
#[derive(Debug, Clone, Copy, Default)]
struct Tally {
    x: f64,
    y: f64,
    count: f64,
    light: f64,
}

impl Tally {
    fn add(&mut self, at: SensorPoint, light: f64) {
        self.x += at.x;
        self.y += at.y;
        self.count += 1.0;
        self.light += light;
    }

    fn mean(&self) -> SensorPoint {
        let count = self.count.max(1.0);
        SensorPoint::new(self.x / count, self.y / count)
    }
}

/// Adds `light` electrons of a Gaussian image of standard deviation
/// `sigma` pixels, centred on `at`, to `image`.
fn draw(image: &mut Array2<f64>, at: SensorPoint, sigma: f64, light: f64) {
    if light <= 0.0 {
        return;
    }
    let (height, width) = image.dim();
    let reach = REACH_SIGMAS * sigma;
    let cols = pixel_span(at.x - reach, at.x + reach, width);
    let across: Vec<f64> = cols
        .clone()
        .map(|col| pixel_share(col, at.x, sigma))
        .collect();
    for row in pixel_span(at.y - reach, at.y + reach, height) {
        let down = light * pixel_share(row, at.y, sigma);
        for (col, share) in cols.clone().zip(&across) {
            if let Some(value) = image.get_mut((row, col)) {
                *value = down.mul_add(*share, *value);
            }
        }
    }
}

/// The indices of the pixels, of `len` along an axis, whose span reaches
/// into `[low, high]`.
fn pixel_span(low: f64, high: f64, len: usize) -> Range<usize> {
    let first = (low + 0.5).floor().max(0.0).to_usize().unwrap_or(len);
    let end = ((high + 0.5).floor() + 1.0)
        .max(0.0)
        .to_usize()
        .unwrap_or(0)
        .min(len);
    first..end.max(first)
}

/// The share of a unit Gaussian of standard deviation `sigma`, centred
/// on `centre`, that falls in the pixel at `index` along one axis.
fn pixel_share(index: usize, centre: f64, sigma: f64) -> f64 {
    let scale = FRAC_1_SQRT_2 / sigma;
    let near = coordinate(index) - centre;
    0.5 * (erf((near + 0.5) * scale) - erf((near - 0.5) * scale))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn camera(config: CameraConfig) -> Camera {
        Camera::new(config, 1).unwrap()
    }

    fn quiet() -> CameraConfig {
        CameraConfig {
            width: 64,
            height: 48,
            gain_e_per_adu: 4.0,
            read_noise_e: 0.0,
            sky_e_per_pixel_per_s: 0.0,
            ..CameraConfig::default()
        }
    }

    fn still(secs: f64) -> Sample {
        Sample {
            pointing: SkyOffset::ZERO,
            flipped: false,
            transmission: 1.0,
            secs,
        }
    }

    fn seeing() -> Seeing {
        Seeing {
            fwhm_arcsec: 3.0,
            common_motion_arcsec: 0.0,
            star_motion_arcsec: 0.0,
        }
    }

    fn star(east: f64, north: f64, e_per_s: f64) -> Star {
        Star {
            position: SkyOffset::new(east, north),
            e_per_s,
        }
    }

    fn render(camera: &mut Camera, stars: &[Star], samples: &[Sample]) -> Frame {
        let duration = Duration::from_secs_f64(samples.iter().map(|s| s.secs).sum());
        camera.render(samples, stars, &seeing(), SimTime::START, duration)
    }

    /// The light-weighted centroid within `half` pixels of `around`.
    fn centroid(pixels: &Array2<u16>, bias: f64, around: SensorPoint, half: usize) -> SensorPoint {
        let (cx, cy) = (around.x.round() as usize, around.y.round() as usize);
        let (mut sx, mut sy, mut sum) = (0.0, 0.0, 0.0);
        for r in cy - half..=cy + half {
            for c in cx - half..=cx + half {
                let v = f64::from(pixels[[r, c]]) - bias;
                sx += v * c as f64;
                sy += v * r as f64;
                sum += v;
            }
        }
        SensorPoint::new(sx / sum, sy / sum)
    }

    fn mean_and_spread(values: &[f64]) -> (f64, f64) {
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        let var = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / values.len() as f64;
        (mean, var.sqrt())
    }

    #[test]
    fn a_star_s_light_lands_on_the_sensor_but_for_its_far_wings() {
        let mut image = Array2::<f64>::zeros((40, 40));
        draw(&mut image, SensorPoint::new(20.3, 19.6), 1.5, 1000.0);
        let kept = 1000.0 * erf(4.0 * FRAC_1_SQRT_2).powi(2);
        assert!((image.sum() - kept).abs() < 0.5, "sum {}", image.sum());
    }

    #[test]
    fn a_star_centred_between_two_pixels_lights_them_equally() {
        let mut image = Array2::<f64>::zeros((9, 9));
        draw(&mut image, SensorPoint::new(4.5, 4.0), 1.0, 1000.0);
        // The approximation reads 1e-9 rather than 0 at 0, which the two
        // pixels see with opposite signs.
        assert!((image[[4, 4]] - image[[4, 5]]).abs() < 1e-5);
        assert!((image[[3, 3]] - image[[5, 6]]).abs() < 1e-5);
    }

    #[test]
    fn a_star_off_the_frame_draws_nothing() {
        let mut image = Array2::<f64>::zeros((9, 9));
        draw(&mut image, SensorPoint::new(-20.0, 4.0), 1.0, 1000.0);
        draw(&mut image, SensorPoint::new(4.0, 30.0), 1.0, 1000.0);
        draw(&mut image, SensorPoint::new(f64::NAN, 4.0), 1.0, 1000.0);
        assert_eq!(image.sum(), 0.0);
    }

    #[test]
    fn east_lies_at_the_angle_and_north_a_quarter_turn_on() {
        let c = camera(quiet());
        let centre = c.centre();
        assert_eq!(
            c.to_sensor(SkyOffset::new(2.0, 0.0), false).minus(centre),
            SensorPoint::new(1.0, 0.0)
        );
        assert_eq!(
            c.to_sensor(SkyOffset::new(0.0, 2.0), false).minus(centre),
            SensorPoint::new(0.0, 1.0)
        );
    }

    #[test]
    fn a_mirror_puts_north_a_quarter_turn_the_other_way() {
        let c = camera(CameraConfig {
            mirrored: true,
            ..quiet()
        });
        let moved = c
            .to_sensor(SkyOffset::new(0.0, 2.0), false)
            .minus(c.centre());
        assert_eq!(moved, SensorPoint::new(0.0, -1.0));
    }

    #[test]
    fn the_other_pier_side_turns_the_view_by_half_a_turn() {
        let c = camera(quiet());
        let moved = c
            .to_sensor(SkyOffset::new(2.0, 2.0), true)
            .minus(c.centre());
        assert!(
            (moved.x + 1.0).abs() < 1e-12 && (moved.y + 1.0).abs() < 1e-12,
            "{moved:?}"
        );
    }

    #[test]
    fn the_angle_turns_east_counter_clockwise_toward_plus_y() {
        let c = camera(CameraConfig {
            angle_deg: 90.0,
            ..quiet()
        });
        let moved = c
            .to_sensor(SkyOffset::new(2.0, 0.0), false)
            .minus(c.centre());
        assert!(
            moved.x.abs() < 1e-12 && (moved.y - 1.0).abs() < 1e-12,
            "{moved:?}"
        );
    }

    #[test]
    fn to_sky_undoes_to_sensor() {
        let c = camera(CameraConfig {
            angle_deg: 33.0,
            mirrored: true,
            ..quiet()
        });
        let sky = SkyOffset::new(12.5, -7.25);
        let back = c.to_sky(c.to_sensor(sky, false));
        assert!((back.east - sky.east).abs() < 1e-9 && (back.north - sky.north).abs() < 1e-9);
    }

    #[test]
    fn a_bright_star_is_centred_where_the_truth_puts_it() {
        let mut c = camera(quiet());
        let frame = render(&mut c, &[star(5.3, -3.1, 200_000.0)], &[still(1.0)]);
        let truth = frame.truth.stars[0].position;
        let measured = centroid(&frame.pixels, 500.0, truth, 5);
        assert!(
            (measured.x - truth.x).abs() < 0.01,
            "{measured:?} against {truth:?}"
        );
        assert!(
            (measured.y - truth.y).abs() < 0.01,
            "{measured:?} against {truth:?}"
        );
        assert!((frame.truth.stars[0].electrons - 200_000.0).abs() < 1e-6);
    }

    #[test]
    fn a_blank_frame_has_the_sky_s_mean_and_the_spread_of_its_noise() {
        let mut c = camera(CameraConfig {
            width: 200,
            height: 200,
            gain_e_per_adu: 2.0,
            bias_adu: 100.0,
            read_noise_e: 5.0,
            sky_e_per_pixel_per_s: 100.0,
            ..CameraConfig::default()
        });
        let frame = render(&mut c, &[], &[still(1.0)]);
        let values: Vec<f64> = frame.pixels.iter().map(|&v| f64::from(v)).collect();
        let (mean, spread) = mean_and_spread(&values);
        assert!((mean - 150.0).abs() < 0.2, "mean {mean}");
        assert!(
            (spread - 125.0_f64.sqrt() / 2.0).abs() < 0.2,
            "spread {spread}"
        );
    }

    #[test]
    fn a_faint_sky_is_drawn_as_whole_electrons() {
        let mut c = camera(CameraConfig {
            width: 200,
            height: 200,
            gain_e_per_adu: 1.0,
            bias_adu: 0.0,
            read_noise_e: 0.0,
            sky_e_per_pixel_per_s: 2.0,
            ..CameraConfig::default()
        });
        let frame = render(&mut c, &[], &[still(1.0)]);
        let values: Vec<f64> = frame.pixels.iter().map(|&v| f64::from(v)).collect();
        let (mean, spread) = mean_and_spread(&values);
        assert!((mean - 2.0).abs() < 0.05, "mean {mean}");
        assert!((spread - 2.0_f64.sqrt()).abs() < 0.05, "spread {spread}");
    }

    #[test]
    fn a_pixel_beyond_full_scale_reads_full_scale() {
        let mut c = camera(CameraConfig {
            gain_e_per_adu: 1.0,
            full_scale_adu: 4_000,
            ..quiet()
        });
        let frame = render(&mut c, &[star(0.0, 0.0, 1_000_000.0)], &[still(1.0)]);
        assert_eq!(frame.pixels.iter().max(), Some(&4_000));
    }

    #[test]
    fn an_rggb_sensor_weights_each_colour_by_its_response() {
        let mut c = camera(CameraConfig {
            width: 200,
            height: 200,
            sensor: SensorType::Rggb(BayerResponse {
                red: 0.5,
                green: 1.0,
                blue: 0.25,
            }),
            gain_e_per_adu: 1.0,
            bias_adu: 0.0,
            sky_e_per_pixel_per_s: 1_000.0,
            ..quiet()
        });
        let frame = render(&mut c, &[], &[still(1.0)]);
        let colour = |row: usize, col: usize| {
            let values: Vec<f64> = frame
                .pixels
                .indexed_iter()
                .filter(|((r, c), _)| r % 2 == row && c % 2 == col)
                .map(|(_, &v)| f64::from(v))
                .collect();
            mean_and_spread(&values).0
        };
        assert!((colour(0, 0) - 500.0).abs() < 1.0);
        assert!((colour(0, 1) - 1_000.0).abs() < 1.0);
        assert!((colour(1, 0) - 1_000.0).abs() < 1.0);
        assert!((colour(1, 1) - 250.0).abs() < 1.0);
    }

    #[test]
    fn hot_pixels_stand_out_in_the_same_places_in_every_frame() {
        let mut c = camera(CameraConfig {
            hot_pixels: 5,
            hot_pixel_e_per_s: 10_000.0,
            ..quiet()
        });
        let hot = |frame: &Frame| -> Vec<(usize, usize)> {
            frame
                .pixels
                .indexed_iter()
                .filter(|(_, &v)| v > 1_000)
                .map(|(at, _)| at)
                .collect()
        };
        let first = hot(&render(&mut c, &[], &[still(1.0)]));
        let second = hot(&render(&mut c, &[], &[still(1.0)]));
        assert!(!first.is_empty() && first.len() <= 5, "{first:?}");
        assert_eq!(first, second);
    }

    #[test]
    fn the_truth_lists_only_the_stars_on_the_frame() {
        let mut c = camera(quiet());
        let frame = render(
            &mut c,
            &[star(0.0, 0.0, 1_000.0), star(1_000.0, 0.0, 1_000.0)],
            &[still(1.0)],
        );
        assert_eq!(frame.truth.stars.len(), 1);
        assert_eq!(frame.truth.stars[0].index, 0);
    }

    #[test]
    fn the_truth_averages_the_samples() {
        let mut c = camera(quiet());
        let moving = [
            Sample {
                pointing: SkyOffset::new(-2.0, 0.0),
                transmission: 1.0,
                ..still(0.5)
            },
            Sample {
                pointing: SkyOffset::new(-6.0, 0.0),
                transmission: 0.5,
                ..still(0.5)
            },
        ];
        let frame = render(&mut c, &[star(0.0, 0.0, 1_000.0)], &moving);
        assert_eq!(frame.truth.pointing, SkyOffset::new(-4.0, 0.0));
        assert_eq!(frame.truth.field_shift, SensorPoint::new(2.0, 0.0));
        assert_eq!(frame.truth.transmission, 0.75);
        assert_eq!(frame.truth.stars[0].electrons, 750.0);
    }

    #[test]
    fn construction_refuses_what_the_renderer_cannot_draw() {
        let refused = |config: CameraConfig| Camera::new(config, 1).unwrap_err().field;
        assert_eq!(
            refused(CameraConfig {
                width: 0,
                ..quiet()
            }),
            "camera.width"
        );
        assert_eq!(
            refused(CameraConfig {
                pixel_scale_arcsec: -1.0,
                ..quiet()
            }),
            "camera.pixel_scale_arcsec"
        );
        assert_eq!(
            refused(CameraConfig {
                sensor: SensorType::Rggb(BayerResponse {
                    red: 1.0,
                    green: -1.0,
                    blue: 1.0,
                }),
                ..quiet()
            }),
            "camera.sensor.green"
        );
    }
}
