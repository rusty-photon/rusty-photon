//! Sizing the sweep from the optics
//! (docs/services/focus-model.md § Sweep sizing).
//!
//! The critical focus zone of the optics at the filter's wavelength
//! sets the step; the geometric growth of a defocused star sets the
//! half width, so the sweep ends at a known multiple of the focused
//! HFR. Pure arithmetic over the facts `rp`'s `get_train_info` reports
//! — nothing here reads a device.

use serde::{Deserialize, Serialize};

use crate::config::{SweepConfig, TrainConfig};
use crate::error::{FocusModelError, Result};

/// The half-flux radius of a uniform blur disc, as a fraction of its
/// diameter.
///
/// Unobstructed aperture; a central obstruction pushes the flux to the
/// rim and the constant toward 0.5, which the measured wing slope of
/// each run is the check on.
pub const BLUR_CONSTANT: f64 = 0.35;

/// The classical critical-focus-zone coefficient: `CFZ = 4.88 λ N²`.
pub const CFZ_COEFFICIENT: f64 = 4.88;

/// The wavelength a filter without one — and a filterless train — is
/// sized at, in nanometres.
pub const DEFAULT_WAVELENGTH_NM: f64 = 550.0;

/// The `optics` block of `rp`'s `get_train_info`: every fact null when
/// unknown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize, Serialize)]
pub struct Optics {
    #[serde(default)]
    pub focal_length_mm: Option<f64>,
    #[serde(default)]
    pub aperture_mm: Option<f64>,
    #[serde(default)]
    pub focal_ratio: Option<f64>,
    #[serde(default)]
    pub pixel_size_um: Option<f64>,
    #[serde(default)]
    pub pixel_scale_arcsec_per_pixel: Option<f64>,
    #[serde(default)]
    pub microns_per_step: Option<f64>,
}

impl Optics {
    /// A fact, when it is a usable positive number.
    fn positive(value: Option<f64>) -> Option<f64> {
        value.filter(|v| v.is_finite() && *v > 0.0)
    }
}

/// Where a sweep's two numbers came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SweepSource {
    /// Both derived from the optics.
    Derived,
    /// Both from the train's config block.
    Configured,
    /// One configured, the other derived.
    Mixed,
}

/// The sweep a run will walk, and the numbers behind it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct SweepPlan {
    pub step_size: i32,
    pub half_width: i32,
    /// The positions the grid holds at this step and half width,
    /// before the focuser's bounds clamp anything.
    pub points: u32,
    pub source: SweepSource,
    pub end_ratio: f64,
    /// The wavelength the critical focus zone was computed at.
    pub wavelength_nm: f64,
    /// The critical focus zone in focuser steps; null when the optics
    /// could not supply it.
    pub cfz_steps: Option<f64>,
    /// The focused HFR the half width was sized from, in pixels.
    pub hfr_focus: Option<f64>,
    /// The HFR growth the geometry predicts, in pixels per 100 steps.
    pub predicted_slope: Option<f64>,
}

/// What a train's sweep needs and the optics did not supply.
fn missing(train_id: &str, fact: &str) -> FocusModelError {
    FocusModelError::Workflow(format!(
        "train '{train_id}' has no derived sweep: {fact} is unknown; set \
         trains.{train_id}.step_size and half_width in focus-model.json or the fact in rp's config"
    ))
}

/// `value.ceil()` as a step count of at least 1.
fn ceil_steps(value: f64) -> i32 {
    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        reason = "`f64` to `i32` has no total spelling; `as` saturates at the rails and maps NaN to 0, and the max below lifts both into the valid range"
    )]
    let steps = value.ceil() as i32;
    steps.max(1)
}

/// The focal ratio: the reported one, else focal length over aperture.
#[must_use]
pub fn focal_ratio(optics: &Optics) -> Option<f64> {
    if let Some(ratio) = Optics::positive(optics.focal_ratio) {
        return Some(ratio);
    }
    let focal_length = Optics::positive(optics.focal_length_mm)?;
    let aperture = Optics::positive(optics.aperture_mm)?;
    Some(focal_length / aperture)
}

/// The critical focus zone in microns at `wavelength_nm` and focal
/// ratio `n`.
#[must_use]
pub fn critical_focus_zone_um(wavelength_nm: f64, n: f64) -> f64 {
    CFZ_COEFFICIENT * (wavelength_nm / 1000.0) * n * n
}

/// The HFR growth the blur geometry predicts, in pixels per focuser
/// step.
#[must_use]
pub fn predicted_slope_per_step(microns_per_step: f64, n: f64, pixel_size_um: f64) -> f64 {
    BLUR_CONSTANT * microns_per_step / (n * pixel_size_um)
}

/// Derive the sweep for one train.
///
/// `filter_wavelength_nm` is the filter's configured wavelength, and
/// `hfr_focus` the filter's last good HFR when the record has one;
/// without it the focused HFR comes from `sweep.seeing_fwhm_arcsec`
/// and the pixel scale. A train whose config block sets both
/// `step_size` and `half_width` needs no optics at all.
///
/// # Errors
///
/// Returns [`FocusModelError::Workflow`] naming the first fact the
/// derivation needs and the optics do not carry.
pub fn plan_sweep(
    train_id: &str,
    optics: &Optics,
    train: &TrainConfig,
    sweep: &SweepConfig,
    filter_wavelength_nm: Option<f64>,
    hfr_focus: Option<f64>,
) -> Result<SweepPlan> {
    let wavelength_nm = filter_wavelength_nm
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(DEFAULT_WAVELENGTH_NM);
    let configured_step = train.step_size.map(crate::config::Steps::get);
    let configured_half = train.half_width.map(crate::config::Steps::get);
    let source = match (configured_step, configured_half) {
        (Some(_), Some(_)) => SweepSource::Configured,
        (None, None) => SweepSource::Derived,
        _ => SweepSource::Mixed,
    };

    // The optics-derived numbers, computed whenever the facts allow it
    // so `get_sweep_plan` can report them beside a configured sweep.
    let n = focal_ratio(optics);
    let microns_per_step = Optics::positive(optics.microns_per_step);
    let pixel_size_um = Optics::positive(optics.pixel_size_um);
    // Each derivation is filtered as well as its inputs: a focal ratio
    // finite on its own can square to infinity, and a sweep sized from
    // that would report one point rather than name the geometry.
    let cfz_steps = match (n, microns_per_step) {
        (Some(n), Some(microns)) => Some(critical_focus_zone_um(wavelength_nm, n) / microns),
        _ => None,
    }
    .filter(|steps| steps.is_finite() && *steps > 0.0);
    let slope_per_step = match (n, microns_per_step, pixel_size_um) {
        (Some(n), Some(microns), Some(pixel)) => Some(predicted_slope_per_step(microns, n, pixel)),
        _ => None,
    }
    .filter(|slope| slope.is_finite() && *slope > 0.0);
    let focused_hfr = hfr_focus.filter(|v| v.is_finite() && *v > 0.0).or_else(|| {
        Optics::positive(optics.pixel_scale_arcsec_per_pixel)
            .map(|scale| 0.5 * sweep.seeing_fwhm_arcsec.get() / scale)
    });

    let derived_half_width = match (focused_hfr, slope_per_step) {
        (Some(hfr), Some(slope)) if slope > 0.0 => {
            let end_ratio = sweep.end_ratio.get();
            Some(ceil_steps(
                hfr * end_ratio.mul_add(end_ratio, -1.0).sqrt() / slope,
            ))
        }
        _ => None,
    };

    let half_width = match configured_half {
        Some(configured) => configured,
        None => derived_half_width
            .ok_or_else(|| missing(train_id, first_missing_fact(optics, focused_hfr.is_some())))?,
    };
    let step_size = match configured_step {
        Some(configured) => configured,
        None => derived_step(
            half_width,
            sweep.points.get(),
            cfz_steps.ok_or_else(|| {
                missing(train_id, first_missing_fact(optics, focused_hfr.is_some()))
            })?,
        ),
    };

    Ok(SweepPlan {
        step_size,
        half_width,
        // What the grid holds at this step, which the critical focus
        // zone's floor on the step can make fewer than the `points`
        // asked for.
        points: u32::try_from(crate::sweep::planned_points(half_width, step_size))
            .unwrap_or(u32::MAX),
        source,
        end_ratio: sweep.end_ratio.get(),
        wavelength_nm,
        cfz_steps,
        hfr_focus: focused_hfr,
        predicted_slope: slope_per_step.map(|slope| slope * 100.0),
    })
}

/// The step that gives `points` samples across the sweep, floored at
/// half a critical focus zone: samples closer together than that
/// measure the same focus.
fn derived_step(half_width: i32, points: u32, cfz_steps: f64) -> i32 {
    let spread = f64::from(half_width.saturating_mul(2));
    let divisor = f64::from(points.saturating_sub(1).max(1));
    ceil_steps(spread / divisor).max(ceil_steps(cfz_steps / 2.0))
}

/// The fact to name when a derivation could not run, in the order the
/// formulas need them.
fn first_missing_fact(optics: &Optics, have_focused_hfr: bool) -> &'static str {
    if Optics::positive(optics.focal_ratio).is_none() {
        if Optics::positive(optics.focal_length_mm).is_none() {
            return "focal_length_mm";
        }
        if Optics::positive(optics.aperture_mm).is_none() {
            return "aperture_mm";
        }
    }
    if Optics::positive(optics.microns_per_step).is_none() {
        return "microns_per_step";
    }
    if Optics::positive(optics.pixel_size_um).is_none() {
        return "pixel_size_um";
    }
    if !have_focused_hfr {
        return "pixel_scale_arcsec_per_pixel";
    }
    "the optics"
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::config::Steps;

    /// The reference rig of the BDD suite: a 500 mm f/5 train with a
    /// 5.6 µm camera and a 2.5 µm/step focuser.
    fn reference_optics() -> Optics {
        Optics {
            focal_length_mm: Some(500.0),
            aperture_mm: Some(100.0),
            focal_ratio: Some(5.0),
            pixel_size_um: Some(5.6),
            pixel_scale_arcsec_per_pixel: Some(206.265 * 5.6 / 500.0),
            microns_per_step: Some(2.5),
        }
    }

    fn train() -> TrainConfig {
        TrainConfig::default()
    }

    fn sweep() -> SweepConfig {
        SweepConfig::default()
    }

    #[test]
    fn the_reference_rig_derives_the_documented_sweep() {
        let plan = plan_sweep("main", &reference_optics(), &train(), &sweep(), None, None).unwrap();
        assert_eq!(plan.source, SweepSource::Derived);
        assert_eq!(plan.half_width, 68);
        assert_eq!(plan.step_size, 17);
        assert_eq!(plan.points, 9, "68 * 2 / 17 + 1");
        assert_eq!(plan.wavelength_nm, 550.0);
        assert!(
            (plan.cfz_steps.unwrap() - 26.84).abs() < 0.01,
            "{:?}",
            plan.cfz_steps
        );
        assert!(
            (plan.predicted_slope.unwrap() - 3.125).abs() < 1e-9,
            "{:?}",
            plan.predicted_slope
        );
    }

    #[test]
    fn a_narrowband_filter_widens_the_critical_focus_zone() {
        let plan = plan_sweep(
            "main",
            &reference_optics(),
            &train(),
            &sweep(),
            Some(656.0),
            None,
        )
        .unwrap();
        assert_eq!(plan.wavelength_nm, 656.0);
        assert!(
            (plan.cfz_steps.unwrap() - 32.0128).abs() < 0.001,
            "{:?}",
            plan.cfz_steps
        );
        // Still 17: the half-width term dominates the CFZ floor here.
        assert_eq!(plan.step_size, 17);
    }

    #[test]
    fn a_filter_without_a_wavelength_is_sized_at_550_nm() {
        let plan = plan_sweep("main", &reference_optics(), &train(), &sweep(), None, None).unwrap();
        let explicit = plan_sweep(
            "main",
            &reference_optics(),
            &train(),
            &sweep(),
            Some(550.0),
            None,
        )
        .unwrap();
        assert_eq!(plan, explicit);
    }

    /// Optics whose numbers are each finite but whose geometry is not:
    /// the sweep is refused naming a fact, not sized from infinity.
    #[test]
    fn a_derivation_that_overflows_is_no_derivation() {
        let optics = Optics {
            focal_ratio: Some(f64::MAX),
            ..reference_optics()
        };
        let err = plan_sweep("main", &optics, &train(), &sweep(), None, None).unwrap_err();
        assert!(err.tool_message().contains("has no derived sweep"), "{err}");
    }

    #[test]
    fn a_coarse_focuser_floors_the_step_at_half_a_critical_focus_zone() {
        // 20 µm/step: the geometry wants a 9-step half width, so the
        // even spread would be 3 steps — the CFZ floor lifts it.
        let optics = Optics {
            microns_per_step: Some(20.0),
            ..reference_optics()
        };
        let plan = plan_sweep("main", &optics, &train(), &sweep(), None, None).unwrap();
        assert_eq!(plan.half_width, 9);
        assert_eq!(plan.step_size, 3, "{plan:?}");
        assert_eq!(
            plan.points, 7,
            "the floor leaves seven positions, not the nine asked for"
        );
    }

    #[test]
    fn a_recorded_focused_hfr_replaces_the_seeing_stand_in() {
        let plan = plan_sweep(
            "main",
            &reference_optics(),
            &train(),
            &sweep(),
            None,
            Some(2.0),
        )
        .unwrap();
        assert_eq!(plan.hfr_focus, Some(2.0));
        // The half width scales with the focused HFR.
        assert_eq!(plan.half_width, 248);
    }

    #[test]
    fn both_overrides_configure_the_sweep_and_need_no_optics() {
        let train = TrainConfig {
            step_size: Some(Steps::try_from(30).unwrap()),
            half_width: Some(Steps::try_from(150).unwrap()),
            ..train()
        };
        let plan = plan_sweep("main", &Optics::default(), &train, &sweep(), None, None).unwrap();
        assert_eq!(plan.source, SweepSource::Configured);
        assert_eq!(plan.step_size, 30);
        assert_eq!(plan.half_width, 150);
        assert_eq!(plan.cfz_steps, None);
        assert_eq!(plan.predicted_slope, None);
    }

    #[test]
    fn one_override_is_a_mixed_sweep_derived_around_it() {
        let train = TrainConfig {
            half_width: Some(Steps::try_from(200).unwrap()),
            ..train()
        };
        let plan = plan_sweep("main", &reference_optics(), &train, &sweep(), None, None).unwrap();
        assert_eq!(plan.source, SweepSource::Mixed);
        assert_eq!(plan.half_width, 200);
        assert_eq!(plan.step_size, 50);
    }

    #[test]
    fn every_missing_fact_is_named_in_the_order_the_formulas_need_it() {
        let cases: [(Optics, &str); 4] = [
            (
                Optics {
                    focal_ratio: None,
                    focal_length_mm: None,
                    ..reference_optics()
                },
                "focal_length_mm",
            ),
            (
                Optics {
                    focal_ratio: None,
                    aperture_mm: None,
                    ..reference_optics()
                },
                "aperture_mm",
            ),
            (
                Optics {
                    microns_per_step: None,
                    ..reference_optics()
                },
                "microns_per_step",
            ),
            (
                Optics {
                    pixel_size_um: None,
                    ..reference_optics()
                },
                "pixel_size_um",
            ),
        ];
        for (optics, fact) in cases {
            let err = plan_sweep("main", &optics, &train(), &sweep(), None, None).unwrap_err();
            let message = err.to_string();
            assert!(message.contains(fact), "expected {fact} in {message}");
            assert!(message.contains("trains.main.step_size"), "{message}");
        }
    }

    #[test]
    fn without_a_pixel_scale_or_a_recorded_hfr_there_is_nothing_to_size_the_width_from() {
        let optics = Optics {
            pixel_scale_arcsec_per_pixel: None,
            ..reference_optics()
        };
        let err = plan_sweep("main", &optics, &train(), &sweep(), None, None).unwrap_err();
        assert!(
            err.to_string().contains("pixel_scale_arcsec_per_pixel"),
            "{err}"
        );
        // A recorded HFR for the filter supplies it instead.
        let plan = plan_sweep("main", &optics, &train(), &sweep(), None, Some(1.5)).unwrap();
        assert_eq!(plan.hfr_focus, Some(1.5));
    }

    #[test]
    fn a_focal_ratio_is_derived_from_the_length_and_the_aperture_when_absent() {
        let optics = Optics {
            focal_ratio: None,
            ..reference_optics()
        };
        assert_eq!(focal_ratio(&optics), Some(5.0));
        let plan = plan_sweep("main", &optics, &train(), &sweep(), None, None).unwrap();
        assert_eq!(plan.step_size, 17);
    }

    #[test]
    fn a_non_positive_fact_counts_as_unknown() {
        let optics = Optics {
            focal_ratio: Some(0.0),
            focal_length_mm: Some(-1.0),
            ..reference_optics()
        };
        assert_eq!(focal_ratio(&optics), None);
    }
}
