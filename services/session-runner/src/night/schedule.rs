//! The schedule's loop: it keeps `night.json` on the night ahead.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, Utc};
use serde_json::{json, Map, Value};
use tracing::{debug, warn};

use super::clock::{candidate_nights, civil_ends, window, NightWindow};
use super::config::NightConfig;
use super::state::NightState;
use super::NightStatus;
use crate::engine::{Clock, ToolCallError, ToolClient};

/// First retry after `rp` fails at the request level; doubles up to
/// [`MAX_BACKOFF`] (design § Night Schedule → Errors).
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// How often a `get_twilight` that answered with a tool error is asked
/// again, and how long the loop sleeps at most between two reads of the
/// night ahead.
const TWILIGHT_RETRY: Duration = Duration::from_hours(1);

/// What reading the night ahead came to, other than a night.
enum ReadFailure {
    /// `rp` answered with a tool error (no site configured, for one).
    Tool(String),
    /// The request failed: `rp` is unreachable.
    Unavailable(String),
}

/// The loop over nights, with what it has logged so far.
pub struct Schedule<'a, C, K> {
    pub rp: &'a C,
    pub clock: &'a K,
    pub night: &'a NightConfig,
    pub state_dir: &'a Path,
    pub status: &'a NightStatus,
    backoff: Duration,
    /// The last tool error logged at `warn!`, so a repeat is not.
    logged_tool_error: Option<String>,
    /// The nights without civil darkness already logged.
    logged_dark_less: BTreeSet<NaiveDate>,
}

impl<'a, C: ToolClient + Sync, K: Clock + Sync> Schedule<'a, C, K> {
    pub const fn new(
        rp: &'a C,
        clock: &'a K,
        night: &'a NightConfig,
        state_dir: &'a Path,
        status: &'a NightStatus,
    ) -> Self {
        Self {
            rp,
            clock,
            night,
            state_dir,
            status,
            backoff: INITIAL_BACKOFF,
            logged_tool_error: None,
            logged_dark_less: BTreeSet::new(),
        }
    }

    /// Run the loop for the life of the service.
    pub async fn run(mut self) {
        loop {
            let wait = self.tick().await;
            self.clock.sleep(wait).await;
        }
    }

    /// Read the night ahead and keep `night.json` on it. Returns how long
    /// to wait before the next read: until that night's dawn, an hour at
    /// most.
    pub async fn tick(&mut self) -> Duration {
        let now = self.clock.now();
        match self.night_ahead(now).await {
            Ok(Some(ahead)) => {
                self.backoff = INITIAL_BACKOFF;
                self.logged_tool_error = None;
                self.keep_on(ahead).await;
                ahead
                    .dawn
                    .signed_duration_since(now)
                    .to_std()
                    .map_or(Duration::ZERO, |wait| wait.min(TWILIGHT_RETRY))
            }
            Ok(None) => {
                self.backoff = INITIAL_BACKOFF;
                debug!(
                    "no night with civil darkness in reach; reading the twilight again in an hour"
                );
                TWILIGHT_RETRY
            }
            Err(ReadFailure::Tool(message)) => {
                self.backoff = INITIAL_BACKOFF;
                if self.logged_tool_error.as_deref() == Some(message.as_str()) {
                    debug!(error = %message, "get_twilight still fails; asking again in an hour");
                } else {
                    warn!(error = %message, "get_twilight failed; the schedule knows no new dusk or dawn until it answers, and asks again every hour");
                    self.logged_tool_error = Some(message);
                }
                TWILIGHT_RETRY
            }
            Err(ReadFailure::Unavailable(message)) => {
                let wait = self.backoff;
                debug!(error = %message, ?wait, "rp unreachable; reading the twilight again");
                self.backoff = self.backoff.saturating_mul(2).min(MAX_BACKOFF);
                wait
            }
        }
    }

    /// The first night whose dawn is still ahead of `now`, skipping a
    /// night without civil darkness.
    async fn night_ahead(
        &mut self,
        now: DateTime<Utc>,
    ) -> Result<Option<NightWindow>, ReadFailure> {
        for night in candidate_nights(now) {
            let mut args = Map::new();
            args.insert("date".to_owned(), json!(night.to_string()));
            args.insert("kind".to_owned(), json!("civil"));
            let answer: Value = match self.rp.call("get_twilight", args).await {
                Ok(answer) => answer,
                Err(ToolCallError::Unavailable(message)) => {
                    return Err(ReadFailure::Unavailable(message))
                }
                Err(e) => return Err(ReadFailure::Tool(e.to_string())),
            };
            let ends = civil_ends(&answer).map_err(ReadFailure::Tool)?;
            let Some(ends) = ends else {
                if self.logged_dark_less.insert(night) {
                    warn!(%night, "the night has no civil darkness; the schedule skips it");
                }
                continue;
            };
            let Some(w) = window(night, ends, self.night.dusk_offset, self.night.dawn_offset)
            else {
                continue;
            };
            if w.dawn > now {
                return Ok(Some(w));
            }
        }
        Ok(None)
    }

    /// Point `night.json` at `ahead` unless it already names that night,
    /// that dusk and that dawn.
    async fn keep_on(&self, ahead: NightWindow) {
        let current = self.status.snapshot();
        let on_it = current.as_ref().is_some_and(|s| {
            s.night == ahead.night && s.dusk == ahead.dusk && s.dawn == ahead.dawn
        });
        if on_it {
            return;
        }
        let state = NightState::ahead(ahead.night, ahead.dusk, ahead.dawn);
        if let Err(e) = state.write(self.state_dir).await {
            warn!(error = %e, "cannot write night.json; the schedule carries on with its place in memory");
        }
        debug!(night = %ahead.night, dusk = %ahead.dusk, dawn = %ahead.dawn, "night.json moved to the night ahead");
        self.status.publish(state);
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use chrono::{TimeDelta, TimeZone};

    use super::*;
    use crate::night::state::Phase;

    /// A clock whose `sleep` moves `now` on at once.
    struct TestClock {
        now: Mutex<DateTime<Utc>>,
    }

    impl TestClock {
        fn at(now: DateTime<Utc>) -> Self {
            Self {
                now: Mutex::new(now),
            }
        }

        fn set(&self, now: DateTime<Utc>) {
            *self.now.lock().unwrap() = now;
        }
    }

    impl Clock for TestClock {
        fn now(&self) -> DateTime<Utc> {
            *self.now.lock().unwrap()
        }

        async fn sleep(&self, duration: Duration) {
            let mut now = self.now.lock().unwrap();
            *now += TimeDelta::from_std(duration).unwrap();
        }

        fn monotonic(&self) -> Duration {
            Duration::ZERO
        }
    }

    /// `rp` answering `get_twilight` from a table of nights; a night not
    /// in it answers with the table's fallback.
    struct TwilightRp {
        nights: BTreeMap<NaiveDate, Value>,
        fallback: Result<Value, ToolCallError>,
        calls: Mutex<usize>,
    }

    impl ToolClient for TwilightRp {
        async fn call(&self, tool: &str, args: Map<String, Value>) -> Result<Value, ToolCallError> {
            assert_eq!(tool, "get_twilight");
            assert_eq!(args["kind"], json!("civil"));
            *self.calls.lock().unwrap() += 1;
            let night: NaiveDate = args["date"].as_str().unwrap().parse().unwrap();
            self.nights
                .get(&night)
                .cloned()
                .map_or_else(|| self.fallback.clone(), Ok)
        }
    }

    fn date(day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, day).unwrap()
    }

    /// Civil darkness from 01:28 to 14:31 UTC after the night's date, as
    /// at 47.6° N, 122° W in late November.
    fn dark(day: u32) -> Value {
        json!({
            "kind": "civil",
            "begin_utc": format!("2026-11-{:02}T01:28:00+00:00", day + 1),
            "end_utc": format!("2026-11-{:02}T14:31:00+00:00", day + 1),
        })
    }

    fn rp_with(nights: &[(u32, Value)]) -> TwilightRp {
        TwilightRp {
            nights: nights.iter().map(|(d, v)| (date(*d), v.clone())).collect(),
            fallback: Err(ToolCallError::Failed("no such night in the test".into())),
            calls: Mutex::new(0),
        }
    }

    fn block() -> NightConfig {
        serde_json::from_value(json!({ "dusk_offset": "-30m" })).unwrap()
    }

    fn at(day: u32, hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, day, hour, 0, 0).unwrap()
    }

    #[tokio::test]
    async fn the_night_ahead_goes_to_night_json_with_its_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let rp = rp_with(&[
            (21, dark(21)),
            (22, dark(22)),
            (23, dark(23)),
            (24, dark(24)),
        ]);
        let clock = TestClock::at(at(23, 20));
        let night = block();
        let status = NightStatus::default();
        let mut schedule = Schedule::new(&rp, &clock, &night, dir.path(), &status);

        let wait = schedule.tick().await;

        let expected = NightState::ahead(
            date(23),
            Utc.with_ymd_and_hms(2026, 11, 24, 0, 58, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 11, 24, 14, 31, 0).unwrap(),
        );
        assert_eq!(status.snapshot(), Some(expected.clone()));
        assert_eq!(NightState::load(dir.path()).await, Some(expected));
        assert_eq!(wait, TWILIGHT_RETRY, "dawn is more than an hour off");
    }

    #[tokio::test]
    async fn a_night_in_progress_is_the_night_ahead_until_its_dawn() {
        let dir = tempfile::tempdir().unwrap();
        let rp = rp_with(&[(22, dark(22)), (23, dark(23)), (24, dark(24))]);
        let clock = TestClock::at(Utc.with_ymd_and_hms(2026, 11, 24, 14, 0, 0).unwrap());
        let night = block();
        let status = NightStatus::default();
        let mut schedule = Schedule::new(&rp, &clock, &night, dir.path(), &status);

        let wait = schedule.tick().await;
        assert_eq!(status.snapshot().unwrap().night, date(23));
        assert_eq!(wait, Duration::from_mins(31), "until dawn");

        clock.set(Utc.with_ymd_and_hms(2026, 11, 24, 14, 31, 0).unwrap());
        schedule.tick().await;
        let state = status.snapshot().unwrap();
        assert_eq!(
            state.night,
            date(24),
            "at dawn the file moves to the next night"
        );
        assert_eq!(state.phase, Phase::Day);
    }

    #[tokio::test]
    async fn a_night_without_civil_darkness_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let no_dark = json!({ "kind": "civil", "begin_utc": null, "end_utc": null });
        let rp = rp_with(&[
            (21, dark(21)),
            (22, dark(22)),
            (23, no_dark),
            (24, dark(24)),
        ]);
        let clock = TestClock::at(at(23, 20));
        let night = block();
        let status = NightStatus::default();
        let mut schedule = Schedule::new(&rp, &clock, &night, dir.path(), &status);

        schedule.tick().await;

        assert_eq!(status.snapshot().unwrap().night, date(24));
    }

    #[tokio::test]
    async fn a_tool_error_keeps_the_place_and_asks_again_in_an_hour() {
        let dir = tempfile::tempdir().unwrap();
        let known = NightState::ahead(date(23), at(24, 1), at(24, 14));
        known.write(dir.path()).await.unwrap();
        let status = NightStatus::default();
        status.publish(known.clone());
        let rp = rp_with(&[]);
        let clock = TestClock::at(at(23, 20));
        let night = block();
        let mut schedule = Schedule::new(&rp, &clock, &night, dir.path(), &status);

        assert_eq!(schedule.tick().await, TWILIGHT_RETRY);
        assert_eq!(status.snapshot(), Some(known.clone()));
        assert_eq!(NightState::load(dir.path()).await, Some(known));
    }

    #[tokio::test]
    async fn an_unreachable_rp_is_retried_backing_off_to_thirty_seconds() {
        let dir = tempfile::tempdir().unwrap();
        let rp = TwilightRp {
            nights: BTreeMap::new(),
            fallback: Err(ToolCallError::Unavailable("connection refused".into())),
            calls: Mutex::new(0),
        };
        let clock = TestClock::at(at(23, 20));
        let night = block();
        let status = NightStatus::default();
        let mut schedule = Schedule::new(&rp, &clock, &night, dir.path(), &status);

        let mut waits = Vec::new();
        for _ in 0..7 {
            waits.push(schedule.tick().await.as_secs());
        }
        assert_eq!(waits, [1, 2, 4, 8, 16, 30, 30]);
        assert_eq!(status.snapshot(), None);
    }

    #[tokio::test]
    async fn a_file_already_on_the_night_ahead_is_left_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let rp = rp_with(&[
            (21, dark(21)),
            (22, dark(22)),
            (23, dark(23)),
            (24, dark(24)),
        ]);
        let clock = TestClock::at(at(23, 20));
        let night = block();
        let status = NightStatus::default();
        let mut on_it = NightState::ahead(
            date(23),
            Utc.with_ymd_and_hms(2026, 11, 24, 0, 58, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 11, 24, 14, 31, 0).unwrap(),
        );
        on_it.cameras_given_up.push("main-cam".into());
        status.publish(on_it.clone());
        let mut schedule = Schedule::new(&rp, &clock, &night, dir.path(), &status);

        schedule.tick().await;

        assert_eq!(status.snapshot(), Some(on_it));
        assert_eq!(
            NightState::load(dir.path()).await,
            None,
            "nothing was written"
        );
    }
}
