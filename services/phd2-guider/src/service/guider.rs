//! Guiding operations behind the HTTP API: settle-blocking guide and
//! dither, the confirmed stop, and the rolling RMS window.
//!
//! Behavior contract: `docs/services/phd2-guider.md` § "HTTP Service
//! Mode". The mutating operations serialize behind a single-flight
//! mutex (overlapping requests queue, not error); the read-only
//! snapshot paths bypass it.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::client::Phd2Client;
use crate::config::SettleParams;
use crate::events::{AppState, Phd2Event};

use super::error::ServiceError;

/// Rolling RMS window size, in guide steps.
const RMS_WINDOW: usize = 50;

/// Wall-clock grace added to the request's settle timeout for the
/// backstop. PHD2 enforces the settle timeout itself and reports
/// expiry via `SettleDone{status≠0}`; the backstop only catches a
/// wedged or disconnected PHD2.
const SETTLE_GRACE: Duration = Duration::from_secs(10);

/// Poll cadence for the stop confirmation loop.
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Capacity of the settle-verdict channel. The pump publishes one
/// verdict per `SettleDone`, and only while a guide or dither holds a
/// subscription — they serialize behind `op_lock`, and a send with no
/// receiver buffers nothing — so it never fills in practice. The raw
/// PHD2 stream carries no such bound: a burst of guide steps there
/// could push a `SettleDone` out of a descheduled waiter's ring and
/// strand the request on the backstop.
const SETTLE_CHANNEL_CAPACITY: usize = 16;

/// One retained guide step: `(RADistanceRaw, DECDistanceRaw)`.
type StepSample = (Option<f64>, Option<f64>);

/// PHD2's verdict on a settle, republished by the event pump once it
/// has folded every event that arrived before it.
#[derive(Debug, Clone)]
struct SettleOutcome {
    status: i32,
    error: Option<String>,
}

#[derive(Debug, Default)]
struct StatsWindow {
    steps: VecDeque<StepSample>,
    last_snr: Option<f64>,
    last_star_mass: Option<f64>,
}

impl StatsWindow {
    fn push(&mut self, ra: Option<f64>, dec: Option<f64>, snr: Option<f64>, mass: Option<f64>) {
        if self.steps.len() == RMS_WINDOW {
            self.steps.pop_front();
        }
        self.steps.push_back((ra, dec));
        // Mirror the most recent guide step exactly — a step without a
        // measurement clears the field rather than leaving stale
        // telemetry in the snapshot.
        self.last_snr = snr;
        self.last_star_mass = mass;
    }

    fn snapshot(&self) -> StatsSnapshot {
        // Single pass over the (bounded) window; no per-call allocation.
        let (mut ra_sum_sq, mut ra_n, mut dec_sum_sq, mut dec_n) = (0.0f64, 0u32, 0.0f64, 0u32);
        for (ra, dec) in &self.steps {
            if let Some(v) = ra {
                ra_sum_sq += v * v;
                ra_n = ra_n.saturating_add(1);
            }
            if let Some(v) = dec {
                dec_sum_sq += v * v;
                dec_n = dec_n.saturating_add(1);
            }
        }
        let rms = |sum_sq: f64, n: u32| (n > 0).then(|| (sum_sq / f64::from(n)).sqrt());
        let rms_ra_px = rms(ra_sum_sq, ra_n);
        let rms_dec_px = rms(dec_sum_sq, dec_n);
        let total_rms_px = match (rms_ra_px, rms_dec_px) {
            (Some(ra), Some(dec)) => Some(ra.hypot(dec)),
            _ => None,
        };
        StatsSnapshot {
            rms_ra_px,
            rms_dec_px,
            total_rms_px,
            snr: self.last_snr,
            star_mass: self.last_star_mass,
            sample_count: self.steps.len(),
        }
    }
}

/// Point-in-time view of the rolling window, embedded in guide,
/// dither, and stats responses.
#[derive(Debug, Clone, Copy)]
pub struct StatsSnapshot {
    pub rms_ra_px: Option<f64>,
    pub rms_dec_px: Option<f64>,
    pub total_rms_px: Option<f64>,
    pub snr: Option<f64>,
    pub star_mass: Option<f64>,
    pub sample_count: usize,
}

/// Stats endpoint payload: the window snapshot plus PHD2's current
/// application state.
#[derive(Debug)]
pub struct GuidingStats {
    pub app_state: AppState,
    pub snapshot: StatsSnapshot,
}

/// Per-frame metrics ring size, in events (`GuideStep` + `StarLost`).
const METRICS_WINDOW: usize = 50;

/// One entry of the per-frame metrics ring behind
/// `GET /api/v1/guiding/metrics`: a `GuideStep`'s star metrics, or a
/// `StarLost` marker (`star_lost: true`, no HFD).
#[derive(Debug, Clone, serde::Serialize)]
pub struct FrameMetrics {
    pub frame: u64,
    pub hfd: Option<f64>,
    pub snr: Option<f64>,
    pub star_mass: Option<f64>,
    pub star_lost: bool,
}

/// Metrics endpoint payload: `guiding` derived from a fresh app-state
/// RPC (as in `stats`) plus the ring, oldest first.
#[derive(Debug)]
pub struct GuidingMetrics {
    pub guiding: bool,
    pub frames: Vec<FrameMetrics>,
}

pub struct GuiderOps {
    client: Arc<Phd2Client>,
    /// Single-flight lock for mutating operations.
    op_lock: tokio::sync::Mutex<()>,
    stats: std::sync::Mutex<StatsWindow>,
    /// Per-frame metrics ring (newest at the back), cleared together
    /// with the RMS window on `guiding/start`.
    metrics: std::sync::Mutex<std::collections::VecDeque<FrameMetrics>>,
    /// Settle verdicts, published by the event pump *after* it folds
    /// the events that preceded them. Waiting here rather than on the
    /// raw PHD2 stream is what makes the snapshot a guide response
    /// returns include every step PHD2 sent before it settled.
    settle_events: broadcast::Sender<SettleOutcome>,
    default_settle: SettleParams,
    stop_timeout: Duration,
}

impl GuiderOps {
    pub fn new(
        client: Arc<Phd2Client>,
        default_settle: SettleParams,
        stop_timeout: Duration,
    ) -> Self {
        Self {
            client,
            op_lock: tokio::sync::Mutex::new(()),
            stats: std::sync::Mutex::new(StatsWindow::default()),
            metrics: std::sync::Mutex::new(std::collections::VecDeque::with_capacity(
                METRICS_WINDOW,
            )),
            settle_events: broadcast::channel(SETTLE_CHANNEL_CAPACITY).0,
            default_settle,
            stop_timeout,
        }
    }

    fn push_metrics(&self, entry: FrameMetrics) {
        let mut ring = self
            .metrics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if ring.len() == METRICS_WINDOW {
            ring.pop_front();
        }
        ring.push_back(entry);
    }

    /// Merge a partial per-request settle override onto the config
    /// defaults, field by field.
    pub fn resolve_settle(
        &self,
        pixels: Option<f64>,
        time: Option<Duration>,
        timeout: Option<Duration>,
    ) -> SettleParams {
        SettleParams {
            pixels: pixels.unwrap_or(self.default_settle.pixels),
            time: time.unwrap_or(self.default_settle.time),
            timeout: timeout.unwrap_or(self.default_settle.timeout),
        }
    }

    /// Fold one PHD2 event into the rolling window and the metrics
    /// ring, then republish a settle verdict for the waiters.
    ///
    /// The order within this function is the contract: a settle
    /// verdict becomes observable only once the events that preceded
    /// it are already in the window, so the snapshot a guide response
    /// carries can never under-report the steps PHD2 sent before it
    /// settled.
    fn ingest(&self, event: Phd2Event) {
        match event {
            Phd2Event::GuideStep(step) => {
                {
                    let mut window = self
                        .stats
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    window.push(
                        step.ra_distance_raw,
                        step.dec_distance_raw,
                        step.snr,
                        step.star_mass,
                    );
                }
                self.push_metrics(FrameMetrics {
                    frame: step.frame,
                    hfd: step.hfd,
                    snr: step.snr,
                    star_mass: step.star_mass,
                    star_lost: false,
                });
            }
            Phd2Event::StarLost {
                frame,
                star_mass,
                snr,
                ..
            } => {
                self.push_metrics(FrameMetrics {
                    frame,
                    hfd: None,
                    snr: Some(snr),
                    star_mass: Some(star_mass),
                    star_lost: true,
                });
            }
            Phd2Event::SettleDone { status, error } => {
                // No receiver means no guide or dither is waiting.
                let _ = self.settle_events.send(SettleOutcome { status, error });
            }
            _ => {}
        }
    }

    /// The single consumer of the client's event stream: it feeds the
    /// rolling window, the metrics ring, and the settle verdicts that
    /// guide and dither wait on. Runs until the event channel closes
    /// (client dropped).
    pub fn spawn_event_pump(self: &Arc<Self>) {
        let ops = Arc::clone(self);
        tokio::spawn(async move {
            let mut rx = ops.client.subscribe();
            loop {
                match rx.recv().await {
                    Ok(event) => ops.ingest(event),
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        debug!("guide event pump lagged, skipped {n} events");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    /// Keep trying to establish the initial PHD2 connection. Once
    /// established, the client's own auto-reconnect owns recovery.
    pub fn spawn_connect_retry(self: &Arc<Self>, interval: Duration) {
        let ops = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                match ops.client.connect().await {
                    Ok(()) => {
                        info!("connected to PHD2");
                        break;
                    }
                    Err(e) => {
                        debug!("PHD2 not reachable yet: {e}; retrying in {interval:?}");
                        tokio::time::sleep(interval).await;
                    }
                }
            }
        });
    }

    pub async fn is_connected(&self) -> bool {
        self.client.is_connected().await
    }

    /// The `host:port` of the PHD2 this service dials — for the health
    /// endpoint's degraded message.
    pub fn phd2_addr(&self) -> String {
        self.client.phd2_addr()
    }

    /// Start guiding and block until PHD2 reports the star settled.
    ///
    /// # Errors
    ///
    /// Returns `phd2_unreachable` when PHD2 cannot be reached or its event
    /// stream closes, `guide_failed` when PHD2 rejects the guide RPC or
    /// reports a failed settle, and `settle_timeout` when no `SettleDone`
    /// arrives within the settle timeout plus its grace backstop.
    pub async fn start_guiding(
        &self,
        settle: SettleParams,
        recalibrate: bool,
    ) -> Result<StatsSnapshot, ServiceError> {
        let _op = self.op_lock.lock().await;
        // Subscribe before issuing the RPC so a fast SettleDone
        // cannot be missed.
        let rx = self.settle_events.subscribe();
        {
            let mut window = self
                .stats
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *window = StatsWindow::default();
        }
        {
            let mut ring = self
                .metrics
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            ring.clear();
        }
        debug!(
            pixels = settle.pixels,
            time = ?settle.time,
            timeout = ?settle.timeout,
            recalibrate,
            "starting guiding"
        );
        self.client
            .start_guiding(&settle, recalibrate, None)
            .await
            .map_err(ServiceError::from)?;
        self.wait_for_settle(rx, &settle).await?;
        Ok(self.stats_snapshot())
    }

    /// Dither and block until PHD2 reports the star settled. Rejected
    /// with `not_guiding` unless PHD2's application state is Guiding.
    ///
    /// # Errors
    ///
    /// Returns `not_guiding` when PHD2 is not currently guiding; otherwise
    /// the same classes as guiding: `phd2_unreachable`, `guide_failed`, and
    /// `settle_timeout` when no `SettleDone` arrives within the backstop.
    pub async fn dither(
        &self,
        amount_px: f64,
        ra_only: bool,
        settle: SettleParams,
    ) -> Result<StatsSnapshot, ServiceError> {
        let _op = self.op_lock.lock().await;
        let state = self
            .client
            .get_app_state()
            .await
            .map_err(ServiceError::from)?;
        if state != AppState::Guiding {
            return Err(ServiceError::NotGuiding(state.to_string()));
        }
        let rx = self.settle_events.subscribe();
        debug!(amount_px, ra_only, "dithering");
        self.client
            .dither(amount_px, ra_only, &settle)
            .await
            .map_err(ServiceError::from)?;
        self.wait_for_settle(rx, &settle).await?;
        Ok(self.stats_snapshot())
    }

    /// Stop capture and block until PHD2 confirms the Stopped state.
    /// Idempotent: an already-stopped PHD2 succeeds immediately.
    ///
    /// # Errors
    ///
    /// Returns the mapped client failure when the state poll or the stop
    /// RPC fails, and `stop_timeout` when PHD2 does not reach Stopped
    /// within the configured stop timeout.
    pub async fn stop(&self) -> Result<(), ServiceError> {
        let _op = self.op_lock.lock().await;
        let state = self
            .client
            .get_app_state()
            .await
            .map_err(ServiceError::from)?;
        if state == AppState::Stopped {
            debug!("stop requested while already stopped");
            return Ok(());
        }
        self.client
            .stop_capture()
            .await
            .map_err(ServiceError::from)?;
        // A `None` deadline means `now + stop_timeout` is unrepresentable —
        // effectively infinite, so the poll never times out (the same
        // far-future reading tokio's own timers give an overflowing add).
        let deadline = tokio::time::Instant::now().checked_add(self.stop_timeout);
        loop {
            tokio::time::sleep(STOP_POLL_INTERVAL).await;
            let state = self
                .client
                .get_app_state()
                .await
                .map_err(ServiceError::from)?;
            if state == AppState::Stopped {
                return Ok(());
            }
            if deadline.is_some_and(|d| tokio::time::Instant::now() >= d) {
                warn!("PHD2 did not reach Stopped within {:?}", self.stop_timeout);
                return Err(ServiceError::StopTimeout(
                    humantime::format_duration(self.stop_timeout).to_string(),
                ));
            }
        }
    }

    /// Pause guiding (PHD2 `set_paused`); with `full`, pause looping
    /// entirely rather than only suppressing guide corrections.
    ///
    /// # Errors
    ///
    /// Returns the mapped client failure: `phd2_unreachable` when PHD2
    /// cannot be reached, `guide_failed` when PHD2 rejects the RPC.
    pub async fn pause(&self, full: bool) -> Result<(), ServiceError> {
        let _op = self.op_lock.lock().await;
        self.client.pause(full).await.map_err(ServiceError::from)
    }

    /// Resume guiding after a pause (PHD2 `set_paused` off).
    ///
    /// # Errors
    ///
    /// Returns the mapped client failure: `phd2_unreachable` when PHD2
    /// cannot be reached, `guide_failed` when PHD2 rejects the RPC.
    pub async fn resume(&self) -> Result<(), ServiceError> {
        let _op = self.op_lock.lock().await;
        self.client.resume().await.map_err(ServiceError::from)
    }

    /// PHD2's application state plus the rolling-window snapshot —
    /// read-only, bypasses the operation mutex.
    ///
    /// # Errors
    ///
    /// Returns the mapped client failure when the app-state poll fails
    /// (`phd2_unreachable` or `guide_failed`).
    pub async fn stats(&self) -> Result<GuidingStats, ServiceError> {
        let app_state = self
            .client
            .get_app_state()
            .await
            .map_err(ServiceError::from)?;
        Ok(GuidingStats {
            app_state,
            snapshot: self.stats_snapshot(),
        })
    }

    /// The per-frame metrics ring plus a fresh guiding flag —
    /// read-only, no mutating mutex (mirrors `stats`).
    ///
    /// # Errors
    ///
    /// Returns the mapped client failure when the app-state poll fails
    /// (`phd2_unreachable` or `guide_failed`).
    pub async fn metrics(&self) -> Result<GuidingMetrics, ServiceError> {
        let app_state = self
            .client
            .get_app_state()
            .await
            .map_err(ServiceError::from)?;
        let frames = {
            let ring = self
                .metrics
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            ring.iter().cloned().collect()
        };
        Ok(GuidingMetrics {
            guiding: app_state == AppState::Guiding,
            frames,
        })
    }

    /// PHD2's current equipment slots — read-only passthrough.
    ///
    /// # Errors
    ///
    /// Returns the mapped client failure: `phd2_unreachable` when PHD2
    /// cannot be reached, `guide_failed` when PHD2 rejects the RPC.
    pub async fn equipment(&self) -> Result<crate::types::Equipment, ServiceError> {
        self.client
            .get_current_equipment()
            .await
            .map_err(ServiceError::from)
    }

    /// Clear PHD2's stored calibration; PHD2 recalibrates on the next
    /// guide start.
    ///
    /// # Errors
    ///
    /// Returns the mapped client failure: `phd2_unreachable` when PHD2
    /// cannot be reached, `guide_failed` when PHD2 rejects the RPC.
    pub async fn clear_calibration(
        &self,
        which: crate::types::CalibrationTarget,
    ) -> Result<(), ServiceError> {
        let _op = self.op_lock.lock().await;
        self.client
            .clear_calibration(which)
            .await
            .map_err(ServiceError::from)
    }

    /// Auto-select a guide star on the current frame (PHD2
    /// `find_star`, full frame).
    ///
    /// # Errors
    ///
    /// Returns the mapped client failure: `phd2_unreachable` when PHD2
    /// cannot be reached, `guide_failed` when PHD2 rejects the RPC.
    pub async fn reselect_star(&self) -> Result<(), ServiceError> {
        let _op = self.op_lock.lock().await;
        self.client
            .find_star(None)
            .await
            .map_err(ServiceError::from)
    }

    fn stats_snapshot(&self) -> StatsSnapshot {
        self.stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot()
    }

    async fn wait_for_settle(
        &self,
        mut rx: broadcast::Receiver<SettleOutcome>,
        settle: &SettleParams,
    ) -> Result<(), ServiceError> {
        let backstop = settle.timeout.saturating_add(SETTLE_GRACE);
        // `sleep` is the total spelling of `timeout_at(now + backstop)`:
        // it performs that add internally via `checked_add` with a
        // far-future fallback.
        let mut backstop_expired = std::pin::pin!(tokio::time::sleep(backstop));
        loop {
            tokio::select! {
                () = &mut backstop_expired => {
                    warn!("no SettleDone within the {backstop:?} backstop");
                    return Err(ServiceError::SettleTimeout(
                        humantime::format_duration(backstop).to_string(),
                    ));
                }
                recv = rx.recv() => match recv {
                    Ok(SettleOutcome { status, error }) => {
                        if status == 0 {
                            return Ok(());
                        }
                        return Err(ServiceError::GuideFailed(
                            error.unwrap_or_else(|| format!("SettleDone status {status}")),
                        ));
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        debug!("settle wait lagged, skipped {n} verdicts");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        return Err(ServiceError::Phd2Unreachable(
                            "PHD2 event stream closed".to_string(),
                        ));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} !~ {b}");
    }

    #[test]
    fn the_rms_window_computes_per_axis_and_total_rms() {
        let mut window = StatsWindow::default();
        window.push(Some(0.3), Some(-0.4), Some(25.1), Some(5340.0));
        window.push(Some(-0.3), Some(0.4), Some(26.0), None);
        let snap = window.snapshot();
        approx(snap.rms_ra_px.unwrap(), 0.3);
        approx(snap.rms_dec_px.unwrap(), 0.4);
        approx(snap.total_rms_px.unwrap(), 0.5);
        assert_eq!(snap.sample_count, 2);
        approx(snap.snr.unwrap(), 26.0);
        // The latest step omitted StarMass: the snapshot mirrors the
        // most recent step exactly rather than holding a stale value.
        assert_eq!(snap.star_mass, None);
    }

    #[test]
    fn an_empty_window_reports_nulls_and_zero_samples() {
        let snap = StatsWindow::default().snapshot();
        assert_eq!(snap.rms_ra_px, None);
        assert_eq!(snap.rms_dec_px, None);
        assert_eq!(snap.total_rms_px, None);
        assert_eq!(snap.snr, None);
        assert_eq!(snap.sample_count, 0);
    }

    #[test]
    fn steps_missing_a_distance_are_skipped_for_that_axis_only() {
        let mut window = StatsWindow::default();
        window.push(Some(0.3), None, None, None);
        window.push(Some(-0.3), Some(0.4), None, None);
        let snap = window.snapshot();
        approx(snap.rms_ra_px.unwrap(), 0.3);
        approx(snap.rms_dec_px.unwrap(), 0.4);
        assert_eq!(snap.sample_count, 2);
    }

    #[test]
    fn the_window_is_capped_at_fifty_steps() {
        let mut window = StatsWindow::default();
        // 50 old steps at 1.0, then one new step at 0.0 evicting one.
        for _ in 0..RMS_WINDOW {
            window.push(Some(1.0), Some(1.0), None, None);
        }
        window.push(Some(0.0), Some(0.0), None, None);
        let snap = window.snapshot();
        assert_eq!(snap.sample_count, RMS_WINDOW);
        approx(snap.rms_ra_px.unwrap(), (49.0f64 / 50.0).sqrt());
    }

    /// The mock PHD2's wire shapes, so the ordering tests below feed
    /// the pump exactly what the service parses off the socket.
    const GUIDE_STEP_1: &str = r#"{"Event":"GuideStep","Frame":1,"Time":1.0,"Mount":"Mock Mount","dx":0.1,"dy":0.1,"RADistanceRaw":0.3,"DECDistanceRaw":-0.4,"SNR":25.1,"StarMass":5340.0,"HFD":2.3}"#;
    const GUIDE_STEP_2: &str = r#"{"Event":"GuideStep","Frame":2,"Time":2.0,"Mount":"Mock Mount","dx":0.1,"dy":0.1,"RADistanceRaw":-0.3,"DECDistanceRaw":0.4,"SNR":25.1,"StarMass":5340.0,"HFD":2.5}"#;
    const STAR_LOST_3: &str =
        r#"{"Event":"StarLost","Frame":3,"Time":3.0,"StarMass":900.0,"SNR":3.1,"Status":"Lost"}"#;

    fn event(json: &str) -> Phd2Event {
        serde_json::from_str(json).unwrap()
    }

    fn test_ops() -> Arc<GuiderOps> {
        let client = Arc::new(Phd2Client::new(crate::config::Phd2Config::default()));
        Arc::new(GuiderOps::new(
            client,
            SettleParams::default(),
            Duration::from_secs(10),
        ))
    }

    /// Drive a settle to completion the way the event pump does — one
    /// ordered burst, verdict last — and hand back what the waiter saw.
    async fn settle_after(
        ops: &Arc<GuiderOps>,
        burst: &[&str],
        verdict: &str,
    ) -> Result<StatsSnapshot, ServiceError> {
        let rx = ops.settle_events.subscribe();
        let waiter = tokio::spawn({
            let ops = Arc::clone(ops);
            async move {
                ops.wait_for_settle(rx, &SettleParams::default())
                    .await
                    .map(|()| ops.stats_snapshot())
            }
        });
        for json in burst {
            ops.ingest(event(json));
        }
        ops.ingest(event(verdict));
        waiter.await.unwrap()
    }

    #[tokio::test]
    async fn a_settle_verdict_lands_only_after_the_events_before_it_are_folded() {
        let ops = test_ops();
        let snapshot = settle_after(
            &ops,
            &[GUIDE_STEP_1, GUIDE_STEP_2, STAR_LOST_3],
            r#"{"Event":"SettleDone","Status":0}"#,
        )
        .await
        .unwrap();
        // Both steps and the star-lost frame are already in place when
        // the wait returns: the pump publishes the verdict last.
        assert_eq!(snapshot.sample_count, 2);
        approx(snapshot.rms_ra_px.unwrap(), 0.3);
        approx(snapshot.rms_dec_px.unwrap(), 0.4);
        assert_eq!(
            ops.metrics
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            3
        );
    }

    #[tokio::test]
    async fn a_failed_settle_surfaces_phd2s_error_text() {
        let ops = test_ops();
        let err = settle_after(
            &ops,
            &[GUIDE_STEP_1],
            r#"{"Event":"SettleDone","Status":1,"Error":"Mock star lost"}"#,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&err, ServiceError::GuideFailed(text) if text == "Mock star lost"),
            "unexpected error: {err:?}"
        );
    }

    #[tokio::test]
    async fn a_failed_settle_without_error_text_names_the_status() {
        let ops = test_ops();
        let err = settle_after(&ops, &[], r#"{"Event":"SettleDone","Status":2}"#)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ServiceError::GuideFailed(text) if text == "SettleDone status 2"),
            "unexpected error: {err:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_settle_verdict_that_never_arrives_trips_the_backstop() {
        let ops = test_ops();
        let rx = ops.settle_events.subscribe();
        let settle = SettleParams {
            timeout: Duration::from_secs(60),
            ..SettleParams::default()
        };
        let err = ops.wait_for_settle(rx, &settle).await.unwrap_err();
        assert!(
            matches!(err, ServiceError::SettleTimeout(_)),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn settle_overrides_merge_field_by_field_onto_the_defaults() {
        let client = Arc::new(Phd2Client::new(crate::config::Phd2Config::default()));
        let ops = GuiderOps::new(client, SettleParams::default(), Duration::from_secs(10));
        let merged = ops.resolve_settle(Some(2.0), None, Some(Duration::from_secs(30)));
        approx(merged.pixels, 2.0);
        assert_eq!(merged.time, Duration::from_secs(10));
        assert_eq!(merged.timeout, Duration::from_secs(30));
    }
}
