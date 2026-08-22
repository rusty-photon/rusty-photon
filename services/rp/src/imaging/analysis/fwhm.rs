//! 2D Gaussian PSF fitting on a postage stamp around a star centroid.
//!
//! Model (no rotation, axis-aligned anisotropy):
//!
//! ```text
//! I(x, y) = A · exp(−((x − x0)² / (2σx²) + (y − y0)² / (2σy²))) + B
//! ```
//!
//! 6 parameters: amplitude `A`, centroid `(x0, y0)`, axis sigmas `(σx, σy)`,
//! background `B`. Solved with Levenberg-Marquardt via `rmpfit`.
//!
//! Why no rotation: most amateur astrophotography PSFs are sufficiently
//! axis-aligned that the rotation angle is poorly constrained by the fit
//! (poor data → spurious θ). Geometric-mean FWHM and (σmin/σmax)-derived
//! eccentricity capture image-quality degradation without it.
//!
//! HFR (half-flux radius) for a Gaussian has a closed form: HFR = σ ·
//! √(2 ln 2) ≈ 1.1774σ. This module exposes only the fit; the empirical
//! HFR (sum of background-subtracted flux out to half the total) lives in
//! `imaging/hfr.rs` and is preferred when the connected-component pixel
//! list is available (i.e. when stars came from `detect_stars`).

use ndarray::ArrayView2;
use rmpfit::{MPError, MPFitter, MPResult};

use super::pixel::Pixel;

/// FWHM = 2 · √(2 · ln 2) · σ for a Gaussian. ≈ 2.3548.
pub const FWHM_OVER_SIGMA: f64 = 2.354_820_045_030_949_4;

/// Result of a successful 2D Gaussian fit.
#[derive(Debug, Clone, Copy)]
pub struct GaussianFit2D {
    pub amplitude: f64,
    pub x0: f64,
    pub y0: f64,
    /// Axis-aligned σ along the first array axis ("x"). Always non-negative
    /// (the fit can produce a negative value due to the model's symmetry;
    /// we take the absolute value before returning).
    pub sigma_x: f64,
    /// Axis-aligned σ along the second array axis ("y"). Always non-negative.
    pub sigma_y: f64,
    pub background: f64,
    /// Geometric-mean FWHM = 2.3548 · √(σx · σy).
    pub fwhm: f64,
    /// √(1 − (σmin/σmax)²). `0.0` for circular PSFs, → 1.0 for very elongated.
    pub eccentricity: f64,
}

/// Fit a 2D Gaussian PSF on a square postage stamp centered on
/// `(centroid_x, centroid_y)`.
///
/// Returns `None` if the stamp would touch / fall off the image edge,
/// if the input view is too small, or if the fit fails to converge.
///
/// `initial_amplitude` should be the *raw* peak (e.g. `Star::peak`); the
/// initial above-background amplitude is `initial_amplitude − initial_background`.
/// `initial_sigma` seeds σx and σy (use the smoothing kernel σ from
/// detection — typically `1.0` — or a per-image seeing estimate).
#[must_use]
pub fn fit_2d_gaussian<T: Pixel>(
    view: &ArrayView2<T>,
    centroid_x: f64,
    centroid_y: f64,
    initial_amplitude: f64,
    initial_background: f64,
    initial_sigma: f64,
    stamp_half_size: usize,
) -> Option<GaussianFit2D> {
    let (rows, cols) = view.dim();
    if rows == 0 || cols == 0 || stamp_half_size == 0 {
        return None;
    }

    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        reason = "`f64` to `isize` has no total spelling; the cast saturates on an absurd centroid and the stamp-bounds check on the next lines rejects exactly those values"
    )]
    let (cxi, cyi) = (centroid_x.round() as isize, centroid_y.round() as isize);
    let h = stamp_half_size.cast_signed();
    // Saturating keeps a saturated-cast centroid out of wraparound; the
    // range check below rejects it either way.
    let r_min = cxi.saturating_sub(h);
    let c_min = cyi.saturating_sub(h);
    let r_max = cxi.saturating_add(h);
    let c_max = cyi.saturating_add(h);
    if r_min < 0 || c_min < 0 || r_max >= rows.cast_signed() || c_max >= cols.cast_signed() {
        return None;
    }

    let side = stamp_half_size.saturating_mul(2).saturating_add(1);
    let n = side.saturating_mul(side);
    let mut xs = Vec::with_capacity(n);
    let mut ys = Vec::with_capacity(n);
    let mut values = Vec::with_capacity(n);
    for r in r_min..=r_max {
        // In-range by the stamp-bounds check; coordinates fit u32 for
        // any in-memory image, and `?` folds the impossible overflow
        // into the existing no-fit result.
        let r_f = f64::from(u32::try_from(r).ok()?);
        for c in c_min..=c_max {
            xs.push(r_f);
            ys.push(f64::from(u32::try_from(c).ok()?));
            values.push(view.get((r.cast_unsigned(), c.cast_unsigned()))?.to_f64());
        }
    }

    let mut fitter = StampFitter { xs, ys, values };

    let above_bg = (initial_amplitude - initial_background).max(1.0);
    let sigma_seed = if initial_sigma > 0.0 {
        initial_sigma
    } else {
        1.5
    };
    let mut params = [
        above_bg,
        centroid_x,
        centroid_y,
        sigma_seed,
        sigma_seed,
        initial_background,
    ];

    fitter.mpfit(&mut params).ok()?;

    let amplitude = params[0];
    let x0 = params[1];
    let y0 = params[2];
    let sigma_x = params[3].abs();
    let sigma_y = params[4].abs();
    let background = params[5];

    if !sigma_x.is_finite() || !sigma_y.is_finite() || sigma_x == 0.0 || sigma_y == 0.0 {
        return None;
    }

    let fwhm = FWHM_OVER_SIGMA * (sigma_x * sigma_y).sqrt();
    let smin = sigma_x.min(sigma_y);
    let smax = sigma_x.max(sigma_y);
    // (1 − r)(1 + r) keeps the tiny 1 − r² of a near-circular star
    // from cancelling away, where the squared form loses it.
    let ratio = smin / smax;
    let eccentricity = ((1.0 - ratio) * (1.0 + ratio)).sqrt();

    Some(GaussianFit2D {
        amplitude,
        x0,
        y0,
        sigma_x,
        sigma_y,
        background,
        fwhm,
        eccentricity,
    })
}

struct StampFitter {
    xs: Vec<f64>,
    ys: Vec<f64>,
    values: Vec<f64>,
}

impl MPFitter for StampFitter {
    #[expect(
        clippy::suboptimal_flops,
        reason = "per-pixel model evaluation inside the fit loop: mul_add lowers to a software fma call on targets without hardware FMA, and a·exp(…) + b is the Gaussian model's shape"
    )]
    fn eval(&mut self, params: &[f64], deviates: &mut [f64]) -> MPResult<()> {
        // mpfit always passes the six parameters it was seeded with.
        let &[a, x0, y0, sx, sy, b] = params else {
            return Err(MPError::Eval);
        };
        let denom_x = 2.0 * sx * sx;
        let denom_y = 2.0 * sy * sy;
        for (((px, py), pv), dev) in self
            .xs
            .iter()
            .zip(self.ys.iter())
            .zip(self.values.iter())
            .zip(deviates.iter_mut())
        {
            let dx = px - x0;
            let dy = py - y0;
            let model = a * (-(dx * dx / denom_x + dy * dy / denom_y)).exp() + b;
            *dev = pv - model;
        }
        Ok(())
    }

    fn number_of_points(&self) -> usize {
        self.values.len()
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use ndarray::Array2;
    use std::f64::consts::E;

    #[allow(clippy::too_many_arguments)]
    fn make_gaussian_2d(
        rows: usize,
        cols: usize,
        cx: f64,
        cy: f64,
        sigma_x: f64,
        sigma_y: f64,
        amplitude: f64,
        background: f64,
    ) -> Array2<u16> {
        let mut arr = Array2::<u16>::zeros((rows, cols));
        for r in 0..rows {
            for c in 0..cols {
                let dx = r as f64 - cx;
                let dy = c as f64 - cy;
                let exponent =
                    -(dx * dx / (2.0 * sigma_x * sigma_x) + dy * dy / (2.0 * sigma_y * sigma_y));
                let v = background + amplitude * E.powf(exponent);
                arr[[r, c]] = v.round().clamp(0.0, 65535.0) as u16;
            }
        }
        arr
    }

    #[test]
    fn fits_circular_gaussian_recovers_sigma_and_centroid() {
        let arr = make_gaussian_2d(64, 64, 32.0, 32.0, 2.0, 2.0, 10_000.0, 1000.0);
        let fit = fit_2d_gaussian(&arr.view(), 32.0, 32.0, 11_000.0, 1000.0, 1.5, 8).unwrap();

        assert!((fit.x0 - 32.0).abs() < 0.05, "x0 = {}", fit.x0);
        assert!((fit.y0 - 32.0).abs() < 0.05, "y0 = {}", fit.y0);
        assert!((fit.sigma_x - 2.0).abs() < 0.1, "sigma_x = {}", fit.sigma_x);
        assert!((fit.sigma_y - 2.0).abs() < 0.1, "sigma_y = {}", fit.sigma_y);
        assert!(
            (fit.fwhm - FWHM_OVER_SIGMA * 2.0).abs() < 0.25,
            "fwhm = {}",
            fit.fwhm
        );
        assert!(
            fit.eccentricity < 0.05,
            "circular PSF eccentricity should be ~0, got {}",
            fit.eccentricity
        );
    }

    #[test]
    fn fits_elongated_gaussian_reports_eccentricity() {
        let arr = make_gaussian_2d(64, 64, 32.0, 32.0, 3.0, 1.5, 10_000.0, 1000.0);
        let fit = fit_2d_gaussian(&arr.view(), 32.0, 32.0, 11_000.0, 1000.0, 1.5, 12).unwrap();

        // sigmas can swap based on convergence; check both axes match the input set.
        let smax = fit.sigma_x.max(fit.sigma_y);
        let smin = fit.sigma_x.min(fit.sigma_y);
        assert!((smax - 3.0).abs() < 0.15, "smax = {smax}");
        assert!((smin - 1.5).abs() < 0.15, "smin = {smin}");

        // Eccentricity ≈ √(1 − (1.5/3.0)²) = √0.75 ≈ 0.866
        assert!(
            (fit.eccentricity - 0.866).abs() < 0.05,
            "eccentricity = {}",
            fit.eccentricity
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn make_gaussian_2d_i32(
        rows: usize,
        cols: usize,
        cx: f64,
        cy: f64,
        sigma_x: f64,
        sigma_y: f64,
        amplitude: f64,
        background: f64,
    ) -> Array2<i32> {
        let mut arr = Array2::<i32>::zeros((rows, cols));
        for r in 0..rows {
            for c in 0..cols {
                let dx = r as f64 - cx;
                let dy = c as f64 - cy;
                let exponent =
                    -(dx * dx / (2.0 * sigma_x * sigma_x) + dy * dy / (2.0 * sigma_y * sigma_y));
                arr[[r, c]] = (background + amplitude * E.powf(exponent)).round() as i32;
            }
        }
        arr
    }

    #[test]
    fn fits_circular_gaussian_in_hdr_i32_recovers_sigma() {
        // Amplitude > u16::MAX. A stray `as u16` cast in the residual eval
        // would make the fit converge to a wrong sigma.
        let arr = make_gaussian_2d_i32(64, 64, 32.0, 32.0, 2.0, 2.0, 200_000.0, 1000.0);
        let fit = fit_2d_gaussian(&arr.view(), 32.0, 32.0, 201_000.0, 1000.0, 1.5, 8).unwrap();
        assert!((fit.sigma_x - 2.0).abs() < 0.1, "sigma_x = {}", fit.sigma_x);
        assert!((fit.sigma_y - 2.0).abs() < 0.1, "sigma_y = {}", fit.sigma_y);
        assert!(
            fit.eccentricity < 0.05,
            "eccentricity = {}",
            fit.eccentricity
        );
    }

    #[test]
    fn rejects_centroid_too_close_to_edge() {
        let arr = make_gaussian_2d(20, 20, 2.0, 10.0, 1.5, 1.5, 10_000.0, 1000.0);
        // half_size = 8 means we need rows 2-h..=2+h = -6..=10 — impossible.
        assert!(fit_2d_gaussian(&arr.view(), 2.0, 10.0, 11_000.0, 1000.0, 1.5, 8).is_none());
    }

    #[test]
    fn rejects_zero_stamp() {
        let arr: Array2<u16> = Array2::from_elem((10, 10), 1000);
        assert!(fit_2d_gaussian(&arr.view(), 5.0, 5.0, 1500.0, 1000.0, 1.5, 0).is_none());
    }

    #[test]
    fn rejects_empty_view() {
        let arr: Array2<u16> = Array2::zeros((0, 0));
        assert!(fit_2d_gaussian(&arr.view(), 0.0, 0.0, 1.0, 0.0, 1.5, 4).is_none());
    }
}
