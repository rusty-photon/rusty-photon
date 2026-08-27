//! The Guide Focus Watch (rp.md § Guide Focus Watch): a background
//! poll over the guider service's per-frame star metrics that turns a
//! degrading HFD trend into **events, never actions**.
//!
//! It emits `guide_focus_degraded` when the trailing median exceeds
//! `baseline × degrade_ratio`, `guide_focus_escalation` when the
//! episode is still degraded `escalation_deadline` later. The
//! orchestrator wires those events to `refocus_train`; rp never moves
//! a focuser on its own initiative.
//!
//! The trend logic lives in [`WatchCore`], a pure state machine over
//! `(guiding, valid HFDs, now)` observations so the thresholds are
//! unit-testable without a runtime; the spawned task owns the polling
//! and the baseline re-arm on guiding-train focus events.

use std::sync::Arc;
use std::time::Instant;

use tracing::debug;

use crate::config::FocusWatchConfig;
use crate::events::EventBus;

/// What one observation asks the surrounding task to emit.
#[derive(Debug, Clone, PartialEq)]
pub enum WatchEvent {
    Degraded { baseline_hfd: f64, current_hfd: f64 },
    Escalation { baseline_hfd: f64, current_hfd: f64 },
}

/// One degradation episode: opened when the degraded event fires,
/// closed silently on recovery.
#[derive(Debug, Clone, Copy)]
struct Episode {
    fired_at: Instant,
    escalated: bool,
}

/// The pure trend state machine. Feed it one observation per poll.
pub struct WatchCore {
    config: FocusWatchConfig,
    baseline: Option<f64>,
    episode: Option<Episode>,
    cooldown_until: Option<Instant>,
}

impl WatchCore {
    #[must_use]
    pub const fn new(config: FocusWatchConfig) -> Self {
        Self {
            config,
            baseline: None,
            episode: None,
            cooldown_until: None,
        }
    }

    /// Drop the baseline, any open episode, and the cooldown — a
    /// fresh focus (or a guiding restart) is a fresh reference, and a
    /// trend degrading against it deserves a fresh event rather than
    /// suppression left over from the previous episode.
    pub const fn rearm(&mut self) {
        self.baseline = None;
        self.episode = None;
        self.cooldown_until = None;
    }

    /// One observation: `valid_hfds` are the metrics ring's valid
    /// HFDs (no star-lost, no null), oldest first. Returns the events
    /// to emit.
    pub fn observe(&mut self, guiding: bool, valid_hfds: &[f64], now: Instant) -> Vec<WatchEvent> {
        if !guiding {
            // Between guide sessions the ring is stale; the next
            // active poll re-derives the baseline from fresh frames.
            self.rearm();
            return Vec::new();
        }
        let window = self.config.window.value();
        if self.baseline.is_none() && valid_hfds.len() >= window {
            // In bounds: the guard above proved `len >= window`.
            self.baseline = Some(median(valid_hfds.get(..window).unwrap_or_default()));
        }
        let Some(baseline) = self.baseline else {
            return Vec::new();
        };
        if valid_hfds.len() < window {
            return Vec::new();
        }
        let current = median(
            valid_hfds
                .get(valid_hfds.len().saturating_sub(window)..)
                .unwrap_or_default(),
        );
        let degraded = current > baseline * self.config.degrade_ratio.value();

        let mut events = Vec::new();
        match (&mut self.episode, degraded) {
            (None, true) => {
                let cooling_down = self.cooldown_until.is_some_and(|until| now < until);
                if !cooling_down {
                    events.push(WatchEvent::Degraded {
                        baseline_hfd: baseline,
                        current_hfd: current,
                    });
                    self.episode = Some(Episode {
                        fired_at: now,
                        escalated: false,
                    });
                    // Unreachable overflow degrades to an already-elapsed
                    // cooldown rather than a panic.
                    self.cooldown_until =
                        Some(now.checked_add(self.config.cooldown).unwrap_or(now));
                }
            }
            (Some(episode), true) => {
                // An overflowing escalation deadline is unrepresentably
                // far away: never reached, matching its meaning.
                if !episode.escalated
                    && episode
                        .fired_at
                        .checked_add(self.config.escalation_deadline)
                        .is_some_and(|d| now >= d)
                {
                    events.push(WatchEvent::Escalation {
                        baseline_hfd: baseline,
                        current_hfd: current,
                    });
                    episode.escalated = true;
                }
            }
            (Some(_), false) => {
                // Recovery ends the episode silently.
                self.episode = None;
            }
            (None, false) => {}
        }
        events
    }
}

/// Median of a non-empty slice (upper median for even lengths, like
/// the metric sweep's per-position sample).
fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        // Callers guarantee non-empty (window >= 1); NaN poisons the
        // comparison conspicuously instead of panicking.
        return f64::NAN;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    // `len / 2` is in bounds for the non-empty slice checked above.
    sorted.get(sorted.len() / 2).copied().unwrap_or(f64::NAN)
}

/// Spawn the watch task. `guiding_train_id` / `guiding_focusers`
/// scope the baseline re-arm to focus events that involved the
/// guiding train.
pub fn spawn(
    client: Arc<dyn rp_guider::GuiderClient>,
    event_bus: Arc<EventBus>,
    config: FocusWatchConfig,
    guiding_train_id: Option<String>,
    guiding_focusers: Vec<String>,
) -> tokio::task::JoinHandle<()> {
    let mut core = WatchCore::new(config);
    let mut bus_rx = event_bus.subscribe();
    tokio::spawn(async move {
        debug!("guide focus watch started");
        loop {
            tokio::select! {
                () = tokio::time::sleep(config.poll_interval) => {
                    let (guiding, valid) = match client.guiding_metrics().await {
                        Ok(metrics) => {
                            let valid: Vec<f64> = metrics
                                .frames
                                .iter()
                                .filter(|f| !f.star_lost)
                                .filter_map(|f| f.hfd)
                                .collect();
                            (metrics.guiding, valid)
                        }
                        Err(e) => {
                            debug!(error = %e, "guide focus watch: metrics unavailable");
                            continue;
                        }
                    };
                    for event in core.observe(guiding, &valid, Instant::now()) {
                        // Both payloads name the guiding train (null
                        // without one) so an orchestrator trigger can
                        // address the responding sweep rig-agnostically.
                        match event {
                            WatchEvent::Degraded { baseline_hfd, current_hfd } => {
                                debug!(baseline_hfd, current_hfd, "guide focus degraded");
                                event_bus.emit(
                                    "guide_focus_degraded",
                                    serde_json::json!({
                                        "train_id": guiding_train_id,
                                        "baseline_hfd": baseline_hfd,
                                        "current_hfd": current_hfd,
                                        "window": config.window.value(),
                                    }),
                                );
                            }
                            WatchEvent::Escalation { baseline_hfd, current_hfd } => {
                                debug!(baseline_hfd, current_hfd, "guide focus escalation");
                                event_bus.emit(
                                    "guide_focus_escalation",
                                    serde_json::json!({
                                        "train_id": guiding_train_id,
                                        "baseline_hfd": baseline_hfd,
                                        "current_hfd": current_hfd,
                                    }),
                                );
                            }
                        }
                    }
                }
                envelope = bus_rx.recv() => {
                    match envelope {
                        Ok(envelope) => {
                            if rearms_baseline(
                                &envelope.event,
                                &envelope.payload,
                                guiding_train_id.as_deref(),
                                &guiding_focusers,
                            ) {
                                debug!(event = %envelope.event, "guide focus watch re-armed");
                                core.rearm();
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            debug!("guide focus watch lagged {n} events");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
    })
}

/// Whether an rp event means the guiding train's focus changed — a
/// metric `focus_complete` in the guiding train, a capture
/// `focus_complete` on a guiding-member focuser, or a
/// `refocus_complete` with a step that did either (a shared
/// guiding-member focuser swept in an imaging train changes the
/// guide focus just the same).
fn rearms_baseline(
    event: &str,
    payload: &serde_json::Value,
    guiding_train_id: Option<&str>,
    guiding_focusers: &[String],
) -> bool {
    let step_touches_guiding = |step: &serde_json::Value| {
        let train_matches =
            guiding_train_id.is_some_and(|id| step["train_id"].as_str() == Some(id));
        let focuser_matches = step["focuser_id"]
            .as_str()
            .is_some_and(|f| guiding_focusers.iter().any(|g| g == f));
        train_matches || focuser_matches
    };
    match event {
        "focus_complete" => step_touches_guiding(payload),
        "refocus_complete" => payload["steps"]
            .as_array()
            .is_some_and(|steps| steps.iter().any(step_touches_guiding)),
        _ => false,
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::time::Duration;

    use super::*;

    fn config(
        window: i64,
        ratio: f64,
        cooldown: Duration,
        escalation: Duration,
    ) -> FocusWatchConfig {
        serde_json::from_value(serde_json::json!({
            "window": window,
            "degrade_ratio": ratio,
            "cooldown": format!("{}ms", cooldown.as_millis()),
            "escalation_deadline": format!("{}ms", escalation.as_millis()),
        }))
        .unwrap()
    }

    #[test]
    fn a_degrading_trend_fires_once_and_escalates_after_the_deadline() {
        let mut core = WatchCore::new(config(
            3,
            1.25,
            Duration::from_mins(10),
            Duration::from_secs(10),
        ));
        let t0 = Instant::now();

        // Baseline forms from the first window; stable trend is quiet.
        assert_eq!(
            core.observe(true, &[2.0, 2.0, 2.0], t0),
            Vec::<WatchEvent>::new()
        );
        assert_eq!(
            core.observe(true, &[2.0, 2.0, 2.0, 2.1], t0),
            Vec::<WatchEvent>::new()
        );

        // Trailing median 3.0 > 2.0 × 1.25 → one degraded event.
        let hfds = [2.0, 2.0, 2.0, 3.0, 3.0, 3.0];
        let events = core.observe(true, &hfds, t0 + Duration::from_secs(1));
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            WatchEvent::Degraded { baseline_hfd, current_hfd }
                if baseline_hfd == 2.0 && current_hfd == 3.0
        ));

        // Still degraded before the deadline: silent (episode open,
        // cooldown holds).
        assert_eq!(
            core.observe(true, &hfds, t0 + Duration::from_secs(5)),
            Vec::<WatchEvent>::new()
        );

        // Past the escalation deadline: exactly one escalation.
        let events = core.observe(true, &hfds, t0 + Duration::from_secs(12));
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], WatchEvent::Escalation { .. }));
        assert_eq!(
            core.observe(true, &hfds, t0 + Duration::from_secs(20)),
            Vec::<WatchEvent>::new()
        );
    }

    #[test]
    fn recovery_ends_the_episode_silently_and_cooldown_gates_the_next_fire() {
        let cooldown = Duration::from_mins(1);
        let mut core = WatchCore::new(config(3, 1.25, cooldown, Duration::from_mins(10)));
        let t0 = Instant::now();

        assert_eq!(
            core.observe(true, &[2.0, 2.0, 2.0], t0),
            Vec::<WatchEvent>::new()
        );
        let degraded = [2.0, 2.0, 2.0, 3.0, 3.0, 3.0];
        assert_eq!(core.observe(true, &degraded, t0).len(), 1);

        // Recovery: silent.
        let recovered = [2.0, 2.0, 2.0, 3.0, 3.0, 3.0, 2.0, 2.0, 2.0];
        assert_eq!(
            core.observe(true, &recovered, t0 + Duration::from_secs(5)),
            Vec::<WatchEvent>::new()
        );

        // Degrading again inside the cooldown: still silent.
        assert_eq!(
            core.observe(true, &degraded, t0 + Duration::from_secs(10)),
            Vec::<WatchEvent>::new()
        );

        // Past the cooldown: fires again.
        assert_eq!(
            core.observe(true, &degraded, t0 + cooldown + Duration::from_secs(1))
                .len(),
            1
        );
    }

    #[test]
    fn rearm_clears_the_cooldown_so_a_fresh_baseline_can_fire() {
        let mut core = WatchCore::new(config(
            3,
            1.25,
            Duration::from_hours(1),
            Duration::from_hours(1),
        ));
        let t0 = Instant::now();
        assert_eq!(
            core.observe(true, &[2.0, 2.0, 2.0], t0),
            Vec::<WatchEvent>::new()
        );
        let degraded = [2.0, 2.0, 2.0, 3.0, 3.0, 3.0];
        assert_eq!(core.observe(true, &degraded, t0).len(), 1);

        // A refocus re-arms; the new baseline degrading again fires
        // immediately — the old episode's cooldown must not linger.
        core.rearm();
        assert_eq!(
            core.observe(true, &[2.0, 2.0, 2.0], t0 + Duration::from_secs(1)),
            Vec::<WatchEvent>::new()
        );
        assert_eq!(
            core.observe(true, &degraded, t0 + Duration::from_secs(2))
                .len(),
            1
        );
    }

    #[test]
    fn not_guiding_resets_the_baseline() {
        let mut core = WatchCore::new(config(
            3,
            1.25,
            Duration::from_mins(10),
            Duration::from_mins(10),
        ));
        let t0 = Instant::now();
        assert_eq!(
            core.observe(true, &[2.0, 2.0, 2.0], t0),
            Vec::<WatchEvent>::new()
        );
        assert_eq!(core.observe(false, &[], t0), Vec::<WatchEvent>::new());
        // Fresh baseline derives from the new frames — 3.0 is now
        // normal, not degraded.
        assert_eq!(
            core.observe(true, &[3.0, 3.0, 3.0], t0),
            Vec::<WatchEvent>::new()
        );
        assert_eq!(
            core.observe(true, &[3.0, 3.0, 3.0, 3.2, 3.2, 3.2], t0),
            Vec::<WatchEvent>::new()
        );
    }

    #[test]
    fn rearm_scoping_matches_guiding_train_focus_events_only() {
        let focusers = vec!["guide-focuser".to_string()];
        for (event, payload, expected) in [
            (
                "focus_complete",
                serde_json::json!({ "train_id": "guide" }),
                true,
            ),
            (
                "focus_complete",
                serde_json::json!({ "focuser_id": "guide-focuser" }),
                true,
            ),
            (
                "focus_complete",
                serde_json::json!({ "focuser_id": "main-focuser" }),
                false,
            ),
            (
                "refocus_complete",
                serde_json::json!({ "steps": [{ "train_id": "guide" }] }),
                true,
            ),
            (
                "refocus_complete",
                serde_json::json!({ "steps": [{ "train_id": "main" }] }),
                false,
            ),
            (
                "refocus_complete",
                serde_json::json!({ "steps": [
                    { "train_id": "main", "focuser_id": "guide-focuser" }
                ] }),
                true,
            ),
            ("exposure_complete", serde_json::json!({}), false),
        ] {
            assert_eq!(
                rearms_baseline(event, &payload, Some("guide"), &focusers),
                expected,
                "event {event} payload {payload}"
            );
        }
    }
}
