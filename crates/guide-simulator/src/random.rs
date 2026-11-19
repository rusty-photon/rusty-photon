//! The random draws and the special function the models need.

use rand::rngs::Xoshiro256PlusPlus;
use rand::{RngExt, SeedableRng};
use std::f64::consts::TAU;

/// A random stream of its own, with the spare deviate the Box–Muller
/// transform yields beside each one it is asked for.
#[derive(Debug, Clone)]
pub struct Stream {
    rng: Xoshiro256PlusPlus,
    spare_normal: Option<f64>,
}

impl Stream {
    /// The stream numbered `index` of the run seeded with `seed`. The
    /// index is hashed into the seed: `seed_from_u64` steps its state by
    /// the golden ratio, so seeds a multiple of it apart would start
    /// from shifted copies of one state.
    pub(crate) fn new(seed: u64, index: u64) -> Self {
        Self {
            rng: Xoshiro256PlusPlus::seed_from_u64(seed ^ mix(index.wrapping_add(GOLDEN))),
            spare_normal: None,
        }
    }

    /// A uniform deviate in `[0, 1)`.
    pub(crate) fn uniform(&mut self) -> f64 {
        self.rng.random::<f64>()
    }

    /// A uniform index in `0..len`; 0 when `len` is 0.
    pub(crate) fn index(&mut self, len: usize) -> usize {
        if len == 0 {
            0
        } else {
            self.rng.random_range(0..len)
        }
    }

    /// A standard normal deviate.
    pub(crate) fn normal(&mut self) -> f64 {
        if let Some(spare) = self.spare_normal.take() {
            return spare;
        }
        // 1 − u lies in (0, 1], so its logarithm is finite.
        let radius = (-2.0 * (1.0 - self.uniform()).ln()).sqrt();
        let angle = TAU * self.uniform();
        self.spare_normal = Some(radius * angle.sin());
        radius * angle.cos()
    }

    /// A Poisson deviate of mean `mean`, by Knuth's product of uniforms:
    /// as many draws as the mean, so only for small means.
    pub(crate) fn poisson(&mut self, mean: f64) -> f64 {
        let limit = (-mean).exp();
        let mut count = 0.0;
        let mut product = self.uniform();
        loop {
            if product <= limit {
                return count;
            }
            count += 1.0;
            product *= self.uniform();
        }
    }
}

/// The golden ratio's fraction of 2^64, `SplitMix64`'s increment.
const GOLDEN: u64 = 0x9E37_79B9_7F4A_7C15;

/// `SplitMix64`'s output function, which spreads `value`'s bits over the
/// whole word.
const fn mix(value: u64) -> u64 {
    let z = (value ^ value.wrapping_shr(30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    let z = (z ^ z.wrapping_shr(27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ z.wrapping_shr(31)
}

/// The error function, by Abramowitz and Stegun 7.1.26: within 1.5e-7
/// of the true value everywhere, and odd, so a star's image is
/// symmetric about its centre.
pub fn erf(x: f64) -> f64 {
    let t = 1.0 / 0.327_591_1_f64.mul_add(x.abs(), 1.0);
    let poly = t * t.mul_add(
        t.mul_add(
            t.mul_add(t.mul_add(1.061_405_429, -1.453_152_027), 1.421_413_741),
            -0.284_496_736,
        ),
        0.254_829_592,
    );
    (-poly).mul_add((-x * x).exp(), 1.0).copysign(x)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn erf_matches_tabulated_values() {
        for (x, expected) in [
            (0.0, 0.0),
            (0.5, 0.520_499_877_8),
            (1.0, 0.842_700_792_9),
            (2.0, 0.995_322_265_0),
            (3.0, 0.999_977_909_5),
        ] {
            assert!((erf(x) - expected).abs() < 2e-7, "erf({x})");
            assert!((erf(-x) + expected).abs() < 2e-7, "erf(-{x})");
        }
    }

    #[test]
    fn the_same_seed_and_index_give_the_same_draws() {
        let mut a = Stream::new(7, 1);
        let mut b = Stream::new(7, 1);
        assert_eq!(a.uniform(), b.uniform());
        assert_eq!(a.normal(), b.normal());
    }

    #[test]
    fn another_index_gives_other_draws() {
        assert_ne!(Stream::new(7, 1).uniform(), Stream::new(7, 2).uniform());
    }

    #[test]
    fn a_stream_is_not_another_seed_s_stream_shifted_by_the_index() {
        let seed = 7_u64;
        let mut next = Stream::new(seed, 1);
        let mut shifted = Stream::new(seed.wrapping_add(GOLDEN), 0);
        let draws = |stream: &mut Stream| (0..4).map(|_| stream.uniform()).collect::<Vec<_>>();
        assert_ne!(draws(&mut next), draws(&mut shifted));
    }

    #[test]
    fn normal_deviates_have_zero_mean_and_unit_spread() {
        let mut stream = Stream::new(1, 0);
        let draws: Vec<f64> = (0..20_000).map(|_| stream.normal()).collect();
        let mean = draws.iter().sum::<f64>() / draws.len() as f64;
        let var = draws.iter().map(|d| (d - mean).powi(2)).sum::<f64>() / draws.len() as f64;
        assert!(mean.abs() < 0.03, "mean {mean}");
        assert!((var - 1.0).abs() < 0.04, "variance {var}");
    }

    #[test]
    fn poisson_deviates_have_their_mean_as_mean_and_variance() {
        let mut stream = Stream::new(2, 0);
        let draws: Vec<f64> = (0..20_000).map(|_| stream.poisson(4.0)).collect();
        let mean = draws.iter().sum::<f64>() / draws.len() as f64;
        let var = draws.iter().map(|d| (d - mean).powi(2)).sum::<f64>() / draws.len() as f64;
        assert!((mean - 4.0).abs() < 0.06, "mean {mean}");
        assert!((var - 4.0).abs() < 0.2, "variance {var}");
        assert!(draws.iter().all(|d| d.fract() == 0.0 && *d >= 0.0));
    }

    #[test]
    fn a_poisson_mean_of_zero_draws_zero() {
        assert_eq!(Stream::new(3, 0).poisson(0.0), 0.0);
    }

    #[test]
    fn index_stays_below_its_bound() {
        let mut stream = Stream::new(4, 0);
        assert!((0..1000).all(|_| stream.index(5) < 5));
        assert_eq!(stream.index(0), 0);
    }
}
