//! One-off generator for the `auto_focus` V-curve fixture FITS files.
//!
//! Renders 11 monochrome 200×200 frames containing a 3×3 grid of
//! synthetic Gaussian-PSF stars whose half-flux radius (HFR) traces a
//! parabola in `position`. The output goes under
//! `services/rp/tests/fixtures/auto_focus/` and is checked into the
//! repo — these are the canonical input frames for the `auto_focus`
//! happy-path test that exercises the pre-split `mcp.rs` test gap
//! covered in commit 0b67701's lines 156-176 of
//! `services/rp/src/mcp/built_in/auto_focus.rs`.
//!
//! The parabolic curve uses
//! `HFR(d) = 2.0 + 0.0005 · d²` for `d ∈ {-100, -80, …, +80, +100}`,
//! with d the focuser-position offset from the center (assumed to be
//! the operator-supplied `step_size` × index — the test wires
//! `starting_position` so the sweep grid lands on these eleven offsets
//! exactly).
//!
//! Star peak: 5000 ADU. Background: 100 ADU + Gaussian noise σ=5 ADU
//! (deterministic xorshift+Box-Muller seeded per-fixture so re-runs
//! produce byte-identical FITS). The noise is essential — without it
//! `sigma_clipped_stddev` would be ~0 and `measure_basic`'s detection
//! threshold would collapse to background, causing the full Gaussian
//! halos at large σ to merge into one connected component bigger
//! than the `max_area=400` cap. With σ=5 ADU noise, the threshold
//! sits at ~125 ADU and stars are detected by their bright cores
//! (well within the 4–400 area band) at every sweep position.
//!
//! Run: `cargo run --example gen_autofocus_fixtures -p rp`

// Fixture-generation math: quantization casts are the point, and the flops
// shapes mirror the analysis code the fixtures feed.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::suboptimal_flops
)]

use std::path::PathBuf;

use rp::persistence::write_fits_u16;

const WIDTH: u32 = 200;
const HEIGHT: u32 = 200;
const BACKGROUND: f64 = 100.0;
const NOISE_STDDEV: f64 = 5.0;
const PEAK: f64 = 5000.0;
const HFR_TO_SIGMA: f64 = 1.1774_f64; // HFR ≈ sigma · sqrt(2·ln 2)

/// Tiny xorshift64 PRNG. Deterministic, `no_std`, no external deps.
struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    const fn new(seed: u64) -> Self {
        // xorshift64 fails on a zero seed; the splat avoids that for
        // any plausible input.
        Self {
            state: seed.wrapping_add(0x9E37_79B9_7F4A_7C15),
        }
    }
    const fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }
    fn uniform_open(&mut self) -> f64 {
        // Map to (0, 1] — Box-Muller needs strictly-positive (`u1.ln()`
        // would diverge to `-∞` on a 0.0 sample). The previous
        // `next_u64() | 1` formulation set the lowest bit pre-shift;
        // the `>> 11` then discarded that bit, so the 53-bit mantissa
        // could still land at zero when the top 53 bits of the raw
        // u64 happened to be zero (probability ≈ 2.2e-16, never
        // observed in fixture generation but real). The OR-with-1
        // moved to AFTER the shift forces the 53-bit integer to be
        // at least 1, putting the result strictly in [2⁻⁵³, 1].
        let bits = (self.next_u64() >> 11) | 1;
        #[expect(
            clippy::as_conversions,
            reason = "the shifted 53-bit integer and 2^53 are both exactly representable in f64"
        )]
        let sample = bits as f64 / ((1u64 << 53) as f64);
        sample
    }
    /// Standard Normal sample via Box-Muller. One per call (we
    /// throw the second of the pair away — fine for fixture use).
    fn standard_normal(&mut self) -> f64 {
        let u1 = self.uniform_open();
        let u2 = self.uniform_open();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }
}

/// 3×3 star grid on a 200×200 frame, 60 px spacing. With
/// `NOISE_STDDEV=5` ADU the detection threshold sits ~5σ above
/// background ≈ 125 ADU, so the "above threshold" radius of a
/// star is ~3.3·σ. At the largest sweep HFR (≈7 px → σ≈5.9 px)
/// that radius is ~19 px, leaving a ~22 px gap between adjacent
/// stars — comfortably non-overlapping. The corresponding
/// connected-component area runs to ~1200 px², so the
/// fixture-check / BDD step uses `max_area=2000` (which matches
/// the rp.md `auto_focus` contract guidance: "donut-shaped PSFs from
/// the secondary obstruction can span many hundreds of pixels —
/// auto-focus callers should set `max_area` accordingly").
const STAR_X: [u32; 3] = [40, 100, 160];
const STAR_Y: [u32; 3] = [40, 100, 160];

/// Eleven sweep offsets (focuser units). Centered on 0, step 20.
/// Maps `d → HFR = 2.0 + 0.0005 · d²` so the V-curve is symmetric
/// with minimum HFR=2.0 px at d=0 and maximum HFR≈7.0 px at d=±100.
const OFFSETS: [i32; 11] = [-100, -80, -60, -40, -20, 0, 20, 40, 60, 80, 100];

fn hfr_for_offset(d: i32) -> f64 {
    2.0 + 0.0005 * f64::from(d).powi(2)
}

/// Render one synthetic frame at the given HFR with deterministic
/// per-fixture noise.
///
/// Returns a flat row-major `u16` pixel buffer of `WIDTH * HEIGHT`
/// pixels: `BACKGROUND` + Gaussian noise (σ = `NOISE_STDDEV` ADU)
/// on every pixel, then a 3×3 grid of Gaussian-PSF stars on top.
/// `seed` is mixed into the xorshift state so each fixture's noise
/// pattern is independent but reproducible.
#[expect(
    clippy::as_conversions,
    reason = "fixture-generator conversions: the f64 narrowings are pre-clamped (or, for the stamp radius, bounded by the fixture HFR range) before the saturating `as`, and the WIDTH*HEIGHT product is a u32-to-usize widening"
)]
fn render(hfr: f64, seed: u64) -> Vec<u16> {
    let sigma = hfr / HFR_TO_SIGMA;
    let two_sigma_sq = 2.0 * sigma * sigma;
    let n = (WIDTH * HEIGHT) as usize;
    let mut buf = vec![0u16; n];
    let mut rng = XorShift64::new(seed);

    // Background floor + noise — every pixel.
    for v in &mut buf {
        let val = (BACKGROUND + NOISE_STDDEV * rng.standard_normal()).round();
        *v = val.clamp(0.0, f64::from(u16::MAX)) as u16;
    }

    // Iterate over the local neighbourhood of each star instead of
    // the full image. radius = 6σ covers >0.999 of the integrated
    // flux; outside that the contribution rounds to zero anyway.
    let radius = (6.0 * sigma).ceil() as i32;
    for &cx in &STAR_X {
        for &cy in &STAR_Y {
            for dy in radius.saturating_neg()..=radius {
                let y = cy.cast_signed().saturating_add(dy);
                if y < 0 || y >= HEIGHT.cast_signed() {
                    continue;
                }
                for dx in radius.saturating_neg()..=radius {
                    let x = cx.cast_signed().saturating_add(dx);
                    if x < 0 || x >= WIDTH.cast_signed() {
                        continue;
                    }
                    let r2 = f64::from(dx.saturating_mul(dx).saturating_add(dy.saturating_mul(dy)));
                    let amp = PEAK * (-r2 / two_sigma_sq).exp();
                    // In bounds: 0 <= y < HEIGHT and 0 <= x < WIDTH by the
                    // guards above.
                    let idx = usize::try_from(y)
                        .unwrap_or(0)
                        .saturating_mul(usize::try_from(WIDTH).unwrap_or(0))
                        .saturating_add(usize::try_from(x).unwrap_or(0));
                    let Some(pixel) = buf.get_mut(idx) else {
                        continue;
                    };
                    let bg_with_noise = f64::from(*pixel);
                    let v = (bg_with_noise + amp)
                        .round()
                        .clamp(0.0, f64::from(u16::MAX)) as u16;
                    if v > *pixel {
                        *pixel = v;
                    }
                }
            }
        }
    }
    buf
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The example is intended to run from the repo root; resolve
    // relative to the rp service crate so a `cargo run --example`
    // from anywhere in the workspace lands the files in the right
    // place.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = manifest_dir
        .join("tests")
        .join("fixtures")
        .join("auto_focus");
    std::fs::create_dir_all(&out_dir)?;

    println!(
        "writing {} fixtures to {}",
        OFFSETS.len(),
        out_dir.display()
    );

    for &d in &OFFSETS {
        let hfr = hfr_for_offset(d);
        // Seed depends on d so each fixture has its own deterministic
        // noise pattern; offset-by-200 keeps |d|=0 from collapsing to
        // an all-zeros seed.
        let seed = 0xCAFE_BABE_DEAD_BEEF_u64
            .wrapping_add(i64::from(d).cast_unsigned())
            .wrapping_add(200);
        let pixels = render(hfr, seed);

        // Filename convention: pos_<sign><abs>.fits where `sign` is `m`
        // for negative offsets (Linux paths can't use `-` in numeric
        // ranges without confusion, and `m`/`p` makes lexicographic
        // sort match numeric sort).
        let name = if d < 0 {
            format!("pos_m{:03}.fits", d.unsigned_abs())
        } else {
            format!("pos_p{:03}.fits", d.cast_unsigned())
        };
        let path = out_dir.join(&name);

        // The DOC_ID is fixture-specific but stable so re-rendering
        // produces byte-identical files.
        let doc_id = format!("autofocus-fixture-{d:+04}");

        write_fits_u16(
            &path,
            &pixels,
            usize::try_from(WIDTH)?,
            usize::try_from(HEIGHT)?,
            &doc_id,
        )
        .await?;
        println!("  {}  HFR={:.3} px  → {}", name, hfr, path.display());
    }

    Ok(())
}
