//! The frame the guider measures: the camera's frame read for its sensor
//! type, its pixels at full scale marked, and its background. See
//! `docs/services/guider.md` (Sensor handling, Star selection and
//! measurement).

use ndarray::{Array2, ArrayView2};
use rp_image_analysis::background::BackgroundStats;
use rp_image_analysis::pixel::Pixel;
use rp_image_analysis::saturation::saturation_level;

use crate::background::sampled_background;

/// How the guider reads a camera's frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SensorType {
    /// The frame as read.
    Monochrome,
    /// A Bayer mosaic read unbinned: each 2×2 cell is summed into one
    /// pixel, a luminance frame wherever the cells start.
    Rggb,
}

/// Why a camera's frame cannot be measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    /// The frame has no pixel, or an RGGB frame no whole 2×2 cell.
    #[error("the frame has no pixels to measure")]
    Empty,
}

/// A frame ready to measure.
#[derive(Debug, Clone)]
pub struct Frame {
    pixels: Array2<i32>,
    full_scale: Array2<bool>,
    background: BackgroundStats,
}

impl Frame {
    /// Reads `camera`, a frame of a sensor of type `sensor`. A pixel is at
    /// full scale when it is at or above the camera frame's saturation
    /// level: its own clip, or `max_adu`, or the lower of the two. A
    /// summed pixel is at full scale when any of its four is. An RGGB
    /// frame's odd last row or column, half a cell, is left out.
    ///
    /// # Errors
    ///
    /// [`FrameError::Empty`] when nothing is left to measure.
    pub fn new<T: Pixel>(
        camera: &ArrayView2<T>,
        sensor: SensorType,
        max_adu: Option<u32>,
    ) -> Result<Self, FrameError> {
        let level = saturation_level(camera, max_adu);
        let at_full_scale = |value: T| level.is_some_and(|level| value.to_u32() >= level);
        let (pixels, full_scale) = match sensor {
            SensorType::Monochrome => (
                camera.mapv(|value| to_i32(value.to_u32())),
                camera.mapv(at_full_scale),
            ),
            SensorType::Rggb => sum_cells(camera, at_full_scale),
        };
        let background = sampled_background(&pixels.view()).ok_or(FrameError::Empty)?;
        Ok(Self {
            pixels,
            full_scale,
            background,
        })
    }

    /// The pixels measured, indexed `[[y, x]]`.
    #[must_use]
    pub fn pixels(&self) -> ArrayView2<'_, i32> {
        self.pixels.view()
    }

    /// The background, from a sample of the pixels.
    #[must_use]
    pub const fn background(&self) -> &BackgroundStats {
        &self.background
    }

    /// Whether the pixel at `row`, `col` is at full scale.
    pub(crate) fn at_full_scale(&self, row: usize, col: usize) -> bool {
        self.full_scale.get((row, col)).copied().unwrap_or(false)
    }
}

/// Each 2×2 cell of `camera` summed into one pixel, and whether any of the
/// four is at full scale.
fn sum_cells<T: Pixel>(
    camera: &ArrayView2<T>,
    at_full_scale: impl Fn(T) -> bool,
) -> (Array2<i32>, Array2<bool>) {
    let (rows, cols) = camera.dim();
    let shape = (rows.div_euclid(2), cols.div_euclid(2));
    let cell = |(row, col): (usize, usize)| {
        let (top, left) = (row.saturating_mul(2), col.saturating_mul(2));
        [
            (top, left),
            (top, left.saturating_add(1)),
            (top.saturating_add(1), left),
            (top.saturating_add(1), left.saturating_add(1)),
        ]
        .into_iter()
        .filter_map(|at| camera.get(at).copied())
    };
    let sums = Array2::from_shape_fn(shape, |at| {
        to_i32(cell(at).fold(0_u32, |sum, value| sum.saturating_add(value.to_u32())))
    });
    let full_scale = Array2::from_shape_fn(shape, |at| cell(at).any(&at_full_scale));
    (sums, full_scale)
}

/// A pixel's value as the measured frame holds it. A sum of four 16-bit
/// pixels fits.
fn to_i32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use ndarray::array;

    #[test]
    fn a_monochrome_frame_is_read_as_it_is() {
        let camera = array![[10_u16, 20, 30], [40, 50, 60]];
        let frame = Frame::new(&camera.view(), SensorType::Monochrome, None).unwrap();
        assert_eq!(frame.pixels(), array![[10, 20, 30], [40, 50, 60]]);
        assert!(!frame.at_full_scale(1, 2));
    }

    #[test]
    fn a_monochrome_pixel_at_max_adu_is_at_full_scale() {
        let camera = array![[10_u16, 20, 60], [40, 50, 61]];
        let frame = Frame::new(&camera.view(), SensorType::Monochrome, Some(60)).unwrap();
        assert!(frame.at_full_scale(0, 2));
        assert!(frame.at_full_scale(1, 2));
        assert!(!frame.at_full_scale(1, 1));
    }

    #[test]
    fn an_rggb_frame_sums_its_cells_and_leaves_out_half_a_cell() {
        let camera = array![[1_u16, 2, 3, 4, 99], [5, 6, 7, 8, 99], [99, 99, 99, 99, 99],];
        let frame = Frame::new(&camera.view(), SensorType::Rggb, None).unwrap();
        assert_eq!(frame.pixels(), array![[14, 22]]);
    }

    #[test]
    fn an_rggb_cell_whose_green_clips_is_at_full_scale_below_four_times_the_level() {
        let camera = array![[100_u16, 1000, 100, 100], [100, 100, 100, 100]];
        let frame = Frame::new(&camera.view(), SensorType::Rggb, Some(1000)).unwrap();
        assert_eq!(frame.pixels(), array![[1300, 400]]);
        assert!(frame.at_full_scale(0, 0));
        assert!(!frame.at_full_scale(0, 1));
    }

    #[test]
    fn a_frame_with_nothing_to_measure_is_refused() {
        let camera = ndarray::Array2::<u16>::zeros((1, 5));
        assert_eq!(
            Frame::new(&camera.view(), SensorType::Rggb, None).unwrap_err(),
            FrameError::Empty
        );
    }
}
