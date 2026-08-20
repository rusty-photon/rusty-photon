//! Engine: orchestrates monitors, transitions, and notifiers

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio_util::sync::CancellationToken;

use crate::config::{TransitionConfig, TransitionDirection};
use crate::monitor::{Monitor, MonitorState};
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

async fn poll_loop(
    monitor: Arc<dyn Monitor>,
    state: StateHandle,
    transitions: Vec<TransitionConfig>,
    notifiers: Vec<Arc<dyn Notifier>>,
    interval: Duration,
    cancel: CancellationToken,
) {
    loop {
        // Poll the monitor
        let new_state = monitor.poll().await;
        let now_ms = current_epoch_ms();
        let monitor_name = monitor.name().to_string();

        // Get the previous state and update
        let (changed, previous_state) = {
            let mut state_lock = state.write().await;
            let previous = state_lock.get_monitor_state(&monitor_name);
            let changed = state_lock.update_monitor(&monitor_name, new_state, now_ms);
            let errors = state_lock.get_monitor_consecutive_errors(&monitor_name);
            drop(state_lock);
            if errors == 5 {
                tracing::warn!(
                    "Monitor '{}' has {} consecutive errors",
                    monitor_name,
                    errors
                );
            }
            (changed, previous.unwrap_or(MonitorState::Unknown))
        };

        tracing::debug!(
            "Poll '{}': {:?} -> {:?} (changed={})",
            monitor_name,
            previous_state,
            new_state,
            changed
        );

        // If state changed, check transition rules and dispatch notifications
        if changed {
            dispatch_notifications(
                &monitor_name,
                previous_state,
                new_state,
                &transitions,
                &notifiers,
                &state,
                now_ms,
            )
            .await;
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
    monitor_name: &str,
    previous: MonitorState,
    current: MonitorState,
    transitions: &[TransitionConfig],
    notifiers: &[Arc<dyn Notifier>],
    state: &StateHandle,
    now_ms: u64,
) {
    for transition in transitions {
        if transition.monitor_name != monitor_name {
            continue;
        }

        if !matches_direction(&transition.direction, previous, current) {
            continue;
        }

        let message = transition
            .message_template
            .replace("%monitor_name%", monitor_name)
            .replace("%new_state%", &current.to_string());

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
                    timestamp_epoch_ms: now_ms,
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

/// Check if a state transition matches a direction rule
#[must_use]
pub fn matches_direction(
    direction: &TransitionDirection,
    previous: MonitorState,
    current: MonitorState,
) -> bool {
    match direction {
        TransitionDirection::SafeToUnsafe => {
            previous == MonitorState::Safe && current == MonitorState::Unsafe
        }
        TransitionDirection::UnsafeToSafe => {
            previous == MonitorState::Unsafe && current == MonitorState::Safe
        }
        TransitionDirection::Both => {
            (previous == MonitorState::Safe && current == MonitorState::Unsafe)
                || (previous == MonitorState::Unsafe && current == MonitorState::Safe)
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

    #[test]
    fn safe_to_unsafe_matches_correct_direction() {
        assert!(matches_direction(
            &TransitionDirection::SafeToUnsafe,
            MonitorState::Safe,
            MonitorState::Unsafe
        ));
        assert!(!matches_direction(
            &TransitionDirection::SafeToUnsafe,
            MonitorState::Unsafe,
            MonitorState::Safe
        ));
    }

    #[test]
    fn unsafe_to_safe_matches_correct_direction() {
        assert!(matches_direction(
            &TransitionDirection::UnsafeToSafe,
            MonitorState::Unsafe,
            MonitorState::Safe
        ));
        assert!(!matches_direction(
            &TransitionDirection::UnsafeToSafe,
            MonitorState::Safe,
            MonitorState::Unsafe
        ));
    }

    #[test]
    fn both_matches_either_direction() {
        assert!(matches_direction(
            &TransitionDirection::Both,
            MonitorState::Safe,
            MonitorState::Unsafe
        ));
        assert!(matches_direction(
            &TransitionDirection::Both,
            MonitorState::Unsafe,
            MonitorState::Safe
        ));
    }

    #[test]
    fn unknown_transitions_dont_match() {
        assert!(!matches_direction(
            &TransitionDirection::SafeToUnsafe,
            MonitorState::Unknown,
            MonitorState::Safe
        ));
        assert!(!matches_direction(
            &TransitionDirection::UnsafeToSafe,
            MonitorState::Unknown,
            MonitorState::Unsafe
        ));
        assert!(!matches_direction(
            &TransitionDirection::Both,
            MonitorState::Unknown,
            MonitorState::Safe
        ));
    }

    #[test]
    fn same_state_doesnt_match() {
        assert!(!matches_direction(
            &TransitionDirection::Both,
            MonitorState::Safe,
            MonitorState::Safe
        ));
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
