//! The temperature fit (docs/services/focus-model.md §
//! `calibrate_temperature`).
//!
//! How far the focuser moves per degree, fitted by least squares over
//! the runs the train has already recorded. Nothing here moves, takes
//! a frame or reads a probe: the measurements were made on the nights
//! the runs were, and this reads them back.
//!
//! Filters are put on one scale before the fit, because two filters
//! focus in different places and a line drawn through both would read
//! that difference as temperature. The offset is only needed when the
//! runs mix filters: a constant shifts the line without tilting it.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use tracing::debug;

use crate::config::Config;
use crate::error::{FocusModelError, Result};
use crate::store::{FocusRecord, FocusStore};
use crate::workflow::{resolve_train, stale_fields, FocusRig};

/// Recorded runs the fit left out, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnusedRuns {
    /// What those runs did instead, in the past tense so the count
    /// reads in front of it: "7 did not confirm".
    pub why: String,
    pub runs: usize,
}

/// The `calibrate_temperature` result.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CalibrationView {
    pub train_id: String,
    /// The slope of the fitted line: focuser steps per °C.
    pub coefficient_steps_per_c: f64,
    /// How many runs the fit used.
    pub runs: usize,
    /// The temperature range those runs cover.
    pub span_c: f64,
    /// The root mean square of those runs about the fitted line — the
    /// night-to-night scatter, in focuser steps.
    pub residual_steps: f64,
    /// The filters the fitted runs were taken through, in name order;
    /// empty on a train with no wheel.
    pub filters: Vec<String>,
    /// The offsets the fit subtracted to put those filters on one
    /// scale, and what the coefficient stands or falls with: a later
    /// write that moves one of them drops the coefficient. Empty when
    /// the fit needed none.
    pub offsets_used: BTreeMap<String, i32>,
    /// The recorded runs the fit left out, by reason.
    pub unused: Vec<UnusedRuns>,
}

/// One run the fit can use.
#[derive(Debug, Clone)]
struct Candidate {
    temperature_c: f64,
    position: i32,
    filter: Option<String>,
}

/// One run on the reference scale: what it measured, and at what
/// temperature.
#[derive(Debug, Clone, Copy)]
struct Sample {
    temperature_c: f64,
    position: f64,
}

/// The candidate runs on one scale: the samples, the filters they
/// came through, and the offsets that put them there.
#[derive(Debug, Default)]
struct Scaled {
    samples: Vec<Sample>,
    filters: Vec<String>,
    offsets_used: BTreeMap<String, i32>,
}

/// A fitted line.
#[derive(Debug, Clone, Copy)]
struct Fit {
    coefficient: f64,
    residual: f64,
}

/// The runs the fit could not use, counted by reason. The fixed
/// reasons come first in the order a run is judged by them; the
/// missing offsets follow, sorted by their own text, so the same
/// record always accounts for itself the same way.
#[derive(Debug, Default)]
struct Excluded {
    unconfirmed: usize,
    no_position: usize,
    no_temperature: usize,
    no_offset: BTreeMap<String, usize>,
}

impl Excluded {
    /// Count a run that had no offset to place it against the others.
    fn no_offset_for(&mut self, filter: Option<&str>) {
        let why = filter.map_or_else(
            || "had no filter to take an offset from".to_owned(),
            |name| format!("had no offset for '{name}'"),
        );
        let count = self.no_offset.entry(why).or_insert(0);
        *count = count.saturating_add(1);
    }

    /// The reasons as the result reports them, empty ones dropped.
    fn into_unused(self) -> Vec<UnusedRuns> {
        let mut unused: Vec<UnusedRuns> = Vec::new();
        push_reason(&mut unused, "did not confirm".to_owned(), self.unconfirmed);
        push_reason(
            &mut unused,
            "recorded no position".to_owned(),
            self.no_position,
        );
        push_reason(
            &mut unused,
            "carried no temperature reading".to_owned(),
            self.no_temperature,
        );
        for (why, runs) in self.no_offset {
            push_reason(&mut unused, why, runs);
        }
        unused
    }
}

fn push_reason(unused: &mut Vec<UnusedRuns>, why: String, runs: usize) {
    if runs > 0 {
        unused.push(UnusedRuns { why, runs });
    }
}

/// One more run left out.
const fn one_more(count: &mut usize) {
    *count = count.saturating_add(1);
}

/// The runs a coefficient can be fitted from: the ones that confirmed,
/// carrying both a position and the temperature they were measured at.
fn candidates(record: &FocusRecord) -> (Vec<Candidate>, Excluded) {
    let mut candidates = Vec::new();
    let mut excluded = Excluded::default();
    for run in &record.runs {
        if !run.outcome.is_confirmed() {
            one_more(&mut excluded.unconfirmed);
            continue;
        }
        let Some(position) = run.position else {
            one_more(&mut excluded.no_position);
            continue;
        };
        let Some(temperature_c) = run.temperature_c else {
            one_more(&mut excluded.no_temperature);
            continue;
        };
        candidates.push(Candidate {
            temperature_c,
            position,
            filter: run.filter.clone(),
        });
    }
    (candidates, excluded)
}

/// Put the candidates on one scale, and name the filters they came
/// through.
///
/// Runs that all came through one filter need no offset at all: the
/// filter's own distance from the reference is a constant, and a
/// constant shifts the line without tilting it, so a train whose
/// offsets were never measured still has a coefficient. Once the runs
/// mix filters each needs its own offset, and one whose filter the
/// record has no offset for cannot be placed against the rest.
fn on_one_scale(
    record: &FocusRecord,
    candidates: Vec<Candidate>,
    excluded: &mut Excluded,
) -> Scaled {
    let mixed = candidates
        .iter()
        .map(|candidate| candidate.filter.as_deref())
        .collect::<BTreeSet<Option<&str>>>()
        .len()
        > 1;
    let mut samples = Vec::new();
    let mut filters = BTreeSet::new();
    let mut offsets_used = BTreeMap::new();
    for candidate in candidates {
        let offset = if mixed {
            let Some(known) = record.offset_for(candidate.filter.as_deref()) else {
                excluded.no_offset_for(candidate.filter.as_deref());
                continue;
            };
            known
        } else {
            0
        };
        if let Some(name) = candidate.filter {
            if mixed {
                offsets_used.insert(name.clone(), offset);
            }
            filters.insert(name);
        }
        samples.push(Sample {
            temperature_c: candidate.temperature_c,
            position: f64::from(candidate.position) - f64::from(offset),
        });
    }
    Scaled {
        samples,
        filters: filters.into_iter().collect(),
        offsets_used,
    }
}

/// The temperature range the samples cover.
fn span_c(samples: &[Sample]) -> f64 {
    let mut lowest = f64::INFINITY;
    let mut highest = f64::NEG_INFINITY;
    for sample in samples {
        lowest = lowest.min(sample.temperature_c);
        highest = highest.max(sample.temperature_c);
    }
    highest - lowest
}

/// The least-squares line through the samples: its slope in steps per
/// °C, and the root mean square of the samples about it.
///
/// `None` when the temperatures are too close together to give a
/// finite slope. The span check ahead of this rules that out at any
/// sane threshold, but one small enough underflows the sum of squares
/// the slope divides by.
fn fit(samples: &[Sample]) -> Option<Fit> {
    let mut n = 0.0_f64;
    let mut sum_t = 0.0_f64;
    let mut sum_p = 0.0_f64;
    for sample in samples {
        n += 1.0;
        sum_t += sample.temperature_c;
        sum_p += sample.position;
    }
    let mean_t = sum_t / n;
    let mean_p = sum_p / n;
    let mut stt = 0.0_f64;
    let mut stp = 0.0_f64;
    for sample in samples {
        let dt = sample.temperature_c - mean_t;
        stt = dt.mul_add(dt, stt);
        stp = dt.mul_add(sample.position - mean_p, stp);
    }
    let coefficient = stp / stt;
    if !coefficient.is_finite() {
        return None;
    }
    let intercept = coefficient.mul_add(-mean_t, mean_p);
    let mut squares = 0.0_f64;
    for sample in samples {
        let residual = sample.position - coefficient.mul_add(sample.temperature_c, intercept);
        squares = residual.mul_add(residual, squares);
    }
    Some(Fit {
        coefficient,
        residual: (squares / n).sqrt(),
    })
}

/// The account a refusal gives of the runs it could not use: ": of
/// the 12 recorded, 7 did not confirm and 2 carried no temperature
/// reading", and nothing at all when it used every run there was.
fn left_out(recorded: usize, unused: &[UnusedRuns]) -> String {
    let mut parts: Vec<String> = unused
        .iter()
        .map(|entry| format!("{} {}", entry.runs, entry.why))
        .collect();
    let Some(last) = parts.pop() else {
        return String::new();
    };
    let counted = if parts.is_empty() {
        last
    } else {
        format!("{} and {last}", parts.join(", "))
    };
    format!(": of the {recorded} recorded, {counted}")
}

/// Fit the record, or refuse naming the threshold it fell short of.
fn fit_record(record: &FocusRecord, config: &Config, train_id: &str) -> Result<CalibrationView> {
    let recorded = record.runs.len();
    let (candidates, mut excluded) = candidates(record);
    let scaled = on_one_scale(record, candidates, &mut excluded);
    let samples = scaled.samples;
    let runs = samples.len();

    let needed = config.min_calibration_runs.get();
    if runs < needed {
        let account = left_out(recorded, &excluded.into_unused());
        return Err(FocusModelError::Workflow(format!(
            "a coefficient needs {needed} runs (min_calibration_runs) \
             and train '{train_id}' has {runs}{account}"
        )));
    }

    let span = span_c(&samples);
    let narrowest = config.min_calibration_span_c.get();
    if span < narrowest {
        return Err(FocusModelError::Workflow(format!(
            "a coefficient needs a temperature span of {narrowest} °C \
             (min_calibration_span_c) and the {runs} runs of train '{train_id}' span {span} °C"
        )));
    }

    let Some(fitted) = fit(&samples) else {
        return Err(FocusModelError::Workflow(format!(
            "the {runs} runs of train '{train_id}' do not fit a line: their temperatures \
             are too close together to give a finite coefficient"
        )));
    };

    Ok(CalibrationView {
        train_id: train_id.to_owned(),
        coefficient_steps_per_c: fitted.coefficient,
        runs,
        span_c: span,
        residual_steps: fitted.residual,
        filters: scaled.filters,
        offsets_used: scaled.offsets_used,
        unused: excluded.into_unused(),
    })
}

/// The `calibrate_temperature` body: fit the record's own runs and
/// write the coefficient.
///
/// The fit and the write are one critical section of the store, so the
/// coefficient describes the record it lands on. A stale record is
/// refused rather than fitted: no run measured through another camera
/// or focuser describes this rig, and a fit is exactly the place that
/// would launder them into one number.
///
/// # Errors
///
/// Returns the resolution errors of [`resolve_train`], a
/// [`FocusModelError::Workflow`] for a train with no record, a stale
/// record, too few runs, too narrow a span or runs no line fits, and
/// the store's.
pub async fn calibrate_temperature(
    rig: &dyn FocusRig,
    store: &FocusStore,
    config: &Config,
    train_id: &str,
) -> Result<CalibrationView> {
    let ctx = resolve_train(rig, train_id).await?;
    let (_, view) = store
        .update(train_id, move |held| {
            let Some(mut record) = held else {
                return Err(FocusModelError::Workflow(format!(
                    "train '{train_id}' has no focus model"
                )));
            };
            let stale = stale_fields(Some(&record), &ctx);
            if !stale.is_empty() {
                return Err(FocusModelError::Workflow(format!(
                    "train '{train_id}' has a stale focus model: {}; no run measured \
                     through the old rig fits this one",
                    stale.join("; ")
                )));
            }
            let view = fit_record(&record, config, train_id)?;
            record.set_temperature_coefficient(
                Some(view.coefficient_steps_per_c),
                view.runs,
                view.span_c,
                view.offsets_used.clone(),
            );
            Ok::<_, FocusModelError>((record, view))
        })
        .await?;
    debug!(
        train_id,
        coefficient = view.coefficient_steps_per_c,
        runs = view.runs,
        span_c = view.span_c,
        "the temperature coefficient was fitted"
    );
    Ok(view)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::sizing::SweepSource;
    use crate::store::{FocusRun, RunOutcome};
    use crate::workflow::{MockFocusRig, TrainInfo};

    /// The defaults: five runs over three degrees.
    const DEFAULTS: &str = r#"{ "mcp_server_url": "http://127.0.0.1:1/mcp" }"#;
    /// Three runs is enough, so a test can say what it means with
    /// three of them.
    const THREE_RUNS: &str = r#"{
        "mcp_server_url": "http://127.0.0.1:1/mcp", "min_calibration_runs": 3
    }"#;

    fn config(json: &str) -> Config {
        crate::config::parse_config(json, "test").unwrap()
    }

    async fn temp_store() -> (FocusStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = FocusStore::open(dir.path().join("focus.redb"))
            .await
            .unwrap();
        (store, dir)
    }

    fn wheel_filters() -> Vec<String> {
        ["Luminance", "Ha", "OIII"]
            .iter()
            .map(|name| (*name).to_owned())
            .collect()
    }

    fn train_info(wheel: bool) -> TrainInfo {
        TrainInfo {
            camera_id: Some("main-cam".to_owned()),
            filter_wheel_id: wheel.then(|| "main-fw".to_owned()),
            filters: wheel.then(wheel_filters),
            terminal_focuser_id: Some("main-focuser".to_owned()),
            ..TrainInfo::default()
        }
    }

    /// A rig that answers `get_train_info` for the reference train and
    /// nothing else: the fit reads the store, not the sky.
    fn rig(wheel: bool) -> MockFocusRig {
        let mut rig = MockFocusRig::new();
        rig.expect_get_train_info()
            .returning(move |_| Box::pin(async move { Ok(train_info(wheel)) }));
        rig
    }

    /// A run that confirmed at `position`, measured at `temperature_c`.
    fn confirmed(day: u32, filter: Option<&str>, temperature_c: f64, position: i32) -> FocusRun {
        let mut run = FocusRun::new(
            format!("2026-09-{day:02}T20:00:00Z"),
            filter.map(str::to_owned),
            RunOutcome::Confirmed,
            17,
            68,
            SweepSource::Derived,
        );
        run.position = Some(position);
        run.temperature_c = Some(temperature_c);
        run.hfr = Some(1.2);
        run
    }

    /// A run that measured nothing.
    fn failed(day: u32) -> FocusRun {
        FocusRun::new(
            format!("2026-09-{day:02}T20:00:00Z"),
            Some("Luminance".to_owned()),
            RunOutcome::NotEnoughStars,
            17,
            68,
            SweepSource::Derived,
        )
    }

    fn record_of(runs: Vec<FocusRun>, wheel: bool) -> FocusRecord {
        let mut record = FocusRecord::new(
            "main",
            Some("main-focuser"),
            Some("main-cam"),
            wheel.then(wheel_filters),
        );
        for run in runs {
            record.push_run(run, 500);
        }
        record
    }

    /// Five nights three degrees apart, sixty steps lower each time.
    fn a_line_of_five() -> Vec<FocusRun> {
        vec![
            confirmed(1, Some("Luminance"), 5.0, 24_950),
            confirmed(2, Some("Luminance"), 8.0, 24_890),
            confirmed(3, Some("Luminance"), 11.0, 24_830),
            confirmed(4, Some("Luminance"), 14.0, 24_770),
            confirmed(5, Some("Luminance"), 17.0, 24_710),
        ]
    }

    async fn stored(runs: Vec<FocusRun>) -> (FocusStore, tempfile::TempDir) {
        let (store, dir) = temp_store().await;
        store.put(record_of(runs, true)).await.unwrap();
        (store, dir)
    }

    #[tokio::test]
    async fn a_line_through_the_recorded_runs_is_the_coefficient() {
        let (store, _dir) = stored(a_line_of_five()).await;

        let view = calibrate_temperature(&rig(true), &store, &config(DEFAULTS), "main")
            .await
            .unwrap();

        assert_eq!(view.coefficient_steps_per_c, -20.0);
        assert_eq!(view.runs, 5);
        assert_eq!(view.span_c, 12.0);
        assert_eq!(view.residual_steps, 0.0);
        assert_eq!(view.filters, ["Luminance"]);
        assert_eq!(view.unused, Vec::new());
        // One filter's own distance from the reference is a constant
        // the slope never saw, so the fit rests on no offset.
        assert_eq!(view.offsets_used, BTreeMap::new());
    }

    #[tokio::test]
    async fn the_coefficient_reaches_the_record_with_its_run_count_and_span() {
        let (store, _dir) = stored(a_line_of_five()).await;

        calibrate_temperature(&rig(true), &store, &config(DEFAULTS), "main")
            .await
            .unwrap();

        let record = store.get("main").await.unwrap().unwrap();
        assert_eq!(record.temperature_coefficient, Some(-20.0));
        assert_eq!(record.coefficient_runs, Some(5));
        assert_eq!(record.coefficient_span_c, Some(12.0));
        // The coefficient and nothing else: the runs it was fitted
        // from stay where they are.
        assert_eq!(record.runs.len(), 5);
    }

    /// The scatter about the line, not the scatter of the runs: five
    /// runs a step either side of a -10.2 line sit 0.85 steps from it.
    #[tokio::test]
    async fn the_residual_is_the_spread_about_the_fitted_line() {
        let (store, _dir) = stored(vec![
            confirmed(1, Some("Luminance"), 0.0, 101),
            confirmed(2, Some("Luminance"), 1.0, 89),
            confirmed(3, Some("Luminance"), 2.0, 81),
            confirmed(4, Some("Luminance"), 3.0, 69),
            confirmed(5, Some("Luminance"), 4.0, 60),
        ])
        .await;

        let view = calibrate_temperature(&rig(true), &store, &config(DEFAULTS), "main")
            .await
            .unwrap();

        assert!(
            (view.coefficient_steps_per_c - -10.2).abs() < 1e-9,
            "{view:?}"
        );
        assert!(
            (view.residual_steps - 0.848_528_137_423_857).abs() < 1e-9,
            "{view:?}"
        );
    }

    /// Two filters focus in different places, and the offsets are what
    /// put them on one scale: the Ha runs sit 46 steps out and fit the
    /// same line once that is taken off.
    #[tokio::test]
    async fn the_offsets_put_two_filters_on_one_scale() {
        let mut record = record_of(
            vec![
                confirmed(1, Some("Luminance"), 5.0, 24_950),
                confirmed(2, Some("Ha"), 8.0, 24_936),
                confirmed(3, Some("Luminance"), 11.0, 24_830),
                confirmed(4, Some("Ha"), 14.0, 24_816),
                confirmed(5, Some("Luminance"), 17.0, 24_710),
            ],
            true,
        );
        record.set_offsets(Some("Luminance"), [("Ha".to_owned(), 46)].into());
        let (store, _dir) = temp_store().await;
        store.put(record).await.unwrap();

        let view = calibrate_temperature(&rig(true), &store, &config(DEFAULTS), "main")
            .await
            .unwrap();

        assert_eq!(view.coefficient_steps_per_c, -20.0);
        assert_eq!(view.residual_steps, 0.0);
        assert_eq!(view.filters, ["Ha", "Luminance"]);
        // What the fit subtracted is what it stands on: moving either
        // of these drops the coefficient.
        assert_eq!(
            view.offsets_used,
            [("Ha".to_owned(), 46), ("Luminance".to_owned(), 0)].into()
        );
        let record = store.get("main").await.unwrap().unwrap();
        assert_eq!(record.coefficient_offsets, view.offsets_used);
    }

    /// One filter needs no offset at all: its own distance from the
    /// reference is a constant, and a constant shifts the line without
    /// tilting it. A train that never measured an offset still
    /// calibrates.
    #[tokio::test]
    async fn runs_all_on_one_unmeasured_filter_need_no_offset() {
        let (store, _dir) = stored(vec![
            confirmed(1, Some("OIII"), 5.0, 25_150),
            confirmed(2, Some("OIII"), 11.0, 25_030),
            confirmed(3, Some("OIII"), 17.0, 24_910),
        ])
        .await;

        let view = calibrate_temperature(&rig(true), &store, &config(THREE_RUNS), "main")
            .await
            .unwrap();

        assert_eq!(view.coefficient_steps_per_c, -20.0);
        assert_eq!(view.filters, ["OIII"]);
        assert_eq!(view.unused, Vec::new());
    }

    /// Once the runs mix filters, one the record cannot place against
    /// the others is left out and named.
    #[tokio::test]
    async fn a_filter_with_no_offset_is_left_out_of_a_mixed_fit() {
        let mut record = record_of(
            vec![
                confirmed(1, Some("Luminance"), 5.0, 24_950),
                confirmed(2, Some("Luminance"), 11.0, 24_830),
                confirmed(3, Some("Luminance"), 17.0, 24_710),
                confirmed(4, Some("OIII"), 8.0, 25_090),
            ],
            true,
        );
        record.set_offsets(Some("Luminance"), BTreeMap::new());
        let (store, _dir) = temp_store().await;
        store.put(record).await.unwrap();

        let view = calibrate_temperature(&rig(true), &store, &config(THREE_RUNS), "main")
            .await
            .unwrap();

        assert_eq!(view.runs, 3);
        assert_eq!(view.filters, ["Luminance"]);
        assert_eq!(
            view.unused,
            vec![UnusedRuns {
                why: "had no offset for 'OIII'".to_owned(),
                runs: 1,
            }]
        );
    }

    /// A run taken with no filter at all, among runs that had one, has
    /// nothing to take an offset from either.
    #[tokio::test]
    async fn a_filterless_run_among_filtered_ones_is_left_out() {
        let mut record = record_of(
            vec![
                confirmed(1, Some("Luminance"), 5.0, 24_950),
                confirmed(2, Some("Luminance"), 11.0, 24_830),
                confirmed(3, Some("Luminance"), 17.0, 24_710),
                confirmed(4, None, 8.0, 24_890),
            ],
            true,
        );
        record.set_offsets(Some("Luminance"), BTreeMap::new());
        let (store, _dir) = temp_store().await;
        store.put(record).await.unwrap();

        let view = calibrate_temperature(&rig(true), &store, &config(THREE_RUNS), "main")
            .await
            .unwrap();

        assert_eq!(
            view.unused,
            vec![UnusedRuns {
                why: "had no filter to take an offset from".to_owned(),
                runs: 1,
            }]
        );
    }

    /// A train with no wheel has no filters to report and no offsets
    /// to need.
    #[tokio::test]
    async fn a_train_without_a_wheel_fits_its_filterless_runs() {
        let (store, _dir) = temp_store().await;
        store
            .put(record_of(
                vec![
                    confirmed(1, None, 5.0, 24_950),
                    confirmed(2, None, 11.0, 24_830),
                    confirmed(3, None, 17.0, 24_710),
                ],
                false,
            ))
            .await
            .unwrap();

        let view = calibrate_temperature(&rig(false), &store, &config(THREE_RUNS), "main")
            .await
            .unwrap();

        assert_eq!(view.coefficient_steps_per_c, -20.0);
        assert_eq!(view.filters, Vec::<String>::new());
    }

    /// A fallback run measured a position but not a trusted fit, and a
    /// failed one measured nothing: neither teaches the model where
    /// focus is, so neither is fitted.
    #[tokio::test]
    async fn a_run_that_did_not_confirm_is_counted_rather_than_fitted() {
        let mut fallback = confirmed(4, Some("Luminance"), 20.0, 24_650);
        fallback.outcome = RunOutcome::Fallback;
        let mut runs = vec![
            confirmed(1, Some("Luminance"), 5.0, 24_950),
            confirmed(2, Some("Luminance"), 11.0, 24_830),
            confirmed(3, Some("Luminance"), 17.0, 24_710),
        ];
        runs.push(fallback);
        runs.push(failed(5));
        let (store, _dir) = stored(runs).await;

        let view = calibrate_temperature(&rig(true), &store, &config(THREE_RUNS), "main")
            .await
            .unwrap();

        assert_eq!(view.runs, 3);
        assert_eq!(
            view.unused,
            vec![UnusedRuns {
                why: "did not confirm".to_owned(),
                runs: 2,
            }]
        );
    }

    /// A focuser with no probe records the run without a temperature,
    /// and a coefficient is exactly what such a run cannot inform.
    #[tokio::test]
    async fn a_run_without_a_reading_or_a_position_is_counted_rather_than_fitted() {
        let mut unread = confirmed(4, Some("Luminance"), 20.0, 24_650);
        unread.temperature_c = None;
        let mut unplaced = confirmed(5, Some("Luminance"), 23.0, 24_590);
        unplaced.position = None;
        let mut runs = vec![
            confirmed(1, Some("Luminance"), 5.0, 24_950),
            confirmed(2, Some("Luminance"), 11.0, 24_830),
            confirmed(3, Some("Luminance"), 17.0, 24_710),
        ];
        runs.push(unread);
        runs.push(unplaced);
        let (store, _dir) = stored(runs).await;

        let view = calibrate_temperature(&rig(true), &store, &config(THREE_RUNS), "main")
            .await
            .unwrap();

        assert_eq!(view.runs, 3);
        assert_eq!(
            view.unused,
            vec![
                UnusedRuns {
                    why: "recorded no position".to_owned(),
                    runs: 1,
                },
                UnusedRuns {
                    why: "carried no temperature reading".to_owned(),
                    runs: 1,
                },
            ]
        );
    }

    /// The refusal names the threshold and accounts for every run the
    /// record does hold, so an operator reads what the history is
    /// short of rather than a bare number.
    #[tokio::test]
    async fn too_few_runs_names_the_threshold_and_the_runs_it_could_not_use() {
        let mut runs = vec![
            confirmed(1, Some("Luminance"), 5.0, 24_950),
            confirmed(2, Some("Luminance"), 11.0, 24_830),
            confirmed(3, Some("Luminance"), 17.0, 24_710),
        ];
        for day in 4..=10 {
            runs.push(failed(day));
        }
        for day in 11..=12 {
            let mut unread = confirmed(day, Some("Luminance"), 20.0, 24_650);
            unread.temperature_c = None;
            runs.push(unread);
        }
        let (store, _dir) = stored(runs).await;

        let error = calibrate_temperature(&rig(true), &store, &config(DEFAULTS), "main")
            .await
            .unwrap_err();

        assert_eq!(
            error.tool_message(),
            "a coefficient needs 5 runs (min_calibration_runs) and train 'main' has 3: \
             of the 12 recorded, 7 did not confirm and 2 carried no temperature reading"
        );
    }

    #[tokio::test]
    async fn a_history_that_never_left_one_temperature_names_the_span_it_needs() {
        let (store, _dir) = stored(vec![
            confirmed(1, Some("Luminance"), 10.0, 24_950),
            confirmed(2, Some("Luminance"), 11.0, 24_930),
            confirmed(3, Some("Luminance"), 12.0, 24_910),
        ])
        .await;

        let error = calibrate_temperature(&rig(true), &store, &config(THREE_RUNS), "main")
            .await
            .unwrap_err();

        assert_eq!(
            error.tool_message(),
            "a coefficient needs a temperature span of 3 °C (min_calibration_span_c) \
             and the 3 runs of train 'main' span 2 °C"
        );
    }

    /// The span check is the one that keeps a line fittable. A
    /// threshold small enough to pass runs a hair apart leaves a slope
    /// nothing can divide, and that is reported rather than written.
    #[tokio::test]
    async fn temperatures_a_hair_apart_fit_no_line() {
        let lenient = r#"{
            "mcp_server_url": "http://127.0.0.1:1/mcp",
            "min_calibration_runs": 3, "min_calibration_span_c": 1e-300
        }"#;
        let (store, _dir) = stored(vec![
            confirmed(1, Some("Luminance"), 0.0, 1000),
            confirmed(2, Some("Luminance"), 1e-300, 990),
            confirmed(3, Some("Luminance"), 2e-300, 980),
        ])
        .await;

        let error = calibrate_temperature(&rig(true), &store, &config(lenient), "main")
            .await
            .unwrap_err();

        assert_eq!(
            error.tool_message(),
            "the 3 runs of train 'main' do not fit a line: their temperatures \
             are too close together to give a finite coefficient"
        );
        let record = store.get("main").await.unwrap().unwrap();
        assert_eq!(record.temperature_coefficient, None);
    }

    /// Tenet 6: a record whose camera moved on predicts nothing, and a
    /// fit is exactly the place that would launder its runs into one
    /// number.
    #[tokio::test]
    async fn a_stale_record_is_refused_and_keeps_the_coefficient_it_had() {
        let mut record = record_of(a_line_of_five(), true);
        record.camera_id = Some("retired-cam".to_owned());
        record.set_temperature_coefficient(Some(-15.0), 9, 7.0, BTreeMap::new());
        let (store, _dir) = temp_store().await;
        store.put(record).await.unwrap();

        let error = calibrate_temperature(&rig(true), &store, &config(DEFAULTS), "main")
            .await
            .unwrap_err();

        assert_eq!(
            error.tool_message(),
            "train 'main' has a stale focus model: camera_id changed from retired-cam \
             to main-cam; no run measured through the old rig fits this one"
        );
        let record = store.get("main").await.unwrap().unwrap();
        assert_eq!(record.temperature_coefficient, Some(-15.0));
    }

    #[tokio::test]
    async fn a_refusal_leaves_the_coefficient_the_record_already_had() {
        let mut record = record_of(vec![confirmed(1, Some("Luminance"), 5.0, 24_950)], true);
        record.set_temperature_coefficient(Some(-15.0), 9, 7.0, BTreeMap::new());
        let (store, _dir) = temp_store().await;
        store.put(record).await.unwrap();

        calibrate_temperature(&rig(true), &store, &config(DEFAULTS), "main")
            .await
            .unwrap_err();

        let record = store.get("main").await.unwrap().unwrap();
        assert_eq!(record.temperature_coefficient, Some(-15.0));
        assert_eq!(record.coefficient_runs, Some(9));
        assert_eq!(record.coefficient_span_c, Some(7.0));
    }

    #[tokio::test]
    async fn a_train_with_no_record_is_refused_naming_it() {
        let (store, _dir) = temp_store().await;

        let error = calibrate_temperature(&rig(true), &store, &config(DEFAULTS), "main")
            .await
            .unwrap_err();

        assert_eq!(error.tool_message(), "train 'main' has no focus model");
    }

    #[tokio::test]
    async fn a_train_the_rig_cannot_resolve_is_the_rigs_error() {
        let (store, _dir) = temp_store().await;
        let mut rig = MockFocusRig::new();
        rig.expect_get_train_info()
            .returning(|_| Box::pin(async { Ok(TrainInfo::default()) }));

        let error = calibrate_temperature(&rig, &store, &config(DEFAULTS), "main")
            .await
            .unwrap_err();

        assert_eq!(error.tool_message(), "train 'main' has no terminal focuser");
    }
}
