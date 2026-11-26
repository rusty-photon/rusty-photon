//! The night's clock: dusk and dawn from `rp`'s civil twilight and the
//! configured offsets (design § Night Schedule → A night).

use chrono::{DateTime, Days, NaiveDate, Utc};
use serde_json::Value;

use super::config::SignedDuration;

/// One night's dusk and dawn, offsets applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NightWindow {
    /// The date `get_twilight` reads the night for.
    pub night: NaiveDate,
    pub dusk: DateTime<Utc>,
    pub dawn: DateTime<Utc>,
}

/// A night's two civil-twilight ends: the evening end, then the morning
/// end.
pub type CivilEnds = (DateTime<Utc>, DateTime<Utc>);

/// The two ends of a `get_twilight` answer: the evening end
/// (`begin_utc`) and the morning end (`end_utc`). `None` when the window
/// lacks either, at a site with no civil darkness that night.
///
/// # Errors
///
/// A message when the answer carries no such fields or a time that does
/// not parse.
pub fn civil_ends(answer: &Value) -> Result<Option<CivilEnds>, String> {
    let end = |key: &str| -> Result<Option<DateTime<Utc>>, String> {
        match answer.get(key) {
            Some(Value::Null) => Ok(None),
            Some(Value::String(text)) => DateTime::parse_from_rfc3339(text)
                .map(|t| Some(t.with_timezone(&Utc)))
                .map_err(|e| format!("get_twilight's {key} `{text}` is not a time: {e}")),
            _ => Err(format!("get_twilight answered without {key}: {answer}")),
        }
    };
    Ok(end("begin_utc")?.zip(end("end_utc")?))
}

/// The night's window: dusk and dawn are the civil ends plus their
/// offsets. `None` only past chrono's date range.
#[must_use]
pub fn window(
    night: NaiveDate,
    (evening, morning): CivilEnds,
    dusk_offset: SignedDuration,
    dawn_offset: SignedDuration,
) -> Option<NightWindow> {
    Some(NightWindow {
        night,
        dusk: evening.checked_add_signed(dusk_offset.0)?,
        dawn: morning.checked_add_signed(dawn_offset.0)?,
    })
}

/// The nights to read for `now`, earliest first.
///
/// A night's window lies
/// between the local noons either side of it, at most half a day from
/// the date's UTC midnight at any longitude, and the offsets move each
/// end by at most 6 h, so the night whose dawn is the first still ahead
/// is among these five.
#[must_use]
pub fn candidate_nights(now: DateTime<Utc>) -> Vec<NaiveDate> {
    let today = now.date_naive();
    let Some(first) = today.checked_sub_days(Days::new(2)) else {
        return Vec::new();
    };
    first.iter_days().take(5).collect()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use chrono::{TimeDelta, TimeZone};
    use serde_json::json;

    #[test]
    fn the_civil_ends_read_from_the_answer() {
        let ends = civil_ends(&json!({
            "kind": "civil",
            "begin_utc": "2026-11-24T01:28:00+00:00",
            "end_utc": "2026-11-24T14:31:00+00:00"
        }))
        .unwrap();
        assert_eq!(
            ends,
            Some((
                Utc.with_ymd_and_hms(2026, 11, 24, 1, 28, 0).unwrap(),
                Utc.with_ymd_and_hms(2026, 11, 24, 14, 31, 0).unwrap()
            ))
        );
    }

    #[test]
    fn a_window_lacking_an_end_reads_as_none() {
        let ends = civil_ends(&json!({ "kind": "civil", "begin_utc": null,
                                       "end_utc": "2026-06-21T02:00:00+00:00" }))
        .unwrap();
        assert_eq!(ends, None);
    }

    #[test]
    fn an_answer_without_the_fields_is_an_error() {
        let err = civil_ends(&json!({ "kind": "civil" })).unwrap_err();
        assert!(err.contains("begin_utc"), "{err}");
        let err = civil_ends(&json!({ "begin_utc": "soon", "end_utc": null })).unwrap_err();
        assert!(err.contains("soon"), "{err}");
    }

    #[test]
    fn the_offsets_move_dusk_and_dawn() {
        let night = NaiveDate::from_ymd_opt(2026, 11, 23).unwrap();
        let evening = Utc.with_ymd_and_hms(2026, 11, 24, 1, 28, 0).unwrap();
        let morning = Utc.with_ymd_and_hms(2026, 11, 24, 14, 31, 0).unwrap();
        let w = window(
            night,
            (evening, morning),
            SignedDuration(TimeDelta::minutes(-30)),
            SignedDuration(TimeDelta::minutes(10)),
        )
        .unwrap();
        assert_eq!(w.night, night);
        assert_eq!(w.dusk, evening - TimeDelta::minutes(30));
        assert_eq!(w.dawn, morning + TimeDelta::minutes(10));
    }

    #[test]
    fn five_nights_around_now_are_read() {
        let now = Utc.with_ymd_and_hms(2026, 11, 24, 3, 0, 0).unwrap();
        let nights: Vec<String> = candidate_nights(now)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            nights,
            [
                "2026-11-22",
                "2026-11-23",
                "2026-11-24",
                "2026-11-25",
                "2026-11-26"
            ]
        );
    }
}
