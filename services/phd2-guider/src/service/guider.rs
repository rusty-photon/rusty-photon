//! Guiding operations behind the HTTP API: settle-blocking guide and
//! dither, the confirmed stop, and the rolling RMS window.
//!
//! Behavior contract: `docs/services/phd2-guider.md` § "HTTP Service
//! Mode". The mutating operations serialize behind a single-flight
//! mutex (overlapping requests queue, not error); the read-only
//! snapshot paths bypass it.
//!
//! The same mutex guards the camera facade's **capture slot**
//! (`docs/services/phd2-guider.md` § "Exclusivity: the capture slot"):
//! a facade frame is claimed only while the mutex is held, every
//! mutation except stop checks the claim once it holds the mutex and
//! is refused with `camera_busy`, and the claim is released when PHD2
//! reports the frame ended or its deadline passes.
//!
//! The facade's **sweep lease** (`docs/services/phd2-guider.md` § "The
//! sweep lease") sits beside the slot: its acquire claims it as a capture
//! does, the same mutations are refused while it is held, and a stop ends
//! it.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::client::Phd2Client;
use crate::config::SettleParams;
use crate::events::{AppState, Phd2Event};

use super::capture::LOCKED_RPC_TIMEOUT;
use super::error::ServiceError;
use super::lease::{self, AcquireRequest, Granted, LeaseBook, LeaseEnd, LeaseError, Token};

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

/// Capacity of the frame-report channel. PHD2 sends one
/// `SingleFrameComplete` per frame, and frames run one at a time.
const FRAME_CHANNEL_CAPACITY: usize = 16;

/// PHD2's `SingleFrameComplete`, republished by the event pump for the
/// camera facade.
#[derive(Debug, Clone)]
pub struct FrameReport {
    pub success: bool,
    pub error: Option<String>,
    /// The saved file, when the capture asked PHD2 to save it.
    pub path: Option<String>,
}

/// A facade frame holding the capture slot.
#[derive(Debug, Clone)]
pub struct CaptureClaim {
    /// The file PHD2 saves the frame to.
    pub path: PathBuf,
    /// When the slot is released even without PHD2's event:
    /// `exposure + capture_grace` after the request, on the monotonic
    /// clock the frame's own wait runs on, so a wall-clock step cannot
    /// end a claim while PHD2 is still exposing.
    pub expires: tokio::time::Instant,
    /// The same moment in wall-clock time, for messages and the restart
    /// marker.
    pub deadline: SystemTime,
    /// A guiding stop or the facade's own abort sent `stop_capture` for
    /// this frame, so a failure PHD2 reports for it is that stop.
    pub stop_requested: bool,
    /// Taken from a restart marker: a frame a previous process started.
    pub recovered: bool,
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
    /// PHD2's single-frame reports, for the camera facade.
    frame_reports: broadcast::Sender<FrameReport>,
    /// The camera facade's capture slot. Set only while `op_lock` is
    /// held; cleared when the frame ends.
    capture: std::sync::Mutex<Option<CaptureClaim>>,
    /// Fires each time the capture slot is released.
    capture_released: tokio::sync::Notify,
    /// The camera facade's sweep lease and the tokens it was named under.
    /// A lease is granted only while `op_lock` is held.
    lease: std::sync::Mutex<LeaseBook>,
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
            frame_reports: broadcast::channel(FRAME_CHANNEL_CAPACITY).0,
            capture: std::sync::Mutex::new(None),
            capture_released: tokio::sync::Notify::new(),
            lease: std::sync::Mutex::new(LeaseBook::default()),
            default_settle,
            stop_timeout,
        }
    }

    /// The mutex the guiding mutations serialize behind. The camera
    /// facade takes it with `try_lock` to claim the capture slot.
    pub(crate) const fn op_lock(&self) -> &tokio::sync::Mutex<()> {
        &self.op_lock
    }

    fn capture_slot(&self) -> std::sync::MutexGuard<'_, Option<CaptureClaim>> {
        self.capture
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The frame holding the capture slot, if any. A claim whose deadline
    /// has passed holds nothing, so the documented bound holds even if
    /// the task that should release it never runs.
    #[must_use]
    pub fn capture_claim(&self) -> Option<CaptureClaim> {
        self.capture_slot()
            .clone()
            .filter(|claim| claim.expires > tokio::time::Instant::now())
    }

    /// Claim the capture slot for a facade frame. The caller holds the
    /// guard of [`Self::op_lock`], which is what makes the claim atomic
    /// with the mutations' check.
    pub(crate) fn claim_capture(&self, _op: &tokio::sync::MutexGuard<'_, ()>, claim: CaptureClaim) {
        debug!(path = %claim.path.display(), recovered = claim.recovered, "capture slot claimed");
        *self.capture_slot() = Some(claim);
    }

    /// Release the capture slot if it is still held for `path`.
    pub(crate) fn release_capture(&self, path: &Path) {
        let released = {
            let mut slot = self.capture_slot();
            if slot.as_ref().is_some_and(|claim| claim.path == path) {
                *slot = None;
                true
            } else {
                false
            }
        };
        if released {
            debug!(path = %path.display(), "capture slot released");
            self.capture_released.notify_waiters();
        }
    }

    /// Wait until the capture slot is no longer held for `path`, or
    /// `limit` passes. Returns whether it was released.
    pub(crate) async fn wait_capture_released(&self, path: &Path, limit: Duration) -> bool {
        let deadline = tokio::time::Instant::now().checked_add(limit);
        loop {
            let released = self.capture_released.notified();
            let mut released = std::pin::pin!(released);
            // Register before checking, so a release between the check
            // and the wait is not missed.
            released.as_mut().enable();
            if self
                .capture_slot()
                .as_ref()
                .is_none_or(|claim| claim.path != path)
            {
                return true;
            }
            let Some(deadline) = deadline else {
                released.await;
                continue;
            };
            if tokio::time::timeout_at(deadline, released).await.is_err() {
                return false;
            }
        }
    }

    /// Mark the frame holding the slot as stopped by this service, so
    /// the failure PHD2 reports for it reads as the stop it was.
    pub(crate) fn mark_capture_stopped(&self) {
        if let Some(claim) = self.capture_slot().as_mut() {
            claim.stop_requested = true;
        }
    }

    /// PHD2's single-frame reports, for the camera facade.
    pub fn subscribe_frames(&self) -> broadcast::Receiver<FrameReport> {
        self.frame_reports.subscribe()
    }

    fn lease_book(&self) -> std::sync::MutexGuard<'_, LeaseBook> {
        self.lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Grant the camera facade's sweep lease: claimed under the guiding
    /// lock with `try_lock`, as a capture is, only with no frame in
    /// flight, no lease held, a token never seen, and PHD2 Stopped. The
    /// caller has checked the camera is connected, and holds its
    /// connect/disconnect lock.
    ///
    /// # Errors
    ///
    /// Returns [`LeaseError::NotConnected`] when PHD2 cannot be reached,
    /// and [`LeaseError::Refused`] naming any other reason.
    pub(crate) async fn acquire_lease(
        &self,
        request: &AcquireRequest,
    ) -> Result<Granted, LeaseError> {
        let Ok(_op) = self.op_lock.try_lock() else {
            return Err(LeaseError::Refused(
                "the guider is busy: a guiding request, or the end of an earlier frame".to_string(),
            ));
        };
        if let Some(claim) = self.capture_claim() {
            return Err(LeaseError::Refused(format!(
                "a frame is still exposing in PHD2 ({}), until {} at the latest; a sweep lease is \
                 granted only while no frame is",
                claim.path.display(),
                humantime::format_rfc3339_seconds(claim.deadline)
            )));
        }
        self.lease_book()
            .check_grant(&request.token, tokio::time::Instant::now())
            .map_err(LeaseError::Refused)?;
        let state = tokio::time::timeout(LOCKED_RPC_TIMEOUT, self.client.get_app_state())
            .await
            .map_err(|_| LeaseError::NotConnected("PHD2 did not answer get_app_state".to_string()))?
            .map_err(|e| match e {
                crate::error::Phd2Error::RpcError { message, .. } => {
                    LeaseError::Refused(format!("PHD2 refused get_app_state: {message}"))
                }
                other => LeaseError::NotConnected(format!("PHD2 unreachable: {other}")),
            })?;
        if state != AppState::Stopped {
            return Err(LeaseError::Refused(format!(
                "PHD2 is {state}; a sweep lease is granted only while PHD2 is Stopped"
            )));
        }
        // Checked again: a release recorded the token while PHD2 answered.
        let granted = self
            .lease_book()
            .grant(
                request.token.clone(),
                request.ttl_s,
                tokio::time::Instant::now(),
            )
            .map_err(LeaseError::Refused)?;
        debug!(token = %request.token, ttl_s = request.ttl_s, "sweep lease granted");
        Ok(granted)
    }

    /// Restart the held lease's TTL. Takes no guiding lock.
    ///
    /// # Errors
    ///
    /// Returns why `token` holds no lease.
    pub(crate) fn renew_lease(&self, token: &Token) -> Result<Granted, String> {
        let granted = self
            .lease_book()
            .renew(token, tokio::time::Instant::now())?;
        debug!(%token, "sweep lease renewed");
        Ok(granted)
    }

    /// End the lease `token` holds. Returns whether it held one. Takes no
    /// guiding lock.
    pub(crate) fn release_lease(&self, token: &Token) -> bool {
        let released = self
            .lease_book()
            .release(token, tokio::time::Instant::now());
        debug!(%token, released, "sweep lease release");
        released
    }

    /// End the held lease, if any: a stop, a disconnect, shutdown.
    pub(crate) fn end_lease(&self, why: LeaseEnd) {
        self.lease_book().end(why, tokio::time::Instant::now());
    }

    /// Refuse a guiding mutation while a facade frame holds the capture
    /// slot or a sweep lease is held. Called with `op_lock` held.
    fn refuse_while_camera_held(&self) -> Result<(), ServiceError> {
        let Some(claim) = self.capture_claim() else {
            let now = tokio::time::Instant::now();
            let Some(until) = self.lease_book().held_until(now) else {
                return Ok(());
            };
            let message = format!(
                "the camera facade's sweep lease is held; guiding operations are refused until it \
                 is released or ends, by {} unless it is renewed",
                lease::when(until, now)
            );
            debug!("{message}");
            return Err(ServiceError::CameraBusy(message));
        };
        let until = humantime::format_rfc3339_seconds(claim.deadline);
        let message = if claim.recovered {
            format!(
                "a camera-facade frame started before this service restarted may still be \
                 exposing; guiding operations are refused until it ends, by {until} at the latest"
            )
        } else {
            format!(
                "the camera facade is capturing a frame; guiding operations are refused until it \
                 ends, by {until} at the latest"
            )
        };
        debug!("{message}");
        Err(ServiceError::CameraBusy(message))
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
    pub(crate) fn ingest(&self, event: Phd2Event) {
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
            Phd2Event::SingleFrameComplete {
                success,
                error,
                path,
            } => {
                // No receiver means the camera facade is off.
                let _ = self.frame_reports.send(FrameReport {
                    success,
                    error,
                    path,
                });
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
        self.refuse_while_camera_held()?;
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
        self.refuse_while_camera_held()?;
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
    /// Never refused for a camera-facade frame: with one in flight, the
    /// stop is sent whatever PHD2's state reads, which ends the frame
    /// (`docs/services/phd2-guider.md` § "Exclusivity: the capture slot").
    /// It ends the facade's sweep lease, if one is held, before it asks
    /// PHD2 anything.
    ///
    /// # Errors
    ///
    /// Returns the mapped client failure when the state poll or the stop
    /// RPC fails, and `stop_timeout` when PHD2 does not reach Stopped
    /// within the configured stop timeout.
    pub async fn stop(&self) -> Result<(), ServiceError> {
        let _op = self.op_lock.lock().await;
        // The privileged stop breaks a sweep lease, whatever PHD2 answers.
        self.end_lease(LeaseEnd::Stopped);
        let state = self
            .client
            .get_app_state()
            .await
            .map_err(ServiceError::from)?;
        // A facade frame of this process is always stopped, whatever the
        // state reads. During every measured frame PHD2 answered Looping,
        // but its source reports Stopped for a capture-active guider in
        // some guider states, so Stopped does not prove no frame is running.
        let frame_in_flight = self.capture_claim().is_some_and(|claim| !claim.recovered);
        if state == AppState::Stopped && !frame_in_flight {
            debug!("stop requested while already stopped");
            return Ok(());
        }
        if frame_in_flight {
            debug!("stop ends the camera facade's frame in flight");
            self.mark_capture_stopped();
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
        self.refuse_while_camera_held()?;
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
        self.refuse_while_camera_held()?;
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
        self.refuse_while_camera_held()?;
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
        self.refuse_while_camera_held()?;
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

    fn claim(path: &str) -> CaptureClaim {
        CaptureClaim {
            path: PathBuf::from(path),
            expires: tokio::time::Instant::now() + Duration::from_secs(60),
            deadline: SystemTime::now() + Duration::from_secs(60),
            stop_requested: false,
            recovered: false,
        }
    }

    async fn claim_slot(ops: &Arc<GuiderOps>, path: &str) {
        let op = ops.op_lock().lock().await;
        ops.claim_capture(&op, claim(path));
    }

    #[tokio::test]
    async fn a_guiding_mutation_is_refused_while_a_frame_holds_the_slot() {
        let ops = test_ops();
        claim_slot(&ops, "/images/phd2-guider-a.fits").await;
        // Refused before any RPC: the test client is not even connected.
        let err = ops.pause(false).await.unwrap_err();
        assert!(
            matches!(&err, ServiceError::CameraBusy(text) if text.contains("capturing a frame")),
            "unexpected error: {err:?}"
        );
        let err = ops
            .clear_calibration(crate::types::CalibrationTarget::Mount)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ServiceError::CameraBusy(_)),
            "unexpected error: {err:?}"
        );
    }

    #[tokio::test]
    async fn a_recovered_frame_is_named_as_one_from_before_the_restart() {
        let ops = test_ops();
        {
            let op = ops.op_lock().lock().await;
            ops.claim_capture(
                &op,
                CaptureClaim {
                    recovered: true,
                    ..claim("/images/phd2-guider-a.fits")
                },
            );
        }
        let err = ops.resume().await.unwrap_err();
        assert!(
            matches!(&err, ServiceError::CameraBusy(text) if text.contains("before this service restarted")),
            "unexpected error: {err:?}"
        );
    }

    #[tokio::test]
    async fn the_slot_is_released_only_for_its_own_frame() {
        let ops = test_ops();
        claim_slot(&ops, "/images/phd2-guider-a.fits").await;
        ops.release_capture(Path::new("/images/phd2-guider-b.fits"));
        assert!(ops.capture_claim().is_some());
        ops.release_capture(Path::new("/images/phd2-guider-a.fits"));
        assert!(ops.capture_claim().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn the_claim_ends_on_the_monotonic_clock_not_the_wall_clock() {
        let ops = test_ops();
        {
            let op = ops.op_lock().lock().await;
            ops.claim_capture(
                &op,
                CaptureClaim {
                    // As after the wall clock stepped forward an hour.
                    deadline: SystemTime::now() - Duration::from_hours(1),
                    ..claim("/images/phd2-guider-a.fits")
                },
            );
        }
        assert!(
            ops.capture_claim().is_some(),
            "a wall-clock step ended the claim"
        );
        tokio::time::advance(Duration::from_secs(61)).await;
        assert!(
            ops.capture_claim().is_none(),
            "the claim outlived its expiry"
        );
    }

    #[tokio::test]
    async fn a_stop_marks_the_frame_in_the_slot_as_stopped() {
        let ops = test_ops();
        claim_slot(&ops, "/images/phd2-guider-a.fits").await;
        ops.mark_capture_stopped();
        assert!(ops.capture_claim().unwrap().stop_requested);
    }

    #[tokio::test]
    async fn a_wait_for_the_slot_returns_once_it_is_released() {
        let ops = test_ops();
        claim_slot(&ops, "/images/phd2-guider-a.fits").await;
        let waiter = tokio::spawn({
            let ops = Arc::clone(&ops);
            async move {
                ops.wait_capture_released(
                    Path::new("/images/phd2-guider-a.fits"),
                    Duration::from_secs(30),
                )
                .await
            }
        });
        // Whether the waiter has registered yet or not, it must see the
        // release: it registers before it checks the slot.
        ops.release_capture(Path::new("/images/phd2-guider-a.fits"));
        assert!(waiter.await.unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn a_wait_for_the_slot_gives_up_at_its_limit() {
        let ops = test_ops();
        claim_slot(&ops, "/images/phd2-guider-a.fits").await;
        let released = ops
            .wait_capture_released(
                Path::new("/images/phd2-guider-a.fits"),
                Duration::from_secs(2),
            )
            .await;
        assert!(!released);
        assert!(ops.capture_claim().is_some());
    }

    fn acquire_request(hex: char) -> AcquireRequest {
        lease::parse_acquire(&format!(r#"{{"token": "{}"}}"#, hex.to_string().repeat(32))).unwrap()
    }

    fn hold_lease(ops: &GuiderOps, hex: char) {
        let request = acquire_request(hex);
        ops.lease_book()
            .grant(request.token, request.ttl_s, tokio::time::Instant::now())
            .unwrap();
    }

    #[tokio::test]
    async fn a_guiding_mutation_is_refused_while_a_lease_is_held() {
        let ops = test_ops();
        hold_lease(&ops, 'a');
        // Refused before any RPC: the test client is not even connected.
        let err = ops.pause(true).await.unwrap_err();
        assert!(
            matches!(&err, ServiceError::CameraBusy(text) if text.contains("sweep lease")),
            "unexpected error: {err:?}"
        );
        let err = ops.reselect_star().await.unwrap_err();
        assert!(
            matches!(err, ServiceError::CameraBusy(_)),
            "unexpected error: {err:?}"
        );
    }

    #[tokio::test]
    async fn a_frame_in_the_slot_is_named_before_a_held_lease() {
        let ops = test_ops();
        hold_lease(&ops, 'a');
        claim_slot(&ops, "/images/phd2-guider-a.fits").await;
        let err = ops.resume().await.unwrap_err();
        assert!(
            matches!(&err, ServiceError::CameraBusy(text) if text.contains("capturing a frame")),
            "unexpected error: {err:?}"
        );
    }

    #[tokio::test]
    async fn a_stop_ends_the_lease_even_when_phd2_cannot_answer() {
        let ops = test_ops();
        hold_lease(&ops, 'a');
        ops.stop().await.unwrap_err();
        let token = acquire_request('a').token;
        let err = ops.renew_lease(&token).unwrap_err();
        assert!(err.contains("guiding stop"), "{err}");
    }

    #[tokio::test]
    async fn an_acquire_is_refused_while_a_guiding_operation_holds_the_lock() {
        let ops = test_ops();
        let _op = ops.op_lock().lock().await;
        let err = ops.acquire_lease(&acquire_request('a')).await.unwrap_err();
        assert!(
            matches!(&err, LeaseError::Refused(text) if text.contains("guider is busy")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn an_acquire_is_refused_while_a_frame_holds_the_slot() {
        let ops = test_ops();
        claim_slot(&ops, "/images/phd2-guider-a.fits").await;
        let err = ops.acquire_lease(&acquire_request('a')).await.unwrap_err();
        assert!(
            matches!(&err, LeaseError::Refused(text) if text.contains("at the latest")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn an_acquire_under_a_used_token_is_refused_before_phd2_is_asked() {
        let ops = test_ops();
        let token = acquire_request('a').token;
        assert!(!ops.release_lease(&token));
        // The test client is not connected: a refusal naming the token
        // was reached before any RPC.
        let err = ops.acquire_lease(&acquire_request('a')).await.unwrap_err();
        assert!(
            matches!(&err, LeaseError::Refused(text) if text.contains("used before")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn an_acquire_with_phd2_unreachable_is_not_connected() {
        let ops = test_ops();
        let err = ops.acquire_lease(&acquire_request('a')).await.unwrap_err();
        assert!(matches!(err, LeaseError::NotConnected(_)), "{err:?}");
        assert!(ops
            .lease_book()
            .held_until(tokio::time::Instant::now())
            .is_none());
    }

    #[tokio::test]
    async fn a_renewal_and_a_release_take_no_guiding_lock() {
        let ops = test_ops();
        hold_lease(&ops, 'a');
        let _op = ops.op_lock().lock().await;
        let token = acquire_request('a').token;
        assert_eq!(ops.renew_lease(&token).unwrap().ttl_s, 60);
        assert!(ops.release_lease(&token));
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
