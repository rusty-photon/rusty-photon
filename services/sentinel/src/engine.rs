//! Engine: orchestrates monitors, transitions, and notifiers

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio_util::sync::CancellationToken;

use crate::config::{TransitionConfig, TransitionDirection};
use crate::monitor::{Monitor, MonitorState, StateChange};
use crate::notifier::{Notification, NotificationRecord, Notifier};
use crate::state::StateHandle;
use crate::watchdog::EventMonitor;

/// The engine orchestrates polling monitors and dispatching notifications
pub struct Engine {
    monitors: Vec<Arc<dyn Monitor>>,
    event_monitors: Vec<Arc<dyn EventMonitor>>,
    notifiers: Vec<Arc<dyn Notifier>>,
    transitions: Vec<TransitionConfig>,
    state: StateHandle,
    cancel: CancellationToken,
}

impl Engine {
    pub fn new(
        monitors: Vec<Arc<dyn Monitor>>,
        event_monitors: Vec<Arc<dyn EventMonitor>>,
        notifiers: Vec<Arc<dyn Notifier>>,
        transitions: Vec<TransitionConfig>,
        state: StateHandle,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            monitors,
            event_monitors,
            notifiers,
            transitions,
            state,
            cancel,
        }
    }

    /// Connect all monitors
    pub async fn connect_all(&self) {
        for monitor in &self.monitors {
            tracing::debug!("Connecting monitor '{}'", monitor.name());
            if let Err(e) = monitor.connect().await {
                tracing::warn!("Failed to connect monitor '{}': {}", monitor.name(), e);
            }
        }
    }

    /// Disconnect all monitors
    pub async fn disconnect_all(&self) {
        for monitor in &self.monitors {
            tracing::debug!("Disconnecting monitor '{}'", monitor.name());
            if let Err(e) = monitor.disconnect().await {
                tracing::warn!("Failed to disconnect monitor '{}': {}", monitor.name(), e);
            }
        }
    }

    /// Start polling all monitors. Returns when the cancellation token is triggered.
    pub async fn run(&self) {
        let mut handles = Vec::new();

        for monitor in &self.monitors {
            let monitor = Arc::clone(monitor);
            let interval = monitor.polling_interval();
            let state = Arc::clone(&self.state);
            let transitions = self.transitions.clone();
            let notifiers: Vec<Arc<dyn Notifier>> = self.notifiers.clone();
            let cancel = self.cancel.clone();

            let handle = tokio::spawn(async move {
                poll_loop(monitor, state, transitions, notifiers, interval, cancel).await;
            });
            handles.push(handle);
        }

        // Event monitors (e.g. the operation watchdog) own a long-lived
        // connection and run until cancelled, in parallel with the poll loops.
        for event_monitor in &self.event_monitors {
            let event_monitor = Arc::clone(event_monitor);
            let cancel = self.cancel.clone();
            let handle = tokio::spawn(async move {
                event_monitor.run(cancel).await;
            });
            handles.push(handle);
        }

        // Wait for cancellation
        self.cancel.cancelled().await;

        // Wait for all polling tasks to finish
        for handle in handles {
            let _ = handle.await;
        }
    }
}

/// How many polls in a row a monitor fails before it is unreadable.
pub const UNREADABLE_AFTER_FAILED_POLLS: u32 = 5;

/// Tells, from one monitor's polls, the state changes transition rules see
/// (`docs/services/sentinel.md` § What counts as a change).
#[derive(Debug, Default)]
struct ChangeDetector {
    /// The last `Safe` or `Unsafe` reading.
    last_known: Option<MonitorState>,
    /// Polls in a row that read `Unknown`.
    failed_polls: u32,
}

impl ChangeDetector {
    /// The change a poll's reading makes, if any.
    fn observe(
        &mut self,
        monitor_name: &str,
        reading: MonitorState,
        now_ms: u64,
    ) -> Option<StateChange> {
        let previous = self.last_known.unwrap_or(MonitorState::Unknown);
        let change = |readable_again| StateChange {
            monitor_name: monitor_name.to_string(),
            previous,
            current: reading,
            readable_again,
            timestamp_epoch_ms: now_ms,
        };
        if reading == MonitorState::Unknown {
            self.failed_polls = self.failed_polls.saturating_add(1);
            return (self.failed_polls == UNREADABLE_AFTER_FAILED_POLLS).then(|| change(false));
        }
        let readable_again = self.failed_polls >= UNREADABLE_AFTER_FAILED_POLLS;
        self.failed_polls = 0;
        self.last_known = Some(reading);
        let differs = previous != MonitorState::Unknown && previous != reading;
        (differs || readable_again).then(|| change(readable_again))
    }
}

async fn poll_loop(
    monitor: Arc<dyn Monitor>,
    state: StateHandle,
    transitions: Vec<TransitionConfig>,
    notifiers: Vec<Arc<dyn Notifier>>,
    interval: Duration,
    cancel: CancellationToken,
) {
    let monitor_name = monitor.name().to_string();
    let mut detector = ChangeDetector::default();
    loop {
        // Poll the monitor; shutdown abandons a poll in flight
        let reading = tokio::select! {
            reading = monitor.poll() => reading,
            () = cancel.cancelled() => {
                tracing::debug!("Polling loop for '{}' cancelled mid-poll", monitor_name);
                break;
            }
        };
        let now_ms = current_epoch_ms();

        let previous_poll = {
            let mut state_lock = state.write().await;
            let previous = state_lock.get_monitor_state(&monitor_name);
            state_lock.update_monitor(&monitor_name, reading, now_ms);
            drop(state_lock);
            previous.unwrap_or(MonitorState::Unknown)
        };
        tracing::debug!(
            "Poll '{}': {:?} -> {:?}",
            monitor_name,
            previous_poll,
            reading
        );

        // A change is measured from the last known reading, not the last
        // poll; check it against the transition rules
        if let Some(change) = detector.observe(&monitor_name, reading, now_ms) {
            if change.current == MonitorState::Unknown {
                tracing::warn!(
                    "Monitor '{}' has {} consecutive errors",
                    monitor_name,
                    UNREADABLE_AFTER_FAILED_POLLS
                );
            }
            tracing::debug!(
                "State change for '{}': {:?} -> {:?} (readable again: {})",
                monitor_name,
                change.previous,
                change.current,
                change.readable_again
            );
            dispatch_notifications(&change, &transitions, &notifiers, &state).await;
        }

        // Wait for the next poll or cancellation
        tokio::select! {
            () = tokio::time::sleep(interval) => {}
            () = cancel.cancelled() => {
                tracing::debug!("Polling loop for '{}' cancelled", monitor_name);
                break;
            }
        }
    }
}

/// Check transition rules and dispatch matching notifications
pub async fn dispatch_notifications(
    change: &StateChange,
    transitions: &[TransitionConfig],
    notifiers: &[Arc<dyn Notifier>],
    state: &StateHandle,
) {
    let monitor_name = change.monitor_name.as_str();
    for transition in transitions {
        if transition.monitor_name != monitor_name {
            continue;
        }

        if !matches_direction(&transition.direction, change) {
            continue;
        }

        let message = transition
            .message_template()
            .replace("%monitor_name%", monitor_name)
            .replace("%new_state%", &change.current.to_string());

        let notification = Notification {
            title: String::new(),
            message: message.clone(),
            priority: transition.priority.unwrap_or(0),
            sound: transition.sound.clone(),
        };

        for notifier_type in &transition.notifiers {
            if let Some(notifier) = notifiers.iter().find(|n| n.type_name() == notifier_type) {
                tracing::debug!(
                    "Dispatching to '{}' for '{}': {}",
                    notifier_type,
                    monitor_name,
                    message
                );

                let result = notifier.notify(&notification).await;
                let record = NotificationRecord {
                    monitor_name: monitor_name.to_string(),
                    notifier_type: notifier_type.clone(),
                    message: message.clone(),
                    success: result.is_ok(),
                    error: result.as_ref().err().map(std::string::ToString::to_string),
                    timestamp_epoch_ms: change.timestamp_epoch_ms,
                };

                if let Err(e) = &result {
                    tracing::warn!(
                        "Notification via '{}' for '{}' failed: {}",
                        notifier_type,
                        monitor_name,
                        e
                    );
                }

                state.write().await.add_notification(record);
            }
        }
    }
}

/// Check if a state change matches a direction rule
#[must_use]
pub fn matches_direction(direction: &TransitionDirection, change: &StateChange) -> bool {
    let to_unsafe = change.previous == MonitorState::Safe && change.current == MonitorState::Unsafe;
    let to_safe = change.previous == MonitorState::Unsafe && change.current == MonitorState::Safe;
    match direction {
        TransitionDirection::SafeToUnsafe => to_unsafe,
        TransitionDirection::UnsafeToSafe => to_safe,
        TransitionDirection::Both => to_unsafe || to_safe,
        TransitionDirection::Unreadable => {
            change.current == MonitorState::Unknown || change.readable_again
        }
    }
}

fn current_epoch_ms() -> u64 {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    // Overflows u64 in the year 584556019. Saturate rather than wrap.
    u64::try_from(ms).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::state::new_state_handle;
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use MonitorState::{Safe, Unknown, Unsafe};

    const NAME: &str = "Roof Monitor";

    fn change(previous: MonitorState, current: MonitorState, readable_again: bool) -> StateChange {
        StateChange {
            monitor_name: NAME.to_string(),
            previous,
            current,
            readable_again,
            timestamp_epoch_ms: 1_000,
        }
    }

    /// Feed `readings` to a fresh detector and return the change each makes.
    fn observe_all(readings: &[MonitorState]) -> Vec<Option<StateChange>> {
        let mut detector = ChangeDetector::default();
        readings
            .iter()
            .map(|&reading| detector.observe(NAME, reading, 1_000))
            .collect()
    }

    fn failed_polls(n: u32) -> Vec<MonitorState> {
        vec![Unknown; n as usize]
    }

    // ---- matches_direction ------------------------------------------------

    #[test]
    fn safe_to_unsafe_matches_correct_direction() {
        let direction = TransitionDirection::SafeToUnsafe;
        assert!(matches_direction(&direction, &change(Safe, Unsafe, false)));
        assert!(!matches_direction(&direction, &change(Unsafe, Safe, false)));
    }

    #[test]
    fn unsafe_to_safe_matches_correct_direction() {
        let direction = TransitionDirection::UnsafeToSafe;
        assert!(matches_direction(&direction, &change(Unsafe, Safe, false)));
        assert!(!matches_direction(&direction, &change(Safe, Unsafe, false)));
    }

    #[test]
    fn both_matches_either_direction() {
        let direction = TransitionDirection::Both;
        assert!(matches_direction(&direction, &change(Safe, Unsafe, false)));
        assert!(matches_direction(&direction, &change(Unsafe, Safe, false)));
    }

    #[test]
    fn a_change_from_no_known_reading_matches_no_safe_unsafe_rule() {
        for direction in [
            TransitionDirection::SafeToUnsafe,
            TransitionDirection::UnsafeToSafe,
            TransitionDirection::Both,
        ] {
            assert!(!matches_direction(&direction, &change(Unknown, Safe, true)));
            assert!(!matches_direction(
                &direction,
                &change(Unknown, Unsafe, true)
            ));
        }
    }

    #[test]
    fn becoming_unreadable_matches_only_unreadable() {
        let unreadable = change(Safe, Unknown, false);
        assert!(matches_direction(
            &TransitionDirection::Unreadable,
            &unreadable
        ));
        for direction in [
            TransitionDirection::SafeToUnsafe,
            TransitionDirection::UnsafeToSafe,
            TransitionDirection::Both,
        ] {
            assert!(!matches_direction(&direction, &unreadable));
        }
    }

    #[test]
    fn readable_again_matches_unreadable_and_the_reading_s_own_direction() {
        let back_unsafe = change(Safe, Unsafe, true);
        assert!(matches_direction(
            &TransitionDirection::Unreadable,
            &back_unsafe
        ));
        assert!(matches_direction(
            &TransitionDirection::SafeToUnsafe,
            &back_unsafe
        ));
        assert!(matches_direction(
            &TransitionDirection::Unreadable,
            &change(Safe, Safe, true)
        ));
    }

    #[test]
    fn unreadable_does_not_match_a_plain_reading_change() {
        assert!(!matches_direction(
            &TransitionDirection::Unreadable,
            &change(Safe, Unsafe, false)
        ));
    }

    // ---- ChangeDetector ---------------------------------------------------

    #[test]
    fn the_first_reading_is_no_change() {
        assert_eq!(observe_all(&[Safe]), vec![None]);
        assert_eq!(observe_all(&[Unsafe]), vec![None]);
    }

    #[test]
    fn a_reading_that_differs_from_the_last_known_one_is_a_change() {
        assert_eq!(
            observe_all(&[Safe, Unsafe, Safe]),
            vec![
                None,
                Some(change(Safe, Unsafe, false)),
                Some(change(Unsafe, Safe, false)),
            ]
        );
    }

    #[test]
    fn the_same_reading_again_is_no_change() {
        assert_eq!(observe_all(&[Safe, Safe]), vec![None, None]);
    }

    #[test]
    fn a_failed_poll_between_two_readings_does_not_hide_their_change() {
        assert_eq!(
            observe_all(&[Safe, Unknown, Unsafe]),
            vec![None, None, Some(change(Safe, Unsafe, false))]
        );
    }

    #[test]
    fn a_short_outage_ending_on_the_last_known_reading_is_no_change() {
        let mut readings = vec![Safe];
        readings.extend(failed_polls(UNREADABLE_AFTER_FAILED_POLLS - 1));
        readings.push(Safe);

        assert!(observe_all(&readings).iter().all(Option::is_none));
    }

    #[test]
    fn the_fifth_failed_poll_makes_the_monitor_unreadable_once() {
        let mut readings = vec![Safe];
        readings.extend(failed_polls(3 * UNREADABLE_AFTER_FAILED_POLLS));

        let changes = observe_all(&readings);

        let fifth = UNREADABLE_AFTER_FAILED_POLLS as usize;
        assert_eq!(changes[fifth], Some(change(Safe, Unknown, false)));
        let others = changes
            .iter()
            .enumerate()
            .filter(|&(i, c)| i != fifth && c.is_some())
            .count();
        assert_eq!(others, 0, "{changes:?}");
    }

    #[test]
    fn the_first_reading_after_an_outage_is_readable_again() {
        let mut readings = vec![Safe];
        readings.extend(failed_polls(UNREADABLE_AFTER_FAILED_POLLS));
        readings.extend([Safe, Safe]);

        let changes = observe_all(&readings);

        let n = changes.len();
        assert_eq!(changes[n - 2], Some(change(Safe, Safe, true)));
        assert_eq!(changes[n - 1], None);
    }

    #[test]
    fn a_reading_after_an_outage_that_differs_is_readable_again_and_a_change() {
        let mut readings = vec![Safe];
        readings.extend(failed_polls(UNREADABLE_AFTER_FAILED_POLLS));
        readings.push(Unsafe);

        let changes = observe_all(&readings);

        assert_eq!(changes.last(), Some(&Some(change(Safe, Unsafe, true))));
    }

    #[test]
    fn an_outage_before_any_reading_is_measured_from_no_known_reading() {
        let mut readings = failed_polls(UNREADABLE_AFTER_FAILED_POLLS);
        readings.push(Unsafe);

        let changes = observe_all(&readings);

        let fifth = UNREADABLE_AFTER_FAILED_POLLS as usize - 1;
        assert_eq!(changes[fifth], Some(change(Unknown, Unknown, false)));
        assert_eq!(changes.last(), Some(&Some(change(Unknown, Unsafe, true))));
    }

    #[test]
    fn a_change_carries_the_monitor_name_and_the_poll_s_time() {
        let mut detector = ChangeDetector::default();
        detector.observe("Dome", Safe, 1);

        let change = detector.observe("Dome", Unsafe, 42).unwrap();

        assert_eq!(change.monitor_name, "Dome");
        assert_eq!(change.timestamp_epoch_ms, 42);
    }

    // ---- dispatch and the poll loop ---------------------------------------

    #[derive(Debug, Default)]
    struct RecordingNotifier {
        sent: Mutex<Vec<String>>,
    }

    impl RecordingNotifier {
        fn sent(&self) -> Vec<String> {
            self.sent.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Notifier for RecordingNotifier {
        fn type_name(&self) -> &'static str {
            "recorder"
        }

        async fn notify(&self, notification: &Notification) -> crate::Result<()> {
            self.sent.lock().unwrap().push(notification.message.clone());
            Ok(())
        }
    }

    fn rule(monitor_name: &str, direction: TransitionDirection) -> TransitionConfig {
        TransitionConfig {
            monitor_name: monitor_name.to_string(),
            direction,
            notifiers: vec!["recorder".to_string()],
            message_template: Some("%monitor_name% is now %new_state%".to_string()),
            priority: None,
            sound: None,
        }
    }

    #[tokio::test]
    async fn dispatch_sends_every_matching_rule_and_records_the_change_s_time() {
        let recorder = Arc::new(RecordingNotifier::default());
        let notifiers: Vec<Arc<dyn Notifier>> = vec![Arc::clone(&recorder) as Arc<dyn Notifier>];
        let state = new_state_handle(vec![], 10);
        let transitions = vec![
            rule(NAME, TransitionDirection::SafeToUnsafe),
            rule(NAME, TransitionDirection::Unreadable),
            rule(NAME, TransitionDirection::UnsafeToSafe),
            rule("Dome", TransitionDirection::Unreadable),
        ];

        dispatch_notifications(
            &change(Safe, Unsafe, true),
            &transitions,
            &notifiers,
            &state,
        )
        .await;

        assert_eq!(
            recorder.sent(),
            vec!["Roof Monitor is now Unsafe", "Roof Monitor is now Unsafe"]
        );
        let history = state.read().await.history.clone();
        assert_eq!(history.len(), 2);
        assert!(history.iter().all(|r| r.timestamp_epoch_ms == 1_000));
    }

    /// Answers each poll with the next reading of its script, then repeats
    /// the last one.
    #[derive(Debug)]
    struct ScriptedMonitor {
        readings: Mutex<VecDeque<MonitorState>>,
    }

    #[async_trait]
    impl Monitor for ScriptedMonitor {
        fn name(&self) -> &'static str {
            NAME
        }

        async fn poll(&self) -> MonitorState {
            let mut readings = self.readings.lock().unwrap();
            if readings.len() > 1 {
                readings.pop_front().unwrap()
            } else {
                readings[0]
            }
        }

        async fn connect(&self) -> crate::Result<()> {
            Ok(())
        }

        async fn disconnect(&self) -> crate::Result<()> {
            Ok(())
        }

        fn polling_interval(&self) -> Duration {
            Duration::from_secs(1)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_poll_loop_notifies_a_change_across_a_failed_poll() {
        let monitor: Arc<dyn Monitor> = Arc::new(ScriptedMonitor {
            readings: Mutex::new(VecDeque::from([Safe, Unknown, Unsafe])),
        });
        let recorder = Arc::new(RecordingNotifier::default());
        let notifiers: Vec<Arc<dyn Notifier>> = vec![Arc::clone(&recorder) as Arc<dyn Notifier>];
        let state = new_state_handle(vec![(NAME.to_string(), Duration::from_secs(1))], 10);
        let cancel = CancellationToken::new();

        let polling = tokio::spawn(poll_loop(
            monitor,
            Arc::clone(&state),
            vec![rule(NAME, TransitionDirection::SafeToUnsafe)],
            notifiers,
            Duration::from_secs(1),
            cancel.clone(),
        ));
        // Past the script's three polls; the last reading repeats after it.
        tokio::time::sleep(Duration::from_secs(10)).await;
        cancel.cancel();
        polling.await.unwrap();

        assert_eq!(recorder.sent(), vec!["Roof Monitor is now Unsafe"]);
        assert_eq!(state.read().await.monitors[0].state, Unsafe);
    }

    /// A monitor whose poll never returns.
    #[derive(Debug)]
    struct SilentMonitor;

    #[async_trait]
    impl Monitor for SilentMonitor {
        fn name(&self) -> &'static str {
            NAME
        }

        async fn poll(&self) -> MonitorState {
            std::future::pending().await
        }

        async fn connect(&self) -> crate::Result<()> {
            Ok(())
        }

        async fn disconnect(&self) -> crate::Result<()> {
            Ok(())
        }

        fn polling_interval(&self) -> Duration {
            Duration::from_secs(1)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_abandons_a_poll_in_flight() {
        let state = new_state_handle(vec![(NAME.to_string(), Duration::from_secs(1))], 10);
        let cancel = CancellationToken::new();
        let polling = tokio::spawn(poll_loop(
            Arc::new(SilentMonitor),
            state,
            vec![],
            vec![],
            Duration::from_secs(1),
            cancel.clone(),
        ));
        tokio::time::sleep(Duration::from_secs(1)).await;

        cancel.cancel();

        tokio::time::timeout(Duration::from_secs(1), polling)
            .await
            .expect("the poll loop waited out its poll")
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn the_poll_loop_notifies_an_unreadable_monitor_once() {
        let monitor: Arc<dyn Monitor> = Arc::new(ScriptedMonitor {
            readings: Mutex::new(VecDeque::from([Safe, Unknown])),
        });
        let recorder = Arc::new(RecordingNotifier::default());
        let notifiers: Vec<Arc<dyn Notifier>> = vec![Arc::clone(&recorder) as Arc<dyn Notifier>];
        let state = new_state_handle(vec![(NAME.to_string(), Duration::from_secs(1))], 10);
        let cancel = CancellationToken::new();

        let polling = tokio::spawn(poll_loop(
            monitor,
            state,
            vec![rule(NAME, TransitionDirection::Unreadable)],
            notifiers,
            Duration::from_secs(1),
            cancel.clone(),
        ));
        // Three times the polls an outage takes to make the monitor unreadable.
        tokio::time::sleep(Duration::from_secs(
            3 * u64::from(UNREADABLE_AFTER_FAILED_POLLS),
        ))
        .await;
        cancel.cancel();
        polling.await.unwrap();

        assert_eq!(recorder.sent(), vec!["Roof Monitor is now Unknown"]);
    }

    #[test]
    fn current_epoch_ms_returns_reasonable_value() {
        let now = current_epoch_ms();
        // Should be after 2024-01-01 (1704067200000 ms)
        assert!(now > 1_704_067_200_000);
        // Should be within a few seconds of now
        let check = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert!(check.abs_diff(now) < 1000);
    }
}
