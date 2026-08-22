//! Decision logic for the convenience planner tools (`get_next_target`,
//! `get_target_status`).
//!
//! Pure function over (target list, current time, site, `Ephemeris`
//! impl, default min-altitude, progress counters); a hand-rolled mock
//! `Ephemeris` can drive it deterministically in tests.
//!
//! v1 implements five of the rp.md §"Dynamic Planner" decision-logic
//! bullets: altitude elimination (the first half of bullet 1),
//! transit preference (bullet 2), progress + filter tie-breaking
//! (bullets 3–4: survivors within [`TRANSIT_TIE_BAND_HOURS`] of the
//! best |HA| count as equally transiting, and among them least
//! completed-to-goal fraction wins, then a next-exposure filter
//! matching the last recorded frame, then target-store list order),
//! and bullet 6 in full — an exhausted target (every plan entry's
//! `count` met per the `record_exposure` counters) is eliminated,
//! all targets exhausted is `EndOfSession`, and otherwise when no
//! target survives, the Sun-elevation cut-off plus the Sun's
//! trend separates `WaitForTwilight` (dusk side), `EndOfSession`
//! (dawn side: the night is over), and `AllBelowMinAltitude` (true
//! astronomical night). Documented gaps: the set-time half of
//! bullet 1 and explicit bullet 5 (meridian-flip-aware exposure-fit
//! check; the choice of smallest-|HA| target satisfies it
//! indirectly) — tracked in the rp.md §"v1 implementation status"
//! callout.
//!
//! The returned `NextTargetReason` is a structured discriminant so
//! a planner plugin can branch without parsing free-form text.

use chrono::{DateTime, Utc};
use rp_ephemeris::{Ephemeris, Site};
use rp_targets::IcrsCoord;
use serde::Serialize;

use super::progress::PlanProgress;

/// Sun altitude (degrees) at astronomical dusk — the boundary that
/// rp.md's prose for `WaitForTwilight` references. Above this, the
/// sky is still too bright for deep-sky imaging (daylight, civil, or
/// nautical twilight); below it, true astronomical night.
const ASTRONOMICAL_DUSK_DEG: f64 = -18.0;

/// How far ahead the Sun is re-sampled to read its altitude trend
/// when the sky is brighter than astronomical dusk. Over 60 s the Sun
/// moves up to ≈ 0.25° in altitude — far above floating-point noise,
/// yet short enough that the sample cannot jump across a culmination
/// to the other side of the night.
const SUN_TREND_SAMPLE_SECS: i64 = 60;

/// Survivors whose |HA| is within this band of the best candidate's
/// count as equally transiting, letting the progress and filter
/// tie-breakers (rp.md §"Dynamic Planner" bullets 3–4) choose among
/// them. Half an hour of hour angle costs a negligible fraction of a
/// degree of altitude near culmination, so trading it for balanced
/// integration (and fewer filter changes) is free.
const TRANSIT_TIE_BAND_HOURS: f64 = 0.5;

/// A planner decision candidate: a target's stable identity (`name` =
/// its store slug), validated ICRS coordinate, altitude floor, and
/// plan.
///
/// This is also the `get_next_target` wire type — its derived
/// `Serialize` produces the tool result's `target` object, so `coord`
/// serializes as a nested `{ra_hours, dec_degrees}` object while the
/// decision-only `exposures` plan is skipped (the selected entry
/// surfaces separately as the recommendation's `exposure`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlannerTarget {
    pub name: String,
    /// Validated ICRS coordinate (the store's plan value type). Nests
    /// on the wire as `coord: {ra_hours, dec_degrees}`.
    pub coord: IcrsCoord,
    /// Per-target altitude floor. `None` falls back to the
    /// planner-wide minimum supplied by the caller.
    pub min_altitude_degrees: Option<f64>,
    /// The target's own framing angle (degrees east of north), layer
    /// one of the effective position angle. A decision input only,
    /// skipped on the wire — the recommendation surfaces the resolved
    /// *effective* angle instead (rp.md § Target Store → Position
    /// angle).
    #[serde(skip)]
    pub position_angle_degrees: Option<f64>,
    /// The target's plan (store goals), in list order — a decision
    /// input only, skipped on the wire. `next_target` surfaces the
    /// first incomplete entry as the recommendation's `exposure`;
    /// empty ⇒ that is null (the orchestrator's own exposure
    /// parameters apply).
    #[serde(skip)]
    pub exposures: Vec<ExposureSpec>,
}

/// One entry of a target's plan, projected from a store
/// `AcquisitionGoal` (rp.md § Target Store).
///
/// When this entry is the recommendation `next_target` surfaces,
/// `filter` / `duration_secs` are the wire `exposure` object; `count`
/// is a decision input (goal tracking) and is skipped on the wire.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExposureSpec {
    /// `None` for an unfiltered entry (an empty store filter — e.g. an
    /// OSC rig without a filter wheel).
    pub filter: Option<String>,
    /// Exposure duration in seconds; positive and finite.
    pub duration_secs: f64,
    /// Integration goal for this entry (frames), tracked by the
    /// `record_exposure` counters. `None` = no finite goal: the entry
    /// recommends forever and its target never exhausts. Skipped on the
    /// wire — a decision input, not part of the tool result.
    #[serde(skip)]
    pub count: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NextTargetReason {
    BestTransitingCandidate,
    NoTargetsConfigured,
    AllBelowMinAltitude,
    WaitForTwilight,
    EndOfSession,
}

#[derive(Debug, Clone, Serialize)]
pub struct NextTargetRecommendation {
    /// `None` when `reason` is `NoTargetsConfigured`,
    /// `AllBelowMinAltitude`, `WaitForTwilight`, or `EndOfSession`.
    pub target: Option<PlannerTarget>,
    pub reason: NextTargetReason,
    /// The recommended target's first *incomplete* `exposures[]`
    /// entry in plan order — what the `filter` / `duration_secs`
    /// fields of the tool result surface. `None` when there is no
    /// target or its plan is empty (the orchestrator's own exposure
    /// parameters apply).
    pub exposure: Option<ExposureSpec>,
    /// The recommended target's *effective* framing angle: its own
    /// `position_angle_degrees`, else the caller-supplied train
    /// default, else 0.0 north-up (rp.md § Target Store → Position
    /// angle, plan Decision 5). `None` exactly when `target` is
    /// `None`.
    pub position_angle_degrees: Option<f64>,
}

/// Pick the next target to slew to. The decision is a pure function
/// of its arguments, so tests can drive it with a hand-rolled
/// `Ephemeris` mock, a frozen `now`, and a hand-filled progress
/// store.
pub fn next_target(
    eph: &impl Ephemeris,
    site: &Site,
    now: DateTime<Utc>,
    targets: &[PlannerTarget],
    default_min_altitude_deg: f64,
    train_default_position_angle_deg: Option<f64>,
    progress: &PlanProgress,
) -> NextTargetRecommendation {
    if targets.is_empty() {
        return none_with(NextTargetReason::NoTargetsConfigured);
    }

    let survivors = eliminate(eph, site, now, targets, default_min_altitude_deg, progress);
    if survivors.is_empty() {
        return none_with(no_survivors_reason(eph, site, now, targets, progress));
    }

    // Step 2: prefer transiting — smallest |HA| from the current LST
    // (bullet 2), with survivors inside `TRANSIT_TIE_BAND_HOURS` of
    // that best |HA| treated as ties for the progress and filter
    // tie-breakers (bullets 3–4) to order: least completed-to-goal
    // fraction first, then a next exposure whose filter matches the
    // last recorded frame's, then target-store list order (survivors
    // keep the store's list order, so the scan's strict `<` is that
    // final tie-break).
    let lst = eph.sidereal_time(site, now).lst_hours;
    let abs_ha = |t: &PlannerTarget| signed_hour_angle(lst, t.coord.ra_hours()).abs();
    let Some(best_ha) = survivors
        .iter()
        .map(|t| abs_ha(t))
        .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
    else {
        // Unreachable: the empty-survivors branch returns above. If a
        // future refactor invalidates that invariant we fall back to
        // the same "nothing above min altitude" outcome rather than
        // panicking.
        return none_with(NextTargetReason::AllBelowMinAltitude);
    };
    let mut chosen: Option<(&PlannerTarget, (f64, bool, f64))> = None;
    for t in &survivors {
        let ha = abs_ha(t);
        if ha > best_ha + TRANSIT_TIE_BAND_HOURS {
            continue;
        }
        let filter_matches_last = match (
            progress.last_filter_key(),
            progress.next_incomplete_entry(t),
        ) {
            (Some(last_filter), Some(entry)) => {
                super::progress::filter_key(entry.filter.as_deref()) == last_filter
            }
            _ => false,
        };
        // Sort key inside the band, in bullet order: least fraction
        // (bullet 3), then a matching filter (bullet 4 — negated so
        // `false` = match sorts first), then the in-band |HA| itself
        // so two otherwise-equal candidates still prefer the closer
        // transit. Config order wins remaining exact ties via the
        // strict `<`.
        let key = (progress.fraction(t), !filter_matches_last, ha);
        let better = match &chosen {
            None => true,
            Some((_, k)) => key
                .partial_cmp(k)
                .unwrap_or(std::cmp::Ordering::Equal)
                .is_lt(),
        };
        if better {
            chosen = Some((t, key));
        }
    }
    let Some((chosen, _)) = chosen else {
        // Unreachable for the same reason as above: at least the
        // best-|HA| survivor is inside its own band.
        return none_with(NextTargetReason::AllBelowMinAltitude);
    };

    // The three-layer effective angle (rp.md § Target Store → Position
    // angle): target value → the caller's train default → 0.0 north-up.
    let position_angle_degrees = Some(
        chosen
            .position_angle_degrees
            .or(train_default_position_angle_deg)
            .unwrap_or(0.0),
    );
    NextTargetRecommendation {
        target: Some(chosen.clone()),
        reason: NextTargetReason::BestTransitingCandidate,
        exposure: progress.next_incomplete_entry(chosen).cloned(),
        position_angle_degrees,
    }
}

/// A target-less recommendation carrying only `reason`.
const fn none_with(reason: NextTargetReason) -> NextTargetRecommendation {
    NextTargetRecommendation {
        target: None,
        reason,
        exposure: None,
        position_angle_degrees: None,
    }
}

/// Step 1: eliminate. A target whose computed alt is below
/// `min_altitude_degrees` (per-target if set, else default) is
/// dropped, and so is an exhausted one — every `exposures[]`
/// entry's `count` met per the `record_exposure` counters
/// (rp.md §"Dynamic Planner" bullet 6's "met its integration
/// goal"). Set-time elimination (the "will set before one
/// exposure can complete" half of rp.md §"Dynamic Planner"
/// bullet 1) is a documented v1 gap — see the §"v1 implementation
/// status" callout in `docs/services/rp.md`.
fn eliminate<'a>(
    eph: &impl Ephemeris,
    site: &Site,
    now: DateTime<Utc>,
    targets: &'a [PlannerTarget],
    default_min_altitude_deg: f64,
    progress: &PlanProgress,
) -> Vec<&'a PlannerTarget> {
    let mut survivors: Vec<&PlannerTarget> = Vec::new();
    for t in targets {
        if progress.is_exhausted(t) {
            tracing::debug!(
                target = %t.name,
                "target met its integration goal; eliminated from next_target evaluation"
            );
            continue;
        }
        // The validated plan coordinate converts total-ly to the
        // ephemeris crate's computed coordinate (ADR-019 boundary
        // bridge — a valid plan coord is always a valid transform
        // input).
        let coords: rp_ephemeris::IcrsCoord = t.coord.into();
        let aa = match eph.alt_az(site, coords, now) {
            Ok(aa) => aa,
            Err(e) => {
                // ERFA can refuse the alt/az transform at degenerate
                // sites (e.g. exactly the pole). Log it so a
                // configuration problem doesn't disguise itself as
                // "all targets below floor"; continue past the
                // offender.
                tracing::debug!(
                    target = %t.name,
                    error = %e,
                    "alt/az transform failed; skipping target in next_target evaluation"
                );
                continue;
            }
        };
        let floor = t.min_altitude_degrees.unwrap_or(default_min_altitude_deg);
        if aa.altitude_degrees >= floor {
            survivors.push(t);
        }
    }
    survivors
}

/// Why an empty survivor set ends (or pauses) the night. Every target
/// met its integration goal: the session is complete regardless of
/// what the sky is doing — the other `EndOfSession` trigger of rp.md
/// §"Dynamic Planner" bullet 6. Otherwise, distinguish "the sky is too
/// bright to image" from "all targets are genuinely below the altitude
/// floor": below the Sun-altitude threshold for astronomical twilight
/// (-18°, true astronomical night) every target under its floor is
/// `AllBelowMinAltitude`. Brighter than that, the Sun's own trend
/// tells the two bright ends of the night apart: a climbing Sun
/// (re-sampled `SUN_TREND_SAMPLE_SECS` ahead) is the dawn side — the
/// night is over, `EndOfSession` — while a descending Sun matches
/// rp.md's "astronomical dusk has not yet begun", `WaitForTwilight`.
/// A level Sun (only at the culminations) ties to waiting, because a
/// wait loop re-asks and self-corrects while ending a session is
/// final.
fn no_survivors_reason(
    eph: &impl Ephemeris,
    site: &Site,
    now: DateTime<Utc>,
    targets: &[PlannerTarget],
    progress: &PlanProgress,
) -> NextTargetReason {
    if targets.iter().all(|t| progress.is_exhausted(t)) {
        return NextTargetReason::EndOfSession;
    }
    let sun_alt = eph.sun_position(site, now).alt_az.altitude_degrees;
    if sun_alt > ASTRONOMICAL_DUSK_DEG {
        // Unreachable overflow degrades to a flat trend, which falls
        // through to the recoverable wait branch below.
        let resample = now
            .checked_add_signed(chrono::Duration::seconds(SUN_TREND_SAMPLE_SECS))
            .unwrap_or(now);
        let sun_alt_later = eph.sun_position(site, resample).alt_az.altitude_degrees;
        if sun_alt_later > sun_alt {
            NextTargetReason::EndOfSession
        } else {
            NextTargetReason::WaitForTwilight
        }
    } else {
        NextTargetReason::AllBelowMinAltitude
    }
}

/// Hour angle of `target_ra_hours` at `lst_hours`, normalised to
/// the half-open interval `(-12, 12]` (negative = east of meridian,
/// positive = west).
#[must_use]
pub fn signed_hour_angle(lst_hours: f64, target_ra_hours: f64) -> f64 {
    let mut ha = (lst_hours - target_ra_hours).rem_euclid(24.0);
    if ha > 12.0 {
        ha -= 24.0;
    }
    ha
}

/// Project a store-backed [`rp_targets::Target`] onto a [`PlannerTarget`]
/// candidate for `next_target` (Decision 9 — altitude-gating parity,
/// `docs/plans/planetarium-target-import.md`). `name` carries the
/// target's `slug` (its stable identity — `display_name` is freely
/// operator-editable and unsuited as a lookup key). The validated
/// `coord` is shared directly — both types hold the same
/// `rp_targets::IcrsCoord`, so no re-validation is needed. Every goal's
/// `desired_count` is a required, finite `u32` (`validate_goals`
/// rejects zero), so each maps to a `count: Some(_)` entry — a
/// store-backed target's plan never "recommends forever": every plan
/// entry it projects to carries a finite goal.
impl From<&rp_targets::Target> for PlannerTarget {
    fn from(t: &rp_targets::Target) -> Self {
        Self {
            name: t.slug.as_str().to_string(),
            coord: t.coord,
            min_altitude_degrees: t.scheduling.and_then(|s| s.min_altitude_degrees),
            position_angle_degrees: t.position_angle_degrees,
            exposures: t
                .goals
                .iter()
                .map(|g| ExposureSpec {
                    filter: (!g.filter.is_empty()).then(|| g.filter.clone()),
                    duration_secs: g.exposure_duration.as_secs_f64(),
                    count: Some(g.desired_count),
                })
                .collect(),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use rp_ephemeris::{
        AltAz, EphemerisError, IcrsCoord, LocalSiderealTime, MoonInfo, RiseSet, Site, SunInfo,
        TwilightKind, TwilightWindow,
    };

    /// A progress snapshot with `good == total` counts per plan entry,
    /// in plan order — the ungraded case, which is what every decision
    /// test here cares about. The scan derives these for real; the
    /// decision logic only ever reads them.
    fn met(entries: &[(&str, &[u32])]) -> PlanProgress {
        let mut p = PlanProgress::default();
        for (name, counts) in entries {
            p.insert(
                name,
                counts
                    .iter()
                    .map(|&n| super::super::progress_scan::GoalProgress { good: n, total: n })
                    .collect(),
            );
        }
        p
    }

    /// Hand-rolled mock so the decision logic is testable without
    /// hitting real ERFA. The closures fix the answers per-target.
    #[derive(Default)]
    struct MockEphemeris {
        /// (`ra_hours`, `dec_degrees`) → `altitude_degrees`
        alt_overrides: Vec<((f64, f64), f64)>,
        /// Sun altitude at the tests' `now()` epoch.
        sun_alt: f64,
        /// Sun-altitude change per minute after `now()` — drives the
        /// dusk/dawn trend check. `0.0` freezes the Sun (a level Sun
        /// reads as the dusk side).
        sun_alt_rate_deg_per_min: f64,
        lst_hours: f64,
    }

    impl Ephemeris for MockEphemeris {
        fn sidereal_time(&self, _site: &Site, _t: DateTime<Utc>) -> LocalSiderealTime {
            LocalSiderealTime {
                lst_hours: self.lst_hours,
            }
        }
        fn alt_az(
            &self,
            _site: &Site,
            target: IcrsCoord,
            _t: DateTime<Utc>,
        ) -> Result<AltAz, EphemerisError> {
            let alt = self
                .alt_overrides
                .iter()
                .find_map(|((ra, dec), alt)| {
                    if (ra - target.ra_hours).abs() < 1e-9
                        && (dec - target.dec_degrees).abs() < 1e-9
                    {
                        Some(*alt)
                    } else {
                        None
                    }
                })
                .unwrap_or(0.0);
            Ok(AltAz {
                altitude_degrees: alt,
                azimuth_degrees: 0.0,
            })
        }
        // The decision logic only consults `alt_az` and
        // `sun_position`; the remaining trait methods exist to satisfy
        // the impl block but are never called from these tests.
        // Mark them coverage-skip so they don't depress the patch %.
        #[cfg_attr(coverage_nightly, coverage(off))]
        fn transit(
            &self,
            _site: &Site,
            _target: IcrsCoord,
            _date: chrono::NaiveDate,
        ) -> Option<DateTime<Utc>> {
            None
        }
        #[cfg_attr(coverage_nightly, coverage(off))]
        fn rise_set(
            &self,
            _site: &Site,
            _target: IcrsCoord,
            _date: chrono::NaiveDate,
            _min: f64,
        ) -> Option<RiseSet> {
            None
        }
        #[cfg_attr(coverage_nightly, coverage(off))]
        fn meridian_flip(
            &self,
            _site: &Site,
            _target: IcrsCoord,
            _t: DateTime<Utc>,
            _side: rp_ephemeris::SideOfPier,
        ) -> Option<chrono::Duration> {
            None
        }
        fn sun_position(&self, _site: &Site, t: DateTime<Utc>) -> SunInfo {
            let minutes = (t - now()).num_seconds() as f64 / 60.0;
            SunInfo {
                coords: IcrsCoord {
                    ra_hours: 0.0,
                    dec_degrees: 0.0,
                },
                alt_az: AltAz {
                    altitude_degrees: self.sun_alt + self.sun_alt_rate_deg_per_min * minutes,
                    azimuth_degrees: 0.0,
                },
            }
        }
        #[cfg_attr(coverage_nightly, coverage(off))]
        fn twilight(
            &self,
            _site: &Site,
            _date: chrono::NaiveDate,
            _kind: TwilightKind,
        ) -> TwilightWindow {
            TwilightWindow {
                begin_utc: None,
                end_utc: None,
            }
        }
        #[cfg_attr(coverage_nightly, coverage(off))]
        fn moon_position(&self, _site: &Site, _t: DateTime<Utc>) -> MoonInfo {
            MoonInfo {
                coords: IcrsCoord {
                    ra_hours: 0.0,
                    dec_degrees: 0.0,
                },
                alt_az: AltAz {
                    altitude_degrees: 0.0,
                    azimuth_degrees: 0.0,
                },
                phase_degrees: 0.0,
                illumination_fraction: 0.5,
            }
        }
        #[cfg_attr(coverage_nightly, coverage(off))]
        fn moon_separation(&self, _target: IcrsCoord, _t: DateTime<Utc>) -> f64 {
            0.0
        }
    }

    fn site() -> Site {
        Site::new(47.6062, -122.3321).unwrap()
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 1, 6, 0, 0).unwrap()
    }

    #[test]
    fn empty_targets_return_no_targets_configured() {
        let rec = next_target(
            &MockEphemeris::default(),
            &site(),
            now(),
            &[],
            20.0,
            None,
            &PlanProgress::default(),
        );
        assert!(rec.target.is_none());
        assert_eq!(rec.reason, NextTargetReason::NoTargetsConfigured);
    }

    #[test]
    fn target_below_min_alt_is_eliminated() {
        let eph = MockEphemeris {
            alt_overrides: vec![((0.7123, 41.27), 10.0)],
            sun_alt: -25.0, // true astronomical night (sun < -18°)
            sun_alt_rate_deg_per_min: 0.0,
            lst_hours: 12.0,
        };
        let targets = vec![PlannerTarget {
            name: "M31".into(),
            coord: rp_targets::IcrsCoord::try_new(0.7123, 41.27).unwrap(),
            min_altitude_degrees: None,
            position_angle_degrees: None,
            exposures: Vec::new(),
        }];
        let rec = next_target(
            &eph,
            &site(),
            now(),
            &targets,
            30.0,
            None,
            &PlanProgress::default(),
        );
        assert!(rec.target.is_none());
        assert_eq!(rec.reason, NextTargetReason::AllBelowMinAltitude);
    }

    #[test]
    fn a_level_daytime_sun_is_wait_for_twilight() {
        let eph = MockEphemeris {
            alt_overrides: vec![((0.7123, 41.27), 10.0)],
            sun_alt: 30.0, // the Sun is up and, frozen at rate 0, not climbing
            sun_alt_rate_deg_per_min: 0.0,
            lst_hours: 12.0,
        };
        let targets = vec![PlannerTarget {
            name: "M31".into(),
            coord: rp_targets::IcrsCoord::try_new(0.7123, 41.27).unwrap(),
            min_altitude_degrees: None,
            position_angle_degrees: None,
            exposures: Vec::new(),
        }];
        let rec = next_target(
            &eph,
            &site(),
            now(),
            &targets,
            30.0,
            None,
            &PlanProgress::default(),
        );
        assert!(rec.target.is_none());
        assert_eq!(rec.reason, NextTargetReason::WaitForTwilight);
    }

    #[test]
    fn nautical_twilight_returns_wait_for_twilight_not_all_below_min_altitude() {
        // Sun at -10° (nautical twilight, between civil at -6° and
        // astronomical at -18°). Per rp.md prose, "astronomical dusk
        // has not yet begun" → WaitForTwilight, not AllBelowMinAltitude.
        let eph = MockEphemeris {
            alt_overrides: vec![((0.7123, 41.27), 10.0)],
            sun_alt: -10.0,
            sun_alt_rate_deg_per_min: 0.0,
            lst_hours: 12.0,
        };
        let targets = vec![PlannerTarget {
            name: "M31".into(),
            coord: rp_targets::IcrsCoord::try_new(0.7123, 41.27).unwrap(),
            min_altitude_degrees: None,
            position_angle_degrees: None,
            exposures: Vec::new(),
        }];
        let rec = next_target(
            &eph,
            &site(),
            now(),
            &targets,
            30.0,
            None,
            &PlanProgress::default(),
        );
        assert_eq!(rec.reason, NextTargetReason::WaitForTwilight);
    }

    #[test]
    fn a_descending_twilight_sun_is_wait_for_twilight() {
        // Evening twilight: the Sun at -10° and sinking — the night
        // has not started, wait for it.
        let eph = MockEphemeris {
            alt_overrides: vec![((0.7123, 41.27), 10.0)],
            sun_alt: -10.0,
            sun_alt_rate_deg_per_min: -0.2,
            lst_hours: 12.0,
        };
        let targets = vec![PlannerTarget {
            name: "M31".into(),
            coord: rp_targets::IcrsCoord::try_new(0.7123, 41.27).unwrap(),
            min_altitude_degrees: None,
            position_angle_degrees: None,
            exposures: Vec::new(),
        }];
        let rec = next_target(
            &eph,
            &site(),
            now(),
            &targets,
            30.0,
            None,
            &PlanProgress::default(),
        );
        assert_eq!(rec.reason, NextTargetReason::WaitForTwilight);
    }

    #[test]
    fn a_climbing_twilight_sun_is_end_of_session() {
        // Morning twilight: the Sun at -10° and climbing — the night
        // is over.
        let eph = MockEphemeris {
            alt_overrides: vec![((0.7123, 41.27), 10.0)],
            sun_alt: -10.0,
            sun_alt_rate_deg_per_min: 0.2,
            lst_hours: 12.0,
        };
        let targets = vec![PlannerTarget {
            name: "M31".into(),
            coord: rp_targets::IcrsCoord::try_new(0.7123, 41.27).unwrap(),
            min_altitude_degrees: None,
            position_angle_degrees: None,
            exposures: Vec::new(),
        }];
        let rec = next_target(
            &eph,
            &site(),
            now(),
            &targets,
            30.0,
            None,
            &PlanProgress::default(),
        );
        assert!(rec.target.is_none());
        assert_eq!(rec.reason, NextTargetReason::EndOfSession);
    }

    #[test]
    fn a_climbing_daytime_sun_is_end_of_session() {
        // A session invoked mid-morning: the Sun is high and still
        // climbing. This calendar night is over — end, don't wait.
        let eph = MockEphemeris {
            alt_overrides: vec![((0.7123, 41.27), 10.0)],
            sun_alt: 30.0,
            sun_alt_rate_deg_per_min: 0.2,
            lst_hours: 12.0,
        };
        let targets = vec![PlannerTarget {
            name: "M31".into(),
            coord: rp_targets::IcrsCoord::try_new(0.7123, 41.27).unwrap(),
            min_altitude_degrees: None,
            position_angle_degrees: None,
            exposures: Vec::new(),
        }];
        let rec = next_target(
            &eph,
            &site(),
            now(),
            &targets,
            30.0,
            None,
            &PlanProgress::default(),
        );
        assert_eq!(rec.reason, NextTargetReason::EndOfSession);
    }

    #[test]
    fn a_climbing_sun_still_below_astronomical_dusk_is_all_below_min_altitude() {
        // Pre-dawn astronomical night: the Sun rises toward -18° but
        // has not crossed it. It is still properly dark, so a
        // below-floor target set is reported as such — dawn is only
        // declared once the sky is actually bright.
        let eph = MockEphemeris {
            alt_overrides: vec![((0.7123, 41.27), 10.0)],
            sun_alt: -25.0,
            sun_alt_rate_deg_per_min: 0.2,
            lst_hours: 12.0,
        };
        let targets = vec![PlannerTarget {
            name: "M31".into(),
            coord: rp_targets::IcrsCoord::try_new(0.7123, 41.27).unwrap(),
            min_altitude_degrees: None,
            position_angle_degrees: None,
            exposures: Vec::new(),
        }];
        let rec = next_target(
            &eph,
            &site(),
            now(),
            &targets,
            30.0,
            None,
            &PlanProgress::default(),
        );
        assert_eq!(rec.reason, NextTargetReason::AllBelowMinAltitude);
    }

    /// A target no computed altitude can reach — forces the
    /// no-survivors branch against the real ephemeris.
    fn never_visible_target() -> Vec<PlannerTarget> {
        vec![PlannerTarget {
            name: "below floor".into(),
            coord: rp_targets::IcrsCoord::try_new(0.0, 0.0).unwrap(),
            min_altitude_degrees: Some(90.0),
            position_angle_degrees: None,
            exposures: Vec::new(),
        }]
    }

    // The two real-ephemeris tests below pin the same equinox
    // instants the BDD dusk/dawn scenarios use, but through
    // `next_target` directly — the mock tests above fix the trend
    // maths, these keep it honest against the real sky. At the UK
    // site on 2026-03-20 the Sun sits near -11° descending at
    // 19:20 UTC and near -10° climbing at 05:00 UTC.

    #[test]
    fn real_ephemeris_evening_twilight_is_wait_for_twilight() {
        let eph = rp_ephemeris::ErfarsEphemeris::new();
        let site = Site::new(51.0786, -0.2944).unwrap();
        let t = Utc.with_ymd_and_hms(2026, 3, 20, 19, 20, 0).unwrap();
        let sun_alt = eph.sun_position(&site, t).alt_az.altitude_degrees;
        assert!(
            (-18.0..0.0).contains(&sun_alt),
            "the pinned instant must sit in twilight; the Sun is at {sun_alt}°"
        );
        let rec = next_target(
            &eph,
            &site,
            t,
            &never_visible_target(),
            20.0,
            None,
            &PlanProgress::default(),
        );
        assert_eq!(rec.reason, NextTargetReason::WaitForTwilight);
    }

    #[test]
    fn real_ephemeris_morning_twilight_is_end_of_session() {
        let eph = rp_ephemeris::ErfarsEphemeris::new();
        let site = Site::new(51.0786, -0.2944).unwrap();
        let t = Utc.with_ymd_and_hms(2026, 3, 20, 5, 0, 0).unwrap();
        let sun_alt = eph.sun_position(&site, t).alt_az.altitude_degrees;
        assert!(
            (-18.0..0.0).contains(&sun_alt),
            "the pinned instant must sit in twilight; the Sun is at {sun_alt}°"
        );
        let rec = next_target(
            &eph,
            &site,
            t,
            &never_visible_target(),
            20.0,
            None,
            &PlanProgress::default(),
        );
        assert_eq!(rec.reason, NextTargetReason::EndOfSession);
    }

    #[test]
    fn full_astronomical_night_with_no_targets_above_floor_is_all_below_min() {
        // Sun well below -18° (true astronomical night) and every
        // target still below the floor → distinguish from twilight.
        let eph = MockEphemeris {
            alt_overrides: vec![((0.7123, 41.27), 10.0)],
            sun_alt: -25.0,
            sun_alt_rate_deg_per_min: 0.0,
            lst_hours: 12.0,
        };
        let targets = vec![PlannerTarget {
            name: "M31".into(),
            coord: rp_targets::IcrsCoord::try_new(0.7123, 41.27).unwrap(),
            min_altitude_degrees: None,
            position_angle_degrees: None,
            exposures: Vec::new(),
        }];
        let rec = next_target(
            &eph,
            &site(),
            now(),
            &targets,
            30.0,
            None,
            &PlanProgress::default(),
        );
        assert_eq!(rec.reason, NextTargetReason::AllBelowMinAltitude);
    }

    #[test]
    fn picks_target_closest_to_transit() {
        // LST = 12.0. Two targets above min alt:
        //   M31 at ra=0.7 → HA = 11.3 → very far from transit
        //   M42 at ra=11.0 → HA = 1.0 → close to transit
        let eph = MockEphemeris {
            alt_overrides: vec![((0.7, 41.0), 50.0), ((11.0, -5.0), 50.0)],
            sun_alt: -20.0,
            sun_alt_rate_deg_per_min: 0.0,
            lst_hours: 12.0,
        };
        let targets = vec![
            PlannerTarget {
                name: "M31".into(),
                coord: rp_targets::IcrsCoord::try_new(0.7, 41.0).unwrap(),
                min_altitude_degrees: None,
                position_angle_degrees: None,
                exposures: Vec::new(),
            },
            PlannerTarget {
                name: "M42".into(),
                coord: rp_targets::IcrsCoord::try_new(11.0, -5.0).unwrap(),
                min_altitude_degrees: None,
                position_angle_degrees: None,
                exposures: Vec::new(),
            },
        ];
        let rec = next_target(
            &eph,
            &site(),
            now(),
            &targets,
            20.0,
            None,
            &PlanProgress::default(),
        );
        let target = rec.target.expect("expected a target");
        assert_eq!(target.name, "M42");
        assert_eq!(rec.reason, NextTargetReason::BestTransitingCandidate);
    }

    // --- progress-aware selection (rp.md bullets 3, 4, and the
    // exhausted-targets half of bullet 6) -------------------------

    /// A dec-0 target above the floor at `ra_hours`, with a plan.
    fn target_with_plan(name: &str, ra_hours: f64, exposures: Vec<ExposureSpec>) -> PlannerTarget {
        PlannerTarget {
            name: name.into(),
            coord: rp_targets::IcrsCoord::try_new(ra_hours, 0.0).unwrap(),
            min_altitude_degrees: None,
            position_angle_degrees: None,
            exposures,
        }
    }

    fn spec(filter: &str, count: u32) -> ExposureSpec {
        ExposureSpec {
            filter: Some(filter.into()),
            duration_secs: 60.0,
            count: Some(count),
        }
    }

    /// Every dec-0 target at the given RAs sits at 50° — selection
    /// tests care about hour angle and progress, not elimination.
    fn night_eph(ras: &[f64]) -> MockEphemeris {
        MockEphemeris {
            alt_overrides: ras.iter().map(|ra| ((*ra, 0.0), 50.0)).collect(),
            sun_alt: -25.0,
            sun_alt_rate_deg_per_min: 0.0,
            lst_hours: 12.0,
        }
    }

    #[test]
    fn effective_angle_prefers_the_targets_own_value_over_the_train_default() {
        let eph = night_eph(&[12.0]);
        let mut t = target_with_plan("M31", 12.0, Vec::new());
        t.position_angle_degrees = Some(121.25);
        let rec = next_target(
            &eph,
            &site(),
            now(),
            &[t],
            20.0,
            Some(254.0),
            &PlanProgress::default(),
        );
        assert_eq!(rec.position_angle_degrees, Some(121.25));
    }

    #[test]
    fn effective_angle_falls_back_to_the_train_default_then_north_up() {
        let eph = night_eph(&[12.0]);
        let targets = vec![target_with_plan("M31", 12.0, Vec::new())];
        let rec = next_target(
            &eph,
            &site(),
            now(),
            &targets,
            20.0,
            Some(254.0),
            &PlanProgress::default(),
        );
        assert_eq!(rec.position_angle_degrees, Some(254.0));
        let rec = next_target(
            &eph,
            &site(),
            now(),
            &targets,
            20.0,
            None,
            &PlanProgress::default(),
        );
        assert_eq!(rec.position_angle_degrees, Some(0.0));
    }

    #[test]
    fn no_recommendation_carries_no_effective_angle() {
        let rec = next_target(
            &MockEphemeris::default(),
            &site(),
            now(),
            &[],
            20.0,
            Some(254.0),
            &PlanProgress::default(),
        );
        assert_eq!(rec.position_angle_degrees, None);
    }

    #[test]
    fn an_exhausted_target_is_eliminated_and_the_backup_recommended() {
        // "M31" transits (HA 0) but its whole plan is complete; the
        // farther "M42" is the only live candidate.
        let eph = night_eph(&[12.0, 10.0]);
        let targets = vec![
            target_with_plan("M31", 12.0, vec![spec("L", 1)]),
            target_with_plan("M42", 10.0, vec![spec("L", 1)]),
        ];
        let p = met(&[("M31", &[1])]);
        let rec = next_target(&eph, &site(), now(), &targets, 20.0, None, &p);
        assert_eq!(rec.target.expect("expected a target").name, "M42");
    }

    #[test]
    fn all_targets_exhausted_is_end_of_session_even_in_deep_night() {
        // Sun at -25° (true astronomical night) and the target still
        // above its floor — but its integration goal is met, so the
        // session is over. This is the non-dawn `EndOfSession`.
        let eph = night_eph(&[12.0]);
        let targets = vec![target_with_plan("M31", 12.0, vec![spec("L", 1)])];
        let p = met(&[("M31", &[1])]);
        let rec = next_target(&eph, &site(), now(), &targets, 20.0, None, &p);
        assert!(rec.target.is_none());
        assert_eq!(rec.reason, NextTargetReason::EndOfSession);
    }

    #[test]
    fn a_below_floor_survivor_prevents_the_exhaustion_end_of_session() {
        // One target exhausted, the other merely below its floor: the
        // night is not over — the sky gating answers (dark sky ⇒
        // AllBelowMinAltitude), so the orchestrator keeps waiting for
        // the unfinished target to rise.
        let eph = MockEphemeris {
            alt_overrides: vec![((12.0, 0.0), 50.0), ((10.0, 0.0), 5.0)],
            sun_alt: -25.0,
            sun_alt_rate_deg_per_min: 0.0,
            lst_hours: 12.0,
        };
        let targets = vec![
            target_with_plan("done", 12.0, vec![spec("L", 1)]),
            target_with_plan("still rising", 10.0, vec![spec("L", 1)]),
        ];
        let p = met(&[("done", &[1])]);
        let rec = next_target(&eph, &site(), now(), &targets, 20.0, None, &p);
        assert!(rec.target.is_none());
        assert_eq!(rec.reason, NextTargetReason::AllBelowMinAltitude);
    }

    #[test]
    fn the_recommendation_rotates_to_the_first_incomplete_plan_entry() {
        let eph = night_eph(&[12.0]);
        let targets = vec![target_with_plan(
            "M31",
            12.0,
            vec![spec("L", 1), spec("R", 1)],
        )];
        let p = PlanProgress::default();
        let rec = next_target(&eph, &site(), now(), &targets, 20.0, None, &p);
        assert_eq!(
            rec.exposure.expect("plan entry").filter.as_deref(),
            Some("L")
        );
        let p = met(&[("M31", &[1, 0])]);
        let rec = next_target(&eph, &site(), now(), &targets, 20.0, None, &p);
        assert_eq!(
            rec.exposure.expect("plan entry").filter.as_deref(),
            Some("R"),
            "the completed Luminance goal must rotate the recommendation to Red"
        );
    }

    #[test]
    fn least_progress_wins_inside_the_transit_tie_band() {
        // "closer" transits exactly (HA 0) but is half done; "fresh"
        // sits 0.3 h away — inside the 0.5 h band, so bullet 3 hands
        // it the recommendation.
        let eph = night_eph(&[12.0, 11.7]);
        let targets = vec![
            target_with_plan("closer", 12.0, vec![spec("L", 2)]),
            target_with_plan("fresh", 11.7, vec![spec("L", 2)]),
        ];
        let p = met(&[("closer", &[1])]);
        let rec = next_target(&eph, &site(), now(), &targets, 20.0, None, &p);
        assert_eq!(rec.target.expect("expected a target").name, "fresh");
    }

    #[test]
    fn a_matching_filter_breaks_a_progress_tie() {
        // Both candidates are untouched (fraction 0) and in-band; the
        // last recorded frame was Red, so the target whose next
        // exposure is Red wins (bullet 4) despite the larger |HA| and
        // later config position.
        let eph = night_eph(&[12.0, 11.7]);
        let targets = vec![
            target_with_plan("blue next", 12.0, vec![spec("Blue", 5)]),
            target_with_plan("red next", 11.7, vec![spec("Red", 5)]),
        ];
        let p = PlanProgress::new(Some("Red".to_string()));
        let rec = next_target(&eph, &site(), now(), &targets, 20.0, None, &p);
        assert_eq!(rec.target.expect("expected a target").name, "red next");
    }

    #[test]
    fn outside_the_band_the_closer_transit_wins_regardless_of_progress() {
        // 1.1 h of hour angle is past the 0.5 h tie band: transit
        // preference (bullet 2) stays primary and the nearly-done
        // transiting target still wins.
        let eph = night_eph(&[12.0, 10.9]);
        let targets = vec![
            target_with_plan("transiting", 12.0, vec![spec("L", 10)]),
            target_with_plan("far and fresh", 10.9, vec![spec("L", 10)]),
        ];
        let p = met(&[("transiting", &[9])]);
        let rec = next_target(&eph, &site(), now(), &targets, 20.0, None, &p);
        assert_eq!(rec.target.expect("expected a target").name, "transiting");
    }

    #[test]
    fn per_target_min_altitude_overrides_default() {
        let eph = MockEphemeris {
            alt_overrides: vec![((1.0, 0.0), 25.0)],
            sun_alt: -20.0,
            sun_alt_rate_deg_per_min: 0.0,
            lst_hours: 1.0,
        };
        let targets = vec![PlannerTarget {
            name: "T1".into(),
            coord: rp_targets::IcrsCoord::try_new(1.0, 0.0).unwrap(),
            min_altitude_degrees: Some(20.0),
            position_angle_degrees: None,
            exposures: Vec::new(),
        }];
        // default 30 would eliminate; per-target 20 keeps it.
        let rec = next_target(
            &eph,
            &site(),
            now(),
            &targets,
            30.0,
            None,
            &PlanProgress::default(),
        );
        assert!(
            rec.target.is_some(),
            "per-target floor must override default"
        );
    }

    #[test]
    fn signed_hour_angle_wraps_correctly() {
        assert!((signed_hour_angle(0.0, 23.5) - 0.5).abs() < 1e-9);
        assert!((signed_hour_angle(23.5, 0.0) - (-0.5)).abs() < 1e-9);
        assert!((signed_hour_angle(12.0, 0.0) - 12.0).abs() < 1e-9);
    }

    fn store_target(
        slug: &str,
        scheduling: Option<rp_targets::SchedulingConstraints>,
        goals: Vec<rp_targets::AcquisitionGoal>,
    ) -> rp_targets::Target {
        rp_targets::Target {
            slug: rp_targets::TargetSlug::new(slug).unwrap(),
            display_name: slug.to_string(),
            coord: rp_targets::IcrsCoord::try_new(1.0, 2.0).unwrap(),
            catalog_ref: None,
            object_type: None,
            magnitude: None,
            size_arcmin: None,
            position_angle_degrees: None,
            priority: 0,
            active: true,
            goals,
            scheduling,
            grading: None,
            notes: None,
            created_at: "2026-01-01T00:00:00.000Z".to_string(),
            updated_at: "2026-01-01T00:00:00.000Z".to_string(),
            created_by: "operator".to_string(),
            updated_by: "operator".to_string(),
        }
    }

    #[test]
    fn from_store_target_uses_slug_as_identity() {
        let t = store_target("ngc7000", None, Vec::new());
        let planner_target = PlannerTarget::from(&t);
        assert_eq!(planner_target.name, "ngc7000");
        assert_eq!(planner_target.coord.ra_hours(), 1.0);
        assert_eq!(planner_target.coord.dec_degrees(), 2.0);
        assert_eq!(planner_target.min_altitude_degrees, None);
    }

    #[test]
    fn from_store_target_reads_the_scheduling_override() {
        let t = store_target(
            "ngc7000",
            Some(rp_targets::SchedulingConstraints {
                min_altitude_degrees: Some(35.0),
                ..Default::default()
            }),
            Vec::new(),
        );
        assert_eq!(PlannerTarget::from(&t).min_altitude_degrees, Some(35.0));
    }

    #[test]
    fn from_store_target_converts_goals_to_finite_exposure_specs() {
        let goal = rp_targets::AcquisitionGoal {
            filter: "L".to_string(),
            binning: rp_targets::Binning { x: 1, y: 1 },
            exposure_duration: std::time::Duration::from_mins(5),
            desired_count: 20,
        };
        let t = store_target("ngc7000", None, vec![goal]);
        let planner_target = PlannerTarget::from(&t);
        assert_eq!(
            planner_target.exposures,
            vec![ExposureSpec {
                filter: Some("L".to_string()),
                duration_secs: 300.0,
                count: Some(20),
            }]
        );
    }

    // --- get_next_target wire shape ------------------------------------
    // The derived `Serialize` of a `NextTargetRecommendation` *is* the
    // tool result (no hand-built view any more), so these pin its
    // contract: a nested `coord` object, a nested `exposure` object, and
    // the decision-only `exposures` / `count` fields kept off the wire.

    #[test]
    fn serialized_no_targets_branch_nulls_target_and_exposure() {
        let rec = NextTargetRecommendation {
            target: None,
            reason: NextTargetReason::NoTargetsConfigured,
            exposure: None,
            position_angle_degrees: None,
        };
        let v = serde_json::to_value(&rec).unwrap();
        assert_eq!(v["reason"], "no_targets_configured");
        assert!(v["target"].is_null());
        assert!(v["exposure"].is_null());
    }

    #[test]
    fn serialized_recommendation_nests_coord_and_hides_the_plan() {
        let rec = NextTargetRecommendation {
            target: Some(PlannerTarget {
                name: "M31".into(),
                coord: rp_targets::IcrsCoord::try_new(0.7, 41.0).unwrap(),
                min_altitude_degrees: Some(25.0),
                position_angle_degrees: None,
                exposures: vec![ExposureSpec {
                    filter: Some("Luminance".to_string()),
                    duration_secs: 300.0,
                    count: Some(1),
                }],
            }),
            reason: NextTargetReason::BestTransitingCandidate,
            exposure: Some(ExposureSpec {
                filter: Some("Red".to_string()),
                duration_secs: 120.0,
                count: Some(2),
            }),
            position_angle_degrees: Some(25.0),
        };
        let v = serde_json::to_value(&rec).unwrap();
        assert_eq!(v["reason"], "best_transiting_candidate");
        assert_eq!(v["target"]["name"], "M31");
        // The coordinate nests as an object, not flat ra/dec keys.
        assert_eq!(v["target"]["coord"]["ra_hours"], 0.7);
        assert_eq!(v["target"]["coord"]["dec_degrees"], 41.0);
        assert_eq!(v["target"]["min_altitude_degrees"], 25.0);
        // The full plan stays off the wire (the target carries identity
        // + coordinate only).
        assert!(
            v["target"].get("exposures").is_none(),
            "the wire target must not leak the plan: {v}"
        );
        // The effective angle is a top-level recommendation field; the
        // target's raw layer-one value stays off the wire.
        assert_eq!(v["position_angle_degrees"], 25.0);
        assert!(
            v["target"].get("position_angle_degrees").is_none(),
            "the wire target must not leak the raw angle: {v}"
        );
        // The selected exposure nests; the goal `count` is not surfaced.
        assert_eq!(v["exposure"]["filter"], "Red");
        assert_eq!(v["exposure"]["duration_secs"], 120.0);
        assert!(
            v["exposure"].get("count").is_none(),
            "the goal count is a decision input, not wire: {v}"
        );
    }

    #[test]
    fn serialized_unfiltered_exposure_leaves_filter_null() {
        let entry = ExposureSpec {
            filter: None,
            duration_secs: 60.0,
            count: None,
        };
        let rec = NextTargetRecommendation {
            target: Some(PlannerTarget {
                name: "OSC Field".into(),
                coord: rp_targets::IcrsCoord::try_new(0.7, 41.0).unwrap(),
                min_altitude_degrees: None,
                position_angle_degrees: None,
                exposures: vec![entry.clone()],
            }),
            reason: NextTargetReason::BestTransitingCandidate,
            exposure: Some(entry),
            position_angle_degrees: Some(0.0),
        };
        let v = serde_json::to_value(&rec).unwrap();
        assert!(v["exposure"]["filter"].is_null());
        assert_eq!(v["exposure"]["duration_secs"], 60.0);
    }
}
