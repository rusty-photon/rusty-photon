//! Pixel-level statistics (median, mean, min, max) used by `compute_image_stats`.
//!
//! Custom implementation on stdlib iterators + `select_nth_unstable` for
//! median (iterative O(n) quickselect, safe for arbitrarily large images).

/// Pixel-level statistics for an image.
#[derive(Debug, Clone)]
pub struct ImageStats {
    pub median_adu: u32,
    pub mean_adu: f64,
    pub min_adu: u32,
    pub max_adu: u32,
    pub pixel_count: u64,
}

/// Compute pixel statistics from a mutable slice of pixel values.
///
/// Takes `&mut [i32]` so the median's `select_nth_unstable` runs
/// in-place rather than over an internal clone — for large images
/// this avoids an extra `n × 4` bytes of peak allocation on top of
/// the cached pixel buffer. The caller's buffer is left in a
/// permuted (partially-ordered) state on return; min/max/mean are
/// read before the first partition so they reflect the original
/// pixel set regardless of ordering.
///
/// Returns `None` if the pixel slice is empty.
pub fn compute_stats(pixels: &mut [i32]) -> Option<ImageStats> {
    if pixels.is_empty() {
        return None;
    }

    let pixel_count = u64::try_from(pixels.len()).unwrap_or(u64::MAX);

    // Fold min/max/mean in a single pass before the partition mutates
    // the buffer; the values are order-independent so reading them
    // up front matches the previous (non-mutating) contract.
    let mut min = i32::MAX;
    let mut max = i32::MIN;
    let mut sum = 0.0_f64;
    for &p in pixels.iter() {
        if p < min {
            min = p;
        }
        if p > max {
            max = p;
        }
        sum += f64::from(p.max(0));
    }
    #[expect(
        clippy::as_conversions,
        reason = "pixel counts are exact below 2^53 in f64 and no total usize-to-f64 spelling exists"
    )]
    let mean_adu = sum / pixels.len() as f64;

    let mid = pixels.len() / 2;
    let median = if pixels.len().is_multiple_of(2) {
        let (_, &mut upper, _) = pixels.select_nth_unstable(mid);
        // Even length and the emptiness guard put `mid` in 1..=len-1.
        let (_, &mut lower, _) = pixels
            .get_mut(..mid)?
            .select_nth_unstable(mid.checked_sub(1)?);
        i32::midpoint(lower, upper)
    } else {
        let (_, &mut m, _) = pixels.select_nth_unstable(mid);
        m
    };

    // Clamp negatives to 0 across all four fields so the reported stats
    // are internally consistent. Production cameras today are u16-only;
    // negatives only arise on the i32 deferred-scientific-camera path
    // (over-subtraction during dark correction, electronic offsets), and
    // when they do, callers see a uniform "negatives become zero" rather
    // than a min/max/median of 0 alongside a negative mean.
    let clamp = |v: i32| -> u32 { v.max(0).cast_unsigned() };

    Some(ImageStats {
        median_adu: clamp(median),
        mean_adu,
        min_adu: clamp(min),
        max_adu: clamp(max),
        pixel_count,
    })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn compute_stats_odd_count() {
        let mut pixels = vec![10, 20, 30, 40, 50];
        let stats = compute_stats(&mut pixels).unwrap();
        assert_eq!(stats.median_adu, 30);
        assert_eq!(stats.min_adu, 10);
        assert_eq!(stats.max_adu, 50);
        assert_eq!(stats.pixel_count, 5);
        assert!((stats.mean_adu - 30.0).abs() < f64::EPSILON);
    }

    #[test]
    fn compute_stats_even_count() {
        let mut pixels = vec![10, 20, 30, 40];
        let stats = compute_stats(&mut pixels).unwrap();
        assert_eq!(stats.median_adu, 25);
        assert_eq!(stats.min_adu, 10);
        assert_eq!(stats.max_adu, 40);
        assert_eq!(stats.pixel_count, 4);
        assert!((stats.mean_adu - 25.0).abs() < f64::EPSILON);
    }

    #[test]
    fn compute_stats_single_pixel() {
        let mut pixels = vec![42];
        let stats = compute_stats(&mut pixels).unwrap();
        assert_eq!(stats.median_adu, 42);
        assert_eq!(stats.min_adu, 42);
        assert_eq!(stats.max_adu, 42);
        assert_eq!(stats.pixel_count, 1);
    }

    #[test]
    fn compute_stats_empty() {
        let mut pixels: Vec<i32> = vec![];
        assert!(compute_stats(&mut pixels).is_none());
    }

    #[test]
    fn compute_stats_all_same() {
        let mut pixels = vec![1000; 100];
        let stats = compute_stats(&mut pixels).unwrap();
        assert_eq!(stats.median_adu, 1000);
        assert_eq!(stats.min_adu, 1000);
        assert_eq!(stats.max_adu, 1000);
    }

    #[test]
    fn compute_stats_unsorted_input() {
        let mut pixels = vec![50, 10, 40, 20, 30];
        let stats = compute_stats(&mut pixels).unwrap();
        assert_eq!(stats.median_adu, 30);
        assert_eq!(stats.min_adu, 10);
        assert_eq!(stats.max_adu, 50);
    }

    #[test]
    fn compute_stats_large_values() {
        let mut pixels = vec![0, 32768, 65535];
        let stats = compute_stats(&mut pixels).unwrap();
        assert_eq!(stats.median_adu, 32768);
        assert_eq!(stats.min_adu, 0);
        assert_eq!(stats.max_adu, 65535);
    }

    #[test]
    fn compute_stats_clamps_negatives_uniformly() {
        // Pins the contract that all four reported fields treat negative
        // pixels as zero. Without uniform clamping, mean_adu could be
        // negative while min_adu/max_adu/median_adu sit at 0 (their u32
        // floor), producing internally inconsistent stats.
        let mut pixels = vec![-100i32, 0, 50];
        let stats = compute_stats(&mut pixels).unwrap();
        assert_eq!(stats.min_adu, 0);
        assert_eq!(stats.max_adu, 50);
        assert_eq!(stats.median_adu, 0);
        // mean = (clamp(-100) + 0 + 50) / 3 = 50/3
        assert!(
            (stats.mean_adu - 50.0 / 3.0).abs() < 1e-9,
            "expected clamped mean ≈ 16.667, got {}",
            stats.mean_adu
        );
    }
}
