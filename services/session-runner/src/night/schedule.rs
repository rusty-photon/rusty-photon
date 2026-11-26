//! The schedule's loop: it runs each dusk and the night's runs, each
//! dawn and the after-dawn checks until the next dusk, and keeps
//! `night.json` on the night ahead.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, Utc};
use serde_json::{json, Map, Value};
use tracing::{debug, warn};

use super::clock::{candidate_nights, civil_ends, window, NightWindow};
use super::config::{NightConfig, NightRun};
use super::state::{NightState, OutputRef, Phase};
use super::{NightStatus, Signals};

use crate::engine::{Clock, ToolCallError, ToolClient};
use crate::runs::StartRefused;

/// First retry after `rp` fails at the request level; doubles up to
/// [`MAX_BACKOFF`] (design § Night Schedule → Errors).
pub(super) const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
pub(super) const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// How often a `get_twilight` that answered with a tool error is asked
/// again, and how long the loop sleeps at most between two reads of the
/// nights.
pub(super) const TWILIGHT_RETRY: Duration = Duration::from_hours(1);

/// A run the slot knows, as the schedule reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnownRun {
    pub run_id: String,
    pub session_id: String,
    /// Running or paused.
    pub active: bool,
}

/// The run slot as the schedule uses it.
pub trait RunSlot: Sync {
    /// The run that holds the slot, if any.
    fn active_run(&self) -> Option<String>;
    /// Every run this process knows.
    fn runs(&self) -> Vec<KnownRun>;
    /// End a run as `POST /runs/{id}/stop` ends it.
    fn stop_run(&self, run_id: &str);
    /// Take the slot; `false` while a run holds it.
    fn hold(&self) -> bool;
    fn release(&self);
    /// Whether the schedule holds the slot.
    fn held(&self) -> bool;
    /// Start `run` under `session_id` as `POST /runs` starts it, the
    /// schedule's hold of the slot handed to it when `from_night`.
    /// Returns the run id.
    fn start_run(
        &self,
        run: &NightRun,
        session_id: &str,
        from_night: bool,
    ) -> impl Future<Output = Result<String, StartRefused>> + Send;
}

/// What reading the nights came to, other than nights.
enum ReadFailure {
    /// `rp` answered with a tool error (no site configured, for one).
    Tool(String),
    /// The request failed: `rp` is unreachable.
    Unavailable(String),
}

/// The two nights the schedule works between: the one whose dawn passed
/// latest, and the first whose dawn is still ahead.
struct Nights {
    latest: Option<NightWindow>,
    ahead: Option<NightWindow>,
}

/// What the schedule keeps in memory for one night.
#[derive(Default)]
pub(super) struct NightMemory {
    /// When the after-dawn checks run next.
    pub(super) next_check: Option<DateTime<Utc>>,
    /// The re-parks the after-dawn checks have called.
    pub(super) parks: u32,
    /// The re-closes the after-dawn checks have called, per calibrator.
    pub(super) closes: BTreeMap<String, u32>,
    /// The outputs whose hold has been logged at `warn!`.
    pub(super) logged_left_on: BTreeSet<OutputRef>,
    /// The outputs left on until dusk: the mount's after its last
    /// re-park, or a cover's after its last re-close.
    pub(super) until_dusk: BTreeSet<OutputRef>,
    /// The outputs left on because their cut failed or their box could
    /// not be read: a check connects their held-out devices first.
    pub(super) reconnect_first: BTreeSet<OutputRef>,
    /// Whether the runs phase has looked for a run of this night the
    /// previous process left.
    pub(super) runs_taken_up: bool,
    /// When the covers still to open are tried next.
    pub(super) next_cover_retry: Option<DateTime<Utc>>,
    /// The operator's run whose wait has been logged.
    pub(super) logged_wait_for: Option<String>,
    /// Whether the end of the night's runs has been logged.
    pub(super) logged_runs_done: bool,
}

/// The loop over nights, with what it has logged so far.
pub struct Schedule<'a, C, K, S> {
    pub(super) rp: &'a C,
    pub(super) clock: &'a K,
    pub(super) slot: &'a S,
    pub(super) night: &'a NightConfig,
    pub(super) state_dir: &'a Path,
    pub(super) status: &'a NightStatus,
    /// What `rp`'s event stream tells the schedule.
    pub(super) signals: Arc<Signals>,
    /// How often dusk reads `get_safety_status` while the roof does not
    /// read safe.
    pub(super) safety_poll: Duration,
    /// How often the event stream has connected; a reconnect reads as
    /// `rp` back from a restart.
    pub(super) connects: Arc<AtomicU64>,
    pub(super) seen_connects: u64,
    /// A call failed at the request level since the last that answered.
    pub(super) rp_was_down: bool,
    /// `rp` came back since the after-dawn checks last looked.
    pub(super) rp_came_back: bool,
    pub(super) memory: NightMemory,
    backoff: Duration,
    /// The last tool error logged at `warn!`, so a repeat is not.
    logged_tool_error: Option<String>,
    /// The nights without civil darkness already logged.
    logged_dark_less: BTreeSet<NaiveDate>,
}

/// The seams a [`Schedule`] runs against.
pub struct Seams<'a, C, K, S> {
    pub rp: &'a C,
    pub clock: &'a K,
    pub slot: &'a S,
    pub night: &'a NightConfig,
    pub state_dir: &'a Path,
    pub status: &'a NightStatus,
    pub signals: Arc<Signals>,
    pub safety_poll: Duration,
    pub connects: Arc<AtomicU64>,
}

impl<'a, C: ToolClient + Sync, K: Clock + Sync, S: RunSlot> Schedule<'a, C, K, S> {
    #[must_use]
    pub fn new(seams: Seams<'a, C, K, S>) -> Self {
        Self {
            rp: seams.rp,
            clock: seams.clock,
            slot: seams.slot,
            night: seams.night,
            state_dir: seams.state_dir,
            status: seams.status,
            signals: seams.signals,
            safety_poll: seams.safety_poll,
            connects: seams.connects,
            seen_connects: 0,
            rp_was_down: false,
            rp_came_back: false,
            memory: NightMemory::default(),
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

    /// One pass: from dusk to dawn the night runs from wherever
    /// `night.json` says it is; by day a dawn owed runs, the after-dawn
    /// checks run when due, and otherwise `night.json` moves to the night
    /// ahead. Returns how long to wait before the next pass.
    pub async fn tick(&mut self) -> Duration {
        let now = self.clock.now();
        let nights = match self.nights(now).await {
            Ok(nights) => {
                self.backoff = INITIAL_BACKOFF;
                self.logged_tool_error = None;
                if self.rp_was_down {
                    self.rp_was_down = false;
                    self.rp_came_back = true;
                }
                nights
            }
            Err(ReadFailure::Tool(message)) => {
                self.backoff = INITIAL_BACKOFF;
                if self.logged_tool_error.as_deref() == Some(message.as_str()) {
                    debug!(error = %message, "get_twilight still fails; asking again in an hour");
                } else {
                    warn!(error = %message, "get_twilight failed; the schedule knows no new dusk or dawn until it answers, and asks again every hour");
                    self.logged_tool_error = Some(message);
                }
                return TWILIGHT_RETRY;
            }
            Err(ReadFailure::Unavailable(message)) => {
                self.rp_was_down = true;
                let wait = self.backoff;
                debug!(error = %message, ?wait, "rp unreachable; reading the twilight again");
                self.backoff = self.backoff.saturating_mul(2).min(MAX_BACKOFF);
                return wait;
            }
        };
        let Some(ahead) = nights.ahead else {
            debug!("no night with civil darkness in reach; reading the twilight again in an hour");
            return TWILIGHT_RETRY;
        };
        // Night: the night ahead's dusk has passed and its dawn has not.
        if now >= ahead.dusk {
            self.keep_on(ahead).await;
            return self.night_time(ahead).await;
        }
        // Daytime: the latest dawn has passed and the next dusk has not.
        if let Some(latest) = nights.latest {
            let state = self.status.snapshot();
            if dawn_owed(latest.night, state.as_ref()) {
                self.dawn(latest, ahead).await;
                return Duration::ZERO;
            }
            if state.is_some_and(|s| s.night == latest.night && s.phase == Phase::AfterDawn) {
                return self.after_dawn(ahead).await;
            }
        }
        self.keep_on(ahead).await;
        until(now, ahead.dusk).min(TWILIGHT_RETRY)
    }

    /// The night between `ahead`'s dusk and its dawn, from wherever
    /// `night.json` says it is (design § Restarts, cases 2 and 4 to 6).
    async fn night_time(&mut self, ahead: NightWindow) -> Duration {
        let Some(state) = self.status.snapshot().filter(|s| s.night == ahead.night) else {
            return Duration::ZERO;
        };
        match state.phase {
            Phase::Day | Phase::Dusk => self.dusk(ahead, state).await,
            Phase::PowerUp | Phase::CameraCheck => self.power_up_again(ahead, state).await,
            Phase::Runs => self.runs(ahead, state).await,
            // A night's dawn and the checks after it come after its dawn.
            Phase::Dawn | Phase::AfterDawn => {
                until(self.clock.now(), ahead.dawn).min(TWILIGHT_RETRY)
            }
        }
    }

    /// The nights around `now`: the one whose dawn passed latest, and
    /// the first whose dawn is still ahead, skipping a night without
    /// civil darkness.
    async fn nights(&mut self, now: DateTime<Utc>) -> Result<Nights, ReadFailure> {
        let mut nights = Nights {
            latest: None,
            ahead: None,
        };
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
                nights.ahead = Some(w);
                return Ok(nights);
            }
            nights.latest = Some(w);
        }
        Ok(nights)
    }

    /// Point `night.json` at `ahead` unless it already names that night,
    /// that dusk and that dawn.
    pub(super) async fn keep_on(&mut self, ahead: NightWindow) {
        let current = self.status.snapshot();
        let on_it = current.as_ref().is_some_and(|s| {
            s.night == ahead.night && s.dusk == ahead.dusk && s.dawn == ahead.dawn
        });
        if on_it {
            return;
        }
        self.memory = NightMemory::default();
        self.status.set_run(None);
        let state = NightState::ahead(ahead.night, ahead.dusk, ahead.dawn);
        self.save(&state).await;
        debug!(night = %ahead.night, dusk = %ahead.dusk, dawn = %ahead.dawn, "night.json moved to the night ahead");
    }

    /// Write `night.json` and publish it.
    pub(super) async fn save(&self, state: &NightState) {
        if let Err(e) = state.write(self.state_dir).await {
            warn!(error = %e, "cannot write night.json; the schedule carries on with its place in memory");
        }
        self.status.publish(state.clone());
    }
}

/// Whether the dawn of `latest` is still owed: nothing shows it done.
/// A file of an earlier night, or of this night in any phase before
/// `after_dawn`, owes it; a file moved to a later night shows it done.
fn dawn_owed(latest: NaiveDate, state: Option<&NightState>) -> bool {
    state.is_none_or(|s| s.night < latest || (s.night == latest && s.phase != Phase::AfterDawn))
}

/// How long from `now` until `then`, zero once it has passed.
pub(super) fn until(now: DateTime<Utc>, then: DateTime<Utc>) -> Duration {
    then.signed_duration_since(now)
        .to_std()
        .unwrap_or(Duration::ZERO)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use chrono::TimeZone;

    use super::super::test_rig::{at, date, done_up_to, Harness};
    use super::*;

    #[test]
    fn a_dawn_is_owed_until_a_file_shows_it_done() {
        assert!(dawn_owed(date(23), None));
        assert!(dawn_owed(date(23), Some(&done_up_to(22))));
        assert!(
            dawn_owed(date(23), Some(&done_up_to(23))),
            "phase day of the same night"
        );
        let mut dawn = done_up_to(23);
        dawn.phase = Phase::Dawn;
        assert!(
            dawn_owed(date(23), Some(&dawn)),
            "a dawn cut short runs again"
        );
        let mut after = done_up_to(23);
        after.phase = Phase::AfterDawn;
        assert!(!dawn_owed(date(23), Some(&after)));
        assert!(
            !dawn_owed(date(23), Some(&done_up_to(24))),
            "moved to the next night"
        );
    }

    #[tokio::test]
    async fn a_start_in_the_night_puts_night_json_on_it_with_its_offsets() {
        let h = Harness::with_runs(at(24, 3), &[]);
        let mut schedule = h.schedule();

        schedule.tick().await;

        let state = NightState::load(h.dir.path()).await.unwrap();
        assert_eq!(h.status.snapshot(), Some(state.clone()));
        assert_eq!(
            (state.night, state.dusk, state.dawn),
            (
                date(23),
                Utc.with_ymd_and_hms(2026, 11, 24, 0, 58, 0).unwrap(),
                Utc.with_ymd_and_hms(2026, 11, 24, 14, 31, 0).unwrap(),
            )
        );
        assert_eq!(h.rp.called("power_down"), 0, "no dawn ran");
    }

    #[tokio::test]
    async fn a_night_in_progress_is_the_night_ahead_until_its_dawn() {
        let h = Harness::with_runs(Utc.with_ymd_and_hms(2026, 11, 24, 14, 0, 0).unwrap(), &[]);
        h.status.publish(done_up_to(23));
        let mut schedule = h.schedule();

        schedule.tick().await;
        assert_eq!(
            h.status.snapshot().unwrap().phase,
            Phase::Runs,
            "a start between dusk and dawn takes the night up at dusk"
        );
        let wait = schedule.tick().await;
        assert_eq!(h.status.snapshot().unwrap().night, date(23));
        assert_eq!(wait, Duration::from_mins(31), "until dawn");
        assert_eq!(h.rp.called("power_down"), 0);
    }

    #[tokio::test]
    async fn a_night_without_civil_darkness_is_skipped() {
        let mut h = Harness::new(at(24, 20));
        h.rp.nights.insert(
            date(24),
            json!({ "kind": "civil", "begin_utc": null, "end_utc": null }),
        );
        let mut after_dawn = done_up_to(23);
        after_dawn.phase = Phase::AfterDawn;
        h.status.publish(after_dawn);
        let mut schedule = h.schedule();

        schedule.tick().await;

        assert_eq!(
            h.status.snapshot().unwrap().night,
            date(25),
            "nothing left on: the file moves past the dark-less night to the next"
        );
    }

    #[tokio::test]
    async fn a_tool_error_keeps_the_place_and_asks_again_in_an_hour() {
        let mut h = Harness::new(at(23, 20));
        h.rp.nights.clear();
        let known = done_up_to(23);
        h.status.publish(known.clone());
        let mut schedule = h.schedule();

        assert_eq!(schedule.tick().await, TWILIGHT_RETRY);
        assert_eq!(h.status.snapshot(), Some(known));
    }

    #[tokio::test]
    async fn an_unreachable_rp_is_retried_backing_off_to_thirty_seconds() {
        let h = Harness::new(at(23, 20));
        h.rp.rig.lock().unwrap().unavailable = u32::MAX;
        let mut schedule = h.schedule();

        let mut waits = Vec::new();
        for _ in 0..7 {
            waits.push(schedule.tick().await.as_secs());
        }
        assert_eq!(waits, [1, 2, 4, 8, 16, 30, 30]);
        assert_eq!(h.status.snapshot(), None);
    }

    #[tokio::test]
    async fn a_file_already_on_the_night_ahead_is_left_as_it_is() {
        let h = Harness::new(at(23, 20));
        let mut on_it = done_up_to(23);
        on_it.cameras_given_up.push("main-cam".into());
        h.status.publish(on_it.clone());
        let mut schedule = h.schedule();

        schedule.tick().await;

        assert_eq!(h.status.snapshot(), Some(on_it));
        assert_eq!(
            NightState::load(h.dir.path()).await,
            None,
            "nothing was written"
        );
    }
}
