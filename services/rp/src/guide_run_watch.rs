//! The Guide Run Watch (rp.md § Guide Run Watch).
//!
//! The guider pushes no event, so rp reads the stats of a run it has
//! seen settle and turns what they show into **events, never actions**.
//! `guide_stopped` with the reason `guider` reports a run that ended
//! without rp asking, and `guide_star_lost` a loss that has lasted.
//!
//! [`WatchedGuider`] is the guider client rp's tools, ladder and safety
//! stop share, with the watch around it: a start the guider answers is
//! the settle the watch starts at, and a stop the guider answers ends
//! it, since that run's end is rp's own doing. A stop answered
//! `stop_timeout` is an answered stop: the guider sends no further
//! pulse and the run ends (guider.md § `POST /api/v1/guiding/stop`).
//! Nothing else rp sends the guider touches the watch, and the watch
//! sends the guider its reads alone.
//!
//! What the reads mean lives in [`RunWatch`], a state machine over
//! `(read, now)` with no clock of its own.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use rp_guider::{
    DitherRequest, GuiderClient, GuiderDevices, GuiderError, GuiderState, GuidingMetrics,
    GuidingStats, SettledOutcome, StartGuidingRequest,
};
use serde_json::json;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::debug;

use crate::events::EventBus;

/// How far apart the watch's reads are.
pub const READ_INTERVAL: Duration = Duration::from_secs(5);

/// How long one read has to answer, whatever the guider client's own
/// timeout is.
pub const READ_BOUND: Duration = Duration::from_secs(5);

/// How long no read may have answered before the run is reported ended
/// with the stop reason `unreachable`.
pub const UNREACHABLE_AFTER: Duration = Duration::from_secs(60);

/// What a read asks the watch to report.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Report {
    /// The run has ended. The watch ends with it.
    Stopped { stop_reason: String },
    /// The run has been in `star_lost` for `lost_for`.
    StarLost { lost_for: Duration },
}

/// A loss of the guide stars, from the first read that showed it.
#[derive(Debug, Clone, Copy)]
struct Loss {
    since: Instant,
    /// How long the loss must have lasted for its next report.
    report_due: Duration,
}

/// What the reads of one run add up to.
#[derive(Debug)]
struct RunWatch {
    star_lost_after: Duration,
    /// When the guider last answered: the settle, then each read.
    last_answer: Instant,
    loss: Option<Loss>,
}

impl RunWatch {
    const fn new(star_lost_after: Duration, settled_at: Instant) -> Self {
        Self {
            star_lost_after,
            last_answer: settled_at,
            loss: None,
        }
    }

    /// A read the guider answered.
    fn answered(&mut self, stats: &GuidingStats, now: Instant) -> Option<Report> {
        self.last_answer = now;
        match stats.state {
            GuiderState::Idle | GuiderState::Looping => {
                // A guider that has stopped nothing since it started is
                // one that started under the run.
                let stop_reason = stats
                    .last_stop
                    .as_ref()
                    .map_or_else(|| "restarted".to_string(), |stop| stop.reason.clone());
                Some(Report::Stopped { stop_reason })
            }
            GuiderState::StarLost => {
                let loss = self.loss.get_or_insert(Loss {
                    since: now,
                    report_due: self.star_lost_after,
                });
                let lost_for = now.saturating_duration_since(loss.since);
                if lost_for < loss.report_due {
                    return None;
                }
                loss.report_due = lost_for.saturating_add(self.star_lost_after);
                Some(Report::StarLost { lost_for })
            }
            GuiderState::Calibrating | GuiderState::Guiding | GuiderState::Paused => {
                self.loss = None;
                None
            }
        }
    }

    /// A read the guider did not answer. It neither counts towards a
    /// loss nor ends one.
    fn unanswered(&self, now: Instant) -> Option<Report> {
        (now.saturating_duration_since(self.last_answer) >= UNREACHABLE_AFTER).then(|| {
            Report::Stopped {
                stop_reason: "unreachable".to_string(),
            }
        })
    }
}

/// What the client and the watch's task share.
#[derive(Debug, Default)]
struct Watching {
    /// Stops rp asked for that the guider has not answered yet.
    stops_in_flight: usize,
    /// Counts the settles and the answered stops, so the task of a run
    /// that rp has stopped or started anew reports nothing more.
    run: u64,
    task: Option<JoinHandle<()>>,
}

impl Watching {
    /// End the watch of the run in progress, if there is one.
    fn end(&mut self) {
        self.run = self.run.wrapping_add(1);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

fn lock(watching: &Mutex<Watching>) -> MutexGuard<'_, Watching> {
    watching.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One stop of rp's in flight. Dropping it, answered or not, lets the
/// watch read again.
struct StopInFlight<'a>(&'a Mutex<Watching>);

impl<'a> StopInFlight<'a> {
    fn begin(watching: &'a Mutex<Watching>) -> Self {
        let mut state = lock(watching);
        state.stops_in_flight = state.stops_in_flight.saturating_add(1);
        drop(state);
        Self(watching)
    }
}

impl Drop for StopInFlight<'_> {
    fn drop(&mut self) {
        let mut state = lock(self.0);
        state.stops_in_flight = state.stops_in_flight.saturating_sub(1);
    }
}

/// The guider client with the Guide Run Watch around it.
pub struct WatchedGuider {
    inner: Arc<dyn GuiderClient>,
    event_bus: Arc<EventBus>,
    star_lost_after: Duration,
    watching: Arc<Mutex<Watching>>,
}

impl WatchedGuider {
    #[must_use]
    pub fn new(
        inner: Arc<dyn GuiderClient>,
        event_bus: Arc<EventBus>,
        star_lost_after: Duration,
    ) -> Self {
        Self {
            inner,
            event_bus,
            star_lost_after,
            watching: Arc::default(),
        }
    }

    /// Watch the run that has just settled, in place of any earlier one.
    fn watch_run(&self) {
        let mut state = lock(&self.watching);
        state.end();
        let run = state.run;
        state.task = Some(tokio::spawn(watch(
            Arc::clone(&self.inner),
            Arc::clone(&self.event_bus),
            RunWatch::new(self.star_lost_after, Instant::now()),
            Arc::clone(&self.watching),
            run,
        )));
        drop(state);
        debug!(run, "guide run watch started");
    }
}

impl Drop for WatchedGuider {
    fn drop(&mut self) {
        lock(&self.watching).end();
    }
}

/// The task of one run's watch: a read every [`READ_INTERVAL`], until
/// the run's end is reported or rp ends the watch.
async fn watch(
    guider: Arc<dyn GuiderClient>,
    event_bus: Arc<EventBus>,
    mut run_watch: RunWatch,
    watching: Arc<Mutex<Watching>>,
    run: u64,
) {
    loop {
        tokio::time::sleep(READ_INTERVAL).await;
        if lock(&watching).stops_in_flight > 0 {
            continue;
        }
        let read = tokio::time::timeout(READ_BOUND, guider.guiding_stats()).await;
        // Judged and reported under the lock: a stop of rp's that the
        // guider answers while this read is out ends the watch before
        // the read can report that stop as the guider's.
        let shared = lock(&watching);
        let ended = if shared.run != run {
            true
        } else if shared.stops_in_flight > 0 {
            false
        } else {
            report_read(&mut run_watch, read, &event_bus)
        };
        drop(shared);
        if ended {
            return;
        }
    }
}

/// One read of the watch's: the guider's answer, its error, or no answer
/// within [`READ_BOUND`].
type Read = Result<Result<GuidingStats, GuiderError>, tokio::time::error::Elapsed>;

/// Emit what `read` adds to the run's watch. `true` when that is the
/// run's end, which ends the watch.
fn report_read(run_watch: &mut RunWatch, read: Read, event_bus: &EventBus) -> bool {
    let now = Instant::now();
    let report = match read {
        Ok(Ok(stats)) => run_watch.answered(&stats, now),
        Ok(Err(e)) => {
            debug!(error = %e, "guide run watch: the stats read failed");
            run_watch.unanswered(now)
        }
        Err(_) => {
            debug!("guide run watch: the stats read did not answer in time");
            run_watch.unanswered(now)
        }
    };
    match report {
        Some(Report::Stopped { stop_reason }) => {
            debug!(%stop_reason, "guide run watch: the run ended without rp asking");
            event_bus.emit(
                "guide_stopped",
                json!({ "reason": "guider", "stop_reason": stop_reason }),
            );
            true
        }
        Some(Report::StarLost { lost_for }) => {
            let lost_for_ms = u64::try_from(lost_for.as_millis()).unwrap_or(u64::MAX);
            debug!(lost_for_ms, "guide run watch: the guide stars are lost");
            event_bus.emit("guide_star_lost", json!({ "lost_for_ms": lost_for_ms }));
            false
        }
        None => false,
    }
}

/// Whether the guider has taken a stop: it answered it, with its state
/// or with `stop_timeout`, after which it sends no further pulse and
/// the run ends. Any other failure leaves the run as it was.
fn stop_taken(answer: &Result<GuiderState, GuiderError>) -> bool {
    answer
        .as_ref()
        .map_or_else(|e| e.code() == Some("stop_timeout"), |_| true)
}

#[async_trait]
impl GuiderClient for WatchedGuider {
    async fn start_guiding(
        &self,
        request: StartGuidingRequest,
    ) -> Result<SettledOutcome, GuiderError> {
        let outcome = self.inner.start_guiding(request).await?;
        self.watch_run();
        Ok(outcome)
    }

    async fn stop_guiding(&self) -> Result<GuiderState, GuiderError> {
        let _in_flight = StopInFlight::begin(&self.watching);
        let answer = self.inner.stop_guiding().await;
        if stop_taken(&answer) {
            lock(&self.watching).end();
        }
        answer
    }

    async fn pause_guiding(&self, full: bool) -> Result<GuiderState, GuiderError> {
        self.inner.pause_guiding(full).await
    }

    async fn resume_guiding(&self) -> Result<GuiderState, GuiderError> {
        self.inner.resume_guiding().await
    }

    async fn dither(&self, request: DitherRequest) -> Result<SettledOutcome, GuiderError> {
        self.inner.dither(request).await
    }

    async fn abort_dither(&self) -> Result<bool, GuiderError> {
        self.inner.abort_dither().await
    }

    async fn guiding_stats(&self) -> Result<GuidingStats, GuiderError> {
        self.inner.guiding_stats().await
    }

    async fn guiding_metrics(&self) -> Result<GuidingMetrics, GuiderError> {
        self.inner.guiding_metrics().await
    }

    async fn devices(&self) -> Result<GuiderDevices, GuiderError> {
        self.inner.devices().await
    }

    async fn clear_calibration(&self) -> Result<GuiderState, GuiderError> {
        self.inner.clear_calibration().await
    }

    async fn reselect_star(&self) -> Result<GuiderState, GuiderError> {
        self.inner.reselect_star().await
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rp_guider::LastStop;
    use tokio::sync::broadcast::error::TryRecvError;
    use tokio::sync::broadcast::Receiver;
    use tokio::sync::Notify;

    use super::*;
    use crate::events::EventEnvelope;

    const STAR_LOST_AFTER: Duration = Duration::from_secs(12);

    fn stats(state: GuiderState) -> GuidingStats {
        GuidingStats::in_state(state)
    }

    fn stopped_for(state: GuiderState, reason: &str) -> GuidingStats {
        let mut stats = stats(state);
        stats.last_stop = Some(LastStop {
            reason: reason.to_string(),
            at: "2026-11-12T03:20:01Z".to_string(),
            message: "the run stopped".to_string(),
        });
        stats
    }

    fn after(start: Instant, seconds: u64) -> Instant {
        start.checked_add(Duration::from_secs(seconds)).unwrap()
    }

    fn stopped(stop_reason: &str) -> Report {
        Report::Stopped {
            stop_reason: stop_reason.to_string(),
        }
    }

    #[test]
    fn a_run_that_goes_on_guiding_reports_nothing() {
        let start = Instant::now();
        let mut watch = RunWatch::new(STAR_LOST_AFTER, start);

        for state in [
            GuiderState::Guiding,
            GuiderState::Paused,
            GuiderState::Calibrating,
        ] {
            assert_eq!(watch.answered(&stats(state), after(start, 5)), None);
        }
    }

    #[test]
    fn a_run_found_idle_or_looping_ended_for_the_guiders_last_stop_reason() {
        for state in [GuiderState::Idle, GuiderState::Looping] {
            let start = Instant::now();
            let mut watch = RunWatch::new(STAR_LOST_AFTER, start);

            let report = watch.answered(&stopped_for(state, "mount_slewing"), after(start, 5));

            assert_eq!(report, Some(stopped("mount_slewing")));
        }
    }

    #[test]
    fn a_run_found_idle_with_no_last_stop_ended_by_a_restart() {
        let start = Instant::now();
        let mut watch = RunWatch::new(STAR_LOST_AFTER, start);

        let report = watch.answered(&stats(GuiderState::Idle), after(start, 5));

        assert_eq!(report, Some(stopped("restarted")));
    }

    #[test]
    fn no_answer_for_60_s_is_an_unreachable_guider_and_not_before() {
        let start = Instant::now();
        let mut watch = RunWatch::new(STAR_LOST_AFTER, start);
        watch.answered(&stats(GuiderState::Guiding), after(start, 5));

        assert_eq!(watch.unanswered(after(start, 64)), None);
        assert_eq!(
            watch.unanswered(after(start, 65)),
            Some(stopped("unreachable"))
        );
    }

    #[test]
    fn a_loss_is_reported_once_it_has_lasted_star_lost_after_and_again_after_each_further_one() {
        let start = Instant::now();
        let mut watch = RunWatch::new(STAR_LOST_AFTER, start);
        let lost = stats(GuiderState::StarLost);
        let lost_for = |seconds| {
            Some(Report::StarLost {
                lost_for: Duration::from_secs(seconds),
            })
        };

        assert_eq!(watch.answered(&lost, after(start, 5)), None);
        assert_eq!(watch.answered(&lost, after(start, 10)), None);
        assert_eq!(watch.answered(&lost, after(start, 15)), None);
        assert_eq!(watch.answered(&lost, after(start, 20)), lost_for(15));
        assert_eq!(watch.answered(&lost, after(start, 25)), None);
        assert_eq!(watch.answered(&lost, after(start, 30)), None);
        assert_eq!(watch.answered(&lost, after(start, 35)), lost_for(30));
    }

    #[test]
    fn a_read_in_another_state_ends_the_loss() {
        let start = Instant::now();
        let mut watch = RunWatch::new(STAR_LOST_AFTER, start);
        let lost = stats(GuiderState::StarLost);
        watch.answered(&lost, after(start, 5));
        watch.answered(&stats(GuiderState::Guiding), after(start, 10));

        assert_eq!(watch.answered(&lost, after(start, 15)), None);
        assert_eq!(watch.answered(&lost, after(start, 25)), None);
        assert_eq!(
            watch.answered(&lost, after(start, 30)),
            Some(Report::StarLost {
                lost_for: Duration::from_secs(15)
            })
        );
    }

    #[test]
    fn a_read_that_fails_neither_ends_a_loss_nor_reports_it() {
        let start = Instant::now();
        let mut watch = RunWatch::new(STAR_LOST_AFTER, start);
        let lost = stats(GuiderState::StarLost);
        watch.answered(&lost, after(start, 5));

        assert_eq!(watch.unanswered(after(start, 20)), None);
        assert_eq!(
            watch.answered(&lost, after(start, 25)),
            Some(Report::StarLost {
                lost_for: Duration::from_secs(20)
            })
        );
    }

    /// How the fake answers a read or a stop.
    #[derive(Debug, Clone)]
    enum Read {
        Stats(Box<GuidingStats>),
        Unreachable,
        /// No answer, ever.
        Hang,
    }

    /// A guider whose start settles, whose stats answer as `read` says
    /// and whose stop answers as `stop` says, waiting for `stop_gate`
    /// when one is set.
    struct ScriptedGuider {
        start: Mutex<Result<(), &'static str>>,
        read: Mutex<Read>,
        reads: AtomicUsize,
        stop: Mutex<Result<(), &'static str>>,
        stop_gate: Mutex<Option<Arc<Notify>>>,
    }

    impl ScriptedGuider {
        fn guiding() -> Arc<Self> {
            Arc::new(Self {
                start: Mutex::new(Ok(())),
                read: Mutex::new(Read::Stats(Box::new(stats(GuiderState::Guiding)))),
                reads: AtomicUsize::new(0),
                stop: Mutex::new(Ok(())),
                stop_gate: Mutex::new(None),
            })
        }

        fn answer_reads(&self, read: Read) {
            *self.read.lock().unwrap() = read;
        }

        fn reads(&self) -> usize {
            self.reads.load(Ordering::SeqCst)
        }
    }

    fn refused<T>(code: &str) -> Result<T, GuiderError> {
        Err(GuiderError::Service {
            code: code.to_string(),
            message: "refused".to_string(),
            details: serde_json::Value::Null,
        })
    }

    fn settled() -> SettledOutcome {
        SettledOutcome {
            state: GuiderState::Guiding,
            rms_ra_px: None,
            rms_dec_px: None,
            total_rms_px: None,
            rms_ra_arcsec: None,
            rms_dec_arcsec: None,
            total_rms_arcsec: None,
            sample_count: 0,
        }
    }

    #[async_trait]
    impl GuiderClient for ScriptedGuider {
        async fn start_guiding(
            &self,
            _: StartGuidingRequest,
        ) -> Result<SettledOutcome, GuiderError> {
            let start = *self.start.lock().unwrap();
            match start {
                Ok(()) => Ok(settled()),
                Err(code) => refused(code),
            }
        }
        async fn stop_guiding(&self) -> Result<GuiderState, GuiderError> {
            let gate = self.stop_gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.notified().await;
            }
            let stop = *self.stop.lock().unwrap();
            match stop {
                Ok(()) => Ok(GuiderState::Idle),
                Err(code) => refused(code),
            }
        }
        async fn pause_guiding(&self, _: bool) -> Result<GuiderState, GuiderError> {
            Ok(GuiderState::Paused)
        }
        async fn resume_guiding(&self) -> Result<GuiderState, GuiderError> {
            Ok(GuiderState::Guiding)
        }
        async fn dither(&self, _: DitherRequest) -> Result<SettledOutcome, GuiderError> {
            Ok(settled())
        }
        async fn abort_dither(&self) -> Result<bool, GuiderError> {
            Ok(true)
        }
        async fn guiding_stats(&self) -> Result<GuidingStats, GuiderError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            let read = self.read.lock().unwrap().clone();
            match read {
                Read::Stats(stats) => Ok(*stats),
                Read::Unreachable => Err(GuiderError::ServiceUnreachable(
                    "connection refused".to_string(),
                )),
                Read::Hang => std::future::pending().await,
            }
        }
        async fn guiding_metrics(&self) -> Result<GuidingMetrics, GuiderError> {
            Ok(GuidingMetrics {
                state: GuiderState::Guiding,
                guiding: true,
                epoch: "test-epoch".to_string(),
                frames: Vec::new(),
            })
        }
        async fn devices(&self) -> Result<GuiderDevices, GuiderError> {
            Ok(GuiderDevices {
                camera: None,
                mount: None,
            })
        }
        async fn clear_calibration(&self) -> Result<GuiderState, GuiderError> {
            Ok(GuiderState::Idle)
        }
        async fn reselect_star(&self) -> Result<GuiderState, GuiderError> {
            Ok(GuiderState::Guiding)
        }
    }

    struct Rig {
        guider: Arc<ScriptedGuider>,
        watched: Arc<WatchedGuider>,
        events: Receiver<EventEnvelope>,
    }

    impl Rig {
        fn new() -> Self {
            let guider = ScriptedGuider::guiding();
            let event_bus = Arc::new(EventBus::from_config(&[], None).unwrap());
            let events = event_bus.subscribe();
            let inner: Arc<dyn GuiderClient> = guider.clone();
            let watched = Arc::new(WatchedGuider::new(inner, event_bus, STAR_LOST_AFTER));
            Self {
                guider,
                watched,
                events,
            }
        }

        async fn settled() -> Self {
            let rig = Self::new();
            rig.watched
                .start_guiding(StartGuidingRequest::default())
                .await
                .unwrap();
            rig
        }

        /// The events emitted so far, as `(event, payload)`.
        fn emitted(&mut self) -> Vec<(String, serde_json::Value)> {
            let mut emitted = Vec::new();
            loop {
                match self.events.try_recv() {
                    Ok(envelope) => emitted.push((envelope.event, envelope.payload)),
                    Err(TryRecvError::Empty) => return emitted,
                    Err(e) => panic!("the event bus failed the test's receiver: {e}"),
                }
            }
        }
    }

    /// Let the paused clock run for `seconds`, with the watch's task
    /// running whatever falls due.
    async fn pass(seconds: u64) {
        tokio::time::sleep(Duration::from_secs(seconds)).await;
        tokio::task::yield_now().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_settled_run_is_read_every_5_s_and_a_guiding_one_emits_nothing() {
        let mut rig = Rig::settled().await;

        pass(4).await;
        assert_eq!(
            rig.guider.reads(),
            0,
            "the first read comes 5 s after the settle"
        );
        pass(12).await;

        assert_eq!(rig.guider.reads(), 3);
        assert_eq!(rig.emitted(), vec![]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_run_the_guider_stopped_is_reported_once_and_read_no_more() {
        let mut rig = Rig::settled().await;
        rig.guider.answer_reads(Read::Stats(Box::new(stopped_for(
            GuiderState::Idle,
            "mount_slewing",
        ))));

        pass(31).await;

        assert_eq!(
            rig.emitted(),
            vec![(
                "guide_stopped".to_string(),
                json!({ "reason": "guider", "stop_reason": "mount_slewing" })
            )]
        );
        assert_eq!(rig.guider.reads(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_guider_that_refuses_every_read_is_reported_unreachable_after_60_s() {
        let mut rig = Rig::settled().await;
        rig.guider.answer_reads(Read::Unreachable);

        pass(59).await;
        assert_eq!(rig.emitted(), vec![]);
        pass(2).await;

        assert_eq!(
            rig.emitted(),
            vec![(
                "guide_stopped".to_string(),
                json!({ "reason": "guider", "stop_reason": "unreachable" })
            )]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_read_that_never_answers_is_given_up_after_5_s() {
        let mut rig = Rig::settled().await;
        rig.guider.answer_reads(Read::Hang);

        pass(59).await;
        assert_eq!(rig.emitted(), vec![]);
        assert_eq!(
            rig.guider.reads(),
            6,
            "a read every 10 s from the fifth second on: 5 s out, 5 s apart"
        );
        pass(2).await;

        assert_eq!(
            rig.emitted(),
            vec![(
                "guide_stopped".to_string(),
                json!({ "reason": "guider", "stop_reason": "unreachable" })
            )]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_lasting_loss_emits_guide_star_lost_with_how_long_it_has_lasted() {
        let mut rig = Rig::settled().await;
        rig.guider
            .answer_reads(Read::Stats(Box::new(stats(GuiderState::StarLost))));

        pass(21).await;

        assert_eq!(
            rig.emitted(),
            vec![(
                "guide_star_lost".to_string(),
                json!({ "lost_for_ms": 15_000 })
            )]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_start_that_fails_is_watched_by_no_one() {
        let mut rig = Rig::new();
        *rig.guider.start.lock().unwrap() = Err("no_stars");

        let err = rig
            .watched
            .start_guiding(StartGuidingRequest::default())
            .await
            .unwrap_err();
        pass(31).await;

        assert_eq!(err.code(), Some("no_stars"));
        assert_eq!(rig.guider.reads(), 0);
        assert_eq!(rig.emitted(), vec![]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_the_guider_answers_ends_the_watch() {
        let mut rig = Rig::settled().await;

        let state = rig.watched.stop_guiding().await.unwrap();
        rig.guider.answer_reads(Read::Stats(Box::new(stopped_for(
            GuiderState::Idle,
            "requested",
        ))));
        pass(31).await;

        assert_eq!(state, GuiderState::Idle);
        assert_eq!(rig.guider.reads(), 0);
        assert_eq!(rig.emitted(), vec![]);
    }

    #[test]
    fn a_stop_is_taken_when_answered_with_a_state_or_stop_timeout_and_not_otherwise() {
        assert!(stop_taken(&Ok(GuiderState::Idle)));
        assert!(stop_taken(&refused("stop_timeout")));
        assert!(!stop_taken(&refused("internal")));
        assert!(!stop_taken(&Err(GuiderError::ServiceUnreachable(
            "connection refused".to_string()
        ))));
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_the_guider_answers_with_stop_timeout_ends_the_watch() {
        let mut rig = Rig::settled().await;
        *rig.guider.stop.lock().unwrap() = Err("stop_timeout");

        let err = rig.watched.stop_guiding().await.unwrap_err();
        rig.guider.answer_reads(Read::Stats(Box::new(stopped_for(
            GuiderState::Idle,
            "requested",
        ))));
        pass(31).await;

        assert_eq!(err.code(), Some("stop_timeout"));
        assert_eq!(rig.guider.reads(), 0);
        assert_eq!(rig.emitted(), vec![]);
    }

    #[tokio::test(start_paused = true)]
    async fn the_watch_goes_on_after_a_stop_the_guider_failed() {
        let mut rig = Rig::settled().await;
        *rig.guider.stop.lock().unwrap() = Err("internal");

        let err = rig.watched.stop_guiding().await.unwrap_err();
        rig.guider.answer_reads(Read::Stats(Box::new(stopped_for(
            GuiderState::Idle,
            "camera_error",
        ))));
        pass(6).await;

        assert_eq!(err.code(), Some("internal"));
        assert_eq!(
            rig.emitted(),
            vec![(
                "guide_stopped".to_string(),
                json!({ "reason": "guider", "stop_reason": "camera_error" })
            )]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_is_read_while_a_stop_of_rps_is_in_flight() {
        let mut rig = Rig::settled().await;
        let gate = Arc::new(Notify::new());
        *rig.guider.stop_gate.lock().unwrap() = Some(Arc::clone(&gate));
        *rig.guider.stop.lock().unwrap() = Err("internal");
        let watched = Arc::clone(&rig.watched);
        let stop = tokio::spawn(async move { watched.stop_guiding().await });

        pass(21).await;
        assert_eq!(rig.guider.reads(), 0);
        gate.notify_one();
        stop.await.unwrap().unwrap_err();
        pass(5).await;

        assert_eq!(
            rig.guider.reads(),
            1,
            "the reads go on once the stop has failed"
        );
        assert_eq!(rig.emitted(), vec![]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_abandoned_before_its_answer_lets_the_watch_read_again() {
        let rig = Rig::settled().await;
        let gate = Arc::new(Notify::new());
        *rig.guider.stop_gate.lock().unwrap() = Some(gate);
        let watched = Arc::clone(&rig.watched);
        let stop = tokio::spawn(async move { watched.stop_guiding().await });
        pass(6).await;

        stop.abort();
        pass(5).await;

        assert_eq!(rig.guider.reads(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_settle_replaces_the_watch_of_the_run_before() {
        let rig = Rig::settled().await;
        pass(3).await;

        rig.watched
            .start_guiding(StartGuidingRequest::default())
            .await
            .unwrap();
        pass(4).await;
        assert_eq!(
            rig.guider.reads(),
            0,
            "the earlier run's watch reads no more"
        );
        pass(2).await;

        assert_eq!(rig.guider.reads(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_dropped_client_ends_its_watch() {
        let rig = Rig::settled().await;
        let guider = Arc::clone(&rig.guider);

        drop(rig);
        pass(31).await;

        assert_eq!(guider.reads(), 0);
    }

    #[tokio::test]
    async fn every_other_call_goes_to_the_guider_as_it_is() {
        let rig = Rig::new();
        let watched = &rig.watched;

        assert_eq!(
            watched.pause_guiding(false).await.unwrap(),
            GuiderState::Paused
        );
        assert_eq!(
            watched.resume_guiding().await.unwrap(),
            GuiderState::Guiding
        );
        let dither = DitherRequest {
            amount: rp_guider::DitherAmount::Px(3.0),
            ra_only: false,
            settle: None,
        };
        assert_eq!(watched.dither(dither).await.unwrap().sample_count, 0);
        assert!(watched.abort_dither().await.unwrap());
        assert!(watched.guiding_stats().await.unwrap().guiding);
        assert_eq!(watched.guiding_metrics().await.unwrap().epoch, "test-epoch");
        assert!(watched.devices().await.unwrap().camera.is_none());
        assert_eq!(
            watched.clear_calibration().await.unwrap(),
            GuiderState::Idle
        );
        assert_eq!(watched.reselect_star().await.unwrap(), GuiderState::Guiding);
    }
}
