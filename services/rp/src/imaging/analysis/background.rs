//! Sigma-clipped background statistics.
//!
//! Iteratively rejects pixels more than `k * stddev` from the running mean
//! until the surviving pixel set stops shrinking (or `max_iters` runs out).
//! The final mean and stddev are robust against bright stars and hot pixels;
//! the median is computed via `select_nth_unstable` on the surviving set.
//!
//! Used by `measure_basic` and (Phase 5) `estimate_background`. See
//! `docs/services/rp.md` (MVP `measure_basic` Contract, algorithm step 2).

use ndarray::ArrayView2;

use super::pixel::Pixel;

/// Result of a sigma-clipped statistics pass.
#[derive(Debug, Clone, Copy)]
pub struct BackgroundStats {
    pub mean: f64,
    pub stddev: f64,
    pub median: f64,
    pub n_pixels: u64,
}

/// Iterative sigma-clip with caller-controlled `k` and iteration cap.
///
/// Returns `None` if the input view is empty or all pixels are clipped away.
#[must_use]
#[expect(
    clippy::suboptimal_flops,
    reason = "mean ± k·stddev is the sigma-clip bound in its plain shape; fusing hides it for no observable gain"
)]
pub fn sigma_clipped_stats<T: Pixel>(
    view: &ArrayView2<T>,
    k: f64,
    max_iters: usize,
) -> Option<BackgroundStats> {
    let mut values: Vec<f64> = view.iter().map(|p| p.to_f64()).collect();
    if values.is_empty() {
        return None;
    }

    let (mut mean, mut stddev) = mean_and_stddev(&values);

    for _ in 0..max_iters {
        let before = values.len();
        let lo = mean - k * stddev;
        let hi = mean + k * stddev;
        values.retain(|&v| v >= lo && v <= hi);
        if values.is_empty() {
            return None;
        }
        let (m, s) = mean_and_stddev(&values);
        mean = m;
        stddev = s;
        if values.len() == before {
            break;
        }
    }

    let median = median_of(&mut values);
    Some(BackgroundStats {
        mean,
        stddev,
        median,
        n_pixels: u64::try_from(values.len()).unwrap_or(u64::MAX),
    })
}

/// Convenience: `k = 3.0`, `max_iters = 5`.
#[must_use]
pub fn estimate_background<T: Pixel>(view: &ArrayView2<T>) -> Option<BackgroundStats> {
    sigma_clipped_stats(view, 3.0, 5)
}

fn mean_and_stddev(values: &[f64]) -> (f64, f64) {
    #[expect(
        clippy::as_conversions,
        clippy::cast_precision_loss,
        reason = "pixel counts are exact below 2^53 in f64 and no total usize-to-f64 spelling exists"
    )]
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    let variance = values
        .iter()
        .map(|v| {
            let d = v - mean;
            d * d
        })
        .sum::<f64>()
        / n;
    (mean, variance.sqrt())
}

fn median_of(values: &mut [f64]) -> f64 {
    let n = values.len();
    if n == 0 {
        // Callers guarantee non-empty; NaN poisons the stats
        // conspicuously instead of panicking in select_nth_unstable_by.
        return f64::NAN;
    }
    let mid = n / 2;
    values.select_nth_unstable_by(mid, |a, b| {
        a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
    });
    // In bounds: `mid < n` for the non-empty slice checked above.
    let upper = values.get(mid).copied().unwrap_or(f64::NAN);
    if n % 2 == 1 {
        upper
    } else {
        let lower = values
            .get(..mid)
            .unwrap_or_default()
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max);
        f64::midpoint(lower, upper)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use ndarray::Array2;

    #[test]
    fn constant_image_stats_are_exact() {
        let arr: Array2<u16> = Array2::from_elem((100, 100), 1000);
        let stats = estimate_background(&arr.view()).unwrap();
        assert_eq!(stats.mean, 1000.0);
        assert_eq!(stats.stddev, 0.0);
        assert_eq!(stats.median, 1000.0);
        assert_eq!(stats.n_pixels, 10_000);
    }

    #[test]
    fn rejects_bright_outliers() {
        let mut arr: Array2<u16> = Array2::from_elem((100, 100), 100);
        arr[[0, 0]] = 60_000;
        arr[[1, 1]] = 60_000;
        arr[[2, 2]] = 60_000;
        arr[[3, 3]] = 60_000;
        arr[[4, 4]] = 60_000;
        let stats = estimate_background(&arr.view()).unwrap();
        assert!(
            (stats.mean - 100.0).abs() < 1e-9,
            "outliers should be clipped, got mean = {}",
            stats.mean
        );
        assert_eq!(stats.median, 100.0);
        assert!(stats.n_pixels < 10_000);
        assert!(stats.n_pixels >= 9_995);
    }

    #[test]
    fn empty_view_returns_none() {
        let arr: Array2<u16> = Array2::zeros((0, 0));
        assert!(estimate_background(&arr.view()).is_none());
    }

    #[test]
    fn known_gaussian_recovers_mean_and_stddev() {
        // Deterministic noise: write a fixed pattern of values that has a known
        // mean and stddev so the test is reproducible without an RNG dependency.
        // Use two-point distribution at mean ± stddev — gives mean=mu, stddev=sigma exactly.
        let mu = 1000.0_f64;
        let sigma = 10.0_f64;
        let mut data: Vec<u16> = Vec::with_capacity(10_000);
        for i in 0..10_000_u32 {
            let v = if i % 2 == 0 { mu - sigma } else { mu + sigma };
            data.push(v as u16);
        }
        let arr = Array2::from_shape_vec((100, 100), data).unwrap();
        let stats = estimate_background(&arr.view()).unwrap();
        assert!((stats.mean - mu).abs() < 0.5, "mean = {}", stats.mean);
        assert!(
            (stats.stddev - sigma).abs() < 0.5,
            "stddev = {}",
            stats.stddev
        );
    }

    #[test]
    fn median_of_constant_set() {
        let arr: Array2<u16> = Array2::from_elem((10, 10), 42);
        let stats = estimate_background(&arr.view()).unwrap();
        assert_eq!(stats.median, 42.0);
    }

    #[test]
    fn i32_pixels_round_trip() {
        let arr: Array2<i32> = Array2::from_elem((20, 20), 100_000);
        let stats = estimate_background(&arr.view()).unwrap();
        assert_eq!(stats.mean, 100_000.0);
        assert_eq!(stats.stddev, 0.0);
        assert_eq!(stats.median, 100_000.0);
    }

    #[test]
    fn caller_controlled_k_tightens_clip() {
        // Two-bin distribution with one extreme → tighter k should reject it,
        // looser k should keep it.
        let mut arr: Array2<u16> = Array2::from_elem((10, 10), 100);
        arr[[0, 0]] = 200;
        let tight = sigma_clipped_stats(&arr.view(), 1.0, 5).unwrap();
        let loose = sigma_clipped_stats(&arr.view(), 100.0, 5).unwrap();
        assert!(tight.n_pixels < loose.n_pixels);
        assert_eq!(loose.n_pixels, 100);
    }
}
