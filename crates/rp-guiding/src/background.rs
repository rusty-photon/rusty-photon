//! The frame's background, from a sample of its pixels.

use ndarray::ArrayView2;
use rp_image_analysis::background::{sigma_clipped_values, BackgroundStats};

/// The fewest pixels the sample holds, when the frame has as many.
pub const MIN_SAMPLE: usize = 50_000;

/// The sigma-clip of the sample: the `k` and the rounds of `rp`'s
/// background.
const CLIP_SIGMA: f64 = 3.0;
const CLIP_ROUNDS: usize = 5;

/// The background of `view`, from a sample of its pixels.
///
/// The sample is every `s`-th pixel of every `s`-th row, `s` the whole
/// square root of the pixels over [`MIN_SAMPLE`] and at least 1, so it
/// holds at least [`MIN_SAMPLE`] pixels, or every pixel of a smaller
/// frame. `None` for an empty frame.
#[must_use]
pub fn sampled_background(view: &ArrayView2<i32>) -> Option<BackgroundStats> {
    let stride = sample_stride(view.len());
    let sample: Vec<f64> = view
        .rows()
        .into_iter()
        .step_by(stride)
        .flat_map(|row| row.into_iter().step_by(stride).copied())
        .map(f64::from)
        .collect();
    sigma_clipped_values(sample, CLIP_SIGMA, CLIP_ROUNDS)
}

/// The stride that samples `pixels` down to at least [`MIN_SAMPLE`].
fn sample_stride(pixels: usize) -> usize {
    pixels.checked_div(MIN_SAMPLE).unwrap_or(0).isqrt().max(1)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use ndarray::Array2;
    use rp_image_analysis::background::estimate_background;

    #[test]
    fn a_small_frame_is_sampled_whole() {
        assert_eq!(sample_stride(0), 1);
        assert_eq!(sample_stride(640 * 480), 2);
        assert_eq!(sample_stride(99_999), 1);
    }

    #[test]
    fn the_sample_holds_at_least_its_minimum() {
        for (rows, cols) in [(2192, 3864), (1096, 1932), (480, 640), (317, 211)] {
            let stride = sample_stride(rows * cols);
            let held = rows.div_ceil(stride) * cols.div_ceil(stride);
            assert!(held >= MIN_SAMPLE.min(rows * cols), "{rows}×{cols}: {held}");
        }
    }

    #[test]
    fn a_frame_sampled_whole_matches_the_full_estimate() {
        let frame = Array2::from_shape_fn((100, 100), |(r, c)| {
            i32::try_from((r * 7 + c * 13) % 50).unwrap()
        });
        let sampled = sampled_background(&frame.view()).unwrap();
        let full = estimate_background(&frame.view()).unwrap();
        assert_eq!(sampled.mean, full.mean);
        assert_eq!(sampled.stddev, full.stddev);
    }

    #[test]
    fn a_sampled_star_field_has_the_whole_frame_s_background() {
        let mut sim = guide_simulator::Simulator::new(guide_simulator::SimConfig {
            seed: 3,
            camera: guide_simulator::CameraConfig {
                width: 1000,
                height: 800,
                ..guide_simulator::CameraConfig::default()
            },
            sky: guide_simulator::SkyConfig {
                stars: guide_simulator::StarField::Random(guide_simulator::RandomStars {
                    count: 200,
                    faintest_e_per_s: 500.0,
                    brightest_e_per_s: 200_000.0,
                }),
                ..guide_simulator::SkyConfig::default()
            },
            ..guide_simulator::SimConfig::default()
        })
        .unwrap();
        let shot = sim.expose(std::time::Duration::from_secs(2));
        let frame = shot.pixels.mapv(i32::from);
        assert_eq!(sample_stride(frame.len()), 4);
        let sampled = sampled_background(&frame.view()).unwrap();
        let full = estimate_background(&frame.view()).unwrap();
        assert!(
            (sampled.mean - full.mean).abs() < 0.2,
            "{sampled:?} against {full:?}"
        );
        assert!(
            (sampled.stddev / full.stddev - 1.0).abs() < 0.02,
            "{sampled:?} against {full:?}"
        );
    }

    #[test]
    fn an_empty_frame_has_no_background() {
        assert!(sampled_background(&Array2::<i32>::zeros((0, 0)).view()).is_none());
    }
}
