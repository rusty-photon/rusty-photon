//! The predicted start (docs/services/focus-model.md § The predicted
//! start).
//!
//! Where a sweep should begin, from what the record remembers: the most
//! recent confirmed focus, moved by the filter offset and the
//! temperature term. Every term the record cannot supply is omitted and
//! named, and a prediction too close to the current position — or
//! outside the focuser's travel — is reported rather than moved to.

use serde::{Deserialize, Serialize};

use crate::store::{FocusRecord, LastGood};

/// The terms that made up a predicted start. Each is null when the
/// record could not supply it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct PredictionTerms {
    /// The anchor's position — the most recent confirmed focus.
    pub last_good: Option<i32>,
    /// The net filter term: the target filter's offset less the
    /// anchor's.
    pub offset: Option<i32>,
    /// The temperature term in steps, before rounding.
    pub temperature: Option<f64>,
}

/// What the provider predicted, and what it did about it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Prediction {
    /// Where the focuser was when the call started.
    pub from_position: i32,
    /// The predicted start; null when the record supplied no anchor.
    pub start: Option<i32>,
    pub terms: PredictionTerms,
    /// The terms the record could not supply, in a fixed order.
    pub missing: Vec<String>,
    /// Whether the focuser was moved to `start`.
    pub moved: bool,
    /// Why a prediction was not moved to; null when it was, or when
    /// there was none.
    pub skipped: Option<String>,
}

impl Prediction {
    /// No record, no prediction: the sweep starts where the focuser is.
    #[must_use]
    pub fn none_from(from_position: i32, missing: Vec<String>) -> Self {
        Self {
            from_position,
            start: None,
            terms: PredictionTerms::default(),
            missing,
            moved: false,
            skipped: None,
        }
    }
}

/// The focuser travel a prediction must land inside.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Bounds {
    pub min: Option<i32>,
    pub max: Option<i32>,
}

impl Bounds {
    #[must_use]
    pub const fn contains(self, position: i32) -> bool {
        let above = match self.min {
            Some(min) => position >= min,
            None => true,
        };
        let below = match self.max {
            Some(max) => position <= max,
            None => true,
        };
        above && below
    }

    /// How the bounds read in a `skipped` message.
    #[must_use]
    pub fn describe(self) -> String {
        let min = self.min.map_or_else(|| "-".to_owned(), |v| v.to_string());
        let max = self.max.map_or_else(|| "-".to_owned(), |v| v.to_string());
        format!("[{min}, {max}]")
    }
}

/// `value.round()` as a step count.
const fn round_steps(value: f64) -> i32 {
    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        reason = "`f64` to `i32` has no total spelling; `as` saturates at the rails and maps NaN to 0, and the caller sums in `i64` so a saturated term cannot wrap"
    )]
    let steps = value.round() as i32;
    steps
}

/// Whether two filter slots are the same one.
fn same_filter(a: Option<&str>, b: Option<&str>) -> bool {
    a == b
}

/// Predict where the sweep should start.
///
/// `min_move` is the smallest move worth making — half a critical focus
/// zone when the optics are known, the configured `min_prediction_move`
/// otherwise. `temperature_now` is the reading taken at the start of
/// the call.
#[must_use]
pub fn predict(
    record: Option<&FocusRecord>,
    filter: Option<&str>,
    from_position: i32,
    temperature_now: Option<f64>,
    bounds: Bounds,
    min_move: i32,
) -> Prediction {
    let Some(record) = record else {
        return Prediction::none_from(from_position, vec!["last_good".to_owned()]);
    };
    let Some(anchor) = record.most_recent_last_good() else {
        return Prediction::none_from(from_position, vec!["last_good".to_owned()]);
    };

    let mut missing: Vec<String> = Vec::new();
    let (anchor, offset_term): (&LastGood, i32) = match filter_term(record, anchor, filter) {
        Some(term) => (anchor, term),
        None => match record.last_good_for(filter) {
            // The target filter's own entry needs no offset term.
            Some(own) => (own, 0),
            None => {
                return Prediction::none_from(from_position, vec!["offset".to_owned()]);
            }
        },
    };

    let temperature_term = match (
        record.temperature_coefficient,
        temperature_now,
        anchor.temperature_c,
    ) {
        (Some(coefficient), Some(now), Some(then)) => Some(coefficient * (now - then)),
        (None, _, _) => {
            missing.push("temperature_coefficient".to_owned());
            None
        }
        _ => {
            missing.push("temperature".to_owned());
            None
        }
    };

    let terms = PredictionTerms {
        last_good: Some(anchor.position),
        offset: Some(offset_term),
        temperature: temperature_term,
    };

    // Summed in `i64`: a hand-entered offset or a large temperature
    // term must not wrap into a position the bounds check would then
    // accept, and a focuser with no reported bounds has no check at
    // all.
    let start = i64::from(anchor.position)
        .checked_add(i64::from(offset_term))
        .and_then(|sum| sum.checked_add(i64::from(temperature_term.map_or(0, round_steps))))
        .and_then(|sum| i32::try_from(sum).ok());
    let Some(start) = start else {
        return Prediction {
            from_position,
            start: None,
            terms,
            missing,
            moved: false,
            skipped: Some("the predicted start is outside the focuser's range".to_owned()),
        };
    };

    let skipped = if !bounds.contains(start) {
        Some(format!(
            "outside the focuser's bounds {}",
            bounds.describe()
        ))
    } else if start.saturating_sub(from_position).saturating_abs() < min_move.max(1) {
        Some(format!("within {min_move} steps of the current position"))
    } else {
        None
    };

    Prediction {
        from_position,
        start: Some(start),
        terms,
        missing,
        moved: skipped.is_none(),
        skipped,
    }
}

/// The net filter term between the anchor's filter and the target one,
/// or `None` when an offset the term needs is missing.
fn filter_term(record: &FocusRecord, anchor: &LastGood, filter: Option<&str>) -> Option<i32> {
    if same_filter(anchor.filter.as_deref(), filter) {
        return Some(0);
    }
    let target = record.offset_for(filter)?;
    let anchor_offset = record.offset_for(anchor.filter.as_deref())?;
    Some(target.saturating_sub(anchor_offset))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::store::FocusRecord;

    fn record() -> FocusRecord {
        FocusRecord::new("main", Some("main-focuser"), Some("main-cam"), None)
    }

    fn last_good(
        filter: Option<&str>,
        position: i32,
        temperature_c: Option<f64>,
        at: &str,
    ) -> LastGood {
        LastGood {
            filter: filter.map(str::to_owned),
            position,
            temperature_c,
            hfr: 1.2,
            at: at.to_owned(),
        }
    }

    const BOUNDS: Bounds = Bounds {
        min: Some(0),
        max: Some(60_000),
    };

    /// A hand-entered offset near the rail must not wrap into a
    /// position an unbounded focuser would then be sent to.
    #[test]
    fn a_term_that_overflows_predicts_nothing() {
        let mut record = FocusRecord::new(
            "main",
            Some("main-focuser"),
            Some("main-cam"),
            Some(vec!["L".to_owned(), "Ha".to_owned()]),
        );
        record.set_offsets(Some("L"), [("Ha".to_owned(), i32::MAX)].into());
        record.set_last_good(last_good(
            Some("L"),
            i32::MAX - 1,
            None,
            "2026-09-10T22:00:00Z",
        ));

        let prediction = predict(Some(&record), Some("Ha"), 10, None, Bounds::default(), 5);
        assert_eq!(prediction.start, None);
        assert!(!prediction.moved);
        assert_eq!(
            prediction.skipped.as_deref(),
            Some("the predicted start is outside the focuser's range")
        );
    }

    #[test]
    fn no_record_means_no_prediction() {
        let prediction = predict(None, Some("L"), 29_740, Some(10.0), BOUNDS, 5);
        assert_eq!(prediction.start, None);
        assert_eq!(prediction.missing, ["last_good"]);
        assert!(!prediction.moved);
    }

    #[test]
    fn an_empty_record_means_no_prediction() {
        let prediction = predict(Some(&record()), None, 100, None, BOUNDS, 5);
        assert_eq!(prediction.start, None);
        assert_eq!(prediction.missing, ["last_good"]);
    }

    #[test]
    fn the_same_filter_predicts_its_own_last_good() {
        let mut record = record();
        record.set_last_good(last_good(
            Some("L"),
            29_766,
            Some(12.0),
            "2026-09-10T22:00:00Z",
        ));
        let prediction = predict(Some(&record), Some("L"), 29_740, Some(12.0), BOUNDS, 5);
        assert_eq!(prediction.start, Some(29_766));
        assert_eq!(prediction.terms.last_good, Some(29_766));
        assert_eq!(prediction.terms.offset, Some(0));
        assert_eq!(prediction.missing, ["temperature_coefficient"]);
        assert!(prediction.moved);
    }

    #[test]
    fn another_filters_anchor_carries_the_offset_difference() {
        let mut record = record();
        record.set_last_good(last_good(
            Some("L"),
            29_766,
            Some(12.0),
            "2026-09-10T22:00:00Z",
        ));
        record.set_offsets(
            Some("L"),
            [("L".to_owned(), 0), ("Ha".to_owned(), 46)].into(),
        );
        let prediction = predict(Some(&record), Some("Ha"), 29_740, Some(12.0), BOUNDS, 5);
        assert_eq!(prediction.terms.offset, Some(46));
        assert_eq!(prediction.start, Some(29_812));
    }

    #[test]
    fn a_missing_offset_falls_back_to_the_filters_own_last_good() {
        let mut record = record();
        record.set_last_good(last_good(
            Some("L"),
            29_766,
            Some(12.0),
            "2026-09-10T22:00:00Z",
        ));
        record.set_last_good(last_good(
            Some("Ha"),
            29_800,
            Some(12.0),
            "2026-09-09T22:00:00Z",
        ));
        // No offsets are recorded, so the cross-filter term cannot be
        // formed; Ha's own entry anchors instead.
        let prediction = predict(Some(&record), Some("Ha"), 29_740, Some(12.0), BOUNDS, 5);
        assert_eq!(prediction.terms.last_good, Some(29_800));
        assert_eq!(prediction.terms.offset, Some(0));
        assert_eq!(prediction.start, Some(29_800));
    }

    #[test]
    fn a_missing_offset_without_the_filters_own_entry_predicts_nothing() {
        let mut record = record();
        record.set_last_good(last_good(
            Some("L"),
            29_766,
            Some(12.0),
            "2026-09-10T22:00:00Z",
        ));
        let prediction = predict(Some(&record), Some("Ha"), 29_740, Some(12.0), BOUNDS, 5);
        assert_eq!(prediction.start, None);
        assert_eq!(prediction.missing, ["offset"]);
    }

    #[test]
    fn the_temperature_term_moves_the_start_and_rounds_to_a_step() {
        let mut record = record();
        record.set_last_good(last_good(
            Some("L"),
            29_766,
            Some(12.0),
            "2026-09-10T22:00:00Z",
        ));
        record.set_temperature_coefficient(Some(-7.4), 6, 4.5);
        let prediction = predict(Some(&record), Some("L"), 29_600, Some(7.0), BOUNDS, 5);
        // −7.4 steps/°C over −5 °C = +37 steps.
        assert_eq!(prediction.terms.temperature, Some(37.0));
        assert_eq!(prediction.start, Some(29_803));
        assert!(prediction.missing.is_empty(), "{:?}", prediction.missing);
    }

    #[test]
    fn a_missing_reading_leaves_the_temperature_term_out_by_name() {
        let mut record = record();
        record.set_last_good(last_good(Some("L"), 29_766, None, "2026-09-10T22:00:00Z"));
        record.set_temperature_coefficient(Some(-7.4), 6, 4.5);
        let prediction = predict(Some(&record), Some("L"), 29_600, Some(7.0), BOUNDS, 5);
        assert_eq!(prediction.missing, ["temperature"]);
        assert_eq!(prediction.start, Some(29_766));
    }

    #[test]
    fn the_most_recent_entry_anchors_whatever_its_filter() {
        let mut record = record();
        record.set_last_good(last_good(
            Some("L"),
            29_766,
            Some(12.0),
            "2026-09-01T22:00:00Z",
        ));
        record.set_last_good(last_good(
            Some("Ha"),
            29_812,
            Some(12.0),
            "2026-09-10T22:00:00Z",
        ));
        record.set_offsets(
            Some("L"),
            [("L".to_owned(), 0), ("Ha".to_owned(), 46)].into(),
        );
        let prediction = predict(Some(&record), Some("L"), 29_600, Some(12.0), BOUNDS, 5);
        assert_eq!(prediction.terms.last_good, Some(29_812));
        assert_eq!(prediction.terms.offset, Some(-46));
        assert_eq!(prediction.start, Some(29_766));
    }

    #[test]
    fn a_prediction_inside_the_dead_band_is_reported_not_moved_to() {
        let mut record = record();
        record.set_last_good(last_good(
            Some("L"),
            29_745,
            Some(12.0),
            "2026-09-10T22:00:00Z",
        ));
        let prediction = predict(Some(&record), Some("L"), 29_740, Some(12.0), BOUNDS, 13);
        assert_eq!(prediction.start, Some(29_745));
        assert!(!prediction.moved);
        assert_eq!(
            prediction.skipped.as_deref(),
            Some("within 13 steps of the current position")
        );
    }

    #[test]
    fn a_prediction_outside_the_travel_is_reported_not_moved_to() {
        let mut record = record();
        record.set_last_good(last_good(
            Some("L"),
            61_000,
            Some(12.0),
            "2026-09-10T22:00:00Z",
        ));
        let prediction = predict(Some(&record), Some("L"), 29_740, Some(12.0), BOUNDS, 5);
        assert!(!prediction.moved);
        assert_eq!(
            prediction.skipped.as_deref(),
            Some("outside the focuser's bounds [0, 60000]")
        );
    }

    #[test]
    fn unbounded_travel_accepts_any_position() {
        assert!(Bounds::default().contains(i32::MIN));
        assert_eq!(Bounds::default().describe(), "[-, -]");
    }

    #[test]
    fn a_filterless_train_anchors_on_its_single_entry() {
        let mut record = record();
        record.set_last_good(last_good(None, 12_000, Some(3.0), "2026-09-10T22:00:00Z"));
        let prediction = predict(Some(&record), None, 11_000, Some(3.0), Bounds::default(), 5);
        assert_eq!(prediction.start, Some(12_000));
        assert!(prediction.moved);
    }
}
